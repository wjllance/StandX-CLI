use crate::cli::*;
use crate::config::Config;
use anyhow::Result;

/// Handle config commands
pub async fn handle_config(
    command: ConfigCommands,
    output_format: OutputFormat,
    config_path: Option<&str>,
) -> Result<()> {
    match command {
        ConfigCommands::Init => {
            let config = Config::load_selected(config_path)?;
            config.save()?;
            match output_format {
                OutputFormat::Json => {
                    let json = serde_json::json!({
                        "status": "success",
                        "message": "Configuration initialized",
                        "config_file": config.config_file()
                    });
                    println!("{}", serde_json::to_string_pretty(&json)?);
                }
                OutputFormat::Quiet => {}
                _ => println!("✅ Configuration initialized at {:?}", config.config_file()),
            }
        }
        ConfigCommands::Set { key, value } => {
            let mut config = Config::load_selected(config_path)?;
            config.set(&key, &value)?;
            match output_format {
                OutputFormat::Json => {
                    let json = serde_json::json!({
                        "status": "success",
                        "key": key,
                        "value": value
                    });
                    println!("{}", serde_json::to_string_pretty(&json)?);
                }
                OutputFormat::Quiet => {}
                _ => println!("✅ Set {} = {}", key, value),
            }
        }
        ConfigCommands::Get { key } => {
            let config = Config::load_selected(config_path)?;
            let value = config.get(&key)?;
            match output_format {
                OutputFormat::Json => {
                    let json = serde_json::json!({
                        "key": key,
                        "value": value
                    });
                    println!("{}", serde_json::to_string_pretty(&json)?);
                }
                OutputFormat::Quiet => println!("{}", value),
                _ => println!("{}: {}", key, value),
            }
        }
        ConfigCommands::Show => {
            let config = Config::load_selected(config_path)?;
            match output_format {
                OutputFormat::Json => {
                    let json = serde_json::json!({
                        "base_url": config.base_url,
                        "output_format": config.output_format,
                        "default_symbol": config.default_symbol
                    });
                    println!("{}", serde_json::to_string_pretty(&json)?);
                }
                OutputFormat::Quiet => {}
                _ => {
                    println!("Configuration:");
                    println!("  base_url: {}", config.base_url);
                    println!("  output_format: {}", config.output_format);
                    println!("  default_symbol: {}", config.default_symbol);
                }
            }
        }
    }
    Ok(())
}
