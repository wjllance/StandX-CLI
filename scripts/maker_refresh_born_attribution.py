#!/usr/bin/env python3
"""Refresh-born attribution for toxic fills. Read-only offline analysis.

Question (candidate 6 scoping): are the worst fills (by mo300) disproportionately
orders that were placed as REFRESH re-quotes — i.e. the placement's own cycle
carries a cancel with reason=mark_moved on the same (side, level)? Reconcile
cancels and re-places within one cycle, so "cancelled then re-placed" is exactly
"same-cycle cancel(mark_moved) + place" with no time-window guessing.

    refresh-born := exists cancel(cycle == place.cycle, side == place.side,
                    level == place.level, reason == "mark_moved")

Toxic definitions (both reported; conclusions must agree in sign):
  - decile:    worst 10% of mo300 (per-run for per-run tables, pooled for pooled)
  - absolute:  mo300 < -25 bps

place events carry no order_id (intent precedes venue id assignment), so the
fill -> placement attribution replays the place/cancel/place_rejected_async
timeline per side with exact price match, same convention as
maker_markout_ab.attribution_rows (fill consumes the OLDEST matching open
placement; cancel consumes the NEWEST). Unmatched fills are reported.

Tables:
  0. preliminary: refresh-born share of ALL placements (discriminative power)
  1. toxic x refresh-born 2x2 + Fisher exact, per run and pooled, both definitions
  2. table 1 stratified by market speed (trailing 60s realized vol of cycle
     marks, terciles) — confound control: fast markets both refresh more and
     toxify more
  3. direction consistency: sign(drift_place) == sign(mo30) rate and
     pearson/spearman(drift_place, mo30) for toxic vs rest — the "run over"
     signature is pre-placement drift toward the quote AND post-fill continuation

Usage:
    python3 scripts/maker_refresh_born_attribution.py RUN.ndjson [RUN.ndjson ...]
"""
import bisect
import sys
from collections import defaultdict
from math import exp, lgamma
from statistics import mean, median, stdev

from maker_markout_ab import parse_ts, load_arm

TOXIC_ABS_BPS = -25.0
MO_TOXIC_H = 300.0
MO_REF_H = 30.0
DRIFT_PLACE_W = (15.0, 30.0)
RVOL_WINDOW_S = 60.0


