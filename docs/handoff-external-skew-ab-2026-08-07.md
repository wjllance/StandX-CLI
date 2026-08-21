# Handoff:external_skew A/B 实验 + 方案 0 快屏（2026-08-07）

> 给接手的 agent：这份文档假设你零上下文。先在会话里读一遍，再行动。
> 写文档时有一个**未处理的实盘残余仓位**和一个**停着的实验**，先看第 0 节。

---

## 0. 紧急事项（按优先级）

### 0.1 残余仓位 0.10 HYPE 多头 —— 等用户授权

- 08-06 23:12Z fail-safe 停机留下 `0.10 HYPE-USD` 多头（~5.6 USD），裸敞口至今。
  maker 永不自动平仓，需人工/授权处理。**无残余挂单**（cleanup 经 query_order
  确认两笔均 canceled）。
- 用户授权后的平仓方式（先 `--dry-run` 验证，再去掉 dry-run 执行）：
  ```bash
  cd /home/lance/workspace/bossx/standx-cli
  sudo bash -c 'set -a; . /etc/standx/maker-stage2-hype-ab.env; set +a; exec target/release/standx order create HYPE-USD sell market --qty 0.1 --reduce-only --yes --dry-run'
  ```
- **未取得明确授权前不要平仓**（AGENTS.md 安全红线）。

### 0.2 A/B 实验停机 —— 等用户决定重启

- 容器 `standx-maker-stage2-ab-hype` 已退出（exit 75，08-06 23:12Z）。编排器
  打出 `CRITICAL stage2 A/B stopped` 后整体停止，此后无报价无数据。
- 重启命令（编排器自动从下一臂边界继续，已完成臂 manifest 都在）：
  ```bash
  cd /home/lance/workspace/bossx/standx-cli/deploy/docker
  sudo STANDX_LOG_DIR_HOST=/opt/standx/var/standx docker compose --profile ab-hype up -d --force-recreate
  sudo docker logs -f standx-maker-stage2-ab-hype 2>&1 | grep -E 'arm starting|arm complete|CRITICAL'
  ```
- **JWT 2026-08-09 09:00Z 到期**。重启前建议先让用户给新 JWT，更新
  `/etc/standx/maker-stage2-hype-ab.env` 的 `STANDX_JWT=` 行（先备份，文件
  0600 root，需要 sudo；旧备份 `.bak-20260801T1628Z`）。若用户已刷新会话
  环境变量，直接取会话里的 `STANDX_JWT`。
- 注意：方案 0 的成交↔行情对接分析**依赖实验在跑产生新 fill**。实验停着，
  快屏录得再久也没有可对的成交。重启决策同时 gate 两条线。

### 0.3 定时任务随旧会话消亡 —— 必须重建

旧会话的两个 cron（每小时健康报告、每 4 小时读数）绑定旧 kimi 会话，新会话
**不会继承**。确认实验重启后按第 3 节模板重建。

---

## 1. 实验全貌（external_skew A/B）

- 设计文档：`docs/28-maker-external-skew-design.md`（判据、guardrail、编排、
  授权记录都在末尾，授权 commit `46cabec`）。
- 机制：candidate 臂在外部（Hyperliquid）价格偏离时对报价中心做有界偏移
  （λ=0.5，cap_bps<enter_bps=10），治的是"成交时点被快市场狙击"。
- 编排：`scripts/run_maker_stage2_ab.sh`，docker compose profile `ab-hype`，
  镜像 `standx-stage2-ab:latest`（从 git `6a3fb3c` 构建）。按 **12h 块交替**
  两臂，换臂约在每天 **05:13Z 和 17:13Z**。
  - baseline 配置 `examples/maker-guard-hype-candidate.toml`，sha 前缀 `6314a37462e3`
  - candidate 配置 `examples/maker-external-skew-hype-candidate.toml`，sha 前缀 `99bec5eabee8`
- 臂日志：`/opt/standx/var/standx/stage2-{baseline,candidate}-<时间戳>-<哈希>.ndjson`
  （root 0600，**读取一律 sudo**）。本实验只取：baseline 时间戳 >=
  20260801T171015Z 且哈希 6314a37462e3;candidate >= 20260802T051046Z 且哈希
  99bec5eabee8。**更早期的 stage2-* 文件属于旧实验，严禁混入。**
- 判定（预注册，docs/28):
  - primary：mo30（script 口径）改善 ≥ 2bps 且单侧 95% 置信下界 > 0;
  - 硬下限：两臂各 **700 笔** fill；不足则 inconclusive，按原参数延长，不改 λ;
  - guardrail 任一命中即 rejected:capture 掉幅 > 1bps;cancel/quote-hour
    涨幅 > 50%；时间加权库存 |pos| 均值或 p95 显著上偏；uptime 降 > 2pp;
  - AS（签名 excess）只是诊断量，只在 rejected 后决定走向，**不得用于晋级**;
  - PnL 逐日记入报告，不作晋级条件。

