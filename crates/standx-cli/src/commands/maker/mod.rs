use crate::cli::*;
use anyhow::Result;
use standx_maker::{
    self as maker, AccountProjectionEvent, AlertMonitor, MakerAccountProjection, MakerConfig,
    MakerEffect, MakerEvent, MakerFill, MakerLedger, MakerState, MakerStats,
    OrderResponseContinuity, PositionAlertAnchor, ProjectionPendingRequest,
    ProjectionRegistryError, RecoveryTarget, RestingQuote, RuntimeStopReason, VolBreaker,
    WorkToken, MAKER_CL_ORD_ID_PREFIX, MAX_CONSECUTIVE_CYCLE_ERRORS,
};
use standx_sdk::account_stream::{
    AccountChannel, AccountEvent, AccountStream, AccountStreamHealth,
};
use standx_sdk::auth::Credentials;
use standx_sdk::client::StandXClient;
use standx_sdk::order_response::OrderResponseStream;
use std::collections::HashSet;
use std::time::Duration;

mod canary;
mod config;
mod cycle;
mod external_feed;
mod feed;
mod ledger;
mod market_data;
mod model;
mod notify;
mod output;
mod pipeline;
mod process_lock;
mod recovery;
mod replay;
mod runtime;
mod startup;
#[cfg(test)]
use runtime::apply_order_responses;
#[cfg(test)]
use startup::new_maker_rest_client;
use startup::{run_startup, LiveSession, MakerStartup};

use cycle::maker_cycle;
use feed::{fresh_ws_sample, market_snapshot, spawn_market_feed, ws_snapshot_issue};
use market_data::{
    classify_market_health, degradation_detail, observe_acquired_market_health,
    AcquiredMarketHealth, ClassifiedMarketHealth, MarketDataDegradedError,
    MARKET_DATA_STANDBY_HEARTBEAT,
};
use model::{
    is_maker_order, position_for_symbol, residual_handoff, AccountFloorError, MakerExit,
    ResidualHandoff,
};
pub use model::{FailSafeShutdown, FAIL_SAFE_EXIT_CODE};
#[cfg(test)]
use notify::webhook_body;
use notify::{
    token_expiry_level, MakerNotifier, PositionChange, RequestTimeoutNotice, RiskNotice,
    TokenExpiryLevel,
};
use pipeline::{
    CycleRequest, CycleState, LiveAccountPollState, OrderRequestDeadlines, TimedOutOrderRequest,
};
use recovery::{
    cancel_maker_orders_with_retry, ctrl_c_latched, probe_position_convergence,
    reconnect_account_stream, reconnect_order_response, AccountStreamReconnect, ConvergenceProbe,
    PositionReconciliationCause, PositionReconciliationError, ReconcileRequest,
    ReconnectCleanupFailed, ReconnectInterrupted, ReconnectRequest, TransportReconnectExhausted,
};
#[cfg(test)]
use recovery::{
    recover_current_run_order_ids_for_reconciliation, validate_reconnect_snapshot, PositionGap,
};
#[cfg(test)]
use standx_maker::ProjectionPendingPlace;
#[cfg(test)]
use standx_sdk::models::{Order, OrderSide, Position, Trade};
#[cfg(test)]
use standx_sdk::order_response::OrderResponse;

// ============================================================================
// Maker bot (SIP-5A community maker yield)
// ============================================================================

/// Build a webhook body for a one-shot panic notification, matching the alert
/// webhook payload shape. Exposed for the top-level panic hook (issue #220) so
/// a silent crash still pushes one last critical message before the process
/// dies.
pub fn panic_webhook_body(format: AlertWebhookFormat, text: &str) -> serde_json::Value {
    let raw = serde_json::json!({
        "text": text,
        "action": "panic",
        "severity": "critical",
    });
    notify::webhook_body(format, text, &raw)
}

/// Env var gating live order placement. The live path ships code-complete but
/// locked until it has been supervised-tested against production.
const LIVE_MAKER_ENV: &str = "STANDX_ENABLE_LIVE_MAKER";

/// REST history depth for ledger sync and reconciliation snapshots. Shared by
/// every account-audit fan-out and the ledger-sync telemetry so the reported
/// limits cannot drift from the ones actually queried.
const ORDER_HISTORY_LIMIT: u32 = 100;
const TRADE_LOOKBACK_LIMIT: u32 = 500;
/// Look-back window for the startup ledger baseline (adopts existing inventory
/// at the session boundary).
const LEDGER_HISTORY_WINDOW_SECS: i64 = 24 * 60 * 60;

