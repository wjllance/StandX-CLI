# 换品种到 BTC-USD：规模换算、决策记录与首窗口计划（2026-08-21）

状态：`authorized_running`。首窗口已于 2026-08-21T06:37Z 开跑（授权与启动记录见 §7）。

## 1. 为什么换

[mark-best 分母证据（2026-08-20）](evidence/mark-best-spread-denominator-2026-08-20.md) 实测五品种：

| 品种 | 锚定偏置 p50（mark 相对 book mid） | 备注 |
|---|---|---|
| **BTC** | **+0.3bps**（mark ≈ mid） | 分母最健康 |
| HYPE | **+1.9bps**（持续偏在 mid 上方） | 半价差 p99 = 4.7bps 厚尾 |
| ETH | +2.5bps | 持续压高 |
| XAG | mean −4.6 / p50 −0.7 | 左偏长尾、薄盘 |

逆选的物理量是「距 best 的距离」，而我们所有几何量锚在 mark 上——**分母直接框住一切
中心偏移机制的起效空间**。八轮机制迭代（stage2/3/3v1/4/nonlinear/guard/external_skew/
microprice）全跑在最差的分母上，而「换品种」这个变量从未被检验。

tick 精度也支持这个选择：BTC `0.01 / 74,650 = 0.00134 bps/tick`，±10bp 带内约 7,460 档，
基本连续（HYPE 约 72 档；XAG 仅 6 档）。价格粒度不再是约束。

## 2. 规模换算与 owner 裁决

mark 74,650.75（2026-08-21）；`qty tick = min_order_qty = 0.0001 BTC ≈ $7.465`。

| | HYPE 现状 | BTC 等名义 | **采用值** |
|---|---|---|---|
| `size` | 0.1（≈ $7.25） | 0.0001（≈ $7.47） | **0.0002（≈ $14.93）** |
| `max_position` | 1.0（≈ $72.5） | 0.001（≈ $74.7） | **0.002（≈ $149.3）** |
| 库存容量比 | 10× size | 10× size | 10× size |

**owner 裁决 2026-08-21：采用 2× 名义（size = 0.0002）。**（同日被第二次裁决
0.0005 取代，见 §7。）两点理由与代价：

- 等名义的 `0.0001` **正好等于 `min_order_qty`**，会让 skew 的缩量退化成二值开关——
  阶段 3 v0 判 rejected 的同一个坑。`size > min_order_qty` 是摆脱它的最低要求。
- **这越过了 [18](18-maker-strategy-roadmap.md) 的红线**「收益读数转正前不扩大 size /
  `max_position` / symbol 数量」（换 symbol 不算，数量仍是 1；加 size 算）。两轮 HYPE
  读数均为负（−1.8591 / −1.66 DUSD），BTC 上零读数。代价是若逐笔经济性与 HYPE 同量级
  （−1.4bps/笔），2× 名义就是 2× 的美元流血。
- **加大 size 不会提升统计功效**：capture / markout / fee 都是 bps 比率，对 size 不变，
  bps 的 σ 也不随 size 缩小。功效只由成交笔数决定。size 只放大美元数额，以及
  SIP-5A maker-hours。

## 3. 换品种意味着证据基础重置

冻结经济口径 `capture 2.8 − markout 3.2 − fee 1 ≈ −1.4bps/笔` **全部是 HYPE 的测量**。
BTC 需要自己的绝对读数；HYPE 的机制判定（nonlinear_skew accepted、guard accepted、
microprice 方向 accepted）**不自动结转到 BTC**——尤其 microprice：其 `lambda = 0.5` 是在
HYPE 的 +1.9bps 锚定偏置上定标的，而 BTC 只有 +0.3bps（六分之一），证据文档明确指出
**单一固定 lambda 无法适配所有品种**。这是本次最大的未决裁决点（见 §5）。

费率与品种无关，已核实：maker 1bps / taker 4bps，11 个 symbol 一致。

## 4. band 预算红线：一个已修复的阻断性缺陷

