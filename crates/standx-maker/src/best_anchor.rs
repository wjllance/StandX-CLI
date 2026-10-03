//! Best-anchor quote mode (OKR v2 item 3.1①). Default off.
//!
//! When `enabled` is false this module is not consulted for prices, cancels,
//! or exits. When it is on, each side is priced from its own touch:
//!
//! ```text
//! stand_off = max(best_jump_p999, one_tick) + margin
//! buy  = best_bid - stand_off(level)
//! sell = best_ask + stand_off(level)
//! ```
//!
//! Mark is not that anchor. It only defines the existing eligibility band
//! (`mark * (1 ± band_bps)`). Inventory, external, and microprice shifts still
//! multiply the touch price by `quote_center / mark`, then the stand-off is
//! re-applied as a hard minimum so those shifts cannot pull a side inside it.
//!
//! The 1-second best-jump p99.9 is an operator-supplied price distance. This
//! module does not estimate it and does not open a market-data feed.

use standx_sdk::models::OrderSide;

use crate::{
    ceil_to_decimals, floor_to_decimals, quote_center, quote_geometry, round_to_decimals,
    DesiredQuote, DesiredQuotesWithGeometry, GuardDecision, MakerConfig, NonlinearSkewConfig,
    QuoteGeometryOutcome, SizeSkewDecision,
};

/// Operator configuration for `[best_anchor]`. Absent and `enabled = false`
/// are the same state: the legacy mark ladder.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BestAnchorConfig {
    pub enabled: bool,
    /// Absolute 1-second best-price jump, p99.9, in price units. Not computed
    /// here. Zero is legal and means "no measured jump above one tick".
    pub best_jump_p999: f64,
    /// Extra price distance added after `max(best_jump_p999, one tick)`.
    pub margin: f64,
}

impl Default for BestAnchorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            best_jump_p999: 0.0,
            margin: 0.0,
        }
    }
}

impl BestAnchorConfig {
    /// Reject non-finite or negative distances even while disabled, so a bad
    /// file cannot sit unnoticed until the flag is turned on.
    pub fn validate(self) -> Result<(), &'static str> {
        if !self.best_jump_p999.is_finite() || self.best_jump_p999 < 0.0 {
            return Err("best_anchor best_jump_p999 must be finite and >= 0");
        }
        if !self.margin.is_finite() || self.margin < 0.0 {
            return Err("best_anchor margin must be finite and >= 0");
        }
        Ok(())
    }
}

/// Stand-off distance in price units when the mode is enabled and the
/// distances are finite. `None` when the mode is off, or when an enabled
/// config is invalid (the caller must then quote nothing rather than fall
/// back to the mark ladder).
pub fn stand_off_price(cfg: &MakerConfig) -> Option<f64> {
    if !cfg.best_anchor.enabled {
        return None;
    }
    if cfg.best_anchor.validate().is_err() {
        return None;
    }
    let tick = cfg.price_tick();
    if !tick.is_finite() || tick <= 0.0 {
        return None;
    }
    Some(cfg.best_anchor.best_jump_p999.max(tick) + cfg.best_anchor.margin)
}

/// Book mid used as the shared anti-flicker anchor while the mode is on.
/// Both sides store this same value, so a mid drift refreshes them together.
pub(crate) fn book_mid(best_bid: Option<f64>, best_ask: Option<f64>) -> Option<f64> {
    match (best_bid, best_ask) {
        (Some(bid), Some(ask))
            if bid.is_finite() && ask.is_finite() && bid > 0.0 && ask > 0.0 && bid < ask =>
        {
            Some((bid + ask) / 2.0)
        }
        _ => None,
    }
}

/// Whether one resting quote is strictly closer to its touch than this level's
/// stand-off. Level 0 uses `max(jump, tick) + margin`; outer levels add
/// `level_step_bps`.
pub(crate) fn resting_price_inside_standoff(
    cfg: &MakerConfig,
    side: OrderSide,
    level: u32,
    price: f64,
    best_bid: Option<f64>,
    best_ask: Option<f64>,
) -> bool {
    let (Some(standoff), Some(bid), Some(ask)) = (stand_off_price(cfg), best_bid, best_ask) else {
        return false;
    };
    if !bid.is_finite() || !ask.is_finite() || bid <= 0.0 || ask <= 0.0 || bid >= ask {
        return false;
    }
    let distance = level_distance(standoff, side, bid, ask, level, cfg.level_step_bps);
    price_inside_standoff(
        side,
        price,
        best_bid,
        best_ask,
        distance,
        cfg.price_tick() * 1e-6,
    )
}

/// Whether a resting price is strictly closer to its touch than `standoff`.
pub(crate) fn price_inside_standoff(
    side: OrderSide,
    price: f64,
    best_bid: Option<f64>,
    best_ask: Option<f64>,
    standoff: f64,
    tolerance: f64,
) -> bool {
    match side {
        OrderSide::Buy => best_bid.is_some_and(|bid| price > bid - standoff + tolerance),
        OrderSide::Sell => best_ask.is_some_and(|ask| price < ask + standoff - tolerance),
    }
}

