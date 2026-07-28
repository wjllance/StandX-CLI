//! SDK payload adapter for the pure maker ledger.

use super::model::{parse_decimal, Decimal};
use anyhow::Result;
use standx_maker::{ExecutionCosts, LedgerTrade, MakerFill, MakerLedger, MakerStats, TradeSource};
use standx_sdk::account_stream::{OrderUpdate, TradeUpdate};
use standx_sdk::models::{FundingHistoryEntry, Order, OrderSide, Trade};

pub(super) fn adopt_order(
    ledger: &mut MakerLedger,
    order: &Order,
    run_order_prefix: &str,
) -> Result<bool> {
    let client_order_id = order.cl_ord_id.as_deref();
    if !standx_maker::is_current_run_client_order_id(client_order_id, run_order_prefix) {
        return Ok(false);
    }
    let order_id = order.id.parse::<u64>().map_err(|_| {
        anyhow::anyhow!(
            "current-run maker order has non-integer exchange ID '{}'",
            order.id
        )
    })?;
    Ok(ledger.adopt_order(order_id, client_order_id, run_order_prefix))
}

pub(super) fn apply_order_update(
    ledger: &mut MakerLedger,
    update: &OrderUpdate,
    symbol: &str,
    run_order_prefix: &str,
    stats: &mut MakerStats,
    fills: &mut Vec<MakerFill>,
) -> Result<bool> {
    if update.symbol != symbol {
        return Ok(false);
    }
    if !ledger.adopt_order(
        update.order_id,
        update.cl_ord_id.as_deref(),
        run_order_prefix,
    ) {
        return Ok(false);
    }
    let exit = ledger.is_exit_order(update.order_id);
    let buffered = ledger.apply_buffered_trades(update.order_id, stats)?;
    let saw_exit_fill = exit && !buffered.is_empty();
    fills.extend(buffered);
    // The cumulative fill fields in an order callback are deliberately not
    // booked here. Only a stable-ID TradeUpdate or REST trade may mutate PnL
    // and expected position, so the order update needs no mark.
    Ok(saw_exit_fill)
}

pub(super) fn apply_account_trade(
    ledger: &mut MakerLedger,
    trade: TradeUpdate,
    symbol: &str,
    mark: f64,
    stats: &mut MakerStats,
    fills: &mut Vec<MakerFill>,
) -> Result<bool> {
    if !trade.symbol.eq_ignore_ascii_case(symbol) {
        return Ok(false);
    }
    let (price, qty) = trade_values(trade.trade_id, &trade.price, &trade.qty)?;
    let event_time_ms = trade_time_ms(trade.trade_id, &trade.trade_ts)?;
    apply_ledger_trade(
        ledger,
        LedgerTrade {
            trade_id: trade.trade_id,
            order_id: trade.order_id,
            side: trade.side,
            price,
            qty,
            mark,
            trade_ts: &trade.trade_ts,
            event_time_ms,
            costs: None,
            source: TradeSource::AccountStream,
        },
        stats,
        fills,
    )
}

pub(super) fn apply_rest_trade(
    ledger: &mut MakerLedger,
    trade: Trade,
    session_started_at: i64,
    now: i64,
    mark: f64,
    stats: &mut MakerStats,
    fills: &mut Vec<MakerFill>,
) -> Result<bool> {
    let Some(order_id) = trade.order_id else {
        return Ok(false);
    };
    if trade.id == 0 {
        return Err(anyhow::anyhow!(
            "maker fill for order {} has no stable trade ID",
            order_id
        ));
    }
    if !trade_is_in_session(&trade, session_started_at, now)? {
        return Err(anyhow::anyhow!(
            "current-run maker trade {} falls outside the session time boundary",
            trade.id
        ));
    }
    let (side, price, qty) = maker_trade_fill(&trade)?;
    let event_time_ms = trade_time_ms(trade.id, &trade.time)?;
    let costs = rest_execution_costs(&trade)?;
    apply_ledger_trade(
        ledger,
        LedgerTrade {
            trade_id: trade.id,
            order_id,
            side,
            price,
            qty,
            mark,
            trade_ts: &trade.time,
            event_time_ms,
            costs,
            source: TradeSource::RestBackfill,
        },
        stats,
        fills,
    )
}

fn apply_ledger_trade(
    ledger: &mut MakerLedger,
    trade: LedgerTrade<'_>,
    stats: &mut MakerStats,
    fills: &mut Vec<MakerFill>,
) -> Result<bool> {
    let exit = ledger.is_exit_order(trade.order_id);
    if let Some(fill) = ledger.record_trade(trade, stats)? {
        fills.push(fill);
        return Ok(exit);
    }
    Ok(false)
}

