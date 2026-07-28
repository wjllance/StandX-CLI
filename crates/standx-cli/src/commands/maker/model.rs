use standx_sdk::account_stream::{AccountStreamHealth, OrderUpdate};
use standx_sdk::models::{Order, OrderSide, OrderStatus, Position};
use standx_sdk::order_response::OrderResponseHealth;

/// The health record of one required live stream.
///
/// Both SDK health types expose the same pair of accessors, and every caller
/// asks the same two questions of them: "is this stream still usable?" and
/// "if not, what do I tell the operator?". Answering those once here keeps the
/// fallback wording from drifting between the cycle guard and the two recovery
/// phases.
pub trait StreamHealth {
    /// Operator-facing name of the stream, used in fallback detail strings.
    const LABEL: &'static str;

    fn is_healthy(&self) -> bool;

    fn failure_reason(&self) -> Option<String>;

    /// Why the stream is unusable. A health record that went unhealthy without
    /// recording a reason still has to produce a detail — silence must never
    /// read as "nothing is wrong".
    fn failure_detail(&self) -> String {
        self.failure_reason().unwrap_or_else(|| {
            format!("{} became unhealthy without a recorded reason", Self::LABEL)
        })
    }
}

impl StreamHealth for AccountStreamHealth {
    const LABEL: &'static str = "account stream";

    fn is_healthy(&self) -> bool {
        AccountStreamHealth::is_healthy(self)
    }

    fn failure_reason(&self) -> Option<String> {
        AccountStreamHealth::failure_reason(self)
    }
}

impl StreamHealth for OrderResponseHealth {
    const LABEL: &'static str = "order-response stream";

    fn is_healthy(&self) -> bool {
        OrderResponseHealth::is_healthy(self)
    }

    fn failure_reason(&self) -> Option<String> {
        OrderResponseHealth::failure_reason(self)
    }
}

/// Why a required live stream cannot be used right now, or `None` when it is
/// healthy. A missing health record fails closed: an absent observation is not
/// evidence of health.
pub fn unhealthy_stream<H: StreamHealth>(health: Option<&H>) -> Option<String> {
    match health {
        Some(health) if health.is_healthy() => None,
        Some(health) => Some(health.failure_detail()),
        None => Some(format!("{} health state is unavailable", H::LABEL)),
    }
}

/// Process exit code emitted when the maker performs an *intentional*
/// fail-safe shutdown: order-response or market-data recovery failed, three
/// maker cycles failed in a row, position reconciliation or an internal
/// accounting invariant failed, or residual maker-owned orders could not be
/// cancelled on the way out.
///
/// Supervisors must treat this as "stop, do NOT auto-restart, notify a
/// human" (systemd `RestartPreventExitStatus=`). It is deliberately
/// distinct from `0` (a clean Ctrl+C / SIGTERM stop: no restart, no alert),
/// from `1` (a generic startup/config/validation error), and from a panic
/// (`101`) or a fatal signal (e.g. SIGKILL -> `137`), so that an
/// *unexpected* death remains restartable while a designed fail-safe exit
/// does not trigger a restart loop.
pub const FAIL_SAFE_EXIT_CODE: i32 = 75;

/// Typed marker for an intentional maker fail-safe shutdown. Carrying the
/// reason as a concrete error (rather than a bare `anyhow::anyhow!`) lets
/// `main` downcast it and map it to [`FAIL_SAFE_EXIT_CODE`] while still
/// printing the message through the normal error path.
#[derive(Debug)]
pub struct FailSafeShutdown {
    pub message: String,
}

impl std::fmt::Display for FailSafeShutdown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for FailSafeShutdown {}

/// Why an armed account hard floor stopped the cycle (stage 5-b).
///
/// `Breach` is the floor doing its job; the other two are the floor refusing to
/// pretend it did. An armed floor evaluated against a stale or unreadable
/// balance is not evidence of solvency, so both fail closed instead of
/// resolving to "no breach".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AccountFloorCause {
    Breach,
    BalanceUnreadable,
    BalanceStale,
}

