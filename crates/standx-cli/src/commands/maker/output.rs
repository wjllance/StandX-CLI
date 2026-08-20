use super::feed::{MarketTelemetrySnapshot, WsSnapshotDiagnostics};
use super::model::{optional_decimal, Decimal};
use super::*;
use standx_maker::{self as maker, Action, MakerConfig, MakerStats};
use standx_sdk::account_stream::AccountEvent;
use standx_sdk::models::{Balance, OrderSide};

pub(super) fn emit_account_event_lag(
    output_format: OutputFormat,
    event: &AccountEvent,
    symbol: &str,
    cycle: u64,
) {
    if output_format != OutputFormat::Json {
        return;
    }
    let (channel, seq, event_time) = match event {
        AccountEvent::Order(update) if update.symbol.eq_ignore_ascii_case(symbol) => {
            ("order", update.seq, update.updated_at.as_str())
        }
        AccountEvent::Position(update) if update.symbol.eq_ignore_ascii_case(symbol) => {
            ("position", update.seq, update.updated_at.as_str())
        }
        AccountEvent::Trade(update) if update.symbol.eq_ignore_ascii_case(symbol) => {
            ("trade", update.seq, update.trade_ts.as_str())
        }
        AccountEvent::Balance(update) => ("balance", update.seq, update.updated_at.as_str()),
        _ => return,
    };
    let received = chrono::Utc::now();
    let event_time_ms = parse_event_time_ms(event_time);
    println!(
        "{}",
        serde_json::json!({
            "action": "account_event_lag",
            "symbol": symbol,
            "cycle": cycle,
            "channel": channel,
            "seq": seq,
            "event_time": event_time,
            "event_time_ms": event_time_ms,
            "received_utc_ms": received.timestamp_millis(),
            "account_event_lag_ms": event_time_ms.map(|event_time_ms| {
                received.timestamp_millis().saturating_sub(event_time_ms)
            }),
            "available": event_time_ms.is_some(),
        })
    );
}

fn parse_event_time_ms(value: &str) -> Option<i64> {
    if let Ok(timestamp) = chrono::DateTime::parse_from_rfc3339(value) {
        return Some(timestamp.timestamp_millis());
    }
    let raw = value.parse::<i64>().ok()?;
    Some(if raw.abs() < 1_000_000_000_000 {
        raw.saturating_mul(1_000)
    } else {
        raw
    })
}

pub(super) fn emit_order_latency(
    output_format: OutputFormat,
    symbol: &str,
    tracker: &standx_maker::OrderLatencyTracker,
) {
    use standx_maker::{LatencyRequestKind, LatencyRequestOutcome};

    if output_format == OutputFormat::Json {
        for request in tracker.requests() {
            let context = &request.context;
            println!(
                "{}",
                serde_json::json!({
                    "action": "order_latency",
                    "request_id": context.request_id,
                    "kind": latency_kind(context.kind),
                    "generation": context.generation,
                    "cycle": context.cycle,
                    "symbol": context.symbol,
                    "side": context.side,
                    "level": context.level,
                    "order_id": context.order_id,
                    "market_source": context.market_source,
                    "recovery": context.recovery,
                    "intent_utc_ms": context.intent_utc_ms,
                    "place_write_ms": (context.kind == LatencyRequestKind::Place)
                        .then(|| request.written_ms.map(|at| at - context.intent_ms)).flatten(),
                    "place_ack_ms": (context.kind == LatencyRequestKind::Place)
                        .then(|| request.ack_ms.map(|at| at - request.written_ms.unwrap_or(context.intent_ms))).flatten(),
                    "place_effective_ms": (context.kind == LatencyRequestKind::Place)
                        .then(|| request.effective_ms.map(|at| at - context.intent_ms)).flatten(),
                    "cancel_write_ms": (context.kind == LatencyRequestKind::Cancel)
                        .then(|| request.written_ms.map(|at| at - context.intent_ms)).flatten(),
                    "cancel_ack_ms": (context.kind == LatencyRequestKind::Cancel)
                        .then(|| request.ack_ms.map(|at| at - request.written_ms.unwrap_or(context.intent_ms))).flatten(),
                    "cancel_effective_ms": (context.kind == LatencyRequestKind::Cancel)
                        .then(|| request.effective_ms.map(|at| at - context.intent_ms)).flatten(),
                    "fill_after_cancel_ms": request.fill_after_cancel_ms,
                    "timeout_phase": request.timeout_phase.map(|phase| phase.label()),
                    "timeout_ms": request.timeout_ms,
                    "outcome": request.outcome.map(latency_outcome),
                })
            );
        }
        for kind in [LatencyRequestKind::Place, LatencyRequestKind::Cancel] {
            let summary = tracker.summary(kind);
            println!("{}", latency_summary_json(symbol, &summary));
        }
    } else if output_format != OutputFormat::Quiet {
        for kind in [LatencyRequestKind::Place, LatencyRequestKind::Cancel] {
            let summary = tracker.summary(kind);
            if summary.requests > 0 {
                println!(
                    "{} latency: requests={} effective={} rejected={} timeout={} p95_ack={} p95_effective={}",
                    latency_kind(kind),
                    summary.requests,
                    summary.effective,
                    summary.rejected,
                    summary.timeout,
                    optional_ms(summary.ack.p95_ms),
                    optional_ms(summary.effective_latency.p95_ms),
                );
            }
        }
    }

    fn latency_outcome(outcome: LatencyRequestOutcome) -> &'static str {
        match outcome {
            LatencyRequestOutcome::Accepted => "accepted",
            LatencyRequestOutcome::Rejected => "rejected",
            LatencyRequestOutcome::Effective => "effective",
            LatencyRequestOutcome::Timeout => "timeout",
            LatencyRequestOutcome::Invalidated => "invalidated",
            LatencyRequestOutcome::ProcessEnded => "process_ended",
        }
    }
}

fn latency_kind(kind: standx_maker::LatencyRequestKind) -> &'static str {
    match kind {
        standx_maker::LatencyRequestKind::Place => "place",
        standx_maker::LatencyRequestKind::Cancel => "cancel",
    }
}

fn latency_metric_json(metric: standx_maker::LatencyMetricSummary) -> serde_json::Value {
    serde_json::json!({
        "samples": metric.samples,
        "p50_ms": metric.p50_ms,
        "p95_ms": metric.p95_ms,
        "p99_ms": metric.p99_ms,
    })
}

fn latency_summary_json(symbol: &str, summary: &standx_maker::LatencySummary) -> serde_json::Value {
    serde_json::json!({
        "action": "order_latency_summary",
        "symbol": symbol,
        "kind": latency_kind(summary.kind),
        "requests": summary.requests,
        "accepted": summary.accepted,
        "rejected": summary.rejected,
        "effective": summary.effective,
        "timeout": summary.timeout,
        "invalidated": summary.invalidated,
        "process_ended": summary.process_ended,
        "pending": summary.pending,
        "reject_rate": summary.reject_rate,
        "timeout_rate": summary.timeout_rate,
        "write": latency_metric_json(summary.write),
        "ack": latency_metric_json(summary.ack),
        "effective_latency": latency_metric_json(summary.effective_latency),
        "fill_after_cancel": latency_metric_json(summary.fill_after_cancel),
        "write_p50_ms": summary.write.p50_ms,
        "write_p95_ms": summary.write.p95_ms,
        "write_p99_ms": summary.write.p99_ms,
        "ack_p50_ms": summary.ack.p50_ms,
        "ack_p95_ms": summary.ack.p95_ms,
        "ack_p99_ms": summary.ack.p99_ms,
        "effective_latency_p50_ms": summary.effective_latency.p50_ms,
        "effective_latency_p95_ms": summary.effective_latency.p95_ms,
        "effective_latency_p99_ms": summary.effective_latency.p99_ms,
        "fill_after_cancel_p50_ms": summary.fill_after_cancel.p50_ms,
        "fill_after_cancel_p95_ms": summary.fill_after_cancel.p95_ms,
        "fill_after_cancel_p99_ms": summary.fill_after_cancel.p99_ms,
    })
}

fn optional_ms(value: Option<u64>) -> String {
    value.map_or_else(|| "-".to_string(), |value| format!("{value}ms"))
}

