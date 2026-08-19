# Handoff: microprice A/B 实盘计划 — 激进推进版（2026-08-13）

> 给接手的 agent：这份文档假设你零上下文。先在会话里读一遍，再行动。
> 设计文档：`docs/31-maker-microprice-design.md`
> 实验协议：`docs/28-experiment-protocol.md`
> 版本：**激进版**（cap=6 一步到位 + band=40 + 中期停损 + 跳过 canary）

---

## 0. 当前状态

- **实验运行中**。candidate 臂先跑，run_id=`stage2-candidate-20260813T113033Z-933a05012c9d`。
  arm_seconds=21600（6h），约 17:30 换 baseline。
- **microprice 代码已合入 main**：commit `9f168ae`（实现）+ `df069c5`（配置校验修复）。
- **镜像已重建**：`standx-stage2-ab:latest` 基于 `df069c5`，含 microprice 代码。
- **编排器门禁已扩展**：新增第 (h) 种模式（microprice pair），脚本 sha=`df069c5`。
- **arm_seconds**：21600（6h），arm_max_seconds=28800（8h）。
- **first_arm**：candidate（`STANDX_STAGE2_FIRST_ARM=candidate` 写入 env 文件）。
- **无残余仓位、无残余挂单**（启动前已平仓清理）。
- **Bayesian 监控 cron 已设**：job_id=`647af7f7f836`，每 30 分钟检查，accept/reject/futile 必送达。

---

## 1. 实验设计（激进版）

### 一句话

在 external_skew candidate 基础上加 `[microprice]`，**cap=6 一步到位**，band 放宽到
40bps 给足偏移空间。跳过 canary，直接 full run。加中期无效性停损，快速判死刑。

### 为什么激进

离线信号很强（Spearman ρ=-0.528，反事实翻正），不值得用 cap=2 磨三步再到 cap=6。
一步到位 → 一轮 4 天出方向 → 不行就换下一个方向，不恋战。

激进的是**参数选值和推进节奏**，不是安全红线：
- ✅ 单变量原则（两臂只差 `[microprice]` 一个 section）
- ✅ 预注册判据（开跑后冻结）
- ✅ fail-open（信号缺失不停报价）
- ✅ fail-closed（account/order 流断 → freeze → cleanup）
- ❌ 跳过了 canary（理由：microprice 默认关、fail-open、纯优化，最坏是没效果）
- ❌ 跳过了逐步升 cap（理由：离线信号强，cap=2 很可能欠饱和）

### 基线与候选

| | Baseline | Candidate |
|---|----------|-----------|
| 配置文件 | `examples/maker-microprice-hype-baseline.toml` | `examples/maker-microprice-hype-candidate.toml` |
| nonlinear_skew | ✓ | ✓ |
| external_guard | ✓ | ✓ |
| external_skew | ✓ | ✓ |
| microprice | ✗ | ✓（cap=6, lambda=0.5, dead_zone=0.5） |
| band_bps | 40 | 40 |
| 差异 | — | 仅 `[microprice]` 一个 section（5 行） |

> **为什么 baseline 也用 band=40 而不是原来的 30？**
> 单变量原则：两臂只能差 microprice。如果 candidate band=40, baseline band=30，
> 你就不知道效果是来自 microprice 还是来自"带更宽了"。
> 所以 baseline 也升到 band=40。代价是 baseline 不再与旧实验直接可比，
> 但这一轮我们回答的是"microprice 有没有用"，不是"带更宽有没有用"。

### Band 预算校验

```
Baseline:  spread(8) + nonlinear.cap(12) + external.cap(8) = 28 ≤ 40  ✓
Candidate: spread(8) + nonlinear.cap(12) + external.cap(8) + micro.cap(6) = 34 ≤ 40  ✓
```

余量 6bps，给库存 skew 的漂移留空间。

---

## 2. 预注册判据（开跑后冻结）

**目标函数**：逐笔签名 markout@30s 改善 ≥ 2bps

### Bayesian 序贯检验（激进版，2026-08-13 新增）

