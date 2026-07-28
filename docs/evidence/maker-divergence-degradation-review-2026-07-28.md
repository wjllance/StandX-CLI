# Divergence 降级方案 B / C / D 复核（2026-07-28）：C 前提消失、B 降级为观测项

复核对象：2026-07-16 live 事故（XAG mark/mid 偏离 6.13bps 悬停 >60s → 停机）之后拟定的
四个方案。A 早已落地，B / C / D 一直挂在
[18 号阶段 5 二级](../18-maker-strategy-roadmap.md)的"背离恢复迟滞、熔断豁免等剩余硬化项
按需纳入"名下，也是 [25 号快照](../25-maker-short-term-roadmap-2026-07-27.md) 里 5-b 的
最后一个代码项。

本次按 release owner 裁决准备实现 C（熔断豁免），**实现前先核前提——前提已经不成立**。
以下是逐条核对结果。

## 结论速览

| 方案 | 原始命题 | 2026-07-28 核对结果 | 处置 |
|---|---|---|---|
| A | 故障分类 + MarketState 无限期 standby | 已落地（`market_data.rs` 纯状态机） | 关闭 |
| **C** | 每次降级都计入共享恢复熔断（3 次/小时），边界震荡第 4 次必硬停 | **前提消失**：共享恢复熔断已从代码中移除，不存在任何跨恢复累计的计数器 | **关闭（无需代码）** |
| **B** | 恢复用同一阈值无迟滞，悬停在阈值上方永远凑不齐连贯快照 | 迟滞仍未实现，但后果已从"60s 后硬停"降级为"standby 期间不报价"（uptime 损失） | **降级为观测项**，等 PnL 采集给出 standby 频率证据再决定是否立项 |
| D | tick 感知阈值 + mark 在盘口内豁免 | 未实现；与 B 同源（都是阈值/恢复条件的形状问题） | 与 B 合并处理 |

## C：共享恢复熔断已不存在

原始命题（2026-07-16 记录）：`RecoveryCircuitBreaker` 按 3 次/小时限流，每次进入行情
降级都 `admit()` 计入，阈值边界震荡的第 4 次会熔断硬停。

核对：

- `RecoveryCircuitBreaker` / `recovery_breaker` / `admit()` 在 `crates/` 下**已无任何
  代码引用**。
- 仅剩两个被显式标注为 deprecated 的配置字段（`crates/standx-cli/src/commands/maker/config.rs`）：
  `recovery_incidents_per_window` / `recovery_window_secs`，注释写明
  "transport recovery no longer uses an incident-count circuit"——存在只为让既有生产
  配置文件继续解析，值被忽略。
- 现在约束恢复的是**每轮独立**的预算：`--order-response-reconnect-attempts` /
  `--account-stream-reconnect-attempts`（默认 3，单轮内耗尽则冻结并按退避进入下一轮），
  以及核心状态机的 `consecutive_cycle_errors`（连续 3 次**周期错误**停机）。行情降级走
  `MakerEvent::MarketDataDegraded` → 冻结/清理/standby，**不是** `CycleFailed`，因此不
  计入连续周期错误。
- 行情降级现在**两类都没有硬期限**：`recovery_flow.rs` 的 standby 分支对
  `MarketDataFaultClass::MarketState` 与 `Transport` 都只做 60s 心跳通知
  （`MARKET_DATA_STANDBY_HEARTBEAT`）并保持 placements 冻结，没有到期停机路径
  （早期记录里"Transport 才有 60s 硬期限"的说法对当前 main 已失效）。

**因此"豁免"没有可豁免的对象**。为一个不存在的熔断新增
`RecoveryTrigger::MarketCondition` 只会增加一个永远为真的分支和一份要维护的测试。
C 关闭，不写代码。

## B：致命部分已消失，残余是 uptime 损失

原始命题的两层里，第二层（"悬停在阈值上方 → 3 连贯快照永远凑不齐"）**依然成立**：

- 降级侧有迟滞（`MARKET_DATA_BAD_OBSERVATIONS_TO_DEGRADE = 3` + `MARKET_DATA_BAD_GRACE_MS
  = 15_000`），但**恢复侧没有比例迟滞**：恢复仍要求偏离回到同一个 `max_divergence_bps`
  之内，连续 `MARKET_DATA_COHERENT_SNAPSHOTS_TO_RECOVER = 3` 个可报价快照。
- 偏离恰好悬停在阈值上方时，连贯快照永远攒不满 → 一直留在 standby。

变的是**后果**：A 落地后 standby 不再到期停机，代价从"停机持仓"变成"standby 期间完全
不报价"（uptime 损失、SIP-5A 不计分），持仓仍按不自动平仓原则交由人工/后续退出处置。
这把 B 从安全项降级为**收益项**。

按仓库的既有纪律（"没有证据不写代码"，阶段 4 的教训），B 不在本轮立项，理由是缺证据：

- 当前冻结基线 `max_divergence_bps = 15.0`（不是事故时 XAG 的 6.0）。
- 阶段 3-guard 的 6 臂 ~24h live（[判定报告](maker-guard-spinoff-ab-judgment-2026-07-27.md)）
  时间加权双边 uptime 为 97.0–99.6%，没有留下长时间 standby 的空间——**在 HYPE / 15bps
  下这个洞至少不是常发**。
- 但这是**间接推断**：本机没有 run 证据目录（`/opt/standx/var/standx` 在部署主机上），
  没有直接统计过 `divergence_standby` 事件。

**处置**：把 standby 观测正式列进下一次采集的每日记录
（[27 号手册](../27-maker-baseline-pnl-collection-runbook.md)已加入
`risk_notification(kind=market_data)` 的 `divergence_standby` / `transport_standby`
事件与时长）。立项触发条件预注册如下，命中任一即按 v0 流程立项 B（+ D）：

- 单次 standby 持续 **> 10 分钟**；或
- 一个采集窗口内 standby 累计占比 **> 1%** 的运行时间；或
- 任何一次 standby 被证明发生在"偏离悬停在阈值上方"而非真实行情异常。

届时的形状仍按原方案：恢复阈值取 `recover_ratio ≈ 0.75 × max_divergence_bps` +
恢复阈值下驻留 ≥5s（只数 3 个快照挡不住 flapping），并与 D（tick 感知下限
`max(bps, N × tick_bps)` + mark 在盘口内豁免）一并评估——两者都是"阈值形状"问题，
分开做会重复走一遍同样的验收。

## 对 5-b 与路线图的影响

5-b 名下的代码项到此为止：四项主体已合并 main（PR #332），B/C/D 按本记录关闭或降级为
观测项。**5-b 不再有待写的代码**——扩大规模的代码级前置已经齐了，剩下的是证据问题
（基线 PnL 读数）与授权问题（开 `stop_*`、写授权文本）。

## 残余风险（记录，不在本轮处理）

MarketState standby 期间持裸库存且不报价：既不能通过报价对冲，也不会自动退出
（D1 定稿：halt/降级期间不自动退出）。这与 halt 期间的取舍是同一件事，已在
[26 号文档 D1](../26-maker-stage5b-design.md) 显式接受，重启该决策需要一次实盘事件的
逐笔证据。
