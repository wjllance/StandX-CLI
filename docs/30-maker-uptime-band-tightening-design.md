# 阶段 6：SIP-5A ±10bp 带内 uptime 达标——渐进式参数收缩设计

## 背景与问题

当前在跑的 external_skew candidate 臂（`maker-external-skew-hype-candidate.toml`，
HYPE-USD）SIP-5A uptime 未达标。2026-08-04 用运行数据实测：对 ndjson 逐周期重建
挂单状态，以 mark 价 ±10bp 为合格带，**双边同时在带内的周期仅占 45.8%**
（4743/10354；窗口 05:12–14:21Z，12397 cycles）。分侧：买侧 60.4%、卖侧 85.6%。

[22-maker-stage3v1-guard-design.md](22-maker-stage3v1-guard-design.md) 早已记录
"现行生产配置本就不满足 SIP-5A 带内约束"，当时 owner 裁决目标函数为"少亏"、
SIP-5A proximity 损失作为已知代价接受。2026-08-04 owner 将带内 uptime 提为
约束条件，目标 **≥75% 时间双边在 ±10bp 内**，并要求渐进收缩、不一步到位。

## 根因（三点叠加）

1. **`band_bps = 30.0`**：策略自身撤单带是场馆合格带的 3 倍，挂单出 ±10bp 后
   不被撤、继续挂在带外不计分。
2. **报价中心偏移叠加**：`spread_bps = 8.0` 起步，叠加 inventory skew（8bp）、
   nonlinear skew（cap 12bp）、external skew（cap 8bp），极端情形中心偏 28bp；
   常态偏移使远侧报价落在 9–13bp（实测买侧距离中位 9.2bp、p90 12.8bp）。
3. **anti-flicker 间隙**：`refresh_bps = 4.0`，挂在 8bp 处的单子 mark 漂 2bp 即
   出场馆带，但要漂过 4bp 才重报，间隙时间挂在带外。

另注意：机器人自报的 `time_weighted_uptime_pct`（当前 98.8%）按 `band_bps`
定义"带内"，不能作为判定依据。判定一律用 ±10bp 重算（见"测量口径"）。

## 离线回放与参数预测

方法：取 2026-08-04 candidate 臂 ndjson 的真实 mark 与 `skew_shift_bps` 序列
（12397 cycles，含 guard 激活事件），按策略语义（band 撤单、refresh 重报、
band clamp）回放各参数组合，统计双边带内占比。模型校准：当前参数回放值
43.4% vs 实测 45.8%，偏差可接受。skew 机制参数全程不动，假设 shift 序列不变。

**按 [28-experiment-protocol.md](28-experiment-protocol.md) 硬规则 1，以下数字
只用于排除候选和排优先级，不作为晋级依据；晋级以 live 读数为准。**

| 变体（相对现状单行变更） | 回放双边带内 | 撤单/100 周期 |
|---|---|---|
| 现状 spread8 / band30 / refresh4 | 43.4%（实测 45.8%） | 9.6 |
| `spread_bps 8→7` | 66.4% | 9.7 |
| `refresh_bps 4→3` | ~55%（另测 band30 组合 72.9% 见下） | 19.0 |
| `band_bps 30→12` | 49.1% | 13.5 |
| spread7 + refresh3（两步叠加） | 72.9% | 13.4 |
| spread7 + refresh3 + band12（三步叠加） | 75.2% | 15.2 |
| 备用：spread7 + band11（refresh4） | 79.7% | 13.5 |
| 备用：spread6 / band30 / refresh4 | 78.7% | 9.7 |

关键读数：先动 band 收益最小（49%）且先推高撤单；spread 是最大杠杆且撤单
不变；三步叠加恰好落在目标线上，撤单 +58%。

## 分阶段方案（每步单行配置变更）

每步变更 `maker-external-skew-hype-candidate.toml` 的**一行**，其余配置（含全部
skew 机制参数）不动。每步作为新窗口开跑，manifest 记录变更点 UTC 时间戳与
配置 sha256；进行中窗口作废，不跨变更点拼接判读。

