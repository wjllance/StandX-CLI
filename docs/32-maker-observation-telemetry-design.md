# 阶段 7：纯观测遥测——盘口几何、clamp 命中、成交流

## 状态

`planned` / 纯观测。不改变任何报价、撤单、退出或停机决策；不引入新的停报来源。
对应 [18-maker-strategy-roadmap.md](18-maker-strategy-roadmap.md)「当前优先级 1」。

## 背景与问题

两轮 live 读数把亏损定位到逆向选择（`capture 2.8 - markout 3.2 - fee 1 ≈ -1.4bps/笔`），
但我们**缺少描述逆选物理成因的字段**。三个具体盲区：

1. **被吃风险的物理量是「距 best 的距离」，我们全部几何量锚在 mark 上。**
   对 best 只有一个硬不穿越检查（[`quote_crosses_touch`](../crates/standx-maker/src/lib.rs) ）。
   全仓 `grep distance|to_touch` 在 maker 路径零命中。

2. **`desired_quotes` 的 no-cross clamp 会把报价钉到距 best 一个 tick 处，且无任何记录。**
   [`crates/standx-maker/src/lib.rs`](../crates/standx-maker/src/lib.rs) 的 ladder 里
   `buy` 的可行上界是 `min(band_hi, best_ask - tick)`，`raw_price.clamp(price_lo, price_hi)`
   之后我们可能挂在全场最危险的价位；可行区间为空时则直接 `continue` 丢弃这一档。
   两种结果目前都不产生 reason code、不产生字段，离线无法区分
   「这一轮没挂」和「这一轮被钉在盘口挂了」。

3. **盘口与成交流数据被丢弃。** `depth_book` 推送带多档量，
   [`feed.rs`](../crates/standx-cli/src/commands/maker/feed.rs) 只 parse `best_bid/best_ask`；
   `public_trade` 频道在 SDK 里**已经**能解析（`websocket.rs` 已把它映射到
   `WsMessage::Trade`），但 maker feed 从未订阅。因此「身前深度多少」「深度是否
   在成交前撤走」「taker 方向」三个问题都无法回答。

第 3 条还有一个边界条件必须记住：**身前挂单集体撤走导致我们暴露于盘口时，
不会有任何成交推送**。所以成交流不能单独作为观测口径，必须同时有深度快照。

## 硬约束（实现必须逐条满足）

1. **零决策影响。** 新字段不得被 `preflight_cycle`、`plan_cycle`、`reconcile`、
   guard/skew/vol 任何分支读取。`public_trade` 与深档量都只进日志。
2. **replay 逐 action 等价。** 现有 ndjson 回放的 `Action` 序列必须字节级不变。
   新字段可以进入 `CyclePlan`（确定性、可离线重算），但不得改变任何既有字段。
3. **不新增停报/重连来源（fail-open）。** 尤其是：
   - `public_trade` **不得**进入 `ChannelFreshness`（否则安静的成交流会每 15s
     触发 idle watchdog 重建连接，凭空造出一个 outage 源）；
   - `coherent_ws_snapshot` / `FeedSnapshotVersion` 不得要求第三个频道推进；
   - 订阅失败、解析失败、字段缺失一律降级为「无观测」，继续正常报价。
4. **有界内存。** 成交 tape 与盘口环形缓冲必须定长，不得随运行时长增长。
5. **不与热路径抢锁。** 每周期 `feed.read().await` 是报价热路径。成交 tape 与
   深档写入**不要**放进 `FeedState`，用独立的锁/结构，避免高频 tape 写入
   拖慢 mark/book 读取。
6. **JSON 向后兼容。** 沿用现有 `with_*_fields` 追加式惯例
   （见 `output.rs` 的 `with_guard_fields` / `with_size_skew_fields`），
   字段一律 top-level 追加、缺失为 `null`。

## Part A：feed 侧盘口深档 + 成交流（对应「第 3 项」）

### A1. 深档保留

- 深档保留在 A2 的独立结构里（**不是** `FeedState`，见硬约束 5）：
  `bid_levels: Vec<(f64, f64)>` / `ask_levels: Vec<(f64, f64)>`，每侧最多保留 **5 档**（与 REST `get_depth(symbol, Some(5))` 对齐），按价格排序，
  非有限/非正值丢弃。`OrderBook.bids/asks` 已是完整向量，无需改 SDK。
- 现有 `best_bid/best_ask` 字段与推导方式**保持不变**，不要改成从新 vec 取首档，
  以免动到决策输入。
- 深档为空或解析失败时保留旧值不是必需的——置空即可（观测缺失优于错误观测）。

### A2. `public_trade` 订阅与 tape

