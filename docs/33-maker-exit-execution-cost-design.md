# 阶段 8：库存退出执行成本——ALO 优先 + IOC 兜底

## 状态

`enabled_pending_judgment`（2026-08-21）。owner 当日第三次裁决直接启用（跳过独立
A/B）：BTC 首窗口 run3 起 `inventory_exit_pct=70 / inventory_exit_qty=0.0005 /
alo_enabled=true`，见 [34](34-maker-btc-migration-2026-08-21.md) §7。IOC 后端接受度已于
当日实盘探针验证。判据（执行成本，见「判据」一节）随窗口每日记录观测，**正式判定
悬置**——裁决时点在 BTC 基线读数出来之后，不在本轮窗口内给 accepted/rejected。
这是**影响成交结果**的变更（改的是成交方式，不是报价位置），按
[28-experiment-protocol](28-experiment-protocol.md) 的常态路径是立项 + 预注册判据 + 独立
A/B；本次启用是 owner 的显式例外裁决，读数解释时必须带上"基线含主动砍仓"这一前提。

## 背景：为什么这是当前性价比最高的一项

2026-08-20 用 `standx market symbols` 核实的场馆事实：**maker `0.0001` = 1bps，
taker `0.0004` = 4bps**，11 个 symbol 全部一致，无 symbol 差异、无已知返佣。

库存退出目前发 `OrderType::Market` + `reduce_only`
（[cycle.rs](../crates/standx-cli/src/commands/maker/cycle.rs) 的 `inventory_exit` 分支），
即**每次退出付满 4bps taker**。改成 ALO 挂单离场付 1bps，每次省约 3bps。

在一个逐笔 `capture 2.8 - markout 3.2 - fee 1 ≈ -1.4bps` 的经济结构里，这是**纯成本项**：
不改变退出的触发条件、不改变风险方向、不需要任何新的信息优势。它不依赖
[32](32-maker-observation-telemetry-design.md) 的观测数据，可以独立推进。

## 目标与非目标

**目标**：把 `InventoryTrim` 的常态执行从 taker 变成 maker，只在亏损越阈值时才付 taker。

**非目标（本文明确不做）**：

- 不改退出的**触发**条件（仍是 `inventory_exit_pct` / `inventory_exit_qty`）；
- 不引入 per-fill 立即 TP（另一件事，需要独立立项）；
- 不让退出与报价并存（见「已知代价 4」）；
- 不改 `WindDown` 语义（见「已知代价 5」）。

## 执行模型（三段状态机）

```
Idle ──(库存越阈值)──> Alo ──(亏损越阈值 或 尝试超时)──> Ioc ──(成交/残量清零)──> Idle
                        │                                  ^
                        └────────(成交/残量清零)────────────┘
```

1. **Alo 段**：`OrderType::Limit` + `TimeInForce::Alo` + `reduce_only`，价格挂在减仓方向
   的对侧盘口（卖出减多头 → `best_ask`，买入减空头 → `best_bid`），可配 tick 偏移。
   盘口偏离超过 `alo_refresh_bps` 时 cancel/replace。
2. **升级条件**（任一满足）：亏损达到 `ioc_loss_bps`（默认 5，与作者口径一致），或
   在 Alo 段停留超过 `alo_max_cycles`。
3. **Ioc 段**：`OrderType::Limit` + `TimeInForce::Ioc`，穿价 `ioc_cross_ticks` 保证成交。
   IOC 不留单，天然可重试；残量下一周期继续。**不保留 Market 兜底**——IOC 穿价已足够，
   且 Market 没有价格保护，是当前这条路径最贵且最不可控的部分。

`TimeInForce` 枚举已有 `Ioc`，[API_DOCUMENTATION.md:318](../API_DOCUMENTATION.md) 明确列出
`GTC` / `ALO` / `IOC`（`FOK` 不在列）。**但我们从未实测 IOC**——第一次 live 必须观察
拒单，不能假定它被接受。

### 分层归属（按 [18](18-maker-strategy-roadmap.md) 的长期不变量）

- **`standx-maker`（纯逻辑）**：三段状态机的**状态与转移判定**。输入是归一化的 typed
  值（当前 phase、position、mark、盘口、break-even、已停留周期数、配置），输出是
  "本周期该做什么"的 typed 决策（挂 ALO / 重挂 / 转 Ioc / 什么都不做）。无 I/O、无时钟。
- **`standx-cli`（执行）**：把该决策翻译成 create/cancel 命令、`cl_ord_id` 生命周期、
  projection 登记、latency 登记、日志与遥测。

状态机放纯逻辑层的理由与 [32](32-maker-observation-telemetry-design.md) 的 B1 裁决同源：
转移判定必须可被 replay 在历史 trace 上重算，否则"为什么那一次升级到了 Ioc"事后无法复原。

### 亏损基准的口径（必须写进 doc comment）

`MakerStats` 是 **cash 累加器**，没有 per-position 入场价。会话级 break-even 为
`-cash / position`，`loss_bps` 由它与当前 mark 按持仓方向求得。

