use super::*;

fn drain_positions(events: Vec<AccountEvent>) -> AccountEventOutcome {
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    for event in events {
        tx.try_send(event).unwrap();
    }
    let mut ledger = MakerLedger::new(0.0);
    let mut stats = MakerStats::default();
    let mut projection = MakerAccountProjection::new(1, "sxmk-test-", 0.0, 0.005, 0.00005);
    let mut state = AccountEventState {
        ledger: &mut ledger,
        stats: &mut stats,
        projection: &mut projection,
    };
    let context = AccountEventContext {
        symbol: "BTC-USD",
        run_order_prefix: "sxmk-test-",
        mark: 100.0,
        cycle: 1,
        output_format: OutputFormat::Quiet,
        excess_bps_at_fill: None,
    };
    apply_account_events(&mut rx, &mut state, &context).expect("benign events drain cleanly")
}

#[test]
fn apply_account_events_records_position_mismatch_with_sign() {
    let buy = drain_positions(vec![AccountEvent::Position(position_update(
        "BTC-USD",
        Some(OrderSide::Buy),
        "0.5",
    ))]);
    assert_eq!(buy.position_observations.last().copied(), Some(0.5));

    let sell = drain_positions(vec![AccountEvent::Position(position_update(
        "BTC-USD",
        Some(OrderSide::Sell),
        "0.5",
    ))]);
    assert_eq!(
        sell.position_observations.last().copied(),
        Some(-0.5),
        "sell position is negative"
    );
}

#[test]
fn apply_account_events_applies_buffered_events_in_order() {
    // The last position update remains available for reconciliation, while
    // every ordered observation is retained for direction-risk state.
    let outcome = drain_positions(vec![
        AccountEvent::Connected { epoch: 1 },
        AccountEvent::Position(position_update("BTC-USD", Some(OrderSide::Buy), "0.2")),
        AccountEvent::Balance(standx_sdk::account_stream::BalanceUpdate {
            seq: 1,
            account_type: "perps".to_string(),
            token: "DUSD".to_string(),
            free: "1".to_string(),
            total: "1".to_string(),
            locked: "0".to_string(),
            occupied: "0".to_string(),
            updated_at: "2026-07-14T00:00:00Z".to_string(),
        }),
        AccountEvent::Position(position_update("BTC-USD", Some(OrderSide::Sell), "0.9")),
    ]);
    assert_eq!(outcome.fills, 0);
    assert_eq!(outcome.position_observations, vec![0.2, -0.9]);
    assert_eq!(
        outcome.position_observations.last().copied(),
        Some(-0.9),
        "latest position reflects last update"
    );
}

#[test]
fn balance_event_updates_raw_projection_without_touching_fill_accounting() {
    let mut ledger = MakerLedger::new(0.0);
    let mut stats = MakerStats::default();
    let mut projection = MakerAccountProjection::new(1, "sxmk-test-", 0.0, 0.005, 0.00005);
    let context = AccountEventContext {
        symbol: "BTC-USD",
        run_order_prefix: "sxmk-test-",
        mark: 100.0,
        cycle: 1,
        output_format: OutputFormat::Quiet,
        excess_bps_at_fill: None,
    };
    let outcome = {
        let mut state = AccountEventState {
            ledger: &mut ledger,
            stats: &mut stats,
            projection: &mut projection,
        };
        apply_account_event(
            AccountEvent::Balance(standx_sdk::account_stream::BalanceUpdate {
                seq: 1,
                account_type: "perps".to_string(),
                token: "DUSD".to_string(),
                free: "90".to_string(),
                total: "100".to_string(),
                locked: "0".to_string(),
                occupied: "10".to_string(),
                updated_at: "2026-07-14T00:00:00Z".to_string(),
            }),
            &mut state,
            &context,
        )
        .unwrap()
    };
    assert_eq!(outcome.fills, 0);
    // A balance event requests a REST refresh but does not mutate the
    // projection (raw wallet fields are not projected).
    assert!(outcome.balance_changed);
    assert_eq!(stats.fills(), 0);
}

#[test]
fn uncorrelated_current_run_order_requires_reconciliation_without_stream_failure() {
    let mut ledger = MakerLedger::new(0.0);
    let mut stats = MakerStats::default();
    let mut projection = MakerAccountProjection::new(1, "sxmk-test-", 0.0, 0.005, 0.00005);
    let mut state = AccountEventState {
        ledger: &mut ledger,
        stats: &mut stats,
        projection: &mut projection,
    };
    let context = AccountEventContext {
        symbol: "BTC-USD",
        run_order_prefix: "sxmk-test-",
        mark: 100.0,
        cycle: 2,
        output_format: OutputFormat::Quiet,
        excess_bps_at_fill: None,
    };
    let update = OrderUpdate {
        seq: 1,
        order_id: 7,
        cl_ord_id: Some("sxmk-test-q00000001b0".to_string()),
        symbol: "BTC-USD".to_string(),
        side: OrderSide::Buy,
        qty: "0.2".to_string(),
        fill_qty: "0".to_string(),
        fill_avg_price: "0".to_string(),
        price: "100".to_string(),
        status: standx_sdk::models::OrderStatus::Open,
        reduce_only: false,
        updated_at: "2026-07-15T00:00:00Z".to_string(),
    };

    let outcome = apply_account_event(AccountEvent::Order(update), &mut state, &context)
        .expect("a late current-run order is a reconciliation trigger, not stream failure");

    assert!(outcome.requires_order_reconciliation);
    assert_eq!(outcome.fills, 0);
}

