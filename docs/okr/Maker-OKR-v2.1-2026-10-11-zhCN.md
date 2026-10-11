# Maker OKR v2.1

窗口：2026-10-06 → 2026-11-02（本月）；O3 挪到下月（2026-11-03 起）
制订：2026-10-03；v2.1 修订：2026-10-11（owner 拍板：OKR 跟 Cursor StandX Project 的改进计划排期走）
状态：O1、O2 本月推进；O3 不在本月上 live。

## v2.1 修订（2026-10-11）

owner 定：以 Project 的改进计划（docs/improvement-plan.md）为排期准绳，OKR 随之调整，不让 Project 追 OKR。

1. **O3 整体挪到下月。** best-anchor 等 3.1 改动、docs/36 预注册、新 live 臂，都排在 Project 策略关键路径之后：SIP-5A 奖励价值 → 定目标 → replay/live 共用准入检查（`admit_actions`）→ best-anchor 在 replay 里按 3 秒视角加事件撤单重新标定 → 预登记 → 单行 A/B。
2. **本月 go/no-go（2.4）只看两件事：** 奖励价值（2.1/2.2 核实后），以及提前止损线（每小时收益 < 历史被吃损失/h 的 1/3 → no-go）。历史被吃损失参考值：BTC mo30 0.2557 USD/h，门槛 0.0852；mo300 0.2829，门槛 0.0943（354 笔/31.33h，未计手续费）。
3. **stand-off 49 / margin=0（#393）暂不采纳。** Project 指出 49 ≈ 6.35bps，和现行 8bps 距离相近，模拟成交约 0.83 笔/h，判不出结果。改为先在 replay 里重新标定。
4. **「本月不做机制 A/B」放宽为：** 本月不上任何新机制 live；下月的 best-anchor 允许按 Project 的规程做单行 A/B。停止规则改成按成交笔数，不按时长。
5. **基线：** 当前 Vultr live 基线（2026-10-10 14:23 HKT 起，配置 `ba8849df`，200 笔或 10-24 截止）是 Project 的「冻结现行配置拿第一个可信绝对 PnL」，external_skew 等旧机制仍开着。3.2「关掉不在方案里的机制」随 O3 一起挪到下月，作为新基线的一部分。
6. **VPS：** owner 已在 Project 授权把实验和 live 迁到 Vultr，取代下文「Vultr 只读测延、不上 live」的旧条。
7. **候选 A：** 已于 2026-10-06 因 PN65 重启中断，以 3 对离线读数结案（样本不足、未判定），不重跑。external_skew 保持开。

### 交给 Project 补进计划的缺口（OKR 有、改进计划没覆盖）

- 1.1 下单到 ACK 的 RTT p50/p99，以及推送晚到时延（服务端时间对本机收到时间）。bot 已在 Vultr，可以直接从 live 日志测，不用另下测试单。
- 1.2 四路流按服务端时间落毫秒日志（StandX 盘口/成交/mark + Binance 成交价和 1 秒量）。#391 里有默认关闭的 `server_time_log`。
- 1.3 3–4 天纯观测，产出 BTC、XAU 的 best–mark spread 和单秒 best 跳变表。
- 1.4 MH 估算器，纯观测、不进决策路径（#391 设计稿）。
- 2.2/2.4 的提前止损线要写进 go/no-go 判据。
- XAU：开市时段的定义（目前用 CME Globex 代替，未核 campaign）和 XAUT 外盘序列都还没有；XAU 1 秒跳变开市 p99.9 = 2.3，n=144,409，欠采样。
- 本 OKR 入库到 main，解决「#391/#392 引用了 OKR v2 但 main 上没有」。

---

（以下为 v2 原文，按上面的修订读；与修订冲突处以修订为准。）


相对上一版 OKR，这版只改三处：报价锚从 mark 换成 best（mark 只判断在不在带内）；被吃后按每笔成交立刻止血，而不是仓位到 70% 才退出；跳过按整点成块调度，而不是每个周期单独判断。

## 和现状的差距

报价锚。方案要求锚 best，mark 只用来判断在不在带内。现在全部几何量锚 mark，`band_bps=30`。要新增一种报价模式，这是核心改动。

两侧同进同退。现在 `external_guard` 和仓位上限会只撤一侧。单边挂单不计分，但照样会被吃，是只担风险、不收权利金的状态。

