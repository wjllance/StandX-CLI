use super::*;

const PREFIX: &str = "sxmk-run-";

fn pending(request_id: &str) -> ProjectionPendingPlace {
    ProjectionPendingPlace {
        request_id: request_id.to_owned(),
        client_order_id: format!("{PREFIX}q00000001b0"),
        side: OrderSide::Buy,
        price: 100.0,
        qty: 0.2,
        level: 0,
        ref_center: 100.0,
        cycle: 1,
    }
}

/// Every step a request's identity can be observed through, so a
/// permutation test reads as an ordered list of observations.
#[derive(Debug, Clone, Copy)]
enum Step {
    /// Register the place before any network write.
    Submit,
    /// Command-stream acknowledgement (accepted / rejected).
    Ack { accepted: bool },
    /// Account-stream order update closing the quote slot.
    Venue { terminal: bool },
    /// Recovery bumped the generation, preserving pending acks.
    ReconnectPreservingAcks,
}

fn drive(steps: &[Step]) -> MakerAccountProjection {
    let mut projection = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
    let mut generation = 1;
    for step in steps {
        match *step {
            Step::Submit => {
                projection.apply(
                    generation,
                    AccountProjectionEvent::PlaceSubmitted(pending("req-1")),
                );
            }
            Step::Ack { accepted } => {
                let event = if accepted {
                    AccountProjectionEvent::PlaceAccepted {
                        request_id: "req-1".to_owned(),
                    }
                } else {
                    AccountProjectionEvent::PlaceRejected {
                        request_id: "req-1".to_owned(),
                    }
                };
                projection.apply(generation, event);
            }
            Step::Venue { terminal } => {
                projection.apply(
                    generation,
                    AccountProjectionEvent::OrderObserved(order(
                        if terminal { 0.0 } else { 0.2 },
                        terminal,
                    )),
                );
            }
            Step::ReconnectPreservingAcks => {
                generation += 1;
                projection.reset_after_cleanup_preserving_pending_acks(generation, 0.0);
            }
        }
    }
    projection
}

/// The classification table, pinned per ordering. These are the permutations
/// #277 requires: WS-then-account, account-then-WS, duplicates, delayed
/// deliveries, partial-fill-then-cancel, reconnect replay, and wrong IDs.
#[test]
fn correlation_verdict_is_pinned_for_every_observation_ordering() {
    let accepted = Step::Ack { accepted: true };
    let rejected = Step::Ack { accepted: false };
    let resting = Step::Venue { terminal: false };
    let gone = Step::Venue { terminal: true };

    // (steps already observed, the ack now arriving, expected verdict label)
    let cases: Vec<(&str, Vec<Step>, bool, &str)> = vec![
        (
            "registered, awaiting its first ack",
            vec![Step::Submit],
            true,
            "matched",
        ),
        (
            "WS ack then account order: duplicate ack after the slot closed",
            vec![Step::Submit, accepted, resting],
            true,
            "late_known",
        ),
        (
            "account order then WS ack: adoption closed the slot first",
            vec![Step::Submit, resting],
            true,
            "matched",
        ),
        (
            "account order then WS *rejection*: the two channels disagree",
            vec![Step::Submit, resting],
            false,
            "venue_contradiction",
        ),
        (
            "cleanup retired the exposure, so a later rejection contradicts nothing live",
            vec![Step::Submit, resting, Step::ReconnectPreservingAcks],
            false,
            "matched",
        ),
        (
            "duplicate ack while the venue has not confirmed yet",
            vec![Step::Submit, accepted],
            true,
            "late_known",
        ),
        (
            "rejection contradicting an accepted place, slot still open",
            vec![Step::Submit, accepted],
            false,
            "contradictory",
        ),
        (
            "acceptance contradicting a rejected place",
            vec![Step::Submit, rejected],
            true,
            "contradictory",
        ),
        (
            "partial fill then terminal: ack replayed after the order is gone",
            vec![Step::Submit, accepted, resting, gone],
            true,
            "late_known",
        ),
        (
            "reconnect replay: ack resolved in the previous generation",
            vec![Step::Submit, accepted, Step::ReconnectPreservingAcks],
            true,
            "late_known",
        ),
        (
            "ack arriving before anything was registered",
            vec![],
            true,
            "orphan_current_run",
        ),
    ];

    for (name, steps, accepted_response, expected) in cases {
        let projection = drive(&steps);
        let verdict = projection.classify_response(Some("req-1"), accepted_response);
        assert_eq!(verdict.label(), expected, "case: {name}");
        // The safety policy follows from the verdict, nothing else.
        assert_eq!(
            verdict.fails_closed(),
            !matches!(expected, "matched" | "late_known"),
            "case: {name}"
        );
    }
}

