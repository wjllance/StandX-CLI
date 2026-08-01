use super::*;

/// mark=100-friendly config: 2 price decimals, 4 qty decimals.
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
    }
}

fn desired(
    cfg: &MakerConfig,
    mark: f64,
    best_bid: Option<f64>,
    best_ask: Option<f64>,
    position: f64,
) -> Vec<DesiredQuote> {
    compute_desired_quotes(
        cfg,
        mark,
        best_bid,
        best_ask,
        position,
        SizeSkewDecision::INACTIVE,
        NonlinearSkewConfig::default(),
        0.0,
        GuardDecision::INACTIVE,
    )
}

fn resting(side: OrderSide, level: u32, price: f64, ref_center: f64) -> RestingQuote {
    RestingQuote {
        order_id: Some("1".into()),
        side,
        level,
        price,
        qty: 0.01,
        ref_center,
        placed_at_cycle: 0,
    }
}

fn find(quotes: &[DesiredQuote], side: OrderSide, level: u32) -> &DesiredQuote {
    quotes
        .iter()
        .find(|q| q.side == side && q.level == level)
        .expect("quote missing")
}

#[test]
fn inventory_exit_plan_is_explicit_capped_and_reducing() {
    assert_eq!(
        inventory_exit_plan(0.04, 0.05, 80.0, 0.015),
        Some(InventoryExit {
            side: OrderSide::Sell,
            qty: 0.015,
            kind: ExitKind::InventoryTrim,
        })
    );
    assert_eq!(
        inventory_exit_plan(-0.05, 0.05, 80.0, 0.10),
        Some(InventoryExit {
            side: OrderSide::Buy,
            qty: 0.05,
            kind: ExitKind::InventoryTrim,
        })
    );
    assert_eq!(inventory_exit_plan(0.039, 0.05, 80.0, 0.01), None);
    assert_eq!(inventory_exit_plan(0.05, 0.05, 0.0, 0.01), None);
    assert_eq!(inventory_exit_plan(0.05, 0.05, 101.0, 0.01), None);
}

#[test]
fn requotes_on_touch_move_without_creating_crossed_quote() {
    // Cycle 0: quote a calm book and let the places rest.
    let desired = desired(&cfg(), 100.0, Some(99.99), Some(100.01), 0.0);
    let actions = reconcile(
        &cfg(),
        100.0,
        0.0,
        Some(99.99),
        Some(100.01),
        &desired,
        &[],
        0,
        Default::default(),
        0.0,
    );
    assert!(actions
        .iter()
        .any(|action| matches!(action, Action::Place(_))));
    let resting: Vec<RestingQuote> = actions
        .iter()
        .filter_map(|action| match action {
            Action::Place(quote) => Some(RestingQuote {
                order_id: None,
                side: quote.side,
                level: quote.level,
                price: quote.price,
                qty: quote.qty,
                ref_center: skew_center(&cfg(), 100.0, 0.0),
                placed_at_cycle: 0,
            }),
            _ => None,
        })
        .collect();

    // Cycle 1: the touch drops below the resting sell; the stale quote is
    // cancelled as WouldCross and no replacement crosses the new touch.
    let (bid, ask) = (99.88, 99.90);
    let desired = self::desired(&cfg(), 100.0, Some(bid), Some(ask), 0.0);
    let actions = reconcile(
        &cfg(),
        100.0,
        0.0,
        Some(bid),
        Some(ask),
        &desired,
        &resting,
        1,
        Default::default(),
        0.0,
    );
    assert!(actions.iter().any(|action| matches!(
        action,
        Action::Cancel {
            reason: CancelReason::WouldCross,
            ..
        }
    )));
    for action in &actions {
        if let Action::Place(quote) = action {
            match quote.side {
                OrderSide::Buy => assert!(quote.price < ask),
                OrderSide::Sell => assert!(quote.price > bid),
            }
        }
    }
}

// 1. Basic two-sided quoting.
#[test]
fn basic_two_sided() {
    let quotes = desired(&cfg(), 100.0, Some(99.99), Some(100.01), 0.0);
    assert_eq!(quotes.len(), 2);
    assert_eq!(find(&quotes, OrderSide::Buy, 0).price, 99.90);
    assert_eq!(find(&quotes, OrderSide::Sell, 0).price, 100.10);
    assert_eq!(find(&quotes, OrderSide::Buy, 0).qty, 0.01);
}

// 2. Spread wider than band: clamp to band edges, not dropped.
#[test]
fn band_clamp() {
    let mut c = cfg();
    c.spread_bps = 30.0; // > band 20
    let quotes = desired(&c, 100.0, Some(99.5), Some(100.5), 0.0);
    assert_eq!(find(&quotes, OrderSide::Buy, 0).price, 99.80);
    assert_eq!(find(&quotes, OrderSide::Sell, 0).price, 100.20);
}

// 3. Directional tick rounding: buy floors, sell ceils.
#[test]
fn tick_rounding_directional() {
    let mut c = cfg();
    c.price_decimals = 1;
    c.spread_bps = 5.0;
    // mark=100.03: raw buy = 99.979985 -> floor(1dp) 99.9
    //              raw sell = 100.080015 -> ceil(1dp) 100.1
    let quotes = desired(&c, 100.03, None, None, 0.0);
    assert_eq!(find(&quotes, OrderSide::Buy, 0).price, 99.9);
    assert_eq!(find(&quotes, OrderSide::Sell, 0).price, 100.1);
    assert_eq!(format_decimals(99.9, 1), "99.9");
}

// 4. Rounding that exits the band is nudged back inside.
#[test]
fn rounding_reenters_band() {
    let mut c = cfg();
    c.price_decimals = 0; // whole-number ticks
    c.spread_bps = 20.0; // == band: raw buy exactly at band edge 99.8
    c.band_bps = 20.0;
    // raw buy = 99.8, floor(0dp) = 99 < band_lo 99.8 -> nudge +1 tick = 100
    // (floor(99.8+1) = 100)... still >= band_lo, inside band.
    let quotes = desired(&c, 100.0, None, None, 0.0);
    let buy = find(&quotes, OrderSide::Buy, 0);
    assert!(buy.price >= 99.8, "price {} left the band", buy.price);
    assert_eq!(buy.price, 100.0);
}

// 5. No-cross clamp on both sides.
#[test]
fn no_cross_clamp() {
    // Best ask (99.85) sits BELOW our raw buy (99.90): buy must clamp
    // down to ask - tick.
    let quotes = desired(&cfg(), 100.0, Some(99.83), Some(99.85), 0.0);
    assert_eq!(quotes.len(), 2, "{quotes:?}");
    let buy = find(&quotes, OrderSide::Buy, 0);
    let sell = find(&quotes, OrderSide::Sell, 0);
    // buy clamped to ask - tick = 99.84
    assert_eq!(buy.price, 99.84);
    // sell raw 100.10 already > bid + tick; unchanged
    assert_eq!(sell.price, 100.10);

    // Symmetric: bid above our raw sell forces sell up to bid + tick.
    let quotes = desired(&cfg(), 100.0, Some(100.15), Some(100.20), 0.0);
    let sell = find(&quotes, OrderSide::Sell, 0);
    assert_eq!(sell.price, 100.16);
}

#[test]
fn drops_side_when_band_and_no_cross_have_no_feasible_tick() {
    let quotes = desired(&cfg(), 100.0, Some(99.78), Some(99.79), 0.0);
    assert!(quotes.iter().all(|quote| quote.side == OrderSide::Sell));

    let quotes = desired(&cfg(), 100.0, Some(100.21), Some(100.22), 0.0);
    assert!(quotes.iter().all(|quote| quote.side == OrderSide::Buy));
}

#[test]
fn invalid_market_values_produce_no_quotes() {
    assert!(desired(&cfg(), f64::NAN, None, None, 0.0).is_empty());
    assert!(desired(&cfg(), 100.0, Some(f64::INFINITY), None, 0.0).is_empty());
    assert!(desired(&cfg(), 100.0, None, Some(0.0), 0.0).is_empty());
}

// 6. Size below min_order_qty -> no quotes at all.
#[test]
fn min_qty_rejection() {
    let mut c = cfg();
    c.size = 0.00001; // rounds to 0.0000 at 4dp -> below min 0.001
    let quotes = desired(&c, 100.0, None, None, 0.0);
    assert!(quotes.is_empty());
}

