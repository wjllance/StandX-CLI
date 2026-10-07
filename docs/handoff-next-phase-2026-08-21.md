# Handoff：下一阶段（2026-08-21）

> 给接手的 agent：这份文档假设你**零上下文**。先读完再动手。
> 状态真源：[18-maker-strategy-roadmap.md](18-maker-strategy-roadmap.md)。
> 实验规程：[28-experiment-protocol.md](28-experiment-protocol.md)。
> 本文只回答一件事：**下一阶段该做什么、按什么顺序、为什么。**

## 0. 已落地（main @ `4983262`，无待审 PR）

| commit | 内容 |
|---|---|
| `7a49329` | 纯观测遥测：`cycle_summary` 新增 `book` / `tape` / `geometry` 三块；band 40→30 |
| `672c788` | `lag_analysis.py` 第 4 节：mark-best spread 分母分析 + 5 品种证据 |
| `b5dd32c` | 库存退出 ALO 优先 + IOC 兜底（**默认关 `alo_enabled = false`**）；费率实测入档；docs/14 canary 要求挂起 |
| `861e5b8` | SDK quick-start doctest 改 `no_run`（不再打生产网络） |
| `4983262` | `external_skew` 状态改为「已启用但未判定」 |

**已核实的场馆事实**：maker `0.0001` = 1bps，taker `0.0004` = 4bps，11 个 symbol 全部一致、
无已知返佣（`standx market symbols`，2026-08-20）。

## 1. 最重要的前提：三件事同时欠一个 live 窗口，而且互相污染

1. **band 40→30 的重新验证** —— 配置变更点，按 [28](28-experiment-protocol.md) 需新窗口。
2. **microprice 的幅度重测** —— accepted 判定成立于 band=40、叠加偏移永不被截断；
   band=30 下会被 band clamp 削掉，故 owner 裁决**只结转方向、不结转效果量**。
3. **`external_skew` 的隔离判定** —— 从未做过；owner 2026-08-21 裁决**保留启用、判定推迟**。

**第 3 项污染前两项。** 它开着期间跑出来的任何读数都含一个效果未知的机制，所以①②的结论
**只能表述为「含未判 `external_skew` 的基线的效果」**，不能表述为「当前基线的效果」。
这不是文字游戏——将来若把 `external_skew` 摘掉或判负，①②的数字都要重读。

## 2. 建议顺序

### 第一步：跑一个短验证窗口（1–2 小时就够）

**目的不是收益读数，而是回答两个二元问题**，它们决定后面所有工作的形状：

- **`public_trade` 到底带不带 `side`？** 这就是我们在 SDK 里加了 50 条原文 stderr 采样的
  原因（`public_trade raw sample: ...`）。`models::Trade` 的 `side` 是 `Option<String>`、
  `is_taker` 带 `#[serde(default)]`——**字段缺失会静默变成 `false`，那是伪造的方向信息**，
  所以代码从不用它反推方向，缺 `side` 一律记 `unknown_qty_5s`。若 venue 压根不推方向，
  taker 方向这条分析线作废，得换口径（例如成交价相对当时 touch mid 的位置）。
- **`cycle_summary.book` 里 `null` 占多大比例？** `book` 只在遥测那份深档的 `received_at`
  **精确等于**本轮决策所用 book 瞬间时才渲染，否则整块 `null`；REST 回退时恒为 `null`。
  这是刻意的（保证深度与决策所用盘口对齐），但若 `null` 比例可观，就把严格匹配从"门"
  降级成"标记"（照常渲染 + `matches_decision_book` 布尔）。**副作用**：当前设计下
  `book.age_ms` 有值时恒为 ~0，无信息量。

这个窗口同时就是①②欠的那个窗口（遥测是纯观测，不污染 A/B）。

**注意**：docs/14 的"每次变更都要新 canary"已被 owner 2026-08-20 **挂起**（判为现阶段
过于保守），所以不要把 canary 当阻塞前置。但 cleanup 终态规则、fail-closed 对账和
`STANDX_ENABLE_LIVE_MAKER` 解锁**仍然适用**。

### 第二步：Part C —— 成交时刻的盘口观测

设计见 [32](32-maker-observation-telemetry-design.md) 的 Part C（Part A/B 已落地）。
**必须排在第一步之后**，因为 `BookAtFill` 复用同一观测源和同一套严格匹配设计：成交样本
比周期样本稀疏得多，若 `book` 有相当比例是 `null`，周期样本还能靠数量扛过去，**成交样本
会直接报废**。先量覆盖率，再决定 Part C 用哪种匹配策略。

手法照抄现有的 `excess_bps_at_fill`：runtime state 持最近一次观测，在**全部** fill emit
站点带上（`runtime/cycle_flow.rs` 4 处、`runtime/recovery_flow.rs` 3 处、`recovery.rs` 的
`FillEmission`），用**一个结构体**而不是多个标量字段。`observation_age_ms` 必须落盘——
成交走 account stream、盘口走 public feed，两者异步，不带 age 的快照离线不可用。

### 第三步：开 `alo_enabled`（退出成本，约 3bps/次）

