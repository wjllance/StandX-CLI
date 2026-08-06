# StandX CLI 文档

这里按使用场景组织 StandX CLI 的安装、认证、行情、交易、Maker 运行、安全门槛和实验记录。
如果你第一次使用，从快速开始进入；如果你在运行或评估 Maker，先看下面的当前状态，再进入
运行手册或 live gate。

## Maker 当前状态

- Maker 引擎已经具备受控 live 能力，但策略收益**尚未验证**；两轮约 36 小时的 HYPE
  冻结基线读数均为负，当前不扩大规模。
- 当前冻结 HYPE 基线接受了 `nonlinear_skew` 和 `external_guard`；Stage 2 未接受、
  Stage 3 v0 被拒、Stage 4 已终止。
- `external_skew` 已实现且默认关闭，仍待独立 live 裁决，不能视为已晋级能力。
- 当前最高优先级是补齐盘口深度和 taker 方向等纯观测遥测，而不是继续叠加策略机制。
- 账户硬熔断默认关闭；Maker 不自动平仓；cleanup 后无法确认空仓时必须报告 `unknown` 并
  交给人工核对。

完整判定、经济口径和下一步见
[18-maker-strategy-roadmap.md](18-maker-strategy-roadmap.md)。历史路线图、已结束候选和旧
runbook 统一放在 [archive/](archive/README.md)，不能作为新的 live 授权。

## 新用户与 CLI 功能

| 文档 | 用途 |
|---|---|
| [01 - 快速开始](01-getting-started.md) | 安装、升级、配置和第一个命令 |
| [02 - 认证管理](02-authentication.md) | 登录、凭证来源、状态和登出 |
| [03 - 市场数据](03-market-data.md) | 交易对、价格、深度和 K 线 |
| [04 - 账户信息](04-account.md) | 余额、持仓和账户订单 |
| [05 - 订单管理](05-orders.md) | 下单、撤单和订单查询 |
| [06 - 交易历史](06-trading.md) | 成交记录查询 |
| [07 - 杠杆与保证金](07-leverage-margin.md) | 杠杆和保证金操作 |
| [08 - 实时数据流](08-streaming.md) | 公共与认证 WebSocket 流 |
| [09 - 输出格式](09-output-formats.md) | Table、JSON 和 CSV 输出契约 |
| [10 - 特殊功能](10-special-features.md) | OpenClaw、Dry Run 等扩展入口 |
| [11 - 故障排除](11-troubleshooting.md) | 安装、认证、网络和命令问题 |

## Maker 当前文档

| 文档 | 用途 | 权威边界 |
|---|---|---|
| [13 - Maker 运行手册](13-maker.md) | 参数、报价、账本、遥测、退出与运行时安全 | 当前运行行为 |
| [14 - Maker live gate](14-maker-live-gate.md) | 实盘解锁证据、恢复与 cleanup 门槛 | 当前 live 安全门槛 |
| [15 - OpenObserve](15-openobserve.md) | 日志采集、查询、看板与告警 | 当前可观测性操作 |
| [16 - WebSocket 目标](16-ws-iteration-goals.md) | 当前 WS 架构、健康与验收边界 | 开发参考 |
| [17 - WS canary 快速启动](17-ws-command-canary-quickstart.md) | 受控命令链 create/cancel 验证 | 操作手册 |
| [18 - 当前状态与路线](18-maker-strategy-roadmap.md) | 机制判定、经济结论、优先级与长期不变量 | 当前策略状态真源 |
| [29 - External skew 候选](29-maker-external-skew-design.md) | 默认关闭候选的设计和预注册判据 | 待裁决候选，不代表已授权 |
| [30 - 带内 uptime 参数收缩](30-maker-uptime-band-tightening-design.md) | SIP-5A ±10bp 带内 uptime 的渐进参数收缩设计与预注册判据 | 待裁决序列，每步需 owner 裁决 |

## 生产操作与实验

| 文档 | 用途 | 注意事项 |
|---|---|---|
| [12 - 发布流程](12-version-checklist.md) | 版本真源、发布 PR 和验证 | 不复制当前版本号 |
| [19 - 受控 A/B 基础 runbook](19-maker-stage2-live-ab-runbook.md) | 当前 stage2 编排器的部署、canary、应急与轮换流程 | 历史授权不可复用 |
| [27 - 冻结基线 PnL 采集](27-maker-baseline-pnl-collection-runbook.md) | 单臂长跑、绝对读数和异常处置 | 新运行必须重新授权 |
| [28 - 实验规程](28-experiment-protocol.md) | 预注册判据、四件套、判定词汇和未判项 | 影响成交结果的变更必须遵守 |
| [ADR 0001](adr/0001-maker-recovery-supervision.md) | Maker 恢复与监督架构 | 已采纳的架构决策 |

## 证据与历史

- [evidence/](evidence/)：不可变的实现、canary、A/B、PnL 和事故复核记录。
- [archive/](archive/README.md)：已失效路线图、已结束设计和历史 runbook。
- [`standx-maker` README](../crates/standx-maker/README.md)：核心能力、模块边界和当前性能摘要。
- [`standx-sdk` README](../crates/standx-sdk/README.md)：认证、HTTP/WebSocket 和传输契约。

## 推荐阅读路径

1. 新用户：01 → 02 → 03 → 09。
2. CLI 交易与自动化：02 → 05 → 08 → 09。
3. Maker paper：13 → 15 → 18。
4. Maker live 或生产运维：13 → 14 → 17/19/27；没有精确授权时停在文档与验证阶段。
5. 策略或安全变更：18 → 28 → 对应 evidence，并遵守仓库 `AGENTS.md` 的验证要求。
