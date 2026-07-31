use super::*;
use standx_maker::{RequestLifecycle, RequestOperation, ResponseCorrelation};

#[test]
fn apply_order_response_keeps_accepted_placement() {
    let mut projection = projection_with_pending(&["req-1"]);
    let matched = apply_order_response(
        order_response(Some("req-1"), 0),
        &mut projection,
        OutputFormat::Quiet,
        "BTC-USD",
        1,
        2,
    )
    .unwrap();
    assert!(!matched.fails_closed());
    assert_eq!(matched.label(), "matched");
    assert_eq!(
        matched.operation().map(RequestOperation::label),
        Some("place")
    );
    assert_eq!(
        projection.pending_places().len(),
        1,
        "accepted placement stays pending"
    );
    assert_eq!(projection.pending_request_count(), 0);
}

#[test]
fn correlation_verdicts_decide_which_acknowledgements_fail_closed() {
    let projection = projection_with_pending(&["req-1"]);

    // A matched ack is never a correlation failure, even while the runtime is
    // frozen for another reason.
    let matched = projection.classify_response(Some("req-1"), true);
    assert_eq!(matched.label(), "matched");
    assert!(!matched.fails_closed());

    // An ID this run never registered fails closed — and is now reported as an
    // orphan rather than as a bare "unexpected request_id".
    let orphan = projection.classify_response(Some("req-unknown"), true);
    assert_eq!(orphan, ResponseCorrelation::Orphan);
    assert!(orphan.fails_closed());

    // A frame with no request_id at all is a protocol violation. The old
    // boolean predicate ignored this case outright, which is exactly the
    // blanket-ignore path #277 rules out.
    let unidentified = projection.classify_response(None, true);
    assert_eq!(unidentified, ResponseCorrelation::Unidentified);
    assert!(unidentified.fails_closed());
}

#[test]
fn account_invalidation_with_matched_buffered_ack_reconciles_without_order_response_stop() {
    // Reproduces the shutdown that a plan-affecting account event (e.g. a
    // fill) used to trigger when the cycle had already buffered one of its
    // own order acks: the freeze targets position reconciliation, but the
    // buffered ack was wrongly read as an order-response correlation
    // failure, flipping a healthy stream unhealthy and colliding with the
    // queued cleanup target.
    let mut projection = projection_with_pending(&["req-1"]);

    let mut runtime_state = MakerState::starting();
    runtime_state.handle(MakerEvent::StartupReady);
    let cycle_token = match runtime_state.next_effect() {
        Some(MakerEffect::RunCycle(token)) => token,
        effect => panic!("expected cycle effect, got {effect:?}"),
    };

    // An invalidating account event freezes the in-flight cycle and queues
    // AbortInFlight + Cleanup { PositionReconciliation }.
    runtime_state.handle(MakerEvent::CycleInvalidated {
        reason: "account state changed during maker cycle".to_string(),
    });

    // The cycle's own placement ack was buffered before the freeze and is
    // now drained. It correlates with the pending request, so it matches.
    let health = OrderResponseHealth::default();
    let response = order_response(Some("req-1"), 0);
    let request_id = response.request_id.clone();
    let matched = apply_order_response(
        response,
        &mut projection,
        OutputFormat::Quiet,
        "BTC-USD",
        1,
        2,
    )
    .unwrap();
    assert!(
        !matched.fails_closed(),
        "buffered ack correlates with the pending request"
    );
    if matched.fails_closed() {
        health.mark_unhealthy(correlation_failure_detail(
            &matched,
            request_id.as_deref(),
            projection.generation(),
        ));
    }

    // A matched ack must leave the order-response stream healthy; otherwise
    // the top-of-loop health check would demand an OrderResponse cleanup.
    assert!(
        health.is_healthy(),
        "a matched ack must not flip the order-response stream unhealthy"
    );

    // The queued cleanup targets position reconciliation, so the maker
    // cleans up and can recover instead of stopping.
    take_cleanup_effect(&mut runtime_state, RecoveryTarget::PositionReconciliation)
        .expect("invalidation must drive a position-reconciliation cleanup, not a stop");

    // Stale completion of the aborted cycle is ignored; the maker stays
    // frozen awaiting recovery rather than resuming on stale work.
    runtime_state.handle(MakerEvent::CycleCompleted(cycle_token));
    assert!(runtime_state.pending_effect().is_none());
}

