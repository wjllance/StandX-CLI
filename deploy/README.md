# StandX maker process supervision

Supervision units for the maker bot and its OpenObserve log uploader, plus a
crash/boot catch-up. The repo targets both Linux (prod) and macOS (dev), so
there are `systemd/` and `launchd/` variants with matching semantics.

Without these units the maker's foreground wrapper (`scripts/run_maker_observed.sh`)
has no supervisor: an unexpected death is neither restarted nor alerted, and a
uploader that dies mid-run only shows up as a stale dashboard. These units fix
all three gaps (see issue #223).

## Exit-code policy

The maker distinguishes an **intentional fail-safe shutdown** from an
**unexpected death** with a dedicated process exit code. Supervision keys off it:

| Exit code | Meaning | Restart? | Notify? |
|-----------|---------|----------|---------|
| `0` | Clean stop (Ctrl+C / SIGTERM) | No | No |
| `75` | Intentional maker fail-safe (order-response stream lost, 3 consecutive cycle errors, position-reconciliation failure, or residual-order cleanup failure) | **No** | **Yes** |
| `1` | Startup / config / validation error | Yes (loop-limited) | Yes |
| `101`, signal (`137` = SIGKILL, …) | Panic, OOM kill, unexpected death | **Yes** | **Yes** |

`75` is defined in Rust as `FAIL_SAFE_EXIT_CODE`
(`crates/standx-cli/src/commands/maker/model.rs`) and returned via the typed
`FailSafeShutdown` error; `main` maps that error to the code. A fail-safe is a
deliberate, safe stop that needs a human, so it must **not** auto-restart — but
it still alerts. An unexpected death is restartable.

- **systemd** enforces this directly: `RestartPreventExitStatus=75` +
  `Restart=on-failure` + `OnFailure=standx-notify@…`. The unit also sets
  `KillMode=mixed`: with the default `control-group` mode, systemd TERMs the
  wrapper's stdout-copy subshell at the same time as the maker, so the maker's
  graceful-shutdown writes hit EPIPE and panic (exit `101`) — cleanup evidence
  is lost and every normal stop pages as a failure.
- **launchd** has no per-code exclusion, so `standx-maker-supervise.sh`
  translates exit `75` to `0` (after notifying) and pairs with
  `KeepAlive.SuccessfulExit=false` (restart only on non-zero).

## What manages the uploader

`run_maker_observed.sh` co-supervises the live uploader: it polls the uploader
while the maker runs and relaunches it (with a stderr + optional webhook notice)
if it dies mid-run. Because the units run the maker *through* the wrapper, a
single maker unit covers both processes. Set `STANDX_SUPERVISOR_WEBHOOK` (and,
optionally, `STANDX_SUPERVISE_INTERVAL`, default 5s) to receive those notices.

## Crash/boot catch-up

`scripts/openobserve_catchup.sh` re-uploads any un-ingested log tail left by a
SIGKILL or power loss. It runs the ingester in incremental (non-follow) mode
with `--run-id <run_id>` per `<run_id>.ndjson`, reusing the exact checkpoint key
the live follow uploader used — so it resumes without re-sending events. It is
idempotent and best-effort (always exits 0). It runs at boot via
`standx-openobserve-catchup.service` (ordered before the maker) / the
`com.standx.openobserve-catchup` launchd agent (`RunAtLoad`), and can also be
run by hand.

---

## Linux (systemd)

Assumes an install root of `/opt/standx`. If you install elsewhere, edit the
absolute paths in the three unit files.

```sh
# 1. Config + secrets
sudo install -d -m 700 /etc/standx
sudo install -m 600 /opt/standx/deploy/systemd/maker.env.example /etc/standx/maker.env
sudoedit /etc/standx/maker.env        # set symbol, config, OpenObserve creds, webhook

# 2. Install units + helper script
sudo cp /opt/standx/deploy/systemd/standx-maker.service \
        /opt/standx/deploy/systemd/standx-openobserve-catchup.service \
        /opt/standx/deploy/systemd/standx-notify@.service \
        /etc/systemd/system/
sudo chmod +x /opt/standx/deploy/systemd/standx-notify.sh \
              /opt/standx/scripts/run_maker_observed.sh \
              /opt/standx/scripts/openobserve_catchup.sh
sudo systemctl daemon-reload

# 3. Enable
sudo systemctl enable --now standx-openobserve-catchup.service
sudo systemctl enable --now standx-maker.service

# Observe
journalctl -u standx-maker.service -f
```

Stop cleanly with `sudo systemctl stop standx-maker.service` (SIGTERM → the
maker runs its fail-safe cleanup and exits 0 → no restart).

## macOS (launchd)

Install as a per-user LaunchAgent. Edit the absolute paths in both plists first.

```sh
cp /opt/standx/deploy/launchd/com.standx.openobserve-catchup.plist \
   /opt/standx/deploy/launchd/com.standx.maker.plist \
   ~/Library/LaunchAgents/
chmod +x /opt/standx/deploy/launchd/standx-maker-supervise.sh \
         /opt/standx/scripts/run_maker_observed.sh \
         /opt/standx/scripts/openobserve_catchup.sh

launchctl load ~/Library/LaunchAgents/com.standx.openobserve-catchup.plist
launchctl load ~/Library/LaunchAgents/com.standx.maker.plist

# Logs
tail -f /opt/standx/var/standx/launchd-maker.err.log
# Stop / remove
launchctl unload ~/Library/LaunchAgents/com.standx.maker.plist
```

Provide OpenObserve credentials and the optional `STANDX_SUPERVISOR_WEBHOOK`
via `deploy/openobserve/.env` (pointed at by `STANDX_ENV_FILE`).

## Live trading

All units default to **paper**. Live trading requires BOTH `--live` in
`STANDX_MAKER_ARGS` (systemd) / the plist `ProgramArguments`, AND
`STANDX_ENABLE_LIVE_MAKER=1`. Read `docs/14-maker-live-gate.md` first. Setting
the env var alone changes nothing.

Stage 2 uses the separate `standx-maker-stage2-ab.service` and a root-owned
`0600` `/etc/standx/maker-stage2-ab.env`. The unit conflicts with the normal
maker service, installs the deadman alert before start, and runs the guarded
baseline/candidate orchestrator. See
`docs/19-maker-stage2-live-ab-runbook.md`; do not start it without the exact
authorization recorded there.

A containerized equivalent of the Stage 2 A/B orchestrator is available at
[`deploy/docker/`](docker/README.md) (docker-compose, gated behind the `ab`
profile). Same runbook and authorization gate apply; see that README for the
systemd→compose mapping and the safety semantics that differ (no
auto-restart, container-local locks by default).
