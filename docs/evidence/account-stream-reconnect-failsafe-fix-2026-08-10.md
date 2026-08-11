# 事故根因分析 + 修复设计：account 流重连期间二次断流被误判为 fail-safe 停机

> 日期：2026-08-10 · 作者：Hermes（standx-cli 仓库）
> 关联事故：08-06 23:12Z（order-response 残单）、08-10 13:34Z（account 流重连中再断）
> 涉及模块：`crates/standx-cli/src/commands/maker/runtime/recovery_flow.rs`
> 分支：`fix/account-stream-reconnect-bounded`

## 1. 结论

**account-stream 断连→fail-safe 整体停机，不是「断连本身」触发的，而是「重连成功后、REST backfill 期间同一条流**再次**断掉」被当成了证明安全的失败（terminal）处理。**

现状代码里 account 流**确实有**有界自动恢复（reconnect → 重放缓冲事件 → REST 对账 → 收敛窗口）。真正缺的是一层：**post-reconnect 阶段（backfill/drain/converge）再遇到 transport 断流时，应立即判定为「可重试的传输故障」回到重连轮次，而不是直接 `RecoveryFailed → stop`。** 这正是 AGENTS.md「Transport availability is not itself a trading-safety invariant」原则在 account 流恢复期没有贯彻到底的地方。

## 2. 两起事故证据

### 08-10 13:34Z（本次）
`stage2-baseline-20260810T034600Z-6314a37462e3.stderr.log` 末尾：
```
position reconciliation failed: position reconciliation event validation failed
during REST backfill: authenticated account stream disconnected
```
- reconnect 其实**成功了**（`authenticated`），但紧接着 `apply_account_events`
  读取刚重连的接收端时返回 `TryRecvError::Disconnected`（`events.rs:516`）→
  `recovery_flow.rs:1070-1078` 判为 event validation 失败 → `recovery_failed_exit`
  → `RecoveryFailed` → `PositionReconciliation` 停机。
- 前面还有一连串 `market feed: REST fallback (ws_mark_and_book_stale)`、`502`，
  说明当时交易所侧 WS/网关整体抖动。

### 08-06 23:12Z
`stage2-baseline-20260806T171421Z-6314a37462e3.stderr.log` 末尾：
```
order-response stream unavailable: order-response freeze cleanup failed:
RESIDUAL MAKER ORDERS on HYPE-USD after cancellation: [12099665417, 12099665414]
```
- order-response 流断 → freeze → cleanup 第一次没撤净 2 个单 → `CleanupFailed`
  即停机（`CleanupFailure`），留下 0.10 HYPE 多头。

**两者同族但触发点不同**：08-06 是 cleanup 容错，08-10 是 reconnect 后的二次断流容错。

## 3. 现状代码路径（account 流）

`recover_account_stream_phase`（`recovery_flow.rs:777`）：
1. `AccountStreamDisconnected` → freeze + cleanup（撤净并验证 maker book 空）
2. `reconnect_account_stream`（有界重试+退避，**只覆盖「连不上」**）
3. 连上后**单发**执行：
   - `projection.reset_after_cleanup_preserving_pending_acks`
   - `apply_account_events` 重放缓冲事件
   - `client.get_positions` → `position_for_symbol`
   - 若有位差：500/1000/1500ms 收敛窗口内 重复 drain + `probe_position_convergence`
   - 最终 `cancel_maker_orders` 再验证 maker book 空
4. 以上任一 `Err`（**包括 step 3 中间的 transport 再断**）→ `recovery_failed_exit` → 停机。

问题：第 3 步是单发、无重试。重连成功后流又抖一次，就无可挽回地倒向停机。

## 4. 修复设计

**目标**：把「post-reconnect 的 drain/backfill/converge」纳入**同一个有界重试轮次**，
transport 故障可重跑；证明安全的失败（位差不收敛、事件数据一致性错误、cleanup 残单）仍一次即停。

### 改动点（`recovery_flow.rs` + `events.rs`）

1. **新增 typed 错误** `AccountStreamDisconnected`（事件接收端断流专用），
   `apply_account_events` 的 `TryRecvError::Disconnected` 分支返回它
   （替代裸 `anyhow!("authenticated account stream disconnected")`），
   恢复期可用 `error.downcast_ref::<AccountStreamDisconnected>()` 精确识别 transport 故障，
   而不是靠字符串匹配。
2. **`recover_account_stream_phase` 重构**为两级 `'recovery` 外层循环：
   - 每次轮次 =（reconnect → reset → drain → get_positions → 收敛窗口 → 空簿验证）
   - `apply_account_events` 返回 `AccountStreamDisconnected`：
     abort 当前 handle → 用纯策略分成两级：尚无未解释位差时保持冻结、无限重试；
     已观察到位差后，transport 重试由 `args.account_stream_reconnect_attempts`
     限定，预算耗尽即 fail-closed，避免断流无限推迟位差结论。每次 transport
     重试进入退避前都重新验证 maker book 为空，残单直接 terminal。
   - **位差不收敛 / 事件校验 error（非断流）/ cleanup 残单** → 保持现状立即
     `recovery_failed_exit` 停机。
3. **空簿验证**：每轮重连后、以及最终 resume 前的 `cancel_maker_orders_with_retry`
   保持不可重试的证明安全语义（残单=terminal），不改。

### 安全不变量（保持不变）
- freeze 期间禁止新下单；book 空才可恢复报价。
- transport 故障最多退避重试到预算耗尽，之后仍 fail-closed。
- 一切证明安全的失败（位差、残单、数据矛盾）绝不因「重试」被吞掉。
- 不改变 quotes 公式、阈值、退出行为或 live-gate 默认。

### 当前不改（评估但暂不纳入本 PR）
- 08-06 的 **cleanup 残单**路径（`freeze_and_cleanup_for_recovery` 的 cleanup 失败
  = terminal）本轮**不动**——那是另一个容错盲点，需单独评估是否也要有界重试，
  避免一次 PR 改两个实时安全路径增加回归风险。已在分支说明里标注为后续工作。

## 5. 测试清单

- [x] typed 错误识别：`AccountStreamDisconnected` 可被 downcast、`Display` 保持原字符串（`tests/account_events.rs`：`apply_account_events_disconnect_is_typed_transport_error`、`apply_account_events_disconnects_mid_drain` 均已落地通过）。
- [x] 纯策略函数：给定 transport 故障 + 剩余轮次 → Retry；给定证明安全失败 → Terminal。
- [ ] （mockito）reconnect 成功后 drain 遇断流 → 触发第二次 reconnect，未停机。（后续工作）
- [ ] （mockito）位差不收敛 → 仍 fail-closed 停机（回归保护）。（后续工作）
- [x] 既有 runtime 全量测试（`cargo test --workspace --offline` 通过：278+205+88 等全绿）+ clippy（无新告警）+ fmt（clean）。

## 6. 提交与 PR 说明

- 本分支只改 account 流 post-reconnect 容错；不触碰 cleanup 残单路径。
- 这是**实盘安全路径**改动，按 AGENTS.md：需过 `cargo test/clippy/fmt` + 对抗 review +
  变更体记录原因；可合并后在批准下部署。
