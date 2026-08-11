//! Pure quoting & reconcile logic for market making (SIP-5A maker yield).
//!
//! No I/O in this module: every function takes plain values and returns
//! decisions, so the whole strategy is unit-testable without a network.
//!
//! The core idea is an **anti-flicker** loop: SIP-5A rewards uptime (orders
//! resting on the book inside an eligibility band around mark price) and
//! penalizes flicker-cancels. So resting quotes are HELD as long as they stay
//! inside the band; re-quoting happens only when mark price drifts more than
//! `refresh_bps` from the mark recorded when the order was placed.
//!
//! Numeric representation: prices/quantities are `f64` internally and
//! formatted to the symbol's tick decimals only at the API edge
//! ([`format_decimals`]). This matches the rest of the codebase (which does
//! ad-hoc f64 math on the API's string values); if symbols with more than ~8
//! price decimals ever list, revisit with a decimal type.

use standx_sdk::models::OrderSide;

mod alerts;
mod stats;

pub mod account_projection;
pub mod external_guard;
pub mod external_skew;
pub mod inventory;
pub mod latency;
pub mod ledger;
pub mod market_data;
pub mod ownership;
pub mod performance;
pub mod recovery;
pub mod replay;
pub mod risk;
pub mod runtime;
pub mod volatility;

pub use account_projection::{
    AccountProjectionEvent, MakerAccountProjection, OrderObservation, OrderResponseContinuity,
    ProjectedOrder, ProjectionOutcome, ProjectionPendingCancel, ProjectionPendingPlace,
    ProjectionPendingRequest, ProjectionRegistryError, ProjectionRequestResolution,
    RequestLifecycle, RequestOperation, ResponseCorrelation, MAX_PENDING_ORDER_REQUESTS,
};
pub use alerts::{account_floor_breach, AccountFloorBreach, Alert, AlertMonitor};
pub use external_guard::{
    ExternalDivergence, GuardConfig, GuardController, GuardDecision, GuardError,
};
pub use external_skew::{external_skew_shift_bps, ExternalSkewConfig};
pub use inventory::{
    NonlinearSkewConfig, SizeSkewConfig, SizeSkewController, SizeSkewDecision, SizeSkewError,
};
pub use latency::{
    LatencyError, LatencyMetricSummary, LatencyRequest, LatencyRequestContext, LatencyRequestKind,
    LatencyRequestOutcome, LatencySummary, OrderLatencyTracker,
};
pub use ledger::{LedgerError, LedgerTrade, MakerFill, MakerLedger, TradeSource};
pub use market_data::{
    MarketDataFaultClass, MarketDataHealth, MarketDataMode, MarketDataObservation,
    MarketDataTransition, MARKET_DATA_BAD_GRACE_MS, MARKET_DATA_BAD_OBSERVATIONS_TO_DEGRADE,
    MARKET_DATA_COHERENT_SNAPSHOTS_TO_RECOVER,
};
pub use ownership::{
    exit_client_order_id, is_current_run_client_order_id, is_maker_client_order_id,
    open_qty_adopts, pending_covers_slot, position_within_limit, quote_client_order_id, QuoteSlot,
    MAKER_CL_ORD_ID_PREFIX,
};
pub use performance::{
    ExecutionCosts, FillRole, InventoryTimeSummary, MarkoutSummary, PerformanceError,
    PerformanceFill, PerformanceLedger, PerformanceSummary, QuoteQualityInterval, QuoteTimeSummary,
    MARKOUT_WINDOWS_MS,
};
pub use recovery::{recovery_retry_delay_secs, MAX_RECOVERY_RETRY_BACKOFF_SECS};
pub use replay::{
    run_replay, ReplayCycle, ReplayCycleOutcome, ReplayError, ReplayEvent, ReplayResult,
    ReplaySettings,
};
pub use risk::{PositionAlertAnchor, PositionRiskEvent, PositionRiskKind};
pub use runtime::{
    order_cancel_rejection_reason, MakerEffect, MakerEvent, MakerState, RecoveryTarget,
    RequestTimeoutPhase, RuntimeStopReason, WorkToken, MAX_CONSECUTIVE_CYCLE_ERRORS,
};
pub use stats::MakerStats;
pub use volatility::{
    AdaptiveSpreadConfig, AdaptiveSpreadError, SpreadController, SpreadDecision, SpreadTier,
    VolBreaker, VolatilityError, VolatilityWindow,
};