/// A reconnect that preserves pending acks must keep the *pending* ack
/// matched, not demote it to a tombstone verdict — that is the whole point of
/// preserving it across the generation bump.
#[test]
fn reconnect_preserving_acks_keeps_an_unacknowledged_request_matched() {
    let projection = drive(&[Step::Submit, Step::ReconnectPreservingAcks]);
    let verdict = projection.classify_response(Some("req-1"), true);
    assert_eq!(
        verdict,
        ResponseCorrelation::Matched {
            operation: RequestOperation::Place,
            lifecycle: RequestLifecycle::AwaitingAck,
        }
    );
}

/// Reproduction for the adversarial-review finding: the account stream shows
/// the order live at the venue, then the command channel rejects the very
/// same placement. Two independent channels disagree about whether the order
/// exists, and the maker has *already adopted it into its book* — so this
/// must fail closed, not be applied as an ordinary rejection.
#[test]
fn rejection_after_the_venue_showed_the_order_live_is_a_contradiction() {
    let mut projection = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
    projection.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("req-1")));
    // Account stream lands first and adopts the live order.
    let adopted = projection.apply(1, AccountProjectionEvent::OrderObserved(order(0.2, false)));
    assert_eq!(adopted.effective_request_id.as_deref(), Some("req-1"));
    assert_eq!(projection.resting_quotes().len(), 1, "order is in the book");

    // An accepted ack for the same request is the normal ordering.
    assert_eq!(
        projection.classify_response(Some("req-1"), true).label(),
        "matched"
    );

    // A rejection is not: the venue says the order is open, the control
    // plane says the placement never happened.
    let verdict = projection.classify_response(Some("req-1"), false);
    assert!(
        verdict.fails_closed(),
        "a rejection contradicting a venue-visible order must fail closed, got {verdict:?}"
    );
    assert_eq!(verdict.label(), "venue_contradiction");
}

/// A missing `request_id` is a protocol violation with its own verdict. It
/// must never share the "unknown ID" path, and it must never be ignored.
#[test]
fn a_frame_without_a_request_id_is_reported_as_a_protocol_violation() {
    let projection = drive(&[Step::Submit]);
    let verdict = projection.classify_response(None, true);
    assert_eq!(verdict, ResponseCorrelation::Unidentified);
    assert!(verdict.fails_closed());
    assert_eq!(verdict.lifecycle(), RequestLifecycle::Unknown);
    assert_eq!(verdict.operation(), None);
}

/// Tombstone eviction must not silently become "consistent". Once the
/// recorded resolution is gone the maker cannot check the acknowledgement,
/// so it fails closed instead of assuming agreement.
#[test]
fn evicting_the_resolution_tombstone_fails_closed_rather_than_assuming_agreement() {
    let mut projection = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
    projection.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("req-1")));
    projection.apply(
        1,
        AccountProjectionEvent::PlaceAccepted {
            request_id: "req-1".to_owned(),
        },
    );
    assert_eq!(
        projection.classify_response(Some("req-1"), true).label(),
        "late_known"
    );

    // Push the tombstone out of the bounded history. The quote slot stays
    // open the whole time, so the request is still registered.
    for index in 0..=MAX_COMPLETED_ORDER_REQUESTS {
        let request_id = format!("filler-{index}");
        let mut filler = pending(&request_id);
        filler.client_order_id = format!("{PREFIX}q0000000{index}b0");
        projection.apply(1, AccountProjectionEvent::PlaceSubmitted(filler));
        projection.apply(1, AccountProjectionEvent::PlaceRejected { request_id });
    }

    let verdict = projection.classify_response(Some("req-1"), true);
    assert_eq!(verdict, ResponseCorrelation::Unverifiable);
    assert!(verdict.fails_closed());
}

