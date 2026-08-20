# Maker live gate

`STANDX_ENABLE_LIVE_MAKER=1` is an intentional release-owner gate, not an
automatic start command. It was unlocked on 2026-07-10 after the paper run,
engineering checks, and supervised production canary were recorded below.
The flag only permits a future `--live` invocation; it does not start the
maker by itself.

## Required engineering evidence

- All maker risk PRs are merged in dependency order and CI is green.
- `cargo test -p standx-sdk --offline`, `cargo test -p standx-cli --lib --offline`, and strict workspace Clippy pass on the merge commit.
- Replay scenarios cover: crossed touch, stale/crossed market data, volatility halt, position-limit pressure, delayed order visibility, stream loss, and inventory exit.
- No telemetry, logs, PR body, or alert output contains credentials or webhook secrets.

## Cleanup terminal-state rule

- For a correlated cancel, a terminal order-response `success` is the preferred proof that venue
  processing completed. A gateway `accepted` frame only proves admission and is not terminal.
- When the response stream is unavailable, closes, times out, or never reaches terminal success,
  cleanup queries that exact `order_id` through REST and accepts only a terminal venue status.
- An open-orders list may help discover candidates but cannot by itself prove that one order was
  cancelled or remains live: the venue has exhibited list read-after-write lag.
- If neither correlated WS success nor authoritative per-order REST status can settle the order,
  cleanup remains fail-closed. It must not render the maker book empty, resume quoting, or report a
  clean shutdown from an unconfirmed list snapshot.

The completed design and incident history are retained in the
[cleanup residual archive](archive/2026-07-maker/29-maker-cleanup-residual-verification.md).

## Paper and connectivity evidence

- Paper mode completes a recorded multi-hour session without panic or invariant failure.
- Order-response and authenticated account-stream authentication succeed; a forced account-stream disconnect freezes placements, cleans maker orders, and only resumes after authentication plus an empty-book/position snapshot.
- Fill ledger records only venue fills whose client-order ID carries the current run tag, enforces the session time boundary, and does not duplicate trade IDs. Historical `sxmk-` trades are ledger-sync evidence, not current-run fills.
- Existing inventory at or below `max_position` is adopted at the startup mark with maker-session PnL zeroed; inventory above the limit rejects startup after maker-order cleanup.
- A venue position change that cannot yet be explained by current-run order callbacks freezes placements immediately. WS order updates and REST snapshots are reconciled for at most three seconds; persistent mismatch triggers fail-safe cleanup and shutdown.
- Account-order cumulative fills and REST trades are tested in both arrival orders and never double-count fills, expected position, or maker-session PnL.
- Every accepted current-run fill atomically synchronizes ledger position and session telemetry position; an internal mismatch beyond half a quantity tick fails closed before further live placement or risk evaluation.
- Pending place/cancel quote slots remain reserved across arbitrarily fast strategy cycles until a correlated response, account-order terminal state, or explicit cleanup closes the venue exposure; cycle count alone never authorizes a replacement order.
- Every submitted create/cancel/inventory-exit request has a 10-second monotonic lifecycle deadline. A missing acknowledgement recovers the order-response stream; an acknowledged request that never becomes visible on the correlated account-order stream recovers the account stream. Both freeze placement and require maker cleanup plus authoritative REST reconciliation; timeout never releases a quote slot by itself.
- Request timeouts emit one structured `risk_notification(kind=order_request_timeout, event=frozen)` with request correlation, missing lifecycle phase, measured age/deadline, and selected recovery target. The terminal `order_latency` event retains the same timeout phase and measured duration.
- Order-response recovery adopts current-run filled orders and exactly-once backfills stable-ID REST trades before comparing ledger and venue positions. A bounded 0.5s/1.0s/1.5s retry window covers cancel-versus-fill visibility races; unexplained position gaps still fail closed.
- Position jumps, account-stream state changes, reconciliation, volatility breaker, inventory exit, residual cleanup, and final fail-safe emit `risk_notification` telemetry; critical shutdown/cleanup delivery is awaited with a timeout.
- Inventory-exit configuration remains disabled unless a supervised test has approved its exact threshold and chunk; the recorded XAG-USD test approves only `max_position=0.8`, trigger `25%`, and chunk `0.2`.

## Supervised canary

- A named operator is present with venue access and a documented emergency cancel procedure.
- Start with one symbol, minimum valid size, one level, and a max position no larger than one exit chunk.
- For a changed WS command path, retain one correlation chain containing the
  create `request_id` and accepted response, the REST-visible venue order ID
  and client-order ID, the cancel `request_id` and accepted response, REST
  absence, and a final position equal to the flat preflight baseline.
- Observe startup, order response, fills, cancellations, alerts, and shutdown;
  retain timestamps and order/trade IDs. The hidden `ws-command-canary` emits
  these checks as `action=ws_command_canary` JSON events in every output mode.
  Both `ws-command-canary` and `maker run --live` automatically load
  `STANDX_SUPERVISOR_WEBHOOK` and optional
  `STANDX_SUPERVISOR_WEBHOOK_FORMAT` from the process environment or a local
  ignored `.env.local` file. Process environment values override the file;
  explicit command-line options take precedence over both.
- Stop immediately on residual maker orders, uncorrelated fills, stream disconnect, unexpected position change, failed cleanup, or breached risk limit.

For the Stage 2 v0 renewed gate, the named operator, exact authorization,
four-class webhook probe, controlled-disconnect recovery sequence and A/B
handoff are specified in
[19-maker-stage2-live-ab-runbook.md](19-maker-stage2-live-ab-runbook.md).

## Unlock decision

Only a release owner may enable the environment variable after the preceding
evidence is reviewed. The explicit unlock approval was recorded on
2026-07-10. Any strategy, venue API, credential, or risk-control change
re-locks the gate and requires a new canary record.

**2026-08-20, owner: the per-change canary requirement above is suspended for
the near term.** It was judged too conservative for the current stage, where
config-level parameter changes (e.g. `band_bps`) are frequent and the standing
risk controls are unchanged. The rule is recorded as suspended rather than
deleted so practice and this document do not silently diverge; the release
owner reinstates it without a new decision. Everything else on this page —
cleanup terminal-state rules, fail-closed reconciliation, and the
`STANDX_ENABLE_LIVE_MAKER` unlock itself — still applies.

The 2026-07-14 XAG-USD correlated command canary now supplies the create/cancel
and post-check evidence required for the WS command transport change. The
successful experiment is evidence for review, not an implicit release-owner
unlock decision.

## Execution records

- [2026-07-21 Stage 3 v0 renewed gate: canary and A/B start](evidence/maker-stage3-canary-2026-07-21.md)
- [2026-07-14 WS order-command controlled canary](evidence/maker-ws-order-command-canary-2026-07-14.md)
- [2026-07-10 maker paper long run, controlled disconnect, and supervised production canary](evidence/maker-live-gate-2026-07-10.md)
