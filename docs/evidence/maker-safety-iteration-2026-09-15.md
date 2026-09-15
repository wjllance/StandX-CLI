# Maker 安全迭代：可成交敞口与止损顺序

日期：2026-09-15。基线：`main` 的 `67df41c`。

## 本轮目标与边界

修复两个可离线复现的安全问题：多层 size skew 与保留旧单组合越仓；既有 session stop-loss 晚于新增订单和通知。交付工作区修改、组合测试、mutation 验证和独立对抗性复审。

保持报价公式、策略参数、gross session PnL 口径、止损阈值、JSON action/字段和实盘授权门禁。没有新增策略开关，没有发布、提交、推送或执行实盘订单/减仓。

## 复现与修正

### 1. 保留旧单不能按缩小后的目标数量占预算

基线已有：`size=.02, max_position=.05, levels=3`，初始同侧两单各 `.02`；内层成交 `.02` 后 size skew 将该侧目标缩至 `.01`。原 cap 给三层各预算 `.01`，reconcile 却保留中层旧单 `.02` 并新放两单各 `.01`，最坏仓位 `.06 > .05`。

修正：planner 每个槽位按 `max(目标量, 现存量之和)` 分配预算；CLI 执行前再次调用 core 的 `ExecutableExposure`，按实际在场单、待确认新单和待撤单检查每个新增订单，并为本轮已允许的订单立即占用预算。买卖两侧分别检查，使用原数量 tick 容差。

代价：取消尚未确认时可能少补一层；不会靠“发送了 cancel”假设其已不能成交。部分成交、双向镜像、tick 边界、非法预算、顺序和 pending place 均有测试。

### 2. gateway accepted 不等于取消完成

独立复审发现的基线问题：仓位 `.02`、待撤买单 `.03`、上限 `.05`；收到 gateway accepted 后原 `CancelResolved` 删除 pending，允许新买 `.02`，新旧单同时可成交时最坏仓位 `.07`。SDK `OrderResponse::is_success` 的两帧协议说明与这一复现一致。

修正：`CancelResolved` 仅结束 ack 等待，保留 venue slot 和数量预留。账号流终态或已验证清理才释放。重复成功或 cancel/fill race 的后续拒绝仍按既有相关性规则幂等处理，不释放预留。缺少终态时，现有 request deadline 继续处于 `AccountOrder` 阶段，超时走 fail-closed 恢复。

没有新增独立缓存或无限期墓碑；复用现有 pending 生命周期。待撤量保守保持快照值，不直接按 `TradeApplied` 扣减，避免 WS partial 与 trade 对同一成交重复释放预算。

### 3. 止损先阻止订单，冻结后清理，再通知

基线已有：`finish_cycle` 在下单后、普通告警等待后检查止损；critical webhook 又先于 `StopRequested` 和 shutdown cleanup。

修正：core 提供 typed `SessionStopLoss` 判定；cycle 开始即检查已知亏损，REST/paper 成交入账后再次检查。新发现的止损保留本轮 fill 输出、统计与 cycle summary，但清空所有 maker action 并禁用本轮 inventory exit。runtime 立即使 generation 失效，进入 shutdown；先尝试清理 maker 订单，再等待既有有界 webhook 重试。止损不自动平仓，残余仓位沿既有 handoff 路径报告。

止损继续采用 `stats.pnl = cash + position * mark`，阈值为 `pnl <= -limit`，`limit=0` 关闭；本轮不改成 net PnL。

## 独立对抗性复审

由未编写实现的 `iteration_safety_review` agent 执行，要求找可复现的高严重性缺陷。

- 确认并补修上面的 gateway ack 预算释放问题（也存在于基线）。
- 指出迭代中提前返回会漏掉已入账 fill 的输出及累计数；改为走完整成交观测路径，并测试触发止损的一笔成交保留计数。
- 指出局部 `inventory_exit` 在清理 plan 前已复制，可能仍然执行退出；将止损条件直接放在执行所用局部 exit 的生成处，并测试同时触发止损和库存退出时仓位不被减仓。

- 最后复核发现，止损直接退出可能丢弃已从账号流接收的 Order。具体路径是 trade 先到、尚无 ownership 被 ledger 暂存，随后 Order 在 cycle await 期间进入 buffer；此时止损退出会遗漏这笔真实 trade。增加确定性红测试（累计成交预期 1、实为 0）后，修正为同步 freeze，随后通过原 `apply_account_event` 记账这些已收事件，累计 fills，不等待通知，再退出清理。重复 Order/Trade 仍只记一次。
- mutation 曾证明纸面测试不能覆盖 live 库存退出：纸面模式本就禁用 active exit。已补实际 CLI live 路径的本地 REST mock 测试，position 先于 trade，由 REST 回补触发止损；移除 stop-loss exit gate 后测试失败。

