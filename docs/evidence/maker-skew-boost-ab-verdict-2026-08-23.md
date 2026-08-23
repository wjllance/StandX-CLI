# skew boost 3→6 A/B 判定报告（2026-08-23，不可变）

**判定：`ab_completed_not_accepted`**（跑完、样本达标、经济门槛未过）。
裁决人：release owner（2026-08-23「停」）。判据预注册于
[docs/35](../35-maker-skew-boost-ab-2026-08-21.md)（开跑后冻结，未修改）。
每日记录：[08-22](maker-skew-boost-ab-daily-2026-08-22.md) /
[08-23](maker-skew-boost-ab-daily-2026-08-23.md)。

## 窗口与样本

- 2026-08-21T15:02Z – 2026-08-23T15:51Z（~48.8h 墙钟），**12 对 24 条完整臂**
  （判据要求 3 对 6 臂，超额 4 倍）。臂长 7200s，交替无限循环至 owner 叫停。
- 作废片段 2 个，均不入分析：首启片段（15:01:19Z，OO 凭据漏导出，~2.5min
  0 成交）；停机时的在跑臂 baseline-150730Z（44min，3 fills，PnL −0.05，
  未满 7200s 窗口）。
- 配置哈希全程不变：baseline `ba8849df…`，candidate `75ba4660…`。

## 冻结判据打勾（终值）

**运维门槛：全过**
- uptime：baseline 各臂 81–97%，candidate 67–96%，全部 ≥50% 绝对值
- 零安全违规：24/24 臂 FLAT 收尾（orders=[] positions=[]）；全部
  reconciliation freeze 在 3s 窗口内 recovered 且 expected==observed；
  无未解释仓位失配、无残余单、无 fail-open、无 critical/halt；
  `inventory_exit_crossed`×2（ALO 被穿→升级 IOC）为设计内路径
- manifest：24/24 VALID，cycle 序列完整

**经济门槛：两条均不满足**
- 净 PnL/笔：baseline 116 笔 Σ−0.731（**−0.0063/笔**）vs candidate 298 笔
  Σ−3.609（**−0.0121/笔**），candidate 约为 baseline 的 1.9 倍亏损率。
  可比较逐对 10 对（p6/p8 一臂零成交剔除）：candidate 更差 7 对、更好 3 对。
- open/add 侧 mo30 改善 ≥1bps：**方向相反**。baseline −7.10bps（n=57）vs
  candidate −13.66bps（n=74），恶化 6.56bps。

**记录项**：reduce 侧 mo300 baseline −4.54bps（n=56）vs candidate
−3.68bps（n=220），candidate 改善 0.86bps，方向符合机制预期但量级不够。

**红线**：无越线。减仓侧全程带内；加仓侧 16bps 出 SIP-5A 带为已登记代价。

## 机制结论

1. **「减仓侧拉近 = 更快回平回血」假设被证伪。** boost 6 把 1 单位库存态的
   减仓单从 mark−2bps 拉到 mark−8bps，退出确实更快（candidate 成交 80% 是
   reduce 单、周转明显加快），但两笔代价吃掉收益：每笔 capture 摊薄
   （pooled cap +1.2 vs +2.9bps），且趋势市况下周转加快直接放大毒性亏损的
   累计（两条 trending 臂贡献 candidate 亏损的 53%）。
2. **加仓侧 16bps 不是保护，是逆选择浓缩器。** 推远后仍成交的加仓单集中在
   更大的逆向漂移里（drift15s 最 adverse 桶 mo30 −6.46bps），open/add mo30
   不降反升。skew 幅度旋钮不能把毒性成交「推掉」，只能把它们浓缩。
3. **安静市况下结论不变**：08-22 后半至停机 fill rate 降至 0–9/h，
   candidate 每笔亏损率仍为 baseline ~2 倍——非单一 episode 伪影。
4. 对 owner 此前「吃单后立即触发库存退出」设想的回答：更快退出方向的收益
   （reduce mo300 +0.86bps）真实存在但太小，抵不过任何把加仓侧推远或把
   减仓侧拉近的连带代价。库存退出维持 70% 阈值触发不变。

## 处置

- 配置无回滚必要：baseline 配置（boost=3.0）全程未动，`maker-btc-boost6.toml`
  作为实验档案保留，不进入日常运行。
- 编排器已于 2026-08-23T15:51Z SIGTERM 停止，末臂 cleanup 后独立复核
  `account positions` / `account orders` 均为 `[]`。
- 附带工具修复（本实验发现）：`run_maker_stage2_ab.sh` 符号白名单 +BTC-USD、
  preflight case (i)、run_arm webhook 透传（`136fb46`）；
  `maker_markout_ab.py` 零成交臂崩溃窄修复（`179b7cd`）。
- 未判项登记：无新增（绝对 PnL 水平仍挂在 docs/28 既有行，走 27 号手册）。
