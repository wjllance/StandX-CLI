use super::*;
use standx_sdk::order_response::OrderResponse;

pub(super) const ORDER_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

pub(super) fn order_request_timeout_detail(timeout: &TimedOutOrderRequest) -> String {
    format!(
        "order request lifecycle timed out after {:.3}s: kind={} request_id={} waiting_for={}; refusing further live orders",
        timeout.age.as_secs_f64(),
        timeout.kind.label(),
        timeout.request_id,
        timeout.phase.label(),
    )
}

pub(super) fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

pub(super) fn request_timeout_notice<'a>(
    timeout: &'a TimedOutOrderRequest,
    message: &'a str,
    symbol: &'a str,
    cycle: u64,
    expected_position: f64,
) -> RequestTimeoutNotice<'a> {
    RequestTimeoutNotice {
        message,
        symbol,
        cycle,
        request_id: &timeout.request_id,
        request_kind: timeout.kind.label(),
        timeout_phase: timeout.phase.label(),
        age_ms: duration_ms(timeout.age),
        timeout_ms: duration_ms(ORDER_REQUEST_TIMEOUT),
        recovery_target: timeout.phase.recovery_target().label(),
        expected_position,
    }
}

/// Operator-facing detail for a correlation that must stop live order work.
///
/// Every fail-closed stop has to carry enough to reconstruct where the request
/// came from and how far it had got, so the fields are always present:
/// the verdict, the request ID, the operation it carried, and its last known
/// lifecycle state. The variant-specific tail says what made it unsafe.
pub(super) fn correlation_failure_detail(
    correlation: &maker::ResponseCorrelation,
    request_id: Option<&str>,
    generation: u64,
) -> String {
    use maker::ResponseCorrelation;

    let tail = match correlation {
        ResponseCorrelation::VenueContradiction { .. } => {
            "account stream already showed this order live at the venue and the book adopted it, \
             but the command channel rejected the placement"
                .to_string()
        }
        ResponseCorrelation::Contradictory {
            resolution,
            resolved_in,
            ..
        } => format!(
            "acknowledgement contradicts the recorded resolution {} from generation {resolved_in}",
            resolution_label(*resolution)
        ),
        ResponseCorrelation::Orphan => {
            "request ID was never registered in this run".to_string()
        }
        ResponseCorrelation::Unverifiable => {
            "request is registered but its recorded resolution was evicted, so consistency cannot be checked".to_string()
        }
        ResponseCorrelation::Unidentified => {
            "acknowledgement carried no request_id, which the protocol requires".to_string()
        }
        // Not fail-closed; included so the match stays exhaustive if the policy
        // in `fails_closed` ever changes.
        ResponseCorrelation::Matched { .. } | ResponseCorrelation::LateKnown { .. } => {
            "correlated".to_string()
        }
    };
    format!(
        "order-response correlation failed closed: verdict={} request_id={} operation={} lifecycle={} generation={generation}; {tail}",
        correlation.label(),
        request_id.unwrap_or("<none>"),
        correlation
            .operation()
            .map_or("<unknown>", maker::RequestOperation::label),
        correlation.lifecycle().label(),
    )
}

