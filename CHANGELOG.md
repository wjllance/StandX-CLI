# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **Maker bot: `standx maker run <SYMBOL>`** (alias `mk`) — two-sided quoting loop targeting SIP-5A community maker yield
  - Anti-flicker reconcile: quotes rest inside the eligibility band and only re-quote when mark drifts past `--refresh-bps`
  - Flags: `--spread-bps`, `--band-bps`, `--size`, `--levels`, `--level-step-bps`, `--refresh-bps`, `--interval`, `--max-position`
  - **Paper mode by default** (full loop, prints intended actions, no orders); `--live` implements real post-only quoting but is locked behind `STANDX_ENABLE_LIVE_MAKER=1` pending supervised production testing
  - Live safety rails: startup cancel-all, exchange open-orders as reconciliation truth, cancel-all-with-retry + verification on exit, fail-safe stop after 3 consecutive API errors
  - JSON-lines output for agents (`--output json` / `--openclaw`)
  - Pure quoting/reconcile core in `standx_sdk::maker` with 26 unit tests
  - Volatility circuit breaker (`--vol-pause-bps`, default 0/off; `--vol-window`, default 12): halts quoting (pulls all resting quotes) when the mark's peak-to-trough range over the window reaches the threshold, and resumes once it falls below half that (hysteresis — the move must roll out of the window). Guards against getting run over during fast moves. Halted cycles surface as `⚡HALT` on the human line, `halted`/`vol_bps` in the JSON summary, and a count in the exit summary
  - Risk alerts (`AlertMonitor`): edge-triggered threshold alerts on the financial risks the telemetry previously only displayed — `--alert-loss` (mark-to-market PnL floor), `--alert-inventory-pct` (position reaches % of `--max-position`), `--alert-uptime` (two-sided uptime floor, after warmup). Each opt-in (0 disables), fires once on breach and once on recovery (no per-cycle spam). Delivered to stderr / JSON always, and to an optional `--alert-webhook` URL, spawned fire-and-forget so a slow endpoint never stalls the loop. `--alert-webhook-format` shapes the payload per platform: `slack` (default), `feishu` (Lark custom bot), `telegram` (sendMessage — token/chat_id in the URL), or `raw` (full structured JSON)
  - Lifecycle webhook: when `--alert-webhook` is set, the bot also posts a 🟢 started message (with mode + key params) on launch and a 🔴 stopped message (with the reason — Ctrl+C or fail-safe — and the session summary) on every exit path. Fires regardless of whether any risk threshold is configured; the stop message awaits delivery so it lands before the process exits. Also emitted as `action:"lifecycle"` JSON lines
  - Inventory skew (`--skew-bps`, default 0/off): shifts the quote center by current position so the reducing side quotes nearer mark and the growing side further, turning `--max-position` from a hard brake into gradual mean reversion. The anti-flicker anchor generalizes from "mark at placement" to "quote center at placement," so the same re-quote rule reacts to both mark drift and inventory skew
  - Paper-mode fill simulation: a resting quote crossed by the touch is treated as filled and folds its signed qty into a simulated position, so inventory (and thus skew) is now observable in paper mode without going live. Fills surface as `FILL` lines / `fill` JSON events and in the exit summary (fills count + ending position)
  - Session telemetry (`standx_sdk::maker::MakerStats`): mark-to-market PnL, favorable spread capture (bps/fill), two-sided uptime %, fill count/volume, and max inventory. Surfaced as `pnl=` on each human cycle line, `pnl`/`uptime_pct`/`avg_capture_bps`/`fills_total` in the JSON cycle summary, and a stats block in the exit summary. Works in paper (exact simulated fills) and live (fills inferred from position deltas). Turns skew/spread/refresh tuning into a measured loop
  - Live cycle output now includes a real exchange account snapshot (`balance`, `equity`, `available`, and account `upnl`) in both human and JSON output, kept distinct from the maker session's own mark-to-market `pnl`
  - WebSocket market feed (price + depth on one connection) with automatic REST fallback when the feed is warming up or stale; `--no-ws` forces REST polling
  - Early re-quote: wakes before the interval elapses when the cached mark has already drifted past `--refresh-bps` (only fires when a re-quote would happen anyway — no added flicker; 1s min gap)
  - mark/mid divergence guard (`--max-divergence-bps`, default 25): skips the cycle without touching resting quotes when mark price and book mid disagree
  - Error classification: post-only (ALO) would-cross rejections and cancels of already-gone orders are treated as normal events (logged, re-quoted next cycle) instead of counting toward the 3-consecutive-error fail-safe — only transient failures (network, 5xx) trip it
  - Bounded order-response recovery: on an authenticated response-stream disconnect, live quoting pauses, maker-owned orders are cancelled and verified, a fresh session is authenticated, and open orders/position/filled orders/session trades are reconciled before quoting resumes. `--order-response-reconnect-attempts` (default 3, 0 disables) and `--order-response-reconnect-backoff` bound recovery; cleanup, authentication, reconciliation, or budget failure remains fail-closed and emits structured `order_response_reconnect` events
  - Partial-fill tolerance: a partially-filled resting order keeps its identity (adopted by side + price, qty ≤ placed) and holds its remainder instead of being cancelled as an unknown order