impl AccountFloorCause {
    pub(super) fn event(self) -> &'static str {
        match self {
            Self::Breach => "triggered",
            Self::BalanceUnreadable | Self::BalanceStale => "unevaluable",
        }
    }
}

/// An armed account hard floor stopped the cycle **before any order work**.
///
/// Raised from inside the cycle body (right after the authoritative balance is
/// resolved and the ledger is synchronized, before planning) so a breached or
/// unverifiable balance can never produce new exposure in the same cycle that
/// observed it. Cleanup on the way out is then the only order traffic left.
#[derive(Debug)]
pub(super) struct AccountFloorError {
    pub(super) cause: AccountFloorCause,
    /// `equity` / `margin` for a breach, otherwise the input that failed.
    pub(super) metric: &'static str,
    pub(super) observed: Option<f64>,
    pub(super) floor: Option<f64>,
    pub(super) detail: String,
}

impl AccountFloorError {
    pub(super) fn breach(metric: &'static str, observed: f64, floor: f64) -> Self {
        Self {
            cause: AccountFloorCause::Breach,
            metric,
            observed: Some(observed),
            floor: Some(floor),
            detail: format!("account {metric} {observed:.2} < floor {floor:.2}"),
        }
    }

    pub(super) fn balance_unreadable(equity: &str, cross_available: &str) -> Self {
        Self {
            cause: AccountFloorCause::BalanceUnreadable,
            metric: "balance_unreadable",
            observed: None,
            floor: None,
            detail: format!(
                "armed account floor cannot be evaluated: unparseable balance (equity='{equity}', cross_available='{cross_available}')"
            ),
        }
    }

    pub(super) fn balance_stale(age_secs: u64, max_age_secs: u64) -> Self {
        Self {
            cause: AccountFloorCause::BalanceStale,
            metric: "balance_stale",
            observed: None,
            floor: None,
            detail: format!(
                "armed account floor cannot be evaluated: balance is {age_secs}s old (max {max_age_secs}s)"
            ),
        }
    }
}

impl std::fmt::Display for AccountFloorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for AccountFloorError {}

/// What a shutdown leaves behind for a human (stage 5-b).
///
/// The maker never auto-flattens, so the exit path owes the operator an
/// unambiguous answer. The venue snapshot is taken **after** cleanup: an order
/// filling while it is being cancelled changes the position, and the account
/// stream is already gone by then, so the session ledger alone cannot be the
/// final word.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum ResidualHandoff {
    /// Venue and session ledger agree there is nothing to hand off.
    Flat,
    /// A position a human must close or hedge, confirmed by the venue.
    Confirmed { position: f64 },
    /// The venue could not confirm the position, or it contradicts the ledger.
    /// Treated as exposure: "cannot confirm flat" is not "flat".
    Unknown {
        ledger: f64,
        venue: Option<f64>,
        reason: &'static str,
    },
}

impl ResidualHandoff {
    /// Machine-readable event label for the JSON handoff.
    pub(super) fn event(&self) -> &'static str {
        match self {
            Self::Flat => "flat",
            Self::Confirmed { .. } => "handoff",
            Self::Unknown { .. } => "unknown",
        }
    }

    /// Whether a human has to act. `Unknown` counts: it may be exposure.
    pub(super) fn needs_operator(&self) -> bool {
        !matches!(self, Self::Flat)
    }
}