`examples/maker-microprice-hype-baseline.toml` 在 band 40→30（2026-08-19）之后
**完全无法启动**：

```
❌ microprice violates band red line:
   spread_bps + ladder + inventory cap + external cap_bps + cap_bps = 34 must be <= band_bps 30
```

红线来自 [29](29-maker-external-skew-design.md)（2026-07-31 立），在**启动时**校验：
所有中心偏移 cap 加 spread 必须装进 band，否则拒绝运行——它**不做运行时截断**，因为
在被截断的偏移上做 A/B 是不可解读的。当时改 band 的注释写「会被 clamp 截断，是接受的
代价」，与这条不变量直接矛盾。

**逃逸路径**：改了 live 配置 → 跑全套单测（无任何测试加载该配置文件，全绿）→
**从未用该配置启动过一次 bot**。为此存在的启动校验器一次都没跑。

**修复**：`[microprice] cap_bps 6 → 2`，`8 + 12 + 8 + 2 = 30 ≤ 30`。选 microprice 是因为
它的**幅度**本就是明确未判项（owner 2026-08-19：方向 accepted、幅度需新窗口重测），
砍它不触碰任何已 accepted 的机制。BTC 配置沿用同一预算。

**教训（已记入流程）**：改动 live 配置文件后，单测绿不构成验证，必须用该配置真正启动
一次（paper 即可）走完启动校验。

## 5. 未决裁决点

1. **microprice 在 BTC 上的 lambda 无依据**（§3）。选项：(a) 沿用 0.5 并把它当未判项，
   (b) 按锚定偏置比例缩到 ~0.1，(c) 在 BTC 上先关掉、跑干净基线。本文件采用 (a) 的
   保守变体：保留启用但 cap 已降到 2bps，实际偏移被硬顶在 2bps 内。
2. ~~`stop_loss` 相对收紧~~ **已裁决（owner 2026-08-21）：`stop_loss` 5.0 → 10.0。**
   它是绝对 DUSD、不随 size 缩放，2× 名义下同样美元亏损只需一半 bps 移动即触发。
   提到 10.0 后按 HYPE run1（−1.8591 / 35.9h）线性外推约 **97h（~4 天）**才撞线，与
   [27](27-maker-baseline-pnl-collection-runbook.md) 的多日单臂采集窗口匹配（留在 5.0
   时约 48h 就会中断窗口）。**`alert_loss` 故意留在 2.5**——它只通知不停机，在一个被
   放宽的刹车前面保留更早的预警是想要的；代价是它在 stop 的 25%（而非 50%）处就响。
   这是本次第二处显式放宽安全限额的裁决（第一处是 [14](14-maker-live-gate.md) 的
   canary 逐次要求挂起）。
3. **`max_divergence_bps = 15.0` 对 BTC 过松**：BTC mark/mid 背离 p50 仅 +0.3bps，
   15bps 实际永不触发，等于关掉这道 skip 门。收紧会改变 skip 行为。
4. **`vol_pause_bps = 40 / 60s` 未按 BTC 波动率重标。**
5. **`external_skew` 仍是未判机制**（owner 2026-08-21 保留）。BTC 读数同样含它。

## 6. 首窗口计划

首窗口**不是收益读数**，而是同时办三件事（遥测是纯观测，不污染读数）：

1. 回答两个二元问题（原 handoff 文档 `handoff-next-phase-2026-08-21.md` 未入库，问题
   已内联于此，不需要它也能执行）：`public_trade` 到底带不带 `side`（看 stderr 的
   `public_trade raw sample:` 前 50 条原文）、`cycle_summary.book` 的 `null` 占比。
2. 拿 BTC 的第一个 `geometry` 分布：`min_distance_to_touch_bps`、`clamped_to_touch`
   计数。**注意**：这些计数在不同 `band_bps` 之间不可比（见 [30](30-maker-uptime-band-tightening-design.md)）。
3. BTC 的第一个绝对 PnL 读数（按 [27](27-maker-baseline-pnl-collection-runbook.md) 的单臂规则）。