> 用 OpenObserve `action=fill` 事件的 `excess_bps_at_fill` 字段实时算 posterior。
> 脚本：`~/.hermes/scripts/microprice_bayesian_ab.py`
> cron：job_id=`647af7f7f836`，每 30 分钟触发一次。

**先验**：`delta = candidate_mean - baseline_mean ~ Normal(μ=0, σ=5bps)`
（95% 先验概率落在 ±10bps，保守、对零信号友好）

**后验更新**（共轭正态）：
```
likelihood:  diff_of_means ~ N(delta, se²)
             se = sqrt(se_cand² + se_base²)
posterior:   delta ~ N(post_mu, post_sigma²)
```

**预注册停止规则**（以下任一满足即停，不等样本量固定）：

| 决策 | 触发条件 | 含义 |
|------|----------|------|
| **accept** | P(δ>2bps) ≥ 0.95 **且** min(n) ≥ 100 | candidate 显著更优，晋级 |
| **reject** | P(δ>0)   ≤ 0.05 **且** min(n) ≥ 50  | candidate 显著更差，放弃 |
| **futile** | P(δ>2bps) ≤ 0.20 **且** min(n) ≥ 150  | 方向可能对但幅度不够，停 |
| **continue** | — | 数据尚不足以判断，继续跑 |

**输出格式**（cron 会自动送达）：
```
baseline : n=xxx  mean=+x.xxxbps  std=x.xxxbps
candidate: n=xxx  mean=+x.xxxbps  std=x.xxxbps
delta    :  ±x.xxxbps
posterior: μ=±x.xxx  σ=x.xxx  se=x.xxx
P(>0bps) :  xx.x%
P(>2bps) :  xx.x%
➤ 🟢 ACCEPT / 🔴 REJECT / 🟡 FUTILE / 🔵 CONTINUE
```

### 中期无效性停损（Futility Stop）

开跑后每满 50 fills 做一次中期评估（用截至当时的数据）。
以下任一条件命中 → 终止整轮实验，判 `rejected`，换下一组参数：
- **200 fills 时**：mo30 改善 < 0.5bps 且点估计为负 → 停
- **300 fills 时**：mo30 单侧 80% CI 上界 < 2bps → 停（大概率达不到目标）

理由：大部分坏方向 200 fills 就能看出趋势，没必要等满 fills 浪费实盘时间。
注意：**只有在效果明确不行时才停**。如果方向对但 CI 还宽，继续跑。

### 运维门槛（任一不过 → rejected）
- [ ] 双边 uptime ≥ 80%
- [ ] 零安全违规（无未解释仓位失配、无残余单、无 fail-open）
- [ ] manifest valid，cycle 序列完整

### 经济门槛
- [ ] mo30（逐笔签名）改善 ≥ 2bps，单侧 95% CI 下界 > 0
- [ ] mo5 不恶化 > 1bps

### Guardrail（任一命中 → rejected）
- [ ] passive capture 掉幅 > 1bps
- [ ] 撤单率上涨 > 50%
- [ ] |position| 均值/p95 上偏
- [ ] uptime 降 > 2pp

### 红线
- microprice 必须 fail-open：信号缺失/异常时偏移归零，不停报价
- 报价不得推出 40bps band（由 cap 配置间接保证）

### 明确不作为晋级条件的指标
| 指标 | 为什么不作条件 | 谁在什么时候补 |
|------|----------------|----------------|
| 净 PnL | 市况不可比，A/B 只回答相对问题 | 冻结基线采集（docs/27）单独回答 |
| 成交率变化 | microprice 会改价，成交率变是预期内的 | 观测项，记入判定报告 |
| band 放宽的单独效果 | 本轮两臂 band 都是 40，测不出 band 贡献 | 如果 microprice accepted，下一轮测"回到 band=30 + microprice" |

---

## 3. 启动前检查清单

### 3.1 代码与镜像

1. `git fetch origin` 确认 main 含 `9f168ae` 和 `df069c5`
2. 重建镜像：
   ```bash
   cd ~/workspace/bossx/standx-cli
   sudo docker build -f deploy/docker/Dockerfile -t standx-stage2-ab:latest .
   ```
