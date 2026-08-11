use crate::MakerConfig;
use std::collections::VecDeque;
use std::error::Error;
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VolatilityWindow {
    Samples(usize),
    DurationMs(u64),
}

impl VolatilityWindow {
    fn normalized(self) -> Self {
        match self {
            Self::Samples(samples) => Self::Samples(samples.max(1)),
            Self::DurationMs(duration_ms) => Self::DurationMs(duration_ms.max(1)),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum VolatilityError {
    InvalidMark(f64),
    NonMonotonicTimestamp { previous_ms: i64, current_ms: i64 },
}

impl fmt::Display for VolatilityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidMark(mark) => write!(f, "mark must be finite and positive, got {mark}"),
            Self::NonMonotonicTimestamp {
                previous_ms,
                current_ms,
            } => write!(
                f,
                "volatility timestamp moved backwards: previous={previous_ms} current={current_ms}"
            ),
        }
    }
}

impl Error for VolatilityError {}

#[derive(Clone, Copy, Debug)]
struct TimedMark {
    time_ms: i64,
    mark: f64,
}

#[derive(Clone, Debug)]
pub struct VolBreaker {
    marks: VecDeque<TimedMark>,
    window: VolatilityWindow,
    pause_bps: f64,
    rearm_bps: f64,
    halted: bool,
    last_vol_bps: f64,
    last_time_ms: Option<i64>,
    next_sample_time_ms: i64,
}

impl VolBreaker {
    pub fn new(window: usize, pause_bps: f64) -> Self {
        Self::with_window(VolatilityWindow::Samples(window), pause_bps)
    }

    pub fn new_duration(window_ms: u64, pause_bps: f64) -> Self {
        Self::with_window(VolatilityWindow::DurationMs(window_ms), pause_bps)
    }

    pub fn with_window(window: VolatilityWindow, pause_bps: f64) -> Self {
        Self {
            marks: VecDeque::new(),
            window: window.normalized(),
            pause_bps,
            rearm_bps: pause_bps * 0.5,
            halted: false,
            last_vol_bps: 0.0,
            last_time_ms: None,
            next_sample_time_ms: 0,
        }
    }

    pub fn window(&self) -> VolatilityWindow {
        self.window
    }

    pub fn enabled(&self) -> bool {
        self.pause_bps > 0.0
    }

    pub fn observe(&mut self, mark: f64) -> bool {
        if !mark.is_finite() || mark <= 0.0 {
            return false;
        }
        let time_ms = self.next_sample_time_ms;
        self.next_sample_time_ms = self.next_sample_time_ms.saturating_add(1);
        self.observe_validated(time_ms, mark)
    }

    pub fn observe_at(&mut self, time_ms: i64, mark: f64) -> Result<bool, VolatilityError> {
        if !mark.is_finite() || mark <= 0.0 {
            return Err(VolatilityError::InvalidMark(mark));
        }
        if matches!(self.window, VolatilityWindow::DurationMs(_)) {
            if let Some(previous_ms) = self.last_time_ms {
                if time_ms < previous_ms {
                    return Err(VolatilityError::NonMonotonicTimestamp {
                        previous_ms,
                        current_ms: time_ms,
                    });
                }
            }
            self.last_time_ms = Some(time_ms);
        }
        Ok(self.observe_validated(time_ms, mark))
    }

