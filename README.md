# StandX Agent Toolkit

> **OpenClaw First. AI Agent Native. Trading Ecosystem Ready.**

[![Rust](https://img.shields.io/badge/rust-1.75%2B-orange.svg)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](LICENSE)
[![OpenClaw](https://img.shields.io/badge/OpenClaw-First-blue.svg)](https://openclaw.ai)

**StandX Agent Toolkit** is a CLI designed for the AI Trading era—**OpenClaw First**, yet universally adaptable to any AI Agent that can execute commands.

We believe the future of trading is conversational. Your agent should trade as naturally as it chats. No complex APIs, no boilerplate—just intent to execution.

```
┌─────────────────────────────────────────────────────────────────┐
│                                                                 │
│   You: "Check my BTC position"                                  │
│   ↓                                                             │
│   OpenClaw → StandX CLI → StandX API                            │
│   ↓                                                             │
│   You: "Long 0.1 BTC, stop loss at $62k"                        │
│   ↓                                                             │
│   ✅ Order executed in seconds                                  │
│                                                                 │
│   Your agent now trades as naturally as it converses.           │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

---

## 🎯 Why StandX Agent Toolkit?

### The Problem

You have an AI Agent (OpenClaw, Claude, AutoGPT, etc.). You want it to trade. But:
- ❌ Traditional trading tools are built for humans clicking buttons
- ❌ APIs require complex integration and parsing
- ❌ No bridge between natural language and execution

### The Solution

**Agent-First Design**—structured output, non-interactive, composable:

| Feature | Traditional Tools | StandX Agent Toolkit |
|---------|-------------------|----------------------|
| **Built For** | Human traders | **AI Agents** |
| **OpenClaw Integration** | Custom code | **Works out of the box** |
| **Output** | Pretty tables | **Structured JSON** |
| **Errors** | Text to parse | **Machine-readable** |
| **Workflow** | Interactive prompts | **100% scriptable** |
| **Other Agents** | Not supported | **CLI = Universal** |

---

## 🚀 Quick Start

### 1. Install

#### Option 1: One-line Installer (Recommended)

```bash
# macOS (Apple Silicon) / Linux (x86_64 & ARM64)
curl -sSL https://raw.githubusercontent.com/wjllance/standx-cli/main/install.sh | sh
```

#### Option 2: Homebrew (macOS)

```bash
brew tap wjllance/standx-cli
brew install standx-cli
```

#### Option 3: Build from Source

```bash
cargo install standx-cli
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

StandX CLI requires authentication for most operations. You need:

1. **JWT Token** (required) - For reading account data
2. **Ed25519 Private Key** (optional, but recommended) - For trading operations

#### Get Credentials

Visit https://standx.com/user/session to generate:
- JWT Token (valid for 7 days)
- Ed25519 Private Key (Base58 encoded)

#### Login Methods

**Interactive (Recommended for first-time setup):**
```bash
standx auth login --interactive
```

**Command line (for scripts/agents):**
```bash
standx auth login \
  --token "$STANDX_JWT" \
  --private-key "$STANDX_PRIVATE_KEY"
```

**From files:**
```bash
standx auth login \
  --token-file ~/.standx_token \
  --key-file ~/.standx_key
```

**Environment variables (auto-detected):**
```bash
export STANDX_JWT="your_jwt_token"
export STANDX_PRIVATE_KEY="your_private_key"
```

#### Check Authentication Status

```bash
standx auth status
```

**Example output:**
```
✅ Authenticated
   Token expires at: 2024-02-02T09:56:07Z
   Remaining: 167 hours
```

#### Logout

```bash
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

#### OpenClaw (Native)

```
You: What's the BTC price?
OpenClaw: [executes: standx market ticker BTC-USD --output json]
          BTC is trading at $65,000 (+2.3% today)

You: Buy 0.1 BTC at market price
OpenClaw: [executes: standx order create BTC-USD buy market --qty 0.1]
          ✅ Market order executed
          Bought 0.1 BTC at $65,001
```

#### Claude / Cursor / Any CLI-capable Agent

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

### Pattern 1: OpenClaw Native (Recommended)

OpenClaw calls StandX CLI directly via `exec`:

```python
# In OpenClaw
result = await exec("standx market ticker BTC-USD --output json")
price_data = json.loads(result.stdout)
# Agent parses and responds naturally
```

**Best for**: OpenClaw users who want seamless conversation-to-trading

### Pattern 2: Universal CLI

Any AI Agent that can execute shell commands:

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

**Best for**: Multi-platform agents, custom workflows

### Pattern 3: Future MCP (Optional)

When you need richer tool definitions:

```bash
# Coming soon
standx mcp serve
```

**Best for**: Complex multi-step workflows across multiple services

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

# With stop loss and take profit
standx order create BTC-USD buy limit --qty 0.1 --price 64000 \
  --sl-price 62000 --tp-price 68000

# Cancel
standx order cancel BTC-USD --order-id ord_xxx
standx order cancel-all BTC-USD
```

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
```

:### Trade History

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

### Maker Bot (SIP-5A Community Maker Yield)

A simple two-sided quoting loop optimized for
[SIP-5A](https://docs.standx.com/sip/sip-5a-community-maker-yield): it keeps
quotes resting inside the eligibility band (uptime is what earns) and only
re-quotes when mark price drifts past a threshold — no flicker-cancelling.

```bash
# Paper mode (default): runs the full loop, prints intended actions,
# places NO orders. Safe to run without credentials. Fills are simulated
# (a quote crossed by the touch), so position and inventory skew are
# observable without going live.
standx maker run BTC-USD --size 0.001 --interval 3

# Tune the strategy
standx maker run BTC-USD \
  --spread-bps 5      # half-spread from mark price
  --band-bps 20       # never quote outside mark ± band
  --refresh-bps 3     # anti-flicker: re-quote only after this much drift
  --levels 2          # quote levels per side
  --max-position 0.05 # suppress the side that would exceed this
  --skew-bps 5        # inventory skew: at full inventory, shift the quote
                      # center this many bps toward the reducing side
                      # (0 = off; live only, paper holds no position)
  --vol-pause-bps 30  # volatility circuit breaker: pull all quotes when the
                      # mark's range over --vol-window cycles hits this (0 = off)

# Risk alerts (edge-triggered; stderr/JSON always, webhook optional):
standx maker run BTC-USD \
  --alert-loss 50            # alert when mark-to-market PnL <= -50
  --alert-inventory-pct 80   # alert when |position| hits 80% of --max-position
  --alert-uptime 90          # alert when two-sided uptime drops below 90%
  --alert-webhook https://hooks.slack.com/services/XXX  # also POST alerts here
  --alert-webhook-format slack   # slack | feishu | telegram | raw

# Market data comes from a WebSocket feed (REST fallback when stale);
# the loop also wakes early when mark has already drifted past
# --refresh-bps, shrinking the reaction window without adding flicker.
# --no-ws forces plain REST polling; --max-divergence-bps (default 25)
# skips a cycle when mark price and the book mid disagree.

# Each cycle reports running telemetry — mark-to-market PnL, favorable
# spread capture (bps/fill), two-sided uptime %, fills, and max inventory —
# and a stats block on exit, so tuning spread/refresh/skew is a measured
# loop rather than guesswork.

# Machine-readable JSON lines (one object per action)
standx maker run BTC-USD --output json

# Live mode places real post-only (ALO) orders and requires a private key.
# It is currently locked behind STANDX_ENABLE_LIVE_MAKER=1 until
# supervised production testing completes.
standx maker run BTC-USD --live
```

In live mode the bot manages only orders tagged with its `sxmk-` client-order
ID prefix: manual/API orders are preserved. It cleans up maker-owned orders on
exit (with retries and verification), stops quoting after 3 consecutive API
errors, and safely pauses on an asynchronous order-response disconnect. It
then cleans and verifies the maker book, authenticates a new response session,
and reconciles orders, position, and fills before quoting can resume. Reconnect
attempts are bounded per round. If a round exhausts only retryable transport
errors, the maker re-verifies its empty book, remains frozen, and retries with
bounded exponential backoff. Cleanup failure, unexplained position mismatch,
terminal authentication failure, or explicitly disabled reconnect still fails
closed.
Live fill telemetry is sourced from authenticated, maker-order-correlated trade
history rather than inferred from position changes. Paper mode simulates fills
when the touch crosses a quote, so position, inventory skew, and PnL telemetry
are observable without going live.

See **[docs/13-maker.md](docs/13-maker.md)** for the full guide — every flag,
the anti-flicker decision table, inventory skew, telemetry, and live safety
rails.

For local structured log collection and SQL analysis, see
**[docs/15-openobserve.md](docs/15-openobserve.md)**.

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

### 1. Natural Language Trading (OpenClaw)

```
You: "I want to long ETH with 0.5 size, entry at 3500"
OpenClaw: "I'll place a limit buy order for 0.5 ETH at $3,500. 
           Current price is $3,480. Confirm?"
You: "Yes"
OpenClaw: "✅ Order placed. Order ID: ord_eth_xxx"
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

### Phase 1: OpenClaw Excellence ✅ (Completed)

**Goal**: Best-in-class OpenClaw integration

- [x] Structured JSON output
- [x] Non-interactive mode
- [x] Dashboard for real-time monitoring
- [x] WebSocket streaming
- [x] Complete trading commands (order, leverage, margin)
- [ ] `--openclaw` optimized defaults
- [ ] Session persistence
- [ ] Batch execution

### Phase 2: Universal Agent Toolkit (Current)

**Goal**: Seamless experience across all AI Agents

- [x] Comprehensive testing framework
- [x] Reusable `standx-sdk` crate (REST/WS/signing, presentation-free) — see [workspace layout](#project-structure)
- [x] Maker bot (SIP-5A): anti-flicker quoting, inventory skew, paper fill simulation, and PnL/uptime telemetry — see [docs/13-maker.md](docs/13-maker.md)
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

| Tool | OpenClaw | Other Agents | Learning Curve |
|------|----------|--------------|----------------|
| **StandX Agent Toolkit** | 🟢 Native | 🟢 CLI = Universal | 🟢 Low |
| Hummingbot | 🔴 Complex | 🔴 Complex | 🔴 High |
| CCXT | 🟡 Wrapper needed | 🟡 Wrapper needed | 🟡 Medium |
| Hyperliquid SDK | 🟡 Integration needed | 🟡 Integration needed | 🟡 Medium |

---

## 🏗️ Project Structure

A Cargo workspace of two crates:

```
standx-cli/
├── crates/standx-sdk/    # lib: REST client, WebSocket streams, models,
│                         #      Ed25519 signing, maker strategy core.
│                         #      Reusable by any Rust agent/bot; zero
│                         #      presentation deps (tables behind a feature).
└── crates/standx-cli/    # bin `standx`: commands, output formatting,
                          #      config, telemetry. Depends on standx-sdk.
```

The `standx` binary, install scripts, and Homebrew formula are unchanged by
the split. Strategy logic (quoting, reconcile, skew, stats) lives in
`standx_sdk::maker` as pure, unit-tested functions — no I/O — so it can be
embedded or tested without a network.

---

## 🛡️ Safety Features

- **Structured errors** - Agents can handle errors programmatically
- **Dry-run mode** - Test without execution
- **Confirmation controls** - `--confirm` / `--no-confirm`
- **Rate limiting** - Built-in protection

---

## 📝 Philosophy

**OpenClaw First** — We optimize for the best OpenClaw experience first.

**Agent Native** — Every design decision prioritizes machine consumption over human readability.

**Ecosystem Ready** — CLI is the universal interface. Works with any agent, today.

**Future Proof** — MCP, SDKs, and advanced features come later. The foundation is solid.

---

## 📜 License

MIT OR Apache-2.0

---

**Built for the AI Trading era.**

*OpenClaw First. Agent Native. Ecosystem Ready.*

---

## 💰 Buy Me Some Tokens

```
┌────────────────────────────────────────────────────────────┐
│                                                            │
│   🤖 Your AI agent made some gains?                        │
│                                                            │
│   💸 Buy it some oil (sponsor API tokens)                  │
│                                                            │
│   ┌────────────────────────────────────────────────┐      │
│   │  0xAb3D58779dFC50BC84caA796003ABE31b5296210   │      │
│   └────────────────────────────────────────────────┘      │
│                                                            │
│   ✨ Support ongoing development & maintenance ✨          │
│                                                            │
└────────────────────────────────────────────────────────────┘
```

**EVM**: `0xAb3D58779dFC50BC84caA796003ABE31b5296210`

*Every token counts. Even a gas fee is appreciated!* ⛽🙏
