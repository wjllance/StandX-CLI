# 外部领先价连续偏移（`[external_skew]`）立项设计（2026-07-29 草案，2026-07-31 细化）

状态：**草案，待 release owner 裁决**（2026-08-01 复核：**保持待裁决，不降级、不关闭**）。

- **2026-07-31 细化**四处（机制与配置面不变）：guard 激活期间的复合语义、band 预算
  红线、验收判据的统计功效口径、证据溯源。
- **2026-08-01 复核**（[尾部分解报告](evidence/maker-markout-tail-decomposition-2026-08-01.md)）：
  立项依据 (b) 三样本复现（靶子更稳），(d) 残余毒性量化为硬边界（尾部集中 + 外部信号
  无判别力），(c) 补价值上限阶梯。**净效果是"靶子更确定、上限更明确"**：机制不变，
  但裁决时须知道它只打其余 90%、现实口径回收净亏 ~16%，**达标也不使策略盈利**；
  亏损大头（60~72%）需要一个独立的尾部避让候选。

live 时间片前置（**2026-07-31 更新**）：原文写的是"采集窗口 2026-07-31T08:17Z
收尾之后"，该表述已失效——三次基线采集 run 全部被截断（run1 35.9h 读-写滞后误报、
run2 2min / run3 51min 两帧 ack，见
[截断报告](evidence/maker-baseline-pnl-2026-07-30-run2-truncated.md)），没有任何一次
跑到授权窗口末端。**现行前置改为条件式**：一次**有效**基线采集 run 收尾（[27 号手册](27-maker-baseline-pnl-collection-runbook.md)
的终止条件之一命中，不是按日历日期）。窗口内不碰冻结基线，本文档与离线分析不消耗
live 时间片。

上游口径：[18-maker-strategy-roadmap.md](18-maker-strategy-roadmap.md)；
信号源机制沿用 [22 号设计](22-maker-stage3v1-guard-design.md)的
`[external_guard]` 与 [24 号独立立项](24-maker-guard-spinoff-design.md)。

## 立项依据（证据链）

### 1. 亏因定位：markout，不是 capture，不是成本

基线 PnL 采集 run 的净额恒等式分解（2026-07-29T03:39Z 时点，~19.4h，
29120 cycles / 289 fills）：

| 项 | 数值（quote） |
|---|---|
| gross spread capture | +0.745（passive capture +5.0 bps） |
| inventory MTM（重估残差） | **-1.703** |
| 手续费 | -0.150 |
| funding | -0.001 |
| **净 PnL** | **-1.109** |

markout：1s +0.07 / 5s -3.0 / 30s **-5.8 bps**。capture 恰好被逆向选择吃光
还有余：单笔真实 edge ≈ +5.0 − 5.8 ≈ -0.8 bps。价格上行段 MTM 项仍持续恶化，
说明不是方向性敞口，是成交时点被系统性逆向选择。所有只能改 capture 或改方差
的旋钮都不对症——只有改 markout 的旋钮对症。

### 2. 离线检验（2026-07-29，同一 run 日志，纯只读）

`cycle_summary.external_divergence_bps`（excess = 原始 divergence 经 300s 半衰期
EWMA 扣除静态 basis 后）每轮都在日志里，fill 事件带 `side/price/mark_at_fill/
event_time_ms`——**这是 microprice 类候选做不到的回溯检验**（盘口量从未落日志）。

**(a) excess 预测未来 mark 移动：斜率 ≈ 1，线性延伸到 ±2bps 桶，非尾部现象。**

| excess 桶（bps） | n | 30s 后 mark 实际移动（bps） |
|---|---|---|
| -10.2 | 71 | -11.6 |
| -6.7 | 217 | -6.7 |
| -4.8 | 1265 | -3.7 |
| -2.8 | 4751 | -2.0 |
| ~0 | 16300 | -0.06 |
| +2.9 | 4610 | +1.8 |
| +4.8 | 1453 | +4.3 |
| +6.7 | 324 | +5.0 |
| +9.6 | 117 | +8.6 |

HL 对 StandX mark 的领先在 ~30s 内被吸收，与 3s 报价周期速率匹配。
二值 guard（enter=10）把 ±2~10bps 区间里与尾部几乎同质量的信号扔掉了。

**(b) 成交时点的逆向选择是系统性的**：买单成交时 excess 均值 **-3.2bps**
（n=145），卖单 **+3.65bps**（n=144），无条件均值 +0.03。被动单恰好在
"外部价预告逆向移动"的时刻被吃。