def replay(path):
    """Per passive fill: refresh-born label + mo300/mo30 + drift_place + age.

    Returns (rows, n_places, n_refresh_born_places, matched, unmatched).
    rows: dicts with side/ts/mo300/mo30/drift_place/age/refresh_born/rvol60.
    Fills whose mo300 horizon is censored by run end keep mo300=None and are
    excluded from the toxic tables (counted separately).
    """
    cycles, fills, _perf, _pnl, timeline, _hold = load_arm(path)
    ts_list = [c[0] for c in cycles]

    # cycles carrying a mark_moved cancel per (side, level)
    mm_cycles = set()
    for kind, d in timeline:
        if kind == "cancel" and d.get("reason") == "mark_moved":
            mm_cycles.add((d.get("cycle"), d.get("side"), d.get("level")))

    n_places = n_rb_places = 0
    open_orders = defaultdict(list)  # side -> [(placed_ts, price, ref_mark, cycle, level, pred_class, pred_gap)]
    cancelled = defaultdict(list)    # side -> [(cancel_ts, price, placed_ts, ref_mark, cycle, level, pred_class, pred_gap)]
    rows, matched, unmatched = [], 0, 0

    # Pass 1: every same-side order exit (fill or cancel) with its cycle/ts.
    # Fills that trigger a re-place are logged AFTER the new place intent, so
    # the predecessor of a placement cannot be read in log order — precompute.
    # Every passive fill removes one same-side resting order (single level).
    exits = defaultdict(list)  # side -> [(cycle, ts, kind)] kind: fill|cancel:<reason>
    for kind, d in timeline:
        if kind == "fill" and d.get("role") == "passive_maker":
            try:
                exits[d.get("side")].append((d.get("cycle"), parse_ts(d["ts"]), "fill"))
            except Exception:
                pass
        elif kind == "cancel":
            try:
                exits[d.get("side")].append(
                    (d.get("cycle"), parse_ts(d["ts"]), f"cancel:{d.get('reason')}"))
            except Exception:
                pass
    for side in exits:
        exits[side].sort()

    def predecessor(side, pc, pt):
        """Why did the previous same-side order leave before this placement.

        Latest exit with cycle <= placement cycle; on a same-cycle tie a fill
        beats a cancel (the fill is what removed the order we now replace).
        Returns (class, gap_s) where gap is place_ts - fill_ts for post_fill.
        """
        cand = [e for e in exits.get(side, ()) if e[0] is not None and pc is not None and e[0] <= pc]
        if not cand:
            return "cold_start", None
        best_cycle = cand[-1][0]
        top = [e for e in cand if e[0] == best_cycle]
        kinds = {e[2] for e in top}
        if "fill" in kinds:
            ft = max(e[1] for e in top if e[2] == "fill")
            return "post_fill", max(0.0, pt - ft)
        reason = sorted(kinds)[0].split(":", 1)[1]
        return {"mark_moved": "post_refresh_gap",
                "side_suppressed": "post_suppressed"}.get(reason, "pushed_out"), None

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

    def mo_at(t, h, sign, mark0):
        j = bisect.bisect_left(ts_list, t + h)
        return (cycles[j][1] - mark0) * sign / mark0 * 1e4 if j < len(cycles) else None

    def rvol60(t):
        j1 = bisect.bisect_right(ts_list, t) - 1
        j0 = bisect.bisect_left(ts_list, t - RVOL_WINDOW_S)
        if j1 - j0 < 3:
            return None
        moves = [(cycles[k + 1][1] - cycles[k][1]) / cycles[k][1] * 1e4
                 for k in range(j0, j1) if cycles[k][1]]
        return stdev(moves) if len(moves) >= 3 else None

    for kind, d in timeline:
        side = d.get("side")
        if kind == "place":
            try:
                pt, pc = parse_ts(d["ts"]), d.get("cycle")
                pred, pgap = predecessor(side, pc, pt)
                entry = (pt, float(d["price"]), float(d["mark"]),
                         pc, d.get("level"), pred, pgap)
            except Exception:
                continue
            open_orders[side].append(entry)
            n_places += 1
            if (entry[3], side, entry[4]) in mm_cycles:
                n_rb_places += 1
        elif kind == "cancel":
            try:
                px = float(d["price"])
            except Exception:
                px = None
            popped = []
            pop_matching(side, px, popped)
            for pt, opx, rm, pc, pl, pred, pgap in popped:
                cancelled[side].append((parse_ts(d["ts"]), opx, pt, rm, pc, pl, pred, pgap))
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
            try:
                maf = float(d.get("mark_at_fill"))
            except Exception:
                maf = mark0
            hit = None
            orders = open_orders.get(side, [])
            for k, (pt, px, rm, pc, pl, pred, pgap) in enumerate(orders):
                if abs(px - price) < 1e-6:
                    hit = (pt, rm, pc, pl, pred, pgap)
                    orders.pop(k)
                    break
            if hit is None:
                for ct, px, pt, rm, pc, pl, pred, pgap in reversed(cancelled.get(side, [])):
                    if abs(px - price) < 1e-6 and 0.0 <= t - ct <= 30.0:
                        hit = (pt, rm, pc, pl, pred, pgap)
                        break
            if hit is None:
                unmatched += 1
                continue
            matched += 1
            pt, rm, pc, pl, pred, pgap = hit
            drift_place = {}
            for w in DRIFT_PLACE_W:
                j = bisect.bisect_right(ts_list, pt - w) - 1
                drift_place[w] = (rm - cycles[j][1]) * sign / rm * 1e4 \
                    if j >= 0 and rm else None
            # pre-fill signed drift, available AT fill time (proxy candidates);
            # same convention as maker_markout_ab drift_in: negative = run-over
            drift_fill = {}
            for w in (5.0, 15.0, 30.0):
                j = bisect.bisect_right(ts_list, t - w) - 1
                drift_fill[w] = (maf - cycles[j][1]) * sign / maf * 1e4 \
                    if j >= 0 and maf else None
            rows.append(dict(side=side, ts=t, matched=True,
                             refresh_born=(pc, side, pl) in mm_cycles,
                             age=max(0.0, t - pt),
                             cap=(mark0 - price) * sign / price * 1e4,
                             mo5=mo_at(t, 5.0, sign, mark0),
                             mo15=mo_at(t, 15.0, sign, mark0),
                             mo300=mo_at(t, MO_TOXIC_H, sign, mark0),
                             mo30=mo_at(t, MO_REF_H, sign, mark0),
                             drift_fill=drift_fill,
                             drift_place=drift_place, rvol60=rvol60(t),
                             pred_class=pred, pred_gap=pgap))
    return rows, n_places, n_rb_places, matched, unmatched


