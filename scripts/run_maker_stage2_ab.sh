#!/usr/bin/env bash
set -uo pipefail

root="${STANDX_INSTALL_ROOT:-/opt/standx}"
standx_bin="${STANDX_BIN:-$root/bin/standx}"
baseline_config="${STANDX_STAGE2_BASELINE_CONFIG:-$root/examples/maker-stage2-xag-baseline.toml}"
candidate_config="${STANDX_STAGE2_CANDIDATE_CONFIG:-$root/examples/maker-stage2-xag-candidate.toml}"
symbol="${STANDX_SYMBOL:-XAG-USD}"
arm_seconds="${STANDX_STAGE2_ARM_SECONDS:-7200}"
# Minimum arm length is arm_seconds. At that mark the orchestrator signals the
# arm with SIGUSR1, latching maker wind-down: the maker stops quoting for good
# and flattens any residual position via reduce-only market exits (bounded,
# deterministic cost) instead of waiting for flow-dependent natural flat. The
# poll below then simply confirms the venue is flat; a maker that stays
# non-flat past arm_max_seconds (the hard cap, counted from arm start and
# inclusive of arm_seconds) still escalates to critical_stop. A warning fires
# every flat_grace_seconds while the arm is not yet flat so a stalled
# wind-down stays visible.
arm_max_seconds="${STANDX_STAGE2_ARM_MAX_SECONDS:-21600}"
flat_grace_seconds=1800
poll_seconds="${STANDX_STAGE2_POLL_SECONDS:-15}"
log_dir="${STANDX_LOG_DIR:-$root/var/standx}"
ab_lock="${STANDX_STAGE2_AB_LOCK_PATH:-/run/lock/standx-maker-stage2-ab.lock}"

notify() {
  local message="$1"
  local severity="${2:-info}"
  printf '%s\n' "$message" >&2
  if [[ -n "${STANDX_SUPERVISOR_WEBHOOK:-}" ]]; then
    STANDX_STAGE2_NOTIFICATION="$message" \
      STANDX_STAGE2_NOTIFICATION_SEVERITY="$severity" \
      python3 - <<'PY' || printf 'stage2 supervisor webhook post failed\n' >&2
import json
import os
import urllib.request

url = os.environ["STANDX_SUPERVISOR_WEBHOOK"]
text = os.environ["STANDX_STAGE2_NOTIFICATION"]
severity = os.environ["STANDX_STAGE2_NOTIFICATION_SEVERITY"]
webhook_format = os.environ.get("STANDX_SUPERVISOR_WEBHOOK_FORMAT", "slack").lower()
raw = {
    "text": text,
    "action": "stage2_ab_supervisor",
    "severity": severity,
}
if webhook_format in {"slack", "telegram"}:
    payload = {"text": text}
elif webhook_format == "feishu":
    payload = {"msg_type": "text", "content": {"text": text}}
elif webhook_format == "raw":
    payload = raw
else:
    raise SystemExit(f"unsupported STANDX_SUPERVISOR_WEBHOOK_FORMAT={webhook_format}")
request = urllib.request.Request(
    url,
    data=json.dumps(payload).encode("utf-8"),
    headers={"Content-Type": "application/json"},
    method="POST",
)
with urllib.request.urlopen(request, timeout=5) as response:
    if not 200 <= response.status < 300:
        raise RuntimeError(f"webhook returned HTTP {response.status}")
PY
  fi
}

critical_stop() {
  notify "CRITICAL stage2 A/B stopped: $1; no automatic flatten was attempted" critical
  exit 75
}

for value in "$poll_seconds" "$arm_seconds" "$arm_max_seconds"; do
  [[ "$value" =~ ^[1-9][0-9]*$ ]] || {
    printf 'stage2 A/B durations must be positive integers\n' >&2
    exit 64
  }
done
((arm_max_seconds > arm_seconds)) || {
  printf 'stage2 A/B arm_max_seconds (%s) must exceed arm_seconds (%s)\n' \
    "$arm_max_seconds" "$arm_seconds" >&2
  exit 64
}
case "$symbol" in
  XAG-USD|HYPE-USD) ;;
  *)
    printf 'stage2 A/B is frozen to XAG-USD and HYPE-USD\n' >&2
    exit 64
    ;;
