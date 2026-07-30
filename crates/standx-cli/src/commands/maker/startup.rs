use super::output::{emit_ledger_sync, emit_startup_rejected};
use super::*;
use standx_sdk::order_response::{OrderCommandSender, OrderResponse, OrderResponseHealth};
use standx_sdk::StandXEndpoints;

/// Everything that exists exactly when the maker runs `--live`: the
/// order-response stream, the authenticated account stream, the projection
/// they feed, and the REST polling schedule for account floors.
///
/// `run_maker` holds a single `Option<LiveSession>` that is `Some` iff
/// `args.live`, so live-only code paths bind the session once and use plain
/// fields instead of re-proving "live implies initialized" per use. Reconnect
/// paths replace the relevant fields in place (aborting the old task handle
/// first); the session itself stays `Some` for the whole run.
pub(super) struct LiveSession {
    pub(super) order_responses: tokio::sync::mpsc::Receiver<OrderResponse>,
    pub(super) order_commands: OrderCommandSender,
    pub(super) order_response_health: OrderResponseHealth,
    pub(super) order_response_handle: tokio::task::JoinHandle<()>,
    pub(super) account_events: tokio::sync::mpsc::Receiver<AccountEvent>,
    pub(super) account_stream_health: AccountStreamHealth,
    pub(super) account_stream_handle: tokio::task::JoinHandle<()>,
    /// Bumped on every account-stream reconnect; stamps projection generations.
    pub(super) account_stream_epoch: u64,
    pub(super) projection: MakerAccountProjection,
    pub(super) order_request_deadlines: OrderRequestDeadlines,
    pub(super) account_poll: LiveAccountPollState,
    pub(super) order_latency: maker::OrderLatencyTracker,
    pub(super) latency_started: std::time::Instant,
}

pub(super) fn new_maker_rest_client(endpoints: &StandXEndpoints) -> Result<StandXClient> {
    let client = StandXClient::from_endpoints(endpoints)?;
    debug_assert!(client.session_id().is_none());
    Ok(client)
}

/// Reject alert thresholds that silently defeat the guard they configure.
///
/// A negative threshold turns the alert off without warning, and a percentage
/// above 100 can never fire; both leave an operator believing a protection is
/// armed when it is not.
pub(super) fn validate_alert_thresholds(
    alert_loss: f64,
    alert_inventory_pct: f64,
    alert_position_change_pct: f64,
    alert_uptime: f64,
) -> Result<()> {
    if alert_loss < 0.0 {
        return Err(anyhow::anyhow!("--alert-loss must be >= 0"));
    }
    if !(0.0..=100.0).contains(&alert_inventory_pct) {
        return Err(anyhow::anyhow!("--alert-inventory-pct must be 0..=100"));
    }
    if !(0.0..=100.0).contains(&alert_position_change_pct) {
        return Err(anyhow::anyhow!(
            "--alert-position-change-pct must be 0..=100"
        ));
    }
    if alert_uptime < 0.0 {
        return Err(anyhow::anyhow!("--alert-uptime must be >= 0"));
    }
    Ok(())
}

/// Everything the quoting loop needs after startup: the REST client, resolved
/// config, canonical symbol casing, the notifier, and — in live mode — the
/// initialized [`LiveSession`] plus the adopted ledger baseline.
pub(super) struct MakerStartup {
    pub(super) live_process_lock: Option<super::process_lock::LiveProcessLock>,
    pub(super) client: StandXClient,
    pub(super) endpoints: StandXEndpoints,
    pub(super) cfg: MakerConfig,
    pub(super) symbol: String,
    pub(super) notifier: MakerNotifier,
    pub(super) qty_tolerance: f64,
    pub(super) run_order_prefix: String,
    pub(super) starting_position: f64,
    pub(super) baseline_mark: f64,
    pub(super) session_started_at: i64,
    pub(super) live_session: Option<LiveSession>,
}

