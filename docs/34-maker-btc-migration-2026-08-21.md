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

**owner 裁决 2026-08-21：采用 2× 名义（size = 0.0002）。** 两点理由与代价：

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

