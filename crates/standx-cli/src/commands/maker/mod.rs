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
use config::MakerRunArgs;
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
use notify::{
    token_expiry_level, MakerNotifier, PositionChange, RequestTimeoutNotice, RiskNotice,
    RiskSeverity, TokenExpiryLevel,
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
    endpoints: &standx_sdk::StandXEndpoints,
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
            flags,
        } => {
            let args = config::merge(flags, config::load(maker_config.as_deref())?, verbose)?;
            runtime::run_maker(symbol, args, output_format, endpoints).await
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
                canary::WsCommandCanaryRequest {
                    symbol,
                    size,
                    price_offset_bps,
                    timeout_secs,
                    alert_webhook,
                    alert_webhook_format,
                    output_format,
                },
                endpoints,
            )
            .await
        }
        MakerCommands::Replay { trace } => replay::run(&trace, output_format),
    }
}
