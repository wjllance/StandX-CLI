# 文档归档

这里保存已经失效、被取代或已完成的设计、路线图和运行记录。归档材料用于追溯决策与
复核证据，**不是当前实现、操作流程或 live 授权的真源**。

当前入口：

- Maker 当前状态与路线：[../18-maker-strategy-roadmap.md](../18-maker-strategy-roadmap.md)
- Maker 运行与安全语义：[../13-maker.md](../13-maker.md)
- Live 解锁门槛：[../14-maker-live-gate.md](../14-maker-live-gate.md)
- 当前实验规程：[../28-experiment-protocol.md](../28-experiment-protocol.md)
- 原始判定与采集证据：[../evidence/](../evidence/)

## 2026-07 Maker 阶段材料

| 归档材料 | 归档原因 | 当前结论或替代入口 |
|---|---|---|
| [完整旧路线图](2026-07-maker/18-maker-strategy-roadmap-full-2026-07.md) | 混合了已完成阶段、历史重排和旧计划 | [当前精简路线](../18-maker-strategy-roadmap.md) |
| [20 号短期路线图](2026-07-maker/20-maker-short-term-roadmap-2026-07.md) | 2026-07-22 失效并被 25 号取代 | 当前结论已收敛到 18 号 |
| [Stage 3 v0 runbook](2026-07-maker/21-maker-stage3-live-ab-runbook.md) | `size_skew` 候选已 rejected | 判定见 [Stage 3 报告](../evidence/maker-stage3-ab-judgment-2026-07-22.md) |
| [Stage 3 v1 组合设计](2026-07-maker/22-maker-stage3v1-guard-design.md) | 组合被 `rejected_split_branch`，后续拆单判定 | `nonlinear_skew` 与 `external_guard` 的当前状态见 18 号 |
| [Stage 3 v1 runbook](2026-07-maker/23-maker-stage3v1-live-ab-runbook.md) | 对应授权窗口与组合实验已结束 | 新 live 动作必须使用当前 gate 和新授权 |
| [Guard 独立设计](2026-07-maker/24-maker-guard-spinoff-design.md) | 候选已 accepted，阶段关闭 | 判定见 [Guard 报告](../evidence/maker-guard-spinoff-ab-judgment-2026-07-27.md) |
| [25 号短期路线图](2026-07-maker/25-maker-short-term-roadmap-2026-07-27.md) | 日期快照已被当前状态页取代 | 当前经济结论与优先级见 18 号 |
| [Stage 5-b 设计](2026-07-maker/26-maker-stage5b-design.md) | 四项主体已实现，设计任务关闭 | 运行语义见 13 号，长期政策见 18 号 |
| [Cleanup 残余判定设计](2026-07-maker/29-maker-cleanup-residual-verification.md) | Phase 1 与 Phase 2 已完成 | 当前 gate 结论见 14 号 |

`legacy/create-docs.sh` 是早期仅打印 01–11 目录的辅助脚本，已经不能反映当前文档结构，
仅为历史追溯保留，不应执行或用于生成索引。

## 归档规则

- 保留原始事实、判据、时间戳和当时的授权边界；仅增加归档提示和修复相对链接。
- 归档中的授权文本不会自动延续。新的 live 操作必须重新确认 symbol、敞口、配置哈希、
  代码版本、时间窗和操作人。
- 历史文档若与当前代码或主目录文档冲突，以当前代码、13/14/18/28 号文档为准。
- `evidence/` 继续保留在主 docs 层级，因为当前状态页和 live gate 仍直接引用这些不可变记录。
