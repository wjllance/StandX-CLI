# 盘口 / 成交流遥测（纯观测）设计（2026-08-02）

状态：**设计待评审**。纯观测、不进决策路径、**replay 的 action 序列按构造等价**。
无 live 动作；部署须等采集窗口边界（[27 号手册](27-maker-baseline-pnl-collection-runbook.md)
"窗口内不重启、不换二进制"）。

## 为什么做（依据）

[尾部分解报告](evidence/maker-markout-tail-decomposition-2026-08-01.md)第三批的两个结论：

1. **触发器四分类**里，唯一"及时且可能足够强"的那类是**盘口 / 流特征**，而它
   **从未落日志**；结果类（等 markout）结构性迟到、成交前 drift 类信息价值 ≈0、
   自有状态类只剩 `post_suppressed` 一支。
2. 亏损的 **60~72% 集中在最差 10% 的成交**上，其中约 2/3 属于"单挂在原地被带信息的
   对手方扫走"——**现状不是没有对策，是连诊断都做不了**。

同一份数据还能解锁 microprice 候选（[28 号立项依据 2](28-maker-external-skew-design.md)
明确写着"盘口量从未落日志"是它一直被阻塞的原因）。**一次落地解锁两件事。**

## 关键发现：数据已经在线上，只是被丢掉了

- **盘口量与深档：零新订阅。** maker feed 已订阅 `depth_book`
  （[feed.rs:365](../crates/standx-cli/src/commands/maker/feed.rs:365)），其 payload 是
  `WsMessage::Depth(WsMarketUpdate<OrderBook>)`，而 `OrderBook` 带
  `bids: Vec<[String; 2]>` / `asks: Vec<[String; 2]>`（**价格 + 数量**、多档）。
  但 feed 只调 `update.data.best_bid()` / `best_ask()` 取最优**价格**写入 `FeedState`
  （WS 路径 [feed.rs:433](../crates/standx-cli/src/commands/maker/feed.rs:433)，
  REST 兜底路径 [feed.rs:529](../crates/standx-cli/src/commands/maker/feed.rs:529)），
  **数量与所有深档在解析后即被丢弃**。两条路径都要改，否则 REST 兜底期间字段会莫名为
  `null`。
- **公开成交流：需要新订阅，但 SDK 已支持。** 公共 WS 已解析 `public_trade` 频道 →
  `WsMessage::Trade(Trade)`，`Trade` 带 `side`（**taker 方向**）/ `price` / `qty` /
  `time` / `id`（[websocket.rs:502](../crates/standx-sdk/src/websocket.rs:502)、
  [models.rs:227](../crates/standx-sdk/src/models.rs:227)）。maker feed 目前只订
  `price` + `depth_book`，**未订 `public_trade`**。

**成本结论**：两半的风险不对等，因此**分两阶段，独立评审**。

## 范围与阶段

| 阶段 | 内容 | 新订阅 | 触及 watchdog 拓扑 | 风险 |
|---|---|---|---|---|
| **Phase 1** | 盘口量 + 前 N 档快照 | **无** | **无** | 低 |
| **Phase 2** | 公开成交流（taker 方向） | 有（`public_trade`） | **有**（第三条频道） | 中，须单独裁决 |

**Phase 1 先做、单独合并。** Phase 2 的风险集中在 watchdog（见下），不应与 Phase 1 混在
一次评审里。

## 设计原则（三条硬边界）

1. **观测数据不进 `standx-maker` 纯核心，不进 planner 输入。** 盘口量只在 CLI 层从
   `FeedState` 流向遥测，**不进入 `MarketSnapshot`、不进入任何决策函数签名**。
   这样 action 序列的等价性是**按构造成立**的，不依赖"我们没读它"这种约定。
   替代方案（放进 `MarketSnapshot`）被否：会污染 replay trace 契约，且给未来"顺手读一下"
   留了口子。
2. **只增不改输出契约。** 新字段一律新增；现有 `cycle_summary` / `fill` 字段不动。
   样本缺失写 `null`，**不写 0**（0 是"量为零"这个真实取值，与"没测到"必须可区分）。
3. **失败方向 = 静默降级。** 盘口量解析失败 / 缺档 → 该字段为 `null`，报价照常。
   **遥测绝不能成为新的停报价来源**（沿用 guard / external_skew 的 fail-open 纪律）。

## Phase 1：盘口快照

### 需要解决的真问题：成交是异步观测的

`fill` 事件来自 account stream，其 `event_time_ms` 与 cycle 边界不对齐。**"成交时刻的
盘口"≠"下一轮 cycle 的盘口"** —— 而尾部分析要的恰恰是被扫那一刻的档位厚薄。

因此需要在 CLI 层加一个**按时间索引的环形缓冲**（`FeedState` 目前只留最新值）：

- 保留最近 **N 秒**（建议 60s，覆盖 mo30 视界 + 余量）的盘口快照，每次 `Depth` 更新写入
  一条；容量按"最坏更新频率 × N"定上界，**固定容量、覆盖写，不随时间增长**。
- `fill` 事件按 `event_time_ms` 取**不晚于成交时刻的最后一条**快照；找不到（缓冲未覆盖、
  或成交早于首条）→ 字段为 `null`。
