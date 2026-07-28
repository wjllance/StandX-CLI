use super::*;

pub(super) struct ShutdownReport<'a> {
    pub(super) live: bool,
    pub(super) output_format: OutputFormat,
    pub(super) symbol: &'a str,
    pub(super) cfg: &'a MakerConfig,
    pub(super) client: &'a StandXClient,
    pub(super) notifier: &'a MakerNotifier,
    pub(super) ledger: &'a MakerLedger,
    pub(super) stats: &'a MakerStats,
    pub(super) breaker: &'a VolBreaker,
    pub(super) exit: MakerExit,
    pub(super) cycle: u64,
    pub(super) total_places: u64,
    pub(super) total_cancels: u64,
    pub(super) total_holds: u64,
    pub(super) total_fills: u64,
    pub(super) total_halted: u64,
    pub(super) sim_position: f64,
    pub(super) last_mark: Option<f64>,
    /// Positions at or below this magnitude count as flat (stage 5-b handoff).
    pub(super) qty_tolerance: f64,
    pub(super) feed_handle: Option<tokio::task::JoinHandle<()>>,
    pub(super) account_stream_handle: Option<tokio::task::JoinHandle<()>>,
    pub(super) order_response_handle: Option<tokio::task::JoinHandle<()>>,
}

/// Abort feed/stream tasks, cancel any residual maker orders, print the human
/// summary, and deliver the stopped-lifecycle notifications. Runs on every
/// exit path; returns the process result (fail-safe error or clean Ok).
pub(super) async fn shutdown_report(report: ShutdownReport<'_>) -> Result<()> {
    let ShutdownReport {
        live,
        output_format,
        symbol,
        cfg,
        client,
        notifier,
        ledger,
        stats,
        breaker,
        exit,
        cycle,
        total_places,
        total_cancels,
        total_holds,
        total_fills,
        total_halted,
        sim_position,
        last_mark,
        qty_tolerance,
        feed_handle,
        account_stream_handle,
        order_response_handle,
    } = report;
    if let Some(handle) = feed_handle {
        handle.abort();
    }
    if let Some(handle) = account_stream_handle {
        handle.abort();
    }
    let final_position = if live {
        ledger.expected_position
    } else {
        stats.position()
    };
    if output_format == OutputFormat::Table {
        println!(
            "\n👋 Stopping maker (ran {} cycles: {} places, {} cancels, {} holds)",
            cycle, total_places, total_cancels, total_holds
        );
        let pnl_note = match last_mark {
            Some(m) => format!(
                " | PnL {:+.2} (mark-to-market)",
                stats.pnl(final_position, m)
            ),
            None => String::new(),
        };
        println!(
            "   {} fills | uptime {:.0}% | max pos {} | avg capture {:.1}bps{}",
            total_fills,
            stats.uptime_pct(),
            maker::format_decimals(stats.max_abs_position, cfg.qty_decimals),
            stats.avg_spread_capture_bps(),
            pnl_note
        );
        if breaker.enabled() {
            println!("   vol breaker: {} cycles halted", total_halted);
        }
        if !live {
            println!(
                "   paper sim: ending position {}",
                maker::format_decimals(sim_position, cfg.qty_decimals)
            );
        }
        // The live ending position is deliberately NOT printed here: it is only
        // authoritative after cleanup, so it belongs to the residual handoff
        // below (stage 5-b).
    }
    if let Some(handle) = order_response_handle {
        handle.abort();
    }
    // Do not return early on cleanup failure: operators need the stopped
    // lifecycle alert most when residual maker orders may still be live.
    let cleanup_error = if live {
        cancel_maker_orders_with_retry(client, symbol, 3, output_format)
            .await
            .err()
    } else {
        None
    };

    // Notify stop on every exit path. Await delivery so the message lands
    // before the process exits.
    let reason = exit.lifecycle_reason();
    let pnl_str = last_mark
        .map(|m| format!("{:+.2}", stats.pnl(final_position, m)))
        .unwrap_or_else(|| "n/a".to_string());
    let cleanup_note = cleanup_error.as_ref().map_or_else(String::new, |error| {
        format!(" | ⚠️ cleanup failed: {error}")
    });

    // Residual position handoff (stage 5-b), decided only now: an order that
    // filled while cleanup was cancelling it changes the position, and the
    // account stream is already gone, so the session ledger cannot be the final
    // word. Ask the venue; if it will not answer, say so instead of reporting a
    // ledger snapshot as confirmed.
    let venue_position = if live {
        match client.get_positions(Some(symbol)).await {
            Ok(positions) => match position_for_symbol(&positions, symbol) {
                Ok(position) => Some(position),
                Err(error) => {
                    eprintln!("⚠️  post-cleanup position snapshot unreadable: {error}");
                    None
                }
            },
            Err(error) => {
                eprintln!("⚠️  post-cleanup position snapshot failed: {error}");
                None
            }
        }
    } else {
        // Paper mode has no venue; the simulation is the authority.
        Some(sim_position)
    };
    let ledger_position = if live { final_position } else { sim_position };
    let handoff = residual_handoff(venue_position, ledger_position, qty_tolerance);
    output::emit_residual_position_handoff(
        output_format,
        symbol,
        cycle,
        output::ResidualHandoffReport {
            handoff: &handoff,
            mark: last_mark,
            qty_decimals: cfg.qty_decimals,
            exit_reason: &reason,
        },
    );
    // The residual travels with every outbound stop message: an operator away
    // from the terminal learns about it from the webhook or not at all, and
    // nothing closes it automatically.
    let residual_note = match &handoff {
        ResidualHandoff::Flat => String::new(),
        ResidualHandoff::Confirmed { position } => format!(
            " | ⚠️ residual position {} (manual close required)",
            maker::format_decimals(*position, cfg.qty_decimals)
        ),
        ResidualHandoff::Unknown { ledger, venue, .. } => format!(
            " | 🛑 residual position UNKNOWN (venue={}, ledger={}; check the venue)",
            venue.map_or_else(
                || "unavailable".to_string(),
                |position| maker::format_decimals(position, cfg.qty_decimals)
            ),
            maker::format_decimals(*ledger, cfg.qty_decimals)
        ),
    };
    if let Some(error) = cleanup_error.as_ref() {
        let message = format!("maker cleanup failed or left residual orders: {error}");
        notifier
            .risk(
                RiskNotice {
                    kind: "maker_cleanup",
                    severity: "critical",
                    event: "residual_orders",
                    message: &message,
                    symbol,
                    cycle,
                    position_before: None,
                    position_after: None,
                    expected: Some(ledger.expected_position),
                    observed: None,
                },
                true,
            )
            .await;
    }
    if handoff.needs_operator() {
        let message = match &handoff {
            ResidualHandoff::Confirmed { position } => format!(
                "maker stopped holding {} {} — the maker never auto-flattens; close or hedge it manually ({})",
                maker::format_decimals(*position, cfg.qty_decimals),
                symbol,
                reason
            ),
            // Flat is filtered out by needs_operator(); keep the arm explicit.
            ResidualHandoff::Flat => String::new(),
            ResidualHandoff::Unknown { reason: cause, .. } => format!(
                "maker stopped on {symbol} but its final position could NOT be confirmed ({cause}){residual_note} — verify at the venue before starting anything else ({reason})"
            ),
        };
        notifier
            .risk(
                RiskNotice {
                    kind: "residual_position",
                    severity: "critical",
                    event: handoff.event(),
                    message: &message,
                    symbol,
                    cycle,
                    position_before: None,
                    position_after: venue_position,
                    expected: Some(ledger.expected_position),
                    observed: venue_position,
                },
                true,
            )
            .await;
    }
    if !matches!(&exit, MakerExit::CtrlC) {
        let fail_safe_message = format!("{reason}{residual_note}");
        notifier
            .risk(
                RiskNotice {
                    kind: "fail_safe",
                    severity: "critical",
                    event: "stopped",
                    message: &fail_safe_message,
                    symbol,
                    cycle,
                    position_before: None,
                    position_after: None,
                    expected: Some(ledger.expected_position),
                    observed: None,
                },
                true,
            )
            .await;
    }
    notifier
        .lifecycle(
            "stopped",
            &format!(
                "🔴 maker stopped ({}) — {} | {} cycles, {} fills, uptime {:.0}%, PnL {}{}{}",
                reason,
                symbol,
                cycle,
                total_fills,
                stats.uptime_pct(),
                pnl_str,
                cleanup_note,
                residual_note,
            ),
            symbol,
            true,
        )
        .await;

    if let Some(error) = cleanup_error {
        // A residual-order cleanup failure is an intentional fail-safe stop
        // that needs a human, not an automatic restart.
        return Err(anyhow::Error::new(FailSafeShutdown {
            message: format!(
                "maker stopped (fail-safe) but maker-owned order cleanup failed: {error}"
            ),
        }));
    }

    match exit.terminal_error() {
        Some(message) => Err(anyhow::Error::new(FailSafeShutdown { message })),
        None => Ok(()),
    }
}