**(b) 已三样本复现（2026-08-01）**——原始值，不做任何取反：

| 样本 | 买单 | 卖单 |
|---|---|---|
| 本节原值（上轮 19.4h 切片，n=289） | -3.2 | +3.65 |
| 上轮全窗（n=527） | -3.25 | +3.31 |
| 本轮 36h（n=522） | -3.35 | +3.53 |

三个独立样本全部落在 **±0.35bps** 内。**这是本立项最稳的一条证据**——靶子真实且
可复现，不是单次读数。详见
[尾部分解报告](evidence/maker-markout-tail-decomposition-2026-08-01.md)发现 5。

**(c) 反事实现金改善（上限口径）**：中心偏 `λ×excess` 且假设全部 fill 仍成交，
λ=0.25 / 0.5 / 1.0 → **+0.14 / +0.27 / +0.54**。上限口径的诚实边界见"风险与已知限制"。

**(c) 价值上限的阶梯（2026-08-01 修订，裁决必读）**：原文写的"对照当前净亏 -1.1，
即可回收 25~50%"**容易被读成"回收一半就快到平衡了"，实际不是**。按 36h 净亏 -1.66
重算，并叠加 (d) 的尾部边界：

| 口径 | 量级 | 占当前净亏 |
|---|---|---|
| 现实（λ=0.5，本节反事实） | +0.27 / 35h | **~16%** |
| 理想（其余 90% 的 markout 全部消除） | +0.6 ~ 1.1 / 35h | 36~66% |
| 尾部那 60~72%（见 (d)） | **本机制碰不到** | — |

**即使 A/B 达标，策略仍然不赚钱**：回收的是净亏的 ~16%，大头结构上在本机制之外。
这不构成否决理由（16% 是真钱，且机制成本极低），但**裁决不能按"这一个就能救活"来批**。
（两个百分比口径不同，别读成打架：尾部占 **markout 亏损质量**的 60~72%，而"全砍其余
90%"能拿回**净亏**的 36~66%——差别在于那些成交的 capture 收入仍然保留。）

**(d) 残余毒性**：excess≈0 桶的逐笔签名 markout@30s 仍为 **-10.6bps**，
与其他桶大致持平——外部领先价只解释毒性的一部分，其余是场内流量。
microprice（盘口量失衡）类候选保留为第二优先，等 depth 观测字段落地后评估；
两者不互斥。

**(d) 已量化（2026-08-01）**：残余毒性不是均匀渗透，而是**尾部集中**，且外部信号对
尾部**无判别力**——本节原来的定性警告现在有了边界。

- 最差 10% 的成交（mo300 均值 -53~-58bps，n≈103 两轮合计）扛了 **60~72%** 的 markout
  亏损质量；其余 90% 只有 -2.5~-4.0bps。
- 有毒 10% 与其余 90% 在成交时刻的外部偏离度**几乎同分布**（\|div\| 均值 4.04 vs 3.83、
  p90 6.96 vs 7.24，上轮同结论），且 `enter_bps=10` 在这 103 笔里**一次都没够着**
  （guard 激活 0%）。
- markout 曲线在 **30s 饱和**（30s -7.8 / 60s -8.4 / 300s -8.1 / 900s -8.0），没有长尾。
  **好消息**：伤害确实落在外部领先价的有效窗口内；**边界**：尾部是场内成因（扫单 /
  本所逆选），另需机制。
- **性质定位**：`[external_skew]` 是**改善价格**的机制（每笔成交价格好一点），不是
  **避开成交**的机制（阻止坏成交发生）。尾部要的是后者。两者不互斥、不重叠，
  应作为两个独立候选各自立项。

全部读数与推论边界见
[尾部分解报告](evidence/maker-markout-tail-decomposition-2026-08-01.md)。

### 3. live 先验与阶段 4 的边界

- guard 臂 2026-07-27 accepted（成本侧 + 信号质量判）：连粗糙的二值化版本都有
  正效果，"外部价领先 StandX"这一事实已有 live 证据，被扔掉的只是幅度信息。
- **不在阶段 4 的终止链条上**：阶段 4 被否的是"drift 前倾的纯测量 = 无条件加宽
  → 加宽 live 为负"。外部价的信息来自另一个场馆，连续偏移不是加宽；18 号文档
  要求"重启须以新的独立证据立项"，guard accepted（07-27）与本节 (a)(b) 正是
  阶段 4 终止（07-20）之后到达的新独立证据。

