use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

const SUPERVISOR_WEBHOOK_ENV: &str = "STANDX_SUPERVISOR_WEBHOOK";
const SUPERVISOR_WEBHOOK_FORMAT_ENV: &str = "STANDX_SUPERVISOR_WEBHOOK_FORMAT";

#[derive(Debug, Default, PartialEq, Eq)]
struct MakerLocalEnv {
    webhook: Option<String>,
    format: Option<String>,
}

fn parse_maker_local_env(contents: &str) -> MakerLocalEnv {
    let mut parsed = MakerLocalEnv::default();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, raw_value)) = line.split_once('=') else {
            continue;
        };
        let value = raw_value.trim();
        let value = if value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            &value[1..value.len() - 1]
        } else {
            value
        };
        if value.is_empty() {
            continue;
        }
        match key.trim() {
            SUPERVISOR_WEBHOOK_ENV => parsed.webhook = Some(value.to_string()),
            SUPERVISOR_WEBHOOK_FORMAT_ENV => parsed.format = Some(value.to_string()),
            _ => {}
        }
    }
    parsed
}

/// Load the maker notification settings from a local ignored env file.
/// Existing process environment values remain authoritative.
pub fn load_maker_local_env(path: &Path) -> std::io::Result<()> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let parsed = parse_maker_local_env(&contents);
    if std::env::var_os(SUPERVISOR_WEBHOOK_ENV).is_none() {
        if let Some(webhook) = parsed.webhook {
            std::env::set_var(SUPERVISOR_WEBHOOK_ENV, webhook);
        }
    }
    if std::env::var_os(SUPERVISOR_WEBHOOK_FORMAT_ENV).is_none() {
        if let Some(format) = parsed.format {
            std::env::set_var(SUPERVISOR_WEBHOOK_FORMAT_ENV, format);
        }
    }
    Ok(())
}

/// Only live maker commands need local notification secrets at process start.
pub fn should_load_maker_local_env(args: &[String]) -> bool {
    args.iter().any(|argument| argument == "ws-command-canary")
        || (args.iter().any(|argument| argument == "maker")
            && args.iter().any(|argument| argument == "run")
            && args.iter().any(|argument| argument == "--live"))
}

