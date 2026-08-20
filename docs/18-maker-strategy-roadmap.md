# Maker 当前状态与迭代路线

本文档是 `standx maker` 的**当前状态真源**：记录已经生效的机制、已关闭的方向、仍待
裁决的候选和下一步优先级。历史阶段的完整设计、运行手册和路线图快照已移入
[archive/](archive/README.md)；历史文档不能作为新的 live 授权。

策略与安全边界以当前代码、[13-maker.md](13-maker.md)、
[14-maker-live-gate.md](14-maker-live-gate.md) 和
[28-experiment-protocol.md](28-experiment-protocol.md) 为准。任何策略、风险或交易所命令路径
变化后，都必须重新锁定 live gate，并按实验规程登记立项、判据、启动记录和判定报告。

## 当前结论

- **工程状态：live-capable；策略状态：收益未验证。** HYPE-USD 冻结基线已有两轮独立的
  约 36 小时 live 读数，均为负；在完整净收益读数转正之前不扩大 size、
  `max_position` 或 symbol 数量。
- **当前冻结 HYPE 基线**启用 `nonlinear_skew(boost=3, cap=12bps)` 与
  `external_guard(enter=10bps, exit=5bps)`；两者分别于 2026-07-25 和 2026-07-27
  判定 accepted，但当时的预注册判据没有把 PnL 作为晋级条件。
- **`external_skew` 已实现但默认关闭。** 当前只有冻结候选配置，没有 accepted 判定；是否
  使用 live 时间片验证，仍需 release owner 独立裁决和新的精确授权。
- **主要损耗是逆向选择。** 两轮数据中，spread capture 无法覆盖成交后 markout 与手续费；
  即使剔除最差 10% 成交，其余 90% 的单笔经济性仍约为负。
- **当前瓶颈是诊断数据，不是缺少机制。** 下一优先级是纯观测的盘口深度、成交方向和
  成交前后快照；这些字段不得进入决策路径，且必须保持 replay action 序列不变。

完整收益口径与数值见 [`standx-maker` README](../crates/standx-maker/README.md)，原始判定和
采集记录见 [evidence/](evidence/)；本文只保留仍影响当前决策的结论。

## 阶段与机制判定

| 项目 | 当前状态 | 当前含义 |
|---|---|---|
| 阶段 0：基线校准 | completed | 基线、配置哈希、数据分类和安全口径已经建立 |
| 阶段 1：账本与回放 | completed | current-run 账本、PnL 归因、markout、延迟、uptime 和 deterministic replay 已落地 |
| 阶段 2：adaptive spread | not accepted | 未进入冻结基线；不能因代码仍存在就宣称策略有效 |
| 阶段 3 v0：size skew | rejected | 最小数量下退化为二值压侧，uptime 代价越过红线 |
| 阶段 4：恒宽/漂移感知报价 | terminated | 8→12bps live 对照显著降低成交并恶化逐笔净额 |
| `nonlinear_skew` | accepted | 更陡但不停报，已进入冻结 HYPE 基线；PnL 仍是未判项 |
| `external_guard` | accepted | 防御侧抑制已进入冻结 HYPE 基线；保持 fail-open 信号语义 |
| 阶段 5-b：退出与账户风险 | implemented | typed trim/wind-down、残余仓位交接和默认关闭的账户硬熔断已落地 |
| cleanup 残余判定硬化 | completed | WS 终态 success 优先、按单 REST status 兜底，未确认时继续 fail-closed |
| `external_skew` | implemented, pending verdict | 默认关闭、关闭时等价；候选尚未获得 accepted 判定 |
| `micro_price` | accepted（方向）/ 幅度未判 | A/B 于 2026-08-19 判 accepted 并提为默认基线配置（`52b0bea`）；判定成立于 band=40，band=30 下偏移会被 clamp 截断，**效果量需新窗口重测**（见 [30](30-maker-uptime-band-tightening-design.md)） |

历史设计、运行手册与判定链见
[2026-07 Maker 归档](archive/2026-07-maker/)。归档中的精确授权文本只证明当时获准的
symbol、敞口、配置和时间窗，不能复用于新的 live 运行。

## 已固化的经济与评审口径

两轮独立 live 读数均为负：run1 约 35.9 小时，完整净 PnL 为 `-1.8591 DUSD`；另一轮
36 小时中途切片约为 `-1.66 DUSD`。这些读数只证明当前冻结配置在对应规模、费率和市场窗口
下亏损，不外推为其他 symbol 或规模的结论。

当前归因的关键事实：

1. 逆选在 30 秒附近已经解释主要亏损，不能再用“长尾尚未显现”为新增机制辩护。
2. 最差 10% 成交承担约 60–72% markout 损失，但其余 90% 的
   `capture 2.8 - markout 3.2 - fee 1 ≈ -1.4bps/笔` 仍为负。
3. 任意压侧、冷却或成交抑制机制在平均每笔都亏损的基线上都会显得“赚钱”。评审必须先扣除
   **等量随机压单基线**，只把超出随机基线的部分视为信息价值；否则机制的极限只是停止做市。
