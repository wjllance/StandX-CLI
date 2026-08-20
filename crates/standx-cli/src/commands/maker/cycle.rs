use super::ledger::{adopt_order, apply_funding_history, apply_rest_trade};
#[cfg(test)]
use super::ledger::{apply_account_trade, apply_order_update, maker_trade_fill};
use super::model::{
    optional_decimal, position_for_symbol, rest_order_observation, unhealthy_stream, Decimal,
};
use super::output::{
    emit_cycle_skip, emit_external_skew_transition, emit_guard_transition, emit_maker_cycle,
    log_maker_event, CycleOutput, ExitStatus, MakerLogEvent,
};
use super::pipeline::{
    fetch_account_audit, CycleRequest, CycleResult, CycleState, OrderRequestKind,
    TimedExternalDivergence, BALANCE_FLOOR_MAX_AGE, FUNDING_HISTORY_LIMIT,
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

/// Execution-layer tracking for an in-flight `InventoryTrim` Alo/Ioc exit
/// order (stage 8, docs/33-maker-exit-execution-cost-design.md).
///
/// Deliberately minimal: `resting_price` and the resting order's venue
/// `order_id` are *not* cached here — they are read fresh from the account
/// projection's `resting_quotes()` every cycle, which is the authoritative
/// source (see docs/33 structural problem #2). Only `phase` and
/// `cycles_in_phase` genuinely cannot be recovered from venue-observed state
/// alone: an `Ioc` leg never rests, so nothing distinguishes "about to open
/// the first Alo attempt" from "already escalated to Ioc" once the book goes
/// quiet, and `cycles_in_phase` is a total-time-in-Alo budget that a
/// `RepriceAlo` must not silently reset.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct InventoryExitOrderTracking {
    pub(super) phase: maker::ExitPhase,
    pub(super) cycles_in_phase: u32,
}

fn external_skew_transitioned(previous: f64, current: f64) -> bool {
    let previous_active = previous != 0.0;
    let current_active = current != 0.0;
    previous_active != current_active
        || (previous_active
            && current_active
            && previous.is_sign_positive() != current.is_sign_positive())
}

fn legacy_inventory_skew_shift_bps(mark: f64, plan: &maker::CyclePlan) -> f64 {
    if mark > 0.0 {
        (mark - plan.inventory_ref_center) / mark * 1e4
    } else {
        0.0
    }
}

fn external_excess_at_plan(
    observation: Option<TimedExternalDivergence>,
    decision: &maker::GuardDecision,
    max_age_ms: u64,
    now: Instant,
) -> (Option<maker::ExternalDivergence>, Option<f64>) {
    let fresh_observation = observation
        .map(|sample| sample.at(now))
        .filter(|sample| sample.age_ms <= max_age_ms && sample.divergence_bps.is_finite());
    let excess_bps =
        fresh_observation.and_then(|_| decision.divergence_bps.filter(|value| value.is_finite()));
    (fresh_observation, excess_bps)
}

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