### 4. 证据溯源与代码基线（2026-07-31 补）

上面 (a)~(d) 的全部离线数字来自**单一 run**：`baseline-pnl-20260728T081712Z`
（代码 `819f0f0`，配置 `6314a374…`，35.9h / 527 fills，
[读数报告](evidence/maker-baseline-pnl-2026-07-30.md)；(a)(b)(c) 用的是该 run
19.4h / 289 fills 时点的中间切片，末态读数为 capture +5.19 / markout@30s -5.14bps，
与 19h 时点同向同量级）。裁决时必须知道这条证据链跨了一次代码变更：

- **A/B 将跑的二进制 ≠ run1 的 `819f0f0`**。07-30/31 之间合并了 cleanup 残余判定
  硬化（[29 号文档](29-maker-cleanup-residual-verification.md) Phase 1+2）与两帧
  ack 三处修复（`4464167` / `97d14e8` / `02f6cea`）。
- **信号侧证据可以平移**：这些修复动的是撤单/下单 ack 落账与 cleanup 残余判定，
  不碰报价中心、excess 计算、band/no-cross、refresh。excess→30s mark 移动的斜率
  与成交时点的 excess 分布（(a)(b)）是市场性质与信号性质，不因订单生命周期记账
  改变。
- **水位侧证据不能平移**：uptime、fill 数、以及"两帧 ack 被误判为通道损坏"路径上
  的 PnL，都可能因修复而变。**因此 run1 不得充当 A/B 的 baseline 臂**——baseline
  必须与 candidate 跑同一个二进制、同一时期（见验收判据的编排要求）。
- 冻结基线配置文件本身未变（`maker-guard-hype-candidate.toml`，sha256
  `6314a374…`）；candidate 臂 = 该文件 + `[external_skew]` 四个键，其余不动。

## 机制设计（`[external_skew]`）

报价梯子已有唯一中心锚点：`skew_center_with`（`standx-maker/src/lib.rs`，
库存 skew，含 nonlinear 变体）。外部偏移在同一锚点上**乘性复合、置于库存
skew 之后**：

```
center = skew_center_with(cfg, nonlinear, mark, position)
         × (1 + shift_bps / 1e4)
shift_bps = clamp(λ × excess_bps, ±cap_bps)     |excess| < dead_zone 时为 0
```

- **excess 来源**：完全复用 guard 链路——同一 HL midPx feed、同一 300s 半衰期
  EWMA basis 扣除、同一 `max_age_ms` 新鲜度判定。不引入第二份配置、第二条订阅。
- **失败方向 OPEN**（与 guard 一致）：样本缺失 / 过期 / 非有限 → shift=0，
  报价照常。外部信号永远是优化，绝不能成为新的停报价来源。
- **与二值 guard 的关系**：连续偏移覆盖 `|excess| < enter_bps` 的中段；尾部
  （≥10bps）单侧 stale 是质变（价位即将被打穿），保留现有压制语义作为尾部保险。
  **候选臂 = 冻结基线（nonlinear_skew + guard 双开）+ `[external_skew]`，
  其他一个字节不动。**

### guard 激活期间的复合语义（2026-07-31 定稿）

原草案只说了"中段 / 尾部分工"，没回答 guard 实际激活时 skew 怎么办。guard 有
`enter=10 / exit=5` 的迟滞，所以存在一段 `excess ∈ [5, 10)` 且 guard **仍然激活**
的区间，这一区间必须有明确语义。**定稿：`external_skew` 无条件作用于 center，
与 guard 状态完全解耦。**

- 两者作用在**不同的杠杆**上，不存在重复计数：guard 决定"危险侧这一轮还挂不挂"
  （[lib.rs:764](../crates/standx-maker/src/lib.rs:764) 的 `guard.active &&
  endangered == Some(side)` → `continue`，整侧不挂 + resting 走
  `SideSuppressed` 撤单），skew 决定"挂着的那些单的锚点在哪"。guard 激活时危险侧
  根本没有报价，center 偏移对它无意义；**存活侧的偏移方向恰好是保护性的**——
  excess>0 时危险侧 = Sell 被摘掉，剩下 Buy，center 上推使买价更靠近即将上行的
  mark，正是"在上行前买到"的方向。
