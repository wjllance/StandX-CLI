use super::events::AccountStreamDisconnected;
use super::*;
use crate::commands::maker::model::StreamHealth;
use crate::commands::maker::output::ts_now;

pub(super) fn mark_request_timeout_stream_unhealthy(
    account_stream_health: &standx_sdk::account_stream::AccountStreamHealth,
    order_response_health: &standx_sdk::order_response::OrderResponseHealth,
    timeout: &TimedOutOrderRequest,
    detail: &str,
) {
    match timeout.phase.recovery_target() {
        RecoveryTarget::AccountStream => account_stream_health.mark_unhealthy(detail.to_string()),
        RecoveryTarget::OrderResponse => order_response_health.mark_unhealthy(detail.to_string()),
        RecoveryTarget::PositionReconciliation | RecoveryTarget::MarketData => {
            unreachable!("request timeout phases never target position reconciliation")
        }
    }
}

pub(super) fn take_cleanup_effect(
    runtime_state: &mut MakerState,
    expected_target: RecoveryTarget,
) -> Result<WorkToken> {
    loop {
        match runtime_state.next_effect() {
            Some(MakerEffect::AbortInFlight(_)) => {}
            Some(MakerEffect::Cleanup { token, target }) if target == expected_target => {
                return Ok(token);
            }
            Some(effect) => {
                return Err(anyhow::anyhow!(
                    "runtime expected {expected_target:?} cleanup, got {effect:?}"
                ));
            }
            None => {
                return Err(anyhow::anyhow!(
                    "runtime did not emit {expected_target:?} cleanup"
                ));
            }
        }
    }
}

pub(super) fn take_recovery_effect(
    runtime_state: &mut MakerState,
    expected_target: RecoveryTarget,
) -> Result<WorkToken> {
    match runtime_state.next_effect() {
        Some(MakerEffect::Recover { token, target }) if target == expected_target => Ok(token),
        Some(effect) => Err(anyhow::anyhow!(
            "runtime expected {expected_target:?} recovery, got {effect:?}"
        )),
        None => Err(anyhow::anyhow!(
            "runtime did not emit {expected_target:?} recovery"
        )),
    }
}

/// Drains queued `AbortInFlight` effects and returns the `Stop` exit the
/// runtime is expected to have queued. The `fallback` names the `MakerExit`
/// variant to use if the runtime is in an unexpected state — the CLI already
/// knows which fault it is handling, so it supplies the variant rather than
/// re-asserting it at each call site.
pub(super) fn take_stop_effect(
    runtime_state: &mut MakerState,
    fallback: fn(String) -> MakerExit,
) -> MakerExit {
    loop {
        match runtime_state.next_effect() {
            Some(MakerEffect::AbortInFlight(_)) => {}
            Some(MakerEffect::Stop(reason)) => return reason.into(),
            Some(effect) => {
                return fallback(format!("runtime expected stop effect, got {effect:?}"));
            }
            None => return fallback("runtime did not emit stop effect".to_string()),
        }
    }
}

pub(super) fn recovery_failed_exit(
    runtime_state: &mut MakerState,
    token: WorkToken,
    reason: String,
) -> MakerExit {
    runtime_state.handle(MakerEvent::RecoveryFailed { token, reason });
    take_stop_effect(runtime_state, MakerExit::PositionReconciliation)
}

pub(super) fn stop_requested_exit(
    runtime_state: &mut MakerState,
    reason: RuntimeStopReason,
) -> MakerExit {
    runtime_state.handle(MakerEvent::StopRequested(reason));
    take_stop_effect(runtime_state, MakerExit::PositionReconciliation)
}

/// How a missing or mismatched runtime effect maps to the flow's stop reason.
/// (`RuntimeStopReason::CleanupFailure` carries the target, so a plain
/// `fn(String) -> RuntimeStopReason` pointer cannot express it.)
#[derive(Clone, Copy)]
pub(super) enum EffectFailureStop {
    CleanupFailure,
    OrderResponse,
    PositionReconciliation,
    MarketData,
}

pub(super) fn effect_failure_stop(
    kind: EffectFailureStop,
    target: RecoveryTarget,
    reason: String,
) -> RuntimeStopReason {
    match kind {
        EffectFailureStop::CleanupFailure => RuntimeStopReason::CleanupFailure { target, reason },
        EffectFailureStop::OrderResponse => RuntimeStopReason::OrderResponse(reason),
        EffectFailureStop::PositionReconciliation => {
            RuntimeStopReason::PositionReconciliation(reason)
        }
        EffectFailureStop::MarketData => RuntimeStopReason::MarketData(reason),
    }
}

/// Payload for the position-reconciliation flow's structured
/// `emit_reconciliation_state` emits around freeze and resume.
pub(super) struct ReconciliationStateNote {
    pub(super) cause: &'static str,
    pub(super) expected: f64,
    pub(super) observed: f64,
}

/// Borrowed bundle of the `run_maker` locals every incident-recovery block
/// touches. Constructed fresh immediately before each helper call; all
/// borrows end when the helper returns, so flow-specific code in between
/// re-borrows the same locals freely.
pub(super) struct RecoveryIo<'a> {
    pub(super) runtime_state: &'a mut MakerState,
    pub(super) notifier: &'a MakerNotifier,
    pub(super) client: &'a StandXClient,
    pub(super) session: Option<&'a mut LiveSession>,
    pub(super) resting: &'a mut Vec<RestingQuote>,
    pub(super) inventory_exit_pending: &'a mut bool,
    pub(super) next_cycle_is_recovery: &'a mut bool,
    pub(super) symbol: &'a str,
    pub(super) cycle: u64,
    pub(super) output_format: OutputFormat,
}

/// Everything that differs between the incident flows' freeze/cleanup
/// preambles. The `notice` is built entirely by the caller so the risk
/// payloads stay reviewable string-for-string at the call sites.
pub(super) enum FreezeNotice<'a> {
    Risk(RiskNotice<'a>),
    RequestTimeout(RequestTimeoutNotice<'a>),
}

pub(super) struct FreezeSpec<'a> {
    pub(super) target: RecoveryTarget,
    pub(super) trigger: MakerEvent,
    pub(super) cleanup_effect_stop: EffectFailureStop,
    pub(super) recovery_effect_stop: EffectFailureStop,
    /// Prepended to `"freeze cleanup failed: {error}"` in the CleanupFailed
    /// reason so each flow keeps its exact historical wording.
    pub(super) cleanup_failure_prefix: String,
    pub(super) cleanup_failed_exit: fn(String) -> MakerExit,
    pub(super) notice: FreezeNotice<'a>,
    pub(super) frozen_note: Option<ReconciliationStateNote>,
    /// Account-stream flow only: abort the stale stream task before
    /// reporting cleanup complete.
    pub(super) abort_account_stream_handle: bool,
    /// Whether the order-response channel survives this freeze (account-stream
    /// and reconciliation) or is being replaced (order-response), deciding
    /// whether pending request acks stay correlated across the cleanup.
    pub(super) continuity: OrderResponseContinuity,
    /// Live flows cancel and verify venue orders; paper recovery only clears
    /// its simulated in-memory maker book.
    pub(super) cancel_venue_orders: bool,
    /// Price formatting for any pre-freeze response the cleanup drain captures
    /// and the preamble re-applies (e.g. a late async place rejection).
    pub(super) price_decimals: u32,
}

/// Live halves [`replay_leftover_responses`] needs from the session, bundled
/// so the function stays under clippy's argument-count lint.
pub(super) struct LeftoverReplayContext<'a> {
    pub(super) projection: &'a mut MakerAccountProjection,
    pub(super) order_latency: &'a mut maker::OrderLatencyTracker,
    pub(super) latency_started: std::time::Instant,
    pub(super) order_response_health: &'a standx_sdk::order_response::OrderResponseHealth,
    pub(super) output_format: OutputFormat,
    pub(super) symbol: &'a str,
    pub(super) cycle: u64,
    pub(super) price_decimals: u32,
}

