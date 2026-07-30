#!/usr/bin/env bash
set -uo pipefail

if [[ $# -lt 1 ]]; then
  printf 'usage: %s <standx maker command...>\n' "$0" >&2
  exit 64
fi

args=("$@")
has_maker=0
has_run=0
has_json=0
config_file=""

for ((i = 0; i < ${#args[@]}; i++)); do
  arg="${args[$i]}"
  [[ "$arg" == "maker" ]] && has_maker=1
  [[ "$arg" == "run" ]] && has_run=1
  [[ "$arg" == "--output=json" ]] && has_json=1
  if [[ "$arg" == "--output" || "$arg" == "-o" ]]; then
    if ((i + 1 < ${#args[@]})) && [[ "${args[$((i + 1))]}" == "json" ]]; then
      has_json=1
    fi
  fi
  if [[ "$arg" == "--maker-config" ]] && ((i + 1 < ${#args[@]})); then
    config_file="${args[$((i + 1))]}"
  elif [[ "$arg" == --maker-config=* ]]; then
    config_file="${arg#--maker-config=}"
  fi
done

if ((has_maker == 0 || has_run == 0 || has_json == 0)); then
  printf 'error: command must be a maker run with --output json\n' >&2
  exit 64
fi

umask 077
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/.." && pwd)"
log_dir="${STANDX_LOG_DIR:-$repo_root/var/standx}"
run_id="${STANDX_RUN_ID:-$(date -u +%Y%m%dT%H%M%SZ)-$$}"
stdout_log="$log_dir/$run_id.ndjson"
stderr_log="$log_dir/$run_id.stderr.log"
manifest_file="$log_dir/$run_id.manifest.json"
pipe_dir=""
child_pid=""
uploader_pid=""

mkdir -p "$log_dir"
pipe_dir="$(mktemp -d "$log_dir/.pipes.XXXXXX")"
mkfifo "$pipe_dir/stdout" "$pipe_dir/stderr"

cleanup() {
  if [[ -n "$uploader_pid" ]] && kill -0 "$uploader_pid" 2>/dev/null; then
    kill -TERM "$uploader_pid" 2>/dev/null || true
    wait "$uploader_pid" 2>/dev/null || true
  fi
  [[ -n "$pipe_dir" && -d "$pipe_dir" ]] && rm -rf "$pipe_dir"
}

forward_signal() {
  local signal="$1"
  if [[ -n "$child_pid" ]] && kill -0 "$child_pid" 2>/dev/null; then
    kill -"$signal" "$child_pid" 2>/dev/null || true
  fi
}

# Emit an operational notice to stderr and, if configured, to a webhook.
# The webhook is best-effort: a failed POST never affects the maker.
notify() {
  local message="$1"
  printf '%s\n' "$message" >&2
  if [[ -n "${STANDX_SUPERVISOR_WEBHOOK:-}" ]] && command -v curl >/dev/null 2>&1; then
    # Escape backslashes and double quotes so the message is valid JSON.
    local escaped="${message//\\/\\\\}"
    escaped="${escaped//\"/\\\"}"
    # Feishu custom bot only. The Slack shape gets `{"code":19002,"msg":
    # "params error, msg_type need"}` at HTTP 200, which curl -f cannot see —
    # the notice would vanish silently.
    curl -fsS -m 5 -X POST \
      -H 'Content-Type: application/json' \
      --data "{\"msg_type\":\"text\",\"content\":{\"text\":\"$escaped\"}}" \
      "$STANDX_SUPERVISOR_WEBHOOK" >/dev/null 2>&1 ||
      printf 'supervisor webhook post failed (message logged above)\n' >&2
  fi
}

trap cleanup EXIT
trap 'forward_signal INT' INT
trap 'forward_signal TERM' TERM
trap 'forward_signal USR1' USR1

: >"$stdout_log"
: >"$stderr_log"

tee "$stdout_log" <"$pipe_dir/stdout" &
stdout_tee_pid=$!
tee "$stderr_log" <"$pipe_dir/stderr" >&2 &
stderr_tee_pid=$!

git_sha="$(git -C "$repo_root" rev-parse --short HEAD 2>/dev/null || true)"
config_hash=""
if [[ -n "$config_file" && -f "$config_file" ]]; then
  config_hash="$(shasum -a 256 "$config_file" | awk '{print $1}')"
fi

manifest_args=(
  "$script_dir/maker_run_manifest.py" start
  --manifest "$manifest_file"
  --log "$stdout_log"
  --run-id "$run_id"
  --repo-root "$repo_root"
  --collector-wrapper "$script_dir/run_maker_observed.sh"
)
[[ -n "$config_file" ]] && manifest_args+=(--config-file "$config_file")
[[ -n "${STANDX_BASELINE_PRICE_TICK_DECIMALS:-}" ]] &&
  manifest_args+=(--price-tick-decimals "$STANDX_BASELINE_PRICE_TICK_DECIMALS")
[[ -n "${STANDX_BASELINE_QTY_TICK_DECIMALS:-}" ]] &&
  manifest_args+=(--qty-tick-decimals "$STANDX_BASELINE_QTY_TICK_DECIMALS")
[[ -n "${STANDX_BASELINE_MIN_ORDER_QTY:-}" ]] &&
  manifest_args+=(--min-order-qty "$STANDX_BASELINE_MIN_ORDER_QTY")
manifest_args+=(-- "${args[@]}")
python3 "${manifest_args[@]}" ||
  printf 'warning: maker baseline manifest could not be initialized: %s\n' "$manifest_file" >&2

uploader_enabled=0

# Launch (or relaunch) the live follow uploader. Follow mode is incremental
# and resumes from the on-disk checkpoint, so a relaunch never re-sends events
# that were already ingested.
start_uploader() {
  local upload_args=(
    "$script_dir/openobserve_ingest.py" "$stdout_log"
    --run-id "$run_id"
    --incremental
    --follow
    --preflight
    --poll-interval "${OPENOBSERVE_UPLOAD_INTERVAL:-2}"
  )
  [[ -n "$git_sha" ]] && upload_args+=(--git-sha "$git_sha")
  [[ -n "$config_hash" ]] && upload_args+=(--config-hash "$config_hash")
  python3 "${upload_args[@]}" >&2 &
  uploader_pid=$!
}

if [[ "${OPENOBSERVE_AUTO_UPLOAD:-0}" == "1" ]]; then
  if [[ -z "${OPENOBSERVE_USER:-}" || -z "${OPENOBSERVE_PASSWORD:-}" ]]; then
    printf 'OpenObserve live upload skipped: credentials are not exported\n' >&2
  else
    uploader_enabled=1
    start_uploader
    printf 'OpenObserve live uploader starting: run_id=%s interval=%ss pid=%s\n' \
      "$run_id" "${OPENOBSERVE_UPLOAD_INTERVAL:-2}" "$uploader_pid" >&2
  fi
fi

"${args[@]}" >"$pipe_dir/stdout" 2>"$pipe_dir/stderr" &
child_pid=$!

# Supervise the uploader while the maker runs. If the follow loop dies
# mid-run (an uncaught error, OOM kill, etc.) the only remote symptom is a
# stale dashboard, so relaunch it and emit a notice. We poll instead of
# blocking on `wait "$child_pid"` so the check happens during the run, not
# after the maker already exited.
supervise_interval="${STANDX_SUPERVISE_INTERVAL:-5}"
while kill -0 "$child_pid" 2>/dev/null; do
  if ((uploader_enabled == 1)) && [[ -n "$uploader_pid" ]] &&
    ! kill -0 "$uploader_pid" 2>/dev/null; then
    uploader_exit=0
    wait "$uploader_pid" 2>/dev/null || uploader_exit=$?
    notify "OpenObserve live uploader died (run_id=$run_id status=$uploader_exit) while maker still running; relaunching"
    start_uploader
    notify "OpenObserve live uploader relaunched (run_id=$run_id pid=$uploader_pid)"
  fi
  sleep "$supervise_interval"
done

wait "$child_pid"
child_status=$?
wait "$stdout_tee_pid" || true
wait "$stderr_tee_pid" || true

printf 'maker run_id=%s exit=%s stdout=%s stderr=%s\n' \
  "$run_id" "$child_status" "$stdout_log" "$stderr_log" >&2

python3 "$script_dir/maker_run_manifest.py" finalize \
  --manifest "$manifest_file" \
  --log "$stdout_log" \
  --exit-status "$child_status" ||
  printf 'warning: maker baseline manifest could not be finalized: %s\n' "$manifest_file" >&2

if [[ -n "$uploader_pid" ]]; then
  uploader_status=0
  if kill -0 "$uploader_pid" 2>/dev/null; then
    kill -TERM "$uploader_pid" 2>/dev/null || true
  fi
  wait "$uploader_pid" || uploader_status=$?
  uploader_pid=""
  if ((uploader_status != 0)); then
    printf 'OpenObserve live uploader exited with status %s; attempting final catch-up\n' \
      "$uploader_status" >&2
    final_upload_args=(
      "$script_dir/openobserve_ingest.py" "$stdout_log"
      --run-id "$run_id"
      --incremental
    )
    [[ -n "$git_sha" ]] && final_upload_args+=(--git-sha "$git_sha")
    [[ -n "$config_hash" ]] && final_upload_args+=(--config-hash "$config_hash")
    python3 "${final_upload_args[@]}" >&2 || \
      printf 'OpenObserve final upload failed; raw logs are intact\n' >&2
  fi
fi

exit "$child_status"
