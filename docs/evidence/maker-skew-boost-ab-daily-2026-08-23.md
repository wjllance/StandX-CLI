# skew boost 3→6 A/B 每日记录（2026-08-23）

实验立项与预注册判据：[docs/35-maker-skew-boost-ab-2026-08-21.md](../35-maker-skew-boost-ab-2026-08-21.md)。
昨日记录：[maker-skew-boost-ab-daily-2026-08-22.md](maker-skew-boost-ab-daily-2026-08-22.md)。
覆盖窗口：2026-08-22T07:04Z 至 2026-08-23T08:55Z，新完成 **6 对 12 臂**
（pair 5–10）。累计 **10 对 20 臂**，编排器仍在跑（第 21 臂
baseline-20260823T070645Z 进行中，不入本期）。

本窗口市况明显转静：成交率 0–9 笔/h（昨日峰值 59.5/h），两个臂零成交
（candidate-130505、baseline-190539）。零成交臂使 maker_markout_ab.py
崩于 NoneType 格式化，已做窄修复（打印 n/a，`179b7cd`）。

## 每臂概览（本窗口 12 臂）

| 臂 | 成交 | uptime | 净 PnL | mo30 均值 |
|----|------|--------|--------|-----------|
| baseline 07:04 | 6 | 89% | −0.135 | −7.19 |
| candidate 09:04 | 12 | 86% | −0.213 | −3.99 |
| baseline 11:04 | 2 | 93% | +0.020 | +0.27 |
| candidate 13:05 | 0 | 95% | 0.000 | n/a |
| baseline 15:05 | 4 | 93% | +0.004 | −6.84 |
| candidate 17:05 | 4 | 96% | −0.058 | −5.66 |
| baseline 19:05 | 0 | 97% | 0.000 | n/a |
| candidate 21:05 | 2 | 93% | −0.010 | −3.88 |
| baseline 23:06 | 2 | 96% | −0.075 | −7.96 |
| candidate 01:06 | 3 | 96% | −0.028 | −6.95 |
| baseline 03:06 | 12 | 90% | −0.103 | −1.33 |
| candidate 05:06 | 18 | 82% | −0.619 | −11.24 |

本窗口 pooled：baseline n=26，ΣPnL −0.290（−0.0112/笔）；candidate n=39，
ΣPnL −0.928（−0.0238/笔）。逐对（剔除一臂零成交的 pair6/pair8）：
candidate 更好 2 对（p5、p9），更差 2 对（p7、p10）——小样本噪声主导，
方向不变。

## 判据打勾（冻结判据，docs/35）

**运维门槛**
- [x] 双臂 uptime ≥50%：baseline 89–97%，candidate 82–96%
- [x] 零安全违规：12/12 臂 residual flat（venue=0，needs_operator=false）；
  reconciliation freeze 6 次全部 recovered 且 expected==observed；
  volatility_breaker 进出成对 3 次；baseline-030624 出现 1 次
  `inventory_exit_crossed`（ALO 被穿 → 升级 IOC 的设计内路径，exit
  confirmed，臂末 FLAT）；无 critical/halt
- [x] manifest 12/12 VALID

**经济门槛（读数按累计 10 对 20 臂）**
- [ ] candidate 净 PnL/笔 优于 baseline：**不满足且差距扩大**。
  累计 baseline 101 笔 ΣPnL −0.566（−0.0056/笔）；candidate 285 笔
  ΣPnL −3.337（−0.0117/笔）。可比较逐对累计：candidate 更差 5 对、
  更好 3 对。
- [ ] open/add 侧 mo30 改善 ≥1bps：**不满足，方向相反**。
  累计 baseline −6.52bps（n=50）vs candidate −13.05bps（n=68），
  恶化 6.53bps。本窗口单看更极端（−15.03 vs −5.80，n=20/14）。

**记录项**
- reduce 侧 mo300：累计 baseline −5.60bps（n=50）vs candidate −3.52bps
  （n=214），candidate 改善 2.08bps，方向持续符合机制预期。

**红线**：减仓侧带内（设计保证），加仓侧 16bps 出 SIP 带为已登记代价。

## 结论输入（供裁决人）

判据映射不变：`ab_completed_not_accepted`。昨日至此累计 candidate 多亏
约 2.77 DUSD（−3.34 vs −0.57），其中绝大部分来自两条 trending 臂
（昨日 21:03/05:04）。本窗口安静市况下 candidate 的每笔劣势仍在
（−0.0238 vs −0.0112），不是单一 episode 伪影。编排器继续空转每 4h
产生一对新臂，边际信息已低，再次提请 release owner 裁决停机与正式判定。