#[test]
fn order_response_cleanup_drain_rejects_position_reconciliation_target() {
    // Regression witness for the collision the fix removes: had a buffered
    // response been treated as an order-response fault while the runtime
    // was frozen for position reconciliation, the top-of-loop
    // order-response recovery would drain the queued cleanup with the wrong
    // target and fail closed into a stop.
    let mut runtime_state = MakerState::starting();
    runtime_state.handle(MakerEvent::StartupReady);
    let _ = runtime_state.next_effect();
    runtime_state.handle(MakerEvent::CycleInvalidated {
        reason: "account state changed during maker cycle".to_string(),
    });
    let error = take_cleanup_effect(&mut runtime_state, RecoveryTarget::OrderResponse)
        .expect_err("position-reconciliation cleanup must not satisfy an order-response drain");
    assert!(error.to_string().contains("expected OrderResponse cleanup"));
}

#[test]
fn apply_order_response_drops_rejected_placement() {
    let mut projection = projection_with_pending(&["req-1"]);
    let matched = apply_order_response(
        order_response(Some("req-1"), 1),
        &mut projection,
        OutputFormat::Quiet,
        "BTC-USD",
        1,
        2,
    )
    .unwrap();
    assert!(!matched.fails_closed());
    assert!(
        projection.pending_places().is_empty(),
        "rejected placement is removed"
    );
}

#[test]
fn apply_order_response_matches_cancel_acknowledgement() {
    let mut projection = MakerAccountProjection::new(1, "sxmk-test-", 0.0, 0.005, 0.00005);
    projection.apply(
        1,
        AccountProjectionEvent::CancelSubmitted(ProjectionPendingCancel {
            request_id: "cancel-1".to_string(),
            order_id: 7,
            side: OrderSide::Buy,
            level: 0,
            price: 100.0,
            cycle: 1,
        }),
    );

    let verdict = correlate(&mut projection, Some("cancel-1"), 0);
    assert_eq!(verdict.label(), "matched");
    assert_eq!(
        verdict.operation().map(RequestOperation::label),
        Some("cancel")
    );
    assert!(projection.pending_cancels().is_empty());
}

#[test]
fn duplicate_place_ack_matches_completed_request_after_cleanup() {
    let mut projection = projection_with_pending(&["req-1"]);
    assert_eq!(
        correlate(&mut projection, Some("req-1"), 0).label(),
        "matched"
    );
    projection.clear_orders_and_pending();

    // The tombstone still identifies the request, and the duplicate agrees with
    // the recorded resolution, so it is a late-but-consistent delivery — not the
    // "unexpected request_id" the old boolean reported it as.
    let duplicate = correlate(&mut projection, Some("req-1"), 0);
    assert_eq!(duplicate.label(), "late_known");
    assert!(!duplicate.fails_closed());
}

#[test]
fn delayed_account_order_and_replayed_ack_survive_account_reconnect() {
    let mut projection = MakerAccountProjection::new(1, "sxmk-test-", 0.0, 0.005, 0.00005);
    projection.apply(
        1,
        AccountProjectionEvent::PlaceSubmitted(ProjectionPendingPlace {
            request_id: "req-1".to_string(),
            client_order_id: "sxmk-test-q00000001b0".to_string(),
            side: OrderSide::Buy,
            price: 100.0,
            qty: 1.0,
            level: 0,
            ref_center: 100.0,
            cycle: 1,
        }),
    );
    assert!(!correlate(&mut projection, Some("req-1"), 0).fails_closed());
    projection.apply(1, AccountProjectionEvent::AdvanceCycle { cycle: 4 });
    projection.reset_after_cleanup_preserving_pending_acks(2, 0.0);

    let outcome = projection.apply(
        2,
        AccountProjectionEvent::OrderObserved(OrderObservation {
            order_id: 7,
            client_order_id: Some("sxmk-test-q00000001b0".to_string()),
            side: OrderSide::Buy,
            price: 100.0,
            open_qty: 1.0,
            terminal: false,
        }),
    );
    assert!(!outcome.unknown_current_run_order);
    assert!(!correlate(&mut projection, Some("req-1"), 0).fails_closed());
}