fn trade_is_in_session(trade: &Trade, session_started_at: i64, now: i64) -> Result<bool> {
    let timestamp = chrono::DateTime::parse_from_rfc3339(&trade.time).map_err(|_| {
        anyhow::anyhow!(
            "maker trade {} has invalid RFC3339 timestamp '{}'",
            trade.id,
            trade.time
        )
    })?;
    let timestamp = timestamp.timestamp();
    Ok(timestamp >= session_started_at && timestamp <= now)
}

pub(super) fn maker_trade_fill(trade: &Trade) -> Result<(OrderSide, f64, f64)> {
    let side = match trade.side.as_deref() {
        Some(side) if side.eq_ignore_ascii_case("buy") => OrderSide::Buy,
        Some(side) if side.eq_ignore_ascii_case("sell") => OrderSide::Sell,
        _ => {
            return Err(anyhow::anyhow!(
                "maker trade {} is missing a valid side",
                trade.id
            ));
        }
    };
    let (price, qty) = trade_values(trade.id, &trade.price, &trade.qty)?;
    Ok((side, price, qty))
}

fn trade_values(trade_id: u64, price: &str, qty: &str) -> Result<(f64, f64)> {
    let price = parse_decimal(
        &format!("maker trade {trade_id} price"),
        price,
        Decimal::Positive,
    )?;
    let qty = parse_decimal(
        &format!("maker trade {trade_id} qty"),
        qty,
        Decimal::Positive,
    )?;
    Ok((price, qty))
}

fn trade_time_ms(trade_id: u64, value: &str) -> Result<i64> {
    if let Ok(timestamp) = chrono::DateTime::parse_from_rfc3339(value) {
        return Ok(timestamp.timestamp_millis());
    }
    let raw = value
        .parse::<i64>()
        .map_err(|_| anyhow::anyhow!("maker trade {trade_id} has invalid timestamp '{value}'"))?;
    Ok(if raw.abs() < 1_000_000_000_000 {
        raw.saturating_mul(1_000)
    } else {
        raw
    })
}

/// Row kind carrying a funding cashflow. Other kinds may appear on the same
/// endpoint; they are not funding and must not enter the attribution.
const FUNDING_TXN_TYPE: &str = "funding_fee";

/// Fold a funding-history batch into the performance ledger.
///
/// Dedup is by row id against `applied`, not by request cursor: the venue's
/// `last_id` parameter pages *backward* into history (verified 2026-07-28), so
/// every audit re-reads the same recent page and this function is what makes
/// re-reading harmless. `record_funding` has no dedup of its own — it just
/// accumulates — so this set is the only thing standing between a re-read and a
/// double-counted cashflow.
///
/// A row that exists but cannot be folded in — settlement asset that is neither
/// the quote nor its D-prefixed form, or an out-of-order arrival the monotonic
/// ledger rejects — is **counted** as unattributed rather than dropped, which
/// clears `net_pnl_complete`. Silently skipping it would let the summary claim a
/// completeness the numbers do not have.
pub(super) fn apply_funding_history(
    ledger: &mut MakerLedger,
    entries: &[FundingHistoryEntry],
    symbol: &str,
    session_started_at: i64,
    applied: &mut std::collections::HashSet<i64>,
) -> Result<()> {
    let quote = symbol.rsplit_once('-').map(|(_, quote)| quote);
    let mut fresh: Vec<(i64, i64, f64, bool)> = Vec::new();
    for entry in entries {
        if applied.contains(&entry.id)
            || !entry.txn_type.eq_ignore_ascii_case(FUNDING_TXN_TYPE)
            || !entry.symbol.eq_ignore_ascii_case(symbol)
        {
            continue;
        }
        let event_time = chrono::DateTime::parse_from_rfc3339(&entry.created_at).map_err(|_| {
            anyhow::anyhow!(
                "funding row {} has invalid RFC3339 created_at '{}'",
                entry.id,
                entry.created_at
            )
        })?;
        // Funding from before this session belongs to whoever held the position
        // then, not to this run's attribution. Marked applied so it is not
        // re-examined on every audit for the life of the session.
        if event_time.timestamp() < session_started_at {
            applied.insert(entry.id);
            continue;
        }
        let qty = parse_decimal(
            &format!("funding row {} qty", entry.id),
            &entry.qty,
            Decimal::Finite,
        )?;
        let convertible = quote.is_some_and(|quote| {
            entry.asset.eq_ignore_ascii_case(quote)
                || entry.asset.eq_ignore_ascii_case(&format!("D{quote}"))
        });
        fresh.push((event_time.timestamp_millis(), entry.id, qty, convertible));
    }
    // The endpoint returns newest-first and the ledger rejects time
    // regressions, so apply chronologically.
    fresh.sort_unstable_by_key(|(event_time_ms, id, _, _)| (*event_time_ms, *id));
    let Some(performance) = ledger.performance_mut() else {
        // Performance accounting is disabled; nothing can consume these rows,
        // and leaving them unmarked keeps the state honest if it comes back.
        return Ok(());
    };
    for (event_time_ms, id, qty, convertible) in fresh {
        if convertible {
            match performance.record_funding(event_time_ms, qty) {
                Ok(()) => {}
                Err(error) => {
                    // An out-of-order arrival must not stop the maker: funding is
                    // attribution, not position accounting. Count it as
                    // unattributed so the incompleteness is visible instead.
                    eprintln!("⚠️  funding row {id} not attributed: {error}");
                    performance.record_unattributed_funding();
                }
            }
        } else {
            performance.record_unattributed_funding();
        }
        applied.insert(id);
    }
    Ok(())
}

