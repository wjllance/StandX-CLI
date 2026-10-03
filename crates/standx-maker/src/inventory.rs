use crate::{floor_to_decimals, round_to_decimals, MakerConfig};
use standx_sdk::models::OrderSide;
use std::error::Error;
use std::fmt;

/// Stage 3 v1: non-linear price-skew strength (`[nonlinear_skew]`).
///
/// Replaces v0's binary add-side suppression with a steeper — but never
/// stopping — center shift: `shift_bps = min(skew_bps × boost × |ratio|,
/// cap_bps)` where `ratio = position / max_position`. Stateless by design (no
/// hysteresis): v0's release-threshold latch pinned suppression for hours, so
/// v1 strength is a pure continuous function of the current position. With
/// `boost = 1` and `cap_bps ≥ skew_bps` the curve equals today's linear skew.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NonlinearSkewConfig {
    pub enabled: bool,
    /// Slope multiplier over the linear skew (≥ 1).
    pub boost: f64,
    /// Hard ceiling on the center shift in bps. Red line: the far-side quote
    /// sits at `spread_bps + shift` from mark, so `spread_bps + cap_bps` must
    /// stay inside `band_bps` (validated in [`Self::validate`]).
    pub cap_bps: f64,
}

impl Default for NonlinearSkewConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            boost: 3.0,
            cap_bps: 12.0,
        }
    }
}