#[test]
fn duplicate_place_rejection_matches_completed_request_after_cleanup() {
    let mut projection = projection_with_pending(&["req-1"]);
    assert!(!correlate(&mut projection, Some("req-1"), 400).fails_closed());
    projection.clear_orders_and_pending();

    assert!(!correlate(&mut projection, Some("req-1"), 400).fails_closed());
}

#[test]
fn duplicate_cancel_ack_matches_completed_request_after_cleanup() {
    let mut projection = MakerAccountProjection::new(1, "sxmk-test-", 0.0, 0.005, 0.00005);
    projection.apply(
        1,
        AccountProjectionEvent::CancelSubmitted(ProjectionPendingCancel {
            request_id: "cancel-1".to_string(),
            order_id: 7,
            side: OrderSide::Buy,
            level: 0,
            price: 100.0,
            cycle: 1,
        }),
    );
    assert!(!correlate(&mut projection, Some("cancel-1"), 0).fails_closed());
    projection.clear_orders_and_pending();

    assert!(!correlate(&mut projection, Some("cancel-1"), 0).fails_closed());
}

#[test]
fn two_frame_place_rejection_without_venue_observation_is_async_rejection() {
    let mut projection = projection_with_pending(&["req-1"]);
    assert!(!correlate(&mut projection, Some("req-1"), 0).fails_closed());

    // The tombstone records PlaceAccepted and the account stream has NOT shown
    // the order: under the venue's two-frame protocol (gateway `accepted`, then
    // terminal `"alo order rejected"` — observed live 2026-07-31, run
    // `baseline-pnl-20260730T163544Z`) this second frame is the ordinary async
    // rejection, not a channel contradiction.
    let verdict = correlate(&mut projection, Some("req-1"), 400);
    assert!(!verdict.fails_closed());
    assert_eq!(verdict.label(), "matched");
    assert_eq!(
        verdict,
        ResponseCorrelation::Matched {
            operation: RequestOperation::Place,
            lifecycle: RequestLifecycle::AwaitingVenue,
        }
    );
    // The rejection is applied as the ordinary async rejection: the quote
    // slot is freed and the request lifecycle is retired.
    assert!(
        projection.pending_places().is_empty(),
        "the async rejection must free the level"
    );
    assert_eq!(projection.pending_request_count(), 0);
}

#[test]
fn two_frame_place_rejection_after_venue_observation_remains_fail_closed() {
    let mut projection = projection_with_pending(&["req-1"]);
    assert!(!correlate(&mut projection, Some("req-1"), 0).fails_closed());
    // The account stream shows the placed order live; the projection adopts it
    // and retires the pending slot.
    projection.apply(
        1,
        AccountProjectionEvent::OrderObserved(standx_maker::OrderObservation {
            order_id: 42,
            client_order_id: Some("sxmk-test-q00000001b0".to_string()),
            side: OrderSide::Buy,
            price: 100.0,
            open_qty: 1.0,
            terminal: false,
        }),
    );

    // The terminal rejection now genuinely contradicts the venue-visible order.
    let verdict = correlate(&mut projection, Some("req-1"), 400);
    assert!(verdict.fails_closed());
    assert_eq!(verdict.label(), "venue_contradiction");
    // The failure detail must let an operator reconstruct all of it.
    let detail = correlation_failure_detail(&verdict, Some("req-1"), projection.generation());
    for expected in [
        "verdict=venue_contradiction",
        "request_id=req-1",
        "operation=place",
    ] {
        assert!(
            detail.contains(expected),
            "{expected} missing from: {detail}"
        );
    }
}

