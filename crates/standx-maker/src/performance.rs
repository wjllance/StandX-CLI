//! Deterministic maker performance attribution and time-weighted observations.
//!
//! The caller supplies normalized event timestamps and quote-currency cash
//! flows. This module never reads a clock, parses exchange payloads, or does
//! I/O, so the same trace can be replayed byte-for-byte in tests and offline
//! tools. Existing strategy decisions do not depend on these observations.

use standx_sdk::models::OrderSide;
use std::collections::{HashMap, HashSet};
use std::fmt;

/// Post-fill horizons required by the strategy roadmap.
pub const MARKOUT_WINDOWS_MS: [i64; 3] = [1_000, 5_000, 30_000];

/// Why a current-run execution happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FillRole {
    /// A post-only quote owned by the maker session.
    PassiveMaker,
    /// A reduce-only active inventory exit owned by the maker session.
    InventoryExit,
}

/// Quote-currency execution costs. Both values are positive; a zero-cost fill
/// uses `Some(ExecutionCosts::default())`, while `None` means costs were not
/// present or could not be converted and must remain visibly unavailable.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ExecutionCosts {
    pub fee_quote: f64,
    pub rebate_quote: f64,
}

/// One normalized, immutable execution accepted by the current-run ledger.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PerformanceFill {
    pub trade_id: u64,
    pub order_id: u64,
    pub role: FillRole,
    pub side: OrderSide,
    pub price: f64,
    pub qty: f64,
    pub mark_at_fill: f64,
    pub event_time_ms: i64,
    pub costs: Option<ExecutionCosts>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PerformanceError {
    InvalidStartingPoint {
        position: f64,
        mark: f64,
    },
    InvalidFill {
        trade_id: u64,
    },
    UnknownExecutionCosts {
        trade_id: u64,
    },
    ConflictingExecutionCosts {
        trade_id: u64,
    },
    InvalidMarketObservation {
        event_time_ms: i64,
        mark: f64,
    },
    MarketTimeRegression {
        previous_ms: i64,
        next_ms: i64,
    },
    InvalidFunding {
        event_time_ms: i64,
        cashflow_quote: f64,
    },
    FundingTimeRegression {
        previous_ms: i64,
        next_ms: i64,
    },
    InvalidQuoteInterval {
        event_time_ms: i64,
        eligible_bid_qty: f64,
        eligible_ask_qty: f64,
    },
    QuoteTimeRegression {
        previous_ms: i64,
        next_ms: i64,
    },
}

impl fmt::Display for PerformanceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidStartingPoint { position, mark } => {
                write!(formatter, "invalid performance baseline position={position}, mark={mark}")
            }
            Self::InvalidFill { trade_id } => {
                write!(formatter, "invalid performance fill trade_id={trade_id}")
            }
            Self::UnknownExecutionCosts { trade_id } => write!(
                formatter,
                "execution costs reference unknown trade_id={trade_id}"
            ),
            Self::ConflictingExecutionCosts { trade_id } => write!(
                formatter,
                "execution costs conflict for trade_id={trade_id}"
            ),
            Self::InvalidMarketObservation {
                event_time_ms,
                mark,
            } => write!(
                formatter,
                "invalid market observation time={event_time_ms}, mark={mark}"
            ),
            Self::MarketTimeRegression {
                previous_ms,
                next_ms,
            } => write!(
                formatter,
                "market observation time regressed from {previous_ms} to {next_ms}"
            ),
            Self::InvalidFunding {
                event_time_ms,
                cashflow_quote,
            } => write!(
                formatter,
                "invalid funding cashflow time={event_time_ms}, quote={cashflow_quote}"
            ),
            Self::FundingTimeRegression {
                previous_ms,
                next_ms,
            } => write!(
                formatter,
                "funding event time regressed from {previous_ms} to {next_ms}"
            ),
            Self::InvalidQuoteInterval {
                event_time_ms,
                eligible_bid_qty,
                eligible_ask_qty,
            } => write!(
                formatter,
                "invalid quote interval time={event_time_ms}, bid_qty={eligible_bid_qty}, ask_qty={eligible_ask_qty}"
            ),
            Self::QuoteTimeRegression {
                previous_ms,
                next_ms,
            } => write!(
                formatter,
                "quote interval time regressed from {previous_ms} to {next_ms}"
            ),
        }
    }
}

impl std::error::Error for PerformanceError {}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct MarkoutAccumulator {
    qty: f64,
    quote_pnl: f64,
    bps_qty_sum: f64,
    samples: u64,
    unavailable: u64,
}

