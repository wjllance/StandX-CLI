# 外部领先价连续偏移（`[external_skew]`）立项设计（2026-07-29 草案）

状态：**草案，待 release owner 裁决**。live 时间片前置：基线 PnL 采集窗口
（[27 号手册](27-maker-baseline-pnl-collection-runbook.md)，run
`baseline-pnl-20260728T081712Z`）2026-07-31T08:17Z 收尾之后才可上 A/B。
窗口内不碰冻结基线，本文档与离线分析不消耗 live 时间片。

上游口径：[18-maker-strategy-roadmap.md](18-maker-strategy-roadmap.md)；
信号源机制沿用 [22 号设计](22-maker-stage3v1-guard-design.md)的
`[external_guard]` 与 [24 号独立立项](24-maker-guard-spinoff-design.md)。

## 立项依据（证据链）

### 1. 亏因定位：markout，不是 capture，不是成本

基线 PnL 采集 run 的净额恒等式分解（2026-07-29T03:39Z 时点，~19.4h，
29120 cycles / 289 fills）：

| 项 | 数值（quote） |
|---|---|
| gross spread capture | +0.745（passive capture +5.0 bps） |
| inventory MTM（重估残差） | **-1.703** |
| 手续费 | -0.150 |
| funding | -0.001 |
| **净 PnL** | **-1.109** |

markout：1s +0.07 / 5s -3.0 / 30s **-5.8 bps**。capture 恰好被逆向选择吃光
还有余：单笔真实 edge ≈ +5.0 − 5.8 ≈ -0.8 bps。价格上行段 MTM 项仍持续恶化，
说明不是方向性敞口，是成交时点被系统性逆向选择。所有只能改 capture 或改方差
的旋钮都不对症——只有改 markout 的旋钮对症。

### 2. 离线检验（2026-07-29，同一 run 日志，纯只读）

`cycle_summary.external_divergence_bps`（excess = 原始 divergence 经 300s 半衰期
EWMA 扣除静态 basis 后）每轮都在日志里，fill 事件带 `side/price/mark_at_fill/
event_time_ms`——**这是 microprice 类候选做不到的回溯检验**（盘口量从未落日志）。

**(a) excess 预测未来 mark 移动：斜率 ≈ 1，线性延伸到 ±2bps 桶，非尾部现象。**

| excess 桶（bps） | n | 30s 后 mark 实际移动（bps） |
|---|---|---|
| -10.2 | 71 | -11.6 |
| -6.7 | 217 | -6.7 |
| -4.8 | 1265 | -3.7 |
| -2.8 | 4751 | -2.0 |
| ~0 | 16300 | -0.06 |
| +2.9 | 4610 | +1.8 |
| +4.8 | 1453 | +4.3 |
| +6.7 | 324 | +5.0 |
| +9.6 | 117 | +8.6 |

HL 对 StandX mark 的领先在 ~30s 内被吸收，与 3s 报价周期速率匹配。
二值 guard（enter=10）把 ±2~10bps 区间里与尾部几乎同质量的信号扔掉了。

**(b) 成交时点的逆向选择是系统性的**：买单成交时 excess 均值 **-3.2bps**
（n=145），卖单 **+3.65bps**（n=144），无条件均值 +0.03。被动单恰好在
"外部价预告逆向移动"的时刻被吃。

**(c) 反事实现金改善（上限口径）**：中心偏 `λ×excess` 且假设全部 fill 仍成交，
λ=0.25 / 0.5 / 1.0 → **+0.14 / +0.27 / +0.54**，对照当前净亏 -1.1，
即可回收 25~50%。上限口径的诚实边界见"风险与已知限制"。

**(d) 残余毒性**：excess≈0 桶的逐笔签名 markout@30s 仍为 **-10.6bps**，
与其他桶大致持平——外部领先价只解释毒性的一部分，其余是场内流量。
microprice（盘口量失衡）类候选保留为第二优先，等 depth 观测字段落地后评估；
两者不互斥。

### 3. live 先验与阶段 4 的边界

- guard 臂 2026-07-27 accepted（成本侧 + 信号质量判）：连粗糙的二值化版本都有
  正效果，"外部价领先 StandX"这一事实已有 live 证据，被扔掉的只是幅度信息。
- **不在阶段 4 的终止链条上**：阶段 4 被否的是"drift 前倾的纯测量 = 无条件加宽
  → 加宽 live 为负"。外部价的信息来自另一个场馆，连续偏移不是加宽；18 号文档
  要求"重启须以新的独立证据立项"，guard accepted（07-27）与本节 (a)(b) 正是
  阶段 4 终止（07-20）之后到达的新独立证据。

## 机制设计（`[external_skew]`）

报价梯子已有唯一中心锚点：`skew_center_with`（`standx-maker/src/lib.rs`，
库存 skew，含 nonlinear 变体）。外部偏移在同一锚点上**乘性复合、置于库存
skew 之后**：

```
center = skew_center_with(cfg, nonlinear, mark, position)
         × (1 + shift_bps / 1e4)
shift_bps = clamp(λ × excess_bps, ±cap_bps)     |excess| < dead_zone 时为 0
```

- **excess 来源**：完全复用 guard 链路——同一 HL midPx feed、同一 300s 半衰期
  EWMA basis 扣除、同一 `max_age_ms` 新鲜度判定。不引入第二份配置、第二条订阅。