fn order(open_qty: f64, terminal: bool) -> OrderObservation {
    OrderObservation {
        order_id: 7,
        client_order_id: Some(format!("{PREFIX}q00000001b0")),
        side: OrderSide::Buy,
        price: 100.0,
        open_qty,
        terminal,
    }
}

#[test]
fn account_order_reports_place_effective_before_ack() {
    let mut state = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
    state.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("p1")));
    let outcome = state.apply(1, AccountProjectionEvent::OrderObserved(order(0.2, false)));
    assert_eq!(outcome.effective_request_id.as_deref(), Some("p1"));
    assert_eq!(
        state.pending_request("p1"),
        Some(&ProjectionPendingRequest::Place(pending("p1")))
    );
}

#[test]
fn terminal_account_order_reports_cancel_effective() {
    let mut state = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
    state.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("p1")));
    state.apply(1, AccountProjectionEvent::OrderObserved(order(0.2, false)));
    state.apply(
        1,
        AccountProjectionEvent::CancelSubmitted(ProjectionPendingCancel {
            request_id: "c1".to_string(),
            order_id: 7,
            side: OrderSide::Buy,
            level: 0,
            price: 100.0,
            cycle: 2,
        }),
    );
    let outcome = state.apply(1, AccountProjectionEvent::OrderObserved(order(0.0, true)));
    assert_eq!(outcome.effective_request_id.as_deref(), Some("c1"));
}

#[test]
fn terminal_account_order_reports_cancel_effective_after_ack() {
    let mut state = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
    state.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("p1")));
    state.apply(1, AccountProjectionEvent::OrderObserved(order(0.2, false)));
    state.apply(
        1,
        AccountProjectionEvent::CancelSubmitted(ProjectionPendingCancel {
            request_id: "c1".to_string(),
            order_id: 7,
            side: OrderSide::Buy,
            level: 0,
            price: 100.0,
            cycle: 2,
        }),
    );
    state.apply(
        1,
        AccountProjectionEvent::CancelResolved {
            request_id: "c1".to_string(),
        },
    );

    let outcome = state.apply(1, AccountProjectionEvent::OrderObserved(order(0.0, true)));
    assert_eq!(outcome.effective_request_id.as_deref(), Some("c1"));
}

#[test]
fn order_then_trade_and_duplicate_trade_outcome_are_idempotent() {
    let mut state = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
    state.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("p1")));
    state.apply(1, AccountProjectionEvent::OrderObserved(order(0.2, false)));
    state.apply(
        1,
        AccountProjectionEvent::TradeApplied {
            order_id: 7,
            qty: 0.1,
        },
    );
    assert_eq!(state.resting_quotes()[0].qty, 0.1);
    // The ledger suppresses duplicate trades, so no second outcome is
    // delivered. Replayed order state converges to the same open qty.
    state.apply(1, AccountProjectionEvent::OrderObserved(order(0.1, false)));
    assert_eq!(state.resting_quotes()[0].qty, 0.1);
}

#[test]
fn trade_before_order_does_not_create_phantom_order() {
    let mut state = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
    state.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("p1")));
    state.apply(
        1,
        AccountProjectionEvent::TradeApplied {
            order_id: 7,
            qty: 0.1,
        },
    );
    assert!(state.resting_quotes().is_empty());
    state.apply(1, AccountProjectionEvent::OrderObserved(order(0.1, false)));
    assert_eq!(state.resting_quotes()[0].qty, 0.1);
}

#[test]
fn partial_fill_then_cancel_is_terminal_in_either_order() {
    let mut state = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
    state.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("p1")));
    state.apply(1, AccountProjectionEvent::OrderObserved(order(0.2, false)));
    state.apply(
        1,
        AccountProjectionEvent::TradeApplied {
            order_id: 7,
            qty: 0.1,
        },
    );
    state.apply(1, AccountProjectionEvent::OrderObserved(order(0.0, true)));
    state.apply(1, AccountProjectionEvent::OrderObserved(order(0.0, true)));
    assert!(state.resting_quotes().is_empty());
}

