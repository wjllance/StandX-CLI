use crate::cli::*;
use anyhow::Result;
use standx_sdk::account_stream::{AccountChannel, AccountEvent, AccountStream};
use standx_sdk::websocket::{StandXWebSocket, WsMessage};
use standx_sdk::StandXEndpoints;

/// Handle stream commands
pub async fn handle_stream(
    command: StreamCommands,
    verbose: bool,
    endpoints: &StandXEndpoints,
) -> Result<()> {
    match command {
        // Public channels - no auth required
        StreamCommands::Price { symbol } => {
            let ws = StandXWebSocket::without_auth_from_endpoints_with_verbose(endpoints, verbose)?;
            let _ = ws.subscribe("price", Some(&symbol)).await;
            let mut rx = ws.connect().await?;

            println!("Streaming price for {}", symbol);
            println!("Press Ctrl+C to exit\n");

            while let Some(msg) = rx.recv().await {
                if let WsMessage::Price(data) = msg {
                    println!(
                        "{} | Mark: {} | Index: {} | Last: {}",
                        data.data.timestamp,
                        data.data.mark_price,
                        data.data.index_price,
                        data.data.last_price
                    );
                }
            }
        }
        StreamCommands::Depth { symbol, levels } => {
            let ws = StandXWebSocket::without_auth_from_endpoints_with_verbose(endpoints, verbose)?;
            let _ = ws.subscribe("depth_book", Some(&symbol)).await;
            let mut rx = ws.connect().await?;

            println!("Streaming depth for {} (top {} levels)", symbol, levels);
            println!("Press Ctrl+C to exit\n");

            while let Some(msg) = rx.recv().await {
                if let WsMessage::Depth(data) = msg {
                    println!("\n=== Order Book: {} ===", data.data.symbol);
                    println!("Asks:");
                    for ask in data.data.asks.iter().take(levels) {
                        println!("  {}: {}", ask[0], ask[1]);
                    }
                    println!("Bids:");
                    for bid in data.data.bids.iter().take(levels) {
                        println!("  {}: {}", bid[0], bid[1]);
                    }
                }
            }
        }
        StreamCommands::Trade { symbol } => {
            let ws = StandXWebSocket::without_auth_from_endpoints_with_verbose(endpoints, verbose)?;
            let _ = ws.subscribe("public_trade", Some(&symbol)).await;
            let mut rx = ws.connect().await?;

            println!("Streaming trades for {}", symbol);
            println!("Press Ctrl+C to exit\n");

            while let Some(msg) = rx.recv().await {
                if let WsMessage::Trade(data) = msg {
                    let side = data.side.as_deref().unwrap_or({
                        if data.is_buyer_taker {
                            "buy"
                        } else {
                            "sell"
                        }
                    });
                    let side_emoji = match side.to_lowercase().as_str() {
                        "buy" => "🟢 BUY",
                        "sell" => "🔴 SELL",
                        _ => side,
                    };
                    println!(
                        "{} | {} | Price: {} | Qty: {}",
                        data.time, side_emoji, data.price, data.qty
                    );
                }
            }
        }
        StreamCommands::Kline { symbol, interval } => {
            let ws = StandXWebSocket::without_auth_from_endpoints_with_verbose(endpoints, verbose)?;
            // Subscribe with interval parameter embedded in topic
            ws.subscribe_with_interval("kline", Some(&symbol), Some(&interval))
                .await?;
            let mut rx = ws.connect().await?;

            println!("Streaming kline for {} [{}]", symbol, interval);
            println!("Press Ctrl+C to exit\n");

            while let Some(msg) = rx.recv().await {
                if let WsMessage::Kline(data) = msg {
                    // Convert timestamp to readable time
                    let time_str = chrono::DateTime::from_timestamp_millis(data.time)
                        .map(|dt| dt.format("%H:%M:%S").to_string())
                        .unwrap_or_else(|| data.time.to_string());

                    println!(
                        "📊 Kline: {} [{}] {}\nO: {}  H: {}  L: {}  C: {}  Vol: {:.3}",
                        data.symbol.unwrap_or_default(),
                        data.interval.unwrap_or_default(),
                        time_str,
                        data.open,
                        data.high,
                        data.low,
                        data.close,
                        data.volume
                    );
                }
            }
        }
        // User-level authenticated channels
        StreamCommands::Order => {
            let stream = AccountStream::from_endpoints(1, endpoints)?;
            let (mut rx, _health, _handle) = stream.connect(&[AccountChannel::Order]).await?;

            println!("Streaming order updates");
            println!("Press Ctrl+C to exit\n");

            while let Some(msg) = rx.recv().await {
                if let AccountEvent::Order(data) = msg {
                    println!("Order update: {}", serde_json::to_string(&data)?);
                }
            }
        }
        StreamCommands::Position => {
            let stream = AccountStream::from_endpoints(1, endpoints)?;
            let (mut rx, _health, _handle) = stream.connect(&[AccountChannel::Position]).await?;

            println!("Streaming position updates");
            println!("Press Ctrl+C to exit\n");

            while let Some(msg) = rx.recv().await {
                if let AccountEvent::Position(data) = msg {
                    println!("Position update: {}", serde_json::to_string(&data)?);
                }
            }
        }
        StreamCommands::Balance => {
            let stream = AccountStream::from_endpoints(1, endpoints)?;
            let (mut rx, _health, _handle) = stream.connect(&[AccountChannel::Balance]).await?;

            println!("Streaming balance updates");
            println!("Press Ctrl+C to exit\n");

            while let Some(msg) = rx.recv().await {
                if let AccountEvent::Balance(data) = msg {
                    println!("Balance update: {}", serde_json::to_string(&data)?);
                }
            }
        }
        StreamCommands::Fills => {
            let stream = AccountStream::from_endpoints(1, endpoints)?;
            let (mut rx, _health, _handle) = stream.connect(&[AccountChannel::Trade]).await?;

            println!("Streaming fill/trade updates");
            println!("Press Ctrl+C to exit\n");

            while let Some(msg) = rx.recv().await {
                if let AccountEvent::Trade(data) = msg {
                    println!(
                        "Fill {} | {} | Price: {} | Qty: {}",
                        data.trade_id,
                        format!("{:?}", data.side).to_uppercase(),
                        data.price,
                        data.qty
                    );
                }
            }
        }
    }

    Ok(())
}
