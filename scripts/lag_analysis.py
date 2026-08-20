#!/usr/bin/env python3
"""Measure how far StandX's mark price lags Hyperliquid. Read-only, stdlib only.

Usage:
    python3 scripts/lag_analysis.py LAG.ndjson [--standx-field mark|mid|index]
                                    [--leader-field mark|mid|index]
                                    [--grid-ms 250] [--max-lag-ms 4000]
                                    [--event-bps 8] [--event-window-ms 2000]
                                    [--cover-frac 0.5]

Input is the NDJSON produced by `standx lag-recorder`: one price observation per
line, each tagged `source` ("standx" | "hyperliquid") with a common-clock
`local_recv_ms` and price fields (mark/mid/index/last/best_bid/best_ask).

Each source uses its own field: the leader (Hyperliquid) defaults to `mid`
(midPx, the fastest-moving public price on that feed), StandX to `mark` (the
price the maker quotes against). The earlier single `--field` option was
replaced by `--standx-field` / `--leader-field`.

Three statistics are printed:

1. Cross-correlation of resampled price increments across candidate lags. The
   lag maximizing correlation is the typical co-movement offset (positive =
   StandX lags Hyperliquid).
2. Event response: when Hyperliquid jumps sharply, how long until StandX covers
   a fraction of that move. More robust than (1) because it isolates the moments
   that actually matter (the jumps that snipe stale quotes), and it is directly
   causal (measured from realized price paths). Coverage outcomes (already
   ahead / never followed / measurable follow) are stratified by jump size in
   tiers of [1x, 2x, 4x, 8x) * --event-bps, so the exploitability of the window
   can be read per regime.
3. StandX own-feed jump response: for jumps detected on StandX's OWN series,
   how fast StandX's mark itself traverses 50% / 90% of the move. This prices
   an "own-feed fast cancel": how much of the move is still ahead of you once
   your own feed starts moving. StandX's mark updates at a slower cadence than
   the leader feed, so its jump scan uses a wider window (--own-window-ms,
   default 3x --event-window-ms).

HONEST CAVEATS (also printed at the end):
- The common local clock carries a FIXED differential-network-latency offset:
  the RTT from the recording host to StandX vs to Hyperliquid differs. The
  VARIABLE part (event-response spread, correlation shape) is robust; the
  ABSOLUTE offset is biased by that differential. Record from the SAME host /
  region as the maker for the number to be representative.
- Resolution floor: Hyperliquid activeAssetCtx (~0.5s/block) and StandX update
  cadence bound the smallest trustworthy lag. Treat sub-0.5s point estimates as
  "below resolution", not zero.
- Single window, single symbol. Re-record per symbol.
"""
import argparse
import bisect
import json
from statistics import mean, median


def load(path, standx_field, leader_field):
    """Return (standx, hyper): each a sorted list of (t_ms, price). Each source
    reads its own price field; lines where that field is null are skipped."""
    fields = {"standx": standx_field, "hyperliquid": leader_field}
    series = {"standx": [], "hyperliquid": []}
    with open(path) as handle:
        for line in handle:
            line = line.strip()
            if not line:
                continue
            try:
                rec = json.loads(line)
            except Exception:
                continue
            source = rec.get("source")
            if source not in series:
                continue
            price = rec.get(fields[source])
            if price is None:
                continue
            t = rec.get("local_recv_ms")
            if t is None:
                continue
            series[source].append((int(t), float(price)))
    for source in series:
        series[source].sort(key=lambda row: row[0])
    return series["standx"], series["hyperliquid"]


def resample(series, grid_ms, t0, t1):
    """Forward-fill a (t_ms, price) series onto a fixed grid. Returns list of
    price (or None before the first sample)."""
    grid = list(range(t0, t1 + 1, grid_ms))
    times = [row[0] for row in series]
    out = []
    for g in grid:
        i = bisect.bisect_right(times, g) - 1
        out.append(series[i][1] if i >= 0 else None)
    return grid, out


def increments(prices):
    """First differences; None where either endpoint is missing."""
    out = []
    for a, b in zip(prices, prices[1:]):
        out.append(b - a if (a is not None and b is not None) else None)
    return out


