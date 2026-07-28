# Agent Execution Guide

This file defines the repository-wide execution rules for coding agents. Human Git workflow remains documented in `WORKFLOW.md`.

## Source of Truth

- `standx-maker` owns deterministic market-making strategy, risk, accounting, and state-transition logic.
- `standx-sdk` owns exchange protocols, authentication, API models, HTTP/WebSocket clients, and transport health.
- `standx-cli` owns command-line configuration, live gating, asynchronous I/O orchestration, exchange reads and writes, telemetry, and user-facing output.
- Preserve this direction of dependency. Strategy and safety decisions must not depend on CLI types, terminal output, webhook delivery, Tokio tasks, or network clients.

## Maker Module Boundary

Move logic into `standx-maker` when all of the following are true:

- The result is deterministic for the same typed inputs.
- The logic can run in a replay or unit test without network, clock, environment, filesystem, or terminal access.
- The logic expresses strategy, risk, order ownership, session accounting, or maker runtime state transitions.
- Failures can be represented as typed results or domain errors rather than printed messages.

Typical `standx-maker` responsibilities include:

- Quote planning, skew, exposure caps, volatility breakers, and inventory-exit decisions.
- Current-run order ownership and bounded client-order-ID construction/parsing.
- WS/REST fill deduplication, partial-fill accounting, session PnL, and expected position.
- Position-limit and quantity-tolerance rules.
- Position-jump, threshold-crossing, and direction-flip detection.
- A pure event reducer that maps maker events to typed effects, including generation invalidation, freeze, cleanup, reconnect, recovery, and stop decisions.

Keep logic in `standx-cli` when it performs or directly coordinates side effects:

- REST/WebSocket connection, authentication, snapshots, subscriptions, reconnect execution, sleeps, and timeouts.
- Placing or cancelling orders and executing reduce-only exits.
- CLI/file/environment configuration merging and live authorization gates.
- JSON/terminal output, OpenObserve emission, webhook formatting, and notification delivery.
- Translating SDK payloads into normalized maker-domain inputs and executing effects returned by `standx-maker`.

Keep exchange-specific payload parsing and transport error classification in `standx-sdk` or a thin CLI adapter. Do not make core maker logic inspect loosely typed JSON or emit user-facing strings.

## Preferred Core Interfaces

- Prefer normalized domain events such as `LedgerEvent` over passing SDK `Trade` or `OrderUpdate` through the strategy core.
- Prefer typed outcomes such as `LedgerOutcome`, `PositionRiskEvent`, and `MakerEffect` over mutating unrelated CLI state.
- Reducers and planners must be pure: they return effects or intents and never execute I/O.
- The CLI runtime is an effect executor. It may translate, schedule, cancel, and report effects, but must not duplicate the safety decision inside ad-hoc branches.
- Keep account events and order-response events on one canonical ingestion path each.
- Use one authoritative current-run ledger for fills, expected position, and deduplication.

## Safety Invariants

- Account-stream loss, unexplained position mismatch, reconciliation timeout, or residual maker orders must fail closed.
- Entering a frozen state invalidates the current generation, prevents new placements, aborts cancellable in-flight work, clears queued actions, and schedules maker cleanup.
- Ignore outcomes from stale generations. Because an aborted request may already have reached the venue, cleanup is always the compensating action.
- Resume quoting only when required streams are healthy, the maker book is empty, and the venue position reconciles with the current-run ledger.
- WS and REST fills may arrive in either order. Partial, repeated, reconnected, and replayed updates must affect fills, PnL, and expected position exactly once.
- Historical `sxmk-` activity may be inspected for startup cleanup or verification but must not enter the current session ledger unless it belongs to the current run tag.
- Preserve existing JSON action names and fields unless an explicit output-contract change is requested.
- Do not change quote formulas, thresholds, PnL semantics, inventory-exit behavior, or live-gate defaults as an incidental part of a refactor.

## Refactoring Guidance

- Do not grow `standx-maker/src/lib.rs` into another monolith. Add focused modules such as `ledger`, `runtime`, `risk`, `ownership`, `planner`, `stats`, and `breaker`, then re-export the intended public API.
- Split mixed modules at the decision/effect boundary. For example, recovery policy belongs in `standx-maker`; REST queries, waits, cleanup calls, and reconnect attempts remain in `standx-cli`.
- Keep transport adapters small and explicit. Parse and validate SDK values once, then pass typed numeric/domain values to the maker crate.
- Avoid large parameter lists. Group stable inputs into context/request/state types with clear ownership.
- A refactor must retain observable trading order, failure behavior, telemetry schema, and configured policy unless the change explicitly states otherwise.
- Before working a standing quality issue, re-verify its checklist item by item against current code. These checklists go stale: items are routinely already done by later commits, and building straight from the list produces duplicate work. Evaluating an item and deliberately dropping it is a valid outcome — record the cost/benefit and the regression risk in the issue and in the commit body.

## Testing Requirements

For maker-core changes, add deterministic tests covering the relevant cases:

- WS then REST, REST then WS, duplicate fills, and partial-fill-then-cancelled orders.
- Position before order and order before position.
- External or wrong-run orders and fills.
- Generation invalidation and ignored stale outcomes.
- Freeze, cleanup, bounded recovery, residual-order stop, and reconciliation timeout.
- Quantity-tick boundaries, position jumps, direction flips, and inventory/max-position threshold crossings.

Before handing off a maker change, run:

```bash
HOME=/tmp/standx-test-home CARGO_HOME=~/.cargo cargo test --workspace --offline
cargo clippy --workspace --all-targets --offline -- -D warnings
cargo fmt --all -- --check
python3 -m py_compile scripts/openobserve_dashboard.py
```

Credential-dependent or production validation is separate from offline verification. Never place live orders, disconnect production streams, execute inventory exits, or flatten positions without explicit authorization for that specific exercise.

Changes whose correctness can only be settled by live market data — strategy parameters, mechanism toggles, quoting or exit behaviour, risk thresholds — additionally follow `docs/28-experiment-protocol.md` (pre-registered criteria, single-config-line arms, unjudged-metric register). The dividing line is whether the change can affect fill outcomes.

## Pre-Merge Verification

A green offline run (fmt, clippy, tests) is necessary, not sufficient. Before handing off or merging a branch that touches maker, safety, or live paths:

- Run one adversarial review pass with a reviewer that did not write the code — a different model or agent, prompted to find high-severity defects rather than to confirm the design. Every high-severity defect found in this area so far came from this pass, not from self-testing.
- Accept only reproducible findings. State the concrete path step by step, reproduce it in a test, then fix. Record in the commit body: the path, why it is dangerous, the fix trade-off, and — when the defect also exists on `main` rather than being introduced by the branch — that fact plus whether it is in scope.
- Verify every new safety test by mutation: deliberately break the implementation and confirm the test goes red. A test that still passes against a broken implementation has pinned nothing, and saying it was added is misleading.
- Ship new mechanisms default-off, and keep behaviour byte-equivalent to the previous release when the toggle is off, with a test pinning the equivalence.
- When a test corrects a design assumption, write the corrected reasoning into a code comment at the decision site, not only into the commit message.