// 7. Max-position suppression, both directions.
#[test]
fn max_position_suppresses_buy() {
    let quotes = desired(&cfg(), 100.0, None, None, 0.05);
    assert!(quotes.iter().all(|q| q.side == OrderSide::Sell));
    assert_eq!(quotes.len(), 1);
}

#[test]
fn max_position_suppresses_sell() {
    let quotes = desired(&cfg(), 100.0, None, None, -0.05);
    assert!(quotes.iter().all(|q| q.side == OrderSide::Buy));
    assert_eq!(quotes.len(), 1);
}

// 8. Anti-flicker: drift within refresh threshold -> Hold.
#[test]
fn reconcile_hold_within_refresh() {
    let c = cfg();
    let mark = 100.02; // 2 bps from ref 100.0, refresh = 3
    let desired = desired(&c, mark, None, None, 0.0);
    let rest = vec![
        resting(OrderSide::Buy, 0, 99.90, 100.0),
        resting(OrderSide::Sell, 0, 100.10, 100.0),
    ];
    let actions = reconcile(
        &c,
        mark,
        0.0,
        None,
        None,
        &desired,
        &rest,
        7,
        Default::default(),
        0.0,
    );
    assert!(
        actions.iter().all(|a| matches!(a, Action::Hold { .. })),
        "{actions:?}"
    );
    assert_eq!(actions.len(), 2);
    if let Action::Hold { age_cycles, .. } = &actions[0] {
        assert_eq!(*age_cycles, 7);
    }
}

// 9. Drift beyond refresh -> Cancel(mark_moved) + Place, cancel first.
#[test]
fn reconcile_requote_beyond_refresh() {
    let c = cfg();
    let mark = 100.05; // 5 bps > refresh 3
    let desired = desired(&c, mark, None, None, 0.0);
    let rest = vec![resting(OrderSide::Buy, 0, 99.90, 100.0)];
    let actions = reconcile(
        &c,
        mark,
        0.0,
        None,
        None,
        &desired,
        &rest,
        1,
        Default::default(),
        0.0,
    );
    // Expect: cancel(buy, mark_moved), then places for buy+sell.
    assert!(matches!(
        actions[0],
        Action::Cancel {
            reason: CancelReason::MarkMovedBeyondRefresh,
            ..
        }
    ));
    let cancel_idx = 0;
    let place_idx = actions
        .iter()
        .position(|a| matches!(a, Action::Place(_)))
        .unwrap();
    assert!(cancel_idx < place_idx);
}

// 10. Outside band takes precedence over refresh drift.
#[test]
fn reconcile_cancel_outside_band_precedence() {
    let c = cfg();
    // Mark gapped 30 bps: resting buy at 99.90 with ref 100.0 is now
    // outside band [100.10, 100.50] around mark 100.30.
    let mark = 100.30;
    let desired = desired(&c, mark, None, None, 0.0);
    let rest = vec![resting(OrderSide::Buy, 0, 99.90, 100.0)];
    let actions = reconcile(
        &c,
        mark,
        0.0,
        None,
        None,
        &desired,
        &rest,
        1,
        Default::default(),
        0.0,
    );
    assert!(
        matches!(
            actions[0],
            Action::Cancel {
                reason: CancelReason::OutsideBand,
                ..
            }
        ),
        "{actions:?}"
    );
}

// 11. Touch moved through a resting quote -> WouldCross.
#[test]
fn reconcile_cancel_would_cross() {
    let c = cfg();
    let mark = 100.01; // tiny drift, within refresh
    let desired = desired(&c, mark, Some(100.12), Some(100.14), 0.0);
    // Resting sell at 100.10 now BELOW best bid 100.12 -> crossed.
    let rest = vec![resting(OrderSide::Sell, 0, 100.10, 100.0)];
    let actions = reconcile(
        &c,
        mark,
        0.0,
        Some(100.12),
        Some(100.14),
        &desired,
        &rest,
        1,
        Default::default(),
        0.0,
    );
    assert!(
        matches!(
            actions[0],
            Action::Cancel {
                reason: CancelReason::WouldCross,
                ..
            }
        ),
        "{actions:?}"
    );
}

#[test]
fn detects_resting_quotes_that_would_cross_a_new_touch() {
    let quotes = vec![resting(OrderSide::Buy, 0, 99.95, 100.0)];
    assert!(resting_quotes_would_cross(
        &quotes,
        Some(99.90),
        Some(99.95)
    ));
    assert!(!resting_quotes_would_cross(
        &quotes,
        Some(99.90),
        Some(99.96)
    ));
}

// 12. Level removed from config -> Stale.
#[test]
fn reconcile_stale_level() {
    let c = cfg(); // levels = 1 -> only level 0 desired
    let mark = 100.0;
    let desired = desired(&c, mark, None, None, 0.0);
    let rest = vec![
        resting(OrderSide::Buy, 0, 99.90, 100.0),
        resting(OrderSide::Buy, 1, 99.88, 100.0), // stale level
    ];
    let actions = reconcile(
        &c,
        mark,
        0.0,
        None,
        None,
        &desired,
        &rest,
        1,
        Default::default(),
        0.0,
    );
    let stale: Vec<_> = actions
        .iter()
        .filter(|a| {
            matches!(
                a,
                Action::Cancel {
                    reason: CancelReason::Stale,
                    level: 1,
                    ..
                }
            )
        })
        .collect();
    assert_eq!(stale.len(), 1, "{actions:?}");
}

// 13. Multi-level ladder + duplicate collapse.
#[test]
fn multi_level_ladder() {
    let mut c = cfg();
    c.levels = 3;
    let quotes = desired(&c, 100.0, None, None, 0.0);
    // Buys descending: 99.90, 99.88, 99.86; sells ascending mirrored.
    assert_eq!(find(&quotes, OrderSide::Buy, 0).price, 99.90);
    assert_eq!(find(&quotes, OrderSide::Buy, 1).price, 99.88);
    assert_eq!(find(&quotes, OrderSide::Buy, 2).price, 99.86);
    assert_eq!(find(&quotes, OrderSide::Sell, 2).price, 100.14);
    assert_eq!(quotes.len(), 6);

    // Ladder flattened by band: spread 18, step 2, band 20 -> levels 1+
    // clamp to the band edge and duplicates collapse.
    let mut c2 = cfg();
    c2.levels = 3;
    c2.spread_bps = 18.0;
    let quotes = desired(&c2, 100.0, None, None, 0.0);
    let buys: Vec<_> = quotes.iter().filter(|q| q.side == OrderSide::Buy).collect();
    assert_eq!(buys.len(), 2, "{buys:?}"); // 99.82, then 99.80 (L1) and L2 dup dropped
    assert_eq!(buys[1].price, 99.80);
}

// 14. Helper edge cases.
#[test]
fn helper_edge_cases() {
    assert_eq!(bps_diff(100.0, 0.0), 0.0);
    assert!((bps_diff(100.05, 100.0) - 5.0).abs() < 1e-9); // ~5 bps
    assert!((bps_diff(99.95, 100.0) - 5.0).abs() < 1e-9); // symmetric
    assert_eq!(round_to_decimals(1.23456, 0), 1.0);
    assert_eq!(round_to_decimals(-1.235, 2), -1.24);
    assert_eq!(floor_to_decimals(99.90, 2), 99.90); // representation artifact guard
    assert_eq!(ceil_to_decimals(100.10, 2), 100.10);
    assert_eq!(format_decimals(99.9, 2), "99.90");
    assert_eq!(format_decimals(0.0123, 4), "0.0123");
}

// 15. Mark/mid divergence guard helper.
#[test]
fn mark_mid_divergence() {
    // mid = 100.0 == mark -> no divergence
    assert_eq!(mark_mid_divergence_bps(100.0, 99.9, 100.1), 0.0);
    // mid = 100.25 vs mark 100.0 -> 25 bps
    assert!((mark_mid_divergence_bps(100.0, 100.2, 100.3) - 25.0).abs() < 1e-9);
    // symmetric below
    assert!((mark_mid_divergence_bps(100.0, 99.7, 99.8) - 25.0).abs() < 1e-9);
    // degenerate mark = 0 -> 0.0, no blowup
    assert_eq!(mark_mid_divergence_bps(0.0, 99.9, 100.1), 0.0);
}