fn resolution_label(resolution: maker::ProjectionRequestResolution) -> &'static str {
    match resolution {
        maker::ProjectionRequestResolution::PlaceAccepted => "place_accepted",
        maker::ProjectionRequestResolution::PlaceRejected => "place_rejected",
        maker::ProjectionRequestResolution::CancelResolved => "cancel_resolved",
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct CancelRejection {
    pub(super) request_id: String,
    pub(super) code: i64,
    pub(super) message: String,
}

pub(super) fn order_response_failure(
    outcome: &std::result::Result<maker::ResponseCorrelation, CancelRejection>,
    request_id: Option<&str>,
    generation: u64,
    runtime_state: &mut MakerState,
) -> Option<String> {
    match outcome {
        Ok(correlation) if correlation.fails_closed() => {
            // A malformed frame has no ID to report; the runtime still has to be
            // told the placement channel is no longer trustworthy.
            runtime_state.handle(MakerEvent::OrderResponseUnmatched {
                request_id: request_id.unwrap_or("<none>").to_string(),
            });
            Some(correlation_failure_detail(
                correlation,
                request_id,
                generation,
            ))
        }
        Err(rejection) => {
            let detail = maker::order_cancel_rejection_reason(
                &rejection.request_id,
                rejection.code,
                &rejection.message,
            );
            runtime_state.handle(MakerEvent::OrderCancelRejected {
                request_id: rejection.request_id.clone(),
                code: rejection.code,
                message: rejection.message.clone(),
            });
            Some(detail)
        }
        _ => None,
    }
}

pub(super) fn observe_order_ack(
    tracker: Option<&mut maker::OrderLatencyTracker>,
    started: Option<std::time::Instant>,
    request_id: Option<&str>,
    accepted: bool,
) {
    let (Some(tracker), Some(started), Some(request_id)) = (tracker, started, request_id) else {
        return;
    };
    let at_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    if let Err(error) = tracker.mark_ack(request_id, at_ms, accepted) {
        eprintln!("⚠️ order latency ack observation unavailable: {error}");
    }
}

pub(super) fn invalidate_session_latency(session: &mut LiveSession) {
    let generation = session.projection.generation();
    let at_ms = u64::try_from(session.latency_started.elapsed().as_millis()).unwrap_or(u64::MAX);
    if let Err(error) = session
        .order_latency
        .invalidate_generation(generation, at_ms)
    {
        eprintln!("⚠️ order latency generation invalidation unavailable: {error}");
    }
}

#[cfg(test)]
pub(in crate::commands::maker) fn apply_order_responses(
    receiver: &mut tokio::sync::mpsc::Receiver<OrderResponse>,
    projection: &mut MakerAccountProjection,
    runtime_state: &mut MakerState,
    output_format: OutputFormat,
    symbol: &str,
    cycle: u64,
    price_decimals: u32,
) -> Result<()> {
    apply_order_responses_observed(
        receiver,
        projection,
        runtime_state,
        OrderResponseObservation {
            output_format,
            symbol,
            cycle,
            price_decimals,
            latency: None,
            latency_started: None,
        },
        &CleanupTombstones::default(),
    )
}

pub(super) struct OrderResponseObservation<'a> {
    pub(super) output_format: OutputFormat,
    pub(super) symbol: &'a str,
    pub(super) cycle: u64,
    pub(super) price_decimals: u32,
    pub(super) latency: Option<&'a mut maker::OrderLatencyTracker>,
    pub(super) latency_started: Option<std::time::Instant>,
}

pub(super) fn apply_order_responses_observed(
    receiver: &mut tokio::sync::mpsc::Receiver<OrderResponse>,
    projection: &mut MakerAccountProjection,
    runtime_state: &mut MakerState,
    mut observation: OrderResponseObservation<'_>,
    cleanup_minted_request_ids: &CleanupTombstones,
) -> Result<()> {
    loop {
        let response = match receiver.try_recv() {
            Ok(response) => response,
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => return Ok(()),
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                return Err(anyhow::anyhow!(
                    "order-response stream disconnected; refusing further live orders"
                ));
            }
        };
        // An ack for a cleanup-minted WS cancel: cleanup already established the
        // venue state through `/api/query_order`, so the frame carries no new
        // information and must not trip the unknown-request-ID fail-closed
        // check. Non-consuming, because a two-frame ack can deliver both frames
        // here.
        if let Some(request_id) = response.request_id.as_deref() {
            if cleanup_minted_request_ids.covers(request_id) {
                continue;
            }
        }
        let request_id = response.request_id.clone();
        observe_order_ack(
            observation.latency.as_deref_mut(),
            observation.latency_started,
            request_id.as_deref(),
            response.accepted(),
        );
        let generation = projection.generation();
        let outcome = apply_order_response(
            response,
            projection,
            observation.output_format,
            observation.symbol,
            observation.cycle,
            observation.price_decimals,
        );
        if let Some(error) =
            order_response_failure(&outcome, request_id.as_deref(), generation, runtime_state)
        {
            return Err(anyhow::anyhow!(error));
        }
    }
}

