# 冻结基线 PnL 绝对读数采集手册（单臂长跑，2–3 天）

立项：[25-maker-short-term-roadmap-2026-07-27.md](25-maker-short-term-roadmap-2026-07-27.md)
候选 2。上游口径见 [18-maker-strategy-roadmap.md](18-maker-strategy-roadmap.md)，
应急处置沿用 [19-maker-stage2-live-ab-runbook.md](19-maker-stage2-live-ab-runbook.md)。

## 这不是什么

**这不是 A/B，不是晋级实验，没有预注册判据，没有 accepted/rejected 分支。**

阶段 3（skew）与阶段 3-guard 都是在"PnL 不作晋级条件"的前提下 accepted 的，所以到今天
为止**没有任何证据说明当前冻结基线在自身规模上是否赚钱**。本次采集只回答一个问题：

> 冻结基线连续跑 2–3 天，净 PnL / markout / uptime 的**绝对读数**是多少？

因此：不换臂、不比较、不调参、不中途改配置。任何"看起来该优化一下"的念头都记录到
观察清单里，不在本次窗口内动手——一改就失去绝对读数的意义。

## 冻结物料

- 配置：`examples/maker-guard-hype-candidate.toml`（skew + guard 双开），sha256
  `6314a37462e3bfda2cb21f14e503fae4d2997dca449f329de80e7ab22be4b9fc`。
  **原样使用，一个字节都不改。** 启动前用 `shasum -a 256` 复核。
- 关键参数（只作复核用，不在本轮调整）：`spread_bps=8` / `band_bps=30` / `size=0.1` /
  `max_position` 与 `stop_loss=5.0` 沿用基线；`max_divergence_bps=15`；
  `vol_pause_bps=40`；`nonlinear_skew(boost=3.0, cap_bps=12.0)`；
  `external_guard(enter=10, exit=5, max_age_ms=5000, basis_half_life_secs=300)`。
- **阶段 5-b 的账户硬熔断本轮保持关闭**（`stop_equity_below` / `stop_margin_below`
  不设置 = 0）。理由：本轮不扩规模，风险预算沿用 canary 口径，`stop_loss=5.0` 已是会话级
  刹车；硬熔断的取值指引见 [23 号手册的硬熔断小节](23-maker-stage3v1-live-ab-runbook.md)，
  扩规模授权时才开。
- 代码：合并 5-b 后的 main（`469550a` 或更新），`cargo build --release`。5-b 是纯类型/
  输出变更 + 默认关能力，不改变报价与退出语义，因此**不需要重新 canary**（编排器白名单
  case (f) 的同理逻辑：无策略路径变更）。若对此有异议，按
  [14-maker-live-gate.md](14-maker-live-gate.md) 补一次受监督 canary 再开跑。

## 前置检查（逐条打勾，缺一不启动）

- [ ] **FLAT 实测**（不依赖上一轮收尾记录）：

  ```bash
  standx -o json account positions
  standx -o json account orders
  ```

  两条都必须为空。非空 → 先手动处置并回填对应判定报告。

- [ ] **auth token 有效期覆盖整个窗口**——这是本手册最容易翻车的一条：采集窗口是
      **2–3 天**，远长于以往 4h 臂，07-24 的 token 过期事件正是在更短的窗口里发生的。

  ```bash
  standx auth status
  ```

  剩余有效期必须覆盖计划窗口 + 余量；不足则先 `standx auth login`（含私钥）刷新。
  **不要依赖 maker 的 `token_expiry_critical` 预警**（提前量仅 ~30 分钟）。
  token 失效的连带后果是 cleanup 也无法撤单，残余单/仓位只能等重新登录后手动处置。
  若单个 token 的有效期无法覆盖 3 天，就把窗口切成两段独立 run（各自 run_id，
  中间干净停机 + FLAT 复核），**不要**在 run 中途换 token。

- [ ] **场馆 metadata 复核**：`standx -o json market symbols`，确认
      `price_tick_decimals=3` / `qty_tick_decimals=2` / `min_order_qty=0.1` 未变。
- [ ] **容器到 Hyperliquid 的连通性**：guard 需要 HL midPx feed（无凭证公共 WS）。
      不通 → candidate 退化为"guard 全程失活"，本次读数就不是双开基线的读数。
      启动后在日志确认 `guard_enabled=true` 且 `external_basis_bps` 已初始化。
- [ ] **webhook 可达**：`--alert-webhook` 已配置且实测送达。
- [ ] **两条 OpenObserve push 告警已 provision**（`python3 scripts/openobserve_alerts.py`，
      见 [15 号文档第 6 节](15-openobserve.md)）：`standx_maker_deadman`（进程静默死亡）
      与 `standx_maker_critical_risk`（`severity=critical` 的任何事件）。
      **无人值守 2–3 天不能只靠进程自己的 webhook**——那个 POST 不重试，端点挂了就没人知道，
      而 deadman 不会响（进程还在正常发 cycle_summary）。
