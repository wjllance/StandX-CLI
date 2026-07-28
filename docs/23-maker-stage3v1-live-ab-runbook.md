# Maker Stage 3 v1 combined candidate live canary and A/B runbook

本手册把 renewed live gate 应用到阶段 3 v1 组合候选（非线性 price skew +
外部价防御门，一个 release、两个独立开关，设计见
[22-maker-stage3v1-guard-design.md](22-maker-stage3v1-guard-design.md)）。
流程与 [19-maker-stage2-live-ab-runbook.md](19-maker-stage2-live-ab-runbook.md)
相同，本文只记录 v1 的差异与本次授权；未提及的章节（应急处置、webhook
探针、bounded canary 判定顺序）以 19 号手册为准，symbol 一律为 HYPE-USD。

Named online operator：**wujunlin**。Live work must not begin until the
release record contains this exact authorization:

> 授权执行 HYPE-USD size=0.1 max_position=1.0 的阶段3v1组合候选 canary 与4小时A/B

**授权记录（release record）**：上述精确文本已由 release owner 于
2026-07-22 在会话中给出，授权生效。

该文本仅授权 HYPE-USD、`size=0.1`、一档、`max_position=1.0`、baseline 与
双开组合两臂。不授权其他 symbol、更大敞口、主动库存退出、自动平仓，也不
授权单开任一侧机制的中间臂（组合被拒时的拆单机制重跑是纯配置 A/B，按
路线图规则不重锁，届时另行记录）。判定标准（预注册）见
[22-maker-stage3v1-guard-design.md](22-maker-stage3v1-guard-design.md)
"验收判据"节。

## Frozen artifacts and preflight

- Release commit：`45311e79e7b211d662e8c37b993a47118aa62c06`
  （stage3v1 组合候选，工作树干净，仅未跟踪的 `.mimocode/` 本地目录）。
- Frozen arm configs（逐行 diff 恰为 `[nonlinear_skew].enabled` 与
  `[external_guard].enabled` 两行 false → true，编排器 preflight 白名单
  case (d) 已接受该形态）：
  - `examples/maker-stage3v1-hype-baseline.toml`
    sha256 `49c0b58d29b4f9f220683d919748e848a0984c15db283b3a27c2efd16a6bb754`
  - `examples/maker-stage3v1-hype-candidate.toml`
    sha256 `44a6b19d8eef7918c40e9cbf3b8ec8faf49c44559dc1a56e5cff0ed41a9cf3e8`
- 本地冻结 binary：`target/release/standx`（canary 用，45311e7 release
  构建），sha256
  `d5450106841b21397fa3c556ad4abe54c47fd41f36680ebdc392d35d44ecc303`。
  A/B 容器内 binary 以镜像构建记录为准（同一 commit 重建）。
- Candidate 固定参数（调参窗口外不得变更）：`nonlinear_skew boost=3.0 /
  cap_bps=12.0`（带内红线 spread 8 + cap 12 = 20 ≤ band 30）；
  `external_guard enter_bps=6.0 / exit_bps=3.0 / max_age_ms=5000 /
  basis_half_life_secs=300`。
- 部署沿用阶段 2/3 v0 的 docker 路径：`deploy/docker/` 的 `ab-hype`
  profile，`env_file=/etc/standx/maker-stage2-hype-ab.env`。v1 仅需把该
  env 中的两条配置路径改指 stage3v1 文件；
  `STANDX_STAGE2_ARM_SECONDS=14400`（4h 臂）与
  `STANDX_STAGE2_ARM_MAX_SECONDS=21600` 已就位。
- **新增网络依赖**：candidate 臂的 HL midPx feed（Hyperliquid 公共行情
  WebSocket，无凭证）在容器内必须可达。feed 仅在
  `external_guard.enabled=true` 时启动（baseline 臂不连接）；feed 失效
  fail-open（guard 失活、报价继续），但 A/B 启动前须实测容器到
  Hyperliquid 的连通性，否则 candidate 臂会退化为"guard 全程失活"的
  无效臂。启动后在 candidate 臂日志确认 `guard_enabled=true` 且
  `external_basis_bps` 已初始化（首样本后 `external_divergence_bps` 应
  在 0 附近，静态基差约 -14~-15.5bps 不得触发激活）。
- 场馆 metadata 沿用 21 号手册 2026-07-21 核对值
  （`price_tick_decimals=3`、`qty_tick_decimals=2`、`min_order_qty=0.1`），
  A/B 启动前用 `standx -o json market symbols` 复核一次。
- **auth token 有效期前置（2026-07-24 事件教训）**：A/B 启动前必须
  `standx auth status` 确认 token 剩余有效期覆盖计划的采集窗口（多对
  4h 臂 + 余量），不足则先 `standx auth login`（含私钥）刷新再启动；
  不要依赖 maker 的 `token_expiry_critical` 预警（提前量仅 ~15 分钟，
  无人值守窗口内不足以响应）。token 失效的连带后果是 cleanup 也无法
  撤单（认证依赖），残余单/仓位只能待重新登录后手动处置。
