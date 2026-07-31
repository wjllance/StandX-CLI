---
name: standx-cli
description: "Crypto trading CLI for StandX exchange v1.3.1. Use when users need to: (1) Query crypto market data (prices, order books, klines, funding rates), (2) Manage trading orders (create, cancel, view), (3) Check account balances, positions, and trade history, (4) Stream real-time market data via WebSocket, (5) Manage leverage and margin settings, (6) Monitor real-time dashboard, (7) View portfolio summary. Supports BTC, ETH, SOL, XRP and other trading pairs."
metadata:
  {
    "openclaw":
      {
        "emoji": "📈",
        "requires": { "bins": ["standx"] },
        "primaryCredential":
          {
            "kind": "env",
            "env": "STANDX_JWT",
            "description": "StandX JWT token from https://standx.com/user/session (valid 7 days)",
          },
        "optionalEnvVars":
          [
            {
              "name": "STANDX_PRIVATE_KEY",
              "description": "Ed25519 private key (Base58) for trading operations",
              "sensitive": true,
            },
          ],
        "install":
          [
            {
              "id": "brew",
              "kind": "brew",
              "formula": "wjllance/standx-cli/standx-cli",
              "bins": ["standx"],
              "label": "Install StandX CLI via Homebrew",
            },
          ],
      },
  }
---

# StandX CLI Skill

StandX CLI is a crypto trading command-line tool for the StandX exchange.

## Installation

### Option 1: ClawHub (Recommended - Auto-install)

```bash
clawhub install standx-cli
```

### Option 2: Homebrew

> **Security Note:** This uses a third-party tap. You can inspect the formula before installing:
> ```bash
> # View the formula source
> curl -sL https://raw.githubusercontent.com/wjllance/homebrew-standx-cli/main/standx-cli.rb
> 
> # Then install
> brew tap wjllance/standx-cli
> brew install standx-cli
> ```

```bash
brew tap wjllance/standx-cli
brew install standx-cli
```

## Quick Start

Check installation:

```bash
standx --version
```

View BTC price:

```bash
standx market ticker BTC-USD
# Or use short alias
standx m t BTC-USD
```

### Command Short Aliases

StandX CLI supports short aliases for faster typing:

| Full Command | Short Alias |
|--------------|-------------|
| `standx market ticker` | `standx m t` |
| `standx market depth` | `standx m d` |
| `standx market kline` | `standx m k` |
| `standx account balances` | `standx a b` |
| `standx account positions` | `standx a p` |
| `standx account orders` | `standx a o` |
| `standx portfolio snapshot` | `standx p s` |
| `standx dashboard --watch` | `standx d -w` |

## Authentication

Most commands require authentication. StandX CLI supports multiple secure authentication methods.

### Environment Variables (Recommended)

The most secure way to authenticate. Credentials are not stored in shell history or command logs.

```bash
# Add to ~/.bashrc or ~/.zshrc
export STANDX_JWT="your_jwt_token"
export STANDX_PRIVATE_KEY="your_ed25519_private_key"

# Reload shell configuration
source ~/.bashrc
```

**Security Best Practices:**

- Never hardcode credentials in commands (appears in shell history)
- Never commit credentials to version control
- Set file permissions to 600 for any files containing credentials
- Rotate tokens regularly (they expire after 7 days)

### Get Credentials

Visit https://standx.com/user/session to generate:

- **JWT Token** (required) - Valid for 7 days, used for reading account data
- **Ed25519 Private Key** (optional but recommended) - Required for trading operations

### Verify Authentication

```bash
standx auth status
```

### Alternative Authentication Methods

#### Interactive Login

For first-time setup or testing:

```bash
standx auth login --interactive
```

#### File-based Login

For automation scripts where environment variables are not available:

