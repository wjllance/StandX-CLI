//! Continuous external-price center offset (`[external_skew]`).
//!
//! The caller passes the excess divergence already normalized by the external
//! guard chain. This module deliberately has no feed, clock, or guard-state
//! dependency: a missing or unusable sample simply produces no shift.

/// Operator configuration for the continuous external-price offset.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ExternalSkewConfig {
    pub enabled: bool,
    /// Offset coefficient applied before clamping (`shift = lambda * excess`).
    pub lambda: f64,
    /// Symmetric hard ceiling for the center offset, in basis points.
    pub cap_bps: f64,
    /// Excess magnitudes strictly below this threshold produce no offset.
    pub dead_zone_bps: f64,
}

impl Default for ExternalSkewConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            lambda: 0.5,
            cap_bps: 8.0,
            dead_zone_bps: 1.0,
        }
    }
}

/// Return the signed quote-center offset for this cycle, in basis points.
///
/// Failure is open: disabled configuration and missing or non-finite samples
/// all return zero, so the external signal can never become a stop-quoting
/// source. Freshness is normalized by the existing guard chain before this
/// pure function is called.
pub fn external_skew_shift_bps(cfg: ExternalSkewConfig, excess_bps: Option<f64>) -> f64 {
    if !cfg.enabled {
        return 0.0;
    }
    let Some(excess_bps) = excess_bps.filter(|value| value.is_finite()) else {
        return 0.0;
    };
    if excess_bps.abs() < cfg.dead_zone_bps {
        return 0.0;
    }
    (cfg.lambda * excess_bps).clamp(-cfg.cap_bps, cfg.cap_bps)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled_cfg() -> ExternalSkewConfig {
        ExternalSkewConfig {
            enabled: true,
            ..ExternalSkewConfig::default()
        }
    }

    #[test]
    fn defaults_to_disabled() {
        let cfg = ExternalSkewConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(
            external_skew_shift_bps(cfg, Some(40.0)).to_bits(),
            0.0_f64.to_bits()
        );
    }

    #[test]
    fn absent_or_non_finite_sample_fails_open() {
        let cfg = enabled_cfg();
        assert_eq!(external_skew_shift_bps(cfg, None), 0.0);
        assert_eq!(external_skew_shift_bps(cfg, Some(f64::NAN)), 0.0);
        assert_eq!(external_skew_shift_bps(cfg, Some(f64::INFINITY)), 0.0);
        assert_eq!(external_skew_shift_bps(cfg, Some(f64::NEG_INFINITY)), 0.0);
    }

    #[test]
    fn dead_zone_is_strict_and_boundary_engages() {
        let cfg = enabled_cfg();
        assert_eq!(external_skew_shift_bps(cfg, Some(0.999)), 0.0);
        assert_eq!(external_skew_shift_bps(cfg, Some(-0.999)), 0.0);
        assert_eq!(external_skew_shift_bps(cfg, Some(1.0)), 0.5);
        assert_eq!(external_skew_shift_bps(cfg, Some(-1.0)), -0.5);
    }

    #[test]
    fn shift_is_proportional_and_signed_like_excess() {
        let cfg = enabled_cfg();
        assert_eq!(external_skew_shift_bps(cfg, Some(4.0)), 2.0);
        assert_eq!(external_skew_shift_bps(cfg, Some(-4.0)), -2.0);
    }

    #[test]
    fn shift_clamps_symmetrically() {
        let cfg = enabled_cfg();
        assert_eq!(external_skew_shift_bps(cfg, Some(100.0)), 8.0);
        assert_eq!(external_skew_shift_bps(cfg, Some(-100.0)), -8.0);
    }

    #[test]
    fn zero_lambda_is_a_no_op() {
        let cfg = ExternalSkewConfig {
            lambda: 0.0,
            ..enabled_cfg()
        };
        assert_eq!(external_skew_shift_bps(cfg, Some(6.0)), 0.0);
    }
}
