#!/usr/bin/env python3
"""Pooled markout / toxicity comparison for stage2 A/B maker arms. Read-only.

Usage:
    python3 scripts/maker_markout_ab.py ARM.ndjson [ARM.ndjson ...]

Each arm file is a maker NDJSON log whose name contains "baseline" or
"candidate" (treatment is inferred from the filename). Prints per-arm
fill/capture/markout stats, pooled per-treatment aggregates, a
matched-condition cut (candidate tier-0 fills vs baseline), and candidate
fills broken down by adaptive-spread tier.

Attribution sections (pooled across all arms, treatment shown for reference):

- Pre-fill signed drift: for each passive fill, drift_in(w) =
  (mark_at_fill - mark(t_fill - w)) * side_sign / mark_at_fill in bps for
  w in 5/15/30s. drift_in < 0 means the pre-fill mark drift was in the
  "run-over" direction (mark fell into our bid / rose into our ask — the
  aggressor flow pushed price the same way). If most fills show drift_in < 0
  and those fills carry the negative markout, quotes are systematically run
  over by drift and drift-aware asymmetric quoting targets the cause.
- Resting-order age at fill: per-side replay of place / cancel /
  place_rejected_async events attributes each passive fill to the placement
  of the order it matched (single level per side; price must match exactly).
  Age is wall-clock seconds from place intent to fill. If toxicity worsens
  with age, faster requote (refresh tuning, a pure config change) has value.
  The hold-event mean age_cycles x cycle period gives the time-weighted
  resting-age benchmark: fills older than that benchmark mean old quotes are
  disproportionately the ones getting hit.
- Quote staleness at fill: each attributed fill is classified as "fresh"
  (drift since placement, toward the quote, below the tier's refresh_bps at
  fill time — the strategy still considered the quote good), "stale" (drift
  >= refresh_bps but no cancel intent logged yet, i.e. the fill landed in
  the <=1-cycle detection-lag window), or "in_flight" (the fill matched an
  order whose cancel intent was already logged within 30s). refresh_bps is
  taken per tier from the frozen stage2 config (4/5/6 bps for t0/t1/t2). A
  high stale+in_flight share would point at cancel/detection speed as the
  lever; a high fresh share means fills hit quotes the strategy stood
  behind, i.e. the quote center itself lacks short-term drift information.
- Signal pricing (offline, open-loop): per fill, drift_place(T) is the signed
  drift over the T seconds BEFORE PLACEMENT (endpoint = placement ref_mark) —
  the signal the strategy could actually act on at quote time. Predictive
  power is reported as pearson/spearman vs mo30 per T. The counterfactual
  front-run estimate shifts the quote center by o = min(cap, k *
  max(0, -drift_place(T))) bps and asks per fill: still filled (30s post-fill
  adverse mark excursion >= o, mark-touch proxy) => gain +o bps; avoided =>
  gain -(cap+mo30) bps. Combos are tuned ONLY on the training window (first
  4 arms chronologically) and reported frozen on the validation window (last
  2 arms). This step can only prove signal existence and rough magnitude:
  open-loop replay cannot generate counterfactual fills, ignores re-quote
  chains/inventory/uptime effects, and the mark-touch proxy deviates from
  real venue touches by the capture residual (~5bps). The real verdict
  requires live A/B.

Markout convention: (mark(t+h) - mark(t_fill)) * side_sign / mark * 1e4 bps,
using cycle_summary marks (~2.45s cadence) as the mark series; the actual
horizon is the first cycle at or after t_fill + h. The runner's own
performance.markout_* fields instead measure from the fill price (i.e. they
include capture), so the two conventions differ by ~capture_bps.
"""
import bisect
import json
import random
import sys
from collections import Counter, defaultdict
from datetime import datetime, timezone
from statistics import mean, median, stdev

HORIZONS = [1.0, 5.0, 30.0]
# attribution_rows computes the full curve; legacy pooled tables keep HORIZONS.
MO_CURVE_HORIZONS = [1.0, 5.0, 30.0, 60.0, 120.0, 300.0]
DRIFT_WINDOWS = [5.0, 15.0, 30.0]
AGE_BUCKETS = [(0, 5), (5, 15), (15, 30), (30, 60), (60, 120), (120, None)]
# refresh_bps per adaptive-spread tier, frozen stage2 config
# (examples/maker-stage2-hype-baseline.toml; baseline arms always tier 0).
REFRESH_BY_TIER = {0: 4.0, 1: 5.0, 2: 6.0}
IN_FLIGHT_WINDOW_S = 30.0
DRIFT_PLACE_WINDOWS = [5.0, 10.0, 15.0, 30.0, 60.0]
GRID_K = [0.0, 0.25, 0.5, 0.75, 1.0, 1.5]
GRID_CAP = [2.0, 4.0, 8.0, 16.0]
AVOID_LIMIT = 0.20  # roadmap: fill-count drop >20% needs separate SIP-5A review