    fn observe_validated(&mut self, time_ms: i64, mark: f64) -> bool {
        self.marks.push_back(TimedMark { time_ms, mark });
        match self.window {
            VolatilityWindow::Samples(window) => {
                while self.marks.len() > window {
                    self.marks.pop_front();
                }
            }
            VolatilityWindow::DurationMs(window_ms) => {
                let cutoff = time_ms.saturating_sub(window_ms.min(i64::MAX as u64) as i64);
                while self
                    .marks
                    .front()
                    .is_some_and(|sample| sample.time_ms < cutoff)
                {
                    self.marks.pop_front();
                }
            }
        }

        let (min, max) = self
            .marks
            .iter()
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), sample| {
                (lo.min(sample.mark), hi.max(sample.mark))
            });
        self.last_vol_bps = if min.is_finite() && min > 0.0 {
            (max - min) / min * 10_000.0
        } else {
            0.0
        };

        if self.pause_bps > 0.0 {
            if !self.halted && self.last_vol_bps >= self.pause_bps {
                self.halted = true;
            } else if self.halted && self.last_vol_bps < self.rearm_bps {
                self.halted = false;
            }
        } else {
            self.halted = false;
        }
        self.halted
    }

    pub fn is_halted(&self) -> bool {
        self.halted
    }

    pub fn halted(&self) -> bool {
        self.halted
    }

    pub fn vol_bps(&self) -> f64 {
        self.last_vol_bps
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SpreadTier {
    pub enter_vol_bps: Option<f64>,
    pub exit_vol_bps: Option<f64>,
    pub spread_bps: f64,
    pub refresh_bps: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AdaptiveSpreadConfig {
    pub enabled: bool,
    pub min_spread_bps: f64,
    pub max_spread_bps: f64,
    pub tiers: Vec<SpreadTier>,
}

impl Default for AdaptiveSpreadConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            min_spread_bps: 0.0,
            max_spread_bps: 0.0,
            tiers: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SpreadDecision {
    pub enabled: bool,
    pub tier: usize,
    pub rolling_vol_bps: f64,
    pub effective_spread_bps: f64,
    pub effective_refresh_bps: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AdaptiveSpreadError(String);

impl AdaptiveSpreadError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for AdaptiveSpreadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for AdaptiveSpreadError {}

#[derive(Clone, Debug)]
pub struct SpreadController {
    config: AdaptiveSpreadConfig,
    current_tier: usize,
}

impl SpreadController {
    pub fn new(
        config: AdaptiveSpreadConfig,
        base: &MakerConfig,
    ) -> Result<Self, AdaptiveSpreadError> {
        if config.tiers.is_empty() {
            if config.enabled {
                return Err(AdaptiveSpreadError::new(
                    "adaptive spread requires 2 or 3 tiers",
                ));
            }
            return Ok(Self {
                config,
                current_tier: 0,
            });
        }
        if !(2..=3).contains(&config.tiers.len()) {
            return Err(AdaptiveSpreadError::new(
                "adaptive spread requires 2 or 3 tiers",
            ));
        }
        if !config.min_spread_bps.is_finite()
            || !config.max_spread_bps.is_finite()
            || config.min_spread_bps <= 0.0
            || config.max_spread_bps < config.min_spread_bps
        {
            return Err(AdaptiveSpreadError::new(
                "adaptive spread bounds must be finite, positive, and ordered",
            ));
        }

        let mut previous_enter = 0.0;
        let mut previous_spread = 0.0;
        let mut previous_refresh = 0.0;
        for (index, tier) in config.tiers.iter().enumerate() {
            if !tier.spread_bps.is_finite()
                || !tier.refresh_bps.is_finite()
                || tier.spread_bps < config.min_spread_bps
                || tier.spread_bps > config.max_spread_bps
                || tier.spread_bps >= base.band_bps
                || tier.refresh_bps < 0.0
                || tier.refresh_bps >= tier.spread_bps
            {
                return Err(AdaptiveSpreadError::new(format!(
                    "adaptive tier {index} has invalid spread/refresh geometry"
                )));
            }
            if index == 0 {
                if tier.enter_vol_bps.is_some() || tier.exit_vol_bps.is_some() {
                    return Err(AdaptiveSpreadError::new(
                        "adaptive base tier must not define enter/exit thresholds",
                    ));
                }
                if (tier.spread_bps - base.spread_bps).abs() > f64::EPSILON
                    || (tier.refresh_bps - base.refresh_bps).abs() > f64::EPSILON
                {
                    return Err(AdaptiveSpreadError::new(
                        "adaptive base tier must match base spread_bps/refresh_bps",
                    ));
                }
            } else {
                let enter = tier.enter_vol_bps.ok_or_else(|| {
                    AdaptiveSpreadError::new(format!(
                        "adaptive tier {index} is missing enter_vol_bps"
                    ))
                })?;
                let exit = tier.exit_vol_bps.ok_or_else(|| {
                    AdaptiveSpreadError::new(format!(
                        "adaptive tier {index} is missing exit_vol_bps"
                    ))
                })?;
                if !enter.is_finite()
                    || !exit.is_finite()
                    || enter <= previous_enter
                    || exit < 0.0
                    || exit >= enter
                {
                    return Err(AdaptiveSpreadError::new(format!(
                        "adaptive tier {index} has invalid hysteresis thresholds"
                    )));
                }
                previous_enter = enter;
            }
            if index > 0
                && (tier.spread_bps < previous_spread || tier.refresh_bps < previous_refresh)
            {
                return Err(AdaptiveSpreadError::new(
                    "adaptive tier spread/refresh must be non-decreasing",
                ));
            }
            previous_spread = tier.spread_bps;
            previous_refresh = tier.refresh_bps;
        }

        Ok(Self {
            config,
            current_tier: 0,
        })
    }

    pub fn observe(&mut self, rolling_vol_bps: f64, base: &MakerConfig) -> SpreadDecision {
        if !self.config.enabled || self.config.tiers.is_empty() {
            return SpreadDecision {
                enabled: false,
                tier: 0,
                rolling_vol_bps,
                effective_spread_bps: base.spread_bps,
                effective_refresh_bps: base.refresh_bps,
            };
        }

        for index in (self.current_tier + 1)..self.config.tiers.len() {
            if rolling_vol_bps
                >= self.config.tiers[index]
                    .enter_vol_bps
                    .unwrap_or(f64::INFINITY)
            {
                self.current_tier = index;
            }
        }
        while self.current_tier > 0 {
            let exit = self.config.tiers[self.current_tier]
                .exit_vol_bps
                .unwrap_or(0.0);
            if rolling_vol_bps < exit {
                self.current_tier -= 1;
            } else {
                break;
            }
        }

        let tier = &self.config.tiers[self.current_tier];
        SpreadDecision {
            enabled: true,
            tier: self.current_tier,
            rolling_vol_bps,
            effective_spread_bps: tier
                .spread_bps
                .clamp(self.config.min_spread_bps, self.config.max_spread_bps),
            effective_refresh_bps: tier.refresh_bps,
        }
    }

    pub fn effective_config(&self, base: &MakerConfig, decision: &SpreadDecision) -> MakerConfig {
        let mut effective = base.clone();
        effective.spread_bps = decision.effective_spread_bps;
        effective.refresh_bps = decision.effective_refresh_bps;
        effective
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{plan_cycle, Action, CycleInput, MarketDataMode, MarketSnapshot, RestingQuote};
    use standx_sdk::models::OrderSide;

    fn base_config() -> MakerConfig {
        MakerConfig {
            spread_bps: 8.0,
            band_bps: 30.0,
            level_step_bps: 2.0,
            refresh_bps: 4.0,
            levels: 1,
            size: 0.01,
            max_position: 0.2,
            skew_bps: 8.0,
            price_decimals: 3,
            qty_decimals: 2,
            min_order_qty: 0.01,
        }
    }

    fn adaptive_config(enabled: bool) -> AdaptiveSpreadConfig {
        AdaptiveSpreadConfig {
            enabled,
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
                SpreadTier {
                    enter_vol_bps: Some(20.0),
                    exit_vol_bps: Some(15.0),
                    spread_bps: 18.0,
                    refresh_bps: 6.0,
                },
            ],
        }
    }

    #[test]
    fn duration_window_evicts_by_time_not_sample_count() {
        let mut breaker = VolBreaker::new_duration(60_000, 0.0);
        breaker.observe_at(0, 100.0).unwrap();
        breaker.observe_at(59_000, 101.0).unwrap();
        assert!((breaker.vol_bps() - 100.0).abs() < 1e-9);

        breaker.observe_at(60_001, 101.0).unwrap();
        assert_eq!(breaker.vol_bps(), 0.0);
    }

    #[test]
    fn duration_window_rejects_time_travel_without_mutating_value() {
        let mut breaker = VolBreaker::new_duration(60_000, 0.0);
        breaker.observe_at(1_000, 100.0).unwrap();
        let err = breaker.observe_at(999, 101.0).unwrap_err();
        assert!(matches!(err, VolatilityError::NonMonotonicTimestamp { .. }));
        assert_eq!(breaker.vol_bps(), 0.0);
    }

    #[test]
    fn controller_uses_hysteresis_and_can_jump_to_high() {
        let base = base_config();
        let mut controller = SpreadController::new(adaptive_config(true), &base).unwrap();

        assert_eq!(controller.observe(20.0, &base).tier, 2);
        assert_eq!(controller.observe(15.0, &base).tier, 2);
        assert_eq!(controller.observe(14.9, &base).tier, 1);
        assert_eq!(controller.observe(7.0, &base).tier, 1);
        assert_eq!(controller.observe(6.9, &base).tier, 0);
    }

    #[test]
    fn disabled_controller_is_exactly_base_geometry() {
        let base = base_config();
        let mut controller = SpreadController::new(adaptive_config(false), &base).unwrap();
        let decision = controller.observe(100.0, &base);
        assert!(!decision.enabled);
        assert_eq!(decision.effective_spread_bps, base.spread_bps);
        assert_eq!(decision.effective_refresh_bps, base.refresh_bps);
    }

    #[test]
    fn invalid_threshold_order_is_rejected() {
        let base = base_config();
        let mut config = adaptive_config(true);
        config.tiers[2].exit_vol_bps = Some(21.0);
        assert!(SpreadController::new(config, &base).is_err());
    }

    #[test]
    fn widening_does_not_cancel_existing_narrow_quotes_before_refresh() {
        let base = base_config();
        let mut controller = SpreadController::new(adaptive_config(true), &base).unwrap();
        let decision = controller.observe(20.0, &base);
        let effective = controller.effective_config(&base, &decision);
        let resting = [
            RestingQuote {
                order_id: Some("bid".to_string()),
                side: OrderSide::Buy,
                level: 0,
                price: 99.92,
                qty: 0.01,
                ref_center: 100.0,
                placed_at_cycle: 1,
            },
            RestingQuote {
                order_id: Some("ask".to_string()),
                side: OrderSide::Sell,
                level: 0,
                price: 100.08,
                qty: 0.01,
                ref_center: 100.0,
                placed_at_cycle: 1,
            },
        ];
        let plan = plan_cycle(
            &effective,
            CycleInput {
                cycle: 2,
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
                external_skew: Default::default(),
                external_excess_bps: None,
                micro_price: Default::default(),
                guard: Default::default(),
                wind_down: false,
                qty_tolerance: 0.0005,
            },
            false,
        );
        assert!(plan
            .actions
            .iter()
            .all(|action| matches!(action, Action::Hold { .. })));
    }
}
