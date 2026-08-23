# 35. Maker skew boost 幅度 A/B（BTC-USD，2026-08-21）

实验规程见 [28-experiment-protocol.md](28-experiment-protocol.md)（四件套、判定词汇、
未判项登记）。本实验是 docs/evidence/maker-toxic-fill-attribution-2026-08-21.md
（毒性成交归因）的直接后续。

## 背景与机制

归因结论（2026-08-21）：tail 成交是 **fresh**（报价挂进了已在漂移的市场，不是 stale
残留），且按加仓/减仓侧拆分后：

- 加仓侧（open/add）mo30 均值 −8.66 / −7.11bps：**已实现亏损**，加仓保护不足。
- 减仓侧（reduce）mo300 均值 −11.38bps：**机会成本**，减仓报价离中心太远，
  库存退出的「尽快回平」目标被 skew 推远的报价拖累。

同一个旋钮 `boost` 耦合了这两端：

```
center_shift = skew_bps(8) × boost × (|pos| / max_position)，封顶 cap_bps(12)
```

- `boost = 3.0`（baseline）：1 单位库存（0.0005，inv_ratio=0.25）时 shift=6bps；
  加仓侧距 mark = 6 + 半价差 4 = 10bps，减仓侧 = 6 − 4 = 2bps。
- `boost = 6.0`（candidate）：1 单位库存时 shift=12bps（顶 cap）；
  加仓侧 12 + 4 = 16bps，减仓侧 12 − 4 = 8bps。

（以持多仓为例：中心下移 shift，减仓侧卖单 = mark − shift + 半价差，加仓侧买单
= mark − shift − 半价差；空仓镜像。）
两臂的**实际差别只在恰好持有 1 单位库存的状态**：≥2 单位（inv_ratio≥0.5）时
两臂都顶到 cap=12bps，报价完全相同。这把实验的解释域收窄为「1 单位库存态的
skew 幅度」，是刻意的——样本集中、因果可读。

`cap_bps` 不变，band 预算红线不变：spread(8) + nonlinear.cap(12) +
external.cap(8) + micro.cap(2) = 30 ≤ band(30)。

## 臂差（单行配置）

`examples/maker-btc-baseline.toml` vs `examples/maker-btc-boost6.toml`，
仅 `[nonlinear_skew]` 内 `boost = 3.0` → `boost = 6.0` 一行不同，其余逐字节相同
（编排器 preflight case (i) 强制，注释差异也会被拒）。

- baseline sha256：`ba8849df2bff60289ee00bdc55c1e53e4769d1c7c3b76526510c1bcffaa7b9a7`
- candidate sha256：`75ba46600810f8845f76c64226c43063d688eb9f3b1160640d88c211a3a867f1`

## 编排器配套改动（本实验首次）

`scripts/run_maker_stage2_ab.sh`：

1. 符号白名单加入 BTC-USD（此前冻结为 XAG-USD/HYPE-USD）。
2. preflight 新增 case (i)：`[nonlinear_skew].boost` 幅度对（3.0→6.0），
   nonlinear_skew 必须在两臂都 enabled（否则测的是零）。已自测：正例通过；
   boost=4.0、注释漂移、字节相同三个反例均 exit=64 拒绝。回归：既有 stage2
   HYPE 对与 guard HYPE 对仍通过。
3. `run_arm` 透传 maker 的 `--alert-webhook`（live 启动门要求；此前编排器不传，
   maker 的 deadman/风险告警没有推送通道）。复用 `STANDX_SUPERVISOR_WEBHOOK`
   与 `STANDX_SUPERVISOR_WEBHOOK_FORMAT`。

### 预注册判据（2026-08-21 入库，开跑后冻结）

- 目标函数：少亏——candidate 相对 baseline 压低毒性成交的已实现亏损，不要求转正。
- 臂长与样本量：7200s/臂（编排器默认），3 对 6 臂（~12h 墙钟）。BTC 实测
  ~34 笔/h → 每臂约 70 笔。单臂成交 <40 笔时该臂标记样本不足，是否顺延补对
  由裁决人决定（判据本身不改）。
- 裁决人：release owner。

**运维门槛（任一不过 → rejected）**
- [ ] 双臂 uptime ≥ 50%（绝对值；run3/run4 在 max_divergence=8 下的实测水平之上取整）
- [ ] 零安全违规（无未解释仓位失配、无残余单、无 fail-open；臂间必须 FLAT 切换）
- [ ] 每臂 manifest valid，cycle 序列完整

