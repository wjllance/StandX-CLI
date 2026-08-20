//! Execution-cost state machine for the `InventoryTrim` exit (stage 8,
//! `docs/33-maker-exit-execution-cost-design.md`).
//!
//! Pure decision logic only: no I/O, no clock, no `Instant`. The caller
//! (`standx-cli`) owns the `cl_ord_id` lifecycle, order submission, and
//! projection/latency bookkeeping; this module only decides what to do this
//! cycle given typed inputs.
//!
//! Deliberately **not** used by [`crate::ExitKind::WindDown`]: an A/B arm past
//! its window must converge to flat deterministically, and an `Alo` order may
//! never fill. `WindDown` keeps the original immediate reduce-only Market
//! path unconditionally, regardless of `alo_enabled` (docs/33 known-cost 5).

use crate::bps_diff;
use standx_sdk::models::OrderSide;

/// Execution-cost configuration for the `InventoryTrim` exit. Disabled
/// (`alo_enabled = false`) by default: the exit keeps using the legacy
/// reduce-only Market order unconditionally, with zero behavior change from
/// this module. All fields are validated by the caller at startup (invalid
/// values must be rejected before any order can be in flight, never panic
/// while one is resting).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InventoryExitConfig {
    /// When `false` (default), `InventoryTrim` exits never enter this state
    /// machine at all.
    pub alo_enabled: bool,
    /// Tick offset from the touch when placing the `Alo` order: for a sell
    /// (reducing a long) the price is `best_ask + offset*tick`; for a buy
    /// (reducing a short) it is `best_bid - offset*tick`. `0` joins the touch
    /// exactly.
    pub alo_price_offset_ticks: i32,
    /// Re-price the resting `Alo` order once the touch drifts more than this
    /// many bps from the price it is currently resting at.
    pub alo_refresh_bps: f64,
    /// Upgrade to `Ioc` once the order has spent this many cycles resting in
    /// the `Alo` phase.
    pub alo_max_cycles: u32,
    /// Upgrade to `Ioc` immediately once the signed, direction-aware loss
    /// versus the session break-even reaches this many bps.
    pub ioc_loss_bps: f64,
    /// Ticks the `Ioc` order crosses the touch by, to guarantee it can match:
    /// for a sell, `best_bid - cross*tick`; for a buy, `best_ask + cross*tick`.
    pub ioc_cross_ticks: u32,
}

impl Default for InventoryExitConfig {
    fn default() -> Self {
        Self {
            alo_enabled: false,
            alo_price_offset_ticks: 0,
            alo_refresh_bps: 2.0,
            alo_max_cycles: 20,
            ioc_loss_bps: 5.0,
            ioc_cross_ticks: 2,
        }
    }
}

/// Which leg of the Alo→Ioc machine an in-flight `InventoryTrim` exit is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitPhase {
    /// A `TimeInForce::Alo` order rests at (or near) the touch.
    Alo,
    /// A `TimeInForce::Ioc` order crosses the touch; it never rests.
    Ioc,
}

impl ExitPhase {
    /// Snake-case label for machine-readable output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Alo => "alo",
            Self::Ioc => "ioc",
        }
    }
}

/// Caller-persisted state of an in-flight exit order, re-evaluated once per
/// cycle by [`plan_exit_order_step`].
///
/// This is a soft cache, not a safety-critical source of truth: the venue's
/// account/order streams are authoritative for whether an exit order is
/// actually resting. A caller that loses this state (e.g. after a
/// recovery/reconnect resets its local bookkeeping) must re-derive it from
/// the venue-observed order before calling this function again — see docs/33
/// §1 ("必须解决的结构性问题"). Passing `None` when an order is genuinely
/// still resting will place a duplicate; that reconciliation is the caller's
/// responsibility, not this function's.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExitPhaseState {
    pub phase: ExitPhase,
    /// Cycles already spent in `phase`, not counting the one about to run.
    pub cycles_in_phase: u32,
    /// Price of the order currently resting/last submitted. Meaningful for
    /// drift comparison only in the `Alo` phase.
    pub resting_price: f64,
}

/// What to do this cycle for the in-flight exit order.
///
/// Every variant other than [`Self::HoldAlo`] means "submit a brand-new order
/// with a freshly minted `cl_ord_id`" — the stable-identity rule (docs/33 §2)
/// is that an id is pinned for as long as [`Self::HoldAlo`] keeps being
/// returned, and only advances when one of the other variants fires.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ExitOrderStep {
    /// No order tracked yet: place a fresh `Alo` order at `price`.
    OpenAlo { price: f64 },
    /// Keep the resting `Alo` order exactly as-is; no order traffic.
    HoldAlo,
    /// Cancel the resting `Alo` order and re-place at `price` (still `Alo`):
    /// the touch drifted more than `alo_refresh_bps` from the resting price.
    RepriceAlo { price: f64 },
    /// Cancel the resting `Alo` order (if any) and submit a crossing `Ioc`
    /// order at `price` instead: the loss or timeout threshold was breached.
    UpgradeToIoc { price: f64 },
    /// Submit a fresh `Ioc` order at `price` for the residual quantity. Used
    /// both to enter the `Ioc` phase with no prior resting order and to retry
    /// a previous `Ioc` attempt that left a residual (`Ioc` never rests, so
    /// there is never a previous `Ioc` order left to cancel).
    SubmitIoc { price: f64 },
}

