# standx-maker

> Deterministic market-making strategy and risk engine for StandX.

[![Rust](https://img.shields.io/badge/rust-1.75%2B-orange.svg)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](../../LICENSE)

This crate is the decision layer behind `standx maker run`. Given a market
snapshot, an inventory position and a set of resting orders, it decides what to
quote, what to cancel, when to trim inventory, and when to halt, freeze, exit or
recover — and it returns those decisions as typed values. It never talks to the
exchange itself; the CLI executes the effects.

**Status: live-capable, strategy not validated.** The engine has run against
real money on HYPE-USD for two independent ~36-hour sessions with no safety
events. Both sessions lost money. See [目前表现](#目前表现) below for the
numbers.

## What it implements

**Quoting** — a multi-level ladder around an inventory-skewed center, clamped
into the SIP-5A eligibility band, then clamped again so post-only orders can
never cross the touch. The core rule is *anti-flicker*: SIP-5A rewards resting
uptime and penalizes cancel churn, so a resting quote is **held** while it is
still inside the band, and re-quoted only once the center has drifted past
`refresh_bps` from where it was placed. `reconcile` is a six-row decision table
(side-suppressed → stale → outside-band → would-cross → mark-moved → hold) that
emits cancels before places, so margin frees up first.

**Inventory control** — three independent layers, each separately toggleable:
linear price skew (`skew_bps`), nonlinear skew that steepens with inventory and
saturates at a cap, and size skew that shrinks the add-side order quantity above
a threshold. On top of that, `cap_desired_exposure` budgets the whole ladder so
that *every level filling at once* still cannot breach `max_position`.

**Exits** — a reduce-only trim at `inventory_exit_pct`, capped to one chunk per
cycle, plus a supervisor-requested wind-down that flattens the full residual and
stops quoting for good. When a volatility halt or a market-data outage swallows
an exit, that is emitted as a typed `ExitSuppression` rather than silently
dropped.

**Risk and halts** — a volatility breaker on the rolling mark range (halt at
`pause_bps`, re-arm at half), adaptive spread tiers that widen with volatility,
an external-price guard that suppresses the endangered side when a leading venue
(Hyperliquid mid) moves and StandX's mark has not yet, edge-triggered alerts on
loss / inventory / uptime / equity / margin, and a hard account solvency floor
that is deliberately distinct from the alerts — the floor stops the session, the
alerts only notify.

**Accounting** — `MakerLedger` attributes fills to this run only (client-order-ID
prefix scoping, dedup strictly by venue `trade_id`). `PerformanceLedger` splits
passive versus exit fills, tracks fees, rebates and funding with explicit
`funding_unattributed` / `funding_coverage_gap` honesty flags and a
`net_pnl_complete` bit, and computes quantity-weighted **markouts at 1s / 5s /
30s** — the adverse-selection measurement this whole project now turns on. Quote
uptime and inventory are both integrated time-weighted, not sampled per cycle.

**Lifecycle** — a pure state machine over `Starting / Ready / Frozen / Stopping`
that turns events into effects (`RunCycle`, `AbortInFlight`, `CommitCycle`,
`Cleanup`, `Recover`, `Stop`), with generation tokens that invalidate in-flight
work, doubling recovery backoff capped at 60s, and a market-data health model
that needs three consecutive bad observations *and* a 15s grace period before
degrading. Transport failures freeze and retry; accounting-invariant violations
stop.

**Measurement** — per-request latency tracking across the full
intent → written → ack → effective lifecycle with p50/p95/p99 and a
`fill_after_cancel_ms` adverse-selection signal, and `run_replay`, which drives
the *same* `preflight_cycle_at` + `plan_cycle` + performance ledger over a typed
event trace with no clock, filesystem or network.

### Off by default

Every optional mechanism ships disabled, and each has a test asserting that the
disabled path is plan-identical to the legacy path:

| Feature | Config | Default |
| --- | --- | --- |
| Nonlinear inventory skew | `NonlinearSkewConfig` | off (`boost 3.0`, `cap 12 bps`) |
| Size skew | `SizeSkewConfig` | off (`activate 30%`, `release 20%`, `factor 0.5`) |
| External skew | `ExternalSkewConfig` | off (`lambda 0.5`, `cap 8 bps`, `dead zone 1 bps`) |
| External-price guard | `GuardConfig` | off (`enter 6 bps`, `exit 3 bps`, `max age 5s`) |
| Adaptive spread tiers | `AdaptiveSpreadConfig` | off (no tiers) |
| Alerts / account floors | `AlertMonitor` | all thresholds `0` = off |

Validation runs even when a feature is disabled — e.g. the band red line
`spread_bps + cap_bps <= band_bps` is enforced at construction, so a config that
would quote outside the eligibility band is rejected up front rather than
silently clamped later.

## 目前表现

口径遵循 [docs/28-experiment-protocol.md](../../docs/28-experiment-protocol.md)：
判决前预注册标准，读数如实写，未收尾的中途切片明确标注。

**两轮独立 live 实盘读数均为负。** 标的 HYPE-USD，冻结基线配置
`examples/maker-guard-hype-candidate.toml`，计价 DUSD：

| | run1 `…20260728T081712Z` | run4 `…20260731T030133Z` |
| --- | --- | --- |
| 时长 | 35.9h（被残单误报打断） | 36h（**中途切片，未收尾**） |
| 净 PnL | **-1.8591** | **-1.66**（≈ 权益 0.9%） |
| gross spread capture | +1.5035 | +1.42 |
| 库存 MTM | -3.0716 | — |
| 手续费 | -0.2896 | -0.27 |
| funding | -0.0014 | — |
| rebate | 0 | 0 |
| 被动成交数 | 527 | 513 |
| 时间加权双边 uptime | 96.6% | — |

run1 的 `net_pnl_complete=true`，所有成本项都已入账；亏损轨迹单调恶化
（19h 时 -1.11 → 35.9h 时 -1.86）。当前烧钱速率约 **-0.046 DUSD/h**。

**逆选解释了 100% 的亏损。** 单笔经济性算术：

```
capture +2.8 bps  −  markout@30s 7.8 bps  −  手续费 ≈1 bps  ≈  −6 bps/笔
−6 bps × 513 笔 × ~5.5 名义  ≈  −1.66 DUSD  =  会话净亏
```

没有无法解释的残差。markout 曲线在 30s 就饱和（30s -7.8 / 60s -8.4 / 300s -8.1
/ 900s -8.0 bps），"长尾持续失血"假设已被证伪。亏损高度集中在尾部：最差 10% 的
成交（n≈103，两轮合计）mo300 达 **-53 ~ -58 bps**，承担了 **60–72%** 的全部
markout 损失。但剩下 90% 的"良性"成交同样亏钱：
`capture 2.8 − markout 3.2 − 手续费 1 ≈ -1.4 bps/笔`。

> **两种 markout 口径不可混用。** 遥测 `performance.markout_*` 从成交价起算、含
> capture（run1 @30s = -5.14 bps）；分析脚本从成交时 mark 起算、不含 capture
> （-7.8 ~ -7.9 bps）。二者相差恰好一个 capture：`-8.09 + 2.91 ≈ -5.14`。

**工程侧稳定，安全边界从未被触及。** 两轮都远离 `stop_loss=5.0`，无安全事件；
run1 的 35.9h 全程 `market_data_standby` 触发 0 次。早期 run2 / run3 分别在约
2 分钟和约 51 分钟夭折，根因是场馆对一个 `order:cancel` 会回**两帧**（网关
`accepted` + 终态帧），打破了"一 request_id 一响应"的设计假设——该问题已修复。
run1 的中断则是残单检测误报（场馆 open-orders 读写延迟 ≥15s），撤单其实已成功。

**已判决的机制。** `nonlinear_skew`（07-25 接受，p95 库存尾部 -29%）与
`external_guard`（07-27 接受，tw 双边 uptime 97.0–98.9%，30s 跟随率 17/18）已进
基线；stage 4「恒宽加宽 8→12 bps」于 07-20 **终止**（成交频次 9–11/h 掉到
2.7–5.7/h，逐笔净额 -1.86 → -5.04 bps）。

**当前瓶颈不是"下一个机制"，是诊断能力。** 盘口深度与吃单方向从未落过日志，
这直接卡住了剩余毒性尾部的归因。因此下一步优先级最高的是纯观测遥测，而不是继续
上机制。同时 uptime 是硬约束（下降 > 2pp 即 rejected），即使用完美先知触发器，
一次 60s 单边压制就要花 2.3pp，这迫使设计形态走"加宽而非压侧"。

一条从这轮分析里固化下来的**强制评审口径**：由于平均每笔成交亏 6.3 bps，*任何*
压制机制看上去都会盈利（等量随机压单的账面收益就有 +6.5 bps）。评审必须先扣掉
等量随机压制的基线，否则所谓"收益"其实只是"少报价"，而它的极限就是"停止做市"。

摆在仲裁桌上的开放问题：**capture 2.8 bps 要养住 1 bps 手续费 + 8 bps 逆选 ——
这个单笔经济性在当前规模与费率下是否成立？** 在读数转正之前，按
[27 号手册](../../docs/27-maker-baseline-pnl-collection-runbook.md)的规则
**不扩规模**。

完整证据链：
[markout 尾部分解](../../docs/evidence/maker-markout-tail-decomposition-2026-08-01.md)、
[基线 PnL run1](../../docs/evidence/maker-baseline-pnl-2026-07-30.md)、
[当前状态与路线](../../docs/18-maker-strategy-roadmap.md)。

## The contract

- **No I/O.** No network, no clock, no filesystem, no terminal. Every function
  takes plain values and returns decisions or typed effects.
- **Deterministic.** Same typed inputs → same outputs, so the whole strategy is
  replayable and unit-testable offline.
- **Depends only on `standx-sdk`**, and only for model types (`OrderSide` at the
  crate root). The CLI executes the effects this crate returns; the effects never
  execute themselves.

That boundary is enforced deliberately — see [AGENTS.md](../../AGENTS.md) for the
rules on what belongs here versus in `standx-cli` or `standx-sdk`.

## Modules

- **Quoting** — `lib.rs` (planner, reconcile, band and no-cross clamps, exposure
  cap, exit plans), `ownership` (per-run client-order-ID scoping)
- **Strategy** — `inventory` (nonlinear + size skew), `volatility` (breaker,
  adaptive tiers), `external_skew`, `external_guard`, `risk`
- **Accounting** — `ledger`, `performance` (markouts, time-weighted uptime),
  `account_projection`, `stats`, `alerts`
- **Lifecycle** — `runtime`, `recovery`, `market_data`
- **Measurement** — `latency`, `replay`

Crate-level docs in [`src/lib.rs`](src/lib.rs) explain the anti-flicker loop and
the numeric representation choice.

## Where to look next

- **Running the bot** (every flag, telemetry, live safety rails) →
  [docs/13-maker.md](../../docs/13-maker.md)
- **Live-mode unlock criteria** → [docs/14-maker-live-gate.md](../../docs/14-maker-live-gate.md)
- **Experiment protocol** (pre-registered criteria, verdict vocabulary) →
  [docs/28-experiment-protocol.md](../../docs/28-experiment-protocol.md)
- **Strategy roadmap and stage verdicts** →
  [docs/18-maker-strategy-roadmap.md](../../docs/18-maker-strategy-roadmap.md)
- **Contribution boundary** → [AGENTS.md](../../AGENTS.md)
- **The transport layer this sits on** → [`standx-sdk`](../standx-sdk/README.md)

205 unit tests, all in-crate, no network:

```bash
cargo test -p standx-maker
```

## License

MIT OR Apache-2.0
