# Maker 短期迭代路线图（2026-07-27 快照）

本文档取代已失效的
[20-maker-short-term-roadmap-2026-07.md](20-maker-short-term-roadmap-2026-07.md)
（其失效条件"阶段 3 A/B 判定完成"早在 07-22 命中，此后一直无替代快照）。

和 20 号文档一样，这是未来 2–4 周的执行层快照，回答"下一步做什么、按什么顺序、什么
条件下改变计划"。长期阶段定义、验收口径和轨道原则见
[18-maker-strategy-roadmap.md](18-maker-strategy-roadmap.md)，本文不重复也不修改它们；
两者冲突时以 18 号文档为准。

## 现状盘点（2026-07-27）

- **阶段 3 全线关闭**：v1 拆单 `nonlinear_skew`（boost=3.0 / cap=12.0）07-25 accepted；
  阶段 3-guard `external_guard`（enter=10 / exit=5）07-27 accepted
  （[判定报告](evidence/maker-guard-spinoff-ab-judgment-2026-07-27.md)）。
  **当前冻结生产基线 = `examples/maker-guard-hype-candidate.toml`**（skew + guard 双开，
  sha256 `6314a37462e3bfda2cb21f14e503fae4d2997dca449f329de80e7ab22be4b9fc`）。
- **alpha 轨当前没有在跑的实验，live 时间片空闲。** 这是本快照要回答的空档。
- **PnL 仍是未判项**：两次晋级都是在"PnL 不作晋级条件"的预注册前提下做出的（skew 以
  尾部/markout 判、guard 以成本侧+信号质量判）。guard 轮方向性读数为 guard 臂 -0.172
  vs baseline +0.006（两臂窗口活跃度不同，混淆无法分离）。**没有任何证据说明当前冻结
  基线在自身规模上是否赚钱。**
- 阶段 4 已终止（07-20，加宽 live 显著为负）；阶段 2 未 accepted，基线继承走的是阶段 3 链。
- **安全轨**：一级（运维件）已完成；二级（5-b）的四项主体已合并 main（07-28，见下），
  剩余硬化项（Divergence B/C/D）未做。5-b 是扩大规模（加 size / max_position /
  多 symbol）的强制前置。
- `maker-recovery-dedup` **挂账项可关闭**：分支的 7 个提交已以不同 sha 全部落在 main
  （`173ecd7` / `b1fc8fe` / `be2be10` / `0ef92c2` / `51512b6` 等），分支本身只是落后 main
  79 个提交的旧副本。处置 = 封存/删除远端分支，不需要再补 live 验证。
- **Divergence 降级（2026-07-28 复核结论）**：A（分类 + standby）已在 main；
  **C（熔断豁免）关闭——前提消失**（共享恢复熔断早已从代码移除，只剩两个 deprecated
  配置字段，没有任何跨恢复累计的计数器）；**B（恢复迟滞）/ D（tick 阈值）降级为观测项**，
  致命后果已从"60s 后硬停"变成"standby 期间不报价"，立项触发条件已预注册。复核记录见
  [maker-divergence-degradation-review-2026-07-28.md](evidence/maker-divergence-degradation-review-2026-07-28.md)。
  六条架构建议 #6 的缺口随 C 一并关闭。
- **ADR 0001 已修订**（2026-07-27）：C-lite 触发条件确认命中，落地形态判定为折中 C-lite，
  往后的扩张判据已写入 ADR 修订记录。此项从债务清单移除。
- **质量债务**：#259 / #260 / #261 / #227 / #277 仍 open（#226 已关闭）。
- **运维尾巴已清**：guard 轮 baseline#4 收尾留下的 -0.1 HYPE 空头已由 owner 手动处置
  （2026-07-28 确认，账户回到 FLAT）。下一个实验的 FLAT 前置不再被它卡住，但仍按
  runbook 实测而非依赖本记录。

## 阻塞前置（先清）

1. ~~**残余 -0.1 HYPE 空头**~~：已处置（2026-07-28 owner 确认），已回填 guard 判定报告。
2. **auth token 有效期**：任何 live 动作前按
   [23 号手册的 token 前置](23-maker-stage3v1-live-ab-runbook.md)确认剩余有效期覆盖
   整个采集窗口（07-24 事件教训）。

## 主线：5-b 安全轨二级（已裁决，2026-07-27）

release owner 于 2026-07-27 裁定主线为**候选 1（5-b）**。本轮四项范围已实现并
**合并 main**（2026-07-28，PR #332 / `469550a`，含一轮对抗式复审的三处返工），
离线验证与 CI 全绿，无 live 动作；立项与实现记录见
[26-maker-stage5b-design.md](26-maker-stage5b-design.md)。5-b 剩余条目（Divergence
B/C/D 等硬化项）未做。候选 2（基线 PnL 采集）仍是推荐的下一步且不占人力，候选 3 保持支线。