/// Desired ladder while best-anchor is enabled. The disabled path never calls
/// this. A missing or crossed touch quotes nothing: one side must not fall
/// back to the mark while the other anchors to the book.
#[allow(clippy::too_many_arguments)]
pub(crate) fn compute_quotes(
    cfg: &MakerConfig,
    mark: f64,
    best_bid: Option<f64>,
    best_ask: Option<f64>,
    position: f64,
    size_skew: SizeSkewDecision,
    nonlinear_skew: NonlinearSkewConfig,
    external_shift_bps: f64,
    guard: GuardDecision,
) -> DesiredQuotesWithGeometry {
    let mut result = DesiredQuotesWithGeometry::default();
    if !mark.is_finite() || mark <= 0.0 {
        return result;
    }

    let band_lo = mark * (1.0 - cfg.band_bps / 1e4);
    let band_hi = mark * (1.0 + cfg.band_bps / 1e4);
    let Some(standoff) = stand_off_price(cfg) else {
        push_all(
            &mut result,
            cfg,
            mark,
            best_bid,
            best_ask,
            band_lo,
            band_hi,
            None,
            QuoteGeometryOutcome::DroppedInfeasible,
        );
        return result;
    };
    let Some((bid, ask)) = best_bid.zip(best_ask).filter(|(bid, ask)| {
        bid.is_finite() && ask.is_finite() && *bid > 0.0 && *ask > 0.0 && bid < ask
    }) else {
        push_all(
            &mut result,
            cfg,
            mark,
            best_bid,
            best_ask,
            band_lo,
            band_hi,
            None,
            QuoteGeometryOutcome::DroppedInfeasible,
        );
        return result;
    };

    // Same relative shift the mark ladder would have applied (inventory, then
    // external + microprice). Mark stays the denominator of that shift and the
    // band; it is not the price the quotes are built around. A non-positive
    // factor means the composed center is unusable, so quote nothing.
    let shift_factor = quote_center(cfg, nonlinear_skew, external_shift_bps, mark, position) / mark;
    if !shift_factor.is_finite() || shift_factor <= 0.0 {
        push_all(
            &mut result,
            cfg,
            mark,
            best_bid,
            best_ask,
            band_lo,
            band_hi,
            Some((bid, ask, standoff, 1.0)),
            QuoteGeometryOutcome::DroppedInfeasible,
        );
        return result;
    }

    let qty = round_to_decimals(cfg.size, cfg.qty_decimals);
    if qty < cfg.min_order_qty || qty <= 0.0 {
        push_all(
            &mut result,
            cfg,
            mark,
            best_bid,
            best_ask,
            band_lo,
            band_hi,
            Some((bid, ask, standoff, shift_factor)),
            QuoteGeometryOutcome::DroppedBelowMinQty,
        );
        return result;
    }

    let suppress_buy = position >= cfg.max_position;
    let suppress_sell = position <= -cfg.max_position;
    let touch = Some((bid, ask, standoff, shift_factor));

    for side in [OrderSide::Buy, OrderSide::Sell] {
        if (side == OrderSide::Buy && suppress_buy) || (side == OrderSide::Sell && suppress_sell) {
            push_side(
                &mut result,
                cfg,
                mark,
                best_bid,
                best_ask,
                band_lo,
                band_hi,
                touch,
                side,
                QuoteGeometryOutcome::SuppressedPosition,
            );
            continue;
        }
        // External guard stays one-sided. This mode does not turn it off.
        if guard.active && guard.endangered == Some(side) {
            push_side(
                &mut result,
                cfg,
                mark,
                best_bid,
                best_ask,
                band_lo,
                band_hi,
                touch,
                side,
                QuoteGeometryOutcome::SuppressedGuard,
            );
            continue;
        }
        let side_qty = if size_skew.active && size_skew.add_side == Some(side) {
            let Some(add_qty) = size_skew.add_qty else {
                continue;
            };
            add_qty
        } else {
            qty
        };
        let mut last_price: Option<f64> = None;
        for level in 0..cfg.levels {
            let raw_price = raw_price(
                side,
                level,
                bid,
                ask,
                standoff,
                cfg.level_step_bps,
                shift_factor,
            );
            let Some((lo, hi, lower_is_touch, upper_is_touch)) =
                feasible_interval(side, bid, ask, standoff, cfg, level, band_lo, band_hi)
            else {
                result.geometry.push(quote_geometry(
                    side,
                    level,
                    raw_price,
                    None,
                    QuoteGeometryOutcome::DroppedInfeasible,
                    mark,
                    best_bid,
                    best_ask,
                    band_lo,
                    band_hi,
                ));
                continue;
            };

            let tick = cfg.price_tick();
            let tolerance = tick * 1e-6;
            let mut outcome = if raw_price < lo {
                if lower_is_touch {
                    QuoteGeometryOutcome::ClampedToTouch
                } else {
                    QuoteGeometryOutcome::ClampedToBand
                }
            } else if raw_price > hi {
                if upper_is_touch {
                    QuoteGeometryOutcome::ClampedToTouch
                } else {
                    QuoteGeometryOutcome::ClampedToBand
                }
            } else {
                QuoteGeometryOutcome::Placed
            };
            let mut price = raw_price.clamp(lo, hi);
            price = match side {
                OrderSide::Buy => floor_to_decimals(price, cfg.price_decimals),
                OrderSide::Sell => ceil_to_decimals(price, cfg.price_decimals),
            };
            if price < lo {
                outcome = if lower_is_touch {
                    QuoteGeometryOutcome::ClampedToTouch
                } else {
                    QuoteGeometryOutcome::ClampedToBand
                };
                price = ceil_to_decimals(lo, cfg.price_decimals);
            } else if price > hi {
                outcome = if upper_is_touch {
                    QuoteGeometryOutcome::ClampedToTouch
                } else {
                    QuoteGeometryOutcome::ClampedToBand
                };
                price = floor_to_decimals(hi, cfg.price_decimals);
            }

            if !price.is_finite()
                || price <= 0.0
                || price < lo - tolerance
                || price > hi + tolerance
                || price_inside_standoff(
                    side,
                    price,
                    best_bid,
                    best_ask,
                    level_distance(standoff, side, bid, ask, level, cfg.level_step_bps),
                    tolerance,
                )
                || best_ask.is_some_and(|ask| side == OrderSide::Buy && price >= ask)
                || best_bid.is_some_and(|bid| side == OrderSide::Sell && price <= bid)
            {
                result.geometry.push(quote_geometry(
                    side,
                    level,
                    raw_price,
                    None,
                    QuoteGeometryOutcome::DroppedInfeasible,
                    mark,
                    best_bid,
                    best_ask,
                    band_lo,
                    band_hi,
                ));
                continue;
            }

            if last_price == Some(price) {
                result.geometry.push(quote_geometry(
                    side,
                    level,
                    raw_price,
                    None,
                    QuoteGeometryOutcome::DroppedDuplicate,
                    mark,
                    best_bid,
                    best_ask,
                    band_lo,
                    band_hi,
                ));
                continue;
            }
            last_price = Some(price);
            result.geometry.push(quote_geometry(
                side,
                level,
                raw_price,
                Some(price),
                outcome,
                mark,
                best_bid,
                best_ask,
                band_lo,
                band_hi,
            ));
            result.quotes.push(DesiredQuote {
                side,
                level,
                price,
                qty: side_qty,
            });
        }
    }

    // Stand-off/band infeasibility is paired per level. If level N cannot sit
    // outside its stand-off and inside the mark band, level N on the other
    // side comes out with it. Other levels that both fit stay. A side removed
    // by max-position or the external guard is left one-sided: those policies
    // are unchanged by this mode. Exposure caps run later and may still drop
    // one side's outer level on budget.
    drop_unpaired_anchor_level(&mut result);
    result
}

