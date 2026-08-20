# mark-盘口分母量化：5 品种 mark-best spread 分布 — 2026-08-20（快照）

## Decision

- Status: `measurement_in_progress_denominator_snapshot_2026_08_20`
- 问题：8 轮 HYPE 机制迭代（stage2/3/3v1/4/nonlinear/guard/external_skew/
  microprice）从未检验过**分母**——StandX 的 mark 相对真实盘口中点（book mid）
  到底锚得多远、半价差（maker 面对的真实触线距离）分布如何。若 mark 长期
  系统性偏在盘口单侧，或半价差厚尾远大于 spread 预算，那么任何报价中心偏移
  机制（尤其场内侧 microprice）的起效空间都被这个分母框死。
- 本测量：新增 `lag_analysis.py` 第 4 节（mark-best spread denominator），
  把每个 StandX mark 与最近一次 `best_bid/best_ask`（depth 通道）配对，输出
  三个统计量：**锚定偏置**（mark 相对 book mid 的 signed bps）、**触线距离**
  （mark 到最近一档最优报价的 bps）、**半价差**（maker 面对的真实半个 spread）。
- 本快照结论（5 品种，HYPE 14 天成熟数据 + 其余 4 品种 ~1h 新数据）：
  - **HYPE 分母最差**：锚定偏置 p50=+1.9bps（mark 持续偏在 book mid **上方**），
    p95=+6.2、p99=+7.6bps；半价差 p50=1.6、p90=3.4、**p99=4.7bps**——厚尾远超
    spread 预算。mark 相对盘口系统性甩开一个可量化的、非对称的距离。
  - **跨品种差异巨大**：BTC 锚定 p50=+0.3bps（mark≈mid，分母最健康）vs
    HYPE p50=+1.9、ETH p50=+2.5（mark 持续压高）；XAG 半价差 p50=3.7/p99=5.2bps
    （薄盘），XAU 触线 p90=2.0bps。
  - **锚定偏置有单侧性**：XAG 锚定 mean=**−4.6**bps 但 p50=−0.7（左偏长尾，
    mark 部分时段大幅低于 mid），HYPE/ETH 则持续为正——不同品种 mark 相对盘口
    的偏置**方向和幅度都不同**，不能用一个统一的偏移常数处理。
- 安全红线：本测量是 read-only 分析（无认证、无订单、不共享 maker 代码路径），
  不改任何报价行为；仅追加遥测统计。离线结果不定案、不批准上线（沿用 08-06
  证据文档硬规则 1）。

## Setup

- 工具：`scripts/lag_analysis.py` 第 4 节（`load_standx_mark_book` +
  `mark_best_spread` + `spread_summary`，stdlib only）。配对窗口 `pair_window_ms`
  = 5000ms（mark 与最近 book 报价年龄差 ≤5s 才配对）。
- 指标定义（每对 mark/bid/ask，mid=(bid+ask)/2）：
  - `anchor_bps = (mark/mid − 1)×10⁴` —— 锚定偏置，signed（+ = mark 在 mid 上）
  - `touch_bps = min(|mark−bid|, |ask−mark|)/mark×10⁴` —— mark 到最近最优报价距离
  - `half_bps = (ask−bid)/2/mid×10⁴` —— maker 面对的半价差
- 数据：
  - `var/standx/lag-rec-20260806T110820Z.ndjson`（HYPE，1.24GB，2026-08-06T11:08Z
    起，**14 天**，本快照取前 30 万行 ≈ 21,707 对配对样本）
  - `var/standx/lag-rec-20260820T065924Z-{BTC,ETH,XAU,XAG}.ndjson`
    （各 ~1–4MB，2026-08-20T06:59Z 起，**~1 小时**，全量扫描各 1,139 对）
- 命令示例：`python3 scripts/lag_analysis.py var/standx/lag-rec-....ndjson --tick-bps 0.5`

## Results（2026-08-20 快照）

### 锚定偏置 anchor（mark relative to book mid, signed bps）

| sym | paired | p50 | p90 | p95 | p99 | mean |
| --- | --- | --- | --- | --- | --- | --- |
| BTC | 1,139 | **+0.3** | 1.6 | 1.8 | 2.3 | +0.4 |
| ETH | 1,139 | **+2.5** | 6.2 | 6.6 | 7.5 | +2.7 |
| XAU | 1,139 | −0.2 | 1.2 | 1.7 | 2.8 | −0.3 |
| XAG | 1,139 | −0.7 | 0.7 | 1.5 | 2.2 | **−4.6** |
| HYPE | 21,707 | **+1.9** | 5.4 | 6.2 | **7.6** | +2.0 |

