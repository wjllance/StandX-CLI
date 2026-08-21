#!/usr/bin/env python3
"""Guardrail metrics for the external_skew A/B (docs/28 验收判据).

Reads stage2 arm NDJSON logs and prints, per arm and pooled by treatment:
  - cancel rate: sum(cycle_summary.cancels) / arm wall hours (cancel/quote-hour);
    guardrail: candidate 上涨 > 50% vs baseline => rejected.
  - time-weighted inventory |position|: per-cycle position weighted by dt to the
    next cycle; mean and weighted p95; guardrail: 均值或 p95 显著上偏 => rejected.
  - uptime: last cycle_summary performance.time_weighted_uptime_pct (cumulative
    for the arm); pooled = hour-weighted mean; guardrail: 下降 > 2pp => rejected.
The fourth guardrail (passive capture drop > 1bps) is reported by
maker_markout_ab.py, not here.

Usage: python3 scripts/maker_guardrail_ab.py <arm.ndjson> [...]
Arm name is taken from the filename: stage2-<arm>-<ts>-<hash>.ndjson.
"""

import json
import sys
from datetime import datetime, timezone


def parse_ts(s):
    return datetime.fromisoformat(s.replace("Z", "+00:00")).astimezone(
        timezone.utc
    ).timestamp()


def read_arm(path):
    """Return dict with cancels, hours, uptime_pct, inv (list of (|pos|, weight))."""
    cycles = []  # (ts, cancels, |position|)
    uptime = None
    with open(path) as f:
        for line in f:
            try:
                d = json.loads(line)
            except Exception:
                continue
            if d.get("action") != "cycle_summary":
                continue
            try:
                ts = parse_ts(d["ts"])
                pos = abs(float(d.get("position") or 0.0))
                cancels = int(d.get("cancels") or 0)
            except Exception:
                continue
            cycles.append((ts, cancels, pos))
            perf = d.get("performance") or {}
            u = perf.get("time_weighted_uptime_pct")
            if u is not None:
                uptime = float(u)
    cycles.sort()
    if len(cycles) < 2:
        return None
    hours = (cycles[-1][0] - cycles[0][0]) / 3600.0
    inv = []
    for i, (ts, _, pos) in enumerate(cycles):
        w = (cycles[i + 1][0] - ts) if i + 1 < len(cycles) else 0.0
        if w > 0:
            inv.append((pos, w))
    return {
        "cancels": sum(c[1] for c in cycles),
        "hours": hours,
        "uptime": uptime,
        "inv": inv,
    }


def wmean(inv):
    tw = sum(w for _, w in inv)
    return sum(p * w for p, w in inv) / tw if tw else 0.0


def wp95(inv):
    tw = sum(w for _, w in inv)
    if not tw:
        return 0.0
    acc = 0.0
    for p, w in sorted(inv):
        acc += w
        if acc >= 0.95 * tw:
            return p
    return inv[-1][0] if inv else 0.0


def pool(arms):
    cancels = sum(a["cancels"] for a in arms)
    hours = sum(a["hours"] for a in arms)
    inv = [pw for a in arms for pw in a["inv"]]
    upt = [(a["uptime"], a["hours"]) for a in arms if a["uptime"] is not None]
    uptw = sum(h for _, h in upt)
    return {
        "n_arms": len(arms),
        "cancel_rate": cancels / hours if hours else 0.0,
        "hours": hours,
        "inv_mean": wmean(inv),
        "inv_p95": wp95(inv),
        "uptime": sum(u * h for u, h in upt) / uptw if uptw else None,
    }


def main(paths):
    by_arm = {}
    for p in paths:
        name = p.rsplit("/", 1)[-1].split("-")[1]
        a = read_arm(p)
        if a is None:
            continue
        by_arm.setdefault(name, []).append((p.rsplit("/", 1)[-1], a))

    print("=== per-arm ===")
    for name in sorted(by_arm):
        for fname, a in sorted(by_arm[name]):
            rate = a["cancels"] / a["hours"] if a["hours"] else 0.0
            print(
                f"{fname}: {a['hours']:.1f}h cancels {a['cancels']} "
                f"rate {rate:.0f}/h inv_mean {wmean(a['inv']):.3f} "
                f"inv_p95 {wp95(a['inv']):.3f} "
                f"uptime {a['uptime'] if a['uptime'] is not None else float('nan'):.2f}%"
            )

    print("\n=== pooled by treatment ===")
    pooled = {}
    for name in sorted(by_arm):
        r = pool([a for _, a in by_arm[name]])
        pooled[name] = r
        up = f"{r['uptime']:.2f}%" if r["uptime"] is not None else "n/a"
        print(
            f"{name}: arms {r['n_arms']} hours {r['hours']:.1f} "
            f"cancel_rate {r['cancel_rate']:.0f}/h "
            f"inv_mean {r['inv_mean']:.3f} inv_p95 {r['inv_p95']:.3f} uptime {up}"
        )

    names = sorted(pooled)
    if len(names) == 2:
        b, c = pooled[names[0]], pooled[names[1]]
        print("\n=== guardrail check (candidate vs baseline, docs/28) ===")
        if b["cancel_rate"] > 0:
            dr = (c["cancel_rate"] - b["cancel_rate"]) / b["cancel_rate"] * 100
            print(
                f"cancel_rate: {b['cancel_rate']:.0f} -> {c['cancel_rate']:.0f}/h "
                f"({dr:+.0f}%, reject if > +50%) "
                f"{'FAIL' if dr > 50 else 'ok'}"
            )
        du = (c["uptime"] or 0.0) - (b["uptime"] or 0.0)
        print(
            f"uptime: {b['uptime']:.2f}% -> {c['uptime']:.2f}% "
            f"({du:+.2f}pp, reject if < -2pp) {'FAIL' if du < -2 else 'ok'}"
        )
        print(
            f"inventory |pos|: mean {b['inv_mean']:.3f} -> {c['inv_mean']:.3f}, "
            f"p95 {b['inv_p95']:.3f} -> {c['inv_p95']:.3f} "
            f"(reject on 显著上偏, judgement call)"
        )


if __name__ == "__main__":
    if len(sys.argv) < 2:
        print(__doc__)
        sys.exit(2)
    main(sys.argv[1:])