## 2. 当前进度快照（08-06 12:12Z 读数 + 事故）

| 指标 | baseline | candidate |
|---|---|---|
| fills（硬下限 700） | 414（59%） | 363（52%） |
| capture | +3.51 ±0.20 | +3.57 ±0.32 |
| mo5 | -6.19 ±0.23 | -7.12 ±0.32 |
| mo30 | -7.97 ±0.33 | -7.92 ±0.50 |
| AS | +3.70 ±0.14 | +3.12 ±0.16 |
| 累计 PnL | -0.999 | -0.806 |
| neff（mo30, 4h 块） | 305 | 201 |

- mo30 连续 8 轮在 ±0.2bps 内（噪声带 ±0.9)，实质打平；AS 差距连续 14 次
  同向（candidate 低 ~0.5bps)。
- guardrail:capture ✅;**撤单率 +31%（阈值 +50%，接近区，逐臂
  105→172→156→187→99/h)**⚠️；库存 ✅;uptime -0.52pp ✅。
- **事故臂**:`stage2-baseline-20260806T171421Z-6314a37462e3.ndjson` 只跑了
  ~6h（25 笔，PnL +0.01，被 fail-safe 截断）。判定时这臂的 fill 照计
  （markout 脚本按 fill 逐笔算），但要在判定报告里注明截断原因
  （order-response 断流 → freeze → cleanup 首次残余 → fail-safe 停机；
  与 29 号文档 cleanup 路径同一族，是编排已知主要风险）。
- 事故时间线：23:11Z order-response 流断 → freeze → cleanup attempt1 残余 2
  单 → 23:12:34Z 重试确认撤净，但 fail-safe 已触发停机并留下 0.10 多头。

## 3. 日常运维（重建 cron 用）

收件人 open_id（BossX):`ou_a7b8a38029ac2112422871a5f22aaf9c`。
lark-cli 发消息固定格式：
```bash
LARKSUITE_CLI_NO_UPDATE_NOTIFIER=1 LARKSUITE_CLI_NO_SKILLS_NOTIFIER=1 \
  lark-cli im +messages-send --user-id ou_a7b8a38029ac2112422871a5f22aaf9c \
  --as bot --markdown "<内容>"
```

### 3.1 每小时健康报告（建议 cron `23 * * * *`)

固定命令（**必须 `sudo env "PATH=$PATH" "HOME=$HOME"`**，否则 sudo 下找不到
nvm 的 lark-cli):
```bash
cd /home/lance/workspace/bossx/standx-cli
latest=$(sudo ls -t /opt/standx/var/standx/stage2-*.ndjson | head -1)
sudo env "PATH=$PATH" "HOME=$HOME" python3 scripts/baseline_pnl_report.py \
  "$latest" --run-id "$(basename "$latest" .ndjson)"
```
- 输出含 ALERT 或非零退出要在回复里给出告警内容；正常一行确认。
- 报告里"进程 DEAD"判定在换臂 wind-down 期间可能误报，若 cycle 时间戳仍在
  推进按正常处理。
- 顺带做 lag-recorder 看门狗（见 4.2)。
- 容器不在/无臂日志 = 实验已停，不发报告并提醒用户删任务。

### 3.2 每 4 小时读数总结（建议 cron `11 */4 * * *`)

三个数据源的完整流程：
```bash
files=$(sudo ls /opt/standx/var/standx/stage2-baseline-2026*-6314a37462e3.ndjson \
  /opt/standx/var/standx/stage2-candidate-2026*-99bec5eabee8.ndjson)
# 注意按 1 节的时间戳规则过滤本实验臂
sudo python3 scripts/maker_markout_ab.py $files > /tmp/ab_summary.txt 2>&1   # markout/PnL/neff
sudo python3 scripts/maker_guardrail_ab.py $files                            # 三条 guardrail
# AS 诊断量：fill 事件的 excess_bps_at_fill，买单取反卖单原值，逐臂池化
# （heredoc 见旧 cron 模板或按此描述重写，~20 行 python）
```
报告必须含：fills/进度、capture、mo5、mo30（含 sem)、AS、累计 PnL、neff、
**guardrail 段（四条各带当前值与 ok/接近阈值/FAIL 判定，撤单率 +30%~+50%
标"接近阈值"并给逐臂趋势）**、与上轮的趋势对比、换臂健康、必要提醒，并
标注"读数非判定"。**数字必须当次从脚本输出抄，禁止凭记忆。**

## 4. 方案 0：快屏 / 反应速度路线（下一阶段主线）

