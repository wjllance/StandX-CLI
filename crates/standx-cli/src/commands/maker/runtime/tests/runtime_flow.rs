use super::*;
use crate::commands::maker::startup::new_maker_rest_client;

#[test]
fn stop_directive_never_becomes_a_cycle_attempt() {
    let mut runtime_state = MakerState::starting();
    runtime_state.handle(MakerEvent::StopRequested(RuntimeStopReason::CtrlC));

    let exit = take_cycle_work(&mut runtime_state).expect_err("stop must exit before cycle work");
    assert!(matches!(exit, MakerExit::CtrlC));
}

#[test]
fn matching_cycle_completion_opens_the_commit_gate() {
    let mut runtime_state = MakerState::starting();
    runtime_state.handle(MakerEvent::StartupReady);
    let token = take_cycle_work(&mut runtime_state)
        .expect("cycle work lookup succeeds")
        .expect("startup schedules cycle work");

    assert!(commit_cycle_effect(&mut runtime_state, token));
}

#[test]
fn stale_cycle_completion_cannot_open_the_commit_gate() {
    let mut runtime_state = MakerState::starting();
    runtime_state.handle(MakerEvent::StartupReady);
    let token = take_cycle_work(&mut runtime_state)
        .expect("cycle work lookup succeeds")
        .expect("startup schedules cycle work");
    runtime_state.handle(MakerEvent::PositionMismatch);

    assert!(!commit_cycle_effect(&mut runtime_state, token));
}

#[test]
fn request_timeout_detail_identifies_request_kind_and_missing_phase() {
    let detail = order_request_timeout_detail(&TimedOutOrderRequest {
        request_id: "request-7".to_string(),
        kind: OrderRequestKind::Cancel,
        phase: RequestTimeoutPhase::AccountOrder,
        age: Duration::from_millis(10_250),
    });

    assert_eq!(
        detail,
        "order request lifecycle timed out after 10.250s: kind=cancel request_id=request-7 waiting_for=account_order; refusing further live orders"
    );
}

#[test]
fn request_timeout_marks_only_the_stream_responsible_for_the_missing_phase() {
    for (phase, account_healthy, order_response_healthy) in [
        (RequestTimeoutPhase::Acknowledgement, true, false),
        (RequestTimeoutPhase::AccountOrder, false, true),
    ] {
        let account = standx_sdk::account_stream::AccountStreamHealth::new(1);
        let order_response = OrderResponseHealth::default();
        let timeout = TimedOutOrderRequest {
            request_id: "request-7".to_string(),
            kind: OrderRequestKind::Place,
            phase,
            age: ORDER_REQUEST_TIMEOUT,
        };

        mark_request_timeout_stream_unhealthy(
            &account,
            &order_response,
            &timeout,
            "request timed out",
        );

        assert_eq!(account.is_healthy(), account_healthy);
        assert_eq!(order_response.is_healthy(), order_response_healthy);
    }
}

/// Pins the per-flow mapping from a missing/mismatched runtime effect to
/// its stop reason: the order-response flow stops as OrderResponse even
/// for cleanup failures, while the other flows carry the target through
/// CleanupFailure or fall back to PositionReconciliation.
#[test]
fn effect_failure_stop_maps_each_flow_variant() {
    assert_eq!(
        effect_failure_stop(
            EffectFailureStop::CleanupFailure,
            RecoveryTarget::AccountStream,
            "boom".to_string(),
        ),
        RuntimeStopReason::CleanupFailure {
            target: RecoveryTarget::AccountStream,
            reason: "boom".to_string(),
        }
    );
    assert_eq!(
        effect_failure_stop(
            EffectFailureStop::CleanupFailure,
            RecoveryTarget::PositionReconciliation,
            "boom".to_string(),
        ),
        RuntimeStopReason::CleanupFailure {
            target: RecoveryTarget::PositionReconciliation,
            reason: "boom".to_string(),
        }
    );
    assert_eq!(
        effect_failure_stop(
            EffectFailureStop::OrderResponse,
            RecoveryTarget::OrderResponse,
            "boom".to_string(),
        ),
        RuntimeStopReason::OrderResponse("boom".to_string())
    );
    assert_eq!(
        effect_failure_stop(
            EffectFailureStop::PositionReconciliation,
            RecoveryTarget::AccountStream,
            "boom".to_string(),
        ),
        RuntimeStopReason::PositionReconciliation("boom".to_string())
    );
    assert_eq!(
        effect_failure_stop(
            EffectFailureStop::MarketData,
            RecoveryTarget::MarketData,
            "boom".to_string(),
        ),
        RuntimeStopReason::MarketData("boom".to_string())
    );
}