被吃后立刻止血。方案是 1bp 止盈，加上 ALO 循环，浮亏 5bps 用 IOC。现在仓位到 70% 才触发 ALO/IOC 退出。要改成按每笔成交触发的退出状态机，可以复用 docs/33 的三段结构。

18 分钟预算，成块跳过。现在 `max_divergence`、`vol_pause` 每个周期单独判断。要一个按整点调度的跳过器。

Binance 1 秒成交量做大单闸。现在没有 Binance 行情，外部价只接了 Hyperliquid。要新增一路行情，放在 sdk 或 cli。

服务端时间戳的毫秒日志。现在只有 mark 带 `server_time`，部分覆盖，要补齐。

每小时 MH 估算。现在没有。新做一个纯观测模块。

对照过的代码面：Binance 行情、服务端时间戳、单边压制、MH 统计。

## 两个先说清的判断

go/no-go 可以在最小单量下判。MH 收益和被吃损失都跟单量成正比，手续费也按 bps 收，所以「MH 收益/h 对比被吃损失/h」这个比值基本和单量无关。例外是 proximity 权重和每侧 cap。不需要先放大单量，用 `min_order_qty` 跑出来的比值就能判。

「立刻止血」先离线验证，再决定 3.1③ 能否打开。2026-10-03 只读回放见文末：354 笔上止血比 mo30/mo300 少亏，但 3 秒格和空手续费不够单独授权上 live。早先的 −8.66/−3.64 只覆盖 8 月开仓子集，不再当全样本结论。

方案里的规则数字（BTC Unit Size 2、每侧 cap 20、每小时上限 10；XAU 30/60/2；权重贴 mark 200%、带边 40%）是外部转述，以 campaign 页为准。KR2.1 先对着官方页面核实，再拿来算。

## O1：搭好测量地基

让后面所有「来不来得及」都用我们自己的数。W1–W2。

1.1 bot 迁到靠近 StandX 的 VPS。测出下单到 ACK 的 RTT p50/p99，以及推送晚到的时延（服务端时间对本机收到时间）。截止 W1。前提是 VPS 选址和开通。

1.2 四路流按服务端时间落毫秒日志：StandX 盘口（best、身前量、我们的排名）、StandX 成交、StandX mark、Binance 成交价和 1 秒成交量。截止 W1。

1.3 跑 3–4 天纯观测，不加任何撤单逻辑。产出 BTC 和 XAU 两张表：best–mark spread 的 p50/p70/p95，单秒 best 跳变的 p99/p99.9。截止 W2 中。

1.4 在 standx-maker 里做一个纯观测的 MH 估算器：按整点统计双边在带内的分钟数、Standard/Boosted 档位、按 proximity 权重折算的有效规模、每侧 cap。不进决策路径。开着和关着时 replay 结果完全一致。截止 W2。

## O2：确立新目标函数，月底给出 go/no-go

2.1 对着 campaign 页核实现行规则：权重曲线（包括 5bps 那一档）、Unit Size、cap、每小时上限、XAU 交易时段和休市乘数。由 owner 签字。截止 W1。未核实的数字不得进 2.2。

2.2 资金效率表：BTC 和 XAU 各需要多少本金才能拉满每小时 MH 上限，以及每小时的预估收益（DUSD/h）。截止 W1。用 2.1 核实后的数字算。

2.3 把 28 号规程的主指标换成「MH × 单价 − 被吃损失 − 手续费」（按小时计）。用新口径重读阶段 4、skew boost、BTC run1–4 三份证据，逐份写明结论是否翻转。截止 W2。

2.4 月底出 go/no-go 报告。截止 W4。

提前止损：如果 2.2 算出的每小时收益低于历史被吃损失/h 的 1/3，O3 不上 live，直接走 no-go。

## O3：上线「锚 best 的最小基线」并跑出读数（v2.1：挪到下月）

原排 W2–W4，v2.1 起挪到 2026-11-03 之后，排期以 Project 改进计划为准。O2 提前止损触发则本目标不上 live。

3.1 下列改动全部默认关闭。关闭时和当前行为完全一致。每条安全测试都要做变异验证。截止 W3 初。

① 报价锚 best。站位距离 = max(单秒跳变 p99.9, 1 tick) + 余量，并且留在带内侧。
② 两侧同进同退。
③ 按每笔成交触发的止血退出：1bp 止盈 / ALO / 浮亏 5bps 用 IOC。上线前须完成「立刻止血 vs 等待」的离线回放；回放不支持立刻止血，则本条保持关闭。
④ 成块跳过：spread 超过该品种 p70、RTT 变差、mark 追不上 best 时跳过，每次有最短时长。
⑤ 停机线：连续 2 小时 uptime < 30 分钟，或者当天被吃亏损超过当天预估收益。

