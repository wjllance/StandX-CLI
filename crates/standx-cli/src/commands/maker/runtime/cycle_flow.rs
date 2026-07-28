use super::*;

pub(super) struct CycleAttempt {
    work_token: WorkToken,
    exit_pending_before: bool,
    breaker_halted_before: bool,
    result: anyhow::Result<CycleSuccess>,
}

struct CycleSuccess {
    places: u64,
    cancels: u64,
    holds: u64,
    fills: u64,
    mark: f64,
    src: &'static str,
    market_fallback_reason: Option<&'static str>,
    halted: bool,
    exit_pending_after: bool,
    balance: Option<standx_sdk::models::Balance>,
}

pub(super) fn take_cycle_work(
    runtime_state: &mut MakerState,
) -> std::result::Result<Option<WorkToken>, MakerExit> {
    if runtime_state.pending_effect().is_none() {
        runtime_state.handle(MakerEvent::Timer);
    }
    match runtime_state.next_effect() {
        Some(MakerEffect::RunCycle(token)) => Ok(Some(token)),
        Some(MakerEffect::Stop(reason)) => Err(reason.into()),
        Some(effect) => Err(stop_requested_exit(
            runtime_state,
            RuntimeStopReason::PositionReconciliation(format!(
                "runtime emitted unexpected effect before cycle: {effect:?}"
            )),
        )),
        None => Ok(None),
    }
}

pub(super) fn commit_cycle_effect(runtime_state: &mut MakerState, token: WorkToken) -> bool {
    runtime_state.handle(MakerEvent::CycleCompleted(token));
    matches!(
        runtime_state.next_effect(),
        Some(MakerEffect::CommitCycle(committed)) if committed == token
    )
}

