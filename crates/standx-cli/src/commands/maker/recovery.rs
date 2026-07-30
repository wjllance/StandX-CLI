use super::ledger::{adopt_order, apply_rest_trade};
use super::model::{
    is_current_run_order, is_maker_order, optional_decimal, position_for_symbol, Decimal,
    StreamHealth,
};
use super::output::{emit_live_fill, ts_now};
use super::pipeline::{fetch_account_audit, AccountAudit};
use crate::cli::OutputFormat;
use anyhow::Result;
use standx_maker::{MakerFill, MakerLedger, MakerStats};
use standx_sdk::account_stream::{
    AccountChannel, AccountEvent, AccountStream, AccountStreamHealth,
};
use standx_sdk::client::StandXClient;
use standx_sdk::models::{Order, OrderStatus, Position, Trade};
use standx_sdk::order_response::{
    OrderCommandSender, OrderResponse, OrderResponseHealth, OrderResponseStream,
};
use std::collections::HashSet;
use std::fmt;
use std::time::Duration;

const MAKER_CLEANUP_VERIFY_INITIAL_DELAY: Duration = Duration::from_millis(500);
const MAKER_CLEANUP_VERIFY_INTERVAL: Duration = Duration::from_secs(1);
const MAKER_CLEANUP_VERIFY_MAX_ATTEMPTS: u32 = 6;
const MAKER_CLEANUP_RETRY_DELAY: Duration = Duration::from_secs(1);

#[derive(Debug)]
pub(super) enum PositionReconciliationCause {
    PositionMismatch,
    UnknownCurrentRunOrder,
    CycleInvalidation,
}

impl PositionReconciliationCause {
    pub(super) fn label(&self) -> &'static str {
        match self {
            Self::PositionMismatch => "position_mismatch",
            Self::UnknownCurrentRunOrder => "unknown_current_run_order",
            Self::CycleInvalidation => "cycle_invalidation",
        }
    }
}

#[derive(Debug)]
pub(super) struct PositionReconciliationError {
    pub(super) expected: f64,
    pub(super) observed: f64,
    pub(super) cause: PositionReconciliationCause,
}

impl PositionReconciliationError {
    pub(super) fn position_mismatch(expected: f64, observed: f64) -> Self {
        Self {
            expected,
            observed,
            cause: PositionReconciliationCause::PositionMismatch,
        }
    }

    pub(super) fn unknown_current_run_order(position: f64) -> Self {
        Self {
            expected: position,
            observed: position,
            cause: PositionReconciliationCause::UnknownCurrentRunOrder,
        }
    }

    pub(super) fn cycle_invalidation(position: f64) -> Self {
        Self {
            expected: position,
            observed: position,
            cause: PositionReconciliationCause::CycleInvalidation,
        }
    }
}

impl fmt::Display for PositionReconciliationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.cause {
            PositionReconciliationCause::PositionMismatch => write!(
                formatter,
                "expected position {:+.8}, venue reported {:+.8}",
                self.expected, self.observed
            ),
            PositionReconciliationCause::UnknownCurrentRunOrder => write!(
                formatter,
                "unknown current-run order requires reconciliation at position {:+.8}",
                self.expected
            ),
            PositionReconciliationCause::CycleInvalidation => write!(
                formatter,
                "account event invalidated active cycle at reconciled position {:+.8}",
                self.expected
            ),
        }
    }
}

impl std::error::Error for PositionReconciliationError {}

/// Marker error: the operator pressed Ctrl+C while a reconnect wait was in
/// progress. The caller routes this to shutdown instead of RecoveryFailed.
#[derive(Debug)]
pub(super) struct ReconnectInterrupted;

impl fmt::Display for ReconnectInterrupted {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "order-response reconnect interrupted by Ctrl+C")
    }
}

impl std::error::Error for ReconnectInterrupted {}

/// Marker error for a reconnect round that exhausted only retryable transport
/// work. The runtime remains frozen and schedules another round.
#[derive(Debug)]
pub(super) struct TransportReconnectExhausted {
    pub(super) reason: String,
}

impl fmt::Display for TransportReconnectExhausted {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.reason)
    }
}

impl std::error::Error for TransportReconnectExhausted {}

/// Marker error for a cleanup that already consumed its bounded retry budget.
/// Without an authoritative empty maker book the runtime must still stop.
#[derive(Debug)]
pub(super) struct ReconnectCleanupFailed {
    pub(super) reason: String,
}

impl fmt::Display for ReconnectCleanupFailed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.reason)
    }
}

impl std::error::Error for ReconnectCleanupFailed {}

fn sdk_reconnect_error_is_terminal(error: &standx_sdk::Error) -> bool {
    matches!(
        error,
        standx_sdk::Error::AuthRequired { .. }
            | standx_sdk::Error::InvalidCredentials { .. }
            | standx_sdk::Error::TokenExpired { .. }
            | standx_sdk::Error::Config { .. }
            | standx_sdk::Error::Validation { .. }
            | standx_sdk::Error::Api {
                code: 401 | 403,
                ..
            }
            | standx_sdk::Error::Http {
                code: 401 | 403,
                ..
            }
    )
}

pub(super) fn reconnect_error_is_terminal(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<standx_sdk::Error>()
        .is_some_and(sdk_reconnect_error_is_terminal)
}

/// Resolves once the runtime's Ctrl+C latch has been set (see runtime.rs);
/// pends forever if the listener is gone so callers' selects don't spin.
pub(super) async fn ctrl_c_latched(ctrl_c: &mut tokio::sync::watch::Receiver<bool>) {
    if ctrl_c.wait_for(|pressed| *pressed).await.is_err() {
        std::future::pending::<()>().await;
    }
}

pub(super) async fn recover_current_run_order_ids_for_reconciliation(
    client: &StandXClient,
    trades: &[Trade],
    gap: PositionGap<'_>,
    ledger: &mut MakerLedger,
) {
    const MAX_ORDER_LOOKUPS: usize = 8;
    let position_gap = gap.observed - gap.expected;
    if position_gap.abs() <= gap.qty_tolerance {
        return;
    }

    let mut candidate_ids = HashSet::new();
    for trade in trades {
        let Some(order_id) = trade.order_id else {
            continue;
        };
        if ledger.maker_order_ids.contains(&order_id) {
            continue;
        }
        let side = match trade
            .side
            .as_deref()
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("buy") => 1.0,
            Some("sell") => -1.0,
            _ => continue,
        };
        let Some(qty) = optional_decimal(&trade.qty, Decimal::Positive) else {
            continue;
        };
        if side * position_gap <= 0.0 || qty > position_gap.abs() + gap.qty_tolerance {
            continue;
        }
        candidate_ids.insert(order_id);
        if candidate_ids.len() == MAX_ORDER_LOOKUPS {
            break;
        }
    }

    for order_id in candidate_ids {
        match client.get_order(order_id).await {
            Ok(order) => {
                if let Err(error) = adopt_order(ledger, &order, gap.run_order_prefix) {
                    eprintln!(
                        "⚠️  reconciliation order lookup returned invalid order {}: {}",
                        order_id, error
                    );
                }
            }
            Err(error) => eprintln!(
                "⚠️  reconciliation order lookup for {} failed: {}",
                order_id, error
            ),
        }
    }
}