def pearson(xs, ys):
    pairs = [(x, y) for x, y in zip(xs, ys) if x is not None and y is not None]
    if len(pairs) < 3:
        return None, len(pairs)
    mx = mean(p[0] for p in pairs)
    my = mean(p[1] for p in pairs)
    sxy = sum((p[0] - mx) * (p[1] - my) for p in pairs)
    sxx = sum((p[0] - mx) ** 2 for p in pairs)
    syy = sum((p[1] - my) ** 2 for p in pairs)
    if sxx <= 0 or syy <= 0:
        return None, len(pairs)
    return sxy / (sxx**0.5 * syy**0.5), len(pairs)


def cross_correlation(standx, hyper, grid_ms, max_lag_ms):
    """Scan integer lags; StandX increments vs Hyperliquid increments shifted by
    `lag` grid steps. Positive lag = StandX lags Hyperliquid."""
    t0 = max(standx[0][0], hyper[0][0])
    t1 = min(standx[-1][0], hyper[-1][0])
    if t1 - t0 < grid_ms * 4:
        return None
    _, sx = resample(standx, grid_ms, t0, t1)
    _, hy = resample(hyper, grid_ms, t0, t1)
    sx_incr = increments(sx)
    hy_incr = increments(hy)
    max_steps = max(1, max_lag_ms // grid_ms)
    best = None
    curve = []
    for lag in range(-max_steps, max_steps + 1):
        # StandX increment at t vs Hyperliquid increment at t-lag.
        if lag >= 0:
            xs = sx_incr[lag:]
            ys = hy_incr[: len(hy_incr) - lag] if lag > 0 else hy_incr
        else:
            xs = sx_incr[: len(sx_incr) + lag]
            ys = hy_incr[-lag:]
        r, n = pearson(xs, ys)
        if r is None:
            continue
        curve.append((lag * grid_ms, r, n))
        if best is None or r > best[1]:
            best = (lag * grid_ms, r, n)
    return best, curve


def detect_jumps(series, event_bps, window_ms):
    """Yield (t0, p0, p1, move_bps) for the first later sample within
    `window_ms` that moves >= event_bps from each anchor sample. Detection
    semantics are shared by the leader and the own-feed scan so jump sets are
    comparable."""
    n = len(series)
    jumps = []
    for i in range(n):
        t0, p0 = series[i]
        k = i + 1
        while k < n and series[k][0] - t0 <= window_ms:
            t1, p1 = series[k]
            move_bps = (p1 / p0 - 1.0) * 1e4
            if abs(move_bps) >= event_bps:
                jumps.append((t0, p0, p1, move_bps))
                break
            k += 1
    return jumps


def event_response(standx, hyper, event_bps, window_ms, cover_frac):
    """For each sharp Hyperliquid jump, measure ms until StandX covers
    `cover_frac` of the move in the same direction. Returns a list of
    (abs_move_bps, outcome, follow_ms) with outcome in
    "already" | "never" | "follow" (follow_ms set only for "follow")."""
    st_times = [row[0] for row in standx]
    st_px = [row[1] for row in standx]

    def standx_at(t):
        i = bisect.bisect_right(st_times, t) - 1
        return st_px[i] if i >= 0 else None

    events = []
    follow_ms_limit = max(window_ms * 4, 8000)

    for t0, p0, p1, move_bps in detect_jumps(hyper, event_bps, window_ms):
        direction = 1.0 if p1 > p0 else -1.0
        target = p0 + cover_frac * (p1 - p0)
        s0 = standx_at(t0)
        if s0 is not None and direction * (s0 - target) >= 0:
            events.append((abs(move_bps), "already", None))
            continue
        # find first StandX sample at/after t0 that reaches target
        idx = bisect.bisect_left(st_times, t0)
        hit = None
        while idx < len(standx) and st_times[idx] - t0 <= follow_ms_limit:
            if direction * (st_px[idx] - target) >= 0:
                hit = st_times[idx] - t0
                break
            idx += 1
        if hit is None:
            events.append((abs(move_bps), "never", None))
        else:
            events.append((abs(move_bps), "follow", hit))
    return events


def own_jump_response(series, event_bps, window_ms, fracs=(0.5, 0.9)):
    """Detect jumps on a single (StandX own) series and measure, per jump, the
    ms from the jump's first sample until the series itself covers each
    fraction of the move. Prices an own-feed fast cancel: how much traverse
    time your own feed gives you once it starts moving.
    Returns (jump_sizes, {frac: [ms, ...]})."""
    times = [row[0] for row in series]
    px = [row[1] for row in series]
    n = len(series)
    by_frac = {f: [] for f in fracs}
    sizes = []
    for t0, p0, p1, move_bps in detect_jumps(series, event_bps, window_ms):
        direction = 1.0 if p1 > p0 else -1.0
        sizes.append(abs(move_bps))
        i0 = bisect.bisect_left(times, t0)
        for f in fracs:
            target = p0 + f * (p1 - p0)
            idx = i0
            while idx < n:
                if direction * (px[idx] - target) >= 0:
                    by_frac[f].append(times[idx] - t0)
                    break
                idx += 1
    return sizes, by_frac


def quantile(xs, q):
    if not xs:
        return None
    s = sorted(xs)
    pos = q * (len(s) - 1)
    lo = int(pos)
    hi = min(lo + 1, len(s) - 1)
    return s[lo] + (s[hi] - s[lo]) * (pos - lo)


def load_standx_mark_book(path, pair_window_ms=5000):
    """Pair each StandX `mark` with the most recent `best_bid`/`best_ask`.

    StandX puts `mark` on the `price` channel and `best_bid`/`best_ask` on the
    `depth_book` channel — separate NDJSON records. Scans all StandX records in
    time order, keeps the latest book quote, and emits one (t, mark, bid, ask)
    row for each mark whose most recent book quote is no older than
    `pair_window_ms`. Returns a list of (t_ms, mark, bid, ask).
    """
    rows = []
    book = None  # (t, best_bid, best_ask); possibly no depth yet
    with open(path) as handle:
        for line in handle:
            line = line.strip()
            if not line:
                continue
            try:
                rec = json.loads(line)
            except Exception:
                continue
            if rec.get("source") != "standx":
                continue
            t = rec.get("local_recv_ms")
            if t is None:
                continue
            t = int(t)
            bid = rec.get("best_bid")
            ask = rec.get("best_ask")
            mark = rec.get("mark")
            if bid is not None and ask is not None:
                book = (t, float(bid), float(ask))
                if mark is None:
                    continue  # pure depth update, nothing to pair yet
            elif mark is not None:
                mark = float(mark)
                if book is not None and t - book[0] <= pair_window_ms:
                    rows.append((t, mark, book[1], book[2]))
                # else: mark too stale vs latest book -> skip
            # else: neither a fresh book nor a mark -> ignore
    rows.sort(key=lambda r: r[0])
    return rows


def mark_best_spread(rows):
    """From (t, mark, bid, ask) rows compute mark-vs-book distances (bps).

    For each row: mid=(bid+ask)/2
      signed  = (mark/mid - 1)*1e4      # mark relative to book mid
      touch   = min(|mark-bid|, |ask-mark|)/mark*1e4  # to near best quote
      half    = (ask-bid)/2/mid*1e4      # maker-facing half spread
    Returns (signed, touch, half) lists of bps.
    """
    signed, touch, half = [], [], []
    for _, mark, bid, ask in rows:
        if not (mark > 0 and bid > 0 and ask > 0):
            continue
        mid = (bid + ask) / 2.0
        signed.append((mark / mid - 1.0) * 1e4)
        touch.append(min(abs(mark - bid), abs(ask - mark)) / mark * 1e4)
        half.append((ask - bid) / 2.0 / mid * 1e4)
    return signed, touch, half


def spread_summary(name, xs, tick_bps=None):
    """Print quantiles of a bps list plus tick- and 10bps-binned counts."""
    if not xs:
        print(f"  {name}: no samples")
        return
    print(f"  {name} (n={len(xs)}):")
    qs = {q: quantile(xs, q) for q in (0.05, 0.25, 0.50, 0.75, 0.90, 0.95, 0.99)}
    line = "   ".join(f"p{int(q*100)}={v:.1f}bps" for q, v in qs.items())
    print(f"    min={min(xs):.1f}  max={max(xs):.1f}  mean={mean(xs):.1f}  {line}")
    if tick_bps:
        hist = {}
        for x in xs:
            tc = int(abs(x) // tick_bps)
            hist[tc] = hist.get(tc, 0) + 1
        bars = " ".join(f"{tc}t:{n}" for tc, n in sorted(hist.items()))
        print(f"    tick histogram ({tick_bps:g}bps/tick): {bars}")
    bins = {}
    for x in xs:
        b = int(abs(x) // 10)
        bins[b] = bins.get(b, 0) + 1
    bars = " ".join(f"[{b*10}-{(b+1)*10})bp:{n}" for b, n in sorted(bins.items()))
    print(f"    10bps bins: {bars}")


def fmt_stats(xs):
    return (f"median={median(xs):.0f}ms  "
            f"p25={quantile(xs, 0.25):.0f}ms  "
            f"p75={quantile(xs, 0.75):.0f}ms  "
            f"mean={mean(xs):.0f}ms")


def main():
    ap = argparse.ArgumentParser(description="Measure StandX price lag vs Hyperliquid.")
    ap.add_argument("path")
    ap.add_argument("--standx-field", default="mark",
                    choices=["mark", "mid", "index"],
                    help="StandX price field (the price we quote against)")
    ap.add_argument("--leader-field", default="mid",
                    choices=["mark", "mid", "index"],
                    help="Hyperliquid leader field (midPx is the fastest public price)")
    ap.add_argument("--grid-ms", type=int, default=250)
    ap.add_argument("--max-lag-ms", type=int, default=4000)
    ap.add_argument("--event-bps", type=float, default=8.0)
    ap.add_argument("--event-window-ms", type=int, default=2000)
    ap.add_argument("--cover-frac", type=float, default=0.5)
    ap.add_argument("--own-window-ms", type=int, default=None,
                    help="jump-scan window for the StandX own-feed statistic; "
                         "defaults to 3x --event-window-ms because StandX's mark "
                         "cadence is slower than the leader feed's")
    ap.add_argument("--tick-bps", type=float, default=None,
                    help="tick size in bps for the mark-best tick histogram "
                         "(e.g. HYPE 0.5bp, XAU 1bp). Omit to skip tick binning.")
    args = ap.parse_args()

    standx, hyper = load(args.path, args.standx_field, args.leader_field)
    print(f"loaded: standx={len(standx)} (field={args.standx_field}) "
          f"hyperliquid={len(hyper)} (field={args.leader_field}) samples")

    # Section 4 needs only StandX mark+book — run before the both-sources guard
    # so XAU/XAG (no Hyperliquid leg) still get the denominator analysis.
    print("\n== 4. StandX mark vs book (mark-best spread denominator) ==")
    rows = load_standx_mark_book(args.path)
    if len(rows) < 5:
        print("  not enough paired mark+book samples; need a longer window.")
    else:
        n = len(rows)
        print(f"  paired mark+book rows: {n} "
              "(mark sampled against a book quote paired from the depth channel)")
        signed, touch, half = mark_best_spread(rows)
        spread_summary("  signed mark-vs-mid anchor (bps, + = mark above mid)",
                       signed, tick_bps=args.tick_bps)
        spread_summary("  distance mark -> near best quote (bps)",
                       touch, tick_bps=args.tick_bps)
        spread_summary("  maker-facing half spread (bps)",
                       half, tick_bps=args.tick_bps)

    if len(standx) < 5 or len(hyper) < 5:
        print("\nnot enough samples on both sources for cross-venue lag; "
              "record a longer window.")
        return
    span_s = (min(standx[-1][0], hyper[-1][0]) - max(standx[0][0], hyper[0][0])) / 1000.0
    print(f"overlapping span: {span_s:.1f}s")

    print("\n== 1. Cross-correlation of price increments ==")
    xc = cross_correlation(standx, hyper, args.grid_ms, args.max_lag_ms)
    if xc is None or xc[0] is None:
        print("  insufficient overlap for cross-correlation.")
    else:
        best, curve = xc
        lag_ms, r, n = best
        sign = "StandX LAGS Hyperliquid" if lag_ms > 0 else (
            "StandX LEADS Hyperliquid" if lag_ms < 0 else "no offset")
        print(f"  peak correlation r={r:.3f} at lag={lag_ms:+d}ms ({sign}), n={n}")
        # show the correlation near the peak for shape
        near = [c for c in curve if abs(c[0] - lag_ms) <= 3 * args.grid_ms]
        print("  nearby: " + ", ".join(f"{c[0]:+d}ms:{c[1]:.2f}" for c in near))
        if abs(lag_ms) < 500:
            print("  NOTE: |lag| < 500ms is below the feed-cadence resolution floor;"
                  " treat as 'no resolvable lead'.")

    print("\n== 2. Event response (Hyperliquid jumps -> StandX follow time) ==")
    events = event_response(
        standx, hyper, args.event_bps, args.event_window_ms, args.cover_frac)
    total = len(events)
    already = sum(1 for _, o, _ in events if o == "already")
    never = sum(1 for _, o, _ in events if o == "never")
    responses = [ms for _, o, ms in events if o == "follow"]
    print(f"  jumps >= {args.event_bps:g}bps within {args.event_window_ms}ms: {total}")
    if total:
        print(f"    StandX already ahead at jump: {already}")
        print(f"    StandX never covered {args.cover_frac:.0%} within follow window: {never}")
    if responses:
        print(f"    follow-time to cover {args.cover_frac:.0%} of the move "
              f"(n={len(responses)}):")
        print(f"      {fmt_stats(responses)}")
    else:
        print("    no measurable follow events (need a longer / more volatile window).")

    # coverage stratified by jump size, in tiers of [1x, 2x, 4x, 8x) * event-bps
    if total:
        print("  coverage by jump size:")
        print("    tier(bps)   jumps  already  never  follow  follow-median")
        edges = [args.event_bps * m for m in (1, 2, 4, 8)]
        for t_i in range(len(edges)):
            lo = edges[t_i]
            hi = edges[t_i + 1] if t_i + 1 < len(edges) else None
            tier = [e for e in events
                    if e[0] >= lo and (hi is None or e[0] < hi)]
            if not tier:
                continue
            t_jumps = len(tier)
            t_already = sum(1 for _, o, _ in tier if o == "already")
            t_never = sum(1 for _, o, _ in tier if o == "never")
            t_follow = [ms for _, o, ms in tier if o == "follow"]
            label = f"{lo:g}-{hi:g}" if hi is not None else f">={lo:g}"
            med = f"{median(t_follow):.0f}ms" if t_follow else "-"
            print(f"    {label:<10} {t_jumps:>6} {t_already:>8} {t_never:>6}"
                  f" {len(t_follow):>7}  {med:>13}")

    print("\n== 3. StandX own-feed jump response (fast-cancel pricing) ==")
    own_window_ms = args.own_window_ms or args.event_window_ms * 3
    sizes, by_frac = own_jump_response(
        standx, args.event_bps, own_window_ms)
    print(f"  StandX own jumps >= {args.event_bps:g}bps within "
          f"{own_window_ms}ms: {len(sizes)}")
    for f in sorted(by_frac):
        xs = by_frac[f]
        if xs:
            print(f"    time for own mark to cover {f:.0%} of its move (n={len(xs)}):")
            print(f"      {fmt_stats(xs)}")
    if not sizes:
        print("    no own-feed jumps (need a longer / more volatile window).")

    print("\n== Caveats ==")
    print("  - Absolute lag carries a fixed differential-network-latency bias "
          "(host->StandX vs host->Hyperliquid). Variable part is robust; run on "
          "the maker's host/region.")
    print("  - Resolution floor set by feed cadence (~0.5s). Sub-0.5s estimates "
          "are 'below resolution', not zero.")
    print("  - Single window / single symbol; re-record per symbol.")


if __name__ == "__main__":
    main()