#[test]
fn apply_order_response_fails_closed_on_rejected_cancel_acknowledgement() {
    let mut projection = MakerAccountProjection::new(1, "sxmk-test-", 0.0, 0.005, 0.00005);
    projection.apply(
        1,
        AccountProjectionEvent::CancelSubmitted(ProjectionPendingCancel {
            request_id: "cancel-1".to_string(),
            order_id: 7,
            side: OrderSide::Buy,
            level: 0,
            price: 100.0,
            cycle: 1,
        }),
    );

    assert_eq!(
        apply_order_response(
            order_response(Some("cancel-1"), 400),
            &mut projection,
            OutputFormat::Quiet,
            "BTC-USD",
            1,
            2,
        ),
        Err(CancelRejection {
            request_id: "cancel-1".to_string(),
            code: 400,
            message: String::new(),
        })
    );
    assert_eq!(projection.pending_cancels().len(), 1);
    assert_eq!(projection.pending_request_count(), 1);
}

#[test]
fn apply_order_response_matches_late_ack_after_terminal_account_order() {
    let mut projection = MakerAccountProjection::new(1, "sxmk-test-", 0.0, 0.005, 0.00005);
    projection.apply(
        1,
        AccountProjectionEvent::PlaceSubmitted(ProjectionPendingPlace {
            request_id: "req-1".to_string(),
            client_order_id: "sxmk-test-q00000001b0".to_string(),
            side: OrderSide::Buy,
            price: 100.0,
            qty: 1.0,
            level: 0,
            ref_center: 100.0,
            cycle: 1,
        }),
    );
    projection.apply(
        1,
        AccountProjectionEvent::OrderObserved(OrderObservation {
            order_id: 7,
            client_order_id: Some("sxmk-test-q00000001b0".to_string()),
            side: OrderSide::Buy,
            price: 100.0,
            open_qty: 0.0,
            terminal: true,
        }),
    );
    assert!(projection.pending_places().is_empty());
    assert_eq!(projection.pending_request_count(), 1);

    assert!(!correlate(&mut projection, Some("req-1"), 0).fails_closed());
    assert_eq!(projection.pending_request_count(), 0);
}

#[test]
fn apply_order_response_reports_unmatched_ids() {
    let mut projection = projection_with_pending(&["req-1"]);

    // An ID this run never registered.
    let orphan = correlate(&mut projection, Some("other"), 0);
    assert_eq!(orphan, ResponseCorrelation::Orphan);
    assert!(orphan.fails_closed());

    // A frame carrying no request_id at all: a protocol violation, reported
    // separately rather than folded into "unknown ID".
    let unidentified = correlate(&mut projection, None, 0);
    assert_eq!(unidentified, ResponseCorrelation::Unidentified);
    assert!(unidentified.fails_closed());
    assert!(
        correlation_failure_detail(&unidentified, None, projection.generation())
            .contains("carried no request_id")
    );

    // Neither observation may touch the registry on its way out.
    assert_eq!(projection.pending_places().len(), 1);
    assert_eq!(projection.pending_request_count(), 1);
}

#[test]
fn apply_order_responses_matched_acks_clear_request_registry() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let mut projection = projection_with_pending(&["req-1", "req-2"]);
    let mut runtime_state = MakerState::starting();
    runtime_state.handle(MakerEvent::StartupReady);
    assert!(matches!(
        runtime_state.next_effect(),
        Some(MakerEffect::RunCycle(_))
    ));

    // Benign matched acknowledgements for placements we are tracking.
    tx.try_send(order_response(Some("req-1"), 0)).unwrap();
    tx.try_send(order_response(Some("req-2"), 0)).unwrap();

    apply_order_responses(
        &mut rx,
        &mut projection,
        &mut runtime_state,
        OutputFormat::Quiet,
        "BTC-USD",
        1,
        2,
    )
    .expect("benign matched acks must not fail closed");

    assert!(runtime_state.pending_effect().is_none());
    // Accepted placements remain pending; the matched arm keeps them.
    assert_eq!(projection.pending_places().len(), 2);
    assert_eq!(projection.pending_request_count(), 0);
}