3. 验证二进制含 microprice：
   ```bash
   sudo docker run --rm standx-stage2-ab:latest standx maker --help 2>&1 | grep -i microprice
   ```

### 3.2 配置校验

1. baseline: `shasum -a 256 examples/maker-microprice-hype-baseline.toml`
2. candidate: `shasum -a 256 examples/maker-microprice-hype-candidate.toml`
3. 用编排器脚本的 diff 校验器验证两臂之差：
   ```bash
   STANDX_STAGE2_BASELINE_CONFIG=examples/maker-microprice-hype-baseline.toml \
   STANDX_STAGE2_CANDIDATE_CONFIG=examples/maker-microprice-hype-candidate.toml \
   STANDX_STAGE2_VALIDATE_ONLY=1 \
   bash scripts/run_maker_stage2_ab.sh
   ```
   （需要先给脚本加 microprice 白名单模式 — 见第 6 节）

### 3.3 账户与权限

1. JWT 状态：检查 `/etc/standx/maker-stage2-hype-ab.env` 里 `STANDX_JWT=` 是否在有效期
2. 确认无残余挂单/仓位
3. 确认没有其他 maker 进程在跑

---

## 4. 启动命令

```bash
cd ~/workspace/bossx/standx-cli/deploy/docker

# 关键 env 变量（均写入 /etc/standx/maker-stage2-hype-ab.env）
# STANDX_STAGE2_ARM_SECONDS=21600          # 臂长 6h
# STANDX_STAGE2_ARM_MAX_SECONDS=28800      # 硬上限 8h
# STANDX_STAGE2_FIRST_ARM=candidate        # candidate 先跑
# STANDX_BASELINE_CONFIG=/app/examples/maker-microprice-hype-baseline.toml
# STANDX_CANDIDATE_CONFIG=/app/examples/maker-microprice-hype-candidate.toml

# 正常停止（让 maker cleanup 撤单）
sudo docker compose --profile ab-hype down

# 启动
sudo docker compose --profile ab-hype up -d
```

### 启动后验证

```bash
sudo docker logs -f standx-maker-stage2-ab-hype 2>&1 | grep -E 'arm starting|arm complete|CRITICAL|error'
```

---

## 5. 监控与读数

### 健康监控（cron，每 30 分钟）

**自动 Bayesian 监控**（已设 cron，job_id=`647af7f7f836`）：
```bash
python3 ~/.hermes/scripts/microprice_bayesian_ab.py
```
- accept/reject/futile → 自动送达当前 chat
- continue/insufficient → 静默（不打扰）

**手动快速检查**：
```bash
# 容器是否在跑
sudo docker ps --format "{{.Names}} {{.Status}}" | grep standx

# 最新臂日志
sudo docker logs standx-maker-stage2-ab-hype 2>&1 | grep -E "arm starting|arm complete|CRITICAL|error|fill" | tail -20

# 实时监控（滚动）
sudo docker logs -f standx-maker-stage2-ab-hype 2>&1 | grep -E "cycle_summary|fill|CRITICAL"
```

### 中期读数（~48h / ~200 fills 后首次）

**Bayesian 脚本已自动每 30 分钟运行**，无需手动触发。
如需手动查：
```bash
python3 ~/.hermes/scripts/microprice_bayesian_ab.py --json
```

### 最终判定

- 满 700 fills/臂 或 6 对臂（或提前 accept/reject/futile 停损）
- 结果写入 `docs/evidence/maker-microprice-ab-judgment-YYYY-MM-DD.md`
- 裁决用固定词汇：`accepted` / `rejected` / `ab_completed_not_accepted`

---

## 6. 编排器脚本改动（启动前必做）

`scripts/run_maker_stage2_ab.sh` 的配置 diff 校验器目前只支持 7 种白名单模式（a-g），
microprice 不在白名单里，直接跑会被拦（exit 64）。

需要新增第 (h) 种模式：

> (h) microprice pair：baseline 带 pre-registered `[external_skew]`，
> candidate 再加 pre-registered `[microprice]` block（enabled=true, lambda=0.5,
> cap_bps=6.0, dead_zone_bps=0.5），其余字节完全相同。