pub(super) struct PositionGap<'a> {
    pub(super) expected: f64,
    pub(super) observed: f64,
    pub(super) qty_tolerance: f64,
    pub(super) run_order_prefix: &'a str,
}

pub(super) async fn reconcile_ledger_snapshot(
    client: &StandXClient,
    request: ReconcileRequest<'_>,
    ledger: &mut MakerLedger,
    stats: &mut MakerStats,
) -> Result<(f64, Vec<MakerFill>)> {
    let now = chrono::Utc::now().timestamp();
    let audit =
        fetch_account_audit(client, request.symbol, request.session_started_at, now).await?;
    reconcile_account_audit(client, request, audit, now, ledger, stats).await
}

async fn reconcile_account_audit(
    client: &StandXClient,
    request: ReconcileRequest<'_>,
    audit: AccountAudit,
    now: i64,
    ledger: &mut MakerLedger,
    stats: &mut MakerStats,
) -> Result<(f64, Vec<MakerFill>)> {
    let AccountAudit {
        open_orders,
        positions,
        filled_orders,
        trades,
        // Funding is attribution-only and is owned by the periodic audit, whose
        // id dedup set lives in the poll state. Recovery reconciles positions
        // and orders; folding cashflows in here would need that same set.
        funding: _,
    } = audit;
    for order in open_orders.iter().chain(filled_orders.iter()) {
        adopt_order(ledger, order, request.run_order_prefix)?;
    }
    let observed = position_for_symbol(&positions, request.symbol)?;
    recover_current_run_order_ids_for_reconciliation(
        client,
        &trades,
        PositionGap {
            expected: ledger.expected_position,
            observed,
            qty_tolerance: request.qty_tolerance,
            run_order_prefix: request.run_order_prefix,
        },
        ledger,
    )
    .await;
    let mut fills = Vec::new();
    for trade in trades {
        apply_rest_trade(
            ledger,
            trade,
            request.session_started_at,
            now,
            request.mark,
            stats,
            &mut fills,
        )?;
    }
    Ok((observed, fills))
}

pub(super) struct ReconcileRequest<'a> {
    pub(super) symbol: &'a str,
    pub(super) session_started_at: i64,
    pub(super) run_order_prefix: &'a str,
    pub(super) qty_tolerance: f64,
    pub(super) mark: f64,
}

pub(super) enum ConvergenceProbe {
    Converged {
        observed: f64,
    },
    Pending {
        observed: f64,
    },
    /// The REST snapshot failed; the caller reports it its own way and keeps
    /// its previously observed position.
    SnapshotFailed(anyhow::Error),
}

/// One iteration of the bounded position-convergence window shared by the
/// account-stream and position-reconciliation recovery paths: REST-reconcile
/// the ledger, emit every newly explained fill, count fills into `fills_sink`,
/// and compare the observed venue position against `ledger.expected_position`
/// at `qty_tolerance`. The caller owns the retry loop, its delays, and the
/// preceding account-event drain.
pub(super) async fn probe_position_convergence(
    client: &StandXClient,
    request: ReconcileRequest<'_>,
    ledger: &mut MakerLedger,
    stats: &mut MakerStats,
    fills_sink: &mut u64,
    cycle: u64,
    output_format: OutputFormat,
) -> ConvergenceProbe {
    let symbol = request.symbol;
    let qty_tolerance = request.qty_tolerance;
    match reconcile_ledger_snapshot(client, request, ledger, stats).await {
        Ok((observed, fills)) => {
            *fills_sink += fills.len() as u64;
            for fill in &fills {
                emit_live_fill(fill, symbol, cycle, output_format);
            }
            if (observed - ledger.expected_position).abs() <= qty_tolerance {
                ConvergenceProbe::Converged { observed }
            } else {
                ConvergenceProbe::Pending { observed }
            }
        }
        Err(error) => ConvergenceProbe::SnapshotFailed(error),
    }
}

fn cleanup_orders_json(snapshots: &[OrderStatusSnapshot]) -> Vec<serde_json::Value> {
    snapshots
        .iter()
        .map(|s| {
            serde_json::json!({
                "order_id": s.order_id,
                "status": format!("{:?}", s.status).to_lowercase(),
                "updated_at": s.updated_at,
            })
        })
        .collect()
}