#[test]
fn apply_order_responses_unknown_request_fails_closed() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let mut projection = projection_with_pending(&[]);
    let mut runtime_state = MakerState::starting();
    runtime_state.handle(MakerEvent::StartupReady);
    assert!(matches!(
        runtime_state.next_effect(),
        Some(MakerEffect::RunCycle(_))
    ));

    tx.try_send(order_response(Some("req-1"), 0)).unwrap();
    let error = apply_order_responses(
        &mut rx,
        &mut projection,
        &mut runtime_state,
        OutputFormat::Quiet,
        "BTC-USD",
        1,
        2,
    )
    .unwrap_err();
    assert!(error.to_string().contains("correlation failed closed"));
    assert!(error.to_string().contains("request_id=req-1"));
    assert!(matches!(
        runtime_state.pending_effect(),
        Some(MakerEffect::AbortInFlight(_))
    ));
}

#[test]
fn cleanup_minted_late_ack_is_dropped_without_failing_closed() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let mut projection = projection_with_pending(&[]);
    let mut runtime_state = MakerState::starting();
    runtime_state.handle(MakerEvent::StartupReady);
    assert!(matches!(
        runtime_state.next_effect(),
        Some(MakerEffect::RunCycle(_))
    ));
    let mut cleanup_minted = CleanupTombstones::default();
    cleanup_minted.remember("cleanup-req".to_string());

    // The late WS ack for a cleanup-minted cancel must be dropped, not judged:
    // cleanup already established the venue state through `/api/query_order`,
    // so the frame carries no new information.
    tx.try_send(order_response(Some("cleanup-req"), 0)).unwrap();

    apply_order_responses_observed(
        &mut rx,
        &mut projection,
        &mut runtime_state,
        OrderResponseObservation {
            output_format: OutputFormat::Quiet,
            symbol: "BTC-USD",
            cycle: 1,
            price_decimals: 2,
            latency: None,
            latency_started: None,
        },
        &cleanup_minted,
    )
    .expect("cleanup-minted late ack must not fail closed");

    assert!(
        cleanup_minted.covers("cleanup-req"),
        "the tombstone is retained, not consumed by the first frame"
    );
    assert!(runtime_state.pending_effect().is_none());
}

/// Regression: the venue answers one `order:cancel` with a gateway `accepted`
/// frame and then the terminal `success` frame, both carrying the same request
/// ID. Cleanup-minted IDs are never in the projection's request registry, so a
/// tombstone that was consumed by the first frame let the second classify as
/// `Orphan` and stopped the maker on a request it minted itself.
#[test]
fn cleanup_minted_two_frame_ack_never_fails_closed() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let mut projection = projection_with_pending(&[]);
    let mut runtime_state = MakerState::starting();
    runtime_state.handle(MakerEvent::StartupReady);
    assert!(matches!(
        runtime_state.next_effect(),
        Some(MakerEffect::RunCycle(_))
    ));
    let mut cleanup_minted = CleanupTombstones::default();
    cleanup_minted.remember("cleanup-req".to_string());

    // Gateway ack, then the terminal ack — same request ID, both after the
    // cleanup drain window closed.
    tx.try_send(order_response(Some("cleanup-req"), 0)).unwrap();
    tx.try_send(order_response(Some("cleanup-req"), 0)).unwrap();

    apply_order_responses_observed(
        &mut rx,
        &mut projection,
        &mut runtime_state,
        OrderResponseObservation {
            output_format: OutputFormat::Quiet,
            symbol: "BTC-USD",
            cycle: 1,
            price_decimals: 2,
            latency: None,
            latency_started: None,
        },
        &cleanup_minted,
    )
    .expect("neither frame of a two-frame cleanup ack may fail closed");

    assert!(
        runtime_state.pending_effect().is_none(),
        "no freeze/recovery effect may be queued for a cleanup-minted ack"
    );
}