def fisher_2x2(a, b, c, d):
    """Two-sided Fisher exact p for [[a, b], [c, d]] (lgamma, big-n safe)."""
    n = a + b + c + d
    r1, c1 = a + b, a + c

    def pmf(x):
        return exp(lgamma(r1 + 1) - lgamma(x + 1) - lgamma(r1 - x + 1)
                   + lgamma(n - r1 + 1) - lgamma(c1 - x + 1) - lgamma(n - r1 - c1 + x + 1)
                   - (lgamma(n + 1) - lgamma(c1 + 1) - lgamma(n - c1 + 1)))

    lo, hi = max(0, c1 - (n - r1)), min(r1, c1)
    p_obs = pmf(a)
    return min(1.0, sum(pmf(x) for x in range(lo, hi + 1) if pmf(x) <= p_obs * (1 + 1e-9)))


def pearson(xs, ys):
    mx, my = mean(xs), mean(ys)
    cov = sum((x - mx) * (y - my) for x, y in zip(xs, ys))
    vx = sum((x - mx) ** 2 for x in xs)
    vy = sum((y - my) ** 2 for y in ys)
    return cov / (vx * vy) ** 0.5 if vx > 0 and vy > 0 else None


def spearman(xs, ys):
    def ranks(v):
        order = sorted(range(len(v)), key=lambda k: v[k])
        r = [0.0] * len(v)
        i = 0
        while i < len(v):
            j = i
            while j + 1 < len(v) and v[order[j + 1]] == v[order[i]]:
                j += 1
            for k in range(i, j + 1):
                r[order[k]] = (i + j) / 2.0 + 1
            i = j + 1
        return r
    return pearson(ranks(xs), ranks(ys))


def table1(rows, label, toxic_pred):
    """2x2: toxic (by toxic_pred) x refresh-born, on matched mo300-available fills."""
    xs = [r for r in rows if r["matched"] and r["mo300"] is not None]
    toxic = [r for r in xs if toxic_pred(r)]
    rest = [r for r in xs if not toxic_pred(r)]
    a = sum(1 for r in toxic if r["refresh_born"])
    b = len(toxic) - a
    c = sum(1 for r in rest if r["refresh_born"])
    d = len(rest) - c
    pt = a / len(toxic) * 100 if toxic else float("nan")
    pr = c / len(rest) * 100 if rest else float("nan")
    p = fisher_2x2(a, b, c, d) if toxic and rest else None
    oratio = (a * d) / (b * c) if b and c else float("inf")
    print(f"  {label}: toxic n{len(toxic)} (rb {a}/{len(toxic)} = {pt:.1f}%) | "
          f"rest n{len(rest)} (rb {c}/{len(rest)} = {pr:.1f}%) | "
          f"diff {pt - pr:+.1f}pp OR {oratio:.2f}"
          + (f" fisher p {p:.4f}" if p is not None else ""))
    return dict(toxic=toxic, rest=rest)


def decile_cut(rows):
    """mo300 threshold of the worst decile among matched, mo300-available fills."""
    xs = sorted(r["mo300"] for r in rows if r["matched"] and r["mo300"] is not None)
    if not xs:
        return None
    k = max(1, int(len(xs) * 0.10))
    return xs[k - 1]