fn ws_snapshot_json(diagnostics: &WsSnapshotDiagnostics) -> serde_json::Value {
    serde_json::json!({
        "mark_seq": diagnostics.mark_seq,
        "book_seq": diagnostics.book_seq,
        "mark_server_time": diagnostics.mark_server_time,
        "book_server_time": diagnostics.book_server_time,
        "mark_envelope_time": diagnostics.mark_envelope_time,
        "book_envelope_time": diagnostics.book_envelope_time,
        "mark_payload_time": diagnostics.mark_payload_time,
        "book_payload_time": diagnostics.book_payload_time,
        "mark_age_ms": diagnostics.mark_age_ms,
        "book_age_ms": diagnostics.book_age_ms,
        "local_skew_ms": diagnostics.local_skew_ms,
        "server_skew_ms": diagnostics.server_skew_ms,
    })
}

/// Which exit policy asked for a reduce-only order this cycle and what
/// happened to it (stage 5-b). Telemetry only — the plan already decided.
///
/// Reported on `cycle_summary` rather than as its own event line so a long
/// halt cannot flood the stream: the halt is already a per-cycle field, and
/// counting suppressed exits stays a one-field filter over existing lines.
#[derive(Clone, Copy, Default)]
pub(super) struct ExitStatus {
    /// The exit policy that produced a plan this cycle, if any.
    pub(super) kind: Option<maker::ExitKind>,
    /// Whether a reduce-only order was actually submitted this cycle.
    pub(super) submitted: bool,
    /// Set when the planned exit was suppressed instead of submitted.
    pub(super) suppressed: Option<maker::SuppressedExit>,
}

/// Per-cycle output: one human line + indented actions, or JSON lines.
pub(super) struct CycleOutput<'a> {
    pub(super) output_format: OutputFormat,
    pub(super) live: bool,
    pub(super) symbol: &'a str,
    pub(super) cycle: u64,
    pub(super) mark: f64,
    pub(super) best_bid: Option<f64>,
    pub(super) best_ask: Option<f64>,
    pub(super) market_source: &'static str,
    pub(super) market_fallback_reason: Option<&'static str>,
    pub(super) ws_snapshot: Option<&'a WsSnapshotDiagnostics>,
    pub(super) market_telemetry: &'a MarketTelemetrySnapshot,
    pub(super) quote_geometry: &'a [maker::QuoteGeometry],
    pub(super) position: f64,
    pub(super) starting_position: f64,
    pub(super) account: Option<&'a Balance>,
    pub(super) actions: &'a [Action],
    pub(super) fills: &'a [MakerFill],
    /// Guard-normalized excess sample in use when these fills were observed.
    pub(super) excess_bps_at_fill: Option<f64>,
    pub(super) stats: &'a MakerStats,
    pub(super) halt_vol_bps: Option<f64>,
    pub(super) spread_decision: &'a maker::SpreadDecision,
    pub(super) size_skew_decision: &'a maker::SizeSkewDecision,
    pub(super) guard_decision: &'a maker::GuardDecision,
    /// Divergence-basis EMA the guard's excess is measured against.
    pub(super) external_basis_bps: Option<f64>,
    /// Continuous external-price component of the quote-center shift.
    pub(super) external_skew_shift_bps: f64,
    /// In-venue touch-mid component of the quote-center shift (0 when disabled).
    pub(super) micro_price_shift_bps: f64,
    /// Legacy inventory-only quote-center shift in bps; covers linear and
    /// nonlinear skew but deliberately excludes the additive external field.
    pub(super) skew_shift_bps: f64,
    /// Stage 5-b: typed exit policy outcome for this cycle.
    pub(super) exit_status: ExitStatus,
    pub(super) cfg: &'a MakerConfig,
    pub(super) performance: Option<&'a maker::PerformanceSummary>,
}

pub(super) fn emit_maker_cycle(output: CycleOutput<'_>) {
    let CycleOutput {
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
        quote_geometry,
        position,
        starting_position,
        account,
        actions,
        fills,
        excess_bps_at_fill,
        stats,
        halt_vol_bps,
        spread_decision,
        size_skew_decision,
        guard_decision,
        external_basis_bps,
        external_skew_shift_bps,
        micro_price_shift_bps,
        skew_shift_bps,
        exit_status,
        cfg,
        performance,
    } = output;
    use maker::format_decimals;

    let pnl = stats.pnl(position, mark);

    let mode = if live { "live" } else { "paper" };
    let counts = actions.iter().fold((0, 0, 0), |mut acc, a| {
        match a {
            Action::Place(_) => acc.1 += 1,
            Action::Cancel { .. } => acc.2 += 1,
            Action::Hold { .. } => acc.0 += 1,
        }
        acc
    });
    let (holds, places, cancels) = counts;

    match output_format {
        OutputFormat::Json => {
            let ts = ts_now();
            for fill in fills {
                println!(
                    "{}",
                    serde_json::json!({
                        "ts": ts, "cycle": cycle, "mode": mode, "symbol": symbol,
                        "action": "fill", "side": fill.side,
                        "price": format_decimals(fill.price, cfg.price_decimals),
                        "qty": format_decimals(fill.qty, cfg.qty_decimals),
                        "mark_at_fill": fill.mark_at_fill,
                        "excess_bps_at_fill": fill_excess_bps_json(excess_bps_at_fill),
                        "event_time_ms": fill.event_time_ms,
                        "trade_id": fill.trade_id,
                        "order_id": fill.order_id,
                        "trade_ts": fill.trade_ts,
                        "origin": fill.origin,
                        "role": match fill.role {
                            maker::FillRole::PassiveMaker => "passive_maker",
                            maker::FillRole::InventoryExit => "inventory_exit",
                        },
                        "fee_quote": fill.costs.map(|costs| costs.fee_quote),
                        "rebate_quote": fill.costs.map(|costs| costs.rebate_quote),
                    })
                );
            }
            for a in actions {
                let obj = match a {
                    Action::Place(q) => serde_json::json!({
                        "ts": ts, "cycle": cycle, "mode": mode, "symbol": symbol,
                        "mark": format_decimals(mark, cfg.price_decimals),
                        "action": "place", "side": q.side, "level": q.level,
                        "price": format_decimals(q.price, cfg.price_decimals),
                        "qty": format_decimals(q.qty, cfg.qty_decimals),
                    }),
                    Action::Cancel {
                        order_id,
                        side,
                        level,
                        price,
                        reason,
                    } => serde_json::json!({
                        "ts": ts, "cycle": cycle, "mode": mode, "symbol": symbol,
                        "mark": format_decimals(mark, cfg.price_decimals),
                        "action": "cancel", "side": side, "level": level,
                        "price": format_decimals(*price, cfg.price_decimals),
                        "reason": reason.as_str(), "order_id": order_id,
                    }),
                    Action::Hold {
                        side,
                        level,
                        price,
                        age_cycles,
                        drift_bps,
                    } => serde_json::json!({
                        "ts": ts, "cycle": cycle, "mode": mode, "symbol": symbol,
                        "mark": format_decimals(mark, cfg.price_decimals),
                        "action": "hold", "side": side, "level": level,
                        "price": format_decimals(*price, cfg.price_decimals),
                        "age_cycles": age_cycles,
                        "drift_bps": (drift_bps * 100.0).round() / 100.0,
                    }),
                };
                println!("{}", obj);
            }
            println!(
                "{}",
                with_geometry_fields(
                    with_book_fields(
                        with_exit_fields(
                            with_guard_fields(
                                with_size_skew_fields(
                                    with_spread_fields(
                                        serde_json::json!({
                                        "ts": ts, "cycle": cycle, "mode": mode, "symbol": symbol,
                                        "action": "cycle_summary",
                                        "mark": format_decimals(mark, cfg.price_decimals),
                                        "best_bid": best_bid, "best_ask": best_ask,
                                        "market_source": market_source,
                                        "market_fallback_reason": market_fallback_reason,
                                        "ws_snapshot": ws_snapshot.map(ws_snapshot_json),
                                        "position": position,
                                        "starting_position": starting_position,
                                        "account": account.map(account_json),
                                        "holds": holds, "places": places, "cancels": cancels,
                                        "fills": fills.len(),
                                        "pnl": (pnl * 1e6).round() / 1e6,
                                        "fills_total": stats.fills(),
                                        "uptime_pct": (stats.uptime_pct() * 10.0).round() / 10.0,
                                        "avg_capture_bps": (stats.avg_spread_capture_bps() * 100.0).round() / 100.0,
                                        "performance": performance.map(performance_json),
                                        "halted": halt_vol_bps.is_some(),
                                        "vol_bps": halt_vol_bps.map(|v| (v * 100.0).round() / 100.0),
                                            }),
                                        spread_decision,
                                    ),
                                    size_skew_decision,
                                ),
                                guard_decision,
                                external_basis_bps,
                                external_skew_shift_bps,
                                micro_price_shift_bps,
                                skew_shift_bps,
                            ),
                            exit_status,
                        ),
                        market_telemetry,
                        mark,
                        best_bid,
                        best_ask,
                    ),
                    quote_geometry,
                    mark,
                )
            );
        }
        OutputFormat::Quiet => {
            for fill in fills {
                println!(
                    "fill {} @ {} x {}",
                    side_str(fill.side),
                    format_decimals(fill.price, cfg.price_decimals),
                    format_decimals(fill.qty, cfg.qty_decimals)
                );
            }
            // Only mutations and their reasons.
            for a in actions {
                match a {
                    Action::Place(q) => println!(
                        "place {} L{} @ {}",
                        side_str(q.side),
                        q.level,
                        format_decimals(q.price, cfg.price_decimals)
                    ),
                    Action::Cancel {
                        side,
                        level,
                        price,
                        reason,
                        ..
                    } => println!(
                        "cancel {} L{} @ {} ({})",
                        side_str(*side),
                        level,
                        format_decimals(*price, cfg.price_decimals),
                        reason.as_str()
                    ),
                    Action::Hold { .. } => {}
                }
            }
        }
        _ => {
            let now = chrono::Local::now().format("%H:%M:%S");
            let mut fill_note = if fills.is_empty() {
                String::new()
            } else {
                format!(" fill={}", fills.len())
            };
            if let Some(v) = halt_vol_bps {
                fill_note.push_str(&format!(" ⚡HALT vol={:.1}bps", v));
            }
            if let Some(suppressed) = exit_status.suppressed {
                fill_note.push_str(&format!(
                    " ⛔exit_suppressed={}/{}",
                    suppressed.kind.as_str(),
                    suppressed.reason.as_str()
                ));
            }
            println!(
                "[{}] #{} mark={} bid={} ask={} pos={} pnl={:.2} | hold={} place={} cancel={}{}",
                now,
                cycle,
                format_decimals(mark, cfg.price_decimals),
                best_bid
                    .map(|b| format_decimals(b, cfg.price_decimals))
                    .unwrap_or_else(|| "-".into()),
                best_ask
                    .map(|a| format_decimals(a, cfg.price_decimals))
                    .unwrap_or_else(|| "-".into()),
                format_decimals(position, cfg.qty_decimals),
                pnl,
                holds,
                places,
                cancels,
                fill_note
            );
            if let Some(account) = account {
                println!(
                    "    ACCOUNT balance={} equity={} available={} upnl={}",
                    format_account_amount(&account.balance),
                    format_account_amount(&account.equity),
                    format_account_amount(&account.cross_available),
                    format_account_amount(&account.upnl),
                );
            }
            for fill in fills {
                println!(
                    "    FILL   {} @ {} x {}",
                    side_str(fill.side),
                    format_decimals(fill.price, cfg.price_decimals),
                    format_decimals(fill.qty, cfg.qty_decimals)
                );
            }
            for a in actions {
                match a {
                    Action::Place(q) => println!(
                        "    PLACE  {} L{} @ {} x {}",
                        side_str(q.side),
                        q.level,
                        format_decimals(q.price, cfg.price_decimals),
                        format_decimals(q.qty, cfg.qty_decimals)
                    ),
                    Action::Cancel {
                        side,
                        level,
                        price,
                        reason,
                        ..
                    } => println!(
                        "    CANCEL {} L{} @ {} ({})",
                        side_str(*side),
                        level,
                        format_decimals(*price, cfg.price_decimals),
                        reason.as_str()
                    ),
                    Action::Hold {
                        side,
                        level,
                        price,
                        age_cycles,
                        drift_bps,
                    } => println!(
                        "    HOLD   {} L{} @ {} (age {} cycles, drift {:.1}bps)",
                        side_str(*side),
                        level,
                        format_decimals(*price, cfg.price_decimals),
                        age_cycles,
                        drift_bps
                    ),
                }
            }
        }
    }
}