/// Re-applies order-response frames the cleanup drain captured for requests it
/// did not mint (acks for pre-freeze cycle requests) through the canonical
/// response path, so the pending-request lifecycle stays correlated across the
/// cleanup. Returns the first fail-closed reason encountered, if any, and marks
/// the order-response stream unhealthy when it does — but does *not* itself
/// stop the runtime.
///
/// It cannot: `order_response_failure` raises `OrderResponseUnmatched` /
/// `OrderCancelRejected` through `runtime_state.handle`, and that event is a
/// no-op once the runtime is already `Frozen` (`MakerState::freeze` returns no
/// effects in that phase — see `standx_maker::runtime`). The freeze preamble is
/// always mid-`Frozen` when this runs, so the caller must route a `Some`
/// return through the same stop path a cleanup failure already uses instead of
/// trusting the event to have queued one.
pub(super) fn replay_leftover_responses(
    leftover_responses: Vec<OrderResponse>,
    ctx: LeftoverReplayContext<'_>,
    runtime_state: &mut MakerState,
) -> Option<String> {
    let LeftoverReplayContext {
        projection,
        order_latency,
        latency_started,
        order_response_health,
        output_format,
        symbol,
        cycle,
        price_decimals,
    } = ctx;
    for response in leftover_responses {
        let request_id = response.request_id.clone();
        observe_order_ack(
            Some(order_latency),
            Some(latency_started),
            request_id.as_deref(),
            response.accepted(),
        );
        let generation = projection.generation();
        let outcome = apply_order_response(
            response,
            projection,
            output_format,
            symbol,
            cycle,
            price_decimals,
        );
        if let Some(reason) =
            order_response_failure(&outcome, request_id.as_deref(), generation, runtime_state)
        {
            order_response_health.mark_unhealthy(reason.clone());
            return Some(reason);
        }
    }
    None
}

/// Freeze/cleanup preamble shared by the incident-recovery flows:
/// report the fault to the runtime, drain the cleanup effect, notify,
/// cancel every maker order, reset the local book state, and drain the
/// recovery effect. `Ok` carries the recovery token for the flow-specific
/// reconnect/convergence work; `Err` carries the exit the caller must
/// `break 'main` with.
pub(super) async fn freeze_and_cleanup_for_recovery(
    io: &mut RecoveryIo<'_>,
    spec: FreezeSpec<'_>,
) -> std::result::Result<WorkToken, MakerExit> {
    io.runtime_state.handle(spec.trigger);
    let cleanup_token = match take_cleanup_effect(io.runtime_state, spec.target) {
        Ok(token) => token,
        Err(error) => {
            return Err(stop_requested_exit(
                io.runtime_state,
                effect_failure_stop(spec.cleanup_effect_stop, spec.target, error.to_string()),
            ));
        }
    };
    if let Some(note) = &spec.frozen_note {
        emit_reconciliation_state(
            io.output_format,
            io.symbol,
            io.cycle,
            "frozen",
            note.cause,
            note.expected,
            note.observed,
        );
    }
    match spec.notice {
        FreezeNotice::Risk(notice) => io.notifier.risk(notice, false).await,
        FreezeNotice::RequestTimeout(notice) => io.notifier.request_timeout(notice, false).await,
    }
    // Acks the cleanup drain captures for pre-freeze cycle requests. Owned out
    // here, and handed to the cleanup as a borrow, so that a frame the drain has
    // already taken off the channel survives a cleanup attempt that later fails
    // — dropping it would leave its pending request unresolvable.
    let mut leftover_responses: Vec<OrderResponse> = Vec::new();
    let cleanup: Result<()> = if spec.cancel_venue_orders {
        // Primary verdict path: while the order-response stream is healthy, the
        // cleanup cancels over WS and trusts the correlated `success` ack. The
        // stream being replaced (order-response freeze), torn down (shutdown),
        // or absent (startup) degrades to the REST point-query verdict.
        let mut ws_cleanup = match io.session.as_deref_mut() {
            Some(session) if session.order_response_health.is_healthy() => Some(WsCleanupContext {
                commands: &session.order_commands,
                responses: &mut session.order_responses,
                health: &session.order_response_health,
                minted: &mut session.cleanup_minted_request_ids,
                leftover: &mut leftover_responses,
            }),
            _ => None,
        };
        cancel_maker_orders_with_retry(
            io.client,
            io.symbol,
            3,
            io.output_format,
            ws_cleanup.as_mut(),
        )
        .await
    } else {
        Ok(())
    };
    // Re-applied through the canonical response path whatever the cleanup
    // verdict was, so the pending-request lifecycle stays correlated across the
    // cleanup (`finish_verified_cleanup` below preserves those acks).
    //
    // A fail-closed correlation discovered here cannot rely on the
    // `OrderResponseUnmatched` / `OrderCancelRejected` event `order_response_failure`
    // raises to queue a new recovery: the runtime is already `Frozen` from
    // `spec.trigger` above, and `MakerState::freeze` is a no-op once already
    // frozen (see `standx_maker::runtime`), so that event would otherwise be
    // silently discarded and the preamble would report `CleanupCompleted` over a
    // contradiction it never actually resolved. Folding the failure into
    // `cleanup` below routes it through the same `CleanupFailed` stop the
    // cancellation verdict already uses.
    let leftover_failure = io.session.as_deref_mut().and_then(|session| {
        replay_leftover_responses(
            leftover_responses,
            LeftoverReplayContext {
                projection: &mut session.projection,
                order_latency: &mut session.order_latency,
                latency_started: session.latency_started,
                order_response_health: &session.order_response_health,
                output_format: io.output_format,
                symbol: io.symbol,
                cycle: io.cycle,
                price_decimals: spec.price_decimals,
            },
            io.runtime_state,
        )
    });
    let cleanup: Result<()> = match (cleanup, leftover_failure) {
        (Err(error), _) => Err(error),
        (Ok(()), Some(reason)) => Err(anyhow::anyhow!(
            "leftover order-response replay failed closed: {reason}"
        )),
        (Ok(()), None) => Ok(()),
    };
    if let Err(error) = cleanup {
        io.runtime_state.handle(MakerEvent::CleanupFailed {
            token: cleanup_token,
            reason: format!(
                "{}freeze cleanup failed: {error}",
                spec.cleanup_failure_prefix
            ),
        });
        return Err(take_stop_effect(io.runtime_state, spec.cleanup_failed_exit));
    }
    io.resting.clear();
    if let Some(session) = io.session.as_deref_mut() {
        invalidate_session_latency(session);
        session.projection.finish_verified_cleanup(spec.continuity);
    }
    *io.inventory_exit_pending = false;
    if spec.abort_account_stream_handle {
        if let Some(session) = io.session.as_deref_mut() {
            session.account_stream_handle.abort();
        }
    }
    io.runtime_state
        .handle(MakerEvent::CleanupCompleted(cleanup_token));
    match take_recovery_effect(io.runtime_state, spec.target) {
        Ok(token) => Ok(token),
        Err(error) => Err(stop_requested_exit(
            io.runtime_state,
            effect_failure_stop(spec.recovery_effect_stop, spec.target, error.to_string()),
        )),
    }
}

/// Everything that differs between the incident flows' resume tails.
/// As with [`FreezeSpec`], the resolved `notice` is built entirely by the
/// caller.
pub(super) struct ResumeSpec<'a> {
    pub(super) recovery_token: WorkToken,
    /// Venue position fed to the projection as authoritative after recovery.
    pub(super) observed: f64,
    pub(super) continuity: OrderResponseContinuity,
    /// Order-response flow only: the paper book is cleared again because the
    /// placement channel was replaced underneath it.
    pub(super) clear_resting: bool,
    pub(super) recovered_note: Option<ReconciliationStateNote>,
    pub(super) notice: RiskNotice<'a>,
}