paper 冒烟已通过（2026-08-21）：配置加载、启动校验、报价与 hold 均正常；
首三周期即观察到 mark 落在最优买价之下、touch 约 1.6bps 宽的不对称形态。

命令：

```bash
standx maker run BTC-USD --maker-config examples/maker-btc-baseline.toml
```

加 `--live` 前需按 [14](14-maker-live-gate.md) 确认 `STANDX_ENABLE_LIVE_MAKER`
（注意 canary 逐次要求已于 2026-08-20 被 owner 挂起，其余门槛仍适用）。

## 7. 启动前检查与授权（2026-08-21 准备）

首窗口按 [27](27-maker-baseline-pnl-collection-runbook.md) 的单臂规则执行（不是 A/B，
没有晋级判据），前置检查套用该手册并适配 BTC。以下均为**启动前实测**，不是转述：

| 检查项 | 结果（2026-08-21T06:2xZ 实测） |
|---|---|
| FLAT | ✅ `account positions` / `account orders` 均为空 |
| auth token | ✅ 剩余 250h（到期 2026-08-31T17:04Z），覆盖 72h 窗口有余量 |
| 场馆 metadata | ✅ `price_tick_decimals=2` / `qty_tick_decimals=4` / `min_order_qty=0.0001`，与 §2 假设一致；maker 1bps / taker 4bps 与全品种一致 |
| Hyperliquid 连通（guard） | ✅ paper 冒烟中 `guard_enabled=true`、`external_basis_bps≈4.45` 已初始化 |
| paper 冒烟（新构建） | ✅ `fa4d130` release 构建跑 70s / 24 cycles：启动校验通过（band 预算 30≤30）、双边上架、`geometry` 遥测正常产出、paper 成交与 residual 交接通知均按预期 |
| 互斥 | ✅ 无其他 live maker 容器/进程在跑 |
| 磁盘 | ✅ 302G 可用（`var/standx` 现有 2.4G） |
| OpenObserve 两条 push 告警 | ❌ 未 provision——`scripts/openobserve_alerts.py` 缺 `OPENOBSERVE_USER` / `OPENOBSERVE_PASSWORD`，启动前必须由操作人补齐 |
| webhook 实测送达 | ⬜ 启动时确认（`--alert-webhook` 配置并实测一次） |

配置冻结值：sha256(`examples/maker-btc-baseline.toml`) =
`7b26cf013f190d057649282578ae138c7a877c7b6eb2f137208b66fcdc963e96`，启动前
`shasum -a 256` 复核，跑动期间一个字节都不改。

**§5 未决项的默认处置**：若授权时 owner 未对 §5 第 1/3/4/5 条另作裁决，启动即视为
采纳当前配置默认——microprice 选项 (a)（沿用 lambda=0.5、cap=2 硬顶）、
`max_divergence_bps=15` 与 `vol_pause` 不重标、`external_skew` 保留启用。这些默认值
已写进配置，读数解释时必须带上这一前提。

精确授权文本（release owner 填写后才能启动）：

已填授权（2026-08-21，首次）：

```text
授权：BTC-USD 首窗口采集（单臂长跑，按 27 号手册规则）
symbol：BTC-USD
配置：examples/maker-btc-baseline.toml（sha256 7b26cf01…，原样）
代码：git sha fa4d1300402526cafb11ecf372b0ec95a40d1d27（启动校验与 paper
      冒烟均在此 sha 的 release 构建上复核通过）
风险边界：单 symbol、一档、size=0.0002（≈$15 名义）、max_position=0.002；
          stop_loss=10.0 生效（owner 2026-08-21 裁决）；账户硬熔断不开启
窗口：2026-08-21T06:37Z 起，计划 72 小时（3 天），不换臂、不调参
emergency cancel 操作人：release owner（BossX）
授权人 / 时间：release owner（BossX），2026-08-21，会话内明确授权（"授权开跑"）
前置：FLAT 实测通过（positions/orders 均空）；token 剩余 250h（到期
      2026-08-31T17:04Z）覆盖窗口；BTC metadata 2/4/0.0001 复核一致；
      OO 两条告警已 provision（deadman + critical_risk，Feishu 目的地已更新）；
      webhook 实测送达（Feishu code 0）；无其他 live maker 在跑
```

