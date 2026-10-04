# BTC best-anchor margin, paper calibration — 2026-10-04

Read-only measurement on the existing StandX BTC depth book. No orders, no config edits, no live authorization.

## Recommendation

**Margin = 0 price units. Stand-off = 49.**

`stand_off = max(best_jump_p999, one_tick) + margin = max(49, 1) + 0 = 49`.

The cushion hypothesis is discarded. Within-second max adverse excursion does **not** stick out past the last-minus-first p99.9, so there is no gap for an operator margin to cover. The k=0.25 and k=0.5 placeholders (margin 12.25 and 24.5) buy down a residual tail that is already outside that bar. This file does not authorize turning a live margin on. Live margin stays 0.

## Fresh read vs the stated book

| item | stated | fresh read |
| --- | --- | --- |
| file | `lag-rec-20260820T065924Z-BTC.ndjson` | `/home/lance/workspace/bossx/standx-cli/var/standx/lag-rec-20260820T065924Z-BTC.ndjson` (376MB) |
| series | StandX `depth_book` best bid and best ask | 1,286,025 StandX rows with both sides present; 0 partial books; 0 crossed books; 0 non-integer prices |
| clock | local receive time | `local_recv_utc` (wall-clock receipt). Not `server_time`. Not monotonic `local_recv_ms`. |
| window | 2026-08-20 14:59 to 2026-08-24 09:34 Asia/Shanghai | 2026-08-20 14:59:38 to 2026-08-24 09:34:56 Asia/Shanghai. Same at minute resolution. |
| sample | 325,962 non-empty seconds | 325,962. Span is 326,119 wall seconds, of which 157 are empty and are not in the denominator. |
| last-minus-first p99.9 | 49.0 both sides | 49.0 bid and 49.0 ask. Rank neighborhood is flat 49.0, so the figure is not an interpolation artifact. |
| minimum positive best move | 1.0 | 1.0 on both sides (consecutive depth prints). |

No disagreement. The fresh read is the source of every count below.

Percentile method matches `scripts/lag_analysis.py` `quantile`: linear interpolation at `q * (n - 1)`.

## Definitions

Each non-empty local-receive second is one trial. The anchor is the first depth print in that second. The quote stands until the last depth print in the same second.

- Bid quote `Qb = first_best_bid - stand_off`
- Ask quote `Qa = first_best_ask + stand_off`
- Last-minus-first jump = `|last - first|` on that side. p99.9 of this series is 49 on both sides.
- Max adverse excursion (MAE), bid = `first_bid - min_bid`. Ask = `max_ask - first_ask`.
- **Run-through:** the visible same-side best prints strictly through the quote (`min_bid < Qb` or `max_ask > Qa`).
- **Touch:** the visible same-side best reaches the quote (`min_bid <= Qb` or `max_ask >= Qa`).
- **Fill proxy (contra-side marketable):** the other side of the displayed book reaches the quote (`min_ask <= Qb` or `max_bid >= Qa`). A resting order at that price would have been marketable against the printed book. This is an upper bound on marketable events. It is not a fill: the historical book did not contain our order, a cancel can move the best, and there is no queue.

Best prices in this file sit on a 1.0 grid. For stand-off 61.25 and 73.5, touch and run-through are the same event (`MAE >= 62` and `MAE >= 74`). For stand-off 49 they differ by the exact-49 prints.

Median book at the first print of each second: mid 77,130, spread 8. Stand-off 49 / 61.25 / 73.5 is 6.35 / 7.94 / 9.53 bps of that mid. Median spread 8 means the contra-side proxy has to travel the stand-off plus the spread.

Depth cadence: median 4 prints per non-empty second (162 seconds have a single print; those jumps are 0).

## Hypothesis

Claim tested: within-second MAE exceeds last-minus-first, so the margin should cover that gap.

| series | bid p99 | ask p99 | bid p99.9 | ask p99.9 | max |
| --- | --- | --- | --- | --- | --- |
| `|last − first|` | 26 | 25 | **49** | **49** | 145 / 184 |
| MAE from the first print | 25 | 24 | **46** | **47** | 158 / 184 |
| excess = MAE − adverse close | 11 | 10 | 22 | 20 | 57 / 51 |

MAE p99.9 is inside the jump p99.9, not outside it. The intra-second wick is a real path (excess p99.9 is 22 bid / 20 ask; excess > 0 in 30,056 bid seconds and 30,983 ask seconds), but it is nested inside the 49-point stand-off:

- Bid seconds with excess > 24.5: 175. Of those, MAE also exceeds 49 in 18.
- Ask seconds with excess > 24.5: 105. Of those, MAE also exceeds 49 in 14.
- Seconds where MAE > 49 while `|last − first|` still ≤ 49 (the hidden gap): 36 bid, 62 ask. At stand-off 61.25 that residue is 6 and 3. At 73.5 it is 3 and 0.

Setting margin to the excess percentile (~20–24) would stack a second percentile on the jump percentile. The design forbids that: margin is an operator cushion, not another percentile of this book. The measured gap does not require one.

Adverse-only last-minus-first (down for the bid, up for the ask) has p99.9 = 44 on both sides. The stated 49 is the absolute jump, which is what this calibration uses.

## Comparison

Denominator for every percent: 325,962 non-empty seconds (90.545 hours). Rates are that count divided by 90.545. They are bursty: at margin 0 the either-side run-through occupies 51 distinct hours (max 89 in one hour, 17 in one minute) and the contra-side proxy occupies 32 hours (max 9 in one hour, 3 in one minute). No second runs through both sides at once at any of these three stand-offs.

| margin | k | stand-off | scope | run-through | touch | contra-side marketable |
| --- | --- | --- | --- | --- | --- | --- |
| 0 | 0 | 49 | bid | 241 (0.0739%, 2.66/h) | 261 (0.0801%, 2.88/h) | 32 (0.0098%, 0.35/h) |
| 0 | 0 | 49 | ask | 263 (0.0807%, 2.90/h) | 283 (0.0868%, 3.13/h) | 43 (0.0132%, 0.47/h) |
| 0 | 0 | 49 | either | 504 (0.1546%, 5.57/h) | 544 (0.1669%, 6.01/h) | 75 (0.0230%, 0.83/h) |
| 12.25 | 0.25 | 61.25 | bid | 77 (0.0236%, 0.85/h) | 77 (0.0236%, 0.85/h) | 16 (0.0049%, 0.18/h) |
| 12.25 | 0.25 | 61.25 | ask | 76 (0.0233%, 0.84/h) | 76 (0.0233%, 0.84/h) | 24 (0.0074%, 0.27/h) |
| 12.25 | 0.25 | 61.25 | either | 153 (0.0469%, 1.69/h) | 153 (0.0469%, 1.69/h) | 40 (0.0123%, 0.44/h) |
| 24.5 | 0.5 | 73.5 | bid | 37 (0.0114%, 0.41/h) | 37 (0.0114%, 0.41/h) | 6 (0.0018%, 0.07/h) |
| 24.5 | 0.5 | 73.5 | ask | 43 (0.0132%, 0.47/h) | 43 (0.0132%, 0.47/h) | 18 (0.0055%, 0.20/h) |
| 24.5 | 0.5 | 73.5 | either | 80 (0.0245%, 0.88/h) | 80 (0.0245%, 0.88/h) | 24 (0.0074%, 0.27/h) |

At margin 0 the quote sits on the p99.9 print: an adverse move of exactly 49 touches it and does not run through it (20 bid seconds and 20 ask seconds). A one-point step would clear those exact prints. It is not recommended. It does not answer the wick-gap claim, and the contra-side proxy at stand-off 49 is already 75 seconds in 3.8 days.

Moving from margin 0 to 12.25 cuts either-side run-through from 504 to 153 and the contra-side proxy from 75 to 40. Moving to 24.5 cuts them to 80 and 24. That is real tail mass. It is the mass beyond a stand-off that was already set at the jump p99.9, about 6.4 bps from the best at the sample median mid, against a median spread of 8 points (0.5 bps half-spread is the separate mark-book result, not re-estimated here). This book does not show a wick gap that those extra points are paying for.

## What this does not prove

- No queue. A contra-side print at the quote is not a fill, a partial, or a place in line.
- No fees, rebate, or PnL.
- Not server time and not matching-engine time. `local_recv_utc` is when this host read the websocket frame. Network delay can reorder that relative to the venue.
- Not a sliding one-second hold. Moves that cross a wall-second boundary are split across two trials. The same bucket definition produced the 49.0 figure; the run-through rates use that definition on purpose.
- Not live authorization. Offline counts can reject a cushion. They do not approve a live margin, a quote change, or an order.
- One symbol, one window (20–24 Aug 2026), one host. Empty seconds (157) are dropped, not treated as zero jumps.