- **拒绝的备选：guard 激活期间令 shift=0**。会在 `enter_bps` 处造成不连续跳变
  （shift 从 cap 突降到 0），且把存活侧的保护性偏移一起关掉；还会让纯函数依赖
  guard 状态机，破坏无状态性与 replay 可测性。
- **代价（记录，不设门槛）**：guard 激活占比实测 1.58%（run1）/ 0.52–1.92%
  （guard 轮 6 臂），这段时间内 skew 顶在 `cap_bps` 附近的单侧报价会略微加大
  库存偏斜倾向。已被 `cap_bps` + 库存 guardrail 覆盖，不单独设判据。

### band 预算红线（2026-07-31 新增）

`external_skew` 与库存 skew 在同一 center 上**同号叠加**（最坏情况），而 band
资格区间锚定的是**真实 mark**（[lib.rs:745](../crates/standx-maker/src/lib.rs:745)
注释明确："Band eligibility is defined around the TRUE mark, not the skewed
center"）。故远侧报价到 mark 的距离在最坏情况下是三项之和，必须留在 band 内：

```
spread_bps + (levels − 1) × level_step_bps + nonlinear.cap_bps + external_skew.cap_bps ≤ band_bps
```

冻结基线代入：`8 + 0 + 12 + 8 = 28 ≤ 30`（`levels=1`，故 level_step 项为 0），
**留 2bps 余量，`cap_bps = 8` 是能取的上限**。这条红线与
[22 号文档](22-maker-stage3v1-guard-design.md)的 `spread + cap ≤ band`（8+12=20）
同源，只是多消耗了 8bps 预算。

越线的后果不是报错，是**静默劣化**，两条都值得知道：

1. **band 夹取（饱和）**：越出 band 的价格被 `clamp(price_lo, price_hi)` 夹到
   band 边沿（[lib.rs:805](../crates/standx-maker/src/lib.rs:805)），不是丢弃该侧
   ——报价照挂，但远侧钉在边沿，梯子变成非预期的不对称形状，且此后 center 再动
   对该侧价格不再有任何影响（机制在这一侧失效而无任何告警）。
2. **撤单 churn**：refresh 判据比的是 **center 漂移**而非价格差
   （[lib.rs:965](../crates/standx-maker/src/lib.rs:965)
   `bps_diff(center, r.ref_center) > refresh_bps`）。价格已被夹住时，center 继续
   移动仍会触发 `MarkMovedBeyondRefresh` → 撤单重挂到**同一个价格**，纯 churn。

- **band / no-cross / refresh 照旧**，以新 center 为锚。`refresh_bps=4` 天然
  限流：λ=0.5 时 excess 2~6bps 产生 1~3bps 偏移，单独不触发重挂，平静期不增加
  撤单 churn；死区进一步防止漂移累积造成的抖动。**但注意偏移是叠加在库存驱动的
  center 漂移之上的**，二者合成后越过 4bps 的频率高于任一单独作用，所以撤单率
  guardrail 是本轮最可能被触发的那一条（见验收判据）。
- **配置面**（`[external_skew]`，默认关，缺省即 replay 等价）：
  - `enabled`（默认 false）
  - `lambda`（偏移系数，预注册单臂 0.5）
  - `cap_bps`（偏移上限，建议 6~8，必须 < enter_bps=10，保证尾部仍归 guard 管）
  - `dead_zone_bps`（建议 1.0）

## 风险与已知限制

- **反事实估计的选择效应盲区**：上限口径假设全部 fill 平移后仍成交。真实世界
  里中心偏移留下最毒的 fill（价格打穿新旧两档）、丢掉边际好 fill——符号与
  量级日志不可算，**只能 A/B 实测**。**晋级判据必须建在 live markout 上**，
  不得用离线反事实数作为晋级证据。（2026-07-31 澄清：AS 是 live 实测量而非离线
  反事实数，但它已降为**诊断量、不得用于晋级**，与本条同向——本条挡的正是"用便宜的
  中间指标替代经济指标去晋级"。）
- **basis 漂移**：静态 basis（HYPE 观测 ~-14bps 场馆溢价）若缓慢漂移，300s
  EWMA 跟踪期内的残差会变成持续性偏移 → 单向推报价 → 库存偏斜。`cap_bps` +
  `dead_zone_bps` 是第一道防线；A/B 期间把库存均值/方差列为 guardrail。