/// Quantity-weighted result for one post-fill horizon.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MarkoutSummary {
    pub window_ms: i64,
    pub samples: u64,
    pub pending: u64,
    pub unavailable: u64,
    pub qty: f64,
    pub quote_pnl: f64,
    pub avg_bps: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct PendingMarkout {
    side: OrderSide,
    price: f64,
    qty: f64,
    target_ms: i64,
    window_index: usize,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct InventoryEvent {
    event_time_ms: i64,
    trade_id: u64,
    delta: f64,
}

/// Time-weighted inventory exposure over the observed market interval.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct InventoryTimeSummary {
    pub observed_ms: i64,
    pub nonzero_ms: i64,
    pub abs_qty_ms: f64,
    pub avg_abs_qty: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct MarketObservation {
    event_time_ms: i64,
    mark: f64,
}

/// A snapshot of eligible resting quantity that remains in force until the
/// next observation. Quantity is base units; its time integral is base-ms.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct QuoteQualityInterval {
    pub event_time_ms: i64,
    pub eligible_bid_qty: f64,
    pub eligible_ask_qty: f64,
}

/// Time-weighted quote availability. The legacy cycle-based uptime remains a
/// separate compatibility metric.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct QuoteTimeSummary {
    pub observed_ms: i64,
    pub two_sided_ms: i64,
    pub two_sided_uptime_pct: f64,
    pub eligible_bid_qty_ms: f64,
    pub eligible_ask_qty_ms: f64,
    pub eligible_total_qty_ms: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct QuoteTimeTracker {
    last: Option<QuoteQualityInterval>,
    summary: QuoteTimeSummary,
}

impl QuoteTimeTracker {
    fn observe(&mut self, next: QuoteQualityInterval) -> Result<(), PerformanceError> {
        validate_quote_interval(next)?;
        if let Some(previous) = self.last {
            if next.event_time_ms < previous.event_time_ms {
                return Err(PerformanceError::QuoteTimeRegression {
                    previous_ms: previous.event_time_ms,
                    next_ms: next.event_time_ms,
                });
            }
            self.accrue(previous, next.event_time_ms - previous.event_time_ms);
        }
        self.last = Some(next);
        Ok(())
    }

    fn finish(&mut self, end_time_ms: i64) -> Result<(), PerformanceError> {
        if let Some(previous) = self.last {
            if end_time_ms < previous.event_time_ms {
                return Err(PerformanceError::QuoteTimeRegression {
                    previous_ms: previous.event_time_ms,
                    next_ms: end_time_ms,
                });
            }
            self.accrue(previous, end_time_ms - previous.event_time_ms);
            self.last = None;
        }
        Ok(())
    }

    fn accrue(&mut self, previous: QuoteQualityInterval, duration_ms: i64) {
        self.summary.observed_ms += duration_ms;
        if previous.eligible_bid_qty > 0.0 && previous.eligible_ask_qty > 0.0 {
            self.summary.two_sided_ms += duration_ms;
        }
        let duration = duration_ms as f64;
        self.summary.eligible_bid_qty_ms += previous.eligible_bid_qty * duration;
        self.summary.eligible_ask_qty_ms += previous.eligible_ask_qty * duration;
        self.summary.eligible_total_qty_ms =
            self.summary.eligible_bid_qty_ms + self.summary.eligible_ask_qty_ms;
        self.summary.two_sided_uptime_pct = if self.summary.observed_ms == 0 {
            0.0
        } else {
            self.summary.two_sided_ms as f64 / self.summary.observed_ms as f64 * 100.0
        };
    }
}

fn validate_quote_interval(interval: QuoteQualityInterval) -> Result<(), PerformanceError> {
    if !interval.eligible_bid_qty.is_finite()
        || !interval.eligible_ask_qty.is_finite()
        || interval.eligible_bid_qty < 0.0
        || interval.eligible_ask_qty < 0.0
    {
        return Err(PerformanceError::InvalidQuoteInterval {
            event_time_ms: interval.event_time_ms,
            eligible_bid_qty: interval.eligible_bid_qty,
            eligible_ask_qty: interval.eligible_ask_qty,
        });
    }
    Ok(())
}

