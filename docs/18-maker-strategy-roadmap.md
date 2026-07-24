# Maker 策略迭代路线

本文档记录 `standx maker` 在当前 anti-flicker 双边报价策略之上的迭代路线。目标是先建立
可信、可重放的绩效归因，再逐步引入自适应报价、库存控制和订单流信号；任何阶段都不能
以破坏现有 fail-closed 安全语义、订单归属、账本 exactly-once 或输出契约为代价。

路线中的数值门槛是进入下一阶段的工程验收线，不是收益承诺。每个阶段开始前必须冻结
基线 commit、配置哈希、数据集和比较口径；未达到门槛时保留旧策略作为默认行为。

## 总体原则

- `standx-maker` 只接收归一化 typed input，负责纯策略、风险、统计和状态决策。
- `standx-cli` 负责行情/账户 I/O、命令执行、回放文件读取、遥测和用户输出。
- `standx-sdk` 负责交易所 payload、协议、认证和传输健康。
- 新能力默认关闭；关闭时必须与当前策略行为等价。
- 保留已有 JSON action 名称和字段；新增指标只能增加可选字段或新 action。
- WS/REST 成交继续走同一账本入口，以稳定 `trade_id` exactly-once 去重。
- 所有策略比较先经过 deterministic replay 等价验证；统计比较通过小额实盘时间片 A/B
  进行，授权边界见"轨道与执行顺序"的风险预算；超出该边界的 live 变更仍须单独授权。
- 策略、风险控制或交易所命令路径发生变化后，按
  [14-maker-live-gate.md](14-maker-live-gate.md) 重新锁定并补充证据。

## 轨道与执行顺序

阶段 0/1 已完成。阶段 2 v0 跳过独立 L0 长跑，把 renewed live gate、运维件和小额
canary 合并完成后，直接进入 baseline → candidate 的 2 小时时间片 A/B。生产执行仍须先
记录 [19-maker-stage2-live-ab-runbook.md](19-maker-stage2-live-ab-runbook.md) 中的精确授权文本。
其余工作按以下顺序执行：

- **L0 并入阶段 2**：不再先做独立静态策略长跑。按
  [14-maker-live-gate.md](14-maker-live-gate.md) 完成受监督 renewed canary 后，用冻结的
  baseline arm 在交替时间片内积累真实基线数据。
- **安全轨收缩为两级**：小额实盘的前置只保留运维件——emergency cancel 操作人、
  runbook（含 stop-loss 停机后残余仓位的手动处置流程）、webhook 告警可达。阶段 5
  的 typed trim/emergency 分离、自动 flatten、alert/hard floor 配置拆名，全部推迟到
  扩大规模（加大 size/max_position 或多 symbol）之前完成。
- **Alpha 轨：阶段 2 v0 → 阶段 4 v0（漂移感知报价）→ 阶段 3 v0，比较方法为实盘时间片 A/B**：
  基线与 candidate 配置按固定时段交替（阶段 2 v0 固定每臂 2 小时），用阶段 1 已有的
  `run_id/config_hash` 对比能力直接出报告，不再建设 shadow plan-diff 设施。每个阶段的
  策略代码合并后按 gate 规则做一次 canary 重验；之后的纯配置调参（档位阈值、系数）不重锁。
- **阶段 4 启动条件已满足，顺序提前且 v0 重新定义**（2026-07-18）：阶段 2 A/B 的归因分析
  证实被动成交的负向 markout 是主要损耗来源，且 85% 的成交前置于 15–30s 反向（碾过报价
  方向）mark 漂移——漂移是可用 core 已有 mark 序列提前观测的信号。阶段 4 v0 因此提前到
  阶段 3 之前，形态从 microprice 收缩为更薄的 mark 动量驱动非对称报价；阶段 3 v0
  （size skew）治理的库存尾部不是当前主要亏损源，顺延其后。原阶段 4 的 depth/microprice
  内容降级为 v1，是否引入凭 v0 的 A/B 证据决定。
- **阶段 4 终止，阶段 3 立项，顺序回摆**（2026-07-20）：前置的零代码恒宽加宽 A/B
  （8 vs 12bps，3 对 6 臂）按预注册规则判定 live 加宽显著为负——成交率 9–11/h → 2.7–5.7/h，
  mo30 -6.0~-6.7 → -10.4~-15.0，gross 也更低（+0.403 vs +0.327），逐笔净额 -1.86 vs
  -5.04bps。离线反事实给"加宽/漂移条件化"的 credit 被 live 证伪，drift 控制器取消，
  阶段 4 整体回设计储备。同日 534 笔全样本 markout 曲线撤回"出血 ~60s 饱和"的旧读法
  （出血延续至 ~600s），阶段 3 重获候选资格；三项仲裁分析（仓位归因/halt 重叠/退出定价）
  完成后阶段 3 v0 立项，见其章节与
  [maker-stage3-arbitration-2026-07-20.md](evidence/maker-stage3-arbitration-2026-07-20.md)。

**风险预算（小额实盘的授权边界）**：已知最坏路径是趋势市库存满仓后 stop-loss 停机
持仓，损失上界约为 `max_position × 不利变动幅度` 加退出成本。授权边界沿用 canary
口径：单 symbol、一个 level、最小有效数量、`max_position` 不超过一个已批准的 exit
chunk。在此边界内，实盘数据采集不再需要逐阶段的 paper 长跑授权；超出边界（扩大
规模）前必须先完成安全轨第二级。这一预算不替代具体生产动作的精确授权；阶段 2
仍以 runbook 中的授权文本为 gate。