#[test]
fn wrong_run_and_stale_generation_are_ignored() {
    let mut state = MakerAccountProjection::new(2, PREFIX, 0.0, 0.005, 0.00005);
    let mut wrong = order(0.2, false);
    wrong.client_order_id = Some("sxmk-other-q00000001b0".to_string());
    assert!(
        !state
            .apply(2, AccountProjectionEvent::OrderObserved(wrong))
            .applied
    );
    assert!(
        !state
            .apply(1, AccountProjectionEvent::PlaceSubmitted(pending("old")))
            .applied
    );
    assert!(state.pending_places().is_empty());
}

#[test]
fn cancel_ack_after_close_is_idempotent() {
    let mut state = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
    state.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("p1")));
    state.apply(1, AccountProjectionEvent::OrderObserved(order(0.2, false)));
    state.apply(
        1,
        AccountProjectionEvent::CancelSubmitted(ProjectionPendingCancel {
            request_id: "c1".to_string(),
            order_id: 7,
            side: OrderSide::Buy,
            level: 0,
            price: 100.0,
            cycle: 2,
        }),
    );
    state.apply(1, AccountProjectionEvent::OrderObserved(order(0.0, true)));
    assert!(state.pending_cancels().is_empty());
    assert!(matches!(
        state.pending_request("c1"),
        Some(ProjectionPendingRequest::Cancel(_))
    ));
    assert!(
        state
            .apply(
                1,
                AccountProjectionEvent::CancelResolved {
                    request_id: "c1".to_string()
                }
            )
            .applied
    );
    assert!(state.resting_quotes().is_empty());
}

#[test]
fn late_open_after_cancel_ack_is_recognized_as_a_retired_current_run_order() {
    let mut state = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
    state.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("p1")));
    state.apply(
        1,
        AccountProjectionEvent::PlaceAccepted {
            request_id: "p2".to_string(),
        },
    );
    state.apply(1, AccountProjectionEvent::OrderObserved(order(0.2, false)));
    state.apply(
        1,
        AccountProjectionEvent::CancelSubmitted(ProjectionPendingCancel {
            request_id: "c1".to_string(),
            order_id: 7,
            side: OrderSide::Buy,
            level: 0,
            price: 100.0,
            cycle: 2,
        }),
    );
    state.apply(
        1,
        AccountProjectionEvent::CancelResolved {
            request_id: "c1".to_string(),
        },
    );

    // The order channel can replay an open state after the cancel command
    // was accepted. It is still ours, so project it as stale for another
    // cancellation instead of treating it as an external/unknown order.
    let outcome = state.apply(1, AccountProjectionEvent::OrderObserved(order(0.2, false)));
    assert!(outcome.applied && outcome.order_changed);
    assert!(!outcome.unknown_current_run_order);
    assert_eq!(state.resting_quotes()[0].level, UNKNOWN_ADOPTED_LEVEL);
}

#[test]
fn cleanup_marks_cleared_orders_as_retired_for_late_open_replays() {
    let mut state = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
    state.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("p1")));
    state.apply(
        1,
        AccountProjectionEvent::PlaceAccepted {
            request_id: "p1".to_string(),
        },
    );
    state.apply(1, AccountProjectionEvent::OrderObserved(order(0.2, false)));

    state.clear_orders_preserving_pending_acks();
    let outcome = state.apply(1, AccountProjectionEvent::OrderObserved(order(0.2, false)));
    assert!(outcome.applied && outcome.order_changed);
    assert!(!outcome.unknown_current_run_order);
    assert_eq!(state.resting_quotes()[0].level, UNKNOWN_ADOPTED_LEVEL);
}

#[test]
fn late_unknown_open_after_verified_cleanup_forces_reconciliation() {
    // After a verified cleanup (either continuity), a fresh current-run
    // open order with no pending place must never silently become a
    // holdable quote: it is flagged unknown and adopted at the sentinel
    // level so reconciliation cancels it rather than resuming on it.
    for continuity in [
        OrderResponseContinuity::Preserved,
        OrderResponseContinuity::Replaced,
    ] {
        let mut state = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
        // Establish and adopt an initial quote, then verify-cleanup it.
        state.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("p1")));
        state.apply(
            1,
            AccountProjectionEvent::PlaceAccepted {
                request_id: "p1".to_string(),
            },
        );
        state.apply(1, AccountProjectionEvent::OrderObserved(order(0.2, false)));
        state.finish_verified_cleanup(continuity);
        assert!(
            state.resting_quotes().is_empty(),
            "{continuity:?}: cleanup must leave no executable quote"
        );

        // A brand-new current-run order (unseen id, no pending place) lands
        // late on the venue.
        let mut late = order(0.2, false);
        late.order_id = 4242;
        late.client_order_id = Some(format!("{PREFIX}q00000099x9"));
        let outcome = state.apply(1, AccountProjectionEvent::OrderObserved(late));

        assert!(
            outcome.unknown_current_run_order,
            "{continuity:?}: a late unknown current-run order must require reconciliation"
        );
        let resting = state.resting_quotes();
        assert_eq!(resting.len(), 1);
        assert_eq!(
            resting[0].level, UNKNOWN_ADOPTED_LEVEL,
            "{continuity:?}: it must be adopted at the sentinel level, not a holdable quote"
        );
    }
}

