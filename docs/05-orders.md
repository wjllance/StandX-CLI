# 05 - 订单管理

本文档介绍 StandX CLI 的订单管理功能，包括创建、取消和查询订单。

---

## 前置条件

需要完成认证并配置私钥，参考 [02-authentication.md](02-authentication.md)。

⚠️ **注意**: 只有读取权限的 Token 无法下单，需要配置 Ed25519 私钥。

---

## 5.1 创建订单

### 命令

```bash
standx order create <SYMBOL> <SIDE> <TYPE> \
  --qty <QUANTITY> \
  [--price <PRICE>] \
  [--tif <TIF>] \
  [--reduce-only] \
  [--sl-price <PRICE>] \
  [--tp-price <PRICE>] \
  [--transport <http|ws>] \
  [--timeout-secs <SECONDS>]
```

### 参数

| 参数 | 说明 | 必需 | 示例 |
|------|------|------|------|
| SYMBOL | 交易对 | 是 | BTC-USD |
| SIDE | 买卖方向 | 是 | buy / sell |
| TYPE | 订单类型 | 是 | limit / market |
| --qty | 订单数量 | 是 | 0.01 |
| --price | 订单价格（限价单必需） | 条件 | 60000 |
| --tif | Time in Force | 否 | GTC / IOC / FOK |
| --reduce-only | 仅减仓 | 否 | - |
| --sl-price | 止损价格 | 否 | 55000 |
| --tp-price | 止盈价格 | 否 | 70000 |
| --transport | 下单传输方式，默认 `http` | 否 | ws |
| --timeout-secs | 等待匹配 WS 回报的秒数，范围 1–30，默认 10 | 否 | 10 |

### Time in Force 说明

| 值 | 说明 |
|----|------|
| GTC | Good Till Cancel - 一直有效直到取消（默认） |
| IOC | Immediate or Cancel - 立即成交或取消 |
| FOK | Fill or Kill - 全部成交或全部取消 |

### 限价单示例

```bash
standx order create BTC-USD buy limit \
  --qty 0.01 \
  --price 60000 \
  --tif GTC
```

**预期输出（成功）：**
```
✅ Order created successfully!
   Order ID: 123456
   Symbol: BTC-USD
   Side: Buy
   Type: Limit
   Quantity: 0.01
   Price: 60000
```

### 市价单示例

```bash
standx order create BTC-USD sell market \
  --qty 0.01
```

**预期输出（成功）：**
```
✅ Order created successfully!
   Order ID: 123457
   Symbol: BTC-USD
   Side: Sell
   Type: Market
   Quantity: 0.01
```

### 带止盈止损的订单

```bash
standx order create BTC-USD buy limit \
  --qty 0.01 \
  --price 60000 \
  --sl-price 55000 \
  --tp-price 70000
```

### 仅减仓订单

```bash
standx order create BTC-USD sell limit \
  --qty 0.01 \
  --price 65000 \
  --reduce-only
```

### WebSocket 下单并查看回报

```bash
standx --output json --verbose order create BTC-USD buy limit \
  --qty 0.01 \
  --price 60000 \
  --transport ws
```

要让同一次调用中的 REST、账户/行情 WS 和订单 WS 全部进入自定义环境，可指定全局
endpoint：

```bash
standx --endpoint https://perps.example.com \
  --output json --verbose \
  order create BTC-USD buy limit \
  --qty 0.0001 --price 60000 --transport ws
```

自定义 endpoint 仍使用当前 JWT 和签名私钥；凭证不适用时会返回认证错误，并且
不会降级到生产环境。

标准输出是稳定的结构化结果：

```json
{
  "transport": "ws",
  "operation": "create",
  "symbol": "BTC-USD",
  "request_id": "…",
  "response_code": 0,
  "response_message": "accepted",
  "accepted": true
}
```

`--verbose` 会把认证完成后的原始入站 WS response 写到 stderr；不会记录 JWT、签名、
认证载荷或出站订单。CLI 收到首个匹配 `request_id` 的回报即结束，不额外等待 REST
可见性。若等待超时或连接中断，订单提交状态未知；请先查询账户订单，避免直接重试造成
重复下单。