**最简先行**：每个 alpha 阶段先以最简可行模型（v0）走完 replay 等价验证 → 实盘时间
片 A/B 的完整流程和验收，目标是尽快用最少的代码把全链路和证据管道打通。复杂度
（v1 项）只有在 v0 的 A/B 证据明确定位不足时才引入，并作为同阶段的增量候选重新走
同一验收，不新开阶段。v0 未达专项门槛时，先排查参数与数据窗口，再考虑升级模型。

| 阶段 | v0 最简模型 | 显式推迟到 v1 的项 |
|---|---|---|
| 2 | 2–3 档阶梯 spread/refresh（阈值 + 迟滞，与 VolBreaker 同模式） | 连续映射、markout/toxicity 输入、latency 输入 |
| 3 | 仅 size skew（单一加仓侧缩减系数），现有线性 price skew 不动 | 非线性 price skew、level skew、inventory age |
| 4 | 仅 mark 动量驱动非对称报价（漂移侧加宽/中心偏移，阈值 + 迟滞） | depth 归一化、microprice、OFI |

| 步骤 | 轨道 | 主题 | 主要产物 | 晋级核心条件 |
|---|---|---|---|---|
| 0 | 已完成 | 基线与证据校准 | 冻结基线、配置/文档对齐、数据集清单 | 基线可复现，配置与 live 证据无冲突 |
| 1 | 已完成 | 绩效账本与回放 | 净 PnL 归因、markout、订单延迟、时间加权 uptime、replay runner | 同一 trace 确定性重放，指标守恒且可查询 |
| L0 | 已并入 2 | 基线小额实盘数据 | canary 重验记录、运维 runbook、baseline arm 数据集 | 不作为独立长跑阶段 |
| 5-a | 安全轨一级 | 实盘运维件 | emergency cancel 操作人、残余仓位手动处置流程 | 与阶段 2 renewed canary 一并完成 |
| 2 | alpha | 波动驱动 spread / refresh（v0 阶梯） | spread 控制器、时间窗波动 | 时间片 A/B 风险改善，收益/uptime 不越过退化线 |
| 4 | 已终止（07-20） | 漂移感知报价 | 加宽 A/B 判负记录 | 恒宽 live 显著为负，条件化 credit 坍塌，回设计储备 |
| 3 | alpha（当前，已立项 07-20） | 库存控制器（v0 size skew，size=venue min 时退化为加仓侧压制） | 加仓侧数量缩减/压制 | 尾部库存显著下降，退出成本和敞口不恶化 |
| 5-b | 安全轨二级 | 分级异常与退出政策（剩余范围） | trim/emergency typed 分离、flatten（默认关）、配置拆名 | 扩大规模的前置 |

## 统一验收口径

数据窗口不再预先策展：阶段 2 A/B 起小额实盘连续采集，按阶段 0 的分类口径事后标注为平静、
趋势、快速波动或 unclassified。alpha 阶段的晋级比较窗口至少包含一段平静时段和一段
趋势时段，且 A/B 两臂在同类时段的 quote-hours 大致平衡；未覆盖趋势时段前不晋级。

参数只允许在训练窗口调整，验收必须使用冻结参数和未参与调参的样本外窗口。比较报告至少
包含：净 PnL、最大回撤、1s/5s/30s markout、下单/撤单 effective latency、时间加权双边
uptime、合格深度时间积分、成交率、撤单率、`p95 |position|`、高库存持续时间和主动退出
成本。

alpha 轨各阶段还统一遵守：

- **基线继承**：阶段 N 的基线是上一 alpha 阶段 accepted 后的冻结配置（含已启用的自适应
  能力），不是原始静态策略；同时每个阶段必须保留"全部自适应能力关闭 ≡ 原始静态策略"的
  逐 action 等价测试，防止组合状态漂移。
- **证据分工**：deterministic replay 是 open-loop 重放（resting/fills 来自录制事件），
  无法生成 candidate 策略的反事实成交。replay 只承担"关闭时等价"的确定性验证；净
  PnL、markout、撤单率等专项门槛由小额实盘时间片 A/B 承担。paper 降级为合并前的
  冒烟测试（多小时无 panic/不变量违规），不再作为晋级证据。
- **时间片 A/B 规程**：基线与 candidate 按冻结的固定时段交替（阶段 2 v0 每臂 2 小时），两臂使用
  相同的 size/max_position/风控配置，只差被验收的策略参数；每臂每段是独立
  `run_id + config_hash`，直接用阶段 1 的对比查询出报告。轮换切换必须经过正常停机
  （空簿、仓位对账），带着仓位切换配置的时段作废。

除各阶段的专项门槛外，所有阶段还必须满足：

- 无 `max_position`、worst-case pending exposure、账本或 generation 安全不变量违规。
- 无 wrong-run/manual fill 进入 current-run ledger。
- 无 stale generation 结果恢复报价；冻结后 maker book 必须先清空再恢复。
- 现有 JSON contract 兼容，live 默认值和 live gate 默认锁定状态不变。
- 通过仓库标准离线验证：

```bash
HOME=/tmp/standx-test-home CARGO_HOME=~/.cargo cargo test --workspace --offline
cargo clippy --workspace --all-targets --offline -- -D warnings
cargo fmt --all -- --check
python3 -m py_compile scripts/openobserve_dashboard.py
```

## 阶段 0：基线与证据校准

当前执行记录：
[maker-strategy-stage-0-baseline-2026-07-15.md](evidence/maker-strategy-stage-0-baseline-2026-07-15.md)。

### 目标