- **EWMA 滞后**：basis 扣除是慢变量，excess 是快变量，滞后不影响 excess 的
  时效（guard 轮已论证）。
- **不解决全部毒性**：见立项依据 (d)。本机制的目标是把 30s markout 拉回一部分，
  不是归零。**2026-08-01 量化后这条已升级为硬边界**：亏损 60~72% 集中在最差 10% 的
  成交上，而外部 excess 对这 10% 无判别力（同分布、guard 零激活）。本机制的可达范围
  只有其余 90%（mo300 -2.5~-4.0bps），现实口径回收净亏 ~16%。**尾部必须另立机制**
  （线索：有毒成交单龄中位 9~26s、成交前 drift 略大；一个待验证假设是 refresh 撤单
  重挂把新单送进仍在移动的市场）。

## 遥测（归因用）

- `cycle_summary` 新增 `external_skew_shift_bps`（本轮实际应用的偏移，含 0）。
- `external_skew` 事件（shift 越过 dead_zone 或方向翻转时记录，风格同
  `external_guard` 事件）。
- **fill 事件新增 `excess_bps_at_fill`（2026-07-31 新增，AS 诊断量的直接输入）**：
  成交被观测到的时刻在用的那个 excess 样本。立项依据 (b) 的买 -3.2 / 卖 +3.65 是
  按时间把 fill join 到 `cycle_summary` 算出来的——估 σ 可以这么干，但
  **AS 要进判定报告，不该建在一次时间 join 上**（成交可能落在两个 cycle
  之间，join 的归属规则会变成读数的隐藏自由度）。
  - **两臂都必须落这个字段**，与 `external_skew.enabled` 无关——否则 baseline 臂
    没有 AS 读数，无从对比。字段可用性只依赖 guard 链路的 excess 样本
    （冻结基线里 guard 是开的，两臂都有）。样本缺失/过期时写 null，不写 0
    （0 是"无偏离"这个真实取值，与"没测到"必须可区分）。
  - 纯遥测新增，不进决策路径，replay 的 action 序列不受影响。
- 现有 `external_divergence_bps` / `external_basis_bps` / guard 事件不动。

## 实现边界

- `standx-maker`：纯函数 `external_skew_shift_bps(excess_bps, cfg) -> f64`
  （clamp + 死区，无状态、无 I/O、无时钟），以及 center 复合点；replay 等价
  （默认关）。测试：死区/clamp/符号方向/fail-open（无样本 → 0）、与库存 skew
  复合的确定性、**guard 激活期间 shift 照常施加**（复合语义定稿的回归锁）、
  **band 边沿行为**（构造越线配置，断言远侧被夹到 band 而非丢弃，锁住"静默劣化"
  这一事实本身）。
- `standx-cli`：配置解析（`[external_skew]` → domain）、把 guard 链路已算好的
  excess 喂给 planner、遥测字段与事件。不新增订阅、不改 feed。
  **配置校验**：`enabled=true` 时校验 band 预算红线
  （`spread_bps + (levels−1) × level_step_bps + nonlinear.cap_bps +
  external_skew.cap_bps ≤ band_bps`）与 `cap_bps < external_guard.enter_bps`，
  越线拒绝启动而不是静默夹取。校验放 CLI 侧（band/levels 都是 CLI 配置面的东西，
  core 纯函数不该知道梯子形状）。
- 不改动：guard 判定语义、basis half-life、refresh/band/no-cross、输出契约
  （只增不改）。

## 验收判据（预注册，live 时间片 A/B）

两臂：baseline = 冻结基线（`maker-guard-hype-candidate.toml` 原样）；
candidate = baseline + `[external_skew] enabled, lambda=0.5, cap_bps=8,
dead_zone_bps=1`。两臂**必须跑同一个二进制**（见证据溯源：run1 不能充当 baseline）。

> **2026-07-31 修订说明**：原草案写"每臂 ≥36h 或 ≥250 笔 fill" + "markout@30s 改善
> ≥2bps"，但没有给出功效论证——σ 当时未测，判据是从 guard 轮抄来的时长口径。σ 已于
> 2026-07-31 实测（见下节），结论是**原样本量在名义口径下刚好够用**，真正的约束来自
> 序列相关而非 σ。本节按实测口径重写，机制与配置面一个字节没改。

### 统计功效（σ 已实测，2026-07-31）

primary 是**逐笔**签名 markout@30s 的两臂均值差。所需样本（单侧检验，假设是方向性的：
candidate 更好；α=0.05、power=80%）：