#[derive(Parser, Debug)]
#[command(name = "standx")]
#[command(about = "OpenClaw-first AI Agent trading toolkit")]
#[command(version = env!("CARGO_PKG_VERSION"))]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    /// Configuration file path
    #[arg(short, long, global = true)]
    pub config: Option<String>,

    /// Output format
    #[arg(short, long, global = true, value_enum, default_value = "table")]
    pub output: OutputFormat,

    /// Verbose output
    #[arg(short, long, global = true)]
    pub verbose: bool,

    /// Quiet mode
    #[arg(short, long, global = true)]
    pub quiet: bool,

    /// OpenClaw mode - optimized for AI Agent execution
    #[arg(long, global = true, env = "STANDX_OPENCLAW_MODE")]
    pub openclaw: bool,

    /// Dry run - show what would be executed without executing
    #[arg(long, global = true)]
    pub dry_run: bool,

    /// Auto-confirm dangerous operations (skip prompts)
    #[arg(long, global = true, env = "STANDX_AUTO_CONFIRM")]
    pub yes: bool,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Configuration management
    Config {
        #[command(subcommand)]
        command: ConfigCommands,
    },
    /// Authentication management
    Auth {
        #[command(subcommand)]
        command: AuthCommands,
    },
    /// Market data (public)
    #[command(visible_alias = "m")]
    Market {
        #[command(subcommand)]
        command: MarketCommands,
    },
    /// Account information (authenticated)
    #[command(visible_alias = "a")]
    Account {
        #[command(subcommand)]
        command: AccountCommands,
    },
    /// Order management (authenticated)
    #[command(visible_alias = "o")]
    Order {
        #[command(subcommand)]
        command: OrderCommands,
    },
    /// Trade history (authenticated)
    #[command(visible_alias = "t")]
    Trade {
        #[command(subcommand)]
        command: TradeCommands,
    },
    /// Leverage management (authenticated)
    Leverage {
        #[command(subcommand)]
        command: LeverageCommands,
    },
    /// Margin management (authenticated)
    Margin {
        #[command(subcommand)]
        command: MarginCommands,
    },
    /// Real-time data stream
    Stream {
        #[command(subcommand)]
        command: StreamCommands,
    },
    /// Dashboard - unified view of account, positions, orders, and market data
    #[command(visible_alias = "d")]
    Dashboard {
        /// Filter by specific symbols (comma-separated)
        #[arg(short, long = "symbols", alias = "symbol")]
        symbols: Option<String>,
        /// Enable verbose output with more details
        #[arg(short, long)]
        verbose: bool,
        /// Watch mode: refresh every N seconds (default 5 when flag used without value)
        #[arg(short, long, num_args = 0..=1, default_missing_value = "5")]
        watch: Option<u64>,
        /// Compact mode: skip RECENT TRADES section
        #[arg(long)]
        compact: bool,
    },
    /// Portfolio - view portfolio summary and performance (alias for portfolio snapshot)
    #[command(visible_alias = "p")]
    Portfolio {
        /// Enable verbose output with more details
        #[arg(short, long)]
        verbose: bool,
        /// Watch mode: refresh every N seconds (default 5 when flag used without value)
        #[arg(short, long, num_args = 0..=1, default_missing_value = "5")]
        watch: Option<u64>,
    },
    /// Block trade operations (authenticated)
    #[command(visible_alias = "b")]
    Block {
        #[command(subcommand)]
        command: BlockCommands,
    },
    /// Market-maker bot (SIP-5A community maker yield)
    #[command(visible_alias = "mk")]
    Maker {
        #[command(subcommand)]
        command: Box<MakerCommands>,
    },
    /// Update this binary to the latest GitHub release
    ///
    /// Downloads the release asset for this platform, verifies its published
    /// sha256, and atomically replaces the running executable. Refuses to touch
    /// a Homebrew-managed install (use `brew upgrade standx-cli`) and never
    /// elevates privileges.
    ///
    /// Confirmation is skipped by the global `--yes` (or
    /// `STANDX_AUTO_CONFIRM=true` — that global flag accepts only `true`/`false`,
    /// not `1`). There is deliberately no per-command copy of that flag.
    #[command(visible_alias = "self-update")]
    Update {
        /// Only report current vs latest; change nothing
        #[arg(long)]
        check: bool,
        /// Consider pre-release versions as update candidates
        #[arg(long)]
        pre: bool,
        /// Reinstall even when the latest version is already installed
        #[arg(long)]
        force: bool,
    },
    /// Record StandX vs Hyperliquid mark prices to NDJSON to measure StandX's
    /// price lag (read-only diagnostic; no auth, no orders).
    LagRecorder {
        /// StandX symbol to record (e.g. HYPE-USD).
        #[arg(long, default_value = "HYPE-USD")]
        symbol: String,
        /// Hyperliquid coin symbol. Defaults to the StandX symbol with its
        /// `-USD`/`-USDT` quote suffix stripped (HYPE-USD -> HYPE).
        #[arg(long)]
        hl_coin: Option<String>,
        /// NDJSON output file (created if absent, appended otherwise).
        #[arg(long)]
        out: String,
        /// Seconds between output-file flushes.
        #[arg(long, default_value_t = 5)]
        flush_secs: u64,
        /// Seconds between stderr status heartbeats.
        #[arg(long, default_value_t = 30)]
        status_secs: u64,
    },
}

#[derive(Subcommand, Debug)]
pub enum ConfigCommands {
    /// Initialize configuration
    Init,
    /// Set configuration value
    Set { key: String, value: String },
    /// Get configuration value
    Get { key: String },
    /// Show full configuration
    Show,
}

#[derive(Subcommand, Debug)]
pub enum AuthCommands {
    /// Login with JWT token (private key optional for read-only access)
    Login {
        /// JWT token (required)
        #[arg(short, long)]
        token: Option<String>,
        /// Token file path
        #[arg(short = 'f', long)]
        token_file: Option<String>,
        /// Private key (Base58) - optional, required for trading
        #[arg(short, long)]
        private_key: Option<String>,
        /// Private key file path
        #[arg(short = 'k', long)]
        key_file: Option<String>,
        /// Interactive mode
        #[arg(short, long)]
        interactive: bool,
    },
    /// Logout and clear credentials
    Logout,
    /// Show authentication status
    Status,
}