在不改变交易行为的前提下，建立后续 A/B 比较的唯一基线，消除配置、注释和 live 证据之间
的歧义。

### 范围

- 记录基线 commit、maker 配置文件哈希、symbol/tick 元数据和数据时间窗。
- 对齐 `examples/maker*.toml`、`13-maker.md` 与 live-gate evidence 中的阈值说明。
- 明确区分：告警阈值、fail-safe stop、正常库存退出和是否平仓。
- 盘点已有 paper/live 日志，登记可用于回放的数据字段和缺口。
- 保持通用 `examples/maker.toml` 的主动库存退出默认关闭。

### Trace 分类口径

阶段 0 的市场分类只用于组织数据集，不改变策略阈值。可比较窗口至少包含 300 个连续
`cycle_summary` 且覆盖 600 秒；不足时只能作为身份/采集校准 run。分类按以下优先级执行：

1. 快速波动/压力：`max_vol_bps >= 50`，或存在 halted cycle 且窗口 range ≥ 50bps；
2. 趋势：`|net_move_bps| >= 75`、`|net_move_bps| / range_bps >= 0.7` 且无 halted cycle；
3. 平静：range ≤ 10bps 且 `|net_move_bps| <= 5`；
4. 其余或必需字段缺失：`unclassified`。

其中 range 使用 `(max_mark - min_mark) / min_mark`，net move 使用
`(end_mark / start_mark - 1)`。分类结果必须连同原始值记录，不能只保存标签。

### 验收标准

- [x] 每个用于比较的基线 run 都能追溯到 `git_sha + config_hash + symbol + time range`。
- [x] 同一配置值在 TOML 注释、maker 文档和 live-gate evidence 中不存在冲突。
- [x] 已生产验证与未验证的 inventory-exit tuple 分开标注；未验证 tuple 不宣称可用于 live。
- [x] 文档明确说明 `alert_*` 只通知，`stop_loss` 会 fail-safe 停机但不会自动平仓。
- [x] 形成至少一份平静、一份趋势和一份快速波动 trace 清单；缺失字段有显式记录。
- [x] 不改变报价、退出、PnL、风控和 JSON 行为；标准离线验证全部通过。

## 阶段 1：绩效账本与确定性回放

### 目标

把“策略是否更好”变成可复现、可归因的判断，而不是只看 session 末尾 PnL。此阶段仍不
改变 live 下单决策。

### 范围

- 分离 passive maker fills、reduce-only inventory exits 和外部/错误 run 事件。
- 增加 gross spread、fee/rebate、funding、exit slippage 和 net PnL 归因。
- 增加数量加权 capture，以及成交后 1s/5s/30s markout。
- 分阶段记录 place/cancel 从 intent、socket write、command ack 到 account-order 生效的延迟。
- 将 uptime 从 cycle 占比升级为时间加权双边 uptime；增加合格报价深度 × 时间。
- 新增 deterministic replay runner，读取归一化 market/account trace 驱动纯 planner/ledger。
- 将新增指标接入 JSON、OpenObserve 查询和 dashboard。

### 下单与撤单延迟观测

不能把“本地 socket 写入成功”“交易所接受命令”和“订单已经在账户/盘口状态中生效”合并成
一个延迟。阶段 1 至少记录以下区间：

| 指标 | 起点 | 终点 | 含义 |
|---|---|---|---|
| `place_write_ms` | place intent 生成 | 完整 command frame 写入本地 WS sink | 本地调度、签名、锁和 socket backlog |
| `place_ack_ms` | frame written | 关联 `request_id` 的 accepted/rejected response | 网络与交易所命令处理 |
| `place_effective_ms` | place intent 生成 | account stream 首次观察到该 current-run order | 订单真正进入 live account projection 前的总延迟 |
| `cancel_write_ms` | cancel intent 生成 | 完整 cancel frame 写入本地 WS sink | 本地撤单发送延迟 |
| `cancel_ack_ms` | frame written | 关联 `request_id` 的 cancel accepted/rejected response | 撤单命令确认延迟 |
| `cancel_effective_ms` | cancel intent 生成 | account stream terminal/零 open qty，或审计确认 absent | 真正解除挂单敞口的总延迟 |
| `account_event_lag_ms` | 交易所 `updated_at` / `trade_ts` | 本地收到对应 typed event | 账户流传输和消费延迟 |
| `fill_after_cancel_ms` | cancel intent 生成 | 撤单窗口内到达的关联成交 | 撤单在途期间的实际成交风险 |

观测实现遵守以下边界：

- `standx-cli` 使用单调时钟计算进程内 duration，并同时记录 UTC 时间供跨系统对齐。
- `standx-maker` 的 projection/reducer 不读取时钟、不保存 `Instant`；以后若策略需要延迟状态，
  只接收 CLI 归一化后的 typed latency summary。
- 使用 `request_id + client_order_id/order_id + generation + cycle` 关联生命周期，允许
  account-order 先于 command ack 到达。
- 超时、拒绝、generation invalidation、断流和进程结束都必须产生 terminal/censored outcome；
  不能只统计成功请求，否则延迟分位数会产生 survivor bias。
- 通过新增可选字段或独立 `action="order_latency"` 输出，不修改现有 JSON action/字段语义。
- 阶段 1 只观测和回放延迟，不因延迟阈值改变报价、冻结或恢复行为。

### 验收标准

