# Stage 3 v1 组合候选实盘时间片 A/B 判定报告（2026-07-24）

判定对象：阶段 3 v1 组合候选——非线性 price skew（`[nonlinear_skew]`
boost=3.0 cap=12.0）+ 外部价防御门（`[external_guard]` enter=6 exit=3
max_age=5000ms basis_half_life=300s），一个 release、两个独立开关。
判据预注册于 [22-maker-stage3v1-guard-design.md](../22-maker-stage3v1-guard-design.md)
"验收判据"节（uptime ≥80% 绝对值系 release owner 2026-07-22 裁决修订）；
执行手册与授权见
[23-maker-stage3v1-live-ab-runbook.md](../23-maker-stage3v1-live-ab-runbook.md)；
gate 与启动记录见
[maker-stage3v1-canary-2026-07-22.md](maker-stage3v1-canary-2026-07-22.md)。

**本报告状态：已裁决（release owner 2026-07-24）——组合不晋级，进入预注册
拆单分支：nonlinear_skew 单开快速 A/B（3 对臂目标 / 4 对硬上限，不重锁）。
guard 本轮不跑；其阈值按反事实回放证据冻结为 enter=10 / exit=5 作为后续
重启基准参数（见末节补充）。**

## 建议判定：组合不晋级，进入预注册拆单分支

8 条预注册判据中 6 条达标、1 条未达（撤单率，pair#3 +54%）、1 条不可判
（net PnL——严格按 18 号市况分类口径，本窗口无纯趋势时段）。组合候选
不成为新基线，生产基线维持 HYPE 静态配置。按 22 号文档预注册分支，建议
以纯配置 A/B（不重锁）重跑占优的一半：**仅 `nonlinear_skew` 单开**（理由
见"事件级归因与拆单选择"）。

## 数据窗口

2026-07-22T17:52Z 启动，2026-07-24T02:06Z 停止（~32.2h，判定数据已足，
release owner 决定终止）。有效臂 8 条（baseline / candidate 各 ~16h），
全部 manifest valid、8 次换臂全部干净（空仓空簿）、零 critical 事件、
零安全不变量违规：

| 臂 | run_id（前缀） | 市况（18 号口径） | fills | net PnL | tw-uptime | p95 \|pos\| | mo30 bps | guard 激活 | 撤单/h |
|---|---|---|---|---|---|---|---|---|---|
| baseline #1 | 20260722T175240Z | fast_vol（halt 62，range 231） | 134 | -0.578 | 96.8% | 0.50 | -5.87 | — | 168 |
| candidate #1 | 20260722T215309Z | unclassified（range 158） | 16 | -0.073 | 90.1% | 0.20 | -4.95 | 11.1% | 130 |
| baseline #2 | 20260723T015336Z | unclassified（range 180） | 34 | -0.186 | 99.2% | 0.60 | -6.10 | — | 110 |
| candidate #2 | 20260723T055406Z | fast_vol（halt 16，range 155） | 12 | +0.018 | 89.1% | 0.20 | -4.26 | 11.5% | 85 |
| baseline #3 | 20260723T095419Z | fast_vol（halt 35，range 198） | 90 | -0.048 | 97.7% | 0.50 | -5.30 | — | 112 |
| candidate #3 | 20260723T135433Z | fast_vol（halt 22，range 305） | 48 | -0.291 | 89.6% | 0.20 | -6.12 | 11.5% | **172** |
| baseline #4 | 20260723T175446Z | fast_vol（halt 70，maxvol 115） | 65 | -0.532 | 96.8% | 0.40 | -9.87 | — | 142 |
| candidate #4 | 20260723T215515Z | unclassified（range 176） | 35 | +0.104 | 82.7% | 0.20 | -4.05 | 18.8% | 124 |

合计：baseline 323 笔 / -1.344；candidate 110 笔 / -0.208。
市况覆盖：两臂均覆盖 fast_vol 与 unclassified；按 18 号分类优先级
（halted + range ≥50bps 优先归 fast_vol），趋势性窗口（candidate#3
net -175bps、baseline#4 net -144.5）均伴随 halted cycle，无臂归入纯
trend——这是 PnL 判据不可判的直接原因（见判据表）。

