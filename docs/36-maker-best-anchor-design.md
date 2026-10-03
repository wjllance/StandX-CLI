# Best-anchor 报价模式（OKR v2 · 3.1①）立项设计

状态：**代码已落地，默认关闭，未授权 live。** 本文只覆盖 3.1①。
逐笔止损（3.1③）、小时块跳过（3.1④）、两小时停机（3.1⑤）不在本次范围内。

## 一、一句话

开关关闭时，报价、撤单、退出与现在的 mark 阶梯逐 action 相同。
开关打开时，买卖价锚定各自的 best bid / best ask；mark 只用来判断价格是否还在现有合格带内。
离盘口的距离（stand-off）至少是 `max(1 秒 best 跳动 p99.9, 1 tick) + margin`。
带内放不下这个距离时，这一侧不报价；若缺的不是仓位帽或 external guard，另一侧一起撤出。

## 二、开关

TOML，缺省即关。任何 live / baseline / stage2 配置都不要写 `enabled = true`。

```toml
[best_anchor]
enabled = false
# 1 秒 best 价跳动的 p99.9，价格单位，由操作者离线测好后填入。
# 规划器不估计它，也不新开行情源。
best_jump_p999 = 0.0
# 加在 max(best_jump_p999, 1 tick) 之后的额外价格距离。
margin = 0.0
```

`enabled = false` 时，即使 `best_jump_p999` / `margin` 非零，也不参与任何价格或撤单算术。
非法值（非有限、负数）在关闭时也会被启动校验拒绝。

XAU 的外部参考仍是 XAUT，BTC 仍是 Binance。本模式不新增市场数据订阅。

## 三、启用后的价格

```text
stand_off = max(best_jump_p999, one_tick) + margin
level_distance = stand_off + level * touch * level_step_bps / 1e4
raw_buy  = (best_bid - level_distance) * (quote_center / mark)
raw_sell = (best_ask + level_distance) * (quote_center / mark)
```

- `quote_center / mark` 是现有库存 skew、`external_skew`、`microprice` 已经算出的相对偏移。这三个机制保持开启，不在本模式里被关掉。`nonlinear_skew` 与 `external_guard` 同样保持原语义。
- 偏移之后，价格仍不得近于该档的 stand-off。偏移把减仓侧拉进 stand-off 时，夹回 stand-off 边界，而不是穿进盘口。
- 可行区间是「mark 合格带」∩「stand-off 之外」∩「不穿对手价」。三者没有交集则丢掉该档，不把报价夹进 stand-off 里。
- 缺一边 touch、盘口交叉、或 stand-off 配置无效：这一轮若进入规划，两边都不报，不退回 mark 阶梯。预检直接跳过的轮次（缺 touch、交叉盘、mark/mid 偏离）仍沿用现有约定：不规划、也不撤已挂单。坏行情不触发盲目撤补；这是 `main` 上已有的行为，本模式不改它。
- `spread_bps` 在本模式里不再是离 mark 的半价差。它仍是关闭时的半价差，并继续参与 `band_bps > spread_bps` 的启动校验。

Anti-flicker 锚从 mark 中心改为 **book mid**，两边记同一个 `ref_center`。mid 相对下单时漂移超过 `refresh_bps` 时，两边一起撤、一起重挂。任一活着的报价钻进自己的 stand-off 时，也是两边一起撤；撤单原因仍是既有的 `mark_moved`，不新增 action 名。开启后这个原因同时表示「共享 book 锚要求两边退出」。

### 两边一起进退，以及它不覆盖的东西

一起进退做在 best-anchor 自己的路径上，没有第二个开关：

- 定价：两边用各自 touch ± 同一个 stand-off。
- 某一档装不进合格带时，只撤这一档的另一侧（`dropped_infeasible`）。两边都放得下的内档留下。整侧都装不下时，结果仍是两边都不报。
- 刷新，以及仍被想要的报价钻进 stand-off：两边所有活着的报价一起撤。

