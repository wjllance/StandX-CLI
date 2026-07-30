# StandX Agent Toolkit

> **Trade by Intent. Built for Agents, Not Buttons.**

[![Rust](https://img.shields.io/badge/rust-1.75%2B-orange.svg)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](LICENSE)

**StandX Agent Toolkit** is a CLI for the AI trading era: any AI Agent that can execute a shell command can read markets, manage positions, and place orders—structured input in, structured output out.

We believe the future of trading is conversational. Your agent should trade as naturally as it chats. No complex APIs, no boilerplate—just intent to execution.

```
┌─────────────────────────────────────────────────────────────────┐
│                                                                 │
│   You: "Check my BTC position"                                  │
│   ↓                                                             │
│   Your agent → StandX CLI → StandX API                          │
│   ↓                                                             │
│   You: "Long 0.1 BTC, stop loss at $62k"                        │
│   ↓                                                             │
│   ✅ Order executed in seconds                                  │
│                                                                 │
│   Your agent now trades as naturally as it converses.           │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

**Status** — v1.2.0. Market data, account, order, leverage/margin, streaming and
dashboard commands are stable and in daily use. The [Rust SDK](crates/standx-sdk)
is usable but pre-1.0 (API may change). The [maker bot](docs/13-maker.md) is
paper-mode by default and its live mode is env-gated; its real-money PnL is still
being measured.

> ⚠️ This tool places real orders with real money. Nothing here is investment
> advice — you own the risk of anything your agent executes.

---

## 🎯 Why StandX Agent Toolkit?

### The Problem

You have an AI Agent (Claude, Cursor, OpenClaw, AutoGPT, …). You want it to trade. But:
- ❌ Traditional trading tools are built for humans clicking buttons
- ❌ APIs require complex integration and parsing
- ❌ No bridge between natural language and execution

### The Solution

**Agent-First Design**—structured output, non-interactive, composable:

| Feature | Traditional Tools | StandX Agent Toolkit |
|---------|-------------------|----------------------|
| **Built For** | Human traders | **AI Agents** |
| **Agent Integration** | Custom wrapper code | **Shell exec, no wrapper** |
| **Output** | Pretty tables | **Structured JSON** |
| **Errors** | Text to parse | **Machine-readable** |
| **Workflow** | Interactive prompts | **100% scriptable** |

---

## 🚀 Quick Start

### 1. Install

#### Option 1: One-line Installer (Recommended)

```bash
# macOS (Apple Silicon) / Linux (x86_64 & ARM64)
curl -sSL https://raw.githubusercontent.com/wjllance/standx-cli/main/install.sh | sh
```

Optional environment variables:

- `STANDX_VERSION` — install a specific tag instead of the latest release (e.g. `STANDX_VERSION=v1.2.0`).
- `INSTALL_DIR` — install somewhere other than `/usr/local/bin`.

#### Option 2: Homebrew (macOS)

```bash
brew tap wjllance/standx-cli
brew install standx-cli
```

#### Option 3: Build from Source

```bash
git clone https://github.com/wjllance/standx-cli && cd standx-cli
cargo install --path crates/standx-cli
```

#### Keeping it current

```bash
standx update --check     # compare installed vs latest release, change nothing
standx update             # download, verify sha256, replace this binary
standx --yes update       # same, no prompt (for scripts; also STANDX_AUTO_CONFIRM=true)
```

`update` refuses to touch a Homebrew-managed binary — use `brew upgrade
standx-cli` there so the formula and the binary stay in agreement. It never
elevates privileges: if the install directory is not writable it says so instead
of reaching for `sudo`.

### 2. Configure

Market data needs no credentials. Everything else needs a **JWT token**, and
trading additionally needs an **Ed25519 private key** for request signing. Both
come from https://standx.com/user/session (the JWT is valid for 7 days).

**Environment variables — the agent-friendly path (auto-detected, no login step):**
```bash
export STANDX_JWT="your_jwt_token"
export STANDX_PRIVATE_KEY="your_private_key"   # optional; required for trading
```

**Or store them once:**
```bash
standx auth login --interactive                       # first-time setup
standx auth login --token "$STANDX_JWT" --private-key "$STANDX_PRIVATE_KEY"
standx auth login --token-file ~/.standx_token --key-file ~/.standx_key
standx auth status                                    # expiry + trading availability
standx auth logout
```

#### Permission Requirements

| Operation | JWT Token | Private Key |
|-----------|-----------|-------------|
| Market data (ticker, depth) | ❌ No | ❌ No |
| Account info (balances, positions) | ✅ Yes | ❌ No |
| View orders & trades | ✅ Yes | ❌ No |
| **Create/cancel orders** | ✅ Yes | ✅ **Yes** |
| **Change leverage** | ✅ Yes | ✅ **Yes** |
| **Margin operations** | ✅ Yes | ✅ **Yes** |

> **Note:** Trading operations require the Ed25519 private key for request signing. If you only provide the JWT token, you'll see: `⚠️ No private key provided - trading operations will be unavailable`

For detailed authentication documentation, see [docs/02-authentication.md](docs/02-authentication.md).

### 3. Use With Your Agent

#### Conversational agents

```
You: What's the BTC price?
Agent: [executes: standx market ticker BTC-USD --output json]
       BTC is trading at $65,000 (+2.3% today)

You: Buy 0.1 BTC at market price
Agent: [executes: standx order create BTC-USD buy market --qty 0.1]
       ✅ Market order executed
       Bought 0.1 BTC at $65,001
```

#### Programmatic (subprocess)

```python
# Same commands work everywhere
import subprocess

result = subprocess.run(
    ["standx", "market", "ticker", "BTC-USD", "--output", "json"],
    capture_output=True
)
data = json.loads(result.stdout)
```

---

## 🛠️ Integration Patterns

### Pattern 1: Shell exec (any agent)

Any agent that can run a shell command is already integrated — there is no
wrapper to write. The same commands work everywhere:

```python
# Generic agent runtime (OpenClaw, Claude Code, Cursor, …)
result = await exec("standx market ticker BTC-USD --output json")
price_data = json.loads(result.stdout)
```

```python
# LangChain
from langchain.tools import ShellTool

tool = ShellTool()
result = tool.run("standx account balances --output json")
```

```python
# AutoGPT
# Add to skills
os.system("standx order create BTC-USD buy market --qty 0.1")
```

**Best for**: conversational trading, multi-platform agents, custom workflows

### Pattern 2: Embed the Rust SDK

When a subprocess per call is too coarse — typed models, persistent WebSocket
streams, order commands over WS:

```rust
let client = standx_sdk::client::StandXClient::new()?;
let ticker = client.get_symbol_market("BTC-USD").await?;
```

**Best for**: Rust bots and long-running strategies. See [Rust SDK](#-rust-sdk).

> MCP support is on the [roadmap](#️-roadmap), not implemented — there is no
> `standx mcp` command today.

---

## 📋 Command Reference

### Market Data

```bash
# Price
standx market ticker BTC-USD --output json

# Order book
standx market depth BTC-USD --limit 10 --output json

# Recent trades
standx market trades BTC-USD --limit 20 --output json

# Candles
standx market kline BTC-USD --resolution 60 --limit 100 --output json

# Funding rate
standx market funding BTC-USD --days 7 --output json
```

### Account

```bash
# Balance
standx account balances --output json

# Positions
standx account positions --symbol BTC-USD --output json

# Open orders
standx account orders --symbol BTC-USD --output json
```

### Trading

```bash
# Market order
standx order create BTC-USD buy market --qty 0.1

# Limit order
standx order create BTC-USD buy limit --qty 0.1 --price 64000

# Authenticated WebSocket order with correlated response
standx --output json --verbose order create BTC-USD buy limit \
  --qty 0.1 --price 64000 --transport ws

# With stop loss and take profit
standx order create BTC-USD buy limit --qty 0.1 --price 64000 \
  --sl-price 62000 --tp-price 68000

# Cancel
standx order cancel BTC-USD --order-id ord_xxx
standx order cancel-all BTC-USD
```

`order create` and single-order `order cancel` accept
`--transport <http|ws>` (default `http`) and `--timeout-secs` (default `10`,
range `1..=30`). WebSocket mode returns the correlated
`request_id`/`response_code`/`response_message`; `--verbose` writes the raw
post-authentication inbound response to stderr. A timeout is an unknown
submission state and never triggers an automatic REST retry.

### Dashboard

```bash
# Launch real-time trading dashboard
standx dashboard

# Watch specific symbols
standx dashboard --symbols BTC-USD,ETH-USD,SOL-USD

# Auto-refresh mode (updates every 5 seconds)
standx dashboard --watch
```

### Leverage & Margin

```bash
# Get leverage
standx leverage get BTC-USD

# Set leverage
standx leverage set BTC-USD 10

# Get margin mode
standx margin mode BTC-USD

# Set margin mode
standx margin mode BTC-USD --set isolated

# Move margin in/out of an isolated position
standx margin transfer BTC-USD 100 --direction in
```

### Trade History

```bash
# Get recent trades
standx trade history BTC-USD --from 1d

# With time range
standx trade history BTC-USD --from 2024-01-01 --to 2024-01-07
```

### Portfolio

```bash
# Get portfolio summary
standx portfolio

# Verbose mode with more details
standx portfolio --verbose

# Auto-refresh mode
standx portfolio --watch
```

### Streaming

```bash
# Real-time price stream
standx stream price BTC-USD

# Order book depth
standx stream depth BTC-USD --levels 5

# Public trades
standx stream trade BTC-USD

# Authenticated streams (requires login)
standx stream order      # Order updates
standx stream position   # Position updates
standx stream balance    # Balance updates
standx stream fills      # Fill updates
```

### Block Trades

```bash
# List block trades (optionally filter by symbol / status)
standx block list --symbol BTC-USD --status completed

# Watch block trades (polling)
standx block watch --interval 10
```

### Config

```bash
standx config init                    # create the config file
standx config set default_symbol BTC-USD
standx config get default_symbol
standx config show
```

### Global Flags

Available on every command:

| Flag | Effect |
|------|--------|
| `--output <table\|json\|csv\|quiet>` | Output format. Agents want `json`. |
| `--openclaw` | Machine-oriented defaults for agent execution (env: `STANDX_OPENCLAW_MODE`) |
| `--dry-run` | Report the command's financial-impact class and exit without touching the network |
| `--yes` | Skip the `standx update` confirmation (env: `STANDX_AUTO_CONFIRM=true`). Today `update` is the only command that prompts — trading commands are non-interactive and have nothing to skip |
| `--config <PATH>` | Use a specific config file |
| `--verbose` / `--quiet` | Log verbosity |

### Maker Bot (SIP-5A Community Maker Yield)

A two-sided quoting loop targeting
[SIP-5A](https://docs.standx.com/sip/sip-5a-community-maker-yield): quotes rest
inside the eligibility band (resting uptime is what earns) and only re-quote when
mark price drifts past a threshold — no flicker-cancelling.

```bash
# Paper mode (default): runs the full loop, prints intended actions, places NO
# orders. Safe without credentials; fills are simulated when the touch crosses
# a quote, so position, skew and PnL telemetry are observable offline.
standx maker run BTC-USD --size 0.001 --interval 3

# Machine-readable JSON lines, one object per action
standx maker run BTC-USD --output json

# Live mode places real post-only (ALO) orders, requires a private key, and is
# gated behind STANDX_ENABLE_LIVE_MAKER=1 pending supervised production testing.
standx maker run BTC-USD --live
```

It is more than a quoting loop — inventory skew, a volatility circuit breaker,
account-level hard floors, ownership isolation (it only touches its own `sxmk-`
orders), bounded reconnect with position reconciliation, webhook alerts, and full
net-PnL attribution including funding. Roughly 11k lines of strategy and risk
engine live in [`crates/standx-maker`](crates/standx-maker/README.md).

Full guide: **[docs/13-maker.md](docs/13-maker.md)** (every flag, the anti-flicker
decision table, telemetry, live safety rails) · live unlock criteria:
**[docs/14-maker-live-gate.md](docs/14-maker-live-gate.md)** · structured log
collection and SQL analysis: **[docs/15-openobserve.md](docs/15-openobserve.md)**.

### Self-update

```bash
standx update --check              # report installed vs latest; exit
standx update                      # verify + replace (prompts unless --yes)
standx --yes update                # no prompt; required when stdin is not a TTY
standx update --pre                # allow pre-release candidates
standx update --force              # reinstall the current version
standx -o json update --check      # machine-readable check
```

The release asset for the running platform is downloaded over TLS and its
SHA-256 verified against the `checksums.txt` published beside it, the archive's
`standx` binary is asked for its own `--version` to confirm it matches the
release, and only then is it atomically renamed over the running executable.

Two limits worth knowing: checksum verification protects against a corrupted or
truncated download, **not** against a compromised release (the checksum ships
from the same place as the archive — provenance would need a detached
signature); and a Homebrew-managed install is refused rather than silently
diverging from its formula.

---

## 💡 Use Cases

### 1. Natural Language Trading

```
You: "I want to long ETH with 0.5 size, entry at 3500"
Agent: "I'll place a limit buy order for 0.5 ETH at $3,500.
        Current price is $3,480. Confirm?"
You: "Yes"
Agent: "✅ Order placed. Order ID: ord_eth_xxx"
```

### 2. Automated Strategy (Any Agent)

```python
# Grid trading bot
async def grid_trade():
    ticker = await exec("standx market ticker BTC-USD --output json")
    price = json.loads(ticker.stdout)["mark_price"]
    
    if price < lower_bound:
        await exec(f"standx order create BTC-USD buy limit --qty 0.01 --price {buy_price}")
```

### 3. Multi-Agent Coordination

```python
# Risk monitoring agent
while True:
    positions = await exec("standx account positions --output json")
    # Alert if exposure too high
    
# Execution agent
await exec("standx order create ...")
```

---

## 🗺️ Roadmap

### Phase 1: Agent-Ready CLI (Done)

**Goal**: Best-in-class experience for any CLI-capable agent

- [x] Structured JSON output
- [x] Non-interactive mode
- [x] Dashboard for real-time monitoring
- [x] WebSocket streaming
- [x] Complete trading commands (order, leverage, margin)
- [x] `--openclaw` optimized defaults
- [x] [OpenClaw skill package](openclaw/) (`standx-cli` skill, brew-installable)

### Phase 2: Universal Agent Toolkit (Current)

**Goal**: Seamless experience across all AI Agents

- [x] Comprehensive testing framework
- [x] Reusable `standx-sdk` crate (REST/WS/signing, presentation-free) — see [crates/standx-sdk](crates/standx-sdk)
- [x] Maker bot (SIP-5A): anti-flicker quoting, inventory skew, risk engine, and net-PnL attribution — see [docs/13-maker.md](docs/13-maker.md)
- [ ] Session persistence & batch execution
- [ ] Portfolio PnL analysis
- [ ] Python SDK - `pip install standx-agent`
- [ ] More strategy templates (Grid, DCA, TWAP)
- [ ] Webhook callbacks
- [ ] MCP support (optional enhancement)

### Phase 3: AI Trading Ecosystem (Future)

**Goal**: Define the standard for AI-native trading

- [ ] Multi-exchange abstraction
- [ ] Natural language strategy builder
- [ ] Agent marketplace
- [ ] Cross-agent coordination protocol

---

## 🤝 Comparison

| Tool | Agent integration | Scope |
|------|-------------------|-------|
| **StandX Agent Toolkit** | 🟢 Shell exec, structured JSON | StandX only |
| Hummingbot | 🔴 Full framework with its own runtime | Many venues, mature |
| CCXT | 🟡 Library — needs a wrapper | Many venues, mature |
| Hyperliquid SDK | 🟡 Library — needs a wrapper | Hyperliquid only |

> These are mature, broader-scope projects — the axis here is agent integration,
> not feature coverage or exchange support.

---

## 🏗️ Project Structure

A Cargo workspace of three crates:

```
standx-cli/
├── crates/standx-sdk/     # lib: REST client, WebSocket streams, typed models,
│                          #      Ed25519 signing. Presentation-free.
├── crates/standx-maker/   # lib: market-making strategy + risk engine. Pure
│                          #      decision functions, no I/O. → standx-sdk
└── crates/standx-cli/     # bin `standx`: commands, output, config, telemetry.
                           #      → standx-sdk, standx-maker
```

Strategy logic (quoting, reconcile, inventory skew, risk gates, PnL accounting)
lives in `standx-maker` as pure functions over plain values — no network, no
printing — so it is unit-testable offline and embeddable without the CLI. The
`standx` binary, install scripts, and Homebrew formula are unaffected by the
crate split.

---

## 📦 Rust SDK

[`standx-sdk`](crates/standx-sdk) is the library the CLI is built on, published
as a standalone crate for Rust bots that need typed models, persistent streams,
and request signing without spawning a subprocess per call.

Not on crates.io yet — depend on it by git:

```toml
standx-sdk = { git = "https://github.com/wjllance/standx-cli" }
```

- `client` — REST: market data, account, orders, leverage/margin, funding
- `websocket` — public streams (`price`, `depth_book`, `public_trade`, `kline`) with venue sequence numbers
- `account_stream` — authenticated order/position/trade/balance events with gap detection
- `order_response` — place and cancel over the authenticated WS command channel
- `auth` — Ed25519 request signing, JWT loading and expiry inspection

Zero presentation dependencies by default; table rendering sits behind the
optional `tabled` feature that only the CLI enables. Pre-1.0 — the API can still
change.

Full module tour and examples: **[crates/standx-sdk/README.md](crates/standx-sdk/README.md)**.

---

## 🛡️ Safety Features

Safety an agent can check programmatically, not just read about:

- **Structured errors** — with `--output json`, failures print
  `{error: {error_type, message}, timestamp}` to **stderr** and exit non-zero, so
  an agent branches on a field rather than a message string.
- **Dry-run** — `--dry-run` reports the command's financial-impact class and exits
  without touching the network. It is a category-level check, not an order
  simulator.
- **Retryable-error classification** — the SDK marks transport failures and HTTP
  429 as retryable and surfaces `RateLimitExceeded { retry_after }`, so callers
  can back off deliberately. There is no client-side throttle — pacing is the
  caller's job.
- **Paper by default** — the maker bot runs its full loop and places no orders
  unless `--live`, which additionally requires `STANDX_ENABLE_LIVE_MAKER=1`.
- **Ownership isolation** — the maker only cancels orders carrying its own
  `sxmk-` client-order-id prefix; it never touches orders it didn't place.

---

## 📝 Philosophy

**Intent to Execution** — an agent should get from a sentence to a resting order without a wrapper layer in between.

**Structured by Default** — machine consumption over human readability: JSON output, typed errors, non-zero exit codes.

**Any Agent, No Privilege** — the CLI is the universal interface. Claude, Cursor, OpenClaw, LangChain all run the same commands.

**Layered, Not Monolithic** — the CLI is the agent surface; [`standx-sdk`](#-rust-sdk) is there when a subprocess per call is too coarse; `standx-maker` is strategy with no I/O. MCP and a Python SDK are still roadmap.

---

## 📜 License

MIT OR Apache-2.0

---

## 💰 Support

If your agent made some gains, you can sponsor API tokens:
`0xAb3D58779dFC50BC84caA796003ABE31b5296210` (EVM). Every bit helps. ⛽

---

**Built for the AI Trading era.**

*Trade by intent. Built for agents, not buttons.*
