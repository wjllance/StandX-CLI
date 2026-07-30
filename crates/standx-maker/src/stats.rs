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