- `spawn_market_feed` 增加 `ws.subscribe("public_trade", Some(&symbol))`。
  SDK 侧无需改动（`websocket.rs` 已 dispatch `public_trade` → `WsMessage::Trade`）。
  确认：maker 的自有成交走独立的 `account_stream.rs`（`AccountEvent::Trade`），
  与 `WsMessage::Trade` **无冲突**。
- 新增独立结构（不放进 `FeedState`，见硬约束 5）：
  定长环形 tape，容量 **256 条**，每条记录
  `{ local_recv_ms（单调）, id, price, qty, side（Option）, is_taker（原样） }`。
- **字段可信度警告：** `models::Trade` 的 `is_buyer_taker` 带 `#[serde(default)]`
  且 `side: Option<String>`。缺字段会静默变成 `false`，那是**伪造的方向信息**。
  因此：
  - 首个交付版本额外记录一个**原始 JSON 采样**（前 N=50 条 `public_trade`
    原文写入 stderr 或独立日志行，仅一次性诊断用），先证实 venue 实际推送的
    形状，再决定离线是否可信任 `side`/`is_taker`；
  - `side` 缺失时写 `null`，**不要**用 `is_taker` 反推方向。

### A3. cycle_summary 追加字段

新增 `with_book_fields(...)`，在 `cycle_summary` 上追加：

```
"book": {
  "bid_levels": [[price, qty], ...],   // 最多 5，观测缺失为 null
  "ask_levels": [[price, qty], ...],
  "bid_qty_top": qty, "ask_qty_top": qty,
  "spread_bps": (best_ask-best_bid)/mark*1e4,
  "mark_mid_divergence_bps": ...,      // 复用 lib.rs 已有 mark_mid_divergence_bps
  "age_ms": ...
},
"tape": {
  "count_5s": n, "buy_qty_5s": q, "sell_qty_5s": q, "unknown_qty_5s": q,
  "last_trade_age_ms": ...
}
```

`tape` 的窗口统计在 CLI 侧按 tape 的单调时间戳现算，不落每笔明细到
`cycle_summary`（明细留在 tape，必要时另行落盘）。

## Part B：报价几何与 clamp 命中（对应「第 2 项」）

### B1. 在 `desired_quotes` 里产出诊断，放进 `CyclePlan`

**裁决（2026-08-19，owner）**：诊断放进 `CyclePlan`（`standx-maker` 纯逻辑层），
不在 CLI 侧另算。理由：诊断必须来自**真正执行 clamp 的那段代码**，否则一定会和实际
行为漂移；放进 `CyclePlan` 还能让现有 replay 直接在历史 trace 上重算这些字段。
代价是动了 pure crate 的公开结构，因此 Part B 必须附带专门的 replay 逐 action 等价测试。

新增（`standx-maker`，pure）：

```rust
pub enum QuoteGeometryOutcome {
    Placed,                  // 进入 desired
    ClampedToBand,           // 被 band 边界拉回
    ClampedToTouch,          // 被 no-cross 上界拉回（危险来源）
    DroppedInfeasible,       // price_lo > price_hi，整档丢弃
    DroppedBelowMinQty,
    DroppedDuplicate,        // clamp 后与上一档同价被折叠
    SuppressedPosition,
    SuppressedGuard,
}

pub struct QuoteGeometry {
    pub side: OrderSide,
    pub level: u32,
    pub raw_price: f64,          // clamp 前
    pub final_price: Option<f64>,// clamp/rounding 后（丢弃时 None）
    pub outcome: QuoteGeometryOutcome,
    pub distance_to_touch_bps: Option<f64>, // 同侧对手盘口距离，bps of mark
    pub band_edge_bps: f64,
}
```

- `CyclePlan` 增加 `pub quote_geometry: Vec<QuoteGeometry>`（追加字段）。
- **同时覆盖 resting 报价**：`reconcile` 每周期对每个在场报价也产出一条
  （`outcome: Placed`，`distance_to_touch_bps` 按当轮盘口重算），
  否则我们只看得到「新挂时的距离」而看不到「挂着时距离被盘口逼近」。
  实现上可以在 `plan_cycle` 末尾统一补，不必侵入 `reconcile`。
- `distance_to_touch_bps` 定义（**写进 doc comment，避免离线口径混用**）：
  买单为 `(best_ask - price) / mark * 1e4`，卖单为 `(price - best_bid) / mark * 1e4`；
  盘口缺该侧时为 `None`。

### B2. cycle_summary 追加字段

新增 `with_geometry_fields(...)`：

