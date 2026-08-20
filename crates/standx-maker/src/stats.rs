use standx_sdk::models::OrderSide;

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
    ///
    /// Consequently the adopted position's [`Self::break_even`] is the
    /// **adoption mark**, not its historical entry price — `cash` is seeded
    /// as `-position * baseline_mark`, exactly as if the position had been
    /// bought/sold at `baseline_mark` at session start. This matches
    /// session-PnL semantics (PnL is zero at adoption, by construction), but
    /// an offline reader of `break_even`/`loss_bps` must not mistake it for
    /// the position's real acquisition cost. The two bases (session vs.
    /// account-level historical cost) must never be mixed — see the maker
    /// roadmap's frozen-terminology note (docs/18).
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

    /// Session break-even price for `position`: `-cash / position`.
    ///
    /// `None` when `position` is zero (no basis to divide by) or the result
    /// is non-finite. There is no per-position entry price in this
    /// accumulator — see the struct docs and [`Self::with_inventory_baseline`]
    /// for the adopted-position caveat: under that constructor this is the
    /// **adoption mark**, not historical cost.
    pub fn break_even(&self, position: f64) -> Option<f64> {
        if position == 0.0 {
            return None;
        }
        let break_even = -self.cash / position;
        break_even.is_finite().then_some(break_even)
    }

    /// Signed, direction-aware loss versus [`Self::break_even`], in **bps of
    /// mark**. Positive means losing (mark has moved against the held
    /// position); `None` when there is no basis (flat, or a non-finite /
    /// non-positive mark).
    ///
    /// The denominator is `mark`, not `break_even`, so this shares one bps
    /// convention with every other bps in the workspace (spread capture,
    /// `distance_to_touch_bps`, `mark_mid_divergence_bps`, the band and
    /// refresh thresholds). The two differ only to second order, but docs/18's
    /// frozen-terminology rule exists precisely so nobody has to guess which
    /// denominator a bps figure used.
    pub fn loss_bps(&self, position: f64, mark: f64) -> Option<f64> {
        let break_even = self.break_even(position)?;
        if !mark.is_finite() || mark <= 0.0 {
            return None;
        }
        let loss_bps = position.signum() * (break_even - mark) / mark * 10_000.0;
        loss_bps.is_finite().then_some(loss_bps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loss_bps_is_none_without_a_usable_mark_denominator() {
        let mut stats = MakerStats::default();
        stats.record_fill(OrderSide::Buy, 100.0, 1.0, 100.0);
        assert_eq!(stats.loss_bps(1.0, 0.0), None);
        assert_eq!(stats.loss_bps(1.0, -1.0), None);
        assert_eq!(stats.loss_bps(1.0, f64::NAN), None);
    }

    #[test]
    fn break_even_and_loss_bps_are_none_when_flat() {
        let stats = MakerStats::default();
        assert_eq!(stats.break_even(0.0), None);
        assert_eq!(stats.loss_bps(0.0, 100.0), None);
    }

    #[test]
    fn long_position_loses_when_mark_drops_below_break_even() {
        let mut stats = MakerStats::default();
        stats.record_fill(OrderSide::Buy, 100.0, 1.0, 100.0);
        assert_eq!(stats.break_even(1.0), Some(100.0));
        // Mark fell 1 unit below break-even: losing, so loss_bps is positive.
        // Denominator is mark (99), not break-even: 1/99 = 101.01bps.
        let loss = stats.loss_bps(1.0, 99.0).unwrap();
        assert!(loss > 0.0);
        assert!((loss - 101.010_101).abs() < 1e-5, "got {loss}");
        // Mark rose above break-even: not losing.
        let gain = stats.loss_bps(1.0, 101.0).unwrap();
        assert!(gain < 0.0);
    }

    #[test]
    fn short_position_loses_when_mark_rises_above_break_even() {
        let mut stats = MakerStats::default();
        stats.record_fill(OrderSide::Sell, 100.0, 1.0, 100.0);
        assert_eq!(stats.break_even(-1.0), Some(100.0));
        let loss = stats.loss_bps(-1.0, 101.0).unwrap();
        assert!(loss > 0.0);
        let gain = stats.loss_bps(-1.0, 99.0).unwrap();
        assert!(gain < 0.0);
    }

    #[test]
    fn adopted_inventory_break_even_is_the_adoption_mark_not_history() {
        // docs/33: under `with_inventory_baseline`, break_even reads back
        // exactly the mark the caller adopted at, never a real cost basis.
        let stats = MakerStats::with_inventory_baseline(2.0, 150.0);
        assert_eq!(stats.break_even(2.0), Some(150.0));
        assert_eq!(stats.loss_bps(2.0, 150.0), Some(0.0));
    }
}