impl ExitOrderStep {
    /// The price this step wants a new order placed at, if it places one.
    pub fn price(self) -> Option<f64> {
        match self {
            Self::OpenAlo { price }
            | Self::RepriceAlo { price }
            | Self::UpgradeToIoc { price }
            | Self::SubmitIoc { price } => Some(price),
            Self::HoldAlo => None,
        }
    }

    /// Whether a currently-resting order must be cancelled before (or as
    /// part of) this step. `OpenAlo`/`SubmitIoc` never cancel anything: the
    /// former runs only when nothing is tracked yet, and `Ioc` never rests so
    /// a residual retry never has a resting order to cancel either.
    pub fn cancels_resting(self) -> bool {
        matches!(self, Self::RepriceAlo { .. } | Self::UpgradeToIoc { .. })
    }
}

fn alo_price(side: OrderSide, best_bid: f64, best_ask: f64, offset_ticks: i32, tick: f64) -> f64 {
    match side {
        OrderSide::Sell => best_ask + f64::from(offset_ticks) * tick,
        OrderSide::Buy => best_bid - f64::from(offset_ticks) * tick,
    }
}

fn ioc_price(side: OrderSide, best_bid: f64, best_ask: f64, cross_ticks: u32, tick: f64) -> f64 {
    match side {
        OrderSide::Sell => best_bid - f64::from(cross_ticks) * tick,
        OrderSide::Buy => best_ask + f64::from(cross_ticks) * tick,
    }
}

/// Decide this cycle's action for an in-flight `InventoryTrim` exit.
///
/// `side` is the exit order's side (opposite the position being reduced; see
/// [`crate::InventoryExit::side`]). `best_bid`/`best_ask` must be a full,
/// healthy two-sided touch — the caller already guarantees this via
/// [`crate::preflight_cycle`] before any order is planned, the same
/// precondition the rest of live order planning relies on. `loss_bps` is the
/// signed, direction-aware loss versus the session break-even (see
/// [`crate::MakerStats`]); `None` (no baseline yet) is treated as "not
/// losing" so a missing signal can never force an unwanted taker escalation.
#[allow(clippy::too_many_arguments)]
pub fn plan_exit_order_step(
    cfg: &InventoryExitConfig,
    state: Option<ExitPhaseState>,
    side: OrderSide,
    best_bid: f64,
    best_ask: f64,
    price_tick: f64,
    loss_bps: Option<f64>,
) -> (ExitOrderStep, ExitPhaseState) {
    let alo_target = alo_price(
        side,
        best_bid,
        best_ask,
        cfg.alo_price_offset_ticks,
        price_tick,
    );
    let ioc_target = ioc_price(side, best_bid, best_ask, cfg.ioc_cross_ticks, price_tick);
    let losing_beyond_threshold =
        loss_bps.is_some_and(|bps| bps.is_finite() && bps >= cfg.ioc_loss_bps);

    match state {
        None => {
            let next = ExitPhaseState {
                phase: ExitPhase::Alo,
                cycles_in_phase: 0,
                resting_price: alo_target,
            };
            (ExitOrderStep::OpenAlo { price: alo_target }, next)
        }
        Some(state) if state.phase == ExitPhase::Alo => {
            if losing_beyond_threshold || state.cycles_in_phase >= cfg.alo_max_cycles {
                let next = ExitPhaseState {
                    phase: ExitPhase::Ioc,
                    cycles_in_phase: 0,
                    resting_price: ioc_target,
                };
                (ExitOrderStep::UpgradeToIoc { price: ioc_target }, next)
            } else if bps_diff(alo_target, state.resting_price) > cfg.alo_refresh_bps {
                let next = ExitPhaseState {
                    phase: ExitPhase::Alo,
                    cycles_in_phase: state.cycles_in_phase + 1,
                    resting_price: alo_target,
                };
                (ExitOrderStep::RepriceAlo { price: alo_target }, next)
            } else {
                let next = ExitPhaseState {
                    cycles_in_phase: state.cycles_in_phase + 1,
                    ..state
                };
                (ExitOrderStep::HoldAlo, next)
            }
        }
        Some(state) => {
            // Already in Ioc: it never rests, so every cycle with a residual
            // simply retries fresh at the current cross price. There is no
            // timeout here — Ioc resolves or fails within one request/response
            // round trip, never spans cycles the way a resting Alo does.
            let next = ExitPhaseState {
                phase: ExitPhase::Ioc,
                cycles_in_phase: state.cycles_in_phase + 1,
                resting_price: ioc_target,
            };
            (ExitOrderStep::SubmitIoc { price: ioc_target }, next)
        }
    }
}

