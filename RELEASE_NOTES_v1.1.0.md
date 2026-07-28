## StandX CLI v1.1.0

Maker safety-track tier 2 (stage 5-b), complete net-PnL attribution, and the
observability an unattended multi-day run needs.

**No default live behavior changes.** Every new capability is either telemetry
or off by default; the frozen production maker config runs unmodified.

### Maker stage 5-b — graded exit policy and residual handoff

The code-level prerequisite for scaling a maker session up (larger size /
`max_position`, or multiple symbols).

- **Typed exit policies.** `ExitKind::{InventoryTrim, WindDown}` separates a
  threshold-driven inventory trim from a supervisor wind-down all the way
  through plan → execution → telemetry. Evidence no longer has to infer where
  an exit came from; `inventory_exit_submitted` carries `exit_kind`.
- **Exit suppression is observable.** `cycle_summary` gains `exit_kind`,
  `exit_submitted` and `exit_suppressed` (`volatility_halt` /
  `market_data_inactive`). A volatility halt suppresses **both** exit kinds, and
  there is deliberately no emergency-exit-during-halt policy — sending a
  reduce-only market order at the moment our price information is least
  trustworthy is not a safety feature.
- **Residual position handoff on every exit path.** The maker never
  auto-flattens, so a shutdown owes the operator one authoritative number.
  Decided *after* cleanup against a venue REST snapshot, with
  `action:"residual_position"` and `event: flat | handoff | unknown`. Flat is
  only believed when two snapshots separated by a settlement delay agree — one
  snapshot can read zero simply because a cancel-race fill has not propagated,
  and a false `flat` is the one outcome that notifies nobody. A missing
  snapshot, a non-finite position or a venue/ledger disagreement is reported as
  `unknown` (critical).
- **Account-level hard floors, default off.** `--stop-equity-below` /
  `--stop-margin-below` (also TOML) stop the session through a separate typed
  outcome (`RuntimeStopReason::AccountFloor`, `action:"account_floor"`),
  distinct from the strategy's `--stop-loss`: one says "this strategy is
  losing", the other "this account can no longer be traded safely". Evaluated
  inside the cycle **before any order work**, so a breached snapshot cannot add
  exposure in the cycle that observed it. With a floor armed, a stale or
  unreadable balance fails closed rather than reading as "no breach".
- `AlertMonitor::with_account_floors` is now `with_account_alerts`: "floor"
  means only the hard brake, never an alert threshold.

### Funding cashflow in net-PnL attribution

`funding_quote` / `funding_available` / `net_pnl_complete` finally carry real
values. `StandXClient::get_funding_history` wraps the authenticated
`GET /api/query_funding_history`, and the maker folds it into the performance
ledger on the 30-second REST audit.

- **Authoritative, not derived.** The venue's signed `qty` *is* the cashflow
  (negative paid / positive received). The public funding-rate history returns
  empty on this venue, so a rate-based reconstruction was never an option.
- Dedup is by row id, not by request cursor: `last_id` pages *backward* into
  history, so every audit re-reads the same recent page.
- A row that cannot be folded in (settlement asset that is neither the quote nor
  its D-prefixed form, or an out-of-order arrival) is counted in
  `funding_unattributed`; a failed fetch or a page returned at the request limit
  sets `funding_coverage_gap`. Both clear `net_pnl_complete` instead of letting
  the summary claim a completeness the numbers do not have.
- A funding failure never fails the account audit: that same audit backs
  position reconciliation and recovery, and letting a telemetry endpoint break
  safety reconciliation would be a severity inversion.

Measured scale on HYPE, which is why this matters: hourly settlement, 91 rows
over 137 hours summing to -0.006252 DUSD (-0.0011/24h) against a ~36h baseline
net PnL of +0.006 — roughly 10–30% of the reading, in the same direction.

### Unattended observability

- **Second OpenObserve alert rule** (`standx_maker_critical_risk`) fires on any
  `severity='critical'` row: stop-loss, account floor, accounting invariant,
  cleanup residual orders, residual-position handoff. The pre-existing deadman
  only covers "the process died"; until now "the process is alive and something
  went wrong" reached an operator solely through the maker's own webhook POST,
  which is not retried. `scripts/openobserve_alerts.py` provisions both.
- **`action:"market_data_standby"`** carries `fault_class` / `paused_secs` /
  `quoteable_streak` / `divergence_bps` and friends as fields. The same facts
  were previously only inside a risk notification's human message, so standby
  duration could not be counted without parsing a sentence.

### Fixed

- `standx auth status` reported expired tokens incorrectly; `auth login` now
  validates its inputs and shows credential source and trading availability.

### Versioning note

`version.json` and `crates/standx-cli/Cargo.toml` are realigned with the release
tag in this version. Both still read `0.8.0` through the v1.0.0 release because
the release pipeline takes its version from the git tag — so the shipped v1.0.0
binary reported `standx 0.8.0`. From v1.1.0 on, `standx --version` matches the
release tag.

v1.0.0 also shipped without a changelog section; `CHANGELOG.md` now records its
contents retroactively under `[1.0.0] - 2026-07-23`.

### Compatibility

- JSON output is additive only: no existing `action` name or field changed, so
  existing consumers (`scripts/` run-manifest validation, OpenObserve
  dashboards) keep working unchanged.
- All new CLI flags default to off. The frozen production maker configuration
  needs no edits.