def parse_ts(s):
    return datetime.fromisoformat(s.replace("Z", "+00:00")).timestamp()


def load_arm(path):
    cycles, fills, timeline, hold_ages = [], [], [], []
    last_perf, pnl_last = None, None
    with open(path) as f:
        for line in f:
            try:
                d = json.loads(line)
            except Exception:
                continue
            a = d.get("action")
            if a == "cycle_summary":
                try:
                    cycles.append((parse_ts(d["ts"]), float(d["mark"]),
                                   d.get("adaptive_spread_tier", 0)))
                except Exception:
                    pass
                last_perf = d.get("performance")
                pnl_last = d.get("pnl")
            elif a == "fill":
                fills.append(d)
                timeline.append((a, d))
            elif a in ("place", "cancel", "place_rejected_async"):
                timeline.append((a, d))
            elif a == "hold":
                try:
                    hold_ages.append(float(d["age_cycles"]))
                except Exception:
                    pass
    cycles.sort()
    return cycles, fills, last_perf, pnl_last, timeline, hold_ages


def arm_rows(cycles, fills):
    """Per passive fill: (cap_bps, mo1, mo5, mo30, tier, side, qty)."""
    ts_list = [c[0] for c in cycles]
    rows = []
    for f in fills:
        if f.get("role") != "passive_maker":
            continue
        t = parse_ts(f["ts"])
        price = float(f["price"])
        sign = 1.0 if f["side"] == "buy" else -1.0
        i = bisect.bisect_left(ts_list, t)
        if i >= len(cycles):
            continue
        mark0 = cycles[i][1]
        tier = cycles[max(0, i - 1)][2]
        cap = (mark0 - price) * sign / price * 1e4
        mo = {}
        for h in HORIZONS:
            j = bisect.bisect_left(ts_list, t + h)
            mo[h] = (cycles[j][1] - mark0) * sign / mark0 * 1e4 if j < len(cycles) else None
        rows.append((cap, mo[1.0], mo[5.0], mo[30.0], tier, f["side"], float(f["qty"])))
    return rows


