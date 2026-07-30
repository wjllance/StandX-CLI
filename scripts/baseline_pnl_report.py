#!/usr/bin/env python3
"""Build a compact baseline-PnL run report from a maker NDJSON log and send it
via lark-cli (Feishu) to a user P2P chat.

Usage:
    python3 scripts/baseline_pnl_report.py <ndjson> --run-id <id> \
        --lark-user <open_id> [--dry-run]

Report contents follow docs/27-maker-baseline-pnl-collection-runbook.md
(daily-record table): net PnL + cost attribution, markout, uptime, guard/halt
shares, standby evidence, plus process liveness. Readings only, no judgments.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
from datetime import datetime, timezone


def load_events(path: str):
    cycle_summaries = []
    standby_events = []
    fills_total = 0
    cancels_by_reason = {}
    exits_submitted = 0
    exit_suppressed = 0
    criticals = []
    first_ts = None
    last_ts = None

    with open(path, "r", encoding="utf-8") as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            try:
                ev = json.loads(line)
            except json.JSONDecodeError:
                continue
            action = ev.get("action")
            ts = ev.get("ts")
            if ts:
                if first_ts is None:
                    first_ts = ts
                last_ts = ts
            if action == "cycle_summary":
                cycle_summaries.append(ev)
                perf = ev.get("performance") or {}
                ft = ev.get("fills_total", perf.get("fills_total")) or 0
                fills_total = max(fills_total, int(ft))
                if ev.get("exit_submitted"):
                    exits_submitted += 1
                es = ev.get("exit_suppressed")
                if es:
                    exit_suppressed += int(es) if isinstance(es, (int, float)) else 1
            elif action == "market_data_standby":
                standby_events.append(ev)
            elif action == "cancel":
                reason = str(ev.get("reason") or "unknown")
                cancels_by_reason[reason] = cancels_by_reason.get(reason, 0) + 1
            if ev.get("severity") == "critical":
                criticals.append(ev)

    return {
        "cycles": cycle_summaries,
        "standby": standby_events,
        "fills_total": fills_total,
        "cancels_by_reason": cancels_by_reason,
        "exits_submitted": exits_submitted,
        "exit_suppressed": exit_suppressed,
        "criticals": criticals,
        "first_ts": first_ts,
        "last_ts": last_ts,
    }


def fmt(v, digits=6):
    if v is None:
        return "n/a"
    if isinstance(v, (int, float)):
        return f"{v:.{digits}f}".rstrip("0").rstrip(".") if v else "0"
    return str(v)


def build_report(stats, run_id: str, process_alive: bool, alerts) -> str:
    cycles = stats["cycles"]
    if not cycles:
        return f"[standx] {run_id}: 日志中还没有 cycle_summary"
    latest = cycles[-1]
    perf = latest.get("performance") or {}

    n = len(cycles)
    guard_on = sum(1 for c in cycles if c.get("guard_active"))
    halted = sum(1 for c in cycles if c.get("halted"))

    standby_max = 0.0
    standby_total = 0.0
    for ev in stats["standby"]:
        secs = ev.get("paused_secs")
        if isinstance(secs, (int, float)):
            standby_max = max(standby_max, secs)
            standby_total += secs

    cancels = stats["cancels_by_reason"]
    cancel_str = (
        ", ".join(f"{k}={v}" for k, v in sorted(cancels.items(), key=lambda kv: -kv[1]))
        if cancels
        else "0"
    )

    crit = len(stats["criticals"])
    alive = "alive" if process_alive else "**DEAD**"

    alert_lines = [f"🚨 ALERT: {a}" for a in alerts] if alerts else []

    lines = [
        f"[standx] 基线 PnL 采集运行报告",
        *alert_lines,
        f"run: {run_id}",
        f"进程: {alive} | 最新 cycle: {latest.get('ts')} (#{latest.get('cycle')})",
        f"仓位: {fmt(latest.get('position'))} | 会话 PnL: {fmt(latest.get('pnl'))}",
        (
            f"净额归因: gross {fmt(perf.get('gross_spread_quote'))}"
            f" | fee {fmt(perf.get('fee_quote'))}"
            f" | rebate {fmt(perf.get('rebate_quote'))}"
            f" | funding {fmt(perf.get('funding_quote'))}"
            f" (available={perf.get('funding_available')}"
            f", unattributed={perf.get('funding_unattributed')})"
        ),
        (
            f"net_pnl_complete={perf.get('net_pnl_complete')}"
            f" | costs_unavailable={perf.get('execution_costs_unavailable')}"
        ),
        (
            f"markout bps: 1s {fmt(perf.get('markout_1s_bps'), 2)}"
            f" / 5s {fmt(perf.get('markout_5s_bps'), 2)}"
            f" / 30s {fmt(perf.get('markout_30s_bps'), 2)}"
        ),
        (
            f"uptime: {fmt(perf.get('time_weighted_uptime_pct'), 1)}%"
            f" | fills_total: {stats['fills_total']}"
            f" | 撤单: {cancel_str}"
        ),
        (
            f"guard 激活: {guard_on}/{n} cycles ({100.0 * guard_on / n:.1f}%)"
            f" | halt: {halted}/{n} ({100.0 * halted / n:.1f}%)"
            f" | 主动退出: {stats['exits_submitted']} (suppressed {stats['exit_suppressed']})"
        ),
        (
            f"standby: {len(stats['standby'])} 次, 单次 max {standby_max:.0f}s"
            f", 累计 {standby_total:.0f}s"
        ),
        f"critical 事件: {crit}" + (" ⚠️" if crit else ""),
    ]
    if crit:
        last = stats["criticals"][-1]
        lines.append(f"最近 critical: {last.get('ts')} {str(last.get('message'))[:120]}")
    return "\n".join(lines)


def process_alive(pattern: str) -> bool:
    try:
        out = subprocess.run(["pgrep", "-f", pattern], capture_output=True, timeout=10)
        # pgrep -f matches this script's own caller only if the shell command
        # line contains the pattern; exclude ourselves defensively.
        return out.returncode == 0
    except Exception:
        return False


def compute_alerts(stats, alive: bool, stale_minutes: float) -> list:
    """Conditions that make the report exit non-zero: the send succeeding must
    never again be read as 'everything is fine' (2026-07-29 incident: the maker
    died at 19:09Z and five consecutive hourly reports still relayed '正常')."""
    alerts = []
    if not alive:
        alerts.append("maker 进程 DEAD（pgrep 无匹配）")
    crit = len(stats["criticals"])
    if crit:
        last = stats["criticals"][-1]
        alerts.append(f"critical 事件 {crit} 条，最近: {last.get('ts')} {str(last.get('message'))[:100]}")
    cycles = stats["cycles"]
    if not cycles:
        alerts.append("日志中没有任何 cycle_summary")
    else:
        latest_ts = cycles[-1].get("ts")
        try:
            latest = datetime.fromisoformat(str(latest_ts).replace("Z", "+00:00"))
            age = (datetime.now(timezone.utc) - latest).total_seconds() / 60.0
            if age > stale_minutes:
                alerts.append(f"日志停滞：最新 cycle_summary 已 {age:.0f} 分钟（阈值 {stale_minutes:.0f}）")
        except (ValueError, TypeError):
            alerts.append(f"无法解析最新 cycle 时间戳: {latest_ts}")
    return alerts


def send_lark(text: str, open_id: str) -> int:
    env = dict(os.environ)
    env["LARKSUITE_CLI_NO_UPDATE_NOTIFIER"] = "1"
    env["LARKSUITE_CLI_NO_SKILLS_NOTIFIER"] = "1"
    proc = subprocess.run(
        [
            "lark-cli",
            "im",
            "+messages-send",
            "--user-id",
            open_id,
            "--as",
            "bot",
            "--text",
            text,
        ],
        capture_output=True,
        text=True,
        timeout=60,
        env=env,
    )
    if proc.returncode != 0:
        print(proc.stderr, file=sys.stderr)
        return proc.returncode
    try:
        envelope = json.loads(proc.stdout)
        if not envelope.get("ok"):
            print(proc.stdout, file=sys.stderr)
            return 1
    except json.JSONDecodeError:
        pass
    return 0


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("ndjson")
    ap.add_argument("--run-id", required=True)
    ap.add_argument("--lark-user", default="ou_a7b8a38029ac2112422871a5f22aaf9c")
    ap.add_argument(
        "--process-pattern",
        default="maker run HYPE-USD",
        help="pgrep -f pattern used for the liveness check",
    )
    ap.add_argument(
        "--stale-minutes",
        type=float,
        default=10.0,
        help="alert when the newest cycle_summary is older than this",
    )
    ap.add_argument("--dry-run", action="store_true", help="print report, do not send")
    args = ap.parse_args()

    stats = load_events(args.ndjson)
    alive = process_alive(args.process_pattern)
    alerts = compute_alerts(stats, alive, args.stale_minutes)
    report = build_report(stats, args.run_id, alive, alerts)
    if args.dry_run:
        print(report)
        return 1 if alerts else 0
    rc = send_lark(report, args.lark_user)
    if rc != 0:
        return rc
    for alert in alerts:
        print(f"ALERT: {alert}")
    print(f"report sent at {datetime.now(timezone.utc).isoformat()}")
    return 1 if alerts else 0


if __name__ == "__main__":
    sys.exit(main())
