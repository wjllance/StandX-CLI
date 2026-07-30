use crate::cli::*;
use anyhow::Result;
use standx_sdk::client::StandXClient;
use standx_sdk::models::BlockTrade;
use standx_sdk::StandXEndpoints;
use std::time::Duration;

/// Handle block trade commands
pub async fn handle_block(
    command: BlockCommands,
    output_format: OutputFormat,
    endpoints: &StandXEndpoints,
) -> Result<()> {
    let client = StandXClient::from_endpoints(endpoints)?;

    match command {
        BlockCommands::List {
            symbol,
            limit,
            status,
        } => {
            let is_completed = match status.as_str() {
                "completed" => Some(true),
                "pending" => Some(false),
                _ => None,
            };

            let trades = client
                .get_block_trades(symbol.as_deref(), limit, is_completed)
                .await?;

            if trades.is_empty() {
                println!("No block trades found");
                return Ok(());
            }

            match output_format {
                OutputFormat::Json => {
                    println!("{}", serde_json::to_string_pretty(&trades)?);
                }
                _ => {
                    println!("{}", format_block_trades_table(&trades));
                }
            }
        }
        BlockCommands::Watch { symbol, interval } => {
            println!(
                "Watching block trades{} (Ctrl+C to exit)\n",
                symbol
                    .as_ref()
                    .map(|s| format!(" for {}", s))
                    .unwrap_or_default()
            );

            let mut last_id: Option<i64> = None;
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(interval)) => {
                        match client.get_block_trades(symbol.as_deref(), 20, None).await {
                            Ok(trades) => {
                                for trade in &trades {
                                    if last_id.map(|id| trade.id > id).unwrap_or(true) {
                                        print_block_trade_line(trade);
                                        last_id = Some(trade.id).max(last_id);
                                    }
                                }
                            }
                            Err(e) => {
                                eprintln!("Error fetching block trades: {}", e);
                            }
                        }
                    }
                    _ = tokio::signal::ctrl_c() => {
                        println!("\nExiting block trade watch");
                        break;
                    }
                }
            }
        }
    }

    Ok(())
}

fn format_block_trades_table(trades: &[BlockTrade]) -> String {
    if trades.is_empty() {
        return "No block trades".to_string();
    }

    let header = format!(
        "{:>6} {:<12} {:>6} {:>14} {:>10} {:<12} {}",
        "ID", "SYMBOL", "SIDE", "PRICE", "QTY", "STATUS", "TIME"
    );
    let separator = "-".repeat(header.len());

    let rows: Vec<String> = trades
        .iter()
        .map(|t| {
            let time = chrono::DateTime::from_timestamp(t.expire_time, 0)
                .map(|dt| dt.format("%H:%M:%S").to_string())
                .unwrap_or_else(|| t.expire_time.to_string());
            format!(
                "{:>6} {:<12} {:>6} {:>14} {:>10} {:<12} {}",
                t.id,
                t.symbol,
                t.side.to_uppercase(),
                t.price,
                t.qty,
                t.block_status,
                time
            )
        })
        .collect();

    format!("{}\n{}\n{}", header, separator, rows.join("\n"))
}

fn print_block_trade_line(trade: &BlockTrade) {
    let time = chrono::DateTime::from_timestamp(trade.expire_time, 0)
        .map(|dt| dt.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| trade.expire_time.to_string());

    println!(
        "🟩 Block | {} | {} {} @ {} | Qty: {} | {}",
        trade.id,
        trade.side.to_uppercase(),
        trade.symbol,
        trade.price,
        trade.qty,
        time
    );
}