fn drop_unpaired_anchor_level(result: &mut DesiredQuotesWithGeometry) {
    let mut levels: Vec<u32> = result.quotes.iter().map(|quote| quote.level).collect();
    levels.sort_unstable();
    levels.dedup();
    let mut drop_slots = Vec::new();
    for level in levels {
        let buy_quoted = result
            .quotes
            .iter()
            .any(|quote| quote.side == OrderSide::Buy && quote.level == level);
        let sell_quoted = result
            .quotes
            .iter()
            .any(|quote| quote.side == OrderSide::Sell && quote.level == level);
        if buy_quoted == sell_quoted {
            continue;
        }
        let missing = if buy_quoted {
            OrderSide::Sell
        } else {
            OrderSide::Buy
        };
        let mut attempted = false;
        let mut suppressed = false;
        for row in result
            .geometry
            .iter()
            .filter(|row| row.side == missing && row.level == level)
        {
            attempted = true;
            if matches!(
                row.outcome,
                QuoteGeometryOutcome::SuppressedPosition | QuoteGeometryOutcome::SuppressedGuard
            ) {
                suppressed = true;
            }
        }
        // No row at this level means the side was skipped before pricing
        // (size-skew with no add qty). That is not a stand-off decision.
        if !attempted || suppressed {
            continue;
        }
        let live = if buy_quoted {
            OrderSide::Buy
        } else {
            OrderSide::Sell
        };
        drop_slots.push((live, level));
    }
    result.quotes.retain(|quote| {
        !drop_slots
            .iter()
            .any(|(side, level)| quote.side == *side && quote.level == *level)
    });
    for row in result.geometry.iter_mut() {
        if row.final_price.is_none() {
            continue;
        }
        if drop_slots
            .iter()
            .any(|(side, level)| row.side == *side && row.level == *level)
        {
            row.outcome = QuoteGeometryOutcome::DroppedInfeasible;
            row.final_price = None;
            row.distance_to_touch_bps = None;
        }
    }
}

type Touch = (f64, f64, f64, f64);

fn raw_price(
    side: OrderSide,
    level: u32,
    bid: f64,
    ask: f64,
    standoff: f64,
    level_step_bps: f64,
    shift_factor: f64,
) -> f64 {
    let distance = level_distance(standoff, side, bid, ask, level, level_step_bps);
    let unshifted = match side {
        OrderSide::Buy => bid - distance,
        OrderSide::Sell => ask + distance,
    };
    unshifted * shift_factor
}

fn level_distance(
    standoff: f64,
    side: OrderSide,
    bid: f64,
    ask: f64,
    level: u32,
    level_step_bps: f64,
) -> f64 {
    let touch = match side {
        OrderSide::Buy => bid,
        OrderSide::Sell => ask,
    };
    standoff + level as f64 * touch * level_step_bps / 1e4
}