/// Warn when the JWT has under 2h of life left; escalate under 15m. Token
/// lifetime caps run duration (there is no renewal endpoint), so an operator
/// needs lead time to re-authenticate before the bot halts.
const TOKEN_EXPIRY_WARN_SECS: i64 = 2 * 60 * 60;
const TOKEN_EXPIRY_CRITICAL_SECS: i64 = 15 * 60;
/// Throttle disk/env credential reloads for the expiry check.
const TOKEN_EXPIRY_CHECK_INTERVAL: Duration = Duration::from_secs(60);
pub async fn handle_maker(
    command: MakerCommands,
    output_format: OutputFormat,
    verbose: bool,
) -> Result<()> {
    // Maker output is emitted as JSON lines or a human table; there is no CSV
    // renderer, so `--output csv` would silently fall back to the table. Reject
    // it up front rather than surprising a pipeline that asked for CSV.
    if output_format == OutputFormat::Csv {
        return Err(anyhow::anyhow!(
            "maker does not support --output csv; use json (machine-readable) or table (human)"
        ));
    }
    match command {
        MakerCommands::Run {
            symbol,
            maker_config,
            spread_bps,
            band_bps,
            size,
            levels,
            level_step_bps,
            refresh_bps,
            interval,
            max_position,
            skew_bps,
            inventory_exit_pct,
            inventory_exit_qty,
            max_divergence_bps,
            vol_pause_bps,
            vol_window,
            adaptive_spread,
            size_skew,
            stop_loss,
            alert_loss,
            alert_inventory_pct,
            alert_position_change_pct,
            alert_uptime,
            alert_equity_below,
            alert_margin_below,
            stop_equity_below,
            stop_margin_below,
            alert_webhook,
            alert_webhook_format,
            no_ws,
            live,
            order_response_reconnect_attempts,
            order_response_reconnect_backoff,
            account_stream_reconnect_attempts,
            account_stream_reconnect_backoff,
            recovery_incidents_per_window,
            recovery_window_secs,
            controlled_disconnect_after,
        } => {
            let file = config::load(maker_config.as_deref())?;
            let selected_vol_window = vol_window.or(file.vol_window);
            let selected_vol_window_secs = file.vol_window_secs;
            if selected_vol_window.is_some() && selected_vol_window_secs.is_some() {
                return Err(anyhow::anyhow!(
                    "--vol-window conflicts with vol_window_secs in TOML; choose samples or seconds"
                ));
            }
            let adaptive_spread = match file.adaptive_spread {
                Some(config) => config.into_domain(adaptive_spread),
                None if adaptive_spread.unwrap_or(false) => {
                    return Err(anyhow::anyhow!(
                        "--adaptive-spread requires an [adaptive_spread] TOML section"
                    ));
                }
                None => maker::AdaptiveSpreadConfig::default(),
            };
            if adaptive_spread.enabled && selected_vol_window_secs.is_none() {
                return Err(anyhow::anyhow!(
                    "adaptive spread requires vol_window_secs in TOML"
                ));
            }
            let size_skew = match file.size_skew {
                Some(config) => config.into_domain(size_skew),
                None if size_skew.is_some() => {
                    return Err(anyhow::anyhow!(
                        "--size-skew requires a [size_skew] TOML section"
                    ));
                }
                None => maker::SizeSkewConfig::default(),
            };
            // Stage 3 v1 combined candidate: TOML-only switches (no CLI
            // overrides) so frozen A/B configs stay the single source of truth.
            let nonlinear_skew = file
                .nonlinear_skew
                .map(|config| config.into_domain())
                .unwrap_or_default();
            let external_guard_basis_half_life_secs = file
                .external_guard
                .as_ref()
                .and_then(|config| config.basis_half_life_secs)
                .unwrap_or(config::DEFAULT_GUARD_BASIS_HALF_LIFE_SECS);
            let external_guard = file
                .external_guard
                .map(|config| config.into_domain())
                .unwrap_or_default();
            // Keep accepting the removed rolling-circuit knobs for one
            // compatibility window so existing production commands/configs do
            // not fail to parse. They deliberately do not enter MakerRunArgs.
            let _legacy_recovery_circuit = (
                recovery_incidents_per_window.or(file.recovery_incidents_per_window),
                recovery_window_secs.or(file.recovery_window_secs),
            );
            runtime::run_maker(
                symbol,
                MakerRunArgs {
                    spread_bps: choose(spread_bps, file.spread_bps, 5.0),
                    band_bps: choose(band_bps, file.band_bps, 20.0),
                    size: choose(size, file.size, 0.01),
                    levels: choose(levels, file.levels, 1),
                    level_step_bps: choose(level_step_bps, file.level_step_bps, 2.0),
                    refresh_bps: choose(refresh_bps, file.refresh_bps, 3.0),
                    interval: choose(interval, file.interval, 5),
                    max_position: choose(max_position, file.max_position, 0.05),
                    skew_bps: choose(skew_bps, file.skew_bps, 0.0),
                    inventory_exit_pct: choose(inventory_exit_pct, file.inventory_exit_pct, 0.0),
                    inventory_exit_qty: choose(inventory_exit_qty, file.inventory_exit_qty, 0.0),
                    max_divergence_bps: choose(max_divergence_bps, file.max_divergence_bps, 25.0),
                    vol_pause_bps: choose(vol_pause_bps, file.vol_pause_bps, 0.0),
                    vol_window: selected_vol_window.unwrap_or(12),
                    vol_window_secs: selected_vol_window_secs,
                    adaptive_spread,
                    size_skew,
                    nonlinear_skew,
                    external_guard,
                    external_guard_basis_half_life_secs,
                    stop_loss: choose(stop_loss, file.stop_loss, 0.0),
                    alert_loss: choose(alert_loss, file.alert_loss, 0.0),
                    alert_inventory_pct: choose(alert_inventory_pct, file.alert_inventory_pct, 0.0),
                    alert_position_change_pct: choose(
                        alert_position_change_pct,
                        file.alert_position_change_pct,
                        0.0,
                    ),
                    alert_uptime: choose(alert_uptime, file.alert_uptime, 0.0),
                    alert_equity_below: choose(alert_equity_below, file.alert_equity_below, 0.0),
                    alert_margin_below: choose(alert_margin_below, file.alert_margin_below, 0.0),
                    stop_equity_below: choose(stop_equity_below, file.stop_equity_below, 0.0),
                    stop_margin_below: choose(stop_margin_below, file.stop_margin_below, 0.0),
                    alert_webhook,
                    alert_webhook_format,
                    no_ws: choose(no_ws, file.no_ws, false),
                    live,
                    order_response_reconnect_attempts: choose(
                        order_response_reconnect_attempts,
                        file.order_response_reconnect_attempts,
                        3,
                    ),
                    order_response_reconnect_backoff: choose(
                        order_response_reconnect_backoff,
                        file.order_response_reconnect_backoff,
                        2,
                    ),
                    account_stream_reconnect_attempts: choose(
                        account_stream_reconnect_attempts,
                        file.account_stream_reconnect_attempts,
                        3,
                    ),
                    account_stream_reconnect_backoff: choose(
                        account_stream_reconnect_backoff,
                        file.account_stream_reconnect_backoff,
                        2,
                    ),
                    controlled_disconnect_after,
                    verbose,
                },
                output_format,
            )
            .await
        }
        MakerCommands::WsCommandCanary {
            symbol,
            size,
            price_offset_bps,
            timeout_secs,
            alert_webhook,
            alert_webhook_format,
        } => {
            canary::run_ws_command_canary(
                symbol,
                size,
                price_offset_bps,
                timeout_secs,
                alert_webhook,
                alert_webhook_format,
                output_format,
            )
            .await
        }
        MakerCommands::Replay { trace } => replay::run(&trace, output_format),
    }
}