启动记录：run_id `btc-first-window-20260821T0637Z`，首臂 2026-08-21T06:37Z（UTC），
配置 sha256 `7b26cf013f190d057649282578ae138c7a877c7b6eb2f137208b66fcdc963e96`，
代码 git sha `fa4d1300402526cafb11ecf372b0ec95a40d1d27`。

### 首次 run 截断记录（2026-08-21）

窗口 2026-08-21T06:37Z → 08:28Z，存活 ~110 分钟后**因 owner 改参裁决截断**
（size 0.0002 → 0.0005，需重启，见下）。读数仍然有效但只是 110 分钟的噪声窗：

| 指标 | 读数 |
|---|---|
| 成交 | 70 笔 |
| 净 PnL | **−0.291 DUSD**（gross +0.596 − fee 0.104 − markout 拖累；≈ −2.7bps/笔，−3.8 DUSD/24h 折算） |
| passive capture | +5.56bps |
| markout 1s / 5s / 30s | +1.38 / −2.34 / −1.68 bps |
| 完整标志 | `net_pnl_complete=false`（`execution_costs_unavailable=2`，硬杀截断 audit 回填周期所致） |
| 二元问题 | `public_trade` **带 `side`**（50/50）；`cycle_summary.book` null 占比 **0%**（390 非 warmup 周期） |
| standby / halt / guard | 无 standby 事件；halt 68/3198 cycles（2.1%）；guard 激活 30 cycles（0.9%） |

**操作教训（两条，已发生的真实代价）**：

1. 停 maker **不要用硬杀**（本次用任务管理器强杀，cleanup 未执行，残余卖单
   `sxmk-…c8es0` 挂在场上）。残余仓位 0.0002 多靠该卖单在撤单前**自行成交**恰好
   闭合（+0.025 DUSD 价差），纯运气；随后手动 `order cancel-all BTC-USD` 复核清零。
   今后停机一律给 wrapper 发 SIGTERM（它转发子进程并走 cleanup + 残余交接）。
2. wrapper 的 OO 实时上传需要 `OPENOBSERVE_AUTO_UPLOAD=1` 且导出凭据，首次启动
   漏配导致前 16 分钟无远端覆盖（deadman 空窗），后用独立 follow 上传器补传。

### owner 裁决（2026-08-21，第二次）：size 0.0002 → 0.0005

会话内明确指示（"增大摆单量到0.0005"）。随之变化的口径：

- 单笔名义 ≈ $38.6（mark ~77,100），≈ HYPE 等名义的 **5×**；再次越过 docs/18 红线。
- `max_position=0.002` **不变**：库存容量比从 10× 降为 **4× size**。
- **`stop_loss` 保持 10.0**（owner 征询后采纳建议）：亏损速率随名义 ×2.5，
  按 HYPE 口径外推 ~39h 撞线，按首次 run 实测（−3.8 DUSD/24h @ 0.0002）折算
  甚至 ~25h；接受为检查点，不再叠加第三道安全限额放宽。`alert_loss=2.5` 不变。
- size=0.0005 = 5× `min_order_qty`，skew 缩量退化为二值开关的坑依然不适用。

已填授权（2026-08-21，第二次，首次 run 截断后重开）：