esac
[[ -f "$baseline_config" && -f "$candidate_config" ]] || {
  printf 'stage2 A/B frozen configs are missing\n' >&2
  exit 64
}
if [[ "${STANDX_STAGE2_VALIDATE_ONLY:-0}" != "1" && ! -x "$standx_bin" ]]; then
  printf 'stage2 A/B binary is missing\n' >&2
  exit 64
fi

# The two arm configs must be byte-identical after normalizing either
#   (a) the single adaptive_spread enable line (adaptive A/B), or
#   (b) the top-level spread_bps assignment AND the base-tier (first
#       [[adaptive_spread.tiers]]) spread_bps, with adaptive_spread disabled
#       in both (constant-width widening A/B). The maker requires base tier
#       == top-level spread_bps/refresh_bps even when adaptive is disabled,
#       so (b) also rejects either config when those pairs disagree — that
#       mismatch must fail here, not at arm start; or
#   (c) the enabled assignment inside [size_skew], with baseline false,
#       candidate true, and adaptive_spread disabled in both arms; or
#   (d) the enabled assignments inside [nonlinear_skew] AND [external_guard]
#       flipping false -> true together (stage-3 v1 combined pair); or
#   (e) the enabled assignment inside [nonlinear_skew] only (stage-3 v1
#       split pair), with external_guard disabled in both arms; or
#   (f) the enabled assignment inside [external_guard] only (guard spinoff
#       pair), with nonlinear_skew equal in both arms (it may be enabled in
#       both as the inherited stage-3 baseline) and guard params identical; or
#   (g) the candidate adding exactly the pre-registered [external_skew] block
#       (docs/28: enabled, lambda=0.5, cap_bps=8.0, dead_zone_bps=1.0) with
#       every other line byte-identical and the baseline carrying no
#       [external_skew] section at all.
python3 - "$baseline_config" "$candidate_config" <<'PY' || exit 64
from pathlib import Path
import re
import sys
# Read raw bytes so the byte-identity comparisons below see the real file:
# read_text()'s universal-newline translation would let a lone-CR or CRLF
# variant pass as "identical" while being invalid TOML downstream.
baseline = Path(sys.argv[1]).read_bytes().decode("utf-8")
candidate = Path(sys.argv[2]).read_bytes().decode("utf-8")

TOP = "top"
TIER0 = "tier0"
OTHER = "other"


def spread_sections(text):
    """Split lines into top-level / first-tier / other; return (rewritten, fields).

    rewritten blanks every spread_bps in the top section and in the first
    [[adaptive_spread.tiers]] block. fields holds the literal spread_bps /
    refresh_bps values found in those two sections for the coherence check.
    """
    lines = text.splitlines(keepends=True)
    section = TOP
    first_tier_seen = False
    fields = {"top_spread": None, "top_refresh": None,
              "tier0_spread": None, "tier0_refresh": None}
    for i, line in enumerate(lines):
        stripped = line.lstrip()
        if stripped.startswith("["):
            if stripped.startswith("[[adaptive_spread.tiers]]") and not first_tier_seen:
                section = TIER0
                first_tier_seen = True
            elif section != TOP:
                section = OTHER
            continue
        if section == TOP:
            if re.fullmatch(r"\s*spread_bps\s*=\s*([0-9.]+)\s*\n?", line):
                fields["top_spread"] = re.fullmatch(
                    r"\s*spread_bps\s*=\s*([0-9.]+)\s*\n?", line).group(1)
                lines[i] = "spread_bps = <normalized>\n"
            elif re.fullmatch(r"\s*refresh_bps\s*=\s*([0-9.]+)\s*\n?", line):
                fields["top_refresh"] = re.fullmatch(
                    r"\s*refresh_bps\s*=\s*([0-9.]+)\s*\n?", line).group(1)
        elif section == TIER0:
            if re.fullmatch(r"\s*spread_bps\s*=\s*([0-9.]+)\s*\n?", line):
                fields["tier0_spread"] = re.fullmatch(
                    r"\s*spread_bps\s*=\s*([0-9.]+)\s*\n?", line).group(1)
                lines[i] = "spread_bps = <normalized>\n"
            elif re.fullmatch(r"\s*refresh_bps\s*=\s*([0-9.]+)\s*\n?", line):
                fields["tier0_refresh"] = re.fullmatch(
                    r"\s*refresh_bps\s*=\s*([0-9.]+)\s*\n?", line).group(1)
    return "".join(lines), fields