### 触线距离 touch（mark to near best quote, bps）

| sym | p50 | p90 | p95 | p99 |
| --- | --- | --- | --- | --- |
| BTC | 0.4 | 1.1 | 1.6 | 2.0 |
| ETH | 1.8 | 5.3 | 5.8 | 6.6 |
| XAU | 0.9 | 2.0 | 2.2 | 3.1 |
| XAG | **3.0** | **9.0** | 9.0 | **10.5** |
| HYPE | 1.4 | 3.8 | 4.6 | 6.0 |

### 半价差 half spread（maker-facing, bps）

| sym | p50 | p90 | p95 | p99 |
| --- | --- | --- | --- | --- |
| BTC | 0.5 | 0.9 | 1.1 | 1.4 |
| ETH | 0.7 | 1.6 | 1.8 | 2.4 |
| XAU | 0.6 | 2.3 | 2.6 | 2.7 |
| XAG | 3.7 | 4.5 | 5.2 | 5.2 |
| HYPE | **1.6** | **3.4** | 3.9 | **4.7** |

## Interpretation

1. **HYPE 分母确实差，且差在「锚定偏置 + 半价差厚尾」两条**：
   - 锚定偏置 p50=+1.9、p95=+6.2、p99=+7.6bps 表示 mark **持续稳定地偏在
     book mid 上方**——不是噪声，是系统性结构。任何以 mark 为锚的报价中心，
     都比真实盘口中点高约 2bps（中位）到 7.6bps（尾部）。
   - 半价差 p90=3.4、p99=4.7bps：即便把报价中心挪到 book mid，maker 面对的真实
     触线距离在尾部仍达 ~5bps —— 这对照 8 轮迭代的 spread/band 预算，直接回答
     「为什么很多机制改善被分母吃掉」：mark 本身锚错 + 盘口薄，报价中心偏移的
     可校准量级只有 ~2bps（中位），尾部被 ~5bps 的半价差 + 离散 tick 兜底。
2. **microprice 的场内侧假设被部分证实、部分复杂化**：mid_bias 信号（mark 相对
   盘口）确实是正均值（HYPE ~+2bps），说明 mark 不是中性锚点——microprice 治的
   正是这块非零偏置。但**偏置方向跨品种不同**（HYPE/ETH 正、XAG 负），一个统一
   的偏移 lambda 无法同时适配，必须按品种/按价区自适应。
3. **跨品种分母梯度 = 机制收益上界**：BTC 锚定 p50=+0.3、半价差 p50=0.5（分母
   最干净），HYPE/ETH/XAG 依次恶化。若要评选「哪个品种的 microprice / 报价中心
   优化最划算」，分母分布直接给出排序：BTC 已接近顶效，HYPE/ETH/XAG 是真正有
   分母空间的品种——与「HYPE 迭代最吃力」的现象吻合。

## Honest limitations

- **本快照是中间态，非定论**：4 个新品种 recorder 21-08-20 06:59Z 才启动，
  ~1 小时、各 1,139 对样本，可能没覆盖到波动/薄盘极端时段；HYPE 用 30 万行
  快照（~前 14 天的开头段），不代表全周期尾部。**最终结论需等 recorder 跑满
  数天后对全文件重扫**（预计 2026-08-23 后可做定论版）。
- 配对窗口 5s：HYPE mark 更新 ~3s 一拍，5s 内配对到的 book 可能已微旧，锚定
  偏置含小幅 book-陈旧噪声；短周期品种（BTC/ETH mark 更密）此误差小。
- 触线距离 = min(|mark−bid|,|ask−mark|)，只反映到近端一档，不含深度/第二档。
- 单点快照横向可比，但各品种数据量、波动时段不同，跨品种**严格**对比需等同等
  时长数据。

## Data integrity

- 5 leg recorder 均存活（cron `5c6caeb4f125` 每 30min 校验），HYPE 14 天 / 其余
  4 品种 ~1h，文件按 append 持续增长（HYPE 已 1.24GB）。
- 分析脚本为 read-only：`python3 scripts/lag_analysis.py <ndjson> --tick-bps 0.5`
  对同一输入可复现同一数字（去空格/逐 token 解析已在 ad-hoc 验证中交叉核对，
  第 4 节在无 Hyperliquid 腿的 XAU/XAG 上仍运行——回归已修）。
- ad-hoc 验证（/tmp，已清理）：确定性 fixture 40 对全部配对、半价差数学精确
  2.0bps、tick/10bps 直方图齐全、5 腿解析 alive —— 12/12 通过。