#[test]
fn account_reconnect_reset_preserves_unacked_order_response_registry() {
    let mut state = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
    state.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("p1")));
    state.apply(
        1,
        AccountProjectionEvent::CancelSubmitted(ProjectionPendingCancel {
            request_id: "c1".to_string(),
            order_id: 7,
            side: OrderSide::Buy,
            level: 0,
            price: 100.0,
            cycle: 1,
        }),
    );

    state.reset_after_cleanup_preserving_pending_acks(2, 0.0);
    assert_eq!(state.generation(), 2);
    assert!(state.pending_places().is_empty());
    assert!(state.pending_cancels().is_empty());
    assert!(matches!(
        state.pending_request("p1"),
        Some(ProjectionPendingRequest::Place(_))
    ));
    assert!(matches!(
        state.pending_request("c1"),
        Some(ProjectionPendingRequest::Cancel(_))
    ));

    state.apply(
        2,
        AccountProjectionEvent::PlaceAccepted {
            request_id: "p1".to_string(),
        },
    );
    state.apply(
        2,
        AccountProjectionEvent::CancelResolved {
            request_id: "c1".to_string(),
        },
    );
    assert_eq!(state.pending_request_count(), 0);
    state.reset_after_cleanup_preserving_pending_acks(3, 0.0);
    assert_eq!(
        state.completed_request_resolution("p1"),
        Some(ProjectionRequestResolution::PlaceAccepted)
    );
    assert_eq!(
        state.completed_request_resolution("c1"),
        Some(ProjectionRequestResolution::CancelResolved)
    );
}

#[test]
fn late_place_ack_matches_after_account_order_is_already_terminal() {
    let mut state = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
    state.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("p1")));
    state.apply(1, AccountProjectionEvent::OrderObserved(order(0.0, true)));
    assert!(state.pending_places().is_empty());
    assert!(matches!(
        state.pending_request("p1"),
        Some(ProjectionPendingRequest::Place(_))
    ));

    let outcome = state.apply(
        1,
        AccountProjectionEvent::PlaceAccepted {
            request_id: "p1".to_string(),
        },
    );
    assert!(outcome.applied);
    assert_eq!(state.pending_request_count(), 0);
}

#[test]
fn freeze_closes_quote_slots_but_preserves_unacked_response_registry() {
    let mut state = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
    state.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("p1")));
    state.apply(
        1,
        AccountProjectionEvent::CancelSubmitted(ProjectionPendingCancel {
            request_id: "c1".to_string(),
            order_id: 7,
            side: OrderSide::Buy,
            level: 0,
            price: 100.0,
            cycle: 1,
        }),
    );

    state.clear_orders_preserving_pending_acks();
    assert!(state.pending_places().is_empty());
    assert!(state.pending_cancels().is_empty());
    assert!(matches!(
        state.pending_request("p1"),
        Some(ProjectionPendingRequest::Place(_))
    ));
    assert!(matches!(
        state.pending_request("c1"),
        Some(ProjectionPendingRequest::Cancel(_))
    ));

    assert!(
        state
            .apply(
                1,
                AccountProjectionEvent::PlaceAccepted {
                    request_id: "p1".to_string(),
                },
            )
            .applied
    );
    assert!(
        state
            .apply(
                1,
                AccountProjectionEvent::CancelResolved {
                    request_id: "c1".to_string(),
                },
            )
            .applied
    );
    assert_eq!(state.pending_request_count(), 0);
}