/// Resume tail shared by the incident-recovery flows: reset the
/// projection to an empty verified book, seed it with the reconciled venue
/// position, report RecoverySucceeded, and send the resolved notice. The
/// caller performs its flow-specific final book verification and session
/// half installation *before* calling this, and `continue`s the main loop
/// afterwards.
pub(super) async fn resume_quoting_after_recovery(io: &mut RecoveryIo<'_>, spec: ResumeSpec<'_>) {
    if spec.clear_resting {
        io.resting.clear();
    }
    if let Some(session) = io.session.as_deref_mut() {
        session.projection.finish_verified_cleanup(spec.continuity);
        let generation = session.projection.generation();
        session.projection.apply(
            generation,
            AccountProjectionEvent::PositionObserved {
                position: spec.observed,
            },
        );
    }
    io.runtime_state
        .handle(MakerEvent::RecoverySucceeded(spec.recovery_token));
    *io.next_cycle_is_recovery = true;
    // The mutations above must stay synchronous and contiguous: no awaits or
    // output may be inserted between them, so the runtime state, projection,
    // and loop flags advance atomically with respect to the notice below.
    if let Some(note) = &spec.recovered_note {
        emit_reconciliation_state(
            io.output_format,
            io.symbol,
            io.cycle,
            "recovered",
            note.cause,
            note.expected,
            note.observed,
        );
    }
    io.notifier.risk(spec.notice, false).await;
}

pub(super) async fn accounting_invariant_exit(
    notifier: &MakerNotifier,
    symbol: &str,
    cycle: u64,
    expected_position: f64,
    stats_position: f64,
    qty_tolerance: f64,
) -> Option<String> {
    if !accounting_position_mismatch(expected_position, stats_position, qty_tolerance) {
        return None;
    }
    let detail = format!(
        "stats position {stats_position:+.8} differs from ledger expected {expected_position:+.8} beyond tolerance {qty_tolerance:.8}"
    );
    notifier
        .risk(
            RiskNotice::critical("accounting_invariant", "mismatch", &detail, symbol, cycle)
                .position_after(expected_position)
                .expected(expected_position)
                .observed(stats_position),
            true,
        )
        .await;
    Some(detail)
}

impl MakerRuntime {
    pub(super) async fn pre_cycle_phase(&mut self) -> LoopDirective {
        self.monitor_token_expiry().await;
        match self.recover_market_data_phase().await {
            LoopDirective::Proceed => {}
            directive => return directive,
        }
        match self.recover_account_stream_phase().await {
            LoopDirective::Proceed => {}
            directive => return directive,
        }
        match self.recover_order_response_phase().await {
            LoopDirective::Proceed => {}
            directive => return directive,
        }
        match self.drain_live_events_phase().await {
            LoopDirective::Proceed => {}
            directive => return directive,
        }
        self.accounting_invariant_phase().await
    }

    async fn monitor_token_expiry(&mut self) {
        let live = self.deps.args.live;
        let symbol = &self.deps.symbol;
        let notifier = &self.deps.notifier;
        let cycle = self.loop_state.counters.cycle;
        if live {
            // JWT expiry monitor. There is no renewal endpoint, so we can only
            // warn: escalate through Warning → Critical and alert once per band.
            let due = self
                .lifecycle
                .last_token_expiry_check
                .map(|last| last.elapsed() >= TOKEN_EXPIRY_CHECK_INTERVAL)
                .unwrap_or(true);
            if due {
                self.lifecycle.last_token_expiry_check = Some(std::time::Instant::now());
                if let Ok(creds) = Credentials::load() {
                    let remaining = creds.remaining_seconds();
                    let level = token_expiry_level(
                        remaining,
                        TOKEN_EXPIRY_WARN_SECS,
                        TOKEN_EXPIRY_CRITICAL_SECS,
                    );
                    if level > self.lifecycle.token_expiry_alerted {
                        self.lifecycle.token_expiry_alerted = level;
                        let (severity, event) = match level {
                            TokenExpiryLevel::Critical => {
                                (RiskSeverity::Critical, "token_expiry_critical")
                            }
                            _ => (RiskSeverity::Warning, "token_expiry_warning"),
                        };
                        let minutes = remaining / 60;
                        let message = format!(
                            "auth token expires in ~{minutes}m ({}); no renewal endpoint — run 'standx auth login' before it lapses or the bot will halt",
                            creds.expires_at_string()
                        );
                        notifier
                            .risk(
                                RiskNotice::with_severity(
                                    severity,
                                    "token_expiry",
                                    event,
                                    &message,
                                    symbol,
                                    cycle,
                                ),
                                false,
                            )
                            .await;
                    }
                }
            }
        }
    }