```
"geometry": {
  "min_distance_to_touch_bps": ...,   // 当轮所有 desired+resting 的最小值
  "clamped_to_touch": n,              // 本轮命中次数
  "clamped_to_band": n,
  "dropped_infeasible": n,
  "quotes": [ {side, level, outcome, raw_bps, final_bps, dist_touch_bps, band_edge_bps}, ... ]
}
```

`raw_bps` / `final_bps` 由 `standx-maker` 的 `side_distance_to_mark_bps`（已 pub）渲染，
CLI 侧不得重写这个符号约定——那正是 B1 裁决要避免的漂移。原本收在结构体里的
`distance_to_mark_bps` 已删除：它只是 `final_bps`（挂上时）或 `raw_bps`（被丢弃时）的
折叠值，可由发出的这一对完全推导。

`quotes` 明细在 `levels=1` 下最多 2 条，体积可接受；若将来 levels 变大，
只保留聚合项，明细降级为最危险的一条。

## Part C：成交时刻的盘口观测（对应 roadmap「成交前后 best bid/ask 数量」）

- 沿用 `excess_bps_at_fill` 的既有手法：runtime state 里持有最近一次
  `BookObservation`，在 emit fill 的**全部**站点带上。
  已知站点：`runtime/cycle_flow.rs`（4 处）、`runtime/recovery_flow.rs`（3 处）、
  `recovery.rs` 的 `FillEmission`。
- **用一个结构体而不是多个标量字段**（避免 8 处各加 4 个参数）：

```rust
pub(super) struct BookAtFill {
    best_bid: Option<f64>, best_ask: Option<f64>,
    bid_qty_top: Option<f64>, ask_qty_top: Option<f64>,
    bid_levels_qty_5: Option<f64>, ask_levels_qty_5: Option<f64>,
    observation_age_ms: Option<u64>,
    tape_count_5s: Option<u32>,
    tape_last_side: Option<String>,
}
```

- `observation_age_ms` 必须落盘：成交事件走 account stream，盘口走 public feed，
  两者异步；不带 age 的盘口快照在离线分析里不可用。
- **本轮不做**成交后（post-fill）盘口窗口。`performance.markout_*` 已有从 mark
  起算的 markout 窗口，post-fill 盘口需要延迟 emit，复杂度不值当，留待下一轮。

## 交付顺序

1. **Part A**（feed 深档 + tape + `with_book_fields`）——Part C 依赖它的观测源。
2. **Part B**（quote geometry，纯 `standx-maker` + 一个 output wrapper）——与 A 独立。
3. **Part C**（成交时刻观测穿线）。

三部分可分别提 PR；每个 PR 独立满足下面的验收要求。

## 测试与验收要求

每个 PR 必须包含：

- **replay 等价证明**：用现有 ndjson trace 跑 `run_replay`，断言 `Action` 序列
  与改动前逐条相同（Part B 需要专门的等价测试，因为它动了 `CyclePlan`）。
- **fail-open 单测**：
  - `public_trade` 完全无推送时，feed 不触发 idle 重建、不影响 `coherent_ws_snapshot`；
  - 深档缺失/畸形时 `best_bid/best_ask` 与既有行为一致；
  - `Trade` 缺 `side` 字段时记为 `null`，不写入 `false` 方向。
- **clamp 命中单测**（Part B）：构造 `best_ask - tick < band_hi` 的输入，
  断言产出 `ClampedToTouch` 且 `distance_to_touch_bps` 等于 1 tick 对应 bps；
  构造 `price_lo > price_hi` 断言 `DroppedInfeasible`。
- **JSON 追加性单测**：沿用 `cycle_summary_*_fields_are_additive_and_top_level`
  的现有测试形状。
- **有界性单测**：tape 写入 300 条后长度仍为 256。

提交前按 roadmap 要求跑全套：

```bash
HOME=/tmp/standx-test-home CARGO_HOME=~/.cargo cargo test --workspace --offline
cargo clippy --workspace --all-targets --offline -- -D warnings
cargo fmt --all -- --check
```

## 明确不做（out of scope）

- 不加 min-distance-to-touch **门控**（那是策略改动，需要单独立项与预注册判据）；
- 不改 `band_bps` / `spread_bps` / `refresh_bps` 任何默认值；
- 不把 tape 或深档接入任何决策、guard 或 skew；
- 不做 post-fill 盘口窗口；
- 不改 symbol、size、`max_position`。

## 后续（本文不授权）

字段积累一个完整窗口后，离线回归 `distance_to_touch_bps` / `clamped_to_touch`
对成交概率与 markout 的解释力。若解释力显著高于现有 `mid_bias_bps`
（filled rho ≈ -0.53），再按 [28-experiment-protocol.md](28-experiment-protocol.md)
立项 min-distance 门控候选。