/// Static per-run configuration (CLI args + symbol metadata).
#[derive(Debug, Clone)]
pub struct MakerConfig {
    /// Half-spread from mark price, in basis points, for level 0.
    pub spread_bps: f64,
    /// Eligibility band: never quote outside `mark * (1 ± band_bps/1e4)`.
    pub band_bps: f64,
    /// Spacing between quote levels, in basis points.
    pub level_step_bps: f64,
    /// Anti-flicker threshold: re-quote only when mark has drifted more than
    /// this (bps) from the mark recorded at placement time.
    pub refresh_bps: f64,
    /// Number of quote levels per side.
    pub levels: u32,
    /// Per-side, per-level order quantity.
    pub size: f64,
    /// Max absolute position; the side that would grow it further is
    /// suppressed once exceeded.
    pub max_position: f64,
    /// Inventory skew: at full inventory (`|position| == max_position`), the
    /// quote center is shifted this many bps away from mark to favor the
    /// reducing side. 0 disables skew (quotes stay centered on mark).
    pub skew_bps: f64,
    /// Price precision (decimal places) from `SymbolInfo.price_tick_decimals`.
    pub price_decimals: u32,
    /// Quantity precision (decimal places) from `SymbolInfo.qty_tick_decimals`.
    pub qty_decimals: u32,
    /// Minimum order quantity from `SymbolInfo.min_order_qty`.
    pub min_order_qty: f64,
}

impl MakerConfig {
    /// One price tick: `10^-price_decimals`.
    pub fn price_tick(&self) -> f64 {
        10f64.powi(-(self.price_decimals as i32))
    }

    /// One quantity tick: `10^-qty_decimals`.
    pub fn qty_tick(&self) -> f64 {
        10f64.powi(-(self.qty_decimals as i32))
    }
}

/// A quote we want resting on the book (prices/qtys already tick-rounded).
#[derive(Debug, Clone, PartialEq)]
pub struct DesiredQuote {
    pub side: OrderSide,
    pub level: u32,
    pub price: f64,
    pub qty: f64,
}

/// Which policy asked for an inventory-reducing order.
///
/// Stage 5-b separates the two normal exit policies so evidence never has to
/// infer an exit's origin from context. Emergency risk exit is deliberately
/// *not* a variant: no such policy exists (a stop-loss or account floor stops
/// and hands the residual position off, it never trades). The current safety
/// policy deliberately rejects inventing such a path without evidence; see the
/// [maker roadmap](../../../docs/18-maker-strategy-roadmap.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitKind {
    /// Threshold-driven inventory trim: normal operation, opt-in through
    /// `inventory_exit_pct` / `inventory_exit_qty`, capped to one chunk.
    InventoryTrim,
    /// Supervisor-requested wind-down (e.g. an A/B arm past its window): take
    /// the whole residual at once and never quote again.
    WindDown,
}

impl ExitKind {
    /// Snake-case label for machine-readable output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InventoryTrim => "inventory_trim",
            Self::WindDown => "wind_down",
        }
    }
}

/// Why a requested exit is not being submitted this cycle.
///
/// Both reasons are policy, not failure: the exit stays requested (so the
/// caller keeps tracking it) but no order is planned. Making the reason typed
/// is what turns "a halt silently ate an exit" into something countable in the
/// evidence pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitSuppression {
    /// A volatility halt is in effect. Emergency execution during a halt needs
    /// a separate, explicitly authorized policy that deliberately does not
    /// exist (docs/26 decision D1).
    VolatilityHalt,
    /// Market data is not Active, so no price is trustworthy enough to trade.
    MarketDataInactive,
}

impl ExitSuppression {
    /// Snake-case label for machine-readable output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::VolatilityHalt => "volatility_halt",
            Self::MarketDataInactive => "market_data_inactive",
        }
    }
}

/// An exit the configured policy asked for but this cycle did not submit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SuppressedExit {
    /// Which policy asked for it.
    pub kind: ExitKind,
    /// Why it was not submitted.
    pub reason: ExitSuppression,
}

/// A deliberate inventory-reducing order. Execution is kept outside this pure
/// strategy module so callers can first cancel conflicting maker quotes and
/// enforce venue-specific reduce-only semantics.
#[derive(Debug, Clone, PartialEq)]
pub struct InventoryExit {
    /// Opposite the current position: sell a long, buy a short.
    pub side: OrderSide,
    /// Never exceeds the current absolute position or the configured chunk.
    pub qty: f64,
    /// Which policy asked for this exit.
    pub kind: ExitKind,
}

/// Decide whether inventory has reached an explicit active-exit threshold.
///
/// A zero threshold or chunk disables active exit. The threshold is expressed
/// as a percentage of `max_position`; values over 100 are invalid/disabled so
/// a typo cannot create a surprising late exit. The result is only a plan —
/// callers must cancel stale quotes and submit a reduce-only order separately.
pub(crate) fn inventory_exit_plan(
    position: f64,
    max_position: f64,
    trigger_pct: f64,
    chunk_qty: f64,
) -> Option<InventoryExit> {
    if !position.is_finite()
        || !max_position.is_finite()
        || !trigger_pct.is_finite()
        || !chunk_qty.is_finite()
        || max_position <= 0.0
        || trigger_pct <= 0.0
        || trigger_pct > 100.0
        || chunk_qty <= 0.0
    {
        return None;
    }

    let abs_position = position.abs();
    // Trigger once |position| reaches the threshold. Exact comparison: the old
    // `+ f64::EPSILON` nudge was sub-tick noise at any real qty scale (machine
    // epsilon is the ULP at 1.0), so it changed nothing meaningful. Not
    // reaching the threshold by a genuine tick means the exit legitimately
    // should not fire yet.
    if abs_position < max_position * trigger_pct / 100.0 {
        return None;
    }
    Some(InventoryExit {
        side: if position > 0.0 {
            OrderSide::Sell
        } else {
            OrderSide::Buy
        },
        qty: abs_position.min(chunk_qty),
        kind: ExitKind::InventoryTrim,
    })
}

