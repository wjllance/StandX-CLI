# 阶段 3 v1 组合候选设计：非线性 price skew + 外部价防御门（2026-07-22）

> **归档记录**：组合候选已经判定 `rejected_split_branch`，后续拆出的
> `nonlinear_skew` 与 `external_guard` 分别完成独立判定。本文只保留设计与证据链，不能作为
> 当前实现或 live 授权依据；当前状态见
> [../../18-maker-strategy-roadmap.md](../../18-maker-strategy-roadmap.md)。

一次发布、两个独立开关、一次 canary 重锁。本文档是实现与验收的唯一设计依据；
判据预注册于本文档"验收判据"节，与 [18](18-maker-strategy-roadmap-full-2026-07.md) 阶段 3
验收标准的差异（uptime 绝对门槛）由 release owner 2026-07-22 裁决。

## 动机与证据链

- 阶段 3 v0（二值加仓侧压制）判定 rejected：尾部治理达标（p95 |position|
  降 40–62%、≥70% 仓时间清零）但双边 uptime 降 43–80pp。死因是结构性的：
  激活阈值落在仓位常住区（激活率 45–79%）+ 释放迟滞闩锁（candidate #3 钉
  +0.2 约 3h，uptime 18%）。见
  [maker-stage3-ab-judgment-2026-07-22.md](../../evidence/maker-stage3-ab-judgment-2026-07-22.md)。
- lag 44.5h 长录 midPx 重切：HL midPx 领先 StandX mark，可测量跟随占 61%、
  条件 lag 中位 2.6s（p25 1.8s）、16–32bps 跳幅档 0 次 already-ahead。见
  [lag-recorder-hype-result-2026-07-22.md](../../evidence/lag-recorder-hype-result-2026-07-22.md)。
- 组合理由：两机制治同一亏损链的不同环节（防御门治首刀、skew 治首刀后的
  逆势累积），canary 重锁按发布计费，合并省一轮 gate；独立开关保证组合被拒
  时拆单机制重跑是纯配置 A/B（按路线图规则不重锁）。

## 机制 1：非线性 price skew（`[nonlinear_skew]`）

替代 v0 的"停挂"：加仓侧报价**永远在场**，只是随库存变陡地歪价。

- 纯函数、**无状态、无迟滞**（v0 闩锁教训直接吸收）：强度是 |position| 的
  连续函数，仓位降强度立刻降。
- 公式：`shift_bps = sign(position) × min(skew_bps × boost × |ratio|, cap_bps)`，
  `ratio = position / max_position`（clamp ±1）。`boost=1` 且 `cap_bps ≥ skew_bps`
  时数值上等于现行线性 skew；`enabled=false` 时走原 `skew_center` 代码路径，
  逐 action 等价。
- 配置：`enabled`（默认 false）、`boost`（≥1，冻结候选 3.0）、`cap_bps`
  （冻结候选 12.0）。
- **红线（修订于实现时，对照代码确认）**：`spread_bps` 是**单侧**偏移
  （报价挂在 `center × (1 ± spread_bps/1e4)`），现行线性 skew（`skew_bps=8`
  满仓偏移）在 |ratio|>0.25 时远侧报价已越出 SIP-5A 10bps 合格带——即
  **现行生产配置本就不满足 SIP-5A 带内约束**，且 release owner 已裁决目标
  函数为"少亏"、不计 SIP-5A 收益。故 cap 红线改锚定策略自身 band：核心校验
  `spread_bps + cap_bps ≤ band_bps`（8+12=20 ≤ 30），band/no-cross 硬 guard
  照常兜底。SIP-5A proximity 损失作为已知代价记录，不作为门槛。
- uptime 结构性不受损：skew 只歪价不撤单，双边报价始终在场（uptime 统计的
  合格深度按 `band_bps=30` 计）。
- 设计输入（v0 数据）：线性 skew 在 0.2–0.3 仓位区的 2–5bps 平移实测挡不住
  趋势碾压；boost=3 下 0.2 仓位即 4.8bps、0.3 仓位 7.2bps，0.5 起顶满
  12bps cap。

## 机制 2：外部价防御门（`[external_guard]`）

HL midPx 已跳走、StandX mark 未跟上时，**临时压制危险侧**；mark 追平即恢复。

- 原始信号：`raw_divergence_bps = (hl_mid / standx_mark − 1) × 1e4`。
- **基差扣除（2026-07-22 paper 冒烟发现后修订）**：HYPE 实测 HL midPx 与
  StandX mark 存在 ~-14bps 的**持久静态基差**（场馆溢价/资金费结构，不是
  狙击信号）。若拿原始水位差触发，guard 会在单侧永久闩锁——精确复刻 v0 的
  uptime 死法。故 CLI 侧维护一条慢 EMA 基线（`basis_half_life_secs`，冻结
  候选 300s），guard 的 typed input 是**超额背离** `excess = raw − basis`：
  秒级跳变原样穿透（相对跳变前的基线度量，不被自身样本吸收），分钟级稳定的
  基差被吸收；首个样本初始化基线，启动基差永不触发。这与 lag 分析的"跳变"
  口径一致。遥测同时记录 `external_divergence_bps`（excess，决策依据）与
  `external_basis_bps`（基线），raw 可重构。
- excess > 0（外部价相对基线更高）→ 我方 ask 是过期便宜货 → 危险侧 =
  Sell；反之 = Buy。