impl MakerRuntime {
    pub(super) async fn announce_start(
        args: &MakerRunArgs,
        output_format: OutputFormat,
        startup: &MakerStartup,
    ) {
        let cfg = &startup.cfg;
        let symbol = &startup.symbol;
        let notifier = &startup.notifier;
        let mode = if args.live { "LIVE" } else { "PAPER" };
        if output_format == OutputFormat::Table {
            println!("┌──────────────────────────────────────────────────────────┐");
            println!("│ standx maker — {} mode on {}", mode, symbol);
            println!(
                "│ spread {}bps | band {}bps | refresh {}bps | {} level(s)",
                cfg.spread_bps, cfg.band_bps, cfg.refresh_bps, cfg.levels
            );
            println!(
                "│ size {} | max-position {} | interval {}s",
                cfg.size, cfg.max_position, args.interval
            );
            if cfg.skew_bps > 0.0 {
                println!(
                    "│ inventory skew {}bps (live only; paper holds no position)",
                    cfg.skew_bps
                );
            }
            if args.inventory_exit_pct > 0.0 {
                println!(
                    "│ active exit: {}% of max, reduce-only chunks of {} (live only)",
                    args.inventory_exit_pct, args.inventory_exit_qty
                );
            }
            println!(
                "│ ticks: price {}dp, qty {}dp | min qty {}",
                cfg.price_decimals, cfg.qty_decimals, cfg.min_order_qty
            );
            if !args.live {
                println!("│ paper mode: no real orders; fills are simulated when the");
                println!("│ touch crosses a quote, so position & skew move. --live for real.");
            } else {
                println!(
                    "│ ⚠️  LIVE: the bot manages only {} orders on {}",
                    MAKER_CL_ORD_ID_PREFIX, symbol
                );
                println!("│ manual/API orders are preserved and ignored.");
                println!(
                    "│ order-response recovery: {} attempt(s), {}s base backoff",
                    args.order_response_reconnect_attempts, args.order_response_reconnect_backoff
                );
                println!(
                    "│ order request deadline: {}s to correlated ACK/account visibility",
                    ORDER_REQUEST_TIMEOUT.as_secs()
                );
                println!(
                    "│ account-stream recovery: {} attempt(s), {}s base backoff",
                    args.account_stream_reconnect_attempts, args.account_stream_reconnect_backoff
                );
                println!(
                    "│ transport recovery: placements stay frozen while bounded reconnect rounds retry"
                );
            }
            if args.no_ws {
                println!("│ feed: REST polling (--no-ws)");
            } else {
                println!(
                    "│ feed: websocket (REST fallback) | divergence guard {}bps",
                    args.max_divergence_bps
                );
            }
            if args.vol_pause_bps > 0.0 {
                println!(
                    "│ vol breaker: halt at {}bps range / {} cycles (resume < {}bps)",
                    args.vol_pause_bps,
                    args.vol_window.max(1),
                    args.vol_pause_bps / 2.0
                );
            }
            if args.stop_loss > 0.0 {
                println!(
                    "│ stop-loss: session PnL -{} → fail-safe shutdown",
                    args.stop_loss
                );
            }
            if args.stop_equity_below > 0.0 || args.stop_margin_below > 0.0 {
                let mut floors = Vec::new();
                if args.stop_equity_below > 0.0 {
                    floors.push(format!("equity <{}", args.stop_equity_below));
                }
                if args.stop_margin_below > 0.0 {
                    floors.push(format!("margin <{}", args.stop_margin_below));
                }
                println!(
                    "│ account floors (hard): {} → fail-safe shutdown, residual handed off",
                    floors.join(", ")
                );
            }
            if args.alert_loss > 0.0
                || args.alert_inventory_pct > 0.0
                || args.alert_position_change_pct > 0.0
                || args.alert_uptime > 0.0
                || args.alert_equity_below > 0.0
                || args.alert_margin_below > 0.0
            {
                let mut parts = Vec::new();
                if args.alert_loss > 0.0 {
                    parts.push(format!("loss -{}", args.alert_loss));
                }
                if args.alert_inventory_pct > 0.0 {
                    parts.push(format!("inv {}%", args.alert_inventory_pct));
                }
                if args.alert_position_change_pct > 0.0 {
                    parts.push(format!("position Δ {}%", args.alert_position_change_pct));
                }
                if args.alert_uptime > 0.0 {
                    parts.push(format!("uptime {}%", args.alert_uptime));
                }
                if args.alert_equity_below > 0.0 {
                    parts.push(format!("equity <{}", args.alert_equity_below));
                }
                if args.alert_margin_below > 0.0 {
                    parts.push(format!("margin <{}", args.alert_margin_below));
                }
                let sink = if args.alert_webhook.is_some() {
                    format!("stderr + webhook ({:?})", args.alert_webhook_format).to_lowercase()
                } else {
                    "stderr".to_string()
                };
                println!("│ risk alerts: {} → {}", parts.join(", "), sink);
            }
            println!("│ Ctrl+C to stop (cancels maker-owned resting orders on exit)");
            println!("└──────────────────────────────────────────────────────────┘");
        }

        // Notify start (fire-and-forget; the process keeps running).
        notifier
            .lifecycle(
                "started",
                &format!(
                "🟢 maker started — {} {} | spread {}bps band {}bps size {} | {} | order-response reconnects {}",
                mode,
                symbol,
                cfg.spread_bps,
                cfg.band_bps,
                cfg.size,
                if args.no_ws { "REST" } else { "WS" },
                if args.live {
                    args.order_response_reconnect_attempts
                } else {
                    0
                }
                ),
                symbol,
                false,
            )
        .await;
    }

