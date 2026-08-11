# Stage 3 v0 live gate 重验记录（2026-07-21）

阶段 3 v0（size skew → 退化为加仓侧压制）策略代码合并后的 renewed live gate
证据。流程依据 [14-maker-live-gate.md](../14-maker-live-gate.md) 与
[21-maker-stage3-live-ab-runbook.md](../archive/2026-07-maker/21-maker-stage3-live-ab-runbook.md)。

- Release commit：`c4fc893fb86d8c16193017a6dd047b097c57b2d0`（工作树干净）
- Named operator：release owner
- 授权文本（release record）：

  > 授权执行 HYPE-USD size=0.1 max_position=1.0 的阶段3 canary 与4小时A/B

## 离线证据（commit c4fc893）

- `cargo test --workspace --offline`：481 passed / 0 failed（exit 0）。
- `cargo clippy --workspace --all-targets --offline -- -D warnings`：干净。
- `cargo fmt --all -- --check`：通过；`py_compile openobserve_dashboard.py`：通过。
- 编排器 preflight 接受 stage3 配置对（仅 `[size_skew].enabled` 一行差异），
  本地与容器内 `STANDX_STAGE2_VALIDATE_ONLY=1` 均通过：
  - baseline `maker-stage3-hype-baseline.toml` sha256
    `cccb5610c086aef9e2fb2b8f1d38266983f3acb877002a27f486f7fd456857db`
  - candidate `maker-stage3-hype-candidate.toml` sha256
    `1e12bf17e35ad9c8105cbb733cbad21d28554677f62f59f38d1e76a798491dda`
- 新鲜 venue metadata（2026-07-21 `standx -o json market symbols`）：
  `price_tick_decimals=3`、`qty_tick_decimals=2`、`min_order_qty=0.1`，与
  `/etc/standx/maker-stage2-hype-ab.env` 的 `STANDX_BASELINE_*` 一致。
- 冻结 binary（canary 用 `target/release/standx`）sha256
  `22a86c60d4caa9f5089aba8a5420784ce2b1c6e16907ce6dd38985ee4334b17f`。
- A/B 用 docker 镜像按 c4fc893 重建（build-time dirty-tree gate 通过）。
- Candidate paper run：见末节。

## Webhook 探针

- `scripts/test_maker_stage2_webhooks.py` 四类探针（stop_loss / position_risk /
  equity / margin）全部发送成功，`test_id=stage2-webhook-829a685b2927`。
- 操作人已在同一接收端人工确认四条全部收到。

## ws-command-canary（HYPE-USD，2026-07-21T01:26Z）

Preflight `orders=[] positions=[]` 后执行，关联链完整：

| 环节 | 值 |
|---|---|
| client_order_id | `sxmk-canary-a912a08a4c22` |
| create request_id | `6c5c39b4-f78b-45f6-be1a-5cdcfb6bfc7d` → accepted (code 0) |
| venue order_id | `11715278851`（REST 可见） |
| cancel request_id | `890e6b66-6678-4e1b-a197-9e931d386251` → accepted (code 0) |
| REST absence | verified |
| 终仓 | 0.0（verified） |

## 受控断流演练（candidate 配置，`--controlled-disconnect-after 15`）

run_id `stage3-canary-20260721T012635Z`，序列与预期完全一致：

1. `01:26:36` lifecycle started（LIVE HYPE-USD）
2. `01:26:51` `risk_notification order_response/disconnected_frozen`
   （15s 故障注入，placements frozen）
3. `01:26:52` `reconnect_unavailable`（受控注入要求 fail-safe）
4. `01:26:53` `fail_safe/stopped` + maker cleanup → 空簿
5. 退出码 75（fail-safe 演习预期结果，非失败）

Manifest 14/14 checks 全部通过（`symbol_metadata_complete`、
`lifecycle_stopped` 等均 true）；`baseline_eligible=false` 仅因 exit 75，
与 stage2 历史 canary manifest 形态一致。演练后独立复核
`orders=[] positions=[]`。

## Candidate paper run

- 初跑 `stage3-paper-20260721T003917Z`（35 分钟，701 cycle_summary 完整无缺
  无重、5 笔 paper fill、零 panic/不变量违规、wrapper 报告 exit 0）；因本地
  采集未带 `STANDX_BASELINE_*` 且停机事件未落盘，manifest 两项检查不通过，
  仅作过程参考。
- 重跑 `stage3-paper-20260721T012548Z`（35 分钟 / 2101s，703 cycle_summary
  完整无缺无重、lifecycle started+stopped、零 panic/不变量违规、exit 0）：
  **manifest `valid: true`，baseline_eligible=true**。

## A/B 启动（2026-07-21T02:03Z）

- `docker compose --profile ab-hype up -d`（镜像 c4fc893，entrypoint 安装
  deadman alert 后启动编排器）。
- **一次误启动及纠正**：首次 `up` 时 env 残留上一次 A/B 的
  `STANDX_STAGE2_FIRST_ARM=candidate`，candidate 臂先跑了约 2.5 分钟
  （run_id `stage2-candidate-20260721T020148Z-1e12bf17e35a`，703→22 cycles，
  无成交、停机后独立复核 `orders=[] positions=[]`）。立即
  `docker compose stop`（SIGTERM → 正常 freeze/cancel-all），把
  `FIRST_ARM` 改回 `baseline` 后重启。该片段 run 不足 300 cycles，
  `comparison_window_eligible=false`，不进入任何比较窗口，但在此留痕。
- 正式 A/B 首臂：`stage2-baseline-20260721T020339Z-cccb5610c086`
  （baseline，config hash `cccb5610c086…`），live 双边报价正常，
  OpenObserve 实时上传正常。臂长 4h（`STANDX_STAGE2_ARM_SECONDS=14400`），
  wind-down 换臂。