def size_skew_sections(text):
    """Blank the controller enable lines ([size_skew], [nonlinear_skew],
    [external_guard]) and return every controller switch."""
    lines = text.splitlines(keepends=True)
    section = None
    enabled = {
        "adaptive_spread": None,
        "size_skew": None,
        "nonlinear_skew": None,
        "external_guard": None,
    }
    NORMALIZED = {"size_skew", "nonlinear_skew", "external_guard"}
    for i, line in enumerate(lines):
        body = line.rstrip("\r\n")
        ending = line[len(body):]
        header = re.fullmatch(r"\s*\[([^][]+)]\s*(?:#.*)?", body)
        if header:
            section = header.group(1).strip()
            continue
        if body.lstrip().startswith("["):
            section = None
            continue
        if section not in enabled:
            continue
        match = re.fullmatch(
            r"(\s*enabled\s*=\s*)(true|false)(\s*(?:#.*)?)", body
        )
        if match is None:
            continue
        if enabled[section] is not None:
            raise SystemExit(f"stage2 config repeats [{section}].enabled")
        enabled[section] = match.group(2) == "true"
        if section in NORMALIZED:
            lines[i] = f"{match.group(1)}<normalized>{match.group(3)}{ending}"
    return "".join(lines), enabled


# (g) external_skew pair (docs/28): the candidate carries exactly the
# pre-registered [external_skew] block and nothing else differs.
PREREG_EXTERNAL_SKEW = {
    "enabled": "true",
    "lambda": "0.5",
    "cap_bps": "8.0",
    "dead_zone_bps": "1.0",
}


def external_skew_sections(text):
    """Strip any [external_skew] section; return (remainder, fields).

    fields is None when the file has no [external_skew] section. The comment
    or blank lines immediately introducing an added block are part of that
    block's diff, so they are stripped with the section; unknown keys or
    unparsable lines inside the section fail closed. A repeated section or a
    repeated key also fails closed: dict last-wins would otherwise let an
    unregistered earlier value ride along in the file the gate just approved."""
    lines = text.splitlines(keepends=True)
    out = []
    fields = {}
    seen = False
    section = None
    for line in lines:
        body = line.strip()
        header = re.fullmatch(r"\[([^][]+)]", body)
        if header:
            section = header.group(1).strip()
            if section == "external_skew":
                if seen:
                    raise SystemExit(
                        "stage2 config repeats [external_skew]")
                seen = True
                while out and (not out[-1].strip()
                               or out[-1].lstrip().startswith("#")):
                    out.pop()
            continue
        if section == "external_skew":
            match = re.fullmatch(r"([a-z_]+)\s*=\s*(\S+)(?:\s+#.*)?", body)
            if match:
                if match.group(1) in fields:
                    raise SystemExit(
                        f"stage2 [external_skew] repeats {match.group(1)}")
                fields[match.group(1)] = match.group(2)
            elif body and not body.startswith("#"):
                raise SystemExit(
                    f"stage2 [external_skew] unparsable line: {body!r}")
            continue
        out.append(line)
    return "".join(out), (fields if seen else None)


baseline_stripped, baseline_es = external_skew_sections(baseline)
candidate_stripped, candidate_es = external_skew_sections(candidate)
external_skew_pair = (
    baseline_es is None
    and candidate_es == PREREG_EXTERNAL_SKEW
    and candidate_stripped == baseline_stripped
)

adaptive_toggle_only = baseline.replace("enabled = false", "enabled = true") == candidate
if external_skew_pair:
    pass
elif adaptive_toggle_only:
    pass