#[test]
fn apply_order_responses_rejected_cancel_fails_closed() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let mut projection = MakerAccountProjection::new(1, "sxmk-test-", 0.0, 0.005, 0.00005);
    projection.apply(
        1,
        AccountProjectionEvent::CancelSubmitted(ProjectionPendingCancel {
            request_id: "cancel-1".to_string(),
            order_id: 7,
            side: OrderSide::Buy,
            level: 0,
            price: 100.0,
            cycle: 1,
        }),
    );
    let mut runtime_state = MakerState::starting();
    runtime_state.handle(MakerEvent::StartupReady);
    assert!(matches!(
        runtime_state.next_effect(),
        Some(MakerEffect::RunCycle(_))
    ));

    tx.try_send(OrderResponse {
        code: 400,
        message: "cancel rejected".to_string(),
        request_id: Some("cancel-1".to_string()),
    })
    .unwrap();
    let error = apply_order_responses(
        &mut rx,
        &mut projection,
        &mut runtime_state,
        OutputFormat::Quiet,
        "BTC-USD",
        1,
        2,
    )
    .unwrap_err();

    assert!(error.to_string().contains("cancel rejected"));
    assert!(matches!(
        runtime_state.pending_effect(),
        Some(MakerEffect::AbortInFlight(_))
    ));
    assert_eq!(projection.pending_cancels().len(), 1);
}

#[test]
fn async_rejection_removes_only_matching_pending_place() {
    let (sender, mut receiver) = tokio::sync::mpsc::channel(4);
    let pending_place = |request_id: &str| ProjectionPendingPlace {
        request_id: request_id.to_string(),
        client_order_id: format!("client-{request_id}"),
        side: OrderSide::Buy,
        price: 100.0,
        qty: 0.01,
        level: 0,
        ref_center: 100.0,
        cycle: 1,
    };
    let mut projection = MakerAccountProjection::new(1, "sxmk-test-", 0.0, 0.005, 0.00005);
    for pending in [pending_place("request-1"), pending_place("request-2")] {
        projection.apply(1, AccountProjectionEvent::PlaceSubmitted(pending));
    }
    let mut runtime_state = MakerState::starting();
    sender
        .try_send(OrderResponse {
            code: 400,
            message: "alo order rejected".to_string(),
            request_id: Some("request-1".to_string()),
        })
        .unwrap();

    apply_order_responses(
        &mut receiver,
        &mut projection,
        &mut runtime_state,
        OutputFormat::Quiet,
        "BTC-USD",
        2,
        2,
    )
    .unwrap();

    assert_eq!(projection.pending_places().len(), 1);
    assert_eq!(projection.pending_places()[0].request_id, "request-2");
}

#[test]
fn async_acceptance_keeps_pending_until_exchange_order_is_visible() {
    let (sender, mut receiver) = tokio::sync::mpsc::channel(2);
    let pending = ProjectionPendingPlace {
        request_id: "request-1".to_string(),
        client_order_id: "client-1".to_string(),
        side: OrderSide::Sell,
        price: 101.0,
        qty: 0.01,
        level: 0,
        ref_center: 100.0,
        cycle: 1,
    };
    let mut projection = MakerAccountProjection::new(1, "sxmk-test-", 0.0, 0.005, 0.00005);
    projection.apply(1, AccountProjectionEvent::PlaceSubmitted(pending));
    let mut runtime_state = MakerState::starting();
    sender
        .try_send(OrderResponse {
            code: 0,
            message: "accepted".to_string(),
            request_id: Some("request-1".to_string()),
        })
        .unwrap();

    apply_order_responses(
        &mut receiver,
        &mut projection,
        &mut runtime_state,
        OutputFormat::Quiet,
        "BTC-USD",
        2,
        2,
    )
    .unwrap();

    for cycle in 3..=100 {
        projection.apply(1, AccountProjectionEvent::AdvanceCycle { cycle });
    }
    assert_eq!(projection.pending_places().len(), 1);
}