| 步骤 | 改动 | 预测带内 | 预测撤单/100cyc |
|---|---|---|---|
| Step 1 | `spread_bps = 8.0 → 7.0` | ~66% | ~9.7 |
| Step 2 | `refresh_bps = 4.0 → 3.0` | ~73% | ~13.4 |
| Step 3 | `band_bps = 30.0 → 12.0` | ~75% | ~15.2 |

Step 3 后实测仍 <75% 时启用备用线（需 owner 再裁决，同为单行变更）：
`band_bps → 11.0`（预测 ~80%）或 `spread_bps → 6.0`（预测 ~83%，叠加态）。

任一步实测大幅低于预测（偏差 >10pp）说明回放假设失效（shift 序列漂移或
市况切换），**停下复核，不机械推进下一步**。

## 预注册判据（2026-08-04 入库，开跑后冻结）

- 目标函数：**带内 uptime 达标** —— ≥75% 时间双边报价在 mark ±10bp 内，
  同时撤单率受控；不以净 PnL 为目标。
- 臂长与样本量：每步 ≥1 个完整 12h 窗口（沿用现有 12h 块约定）；
  判定用窗口内全部完整 cycle。
- 裁决人：release owner。

**运维门槛（任一不过 → rejected）**
- [ ] 零安全违规（无未解释仓位失配、无残余单、无 fail-open）
- [ ] manifest valid，cycle 序列完整
- [ ] skew 机制配置（nonlinear_skew / external_skew / external_guard / skew_bps）
      三步全程字节不变

**经济门槛（按步判定）**
- [ ] Step 1：±10bp 双边带内 ≥ 60%，且撤单/100cyc ≤ 12
- [ ] Step 2：±10bp 双边带内 ≥ 70%，且撤单/100cyc ≤ 16
- [ ] Step 3：±10bp 双边带内 ≥ 75%，且撤单/100cyc ≤ 20

**红线（越线即 rejected，与门槛分开写）**
- 每步只允许差一行配置；跨步不得合并变更。
- 不得以停挂任一侧换取带内数字（guard 之外的 side suppression 时间占比不得
  显著上升：two-sided 周期占比 ≥ 95%）。
- 撤单/100cyc > 20（约现状 2 倍）任一步即停，回到 owner 裁决。

**明确不作为晋级条件的指标（必填，不许留空）**

| 指标 | 为什么不作条件 | 谁在什么时候补 |
|------|----------------|----------------|
| 净 PnL / spread capture 变化 | spread 收窄改变成交经济学，但臂间市况不可比，本序列只回答 uptime 问题 | 登记未判项；用冻结基线对照窗或 [27 号手册](27-maker-baseline-pnl-collection-runbook.md) 绝对读数补 |
| SIP-5A $/MH 实际收益 | 当前规模收入≈0，28 号表已关闭 | 规模发生量级变化时重开（沿用 28 号表原行） |
| 成交率/成交笔数变化 | 与 PnL 同理，市况不可比 | 随净 PnL 未判项一起补 |

## 测量口径

判定指标从 ndjson 重算，不用机器人自报 uptime：

1. 逐行读 `place`/`hold`/`cancel` 事件重建每周期各侧挂单价格与 mark；
2. 每周期判定买/卖侧 `|price - mark|/mark ≤ 10bp`；
3. 双边带内占比 = 两侧均在带内的周期数 / 双侧均有挂单的周期数；
4. 撤单率 = cancel 事件数 / 周期数 × 100，按原因分解留档
   （outside_band / mark_moved / side_suppressed 占比必须随步可解释）。

机器人自报的 `time_weighted_uptime_pct` 仅作旁证：Step 3 后其定义带（12bp）
仍宽于场馆带，预计读数系统性偏高于 ±10bp 重算值，属预期，不算异常。

## 风险与备注

- 预测基于单日 ~9h 行情，波动 regime 切换会使实测偏离；判据门槛已相对预测
  留 5–6pp 余量。
- 回放未建模周期内（3s 间隔之间）的 mark 抖动，实测带内占比大概率略低于
  回放值。
- Step 3 的 band12 仍宽于场馆 10bp：报价允许落在 10–12bp 区间（带外），这是
  预测中 75% 而非 100% 的主要来源；备用线 band11 即为此准备。
- 当前 candidate 臂跑到一半的 A/B 窗口在 Step 1 开跑时作废，external_skew
  机制本身的判定不受影响（其判定窗口另行截取）。