#[test]
fn maker_rest_client_is_isolated_from_order_response_session() {
    let endpoints = standx_sdk::StandXEndpoints::new("https://perps.example.com").unwrap();
    let client = new_maker_rest_client(&endpoints).expect("maker REST client is constructible");
    assert_eq!(client.base_url(), "https://perps.example.com");
    assert_eq!(client.session_id(), None);
}

#[test]
fn runtime_effect_executor_orders_abort_cleanup_and_recovery() {
    let mut runtime_state = MakerState::starting();
    runtime_state.handle(MakerEvent::StartupReady);
    let cycle_token = match runtime_state.next_effect() {
        Some(MakerEffect::RunCycle(token)) => token,
        effect => panic!("expected cycle effect, got {effect:?}"),
    };

    runtime_state.handle(MakerEvent::PositionMismatch);
    let cleanup = take_cleanup_effect(&mut runtime_state, RecoveryTarget::PositionReconciliation)
        .expect("abort must be drained before cleanup");
    runtime_state.handle(MakerEvent::CycleCompleted(cycle_token));
    assert!(runtime_state.pending_effect().is_none());

    runtime_state.handle(MakerEvent::CleanupCompleted(cleanup));
    let recovery = take_recovery_effect(&mut runtime_state, RecoveryTarget::PositionReconciliation)
        .expect("cleanup completion must schedule recovery");
    runtime_state.handle(MakerEvent::RecoverySucceeded(recovery));
    assert!(matches!(
        runtime_state.next_effect(),
        Some(MakerEffect::RunCycle(_))
    ));
}

#[test]
fn ws_balance_request_schedules_immediate_authoritative_refresh_for_account_alerts() {
    let now = std::time::Instant::now();
    let mut poll = LiveAccountPollState::new(account_balance(), now);
    let mut requested = true;

    assert!(!poll.balance_refresh_due(now));
    assert!(schedule_account_balance_refresh(
        &mut requested,
        true,
        &mut poll,
        now,
    ));
    assert!(!requested);
    assert!(poll.balance_refresh_due(now));

    let mut disabled_request = true;
    let later = now + Duration::from_secs(1);
    poll.record_balance_refresh(account_balance(), later);
    assert!(!schedule_account_balance_refresh(
        &mut disabled_request,
        false,
        &mut poll,
        later,
    ));
    assert!(!disabled_request);
    assert!(!poll.balance_refresh_due(later));
}

#[test]
fn runtime_recovery_failure_is_the_stop_source_of_truth() {
    let mut runtime_state = MakerState::starting();
    runtime_state.handle(MakerEvent::StartupReady);
    let _ = runtime_state.next_effect();
    runtime_state.handle(MakerEvent::OrderResponseDisconnected("closed".to_string()));
    let cleanup = take_cleanup_effect(&mut runtime_state, RecoveryTarget::OrderResponse)
        .expect("cleanup effect");
    runtime_state.handle(MakerEvent::CleanupCompleted(cleanup));
    let recovery = take_recovery_effect(&mut runtime_state, RecoveryTarget::OrderResponse)
        .expect("recovery effect");
    let exit = recovery_failed_exit(
        &mut runtime_state,
        recovery,
        "residual maker orders".to_string(),
    );
    assert!(matches!(exit, MakerExit::OrderResponse(reason) if reason == "residual maker orders"));
}

