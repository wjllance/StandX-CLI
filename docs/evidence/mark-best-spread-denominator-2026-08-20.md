# mark-盘口分母量化：5 品种 mark-best spread 分布 — 2026-08-20（最终定论版）

## Decision

- Status: `measurement_complete_denominator_final_2026_08_24`
- 问题：8 轮 HYPE 机制迭代（stage2/3/3v1/4/nonlinear/guard/external_skew/
  microprice）从未检验过**分母**——StandX 的 mark 相对真实盘口中点（book mid）
  到底锚得多远、半价差（maker 面对的真实触线距离）分布如何。若 mark 长期
  系统性偏在盘口单侧，或半价差厚尾远大于 spread 预算，那么任何报价中心偏移
  机制（尤其场内侧 microprice）的起效空间都被这个分母框死。
- 本测量：`scripts/lag_analysis.py` 第 4 节（mark-best spread denominator），
  把每个 StandX mark 与最近一次 `best_bid/best_ask`（depth 通道）配对，输出
  三个统计量：**锚定偏置**（mark 相对 book mid 的 signed bps）、**触线距离**
  （mark 到最近一档最优报价的 bps）、**半价差**（maker 面对的真实半个 spread）。
- **最终结论（2026-08-24，5 leg recorder 全部优雅停机，全量重扫）**：
  - **HYPE 分母仍是全品种最差**：锚定偏置 p99=+9.8bps（mark 持续偏在 book mid
    上方）、触线距离 p99=**12.6bps**、半价差 p99=5.5bps——厚尾全部远超 spread
    预算。mark 相对盘口系统性甩开的非对称距离在成熟数据下依然成立。
  - **跨品种差异巨大**：BTC 锚定 p50≈+0.1bps（mark≈mid，分母最健康）vs
    HYPE p50=+0.7、ETH p50=+1.2（mark 持续压高）；XAG 半价差 p50=2.9/p99=4.5bps
    （薄盘），XAU 触线 p99=3.1bps（三品种中触线最短）。
  - **锚定偏置有方向性**：HYPE/ETH 持续为正（mark 偏 mid 上方）、XAG 偏负
    （mean=−2.3，mark 部分时段大幅低于 mid）、BTC/XAU≈0——不同品种 mark 相对
    盘口的偏置**方向和幅度都不同**，不能用一个统一的偏移常数处理。
  - **跨场馆 lag（有 HL 腿的 BTC/ETH/HYPE）**：StandX 落后 HL 峰值相关约
    +1.25~1.75s；HL 跳变≥8bps 时 HYPE 中位跟随时长 3376ms 且 **36% 跳变在窗口
    内永远不跟随**（2247/6218），是 3 品种中跟随最慢、永不覆盖比例最高的。
  - **own-feed 快撤窗口（HYPE）**：自身 mark 跳变后 50% 覆盖中位 ~3s、90% 覆盖
    中位 ~5.8s——自身 feed 开始移动后仍有 ~3–6s 可操作窗口，own-feed 快撤路线
    在 HYPE 上可行性高。
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
- 数据（**全量重扫，2026-08-24 停机后最终状态**）：
  - `var/standx/lag-rec-20260806T110820Z.ndjson`（HYPE，1.5GB，2026-08-06T11:08Z
    起至 08-24，**18 天**，506,452 对配对样本）
  - `var/standx/lag-rec-20260820T065924Z-{BTC,ETH,XAU,XAG}.ndjson`
    （各 105–376MB，2026-08-20T06:59Z 起至 08-24，**~3.8 天**，各 108,672 对）
  - 4 个新品种 recorder 已全部 SIGTERM 优雅停机（PPID=1/systemd 1520 原全程运行），
    数据已含完整周期尾部；记录器 fleet cron 已暂停。
- 命令示例：`python3 scripts/lag_analysis.py var/standx/lag-rec-....ndjson --tick-bps 0.5`

## Results（最终全量）

### 锚定偏置 anchor（mark relative to book mid, signed bps）

| sym | paired | p50 | p90 | p95 | p99 | mean |
| --- | --- | --- | --- | --- | --- | --- |
| BTC | 108,672 | +0.1 | 2.1 | 2.9 | 4.7 | +0.1 |
| ETH | 108,673 | **+1.2** | 5.7 | 6.7 | 8.6 | +1.8 |
| XAU | 108,669 | −0.1 | 1.1 | 1.8 | 2.9 | −0.2 |
| XAG | 108,673 | −1.4 | 1.4 | 2.9 | 6.5 | −2.3 |
| HYPE | 506,452 | +0.7 | 5.7 | 7.1 | **9.8** | +0.6 |

### 触线距离 touch（mark to near best quote, bps）

| sym | p50 | p90 | p95 | p99 |
| --- | --- | --- | --- | --- |
| BTC | 0.5 | 2.2 | 2.9 | 4.4 |
| ETH | 1.2 | 4.1 | 5.0 | 6.9 |
| XAU | 1.3 | 2.4 | 2.6 | 3.1 |
| XAG | 2.9 | 5.7 | 5.9 | 8.6 |
| HYPE | 1.4 | 5.5 | 7.3 | **12.6** |

### 半价差 half spread（maker-facing, bps）