// 23. Paper fill model: crossed touch fills, otherwise not.
#[test]
fn paper_fills_on_crossed_touch() {
    // Resting buy at 99.90: fills once offers reach down to it.
    assert!(!quote_crosses_touch(
        OrderSide::Buy,
        99.90,
        Some(99.80),
        Some(99.95)
    ));
    assert!(quote_crosses_touch(
        OrderSide::Buy,
        99.90,
        Some(99.80),
        Some(99.90)
    ));
    assert!(quote_crosses_touch(
        OrderSide::Buy,
        99.90,
        Some(99.80),
        Some(99.85)
    ));
    // Resting sell at 100.10: fills once bids reach up to it.
    assert!(!quote_crosses_touch(
        OrderSide::Sell,
        100.10,
        Some(100.05),
        Some(100.2)
    ));
    assert!(quote_crosses_touch(
        OrderSide::Sell,
        100.10,
        Some(100.10),
        Some(100.2)
    ));
    // Absent book side never fills.
    assert!(!quote_crosses_touch(OrderSide::Buy, 99.90, None, None));
    assert!(!quote_crosses_touch(OrderSide::Sell, 100.10, None, None));
}

// 24. Stats: spread capture, mark-to-market PnL, uptime.
#[test]
fn stats_pnl_and_capture() {
    let mut s = MakerStats::default();
    // Buy 1 @ 99.90 (mark 100) then sell 1 @ 100.10 (mark 100): a round
    // trip capturing 10 + 10 bps, net cash +0.20, flat position.
    s.record_fill(OrderSide::Buy, 99.90, 1.0, 100.0);
    s.record_fill(OrderSide::Sell, 100.10, 1.0, 100.0);
    assert_eq!(s.fills(), 2);
    assert!((s.filled_qty - 2.0).abs() < 1e-9);
    assert!((s.cash - 0.20).abs() < 1e-9);
    // Flat position -> PnL is just the captured cash.
    assert!((s.pnl(0.0, 100.0) - 0.20).abs() < 1e-9);
    // Each leg captured 10 bps -> avg 10.
    assert!((s.avg_spread_capture_bps() - 10.0).abs() < 1e-6);
}

#[test]
fn stats_unrealized_inventory() {
    let mut s = MakerStats::default();
    // Buy 2 @ 100 (no edge), then mark rises to 101: unrealized +2.
    s.record_fill(OrderSide::Buy, 100.0, 2.0, 100.0);
    assert!((s.pnl(2.0, 101.0) - 2.0).abs() < 1e-9);
    assert!((s.pnl(2.0, 100.0)).abs() < 1e-9); // flat at entry mark
}

#[test]
fn stats_adopted_inventory_starts_at_zero_for_long_and_short() {
    let long = MakerStats::with_inventory_baseline(0.13, 59.72);
    let short = MakerStats::with_inventory_baseline(-0.13, 59.72);
    assert!(long.pnl(0.13, 59.72).abs() < 1e-9);
    assert!(short.pnl(-0.13, 59.72).abs() < 1e-9);
    assert!((long.pnl(0.13, 60.72) - 0.13).abs() < 1e-9);
    assert!((short.pnl(-0.13, 60.72) + 0.13).abs() < 1e-9);
}

#[test]
fn stats_adopted_inventory_and_new_fill_share_session_basis() {
    let mut stats = MakerStats::with_inventory_baseline(-0.2, 60.0);
    stats.record_fill(OrderSide::Buy, 59.5, 0.2, 59.5);
    assert!((stats.pnl(0.0, 59.5) - 0.1).abs() < 1e-9);
    assert_eq!(stats.fills(), 1);
}

#[test]
fn stats_uptime_and_live_inference() {
    let mut s = MakerStats::default();
    s.end_cycle(0.0, true); // two-sided
    s.end_cycle(0.0, false); // one-sided
    assert_eq!(s.cycles, 2);
    assert!((s.uptime_pct() - 50.0).abs() < 1e-9);
    // Position movement alone must not fabricate a live fill: exact
    // maker fills are supplied by the venue ledger.
    let mut l = MakerStats::default();
    l.end_cycle(0.01, true);
    assert_eq!(l.fills(), 0);
    assert_eq!(l.buy_fills, 0);
    assert!((l.max_abs_position - 0.01).abs() < 1e-9);
}

// 16. skew_center helper: directional, zero cases.
#[test]
fn skew_center_directional() {
    let mut c = cfg();
    c.skew_bps = 10.0;
    // flat position -> center = mark
    assert_eq!(skew_center(&c, 100.0, 0.0), 100.0);
    // long (ratio +0.5) -> center down 99.95
    assert!((skew_center(&c, 100.0, 0.025) - 99.95).abs() < 1e-9);
    // short (ratio -0.5) -> center up 100.05
    assert!((skew_center(&c, 100.0, -0.025) - 100.05).abs() < 1e-9);
    // skew off -> center = mark regardless of position
    let c0 = cfg();
    assert_eq!(skew_center(&c0, 100.0, 0.05), 100.0);
}

// 17. Long inventory shifts the whole ladder down; reducing side (sell)
// moves nearer mark, growing side (buy) further.
#[test]
fn skew_long_shifts_center_down() {
    let mut c = cfg();
    c.skew_bps = 10.0;
    // half-max long -> center 99.95; buy = 99.85, sell = 100.05
    let q = desired(&c, 100.0, None, None, 0.025);
    assert_eq!(find(&q, OrderSide::Buy, 0).price, 99.85);
    assert_eq!(find(&q, OrderSide::Sell, 0).price, 100.05);
    // both below the no-skew baseline (99.90 / 100.10)
    assert!(find(&q, OrderSide::Buy, 0).price < 99.90);
    assert!(find(&q, OrderSide::Sell, 0).price < 100.10);
}

// 18. Short inventory shifts up; reducing side (buy) nearer mark.
#[test]
fn skew_short_shifts_center_up() {
    let mut c = cfg();
    c.skew_bps = 10.0;
    // half-max short -> center 100.05; buy = 99.94, sell = 100.16
    let q = desired(&c, 100.0, None, None, -0.025);
    assert_eq!(find(&q, OrderSide::Buy, 0).price, 99.94);
    assert_eq!(find(&q, OrderSide::Sell, 0).price, 100.16);
    // buy moved nearer mark than the no-skew baseline 99.90
    assert!(find(&q, OrderSide::Buy, 0).price > 99.90);
}

// 19. skew_bps = 0 is a no-op regardless of position.
#[test]
fn skew_zero_is_noop() {
    let c = cfg(); // skew_bps = 0
    let base = desired(&c, 100.0, None, None, 0.0);
    let with_pos = desired(&c, 100.0, None, None, 0.025);
    assert_eq!(base, with_pos);
}

#[test]
fn exposure_cap_limits_all_same_side_fills() {
    let mut c = cfg();
    c.levels = 3;
    c.size = 0.02;
    c.max_position = 0.05;
    let raw = desired(&c, 100.0, None, None, 0.03);
    let capped = cap_desired_exposure(&c, 0.03, &raw, &[]);

    // At +0.03, only one additional 0.02 buy can be exposed. All three
    // sells remain safe: even if they all fill, the position is -0.03.
    assert_eq!(
        capped
            .iter()
            .filter(|quote| quote.side == OrderSide::Buy)
            .count(),
        1
    );
    assert_eq!(
        capped
            .iter()
            .filter(|quote| quote.side == OrderSide::Sell)
            .count(),
        3
    );
    let buy_qty: f64 = capped
        .iter()
        .filter(|quote| quote.side == OrderSide::Buy)
        .map(|quote| quote.qty)
        .sum();
    assert!(0.03 + buy_qty <= c.max_position + 1e-9);
}

#[test]
fn exposure_cap_reserves_pending_slot_before_new_levels() {
    let mut c = cfg();
    c.levels = 3;
    c.size = 0.02;
    c.max_position = 0.05;
    let raw = desired(&c, 100.0, None, None, 0.03);
    let capped = cap_desired_exposure(&c, 0.03, &raw, &[(OrderSide::Buy, 2)]);

    // The in-flight outer bid gets the only 0.02 buy budget. A later
    // reconcile cannot place level 0 in addition while level 2 is still
    // awaiting exchange visibility.
    assert!(capped
        .iter()
        .any(|quote| quote.side == OrderSide::Buy && quote.level == 2));
    assert!(!capped
        .iter()
        .any(|quote| quote.side == OrderSide::Buy && quote.level == 0));
}

// 20. Inventory ratio saturates at ±1 past max_position.
#[test]
fn skew_clamps_at_full_inventory() {
    let mut c = cfg();
    c.skew_bps = 10.0;
    // 2x max short: growing side (sell) suppressed, only buy remains;
    // ratio clamps to -1 -> center 100.10 -> buy 99.99 (NOT the ratio=-2
    // value 100.09).
    let q = desired(&c, 100.0, None, None, -0.10);
    assert!(q.iter().all(|d| d.side == OrderSide::Buy));
    assert_eq!(find(&q, OrderSide::Buy, 0).price, 99.99);
}