impl NonlinearSkewConfig {
    /// Validate against the base strategy config. Invalid values are rejected
    /// even when disabled so a bad file never rides along silently.
    pub fn validate(&self, base: &MakerConfig) -> Result<(), SizeSkewError> {
        if !self.boost.is_finite() || !self.cap_bps.is_finite() {
            return Err(SizeSkewError::new("nonlinear skew values must be finite"));
        }
        if self.boost < 1.0 {
            return Err(SizeSkewError::new("nonlinear skew boost must be >= 1"));
        }
        if self.cap_bps <= 0.0 {
            return Err(SizeSkewError::new("nonlinear skew cap_bps must be > 0"));
        }
        if self.enabled {
            if !base.max_position.is_finite() || base.max_position <= 0.0 {
                return Err(SizeSkewError::new(
                    "enabled nonlinear skew requires positive finite max_position",
                ));
            }
            if base.spread_bps + self.cap_bps > base.band_bps {
                return Err(SizeSkewError::new(
                    "nonlinear skew violates band red line: spread_bps + cap_bps must be <= band_bps",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SizeSkewConfig {
    pub enabled: bool,
    pub activate_pct: f64,
    pub release_pct: f64,
    pub add_side_factor: f64,
}

impl Default for SizeSkewConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            activate_pct: 30.0,
            release_pct: 20.0,
            add_side_factor: 0.5,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SizeSkewDecision {
    pub enabled: bool,
    pub active: bool,
    pub add_side: Option<OrderSide>,
    pub inventory_ratio: f64,
    pub add_qty: Option<f64>,
}

impl SizeSkewDecision {
    pub const INACTIVE: Self = Self {
        enabled: false,
        active: false,
        add_side: None,
        inventory_ratio: 0.0,
        add_qty: None,
    };
}

impl Default for SizeSkewDecision {
    fn default() -> Self {
        Self::INACTIVE
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SizeSkewError(String);

impl SizeSkewError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for SizeSkewError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for SizeSkewError {}

#[derive(Clone, Debug)]
pub struct SizeSkewController {
    config: SizeSkewConfig,
    active: bool,
}

impl SizeSkewController {
    pub fn new(config: SizeSkewConfig, base: &MakerConfig) -> Result<Self, SizeSkewError> {
        if !config.activate_pct.is_finite()
            || !config.release_pct.is_finite()
            || !config.add_side_factor.is_finite()
        {
            return Err(SizeSkewError::new("size skew values must be finite"));
        }
        if config.release_pct <= 0.0
            || config.release_pct >= config.activate_pct
            || config.activate_pct > 100.0
        {
            return Err(SizeSkewError::new(
                "size skew thresholds must satisfy 0 < release_pct < activate_pct <= 100",
            ));
        }
        if config.add_side_factor < 0.0 || config.add_side_factor >= 1.0 {
            return Err(SizeSkewError::new(
                "size skew add_side_factor must satisfy 0 <= factor < 1",
            ));
        }
        if config.enabled && (!base.max_position.is_finite() || base.max_position <= 0.0) {
            return Err(SizeSkewError::new(
                "enabled size skew requires positive finite max_position",
            ));
        }

        Ok(Self {
            config,
            active: false,
        })
    }

    pub fn is_degenerate(&self, cfg: &MakerConfig) -> bool {
        if !self.config.enabled {
            return false;
        }
        let base = round_to_decimals(cfg.size, cfg.qty_decimals);
        let reduced = floor_to_decimals(base * self.config.add_side_factor, cfg.qty_decimals);
        reduced < cfg.min_order_qty || reduced <= 0.0
    }

    pub fn observe(&mut self, position: f64, cfg: &MakerConfig) -> SizeSkewDecision {
        if !self.config.enabled {
            self.active = false;
            return SizeSkewDecision::INACTIVE;
        }

        let inventory_ratio = (position.abs() / cfg.max_position).clamp(0.0, 1.0);
        let activate_ratio = self.config.activate_pct / 100.0;
        let release_ratio = self.config.release_pct / 100.0;
        if !self.active && inventory_ratio >= activate_ratio {
            self.active = true;
        } else if self.active && inventory_ratio < release_ratio {
            self.active = false;
        }

        let add_side = self.active.then_some({
            if position > 0.0 {
                OrderSide::Buy
            } else {
                OrderSide::Sell
            }
        });
        let base = round_to_decimals(cfg.size, cfg.qty_decimals);
        let reduced = floor_to_decimals(base * self.config.add_side_factor, cfg.qty_decimals);
        let add_qty = (reduced >= cfg.min_order_qty && reduced > 0.0).then_some(reduced);

        SizeSkewDecision {
            enabled: true,
            active: self.active,
            add_side,
            inventory_ratio,
            add_qty,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_config() -> MakerConfig {
        MakerConfig {
            spread_bps: 8.0,
            band_bps: 30.0,
            level_step_bps: 2.0,
            refresh_bps: 4.0,
            levels: 1,
            size: 0.02,
            max_position: 1.0,
            skew_bps: 8.0,
            price_decimals: 3,
            qty_decimals: 3,
            min_order_qty: 0.001,
            best_anchor: crate::BestAnchorConfig::default(),
        }
    }

    fn enabled_config() -> SizeSkewConfig {
        SizeSkewConfig {
            enabled: true,
            ..SizeSkewConfig::default()
        }
    }

    #[test]
    fn rejects_invalid_configuration_even_when_disabled() {
        let base = base_config();
        for config in [
            SizeSkewConfig {
                activate_pct: f64::NAN,
                ..SizeSkewConfig::default()
            },
            SizeSkewConfig {
                release_pct: f64::INFINITY,
                ..SizeSkewConfig::default()
            },
            SizeSkewConfig {
                add_side_factor: f64::NEG_INFINITY,
                ..SizeSkewConfig::default()
            },
            SizeSkewConfig {
                activate_pct: 20.0,
                release_pct: 20.0,
                ..SizeSkewConfig::default()
            },
            SizeSkewConfig {
                release_pct: 0.0,
                ..SizeSkewConfig::default()
            },
            SizeSkewConfig {
                activate_pct: 101.0,
                ..SizeSkewConfig::default()
            },
            SizeSkewConfig {
                add_side_factor: -0.1,
                ..SizeSkewConfig::default()
            },
            SizeSkewConfig {
                add_side_factor: 1.0,
                ..SizeSkewConfig::default()
            },
        ] {
            assert!(SizeSkewController::new(config, &base).is_err());
        }

        let mut invalid_base = base;
        invalid_base.max_position = 0.0;
        assert!(SizeSkewController::new(enabled_config(), &invalid_base).is_err());
        assert!(SizeSkewController::new(SizeSkewConfig::default(), &invalid_base).is_ok());
    }

    #[test]
    fn hysteresis_honors_exact_activation_and_release_boundaries() {
        let base = base_config();
        let mut controller = SizeSkewController::new(enabled_config(), &base).unwrap();

        assert!(!controller.observe(0.299, &base).active);
        assert!(controller.observe(0.3, &base).active);
        assert!(controller.observe(0.2, &base).active);
        assert!(!controller.observe(0.199, &base).active);
    }

    #[test]
    fn long_and_short_positions_are_mirrored() {
        let base = base_config();
        let mut long = SizeSkewController::new(enabled_config(), &base).unwrap();
        let mut short = SizeSkewController::new(enabled_config(), &base).unwrap();

        let long_decision = long.observe(0.3, &base);
        let short_decision = short.observe(-0.3, &base);
        assert_eq!(
            long_decision.inventory_ratio,
            short_decision.inventory_ratio
        );
        assert_eq!(long_decision.add_qty, short_decision.add_qty);
        assert_eq!(long_decision.add_side, Some(OrderSide::Buy));
        assert_eq!(short_decision.add_side, Some(OrderSide::Sell));
    }

    #[test]
    fn zero_position_releases_and_has_no_add_side() {
        let base = base_config();
        let mut controller = SizeSkewController::new(enabled_config(), &base).unwrap();
        assert!(controller.observe(0.4, &base).active);

        let decision = controller.observe(0.0, &base);
        assert!(!decision.active);
        assert_eq!(decision.add_side, None);
        assert_eq!(decision.inventory_ratio, 0.0);
    }

    #[test]
    fn active_direction_flip_keeps_state_and_follows_position_sign() {
        let base = base_config();
        let mut controller = SizeSkewController::new(enabled_config(), &base).unwrap();
        assert_eq!(
            controller.observe(0.4, &base).add_side,
            Some(OrderSide::Buy)
        );

        let flipped = controller.observe(-0.4, &base);
        assert!(flipped.active);
        assert_eq!(flipped.add_side, Some(OrderSide::Sell));
    }

    #[test]
    fn reduced_quantity_floors_to_tick_and_obeys_minimum() {
        let mut base = base_config();
        base.size = 0.021;
        base.min_order_qty = 0.01;
        let mut controller = SizeSkewController::new(enabled_config(), &base).unwrap();
        assert_eq!(controller.observe(0.3, &base).add_qty, Some(0.01));

        base.size = 0.019;
        let mut controller = SizeSkewController::new(enabled_config(), &base).unwrap();
        assert_eq!(controller.observe(0.3, &base).add_qty, None);

        base.size = 0.015;
        base.min_order_qty = 0.001;
        let mut controller = SizeSkewController::new(enabled_config(), &base).unwrap();
        let decision = controller.observe(0.3, &base);
        assert_eq!(decision.add_qty, Some(0.007));
        assert_eq!(round_to_decimals(0.015 * 0.5, 3), 0.008);
    }

    #[test]
    fn disabled_is_inactive_and_inventory_ratio_saturates_at_one() {
        let base = base_config();
        let mut disabled = SizeSkewController::new(SizeSkewConfig::default(), &base).unwrap();
        assert_eq!(disabled.observe(2.0, &base), SizeSkewDecision::INACTIVE);

        let mut enabled = SizeSkewController::new(enabled_config(), &base).unwrap();
        let decision = enabled.observe(2.0, &base);
        assert!(decision.active);
        assert_eq!(decision.inventory_ratio, 1.0);
    }

    #[test]
    fn nonlinear_skew_validation_rejects_bad_values_and_band_violation() {
        let base = base_config(); // spread 8, band 30
        for config in [
            NonlinearSkewConfig {
                boost: f64::NAN,
                ..NonlinearSkewConfig::default()
            },
            NonlinearSkewConfig {
                cap_bps: f64::INFINITY,
                ..NonlinearSkewConfig::default()
            },
            NonlinearSkewConfig {
                boost: 0.9,
                ..NonlinearSkewConfig::default()
            },
            NonlinearSkewConfig {
                cap_bps: 0.0,
                ..NonlinearSkewConfig::default()
            },
        ] {
            assert!(config.validate(&base).is_err(), "{config:?}");
        }

        // Band red line: spread(8) + cap must stay <= band(30).
        let too_wide = NonlinearSkewConfig {
            enabled: true,
            boost: 3.0,
            cap_bps: 22.1,
        };
        assert!(too_wide.validate(&base).is_err());
        // Same cap is fine while disabled (values still sanity-checked).
        let disabled_wide = NonlinearSkewConfig {
            enabled: false,
            ..too_wide
        };
        assert!(disabled_wide.validate(&base).is_ok());

        // Enabled requires a positive max_position.
        let mut no_max = base_config();
        no_max.max_position = 0.0;
        let enabled = NonlinearSkewConfig {
            enabled: true,
            ..NonlinearSkewConfig::default()
        };
        assert!(enabled.validate(&no_max).is_err());
        assert!(enabled.validate(&base).is_ok());
    }

    #[test]
    fn degenerate_detection_requires_enabled_and_below_minimum_quantity() {
        let mut base = base_config();
        base.size = 0.019;
        base.min_order_qty = 0.01;
        let enabled = SizeSkewController::new(enabled_config(), &base).unwrap();
        let disabled = SizeSkewController::new(SizeSkewConfig::default(), &base).unwrap();
        assert!(enabled.is_degenerate(&base));
        assert!(!disabled.is_degenerate(&base));

        base.size = 0.021;
        let enabled = SizeSkewController::new(enabled_config(), &base).unwrap();
        assert!(!enabled.is_degenerate(&base));
    }
}