def attribution_rows(cycles, timeline):
    """Replay per-side open orders over the arm timeline.

    Per passive fill returns dicts with cap/mo (same convention as arm_rows),
    pre-fill signed drift per DRIFT_WINDOWS (endpoint = fill's mark_at_fill,
    falling back to the first cycle mark), resting-order age in seconds, and a
    staleness class: "fresh" (drift since placement toward the quote below the
    tier's refresh_bps at fill time), "stale" (drift >= refresh_bps, cancel
    not yet logged — hold logs show cancels fire promptly once drift >=
    refresh at a cycle, so this is at most a ~1-cycle detection-lag window),
    or "in_flight" (fill matched an order whose cancel intent was logged
    within IN_FLIGHT_WINDOW_S before the fill).

    Age attribution keeps a per-side list of open placements because log order
    within a cycle is not causal: the fill that triggers a re-place is logged
    AFTER the new place intent. A fill matches the OLDEST open placement with
    an exact (side, price) match and consumes it (fills here are full-size);
    cancels move the NEWEST price-matching placement to the cancelled list,
    rejects just drop it. Fills after the last cycle are skipped (same rule
    as arm_rows).
    """
    ts_list = [c[0] for c in cycles]
    open_orders = defaultdict(list)  # side -> [(placed_epoch, price, ref_mark)]
    cancelled = defaultdict(list)    # side -> [(cancel_epoch, price, placed_epoch, ref_mark)]
    rows, matched, unmatched = [], 0, 0

    def pop_matching(side, price, keep):
        orders = open_orders.get(side)
        if not orders:
            return
        for k in range(len(orders) - 1, -1, -1):
            if price is not None and abs(orders[k][1] - price) < 1e-6:
                keep.append(orders.pop(k))
                return
        if price is None:
            keep.append(orders.pop())

    for kind, d in timeline:
        side = d.get("side")
        if kind == "place":
            try:
                open_orders[side].append(
                    (parse_ts(d["ts"]), float(d["price"]), float(d["mark"])))
            except Exception:
                pass
        elif kind == "cancel":
            try:
                px = float(d["price"])
            except Exception:
                px = None
            popped = []
            pop_matching(side, px, popped)
            for pt, opx, rm in popped:
                cancelled[side].append((parse_ts(d["ts"]), opx, pt, rm))
        elif kind == "place_rejected_async":
            try:
                px = float(d["price"])
            except Exception:
                px = None
            pop_matching(side, px, [])
        elif kind == "fill":
            if d.get("role") != "passive_maker":
                continue
            try:
                t = parse_ts(d["ts"])
                price = float(d["price"])
            except Exception:
                continue
            sign = 1.0 if d.get("side") == "buy" else -1.0
            i = bisect.bisect_left(ts_list, t)
            if i >= len(cycles):
                continue
            mark0 = cycles[i][1]
            tier = cycles[max(0, i - 1)][2]
            try:
                maf = float(d.get("mark_at_fill"))
            except Exception:
                maf = mark0
            drift = {}
            for w in DRIFT_WINDOWS:
                j = bisect.bisect_right(ts_list, t - w) - 1
                drift[w] = (maf - cycles[j][1]) * sign / maf * 1e4 if j >= 0 and maf else None
            mo = {}
            for h in MO_CURVE_HORIZONS:
                j = bisect.bisect_left(ts_list, t + h)
                mo[h] = (cycles[j][1] - mark0) * sign / mark0 * 1e4 if j < len(cycles) else None
            thr = REFRESH_BY_TIER.get(tier, REFRESH_BY_TIER[0])
            age = cls = stale_bps = stale_age = None
            pt_sig = rm_sig = None
            hit = None
            orders = open_orders.get(side, [])
            for k, (pt, px, rm) in enumerate(orders):
                if abs(px - price) < 1e-6:
                    hit = (pt, rm)
                    orders.pop(k)
                    break
            if hit is not None:
                pt, rm = hit
                pt_sig, rm_sig = pt, rm
                age = max(0.0, t - pt)
                matched += 1
                stale_bps = (rm - maf) * sign / rm * 1e4 if maf and rm else None
                if stale_bps is not None:
                    cls = "stale" if stale_bps >= thr else "fresh"
                    if cls == "stale":
                        # time since the quote continuously reads stale on the
                        # cycle series; None = crossed intra-cycle (< 1 cycle)
                        j = i - 1
                        while j >= 0 and t - ts_list[j] <= 300.0:
                            db = (rm - cycles[j][1]) * sign / rm * 1e4
                            if db < thr:
                                break
                            stale_age = t - ts_list[j]
                            j -= 1
            else:
                for ct, px, pt, rm in reversed(cancelled.get(side, [])):
                    if abs(px - price) < 1e-6 and 0.0 <= t - ct <= IN_FLIGHT_WINDOW_S:
                        cls = "in_flight"
                        pt_sig, rm_sig = pt, rm
                        age = max(0.0, t - pt)
                        stale_age = t - ct
                        stale_bps = (rm - maf) * sign / rm * 1e4 if maf and rm else None
                        matched += 1
                        break
                if cls is None:
                    unmatched += 1
            # signal at placement time (causal): drift over w seconds before
            # the order was placed, endpoint = its ref_mark; None if the
            # placement is unknown or the lookback runs past arm start
            drift_place = {}
            for w in DRIFT_PLACE_WINDOWS:
                drift_place[w] = None
                if pt_sig is not None and rm_sig:
                    j = bisect.bisect_right(ts_list, pt_sig - w) - 1
                    if j >= 0:
                        drift_place[w] = (rm_sig - cycles[j][1]) * sign / rm_sig * 1e4
            # 30s post-fill adverse mark excursion from mark_at_fill; None if
            # the horizon is censored by arm end (same rule as mo30)
            exc = None
            if maf:
                end = bisect.bisect_left(ts_list, t + 30.0)
                j0 = bisect.bisect_right(ts_list, t)
                if end < len(cycles) and j0 <= end:
                    seg = [c[1] for c in cycles[j0:end + 1]]
                    if seg:
                        adverse = min(seg) if sign > 0 else max(seg)
                        exc = (maf - adverse) * sign / maf * 1e4
            rows.append(dict(side=d["side"], tier=tier, ts=t,
                             cap=(mark0 - price) * sign / price * 1e4,
                             mo=mo, drift=drift, age=age, cls=cls,
                             stale_bps=stale_bps, stale_age=stale_age,
                             drift_place=drift_place, exc=exc))
    return rows, matched, unmatched


def print_markout_curve(rows):
    print("\n=== markout curve after fill (pooled passive fills; n shrinks via arm-end censoring) ===")
    print("signed mark move after fill, bps; still falling at longer horizon = continued bleeding,")
    print("recovering toward zero = mean reversion (holding is free, exiting realizes the loss)")
    for h in MO_CURVE_HORIZONS:
        xs = [r["mo"][h] for r in rows if r["mo"].get(h) is not None]
        if not xs:
            continue
        buys = [r["mo"][h] for r in rows if r["side"] == "buy" and r["mo"].get(h) is not None]
        sells = [r["mo"][h] for r in rows if r["side"] == "sell" and r["mo"].get(h) is not None]
        neg = sum(1 for x in xs if x < 0) / len(xs) * 100
        bs = f"buy {mean(buys):+6.2f}(n{len(buys):3d})" if buys else "buy n/a"
        ss = f"sell {mean(sells):+6.2f}(n{len(sells):3d})" if sells else "sell n/a"
        print(f" mo{h:>4.0f}s: n{len(xs):3d} mean{mean(xs):+6.2f} med{median(xs):+6.2f} "
              f"neg%{neg:3.0f} | {bs} {ss}")


