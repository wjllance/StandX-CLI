//! Pure position-risk detection for maker sessions.

/// The existing gross session-MTM stop, evaluated before order effects.
/// Fees/funding deliberately remain outside this policy's historical basis.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SessionStopLoss {
    pub pnl: f64,
    pub limit: f64,
    pub mark: f64,
}

impl SessionStopLoss {
    pub fn check(
        stats: &crate::MakerStats,
        position: f64,
        mark: f64,
        limit: f64,
    ) -> Result<(), Self> {
        let pnl = stats.pnl(position, mark);
        if limit > 0.0 && pnl <= -limit {
            Err(Self { pnl, limit, mark })
        } else {
            Ok(())
        }
    }
}

impl std::fmt::Display for SessionStopLoss {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "session PnL {:+.2} <= -{:.2}", self.pnl, self.limit)
    }
}

impl std::error::Error for SessionStopLoss {}

/// The reason a position change requires a maker risk notification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PositionRiskKind {
    Jump,
    DirectionFlip,
    MaxPositionCrossed,
    InventoryExitCrossed,
}

/// A position change accepted by the position-risk detector.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PositionRiskEvent {
    pub kind: PositionRiskKind,
    pub before: f64,
    pub after: f64,
}

/// Tracks consecutive position observations independently from the
/// notification anchor, allowing small changes to accumulate without using a
/// stale alert anchor for direction and threshold-crossing decisions.
#[derive(Debug)]
pub struct PositionAlertAnchor {
    last_observed_position: f64,
    notification_anchor: f64,
    change_pct: f64,
    neutral_deadband: f64,
}

impl PositionAlertAnchor {
    pub fn new(position: f64, change_pct: f64, neutral_deadband: f64) -> Self {
        Self {
            last_observed_position: position,
            notification_anchor: position,
            change_pct,
            // An invalid deadband must not suppress genuine reversals. Runtime
            // configuration supplies a finite non-negative size-derived value,
            // while zero safely retains the strict sign-crossing fallback.
            neutral_deadband: if neutral_deadband.is_finite() && neutral_deadband >= 0.0 {
                neutral_deadband
            } else {
                0.0
            },
        }
    }

    /// Evaluate a position update without sending a notification.
    pub fn evaluate(
        &mut self,
        observed: f64,
        max_position: f64,
        inventory_exit_pct: f64,
        qty_tolerance: f64,
    ) -> Option<PositionRiskEvent> {
        let observed_before = self.last_observed_position;
        self.last_observed_position = observed;
        let notification_before = self.notification_anchor;
        let notification_delta = observed - notification_before;

        let jump_threshold = max_position * self.change_pct / 100.0;
        let jump = self.change_pct > 0.0
            && notification_delta.abs() > qty_tolerance
            && notification_delta.abs() + qty_tolerance >= jump_threshold;
        let direction_flip = (observed_before < -self.neutral_deadband
            && observed > self.neutral_deadband)
            || (observed_before > self.neutral_deadband && observed < -self.neutral_deadband);
        let crossed_max = observed_before.abs() + qty_tolerance < max_position
            && observed.abs() + qty_tolerance >= max_position;
        let exit_threshold = max_position * inventory_exit_pct / 100.0;
        let crossed_exit = inventory_exit_pct > 0.0
            && observed_before.abs() + qty_tolerance < exit_threshold
            && observed.abs() + qty_tolerance >= exit_threshold;

        let (kind, before) = if direction_flip {
            (PositionRiskKind::DirectionFlip, observed_before)
        } else if crossed_max {
            (PositionRiskKind::MaxPositionCrossed, observed_before)
        } else if crossed_exit {
            (PositionRiskKind::InventoryExitCrossed, observed_before)
        } else if jump {
            (PositionRiskKind::Jump, notification_before)
        } else {
            return None;
        };
        self.notification_anchor = observed;
        Some(PositionRiskEvent {
            kind,
            before,
            after: observed,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accumulates_changes_until_the_jump_threshold() {
        let mut anchor = PositionAlertAnchor::new(0.001, 20.0, 0.1);
        assert!(anchor.evaluate(0.10, 0.8, 25.0, 0.0005).is_none());
        let event = anchor.evaluate(0.161, 0.8, 25.0, 0.0005).unwrap();
        assert_eq!(event.kind, PositionRiskKind::Jump);
        assert!((event.after - event.before - 0.160).abs() < 1e-9);
        assert!(anchor.evaluate(0.161, 0.8, 25.0, 0.0005).is_none());
    }

    #[test]
    fn neutral_transitions_are_jumps_not_direction_flips() {
        let mut anchor = PositionAlertAnchor::new(-0.074, 20.0, 0.1);
        let neutral_to_long = anchor.evaluate(0.126, 0.8, 0.0, 0.0005).unwrap();
        assert_eq!(neutral_to_long.kind, PositionRiskKind::Jump);
        assert!((neutral_to_long.before - -0.074).abs() < 1e-9);

        let long_to_neutral = anchor.evaluate(-0.074, 0.8, 0.0, 0.0005).unwrap();
        assert_eq!(long_to_neutral.kind, PositionRiskKind::Jump);
        assert!((long_to_neutral.before - 0.126).abs() < 1e-9);
    }

    #[test]
    fn direction_flip_requires_consecutive_positions_beyond_the_deadband() {
        let mut direct = PositionAlertAnchor::new(-0.126, 0.0, 0.1);
        assert_eq!(
            direct.evaluate(0.126, 0.8, 0.0, 0.0005).unwrap().kind,
            PositionRiskKind::DirectionFlip
        );

        let mut through_neutral = PositionAlertAnchor::new(-0.126, 0.0, 0.1);
        assert!(through_neutral.evaluate(-0.074, 0.8, 0.0, 0.0005).is_none());
        assert!(through_neutral.evaluate(0.126, 0.8, 0.0, 0.0005).is_none());

        let mut boundary = PositionAlertAnchor::new(-0.1, 0.0, 0.1);
        assert!(boundary.evaluate(0.126, 0.8, 0.0, 0.0005).is_none());
    }

    #[test]
    fn threshold_crossings_use_the_last_observation_not_the_notification_anchor() {
        let mut exit = PositionAlertAnchor::new(0.3, 0.0, 0.1);
        assert!(exit.evaluate(0.19, 0.8, 25.0, 0.0005).is_none());
        assert_eq!(
            exit.evaluate(0.20, 0.8, 25.0, 0.0005).unwrap().kind,
            PositionRiskKind::InventoryExitCrossed
        );
    }
}

#[cfg(test)]
mod session_stop_loss_tests {
    use super::SessionStopLoss;
    use crate::MakerStats;

    #[test]
    fn stop_loss_preserves_gross_basis_inclusive_threshold_and_disabled_policy() {
        for (position, mark) in [(1.0, 90.0), (-1.0, 110.0)] {
            let stats = MakerStats::with_inventory_baseline(position, 100.0);
            let loss = SessionStopLoss::check(&stats, position, mark, 10.0).unwrap_err();
            assert_eq!(loss.pnl, -10.0);
            assert!(SessionStopLoss::check(&stats, position, mark, 10.01).is_ok());
            assert!(SessionStopLoss::check(&stats, position, mark, 0.0).is_ok());
            assert!(SessionStopLoss::check(&stats, position, 100.0, 10.0).is_ok());
        }
    }
}