```
n_per_arm = 2 σ² (z₀.₉₅ + z₀.₈)² / Δ²  =  12.37 σ² / Δ²
MDE(n)    = σ √(12.37 / n)
SE_diff   = σ √(2 / n)
```

**实测（2026-07-31）**：两个 baseline-pnl run 池化，n=633 笔被动成交、约 44h
（≈14.4 fills/h）。口径为下文"script 口径"（从成交时的 mark 起算，不含 capture）：

| 量 | mean | median | σ | sem |
|---|---|---|---|---|
| capture | +2.91 | +2.57 | 4.25 | 0.17 |
| markout@1s | -2.63 | -0.00 | 4.17 | 0.17 |
| markout@5s | -5.79 | -5.44 | 5.03 | 0.20 |
| **markout@30s** | **-8.09** | -7.27 | **8.60** | 0.34 |

代入 σ=8.60、Δ=2bps：

| 口径 | Δ=2bps 所需 n/臂 | 折算时长/臂 |
|---|---|---|
| 单侧（预注册口径） | **229** | 16h |
| 双侧（参考） | 291 | 20h |

n=250/臂 时：SE_diff = 0.77bps，单侧 MDE = **1.91bps**，对真实效应 2bps 的功效
≈ 83%。**即原判据的 250 笔在名义口径下刚好够分辨 2bps**——σ 的实测值远低于施工前
的先验估计（先验按波动上限推 15~25，实测 8.6），此前"样本量差 6~8 倍"的判断按
实测数据不成立。

**σ 不是瓶颈，下面两条才是**（这也是窗口长度真正的定法）：

1. **序列相关（binding）**：成交在时间上成簇，同一簇共享同一段行情，**有效样本
   小于名义样本**，上表的 sem 是下界。按簇内相关 ρ 的方差膨胀因子
   `VIF = 1 + (m−1)ρ`（m ≈ 14，即每小时的成交数）：

   | ρ | VIF | Δ=2bps 实际所需 n/臂 | 折算时长/臂 |
   |---|---|---|---|
   | 0.05 | 1.65 | 378 | 26h |
   | 0.10 | 2.30 | 527 | 37h |
   | 0.20 | 3.60 | 824 | 57h |
   | 0.30 | 4.90 | 1121 | 78h |

   **已实测（2026-07-31，block bootstrap，44 个小时块）**：只有 mo30s 真的扎堆，
   其余指标名义笔数≈有效笔数。

   | 指标 | 名义 sem | deff（1h / 2h / 4h 块） | neff（名义 633） |
   |---|---|---|---|
   | capture | 0.17 | 1.2 / 1.1 / 1.3 | ~530 |
   | markout@1s | 0.17 | 0.7 / 1.0 / 0.7 | ~660（不扎堆） |
   | markout@5s | 0.20 | 1.2 / 1.2 / 1.2 | ~520 |
   | **markout@30s** | 0.34 | **1.8 / 2.0 / 3.1** | 348 / 315 / **202** |

   **deff 只是下界**：块长拉到 4h 时 deff 还在涨（1.8→2.0→3.1），说明同一段行情的
   影响超过 4h，现有数据看不到它封顶在哪；且 4h 块只有 11 个，3.1 这个点估计的
   误差棒很粗。**这是量级判断，不是精确值**——mo30 的部分扎堆还是机械性的（相邻
   成交的 30s 前瞻窗口本身重叠），这部分不会因编排改善而消失。
2. **regime 依赖**：这 44h 是单一行情段，σ 本身跨段会漂。故窗口按"≥n 笔 fill/臂"
   计而不按小时计（candidate 的 center 偏移本身也会改变成交率），且两臂必须分块
   交替以共享同一 regime 序列（见编排节）。

**预注册样本量（2026-07-31 定稿）**：不押注 deff 这个数，押方法。

| 名义笔数/臂 | deff=3 | deff=4 | deff=5 |
|---|---|---|---|
| 700（≈48h） | neff 233 ✓ | 175 ✗ | 140 ✗ |
| **1000（≈70h）** | 333 ✓ | 250 ✓ | 200 △ |