- 内存量级：每条快照存前 N 档（建议 5 档）= 20 个 f64 + 一个时间戳，60s 缓冲在秒级更新下
  是 KB 量级，可忽略。

### 字段

**`cycle_summary` 新增**（每轮采样，成本近零，给"成交前"的基线分布）：

- `book_bid_qty` / `book_ask_qty`：最优档数量；
- `book_bid_qty_n` / `book_ask_qty_n`：前 N 档数量之和（N 随实现固定并写入字段名或元数据）；
- `book_levels_bid` / `book_levels_ask`：实际收到的档位数（用于判断深度是否被场馆截断）。

**`fill` 事件新增**（按 `event_time_ms` 对齐，Stage 分析的直接输入）：

- `book_at_fill`：一个对象，含上述同名字段 + `book_ts`（该快照的来源时间）与
  `book_age_ms`（成交时刻 − 快照时间，**用于评估对齐质量，不可省**）。

`book_age_ms` 是这批数据可信度的关键：如果它经常是几百毫秒甚至更大，那"成交那一刻的
盘口"就只是个近似，后续分析必须按它分层。**先落这个字段，再谈结论。**

### 触及的文件（预估）

- `crates/standx-cli/src/commands/maker/feed.rs`：`FeedState` 保留 `OrderBook` 的量与前 N
  档；新增环形缓冲与按时间查询函数。
- `crates/standx-cli/src/commands/maker/output.rs`：两处新增字段序列化。
- `crates/standx-cli/src/commands/maker/`（cycle / pipeline 附近）：把快照与 fill 的
  `event_time_ms` 对齐后传给输出层。
- **不改**：`crates/standx-maker/**`（纯核心）、planner 签名、band / no-cross / refresh、
  guard 与 external_skew 的任何语义。

## Phase 2：公开成交流（单独裁决）

`public_trade` 能给 taker 方向，这是"谁在扫"的直接证据。但它引入一条第三频道，而 maker
feed 的看门狗**是围绕恰好两条频道建的**：`ChannelFreshness` 只有 `price` / `book` 两个
时间戳（[feed.rs:135](../crates/standx-cli/src/commands/maker/feed.rs:135)），
`idle_issue`（[feed.rs:152](../crates/standx-cli/src/commands/maker/feed.rs:152)）的三种
组合（`PriceIdle` / `BookIdle` / `PriceAndBookIdle`）到期即 **abort 连接并重建**。

**核心安全决策：`public_trade` 空闲绝不计入故障。**

- 理由：**安静的市场本来就没有成交**。把 trade-idle 当故障 = 在流动性最差的时候主动
  重连甚至停机 —— 与 run1 那次 `ws_price_idle` 截断同一类事故的自造版本。
- 落地：`public_trade` 只写遥测缓冲，**不参与 `ChannelFreshness`、不进 `idle_issue`、
  不影响 `reconnect_issue`**。订阅失败或该频道从不来数据时，Phase 1 的能力必须**完好
  不受影响**。
- 反向也要挡住：`public_trade` 的洪水（高频成交）不得挤占 price / depth 的处理 —— 单轮
  处理量设上限，超出即丢弃并计数（`public_trade_dropped`），**丢弃优于阻塞行情**。

字段（`fill` 事件）：成交前 M 秒窗口内的 taker 方向汇总（买量 / 卖量 / 笔数），M 建议
与 drift 视界一致（5 / 15 / 30s 三档，便于与既有 `drift_place` 对齐）。

## 验收（全离线，无 live 动作）

- **replay 等价**：以现有 `examples/maker-replay-trace.ndjson` 跑 replay，**action 序列
  逐条相同**。这是本设计第一原则的可执行检验。
- 单测：环形缓冲的时间查询（命中 / 早于首条 / 晚于末条 / 空缓冲）；缺档与解析失败 →
  `null` 而非 0；`book_age_ms` 计算；Phase 2 的丢弃计数与"trade 空闲不算故障"。
- CI 三项照跑（`fmt --check` / `clippy -D warnings` / `test`）。
- **无 canary、无 live gate 变更**：本改动不动决策路径，按纯遥测处理；但部署仍须等采集
  窗口边界（不在运行中的 run 里换二进制）。

## 明确不做

- 不把盘口量喂给任何决策（microprice / 尾部避让的**机制**都不在本设计内，本设计只产数据）。
- 不新增第二条 WS 连接（`public_trade` 与 price / depth 共用现有公共连接）。
- 不改 `depth_book` 的订阅参数去要更多档 —— 先看场馆默认给几档（`book_levels_*` 字段就是
  为此），**不够再另议**，避免一上来就改订阅面。
- 不做历史回填：新字段只对新数据生效，**旧 run 无法追溯**（这是"需等一个采集窗口"的
  由来）。

## 失效条件

- 场馆 `depth_book` 实际不带量或只给 1 档（`book_levels_*` 会立刻暴露）→ Phase 1 退化为
  "只有最优档数量"，microprice 候选仍被阻塞，须回 18 号重排；
- `public_trade` 不带 `side` 或与我方成交无法区分 → Phase 2 关闭；
- `book_age_ms` 分布过大（对齐质量不足以支撑"成交那一刻"的结论）→ 需改为按 cycle 采样的
  粗口径，并下调基于它的所有推论。