#[derive(Subcommand, Debug)]
pub enum MarketCommands {
    /// List all trading symbols
    Symbols,
    /// Get ticker for a symbol (includes funding rate)
    #[command(visible_alias = "t")]
    Ticker { symbol: String },
    /// Get all tickers
    #[command(visible_alias = "ts")]
    Tickers,
    /// Get recent trades
    #[command(visible_alias = "tr")]
    Trades {
        symbol: String,
        #[arg(short, long)]
        limit: Option<u32>,
    },
    /// Get order book depth
    #[command(visible_alias = "dep")]
    Depth {
        symbol: String,
        #[arg(short, long)]
        limit: Option<u32>,
    },
    /// Get kline data
    #[command(visible_alias = "k")]
    Kline {
        symbol: String,
        #[arg(
            short,
            long,
            help = "Resolution: 1, 5, 15, 30, 60, 240, 720, 1D, 1W, 1M"
        )]
        resolution: String,
        /// Start time (timestamp, ISO date YYYY-MM-DD, or relative like 1h, 1d, 7d)
        #[arg(short, long)]
        from: Option<String>,
        /// End time (timestamp, ISO date YYYY-MM-DD, or relative like 1h, 1d, 7d, defaults to now)
        #[arg(short, long)]
        to: Option<String>,
        /// Number of candles to fetch (alternative to from/to)
        #[arg(short, long, help = "Number of candles (alternative to from/to)")]
        limit: Option<u32>,
    },
    /// Get funding rate history
    #[command(visible_alias = "f")]
    Funding {
        symbol: String,
        #[arg(short, long, default_value = "7")]
        days: i64,
    },
}

#[derive(Subcommand, Debug)]
pub enum AccountCommands {
    /// Get account balances
    #[command(visible_alias = "b")]
    Balances,
    /// Get positions
    #[command(visible_alias = "p")]
    Positions {
        #[arg(short, long)]
        symbol: Option<String>,
    },
    /// Get open orders
    #[command(visible_alias = "o")]
    Orders {
        #[arg(short, long)]
        symbol: Option<String>,
    },
    /// Get order history
    #[command(visible_alias = "h")]
    History {
        #[arg(short, long)]
        symbol: Option<String>,
        #[arg(short, long, default_value = "50")]
        limit: u32,
    },
    /// Get position config
    #[command(visible_alias = "cfg")]
    Config { symbol: String },
}

#[derive(Subcommand, Debug)]
pub enum OrderCommands {
    /// Create a new order
    #[command(visible_alias = "c")]
    Create {
        symbol: String,
        side: String,
        order_type: String,
        // No short: `-q` is taken by the global `--quiet` flag (clap
        // debug-asserts on the collision in debug builds).
        #[arg(long)]
        qty: String,
        #[arg(short, long)]
        price: Option<String>,
        #[arg(short, long)]
        tif: Option<String>,
        #[arg(long)]
        reduce_only: bool,
        #[arg(long)]
        sl_price: Option<String>,
        #[arg(long)]
        tp_price: Option<String>,
    },
    /// Cancel an order by ID
    #[command(visible_alias = "cxl")]
    Cancel {
        symbol: String,
        #[arg(short = 'i', long)]
        order_id: String,
    },
    /// Cancel all orders for a symbol
    #[command(visible_alias = "cxa")]
    CancelAll { symbol: String },
}

#[derive(Subcommand, Debug)]
pub enum TradeCommands {
    /// Get trade history
    History {
        symbol: String,
        /// Start time (timestamp, ISO date YYYY-MM-DD, or relative like 1h, 1d, 7d)
        #[arg(short, long)]
        from: Option<String>,
        /// End time (timestamp, ISO date YYYY-MM-DD, or relative like 1h, 1d, 7d, defaults to now)
        #[arg(short, long)]
        to: Option<String>,
        /// Number of trades to fetch
        #[arg(short, long)]
        limit: Option<u32>,
    },
}

#[derive(Subcommand, Debug)]
pub enum LeverageCommands {
    /// Get leverage for a symbol
    Get { symbol: String },
    /// Set leverage for a symbol
    Set { symbol: String, leverage: String },
}