注意 `MakerStats::with_inventory_baseline` 下，**被采纳仓位的 break-even 是采纳时的
mark，不是历史成本**。这与 session-PnL 语义一致，但离线读数时不能当成真实入场价。
两种口径不得混用（同 [18](18-maker-strategy-roadmap.md) 已固化口径第 5 条的性质）。

## 必须解决的结构性问题

这些不是实现细节，是「退出从不留单」这个旧前提被打破后暴露的真实冲突。**每一条都要有
测试**。

1. **`account_clear` 会死锁。** 现在退出提交的前置条件是 maker book 为空
   （`projection.resting_quotes().is_empty() && pending_places().is_empty() &&
   pending_cancels().is_empty()`）。ALO 退出单自己一挂上，这个条件就永远不再为真，
   任何重挂/升级逻辑都会卡死。必须把**退出单自身**从这个判定里排除，或给退出单独立
   的 gating 谓词。
2. **退出单需要稳定身份。** 现在退出单用 `level: u32::MAX` 哨兵避免占用报价 slot，
   `cl_ord_id` 由 `exit_client_order_id(prefix, cycle)` 按**当前 cycle** 生成。留单之后
   每周期换 id 会让对账追不上。改为：id 在**进入 Alo 段的那个 cycle** 固定，只有
   cancel/replace 时才推进。
3. **fail-closed 对账会误伤。** 所有权按 prefix 判定（`is_current_run_order`），所以
   退出单会被认成 current-run maker order；但 slot 解析只认 `q{side}{level}` 形状，
   `x{cycle}` 解析不出 slot，可能触发 `unknown_current_run_order` 硬停。
   **当前场景不可能出现（退出从不留单），改成留单就会出现。** 必须显式处理，且必须
   有一条测试专门钉住"在场退出单不触发硬停"。
4. **cleanup 必须能撤在场退出单。** 机制上已被 prefix 所有权覆盖（终态 success 优先、
   按 `order_id` REST 兜底、无法确认时 fail-closed），但从未有留单退出走过这条路，
   需要测试证明。

## 已知代价（接受，不是 bug）

4. **退出期间报价全停。** `plan_cycle` 在 `inventory_exit.is_some()` 时清空 desired
   quotes。ALO 退出可能横跨多个周期，等于把 uptime 预算烧在退出上；现在的 Market
   退出也停一轮，但只停一轮。第一版接受此代价并记录——让退出与报价并存是更大的改动，
   需要单独立项。`alo_max_cycles` 就是这个代价的上限旋钮。
5. **`WindDown` 不走 Alo 段。** A/B 臂到点必须确定性平掉，而 ALO 可能永远不成交。
   本次改动**只作用于 `InventoryTrim`**；`WindDown` 保持现有的立即穿价路径。

## 配置（默认关闭）

新 `[inventory_exit]` section：

| 键 | 默认 | 含义 |
|---|---|---|
| `alo_enabled` | `false` | 关闭时走现有 Market 路径，逐 action 等价 |
| `alo_price_offset_ticks` | `0` | 相对对侧盘口的 tick 偏移 |
| `alo_refresh_bps` | `2.0` | 盘口偏离超过此值才重挂（抗抖动） |
| `alo_max_cycles` | `20` | Alo 段停留上限，超过升级 Ioc |
| `ioc_loss_bps` | `5.0` | 亏损越此值立即升级 Ioc |
| `ioc_cross_ticks` | `2` | Ioc 穿价幅度 |

非法值必须在启动时校验并拒绝，不能在挂单在场时 panic（`df069c5` 的教训）。

## 测试要求

- **关闭时逐 action 等价**（replay，沿用 [32](32-maker-observation-telemetry-design.md) 的
  字节 pin 手法）；
- Alo 段重挂：盘口移动越 `alo_refresh_bps` → cancel+place，且 `cl_ord_id` 语义正确；
- 升级路径：亏损越 `ioc_loss_bps` → 转 Ioc；`alo_max_cycles` 超时 → 转 Ioc；
- `WindDown` 不进 Alo 段；
- 在场退出单**不**导致 `account_clear` 死锁；
- 在场退出单**不**触发 `unknown_current_run_order`；
- cleanup 能撤掉在场退出单，未确认时 fail-closed；
- 配置校验拒绝非法值。

## 判据（预注册，开跑前冻结）

判据是**执行成本**，不是 PnL——PnL 在这个规模上噪声远大于 3bps/次的效应：

- 主判据：`InventoryTrim` 退出中 maker 成交（1bps）的**数量占比** ≥ 60%；
- 次判据：单次退出的平均实现费率从 4bps 降到 ≤ 2.5bps；
- 红线：退出**未完成率**（越过 `alo_max_cycles` 仍未平的比例）不得上升到使
  `|position|` p95 超过现有读数；
- 红线：不得出现在场退出单导致的硬停或 cleanup fail-closed 事件。

## 明确不在本文范围

per-fill 立即 TP、退出与报价并存、min-distance-to-touch 门控、任何报价几何参数改动、
symbol / size / `max_position` 改动。