/// Immutable snapshot used by JSON output, dashboards, and replay assertions.
#[derive(Clone, Debug, PartialEq)]
pub struct PerformanceSummary {
    pub passive_fills: u64,
    pub passive_qty: f64,
    pub passive_cashflow_quote: f64,
    pub passive_capture_bps: Option<f64>,
    pub exit_fills: u64,
    pub exit_qty: f64,
    pub exit_cashflow_quote: f64,
    pub gross_spread_quote: f64,
    pub fee_quote: f64,
    pub rebate_quote: f64,
    pub execution_costs_unavailable: u64,
    pub funding_quote: f64,
    pub funding_available: bool,
    /// Funding rows that exist but are **not** inside `funding_quote`: a
    /// settlement asset that is neither the quote nor its D-prefixed form, or a
    /// row that arrived out of chronological order. Counted rather than
    /// dropped — an unattributed cashflow makes the attribution incomplete, and
    /// `net_pnl_complete` must say so.
    pub funding_unattributed: u64,
    /// The session could not establish full funding coverage: a failed history
    /// fetch, or a page that came back at the caller's limit and may have cut
    /// older rows. Distinct from `funding_unattributed`, which counts rows we
    /// *saw* but could not fold in.
    pub funding_coverage_gap: bool,
    pub net_pnl_complete: bool,
    pub exit_cost_quote: f64,
    pub inventory_mtm_change_quote: f64,
    pub net_pnl_quote: f64,
    pub position: f64,
    pub markouts: [MarkoutSummary; 3],
    pub quote_time: QuoteTimeSummary,
    pub inventory_time: InventoryTimeSummary,
}

/// Current-run performance state. Trade IDs are deduplicated defensively even
/// though the authoritative maker ledger should only forward accepted fills.
#[derive(Clone, Debug)]
pub struct PerformanceLedger {
    starting_position: f64,
    starting_mark: f64,
    position: f64,
    fill_cash: f64,
    passive_fills: u64,
    passive_qty: f64,
    passive_cashflow_quote: f64,
    passive_capture_bps_qty_sum: f64,
    exit_fills: u64,
    exit_qty: f64,
    exit_cashflow_quote: f64,
    gross_spread_quote: f64,
    fee_quote: f64,
    rebate_quote: f64,
    funding_quote: f64,
    funding_observed: bool,
    funding_unattributed: u64,
    funding_coverage_gap: bool,
    exit_cost_quote: f64,
    seen_trade_ids: HashSet<u64>,
    costs_by_trade_id: HashMap<u64, ExecutionCosts>,
    markets: Vec<MarketObservation>,
    inventory_events: Vec<InventoryEvent>,
    observation_start_ms: Option<i64>,
    observation_end_ms: Option<i64>,
    pending_markouts: Vec<PendingMarkout>,
    markouts: [MarkoutAccumulator; 3],
    last_funding_time_ms: Option<i64>,
    quote_time: QuoteTimeTracker,
}

impl PerformanceLedger {
    pub fn new(starting_position: f64, starting_mark: f64) -> Result<Self, PerformanceError> {
        if !starting_position.is_finite() || !starting_mark.is_finite() || starting_mark <= 0.0 {
            return Err(PerformanceError::InvalidStartingPoint {
                position: starting_position,
                mark: starting_mark,
            });
        }
        Ok(Self {
            starting_position,
            starting_mark,
            position: starting_position,
            fill_cash: 0.0,
            passive_fills: 0,
            passive_qty: 0.0,
            passive_cashflow_quote: 0.0,
            passive_capture_bps_qty_sum: 0.0,
            exit_fills: 0,
            exit_qty: 0.0,
            exit_cashflow_quote: 0.0,
            gross_spread_quote: 0.0,
            fee_quote: 0.0,
            rebate_quote: 0.0,
            funding_quote: 0.0,
            funding_observed: false,
            funding_unattributed: 0,
            funding_coverage_gap: false,
            exit_cost_quote: 0.0,
            seen_trade_ids: HashSet::new(),
            costs_by_trade_id: HashMap::new(),
            markets: Vec::new(),
            inventory_events: Vec::new(),
            observation_start_ms: None,
            observation_end_ms: None,
            pending_markouts: Vec::new(),
            markouts: [MarkoutAccumulator::default(); 3],
            last_funding_time_ms: None,
            quote_time: QuoteTimeTracker::default(),
        })
    }