```text
授权：BTC-USD 首窗口采集（单臂长跑，按 27 号手册规则，第二次）
symbol：BTC-USD
配置：examples/maker-btc-baseline.toml（size=0.0005，sha256 见下，原样）
代码：git sha 与首次相同（fa4d130 + docs/启动记录提交，无代码变更）
风险边界：单 symbol、一档、size=0.0005（≈$38.6 名义）、max_position=0.002（4× size）；
          stop_loss=10.0 生效；账户硬熔断不开启
窗口：2026-08-21T09:24Z 起，计划 72 小时（3 天），不换臂、不调参
emergency cancel 操作人：release owner（BossX）
授权人 / 时间：release owner（BossX），2026-08-21，会话内明确授权
      （"增大摆单量到0.0005" + "继续"）
前置：FLAT 实测通过（首次 run 残余已处置：卖单自行成交闭合 + cancel-all 复核清零）；
      token 剩余 ~250h 覆盖窗口；OO 两条告警已 provision 且本次
      OPENOBSERVE_AUTO_UPLOAD=1 实时上传；webhook 通道已实测
```

启动记录（第二次）：run_id `btc-first-window-20260821T0924Z`，首臂 2026-08-21T09:24Z（UTC），
配置 sha256 `24e8381a5bd9c915f321db17989492213acc5fad7f5a3c41a9b1f3c9c7593d0c`
（size=0.0005），代码 git sha 同首次（`fa4d130`，期间仅 docs/配置提交，无代码变更）。

### run2 截断记录（2026-08-21）

窗口 09:28Z → 10:10Z（~42 分钟），因 owner 第三次裁决（启用库存退出）截断。
这次走 SIGTERM 优雅停机：cleanup 撤单经 `query_order` 双向确认，residual
−0.0001（部分成交所致）交接后手动 reduce-only 市价平仓，FLAT 复核通过。

| 指标 | 读数 |
|---|---|
| 成交 | 34 笔 |
| 净 PnL | **−0.350 DUSD**（`net_pnl_complete=true`；≈ −2.6bps/笔，与 run1 的 −2.7bps 一致） |
| passive capture / markout 1s/5s/30s | +3.72 / +1.11 / −2.07 / −5.07 bps |
| uptime | 64%（明显低于 HYPE 窗口的 ~88%，待观察） |
| halt / guard | 22 / 1340 cycles（1.6%）；guard 激活 12 cycles |

### owner 裁决（2026-08-21，第三次）：启用库存退出 ALO+IOC（70%）

会话内明确指示（"我等不了，现在上，70%"）。随之变化：

- `inventory_exit_pct=70`（|pos| ≥ 0.002×70% = 0.0014 触发）+
  `inventory_exit_qty=0.0005`（每 cycle 最多砍一笔 size）+
  `[inventory_exit] alo_enabled=true`（ALO 1bps 优先，亏损越 5bps 或 Alo 段超时
  升级 IOC 4bps 穿价成交）。
- **代码事实**：`inventory_exit_plan` 要求 `chunk_qty > 0`，只设 pct 不设 qty 是
  空配置——三个值必须一起改。
- **IOC 后端接受度已实盘探针验证**（docs/33 的前置要求）：不交叉 IOC 买单
  （74,000 @ 0.0001，mark ~77,700）被立即取消，无成交、无留单。
- **语义变化（必须带着读数）**：主动砍仓进入基线，退出成本进 PnL，本窗口读数
  不再是与 HYPE 同口径的纯被动基线。docs/33 的判定（执行成本：退出笔数 × 节省
  ~3bps、ALO/IOC 占比、exit_cost_quote）随每日记录观测，正式判定仍悬置——本轮
  不给出 accepted/rejected，只采集。

已填授权（2026-08-21，第三次，run2 截断后重开）：

```text
授权：BTC-USD 首窗口采集（单臂长跑，按 27 号手册规则，第三次）
symbol：BTC-USD
配置：examples/maker-btc-baseline.toml（size=0.0005 + 库存退出 70%/0.0005/ALO+IOC，
      sha256 951de334…，原样）
代码：git sha 同前（fa4d130 + docs/配置提交，无代码变更）
风险边界：单 symbol、一档、size=0.0005（≈$38.6 名义）、max_position=0.002；
          库存退出 70% 触发、chunk 0.0005、ALO+IOC；stop_loss=10.0 生效；
          账户硬熔断不开启
窗口：2026-08-21T10:15Z 起，计划 72 小时（3 天），不换臂、不调参
emergency cancel 操作人：release owner（BossX）
授权人 / 时间：release owner（BossX），2026-08-21，会话内明确授权
      （"我等不了，现在上，70%"）
前置：FLAT 实测通过（run2 residual −0.0001 已手动平仓）；IOC 探针通过；
      token 覆盖窗口；OO 告警已 provision + AUTO_UPLOAD 实时上传；webhook 已实测
```