- **FLAT 前置（2026-07-27 补充）**：启动任何 canary / A/B 前必须实测账户为空仓空簿，
  不能只依赖上一轮的收尾记录——阶段 3-guard 轮结束时 baseline#4 留下 -0.1 HYPE 空头
  （按"不自动平仓"原则由 owner 手动处置，见
  [guard 判定报告运维记录](evidence/maker-guard-spinoff-ab-judgment-2026-07-27.md)）。
  复核方式（两条都要看，`positions` 为空但 `orders` 非空同样是阻塞项）：

  ```bash
  standx -o json account positions
  standx -o json account orders
  ```

  非空 → 先手动处置并回填到对应判定报告的运维记录，再启动。
- 离线证据（2026-07-22，commit 45311e7）：workspace tests 全绿
  （cli 198 / maker 179 / sdk 75 / integration 13+31+2，2 个 credential
  e2e 照旧 ignored）；strict Clippy 干净；`cargo fmt --check` 通过；
  `py_compile scripts/openobserve_dashboard.py` 通过；编排器
  `STANDX_STAGE2_VALIDATE_ONLY=1` 通过（pair 形态
  nonlinear_skew.enabled + external_guard.enabled 双开关翻转，case (d)）。
- Candidate paper 冒烟：见
  [maker-stage3v1-implementation-2026-07-23.md](evidence/maker-stage3v1-implementation-2026-07-23.md)
  （基差扣除修复后 36 cycles：基线初始化零穿透、事件级激活/释放/换边、
  skew_shift 公式精确、遥测齐全、SIGTERM 干净退出）。
- canary 期间 XAG/HYPE 两条 A/B 容器与任何手工 live maker 全部停止；锁路径
  为容器本地（docker 部署的既有取舍，见 deploy/docker/README.md）。

## Bounded canary（HYPE-USD）

确认 `orders=[]` / `positions=[]` 后执行场馆最小 `ws-command-canary`，保留
完整 create/cancel 关联链；随后用 **candidate** 配置（双开）做 15 秒受控
断流演练（fail-safe 停机演习，非重连演习，语义见 19 号手册）：

```bash
export STANDX_ENABLE_LIVE_MAKER=1
target/release/standx --output json maker ws-command-canary HYPE-USD

export STANDX_RUN_ID="stage3v1-canary-$(date -u +%Y%m%dT%H%M%SZ)"
scripts/run_maker_observed.sh target/release/standx --output json maker run HYPE-USD \
  --maker-config examples/maker-stage3v1-hype-candidate.toml --live \
  --controlled-disconnect-after 15
```

期望序列：order-response fault observed → frozen → maker cleanup/empty book →
fail-safe shutdown（非零退出是演习预期结果）。v1 额外确认：run 期间
`cycle_summary` 带 `guard_enabled=true`、`external_basis_bps` 已初始化且
guard 未因静态基差闩锁激活（激活只能由超额背离触发，见 22 号文档基差扣除
设计）。任何残余订单、非零终仓、cleanup 失败或 guard 闩锁 → 走 19 号手册
应急处置（symbol 换 HYPE-USD），本次 run 标记失败，重试需要新的精确授权。

## Four-hour automatic A/B

Canary 证据接受后：

```bash
cd deploy/docker
docker compose --profile ab-hype up -d --build   # 镜像按 45311e7 重建
docker compose --profile ab-hype logs -f
```

编排器 baseline 先行、candidate 随后交替；每臂 4 小时最小时长 + SIGUSR1
wind-down 换臂，换臂前 manifest validate + 独立空订单/空仓检查。判定按 22
号文档预注册判据：样本外 `p95 |position|` 降 ≥15% 或
`|position| >= 70% max_position` 时间降 ≥25%；主动退出次数与总 taker
exit cost 不高于基线；net PnL ≥ 基线 95%（两臂各至少覆盖一段趋势时段，
否则不判 PnL）；**时间加权双边 uptime ≥ 80%（绝对值，release owner
2026-07-22 裁决，替代 ≤3pp 相对判据）**；每 quote-hour 撤单数相对基线
增加 ≤20%；guard 激活时间占比与激活次数 ≤ lag 数据预算的 3 倍（超预算
记录为设计缺陷输入，臂照跑）。比较窗口须同时覆盖平静与趋势时段，趋势
不足则延长采集。

组合被拒时按预注册分支执行：用 `external_guard` 转换事件与
`skew_shift_bps` 遥测做事件级归因，拆单机制（仅 `nonlinear_skew` 或仅
`external_guard`）以纯配置 A/B 重跑占优的一半，不重锁；两半都无信号则
阶段 3 收束、回到静态 baseline。

**注意**：不要把 `--controlled-disconnect-after` 传给 A/B 编排器（提前
退出的臂会被判 critical stop）。
