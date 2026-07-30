use crate::cli::*;
use crate::output;
use anyhow::Result;
use standx_sdk::client::StandXClient;
use standx_sdk::StandXEndpoints;

/// Handle account commands
pub async fn handle_account(
    command: AccountCommands,
    output_format: OutputFormat,
    endpoints: &StandXEndpoints,
) -> Result<()> {
    let client = StandXClient::from_endpoints(endpoints)?;

    match command {
        AccountCommands::Balances => {
            let balance = client.get_balance().await?;

            match output_format {
                OutputFormat::Table => println!("{}", output::format_item(balance)),
                OutputFormat::Json => println!("{}", output::format_json(&balance)?),
                OutputFormat::Csv => println!("CSV format not supported for single item"),
                OutputFormat::Quiet => {}
            }
        }
        AccountCommands::Positions { symbol } => {
            let mut positions = client.get_positions(symbol.as_deref()).await?;

            // Only hide a quantity that is provably finite and exactly zero.
            // Signed short quantities remain visible, while malformed/non-finite
            // values fail closed as a non-empty result for operational checks.
            positions.retain(|position| position_quantity_is_nonzero_or_invalid(&position.qty));

            match output_format {
                OutputFormat::Table => println!("{}", output::format_table(positions)),
                OutputFormat::Json => println!("{}", output::format_json(&positions)?),
                OutputFormat::Csv => println!("{}", output::format_csv(&positions)?),
                OutputFormat::Quiet => {}
            }
        }
        AccountCommands::Orders { symbol } => {
            let orders = client.get_open_orders(symbol.as_deref()).await?;

            match output_format {
                OutputFormat::Table => println!("{}", output::format_table(orders)),
                OutputFormat::Json => println!("{}", output::format_json(&orders)?),
                OutputFormat::Csv => println!("{}", output::format_csv(&orders)?),
                OutputFormat::Quiet => {}
            }
        }
        AccountCommands::History { symbol, limit } => {
            let orders = client
                .get_order_history(symbol.as_deref(), Some(limit))
                .await?;

            match output_format {
                OutputFormat::Table => println!("{}", output::format_table(orders)),
                OutputFormat::Json => println!("{}", output::format_json(&orders)?),
                OutputFormat::Csv => println!("{}", output::format_csv(&orders)?),
                OutputFormat::Quiet => {}
            }
        }
        AccountCommands::Config { symbol } => {
            println!("Position config for {} not yet implemented", symbol);
        }
    }
    Ok(())
}

fn position_quantity_is_nonzero_or_invalid(value: &str) -> bool {
    match value.parse::<f64>() {
        Ok(quantity) if quantity.is_finite() => quantity != 0.0,
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::position_quantity_is_nonzero_or_invalid;

    #[test]
    fn position_filter_only_hides_proven_finite_zero() {
        assert!(!position_quantity_is_nonzero_or_invalid("0"));
        assert!(!position_quantity_is_nonzero_or_invalid("-0.000"));
        assert!(position_quantity_is_nonzero_or_invalid("0.001"));
        assert!(position_quantity_is_nonzero_or_invalid("-0.001"));
        assert!(position_quantity_is_nonzero_or_invalid("NaN"));
        assert!(position_quantity_is_nonzero_or_invalid("not-a-number"));
    }
}