#[test]
fn typed_trade_event_is_booked_once_after_order_ownership() {
    let order = standx_sdk::account_stream::OrderUpdate {
        seq: 1,
        order_id: 7,
        cl_ord_id: Some("sxmk-test-q00000001b0".to_string()),
        symbol: "BTC-USD".to_string(),
        side: OrderSide::Buy,
        qty: "0.2".to_string(),
        fill_qty: "0.2".to_string(),
        fill_avg_price: "100".to_string(),
        price: "100".to_string(),
        status: standx_sdk::models::OrderStatus::Filled,
        reduce_only: false,
        updated_at: "2026-07-14T00:00:00Z".to_string(),
    };
    let trade = standx_sdk::account_stream::TradeUpdate {
        seq: 2,
        trade_id: 11,
        order_id: 7,
        symbol: "BTC-USD".to_string(),
        side: OrderSide::Buy,
        price: "100".to_string(),
        qty: "0.2".to_string(),
        trade_ts: "2026-07-14T00:00:00Z".to_string(),
    };

    let outcome = drain_positions(vec![
        AccountEvent::Order(order),
        AccountEvent::Trade(trade.clone()),
        AccountEvent::Trade(trade),
    ]);
    assert_eq!(outcome.fills, 1);
    assert_eq!(outcome.position_observations.last().copied(), None);
}

#[test]
fn apply_account_events_ignores_other_symbols() {
    let outcome = drain_positions(vec![AccountEvent::Position(position_update(
        "ETH-USD",
        Some(OrderSide::Buy),
        "1.0",
    ))]);
    assert_eq!(outcome.fills, 0);
    assert_eq!(
        outcome.position_observations.last().copied(),
        None,
        "position updates for other symbols are ignored"
    );
}

#[test]
fn stable_trade_reports_current_run_inventory_exit_once() {
    let mut ledger = MakerLedger::new(0.2);
    let mut stats = MakerStats::with_inventory_baseline(0.2, 100.0);
    let mut projection = MakerAccountProjection::new(1, "sxmk-test-", 0.2, 0.005, 0.00005);
    let mut state = AccountEventState {
        ledger: &mut ledger,
        stats: &mut stats,
        projection: &mut projection,
    };
    let context = AccountEventContext {
        symbol: "BTC-USD",
        run_order_prefix: "sxmk-test-",
        mark: 100.0,
        cycle: 1,
        output_format: OutputFormat::Quiet,
        excess_bps_at_fill: None,
    };
    let update = OrderUpdate {
        seq: 1,
        order_id: 7,
        cl_ord_id: Some("sxmk-test-x00000001".to_string()),
        symbol: "BTC-USD".to_string(),
        side: OrderSide::Sell,
        qty: "0.2".to_string(),
        fill_qty: "0.2".to_string(),
        fill_avg_price: "100".to_string(),
        price: "100".to_string(),
        status: standx_sdk::models::OrderStatus::Filled,
        reduce_only: true,
        updated_at: "2026-07-14T00:00:00Z".to_string(),
    };

    let order = apply_account_event(AccountEvent::Order(update), &mut state, &context)
        .expect("exit order is valid");
    assert_eq!(order.fills, 0);
    assert!(!order.exit_fill_observed);

    let trade = standx_sdk::account_stream::TradeUpdate {
        seq: 2,
        trade_id: 11,
        order_id: 7,
        symbol: "BTC-USD".to_string(),
        side: OrderSide::Sell,
        price: "100".to_string(),
        qty: "0.2".to_string(),
        trade_ts: "2026-07-14T00:00:00Z".to_string(),
    };
    let first = apply_account_event(AccountEvent::Trade(trade.clone()), &mut state, &context)
        .expect("exit trade is valid");
    assert_eq!(first.fills, 1);
    assert!(first.exit_fill_observed);

    let duplicate = apply_account_event(AccountEvent::Trade(trade), &mut state, &context)
        .expect("duplicate exit fill is valid");
    assert_eq!(duplicate.fills, 0);
    assert!(!duplicate.exit_fill_observed);
}