/// Decide the residual handoff from the post-cleanup venue snapshot and the
/// session ledger.
///
/// `venue` is `None` when the snapshot could not be fetched. In paper mode the
/// simulated position is passed as both arguments — the simulation *is* the
/// venue there.
pub(super) fn residual_handoff(
    venue: Option<f64>,
    ledger: f64,
    qty_tolerance: f64,
) -> ResidualHandoff {
    let tolerance = if qty_tolerance.is_finite() && qty_tolerance >= 0.0 {
        qty_tolerance
    } else {
        0.0
    };
    let Some(venue_position) = venue else {
        return ResidualHandoff::Unknown {
            ledger,
            venue: None,
            reason: "venue_snapshot_unavailable",
        };
    };
    if !venue_position.is_finite() {
        return ResidualHandoff::Unknown {
            ledger,
            venue: Some(venue_position),
            reason: "venue_position_unreadable",
        };
    }
    if !ledger.is_finite() {
        return ResidualHandoff::Unknown {
            ledger,
            venue: Some(venue_position),
            reason: "ledger_position_unknown",
        };
    }
    // A disagreement is reported before flatness: "the venue says flat but the
    // ledger says we hold 0.2" is exactly the case that must never render as
    // "nothing to do".
    if (venue_position - ledger).abs() > tolerance {
        return ResidualHandoff::Unknown {
            ledger,
            venue: Some(venue_position),
            reason: "venue_ledger_mismatch",
        };
    }
    if venue_position.abs() > tolerance {
        return ResidualHandoff::Confirmed {
            position: venue_position,
        };
    }
    ResidualHandoff::Flat
}

#[derive(Debug)]
pub(super) enum MakerExit {
    CtrlC,
    OrderResponse(String),
    ConsecutiveErrors(String),
    PositionReconciliation(String),
    MarketData(String),
    AccountingInvariant(String),
    StopLoss(String),
    /// Account-level hard floor (equity / available margin) breached — a
    /// solvency stop, kept separate from the strategy's `StopLoss`.
    AccountFloor(String),
}

impl MakerExit {
    pub(super) fn lifecycle_reason(&self) -> String {
        match self {
            Self::CtrlC => "Ctrl+C".to_string(),
            Self::OrderResponse(error) => {
                format!("fail-safe: order-response stream unavailable: {error}")
            }
            Self::ConsecutiveErrors(error) => {
                format!("fail-safe: 3 consecutive maker cycle errors: {error}")
            }
            Self::PositionReconciliation(error) => {
                format!("fail-safe: position reconciliation failed: {error}")
            }
            Self::MarketData(error) => {
                format!("fail-safe: market data recovery failed: {error}")
            }
            Self::AccountingInvariant(detail) => {
                format!("fail-safe: accounting invariant failed: {detail}")
            }
            Self::StopLoss(detail) => {
                format!("fail-safe: stop-loss breached: {detail}")
            }
            Self::AccountFloor(detail) => {
                format!("fail-safe: account floor breached: {detail}")
            }
        }
    }

    pub(super) fn terminal_error(&self) -> Option<String> {
        match self {
            Self::CtrlC => None,
            Self::OrderResponse(error) => Some(format!(
                "maker stopped immediately (fail-safe): order-response stream unavailable: {error}"
            )),
            Self::ConsecutiveErrors(error) => Some(format!(
                "maker stopped after 3 consecutive maker cycle errors (fail-safe): {error}"
            )),
            Self::PositionReconciliation(error) => Some(format!(
                "maker stopped immediately (fail-safe): position reconciliation failed: {error}"
            )),
            Self::MarketData(error) => Some(format!(
                "maker stopped immediately (fail-safe): market data recovery failed: {error}"
            )),
            Self::AccountingInvariant(detail) => Some(format!(
                "maker stopped immediately (fail-safe): accounting invariant failed: {detail}"
            )),
            Self::StopLoss(detail) => Some(format!(
                "maker stopped immediately (fail-safe): stop-loss breached: {detail}"
            )),
            Self::AccountFloor(detail) => Some(format!(
                "maker stopped immediately (fail-safe): account floor breached: {detail}"
            )),
        }
    }
}