3.2 关掉不在方案里的机制：`external_skew`、`microprice`、`nonlinear_skew`，以及只撤一侧的 `external_guard`。drift 收到 0.4bps 量级，size = `min_order_qty`。截止 W3 初。只关配置，不删代码。

3.3 写 docs/36 预注册（含新口径判据）。拿到 owner 精确授权后开跑：XAU 开市时段跑 XAU，休市跑 BTC。XAU 先按单独窗口跑，自动切换放到下个月。截止 W3。

3.4 Boosted 小时（≥ 42 分钟）占比 ≥ 80%。所有窗口都以空仓无挂单收尾。`net_pnl_complete=true`。截止 W4。

3.5 每天定时出四个数（可以复用现有的 Lark 日报脚本）：被吃笔数和平均亏损 bps、每小时 uptime、跳过次数和原因、RTT p50/p99。从 W3 起。

加分项，不算进考核：Binance 1 秒大单闸只在 1.3 标定出的精度显著高于随机基准时才加，并且只能作为投票器里的一票。

## 本月明确不做

不新建方案里列出的任何检测类闸门：成交静默、盘口蒸发、身前量、贴脸刹车、排名刹车。

不让多个信号各自有权停机。不做任何机制 A/B。

不扩大单量。不加品种。XAG 因为 tick 太粗直接排除。

不删旧机制的代码，只在配置里关掉。

## 风险

W2–W3 最紧。3.1 一共五项改动，安全路径要做对抗 review 和变异测试，一周很满。时间不够就按 ①②③⑤④ 的顺序砍。成块跳过可以先只做「mark 追不上 best」这一条。

## 需要 owner 提供或决定

2026-10-03 16:00 上海，owner 已口头定三件：

1. VPS：Vultr JP（198.13.43.68）。只读测延，不迁候选 A，不上 live maker。公开 GET 的 TTFB 已有；下单到 ACK 未测，未授权发单前不做。
2. campaign：现在没有明确数字。2.2 资金效率表保持空，未核实的数字不得填入。
3. XAU 外盘参照：XAUT。BTC 仍用 Binance。
4. W3 live：已授权。授权不等于现在开跑。3.1 默认关闭的实现和 docs/36 预注册完成之前，不开 best-anchor live。

## 不依赖上述输入、可以先做

这三件已做完，不代表可以上 live：

MH 估算器设计稿和默认关闭的服务端毫秒日志：draft PR #391，未合。
立刻止血离线回放：见文末。3.1③ 仍关。
3.1① 默认关闭实现：draft PR #392，未合；不改 `scripts/run_maker_stage2_ab.sh`，无 toml 打开。

2026-10-03 20:58 上海：离线前置设计已落 `standx-pn65-docs/36-best-anchor-offline-prerequisites-2026-10-03-zhCN.md`——1s best 跳变 p99.9 离线采法、margin 占位、冻卡填空稿、双重计数处理（第一张 live 卡应对齐 3.2 关 skew）。

2026-10-04 离线进展（纸面，未授权 live）：