pub(super) fn apply_order_response(
    response: OrderResponse,
    projection: &mut MakerAccountProjection,
    output_format: OutputFormat,
    symbol: &str,
    cycle: u64,
    price_decimals: u32,
) -> std::result::Result<maker::ResponseCorrelation, CancelRejection> {
    let request_id = response.request_id.as_deref();
    let correlation = projection.classify_response(request_id, response.accepted());
    // Only a live pending request produces new projection state. Everything else
    // is either idempotent (a late duplicate) or refused by the caller's
    // fail-closed policy, and must not mutate the projection on the way out.
    if !matches!(
        correlation,
        maker::ResponseCorrelation::Matched {
            lifecycle: maker::RequestLifecycle::AwaitingAck,
            ..
        }
    ) {
        return Ok(correlation);
    }
    let request_id = request_id.expect("a matched correlation carries a request ID");
    let pending = projection
        .pending_request(request_id)
        .cloned()
        .expect("a matched correlation has a pending entry");
    let generation = projection.generation();
    match pending {
        ProjectionPendingRequest::Cancel(_) => {
            if !response.accepted() {
                return Err(CancelRejection {
                    request_id: request_id.to_string(),
                    code: response.code,
                    message: response.message,
                });
            }
            projection.apply(
                generation,
                AccountProjectionEvent::CancelResolved {
                    request_id: request_id.to_string(),
                },
            );
        }
        ProjectionPendingRequest::Place(place) => {
            if response.accepted() {
                projection.apply(
                    generation,
                    AccountProjectionEvent::PlaceAccepted {
                        request_id: request_id.to_string(),
                    },
                );
            } else {
                projection.apply(
                    generation,
                    AccountProjectionEvent::PlaceRejected {
                        request_id: request_id.to_string(),
                    },
                );
                output::log_maker_event(output::MakerLogEvent {
                    output_format,
                    symbol,
                    cycle,
                    action: "place_rejected_async",
                    side: place.side,
                    level: place.level,
                    price: place.price,
                    price_decimals,
                    detail: &response.message,
                    exit_kind: None,
                });
            }
        }
    }
    Ok(correlation)
}

pub(super) struct AccountEventContext<'a> {
    pub(super) symbol: &'a str,
    pub(super) run_order_prefix: &'a str,
    pub(super) mark: f64,
    pub(super) cycle: u64,
    pub(super) output_format: OutputFormat,
}

pub(super) struct AccountEventState<'a> {
    pub(super) ledger: &'a mut MakerLedger,
    pub(super) stats: &'a mut MakerStats,
    pub(super) projection: &'a mut MakerAccountProjection,
}

#[derive(Debug, Default)]
pub(super) struct AccountEventOutcome {
    pub(super) fills: u64,
    pub(super) position_observations: Vec<f64>,
    pub(super) exit_fill_observed: bool,
    pub(super) balance_changed: bool,
    pub(super) requires_order_reconciliation: bool,
    pub(super) effective_request_ids: Vec<String>,
    pub(super) fill_order_ids: Vec<u64>,
}

impl AccountEventOutcome {
    fn merge(&mut self, other: Self) {
        let Self {
            fills,
            position_observations,
            exit_fill_observed,
            balance_changed,
            requires_order_reconciliation,
            effective_request_ids,
            fill_order_ids,
        } = other;
        self.fills += fills;
        self.position_observations.extend(position_observations);
        self.exit_fill_observed |= exit_fill_observed;
        self.balance_changed |= balance_changed;
        self.requires_order_reconciliation |= requires_order_reconciliation;
        self.effective_request_ids.extend(effective_request_ids);
        self.fill_order_ids.extend(fill_order_ids);
    }
}

pub(super) fn schedule_account_balance_refresh(
    requested: &mut bool,
    // Account-risk watch flag: an alert threshold **or** an armed stage 5-b
    // hard floor (see `account_risk_watch_enabled`). A floor-only session must
    // still get balance refreshes.
    account_risk_watched: bool,
    poll: &mut LiveAccountPollState,
    now: std::time::Instant,
) -> bool {
    if !std::mem::take(requested) || !account_risk_watched {
        return false;
    }
    poll.request_balance_refresh(now);
    true
}

/// Loop-carried state and context an account outcome feeds back into, borrowed
/// for the duration of one [`absorb_account_outcome`] call.
pub(super) struct OutcomeSink<'a> {
    pub(super) total_fills: &'a mut u64,
    pub(super) balance_refresh_requested: &'a mut bool,
    pub(super) inventory_exit_pending: &'a mut bool,
    pub(super) notifier: &'a MakerNotifier,
    pub(super) position_alert_anchor: &'a mut PositionAlertAnchor,
    pub(super) expected_position: f64,
    pub(super) max_position: f64,
    pub(super) inventory_exit_pct: f64,
    pub(super) qty_tolerance: f64,
    pub(super) symbol: &'a str,
    pub(super) cycle: u64,
    pub(super) order_latency: Option<&'a mut maker::OrderLatencyTracker>,
    pub(super) latency_started: Option<std::time::Instant>,
}