#[test]
fn request_registry_is_strictly_bounded_and_rejects_duplicates() {
    let mut state = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
    for index in 0..MAX_PENDING_ORDER_REQUESTS {
        let outcome = state.apply(
            1,
            AccountProjectionEvent::PlaceSubmitted(pending(&format!("p{index}"))),
        );
        assert!(outcome.request_registry_error.is_none());
    }
    assert_eq!(state.pending_request_count(), MAX_PENDING_ORDER_REQUESTS);

    let overflow = state.apply(
        1,
        AccountProjectionEvent::PlaceSubmitted(pending("overflow")),
    );
    assert!(matches!(
        overflow.request_registry_error,
        Some(ProjectionRegistryError::Capacity {
            limit: MAX_PENDING_ORDER_REQUESTS
        })
    ));

    let mut duplicate = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
    duplicate.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("same")));
    let outcome = duplicate.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("same")));
    assert!(matches!(
        outcome.request_registry_error,
        Some(ProjectionRegistryError::DuplicateRequestId { .. })
    ));
}

#[test]
fn position_projects_independently_of_ordering() {
    let mut state = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
    let outcome = state.apply(
        1,
        AccountProjectionEvent::PositionObserved { position: 0.2 },
    );
    assert!(outcome.position_changed);
    assert_eq!(state.observed_position(), 0.2);
}

#[test]
fn order_before_position_and_position_before_order_converge() {
    let mut order_first = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
    order_first.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("p1")));
    order_first.apply(1, AccountProjectionEvent::OrderObserved(order(0.2, false)));
    order_first.apply(
        1,
        AccountProjectionEvent::PositionObserved { position: 0.2 },
    );

    let mut position_first = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
    position_first.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("p1")));
    position_first.apply(
        1,
        AccountProjectionEvent::PositionObserved { position: 0.2 },
    );
    position_first.apply(1, AccountProjectionEvent::OrderObserved(order(0.2, false)));

    assert_eq!(
        order_first.observed_position(),
        position_first.observed_position()
    );
    assert_eq!(
        order_first.resting_quotes(),
        position_first.resting_quotes()
    );
}

#[test]
fn rest_audit_tolerates_projection_absence_and_known_quantity_drift() {
    let mut state = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
    state.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("p1")));
    state.apply(1, AccountProjectionEvent::OrderObserved(order(0.2, false)));

    assert!(state.unexpected_rest_open_order_ids(1, &[]).is_empty());
    assert!(state
        .unexpected_rest_open_order_ids(1, &[order(0.1, false)])
        .is_empty());
    assert_eq!(state.resting_quotes()[0].qty, 0.2);
}

#[test]
fn rest_audit_tolerates_projected_order_plus_place_awaiting_account_stream() {
    let mut state = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
    state.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("p1")));
    state.apply(1, AccountProjectionEvent::OrderObserved(order(0.2, false)));

    let mut second = pending("p2");
    second.client_order_id = format!("{PREFIX}q00000002a0");
    second.side = OrderSide::Sell;
    second.price = 101.0;
    second.level = 1;
    second.cycle = 2;
    state.apply(1, AccountProjectionEvent::PlaceSubmitted(second.clone()));

    let mut projected = order(0.1, false);
    let rest_only = OrderObservation {
        order_id: 8,
        client_order_id: Some(second.client_order_id),
        side: second.side,
        price: second.price,
        open_qty: second.qty,
        terminal: false,
    };

    assert!(state
        .unexpected_rest_open_order_ids(1, &[projected.clone(), rest_only.clone()])
        .is_empty());

    state.apply(
        1,
        AccountProjectionEvent::PlaceAccepted {
            request_id: "p1".to_string(),
        },
    );
    projected.open_qty = 0.05;
    assert!(state
        .unexpected_rest_open_order_ids(1, &[projected, rest_only])
        .is_empty());
}