    /// Returns false for a duplicate stable trade ID.
    pub fn record_fill(&mut self, fill: PerformanceFill) -> Result<bool, PerformanceError> {
        if fill.trade_id == 0
            || fill.order_id == 0
            || !fill.price.is_finite()
            || !fill.qty.is_finite()
            || !fill.mark_at_fill.is_finite()
            || fill.price <= 0.0
            || fill.qty <= 0.0
            || fill.mark_at_fill <= 0.0
            || fill.costs.is_some_and(|costs| !valid_costs(costs))
        {
            return Err(PerformanceError::InvalidFill {
                trade_id: fill.trade_id,
            });
        }
        if !self.seen_trade_ids.insert(fill.trade_id) {
            return Ok(false);
        }

        let direction = side_direction(fill.side);
        self.position += direction * fill.qty;
        let cashflow_quote = -direction * fill.price * fill.qty;
        self.fill_cash += cashflow_quote;
        self.inventory_events.push(InventoryEvent {
            event_time_ms: fill.event_time_ms,
            trade_id: fill.trade_id,
            delta: direction * fill.qty,
        });
        if let Some(costs) = fill.costs {
            self.apply_execution_costs(fill.trade_id, costs);
        }
        match fill.role {
            FillRole::PassiveMaker => {
                self.passive_fills += 1;
                self.passive_qty += fill.qty;
                self.passive_cashflow_quote += cashflow_quote;
                self.gross_spread_quote += direction * (fill.mark_at_fill - fill.price) * fill.qty;
                self.passive_capture_bps_qty_sum +=
                    direction * (fill.mark_at_fill - fill.price) / fill.price * 10_000.0 * fill.qty;
            }
            FillRole::InventoryExit => {
                self.exit_fills += 1;
                self.exit_qty += fill.qty;
                self.exit_cashflow_quote += cashflow_quote;
                self.exit_cost_quote += direction * (fill.price - fill.mark_at_fill) * fill.qty;
            }
        }

        for (window_index, window_ms) in MARKOUT_WINDOWS_MS.into_iter().enumerate() {
            let target_ms = fill.event_time_ms.saturating_add(window_ms);
            if let Some(observation) = self
                .markets
                .iter()
                .find(|market| market.event_time_ms >= target_ms)
                .copied()
            {
                self.apply_markout(
                    window_index,
                    fill.side,
                    fill.price,
                    fill.qty,
                    observation.mark,
                );
            } else {
                self.pending_markouts.push(PendingMarkout {
                    side: fill.side,
                    price: fill.price,
                    qty: fill.qty,
                    target_ms,
                    window_index,
                });
            }
        }
        Ok(true)
    }

    /// Enrich an existing execution when REST supplies costs after the account
    /// stream trade. Exact duplicates are idempotent; conflicting values fail.
    pub fn record_execution_costs(
        &mut self,
        trade_id: u64,
        costs: ExecutionCosts,
    ) -> Result<bool, PerformanceError> {
        if !self.seen_trade_ids.contains(&trade_id) {
            return Err(PerformanceError::UnknownExecutionCosts { trade_id });
        }
        if !valid_costs(costs) {
            return Err(PerformanceError::InvalidFill { trade_id });
        }
        if let Some(previous) = self.costs_by_trade_id.get(&trade_id) {
            if *previous == costs {
                return Ok(false);
            }
            return Err(PerformanceError::ConflictingExecutionCosts { trade_id });
        }
        self.apply_execution_costs(trade_id, costs);
        Ok(true)
    }

    /// Observe a normalized mark. Equal timestamps are allowed; regressions
    /// are rejected so replay ordering mistakes cannot silently alter markout.
    pub fn observe_market(
        &mut self,
        event_time_ms: i64,
        mark: f64,
    ) -> Result<(), PerformanceError> {
        if !mark.is_finite() || mark <= 0.0 {
            return Err(PerformanceError::InvalidMarketObservation {
                event_time_ms,
                mark,
            });
        }
        if let Some(previous) = self.markets.last() {
            if event_time_ms < previous.event_time_ms {
                return Err(PerformanceError::MarketTimeRegression {
                    previous_ms: previous.event_time_ms,
                    next_ms: event_time_ms,
                });
            }
        }
        self.markets.push(MarketObservation {
            event_time_ms,
            mark,
        });
        self.observation_start_ms.get_or_insert(event_time_ms);
        self.observation_end_ms = Some(event_time_ms);

        let mut unresolved = Vec::with_capacity(self.pending_markouts.len());
        for pending in std::mem::take(&mut self.pending_markouts) {
            if event_time_ms >= pending.target_ms {
                self.apply_markout(
                    pending.window_index,
                    pending.side,
                    pending.price,
                    pending.qty,
                    mark,
                );
            } else {
                unresolved.push(pending);
            }
        }
        self.pending_markouts = unresolved;
        Ok(())
    }