4. uptime 是硬约束。一次高频、持续 60 秒的单边压制即可耗尽约 2pp 预算，不能用牺牲
   合格双边报价时间来制造表面收益。
5. `performance.markout_*` 从成交价起算并包含 capture；离线分析常从成交时 mark 起算，
   不包含 capture。两种口径不得混用。

因此，在收益读数转正前：

- 不扩大 size、`max_position` 或 symbol 数量；
- 不把 SIP-5A 奖励、返佣或未验证的未来规模收益提前计入 PnL；
- `net_pnl_complete=false` 的读数不能用于扩规模判断；
- 新的抑制类候选必须同时报告原始效果和扣除随机压单后的信息价值。

## 当前优先级

1. **补齐纯观测遥测**：记录成交前后的 best bid/ask 数量、可得的前几档深度和 taker
   方向；新增字段只用于日志和离线分析。
2. **重放 `post_suppressed` 分支**：它使用 maker 自有库存帽状态，不依赖迟到的成交结果
   标签；先用 replay 证明不会把一种毒性换成另一种，再考虑立项。
3. **核对费率与返佣事实**：确认当前账户、symbol 和规模的真实 maker fee/rebate，不能用
   假设值补齐经济缺口。
4. **裁决 `external_skew`**：如决定继续，使用
   [29-maker-external-skew-design.md](29-maker-external-skew-design.md) 的冻结单候选和预注册
   判据，并重新记录授权；未裁决前保持默认关闭。

盘口/流特征数据未积累到足够窗口前，不启动 OFI 或新的自动暂停机制。microprice 已实现
并于 2026-08-19 判 accepted（方向），但其幅度在当前 band=30 下未判；纯观测遥测见
[32](32-maker-observation-telemetry-design.md)。

## 长期不变量

- `standx-maker` 只接收归一化 typed input，负责纯策略、风险、账本和状态决策；
  `standx-cli` 执行 I/O、live gate、交易所命令、遥测和输出；`standx-sdk` 负责协议、认证、
  payload 和传输健康。
- 新机制默认关闭；关闭时必须与旧路径逐 action 等价。保持现有 JSON action 和字段兼容。
- WS/REST 成交进入同一 current-run 账本，并以稳定 `trade_id` exactly-once 去重。
- replay 证明确定性和关闭等价，不生成反事实成交；收益和 markout 晋级结论仍需预注册的
  小额 live 时间片证据。
- account-stream 丢失、仓位无法解释、对账超时或残余 maker 订单必须 fail closed。
- 冻结会失效当前 generation、阻止新 placement、终止可取消的在途工作并安排 cleanup；
  maker book 未清空、仓位未对账或流未恢复健康时不得恢复报价。

## 当前安全政策

- `alert_*` 只通知；`stop_loss` 和已武装的 `stop_*` 会 fail-safe 停机，但都不会自动平仓。
- `stop_equity_below` / `stop_margin_below` 默认 `0`（关闭）。启用后，余额缺失、不可解析或
  超过 freshness 预算时同样停机，不能用旧快照当作“未破线”。
- `InventoryTrim` 与 `WindDown` 是两个正常退出来源；不存在 volatility halt 期间的紧急
  自动退出策略，halt 时退出被显式记录为 suppressed。
- 残余仓位在 cleanup 后以场馆 REST 仓位为权威：确认非零为 `handoff`，读取失败、非有限值
  或与账本不一致为 `unknown`。**无法确认空仓不等于空仓。**
- cleanup 优先等待相关 WS order-response 的终态 success；不可用、超时或仅有 gateway
  accepted 时按 `order_id` 查询终态。open-orders 列表可以辅助发现订单，但不能单独证明某单
  已撤或仍残留；所有路径都无法确认时保持 fail-closed。

运行与参数细节见 [13-maker.md](13-maker.md)，实盘解锁与 cleanup 证据要求见
[14-maker-live-gate.md](14-maker-live-gate.md)，当前受控 A/B 基础操作手册见
[19-maker-stage2-live-ab-runbook.md](19-maker-stage2-live-ab-runbook.md)，绝对 PnL 采集规则见
[27-maker-baseline-pnl-collection-runbook.md](27-maker-baseline-pnl-collection-runbook.md)。

## 变更与验证要求

每次更新本页状态时，同时更新对应 evidence、候选设计状态和 `docs/README.md`。Maker 策略、
安全或 live 路径改动交付前至少运行：

```bash
HOME=/tmp/standx-test-home CARGO_HOME=~/.cargo cargo test --workspace --offline
cargo clippy --workspace --all-targets --offline -- -D warnings
cargo fmt --all -- --check
python3 -m py_compile scripts/openobserve_dashboard.py
```

策略参数、机制开关、报价或退出行为等会影响成交结果的变更，另外遵守
[28-experiment-protocol.md](28-experiment-protocol.md)。