fn choose<T: Copy>(cli: Option<T>, file: Option<T>, default: T) -> T {
    cli.or(file).unwrap_or(default)
}

struct MakerRunArgs {
    spread_bps: f64,
    band_bps: f64,
    size: f64,
    levels: u32,
    level_step_bps: f64,
    refresh_bps: f64,
    interval: u64,
    max_position: f64,
    skew_bps: f64,
    inventory_exit_pct: f64,
    inventory_exit_qty: f64,
    max_divergence_bps: f64,
    vol_pause_bps: f64,
    vol_window: u32,
    vol_window_secs: Option<u64>,
    adaptive_spread: maker::AdaptiveSpreadConfig,
    size_skew: maker::SizeSkewConfig,
    nonlinear_skew: maker::NonlinearSkewConfig,
    external_guard: maker::GuardConfig,
    external_guard_basis_half_life_secs: u64,
    stop_loss: f64,
    alert_loss: f64,
    alert_inventory_pct: f64,
    alert_position_change_pct: f64,
    alert_uptime: f64,
    alert_equity_below: f64,
    alert_margin_below: f64,
    /// Account-level hard floors (stage 5-b): breaching either stops the
    /// session through `MakerExit::AccountFloor`. 0 = off, the default.
    stop_equity_below: f64,
    stop_margin_below: f64,
    alert_webhook: Option<String>,
    alert_webhook_format: AlertWebhookFormat,
    no_ws: bool,
    live: bool,
    order_response_reconnect_attempts: u32,
    order_response_reconnect_backoff: u64,
    account_stream_reconnect_attempts: u32,
    account_stream_reconnect_backoff: u64,
    controlled_disconnect_after: Option<u64>,
    verbose: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::{Matcher, Server};