#[test]
fn rest_audit_rejects_unexpected_or_retired_current_run_open_order() {
    let mut state = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
    let unexpected = order(0.2, false);
    assert_eq!(
        state.unexpected_rest_open_order_ids(1, std::slice::from_ref(&unexpected)),
        vec![7]
    );

    state.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("p1")));
    state.apply(1, AccountProjectionEvent::OrderObserved(unexpected.clone()));
    state.apply(1, AccountProjectionEvent::OrderObserved(order(0.0, true)));
    assert_eq!(
        state.unexpected_rest_open_order_ids(1, std::slice::from_ref(&unexpected)),
        vec![7]
    );

    let mut wrong_run = unexpected;
    wrong_run.client_order_id = Some("sxmk-other-q00000001b0".to_string());
    assert!(state
        .unexpected_rest_open_order_ids(1, &[wrong_run])
        .is_empty());
    assert!(state
        .unexpected_rest_open_order_ids(2, &[order(0.2, false)])
        .is_empty());
}

#[test]
fn rest_audit_tolerates_order_until_pending_cancel_resolves() {
    let mut state = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
    state.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("p1")));
    let open = order(0.2, false);
    state.apply(1, AccountProjectionEvent::OrderObserved(open.clone()));
    state.apply(
        1,
        AccountProjectionEvent::CancelSubmitted(ProjectionPendingCancel {
            request_id: "c1".to_string(),
            order_id: open.order_id,
            side: open.side,
            level: 0,
            price: open.price,
            cycle: 2,
        }),
    );

    assert!(state
        .unexpected_rest_open_order_ids(1, std::slice::from_ref(&open))
        .is_empty());

    state.apply(
        1,
        AccountProjectionEvent::CancelResolved {
            request_id: "c1".to_string(),
        },
    );
    assert_eq!(state.unexpected_rest_open_order_ids(1, &[open]), vec![7]);
}

#[test]
fn rapid_cycle_advances_keep_unconfirmed_slots_reserved() {
    let mut state = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
    state.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("p1")));
    state.apply(
        1,
        AccountProjectionEvent::CancelSubmitted(ProjectionPendingCancel {
            request_id: "c1".to_string(),
            order_id: 9,
            side: OrderSide::Sell,
            level: 0,
            price: 101.0,
            cycle: 1,
        }),
    );
    assert_eq!(state.pending_places().len(), 1);
    assert_eq!(state.pending_cancels().len(), 1);

    // Account events can wake several cycles in one wall-clock second.
    // None may release the quote slot while the original venue request is
    // still awaiting a correlated outcome.
    for cycle in 2..=100 {
        state.apply(1, AccountProjectionEvent::AdvanceCycle { cycle });
    }
    assert_eq!(state.pending_places().len(), 1);
    assert_eq!(state.pending_cancels().len(), 1);
    assert_eq!(state.pending_request_count(), 2);
    assert!(matches!(
        state.pending_request("p1"),
        Some(ProjectionPendingRequest::Place(_))
    ));
    assert!(matches!(
        state.pending_request("c1"),
        Some(ProjectionPendingRequest::Cancel(_))
    ));

    // A rejection is the explicit terminal outcome that releases it.
    state.apply(
        1,
        AccountProjectionEvent::PlaceRejected {
            request_id: "p1".to_string(),
        },
    );
    assert!(state.pending_places().is_empty());
    assert_eq!(state.pending_request_count(), 1);
    state.apply(
        1,
        AccountProjectionEvent::CancelResolved {
            request_id: "c1".to_string(),
        },
    );
    assert!(state.pending_cancels().is_empty());
    assert_eq!(state.pending_request_count(), 0);
}

#[test]
fn accepted_place_stays_reserved_until_account_order_is_visible() {
    let mut state = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
    state.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("p1")));
    state.apply(
        1,
        AccountProjectionEvent::PlaceAccepted {
            request_id: "p1".to_string(),
        },
    );
    for cycle in 2..=100 {
        state.apply(1, AccountProjectionEvent::AdvanceCycle { cycle });
    }
    assert_eq!(state.pending_places().len(), 1);
    assert_eq!(state.pending_request_count(), 0);
    assert_eq!(
        state.completed_request_resolution("p1"),
        Some(ProjectionRequestResolution::PlaceAccepted)
    );

    let outcome = state.apply(1, AccountProjectionEvent::OrderObserved(order(0.2, false)));
    assert!(outcome.applied && outcome.order_changed);
    assert!(
        !outcome.unknown_current_run_order,
        "a delayed account update must retain the accepted place identity"
    );
    assert!(state.pending_places().is_empty());
    assert_eq!(outcome.effective_request_id.as_deref(), Some("p1"));
    assert_eq!(state.resting_quotes()[0].level, 0);
}

