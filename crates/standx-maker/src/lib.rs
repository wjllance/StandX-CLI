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

pub mod account_projection;
pub mod external_guard;
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
    MAX_PENDING_ORDER_REQUESTS,
};
pub use external_guard::{
    ExternalDivergence, GuardConfig, GuardController, GuardDecision, GuardError,
};
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
/// and hands the residual position off, it never trades), and inventing one
/// without evidence is exactly what
/// [docs/26](../../../docs/26-maker-stage5b-design.md) declines to do.
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

/// Running telemetry for a maker session: fills, mark-to-market PnL, spread
/// capture, two-sided uptime, and inventory extent.
///
/// PnL is mark-to-market via a signed cash accumulator: a buy of `q@p` does
/// `cash -= p*q`, a sell `cash += p*q`, and equity is `cash + position*mark`.
/// This credits captured spread (fills away from mark) and inventory drift in
/// one number. Spread capture is the favorable distance of each fill from the
/// mark at fill time, in bps (positive = earned edge).
#[derive(Debug, Clone, Default)]
pub struct MakerStats {
    pub cycles: u64,
    pub two_sided_cycles: u64,
    pub buy_fills: u64,
    pub sell_fills: u64,
    /// Total filled base quantity (both sides).
    pub filled_qty: f64,
    /// Signed quote cash flow from fills (see struct docs).
    pub cash: f64,
    spread_bps_sum: f64,
    spread_bps_n: u64,
    pub max_abs_position: f64,
    /// Last observed position, used for mark-to-market and inventory telemetry.
    last_position: f64,
}

impl MakerStats {
    /// Start a maker session while adopting an existing venue position.
    /// Session PnL is zero at `baseline_mark`; venue/account PnL retains its
    /// historical cost basis and is reported separately by the CLI.
    pub fn with_inventory_baseline(position: f64, baseline_mark: f64) -> Self {
        Self {
            cash: -position * baseline_mark,
            max_abs_position: position.abs(),
            last_position: position,
            ..Self::default()
        }
    }

    /// Record an executed fill at `price` against `mark` at fill time.
    pub fn record_fill(&mut self, side: OrderSide, price: f64, qty: f64, mark: f64) {
        self.filled_qty += qty;
        match side {
            OrderSide::Buy => {
                self.buy_fills += 1;
                self.cash -= price * qty;
            }
            OrderSide::Sell => {
                self.sell_fills += 1;
                self.cash += price * qty;
            }
        }
        // Favorable distance from mark: a buy earns when below mark, a sell
        // when above.
        if mark > 0.0 {
            let capture = match side {
                OrderSide::Buy => (mark - price) / mark,
                OrderSide::Sell => (price - mark) / mark,
            } * 10_000.0;
            self.spread_bps_sum += capture;
            self.spread_bps_n += 1;
        }
    }

    /// Synchronize the cached telemetry position with the authoritative
    /// current-run ledger without closing another maker cycle.
    pub(crate) fn observe_position(&mut self, position: f64) {
        self.last_position = position;
        self.max_abs_position = self.max_abs_position.max(position.abs());
    }

    /// Close out a cycle after the caller has recorded exact venue fills.
    /// `two_sided` is whether both a bid and an ask were resting this cycle.
    pub fn end_cycle(&mut self, position: f64, two_sided: bool) {
        self.observe_position(position);
        self.cycles += 1;
        if two_sided {
            self.two_sided_cycles += 1;
        }
    }

    /// Total fills across both sides.
    pub fn fills(&self) -> u64 {
        self.buy_fills + self.sell_fills
    }

    /// Mark-to-market equity: realized cash plus inventory valued at `mark`.
    pub fn pnl(&self, position: f64, mark: f64) -> f64 {
        self.cash + position * mark
    }

    /// The last observed position.
    pub fn position(&self) -> f64 {
        self.last_position
    }

    /// Fraction of cycles (0–100) with quotes resting on both sides.
    pub fn uptime_pct(&self) -> f64 {
        if self.cycles == 0 {
            return 0.0;
        }
        self.two_sided_cycles as f64 / self.cycles as f64 * 100.0
    }

    /// Average favorable spread capture per fill, in bps (0 with no fills).
    pub fn avg_spread_capture_bps(&self) -> f64 {
        if self.spread_bps_n == 0 {
            return 0.0;
        }
        self.spread_bps_sum / self.spread_bps_n as f64
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
    /// Anchor used for any newly submitted quote.
    pub ref_center: f64,
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
        ),
        ref_center: skew_center_with(cfg, input.nonlinear_skew, input.market.mark, input.position),
    }
}

/// A risk alert raised (or cleared) by [`AlertMonitor`].
#[derive(Debug, Clone, PartialEq)]
pub struct Alert {
    /// Machine-readable slug: `loss` | `inventory` | `uptime`.
    pub kind: &'static str,
    /// true = the condition just started breaching; false = it just recovered.
    pub firing: bool,
    /// Human-readable one-liner.
    pub message: String,
}

/// Hysteresis: a fired loss alert clears once PnL has recovered past half the
/// configured limit, so PnL hovering at the limit cannot flap the alert.
const LOSS_ALERT_CLEAR_FRACTION: f64 = 0.5;
/// Hysteresis: a fired inventory alert clears once |position| falls back below
/// 90% of the alert threshold.
const INVENTORY_ALERT_CLEAR_FRACTION: f64 = 0.9;
/// Hysteresis: a fired equity/margin alert clears once the account recovers to
/// 10% above the alert threshold.
const ACCOUNT_ALERT_CLEAR_MULTIPLE: f64 = 1.1;

/// One edge-triggered alert condition: it reports a transition only when the
/// breach state actually changes, so a held breach does not re-emit every
/// cycle.
///
/// The caller passes the two evaluated predicates rather than a threshold
/// pair, because the alerts do not share a polarity or a boundary convention
/// (PnL and inventory include the threshold value, uptime and the account
/// alerts exclude it). Keeping each comparison at its call site keeps the
/// boundary documented next to the threshold it belongs to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct EdgeTrigger {
    on: bool,
}

impl EdgeTrigger {
    /// `Some(true)` when the condition just started breaching, `Some(false)`
    /// when it just recovered, `None` when the state is unchanged.
    fn observe(&mut self, breaching: bool, recovered: bool) -> Option<bool> {
        match (self.on, breaching, recovered) {
            (false, true, _) => {
                self.on = true;
                Some(true)
            }
            (true, _, true) => {
                self.on = false;
                Some(false)
            }
            _ => None,
        }
    }
}

/// Threshold-based risk alerting over the running [`MakerStats`]. Each
/// condition is edge-triggered — it emits once when it starts breaching and
/// once when it recovers — so a held breach doesn't spam every cycle. Delivery
/// (stderr / webhook) is the caller's job; this type only decides.
///
/// Each threshold is independently opt-in (0 disables it).
#[derive(Debug, Clone, Default)]
pub struct AlertMonitor {
    /// Alert when mark-to-market PnL <= -loss_limit (quote units). 0 = off.
    loss_limit: f64,
    /// Alert when |position| >= max_position * inventory_pct/100. 0 = off.
    inventory_pct: f64,
    /// Alert when two-sided uptime% < uptime_floor (after warmup). 0 = off.
    uptime_floor: f64,
    /// Alert when account equity drops below this (quote units). 0 = off.
    equity_alert_below: f64,
    /// Alert when available cross margin drops below this (quote units).
    /// 0 = off.
    margin_alert_below: f64,
    loss: EdgeTrigger,
    inventory: EdgeTrigger,
    uptime: EdgeTrigger,
    equity: EdgeTrigger,
    margin: EdgeTrigger,
}

impl AlertMonitor {
    /// Uptime is meaningless in the first few cycles; don't alert on it until
    /// the session has run at least this long.
    const UPTIME_WARMUP_CYCLES: u64 = 20;

    pub fn new(loss_limit: f64, inventory_pct: f64, uptime_floor: f64) -> Self {
        Self {
            loss_limit,
            inventory_pct,
            uptime_floor,
            ..Default::default()
        }
    }

    /// Configure the account equity / available-margin **alert** thresholds
    /// (quote units); 0 disables either one.
    ///
    /// These only notify. The word "floor" is reserved for the stage 5-b hard
    /// floors (`stop_equity_below` / `stop_margin_below`), which stop the
    /// session through a separate typed outcome — an alert threshold and a
    /// solvency brake must never be mistaken for each other in config or code.
    pub fn with_account_alerts(mut self, equity_alert_below: f64, margin_alert_below: f64) -> Self {
        self.equity_alert_below = equity_alert_below;
        self.margin_alert_below = margin_alert_below;
        self
    }

    /// Whether any of the session-metric alerts evaluated by
    /// [`AlertMonitor::evaluate`] (loss / inventory / uptime) is configured.
    /// The account alerts are gated separately by
    /// [`AlertMonitor::account_enabled`], because they need an account
    /// snapshot the paper path does not have.
    pub fn session_enabled(&self) -> bool {
        self.loss_limit > 0.0 || self.inventory_pct > 0.0 || self.uptime_floor > 0.0
    }

    /// Whether an account equity or available-margin alert is configured, i.e.
    /// whether [`AlertMonitor::evaluate_account`] has anything to evaluate.
    pub fn account_enabled(&self) -> bool {
        self.equity_alert_below > 0.0 || self.margin_alert_below > 0.0
    }

