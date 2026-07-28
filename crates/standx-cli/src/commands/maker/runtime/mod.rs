use super::output::{
    emit_account_floor_triggered, emit_live_fill, emit_market_data_standby,
    emit_reconciliation_snapshot_error, emit_reconciliation_state, emit_stop_loss_triggered,
    AccountFloorStop, MarketDataStandby,
};
use super::*;
use standx_sdk::order_response::OrderResponse;

/// Whether the session watches account-level risk at all: an alert threshold
/// (`alert_equity_below` / `alert_margin_below`) or a stage 5-b hard floor
/// (`stop_equity_below` / `stop_margin_below`).
///
/// Balance-event wakeups and refresh scheduling key off this rather than off
/// the alert thresholds alone: an armed hard floor left on the 30-second cache
/// cadence would be a solvency brake in name only.
pub(super) fn account_risk_watch_enabled(
    args: &MakerRunArgs,
    alerts: &maker::AlertMonitor,
) -> bool {
    alerts.account_enabled() || account_floors_armed(args)
}

/// Whether either stage 5-b account hard floor is armed.
pub(super) fn account_floors_armed(args: &MakerRunArgs) -> bool {
    args.stop_equity_below > 0.0 || args.stop_margin_below > 0.0
}

mod cycle_flow;
mod events;
mod lifecycle;
mod recovery_flow;
mod state;

#[cfg(test)]
use cycle_flow::{commit_cycle_effect, take_cycle_work};
#[cfg(test)]
pub(super) use events::apply_order_responses;
use events::{
    absorb_account_outcome, account_event_invalidates_cycle, accounting_position_mismatch,
    apply_account_event, apply_account_events, apply_order_response,
    apply_order_responses_observed, duration_ms, invalidate_session_latency,
    market_update_requires_replan, observe_order_ack, order_request_timeout_detail,
    order_response_failure, reconciliation_error_for_cycle, request_timeout_notice,
    schedule_account_balance_refresh, AccountEventContext, AccountEventState,
    OrderResponseObservation, OutcomeSink, ORDER_REQUEST_TIMEOUT,
};
#[cfg(test)]
use events::{correlation_failure_detail, AccountEventOutcome, CancelRejection};
use recovery_flow::*;
use state::*;
pub(super) async fn run_maker(
    symbol: String,
    args: MakerRunArgs,
    output_format: OutputFormat,
) -> Result<()> {
    let startup = run_startup(symbol, &args, output_format).await?;
    MakerRuntime::announce_start(&args, output_format, &startup).await;
    let runtime = MakerRuntime::new(args, output_format, startup)?;
    let (runtime, exit) = runtime.drive().await;
    runtime.shutdown(exit).await
}

#[cfg(test)]
mod tests;