def run_tables(rows, label, n_unmatched=0):
    xs = [r for r in rows if r["matched"]]
    cens = sum(1 for r in xs if r["mo300"] is None)
    print(f"[{label}] matched {sum(1 for r in xs)} unmatched {n_unmatched} "
          f"| mo300-censored {cens}")
    thr = decile_cut(rows)
    print(f"  mo300 decile cut: {thr:+.2f} bps")
    print(f"  -- toxic = worst decile (mo300 <= {thr:+.2f}) --")
    out_dec = table1(rows, "decile   ", lambda r: r["mo300"] <= thr)
    print(f"  -- toxic = mo300 < {TOXIC_ABS_BPS} bps (absolute) --")
    out_abs = table1(rows, "absolute ", lambda r: r["mo300"] < TOXIC_ABS_BPS)
    return out_dec, out_abs


def qtile(xs, p):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(p * len(xs)))] if xs else None


def same_side_pairs(run_rows):
    """(current, next) consecutive same-side fill pairs, toxic labeled per-run
    decile; plus total run hours across all runs."""
    pairs, hours = [], 0.0
    for _name, rows in run_rows:
        thr = decile_cut(rows)
        if thr is None:
            continue
        by_side = defaultdict(list)
        for r in rows:
            if r["matched"] and r["mo300"] is not None:
                rr = dict(r)
                rr["toxic"] = r["mo300"] <= thr
                by_side[r["side"]].append(rr)
        for seq in by_side.values():
            seq.sort(key=lambda r: r["ts"])
            for prev, cur in zip([None] + seq, seq):
                cur["prev_gap_side"] = (cur["ts"] - prev["ts"]) if prev else None
            pairs.extend(zip(seq, seq[1:]))
        ts = [r["ts"] for r in rows]
        if ts:
            hours += (max(ts) - min(ts)) / 3600.0
    return pairs, hours


# trigger proxy candidates: (name, value fn, thetas, seconds the strategy must
# wait after the fill before it can evaluate). Fired = value <= -theta.
# Negative value = adverse (run-over); chain proxies encode prev same-side gap
# as -gap, so theta reads as "previous same-side fill within theta seconds".
PROXIES = [
    ("drift5 ", lambda r: r["drift_fill"].get(5.0), (2.0, 4.0, 6.0, 8.0), 0),
    ("drift15", lambda r: r["drift_fill"].get(15.0), (2.0, 4.0, 6.0, 8.0), 0),
    ("drift30", lambda r: r["drift_fill"].get(30.0), (2.0, 4.0, 6.0, 8.0), 0),
    ("mo5    ", lambda r: r["mo5"], (2.0, 4.0, 6.0, 8.0), 5),
    ("mo15   ", lambda r: r["mo15"], (2.0, 4.0, 6.0, 8.0), 15),
    ("mo30   ", lambda r: r["mo30"], (2.0, 4.0, 6.0, 8.0), 30),
    ("chain  ", lambda r: -r["prev_gap_side"] if r.get("prev_gap_side") is not None else None,
     (10.0, 30.0, 60.0, 120.0), 0),
]


def table_proxy(run_rows, label):
    """Decision quality of fast toxicity proxies: for 'suppress the side after
    a fill whose proxy reads adverse <= -theta', report trigger rate (uptime
    budget), P(next same-side fill toxic | fired) (precision), and coverage
    (share of toxic next-fills that follow a trigger)."""
    pairs, hours = same_side_pairs(run_rows)
    if not pairs:
        return
    base = sum(1 for _a, b in pairs if b["toxic"]) / len(pairs)
    print(f"  {label}: pairs {len(pairs)}, base P(next toxic) {base * 100:.1f}%, "
          f"runs {hours:.1f}h")
    print(f"  {'proxy':7s} {'wait':>4s} {'theta':>6s} {'trig%':>6s} {'trig/side-h':>11s} "
          f"{'P(tox|fire)':>11s} {'P(tox|not)':>10s} {'cover%':>7s}")
    for name, fn, thetas, wait in PROXIES:
        valid = [(a, b) for a, b in pairs if fn(a) is not None]
        tox_next = sum(1 for _a, b in valid if b["toxic"])
        for theta in thetas:
            fired = [(a, b) for a, b in valid if fn(a) <= -theta]
            if not fired:
                continue
            nf = len(fired)
            p_f = sum(1 for _a, b in fired if b["toxic"]) / nf * 100
            rest = len(valid) - nf
            p_n = (tox_next - sum(1 for _a, b in fired if b["toxic"])) / rest * 100 if rest else 0.0
            cov = sum(1 for _a, b in fired if b["toxic"]) / tox_next * 100 if tox_next else 0.0
            print(f"  {name:7s} {wait:4d}s {-theta:6.0f} {nf / len(valid) * 100:5.1f}% "
                  f"{nf / hours / 2:10.1f} {p_f:10.1f}% {p_n:9.1f}% {cov:6.0f}%")


