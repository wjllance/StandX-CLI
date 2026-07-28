use super::ledger::{adopt_order, apply_rest_trade};
#[cfg(test)]
use super::ledger::{apply_account_trade, apply_order_update, maker_trade_fill};
use super::model::{position_for_symbol, rest_order_observation};
use super::output::{
    emit_cycle_skip, emit_guard_transition, emit_maker_cycle, log_maker_event, CycleOutput,
    ExitStatus, MakerLogEvent,
};
use super::pipeline::{
    fetch_account_audit, CycleRequest, CycleResult, CycleState, OrderRequestKind,
    BALANCE_FLOOR_MAX_AGE,
};
use super::recovery::PositionReconciliationError;
use anyhow::Result;
use standx_maker::{
    self as maker, AccountProjectionEvent, MakerAccountProjection, MakerFill, MakerLedger,
    MakerStats, OrderLatencyTracker, ProjectionPendingCancel, ProjectionPendingPlace,
    ProjectionRegistryError, RestingQuote, MAX_PENDING_ORDER_REQUESTS,
};
use standx_sdk::account_stream::AccountStreamHealth;
#[cfg(test)]
use standx_sdk::account_stream::{OrderUpdate, TradeUpdate};
use standx_sdk::client::order::CreateOrderParams;
use standx_sdk::models::{Balance, OrderSide, OrderType, TimeInForce, Trade};
use standx_sdk::order_response::{OrderCommandSender, OrderResponseHealth};
use std::time::Instant;

const ORDER_LATENCY_TIMEOUT_MS: u64 = 15_000;

struct LatencyRegistration<'a> {
    started: Option<Instant>,
    request_id: &'a str,
    kind: maker::LatencyRequestKind,
    generation: u64,
    cycle: u64,
    symbol: &'a str,
    side: OrderSide,
    level: u32,
    order_id: Option<u64>,
    market_source: &'a str,
    recovery: bool,
}

fn register_order_latency(
    tracker: &mut Option<&mut OrderLatencyTracker>,
    registration: LatencyRegistration<'_>,
) {
    let LatencyRegistration {
        started,
        request_id,
        kind,
        generation,
        cycle,
        symbol,
        side,
        level,
        order_id,
        market_source,
        recovery,
    } = registration;
    let (Some(tracker), Some(started)) = (tracker.as_deref_mut(), started) else {
        return;
    };
    if let Err(error) = tracker.register(maker::LatencyRequestContext {
        request_id: request_id.to_string(),
        kind,
        generation,
        cycle,
        symbol: symbol.to_string(),
        side: Some(side),
        level: Some(level),
        order_id,
        market_source: Some(market_source.to_string()),
        recovery,
        intent_ms: elapsed_ms(started),
        intent_utc_ms: chrono::Utc::now().timestamp_millis(),
    }) {
        eprintln!("⚠️ order latency registration unavailable: {error}");
    }
}

