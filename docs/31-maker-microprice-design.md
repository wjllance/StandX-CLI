# 场内侧 touch-mid 中心偏移（`[microprice]`）立项设计（2026-08-11）

状态：**实现已完成（默认关，待对抗 review 后判 A/B 时机）**。本文档是外部领先价偏移
（[28 号文档](28-maker-external-skew-design.md)）的**场内侧互补**候选。

## 一、一句话

把报价中心从「StandX mark 锚点」偏移到「成交时刻的场馆内盘口 touch-mid 相对该 mark
的偏置」方向上前进：填单发生在盘口 touch-mid 已压/挂离 mark 时，30s markout 系统性
恶化；跟随盘口能避开最毒的成交窗口。信号只需 `best_bid/best_ask/mark` 三个周期现成
字段，**零新增 feed、零阻塞**（突破 28 号文档「microprice 被卡在 depth 字段」的旧判断）。

## 二、立项依据（证据链，2026-08-11 离线实测）

6 个 stage-2 A/B 臂（251MB，`/tmp/mo-arms`）池化被动成交，script 口径 mo30（从成交时
mark 起算，不含 capture）：

### 1. 信号：mid_bias = ((best_bid+best_ask)/2 − mark) / mark（bps）

与 mo30 的 **Spearman ρ = -0.528**，逐桶单调：

| 成交时 mid_bias 桶 | n | mo30 均值（bps） |
|---|---|---|
| < -2（盘口压在 mark 下） | 182 | **+8.98** |
| -2..-0.5 | 26 | +4.97 |
| -0.5..0.5 | 13 | -2.59 |
| 0.5..2 | 33 | -10.47 |
| > +2（盘口挂 mark 上） | 130 | **-16.42** |

### 2. 反事实：翻正

| 指标 | 值 |
|---|---|
| 总 mo30 | -749.7 bps·fills（均 -1.95） |
| 避免 `bias>+2` 的成交（n=130）后剩余 | **+1384.5（翻正 ✓）** |
| 避免掉的那部分 | -2134.2，即最毒 34% 成交通着 -16.42，占全部亏损的大头 |

> **诚实边界（选择效应）**：上面是「避开该桶成交」的 *selection* 反事实，不是「中心
> 偏移后实际成交」的效果。偏移会让一部分原本成交的单子改价/离开、也会把原来不成交的
> 单子放进来，符号与量级离线不可算——**最终必须 A/B 实测 markout**，反事实只用来
> 证明「机制有真实可逼近的信号」，不充当晋级证据。（同 28 号文档的盲区声明。）

### 3. 与外部价信号的关系（互补，不是重复）

- `[external_skew]` 治「外部领先价 vs StandX mark」的偏置（领导人 feed）。
- `[microprice]` 治「场馆自身盘口 vs mark」的偏置（场内流量/时序）。
- 28 号文档 (d) 明确残余毒性 `excess≈0 桶 mo30 仍 -10.6` 指向场内流量——正是本机制要
  覆盖的那部分。两者同号叠加进同一 center 合成（`total_shift = external + micro`）。

## 三、机制设计（`[microprice]`）

完全复用 `[external_skew]` 脚手架，在 `quote_center` 合成点**同号相加**：

```
center = skew_center_with(cfg, nonlinear, mark, position)        # 库存锚点
         then × (1 + total_shift_bps / 1e4)                      # 外部+场内偏移
total_shift_bps = external_shift_bps + micro_shift_bps
micro_shift_bps = clamp(λ × mid_bias_bps, ±cap_bps)   |mid_bias| < dead_zone 时为 0
```

- **信号来源**：`plan_cycle` 内直接由 `CycleInput.market`（best_bid/best_ask/mark）算出，
  无第二个订阅、无新 feed。`mid_bias` 缺失/non-finite/mark≤0 → `None` → fail-open 归零。
- **失败方向 OPEN**（与 guard/external 一致）：永远只是优化，绝不成为停报价来源。
- **默认关**（`enabled=false`）→ `micro_shift_bps=0` → `total_shift==external_shift` →
  报价中心、ref_center、action 序列与未配置时**逐字节一致**（replay 等价红线）。

### 配置面（默认关，缺省即 replay 等价）

```toml
[microprice]
enabled      = false
lambda       = 0.5
cap_bps      = 6.0
dead_zone_bps = 0.5
```

- `cap_bps=6` 建议 ≤ external 的 8，且受 band 预算红线约束（`spread + nonlinear.cap +
  external.cap + micro.cap ≤ band`）。默认不强制校验（默认关），启用时由 CLI 校验。

## 四、遥测（归因用，只在 cycle_summary 增字段，不进决策路径）

- `cycle_summary.micro_price_shift_bps`：本轮实际施加的场内偏移（含 0；与
  `external_skew_shift_bps` 并列，便于拆解总 shift 的两个成分）。
- 不改动 `external_*` 任何既有字段。纯遥测新增，replay 的 action 序列不受影响。

## 五、实现边界

- `standx-maker`：纯函数 `micro_price_shift_bps`（无状态、无 I/O、无时钟），`plan_cycle`
  内算 `mid_bias_bps` + 合成 `total_shift_bps`；replay 等价（默认关）。
- `standx-cli`：TOML `[microprice]` 解析 → `MicroPriceConfig` 线程 + 遥测字段。
- 不改动：guard 判定、basis half-life、refresh/band/no-cross 语义、输出契约（只增）。

## 六、验收判据（预注册，live A/B）

> 平台同 28 号文档：两臂同一二进制、≥12h 块交替。**判据建在 live markout 上，反事实
> 数不用于晋级。**

- **primary**：逐笔签名 markout@30s（script 口径）**改善 ≥ 2bps**，单侧 95% 置信下界
  > 0；5s markout 不恶化 >1bps。样本量按 28 号文档实测 σ=8.60 / block bootstrap。
- **guardrail**：passive capture 掉幅 >1bps、撤单率上涨 >50%（偏移叠加在库存漂移上最可能
  触发这条）、|position| 均值/p95 上偏、uptime 降 >2pp —— 任一命中即 rejected。
- **AS 诊断量**：成交时刻 `excess_bps_at_fill`（28 号遥测字段）用于判 rejected 后走关闭
  还是转更细 depth-microprice（需 depth 落盘，仍为第二优先）。

## 七、明确不做

- 不新增 depth/量字段订阅（本轮用纯价格信号；depth-microprice 留作 rejected 后的方向）。
- 不做多场馆加权、不做盘口量失衡（缺 depth）、不改 guard/external 语义。
- 不在基线 PnL / external_skew 采集窗口内同时部署。

## 八、风险与已知限制

- **选择效应盲区**（见二.2）：真实改善只能 A/B 测，符号/量级离线不可算。
- **与库存 skew 复合**：偏移叠加在库存 center 漂移上，撤单率 guardrail 是最可能触发的
  红线（同 28 号文档）。
- **mark 是否实时**：mid_bias 对 mark 的绝对口径敏感；mark 滞后会让偏置失真。cap 兜底。
- **单一场馆源**：信号来自 StandX 自身盘口，若 StandX 盘口与真实成交价长期错位，偏置
  会带偏差；用 A/B 期间 inventory/position guardrail 监控。