- **Funding cashflow in maker net-PnL attribution** — `StandXClient::get_funding_history` wraps the authenticated `GET /api/query_funding_history`, and the maker folds it into the performance ledger on the 30s REST audit, so `funding_quote` / `funding_available` / `net_pnl_complete` finally carry real values instead of always reporting an incomplete attribution
  - Authoritative, not derived: the venue's signed `qty` is the cashflow (negative paid / positive received), matching the ledger's existing convention. The public `query_funding_rates` history returns empty on this venue, so a rate-based reconstruction was never an option
  - Dedup is by row id, not by request cursor: `last_id` pages *backward* into history, so every audit re-reads the same recent page and `record_funding` (which only accumulates) would otherwise double-count
  - Rows that exist but cannot be folded in — a settlement asset that is neither the quote nor its D-prefixed form, or an out-of-order arrival — are counted in the new `funding_unattributed` and clear `net_pnl_complete` rather than being silently dropped. An out-of-order row never stops the maker: funding is attribution, not position accounting
  - Measured scale on HYPE (why it matters): hourly settlement, 91 rows over 137h summing to -0.006252 DUSD (-0.0011/24h) against a ~36h baseline net PnL of +0.006 — roughly 10–30% of the reading, in the same direction
- `TimeInForce::Alo` (post-only / add-liquidity-only), matching the backend enum; `standx order create --tif ALO` now supported
- Block trade commands: `standx block list` / `standx block watch`
- **Maker stage 5-b (safety track, tier 2): graded exit policy and residual handoff** — the code-level prerequisite for scaling up. No default live behavior changes; everything new is either telemetry or off by default. See `docs/26-maker-stage5b-design.md`
  - Typed exit policies: `ExitKind::{InventoryTrim, WindDown}` distinguishes a threshold-driven inventory trim from a supervisor wind-down all the way through plan → execution → telemetry, so evidence no longer has to infer an exit's origin. `inventory_exit_submitted` carries `exit_kind`
  - Exit suppression is observable: `CyclePlan::exit_suppression` reports `volatility_halt` / `market_data_inactive` as a typed value, surfaced as the additive `cycle_summary` fields `exit_kind` / `exit_submitted` / `exit_suppressed`. A volatility halt suppresses **both** exit kinds and there is deliberately no emergency-exit-during-halt policy (design decision D1)
  - Residual position handoff on every exit path, decided **after** cleanup against a venue REST snapshot (an order can fill while it is being cancelled, and the account stream is gone by then): `action:"residual_position"` with `event: flat | handoff | unknown`, plus `venue_position` / `ledger_position` / `unknown_reason` / `needs_operator`. A missing snapshot, a non-finite position, or a venue/ledger disagreement is reported as `unknown` (critical) — "cannot confirm flat" is never rendered as flat. Also a critical `kind:"residual_position"` webhook and the residual in the `🔴 maker stopped` lifecycle message. The maker never auto-flattens
  - Account-level hard floors, default off: `--stop-equity-below` / `--stop-margin-below` (also TOML `stop_equity_below` / `stop_margin_below`) stop the session through the separate `RuntimeStopReason::AccountFloor` / `action:"account_floor"`, distinct from the strategy's `--stop-loss`. Evaluated inside the cycle **before any order work**, so a breached snapshot cannot add exposure in the cycle that observed it. Equity is checked before margin. With a floor armed, a balance older than 35s or an unparseable field the floor actually reads fails closed (`event:"unevaluable"`) instead of reading as "no breach", and an armed floor now counts as watching account risk for balance-refresh purposes; disarmed floors (the default) can never stop a run
  - `AlertMonitor::with_account_floors` renamed to `with_account_alerts` (with `equity_alert_below` / `margin_alert_below` fields): "floor" now means only the hard brake, never an alert threshold