/// Feasible price interval: inside the mark band, at least `stand-off` away
/// from this side's touch, and not crossing the opposite touch.
///
/// Returns `None` when the band and the stand-off do not overlap. The quote
/// is dropped rather than clamped into the stand-off. That is the property
/// that keeps "inside the band" from pulling a quote onto the touch.
#[allow(clippy::too_many_arguments)]
fn feasible_interval(
    side: OrderSide,
    bid: f64,
    ask: f64,
    standoff: f64,
    cfg: &MakerConfig,
    level: u32,
    band_lo: f64,
    band_hi: f64,
) -> Option<(f64, f64, bool, bool)> {
    let tick = cfg.price_tick();
    let distance = level_distance(standoff, side, bid, ask, level, cfg.level_step_bps);
    let tolerance = tick * 1e-6;
    let (lo, hi, lower_is_touch, upper_is_touch) = match side {
        OrderSide::Buy => {
            let stand_hi = bid - distance;
            let cross_hi = ask - tick;
            let hi = stand_hi.min(cross_hi).min(band_hi);
            let upper_is_touch = hi < band_hi - tolerance;
            (band_lo, hi, false, upper_is_touch)
        }
        OrderSide::Sell => {
            let stand_lo = ask + distance;
            let cross_lo = bid + tick;
            let lo = stand_lo.max(cross_lo).max(band_lo);
            let lower_is_touch = lo > band_lo + tolerance;
            (lo, band_hi, lower_is_touch, false)
        }
    };
    if !lo.is_finite() || !hi.is_finite() || lo > hi + tolerance {
        None
    } else {
        Some((lo, hi, lower_is_touch, upper_is_touch))
    }
}

