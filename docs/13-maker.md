# 13 - 做市机器人（Maker Bot）

本文档介绍 `standx maker` —— 面向 [SIP-5A 社区做市收益](https://docs.standx.com/sip/sip-5a-community-maker-yield)的双边报价机器人。

SIP-5A 按**在线时长（uptime）**、**报价贴近 mark 价（须在合格带内）**和**深度**奖励做市商，并惩罚闪撤（flicker-cancel）与短命报价。因此本机器人的核心是 **anti-flicker 循环**：只要报价还在合格带内就一直挂着，仅当 mark 价相对下单时漂移超过阈值才重新报价。

---

## 前置条件

- **Paper（模拟）模式**：无需认证，只读公共行情，不下任何真实单。默认即此模式。
- **Live（实盘）模式**：需要 JWT + Ed25519 私钥（参考 [02-authentication.md](02-authentication.md)），且当前锁定在环境变量 `STANDX_ENABLE_LIVE_MAKER=1` 之后（见 [13.5](#135-live-实盘模式)）。

---

## 13.1 快速开始

```bash
# Paper 模式（默认）：跑完整循环、打印将要执行的动作，但不下真实单。
# 成交会被模拟（touch 穿过报价即视为成交），所以仓位与库存 skew 可观测。
standx maker run BTC-USD --size 0.001 --interval 3
```

仓库内可用下面的快捷命令启动默认的 paper 策略（使用
`examples/maker.toml`，不会附加 `--live`）：

```bash
make maker
make maker SYMBOL=ETH-USD MAKER_ARGS="--interval 3"
```

**预期输出（表格模式）：**
```
┌──────────────────────────────────────────────────────────┐
│ standx maker — PAPER mode on BTC-USD
│ spread 5bps | band 20bps | refresh 3bps | 1 level(s)
│ size 0.001 | max-position 0.05 | interval 3s
│ ticks: price 2dp, qty 4dp | min qty 0.0001
│ paper mode: no real orders; fills are simulated when the
│ touch crosses a quote, so position & skew move. --live for real.
│ feed: websocket (REST fallback) | divergence guard 25bps
│ Ctrl+C to stop (cancels all resting orders on exit)
└──────────────────────────────────────────────────────────┘
[12:00:00] #0 mark=62000.00 bid=61999.00 ask=62001.00 pos=0.0000 pnl=0.00 | hold=0 place=2 cancel=0
    PLACE  buy  L0 @ 61969.00 x 0.0010
    PLACE  sell L0 @ 62031.00 x 0.0010
[12:00:03] #1 mark=62001.00 bid=62000.00 ask=62002.00 pos=0.0000 pnl=0.00 | hold=2 place=0 cancel=0
    HOLD   buy  L0 @ 61969.00 (age 1 cycles, drift 0.2bps)
    HOLD   sell L0 @ 62031.00 (age 1 cycles, drift 0.2bps)
```

按 `Ctrl+C` 退出，机器人会撤掉所有挂单并打印本次会话的统计。

---

## 13.2 策略参数

```bash
standx maker run <SYMBOL> [OPTIONS]
```

| 参数 | 默认 | 说明 |
|------|------|------|
| `<SYMBOL>` | — | 交易对，如 `BTC-USD`（必填） |
| `--spread-bps` | `5` | 距 mark 价的半价差（bps）：L0 买 = mark×(1−spread)，卖 = mark×(1+spread) |
| `--band-bps` | `20` | 合格带守卫：绝不报到 mark ± band 之外（须 > spread） |
| `--size` | `0.01` | 每侧每档数量（取整后须 ≥ 交易对最小下单量） |
| `--levels` | `1` | 每侧报价档数 |
| `--level-step-bps` | `2` | 档间距（bps，`--levels > 1` 时生效） |
| `--refresh-bps` | `3` | anti-flicker：报价中心相对下单时漂移超过此值才重报 |
| `-i, --interval` | `5` | 循环间隔（秒） |
| `--max-position` | `0.05` | 最大绝对持仓；会把继续加仓的一侧压制掉；live 启动仓位超过该值时拒绝启动 |
| `--skew-bps` | `0` | 库存 skew：满仓时把报价中心向减仓侧偏移的最大幅度（bps），0 关闭。见 [13.3](#库存-skew) |
| `--inventory-exit-pct` | `0` | 主动减仓触发线：仓位达到 `--max-position` 的此百分比时启动退出流程；需同时设置 `--inventory-exit-qty`，0 关闭 |
| `--inventory-exit-qty` | `0` | 单次 reduce-only 主动退出的最大数量；先确认 maker 空簿再下市价单，提交后未确认会 fail-safe，0 关闭 |
| `--max-divergence-bps` | `25` | 当 mark 价与盘口中价背离超过此值时跳过该轮（不动挂单） |
| `--vol-pause-bps` | `0` | 波动率熔断：mark 在配置窗口内的极差达到此值（bps）即撤掉全部报价暂停，回落到一半以下才恢复。0 关闭。见 [13.3](#波动率熔断) |
| `--vol-window` | `12` | 波动率熔断测量极差的窗口（最近 N 轮） |
| `--adaptive-spread[=true\|false]` | 文件值/关 | 只覆盖 TOML `[adaptive_spread].enabled`；档位和边界仅在 TOML 中配置 |
| `--stop-loss` | `0` | 会话 PnL 跌到负阈值时 fail-safe：冻结、撤销 maker 单、等待 critical webhook 后停机；不会自动平仓。0 关闭 |
| `--alert-loss` | `0` | 风险告警：mark-to-market PnL 跌到 −此值（计价单位）时告警。0 关闭 |
| `--alert-inventory-pct` | `0` | 风险告警：\|仓位\| 达到 `--max-position` 的此百分比时告警。0 关闭 |
| `--alert-position-change-pct` | `0` | 实际仓位相对通知锚点累计变化达到 `--max-position` 的此百分比时告警。0 关闭 |
| `--alert-uptime` | `0` | 风险告警：双边 uptime 跌破此百分比时告警（过预热期后）。0 关闭 |
| `--alert-equity-below` | `0` | 账户 equity 低于此绝对值时告警；仅 live、需要账户快照。0 关闭 |
| `--alert-margin-below` | `0` | cross available margin 低于此绝对值时告警；仅 live。0 关闭 |
| `--stop-equity-below` | `0` | **硬熔断**（不是告警）：账户 equity 低于此绝对值时 fail-safe 停机并交接残余仓位；仅 live。0 关闭 |
| `--stop-margin-below` | `0` | **硬熔断**（不是告警）：cross available margin 低于此绝对值时 fail-safe 停机并交接残余仓位；仅 live。0 关闭 |
| `--alert-webhook` | 无 | 除 stderr/JSON 外，把告警 POST 到此 URL |
| `--alert-webhook-format` | `slack` | webhook 报文格式：`slack` / `feishu` / `telegram` / `raw` |
| `--no-ws` | 关 | 禁用 WebSocket 行情，改为每轮 REST 轮询 |
| `--live` | 关 | 下真实单（不带此标志即 paper 模式） |
| `--order-response-reconnect-attempts` | `3` | 单轮订单回报流安全重连次数；每次先清理、换 session 并对账，`0` 禁用并保持原 fail-closed 行为 |
| `--order-response-reconnect-backoff` | `2` | 安全重连基础退避秒数；单轮耗尽后保持冻结并按最高 60 秒的退避继续下一轮 |
| `--account-stream-reconnect-attempts` | `3` | 单轮账户流安全重连次数；重连后回放、REST 回补并对账，`0` 禁用并保持原 fail-closed 行为 |
| `--account-stream-reconnect-backoff` | `2` | 账户流重连基础退避秒数；单轮耗尽后重新验证空簿并继续下一轮 |

传输故障本身只影响可用性：重连期间 maker 保持冻结且不下新单。只有清理无法确认空簿、
持仓回补后仍无法对账、硬鉴权错误或显式禁用重连时才 fail-safe 停机。旧版
`--recovery-incidents-per-window` / `--recovery-window-secs` 仍兼容解析，但已弃用且不参与运行决策。

启动时会做快速校验（fail fast）：交易对存在且在交易中、`spread-bps > 0`、`band-bps > spread-bps`、`size` 取整后 ≥ 最小下单量、`skew-bps ≥ 0`；主动退出必须同时设置百分比与数量。

### 专用配置文件

默认会读取 `~/.config/standx/maker.toml`（macOS 为 `~/Library/Application Support/standx/maker.toml`）；也可用 `--maker-config <PATH>` 指定文件。文件只保存非敏感策略参数，命令行显式参数优先于文件，文件未设置的字段继续使用内置默认值。

完整可复制模板见 [`examples/maker.toml`](../examples/maker.toml)。

```toml
# maker.toml — 不放 JWT、私钥、--live 或 webhook URL
spread_bps = 8.0
band_bps = 30.0
size = 0.001
levels = 2
level_step_bps = 2.0
refresh_bps = 4.0
interval = 5
max_position = 0.01
skew_bps = 6.0
max_divergence_bps = 25.0
vol_pause_bps = 40.0
vol_window_secs = 60
alert_inventory_pct = 80.0
alert_position_change_pct = 20.0
no_ws = false

[adaptive_spread]
enabled = true
min_spread_bps = 8.0
max_spread_bps = 18.0

[[adaptive_spread.tiers]]
spread_bps = 8.0
refresh_bps = 4.0

[[adaptive_spread.tiers]]
enter_vol_bps = 10.0
exit_vol_bps = 7.0
spread_bps = 12.0
refresh_bps = 5.0
```

`vol_window_secs` 是 TOML-only 时间窗字段，与 CLI `--vol-window` 冲突，adaptive 开启时
必须配置。Adaptive spread 接受 2–3 档。基础档必须与顶层 spread/refresh 相同；更高档定义升档和
回落阈值。每轮只派生 effective spread/refresh。升档不会立刻撤掉已有窄单，仍由原
anti-flicker ref-center 漂移或安全条件触发自然换价。关闭时与静态 planner 等价。

```bash
# 文件值生效
standx maker run BTC-USD

# 本次运行覆盖文件中的 size
standx maker run BTC-USD --size 0.002

# 使用另一套策略文件
standx maker run ETH-USD --maker-config ./configs/eth-maker.toml
```

---

## 13.3 工作原理

### Anti-flicker reconcile

每一轮，机器人对比"期望报价"与"当前挂单"，按以下决策表逐条处理每个挂单（顺序即优先级）：

| # | 条件 | 动作 |
|---|------|------|
| 1 | 该侧被 max-position 压制 | 撤单（side_suppressed） |
| 2 | 该 (side, level) 已无期望报价 | 撤单（stale） |
| 3 | 挂单价出了当前合格带 | 撤单（outside_band） |
| 4 | 挂单价穿过当前 touch | 撤单（would_cross） |
| 5 | 报价中心相对下单时漂移 > refresh-bps | 撤单（mark_moved） |
| 6 | 以上都不满足 | **保持（HOLD）** |

保持是关键：只要还在带内、未穿价、漂移未超阈值，就不动它 —— 这正是 SIP-5A 奖励的 uptime。

### 库存 skew

被动做市有天然逆向选择：买单只在下跌时成交、卖单只在上涨时成交，库存会往亏损方向累积。`--skew-bps` 把报价中心按当前仓位偏移：

```
center = mark × (1 − skew_bps × clamp(position / max_position, ±1) / 1e4)
```

持多头时中心下移 → 减仓侧（卖）更贴近 mark（更易成交）、加仓侧（买）更远（更难成交）；持空头相反。这把 `max-position` 从"急刹车"变成"渐进回中"。anti-flicker 的锚点也是这个中心，所以同一条重报规则同时响应 mark 漂移与库存 skew。

> **注意**：paper 模式下 skew 只有在模拟成交累积出仓位后才生效；`--skew-bps 0`（默认）时行为与不带 skew 完全一致。

### 主动库存退出

`--inventory-exit-pct 80 --inventory-exit-qty 0.01` 表示仓位达到上限的 80% 后，先撤销 maker 自有报价；在下一轮确认 maker 空簿且没有待确认下单时，再提交一笔最大 `0.01` 的 reduce-only 市价单。多头只卖出、空头只买入，数量不会超过当前仓位。该功能只在 live 模式生效，默认关闭。波动熔断期间只撤单、不发送主动市价退出；每笔退出必须由 `sxmk-exit-` 关联成交进入账本后才能允许下一笔 chunk，未确认时策略 fail-safe，不会重复追单。

### 行情来源与守卫

- **WebSocket feed**：价格与深度走同一条公共连接；每条行情保留交易所 `seq`、服务端时间与本地单调接收时间。StandX 的 `price` 与 `depth_book` 推送频率不同，因此按各自 5 秒 freshness 预算组成最新快照；任一边超过 5 秒、序号/服务端时间回退，或两边本地/服务端时间差超过同一 5 秒预算时自动回退 REST（覆盖预热、断线、`--no-ws`）。mark 与 book mid 仍由 `max_divergence_bps` 独立约束，盘口交叉则直接 skip 本轮，不用不可信的 touch 继续报价。
- **早醒重报**：循环在 sleep 期间若发现 mark 已漂过 `--refresh-bps`，或新的 touch 会使已有报价穿价、触发 mark/mid 背离守卫，会提前进入下一轮；最小间隔为 1 秒，避免形成逐 tick 的撤挂循环。
- **mark/mid 背离守卫**：mark 价与盘口中价背离超过 `--max-divergence-bps` 时，本轮不做任何动作（不撤不挂），避免在数据源打架时误动作。

### 波动率熔断

快速行情里被动做市最容易被"扫单"（逆向选择）。`--vol-pause-bps` 开启后，机器人跟踪 mark 在样本窗或 `--vol-window-secs` 时间窗内的极差（(max−min)/min，bps）：

- 极差 **达到 `--vol-pause-bps`** → **熔断**：撤掉全部挂单、暂停报价（`⚡HALT`）。
- 极差 **回落到阈值一半以下** → 恢复报价。

采用滞回（hysteresis）：大幅波动必须先滚出窗口、极差降到一半以下才恢复，避免在阈值附近反复开关。熔断期间不报价 = 主动放弃这段 uptime，换取不被扫单——这是有意的取舍。`--vol-pause-bps 0`（默认）关闭。

> 权衡：熔断会牺牲 SIP-5A 的 uptime 得分。阈值要结合 [13.4](#134-输出与遥测) 的遥测来定——熔断太频繁会拉低 uptime，太松则起不到保护。mark 价通常比盘口 touch 更"黏"，据此设阈值。

---

## 13.4 输出与遥测

三种输出格式：

- **表格（默认）**：每轮一行 `[时间] #轮次 mark= bid= ask= pos= pnl= | hold= place= cancel=`，其下缩进列出 PLACE / CANCEL / HOLD / FILL 明细。Live 模式还会打印 `ACCOUNT balance= equity= available= upnl=`，数据来自最近一次 REST 账户快照（正常每 30 秒刷新；启用 equity/margin floor 时，account-stream `balance` 更新会立即触发一次权威 REST 刷新；短暂失败时最多复用 60 秒）；这里的账户 `upnl` 与机器人本次会话的 `pnl` 是两个不同口径。
- **JSON（`--output json` 或 `--openclaw`）**：每个动作一行 JSON；每轮末尾一条 `cycle_summary`，保留原字段并新增可选 `rolling_vol_bps`、`adaptive_spread_enabled`、`adaptive_spread_tier`、`effective_spread_bps`、`effective_refresh_bps`、`size_skew_enabled`、`size_skew_active`、`size_skew_add_side`、`size_skew_inventory_ratio`、`size_skew_add_qty`、`exit_kind`、`exit_submitted`、`exit_suppressed` 与 `performance`
对象。阶段 5-b 的三个 exit 字段始终存在（无退出时为 `null` / `false`），因此"本轮没有
退出"与"旧 run 缺字段"可以区分：`exit_kind` 是 `inventory_trim`（阈值 trim）或
`wind_down`（监督者收尾），`exit_suppressed` 是 `volatility_halt` 或
`market_data_inactive`——波动熔断期间两种退出都被抑制（不存在"熔断期紧急退出"策略，
见 [18 号当前安全政策](18-maker-strategy-roadmap.md)），抑制不再是静默的。
`inventory_exit_submitted` 事件行也带上了 `exit_kind`。

行情降级进入 standby 时，除了每 60 秒一条 `risk_notification(kind=market_data,
event=divergence_standby|transport_standby)`，还会同步发一条**结构化**的
`action:"market_data_standby"`：`fault_class` / `paused_secs` / `quoteable_streak` /
`snapshots_required` / `divergence_bps` / `threshold_bps` / `maker_book_empty`。
风险通知负责叫醒人，结构化事件负责可计数——standby 时长是"恢复迟滞是否值得实现"
（Divergence B）的判据输入，而只能靠 regex 一句话解析出来的阈值不算阈值。`vol_bps` 仍只在 halted 时出现，旧语义不变。performance 包含 passive/exit 数量与现金流、数量加权 capture、净 PnL 归因、1s/5s/30s markout、时间加权双边 uptime、合格深度时间积分和库存持有时间。启用公共 WS 时，`cycle_summary.ws_snapshot` 以观察字段记录 mark/book 的 seq、统一/原始 envelope/payload 时间、本地 age，以及 server/local skew；这些字段不参与策略、风控或行情源选择。停机时另有 `performance_summary`、逐请求 `order_latency`、place/cancel 分位数 `order_latency_summary`；账户 typed event 产生 `account_event_lag`。逐请求延迟保留 `generation/cycle/symbol/side/level/market_source/recovery`，自动恢复完成后的首个成功周期标为 `recovery=true`；socket write、venue ack 与 account effective 始终分开，超时请求另有 `timeout_phase` 和从 intent 起算的 `timeout_ms`。旧消费者可以忽略这些新 action/可选字段。`funding_available=false`、`execution_costs_unavailable>0` 或 `funding_unattributed>0` 时，`net_pnl_complete=false`，数值字段仍可用于已知部分但不能被解释为完整净收益。**funding 现金流自 2026-07-28 起从 authenticated `GET /api/query_funding_history` 权威读入**（不是按费率推导）：在 30 秒 REST audit 周期里增量拉取、按行 id 去重（该端点的 `last_id` 是向历史翻页的游标，不是"since"游标）、按时间升序折进 `funding_quote`；`qty` 的符号即场馆口径（负=付出、正=收取）。结算资产不是 quote 也不是其 D 前缀形式（如 `DUSD` 对 `USD`）、或乱序到达无法折算的行，计入 `funding_unattributed` 而不是被丢弃——现金流真实存在，所以完整标志必须为 false。另有 `funding_coverage_gap`（布尔、latch）表示"这一段根本没读到"：funding 拉取失败，或返回页数达到请求上限、更早的行可能被截断；它与 `funding_unattributed` 的区别是后者计的是"看到了但折不进去"的行。**funding 拉取失败不会让 audit 失败**——同一个 audit 还承担仓位对账与恢复，让遥测端点故障中断安全对账是严重性倒置。`pnl` 和 `fills_total` 只属于当前 maker session：已有仓位按启动 mark 自动接管并把 session PnL 归零，历史交易所盈亏仍看 `account.upnl`。live session PnL 使用 current-run ledger 的权威仓位；每个去重后的增量成交会原子更新现金流与统计仓位。live fill 还包含 `trade_id`、`order_id`、`trade_ts`、`origin`、`role`、成交时 mark/事件时间，以及可换算时的 `fee_quote` / `rebate_quote`。

live 启动时会先清理旧 `sxmk-` 订单并同步账本，再认证 `order + position + trade + balance` account stream 和 Order Response Stream。绝对仓位不超过 `max_position` 时自动接管，输出 `ledger_sync` / `inventory_adopted`；超过上限（允许半个数量 tick 误差）则输出 `startup_rejected` 并退出。带稳定 `trade_id`/`order_id` 的 account-stream trade 与 REST backfill trade 走同一账本入口并按 `trade_id` exactly-once 去重；order 回调只确认订单归属与生命周期，不能单独记账。健康运行时订单、pending 命令、仓位和原始余额由本地 typed-event 投影维护，普通 maker cycle 不读取账户 REST；每 30 秒并发读取 open orders、positions、order history 和 trades 做完整审计，derived balance 默认按 30 秒 REST 快照刷新。WS `balance` 只包含钱包级 `free/total/locked/occupied`，不能冒充统一余额的 `equity/cross_available`；配置账户风险 floor 后，该事件会合并触发下一轮立即读取权威 REST balance，并复用同一 edge-triggered 告警状态机。account stream 断开、投影审计不一致或仓位不一致时立即冻结 placements、撤净 maker 订单，并在约 0.5s、1.5s、3.0s 结合 WS 与 REST 核对；恢复后从空 maker book 继续，3 秒仍不一致则 fail-safe 停机。

内部统计仓位与 current-run ledger 仓位也必须保持在半个数量 tick 的容差内；超出容差会输出 `risk_notification(kind=accounting_invariant, severity=critical)`，立即进入 fail-safe 清理并以退出码 75 停机，避免使用不一致的仓位计算告警或 stop-loss。

运行时使用单一事件循环同时等待周期任务、account/order-response 回调、行情刷新和 Ctrl+C。仓位、成交或流错误在多动作 cycle 中途到达时，会先递增 generation 并进入冻结清理，再取消当前 REST/下单 future；因此本轮后续 placement 不会继续提交，可能已到达交易所的请求也始终由 maker 清理和 REST 对账补偿。旧 generation 的工作结果不会重新开启挂单。普通订单生命周期回报在 cycle 内短暂缓冲，普通行情刷新则合并为一次后续 replan，避免首单确认撕裂多订单计划或产生并发 cycle。

订单回报连接每 30 秒主动发送 heartbeat，45 秒无任何入站流量视为半开，并在 24 小时上限前（23 小时 50 分）主动轮换。create、cancel 和库存退出共用最多 256 项的 request registry；接受、拒绝和晚到回报都按 `request_id` 严格闭环。每个已提交请求还有独立的 10 秒单调时钟安全截止：ACK 未闭环时恢复 order-response stream；ACK 已接受但对应 account-order 仍不可见时恢复 account stream。两种阶段都会先冻结 placement、清理 maker 订单，再以 REST 空簿/仓位对账作为恢复门槛；绝不会因超时直接释放占位并继续下单。冻结时输出 `risk_notification(kind=order_request_timeout, event=frozen)`，包含 `request_id`、`request_kind`、`timeout_phase`、`age_ms`、`timeout_ms` 和 `recovery_target`，不会再为同一超时重复输出通用 stream-disconnected warning。重连认证后的权威快照会先按当前 run 的 filled orders 认领订单，再把稳定 `trade_id` 的 REST trades exactly-once 回补到 ledger；REST 可见性短暂滞后时按 0.5s/1.0s/1.5s 有界重查，回补后的 expected position 与 venue position 一致且 maker book 为空才恢复。无法归因的仓位差仍 fail closed。未确认 place/cancel 的 quote slot 不按策略 cycle 数过期：账户事件可能在一秒内推进多个 cycle，只有关联响应、账户订单终态或显式 cleanup 才能释放对应的 venue exposure。坏载荷、缺失或未知 `request_id`、close/error、registry 重复或溢出也会走相同 fail-closed 路径。

`alert_position_change_pct` 默认 `0`（关闭）。设置为 `20` 时，实际仓位相对上次通知锚点累计变化达到 `max_position` 的 20% 会输出 `risk_notification(kind=position_jump)` 并复用现有 webhook。方向判断独立跟踪每次连续仓位观测，并以单笔 `size / 2` 作为 neutral deadband：仓位严格低于负 deadband 为 Short、严格高于正 deadband 为 Long，其余（包括边界）为 Neutral；只有相邻观测直接发生 `Short ↔ Long` 才输出 critical `direction_flip`，经过 Neutral 的转换不会误报。例如 `size=0.2` 时，`-0.074 → +0.126` 是 `Neutral → Long`，`+0.126 → -0.074` 是 `Long → Neutral`，达到累计变化阈值时最多输出 warning `position_jump`。跨越 inventory-exit/max-position 阈值仍不受 jump 百分比限制。account stream 断流/恢复、reconciliation 冻结/恢复/失败、volatility breaker、inventory exit、cleanup residual 和最终 fail-safe 也会生成结构化风险通知。
- **Quiet（`--quiet`）**：只打印成交与增删挂单。

退出时打印本次会话统计：

```
👋 Stopping maker (ran 120 cycles: 40 places, 38 cancels, 210 holds)
   6 fills | uptime 92% | max pos 0.0030 | avg capture 1.7bps | PnL +0.42 (mark-to-market)
   paper sim: ending position 0.0010
```

**遥测指标含义**：

| 指标 | 含义 |
|------|------|
| PnL（mark-to-market） | 已实现现金 + 持仓按 mark 计价，一个数同时体现点差捕获与库存盈亏 |
| avg capture (bps) | 每笔成交在赚钱方向上离 mark 的平均距离 —— 做市 edge |
| uptime % | 同时挂着买卖单的周期占比 —— SIP-5A 真正奖励的东西 |
| fills / max pos | 成交笔数、会话内最大绝对持仓 |

> paper 用精确模拟成交计算；live 从认证成交历史读取本次会话内、已关联到 `sxmk-` maker 订单的成交，并按成交 ID 去重。不会把手工/API 订单或单纯仓位变化当作 maker 成交。当前 mark-to-market 仍基于成交现金流与仓位估值；费用币种和交易所已实现盈亏会单列核对。**调参在 paper 里做**：观察 avg capture 与 PnL 的关系来调 `--spread-bps` / `--refresh-bps` / `--skew-bps`。

### 确定性回放

阶段 1 的 normalized NDJSON trace 可通过下面的纯回放命令验证；该子命令不加载认证、环境
变量或实时时钟，不连接网络，也不执行订单 I/O：

```bash
standx --output json maker replay ./traces/btc-phase1.ndjson
# 也可从 stdin 读取
standx --output json maker replay - < ./traces/btc-phase1.ndjson
# 仓库内可直接运行的 schema v1 示例
standx --output json maker replay ./examples/maker-replay-trace.ndjson
```

trace 第一条必须是 `schema_version=1` 的 `header`，冻结 `git_sha`、`config_hash`、`seed`、
完整 maker config 和 replay settings；随后只接受 typed `cycle`、`fill`、`funding`，最后以
`finish` 关闭时间加权区间和未满足的 markout 窗口。未知字段、header/finish 缺失、时间回退
或非有限数值都会拒绝回放。同一 trace 连续运行会得到相同的 `replay_cycle` 与
`replay_summary` 结构化结果。

仓库内阶段 1 合成验收可直接执行：

```bash
cargo build -p standx-cli --offline
python3 scripts/verify_maker_stage1.py
```

验收器会连续执行三次回放并比较完整 stdout，同时手工核对 PnL 守恒、passive/exit 归因、
markout unavailable、时间加权 uptime/深度积分和库存持有时间；它只运行本地二进制和仓库内
trace，不连接交易所。

### 风险告警

遥测默认只**展示**指标；`alert_*` 把它变成**主动通知**。所有阈值各自 opt-in（0 关闭）：

- `--alert-loss <X>`：mark-to-market PnL 跌到 −X 时告警（回升到 −X/2 以上解除）。
- `--alert-inventory-pct <P>`：\|仓位\| 达到 `--max-position` 的 P% 时告警（跌回 0.9×P% 以下解除）——趋势市里最先亮的灯。
- `--alert-position-change-pct <P>`：实际仓位相对上次通知锚点累计变化达到 `--max-position` 的 P% 时告警。
- `--alert-uptime <U>`：双边 uptime 跌破 U% 时告警（前 20 轮预热期不判）。
- `--alert-equity-below <X>`：live 账户 equity 低于 X 时告警，恢复到 1.1×X 以上解除。
- `--alert-margin-below <X>`：live 账户 cross available margin 低于 X 时告警，恢复到 1.1×X 以上解除。

告警是**边沿触发**的：进入异常态时响一次、恢复时响一次，不会每轮刷屏。输出到 stderr（`🚨 ALERT` / `✅ RESOLVED`）与 JSON（`action:"alert"`）；若设了 `--alert-webhook <url>`，还会 POST 一条 JSON（fire-and-forget，慢/坏的端点不会拖住报价循环）。

`--stop-loss` 不是告警阈值，也不是止盈/自动平仓：它在会话 PnL 触线后执行 fail-safe
停机，撤销 maker 自有挂单并交接残余仓位。`--inventory-exit-pct/qty` 才是正常运行中的
reduce-only 主动库存退出，而且只在 live 生效；两者不能互相替代。

**账户硬熔断（`--stop-equity-below` / `--stop-margin-below`，阶段 5-b）**与上面的
`--alert-equity-below` / `--alert-margin-below` 是两件事，配置名和 typed 结果都不同：

| | 告警 (`alert_*`) | 硬熔断 (`stop_*`) |
|---|---|---|
| 作用 | 通知，边沿触发，运行不变 | fail-safe 停机（撤 maker 单 + 残余仓位交接） |
| typed 结果 | `action:"alert"` | `action:"account_floor"` + `RuntimeStopReason::AccountFloor` |
| 与 `--stop-loss` 的关系 | 无 | 同为停机，但 `stop_loss` 判"策略在亏"，account floor 判"账户还能不能交易" |
| 默认 | 0（关） | 0（关） |

判定时点与失败语义（都在**下单之前**）：

- 硬熔断在 cycle 内拿到权威余额、账本同步完成之后、**任何报价/退出下单之前**判定。
  破线那一轮因此一张单都不会发，只剩收尾撤单。
- equity 先判、margin 后判；恰好等于阈值不算触发。
- **余额读不出来或过期时 fail-closed 停机**（`event:"unevaluable"`，
  `metric:"balance_unreadable"` / `"balance_stale"`）：武装了硬熔断却拿旧快照当"未破线"
  等于没有熔断。过期线为 35s（一次 30s 刷新周期 + 一次 5s 重试），比告警侧复用缓存的
  60s 上限更严。只有**该熔断真正读的字段**需要可解析——只武装 equity floor 时，
  margin 字段坏掉不会牵连停机。
- 武装任一硬熔断即视为"观察账户风险"，余额事件会照常唤醒刷新（不再只看 `alert_*`）。
- 熔断未武装（默认 0）时以上全部短路：过期或坏字段都不会停机。

两者都**不会自动平仓**——见下节。

### 残余仓位交接（阶段 5-b）

maker 在任何路径上都不自动平仓，所以每条退出路径都要给运维人员一个**唯一权威**的数字。

交接**在撤单收尾之后**判定，并以场馆 REST 仓位为准：正在被撤的挂单可能在撤单过程中成交，
而那时账户流已经断开，所以会话账本不能算最终答案。三种结果（JSON `action:"residual_position"`）：

| `event` | 含义 | 输出 |
|---|---|---|
| `flat` | 场馆与账本一致确认空仓 | `ending position flat (venue-confirmed)` |
| `handoff` | 场馆确认持仓，需人工平 | `⚠️ residual position handoff: ...` + critical webhook |
| `unknown` | 场馆快照拿不到/读不出，或与账本不一致 | `🛑 residual position UNKNOWN ...` + critical webhook |

**"无法确认空仓"不等于"空仓"**：REST 失败、场馆或账本数值非有限、两者差异超过数量容差
（例如场馆报空但账本记 0.2），全部归为 `unknown` 并要求人工去场馆核对。JSON 里另有
`venue_position` / `ledger_position` / `unknown_reason` / `needs_operator` 字段；
`🔴 maker stopped` 生命周期消息和 critical webhook 同样带上仓位或 UNKNOWN 提示——
离开终端的运维人员只能从 webhook 得知这件事。paper 模式没有场馆，模拟仓位即权威。

`--alert-webhook-format` 按目标平台组织报文:

| 格式 | 报文 | 用法 |
|------|------|------|
| `slack`(默认) | `{"text":"..."}` | Slack incoming webhook |
| `feishu` | `{"msg_type":"text","content":{"text":"..."}}` | 飞书/Lark 自定义机器人 |
| `telegram` | `{"text":"..."}` | Telegram sendMessage,token 和 chat_id 放 URL 里:`https://api.telegram.org/bot<TOKEN>/sendMessage?chat_id=<ID>` |
| `raw` | 完整结构化对象(text + ts/symbol/kind/firing/message) | 自定义消费端 |

```bash
# 飞书
standx maker run BTC-USD --alert-loss 50 --alert-inventory-pct 80 \
  --alert-webhook <飞书机器人地址> --alert-webhook-format feishu

# Telegram
standx maker run BTC-USD --alert-inventory-pct 80 \
  --alert-webhook "https://api.telegram.org/bot<TOKEN>/sendMessage?chat_id=<ID>" \
  --alert-webhook-format telegram
```

> 注意:这些覆盖的是**金融风险**;基础设施类告警(feed 断线、连续错误 fail-safe、退出残留单)是内置的,始终开启。

**启动/停止通知**:只要设了 `--alert-webhook`,机器人还会在启动时推一条 🟢(含模式与关键参数)、退出时推一条 🔴(含停止原因——Ctrl+C 或 fail-safe——及本次会话汇总)。**不需要**配任何风险阈值即可生效;停止消息会等待投递完成再退出进程,确保送达。JSON 模式下同时输出 `action:"lifecycle"` 行。

---

## 13.5 Live（实盘）模式

> ⚠️ **风险提示**：live 模式会持续下真实的 post-only（ALO）订单，且始终锁定在
> release-owner 环境变量之后。生产 canary 记录见 [14-maker-live-gate.md](14-maker-live-gate.md)。

在仓库根目录的 `.env.local` 配置通知地址：

```bash
STANDX_SUPERVISOR_WEBHOOK=https://your-webhook-url
STANDX_SUPERVISOR_WEBHOOK_FORMAT=feishu
```

`maker run --live` 会自动读取这两个字段，无需再传 `--alert-webhook`：

```bash
export STANDX_ENABLE_LIVE_MAKER=1        # 解锁（自行承担风险）
standx --output json maker run XAG-USD \
  --maker-config examples/maker-xag-100u-conservative.toml \
  --live --yes
```

优先级为：显式命令行参数 > 进程环境变量 > `.env.local`。

live 模式的安全栏：

- **启动即 cancel-all**：从干净的盘口开始，避免上一轮残留干扰对账。
- **机器人接管该交易对的全部挂单**：手动挂的单会被当作 stale 撤掉。
- **拒单容错**：post-only 穿价拒单、撤已成交单等属正常事件（记录后下轮重报），不计入 fail-safe；只有网络/5xx 等瞬时故障才计数。
- **部分成交容忍**:部分成交的挂单保留剩余部分继续挂,不会被误撤。
- **订单回报安全重连**：WebSocket 断开或报错后立即停止新下单，记录 close code、
  close reason 或底层错误，清理并验证 maker 空簿；随后使用新 session 认证，并重新
  查询挂单、仓位、已成交订单及会话成交。只有全部对账通过才恢复报价。
- **fail-safe 停机**：安全重连关闭、清理/对账失败、终态鉴权错误，或连续 3 次其他瞬时
  cycle 错误时停机并再次清理。单轮重连次数耗尽只会保持冻结、重新验证空簿并退避进入
  下一轮。隐藏的 `--controlled-disconnect-after` 用于受监督 gate：它在指定秒数后注入一次
  本地订单回报断流，强制走 fail-safe 停机演练——冻结、只撤 maker 自己的挂单、然后 fail-safe
  停机（非零退出），**不会重连恢复报价**。它专门验证 fail-closed 路径，重连/对账/恢复报价由
  真实断流覆盖；仅可按 live runbook 使用。
- **退出必清理**:所有退出路径都会 cancel-all(3 次重试 + 校验),有残留会大字告警并给出手动撤单命令。

---

## 13.6 使用示例

```bash
# 多档报价(每侧 3 档,档间距 1bps)
standx maker run BTC-USD --levels 3 --level-step-bps 1 --size 0.001

# 开启库存 skew,并用 JSON 输出喂给 agent / 日志管道
standx maker run ETH-USD --skew-bps 5 --output json

# 强制 REST 轮询(不用 WebSocket)
standx maker run BTC-USD --no-ws --interval 5

# 全局 --dry-run:只打印说明,不进入循环
standx --dry-run maker run BTC-USD
```

---

## 13.7 测试检查清单

### Paper 模式(零风险,只读公共 API)

- [ ] `standx maker run BTC-USD --size 0.001 --interval 3` 首轮两侧各下一单
- [ ] 平静期出现 HOLD,age 递增
- [ ] mark 漂过 refresh-bps 时出现 CANCEL(mark_moved)+PLACE
- [ ] 盘口穿过某侧挂单时,只撤违规一侧(would_cross)
- [ ] `--output json` 每行都是合法 JSON,含 cycle_summary
- [ ] touch 穿过报价时出现 FILL,pos 与 pnl 变化
- [ ] `--vol-pause-bps` 设很小(如 2)时,波动中出现 `⚡HALT`、全部挂单被撤、平静后恢复
- [ ] `--alert-inventory-pct 50`(配小 `--max-position`)时,仓位过半出现 `🚨 ALERT`,只响一次;`--alert-webhook` 指向本地监听器能收到 POST
- [ ] `Ctrl+C` 退出时打印统计并(live)清理挂单

### 参数校验

- [ ] `--band-bps` ≤ `--spread-bps` 时报错
- [ ] `--size` 小于最小下单量时报错
- [ ] `--skew-bps` 为负时报错
- [ ] 未知交易对时报错并列出可用交易对

### Live 模式(需 `STANDX_ENABLE_LIVE_MAKER=1` + 认证)

- [ ] 不设环境变量时 `--live` 报 "live mode not yet enabled"
- [ ] 解锁后小额观测:ALO 拒单行为、启动 cancel-all、退出清理

---

## 13.8 常见问题

### Q: paper 模式为什么 PnL / skew 一直是 0?

paper 只有在模拟成交累积出仓位后才有非零仓位与 skew。若市场平静、touch 从未穿过你的报价,就不会有成交。用更紧的 `--spread-bps` 或更长运行时间即可观察到。

### Q: `--live` 报 "live mode not yet enabled"?

live 模式当前锁定,需要 `export STANDX_ENABLE_LIVE_MAKER=1` 解锁,并确保已 `standx auth login`(含私钥)。

### Q: 如何选择 `--spread-bps` / `--refresh-bps` / `--skew-bps`?

在 paper 里跑,观察退出统计的 **avg capture(点差捕获)** 与 **PnL**:capture 为正说明在赚点差,PnL 为负说明库存风险压过了点差 —— 此时应收紧仓位或调大 skew。

---

## 下一步

- 认证配置:[02-authentication.md](02-authentication.md)
- 输出格式:[09-output-formats.md](09-output-formats.md)
- 下单/撤单基础:[05-orders.md](05-orders.md)
- 策略迭代路线与验收标准:[18-maker-strategy-roadmap.md](18-maker-strategy-roadmap.md)
