use super::util::{is_auth_error, run_watch_loop};
use crate::cli::*;
use crate::output;
use anyhow::Result;
use standx_sdk::client::StandXClient;
use standx_sdk::models::PortfolioSnapshot;
use standx_sdk::StandXEndpoints;

/// Portfolio command for direct execution (without subcommands)
#[derive(Debug)]
pub enum PortfolioCommand {
    Snapshot { _verbose: bool, watch: Option<u64> },
}

/// Handle portfolio commands - view portfolio summary and performance
pub async fn handle_portfolio(
    command: PortfolioCommand,
    output_format: OutputFormat,
    endpoints: &StandXEndpoints,
) -> Result<()> {
    match command {
        PortfolioCommand::Snapshot { _verbose, watch } => {
            let client = StandXClient::from_endpoints(endpoints)?;
            run_watch_loop(
                watch,
                || build_portfolio_output(&client, _verbose, output_format),
                "Portfolio refresh failed",
                None,
            )
            .await?;
        }
    }
    Ok(())
}

/// Build portfolio output
async fn build_portfolio_output(
    client: &StandXClient,
    verbose: bool,
    output_format: OutputFormat,
) -> Result<String> {
    // Try to fetch authenticated data, handle auth errors gracefully
    let balance_result = client.get_balance().await;
    let balance = match balance_result {
        Ok(b) => Some(b),
        Err(e) => {
            if is_auth_error(&e) {
                None
            } else {
                return Err(e.into());
            }
        }
    };

    // If not authenticated, show market data only
    let positions = if balance.is_some() {
        let positions = match client.get_positions(None).await {
            Ok(positions) => positions,
            Err(e) if is_auth_error(&e) => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        // Filter out zero-qty positions
        positions
            .into_iter()
            .filter(|p| p.qty.parse::<f64>().unwrap_or(0.0) > 0.0)
            .collect()
    } else {
        Vec::new()
    };

    // Calculate total values
    let total_value_usd = balance
        .as_ref()
        .map(|b| b.equity.clone())
        .unwrap_or_default();
    let total_pnl_24h = balance
        .as_ref()
        .map(|b| b.pnl_24h.clone())
        .unwrap_or_default();
    let total_pnl_realized = balance.as_ref().map(|b| b.upnl.clone()).unwrap_or_default();

    // Create portfolio snapshot
    let snapshot = PortfolioSnapshot {
        timestamp: chrono::Utc::now().to_rfc3339(),
        total_value_usd,
        total_pnl_24h,
        total_pnl_realized,
        positions,
    };

    let rendered = match output_format {
        OutputFormat::Table => {
            let mut text = String::new();
            if balance.is_none() {
                text.push_str(
                    "⚠️  Not authenticated. Run 'standx auth login' to access account data.\n\n",
                );
            }
            text.push_str("=== Portfolio Summary ===\n");
            text.push_str(&format!("Timestamp: {}\n\n", snapshot.timestamp));

            // Account summary
            text.push_str("--- Account ---\n");
            text.push_str(&format!("  Total Value: ${}\n", snapshot.total_value_usd));
            text.push_str(&format!("  PnL 24h: ${}\n", snapshot.total_pnl_24h));
            text.push_str(&format!(
                "  Unrealized PnL: ${}\n\n",
                snapshot.total_pnl_realized
            ));

            // Positions
            if !snapshot.positions.is_empty() {
                text.push_str(&format!(
                    "--- Positions ({}) ---\n",
                    snapshot.positions.len()
                ));
                text.push_str(&format!("{}\n", output::format_table(snapshot.positions)));
            } else {
                text.push_str("--- No open positions ---\n");
            }

            if verbose {
                text.push_str("\n--- Verbose Details ---\n");
                if let Some(b) = &balance {
                    text.push_str(&format!("  Balance: ${}\n", b.balance));
                    text.push_str(&format!("  Available: ${}\n", b.cross_available));
                    text.push_str(&format!("  Equity: ${}\n", b.equity));
                    text.push_str(&format!("  Cross Margin: ${}\n", b.cross_margin));
                    text.push_str(&format!("  Cross UPNL: ${}\n", b.cross_upnl));
                    text.push_str(&format!("  Locked: ${}\n", b.locked));
                } else {
                    text.push_str("  (Not authenticated - no balance details)\n");
                }
            }
            text
        }
        OutputFormat::Json => format!("{}\n", output::format_json(&snapshot)?),
        OutputFormat::Csv => {
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
