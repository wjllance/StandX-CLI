# Cleanup 残余判定硬化立项：WS order-response 主判据 + 按单查询兜底（2026-07-30 草案）

状态：**Phase 1 已完成**（REST `cancel_orders` + 按 `order_id` 单查
`/api/query_order` 的 `status`），已合入 main（`ccccdc7`）。安全轨硬化项（5-b 后续），
非 alpha 候选，不占用 live 时间片（纯代码 + 离线验证 + 受监督 canary），可与任何采集/实验并行。

触发事故：[基线 PnL 采集截断报告](evidence/maker-baseline-pnl-2026-07-30.md)
（run `baseline-pnl-20260728T081712Z`，2026-07-29T19:09:57Z fail-safe，exit 75）。

## 事故定性与损失

**这是一次误报性停机。** 事后 `/api/query_order` 单查证实：两张"残余单"
（`11924302689` / `11924302685`）在 `2026-07-29T19:09:41.928Z` 即已
`status=canceled`、`fill_qty=0`——cleanup 的 REST 撤单**第一时间就成功了**。
但残余判定使用的 REST `query_open_orders` 列表在此后 **≥15 秒、6 次重试**里
持续返回陈旧数据，把"已撤成"误判为"撤不掉"，命中"残余 maker 订单 fail-closed"
不变量，进程带着本可正常交接的状态死掉，采集窗口因此从 72h 截断到 35.9h。

同类征兆在当天早些时候已出现过一次：08:17:32 启动清理也报了一次残余
（`11880164451`），只是那次在 3 秒 freeze 窗口内自行恢复，没有造成停机。
**同一根因，两次目击。**

## 现状机制（问题定位）

正常下单/撤单路径（无问题，不动）：

- 走认证 WS `order:new` / `order:cancel`，信封 `request_id` 关联响应
  （`standx-sdk/src/order_response.rs:99-152`），未注册 request_id fail-closed。
- 但 SDK `OrderResponse` 只解析 `code/message/request_id`——场馆文档里
  `status="accepted"`（仅过网关检查）与 `message="success"`（处理完成）两种
  code==0 被 `accepted()` 混为一谈，调用方无法要求"等到处理完成"。

cleanup 路径（问题所在，`standx-cli/src/commands/maker/recovery.rs:424-459`）：

1. REST `query_open_orders` 列 maker 单 → REST `cancel_orders` 批量撤
   （HTTP 200 = 请求被接受；SDK 注释自述 "The accepted response is asynchronous"）；
2. sleep **500ms**（`MAKER_CLEANUP_VERIFY_DELAY`）→ REST `query_open_orders`
   再查，仍在列表 = 残余 → 1s 间隔重试，6 次判失败；
3. **残余判定的唯一真相源是一个实测有 ≥15s 读-写滞后的列表查询。**

WS order-response 通道在 cleanup 时刻大概率仍然健康（本次事故中行情 feed 异常，
order-response 并无异常记录），撤单的场馆权威确认本就可用，却没有被使用。

## 设计

### 1. SDK：`OrderResponse` 分层 accepted / success

- 增解析 `status` 字段；`accepted()`（code==0，现状保留）之外新增
  `is_success()`（`code==0 && message=="success"`，即场馆确认已处理）。
- 传输层解析与错误分类留在 SDK（AGENTS.md 边界），不向 core/CLI 泄漏裸 JSON。

### 2. CLI cleanup：撤单与判定改为三级，逐级降级

- **主判据（WS 健康时）**：撤单走 WS `order:cancel`，按 request_id 关联等待
  `is_success()` 响应（带超时）。拿到 success = 该单已撤，**不再查列表**。
- **第一兜底（WS 不可用/响应超时）**：REST `cancel_orders` 后，残余判定从
  "open-orders 列表没有"改为**按 order_id 单查 `/api/query_order` 的
  `status==canceled`**（`StandXClient::get_order` 现成，SDK 注释写明正是为
  "列表轮询可能漏单"设计的）。单查是点查询，不受列表物化滞后影响。
- **最终兜底（单查也失败/返回非终态）**：保留现有列表扫描，但把
  `MAKER_CLEANUP_VERIFY_DELAY` 从 500ms 提高到可覆盖实测滞后的宽限
  （实测 ≥15s；建议 5s × 重试，总预算与现有 6 次重试对齐），并仍判残余时
  **附上单查到的 status**（如 `canceled@19:09:41`）进 critical 事件，让
  事后核查不用翻 API。