/// Additive Part-A fields. The bounded source snapshot is observation-only;
/// calculations here cannot feed back into planning, guards, or reconciliation.
fn with_book_fields(
    mut summary: serde_json::Value,
    telemetry: &MarketTelemetrySnapshot,
    mark: f64,
    best_bid: Option<f64>,
    best_ask: Option<f64>,
) -> serde_json::Value {
    let bid_qty_top = telemetry
        .book
        .bid_levels
        .as_ref()
        .and_then(|levels| levels.first())
        .filter(|(price, _)| best_bid == Some(*price))
        .map(|(_, qty)| *qty);
    let ask_qty_top = telemetry
        .book
        .ask_levels
        .as_ref()
        .and_then(|levels| levels.first())
        .filter(|(price, _)| best_ask == Some(*price))
        .map(|(_, qty)| *qty);
    let spread_bps = best_bid
        .zip(best_ask)
        .filter(|_| mark.is_finite() && mark > 0.0)
        .and_then(|(bid, ask)| {
            let spread = (ask - bid) / mark * 1e4;
            spread.is_finite().then_some(spread)
        });
    let mark_mid_divergence_bps = best_bid
        .zip(best_ask)
        .filter(|_| mark.is_finite() && mark > 0.0)
        .and_then(|(bid, ask)| {
            let divergence = maker::mark_mid_divergence_bps(mark, bid, ask);
            divergence.is_finite().then_some(divergence)
        });
    let object = summary
        .as_object_mut()
        .expect("cycle summary JSON must be an object");
    object.insert(
        "book".to_string(),
        serde_json::json!({
            "bid_levels": telemetry.book.bid_levels,
            "ask_levels": telemetry.book.ask_levels,
            "bid_qty_top": bid_qty_top,
            "ask_qty_top": ask_qty_top,
            "spread_bps": spread_bps,
            "mark_mid_divergence_bps": mark_mid_divergence_bps,
            "age_ms": telemetry.book.age_ms,
        }),
    );
    object.insert(
        "tape".to_string(),
        serde_json::json!({
            "count_5s": telemetry.tape.count_5s,
            "buy_qty_5s": telemetry.tape.buy_qty_5s,
            "sell_qty_5s": telemetry.tape.sell_qty_5s,
            "unknown_qty_5s": telemetry.tape.unknown_qty_5s,
            "last_trade_age_ms": telemetry.tape.last_trade_age_ms,
        }),
    );
    summary
}

/// Additive Part-B quote-geometry diagnostics. These values are produced by
/// the pure planner and are never fed back into quote or safety decisions.
fn with_geometry_fields(
    mut summary: serde_json::Value,
    geometry: &[maker::QuoteGeometry],
    mark: f64,
) -> serde_json::Value {
    let min_distance_to_touch_bps = geometry
        .iter()
        .filter_map(|quote| quote.distance_to_touch_bps)
        .filter(|distance| distance.is_finite())
        .reduce(f64::min);
    let count = |outcome| {
        geometry
            .iter()
            .filter(|quote| quote.outcome == outcome)
            .count()
    };
    let quotes: Vec<_> = geometry
        .iter()
        .map(|quote| {
            let raw_bps = maker::side_distance_to_mark_bps(quote.side, quote.raw_price, mark);
            let final_bps = quote
                .final_price
                .map(|price| maker::side_distance_to_mark_bps(quote.side, price, mark));
            serde_json::json!({
                "side": quote.side,
                "level": quote.level,
                "outcome": quote.outcome.as_str(),
                "raw_bps": raw_bps,
                "final_bps": final_bps,
                "dist_touch_bps": quote.distance_to_touch_bps,
                "band_edge_bps": quote.band_edge_bps,
            })
        })
        .collect();
    let object = summary
        .as_object_mut()
        .expect("cycle summary JSON must be an object");
    object.insert(
        "geometry".to_string(),
        serde_json::json!({
            "min_distance_to_touch_bps": min_distance_to_touch_bps,
            "clamped_to_touch": count(maker::QuoteGeometryOutcome::ClampedToTouch),
            "clamped_to_band": count(maker::QuoteGeometryOutcome::ClampedToBand),
            "dropped_infeasible": count(maker::QuoteGeometryOutcome::DroppedInfeasible),
            "quotes": quotes,
        }),
    );
    summary
}

/// Stage 5-b exit fields, additive and top-level like the other wrappers.
/// All three are always present (null when nothing happened) so a consumer can
/// tell "no exit this cycle" from "field missing on an older run".
fn with_exit_fields(mut summary: serde_json::Value, status: ExitStatus) -> serde_json::Value {
    let object = summary
        .as_object_mut()
        .expect("cycle summary JSON must be an object");
    object.insert(
        "exit_kind".to_string(),
        match status.kind {
            Some(kind) => serde_json::Value::from(kind.as_str()),
            None => serde_json::Value::Null,
        },
    );
    object.insert(
        "exit_submitted".to_string(),
        serde_json::Value::from(status.submitted),
    );
    object.insert(
        "exit_suppressed".to_string(),
        match status.suppressed {
            Some(suppressed) => serde_json::Value::from(suppressed.reason.as_str()),
            None => serde_json::Value::Null,
        },
    );
    summary
}

