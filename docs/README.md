# StandX CLI 使用说明文档

本文档目录包含 StandX CLI 的详细使用说明，按功能模块分类，每个文档包含命令用法、参数说明、示例输出和测试方法。

---

## 文档结构

```
docs/
├── README.md                 # 文档目录说明
├── 01-getting-started.md     # 快速开始
├── 02-authentication.md      # 认证管理
├── 03-market-data.md         # 市场数据
├── 04-account.md             # 账户信息
├── 05-orders.md              # 订单管理
├── 06-trading.md             # 交易历史
├── 07-leverage-margin.md     # 杠杆与保证金
├── 08-streaming.md           # 实时数据流
├── 09-output-formats.md      # 输出格式
├── 10-special-features.md    # 特殊功能
├── 11-troubleshooting.md     # 故障排除
├── 12-version-checklist.md   # 版本发布检查清单
├── 13-maker.md               # 做市机器人（Maker Bot）
├── 14-maker-live-gate.md     # Maker 实盘解锁门槛
├── 15-openobserve.md         # OpenObserve 遥测
├── 16-ws-iteration-goals.md  # Maker WebSocket 迭代目标
├── 17-ws-command-canary-quickstart.md # WS 生产 Canary 快速启动
├── 18-maker-strategy-roadmap.md # Maker 策略迭代路线与验收标准
├── 19-maker-stage2-live-ab-runbook.md # Stage 2 canary/A-B 操作手册
├── 20-maker-short-term-roadmap-2026-07.md # Maker 短期迭代路线图快照（2026-07，已被 25 取代）
├── 21-maker-stage3-live-ab-runbook.md # Stage 3 canary/A-B 操作手册
├── 22-maker-stage3v1-guard-design.md # 阶段 3 v1 组合候选设计（非线性 skew + 防御门）
├── 23-maker-stage3v1-live-ab-runbook.md # Stage 3 v1 canary/A-B 操作手册
├── 24-maker-guard-spinoff-design.md # 阶段 3-guard 独立立项设计与判据
├── 25-maker-short-term-roadmap-2026-07-27.md # Maker 短期迭代路线图快照（2026-07-27，当前）
├── 26-maker-stage5b-design.md # 阶段 5-b 安全轨二级设计与实现记录
└── 27-maker-baseline-pnl-collection-runbook.md # 冻结基线 PnL 绝对读数采集手册
```

---

## 快速导航

| 文档 | 内容 | 阅读顺序 |
|------|------|----------|
| [01-getting-started.md](01-getting-started.md) | 安装、配置、第一个命令 | 1 |
| [02-authentication.md](02-authentication.md) | 登录、登出、状态检查 | 2 |
| [03-market-data.md](03-market-data.md) | 行情、深度、K线等 | 3 |
| [04-account.md](04-account.md) | 余额、持仓、订单 | 4 |
| [05-orders.md](05-orders.md) | 下单、撤单、查询 | 5 |
| [06-trading.md](06-trading.md) | 成交历史 | 6 |
| [07-leverage-margin.md](07-leverage-margin.md) | 杠杆、保证金 | 7 |
| [08-streaming.md](08-streaming.md) | WebSocket 实时数据 | 8 |
| [09-output-formats.md](09-output-formats.md) | 表格、JSON、CSV | 9 |
| [10-special-features.md](10-special-features.md) | OpenClaw、Dry Run | 10 |
| [11-troubleshooting.md](11-troubleshooting.md) | 常见问题解决 | 参考 |
| [12-version-checklist.md](12-version-checklist.md) | 版本发布检查清单 | 参考 |
| [13-maker.md](13-maker.md) | 做市机器人（SIP-5A、paper/live、遥测） | 进阶 |
| [14-maker-live-gate.md](14-maker-live-gate.md) | Maker 实盘解锁门槛与证据 | 进阶 |
| [15-openobserve.md](15-openobserve.md) | OpenObserve 遥测与看板 | 进阶 |
| [16-ws-iteration-goals.md](16-ws-iteration-goals.md) | Maker WebSocket 迭代目标与验收边界 | 开发参考 |
| [17-ws-command-canary-quickstart.md](17-ws-command-canary-quickstart.md) | WS 生产 Canary 配置、执行与成功判据 | 操作手册 |
| [18-maker-strategy-roadmap.md](18-maker-strategy-roadmap.md) | Maker 策略分阶段目标、量化验收标准与晋级门槛 | 开发参考 |
| [19-maker-stage2-live-ab-runbook.md](19-maker-stage2-live-ab-runbook.md) | Stage 2 小额 canary、应急处置与 2 小时 A/B | 操作手册 |
| [20-maker-short-term-roadmap-2026-07.md](20-maker-short-term-roadmap-2026-07.md) | Maker 短期迭代路线图快照（主线阶段 3 A/B，已失效并被 25 取代） | 历史记录 |
| [21-maker-stage3-live-ab-runbook.md](21-maker-stage3-live-ab-runbook.md) | Stage 3 canary、应急处置与 4 小时 A/B | 操作手册 |
| [22-maker-stage3v1-guard-design.md](22-maker-stage3v1-guard-design.md) | 阶段 3 v1 组合候选设计（非线性 price skew + 外部价防御门） | 开发参考 |
| [23-maker-stage3v1-live-ab-runbook.md](23-maker-stage3v1-live-ab-runbook.md) | Stage 3 v1 canary、前置检查与 4 小时 A/B | 操作手册 |
| [24-maker-guard-spinoff-design.md](24-maker-guard-spinoff-design.md) | 阶段 3-guard 独立立项设计与预注册判据（accepted） | 开发参考 |
| [25-maker-short-term-roadmap-2026-07-27.md](25-maker-short-term-roadmap-2026-07-27.md) | Maker 短期迭代路线图快照（阶段 3 关闭后的当前快照） | 开发参考 |
| [26-maker-stage5b-design.md](26-maker-stage5b-design.md) | 阶段 5-b（安全轨二级）退出政策 typed 分离、残余仓位交接与账户硬熔断 | 开发参考 |
| [27-maker-baseline-pnl-collection-runbook.md](27-maker-baseline-pnl-collection-runbook.md) | 冻结基线 PnL 绝对读数采集（单臂 2–3 天）前置、授权与记录清单 | 操作手册 |

---

## 测试环境要求

- **Rust**: 1.75+ (用于构建)
- **操作系统**: Linux, macOS, Windows
- **网络**: 可访问 https://perps.standx.com
- **认证** (可选): JWT Token 和 Ed25519 私钥

---

## 阅读建议

1. **新用户**: 按顺序阅读 01 → 02 → 03 → 04
2. **开发者**: 重点关注 03, 05, 08, 09, 10
3. **做市/量化**: 13-maker.md（依赖 02, 05, 08, 09）；迭代 WS 能力时配合 16-ws-iteration-goals.md，迭代策略时配合 18-maker-strategy-roadmap.md
4. **测试人员**: 每个文档末尾的「测试检查清单」
5. **故障排查**: 直接查阅 11-troubleshooting.md

---

*文档版本: 0.3.1*  
*最后更新: 2026-02-26*