启动记录（第三次）：run_id `btc-first-window-20260821T1015Z`，首臂 2026-08-21T10:15Z（UTC），
配置 sha256 `951de334af5b76d704e3962c74eb0f2cee47a948e0c239443e5d673b24b4ca04`，
代码 git sha 同首次（`fa4d130`，无代码变更）。

### run3 截断记录（2026-08-21，基础设施事故，非 maker 故障）

窗口 10:15Z → 12:39Z（~2.4h），**被宿主任务系统的输出上限（16MiB）强杀**——maker
stderr 的 `public_trade raw sample` 等输出被任务捕获管道吞满所致，maker 本身无任何
故障。硬杀导致 cleanup 未执行：残余卖单 1 张（0.0005 @ 77361.43）手动
`cancel-all` 清除，仓位本已 FLAT，复核通过。OO 已 drain（16230 条）。

截断前读数（4014 cycles，80 笔，买/卖 40/40）：净 −0.458 DUSD（≈ −1.5bps/笔，
好于 run1/run2 的 −2.6/−2.7），capture +3.73bps，markout 5s/30s = −1.95/−2.91bps。
**库存退出首实战**：仓位顶到 0.002 触发 trim，ALO 挂 3s 未成交 → 升 IOC →
残量重试，两笔各 0.0005 @ 76905.99 成交，砍回 0.001；`exit_cost_quote` −0.0258
（~3.3bps，本集全额 taker）。

**教训（第三条，流程级）**：长跑 run 的 wrapper 输出必须重定向到文件
（`>> var/standx/<run>.wrapper.log 2>&1`），不能让宿主管道捕获 maker stderr——
它无界增长会触发宿主强杀，等于绕过所有 fail-safe。

启动记录（第四次，配置不变，run3 事故的继续采集，沿用第三次授权——同配置、
同风险边界，仅重开窗口）：run_id `btc-first-window-20260821T1241Z`，首臂
2026-08-21T12:41Z（UTC），配置 sha256 同前（`951de334…`），代码 git sha 不变。
72h 窗口至 2026-08-24T12:41Z。

### run4 截断记录（2026-08-21）+ owner 第四次裁决：max_divergence 15→8

窗口 12:41Z → 13:40Z（~1h），因 owner 第四次裁决截断。SIGTERM 优雅停机，
residual flat（无需人工处置）。读数：42 笔，净 −0.568 DUSD，capture +3.95bps，
markout 5s/30s = −3.0/−4.48bps，uptime 70%，exit_fills=1。

**owner 裁决（2026-08-21，第四次）：`max_divergence_bps` 15.0 → 8.0**（"现在就收紧"）。
阈值依据 run3/run4 实测逐周期 |mark−mid| 分布（n=5797）：p50=1.9 / p90=5.3 /
p99=11.65 / max=14.8bps；15bps 零触发（死门确认），8bps 跳过 ~3% 周期、切在 p99
尾巴前。备选 5bps 会跳 12% 周期、叠加本已偏低的 uptime（64–70%）被放弃。
改变的是 skip/standby 行为：背离越阈时进入行情降级 standby（不报价），属防御性收紧。

启动记录（第五次，沿用第三/四次授权的风险边界，仅配置一行变化）：
run_id `btc-first-window-20260821T1345Z`，首臂 2026-08-21T13:45Z（UTC），
配置 sha256 `ba8849df2bff60289ee00bdc55c1e53e4769d1c7c3b76526510c1bcffaa7b9a7`，
代码 git sha 不变（`fa4d130`，无代码变更）。72h 窗口至 2026-08-24T13:45Z。