def pearson(xs, ys):
    mx, my = mean(xs), mean(ys)
    cov = sum((x - mx) * (y - my) for x, y in zip(xs, ys))
    vx = sum((x - mx) ** 2 for x in xs)
    vy = sum((y - my) ** 2 for y in ys)
    return cov / ((vx * vy) ** 0.5) if vx > 0 and vy > 0 else 0.0


def ranks(xs):
    order = sorted(range(len(xs)), key=lambda i: xs[i])
    r = [0.0] * len(xs)
    i = 0
    while i < len(order):
        j = i
        while j + 1 < len(order) and xs[order[j + 1]] == xs[order[i]]:
            j += 1
        avg = (i + j) / 2.0 + 1.0
        for k in range(i, j + 1):
            r[order[k]] = avg
        i = j + 1
    return r


def spearman(xs, ys):
    return pearson(ranks(xs), ranks(ys))


def frontrun_eval(rows, T, k, cap):
    """Open-loop counterfactual: quote center shifted by o = min(cap,
    k*max(0,-drift_place(T))) bps in the adverse direction. Still-filled iff
    the 30s post-fill adverse mark excursion >= o (mark-touch proxy).
    Gain: still-filled +o bps; avoided -(cap+mo30) bps.
    Returns (n_scored, n_affected, n_avoided, gain_sum_bps)."""
    n_scored = n_aff = n_av = 0
    gain = 0.0
    for r in rows:
        mo30 = r["mo"][30.0]
        if mo30 is None or r["exc"] is None:
            continue
        n_scored += 1
        dp = r["drift_place"].get(T)
        o = min(cap, k * max(0.0, -dp)) if dp is not None else 0.0
        if o <= 0.0:
            continue
        n_aff += 1
        if r["exc"] >= o:
            gain += o
        else:
            n_av += 1
            gain += -(r["cap"] + mo30)
    return n_scored, n_aff, n_av, gain


def frontrun_control(rows, o):
    """Unconditional-offset control: every quote shifted by o bps regardless
    of the drift signal. Isolates the mechanical credit any widening earns in
    this accounting — the drift-conditioned combos must BEAT this to claim
    signal value. Returns (n_scored, n_avoided, gain_sum_bps)."""
    n_scored = n_av = 0
    gain = 0.0
    for r in rows:
        mo30 = r["mo"][30.0]
        if mo30 is None or r["exc"] is None:
            continue
        n_scored += 1
        if r["exc"] >= o:
            gain += o
        else:
            n_av += 1
            gain += -(r["cap"] + mo30)
    return n_scored, n_av, gain