fn with_spread_fields(
    mut summary: serde_json::Value,
    decision: &maker::SpreadDecision,
) -> serde_json::Value {
    let object = summary
        .as_object_mut()
        .expect("cycle summary JSON must be an object");
    object.insert(
        "rolling_vol_bps".to_string(),
        serde_json::json!((decision.rolling_vol_bps * 100.0).round() / 100.0),
    );
    object.insert(
        "adaptive_spread_enabled".to_string(),
        serde_json::json!(decision.enabled),
    );
    object.insert(
        "adaptive_spread_tier".to_string(),
        serde_json::json!(decision.tier),
    );
    object.insert(
        "effective_spread_bps".to_string(),
        serde_json::json!(decision.effective_spread_bps),
    );
    object.insert(
        "effective_refresh_bps".to_string(),
        serde_json::json!(decision.effective_refresh_bps),
    );
    summary
}

/// Additive stage-3 v1 fields: external-guard state and the realized skew
/// shift. Optional top-level keys only — old consumers keep working.
fn with_guard_fields(
    mut summary: serde_json::Value,
    decision: &maker::GuardDecision,
    external_basis_bps: Option<f64>,
    external_skew_shift_bps: f64,
    micro_price_shift_bps: f64,
    skew_shift_bps: f64,
) -> serde_json::Value {
    let object = summary
        .as_object_mut()
        .expect("cycle summary JSON must be an object");
    object.insert(
        "guard_enabled".to_string(),
        serde_json::json!(decision.enabled),
    );
    object.insert(
        "guard_active".to_string(),
        serde_json::json!(decision.active),
    );
    object.insert(
        "guard_side".to_string(),
        serde_json::json!(decision.endangered),
    );
    object.insert(
        "external_divergence_bps".to_string(),
        serde_json::json!(decision.divergence_bps.map(|d| (d * 100.0).round() / 100.0)),
    );
    object.insert(
        "external_basis_bps".to_string(),
        serde_json::json!(external_basis_bps.map(|d| (d * 100.0).round() / 100.0)),
    );
    object.insert(
        "external_skew_shift_bps".to_string(),
        serde_json::json!((external_skew_shift_bps * 100.0).round() / 100.0),
    );
    object.insert(
        "micro_price_shift_bps".to_string(),
        serde_json::json!((micro_price_shift_bps * 100.0).round() / 100.0),
    );
    object.insert(
        "skew_shift_bps".to_string(),
        serde_json::json!((skew_shift_bps * 100.0).round() / 100.0),
    );
    summary
}

fn with_size_skew_fields(
    mut summary: serde_json::Value,
    decision: &maker::SizeSkewDecision,
) -> serde_json::Value {
    let object = summary
        .as_object_mut()
        .expect("cycle summary JSON must be an object");
    object.insert(
        "size_skew_enabled".to_string(),
        serde_json::json!(decision.enabled),
    );
    object.insert(
        "size_skew_active".to_string(),
        serde_json::json!(decision.active),
    );
    object.insert(
        "size_skew_add_side".to_string(),
        serde_json::json!(decision.add_side),
    );
    object.insert(
        "size_skew_inventory_ratio".to_string(),
        serde_json::json!(decision.inventory_ratio),
    );
    object.insert(
        "size_skew_add_qty".to_string(),
        serde_json::json!(decision.add_qty),
    );
    summary
}

fn performance_json(summary: &maker::PerformanceSummary) -> serde_json::Value {
    serde_json::json!({
        "passive_fills": summary.passive_fills,
        "passive_qty": summary.passive_qty,
        "passive_cashflow_quote": summary.passive_cashflow_quote,
        "passive_capture_bps": summary.passive_capture_bps,
        "exit_fills": summary.exit_fills,
        "exit_qty": summary.exit_qty,
        "exit_cashflow_quote": summary.exit_cashflow_quote,
        "gross_spread_quote": summary.gross_spread_quote,
        "fee_quote": summary.fee_quote,
        "rebate_quote": summary.rebate_quote,
        "execution_costs_unavailable": summary.execution_costs_unavailable,
        "funding_unattributed": summary.funding_unattributed,
        "funding_coverage_gap": summary.funding_coverage_gap,
        "funding_quote": summary.funding_quote,
        "funding_available": summary.funding_available,
        "net_pnl_complete": summary.net_pnl_complete,
        "exit_cost_quote": summary.exit_cost_quote,
        "inventory_mtm_change_quote": summary.inventory_mtm_change_quote,
        "net_pnl_quote": summary.net_pnl_quote,
        "position": summary.position,
        "markout_1s_bps": summary.markouts[0].avg_bps,
        "markout_5s_bps": summary.markouts[1].avg_bps,
        "markout_30s_bps": summary.markouts[2].avg_bps,
        "markout_1s_unavailable": summary.markouts[0].unavailable,
        "markout_5s_unavailable": summary.markouts[1].unavailable,
        "markout_30s_unavailable": summary.markouts[2].unavailable,
        "time_weighted_uptime_pct": summary.quote_time.two_sided_uptime_pct,
        "eligible_bid_qty_ms": summary.quote_time.eligible_bid_qty_ms,
        "eligible_ask_qty_ms": summary.quote_time.eligible_ask_qty_ms,
        "eligible_total_qty_ms": summary.quote_time.eligible_total_qty_ms,
        "inventory_observed_ms": summary.inventory_time.observed_ms,
        "inventory_nonzero_ms": summary.inventory_time.nonzero_ms,
        "inventory_abs_qty_ms": summary.inventory_time.abs_qty_ms,
        "inventory_avg_abs_qty": summary.inventory_time.avg_abs_qty,
    })
}

pub(super) fn emit_performance_summary(
    output_format: OutputFormat,
    symbol: &str,
    summary: &maker::PerformanceSummary,
) {
    if output_format == OutputFormat::Json {
        let mut value = performance_json(summary);
        if let Some(object) = value.as_object_mut() {
            object.insert(
                "action".to_string(),
                serde_json::json!("performance_summary"),
            );
            object.insert("symbol".to_string(), serde_json::json!(symbol));
        }
        println!("{value}");
    } else if output_format != OutputFormat::Quiet {
        println!(
            "Performance: passive={} exit={} net_pnl={:.6} time-weighted uptime={:.2}%",
            summary.passive_fills,
            summary.exit_fills,
            summary.net_pnl_quote,
            summary.quote_time.two_sided_uptime_pct,
        );
    }
}

fn account_json(account: &Balance) -> serde_json::Value {
    serde_json::json!({
        "balance": account.balance,
        "equity": account.equity,
        "available": account.cross_available,
        "upnl": account.upnl,
    })
}

fn format_account_amount(value: &str) -> String {
    // Display only: an unparseable balance is shown verbatim rather than hidden,
    // so an operator sees what the venue actually sent.
    match optional_decimal(value, Decimal::Finite) {
        Some(amount) => format!("{amount:.2}"),
        None => value.to_string(),
    }
}

fn side_str(side: OrderSide) -> &'static str {
    match side {
        OrderSide::Buy => "buy ",
        OrderSide::Sell => "sell",
    }
}

/// Emit a one-off maker event (order rejection, no-op cancel) inline,
/// respecting the output format. Only reached in live mode.
/// One line per external-guard activation/release/side-switch (a new additive
/// `action`; existing consumers ignore unknown actions). Event-level
/// attribution joins these against fills avoided inside guard windows.
pub(super) fn emit_guard_transition(
    output_format: OutputFormat,
    symbol: &str,
    cycle: u64,
    decision: &maker::GuardDecision,
) {
    match output_format {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::json!({
                    "ts": ts_now(),
                    "cycle": cycle, "symbol": symbol,
                    "action": "external_guard",
                    "active": decision.active,
                    "side": decision.endangered,
                    "divergence_bps": decision.divergence_bps
                        .map(|d| (d * 100.0).round() / 100.0),
                })
            );
        }
        OutputFormat::Quiet => {}
        _ => {
            if let Some(side) = decision.endangered {
                eprintln!(
                    "    🛡️ external guard: suppressing {} (divergence {:+.2}bps)",
                    side_str(side),
                    decision.divergence_bps.unwrap_or(f64::NAN),
                );
            } else {
                eprintln!("    🛡️ external guard: released");
            }
        }
    }
}

