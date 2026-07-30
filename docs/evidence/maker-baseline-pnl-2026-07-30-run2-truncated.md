# 基线 PnL 采集第二次截断报告：两帧 ack 击穿两个 fail-closed 判定（2026-07-30）

run：`baseline-pnl-20260730T153920Z`，2026-07-30T15:39:20Z 启动，
**15:41:31Z fail-safe 停机（exit 75），存活约 2 分钟**。
代码 `0420fe0`（cleanup 硬化 Phase 1+2 + 两次评审修复）。停机时状态安全：
仓位 FLAT、挂单簿已清、告警全部送达（maker-dev 群 23:40/23:41 + OpenObserve
critical 23:42）。

这是**可用性事故，不是安全 breach**：两次 fail-closed 都是把场馆的正常行为
误判为通道损坏。安全不变量全程成立。

## 时间线（全部 UTC，证据 `var/standx/baseline-pnl-20260730T153920Z.ndjson`）

| 时刻 | 事件 |
|---|---|
| 15:39:20 | 启动，cleanup 空簿通过，FLAT 开跑 |
| 15:40:34 | **事件 1**：cycle 36 对卖单 `11949281438` 发 cancel（request `b9744756`，reason=mark_moved）。场馆回**两帧**：网关 `accepted`（→ cancel_resolved 落账），随后终态帧是 **rejection**（该单已成交：fill sell 0.1 @54.349，15:40:37 经 REST 回填确认）→ projection 判 `contradictory` → fail-closed freeze |
| 15:40:35 | freeze cleanup 走 REST 单查路径（order-response freeze，WS 正在替换），两单确认 canceled |
| 15:40:37 | order-response 重连成功（新 session `2e52e09a`），仓位 -0.1 正确归属 |
| 15:41:27 | **事件 2**：position_reconciliation freeze（cycle_invalidation：account update 使活动 cycle 失效） |
| 15:41:28 | cleanup attempt 1：WS 撤 `11949283729`，drain 只收到 `accepted`（非 success）→ 降级 REST 单查确认 canceled；同时发现迟到可见单 `11949300497` → residual，重试 |
| 15:41:30 | attempt 2：WS 撤 `11949300497`，同样 `accepted` → REST 单查确认 canceled → complete |
| 15:41:31 | **leftover replay 判 `orphan_current_run`（request `3f31b25c`）→ fail-closed → cleanup 判失败 → 停机**。`residual_position: flat` |

## 根因

**场馆 ws-api 对一个 `order:cancel` 会回两帧**：网关 `accepted` + 终态结果
（正常撤销是 `success`；撤销与成交竞态时是 `rejection`）。这是本次 run 两分钟
内两次目击的实证行为，与设计假设"一个 request_id 恰好一帧响应"直接冲突。

- **事件 1（`b9744756`）**：常规 cycle 撤单与成交竞态 → accepted + rejected
  两帧。projection 把第二帧判为 `Contradictory`（已记录的 cancel_resolved 与
  rejection 矛盾）→ freeze。对 maker 来说撤单/成交竞态是日常事件，这个判定把
  常态变成了 fail-closed。
- **事件 2（`3f31b25c`）**：cleanup WS 撤单同样只收到 `accepted`（两个
  `confirmed_by=query_order` 可证），其第二帧（终态 `success`）在后一个
  attempt 的 drain 窗口到达——该 request_id 已从 pending 释放，于是进了
  leftover。replay 路径（`replay_leftover_responses`，0420fe0）**不查
  cleanup 墓碑**，projection 从未登记过 cleanup 自己 mint 的 request_id →
  `Orphan` → fail-closed → 停机。`3f31b25c` 在本 run 日志中无任何其他出现，
  与"attempt 1 撤单的第二帧"推断一致（不能 100% 排除是跨 session 陈旧帧；
  cleanup 当时未记录自己 mint 的 request_id，本次修复已补此遥测）。

旁证：07-28 首轮 run 跑了 35.9h、391 笔成交、数千次撤单，**零次**此类
contradictory/orphan 事件；本次 90 秒内两次。两帧行为可能是场馆侧新近变化，
也需要继续观察。

## 修复（本分支）

1. **Fix A（standx-maker）**：`classify_response` 对已决议（CancelResolved）
   的 cancel 的第二帧一律判 `late_known`（幂等可丢），无论 accepted 与否。
   依据：cancel 的决议已落账，订单真实状态由账户流 / `/api/query_order`
   独立确立，第二帧不携带新信息。**place 决议保持严格**（accepted-then-
   rejected 的 place 仍 `Contradictory` fail-closed）。
2. **Fix B（standx-cli）**：cleanup drain 对墓碑覆盖的 request_id（本轮或
   上一轮 cleanup mint 的）在捕获时直接丢弃（计数诊断），不再作为 leftover
   交给 replay。同时消除"cleanup 自己的第二帧"与"跨 attempt 迟到帧"两类
   orphan。
3. **遥测**：`maker_cleanup` 事件 `orders[]` 新增 `ws_request_id`，cleanup
   mint 的 request_id 与订单的对应关系从此可查证。

两个修复的回归测试均经变异验证（摘除实现后测试确实变红）：
`post_resolution_cancel_ack_is_idempotent_even_when_rejected`、
`ws_drain_drops_tombstoned_cleanup_frame_instead_of_leftovering_it`。

## 仍开放的问题

- **Fix A 的残余窗口（对抗复审 finding）**：Fix A 丢弃的终态 rejection 帧，
  在已观测语义（"已成交/已撤销"，订单必为终态）下确实无信息；但若场馆存在
  未观测的"撤销被拒但订单仍活"的 rejection 语义，maker 会在 CancelSubmitted
  已把该档从账上释放的前提下继续报价，可能在同一档重复挂单（敞口最高 2×），
  直到 30 秒 REST audit 的 `unexpected_rest_open_order_ids` 发现并以
  position_reconciliation freeze 兜底。即安全边际从"立即 freeze"收窄为
  "最长一个 audit 周期的重复敞口"。当前无证据表明该语义存在；一旦出现
  该类 freeze，应用本遥测（`ws_request_id` + 撤单原因）对照确认。
- 若未来目击到**既非 cleanup mint、也非本 run 登记**的响应帧（真·跨 session
  陈旧帧），当前仍会 fail-closed。这是保留的安全姿态；出现时用
  `ws_request_id` 遥测对照确认来源后再议。
- 事件 1 的 freeze 本身是"撤单与成交竞态"的日常化触发——Fix A 后该类不再
  freeze。两帧行为若为场馆新变更，值得向场馆确认协议语义。
- 两帧行为下 WS cleanup 的 `confirmed_by=ws_success` 路径实际上很少命中：
  drain 在网关 `accepted` 帧即释放 request_id 并降级 REST 单查，终态
  `success` 帧作为 tombstoned 帧丢弃。行为正确但 `ws_success` 将近乎不出现，
  且每次 freeze cleanup 会对 WS 已撤成的单多发一次幂等的 REST `cancel_orders`。

## 处置记录

- 按 [27 号手册](../27-maker-baseline-pnl-collection-runbook.md)：fail-safe
  停机不自动重启，记录原因；重启需修复合入 + 新的授权文本。
- 授权记录（27 号手册"已填授权"节）同步回填本次窗口与截断原因。