- BTC 1s best 跳变草稿：标量 **49.0**（bid/ask p99.9 均为 49；`local_recv_utc`；n=325962；来源 8 月 StandX `depth_book`）。本机镜像已恢复：`37-btc-best-jump-draft-2026-10-04-zhCN.md` + `.csv`（2026-10-04 ~20:40 HKT 自 PN65 agent-store `bc-a98d75e0` 只读回填）。
- Paper 校准 margin：推荐 **margin=0 / stand-off=49**（秒内 MAE p99.9 未超出 49，不加垫）。证据 draft PR https://github.com/wjllance/StandX-CLI/pull/393 （仅 `docs/evidence/btc-best-anchor-margin-paper-2026-10-04.md`）；本机镜像 `standx-pn65-docs/evidence/btc-best-anchor-margin-paper-2026-10-04.md`。
- XAU 1s 跳变数据盘点（2026-10-05 ~08:45 HKT，PN65 只读，未计算）：PN65 有 StandX `XAU-USD` 盘口，`var/standx/lag-rec-20260820T065924Z-XAU.ndjson`（212 MB），2026-08-20 06:59Z 至 08-24 01:34Z，`depth_book` 双边 860,552 行，0 残缺 0 交叉；每行有 `local_recv_utc`，盘口行另有 `server_time`（毫秒）和 `seq`。形状与 8 月 BTC 同一批录制一致。窗口跨周末，算主 p99.9 前须按 XAU 开市时段过滤（36 号会话规则）。**没有 XAUT 外盘数据**：录制器订的是 Hyperliquid `XAU`，零条；`xyz:GOLD` 未录。XAU 表可离线出 StandX 主表，外盘对照列暂空。
- XAU 1s 跳变草稿（2026-10-05 ~13:00 HKT，PN65 只读离线，CloudAgent bc-67eeb069）：同 37 号 BTC 口径（local_recv_utc 整秒、首末端点差、type 7）。观测量子 0.1。会话按 CME Globex 金替代口径切分（未核 campaign 时段）。**开市主标量 p99.9 = 2.3**（bid 2.0 / ask 2.3），n=144,409 < 2e5，**标欠采样**；全时段 2.2（n=301,457），休市段 2.2。best–mark 触边（5s 配对）开市 p50/p70/p95 = 1.09/1.55/2.44 bps，全样本 p50 1.30 bps。休市段盘口不断流（最长空洞 7s），但 best 长时间不动，与替代口径休市对齐。原件在 PN65 docs/evidence/xau-best-jump-draft-2026-10-05-zhCN.md（未提交），本机镜像 evidence/xau-best-jump-draft-2026-10-05-report-mirror.md。外盘 XAUT 列仍空。
- 未冻结预注册草稿：本机镜像已恢复：`38-best-anchor-prereg-draft-2026-10-04-zhCN.md`（重建稿：PN65 无原件；目标对齐 3.2 关 skew；margin=0 纸面推荐已写入清单，正式采纳仍待）。

正式采纳 margin=0、冻卡勾选门槛/primary Δ/两臂 sha、以及「授权开跑」仍待 owner。未写 live toml，未开 best-anchor live，不碰候选 A。#391/#392/#393 均仍 OPEN draft（wjllance/StandX-CLI）；#393 check 红、build-matrix 绿（docs-only，不挡纸面审阅）。

下一步：等 owner 对 #393 纸面结论做正式采纳（或否决并指定 k 垫）；在 38 草稿上勾门槛 / primary Δ / 两臂 sha；冻卡全绿且本轮口头授权前不开 best-anchor。不碰候选 A。

## 和正在跑的候选 A 的关系

2026-10-03 约 10:41 上海，候选 A 已 live：`stage2-baseline-20261003T024129Z-ba8849df2bff`，只改 `external_skew` true→false，12h×4 对。本 OKR 写明本月不做机制 A/B，也要关掉 `external_skew`。这份文档不自动取消那一跑。要停，另说一声。跑完前不要 reset `scripts/run_maker_stage2_ab.sh`。

## 离线回放（2026-10-03，只读）

样本不是连续 50h。盘上已收口 BTC 被动成交：6 个窗口合计 31.33h、354 笔（四段 8 月 first-window + T1345Z，加 pn65-btc-24h）。活臂 0 笔，未进均值。标记用 cycle mark，约 3 秒一格。逐笔 fee 全空。+1bp 按碰到即当 ALO 成交。

- 立刻止血：92 笔 +1、262 笔 −5、0 笔没碰到。均值 −3.44 bps。触发格实际 mark 均值 −4.92（+1 侧 +2.59，−5 侧 −7.55）。
- mo30：−6.63（n=352）
- mo300：−7.33（n=333）

同批上止血比 mo30 和 mo300 都少亏。即使按触发格实际 mark −4.92 记，仍然少亏。

−8.66 / −3.64 只在四段 8 月开仓成交上复现（mo30 n=69，mo300 n=65），不是全样本，也不是 pn65 24h。用 mark_at_fill 或成交价算 mo30 复不出 −8.66。因此「等 5 分钟反而少亏」不再作为全样本结论。

缺口：3 秒格分不清格内先碰 +1 还是 −5；逐笔 fee/rebate 空（pn65 会话级 fee 约 1.00bp，没进反事实）；ALO 是碰价假设；8/21–8/23 stage2 skew-boost 约 24.8h 未纳入。

3.1③ 仍默认关闭。这份回放方向支持止血，但精度不够，不能单独授权上 live。