/// Emit when the continuous shift enters/leaves its dead zone or changes sign.
/// This event is additive and never feeds a decision path.
pub(super) fn emit_external_skew_transition(
    output_format: OutputFormat,
    symbol: &str,
    cycle: u64,
    shift_bps: f64,
    excess_bps: Option<f64>,
) {
    match output_format {
        OutputFormat::Json => println!(
            "{}",
            serde_json::json!({
                "ts": ts_now(),
                "cycle": cycle,
                "symbol": symbol,
                "action": "external_skew",
                "active": shift_bps != 0.0,
                "shift_bps": shift_bps,
                "excess_bps": excess_bps,
            })
        ),
        OutputFormat::Quiet => {}
        _ if shift_bps == 0.0 => eprintln!("    ↔️ external skew: returned to dead zone"),
        _ => eprintln!(
            "    ↔️ external skew: shift {shift_bps:+.2}bps (excess {:+.2}bps)",
            excess_bps.unwrap_or(f64::NAN),
        ),
    }
}

pub(super) struct MakerLogEvent<'a> {
    pub(super) output_format: OutputFormat,
    pub(super) symbol: &'a str,
    pub(super) cycle: u64,
    pub(super) action: &'a str,
    pub(super) side: OrderSide,
    pub(super) level: u32,
    pub(super) price: f64,
    pub(super) price_decimals: u32,
    pub(super) detail: &'a str,
    /// Stage 5-b: which exit policy this event belongs to, for exit events.
    /// `None` on everything else, and then omitted from the JSON entirely so
    /// the existing per-event contract is unchanged.
    pub(super) exit_kind: Option<&'a str>,
}

pub(super) fn log_maker_event(event: MakerLogEvent<'_>) {
    let MakerLogEvent {
        output_format,
        symbol,
        cycle,
        action,
        side,
        level,
        price,
        price_decimals,
        detail,
        exit_kind,
    } = event;
    use maker::format_decimals;
    match output_format {
        OutputFormat::Json => {
            let mut payload = serde_json::json!({
                "ts": ts_now(),
                "cycle": cycle, "mode": "live", "symbol": symbol,
                "action": action, "side": side, "level": level,
                "price": format_decimals(price, price_decimals),
                "detail": detail,
            });
            if let Some(kind) = exit_kind {
                payload
                    .as_object_mut()
                    .expect("maker log event JSON must be an object")
                    .insert("exit_kind".to_string(), serde_json::Value::from(kind));
            }
            println!("{}", payload);
        }
        _ => {
            let kind_note = exit_kind.map_or_else(String::new, |kind| format!(" [{kind}]"));
            eprintln!(
                "    {}{} {} L{} @ {} — {}",
                action,
                kind_note,
                side_str(side),
                level,
                format_decimals(price, price_decimals),
                detail
            );
        }
    }
}

pub(super) fn emit_live_fill(
    fill: &MakerFill,
    symbol: &str,
    cycle: u64,
    output_format: OutputFormat,
    excess_bps_at_fill: Option<f64>,
) {
    match output_format {
        OutputFormat::Json => println!(
            "{}",
            serde_json::json!({
                "ts": ts_now(),
                "symbol": symbol,
                "cycle": cycle,
                "action": "fill",
                "origin": fill.origin,
                "order_id": fill.order_id,
                "trade_id": fill.trade_id,
                "trade_ts": fill.trade_ts,
                "side": fill.side,
                "price": fill.price,
                "qty": fill.qty,
                "mark_at_fill": fill.mark_at_fill,
                "excess_bps_at_fill": fill_excess_bps_json(excess_bps_at_fill),
                "event_time_ms": fill.event_time_ms,
                "role": match fill.role {
                    maker::FillRole::PassiveMaker => "passive_maker",
                    maker::FillRole::InventoryExit => "inventory_exit",
                },
                "fee_quote": fill.costs.map(|costs| costs.fee_quote),
                "rebate_quote": fill.costs.map(|costs| costs.rebate_quote),
            })
        ),
        _ => eprintln!(
            "⚡ account fill {:?} {} @ {} (order {})",
            fill.side,
            fill.qty,
            fill.price,
            fill.order_id.unwrap_or_default()
        ),
    }
}

fn fill_excess_bps_json(excess_bps_at_fill: Option<f64>) -> serde_json::Value {
    excess_bps_at_fill.map_or(serde_json::Value::Null, serde_json::Value::from)
}

pub(super) fn emit_reconciliation_state(
    output_format: OutputFormat,
    symbol: &str,
    cycle: u64,
    event: &str,
    cause: &str,
    expected: f64,
    observed: f64,
) {
    if output_format == OutputFormat::Json {
        println!(
            "{}",
            serde_json::json!({
                "ts": ts_now(),
                "symbol": symbol,
                "cycle": cycle,
                "action": "position_reconciliation",
                "event": event,
                "cause": cause,
                "expected_position": expected,
                "observed_position": observed,
            })
        );
    } else {
        eprintln!(
            "⚠️  position reconciliation {event} ({cause}): expected {expected:+.8}, observed {observed:+.8}"
        );
    }
}

pub(super) fn emit_stop_loss_triggered(
    output_format: OutputFormat,
    symbol: &str,
    cycle: u64,
    pnl: f64,
    stop_loss: f64,
) {
    if output_format == OutputFormat::Json {
        println!(
            "{}",
            serde_json::json!({
                "ts": ts_now(),
                "symbol": symbol,
                "cycle": cycle,
                "action": "stop_loss",
                "event": "triggered",
                "pnl": pnl,
                "stop_loss": stop_loss,
            })
        );
    } else {
        eprintln!(
            "🛑 stop-loss triggered: session PnL {pnl:+.2} breached -{stop_loss:.2}; shutting down"
        );
    }
}

/// An armed account hard floor stopping the session (stage 5-b).
///
/// `event` is `triggered` for a real breach and `unevaluable` when the floor
/// refused to read a stale/unparseable balance as "no breach" — the operator
/// remedy differs, so the two never share a label.
pub(super) struct AccountFloorStop<'a> {
    pub(super) event: &'a str,
    pub(super) metric: &'a str,
    pub(super) observed: Option<f64>,
    pub(super) floor: Option<f64>,
    pub(super) detail: &'a str,
}

/// Account-level hard floor stop (stage 5-b). Deliberately a different
/// `action` from `stop_loss`: the session PnL brake and the account solvency
/// brake are different policies with different remedies.
pub(super) fn emit_account_floor_triggered(
    output_format: OutputFormat,
    symbol: &str,
    cycle: u64,
    stop: AccountFloorStop<'_>,
) {
    if output_format == OutputFormat::Json {
        println!(
            "{}",
            serde_json::json!({
                "ts": ts_now(),
                "symbol": symbol,
                "cycle": cycle,
                "action": "account_floor",
                "event": stop.event,
                "metric": stop.metric,
                "observed": stop.observed,
                "floor": stop.floor,
                "detail": stop.detail,
            })
        );
    } else {
        eprintln!(
            "🛑 account floor {}: {}; shutting down",
            stop.event, stop.detail
        );
    }
}

/// One market-data standby heartbeat, as machine-readable fields.
///
/// The `risk_notification` heartbeat carries the same facts, but only inside its
/// human `message` string. Standby duration is the input to a pre-registered
/// decision (whether the missing recovery hysteresis — Divergence B — is worth
/// implementing: see
/// `docs/evidence/maker-divergence-degradation-review-2026-07-28.md`), and a
/// threshold you can only measure by regexing a sentence is not a threshold.
pub(super) struct MarketDataStandby<'a> {
    pub(super) fault_class: &'a str,
    pub(super) paused_secs: u64,
    pub(super) quoteable_streak: u32,
    pub(super) snapshots_required: u32,
    /// Current mark/mid divergence; `None` when no sample is available or the
    /// fault is transport-class (where divergence is not the trigger).
    pub(super) divergence_bps: Option<f64>,
    pub(super) threshold_bps: f64,
    pub(super) maker_book_empty: bool,
}