#[derive(Subcommand, Debug)]
pub enum MarginCommands {
    /// Transfer margin
    Transfer {
        symbol: String,
        amount: String,
        #[arg(short, long)]
        direction: String,
    },
    /// Get/Set margin mode
    Mode {
        symbol: String,
        #[arg(short, long)]
        set: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum StreamCommands {
    /// Stream price ticker (public channel)
    Price { symbol: String },
    /// Stream order book depth (public channel)
    Depth {
        symbol: String,
        #[arg(short, long, default_value = "10")]
        levels: usize,
    },
    /// Stream public trades (public channel)
    Trade { symbol: String },
    /// Stream candlestick/kline data (public channel)
    Kline {
        symbol: String,
        /// Interval: 3S, 1, 5, 15, 60 (minutes), 1D (day)
        #[arg(short, long, default_value = "3S")]
        interval: String,
    },
    /// Stream order updates (authenticated)
    Order,
    /// Stream position updates (authenticated)
    Position,
    /// Stream balance updates (authenticated)
    Balance,
    /// Stream fill/trade updates (authenticated)
    Fills,
}

#[derive(Subcommand, Debug)]
pub enum BlockCommands {
    /// List block trades
    List {
        /// Filter by symbol (e.g., BTC-USD)
        #[arg(short, long)]
        symbol: Option<String>,
        /// Limit number of results
        #[arg(short, long, default_value = "30")]
        limit: u32,
        /// Filter by status: completed, pending, all
        #[arg(short, long, default_value = "all")]
        status: String,
    },
    /// Watch block trades (polling mode)
    Watch {
        /// Filter by symbol
        #[arg(short, long)]
        symbol: Option<String>,
        /// Poll interval in seconds
        #[arg(short, long, default_value = "10")]
        interval: u64,
    },
}

// `Run` is intentionally a flat, user-facing strategy CLI. Keeping the
// supervised canary as a separate small variant makes the command surface
// clearer but triggers Clippy's enum-layout lint; boxing `Run` would make the
// established Clap declarations needlessly indirect.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand, Debug)]
pub enum MakerCommands {
    /// Run the maker quoting loop (paper mode by default; --live to place orders)
    Run {
        /// Symbol to quote (e.g., BTC-USD)
        symbol: String,
        /// Maker strategy TOML file. If omitted, loads maker.toml from the
        /// StandX config directory when it exists.
        #[arg(long)]
        maker_config: Option<PathBuf>,
        /// Half-spread from mark price in basis points
        #[arg(long)]
        spread_bps: Option<f64>,
        /// Eligibility band guard in bps: never quote outside mark ± band
        #[arg(long)]
        band_bps: Option<f64>,
        /// Per-side, per-level order quantity
        #[arg(long)]
        size: Option<f64>,
        /// Number of quote levels per side
        #[arg(long)]
        levels: Option<u32>,
        /// Spacing between levels in bps (when levels > 1)
        #[arg(long)]
        level_step_bps: Option<f64>,
        /// Anti-flicker: re-quote only when mark moved more than this (bps)
        /// since the order was placed
        #[arg(long)]
        refresh_bps: Option<f64>,
        /// Loop interval in seconds
        #[arg(short, long)]
        interval: Option<u64>,
        /// Max absolute position; suppress the side that would exceed it
        #[arg(long)]
        max_position: Option<f64>,
        /// Inventory skew: at full inventory, shift the quote center this many
        /// bps away from mark to favor the reducing side (0 disables). Only
        /// takes effect in live mode; paper holds no position. Suggested
        /// starting point: roughly your --spread-bps
        #[arg(long)]
        skew_bps: Option<f64>,
        /// Actively exit inventory once |position| reaches this percent of
        /// --max-position. 0 disables; requires --inventory-exit-qty.
        #[arg(long)]
        inventory_exit_pct: Option<f64>,
        /// Maximum base quantity for one reduce-only inventory exit. 0
        /// disables; requires --inventory-exit-pct.
        #[arg(long)]
        inventory_exit_qty: Option<f64>,
        /// Sanity guard: skip the cycle (no places/cancels) when mark price
        /// and book mid diverge by more than this (bps) — the data sources
        /// disagree and acting on them would be unsafe
        #[arg(long)]
        max_divergence_bps: Option<f64>,
        /// Volatility circuit breaker: halt quoting (pull all quotes) when the
        /// mark's range over --vol-window cycles reaches this many bps; resume
        /// once it falls below half that. 0 disables the breaker
        #[arg(long)]
        vol_pause_bps: Option<f64>,
        /// Number of recent cycles the volatility breaker measures range over
        #[arg(long)]
        vol_window: Option<u32>,
        /// Enable or disable the TOML-defined adaptive spread controller.
        /// `--adaptive-spread=false` provides a baseline-arm override without
        /// changing any tier geometry.
        #[arg(long, num_args = 0..=1, default_missing_value = "true", require_equals = true)]
        adaptive_spread: Option<bool>,
        /// Enable or disable the TOML-defined size skew controller.
        /// `--size-skew=false` provides a baseline-arm override without
        /// changing any size skew geometry.
        #[arg(long, num_args = 0..=1, default_missing_value = "true", require_equals = true)]
        size_skew: Option<bool>,
        /// Financial brake: when session mark-to-market PnL drops to -this
        /// (quote units), run the fail-safe shutdown (freeze, cancel the maker
        /// book, await critical webhook, exit). 0 disables
        #[arg(long)]
        stop_loss: Option<f64>,
        /// Risk alert: fire when mark-to-market PnL drops to -this (quote
        /// units). 0 disables
        #[arg(long)]
        alert_loss: Option<f64>,
        /// Risk alert: fire when |position| reaches this percent of
        /// --max-position. 0 disables
        #[arg(long)]
        alert_inventory_pct: Option<f64>,
        /// Risk notification: fire when actual position changes by this
        /// percent of --max-position since the last notification anchor.
        /// Small changes accumulate; 0 disables.
        #[arg(long)]
        alert_position_change_pct: Option<f64>,
        /// Risk alert: fire when two-sided uptime drops below this percent
        /// (after warmup). 0 disables
        #[arg(long)]
        alert_uptime: Option<f64>,
        /// Risk alert: fire when account equity drops below this (quote
        /// units). Live-only (needs an account snapshot). 0 disables
        #[arg(long)]
        alert_equity_below: Option<f64>,
        /// Risk alert: fire when available cross margin drops below this
        /// (quote units). Live-only. 0 disables
        #[arg(long)]
        alert_margin_below: Option<f64>,
        /// Hard floor (NOT an alert): stop the session when account equity
        /// drops below this (quote units). Residual position is handed off,
        /// never auto-flattened. Live-only. 0 disables (default)
        #[arg(long)]
        stop_equity_below: Option<f64>,
        /// Hard floor (NOT an alert): stop the session when available cross
        /// margin drops below this (quote units). Live-only. 0 disables
        /// (default)
        #[arg(long)]
        stop_margin_below: Option<f64>,
        /// Also POST risk alerts to this URL. stderr/JSON always get them
        /// regardless. Payload shape is set by --alert-webhook-format
        #[arg(long, env = "STANDX_SUPERVISOR_WEBHOOK")]
        alert_webhook: Option<String>,
        /// Webhook payload format for the target chat platform
        #[arg(
            long,
            value_enum,
            default_value = "slack",
            env = "STANDX_SUPERVISOR_WEBHOOK_FORMAT"
        )]
        alert_webhook_format: AlertWebhookFormat,
        /// Disable the WebSocket market feed and poll REST every cycle.
        /// `--no-ws` enables REST polling; `--no-ws=false` forces the WS feed
        /// back on even when a config file sets `no_ws = true`.
        #[arg(long, num_args = 0..=1, default_missing_value = "true", require_equals = true)]
        no_ws: Option<bool>,
        /// Place real orders (without this flag the bot runs in paper mode:
        /// full loop, prints intended actions, no orders placed)
        #[arg(long)]
        live: bool,
        /// Maximum authenticated order-response reconnect attempts per round.
        /// Each attempt first cleans maker orders and must
        /// reconcile an empty maker book before quoting resumes. 0 disables.
        #[arg(long)]
        order_response_reconnect_attempts: Option<u32>,
        /// Base delay in seconds between order-response reconnect attempts.
        /// Later attempts use bounded exponential backoff.
        #[arg(long)]
        order_response_reconnect_backoff: Option<u64>,
        /// Maximum account-stream reconnect attempts per recovery round.
        /// Each attempt reconnects the
        /// authenticated account stream, replays buffered events, and backs
        /// fill gaps with REST trades before reconciling the venue position.
        /// 0 disables reconnect entirely (fail closed immediately).
        #[arg(long)]
        account_stream_reconnect_attempts: Option<u32>,
        /// Base delay in seconds between account-stream reconnect attempts.
        /// Later attempts use bounded exponential backoff.
        #[arg(long)]
        account_stream_reconnect_backoff: Option<u64>,
        /// Deprecated compatibility option. Transport faults now remain
        /// frozen and retry instead of stopping on an incident count.
        #[arg(long, hide = true)]
        recovery_incidents_per_window: Option<u32>,
        /// Deprecated compatibility option; accepted but no longer used.
        #[arg(long, hide = true)]
        recovery_window_secs: Option<u64>,
        /// Supervised fault injection: close the local order-response stream
        /// after this many seconds. Hidden because it is only for live-gate
        /// validation and is limited by the maker command to 60 seconds.
        #[arg(long, hide = true)]
        controlled_disconnect_after: Option<u64>,
    },
    /// Deterministically replay a normalized maker NDJSON trace (no network or order I/O)
    Replay {
        /// Normalized phase-1 trace file; use '-' to read stdin
        trace: PathBuf,
    },
    /// Supervised live-gate check for the WebSocket order:new/order:cancel path.
    #[command(hide = true)]
    WsCommandCanary {
        /// Symbol to verify (e.g., BTC-USD)
        symbol: String,
        /// Order quantity; defaults to the venue minimum for the symbol
        #[arg(long)]
        size: Option<f64>,
        /// Put the post-only buy this many bps below the mark to avoid taking
        #[arg(long, default_value_t = 100.0)]
        price_offset_bps: f64,
        /// Bound each response and REST visibility check (1..=30 seconds)
        #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u64).range(1..=30))]
        timeout_secs: u64,
        /// Required push channel for start, failure, and completion events
        #[arg(long, env = "STANDX_SUPERVISOR_WEBHOOK", required = true)]
        alert_webhook: String,
        /// Webhook payload format for the target chat platform
        #[arg(
            long,
            value_enum,
            default_value = "slack",
            env = "STANDX_SUPERVISOR_WEBHOOK_FORMAT"
        )]
        alert_webhook_format: AlertWebhookFormat,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum OutputFormat {
    #[default]
    Table,
    Json,
    Csv,
    Quiet,
}