/// Wind-down exit: flatten any position larger than the quantity tolerance in
/// one reduce-only request, ignoring the configured trigger thresholds. The
/// whole residual is taken at once because session position caps are small by
/// design. Like [`inventory_exit_plan`] this is only a plan — the caller
/// cancels stale quotes and submits the reduce-only order separately.
pub(crate) fn wind_down_exit_plan(position: f64, qty_tolerance: f64) -> Option<InventoryExit> {
    if !position.is_finite() || !qty_tolerance.is_finite() || qty_tolerance < 0.0 {
        return None;
    }
    let abs_position = position.abs();
    if abs_position <= qty_tolerance {
        return None;
    }
    Some(InventoryExit {
        side: if position > 0.0 {
            OrderSide::Sell
        } else {
            OrderSide::Buy
        },
        qty: abs_position,
        kind: ExitKind::WindDown,
    })
}

/// A quote currently resting (a real order in live mode, simulated in paper
/// mode).
#[derive(Debug, Clone, PartialEq)]
pub struct RestingQuote {
    /// Exchange order id (None in paper mode / before adoption).
    pub order_id: Option<String>,
    pub side: OrderSide,
    pub level: u32,
    pub price: f64,
    pub qty: f64,
    /// The quote center (`skew_center(mark, position)`) when this quote was
    /// placed — the anti-flicker anchor. Equals the mark at placement when
    /// skew is off; re-quoting keys off drift of the current center from this.
    pub ref_center: f64,
    pub placed_at_cycle: u64,
}

/// Why a resting quote is being cancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelReason {
    /// The quote center drifted more than `refresh_bps` from the quote's
    /// `ref_center` — driven by mark movement and/or inventory skew.
    MarkMovedBeyondRefresh,
    /// The resting price left the eligibility band (earns nothing there).
    OutsideBand,
    /// The resting price now crosses the touch (would fill as taker).
    WouldCross,
    /// The quote's side is suppressed by the max-position limit.
    SideSuppressed,
    /// No desired quote exists at this (side, level) anymore.
    Stale,
}

impl CancelReason {
    /// Snake-case label for machine-readable output.
    pub fn as_str(&self) -> &'static str {
        match self {
            CancelReason::MarkMovedBeyondRefresh => "mark_moved",
            CancelReason::OutsideBand => "outside_band",
            CancelReason::WouldCross => "would_cross",
            CancelReason::SideSuppressed => "side_suppressed",
            CancelReason::Stale => "stale",
        }
    }
}

/// One reconcile decision.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Place(DesiredQuote),
    Cancel {
        order_id: Option<String>,
        side: OrderSide,
        level: u32,
        price: f64,
        reason: CancelReason,
    },
    Hold {
        side: OrderSide,
        level: u32,
        price: f64,
        age_cycles: u64,
        /// Current drift of the quote center from the quote's ref_center, in
        /// bps (for display).
        drift_bps: f64,
    },
}

/// Round half-up to `decimals` decimal places.
pub fn round_to_decimals(value: f64, decimals: u32) -> f64 {
    let factor = 10f64.powi(decimals as i32);
    (value * factor).round() / factor
}

/// Round DOWN to `decimals` decimal places (used for buy prices).
pub(crate) fn floor_to_decimals(value: f64, decimals: u32) -> f64 {
    let factor = 10f64.powi(decimals as i32);
    // Nudge by a hair to avoid f64 representation artifacts like
    // 99.90 * 100 = 9989.999999... flooring to 99.89.
    ((value * factor) + 1e-9).floor() / factor
}

/// Round UP to `decimals` decimal places (used for sell prices).
pub(crate) fn ceil_to_decimals(value: f64, decimals: u32) -> f64 {
    let factor = 10f64.powi(decimals as i32);
    ((value * factor) - 1e-9).ceil() / factor
}

/// Format for API strings with exactly `decimals` decimal places.
pub fn format_decimals(value: f64, decimals: u32) -> String {
    format!("{:.*}", decimals as usize, value)
}

/// Absolute difference between `a` and `b` in basis points of `b`.
/// Returns 0.0 when `b` is 0 (avoids division blowup on degenerate input).
pub fn bps_diff(a: f64, b: f64) -> f64 {
    if b == 0.0 {
        return 0.0;
    }
    ((a - b) / b).abs() * 10_000.0
}

/// Divergence between mark price and the book mid, in bps of mark.
///
/// A large value means the two data sources disagree (stale feed, bad print,
/// or a dislocated book) — quotes anchored to mark would sit nonsensically
/// relative to the book, so callers should skip acting on such a snapshot.
pub fn mark_mid_divergence_bps(mark: f64, best_bid: f64, best_ask: f64) -> f64 {
    bps_diff((best_bid + best_ask) / 2.0, mark)
}