    #[test]
    fn position_jump_alert_uses_anchor_and_half_tick_tolerance() {
        let mut anchor = PositionAlertAnchor::new(0.001, 20.0, 0.1);
        assert!(anchor.evaluate(0.10, 0.8, 25.0, 0.0005).is_none());
        let alert = anchor.evaluate(0.161, 0.8, 25.0, 0.0005).unwrap();
        assert!((alert.before - 0.001).abs() < 1e-9);
        assert!((alert.after - alert.before - 0.160).abs() < 1e-9);
        assert!(anchor.evaluate(0.161, 0.8, 25.0, 0.0005).is_none());
    }

    #[test]
    fn position_jump_alert_respects_neutral_deadband_and_exit_crossing() {
        let mut direction = PositionAlertAnchor::new(-0.074, 20.0, 0.1);
        assert_eq!(
            direction.evaluate(0.126, 0.8, 0.0, 0.0005).unwrap().kind,
            maker::PositionRiskKind::Jump
        );

        let mut exit = PositionAlertAnchor::new(0.19, 0.0, 0.1);
        assert!(exit.evaluate(0.20, 0.8, 25.0, 0.0005).is_some());
    }

    #[test]
    fn order_response_exit_message_does_not_claim_three_errors() {
        let exit = MakerExit::OrderResponse(
            "order-response WebSocket closed: code=1008 reason=\"maintenance\"".to_string(),
        );

        let lifecycle = exit.lifecycle_reason();
        let terminal = exit.terminal_error().unwrap();
        assert!(lifecycle.contains("code=1008"), "{lifecycle}");
        assert!(terminal.contains("stopped immediately"), "{terminal}");
        assert!(!terminal.contains("3 consecutive"), "{terminal}");
    }

    #[test]
    fn consecutive_cycle_exit_message_names_three_errors() {
        let exit = MakerExit::ConsecutiveErrors("network timeout".to_string());

        let lifecycle = exit.lifecycle_reason();
        let terminal = exit.terminal_error().unwrap();
        assert!(lifecycle.contains("3 consecutive maker cycle errors"));
        assert!(terminal.contains("3 consecutive maker cycle errors"));
    }

    #[test]
    fn position_reconciliation_exit_is_immediate() {
        let exit = MakerExit::PositionReconciliation(
            "expected position -0.13000000, venue reported +0.07000000".to_string(),
        );
        let terminal = exit.terminal_error().unwrap();
        assert!(terminal.contains("stopped immediately"));
        assert!(!terminal.contains("3 consecutive"));
    }

    #[test]
    fn accounting_invariant_exit_is_immediate() {
        let exit = MakerExit::AccountingInvariant(
            "stats position -0.20000000 differs from ledger expected +0.00000000".to_string(),
        );
        let lifecycle = exit.lifecycle_reason();
        let terminal = exit.terminal_error().unwrap();
        assert!(lifecycle.contains("accounting invariant failed"));
        assert!(terminal.contains("stopped immediately"));
        assert!(terminal.contains("ledger expected"));
    }

    #[test]
    fn stop_loss_exit_reports_the_breach_and_is_terminal() {
        let exit = MakerExit::StopLoss("session PnL -12.50 <= -10.00".to_string());
        let lifecycle = exit.lifecycle_reason();
        let terminal = exit.terminal_error().unwrap();
        assert!(lifecycle.contains("stop-loss breached"), "{lifecycle}");
        assert!(lifecycle.contains("-12.50"), "{lifecycle}");
        assert!(terminal.contains("stopped immediately"), "{terminal}");
        assert!(terminal.contains("stop-loss breached"), "{terminal}");
    }

    #[test]
    fn inherited_position_allows_half_tick_tolerance_but_rejects_real_excess() {
        assert!(maker::position_within_limit(0.800_05, 0.8, 3));
        assert!(!maker::position_within_limit(0.800_6, 0.8, 3));
        assert!(maker::position_within_limit(-0.8, 0.8, 3));
    }

    #[test]
    fn position_uses_side_to_normalize_signed_and_unsigned_quantities() {
        assert_eq!(
            position_for_symbol(&[test_position("buy", "0.13")], "XAG-USD").unwrap(),
            0.13
        );
        assert_eq!(
            position_for_symbol(&[test_position("sell", "0.13")], "XAG-USD").unwrap(),
            -0.13
        );
        assert_eq!(
            position_for_symbol(&[test_position("sell", "-0.13")], "XAG-USD").unwrap(),
            -0.13
        );
        assert!(position_for_symbol(&[test_position("sell", "NaN")], "XAG-USD").is_err());
        assert_eq!(
            model::signed_position_quantity("-0.13", None).unwrap(),
            -0.13
        );
        assert_eq!(
            model::signed_position_quantity("0.13", Some(OrderSide::Sell)).unwrap(),
            -0.13
        );
    }

