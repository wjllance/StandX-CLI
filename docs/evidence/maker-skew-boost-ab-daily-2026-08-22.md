# skew boost 3→6 A/B 每日记录（2026-08-22）

实验立项与预注册判据：[docs/35-maker-skew-boost-ab-2026-08-21.md](../35-maker-skew-boost-ab-2026-08-21.md)。
覆盖窗口：2026-08-21T15:02Z（首臂启动）至 2026-08-22T08:53Z，已完成 **4 对 8 臂**
（判据要求 3 对 6 臂，样本量已满足）。第 9 臂（baseline-20260822T070433Z）在跑，
不入本期统计。作废片段（15:01:19Z 首启 0 成交）不入。

数据来源：`var/standx/stage2-{baseline,candidate}-*.ndjson` +
`scripts/maker_markout_ab.py`（pooled markout/聚类/归因）+ 分侧拆分脚本
（回放 fill 序列得仓位，按成交前仓位分 open/add/reduce；mo 以 fill 的
mark_at_fill 为基准、cycle mark 序列取 horizons）。

## 每臂概览

| 臂 | 成交 | 成交/h | uptime | 净 PnL | mo30 均值 | cap 均值 |
|----|------|--------|--------|--------|-----------|----------|
| baseline 15:02 | 28 | 14.0 | 81% | +0.005 | −3.11 | +3.14 |
| candidate 17:02 | 30 | 15.0 | 82% | −0.290 | −4.25 | +0.69 |
| baseline 19:02 | 22 | 11.0 | 86% | −0.135 | −3.39 | +2.81 |
| candidate 21:03 | 119 | 59.5 | 68% | −1.132 | −4.45 | +1.18 |
| baseline 23:03 | 12 | 6.0 | 83% | −0.165 | −6.78 | +2.47 |
| candidate 01:03 | 19 | 9.5 | 87% | −0.203 | −2.77 | +1.57 |
| baseline 03:03 | 13 | 6.5 | 88% | +0.020 | −3.26 | +2.73 |
| candidate 05:04 | 78 | 38.9 | 67% | −0.783 | −4.43 | +1.36 |

pooled：baseline n=75，ΣPnL −0.276（−0.0037/笔）；candidate n=246，
ΣPnL −2.409（−0.0098/笔）。

candidate 的两条重成交臂（21:03 与 05:04，119/78 笔）恰逢 trending 市况
（臂内 mark 行程 ~1000/2000 美元），baseline 同期臂成交率 6–15/h。
市况混杂是解释难点，block bootstrap 对 baseline 报 "too few blocks"
（4 个 2h 臂 = 4 块），两臂的统计功效不对称，读数按方向性证据处理。

## 判据打勾（预注册于 docs/35，冻结）

**运维门槛**
- [x] 双臂 uptime ≥50%：baseline 81–88%，candidate 67–87%（绝对值，全臂达标）
- [x] 零安全违规：无未解释仓位失配（position_reconciliation 全部 3s 窗口内
  recovered 且 expected==observed：baseline 14 次 / candidate 37 次）；
  无残余单（8 臂 arm complete 均 orders=[] positions=[]）；无 fail-open；
  无 critical/halt。`inventory_exit failed`×2/臂×3 臂 = 等 venue 确认期间的
  防重复提交门，设计内行为，非 venue 拒绝。volatility_breaker 进出成对
  （baseline 1 次 / candidate 6 次），market_data degraded 1 次已 recovered。
- [x] manifest valid + cycle 序列完整：8/8 完成臂 VALID（在跑臂 INVALID 属正常）

**经济门槛**
- [ ] candidate 净 PnL/笔 优于 baseline：**不满足**。pooled −0.0098 vs −0.0037；
  逐对：pair1 更差（−0.0097 vs +0.0002）、pair2 更差（−0.0095 vs −0.0061）、
  pair3 更好（−0.0107 vs −0.0138）、pair4 更差（−0.0100 vs +0.0015）→ 3/4 对为负
- [ ] open/add 侧 mo30 均值改善 ≥1bps：**不满足，方向相反**。
  baseline −6.80bps（n=36）vs candidate −12.22bps（n=48），恶化 5.42bps。
  与逆选择浓缩一致：加仓侧推到 16bps 后，仍成交的单子集中在更大的逆向
  漂移里（drift15s 分桶：d<−4 桶 mo30 −6.46，candidate 在该桶占比更高）。

**记录项（不作门槛）**
- reduce 侧 mo300：baseline −3.90bps（n=38）vs candidate −3.07bps（n=195），
  改善 0.83bps，方向如机制预期（减仓侧更近 → 退出后 mark 继续有利走的
  机会成本减小），但量级远小于 open/add 侧的恶化。

**红线**
- 加仓侧 16bps 出 SIP-5A 带：已登记的已知代价（SIP 收入≈0 已关闭为判据）。
- 减仓侧须在 10bps 带内：设计保证（1 单位 = 8bps，≥2 单位两臂同顶 cap），
  无越线。

## 机制层观察（解释用，不改判据）

- candidate cap 均值 +1.21 vs baseline +2.86：减仓侧 8bps 位退出比 2bps 位
  每笔少捕获 ~1.7bps，且 candidate 成交构成里 reduce 占 80%（195/246），
  加权后每笔捕获被摊薄——"更快退出"的代价直接体现在 capture 上。
- candidate 成交率更高（pooled 30.8/h vs 9.4/h）主要来自两条 trending 臂：
  1 单位库存态在趋势中反复出现，boost 6 把减仓单拉近 mark 8bps → 更快被
  吃 → 更快回到零库存 → 更快重新开仓。周转加快本身放大每笔毒性亏损的
  绝对累计（ΣPnL −2.41 vs −0.28）。

## 结论输入（供裁决人）

预注册样本量（3 对）已跑完（4 对）。两条经济门槛均不满足，运维门槛全过。
按冻结判据的映射，当前读数指向 `ab_completed_not_accepted`。最终判定与
是否停编排器（循环仍在继续，每对约 4h+wind-down）由 release owner 裁决。