/// Inventory-skewed quote center.
///
/// The quote ladder is built around this instead of mark. Holding a long
/// position (`position > 0`) shifts the center DOWN, which moves the reducing
/// side (sell) nearer the true mark (more likely to fill) and the growing side
/// (buy) further away (less likely to fill) — turning `max_position` from a
/// hard brake into gradual mean reversion. Short positions shift it up. The
/// shift scales linearly with inventory and saturates at `skew_bps` when
/// `|position| >= max_position`. Returns mark unchanged when skew is off or
/// `max_position` is non-positive.
pub(crate) fn skew_center(cfg: &MakerConfig, mark: f64, position: f64) -> f64 {
    if cfg.max_position <= 0.0 {
        return mark;
    }
    let inv_ratio = (position / cfg.max_position).clamp(-1.0, 1.0);
    mark * (1.0 - cfg.skew_bps * inv_ratio / 1e4)
}

/// Nonlinear-aware quote center (stage 3 v1). With the feature disabled this
/// is byte-for-byte the legacy [`skew_center`] path; enabled, the shift grows
/// `boost`× steeper than linear and saturates at `cap_bps`:
/// `shift = sign(ratio) × min(skew_bps × boost × |ratio|, cap_bps)`.
/// Stateless on purpose — no hysteresis, so strength tracks the live position
/// with no latch (the failure mode that rejected v0).
pub(crate) fn skew_center_with(
    cfg: &MakerConfig,
    nonlinear: NonlinearSkewConfig,
    mark: f64,
    position: f64,
) -> f64 {
    if !nonlinear.enabled {
        return skew_center(cfg, mark, position);
    }
    if cfg.max_position <= 0.0 {
        return mark;
    }
    let inv_ratio = (position / cfg.max_position).clamp(-1.0, 1.0);
    let shift_bps = (cfg.skew_bps * nonlinear.boost * inv_ratio.abs()).min(nonlinear.cap_bps);
    mark * (1.0 - shift_bps * inv_ratio.signum() / 1e4)
}

/// The single quote-center composition point used by planning, price
/// generation, and refresh reconciliation.
///
/// Keep the zero-shift branch on the exact legacy path. Besides documenting the
/// default-off contract, this prevents even a mathematically neutral extra
/// floating-point operation from entering old action sequences.
pub(crate) fn quote_center(
    cfg: &MakerConfig,
    nonlinear: NonlinearSkewConfig,
    external_shift_bps: f64,
    mark: f64,
    position: f64,
) -> f64 {
    let inventory_center = skew_center_with(cfg, nonlinear, mark, position);
    if external_shift_bps == 0.0 {
        inventory_center
    } else {
        inventory_center * (1.0 + external_shift_bps / 1e4)
    }
}

/// Whether a quote at `price` on `side` crosses the current touch: a buy at or
/// above the best ask (`price >= best_ask`), or a sell at or below the best bid
/// (`price <= best_bid`). Returns false when the relevant book side is absent.
///
/// This one predicate answers both questions the maker asks about the touch:
///
/// - *paper mode*: "would this resting quote have filled?" — a discrete-time
///   "crossed → filled" proxy used only to simulate inventory. A real venue
///   matches on the trade stream instead.
/// - *live mode*: "does this resting quote cross the book?" — see
///   [`resting_quotes_would_cross`], which drives the `WouldCross` cancel and
///   the replan trigger.
///
/// The two must stay the same event: a paper fill the live path would have
/// cancelled (or vice versa) would make paper and live inventory diverge for
/// reasons that have nothing to do with the strategy.
pub fn quote_crosses_touch(
    side: OrderSide,
    price: f64,
    best_bid: Option<f64>,
    best_ask: Option<f64>,
) -> bool {
    match side {
        OrderSide::Buy => best_ask.is_some_and(|ask| price >= ask),
        OrderSide::Sell => best_bid.is_some_and(|bid| price <= bid),
    }
}

/// Market data required to make one maker decision.
///
/// This intentionally contains only plain values so it can be recorded and
/// replayed without a client, websocket, or clock.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MarketSnapshot {
    pub mark: f64,
    pub best_bid: Option<f64>,
    pub best_ask: Option<f64>,
}

/// Why the strategy refused to make a decision for a market snapshot.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CycleSkip {
    /// The best bid is at or above the best ask, so no safe touch exists.
    CrossedBook,
    /// Mark and book mid disagree enough that either source may be stale.
    MarkMidDivergence { divergence_bps: f64 },
    /// A live maker cannot safely enforce post-only pricing without both sides.
    MissingTouch,
}

/// Result of the checks that must run before any account or order I/O.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CyclePreflight {
    /// The volatility breaker state after observing this mark.
    pub halted: bool,
    /// Present when the caller must skip the whole cycle.
    pub skip: Option<CycleSkip>,
}