#[test]
fn plan_affecting_account_events_invalidate_cycle_work() {
    assert!(account_event_invalidates_cycle(&AccountEvent::Position(
        position_update("BTC-USD", Some(OrderSide::Buy), "0.5")
    )));
    assert!(account_event_invalidates_cycle(&AccountEvent::Error {
        reason: "bad payload".to_string(),
    }));
    assert!(!account_event_invalidates_cycle(&AccountEvent::Order(
        OrderUpdate {
            seq: 1,
            order_id: 7,
            cl_ord_id: Some("sxmk-test-q00000001b0".to_string()),
            symbol: "BTC-USD".to_string(),
            side: OrderSide::Buy,
            qty: "1".to_string(),
            fill_qty: "0".to_string(),
            fill_avg_price: "0".to_string(),
            price: "100".to_string(),
            status: standx_sdk::models::OrderStatus::Open,
            reduce_only: false,
            updated_at: String::new(),
        }
    )));
    assert!(!account_event_invalidates_cycle(&AccountEvent::Connected {
        epoch: 1,
    }));
}

#[test]
fn account_cycle_invalidation_routes_through_cleanup_without_a_position_gap() {
    let reconciliation = reconciliation_error_for_cycle(0.2, None, None, true)
        .expect("an invalidated cycle must enter reconciliation cleanup");
    assert_eq!(reconciliation.expected, 0.2);
    assert_eq!(reconciliation.observed, 0.2);
    assert_eq!(reconciliation.cause.label(), "cycle_invalidation");

    let mut runtime_state = MakerState::starting();
    runtime_state.handle(MakerEvent::StartupReady);
    let cycle_token = match runtime_state.next_effect() {
        Some(MakerEffect::RunCycle(token)) => token,
        effect => panic!("expected cycle effect, got {effect:?}"),
    };
    runtime_state.handle(MakerEvent::CycleInvalidated {
        reason: "account state changed during maker cycle".to_string(),
    });
    // `PositionMismatch` is deliberately a no-op while frozen. The
    // pending abort is consumed by the cleanup executor instead of being
    // misread as an unexpected effect before the next cycle.
    runtime_state.handle(MakerEvent::PositionMismatch);
    let cleanup = take_cleanup_effect(&mut runtime_state, RecoveryTarget::PositionReconciliation)
        .expect("invalidated cycle must drain AbortInFlight before cleanup");
    runtime_state.handle(MakerEvent::CycleCompleted(cycle_token));
    runtime_state.handle(MakerEvent::CleanupCompleted(cleanup));
    let recovery = take_recovery_effect(&mut runtime_state, RecoveryTarget::PositionReconciliation)
        .expect("cleanup must lead to recovery");
    runtime_state.handle(MakerEvent::RecoverySucceeded(recovery));
    assert!(matches!(
        runtime_state.next_effect(),
        Some(MakerEffect::RunCycle(_))
    ));
}

#[test]
fn correlated_private_stream_failures_never_expose_the_intermediate_cycle() {
    let mut runtime_state = MakerState::starting();
    runtime_state.handle(MakerEvent::StartupReady);
    let first_cycle = match runtime_state.next_effect() {
        Some(MakerEffect::RunCycle(token)) => token,
        effect => panic!("expected initial cycle, got {effect:?}"),
    };

    runtime_state.handle(MakerEvent::AccountStreamDisconnected(
        "connection reset".to_string(),
    ));
    let account_cleanup =
        take_cleanup_effect(&mut runtime_state, RecoveryTarget::AccountStream).unwrap();
    runtime_state.handle(MakerEvent::CycleCompleted(first_cycle));
    runtime_state.handle(MakerEvent::CleanupCompleted(account_cleanup));
    let account_recovery =
        take_recovery_effect(&mut runtime_state, RecoveryTarget::AccountStream).unwrap();
    runtime_state.handle(MakerEvent::RecoverySucceeded(account_recovery));

    // The first recovery schedules a cycle, but the second private stream is
    // checked before execution. Its freeze clears that queued cycle and starts
    // another cleanup, so no placement can occur between correlated failures.
    runtime_state.handle(MakerEvent::OrderResponseDisconnected(
        "connection reset".to_string(),
    ));
    let order_cleanup =
        take_cleanup_effect(&mut runtime_state, RecoveryTarget::OrderResponse).unwrap();
    runtime_state.handle(MakerEvent::CleanupCompleted(order_cleanup));
    let order_recovery =
        take_recovery_effect(&mut runtime_state, RecoveryTarget::OrderResponse).unwrap();
    runtime_state.handle(MakerEvent::RecoverySucceeded(order_recovery));

    assert!(matches!(
        runtime_state.next_effect(),
        Some(MakerEffect::RunCycle(_))
    ));
    assert!(runtime_state.next_effect().is_none());
}