impl From<standx_maker::RuntimeStopReason> for MakerExit {
    fn from(reason: standx_maker::RuntimeStopReason) -> Self {
        match reason {
            standx_maker::RuntimeStopReason::CtrlC => Self::CtrlC,
            standx_maker::RuntimeStopReason::OrderResponse(detail) => Self::OrderResponse(detail),
            standx_maker::RuntimeStopReason::PositionReconciliation(detail) => {
                Self::PositionReconciliation(detail)
            }
            standx_maker::RuntimeStopReason::MarketData(detail) => Self::MarketData(detail),
            standx_maker::RuntimeStopReason::CleanupFailure { target, reason } => match target {
                standx_maker::RecoveryTarget::OrderResponse => Self::OrderResponse(reason),
                standx_maker::RecoveryTarget::MarketData => Self::MarketData(reason),
                standx_maker::RecoveryTarget::AccountStream
                | standx_maker::RecoveryTarget::PositionReconciliation => {
                    Self::PositionReconciliation(reason)
                }
            },
            standx_maker::RuntimeStopReason::ConsecutiveCycleErrors(detail) => {
                Self::ConsecutiveErrors(detail)
            }
            standx_maker::RuntimeStopReason::StopLoss(detail) => Self::StopLoss(detail),
            standx_maker::RuntimeStopReason::AccountFloor(detail) => Self::AccountFloor(detail),
            standx_maker::RuntimeStopReason::AccountingInvariant(detail) => {
                Self::AccountingInvariant(detail)
            }
        }
    }
}

pub(super) fn is_maker_order(order: &Order) -> bool {
    standx_maker::is_maker_client_order_id(order.cl_ord_id.as_deref())
}

fn terminal_order_status(status: OrderStatus) -> bool {
    matches!(
        status,
        OrderStatus::Filled | OrderStatus::Canceled | OrderStatus::Rejected | OrderStatus::Expired
    )
}

pub(super) fn rest_order_observation(
    order: &Order,
) -> anyhow::Result<standx_maker::OrderObservation> {
    let order_id = order
        .id
        .parse::<u64>()
        .map_err(|_| anyhow::anyhow!("order has non-integer exchange ID '{}'", order.id))?;
    let price = order
        .price
        .parse::<f64>()
        .map_err(|_| anyhow::anyhow!("order {order_id} has invalid price '{}'", order.price))?;
    let open_qty = order
        .qty
        .parse::<f64>()
        .map_err(|_| anyhow::anyhow!("order {order_id} has invalid qty '{}'", order.qty))?;
    if !price.is_finite() || !open_qty.is_finite() || price <= 0.0 || open_qty < 0.0 {
        return Err(anyhow::anyhow!(
            "order {order_id} has invalid projection values price={price}, qty={open_qty}"
        ));
    }
    Ok(standx_maker::OrderObservation {
        order_id,
        client_order_id: order.cl_ord_id.clone(),
        side: order.side,
        price,
        open_qty,
        terminal: terminal_order_status(order.status),
    })
}

pub(super) fn stream_order_observation(
    order: &OrderUpdate,
) -> anyhow::Result<standx_maker::OrderObservation> {
    let terminal = terminal_order_status(order.status);
    let raw_price = if order.price.is_empty() || order.price == "0" {
        &order.fill_avg_price
    } else {
        &order.price
    };
    let price = if raw_price.is_empty() {
        0.0
    } else {
        raw_price.parse::<f64>().map_err(|_| {
            anyhow::anyhow!(
                "account order {} has invalid price '{}'",
                order.order_id,
                raw_price
            )
        })?
    };
    let qty = order.qty.parse::<f64>().map_err(|_| {
        anyhow::anyhow!(
            "account order {} has invalid qty '{}'",
            order.order_id,
            order.qty
        )
    })?;
    let fill_qty = order.fill_qty.parse::<f64>().map_err(|_| {
        anyhow::anyhow!(
            "account order {} has invalid fill qty '{}'",
            order.order_id,
            order.fill_qty
        )
    })?;
    let open_qty = (qty - fill_qty).max(0.0);
    if !price.is_finite()
        || !qty.is_finite()
        || !fill_qty.is_finite()
        || (!terminal && price <= 0.0)
        || qty < 0.0
        || fill_qty < 0.0
    {
        return Err(anyhow::anyhow!(
            "account order {} has invalid projection values",
            order.order_id
        ));
    }
    Ok(standx_maker::OrderObservation {
        order_id: order.order_id,
        client_order_id: order.cl_ord_id.clone(),
        side: order.side,
        price,
        open_qty,
        terminal,
    })
}

pub(super) fn is_current_run_order(order: &Order, run_order_prefix: &str) -> bool {
    standx_maker::is_current_run_client_order_id(order.cl_ord_id.as_deref(), run_order_prefix)
}