- [ ] **磁盘**：NDJSON 证据 2–3 天的量级远大于 4h 臂，确认 `STANDX_LOG_DIR_HOST`
      所在卷有余量。
- [ ] **互斥**：XAG / HYPE 的任何 A/B 容器与手工 live maker 全部停止（maker 锁是容器本地
      的，两个部署不会互斥，见 `deploy/docker/README.md`）。

## 精确授权文本（release owner 填写后才能启动）

```text
授权：冻结基线 PnL 绝对读数采集（单臂长跑）
symbol：HYPE-USD
配置：examples/maker-guard-hype-candidate.toml（sha256 6314a374…，原样）
代码：git sha ______
风险边界：单 symbol、一档、最小有效数量、max_position 沿用基线；
          stop_loss=5.0 生效；账户硬熔断不开启
窗口：______ 起，计划 ____ 小时（2–3 天），不换臂、不调参
emergency cancel 操作人：______
授权人 / 时间：______
```

风险预算沿用 canary 口径（[18 号"风险预算"](18-maker-strategy-roadmap.md)）：已知最坏
路径是趋势市库存满仓后 stop-loss 停机持仓，损失上界约
`max_position × 不利变动幅度 + 退出成本`。本轮不扩规模，因此不触发安全轨二级的额外前置。

## 启动

单臂长跑**不用 A/B 编排器**（它按 `STANDX_STAGE2_ARM_SECONDS` 每 4h wind-down 换臂，
正是本轮要避免的）。直接单进程跑：

```bash
export STANDX_ENABLE_LIVE_MAKER=1
export STANDX_RUN_ID="baseline-pnl-$(date -u +%Y%m%dT%H%M%SZ)"
scripts/run_maker_observed.sh target/release/standx --output json maker run HYPE-USD \
  --maker-config examples/maker-guard-hype-candidate.toml --live
```

启动后 15 分钟内确认一次：`cycle_summary` 正常产出、`guard_enabled=true`、
`external_basis_bps` 已初始化（静态基差约 -14~-15.5bps 不得触发激活）、
`🟢 started` webhook 已送达、OpenObserve 有数据。

## 每日记录（一天一次，5 分钟）

绝对读数为主，不做任何对比结论：

| 项 | 取值来源 |
|---|---|
| 净 PnL（会话累计） | `cycle_summary.pnl` 最新值 / `performance_summary` |
| 净 PnL 归因（capture / markout） | `performance_summary` |
| **手续费 / rebate 合计** | `performance_summary` 的 `fee_quote` / `rebate_quote` |
| **不可换算成本笔数** | `performance_summary.execution_costs_unavailable`（>0 说明有成交的 fee 资产不是 quote/D-quote，或 audit 漏掉） |
| **funding 现金流合计** | `performance_summary.funding_quote`（负 = 净付出）+ `funding_available` |
| **未归属 funding 笔数** | `performance_summary.funding_unattributed`（>0 → 有现金流没进净额，见下） |
| **funding 覆盖缺口** | `performance_summary.funding_coverage_gap`（true → 有一段 funding 根本没读到：拉取失败或分页被截断） |
| **净 PnL 完整标志** | `performance_summary.net_pnl_complete`（手续费与 funding 都齐才为 true） |
| 1s / 5s / 30s markout | `performance_summary` |
| 时间加权双边 uptime | `performance_summary` |
| 成交数与量 | `fills_total` |
| `p95 \|position\|`、≥70% max_position 时间 | 仓位序列 |
| 每 quote-hour 撤单数 + 原因分解 | `cancel` 事件按 `reason` 计数 |
| 主动退出次数与成本 | `inventory_exit_submitted`（本轮起带 `exit_kind`） |
| **退出被抑制次数** | `cycle_summary.exit_suppressed`（5-b 新增；halt 吃掉的退出） |
| guard 激活时间占比 / 转换次数 | `guard_active` / `guard_side` 变化 |
| halt 轮数与占比 | `cycle_summary.halted` |
| **行情降级 standby 事件与时长** | `action:"market_data_standby"` 的 `paused_secs` / `fault_class` / `divergence_bps`（结构化字段，不用解析 message；见下） |
| token 剩余有效期 | `standx auth status` |

**standby 要单独盯**：行情降级现在是无限期 standby（不再停机），代价是 standby 期间
完全不报价。这次采集顺带回答"这个洞在 HYPE / `max_divergence_bps=15` 下会不会真的咬人"
——它是 Divergence B（恢复迟滞）是否值得立项的唯一证据来源，见
[Divergence 降级复核记录](evidence/maker-divergence-degradation-review-2026-07-28.md)。
取数直接用 `market_data_standby` 事件的 `paused_secs` 最大值与累计值（单次 > 10 分钟 /
窗口内累计 > 1% 运行时间即命中预注册的立项触发条件）。

