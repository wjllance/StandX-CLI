#!/usr/bin/env python3
"""Quote staleness at adverse-move moments: 3s vs 0.5s refresh cadence.

Question (docs/28 方案 0): at each observed adverse move, how many bps stale is
a quote refreshing every 3s vs every 0.5s? This prices how much of the measured
adverse selection (mo5 ~ -6.2bps) is pure staleness rather than strategy.

Method: reuse lag_analysis.load / detect_jumps on the lag-recorder NDJSON
(Hyperliquid midPx = fast leader, StandX mark = the maker's quote basis).
An adverse move is a leader jump >= event-bps within window-ms (same detection
as the 2026-07-22 evidence doc, deduped with a window-ms refractory). At the
detection instant t1, a quote on a fixed refresh grid of cadence C has a random
age ~ U[0, C]; we evaluate staleness at the mean age (C/2) and worst age (C):

    staleness(a, ref) = sign(jump) * (ref(t1) - ref(t1 - a))   [bps, adverse +]

ref = HL mid  -> true staleness vs the fast market (what the taker sees);
ref = SX mark -> staleness visible on the maker's own feed (what the maker
                 could have reacted to without an external signal).

The gap between the two is the part only an external signal can fix; the SX
mark part is bounded below by StandX's ~3s mark-tick cadence regardless of
refresh rate.

Caveats (read before quoting numbers):
- Offline, open-loop: says nothing about whether a faster refresh actually
  avoids the fill (cancel RTT 0.3-0.5s, exchange rate limits, re-quote queue).
  Per hard rule 1 this prioritizes work; it does not approve any live change.
- Single 44.5h window, HYPE only, no >10% tail regime.
- Fixed differential network latency biases absolute levels (same as the lag
  evidence doc); cadence DIFFERENCES at the same age are robust to it.

Usage: python3 scripts/quote_staleness.py var/standx/lag-rec-XXX.ndjson
           [--event-bps 8] [--event-window-ms 2000]
"""

import argparse
import bisect
import json
import os
import sys
from statistics import mean, median

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from lag_analysis import load, detect_jumps, quantile  # noqa: E402

CADENCES_S = (3.0, 0.5)


def dedupe(jumps, refractory_ms):
    """Keep the first jump of each cluster; detect_jumps fires once per anchor
    sample, so one market move yields many overlapping anchors."""
    out = []
    last_t1 = None
    for t0, p0, t1, p1, move in jumps:
        if last_t1 is None or t1 - last_t1 > refractory_ms:
            out.append((t0, p0, t1, p1, move))
            last_t1 = t1
    return out


def make_lookup(series):
    ts = [t for t, _ in series]

    def at(t_ms):
        """Last observation at or before t_ms (step function, like the feed)."""
        i = bisect.bisect_right(ts, t_ms) - 1
        return series[i][1] if i >= 0 else None

    return at


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("path")
    ap.add_argument("--event-bps", type=float, default=8.0)
    ap.add_argument("--event-window-ms", type=int, default=2000)
    args = ap.parse_args()

    standx, hyper = load(args.path, "mark", "mid")
    print(f"standx ticks {len(standx)}  hyper ticks {len(hyper)}")

    # detect_jumps yields (t0, p0, p1, move_bps) without t1; recover the
    # detection instant t1 (first sample within the window crossing event-bps).
    raw = []
    n = len(hyper)
    hts = [t for t, _ in hyper]
    for t0, p0, p1, move in detect_jumps(hyper, args.event_bps, args.event_window_ms):
        i = bisect.bisect_left(hts, t0)
        k = i + 1
        while k < n and hyper[k][0] - t0 <= args.event_window_ms:
            if abs((hyper[k][1] / p0 - 1.0) * 1e4) >= args.event_bps:
                raw.append((t0, p0, hyper[k][0], hyper[k][1], move))
                break
            k += 1
    events = dedupe(raw, args.event_window_ms)
    print(f"leader jumps >= {args.event_bps:g}bps/{args.event_window_ms}ms: "
          f"{len(raw)} raw, {len(events)} after {args.event_window_ms}ms refractory")

    sx_at = make_lookup(standx)
    hl_at = make_lookup(hyper)

    ages = sorted({c / 2 for c in CADENCES_S} | set(CADENCES_S))
    # staleness[cadence][age_kind][ref] -> list of bps
    res = {c: {kind: {"hl": [], "sx": []} for kind in ("mean", "worst")}
           for c in CADENCES_S}
    invisible = 0  # events where SX mark has barely moved at detection
    used = 0
    for t0, p0, t1, p1, move in events:
        sign = 1.0 if move > 0 else -1.0
        hl_now = hl_at(t1)
        sx_now = sx_at(t1)
        if hl_now is None or sx_now is None:
            continue
        used += 1
        sx_then = sx_at(t1 - 1000)  # own-feed visibility at detection
        if sx_then is not None and abs((sx_now / sx_then - 1.0) * 1e4) < 1.0:
            invisible += 1
        for c in CADENCES_S:
            for kind, age in (("mean", c / 2), ("worst", c)):
                a_ms = age * 1000
                hl_past = hl_at(t1 - a_ms)
                sx_past = sx_at(t1 - a_ms)
                if hl_past is not None:
                    res[c][kind]["hl"].append(sign * (hl_now / hl_past - 1.0) * 1e4)
                if sx_past is not None:
                    res[c][kind]["sx"].append(sign * (sx_now / sx_past - 1.0) * 1e4)

    print(f"events scored: {used}; SX mark moved <1bps in the 1s before "
          f"detection (own-feed invisible): {invisible} ({invisible / max(used, 1) * 100:.0f}%)\n")
    print(f"{'cadence':>8} {'age':>6} {'ref':>4} {'n':>4} {'mean':>7} "
          f"{'median':>7} {'p90':>6}")
    for c in CADENCES_S:
        for kind in ("mean", "worst"):
            age = c / 2 if kind == "mean" else c
            for ref, label in (("hl", "HL"), ("sx", "SX")):
                xs = res[c][kind][ref]
                print(f"{c:>6.1f}s {age:>5.2f}s {label:>4} {len(xs):>4} "
                      f"{mean(xs):>7.2f} {median(xs):>7.2f} "
                      f"{quantile(xs, 0.9):>6.2f}")

    print("\n=== reading ===")
    for c in CADENCES_S:
        hl = res[c]["mean"]["hl"]
        sx = res[c]["mean"]["sx"]
        print(f"cadence {c:.1f}s (mean age {c / 2:.2f}s): true staleness "
              f"{mean(hl):.2f}bps, of which own-feed-visible {mean(sx):.2f}bps, "
              f"external-only {mean(hl) - mean(sx):.2f}bps")
    save_hl = mean(res[3.0]["mean"]["hl"]) - mean(res[0.5]["mean"]["hl"])
    save_sx = mean(res[3.0]["mean"]["sx"]) - mean(res[0.5]["mean"]["sx"])
    print(f"\n3s -> 0.5s refresh saves at adverse-move moments: "
          f"{save_hl:.2f}bps true / {save_sx:.2f}bps own-feed-visible "
          f"(per adversely-hit quote, BEFORE cancel RTT and fill-avoidance "
          f"realizability haircuts)")


if __name__ == "__main__":
    main()