/// Emit the standby heartbeat as its own countable event.
///
/// JSON only: the human/quiet paths already print the risk-notification line,
/// and a second sentence per minute would only add noise there.
pub(super) fn emit_market_data_standby(
    output_format: OutputFormat,
    symbol: &str,
    cycle: u64,
    standby: MarketDataStandby<'_>,
) {
    if output_format != OutputFormat::Json {
        return;
    }
    println!(
        "{}",
        serde_json::json!({
            "ts": ts_now(),
            "symbol": symbol,
            "cycle": cycle,
            "action": "market_data_standby",
            "fault_class": standby.fault_class,
            "paused_secs": standby.paused_secs,
            "quoteable_streak": standby.quoteable_streak,
            "snapshots_required": standby.snapshots_required,
            "divergence_bps": standby.divergence_bps.map(|bps| (bps * 100.0).round() / 100.0),
            "threshold_bps": standby.threshold_bps,
            "maker_book_empty": standby.maker_book_empty,
        })
    );
}

/// Residual-position handoff on shutdown (stage 5-b).
pub(super) struct ResidualHandoffReport<'a> {
    pub(super) handoff: &'a super::model::ResidualHandoff,
    pub(super) mark: Option<f64>,
    pub(super) qty_decimals: u32,
    pub(super) exit_reason: &'a str,
}

/// Report what a shutdown left behind, on every exit path.
///
/// The maker never auto-flattens (docs/26 D1/D2), so the exit owes the operator
/// one authoritative number: a venue-confirmed position, a venue-confirmed
/// flat, or an explicit "cannot confirm" — the last of which must read as
/// possible exposure, not as nothing to do.
pub(super) fn emit_residual_position_handoff(
    output_format: OutputFormat,
    symbol: &str,
    cycle: u64,
    report: ResidualHandoffReport<'_>,
) {
    use super::model::ResidualHandoff;
    use maker::format_decimals;

    let ResidualHandoffReport {
        handoff,
        mark,
        qty_decimals,
        exit_reason,
    } = report;
    let reported_position = match handoff {
        ResidualHandoff::Flat => Some(0.0),
        ResidualHandoff::Confirmed { position } => Some(*position),
        ResidualHandoff::Unknown { venue, .. } => *venue,
    };
    let notional = reported_position
        .zip(mark)
        .map(|(position, mark)| (position * mark).abs());
    let side = reported_position.map(|position| {
        if position > 0.0 {
            "long"
        } else if position < 0.0 {
            "short"
        } else {
            "flat"
        }
    });
    if output_format == OutputFormat::Json {
        let (ledger, venue, reason) = match handoff {
            ResidualHandoff::Flat | ResidualHandoff::Confirmed { .. } => {
                (reported_position, reported_position, None)
            }
            ResidualHandoff::Unknown {
                ledger,
                venue,
                reason,
            } => (Some(*ledger), *venue, Some(*reason)),
        };
        println!(
            "{}",
            serde_json::json!({
                "ts": ts_now(),
                "symbol": symbol,
                "cycle": cycle,
                "action": "residual_position",
                "event": handoff.event(),
                "position": reported_position,
                "venue_position": venue,
                "ledger_position": ledger,
                "unknown_reason": reason,
                "side": side,
                "mark": mark,
                "notional": notional,
                "exit_reason": exit_reason,
                "needs_operator": handoff.needs_operator(),
                "auto_flatten": false,
                "detail": "maker never auto-flattens; close or hedge any residual manually",
            })
        );
        return;
    }
    let notional_note = notional.map_or_else(String::new, |n| format!(" (~{n:.2} notional)"));
    match handoff {
        ResidualHandoff::Flat => {
            eprintln!("   ending position flat (venue-confirmed) after {exit_reason}");
        }
        ResidualHandoff::Confirmed { position } => {
            eprintln!(
                "⚠️  residual position handoff: {} {}{} after {} — the maker does NOT auto-flatten; close or hedge it manually",
                side.unwrap_or("?"),
                format_decimals(position.abs(), qty_decimals),
                notional_note,
                exit_reason
            );
        }
        ResidualHandoff::Unknown {
            ledger,
            venue,
            reason,
        } => {
            let venue_note = venue.map_or_else(
                || "unavailable".to_string(),
                |position| format_decimals(position, qty_decimals),
            );
            eprintln!(
                "🛑 residual position UNKNOWN after {exit_reason} ({reason}): venue={venue_note}, session ledger={} — check the venue manually before starting anything else",
                format_decimals(*ledger, qty_decimals)
            );
        }
    }
}

pub(super) fn emit_reconciliation_snapshot_error(
    output_format: OutputFormat,
    symbol: &str,
    cycle: u64,
    message: &str,
) {
    // Precursor signal: a failed reconciliation snapshot inside the freeze
    // window is an early warning that the fail-safe may not converge. Surface
    // it on stdout (JSON mode) so ingest uploads it rather than losing it to
    // local stderr only.
    if output_format == OutputFormat::Json {
        println!(
            "{}",
            serde_json::json!({
                "ts": ts_now(),
                "symbol": symbol,
                "cycle": cycle,
                "action": "position_reconciliation",
                "event": "snapshot_failed",
                "severity": "warning",
                "message": message,
            })
        );
    } else {
        eprintln!("⚠️  bounded position reconciliation snapshot failed: {message}");
    }
}

pub(super) fn emit_ledger_sync(
    output_format: OutputFormat,
    symbol: &str,
    starting_position: f64,
    baseline_mark: f64,
    historical_orders: usize,
    historical_trades: usize,
) {
    if output_format == OutputFormat::Json {
        println!(
            "{}",
            serde_json::json!({
                "ts": ts_now(),
                "symbol": symbol,
                "action": "ledger_sync",
                "event": "complete",
                "starting_position": starting_position,
                "baseline_mark": baseline_mark,
                "pnl_baseline": 0.0,
                "historical_maker_orders": historical_orders,
                "historical_maker_trades_ignored": historical_trades,
                "history_window_seconds": LEDGER_HISTORY_WINDOW_SECS,
                "history_order_limit": ORDER_HISTORY_LIMIT,
                "history_trade_limit": TRADE_LOOKBACK_LIMIT,
                "current_run_fills": 0,
            })
        );
        if starting_position.abs() > f64::EPSILON {
            println!(
                "{}",
                serde_json::json!({
                    "ts": ts_now(),
                    "symbol": symbol,
                    "action": "inventory_adopted",
                    "event": "complete",
                    "starting_position": starting_position,
                    "baseline_mark": baseline_mark,
                    "pnl_baseline": 0.0,
                })
            );
        }
    } else {
        eprintln!(
            "✅ maker ledger synchronized: position={starting_position:+.8}, baseline mark={baseline_mark:.8}, ignored historical fills={historical_trades}"
        );
    }
}

pub(super) fn emit_startup_rejected(
    output_format: OutputFormat,
    symbol: &str,
    position: f64,
    max_position: f64,
) {
    let message = format!(
        "starting position {position:+.8} exceeds max_position {max_position:.8}; refusing live maker"
    );
    if output_format == OutputFormat::Json {
        println!(
            "{}",
            serde_json::json!({
                "ts": ts_now(),
                "symbol": symbol,
                "action": "startup_rejected",
                "event": "position_over_limit",
                "position": position,
                "max_position": max_position,
                "message": message,
            })
        );
    } else {
        eprintln!("⚠️  {message}");
    }
}