elif "enabled = false" in baseline and "enabled = false" in candidate:
    baseline_norm, baseline_fields = spread_sections(baseline)
    candidate_norm, candidate_fields = spread_sections(candidate)
    for name, fields in (("baseline", baseline_fields), ("candidate", candidate_fields)):
        if (fields["top_spread"] != fields["tier0_spread"]
                or fields["top_refresh"] != fields["tier0_refresh"]):
            raise SystemExit(
                f"stage2 {name} config incoherent: base tier spread/refresh "
                f"({fields['tier0_spread']}/{fields['tier0_refresh']}) != top-level "
                f"({fields['top_spread']}/{fields['top_refresh']})"
            )
    if baseline_norm == candidate_norm:
        pass
    else:
        baseline_size_norm, baseline_enabled = size_skew_sections(baseline)
        candidate_size_norm, candidate_enabled = size_skew_sections(candidate)
        adaptive_off_both = (
            baseline_enabled["adaptive_spread"] is False
            and candidate_enabled["adaptive_spread"] is False
        )
        # (c) stage-3 v0 pair: only [size_skew].enabled flips.
        size_skew_toggle_only = (
            baseline_size_norm == candidate_size_norm
            and adaptive_off_both
            and baseline_enabled["size_skew"] is False
            and candidate_enabled["size_skew"] is True
            and baseline_enabled["nonlinear_skew"] == candidate_enabled["nonlinear_skew"]
            and baseline_enabled["external_guard"] == candidate_enabled["external_guard"]
            and candidate_enabled["nonlinear_skew"] is not True
            and candidate_enabled["external_guard"] is not True
        )
        # (d) stage-3 v1 combined pair: [nonlinear_skew].enabled AND
        #     [external_guard].enabled both flip false -> true together.
        combined_toggle_only = (
            baseline_size_norm == candidate_size_norm
            and adaptive_off_both
            and baseline_enabled["size_skew"] is False
            and candidate_enabled["size_skew"] is False
            and baseline_enabled["nonlinear_skew"] is False
            and candidate_enabled["nonlinear_skew"] is True
            and baseline_enabled["external_guard"] is False
            and candidate_enabled["external_guard"] is True
        )
        # (e) stage-3 v1 split pair: only [nonlinear_skew].enabled flips.
        nonlinear_toggle_only = (
            baseline_size_norm == candidate_size_norm
            and adaptive_off_both
            and baseline_enabled["size_skew"] is False
            and candidate_enabled["size_skew"] is False
            and baseline_enabled["nonlinear_skew"] is False
            and candidate_enabled["nonlinear_skew"] is True
            and baseline_enabled["external_guard"] == candidate_enabled["external_guard"]
            and candidate_enabled["external_guard"] is not True
        )
        # (f) guard spinoff pair: only [external_guard].enabled flips;
        #     [nonlinear_skew] may be true in BOTH arms (inherited stage-3
        #     accepted baseline). Guard params must be byte-identical.
        guard_toggle_only = (
            baseline_size_norm == candidate_size_norm
            and adaptive_off_both
            and baseline_enabled["size_skew"] is False
            and candidate_enabled["size_skew"] is False
            and baseline_enabled["nonlinear_skew"] == candidate_enabled["nonlinear_skew"]
            and baseline_enabled["external_guard"] is False
            and candidate_enabled["external_guard"] is True
        )
        if not (size_skew_toggle_only or combined_toggle_only
                or nonlinear_toggle_only or guard_toggle_only):
            raise SystemExit(
                "stage2 arm configs differ outside adaptive_spread.enabled / "
                "spread_bps / size_skew.enabled / "
                "nonlinear_skew.enabled(+external_guard.enabled) / "
                "the pre-registered [external_skew] block"
            )
else:
    raise SystemExit(
        "stage2 arm configs differ outside adaptive_spread.enabled / "
        "spread_bps / size_skew.enabled / "
        "the pre-registered [external_skew] block"
    )
PY

if [[ "${STANDX_STAGE2_VALIDATE_ONLY:-0}" == "1" ]]; then
  printf 'stage2 A/B validation ok: symbol=%s baseline=%s candidate=%s\n' \
    "$symbol" "$(sha256sum "$baseline_config" | awk '{print $1}')" \
    "$(sha256sum "$candidate_config" | awk '{print $1}')"
  exit 0
