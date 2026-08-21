# 毒性成交归因：fresh/stale + 加/减仓拆分，skew 幅度裁决的输入（2026-08-21）

承接 [maker-perfill-stop-tail-2026-08-21](maker-perfill-stop-tail-2026-08-21.md)（81% 亏损在
最差 decile）。回答两个问题：tail 是 fresh 还是 stale；以及按成交性质（open/add/reduce）
拆开后，亏损究竟在哪一侧。数据：BTC run1–run4 合并（205–219 笔被动成交）。

工具：`scripts/maker_refresh_born_attribution.py`（pooled 表）+ 自写加/减仓拆分
（回放 fill 序列得仓位，按成交前仓位符号分 open/add/reduce，复用
`maker_markout_ab.attribution_rows` 的 mo30/mo300）。

## 结果

**fresh/stale（pooled，219 笔归因）**：

- tail（mo300 最差 decile，n=20）单龄中位 **4s**（其余 10s），挂出时 drift_place15s
  均值 **+5.75bps**（其余 +2.87），方向一致率 60–65%，毒性组内 drift×mo30 相关
  pearson +0.46/+0.61（其余组 ≈0）。**tail 是"挂进已在漂移的市场"，确认。**
- 但 refresh-born 标签区分度弱：tail 80% vs 其余 73%（+6.7pp，Fisher p=0.60）——
  全部挂单的 84.3% 都是 refresh-born，标签近似无效。
- 毒性聚簇：P(下一笔 toxic | 上一笔 toxic) = 33.3% vs 基准 9.4%（Fisher p=0.0027）——
  毒性按时段/市况成簇，不是均匀随机。
- table 7 的简单 drift/mo 代理触发器 P(tox|fire) 最高仅 15.8%（基准 9.4%）——
  没有干净的单行触发器。

**加/减仓拆分（205 笔有完整 mo300）**：

| 成交性质 | n | mo30s mean / 负占比 | mo300s mean |
|---|---|---|---|
| open（开新仓） | 69 | **−8.66 / 84%** | −3.64 |
| add（加仓） | 35 | −7.11 / 69% | −0.58 |
| reduce（减仓/退出） | 118 | −5.64 / 75% | **−11.38** |

tail（mo300 最差 20 笔）构成：**reduce 11 / open 6 / add 3**。

## 解读

- mo300 tail 里占多数的 reduce 单是"退出后 mark 继续朝有利方向走"的**机会成本**
  （少赚），不是开仓被套的已实现亏损。对 PnL 真正咬人的是 open/add 在 30s 的
  −8.66/−7.11bps（84%/69% 为负）——刚开仓就被打穿的水下包袱。
- 对"skew 幅度该强还是弱"的裁决含义：加强 skew（boost 3→6）把**加仓侧推远**
  （压最痛的 open/add 格子）同时把**减仓侧拉近**（加重 mo300 −11.38 的过早退出桶）。
  一个旋钮耦合一正一负两个效应，离线无法定净符号 → 按硬规则 1 只能实测。
- 数据指向的干净杠杆是**漂移期间抑制新开仓/refresh**（post_suppressed replay、
  staleness 门）：只切 open/add 的 84%-负格子，不碰减仓侧。

## 处置

- 「skew boost 加强（3→6）」：从"建议做"降级为**歧义项，仅可作为 A/B 臂实测**，
  判据必须含净 PnL/笔 + open/add 侧 mo30 改善 + reduce 侧 mo300 恶化量级三个读数。
- 「漂移门控报价」升为成交端防御的优先候选（roadmap 的 post_suppressed replay 先行，
  纯离线可证）。
- 复核方式：任意新窗口重跑本分析（两脚本均只读），重点看 open/add mo30 负占比
  是否仍 ≥80%。