    pub(super) async fn shutdown(self, exit: MakerExit) -> Result<()> {
        let MakerRuntime {
            deps,
            loop_state,
            market,
            live_session,
            ..
        } = self;
        let RuntimeDeps {
            args,
            output_format,
            client,
            cfg,
            symbol,
            notifier,
            qty_tolerance,
            ..
        } = deps;
        let RuntimeLoopState {
            mut ledger,
            performance_started,
            performance_epoch_ms,
            stats,
            breaker,
            sim_position,
            counters,
            ..
        } = loop_state;
        let RuntimeCounters {
            cycle,
            total_places,
            total_cancels,
            total_holds,
            total_fills,
            total_halted,
            ..
        } = counters;
        let RuntimeMarketState {
            feed_handle,
            last_mark,
            ..
        } = market;

        if let (Some(performance), Some(final_mark)) = (ledger.performance_mut(), last_mark) {
            let end_time_ms = performance_epoch_ms.saturating_add(
                i64::try_from(performance_started.elapsed().as_millis()).unwrap_or(i64::MAX),
            );
            if let Err(error) = performance.finish(end_time_ms) {
                eprintln!("⚠️ performance finalization unavailable: {error}");
            }
            match performance.summary(final_mark) {
                Ok(summary) => output::emit_performance_summary(output_format, &symbol, &summary),
                Err(error) => eprintln!("⚠️ performance summary unavailable: {error}"),
            }
        }
        let (account_stream_handle, order_response_handle) = match live_session {
            Some(mut session) => {
                let ended_ms = u64::try_from(session.latency_started.elapsed().as_millis())
                    .unwrap_or(u64::MAX);
                if let Err(error) = session.order_latency.finish_process(ended_ms) {
                    eprintln!("⚠️ order latency finalization unavailable: {error}");
                }
                output::emit_order_latency(output_format, &symbol, &session.order_latency);
                (
                    Some(session.account_stream_handle),
                    Some(session.order_response_handle),
                )
            }
            None => (None, None),
        };
        shutdown_report(ShutdownReport {
            live: args.live,
            output_format,
            symbol: &symbol,
            cfg: &cfg,
            client: &client,
            notifier: &notifier,
            ledger: &ledger,
            stats: &stats,
            breaker: &breaker,
            exit,
            cycle,
            total_places,
            total_cancels,
            total_holds,
            total_fills,
            total_halted,
            sim_position,
            last_mark,
            qty_tolerance,
            feed_handle,
            account_stream_handle,
            order_response_handle,
        })
        .await
    }
}