#[allow(clippy::too_many_arguments)]
fn push_all(
    result: &mut DesiredQuotesWithGeometry,
    cfg: &MakerConfig,
    mark: f64,
    best_bid: Option<f64>,
    best_ask: Option<f64>,
    band_lo: f64,
    band_hi: f64,
    touch: Option<Touch>,
    outcome: QuoteGeometryOutcome,
) {
    for side in [OrderSide::Buy, OrderSide::Sell] {
        push_side(
            result, cfg, mark, best_bid, best_ask, band_lo, band_hi, touch, side, outcome,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn push_side(
    result: &mut DesiredQuotesWithGeometry,
    cfg: &MakerConfig,
    mark: f64,
    best_bid: Option<f64>,
    best_ask: Option<f64>,
    band_lo: f64,
    band_hi: f64,
    touch: Option<Touch>,
    side: OrderSide,
    outcome: QuoteGeometryOutcome,
) {
    for level in 0..cfg.levels {
        let raw_price = match touch {
            Some((bid, ask, standoff, shift_factor)) => raw_price(
                side,
                level,
                bid,
                ask,
                standoff,
                cfg.level_step_bps,
                shift_factor,
            ),
            None => mark,
        };
        result.geometry.push(quote_geometry(
            side, level, raw_price, None, outcome, mark, best_bid, best_ask, band_lo, band_hi,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        plan_cycle, Action, CancelReason, CycleInput, DesiredQuote, ExitKind, GuardDecision,
        InventoryExit, MarketDataMode, MarketSnapshot, NonlinearSkewConfig, OrderSide,
        RestingQuote, SizeSkewDecision,
    };

    fn cfg() -> MakerConfig {
        MakerConfig {
            spread_bps: 10.0,
            band_bps: 20.0,
            level_step_bps: 2.0,
            refresh_bps: 3.0,
            levels: 1,
            size: 0.01,
            max_position: 0.05,
            skew_bps: 0.0,
            price_decimals: 2,
            qty_decimals: 4,
            min_order_qty: 0.001,
            best_anchor: BestAnchorConfig::default(),
        }
    }

    fn integer_cfg() -> MakerConfig {
        let mut cfg = cfg();
        cfg.price_decimals = 0;
        cfg.band_bps = 20.0;
        cfg
    }

    fn enabled(mut cfg: MakerConfig, jump: f64, margin: f64) -> MakerConfig {
        cfg.best_anchor = BestAnchorConfig {
            enabled: true,
            best_jump_p999: jump,
            margin,
        };
        cfg
    }

    fn disabled_wild() -> MakerConfig {
        let mut cfg = cfg();
        cfg.best_anchor = BestAnchorConfig {
            enabled: false,
            best_jump_p999: 50.0,
            margin: 25.0,
        };
        cfg
    }

    fn market(mark: f64, bid: f64, ask: f64) -> MarketSnapshot {
        MarketSnapshot {
            mark,
            best_bid: Some(bid),
            best_ask: Some(ask),
        }
    }

    fn input<'a>(
        market: MarketSnapshot,
        position: f64,
        resting: &'a [RestingQuote],
        guard: GuardDecision,
        nonlinear: NonlinearSkewConfig,
    ) -> CycleInput<'a> {
        CycleInput {
            cycle: 1,
            market,
            position,
            resting,
            pending_slots: &[],
            market_data_mode: MarketDataMode::Active,
            active_exit_enabled: true,
            inventory_exit_pct: 80.0,
            inventory_exit_qty: 0.01,
            size_skew: SizeSkewDecision::INACTIVE,
            nonlinear_skew: nonlinear,
            external_skew: Default::default(),
            external_excess_bps: None,
            micro_price: Default::default(),
            guard,
            wind_down: false,
            qty_tolerance: 0.0005,
        }
    }

    fn places(plan: &crate::CyclePlan) -> Vec<&DesiredQuote> {
        plan.actions
            .iter()
            .filter_map(|action| match action {
                Action::Place(quote) => Some(quote),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn default_is_off_and_disabled_wild_distances_match_legacy_quotes() {
        assert!(!BestAnchorConfig::default().enabled);
        assert!(stand_off_price(&cfg()).is_none());
        assert!(stand_off_price(&disabled_wild()).is_none());

        let snapshot = market(100.0, 99.99, 100.01);
        let off = plan_cycle(
            &cfg(),
            input(
                snapshot,
                0.0,
                &[],
                GuardDecision::INACTIVE,
                NonlinearSkewConfig::default(),
            ),
            false,
        );
        let wild = plan_cycle(
            &disabled_wild(),
            input(
                snapshot,
                0.0,
                &[],
                GuardDecision::INACTIVE,
                NonlinearSkewConfig::default(),
            ),
            false,
        );
        assert_eq!(off, wild);
        let quoted = places(&off);
        assert_eq!(quoted.len(), 2);
        assert_eq!(quoted[0].price, 99.90);
        assert_eq!(quoted[1].price, 100.10);
        assert_eq!(off.ref_center, 100.0);
        assert!(off.inventory_exit.is_none());
    }

    #[test]
    fn disabled_matches_legacy_across_cancel_hold_and_exit() {
        let snapshot = market(100.0, 99.99, 100.01);
        let resting = [
            RestingQuote {
                order_id: Some("1".into()),
                side: OrderSide::Buy,
                level: 0,
                price: 99.90,
                qty: 0.01,
                ref_center: 100.0,
                placed_at_cycle: 0,
            },
            RestingQuote {
                order_id: Some("2".into()),
                side: OrderSide::Sell,
                level: 0,
                price: 100.10,
                qty: 0.01,
                ref_center: 100.0,
                placed_at_cycle: 0,
            },
        ];
        let hold_market = market(100.02, 100.0, 100.04);
        let refresh_market = market(100.50, 100.48, 100.52);
        let exit_position = 0.04;

        for (label, snapshot, position, halted) in [
            ("hold", hold_market, 0.0, false),
            ("refresh", refresh_market, 0.0, false),
            ("exit", snapshot, exit_position, false),
            ("halt", snapshot, exit_position, true),
        ] {
            let legacy = plan_cycle(
                &cfg(),
                input(
                    snapshot,
                    position,
                    &resting,
                    GuardDecision::INACTIVE,
                    NonlinearSkewConfig::default(),
                ),
                halted,
            );
            let wild = plan_cycle(
                &disabled_wild(),
                input(
                    snapshot,
                    position,
                    &resting,
                    GuardDecision::INACTIVE,
                    NonlinearSkewConfig::default(),
                ),
                halted,
            );
            assert_eq!(legacy.actions, wild.actions, "{label} actions");
            assert_eq!(legacy.inventory_exit, wild.inventory_exit, "{label} exit");
            assert_eq!(
                legacy.requested_inventory_exit, wild.requested_inventory_exit,
                "{label} requested exit"
            );
            assert_eq!(
                legacy.exit_suppression, wild.exit_suppression,
                "{label} suppression"
            );
            assert_eq!(legacy.ref_center, wild.ref_center, "{label} ref");
        }

        let exit = plan_cycle(
            &disabled_wild(),
            input(
                snapshot,
                exit_position,
                &[],
                GuardDecision::INACTIVE,
                NonlinearSkewConfig::default(),
            ),
            false,
        );
        assert_eq!(
            exit.inventory_exit,
            Some(InventoryExit {
                side: OrderSide::Sell,
                qty: 0.01,
                kind: ExitKind::InventoryTrim,
            })
        );
        assert!(exit
            .actions
            .iter()
            .all(|action| !matches!(action, Action::Place(_))));
    }

    #[test]
    fn enabled_anchors_both_sides_to_touch_inside_the_mark_band() {
        let cfg = enabled(integer_cfg(), 3.0, 1.0);
        assert_eq!(stand_off_price(&cfg), Some(4.0));
        // Mid is 10003, not the mark. ref_center must follow the book.
        let plan = plan_cycle(
            &cfg,
            input(
                market(10_000.0, 9_998.0, 10_008.0),
                0.0,
                &[],
                GuardDecision::INACTIVE,
                NonlinearSkewConfig::default(),
            ),
            false,
        );
        let quoted = places(&plan);
        assert_eq!(quoted.len(), 2);
        assert_eq!(
            quoted[0],
            &DesiredQuote {
                side: OrderSide::Buy,
                level: 0,
                price: 9_994.0,
                qty: 0.01,
            }
        );
        assert_eq!(
            quoted[1],
            &DesiredQuote {
                side: OrderSide::Sell,
                level: 0,
                price: 10_012.0,
                qty: 0.01,
            }
        );
        assert_eq!(plan.ref_center, 10_003.0);
        for quote in &quoted {
            assert!((9_980.0..=10_020.0).contains(&quote.price));
        }
        // Mark ladder would have been 9990 / 10010. The touch anchor is not that.
        assert_ne!(quoted[0].price, 9_990.0);
        assert_ne!(quoted[1].price, 10_010.0);
    }

    #[test]
    fn stand_off_is_max_of_jump_and_tick_plus_margin() {
        let mut cfg = integer_cfg();
        cfg.best_anchor.enabled = true;
        cfg.best_anchor.best_jump_p999 = 0.0;
        cfg.best_anchor.margin = 0.0;
        assert_eq!(stand_off_price(&cfg), Some(1.0));

        cfg.best_anchor.best_jump_p999 = 0.4;
        cfg.best_anchor.margin = 2.0;
        assert_eq!(stand_off_price(&cfg), Some(3.0));

        cfg.price_decimals = 2;
        cfg.best_anchor.best_jump_p999 = 0.001;
        cfg.best_anchor.margin = 0.02;
        assert_eq!(stand_off_price(&cfg), Some(0.03));
    }

    #[test]
    fn one_side_outside_the_band_takes_the_other_side_out() {
        let cfg = enabled(integer_cfg(), 4.0, 1.0);
        assert_eq!(stand_off_price(&cfg), Some(5.0));
        let plan = plan_cycle(
            &cfg,
            input(
                market(10_000.0, 10_010.0, 10_018.0),
                0.0,
                &[],
                GuardDecision::INACTIVE,
                NonlinearSkewConfig::default(),
            ),
            false,
        );
        assert!(
            places(&plan).is_empty(),
            "sell stand-off is outside the band, so the bid comes out too"
        );
        assert!(plan.quote_geometry.iter().all(|row| {
            row.final_price.is_none() && row.outcome == QuoteGeometryOutcome::DroppedInfeasible
        }));
    }

    #[test]
    fn outer_level_outside_the_band_drops_only_that_level_on_both_sides() {
        // Side-wide pairing would keep Buy L1 after Sell L1 falls outside the
        // band. The touch then leaves one-sided outer liquidity.
        let mut cfg = enabled(cfg(), 0.04, 0.01);
        cfg.levels = 2;
        cfg.level_step_bps = 10.0;
        cfg.max_position = 1.0;
        cfg.band_bps = 20.0;
        let plan = plan_cycle(
            &cfg,
            input(
                market(100.0, 100.05, 100.10),
                0.0,
                &[],
                GuardDecision::INACTIVE,
                NonlinearSkewConfig::default(),
            ),
            false,
        );
        let quoted = places(&plan);
        assert_eq!(
            quoted
                .iter()
                .map(|quote| (quote.side, quote.level, quote.price))
                .collect::<Vec<_>>(),
            vec![(OrderSide::Buy, 0, 100.00), (OrderSide::Sell, 0, 100.15),]
        );
        assert!(plan.quote_geometry.iter().any(|row| {
            row.side == OrderSide::Buy
                && row.level == 1
                && row.final_price.is_none()
                && row.outcome == QuoteGeometryOutcome::DroppedInfeasible
        }));
        assert!(plan.quote_geometry.iter().any(|row| {
            row.side == OrderSide::Sell
                && row.level == 1
                && row.final_price.is_none()
                && row.outcome == QuoteGeometryOutcome::DroppedInfeasible
        }));
    }

    #[test]
    fn resting_unpaired_outer_level_cancels_on_both_sides() {
        let mut cfg = enabled(cfg(), 0.04, 0.01);
        cfg.levels = 2;
        cfg.level_step_bps = 10.0;
        cfg.max_position = 1.0;
        cfg.band_bps = 20.0;
        let resting = [
            RestingQuote {
                order_id: Some("b0".into()),
                side: OrderSide::Buy,
                level: 0,
                price: 100.00,
                qty: 0.01,
                ref_center: 100.075,
                placed_at_cycle: 0,
            },
            RestingQuote {
                order_id: Some("b1".into()),
                side: OrderSide::Buy,
                level: 1,
                price: 99.89,
                qty: 0.01,
                ref_center: 100.075,
                placed_at_cycle: 0,
            },
            RestingQuote {
                order_id: Some("s0".into()),
                side: OrderSide::Sell,
                level: 0,
                price: 100.15,
                qty: 0.01,
                ref_center: 100.075,
                placed_at_cycle: 0,
            },
            RestingQuote {
                order_id: Some("s1".into()),
                side: OrderSide::Sell,
                level: 1,
                price: 100.19,
                qty: 0.01,
                ref_center: 100.075,
                placed_at_cycle: 0,
            },
        ];
        let plan = plan_cycle(
            &cfg,
            input(
                market(100.0, 100.05, 100.10),
                0.0,
                &resting,
                GuardDecision::INACTIVE,
                NonlinearSkewConfig::default(),
            ),
            false,
        );
        let mut cancels: Vec<_> = plan
            .actions
            .iter()
            .filter_map(|action| match action {
                Action::Cancel { side, level, .. } => Some((*side, *level)),
                _ => None,
            })
            .collect();
        cancels.sort_by_key(|(side, level)| (*level, *side as u8));
        assert_eq!(cancels, vec![(OrderSide::Buy, 1), (OrderSide::Sell, 1)]);
        let holds: Vec<_> = plan
            .actions
            .iter()
            .filter_map(|action| match action {
                Action::Hold { side, level, .. } => Some((*side, *level)),
                _ => None,
            })
            .collect();
        assert_eq!(holds, vec![(OrderSide::Buy, 0), (OrderSide::Sell, 0)]);
    }

    #[test]
    fn guard_still_suppresses_only_the_endangered_side() {
        let cfg = enabled(integer_cfg(), 3.0, 1.0);
        let plan = plan_cycle(
            &cfg,
            input(
                market(10_000.0, 9_995.0, 10_005.0),
                0.0,
                &[],
                GuardDecision {
                    enabled: true,
                    active: true,
                    endangered: Some(OrderSide::Sell),
                    divergence_bps: Some(12.0),
                },
                NonlinearSkewConfig::default(),
            ),
            false,
        );
        let quoted = places(&plan);
        assert_eq!(quoted.len(), 1);
        assert_eq!(quoted[0].side, OrderSide::Buy);
        assert_eq!(quoted[0].price, 9_991.0);
        assert!(plan.quote_geometry.iter().any(|row| {
            row.side == OrderSide::Sell && row.outcome == QuoteGeometryOutcome::SuppressedGuard
        }));
    }

    #[test]
    fn inventory_shift_still_moves_both_prices_but_not_inside_the_stand_off() {
        let mut cfg = enabled(integer_cfg(), 3.0, 1.0);
        cfg.skew_bps = 10.0;
        // Below the 80% exit threshold and below max_position, so this is a
        // skew shift rather than a side suppression or an inventory exit.
        let plan = plan_cycle(
            &cfg,
            input(
                market(10_000.0, 9_995.0, 10_005.0),
                0.03,
                &[],
                GuardDecision::INACTIVE,
                NonlinearSkewConfig::default(),
            ),
            false,
        );
        let quoted = places(&plan);
        assert_eq!(
            quoted.len(),
            2,
            "a long inventory shift must not erase one side"
        );
        let buy = quoted
            .iter()
            .find(|quote| quote.side == OrderSide::Buy)
            .unwrap();
        let sell = quoted
            .iter()
            .find(|quote| quote.side == OrderSide::Sell)
            .unwrap();
        assert!(buy.price <= 9_991.0);
        assert!(
            buy.price < 9_991.0,
            "the growing side is pushed further out"
        );
        assert!(
            sell.price >= 10_009.0,
            "the reducing side is not allowed inside the stand-off"
        );
        assert!((9_980.0..=10_020.0).contains(&buy.price));
        assert!((9_980.0..=10_020.0).contains(&sell.price));
    }

    #[test]
    fn external_and_micro_shifts_still_move_enabled_quotes() {
        use crate::{ExternalSkewConfig, MicroPriceConfig};
        let cfg = enabled(integer_cfg(), 3.0, 1.0);
        let mut cycle = input(
            market(10_000.0, 9_990.0, 10_000.0),
            0.0,
            &[],
            GuardDecision::INACTIVE,
            NonlinearSkewConfig::default(),
        );
        cycle.external_skew = ExternalSkewConfig {
            enabled: true,
            ..ExternalSkewConfig::default()
        };
        cycle.external_excess_bps = Some(20.0);
        cycle.micro_price = MicroPriceConfig {
            enabled: true,
            ..MicroPriceConfig::default()
        };
        let shifted = plan_cycle(&cfg, cycle, false);
        assert!(shifted.external_skew_shift_bps > 0.0);
        assert!(shifted.micro_price_shift_bps < 0.0);
        let unshifted = plan_cycle(
            &cfg,
            input(
                cycle.market,
                0.0,
                &[],
                GuardDecision::INACTIVE,
                NonlinearSkewConfig::default(),
            ),
            false,
        );
        assert_eq!(places(&unshifted).len(), 2);
        assert_eq!(places(&shifted).len(), 2);
        assert_ne!(places(&shifted), places(&unshifted));
        for quote in places(&shifted) {
            assert!((9_980.0..=10_020.0).contains(&quote.price));
        }
    }

    #[test]
    fn nonlinear_skew_still_changes_the_enabled_ladder() {
        let mut cfg = enabled(integer_cfg(), 3.0, 1.0);
        cfg.skew_bps = 10.0;
        let linear = plan_cycle(
            &cfg,
            input(
                market(10_000.0, 9_995.0, 10_005.0),
                0.025,
                &[],
                GuardDecision::INACTIVE,
                NonlinearSkewConfig::default(),
            ),
            false,
        );
        let nonlinear = plan_cycle(
            &cfg,
            input(
                market(10_000.0, 9_995.0, 10_005.0),
                0.025,
                &[],
                GuardDecision::INACTIVE,
                NonlinearSkewConfig {
                    enabled: true,
                    ..NonlinearSkewConfig::default()
                },
            ),
            false,
        );
        assert_eq!(places(&linear).len(), 2);
        assert_eq!(places(&nonlinear).len(), 2);
        assert_ne!(places(&linear), places(&nonlinear));
    }

    #[test]
    fn stand_off_breach_cancels_both_sides_together() {
        let cfg = enabled(integer_cfg(), 3.0, 1.0);
        let resting = [
            RestingQuote {
                order_id: Some("b".into()),
                side: OrderSide::Buy,
                level: 0,
                price: 9_996.0,
                qty: 0.01,
                ref_center: 10_005.0,
                placed_at_cycle: 0,
            },
            RestingQuote {
                order_id: Some("s".into()),
                side: OrderSide::Sell,
                level: 0,
                price: 10_014.0,
                qty: 0.01,
                ref_center: 10_005.0,
                placed_at_cycle: 0,
            },
        ];
        let plan = plan_cycle(
            &cfg,
            input(
                market(10_000.0, 9_999.0, 10_010.0),
                0.0,
                &resting,
                GuardDecision::INACTIVE,
                NonlinearSkewConfig::default(),
            ),
            false,
        );
        let cancels: Vec<_> = plan
            .actions
            .iter()
            .filter_map(|action| match action {
                Action::Cancel { side, reason, .. } => Some((*side, *reason)),
                _ => None,
            })
            .collect();
        assert_eq!(
            cancels,
            vec![
                (OrderSide::Buy, CancelReason::MarkMovedBeyondRefresh),
                (OrderSide::Sell, CancelReason::MarkMovedBeyondRefresh),
            ]
        );
        let quoted = places(&plan);
        assert_eq!(quoted.len(), 2);
        assert_eq!(quoted[0].price, 9_995.0);
        assert_eq!(quoted[1].price, 10_014.0);
    }

    #[test]
    fn unchanged_book_holds_both_sides() {
        let cfg = enabled(integer_cfg(), 3.0, 1.0);
        let resting = [
            RestingQuote {
                order_id: Some("b".into()),
                side: OrderSide::Buy,
                level: 0,
                price: 9_991.0,
                qty: 0.01,
                ref_center: 10_000.0,
                placed_at_cycle: 0,
            },
            RestingQuote {
                order_id: Some("s".into()),
                side: OrderSide::Sell,
                level: 0,
                price: 10_009.0,
                qty: 0.01,
                ref_center: 10_000.0,
                placed_at_cycle: 0,
            },
        ];
        let plan = plan_cycle(
            &cfg,
            input(
                market(10_000.0, 9_995.0, 10_005.0),
                0.0,
                &resting,
                GuardDecision::INACTIVE,
                NonlinearSkewConfig::default(),
            ),
            false,
        );
        assert!(plan
            .actions
            .iter()
            .all(|action| matches!(action, Action::Hold { .. })));
        assert_eq!(plan.actions.len(), 2);
    }

    #[test]
    fn shared_mid_refresh_brings_both_sides_in_together() {
        let cfg = enabled(integer_cfg(), 3.0, 1.0);
        let resting = [
            RestingQuote {
                order_id: Some("b".into()),
                side: OrderSide::Buy,
                level: 0,
                price: 9_900.0,
                qty: 0.01,
                ref_center: 10_000.0,
                placed_at_cycle: 0,
            },
            RestingQuote {
                order_id: Some("s".into()),
                side: OrderSide::Sell,
                level: 0,
                price: 10_100.0,
                qty: 0.01,
                ref_center: 10_000.0,
                placed_at_cycle: 0,
            },
        ];
        let plan = plan_cycle(
            &cfg,
            input(
                market(10_000.0, 10_000.0, 10_010.0),
                0.0,
                &resting,
                GuardDecision::INACTIVE,
                NonlinearSkewConfig::default(),
            ),
            false,
        );
        assert_eq!(
            plan.actions
                .iter()
                .filter(|action| matches!(action, Action::Cancel { .. }))
                .count(),
            2
        );
        let quoted = places(&plan);
        assert_eq!(quoted[0].price, 9_996.0);
        assert_eq!(quoted[1].price, 10_014.0);
    }

    #[test]
    fn enabled_exit_matches_disabled_exit() {
        let snapshot = market(10_000.0, 9_995.0, 10_005.0);
        let on = plan_cycle(
            &enabled(integer_cfg(), 3.0, 1.0),
            input(
                snapshot,
                0.05,
                &[],
                GuardDecision::INACTIVE,
                NonlinearSkewConfig::default(),
            ),
            false,
        );
        let off = plan_cycle(
            &integer_cfg(),
            input(
                snapshot,
                0.05,
                &[],
                GuardDecision::INACTIVE,
                NonlinearSkewConfig::default(),
            ),
            false,
        );
        assert_eq!(on.inventory_exit, off.inventory_exit);
        assert_eq!(on.requested_inventory_exit, off.requested_inventory_exit);
        assert!(on
            .actions
            .iter()
            .all(|action| !matches!(action, Action::Place(_))));
        assert!(off
            .actions
            .iter()
            .all(|action| !matches!(action, Action::Place(_))));
    }

    #[test]
    fn missing_touch_does_not_fall_back_to_mark() {
        let cfg = enabled(integer_cfg(), 3.0, 1.0);
        let mut cycle = input(
            market(10_000.0, 9_995.0, 10_005.0),
            0.0,
            &[],
            GuardDecision::INACTIVE,
            NonlinearSkewConfig::default(),
        );
        cycle.market.best_ask = None;
        let plan = plan_cycle(&cfg, cycle, false);
        assert!(places(&plan).is_empty());
    }

    #[test]
    fn validate_rejects_bad_distances_while_disabled() {
        let bad = BestAnchorConfig {
            enabled: false,
            best_jump_p999: -1.0,
            margin: 0.0,
        };
        assert!(bad.validate().is_err());
        assert!(BestAnchorConfig::default().validate().is_ok());
    }
}