    /// Signed funding cashflow in quote currency: positive is received,
    /// negative is paid.
    pub fn record_funding(
        &mut self,
        event_time_ms: i64,
        cashflow_quote: f64,
    ) -> Result<(), PerformanceError> {
        if !cashflow_quote.is_finite() {
            return Err(PerformanceError::InvalidFunding {
                event_time_ms,
                cashflow_quote,
            });
        }
        if let Some(previous_ms) = self.last_funding_time_ms {
            if event_time_ms < previous_ms {
                return Err(PerformanceError::FundingTimeRegression {
                    previous_ms,
                    next_ms: event_time_ms,
                });
            }
        }
        self.last_funding_time_ms = Some(event_time_ms);
        self.funding_observed = true;
        self.funding_quote += cashflow_quote;
        Ok(())
    }

    /// A funding cashflow that exists but could not be folded into
    /// `funding_quote` (unconvertible settlement asset, or an out-of-order
    /// arrival the monotonic ledger cannot accept). Never silently dropped: it
    /// clears `net_pnl_complete` the same way an unconvertible execution cost
    /// does.
    pub fn record_unattributed_funding(&mut self) {
        self.funding_unattributed = self.funding_unattributed.saturating_add(1);
    }

    /// The caller could not establish full funding coverage for the session
    /// (fetch failed, or the returned page hit its limit and may be truncated).
    /// Latching rather than counting: once coverage has a hole, the session's
    /// funding total is a lower bound for the rest of the run.
    pub fn record_funding_coverage_gap(&mut self) {
        self.funding_coverage_gap = true;
    }

    pub fn observe_quote_quality(
        &mut self,
        interval: QuoteQualityInterval,
    ) -> Result<(), PerformanceError> {
        self.quote_time.observe(interval)
    }

    /// Close time-weighted quote observation and censor every unresolved
    /// markout. Censored windows are counted as unavailable, never substituted
    /// with the latest mark.
    pub fn finish(&mut self, end_time_ms: i64) -> Result<(), PerformanceError> {
        self.quote_time.finish(end_time_ms)?;
        if let Some(start_ms) = self.observation_start_ms {
            if end_time_ms < start_ms {
                return Err(PerformanceError::MarketTimeRegression {
                    previous_ms: start_ms,
                    next_ms: end_time_ms,
                });
            }
            self.observation_end_ms = Some(end_time_ms);
        }
        for pending in self.pending_markouts.drain(..) {
            self.markouts[pending.window_index].unavailable += 1;
        }
        Ok(())
    }

    pub fn summary(&self, final_mark: f64) -> Result<PerformanceSummary, PerformanceError> {
        if !final_mark.is_finite() || final_mark <= 0.0 {
            return Err(PerformanceError::InvalidMarketObservation {
                event_time_ms: self.markets.last().map_or(0, |market| market.event_time_ms),
                mark: final_mark,
            });
        }
        let net_pnl_quote = self.fill_cash + self.position * final_mark
            - self.starting_position * self.starting_mark
            + self.rebate_quote
            - self.fee_quote
            + self.funding_quote;
        // This residual is the inventory revaluation component that makes the
        // roadmap identity exact without double-counting active-exit slippage.
        let inventory_mtm_change_quote =
            net_pnl_quote - self.gross_spread_quote - self.rebate_quote + self.fee_quote
                - self.funding_quote
                + self.exit_cost_quote;

        let markouts = std::array::from_fn(|index| {
            let value = self.markouts[index];
            MarkoutSummary {
                window_ms: MARKOUT_WINDOWS_MS[index],
                samples: value.samples,
                pending: self
                    .pending_markouts
                    .iter()
                    .filter(|pending| pending.window_index == index)
                    .count() as u64,
                unavailable: value.unavailable,
                qty: value.qty,
                quote_pnl: value.quote_pnl,
                avg_bps: (value.qty > 0.0).then_some(value.bps_qty_sum / value.qty),
            }
        });
        let inventory_time = self.inventory_time_summary();
        let execution_costs_unavailable =
            self.seen_trade_ids
                .len()
                .saturating_sub(self.costs_by_trade_id.len()) as u64;
        Ok(PerformanceSummary {
            passive_fills: self.passive_fills,
            passive_qty: self.passive_qty,
            passive_cashflow_quote: self.passive_cashflow_quote,
            passive_capture_bps: (self.passive_qty > 0.0)
                .then_some(self.passive_capture_bps_qty_sum / self.passive_qty),
            exit_fills: self.exit_fills,
            exit_qty: self.exit_qty,
            exit_cashflow_quote: self.exit_cashflow_quote,
            gross_spread_quote: self.gross_spread_quote,
            fee_quote: self.fee_quote,
            rebate_quote: self.rebate_quote,
            execution_costs_unavailable,
            funding_quote: self.funding_quote,
            funding_available: self.funding_observed,
            funding_unattributed: self.funding_unattributed,
            funding_coverage_gap: self.funding_coverage_gap,
            net_pnl_complete: execution_costs_unavailable == 0
                && self.funding_observed
                && self.funding_unattributed == 0
                && !self.funding_coverage_gap,
            exit_cost_quote: self.exit_cost_quote,
            inventory_mtm_change_quote,
            net_pnl_quote,
            position: self.position,
            markouts,
            quote_time: self.quote_time.summary,
            inventory_time,
        })
    }