pub(super) fn position_for_symbol(positions: &[Position], symbol: &str) -> anyhow::Result<f64> {
    positions
        .iter()
        .filter(|position| position.symbol.eq_ignore_ascii_case(symbol))
        .try_fold(0.0, |total, position| {
            let signed_qty =
                signed_position_quantity(&position.qty, position.side).map_err(|error| {
                    anyhow::anyhow!("position on {symbol} has invalid qty: {error}")
                })?;
            Ok(total + signed_qty)
        })
}

pub(super) fn signed_position_quantity(
    raw_qty: &str,
    side: Option<OrderSide>,
) -> anyhow::Result<f64> {
    let qty = raw_qty
        .parse::<f64>()
        .map_err(|_| anyhow::anyhow!("'{raw_qty}' is not numeric"))?;
    if !qty.is_finite() {
        return Err(anyhow::anyhow!("'{raw_qty}' is not finite"));
    }
    Ok(match side {
        Some(OrderSide::Sell) => -qty.abs(),
        Some(OrderSide::Buy) => qty.abs(),
        None => qty,
    })
}

#[cfg(test)]
mod exit_mapping_tests {
    use super::MakerExit;
    use standx_maker::{RecoveryTarget, RuntimeStopReason};

    /// Pins the full RuntimeStopReason → MakerExit mapping so a new stop
    /// reason (or a re-targeted CleanupFailure) cannot silently land in the
    /// wrong fail-safe exit bucket.
    #[test]
    fn every_runtime_stop_reason_maps_to_the_expected_exit() {
        assert!(matches!(
            MakerExit::from(RuntimeStopReason::CtrlC),
            MakerExit::CtrlC
        ));
        assert!(matches!(
            MakerExit::from(RuntimeStopReason::OrderResponse("boom".to_string())),
            MakerExit::OrderResponse(detail) if detail == "boom"
        ));
        assert!(matches!(
            MakerExit::from(RuntimeStopReason::PositionReconciliation("boom".to_string())),
            MakerExit::PositionReconciliation(detail) if detail == "boom"
        ));
        assert!(matches!(
            MakerExit::from(RuntimeStopReason::MarketData("boom".to_string())),
            MakerExit::MarketData(detail) if detail == "boom"
        ));
        assert!(matches!(
            MakerExit::from(RuntimeStopReason::ConsecutiveCycleErrors("boom".to_string())),
            MakerExit::ConsecutiveErrors(detail) if detail == "boom"
        ));
        assert!(matches!(
            MakerExit::from(RuntimeStopReason::StopLoss("boom".to_string())),
            MakerExit::StopLoss(detail) if detail == "boom"
        ));
        assert!(matches!(
            MakerExit::from(RuntimeStopReason::AccountFloor("boom".to_string())),
            MakerExit::AccountFloor(detail) if detail == "boom"
        ));
        assert!(matches!(
            MakerExit::from(RuntimeStopReason::AccountingInvariant("boom".to_string())),
            MakerExit::AccountingInvariant(detail) if detail == "boom"
        ));
        assert!(matches!(
            MakerExit::from(RuntimeStopReason::CleanupFailure {
                target: RecoveryTarget::OrderResponse,
                reason: "boom".to_string(),
            }),
            MakerExit::OrderResponse(detail) if detail == "boom"
        ));
        for target in [
            RecoveryTarget::AccountStream,
            RecoveryTarget::PositionReconciliation,
        ] {
            assert!(matches!(
                MakerExit::from(RuntimeStopReason::CleanupFailure {
                    target,
                    reason: "boom".to_string(),
                }),
                MakerExit::PositionReconciliation(detail) if detail == "boom"
            ));
        }
        assert!(matches!(
            MakerExit::from(RuntimeStopReason::CleanupFailure {
                target: RecoveryTarget::MarketData,
                reason: "boom".to_string(),
            }),
            MakerExit::MarketData(detail) if detail == "boom"
        ));
    }