/// Fold one account-event outcome into the loop totals: accumulate fills, clear
/// the pending inventory exit once its fill is observed, and feed every ordered
/// position observation into risk detection. Returns the latest observed
/// position (if any) so the caller can apply its own mismatch bookkeeping — the
/// one part that legitimately differs between the cycle, replan, and
/// reconciliation paths.
pub(super) async fn absorb_account_outcome(
    outcome: AccountEventOutcome,
    mut sink: OutcomeSink<'_>,
) -> Option<f64> {
    if let (Some(tracker), Some(started)) =
        (sink.order_latency.as_deref_mut(), sink.latency_started)
    {
        let at_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        for order_id in &outcome.fill_order_ids {
            if let Err(error) = tracker.record_fill_after_cancel_order(*order_id, at_ms) {
                eprintln!("⚠️ fill-after-cancel observation unavailable: {error}");
            }
        }
        for request_id in &outcome.effective_request_ids {
            if let Err(error) = tracker.mark_effective(request_id, at_ms) {
                eprintln!("⚠️ order latency effective observation unavailable: {error}");
            }
        }
    }
    *sink.total_fills += outcome.fills;
    *sink.balance_refresh_requested |= outcome.balance_changed;
    if outcome.exit_fill_observed {
        *sink.inventory_exit_pending = false;
    }
    let position = outcome.position_observations.last().copied();
    for observed in outcome.position_observations {
        sink.notifier
            .position_jump(
                sink.position_alert_anchor,
                PositionChange {
                    observed,
                    expected: sink.expected_position,
                    max_position: sink.max_position,
                    inventory_exit_pct: sink.inventory_exit_pct,
                    qty_tolerance: sink.qty_tolerance,
                    symbol: sink.symbol,
                    cycle: sink.cycle,
                },
            )
            .await;
    }
    position
}
pub(super) fn apply_account_events(
    receiver: &mut tokio::sync::mpsc::Receiver<AccountEvent>,
    state: &mut AccountEventState<'_>,
    context: &AccountEventContext<'_>,
) -> Result<AccountEventOutcome> {
    let mut outcome = AccountEventOutcome::default();
    loop {
        let event = match receiver.try_recv() {
            Ok(event) => event,
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                return Ok(outcome);
            }
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                return Err(anyhow::anyhow!("authenticated account stream disconnected"));
            }
        };
        outcome.merge(apply_account_event(event, state, context)?);
    }
}

pub(super) fn account_event_invalidates_cycle(event: &AccountEvent) -> bool {
    matches!(
        event,
        AccountEvent::Position(_)
            | AccountEvent::Trade(_)
            | AccountEvent::Disconnected { .. }
            | AccountEvent::Error { .. }
    )
}

/// A position/trade event that arrives while a cycle owns in-flight work must
/// still take the reconciliation cleanup path even when its observed position
/// already agrees with the ledger. The reducer has queued `AbortInFlight` and
/// `Cleanup` at that point; skipping straight to the next cycle would leave
/// those effects pending and turn the abort into a spurious fail-safe stop.
pub(super) fn reconciliation_error_for_cycle(
    expected: f64,
    mismatch: Option<f64>,
    account_position_mismatch: Option<f64>,
    cycle_invalidated_by_account: bool,
) -> Option<PositionReconciliationError> {
    if let Some(observed) = mismatch.or(account_position_mismatch) {
        Some(PositionReconciliationError::position_mismatch(
            expected, observed,
        ))
    } else if cycle_invalidated_by_account {
        Some(PositionReconciliationError::cycle_invalidation(expected))
    } else {
        None
    }
}

pub(super) fn market_update_requires_replan(
    previous_mark: f64,
    mark: f64,
    best_bid: Option<f64>,
    best_ask: Option<f64>,
    resting: &[RestingQuote],
    refresh_bps: f64,
    max_divergence_bps: f64,
) -> bool {
    // Crossed book and mark/mid divergence share the exact skip rules the
    // strategy applies in preflight_cycle; route through the same predicate so
    // the replan trigger cannot drift from it.
    maker::bps_diff(mark, previous_mark) > refresh_bps
        || maker::touch_skip(mark, best_bid, best_ask, max_divergence_bps).is_some()
        || maker::resting_quotes_would_cross(resting, best_bid, best_ask)
}