    /// Evaluate the current metrics and return only the alerts whose state
    /// changed this cycle (fired or cleared).
    pub fn evaluate(
        &mut self,
        stats: &MakerStats,
        position: f64,
        mark: f64,
        max_position: f64,
        cycle: u64,
    ) -> Vec<Alert> {
        let mut out = Vec::new();

        // Loss limit: fire at -loss_limit (inclusive), clear back above half.
        if self.loss_limit > 0.0 {
            let pnl = stats.pnl(position, mark);
            if let Some(firing) = self.loss.observe(
                pnl <= -self.loss_limit,
                pnl > -self.loss_limit * LOSS_ALERT_CLEAR_FRACTION,
            ) {
                out.push(Alert {
                    kind: "loss",
                    firing,
                    message: if firing {
                        format!(
                            "mark-to-market PnL {:+.2} breached loss limit -{:.2}",
                            pnl, self.loss_limit
                        )
                    } else {
                        format!("PnL recovered to {pnl:+.2}")
                    },
                });
            }
        }

        // Inventory: fire at pct of max_position (inclusive), clear below 0.9x.
        if self.inventory_pct > 0.0 && max_position > 0.0 {
            let threshold = max_position * self.inventory_pct / 100.0;
            let abs_pos = position.abs();
            if let Some(firing) = self.inventory.observe(
                abs_pos >= threshold,
                abs_pos < threshold * INVENTORY_ALERT_CLEAR_FRACTION,
            ) {
                out.push(Alert {
                    kind: "inventory",
                    firing,
                    message: if firing {
                        format!(
                            "position {:+.4} reached {:.0}% of max ({:.4})",
                            position, self.inventory_pct, max_position
                        )
                    } else {
                        format!("position back to {position:+.4}")
                    },
                });
            }
        }

        // Uptime: only after warmup. Deliberately has no hysteresis band —
        // it fires below the floor and clears the moment it is back at it,
        // because uptime is a slow-moving ratio that cannot flap per cycle.
        if self.uptime_floor > 0.0 && cycle >= Self::UPTIME_WARMUP_CYCLES {
            let uptime = stats.uptime_pct();
            if let Some(firing) = self
                .uptime
                .observe(uptime < self.uptime_floor, uptime >= self.uptime_floor)
            {
                out.push(Alert {
                    kind: "uptime",
                    firing,
                    message: if firing {
                        format!(
                            "two-sided uptime {:.0}% below floor {:.0}%",
                            uptime, self.uptime_floor
                        )
                    } else {
                        format!("uptime recovered to {uptime:.0}%")
                    },
                });
            }
        }

        out
    }

    /// Evaluate the account equity / available-margin **alert** thresholds
    /// against the latest account snapshot and return only the alerts whose
    /// state changed this cycle. Like [`AlertMonitor::evaluate`], each one is
    /// edge-triggered: it fires once on breach and clears once it recovers to
    /// 10% above the threshold (hysteresis avoids flapping).
    ///
    /// This never stops the session — see [`account_floor_breach`] for the
    /// hard floor.
    pub fn evaluate_account(&mut self, equity: f64, available: f64) -> Vec<Alert> {
        let mut out = Vec::new();

        if self.equity_alert_below > 0.0 {
            if let Some(firing) = self.equity.observe(
                equity < self.equity_alert_below,
                equity >= self.equity_alert_below * ACCOUNT_ALERT_CLEAR_MULTIPLE,
            ) {
                out.push(Alert {
                    kind: "equity",
                    firing,
                    message: if firing {
                        format!(
                            "account equity {:.2} below alert threshold {:.2}",
                            equity, self.equity_alert_below
                        )
                    } else {
                        format!("account equity recovered to {equity:.2}")
                    },
                });
            }
        }

        if self.margin_alert_below > 0.0 {
            if let Some(firing) = self.margin.observe(
                available < self.margin_alert_below,
                available >= self.margin_alert_below * ACCOUNT_ALERT_CLEAR_MULTIPLE,
            ) {
                out.push(Alert {
                    kind: "margin",
                    firing,
                    message: if firing {
                        format!(
                            "available margin {:.2} below alert threshold {:.2}",
                            available, self.margin_alert_below
                        )
                    } else {
                        format!("available margin recovered to {available:.2}")
                    },
                });
            }
        }

        out
    }
}

/// Which account-level hard floor was breached (stage 5-b).
///
/// Deliberately distinct from the session PnL brake (`stop_loss`): that one
/// says "this strategy is losing", these say "this account can no longer be
/// traded safely". Different config names, different typed stop reason,
/// different operator remedy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountFloorBreach {
    /// Account equity fell below `stop_equity_below`.
    Equity,
    /// Available cross margin fell below `stop_margin_below`.
    Margin,
}

impl AccountFloorBreach {
    /// Snake-case metric label for machine-readable output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Equity => "equity",
            Self::Margin => "margin",
        }
    }
}