```bash
# Store credentials in files with restricted permissions
echo "your_jwt_token" > ~/.standx_token
echo "your_private_key" > ~/.standx_key
chmod 600 ~/.standx_token ~/.standx_key

# Login using files
standx auth login --token-file ~/.standx_token --key-file ~/.standx_key
```

**⚠️ Avoid this in production:**

```bash
# DANGER: Credentials will be visible in shell history
standx auth login --token "your_token" --private-key "your_key"
```

### Logout

```bash
standx auth logout
```

## Market Data (No auth required)

### List trading pairs

```bash
standx market symbols
```

### Get ticker

```bash
standx market ticker BTC-USD
standx market ticker ETH-USD
```

### Order book depth

```bash
standx market depth BTC-USD --limit 10
```

### K-line (candlestick) data

```bash
# Last 24 hours, 1-hour candles
standx market kline BTC-USD -r 60 --from 1d

# Last 7 days, daily candles
standx market kline BTC-USD -r 1D --from 7d

# Specific date range
standx market kline BTC-USD -r 60 --from 2024-01-01 --to 2024-01-07
```

### Funding rate

```bash
standx market funding BTC-USD --days 7
```

## Account & Trading (Auth required)

### Account info

```bash
standx account balances
standx account positions
standx account orders
standx account history --limit 20
```

### Create order

```bash
# Limit buy
standx order create BTC-USD buy limit --qty 0.01 --price 60000

# Market sell
standx order create BTC-USD sell market --qty 0.01
```

### Cancel order

```bash
standx order cancel BTC-USD --order-id 123456
standx order cancel-all BTC-USD
```

### Trade history

```bash
standx trade history BTC-USD --from 7d
```

## Dashboard & Portfolio (Auth required)

### Real-time Dashboard

Interactive trading dashboard with auto-refresh:

```bash
# Launch dashboard with auto-refresh (watch mode)
standx dashboard --watch

# Dashboard for specific symbols
standx dashboard --symbols BTC-USD,ETH-USD

# Dashboard with custom refresh interval (seconds)
standx dashboard --watch --interval 5
```

**Dashboard Features:**
- Real-time account balance display
- Active positions with PnL
- Open orders summary
- Order book depth visualization
- Recent trades panel (BUY/SELL)
- Auto-refresh with graceful exit (Ctrl+C)

### Portfolio Snapshot

View portfolio summary and performance:

```bash
# Portfolio snapshot
standx portfolio snapshot

# Short alias
standx p s
```

## Leverage & Margin (Auth required)

```bash
# Query leverage
standx leverage get BTC-USD

# Set leverage
standx leverage set BTC-USD 10

# Query margin mode
standx margin mode BTC-USD

# Set margin mode
standx margin mode BTC-USD --set isolated
```

## Real-time Streaming

### Public streams (No auth)

```bash
# Price stream
standx stream price BTC-USD

# Order book stream
standx stream depth BTC-USD --levels 5

# Trade stream
standx stream trade BTC-USD
```

### User streams (Auth required)

```bash
standx stream order     # Order updates
standx stream position  # Position updates
standx stream balance   # Balance updates
standx stream fills     # Fill updates
```

## Output Formats

```bash
# JSON output
standx -o json market ticker BTC-USD

# CSV export
standx -o csv market symbols > symbols.csv

# Quiet mode (just values)
standx -o quiet config get base_url
```

## Special Modes

### OpenClaw mode (AI-optimized JSON)

```bash
standx --openclaw market ticker BTC-USD
```

### Dry run (preview without executing)

```bash
standx --dry-run order create BTC-USD buy limit --qty 0.01 --price 60000
```

## References

- [API Documentation](references/api-docs.md)
- [Authentication Details](references/authentication.md)
- [Command Examples](references/examples.md)
- [Troubleshooting](references/troubleshooting.md)

## Links

- GitHub: https://github.com/wjllance/standx-cli
- Docs: https://github.com/wjllance/standx-cli/tree/main/docs
- Issues: https://github.com/wjllance/standx-cli/issues