### 4.1 已有结论（详见 evidence 文档）

`docs/evidence/quote-staleness-cadence-vs-signal-2026-08-06.md`:
- 不利跳动（≥8bps/2s,107 次）检出瞬间：3s 刷新报价过期 ~11.3bps,0.5s 刷新
  ~9.4bps——**刷新节奏只值 ~2bps，这条路封掉**；
- 跳动亚秒完成，77% 在 StandX mark(3s 一拍）上完全不可见；
- **StandX 自有 BBO feed 250ms 一拍，检出瞬间已含跳动的 68%**——真正的杠杆
  是检测信号速度，own-feed 就有快信号；
- 扫单扎堆：被扫一次后下一笔再被扫概率 48.5% vs 平时 5.7%（历史测量）;
- 纪律：任何"收摊省钱"的评估必须**先减掉随机收摊基线**（8-02 的坑：最好
  的报警器减完基线只剩噪音）；离线结果不定案、不批准上线。

### 4.2 正在录的快屏数据

- 进程：`target/release/standx lag-recorder --symbol HYPE-USD --out
  var/standx/lag-rec-20260806T110820Z.ndjson --status-secs 300`(nohup
  脱离会话，只读无认证）。08-06 11:08Z 起录，与 maker 同主机。
- 看门狗（挂在每小时 cron 里）:`pgrep -f 'standx lag-recorder'` + 最新
  `var/standx/lag-rec-2026*.ndjson` mtime 超过 10 分钟没更新 → 告警并用同
  命令换新时间戳文件重启。
- 旧数据：`var/standx/lag-rec-20260720T050910Z.ndjson`(44.5h，结论见
  `docs/evidence/lag-recorder-hype-result-2026-07-22.md`)。

### 4.3 下一步（等实验重启 + 录够 ~48h 重叠后）

做 08-02 没做成的对接分析（成交↔快屏逐笔 join):
1. **BBO 背离报警器认扫单的准确率**：对照基准（平时 ~10% 成交被扫、旧报警
   器 17%);
2. **减掉随机收摊基线后的净节省**（掷骰子对照，必须的纪律）。
两个数都明显过线才立项写代码（候选：BBO/HL 背离触发快撤/快移）；不过就
埋掉。别重蹈候选 6（先立项后发现信号够不着）。

## 5. 环境与操作要点

- 工作目录 `/home/lance/workspace/bossx/standx-cli`;Rust workspace
  (standx-cli / standx-maker / standx-sdk)，边界与测试要求见 `AGENTS.md`。
- `/opt/standx/var/standx/` 与 `/etc/standx/` 均为 root 0600，一律 sudo;
  docker compose 也要 sudo（否则读不了 env 文件）。
- OpenObserve 容器在本机 127.0.0.1:5080 运行，臂日志自动上传。
- **不要 git commit/push**，除非用户明确要求。
- 工作区未提交内容：
  - 本线新增（未提交）:`scripts/maker_guardrail_ab.py`、
    `scripts/quote_staleness.py`、
    `docs/evidence/quote-staleness-cadence-vs-signal-2026-08-06.md`、本文档；
  - 他人改动（**不要动**):`docs/27*`、`scripts/maker_markout_ab.py` 的
    block bootstrap 部分。
- 安全红线：fail-closed 不变量（AGENTS.md)；不授权不平仓/不重启实盘；改
  maker 核心要跑 `cargo test --workspace --offline` + clippy + fmt，并过
  对抗 review。
- 用户是 release owner(BossX)，中文交流，风格直接；报告数字宁缺勿假。

## 6. 速查

| 项 | 值 |
|---|---|
| 容器 | `standx-maker-stage2-ab-hype`(compose profile `ab-hype`) |
| env 文件 | `/etc/standx/maker-stage2-hype-ab.env`(0600，备份 `.bak-20260801T1628Z`) |
| 臂日志 | `/opt/standx/var/standx/stage2-*-<hash>.ndjson` |
| baseline 哈希 | `6314a37462e3`(>= 20260801T171015Z) |
| candidate 哈希 | `99bec5eabee8`(>= 20260802T051046Z) |
| 换臂时刻 | 每天 ~05:13Z / ~17:13Z |
| 设计文档 | `docs/28-maker-external-skew-design.md` |
| 判读脚本 | `scripts/maker_markout_ab.py` / `maker_guardrail_ab.py` |
| 快屏录制 | `var/standx/lag-rec-20260806T110820Z.ndjson`(08-06 11:08Z 起） |
| 陈旧度证据 | `docs/evidence/quote-staleness-cadence-vs-signal-2026-08-06.md` |
| JWT 到期 | **2026-08-09 09:00Z** |
| 残余仓位 | 0.10 HYPE 多头（08-06 23:12Z 起，待授权处理） |