- **硬下限：每臂 ≥700 笔 fill。** 低于此不判定，直接算 inconclusive。
- **授权按 ~1000 笔/臂（≈70h）申请**，deff 到 4 仍然够用。
- **最终置信区间用 block bootstrap 在 A/B 实测数据上算，不用假定的 deff 反推。**
  预注册的是**方法**而非 deff 的取值：deff 估错只会让区间变宽、落进已有的
  inconclusive 分支（按原参数延长窗口，不改 λ、不加臂），不会伤到判定的有效性。
- **自举块长 = 4h**（deff=3.1 的测量尺度）。自举块不得跨越换臂边界；编排的 12h
  臂块与 4h 自举块整齐嵌套（每臂块 3 个自举块），70h/臂 约 17 个块。

### markout 口径（预注册，2026-07-31 定稿）

仓库里有两个 markout 定义，差一个 capture，此前设计文档混用了两者，实测数据把它
们对上了：

| 口径 | 起算点 | 含 capture | 实测 mo30 |
|---|---|---|---|
| **script**（[`maker_markout_ab.py`](../scripts/maker_markout_ab.py) 的 mo30，**预注册采用**） | 成交时的 mark | 否 | **-8.09** |
| runner（`performance.markout_*` 遥测字段，仅记录） | 成交价 | 是 | -5.18 |

一致性校验：`-8.09 (script) + 2.91 (capture) = -5.18`，与 run1 报告的遥测读数
-5.14 吻合——两个口径确实只差一个 capture，没有第三种误差。

**采用 script 口径**：它把"成交时机好不好"与"价差挣多少"分开，而本机制治的恰恰是
前者；capture 已经单独是 guardrail，混在一个数里会让 primary 与 guardrail 相互污染。
判定脚本与功效计算共用这一个口径。（注意：立项依据 1 的 -5.8 与读数报告的 -5.14 是
runner 口径，立项依据 (d) 的 -10.6 是 script 口径下的**分桶条件**读数，都不是本节的
池化 -8.09，引用时别混。）

### primary 判据与 AS 诊断量

> **2026-07-31 两次修订，结构已变**：原草案的"Stage A 机制段 / Stage B 经济段"
> 两段式**撤销**。撤销理由是它唯一的卖点（"用便宜的中间指标先筛一遍"）经实测不成立：
> 中间量 AS 的样本需求与 markout 同量级，先跑它不省时间，分段就只剩管理成本。
> **改为单一窗口跑到底、两个指标在分析时一起算。**

**primary**：逐笔签名 markout@30s（script 口径）改善 ≥ 2bps，判定看**单侧 95%
置信下界 > 0 且点估计 ≥ 2bps**，区间用 **block bootstrap（4h 块）** 而非 iid 公式；
且 5s markout 不恶化超过 1bps（σ=5.03、deff≈1.2，分辨力充裕）。

**AS（逆选暴露）= 诊断量，不是判据**。定义：全部 passive fill 上"签名 excess"的
均值，符号约定为**正 = 外部价预告的移动方向对我们不利**（买单成交时 excess 为负
即不利，故买单取相反数、卖单取原值）。基线 ≈ **+3.4bps**（立项依据 (b) 的买 -3.2 /
卖 +3.65 池化）。

它的作用是**给 null 结果解释**——markout 没改善有两种完全不同的成因，后续动作相反
（见判定表）：AS 也没动 = 机制压根没生效，关闭方向；AS 明显改善 = 机制生效但经济量
没跟上（残余毒性占主导），转 microprice 并把本轮读数作为其立项证据。这个理由与成本
无关，因此不受 σ 修正影响。

- **不设 AS 的硬阈值**：定阈值需要 AS 的实测 σ，而该值尚未测出（见下）。它现在的
  角色是解释而非判定，定性区分足够。
- **AS 不得用于晋级**，任何情况下都不能替代 primary。这一点必须写进授权文本。
- 保留一个**中途弃疗检查**（跑到一半时看一眼，两个指标都毫无动静则 owner 可提前停）：
  纯省时间的选项，不是判定环节，不预设阈值，不对 primary 做效能声明，因此不需要对
  α 做多重比较校正。

**AS 的 σ 仍未实测（唯一剩余未知量）**。注意两个容易搞错的地方：

- **AS 的 excess 是外部量，不是场内漂移。** 它来自 `[external_guard]` 链路的
  `external_divergence_bps`（HL midPx − StandX mark，已扣 300s EWMA 静态基差），
  **不是**成交前的场内 mark 漂移（drift）。用 drift 类指标当判据会把本轮变成已终止的
  阶段 4 的翻版——立项依据 3 的边界正是"信息来自另一个场馆"。