## 预注册判据逐条结果

| 判据 | 结果 | 说明 |
|---|---|---|
| 全关 ≡ 现行策略（状态网格离线等价） | ✅ | 35 个新增离线测试（含全关/单开×2/双开网格），A/B 期间 live 默认值未变 |
| 无 max_position / band / no-cross / 账本 / generation 安全违规 | ✅ | 全程 max \|pos\| 0.6 < 1.0；8 次换臂 orders=[] positions=[]；零 critical |
| **样本外 p95 \|position\| 降 ≥15%** | ✅ **达标** | candidate 四臂全部 0.20 vs baseline 0.40–0.60，**降 50–67%** |
| ≥70% max_position 时间降 ≥25% | ✅（参照项） | 两臂均 ≈0%，无恶化 |
| 主动退出次数不高于基线 | ✅ | candidate 2 次 vs baseline 3 次（均为 wind-down 边界退出） |
| 总 taker exit cost 不高于基线 | ✅ | candidate 0.0149 vs baseline 0.0394 |
| net PnL ≥ 基线 95%（须各覆盖一段趋势） | ⚠️ **不可判** | 无纯趋势窗（见上）；方向性读数见下 |
| **tw 双边 uptime ≥80%（绝对值）** | ✅ **达标** | 90.1 / 89.1 / 89.6 / 82.7；candidate#3/#4 开局 guard-hot 曾瞬时跌至 61–76%，收官全部回线 |
| **每 quote-hour 撤单 ≤ 基线 +20%** | ❌ **未达** | 成对：-23% / -23% / **+54%** / -13%；pair#3 超线（机制见下） |
| guard 激活 ≤ lag 预算 3 倍（~2.1%） | ❌ **未达（按预注册记录为设计缺陷输入）** | 激活时间占比 11.1–22.6%；转换事件 170–250 次/4h |

方向性 PnL 读数（非判定依据，仅供拆单参考）：candidate 单笔均损
≈-0.19¢ vs baseline ≈-0.42¢；mo30 candidate -4.05~-6.12 vs baseline
-5.30~-9.87；candidate 总量 PnL -0.208 vs baseline -1.344。市况窗口
不完全匹配且 candidate 早期两臂成交稀疏（12–16 笔），不做结论。

## 未达机制分析（设计缺陷输入）

**撤单率超线的机制**：candidate#3 的 172/h 撤单中 118 次为
`side_suppressed`（guard 激活/释放/换边时对被压侧的撤单），叠加
`mark_moved` 常规轮换。guard 在该臂 250 次转换（~62/h），每次转换伴随
一侧撤单——**转换频率而非激活时长才是 churn 来源**。baseline#3 同窗口
仅 2 次 side_suppressed。这是 guard 迟滞参数（enter=6/exit=3bps）在
跳动行情下往返切换的结构性代价，同时触碰 SIP-5A short-cycle cancels
条款风险线（该条款为撤单判据的设立初衷）。

**guard 超预算**：lag 分析预算按"仅跳变"口径 ~0.7%/天；实测在趋势/跳动
行情段 guard 整段激活（单臂峰值 22.6%），与 paper 冒烟两轮观察
（28–34%）一致。基差 EMA（半衰期 300s）对分钟级持续背离的吸收不够慢，
是预算偏差的已知简化（docs/22 已声明接受）。注：超预算不直接判负，
但与撤单率、uptime 压力同源于 guard 的切换频率。

## 事件级归因与拆单选择

- **nonlinear_skew（占优，建议单跑）**：p95 |pos| 四臂稳定 0.20（baseline
  线性 skew 同期 0.40–0.60）是全程性、无 churn 的尾部治理——guard 仅
  在 11–23% 时间激活，无法解释全程 p95 压制；skew 的中心连续移动不产生
  额外撤单（candidate 臂 `mark_moved` 撤单率与 baseline 相当）。uptime
  代价仅来自中心移动导致的单侧远离 touch，四臂均 ≥82.7%。
