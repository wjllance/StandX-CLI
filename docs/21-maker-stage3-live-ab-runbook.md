# Maker Stage 3 v0 live canary and A/B runbook

本手册把 renewed live gate 应用到阶段 3 v0（size skew，退化为加仓侧压制）。
流程与 [19-maker-stage2-live-ab-runbook.md](19-maker-stage2-live-ab-runbook.md)
相同，本文只记录阶段 3 的差异与本次授权；未提及的章节（应急处置、webhook
探针、bounded canary 判定顺序）以 19 号手册为准，symbol 一律替换为 HYPE-USD。

Named online operator：**release owner**。Live work must not begin until the
release record contains this exact authorization:

> 授权执行 HYPE-USD size=0.1 max_position=1.0 的阶段3 canary 与4小时A/B

该文本仅授权 HYPE-USD、`size=0.1`、一档、`max_position=1.0`。不授权其他
symbol、更大敞口、主动库存退出或自动平仓。判定标准（预注册）见
[18-maker-strategy-roadmap.md](18-maker-strategy-roadmap.md) 阶段 3 验收标准。

## Frozen artifacts and preflight

- Release commit：`c4fc893fb86d8c16193017a6dd047b097c57b2d0`
  （stage3 v0 commit1–3 + 路线图快照，工作树干净）。
- Frozen arm configs（仅 `[size_skew].enabled` 一行不同，编排器 preflight
  已接受该形态）：
  - `examples/maker-stage3-hype-baseline.toml`
    sha256 `cccb5610c086aef9e2fb2b8f1d38266983f3acb877002a27f486f7fd456857db`
  - `examples/maker-stage3-hype-candidate.toml`
    sha256 `1e12bf17e35ad9c8105cbb733cbad21d28554677f62f59f38d1e76a798491dda`
- 本地冻结 binary：`target/release/standx`（canary 用），sha256
  `22a86c60d4caa9f5089aba8a5420784ce2b1c6e16907ce6dd38985ee4334b17f`。
  A/B 容器内 binary 以镜像构建记录为准（同一 commit）。
- 部署沿用阶段 2 HYPE 的 docker 路径：`deploy/docker/` 的 `ab-hype`
  profile，`env_file=/etc/standx/maker-stage2-hype-ab.env`。阶段 3 仅需把该
  env 中的两条配置路径改指 stage3 文件；
  `STANDX_STAGE2_ARM_SECONDS=14400`（4h 臂）与
  `STANDX_STAGE2_ARM_MAX_SECONDS=21600` 已就位。
- 场馆 metadata（2026-07-21 新鲜 `standx -o json market symbols` 核对）：
  `price_tick_decimals=3`、`qty_tick_decimals=2`、`min_order_qty=0.1`，与
  env 的 `STANDX_BASELINE_*` 一致。
- 离线证据（2026-07-21，commit c4fc893）：workspace tests 481 passed / 0
  failed；strict Clippy 干净；`cargo fmt --check` 通过；
  `py_compile scripts/openobserve_dashboard.py` 通过；编排器
  `STANDX_STAGE2_VALIDATE_ONLY=1` 通过（pair 形态 size_skew.enabled-only）。
- Candidate paper run：run_id `stage3-paper-20260721T012548Z`（35 分钟，
  candidate 配置，703 cycles 完整、无 panic / 无不变量违规，manifest
  `valid: true`）。
- canary 期间 XAG/HYPE 两条 A/B 容器与任何手工 live maker 全部停止；锁路径
  为容器本地（docker 部署的既有取舍，见 deploy/docker/README.md）。

## Bounded canary（HYPE-USD）

确认 `orders=[]` / `positions=[]` 后执行场馆最小 `ws-command-canary`，保留
完整 create/cancel 关联链；随后用 **candidate** 配置做 15 秒受控断流演练
（fail-safe 停机演习，非重连演习，语义见 19 号手册）：

```bash
export STANDX_ENABLE_LIVE_MAKER=1
target/release/standx --output json maker ws-command-canary HYPE-USD

export STANDX_RUN_ID="stage3-canary-$(date -u +%Y%m%dT%H%M%SZ)"
scripts/run_maker_observed.sh target/release/standx --output json maker run HYPE-USD \
  --maker-config examples/maker-stage3-hype-candidate.toml --live \
  --controlled-disconnect-after 15
```

期望序列：order-response fault observed → frozen → maker cleanup/empty book →
fail-safe shutdown（非零退出是演习预期结果）。任何残余订单、非零终仓或
cleanup 失败 → 走 19 号手册应急处置（symbol 换 HYPE-USD），本次 run 标记
失败，重试需要新的精确授权。

## Four-hour automatic A/B

Canary 证据接受后：

```bash
cd deploy/docker
docker compose --profile ab-hype up -d --build   # 镜像按 c4fc893 重建
docker compose --profile ab-hype logs -f
```

编排器 baseline 先行、candidate 随后交替；每臂 4 小时最小时长 + SIGUSR1
wind-down 换臂，换臂前 manifest validate + 独立空订单/空仓检查。判定按 18
号文档阶段 3 验收标准：样本外 `p95 |position|` 降 ≥15% 或
`|position| >= 70% max_position` 时间降 ≥25%；net PnL ≥ 基线 95%；时间加权
双边 uptime 降幅 ≤3pp；主动退出次数与总 taker exit cost 不高于基线。比较
窗口须同时覆盖平静与趋势时段，趋势不足则延长采集。

**注意**：不要把 `--controlled-disconnect-after` 传给 A/B 编排器（提前退出的
臂会被判 critical stop）。