### 3. fail-closed 语义不变

任何一级"判不定"（超时、查询失败、非终态）仍然按残余处理——**本立项只消除
"已撤成却被误判"的误报，不放松真残余的判定**。真残余（撤单请求本身失败、
单查显示非终态）的 fail-closed 行为与现状逐字节一致。

## Phase 1 实现范围（当前变更）

- SDK：`OrderResponse::is_success()` 已增加（`code==0 && message==success`）。
- CLI cleanup：残余判定从“`query_open_orders` 列表为空”改为
  “对每个 maker order 调用 `/api/query_order` 直到进入终态
  （`filled`/`canceled`/`rejected`/`expired`）”。
  - 轮询：首次等待 500ms，之后 1s 间隔，最多 6 次。
  - 终态视为已处理；仍返回 `new`/`open`/`partially_filled`/`untriggered`
    则判为真残余，fail-closed 不变。
  - `maker_cleanup` JSON 遥测事件附带每个单查到的 `status` 与 `updated_at`。
- **迟到单兜底**：单查全部终态后，仍再读一次 open-orders 列表，凡**不在本次
  撤单批次内**的 maker 单一律判残余（外层重试会撤它）。撤单请求发出前场馆刚
  接受、但直到首次快照之后才可见的单子，从不在撤单批次里；少了这一步，
  `runtime::cycle_flow` 恢复窗口末尾那次"再验一遍挂单簿"就形同虚设。批次内的
  单子在这一步被无条件忽略——列表对它们的滞后正是单查要吸收的东西。
- **reconnect 快照同步改造**：`validate_reconnect_snapshot` 不再直接用列表判残余，
  改为先经 `confirm_residual_maker_orders()` 逐个单查确认。否则误报只是从 cleanup
  搬到了 reconnect：cleanup 现在 ~0.5s 就返回成功，列表反而更可能还是陈旧的，
  reconnect 校验会以"maker orders appeared after cleanup"打死同一个 run。
- 主判据（WS `order:cancel` 等 `is_success()`）保留在设计中，尚未实现。

## 测试要求（离线确定性）

- accepted vs success 分层解析（含缺 `status` 字段的旧式响应向后兼容）；
- 撤单已 success 但列表仍陈旧 → **不判残余**（本次事故的回归测试）；
- WS 不可用 → 降级 REST + 单查；单查 `canceled` → 不判残余；
- 单查返回 `filled`（撤单前已被吃）→ 走既有"order before position"账本路径，
  不算残余也不算误撤；
- 真残余（撤单被拒绝/单查持续 `open`）→ 仍然 fail-closed，critical 事件带
  单查 status；
- 撤单批次外的迟到单（首次快照之后才可见）→ 仍然 fail-closed，重试把它撤掉；
- reconnect 快照遇到陈旧列表里已 `canceled` 的 maker 单 → **不判残余**；同一次
  校验里真正 `open` 的单仍然打死重连；
- 全部重试耗尽后的错误文本仍指向 `standx order cancel-all`（现状行为）。

## 验收

- `cargo test --workspace --offline` 全绿 + 上述新增用例；
- clippy / fmt 按 AGENTS.md 门禁；
- **受监督 canary 一次**（[14 号文档](14-maker-live-gate.md)）：cleanup 位于
  停机路径，offline 测不到场馆滞后，需在 canary 的完整 start→stop→cleanup
  循环里观察一次真实残余判定（重点：停机时 cleanup 日志出现"单查确认
  canceled"路径而非裸列表判定）。

## 明确不做

- 不改正常报价路径的 WS 关联纪律（无问题）。
- 不改 fail-closed 不变量本身（残余单就该停机，改的是判定数据源）。
- 不引入新配置项（宽限值为常量，随代码评审定稿；若实测需要可调再补配置）。
- 不做 venue 侧滞后监控（那是观测项，不在本立项范围）。

## 失效条件

- canary 或 live 出现"WS success 了但单实际未撤"的反例（主判据信任假设破灭，
  退回单查为主并重审）；
- 场馆文档变更使 `message=="success"` 不再是处理完成语义。