    struct EnvGuard {
        key: &'static str,
        original: Option<String>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let lock = crate::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let original = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self {
                key,
                original,
                _lock: lock,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.original {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    fn test_order(id: &str, cl_ord_id: Option<&str>) -> Order {
        Order {
            id: id.to_string(),
            cl_ord_id: cl_ord_id.map(str::to_string),
            symbol: "XAG-USD".to_string(),
            side: OrderSide::Buy,
            order_type: standx_sdk::models::OrderType::Limit,
            qty: "0.2".to_string(),
            fill_qty: "0".to_string(),
            price: "59.40".to_string(),
            status: standx_sdk::models::OrderStatus::New,
            created_at: "now".to_string(),
            updated_at: "now".to_string(),
        }
    }

    fn test_position(side: &str, qty: &str) -> Position {
        serde_json::from_value(serde_json::json!({
            "id": 1,
            "symbol": "XAG-USD",
            "side": side,
            "qty": qty,
            "entry_price": "59.40",
            "entry_value": "11.88",
            "holding_margin": "1",
            "initial_margin": "1",
            "leverage": "1",
            "mark_price": "59.40",
            "margin_asset": "USDT",
            "margin_mode": "cross",
            "position_value": "11.88",
            "realized_pnl": "0",
            "required_margin": "1",
            "status": "open",
            "upnl": "0",
            "time": "now",
            "created_at": "now",
            "updated_at": "now",
            "user": "test"
        }))
        .unwrap()
    }

    fn test_trade(id: u64, order_id: u64) -> Trade {
        Trade {
            id,
            time: "now".to_string(),
            price: "59.40".to_string(),
            qty: "0.2".to_string(),
            side: Some("buy".to_string()),
            is_buyer_taker: false,
            fee_asset: None,
            fee_qty: None,
            pnl: None,
            order_id: Some(order_id),
            symbol: Some("XAG-USD".to_string()),
            value: None,
        }
    }

    #[test]
    fn reconnect_snapshot_requires_empty_maker_book_and_valid_ledger() {
        let manual = test_order("99", Some("manual-order"));
        let filled = test_order("42", Some("sxmk-filled"));
        let snapshot = validate_reconnect_snapshot(
            "XAG-USD",
            "sxmk-",
            &[manual],
            &[test_position("sell", "0.2")],
            &[filled],
            &[test_trade(7, 42)],
        )
        .unwrap();

        assert_eq!(snapshot.position, -0.2);
        assert_eq!(snapshot.maker_filled_orders, 1);
        assert_eq!(snapshot.maker_trades, 1);
    }

    #[test]
    fn reconnect_snapshot_rejects_residual_maker_order() {
        let error = validate_reconnect_snapshot(
            "XAG-USD",
            "sxmk-",
            &[test_order("42", Some("sxmk-still-open"))],
            &[],
            &[],
            &[],
        )
        .unwrap_err();

        assert!(error.to_string().contains("appeared after cleanup"));
    }

    #[test]
    fn reconnect_snapshot_rejects_unstable_maker_trade_id() {
        let error = validate_reconnect_snapshot(
            "XAG-USD",
            "sxmk-",
            &[],
            &[],
            &[test_order("42", Some("sxmk-filled"))],
            &[test_trade(0, 42)],
        )
        .unwrap_err();

        assert!(error.to_string().contains("stable trade ID"));
    }

    #[test]
    fn alert_thresholds_reject_silent_disable_and_unfireable_ranges() {
        use startup::validate_alert_thresholds;
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

    #[test]
    fn webhook_body_shapes() {
        let txt = "🚨 ALERT [BTC-USD] loss — PnL -50 breached";
        // Structured object a caller would build for the Raw format.
        let raw_in = serde_json::json!({
            "text": txt, "symbol": "BTC-USD", "kind": "loss", "firing": true,
        });

        // Slack / Telegram: bare {"text": ...}
        let slack = webhook_body(AlertWebhookFormat::Slack, txt, &raw_in);
        assert_eq!(slack["text"], txt);
        assert!(slack.get("msg_type").is_none());
        let tg = webhook_body(AlertWebhookFormat::Telegram, txt, &raw_in);
        assert_eq!(tg["text"], txt);
        assert!(tg.get("kind").is_none()); // not the structured object

        // Feishu: {"msg_type":"text","content":{"text":...}}
        let feishu = webhook_body(AlertWebhookFormat::Feishu, txt, &raw_in);
        assert_eq!(feishu["msg_type"], "text");
        assert_eq!(feishu["content"]["text"], txt);

        // Raw: the structured object verbatim.
        let raw = webhook_body(AlertWebhookFormat::Raw, txt, &raw_in);
        assert_eq!(raw["kind"], "loss");
        assert_eq!(raw["firing"], true);
        assert_eq!(raw["symbol"], "BTC-USD");
    }

    #[test]
    fn partial_fill_stays_adopted() {
        // Full remainder adopts.
        assert!(maker::open_qty_adopts(0.01, 0.01));
        // Partial remainder (half filled) still adopts.
        assert!(maker::open_qty_adopts(0.005, 0.01));
        // Tiny remainder adopts.
        assert!(maker::open_qty_adopts(0.0001, 0.01));
        // Zero / fully filled does not adopt (no open order to match).
        assert!(!maker::open_qty_adopts(0.0, 0.01));
        // Larger than placed is someone else's order.
        assert!(!maker::open_qty_adopts(0.02, 0.01));
        // Float slop just under the placed qty is tolerated.
        assert!(maker::open_qty_adopts(0.01 + 1e-9, 0.01));
    }

    #[test]
    fn maker_order_ownership_uses_reserved_client_id_prefix() {
        assert!(is_maker_order(&test_order("123", Some("sxmk-7f2b"))));
        assert!(!is_maker_order(&test_order("123", Some("manual-7f2b"))));
        assert!(!is_maker_order(&test_order("123", None)));
    }

    #[test]
    fn pending_order_reserves_its_quote_slot() {
        let pending = [ProjectionPendingPlace {
            request_id: "request-1".to_string(),
            client_order_id: "sxmk-1".to_string(),
            side: OrderSide::Buy,
            price: 100.0,
            qty: 0.01,
            level: 0,
            ref_center: 100.0,
            cycle: 1,
        }];

        assert!(maker::pending_covers_slot(
            pending.iter().map(|place| maker::QuoteSlot {
                side: place.side,
                level: place.level,
            }),
            OrderSide::Buy,
            0,
        ));
        assert!(!maker::pending_covers_slot(
            pending.iter().map(|place| maker::QuoteSlot {
                side: place.side,
                level: place.level,
            }),
            OrderSide::Buy,
            1,
        ));
        assert!(!maker::pending_covers_slot(
            pending.iter().map(|place| maker::QuoteSlot {
                side: place.side,
                level: place.level,
            }),
            OrderSide::Sell,
            0,
        ));
    }

    #[test]
    fn cli_value_overrides_maker_file_then_default() {
        assert_eq!(choose(Some(3_u32), Some(2), 1), 3);
        assert_eq!(choose(None, Some(2_u32), 1), 2);
        assert_eq!(choose(None::<u32>, None, 1), 1);
    }

    #[test]
    fn async_rejection_removes_only_matching_pending_place() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(4);
        let pending_place = |request_id: &str| ProjectionPendingPlace {
            request_id: request_id.to_string(),
            client_order_id: format!("client-{request_id}"),
            side: OrderSide::Buy,
            price: 100.0,
            qty: 0.01,
            level: 0,
            ref_center: 100.0,
            cycle: 1,
        };
        let mut projection = MakerAccountProjection::new(1, "sxmk-test-", 0.0, 0.005, 0.00005);
        for pending in [pending_place("request-1"), pending_place("request-2")] {
            projection.apply(1, AccountProjectionEvent::PlaceSubmitted(pending));
        }
        let mut runtime_state = MakerState::starting();
        sender
            .try_send(OrderResponse {
                code: 400,
                message: "alo order rejected".to_string(),
                request_id: Some("request-1".to_string()),
            })
            .unwrap();

        apply_order_responses(
            &mut receiver,
            &mut projection,
            &mut runtime_state,
            OutputFormat::Quiet,
            "BTC-USD",
            2,
            2,
        )
        .unwrap();

        assert_eq!(projection.pending_places().len(), 1);
        assert_eq!(projection.pending_places()[0].request_id, "request-2");
    }

    #[test]
    fn async_acceptance_keeps_pending_until_exchange_order_is_visible() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(2);
        let pending = ProjectionPendingPlace {
            request_id: "request-1".to_string(),
            client_order_id: "client-1".to_string(),
            side: OrderSide::Sell,
            price: 101.0,
            qty: 0.01,
            level: 0,
            ref_center: 100.0,
            cycle: 1,
        };
        let mut projection = MakerAccountProjection::new(1, "sxmk-test-", 0.0, 0.005, 0.00005);
        projection.apply(1, AccountProjectionEvent::PlaceSubmitted(pending));
        let mut runtime_state = MakerState::starting();
        sender
            .try_send(OrderResponse {
                code: 0,
                message: "accepted".to_string(),
                request_id: Some("request-1".to_string()),
            })
            .unwrap();

        apply_order_responses(
            &mut receiver,
            &mut projection,
            &mut runtime_state,
            OutputFormat::Quiet,
            "BTC-USD",
            2,
            2,
        )
        .unwrap();

        for cycle in 3..=100 {
            projection.apply(1, AccountProjectionEvent::AdvanceCycle { cycle });
        }
        assert_eq!(projection.pending_places().len(), 1);
    }

    #[test]
    fn disconnected_order_response_stream_is_fail_closed() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        drop(sender);
        let mut projection = MakerAccountProjection::new(1, "sxmk-test-", 0.0, 0.005, 0.00005);
        let mut runtime_state = MakerState::starting();

        let error = apply_order_responses(
            &mut receiver,
            &mut projection,
            &mut runtime_state,
            OutputFormat::Quiet,
            "BTC-USD",
            1,
            2,
        )
        .unwrap_err();

        assert!(error.to_string().contains("disconnected"));
    }