- 控制器（core 纯逻辑，`GuardConfig { enabled, enter_bps, exit_bps, max_age_ms }`）：
  `|excess| ≥ enter_bps` 激活，回落 `< exit_bps` 解除（小迟滞防抖动）；激活
  期间反号越过 enter 立即换边。与 v0 闩锁的本质区别：解除条件是 excess 收敛
  （StandX 追平即闭合，实测中位 2.6s），不依赖成交，无长闩锁风险。
  **冻结候选 `enter=10 / exit=5`（Round 2，2026-07-23 release owner 固定为
  base）**。Round 1 用 `enter=6 / exit=3` 实测激活率 11.5–22.6%（跳变预算
  ~2.1% 的 5–10 倍），uptime ≈ 100%−激活率；lag 分层显示防御价值集中在
  ≥16bps 大跳档（8–16bps 档信噪比差），故上调阈值剔除噪声段、保留尾部杀手
  覆盖。纯配置变更，不重锁。
- 压制语义复用现有 `SideSuppressed` 路径：激活时危险侧 desired 为空 →
  reconcile 撤该侧 resting、不摆新单；解除后自然恢复。事件时长秒级，
  按 lag 数据预算激活时间 ~0.7%/天，uptime 影响可忽略。
- **降级方向 = 关（fail-open to normal）**：HL feed 缺失、断线、样本过期
  （`age_ms > max_age_ms`，冻结候选 5000ms）→ 门自动失效、正常报价。
  外部依赖绝不能成为新的停机源；guard 是防御优化，不是安全不变量。
- **不建亚秒快速通道**：挂现有 cycle 早醒机制（外部 divergence 越过 enter
  阈值 → 唤醒，仍受 1s min-gap 地板约束），反应 ~1–1.5s，对中位 2.6s /
  p25 1.8s 的窗口够用。快速通道留作后续增量。
- CLI 侧：HL WS 客户端从 lag-recorder 抽出为共享模块；feed 任务写
  `Arc<RwLock>` 状态 + watch 唤醒；每 cycle 归一化为
  `Option<ExternalDivergence { divergence_bps, age_ms }>` typed input，
  core 不做任何 I/O（架构边界不变）。
- Replay：确定性回放无 HL feed → guard 输入恒 None → 恒不激活。replay 只
  承担"关闭时等价"验证（路线图既定分工），guard 的经济价值由 live A/B 判。

## 遥测（归因用，全部为可选新增字段/事件）

- `cycle_summary` 新增：`skew_shift_bps`、`external_divergence_bps`、
  `guard_active`、`guard_side`。
- guard 激活/解除各发一条 maker log 事件（新 action，additive）。
- 目的：两臂 A/B 判组合生死；事件级归因（guard 激活窗口内躲掉的成交 vs
  skew 生效期的库存轨迹）从日志做，组合被拒时决定拆哪一半单跑。

## Paper 冒烟记录（2026-07-22/23）

第一轮冒烟（candidate 配置，公共行情，无订单）**抓到基差缺陷**：guard 启动
即因 -14bps 静态基差永久激活 Buy 侧压制——上节的基差扣除修订即由此而来，
这正是"高仓位+外部跳动"类冒烟先行的价值。第二轮冒烟验证修复后行为（基线
初始化后 excess≈0、guard 不再因基差激活、遥测字段齐全），结果随 evidence
记录。

## 组合交互与冒烟场景

已知交互：多头高库存时 skew 把中心下移（ask 更近 touch），若此时外部价
向下跳，guard 压 Buy 侧——两机制同向叠加是**设计内**行为（都在防同一方向
风险）。需专门冒烟的场景：高库存 + 外部跳动同时发生时，(a) 不违反
max_position / band / no-cross；(b) guard 解除后 skew 单独作用的报价恢复
正确；(c) 全关组合 ≡ 现行为。paper 冒烟必须覆盖。

## 验收判据（预注册，live 时间片 A/B，两臂：baseline vs 双开组合）

沿用阶段 2/3 A/B 规程（4h 臂、wind-down 换臂、同 size/max_position）：

- [ ] 全关（两开关均 off）≡ 现行策略，逐 action 等价（含单开×2、双开、
  全关的状态网格离线测试）。
- [ ] 无 max_position / band / no-cross / 账本 / generation 安全违规。
- [ ] 样本外 p95 |position| 降 ≥15%，或 ≥70% max_position 时间降 ≥25%
  （继承 v0 达标线）。
- [ ] 主动退出次数与总 taker exit cost 不高于基线。
- [ ] net PnL ≥ 基线 95%；两臂各至少覆盖一段趋势时段，否则不判 PnL。
- [ ] **时间加权双边 uptime ≥ 80%（绝对值；release owner 2026-07-22 裁决，
  替代原 ≤3pp 相对判据）**。
- [ ] 每 quote-hour 撤单数相对基线增加 ≤20%（兼防 SIP-5A short-cycle
  cancels 条款）。
- [ ] guard 激活时间占比与激活次数落在 lag 数据预算的 3 倍以内
  （防 HL 噪声导致的失控触发；超预算即视为信号质量问题，臂照跑但
  记录为设计缺陷输入）。

组合被拒时的预注册分支：按遥测归因拆单机制，以纯配置 A/B 重跑占优的
一半（不重锁）；两半都无信号则阶段 3 收束、回到基线。

## 实现边界

- core（standx-maker）：`skew_center` 非线性扩展、`guard.rs` 纯控制器、
  `CycleInput` 增 `guard: GuardDecision`；全部纯函数/纯状态机，不读时钟
  不做 I/O。
- CLI（standx-cli）：`[nonlinear_skew]`/`[external_guard]` 配置解析与校验
  （含 cap_bps 带内红线）、HL feed 共享模块、cycle 装配、wait_phase 唤醒
  分支、遥测输出。
- 不改变：现有 JSON 字段语义、live 默认锁、fail-closed 语义、退出/熔断
  政策、订单归属与账本 exactly-once。
