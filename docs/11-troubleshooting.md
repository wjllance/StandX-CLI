# 11 - 故障排除

本文档汇总 StandX CLI 的常见问题和解决方案。

---

## 11.1 安装问题

### Q: 构建失败，提示 Rust 版本过低？

**A:** 更新 Rust 工具链：

```bash
# 更新 Rust
rustup update

# 验证版本
rustc --version  # 需要 1.85+
```

### Q: 构建失败，提示缺少依赖？

**A:** 安装系统依赖：

```bash
# Ubuntu/Debian
sudo apt-get update
sudo apt-get install -y pkg-config libssl-dev

# macOS
brew install openssl

# 然后重新构建
cargo build --release
```

### Q: Homebrew 安装失败？

**A:** 检查 tap 是否正确添加：

```bash
# 重新添加 tap
brew tap wjllance/standx-cli

# 更新
brew update

# 安装
brew install standx-cli
```

---

## 11.2 认证问题

### Q: 提示 "Not authenticated"？

**A:** 执行登录：

```bash
# 交互式登录
standx auth login --interactive

# 或命令行登录
standx auth login --token "your_jwt_token"
```

### Q: 提示 "Token expired"？

**A:** 重新获取 Token 并登录：

```bash
# 1. 访问 https://standx.com/user/session 获取新 Token
# 2. 重新登录
standx auth login --token "new_token"
```

### Q: 交易操作提示需要私钥？

**A:** 重新登录并添加私钥：

```bash
standx auth login --interactive
# 输入 JWT Token
# 输入 Ed25519 Private Key
```

### Q: 用户频道返回 "invalid token"？

**A:** 这是已知问题 [#3](https://github.com/wjllance/standx-cli/issues/3)：

1. 确认 Token 未过期：`standx auth status`
2. 尝试重新登录
3. 临时使用公共频道替代

---

## 11.3 命令执行问题

### Q: 命令执行超时？

**A:** 检查网络连接：

```bash
# 测试 API 连通性
curl https://perps.standx.com/api/query_symbol_info

# 检查代理设置
env | grep -i proxy
```

### Q: 返回 "Symbol not found"？

**A:** 检查交易对名称：

```bash
# 查看所有可用交易对
standx market symbols

# 注意大小写和格式（如 BTC-USD）
```

### Q: K-line 时间格式不支持？

**A:** 检查时间格式：

```bash
# 支持的格式：
# 相对时间
standx market kline BTC-USD -r 60 --from 1d

# ISO 日期
standx market kline BTC-USD -r 60 --from 2024-01-01

# Unix 时间戳
standx market kline BTC-USD -r 60 --from 1704067200
```

### Q: 下单失败，提示余额不足？

**A:** 检查账户余额：

```bash
# 查看余额
standx account balances

# 查看当前持仓（可能占用保证金）
standx account positions
```

---

## 11.4 输出格式问题

### Q: JSON 输出格式错误？

**A:** 使用 jq 验证：

```bash
# 检查 JSON 有效性
standx -o json market ticker BTC-USD | jq .

# 如果 jq 报错，可能是 API 返回了非 JSON 数据
```

### Q: CSV 输出中文乱码？

**A:** 设置正确的编码：

```bash
# Linux/macOS
export LANG=en_US.UTF-8

# Windows PowerShell
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
```

---

## 11.5 WebSocket 问题

### Q: 流数据连接断开？

**A:** CLI 会自动重连，无需手动操作。如需调试：

```bash
# 启用调试模式
standx -v stream price BTC-USD
```

### Q: 用户频道无法连接？

**A:** 参考 [08-streaming.md](08-streaming.md) 和 Issue [#3](https://github.com/wjllance/standx-cli/issues/3)。

---

## 11.6 CI/构建问题

### Q: CI 格式检查失败？

**A:** 本地运行格式化：

```bash
# 格式化代码
cargo fmt

# 检查格式
cargo fmt -- --check

# 提交修复
git add -A && git commit -m "style: cargo fmt"
```

### Q: Clippy 警告？

**A:** 修复警告：

```bash
# 查看警告
cargo clippy

# 自动修复（部分）
cargo clippy --fix
```

---

## 11.7 获取帮助

### 查看命令帮助

```bash
# 总帮助
standx --help

# 子命令帮助
standx market --help
standx order create --help
```

### 查看文档

- 快速开始：[01-getting-started.md](01-getting-started.md)
- 认证管理：[02-authentication.md](02-authentication.md)
- 市场数据：[03-market-data.md](03-market-data.md)

### 提交 Issue

遇到问题可以提交 Issue：
https://github.com/wjllance/standx-cli/issues

---

## 11.8 快速诊断命令

```bash
# 检查版本
standx --version

# 检查认证状态
standx auth status

# 测试公共 API
standx market symbols

# 测试认证 API（需要登录）
standx account balances

# 检查网络
ping perps.standx.com
```

---

## 11.9 常见错误代码

| 错误 | 原因 | 解决方案 |
|------|------|----------|
| 401 Unauthorized | Token 无效或过期 | 重新登录 |
| 403 Forbidden | 权限不足 | 检查私钥配置 |
| 404 Not Found | 资源不存在 | 检查参数 |
| 429 Too Many Requests | 请求过于频繁 | 稍后重试 |
| 500 Internal Server Error | 服务器错误 | 联系支持 |

---

*文档版本: 0.3.1*  
*最后更新: 2026-02-26*