// 21. Large skew still respects the band and no-cross guards.
#[test]
fn skew_still_respects_band_and_no_cross() {
    let mut c = cfg();
    c.skew_bps = 100.0; // would push the sell far below mark
    let (bid, ask) = (99.90, 100.00);
    // full long: buy suppressed; sell pulled down hard but held above the
    // band floor and one tick above the bid.
    let q = desired(&c, 100.0, Some(bid), Some(ask), 0.05);
    assert_eq!(q.len(), 1, "{q:?}");
    let sell = find(&q, OrderSide::Sell, 0).price;
    assert!(sell >= 99.80 - 1e-9, "sell {sell} below band floor");
    assert!(sell > bid, "sell {sell} crosses bid {bid}");
}

// 22. Inventory skew alone (mark unchanged) triggers a re-quote once the
// center drifts past refresh_bps; stays held within it.
#[test]
fn reconcile_skew_requote() {
    let mut c = cfg();
    c.skew_bps = 10.0;
    let mark = 100.0;
    // Resting sell placed when flat (ref_center = 100.0).
    // Long 0.025 -> center 99.95, drift 5bps > refresh 3 -> re-quote.
    let pos = 0.025;
    let desired = desired(&c, mark, None, None, pos);
    let rest = vec![resting(OrderSide::Sell, 0, 100.10, 100.0)];
    let actions = reconcile(
        &c,
        mark,
        pos,
        None,
        None,
        &desired,
        &rest,
        1,
        Default::default(),
        0.0,
    );
    assert!(actions.iter().any(|a| matches!(
        a,
        Action::Cancel {
            reason: CancelReason::MarkMovedBeyondRefresh,
            ..
        }
    )));

    // Smaller long 0.01 -> center 99.98, drift 2bps < refresh -> hold.
    let pos2 = 0.01;
    let desired2 = self::desired(&c, mark, None, None, pos2);
    let rest2 = vec![resting(OrderSide::Sell, 0, 100.10, 100.0)];
    let actions2 = reconcile(
        &c,
        mark,
        pos2,
        None,
        None,
        &desired2,
        &rest2,
        1,
        Default::default(),
        0.0,
    );
    assert!(actions2.iter().any(|a| matches!(a, Action::Hold { .. })));
    assert!(!actions2.iter().any(|a| matches!(a, Action::Cancel { .. })));
}

// 25. Vol breaker: disabled is a no-op.
#[test]
fn vol_breaker_disabled() {
    let mut b = VolBreaker::new(5, 0.0);
    assert!(!b.enabled());
    assert!(!b.observe(100.0));
    assert!(!b.observe(200.0)); // huge move, but disabled -> never halts
    assert!(!b.halted());
}

// 26. Vol breaker: trips on a fast move, resumes with hysteresis.
#[test]
fn vol_breaker_trip_and_rearm() {
    // window 4, pause 30bps -> rearm 15bps.
    let mut b = VolBreaker::new(4, 30.0);
    assert!(!b.observe(100.0));
    assert!(!b.observe(100.1)); // 10bps range, calm
    assert!(!b.halted());
    // Jump to 100.4: range now (100.4-100.0)/100 = 40bps >= 30 -> halt.
    assert!(b.observe(100.4));
    assert!(b.halted());
    // Still elevated while the low sample (100.0) is in the window.
    assert!(b.observe(100.4)); // range 40bps, still halted
                               // Push new samples near 100.4 so old lows roll out; range collapses.
    b.observe(100.4);
    let halted = b.observe(100.4); // window now all ~100.4 -> range ~0 < 15
    assert!(!halted);
    assert!(!b.halted());
}

// 27. Vol breaker: hysteresis holds between rearm and pause.
#[test]
fn vol_breaker_hysteresis_band() {
    let mut b = VolBreaker::new(3, 40.0); // rearm 20bps
    b.observe(100.0);
    assert!(b.observe(100.5)); // 50bps -> halt
                               // Range drifts to ~25bps (between rearm 20 and pause 40): stays halted.
    b.observe(100.25);
    let halted = b.observe(100.25); // window {100.5,100.25,100.25} range 25bps
    assert!(halted, "should stay halted in the hysteresis band");
}

#[test]
fn preflight_skips_crossed_divergent_or_incomplete_live_books() {
    let mut breaker = VolBreaker::new(3, 0.0);
    let crossed = preflight_cycle(
        &mut breaker,
        MarketSnapshot {
            mark: 100.0,
            best_bid: Some(100.1),
            best_ask: Some(100.0),
        },
        10.0,
        true,
    );
    assert_eq!(crossed.skip, Some(CycleSkip::CrossedBook));

    let divergent = preflight_cycle(
        &mut breaker,
        MarketSnapshot {
            mark: 100.0,
            best_bid: Some(90.0),
            best_ask: Some(90.1),
        },
        10.0,
        true,
    );
    assert!(matches!(
        divergent.skip,
        Some(CycleSkip::MarkMidDivergence { divergence_bps }) if divergence_bps > 10.0
    ));

    let incomplete = preflight_cycle(
        &mut breaker,
        MarketSnapshot {
            mark: 100.0,
            best_bid: Some(99.9),
            best_ask: None,
        },
        10.0,
        true,
    );
    assert_eq!(incomplete.skip, Some(CycleSkip::MissingTouch));
}

#[test]
fn cycle_plan_pulls_quotes_for_exit_and_suppresses_exit_during_vol_halt() {
    let c = cfg();
    let resting = vec![resting(OrderSide::Buy, 0, 99.90, 100.0)];
    let input = CycleInput {
        cycle: 1,
        market: MarketSnapshot {
            mark: 100.0,
            best_bid: Some(99.8),
            best_ask: Some(100.2),
        },
        position: c.max_position,
        resting: &resting,
        pending_slots: &[],
        market_data_mode: MarketDataMode::Active,
        active_exit_enabled: true,
        inventory_exit_pct: 80.0,
        inventory_exit_qty: 0.01,
        size_skew: Default::default(),
        nonlinear_skew: Default::default(),
        external_skew: Default::default(),
        external_excess_bps: None,
        guard: Default::default(),
        wind_down: false,
        qty_tolerance: 0.0005,
    };

    let exit_plan = plan_cycle(&c, input, false);
    assert_eq!(
        exit_plan.requested_inventory_exit,
        Some(InventoryExit {
            side: OrderSide::Sell,
            qty: 0.01,
            kind: ExitKind::InventoryTrim,
        })
    );
    assert_eq!(exit_plan.inventory_exit, exit_plan.requested_inventory_exit);
    assert!(exit_plan
        .actions
        .iter()
        .any(|action| matches!(action, Action::Cancel { .. })));
    assert!(!exit_plan
        .actions
        .iter()
        .any(|action| matches!(action, Action::Place(_))));

    assert_eq!(exit_plan.exit_suppression, None);

    let halted_plan = plan_cycle(&c, input, true);
    assert_eq!(
        halted_plan.requested_inventory_exit,
        exit_plan.requested_inventory_exit
    );
    assert_eq!(halted_plan.inventory_exit, None);
    assert_eq!(
        halted_plan.exit_suppression,
        Some(SuppressedExit {
            kind: ExitKind::InventoryTrim,
            reason: ExitSuppression::VolatilityHalt,
        })
    );
}

/// Stage 5-b D1: a volatility halt suppresses BOTH exit policies, and says
/// so in a typed field. Wind-down is the one that used to be easy to assume
/// still ran: an arm ending mid-halt keeps its inventory until the halt
/// clears, and that trade-off is now pinned by a test, not by a comment.
#[test]
fn vol_halt_suppresses_wind_down_exit_with_typed_reason() {
    let c = cfg();
    let input = CycleInput {
        cycle: 7,
        market: MarketSnapshot {
            mark: 100.0,
            best_bid: Some(99.8),
            best_ask: Some(100.2),
        },
        position: 0.02,
        resting: &[],
        pending_slots: &[],
        market_data_mode: MarketDataMode::Active,
        // Wind-down ignores the configured trigger entirely.
        active_exit_enabled: false,
        inventory_exit_pct: 0.0,
        inventory_exit_qty: 0.0,
        size_skew: Default::default(),
        nonlinear_skew: Default::default(),
        external_skew: Default::default(),
        external_excess_bps: None,
        guard: Default::default(),
        wind_down: true,
        qty_tolerance: 0.0005,
    };

    let running = plan_cycle(&c, input, false);
    assert_eq!(
        running.inventory_exit,
        Some(InventoryExit {
            side: OrderSide::Sell,
            qty: 0.02,
            kind: ExitKind::WindDown,
        })
    );
    assert_eq!(running.exit_suppression, None);

    let halted = plan_cycle(&c, input, true);
    assert_eq!(halted.inventory_exit, None);
    assert_eq!(
        halted.exit_suppression,
        Some(SuppressedExit {
            kind: ExitKind::WindDown,
            reason: ExitSuppression::VolatilityHalt,
        })
    );
    // No emergency execution appears in place of the suppressed exit.
    assert!(!halted
        .actions
        .iter()
        .any(|action| matches!(action, Action::Place(_))));
}