## 判定

Gate 重验通过项：离线工程证据、webhook 可达、ws-command 关联链、受控断流
fail-safe 序列、candidate paper 长跑 manifest 有效、账面独立复核。
A/B 于 2026-07-21T02:03Z 开跑，进入采集阶段；判定按 18 号文档阶段 3 验收
标准执行。

## 事件：baseline 首臂 manifest 误判作废与工具修复（2026-07-21T06:03Z）

- **经过**：baseline 首臂（`stage2-baseline-20260721T020339Z-cccb5610c086`）
  4h 窗口正常交易收官（5069 cycles、22 笔被动成交、net PnL +0.12、时间加权
  uptime 99.4%、wind-down 后空仓空簿 exit 0），但 manifest
  `cycle_sequence_complete` 检查因 1 个重复 cycle_summary（cycle 4174）
  失败，编排器 fail-closed 判臂作废并 critical stop（exit 75）。
- **根因**：05:21:52 cycle 4174 的 summary 发出后，cancel 打在刚成交的订单
  上被场馆拒绝（400 "order is not open"，cancel-after-fill 竞争），运行时按
  fail-closed 设计冻结 → cleanup → order-response 重连（2s 恢复，账本
  17 fills = 17 trades 无重复）→ 重试该 cycle 并第二次发出 summary。这是
  运行时"cycle 失效重试"的既定行为与 manifest"不允许重复 cycle"检查之间的
  契约缺口，非交易故障。臂内另有 9 次 position_reconciliation 冻结（均 3s
  窗口内恢复）未触发重复。
- **处置（release owner 裁决）**：修复证据工具——`maker_run_manifest.py`
  的 `cycle_sequence_complete` 现仅容忍"两次发射之间存在冻结事件
  （`risk_notification` 的 `frozen`/`disconnected_frozen`）"的同号重复，
  记为 `freeze_retried_cycles`；无冻结相邻的重复仍然判失败。补两个测试
  （冻结相邻通过 / 冻结不相邻失败），10/10 通过。commit `7e8c556`。
- **重验**：用修复后工具对原臂重新 finalize + validate：`valid: true`，
  `baseline_eligible=true`，`duplicate_cycles=[4174]` 全部标记为
  freeze-retried；manifest 保留 `invalidated_at=2026-07-21T06:03:52Z` 作为
  审计痕迹。该臂数据计入比较窗口。
- **续跑**：镜像按 7e8c556 重建（后续臂用修复后工具），
  `STANDX_STAGE2_FIRST_ARM=candidate` 从 candidate 臂续跑；之后的循环自动
  回到 baseline。

## 事件 2：市况冻结致缺 cycle，同一契约缺口的镜像形态（2026-07-21T17:5xZ）

- **经过**：candidate 臂（`stage2-candidate-20260721T093136Z-1e12bf17e35a`）
  完成且 manifest 通过；随后的 baseline 臂
  （`stage2-baseline-20260721T133204Z-cccb5610c086`，5496 cycles）在
  wind-down 正常收官（含一次 0.4 HYPE reduce-only 平仓，账面空仓空簿）后，
  因缺 cycle 5153 再被判无效，A/B 中断约 8 小时。
- **根因**：17:15:09–17:15:50 场馆行情传输静默 ~41s
  （`market_data degraded_frozen: feed_idle bad_for_ms=40930`），cycle
  5153 撞上市况冻结被作废且未发任何终态事件（无 summary 无 skip）。与事件
  1 是同一契约缺口的镜像：运行时冻结作废 cycle 可能多发（重试）或少发
  （丢失）终态事件。
- **处置（同一裁决原则）**：`cycle_sequence_complete` 扩展为——缺失 cycle
  在其两侧最近观测 cycle 之间存在"cycle 字段命中 {N, N+1}"的冻结通知时记为
  `freeze_lost_cycles` 并容忍；重复 cycle 的容忍条件同步收紧为冻结通知的
  cycle 字段必须等于该 cycle（含 `degraded_frozen` 事件名）。新增 2 个测试，
  12/12 通过。commit `f259582`。
- **重验**：两条 baseline 臂均 `valid: true`、`baseline_eligible=true`
  （事件 1 的臂在收紧后的规则下依然通过）。两臂数据计入比较窗口。
- **续跑**：镜像按 f259582 重建，candidate 臂
  `stage2-candidate-20260722T020734Z-1e12bf17e35a` 于 2026-07-22T02:07Z
  起跑。

**运维注记**：运行时侧让每个 cycle 必发终态事件（summary/skip）是根治方向，
涉及遥测契约变更与重新 gate，记录为后续候选改进；在此之前工具侧容忍规则
覆盖已观测到的两类冻结形态。

## 观察：candidate #3 的释放阈值闩锁（2026-07-22T11:2xZ，监控记录）

candidate 第三臂（`stage2-candidate-20260722T100833Z-1e12bf17e35a`）运行至
~1.4h 时 tw-uptime 降至 19.6%（轨迹 51.0→24.9→19.6）。机制：仓位钉在
+0.2（恰为释放阈值 0.2×max_position），迟滞需回落至 0.1 才解除加仓侧压制，
而减仓侧成交稀疏（1.4h 仅 8 笔），压制长时间闩锁（最近 100 cycles 激活率
100%）。与前两臂（激活 ~45% / uptime ~53%）相比，这是 v0 二值压制更坏的
失败模式：仓位停在释放阈值附近时双边 uptime 可崩至 ~20%。非安全事件（流
断开均自恢复、无冻结循环），未干预；作为判定输入与 v1（非线性 price skew，
"更陡但不停"）的设计依据存档。