### 成本项的口径（读数解释时必须带上）

**本次读数是完整的净额：gross − 手续费 − funding。** 判断口径完整性看
`net_pnl_complete`（下面三项全部满足才为 true）。

- **手续费**：机制端到端接好了——REST audit 解析 `fee_qty`/`fee_asset`，核心账本对已见过的
  成交会**回填** costs，所以"账户流先记下成交（无 fee）→ 30 秒后 audit 补上"是正常路径。
  不可换算的部分（fee 资产不是 quote 也不是 D-quote）已被 `execution_costs_unavailable`
  显式计数，不会被静默当成零成本。
- **funding（2026-07-28 接入）**：authenticated `GET /api/query_funding_history` 是权威
  来源（不是推导），在 30 秒 audit 周期里增量拉取并折进 `funding_quote`。
  `qty` 的符号即场馆口径：**负 = 付出，正 = 收取**，与 core 的约定一致，无需翻转。
  无法折算的行（结算资产既不是 quote 也不是其 D 前缀形式，或乱序到达）计入
  `funding_unattributed` 并清掉 `net_pnl_complete`——不会被静默丢弃。拉取失败或分页
  达到上限（可能截断更早的行）另计入 `funding_coverage_gap`，同样清掉完整标志。
  **funding 故障不会中断报价或仓位对账**：同一个 audit 也承担安全对账，让遥测端点
  拖垮它是严重性倒置。
- **实测量级（决定了为什么必须接）**：HYPE 是**每小时**结算（不是 8 小时）。
  2026-07-21→27 的 137 小时窗口共 91 期，合计 **-0.006252 DUSD**（70 期付 / 21 期收，
  单期均值 |0.000119|），即 **-0.0011/24h**。对照 guard 轮 baseline 三臂 ~36h 的
  **+0.006** 净 PnL——**funding 约占读数的 10–30%，同向拖累**。它不足以推翻一个明确为正的
  读数，但足以决定一个接近零的读数的符号，而基线恰恰在零附近。
- **`/api/query_funding_rates`（公开历史费率）实测对 HYPE / BTC / XAG 全部返回 `[]`**，
  所以"事后反算"这条路不存在——这也是必须在采集期间接入而不是事后补的原因。
- **扩规模时按新敞口重算**：名义敞口放大 N 倍，funding 同比放大，且趋势市里费率本身会变。

**无人值守前提**：本次窗口 2–3 天，必须先按
[15 号文档第 6 节](15-openobserve.md) provision **两条** OpenObserve 告警——
deadman（进程静默死亡）+ critical risk（进程还活着但出了 stop-loss / 账户硬熔断 /
残余仓位 UNKNOWN 等）。只靠进程自己的 webhook 不够：那个 POST 不重试。

## 异常处置

| 情况 | 动作 |
|---|---|
| stop-loss 触发（会话 PnL ≤ -5.0） | 这是设计行为，不是故障。进程 fail-safe 停机 + 撤单 + **残余仓位交接**；按交接输出处置残余，记录窗口截断的原因与时点，**不要**为了跑满窗口重启 |
| `residual_position` 报 `unknown` | 立刻人工去场馆核对（5-b 起"无法确认空仓"不等于空仓） |
| `residual_position` 报 `handoff` | 手动平掉并回填记录 |
| token 临近过期 | 干净停机（Ctrl+C / SIGTERM）→ 刷新 token → 作为新 run 继续；不在 run 内换 token |
| 长时间 halt 或 standby | 记录起止时点与时长，**不干预**；这是读数的一部分 |
| cleanup 失败 / 残余订单 | 走 19 号手册应急处置（symbol 换 HYPE-USD），本次 run 标记为截断 |
| 其他 fail-safe 停机（退出码 75） | 记录原因，不自动重启；重启需要新的授权文本 |

## 终止条件

任一满足即结束采集：

- 计划窗口跑满（2–3 天）；
- stop-loss 或其他 fail-safe 停机（窗口截断，读数仍然有效，注明截断原因）；
- release owner 主动裁决终止。

结束后：干净停机 → 撤单与终仓复核（看 5-b 的交接输出）→ 把每日记录汇总成一份
`docs/evidence/maker-baseline-pnl-<日期>.md`，**只报读数与观察，不给晋级/否决结论**。

## 这份读数拿来干什么

它是**扩大规模决策的输入**，不是策略判决：

- 读数明确为正 → 扩规模的经济前提成立，按安全轨二级的 runbook 开启 `stop_*` 硬熔断
  并走扩规模授权（SIP-5A 奖励只有在规模上来后才有意义）。**先确认
  `net_pnl_complete=true`**：为 false 时读数不含全部成本，不能作为扩规模依据。
- 读数明确为负 → 扩规模会按比例放大损耗；先回 18 号文档找下一个 alpha 候选，不扩。
- 读数在噪声内 → 延长采集或先补一段趋势时段，不急于决策。