fn observe_order_write(
    tracker: &mut Option<&mut OrderLatencyTracker>,
    started: Option<Instant>,
    request_id: &str,
    sent: bool,
) {
    let (Some(tracker), Some(started)) = (tracker.as_deref_mut(), started) else {
        return;
    };
    let at_ms = elapsed_ms(started);
    let outcome = if sent {
        tracker.mark_written(request_id, at_ms)
    } else {
        tracker.mark_invalidated(request_id, at_ms)
    };
    if let Err(error) = outcome {
        eprintln!("⚠️ order latency write observation unavailable: {error}");
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn collect_current_run_fills(
    trades: Vec<Trade>,
    ledger: &mut MakerLedger,
    session_started_at: i64,
    now: i64,
    mark: f64,
    stats: &mut MakerStats,
    fills: &mut Vec<MakerFill>,
) -> Result<bool> {
    let mut exit_fill_observed = false;
    for trade in trades {
        exit_fill_observed |=
            apply_rest_trade(ledger, trade, session_started_at, now, mark, stats, fills)?;
    }
    Ok(exit_fill_observed)
}

fn unhealthy_order_response(health: Option<&OrderResponseHealth>) -> Option<String> {
    match health {
        Some(health) if health.is_healthy() => None,
        Some(health) => Some(health.failure_reason().unwrap_or_else(|| {
            "order-response stream became unhealthy without a recorded reason".to_string()
        })),
        None => Some("order-response health state is unavailable".to_string()),
    }
}

fn unhealthy_account_stream(health: Option<&AccountStreamHealth>) -> Option<String> {
    match health {
        Some(health) if health.is_healthy() => None,
        Some(health) => Some(health.failure_reason().unwrap_or_else(|| {
            "account stream became unhealthy without a recorded reason".to_string()
        })),
        None => Some("account stream health state is unavailable".to_string()),
    }
}

fn ensure_live_streams_healthy(
    account_health: Option<&AccountStreamHealth>,
    order_health: Option<&OrderResponseHealth>,
) -> Result<()> {
    if let Some(reason) = unhealthy_account_stream(account_health) {
        return Err(anyhow::anyhow!(
            "{reason}; refusing further live order actions"
        ));
    }
    if let Some(reason) = unhealthy_order_response(order_health) {
        return Err(anyhow::anyhow!(
            "{reason}; refusing further live order actions"
        ));
    }
    Ok(())
}

fn apply_request_submission(
    projection: &mut MakerAccountProjection,
    event: AccountProjectionEvent,
) -> Result<()> {
    let generation = projection.generation();
    let outcome = projection.apply(generation, event);
    match outcome.request_registry_error {
        Some(error) => Err(anyhow::Error::new(error)),
        None => Ok(()),
    }
}

fn ensure_request_registry_capacity(projection: Option<&MakerAccountProjection>) -> Result<()> {
    let projection =
        projection.ok_or_else(|| anyhow::anyhow!("live maker request registry is unavailable"))?;
    if projection.pending_request_count() >= MAX_PENDING_ORDER_REQUESTS {
        return Err(anyhow::Error::new(ProjectionRegistryError::Capacity {
            limit: MAX_PENDING_ORDER_REQUESTS,
        }));
    }
    Ok(())
}

fn order_creation_allowed(live: bool, rest_position_recheck_pending: bool) -> bool {
    !live || !rest_position_recheck_pending
}

/// Evaluate armed account hard floors against the authoritative balance
/// (stage 5-b). `None` means "keep quoting"; anything else stops the cycle
/// before it can place an order.
///
/// Disarmed floors (the default `0`) short-circuit, so neither staleness nor an
/// unparseable field can stop a run that never asked for a solvency brake. When
/// a floor *is* armed, only the fields that floor actually reads have to be
/// readable — an armed equity floor is not held hostage by a broken margin
/// field.
fn account_floor_stop(
    balance: &Balance,
    balance_age: std::time::Duration,
    equity_floor: f64,
    margin_floor: f64,
) -> Option<super::model::AccountFloorError> {
    use super::model::AccountFloorError;

    let equity_armed = equity_floor.is_finite() && equity_floor > 0.0;
    let margin_armed = margin_floor.is_finite() && margin_floor > 0.0;
    if !equity_armed && !margin_armed {
        return None;
    }
    // A balance this old is no longer evidence of solvency. Failing closed here
    // is the whole point of arming a floor: the alternative is reading a stale
    // snapshot as "no breach".
    if balance_age > BALANCE_FLOOR_MAX_AGE {
        return Some(AccountFloorError::balance_stale(
            balance_age.as_secs(),
            BALANCE_FLOOR_MAX_AGE.as_secs(),
        ));
    }
    let parse = |raw: &str| raw.parse::<f64>().ok().filter(|value| value.is_finite());
    let equity = parse(&balance.equity);
    let available = parse(&balance.cross_available);
    if (equity_armed && equity.is_none()) || (margin_armed && available.is_none()) {
        return Some(AccountFloorError::balance_unreadable(
            &balance.equity,
            &balance.cross_available,
        ));
    }
    maker::account_floor_breach(
        equity.unwrap_or(f64::NAN),
        available.unwrap_or(f64::NAN),
        equity_floor,
        margin_floor,
    )
    .map(|(metric, observed, floor)| AccountFloorError::breach(metric.as_str(), observed, floor))
}

fn live_order_commands(commands: Option<&OrderCommandSender>) -> Result<&OrderCommandSender> {
    commands.ok_or_else(|| anyhow::anyhow!("order-command stream is unavailable"))
}

/// One reconcile cycle over an already-acquired market snapshot.
/// Returns (places, cancels, holds, fills) counts. `sim_position` carries the
/// paper-mode simulated inventory across cycles (unused in live).
pub(super) async fn maker_cycle(
    request: CycleRequest<'_>,
    state: CycleState<'_>,
) -> Result<CycleResult> {
    let CycleRequest {
        client,
        symbol,
        cfg,
        live,
        cycle,
        mark,
        best_bid,
        best_ask,
        market_data_mode,
        market_source,
        recovery,
        market_fallback_reason,
        ws_snapshot,
        max_divergence_bps,
        inventory_exit_pct,
        inventory_exit_qty,
        stop_equity_below,
        stop_margin_below,
        wind_down,
        qty_tolerance,
        session_started_at,
        run_order_prefix,
        starting_position,
        output_format,
        order_commands,
        order_response_health,
        account_stream_health,
        performance_time_ms,
    } = request;
    let CycleState {
        resting,
        mut account_projection,
        inventory_exit_pending,
        ledger,
        sim_position,
        stats,
        breaker,
        spread_controller,
        size_skew_controller,
        nonlinear_skew,
        guard_controller,
        external_divergence,
        external_basis_bps,
        mut order_request_deadlines,
        live_account_poll,
        mut order_latency,
        latency_started,
    } = state;
    use maker::{format_decimals, paper_quote_filled, Action, CycleInput, MarketSnapshot};

    // 0. Run all market-only guards before any account/order I/O. The pure
    // planner owns breaker observation and data-consistency policy; this
    // adapter only renders the resulting skip decision.
    let market = MarketSnapshot {
        mark,
        best_bid,
        best_ask,
    };
    if let Some(performance) = ledger.performance_mut() {
        let observed_resting = account_projection
            .as_deref()
            .map(|projection| projection.resting_quotes())
            .unwrap_or_else(|| resting.clone());
        let (eligible_bid_qty, eligible_ask_qty) =
            eligible_quote_qty(&observed_resting, mark, cfg.band_bps);
        let observation = performance
            .observe_market(performance_time_ms, mark)
            .and_then(|()| {
                performance.observe_quote_quality(maker::QuoteQualityInterval {
                    event_time_ms: performance_time_ms,
                    eligible_bid_qty,
                    eligible_ask_qty,
                })
            });
        if let Err(error) = observation {
            eprintln!("⚠️ maker performance observation disabled: {error}");
            ledger.disable_performance();
        }
    }
    let preflight = maker::preflight_cycle_at(
        breaker,
        performance_time_ms,
        market,
        max_divergence_bps,
        live,
    )?;
    let spread_decision = spread_controller.observe(breaker.vol_bps(), cfg);
    let effective_cfg = spread_controller.effective_config(cfg, &spread_decision);
    let cfg = &effective_cfg;
    let halted = match preflight.skip {
        Some(skip) => {
            emit_cycle_skip(
                output_format,
                cycle,
                symbol,
                live,
                mark,
                cfg.price_decimals,
                max_divergence_bps,
                skip,
            );
            if market_data_mode == maker::MarketDataMode::Active {
                return Ok(CycleResult::default());
            }
            preflight.halted
        }
        None => preflight.halted,
    };

    // 2. Use the authenticated account-stream projection in live mode or the
    //    simulated in-memory book in paper mode. REST is only a periodic audit.
    let position: f64;
    let mut projected_resting = Vec::new();
    let mut account_balance: Option<Balance> = None;
    let mut account_floor_stop_reason = None;
    let mut fills: Vec<MakerFill> = Vec::new();
    let mut exit_fill_observed = false;
    let mut rest_position_recheck_pending = false;
    if live {
        let projection = account_projection
            .as_deref_mut()
            .expect("live maker cycles require initialized account projection");
        let generation = projection.generation();
        projection.apply(generation, AccountProjectionEvent::AdvanceCycle { cycle });
        if let (Some(tracker), Some(started)) = (order_latency.as_deref_mut(), latency_started) {
            let at_ms = elapsed_ms(started);
            if let Err(error) = tracker.timeout_pending(at_ms, ORDER_LATENCY_TIMEOUT_MS) {
                eprintln!("⚠️ order latency timeout observation unavailable: {error}");
            }
        }
        let poll =
            live_account_poll.expect("live maker cycles require initialized account polling state");
        let poll_now = std::time::Instant::now();
        let now = chrono::Utc::now().timestamp();
        let audit_due = poll.account_audit_due(poll_now);
        let balance_refresh_due = poll.balance_refresh_due(poll_now);
        let audit_future = async {
            if audit_due {
                Some(fetch_account_audit(client, symbol, session_started_at, now).await)
            } else {
                None
            }
        };
        let balance_future = async {
            if balance_refresh_due {
                Some(client.get_balance().await)
            } else {
                None
            }
        };
        let (audit, refreshed_balance) = tokio::join!(audit_future, balance_future);
        // Resolve every due read before mutating the current-run ledger. A
        // failed audit must leave this cycle's accounting exactly untouched.
        let audit = match audit {
            Some(audit) => Some(audit?),
            None => None,
        };
        if let Some(refreshed_balance) = refreshed_balance {
            match refreshed_balance {
                Ok(balance) => poll.record_balance_refresh(balance, poll_now),
                Err(error) => {
                    poll.record_balance_refresh_failure(poll_now);
                    if !poll.balance_is_within_stale_limit(poll_now) {
                        return Err(error.into());
                    }
                    eprintln!(
                        "⚠️  account balance refresh failed; reusing cached balance for up to 60s: {error}"
                    );
                }
            }
        }
        account_balance = Some(poll.balance().clone());
        // Stage 5-b: decide the solvency verdict here, where the authoritative
        // balance is freshest, and act on it before step 3 plans anything. The
        // accounting below still runs to completion so the position handed off
        // on the way out is the fully synchronized one.
        account_floor_stop_reason = account_floor_stop(
            poll.balance(),
            poll.balance_age(poll_now),
            stop_equity_below,
            stop_margin_below,
        );

        if let Some(audit) = audit {
            for order in audit.open_orders.iter().chain(audit.filled_orders.iter()) {
                adopt_order(ledger, order, run_order_prefix)?;
            }
            let fill_start = fills.len();
            exit_fill_observed |= collect_current_run_fills(
                audit.trades,
                ledger,
                session_started_at,
                now,
                mark,
                stats,
                &mut fills,
            )?;
            for fill in &fills[fill_start..] {
                if let Some(order_id) = fill.order_id {
                    projection.apply(
                        generation,
                        AccountProjectionEvent::TradeApplied {
                            order_id,
                            qty: fill.qty,
                        },
                    );
                }
            }
            let observed_position = position_for_symbol(&audit.positions, symbol)?;
            let observations = audit
                .open_orders
                .iter()
                .map(rest_order_observation)
                .collect::<Result<Vec<_>>>()?;
            let qty_tolerance = 10_f64.powi(-(cfg.qty_decimals as i32)) / 2.0;
            let unexpected_order_ids =
                projection.unexpected_rest_open_order_ids(generation, &observations);
            if !unexpected_order_ids.is_empty() {
                eprintln!(
                    "⚠️  REST audit found unexpected current-run open order IDs: {unexpected_order_ids:?}"
                );
                return Err(anyhow::Error::new(
                    PositionReconciliationError::unknown_current_run_order(
                        ledger.expected_position,
                    ),
                ));
            }

            let projected_position = projection.observed_position();
            if (projected_position - ledger.expected_position).abs() > qty_tolerance {
                return Err(anyhow::Error::new(
                    PositionReconciliationError::position_mismatch(
                        ledger.expected_position,
                        projected_position,
                    ),
                ));
            }

            if (observed_position - ledger.expected_position).abs() > qty_tolerance {
                if poll.record_rest_position_mismatch(poll_now) {
                    return Err(anyhow::Error::new(
                        PositionReconciliationError::position_mismatch(
                            ledger.expected_position,
                            observed_position,
                        ),
                    ));
                }
                eprintln!(
                    "⚠️  REST position {observed_position:+.8} differs from healthy WS/ledger {:+.8}; suppressing new orders until one recheck in 3s",
                    ledger.expected_position
                );
            } else {
                if poll.rest_position_recheck_pending() {
                    eprintln!(
                        "✅ REST position recheck converged at {observed_position:+.8}; resuming new orders"
                    );
                }
                poll.record_account_audit(poll_now);
            }
            if let (Some(tracker), Some(started)) = (order_latency.as_deref_mut(), latency_started)
            {
                let open_order_ids = observations
                    .iter()
                    .map(|observation| observation.order_id)
                    .collect::<Vec<_>>();
                let at_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                if let Err(error) = tracker.mark_absent_cancels_effective(&open_order_ids, at_ms) {
                    eprintln!("⚠️ order latency REST-effective observation unavailable: {error}");
                }
            }
        }
        rest_position_recheck_pending = poll.rest_position_recheck_pending();
        position = ledger.expected_position;
        projected_resting = projection.resting_quotes();
    } else {
        // Paper mode: simulate fills against the touch so inventory (and thus
        // skew) is observable without going live. A crossed resting quote is
        // taken off the book and its signed qty folded into the position; the
        // reconcile below then re-quotes the vacated level.
        let mut i = 0;
        while market_data_mode == maker::MarketDataMode::Active && i < resting.len() {
            if paper_quote_filled(resting[i].side, resting[i].price, best_bid, best_ask) {
                let q = resting.remove(i);
                *sim_position += match q.side {
                    OrderSide::Buy => q.qty,
                    OrderSide::Sell => -q.qty,
                };
                stats.record_fill(q.side, q.price, q.qty, mark);
                let performance_fill = if let Some(performance) = ledger.performance_mut() {
                    let side_bit = u64::from(q.side == OrderSide::Sell);
                    let synthetic_id =
                        (1_u64 << 63) | (cycle << 32) | (u64::from(q.level) << 1) | side_bit;
                    performance.record_fill(maker::PerformanceFill {
                        trade_id: synthetic_id,
                        order_id: synthetic_id,
                        role: maker::FillRole::PassiveMaker,
                        side: q.side,
                        price: q.price,
                        qty: q.qty,
                        mark_at_fill: mark,
                        event_time_ms: performance_time_ms,
                        // Paper simulation has no venue fee model. Preserve
                        // that gap instead of silently assuming zero cost.
                        costs: None,
                    })
                } else {
                    Ok(false)
                };
                if let Err(error) = performance_fill {
                    eprintln!("⚠️ maker performance observation disabled: {error}");
                    ledger.disable_performance();
                }
                fills.push(MakerFill {
                    side: q.side,
                    price: q.price,
                    qty: q.qty,
                    mark_at_fill: mark,
                    event_time_ms: performance_time_ms,
                    trade_id: None,
                    order_id: None,
                    trade_ts: None,
                    origin: "paper",
                    role: maker::FillRole::PassiveMaker,
                    costs: None,
                });
            } else {
                i += 1;
            }
        }
        position = *sim_position;
    }

    // 2b. Armed account hard floor (stage 5-b): fail closed before planning.
    // Everything above is reads and accounting; returning here guarantees the
    // breached cycle writes no orders at all, leaving shutdown cleanup as the
    // only remaining order traffic.
    if let Some(reason) = account_floor_stop_reason {
        return Err(anyhow::Error::new(reason));
    }

    // 3. Build the pure quote/exit plan from the synchronized state.
    let size_skew_decision = size_skew_controller.observe(position, cfg);
    let previous_guard_side = guard_controller.endangered();
    let guard_decision = guard_controller.observe(external_divergence);
    if guard_decision.endangered != previous_guard_side {
        emit_guard_transition(output_format, symbol, cycle, &guard_decision);
    }
    let active_resting = if live {
        projected_resting.as_slice()
    } else {
        resting.as_slice()
    };
    let pending_slots = account_projection
        .as_deref()
        .map(|projection| projection.pending_places())
        .unwrap_or_default()
        .iter()
        .map(|place| (place.side, place.level))
        .collect::<Vec<_>>();
    let plan = maker::plan_cycle(
        cfg,
        CycleInput {
            cycle,
            market,
            position,
            resting: active_resting,
            pending_slots: &pending_slots,
            market_data_mode,
            active_exit_enabled: live,
            inventory_exit_pct,
            inventory_exit_qty,
            size_skew: size_skew_decision,
            nonlinear_skew,
            guard: guard_decision,
            wind_down,
            qty_tolerance,
        },
        halted,
    );
    let raw_inventory_exit = plan.requested_inventory_exit;
    if exit_fill_observed {
        *inventory_exit_pending = false;
    }
    if raw_inventory_exit.is_none() {
        *inventory_exit_pending = false;
    }
    // A still-unconfirmed exit must never be duplicated, but waiting for its
    // venue confirmation is a normal cycle outcome rather than a failure:
    // suppress all new order work for this cycle and let the cycle complete
    // so the cycle_summary sequence stays gap-free for run-manifest
    // validation.
    let exit_awaiting_confirmation = raw_inventory_exit.is_some() && *inventory_exit_pending;

    let create_orders_allowed = market_data_mode == maker::MarketDataMode::Active
        && order_creation_allowed(live, rest_position_recheck_pending);
    let inventory_exit = if create_orders_allowed && !exit_awaiting_confirmation {
        plan.inventory_exit
    } else {
        None
    };
    // The pure reconciler intentionally knows nothing about transport state.
    // Remove desired placements whose slots are still reserved by an HTTP
    // submission before both execution and telemetry, so output never claims
    // a duplicate place occurred.
    let actions: Vec<Action> = plan
        .actions
        .into_iter()
        .filter(|action| match action {
            // While awaiting exit confirmation this cycle performs no order
            // work at all: no duplicate exit, no quote churn.
            _ if exit_awaiting_confirmation => false,
            Action::Place(_) if !create_orders_allowed => false,
            Action::Place(q)
                if live
                    && maker::pending_covers_slot(
                        account_projection
                            .as_deref()
                            .into_iter()
                            .flat_map(|projection| projection.pending_places())
                            .map(|place| maker::QuoteSlot {
                                side: place.side,
                                level: place.level,
                            }),
                        q.side,
                        q.level,
                    ) =>
            {
                log_maker_event(MakerLogEvent {
                    output_format,
                    symbol,
                    cycle,
                    action: "place_pending",
                    side: q.side,
                    level: q.level,
                    price: q.price,
                    price_decimals: cfg.price_decimals,
                    detail: "awaiting asynchronous order confirmation",
                    exit_kind: None,
                });
                false
            }
            _ => true,
        })
        .collect();

    // The pure planner provides the anti-flicker anchor for new placements.
    let ref_center = plan.ref_center;

    // 4. Execute. A socket-write failure propagates toward the fail-safe;
    // business acceptance/rejection is handled later through the correlated
    // order-response stream.
    let mut places: u64 = 0;
    let mut cancels: u64 = 0;
    let mut holds: u64 = 0;
    for action in &actions {
        match action {
            Action::Cancel {
                order_id,
                side,
                level,
                price,
                ..
            } => {
                if live {
                    ensure_live_streams_healthy(account_stream_health, order_response_health)?;
                    if let Some(id) = order_id {
                        ensure_request_registry_capacity(account_projection.as_deref())?;
                        let order_id = id.parse::<u64>().map_err(|_| {
                            anyhow::anyhow!(
                                "projected maker order has non-integer exchange ID '{id}'"
                            )
                        })?;
                        let commands = live_order_commands(order_commands)?;
                        let command = commands.prepare_cancel_order(id)?;
                        let request_id = command.request_id().to_string();
                        let projection = account_projection
                            .as_deref_mut()
                            .expect("live maker cycles require initialized account projection");
                        apply_request_submission(
                            projection,
                            AccountProjectionEvent::CancelSubmitted(ProjectionPendingCancel {
                                request_id: request_id.clone(),
                                order_id,
                                side: *side,
                                level: *level,
                                price: *price,
                                cycle,
                            }),
                        )?;
                        order_request_deadlines
                            .as_deref_mut()
                            .expect("live maker cycles require initialized request deadlines")
                            .record(request_id.clone(), OrderRequestKind::Cancel, Instant::now());
                        register_order_latency(
                            &mut order_latency,
                            LatencyRegistration {
                                started: latency_started,
                                request_id: &request_id,
                                kind: maker::LatencyRequestKind::Cancel,
                                generation: projection.generation(),
                                cycle,
                                symbol,
                                side: *side,
                                level: *level,
                                order_id: Some(order_id),
                                market_source,
                                recovery,
                            },
                        );
                        let sent = commands.send_prepared(command).await;
                        observe_order_write(
                            &mut order_latency,
                            latency_started,
                            &request_id,
                            sent.is_ok(),
                        );
                        sent?;
                        cancels += 1;
                    }
                } else {
                    resting.retain(|r| !(r.side == *side && r.level == *level));
                    cancels += 1;
                }
            }
            Action::Place(q) => {
                if live {
                    ensure_live_streams_healthy(account_stream_health, order_response_health)?;
                    ensure_request_registry_capacity(account_projection.as_deref())?;
                    let cl_ord_id =
                        maker::quote_client_order_id(run_order_prefix, cycle, q.side, q.level);
                    let commands = live_order_commands(order_commands)?;
                    let command = commands.prepare_create_order(&CreateOrderParams {
                        symbol: symbol.to_string(),
                        cl_ord_id: Some(cl_ord_id.clone()),
                        side: q.side,
                        order_type: OrderType::Limit,
                        quantity: format_decimals(q.qty, cfg.qty_decimals),
                        price: Some(format_decimals(q.price, cfg.price_decimals)),
                        // Post-only: reject instead of taking if the
                        // price would cross by arrival time.
                        time_in_force: Some(TimeInForce::Alo),
                        reduce_only: false,
                        stop_price: None,
                        sl_price: None,
                        tp_price: None,
                    })?;
                    let request_id = command.request_id().to_string();
                    let projection = account_projection
                        .as_deref_mut()
                        .expect("live maker cycles require initialized account projection");
                    apply_request_submission(
                        projection,
                        AccountProjectionEvent::PlaceSubmitted(ProjectionPendingPlace {
                            request_id: request_id.clone(),
                            client_order_id: cl_ord_id,
                            side: q.side,
                            price: q.price,
                            qty: q.qty,
                            level: q.level,
                            ref_center,
                            cycle,
                        }),
                    )?;
                    order_request_deadlines
                        .as_deref_mut()
                        .expect("live maker cycles require initialized request deadlines")
                        .record(request_id.clone(), OrderRequestKind::Place, Instant::now());
                    register_order_latency(
                        &mut order_latency,
                        LatencyRegistration {
                            started: latency_started,
                            request_id: &request_id,
                            kind: maker::LatencyRequestKind::Place,
                            generation: projection.generation(),
                            cycle,
                            symbol,
                            side: q.side,
                            level: q.level,
                            order_id: None,
                            market_source,
                            recovery,
                        },
                    );
                    let sent = commands.send_prepared(command).await;
                    observe_order_write(
                        &mut order_latency,
                        latency_started,
                        &request_id,
                        sent.is_ok(),
                    );
                    sent?;
                    places += 1;
                } else {
                    resting.push(RestingQuote {
                        order_id: None,
                        side: q.side,
                        level: q.level,
                        price: q.price,
                        qty: q.qty,
                        ref_center,
                        placed_at_cycle: cycle,
                    });
                    places += 1;
                }
            }
            Action::Hold { .. } => holds += 1,
        }
    }

    // Stage 5-b telemetry: which exit policy wanted an order this cycle, and
    // whether it was submitted or suppressed. `kind` covers the suppressed
    // case too (an inactive market never reaches `requested_inventory_exit`).
    let mut exit_status = ExitStatus {
        kind: raw_inventory_exit
            .as_ref()
            .map(|exit| exit.kind)
            .or_else(|| plan.exit_suppression.map(|suppressed| suppressed.kind)),
        submitted: false,
        suppressed: plan.exit_suppression,
    };

    if let Some(exit) = inventory_exit {
        // Do not race a reduce-only market order against quote cancellations.
        // The next cycle must observe an empty maker book before the single
        // exit request can be submitted.
        let account_clear = account_projection.as_deref().is_some_and(|projection| {
            projection.resting_quotes().is_empty()
                && projection.pending_places().is_empty()
                && projection.pending_cancels().is_empty()
        });
        if account_clear {
            ensure_live_streams_healthy(account_stream_health, order_response_health)?;
            ensure_request_registry_capacity(account_projection.as_deref())?;
            let cl_ord_id = maker::exit_client_order_id(run_order_prefix, cycle);
            let commands = live_order_commands(order_commands)?;
            let command = commands.prepare_create_order(&CreateOrderParams {
                symbol: symbol.to_string(),
                cl_ord_id: Some(cl_ord_id.clone()),
                side: exit.side,
                order_type: OrderType::Market,
                quantity: format_decimals(exit.qty, cfg.qty_decimals),
                price: None,
                time_in_force: None,
                reduce_only: true,
                stop_price: None,
                sl_price: None,
                tp_price: None,
            })?;
            let request_id = command.request_id().to_string();
            // Register the exit submission so its asynchronous ack correlates
            // to a pending entry instead of counting as an unmatched response.
            // The sentinel level keeps it out of quote-slot reservation; a
            // reduce-only market order never rests, so its request lifecycle
            // stays tracked until a correlated response/account event or
            // explicit cleanup resolves it.
            let projection = account_projection
                .as_deref_mut()
                .expect("live inventory exits require initialized account projection");
            apply_request_submission(
                projection,
                AccountProjectionEvent::PlaceSubmitted(ProjectionPendingPlace {
                    request_id: request_id.clone(),
                    client_order_id: cl_ord_id,
                    side: exit.side,
                    price: mark,
                    qty: exit.qty,
                    level: u32::MAX,
                    ref_center: mark,
                    cycle,
                }),
            )?;
            order_request_deadlines
                .expect("live inventory exits require initialized request deadlines")
                .record(
                    request_id.clone(),
                    OrderRequestKind::InventoryExit,
                    Instant::now(),
                );
            register_order_latency(
                &mut order_latency,
                LatencyRegistration {
                    started: latency_started,
                    request_id: &request_id,
                    kind: maker::LatencyRequestKind::Place,
                    generation: projection.generation(),
                    cycle,
                    symbol,
                    side: exit.side,
                    level: u32::MAX,
                    order_id: None,
                    market_source,
                    recovery,
                },
            );
            let sent = commands.send_prepared(command).await;
            observe_order_write(
                &mut order_latency,
                latency_started,
                &request_id,
                sent.is_ok(),
            );
            sent?;
            *inventory_exit_pending = true;
            exit_status.submitted = true;
            log_maker_event(MakerLogEvent {
                output_format,
                symbol,
                cycle,
                action: "inventory_exit_submitted",
                side: exit.side,
                level: 0,
                price: mark,
                price_decimals: cfg.price_decimals,
                detail: "reduce-only market order submitted after maker book cleared",
                exit_kind: Some(exit.kind.as_str()),
            });
        }
    }

    // 5. Telemetry uses exact ledger fills in live mode and simulated fills
    // in paper mode; never infer a fill from a position delta.
    let final_resting = if live {
        account_projection
            .as_deref()
            .map(|projection| projection.resting_quotes())
            .unwrap_or_default()
    } else {
        resting.clone()
    };
    let two_sided = final_resting.iter().any(|r| r.side == OrderSide::Buy)
        && final_resting.iter().any(|r| r.side == OrderSide::Sell);
    stats.end_cycle(position, two_sided);
    let quote_observation = if let Some(performance) = ledger.performance_mut() {
        let (eligible_bid_qty, eligible_ask_qty) =
            eligible_quote_qty(&final_resting, mark, cfg.band_bps);
        performance.observe_quote_quality(maker::QuoteQualityInterval {
            event_time_ms: performance_time_ms,
            eligible_bid_qty,
            eligible_ask_qty,
        })
    } else {
        Ok(())
    };
    if let Err(error) = quote_observation {
        eprintln!("⚠️ maker performance observation disabled: {error}");
        ledger.disable_performance();
    }
    let performance_summary = match ledger
        .performance()
        .map(|performance| performance.summary(mark))
        .transpose()
    {
        Ok(summary) => summary,
        Err(error) => {
            eprintln!("⚠️ maker performance summary disabled: {error}");
            ledger.disable_performance();
            None
        }
    };

    // 6. Emit.
    emit_maker_cycle(CycleOutput {
        output_format,
        live,
        symbol,
        cycle,
        mark,
        best_bid,
        best_ask,
        market_source,
        market_fallback_reason,
        ws_snapshot,
        position,
        starting_position,
        account: account_balance.as_ref(),
        actions: &actions,
        fills: &fills,
        stats,
        halt_vol_bps: halted.then(|| breaker.vol_bps()),
        spread_decision: &spread_decision,
        size_skew_decision: &size_skew_decision,
        guard_decision: &guard_decision,
        external_basis_bps,
        skew_shift_bps: if mark > 0.0 {
            (mark - ref_center) / mark * 1e4
        } else {
            0.0
        },
        exit_status,
        cfg,
        performance: performance_summary.as_ref(),
    });

    Ok(CycleResult {
        places,
        cancels,
        holds,
        fills: fills.len() as u64,
        balance: account_balance,
    })
}

fn eligible_quote_qty(resting: &[RestingQuote], mark: f64, band_bps: f64) -> (f64, f64) {
    let band = mark * band_bps / 10_000.0;
    resting
        .iter()
        .filter(|quote| (quote.price - mark).abs() <= band + f64::EPSILON)
        .fold((0.0, 0.0), |mut qty, quote| {
            match quote.side {
                OrderSide::Buy => qty.0 += quote.qty,
                OrderSide::Sell => qty.1 += quote.qty,
            }
            qty
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trade(side: Option<&str>, price: &str, qty: &str) -> Trade {
        Trade {
            id: 42,
            time: "2026-07-10T00:00:00Z".to_string(),
            price: price.to_string(),
            qty: qty.to_string(),
            side: side.map(str::to_string),
            is_buyer_taker: false,
            fee_asset: None,
            fee_qty: None,
            pnl: None,
            order_id: Some(7),
            symbol: Some("BTC-USD".to_string()),
            value: None,
        }
    }

    fn balance(equity: &str, cross_available: &str) -> Balance {
        Balance {
            balance: "100".to_string(),
            cross_available: cross_available.to_string(),
            cross_balance: "100".to_string(),
            cross_margin: "0".to_string(),
            cross_upnl: "0".to_string(),
            equity: equity.to_string(),
            isolated_balance: "0".to_string(),
            isolated_upnl: "0".to_string(),
            locked: "0".to_string(),
            pnl_24h: "0".to_string(),
            pnl_freeze: "0".to_string(),
            upnl: "0".to_string(),
        }
    }

    /// Stage 5-b: the default (disarmed) floors must never stop a run, no
    /// matter how broken or stale the balance is. Every other maker session
    /// depends on this staying true.
    #[test]
    fn disarmed_account_floors_never_stop_the_cycle() {
        let fresh = std::time::Duration::from_secs(0);
        let ancient = BALANCE_FLOOR_MAX_AGE + std::time::Duration::from_secs(600);
        for age in [fresh, ancient] {
            assert!(account_floor_stop(&balance("100", "90"), age, 0.0, 0.0).is_none());
            assert!(account_floor_stop(&balance("-5", "-5"), age, 0.0, 0.0).is_none());
            assert!(account_floor_stop(&balance("oops", "oops"), age, 0.0, 0.0).is_none());
        }
    }

    /// An armed floor is enforced against a *fresh* balance, and refuses to
    /// read a stale or unparseable one as "no breach" — the whole point of a
    /// solvency brake is that it fails closed.
    #[test]
    fn armed_account_floor_breaches_and_fails_closed() {
        use super::super::model::AccountFloorCause;
        let fresh = std::time::Duration::from_secs(1);

        assert!(account_floor_stop(&balance("100", "90"), fresh, 90.0, 0.0).is_none());

        let breach = account_floor_stop(&balance("89.5", "90"), fresh, 90.0, 0.0)
            .expect("equity below the armed floor stops the cycle");
        assert_eq!(breach.cause, AccountFloorCause::Breach);
        assert_eq!(breach.metric, "equity");
        assert_eq!(breach.observed, Some(89.5));
        assert_eq!(breach.floor, Some(90.0));
        assert_eq!(breach.cause.event(), "triggered");

        let margin = account_floor_stop(&balance("100", "19"), fresh, 0.0, 20.0)
            .expect("margin below the armed floor stops the cycle");
        assert_eq!(margin.metric, "margin");

        // Stale: an old snapshot is not evidence of solvency.
        let stale = account_floor_stop(
            &balance("100", "90"),
            BALANCE_FLOOR_MAX_AGE + std::time::Duration::from_secs(1),
            90.0,
            0.0,
        )
        .expect("a stale balance cannot clear an armed floor");
        assert_eq!(stale.cause, AccountFloorCause::BalanceStale);
        assert_eq!(stale.cause.event(), "unevaluable");
        // Exactly at the limit is still usable.
        assert!(
            account_floor_stop(&balance("100", "90"), BALANCE_FLOOR_MAX_AGE, 90.0, 0.0).is_none()
        );

        // Unparseable / non-finite fields the armed floor actually reads.
        for raw in ["", "n/a", "NaN"] {
            let unreadable = account_floor_stop(&balance(raw, "90"), fresh, 90.0, 0.0)
                .expect("an unreadable equity cannot clear an armed equity floor");
            assert_eq!(unreadable.cause, AccountFloorCause::BalanceUnreadable);
        }
        // …but a broken field the armed floor does NOT read is not its problem.
        assert!(account_floor_stop(&balance("100", "oops"), fresh, 90.0, 0.0).is_none());
        assert!(account_floor_stop(&balance("oops", "90"), fresh, 0.0, 20.0).is_none());
    }

    #[test]
    fn latency_registration_preserves_recovery_classification() {
        let mut tracker = OrderLatencyTracker::default();
        let started = Instant::now();
        let mut tracker_ref = Some(&mut tracker);
        register_order_latency(
            &mut tracker_ref,
            LatencyRegistration {
                started: Some(started),
                request_id: "recovery-place",
                kind: maker::LatencyRequestKind::Place,
                generation: 7,
                cycle: 11,
                symbol: "BTC-USD",
                side: OrderSide::Buy,
                level: 0,
                order_id: None,
                market_source: "ws",
                recovery: true,
            },
        );

        let request = tracker.requests().next().expect("registered request");
        assert!(request.context.recovery);
        assert_eq!(request.context.generation, 7);
        assert_eq!(request.context.market_source.as_deref(), Some("ws"));
    }

    #[test]
    fn rest_position_recheck_blocks_only_live_order_creation() {
        assert!(!order_creation_allowed(true, true));
        assert!(order_creation_allowed(true, false));
        assert!(order_creation_allowed(false, true));
    }

    #[test]
    fn maker_trade_fill_requires_complete_venue_fields() {
        assert_eq!(
            maker_trade_fill(&trade(Some("buy"), "99.5", "0.02")).unwrap(),
            (OrderSide::Buy, 99.5, 0.02)
        );
        assert!(maker_trade_fill(&trade(None, "99.5", "0.02"))
            .unwrap_err()
            .to_string()
            .contains("valid side"));
        assert!(maker_trade_fill(&trade(Some("sell"), "bad", "0.02"))
            .unwrap_err()
            .to_string()
            .contains("invalid price"));
    }

    #[test]
    fn current_run_fill_is_recorded_once_with_trade_identity() {
        let trade = trade(Some("buy"), "59.50", "0.20");
        let start = chrono::DateTime::parse_from_rfc3339("2026-07-10T00:00:00Z")
            .unwrap()
            .timestamp();
        let mut stats = MakerStats::default();
        let mut ledger = MakerLedger::new(0.0);
        ledger.maker_order_ids.insert(7);
        let mut fills = Vec::new();

        collect_current_run_fills(
            vec![trade.clone()],
            &mut ledger,
            start,
            start + 60,
            59.50,
            &mut stats,
            &mut fills,
        )
        .unwrap();
        collect_current_run_fills(
            vec![trade],
            &mut ledger,
            start,
            start + 60,
            59.50,
            &mut stats,
            &mut fills,
        )
        .unwrap();

        assert_eq!(stats.fills(), 1);
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].trade_id, Some(42));
        assert_eq!(fills[0].order_id, Some(7));
        assert_eq!(fills[0].origin, "current_run_rest_trade");
        assert!((ledger.expected_position - 0.2).abs() < 1e-9);
    }

    fn order_update(fill_qty: &str, avg: &str) -> OrderUpdate {
        OrderUpdate {
            seq: 10,
            order_id: 7,
            cl_ord_id: Some("sxmk-0123456789ab-q00000001b0".to_string()),
            symbol: "BTC-USD".to_string(),
            side: OrderSide::Buy,
            qty: "0.20".to_string(),
            fill_qty: fill_qty.to_string(),
            fill_avg_price: avg.to_string(),
            price: "59.50".to_string(),
            status: standx_sdk::models::OrderStatus::PartiallyFilled,
            reduce_only: false,
            updated_at: "2026-07-10T00:00:01Z".to_string(),
        }
    }

    fn account_trade(side: OrderSide, qty: &str) -> TradeUpdate {
        TradeUpdate {
            seq: 11,
            trade_id: 42,
            order_id: 7,
            symbol: "BTC-USD".to_string(),
            side,
            price: "59.50".to_string(),
            qty: qty.to_string(),
            trade_ts: "2026-07-10T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn websocket_then_rest_trade_is_not_double_counted() {
        let start = chrono::DateTime::parse_from_rfc3339("2026-07-10T00:00:00Z")
            .unwrap()
            .timestamp();
        let mut ledger = MakerLedger::new(0.0);
        let mut stats = MakerStats::default();
        let mut fills = Vec::new();
        apply_order_update(
            &mut ledger,
            &order_update("0.20", "59.50"),
            "BTC-USD",
            "sxmk-0123456789ab-",
            &mut stats,
            &mut fills,
        )
        .unwrap();
        apply_account_trade(
            &mut ledger,
            account_trade(OrderSide::Buy, "0.20"),
            "BTC-USD",
            59.50,
            &mut stats,
            &mut fills,
        )
        .unwrap();
        collect_current_run_fills(
            vec![trade(Some("buy"), "59.50", "0.20")],
            &mut ledger,
            start,
            start + 60,
            59.50,
            &mut stats,
            &mut fills,
        )
        .unwrap();
        assert_eq!(stats.fills(), 1);
        assert_eq!(fills.len(), 1);
        assert!((ledger.expected_position - 0.20).abs() < 1e-9);
    }

    #[test]
    fn rest_then_websocket_trade_is_not_double_counted() {
        let start = chrono::DateTime::parse_from_rfc3339("2026-07-10T00:00:00Z")
            .unwrap()
            .timestamp();
        let mut ledger = MakerLedger::new(0.0);
        ledger.maker_order_ids.insert(7);
        let mut stats = MakerStats::default();
        let mut fills = Vec::new();
        collect_current_run_fills(
            vec![trade(Some("buy"), "59.50", "0.20")],
            &mut ledger,
            start,
            start + 60,
            59.50,
            &mut stats,
            &mut fills,
        )
        .unwrap();
        apply_account_trade(
            &mut ledger,
            account_trade(OrderSide::Buy, "0.20"),
            "BTC-USD",
            59.50,
            &mut stats,
            &mut fills,
        )
        .unwrap();
        assert_eq!(stats.fills(), 1);
        assert_eq!(fills.len(), 1);
        assert!((ledger.expected_position - 0.20).abs() < 1e-9);
    }

    #[test]
    fn historical_trade_without_current_run_order_is_ignored() {
        let mut stats = MakerStats::default();
        let mut fills = Vec::new();
        let mut ledger = MakerLedger::new(-0.13);
        collect_current_run_fills(
            vec![trade(Some("sell"), "59.50", "0.20")],
            &mut ledger,
            1_783_000_000,
            1_784_000_000,
            59.50,
            &mut stats,
            &mut fills,
        )
        .unwrap();
        assert_eq!(stats.fills(), 0);
        assert!(fills.is_empty());
        assert_eq!(ledger.expected_position, -0.13);
    }

    #[test]
    fn current_run_trade_outside_session_is_rejected() {
        let mut stats = MakerStats::default();
        let mut fills = Vec::new();
        let mut ledger = MakerLedger::new(0.0);
        ledger.maker_order_ids.insert(7);
        let error = collect_current_run_fills(
            vec![trade(Some("buy"), "59.50", "0.20")],
            &mut ledger,
            1_783_700_000,
            1_783_700_100,
            59.50,
            &mut stats,
            &mut fills,
        )
        .unwrap_err();
        assert!(error.to_string().contains("outside the session"));
    }

    #[test]
    fn current_run_client_order_ids_are_bounded_and_scoped() {
        let prefix = "sxmk-0123456789ab-";
        let quote = maker::quote_client_order_id(prefix, u64::MAX, OrderSide::Sell, u32::MAX);
        let exit = maker::exit_client_order_id(prefix, u64::MAX);
        assert!(quote.starts_with(prefix));
        assert!(exit.starts_with(prefix));
        assert!(quote.len() <= 41, "{quote}");
        assert!(exit.len() <= 41, "{exit}");
    }
}
