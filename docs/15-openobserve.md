# OpenObserve maker 日志

这是面向单机、小规模 maker 的最小日志方案。maker 的 stdout 先完整写入本地
NDJSON，stderr 单独保存；启用自动上传后，运行期间按 checkpoint 增量上传，退出时
再做一次最终补传。远端不可用时，原始日志仍保留，上传错误不会改变 maker 的退出
状态，也不会进入报价循环。

## 1. 启动本地 OpenObserve

```bash
cp deploy/openobserve/.env.example deploy/openobserve/.env
# 修改 OPENOBSERVE_PASSWORD，保留大小写字母、数字和特殊字符
make openobserve-up
```

管理界面：<http://127.0.0.1:5080>。Compose 只绑定 loopback，数据保存在
`deploy/openobserve/data/`，该目录和 `.env` 均不进入 Git。

停止或查看服务日志：

```bash
make openobserve-logs
make openobserve-down
```

## 2. 运行并采集 maker

先把 OpenObserve 环境变量导入当前 shell：

```bash
set -a
source deploy/openobserve/.env
set +a
export OPENOBSERVE_AUTO_UPLOAD=1
# 可选，默认每 2 秒上传一次新增事件
export OPENOBSERVE_UPLOAD_INTERVAL=2
```

纸面示例：

```bash
scripts/run_maker_observed.sh \
  target/debug/standx --output json maker run XAG-USD \
  --maker-config examples/maker-xag-100u.toml
```

包装器会：

1. 为每次运行生成唯一 `run_id`。
2. 将 stdout 写入 `var/standx/<run_id>.ndjson`。
3. 将 stderr 写入 `var/standx/<run_id>.stderr.log`，同时保留终端显示。
4. 将基线身份和结束完整性写入 `var/standx/<run_id>.manifest.json`，不修改 maker JSON。
5. 转发 Ctrl+C/TERM 给 maker，并等待 lifecycle/cleanup 日志写完。
6. 若 `OPENOBSERVE_AUTO_UPLOAD=1`，启动时验证 OpenObserve 连接，运行期间持续增量上传。
7. maker 退出后等待 lifecycle/cleanup 落盘，再做最终补传。

sidecar manifest 包含完整 `git_sha`、整仓 dirty paths、策略/运行时 source dirty paths、实际
执行程序 SHA-256、采集脚本 SHA-256、配置内容哈希、非敏感 CLI 策略覆盖、symbol、UTC
时间窗、日志 SHA-256、cycle 缺口、lifecycle 完整性和统一 regime 摘要。摘要包括首末/
极值 mark、净移动/range bps、directionality、halt/fallback、最大 `vol_bps`、fills 和
平均 uptime；包装器不会记录 webhook、凭据或完整原始命令。若要把 run 晋级为阶段 0
baseline，还需在启动前从权威 symbol metadata 填入：

```bash
export STANDX_BASELINE_PRICE_TICK_DECIMALS=2
export STANDX_BASELINE_QTY_TICK_DECIMALS=4
export STANDX_BASELINE_MIN_ORDER_QTY=0.0001
```

`standx-maker/standx-sdk/standx-cli`、Cargo 锁定文件和本次使用的通用 maker 配置必须相对
`git_sha` clean；文档或采集脚本本身可以处于待提交状态，但完整 dirty paths 会保留，实际
参与采集的脚本还会记录 collector SHA-256。策略源码不 clean、缺少身份字段、存在非法 JSON/缺 cycle、缺
`started/stopped` lifecycle 或进程非零退出时，manifest 会令
`validation.baseline_eligible=false`，不会静默纳入比较基线。

采集后重新核对 manifest 判定和原始日志哈希：

```bash
python3 scripts/maker_run_manifest.py validate \
  --manifest var/standx/<run_id>.manifest.json \
  --repo-root .
```

终端看到以下信息表示启动检查和实时上传正常：

```text
OpenObserve live uploader starting: run_id=... interval=2s
OpenObserve preflight ok: org=default stream=standx_maker
OpenObserve live upload: run_id=... uploaded=... checkpoint=...
```

上传失败只会输出 warning；checkpoint 不前移，网络恢复后自动补传，本地 maker 不停机。
Dashboard 页面需要选择包含当前时间的范围，并把自动刷新设为 `5s`（或手动点击刷新），
即可在 maker 运行期间看到新增周期、成交和告警。