- **external_guard（建议暂不单跑）**：防御收益证据混杂——激活期间仍成交
  21 笔（四臂合计），mo30 在 candidate#1/#2/#4（-4.05~-4.95）改善但
  candidate#3（-6.12）与 baseline 臂无差；而成本侧（撤单 churn、uptime
  压力、超预算激活）全部实锤。若未来重录 lag 数据并加宽迟滞带（如
  enter=10/exit=6）降低切换频率，可重新立项。
- 组合被拒时的另一半备选"两半都无信号则阶段 3 收束"不适用：
  nonlinear_skew 的尾部治理信号明确（p95 判据 4/4 臂达标）。

## 安全与运维记录

- 终止路径：release owner 决定后 `docker compose stop`，SIGTERM → 臂内
  freeze/cancel-all，编排器退出；独立复核 orders=[]。
- 残余仓位：终止时 baseline#5（不完整臂，241 cycles，comparison window
  不合格，不进入任何比较）收尾瞬间有 1 笔被动买 fill（0.1 @ 57.959，
  current_run 记账），留下 +0.1 HYPE 多头（~$5.8）。按"不自动平仓"
  原则未动，release owner 选择手动处置（2026-07-24 会话记录）。
- OpenObserve 全程上传连续；deadman alert 无触发；webhook 无未达。

## 后续动作（待 release owner 裁决）

1. 若采纳建议判定：`examples/` 新增 nonlinear-only candidate 配置对
   （baseline 不变、仅 `[nonlinear_skew].enabled` 一行差异，编排器
   preflight case (b) 既有形态），纯配置 A/B 重跑，不重锁；判定沿用 22
   号文档判据（撤单 +20% 与 guard 预算条对无 guard 臂自然失效）。
2. PnL 判据的趋势窗缺口：拆单 A/B 期间继续按统一口径采集，两臂覆盖纯
   趋势时段前不判 PnL。
3. guard 的 lag 信号与迟滞参数结论（超预算 + churn 机制）归档为阶段 4
   （设计储备）的输入。

## 补充：guard 阈值反事实回放与重启参数冻结（2026-07-24，release owner 裁决输入）

用 4 条 candidate 臂实录的 `external_divergence_bps` 逐 cycle 序列回放
guard 状态机（模拟器先经实测校验：enter6/exit3 下模拟激活率与转换次数
和实际逐臂吻合，误差 <0.1pp / 0 次）：

| 臂 | 实测 enter6/exit3 | 反事实 enter10/exit5 | 反事实 enter12/exit6 |
|---|---|---|---|
| candidate#1 | 激活 11.1%，转换 170 | 0.2%，8 次 | 0.0%，2 次 |
| candidate#2 | 11.5%，122 | 0.9%，10 次 | 0.0%，2 次 |
| candidate#3 | 11.5%，250 | 0.4%，10 次 | 0.1%，2 次 |
| candidate#4 | 18.8%，238 | 1.5%，18 次 | 0.4%，4 次 |

结论（owner 2026-07-24 裁决）：

- **guard 重启基准参数冻结为 enter=10bps / exit=5bps**：激活率压至
  0.2–1.5%（回到 ~2.1% 预算线内），转换降至 ~2–4/h（撤单 churn 结构性
  消除）；enter=12 几乎不触发（2–4 次/4h），防御名存实亡，不取。
  依据：lag 分层数据中防御价值集中在 16–32bps 大跳档，enter=6 把网撒
  在信噪比最差的小跳区与日常噪声上。
- **基差半衰期本轮不动（300s）**：待激活事件段（时长×幅度）分析后再定
  是否缩至 60–120s；一次只动一个参数，避免归因混淆。
- **"事件加宽"（guard 激活时危险侧推远 ~10bps 而不压制）归档为 v1.1
  代码级备选**：它能把 uptime 成本归零（带内报价仍计合格深度），但不
  减少撤单 churn（改价=撤旧挂新），且在 16–32bps 大跳档躲不开扫单
  （18bps 远价位仍被碾过）——仅在 enter=10 压不住激活率时重新立项，
  需改核心代码并重锁 gate。