以下仍可以只压一侧，因为它们不是本模式的报价锚，而且任务要求保持原样：

- `external_guard` 仍只压受威胁的一侧。
- `|position| >= max_position` 仍只压加仓侧。
- 多档暴露帽（`cap_desired_exposure`）仍可按预算丢掉一侧的外档。

## 四、遥测

`cycle_summary` 增加两个字段，不改既有字段：

- `best_anchor_enabled`（默认 `false`）
- `best_anchor_standoff`（关闭时为 `0`，价格单位）

`ref_center` 的字段名不变。关闭时它仍是 mark 报价中心；开启时它是 book mid。

## 五、预注册判据（stub，未冻结，不得据此开跑）

下面是 [28 号规程](28-experiment-protocol.md) 的空表。数字在开跑前由 release owner 填上并 commit。填之前改这张表不算「改判据」。**现在不许开跑。**

### 预注册判据（未入库冻结）

- 目标函数：少亏 —— 成交更靠近真实盘口之后，30s markout 的下行被压住；不要求净 PnL 转正
- 臂长与样本量：待填。两臂只差 `[best_anchor].enabled`（以及该臂必须填写的 `best_jump_p999` / `margin`）。baseline 臂不得打开本开关
- 裁决人：release owner

**运维门槛（任一不过 → rejected）**

- [ ] 双边 uptime ≥ 待填的绝对值
- [ ] 零安全违规（无未解释仓位失配、无残余单、无 fail-open）
- [ ] manifest valid，cycle 序列完整

**经济门槛**

- [ ] 待填：签名 markout@30s 的方向与门槛。离线反事实不作晋级依据

**红线（越线即 rejected）**

- 不得把报价推出现有合格带。装不下 stand-off 就两边不报，而不是夹到带外或夹进 stand-off
- 不得为了本实验关闭 `external_skew`、`microprice`、`nonlinear_skew`、`external_guard`
- 不得在同一次实验里打开 3.1③ / 3.1④ / 3.1⑤

**明确不作为晋级条件的指标（必填）**

| 指标 | 为什么不作条件 | 谁在什么时候补 |
|------|----------------|----------------|
| 净 PnL | 臂间市况不可比；A/B 只回答相对 markout / uptime | 沿用 28 号未判项，不在本轮新开 |
| 绝对 stand-off 是否「最优」 | p99.9 与 margin 是输入，不是本轮要搜的参数 | 单独的参数实验，且一次只动一行 |

## 六、打开 live 之前仍然缺的东西

1. **按品种测好的 1 秒 best 跳动 p99.9。** BTC 与 XAU 分开。代码不估计这个数。没写进候选配置之前，没有可解释的 stand-off。
2. **margin 没有校准。** 它只是 p99.9 与 1 tick 之外的操作者余量。
3. **本节第五节的判据仍是 stub。** 开跑前必须填门槛、commit、冻结。没有 owner 授权文本。
4. **`microprice` / `external_skew` 与 book 锚叠加可能重复计算 mark/book 缺口。** 偏移仍会施加（机制不许关掉），但是否双重计数要在对抗 review 和只差这一行的 A/B 里看，不能靠离线假设放行。
5. **按档一起退出会砍 uptime。** 某一档装不进带时，该档的另一侧也撤；内档可以留下。整侧都装不下时两边都不报。这是故意的，但是否击穿 uptime 门槛没有 live 读数。
6. **对抗 review 只修了可复现的档位配平，没有 live canary。** 多档时「整侧有报价就保留」会留下单边外档；已改为按档配平，并用单测钉住。预检跳过仍不撤单（见第三节），这是 `main` 上的既有约定，不在本变更里改。关闭等价已有单测；启用路径没有生产验证。
7. **不要改** `scripts/run_maker_stage2_ab.sh`，也不要在 baseline / stage2 / live toml 里打开本开关。