def table_gaps(run_rows, label):
    """Cooldown-window sizing: inter-arrival of the NEXT same-side fill after a
    toxic one (the fill a cooldown must catch), plus net markout effect per
    window length for an exemplar trigger (mo15 <= -4bps). Net = forgoing the
    suppressed fills' cap+mo300; blocking a toxic fill gains, blocking a good
    fill pays. Lag-1 pairs approximate multi-fill windows (rare for L<=300s)."""
    pairs, hours = same_side_pairs(run_rows)
    g_tt = [b["ts"] - a["ts"] for a, b in pairs if a["toxic"] and b["toxic"]]
    g_ta = [b["ts"] - a["ts"] for a, b in pairs if a["toxic"]]
    for nm, g in (("toxic->toxic", g_tt), ("toxic->any   ", g_ta)):
        if not g:
            continue
        shares = " ".join(f"<={L}s {sum(1 for x in g if x <= L) / len(g) * 100:.0f}%"
                          for L in (15, 30, 60, 120, 300))
        print(f"  {label} {nm}: n{len(g)} p25 {qtile(g, .25):.0f}s med {qtile(g, .5):.0f}s "
              f"p75 {qtile(g, .75):.0f}s p90 {qtile(g, .9):.0f}s | {shares}")
    fns = {n.strip(): f for n, f, _t, _w in PROXIES}
    for trig_name, trig_fn, trig_th in (("mo15<=-4bps", fns["mo15"], -4.0),
                                        ("drift15<=-6bps", fns["drift15"], -6.0),
                                        ("chain<=30s", fns["chain"], -30.0)):
        fired = [(a, b) for a, b in pairs if trig_fn(a) is not None and trig_fn(a) <= trig_th]
        print(f"  net per window, trigger {trig_name} ({len(fired)} fired pairs, {hours:.1f}h):")
        for L in (15, 30, 60, 120, 300):
            sup = [(a, b) for a, b in fired if b["ts"] - a["ts"] <= L]
            if not sup:
                continue
            ok = [(a, b) for a, b in sup if b["cap"] is not None and b["mo300"] is not None]
            net = -sum(b["cap"] + b["mo300"] for _a, b in ok)
            av = sum(1 for _a, b in sup if b["toxic"])
            up = len(fired) * L / (hours * 3600 * 2) * 100  # upper bound, ignores overlap
            print(f"    L={L:3d}s: suppress {len(sup)} fills (toxic {av}) | "
                  f"net {net:+8.1f} bps total, {net * 36 / hours:+6.1f} bps/36h | "
                  f"uptime cost <={up:.0f}% side-time")


def table_age4(out, label):
    """Four-cell age cut: toxic/rest x refresh-born/not. Separates 'young fills
    are toxic' from 'birth-manner is toxic' — decides whether the refresh_bps
    knob (which only changes how long orders live, not how they are born) has
    any support at all."""
    for tox, rows in (("toxic", out["toxic"]), ("rest ", out["rest"])):
        for rb in (True, False):
            ages = [r["age"] for r in rows if r["refresh_born"] is rb and r["age"] is not None]
            if not ages:
                continue
            q1, q2, q3 = qtile(ages, .25), qtile(ages, .5), qtile(ages, .75)
            print(f"  {label} {tox} rb={str(rb):5s}: n{len(ages):4d} age "
                  f"p25 {q1:5.0f}s med {q2:5.0f}s p75 {q3:5.0f}s mean {mean(ages):5.0f}s")


