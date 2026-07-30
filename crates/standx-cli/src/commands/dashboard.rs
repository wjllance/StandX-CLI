use super::util::{is_auth_error, run_watch_loop};
use crate::cli::*;
use crate::output;
use anyhow::Result;
use futures_util::future::join_all;
use standx_sdk::client::StandXClient;
use standx_sdk::models::{DashboardSnapshot, Trade};
use standx_sdk::websocket::{StandXWebSocket, WsMessage};
use standx_sdk::StandXEndpoints;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::{watch, RwLock};

/// Handle dashboard commands - unified view of account, positions, orders, and market data
pub async fn handle_dashboard(
    symbols: Option<String>,
    verbose: bool,
    watch: Option<u64>,
    compact: bool,
    output_format: OutputFormat,
    endpoints: &StandXEndpoints,
) -> Result<()> {
    // Build list of symbols to track
    let symbol_list: Vec<String> = if let Some(s) = symbols {
        s.split(',')
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
            .map(|v| v.to_string())
            .collect()
    } else {
        vec![]
    };
    let client = StandXClient::from_endpoints(endpoints)?;
    let ws_trades: Arc<RwLock<VecDeque<Trade>>> = Arc::new(RwLock::new(VecDeque::new()));
    let mut ws_trade_updates_rx: Option<watch::Receiver<u64>> = None;
    let mut ws_trades_enabled = false;

    if watch.is_some() {
        let first_symbol = if let Some(symbol) = symbol_list.first() {
            Some(symbol.clone())
        } else {
            client
                .get_symbol_info()
                .await
                .ok()
                .and_then(|symbols| symbols.into_iter().next().map(|s| s.symbol))
        };

        if let Some(first_symbol) = first_symbol {
            // Seed initial trades so first frame has data even before websocket receives updates.
            if let Ok(initial_trades) = client.get_recent_trades(&first_symbol, Some(7)).await {
                let mut buf = ws_trades.write().await;
                for trade in initial_trades {
                    buf.push_back(trade);
                }
                while buf.len() > 7 {
                    buf.pop_back();
                }
            }

            if let Ok(ws) =
                StandXWebSocket::without_auth_from_endpoints_with_verbose(endpoints, false)
            {
                if ws
                    .subscribe("public_trade", Some(&first_symbol))
                    .await
                    .is_ok()
                {
                    let (trade_updates_tx, trade_updates_rx) = watch::channel(0_u64);
                    ws_trade_updates_rx = Some(trade_updates_rx);
                    let mut update_seq: u64 = 0;
                    if let Ok(mut rx) = ws.connect().await {
                        ws_trades_enabled = true;
                        let ws_trades_clone = ws_trades.clone();
                        tokio::spawn(async move {
                            while let Some(msg) = rx.recv().await {
                                if let WsMessage::Trade(trade) = msg {
                                    let mut trades = ws_trades_clone.write().await;
                                    trades.push_front(trade);
                                    while trades.len() > 7 {
                                        trades.pop_back();
                                    }
                                    update_seq = update_seq.wrapping_add(1);
                                    let _ = trade_updates_tx.send(update_seq);
                                }
                            }
                        });
                    }
                }
            }
        }
    }

    run_watch_loop(
        watch,
        || {
            build_dashboard_output(
                &client,
                &symbol_list,
                verbose,
                output_format,
                compact,
                if ws_trades_enabled {
                    Some(ws_trades.clone())
                } else {
                    None
                },
            )
        },
        "Dashboard refresh failed",
        ws_trade_updates_rx,
    )
    .await
}