---

## 5.2 取消订单

### 命令

```bash
standx order cancel <SYMBOL> --order-id <ID> \
  [--transport <http|ws>] \
  [--timeout-secs <SECONDS>]
```

### 参数

| 参数 | 说明 | 必需 | 示例 |
|------|------|------|------|
| SYMBOL | 交易对 | 是 | BTC-USD |
| --order-id | 订单ID | 是 | 123456 |
| --transport | 撤单传输方式，默认 `http` | 否 | ws |
| --timeout-secs | 等待匹配 WS 回报的秒数，范围 1–30，默认 10 | 否 | 10 |

### 示例

```bash
standx order cancel BTC-USD --order-id 123456
```

WebSocket 撤单：

```bash
standx --output json order cancel BTC-USD \
  --order-id 123456 \
  --transport ws
```

**预期输出（成功）：**
```
✅ Order 123456 cancelled successfully
```

**预期输出（失败）：**
```
⚠️  Failed to cancel order 123456
   Error: Order not found or already filled/cancelled
```

---

## 5.3 取消所有订单

### 命令

```bash
standx order cancel-all <SYMBOL>
```

### 参数

| 参数 | 说明 | 必需 | 示例 |
|------|------|------|------|
| SYMBOL | 交易对 | 是 | BTC-USD |

### 示例

```bash
standx order cancel-all BTC-USD
```

`cancel-all` 继续使用 REST，不接受 `--transport ws`。

**预期输出（成功）：**
```
✅ All orders for BTC-USD cancelled successfully
```

---

## 5.4 查询订单

参考 [04-account.md](04-account.md) 的以下命令：

- `standx account orders` - 当前未成交订单
- `standx account history` - 历史订单

---

## 5.5 Dry Run 模式 ⭐

在实际下单前，可以使用 Dry Run 模式预览操作：

```bash
standx --dry-run order create BTC-USD buy limit \
  --qty 0.01 \
  --price 60000
```

**预期输出：**
```
🔍 DRY RUN - No actual execution
Command: order create BTC-USD buy limit
Parameters:
  Symbol: BTC-USD
  Side: Buy
  Type: Limit
  Quantity: 0.01
  Price: 60000
⚠️  This is a financial operation - use with caution in production
```

---

## 5.6 完整交易流程示例

### 场景：买入 BTC，设置止盈止损

```bash
# 1. 查看当前行情
standx market ticker BTC-USD

# 2. 查看账户余额
standx account balances

# 3. 创建限价买单（Dry Run 预览）
standx --dry-run order create BTC-USD buy limit \
  --qty 0.01 \
  --price 60000 \
  --sl-price 55000 \
  --tp-price 70000

# 4. 确认无误后执行
standx order create BTC-USD buy limit \
  --qty 0.01 \
  --price 60000 \
  --sl-price 55000 \
  --tp-price 70000

# 5. 查看订单状态
standx account orders --symbol BTC-USD

# 6. 如需取消
standx order cancel BTC-USD --order-id 123456
```

---

## 5.7 测试检查清单

### 基础功能测试
- [ ] 创建限价买单成功
- [ ] 创建限价卖单成功
- [ ] 创建市价单成功
- [ ] 取消指定订单成功
- [ ] 取消所有订单成功

### 参数测试
- [ ] 不同 TIF 类型（GTC, IOC, FOK）
- [ ] 设置止盈止损价格
- [ ] 仅减仓模式（--reduce-only）

### 边界情况测试
- [ ] 余额不足时下单失败
- [ ] 价格超出范围时失败
- [ ] 取消已成交订单失败
- [ ] 取消不存在的订单失败

### 特殊功能测试
- [ ] Dry Run 模式正常显示
- [ ] 不同输出格式（table, json）

---

## 下一步

- 查看成交历史？阅读 [06-trading.md](06-trading.md)
- 调整杠杆？阅读 [07-leverage-margin.md](07-leverage-margin.md)
- 实时数据流？阅读 [08-streaming.md](08-streaming.md)

---

*文档版本: 0.3.1*  
*最后更新: 2026-02-26*
