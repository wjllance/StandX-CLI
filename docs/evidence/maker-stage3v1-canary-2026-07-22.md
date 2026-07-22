# Stage 3 v1 组合候选 live gate 重验记录（2026-07-22/23）

阶段 3 v1 组合候选（非线性 price skew + 外部价防御门，一个 release 两个
开关，设计见 [22-maker-stage3v1-guard-design.md](../22-maker-stage3v1-guard-design.md)）
策略代码合并后的 renewed live gate 证据。流程依据
[14-maker-live-gate.md](../14-maker-live-gate.md) 与
[23-maker-stage3v1-live-ab-runbook.md](../23-maker-stage3v1-live-ab-runbook.md)。

- Release commit：`45311e79e7b211d662e8c37b993a47118aa62c06`（工作树干净，
  仅未跟踪的 `.mimocode/` 本地目录）
- Named operator：wujunlin
- 授权文本（release record，2026-07-22 会话中给出）：

  > 授权执行 HYPE-USD size=0.1 max_position=1.0 的阶段3v1组合候选 canary 与4小时A/B

## 离线证据（commit 45311e7）

- `cargo test --workspace --offline`：全绿（cli 198 / maker 179 / sdk 75 /
  integration 13+31+2，2 个 credential e2e 照旧 ignored）。
- `cargo clippy --workspace --all-targets --offline -- -D warnings`：干净。
- `cargo fmt --all -- --check`：通过；`py_compile openobserve_dashboard.py`：通过。
- 编排器 preflight 接受 stage3v1 配置对（case (d)：`[nonlinear_skew].enabled`
  + `[external_guard].enabled` 双开关翻转），本地
  `STANDX_STAGE2_VALIDATE_ONLY=1` 通过：
  - baseline `maker-stage3v1-hype-baseline.toml` sha256
    `49c0b58d29b4f9f220683d919748e848a0984c15db283b3a27c2efd16a6bb754`
  - candidate `maker-stage3v1-hype-candidate.toml` sha256
    `44a6b19d8eef7918c40e9cbf3b8ec8faf49c44559dc1a56e5cff0ed41a9cf3e8`
  - 逐行 diff 恰为两条 `enabled = false → true`。
- 新鲜 venue metadata（2026-07-22 `standx -o json market symbols`）：
  `price_tick_decimals=3`、`qty_tick_decimals=2`、`min_order_qty=0.1`，与
  `STANDX_BASELINE_*` 一致。
- 冻结 binary（canary 用 `target/release/standx`，45311e7 release 构建）
  sha256 `d5450106841b21397fa3c556ad4abe54c47fd41f36680ebdc392d35d44ecc303`。

## Webhook 探针

- `scripts/test_maker_stage2_webhooks.py` 四类探针（stop_loss / position_risk /
  equity / margin）全部发送成功，`test_id=stage2-webhook-57d86e06692f`。
- 操作人已在同一接收端人工确认四条全部收到（2026-07-22）。

## ws-command-canary（HYPE-USD，2026-07-22T16:31Z）

Preflight `orders=[] positions=[]`（另确认无 A/B 容器、无手工 live maker
进程）后执行，关联链完整：

| 环节 | 值 |
|---|---|
| client_order_id | `sxmk-canary-7fdc546b54ff` |
| create request_id | `8217835c-594b-4d92-9034-2265e8cbbefe` → accepted (code 0) |
| venue order_id | `11754673209`（REST 可见） |
| cancel request_id | `42e6a68b-3416-46b2-bd11-1ba29153f2d7` → accepted (code 0) |
| REST absence | verified |
| 终仓 | 0.0（verified） |

## 受控断流演练（candidate 双开配置，`--controlled-disconnect-after 15`）

首轮 `stage3v1-canary-20260722T163125Z` 行为序列正确但本地采集未带
`STANDX_BASELINE_*`，manifest `symbol_metadata_complete` 不通过，仅作过程
参考；补 env 后重跑：

run_id `stage3v1-canary-20260722T163237Z`，序列与预期完全一致：

1. `16:32:xx` lifecycle started（LIVE HYPE-USD）
2. `risk_notification order_response/disconnected_frozen`（15s 故障注入，
   placements frozen）
3. `reconnect_unavailable`（受控注入要求 fail-safe）
4. `fail_safe/stopped` + maker cleanup → `remaining_maker_orders=0` 空簿
5. 退出码 75（fail-safe 演习预期结果，非失败）

v1 专项确认：末个 cycle_summary（cycle 7）`guard_enabled=true`、
`external_basis_bps=-8.94`（慢 EMA 已初始化）、
`external_divergence_bps=0.02`（超额背离 ≈0）、`guard_active=false`——
静态基差未触发闩锁，基差扣除设计在 live 装配下行为正确。演练后独立复核
`orders=[] positions=[]`。

Manifest：除 `baseline_eligible` 外全部 checks 通过；
`baseline_eligible=false` 仅因 exit 75，与历史 canary manifest 形态一致。

## Candidate paper run

- （35 分钟 paper run 进行中，完成后回填：run_id、cycle 完整性、manifest
  validity。）

## 判定

（待 paper run 与 webhook 确认后回填。）