/// Inactive market data suppresses the exit too, and outranks the halt as
/// the reported reason: without a trusted price the halt verdict itself is
/// computed from stale marks.
#[test]
fn inactive_market_data_suppresses_exit_and_outranks_halt() {
    let c = cfg();
    let mk = |mode, wind_down| CycleInput {
        cycle: 9,
        market: MarketSnapshot {
            mark: 100.0,
            best_bid: Some(99.8),
            best_ask: Some(100.2),
        },
        position: c.max_position,
        resting: &[],
        pending_slots: &[],
        market_data_mode: mode,
        active_exit_enabled: true,
        inventory_exit_pct: 80.0,
        inventory_exit_qty: 0.01,
        size_skew: Default::default(),
        nonlinear_skew: Default::default(),
        external_skew: Default::default(),
        external_excess_bps: None,
        guard: Default::default(),
        wind_down,
        qty_tolerance: 0.0005,
    };

    for wind_down in [false, true] {
        let expected_kind = if wind_down {
            ExitKind::WindDown
        } else {
            ExitKind::InventoryTrim
        };
        for halted in [false, true] {
            let plan = plan_cycle(&c, mk(MarketDataMode::Paused, wind_down), halted);
            // Nothing is even requested while the feed is untrusted, so
            // the caller's exit tracking stays untouched.
            assert_eq!(plan.requested_inventory_exit, None);
            assert_eq!(plan.inventory_exit, None);
            assert_eq!(
                plan.exit_suppression,
                Some(SuppressedExit {
                    kind: expected_kind,
                    reason: ExitSuppression::MarketDataInactive,
                }),
                "wind_down={wind_down} halted={halted}"
            );
        }
    }
}

/// Stage 5-b D2: the account hard floors are off by default, equity is
/// checked before margin, and a bad snapshot never trips a stop.
#[test]
fn account_floor_breach_is_opt_in_and_ordered() {
    // Default (0/0) can never fire, whatever the balances look like.
    assert_eq!(account_floor_breach(0.0, 0.0, 0.0, 0.0), None);
    assert_eq!(account_floor_breach(-50.0, -50.0, 0.0, 0.0), None);

    assert_eq!(
        account_floor_breach(99.0, 500.0, 100.0, 0.0),
        Some((AccountFloorBreach::Equity, 99.0, 100.0))
    );
    // Exactly at the floor is not a breach.
    assert_eq!(account_floor_breach(100.0, 500.0, 100.0, 0.0), None);
    assert_eq!(
        account_floor_breach(500.0, 40.0, 0.0, 50.0),
        Some((AccountFloorBreach::Margin, 40.0, 50.0))
    );
    // Both breached: equity is the more fundamental condition and wins.
    assert_eq!(
        account_floor_breach(99.0, 40.0, 100.0, 50.0),
        Some((AccountFloorBreach::Equity, 99.0, 100.0))
    );
    // Unparseable/absent balances arrive as NaN and must not stop a run.
    assert_eq!(account_floor_breach(f64::NAN, f64::NAN, 100.0, 50.0), None);
    assert_eq!(
        account_floor_breach(f64::NAN, 40.0, 100.0, 50.0),
        Some((AccountFloorBreach::Margin, 40.0, 50.0))
    );
}

/// The labels are part of the evidence contract: run manifests and
/// dashboards key off these strings.
#[test]
fn exit_and_floor_labels_are_stable() {
    assert_eq!(ExitKind::InventoryTrim.as_str(), "inventory_trim");
    assert_eq!(ExitKind::WindDown.as_str(), "wind_down");
    assert_eq!(ExitSuppression::VolatilityHalt.as_str(), "volatility_halt");
    assert_eq!(
        ExitSuppression::MarketDataInactive.as_str(),
        "market_data_inactive"
    );
    assert_eq!(AccountFloorBreach::Equity.as_str(), "equity");
    assert_eq!(AccountFloorBreach::Margin.as_str(), "margin");
}