fi

mkdir -p "$(dirname "$ab_lock")" "$log_dir"
exec 9>"$ab_lock"
flock -n 9 || {
  printf 'stage2 A/B orchestrator is already running or a normal live maker owns the guard\n' >&2
  exit 75
}
export STANDX_STAGE2_AB_MEMBER=1
export STANDX_STAGE2_AB_LOCK_PATH="$ab_lock"

# PID of the maker arm currently running (via run_maker_observed.sh). Tracked so
# a SIGTERM/SIGINT — e.g. `docker stop`, `systemctl stop` — is forwarded to the
# live arm for its normal freeze/cancel-all cleanup instead of orphaning a live
# maker. The observed wrapper already forwards the signal on to the binary.
current_arm_pid=""
graceful_shutdown() {
  local signal="$1"
  trap - TERM INT
  notify "stage2 A/B received SIG$signal; forwarding to the active arm for cleanup" warning
  if [[ -n "$current_arm_pid" ]] && kill -0 "$current_arm_pid" 2>/dev/null; then
    kill -TERM "$current_arm_pid" 2>/dev/null || true
    wait "$current_arm_pid" 2>/dev/null || true
  fi
  notify "stage2 A/B stopped on SIG$signal after arm cleanup; no automatic flatten was attempted"
  exit 0
}
trap 'graceful_shutdown TERM' TERM
trap 'graceful_shutdown INT' INT

positions_json() {
  "$standx_bin" --output json account positions --symbol "$symbol"
}

orders_json() {
  "$standx_bin" --output json account orders --symbol "$symbol"
}

json_array_empty() {
  python3 -c 'import json,sys
try:
    value = json.load(sys.stdin)
except (json.JSONDecodeError, UnicodeDecodeError):
    raise SystemExit(2)
if not isinstance(value, list):
    raise SystemExit(2)
raise SystemExit(0 if value == [] else 1)'
}

position_state() {
  local payload
  payload="$(positions_json)" || return 2
  printf '%s' "$payload" | json_array_empty
}

postcheck_is_empty() {
  orders_json | json_array_empty && position_state
}

manifest_position_is_flat() {
  python3 -c 'import json,sys; value=json.load(open(sys.argv[1], encoding="utf-8"))["log"].get("final_position"); raise SystemExit(0 if value is not None and abs(float(value)) <= 1e-12 else 1)' "$1"
}

postcheck_is_empty ||
  critical_stop "initial/restart preflight could not prove orders=[] and positions=[]"
notify "stage2 A/B preflight verified: orders=[] positions=[]"