/// Decide whether an account-level hard floor is breached.
///
/// Each floor is independently opt-in: `0` (or any non-positive / non-finite
/// value) disables it, which is the default — stage 5-b ships this brake armed
/// only when an operator explicitly configures it. Equity is checked before
/// margin because equity going through the floor is the more fundamental
/// condition. Non-finite observations never trip a stop: a bad snapshot is a
/// data problem, and the caller already warns about unparseable balances.
pub fn account_floor_breach(
    equity: f64,
    available: f64,
    equity_floor: f64,
    margin_floor: f64,
) -> Option<(AccountFloorBreach, f64, f64)> {
    if equity_floor.is_finite() && equity_floor > 0.0 && equity.is_finite() && equity < equity_floor
    {
        return Some((AccountFloorBreach::Equity, equity, equity_floor));
    }
    if margin_floor.is_finite()
        && margin_floor > 0.0
        && available.is_finite()
        && available < margin_floor
    {
        return Some((AccountFloorBreach::Margin, available, margin_floor));
    }
    None
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

    // Ladder is centered on the inventory-skewed price; the band/no-cross
    // guards below still reference the true mark and touch.
    let center = skew_center_with(cfg, nonlinear_skew, mark, position);

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
) -> Vec<Action> {
    // Band/no-cross reference the true mark and touch; the anti-flicker anchor
    // uses the inventory-skewed center.
    let band_lo = mark * (1.0 - cfg.band_bps / 1e4);
    let band_hi = mark * (1.0 + cfg.band_bps / 1e4);
    let center = skew_center_with(cfg, nonlinear_skew, mark, position);

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
mod tests {
    use super::*;

    /// mark=100-friendly config: 2 price decimals, 4 qty decimals.
    fn cfg() -> MakerConfig {
        MakerConfig {
            spread_bps: 10.0,
            band_bps: 20.0,
            level_step_bps: 2.0,
            refresh_bps: 3.0,
            levels: 1,
            size: 0.01,
            max_position: 0.05,
            skew_bps: 0.0,
            price_decimals: 2,
            qty_decimals: 4,
            min_order_qty: 0.001,
        }
    }

    fn desired(
        cfg: &MakerConfig,
        mark: f64,
        best_bid: Option<f64>,
        best_ask: Option<f64>,
        position: f64,
    ) -> Vec<DesiredQuote> {
        compute_desired_quotes(
            cfg,
            mark,
            best_bid,
            best_ask,
            position,
            SizeSkewDecision::INACTIVE,
            NonlinearSkewConfig::default(),
            GuardDecision::INACTIVE,
        )
    }

    fn resting(side: OrderSide, level: u32, price: f64, ref_center: f64) -> RestingQuote {
        RestingQuote {
            order_id: Some("1".into()),
            side,
            level,
            price,
            qty: 0.01,
            ref_center,
            placed_at_cycle: 0,
        }
    }

    fn find(quotes: &[DesiredQuote], side: OrderSide, level: u32) -> &DesiredQuote {
        quotes
            .iter()
            .find(|q| q.side == side && q.level == level)
            .expect("quote missing")
    }

    #[test]
    fn inventory_exit_plan_is_explicit_capped_and_reducing() {
        assert_eq!(
            inventory_exit_plan(0.04, 0.05, 80.0, 0.015),
            Some(InventoryExit {
                side: OrderSide::Sell,
                qty: 0.015,
                kind: ExitKind::InventoryTrim,
            })
        );
        assert_eq!(
            inventory_exit_plan(-0.05, 0.05, 80.0, 0.10),
            Some(InventoryExit {
                side: OrderSide::Buy,
                qty: 0.05,
                kind: ExitKind::InventoryTrim,
            })
        );
        assert_eq!(inventory_exit_plan(0.039, 0.05, 80.0, 0.01), None);
        assert_eq!(inventory_exit_plan(0.05, 0.05, 0.0, 0.01), None);
        assert_eq!(inventory_exit_plan(0.05, 0.05, 101.0, 0.01), None);
    }

    #[test]
    fn requotes_on_touch_move_without_creating_crossed_quote() {
        // Cycle 0: quote a calm book and let the places rest.
        let desired = desired(&cfg(), 100.0, Some(99.99), Some(100.01), 0.0);
        let actions = reconcile(
            &cfg(),
            100.0,
            0.0,
            Some(99.99),
            Some(100.01),
            &desired,
            &[],
            0,
            Default::default(),
        );
        assert!(actions
            .iter()
            .any(|action| matches!(action, Action::Place(_))));
        let resting: Vec<RestingQuote> = actions
            .iter()
            .filter_map(|action| match action {
                Action::Place(quote) => Some(RestingQuote {
                    order_id: None,
                    side: quote.side,
                    level: quote.level,
                    price: quote.price,
                    qty: quote.qty,
                    ref_center: skew_center(&cfg(), 100.0, 0.0),
                    placed_at_cycle: 0,
                }),
                _ => None,
            })
            .collect();

        // Cycle 1: the touch drops below the resting sell; the stale quote is
        // cancelled as WouldCross and no replacement crosses the new touch.
        let (bid, ask) = (99.88, 99.90);
        let desired = self::desired(&cfg(), 100.0, Some(bid), Some(ask), 0.0);
        let actions = reconcile(
            &cfg(),
            100.0,
            0.0,
            Some(bid),
            Some(ask),
            &desired,
            &resting,
            1,
            Default::default(),
        );
        assert!(actions.iter().any(|action| matches!(
            action,
            Action::Cancel {
                reason: CancelReason::WouldCross,
                ..
            }
        )));
        for action in &actions {
            if let Action::Place(quote) = action {
                match quote.side {
                    OrderSide::Buy => assert!(quote.price < ask),
                    OrderSide::Sell => assert!(quote.price > bid),
                }
            }
        }
    }

    // 1. Basic two-sided quoting.
    #[test]
    fn basic_two_sided() {
        let quotes = desired(&cfg(), 100.0, Some(99.99), Some(100.01), 0.0);
        assert_eq!(quotes.len(), 2);
        assert_eq!(find(&quotes, OrderSide::Buy, 0).price, 99.90);
        assert_eq!(find(&quotes, OrderSide::Sell, 0).price, 100.10);
        assert_eq!(find(&quotes, OrderSide::Buy, 0).qty, 0.01);
    }

    // 2. Spread wider than band: clamp to band edges, not dropped.
    #[test]
    fn band_clamp() {
        let mut c = cfg();
        c.spread_bps = 30.0; // > band 20
        let quotes = desired(&c, 100.0, Some(99.5), Some(100.5), 0.0);
        assert_eq!(find(&quotes, OrderSide::Buy, 0).price, 99.80);
        assert_eq!(find(&quotes, OrderSide::Sell, 0).price, 100.20);
    }

    // 3. Directional tick rounding: buy floors, sell ceils.
    #[test]
    fn tick_rounding_directional() {
        let mut c = cfg();
        c.price_decimals = 1;
        c.spread_bps = 5.0;
        // mark=100.03: raw buy = 99.979985 -> floor(1dp) 99.9
        //              raw sell = 100.080015 -> ceil(1dp) 100.1
        let quotes = desired(&c, 100.03, None, None, 0.0);
        assert_eq!(find(&quotes, OrderSide::Buy, 0).price, 99.9);
        assert_eq!(find(&quotes, OrderSide::Sell, 0).price, 100.1);
        assert_eq!(format_decimals(99.9, 1), "99.9");
    }

    // 4. Rounding that exits the band is nudged back inside.
    #[test]
    fn rounding_reenters_band() {
        let mut c = cfg();
        c.price_decimals = 0; // whole-number ticks
        c.spread_bps = 20.0; // == band: raw buy exactly at band edge 99.8
        c.band_bps = 20.0;
        // raw buy = 99.8, floor(0dp) = 99 < band_lo 99.8 -> nudge +1 tick = 100
        // (floor(99.8+1) = 100)... still >= band_lo, inside band.
        let quotes = desired(&c, 100.0, None, None, 0.0);
        let buy = find(&quotes, OrderSide::Buy, 0);
        assert!(buy.price >= 99.8, "price {} left the band", buy.price);
        assert_eq!(buy.price, 100.0);
    }

    // 5. No-cross clamp on both sides.
    #[test]
    fn no_cross_clamp() {
        // Best ask (99.85) sits BELOW our raw buy (99.90): buy must clamp
        // down to ask - tick.
        let quotes = desired(&cfg(), 100.0, Some(99.83), Some(99.85), 0.0);
        assert_eq!(quotes.len(), 2, "{quotes:?}");
        let buy = find(&quotes, OrderSide::Buy, 0);
        let sell = find(&quotes, OrderSide::Sell, 0);
        // buy clamped to ask - tick = 99.84
        assert_eq!(buy.price, 99.84);
        // sell raw 100.10 already > bid + tick; unchanged
        assert_eq!(sell.price, 100.10);

        // Symmetric: bid above our raw sell forces sell up to bid + tick.
        let quotes = desired(&cfg(), 100.0, Some(100.15), Some(100.20), 0.0);
        let sell = find(&quotes, OrderSide::Sell, 0);
        assert_eq!(sell.price, 100.16);
    }

    #[test]
    fn drops_side_when_band_and_no_cross_have_no_feasible_tick() {
        let quotes = desired(&cfg(), 100.0, Some(99.78), Some(99.79), 0.0);
        assert!(quotes.iter().all(|quote| quote.side == OrderSide::Sell));

        let quotes = desired(&cfg(), 100.0, Some(100.21), Some(100.22), 0.0);
        assert!(quotes.iter().all(|quote| quote.side == OrderSide::Buy));
    }

    #[test]
    fn invalid_market_values_produce_no_quotes() {
        assert!(desired(&cfg(), f64::NAN, None, None, 0.0).is_empty());
        assert!(desired(&cfg(), 100.0, Some(f64::INFINITY), None, 0.0).is_empty());
        assert!(desired(&cfg(), 100.0, None, Some(0.0), 0.0).is_empty());
    }

    // 6. Size below min_order_qty -> no quotes at all.
    #[test]
    fn min_qty_rejection() {
        let mut c = cfg();
        c.size = 0.00001; // rounds to 0.0000 at 4dp -> below min 0.001
        let quotes = desired(&c, 100.0, None, None, 0.0);
        assert!(quotes.is_empty());
    }

    // 7. Max-position suppression, both directions.
    #[test]
    fn max_position_suppresses_buy() {
        let quotes = desired(&cfg(), 100.0, None, None, 0.05);
        assert!(quotes.iter().all(|q| q.side == OrderSide::Sell));
        assert_eq!(quotes.len(), 1);
    }

    #[test]
    fn max_position_suppresses_sell() {
        let quotes = desired(&cfg(), 100.0, None, None, -0.05);
        assert!(quotes.iter().all(|q| q.side == OrderSide::Buy));
        assert_eq!(quotes.len(), 1);
    }

    // 8. Anti-flicker: drift within refresh threshold -> Hold.
    #[test]
    fn reconcile_hold_within_refresh() {
        let c = cfg();
        let mark = 100.02; // 2 bps from ref 100.0, refresh = 3
        let desired = desired(&c, mark, None, None, 0.0);
        let rest = vec![
            resting(OrderSide::Buy, 0, 99.90, 100.0),
            resting(OrderSide::Sell, 0, 100.10, 100.0),
        ];
        let actions = reconcile(
            &c,
            mark,
            0.0,
            None,
            None,
            &desired,
            &rest,
            7,
            Default::default(),
        );
        assert!(
            actions.iter().all(|a| matches!(a, Action::Hold { .. })),
            "{actions:?}"
        );
        assert_eq!(actions.len(), 2);
        if let Action::Hold { age_cycles, .. } = &actions[0] {
            assert_eq!(*age_cycles, 7);
        }
    }

    // 9. Drift beyond refresh -> Cancel(mark_moved) + Place, cancel first.
    #[test]
    fn reconcile_requote_beyond_refresh() {
        let c = cfg();
        let mark = 100.05; // 5 bps > refresh 3
        let desired = desired(&c, mark, None, None, 0.0);
        let rest = vec![resting(OrderSide::Buy, 0, 99.90, 100.0)];
        let actions = reconcile(
            &c,
            mark,
            0.0,
            None,
            None,
            &desired,
            &rest,
            1,
            Default::default(),
        );
        // Expect: cancel(buy, mark_moved), then places for buy+sell.
        assert!(matches!(
            actions[0],
            Action::Cancel {
                reason: CancelReason::MarkMovedBeyondRefresh,
                ..
            }
        ));
        let cancel_idx = 0;
        let place_idx = actions
            .iter()
            .position(|a| matches!(a, Action::Place(_)))
            .unwrap();
        assert!(cancel_idx < place_idx);
    }

    // 10. Outside band takes precedence over refresh drift.
    #[test]
    fn reconcile_cancel_outside_band_precedence() {
        let c = cfg();
        // Mark gapped 30 bps: resting buy at 99.90 with ref 100.0 is now
        // outside band [100.10, 100.50] around mark 100.30.
        let mark = 100.30;
        let desired = desired(&c, mark, None, None, 0.0);
        let rest = vec![resting(OrderSide::Buy, 0, 99.90, 100.0)];
        let actions = reconcile(
            &c,
            mark,
            0.0,
            None,
            None,
            &desired,
            &rest,
            1,
            Default::default(),
        );
        assert!(
            matches!(
                actions[0],
                Action::Cancel {
                    reason: CancelReason::OutsideBand,
                    ..
                }
            ),
            "{actions:?}"
        );
    }

    // 11. Touch moved through a resting quote -> WouldCross.
    #[test]
    fn reconcile_cancel_would_cross() {
        let c = cfg();
        let mark = 100.01; // tiny drift, within refresh
        let desired = desired(&c, mark, Some(100.12), Some(100.14), 0.0);
        // Resting sell at 100.10 now BELOW best bid 100.12 -> crossed.
        let rest = vec![resting(OrderSide::Sell, 0, 100.10, 100.0)];
        let actions = reconcile(
            &c,
            mark,
            0.0,
            Some(100.12),
            Some(100.14),
            &desired,
            &rest,
            1,
            Default::default(),
        );
        assert!(
            matches!(
                actions[0],
                Action::Cancel {
                    reason: CancelReason::WouldCross,
                    ..
                }
            ),
            "{actions:?}"
        );
    }

    #[test]
    fn detects_resting_quotes_that_would_cross_a_new_touch() {
        let quotes = vec![resting(OrderSide::Buy, 0, 99.95, 100.0)];
        assert!(resting_quotes_would_cross(
            &quotes,
            Some(99.90),
            Some(99.95)
        ));
        assert!(!resting_quotes_would_cross(
            &quotes,
            Some(99.90),
            Some(99.96)
        ));
    }

    // 12. Level removed from config -> Stale.
    #[test]
    fn reconcile_stale_level() {
        let c = cfg(); // levels = 1 -> only level 0 desired
        let mark = 100.0;
        let desired = desired(&c, mark, None, None, 0.0);
        let rest = vec![
            resting(OrderSide::Buy, 0, 99.90, 100.0),
            resting(OrderSide::Buy, 1, 99.88, 100.0), // stale level
        ];
        let actions = reconcile(
            &c,
            mark,
            0.0,
            None,
            None,
            &desired,
            &rest,
            1,
            Default::default(),
        );
        let stale: Vec<_> = actions
            .iter()
            .filter(|a| {
                matches!(
                    a,
                    Action::Cancel {
                        reason: CancelReason::Stale,
                        level: 1,
                        ..
                    }
                )
            })
            .collect();
        assert_eq!(stale.len(), 1, "{actions:?}");
    }

    // 13. Multi-level ladder + duplicate collapse.
    #[test]
    fn multi_level_ladder() {
        let mut c = cfg();
        c.levels = 3;
        let quotes = desired(&c, 100.0, None, None, 0.0);
        // Buys descending: 99.90, 99.88, 99.86; sells ascending mirrored.
        assert_eq!(find(&quotes, OrderSide::Buy, 0).price, 99.90);
        assert_eq!(find(&quotes, OrderSide::Buy, 1).price, 99.88);
        assert_eq!(find(&quotes, OrderSide::Buy, 2).price, 99.86);
        assert_eq!(find(&quotes, OrderSide::Sell, 2).price, 100.14);
        assert_eq!(quotes.len(), 6);

        // Ladder flattened by band: spread 18, step 2, band 20 -> levels 1+
        // clamp to the band edge and duplicates collapse.
        let mut c2 = cfg();
        c2.levels = 3;
        c2.spread_bps = 18.0;
        let quotes = desired(&c2, 100.0, None, None, 0.0);
        let buys: Vec<_> = quotes.iter().filter(|q| q.side == OrderSide::Buy).collect();
        assert_eq!(buys.len(), 2, "{buys:?}"); // 99.82, then 99.80 (L1) and L2 dup dropped
        assert_eq!(buys[1].price, 99.80);
    }

    // 14. Helper edge cases.
    #[test]
    fn helper_edge_cases() {
        assert_eq!(bps_diff(100.0, 0.0), 0.0);
        assert!((bps_diff(100.05, 100.0) - 5.0).abs() < 1e-9); // ~5 bps
        assert!((bps_diff(99.95, 100.0) - 5.0).abs() < 1e-9); // symmetric
        assert_eq!(round_to_decimals(1.23456, 0), 1.0);
        assert_eq!(round_to_decimals(-1.235, 2), -1.24);
        assert_eq!(floor_to_decimals(99.90, 2), 99.90); // representation artifact guard
        assert_eq!(ceil_to_decimals(100.10, 2), 100.10);
        assert_eq!(format_decimals(99.9, 2), "99.90");
        assert_eq!(format_decimals(0.0123, 4), "0.0123");
    }

    // 15. Mark/mid divergence guard helper.
    #[test]
    fn mark_mid_divergence() {
        // mid = 100.0 == mark -> no divergence
        assert_eq!(mark_mid_divergence_bps(100.0, 99.9, 100.1), 0.0);
        // mid = 100.25 vs mark 100.0 -> 25 bps
        assert!((mark_mid_divergence_bps(100.0, 100.2, 100.3) - 25.0).abs() < 1e-9);
        // symmetric below
        assert!((mark_mid_divergence_bps(100.0, 99.7, 99.8) - 25.0).abs() < 1e-9);
        // degenerate mark = 0 -> 0.0, no blowup
        assert_eq!(mark_mid_divergence_bps(0.0, 99.9, 100.1), 0.0);
    }

    // 23. Paper fill model: crossed touch fills, otherwise not.
    #[test]
    fn paper_fills_on_crossed_touch() {
        // Resting buy at 99.90: fills once offers reach down to it.
        assert!(!quote_crosses_touch(
            OrderSide::Buy,
            99.90,
            Some(99.80),
            Some(99.95)
        ));
        assert!(quote_crosses_touch(
            OrderSide::Buy,
            99.90,
            Some(99.80),
            Some(99.90)
        ));
        assert!(quote_crosses_touch(
            OrderSide::Buy,
            99.90,
            Some(99.80),
            Some(99.85)
        ));
        // Resting sell at 100.10: fills once bids reach up to it.
        assert!(!quote_crosses_touch(
            OrderSide::Sell,
            100.10,
            Some(100.05),
            Some(100.2)
        ));
        assert!(quote_crosses_touch(
            OrderSide::Sell,
            100.10,
            Some(100.10),
            Some(100.2)
        ));
        // Absent book side never fills.
        assert!(!quote_crosses_touch(OrderSide::Buy, 99.90, None, None));
        assert!(!quote_crosses_touch(OrderSide::Sell, 100.10, None, None));
    }

    // 24. Stats: spread capture, mark-to-market PnL, uptime.
    #[test]
    fn stats_pnl_and_capture() {
        let mut s = MakerStats::default();
        // Buy 1 @ 99.90 (mark 100) then sell 1 @ 100.10 (mark 100): a round
        // trip capturing 10 + 10 bps, net cash +0.20, flat position.
        s.record_fill(OrderSide::Buy, 99.90, 1.0, 100.0);
        s.record_fill(OrderSide::Sell, 100.10, 1.0, 100.0);
        assert_eq!(s.fills(), 2);
        assert!((s.filled_qty - 2.0).abs() < 1e-9);
        assert!((s.cash - 0.20).abs() < 1e-9);
        // Flat position -> PnL is just the captured cash.
        assert!((s.pnl(0.0, 100.0) - 0.20).abs() < 1e-9);
        // Each leg captured 10 bps -> avg 10.
        assert!((s.avg_spread_capture_bps() - 10.0).abs() < 1e-6);
    }

    #[test]
    fn stats_unrealized_inventory() {
        let mut s = MakerStats::default();
        // Buy 2 @ 100 (no edge), then mark rises to 101: unrealized +2.
        s.record_fill(OrderSide::Buy, 100.0, 2.0, 100.0);
        assert!((s.pnl(2.0, 101.0) - 2.0).abs() < 1e-9);
        assert!((s.pnl(2.0, 100.0)).abs() < 1e-9); // flat at entry mark
    }

    #[test]
    fn stats_adopted_inventory_starts_at_zero_for_long_and_short() {
        let long = MakerStats::with_inventory_baseline(0.13, 59.72);
        let short = MakerStats::with_inventory_baseline(-0.13, 59.72);
        assert!(long.pnl(0.13, 59.72).abs() < 1e-9);
        assert!(short.pnl(-0.13, 59.72).abs() < 1e-9);
        assert!((long.pnl(0.13, 60.72) - 0.13).abs() < 1e-9);
        assert!((short.pnl(-0.13, 60.72) + 0.13).abs() < 1e-9);
    }

    #[test]
    fn stats_adopted_inventory_and_new_fill_share_session_basis() {
        let mut stats = MakerStats::with_inventory_baseline(-0.2, 60.0);
        stats.record_fill(OrderSide::Buy, 59.5, 0.2, 59.5);
        assert!((stats.pnl(0.0, 59.5) - 0.1).abs() < 1e-9);
        assert_eq!(stats.fills(), 1);
    }

    #[test]
    fn stats_uptime_and_live_inference() {
        let mut s = MakerStats::default();
        s.end_cycle(0.0, true); // two-sided
        s.end_cycle(0.0, false); // one-sided
        assert_eq!(s.cycles, 2);
        assert!((s.uptime_pct() - 50.0).abs() < 1e-9);
        // Position movement alone must not fabricate a live fill: exact
        // maker fills are supplied by the venue ledger.
        let mut l = MakerStats::default();
        l.end_cycle(0.01, true);
        assert_eq!(l.fills(), 0);
        assert_eq!(l.buy_fills, 0);
        assert!((l.max_abs_position - 0.01).abs() < 1e-9);
    }

    // 16. skew_center helper: directional, zero cases.
    #[test]
    fn skew_center_directional() {
        let mut c = cfg();
        c.skew_bps = 10.0;
        // flat position -> center = mark
        assert_eq!(skew_center(&c, 100.0, 0.0), 100.0);
        // long (ratio +0.5) -> center down 99.95
        assert!((skew_center(&c, 100.0, 0.025) - 99.95).abs() < 1e-9);
        // short (ratio -0.5) -> center up 100.05
        assert!((skew_center(&c, 100.0, -0.025) - 100.05).abs() < 1e-9);
        // skew off -> center = mark regardless of position
        let c0 = cfg();
        assert_eq!(skew_center(&c0, 100.0, 0.05), 100.0);
    }

    // 17. Long inventory shifts the whole ladder down; reducing side (sell)
    // moves nearer mark, growing side (buy) further.
    #[test]
    fn skew_long_shifts_center_down() {
        let mut c = cfg();
        c.skew_bps = 10.0;
        // half-max long -> center 99.95; buy = 99.85, sell = 100.05
        let q = desired(&c, 100.0, None, None, 0.025);
        assert_eq!(find(&q, OrderSide::Buy, 0).price, 99.85);
        assert_eq!(find(&q, OrderSide::Sell, 0).price, 100.05);
        // both below the no-skew baseline (99.90 / 100.10)
        assert!(find(&q, OrderSide::Buy, 0).price < 99.90);
        assert!(find(&q, OrderSide::Sell, 0).price < 100.10);
    }

    // 18. Short inventory shifts up; reducing side (buy) nearer mark.
    #[test]
    fn skew_short_shifts_center_up() {
        let mut c = cfg();
        c.skew_bps = 10.0;
        // half-max short -> center 100.05; buy = 99.94, sell = 100.16
        let q = desired(&c, 100.0, None, None, -0.025);
        assert_eq!(find(&q, OrderSide::Buy, 0).price, 99.94);
        assert_eq!(find(&q, OrderSide::Sell, 0).price, 100.16);
        // buy moved nearer mark than the no-skew baseline 99.90
        assert!(find(&q, OrderSide::Buy, 0).price > 99.90);
    }

    // 19. skew_bps = 0 is a no-op regardless of position.
    #[test]
    fn skew_zero_is_noop() {
        let c = cfg(); // skew_bps = 0
        let base = desired(&c, 100.0, None, None, 0.0);
        let with_pos = desired(&c, 100.0, None, None, 0.025);
        assert_eq!(base, with_pos);
    }

    #[test]
    fn exposure_cap_limits_all_same_side_fills() {
        let mut c = cfg();
        c.levels = 3;
        c.size = 0.02;
        c.max_position = 0.05;
        let raw = desired(&c, 100.0, None, None, 0.03);
        let capped = cap_desired_exposure(&c, 0.03, &raw, &[]);

        // At +0.03, only one additional 0.02 buy can be exposed. All three
        // sells remain safe: even if they all fill, the position is -0.03.
        assert_eq!(
            capped
                .iter()
                .filter(|quote| quote.side == OrderSide::Buy)
                .count(),
            1
        );
        assert_eq!(
            capped
                .iter()
                .filter(|quote| quote.side == OrderSide::Sell)
                .count(),
            3
        );
        let buy_qty: f64 = capped
            .iter()
            .filter(|quote| quote.side == OrderSide::Buy)
            .map(|quote| quote.qty)
            .sum();
        assert!(0.03 + buy_qty <= c.max_position + 1e-9);
    }

    #[test]
    fn exposure_cap_reserves_pending_slot_before_new_levels() {
        let mut c = cfg();
        c.levels = 3;
        c.size = 0.02;
        c.max_position = 0.05;
        let raw = desired(&c, 100.0, None, None, 0.03);
        let capped = cap_desired_exposure(&c, 0.03, &raw, &[(OrderSide::Buy, 2)]);

        // The in-flight outer bid gets the only 0.02 buy budget. A later
        // reconcile cannot place level 0 in addition while level 2 is still
        // awaiting exchange visibility.
        assert!(capped
            .iter()
            .any(|quote| quote.side == OrderSide::Buy && quote.level == 2));
        assert!(!capped
            .iter()
            .any(|quote| quote.side == OrderSide::Buy && quote.level == 0));
    }

    // 20. Inventory ratio saturates at ±1 past max_position.
    #[test]
    fn skew_clamps_at_full_inventory() {
        let mut c = cfg();
        c.skew_bps = 10.0;
        // 2x max short: growing side (sell) suppressed, only buy remains;
        // ratio clamps to -1 -> center 100.10 -> buy 99.99 (NOT the ratio=-2
        // value 100.09).
        let q = desired(&c, 100.0, None, None, -0.10);
        assert!(q.iter().all(|d| d.side == OrderSide::Buy));
        assert_eq!(find(&q, OrderSide::Buy, 0).price, 99.99);
    }

    // 21. Large skew still respects the band and no-cross guards.
    #[test]
    fn skew_still_respects_band_and_no_cross() {
        let mut c = cfg();
        c.skew_bps = 100.0; // would push the sell far below mark
        let (bid, ask) = (99.90, 100.00);
        // full long: buy suppressed; sell pulled down hard but held above the
        // band floor and one tick above the bid.
        let q = desired(&c, 100.0, Some(bid), Some(ask), 0.05);
        assert_eq!(q.len(), 1, "{q:?}");
        let sell = find(&q, OrderSide::Sell, 0).price;
        assert!(sell >= 99.80 - 1e-9, "sell {sell} below band floor");
        assert!(sell > bid, "sell {sell} crosses bid {bid}");
    }

    // 22. Inventory skew alone (mark unchanged) triggers a re-quote once the
    // center drifts past refresh_bps; stays held within it.
    #[test]
    fn reconcile_skew_requote() {
        let mut c = cfg();
        c.skew_bps = 10.0;
        let mark = 100.0;
        // Resting sell placed when flat (ref_center = 100.0).
        // Long 0.025 -> center 99.95, drift 5bps > refresh 3 -> re-quote.
        let pos = 0.025;
        let desired = desired(&c, mark, None, None, pos);
        let rest = vec![resting(OrderSide::Sell, 0, 100.10, 100.0)];
        let actions = reconcile(
            &c,
            mark,
            pos,
            None,
            None,
            &desired,
            &rest,
            1,
            Default::default(),
        );
        assert!(actions.iter().any(|a| matches!(
            a,
            Action::Cancel {
                reason: CancelReason::MarkMovedBeyondRefresh,
                ..
            }
        )));

        // Smaller long 0.01 -> center 99.98, drift 2bps < refresh -> hold.
        let pos2 = 0.01;
        let desired2 = self::desired(&c, mark, None, None, pos2);
        let rest2 = vec![resting(OrderSide::Sell, 0, 100.10, 100.0)];
        let actions2 = reconcile(
            &c,
            mark,
            pos2,
            None,
            None,
            &desired2,
            &rest2,
            1,
            Default::default(),
        );
        assert!(actions2.iter().any(|a| matches!(a, Action::Hold { .. })));
        assert!(!actions2.iter().any(|a| matches!(a, Action::Cancel { .. })));
    }

    // 25. Vol breaker: disabled is a no-op.
    #[test]
    fn vol_breaker_disabled() {
        let mut b = VolBreaker::new(5, 0.0);
        assert!(!b.enabled());
        assert!(!b.observe(100.0));
        assert!(!b.observe(200.0)); // huge move, but disabled -> never halts
        assert!(!b.halted());
    }

    // 26. Vol breaker: trips on a fast move, resumes with hysteresis.
    #[test]
    fn vol_breaker_trip_and_rearm() {
        // window 4, pause 30bps -> rearm 15bps.
        let mut b = VolBreaker::new(4, 30.0);
        assert!(!b.observe(100.0));
        assert!(!b.observe(100.1)); // 10bps range, calm
        assert!(!b.halted());
        // Jump to 100.4: range now (100.4-100.0)/100 = 40bps >= 30 -> halt.
        assert!(b.observe(100.4));
        assert!(b.halted());
        // Still elevated while the low sample (100.0) is in the window.
        assert!(b.observe(100.4)); // range 40bps, still halted
                                   // Push new samples near 100.4 so old lows roll out; range collapses.
        b.observe(100.4);
        let halted = b.observe(100.4); // window now all ~100.4 -> range ~0 < 15
        assert!(!halted);
        assert!(!b.halted());
    }

    // 27. Vol breaker: hysteresis holds between rearm and pause.
    #[test]
    fn vol_breaker_hysteresis_band() {
        let mut b = VolBreaker::new(3, 40.0); // rearm 20bps
        b.observe(100.0);
        assert!(b.observe(100.5)); // 50bps -> halt
                                   // Range drifts to ~25bps (between rearm 20 and pause 40): stays halted.
        b.observe(100.25);
        let halted = b.observe(100.25); // window {100.5,100.25,100.25} range 25bps
        assert!(halted, "should stay halted in the hysteresis band");
    }

    #[test]
    fn preflight_skips_crossed_divergent_or_incomplete_live_books() {
        let mut breaker = VolBreaker::new(3, 0.0);
        let crossed = preflight_cycle(
            &mut breaker,
            MarketSnapshot {
                mark: 100.0,
                best_bid: Some(100.1),
                best_ask: Some(100.0),
            },
            10.0,
            true,
        );
        assert_eq!(crossed.skip, Some(CycleSkip::CrossedBook));

        let divergent = preflight_cycle(
            &mut breaker,
            MarketSnapshot {
                mark: 100.0,
                best_bid: Some(90.0),
                best_ask: Some(90.1),
            },
            10.0,
            true,
        );
        assert!(matches!(
            divergent.skip,
            Some(CycleSkip::MarkMidDivergence { divergence_bps }) if divergence_bps > 10.0
        ));

        let incomplete = preflight_cycle(
            &mut breaker,
            MarketSnapshot {
                mark: 100.0,
                best_bid: Some(99.9),
                best_ask: None,
            },
            10.0,
            true,
        );
        assert_eq!(incomplete.skip, Some(CycleSkip::MissingTouch));
    }

    #[test]
    fn cycle_plan_pulls_quotes_for_exit_and_suppresses_exit_during_vol_halt() {
        let c = cfg();
        let resting = vec![resting(OrderSide::Buy, 0, 99.90, 100.0)];
        let input = CycleInput {
            cycle: 1,
            market: MarketSnapshot {
                mark: 100.0,
                best_bid: Some(99.8),
                best_ask: Some(100.2),
            },
            position: c.max_position,
            resting: &resting,
            pending_slots: &[],
            market_data_mode: MarketDataMode::Active,
            active_exit_enabled: true,
            inventory_exit_pct: 80.0,
            inventory_exit_qty: 0.01,
            size_skew: Default::default(),
            nonlinear_skew: Default::default(),
            guard: Default::default(),
            wind_down: false,
            qty_tolerance: 0.0005,
        };

        let exit_plan = plan_cycle(&c, input, false);
        assert_eq!(
            exit_plan.requested_inventory_exit,
            Some(InventoryExit {
                side: OrderSide::Sell,
                qty: 0.01,
                kind: ExitKind::InventoryTrim,
            })
        );
        assert_eq!(exit_plan.inventory_exit, exit_plan.requested_inventory_exit);
        assert!(exit_plan
            .actions
            .iter()
            .any(|action| matches!(action, Action::Cancel { .. })));
        assert!(!exit_plan
            .actions
            .iter()
            .any(|action| matches!(action, Action::Place(_))));

        assert_eq!(exit_plan.exit_suppression, None);

        let halted_plan = plan_cycle(&c, input, true);
        assert_eq!(
            halted_plan.requested_inventory_exit,
            exit_plan.requested_inventory_exit
        );
        assert_eq!(halted_plan.inventory_exit, None);
        assert_eq!(
            halted_plan.exit_suppression,
            Some(SuppressedExit {
                kind: ExitKind::InventoryTrim,
                reason: ExitSuppression::VolatilityHalt,
            })
        );
    }

    /// Stage 5-b D1: a volatility halt suppresses BOTH exit policies, and says
    /// so in a typed field. Wind-down is the one that used to be easy to assume
    /// still ran: an arm ending mid-halt keeps its inventory until the halt
    /// clears, and that trade-off is now pinned by a test, not by a comment.
    #[test]
    fn vol_halt_suppresses_wind_down_exit_with_typed_reason() {
        let c = cfg();
        let input = CycleInput {
            cycle: 7,
            market: MarketSnapshot {
                mark: 100.0,
                best_bid: Some(99.8),
                best_ask: Some(100.2),
            },
            position: 0.02,
            resting: &[],
            pending_slots: &[],
            market_data_mode: MarketDataMode::Active,
            // Wind-down ignores the configured trigger entirely.
            active_exit_enabled: false,
            inventory_exit_pct: 0.0,
            inventory_exit_qty: 0.0,
            size_skew: Default::default(),
            nonlinear_skew: Default::default(),
            guard: Default::default(),
            wind_down: true,
            qty_tolerance: 0.0005,
        };

        let running = plan_cycle(&c, input, false);
        assert_eq!(
            running.inventory_exit,
            Some(InventoryExit {
                side: OrderSide::Sell,
                qty: 0.02,
                kind: ExitKind::WindDown,
            })
        );
        assert_eq!(running.exit_suppression, None);

        let halted = plan_cycle(&c, input, true);
        assert_eq!(halted.inventory_exit, None);
        assert_eq!(
            halted.exit_suppression,
            Some(SuppressedExit {
                kind: ExitKind::WindDown,
                reason: ExitSuppression::VolatilityHalt,
            })
        );
        // No emergency execution appears in place of the suppressed exit.
        assert!(!halted
            .actions
            .iter()
            .any(|action| matches!(action, Action::Place(_))));
    }

    /// Inactive market data suppresses the exit too, and outranks the halt as
    /// the reported reason: without a trusted price the halt verdict itself is
    /// computed from stale marks.
    #[test]
    fn inactive_market_data_suppresses_exit_and_outranks_halt() {
        let c = cfg();
        let mk = |mode, wind_down| CycleInput {
            cycle: 9,
            market: MarketSnapshot {
                mark: 100.0,
                best_bid: Some(99.8),
                best_ask: Some(100.2),
            },
            position: c.max_position,
            resting: &[],
            pending_slots: &[],
            market_data_mode: mode,
            active_exit_enabled: true,
            inventory_exit_pct: 80.0,
            inventory_exit_qty: 0.01,
            size_skew: Default::default(),
            nonlinear_skew: Default::default(),
            guard: Default::default(),
            wind_down,
            qty_tolerance: 0.0005,
        };

        for wind_down in [false, true] {
            let expected_kind = if wind_down {
                ExitKind::WindDown
            } else {
                ExitKind::InventoryTrim
            };
            for halted in [false, true] {
                let plan = plan_cycle(&c, mk(MarketDataMode::Paused, wind_down), halted);
                // Nothing is even requested while the feed is untrusted, so
                // the caller's exit tracking stays untouched.
                assert_eq!(plan.requested_inventory_exit, None);
                assert_eq!(plan.inventory_exit, None);
                assert_eq!(
                    plan.exit_suppression,
                    Some(SuppressedExit {
                        kind: expected_kind,
                        reason: ExitSuppression::MarketDataInactive,
                    }),
                    "wind_down={wind_down} halted={halted}"
                );
            }
        }
    }

    /// Stage 5-b D2: the account hard floors are off by default, equity is
    /// checked before margin, and a bad snapshot never trips a stop.
    #[test]
    fn account_floor_breach_is_opt_in_and_ordered() {
        // Default (0/0) can never fire, whatever the balances look like.
        assert_eq!(account_floor_breach(0.0, 0.0, 0.0, 0.0), None);
        assert_eq!(account_floor_breach(-50.0, -50.0, 0.0, 0.0), None);

        assert_eq!(
            account_floor_breach(99.0, 500.0, 100.0, 0.0),
            Some((AccountFloorBreach::Equity, 99.0, 100.0))
        );
        // Exactly at the floor is not a breach.
        assert_eq!(account_floor_breach(100.0, 500.0, 100.0, 0.0), None);
        assert_eq!(
            account_floor_breach(500.0, 40.0, 0.0, 50.0),
            Some((AccountFloorBreach::Margin, 40.0, 50.0))
        );
        // Both breached: equity is the more fundamental condition and wins.
        assert_eq!(
            account_floor_breach(99.0, 40.0, 100.0, 50.0),
            Some((AccountFloorBreach::Equity, 99.0, 100.0))
        );
        // Unparseable/absent balances arrive as NaN and must not stop a run.
        assert_eq!(account_floor_breach(f64::NAN, f64::NAN, 100.0, 50.0), None);
        assert_eq!(
            account_floor_breach(f64::NAN, 40.0, 100.0, 50.0),
            Some((AccountFloorBreach::Margin, 40.0, 50.0))
        );
    }

    /// The labels are part of the evidence contract: run manifests and
    /// dashboards key off these strings.
    #[test]
    fn exit_and_floor_labels_are_stable() {
        assert_eq!(ExitKind::InventoryTrim.as_str(), "inventory_trim");
        assert_eq!(ExitKind::WindDown.as_str(), "wind_down");
        assert_eq!(ExitSuppression::VolatilityHalt.as_str(), "volatility_halt");
        assert_eq!(
            ExitSuppression::MarketDataInactive.as_str(),
            "market_data_inactive"
        );
        assert_eq!(AccountFloorBreach::Equity.as_str(), "equity");
        assert_eq!(AccountFloorBreach::Margin.as_str(), "margin");
    }

    #[test]
    fn cycle_plan_reserves_delayed_places_and_caps_directional_exposure() {
        let mut c = cfg();
        c.levels = 2;
        c.max_position = 0.015;
        let pending_slots = [(OrderSide::Buy, 0)];
        let plan = plan_cycle(
            &c,
            CycleInput {
                cycle: 4,
                market: MarketSnapshot {
                    mark: 100.0,
                    best_bid: Some(99.9),
                    best_ask: Some(100.1),
                },
                // The pending 0.01 buy already reserves more than the 0.005
                // remaining long-inventory budget.
                position: 0.01,
                resting: &[],
                pending_slots: &pending_slots,
                market_data_mode: MarketDataMode::Active,
                active_exit_enabled: false,
                inventory_exit_pct: 0.0,
                inventory_exit_qty: 0.0,
                size_skew: Default::default(),
                nonlinear_skew: Default::default(),
                guard: Default::default(),
                wind_down: false,
                qty_tolerance: 0.0005,
            },
            false,
        );

        let buy_places = plan
            .actions
            .iter()
            .filter_map(|action| match action {
                Action::Place(quote) if quote.side == OrderSide::Buy => Some(quote),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(buy_places.is_empty());
    }

    #[test]
    fn paused_market_data_cancels_without_placing_or_exiting() {
        let c = cfg();
        let resting = vec![
            resting(OrderSide::Buy, 0, 99.9, 100.0),
            resting(OrderSide::Sell, 0, 100.1, 100.0),
        ];
        let plan = plan_cycle(
            &c,
            CycleInput {
                cycle: 5,
                market: MarketSnapshot {
                    mark: 100.0,
                    best_bid: Some(99.9),
                    best_ask: Some(100.1),
                },
                position: c.max_position,
                resting: &resting,
                pending_slots: &[],
                market_data_mode: MarketDataMode::Paused,
                active_exit_enabled: true,
                inventory_exit_pct: 80.0,
                inventory_exit_qty: 0.01,
                size_skew: Default::default(),
                nonlinear_skew: Default::default(),
                guard: Default::default(),
                wind_down: false,
                qty_tolerance: 0.0005,
            },
            false,
        );

        assert_eq!(plan.requested_inventory_exit, None);
        assert_eq!(plan.inventory_exit, None);
        assert_eq!(
            plan.actions
                .iter()
                .filter(|action| matches!(action, Action::Cancel { .. }))
                .count(),
            resting.len()
        );
        assert!(!plan
            .actions
            .iter()
            .any(|action| matches!(action, Action::Place(_))));
    }

    #[test]
    fn inactive_size_skew_is_exactly_plan_equivalent_across_state_grid() {
        let mut c = cfg();
        c.levels = 2;
        c.max_position = 0.05;
        let resting_sets = [
            Vec::new(),
            vec![
                resting(OrderSide::Buy, 0, 99.9, 100.0),
                resting(OrderSide::Sell, 0, 100.1, 100.0),
            ],
        ];
        let pending_sets = [Vec::new(), vec![(OrderSide::Buy, 0), (OrderSide::Sell, 1)]];
        let inactive = SizeSkewDecision {
            enabled: true,
            active: false,
            add_side: Some(OrderSide::Buy),
            inventory_ratio: 0.99,
            add_qty: Some(c.min_order_qty),
        };

        for position in [-0.025, 0.0, 0.025] {
            for resting in &resting_sets {
                for pending_slots in &pending_sets {
                    let input = CycleInput {
                        cycle: 6,
                        market: MarketSnapshot {
                            mark: 100.0,
                            best_bid: Some(99.99),
                            best_ask: Some(100.01),
                        },
                        position,
                        resting,
                        pending_slots,
                        market_data_mode: MarketDataMode::Active,
                        active_exit_enabled: false,
                        inventory_exit_pct: 0.0,
                        inventory_exit_qty: 0.0,
                        size_skew: SizeSkewDecision::default(),
                        nonlinear_skew: Default::default(),
                        guard: Default::default(),
                        wind_down: false,
                        qty_tolerance: 0.0005,
                    };
                    let default_plan = plan_cycle(&c, input, false);
                    let inactive_plan = plan_cycle(
                        &c,
                        CycleInput {
                            size_skew: inactive,
                            nonlinear_skew: Default::default(),
                            guard: Default::default(),
                            ..input
                        },
                        false,
                    );
                    assert_eq!(inactive_plan, default_plan);
                }
            }
        }
    }

    #[test]
    fn disabled_nonlinear_and_inactive_guard_are_plan_equivalent_across_state_grid() {
        let mut c = cfg();
        c.levels = 2;
        c.max_position = 0.05;
        c.skew_bps = 10.0;
        let resting_sets = [
            Vec::new(),
            vec![
                resting(OrderSide::Buy, 0, 99.9, 100.0),
                resting(OrderSide::Sell, 0, 100.1, 100.0),
            ],
        ];
        // Disabled nonlinear config with aggressive non-default parameters, and
        // an enabled-but-inactive guard: neither may perturb a single action.
        let disabled_nonlinear = NonlinearSkewConfig {
            enabled: false,
            boost: 7.0,
            cap_bps: 9.0,
        };
        let inactive_guard = GuardDecision {
            enabled: true,
            active: false,
            endangered: None,
            divergence_bps: Some(2.0),
        };

        for position in [-0.04, -0.025, 0.0, 0.025, 0.04] {
            for resting in &resting_sets {
                let input = CycleInput {
                    cycle: 6,
                    market: MarketSnapshot {
                        mark: 100.0,
                        best_bid: Some(99.99),
                        best_ask: Some(100.01),
                    },
                    position,
                    resting,
                    pending_slots: &[],
                    market_data_mode: MarketDataMode::Active,
                    active_exit_enabled: false,
                    inventory_exit_pct: 0.0,
                    inventory_exit_qty: 0.0,
                    size_skew: SizeSkewDecision::default(),
                    nonlinear_skew: Default::default(),
                    guard: Default::default(),
                    wind_down: false,
                    qty_tolerance: 0.0005,
                };
                let default_plan = plan_cycle(&c, input, false);
                let candidate_plan = plan_cycle(
                    &c,
                    CycleInput {
                        nonlinear_skew: disabled_nonlinear,
                        guard: inactive_guard,
                        ..input
                    },
                    false,
                );
                assert_eq!(candidate_plan, default_plan);
            }
        }
    }

    #[test]
    fn nonlinear_skew_boost_one_with_high_cap_equals_linear() {
        let mut c = cfg();
        c.skew_bps = 8.0;
        c.max_position = 1.0;
        let nl = NonlinearSkewConfig {
            enabled: true,
            boost: 1.0,
            cap_bps: 8.0,
        };
        for position in [-1.0, -0.6, -0.2, 0.0, 0.2, 0.6, 1.0] {
            assert_eq!(
                skew_center_with(&c, nl, 100.0, position),
                skew_center(&c, 100.0, position),
                "position {position}"
            );
        }
    }

    #[test]
    fn nonlinear_skew_steepens_saturates_and_mirrors() {
        let mut c = cfg();
        c.skew_bps = 8.0;
        c.max_position = 1.0;
        let nl = NonlinearSkewConfig {
            enabled: true,
            boost: 3.0,
            cap_bps: 12.0,
        };
        // ratio 0.2 -> 8*3*0.2 = 4.8bps below mark (long favors selling).
        let long = skew_center_with(&c, nl, 100.0, 0.2);
        assert!((long - (100.0 * (1.0 - 4.8 / 1e4))).abs() < 1e-9);
        // Steeper than linear (linear at 0.2 is 1.6bps).
        assert!(long < skew_center(&c, 100.0, 0.2));
        // ratio 0.5 -> 12bps raw but capped at 12 -> exactly the cap; deeper
        // inventory saturates at the same shift.
        let at_half = skew_center_with(&c, nl, 100.0, 0.5);
        let at_full = skew_center_with(&c, nl, 100.0, 1.0);
        assert!((at_half - (100.0 * (1.0 - 12.0 / 1e4))).abs() < 1e-9);
        assert_eq!(at_half, at_full);
        // Long/short mirror around the mark.
        let short = skew_center_with(&c, nl, 100.0, -0.2);
        assert!((short + long - 200.0).abs() < 1e-9);
        // Zero position never shifts.
        assert_eq!(skew_center_with(&c, nl, 100.0, 0.0), 100.0);
    }

    #[test]
    fn guard_suppresses_endangered_side_and_cancels_its_resting_quotes() {
        let c = cfg();
        let resting = vec![
            resting(OrderSide::Buy, 0, 99.9, 100.0),
            resting(OrderSide::Sell, 0, 100.1, 100.0),
        ];
        let plan = plan_cycle(
            &c,
            CycleInput {
                cycle: 3,
                market: MarketSnapshot {
                    mark: 100.0,
                    best_bid: Some(99.99),
                    best_ask: Some(100.01),
                },
                position: 0.0,
                resting: &resting,
                pending_slots: &[],
                market_data_mode: MarketDataMode::Active,
                active_exit_enabled: false,
                inventory_exit_pct: 0.0,
                inventory_exit_qty: 0.0,
                size_skew: Default::default(),
                nonlinear_skew: Default::default(),
                guard: GuardDecision {
                    enabled: true,
                    active: true,
                    endangered: Some(OrderSide::Sell),
                    divergence_bps: Some(9.0),
                },
                wind_down: false,
                qty_tolerance: 0.0005,
            },
            false,
        );

        // The endangered sell side: no new quotes, resting cancelled as
        // side-suppressed.
        assert!(!plan
            .actions
            .iter()
            .any(|action| matches!(action, Action::Place(q) if q.side == OrderSide::Sell)));
        assert!(plan.actions.iter().any(|action| matches!(
            action,
            Action::Cancel {
                side: OrderSide::Sell,
                reason: CancelReason::SideSuppressed,
                ..
            }
        )));
        // The safe buy side keeps quoting (hold of the fresh resting quote).
        assert!(plan.actions.iter().any(|action| matches!(
            action,
            Action::Hold {
                side: OrderSide::Buy,
                ..
            }
        )));
    }

    #[test]
    fn combined_high_inventory_and_guard_keeps_all_invariants() {
        let mut c = cfg();
        c.skew_bps = 8.0;
        c.band_bps = 30.0;
        c.max_position = 0.05;
        let nl = NonlinearSkewConfig {
            enabled: true,
            boost: 3.0,
            cap_bps: 12.0,
        };
        // Long 80% of max while the external leader jumps DOWN: guard protects
        // the buy side (stale-rich bids), nonlinear skew is already pushing the
        // center down. Both act in the same defensive direction by design.
        let position = 0.04;
        let plan = plan_cycle(
            &c,
            CycleInput {
                cycle: 9,
                market: MarketSnapshot {
                    mark: 100.0,
                    best_bid: Some(99.99),
                    best_ask: Some(100.01),
                },
                position,
                resting: &[],
                pending_slots: &[],
                market_data_mode: MarketDataMode::Active,
                active_exit_enabled: false,
                inventory_exit_pct: 0.0,
                inventory_exit_qty: 0.0,
                size_skew: Default::default(),
                nonlinear_skew: nl,
                guard: GuardDecision {
                    enabled: true,
                    active: true,
                    endangered: Some(OrderSide::Buy),
                    divergence_bps: Some(-8.0),
                },
                wind_down: false,
                qty_tolerance: 0.0005,
            },
            false,
        );

        let band_lo = 100.0 * (1.0 - c.band_bps / 1e4);
        let band_hi = 100.0 * (1.0 + c.band_bps / 1e4);
        let mut worst_case_long = position;
        for action in &plan.actions {
            if let Action::Place(q) = action {
                // Guard: no buy quotes at all.
                assert_ne!(q.side, OrderSide::Buy);
                // Band and no-cross hold under the combined shift.
                assert!(q.price >= band_lo && q.price <= band_hi);
                assert!(q.price >= 99.99 + c.price_tick() - 1e-9);
                if q.side == OrderSide::Buy {
                    worst_case_long += q.qty;
                }
            }
        }
        assert!(worst_case_long <= c.max_position + 1e-9);

        // Release: guard inactive next cycle restores the buy ladder under the
        // same (still skewed) center.
        let released = plan_cycle(
            &c,
            CycleInput {
                cycle: 10,
                market: MarketSnapshot {
                    mark: 100.0,
                    best_bid: Some(99.99),
                    best_ask: Some(100.01),
                },
                position,
                resting: &[],
                pending_slots: &[],
                market_data_mode: MarketDataMode::Active,
                active_exit_enabled: false,
                inventory_exit_pct: 0.0,
                inventory_exit_qty: 0.0,
                size_skew: Default::default(),
                nonlinear_skew: nl,
                guard: GuardDecision {
                    enabled: true,
                    active: false,
                    endangered: None,
                    divergence_bps: Some(1.0),
                },
                wind_down: false,
                qty_tolerance: 0.0005,
            },
            false,
        );
        assert!(released
            .actions
            .iter()
            .any(|action| matches!(action, Action::Place(q) if q.side == OrderSide::Buy)));
    }

    // 28. Alert monitor: disabled emits nothing.
    #[test]
    fn alerts_disabled() {
        let mut m = AlertMonitor::new(0.0, 0.0, 0.0);
        assert!(!m.session_enabled());
        let s = MakerStats::default();
        assert!(m.evaluate(&s, 5.0, 100.0, 0.05, 100).is_empty());
    }

    // 29. Loss alert: edge-triggered fire then clear.
    #[test]
    fn alerts_loss_edge() {
        let mut m = AlertMonitor::new(1.0, 0.0, 0.0); // loss limit 1.0
        let mut s = MakerStats::default();
        // Buy 1 @ 100 (cash -100), mark drops to 98 -> pnl = -100 + 1*98 = -2.
        s.record_fill(OrderSide::Buy, 100.0, 1.0, 100.0);
        let a = m.evaluate(&s, 1.0, 98.0, 0.05, 5);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].kind, "loss");
        assert!(a[0].firing);
        // Held breach -> no repeat.
        assert!(m.evaluate(&s, 1.0, 98.0, 0.05, 6).is_empty());
        // Recover above -limit/2 (pnl at mark 100 = 0) -> clear.
        let a = m.evaluate(&s, 1.0, 100.0, 0.05, 7);
        assert_eq!(a.len(), 1);
        assert!(!a[0].firing);
    }

    // 30. Inventory alert fires at the configured pct of max.
    #[test]
    fn alerts_inventory_pct() {
        let mut m = AlertMonitor::new(0.0, 80.0, 0.0); // 80% of max
        let s = MakerStats::default();
        // max 0.05 -> threshold 0.04. Position 0.03 -> no alert.
        assert!(m.evaluate(&s, 0.03, 100.0, 0.05, 5).is_empty());
        // 0.045 >= 0.04 -> fire.
        let a = m.evaluate(&s, 0.045, 100.0, 0.05, 6);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].kind, "inventory");
        assert!(a[0].firing);
        // Short side symmetric: still on (held), no repeat.
        assert!(m.evaluate(&s, 0.045, 100.0, 0.05, 7).is_empty());
    }

    // 31. Uptime alert waits for warmup.
    #[test]
    fn alerts_uptime_warmup() {
        let mut m = AlertMonitor::new(0.0, 0.0, 50.0); // floor 50%
        let mut s = MakerStats::default();
        // One one-sided cycle -> uptime 0%, but before warmup: no alert.
        s.end_cycle(0.0, false);
        assert!(m.evaluate(&s, 0.0, 100.0, 0.05, 5).is_empty());
        // After warmup, still 0% < 50% -> fire.
        let a = m.evaluate(&s, 0.0, 100.0, 0.05, 25);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].kind, "uptime");
        assert!(a[0].firing);
    }

    // 32. Account floors disabled by default; account_enabled reflects config.
    #[test]
    fn alerts_account_disabled_by_default() {
        let mut m = AlertMonitor::new(0.0, 0.0, 0.0);
        assert!(!m.account_enabled());
        assert!(m.evaluate_account(10.0, 5.0).is_empty());
    }

    // 33. Equity floor: edge-triggered fire then clear with hysteresis.
    #[test]
    fn alerts_equity_alert_edge() {
        let mut m = AlertMonitor::new(0.0, 0.0, 0.0).with_account_alerts(100.0, 0.0);
        assert!(m.account_enabled());
        // Above floor -> no alert.
        assert!(m.evaluate_account(120.0, 999.0).is_empty());
        // Drop below floor -> fire.
        let a = m.evaluate_account(90.0, 999.0);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].kind, "equity");
        assert!(a[0].firing);
        // Held breach -> no repeat.
        assert!(m.evaluate_account(90.0, 999.0).is_empty());
        // Back above floor but within hysteresis band (< 110) -> still on.
        assert!(m.evaluate_account(105.0, 999.0).is_empty());
        // Recover to >= floor*1.1 -> clear.
        let a = m.evaluate_account(111.0, 999.0);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].kind, "equity");
        assert!(!a[0].firing);
    }

    // 34. Available-margin floor fires independently of equity.
    #[test]
    fn alerts_margin_alert_fires() {
        let mut m = AlertMonitor::new(0.0, 0.0, 0.0).with_account_alerts(0.0, 50.0);
        assert!(m.evaluate_account(9999.0, 60.0).is_empty());
        let a = m.evaluate_account(9999.0, 40.0);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].kind, "margin");
        assert!(a[0].firing);
    }

    // 35. Wind-down: any residual position yields a full reduce-only exit
    // plan and no new quotes, even with configured exits fully disabled
    // (the frozen live configs). A vol halt suppresses the taker exit but
    // never the quote suppression.
    #[test]
    fn wind_down_flattens_residual_and_stops_quoting() {
        let c = cfg();
        let resting = vec![
            resting(OrderSide::Buy, 0, 99.9, 100.0),
            resting(OrderSide::Sell, 0, 100.1, 100.0),
        ];
        let input = CycleInput {
            cycle: 7,
            market: MarketSnapshot {
                mark: 100.0,
                best_bid: Some(99.9),
                best_ask: Some(100.1),
            },
            position: -0.02,
            resting: &resting,
            pending_slots: &[],
            market_data_mode: MarketDataMode::Active,
            active_exit_enabled: true,
            inventory_exit_pct: 0.0,
            inventory_exit_qty: 0.0,
            size_skew: Default::default(),
            nonlinear_skew: Default::default(),
            guard: Default::default(),
            wind_down: true,
            qty_tolerance: 0.0005,
        };
        let plan = plan_cycle(&c, input, false);
        assert_eq!(
            plan.requested_inventory_exit,
            Some(InventoryExit {
                side: OrderSide::Buy,
                qty: 0.02,
                kind: ExitKind::WindDown,
            })
        );
        assert_eq!(plan.inventory_exit, plan.requested_inventory_exit);
        assert!(plan
            .actions
            .iter()
            .any(|action| matches!(action, Action::Cancel { .. })));
        assert!(!plan
            .actions
            .iter()
            .any(|action| matches!(action, Action::Place(_))));

        let halted = plan_cycle(&c, input, true);
        assert_eq!(halted.inventory_exit, None);
        assert!(!halted
            .actions
            .iter()
            .any(|action| matches!(action, Action::Place(_))));
    }

    // 36. Wind-down keeps quotes off even when already flat, so inventory
    // cannot re-accumulate while the supervisor waits to switch.
    #[test]
    fn wind_down_flat_still_suppresses_quotes() {
        let c = cfg();
        let plan = plan_cycle(
            &c,
            CycleInput {
                cycle: 8,
                market: MarketSnapshot {
                    mark: 100.0,
                    best_bid: Some(99.9),
                    best_ask: Some(100.1),
                },
                position: 0.0,
                resting: &[],
                pending_slots: &[],
                market_data_mode: MarketDataMode::Active,
                active_exit_enabled: true,
                inventory_exit_pct: 0.0,
                inventory_exit_qty: 0.0,
                size_skew: Default::default(),
                nonlinear_skew: Default::default(),
                guard: Default::default(),
                wind_down: true,
                qty_tolerance: 0.0005,
            },
            false,
        );
        assert_eq!(plan.requested_inventory_exit, None);
        assert_eq!(plan.inventory_exit, None);
        assert!(plan.actions.is_empty());
    }

    // 37. Wind-down overrides the configured exit threshold (a residual
    // below the enabled trigger still exits, both sides) and treats
    // positions at or below the quantity tolerance as flat.
    #[test]
    fn wind_down_overrides_threshold_and_honors_tolerance() {
        let c = cfg();
        let mk = |position: f64| CycleInput {
            cycle: 9,
            market: MarketSnapshot {
                mark: 100.0,
                best_bid: Some(99.9),
                best_ask: Some(100.1),
            },
            position,
            resting: &[],
            pending_slots: &[],
            market_data_mode: MarketDataMode::Active,
            active_exit_enabled: true,
            inventory_exit_pct: 80.0,
            inventory_exit_qty: 0.01,
            size_skew: Default::default(),
            nonlinear_skew: Default::default(),
            guard: Default::default(),
            wind_down: true,
            qty_tolerance: 0.0005,
        };
        // 0.02 is below the configured 80%-of-max trigger (0.04): the
        // configured path stays inactive, wind-down exits everything.
        let long = plan_cycle(&c, mk(0.02), false);
        assert_eq!(
            long.inventory_exit,
            Some(InventoryExit {
                side: OrderSide::Sell,
                qty: 0.02,
                kind: ExitKind::WindDown,
            })
        );
        assert_eq!(plan_cycle(&c, mk(0.0005), false).inventory_exit, None);
        assert_eq!(plan_cycle(&c, mk(-0.0005), false).inventory_exit, None);
        assert_eq!(
            plan_cycle(&c, mk(-0.0006), false).inventory_exit,
            Some(InventoryExit {
                side: OrderSide::Buy,
                qty: 0.0006,
                kind: ExitKind::WindDown,
            })
        );
    }
}