- **已知代价**：激活率 ~1% 后 guard 的经济收益在统计上不可测量，此后
  定位为"成本有界的尾部保险"，判定只看成本侧（uptime/撤单不受威胁）。

## 拆单 A/B 启动记录（skew 单开，2026-07-24）

- 授权文本（release record，release owner 2026-07-24 会话中给出）：

  > 授权执行 HYPE-USD size=0.1 max_position=1.0 的阶段3v1拆单 nonlinear_skew 单开 4小时A/B

- 冻结配置对（编排器 preflight 新增 case (e) 单 nonlinear_skew 翻转，
  case (d) 回归通过、guard 单开半翻转正确拒绝）：
  - baseline `examples/maker-stage3v1-hype-baseline.toml` sha256
    `49c0b58d29b4f9f220683d919748e848a0984c15db283b3a27c2efd16a6bb754`（沿用）
  - candidate `examples/maker-stage3v1-hype-skewonly.toml` sha256
    `8569c74eef271c493afe6f3d57dc0670c7e3c12296edfae5a49c3f63d5a1e90a`
    （与 baseline 恰一行 `[nonlinear_skew].enabled` 差异）
- 规模：**快速验证——3 对臂（~24h）判定目标，4 对臂（~32h）硬上限**
  （release owner 2026-07-24）；判据沿用 22 号文档，撤单 +20% 与 guard
  预算条对无 guard 臂自然失效，遇纯趋势窗判 PnL。
- 启动前置：残余 +0.1 多头已由操作人手动平仓（启动前复核 FLAT）、
  orders=[]、无手工 maker；镜像按 f3adb9b 重建（策略源码与 45311e7
  一致），容器内 validate-only 通过。
- 首臂：`stage2-baseline-20260724T090811Z-49c0b58d29b4`（baseline 先行），
  2026-07-24T09:08Z 起跑，live 健康。

### 事件：auth token 过期致首臂作废与 A/B 中断（2026-07-24 09:24Z，已恢复）

- **经过**：maker 于 09:09Z 发出 `token_expiry_critical` 预警（token
  09:24:03Z 到期，预警提前量 ~15 分钟）；操作人未在窗口内完成重新登录。
  09:24Z token 失效 → 连续 3 次 `Authentication required` cycle 错误 →
  fail-safe 停机；**停机时 maker cleanup 同样因认证失败未能撤单**
  （`residual_orders` critical），编排器判臂提前退出 critical stop
  （exit 75），A/B 中断约 4.8 小时。
- **暴露**：停机期间场馆留有 1 条残余 sell 挂单（58.863，0.1）+
  +0.1 HYPE 多头（臂内 2 笔成交净额），名义合计 ~$12，风险有界。
- **处置（2026-07-24 14:0xZ，操作人重新登录后）**：独立查询确认残余
  状态 → `order cancel-all HYPE-USD` 撤空 → 经操作人授权执行
  reduce-only 市价 sell 0.1 平仓 → 复核 orders=[] positions=[]。
  中途一次 401 插曲：首次重登疑似未含私钥（账户接口 401、公共接口
  正常），重新完整登录后恢复。
- **作废臂**：`stage2-baseline-20260724T090811Z`（348 cycles，2 fills）
  manifest 已显式 `invalidate`（reason 记录于 manifest），不进入任何
  比较窗口。
- **恢复**：A/B 于 14:14Z 重启，新 baseline 首臂
  `stage2-baseline-20260724T141428Z-49c0b58d29b4`，启动前置复核
  FLAT + orders=[]，live 健康。
- **教训与流程修订**：① 启动 A/B 前置必须包含 token 剩余有效期检查
  （`standx auth status`，剩余 < 实验窗口长度时先重新登录）——已补入
  [23-maker-stage3v1-live-ab-runbook.md](../23-maker-stage3v1-live-ab-runbook.md)；
  ② token 预警提前量 ~15 分钟对无人值守窗口太短，7 天期 token 应在
  启动前主动刷新而非依赖预警；③ cleanup 依赖有效认证——auth 失效路径
  下 fail-closed 只能保证"不再开新仓"，残余单撤除依赖操作人，runbook
  应急章节已覆盖本次处置路径。