/// Validate the scalar constraints on [`InventoryExitConfig`]. Rejected even
/// while `alo_enabled` is `false`, so a bad file never rides along silently
/// until the day someone flips the switch — the same discipline as
/// `validate_external_skew`/`validate_microprice` in the CLI. Never panics:
/// `df069c5` was a hard lesson that an illegal-operator-value panic while
/// orders are resting is a live-order-path outage, not a config nit.
pub fn validate_inventory_exit_config(cfg: &InventoryExitConfig) -> Result<(), &'static str> {
    if !cfg.alo_refresh_bps.is_finite() || cfg.alo_refresh_bps <= 0.0 {
        return Err("inventory_exit alo_refresh_bps must be finite and > 0");
    }
    if cfg.alo_max_cycles == 0 {
        return Err("inventory_exit alo_max_cycles must be >= 1");
    }
    if !cfg.ioc_loss_bps.is_finite() || cfg.ioc_loss_bps <= 0.0 {
        return Err("inventory_exit ioc_loss_bps must be finite and > 0");
    }
    if cfg.ioc_cross_ticks == 0 {
        return Err("inventory_exit ioc_cross_ticks must be >= 1");
    }
    if cfg.alo_price_offset_ticks.unsigned_abs() > 10_000 {
        return Err("inventory_exit alo_price_offset_ticks is out of a sane range");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TICK: f64 = 0.01;

    #[test]
    fn opens_alo_at_the_touch_when_nothing_is_tracked_yet() {
        let cfg = InventoryExitConfig::default();
        let (step, state) =
            plan_exit_order_step(&cfg, None, OrderSide::Sell, 99.0, 100.0, TICK, None);
        assert_eq!(step, ExitOrderStep::OpenAlo { price: 100.0 });
        assert_eq!(state.phase, ExitPhase::Alo);
        assert_eq!(state.cycles_in_phase, 0);
        assert_eq!(state.resting_price, 100.0);

        let (step, _) = plan_exit_order_step(&cfg, None, OrderSide::Buy, 99.0, 100.0, TICK, None);
        assert_eq!(step, ExitOrderStep::OpenAlo { price: 99.0 });
    }

    #[test]
    fn applies_the_configured_tick_offset_away_from_the_touch() {
        let cfg = InventoryExitConfig {
            alo_price_offset_ticks: 3,
            ..InventoryExitConfig::default()
        };
        let (step, _) = plan_exit_order_step(&cfg, None, OrderSide::Sell, 99.0, 100.0, TICK, None);
        assert_eq!(step, ExitOrderStep::OpenAlo { price: 100.03 });
        let (step, _) = plan_exit_order_step(&cfg, None, OrderSide::Buy, 99.0, 100.0, TICK, None);
        assert_eq!(step, ExitOrderStep::OpenAlo { price: 98.97 });
    }

    #[test]
    fn holds_within_refresh_tolerance_and_advances_the_cycle_counter() {
        let cfg = InventoryExitConfig::default();
        let state = ExitPhaseState {
            phase: ExitPhase::Alo,
            cycles_in_phase: 4,
            resting_price: 100.0,
        };
        // 100.0 -> 100.001 is well under the default 2bps refresh threshold.
        let (step, next) = plan_exit_order_step(
            &cfg,
            Some(state),
            OrderSide::Sell,
            99.0,
            100.001,
            TICK,
            None,
        );
        assert_eq!(step, ExitOrderStep::HoldAlo);
        assert_eq!(next.cycles_in_phase, 5);
        assert_eq!(next.resting_price, 100.0, "price is unchanged on Hold");
    }

    #[test]
    fn reprices_once_the_touch_drifts_beyond_refresh_bps() {
        let cfg = InventoryExitConfig::default();
        let state = ExitPhaseState {
            phase: ExitPhase::Alo,
            cycles_in_phase: 1,
            resting_price: 100.0,
        };
        // 100.0 -> 100.03 is 3bps, beyond the default 2bps threshold.
        let (step, next) =
            plan_exit_order_step(&cfg, Some(state), OrderSide::Sell, 99.0, 100.03, TICK, None);
        assert_eq!(step, ExitOrderStep::RepriceAlo { price: 100.03 });
        assert!(step.cancels_resting());
        assert_eq!(next.phase, ExitPhase::Alo);
        assert_eq!(next.cycles_in_phase, 2);
        assert_eq!(next.resting_price, 100.03);
    }

    #[test]
    fn upgrades_to_ioc_once_loss_crosses_the_threshold() {
        let cfg = InventoryExitConfig::default();
        let state = ExitPhaseState {
            phase: ExitPhase::Alo,
            cycles_in_phase: 1,
            resting_price: 100.0,
        };
        let (step, next) = plan_exit_order_step(
            &cfg,
            Some(state),
            OrderSide::Sell,
            99.0,
            100.0,
            TICK,
            Some(5.0),
        );
        assert_eq!(
            step,
            ExitOrderStep::UpgradeToIoc {
                price: 99.0 - 2.0 * TICK
            }
        );
        assert!(step.cancels_resting());
        assert_eq!(next.phase, ExitPhase::Ioc);
        assert_eq!(next.cycles_in_phase, 0);

        // Just under the threshold: stays in Alo.
        let (step, _) = plan_exit_order_step(
            &cfg,
            Some(state),
            OrderSide::Sell,
            99.0,
            100.0,
            TICK,
            Some(4.999),
        );
        assert_ne!(step, ExitOrderStep::UpgradeToIoc { price: 98.98 });
    }

    #[test]
    fn upgrades_to_ioc_once_alo_max_cycles_is_reached() {
        let cfg = InventoryExitConfig {
            alo_max_cycles: 3,
            ..InventoryExitConfig::default()
        };
        let almost_timed_out = ExitPhaseState {
            phase: ExitPhase::Alo,
            cycles_in_phase: 2,
            resting_price: 100.0,
        };
        let (step, _) = plan_exit_order_step(
            &cfg,
            Some(almost_timed_out),
            OrderSide::Sell,
            99.0,
            100.0,
            TICK,
            None,
        );
        assert!(
            matches!(step, ExitOrderStep::HoldAlo),
            "one cycle before the timeout must still hold"
        );

        let timed_out = ExitPhaseState {
            cycles_in_phase: 3,
            ..almost_timed_out
        };
        let (step, next) = plan_exit_order_step(
            &cfg,
            Some(timed_out),
            OrderSide::Sell,
            99.0,
            100.0,
            TICK,
            None,
        );
        assert!(matches!(step, ExitOrderStep::UpgradeToIoc { .. }));
        assert_eq!(next.phase, ExitPhase::Ioc);
    }

    #[test]
    fn ioc_never_rests_and_always_retries_fresh_on_a_residual() {
        let cfg = InventoryExitConfig::default();
        let state = ExitPhaseState {
            phase: ExitPhase::Ioc,
            cycles_in_phase: 0,
            resting_price: 98.98,
        };
        let (step, next) =
            plan_exit_order_step(&cfg, Some(state), OrderSide::Sell, 99.5, 100.5, TICK, None);
        assert_eq!(
            step,
            ExitOrderStep::SubmitIoc {
                price: 99.5 - 2.0 * TICK
            }
        );
        assert!(!step.cancels_resting());
        assert_eq!(next.phase, ExitPhase::Ioc);
        assert_eq!(next.cycles_in_phase, 1);
    }

    #[test]
    fn ioc_prices_cross_the_touch_for_a_guaranteed_match() {
        let cfg = InventoryExitConfig::default();
        // Sell exit crosses below best_bid; buy exit crosses above best_ask.
        let (step, _) = plan_exit_order_step(
            &cfg,
            Some(ExitPhaseState {
                phase: ExitPhase::Ioc,
                cycles_in_phase: 0,
                resting_price: 0.0,
            }),
            OrderSide::Buy,
            99.0,
            100.0,
            TICK,
            None,
        );
        assert_eq!(step, ExitOrderStep::SubmitIoc { price: 100.02 });
    }

    #[test]
    fn validation_rejects_illegal_values_without_panicking() {
        let bad = [
            InventoryExitConfig {
                alo_refresh_bps: 0.0,
                ..InventoryExitConfig::default()
            },
            InventoryExitConfig {
                alo_refresh_bps: f64::NAN,
                ..InventoryExitConfig::default()
            },
            InventoryExitConfig {
                alo_max_cycles: 0,
                ..InventoryExitConfig::default()
            },
            InventoryExitConfig {
                ioc_loss_bps: -1.0,
                ..InventoryExitConfig::default()
            },
            InventoryExitConfig {
                ioc_cross_ticks: 0,
                ..InventoryExitConfig::default()
            },
        ];
        for cfg in bad {
            assert!(validate_inventory_exit_config(&cfg).is_err());
        }
        assert!(validate_inventory_exit_config(&InventoryExitConfig::default()).is_ok());
    }
}