    /// Every fail-safe exit must surface a terminal error (only a clean
    /// Ctrl+C stop is silent) so supervisors always see a reason on exit 75.
    #[test]
    fn only_ctrl_c_exits_without_a_terminal_error() {
        assert!(MakerExit::CtrlC.terminal_error().is_none());
        for exit in [
            MakerExit::OrderResponse("boom".to_string()),
            MakerExit::ConsecutiveErrors("boom".to_string()),
            MakerExit::PositionReconciliation("boom".to_string()),
            MakerExit::MarketData("boom".to_string()),
            MakerExit::AccountingInvariant("boom".to_string()),
            MakerExit::StopLoss("boom".to_string()),
            MakerExit::AccountFloor("boom".to_string()),
        ] {
            let error = exit
                .terminal_error()
                .expect("fail-safe exits carry an error");
            assert!(error.contains("boom"));
            assert!(exit.lifecycle_reason().contains("boom"));
        }
    }
}

#[cfg(test)]
mod residual_handoff_tests {
    use super::*;

    const TOL: f64 = 0.0005;

    /// Stage 5-b: the shutdown handoff must never render "cannot confirm" as
    /// "nothing to do". The venue snapshot is taken after cleanup, so a fill
    /// that landed while orders were being cancelled shows up here.
    #[test]
    fn venue_confirmed_flat_and_position() {
        assert_eq!(residual_handoff(Some(0.0), 0.0, TOL), ResidualHandoff::Flat);
        assert_eq!(
            residual_handoff(Some(0.0005), 0.0005, TOL),
            ResidualHandoff::Flat
        );
        assert_eq!(
            residual_handoff(Some(-0.1), -0.1, TOL),
            ResidualHandoff::Confirmed { position: -0.1 }
        );
        assert!(!ResidualHandoff::Flat.needs_operator());
        assert!(ResidualHandoff::Confirmed { position: -0.1 }.needs_operator());
    }

    #[test]
    fn missing_or_unreadable_venue_snapshot_is_unknown_not_flat() {
        assert_eq!(
            residual_handoff(None, 0.0, TOL),
            ResidualHandoff::Unknown {
                ledger: 0.0,
                venue: None,
                reason: "venue_snapshot_unavailable",
            }
        );
        // Even a ledger that believes it is flat cannot upgrade this to Flat.
        assert!(residual_handoff(None, 0.0, TOL).needs_operator());
        // NaN never equals itself, so these two assert on the variant instead.
        assert!(matches!(
            residual_handoff(Some(f64::NAN), 0.0, TOL),
            ResidualHandoff::Unknown {
                venue: Some(venue),
                reason: "venue_position_unreadable",
                ..
            } if venue.is_nan()
        ));
        assert!(matches!(
            residual_handoff(Some(0.0), f64::NAN, TOL),
            ResidualHandoff::Unknown {
                ledger,
                venue: Some(0.0),
                reason: "ledger_position_unknown",
            } if ledger.is_nan()
        ));
    }

    /// The case the pre-cleanup snapshot used to get wrong: the venue and the
    /// session ledger disagree, in either direction.
    #[test]
    fn venue_ledger_disagreement_is_unknown_in_both_directions() {
        assert_eq!(
            residual_handoff(Some(0.0), 0.2, TOL),
            ResidualHandoff::Unknown {
                ledger: 0.2,
                venue: Some(0.0),
                reason: "venue_ledger_mismatch",
            }
        );
        assert_eq!(
            residual_handoff(Some(0.2), 0.0, TOL),
            ResidualHandoff::Unknown {
                ledger: 0.0,
                venue: Some(0.2),
                reason: "venue_ledger_mismatch",
            }
        );
        // Agreement within tolerance is not a mismatch.
        assert_eq!(
            residual_handoff(Some(0.10004), 0.1, TOL),
            ResidualHandoff::Confirmed { position: 0.10004 }
        );
    }

    #[test]
    fn nonsense_tolerance_degrades_to_exact_comparison() {
        assert_eq!(
            residual_handoff(Some(0.0), 0.0, f64::NAN),
            ResidualHandoff::Flat
        );
        assert_eq!(
            residual_handoff(Some(0.001), 0.001, -1.0),
            ResidualHandoff::Confirmed { position: 0.001 }
        );
    }
}
