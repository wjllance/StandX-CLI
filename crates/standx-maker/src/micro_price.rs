//! Continuous in-venue touch-mid center offset (`[microprice]`).
//!
//! The signal is the venue's book touch-mid relative to the StandX mark anchor
//! at quote time: `mid_bias_bps = ((best_bid + best_ask)/2 - mark) / mark`.
//! Offline attribution across real stage-2 A/B arms shows this field (filled
//! rho ≈ -0.53 vs markout@30s) is nearly monotone: fills taken while the in-venue
//! touch sits above our mark anchor carry strongly negative 30s markout, while
//! fills while it sits below are net positive. Unlike `[external_skew]` (an
//! *external* leader feed), this anchors against the venue's own book and shares
//! the load of the in-venue (non-external) toxicity component the leader feed
//! cannot see.
//!
//! The module deliberately has no feed, clock, or guard-state dependency: the
//! caller passes the already-normalized scalar. A missing or non-finite input
//! simply produces no shift (failure is open). Default-disabled so an untouched
//! config is byte-for-byte replay equivalent.

/// Operator configuration for the in-venue touch-mid center offset.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MicroPriceConfig {
    pub enabled: bool,
    /// Offset coefficient applied before clamping (`shift = lambda * mid_bias`).
    pub lambda: f64,
    /// Symmetric hard ceiling for the center offset, in basis points.
    pub cap_bps: f64,
    /// Mid-bias magnitudes strictly below this threshold produce no offset.
    pub dead_zone_bps: f64,
}

impl Default for MicroPriceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            lambda: 0.5,
            cap_bps: 6.0,
            dead_zone_bps: 0.5,
        }
    }
}

/// Return the signed quote-center offset for this cycle, in basis points.
///
/// Failure is open: disabled or invalid configuration and missing/non-finite
/// inputs all return zero, so this signal can never become a stop-quoting source.
/// `mid_bias_bps` is the signed in-venue touch-mid minus the mark anchor, in
/// bps of mark. A negative bias (book sitting below our anchor) yields a
/// negative shift; positive bias a positive shift, so the ladder follows the
/// venue's own screen rather than a possibly stale StandX mark.
pub fn micro_price_shift_bps(cfg: MicroPriceConfig, mid_bias_bps: Option<f64>) -> f64 {
    if !cfg.enabled {
        return 0.0;
    }
    if !cfg.lambda.is_finite()
        || cfg.lambda < 0.0
        || !cfg.cap_bps.is_finite()
        || cfg.cap_bps <= 0.0
        || !cfg.dead_zone_bps.is_finite()
        || cfg.dead_zone_bps < 0.0
    {
        return 0.0;
    }
    let Some(mid_bias_bps) = mid_bias_bps.filter(|value| value.is_finite()) else {
        return 0.0;
    };
    if mid_bias_bps.abs() < cfg.dead_zone_bps {
        return 0.0;
    }
    (cfg.lambda * mid_bias_bps).clamp(-cfg.cap_bps, cfg.cap_bps)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled_cfg() -> MicroPriceConfig {
        MicroPriceConfig {
            enabled: true,
            ..MicroPriceConfig::default()
        }
    }

    #[test]
    fn defaults_to_disabled() {
        let cfg = MicroPriceConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(
            micro_price_shift_bps(cfg, Some(40.0)).to_bits(),
            0.0_f64.to_bits()
        );
    }

    #[test]
    fn absent_or_non_finite_sample_fails_open() {
        let cfg = enabled_cfg();
        assert_eq!(micro_price_shift_bps(cfg, None), 0.0);
        assert_eq!(micro_price_shift_bps(cfg, Some(f64::NAN)), 0.0);
        assert_eq!(micro_price_shift_bps(cfg, Some(f64::INFINITY)), 0.0);
        assert_eq!(micro_price_shift_bps(cfg, Some(f64::NEG_INFINITY)), 0.0);
    }

    #[test]
    fn invalid_config_fails_open_without_panicking() {
        for cfg in [
            MicroPriceConfig {
                lambda: f64::NAN,
                ..enabled_cfg()
            },
            MicroPriceConfig {
                lambda: f64::INFINITY,
                ..enabled_cfg()
            },
            MicroPriceConfig {
                lambda: -0.5,
                ..enabled_cfg()
            },
            MicroPriceConfig {
                cap_bps: f64::NAN,
                ..enabled_cfg()
            },
            MicroPriceConfig {
                cap_bps: f64::INFINITY,
                ..enabled_cfg()
            },
            MicroPriceConfig {
                cap_bps: -1.0,
                ..enabled_cfg()
            },
            MicroPriceConfig {
                dead_zone_bps: f64::NAN,
                ..enabled_cfg()
            },
            MicroPriceConfig {
                dead_zone_bps: f64::INFINITY,
                ..enabled_cfg()
            },
            MicroPriceConfig {
                dead_zone_bps: -1.0,
                ..enabled_cfg()
            },
        ] {
            assert_eq!(micro_price_shift_bps(cfg, Some(4.0)), 0.0);
        }
    }

    #[test]
    fn dead_zone_is_strict_and_boundary_engages() {
        let cfg = enabled_cfg();
        assert_eq!(micro_price_shift_bps(cfg, Some(0.49)), 0.0);
        assert_eq!(micro_price_shift_bps(cfg, Some(-0.49)), 0.0);
        assert_eq!(micro_price_shift_bps(cfg, Some(0.5)), 0.25);
        assert_eq!(micro_price_shift_bps(cfg, Some(-0.5)), -0.25);
    }

    #[test]
    fn shift_is_proportional_and_signed_like_mid_bias() {
        let cfg = enabled_cfg();
        assert_eq!(micro_price_shift_bps(cfg, Some(4.0)), 2.0);
        assert_eq!(micro_price_shift_bps(cfg, Some(-4.0)), -2.0);
    }

    #[test]
    fn shift_clamps_symmetrically() {
        let cfg = enabled_cfg();
        assert_eq!(micro_price_shift_bps(cfg, Some(100.0)), 6.0);
        assert_eq!(micro_price_shift_bps(cfg, Some(-100.0)), -6.0);
    }

    #[test]
    fn zero_lambda_is_a_no_op() {
        let cfg = MicroPriceConfig {
            lambda: 0.0,
            ..enabled_cfg()
        };
        assert_eq!(micro_price_shift_bps(cfg, Some(6.0)), 0.0);
    }
}