    fn inventory_time_summary(&self) -> InventoryTimeSummary {
        let (Some(start_ms), Some(end_ms)) = (self.observation_start_ms, self.observation_end_ms)
        else {
            return InventoryTimeSummary::default();
        };
        let mut events = self.inventory_events.clone();
        events.sort_by_key(|event| (event.event_time_ms, event.trade_id));
        let mut position = self.starting_position;
        let mut cursor_ms = start_ms;
        let mut summary = InventoryTimeSummary {
            observed_ms: end_ms.saturating_sub(start_ms),
            ..InventoryTimeSummary::default()
        };
        for event in events {
            let event_ms = event.event_time_ms.clamp(start_ms, end_ms);
            accrue_inventory_time(&mut summary, position, event_ms.saturating_sub(cursor_ms));
            position += event.delta;
            cursor_ms = cursor_ms.max(event_ms);
        }
        accrue_inventory_time(&mut summary, position, end_ms.saturating_sub(cursor_ms));
        summary.avg_abs_qty = if summary.observed_ms > 0 {
            summary.abs_qty_ms / summary.observed_ms as f64
        } else {
            0.0
        };
        summary
    }

    fn apply_markout(
        &mut self,
        window_index: usize,
        side: OrderSide,
        price: f64,
        qty: f64,
        future_mark: f64,
    ) {
        let direction = side_direction(side);
        let quote_pnl = direction * (future_mark - price) * qty;
        let bps = direction * (future_mark - price) / price * 10_000.0;
        let accumulator = &mut self.markouts[window_index];
        accumulator.qty += qty;
        accumulator.quote_pnl += quote_pnl;
        accumulator.bps_qty_sum += bps * qty;
        accumulator.samples += 1;
    }

    fn apply_execution_costs(&mut self, trade_id: u64, costs: ExecutionCosts) {
        self.costs_by_trade_id.insert(trade_id, costs);
        self.fee_quote += costs.fee_quote;
        self.rebate_quote += costs.rebate_quote;
    }
}

fn valid_costs(costs: ExecutionCosts) -> bool {
    costs.fee_quote.is_finite()
        && costs.rebate_quote.is_finite()
        && costs.fee_quote >= 0.0
        && costs.rebate_quote >= 0.0
}

fn side_direction(side: OrderSide) -> f64 {
    match side {
        OrderSide::Buy => 1.0,
        OrderSide::Sell => -1.0,
    }
}