/// Payload shape for `--alert-webhook`, per target chat platform.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum AlertWebhookFormat {
    /// Slack incoming webhook: `{"text": "..."}`.
    #[default]
    Slack,
    /// Feishu/Lark custom bot: `{"msg_type":"text","content":{"text":"..."}}`.
    Feishu,
    /// Telegram sendMessage: `{"text":"..."}` (put the bot token and
    /// `chat_id` in the URL, e.g. `.../bot<TOKEN>/sendMessage?chat_id=<ID>`).
    Telegram,
    /// Generic: the full structured object (text + ts/symbol/kind/firing).
    Raw,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn maker_live_and_canary_read_supervisor_webhook_environment() {
        let command = Cli::command();
        let maker = command
            .find_subcommand("maker")
            .expect("maker command should remain registered");

        for subcommand in ["run", "ws-command-canary"] {
            let command = maker
                .find_subcommand(subcommand)
                .expect("maker subcommand should remain registered");
            let webhook = command
                .get_arguments()
                .find(|argument| argument.get_id() == "alert_webhook")
                .expect("webhook argument should remain registered");
            let format = command
                .get_arguments()
                .find(|argument| argument.get_id() == "alert_webhook_format")
                .expect("webhook format should remain registered");

            assert_eq!(
                webhook.get_env().map(|value| value.to_string_lossy()),
                Some("STANDX_SUPERVISOR_WEBHOOK".into())
            );
            assert_eq!(
                format.get_env().map(|value| value.to_string_lossy()),
                Some("STANDX_SUPERVISOR_WEBHOOK_FORMAT".into())
            );
        }
    }

    #[test]
    fn local_maker_env_parser_accepts_only_supervisor_settings() {
        let parsed = parse_maker_local_env(
            r#"
                # ignored comment
                export STANDX_SUPERVISOR_WEBHOOK='https://hooks.example/canary'
                STANDX_SUPERVISOR_WEBHOOK_FORMAT="feishu"
                STANDX_JWT=must-not-be-loaded
            "#,
        );

        assert_eq!(
            parsed,
            MakerLocalEnv {
                webhook: Some("https://hooks.example/canary".to_string()),
                format: Some("feishu".to_string()),
            }
        );
    }

    #[test]
    fn local_maker_env_loads_for_canary_and_live_but_not_paper() {
        let args = |values: &[&str]| {
            values
                .iter()
                .map(|value| value.to_string())
                .collect::<Vec<_>>()
        };

        assert!(should_load_maker_local_env(&args(&[
            "standx",
            "maker",
            "ws-command-canary",
            "XAG-USD",
        ])));
        assert!(should_load_maker_local_env(&args(&[
            "standx", "maker", "run", "XAG-USD", "--live",
        ])));
        assert!(!should_load_maker_local_env(&args(&[
            "standx", "maker", "run", "XAG-USD",
        ])));
    }

    /// `--yes` exists exactly once, globally. A second copy on the subcommand
    /// makes clap panic in debug builds (`Long option names must be unique`) and
    /// silently splits the confirmation signal in release builds — the update
    /// command must read the global one.
    #[test]
    fn update_confirmation_comes_only_from_the_global_yes() {
        for argv in [
            vec!["standx", "update", "--yes"],
            vec!["standx", "--yes", "update"],
        ] {
            let cli = Cli::try_parse_from(&argv)
                .unwrap_or_else(|error| panic!("{argv:?} should parse: {error}"));
            assert!(cli.yes, "{argv:?} should set the global confirmation flag");
            assert!(matches!(cli.command, Commands::Update { .. }));
        }
        // The global flag is long-only, so there is no `-y`. Asserting that
        // keeps a future `-y` from being added on the subcommand alone, which is
        // how the duplicate crept in the first time.
        assert!(Cli::try_parse_from(["standx", "update", "-y"]).is_err());

        let cli = Cli::try_parse_from(["standx", "update"]).expect("bare update should parse");
        assert!(!cli.yes);
    }

    /// The command's own flags stay on the subcommand and default to off.
    #[test]
    fn update_flags_parse_independently() {
        let cli = Cli::try_parse_from(["standx", "update"]).expect("bare update should parse");
        let Commands::Update { check, pre, force } = cli.command else {
            panic!("expected update command");
        };
        assert!(!check && !pre && !force);

        let cli = Cli::try_parse_from(["standx", "update", "--check", "--pre", "--force"])
            .expect("all update flags should parse");
        let Commands::Update { check, pre, force } = cli.command else {
            panic!("expected update command");
        };
        assert!(check && pre && force);

        // The documented alias must keep working.
        let cli = Cli::try_parse_from(["standx", "self-update", "--check"])
            .expect("self-update alias should parse");
        assert!(matches!(cli.command, Commands::Update { check: true, .. }));
    }

    /// Clap's duplicate-option assertion only fires in debug builds, so a
    /// release-only smoke test cannot catch it — that is exactly how the
    /// duplicate `--yes` shipped through manual testing. This asserts the update
    /// subtree specifically.
    ///
    /// Scoped rather than whole-tree on purpose: `Cli::command().debug_assert()`
    /// currently fails on a **pre-existing** collision in `block list`, where
    /// `-s` is claimed by both `--symbol` and `--status` (so `standx block list
    /// --help` panics in debug builds today). Fixing that changes a published
    /// short flag and belongs to its own change.
    #[test]
    fn update_subcommand_passes_clap_debug_assertions() {
        use clap::CommandFactory;
        Cli::command()
            .find_subcommand("update")
            .expect("update subcommand exists")
            .clone()
            .debug_assert();
    }

    #[test]
    fn adaptive_spread_flag_accepts_bare_true_and_explicit_false() {
        for (argument, expected) in [
            ("--adaptive-spread", true),
            ("--adaptive-spread=false", false),
        ] {
            let cli = Cli::try_parse_from(["standx", "maker", "run", "XAG-USD", argument])
                .expect("adaptive spread flag should parse");
            let Commands::Maker { command } = cli.command else {
                panic!("expected maker command");
            };
            let MakerCommands::Run {
                adaptive_spread, ..
            } = *command
            else {
                panic!("expected maker run command");
            };
            assert_eq!(adaptive_spread, Some(expected));
        }
    }

    #[test]
    fn size_skew_flag_accepts_bare_true_and_explicit_false() {
        for (argument, expected) in [("--size-skew", true), ("--size-skew=false", false)] {
            let cli = Cli::try_parse_from(["standx", "maker", "run", "XAG-USD", argument])
                .expect("size skew flag should parse");
            let Commands::Maker { command } = cli.command else {
                panic!("expected maker command");
            };
            let MakerCommands::Run { size_skew, .. } = *command else {
                panic!("expected maker run command");
            };
            assert_eq!(size_skew, Some(expected));
        }
    }
}
