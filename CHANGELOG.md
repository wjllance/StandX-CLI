# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed
- **Maker cleanup decides "residual" per order, not from the open-orders list.** After `cancel_orders`, each maker order is polled through `/api/query_order` until it reaches a terminal status (`filled` / `canceled` / `rejected` / `expired`) — 500ms, then 1s intervals, up to 6 attempts. `/api/query_open_orders` can lag ~15s behind a successful cancel, and on 2026-07-29 that lag alone truncated a 35.9h baseline run with a false fail-safe; per-order status is not subject to it. A non-terminal status is still residual and still fails closed, and the `maker_cleanup` JSON event now carries each order's observed `status` and `updated_at`
  - The book is still re-read once after every order is confirmed terminal, and any maker order **outside the cancel batch** fails the pass closed: an order the venue accepted just before cleanup can become visible only after the initial snapshot, so it was never cancelled. Orders inside the batch are ignored by that re-read — absorbing the list lag is the point of the per-order query
  - The reconnect snapshot check (`validate_reconnect_snapshot`) confirms the same way. A maker order the list still shows as open is only a candidate until `/api/query_order` says it is live; otherwise the false positive would simply move from cleanup to the reconnect path, which fails closed on the same stale read
- **Documentation positioning: agent-first, but OpenClaw is no longer privileged.** The README tagline is now "Trade by Intent. Built for Agents, Not Buttons."; OpenClaw sits alongside Claude / Cursor / LangChain / AutoGPT instead of ahead of them. The `--openclaw` flag, `STANDX_OPENCLAW_MODE`, and the `openclaw/` skill package are unchanged — only their framing is. `standx --help` and the `standx-cli` crate description no longer read "OpenClaw-first"
- **README claims that did not match the code are fixed.** `--confirm` / `--no-confirm` never existed and are gone (`--yes` is documented as what it actually gates today: `standx update`'s confirmation, and nothing else — order/leverage/margin have no prompt to skip); "Rate limiting — built-in protection" is replaced by what actually ships (transport failures and HTTP 429 classified as retryable with `RateLimitExceeded { retry_after }`, and no client-side throttle); `--dry-run` is described as the category-level check it is rather than an order simulator; the workspace is described as three crates, removing the last `standx_sdk::maker` reference in the repo

### Added
- `crates/standx-sdk/README.md` — standalone crate README: module tour, REST method table, `tabled` feature, and the presentation-free contract (opt-in WebSocket debug tracing goes to stderr). The root README gains a `Rust SDK` section so the library has an entry in the table of contents
- `crates/standx-maker/README.md` — a pointer document for the crate boundary (no I/O, deterministic, `standx-sdk` only), with a coarse module map and links to `docs/13-maker.md`, `docs/14-maker-live-gate.md`, and `AGENTS.md`. Deliberately carries no flags, config keys, or PnL figures, which live in `docs/`

## [1.2.0] - 2026-07-28

Self-update. `standx --version` now also matches its release tag as a rule rather
than by accident (see the v1.1.0 versioning note).

### Added
- **`standx update`** (alias `self-update`) — replace the running binary with the
  latest GitHub release. `--check` reports installed vs latest and changes
  nothing; `--pre` considers pre-releases; `--force` reinstalls the current
  version. Confirmation uses the existing global `--yes` /
  `STANDX_AUTO_CONFIRM=true` (required when stdin is not a TTY) rather than a
  second copy of that flag. `-o json` emits `update_check` / `update_applied`
  - The release asset for the running platform is downloaded over TLS, its
    SHA-256 verified against the published `checksums.txt`, and the unpacked
    binary asked for its own `--version` to confirm it matches the release before
    an atomic rename over the running executable. Any failure leaves the existing
    binary untouched
  - Checksum verification covers corruption and truncation, **not** provenance:
    the checksum ships from the same place as the archive. A detached signature
    would be the next step and is not implemented. Because of that, the one place
    the downloaded binary is executed (the `--version` probe) runs with
    `env_clear()` and a minimal allow-list, so a hostile release cannot read this
    process's `STANDX_JWT` / `STANDX_PRIVATE_KEY` / `GITHUB_TOKEN` on the way in
  - A Homebrew-managed install is refused with a pointer to `brew upgrade
    standx-cli`, so the formula and the installed binary cannot silently diverge.
    An unwritable install directory is an error with instructions — the command
    never elevates privileges
  - Stable checks resolve the latest tag through the `releases/latest` redirect
    rather than the REST API, so they are not subject to the 60-per-hour
    unauthenticated API limit. Only `--pre` needs the API, and it names the rate
    limit explicitly (and honours `GITHUB_TOKEN`) instead of surfacing a bare 403
  - Known gaps, tracked rather than hidden: provenance verification is not
    implemented (#336), `--force` still permits a silent downgrade and `--pre`
    can select an unpublished draft (#337)

### Fixed
- `standx <subcommand> --help` no longer risks a clap duplicate-option panic in
  debug builds for the update command: `--yes` exists only as the global flag, and
  a test now runs clap's `debug_assert()` over the update subtree. Release builds
  compile those assertions out, which is exactly how a duplicated `--yes` survived
  manual smoke testing
  - Unrelated and still open: `standx block list --help` panics in debug builds
    because `-s` is claimed by both `--symbol` and `--status`. Fixing it changes a
    published short flag, so it is its own change — which is why the new assertion
    is scoped to the update subtree instead of the whole command tree

### Fixed
- `block list`: removed the `-s` short flag from `--status` (it collided with `--symbol`, and clap panics on the collision in debug builds — `standx block list --help` aborted for anyone not on a release build). `-s` keeps its meaning of `--symbol`, matching `block watch` and every other command; use the long `--status` for the status filter
- The clap structural check now covers the whole command tree (`Cli::command().debug_assert()`) instead of only the `update` subtree, so a duplicate short flag anywhere fails in CI rather than at a user's `--help`

## [1.1.0] - 2026-07-28

Maker safety-track tier 2 (stage 5-b), complete net-PnL attribution, and the
observability an unattended multi-day run needs. No default live behavior
changes: every new capability is either telemetry or off by default.

### Added
- **Maker stage 5-b (safety track, tier 2): graded exit policy and residual handoff** — the code-level prerequisite for scaling up. See `docs/26-maker-stage5b-design.md`
  - Typed exit policies: `ExitKind::{InventoryTrim, WindDown}` distinguishes a threshold-driven inventory trim from a supervisor wind-down all the way through plan → execution → telemetry, so evidence no longer has to infer an exit's origin. `inventory_exit_submitted` carries `exit_kind`
  - Exit suppression is observable: `CyclePlan::exit_suppression` reports `volatility_halt` / `market_data_inactive` as a typed value, surfaced as the additive `cycle_summary` fields `exit_kind` / `exit_submitted` / `exit_suppressed`. A volatility halt suppresses **both** exit kinds and there is deliberately no emergency-exit-during-halt policy (design decision D1)
  - Residual position handoff on every exit path, decided **after** cleanup against a venue REST snapshot (an order can fill while it is being cancelled, and the account stream is gone by then): `action:"residual_position"` with `event: flat | handoff | unknown`, plus `venue_position` / `ledger_position` / `unknown_reason` / `needs_operator`. A missing snapshot, a non-finite position, or a venue/ledger disagreement is reported as `unknown` (critical) — "cannot confirm flat" is never rendered as flat. Also a critical `kind:"residual_position"` webhook and the residual in the `🔴 maker stopped` lifecycle message. The maker never auto-flattens
  - Account-level hard floors, default off: `--stop-equity-below` / `--stop-margin-below` (also TOML `stop_equity_below` / `stop_margin_below`) stop the session through the separate `RuntimeStopReason::AccountFloor` / `action:"account_floor"`, distinct from the strategy's `--stop-loss`. Evaluated inside the cycle **before any order work**, so a breached snapshot cannot add exposure in the cycle that observed it. Equity is checked before margin. With a floor armed, a balance older than 35s or an unparseable field the floor actually reads fails closed (`event:"unevaluable"`) instead of reading as "no breach", and an armed floor now counts as watching account risk for balance-refresh purposes; disarmed floors (the default) can never stop a run
  - Flat is only believed after two venue snapshots separated by a settlement delay: one snapshot can read zero simply because a cancel-race fill has not propagated, and a false `flat` is the one outcome that notifies nobody
  - Hard-floor balance freshness is timestamped after the audit/balance join completes, not before it: dating a refresh by when its request was issued under-reports the age, which is the direction that would let an armed floor accept a too-old snapshot
  - `AlertMonitor::with_account_floors` renamed to `with_account_alerts` (with `equity_alert_below` / `margin_alert_below` fields): "floor" now means only the hard brake, never an alert threshold
- **Funding cashflow in maker net-PnL attribution** — `StandXClient::get_funding_history` wraps the authenticated `GET /api/query_funding_history`, and the maker folds it into the performance ledger on the 30s REST audit, so `funding_quote` / `funding_available` / `net_pnl_complete` finally carry real values instead of always reporting an incomplete attribution
  - Authoritative, not derived: the venue's signed `qty` is the cashflow (negative paid / positive received), matching the ledger's existing convention. The public `query_funding_rates` history returns empty on this venue, so a rate-based reconstruction was never an option
  - Dedup is by row id, not by request cursor: `last_id` pages *backward* into history, so every audit re-reads the same recent page and `record_funding` (which only accumulates) would otherwise double-count
  - Rows that exist but cannot be folded in — a settlement asset that is neither the quote nor its D-prefixed form, or an out-of-order arrival — are counted in the new `funding_unattributed` and clear `net_pnl_complete` rather than being silently dropped. An out-of-order row never stops the maker: funding is attribution, not position accounting
  - A funding fetch failure or a page returned at the request limit sets `funding_coverage_gap`, which also clears `net_pnl_complete`. The funding request shares the account audit for concurrency but its failure is **not** propagated: that same audit backs position reconciliation and recovery, so letting a telemetry endpoint fail it would be a severity inversion
  - Measured scale on HYPE (why it matters): hourly settlement, 91 rows over 137h summing to -0.006252 DUSD (-0.0011/24h) against a ~36h baseline net PnL of +0.006 — roughly 10–30% of the reading, in the same direction
- **Unattended observability**
  - Second OpenObserve alert rule, `standx_maker_critical_risk`: fires on any `severity='critical'` row (stop-loss, account floor, accounting invariant, cleanup residual orders, residual-position handoff). The pre-existing deadman only covers "the process died"; until now, "the process is alive and something went wrong" reached an operator solely through the maker's own webhook POST, which is not retried. `scripts/openobserve_alerts.py` provisions both
  - New `action:"market_data_standby"` event carrying `fault_class` / `paused_secs` / `quoteable_streak` / `snapshots_required` / `divergence_bps` / `threshold_bps` / `maker_book_empty`. The same facts were previously only inside a risk notification's human `message`, so standby duration could not be counted without parsing a sentence
- New frozen maker baseline configs: `examples/maker-guard-hype-{baseline,candidate}.toml` (nonlinear skew + external-price guard, the accepted stage 3 production baseline) and `examples/maker-stage3v1-hype-skewonly.toml`

### Changed
- Maker docker deployment forces env-only auth (`STANDX_JWT` / `STANDX_PRIVATE_KEY`); the `credentials.enc` mount is gone. Refresh a token by editing the env file and recreating the container
- `version.json` and `crates/standx-cli/Cargo.toml` are realigned with the released version. Both still read `0.8.0` through the v1.0.0 release because the release pipeline takes its version from the git tag, so the shipped v1.0.0 binary reported `standx 0.8.0`

### Fixed
- `standx auth status` reported expired tokens incorrectly; `auth login` now validates its inputs and shows credential source and trading availability

## [1.0.0] - 2026-07-23

First release carrying the maker bot and the extracted SDK crate. Released from
tag `v1.0.0` (commit `45311e7`) without a changelog section at the time; the
entries below are that release's content, recorded retroactively.

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
- `TimeInForce::Alo` (post-only / add-liquidity-only), matching the backend enum; `standx order create --tif ALO` now supported
- Block trade commands: `standx block list` / `standx block watch`
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