def table_pred(out, label):
    """Non-refresh-born fills by predecessor exit reason (exact fields, not a
    time proxy): what removed the previous same-side order before the matched
    placement. post_fill gap is fill->place (exact; the re-place lands in the
    same or next cycle)."""
    for tox, rows in (("toxic", out["toxic"]), ("rest ", out["rest"])):
        nonrb = [r for r in rows if r["refresh_born"] is False]
        n = len(nonrb)
        if not n:
            continue
        from collections import Counter
        c = Counter(r["pred_class"] for r in nonrb)
        parts = " ".join(f"{k} {v} ({v / n * 100:.0f}%)" for k, v in c.most_common())
        print(f"  {label} {tox} non-rb n{n}: {parts}")
        pf = [r["pred_gap"] for r in nonrb if r["pred_class"] == "post_fill" and r["pred_gap"] is not None]
        if pf:
            q1, q2, q3 = qtile(pf, .25), qtile(pf, .5), qtile(pf, .75)
            print(f"           post_fill fill->place gap: p25 {q1:.0f}s med {q2:.0f}s "
                  f"p75 {q3:.0f}s mean {mean(pf):.0f}s | <5s {sum(1 for g in pf if g < 5)}/{len(pf)}")


def table_autocorr(run_rows, label):
    """Same-side toxicity clustering: is the NEXT same-side fill after a toxic
    fill itself toxic more often than the base rate? Positive autocorrelation
    is the 'flow continuation / iceberg sweep' signature; zero kills that
    branch and points back to 'picked off while resting'."""
    tt = tn = nt = nn = 0
    gaps_t, gaps_n = [], []
    for _name, rows in run_rows:
        thr = decile_cut(rows)
        if thr is None:
            continue
        by_side = defaultdict(list)
        for r in rows:
            if r["matched"] and r["mo300"] is not None:
                by_side[r["side"]].append(r)
        for side, seq in by_side.items():
            seq.sort(key=lambda r: r["ts"])
            for a, b in zip(seq, seq[1:]):
                ta, tb = a["mo300"] <= thr, b["mo300"] <= thr
                if ta:
                    tt += tb
                    tn += (not tb)
                    gaps_t.append(b["ts"] - a["ts"])
                else:
                    nt += tb
                    nn += (not tb)
                    gaps_n.append(b["ts"] - a["ts"])
    base = (tt + nt) / (tt + tn + nt + nn) if (tt + tn + nt + nn) else 0
    p_t = tt / (tt + tn) if (tt + tn) else float("nan")
    p_n = nt / (nt + nn) if (nt + nn) else float("nan")
    p = fisher_2x2(tt, tn, nt, nn)
    gt = f"med gap {median(gaps_t):.0f}s" if gaps_t else ""
    gn = f"med gap {median(gaps_n):.0f}s" if gaps_n else ""
    print(f"  {label}: base rate {base * 100:.1f}% | "
          f"P(next toxic | toxic) = {p_t * 100:.1f}% ({tt}/{tt + tn}) {gt} | "
          f"P(next toxic | non-toxic) = {p_n * 100:.1f}% ({nt}/{nt + nn}) {gn} | "
          f"fisher p {p:.4f}")