/// The touch-level reason a two-sided book cannot be quoted, if any: a crossed
/// book (`best_bid >= best_ask`) or mark/mid divergence beyond the limit.
///
/// Returns `None` for a healthy book or a one-sided/missing touch — the caller
/// decides how to treat a missing side. Shared by [`preflight_cycle`] (which
/// maps it to a skip) and the CLI's replan trigger so the two cannot drift.
pub fn touch_skip(
    mark: f64,
    best_bid: Option<f64>,
    best_ask: Option<f64>,
    max_divergence_bps: f64,
) -> Option<CycleSkip> {
    let (best_bid, best_ask) = (best_bid?, best_ask?);
    if best_bid >= best_ask {
        return Some(CycleSkip::CrossedBook);
    }
    let divergence_bps = mark_mid_divergence_bps(mark, best_bid, best_ask);
    if divergence_bps > max_divergence_bps {
        return Some(CycleSkip::MarkMidDivergence { divergence_bps });
    }
    None
}

/// Observe volatility and validate a snapshot before account/order I/O.
///
/// A skipped cycle deliberately leaves resting quotes untouched. This mirrors
/// the existing fail-safe behavior: bad market data must not trigger a blind
/// cancel-and-replace sequence.
pub fn preflight_cycle(
    breaker: &mut VolBreaker,
    market: MarketSnapshot,
    max_divergence_bps: f64,
    require_full_touch: bool,
) -> CyclePreflight {
    let halted = breaker.observe(market.mark);
    if let Some(skip) = touch_skip(
        market.mark,
        market.best_bid,
        market.best_ask,
        max_divergence_bps,
    ) {
        return CyclePreflight {
            halted,
            skip: Some(skip),
        };
    }
    if require_full_touch && (market.best_bid.is_none() || market.best_ask.is_none()) {
        return CyclePreflight {
            halted,
            skip: Some(CycleSkip::MissingTouch),
        };
    }
    CyclePreflight { halted, skip: None }
}

/// Timestamped variant used by duration-based volatility windows.
pub fn preflight_cycle_at(
    breaker: &mut VolBreaker,
    event_time_ms: i64,
    market: MarketSnapshot,
    max_divergence_bps: f64,
    require_full_touch: bool,
) -> Result<CyclePreflight, VolatilityError> {
    let halted = breaker.observe_at(event_time_ms, market.mark)?;
    if let Some(skip) = touch_skip(
        market.mark,
        market.best_bid,
        market.best_ask,
        max_divergence_bps,
    ) {
        return Ok(CyclePreflight {
            halted,
            skip: Some(skip),
        });
    }
    if require_full_touch && (market.best_bid.is_none() || market.best_ask.is_none()) {
        return Ok(CyclePreflight {
            halted,
            skip: Some(CycleSkip::MissingTouch),
        });
    }
    Ok(CyclePreflight { halted, skip: None })
}

/// Inputs owned by the strategy for one post-account-sync decision.
#[derive(Debug, Clone, Copy)]
pub struct CycleInput<'a> {
    pub cycle: u64,
    pub market: MarketSnapshot,
    pub position: f64,
    pub resting: &'a [RestingQuote],
    /// Submitted orders that have not become visible in the venue order book.
    pub pending_slots: &'a [(OrderSide, u32)],
    pub market_data_mode: MarketDataMode,
    pub active_exit_enabled: bool,
    pub inventory_exit_pct: f64,
    pub inventory_exit_qty: f64,
    pub size_skew: SizeSkewDecision,
    /// Stage 3 v1 nonlinear price-skew strength; disabled ≡ legacy linear skew.
    pub nonlinear_skew: NonlinearSkewConfig,
    /// Continuous external-price center offset configuration; default disabled.
    pub external_skew: ExternalSkewConfig,
    /// Fresh, finite excess from the external-guard signal chain. `None` fails
    /// open to an exact zero shift.
    pub external_excess_bps: Option<f64>,
    /// External-price guard outcome for this cycle; inactive ≡ no suppression.
    pub guard: GuardDecision,
    /// Supervisor-requested wind-down (e.g. an A/B arm past its scheduled
    /// window): never place new quotes again and flatten any residual
    /// position through the reduce-only exit path, ignoring the configured
    /// exit thresholds. Converges to flat instead of re-accumulating.
    pub wind_down: bool,
    /// Positions at or below this magnitude count as flat during wind-down.
    pub qty_tolerance: f64,
}

/// A deterministic plan for the executor to apply after a successful preflight.
#[derive(Debug, Clone, PartialEq)]
pub struct CyclePlan {
    /// The configured exit request before volatility policy is applied.
    /// Callers use this to track venue confirmation and avoid duplicate exits.
    pub requested_inventory_exit: Option<InventoryExit>,
    /// The active exit to submit this cycle. A volatility halt always suppresses it.
    pub inventory_exit: Option<InventoryExit>,
    /// Set when the exit policy produced a plan that this cycle does not
    /// submit: which policy asked, and why it was suppressed. `None` when no
    /// exit was wanted or the wanted exit is being submitted.
    pub exit_suppression: Option<SuppressedExit>,
    /// Cancels, places, and holds in executor-safe order.
    pub actions: Vec<Action>,
    /// Inventory-only anchor retained for the legacy `skew_shift_bps`
    /// telemetry contract.
    pub inventory_ref_center: f64,
    /// Anchor used for any newly submitted quote.
    pub ref_center: f64,
    /// External component actually applied to the shared quote center this cycle.
    pub external_skew_shift_bps: f64,
}

