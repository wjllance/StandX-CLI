use crate::cli::*;
use anyhow::Result;
use standx_sdk::client::StandXClient;
use standx_sdk::StandXEndpoints;

/// Handle margin commands
pub async fn handle_margin(command: MarginCommands, endpoints: &StandXEndpoints) -> Result<()> {
    let client = StandXClient::from_endpoints(endpoints)?;

    match command {
        MarginCommands::Transfer {
            symbol,
            amount,
            direction,
        } => match client.transfer_margin(&symbol, &amount, &direction).await {
            Ok(_) => {
                println!(
                    "✅ Margin transferred for {}: {} (direction: {})",
                    symbol, amount, direction
                );
            }
            Err(e) => {
                println!("⚠️  Margin transfer failed");
                println!("   Symbol: {}", symbol);
                println!("   Amount: {}", amount);
                println!("   Direction: {}", direction);
                println!("   Error: {}", e);
            }
        },
        MarginCommands::Mode { symbol, set } => {
            if let Some(mode) = set {
                // Set margin mode
                match client.change_margin_mode(&symbol, &mode).await {
                    Ok(_) => println!("✅ Margin mode for {} set to {}", symbol, mode),
                    Err(e) => {
                        println!("⚠️  Margin mode change failed");
                        println!("   Symbol: {}", symbol);
                        println!("   Mode: {}", mode);
                        println!("   Error: {}", e);
                    }
                }
            } else {
                // Get margin mode from position config
                let config = client.get_position_config(&symbol).await?;
                let mode = if config.margin_mode.is_empty() {
                    "cross"
                } else {
                    &config.margin_mode
                };
                println!(
                    "Margin mode for {}: {} (leverage: {}x)",
                    symbol, mode, config.leverage
                );
            }
        }
    }
    Ok(())
}
