# Stage 3 v1 组合候选实现证据 — 2026-07-23

## Decision

- Status: `implemented_pending_gate`
- 范围：设计文档 [22-maker-stage3v1-guard-design.md](../archive/2026-07-maker/22-maker-stage3v1-guard-design.md)
  的完整实现——非线性 price skew（`[nonlinear_skew]`）+ 外部价防御门
  （`[external_guard]`），一个 release、两个独立开关。
- 立项与判据修订（release owner 2026-07-22）：两机制合并为一个候选；
  **uptime 判据 = 时间加权双边 ≥80% 绝对值**（替代 ≤3pp 相对判据）；组合
  被拒时拆单机制以纯配置 A/B 重跑（不重锁）。
- 本文档只覆盖离线实现证据与 paper 冒烟。canary 重验、live A/B 与其授权
  文本按 runbook 另行执行记录。

## 实现摘要

- core（standx-maker，纯函数/纯状态机，无 I/O 无时钟）：
  - `inventory.rs`: `NonlinearSkewConfig { enabled, boost, cap_bps }` +
    校验（含带内红线 `spread_bps + cap_bps ≤ band_bps`）。
  - `lib.rs`: `skew_center_with`（disabled ≡ 原 `skew_center` 路径；enabled:
    `shift = sign × min(skew_bps × boost × |ratio|, cap_bps)`，无迟滞），
    三个中心计算点（ref_center / desired / reconcile）统一走它。
  - `external_guard.rs`（新）: `GuardController` enter/exit 迟滞 +
    换边即时切换；`None`/过期/非有限样本一律 fail-open 且状态清零。
  - `CycleInput` 增 `nonlinear_skew` + `guard` 两个 typed 输入。
- CLI（standx-cli）：
  - `external_feed.rs`（新）: HL midPx 常驻任务（`activeAssetCtx`，断线
    自愈，绝不停机）+ **`DivergenceBaseline` 慢 EMA 基差扣除**（见下）+
    typed input 归一化。
  - 配置解析/校验、`MakerRunArgs` 穿线、cycle 装配、wait_phase 外部唤醒
    分支（受 1s min-gap 地板约束；换边条件覆盖）、遥测。
  - `cycle_summary` 新增可选字段：`guard_enabled/guard_active/guard_side/
    external_divergence_bps/external_basis_bps/skew_shift_bps`；新增
    `action="external_guard"` 转换事件（激活/释放/换边各一条）。
- 冻结配置对 `examples/maker-stage3v1-hype-{baseline,candidate}.toml`：
  逐行 diff 恰为两条 `enabled = false → true`；候选参数 boost=3.0 /
  cap=12.0 / enter=6 / exit=3 / max_age_ms=5000 / basis_half_life=300s。
- 编排器 `run_maker_stage2_ab.sh` 冻结配置白名单新增 case (d)（两开关
  同时翻转），五种配对形态离线验证（v1 组合/v0/自适应通过；篡改参数/
  半翻转拒绝）。

## 关键设计修订：基差扣除（paper 冒烟捕获）

第一轮 paper 冒烟（candidate 配置，公共行情，无订单）暴露设计缺陷：
HL midPx 与 StandX mark 在 HYPE 上存在 **~-14 至 -15.5bps 的持久静态基差**
（场馆溢价/资金费结构）。原实现以原始水位差触发，guard 启动即永久压制
Buy 侧——若进 A/B 将精确复刻 v0 的 uptime 死法。

修复：CLI 侧 `DivergenceBaseline` 慢 EMA（半衰期 300s，配置项
`basis_half_life_secs`），guard 触发于**超额背离** `excess = raw − basis`。
跳变相对跳变前基线度量（不被自身样本吸收）；首样本初始化基线，启动基差
永不触发。与 lag 分析的"跳变"口径一致。

## 离线验证（全绿）

- `cargo test --workspace --offline`：cli 198 / maker 179 / sdk 75 +
  integration 13 + unit 31 + main 2 + e2e/doc（2 credential e2e 照旧
  ignored）。新增测试 35 个，含：
  - 状态网格等价：两开关全关（含非默认参数的 disabled 配置与
    enabled-but-inactive guard）≡ 现行策略逐 action 相等；
  - nonlinear：boost=1+cap≥skew ≡ 线性逐点相等、陡化/饱和/多空镜像、
    带内红线校验拒绝；
  - guard：方向映射、迟滞边界、换边、fail-open（缺失/过期/NaN、状态不
    跨数据缺口存活）；
  - 组合场景：高仓位(0.8×max) + 外部下跳 → guard 压 Buy、skew 已下移、
    band/no-cross/max_position 全不变量保持、释放后恢复双边；
  - `DivergenceBaseline`：静态基差零穿透、8bps 跳变原样穿透、半衰期
    吸收速率、peek 无副作用；
  - 配置：新 section 解析/部分字段回退、冻结对仅差开关行、契约测试
    （guard 字段 additive、inactive 时键在值 null）。
- clippy `--workspace --all-targets -D warnings`、`cargo fmt --check`、
  `py_compile`（dashboard + lag_analysis）、`bash -n` 编排器：全部干净。

## Paper 冒烟 #2（修复后，2026-07-22T16:0xZ，36 cycles）

- 基线初始化：cycle 1 `basis=-15.52, excess=0.0`，guard 不触发 ✓
- 事件级激活：cycle 5 `excess=-6.83` 越过 enter=6 → 压 Buy；cycle 7
  `excess=-1.83` 低于 exit=3 → 释放（持续 ~6s，事件级而非闩锁级）✓
- 真实上行段：cycle 12 起 `excess +10~+22bps` → 压 Sell，基线缓慢上调
  （-15.6 → -14.3），行情段内持续激活 ✓
- nonlinear skew：pos 0.2 → `skew_shift_bps=4.8`（= 8×3×0.2，公式精确）✓
- 遥测：guard 转换事件 3 条（激活/释放/换边向上）、summary 新字段齐全 ✓
- cycle 序列 0..35 完整（8 个市况 skip 事件均有记录，无缺号）✓
- SIGTERM 干净退出 ✓

**冒烟观察（A/B 预期管理）**：持续单边行情中 guard 会整段激活（本次
~16 cycles），激活时间会超出 lag 数据"仅跳变"口径的 ~0.7%/天预算——判据
中"激活预算 3 倍上限"按此校准；uptime 硬门槛（≥80% 绝对）不受威胁，
因为激活只压单侧且行情段有限。

## 已知边界

- Guard 无法回放（无外部 feed 录制）；replay 只承担关闭等价验证（既定
  分工）。经济价值由 live A/B 判定。
- 基差 EMA 会缓慢吸收持续性真实背离（半衰期 300s）——对秒级跳杀无影响，
  对分钟级持续错价的防御打折；接受为 v0 简化。
- 单 symbol（HYPE）参数；换 symbol 需重录 lag 并重估基差。