实盘仍需显式 `--live --yes`，采集脚本不会自行开启实盘，也不会读取或上传
`STANDX_JWT`、`STANDX_PRIVATE_KEY`。OpenObserve 密码通过环境变量传入，不出现在
命令参数或日志中。

## 3. 导入已有日志

先离线验证；非 JSON 行会被统计并跳过：

```bash
python3 scripts/openobserve_ingest.py --dry-run \
  docs/evidence/logs/maker-paper-20260710T0705Z.log
```

确认后上传：

```bash
python3 scripts/openobserve_ingest.py \
  docs/evidence/logs/maker-paper-20260710T0705Z.log
```

上传器按 500 条批量写入，失败自动重试，并在
`var/standx/openobserve-uploaded.json` 保存逐行 checkpoint。每条记录会补充：

- `schema_version=maker_event_v1`
- `run_id`
- `event_id`（文件哈希与行号生成）
- `source_file`
- `_timestamp`
- `git_sha`、`config_hash`（包装器可获取时）

实时模式使用稳定的 `run_id + 行号` 生成 `event_id`，文件增长不会改变已经上传事件的
身份，正常反复扫描不会重复发送。若 HTTP 已写入但响应丢失，at-least-once 重试仍可能
产生相同 `event_id` 的重复行，分析查询应继续使用 `count(DISTINCT event_id)`。手工导入
已结束的不可变日志仍保持原有的文件哈希 checkpoint 语义。

checkpoint 记录文件身份（inode + 大小）：日志轮转（inode 变化）或截断（大小变小）时
自动从头重扫，避免行号错位；重复行仍靠 `event_id` 去重。状态文件损坏（非法 JSON）时
自动重置而非启动即失败，`--follow` 不再需要人工删除。checkpoint 条目数量有上界，超出时
淘汰最久未更新的条目，`openobserve-uploaded.json` 不会无界增长。

JWT、private key、token、password、authorization、webhook 等字段在上传前会递归
替换为 `[REDACTED]`。原始本地文件不被修改。

## 4. 第一组分析查询

在 OpenObserve Logs 中选择 `standx_maker` stream：

```sql
SELECT
  run_id,
  symbol,
  max(fills_total) AS fills,
  min(pnl) AS min_pnl,
  max(pnl) AS max_pnl,
  avg(uptime_pct) AS avg_uptime,
  sum(CASE WHEN halted THEN 1 ELSE 0 END) AS halted_cycles
FROM "standx_maker"
WHERE action = 'cycle_summary'
GROUP BY run_id, symbol
ORDER BY run_id DESC;
```

撤单原因分布：

```sql
SELECT run_id, reason, count(DISTINCT event_id) AS events
FROM "standx_maker"
WHERE action = 'cancel'
GROUP BY run_id, reason
ORDER BY events DESC;
```

分析时以 `event_id` 去重；外部预置仓位不属于 maker-correlated fill ledger，不能把
对应会话的 maker PnL 当成账户真实盈亏。

同一查询也可以从终端只读执行：

```bash
python3 scripts/openobserve_query.py --hours 24
```

订单请求超时可以按 request 直接关联冻结告警、恢复目标和停机延迟记录：

```sql
SELECT _timestamp, action, event, request_id, request_kind, timeout_phase,
       age_ms, timeout_ms, recovery_target, outcome, message
FROM "standx_maker"
WHERE run_id = '<run_id>'
  AND ((action = 'risk_notification' AND kind = 'order_request_timeout')
       OR action = 'order_latency')
ORDER BY _timestamp ASC;
```

`timeout_phase=acknowledgement` 应选择 `recovery_target=order_response`；
`timeout_phase=account_order` 应选择 `recovery_target=account_stream`。两者都必须先出现冻结/清理，
通过空簿与仓位对账后才能恢复 placement。

### 4.1 Maker WebSocket 事故排查手册

先用 `run_id` 重建事件顺序，再判断是行情缓存新鲜度、账户流还是订单回报流问题；不要只
根据一条 `REST fallback` 或 `websocket live` 日志判断 TCP 连接是否断开。`REST fallback`
表示公共行情快照未满足本地新鲜度/时钟校验，也可能是某一侧的 mark 或 book 暂时没有更新。

