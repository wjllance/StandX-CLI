# 阶段 5-b（安全轨二级）立项：分级退出政策的 typed 分离与残余仓位交接

**状态：本轮四项范围已实现（2026-07-27，`implemented`），离线验证全绿；未合并、无 live
动作。剩余 5-b 条目（Divergence B/C/D 等硬化项）见"不在本轮范围"。实现摘要见文末
[实现记录](#实现记录2026-07-27)。**

2026-07-27 立项（release owner 裁决：阶段 3 与阶段 3-guard 关闭后，主线推进 5-b）。
上游：[18-maker-strategy-roadmap.md 阶段 5](18-maker-strategy-roadmap.md)、
[25-maker-short-term-roadmap-2026-07-27.md](25-maker-short-term-roadmap-2026-07-27.md)、
[adr/0001-maker-recovery-supervision.md](adr/0001-maker-recovery-supervision.md)。

**定位**：5-b 是**扩大规模（加 size / max_position / 多 symbol）的代码级前置**，不是
alpha 候选。它不改变任何默认 live 行为、不消耗 live 时间片、不追求 PnL 改善；产出是
"同一个词不再指两件事"——退出路径、账户熔断和残余仓位交接在类型、日志和 JSON 上可区分，
并有确定性测试钉住。

## 现状对照（2026-07-27 逐项核对 main）

| 18 号阶段 5 二级条目 | 代码现状 | 缺口 |
|---|---|---|
| 正常 trim 与 emergency exit 用不同 typed policy/effect | 只有一个 `InventoryExit { side, qty }`（`crates/standx-maker/src/lib.rs:137`），由 `inventory_exit_plan`（阈值 trim）或 `wind_down_exit_plan`（监督者收尾）产出，**类型上无法区分**；执行侧统一走 `OrderRequestKind::InventoryExit`（`pipeline.rs:27`）与 `action:"inventory_exit_submitted"`（`cycle.rs:871`） | 证据侧只能人工推断退出来源——阶段 3-guard 判定报告里"3 次主动退出均为 wind-down 边界退出"就是人工归因的结果。**紧急退出今天不存在**（stop-loss 只停机不平仓），所以 typed 分离要做的是"把已有的两种正常退出区分开，并为紧急退出留出显式的、默认关的位置" |
| 明确 volatility halt 期间是否允许紧急退出；默认不得继承正常退出行为 | 已实现且方向正确：`plan_cycle` 的 `inventory_exit = (!halted && market_active).then(...)`（`lib.rs:693`），注释写明"emergency execution needs a separate explicit policy" | **抑制是静默的**：`requested_inventory_exit` 与 `inventory_exit` 的差异没有任何事件输出，遥测里看不到"halt 吃掉了一次退出"。政策也从未正式定稿（本文定稿，见下） |
| stop-loss 后残余仓位输出明确 handoff | 残余**订单**有完整处理：`cancel_maker_orders_with_retry` + `kind:"maker_cleanup"/event:"residual_orders"` critical 通知 + fail-safe 非零退出（`runtime/lifecycle.rs:100-182`） | 残余**仓位**没有交接输出：live 模式的收尾摘要只打 PnL，**终仓一行只在 paper 模式打**（`lifecycle.rs:88-93`）；JSON 模式没有任何终仓事件；fail-safe 通知里仓位只作为 `expected` 字段出现，没有"你现在持有多少、去哪里手动处置"的显式交接 |
| 自动 flatten 默认关闭、单独授权 | 代码里**根本没有 flatten**，因此"默认关闭"平凡满足 | 需要把"不自动平仓"从"未实现"升级为"显式记录的政策"，否则下一个人会把它当作遗漏来补 |
| equity/margin 的 alert 与 hard floor 用不同配置名和不同 typed outcome | 只有 alert：配置 `alert_equity_below` / `alert_margin_below`（`config.rs:152`）→ `AlertMonitor::with_account_floors`（`lib.rs:785`），内部字段叫 `equity_floor` / `margin_floor` | **命名已经在混**：alert 阈值在内部就叫 "floor"，等 hard floor 真的加进来必然撞名。hard floor 本身不存在——扩规模后账户级刹车缺位 |
| 背离恢复迟滞 / 熔断豁免 | 方案 A（分类 + standby）已在 main；B/C/D 未做 | 本次**不纳入**（独立面，见"不在本轮范围"） |

前三项在 18 号验收标准里标注为"针对已落地行为、验收方式为复核"的条目
（短暂背离宽限、超阈值冻结、cleanup 未确认不恢复）已由
[ADR 0001](adr/0001-maker-recovery-supervision.md) 与 `market_data.rs` 状态机覆盖，
本轮只补引用不动代码。

## 政策定稿（本文的两个决策）

### D1：volatility halt 期间不允许任何自动退出（维持现状，正式定稿）

**决定**：halt 期间**两种**正常退出（阈值 trim 与 wind-down 收尾）都继续被抑制，且
**不引入** "halt 期间紧急退出" 的策略开关。

依据：

- halt 的语义是"行情快速移动、我们的价格信息最不可信"，此时发 reduce-only **市价**单
  是把最差的滑点主动锁定；已终止的阶段 4 已经证明"在最糟糕的时刻主动改变报价行为"是
  负期望（恒宽加宽 live 判负）。
- 尾部风险在小额边界内由风险预算覆盖（最坏路径 = `max_position × 不利变动 + 退出成本`）；
  在扩规模之前，紧急退出的经济价值没有任何证据支持。
- 一个默认关的开关也不是零成本：它要求 halt 期间的退出路径、部分成交与拒单都有测试与
  运维预案，而这些工作在没有证据的情况下只会腐化。

**代价与显式接受**：halt 期间 A/B 臂到点收尾会**等 halt 解除后才平仓**（wind-down 退出
同样被抑制）。这已经是今天的行为，本文把它记为已知取舍而非缺陷。触发重启该决策的条件：
出现一次"halt 期间持仓被显著打穿、且 halt 解除后退出成本明显高于 halt 内退出的反事实
估计"的实盘事件，并有该事件的逐笔证据。

**本轮的可执行部分**：把静默抑制变成**可观测**——抑制原因作为 typed 值进入 plan 与
遥测，使"halt 吃掉退出"在证据里可计数。

### D2：新增 equity/margin hard floor，默认关闭，与 alert 彻底拆名

**决定**：

- alert 侧保留现有配置名 `alert_equity_below` / `alert_margin_below`，但把内部字段与
  构造器从 `*_floor` / `with_account_floors` 改名为 `*_alert_below` /
  `with_account_alerts`——"floor" 一词此后**只**指硬熔断。
- 新增 hard floor 配置 `stop_equity_below` / `stop_margin_below`（quote 单位，默认
  `0` = 关闭），命中后走**独立** typed 结果 `RuntimeStopReason::AccountFloor`，与
  `StopLoss`（会话 PnL 刹车）区分，日志/JSON/webhook 上也不同名。
- hard floor 与 stop-loss 一样是"停机 + 撤单 + 残余仓位交接"，**不自动平仓**（与 D1 一致）。

依据：会话级 PnL 刹车（`stop_loss`）保护的是策略损耗，账户级 equity/margin 保护的是
"还能不能继续交易"——多 symbol / 加 size 后两者会脱钩，而现在账户侧只有告警、没有刹车。
默认关闭意味着本轮零 live 行为变化；扩规模授权时按 runbook 显式开启。

## 本轮范围（四项，全部默认关或纯观测）

1. **typed 退出分离**：core 引入 `ExitKind { InventoryTrim, WindDown }`，`InventoryExit`
   带 kind；`CyclePlan` 暴露 typed 抑制原因（`VolatilityHalt` / `MarketDataInactive`）。
   CLI 执行与遥测按 kind 区分（**JSON 只加字段、不改既有 `action` 名**，保持 contract 兼容）。
2. **抑制可观测**：requested 与 planned 不一致时发一条 typed 事件（新 action，additive）。
3. **残余仓位 handoff**：每条退出路径都给出唯一权威的终仓结论——**撤单收尾之后**以场馆
   REST 仓位为准，输出 `flat` / `handoff` / `unknown` 三态（human 一行 + JSON 事件 +
   critical webhook），并在文案里指明手动处置路径。"无法确认空仓"必须归 `unknown`，
   不得渲染成"无事可做"。flatten 保持不存在，政策写入文档与测试。
4. **equity/margin 拆名 + hard floor**（D2）：改名 + 新增默认关的 `stop_*` 配置与
   `RuntimeStopReason::AccountFloor`。

### 不在本轮范围

- **Divergence B / C / D**（恢复迟滞、熔断豁免、tick 阈值）：属于行情降级政策面，与本轮
  的退出/账户面正交，按各自证据独立处理。
- **自动 flatten 的实现**：D1/D2 都明确不自动平仓，本轮只固化"默认关"这一政策。
- **紧急退出的实现**：无证据，不写。typed 分离为它留出位置即可。
- 任何 alpha 参数与 PnL 目标。

## 预注册验收判据

结构与政策项：

- [ ] `ExitKind` 在 core 与 CLI 全链路可区分：plan → 执行 → 日志/JSON → 延迟/账本关联。
- [ ] 既有 JSON contract 兼容：`action` 名与既有字段不变，只新增字段/新增事件；
      `scripts/` 侧解析（run manifest / dashboard / alerts）不需要改动即可继续通过。
- [ ] 默认配置下（`stop_equity_below = stop_margin_below = 0`）逐 action 行为与本轮前
      等价——**全关 ≡ 现生产基线**的既有等价约束不被破坏。
- [ ] 冻结生产基线配置 `examples/maker-guard-hype-candidate.toml` 无需修改即可运行。

确定性测试（18 号阶段 5 验收标准逐条）：

- [ ] halt + 高库存：`InventoryTrim` 与 `WindDown` **两种** kind 都被抑制，且抑制原因为
      `VolatilityHalt`（D1 的机器可读证明）。
- [ ] 行情非 Active + 高库存：抑制原因为 `MarketDataInactive`。
- [ ] stop-loss + 残余仓位：产生 typed handoff，且不产生任何平仓订单。
- [ ] account floor 命中：走 `AccountFloor` 而非 `StopLoss`；默认 0 时永不命中。
- [ ] 退出部分成交、退出未确认（`exit_awaiting_confirmation` 不重复发单）、退出拒单、
      cleanup residual：沿用既有测试并补 kind 维度。
- [ ] 正常 trim、wind-down、hard stop、residual handoff 在类型、日志和 JSON action 上
      可区分（18 号该条的机器可读版本）。

离线验证（仓库标准，见 18 号"统一验收口径"）：

```bash
HOME=/tmp/standx-test-home CARGO_HOME=~/.cargo cargo test --workspace --offline
cargo clippy --workspace --all-targets --offline -- -D warnings
cargo fmt --all -- --check
python3 -m py_compile scripts/openobserve_dashboard.py
```

## live 授权

**本轮不需要 live 授权**：所有新增能力默认关闭，退出语义无变化，替换的是类型与输出。
扩大规模的授权（届时按 19/23 号手册记录精确授权文本）才需要开启 `stop_*` 硬熔断，
并在该次授权中记录 floor 取值与 emergency cancel 操作人。

## 实现记录（2026-07-27）

### 落地位置

| 范围项 | core（`standx-maker`） | CLI（`standx-cli`） |
|---|---|---|
| typed 退出分离 | `ExitKind{InventoryTrim,WindDown}`；`InventoryExit.kind`；两个 plan 函数各自标注来源 | `inventory_exit_submitted` 事件新增 `exit_kind`；`MakerLogEvent.exit_kind`（`None` 时 JSON 不出现该键） |
| 抑制可观测 | `ExitSuppression{VolatilityHalt,MarketDataInactive}`、`SuppressedExit{kind,reason}`、`CyclePlan.exit_suppression`；`plan_cycle` 先算"政策想要什么"再过 market/halt 门 | `ExitStatus` + `with_exit_fields`：`cycle_summary` 新增 `exit_kind` / `exit_submitted` / `exit_suppressed`（恒存在，空时 null/false）；human 行追加 `⛔exit_suppressed=kind/reason` |
| 残余仓位 handoff | — | `ResidualHandoff{Flat,Confirmed,Unknown}` + 纯函数 `residual_handoff`（model.rs）；**撤单收尾之后**取场馆 REST 仓位再判定；`emit_residual_position_handoff` 三态渲染（human + `action:"residual_position"`）；critical webhook `kind:"residual_position"`；`residual_note` 进 fail-safe 与 `🔴 maker stopped` 文案 |
| equity/margin 拆名 + hard floor | `with_account_floors`→`with_account_alerts`（字段 `*_alert_below`）；新增纯函数 `account_floor_breach` 与 `AccountFloorBreach{Equity,Margin}`；`RuntimeStopReason::AccountFloor` | `--stop-equity-below` / `--stop-margin-below` + 同名 TOML 键（默认 0）；**cycle 内、下单之前**判定的 `account_floor_stop` + typed `AccountFloorError{Breach,BalanceUnreadable,BalanceStale}`；`MakerExit::AccountFloor`；`action:"account_floor"`（`event: triggered` / `unevaluable`）；`BALANCE_FLOOR_MAX_AGE=35s`；武装 floor 即触发余额刷新观察（`account_risk_watch_enabled`）；启动横幅显示已武装的 floor |

抑制事件刻意**不**做成独立事件行：halt 可能持续很久，而 `halted` 本来就是每轮字段，
所以抑制作为 `cycle_summary` 上的一个字段既不会刷屏、又能用一次字段过滤计数。

### 对抗式复审驱动的三处返工（2026-07-27）

首版实现经一次对抗式复审判 `needs-attention`（3 个 high），三项已按下述方式返工，
并各自补了确定性测试：

1. **硬熔断曾在下单之后才判**：余额随 cycle 结果返回，判定发生在 `maker_cycle` 已经
   下过单之后——破线的那一轮仍可能新增暴露，撤单只是补救且会与成交竞速。改为在 cycle
   内拿到权威余额、账本同步完成之后、**任何下单之前**判定（`account_floor_stop` →
   `AccountFloorError` → 提前返回），破线轮的订单写入量为零，只剩收尾撤单。
2. **硬熔断不触发余额刷新、可能读旧数据**：余额事件唤醒被 `alerts.account_enabled()`
   门控，只反映 `alert_*`，不含 `stop_*`。改为 `account_risk_watch_enabled`
   （告警**或**武装的硬熔断）；并且武装状态下余额过期（>35s）或该熔断读的字段解析失败
   一律 fail-closed 停机（`event:"unevaluable"`），不再把旧快照/NaN 当作"未破线"。
3. **残余仓位交接曾在撤单之前算**：账户流已 abort，撤单期间成交无法反映，可能"报空实持"
   或数量错误。改为撤单之后取场馆 REST 仓位判定，三态输出
   `flat` / `handoff` / `unknown`；REST 失败、数值非有限、场馆与账本差异超容差都归
   `unknown`（critical，要求人工核对场馆）——**"无法确认空仓"不再等于"空仓"**。

### 判据结果

- ✅ `ExitKind` 全链路可区分（plan → 执行 → 日志/JSON）。
- ✅ JSON contract 兼容：只新增字段/键，既有 `action` 名与字段一字未改；
  `scripts/` 侧无需改动（`python3 -m py_compile scripts/openobserve_dashboard.py` 通过）。
- ✅ 默认配置行为等价：`stop_*` 默认 0，新增字段为纯遥测；冻结生产基线
  `examples/maker-guard-hype-candidate.toml`（sha256 `6314a374…`）**未修改**，并新增
  测试钉住"它解析通过且两个 hard floor 未武装"。
- ✅ 确定性测试（新增 13 项）：halt 抑制 trim / halt 抑制 wind-down（含"抑制处不出现
  任何下单"）、行情非 Active 抑制且原因优先于 halt（2×2 组合）、`account_floor_breach`
  默认关 / equity 优先 / 恰好等于阈值不触发 / NaN 不触发、标签稳定性、`AccountFloor` 与
  `StopLoss` 的 typed 映射区分、`cycle_summary` exit 字段恒存在、hard floor 与 alert
  配置键互不串台；返工后新增 `account_floor_stop` 的"未武装绝不停机（含过期/坏字段）"
  与"武装则破线/过期/坏字段各自 fail-closed、且只关心自己读的字段"、`residual_handoff`
  的场馆确认 / 快照缺失 / 双侧非有限 / 双向不一致 / 容差退化五组。
- ✅ 离线验证：`cargo test --workspace` 全绿（cli 198→207、maker 179→183，其余不变）；
  `cargo clippy --workspace --all-targets -- -D warnings` 干净；`cargo fmt --check` 通过。
  注：`integration::cli_market_commands` 里两个走公网 API 的用例偶发失败（与本轮改动无关，
  重跑即过）。

### 遗留（本轮明确未做）

- 退出**部分成交 / 未确认 / 拒单**与 cleanup residual 的既有测试未按 `ExitKind` 维度
  加参数化——现有覆盖仍然有效（这些路径不读 kind），补维度属低收益，留待有需要时做。
- 硬熔断"破线轮零订单写入"目前由**结构**保证（判定点在所有下单之前、提前返回）与
  `account_floor_stop` 的单测覆盖，没有端到端断言订单写入量为 0 的集成测试——那需要一个
  带可注入余额的 live 会话夹具（`OrderCommandSender` 私有构造器的老问题）。
- 18 号阶段 5 验收清单里的前三项（背离宽限、超阈值冻结、cleanup 未确认不恢复）仍以
  [ADR 0001](adr/0001-maker-recovery-supervision.md) 与 `market_data.rs` 状态机的既有
  证据复核为准，本轮未新增代码。
- Divergence B/C/D 与自动 flatten / 紧急退出：见"不在本轮范围"。