pub(super) fn apply_account_event(
    event: AccountEvent,
    state: &mut AccountEventState<'_>,
    context: &AccountEventContext<'_>,
) -> Result<AccountEventOutcome> {
    output::emit_account_event_lag(context.output_format, &event, context.symbol, context.cycle);
    match event {
        AccountEvent::Connected { epoch } => {
            if epoch != state.projection.generation() {
                return Err(anyhow::anyhow!(
                    "stale account-stream generation {epoch}; current projection generation is {}",
                    state.projection.generation()
                ));
            }
            Ok(AccountEventOutcome::default())
        }
        AccountEvent::Order(update) => {
            let mut fills = Vec::new();
            let observation = model::stream_order_observation(&update)?;
            let exit_fill_observed = ledger::apply_order_update(
                state.ledger,
                &update,
                context.symbol,
                context.run_order_prefix,
                state.stats,
                &mut fills,
            )?;
            let generation = state.projection.generation();
            let projection_outcome = state.projection.apply(
                generation,
                AccountProjectionEvent::OrderObserved(observation),
            );
            let requires_order_reconciliation = projection_outcome.unknown_current_run_order;
            for fill in &fills {
                if let Some(order_id) = fill.order_id {
                    state.projection.apply(
                        generation,
                        AccountProjectionEvent::TradeApplied {
                            order_id,
                            qty: fill.qty,
                        },
                    );
                }
                emit_live_fill(fill, context.symbol, context.cycle, context.output_format);
            }
            Ok(AccountEventOutcome {
                fills: fills.len() as u64,
                position_observations: Vec::new(),
                exit_fill_observed,
                balance_changed: false,
                requires_order_reconciliation,
                effective_request_ids: projection_outcome
                    .effective_request_id
                    .into_iter()
                    .collect(),
                fill_order_ids: fills.iter().filter_map(|fill| fill.order_id).collect(),
            })
        }
        AccountEvent::Position(update) => {
            if !update.symbol.eq_ignore_ascii_case(context.symbol) {
                return Ok(AccountEventOutcome::default());
            }
            let qty =
                model::signed_position_quantity(&update.qty, update.side).map_err(|error| {
                    anyhow::anyhow!("account position update has invalid qty: {error}")
                })?;
            let generation = state.projection.generation();
            state.projection.apply(
                generation,
                AccountProjectionEvent::PositionObserved { position: qty },
            );
            Ok(AccountEventOutcome {
                fills: 0,
                position_observations: vec![qty],
                exit_fill_observed: false,
                balance_changed: false,
                requires_order_reconciliation: false,
                effective_request_ids: Vec::new(),
                fill_order_ids: Vec::new(),
            })
        }
        AccountEvent::Trade(trade) => {
            let mut fills = Vec::new();
            let exit_fill_observed = ledger::apply_account_trade(
                state.ledger,
                trade,
                context.symbol,
                context.mark,
                state.stats,
                &mut fills,
            )?;
            let generation = state.projection.generation();
            for fill in &fills {
                if let Some(order_id) = fill.order_id {
                    state.projection.apply(
                        generation,
                        AccountProjectionEvent::TradeApplied {
                            order_id,
                            qty: fill.qty,
                        },
                    );
                }
                emit_live_fill(fill, context.symbol, context.cycle, context.output_format);
            }
            Ok(AccountEventOutcome {
                fills: fills.len() as u64,
                position_observations: Vec::new(),
                exit_fill_observed,
                balance_changed: false,
                requires_order_reconciliation: false,
                effective_request_ids: Vec::new(),
                fill_order_ids: fills.iter().filter_map(|fill| fill.order_id).collect(),
            })
        }
        // A balance update only signals that the REST-backed margin snapshot
        // the alert floors read is now stale; flag a refresh. The raw wallet
        // fields are not projected — nothing reads a projected balance.
        AccountEvent::Balance(_update) => Ok(AccountEventOutcome {
            balance_changed: true,
            ..AccountEventOutcome::default()
        }),
        AccountEvent::Disconnected { reason } | AccountEvent::Error { reason } => Err(
            anyhow::anyhow!("authenticated account stream unhealthy: {reason}"),
        ),
    }
}

pub(super) fn accounting_position_mismatch(
    expected_position: f64,
    stats_position: f64,
    qty_tolerance: f64,
) -> bool {
    let delta = (stats_position - expected_position).abs();
    // Fail closed: a non-finite delta (NaN from a poisoned position) would make
    // a bare `>` comparison false and silently pass the invariant, so treat any
    // non-finite value as a mismatch.
    !delta.is_finite() || delta > qty_tolerance
}