如果目标是远端 OpenObserve，`deploy/openobserve/.env` 中常常仍指向本机的
`127.0.0.1:5080`。导入凭证后必须在同一条查询命令中显式覆盖 URL，避免误查本地历史数据：

```bash
set -a
source deploy/openobserve/.env
set +a

OPENOBSERVE_URL=http://192.168.193.65:5080 \
python3 scripts/openobserve_query.py --hours 2 --size 500 --sql '
SELECT _timestamp, action, event, cycle, order_id, trade_id, message
FROM "standx_maker"
WHERE run_id = '\''<run_id>'\''
  AND action IN ('\''lifecycle'\'', '\''order_response_reconnect'\'', '\''fill'\'', '\''position_reconciliation'\'')
ORDER BY _timestamp ASC'
```

按以下顺序判断：

1. 统计同一 `run_id` 的 `order_response_reconnect` 和 `risk_notification`。单轮耗尽只应出现
   `reconnect_retrying`，maker 保持冻结并在重新验证空簿后退避重试；不应再因累计断线次数出现
   `lifecycle/stopped`。
2. 在每次相关失败前后对齐 `fill`、仓位对账和订单回报。账户成交可能合法地先于
   `PlaceAccepted` 或 `CancelResolved` 到达；这不是订单回报断开或未知订单的证据。
3. 若成交/账户事件触发 cycle invalidation，随后清理投影状态，再把延迟到达的 ACK 报为
   `unknown request_id`，应检查清理是否删掉了未确认的 request registry。
4. 只有空簿清理失败、持仓/成交无法对账、硬鉴权失败或重连被显式禁用时才维持
   fail-safe 停机；可重试的协议/连接故障保持 Frozen。

2026-07-14 的 XAG-USD 事故属于第 3 类：成交先抵达账户流，冻结清理把订单投影和未确认
请求一起清空，随后正常的订单回报无法关联；当时每次安全重连都成功，但第三次后预算耗尽而停机。
当前运行时已移除该累计停机条件。修复原则是：账户流或仓位对账冻结时清除可执行挂单和 quote slot，同时保留尚未确认的
current-run request，允许迟到 ACK 完成关联；账户流重连切换 projection generation 时也必须
保留这份 ACK registry，不能由通用 reset 再次清空。已提交撤单的订单还要保留有界的 retired
ID：若账户流随后重放该订单的 open 状态，应把它作为 stale maker order 再次撤销，而不是误报
外部未知订单。订单回报流已故障时的清理、换 session 和真正未知 request 仍必须清空并 fail
closed，不能以此放宽实时下单安全边界。

修复后，至少验证：

- 单元测试覆盖“冻结后迟到的 `PlaceAccepted`/`CancelResolved` 可关联”；
- 单元测试覆盖“撤单 ACK 后迟到的 open order 仍被识别为本 run 的 stale order”；
- 单元测试覆盖“账户流重连 reset 后迟到 ACK 仍可关联”；
- 一个 account/fill 在 ACK 之前到达的运行不会产生 `order-response correlation failed closed`；
- 真正断开 order-response stream 时仍出现冻结、清理和单轮有限重连；单轮耗尽后保持
  Frozen、重新验证空簿并退避进入下一轮；
- 看板中 `order_response_reconnect` 与 `lifecycle/stopped` 按 `event_id` 去重后不再重复出现
  无对应 stream 故障的相关失败。

## 5. 创建或刷新 maker Dashboard

创建原生 `StandX Maker Overview` Dashboard；如果同名 Dashboard 已存在，则更新它：

```bash
set -a
source deploy/openobserve/.env
set +a
python3 scripts/openobserve_dashboard.py
```

Dashboard 默认选择最新 maker run，包含 `Overview`、`Runs & Events` 和
`Performance & Latency` 三个页签。
PnL 面板展示 maker session 自身的 PnL；reduce-only exit 演练所用的外部预置库存，
通过 Inventory 和 Events 面板单独判断。计数类面板统一使用 `count(DISTINCT event_id)`，
重复上传不会虚增指标。