    async fn recover_market_data_phase(&mut self) -> LoopDirective {
        let live = self.deps.args.live;
        let max_divergence_bps = self.deps.args.max_divergence_bps;
        let output_format = self.deps.output_format;
        let client = &self.deps.client;
        let symbol = &self.deps.symbol;
        let notifier = &self.deps.notifier;
        let cycle = self.loop_state.counters.cycle;
        let exit = 'phase: {
            let pending_market_recovery;
            if let Some(detail) = self.market.pending_degradation.take() {
                let frozen_message =
                    format!("{detail}; placements frozen and maker cleanup starting");
                let recovery_token = match freeze_and_cleanup_for_recovery(
                    &mut RecoveryIo {
                        runtime_state: &mut self.recovery.runtime_state,
                        notifier,
                        client,
                        session: self.live_session.as_mut(),
                        resting: &mut self.loop_state.resting,
                        inventory_exit_pending: &mut self.loop_state.inventory_exit_pending,
                        next_cycle_is_recovery: &mut self.loop_state.next_cycle_is_recovery,
                        symbol,
                        cycle,
                        output_format,
                    },
                    FreezeSpec {
                        target: RecoveryTarget::MarketData,
                        // The event is normally already applied at the detection
                        // site. Re-applying it is intentionally idempotent while
                        // frozen and lets this preamble remain self-contained.
                        trigger: MakerEvent::MarketDataDegraded(detail.clone()),
                        cleanup_effect_stop: EffectFailureStop::CleanupFailure,
                        recovery_effect_stop: EffectFailureStop::MarketData,
                        cleanup_failure_prefix: "market data ".to_string(),
                        cleanup_failed_exit: MakerExit::MarketData,
                        notice: FreezeNotice::Risk(
                            RiskNotice::warning(
                                "market_data",
                                "degraded_frozen",
                                &frozen_message,
                                symbol,
                                cycle,
                            )
                            .position_after(self.loop_state.ledger.expected_position)
                            .expected(self.loop_state.ledger.expected_position),
                        ),
                        frozen_note: None,
                        abort_account_stream_handle: false,
                        continuity: OrderResponseContinuity::Preserved,
                        cancel_venue_orders: live,
                        price_decimals: self.deps.cfg.price_decimals,
                    },
                )
                .await
                {
                    Ok(token) => token,
                    Err(exit) => break 'phase exit,
                };
                pending_market_recovery = Some((recovery_token, detail));
            } else {
                pending_market_recovery = None;
            }

            if !self.market.health.is_degraded() {
                return LoopDirective::Proceed;
            }

            if let Some((recovery_token, _)) = pending_market_recovery {
                // Cleanup closes the reducer's frozen generation. Market data
                // remains paused in the pure health gate, so normal account and
                // order ingestion can resume without allowing new orders.
                self.recovery
                    .runtime_state
                    .handle(MakerEvent::RecoverySucceeded(recovery_token));
                let now = std::time::Instant::now();
                self.market.standby_started = Some(now);
                self.market.next_heartbeat = Some(now + MARKET_DATA_STANDBY_HEARTBEAT);
                self.market.maker_book_verified_empty = true;
                eprintln!(
                    "⚠️  market data paused; waiting for {} paired quoteable WS snapshots before re-quoting",
                    maker::MARKET_DATA_COHERENT_SNAPSHOTS_TO_RECOVER
                );
                return LoopDirective::Restart;
            }

            let now = std::time::Instant::now();
            if self
                .market
                .next_heartbeat
                .is_some_and(|deadline| now >= deadline)
            {
                let standby_secs = self.market.standby_started.map_or(0, |started| {
                    now.saturating_duration_since(started).as_secs()
                });
                let class = self
                    .market
                    .health
                    .degraded_class()
                    .expect("degraded market data must have a fault class");
                // Structured twin of the heartbeat below: same facts, but as
                // fields, so standby duration can be counted instead of parsed
                // out of a sentence.
                emit_market_data_standby(
                    output_format,
                    symbol,
                    cycle,
                    MarketDataStandby {
                        fault_class: class.label(),
                        paused_secs: standby_secs,
                        quoteable_streak: self.market.health.quoteable_streak(),
                        snapshots_required: maker::MARKET_DATA_COHERENT_SNAPSHOTS_TO_RECOVER,
                        divergence_bps: match class {
                            maker::MarketDataFaultClass::MarketState => {
                                self.market.last_divergence_bps
                            }
                            maker::MarketDataFaultClass::Transport => None,
                        },
                        threshold_bps: max_divergence_bps,
                        maker_book_empty: self.market.maker_book_verified_empty,
                    },
                );
                let (event, message) = match class {
                    maker::MarketDataFaultClass::MarketState => {
                        let divergence = self.market.last_divergence_bps.map_or_else(
                            || "n/a".to_string(),
                            |bps| format!("{bps:.2}bps"),
                        );
                        (
                            "divergence_standby",
                            format!(
                                "market-state standby: divergence={divergence} threshold={:.2}bps paused={}s quoteable_streak={}/{} maker_book_empty={}",
                                max_divergence_bps,
                                standby_secs,
                                self.market.health.quoteable_streak(),
                                maker::MARKET_DATA_COHERENT_SNAPSHOTS_TO_RECOVER,
                                self.market.maker_book_verified_empty,
                            ),
                        )
                    }
                    maker::MarketDataFaultClass::Transport => (
                        "transport_standby",
                        format!(
                            "market-data transport standby: paused={}s quoteable_streak={}/{} maker_book_empty={}; retrying while placements remain frozen",
                            standby_secs,
                            self.market.health.quoteable_streak(),
                            maker::MARKET_DATA_COHERENT_SNAPSHOTS_TO_RECOVER,
                            self.market.maker_book_verified_empty,
                        ),
                    ),
                };
                notifier
                    .risk(
                        RiskNotice::warning("market_data", event, &message, symbol, cycle)
                            .position_after(self.loop_state.ledger.expected_position)
                            .expected(self.loop_state.ledger.expected_position),
                        false,
                    )
                    .await;
                self.market.next_heartbeat = Some(now + MARKET_DATA_STANDBY_HEARTBEAT);
            }

            if self.market.health.recovery_ready() {
                self.market.maker_book_verified_empty = false;
                if live {
                    if let Err(error) =
                        cancel_maker_orders_with_retry(client, symbol, 3, output_format, None).await
                    {
                        break 'phase stop_requested_exit(
                            &mut self.recovery.runtime_state,
                            RuntimeStopReason::MarketData(format!(
                                "market data recovery book verification failed: {error}"
                            )),
                        );
                    }
                }
                self.loop_state.resting.clear();
                if let Some(session) = self.live_session.as_mut() {
                    invalidate_session_latency(session);
                    session
                        .projection
                        .finish_verified_cleanup(OrderResponseContinuity::Preserved);
                }
                self.market.maker_book_verified_empty = true;

                let latest = match self.market.feed.as_ref() {
                    Some(feed) => {
                        let state = feed.read().await;
                        fresh_ws_sample(&state).map(|(mark, bid, ask, _)| {
                            classify_market_health(mark, bid, ask, max_divergence_bps)
                        })
                    }
                    None => None,
                };
                let Some(latest) = latest else {
                    let now_ms = duration_ms(self.market.health_started.elapsed());
                    let _ = self
                        .market
                        .health
                        .observe(now_ms, maker::MarketDataObservation::InvalidSnapshot);
                    self.market.maker_book_verified_empty = false;
                    return LoopDirective::Restart;
                };
                self.market.last_divergence_bps = latest.divergence_bps;
                if latest.observation != maker::MarketDataObservation::Coherent {
                    let now_ms = duration_ms(self.market.health_started.elapsed());
                    let _ = self.market.health.observe(now_ms, latest.observation);
                    self.market.maker_book_verified_empty = false;
                    return LoopDirective::Restart;
                }

                if matches!(
                    self.market.health.confirm_recovered(),
                    maker::MarketDataTransition::Recovered
                ) {
                    let observed = self.loop_state.ledger.expected_position;
                    self.market.standby_started = None;
                    self.market.next_heartbeat = None;
                    self.market.last_divergence_bps = None;
                    self.market.last_src = None;
                    self.loop_state.next_cycle_is_recovery = true;
                    notifier
                        .risk(
                            RiskNotice::resolved("market_data", "recovered", "market data recovered with consecutive quoteable snapshots and a verified empty maker book; quoting may resume", symbol, cycle).position_after(observed).expected(observed).observed(observed),
                            false,
                        )
                        .await;
                    return LoopDirective::Restart;
                }
            }
            return LoopDirective::Proceed;
        };
        LoopDirective::Exit(exit)
    }

    async fn recover_account_stream_phase(&mut self) -> LoopDirective {
        let args = &self.deps.args;
        let output_format = self.deps.output_format;
        let client = &self.deps.client;
        let symbol = &self.deps.symbol;
        let notifier = &self.deps.notifier;
        let qty_tolerance = self.deps.qty_tolerance;
        let run_order_prefix = &self.deps.run_order_prefix;
        let baseline_mark = self.deps.baseline_mark;
        let session_started_at = self.deps.session_started_at;
        let cycle = self.loop_state.counters.cycle;
        let exit = 'phase: {
            if let Some(session) = self.live_session.as_mut() {
                if !session.account_stream_health.is_healthy() {
                    let detail = session.account_stream_health.failure_detail();
                    let message = format!(
                        "account stream unavailable; placements frozen and cleanup starting: {detail}"
                    );
                    let request_timeout = if self
                        .recovery
                        .pending_request_timeout
                        .as_ref()
                        .is_some_and(|timeout| {
                            timeout.phase.recovery_target() == RecoveryTarget::AccountStream
                        }) {
                        self.recovery.pending_request_timeout.take()
                    } else {
                        None
                    };
                    let notice = request_timeout.as_ref().map_or_else(
                        || {
                            FreezeNotice::Risk(
                                RiskNotice::warning(
                                    "account_stream",
                                    "disconnected_frozen",
                                    &message,
                                    symbol,
                                    cycle,
                                )
                                .expected(self.loop_state.ledger.expected_position),
                            )
                        },
                        |timeout| {
                            FreezeNotice::RequestTimeout(request_timeout_notice(
                                timeout,
                                &detail,
                                symbol,
                                cycle,
                                self.loop_state.ledger.expected_position,
                            ))
                        },
                    );
                    // Freeze immediately: no further cycle can place while the
                    // authoritative account stream is unavailable. The stale
                    // receiver/health stay in place until the reconnect replaces
                    // them; every failure path below exits the loop.
                    let recovery_token = match freeze_and_cleanup_for_recovery(
                        &mut RecoveryIo {
                            runtime_state: &mut self.recovery.runtime_state,
                            notifier,
                            client,
                            session: Some(&mut *session),
                            resting: &mut self.loop_state.resting,
                            inventory_exit_pending: &mut self.loop_state.inventory_exit_pending,
                            next_cycle_is_recovery: &mut self.loop_state.next_cycle_is_recovery,
                            symbol,
                            cycle,
                            output_format,
                        },
                        FreezeSpec {
                            target: RecoveryTarget::AccountStream,
                            trigger: MakerEvent::AccountStreamDisconnected(detail.clone()),
                            cleanup_effect_stop: EffectFailureStop::CleanupFailure,
                            recovery_effect_stop: EffectFailureStop::PositionReconciliation,
                            cleanup_failure_prefix: format!(
                                "account stream disconnected ({detail}); "
                            ),
                            cleanup_failed_exit: MakerExit::PositionReconciliation,
                            notice,
                            frozen_note: None,
                            abort_account_stream_handle: true,
                            continuity: OrderResponseContinuity::Preserved,
                            cancel_venue_orders: true,
                            price_decimals: self.deps.cfg.price_decimals,
                        },
                    )
                    .await
                    {
                        Ok(token) => token,
                        Err(exit) => break 'phase exit,
                    };

                    if args.account_stream_reconnect_attempts == 0 {
                        break 'phase recovery_failed_exit(
                            &mut self.recovery.runtime_state,
                            recovery_token,
                            format!("account stream disconnected ({detail}); reconnect disabled"),
                        );
                    }
                    let mut failed_rounds = 0_u32;
                    // The whole account-stream recovery runs as one bounded
                    // retry loop: reconnect, drain buffered events, reconcile
                    // the venue position, and verify the maker book empty. A
                    // *transport* fault at any post-connect step — including the
                    // freshly reauthenticated account stream dropping again
                    // mid-backfill, which is what the 2026-08-10 incident hit —
                    // is retried like a connect failure rather than failing
                    // closed. Only proof-of-safety failures (an unexplained
                    // position gap, a residual maker book, or an event-validation
                    // error that is not the transport disconnect) remain
                    // terminal.
                    'recovery: loop {
                        let (mut events, health, handle) = match reconnect_account_stream(
                            &mut session.account_stream_epoch,
                            args.account_stream_reconnect_attempts,
                            args.account_stream_reconnect_backoff,
                            &mut self.ctrl_c_rx,
                            &self.deps.endpoints,
                        )
                        .await
                        {
                            AccountStreamReconnect::Connected(triple) => triple,
                            AccountStreamReconnect::Interrupted => {
                                self.recovery
                                    .runtime_state
                                    .handle(MakerEvent::StopRequested(RuntimeStopReason::CtrlC));
                                break 'phase take_stop_effect(
                                    &mut self.recovery.runtime_state,
                                    MakerExit::PositionReconciliation,
                                );
                            }
                            AccountStreamReconnect::Terminal(reason) => {
                                break 'phase recovery_failed_exit(
                                    &mut self.recovery.runtime_state,
                                    recovery_token,
                                    format!(
                                        "account stream disconnected ({detail}); reconnect is not retryable: {reason}"
                                    ),
                                );
                            }
                            AccountStreamReconnect::Exhausted(reason) => {
                                if let Err(error) = cancel_maker_orders_with_retry(
                                    client,
                                    symbol,
                                    3,
                                    output_format,
                                    None,
                                )
                                .await
                                {
                                    break 'phase stop_requested_exit(
                                        &mut self.recovery.runtime_state,
                                        RuntimeStopReason::CleanupFailure {
                                            target: RecoveryTarget::AccountStream,
                                            reason: format!(
                                                "account-stream retry cleanup failed: {error}"
                                            ),
                                        },
                                    );
                                }
                                failed_rounds = failed_rounds.saturating_add(1);
                                let delay_secs = maker::recovery_retry_delay_secs(
                                    args.account_stream_reconnect_backoff,
                                    failed_rounds,
                                );
                                let retry_message = format!(
                                    "account stream reconnect round {failed_rounds} exhausted ({reason}); maker book re-verified empty; placements remain frozen; retrying in {delay_secs}s"
                                );
                                notifier
                                    .risk(
                                        RiskNotice::warning(
                                            "account_stream",
                                            "reconnect_retrying",
                                            &retry_message,
                                            symbol,
                                            cycle,
                                        )
                                        .expected(self.loop_state.ledger.expected_position),
                                        false,
                                    )
                                    .await;
                                tokio::select! {
                                    biased;
                                    _ = ctrl_c_latched(&mut self.ctrl_c_rx) => {
                                        self.recovery.runtime_state.handle(
                                            MakerEvent::StopRequested(RuntimeStopReason::CtrlC),
                                        );
                                        break 'phase take_stop_effect(
                                            &mut self.recovery.runtime_state,
                                            MakerExit::PositionReconciliation,
                                        );
                                    }
                                    _ = tokio::time::sleep(Duration::from_secs(delay_secs)) => {}
                                }
                                continue 'recovery;
                            }
                        };

                        let projection = &mut session.projection;
                        projection.reset_after_cleanup_preserving_pending_acks(
                            session.account_stream_epoch,
                            self.loop_state.ledger.expected_position,
                        );

                        macro_rules! retry_transport {
                            () => {{
                                failed_rounds = failed_rounds.saturating_add(1);
                                let delay_secs = maker::recovery_retry_delay_secs(
                                    args.account_stream_reconnect_backoff,
                                    failed_rounds,
                                );
                                let retry_message = format!(
                                    "account stream reconnect round {failed_rounds} dropped mid-backfill; maker book re-verified empty; placements remain frozen; retrying in {delay_secs}s"
                                );
                                notifier
                                    .risk(
                                        RiskNotice::warning(
                                            "account_stream",
                                            "reconnect_retrying",
                                            &retry_message,
                                            symbol,
                                            cycle,
                                        )
                                        .expected(self.loop_state.ledger.expected_position),
                                        false,
                                    )
                                    .await;
                                tokio::select! {
                                    biased;
                                    _ = ctrl_c_latched(&mut self.ctrl_c_rx) => {
                                        self.recovery.runtime_state.handle(
                                            MakerEvent::StopRequested(RuntimeStopReason::CtrlC),
                                        );
                                        break 'phase take_stop_effect(
                                            &mut self.recovery.runtime_state,
                                            MakerExit::PositionReconciliation,
                                        );
                                    }
                                    _ = tokio::time::sleep(Duration::from_secs(delay_secs)) => {}
                                }
                                continue 'recovery;
                            }};
                        }

                        let reconnect_outcome = match apply_account_events(
                            &mut events,
                            &mut AccountEventState {
                                ledger: &mut self.loop_state.ledger,
                                stats: &mut self.loop_state.stats,
                                projection,
                            },
                            &AccountEventContext {
                                symbol,
                                run_order_prefix,
                                mark: self.market.last_mark.unwrap_or(baseline_mark),
                                cycle,
                                output_format,
                                excess_bps_at_fill: self
                                    .loop_state
                                    .external_excess_telemetry
                                    .current(std::time::Instant::now()),
                            },
                        ) {
                            Ok(outcome) => outcome,
                            Err(error)
                                if error.downcast_ref::<AccountStreamDisconnected>().is_some() =>
                            {
                                handle.abort();
                                notifier
                                    .risk(
                                        RiskNotice::warning(
                                            "account_stream",
                                            "reconnect_retrying",
                                            "account stream reconnect drained then dropped; retrying",
                                            symbol,
                                            cycle,
                                        )
                                        .expected(self.loop_state.ledger.expected_position),
                                        false,
                                    )
                                    .await;
                                retry_transport!();
                            }
                            Err(error) => {
                                handle.abort();
                                break 'phase recovery_failed_exit(
                                    &mut self.recovery.runtime_state,
                                    recovery_token,
                                    format!(
                                        "account stream reconnect event validation failed: {error}"
                                    ),
                                );
                            }
                        };
                        self.loop_state.account_balance_refresh_requested |=
                            reconnect_outcome.balance_changed;
                        let mut reconnect_fills = reconnect_outcome.fills;
                        let positions = match client.get_positions(Some(symbol)).await {
                            Ok(positions) => positions,
                            Err(error) => {
                                handle.abort();
                                break 'phase recovery_failed_exit(
                                    &mut self.recovery.runtime_state,
                                    recovery_token,
                                    format!("account stream reconnect snapshot failed: {error}"),
                                );
                            }
                        };
                        let mut observed = match position_for_symbol(&positions, symbol) {
                            Ok(position) => position,
                            Err(error) => {
                                handle.abort();
                                break 'phase recovery_failed_exit(
                                    &mut self.recovery.runtime_state,
                                    recovery_token,
                                    error.to_string(),
                                );
                            }
                        };

                        if (observed - self.loop_state.ledger.expected_position).abs()
                            > qty_tolerance
                        {
                            // WS events can lag REST settlement across a reconnect: give
                            // a bounded window to explain the gap with REST trades
                            // (mirrors the in-cycle freeze-path reconciliation) before
                            // failing closed.
                            let mut gap_closed = false;
                            for delay in [500_u64, 1_000, 1_500] {
                                // The maker book is verified empty at this point, so
                                // aborting the convergence wait on Ctrl+C is safe.
                                tokio::select! {
                                    _ = ctrl_c_latched(&mut self.ctrl_c_rx) => {
                                        handle.abort();
                                        break 'phase stop_requested_exit(
                                            &mut self.recovery.runtime_state,
                                            RuntimeStopReason::CtrlC,
                                        );
                                    }
                                    _ = tokio::time::sleep(Duration::from_millis(delay)) => {}
                                }
                                match apply_account_events(
                                    &mut events,
                                    &mut AccountEventState {
                                        ledger: &mut self.loop_state.ledger,
                                        stats: &mut self.loop_state.stats,
                                        projection,
                                    },
                                    &AccountEventContext {
                                        symbol,
                                        run_order_prefix,
                                        mark: self.market.last_mark.unwrap_or(baseline_mark),
                                        cycle,
                                        output_format,
                                        excess_bps_at_fill: self
                                            .loop_state
                                            .external_excess_telemetry
                                            .current(std::time::Instant::now()),
                                    },
                                ) {
                                    Ok(outcome) => {
                                        reconnect_fills += outcome.fills;
                                        self.loop_state.account_balance_refresh_requested |=
                                            outcome.balance_changed;
                                    }
                                    Err(error)
                                        if error
                                            .downcast_ref::<AccountStreamDisconnected>()
                                            .is_some() =>
                                    {
                                        handle.abort();
                                        retry_transport!();
                                    }
                                    Err(error) => {
                                        handle.abort();
                                        break 'phase recovery_failed_exit(
                                            &mut self.recovery.runtime_state,
                                            recovery_token,
                                            format!(
                                                "account stream reconnect event validation failed during REST backfill: {error}"
                                            ),
                                        );
                                    }
                                }
                                match probe_position_convergence(
                                    client,
                                    ReconcileRequest {
                                        symbol,
                                        session_started_at,
                                        run_order_prefix,
                                        qty_tolerance,
                                        mark: self.market.last_mark.unwrap_or(baseline_mark),
                                    },
                                    &mut self.loop_state.ledger,
                                    &mut self.loop_state.stats,
                                    &mut reconnect_fills,
                                    FillEmissionContext {
                                        cycle,
                                        output_format,
                                        excess_bps_at_fill: self
                                            .loop_state
                                            .external_excess_telemetry
                                            .current(std::time::Instant::now()),
                                    },
                                )
                                .await
                                {
                                    ConvergenceProbe::Converged { observed: obs } => {
                                        observed = obs;
                                        gap_closed = true;
                                        break;
                                    }
                                    ConvergenceProbe::Pending { observed: obs } => observed = obs,
                                    ConvergenceProbe::SnapshotFailed(error) => eprintln!(
                                        "⚠️  account stream reconnect REST trade backfill failed: {error}"
                                    ),
                                }
                            }
                            if !gap_closed {
                                handle.abort();
                                break 'phase recovery_failed_exit(
                                    &mut self.recovery.runtime_state,
                                    recovery_token,
                                    format!(
                                        "account stream reconnect snapshot expected {:+.8}, observed {:+.8} (REST trade backfill did not close the gap)",
                                        self.loop_state.ledger.expected_position, observed
                                    ),
                                );
                            }
                        }

                        // A current-run order may surface after the first freeze
                        // cleanup while the account stream is authenticating. Require
                        // one final authoritative empty-book verification before the
                        // recovered stream can resume quoting.
                        if let Err(error) =
                            cancel_maker_orders_with_retry(client, symbol, 3, output_format, None)
                                .await
                        {
                            handle.abort();
                            break 'phase recovery_failed_exit(
                                &mut self.recovery.runtime_state,
                                recovery_token,
                                format!(
                                    "account stream reconnect final maker-book verification failed: {error}"
                                ),
                            );
                        }
                        session.account_events = events;
                        session.account_stream_health = health;
                        session.account_stream_handle = handle;
                        self.loop_state.counters.total_fills += reconnect_fills;
                        resume_quoting_after_recovery(
                            &mut RecoveryIo {
                                runtime_state: &mut self.recovery.runtime_state,
                                notifier,
                                client,
                                session: Some(&mut *session),
                                resting: &mut self.loop_state.resting,
                                inventory_exit_pending: &mut self.loop_state.inventory_exit_pending,
                                next_cycle_is_recovery: &mut self.loop_state.next_cycle_is_recovery,
                                symbol,
                                cycle,
                                output_format,
                            },
                            ResumeSpec {
                                recovery_token,
                                observed,
                                continuity: OrderResponseContinuity::Preserved,
                                clear_resting: false,
                                recovered_note: None,
                                notice: RiskNotice::resolved("account_stream", "reconnected", "account stream reauthenticated; buffered events and REST trades reconciled against the venue position", symbol, cycle).expected(self.loop_state.ledger.expected_position).observed(observed),
                            },
                        )
                        .await;
                        break 'recovery;
                    }
                    return LoopDirective::Restart;
                }
            }
            return LoopDirective::Proceed;
        };
        LoopDirective::Exit(exit)
    }

    async fn recover_order_response_phase(&mut self) -> LoopDirective {
        let args = &self.deps.args;
        let output_format = self.deps.output_format;
        let client = &self.deps.client;
        let symbol = &self.deps.symbol;
        let notifier = &self.deps.notifier;
        let qty_tolerance = self.deps.qty_tolerance;
        let run_order_prefix = &self.deps.run_order_prefix;
        let baseline_mark = self.deps.baseline_mark;
        let session_started_at = self.deps.session_started_at;
        let cycle = self.loop_state.counters.cycle;
        let exit = 'phase: {
            if let Some(session) = self.live_session.as_mut() {
                if !session.order_response_health.is_healthy() {
                    let detail = session.order_response_health.failure_detail();
                    let controlled_fault = detail.starts_with("controlled fault injection");
                    // Mirror the account-stream path: the order-response stream was
                    // previously silent on the webhook across disconnect/reconnect.
                    let disconnect_message =
                        format!("order-response stream unavailable; placements frozen: {detail}");
                    let request_timeout = if self
                        .recovery
                        .pending_request_timeout
                        .as_ref()
                        .is_some_and(|timeout| {
                            timeout.phase.recovery_target() == RecoveryTarget::OrderResponse
                        }) {
                        self.recovery.pending_request_timeout.take()
                    } else {
                        None
                    };
                    let notice = request_timeout.as_ref().map_or_else(
                        || {
                            FreezeNotice::Risk(
                                RiskNotice::warning(
                                    "order_response",
                                    "disconnected_frozen",
                                    &disconnect_message,
                                    symbol,
                                    cycle,
                                )
                                .expected(self.loop_state.ledger.expected_position),
                            )
                        },
                        |timeout| {
                            FreezeNotice::RequestTimeout(request_timeout_notice(
                                timeout,
                                &detail,
                                symbol,
                                cycle,
                                self.loop_state.ledger.expected_position,
                            ))
                        },
                    );
                    let recovery_token = match freeze_and_cleanup_for_recovery(
                        &mut RecoveryIo {
                            runtime_state: &mut self.recovery.runtime_state,
                            notifier,
                            client,
                            session: Some(&mut *session),
                            resting: &mut self.loop_state.resting,
                            inventory_exit_pending: &mut self.loop_state.inventory_exit_pending,
                            next_cycle_is_recovery: &mut self.loop_state.next_cycle_is_recovery,
                            symbol,
                            cycle,
                            output_format,
                        },
                        FreezeSpec {
                            target: RecoveryTarget::OrderResponse,
                            trigger: MakerEvent::OrderResponseDisconnected(detail.clone()),
                            cleanup_effect_stop: EffectFailureStop::OrderResponse,
                            recovery_effect_stop: EffectFailureStop::OrderResponse,
                            cleanup_failure_prefix: "order-response ".to_string(),
                            cleanup_failed_exit: MakerExit::OrderResponse,
                            notice,
                            frozen_note: None,
                            abort_account_stream_handle: false,
                            continuity: OrderResponseContinuity::Replaced,
                            cancel_venue_orders: true,
                            price_decimals: self.deps.cfg.price_decimals,
                        },
                    )
                    .await
                    {
                        Ok(token) => token,
                        Err(exit) => break 'phase exit,
                    };
                    let reconnect_unavailable = if controlled_fault {
                        Some("controlled fault injection requires fail-safe shutdown".to_string())
                    } else if args.order_response_reconnect_attempts == 0 {
                        Some("safe reconnect is disabled".to_string())
                    } else {
                        None
                    };
                    if reconnect_unavailable.is_none() {
                        // Abort the stream task in place; the stale halves are
                        // replaced together on success. Retryable transport
                        // exhaustion leaves the core recovery token in flight,
                        // so the maker stays frozen until a later round succeeds.
                        session.order_response_handle.abort();
                        let mut failed_rounds = 0_u32;
                        let mut recovered_fills = Vec::new();
                        let reconnected = loop {
                            match reconnect_order_response(
                                ReconnectRequest {
                                    cleanup_client: client.clone(),
                                    symbol,
                                    session_started_at,
                                    run_order_prefix,
                                    qty_tolerance,
                                    mark: self.market.last_mark.unwrap_or(baseline_mark),
                                    output_format,
                                    max_attempts: args.order_response_reconnect_attempts,
                                    base_backoff: Duration::from_secs(
                                        args.order_response_reconnect_backoff,
                                    ),
                                    original_failure: &detail,
                                    ctrl_c: self.ctrl_c_rx.clone(),
                                    endpoints: &self.deps.endpoints,
                                },
                                &mut self.loop_state.ledger,
                                &mut self.loop_state.stats,
                                &mut recovered_fills,
                            )
                            .await
                            {
                                Ok(reconnected) => break reconnected,
                                Err(error) => {
                                    if error.downcast_ref::<ReconnectInterrupted>().is_some() {
                                        self.recovery.runtime_state.handle(
                                            MakerEvent::StopRequested(RuntimeStopReason::CtrlC),
                                        );
                                        break 'phase take_stop_effect(
                                            &mut self.recovery.runtime_state,
                                            MakerExit::OrderResponse,
                                        );
                                    }
                                    if let Some(reconciliation) =
                                        error.downcast_ref::<PositionReconciliationError>()
                                    {
                                        if output_format == OutputFormat::Json {
                                            println!(
                                                "{}",
                                                serde_json::json!({
                                                    "ts": ts_now(),
                                                    "symbol": symbol,
                                                    "action": "position_reconciliation",
                                                    "event": "failed_during_reconnect",
                                                    "expected_position": reconciliation.expected,
                                                    "observed_position": reconciliation.observed,
                                                    "message": "post-reconnect venue position cannot be explained by current-run maker fills",
                                                })
                                            );
                                        }
                                        let reconciliation_message = format!(
                                            "order-response reconnect failed reconciliation: {error}"
                                        );
                                        notifier
                                            .risk(
                                                RiskNotice::critical(
                                                    "order_response",
                                                    "reconnect_failed",
                                                    &reconciliation_message,
                                                    symbol,
                                                    cycle,
                                                )
                                                .expected(self.loop_state.ledger.expected_position),
                                                true,
                                            )
                                            .await;
                                        self.recovery.runtime_state.handle(
                                            MakerEvent::RecoveryFailed {
                                                token: recovery_token,
                                                reason: error.to_string(),
                                            },
                                        );
                                        break 'phase take_stop_effect(
                                            &mut self.recovery.runtime_state,
                                            MakerExit::PositionReconciliation,
                                        );
                                    }
                                    if let Some(cleanup) =
                                        error.downcast_ref::<ReconnectCleanupFailed>()
                                    {
                                        break 'phase stop_requested_exit(
                                            &mut self.recovery.runtime_state,
                                            RuntimeStopReason::CleanupFailure {
                                                target: RecoveryTarget::OrderResponse,
                                                reason: cleanup.to_string(),
                                            },
                                        );
                                    }
                                    if error
                                        .downcast_ref::<TransportReconnectExhausted>()
                                        .is_some()
                                    {
                                        if let Err(cleanup_error) = cancel_maker_orders_with_retry(
                                            client,
                                            symbol,
                                            3,
                                            output_format,
                                            None,
                                        )
                                        .await
                                        {
                                            let reason = format!(
                                                "order-response retry cleanup failed: {cleanup_error}"
                                            );
                                            break 'phase stop_requested_exit(
                                                &mut self.recovery.runtime_state,
                                                RuntimeStopReason::CleanupFailure {
                                                    target: RecoveryTarget::OrderResponse,
                                                    reason,
                                                },
                                            );
                                        }
                                        failed_rounds = failed_rounds.saturating_add(1);
                                        let delay_secs = maker::recovery_retry_delay_secs(
                                            args.order_response_reconnect_backoff,
                                            failed_rounds,
                                        );
                                        let retry_message = format!(
                                            concat!(
                                                "order-response reconnect round {} exhausted ({}); ",
                                                "maker book re-verified empty; placements remain frozen; ",
                                                "retrying in {}s"
                                            ),
                                            failed_rounds, error, delay_secs,
                                        );
                                        notifier
                                            .risk(
                                                RiskNotice::warning(
                                                    "order_response",
                                                    "reconnect_retrying",
                                                    &retry_message,
                                                    symbol,
                                                    cycle,
                                                )
                                                .expected(self.loop_state.ledger.expected_position),
                                                false,
                                            )
                                            .await;
                                        tokio::select! {
                                            biased;
                                            _ = ctrl_c_latched(&mut self.ctrl_c_rx) => {
                                                self.recovery.runtime_state.handle(
                                                    MakerEvent::StopRequested(RuntimeStopReason::CtrlC),
                                                );
                                                break 'phase take_stop_effect(
                                                    &mut self.recovery.runtime_state,
                                                    MakerExit::OrderResponse,
                                                );
                                            }
                                            _ = tokio::time::sleep(Duration::from_secs(delay_secs)) => {}
                                        }
                                        continue;
                                    }
                                    let reconnect_failed_message = format!(
                                        "{detail}; safe reconnect is not retryable: {error}; refusing further live orders"
                                    );
                                    notifier
                                        .risk(
                                            RiskNotice::critical(
                                                "order_response",
                                                "reconnect_failed",
                                                &reconnect_failed_message,
                                                symbol,
                                                cycle,
                                            )
                                            .expected(self.loop_state.ledger.expected_position),
                                            true,
                                        )
                                        .await;
                                    self.recovery.runtime_state.handle(
                                        MakerEvent::RecoveryFailed {
                                            token: recovery_token,
                                            reason: reconnect_failed_message,
                                        },
                                    );
                                    break 'phase take_stop_effect(
                                        &mut self.recovery.runtime_state,
                                        MakerExit::OrderResponse,
                                    );
                                }
                            }
                        };

                        self.loop_state.counters.total_fills += reconnected.fills.len() as u64;
                        for fill in &reconnected.fills {
                            emit_live_fill(
                                fill,
                                symbol,
                                cycle,
                                output_format,
                                self.loop_state
                                    .external_excess_telemetry
                                    .current(std::time::Instant::now()),
                            );
                            if let Some(order_id) = fill.order_id {
                                let at_ms =
                                    u64::try_from(session.latency_started.elapsed().as_millis())
                                        .unwrap_or(u64::MAX);
                                if let Err(error) = session
                                    .order_latency
                                    .record_fill_after_cancel_order(order_id, at_ms)
                                {
                                    eprintln!(
                                        "⚠️ fill-after-cancel observation unavailable: {error}"
                                    );
                                }
                            }
                        }
                        self.loop_state.account_balance_refresh_requested |=
                            !reconnected.fills.is_empty();
                        let reconciled_position = reconnected.position;
                        session.order_commands = reconnected.commands;
                        session.order_responses = reconnected.responses;
                        session.order_response_health = reconnected.health;
                        session.order_response_handle = reconnected.handle;
                        // The new channel never delivers acks for cleanup
                        // cancels minted on the old one.
                        session.cleanup_minted_request_ids.clear();
                        // Cleanup verified an empty maker book. The next cycle
                        // rebuilds exchange state before it may place.
                        resume_quoting_after_recovery(
                            &mut RecoveryIo {
                                runtime_state: &mut self.recovery.runtime_state,
                                notifier,
                                client,
                                session: Some(&mut *session),
                                resting: &mut self.loop_state.resting,
                                inventory_exit_pending: &mut self.loop_state.inventory_exit_pending,
                                next_cycle_is_recovery: &mut self.loop_state.next_cycle_is_recovery,
                                symbol,
                                cycle,
                                output_format,
                            },
                            ResumeSpec {
                                recovery_token,
                                observed: reconciled_position,
                                continuity: OrderResponseContinuity::Replaced,
                                clear_resting: true,
                                recovered_note: None,
                                notice: RiskNotice::resolved("order_response", "reconnected", "order-response stream reconnected; maker book verified empty before quoting resumes", symbol, cycle).expected(self.loop_state.ledger.expected_position).observed(reconciled_position),
                            },
                        )
                        .await;
                        return LoopDirective::Restart;
                    }
                    let reconnect_note = reconnect_unavailable
                        .expect("unavailable reconnect must carry a fail-safe reason");
                    let refuse_message =
                        format!("{detail}; {reconnect_note}; refusing further live orders");
                    notifier
                        .risk(
                            RiskNotice::critical(
                                "order_response",
                                "reconnect_unavailable",
                                &refuse_message,
                                symbol,
                                cycle,
                            )
                            .expected(self.loop_state.ledger.expected_position),
                            true,
                        )
                        .await;
                    self.recovery
                        .runtime_state
                        .handle(MakerEvent::RecoveryFailed {
                            token: recovery_token,
                            reason: refuse_message,
                        });
                    break 'phase take_stop_effect(
                        &mut self.recovery.runtime_state,
                        MakerExit::OrderResponse,
                    );
                }
            }
            return LoopDirective::Proceed;
        };
        LoopDirective::Exit(exit)
    }

    async fn drain_live_events_phase(&mut self) -> LoopDirective {
        let args = &self.deps.args;
        let output_format = self.deps.output_format;
        let cfg = &self.deps.cfg;
        let symbol = &self.deps.symbol;
        let notifier = &self.deps.notifier;
        let qty_tolerance = self.deps.qty_tolerance;
        let run_order_prefix = &self.deps.run_order_prefix;
        let baseline_mark = self.deps.baseline_mark;
        let cycle = self.loop_state.counters.cycle;
        if let Some(session) = self.live_session.as_mut() {
            if let Err(error) = apply_order_responses_observed(
                &mut session.order_responses,
                &mut session.projection,
                &mut self.recovery.runtime_state,
                OrderResponseObservation {
                    output_format,
                    symbol,
                    cycle,
                    price_decimals: cfg.price_decimals,
                    latency: Some(&mut session.order_latency),
                    latency_started: Some(session.latency_started),
                },
                &session.cleanup_minted_request_ids,
            ) {
                session
                    .order_response_health
                    .mark_unhealthy(error.to_string());
                return LoopDirective::Restart;
            }
            match apply_account_events(
                &mut session.account_events,
                &mut AccountEventState {
                    ledger: &mut self.loop_state.ledger,
                    stats: &mut self.loop_state.stats,
                    projection: &mut session.projection,
                },
                &AccountEventContext {
                    symbol,
                    run_order_prefix,
                    mark: self.market.last_mark.unwrap_or(baseline_mark),
                    cycle,
                    output_format,
                    excess_bps_at_fill: self
                        .loop_state
                        .external_excess_telemetry
                        .current(std::time::Instant::now()),
                },
            ) {
                Ok(outcome) => {
                    self.recovery.account_order_reconciliation_required |=
                        outcome.requires_order_reconciliation;
                    let position = absorb_account_outcome(
                        outcome,
                        OutcomeSink {
                            total_fills: &mut self.loop_state.counters.total_fills,
                            balance_refresh_requested: &mut self
                                .loop_state
                                .account_balance_refresh_requested,
                            inventory_exit_pending: &mut self.loop_state.inventory_exit_pending,
                            notifier,
                            position_alert_anchor: &mut self.loop_state.position_alert_anchor,
                            expected_position: self.loop_state.ledger.expected_position,
                            max_position: cfg.max_position,
                            inventory_exit_pct: args.inventory_exit_pct,
                            qty_tolerance,
                            symbol,
                            cycle,
                            order_latency: Some(&mut session.order_latency),
                            latency_started: Some(session.latency_started),
                        },
                    )
                    .await;
                    if let Some(position) = position.filter(|position| {
                        (*position - self.loop_state.ledger.expected_position).abs() > qty_tolerance
                    }) {
                        self.recovery.account_position_mismatch = Some(position);
                    }
                }
                Err(error) => {
                    session
                        .account_stream_health
                        .mark_unhealthy(error.to_string());
                    return LoopDirective::Restart;
                }
            }
            schedule_account_balance_refresh(
                &mut self.loop_state.account_balance_refresh_requested,
                account_risk_watch_enabled(args, &self.loop_state.alerts),
                &mut session.account_poll,
                std::time::Instant::now(),
            );
            session
                .order_request_deadlines
                .retain_pending(&session.projection);
            if session.order_response_health.is_healthy()
                && session.account_stream_health.is_healthy()
            {
                if let Some(timeout) = session.order_request_deadlines.timed_out(
                    &session.projection,
                    std::time::Instant::now(),
                    ORDER_REQUEST_TIMEOUT,
                ) {
                    let detail = order_request_timeout_detail(&timeout);
                    let timeout_at_ms = duration_ms(session.latency_started.elapsed());
                    if let Err(error) = session.order_latency.mark_timeout_phase(
                        &timeout.request_id,
                        timeout.phase,
                        timeout_at_ms,
                    ) {
                        eprintln!(
                                "warning: failed to record order request timeout latency for {}: {error}",
                                timeout.request_id
                            );
                    }
                    mark_request_timeout_stream_unhealthy(
                        &session.account_stream_health,
                        &session.order_response_health,
                        &timeout,
                        &detail,
                    );
                    self.recovery.pending_request_timeout = Some(timeout);
                    return LoopDirective::Restart;
                }
            }
        }
        LoopDirective::Proceed
    }

    async fn accounting_invariant_phase(&mut self) -> LoopDirective {
        let args = &self.deps.args;
        let symbol = &self.deps.symbol;
        let notifier = &self.deps.notifier;
        let qty_tolerance = self.deps.qty_tolerance;
        let cycle = self.loop_state.counters.cycle;
        let exit = 'phase: {
            if self
                .recovery
                .account_position_mismatch
                .is_some_and(|position| {
                    (position - self.loop_state.ledger.expected_position).abs() <= qty_tolerance
                })
            {
                self.recovery.account_position_mismatch = None;
            }
            if args.live {
                if let Some(detail) = accounting_invariant_exit(
                    notifier,
                    symbol,
                    cycle,
                    self.loop_state.ledger.expected_position,
                    self.loop_state.stats.position(),
                    qty_tolerance,
                )
                .await
                {
                    break 'phase stop_requested_exit(
                        &mut self.recovery.runtime_state,
                        RuntimeStopReason::AccountingInvariant(detail),
                    );
                }
            }
            return LoopDirective::Proceed;
        };
        LoopDirective::Exit(exit)
    }
}
