# MH 估算器设计（纯观测，未接入）

## 状态

`design_only`。本文只规定将来的纯观测估算器。**本变更不实现它，也不把它接进
`preflight_cycle`、`plan_cycle`、`reconcile`、guard、skew、波动熔断或退出。**

对应 Maker OKR v2 的 O1.4。`docs/36` 留给以后的 live 预注册，本文不用那个编号。

campaign 页上的规则尚未核实（OKR KR2.1）。下面每一个会改变估算结果的数都是
**待核实输入**。缺输入时输出 `null`，禁止用转述值填上。

待核实、禁止写成常量或测试夹具默认值的输入：

| 输入 | 含义 | 核实前 |
|---|---|---|
| `unit_size` | 计分用的单位规模 | `null`，不估算有效规模 |
| `per_side_cap` | 每一侧计入的数量上限 | `null`，不把报价量当成已封顶的量 |
| `hourly_cap` | 一小时 MH 计分上限 | `null`，不报「是否打满」 |
| `weight_curve` | proximity → 权重，含每一档距离 | `null`，不把权重当成 1 |
| `tier_table` | 双边在带内分钟数 → Standard / Boosted（或其他档） | `null`，不贴档位名 |
| `proximity_reference` | 距离相对 mark 还是 best | 不默认选一个 |
| `minute_credit` | campaign 怎样把一分钟算进档位 | 见下文，只报我们自己的观测分钟 |
| `session_multiplier` | 交易时段、休市乘数 | `null`，不乘 1 冒充「无休市」 |

权重曲线包括贴参考价的那一档和带边的那一档，两档都算未核实，不在代码或本文算例里写死比例。

## 它算什么

每个整点小时出一条观测，四项都来自这一小时里的报价状态，不来自新的下单决策：

1. **双边在带内的分钟数。** 只有买侧和卖侧在同一分钟里都处于合格带内，这一分钟才计入。单边在带内不计。
2. **Standard / Boosted 档位。** 只做 `tier_table` 的查表。表还没签字时，档位是 `null`。
3. **按 proximity 权重折算的有效规模。** 先用 `per_side_cap` 截断该侧数量，再乘 `weight_curve` 在该侧距离上的权重。曲线或 cap 缺任一，有效规模是 `null`。
4. **每侧 cap。** 把输入里的 cap 原样回显，并报告该侧报价量有没有超过它。cap 未提供时，回显和「是否超过」都是 `null`。

这四项是观测，不是 campaign 得分。campaign 的分钟记账、档位边界和权重公式核实之前，另两个字段保持 `null`：`campaign_minutes`、`campaign_effective_size`。不要把观测值抄进这两个字段。

## 观测分钟怎么数

时间桶只用服务端时间毫秒，按整点切小时，小时内再切 60 个时钟分钟。没有服务端时间的样本记入 `dropped_missing_server_time`，**不**用本机收到时间补桶。

一分钟记为 `both_in_band` 须同时满足：

- 该分钟被已有的报价区间盖满，没有缺口；
- 每个重叠区间的 `eligible_bid_qty > 0` 且 `eligible_ask_qty > 0`。

两侧都用 performance 区间里已经记着的合格数量。估算器不另写一套「在不在带内」。

覆盖不满的分钟是 `unknown`，不是 `false`。`both_in_band_minutes` 只数 `both_in_band`。只要这一小时有 `unknown` 分钟，档位就是 `null`：不把残缺小时标成 Standard。

`minute_credit` 仍是待核实输入。核实结果若与「盖满且双边合格才计 1 分钟」不同，只允许填 `campaign_minutes`，不许改写 `both_in_band_minutes`。

## 有效规模

对每一个 `both_in_band` 分钟、每一侧：

```
capped_qty = min(eligible_qty, per_side_cap)
weight     = weight_curve.lookup(distance_bps)
side_size  = capped_qty * weight
```

`distance_bps` 相对 `proximity_reference`。参考价未核实、该分钟没有对应价格、或距离落在曲线未覆盖的档上时，这一侧该分钟的 `side_size` 为 `null`。小时有效规模是两侧 `side_size` 在全部 `both_in_band` 分钟上的时间加权平均；任一分钟为 `null`，小时值就是 `null`，不许把缺的权重当成 1，也不许丢掉这一分钟再平均。

截断再乘权重是观测公式，字段名用 `observation_effective_size`。它不是 campaign 公式。

`hourly_cap` 只参与「观测值是否超过输入上限」的布尔回显。上限缺失时该布尔为 `null`。估算器不因超过上限去撤单或缩小下一笔报价。

## 模块边界

将来实现时放在 `standx-maker` 的独立模块（建议 `mh_estimate`），纯函数：

```text
estimate_hours(samples, inputs) -> Vec<HourEstimate>
```

- `samples` 只用已经记在 trace 里的类型：报价区间的时间、两侧合格数量、mark、best。不读 CLI、网络、时钟或终端。
- `inputs` 是上面的待核实表。全空是合法输入，结果里所有 campaign 字段和依赖缺输入的字段为 `null`。
- 返回值不写进 `CyclePlan`、`Action` 或 ledger。
- CLI 只在开关打开时把 `HourEstimate` 打成单独的观测行。开关默认关。

不在 `standx-sdk` 里做这个估算。交易所协议不拥有计分规则。

## 开着和关着

开关关闭时：

- 不调用 `estimate_hours`，或不采用它的返回值；
- 不增加 JSON 字段、不增加 action；
- `run_replay` 的 `Action` 序列与开关不存在时逐字节相同。

开关打开时：

- 可以在 replay 之外多跑一遍 `estimate_hours`；
- `Action` 字节仍必须与关闭时相同。多出来的只有观测记录。
- 缺输入不得触发任何 place、cancel、hold 或 exit。

实现时的等价测试：

1. 同一条 trace、同一份 `MakerConfig`，关闭与打开各跑一次 `run_replay`，断言 action 字节相等。
2. `inputs` 全空时，每一小时的 `tier`、`observation_effective_size`、`campaign_minutes`、`campaign_effective_size`、`hourly_cap` 都是 `null`。
3. 把实现改成在缺权重时使用常数权重，测试 2 必须变红。这是观测不变量，不是报价安全测试，但同样要用变异确认。

现有 `quote_geometry` 的做法是对照：诊断可以出现，action 字节用改动前的字面量钉住。MH 估算比那更严——它甚至不进 `CyclePlan`。

## 明确不做

- 不改报价锚、`external_skew`、撤单、退出、停机。
- 不把估算结果送进任何门控。OKR 写明本月不做身前量、排名、贴脸一类检测闸。
- 不读 Binance，不用外盘价计算 proximity。
- 不在未签字前把任何品种的 Unit Size、cap、每小时上限或权重写进示例配置。
- 不改 `scripts/run_maker_stage2_ab.sh`，不改正在 live 的候选配置。

## 和盘口/成交毫秒日志的关系

小时桶要的是服务端时间。mark 的 `server_time` 已经在 `ws_snapshot` 里。盘口和公共成交的毫秒日志是另一条默认关闭的观测（`server_time_book` / `server_time_trade`），同样不进决策。估算器实现时可以事后读这些行，但不从本机时钟补服务端时间。那些行里的 `size_ahead` 和 `our_rank` 只在报文真有对应字段时才有值；它们不是 MH 公式的输入，除非以后的核实文本明确把它们写进计分。