/// REST is currently the only fill source carrying fee fields. Convert only
/// the symbol's quote asset (including StandX's D-prefixed settlement asset);
/// all other assets remain unavailable for an explicit later conversion.
fn rest_execution_costs(trade: &Trade) -> Result<Option<ExecutionCosts>> {
    let Some(raw_qty) = trade.fee_qty.as_deref() else {
        return Ok(None);
    };
    // A rebate is a negative fee, so only NaN/inf are rejected here.
    let fee = parse_decimal(
        &format!("maker trade {} fee qty", trade.id),
        raw_qty,
        Decimal::Finite,
    )?;
    let (Some(asset), Some(symbol)) = (trade.fee_asset.as_deref(), trade.symbol.as_deref()) else {
        return Ok(None);
    };
    let Some((_, quote)) = symbol.rsplit_once('-') else {
        return Ok(None);
    };
    if !asset.eq_ignore_ascii_case(quote) && !asset.eq_ignore_ascii_case(&format!("D{quote}")) {
        return Ok(None);
    }
    Ok(Some(if fee >= 0.0 {
        ExecutionCosts {
            fee_quote: fee,
            rebate_quote: 0.0,
        }
    } else {
        ExecutionCosts {
            fee_quote: 0.0,
            rebate_quote: -fee,
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use standx_sdk::models::OrderStatus;

    fn funding_row(id: i64, created_at: &str, qty: &str) -> FundingHistoryEntry {
        FundingHistoryEntry {
            id,
            user: "solana_test".to_string(),
            asset: "DUSD".to_string(),
            symbol: "HYPE-USD".to_string(),
            qty: qty.to_string(),
            txn_type: "funding_fee".to_string(),
            created_at: created_at.to_string(),
            updated_at: created_at.to_string(),
            transact_time: None,
        }
    }

    fn funding_ledger() -> MakerLedger {
        let mut ledger = MakerLedger::new(0.0);
        ledger
            .enable_performance(55.0)
            .expect("performance enabled");
        ledger
    }

    /// The venue's `last_id` pages backward, so every audit re-reads the same
    /// recent page. Without id dedup the session would double-count on the very
    /// next audit — `record_funding` only accumulates, it never dedups.
    #[test]
    fn repeated_funding_pages_are_counted_once() {
        let mut ledger = funding_ledger();
        let mut applied = std::collections::HashSet::new();
        // Newest-first, exactly as the endpoint returns it.
        let page = vec![
            funding_row(3, "2026-07-27T03:00:00.254907Z", "0.000074457"),
            funding_row(2, "2026-07-27T01:00:00.179068Z", "-0.000119095"),
            funding_row(1, "2026-07-27T00:00:00.273249Z", "-0.000074569"),
        ];
        let session = 1_784_000_000; // well before the rows

        apply_funding_history(&mut ledger, &page, "HYPE-USD", session, &mut applied).unwrap();
        let first = ledger.performance().unwrap().summary(55.0).unwrap();
        assert!(first.funding_available);
        assert_eq!(first.funding_unattributed, 0);
        let expected = 0.000074457 - 0.000119095 - 0.000074569;
        assert!((first.funding_quote - expected).abs() < 1e-12);

        // Same page again, plus one genuinely new row.
        let mut page2 = page.clone();
        page2.insert(
            0,
            funding_row(4, "2026-07-27T04:00:00.000000Z", "-0.000070000"),
        );
        apply_funding_history(&mut ledger, &page2, "HYPE-USD", session, &mut applied).unwrap();
        let second = ledger.performance().unwrap().summary(55.0).unwrap();
        assert!((second.funding_quote - (expected - 0.000070000)).abs() < 1e-12);
        assert_eq!(second.funding_unattributed, 0);
    }

    /// Rows outside this session, other symbols, and other row kinds are not
    /// this session's cashflow — and none of them may clear completeness.
    #[test]
    fn funding_filters_session_symbol_and_row_kind() {
        let mut ledger = funding_ledger();
        let mut applied = std::collections::HashSet::new();
        // Session starts 2026-07-27T02:00:00Z.
        let session = chrono::DateTime::parse_from_rfc3339("2026-07-27T02:00:00Z")
            .unwrap()
            .timestamp();
        let mut other_symbol = funding_row(11, "2026-07-27T03:00:00Z", "-0.5");
        other_symbol.symbol = "BTC-USD".to_string();
        let mut other_kind = funding_row(12, "2026-07-27T03:00:00Z", "-0.5");
        other_kind.txn_type = "realized_pnl".to_string();
        let rows = vec![
            funding_row(10, "2026-07-27T01:00:00Z", "-0.5"), // before session
            other_symbol,
            other_kind,
            funding_row(13, "2026-07-27T03:00:00Z", "-0.000070000"), // the only one that counts
        ];

        apply_funding_history(&mut ledger, &rows, "HYPE-USD", session, &mut applied).unwrap();
        let summary = ledger.performance().unwrap().summary(55.0).unwrap();
        assert!((summary.funding_quote - -0.000070000).abs() < 1e-12);
        assert_eq!(summary.funding_unattributed, 0);
        // Pre-session rows are remembered so they are not re-examined forever;
        // rows for other symbols/kinds are simply never ours.
        assert!(applied.contains(&10));
        assert!(applied.contains(&13));
    }

    /// A cashflow we cannot express in quote currency must be counted, not
    /// dropped: `net_pnl_complete` has to stop claiming completeness.
    #[test]
    fn unconvertible_funding_asset_clears_net_pnl_complete() {
        let mut ledger = funding_ledger();
        let mut applied = std::collections::HashSet::new();
        let mut exotic = funding_row(20, "2026-07-27T03:00:00Z", "-0.004");
        exotic.asset = "SOL".to_string();

        apply_funding_history(&mut ledger, &[exotic], "HYPE-USD", 0, &mut applied).unwrap();
        let summary = ledger.performance().unwrap().summary(55.0).unwrap();
        assert_eq!(summary.funding_quote, 0.0);
        assert!(!summary.funding_available);
        assert_eq!(summary.funding_unattributed, 1);
        assert!(!summary.net_pnl_complete);
        assert!(applied.contains(&20));
    }

    /// `DUSD` is the settlement form of `USD` — the same D-prefix rule the fee
    /// converter uses. Getting this wrong would silently zero out all funding.
    #[test]
    fn d_prefixed_settlement_asset_is_the_quote_asset() {
        let mut ledger = funding_ledger();
        let mut applied = std::collections::HashSet::new();
        let mut plain = funding_row(30, "2026-07-27T03:00:00Z", "-0.001");
        plain.asset = "USD".to_string();

        apply_funding_history(&mut ledger, &[plain], "HYPE-USD", 0, &mut applied).unwrap();
        apply_funding_history(
            &mut ledger,
            &[funding_row(31, "2026-07-27T04:00:00Z", "-0.002")],
            "HYPE-USD",
            0,
            &mut applied,
        )
        .unwrap();
        let summary = ledger.performance().unwrap().summary(55.0).unwrap();
        assert!((summary.funding_quote - -0.003).abs() < 1e-12);
        assert_eq!(summary.funding_unattributed, 0);
    }

    /// An out-of-order arrival must not stop the maker — funding is
    /// attribution, not position accounting — but it must not vanish either.
    #[test]
    fn out_of_order_funding_is_counted_not_fatal() {
        let mut ledger = funding_ledger();
        let mut applied = std::collections::HashSet::new();
        apply_funding_history(
            &mut ledger,
            &[funding_row(40, "2026-07-27T05:00:00Z", "-0.001")],
            "HYPE-USD",
            0,
            &mut applied,
        )
        .unwrap();
        // A late row stamped earlier than what the ledger already accepted.
        apply_funding_history(
            &mut ledger,
            &[funding_row(41, "2026-07-27T04:00:00Z", "-0.002")],
            "HYPE-USD",
            0,
            &mut applied,
        )
        .expect("a late funding row is not a fatal error");
        let summary = ledger.performance().unwrap().summary(55.0).unwrap();
        assert!((summary.funding_quote - -0.001).abs() < 1e-12);
        assert_eq!(summary.funding_unattributed, 1);
        assert!(!summary.net_pnl_complete);
    }

    #[test]
    fn malformed_funding_rows_fail_closed() {
        let mut ledger = funding_ledger();
        let mut applied = std::collections::HashSet::new();
        let bad_time = funding_row(50, "not-a-timestamp", "-0.001");
        assert!(
            apply_funding_history(&mut ledger, &[bad_time], "HYPE-USD", 0, &mut applied).is_err()
        );
        let bad_qty = funding_row(51, "2026-07-27T03:00:00Z", "not-a-number");
        assert!(
            apply_funding_history(&mut ledger, &[bad_qty], "HYPE-USD", 0, &mut applied).is_err()
        );
    }

    fn order_update(side: OrderSide, fill_qty: &str) -> OrderUpdate {
        OrderUpdate {
            seq: 1,
            order_id: 7,
            cl_ord_id: Some("sxmk-run-q00000001a0".to_string()),
            symbol: "BTC-USD".to_string(),
            side,
            qty: "0.20".to_string(),
            fill_qty: fill_qty.to_string(),
            fill_avg_price: "100.00".to_string(),
            price: "100.00".to_string(),
            status: OrderStatus::Filled,
            reduce_only: false,
            updated_at: "2026-07-13T00:00:00Z".to_string(),
        }
    }

    fn trade_update(side: OrderSide, price: &str, qty: &str) -> TradeUpdate {
        TradeUpdate {
            seq: 2,
            trade_id: 11,
            order_id: 7,
            symbol: "BTC-USD".to_string(),
            side,
            price: price.to_string(),
            qty: qty.to_string(),
            trade_ts: "2026-07-13T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn typed_account_trade_is_the_only_order_callback_accounting_path() {
        let mut ledger = MakerLedger::new(0.0);
        let mut stats = MakerStats::default();
        let mut fills = Vec::new();

        apply_order_update(
            &mut ledger,
            &order_update(OrderSide::Sell, "-0.20"),
            "BTC-USD",
            "sxmk-run-",
            &mut stats,
            &mut fills,
        )
        .unwrap();

        assert!(
            fills.is_empty(),
            "cumulative order fills must not be booked"
        );
        apply_account_trade(
            &mut ledger,
            trade_update(OrderSide::Sell, "100.00", "0.20"),
            "BTC-USD",
            100.0,
            &mut stats,
            &mut fills,
        )
        .unwrap();

        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].side, OrderSide::Sell);
        assert!((fills[0].qty - 0.20).abs() < 1e-9);
        assert!((ledger.expected_position + 0.20).abs() < 1e-9);
    }

    #[test]
    fn non_finite_typed_trade_quantity_is_rejected_explicitly() {
        let mut ledger = MakerLedger::new(0.0);
        let mut stats = MakerStats::default();
        let mut fills = Vec::new();

        apply_order_update(
            &mut ledger,
            &order_update(OrderSide::Sell, "NaN"),
            "BTC-USD",
            "sxmk-run-",
            &mut stats,
            &mut fills,
        )
        .unwrap();
        let error = apply_account_trade(
            &mut ledger,
            trade_update(OrderSide::Sell, "100.00", "NaN"),
            "BTC-USD",
            100.0,
            &mut stats,
            &mut fills,
        )
        .unwrap_err();

        // The converged message must still name the offending field and value.
        assert!(
            error
                .to_string()
                .contains("qty 'NaN' is not a finite number > 0"),
            "unexpected rejection message: {error}"
        );
    }

    #[test]
    fn partial_fill_then_cancelled_keeps_ledger_and_stats_positions_aligned() {
        let mut ledger = MakerLedger::new(0.0);
        let mut stats = MakerStats::default();
        let mut fills = Vec::new();
        let mut update = order_update(OrderSide::Buy, "0.10");
        update.status = OrderStatus::Canceled;

        apply_order_update(
            &mut ledger,
            &update,
            "BTC-USD",
            "sxmk-run-",
            &mut stats,
            &mut fills,
        )
        .unwrap();

        apply_account_trade(
            &mut ledger,
            trade_update(OrderSide::Buy, "100.00", "0.10"),
            "BTC-USD",
            100.0,
            &mut stats,
            &mut fills,
        )
        .unwrap();

        assert_eq!(fills.len(), 1);
        assert!((ledger.expected_position - 0.10).abs() < 1e-9);
        assert!((stats.position() - ledger.expected_position).abs() < 1e-9);
        assert!(stats.pnl(ledger.expected_position, 100.0).abs() < 1e-9);
    }
}