| sym | p50 | p90 | p95 | p99 |
| --- | --- | --- | --- | --- |
| BTC | 0.5 | 1.2 | 1.5 | 2.3 |
| ETH | 1.6 | 2.6 | 3.0 | 3.9 |
| XAU | 1.3 | 2.4 | 2.6 | 2.8 |
| XAG | **2.9** | 3.7 | 3.7 | 4.5 |
| HYPE | 1.6 | 3.5 | 4.1 | **5.5** |

### 跨场馆 lag（有 HL 腿的 3 品种，第 1–3 节）

| sym | peak corr lag | events≥8bps | 永不覆盖% | 50% 覆盖中位 |
| --- | --- | --- | --- | --- |
| BTC | +1250ms (r=0.081) | 379 | 17% (64) | 2773ms |
| ETH | +1750ms (r=0.081) | 969 | 29% (281) | 2952ms |
| HYPE | +1750ms (r=0.081) | 6218 | **36% (2247)** | **3376ms** |

## Interpretation

1. **HYPE 分母确实差，且差在「锚定偏置 + 触线厚尾」两条**：
   - 锚定偏置 p50=+0.7、p90=+5.7、p99=+9.8bps 表示 mark **持续稳定地偏在
     book mid 上方**——成熟 18 天下不是噪声，是系统性结构。任何以 mark 为锚的
     报价中心，都比真实盘口中点高约 1–10bps。
   - 触线距离 p99=**12.6bps** 是重灾区：尾部时刻 mark 与真实盘口脱节超过 12.6bps，
     正好是 HYPE 线盈利被吞的量级来源。半价差 p50=1.6/p99=5.5bps——即便把报价
     中心挪到 book mid，maker 面对的真实触线距离在尾部仍达 ~5.5bps。这对照 8 轮
     迭代的 spread/band 预算，直接回答「为什么很多机制改善被分母吃掉」。
   - **数据越足，厚尾越严重**：中间快照（HYPE 前 30 万行）触线 p99=6.0、半价差
     p99=4.7；全量 18 天触线 p99 升到 12.6、锚定 p99 9.8——早期快照低估了尾部。
     中期 put-off（08-20 说「等跑满数天重扫」）得到验证，值得标本。
2. **microprice 的场内侧假设被部分证实、部分复杂化**：mid_bias 信号（mark 相对
   盘口）确实是正均值（HYPE ~+0.6、ETH ~+1.8），说明 mark 不是中性锚点。
   但**偏置方向跨品种不同**（HYPE/ETH 正、XAG 负），一个统一的偏移 lambda 无法
   同时适配，必须按品种/按价区自适应——这是 microprice 从「单固定 lambda」走向
   「按品种自适应」的量化依据。
3. **跨品种分母梯度 = 机制收益上界**：BTC 锚定 p50=+0.1、半价差 p50=0.5（分母
   最干净），HYPE/ETH/XAG 依次恶化。若要评选「哪个品种的 microprice / 报价中心
   优化最划算」，分母分布直接给出排序：BTC 已接近顶效，HYPE/ETH/XAG 是真正有
   分母空间的品种——与「HYPE 迭代最吃力」的现象吻合。
4. **跨场馆 lag 增强「own-feed / 外部价」两条正交通道**：HYPE 跟随 HL 中位 3376ms
   且 36% 永不跟随（mark cadence 慢），外部价（external_skew）可治；而 own-feed
   快撤窗口 ~3–6s 说明自身 feed 开始移动后反应仍有充足余量——两条路线与中线
   平移（microprice）正交，可叠加。

## Honest limitations

- **跨品种严格对比仍有保留**：BTC/ETH/XAG/XAU 各 3.8 天、HYPE 18 天，时长不同；
  但各品种都已是「成熟数据」而非快照，尾部/波动时段已覆盖充分。
- 配对窗口 5s：HYPE mark 更新 ~3s 一拍，5s 内配对到的 book 可能已微旧，锚定偏置
  含小幅 book-陈旧噪声；短周期品种（BTC/ETH mark 更密）此误差小。
- 触线距离 = min(|mark−bid|,|ask−mark|)，只反映到近端一档，不含深度/第二档。
- 跨场馆 lag 的绝对偏移含固定差分网络延迟偏置（host→StandX vs host→HL），变量
  部分（跟随时间、永不比例）稳健；跑道与 maker 同机同区时代表性最高。
- 记录器已停机，本次为**最终定论**；如需更多品种或更长窗口需重新启动 recorder。

## Data integrity

- 5 leg recorder 全程存活至 2026-08-24 优雅停机（SIGTERM，PPID=1/systemd 1520），
  HYPE 18 天 / 其余 4 品种 3.8 天全部 append 完成为止。记录器 fleet cron
  （`5c6caeb4f125`）已暂停。
- 分析脚本为 read-only：`python3 scripts/lag_analysis.py <ndjson>` 对同一输入可
  复现同一数字；第 4 节在无 Hyperliquid 腿的 XAU/XAG 上仍运行（回归已修，见
  PR #380）。
- 最终数字由全量重扫得出：HYPE 506,452 对、BTC/ETH/XAU/XAG 各 108,672 对，远高于
  中间快照（HYPE 21,707 / 各 1,139 对），统计可靠性充分。