def table2(rows):
    """Table 1 (decile toxicity) stratified by trailing-60s realized vol terciles."""
    xs = [r for r in rows if r["matched"] and r["mo300"] is not None and r["rvol60"] is not None]
    if len(xs) < 30:
        print("  stratified: too few fills")
        return
    vs = sorted(r["rvol60"] for r in xs)
    q1, q2 = vs[len(vs) // 3], vs[2 * len(vs) // 3]
    thr = decile_cut(rows)
    for name, lo, hi in (("slow", None, q1), ("mid", q1, q2), ("fast", q2, None)):
        seg = [r for r in xs if (lo is None or r["rvol60"] >= lo)
               and (hi is None or r["rvol60"] < hi)]
        table1(seg, f"rvol {name:4s}", lambda r: r["mo300"] <= thr)


def table3(out, label):
    for group, rows in (("toxic", out["toxic"]), ("rest", out["rest"])):
        for w in DRIFT_PLACE_W:
            pairs = [(r["drift_place"][w], r["mo30"]) for r in rows
                     if r["drift_place"].get(w) is not None and r["mo30"] is not None]
            if len(pairs) < 10:
                continue
            dp = [p[0] for p in pairs]
            mo = [p[1] for p in pairs]
            agree = sum(1 for x, y in pairs if (x < 0) == (y < 0)) / len(pairs) * 100
            bothneg = sum(1 for x, y in pairs if x < 0 and y < 0) / len(pairs) * 100
            pe, sp = pearson(dp, mo), spearman(dp, mo)
            print(f"  {label} {group:5s} drift_place{w:.0f}s: n{len(pairs)} "
                  f"sign-agree {agree:.0f}% both-neg {bothneg:.0f}% "
                  f"drift{mean(dp):+5.2f} mo30{mean(mo):+6.2f} "
                  f"pearson {pe:+.3f} spearman {sp:+.3f}")


def main(paths):
    all_rows = []
    per_run = []
    for path in paths:
        name = path.rsplit("/", 1)[-1].replace(".ndjson", "")
        rows, n_pl, n_rb, matched, unmatched = replay(path)
        rb_fill = sum(1 for r in rows if r["refresh_born"])
        n_att = sum(1 for r in rows if r["matched"])
        print(f"\n===== {name} =====")
        print(f"placements {n_pl}, refresh-born {n_rb} ({n_rb / n_pl * 100:.1f}%) "
              f"| attributed fills {n_att}, refresh-born {rb_fill} "
              f"({rb_fill / n_att * 100 if n_att else 0:.1f}%)")
        out_dec, out_abs = run_tables(rows, name, unmatched)
        print("  -- table 2: stratified by trailing 60s realized vol (decile toxicity) --")
        table2(rows)
        print("  -- table 4: four-cell age (toxic x refresh-born) --")
        table_age4(out_dec, name)
        print("  -- table 5: non-rb predecessor exit reason --")
        table_pred(out_dec, name)
        print("  -- table 3: direction consistency (decile toxicity) --")
        table3(out_dec, name)
        per_run.append((name, rows))
        all_rows.extend(rows)

    print("\n===== POOLED =====")
    n_pl = n_rb = n_un = 0
    for path in paths:  # placement shares again, pooled
        _rows, a, b, _m, _u = replay(path)
        n_pl += a
        n_rb += b
        n_un += _u
    rb_fill = sum(1 for r in all_rows if r["refresh_born"])
    n_att = sum(1 for r in all_rows if r["matched"])
    print(f"placements {n_pl}, refresh-born {n_rb} ({n_rb / n_pl * 100:.1f}%) "
          f"| attributed fills {n_att}, refresh-born {rb_fill} "
          f"({rb_fill / n_att * 100 if n_att else 0:.1f}%)")
    out_dec, out_abs = run_tables(all_rows, "pooled", n_un)
    print("  -- table 2: stratified by trailing 60s realized vol (decile toxicity) --")
    table2(all_rows)
    print("  -- table 4: four-cell age (toxic x refresh-born) --")
    table_age4(out_dec, "pooled")
    print("  -- table 5: non-rb predecessor exit reason --")
    table_pred(out_dec, "pooled")
    print("  -- table 3: direction consistency (decile toxicity) --")
    table3(out_dec, "pooled")
    print("  -- table 6: same-side toxicity autocorrelation (lag-1) --")
    table_autocorr(per_run, "pooled")
    print("  -- table 7: fast-proxy decision quality (trigger = suppress side) --")
    table_proxy(per_run, "pooled")
    print("  -- table 8: cooldown window sizing + net per window --")
    table_gaps(per_run, "pooled")


if __name__ == "__main__":
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    main(sys.argv[1:])