fn ensure_live_streams_healthy(
    account_health: Option<&AccountStreamHealth>,
    order_health: Option<&OrderResponseHealth>,
) -> Result<()> {
    let unusable = unhealthy_stream(account_health).or_else(|| unhealthy_stream(order_health));
    match unusable {
        Some(reason) => Err(anyhow::anyhow!(
            "{reason}; refusing further live order actions"
        )),
        None => Ok(()),
    }
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
    // A floor's own reading must be usable; an unparseable field is reported as
    // unevaluable below rather than silently treated as "no breach".
    let equity = optional_decimal(&balance.equity, Decimal::Finite);
    let available = optional_decimal(&balance.cross_available, Decimal::Finite);
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
        market_telemetry,
        max_divergence_bps,
        inventory_exit_pct,
        inventory_exit_qty,
        inventory_exit_cfg,
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
        inventory_exit_order,
        ledger,
        sim_position,
        stats,
        breaker,
        spread_controller,
        size_skew_controller,
        nonlinear_skew,
        external_skew,
        microprice,
        external_skew_previous_shift_bps,
        external_excess_telemetry,
        guard_controller,
        external_divergence,
        external_basis_bps,
        mut order_request_deadlines,
        live_account_poll,
        mut order_latency,
        latency_started,
    } = state;
    use maker::{format_decimals, quote_crosses_touch, Action, CycleInput, MarketSnapshot};

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
        // Every timestamp below is taken AFTER the joined I/O, not from the
        // pre-flight `poll_now`. A stalled audit call can hold this join open
        // for seconds; dating the balance by when the request was *issued*
        // would report an age smaller than the real one, which is exactly the
        // direction that lets an armed hard floor read a too-old snapshot as
        // "no breach". `poll_now` stays what it is used for — deciding whether
        // the reads were due in the first place.
        let settled = std::time::Instant::now();
        // Resolve every due read before mutating the current-run ledger. A
        // failed audit must leave this cycle's accounting exactly untouched.
        let audit = match audit {
            Some(audit) => Some(audit?),
            None => None,
        };
        if let Some(refreshed_balance) = refreshed_balance {
            match refreshed_balance {
                Ok(balance) => poll.record_balance_refresh(balance, settled),
                Err(error) => {
                    poll.record_balance_refresh_failure(settled);
                    if !poll.balance_is_within_stale_limit(settled) {
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
            poll.balance_age(std::time::Instant::now()),
            stop_equity_below,
            stop_margin_below,
        );

        if let Some(audit) = audit {
            // Funding first: it only touches the performance ledger's cashflow
            // accumulator, never positions or orders, so it cannot disturb the
            // reconciliation below. A funding problem never propagates as a
            // cycle error either — it marks the attribution incomplete instead,
            // because failing a safety reconciliation over telemetry would be a
            // severity inversion.
            match &audit.funding {
                Ok(rows) => {
                    if rows.len() as u32 >= FUNDING_HISTORY_LIMIT {
                        // No silent caps: a full page means older funding since
                        // session start may have been cut off.
                        eprintln!(
                            "⚠️  funding history page is full ({} rows); funding coverage may be truncated",
                            rows.len()
                        );
                        if let Some(performance) = ledger.performance_mut() {
                            performance.record_funding_coverage_gap();
                        }
                    }
                    apply_funding_history(
                        ledger,
                        rows,
                        symbol,
                        session_started_at,
                        poll.applied_funding_ids(),
                    )?;
                }
                Err(error) => {
                    eprintln!("⚠️  funding history unavailable this audit: {error}");
                    if let Some(performance) = ledger.performance_mut() {
                        performance.record_funding_coverage_gap();
                    }
                }
            }
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
            if quote_crosses_touch(resting[i].side, resting[i].price, best_bid, best_ask) {
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
    let guard_observed_at = Instant::now();
    let guard_max_age_ms = guard_controller.config().max_age_ms;
    // Preserve the established guard action path exactly: it consumes the age
    // normalized before maker_cycle just as it did before external skew. The
    // new shift/fill fields additionally account for I/O elapsed since that
    // normalization, so a newly introduced center offset never uses a sample
    // that is stale at planning time.
    let guard_input = external_divergence.map(|sample| sample.value);
    let previous_guard_side = guard_controller.endangered();
    let guard_decision = guard_controller.observe(guard_input);
    let (fresh_external_divergence, external_excess_bps) = external_excess_at_plan(
        external_divergence,
        &guard_decision,
        guard_max_age_ms,
        guard_observed_at,
    );
    external_excess_telemetry.observe(
        external_excess_bps,
        fresh_external_divergence,
        guard_max_age_ms,
        guard_observed_at,
    );
    if guard_decision.endangered != previous_guard_side {
        emit_guard_transition(output_format, symbol, cycle, &guard_decision);
    }
    // Stage 8 (docs/33 structural problem #1): the reduce-only exit order
    // (Market, or a resting Alo/Ioc leg) is never a maker-ladder quote, so it
    // must never reach the pure reconciler's `resting`/`pending_slots`
    // inputs. `reconcile` cancels any resting quote with no matching desired
    // quote at its (side, level) — and while an exit is active, `desired` is
    // always empty (`plan_cycle` suppresses new quotes whenever
    // `inventory_exit.is_some()`), so an un-filtered exit-level entry would
    // be cancelled by `reconcile` as `Stale` every single cycle, racing the
    // dedicated exit-management logic below that also owns that order's
    // cancel/replace lifecycle. Filtering here keeps `plan_cycle` exactly as
    // ignorant of exit execution as it always was; `final_resting` below
    // (telemetry/uptime) intentionally still sees the exit order, since a
    // resting Alo leg is genuine SIP-5A-eligible maker liquidity.
    let active_resting: Vec<RestingQuote> = if live {
        planner_resting(&projected_resting)
    } else {
        resting.clone()
    };
    let pending_slots = planner_pending_slots(
        &account_projection
            .as_deref()
            .map(|projection| projection.pending_places())
            .unwrap_or_default(),
    );
    let plan = maker::plan_cycle(
        cfg,
        CycleInput {
            cycle,
            market,
            position,
            resting: &active_resting,
            pending_slots: &pending_slots,
            market_data_mode,
            active_exit_enabled: live,
            inventory_exit_pct,
            inventory_exit_qty,
            size_skew: size_skew_decision,
            nonlinear_skew,
            external_skew,
            external_excess_bps,
            micro_price: microprice,
            guard: guard_decision,
            wind_down,
            qty_tolerance,
        },
        halted,
    );
    let external_skew_shift_bps = plan.external_skew_shift_bps;
    let micro_price_shift_bps = plan.micro_price_shift_bps;
    let inventory_skew_shift_bps = legacy_inventory_skew_shift_bps(mark, &plan);
    let quote_geometry = plan.quote_geometry;
    if external_skew_transitioned(*external_skew_previous_shift_bps, external_skew_shift_bps) {
        emit_external_skew_transition(
            output_format,
            symbol,
            cycle,
            external_skew_shift_bps,
            external_excess_bps,
        );
    }
    *external_skew_previous_shift_bps = external_skew_shift_bps;
    let raw_inventory_exit = plan.requested_inventory_exit;
    if exit_fill_observed {
        *inventory_exit_pending = false;
    }
    if raw_inventory_exit.is_none() {
        *inventory_exit_pending = false;
        // Stage 8 (docs/33): nothing is being requested anymore (flattened,
        // or gated off by a halt/inactive market) — drop any Alo/Ioc
        // tracking so a later new exit starts clean instead of resuming a
        // stale phase.
        *inventory_exit_order = None;
    }
    // Stage 8 (docs/33 structural problem #1, second half): `inventory_exit_pending`
    // was designed around a reduce-only Market order, whose accept-ack and
    // resolution are effectively the same instant — so waiting for "the ack"
    // and waiting for "the exit to be done" were never actually different
    // things to wait for. A resting Alo order breaks that: it gets
    // acknowledged (and starts resting) long before it resolves, and this
    // flag must not keep the whole exit block suppressed for that entire
    // resting window, or the Alo/Ioc state machine below could never run its
    // per-cycle hold/reprice/upgrade decision. So for a tracked Alo/Ioc exit
    // specifically, treat "awaiting confirmation" as scoped to the most
    // recent wire submission only: once the account projection shows no more
    // open exit-level place/cancel request, that submission's ack has
    // landed (accepted-and-resting, terminal, or rejected) and the state
    // machine may run again. This can never fire for the legacy Market path
    // (`inventory_exit_order` stays `None` there), so the default-off
    // behavior is untouched byte-for-byte.
    if inventory_exit_order.is_some() && exit_wire_submission_settled(account_projection.as_deref())
    {
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
        if !exit_uses_alo_ioc(exit.kind, &inventory_exit_cfg) {
            // Do not race a reduce-only market order against quote
            // cancellations. The next cycle must observe an empty maker book
            // before the single exit request can be submitted.
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
                        level: maker::EXIT_ORDER_LEVEL,
                        ref_center: mark,
                        cycle,
                    }),
                )?;
                order_request_deadlines
                    .as_deref_mut()
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
                        level: maker::EXIT_ORDER_LEVEL,
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
        } else {
            // ---- Stage 8 Alo/Ioc execution-cost path (docs/33) ----
            let ordinary_book_clear = exit_order_book_clear(account_projection.as_deref());
            let resting_exit = resting_exit_order(account_projection.as_deref());
            let phase_state =
                exit_phase_state_for_cycle(inventory_exit_order.as_ref(), resting_exit.as_ref());

            let exit_actionable = phase_state.is_some() || ordinary_book_clear;
            let full_touch = best_bid.zip(best_ask);
            if exit_actionable && full_touch.is_none() {
                // `preflight_cycle`'s full-touch requirement is conditioned on
                // `live` (see its call site), so a one-sided book legitimately
                // reaches here in paper mode — which is exactly where
                // `alo_enabled` gets tried first. Take no action for this
                // cycle rather than pricing an exit blind or blind-cancelling
                // a resting leg: the exit stays requested and the next
                // coherent touch resumes the state machine. Observable so a
                // stalled exit is never silent.
                log_maker_event(MakerLogEvent {
                    output_format,
                    symbol,
                    cycle,
                    action: "inventory_exit_touch_incomplete",
                    side: exit.side,
                    level: 0,
                    price: mark,
                    price_decimals: cfg.price_decimals,
                    detail: "alo/ioc exit needs a full touch; no exit action this cycle",
                    exit_kind: Some(exit.kind.as_str()),
                });
            }
            if let Some((best_bid, best_ask)) = full_touch.filter(|_| exit_actionable) {
                let (step, next_state) = maker::plan_exit_order_step(
                    &inventory_exit_cfg,
                    phase_state,
                    exit.side,
                    best_bid,
                    best_ask,
                    cfg.price_tick(),
                    stats.loss_bps(position, mark),
                );
                *inventory_exit_order = Some(InventoryExitOrderTracking {
                    phase: next_state.phase,
                    cycles_in_phase: next_state.cycles_in_phase,
                });

                if let maker::ExitOrderStep::HoldAlo = step {
                    log_maker_event(MakerLogEvent {
                        output_format,
                        symbol,
                        cycle,
                        action: "inventory_exit_alo_held",
                        side: exit.side,
                        level: 0,
                        price: resting_exit
                            .as_ref()
                            .map(|resting| resting.price)
                            .unwrap_or(mark),
                        price_decimals: cfg.price_decimals,
                        detail: "resting Alo exit order within refresh tolerance",
                        exit_kind: Some(exit.kind.as_str()),
                    });
                } else {
                    ensure_live_streams_healthy(account_stream_health, order_response_health)?;

                    if step.cancels_resting() {
                        // `plan_exit_order_step` only returns a
                        // cancel-then-replace step when it was handed an Alo
                        // `ExitPhaseState`, and `exit_phase_state_for_cycle`
                        // only ever produces `Alo` alongside a `resting_exit`
                        // — a live venue order id is guaranteed here. Fail
                        // closed instead of silently placing a second order
                        // on top of an uncancelled one if that invariant is
                        // ever broken by a future change.
                        let resting = resting_exit.as_ref().ok_or_else(|| {
                            anyhow::anyhow!(
                                "inventory exit reprice/upgrade decided with no resting order to cancel"
                            )
                        })?;
                        let order_id_str = resting.order_id.as_deref().ok_or_else(|| {
                            anyhow::anyhow!("resting exit order is missing its venue order id")
                        })?;
                        ensure_request_registry_capacity(account_projection.as_deref())?;
                        let order_id = order_id_str.parse::<u64>().map_err(|_| {
                            anyhow::anyhow!(
                                "resting exit order has non-integer exchange ID '{order_id_str}'"
                            )
                        })?;
                        let commands = live_order_commands(order_commands)?;
                        let command = commands.prepare_cancel_order(order_id_str)?;
                        let request_id = command.request_id().to_string();
                        let projection = account_projection
                            .as_deref_mut()
                            .expect("live inventory exits require initialized account projection");
                        apply_request_submission(
                            projection,
                            AccountProjectionEvent::CancelSubmitted(ProjectionPendingCancel {
                                request_id: request_id.clone(),
                                order_id,
                                side: exit.side,
                                level: maker::EXIT_ORDER_LEVEL,
                                price: resting.price,
                                cycle,
                            }),
                        )?;
                        order_request_deadlines
                            .as_deref_mut()
                            .expect("live inventory exits require initialized request deadlines")
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
                                side: exit.side,
                                level: maker::EXIT_ORDER_LEVEL,
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
                    }

                    if let Some(price) = step.price() {
                        ensure_request_registry_capacity(account_projection.as_deref())?;
                        let time_in_force = match step {
                            maker::ExitOrderStep::UpgradeToIoc { .. }
                            | maker::ExitOrderStep::SubmitIoc { .. } => TimeInForce::Ioc,
                            _ => TimeInForce::Alo,
                        };
                        let cl_ord_id = maker::exit_client_order_id(run_order_prefix, cycle);
                        let commands = live_order_commands(order_commands)?;
                        let command = commands.prepare_create_order(&CreateOrderParams {
                            symbol: symbol.to_string(),
                            cl_ord_id: Some(cl_ord_id.clone()),
                            side: exit.side,
                            order_type: OrderType::Limit,
                            quantity: format_decimals(exit.qty, cfg.qty_decimals),
                            price: Some(format_decimals(price, cfg.price_decimals)),
                            time_in_force: Some(time_in_force),
                            reduce_only: true,
                            stop_price: None,
                            sl_price: None,
                            tp_price: None,
                        })?;
                        let request_id = command.request_id().to_string();
                        let projection = account_projection
                            .as_deref_mut()
                            .expect("live inventory exits require initialized account projection");
                        apply_request_submission(
                            projection,
                            AccountProjectionEvent::PlaceSubmitted(ProjectionPendingPlace {
                                request_id: request_id.clone(),
                                client_order_id: cl_ord_id,
                                side: exit.side,
                                price,
                                qty: exit.qty,
                                level: maker::EXIT_ORDER_LEVEL,
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
                                level: maker::EXIT_ORDER_LEVEL,
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
                        let (action, detail) = match step {
                            maker::ExitOrderStep::OpenAlo { .. } => (
                                "inventory_exit_alo_opened",
                                "Alo order rests at the touch, joining the maker queue",
                            ),
                            maker::ExitOrderStep::RepriceAlo { .. } => (
                                "inventory_exit_alo_repriced",
                                "touch drifted beyond alo_refresh_bps; cancelled and re-rested",
                            ),
                            maker::ExitOrderStep::UpgradeToIoc { .. } => (
                                "inventory_exit_upgraded_ioc",
                                "loss or alo_max_cycles threshold breached; escalated to a \
                                 crossing Ioc order \u{2014} venue acceptance of IOC on this \
                                 route is unverified, watch for a rejection",
                            ),
                            maker::ExitOrderStep::SubmitIoc { .. } => (
                                "inventory_exit_ioc_submitted",
                                "residual retry with a fresh crossing Ioc order \u{2014} venue \
                                 acceptance of IOC on this route is unverified, watch for a \
                                 rejection",
                            ),
                            maker::ExitOrderStep::HoldAlo => {
                                unreachable!("Hold never reaches a price submission")
                            }
                        };
                        log_maker_event(MakerLogEvent {
                            output_format,
                            symbol,
                            cycle,
                            action,
                            side: exit.side,
                            level: 0,
                            price,
                            price_decimals: cfg.price_decimals,
                            detail,
                            exit_kind: Some(exit.kind.as_str()),
                        });
                    }
                }
            }
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
        market_telemetry,
        quote_geometry: &quote_geometry,
        position,
        starting_position,
        account: account_balance.as_ref(),
        actions: &actions,
        fills: &fills,
        excess_bps_at_fill: external_excess_bps,
        stats,
        halt_vol_bps: halted.then(|| breaker.vol_bps()),
        spread_decision: &spread_decision,
        size_skew_decision: &size_skew_decision,
        guard_decision: &guard_decision,
        external_basis_bps,
        external_skew_shift_bps,
        micro_price_shift_bps,
        skew_shift_bps: inventory_skew_shift_bps,
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

/// docs/33 known-cost 5 / scope limit: `WindDown` never enters the Alo/Ioc
/// machine (an A/B arm must converge to flat deterministically, and an Alo
/// order may never fill), and the whole machine is off by default. Both
/// cases keep exactly the pre-stage-8 reduce-only Market path.
/// Resting quotes handed to `plan_cycle`, with the inventory exit's own order
/// removed.
///
/// The exit order is not a quote slot and `plan_cycle` has never known about
/// exit execution: while an exit is outstanding `desired` is always empty (the
/// planner suppresses quotes whenever `inventory_exit.is_some()`), so leaving
/// the exit's entry in would make `reconcile` emit a `Stale` cancel for it
/// every cycle, racing the exit logic that owns that order's lifecycle.
///
/// This also applies to the default-off (Market) exit, whose projection entry
/// sits at the same sentinel. That is harmless for the same reason — quotes are
/// suppressed for the whole time such an entry can exist — and
/// [`planner_inputs_exclude_only_the_exit_footprint`] pins it.
fn planner_resting(projected: &[RestingQuote]) -> Vec<RestingQuote> {
    projected
        .iter()
        .filter(|quote| quote.level != maker::EXIT_ORDER_LEVEL)
        .cloned()
        .collect()
}

/// Pending quote slots handed to `plan_cycle`, with the inventory exit's own
/// in-flight place removed. See [`planner_resting`].
fn planner_pending_slots(places: &[ProjectionPendingPlace]) -> Vec<(OrderSide, u32)> {
    places
        .iter()
        .filter(|place| place.level != maker::EXIT_ORDER_LEVEL)
        .map(|place| (place.side, place.level))
        .collect()
}

fn exit_uses_alo_ioc(kind: maker::ExitKind, cfg: &maker::InventoryExitConfig) -> bool {
    kind == maker::ExitKind::InventoryTrim && cfg.alo_enabled
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

/// docs/33 structural problem #1 (second half): whether the exit's most
/// recent wire submission (a place or a cancel at `EXIT_ORDER_LEVEL`) has
/// settled — accepted-and-resting, terminal, or rejected. `false` while
/// nothing is known (no projection) so a transiently-unavailable projection
/// can never be misread as "settled".
///
/// `inventory_exit_pending` predates this feature and was designed around a
/// reduce-only Market order, whose accept-ack and resolution are effectively
/// the same instant. A resting `Alo` order breaks that: it gets acknowledged
/// (and starts resting) long before it resolves. Scoping "awaiting
/// confirmation" to just the most recent submission — rather than the whole
/// exit's lifetime — is what lets the Alo/Ioc state machine run its
/// per-cycle hold/reprice/upgrade decision on every later cycle instead of
/// being permanently suppressed after the first submission.
fn exit_wire_submission_settled(projection: Option<&MakerAccountProjection>) -> bool {
    match projection {
        Some(projection) => {
            !projection
                .pending_places()
                .iter()
                .any(|place| place.level == maker::EXIT_ORDER_LEVEL)
                && !projection
                    .pending_cancels()
                    .iter()
                    .any(|cancel| cancel.level == maker::EXIT_ORDER_LEVEL)
        }
        None => false,
    }
}

/// docs/33 structural problem #1: whether the *ordinary* maker book (every
/// resting quote, pending place, and pending cancel other than the exit's own
/// `EXIT_ORDER_LEVEL` entries) is clear. Excluding the exit's own footprint is
/// what stops a resting Alo order from permanently blocking its own future
/// reprice/upgrade — the original (pre-stage-8) gate required the *entire*
/// book including the exit's own pending place to be empty, which a
/// reduce-only Market order (never resting) trivially satisfied but a
/// resting Alo order never would again.
fn exit_order_book_clear(projection: Option<&MakerAccountProjection>) -> bool {
    match projection {
        Some(projection) => {
            projection
                .resting_quotes()
                .iter()
                .all(|quote| quote.level == maker::EXIT_ORDER_LEVEL)
                && projection
                    .pending_places()
                    .iter()
                    .all(|place| place.level == maker::EXIT_ORDER_LEVEL)
                && projection
                    .pending_cancels()
                    .iter()
                    .all(|cancel| cancel.level == maker::EXIT_ORDER_LEVEL)
        }
        None => false,
    }
}

/// The exit's own resting order, if the venue currently shows one.
fn resting_exit_order(projection: Option<&MakerAccountProjection>) -> Option<RestingQuote> {
    projection?
        .resting_quotes()
        .into_iter()
        .find(|quote| quote.level == maker::EXIT_ORDER_LEVEL)
}

/// docs/33 structural problem #2: derive this cycle's `ExitPhaseState` for
/// [`maker::plan_exit_order_step`] from the CLI-local cache and the venue's
/// authoritative resting-order truth, never from the cache alone.
///
/// - Local cache present and in the `Ioc` phase: `Ioc` never rests, so the
///   venue has nothing to corroborate with — trust the cache (it is the only
///   record of "already escalated" once the book goes quiet).
/// - Local cache present and a matching resting order exists: the normal
///   case, refresh `resting_price` from the venue.
/// - Local cache present but nothing is resting: the tracked order must have
///   resolved (filled/rejected) without being observed yet, or the cache is
///   stale after a recovery. Report untracked so the caller re-evaluates
///   from scratch rather than trusting a value that no longer corresponds to
///   anything live.
/// - No local cache but a resting order exists: rehydration after a
///   recovery reset discarded the cache. The venue-side order is real, so
///   assume `Alo` (the only phase that rests) and restart the per-phase
///   cycle counter — a conservative widening of `alo_max_cycles`'s budget,
///   never a safety issue.
fn exit_phase_state_for_cycle(
    tracking: Option<&InventoryExitOrderTracking>,
    resting: Option<&RestingQuote>,
) -> Option<maker::ExitPhaseState> {
    match (tracking, resting) {
        (Some(tracking), _) if tracking.phase == maker::ExitPhase::Ioc => {
            Some(maker::ExitPhaseState {
                phase: maker::ExitPhase::Ioc,
                cycles_in_phase: tracking.cycles_in_phase,
                resting_price: 0.0,
            })
        }
        (Some(tracking), Some(resting)) => Some(maker::ExitPhaseState {
            phase: tracking.phase,
            cycles_in_phase: tracking.cycles_in_phase,
            resting_price: resting.price,
        }),
        (Some(_), None) => None,
        (None, Some(resting)) => Some(maker::ExitPhaseState {
            phase: maker::ExitPhase::Alo,
            cycles_in_phase: 0,
            resting_price: resting.price,
        }),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_skew_transition_detects_dead_zone_crossings_and_sign_flips() {
        assert!(!external_skew_transitioned(0.0, 0.0));
        assert!(external_skew_transitioned(0.0, 1.0));
        assert!(external_skew_transitioned(1.0, 0.0));
        assert!(!external_skew_transitioned(1.0, 2.0));
        assert!(external_skew_transitioned(1.0, -1.0));
    }

    #[test]
    fn legacy_skew_telemetry_excludes_external_center_offset() {
        let plan = maker::CyclePlan {
            requested_inventory_exit: None,
            inventory_exit: None,
            exit_suppression: None,
            actions: Vec::new(),
            inventory_ref_center: 100.0,
            ref_center: 100.04,
            external_skew_shift_bps: 4.0,
            micro_price_shift_bps: 0.0,
            quote_geometry: Vec::new(),
        };

        assert_eq!(legacy_inventory_skew_shift_bps(100.0, &plan), 0.0);
        assert_ne!(plan.ref_center, plan.inventory_ref_center);
    }

    #[test]
    fn fill_excess_telemetry_expires_to_none_at_guard_freshness_boundary() {
        let now = Instant::now();
        let mut telemetry = super::super::pipeline::ExternalExcessTelemetry::default();
        let decision = maker::GuardDecision {
            enabled: true,
            active: false,
            endangered: None,
            divergence_bps: Some(0.0),
        };
        telemetry.observe(
            decision.divergence_bps,
            Some(maker::ExternalDivergence {
                divergence_bps: 0.0,
                age_ms: 5000,
            }),
            5000,
            now,
        );
        assert_eq!(telemetry.current(now), Some(0.0));
        assert_eq!(
            telemetry.current(now + std::time::Duration::from_millis(1)),
            None
        );

        telemetry.observe(None, None, 5000, now);
        assert_eq!(telemetry.current(now), None);
    }

    #[test]
    fn io_delay_is_included_before_external_freshness_decision() {
        let normalized_at = Instant::now();
        let observed_at = normalized_at + std::time::Duration::from_millis(200);
        let timed = super::super::pipeline::TimedExternalDivergence {
            value: maker::ExternalDivergence {
                divergence_bps: 8.0,
                age_ms: 4_900,
            },
            normalized_at,
        };
        let aged = timed.at(observed_at);
        assert_eq!(aged.age_ms, 5_100);

        let mut guard = maker::GuardController::new(maker::GuardConfig {
            enabled: true,
            enter_bps: 6.0,
            exit_bps: 3.0,
            max_age_ms: 5_000,
        })
        .unwrap();
        // The pre-existing guard sees exactly the pre-I/O typed input, so the
        // default-off arm retains its established side-suppression action.
        let decision = guard.observe(Some(timed.value));
        assert!(decision.active);
        assert_eq!(decision.endangered, Some(OrderSide::Sell));
        let (fresh, excess_bps) =
            external_excess_at_plan(Some(timed), &decision, 5_000, observed_at);
        assert_eq!(fresh, None);
        assert_eq!(excess_bps, None);
        assert_eq!(
            maker::external_skew_shift_bps(
                maker::ExternalSkewConfig {
                    enabled: true,
                    ..Default::default()
                },
                excess_bps,
            ),
            0.0
        );

        let mut telemetry = super::super::pipeline::ExternalExcessTelemetry::default();
        // Defensively reject the already-stale sample even if a caller passes
        // it alongside a numeric value.
        telemetry.observe(decision.divergence_bps, Some(aged), 5_000, observed_at);
        assert_eq!(telemetry.current(observed_at), None);
    }

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
            .contains("price 'bad' is not a number"));
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

    const TEST_PREFIX: &str = "sxmk-cycletest-";

    fn quote_place(level: u32) -> ProjectionPendingPlace {
        ProjectionPendingPlace {
            request_id: "req-1".to_owned(),
            client_order_id: maker::quote_client_order_id(TEST_PREFIX, 1, OrderSide::Buy, level),
            side: OrderSide::Buy,
            price: 100.0,
            qty: 0.01,
            level,
            ref_center: 100.0,
            cycle: 1,
        }
    }

    fn exit_place() -> ProjectionPendingPlace {
        ProjectionPendingPlace {
            request_id: "exit-req".to_owned(),
            client_order_id: maker::exit_client_order_id(TEST_PREFIX, 1),
            side: OrderSide::Sell,
            price: 101.0,
            qty: 0.05,
            level: maker::EXIT_ORDER_LEVEL,
            ref_center: 101.0,
            cycle: 1,
        }
    }

    fn observe_open(
        projection: &mut MakerAccountProjection,
        place: &ProjectionPendingPlace,
        order_id: u64,
    ) {
        projection.apply(
            1,
            AccountProjectionEvent::OrderObserved(maker::OrderObservation {
                order_id,
                client_order_id: Some(place.client_order_id.clone()),
                side: place.side,
                price: place.price,
                open_qty: place.qty,
                terminal: false,
            }),
        );
    }

    /// docs/33 review finding 2: the exit-level filters on `plan_cycle`'s
    /// inputs are NOT conditioned on `alo_enabled`, so they also apply to the
    /// default-off Market exit, whose projection entry sits at the same
    /// sentinel. That is a real (if inert) change to the default-off path —
    /// inert because quotes are fully suppressed for the entire window such an
    /// entry can exist — and it was previously unpinned by any test. Pin both
    /// halves: only the exit footprint is dropped, and the sentinel can never
    /// alias a real quote slot.
    #[test]
    fn planner_inputs_exclude_only_the_exit_footprint() {
        // The sentinel must be unreachable as a real level, or filtering by it
        // would silently drop a genuine quote slot.
        assert_eq!(maker::EXIT_ORDER_LEVEL, u32::MAX - 1);

        let ordinary = quote_place(0);
        let exit = exit_place();
        assert_eq!(exit.level, maker::EXIT_ORDER_LEVEL);
        assert_ne!(ordinary.level, maker::EXIT_ORDER_LEVEL);

        let slots = planner_pending_slots(&[ordinary.clone(), exit.clone()]);
        assert_eq!(slots, vec![(ordinary.side, ordinary.level)]);

        let as_resting = |place: &ProjectionPendingPlace| RestingQuote {
            order_id: Some("1".to_owned()),
            side: place.side,
            level: place.level,
            price: place.price,
            qty: place.qty,
            ref_center: place.ref_center,
            placed_at_cycle: place.cycle,
        };
        let kept = planner_resting(&[as_resting(&ordinary), as_resting(&exit)]);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].level, ordinary.level);
    }

    /// docs/33 structural problem #1: a resting Alo exit order must not
    /// permanently block its own management. This proves the gate the
    /// Alo/Ioc path uses (`exit_order_book_clear`) stays clear with only the
    /// exit's own resting order present — unlike the original (pre-stage-8)
    /// `account_clear` gate, which required the *entire* book empty and
    /// would have wedged forever once an exit order could rest.
    #[test]
    fn exit_order_book_clear_ignores_the_exits_own_footprint_but_not_ordinary_quotes() {
        assert!(
            !exit_order_book_clear(None),
            "no projection is not a clear book"
        );

        let mut projection = MakerAccountProjection::new(1, TEST_PREFIX, 0.0, 0.005, 0.00005);
        assert!(exit_order_book_clear(Some(&projection)));

        // An ordinary resting quote must still block (the maker book has not
        // actually cleared).
        let quote = quote_place(0);
        projection.apply(1, AccountProjectionEvent::PlaceSubmitted(quote.clone()));
        observe_open(&mut projection, &quote, 1);
        assert!(!exit_order_book_clear(Some(&projection)));

        // Replace it with only the exit's own resting order: the gate must
        // now read clear, precisely the case that used to deadlock.
        let mut projection = MakerAccountProjection::new(1, TEST_PREFIX, 0.0, 0.005, 0.00005);
        let exit = exit_place();
        projection.apply(1, AccountProjectionEvent::PlaceSubmitted(exit.clone()));
        observe_open(&mut projection, &exit, 2);
        assert!(
            exit_order_book_clear(Some(&projection)),
            "the exit's own resting order must not block its own gate"
        );

        // An ordinary pending place alongside the resting exit still blocks.
        let mut still_pending = quote_place(0);
        still_pending.request_id = "req-2".to_owned();
        projection.apply(1, AccountProjectionEvent::PlaceSubmitted(still_pending));
        assert!(!exit_order_book_clear(Some(&projection)));
    }

    /// docs/33 structural problem #1 (second half): the crux property that
    /// makes the Alo/Ioc machine able to run at all past its first cycle.
    /// Without this, `inventory_exit_pending` (set on every submission) would
    /// keep `exit_awaiting_confirmation` true for as long as the trigger
    /// holds — i.e. for the entire time the exit is resting — and the exit
    /// block would never run its hold/reprice/upgrade decision again.
    #[test]
    fn exit_wire_submission_settles_once_the_ack_lands_not_when_the_exit_resolves() {
        assert!(
            !exit_wire_submission_settled(None),
            "no projection is never settled"
        );

        let mut projection = MakerAccountProjection::new(1, TEST_PREFIX, 0.0, 0.005, 0.00005);
        assert!(
            exit_wire_submission_settled(Some(&projection)),
            "nothing outstanding is trivially settled"
        );

        // Submitted, ack not yet observed: still awaiting confirmation.
        let exit = exit_place();
        projection.apply(1, AccountProjectionEvent::PlaceSubmitted(exit.clone()));
        assert!(!exit_wire_submission_settled(Some(&projection)));

        // The venue shows it resting: settled, even though the exit itself
        // (the position reduction) has not happened yet — that is exactly
        // the gap `inventory_exit_pending` alone could not express.
        observe_open(&mut projection, &exit, 7);
        assert!(exit_wire_submission_settled(Some(&projection)));
    }

    #[test]
    fn resting_exit_order_finds_only_the_exit_level_entry() {
        let mut projection = MakerAccountProjection::new(1, TEST_PREFIX, 0.0, 0.005, 0.00005);
        assert_eq!(resting_exit_order(Some(&projection)), None);

        let quote = quote_place(0);
        projection.apply(1, AccountProjectionEvent::PlaceSubmitted(quote.clone()));
        observe_open(&mut projection, &quote, 1);
        assert_eq!(
            resting_exit_order(Some(&projection)),
            None,
            "an ordinary resting quote is not the exit order"
        );

        let exit = exit_place();
        projection.apply(1, AccountProjectionEvent::PlaceSubmitted(exit.clone()));
        observe_open(&mut projection, &exit, 2);
        let found = resting_exit_order(Some(&projection)).expect("exit order must be found");
        assert_eq!(found.level, maker::EXIT_ORDER_LEVEL);
        assert_eq!(found.price, 101.0);
    }

    #[test]
    fn exit_phase_state_trusts_the_cache_for_ioc_since_it_never_rests() {
        let tracking = InventoryExitOrderTracking {
            phase: maker::ExitPhase::Ioc,
            cycles_in_phase: 3,
        };
        let state = exit_phase_state_for_cycle(Some(&tracking), None)
            .expect("an Ioc-phase cache must be trusted with no resting order");
        assert_eq!(state.phase, maker::ExitPhase::Ioc);
        assert_eq!(state.cycles_in_phase, 3);
    }

    #[test]
    fn exit_phase_state_refreshes_resting_price_from_the_venue() {
        let tracking = InventoryExitOrderTracking {
            phase: maker::ExitPhase::Alo,
            cycles_in_phase: 2,
        };
        let resting = RestingQuote {
            order_id: Some("9".to_owned()),
            side: OrderSide::Sell,
            level: maker::EXIT_ORDER_LEVEL,
            price: 103.5,
            qty: 0.02,
            ref_center: 103.5,
            placed_at_cycle: 1,
        };
        let state = exit_phase_state_for_cycle(Some(&tracking), Some(&resting)).unwrap();
        assert_eq!(state.phase, maker::ExitPhase::Alo);
        assert_eq!(state.cycles_in_phase, 2);
        assert_eq!(state.resting_price, 103.5);
    }

    #[test]
    fn exit_phase_state_drops_a_stale_alo_cache_with_nothing_resting() {
        let tracking = InventoryExitOrderTracking {
            phase: maker::ExitPhase::Alo,
            cycles_in_phase: 5,
        };
        assert_eq!(exit_phase_state_for_cycle(Some(&tracking), None), None);
    }

    /// docs/33 structural problem #2: a recovery reset discards the CLI-local
    /// cache, but the venue can still show a resting exit order under this
    /// run's prefix. Rehydration must recover `Alo` from that fact alone,
    /// not silently open a duplicate order.
    #[test]
    fn exit_phase_state_rehydrates_alo_from_a_resting_order_with_no_local_cache() {
        let resting = RestingQuote {
            order_id: Some("42".to_owned()),
            side: OrderSide::Buy,
            level: maker::EXIT_ORDER_LEVEL,
            price: 97.0,
            qty: 0.03,
            ref_center: 97.0,
            placed_at_cycle: 10,
        };
        let state = exit_phase_state_for_cycle(None, Some(&resting)).unwrap();
        assert_eq!(state.phase, maker::ExitPhase::Alo);
        assert_eq!(state.cycles_in_phase, 0, "the per-phase budget restarts");
        assert_eq!(state.resting_price, 97.0);
    }

    #[test]
    fn exit_phase_state_is_untracked_with_no_cache_and_nothing_resting() {
        assert_eq!(exit_phase_state_for_cycle(None, None), None);
    }

    /// docs/33 scope limit: `WindDown` must never enter the Alo/Ioc machine,
    /// regardless of `alo_enabled` — an A/B arm has to converge to flat
    /// deterministically, and an Alo order may never fill.
    #[test]
    fn wind_down_never_uses_alo_ioc_even_when_enabled() {
        let enabled = maker::InventoryExitConfig {
            alo_enabled: true,
            ..maker::InventoryExitConfig::default()
        };
        let disabled = maker::InventoryExitConfig::default();
        assert!(!exit_uses_alo_ioc(maker::ExitKind::WindDown, &enabled));
        assert!(!exit_uses_alo_ioc(maker::ExitKind::WindDown, &disabled));
    }

    #[test]
    fn inventory_trim_uses_alo_ioc_only_when_enabled() {
        let enabled = maker::InventoryExitConfig {
            alo_enabled: true,
            ..maker::InventoryExitConfig::default()
        };
        let disabled = maker::InventoryExitConfig::default();
        assert!(exit_uses_alo_ioc(maker::ExitKind::InventoryTrim, &enabled));
        assert!(!exit_uses_alo_ioc(
            maker::ExitKind::InventoryTrim,
            &disabled
        ));
    }
}