pub(super) async fn cancel_maker_orders_with_retry(
    client: &StandXClient,
    symbol: &str,
    attempts: u32,
    output_format: OutputFormat,
) -> Result<()> {
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 1..=attempts {
        let result = cleanup_once(client, symbol).await;
        match result {
            Ok(CleanupVerification::Complete(snapshots)) => {
                if output_format == OutputFormat::Json {
                    println!(
                        "{}",
                        serde_json::json!({
                            "ts": ts_now(),
                            "symbol": symbol,
                            "action": "maker_cleanup",
                            "event": "complete",
                            "orders": cleanup_orders_json(&snapshots),
                        })
                    );
                } else {
                    println!("✅ All maker-owned {} orders cancelled", symbol);
                }
                return Ok(());
            }
            Ok(CleanupVerification::Residual(snapshots)) => {
                let residual_ids: Vec<String> = snapshots
                    .iter()
                    .filter(|s| !s.status.is_terminal())
                    .map(|s| s.order_id.to_string())
                    .collect();
                let message = format!(
                    "RESIDUAL MAKER ORDERS on {} after cancellation: [{}]",
                    symbol,
                    residual_ids.join(", ")
                );
                // Precursor signal: an incomplete cleanup retry often precedes a
                // failed shutdown. Emit it on stdout (JSON mode) so the ingest
                // pipeline uploads it, instead of leaving it only in local stderr.
                if output_format == OutputFormat::Json {
                    println!(
                        "{}",
                        serde_json::json!({
                            "ts": ts_now(),
                            "symbol": symbol,
                            "action": "maker_cleanup",
                            "event": "retry_incomplete",
                            "severity": "warning",
                            "attempt": attempt,
                            "max_attempts": attempts,
                            "message": message,
                            "orders": cleanup_orders_json(&snapshots),
                        })
                    );
                } else {
                    eprintln!(
                        "⚠️  maker-order cancellation attempt {}/{} incomplete: {}",
                        attempt, attempts, message
                    );
                }
                last_err = Some(anyhow::anyhow!(message));
                if attempt < attempts {
                    tokio::time::sleep(MAKER_CLEANUP_RETRY_DELAY).await;
                }
            }
            Err(error) => {
                if output_format == OutputFormat::Json {
                    println!(
                        "{}",
                        serde_json::json!({
                            "ts": ts_now(),
                            "symbol": symbol,
                            "action": "maker_cleanup",
                            "event": "retry_incomplete",
                            "severity": "warning",
                            "attempt": attempt,
                            "max_attempts": attempts,
                            "message": error.to_string(),
                        })
                    );
                } else {
                    eprintln!(
                        "⚠️  maker-order cancellation attempt {}/{} incomplete: {}",
                        attempt, attempts, error
                    );
                }
                last_err = Some(error);
                if attempt < attempts {
                    tokio::time::sleep(MAKER_CLEANUP_RETRY_DELAY).await;
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| {
        anyhow::anyhow!(
            "maker-order cancellation had no attempts — inspect or cancel manually with 'standx order cancel-all {}'",
            symbol
        )
    }))
}

/// Per-order status observed through `/api/query_order` during cleanup
/// verification.
#[derive(Debug)]
struct OrderStatusSnapshot {
    order_id: u64,
    status: OrderStatus,
    updated_at: String,
}

/// Outcome of a single cleanup verification pass.
#[derive(Debug)]
enum CleanupVerification {
    /// Every tracked maker order reached a terminal status.
    Complete(Vec<OrderStatusSnapshot>),
    /// At least one tracked maker order is still active after polling.
    Residual(Vec<OrderStatusSnapshot>),
}

async fn cleanup_once(client: &StandXClient, symbol: &str) -> Result<CleanupVerification> {
    let orders = client.get_open_orders(Some(symbol)).await?;
    let maker_orders = orders
        .into_iter()
        .filter(is_maker_order)
        .map(|order| {
            let id = order.id.parse::<i64>().map_err(|_| {
                anyhow::anyhow!(
                    "maker-owned order has non-integer exchange ID '{}'",
                    order.id
                )
            })?;
            Ok((order.id, id))
        })
        .collect::<Result<Vec<_>>>()?;
    if maker_orders.is_empty() {
        return Ok(CleanupVerification::Complete(Vec::new()));
    }

    let order_ids_i64: Vec<i64> = maker_orders.iter().map(|(_, id)| *id).collect();
    client.cancel_orders(&order_ids_i64).await?;

    tokio::time::sleep(MAKER_CLEANUP_VERIFY_INITIAL_DELAY).await;

    let mut snapshots = Vec::with_capacity(maker_orders.len());
    let mut residual_ids = Vec::new();
    for (id_str, id_i64) in maker_orders {
        let id_u64 = id_i64 as u64;
        let mut last_status = OrderStatus::New;
        let mut last_updated_at = String::new();
        let mut resolved = false;

        for attempt in 1..=MAKER_CLEANUP_VERIFY_MAX_ATTEMPTS {
            match client.get_order(id_u64).await {
                Ok(order) => {
                    last_status = order.status;
                    last_updated_at = order.updated_at;
                    if last_status.is_terminal() {
                        resolved = true;
                        break;
                    }
                    if attempt < MAKER_CLEANUP_VERIFY_MAX_ATTEMPTS {
                        tokio::time::sleep(MAKER_CLEANUP_VERIFY_INTERVAL).await;
                    }
                }
                Err(error) => {
                    return Err(anyhow::anyhow!(
                        "order status query failed for {}: {}",
                        id_str,
                        error
                    ));
                }
            }
        }

        snapshots.push(OrderStatusSnapshot {
            order_id: id_u64,
            status: last_status,
            updated_at: last_updated_at,
        });

        if !resolved {
            residual_ids.push(id_str);
        }
    }

    if residual_ids.is_empty() {
        Ok(CleanupVerification::Complete(snapshots))
    } else {
        Ok(CleanupVerification::Residual(snapshots))
    }
}

#[derive(Debug, PartialEq)]
pub(super) struct ReconnectSnapshot {
    pub(super) position: f64,
    pub(super) maker_filled_orders: usize,
    pub(super) maker_trades: usize,
}

pub(super) struct ReconnectedOrderResponse {
    pub(super) commands: OrderCommandSender,
    pub(super) responses: tokio::sync::mpsc::Receiver<OrderResponse>,
    pub(super) health: OrderResponseHealth,
    pub(super) handle: tokio::task::JoinHandle<()>,
    pub(super) position: f64,
    pub(super) fills: Vec<MakerFill>,
}

fn emit_order_response_reconnect(
    output_format: OutputFormat,
    symbol: &str,
    event: &str,
    attempt: u32,
    max_attempts: u32,
    message: &str,
) {
    if output_format == OutputFormat::Json {
        println!(
            "{}",
            serde_json::json!({
                "ts": ts_now(),
                "symbol": symbol,
                "action": "order_response_reconnect",
                "event": event,
                "attempt": attempt,
                "max_attempts": max_attempts,
                "message": message,
            })
        );
    } else {
        eprintln!(
            "⚠️  order-response reconnect {event} ({attempt}/{max_attempts}) on {symbol}: {message}"
        );
    }
}

pub(super) fn validate_reconnect_snapshot(
    symbol: &str,
    run_order_prefix: &str,
    open_orders: &[Order],
    positions: &[Position],
    filled_orders: &[Order],
    trades: &[Trade],
) -> Result<ReconnectSnapshot> {
    let residual_ids = open_orders
        .iter()
        .filter(|order| is_maker_order(order))
        .map(|order| order.id.as_str())
        .collect::<Vec<_>>();
    if !residual_ids.is_empty() {
        return Err(anyhow::anyhow!(
            "maker orders appeared after cleanup on {symbol}: [{}]",
            residual_ids.join(", ")
        ));
    }

    let position = position_for_symbol(positions, symbol).map_err(|error| {
        anyhow::anyhow!("reconnect reconciliation found invalid position on {symbol}: {error}")
    })?;

    let maker_filled_order_ids = filled_orders
        .iter()
        .filter(|order| is_current_run_order(order, run_order_prefix))
        .map(|order| {
            order.id.parse::<u64>().map_err(|_| {
                anyhow::anyhow!(
                    "reconnect reconciliation found non-integer maker order ID '{}'",
                    order.id
                )
            })
        })
        .collect::<Result<HashSet<_>>>()?;
    let maker_trades = trades
        .iter()
        .filter(|trade| {
            trade
                .order_id
                .is_some_and(|order_id| maker_filled_order_ids.contains(&order_id))
        })
        .map(|trade| {
            if trade.id == 0 {
                Err(anyhow::anyhow!(
                    "reconnect reconciliation found maker trade without a stable trade ID"
                ))
            } else {
                Ok(())
            }
        })
        .collect::<Result<Vec<_>>>()?
        .len();

    Ok(ReconnectSnapshot {
        position,
        maker_filled_orders: maker_filled_order_ids.len(),
        maker_trades,
    })
}

async fn query_reconnect_snapshot(
    client: &StandXClient,
    request: ReconcileRequest<'_>,
    ledger: &mut MakerLedger,
    stats: &mut MakerStats,
) -> Result<(ReconnectSnapshot, Vec<MakerFill>)> {
    let now = chrono::Utc::now().timestamp();
    let audit =
        fetch_account_audit(client, request.symbol, request.session_started_at, now).await?;
    reconcile_reconnect_audit(client, request, audit, now, ledger, stats).await
}

async fn reconcile_reconnect_audit(
    client: &StandXClient,
    request: ReconcileRequest<'_>,
    audit: AccountAudit,
    now: i64,
    ledger: &mut MakerLedger,
    stats: &mut MakerStats,
) -> Result<(ReconnectSnapshot, Vec<MakerFill>)> {
    let snapshot = validate_reconnect_snapshot(
        request.symbol,
        request.run_order_prefix,
        &audit.open_orders,
        &audit.positions,
        &audit.filled_orders,
        &audit.trades,
    )?;
    let (_, fills) = reconcile_account_audit(client, request, audit, now, ledger, stats).await?;
    Ok((snapshot, fills))
}

/// The live halves of a freshly authenticated account stream.
pub(super) type AccountStreamConnection = (
    tokio::sync::mpsc::Receiver<AccountEvent>,
    AccountStreamHealth,
    tokio::task::JoinHandle<()>,
);

/// Terminal outcome of the account-stream reconnect loop, mirroring the
/// order-response reconnect: either a live connection, an operator Ctrl+C, or
/// the attempt budget exhausted.
pub(super) enum AccountStreamReconnect {
    Connected(AccountStreamConnection),
    Interrupted,
    Terminal(String),
    Exhausted(String),
}

/// Reconnect the authenticated account stream with bounded attempts and
/// exponential backoff, both interruptible by Ctrl+C. Bumps `epoch` per
/// attempt so the caller's projection reset follows the connected stream. The
/// maker book is already cancelled by the completed cleanup, so
/// aborting the waits on Ctrl+C is safe. Symmetric with
/// [`reconnect_order_response`]; the caller owns the post-connect event
/// application and REST reconciliation (account-stream-specific).
pub(super) async fn reconnect_account_stream(
    epoch: &mut u64,
    max_attempts: u32,
    backoff_secs: u64,
    ctrl_c: &mut tokio::sync::watch::Receiver<bool>,
) -> AccountStreamReconnect {
    let mut last_connect_error: Option<String> = None;
    for attempt in 1..=max_attempts {
        *epoch = epoch.saturating_add(1);
        let connect_epoch = *epoch;
        let reconnect = async {
            let stream = AccountStream::new(connect_epoch)?;
            stream
                .connect(&[
                    AccountChannel::Order,
                    AccountChannel::Position,
                    AccountChannel::Trade,
                    AccountChannel::Balance,
                ])
                .await
                .map_err(anyhow::Error::from)
        };
        let connect_attempt = tokio::select! {
            biased;
            _ = ctrl_c_latched(ctrl_c) => None,
            result = tokio::time::timeout(Duration::from_secs(15), reconnect) => Some(result),
        };
        let Some(connect_attempt) = connect_attempt else {
            return AccountStreamReconnect::Interrupted;
        };
        match connect_attempt {
            Ok(Ok(triple)) => return AccountStreamReconnect::Connected(triple),
            Ok(Err(error)) if reconnect_error_is_terminal(&error) => {
                return AccountStreamReconnect::Terminal(error.to_string());
            }
            Ok(Err(error)) => last_connect_error = Some(format!("connect failed: {error}")),
            Err(_) => last_connect_error = Some("connect timed out after 15 seconds".to_string()),
        }
        eprintln!(
            "⚠️  account stream reconnect attempt {}/{} failed: {}",
            attempt,
            max_attempts,
            last_connect_error.as_deref().unwrap_or("unknown error")
        );
        if attempt < max_attempts {
            let multiplier = 1_u32 << attempt.saturating_sub(1).min(4);
            let backoff = Duration::from_secs(backoff_secs).saturating_mul(multiplier);
            tokio::select! {
                biased;
                _ = ctrl_c_latched(ctrl_c) => return AccountStreamReconnect::Interrupted,
                _ = tokio::time::sleep(backoff) => {}
            }
        }
    }
    AccountStreamReconnect::Exhausted(
        last_connect_error.unwrap_or_else(|| "no attempts available".to_string()),
    )
}

pub(super) struct ReconnectRequest<'a> {
    pub(super) cleanup_client: StandXClient,
    pub(super) symbol: &'a str,
    pub(super) session_started_at: i64,
    pub(super) run_order_prefix: &'a str,
    pub(super) qty_tolerance: f64,
    pub(super) mark: f64,
    pub(super) output_format: OutputFormat,
    pub(super) max_attempts: u32,
    pub(super) base_backoff: Duration,
    pub(super) original_failure: &'a str,
    pub(super) ctrl_c: tokio::sync::watch::Receiver<bool>,
}

pub(super) async fn reconnect_order_response(
    request: ReconnectRequest<'_>,
    ledger: &mut MakerLedger,
    stats: &mut MakerStats,
    recovered_fills: &mut Vec<MakerFill>,
) -> Result<ReconnectedOrderResponse> {
    let ReconnectRequest {
        cleanup_client,
        symbol,
        session_started_at,
        run_order_prefix,
        qty_tolerance,
        mark,
        output_format,
        max_attempts,
        base_backoff,
        original_failure,
        ctrl_c,
    } = request;
    let mut ctrl_c = ctrl_c;
    let mut last_error = None;
    // The runtime Cleanup effect has already emptied and verified the maker
    // book before it emits Recover. Only repeat cleanup between failed
    // reconnect attempts, when a late venue-side request may have surfaced.
    let mut cleanup_needed = false;

    for attempt in 1..=max_attempts {
        emit_order_response_reconnect(
            output_format,
            symbol,
            "starting",
            attempt,
            max_attempts,
            original_failure,
        );

        let cleanup_ok = if cleanup_needed {
            match cancel_maker_orders_with_retry(&cleanup_client, symbol, 3, output_format).await {
                Ok(()) => true,
                Err(error) => {
                    return Err(anyhow::Error::new(ReconnectCleanupFailed {
                        reason: format!("retry cleanup failed: {error}"),
                    }));
                }
            }
        } else {
            true
        };
        if cleanup_ok {
            // Give just-submitted HTTP orders time to become visible, then
            // require a second authoritative snapshot after authentication.
            // The maker book is verified empty at this point, so aborting the
            // reconnect waits on Ctrl+C is safe.
            tokio::select! {
                biased;
                _ = ctrl_c_latched(&mut ctrl_c) => {
                    return Err(anyhow::Error::new(ReconnectInterrupted));
                }
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
            let session_id = uuid::Uuid::new_v4().to_string();
            let stream = OrderResponseStream::new(&session_id)?;
            let connect_attempt = tokio::select! {
                biased;
                _ = ctrl_c_latched(&mut ctrl_c) => {
                    return Err(anyhow::Error::new(ReconnectInterrupted));
                }
                result = tokio::time::timeout(Duration::from_secs(15), stream.connect()) => result,
            };
            match connect_attempt {
                Ok(Ok((commands, responses, health, handle))) => 'reconcile: {
                    let mut snapshot = match query_reconnect_snapshot(
                        &cleanup_client,
                        ReconcileRequest {
                            symbol,
                            session_started_at,
                            run_order_prefix,
                            qty_tolerance,
                            mark,
                        },
                        ledger,
                        stats,
                    )
                    .await
                    {
                        Ok((snapshot, fills)) => {
                            recovered_fills.extend(fills);
                            snapshot
                        }
                        Err(error) => {
                            handle.abort();
                            last_error =
                                Some(anyhow::anyhow!("post-auth reconciliation failed: {error}"));
                            break 'reconcile;
                        }
                    };

                    if (snapshot.position - ledger.expected_position).abs() > qty_tolerance {
                        let mut gap_closed = false;
                        for delay in [500_u64, 1_000, 1_500] {
                            tokio::select! {
                                biased;
                                _ = ctrl_c_latched(&mut ctrl_c) => {
                                    handle.abort();
                                    return Err(anyhow::Error::new(ReconnectInterrupted));
                                }
                                _ = tokio::time::sleep(Duration::from_millis(delay)) => {}
                            }
                            match query_reconnect_snapshot(
                                &cleanup_client,
                                ReconcileRequest {
                                    symbol,
                                    session_started_at,
                                    run_order_prefix,
                                    qty_tolerance,
                                    mark,
                                },
                                ledger,
                                stats,
                            )
                            .await
                            {
                                Ok((next_snapshot, fills)) => {
                                    snapshot = next_snapshot;
                                    recovered_fills.extend(fills);
                                    if (snapshot.position - ledger.expected_position).abs()
                                        <= qty_tolerance
                                    {
                                        gap_closed = true;
                                        break;
                                    }
                                }
                                Err(error) => eprintln!(
                                    "⚠️  order-response reconnect REST trade backfill failed: {error}"
                                ),
                            }
                        }
                        if !gap_closed {
                            handle.abort();
                            return Err(anyhow::Error::new(
                                PositionReconciliationError::position_mismatch(
                                    ledger.expected_position,
                                    snapshot.position,
                                ),
                            ));
                        }
                    }

                    if !health.is_healthy() {
                        let reason = health.failure_detail();
                        handle.abort();
                        last_error = Some(anyhow::anyhow!(
                            "new order-response session failed during reconciliation: {reason}"
                        ));
                    } else {
                        let message = format!(
                            "authenticated new session {}; maker book empty; position={:+.8}; maker filled orders={}; maker trades={}",
                            session_id,
                            snapshot.position,
                            snapshot.maker_filled_orders,
                            snapshot.maker_trades,
                        );
                        emit_order_response_reconnect(
                            output_format,
                            symbol,
                            "complete",
                            attempt,
                            max_attempts,
                            &message,
                        );
                        return Ok(ReconnectedOrderResponse {
                            commands,
                            responses,
                            health,
                            handle,
                            position: snapshot.position,
                            fills: std::mem::take(recovered_fills),
                        });
                    }
                }
                Ok(Err(error)) if sdk_reconnect_error_is_terminal(&error) => {
                    return Err(anyhow::Error::new(error));
                }
                Ok(Err(error)) => {
                    last_error = Some(anyhow::anyhow!(
                        "order-response authentication failed: {error}"
                    ));
                }
                Err(_) => {
                    last_error = Some(anyhow::anyhow!(
                        "order-response reconnect timed out after 15 seconds"
                    ));
                }
            }
        }
        cleanup_needed = true;

        let error_text = last_error
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| "unknown reconnect failure".to_string());
        emit_order_response_reconnect(
            output_format,
            symbol,
            "attempt_failed",
            attempt,
            max_attempts,
            &error_text,
        );
        if attempt < max_attempts {
            let local_attempt = attempt.saturating_sub(1).min(4);
            let multiplier = 1_u32 << local_attempt;
            tokio::select! {
                biased;
                _ = ctrl_c_latched(&mut ctrl_c) => {
                    return Err(anyhow::Error::new(ReconnectInterrupted));
                }
                _ = tokio::time::sleep(base_backoff.saturating_mul(multiplier)) => {}
            }
        }
    }

    Err(anyhow::Error::new(TransportReconnectExhausted {
        reason: format!(
            "safe order-response reconnect exhausted: {}",
            last_error
                .map(|error| error.to_string())
                .unwrap_or_else(|| "no attempts available".to_string())
        ),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use standx_sdk::models::{OrderSide, OrderStatus, OrderType};

    const SYMBOL: &str = "XAG-USD";
    const RUN_PREFIX: &str = "sxmk-reconnect-";
    const ORDER_ID: u64 = 11_575_317_826;
    const TRADE_ID: u64 = 900_001;

    /// Snapshot-validation fixtures. Deliberately separate from the
    /// reconciliation fixtures above: these exercise ownership and trade-ID
    /// stability, so they need to vary the client order ID freely.
    fn snapshot_order(id: &str, cl_ord_id: Option<&str>) -> Order {
        Order {
            id: id.to_string(),
            cl_ord_id: cl_ord_id.map(str::to_string),
            symbol: SYMBOL.to_string(),
            side: OrderSide::Buy,
            order_type: OrderType::Limit,
            qty: "0.2".to_string(),
            fill_qty: "0".to_string(),
            price: "59.40".to_string(),
            status: OrderStatus::New,
            created_at: "now".to_string(),
            updated_at: "now".to_string(),
        }
    }

    fn snapshot_position(side: &str, qty: &str) -> Position {
        serde_json::from_value(serde_json::json!({
            "id": 1,
            "symbol": SYMBOL,
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

    fn snapshot_trade(id: u64, order_id: u64) -> Trade {
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
            symbol: Some(SYMBOL.to_string()),
            value: None,
        }
    }

    #[test]
    fn reconnect_snapshot_requires_empty_maker_book_and_valid_ledger() {
        let manual = snapshot_order("99", Some("manual-order"));
        let filled = snapshot_order("42", Some("sxmk-filled"));
        let snapshot = validate_reconnect_snapshot(
            SYMBOL,
            "sxmk-",
            &[manual],
            &[snapshot_position("sell", "0.2")],
            &[filled],
            &[snapshot_trade(7, 42)],
        )
        .unwrap();

        assert_eq!(snapshot.position, -0.2);
        assert_eq!(snapshot.maker_filled_orders, 1);
        assert_eq!(snapshot.maker_trades, 1);
    }

    #[test]
    fn reconnect_snapshot_rejects_residual_maker_order() {
        let error = validate_reconnect_snapshot(
            SYMBOL,
            "sxmk-",
            &[snapshot_order("42", Some("sxmk-still-open"))],
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
            SYMBOL,
            "sxmk-",
            &[],
            &[],
            &[snapshot_order("42", Some("sxmk-filled"))],
            &[snapshot_trade(0, 42)],
        )
        .unwrap_err();

        assert!(error.to_string().contains("stable trade ID"));
    }

    #[test]
    fn reconnect_error_classification_keeps_auth_terminal_and_transport_retryable() {
        let auth = anyhow::Error::new(standx_sdk::Error::AuthRequired {
            message: "expired".to_string(),
            resolution: "login".to_string(),
        });
        assert!(reconnect_error_is_terminal(&auth));

        let transport = anyhow::Error::new(standx_sdk::Error::WebSocket {
            message: "connection reset".to_string(),
        });
        assert!(!reconnect_error_is_terminal(&transport));
    }

    fn filled_sell_order() -> Order {
        Order {
            id: ORDER_ID.to_string(),
            cl_ord_id: Some(format!("{RUN_PREFIX}q0000028cs0")),
            symbol: SYMBOL.to_string(),
            side: OrderSide::Sell,
            order_type: OrderType::Limit,
            qty: "0.2".to_string(),
            fill_qty: "0.2".to_string(),
            price: "58.23".to_string(),
            status: OrderStatus::Filled,
            created_at: "2026-07-15T08:27:04Z".to_string(),
            updated_at: "2026-07-15T08:28:19Z".to_string(),
        }
    }

    fn short_position() -> Position {
        serde_json::from_value(serde_json::json!({
            "id": 1,
            "symbol": SYMBOL,
            "side": "short",
            "qty": "0.2",
            "entry_price": "58.23",
            "entry_value": "11.646",
            "holding_margin": "1",
            "initial_margin": "1",
            "leverage": "1",
            "mark_price": "58.20",
            "margin_asset": "USDT",
            "margin_mode": "cross",
            "position_value": "11.64",
            "realized_pnl": "0",
            "required_margin": "1",
            "status": "open",
            "upnl": "0.006",
            "time": "2026-07-15T08:28:22Z",
            "created_at": "2026-07-15T08:28:19Z",
            "updated_at": "2026-07-15T08:28:22Z",
            "user": "test"
        }))
        .unwrap()
    }

    fn sell_trade(now: i64) -> Trade {
        Trade {
            id: TRADE_ID,
            time: chrono::DateTime::from_timestamp(now, 0)
                .unwrap()
                .to_rfc3339(),
            price: "58.23".to_string(),
            qty: "0.2".to_string(),
            side: Some("sell".to_string()),
            is_buyer_taker: false,
            fee_asset: None,
            fee_qty: None,
            pnl: None,
            order_id: Some(ORDER_ID),
            symbol: Some(SYMBOL.to_string()),
            value: Some("11.646".to_string()),
        }
    }

    fn filled_audit(now: i64) -> AccountAudit {
        AccountAudit {
            open_orders: Vec::new(),
            positions: vec![short_position()],
            filled_orders: vec![filled_sell_order()],
            trades: vec![sell_trade(now)],
            funding: Ok(Vec::new()),
        }
    }

    fn unexplained_audit() -> AccountAudit {
        AccountAudit {
            open_orders: Vec::new(),
            positions: vec![short_position()],
            filled_orders: Vec::new(),
            trades: Vec::new(),
            funding: Ok(Vec::new()),
        }
    }

    #[tokio::test]
    async fn cancel_race_fill_is_backfilled_before_reconnect_position_check() {
        let now = chrono::Utc::now().timestamp();
        let client = StandXClient::new().unwrap();
        let mut ledger = MakerLedger::new(0.0);
        let mut stats = MakerStats::with_inventory_baseline(0.0, 58.20);

        let (snapshot, fills) = reconcile_reconnect_audit(
            &client,
            ReconcileRequest {
                symbol: SYMBOL,
                session_started_at: now - 60,
                run_order_prefix: RUN_PREFIX,
                qty_tolerance: 0.0005,
                mark: 58.20,
            },
            filled_audit(now),
            now,
            &mut ledger,
            &mut stats,
        )
        .await
        .unwrap();

        assert_eq!(snapshot.position, -0.2);
        assert_eq!(snapshot.maker_filled_orders, 1);
        assert_eq!(snapshot.maker_trades, 1);
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].trade_id, Some(TRADE_ID));
        assert_eq!(ledger.expected_position, -0.2);
        assert_eq!(stats.position(), -0.2);
        assert!((snapshot.position - ledger.expected_position).abs() <= 0.0005);
    }

    #[tokio::test]
    async fn repeated_reconnect_snapshot_deduplicates_rest_fill() {
        let now = chrono::Utc::now().timestamp();
        let client = StandXClient::new().unwrap();
        let mut ledger = MakerLedger::new(0.0);
        let mut stats = MakerStats::with_inventory_baseline(0.0, 58.20);
        let request = || ReconcileRequest {
            symbol: SYMBOL,
            session_started_at: now - 60,
            run_order_prefix: RUN_PREFIX,
            qty_tolerance: 0.0005,
            mark: 58.20,
        };

        let (_, first) = reconcile_reconnect_audit(
            &client,
            request(),
            filled_audit(now),
            now,
            &mut ledger,
            &mut stats,
        )
        .await
        .unwrap();
        let (_, duplicate) = reconcile_reconnect_audit(
            &client,
            request(),
            filled_audit(now),
            now,
            &mut ledger,
            &mut stats,
        )
        .await
        .unwrap();

        assert_eq!(first.len(), 1);
        assert!(duplicate.is_empty());
        assert_eq!(ledger.expected_position, -0.2);
        assert_eq!(stats.sell_fills, 1);
    }

    #[tokio::test]
    async fn unexplained_reconnect_position_remains_fail_closed() {
        let now = chrono::Utc::now().timestamp();
        let client = StandXClient::new().unwrap();
        let mut ledger = MakerLedger::new(0.0);
        let mut stats = MakerStats::with_inventory_baseline(0.0, 58.20);

        let (snapshot, fills) = reconcile_reconnect_audit(
            &client,
            ReconcileRequest {
                symbol: SYMBOL,
                session_started_at: now - 60,
                run_order_prefix: RUN_PREFIX,
                qty_tolerance: 0.0005,
                mark: 58.20,
            },
            unexplained_audit(),
            now,
            &mut ledger,
            &mut stats,
        )
        .await
        .unwrap();

        assert!(fills.is_empty());
        assert_eq!(ledger.expected_position, 0.0);
        assert!((snapshot.position - ledger.expected_position).abs() > 0.0005);
    }

    struct JwtGuard {
        original: Option<String>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl JwtGuard {
        fn set() -> Self {
            // Share the crate-wide env lock so this STANDX_JWT mutation cannot
            // race env reads in other modules' tests. See crate::TEST_ENV_LOCK.
            let lock = crate::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let original = std::env::var("STANDX_JWT").ok();
            std::env::set_var("STANDX_JWT", "recovery-test-jwt");
            Self {
                original,
                _lock: lock,
            }
        }
    }

    impl Drop for JwtGuard {
        fn drop(&mut self) {
            match &self.original {
                Some(value) => std::env::set_var("STANDX_JWT", value),
                None => std::env::remove_var("STANDX_JWT"),
            }
        }
    }

    /// Mocks the four REST reads behind `fetch_account_audit` with the given
    /// audit content and returns the server (kept alive by the caller).
    async fn mock_audit_endpoints(
        open_orders: &[Order],
        positions: &[Position],
        filled_orders: &[Order],
        trades: &[Trade],
    ) -> (mockito::ServerGuard, StandXClient) {
        use mockito::{Matcher, Server};
        let mut server = Server::new_async().await;
        let wrapped = |items: String| format!(r#"{{"code":0,"message":"ok","result":{items}}}"#);
        server
            .mock("GET", "/api/query_open_orders")
            .match_query(Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(wrapped(serde_json::to_string(open_orders).unwrap()))
            .create_async()
            .await;
        server
            .mock("GET", "/api/query_positions")
            .match_query(Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(serde_json::to_string(positions).unwrap())
            .create_async()
            .await;
        server
            .mock("GET", "/api/query_orders")
            .match_query(Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(wrapped(serde_json::to_string(filled_orders).unwrap()))
            .create_async()
            .await;
        server
            .mock("GET", "/api/query_trades")
            .match_query(Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(wrapped(serde_json::to_string(trades).unwrap()))
            .create_async()
            .await;
        // The audit fetch also reads funding history (a bare array). Recovery
        // ignores those rows, but the request still has to succeed.
        server
            .mock("GET", "/api/query_funding_history")
            .match_query(Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body("[]")
            .create_async()
            .await;
        let client = StandXClient::with_base_url(server.url()).unwrap();
        (server, client)
    }

    /// Invariant: one probe iteration backfills the cancel-race fill exactly
    /// once, counts it into the caller's sink, and reports convergence when
    /// the REST trades explain the whole position gap.
    #[tokio::test]
    async fn probe_backfills_trades_into_the_sink_and_converges() {
        let _jwt = JwtGuard::set();
        let now = chrono::Utc::now().timestamp();
        let (_server, client) = mock_audit_endpoints(
            &[],
            &[short_position()],
            &[filled_sell_order()],
            &[sell_trade(now)],
        )
        .await;
        let mut ledger = MakerLedger::new(0.0);
        let mut stats = MakerStats::with_inventory_baseline(0.0, 58.20);
        let mut fills_sink = 0_u64;

        let probe = probe_position_convergence(
            &client,
            ReconcileRequest {
                symbol: SYMBOL,
                session_started_at: now - 60,
                run_order_prefix: RUN_PREFIX,
                qty_tolerance: 0.0005,
                mark: 58.20,
            },
            &mut ledger,
            &mut stats,
            &mut fills_sink,
            7,
            OutputFormat::Quiet,
        )
        .await;

        assert!(
            matches!(probe, ConvergenceProbe::Converged { observed } if observed == -0.2),
            "REST-explained gap must converge"
        );
        assert_eq!(fills_sink, 1, "the backfilled fill must be counted once");
        assert_eq!(ledger.expected_position, -0.2);
    }

    /// Invariant: an unexplained gap stays pending — the probe must not
    /// invent convergence, and the sink must stay untouched.
    #[tokio::test]
    async fn probe_reports_unexplained_gap_as_pending() {
        let _jwt = JwtGuard::set();
        let now = chrono::Utc::now().timestamp();
        let (_server, client) = mock_audit_endpoints(&[], &[short_position()], &[], &[]).await;
        let mut ledger = MakerLedger::new(0.0);
        let mut stats = MakerStats::with_inventory_baseline(0.0, 58.20);
        let mut fills_sink = 0_u64;

        let probe = probe_position_convergence(
            &client,
            ReconcileRequest {
                symbol: SYMBOL,
                session_started_at: now - 60,
                run_order_prefix: RUN_PREFIX,
                qty_tolerance: 0.0005,
                mark: 58.20,
            },
            &mut ledger,
            &mut stats,
            &mut fills_sink,
            7,
            OutputFormat::Quiet,
        )
        .await;

        assert!(
            matches!(probe, ConvergenceProbe::Pending { observed } if observed == -0.2),
            "an unexplained gap must stay pending"
        );
        assert_eq!(fills_sink, 0);
        assert_eq!(ledger.expected_position, 0.0);
    }

    /// Invariant: a failed REST snapshot is reported as such — the caller
    /// keeps its previous observation and its own error reporting.
    #[tokio::test]
    async fn probe_surfaces_snapshot_failures() {
        use mockito::{Matcher, Server};
        let _jwt = JwtGuard::set();
        let now = chrono::Utc::now().timestamp();
        let mut server = Server::new_async().await;
        server
            .mock("GET", "/api/query_open_orders")
            .match_query(Matcher::Any)
            .with_status(500)
            .with_body("venue unavailable")
            .create_async()
            .await;
        let client = StandXClient::with_base_url(server.url()).unwrap();
        let mut ledger = MakerLedger::new(0.0);
        let mut stats = MakerStats::with_inventory_baseline(0.0, 58.20);
        let mut fills_sink = 0_u64;

        let probe = probe_position_convergence(
            &client,
            ReconcileRequest {
                symbol: SYMBOL,
                session_started_at: now - 60,
                run_order_prefix: RUN_PREFIX,
                qty_tolerance: 0.0005,
                mark: 58.20,
            },
            &mut ledger,
            &mut stats,
            &mut fills_sink,
            7,
            OutputFormat::Quiet,
        )
        .await;

        assert!(matches!(probe, ConvergenceProbe::SnapshotFailed(_)));
        assert_eq!(fills_sink, 0);
    }
}

#[cfg(test)]
mod live_gate_tests {
    //! End-to-end checks over a mocked REST surface: the controlled-disconnect
    //! live gate, cleanup retry against a stale open-order read, and fast
    //! current-run fill recovery by order ID. They live here rather than in the
    //! command module because every one of them drives a `recovery` entry point.

    use super::*;
    // The controlled-disconnect gate spans both halves of the live session: it
    // drains the order-response stream, then verifies the REST cleanup that
    // follows, so it needs the runtime's order-response entry point too.
    use crate::commands::maker::runtime::apply_order_responses;

    use mockito::{Matcher, Server};
    use standx_maker::{MakerAccountProjection, MakerState};

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
        let query_after = server
            .mock("GET", "/api/query_order")
            .match_query(Matcher::UrlEncoded("order_id".into(), "42".into()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"id":"42","cl_ord_id":"sxmk-controlled-buy","symbol":"BTC-USD","side":"buy","order_type":"limit","qty":"0.001","fill_qty":"0","price":"63000","status":"canceled","created_at":"2026-07-10T00:00:00Z","updated_at":"2026-07-10T00:00:01Z"}"#,
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
        query_after.assert_async().await;
    }

    #[tokio::test]
    async fn maker_cleanup_retries_stale_open_order_verification() {
        let _jwt = EnvGuard::set("STANDX_JWT", "controlled-test-jwt");
        let mut server = Server::new_async().await;
        let maker_and_manual = r#"{"code":0,"message":"ok","result":[
            {"id":"42","cl_ord_id":"sxmk-controlled-buy","symbol":"BTC-USD","side":"buy","order_type":"limit","qty":"0.001","fill_qty":"0","price":"63000","status":"open","created_at":"2026-07-10T00:00:00Z","updated_at":"2026-07-10T00:00:00Z"},
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
        let stale_query = server
            .mock("GET", "/api/query_order")
            .match_query(Matcher::UrlEncoded("order_id".into(), "42".into()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"id":"42","cl_ord_id":"sxmk-controlled-buy","symbol":"BTC-USD","side":"buy","order_type":"limit","qty":"0.001","fill_qty":"0","price":"63000","status":"open","created_at":"2026-07-10T00:00:00Z","updated_at":"2026-07-10T00:00:01Z"}"#,
            )
            .expect(6)
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
        let cleared_query = server
            .mock("GET", "/api/query_order")
            .match_query(Matcher::UrlEncoded("order_id".into(), "42".into()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"id":"42","cl_ord_id":"sxmk-controlled-buy","symbol":"BTC-USD","side":"buy","order_type":"limit","qty":"0.001","fill_qty":"0","price":"63000","status":"canceled","created_at":"2026-07-10T00:00:00Z","updated_at":"2026-07-10T00:00:02Z"}"#,
            )
            .expect(1)
            .create_async()
            .await;

        let client = StandXClient::with_base_url(server.url()).unwrap();
        cancel_maker_orders_with_retry(&client, "BTC-USD", 3, OutputFormat::Quiet)
            .await
            .unwrap();

        open_before.assert_async().await;
        cancel_first.assert_async().await;
        stale_query.assert_async().await;
        open_retry.assert_async().await;
        cancel_retry.assert_async().await;
        cleared_query.assert_async().await;
    }

    /// Regression: a stale `/api/query_open_orders` snapshot that still lists
    /// a maker order must not be treated as residual. The authoritative source
    /// is `/api/query_order`; if it reports `canceled`, cleanup succeeds.
    #[tokio::test]
    async fn maker_cleanup_succeeds_when_query_order_shows_canceled() {
        let _jwt = EnvGuard::set("STANDX_JWT", "controlled-test-jwt");
        let mut server = Server::new_async().await;
        let open_before = server
            .mock("GET", "/api/query_open_orders")
            .match_query(Matcher::UrlEncoded("symbol".into(), "BTC-USD".into()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"code":0,"message":"ok","result":[
                    {"id":"42","cl_ord_id":"sxmk-controlled-buy","symbol":"BTC-USD","side":"buy","order_type":"limit","qty":"0.001","fill_qty":"0","price":"63000","status":"open","created_at":"2026-07-10T00:00:00Z","updated_at":"2026-07-10T00:00:00Z"}
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
        let query_order = server
            .mock("GET", "/api/query_order")
            .match_query(Matcher::UrlEncoded("order_id".into(), "42".into()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"id":"42","cl_ord_id":"sxmk-controlled-buy","symbol":"BTC-USD","side":"buy","order_type":"limit","qty":"0.001","fill_qty":"0","price":"63000","status":"canceled","created_at":"2026-07-10T00:00:00Z","updated_at":"2026-07-10T00:00:01Z"}"#,
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
        query_order.assert_async().await;
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
