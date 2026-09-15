//! Quantity budgets for orders that can still execute at the venue.

use crate::{Action, MakerConfig};
use standx_sdk::models::OrderSide;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ExecutableExposure {
    buy_qty: f64,
    sell_qty: f64,
    invalid: bool,
}

impl ExecutableExposure {
    /// A cancel intent does not release its open quantity budget.
    pub fn reserve(&mut self, side: OrderSide, qty: f64) {
        if !qty.is_finite() || qty < 0.0 {
            self.invalid = true;
            return;
        }
        match side {
            OrderSide::Buy => self.buy_qty += qty,
            OrderSide::Sell => self.sell_qty += qty,
        }
    }

    /// Keep cancels/holds in order; admit placements only alongside all open
    /// venue exposure. Reserve admitted quantities before asynchronous writes.
    pub fn retain_safe_placements(
        mut self,
        cfg: &MakerConfig,
        position: f64,
        actions: &mut Vec<Action>,
    ) {
        actions.retain(|action| {
            let Action::Place(quote) = action else {
                return true;
            };
            let (reserved, limit) = match quote.side {
                OrderSide::Buy => (self.buy_qty, cfg.max_position - position),
                OrderSide::Sell => (self.sell_qty, cfg.max_position + position),
            };
            let allowed = !self.invalid
                && position.is_finite()
                && cfg.max_position.is_finite()
                && cfg.max_position > 0.0
                && quote.qty.is_finite()
                && quote.qty > 0.0
                && reserved.is_finite()
                && reserved + quote.qty <= limit + cfg.qty_tick() / 2.0;
            if allowed {
                self.reserve(quote.side, quote.qty);
            }
            allowed
        });
    }
}