    #[tokio::test]
    async fn controlled_disconnect_fails_closed_then_cleans_only_maker_orders() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        drop(sender);
        let mut projection = MakerAccountProjection::new(1, "sxmk-test-", 0.0, 0.005, 0.00005);
        let mut runtime_state = MakerState::starting();

        let error = apply_order_responses(
            &mut receiver,
            &mut projection,
            &mut runtime_state,
            OutputFormat::Quiet,
            "BTC-USD",
            7,
            2,
        )
        .unwrap_err();
        assert!(error.to_string().contains("disconnected"));
        eprintln!("controlled disconnect -> fail-safe: {error}");

        let _jwt = EnvGuard::set("STANDX_JWT", "controlled-test-jwt");
        let mut server = Server::new_async().await;
        let open_before = server
            .mock("GET", "/api/query_open_orders")
            .match_query(Matcher::UrlEncoded("symbol".into(), "BTC-USD".into()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"code":0,"message":"ok","result":[
                    {"id":"42","cl_ord_id":"sxmk-controlled-buy","symbol":"BTC-USD","side":"buy","order_type":"limit","qty":"0.001","fill_qty":"0","price":"63000","status":"open","created_at":"2026-07-10T00:00:00Z","updated_at":"2026-07-10T00:00:00Z"},
                    {"id":"99","cl_ord_id":"manual-order","symbol":"BTC-USD","side":"sell","order_type":"limit","qty":"0.001","fill_qty":"0","price":"65000","status":"open","created_at":"2026-07-10T00:00:00Z","updated_at":"2026-07-10T00:00:00Z"}
                ]}"#,
            )
            .expect(1)
            .create_async()
            .await;
        let cancel = server
            .mock("POST", "/api/cancel_orders")
            .match_body(Matcher::Json(serde_json::json!({ "order_id_list": [42] })))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"code":0,"message":"accepted"}"#)
            .expect(1)
            .create_async()
            .await;
        let open_after = server
            .mock("GET", "/api/query_open_orders")
            .match_query(Matcher::UrlEncoded("symbol".into(), "BTC-USD".into()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"code":0,"message":"ok","result":[
                    {"id":"99","cl_ord_id":"manual-order","symbol":"BTC-USD","side":"sell","order_type":"limit","qty":"0.001","fill_qty":"0","price":"65000","status":"open","created_at":"2026-07-10T00:00:00Z","updated_at":"2026-07-10T00:00:00Z"}
                ]}"#,
            )
            .expect(1)
            .create_async()
            .await;