run_arm() {
  local arm="$1"
  local config="$2"
  local config_hash run_id manifest pid status arm_start deadline position_status
  local hard_deadline next_extension_notice extending
  config_hash="$(sha256sum "$config" | awk '{print $1}')"
  run_id="stage2-${arm}-$(date -u +%Y%m%dT%H%M%SZ)-${config_hash:0:12}"
  manifest="$log_dir/$run_id.manifest.json"
  invalidate_arm() {
    python3 "$root/scripts/maker_run_manifest.py" invalidate \
      --manifest "$manifest" --reason "$1" >/dev/null 2>&1 || true
  }
  notify "stage2 A/B arm starting: arm=$arm run_id=$run_id config_hash=$config_hash"

  STANDX_RUN_ID="$run_id" "$root/scripts/run_maker_observed.sh" \
    "$standx_bin" --output json maker run "$symbol" --maker-config "$config" --live &
  pid=$!
  current_arm_pid="$pid"
  arm_start=$SECONDS
  deadline=$((arm_start + arm_seconds))
  while ((SECONDS < deadline)); do
    if ! kill -0 "$pid" 2>/dev/null; then
      wait "$pid"
      status=$?
      invalidate_arm "arm exited before scheduled duration"
      critical_stop "arm=$arm exited before its ${arm_seconds}s window (status=$status run_id=$run_id)"
    fi
    sleep "$poll_seconds"
  done

  # Past the arm_seconds minimum: latch maker wind-down with SIGUSR1. The
  # maker stops quoting for good and flattens any residual position via
  # reduce-only market exits, so the arm should reach flat within a few
  # cycles; the poll below confirms it on the venue. Escalate only on a
  # genuinely unsafe state (position query fails, maker dies) or when the arm
  # stays non-flat past the arm_max_seconds hard cap (wind-down stalled).
  kill -USR1 "$pid" 2>/dev/null || true
  hard_deadline=$((arm_start + arm_max_seconds))
  next_extension_notice=$SECONDS
  extending=0
  while true; do
    position_state
    position_status=$?
    if [[ "$position_status" == "0" ]]; then
      break
    fi
    if [[ "$position_status" != "1" ]]; then
      kill -TERM "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
      invalidate_arm "venue position query or JSON validation failed"
      critical_stop "arm=$arm could not prove venue position state (run_id=$run_id)"
    fi
    if ! kill -0 "$pid" 2>/dev/null; then
      wait "$pid"
      status=$?
      invalidate_arm "arm exited while awaiting wind-down flat"
      critical_stop "arm=$arm exited while waiting for wind-down flat (status=$status run_id=$run_id)"
    fi
    if ((SECONDS >= hard_deadline)); then
      kill -TERM "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
      invalidate_arm "position stayed nonzero past the hard arm cap"
      critical_stop "arm=$arm stayed non-flat past ${arm_max_seconds}s hard cap (run_id=$run_id invalid)"
    fi
    if ((SECONDS >= next_extension_notice)); then
      extending=1
      # Re-signal with each warning: wind-down is latched, so repeats are
      # harmless and cover a signal lost to a mid-arm maker restart.
      kill -USR1 "$pid" 2>/dev/null || true
      notify "stage2 A/B arm=$arm past its ${arm_seconds}s window, wind-down signaled but not yet flat (hard cap ${arm_max_seconds}s, run_id=$run_id)" warning
      next_extension_notice=$((SECONDS + flat_grace_seconds))
    fi
    sleep "$poll_seconds"
  done
  if ((extending == 1)); then
    notify "stage2 A/B arm=$arm flat after wind-down; proceeding to switch (run_id=$run_id)"
  fi

  kill -TERM "$pid" 2>/dev/null || true
  wait "$pid"
  status=$?
  if [[ "$status" != "0" ]]; then
    invalidate_arm "maker cleanup exited nonzero"
    critical_stop "arm=$arm cleanup exited $status (run_id=$run_id)"
  fi
  python3 "$root/scripts/maker_run_manifest.py" validate \
    --manifest "$manifest" --repo-root "$root" >/dev/null ||
    {
      invalidate_arm "manifest validation failed"
      critical_stop "arm=$arm manifest validation failed (run_id=$run_id)"
    }
  manifest_position_is_flat "$manifest" || {
    invalidate_arm "maker ledger manifest ended nonzero or unavailable"
    critical_stop "arm=$arm maker ledger manifest was not flat (run_id=$run_id)"
  }
  postcheck_is_empty || {
    invalidate_arm "independent post-check found orders or position"
    critical_stop "arm=$arm independent post-check found orders or position (run_id=$run_id)"
  }
  notify "stage2 A/B arm complete: arm=$arm run_id=$run_id config_hash=$config_hash orders=[] positions=[]"
  current_arm_pid=""
}

# Arm the loop starts with. Default baseline; set to "candidate" only when
# resuming an interrupted A/B whose first baseline arm already completed
# (operator-directed). Any other value fails closed.
first_arm="${STANDX_STAGE2_FIRST_ARM:-baseline}"
case "$first_arm" in
  baseline|candidate) ;;
  *)
    printf 'stage2 A/B STANDX_STAGE2_FIRST_ARM must be baseline or candidate\n' >&2
    exit 64
    ;;
esac

while true; do
  if [[ "$first_arm" == "baseline" ]]; then
    run_arm baseline "$baseline_config"
  fi
  first_arm=baseline
  run_arm candidate "$candidate_config"
done