### 候选 1（已选为主线，四项主体已合并 main）：5-b 安全轨二级

18 号文档写明的下一步，也是**唯一能解锁扩大规模的路**。范围（见
[18 号阶段 5 剩余范围](18-maker-strategy-roadmap.md)）：

- 正常 inventory trim 与 emergency risk exit 使用不同 typed policy/effect；
- volatility halt 期间是否允许紧急退出——**定稿在本级**（原"阶段 3 v1 前"的时点已随
  v1 上线而过，v1 范围不触碰退出语义，已在 18 号文档中对齐）；
- stop-loss 后残余仓位的显式 handoff；自动 flatten 必须默认关闭 + 单独授权；
- equity/margin 的 alert 与 hard floor 拆配置名、拆 typed outcome；
- 背离恢复迟滞（Divergence B）、熔断豁免（Divergence C = 架构建议 #6 缺口）按需并入
  ——**2026-07-28 复核后关闭/降级为观测项，5-b 名下不再有待写的代码**（见现状盘点）。

优点：纯代码 + 默认关 + replay 等价，**不消耗 live 时间片**，可与候选 2 完全并行。
经济动因：SIP-5A 奖励在当前规模 ≈ 0，不扩规模，刚 accepted 的两个机制也兑现不了收益。

### 候选 2（**当前主线**，2026-07-28）：基线 PnL 绝对读数采集

用新冻结基线单臂连续跑 2–3 天，只求 PnL / markout / uptime 的**绝对读数**，不做 A/B、
不设预注册判据、不带晋级压力。理由：5-b 完成后的下一步就是放大 size，而放大一个
"不知道是否赚钱"的基线会把损耗按比例放大。这一步把 PnL 未判项从"结构性缺口"降级为
"有数字的已知量"。占用人力极小（开机 + 每日看一眼遥测）。

**手册已就绪**：[27-maker-baseline-pnl-collection-runbook.md](27-maker-baseline-pnl-collection-runbook.md)
（前置检查、授权文本模板、单臂跑法、每日记录清单、异常处置、终止条件）。等 release owner
填授权文本即可开跑；采集顺带记录 `divergence_standby` 事件，为 Divergence B 是否立项提供
唯一证据来源。

### 候选 4（草案，2026-07-29，待裁决）：外部领先价连续偏移 `[external_skew]`

基线 PnL 采集的亏因分解（capture +5.0bps vs 30s markout -5.8bps，净 -1.1/19h）
把靶心定在 markout 上；离线检验（同一 run 日志，只读）显示 excess→30s mark
移动斜率 ≈ 1 且线性延伸到 ±2bps 桶，成交时点逆向选择系统性存在（买 -3.2 /
卖 +3.65bps），反事实上限 λ=0.5 → +0.27（回收亏损 25~50%）。设计与预注册判据见
[28-maker-external-skew-design.md](28-maker-external-skew-design.md)。
**不占 live 时间片、窗口内不部署**；裁决与 A/B 排在基线 PnL 窗口收尾之后。
microprice（盘口量失衡）降为第二优先，阻塞在 depth 观测字段（量从未落日志）。

### 候选 3：挂账清理与质量债

- `maker-recovery-dedup` 分支封存（见现状盘点，已确认可无损删除）；
- Divergence B + C 实现（默认关、replay 等价）——若走候选 1，这两项直接并入 5-b；
- 质量债 #259（dedup 残余）/ #261（CLI 一致性）/ #277（请求生命周期关联）。

## 明确不做（含理由）

- **基差半衰期评估**（`basis_half_life_secs` 300s → 60–120s 的条件性评估）与
  **"事件加宽" v1.1**：guard 判定报告已声明二者按各自证据另行立项。当前 guard 激活占比
  0.52–1.92%、误报 1/18，没有证据指向半衰期是瓶颈。压着。
- **趋势滤波暂停报价**：与已终止的阶段 4 同族，证据门槛只会更高（继承 20 号文档判断）。
- **对冲腿（跨所对冲）**：架构级变更，必须在安全轨二级完成之后才有资格立项。
- **SIP-5A 奖励层报价**：当前规模下奖励 ≈ 0，与扩规模决策绑定评估。
- **宏观事件窗口 / 周末缩量的自动化**：继续只做运维规则（runbook + 遥测标注），累积
  4–8 周带标注数据后再谈立项（继承 20 号文档结论，避免重蹈阶段 4 的覆辙）。

## 本文档的失效条件

出现以下任一情况，本快照作废，回 18 号文档重排：

- 主线裁决落定并执行完毕（5-b accepted，或 PnL 采集给出改变优先级的结论）；
- 决定扩大规模（触发安全轨二级的全部前置）；
- live gate 或熔断语义发生变化；
- 新的 alpha 候选立项（届时按 18 号文档的 v0 流程走）。