        let client = StandXClient::with_base_url(server.url()).unwrap();
        cancel_maker_orders_with_retry(&client, "BTC-USD", 3, OutputFormat::Quiet)
            .await
            .unwrap();

        open_before.assert_async().await;
        cancel.assert_async().await;
        open_after.assert_async().await;
    }

    #[tokio::test]
    async fn maker_cleanup_retries_stale_open_order_verification() {
        let _jwt = EnvGuard::set("STANDX_JWT", "controlled-test-jwt");
        let mut server = Server::new_async().await;
        let maker_and_manual = r#"{"code":0,"message":"ok","result":[
            {"id":"42","cl_ord_id":"sxmk-controlled-buy","symbol":"BTC-USD","side":"buy","order_type":"limit","qty":"0.001","fill_qty":"0","price":"63000","status":"open","created_at":"2026-07-10T00:00:00Z","updated_at":"2026-07-10T00:00:00Z"},
            {"id":"99","cl_ord_id":"manual-order","symbol":"BTC-USD","side":"sell","order_type":"limit","qty":"0.001","fill_qty":"0","price":"65000","status":"open","created_at":"2026-07-10T00:00:00Z","updated_at":"2026-07-10T00:00:00Z"}
        ]}"#;
        let manual_only = r#"{"code":0,"message":"ok","result":[
            {"id":"99","cl_ord_id":"manual-order","symbol":"BTC-USD","side":"sell","order_type":"limit","qty":"0.001","fill_qty":"0","price":"65000","status":"open","created_at":"2026-07-10T00:00:00Z","updated_at":"2026-07-10T00:00:00Z"}
        ]}"#;
        let open_before = server
            .mock("GET", "/api/query_open_orders")
            .match_query(Matcher::UrlEncoded("symbol".into(), "BTC-USD".into()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(maker_and_manual)
            .expect(1)
            .create_async()
            .await;
        let cancel_first = server
            .mock("POST", "/api/cancel_orders")
            .match_body(Matcher::Json(serde_json::json!({ "order_id_list": [42] })))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"code":0,"message":"accepted"}"#)
            .expect(1)
            .create_async()
            .await;
        let stale_verify = server
            .mock("GET", "/api/query_open_orders")
            .match_query(Matcher::UrlEncoded("symbol".into(), "BTC-USD".into()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(maker_and_manual)
            .expect(1)
            .create_async()
            .await;
        let open_retry = server
            .mock("GET", "/api/query_open_orders")
            .match_query(Matcher::UrlEncoded("symbol".into(), "BTC-USD".into()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(maker_and_manual)
            .expect(1)
            .create_async()
            .await;
        let cancel_retry = server
            .mock("POST", "/api/cancel_orders")
            .match_body(Matcher::Json(serde_json::json!({ "order_id_list": [42] })))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"code":0,"message":"accepted"}"#)
            .expect(1)
            .create_async()
            .await;
        let cleared_verify = server
            .mock("GET", "/api/query_open_orders")
            .match_query(Matcher::UrlEncoded("symbol".into(), "BTC-USD".into()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(manual_only)
            .expect(1)
            .create_async()
            .await;

        let client = StandXClient::with_base_url(server.url()).unwrap();
        cancel_maker_orders_with_retry(&client, "BTC-USD", 3, OutputFormat::Quiet)
            .await
            .unwrap();

        open_before.assert_async().await;
        cancel_first.assert_async().await;
        stale_verify.assert_async().await;
        open_retry.assert_async().await;
        cancel_retry.assert_async().await;
        cleared_verify.assert_async().await;
    }

    #[tokio::test]
    async fn reconciliation_recovers_fast_current_run_fill_by_order_id() {
        let _jwt = EnvGuard::set("STANDX_JWT", "controlled-test-jwt");
        let mut server = Server::new_async().await;
        let order_lookup = server
            .mock("GET", "/api/query_order")
            .match_query(Matcher::UrlEncoded(
                "order_id".into(),
                "11477424747".into(),
            ))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"id":"11477424747","cl_ord_id":"sxmk-0123456789ab-q00000001b0","symbol":"XAG-USD","side":"buy","order_type":"limit","qty":"0.001","fill_qty":"0.001","price":"59.89","status":"filled","created_at":"2026-07-11T07:06:05Z","updated_at":"2026-07-11T07:06:07Z"}"#,
            )
            .expect(1)
            .create_async()
            .await;
        let trade = Trade {
            id: 316_912_722,
            time: "2026-07-11T07:06:07.128726Z".to_string(),
            price: "59.89".to_string(),
            qty: "0.001".to_string(),
            side: Some("buy".to_string()),
            is_buyer_taker: false,
            fee_asset: Some("DUSD".to_string()),
            fee_qty: Some("0.000005989".to_string()),
            pnl: Some("0.00008".to_string()),
            order_id: Some(11_477_424_747),
            symbol: Some("XAG-USD".to_string()),
            value: Some("0.05989".to_string()),
        };
        let client = StandXClient::with_base_url(server.url()).unwrap();
        let mut ledger = MakerLedger::new(-0.001);

        recover_current_run_order_ids_for_reconciliation(
            &client,
            &[trade],
            PositionGap {
                expected: -0.001,
                observed: 0.0,
                qty_tolerance: 0.0005,
                run_order_prefix: "sxmk-0123456789ab-",
            },
            &mut ledger,
        )
        .await;

        assert!(ledger.maker_order_ids.contains(&11_477_424_747));
        assert!(ledger.exit_order_ids.is_empty());
        order_lookup.assert_async().await;
    }
}