/// Build a deterministic quote/exit plan after the caller has synchronized
/// position and resting orders with the venue.
///
/// The caller owns transport state (pending HTTP submissions and exit
/// acknowledgements) and must run [`preflight_cycle`] first. This function
/// deliberately cannot perform I/O.
///
/// With `input.wind_down` set (a supervisor requesting session end, e.g. an
/// A/B arm past its scheduled window) the plan converges to flat: no new
/// quotes are ever desired — even once the position reaches zero — and any
/// residual position above `input.qty_tolerance` yields a reduce-only exit
/// plan regardless of the configured exit thresholds.
pub fn plan_cycle(cfg: &MakerConfig, input: CycleInput<'_>, halted: bool) -> CyclePlan {
    let external_shift_bps =
        external_skew_shift_bps(input.external_skew, input.external_excess_bps);
    let market_active = input.market_data_mode == MarketDataMode::Active;
    // What the configured exit policy asks for, before any market-state or
    // volatility gate. Kept separate purely so suppression is observable: the
    // gates below decide what is actually requested and submitted.
    let policy_exit = (input.active_exit_enabled || input.wind_down)
        .then(|| {
            if input.wind_down {
                wind_down_exit_plan(input.position, input.qty_tolerance)
            } else {
                inventory_exit_plan(
                    input.position,
                    cfg.max_position,
                    input.inventory_exit_pct,
                    input.inventory_exit_qty,
                )
            }
        })
        .flatten();
    let requested_inventory_exit = market_active.then(|| policy_exit.clone()).flatten();

    // During a volatility halt, pull resting liquidity but never send an
    // opt-in taker exit — for either exit kind. Emergency execution during a
    // halt needs a separate explicitly authorized policy that deliberately
    // does not exist (docs/26 decision D1).
    let inventory_exit = (!halted)
        .then_some(requested_inventory_exit.clone())
        .flatten();
    // Inactive market data outranks the halt as a reason: without a trusted
    // price the halt verdict itself is computed from stale marks.
    let exit_suppression = policy_exit.as_ref().and_then(|exit| {
        let reason = match (market_active, halted) {
            (false, _) => Some(ExitSuppression::MarketDataInactive),
            (true, true) => Some(ExitSuppression::VolatilityHalt),
            (true, false) => None,
        }?;
        Some(SuppressedExit {
            kind: exit.kind,
            reason,
        })
    });
    // Wind-down never places new quotes, even once flat: the session must
    // converge to flat instead of re-accumulating inventory.
    let desired = if halted || !market_active || inventory_exit.is_some() || input.wind_down {
        Vec::new()
    } else {
        let raw = compute_desired_quotes(
            cfg,
            input.market.mark,
            input.market.best_bid,
            input.market.best_ask,
            input.position,
            input.size_skew,
            input.nonlinear_skew,
            external_shift_bps,
            input.guard,
        );
        cap_desired_exposure(cfg, input.position, &raw, input.pending_slots)
    };

    CyclePlan {
        requested_inventory_exit,
        inventory_exit,
        exit_suppression,
        actions: reconcile(
            cfg,
            input.market.mark,
            input.position,
            input.market.best_bid,
            input.market.best_ask,
            &desired,
            input.resting,
            input.cycle,
            input.nonlinear_skew,
            external_shift_bps,
        ),
        inventory_ref_center: quote_center(
            cfg,
            input.nonlinear_skew,
            0.0,
            input.market.mark,
            input.position,
        ),
        ref_center: quote_center(
            cfg,
            input.nonlinear_skew,
            external_shift_bps,
            input.market.mark,
            input.position,
        ),
        external_skew_shift_bps: external_shift_bps,
    }
}