代码已在 main，**默认关**。判据是**执行成本不是 PnL**（3bps/次在当前规模会被 PnL 噪声
淹没）——见 [33](33-maker-exit-execution-cost-design.md)：maker 成交数量占比 ≥60%、单次
退出平均实现费率 4bps → ≤2.5bps；红线是退出未完成率不得把 `|position|` p95 推高、
不得出现留单退出导致的硬停或 cleanup fail-closed。

**先在 paper 开，再上 live。** 两个已知未验证点：
- **IOC 从未对本场馆实测过。** `TimeInForce::Ioc` 在枚举里，
  [API_DOCUMENTATION.md:318](../API_DOCUMENTATION.md) 列了 `GTC/ALO/IOC`（无 FOK），
  但拒单路径**故意不做兜底遮掩**，而是记成明确可观测事件。第一次开就盯这个。
- **paper 单边盘口那条修复没有单元测试**（它在 `maker_cycle` 巨型 async fn 内部，不是
  可测缝）。它只是移除了一条 `return Err`，状态一致性由代码阅读确认。要真覆盖需把退出
  决策拆出缝，属独立重构。

### 推迟：`external_skew` 隔离判定

判据仍冻结在 [29](29-maker-external-skew-design.md) 且有效（primary：逐笔签名
markout@30s 改善 ≥2bps、单侧 95% 下界 >0、4h block bootstrap；护栏：5s markout 不恶化
>1bps；AS 为诊断量）。**但臂定义已过期**——文中 baseline 臂是 microprice 晋级前的
`maker-guard-hype-candidate.toml`，须重挂到当前基线（baseline = 当前减 `external_skew`，
candidate = 当前）。样本量口径不变：实测 ≈14.4 fills/h，**两臂合计约 4 天 live**。

## 3. 一条尚未被消化的证据（可能比上面所有事都重要）

[mark-best 分母证据（2026-08-20）](evidence/mark-best-spread-denominator-2026-08-20.md)：

- **HYPE 分母最差**：锚定偏置 p50 = **+1.9bps**（mark 持续偏在 book mid 上方）、
  p99 半价差 **4.7bps**（厚尾远超 spread 预算）。
- **BTC 最健康**：锚定 p50 = **+0.3bps**（mark ≈ mid）。
- **各品种锚定偏置的方向和幅度都不同**（HYPE/ETH 持续为正；XAG mean −4.6 / p50 −0.7，
  左偏长尾）。

含义有两层，都还没有对应的行动项：

1. **八轮机制迭代（stage2/3/3v1/4/nonlinear/guard/external_skew/microprice）全都跑在
   最差的分母上。** 逆选的物理量是"距 best 的距离"，而我们所有几何量锚在 mark 上；
   HYPE 的 mark 系统性偏离盘口一个可量化的、非对称的距离。**换品种可能比再加一个机制
   有效得多**，而这件事从未被检验过。
2. **一个固定 λ 不可能适配所有品种。** 这对 microprice / external_skew 这类中心偏移机制
   是结构性约束，不是调参问题。

`geometry` 遥测（`clamped_to_touch` / `min_distance_to_touch_bps`）就是为了量化第 1 点
而加的。**注意：`ClampedToBand` / `ClampedToTouch` 的计数在 band=40 与 band=30 下不可
直接比较**（band 越宽，绑定约束越容易从 band 移到盘口），已记入 [30](30-maker-uptime-band-tightening-design.md)。

## 4. 陷阱清单（都真实发生过）

- **报测试门结果前先排除三种假象**：管道吞掉 cargo 退出码（`| tail` 的 `$?` 是 `tail` 的，
  必须 `set -o pipefail`）；切分支后 rustdoc 增量竞态（几百条 `can't find crate for ...`、
  exit 1，但**零测试 FAILED**，重跑即过）；曾有打生产网络的 doctest（已修）。
- **`distance_to_touch_bps` 的口径**已写进 `standx-maker` 的 doc comment：买单
  `(best_ask - price) / mark * 1e4`、卖单 `(price - best_bid) / mark * 1e4`。全仓 bps
  一律**以 mark 为分母**（`loss_bps` 也已统一），不要引入第二种分母。
- **`geometry` 的 `DroppedInfeasible` 盖了两种成因**（可行区间为空 vs rounding 后穿过
  盘口），离线分不开；每个 slot 只有一行，撤旧挂新时只看到新单，且 resting 行与新挂行
  的 outcome 都是 `placed`（无 `is_resting` 标记）。
- **`scripts/spread_percentiles.py` 与 `lag_analysis.py` 第 4 节功能重叠**，前者未入库
  （工作树里可能仍有未跟踪副本）。前者独有的角度是**"最差一侧"统计量**：暴露量是
  `max(mark-best_bid, best_ask-mark)` 而不是 touch 宽度——mark 离哪一侧的 best 更远，
  我们在那一侧就更容易成为新的最优报价而被打。要合并请先裁决，不要留两份实现。
- **`external_skew` 开着**（见 §1）。任何窗口结论都要带这个限定。