fn accrue_inventory_time(summary: &mut InventoryTimeSummary, position: f64, duration_ms: i64) {
    if duration_ms <= 0 {
        return;
    }
    if position.abs() > 1e-12 {
        summary.nonzero_ms += duration_ms;
    }
    summary.abs_qty_ms += position.abs() * duration_ms as f64;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fill(
        trade_id: u64,
        role: FillRole,
        side: OrderSide,
        price: f64,
        qty: f64,
        event_time_ms: i64,
    ) -> PerformanceFill {
        PerformanceFill {
            trade_id,
            order_id: trade_id + 100,
            role,
            side,
            price,
            qty,
            mark_at_fill: 100.0,
            event_time_ms,
            costs: Some(ExecutionCosts::default()),
        }
    }

    #[test]
    fn attributes_passive_and_exit_fills_and_conserves_net_pnl() {
        let mut ledger = PerformanceLedger::new(0.0, 100.0).unwrap();
        let mut passive = fill(1, FillRole::PassiveMaker, OrderSide::Buy, 99.0, 1.0, 0);
        passive.costs = Some(ExecutionCosts {
            fee_quote: 0.10,
            rebate_quote: 0.02,
        });
        assert!(ledger.record_fill(passive).unwrap());
        assert!(!ledger.record_fill(passive).unwrap(), "trade IDs dedupe");

        let exit = fill(
            2,
            FillRole::InventoryExit,
            OrderSide::Sell,
            101.0,
            1.0,
            10_000,
        );
        ledger.record_fill(exit).unwrap();
        ledger.record_funding(20_000, 0.25).unwrap();
        let summary = ledger.summary(100.0).unwrap();

        assert_eq!(summary.passive_fills, 1);
        assert_eq!(summary.exit_fills, 1);
        assert!((summary.passive_cashflow_quote + 99.0).abs() < 1e-12);
        assert!((summary.exit_cashflow_quote - 101.0).abs() < 1e-12);
        assert!((summary.passive_capture_bps.unwrap() - 100.0 / 99.0 * 100.0).abs() < 1e-12);
        assert!((summary.gross_spread_quote - 1.0).abs() < 1e-12);
        assert!((summary.exit_cost_quote + 1.0).abs() < 1e-12);
        assert!((summary.net_pnl_quote - 2.17).abs() < 1e-12);
        let recomposed =
            summary.gross_spread_quote + summary.inventory_mtm_change_quote + summary.rebate_quote
                - summary.fee_quote
                + summary.funding_quote
                - summary.exit_cost_quote;
        assert!((recomposed - summary.net_pnl_quote).abs() < 1e-12);
    }

    #[test]
    fn inventory_holding_time_uses_event_time_and_survives_late_arrival() {
        let mut ledger = PerformanceLedger::new(0.0, 100.0).unwrap();
        ledger.observe_market(0, 100.0).unwrap();
        ledger.observe_market(6_000, 100.0).unwrap();
        // Deliver the later sell first to prove the integral is reconstructed
        // from normalized event time rather than transport arrival order.
        ledger
            .record_fill(fill(
                2,
                FillRole::PassiveMaker,
                OrderSide::Sell,
                100.0,
                1.0,
                4_000,
            ))
            .unwrap();
        ledger
            .record_fill(fill(
                1,
                FillRole::PassiveMaker,
                OrderSide::Buy,
                100.0,
                2.0,
                1_000,
            ))
            .unwrap();
        ledger.finish(6_000).unwrap();

        let inventory = ledger.summary(100.0).unwrap().inventory_time;
        assert_eq!(inventory.observed_ms, 6_000);
        assert_eq!(inventory.nonzero_ms, 5_000);
        assert!((inventory.abs_qty_ms - 8_000.0).abs() < 1e-12);
        assert!((inventory.avg_abs_qty - 4.0 / 3.0).abs() < 1e-12);
    }

    #[test]
    fn late_rest_costs_enrich_a_ws_fill_once_and_conflicts_fail() {
        let mut ledger = PerformanceLedger::new(0.0, 100.0).unwrap();
        let mut ws_fill = fill(1, FillRole::PassiveMaker, OrderSide::Buy, 99.0, 1.0, 0);
        ws_fill.costs = None;
        ledger.record_fill(ws_fill).unwrap();
        assert_eq!(
            ledger.summary(100.0).unwrap().execution_costs_unavailable,
            1
        );

        let costs = ExecutionCosts {
            fee_quote: 0.10,
            rebate_quote: 0.0,
        };
        assert!(ledger.record_execution_costs(1, costs).unwrap());
        assert!(!ledger.record_execution_costs(1, costs).unwrap());
        assert_eq!(
            ledger.summary(100.0).unwrap().execution_costs_unavailable,
            0
        );
        assert!(matches!(
            ledger.record_execution_costs(
                1,
                ExecutionCosts {
                    fee_quote: 0.20,
                    rebate_quote: 0.0,
                }
            ),
            Err(PerformanceError::ConflictingExecutionCosts { trade_id: 1 })
        ));
    }

    #[test]
    fn missing_funding_is_explicit_and_zero_event_completes_attribution() {
        let mut ledger = PerformanceLedger::new(0.0, 100.0).unwrap();
        ledger
            .record_fill(fill(
                1,
                FillRole::PassiveMaker,
                OrderSide::Buy,
                100.0,
                1.0,
                0,
            ))
            .unwrap();
        let incomplete = ledger.summary(100.0).unwrap();
        assert!(!incomplete.funding_available);
        assert!(!incomplete.net_pnl_complete);

        ledger.record_funding(0, 0.0).unwrap();
        let complete = ledger.summary(100.0).unwrap();
        assert!(complete.funding_available);
        assert!(complete.net_pnl_complete);
    }

    #[test]
    fn resolves_markout_with_first_mark_at_or_after_each_horizon() {
        let mut ledger = PerformanceLedger::new(0.0, 100.0).unwrap();
        ledger
            .record_fill(fill(
                1,
                FillRole::PassiveMaker,
                OrderSide::Buy,
                100.0,
                2.0,
                1_000,
            ))
            .unwrap();
        ledger.observe_market(1_500, 100.2).unwrap();
        ledger.observe_market(2_000, 101.0).unwrap();
        ledger.observe_market(6_100, 99.0).unwrap();
        ledger.observe_market(31_000, 102.0).unwrap();
        let summary = ledger.summary(102.0).unwrap();

        assert_eq!(summary.markouts[0].samples, 1);
        assert_eq!(summary.markouts[1].samples, 1);
        assert_eq!(summary.markouts[2].samples, 1);
        assert!((summary.markouts[0].avg_bps.unwrap() - 100.0).abs() < 1e-12);
        assert!((summary.markouts[1].avg_bps.unwrap() + 100.0).abs() < 1e-12);
        assert!((summary.markouts[2].avg_bps.unwrap() - 200.0).abs() < 1e-12);
    }

    #[test]
    fn late_fill_uses_retained_market_history() {
        let mut ledger = PerformanceLedger::new(0.0, 100.0).unwrap();
        ledger.observe_market(2_000, 101.0).unwrap();
        ledger.observe_market(6_000, 99.0).unwrap();
        ledger
            .record_fill(fill(
                1,
                FillRole::PassiveMaker,
                OrderSide::Sell,
                100.0,
                1.0,
                1_000,
            ))
            .unwrap();
        let summary = ledger.summary(99.0).unwrap();
        assert!((summary.markouts[0].avg_bps.unwrap() + 100.0).abs() < 1e-12);
        assert!((summary.markouts[1].avg_bps.unwrap() - 100.0).abs() < 1e-12);
        assert_eq!(summary.markouts[2].pending, 1);
    }

    #[test]
    fn finish_marks_missing_horizons_unavailable_without_fabricating_marks() {
        let mut ledger = PerformanceLedger::new(0.0, 100.0).unwrap();
        ledger
            .record_fill(fill(
                1,
                FillRole::PassiveMaker,
                OrderSide::Buy,
                100.0,
                1.0,
                0,
            ))
            .unwrap();
        ledger.observe_market(1_000, 100.5).unwrap();
        ledger.finish(2_000).unwrap();
        let summary = ledger.summary(100.5).unwrap();
        assert_eq!(summary.markouts[0].samples, 1);
        assert_eq!(summary.markouts[1].unavailable, 1);
        assert_eq!(summary.markouts[2].unavailable, 1);
    }

    #[test]
    fn time_weights_two_sided_uptime_and_depth_integrals() {
        let mut ledger = PerformanceLedger::new(0.0, 100.0).unwrap();
        ledger
            .observe_quote_quality(QuoteQualityInterval {
                event_time_ms: 0,
                eligible_bid_qty: 2.0,
                eligible_ask_qty: 3.0,
            })
            .unwrap();
        ledger
            .observe_quote_quality(QuoteQualityInterval {
                event_time_ms: 1_000,
                eligible_bid_qty: 2.0,
                eligible_ask_qty: 0.0,
            })
            .unwrap();
        ledger.finish(3_000).unwrap();
        let summary = ledger.summary(100.0).unwrap().quote_time;

        assert_eq!(summary.observed_ms, 3_000);
        assert_eq!(summary.two_sided_ms, 1_000);
        assert!((summary.two_sided_uptime_pct - 100.0 / 3.0).abs() < 1e-12);
        assert!((summary.eligible_bid_qty_ms - 6_000.0).abs() < 1e-12);
        assert!((summary.eligible_ask_qty_ms - 3_000.0).abs() < 1e-12);
        assert!((summary.eligible_total_qty_ms - 9_000.0).abs() < 1e-12);
    }

    #[test]
    fn rejects_non_monotonic_typed_observations() {
        let mut ledger = PerformanceLedger::new(0.0, 100.0).unwrap();
        ledger.observe_market(2, 100.0).unwrap();
        assert!(matches!(
            ledger.observe_market(1, 100.0),
            Err(PerformanceError::MarketTimeRegression { .. })
        ));
        ledger
            .observe_quote_quality(QuoteQualityInterval {
                event_time_ms: 2,
                eligible_bid_qty: 1.0,
                eligible_ask_qty: 1.0,
            })
            .unwrap();
        assert!(matches!(
            ledger.observe_quote_quality(QuoteQualityInterval {
                event_time_ms: 1,
                eligible_bid_qty: 1.0,
                eligible_ask_qty: 1.0,
            }),
            Err(PerformanceError::QuoteTimeRegression { .. })
        ));
    }
}