impl MakerRuntime {
    pub(super) async fn drive(mut self) -> (Self, MakerExit) {
        let exit = 'main: loop {
            match self.pre_cycle_phase().await {
                LoopDirective::Proceed => {}
                LoopDirective::Restart => continue 'main,
                LoopDirective::Exit(exit) => break exit,
            }
            match self.run_cycle_phase().await {
                LoopDirective::Proceed => {}
                LoopDirective::Restart => continue 'main,
                LoopDirective::Exit(exit) => break exit,
            }
            match self.wait_phase().await {
                LoopDirective::Proceed | LoopDirective::Restart => continue 'main,
                LoopDirective::Exit(exit) => break exit,
            }
        };
        (self, exit)
    }

    async fn run_cycle_phase(&mut self) -> LoopDirective {
        let attempt = match self.execute_cycle().await {
            Ok(attempt) => attempt,
            Err(directive) => return directive,
        };
        self.finish_cycle(attempt).await
    }

    async fn execute_cycle(&mut self) -> std::result::Result<CycleAttempt, LoopDirective> {
        let args = &self.deps.args;
        let output_format = self.deps.output_format;
        let client = &self.deps.client;
        let cfg = &self.deps.cfg;
        let symbol = &self.deps.symbol;
        let notifier = &self.deps.notifier;
        let qty_tolerance = self.deps.qty_tolerance;
        let run_order_prefix = &self.deps.run_order_prefix;
        let starting_position = self.deps.starting_position;
        let baseline_mark = self.deps.baseline_mark;
        let session_started_at = self.deps.session_started_at;
        let cycle = self.loop_state.counters.cycle;
        let performance_started = self.loop_state.performance_started;
        let performance_epoch_ms = self.loop_state.performance_epoch_ms;
        let market_data_health_started = self.market.health_started;
        let feed = &self.market.feed;
        let exit = 'execute: {
            // Work phase raced against Ctrl+C so a slow API call can be
            // interrupted (mirrors run_watch_loop).
            let mismatch = self.recovery.account_position_mismatch.take();
            let order_reconciliation_required =
                std::mem::take(&mut self.recovery.account_order_reconciliation_required);
            let exit_pending_before = self.loop_state.inventory_exit_pending;
            let breaker_halted_before = self.loop_state.breaker.halted();
            let recovery_cycle = self.loop_state.next_cycle_is_recovery;
            let market_paused_before_cycle = self.market.health.is_degraded();
            // Latch a supervisor wind-down request (SIGUSR1). Once set, the
            // planner stops quoting and flattens via reduce-only exits.
            if !self.loop_state.wind_down && *self.wind_down_rx.borrow_and_update() {
                self.loop_state.wind_down = true;
                notifier
                    .lifecycle(
                        "wind_down",
                        "wind-down requested: quoting stopped, flattening via reduce-only exits",
                        symbol,
                        false,
                    )
                    .await;
            }
            let cycle_work_token = match take_cycle_work(&mut self.recovery.runtime_state) {
                Ok(Some(token)) => token,
                Ok(None) => return Err(LoopDirective::Restart),
                Err(exit) => break 'execute exit,
            };
            // Split the live session into disjoint field borrows once per cycle:
            // the pinned `work` future holds the command/health/projection/poll
            // halves for its whole lifetime while the select loop below drains
            // both receivers concurrently, so a plain `live_session.as_mut()` in
            // each place would alias.
            let (
                mut cycle_order_responses,
                mut cycle_account_events,
                cycle_order_commands,
                cycle_order_response_health,
                cycle_account_stream_health,
                mut cycle_projection,
                mut cycle_order_request_deadlines,
                mut cycle_account_poll,
                mut cycle_order_latency,
                cycle_latency_started,
            ) = match self.live_session.as_mut() {
                Some(LiveSession {
                    order_responses,
                    order_commands,
                    order_response_health,
                    account_events,
                    account_stream_health,
                    projection,
                    order_request_deadlines,
                    account_poll,
                    order_latency,
                    latency_started,
                    ..
                }) => (
                    Some(order_responses),
                    Some(account_events),
                    Some(&*order_commands),
                    Some(&*order_response_health),
                    Some(&*account_stream_health),
                    Some(projection),
                    Some(order_request_deadlines),
                    Some(account_poll),
                    Some(order_latency),
                    Some(*latency_started),
                ),
                None => (None, None, None, None, None, None, None, None, None, None),
            };
            let work = async {
                if let Some(observed) = mismatch {
                    return Err(anyhow::Error::new(
                        PositionReconciliationError::position_mismatch(
                            self.loop_state.ledger.expected_position,
                            observed,
                        ),
                    ));
                }
                if order_reconciliation_required {
                    return Err(anyhow::Error::new(
                        PositionReconciliationError::unknown_current_run_order(
                            self.loop_state.ledger.expected_position,
                        ),
                    ));
                }
                let market = market_snapshot(client, symbol, feed.as_ref()).await?;
                let mark = market.mark;
                let best_bid = market.best_bid;
                let best_ask = market.best_ask;
                let src = market.source;
                let market_fallback_reason = market.fallback_reason;
                if feed.is_some() && !self.market.health.is_degraded() {
                    let health_now_ms = duration_ms(market_data_health_started.elapsed());
                    let update = observe_acquired_market_health(
                        &mut self.market.health,
                        AcquiredMarketHealth {
                            now_ms: health_now_ms,
                            source: src,
                            fallback_reason: market_fallback_reason,
                            mark,
                            best_bid,
                            best_ask,
                            max_divergence_bps: args.max_divergence_bps,
                        },
                    );
                    self.market.last_divergence_bps = update.divergence_bps;
                    if let Some(detail) = update.degradation_detail() {
                        return Err(anyhow::Error::new(MarketDataDegradedError { detail }));
                    }
                }
                let market_data_mode = if self.market.health.is_degraded() {
                    maker::MarketDataMode::Paused
                } else {
                    maker::MarketDataMode::Active
                };
                // Normalize the leader feed against THIS cycle's mark. A
                // missing/stale sample yields None and the guard fails open.
                let cycle_external_divergence = match self.loop_state.external_feed.as_ref() {
                    Some(feed) => {
                        let state = *feed.read().await;
                        super::super::external_feed::divergence_input(
                            state,
                            mark,
                            std::time::Instant::now(),
                            &mut self.loop_state.external_basis,
                        )
                    }
                    None => None,
                };
                let cycle_external_basis_bps = self.loop_state.external_basis.basis_bps();
                let result = maker_cycle(
                    CycleRequest {
                        client,
                        symbol,
                        cfg,
                        live: args.live,
                        cycle,
                        mark,
                        best_bid,
                        best_ask,
                        market_data_mode,
                        market_source: src,
                        recovery: recovery_cycle,
                        market_fallback_reason,
                        ws_snapshot: market.ws_snapshot.as_ref(),
                        max_divergence_bps: args.max_divergence_bps,
                        inventory_exit_pct: args.inventory_exit_pct,
                        inventory_exit_qty: args.inventory_exit_qty,
                        stop_equity_below: args.stop_equity_below,
                        stop_margin_below: args.stop_margin_below,
                        wind_down: self.loop_state.wind_down,
                        qty_tolerance,
                        session_started_at,
                        run_order_prefix,
                        starting_position,
                        output_format,
                        order_commands: cycle_order_commands,
                        order_response_health: cycle_order_response_health,
                        account_stream_health: cycle_account_stream_health,
                        performance_time_ms: performance_epoch_ms.saturating_add(
                            i64::try_from(performance_started.elapsed().as_millis())
                                .unwrap_or(i64::MAX),
                        ),
                    },
                    CycleState {
                        resting: &mut self.loop_state.resting,
                        account_projection: cycle_projection.as_deref_mut(),
                        inventory_exit_pending: &mut self.loop_state.inventory_exit_pending,
                        ledger: &mut self.loop_state.ledger,
                        sim_position: &mut self.loop_state.sim_position,
                        stats: &mut self.loop_state.stats,
                        breaker: &mut self.loop_state.breaker,
                        spread_controller: &mut self.loop_state.spread_controller,
                        size_skew_controller: &mut self.loop_state.size_skew_controller,
                        nonlinear_skew: self.loop_state.nonlinear_skew,
                        guard_controller: &mut self.loop_state.guard_controller,
                        external_divergence: cycle_external_divergence,
                        external_basis_bps: cycle_external_basis_bps,
                        order_request_deadlines: cycle_order_request_deadlines.as_deref_mut(),
                        live_account_poll: cycle_account_poll.as_deref_mut(),
                        order_latency: cycle_order_latency.as_deref_mut(),
                        latency_started: cycle_latency_started,
                    },
                )
                .await?;
                Ok::<_, anyhow::Error>(CycleSuccess {
                    places: result.places,
                    cancels: result.cancels,
                    holds: result.holds,
                    fills: result.fills,
                    mark,
                    src,
                    market_fallback_reason,
                    halted: self.loop_state.breaker.halted(),
                    exit_pending_after: self.loop_state.inventory_exit_pending,
                    balance: result.balance,
                })
            };
            // Order lifecycle and balance events are buffered so a cycle's own
            // acknowledgement cannot tear apart a multi-order plan. Position,
            // trade, and stream-failure events can change risk or invalidate the
            // plan. They freeze the reducer before this future is dropped so the
            // queued Cleanup effect compensates for any request that may already
            // have reached the venue.
            let mut buffered_account: Vec<AccountEvent> = Vec::new();
            let mut buffered_orders: Vec<OrderResponse> = Vec::new();
            let mut cycle_invalidated_by_account = false;
            let mut cycle_invalidated_by_market: Option<String> = None;
            // Scope the pinned work future so it (and its ledger/pending borrows)
            // is dropped once it resolves, before the buffered events are applied.
            let cycle_result = {
                tokio::pin!(work);
                loop {
                    let account_during_work = async {
                        match cycle_account_events.as_deref_mut() {
                            Some(receiver) => receiver.recv().await,
                            None => std::future::pending().await,
                        }
                    };
                    let order_during_work = async {
                        match cycle_order_responses.as_deref_mut() {
                            Some(receiver) => receiver.recv().await,
                            None => std::future::pending().await,
                        }
                    };
                    let market_during_work = async {
                        match self.market.market_watchdog_updates.as_mut() {
                            Some(receiver) => receiver.changed().await.is_ok(),
                            None => std::future::pending().await,
                        }
                    };
                    tokio::select! {
                        biased;
                        _ = ctrl_c_latched(&mut self.ctrl_c_rx) => {
                            self.recovery.runtime_state.handle(MakerEvent::StopRequested(RuntimeStopReason::CtrlC));
                            break 'execute take_stop_effect(&mut self.recovery.runtime_state, MakerExit::PositionReconciliation);
                        },
                        event = account_during_work => {
                            let Some(event) = event else {
                                let reason = "authenticated account stream disconnected during cycle".to_string();
                                self.recovery.runtime_state.handle(MakerEvent::AccountStreamDisconnected(reason.clone()));
                                if let Some(health) = cycle_account_stream_health {
                                    health.mark_unhealthy(reason);
                                }
                                return Err(LoopDirective::Restart);
                            };
                            let invalidates = account_event_invalidates_cycle(&event);
                            buffered_account.push(event);
                            if invalidates {
                                self.recovery.runtime_state.handle(MakerEvent::CycleInvalidated {
                                    reason: "account state changed during maker cycle".to_string(),
                                });
                                cycle_invalidated_by_account = true;
                                break None;
                            }
                        },
                        response = order_during_work => {
                            let Some(response) = response else {
                                let reason = "order-response stream disconnected during cycle".to_string();
                                self.recovery.runtime_state.handle(MakerEvent::OrderResponseDisconnected(reason.clone()));
                                if let Some(health) = cycle_order_response_health {
                                    health.mark_unhealthy(reason);
                                }
                                return Err(LoopDirective::Restart);
                            };
                            buffered_orders.push(response);
                        },
                        result = &mut work => break Some(result),
                        changed = market_during_work => {
                            if !changed {
                                self.market.market_watchdog_updates = None;
                                continue;
                            }
                            let idle_issue = match feed.as_ref() {
                                Some(feed) => {
                                    let state = feed.read().await;
                                    ws_snapshot_issue(&state, std::time::Instant::now())
                                        .filter(|issue| issue.is_idle())
                                }
                                None => None,
                            };
                            if let (false, Some(issue)) =
                                (market_paused_before_cycle, idle_issue)
                            {
                                cycle_invalidated_by_market = Some(format!(
                                    "market feed effective-update watchdog fired during maker cycle: {}",
                                    issue.as_str(),
                                ));
                                break None;
                            }
                        },
                    }
                }
            };
            if let Some(detail) = cycle_invalidated_by_market {
                let now_ms = duration_ms(market_data_health_started.elapsed());
                let transition = self
                    .market
                    .health
                    .observe(now_ms, maker::MarketDataObservation::FeedIdle);
                if let Some(detail) = degradation_detail(transition, &detail) {
                    self.recovery
                        .runtime_state
                        .handle(MakerEvent::MarketDataDegraded(detail.clone()));
                    self.market.pending_degradation = Some(detail);
                }
            }
            // The buffers are only fed from live-session receivers, so both are
            // empty in paper mode.
            if let Some(session) = self.live_session.as_mut() {
                if cycle_invalidated_by_account {
                    while let Ok(event) = session.account_events.try_recv() {
                        buffered_account.push(event);
                    }
                }
                // Apply the events buffered during work, ordering order-responses
                // before account events to mirror the top-of-loop drain.
                for response in buffered_orders {
                    let request_id = response.request_id.clone();
                    observe_order_ack(
                        Some(&mut session.order_latency),
                        Some(session.latency_started),
                        request_id.as_deref(),
                        response.accepted(),
                    );
                    let outcome = apply_order_response(
                        response,
                        &mut session.projection,
                        output_format,
                        symbol,
                        cycle,
                        cfg.price_decimals,
                    );
                    if let Some(reason) = order_response_failure(
                        &outcome,
                        request_id.as_deref(),
                        &mut self.recovery.runtime_state,
                    ) {
                        session.order_response_health.mark_unhealthy(reason);
                    }
                }
                for event in buffered_account {
                    match apply_account_event(
                        event,
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
                        },
                    ) {
                        Ok(outcome) => {
                            if outcome.requires_order_reconciliation {
                                cycle_invalidated_by_account = true;
                            }
                            let position = absorb_account_outcome(
                                outcome,
                                OutcomeSink {
                                    total_fills: &mut self.loop_state.counters.total_fills,
                                    balance_refresh_requested: &mut self
                                        .loop_state
                                        .account_balance_refresh_requested,
                                    inventory_exit_pending: &mut self
                                        .loop_state
                                        .inventory_exit_pending,
                                    notifier,
                                    position_alert_anchor: &mut self
                                        .loop_state
                                        .position_alert_anchor,
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
                            if let Some(position) = position {
                                if (position - self.loop_state.ledger.expected_position).abs()
                                    > qty_tolerance
                                {
                                    self.recovery.account_position_mismatch = Some(position);
                                } else {
                                    self.recovery.account_position_mismatch = None;
                                }
                            }
                        }
                        Err(error) => {
                            self.recovery
                                .runtime_state
                                .handle(MakerEvent::AccountStreamDisconnected(error.to_string()));
                            session
                                .account_stream_health
                                .mark_unhealthy(error.to_string());
                        }
                    }
                }
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
                    break 'execute stop_requested_exit(
                        &mut self.recovery.runtime_state,
                        RuntimeStopReason::AccountingInvariant(detail),
                    );
                }
            }

            let cycle_result = if let Some(reconciliation) = reconciliation_error_for_cycle(
                self.loop_state.ledger.expected_position,
                mismatch,
                self.recovery.account_position_mismatch.take(),
                cycle_invalidated_by_account,
            ) {
                Err(anyhow::Error::new(reconciliation))
            } else if let Some(cycle_result) = cycle_result {
                cycle_result
            } else {
                return Err(LoopDirective::Restart);
            };

            if !matches!(
                self.recovery.runtime_state.pending_effect(),
                None | Some(MakerEffect::RunCycle(_))
            ) && cycle_result.is_ok()
            {
                // A fail-closed event invalidated the generation while cycle work
                // was running. Do not commit its counters/alerts; the queued
                // abort/cleanup effects are consumed by the recovery path.
                return Err(LoopDirective::Restart);
            }
            return Ok(CycleAttempt {
                work_token: cycle_work_token,
                exit_pending_before,
                breaker_halted_before,
                result: cycle_result,
            });
        };
        Err(LoopDirective::Exit(exit))
    }

    async fn finish_cycle(&mut self, attempt: CycleAttempt) -> LoopDirective {
        let args = &self.deps.args;
        let output_format = self.deps.output_format;
        let client = &self.deps.client;
        let cfg = &self.deps.cfg;
        let symbol = &self.deps.symbol;
        let notifier = &self.deps.notifier;
        let qty_tolerance = self.deps.qty_tolerance;
        let run_order_prefix = &self.deps.run_order_prefix;
        let baseline_mark = self.deps.baseline_mark;
        let session_started_at = self.deps.session_started_at;
        let cycle = self.loop_state.counters.cycle;
        let CycleAttempt {
            work_token: cycle_work_token,
            exit_pending_before,
            breaker_halted_before,
            result: cycle_result,
        } = attempt;
        let exit = 'phase: {
            match cycle_result {
                Ok(CycleSuccess {
                    places,
                    cancels,
                    holds,
                    fills,
                    mark,
                    src,
                    market_fallback_reason,
                    halted,
                    exit_pending_after,
                    balance,
                }) => {
                    if !commit_cycle_effect(&mut self.recovery.runtime_state, cycle_work_token) {
                        return LoopDirective::Restart;
                    }
                    self.loop_state.next_cycle_is_recovery = false;
                    self.loop_state.counters.total_places += places;
                    self.loop_state.counters.total_cancels += cancels;
                    self.loop_state.counters.total_holds += holds;
                    self.loop_state.counters.total_fills += fills;
                    self.loop_state.counters.total_halted += halted as u64;
                    self.market.last_mark = Some(mark);
                    if halted != breaker_halted_before {
                        let (severity, event, message) = if halted {
                            (
                                RiskSeverity::Warning,
                                "entered",
                                "volatility breaker entered; maker quotes are being pulled",
                            )
                        } else {
                            (
                                RiskSeverity::Resolved,
                                "cleared",
                                "volatility breaker cleared; quoting may resume",
                            )
                        };
                        notifier
                            .risk(
                                RiskNotice::with_severity(
                                    severity,
                                    "volatility_breaker",
                                    event,
                                    message,
                                    symbol,
                                    cycle,
                                )
                                .position_after(self.loop_state.ledger.expected_position)
                                .expected(self.loop_state.ledger.expected_position),
                                false,
                            )
                            .await;
                    }
                    if !exit_pending_before && exit_pending_after {
                        notifier
                            .risk(
                                RiskNotice::warning(
                                    "inventory_exit",
                                    "submitted",
                                    "reduce-only inventory exit submitted",
                                    symbol,
                                    cycle,
                                )
                                .position_after(self.loop_state.ledger.expected_position)
                                .expected(self.loop_state.ledger.expected_position),
                                false,
                            )
                            .await;
                    } else if exit_pending_before && !exit_pending_after {
                        notifier
                            .risk(
                                RiskNotice::resolved("inventory_exit", "confirmed", "reduce-only inventory exit is no longer pending after ledger reconciliation", symbol, cycle).position_after(self.loop_state.ledger.expected_position).expected(self.loop_state.ledger.expected_position).observed(self.loop_state.ledger.expected_position),
                                false,
                            )
                        .await;
                    } else if exit_pending_before && exit_pending_after {
                        // The in-flight exit is still awaiting venue
                        // confirmation. The cycle itself now completes
                        // normally (cycle_summary stays gap-free for
                        // run-manifest validation); keep the historical
                        // notification wording so downstream consumers see
                        // the same contract as the pre-fix refused cycle.
                        notifier
                            .risk(
                                RiskNotice::warning("inventory_exit", "failed", "inventory exit cycle failed: inventory exit is still awaiting venue confirmation; refusing to submit another", symbol, cycle).position_after(self.loop_state.ledger.expected_position).expected(self.loop_state.ledger.expected_position),
                                false,
                            )
                        .await;
                    }
                    if !args.no_ws && self.market.last_src != Some(src) {
                        match src {
                            "ws" => eprintln!("✅ market feed: websocket live"),
                            _ => eprintln!(
                                "⚠️  market feed: REST fallback (reason={})",
                                market_fallback_reason.unwrap_or("ws_disabled")
                            ),
                        }
                        self.market.last_src = Some(src);
                    }
                    // Risk alerts: evaluate over the just-updated stats and
                    // deliver any state changes (stderr always; webhook if set).
                    let session_position = if args.live {
                        self.loop_state.ledger.expected_position
                    } else {
                        self.loop_state.stats.position()
                    };
                    if self.loop_state.alerts.session_enabled() {
                        let fired = self.loop_state.alerts.evaluate(
                            &self.loop_state.stats,
                            session_position,
                            mark,
                            cfg.max_position,
                            cycle,
                        );
                        for alert in fired {
                            // Await firing alerts so a breach raised on the final
                            // cycle before shutdown is not dropped with its task.
                            let await_delivery = alert.firing;
                            notifier.alert(&alert, symbol, await_delivery).await;
                        }
                    }
                    // Account equity / available-margin floors. The snapshot is
                    // only fetched in live mode, so these stay quiet in paper.
                    if self.loop_state.alerts.account_enabled() {
                        if let Some(balance) = balance.as_ref() {
                            let equity = balance.equity.parse::<f64>().ok();
                            let available = balance.cross_available.parse::<f64>().ok();
                            if let (Some(equity), Some(available)) = (equity, available) {
                                self.loop_state.balance_floor_parse_warned = false;
                                let fired =
                                    self.loop_state.alerts.evaluate_account(equity, available);
                                for alert in fired {
                                    let await_delivery = alert.firing;
                                    notifier.alert(&alert, symbol, await_delivery).await;
                                }
                            } else if !self.loop_state.balance_floor_parse_warned {
                                // An armed --alert-equity-below / --alert-margin-below
                                // must not go silently dark on unparseable balances.
                                self.loop_state.balance_floor_parse_warned = true;
                                eprintln!(
                                    "⚠️  equity/margin floor alerts skipped: unparseable balance fields (equity='{}', cross_available='{}')",
                                    balance.equity, balance.cross_available
                                );
                            }
                        }
                    }
                    // Financial brake: a session loss breaching --stop-loss routes
                    // through the fail-safe shutdown (freeze, cancel the maker
                    // book, await the critical webhook, exit) — the same path the
                    // other MakerExit variants use.
                    if args.stop_loss > 0.0 {
                        let pnl = self.loop_state.stats.pnl(session_position, mark);
                        if pnl <= -args.stop_loss {
                            emit_stop_loss_triggered(
                                output_format,
                                symbol,
                                cycle,
                                pnl,
                                args.stop_loss,
                            );
                            notifier
                                .risk(
                                    RiskNotice::critical("stop_loss", "triggered", &format!( "session PnL {pnl:+.2} breached stop-loss -{:.2}; shutting down", args.stop_loss ), symbol, cycle).position_after(self.loop_state.ledger.expected_position).expected(self.loop_state.ledger.expected_position),
                                    true,
                                )
                                .await;
                            self.recovery
                                .runtime_state
                                .handle(MakerEvent::StopRequested(RuntimeStopReason::StopLoss(
                                    format!("session PnL {pnl:+.2} <= -{:.2}", args.stop_loss),
                                )));
                            break 'phase take_stop_effect(
                                &mut self.recovery.runtime_state,
                                MakerExit::PositionReconciliation,
                            );
                        }
                    }
                }
                Err(e) => {
                    // Solvency brake (stage 5-b): raised from inside the cycle
                    // *before* any order work, so reaching here means the
                    // breached (or unverifiable) balance added no exposure.
                    // A different policy from the strategy's --stop-loss —
                    // separate config names, separate typed stop reason. Both
                    // stop and hand off; neither ever auto-flattens.
                    if let Some(floor) = e.downcast_ref::<AccountFloorError>() {
                        emit_account_floor_triggered(
                            output_format,
                            symbol,
                            cycle,
                            AccountFloorStop {
                                event: floor.cause.event(),
                                metric: floor.metric,
                                observed: floor.observed,
                                floor: floor.floor,
                                detail: &floor.detail,
                            },
                        );
                        notifier
                            .risk(
                                RiskNotice::critical(
                                    "account_floor",
                                    floor.cause.event(),
                                    &format!("{}; shutting down", floor.detail),
                                    symbol,
                                    cycle,
                                )
                                .position_after(self.loop_state.ledger.expected_position)
                                .expected(self.loop_state.ledger.expected_position),
                                true,
                            )
                            .await;
                        self.recovery
                            .runtime_state
                            .handle(MakerEvent::StopRequested(RuntimeStopReason::AccountFloor(
                                floor.detail.clone(),
                            )));
                        break 'phase take_stop_effect(
                            &mut self.recovery.runtime_state,
                            MakerExit::AccountFloor,
                        );
                    }
                    if let Some(degraded) = e.downcast_ref::<MarketDataDegradedError>() {
                        let detail = degraded.detail.clone();
                        self.recovery
                            .runtime_state
                            .handle(MakerEvent::MarketDataDegraded(detail.clone()));
                        self.market.pending_degradation = Some(detail);
                        return LoopDirective::Restart;
                    }
                    if e.downcast_ref::<ProjectionRegistryError>().is_some() {
                        let detail = format!("order-response correlation failed closed: {e}");
                        if let Some(session) = self.live_session.as_ref() {
                            session.order_response_health.mark_unhealthy(detail.clone());
                        }
                        self.recovery
                            .runtime_state
                            .handle(MakerEvent::OrderResponseDisconnected(detail));
                        return LoopDirective::Restart;
                    }
                    if let Some(mismatch) = e.downcast_ref::<PositionReconciliationError>() {
                        let reconciliation_cause = mismatch.cause.label();
                        // A mismatch is not a normal cycle error. Freeze quoting,
                        // empty the maker book, and give account-order callbacks
                        // plus REST settlement a bounded three-second window to
                        // converge before failing closed.
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
                                target: RecoveryTarget::PositionReconciliation,
                                trigger: MakerEvent::PositionMismatch,
                                cleanup_effect_stop: EffectFailureStop::CleanupFailure,
                                recovery_effect_stop: EffectFailureStop::PositionReconciliation,
                                cleanup_failure_prefix: String::new(),
                                cleanup_failed_exit: MakerExit::PositionReconciliation,
                                notice: FreezeNotice::Risk(RiskNotice::warning("position_reconciliation", "frozen", match &mismatch.cause { PositionReconciliationCause::CycleInvalidation => "account update invalidated active cycle; placements frozen and maker cleanup starting", PositionReconciliationCause::UnknownCurrentRunOrder => "unknown current-run order detected; placements frozen and maker cleanup starting", PositionReconciliationCause::PositionMismatch => "position mismatch detected; placements frozen and maker cleanup starting", }, symbol, cycle).expected(mismatch.expected).observed(mismatch.observed)),
                                frozen_note: Some(ReconciliationStateNote {
                                    cause: reconciliation_cause,
                                    expected: mismatch.expected,
                                    observed: mismatch.observed,
                                }),
                                abort_account_stream_handle: false,
                                continuity: OrderResponseContinuity::Preserved,
                                cancel_venue_orders: true,
                            },
                        )
                        .await
                        {
                            Ok(token) => token,
                            Err(exit) => break 'phase exit,
                        };
                        let mut recovered = false;
                        let mut last_observed = mismatch.observed;
                        for delay in [500_u64, 1_000, 1_500] {
                            // The maker book is verified empty at this point, so
                            // aborting the convergence wait on Ctrl+C is safe.
                            tokio::select! {
                                _ = ctrl_c_latched(&mut self.ctrl_c_rx) => {
                                    break 'phase stop_requested_exit(
                                        &mut self.recovery.runtime_state,
                                        RuntimeStopReason::CtrlC,
                                    );
                                }
                                _ = tokio::time::sleep(Duration::from_millis(delay)) => {}
                            }
                            if let Some(session) = self.live_session.as_mut() {
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
                                    },
                                ) {
                                    Ok(outcome) => {
                                        if let Some(position) = absorb_account_outcome(
                                            outcome,
                                            OutcomeSink {
                                                total_fills: &mut self
                                                    .loop_state
                                                    .counters
                                                    .total_fills,
                                                balance_refresh_requested: &mut self
                                                    .loop_state
                                                    .account_balance_refresh_requested,
                                                inventory_exit_pending: &mut self
                                                    .loop_state
                                                    .inventory_exit_pending,
                                                notifier,
                                                position_alert_anchor: &mut self
                                                    .loop_state
                                                    .position_alert_anchor,
                                                expected_position: self
                                                    .loop_state
                                                    .ledger
                                                    .expected_position,
                                                max_position: cfg.max_position,
                                                inventory_exit_pct: args.inventory_exit_pct,
                                                qty_tolerance,
                                                symbol,
                                                cycle,
                                                order_latency: Some(&mut session.order_latency),
                                                latency_started: Some(session.latency_started),
                                            },
                                        )
                                        .await
                                        {
                                            last_observed = position;
                                        }
                                    }
                                    Err(error) => {
                                        // Fail closed like the account-stream
                                        // path: recovery cannot trust a ledger
                                        // whose event drain failed validation.
                                        break 'phase recovery_failed_exit(
                                            &mut self.recovery.runtime_state,
                                            recovery_token,
                                            format!(
                                                "position reconciliation event validation failed during REST backfill: {error}"
                                            ),
                                        );
                                    }
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
                                &mut self.loop_state.counters.total_fills,
                                cycle,
                                output_format,
                            )
                            .await
                            {
                                ConvergenceProbe::Converged { observed } => {
                                    last_observed = observed;
                                    recovered = true;
                                    break;
                                }
                                ConvergenceProbe::Pending { observed } => last_observed = observed,
                                ConvergenceProbe::SnapshotFailed(error) => {
                                    emit_reconciliation_snapshot_error(
                                        output_format,
                                        symbol,
                                        cycle,
                                        &error.to_string(),
                                    )
                                }
                            }
                        }
                        if recovered {
                            // Requests already accepted by the venue can become
                            // visible after the initial freeze cleanup. Verify the
                            // maker book again at the end of the bounded recovery
                            // window before unfreezing placements.
                            if let Err(error) =
                                cancel_maker_orders_with_retry(client, symbol, 3, output_format)
                                    .await
                            {
                                break 'phase recovery_failed_exit(
                                    &mut self.recovery.runtime_state,
                                    recovery_token,
                                    format!(
                                        "position reconciliation final maker-book verification failed: {error}"
                                    ),
                                );
                            }
                            self.recovery.account_order_reconciliation_required = false;
                            resume_quoting_after_recovery(
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
                                ResumeSpec {
                                    recovery_token,
                                    observed: last_observed,
                                    continuity: OrderResponseContinuity::Preserved,
                                    clear_resting: false,
                                    recovered_note: Some(ReconciliationStateNote {
                                        cause: reconciliation_cause,
                                        expected: self.loop_state.ledger.expected_position,
                                        observed: last_observed,
                                    }),
                                    notice: RiskNotice::resolved("position_reconciliation", "recovered", "position ledger recovered within the 3-second freeze window; quoting may resume from an empty maker book", symbol, cycle).expected(self.loop_state.ledger.expected_position).observed(last_observed),
                                },
                            )
                            .await;
                            return LoopDirective::Restart;
                        }
                        emit_reconciliation_state(
                            output_format,
                            symbol,
                            cycle,
                            "failed",
                            reconciliation_cause,
                            self.loop_state.ledger.expected_position,
                            last_observed,
                        );
                        notifier
                            .risk(
                                RiskNotice::critical("position_reconciliation", "failed", "position ledger remained inconsistent after the 3-second freeze window", symbol, cycle).expected(self.loop_state.ledger.expected_position).observed(last_observed),
                                true,
                            )
                        .await;
                        self.recovery
                            .runtime_state
                            .handle(MakerEvent::RecoveryFailed {
                                token: recovery_token,
                                reason: format!(
                                "expected position {:+.8}, venue reported {:+.8} after 3s freeze",
                                self.loop_state.ledger.expected_position, last_observed
                            ),
                            });
                        break 'phase take_stop_effect(
                            &mut self.recovery.runtime_state,
                            MakerExit::PositionReconciliation,
                        );
                    }
                    if exit_pending_before {
                        let message = format!("inventory exit cycle failed: {e}");
                        notifier
                            .risk(
                                RiskNotice::warning(
                                    "inventory_exit",
                                    "failed",
                                    &message,
                                    symbol,
                                    cycle,
                                )
                                .position_after(self.loop_state.ledger.expected_position)
                                .expected(self.loop_state.ledger.expected_position),
                                false,
                            )
                            .await;
                    }
                    self.recovery.runtime_state.handle(MakerEvent::CycleFailed {
                        token: cycle_work_token,
                        reason: e.to_string(),
                    });
                    eprintln!(
                        "⚠️  maker cycle failed ({}/{}): {}",
                        self.recovery.runtime_state.consecutive_cycle_errors(),
                        MAX_CONSECUTIVE_CYCLE_ERRORS,
                        e
                    );
                    if matches!(
                        self.recovery.runtime_state.pending_effect(),
                        Some(MakerEffect::Stop(_))
                    ) {
                        break 'phase take_stop_effect(
                            &mut self.recovery.runtime_state,
                            MakerExit::ConsecutiveErrors,
                        );
                    }
                }
            }
            return LoopDirective::Proceed;
        };
        LoopDirective::Exit(exit)
    }

    async fn wait_phase(&mut self) -> LoopDirective {
        let args = &self.deps.args;
        let output_format = self.deps.output_format;
        let cfg = &self.deps.cfg;
        let symbol = &self.deps.symbol;
        let notifier = &self.deps.notifier;
        let qty_tolerance = self.deps.qty_tolerance;
        let run_order_prefix = &self.deps.run_order_prefix;
        let baseline_mark = self.deps.baseline_mark;
        let market_data_health_started = self.market.health_started;
        let feed = &self.market.feed;
        let exit = 'phase: {
            self.loop_state.counters.cycle += 1;

            if matches!(
                self.recovery.runtime_state.pending_effect(),
                Some(MakerEffect::RunCycle(_))
            ) {
                return LoopDirective::Restart;
            }
            if self.loop_state.account_balance_refresh_requested
                && account_risk_watch_enabled(args, &self.loop_state.alerts)
            {
                // A balance event arrived while the just-finished cycle was doing
                // I/O. Skip the normal interval so the next cycle can fetch the
                // authoritative unified balance and evaluate account floors.
                return LoopDirective::Restart;
            }
            self.loop_state.account_balance_refresh_requested = false;

            // Sleep until the next cycle, but wake early when a coherent market
            // update invalidates the prior decision: mark drift, a quote crossing
            // the new touch, or mark/mid divergence. The one-second floor keeps
            // this a bounded safety replan rather than a per-tick cancel loop.
            let deadline = tokio::time::Instant::now() + Duration::from_secs(args.interval);
            let min_gap = tokio::time::Instant::now() + Duration::from_secs(1);
            // Price and book are published independently. Start from the
            // current pair so repeated updates from only one channel cannot
            // be miscounted as multiple distinct health observations.
            let mut health_version = match feed.as_ref() {
                Some(feed) => {
                    let state = feed.read().await;
                    fresh_ws_sample(&state).map(|(_, _, _, version)| version)
                }
                None => None,
            };
            loop {
                let request_deadline = self.live_session.as_ref().and_then(|session| {
                    session
                        .order_request_deadlines
                        .next_deadline(ORDER_REQUEST_TIMEOUT)
                });
                let request_timeout = async {
                    match request_deadline {
                        Some(deadline) => {
                            tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await
                        }
                        None => std::future::pending().await,
                    }
                };
                let market_wakeup_at = self
                    .market
                    .health
                    .is_degraded()
                    .then_some(self.market.next_heartbeat)
                    .flatten();
                let market_wakeup = async {
                    match market_wakeup_at {
                        Some(deadline) => {
                            tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await
                        }
                        None => std::future::pending().await,
                    }
                };
                let update = async {
                    match self.market.updates.as_mut() {
                        Some(rx) => rx.changed().await.is_ok(),
                        None => std::future::pending().await,
                    }
                };
                let account_update = async {
                    match self.live_session.as_mut() {
                        Some(session) => session.account_events.recv().await,
                        None => std::future::pending().await,
                    }
                };
                let external_update = async {
                    match self.loop_state.external_updates.as_mut() {
                        Some(rx) => rx.changed().await.is_ok(),
                        None => std::future::pending().await,
                    }
                };
                tokio::select! {
                    _ = ctrl_c_latched(&mut self.ctrl_c_rx) => {
                        self.recovery.runtime_state.handle(MakerEvent::StopRequested(RuntimeStopReason::CtrlC));
                        break 'phase take_stop_effect(&mut self.recovery.runtime_state, MakerExit::PositionReconciliation);
                    },
                    _ = tokio::time::sleep_until(deadline) => break,
                    _ = request_timeout => break,
                    _ = market_wakeup => break,
                    ok = external_update => {
                        // Leader-feed early wake: replan only when the fresh
                        // divergence would change the guard's decision, and
                        // never faster than the bounded-replan floor. A dead
                        // feed channel is a fail-open non-event.
                        if !ok {
                            self.loop_state.external_updates = None;
                            continue;
                        }
                        if tokio::time::Instant::now() < min_gap {
                            continue;
                        }
                        let Some(feed) = self.loop_state.external_feed.as_ref() else {
                            continue;
                        };
                        let Some(mark) = self.market.last_mark else {
                            continue;
                        };
                        let state = *feed.read().await;
                        let Some((raw_bps, age_ms)) =
                            super::super::external_feed::raw_divergence(
                                state,
                                mark,
                                std::time::Instant::now(),
                            )
                        else {
                            continue;
                        };
                        let config = self.loop_state.guard_controller.config();
                        if age_ms > config.max_age_ms {
                            continue;
                        }
                        // Excess over the slow basis, read-only: the cycle is
                        // the single writer of the baseline.
                        let excess = self.loop_state.external_basis.peek(raw_bps);
                        let magnitude = excess.abs();
                        let toward = if excess > 0.0 {
                            standx_sdk::models::OrderSide::Sell
                        } else {
                            standx_sdk::models::OrderSide::Buy
                        };
                        let would_change = match self.loop_state.guard_controller.endangered() {
                            // Release, or a sign flip past the enter threshold
                            // (side switch), both warrant an early replan.
                            Some(side) => {
                                magnitude < config.exit_bps
                                    || (magnitude >= config.enter_bps && toward != side)
                            }
                            None => magnitude >= config.enter_bps,
                        };
                        if would_change {
                            break;
                        }
                    },
                    event = account_update => {
                        // The branch futures are dropped before select! handlers
                        // run, so the session can be re-borrowed here. An event
                        // only arrives when the live session exists.
                        match (event, self.live_session.as_mut()) {
                            (Some(event), Some(session)) => match apply_account_event(
                                event,
                                &mut AccountEventState {
                                    ledger: &mut self.loop_state.ledger,
                                    stats: &mut self.loop_state.stats,
                                    projection: &mut session.projection,
                                },
                                &AccountEventContext {
                                    symbol,
                                    run_order_prefix,
                                    mark: self.market.last_mark.unwrap_or(baseline_mark),
                                    cycle: self.loop_state.counters.cycle,
                                    output_format,
                                },
                            ) {
                                Ok(outcome) => {
                                    self.recovery.account_order_reconciliation_required |=
                                        outcome.requires_order_reconciliation;
                                    let position = absorb_account_outcome(
                                        outcome,
                                        OutcomeSink {
                                            total_fills: &mut self.loop_state.counters.total_fills,
                                            balance_refresh_requested: &mut self.loop_state.account_balance_refresh_requested,
                                            inventory_exit_pending: &mut self.loop_state.inventory_exit_pending,
                                            notifier,
                                            position_alert_anchor: &mut self.loop_state.position_alert_anchor,
                                            expected_position: self.loop_state.ledger.expected_position,
                                            max_position: cfg.max_position,
                                            inventory_exit_pct: args.inventory_exit_pct,
                                            qty_tolerance,
                                            symbol,
                                            cycle: self.loop_state.counters.cycle,
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
                                    break;
                                }
                                Err(error) => {
                                    session.account_stream_health.mark_unhealthy(error.to_string());
                                    break;
                                }
                            },
                            (None, Some(session)) => {
                                session
                                    .account_stream_health
                                    .mark_unhealthy("authenticated account stream disconnected");
                                break;
                            }
                            (_, None) => break,
                        }
                    }
                    ok = update => {
                        if !ok {
                            let now_ms = duration_ms(market_data_health_started.elapsed());
                            let transition = self.market.health.observe(
                                now_ms,
                                maker::MarketDataObservation::RestFallback,
                            );
                            if let Some(detail) =
                                degradation_detail(transition, "market feed task ended")
                            {
                                self.recovery.runtime_state.handle(MakerEvent::MarketDataDegraded(detail.clone()));
                                self.market.pending_degradation = Some(detail);
                                break;
                            }
                            if matches!(
                                transition,
                                maker::MarketDataTransition::ClassChanged { .. }
                                    | maker::MarketDataTransition::RecoveryReady
                            ) {
                                self.market.updates = None;
                                break;
                            }
                            // The next interval cycle records another REST
                            // fallback observation and may cross the bounded grace.
                            self.market.updates = None;
                            continue;
                        }
                        let Some(feed) = feed.as_ref() else {
                            continue;
                        };
                        let resting_for_replan = match self.live_session.as_ref() {
                            Some(session) => session.projection.resting_quotes(),
                            None => self.loop_state.resting.clone(),
                        };
                        let (classified, requires_replan) = {
                            let s = feed.read().await;
                            match fresh_ws_sample(&s) {
                                Some((mark, best_bid, best_ask, version)) => {
                                    let classified = classify_market_health(
                                        mark,
                                        best_bid,
                                        best_ask,
                                        args.max_divergence_bps,
                                    );
                                    let requires_replan = self.market.last_mark.is_some_and(|prev| {
                                        market_update_requires_replan(
                                            prev,
                                            mark,
                                            best_bid,
                                            best_ask,
                                            &resting_for_replan,
                                            cfg.refresh_bps,
                                            args.max_divergence_bps,
                                        )
                                    });
                                    let classified = if version.both_advanced_from(health_version) {
                                        health_version = Some(version);
                                        Some(classified)
                                    } else {
                                        None
                                    };
                                    (classified, requires_replan)
                                }
                                None => {
                                    let issue = ws_snapshot_issue(&s, std::time::Instant::now());
                                    let observation = if issue.is_some_and(|issue| issue.is_idle()) {
                                        maker::MarketDataObservation::FeedIdle
                                    } else {
                                        maker::MarketDataObservation::RestFallback
                                    };
                                    let classified = ClassifiedMarketHealth {
                                        observation,
                                        detail: format!(
                                            "websocket cache unavailable: {}",
                                            issue.map_or("unknown", |issue| issue.as_str())
                                        ),
                                        divergence_bps: None,
                                    };
                                    (Some(classified), false)
                                }
                            }
                        };
                        if let Some(classified) = classified {
                            let now_ms = duration_ms(market_data_health_started.elapsed());
                            self.market.last_divergence_bps = classified.divergence_bps;
                            let transition = self
                                .market
                                .health
                                .observe(now_ms, classified.observation);
                            if let Some(detail) = degradation_detail(transition, &classified.detail) {
                                self.recovery.runtime_state.handle(MakerEvent::MarketDataDegraded(detail.clone()));
                                self.market.pending_degradation = Some(detail);
                                break;
                            }
                            if matches!(
                                transition,
                                maker::MarketDataTransition::ClassChanged { .. }
                                    | maker::MarketDataTransition::RecoveryReady
                            ) {
                                break;
                            }
                        }
                        if tokio::time::Instant::now() < min_gap {
                            continue;
                        }
                        if requires_replan {
                            self.recovery.runtime_state.handle(MakerEvent::MarketChanged);
                            break; // early re-quote cycle
                        }
                    }
                }
            }
            return LoopDirective::Restart;
        };
        LoopDirective::Exit(exit)
    }
}