`Overview` 另含权益/uPnL/可用保证金趋势（live 模式的 `account` 字段；paper 为 null）
与数据新鲜度（`max(_timestamp)`）；`Runs & Events` 含拒单/错误信号与流健康/重连面板，
便于在告警触发前观察前兆。

`Performance & Latency` 展示 passive/exit 现金流、capture、PnL 组成、1s/5s/30s markout、
时间加权双边 uptime、合格深度和库存持有时间，以及 place/cancel 的 write/ack/effective
p50/p95/p99、拒绝率、超时率和 cancel 后成交。逐请求表可按 `recovery`、symbol、side、level
和 market source 过滤；跨 run 表保留 `run_id + config_hash + symbol`，用于冻结配置间比较。
阶段 1 的字段映射与查询 payload 可离线回归：

```bash
python3 scripts/test_openobserve_ingest.py
python3 scripts/test_openobserve_dashboard.py
```

## 6. Push 告警（deadman + critical risk）

Dashboard 是纯拉取式的，需要人盯着看。`scripts/openobserve_alerts.py` 在 OpenObserve
上 upsert **两条**定时告警，覆盖两类互不重叠的失败：

| 告警 | 触发条件 | 覆盖的情况 |
|---|---|---|
| `standx_maker_deadman` | `standx_maker` stream 在最近约 3 分钟内没有任何 `action='cycle_summary'` | **进程死了**：SIGKILL / OOM / panic / 宿主宕机——cleanup 没跑、"stopped" 通知没发、挂单留在盘口 |
| `standx_maker_critical_risk` | stream 里出现任何 `severity='critical'` 的行 | **进程还活着但出事了**：stop-loss、账户硬熔断、accounting invariant、cleanup 残余订单、残余仓位交接/UNKNOWN |

第二条是 2026-07-28 补的。在它之前，上述 critical 事件的**唯一即时通道是 maker 进程
自己发的 webhook POST，而那个 POST 不重试**——端点慢或挂掉就没人知道；deadman 也不会响，
因为进程还在正常发 cycle_summary。无人值守长跑（见
[27 号采集手册](27-maker-baseline-pnl-collection-runbook.md)）必须两条都配。

```bash
set -a
source deploy/openobserve/.env
set +a
export OPENOBSERVE_ALERT_WEBHOOK="https://hooks.slack.com/services/XXX/YYY/ZZZ"
# 可选，默认 3 分钟无 cycle_summary 即触发
export OPENOBSERVE_ALERT_MINUTES=3
# 可选，默认 5 分钟；critical 事件在 fail-safe 停机时成串出现，静默窗把一次事故收敛成一条通知
export OPENOBSERVE_CRITICAL_SILENCE_MINUTES=5
python3 scripts/openobserve_alerts.py
```

每条告警各自 upsert 一个 template、一个 destination 和告警本体（同名则更新；
OpenObserve 把 template 绑在 destination 上，两条文案不同所以不能共用）。鉴权和 env
变量与 dashboard 脚本一致。

**critical 告警的已知限制**：文案只说"窗口内出现了 critical 行"，不说是哪一条——
本部署上只验证过 `{stream_name}` / `{alert_name}` / `{org_name}` 三个变量会被替换。
具体行去 dashboard 的 "Rejections & Error Signals" 面板看。告警条件用的是
`severity='critical'` 单条件（不是逐 action 列举），因此以后新增的 critical 事件自动被
覆盖，不需要再改这个脚本。

进程内 webhook（`--alert-webhook`）负责活着时的风险/停止通知；这两条 OpenObserve 告警
是与进程解耦的第二通道。实盘三者都应配置。

## 7. 安全边界

- OpenObserve 仅监听 `127.0.0.1:5080`，不要直接暴露公网。
- `deploy/openobserve/.env` 使用独立密码，不复用交易所凭证。
- stdout 才进入分析 stream；stderr 只本地保存，避免诊断噪声污染事件表。撤单重试失败、
  reconciliation 快照失败等前兆信号在 JSON 模式下以结构化 stdout 事件（`severity=warning`）
  输出，随 stdout 一并上传；非 JSON 模式仍打印人类可读的 stderr 行。
- verbose WebSocket 日志不得包含认证载荷；SDK 只记录认证动作和 stream 数量。
- 先落盘、后上传是有意设计：日志后端绝不能给 maker 施加背压。
