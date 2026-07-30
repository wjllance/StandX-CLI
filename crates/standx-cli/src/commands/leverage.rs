use crate::cli::*;
use crate::output;
use anyhow::Result;
use standx_sdk::client::StandXClient;
use standx_sdk::StandXEndpoints;

/// Handle leverage commands
pub async fn handle_leverage(
    command: LeverageCommands,
    output_format: OutputFormat,
    endpoints: &StandXEndpoints,
) -> Result<()> {
    let client = StandXClient::from_endpoints(endpoints)?;

    match command {
        LeverageCommands::Get { symbol } => {
            // Try to get position config first, fallback to symbol info
            match client.get_position_config(&symbol).await {
                Ok(config) => match output_format {
                    OutputFormat::Table => println!("{}", output::format_item(config)),
                    OutputFormat::Json => println!("{}", output::format_json(&config)?),
                    OutputFormat::Csv => println!("{}", output::format_csv(&[config])?),
                    OutputFormat::Quiet => println!("{}", config.leverage),
                },
                Err(_) => {
                    // Fallback: get leverage from symbol info
                    let symbol_info = client
                        .get_symbol_info()
                        .await?
                        .into_iter()
                        .find(|s| s.symbol == symbol)
                        .ok_or_else(|| anyhow::anyhow!("Symbol {} not found", symbol))?;

                    let config = standx_sdk::models::PositionConfig {
                        symbol: symbol.clone(),
                        leverage: symbol_info.def_leverage.clone(),
                        max_leverage: symbol_info.max_leverage.clone(),
                        def_leverage: symbol_info.def_leverage,
                        margin_mode: "cross".to_string(),
                    };

                    match output_format {
                        OutputFormat::Table => println!("{}", output::format_item(config)),
                        OutputFormat::Json => println!("{}", output::format_json(&config)?),
                        OutputFormat::Csv => println!("{}", output::format_csv(&[config])?),
                        OutputFormat::Quiet => println!("{}", config.leverage),
                    }
                }
            }
        }
        LeverageCommands::Set { symbol, leverage } => {
            let leverage_val: u32 = leverage
                .parse()
                .map_err(|_| anyhow::anyhow!("Invalid leverage value: {}", leverage))?;

            match client.change_leverage(&symbol, leverage_val).await {
                Ok(_) => println!("✅ Leverage for {} set to {}x", symbol, leverage),
                Err(e) => {
                    println!("⚠️  Leverage change failed");
                    println!("   Symbol: {}", symbol);
                    println!("   Requested leverage: {}x", leverage);
                    println!("   Error: {}", e);
                }
            }
        }
    }
    Ok(())
}
