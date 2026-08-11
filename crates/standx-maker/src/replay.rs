//! Pure deterministic replay of normalized maker inputs.
//!
//! Trace parsing belongs to the CLI. This module accepts typed events, drives
//! the same preflight/planner functions as the live executor, and feeds the
//! performance ledger without reading a clock, environment, filesystem, or
//! network.

use crate::{
    plan_cycle, preflight_cycle_at, AdaptiveSpreadConfig, CyclePlan, CyclePreflight, MakerConfig,
    MarketSnapshot, PerformanceError, PerformanceFill, PerformanceLedger, PerformanceSummary,
    QuoteQualityInterval, RestingQuote, SizeSkewConfig, SizeSkewController, SizeSkewDecision,
    SpreadController, SpreadDecision, VolBreaker, VolatilityError,
};
use standx_sdk::models::OrderSide;
use std::fmt;

#[derive(Clone, Debug, PartialEq)]
pub struct ReplaySettings {
    pub starting_position: f64,
    pub starting_mark: f64,
    pub max_divergence_bps: f64,
    pub require_full_touch: bool,
    pub vol_window: usize,
    pub vol_window_secs: Option<u64>,
    pub vol_pause_bps: f64,
    pub adaptive_spread: AdaptiveSpreadConfig,
    pub size_skew: SizeSkewConfig,
    pub active_exit_enabled: bool,
    pub inventory_exit_pct: f64,
    pub inventory_exit_qty: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReplayCycle {
    pub event_time_ms: i64,
    pub cycle: u64,
    pub market: MarketSnapshot,
    pub position: f64,
    pub resting: Vec<RestingQuote>,
    pub pending_slots: Vec<(OrderSide, u32)>,
    pub eligible_bid_qty: f64,
    pub eligible_ask_qty: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ReplayEvent {
    Cycle(ReplayCycle),
    Fill(PerformanceFill),
    Funding {
        event_time_ms: i64,
        cashflow_quote: f64,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReplayCycleOutcome {
    pub event_time_ms: i64,
    pub cycle: u64,
    pub preflight: CyclePreflight,
    pub spread_decision: SpreadDecision,
    pub size_skew_decision: SizeSkewDecision,
    pub plan: Option<CyclePlan>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReplayResult {
    pub cycles: Vec<ReplayCycleOutcome>,
    pub performance: PerformanceSummary,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ReplayError {
    InvalidSettings(&'static str),
    Performance(PerformanceError),
    Volatility(VolatilityError),
    AdaptiveSpread(String),
    SizeSkew(String),
    MissingFinalMark,
}

impl fmt::Display for ReplayError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSettings(reason) => write!(formatter, "invalid replay settings: {reason}"),
            Self::Performance(error) => write!(formatter, "replay performance error: {error}"),
            Self::Volatility(error) => write!(formatter, "replay volatility error: {error}"),
            Self::AdaptiveSpread(error) => {
                write!(formatter, "invalid replay adaptive spread: {error}")
            }
            Self::SizeSkew(error) => write!(formatter, "invalid replay size skew: {error}"),
            Self::MissingFinalMark => formatter.write_str("replay has no final market mark"),
        }
    }
}

impl std::error::Error for ReplayError {}

impl From<PerformanceError> for ReplayError {
    fn from(value: PerformanceError) -> Self {
        Self::Performance(value)
    }
}

impl From<VolatilityError> for ReplayError {
    fn from(value: VolatilityError) -> Self {
        Self::Volatility(value)
    }
}

/// Replay normalized events in their supplied arrival order.
///
/// The end timestamp closes the last quote-quality interval. A caller should
/// normally use the trace's terminal lifecycle timestamp, which can be later
/// than the final market snapshot.
pub fn run_replay(
    cfg: &MakerConfig,
    settings: ReplaySettings,
    events: &[ReplayEvent],
    end_time_ms: i64,
) -> Result<ReplayResult, ReplayError> {
    validate_settings(&settings)?;
    let mut performance =
        PerformanceLedger::new(settings.starting_position, settings.starting_mark)?;
    let mut breaker = match settings.vol_window_secs {
        Some(seconds) => VolBreaker::new_duration(
            seconds
                .checked_mul(1_000)
                .ok_or(ReplayError::InvalidSettings("vol_window_secs is too large"))?,
            settings.vol_pause_bps,
        ),
        None => VolBreaker::new(settings.vol_window, settings.vol_pause_bps),
    };
    let mut spread_controller = SpreadController::new(settings.adaptive_spread.clone(), cfg)
        .map_err(|error| ReplayError::AdaptiveSpread(error.to_string()))?;
    let mut size_skew_controller = SizeSkewController::new(settings.size_skew, cfg)
        .map_err(|error| ReplayError::SizeSkew(error.to_string()))?;
    let mut outcomes = Vec::new();
    let mut final_mark = None;

    for event in events {
        match event {
            ReplayEvent::Cycle(cycle) => {
                performance.observe_market(cycle.event_time_ms, cycle.market.mark)?;
                performance.observe_quote_quality(QuoteQualityInterval {
                    event_time_ms: cycle.event_time_ms,
                    eligible_bid_qty: cycle.eligible_bid_qty,
                    eligible_ask_qty: cycle.eligible_ask_qty,
                })?;
                final_mark = Some(cycle.market.mark);
                let preflight = preflight_cycle_at(
                    &mut breaker,
                    cycle.event_time_ms,
                    cycle.market,
                    settings.max_divergence_bps,
                    settings.require_full_touch,
                )?;
                let spread_decision = spread_controller.observe(breaker.vol_bps(), cfg);
                let effective_cfg = spread_controller.effective_config(cfg, &spread_decision);
                let size_skew_decision =
                    size_skew_controller.observe(cycle.position, &effective_cfg);
                let plan = preflight.skip.is_none().then(|| {
                    plan_cycle(
                        &effective_cfg,
                        crate::CycleInput {
                            cycle: cycle.cycle,
                            market: cycle.market,
                            position: cycle.position,
                            resting: &cycle.resting,
                            pending_slots: &cycle.pending_slots,
                            market_data_mode: crate::MarketDataMode::Active,
                            active_exit_enabled: settings.active_exit_enabled,
                            inventory_exit_pct: settings.inventory_exit_pct,
                            inventory_exit_qty: settings.inventory_exit_qty,
                            size_skew: size_skew_decision,
                            nonlinear_skew: Default::default(),
                            external_skew: Default::default(),
                            external_excess_bps: None,
                            micro_price: Default::default(),
                            guard: Default::default(),
                            wind_down: false,
                            qty_tolerance: 0.0005,
                        },
                        preflight.halted,
                    )
                });
                outcomes.push(ReplayCycleOutcome {
                    event_time_ms: cycle.event_time_ms,
                    cycle: cycle.cycle,
                    preflight,
                    spread_decision,
                    size_skew_decision,
                    plan,
                });
            }
            ReplayEvent::Fill(fill) => {
                performance.record_fill(*fill)?;
            }
            ReplayEvent::Funding {
                event_time_ms,
                cashflow_quote,
            } => performance.record_funding(*event_time_ms, *cashflow_quote)?,
        }
    }
    performance.finish(end_time_ms)?;
    let final_mark = final_mark.ok_or(ReplayError::MissingFinalMark)?;
    Ok(ReplayResult {
        cycles: outcomes,
        performance: performance.summary(final_mark)?,
    })
}

fn validate_settings(settings: &ReplaySettings) -> Result<(), ReplayError> {
    if !settings.max_divergence_bps.is_finite() || settings.max_divergence_bps < 0.0 {
        return Err(ReplayError::InvalidSettings(
            "max_divergence_bps must be finite and non-negative",
        ));
    }
    if settings.vol_window_secs == Some(0) {
        return Err(ReplayError::InvalidSettings(
            "vol_window_secs must be positive",
        ));
    }
    if settings.vol_window_secs.is_none() && settings.vol_window == 0 {
        return Err(ReplayError::InvalidSettings("vol_window must be positive"));
    }
    if !settings.vol_pause_bps.is_finite()
        || !settings.inventory_exit_pct.is_finite()
        || !settings.inventory_exit_qty.is_finite()
    {
        return Err(ReplayError::InvalidSettings(
            "volatility and inventory-exit values must be finite",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ExecutionCosts, FillRole, RestingQuote, SpreadTier};

    fn config() -> MakerConfig {
        MakerConfig {
            spread_bps: 5.0,
            band_bps: 20.0,
            level_step_bps: 2.0,
            refresh_bps: 3.0,
            levels: 1,
            size: 1.0,
            max_position: 10.0,
            skew_bps: 0.0,
            price_decimals: 2,
            qty_decimals: 2,
            min_order_qty: 0.01,
        }
    }

    fn settings() -> ReplaySettings {
        ReplaySettings {
            starting_position: 0.0,
            starting_mark: 100.0,
            max_divergence_bps: 25.0,
            require_full_touch: true,
            vol_window: 12,
            vol_window_secs: None,
            vol_pause_bps: 0.0,
            adaptive_spread: AdaptiveSpreadConfig::default(),
            size_skew: SizeSkewConfig::default(),
            active_exit_enabled: false,
            inventory_exit_pct: 0.0,
            inventory_exit_qty: 0.0,
        }
    }

    fn cycle(event_time_ms: i64, cycle: u64, mark: f64) -> ReplayEvent {
        cycle_with_position(event_time_ms, cycle, mark, 0.0)
    }

    fn cycle_with_position(
        event_time_ms: i64,
        cycle: u64,
        mark: f64,
        position: f64,
    ) -> ReplayEvent {
        ReplayEvent::Cycle(ReplayCycle {
            event_time_ms,
            cycle,
            market: MarketSnapshot {
                mark,
                best_bid: Some(mark - 0.01),
                best_ask: Some(mark + 0.01),
            },
            position,
            resting: Vec::new(),
            pending_slots: Vec::new(),
            eligible_bid_qty: 1.0,
            eligible_ask_qty: 1.0,
        })
    }

    #[test]
    fn same_trace_replays_to_identical_plans_and_summary_three_times() {
        let events = vec![
            cycle_with_position(0, 0, 100.0, 4.0),
            ReplayEvent::Fill(PerformanceFill {
                trade_id: 1,
                order_id: 10,
                role: FillRole::PassiveMaker,
                side: OrderSide::Buy,
                price: 99.95,
                qty: 1.0,
                mark_at_fill: 100.0,
                event_time_ms: 0,
                costs: Some(ExecutionCosts::default()),
            }),
            cycle_with_position(1_000, 1, 100.1, 4.0),
            cycle_with_position(5_000, 2, 100.2, 4.0),
            cycle_with_position(30_000, 3, 100.3, 4.0),
        ];
        let mut replay_settings = settings();
        replay_settings.size_skew = SizeSkewConfig {
            enabled: true,
            ..SizeSkewConfig::default()
        };
        let first = run_replay(&config(), replay_settings.clone(), &events, 30_000).unwrap();
        let second = run_replay(&config(), replay_settings.clone(), &events, 30_000).unwrap();
        let third = run_replay(&config(), replay_settings, &events, 30_000).unwrap();

        assert_eq!(first, second);
        assert_eq!(second, third);
        assert_eq!(first.cycles.len(), 4);
        assert_eq!(first.performance.markouts[0].samples, 1);
        assert_eq!(first.performance.markouts[1].samples, 1);
        assert_eq!(first.performance.markouts[2].samples, 1);
        assert!(first
            .cycles
            .iter()
            .all(|cycle| cycle.size_skew_decision.active));
        assert!(first.cycles.iter().all(|cycle| {
            cycle.size_skew_decision.add_side == Some(OrderSide::Buy)
                && cycle.size_skew_decision.inventory_ratio == 0.4
                && cycle.size_skew_decision.add_qty == Some(0.5)
        }));
        // Fill events feed performance only. Position ownership/filtering is
        // resolved before replay produces the next normalized Cycle input.
        assert_eq!(
            first.cycles[0].size_skew_decision,
            first.cycles[1].size_skew_decision
        );
    }

    #[test]
    fn preflight_skip_has_no_plan_and_does_not_hide_the_observation() {
        let events = vec![ReplayEvent::Cycle(ReplayCycle {
            event_time_ms: 0,
            cycle: 0,
            market: MarketSnapshot {
                mark: 100.0,
                best_bid: Some(101.0),
                best_ask: Some(100.0),
            },
            position: 0.0,
            resting: vec![RestingQuote {
                order_id: Some("1".to_string()),
                side: OrderSide::Buy,
                level: 0,
                price: 99.0,
                qty: 1.0,
                ref_center: 100.0,
                placed_at_cycle: 0,
            }],
            pending_slots: Vec::new(),
            eligible_bid_qty: 1.0,
            eligible_ask_qty: 0.0,
        })];
        let result = run_replay(&config(), settings(), &events, 1_000).unwrap();
        assert!(result.cycles[0].preflight.skip.is_some());
        assert!(result.cycles[0].plan.is_none());
        assert_eq!(result.performance.quote_time.observed_ms, 1_000);
    }

    #[test]
    fn duration_window_adaptive_trace_is_deterministic_three_times() {
        let mut cfg = config();
        cfg.spread_bps = 8.0;
        cfg.refresh_bps = 4.0;
        cfg.band_bps = 30.0;
        let mut settings = settings();
        settings.vol_window_secs = Some(60);
        settings.adaptive_spread = AdaptiveSpreadConfig {
            enabled: true,
            min_spread_bps: 8.0,
            max_spread_bps: 18.0,
            tiers: vec![
                SpreadTier {
                    enter_vol_bps: None,
                    exit_vol_bps: None,
                    spread_bps: 8.0,
                    refresh_bps: 4.0,
                },
                SpreadTier {
                    enter_vol_bps: Some(10.0),
                    exit_vol_bps: Some(7.0),
                    spread_bps: 12.0,
                    refresh_bps: 5.0,
                },
            ],
        };
        let events = vec![cycle(0, 0, 100.0), cycle(1_000, 1, 100.11)];
        let first = run_replay(&cfg, settings.clone(), &events, 1_000).unwrap();
        let second = run_replay(&cfg, settings.clone(), &events, 1_000).unwrap();
        let third = run_replay(&cfg, settings, &events, 1_000).unwrap();

        assert_eq!(first, second);
        assert_eq!(second, third);
        assert_eq!(first.cycles[1].spread_decision.tier, 1);
        assert_eq!(first.cycles[1].spread_decision.effective_spread_bps, 12.0);
    }
}