/// Validate arguments, resolve symbol metadata into a [`MakerConfig`], and — in
/// live mode — run the clean-start handshake: cancel leftover maker orders,
/// adopt existing inventory at the current mark, connect the authenticated
/// account stream and the order-response stream, and reconcile the post-auth
/// snapshot. Fails fast on any invariant violation so the quoting loop only
/// ever starts from a verified, fully-initialized state.
pub(super) async fn run_startup(
    symbol: String,
    args: &MakerRunArgs,
    output_format: OutputFormat,
    endpoints: &StandXEndpoints,
) -> Result<MakerStartup> {
    let live_process_lock = args
        .live
        .then(super::process_lock::LiveProcessLock::acquire)
        .transpose()?;
    let order_session_id = args.live.then(|| uuid::Uuid::new_v4().to_string());
    let run_uuid = uuid::Uuid::new_v4().simple().to_string();
    let run_order_prefix = format!("{}{}-", MAKER_CL_ORD_ID_PREFIX, &run_uuid[..12]);
    let mut starting_position = 0.0_f64;
    let mut baseline_mark = 0.0_f64;
    let mut session_started_at = chrono::Utc::now().timestamp();

    if let Some(after) = args.controlled_disconnect_after {
        if !args.live {
            return Err(anyhow::anyhow!(
                "--controlled-disconnect-after requires --live"
            ));
        }
        if after == 0 || after > 60 {
            return Err(anyhow::anyhow!(
                "--controlled-disconnect-after must be between 1 and 60 seconds"
            ));
        }
    }
    if args.order_response_reconnect_attempts > 10 {
        return Err(anyhow::anyhow!(
            "--order-response-reconnect-attempts must be between 0 and 10"
        ));
    }
    if args.order_response_reconnect_attempts > 0
        && !(1..=60).contains(&args.order_response_reconnect_backoff)
    {
        return Err(anyhow::anyhow!(
            "--order-response-reconnect-backoff must be between 1 and 60 seconds when reconnect is enabled"
        ));
    }
    if args.account_stream_reconnect_attempts > 10 {
        return Err(anyhow::anyhow!(
            "--account-stream-reconnect-attempts must be between 0 and 10"
        ));
    }
    if args.account_stream_reconnect_attempts > 0
        && !(1..=60).contains(&args.account_stream_reconnect_backoff)
    {
        return Err(anyhow::anyhow!(
            "--account-stream-reconnect-backoff must be between 1 and 60 seconds when reconnect is enabled"
        ));
    }
    // REST reads, audits, and fail-safe cleanup must stay outside the
    // order-response session. Attaching x-session-id to REST cancellation
    // would route its asynchronous response into the command response stream,
    // where it has no projection request entry and would look uncorrelated.
    let client = new_maker_rest_client(endpoints)?;

    // ---- Startup: symbol metadata + invariants (fail fast) ----
    let infos = client.get_symbol_info().await?;
    let info = infos
        .iter()
        .find(|i| i.symbol.eq_ignore_ascii_case(&symbol))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Unknown symbol '{}'. Available: {}",
                symbol,
                infos
                    .iter()
                    .map(|i| i.symbol.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
    if info.status != "trading" {
        return Err(anyhow::anyhow!(
            "Symbol {} is not trading (status: {})",
            info.symbol,
            info.status
        ));
    }
    let symbol = info.symbol.clone(); // canonical casing

    let min_order_qty: f64 = info.min_order_qty.parse().map_err(|_| {
        anyhow::anyhow!(
            "unparseable min_order_qty '{}' for {} from venue symbol info",
            info.min_order_qty,
            info.symbol
        )
    })?;
    let cfg = MakerConfig {
        spread_bps: args.spread_bps,
        band_bps: args.band_bps,
        level_step_bps: args.level_step_bps,
        refresh_bps: args.refresh_bps,
        levels: args.levels.max(1),
        size: args.size,
        max_position: args.max_position,
        skew_bps: args.skew_bps,
        price_decimals: info.price_tick_decimals,
        qty_decimals: info.qty_tick_decimals,
        min_order_qty,
    };

    if cfg.spread_bps <= 0.0 {
        return Err(anyhow::anyhow!("--spread-bps must be > 0"));
    }
    if args.interval == 0 {
        return Err(anyhow::anyhow!("--interval must be at least 1 second"));
    }
    if args.vol_window_secs == Some(0) {
        return Err(anyhow::anyhow!(
            "vol_window_secs in TOML must be greater than 0"
        ));
    }
    if cfg.skew_bps < 0.0 {
        return Err(anyhow::anyhow!("--skew-bps must be >= 0"));
    }
    if !(0.0..=100.0).contains(&args.inventory_exit_pct) || args.inventory_exit_qty < 0.0 {
        return Err(anyhow::anyhow!(
            "--inventory-exit-pct must be 0..=100 and --inventory-exit-qty must be >= 0"
        ));
    }
    if (args.inventory_exit_pct > 0.0) != (args.inventory_exit_qty > 0.0) {
        return Err(anyhow::anyhow!(
            "active inventory exit requires both --inventory-exit-pct and --inventory-exit-qty"
        ));
    }
    validate_alert_thresholds(
        args.alert_loss,
        args.alert_inventory_pct,
        args.alert_position_change_pct,
        args.alert_uptime,
    )?;
    if cfg.band_bps <= cfg.spread_bps {
        return Err(anyhow::anyhow!(
            "--band-bps ({}) must be greater than --spread-bps ({}): quotes clamped to the band edge would sit exactly at the boundary",
            cfg.band_bps,
            cfg.spread_bps
        ));
    }
    maker::SpreadController::new(args.adaptive_spread.clone(), &cfg)
        .map_err(|error| anyhow::anyhow!("invalid adaptive spread config: {error}"))?;
    let size_skew_controller = maker::SizeSkewController::new(args.size_skew, &cfg)
        .map_err(|error| anyhow::anyhow!("invalid size skew config: {error}"))?;
    let rounded_size = maker::round_to_decimals(cfg.size, cfg.qty_decimals);
    if rounded_size < cfg.min_order_qty || rounded_size <= 0.0 {
        return Err(anyhow::anyhow!(
            "--size {} (rounded to {} at {} decimals) is below min order qty {} for {}",
            cfg.size,
            rounded_size,
            cfg.qty_decimals,
            cfg.min_order_qty,
            symbol
        ));
    }
    if size_skew_controller.is_degenerate(&cfg) {
        eprintln!("⚠️  size skew 缩量低于最小下单量，退化为二值加仓侧压制");
    }
    if cfg.refresh_bps >= cfg.spread_bps {
        eprintln!(
            "⚠️  --refresh-bps ({}) >= --spread-bps ({}): quotes will be held through large drifts",
            cfg.refresh_bps, cfg.spread_bps
        );
    }
    if cfg.levels > 1
        && cfg.spread_bps + (cfg.levels - 1) as f64 * cfg.level_step_bps >= cfg.band_bps
    {
        eprintln!("⚠️  outer quote levels exceed the band and will be clamped/collapsed");
    }

    let notifier = MakerNotifier::new(
        output_format,
        args.alert_webhook.clone(),
        args.alert_webhook_format,
    );

    // Half a qty tick: the adoption/mismatch tolerance used throughout the run.
    let qty_tolerance = 10_f64.powi(-(cfg.qty_decimals as i32)) / 2.0;

    // Performance attribution needs a positive session baseline in paper mode
    // too. Live establishes it alongside the authoritative position snapshot
    // below; paper has no account snapshot, so seed it from a public mark
    // before constructing the runtime ledger.
    if !args.live {
        baseline_mark = market_snapshot(&client, &symbol, None).await?.mark;
    }

    // ---- Live gating & clean start ----
    // `order_session_id` is `Some` iff `args.live`, so this block is the live
    // startup path; it either fails fast or yields a complete `LiveSession`.
    let mut live_session: Option<LiveSession> = None;
    if let Some(order_session_id) = order_session_id.as_deref() {
        if std::env::var(LIVE_MAKER_ENV).ok().as_deref() != Some("1") {
            return Err(anyhow::anyhow!(
                "live mode not yet enabled: it has not been supervised-tested against production. Set {}=1 to unlock (at your own risk).",
                LIVE_MAKER_ENV
            ));
        }
        // A live run with no push channel is how #220 happens: if the process
        // dies (SIGKILL/OOM/panic/host down) nobody is notified and resting
        // orders are left on the venue. Refuse to start live without a
        // webhook, and refuse if the webhook can never fire because every
        // alert threshold is disabled.
        if args.alert_webhook.is_none() {
            return Err(anyhow::anyhow!(
                "live mode requires --alert-webhook so the maker can push risk/stop notifications; refusing to run live with no push channel"
            ));
        }
        if args.alert_loss <= 0.0
            && args.alert_inventory_pct <= 0.0
            && args.alert_position_change_pct <= 0.0
            && args.alert_uptime <= 0.0
            && args.alert_equity_below <= 0.0
            && args.alert_margin_below <= 0.0
        {
            return Err(anyhow::anyhow!(
                "live mode requires at least one alert threshold; all maker and account thresholds are 0 so the webhook would never fire"
            ));
        }
        let creds = Credentials::load()?;
        if creds.is_expired() {
            return Err(anyhow::anyhow!(
                "Credentials expired. Run 'standx auth login' first."
            ));
        }
        if creds.private_key.is_empty() {
            return Err(anyhow::anyhow!(
                "Live mode requires a private key for order signing. Run 'standx auth login' with --private-key."
            ));
        }
        let open_orders = client.get_open_orders(Some(&symbol)).await?;
        let manual_orders = open_orders
            .iter()
            .filter(|order| !is_maker_order(order))
            .count();
        if manual_orders > 0 {
            eprintln!(
                "ℹ️  preserving {} manually-managed order(s) on {}; only {} orders are managed",
                manual_orders, symbol, MAKER_CL_ORD_ID_PREFIX
            );
        }
        // Clean only leftover orders owned by this maker. Manual/API orders
        // are not part of the strategy's reconciliation state and must never
        // be adopted or cancelled as stale.
        cancel_maker_orders_with_retry(&client, &symbol, 3, output_format).await?;

        // Establish the session ledger boundary before any new order can be
        // submitted. Existing inventory is adopted at the current mark, so
        // maker-session PnL starts at zero while account upnl remains intact.
        let history_to = chrono::Utc::now().timestamp();
        let history_from = history_to.saturating_sub(LEDGER_HISTORY_WINDOW_SECS);
        let (positions, startup_market, filled_orders, historical_trades, balance) = tokio::join!(
            client.get_positions(Some(&symbol)),
            market_snapshot(&client, &symbol, None),
            client.get_order_history(Some(&symbol), Some(ORDER_HISTORY_LIMIT)),
            client.get_user_trades(
                &symbol,
                history_from,
                history_to,
                Some(TRADE_LOOKBACK_LIMIT)
            ),
            client.get_balance(),
        );
        let positions = positions?;
        let mark = startup_market?.mark;
        let filled_orders = filled_orders?;
        let historical_trades = historical_trades?;
        let balance = balance?;
        starting_position = position_for_symbol(&positions, &symbol)?;
        baseline_mark = mark;
        let account_poll = LiveAccountPollState::new(balance, std::time::Instant::now());

        let historical_order_ids = filled_orders
            .iter()
            .filter(|order| is_maker_order(order))
            .map(|order| {
                order.id.parse::<u64>().map_err(|_| {
                    anyhow::anyhow!(
                        "historical maker order has non-integer exchange ID '{}'",
                        order.id
                    )
                })
            })
            .collect::<Result<HashSet<_>>>()?;
        let historical_maker_orders = historical_order_ids.len();
        let historical_maker_trades = historical_trades
            .iter()
            .filter(|trade| {
                trade
                    .order_id
                    .is_some_and(|order_id| historical_order_ids.contains(&order_id))
            })
            .count();

        if !maker::position_within_limit(starting_position, cfg.max_position, cfg.qty_decimals) {
            emit_startup_rejected(output_format, &symbol, starting_position, cfg.max_position);
            let message = format!(
                "starting position {starting_position:+.8} exceeds max_position {:.8}",
                cfg.max_position
            );
            notifier
                .risk(
                    RiskNotice::critical(
                        "startup_position_limit",
                        "startup_rejected",
                        &message,
                        &symbol,
                        0,
                    )
                    .position_after(starting_position)
                    .observed(starting_position),
                    true,
                )
                .await;
            return Err(anyhow::anyhow!(
                "starting position {:+.8} exceeds max_position {:.8}",
                starting_position,
                cfg.max_position
            ));
        }
        session_started_at = chrono::Utc::now().timestamp();

        // Authenticated account state is a hard live dependency. Connect it
        // before order-response readiness, then require a second REST
        // snapshot so events buffered during authentication cannot create an
        // unobserved startup gap.
        let account_stream_epoch = 1_u64;
        let account_stream = AccountStream::from_endpoints(account_stream_epoch, endpoints)?;
        let (account_events, account_stream_health, account_stream_handle) = account_stream
            .connect(&[
                AccountChannel::Order,
                AccountChannel::Position,
                AccountChannel::Trade,
                AccountChannel::Balance,
            ])
            .await?;
        let post_auth_positions = client.get_positions(Some(&symbol)).await?;
        let post_auth_position = position_for_symbol(&post_auth_positions, &symbol)?;
        if (post_auth_position - starting_position).abs() > qty_tolerance {
            account_stream_handle.abort();
            notifier
                .risk(
                    RiskNotice::critical(
                        "position_reconciliation",
                        "startup_sync_failed",
                        "position changed while the account stream was authenticating",
                        &symbol,
                        0,
                    )
                    .position_change(starting_position, post_auth_position)
                    .expected(starting_position)
                    .observed(post_auth_position),
                    true,
                )
                .await;
            return Err(anyhow::anyhow!(
                "position changed while account stream was authenticating: baseline {starting_position:+.8}, snapshot {post_auth_position:+.8}"
            ));
        }
        let stream = OrderResponseStream::from_endpoints(order_session_id, endpoints)?;
        let (order_commands, order_responses, order_response_health, order_response_handle) =
            stream.connect().await?;
        if let Some(after) = args.controlled_disconnect_after {
            let health_for_fault = order_response_health.clone();
            let abort = order_response_handle.abort_handle();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(after)).await;
                // Aborting drops the local WebSocket halves; set health first
                // so the runtime enters its existing freeze/cleanup/reconnect
                // path even if it delays observing the socket close.
                health_for_fault.mark_unhealthy(format!(
                    "controlled fault injection closed the order-response stream after {after}s"
                ));
                abort.abort();
            });
            eprintln!(
                "⚠️ controlled fault injection armed: closing order-response stream after {after}s"
            );
        }
        live_session = Some(LiveSession {
            order_responses,
            order_commands,
            order_response_health,
            order_response_handle,
            account_events,
            account_stream_health,
            account_stream_handle,
            account_stream_epoch,
            projection: MakerAccountProjection::new(
                account_stream_epoch,
                run_order_prefix.clone(),
                starting_position,
                cfg.price_tick() / 2.0,
                qty_tolerance,
            ),
            order_request_deadlines: OrderRequestDeadlines::default(),
            account_poll,
            order_latency: maker::OrderLatencyTracker::default(),
            latency_started: std::time::Instant::now(),
        });
        emit_ledger_sync(
            output_format,
            &symbol,
            starting_position,
            baseline_mark,
            historical_maker_orders,
            historical_maker_trades,
        );
        if starting_position.abs() > qty_tolerance {
            let message = format!("adopted non-zero starting inventory {starting_position:+.8}");
            notifier
                .risk(
                    RiskNotice::warning("inventory_adopted", "startup", &message, &symbol, 0)
                        .position_change(0.0, starting_position)
                        .expected(starting_position)
                        .observed(starting_position),
                    false,
                )
                .await;
        }
    }

    Ok(MakerStartup {
        live_process_lock,
        client,
        endpoints: endpoints.clone(),
        cfg,
        symbol,
        notifier,
        qty_tolerance,
        run_order_prefix,
        starting_position,
        baseline_mark,
        session_started_at,
        live_session,
    })
}

#[cfg(test)]
mod tests {
    use super::validate_alert_thresholds;

    #[test]
    fn alert_thresholds_reject_silent_disable_and_unfireable_ranges() {
        // Baseline: all valid.
        assert!(validate_alert_thresholds(50.0, 80.0, 20.0, 3600.0).is_ok());
        // Zero everywhere means "disabled" and is allowed.
        assert!(validate_alert_thresholds(0.0, 0.0, 0.0, 0.0).is_ok());
        // Negative thresholds silently disable the guard.
        assert!(validate_alert_thresholds(-1.0, 80.0, 20.0, 3600.0).is_err());
        assert!(validate_alert_thresholds(50.0, -1.0, 20.0, 3600.0).is_err());
        assert!(validate_alert_thresholds(50.0, 80.0, 20.0, -1.0).is_err());
        // Percentages above 100 can never fire.
        assert!(validate_alert_thresholds(50.0, 170.0, 20.0, 3600.0).is_err());
        assert!(validate_alert_thresholds(50.0, 80.0, 170.0, 3600.0).is_err());
    }
}
