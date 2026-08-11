# Maker Stage 2 v0 live canary and A/B runbook

> **Current operational base, not a strategy verdict.** The repository still uses the
> stage2-named orchestrator and deployment assets, so this runbook remains in the main docs.
> Every live run must record a fresh authorization against its exact commit, binary/config
> hashes, symbol, exposure and time window. Earlier authorization records do not carry forward.

This runbook merges the renewed live gate with Stage 2 rollout. It does not
record a pass by itself. The named online operator is **the release owner**. Live work
must not begin until the release record contains this exact authorization:

> 授权执行 XAG-USD size=0.01 max_position=0.2 的阶段2 canary 与2小时A/B

That text authorizes only XAG-USD, `size=0.01`, one level and
`max_position=0.2`. It does not authorize another symbol, larger exposure,
active inventory exit or automatic flatten. It must be recorded anew for each authorized run;
its presence in this runbook is a scope template, not standing permission.

## Frozen artifacts and preflight

- Install one release binary built from the recorded commit at
  `/opt/standx/bin/standx` and record its SHA-256.
- Install `maker-stage2-xag-baseline.toml` and
  `maker-stage2-xag-candidate.toml`; verify the files differ only at
  `adaptive_spread.enabled` and record both hashes. (Widening-control
  exercises instead use a pair differing only at top-level + base-tier
  `spread_bps` with `adaptive_spread` disabled in both — the maker requires
  base tier == top level even when disabled, and the orchestrator preflight
  rejects incoherent pairs. The orchestrator check accepts exactly these two
  shapes. Resume after a completed baseline arm uses
  `STANDX_STAGE2_FIRST_ARM=candidate`. See
  `docs/evidence/maker-stage4-wide12-ab-2026-07-18.md`.)
- Install `standx-maker-stage2-ab.service`, `run_maker_stage2_ab.sh`, the
  observed-run/OpenObserve tools, and a root-owned `0600`
  `/etc/standx/maker-stage2-ab.env`. A containerized alternative to this unit
  is available at [`deploy/docker/`](../deploy/docker/README.md)
  (docker-compose, same authorization gate); its lock paths default to a
  container-local directory rather than `/run/lock` — see that README before
  using it.
- Keep both ordinary-maker and A/B environment files on the same
  `/run/lock/standx-maker-live.lock` and
  `/run/lock/standx-maker-stage2-ab.lock` paths. A manual live process that
  cannot open those production locks must fail closed, not select another lock.
  (This host-shared-lock requirement is specific to the systemd deployment;
  the docker deployment's container-local locks trade away this guarantee —
  see its README.)
- Read current XAG symbol info and fill the three
  `STANDX_BASELINE_*` metadata values in that environment file. Blank or stale
  metadata is a gate failure; the orchestrator refuses to switch arms when
  manifest validation fails.
- Record passing workspace tests, strict Clippy, fmt, replay output equality
  across three runs, manifest/JSON/config tests, and a candidate paper run of
  at least 30 minutes.
- Keep both `standx-maker.service` and the A/B unit stopped during the canary.
  The live-process locks must reject a concurrent manual or supervised maker.

Configure OpenObserve live upload, catch-up and the external deadman alert.
The catch-up unit optionally loads `/etc/standx/maker-stage2-ab.env` after the
ordinary maker environment, so its log directory and OpenObserve credentials
must match the frozen A/B deployment.
Then send the four marked webhook probes and confirm all four messages in the
same receiver:

```bash
cd /opt/standx
python3 scripts/test_maker_stage2_webhooks.py
```

Record the common `test_id`, send timestamps and human receipt confirmation.
A successful HTTP response without receiver confirmation is not a pass.

## Emergency procedure

At least the release owner must have authenticated venue access throughout the first 30
minutes. On any unknown fill, residual maker order, failed reconnect/reconcile,
missing webhook or non-zero terminal position:

1. Stop the maker or A/B unit; do not restart or switch arms.
2. Cancel all XAG orders and independently query both resources:

   ```bash
   /opt/standx/bin/standx order cancel-all XAG-USD
   /opt/standx/bin/standx --output json account orders --symbol XAG-USD
   /opt/standx/bin/standx --output json account positions --symbol XAG-USD
   ```