最终独立复核确认上述四项均关闭，当前实现未再发现可复现的 P1/P2。尚未从 receiver 消费的到达事件继续遵循既有 shutdown 边界；清理后读取 venue position 完成残仓交接，不把 session ledger 当作最终 venue 仓位。

## 离线验证

最终恢复源码后运行：

| 检查 | 结果 |
|---|---|
| `cargo test --workspace --offline --quiet` | 654 passed，0 failed，1 ignored（原有 `no_run` 文档示例） |
| `cargo clippy --workspace --all-targets --offline -- -D warnings` | 通过 |
| `cargo fmt --all -- --check` | 通过 |
| `python3 -m py_compile scripts/openobserve_dashboard.py` | 通过；bytecode cache 指向临时目录 |
| `git diff --check` | 通过 |
| 安全测试 mutation | 15/15 被测试检出；所有临时破坏已恢复 |

测试环境说明：完整测试需要本机 HTTP/WS mock 监听，默认工具沙箱曾报 `Operation not permitted`，已在允许本机监听的执行环境重跑。SDK 的原有 `test_from_env_missing` 会尝试删除凭据，本轮没有改动该测试，也没有改变 `HOME`；完整测试外加 macOS 文件沙箱，禁止读写真实 StandX 凭据目录，全部测试仍参与。Python 用 `PYTHONPYCACHEPREFIX` 指向临时目录以避免默认缓存目录的写权限问题。没有调用实盘验证。

Clippy 仍显示原依赖 `proc-macro-error2 v2.0.1` 的 future-incompatibility 提示，命令退出码为 0。

### Mutation 记录

每项临时破坏都要求对应测试实际失败（不是编译失败），随后恢复原实现。最后再次运行全工作区测试。

| Mutation | 故意破坏的行为 | 结果 |
|---|---|---|
| M01 | 按目标量代替保留旧单量 | 测试失败，恢复后通过 |
| M02 | 不预留本轮已允许新单 | 测试失败，恢复后通过 |
| M03 | 忽略 pending place 数量 | 测试失败，恢复后通过 |
| M04 | gateway ack 释放 cancel slot | 测试失败，恢复后通过 |
| M05 | 止损阈值从包含等号改为不含等号 | 测试失败，恢复后通过 |
| M06 | 跳过 cycle 最初的已知亏损检查 | 测试失败，恢复后通过 |
| M07 | 止损后仍执行 maker actions | 测试失败，恢复后通过 |
| M08 | 止损后仍执行 live 库存退出 | 测试失败，恢复后通过 |
| M09 | 丢掉触发止损成交的 cycle 计数 | 测试失败，恢复后通过 |
| M10 | 绕过执行前敞口上限 | 测试失败，恢复后通过 |
| M11 | 意外启用 limit=0 的止损 | 测试失败，恢复后通过 |
| M12 | 省略 generation 失效 | 测试失败，恢复后通过 |
| M13 | 把 webhook 移到清理之前 | 测试失败，恢复后通过 |
| M14 | ack 后移除撤单的终态等待/超时 | 测试失败，恢复后通过 |
| M15 | 丢弃已接收的 ownership 事件 | 测试失败，恢复后通过 |

## 实盘与经济效果未判项

此修复会影响补单和停止时机，不能把离线通过视为交易表现晋级。按 `docs/28-experiment-protocol.md`，本轮没有开展或判定实盘实验；上线前仍需独立授权及其要求的设计、冻结配置、预注册判据和启动记录。未将经济效果标记为 `accepted`。

| 未判项 | 原因 | 复核时点与方式 |
|---|---|---|
| 取消终态等待对 uptime、补单延迟、成交率的影响 | 本轮只证明安全预算与顺序；没有真实行情样本 | 实盘授权前按 docs/28 另立观测/实验记录；明确终态延迟分布与超时恢复次数 |
| 净 PnL、逆向选择、手续费后的止损表现 | 保留既有 gross stop-loss 语义，未改变策略参数 | 下一次经济效果迭代预注册；不以本轮测试推断收益改进 |

安全缺陷修复不提供关闭保护的生产开关。不要为 A/B 把已知越仓路径作为可上线候选；策略参数试验仍须遵守单配置行对照规则。