- **失败方向 OPEN**（与 guard 一致）：样本缺失 / 过期 / 非有限 → shift=0，
  报价照常。外部信号永远是优化，绝不能成为新的停报价来源。
- **与二值 guard 的关系**：连续偏移覆盖 `|excess| < enter_bps` 的中段；尾部
  （≥10bps）单侧 stale 是质变（价位即将被打穿），保留现有压制语义作为尾部保险。
  **候选臂 = 冻结基线（nonlinear_skew + guard 双开）+ `[external_skew]`，
  其他一个字节不动。**
- **band / no-cross / refresh 照旧**，以新 center 为锚。`refresh_bps=4` 天然
  限流：λ=0.5 时 excess 2~6bps 产生 1~3bps 偏移，不触发重挂，平静期不增加
  撤单 churn；死区进一步防止漂移累积造成的抖动。
- **配置面**（`[external_skew]`，默认关，缺省即 replay 等价）：
  - `enabled`（默认 false）
  - `lambda`（偏移系数，预注册单臂 0.5）
  - `cap_bps`（偏移上限，建议 6~8，必须 < enter_bps=10，保证尾部仍归 guard 管）
  - `dead_zone_bps`（建议 1.0）

## 风险与已知限制

- **反事实估计的选择效应盲区**：上限口径假设全部 fill 平移后仍成交。真实世界
  里中心偏移留下最毒的 fill（价格打穿新旧两档）、丢掉边际好 fill——符号与
  量级日志不可算，**只能 A/B 实测**。预注册判据必须建在 live markout 上，
  不得用离线反事实数作为晋级证据。
- **basis 漂移**：静态 basis（HYPE 观测 ~-14bps 场馆溢价）若缓慢漂移，300s
  EWMA 跟踪期内的残差会变成持续性偏移 → 单向推报价 → 库存偏斜。`cap_bps` +
  `dead_zone_bps` 是第一道防线；A/B 期间把库存均值/方差列为 guardrail。
- **EWMA 滞后**：basis 扣除是慢变量，excess 是快变量，滞后不影响 excess 的
  时效（guard 轮已论证）。
- **不解决全部毒性**：见立项依据 (d)。本机制的目标是把 30s markout 从 -5.8
  拉回一部分，不是归零。

## 遥测（归因用）

- `cycle_summary` 新增 `external_skew_shift_bps`（本轮实际应用的偏移，含 0）。
- `external_skew` 事件（shift 越过 dead_zone 或方向翻转时记录，风格同
  `external_guard` 事件）。
- 现有 `external_divergence_bps` / `external_basis_bps` / guard 事件不动。

## 实现边界

- `standx-maker`：纯函数 `external_skew_shift_bps(excess_bps, cfg) -> f64`
  （clamp + 死区，无状态、无 I/O、无时钟），以及 center 复合点；replay 等价
  （默认关）。测试：死区/clamp/符号方向/fail-open（无样本 → 0）、与库存 skew
  复合的确定性。
- `standx-cli`：配置解析（`[external_skew]` → domain）、把 guard 链路已算好的
  excess 喂给 planner、遥测字段与事件。不新增订阅、不改 feed。
- 不改动：guard 判定语义、basis half-life、refresh/band/no-cross、输出契约
  （只增不改）。

## 验收判据（预注册，live 时间片 A/B）

两臂：baseline = 冻结基线（`maker-guard-hype-candidate.toml` 原样）；
candidate = baseline + `[external_skew] enabled, lambda=0.5, cap_bps=8,
dead_zone_bps=1`。跑法沿用 guard 轮的编排器与时长口径（每臂 ≥36h 或
≥250 笔 fill，以先到后补为准，启动时写入授权文本）。

- **primary**：逐笔签名 markout@30s，candidate 相对 baseline 改善 ≥ 2bps
  （约相对 1/3），且 5s markout 不恶化超过 1bps。
- **guardrail**（任一命中即 rejected，不论 primary）：
  - passive capture 掉幅 > 1bps；
  - 撤单率（cancel/quote-hour）上涨 > 50%；
  - 时间加权库存均值 |position| 或 p95 显著上偏（basis 漂移征兆）；
  - uptime 下降 > 2pp。
- **PnL 依旧不作晋级条件**（沿用 skew/guard 两轮口径），但逐日记入报告。
- 判定：accepted → 并入新冻结基线；rejected → 回滚配置，证据归档。
- **不做多 λ 赛马**：单臂 λ=0.5（反事实估计中点）；accepted 后如需调优另行
  立项。

## 明确不做

- 不用连续版替换二值 guard（尾部压制语义保留，见机制设计）。
- 不改 `basis_half_life_secs`（300s）——按 guard 判定报告声明，基差半衰期
  评估按各自证据另行立项。
- 不做多场馆加权领先价（单 HL 源，与 guard 同源）。
- 不在基线 PnL 采集窗口内部署（含"只加遥测"——运行中进程不重启、不换二进制）。

## 失效条件

- A/B 判出 rejected（回滚，本立项关闭）；
- 基线 PnL 窗口给出明确为负且 markout 无改善空间的结论（回 18 号重排）；
- HL feed 语义或 StandX mark 口径变化（excess 定义失效，需重新校准 basis）。
