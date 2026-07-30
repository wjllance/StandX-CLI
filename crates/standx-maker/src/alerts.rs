use crate::stats::MakerStats;

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