- 文档早前写的 σ_excess ≈ 2.5 是从立项依据 (a) 的**全 cycle 无条件分布**分桶反推的
  （均值 +0.03），而 AS 要的是**成交时刻的条件分布**（均值 +3.4）。两者不是同一个
  总体，那个 2.5 无论如何都要重测。

测法（数据已具备，不占 live 时间片，不必等 `excess_bps_at_fill` 落地）：把 fill 按
时间 join 到最近的 `cycle_summary`、取该轮 `external_divergence_bps`、按上述符号约定
定号，估 σ 与 deff。**口径校验**：池化均值应落在 +3.4 附近，对不上说明 join 或符号
搞错了。预注册判据仍要求字段化（见遥测节）——**时间 join 只用于估 σ，不得回填成
预注册数**。

### guardrail（任一命中即 rejected，不论 primary）

- passive capture 掉幅 > 1bps（σ=4.25 实测，n=600 时 SE_diff=0.25bps，分辨力充裕）；
- **撤单率（cancel/quote-hour）上涨 > 50%**——本轮最可能触发的一条，机理见
  band 预算红线节（偏移叠加在库存 center 漂移之上，合成后更频繁越过
  `refresh_bps=4`）；
- 时间加权库存均值 |position| 或 p95 显著上偏（basis 漂移征兆）；
- uptime 下降 > 2pp。

### 编排：分块交替，不是两段长跑

guard 轮的 PnL 读数就是因为"两臂窗口活跃度不同，混淆无法分离"而作废的
（[25 号现状盘点](25-maker-short-term-roadmap-2026-07-27.md)）。本轮两臂按
**≥12h 的块交替**，块边界覆盖不同 UTC 时段，使两臂的 regime 暴露大致平衡。

- 代价：每次换臂要重启进程，而重启要过 cleanup / 残余判定这一关——run1/run2/run3
  三次截断有两次死在这里。[29 号文档](29-maker-cleanup-residual-verification.md)
  Phase 1+2 与两帧 ack 三处修复已合并，per-restart 风险已降低，**但这是本编排的
  已知主要风险，不是零**。块长取 12h（而非 4h）就是为了把重启次数压到 ~10 次量级。
- 交替不改善 primary 的功效（逐笔方差是主项，块设计治的是偏差不是方差），
  所以样本量仍按上表算，不因交替打折。

### 判定与后续

判定按 primary（markout@30s）走，AS 只决定 rejected 之后往哪走：

| 结果 | 处置 |
|---|---|
| primary 达标 + guardrail 全过 | **accepted**，并入新冻结基线（AS 读数记入报告作佐证，不参与判定） |
| 样本 < 700 笔/臂，或 block bootstrap 区间跨 0 | **inconclusive**：按原参数把窗口延到 n 笔，**不改 λ、不加臂**（改参数就等于重新立项） |
| 样本已达 n + primary 未达标 + **AS 也没动** | **rejected**，本立项关闭。结论是"外部价偏移挪不动成交时点"，机制本身没按模型工作 |
| 样本已达 n + primary 未达标 + **AS 明显改善** | **rejected**，但这是有信息的否决：机制生效而经济量没跟上 = 立项依据 (d) 的残余毒性占主导 → 按 18 号 v0 流程转 microprice（阻塞在 depth 观测字段），并把本轮 AS/markout 联合读数作为该候选的立项证据 |
| guardrail 命中 | **rejected**，不论 primary |

- **PnL 依旧不作晋级条件**（沿用 skew/guard 两轮口径），但逐日记入报告。
- **不做多 λ 赛马**：单臂 λ=0.5（反事实估计中点）；accepted 后如需调优另行立项。

## 明确不做

- 不用连续版替换二值 guard（尾部压制语义保留，见机制设计）。
- 不改 `basis_half_life_secs`（300s）——按 guard 判定报告声明，基差半衰期
  评估按各自证据另行立项。
- 不做多场馆加权领先价（单 HL 源，与 guard 同源）。
- 不在基线 PnL 采集窗口内部署（含"只加遥测"——运行中进程不重启、不换二进制）。

## 失效条件

- A/B 判出 rejected（回滚，本立项关闭）；
- 基线 PnL 窗口给出明确为负且 markout 无改善空间的结论（回 18 号重排）；
- HL feed 语义或 StandX mark 口径变化（excess 定义失效，需重新校准 basis）。