- [x] 同一 trace、配置和 seed 连续重放三次，结构化 summary 完全一致。
- [x] WS→REST、REST→WS、重复成交、部分成交后撤单均只记账一次。
- [x] passive fill 和 active exit 的数量、现金流、capture/exit cost 可独立汇总。
- [x] `gross spread + inventory MTM change + rebate - fee + signed funding cashflow - exit cost` 与 net PnL 的差异不超过一个计价币最小精度单位；无法换算的费用单列且不静默忽略。
- [x] 1s/5s/30s markout 使用成交时点后的行情；缺失窗口标为 unavailable，不使用回补时的当前 mark 伪造。
- [x] 合成 trace 中时间加权 uptime、深度时间积分和库存持有时间与手工计算一致。
- [x] 100% 已注册 place/cancel request 最终归入 accepted、rejected、effective、timeout、
  invalidated 或 process_ended 之一；没有无解释的 pending request。
- [x] 同一请求的生命周期时间单调、duration 非负，并正确覆盖 account-order 先于 ack、晚到
  ack、拒单、超时、重连和 stale generation。
- [x] place/cancel 分别输出 p50、p95、p99、timeout rate 和 reject rate；超时样本作为
  censored/timeout 保留，不从分布中静默删除。
- [x] `cancel_effective_ms` 能与 `fill_after_cancel_ms`、负向 markout 和仓位跳变按
  `run_id/config_hash/symbol` 关联查询。
- [x] 延迟指标可按正常运行/recovery、symbol、side、level 和 market source 分组，且不会把
  socket write success 误标为 venue accepted。
- [x] replay runner 不访问网络、环境变量、终端或实时时钟，且不会执行任何订单 I/O。
- [x] 旧 JSON 字段语义不变；新增字段在旧消费者缺失时仍可正常工作。
- [x] dashboard 能按 `run_id/config_hash` 对比上述指标，区分 passive fill 与 inventory exit，
  并展示 place/cancel latency 分位数、超时率和撤单后晚到成交。

## 阶段 2：波动驱动的自适应 Spread 与 Refresh（v0 单因子）

当前状态：`ab_completed_not_accepted`（2026-07-18 判定）。代码、冻结配置与自动 A/B 运维件已通过离线验收；
canary 重验通过；2026-07-17T15:23Z → 07-18T16:1xZ 在 HYPE-USD 完成 3 对 4 小时臂的实盘时间片 A/B
（docker,24h+ 连续，6 次 wind-down 换臂全部干净），candidate 未达到下方经济晋级门槛，按规约不晋级。
执行手册见
[19-maker-stage2-live-ab-runbook.md](19-maker-stage2-live-ab-runbook.md)，实现证据见
[maker-strategy-stage-2-v0-implementation-2026-07-16.md](evidence/maker-strategy-stage-2-v0-implementation-2026-07-16.md)，
A/B 全程记录与 markout 分析见
[maker-strategy-stage-2-canary-ab-2026-07-17.md](evidence/maker-strategy-stage-2-canary-ab-2026-07-17.md)
（分析脚本 `scripts/maker_markout_ab.py`）。

A/B 要点（3 对，baseline n=222 / candidate n=112 笔被动成交）：单笔毒性、capture、净边际两臂在噪声内
一致（mo5 -5.51 vs -5.35 bps,mo30 -8.71 vs -9.13 bps）；自适应加宽的唯一实测效果是高波动时段少成交
（tier 激活 14–23% 时间），未转化为 PnL 或 markout 改善。运维门槛（uptime、撤单率）达标，经济门槛未达。
附带策略级发现：当前 8bps / ~2.45s 报价循环在 HYPE 上为结构性负边际（每笔 30s 净边际约 -4~-5bps,
90%+ 成交 5s 内被反向穿越），亏损主因是逆选择与库存盯市而非费率——这是后续迭代（非对称报价 /
requote 提速 / 漂移侧收手）应优先攻击的问题，而非继续调 tier 宽度。2026-07-18 的漂移/账龄归因
分析（见同一 evidence 文档）进一步确认：85% 成交前置于反向漂移、年轻挂单毒性最集中（单纯
requote 提速不受益），阶段 4 v0 据此提前并重定义为漂移感知报价。

运维件现有两条部署路径：systemd（`deploy/systemd/standx-maker-stage2-ab.service`）与容器化
（`deploy/docker/`，docker-compose，同一 runbook 授权门槛）。容器化路径已在 2026-07-16 完成首次
真实部署联调，修复过三处环境问题（builder 工具链 MSRV、OpenObserve alerts API 版本迁移、
host `/run/lock` 挂载权限 → 容器本地锁），细节见 `deploy/docker/README.md` 的 Troubleshooting。
这只解决了部署可用性，不构成 canary 或 live A/B 证据；renewed canary 记录与 A/B 验收仍按本节
标准和 runbook 执行，需操作员自行完成并记录。

### 目标

让报价宽度和重报阈值响应短窗波动，同时保持 SIP-5A band、post-only 和 anti-flicker 约束。

v0 刻意收缩为单因子：只使用核心已有的滚动波动（`VolBreaker` 每 cycle 计算的
peak-to-trough `vol_bps`）和当前 touch spread。markout/toxicity 与滚动 latency summary
输入推迟到 v1（阶段 2.5），仅当 v0 的 A/B 证据表明单因子不足时再引入——两者都需要
新增滚动统计管道，且 markout 依赖自身成交，样本稀疏时噪声大，并引入报价→成交→信号的
反馈回路。费用下限不建 fee 模型，用 `min_spread_bps` 配置项由操作者按已知费率设置。

### 范围