#[test]
fn rejected_place_tombstone_does_not_authorize_an_open_order() {
    let mut state = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
    state.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("p1")));
    state.apply(
        1,
        AccountProjectionEvent::PlaceRejected {
            request_id: "p1".to_string(),
        },
    );

    let outcome = state.apply(1, AccountProjectionEvent::OrderObserved(order(0.2, false)));
    assert!(outcome.unknown_current_run_order);
}

#[test]
fn completed_request_tombstones_are_bounded_and_reset_with_the_run() {
    let mut state = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
    for index in 0..=MAX_COMPLETED_ORDER_REQUESTS {
        let request_id = format!("p{index}");
        state.apply(
            1,
            AccountProjectionEvent::PlaceSubmitted(pending(&request_id)),
        );
        state.apply(1, AccountProjectionEvent::PlaceRejected { request_id });
    }
    assert_eq!(state.completed.len(), MAX_COMPLETED_ORDER_REQUESTS);
    assert_eq!(state.completed_request_resolution("p0"), None);
    assert_eq!(
        state.completed_request_resolution(&format!("p{MAX_COMPLETED_ORDER_REQUESTS}")),
        Some(ProjectionRequestResolution::PlaceRejected)
    );

    state.reset(2, 0.0);
    assert!(state.completed.is_empty());
}

#[test]
fn open_observation_adopts_pending_by_price_qty_heuristic() {
    let mut state = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
    state.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("p1")));

    // A different (but still current-run) client-order-id that matches the
    // pending place on side/price/qty is adopted via the heuristic branch.
    let mut observation = order(0.2, false);
    observation.order_id = 42;
    observation.client_order_id = Some(format!("{PREFIX}q00000009z9"));
    let outcome = state.apply(1, AccountProjectionEvent::OrderObserved(observation));

    assert!(outcome.applied && outcome.order_changed);
    assert!(
        !outcome.unknown_current_run_order,
        "a heuristic pending match is not an unknown order"
    );
    let resting = state.resting_quotes();
    assert_eq!(resting.len(), 1);
    assert_eq!(resting[0].level, 0, "adopts the pending place's level");
    assert!(state.pending_places().is_empty(), "the slot is consumed");
}

#[test]
fn unknown_current_run_order_adopts_with_sentinel_level() {
    let mut state = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);

    // A current-run order with no pending place and no prior projection is
    // adopted at the out-of-range sentinel level so reconcile cancels it.
    let outcome = state.apply(1, AccountProjectionEvent::OrderObserved(order(0.2, false)));
    assert!(outcome.applied);
    assert!(outcome.unknown_current_run_order);
    let resting = state.resting_quotes();
    assert_eq!(resting.len(), 1);
    assert_eq!(resting[0].level, UNKNOWN_ADOPTED_LEVEL);
}

#[test]
fn heuristic_adopts_pending_despite_one_ulp_price_echo_difference() {
    let mut state = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
    // pending("p1") rests a buy at price 100.0, qty 0.2, level 0.
    state.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("p1")));

    // The venue echoes the "same" price one ULP away (~1.4e-14 at 100) —
    // far above f64::EPSILON but far below half a price tick. The old
    // `<= f64::EPSILON` compare would miss the pending place and adopt the
    // order at the unknown sentinel level; the tick tolerance matches it.
    let echoed_price = f64::from_bits(100.0_f64.to_bits() + 1);
    assert_ne!(echoed_price, 100.0);
    assert!((echoed_price - 100.0).abs() > f64::EPSILON);

    let mut observation = order(0.2, false);
    observation.order_id = 55;
    // A current-run id that does NOT match the pending's client-order-id,
    // forcing the side/price/qty heuristic branch.
    observation.client_order_id = Some(format!("{PREFIX}q00000042c0"));
    observation.price = echoed_price;

    let outcome = state.apply(1, AccountProjectionEvent::OrderObserved(observation));
    assert!(outcome.applied && outcome.order_changed);
    assert!(
        !outcome.unknown_current_run_order,
        "a one-ULP price echo still matches its pending place"
    );
    assert_eq!(
        state.resting_quotes()[0].level,
        0,
        "adopts the pending place's real level, not the unknown sentinel"
    );
    assert!(state.pending_places().is_empty());
}