/// Compute the desired quote set for the current market snapshot.
///
/// Applies, in order: the inventory-skewed spread/level ladder, the band clamp,
/// the no-cross clamp, directional tick rounding (with band re-entry), the
/// min-qty filter, and max-position side suppression. Quotes that fail a guard
/// are dropped; duplicate prices after clamping/rounding are collapsed (outer
/// level wins nothing — the inner level is kept).
#[allow(clippy::too_many_arguments)]
pub(crate) fn compute_desired_quotes(
    cfg: &MakerConfig,
    mark: f64,
    best_bid: Option<f64>,
    best_ask: Option<f64>,
    position: f64,
    size_skew: SizeSkewDecision,
    nonlinear_skew: NonlinearSkewConfig,
    external_shift_bps: f64,
    guard: GuardDecision,
) -> Vec<DesiredQuote> {
    let mut out = Vec::new();
    if !mark.is_finite()
        || mark <= 0.0
        || best_bid.is_some_and(|price| !price.is_finite() || price <= 0.0)
        || best_ask.is_some_and(|price| !price.is_finite() || price <= 0.0)
    {
        return out;
    }

    let qty = round_to_decimals(cfg.size, cfg.qty_decimals);
    if qty < cfg.min_order_qty || qty <= 0.0 {
        return out;
    }

    let tick = cfg.price_tick();
    // Band eligibility is defined around the TRUE mark, not the skewed center.
    let band_lo = mark * (1.0 - cfg.band_bps / 1e4);
    let band_hi = mark * (1.0 + cfg.band_bps / 1e4);

    // Ladder is centered on the shared quote center (inventory skew, then the
    // external offset); the band/no-cross guards below still reference the true
    // mark and touch, so an oversized offset gets clamped to the band edge
    // rather than moving the band with it.
    let center = quote_center(cfg, nonlinear_skew, external_shift_bps, mark, position);

    let suppress_buy = position >= cfg.max_position;
    let suppress_sell = position <= -cfg.max_position;

    for side in [OrderSide::Buy, OrderSide::Sell] {
        if (side == OrderSide::Buy && suppress_buy) || (side == OrderSide::Sell && suppress_sell) {
            continue;
        }
        // External-price guard: the endangered side's quotes are stale against
        // a leading market that has already moved — do not quote it this
        // cycle. Resting quotes on that side cancel via the SideSuppressed
        // path; the guard releases once StandX's mark catches up.
        if guard.active && guard.endangered == Some(side) {
            continue;
        }
        let side_qty = if size_skew.active && size_skew.add_side == Some(side) {
            let Some(add_qty) = size_skew.add_qty else {
                continue;
            };
            add_qty
        } else {
            qty
        };
        let mut last_price: Option<f64> = None;
        for level in 0..cfg.levels {
            let offset_bps = cfg.spread_bps + level as f64 * cfg.level_step_bps;
            let raw_price = match side {
                OrderSide::Buy => center * (1.0 - offset_bps / 1e4),
                OrderSide::Sell => center * (1.0 + offset_bps / 1e4),
            };

            // Intersect the eligibility band with the post-only no-cross
            // interval. If no tick can satisfy both, omit this side instead of
            // emitting a quote outside the band or relying on ALO rejection.
            let (price_lo, price_hi) = match side {
                OrderSide::Buy => (
                    band_lo,
                    best_ask.map_or(band_hi, |ask| band_hi.min(ask - tick)),
                ),
                OrderSide::Sell => (
                    best_bid.map_or(band_lo, |bid| band_lo.max(bid + tick)),
                    band_hi,
                ),
            };
            let price_tolerance = tick * 1e-6;
            if !raw_price.is_finite()
                || !price_lo.is_finite()
                || !price_hi.is_finite()
                || price_lo > price_hi + price_tolerance
            {
                continue;
            }

            let mut price = raw_price.clamp(price_lo, price_hi);

            // Directional tick rounding: away from mark, so rounding never
            // pushes us through the touch.
            price = match side {
                OrderSide::Buy => floor_to_decimals(price, cfg.price_decimals),
                OrderSide::Sell => ceil_to_decimals(price, cfg.price_decimals),
            };

            // Directional rounding can leave the feasible interval when the
            // band boundary is not tick-aligned. Snap back to the nearest
            // valid tick, then re-check every constraint.
            if price < price_lo {
                price = ceil_to_decimals(price_lo, cfg.price_decimals);
            } else if price > price_hi {
                price = floor_to_decimals(price_hi, cfg.price_decimals);
            }

            if !price.is_finite()
                || price <= 0.0
                || price < price_lo - price_tolerance
                || price > price_hi + price_tolerance
                || best_ask.is_some_and(|ask| side == OrderSide::Buy && price >= ask)
                || best_bid.is_some_and(|bid| side == OrderSide::Sell && price <= bid)
            {
                continue;
            }

            // Collapse duplicate levels (clamping can flatten the ladder).
            if last_price == Some(price) {
                continue;
            }
            last_price = Some(price);

            out.push(DesiredQuote {
                side,
                level,
                price,
                qty: side_qty,
            });
        }
    }

    out
}

/// Limit a desired ladder so that all quotes on either side filling cannot
/// push the account beyond `max_position`.
///
/// Position-only suppression is insufficient for a multi-level ladder: while
/// the current position may be inside the cap, several resting bids (or asks)
/// can all fill before the next reconciliation cycle. This guard budgets each
/// directional ladder independently. `reserved_slots` are considered first;
/// callers use them for submitted-but-not-yet-visible orders, so transport
/// delay cannot make a later level lose its exposure reservation.
pub(crate) fn cap_desired_exposure(
    cfg: &MakerConfig,
    position: f64,
    desired: &[DesiredQuote],
    reserved_slots: &[(OrderSide, u32)],
) -> Vec<DesiredQuote> {
    let mut buy_budget = (cfg.max_position - position).max(0.0);
    let mut sell_budget = (cfg.max_position + position).max(0.0);
    let mut candidates = desired.to_vec();
    // Stable ordering keeps the configured inner-to-outer ladder order while
    // moving only submitted-but-not-yet-visible slots to the front.
    candidates.sort_by_key(|quote| !reserved_slots.contains(&(quote.side, quote.level)));

    candidates
        .into_iter()
        .filter(|quote| {
            let budget = match quote.side {
                OrderSide::Buy => &mut buy_budget,
                OrderSide::Sell => &mut sell_budget,
            };
            // Retain only full, tick-aligned orders. Shrinking a level would
            // create a quantity not represented by the strategy's config and
            // could fall below the venue's minimum order size. Allow half a qty
            // tick of slack so a budget that lands a hair under a whole-tick
            // quote (float noise) still admits it.
            if quote.qty <= *budget + cfg.qty_tick() / 2.0 {
                *budget = (*budget - quote.qty).max(0.0);
                true
            } else {
                false
            }
        })
        .collect()
}

