# Guard 独立立项（阶段 3-guard）：enter=10/exit=5 防御门纯配置 A/B 设计与判据预注册

> **归档记录**：本候选已 accepted，阶段已经关闭。本文保留预注册判据和当时的授权记录，
> 不能授权新的 live 运行；当前机制状态见
> [../../18-maker-strategy-roadmap.md](../../18-maker-strategy-roadmap.md)。

**状态：accepted（2026-07-27，release owner 裁决）——guard 并入 HYPE
生产基线（与 skew 共存），阶段 3-guard 关闭。判定报告见
[maker-guard-spinoff-ab-judgment-2026-07-27.md](../../evidence/maker-guard-spinoff-ab-judgment-2026-07-27.md)。**

2026-07-25 立项（release owner 裁决链：v1 组合 rejected_split_branch →
拆单 skew accepted → guard 以冻结参数独立立项重验）。
上游文档：[22-maker-stage3v1-guard-design.md](22-maker-stage3v1-guard-design.md)
（机制设计、红线、fail-open）、
[maker-stage3v1-ab-judgment-2026-07-24.md](../../evidence/maker-stage3v1-ab-judgment-2026-07-24.md)
（组合判定与反事实回放数据）、
[maker-stage3v1-skew-ab-judgment-2026-07-25.md](../../evidence/maker-stage3v1-skew-ab-judgment-2026-07-25.md)
（skew accepted，构成本轮基线）。

## 立项依据

组合轮（enter=6/exit=3）的 guard 失败于成本侧：激活 11.1–22.6%（超 ~2.1%
预算 5–10 倍）、迟滞往返切换 170–250 次/4h 致撤单率 pair#3 +54%。但机制
本身（fail-open、基差扣除、换边即时）无安全缺陷，且 lag 分层数据显示
防御价值集中在 16–32bps 大跳档——enter=6 把网撒在了信噪比最差的小跳区
与日常噪声上。

用 4 条 candidate 臂实录的 `external_divergence_bps` 序列做反事实回放
（模拟器经实测逐臂校验，误差 <0.1pp/0 次）：

| 阈值 | 激活时间占比 | 转换次数/4h |
|---|---|---|
| enter=6/exit=3（实测） | 11.1–18.8% | 122–250 |
| **enter=10/exit=5（本轮冻结）** | **0.2–1.5%** | **8–18（~2–4/h）** |
| enter=12/exit=6 | 0.0–0.4% | 2–4（名存实亡，不取） |

enter=10/exit=5 把激活率压回预算线内、撤单 churn 结构性消除，同时保留
大跳档覆盖。enter=12 几乎不触发，防御名存实亡，不取。

**定位与可判定性（立项即声明）**：激活率 ~1% 后 guard 的经济收益在
统计上不可测量（稀有事件），本机制定位为**成本有界的尾部保险**；判定
只看成本侧与信号质量，不以 PnL/markout 改善作为晋级条件。

## 范围

- **纯配置 A/B，不重锁**：代码与组合轮同一 release（45311e7，已过
  gate）；orchestrator 白名单新增 case (f)（guard 单翻转、skew 两臂
  对称、guard 参数两臂逐字节一致；正例通过、参数篡改/开关不对称均
  正确拒绝）。
- **基线继承**：baseline' = 生产基线（nonlinear_skew 开）+
  `[external_guard]` enter=10/exit=5（关闭）。guard 关闭时参数不参与
  行为（feed 不启动），baseline' 与生产基线逐 action 等价（组合轮
  状态网格测试已覆盖含非默认参数的 disabled 等价）。
- candidate = baseline' 仅 `[external_guard].enabled` 一行差异。
- 冻结配置对：
  - `examples/maker-guard-hype-baseline.toml` sha256
    `a396eb8209c6e398e653c992c564c979ff04bcd5b333f545d71b7b0a88cb0dfb`
  - `examples/maker-guard-hype-candidate.toml` sha256
    `6314a37462e3bfda2cb21f14e503fae4d2997dca449f329de80e7ab22be4b9fc`
- 规模沿用拆单规程：4h 臂、wind-down 换臂，**3 对臂判定目标 /
  4 对臂硬上限**。
- 明确不做：不改 guard 代码、不调基差半衰期（300s；待本轮激活事件段
  分析后再评估 60–120s，一次只动一个参数）、不上"事件加宽"（v1.1
  备选，仅在 enter=10 压不住激活率时重启）。

## 验收判据（预注册，live 时间片 A/B）

- [ ] 全关 ≡ 生产基线逐 action 等价（离线状态网格，组合轮证据沿用）。
- [ ] 无 max_position / band / no-cross / 账本 / generation 安全违规。
- [ ] **tw 双边 uptime ≥80%（绝对值）**，且不相对 baseline 臂显著
  恶化（参照：skew 轮各臂 96.7–99.4%）。
- [ ] **每 quote-hour 撤单 ≤ baseline +20%**（SIP-5A short-cycle 红线；
  组合轮失败项，本轮转换预算 ~2–4/h）。
- [ ] **guard 激活时间占比 ≤ lag 预算 3 倍（~2.1%）**，转换次数记录
  在案；超预算即信号质量缺陷，臂照跑。
- [ ] 主动退出次数与总 taker exit cost 不高于基线。
- [ ] **信号质量（事件级归因）**：激活事件与 ≥10bps 外部跳变的对齐
  分布（命中率）与误报率（激活后 30s 内 StandX mark 未跟随的占比），
  从 `external_guard` 转换事件与 `external_divergence_bps` 遥测计算。
- [ ] net PnL 沿用趋势窗规则（预计仍不可判，记录方向性读数，不作为
  晋级条件——见"定位与可判定性"）。

晋级 = 成本侧全绿（uptime/撤单/激活预算/安全）+ 信号质量可解释
（激活集中在大跳档而非噪声）。晋级后 guard 并入生产基线（与 skew
共存），半衰期评估与 v1.1 备选按其证据另行立项；被拒则 guard 归档，
阶段 3-guard 关闭。

## 启动记录（2026-07-26）

- 授权文本（release record，release owner 2026-07-25 会话中给出）：

  > 授权执行 HYPE-USD size=0.1 max_position=1.0 的阶段3-guard enter10/exit5 单开 4小时A/B

- 启动前置（含 2026-07-24 token 事件后的新前置）：`standx auth status`
  token 剩余 132h（覆盖采集窗口）；orders=[] positions=[]；无其他
  live maker；镜像按 01e914a 重建（策略源码与 45311e7 一致）；容器内
  `STANDX_STAGE2_VALIDATE_ONLY=1` 通过（配置对哈希匹配）。
- 首臂：`stage2-baseline-20260726T015414Z-a396eb8209c6`（baseline 先行，
  skew 在基线中生效、guard 关闭），2026-07-26T01:54Z 起跑，live 健康。