/// Build dashboard output with optional symbol filtering
async fn build_dashboard_output(
    client: &StandXClient,
    symbol_filter: &[String],
    _verbose: bool,
    output_format: OutputFormat,
    compact: bool,
    ws_trades: Option<Arc<RwLock<VecDeque<Trade>>>>,
) -> Result<String> {
    // Check if filtering by symbols
    let has_filter = !symbol_filter.is_empty();

    // Determine which symbols to track
    let symbol_list: Vec<String> = if has_filter {
        symbol_filter.to_vec()
    } else {
        // Get all available symbols from API
        client
            .get_symbol_info()
            .await?
            .into_iter()
            .map(|s| s.symbol)
            .collect()
    };

    // Fetch authenticated endpoints concurrently
    let (balance_result, positions_result, orders_result) = tokio::join!(
        client.get_balance(),
        client.get_positions(None),
        client.get_open_orders(None)
    );

    // Try to fetch authenticated data, handle auth errors gracefully
    let (account, auth_warning) = match balance_result {
        Ok(balance) => (Some(balance), None),
        Err(e) => {
            if is_auth_error(&e) {
                (
                    None,
                    Some("⚠️  Not authenticated. Run 'standx auth login' to access account data."),
                )
            } else {
                return Err(e.into());
            }
        }
    };

    let all_positions = match positions_result {
        Ok(positions) => positions,
        Err(e) if is_auth_error(&e) => Vec::new(),
        Err(e) => return Err(e.into()),
    };
    let total_realized_pnl_all_positions: f64 = all_positions
        .iter()
        .map(|p| p.realized_pnl.parse::<f64>().unwrap_or(0.0))
        .sum();
    let all_orders = match orders_result {
        Ok(orders) => orders,
        Err(e) if is_auth_error(&e) => Vec::new(),
        Err(e) => return Err(e.into()),
    };

    // Filter by symbol if specified, and filter out zero-qty positions
    let positions = if has_filter {
        all_positions
            .into_iter()
            .filter(|p| {
                p.qty.parse::<f64>().unwrap_or(0.0) > 0.0
                    && symbol_filter
                        .iter()
                        .any(|s| s.eq_ignore_ascii_case(&p.symbol))
            })
            .collect()
    } else {
        all_positions
            .into_iter()
            .filter(|p| p.qty.parse::<f64>().unwrap_or(0.0) > 0.0)
            .collect()
    };

    let orders = if has_filter {
        all_orders
            .into_iter()
            .filter(|o| {
                symbol_filter
                    .iter()
                    .any(|s| s.eq_ignore_ascii_case(&o.symbol))
            })
            .collect()
    } else {
        all_orders
    };

    // Fetch market + kline data for tracked symbols in parallel.
    // Kline open is used as a fallback to compute 24h change when ticker field is missing.
    let now_ts = chrono::Utc::now().timestamp();
    let from_ts = now_ts - 86400;
    let (market_results, kline_results) = tokio::join!(
        join_all(
            symbol_list
                .iter()
                .map(|symbol| client.get_symbol_market(symbol))
        ),
        join_all(
            symbol_list
                .iter()
                .map(|symbol| client.get_kline(symbol, "1D", from_ts, now_ts))
        )
    );

    let mut open_prices: HashMap<String, f64> = HashMap::new();
    for (index, result) in kline_results.into_iter().enumerate() {
        if let Ok(klines) = result {
            if let Some(kline) = klines.first() {
                if let Ok(open) = kline.open.parse::<f64>() {
                    if open > 0.0 {
                        open_prices.insert(symbol_list[index].clone(), open);
                    }
                }
            }
        }
    }

    let mut market: Vec<_> = market_results
        .into_iter()
        .filter_map(std::result::Result::ok)
        .collect();

    for ticker in &mut market {
        if ticker.change_24h_percent.is_empty() || ticker.change_24h_percent == "0" {
            if let Some(open) = open_prices.get(&ticker.symbol) {
                if let Ok(last) = ticker.last_price.parse::<f64>() {
                    let change = ((last - open) / open) * 100.0;
                    ticker.change_24h_percent = format!("{:.2}", change);
                }
            }
        }
    }

    // Fetch recent trades + order book for first symbol.
    // In watch mode we prefer websocket-fed trades buffer to avoid polling for trades.
    let (trades, order_book) = if let Some(first_symbol) = symbol_list.first() {
        let trades = if let Some(ws_buf) = ws_trades {
            let buf = ws_buf.read().await;
            buf.iter().cloned().collect()
        } else {
            client
                .get_recent_trades(first_symbol, Some(7))
                .await
                .unwrap_or_default()
        };

        let order_book = client.get_depth(first_symbol, Some(5)).await.ok();
        (trades, order_book)
    } else {
        (Vec::new(), None)
    };

    // Create dashboard snapshot
    let snapshot = DashboardSnapshot {
        timestamp: chrono::Utc::now().to_rfc3339(),
        account,
        positions,
        total_realized_pnl: total_realized_pnl_all_positions.to_string(),
        orders,
        market,
        trades,
        order_book,
    };

    let rendered = match output_format {
        OutputFormat::Table => {
            // Use MVP format (Issue #156)
            let mut text = String::new();
            if let Some(warning) = auth_warning {
                text.push_str(warning);
                text.push_str("\n\n");
            }
            text.push_str(&output::format_dashboard_mvp(&snapshot, compact));
            text
        }
        OutputFormat::Json => format!("{}\n", output::format_json(&snapshot)?),
        OutputFormat::Csv => {
            // For CSV, output positions as they're the most important
            if !snapshot.positions.is_empty() {
                format!("{}\n", output::format_csv(&snapshot.positions)?)
            } else {
                "No positions to display\n".to_string()
            }
        }
        OutputFormat::Quiet => String::new(),
    };

    Ok(rendered)
}