#[test]
fn disconnected_order_response_stream_is_fail_closed() {
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    drop(sender);
    let mut projection = MakerAccountProjection::new(1, "sxmk-test-", 0.0, 0.005, 0.00005);
    let mut runtime_state = MakerState::starting();

    let error = apply_order_responses(
        &mut receiver,
        &mut projection,
        &mut runtime_state,
        OutputFormat::Quiet,
        "BTC-USD",
        1,
        2,
    )
    .unwrap_err();

    assert!(error.to_string().contains("disconnected"));
}

/// Adversarial-review regression, driven through the runtime failure path rather
/// than only the classifier: the account stream adopts the placed order, then the
/// command channel rejects that same request. The maker must freeze and mark the
/// order-response stream unhealthy, and — critically — must *not* apply the
/// rejection, because that would drop a venue-visible order out of the
/// projection while the venue still has it.
#[test]
fn rejection_after_venue_adoption_freezes_instead_of_dropping_the_live_order() {
    // The client order ID has to carry the run prefix, or the projection
    // discards the observation as not-ours before any adoption can happen.
    let client_order_id = "sxmk-test-q00000001b0".to_string();
    let mut projection = MakerAccountProjection::new(1, "sxmk-test-", 0.0, 0.005, 0.00005);
    projection.apply(
        1,
        AccountProjectionEvent::PlaceSubmitted(ProjectionPendingPlace {
            request_id: "req-1".to_string(),
            client_order_id: client_order_id.clone(),
            side: OrderSide::Buy,
            price: 100.0,
            qty: 1.0,
            level: 0,
            ref_center: 100.0,
            cycle: 1,
        }),
    );

    // The account stream lands first and adopts the live order into the book.
    let adopted = projection.apply(
        1,
        AccountProjectionEvent::OrderObserved(OrderObservation {
            order_id: 4_242,
            client_order_id: Some(client_order_id),
            side: OrderSide::Buy,
            price: 100.0,
            open_qty: 1.0,
            terminal: false,
        }),
    );
    assert_eq!(adopted.effective_request_id.as_deref(), Some("req-1"));
    assert_eq!(projection.resting_quotes().len(), 1);

    let mut runtime_state = MakerState::starting();
    runtime_state.handle(MakerEvent::StartupReady);
    let cycle_token = match runtime_state.next_effect() {
        Some(MakerEffect::RunCycle(token)) => token,
        effect => panic!("expected cycle effect, got {effect:?}"),
    };

    // Now the rejection for that same request arrives on the command channel.
    let generation = projection.generation();
    let verdict = correlate(&mut projection, Some("req-1"), 400);
    assert_eq!(verdict.label(), "venue_contradiction");
    assert!(verdict.fails_closed());

    let health = OrderResponseHealth::default();
    let reason =
        order_response_failure(&Ok(verdict), Some("req-1"), generation, &mut runtime_state)
            .expect("a venue contradiction must produce a fail-closed reason");
    health.mark_unhealthy(reason.clone());
    assert!(!health.is_healthy());
    for expected in [
        "verdict=venue_contradiction",
        "request_id=req-1",
        "operation=place",
        "lifecycle=awaiting_ack",
        "rejected the placement",
    ] {
        assert!(
            reason.contains(expected),
            "{expected} missing from: {reason}"
        );
    }

    // The disputed order stays in the book: fail-closed recovery reconciles it,
    // it is never silently forgotten.
    assert_eq!(
        projection.resting_quotes().len(),
        1,
        "the venue-visible order must survive the contradiction"
    );
    assert_eq!(
        projection.pending_request_count(),
        1,
        "the rejection must not be applied"
    );

    // The runtime froze for order-response recovery: the queued cleanup targets
    // the placement channel, and the aborted cycle's late completion is ignored.
    take_cleanup_effect(&mut runtime_state, RecoveryTarget::OrderResponse)
        .expect("a venue contradiction must drive an order-response cleanup");
    runtime_state.handle(MakerEvent::CycleCompleted(cycle_token));
    assert!(runtime_state.pending_effect().is_none());
}
