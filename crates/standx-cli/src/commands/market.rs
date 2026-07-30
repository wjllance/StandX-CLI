use super::util::parse_time_string;
use crate::cli::*;
use crate::output;
use anyhow::Result;
use standx_sdk::client::StandXClient;
use standx_sdk::StandXEndpoints;

/// Handle market commands
pub async fn handle_market(
    command: MarketCommands,
    output_format: OutputFormat,
    endpoints: &StandXEndpoints,
) -> Result<()> {
    let client = StandXClient::from_endpoints(endpoints)?;

    match command {
        MarketCommands::Symbols => {
            let symbols = client.get_symbol_info().await?;

            match output_format {
                OutputFormat::Table => println!("{}", output::format_table(symbols)),
                OutputFormat::Json => println!("{}", output::format_json(&symbols)?),
                OutputFormat::Csv => println!("{}", output::format_csv(&symbols)?),
                OutputFormat::Quiet => {}
            }
        }
        MarketCommands::Ticker { symbol } => {
            let ticker = client.get_symbol_market(&symbol).await?;

            match output_format {
                OutputFormat::Table => println!("{}", output::format_item(ticker)),
                OutputFormat::Json => println!("{}", output::format_json(&ticker)?),
                OutputFormat::Csv => println!("{}", output::format_csv(&[ticker])?),
                OutputFormat::Quiet => println!("{}", ticker.last_price),
            }
        }
        MarketCommands::Tickers => {
            let symbols = client.get_symbol_info().await?;
            let mut tickers = vec![];

            for symbol_info in symbols {
                match client.get_symbol_market(&symbol_info.symbol).await {
                    Ok(ticker) => tickers.push(ticker),
                    Err(e) => eprintln!("Warning: Failed to get {}: {}", symbol_info.symbol, e),
                }
            }

            match output_format {
                OutputFormat::Table => println!("{}", output::format_table(tickers)),
                OutputFormat::Json => println!("{}", output::format_json(&tickers)?),
                OutputFormat::Csv => println!("{}", output::format_csv(&tickers)?),
                OutputFormat::Quiet => {}
            }
        }
        MarketCommands::Trades { symbol, limit } => {
            let trades = client.get_recent_trades(&symbol, limit).await?;

            match output_format {
                OutputFormat::Table => println!("{}", output::format_table(trades)),
                OutputFormat::Json => println!("{}", output::format_json(&trades)?),
                OutputFormat::Csv => println!("{}", output::format_csv(&trades)?),
                OutputFormat::Quiet => {}
            }
        }
        MarketCommands::Depth { symbol, limit } => {
            let book = client.get_depth(&symbol, limit).await?;

            match output_format {
                OutputFormat::Table => println!(
                    "{}",
                    output::format_order_book(&book, limit.unwrap_or(10) as usize)
                ),
                OutputFormat::Json => println!("{}", output::format_json(&book)?),
                OutputFormat::Csv => println!("CSV format not supported for order book"),
                OutputFormat::Quiet => {
                    if let (Some(bid), Some(ask)) = (book.best_bid(), book.best_ask()) {
                        println!("{} {}", bid, ask);
                    }
                }
            }
        }
        MarketCommands::Kline {
            symbol,
            resolution,
            from,
            to,
            limit,
        } => {
            // Parse time parameters
            let now = chrono::Utc::now().timestamp();
            let from_ts = match from {
                Some(f) => parse_time_string(&f, false)?,
                None => now - 86400, // Default: 1 day ago
            };
            let to_ts = match to {
                Some(t) => parse_time_string(&t, false)?, // Fix: use false to get past time
                None => now,                              // Default: now
            };

            let klines = client
                .get_kline(&symbol, &resolution, from_ts, to_ts)
                .await?;

            // Apply limit if specified
            let klines: Vec<_> = if let Some(lim) = limit {
                klines.into_iter().take(lim as usize).collect()
            } else {
                klines
            };

            match output_format {
                OutputFormat::Table => {
                    println!("Kline data for {} ({}):", symbol, resolution);
                    for kline in klines {
                        // Format timestamp to readable time
                        let time_str = match kline.time.parse::<i64>() {
                            Ok(ts) => chrono::DateTime::from_timestamp(ts, 0)
                                .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
                                .unwrap_or_else(|| kline.time.clone()),
                            Err(_) => kline.time.clone(),
                        };
                        println!(
                            "  {}: O:{} H:{} L:{} C:{} V:{}",
                            time_str, kline.open, kline.high, kline.low, kline.close, kline.volume
                        );
                    }
                }
                OutputFormat::Json => println!("{}", output::format_json(&klines)?),
                OutputFormat::Csv => println!("{}", output::format_csv(&klines)?),
                OutputFormat::Quiet => {}
            }
        }
        MarketCommands::Funding { symbol, days } => {
            let now = chrono::Utc::now().timestamp();
            let start_time = now - days * 24 * 60 * 60;
            let funding_rates = client.get_funding_rate(&symbol, start_time, now).await?;

            if funding_rates.is_empty() {
                println!(
                    "ℹ️  No funding rate data available for {} in the last {} days",
                    symbol, days
                );
                println!("   This may be because:");
                println!("   - The symbol is not actively trading");
                println!("   - Funding rates are only recorded at specific intervals");
                println!(
                    "   - Try checking the current funding rate with: standx market ticker {}",
                    symbol
                );
            } else {
                match output_format {
                    OutputFormat::Table => println!("{}", output::format_table(funding_rates)),
                    OutputFormat::Json => println!("{}", output::format_json(&funding_rates)?),
                    OutputFormat::Csv => println!("{}", output::format_csv(&funding_rates)?),
                    OutputFormat::Quiet => {}
                }
            }
        }
    }
    Ok(())
}