#[test]
fn cycle_plan_reserves_delayed_places_and_caps_directional_exposure() {
    let mut c = cfg();
    c.levels = 2;
    c.max_position = 0.015;
    let pending_slots = [(OrderSide::Buy, 0)];
    let plan = plan_cycle(
        &c,
        CycleInput {
            cycle: 4,
            market: MarketSnapshot {
                mark: 100.0,
                best_bid: Some(99.9),
                best_ask: Some(100.1),
            },
            // The pending 0.01 buy already reserves more than the 0.005
            // remaining long-inventory budget.
            position: 0.01,
            resting: &[],
            pending_slots: &pending_slots,
            market_data_mode: MarketDataMode::Active,
            active_exit_enabled: false,
            inventory_exit_pct: 0.0,
            inventory_exit_qty: 0.0,
            size_skew: Default::default(),
            nonlinear_skew: Default::default(),
            external_skew: Default::default(),
            external_excess_bps: None,
            guard: Default::default(),
            wind_down: false,
            qty_tolerance: 0.0005,
        },
        false,
    );

    let buy_places = plan
        .actions
        .iter()
        .filter_map(|action| match action {
            Action::Place(quote) if quote.side == OrderSide::Buy => Some(quote),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(buy_places.is_empty());
}

#[test]
fn paused_market_data_cancels_without_placing_or_exiting() {
    let c = cfg();
    let resting = vec![
        resting(OrderSide::Buy, 0, 99.9, 100.0),
        resting(OrderSide::Sell, 0, 100.1, 100.0),
    ];
    let plan = plan_cycle(
        &c,
        CycleInput {
            cycle: 5,
            market: MarketSnapshot {
                mark: 100.0,
                best_bid: Some(99.9),
                best_ask: Some(100.1),
            },
            position: c.max_position,
            resting: &resting,
            pending_slots: &[],
            market_data_mode: MarketDataMode::Paused,
            active_exit_enabled: true,
            inventory_exit_pct: 80.0,
            inventory_exit_qty: 0.01,
            size_skew: Default::default(),
            nonlinear_skew: Default::default(),
            external_skew: Default::default(),
            external_excess_bps: None,
            guard: Default::default(),
            wind_down: false,
            qty_tolerance: 0.0005,
        },
        false,
    );

    assert_eq!(plan.requested_inventory_exit, None);
    assert_eq!(plan.inventory_exit, None);
    assert_eq!(
        plan.actions
            .iter()
            .filter(|action| matches!(action, Action::Cancel { .. }))
            .count(),
        resting.len()
    );
    assert!(!plan
        .actions
        .iter()
        .any(|action| matches!(action, Action::Place(_))));
}

#[test]
fn inactive_size_skew_is_exactly_plan_equivalent_across_state_grid() {
    let mut c = cfg();
    c.levels = 2;
    c.max_position = 0.05;
    let resting_sets = [
        Vec::new(),
        vec![
            resting(OrderSide::Buy, 0, 99.9, 100.0),
            resting(OrderSide::Sell, 0, 100.1, 100.0),
        ],
    ];
    let pending_sets = [Vec::new(), vec![(OrderSide::Buy, 0), (OrderSide::Sell, 1)]];
    let inactive = SizeSkewDecision {
        enabled: true,
        active: false,
        add_side: Some(OrderSide::Buy),
        inventory_ratio: 0.99,
        add_qty: Some(c.min_order_qty),
    };

    for position in [-0.025, 0.0, 0.025] {
        for resting in &resting_sets {
            for pending_slots in &pending_sets {
                let input = CycleInput {
                    cycle: 6,
                    market: MarketSnapshot {
                        mark: 100.0,
                        best_bid: Some(99.99),
                        best_ask: Some(100.01),
                    },
                    position,
                    resting,
                    pending_slots,
                    market_data_mode: MarketDataMode::Active,
                    active_exit_enabled: false,
                    inventory_exit_pct: 0.0,
                    inventory_exit_qty: 0.0,
                    size_skew: SizeSkewDecision::default(),
                    nonlinear_skew: Default::default(),
                    external_skew: Default::default(),
                    external_excess_bps: None,
                    guard: Default::default(),
                    wind_down: false,
                    qty_tolerance: 0.0005,
                };
                let default_plan = plan_cycle(&c, input, false);
                let inactive_plan = plan_cycle(
                    &c,
                    CycleInput {
                        size_skew: inactive,
                        nonlinear_skew: Default::default(),
                        guard: Default::default(),
                        ..input
                    },
                    false,
                );
                assert_eq!(inactive_plan, default_plan);
            }
        }
    }
}

#[test]
fn disabled_nonlinear_and_inactive_guard_are_plan_equivalent_across_state_grid() {
    let mut c = cfg();
    c.levels = 2;
    c.max_position = 0.05;
    c.skew_bps = 10.0;
    let resting_sets = [
        Vec::new(),
        vec![
            resting(OrderSide::Buy, 0, 99.9, 100.0),
            resting(OrderSide::Sell, 0, 100.1, 100.0),
        ],
    ];
    // Disabled nonlinear config with aggressive non-default parameters, and
    // an enabled-but-inactive guard: neither may perturb a single action.
    let disabled_nonlinear = NonlinearSkewConfig {
        enabled: false,
        boost: 7.0,
        cap_bps: 9.0,
    };
    let inactive_guard = GuardDecision {
        enabled: true,
        active: false,
        endangered: None,
        divergence_bps: Some(2.0),
    };

    for position in [-0.04, -0.025, 0.0, 0.025, 0.04] {
        for resting in &resting_sets {
            let input = CycleInput {
                cycle: 6,
                market: MarketSnapshot {
                    mark: 100.0,
                    best_bid: Some(99.99),
                    best_ask: Some(100.01),
                },
                position,
                resting,
                pending_slots: &[],
                market_data_mode: MarketDataMode::Active,
                active_exit_enabled: false,
                inventory_exit_pct: 0.0,
                inventory_exit_qty: 0.0,
                size_skew: SizeSkewDecision::default(),
                nonlinear_skew: Default::default(),
                external_skew: Default::default(),
                external_excess_bps: None,
                guard: Default::default(),
                wind_down: false,
                qty_tolerance: 0.0005,
            };
            let default_plan = plan_cycle(&c, input, false);
            let candidate_plan = plan_cycle(
                &c,
                CycleInput {
                    nonlinear_skew: disabled_nonlinear,
                    guard: inactive_guard,
                    ..input
                },
                false,
            );
            assert_eq!(candidate_plan, default_plan);
        }
    }
}

#[test]
fn nonlinear_skew_boost_one_with_high_cap_equals_linear() {
    let mut c = cfg();
    c.skew_bps = 8.0;
    c.max_position = 1.0;
    let nl = NonlinearSkewConfig {
        enabled: true,
        boost: 1.0,
        cap_bps: 8.0,
    };
    for position in [-1.0, -0.6, -0.2, 0.0, 0.2, 0.6, 1.0] {
        assert_eq!(
            skew_center_with(&c, nl, 100.0, position),
            skew_center(&c, 100.0, position),
            "position {position}"
        );
    }
}

#[test]
fn nonlinear_skew_steepens_saturates_and_mirrors() {
    let mut c = cfg();
    c.skew_bps = 8.0;
    c.max_position = 1.0;
    let nl = NonlinearSkewConfig {
        enabled: true,
        boost: 3.0,
        cap_bps: 12.0,
    };
    // ratio 0.2 -> 8*3*0.2 = 4.8bps below mark (long favors selling).
    let long = skew_center_with(&c, nl, 100.0, 0.2);
    assert!((long - (100.0 * (1.0 - 4.8 / 1e4))).abs() < 1e-9);
    // Steeper than linear (linear at 0.2 is 1.6bps).
    assert!(long < skew_center(&c, 100.0, 0.2));
    // ratio 0.5 -> 12bps raw but capped at 12 -> exactly the cap; deeper
    // inventory saturates at the same shift.
    let at_half = skew_center_with(&c, nl, 100.0, 0.5);
    let at_full = skew_center_with(&c, nl, 100.0, 1.0);
    assert!((at_half - (100.0 * (1.0 - 12.0 / 1e4))).abs() < 1e-9);
    assert_eq!(at_half, at_full);
    // Long/short mirror around the mark.
    let short = skew_center_with(&c, nl, 100.0, -0.2);
    assert!((short + long - 200.0).abs() < 1e-9);
    // Zero position never shifts.
    assert_eq!(skew_center_with(&c, nl, 100.0, 0.0), 100.0);
}

#[test]
fn guard_suppresses_endangered_side_and_cancels_its_resting_quotes() {
    let c = cfg();
    let resting = vec![
        resting(OrderSide::Buy, 0, 99.9, 100.0),
        resting(OrderSide::Sell, 0, 100.1, 100.0),
    ];
    let plan = plan_cycle(
        &c,
        CycleInput {
            cycle: 3,
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
            guard: GuardDecision {
                enabled: true,
                active: true,
                endangered: Some(OrderSide::Sell),
                divergence_bps: Some(9.0),
            },
            wind_down: false,
            qty_tolerance: 0.0005,
        },
        false,
    );

    // The endangered sell side: no new quotes, resting cancelled as
    // side-suppressed.
    assert!(!plan
        .actions
        .iter()
        .any(|action| matches!(action, Action::Place(q) if q.side == OrderSide::Sell)));
    assert!(plan.actions.iter().any(|action| matches!(
        action,
        Action::Cancel {
            side: OrderSide::Sell,
            reason: CancelReason::SideSuppressed,
            ..
        }
    )));
    // The safe buy side keeps quoting (hold of the fresh resting quote).
    assert!(plan.actions.iter().any(|action| matches!(
        action,
        Action::Hold {
            side: OrderSide::Buy,
            ..
        }
    )));
}

#[test]
fn combined_high_inventory_and_guard_keeps_all_invariants() {
    let mut c = cfg();
    c.skew_bps = 8.0;
    c.band_bps = 30.0;
    c.max_position = 0.05;
    let nl = NonlinearSkewConfig {
        enabled: true,
        boost: 3.0,
        cap_bps: 12.0,
    };
    // Long 80% of max while the external leader jumps DOWN: guard protects
    // the buy side (stale-rich bids), nonlinear skew is already pushing the
    // center down. Both act in the same defensive direction by design.
    let position = 0.04;
    let plan = plan_cycle(
        &c,
        CycleInput {
            cycle: 9,
            market: MarketSnapshot {
                mark: 100.0,
                best_bid: Some(99.99),
                best_ask: Some(100.01),
            },
            position,
            resting: &[],
            pending_slots: &[],
            market_data_mode: MarketDataMode::Active,
            active_exit_enabled: false,
            inventory_exit_pct: 0.0,
            inventory_exit_qty: 0.0,
            size_skew: Default::default(),
            nonlinear_skew: nl,
            external_skew: Default::default(),
            external_excess_bps: None,
            guard: GuardDecision {
                enabled: true,
                active: true,
                endangered: Some(OrderSide::Buy),
                divergence_bps: Some(-8.0),
            },
            wind_down: false,
            qty_tolerance: 0.0005,
        },
        false,
    );

    let band_lo = 100.0 * (1.0 - c.band_bps / 1e4);
    let band_hi = 100.0 * (1.0 + c.band_bps / 1e4);
    let mut worst_case_long = position;
    for action in &plan.actions {
        if let Action::Place(q) = action {
            // Guard: no buy quotes at all.
            assert_ne!(q.side, OrderSide::Buy);
            // Band and no-cross hold under the combined shift.
            assert!(q.price >= band_lo && q.price <= band_hi);
            assert!(q.price >= 99.99 + c.price_tick() - 1e-9);
            if q.side == OrderSide::Buy {
                worst_case_long += q.qty;
            }
        }
    }
    assert!(worst_case_long <= c.max_position + 1e-9);

    // Release: guard inactive next cycle restores the buy ladder under the
    // same (still skewed) center.
    let released = plan_cycle(
        &c,
        CycleInput {
            cycle: 10,
            market: MarketSnapshot {
                mark: 100.0,
                best_bid: Some(99.99),
                best_ask: Some(100.01),
            },
            position,
            resting: &[],
            pending_slots: &[],
            market_data_mode: MarketDataMode::Active,
            active_exit_enabled: false,
            inventory_exit_pct: 0.0,
            inventory_exit_qty: 0.0,
            size_skew: Default::default(),
            nonlinear_skew: nl,
            external_skew: Default::default(),
            external_excess_bps: None,
            guard: GuardDecision {
                enabled: true,
                active: false,
                endangered: None,
                divergence_bps: Some(1.0),
            },
            wind_down: false,
            qty_tolerance: 0.0005,
        },
        false,
    );
    assert!(released
        .actions
        .iter()
        .any(|action| matches!(action, Action::Place(q) if q.side == OrderSide::Buy)));
}

fn enabled_external_skew() -> ExternalSkewConfig {
    ExternalSkewConfig {
        enabled: true,
        lambda: 0.5,
        cap_bps: 8.0,
        dead_zone_bps: 1.0,
    }
}

#[test]
fn external_skew_composes_after_inventory_skew_deterministically() {
    let mut c = cfg();
    c.skew_bps = 8.0;
    c.max_position = 1.0;
    let nonlinear = NonlinearSkewConfig {
        enabled: true,
        boost: 3.0,
        cap_bps: 12.0,
    };
    let inventory_center = skew_center_with(&c, nonlinear, 100.0, 0.25);
    let composed = quote_center(&c, nonlinear, 3.5, 100.0, 0.25);
    assert_eq!(composed, inventory_center * (1.0 + 3.5 / 1e4));
}

#[test]
fn external_skew_disabled_is_exactly_plan_equivalent() {
    let mut c = cfg();
    c.skew_bps = 8.0;
    let input = CycleInput {
        cycle: 11,
        market: MarketSnapshot {
            mark: 100.0,
            best_bid: Some(99.8),
            best_ask: Some(100.2),
        },
        position: 0.02,
        resting: &[],
        pending_slots: &[],
        market_data_mode: MarketDataMode::Active,
        active_exit_enabled: false,
        inventory_exit_pct: 0.0,
        inventory_exit_qty: 0.0,
        size_skew: Default::default(),
        nonlinear_skew: Default::default(),
        external_skew: Default::default(),
        external_excess_bps: None,
        guard: Default::default(),
        wind_down: false,
        qty_tolerance: 0.0005,
    };
    let baseline = plan_cycle(&c, input, false);
    let explicitly_disabled = plan_cycle(
        &c,
        CycleInput {
            external_skew: ExternalSkewConfig {
                enabled: false,
                lambda: 7.0,
                cap_bps: 99.0,
                dead_zone_bps: 0.0,
            },
            external_excess_bps: Some(40.0),
            ..input
        },
        false,
    );

    assert_eq!(explicitly_disabled.actions, baseline.actions);
    assert_eq!(
        explicitly_disabled.ref_center.to_bits(),
        baseline.ref_center.to_bits()
    );
    assert_eq!(
        explicitly_disabled.external_skew_shift_bps.to_bits(),
        0.0_f64.to_bits()
    );
}

#[test]
fn external_skew_still_shifts_surviving_side_while_guard_is_active() {
    let c = cfg();
    let guard = GuardDecision {
        enabled: true,
        active: true,
        endangered: Some(OrderSide::Sell),
        divergence_bps: Some(7.0),
    };
    let make_input = |external_skew, external_excess_bps| CycleInput {
        cycle: 12,
        market: MarketSnapshot {
            mark: 100.0,
            best_bid: Some(99.8),
            best_ask: Some(100.2),
        },
        position: 0.0,
        resting: &[],
        pending_slots: &[],
        market_data_mode: MarketDataMode::Active,
        active_exit_enabled: false,
        inventory_exit_pct: 0.0,
        inventory_exit_qty: 0.0,
        size_skew: Default::default(),
        nonlinear_skew: Default::default(),
        external_skew,
        external_excess_bps,
        guard,
        wind_down: false,
        qty_tolerance: 0.0005,
    };
    let baseline = plan_cycle(&c, make_input(Default::default(), Some(7.0)), false);
    let shifted = plan_cycle(&c, make_input(enabled_external_skew(), Some(7.0)), false);
    let buy_price = |plan: &CyclePlan| {
        plan.actions
            .iter()
            .find_map(|action| match action {
                Action::Place(quote) if quote.side == OrderSide::Buy => Some(quote.price),
                _ => None,
            })
            .expect("guard-surviving buy quote")
    };

    assert!(buy_price(&shifted) > buy_price(&baseline));
    assert!(shifted
        .actions
        .iter()
        .all(|action| !matches!(action, Action::Place(q) if q.side == OrderSide::Sell)));
    assert_eq!(shifted.external_skew_shift_bps, 3.5);
}

#[test]
fn shared_external_quote_center_prevents_false_refresh() {
    let c = cfg();
    let market = MarketSnapshot {
        mark: 100.0,
        best_bid: Some(99.8),
        best_ask: Some(100.2),
    };
    let first = plan_cycle(
        &c,
        CycleInput {
            cycle: 20,
            market,
            position: 0.0,
            resting: &[],
            pending_slots: &[],
            market_data_mode: MarketDataMode::Active,
            active_exit_enabled: false,
            inventory_exit_pct: 0.0,
            inventory_exit_qty: 0.0,
            size_skew: Default::default(),
            nonlinear_skew: Default::default(),
            external_skew: enabled_external_skew(),
            external_excess_bps: Some(8.0),
            guard: Default::default(),
            wind_down: false,
            qty_tolerance: 0.0005,
        },
        false,
    );
    assert_eq!(first.external_skew_shift_bps, 4.0);
    assert_eq!(
        first.ref_center.to_bits(),
        quote_center(&c, Default::default(), 4.0, 100.0, 0.0).to_bits()
    );
    let resting = first
        .actions
        .iter()
        .filter_map(|action| match action {
            Action::Place(quote) => Some(RestingQuote {
                order_id: Some(format!("{:?}-{}", quote.side, quote.level)),
                side: quote.side,
                level: quote.level,
                price: quote.price,
                qty: quote.qty,
                ref_center: first.ref_center,
                placed_at_cycle: 20,
            }),
            _ => None,
        })
        .collect::<Vec<_>>();
    let second = plan_cycle(
        &c,
        CycleInput {
            cycle: 21,
            market,
            position: 0.0,
            resting: &resting,
            pending_slots: &[],
            market_data_mode: MarketDataMode::Active,
            active_exit_enabled: false,
            inventory_exit_pct: 0.0,
            inventory_exit_qty: 0.0,
            size_skew: Default::default(),
            nonlinear_skew: Default::default(),
            external_skew: enabled_external_skew(),
            external_excess_bps: Some(8.0),
            guard: Default::default(),
            wind_down: false,
            qty_tolerance: 0.0005,
        },
        false,
    );
    assert!(second
        .actions
        .iter()
        .all(|action| matches!(action, Action::Hold { .. })));
    assert!(!second.actions.iter().any(|action| matches!(
        action,
        Action::Cancel {
            reason: CancelReason::MarkMovedBeyondRefresh,
            ..
        }
    )));
}

#[test]
fn external_skew_over_band_is_clamped_not_dropped_if_validation_is_bypassed() {
    let mut c = cfg();
    c.band_bps = 20.0;
    let quotes = compute_desired_quotes(
        &c,
        100.0,
        Some(99.5),
        Some(100.5),
        0.0,
        Default::default(),
        Default::default(),
        30.0,
        Default::default(),
    );
    let sell = find(&quotes, OrderSide::Sell, 0);
    assert_eq!(sell.price, 100.20);
}

// 28. Alert monitor: disabled emits nothing.
#[test]
fn alerts_disabled() {
    let mut m = AlertMonitor::new(0.0, 0.0, 0.0);
    assert!(!m.session_enabled());
    let s = MakerStats::default();
    assert!(m.evaluate(&s, 5.0, 100.0, 0.05, 100).is_empty());
}

// 29. Loss alert: edge-triggered fire then clear.
#[test]
fn alerts_loss_edge() {
    let mut m = AlertMonitor::new(1.0, 0.0, 0.0); // loss limit 1.0
    let mut s = MakerStats::default();
    // Buy 1 @ 100 (cash -100), mark drops to 98 -> pnl = -100 + 1*98 = -2.
    s.record_fill(OrderSide::Buy, 100.0, 1.0, 100.0);
    let a = m.evaluate(&s, 1.0, 98.0, 0.05, 5);
    assert_eq!(a.len(), 1);
    assert_eq!(a[0].kind, "loss");
    assert!(a[0].firing);
    // Held breach -> no repeat.
    assert!(m.evaluate(&s, 1.0, 98.0, 0.05, 6).is_empty());
    // Recover above -limit/2 (pnl at mark 100 = 0) -> clear.
    let a = m.evaluate(&s, 1.0, 100.0, 0.05, 7);
    assert_eq!(a.len(), 1);
    assert!(!a[0].firing);
}

// 30. Inventory alert fires at the configured pct of max.
#[test]
fn alerts_inventory_pct() {
    let mut m = AlertMonitor::new(0.0, 80.0, 0.0); // 80% of max
    let s = MakerStats::default();
    // max 0.05 -> threshold 0.04. Position 0.03 -> no alert.
    assert!(m.evaluate(&s, 0.03, 100.0, 0.05, 5).is_empty());
    // 0.045 >= 0.04 -> fire.
    let a = m.evaluate(&s, 0.045, 100.0, 0.05, 6);
    assert_eq!(a.len(), 1);
    assert_eq!(a[0].kind, "inventory");
    assert!(a[0].firing);
    // Short side symmetric: still on (held), no repeat.
    assert!(m.evaluate(&s, 0.045, 100.0, 0.05, 7).is_empty());
}

// 31. Uptime alert waits for warmup.
#[test]
fn alerts_uptime_warmup() {
    let mut m = AlertMonitor::new(0.0, 0.0, 50.0); // floor 50%
    let mut s = MakerStats::default();
    // One one-sided cycle -> uptime 0%, but before warmup: no alert.
    s.end_cycle(0.0, false);
    assert!(m.evaluate(&s, 0.0, 100.0, 0.05, 5).is_empty());
    // After warmup, still 0% < 50% -> fire.
    let a = m.evaluate(&s, 0.0, 100.0, 0.05, 25);
    assert_eq!(a.len(), 1);
    assert_eq!(a[0].kind, "uptime");
    assert!(a[0].firing);
}

// 32. Account floors disabled by default; account_enabled reflects config.
#[test]
fn alerts_account_disabled_by_default() {
    let mut m = AlertMonitor::new(0.0, 0.0, 0.0);
    assert!(!m.account_enabled());
    assert!(m.evaluate_account(10.0, 5.0).is_empty());
}

// 33. Equity floor: edge-triggered fire then clear with hysteresis.
#[test]
fn alerts_equity_alert_edge() {
    let mut m = AlertMonitor::new(0.0, 0.0, 0.0).with_account_alerts(100.0, 0.0);
    assert!(m.account_enabled());
    // Above floor -> no alert.
    assert!(m.evaluate_account(120.0, 999.0).is_empty());
    // Drop below floor -> fire.
    let a = m.evaluate_account(90.0, 999.0);
    assert_eq!(a.len(), 1);
    assert_eq!(a[0].kind, "equity");
    assert!(a[0].firing);
    // Held breach -> no repeat.
    assert!(m.evaluate_account(90.0, 999.0).is_empty());
    // Back above floor but within hysteresis band (< 110) -> still on.
    assert!(m.evaluate_account(105.0, 999.0).is_empty());
    // Recover to >= floor*1.1 -> clear.
    let a = m.evaluate_account(111.0, 999.0);
    assert_eq!(a.len(), 1);
    assert_eq!(a[0].kind, "equity");
    assert!(!a[0].firing);
}

// 34. Available-margin floor fires independently of equity.
#[test]
fn alerts_margin_alert_fires() {
    let mut m = AlertMonitor::new(0.0, 0.0, 0.0).with_account_alerts(0.0, 50.0);
    assert!(m.evaluate_account(9999.0, 60.0).is_empty());
    let a = m.evaluate_account(9999.0, 40.0);
    assert_eq!(a.len(), 1);
    assert_eq!(a[0].kind, "margin");
    assert!(a[0].firing);
}

// 35. Wind-down: any residual position yields a full reduce-only exit
// plan and no new quotes, even with configured exits fully disabled
// (the frozen live configs). A vol halt suppresses the taker exit but
// never the quote suppression.
#[test]
fn wind_down_flattens_residual_and_stops_quoting() {
    let c = cfg();
    let resting = vec![
        resting(OrderSide::Buy, 0, 99.9, 100.0),
        resting(OrderSide::Sell, 0, 100.1, 100.0),
    ];
    let input = CycleInput {
        cycle: 7,
        market: MarketSnapshot {
            mark: 100.0,
            best_bid: Some(99.9),
            best_ask: Some(100.1),
        },
        position: -0.02,
        resting: &resting,
        pending_slots: &[],
        market_data_mode: MarketDataMode::Active,
        active_exit_enabled: true,
        inventory_exit_pct: 0.0,
        inventory_exit_qty: 0.0,
        size_skew: Default::default(),
        nonlinear_skew: Default::default(),
        external_skew: Default::default(),
        external_excess_bps: None,
        guard: Default::default(),
        wind_down: true,
        qty_tolerance: 0.0005,
    };
    let plan = plan_cycle(&c, input, false);
    assert_eq!(
        plan.requested_inventory_exit,
        Some(InventoryExit {
            side: OrderSide::Buy,
            qty: 0.02,
            kind: ExitKind::WindDown,
        })
    );
    assert_eq!(plan.inventory_exit, plan.requested_inventory_exit);
    assert!(plan
        .actions
        .iter()
        .any(|action| matches!(action, Action::Cancel { .. })));
    assert!(!plan
        .actions
        .iter()
        .any(|action| matches!(action, Action::Place(_))));

    let halted = plan_cycle(&c, input, true);
    assert_eq!(halted.inventory_exit, None);
    assert!(!halted
        .actions
        .iter()
        .any(|action| matches!(action, Action::Place(_))));
}

// 36. Wind-down keeps quotes off even when already flat, so inventory
// cannot re-accumulate while the supervisor waits to switch.
#[test]
fn wind_down_flat_still_suppresses_quotes() {
    let c = cfg();
    let plan = plan_cycle(
        &c,
        CycleInput {
            cycle: 8,
            market: MarketSnapshot {
                mark: 100.0,
                best_bid: Some(99.9),
                best_ask: Some(100.1),
            },
            position: 0.0,
            resting: &[],
            pending_slots: &[],
            market_data_mode: MarketDataMode::Active,
            active_exit_enabled: true,
            inventory_exit_pct: 0.0,
            inventory_exit_qty: 0.0,
            size_skew: Default::default(),
            nonlinear_skew: Default::default(),
            external_skew: Default::default(),
            external_excess_bps: None,
            guard: Default::default(),
            wind_down: true,
            qty_tolerance: 0.0005,
        },
        false,
    );
    assert_eq!(plan.requested_inventory_exit, None);
    assert_eq!(plan.inventory_exit, None);
    assert!(plan.actions.is_empty());
}

// 37. Wind-down overrides the configured exit threshold (a residual
// below the enabled trigger still exits, both sides) and treats
// positions at or below the quantity tolerance as flat.
#[test]
fn wind_down_overrides_threshold_and_honors_tolerance() {
    let c = cfg();
    let mk = |position: f64| CycleInput {
        cycle: 9,
        market: MarketSnapshot {
            mark: 100.0,
            best_bid: Some(99.9),
            best_ask: Some(100.1),
        },
        position,
        resting: &[],
        pending_slots: &[],
        market_data_mode: MarketDataMode::Active,
        active_exit_enabled: true,
        inventory_exit_pct: 80.0,
        inventory_exit_qty: 0.01,
        size_skew: Default::default(),
        nonlinear_skew: Default::default(),
        external_skew: Default::default(),
        external_excess_bps: None,
        guard: Default::default(),
        wind_down: true,
        qty_tolerance: 0.0005,
    };
    // 0.02 is below the configured 80%-of-max trigger (0.04): the
    // configured path stays inactive, wind-down exits everything.
    let long = plan_cycle(&c, mk(0.02), false);
    assert_eq!(
        long.inventory_exit,
        Some(InventoryExit {
            side: OrderSide::Sell,
            qty: 0.02,
            kind: ExitKind::WindDown,
        })
    );
    assert_eq!(plan_cycle(&c, mk(0.0005), false).inventory_exit, None);
    assert_eq!(plan_cycle(&c, mk(-0.0005), false).inventory_exit, None);
    assert_eq!(
        plan_cycle(&c, mk(-0.0006), false).inventory_exit,
        Some(InventoryExit {
            side: OrderSide::Buy,
            qty: 0.0006,
            kind: ExitKind::WindDown,
        })
    );
}