这是**门禁收紧方向**的扩展（新增一个明确的白名单 case，不是放宽校验）。

需要改的地方：
1. 新增 `PREREG_MICROPRICE` 字典（预注册参数）
2. 新增 `microprice_sections()` 函数（剥掉 `[microprice]` section，返回剩余 + 参数）
3. 在主判定逻辑里加 `microprice_pair` 分支

---

## 7. 授权记录

> 开跑前由用户授权，填入并 commit 到 main。

- **授权人**：_待填_
- **授权时间**：_待填_
- **首臂启动时间（UTC）**：_待填_
- **baseline 配置哈希**：_待填_
- **candidate 配置哈希**：_待填_
- **git commit**：_待填_
- **预注册判据文档**：本文档第 2 节
- **备注**：激进版 — cap=6 + band=40 + 中期停损，跳过 canary

---

## 8. 激进推进的节奏（不止这一轮）

如果这轮 microprice 的结论是：

- **accepted** → 立刻开下一轮：microprice + band=30（测能不能在窄带里也有改善，
  即不依赖带变宽）；同时开始准备下一个候选方向（depth-microprice、
  主动快撤、spread 不对称等），不等这轮完全结束就开始准备下一轮的代码和文档。

- **rejected** → 立刻换下一个方向，不花时间调参。从排期表里挑信号最强的下一个
  （比如 depth-microprice、主动快撤），一周内上实盘。

- **ab_completed_not_accepted**（有改善但不达 2bps）→ 调一个参数（比如 lambda 或 cap）
  快速再测一轮，但最多追加 1 轮，不做参数扫描。

**核心原则：快速判死刑，快速切换方向。** 不要在一个方向上磨 6 周。

---

## 9. 实验结论（2026-08-19）— ACCEPTED

**判定：接受 candidate（microprice@cap=6/band=40）为新默认 baseline。**

最终统计（OpenObserve，48h 窗，passive_maker）：

| 指标 | baseline | candidate |
|---|---|---|
| n | 102 | 39 |
| excess 均值 | -0.38 bps | +0.72 bps |
| std | 4.96 | 3.37 |
| delta | | +1.10 bps |
| P(δ>0) | 93.3% | |
| P(δ>2bps) | 10.2% | |

阶段判定：INSUFFICIENT_DATA（min_n=39 < 50，未达 accept/reject/futile 阈值）。信号偏正（baseline 负均值吃 adverse selection，candidate 转正），Lance 决策：**接受 candidate**。

**收尾动作（2026-08-19）：**
- 优雅停容器 `standx-maker-stage2-ab-hype`（exit 0，maker cleanup 撤净；latest arm `933a05012c9d` 正常 finished）。
- 账户校验：`orders=[]` AND `positions=[]` 干净。
- 新 baseline 定为 candidate 内容（含 `[microprice]`），哈希 `933a05012c9d`。
- 旧 baseline（无 microprice，`ed8f270451ee`）存档：`examples/maker-microprice-hype-baseline.arch-20260819.toml`。
- 镜像/编排器 env 未动，等下一轮实验时随新 candidate 一起重建。
- `examples/maker-microprice-hype-candidate.toml` 已删除：promote 之后它与新
  baseline 逐字节相同，留着只会让人误起一个两臂一样的 A/B（编排器现在也会直接
  拒绝逐字节相同的两臂）。下一轮定下新方向时再建。

> **本文档 1/3/4 节里的路径是开跑当时的。** promote 之后，那一轮实际跑的两臂
> 对应到现在的文件是：
> baseline = `examples/maker-microprice-hype-baseline.arch-20260819.toml`（`ed8f270451ee`），
> candidate = `examples/maker-microprice-hype-baseline.toml`（`933a05012c9d`）。
> 这一对仍然能通过编排器的 (h) 门禁，实验可复现。

**下一轮候选方向**（见第8节，Lance 有下一步想法待验证，具体新 candidate 待定）：depth-microprice / 主动快撤 / spread 不对称；band 收窄测窄带是否仍改善。