#[test]
fn touch_or_divergence_can_request_an_early_replan_without_mark_drift() {
    let quote = RestingQuote {
        order_id: Some("7".to_string()),
        side: OrderSide::Buy,
        level: 0,
        price: 99.95,
        qty: 0.1,
        ref_center: 100.0,
        placed_at_cycle: 1,
    };
    assert!(market_update_requires_replan(
        100.0,
        100.0,
        Some(99.90),
        Some(99.95),
        std::slice::from_ref(&quote),
        3.0,
        25.0,
    ));
    assert!(market_update_requires_replan(
        100.0,
        100.0,
        Some(90.0),
        Some(90.1),
        &[quote],
        3.0,
        25.0,
    ));
    assert!(!market_update_requires_replan(
        100.0,
        100.0,
        Some(99.90),
        Some(99.96),
        &[],
        3.0,
        25.0,
    ));
}

#[tokio::test]
async fn buffered_fill_stop_skips_position_notification_before_freeze() {
    use standx_maker::{PositionAlertAnchor, PositionRiskKind};

    async fn ingest(
        stats: &maker::MakerStats,
        mark: f64,
        stop_loss: f64,
        anchor: &mut PositionAlertAnchor,
        total_fills: &mut u64,
    ) -> Result<Option<f64>, maker::SessionStopLoss> {
        let notifier = MakerNotifier::new(
            OutputFormat::Quiet,
            None,
            crate::cli::AlertWebhookFormat::Raw,
        );
        let mut balance_refresh_requested = false;
        let mut inventory_exit_pending = true;
        absorb_account_outcome_or_stop(
            AccountEventOutcome {
                fills: 1,
                position_observations: vec![0.2],
                exit_fill_observed: true,
                ..AccountEventOutcome::default()
            },
            stats,
            0.2,
            mark,
            stop_loss,
            OutcomeSink {
                total_fills,
                balance_refresh_requested: &mut balance_refresh_requested,
                inventory_exit_pending: &mut inventory_exit_pending,
                notifier: &notifier,
                position_alert_anchor: anchor,
                expected_position: 0.2,
                max_position: 1.0,
                inventory_exit_pct: 0.0,
                qty_tolerance: 0.00005,
                symbol: "BTC-USD",
                cycle: 4,
                order_latency: None,
                latency_started: None,
            },
        )
        .await
    }

    // Buy 0.2 at baseline 110, marked at 100: gross PnL = -2.
    let stats = maker::MakerStats::with_inventory_baseline(0.2, 110.0);
    let mut anchor = PositionAlertAnchor::new(-0.2, 0.0, 0.05);
    let mut total_fills = 0;
    let loss = ingest(&stats, 100.0, 1.0, &mut anchor, &mut total_fills)
        .await
        .expect_err("breached gross loss must stop before notifications");
    assert!((loss.pnl + 2.0).abs() < 1e-9);
    assert_eq!(loss.mark, 100.0);
    assert_eq!(
        total_fills, 1,
        "triggering fill is credited without notifying"
    );
    // Direction flip -0.2 → +0.2 is still available, so position_jump did not run.
    assert_eq!(
        anchor.evaluate(0.2, 1.0, 0.0, 0.00005).unwrap().kind,
        PositionRiskKind::DirectionFlip
    );

    let mut anchor = PositionAlertAnchor::new(-0.2, 0.0, 0.05);
    let mut total_fills = 0;
    let continued = ingest(&stats, 110.0, 1.0, &mut anchor, &mut total_fills)
        .await
        .expect("flat pnl must still notify");
    assert_eq!(continued, Some(0.2));
    assert_eq!(total_fills, 1);
    assert!(
        anchor.evaluate(0.2, 1.0, 0.0, 0.00005).is_none(),
        "position_jump must consume the observation when stop is not breached"
    );

    let mut anchor = PositionAlertAnchor::new(-0.2, 0.0, 0.05);
    let mut total_fills = 0;
    ingest(&stats, 100.0, 0.0, &mut anchor, &mut total_fills)
        .await
        .expect("stop_loss 0 stays disabled");
    assert!(anchor.evaluate(0.2, 1.0, 0.0, 0.00005).is_none());
}