#[test]
fn accounting_position_mismatch_respects_half_tick_tolerance() {
    let tolerance = 0.0005;
    assert!(!accounting_position_mismatch(0.2, 0.20049, tolerance));
    assert!(accounting_position_mismatch(0.2, 0.20051, tolerance));
    assert!(!accounting_position_mismatch(-0.2, -0.20049, tolerance));
    assert!(accounting_position_mismatch(-0.2, -0.20051, tolerance));
}

#[test]
fn accounting_position_mismatch_fails_closed_on_non_finite() {
    let tolerance = 0.0005;
    assert!(accounting_position_mismatch(f64::NAN, 0.2, tolerance));
    assert!(accounting_position_mismatch(0.2, f64::NAN, tolerance));
    assert!(accounting_position_mismatch(f64::INFINITY, 0.2, tolerance));
}

#[tokio::test]
async fn accounting_invariant_mismatch_becomes_fail_safe_exit() {
    let notifier = MakerNotifier::new(
        OutputFormat::Quiet,
        None,
        crate::cli::AlertWebhookFormat::Raw,
    );

    assert!(
        accounting_invariant_exit(&notifier, "XAG-USD", 1396, 0.0, -0.2, 0.0005,)
            .await
            .is_some_and(|detail| detail.contains("differs from ledger expected"))
    );
    assert!(
        accounting_invariant_exit(&notifier, "XAG-USD", 1396, 0.0, 0.00049, 0.0005,)
            .await
            .is_none()
    );
}

// ---- Fault-injection conformance tests for the shared recovery helpers ----

// A dropped account-stream channel must surface as the typed, downcast-able
// transport error (account-stream-disconnect retry), never as a generic
// validation failure. This is the contract `recovery_flow` relies on to decide
// between a *bounded retry* (transport) and a *fail-closed* exit (safety).
// Regression for the 2026-08-10 incident: mid-backfill disconnect was being
// misclassified as event-validation failure and failed the whole run.
#[test]
fn apply_account_events_disconnect_is_typed_transport_error() {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<AccountEvent>(16);
    drop(tx); // the authenticated stream channel closed

    let mut ledger = MakerLedger::new(0.0);
    let mut stats = MakerStats::default();
    let mut projection = MakerAccountProjection::new(1, "sxmk-test-", 0.0, 0.005, 0.00005);
    let mut state = AccountEventState {
        ledger: &mut ledger,
        stats: &mut stats,
        projection: &mut projection,
    };
    let context = AccountEventContext {
        symbol: "BTC-USD",
        run_order_prefix: "sxmk-test-",
        mark: 100.0,
        cycle: 1,
        output_format: OutputFormat::Quiet,
        excess_bps_at_fill: None,
    };

    let error = apply_account_events(&mut rx, &mut state, &context)
        .expect_err("a dropped receiver must error");

    // 1. It is the typed transport error, not an untyped validation error.
    assert!(
        error.downcast_ref::<AccountStreamDisconnected>().is_some(),
        "disconnect must downcast to AccountStreamDisconnected, got: {error}"
    );
    // 2. Display preserves the historical message so log/reason strings are unchanged.
    assert_eq!(
        error.to_string(),
        "authenticated account stream disconnected"
    );
    // 3. Other error types are NOT treated as the transport error (so real
    //    validation failures still fail closed).
    let unrelated = anyhow::anyhow!("event validation failed");
    assert!(unrelated
        .downcast_ref::<AccountStreamDisconnected>()
        .is_none());
}

// Disconnect mid-drain: a channel that yields events then disconnects must
// still surface the typed transport error (not silently succeed with partial
// events), so the recovery loop re-runs the backfill round rather than resuming
// quoting on a stream that is already gone.
#[test]
fn apply_account_events_disconnects_mid_drain() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    tx.try_send(AccountEvent::Connected { epoch: 1 }).unwrap();
    tx.try_send(AccountEvent::Position(position_update(
        "BTC-USD",
        Some(OrderSide::Buy),
        "0.5",
    )))
    .unwrap();
    drop(tx); // stream drops after buffering events

    let mut ledger = MakerLedger::new(0.0);
    let mut stats = MakerStats::default();
    let mut projection = MakerAccountProjection::new(1, "sxmk-test-", 0.0, 0.005, 0.00005);
    let mut state = AccountEventState {
        ledger: &mut ledger,
        stats: &mut stats,
        projection: &mut projection,
    };
    let context = AccountEventContext {
        symbol: "BTC-USD",
        run_order_prefix: "sxmk-test-",
        mark: 100.0,
        cycle: 1,
        output_format: OutputFormat::Quiet,
        excess_bps_at_fill: None,
    };

    let error = apply_account_events(&mut rx, &mut state, &context)
        .expect_err("an early disconnect after buffered events must error");
    assert!(
        error.downcast_ref::<AccountStreamDisconnected>().is_some(),
        "mid-drain disconnect must be typed as transport fault, got: {error}"
    );
}