- 新增可关闭的纯函数 spread 控制器，v0 为 2–3 档阶梯模型：每档一组
  `(spread_bps, refresh_bps)`，档位由时间窗 `vol_bps` 的阈值切换，并带升快降慢的
  不对称 hysteresis——与 `VolBreaker` 已验证的"阈值 + 迟滞"模式同构，只是输出从
  halt 变成加宽。`min_spread_bps`/`max_spread_bps` 仍是硬边界；连续映射留给 v1。
  阶梯模型天然离散，逐 tick 变化的问题不存在。
- 将 `VolBreaker` 窗口从"最近 N 个 cycle"改为明确时间窗（按时长驱逐 `(ts, mark)`），
  使 cycle 频率变化不再改变统计周期。
- 接入方式：每 cycle 派生 effective `MakerConfig`（仅调整 `spread_bps`/`refresh_bps`）
  后传入 `plan_cycle`。策略关闭时派生为恒等，逐 action 等价自动成立；band、no-cross、
  tick rounding、exposure cap 均不改动即天然生效。
- 显式政策：spread 变宽时不主动撤已挂的窄单，依靠 refresh 自然轮换与 vol halt 兜底。
  reconcile 按 `(side, level)` + ref_center 漂移持有报价，spread 变化只在自然 re-quote
  边界生效——这是有意保留的 anti-flicker 行为，不是遗漏。
- `run_replay` 接入控制器，用于关闭等价的确定性验证。

### 验收标准

确定性（replay / 单测）：

- [x] 控制器对同一 typed input 始终返回相同结果；关闭时与旧 planner 逐 action 等价。
- [x] 有效 spread 不低于 `min_spread_bps`，不高于 band 可容纳的安全上限。
- [x] planner 回归覆盖不会生成穿 touch、出 band、低于最小数量或突破敞口预算的报价。
- [x] 波动单调恶化时 spread 不收窄；恢复时通过 hysteresis 回落；档位切换在阈值附近无振荡。

实盘时间片 A/B（按统一验收口径；2026-07-17/18 HYPE 3 对 4h 臂实测）：

- [x] 代码合并后完成一次 canary 重验，A/B 在风险预算边界内运行。
- [ ] 比较窗口（含平静与趋势时段）合计 net PnL 不低于静态基线的 95%。
  **未达**:Σ baseline -0.173 vs Σ candidate -0.740 USD（先后时段、行情混淆，但门槛未过）。