#[test]
fn stop_loss_invalidates_inflight_generation_before_shutdown() {
    let mut state = MakerState::starting();
    state.handle(MakerEvent::StartupReady);
    let token = take_cycle_work(&mut state).unwrap().unwrap();
    let exit = super::super::cycle_flow::stop_loss_exit(
        &mut state,
        OutputFormat::Quiet,
        "BTC-USD",
        1,
        maker::SessionStopLoss {
            pnl: -10.0,
            limit: 10.0,
            mark: 100.0,
        },
        None,
    );
    assert!(matches!(exit, MakerExit::StopLoss(_)));
    assert!(!commit_cycle_effect(&mut state, token));
    assert!(take_cycle_work(&mut state).unwrap().is_none());
}

#[test]
fn stop_loss_books_buffered_trade_ownership_without_notification_waits() {
    use super::super::cycle_flow::{stop_loss_exit, StopAccountBuffer};
    let mut runtime = MakerState::starting();
    runtime.handle(MakerEvent::StartupReady);
    let token = take_cycle_work(&mut runtime).unwrap().unwrap();
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
    let trade = standx_sdk::account_stream::TradeUpdate {
        seq: 1,
        trade_id: 11,
        order_id: 7,
        symbol: "BTC-USD".into(),
        side: OrderSide::Buy,
        price: "101".into(),
        qty: "0.02".into(),
        trade_ts: "2026-07-14T00:00:00Z".into(),
    };
    let outcome =
        apply_account_event(AccountEvent::Trade(trade.clone()), &mut state, &context).unwrap();
    assert_eq!(outcome.fills, 0, "trade waits for current-run ownership");
    let order = OrderUpdate {
        seq: 2,
        order_id: 7,
        cl_ord_id: Some("sxmk-test-q00000001b0".into()),
        symbol: "BTC-USD".into(),
        side: OrderSide::Buy,
        qty: "0.02".into(),
        fill_qty: "0.02".into(),
        fill_avg_price: "101".into(),
        price: "101".into(),
        status: standx_sdk::models::OrderStatus::Filled,
        reduce_only: false,
        updated_at: "2026-07-14T00:00:00Z".into(),
    };
    assert!(!account_event_invalidates_cycle(&AccountEvent::Order(
        order.clone()
    )));
    let mut total_fills = 0;
    let exit = stop_loss_exit(
        &mut runtime,
        OutputFormat::Quiet,
        "BTC-USD",
        1,
        maker::SessionStopLoss {
            pnl: -1.0,
            limit: 1.0,
            mark: 100.0,
        },
        Some(StopAccountBuffer {
            events: vec![
                AccountEvent::Order(order.clone()),
                AccountEvent::Order(order),
            ],
            state,
            context,
            total_fills: &mut total_fills,
        }),
    );
    assert!(matches!(exit, MakerExit::StopLoss(_)));
    assert_eq!(total_fills, 1);
    assert_eq!(stats.fills(), 1);
    assert_eq!(ledger.expected_position, 0.02);
    assert!((stats.pnl(ledger.expected_position, 100.0) + 0.02).abs() < 1e-9);
    assert!(!commit_cycle_effect(&mut runtime, token));
    // The stable-ID trade remains deduplicated if a later cleanup audit sees it.
    let duplicate = apply_account_event(
        AccountEvent::Trade(trade),
        &mut AccountEventState {
            ledger: &mut ledger,
            stats: &mut stats,
            projection: &mut projection,
        },
        &AccountEventContext {
            symbol: "BTC-USD",
            run_order_prefix: "sxmk-test-",
            mark: 100.0,
            cycle: 1,
            output_format: OutputFormat::Quiet,
            excess_bps_at_fill: None,
        },
    )
    .unwrap();
    assert_eq!(duplicate.fills, 0);
}