/// The current instant as an RFC3339 string, truncated to whole seconds — the
/// timestamp format every maker telemetry line uses.
pub(super) fn ts_now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// As [`ts_now`], but with milliseconds. Only the WS command canary uses this:
/// its whole purpose is to time a request/response round trip, and whole
/// seconds cannot order two events inside the same second.
pub(super) fn ts_now_millis() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Emit a skipped-cycle event. Unlike the previous inline handling, all three
/// reasons — including `MissingTouch` — now produce a JSON event, so an ingest
/// pipeline sees every skip rather than silently missing empty-book cycles.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_cycle_skip(
    output_format: OutputFormat,
    cycle: u64,
    symbol: &str,
    live: bool,
    mark: f64,
    price_decimals: u32,
    max_divergence_bps: f64,
    skip: maker::CycleSkip,
) {
    if output_format == OutputFormat::Json {
        let mut event = serde_json::json!({
            "ts": ts_now(),
            "cycle": cycle,
            "mode": if live { "live" } else { "paper" },
            "symbol": symbol,
            "action": "skip",
            "mark": maker::format_decimals(mark, price_decimals),
        });
        let fields = event.as_object_mut().expect("json object");
        match skip {
            maker::CycleSkip::CrossedBook => {
                fields.insert("reason".into(), "crossed_book".into());
            }
            maker::CycleSkip::MarkMidDivergence { divergence_bps } => {
                fields.insert("reason".into(), "mark_mid_divergence".into());
                fields.insert(
                    "divergence_bps".into(),
                    ((divergence_bps * 100.0).round() / 100.0).into(),
                );
                fields.insert("max_divergence_bps".into(), max_divergence_bps.into());
            }
            maker::CycleSkip::MissingTouch => {
                fields.insert("reason".into(), "missing_touch".into());
            }
        }
        println!("{event}");
        return;
    }
    match skip {
        maker::CycleSkip::CrossedBook => eprintln!(
            "⚠️  #{cycle} crossed order book on {symbol}; skipping cycle (no actions)"
        ),
        maker::CycleSkip::MarkMidDivergence { divergence_bps } => eprintln!(
            "⚠️  #{cycle} mark/mid divergence {divergence_bps:.1}bps > {max_divergence_bps}bps — skipping cycle (no actions)"
        ),
        maker::CycleSkip::MissingTouch => {
            // Fail-safe: without a touch we cannot guarantee no-cross pricing.
            eprintln!("⚠️  #{cycle} empty order book on {symbol}; skipping this cycle")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn balance() -> Balance {
        Balance {
            balance: "100.125".into(),
            cross_available: "80.5".into(),
            cross_balance: "100.125".into(),
            cross_margin: "19.625".into(),
            cross_upnl: "1.25".into(),
            equity: "101.375".into(),
            isolated_balance: "0".into(),
            isolated_upnl: "0".into(),
            locked: "0".into(),
            pnl_24h: "2.5".into(),
            pnl_freeze: "0".into(),
            upnl: "1.25".into(),
        }
    }

    #[test]
    fn account_snapshot_uses_real_balance_fields() {
        let json = account_json(&balance());
        assert_eq!(json["balance"], "100.125");
        assert_eq!(json["equity"], "101.375");
        assert_eq!(json["available"], "80.5");
        assert_eq!(json["upnl"], "1.25");
    }

    #[test]
    fn account_amounts_are_compact_without_hiding_invalid_values() {
        assert_eq!(format_account_amount("101.375"), "101.38");
        assert_eq!(format_account_amount("-0.005"), "-0.01");
        assert_eq!(format_account_amount("unavailable"), "unavailable");
    }

    #[test]
    fn fill_excess_distinguishes_missing_sample_from_zero_divergence() {
        assert!(fill_excess_bps_json(None).is_null());
        assert_eq!(fill_excess_bps_json(Some(0.0)), serde_json::json!(0.0));
    }

    #[test]
    fn phase_one_performance_json_exposes_cashflow_capture_and_inventory_time() {
        let mut ledger = maker::PerformanceLedger::new(0.0, 100.0).unwrap();
        ledger.observe_market(0, 100.0).unwrap();
        ledger
            .record_fill(maker::PerformanceFill {
                trade_id: 1,
                order_id: 2,
                role: maker::FillRole::PassiveMaker,
                side: OrderSide::Buy,
                price: 99.0,
                qty: 1.0,
                mark_at_fill: 100.0,
                event_time_ms: 0,
                costs: Some(maker::ExecutionCosts::default()),
            })
            .unwrap();
        ledger.finish(1_000).unwrap();
        let json = performance_json(&ledger.summary(100.0).unwrap());

        assert_eq!(json["passive_cashflow_quote"], -99.0);
        assert!(json["passive_capture_bps"].as_f64().unwrap() > 100.0);
        assert_eq!(json["position"], 1.0);
        assert_eq!(json["inventory_nonzero_ms"], 1_000);
        assert_eq!(json["inventory_abs_qty_ms"], 1_000.0);
    }

    #[test]
    fn latency_summary_json_has_flat_dashboard_fields_and_symbol() {
        let metric = maker::LatencyMetricSummary {
            samples: 3,
            p50_ms: Some(10),
            p95_ms: Some(20),
            p99_ms: Some(30),
        };
        let summary = maker::LatencySummary {
            kind: maker::LatencyRequestKind::Cancel,
            requests: 3,
            accepted: 1,
            rejected: 0,
            effective: 1,
            timeout: 1,
            invalidated: 0,
            process_ended: 0,
            pending: 0,
            reject_rate: 0.0,
            timeout_rate: 1.0 / 3.0,
            write: metric,
            ack: metric,
            effective_latency: metric,
            fill_after_cancel: metric,
        };
        let json = latency_summary_json("XAG-USD", &summary);

        assert_eq!(json["symbol"], "XAG-USD");
        assert_eq!(json["kind"], "cancel");
        assert_eq!(json["ack_p95_ms"], 20);
        assert_eq!(json["effective_latency_p99_ms"], 30);
        assert_eq!(json["fill_after_cancel_p50_ms"], 10);
        assert_eq!(json["ack"]["p95_ms"], 20);
    }

    #[test]
    fn ws_snapshot_json_exposes_raw_times_and_skew_measurements() {
        let diagnostics = WsSnapshotDiagnostics {
            mark_seq: Some(10),
            book_seq: Some(20),
            mark_server_time: Some("2026-07-15T00:00:01Z".to_string()),
            book_server_time: Some("2026-07-15T00:00:03Z".to_string()),
            mark_envelope_time: Some("1752537601000".to_string()),
            book_envelope_time: Some("1752537603000".to_string()),
            mark_payload_time: Some("2026-07-15T00:00:01Z".to_string()),
            book_payload_time: Some("2026-07-15T00:00:02Z".to_string()),
            mark_age_ms: Some(250),
            book_age_ms: Some(50),
            local_skew_ms: Some(200),
            server_skew_ms: Some(2_000),
        };

        let json = ws_snapshot_json(&diagnostics);

        assert_eq!(json["mark_seq"], 10);
        assert_eq!(json["book_seq"], 20);
        assert_eq!(json["mark_age_ms"], 250);
        assert_eq!(json["local_skew_ms"], 200);
        assert_eq!(json["server_skew_ms"], 2_000);
        assert_eq!(json["book_payload_time"], "2026-07-15T00:00:02Z");
    }

    #[test]
    fn cycle_summary_adaptive_fields_are_additive_and_top_level() {
        let decision = maker::SpreadDecision {
            enabled: true,
            tier: 2,
            rolling_vol_bps: 20.126,
            effective_spread_bps: 18.0,
            effective_refresh_bps: 6.0,
        };
        let json = with_spread_fields(
            serde_json::json!({"action": "cycle_summary", "vol_bps": null}),
            &decision,
        );

        assert_eq!(json["action"], "cycle_summary");
        assert!(json["vol_bps"].is_null());
        assert_eq!(json["rolling_vol_bps"], 20.13);
        assert_eq!(json["adaptive_spread_enabled"], true);
        assert_eq!(json["adaptive_spread_tier"], 2);
        assert_eq!(json["effective_spread_bps"], 18.0);
        assert_eq!(json["effective_refresh_bps"], 6.0);
    }

    #[test]
    fn cycle_summary_book_and_tape_fields_are_additive_and_top_level() {
        let base = serde_json::json!({
            "action": "cycle_summary",
            "vol_bps": null,
            "best_bid": 99.9,
            "best_ask": 100.1,
        });
        let original = base.clone();
        let telemetry = MarketTelemetrySnapshot {
            book: super::super::feed::BookTelemetrySnapshot {
                bid_levels: Some(vec![(99.9, 2.0), (99.8, 3.0)]),
                ask_levels: Some(vec![(100.1, 4.0), (100.2, 5.0)]),
                age_ms: Some(125),
            },
            tape: super::super::feed::TapeTelemetrySnapshot {
                count_5s: 3,
                buy_qty_5s: 1.25,
                sell_qty_5s: 2.5,
                unknown_qty_5s: 0.75,
                last_trade_age_ms: Some(50),
            },
        };

        let json = with_book_fields(base, &telemetry, 100.0, Some(99.9), Some(100.1));

        for (key, value) in original.as_object().unwrap() {
            assert_eq!(&json[key], value, "existing field changed: {key}");
        }
        assert_eq!(
            json.as_object().unwrap().len(),
            original.as_object().unwrap().len() + 2
        );
        assert_eq!(
            json["book"]["bid_levels"][0],
            serde_json::json!([99.9, 2.0])
        );
        assert_eq!(
            json["book"]["ask_levels"][0],
            serde_json::json!([100.1, 4.0])
        );
        assert_eq!(json["book"]["bid_qty_top"], 2.0);
        assert_eq!(json["book"]["ask_qty_top"], 4.0);
        assert!((json["book"]["spread_bps"].as_f64().unwrap() - 20.0).abs() < 1e-9);
        assert_eq!(json["book"]["mark_mid_divergence_bps"], 0.0);
        assert_eq!(json["book"]["age_ms"], 125);
        assert_eq!(json["tape"]["count_5s"], 3);
        assert_eq!(json["tape"]["buy_qty_5s"], 1.25);
        assert_eq!(json["tape"]["sell_qty_5s"], 2.5);
        assert_eq!(json["tape"]["unknown_qty_5s"], 0.75);
        assert_eq!(json["tape"]["last_trade_age_ms"], 50);
        assert!(json.get("bid_levels").is_none());
        assert!(json.get("count_5s").is_none());
    }

    #[test]
    fn cycle_summary_book_and_tape_observation_gaps_are_null_or_zero() {
        let json = with_book_fields(
            serde_json::json!({"action": "cycle_summary"}),
            &MarketTelemetrySnapshot::default(),
            100.0,
            None,
            None,
        );

        assert!(json["book"]["bid_levels"].is_null());
        assert!(json["book"]["ask_levels"].is_null());
        assert!(json["book"]["bid_qty_top"].is_null());
        assert!(json["book"]["ask_qty_top"].is_null());
        assert!(json["book"]["spread_bps"].is_null());
        assert!(json["book"]["mark_mid_divergence_bps"].is_null());
        assert!(json["book"]["age_ms"].is_null());
        assert_eq!(json["tape"]["count_5s"], 0);
        assert_eq!(json["tape"]["unknown_qty_5s"], 0.0);
        assert!(json["tape"]["last_trade_age_ms"].is_null());
    }

    #[test]
    fn cycle_summary_geometry_fields_are_additive_and_top_level() {
        let base = serde_json::json!({
            "action": "cycle_summary",
            "places": 1,
            "best_ask": 100.0,
        });
        let original = base.clone();
        let geometry = vec![
            maker::QuoteGeometry {
                side: OrderSide::Buy,
                level: 0,
                raw_price: 99.9,
                final_price: Some(99.99),
                outcome: maker::QuoteGeometryOutcome::ClampedToTouch,
                distance_to_touch_bps: Some(1.0),
                band_edge_bps: 20.0,
            },
            maker::QuoteGeometry {
                side: OrderSide::Sell,
                level: 0,
                raw_price: 100.3,
                final_price: Some(100.2),
                outcome: maker::QuoteGeometryOutcome::ClampedToBand,
                distance_to_touch_bps: Some(21.0),
                band_edge_bps: 20.0,
            },
            maker::QuoteGeometry {
                side: OrderSide::Buy,
                level: 1,
                raw_price: 99.8,
                final_price: None,
                outcome: maker::QuoteGeometryOutcome::DroppedInfeasible,
                distance_to_touch_bps: None,
                band_edge_bps: 20.0,
            },
        ];

        let json = with_geometry_fields(base, &geometry, 100.0);

        for (key, value) in original.as_object().unwrap() {
            assert_eq!(&json[key], value, "existing field changed: {key}");
        }
        assert_eq!(
            json.as_object().unwrap().len(),
            original.as_object().unwrap().len() + 1
        );
        assert_eq!(json["geometry"]["min_distance_to_touch_bps"], 1.0);
        assert_eq!(json["geometry"]["clamped_to_touch"], 1);
        assert_eq!(json["geometry"]["clamped_to_band"], 1);
        assert_eq!(json["geometry"]["dropped_infeasible"], 1);
        assert_eq!(json["geometry"]["quotes"].as_array().unwrap().len(), 3);
        assert_eq!(json["geometry"]["quotes"][0]["side"], "buy");
        assert_eq!(json["geometry"]["quotes"][0]["outcome"], "clamped_to_touch");
        assert!((json["geometry"]["quotes"][0]["raw_bps"].as_f64().unwrap() - 10.0).abs() < 1e-9);
        assert!((json["geometry"]["quotes"][0]["final_bps"].as_f64().unwrap() - 1.0).abs() < 1e-9);
        assert_eq!(json["geometry"]["quotes"][0]["dist_touch_bps"], 1.0);
        assert_eq!(json["geometry"]["quotes"][0]["band_edge_bps"], 20.0);
        // Dropped slots still carry the band edge; only the fill-risk distance
        // is unknowable without a resting price.
        assert_eq!(json["geometry"]["quotes"][2]["band_edge_bps"], 20.0);
        assert!(json["geometry"]["quotes"][2]["dist_touch_bps"].is_null());
        assert!(json.get("clamped_to_touch").is_none());
        assert!(json.get("quotes").is_none());
    }

    #[test]
    fn cycle_summary_size_skew_fields_are_additive_and_top_level() {
        let decision = maker::SizeSkewDecision {
            enabled: true,
            active: true,
            add_side: Some(OrderSide::Buy),
            inventory_ratio: 0.3,
            add_qty: Some(0.05),
        };
        let json = with_size_skew_fields(
            serde_json::json!({"action": "cycle_summary", "vol_bps": null}),
            &decision,
        );

        assert_eq!(json["action"], "cycle_summary");
        assert!(json["vol_bps"].is_null());
        assert_eq!(json["size_skew_enabled"], true);
        assert_eq!(json["size_skew_active"], true);
        assert_eq!(json["size_skew_add_side"], "buy");
        assert_eq!(json["size_skew_inventory_ratio"], 0.3);
        assert_eq!(json["size_skew_add_qty"], 0.05);
    }

    #[test]
    fn cycle_summary_guard_fields_are_additive_and_top_level() {
        let decision = maker::GuardDecision {
            enabled: true,
            active: true,
            endangered: Some(OrderSide::Sell),
            divergence_bps: Some(7.816),
        };
        let json = with_guard_fields(
            serde_json::json!({"action": "cycle_summary", "vol_bps": null}),
            &decision,
            Some(-14.2),
            3.25,
            1.75,
            4.804,
        );

        assert_eq!(json["action"], "cycle_summary");
        assert!(json["vol_bps"].is_null());
        assert_eq!(json["guard_enabled"], true);
        assert_eq!(json["guard_active"], true);
        assert_eq!(json["guard_side"], "sell");
        assert_eq!(json["external_divergence_bps"], 7.82);
        assert_eq!(json["external_basis_bps"], -14.2);
        assert_eq!(json["external_skew_shift_bps"], 3.25);
        assert_eq!(json["micro_price_shift_bps"], 1.75);
        assert_eq!(json["skew_shift_bps"], 4.8);

        // Inactive guard serializes nulls, never drops the keys.
        let idle = with_guard_fields(
            serde_json::json!({"action": "cycle_summary"}),
            &maker::GuardDecision::INACTIVE,
            None,
            0.0,
            0.0,
            0.0,
        );
        assert_eq!(idle["guard_enabled"], false);
        assert_eq!(idle["guard_active"], false);
        assert!(idle["guard_side"].is_null());
        assert!(idle["external_divergence_bps"].is_null());
        assert!(idle["external_basis_bps"].is_null());
        assert_eq!(idle["external_skew_shift_bps"], 0.0);
        assert_eq!(idle["micro_price_shift_bps"], 0.0);
        assert_eq!(idle["skew_shift_bps"], 0.0);
    }

    /// Stage 5-b: the three exit keys are always present (null when idle) so a
    /// consumer can distinguish "no exit this cycle" from "field missing on an
    /// older run", and they never disturb the existing summary keys.
    #[test]
    fn cycle_summary_exit_fields_are_additive_and_always_present() {
        let idle = with_exit_fields(
            serde_json::json!({"action": "cycle_summary", "vol_bps": null}),
            ExitStatus::default(),
        );
        assert_eq!(idle["action"], "cycle_summary");
        assert!(idle["vol_bps"].is_null());
        assert!(idle["exit_kind"].is_null());
        assert_eq!(idle["exit_submitted"], false);
        assert!(idle["exit_suppressed"].is_null());

        let submitted = with_exit_fields(
            serde_json::json!({"action": "cycle_summary"}),
            ExitStatus {
                kind: Some(maker::ExitKind::WindDown),
                submitted: true,
                suppressed: None,
            },
        );
        assert_eq!(submitted["exit_kind"], "wind_down");
        assert_eq!(submitted["exit_submitted"], true);
        assert!(submitted["exit_suppressed"].is_null());

        let suppressed = with_exit_fields(
            serde_json::json!({"action": "cycle_summary"}),
            ExitStatus {
                kind: Some(maker::ExitKind::InventoryTrim),
                submitted: false,
                suppressed: Some(maker::SuppressedExit {
                    kind: maker::ExitKind::InventoryTrim,
                    reason: maker::ExitSuppression::VolatilityHalt,
                }),
            },
        );
        assert_eq!(suppressed["exit_kind"], "inventory_trim");
        assert_eq!(suppressed["exit_submitted"], false);
        assert_eq!(suppressed["exit_suppressed"], "volatility_halt");
    }
}
