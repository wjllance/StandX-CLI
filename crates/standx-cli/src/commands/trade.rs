use super::util::parse_time_string;
use crate::cli::*;
use crate::output;
use anyhow::Result;
use standx_sdk::client::StandXClient;
use standx_sdk::StandXEndpoints;

/// Handle trade commands
pub async fn handle_trade(
    command: TradeCommands,
    output_format: OutputFormat,
    endpoints: &StandXEndpoints,
) -> Result<()> {
    let client = StandXClient::from_endpoints(endpoints)?;

    match command {
        TradeCommands::History {
            symbol,
            from,
            to,
            limit,
        } => {
            // Parse time parameters with defaults
            let now = chrono::Utc::now().timestamp();
            let from_ts = match from {
                Some(f) => parse_time_string(&f, false)?,
                None => now - 86400, // Default: 1 day ago
            };
            let to_ts = match to {
                Some(t) => parse_time_string(&t, true)?,
                None => now, // Default: now
            };

            let trades = client
                .get_user_trades(&symbol, from_ts, to_ts, limit)
                .await?;

            match output_format {
                OutputFormat::Table => {
                    if trades.is_empty() {
                        println!(
                            "ℹ️  No trades found for {} in the specified time range",
                            symbol
                        );
                    } else {
                        println!("{}", output::format_table(trades));
                    }
                }
                OutputFormat::Json => println!("{}", output::format_json(&trades)?),
                OutputFormat::Csv => println!("{}", output::format_csv(&trades)?),
                OutputFormat::Quiet => {}
            }
        }
    }
    Ok(())
}