def print_signal_pricing(rows):
    arm_ts = sorted({r["arm_ts"] for r in rows})
    train_ts, val_ts = set(arm_ts[:4]), set(arm_ts[4:])
    train = [r for r in rows if r["arm_ts"] in train_ts]
    val = [r for r in rows if r["arm_ts"] in val_ts]
    print("\n=== signal pricing: drift-at-placement vs mo30 ===")
    print(f"train = first {len(train_ts)} arms chronologically (n={len(train)} fills), "
          f"validation = last {len(val_ts)} (n={len(val)}), strictly temporal split")
    print("signal = signed drift(T) at placement, bps; negative = adverse "
          "(mark moved toward the quote side before the order was placed)")
    for T in DRIFT_PLACE_WINDOWS:
        out = []
        for part in (train, val):
            xs = [(r["drift_place"][T], r["mo"][30.0]) for r in part
                  if r["drift_place"].get(T) is not None and r["mo"][30.0] is not None]
            if len(xs) >= 10:
                out.append(f"n{len(xs):3d} r{pearson([x[0] for x in xs], [x[1] for x in xs]):+.2f} "
                           f"rho{spearman([x[0] for x in xs], [x[1] for x in xs]):+.2f}")
            else:
                out.append(f"n{len(xs):3d} (too few)")
        print(f" T={T:4.0f}s | train {out[0]} | val {out[1]}")
    T = 15.0
    xs = sorted(((r["drift_place"][T], r["mo"][30.0]) for r in train
                 if r["drift_place"].get(T) is not None and r["mo"][30.0] is not None))
    if xs:
        print(f"drift_place({T:.0f}s) quartiles on train (most adverse first):")
        q = max(1, len(xs) // 4)
        for qi in range(4):
            seg = xs[qi * q:(qi + 1) * q] if qi < 3 else xs[qi * q:]
            if seg:
                print(f"  Q{qi + 1}: n{len(seg):3d} drift{mean([s[0] for s in seg]):+6.2f} "
                      f"mo30{mean([s[1] for s in seg]):+6.2f}")
    print("\n=== signal pricing: center front-run counterfactual (grid tuned on train only) ===")
    print("offset o = min(cap, k*max(0,-drift_place(T))); still-filled iff 30s post-fill adverse")
    print("mark excursion >= o; gain: still-filled +o bps, avoided -(cap+mo30) bps")
    scored = []
    for T in DRIFT_PLACE_WINDOWS:
        for k in GRID_K:
            for cap in GRID_CAP:
                n, aff, av, g = frontrun_eval(train, T, k, cap)
                if n:
                    scored.append((g / n, T, k, cap, n, aff, av, g))
    ok = sorted((s for s in scored if s[2] > 0 and s[6] / s[4] <= AVOID_LIMIT), reverse=True)
    print(f"top combos on train (avoided <= {AVOID_LIMIT:.0%} of scored):")
    for gpn, T, k, cap, n, aff, av, g in ok[:8]:
        line = (f" T={T:4.0f} k={k:4.2f} cap={cap:4.0f}: train affected {aff / n:4.0%} "
                f"avoided {av / n:4.0%} gain {gpn:+5.2f}bps/fill")
        vn, vaff, vav, vg = frontrun_eval(val, T, k, cap)
        if vn:
            line += f" | val avoided {vav / vn:4.0%} gain {vg / vn:+5.2f}bps/fill"
        print(line)
    if scored:
        gpn, T, k, cap, n, aff, av, g = max(scored)
        print(f"unconstrained best on train: T={T:.0f} k={k:.2f} cap={cap:.0f} avoided {av / n:.0%} "
              f"gain {gpn:+.2f}bps/fill — degenerate 'quote less' optimum, rejected by the "
              f"{AVOID_LIMIT:.0%} avoidance rule")
    print("control (unconditional offset, no signal — drift combos must beat this):")
    for o in (1.0, 2.0, 4.0, 8.0):
        n, av, g = frontrun_control(train, o)
        line = f" o={o:4.0f}bps: train avoided {av / n:4.0%} gain {g / n:+5.2f}bps/fill"
        vn, vav, vg = frontrun_control(val, o)
        if vn:
            line += f" | val avoided {vav / vn:4.0%} gain {vg / vn:+5.2f}bps/fill"
        print(line)
    print("limitations: open-loop replay cannot create counterfactual fills; still-filled/avoided")
    print(" uses a mark-touch proxy (venue touches deviate from mark by the ~5bps capture residual);")
    print(" still-filled gain assumes the post-fill path is unchanged; re-quote chains after an")
    print(" avoided fill, inventory path, and uptime/fill-rate effects are ignored; grid argmax on")
    print(" train is optimistic — the val column is the honest estimate; the real verdict is live A/B.")


def block_bootstrap_sem(blocks, reps=1000, seed=20060731):
    """Clustered (block) bootstrap SEM of the mean.

    Fills inside one block share the same market episode, so resampling fills
    one by one understates the uncertainty of the mean. Resampling whole
    blocks (with replacement, same count as the input) preserves within-block
    dependence; the spread of replicate means is the honest SEM. `blocks` is a
    list of value-lists, one per block."""
    rng = random.Random(seed)
    flat = [v for block in blocks for v in block]
    if len(blocks) < 2 or len(flat) < 2:
        return float("nan")
    means = []
    for _ in range(reps):
        total = count = 0
        for _ in range(len(blocks)):
            block = blocks[rng.randrange(len(blocks))]
            total += sum(block)
            count += len(block)
        means.append(total / count)
    return stdev(means)


def print_clustering(rows):
    print("\n=== clustering: block bootstrap (design effect of same-episode fills) ===")
    print("fills cluster inside shared market episodes, so n nominal fills carry the")
    print("information of fewer independent ones. deff = (block_sem / nominal_sem)^2;")
    print("neff = n / deff is 'how many independent fills the sample is really worth'.")
    print("Blocks are per arm x wall-clock window; three window lengths shown because")
    print("clustering longer than the window still leaks into the estimate.")
    metrics = [
        ("cap", lambda r: r["cap"]),
        ("mo1s", lambda r: r["mo"].get(1.0)),
        ("mo5s", lambda r: r["mo"].get(5.0)),
        ("mo30s", lambda r: r["mo"].get(30.0)),
        ("drift15s", lambda r: r["drift"].get(15.0)),
    ]
    for t in ("baseline", "candidate"):
        arm_rows_t = [r for r in rows if r.get("treatment") == t]
        if not arm_rows_t:
            continue
        print(f"{t}:")
        for name, get in metrics:
            vals = [(r["arm_ts"], r["ts"], get(r)) for r in arm_rows_t
                    if r.get("ts") is not None and get(r) is not None]
            n = len(vals)
            if n < 30:
                print(f" {name:9s}: n{n} (too few)")
                continue
            xs = [v for _, _, v in vals]
            nominal = stdev(xs) / n ** 0.5
            line = (f" {name:9s} n{n:4d} mean{mean(xs):+6.2f} sd{stdev(xs):5.2f} "
                    f"sem_nom{nominal:5.2f}")
            for hours in (1, 2, 4):
                blocks = defaultdict(list)
                for arm_ts, ts, v in vals:
                    blocks[(arm_ts, int(ts // (hours * 3600)))].append(v)
                bl = list(blocks.values())
                if len(bl) < 8:
                    line += f" | {hours}h: {len(bl)} blocks (too few)"
                    continue
                sem_b = block_bootstrap_sem(bl)
                deff = (sem_b / nominal) ** 2
                line += f" | {hours}h sem{sem_b:5.2f} deff{deff:4.1f} neff{n / deff:4.0f}"
            print(line)


def stat(xs):
    xs = [x for x in xs if x is not None]
    if not xs:
        return "n/a"
    neg = sum(1 for x in xs if x < 0) / len(xs) * 100
    sd_s = sem_s = "   n/a"
    if len(xs) > 1:
        sd = stdev(xs)  # sample standard deviation (ddof=1)
        sd_s = f"{sd:5.2f}"
        sem_s = f"{sd / len(xs) ** 0.5:5.2f}"
    return f"mean{mean(xs):+6.2f} med{median(xs):+6.2f} sd{sd_s} sem{sem_s} neg%{neg:3.0f}"


def m(xs):
    xs = [x for x in xs if x is not None]
    return f"{mean(xs):+6.2f}" if xs else "   n/a"


def print_drift_attribution(rows):
    print("\n=== attribution: pre-fill signed drift (pooled passive fills) ===")
    print("drift_in = signed pre-fill mark drift, bps; <0 = run-over direction "
          "(mark drifted into our quote)")
    for w in DRIFT_WINDOWS:
        xs = [r for r in rows if r["drift"][w] is not None]
        if not xs:
            continue
        neg = [r for r in xs if r["drift"][w] < 0]
        pos = [r for r in xs if r["drift"][w] >= 0]
        pct = len(neg) / len(xs) * 100
        # share of total mo30 mass carried by run-over fills
        mo30_all = [r["mo"][30.0] for r in xs if r["mo"][30.0] is not None]
        mo30_neg = [r["mo"][30.0] for r in neg if r["mo"][30.0] is not None]
        mass = sum(mo30_neg) / sum(mo30_all) * 100 if mo30_all and sum(mo30_all) else 0.0
        print(f"lookback {w:4.0f}s: n{len(xs):3d} run-over {pct:4.0f}% "
              f"drift_in{m([r['drift'][w] for r in xs])} "
              f"| mo5 d<0 {m([r['mo'][5.0] for r in neg])} vs d>=0 {m([r['mo'][5.0] for r in pos])} "
              f"| mo30 d<0 {m([r['mo'][30.0] for r in neg])} vs d>=0 {m([r['mo'][30.0] for r in pos])} "
              f"| mo30 mass {mass:4.0f}%")
    # sizing cut: net per-fill economics by 15s drift bucket (net = cap + mo30)
    w = 15.0
    xs = [r for r in rows if r["drift"][w] is not None and r["mo"][30.0] is not None]
    if xs:
        print(f"sizing by drift{w:.0f}s bucket (net = cap + mo30, bps/fill):")
        for lo, hi in ((None, -4.0), (-4.0, -2.0), (-2.0, 0.0), (0.0, None)):
            ys = [r for r in xs if (lo is None or r["drift"][w] >= lo)
                  and (hi is None or r["drift"][w] < hi)]
            if not ys:
                continue
            label = f"  d<-4  " if lo is None else f"  d>=0  " if hi is None else f"  {lo:.0f}..{hi:.0f}"
            net = [r["cap"] + r["mo"][30.0] for r in ys]
            print(f"{label}: n{len(ys):3d} ({len(ys) / len(xs) * 100:4.1f}%) "
                  f"cap{m([r['cap'] for r in ys])} mo30{m([r['mo'][30.0] for r in ys])} "
                  f"net{m(net)}")


def print_staleness_attribution(rows):
    print("\n=== attribution: quote staleness at fill (pooled passive fills) ===")
    print("fresh = drift since placement < refresh_bps (strategy still stood behind the quote);")
    print("stale = drift >= refresh_bps, cancel not yet logged (<=1 cycle detection lag);")
    print("in_flight = fill matched an order whose cancel intent was already logged")
    tot = len(rows)
    mo30_total = sum(r["mo"][30.0] for r in rows if r["mo"][30.0] is not None)
    for cls in ("fresh", "stale", "in_flight", None):
        xs = [r for r in rows if r["cls"] == cls]
        if not xs:
            continue
        label = cls or "unmatched"
        mo5 = [r["mo"][5.0] for r in xs if r["mo"][5.0] is not None]
        mo30 = [r["mo"][30.0] for r in xs if r["mo"][30.0] is not None]
        neg5 = sum(1 for x in mo5 if x < 0) / len(mo5) * 100 if mo5 else 0.0
        mass = sum(mo30) / mo30_total * 100 if mo30_total else 0.0
        sa = [r["stale_age"] for r in xs if r["stale_age"] is not None]
        sa_s = f" stale_age med{median(sa):.0f}s max{max(sa):.0f}s" if sa else ""
        drift15 = [r["drift"][15.0] for r in xs]
        print(f"{label:9s}: n{len(xs):3d} ({len(xs) / tot * 100:4.1f}%) "
              f"cap{m([r['cap'] for r in xs])} mo5{m(mo5)} mo30{m(mo30)} "
              f"neg5%{neg5:3.0f} mo30mass{mass:5.1f}% drift15{m(drift15)}{sa_s}")


def print_age_attribution(rows, matched, unmatched, rest_age_s):
    print("\n=== attribution: resting-order age at fill (pooled passive fills) ===")
    tot = matched + unmatched
    if tot:
        print(f"age matched {matched}/{tot} ({matched / tot * 100:.0f}%)")
    if rest_age_s is not None:
        aged = [r["age"] for r in rows if r["age"] is not None]
        if aged:
            print(f"time-weighted resting age (hold events) {rest_age_s:.0f}s "
                  f"vs mean age at fill {mean(aged):.0f}s")
    for lo, hi in AGE_BUCKETS:
        xs = [r for r in rows if r["age"] is not None and r["age"] >= lo
              and (hi is None or r["age"] < hi)]
        if not xs:
            continue
        label = f"{lo:3d}-{hi:3d}s" if hi is not None else f"{lo:3d}s+   "
        mo5 = [r["mo"][5.0] for r in xs if r["mo"][5.0] is not None]
        neg5 = sum(1 for x in mo5 if x < 0) / len(mo5) * 100 if mo5 else 0.0
        print(f"age {label}: n{len(xs):3d} cap{m([r['cap'] for r in xs])} "
              f"mo5{m(mo5)} mo30{m([r['mo'][30.0] for r in xs])} "
              f"neg5%{neg5:3.0f} drift15{m([r['drift'][15.0] for r in xs])}")


def main(paths):
    arm_summary = []
    pooled = defaultdict(list)
    tier0_only = defaultdict(list)
    attr_rows = []
    attr_matched = attr_unmatched = 0
    hold_age_sum = hold_age_n = 0
    for path in paths:
        base = path.rsplit("/", 1)[-1]
        treatment = "baseline" if "baseline" in base else "candidate" if "candidate" in base else None
        if treatment is None:
            print(f"skip (no treatment in name): {base}")
            continue
        name = base.split(".")[0]
        cycles, fills, last_perf, pnl_last, timeline, hold_ages = load_arm(path)
        if not cycles:
            print(f"skip (no cycles): {base}")
            continue
        ts_list = [c[0] for c in cycles]
        dur_h = (ts_list[-1] - ts_list[0]) / 3600
        marks = [c[1] for c in cycles]
        tiers = Counter(c[2] for c in cycles)
        rows = arm_rows(cycles, fills)
        pooled[treatment].extend(rows)
        tier0_only[treatment].extend(r for r in rows if r[4] == 0)
        arows, am, au = attribution_rows(cycles, timeline)
        for r in arows:
            r["treatment"] = treatment
            r["arm_ts"] = ts_list[0]
        attr_rows.extend(arows)
        attr_matched += am
        attr_unmatched += au
        if hold_ages and len(cycles) > 1:
            period = (ts_list[-1] - ts_list[0]) / (len(cycles) - 1)
            hold_age_sum += sum(hold_ages) * period
            hold_age_n += len(hold_ages)
        mo5 = [r[2] for r in rows if r[2] is not None]
        mo30 = [r[3] for r in rows if r[3] is not None]
        caps = [r[0] for r in rows]
        arm_summary.append(dict(
            name=name, treatment=treatment, dur=dur_h, n=len(rows),
            rate=len(rows) / dur_h, cap=mean(caps) if caps else None,
            mo5=mean(mo5) if mo5 else None, mo30=mean(mo30) if mo30 else None,
            pnl=pnl_last if isinstance(pnl_last, (int, float)) else None,
            mark0=marks[0], mark1=marks[-1], mlo=min(marks), mhi=max(marks),
            tiers=dict(tiers), gross=(last_perf or {}).get("gross_spread_quote"),
            inv=(last_perf or {}).get("inventory_avg_abs_qty")))

    print("=== per-arm ===")
    for a in arm_summary:
        tier_s = f"  tiers {a['tiers']}" if a["treatment"] == "candidate" else ""
        pnl_s = f"{a['pnl']:+7.3f}" if a["pnl"] is not None else "    n/a"
        print(f"{a['name']}: {a['dur']:.1f}h fills {a['n']:3d} ({a['rate']:4.1f}/h) "
              f"cap{a['cap']:+5.2f} mo5{a['mo5']:+6.2f} mo30{a['mo30']:+6.2f} "
              f"pnl{pnl_s} gross{a['gross']:+5.3f} inv{a['inv']:.2f} "
              f"mark {a['mark0']:.2f}->{a['mark1']:.2f} [{a['mlo']:.2f},{a['mhi']:.2f}]{tier_s}")

    print("\n=== pooled by treatment (all passive fills) ===")
    for t in ("baseline", "candidate"):
        rows = pooled.get(t, [])
        if not rows:
            continue
        print(f"{t:9s}: n{len(rows):3d}  cap {stat([r[0] for r in rows])}")
        for i, h in enumerate(HORIZONS):
            print(f"{'':9s}  mo{h:>4.0f}s {stat([r[1 + i] for r in rows])}")
        arms = [a for a in arm_summary if a["treatment"] == t and a["pnl"] is not None]
        if arms:
            print(f"{'':9s}  sum pnl {sum(a['pnl'] for a in arms):+.3f}  "
                  f"sum gross_spread {sum(a['gross'] for a in arms if a['gross']):+.3f}")

    if attr_rows:
        print_clustering(attr_rows)

    if pooled.get("baseline") and tier0_only.get("candidate"):
        print("\n=== matched-condition: candidate tier0 fills vs all baseline fills ===")
        for label, rows in (("baseline(all)", pooled["baseline"]),
                            ("candidate(t0)", tier0_only["candidate"])):
            mo5 = [r[2] for r in rows if r[2] is not None]
            mo30 = [r[3] for r in rows if r[3] is not None]
            print(f"{label:14s}: n{len(rows):3d} cap{mean([r[0] for r in rows]):+5.2f} "
                  f"mo5{mean(mo5):+6.2f} mo30{mean(mo30):+6.2f}")

    if pooled.get("candidate"):
        print("\n=== candidate fills by tier ===")
        by_t = defaultdict(list)
        for r in pooled["candidate"]:
            by_t[r[4]].append(r)
        for t in sorted(by_t):
            rows = by_t[t]
            mo5 = [r[2] for r in rows if r[2] is not None]
            mo30 = [r[3] for r in rows if r[3] is not None]
            mo5_s = f"{mean(mo5):+6.2f}" if mo5 else "   n/a"
            mo30_s = f"{mean(mo30):+6.2f}" if mo30 else "   n/a"
            print(f"tier{t}: n{len(rows):3d} cap{mean([r[0] for r in rows]):+5.2f} "
                  f"mo5{mo5_s} mo30{mo30_s}")

    if attr_rows:
        rest_age_s = hold_age_sum / hold_age_n if hold_age_n else None
        print_drift_attribution(attr_rows)
        print_age_attribution(attr_rows, attr_matched, attr_unmatched, rest_age_s)
        print_staleness_attribution(attr_rows)
        print_markout_curve(attr_rows)
        print("\n=== attribution robustness: by treatment ===")
        for t in ("baseline", "candidate"):
            xs = [r for r in attr_rows if r["treatment"] == t and r["drift"][15.0] is not None]
            if not xs:
                continue
            negpct = sum(1 for r in xs if r["drift"][15.0] < 0) / len(xs) * 100
            freshpct = sum(1 for r in xs if r["cls"] == "fresh") / len(xs) * 100
            aged = [r["age"] for r in xs if r["age"] is not None]
            age_s = f"{mean(aged):.0f}s" if aged else "n/a"
            print(f"{t:9s}: n{len(xs):3d} run-over15s {negpct:4.0f}% fresh {freshpct:4.0f}% "
                  f"drift15{m([r['drift'][15.0] for r in xs])} mean age {age_s}")
        print_signal_pricing(attr_rows)


if __name__ == "__main__":
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    main(sys.argv[1:])