**经济门槛**
- [ ] candidate 净 PnL/笔 优于 baseline（pair 内相对比较）
- [ ] candidate 的 open/add 侧 mo30 均值相对 baseline 改善 ≥ 1bps

**记录项（不作门槛，但必须写进判定报告）**
- reduce 侧 mo300 均值的臂间变化（机会成本方向预期为改善，若恶化须量化）

**红线（越线即 rejected，与门槛分开写）**
- 加仓侧在 1 单位库存态被推至 16bps（SIP-5A 10bps 合格带之外）是**已登记的已知
  代价**：SIP 收入≈0 已被 owner 关闭为判据（docs/28 未判项表），且加仓侧被压远
  正是本机制的保护意图——不构成 rejected。
- 但减仓侧必须始终在 10bps 带内（1 单位 = |12−4|=8bps；≥2 单位与基线相同顶
  cap）。若实测减仓侧也被推出带，说明几何假设错误 → rejected。

**明确不作为晋级条件的指标（必填）**

| 指标 | 为什么不作条件 | 谁在什么时候补 |
|------|----------------|----------------|
| 绝对净 PnL 水平 | 臂间市况不可比，A/B 只回答相对问题 | 已登记在 docs/28 未判项表「冻结基线净 PnL」，走 27 号手册单臂采集 |
| SIP-5A $/MH 收入 | owner 已裁决关闭（当前规模收入≈0） | 规模量级变化时重开（docs/28 未判项表，状态：已关闭） |
| funding / 费率项 | markout 口径不含 funding，与本机制无关 | 不需要补 |

判定词汇只用：`accepted` / `rejected` / `ab_completed_not_accepted` /
`rejected_split_branch`。

## 授权与风险边界

授权文本（owner，2026-08-21）：「现在就做这个A/B」（前序：tail fresh/stale 测量
完成后立即执行 boost 3→6 A/B）。边界：单 symbol BTC-USD；size=0.0005、
max_position=0.002；stop_loss=10.0 / alert_loss=2.5；库存退出 70%/0.0005/
ALO+IOC；max_divergence_bps=8；窗口 ~12h 墙钟（6 臂 × 2h + 臂间 wind-down）。

## 启动记录

- git sha：`136fb46`（main，本立项 commit；本地 main 领先 origin/main，推送待 owner）
- 首臂（baseline）UTC 启动：2026-08-21T15:02:23Z
  （run_id `stage2-baseline-20260821T150223Z-ba8849df2bff`，maker lifecycle
  started 15:02:25Z，LIVE + feishu webhook 已确认）
- 编排器 pid 332421，日志 `var/standx/stage2-ab-boost6-20260821T150223Z.orchestrator.log`，
  锁 `var/standx/.ab.lock`；臂长 7200s，臂序 baseline→candidate 交替，臂末
  SIGUSR1 wind-down（reduce-only 市价平仓残余），FLAT 才换臂。
- OO 实时上传已随臂启动（openobserve_ingest --follow，interval 2s）。
- 作废片段：15:01:19Z 首次启动因 OO 凭据漏导出主动 SIGTERM 重开，该片段
  ~2.5 分钟、0 成交（fills_total=0），不进入任何分析。
- 前置状态：run5（`btc-first-window-20260821T1345Z`，配置同 baseline）已于
  15:00:01Z 优雅停止，residual flat，exit=0，66 fills / 75min / PnL -0.68；
  独立复核 `account positions` / `account orders` 均为 `[]`。

## 最终状态（2026-08-23 收尾）

- **判定：`ab_completed_not_accepted`**，判定报告
  [evidence/maker-skew-boost-ab-verdict-2026-08-23.md](evidence/maker-skew-boost-ab-verdict-2026-08-23.md)
  （不可变）。12 对 24 条完整臂，运维门槛全过，两条经济门槛均不满足。
- 停机：owner 2026-08-23 裁决「停」，编排器 15:51Z SIGTERM，末臂 cleanup 后
  独立复核 FLAT。停机时在跑臂（baseline-150730Z，44min 3 fills）为未完成片段，
  不入分析。
- 每日记录 cron 已随停机删除。配置无回滚（baseline 全程未动）。