- [ ] 最大回撤绝对值不得大于基线；5s 负向 markout 绝对值至少改善 10%，否则不晋级。
  **未达**:mo5 -5.35 vs -5.51 bps(≈3%,噪声内）,mo30 反而略差；candidate 最差臂 PnL -0.280
  大于 baseline 最差 -0.166。
- [x] 时间加权双边 uptime 相对基线下降不超过 3 个百分点。
  （baseline 均值 98.5% / candidate 99.2%,candidate 反而略高。）
- [x] 每 quote-hour 撤单数相对基线增加不超过 20%。
  （baseline 均值约 118 / candidate 约 98，无增加。）

判定：运维门槛通过，经济门槛未过，candidate 不晋级；阶段 2 v0 维持 baseline 配置为生产基线。

## 阶段 3：非线性库存控制

当前状态：`v1_split_skew_ab`（2026-07-24 立项）。v1 组合候选判定
`rejected_split_branch`（2026-07-24，release owner 裁决）：8 臂 32h 实盘
A/B（baseline/candidate 各 4 臂）完成，8 判据 6 达标、撤单率 pair#3
+54% 未达（guard 迟滞切换 churn）、PnL 不可判（无纯趋势窗）；组合不晋级，
按预注册分支拆单——nonlinear_skew 单开快速 A/B（3 对臂目标 / 4 对硬
上限，不重锁）；guard 阈值按反事实回放证据冻结 enter=10/exit=5 备后续
重启，本轮不跑。判定报告与反事实数据见
[maker-stage3v1-ab-judgment-2026-07-24.md](evidence/maker-stage3v1-ab-judgment-2026-07-24.md)。
v0 判定 `rejected_uptime_cost`（2026-07-22）：6 臂实盘 A/B 完成，尾部治理
达标（p95 |position| 降 40–62%、≥70% 仓时间清零、退出成本未恶化），但
二值加仓侧压制使双边 uptime 降 43–80pp，按预注册判据不晋级；
baseline 维持 HYPE 静态配置。判定报告见
[maker-stage3-ab-judgment-2026-07-22.md](evidence/maker-stage3-ab-judgment-2026-07-22.md)。

**v1 组合候选（release owner 2026-07-22 裁决）**：非线性 price skew（"更陡但不停"）与
外部价防御门（HL midPx 领先信号压制危险侧，lag 证据见
[lag-recorder-hype-result-2026-07-22.md](evidence/lag-recorder-hype-result-2026-07-22.md)）
**合并为一个 release、各带独立 enable 开关**，一次 canary 重锁，A/B 两臂
（baseline vs 双开组合）+ 遥测事件级归因；组合被拒时拆单机制以纯配置 A/B 重跑（不重锁）。
**uptime 判据修订为绝对值 ≥80%**（替代原 ≤3pp 相对判据，release owner 2026-07-22）。
设计、红线（SIP-5A 带内 cap、无迟滞、guard fail-open）与预注册判据全文见
[22-maker-stage3v1-guard-design.md](22-maker-stage3v1-guard-design.md)；
canary 与 A/B 执行手册见
[23-maker-stage3v1-live-ab-runbook.md](23-maker-stage3v1-live-ab-runbook.md)
（2026-07-22 冻结 commit 45311e7、配置对与授权文本）。

**Round 1 → Round 2（2026-07-23 release owner 决定）**：Round 1（enter=6/exit=3）
8 臂中期读数——非线性 skew 达标（p95 |pos| 降 50–67%、零 uptime 代价，判定为
干净赢家）；防御门方向对但剂量超标（激活率 11.5–22.6%，是跳变预算的 5–10 倍，
uptime 贴 80% 线、撤单率触边）。三症状同源=guard 开门过勤。处置：直接把
`enter=10 / exit=5` 固定为新 base（不再单独验证阈值，纯配置变更不重锁），下一轮
A/B 用此配置对；lag 分层证据支持——防御价值集中在 ≥16bps 大跳。skew 参数不动。
立项依据是三项仲裁分析（610 笔 fill / 81 个库存事件 / 16 笔实测退出成本），记录见
[maker-stage3-arbitration-2026-07-20.md](evidence/maker-stage3-arbitration-2026-07-20.md)，
分析脚本 `scripts/maker_tail_arbitration.py`。

### 目标

把库存治理从“线性移动整个报价中心 + 达阈值市价退出”升级为价格、数量和档位共同作用的
控制器，减少高库存持续时间和被动积累后的退出成本。

数据定位（2026-07-20，取代 07-18 基于 334 笔子样本的"出血 ~60s 饱和"旧读法——全样本
534 笔下该读法不成立，已撤回）：成交后出血延续至 ~600s（mo30 -8.8 → mo300 -11.0 →
mo600 -13.6bps，neg% 92→66）；mo300 损失质量的 51% 集中在最差 10% 成交（均值 -56bps）。
仲裁分析进一步显示尾部的固定剧本是**"第一刀 + 逆势加仓"**：毒性成交后 +300s 内仓位
中位从 0.3 翻倍到 0.6——策略在出血方向继续进货。本阶段 v0 治理的正是这个放大段；
第一刀（尾部的 13% 首笔即挨刀）不在任何库存机制的射程内，预期是削尾部、压 p95 仓位，
不是转正。现有线性 price skew（`skew_bps=8`）在全部实验数据中始终开启，2–5bps 的中心
偏移被证明挡不住趋势碾压，故 v0 采用其极限形式（见范围）。

### 范围

- 基线为静态 baseline 冻结配置（阶段 2、恒宽加宽与阶段 4 均未 accepted，按基线继承
  规则落在 HYPE 静态 baseline：8bps、size 0.1、adaptive_spread 关闭）。
- v0 只做 size skew：`|position|` 超过配置阈值后，按单一系数缩减加仓侧数量；缩减后
  必须 tick 对齐，低于 venue minimum 时丢弃该档而不是提交非法数量。现有线性 price
  skew（`skew_bps`）保持不变。
- **退化形态（2026-07-20 立项确认）**：当前 HYPE 配置 `size=0.1` 恰为 venue
  `min_order_qty`，任何缩减都触发"低于 minimum → 丢弃该档"，v0 在此规模下退化为
  二值的**加仓侧压制**（`|position| ≥ 阈值 → 加仓侧不挂单`）。这与"少亏"目标一致
  （被压掉的是负期望的逆势加仓单），代价是压制期间双边 uptime 下降——由 A/B 的
  uptime 门槛（-3pp）实测裁决，不预先豁免。实现上等价于把 core 已有的临近
  `max_position` 加仓侧压制边界（`SideSuppressed` 路径）提前到阈值处。
- **v0 参数（训练窗选定，A/B 冻结）**：激活阈值 `0.3 × max_position`，恢复迟滞
  `0.2 × max_position`（与 VolBreaker 同款升快降慢模式，防阈值振荡）。依据：79% 的
  尾部损失发生在半仓以下、尾部 pos_pre 中位 0.3、放大段为 0.3→0.6，阈值定 0.5 会
  漏掉大半放大段。参数仅由 2026-07 的 610 笔训练窗数据选定。
- **明确不做（本轮仲裁判死，不再回锅）**：drawdown 触发的主动退出——离线定价训练窗
  最优 +52.1 bps·qty，按 45 个独立行情事件切分的样本外全部转负（-15.6），实测 taker
  成本中位 3.72bps；触发后路径样本外无预测力。v0 不含任何主动退出变更。
- **无 5-b 政策前置（仲裁 B 实测）**：尾部成交 0% 发生在 halted 状态，+300s 窗口
  熔断占比中位 0%，vol_pause 未参与尾部事件。halt 期间退出语义仍按原计划在扩大
  规模前由安全轨二级定稿，本阶段不触碰。
- v1（凭 v0 的 A/B 证据引入）：price skew 靠近上限时的非线性增强、level
  skew（高库存减少加仓侧档数）、inventory age（长期未回中逐步提高减仓强度；所需
  时间由 CLI 归一化后作为 typed input 传入，core 保持不读时钟的既有边界）。若 v0
  的二值压制被 A/B 证明过于粗暴（uptime 代价大/恢复抖动），v1 优先考虑"更陡但不停"
  的非线性 price skew 中间档。
- 主动退出继续是独立策略；vol halt 期间的退出语义在扩大规模前由安全轨二级定稿，
  本阶段不隐式改变市价退出或波动熔断行为。
- 统计证据通过实盘时间片 A/B 积累（同阶段 2 规程，wind-down 换臂，4h 臂，比较窗口
  须含平静与趋势时段）。

### 验收标准

- [ ] 控制器关闭时，价格、数量、档位和 action 顺序与旧策略等价。
- [ ] long/short 完全对称；零库存不产生 skew；满仓及越界输入正确饱和。
- [ ] 当前仓位、所有 resting quote 和 pending place 全部同侧成交后仍不突破 `max_position`（半个 qty tick 容差）。
- [ ] 数量始终 tick-aligned 且不低于 venue minimum；无法安全缩量时丢弃该档而不是提交非法数量。
- [ ] 样本外 `p95 |position|` 至少下降 15%，或处于 `|position| >= 70% max_position` 的时间至少下降 25%。
- [ ] 主动退出次数和总 taker exit cost 均不得高于基线；若其中一项增加，阶段不晋级。
- [ ] net PnL 不低于基线的 95%，时间加权双边 uptime 下降不超过 3 个百分点。
- [ ] 覆盖数量 tick 边界、方向翻转、阈值跨越、部分成交、pending reservation 和 wrong-run 事件测试。

## 阶段 4：漂移感知报价与公平价信号

当前状态：`terminated_design_reserve`（2026-07-20 判定）。前置的零代码恒宽加宽 A/B
（8 vs 12bps，3 对 6 臂，三项指标区间互不重叠）按预注册规则判定 live 加宽显著为负：
多收的 +3.3bps capture 被翻倍的毒性（mo30 -10.4~-15.0 vs -6.0~-6.7）吃掉，gross 亦
更低。离线反事实给漂移条件化的全部 credit 随之坍塌（无条件加宽即其纯测量），drift
控制器不再实现，本阶段整体回设计储备；重启须以新的独立证据立项。以下内容保留为
历史设计记录。

启动条件已于 2026-07-18 满足：阶段 2 A/B 的归因分析证实被动成交的负向 markout 是主要
损耗来源（每笔 30s 净边际约 -4~-5bps），且 85% 的成交前置于 15–30s 反向（碾过报价方向）
mark 漂移，账龄 <15s 的年轻挂单成交毒性最集中；进一步的三类归属显示 91.9% 的毒性成交
（92.8% 的负向 mo30 质量）打在策略仍持有的新鲜挂单上，stale 未撤与撤单在途合计仅
3.6%——撤单/检测提速工程的上限被定量排除，问题在报价中心本身不含短期漂移信息，而
漂移是可用 core 已有 mark 序列提前观测的信号。v0 因此提前到阶段 3 之前执行，形态收缩为
mark 动量驱动的非对称报价，比原 microprice 设计更薄：不需要 depth_book 管道，完整复用
阶段 2 的控制器接入模式（每 cycle 派生 effective config → `plan_cycle`）与全部 A/B 运维件。
归因细节见
[maker-strategy-stage-2-canary-ab-2026-07-17.md](evidence/maker-strategy-stage-2-canary-ab-2026-07-17.md)。

### 目标

在不脱离 mark eligibility band 的前提下，用短窗 mark 漂移修正被碾侧报价（加宽或中心
偏移），减少报价被漂移碾过后的负向 markout。

### 范围

- 基线为静态 baseline 配置（阶段 2 未 accepted，按基线继承规则阶段 4 v0 直接落在
  原始静态策略之上）。
- v0 只做 mark 动量：使用 core 已有的时间窗 `(ts, mark)` 序列（阶段 2 已为 VolBreaker
  建好）计算短窗 signed drift（离线信号定价显示 T=30–60s 优于 15s，见下），按 2–3 档
  阶梯 + 不对称迟滞（与阶段 2 控制器同构）派生 effective config：漂移越强，被碾侧报价
  离 mark 越远（中心前倾 k·drift + cap 截断）；反向侧不变。v0 不调 refresh/cycle 参数——
  账龄分析显示年轻单最毒，单纯提速反而增加最毒的年轻成交占比；refresh 调参如需引入，
  只作为 v0 内部的互补手段重新走同一验收。
- 离线信号定价（2026-07-18，train/val 严格切分，见 evidence 归因补充）给 v0 两条硬
  约束：其一，drift 逐笔判别力弱（r≈0.1），条件前倾在 open-loop 记账下不优于无条件
  加宽；其二，初始参数取训练窗调参区 T=30–60s、k≈1.0–1.5、cap=8bps，后续调参只准
  在训练窗口进行，验收用冻结参数 + 样本外窗口（统一验收口径）。执行因此拆成两步：
  **第一步是零代码加宽 A/B（8 vs 12bps 恒宽，两臂，纯配置不重锁 gate，复用 stage2
  harness），直接测量离线记账偏置**——若 live 加宽 ≈ 0（阶段 2 tier 数据的预示），
  漂移前倾的离线 credit 整体坍塌，控制器投入（实现 + replay 等价 + canary 重锁 +
  三臂 A/B）取消，本阶段回到设计储备；若显著为正，加宽按纯配置调参晋级并入基线。
  **第二步（仅当第一步支持）：建 drift 控制器，三臂 A/B（baseline / 恒宽 / 漂移）**。
  第一步的授权与执行记录见
  [maker-stage4-wide12-ab-2026-07-18.md](evidence/maker-stage4-wide12-ab-2026-07-18.md)。
- 派生仅调整 spread/skew 类参数，band、no-cross、tick rounding、exposure cap 不改动；
  关闭时派生为恒等，逐 action 等价自动成立。新增 drift/档位指标只能以可选字段或新
  action 进入 JSON 输出。
- v1（凭 v0 证据引入）：depth 归一化、top-of-book microprice（SDK 已有 `depth_book`
  频道与 `OrderBook` 模型）、多档深度、OFI；届时再决定是否补建 plan-diff shadow。
- 统计证据通过实盘时间片 A/B 积累（同阶段 2 规程，含 wind-down 换臂）。

### 验收标准

- [ ] 控制器对同一 typed input 始终返回相同结果；关闭时与静态 baseline 逐 action 等价。
- [ ] drift 只由 core 已有 mark 序列计算，不引入松散 JSON/SDK payload；缺窗、乱序、
  非有限值安全降级为恒等派生。
- [ ] 报价始终受 mark band、no-cross、tick 和 exposure cap 约束；漂移单调恶化时被碾侧
  不收窄，恢复时经 hysteresis 回落，阈值附近无振荡。
- [ ] 双胜门槛：以下全部经济门槛对 baseline 臂和恒宽臂分别成立（同一比较窗口）；
  仅胜过 baseline 不晋级。零代码加宽 A/B 未支持控制器立项时，本组标准不启动。
- [ ] 冻结参数的样本外窗口中，passive fills 的 5s 负向 markout 绝对值至少改善 10%。
- [ ] net PnL 不低于基线的 95%，最大回撤和高库存时间均不得恶化。
- [ ] 时间加权双边 uptime 下降不超过 3 个百分点，每 quote-hour 撤单数增加不超过 20%。
- [ ] 改善不能只来自大幅减少成交：成交数量下降超过 20% 时必须单独评审 SIP-5A 收益/uptime 影响。

## 阶段 5（安全轨，分两级执行）：分级异常与退出政策

### 目标

明确区分短暂数据不一致、持续行情异常、正常库存修剪和紧急风险处置，避免“坏数据时永久
保留挂单”或“波动最大时只能停机持仓”的隐含政策。

本阶段拆为两级：一级是小额实盘（L0）的运维前置，随 L0 canary 一并完成；二级是扩大
规模（加大 size/max_position 或多 symbol）的代码级前置。canary 重验由 L0 承担，本阶段
验收中涉及 canary 的条款以 L0 的记录为准。已知最大风险（趋势库存 + stop-loss 只停机
不平仓）在小额边界内由风险预算覆盖，在扩大规模前由二级修复。

### 现状（2026-07-16 对照 main）

背离分级的主体已经落地，本阶段不重复建设，对应验收项的工作方式是证据复核而非新实现：

- 短暂 hold / 持续 freeze / 恢复对账：`standx-maker/src/market_data.rs` 已实现
  grace（连续 3 次不健康观察且持续 15s）→ Degraded/Paused → 连续 coherent 快照
  确认恢复的状态机，并区分 market-state 与 transport 故障类别。
- 冻结后的清理、空簿对账、恢复入场与恢复熔断计量已在 recovery flow 中实现。
- stop-loss 生命周期已重构为经由 core reducer 的统一路径。

### 剩余范围（分两级）

一级（小额实盘前置，运维件而非代码，随 L0 canary 一并完成）：

- 指定 emergency cancel 操作人；形成 runbook：stop-loss 停机后残余仓位的手动处置
  流程、告警响应时限、venue 侧手动撤单路径。
- 验证 webhook 告警（stop-loss、position risk、equity/margin alert）实际送达。

二级（扩大规模的代码级前置）：

- 正常 inventory trim 与 emergency risk exit 使用不同 typed policy/effect。
- 明确 volatility halt 期间是否允许紧急退出；默认不得自动继承正常退出行为。该决定
  须在阶段 3 v1（非线性控制器）前定稿；v0 size skew 不触碰退出语义，不受此阻塞。
- stop-loss 后残余仓位输出明确 handoff；自动 flatten 必须是默认关闭、单独授权的 live policy。
- equity/margin 的 alert 与 hard floor 使用不同配置名和不同 typed outcome。
- 背离恢复迟滞、熔断豁免等剩余硬化项按需纳入。

### 验收标准

前三项针对已落地行为，验收方式为复核并在 evidence 文档中补引用，不要求新代码：

- [ ] 短暂背离在配置宽限期内保留当前兼容行为，不盲目 cancel/re-place。
- [ ] 背离超过持续时间或严重度阈值后，generation 失效、placement 冻结、queued action 清空并安排 maker cleanup。
- [ ] cleanup 未确认空簿、required stream 不健康或仓位未与 ledger 对齐时绝不恢复报价。
- [ ] 正常 trim、emergency exit、hard stop 和 residual-position handoff 在类型、日志和 JSON action 上可区分。
- [ ] vol halt + 高库存、stop-loss + 残余仓位、退出部分成交、退出未确认、退出拒单和 cleanup residual 均有确定性测试。
- [ ] emergency flatten 默认关闭；未显式授权时，测试和运行路径只能告警、清理 maker 单并交接残余仓位。
- [ ] live canary 使用一个 symbol、最小有效数量、一个 level 和明确的 emergency cancel 操作人。
- [ ] canary 证据包含 create/cancel/exit request correlation、trade IDs、空 maker book、最终仓位和 webhook 送达结果。
- [ ] 只有 release owner 审核证据后才能解锁；一次 canary 不授权连续 live maker 或其他退出 tuple。

## 阶段状态记录模板

每个阶段进入实施时，在对应 PR/issue 或 evidence 文档中填写：

```text
阶段：
状态：planned | implementing | replay | canary | live_ab | accepted | rejected
baseline_git_sha：
candidate_git_sha：
baseline_config_hash：
candidate_config_hash：
训练数据窗口：
样本外验收窗口：
专项指标结果：
统一安全门槛结果：
离线验证结果：
live 授权范围（如无则写 none）：
release owner 决定：
```

只有所有必选验收项都有可复查证据时，阶段状态才能改为 `accepted`。收益指标改善但安全门槛
失败时必须判定为 `rejected`；安全通过但收益/风险没有达到专项门槛时继续保持 A/B 观察，
不能仅凭主观观察晋级。
