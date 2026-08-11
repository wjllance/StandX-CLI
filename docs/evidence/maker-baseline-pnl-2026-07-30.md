# 冻结基线 PnL 绝对读数：baseline-pnl-20260728T081712Z（截断，35.9h）

采集依据：[27 号手册](../27-maker-baseline-pnl-collection-runbook.md)（候选 2，
授权文本 2026-07-28 已填）。**本报告只报读数与观察，不给晋级/否决结论。**

## 运行元数据

- run_id：`baseline-pnl-20260728T081712Z`；symbol HYPE-USD；单臂长跑，不换臂不调参。
- 配置：`examples/maker-guard-hype-candidate.toml`（sha256 `6314a374…`，原样）。
- 代码：`819f0f0`（含 5-b `469550a` + funding 接入）。
- 窗口：2026-07-28T08:17:12Z → 2026-07-29T19:09:57Z，**35.9h，fail-safe 截断**
  （授权窗口 72h，未跑满；按手册"不为跑满窗口重启"）。
- 日志：`var/standx/baseline-pnl-20260728T081712Z.ndjson`（182k 行已全量上传
  OpenObserve）。

## 截断原因（事故链）

1. 19:09 前后 market feed 触发 `ws_price_idle` 看门狗，行情 freeze，进入恢复流程。
2. freeze cleanup **连续 6 次**未能撤掉两张 maker 单（`11924302689` /
   `11924302685`）→ 命中"残余 maker 订单 fail-closed"不变量。
3. fail-safe 停机（exit 75），critical 三连：`maker_cleanup`（残余单）→
   `residual_position`（handoff 0.30 多头）→ `fail_safe`。停机账面 48521 cycles /
   527 fills / uptime 85%（lifecycle 行口径）/ PnL -1.57。

**事后场馆核查（2026-07-30T00:40Z）**：挂单簿为空；仓位 0.30 HYPE 多头
（entry 55.238，mark 53.943，浮亏 -0.39）与停机时 ledger 一致，残余单未被成交。
**残余单去向已查明（2026-07-30，`/api/query_order` 单查）**：两张单均
`status=canceled`、`fill_qty=0`、`updated_at=2026-07-29T19:09:41.928Z`——
**cleanup 的撤单请求在第一时间就成功了**，但验证查询（open-orders 口径）在此后
15 秒、6 次重试里持续返回陈旧结果，被误判为"残余单撤不掉"并触发 fail-safe。
owner 确认非手动撤单。**结论：这是一次场馆读-写一致性滞后引发的误报性停机；
0.30 多头是 freeze 时点的真实在途库存，交接语义正确，尾部实际干净。**
改进项（另行列账）：cleanup 残余判定应对读-写滞后容错（按 order_id 单查
`/api/query_order` 的 status——SDK 已有 `get_order` 正是为此设计——或引入
短暂宽限期后再判残余），避免"撤单已成功但被陈旧读误判"把 run 打死。
~~**残余 0.30 多头由 owner 手动处置中**（2026-07-30
会话内声明），处置后回填 FLAT 复核。~~ **FLAT 已复核（2026-07-30T01:36:43Z）**：
owner 手动平仓后 `account positions` / `account orders` 双查均为空，账户回到
FLAT，本次 run 尾部全部结清。

## 读数（末态 cycle_summary，2026-07-29T19:09:39Z）

### 净 PnL 与归因（完整口径，`net_pnl_complete=true`）

| 项 | 数值（DUSD） |
|---|---|
| gross spread capture | +1.5035 |
| inventory MTM（重估残差） | -3.0716 |
| 手续费 | -0.2896 |
| funding（35h 全量，权威源） | -0.0014 |
| rebate | 0 |
| **净 PnL** | **-1.8591**（会话 mark-to-market 口径 -1.5681） |

成本完整性：`execution_costs_unavailable=0`、`funding_available=true`（启动后首次
整点结算即翻 true）、`funding_unattributed=0`——**成本项全部入账，读数是完整净额**。
手续费+funding 合计 -0.29，即使全免也不改变符号。

### 质量指标

- passive capture **+5.19 bps**；markout 1s +0.13 / 5s **-2.87** / 30s **-5.14 bps**。
- 时间加权双边 uptime **96.6%**；成交 **527 笔**；`inventory_avg_abs` 0.0995
  （max_position=1.0）；主动退出 0 次；`exit_suppressed` 无触发。
- guard 激活 **1.58%** cycles；halt **0.85%**。
- **`market_data_standby` 0 次**（35.9h 全程）——Divergence B（恢复迟滞）
  立项触发条件（单次 >10min 或累计 >1% 运行时间）**未命中**，且差距是
  "零 vs 任何正数"。这是该观测项目前最强的证据。
- critical 3 条，全部属于截断事故本身（事故前 35.8h 零 critical）。

### 轨迹

净 PnL 全程单调恶化（19h 时点 -1.11 → 35.9h -1.86），capture 稳定为正、
markout 稳定为负，未见 regime 性翻转。亏因分解与机制分析见
[29 号设计文档立项依据](../29-maker-external-skew-design.md)（19h 时点：
capture +5.0bps vs markout@30s -5.8bps，逆向选择系统性存在）。

## 观察（非结论）

- 35.9h 读数与 19h 读数方向一致、量级按时间线性放大：基线在自身规模上
  **持续小幅失血**，主因是成交时点逆向选择，不是成本项。
- 截断事故本身暴露的运维缺口（与策略读数无关，另行列账）：
  - ~~freeze cleanup 撤单连续失败 6 次的原因待查~~ **已查明**：撤单第一时间成功，
    是验证路径的读-写滞后误报（见上节）；改进已单独立项：
    [29-maker-cleanup-residual-verification.md](../archive/2026-07-maker/29-maker-cleanup-residual-verification.md)
    （WS success 主判据 + 按单查询兜底 + 列表宽限，fail-closed 语义不变）；
  - ~~无人值守告警链路有效性未证实~~ **已证实有效**：deadman/critical 两条
    OpenObserve push 告警于停机后送达飞书（owner 2026-07-30 确认收到）；
  - 本机小时报告脚本只以"发送成功"为退出条件，进程 DEAD 后仍报"正常"——
    已挂账修复（DEAD/critical/日志停滞 → 非零退出 + ALERT 字样）。

## 这份读数对扩规模决策的含义（手册既定口径）

读数为负且非噪声边缘（35.9h、527 笔、方向单调）：按 27 号手册"读数明确为负 →
扩规模会按比例放大损耗；先回 18 号文档找下一个 alpha 候选，不扩"。
候选队列现状：external_skew（[29 号设计](../29-maker-external-skew-design.md)，
草案待裁决，离线证据已附）→ microprice（阻塞在 depth 观测字段）。