3. If a residual position remains after stop-loss or cleanup, the release owner reads
   its exact side and quantity, submits one manually reviewed opposite-side
   `--reduce-only` order, and rechecks orders and positions. Never infer the
   quantity from configured `max_position`; never submit an automated flatten.
   Choose exactly one command after the side and `<EXACT_QTY>` have been
   independently reviewed:

   ```bash
   # Residual long only:
   /opt/standx/bin/standx order new XAG-USD sell market --qty <EXACT_QTY> --reduce-only
   # Residual short only:
   /opt/standx/bin/standx order new XAG-USD buy market --qty <EXACT_QTY> --reduce-only
   ```

4. Preserve logs, request/order/trade IDs and webhook evidence. Mark the run
   failed. A new exact authorization is required before retrying the canary.

## Bounded canary

After confirming `orders=[]` and `positions=[]`, execute the venue-minimum
`ws-command-canary` and retain its full create/cancel correlation chain. Then
start the final candidate config with a single controlled order-response
disconnect at 15 seconds. `--controlled-disconnect-after` is a fail-safe
shutdown drill: it does **not** reconnect or resume quoting. It forces the
fail-closed path so the gate can confirm the maker freezes, cancels only its
own orders, and shuts down cleanly on an order-response fault. Exercising the
reconnect/reconcile/resume path is left to an organic disconnect, not this
flag.

```bash
cd /opt/standx
export STANDX_ENABLE_LIVE_MAKER=1
bin/standx --output json maker ws-command-canary XAG-USD

export STANDX_ENABLE_LIVE_MAKER=1
export STANDX_RUN_ID="stage2-canary-$(date -u +%Y%m%dT%H%M%SZ)"
scripts/run_maker_observed.sh bin/standx --output json maker run XAG-USD \
  --maker-config examples/maker-stage2-xag-candidate.toml --live \
  --controlled-disconnect-after 15
```

Verify the sequence `order-response fault observed → frozen → maker
cleanup/empty book → fail-safe shutdown`. The run stops with a critical
`fail_safe` risk notification and a non-zero exit; that non-zero exit is the
expected drill outcome, not a failure. Then validate its manifest and
independently require `orders=[]` and `positions=[]`. Any deviation from this
sequence — residual maker order, non-zero terminal position, or failed cleanup
— invokes the emergency procedure.

Because this drill always fails safe, do not pass `--controlled-disconnect-after`
to the automatic A/B orchestrator: it treats an arm that exits before its
scheduled window as a critical stop.

## Two-hour automatic A/B

Only after the canary evidence is accepted:

```bash
sudo systemctl enable --now standx-openobserve-catchup.service
sudo systemctl start standx-maker-stage2-ab.service
journalctl -u standx-maker-stage2-ab.service -f
```

The orchestrator alternates baseline then candidate. Each arm has a unique
`run_id/config_hash`, runs for a two-hour **minimum**, and switches only from a
flat position after normal maker cleanup, manifest validation, and independent
empty-order/empty-position checks. At the two-hour mark the orchestrator
signals the arm with `SIGUSR1`, latching maker **wind-down**: the maker stops
quoting for good and flattens any residual position via reduce-only market
exits (bounded, deterministic cost — spread plus taker fee on the residual),
so the arm should reach flat within a few cycles. A warning webhook fires
every 30 minutes while an arm is not yet flat past two hours, and the signal
is re-sent with each warning. Only an arm that stays non-flat past the hard
cap (`STANDX_STAGE2_ARM_MAX_SECONDS`, default six hours from arm start — i.e.
a stalled wind-down) is invalidated with a critical webhook and exit 75 —
never with an automatic flatten beyond the reduce-only exits. Fail-safe exit,
invalid manifest, failed post-check, a failed venue position query, or a maker
that dies mid-wait also block the next arm. Arms now run close to two hours
plus a short wind-down, but acceptance still balances **quote-hours** per
regime rather than assuming equal wall-clock.

Deployment constraint: the orchestrator, the observed wrapper, and the maker
binary must ship in the **same image**. `SIGUSR1` sent to a binary built
before wind-down support terminates it (default disposition), so never pair a
new orchestrator with an older maker image.

Keep the Stage 2 state `live_ab` until valid, market-matched quote-hours include
at least one calm and one trend window. Acceptance requires net PnL at least
95% of baseline, no worse maximum drawdown, at least 10% improvement in
negative 5-second markout, uptime loss no more than 3 percentage points, and
no more than 20% cancel growth per quote-hour.
