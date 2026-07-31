# 基线 PnL 采集截断报告：两帧 ack 击穿 fail-closed 判定（2026-07-30/31）

> 本文档原记录第二次 run（run2，15:41 截断）。第三次 run（run3，
> `baseline-pnl-20260730T163544Z`）于 2026-07-31T01:26:53Z 再次被同一协议
> 行为的 **place 侧变体**击杀，附在后半部分。

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

---

# run3 截断（2026-07-31T01:26:53Z）：place 侧两帧经 leftover replay 击杀

run：`baseline-pnl-20260730T163544Z`，存活约 51 分钟（99 笔成交后才出事，
期间 cycle 459 时已目击 2 次 place 侧 `contradictory` freeze 并自恢复）。

## 事故链

1. 01:26:52 一笔 buy fill 0.1（仓位 -0.2 → -0.1），账户事件使 cycle 失效 →
   position_reconciliation freeze；
2. freeze cleanup 正常完成（挂单簿清空）；
3. cleanup drain 捕获 leftover：买单 place（request `6756a3ca`）的第二帧
   终态 `"alo order rejected"`——该 place 的网关 `accepted` 已落账
   （slot 等场馆确认中），终态帧 ALO 拒单（would-cross）在 freeze 窗口到达；
4. replay 经 `classify_response` 判 `Contradictory`（place + awaiting_venue）
   → cleanup 判失败 → exit 75；
5. 停机时 **-0.10 HYPE 空头 handoff**（venue 确认 short -0.10，无挂单），
   由 owner 手动处置。

市场背景：HYPE 当小时 54.6 → 56.1（+2.8%），波动放大 would-cross 竞态，
ALO 拒单的两帧形态从"罕见"变成"每小时数次"。

## 根因（place 侧）

与 cancel 侧同一协议行为：`order:new` 同样是网关 `accepted` + 终态帧
两帧。ALO 挂单被 would-cross 拒掉时，终态帧是 rejection——对 maker 这是
日常竞态，但 `classify_response` 把"已 PlaceAccepted 且 slot 未关闭"的
rejection 一律判 `Contradictory` fail-closed。两帧里真正需要区分的只有：
**账户流是否已展示过这张单**——展示过才是真的双通道矛盾（VenueContradiction）。

## 修复（Fix C，本分支）

- `classify_response`：`PlaceAccepted` 墓碑 + 终态 rejection，**账户流未
  展示** → `Matched{AwaitingVenue}`，由 apply 路径按普通异步拒单落账
  `PlaceRejected`（释放档位，记 `place_rejected_async` 日志）；**已展示**
  → `VenueContradiction`（fail-closed 不变）。
- "账户流是否展示过"记录在 **completed 墓碑新增的 `venue_observed`** 上：
  收养（`match_pending_slot` / `completed_place_slot`）时置位。pending 条目
  承担不了这个信号——收养即关闭 slot、条目 settle 并被丢弃。
- cancel 侧（Fix A）、cleanup 墓碑侧（Fix B）、place 严格侧
  （`PlaceRejected` 墓碑 + acceptance 仍 `Contradictory`）行为不变。

测试：`two_frame_place_rejection_without_venue_observation_is_async_rejection`
（含 slot 释放断言）与 `two_frame_place_rejection_after_venue_observation_
remains_fail_closed`，外加 maker-core 钉住表两个新用例；两处变异验证
（摘除 classify 分支 / 摘除 apply 臂）均确认测试变红。旧用例
`contradictory_replay_for_completed_request_remains_fail_closed` 按新语义
改写——它钉的正是被场馆协议证伪的旧假设。

## run3 的残余窗口（同 Fix A 复审结论）

若场馆存在"ALO 拒单但订单实际挂上"的未观测语义，Fix C 会在账户流未展示的
前提下释放档位，最长一个 30 秒 REST audit 周期后由
`unexpected_rest_open_order_ids` freeze 兜底。当前无证据表明该语义存在。