/// Whether any resting quote crosses the current touch (buy at/above the ask,
/// sell at/below the bid).
pub fn resting_quotes_would_cross(
    resting: &[RestingQuote],
    best_bid: Option<f64>,
    best_ask: Option<f64>,
) -> bool {
    resting
        .iter()
        .any(|quote| quote_crosses_touch(quote.side, quote.price, best_bid, best_ask))
}

/// Diff desired vs resting quotes, applying the anti-flicker hold rule.
///
/// Decision table per resting quote (checked in order):
///
/// | # | Condition                                        | Action                        |
/// |---|--------------------------------------------------|-------------------------------|
/// | 1 | side suppressed by max-position                  | Cancel (SideSuppressed)       |
/// | 2 | no desired quote at (side, level)                | Cancel (Stale)                |
/// | 3 | price outside current band                       | Cancel (OutsideBand)          |
/// | 4 | price crosses current touch                      | Cancel (WouldCross)           |
/// | 5 | quote center drifted > refresh_bps from ref_center | Cancel (MarkMovedBeyondRefresh) |
/// | 6 | otherwise                                        | Hold (anti-flicker)           |
///
/// The center (row 5) is `skew_center(mark, position)`, so this single rule
/// re-quotes on both mark movement and inventory skew; with skew off it is the
/// bare mark, identical to prior behavior. Every desired quote without a
/// surviving resting counterpart yields a `Place`. The returned Vec orders all
/// Cancels before all Places so the executor frees margin before re-placing;
/// Holds come last.
#[allow(clippy::too_many_arguments)]
pub(crate) fn reconcile(
    cfg: &MakerConfig,
    mark: f64,
    position: f64,
    best_bid: Option<f64>,
    best_ask: Option<f64>,
    desired: &[DesiredQuote],
    resting: &[RestingQuote],
    cycle: u64,
    nonlinear_skew: NonlinearSkewConfig,
    external_shift_bps: f64,
) -> Vec<Action> {
    // Band/no-cross reference the true mark and touch; the anti-flicker anchor
    // uses the shared quote center (inventory skew plus the external offset),
    // the same one `compute_desired_quotes` priced this cycle's ladder from —
    // comparing against a different anchor would requote every cycle the
    // offset moved.
    let band_lo = mark * (1.0 - cfg.band_bps / 1e4);
    let band_hi = mark * (1.0 + cfg.band_bps / 1e4);
    let center = quote_center(cfg, nonlinear_skew, external_shift_bps, mark, position);

    let desired_has = |side: OrderSide, level: u32| -> bool {
        desired.iter().any(|d| d.side == side && d.level == level)
    };
    // A side with zero desired quotes this cycle is suppressed (either by
    // max-position or because every quote failed a guard).
    let side_live = |side: OrderSide| -> bool { desired.iter().any(|d| d.side == side) };

    let mut cancels = Vec::new();
    let mut holds = Vec::new();
    // (side, level) pairs covered by a surviving (held) resting quote.
    let mut covered: Vec<(OrderSide, u32)> = Vec::new();

    for r in resting {
        let reason = if !side_live(r.side) {
            Some(CancelReason::SideSuppressed)
        } else if !desired_has(r.side, r.level) {
            Some(CancelReason::Stale)
        } else if r.price < band_lo || r.price > band_hi {
            Some(CancelReason::OutsideBand)
        } else if resting_quotes_would_cross(std::slice::from_ref(r), best_bid, best_ask) {
            Some(CancelReason::WouldCross)
        } else if bps_diff(center, r.ref_center) > cfg.refresh_bps {
            Some(CancelReason::MarkMovedBeyondRefresh)
        } else {
            None
        };

        match reason {
            Some(reason) => cancels.push(Action::Cancel {
                order_id: r.order_id.clone(),
                side: r.side,
                level: r.level,
                price: r.price,
                reason,
            }),
            None => {
                covered.push((r.side, r.level));
                holds.push(Action::Hold {
                    side: r.side,
                    level: r.level,
                    price: r.price,
                    age_cycles: cycle.saturating_sub(r.placed_at_cycle),
                    drift_bps: bps_diff(center, r.ref_center),
                });
            }
        }
    }

    let places: Vec<Action> = desired
        .iter()
        .filter(|d| !covered.contains(&(d.side, d.level)))
        .map(|d| Action::Place(d.clone()))
        .collect();

    // Cancels first (free margin), then places, then holds (display only).
    let mut actions = cancels;
    actions.extend(places);
    actions.extend(holds);
    actions
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