### Changed
- **Workspace split: `standx-sdk` extracted as an independent crate**
  - `crates/standx-sdk` (v0.1.0): REST client, WebSocket streams, models, auth/signing, errors — reusable by any Rust agent/bot; zero presentation dependencies by default (table rendering behind the optional `tabled` feature)
  - `crates/standx-cli` (v0.8.0): the `standx` binary — commands, output formatting, config, telemetry; re-exports the SDK surface for backward compatibility
  - Release artifacts unchanged (binary name `standx`, same CI/homebrew/install.sh flow)
- Removed unused dependencies: `comfy-table`, `once_cell`, `config`, `keyring` (and the vestigial `no-keyring` feature)

### Fixed
- Kline streaming: handle symbol/interval in parent message
- `order create`: removed `-q` short flag (collided with global `--quiet`; clap panics on the collision in debug builds). Use `--qty`.
- Deflaked env-var tests (config + credentials) by serializing them with a lock
- Version integration test no longer hardcodes the version number
- Wired the previously-orphaned `tests/unit/` tree into a compiled test target

## [0.7.0] - 2026-03-05

### Added
- **Dashboard MVP** (#157)
  - Complete dashboard redesign with comfy-table formatting
  - Real-time order book depth display
  - Recent trades panel showing BUY/SELL activity
  - Enhanced account balance formatting with local timezone
  - Watch mode with graceful exit handling (Ctrl+C)
  - Instant refresh: fetch data before clearing screen
  - Dashboard title includes version number
- **Automated Pre-release Workflow** (#167)
  - Push tag to auto-create Pre-release
  - Multi-platform binary builds (macOS ARM64, Linux x86_64/ARM64)
  - Automatic checksum generation

### Changed
- **Dashboard Output Structure**
  - Reorganized display sections for improved clarity
  - Enhanced order display formatting
  - Better refresh label formatting
  - Cleaner table alignment
- **CI/CD Improvements**
  - Auto-prerelease for RC/Beta/Alpha versions
  - Homebrew update only for stable releases

### Fixed
- **Dashboard Data Flow**
  - Improved dashboard and portfolio command handling
  - Enhanced trade handling and output formatting
  - Removed duplicate tests module in output.rs

## [0.7.0-rc.1] - 2026-03-04

### Added
- **Dashboard MVP** (#157)
  - Complete dashboard redesign with comfy-table formatting
  - Real-time order book depth display
  - Recent trades panel showing BUY/SELL activity
  - Enhanced account balance formatting with local timezone
  - Watch mode with graceful exit handling (Ctrl+C)
  - Instant refresh: fetch data before clearing screen
  - Dashboard title includes version number

### Changed
- **Dashboard Output Structure**
  - Reorganized display sections for improved clarity
  - Enhanced order display formatting
  - Better refresh label formatting
  - Cleaner table alignment

### Fixed
- **Dashboard Data Flow**
  - Improved dashboard and portfolio command handling
  - Enhanced trade handling and output formatting
  - Removed duplicate tests module in output.rs

## [0.6.3-rc.3] - 2026-03-03

### Fixed
- **Market Trades API Decoding** (#143)
  - Resolve trades API response decoding error
  - Fix trade history data parsing issues
- **Market Depth Table Alignment** (#144)
  - Fix output table formatting alignment
  - Improve depth display readability
- **Zero Quantity Positions** (#140)
  - Filter out zero-quantity positions from display
  - Cleaner portfolio view
- **Quiet Mode Flag** (#141)
  - Properly handle `-q` (quiet) flag
  - Suppress non-essential output when quiet mode is enabled
- **Test Environment** (#142)
  - Resolve test_from_env failure in CI
  - Improve test stability

## [0.6.3-rc.2] - 2026-03-03

### Added
- **Command Short Aliases** (#137)
  - Add short aliases for common commands (e.g., `s` for `snapshot`, `w` for `watch`)
  - Improve CLI usability and efficiency

### Fixed
- **Kline Timestamp Format** (#129)
  - Format timestamp to human-readable time
  - Improve readability of kline/candlestick data
- **Depth Spread Display** (#138)
  - Show spread in both dollar amount and percentage
  - Better market depth visualization
- **WebSocket Debug Logs** (#139)
  - Ensure debug logs only show with verbose flag
  - Clean up watch mode output

## [0.6.3-rc.1] - 2026-03-02

### Fixed
- **Auth Non-TTY Support** (#127)
  - Support non-TTY environments for login
  - Fix authentication issues in CI/automated environments
- **Dashboard+Portfolio Auth Handling** (#125)
  - Properly handle AuthRequired error for anonymous mode
  - Improve error messages for unauthenticated users

## [0.6.2] - 2026-03-01

### Fixed
- **Trade Model Field Mapping** (#113)
  - Correct Trade model field mapping for proper decoding
  - Fix trade history display issues

### Documentation
- **README Portfolio Command** (#115)
  - Add Portfolio command documentation to README
  - Include usage examples and options

## [0.6.1] - 2026-03-01

### Added
- **Dashboard Anonymous Mode** (#108)
  - Show login prompt when user is not authenticated
  - Support anonymous browsing of market data
- **Portfolio Base Functionality** (#106)
  - Add `portfolio` command with `snapshot` subcommand
  - Portfolio summary and performance view framework

### Fixed
- **Duplicate Portfolio Command** (#110)
  - Remove duplicate `Portfolio` enum variant in `Commands`
  - Fix merge conflict residue from PR #106
- **Dashboard Duplicate Call** (#109)
  - Avoid calling `get_balance()` twice in dashboard
  - Optimize data fetching logic

## [0.6.0] - 2026-03-01

### Added
- **Dashboard Command** (#35, #75, #83, #84, #100, #101)
  - Real-time trading dashboard with auto-refresh (`--watch`)
  - Symbol filtering (`--symbols`)
  - Table output formatting with color coding
  - Position, order, and market data in one view
- **Portfolio Command Base** (#105, #106)
  - Portfolio snapshot infrastructure
  - Framework for portfolio PnL analysis

### Fixed
- **Dashboard Symbol Filter** (#101)
  - Simplified symbol filter logic with `has_filter` variable
  - Changed `Ordering::SeqCst` to `Ordering::Relaxed` for AtomicBool

## [0.5.0] - 2026-03-01

### Added
- **Phase 3 Integration Tests** (#61, #62)
  - CLI command integration tests using `assert_cmd`
  - API flow tests with mock servers (`mockito`)
  - Output format tests (JSON, Table, CSV, Quiet)
  - Market data command tests
- **Phase 4 E2E Tests** (#32)
  - New user journey test suite
  - Trader daily workflow test suite
  - Automated end-to-end testing framework
- **Config Testability** (#66)
  - Added `load_from_path` for better testability
  - Environment variable override tests

### Fixed
- **E2E Test Parameter Format** (380bd8c)
  - Fixed market ticker command to use positional arg instead of `--symbol`

### Changed
- **Test Dependencies**
  - Added `tokio-test`, `mockito`, `tempfile`, `assert_cmd`, `predicates`
  - Improved test coverage and reliability

## [0.4.2] - 2026-02-26

### Fixed
- Position model updated (PR #24)
- Splash screen version (PR #23)

## [0.4.0] - 2026-02-26

### Added
- Telemetry module (PR #19)
- Improved authentication flow
- Splash screen improvements

## [0.3.6] - 2026-02-26

### Documentation
- Improved README authentication section

## [0.3.5] - 2026-02-26

### Changed
- OpenClaw Skill improvements
- Fixed GitHub Release binary upload in CI workflow
