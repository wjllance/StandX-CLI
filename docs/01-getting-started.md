# 01 - 快速开始

本文档帮助你快速上手 StandX CLI，从安装到运行第一个命令。

---

## 1.1 安装

### 方式一：Homebrew (推荐 macOS/Linux)

```bash
# 添加 tap
brew tap wjllance/standx-cli

# 安装
brew install standx-cli

# 验证安装
standx --version
```

### 升级

```bash
standx update --check   # 只看版本差异，不改任何东西
standx update           # 下载、校验 sha256、原子替换当前二进制
standx --yes update     # 跳过确认（脚本用；等价于 STANDX_AUTO_CONFIRM=true）
```

Homebrew 装的用 `brew upgrade standx-cli`——`standx update` 会检测到并拒绝自替换，
避免 formula 与实际二进制不一致。安装目录不可写时会直接报错并给出说明，**不会**自动
提权。

### 方式二：直接下载二进制

```bash
# 下载对应平台的二进制文件
# macOS ARM64
curl -L -o standx.tar.gz https://github.com/wjllance/standx-cli/releases/latest/download/standx-macos-aarch64.tar.gz

# Linux x86_64
curl -L -o standx.tar.gz https://github.com/wjllance/standx-cli/releases/latest/download/standx-linux-x86_64.tar.gz

# 解压
tar -xzf standx.tar.gz
chmod +x standx
sudo mv standx /usr/local/bin/

# 验证
standx --version
```

### 方式三：从源码构建

```bash
# 克隆仓库
git clone https://github.com/wjllance/standx-cli.git
cd standx-cli

# 构建 Release 版本
cargo build --release

# 二进制位置
./target/release/standx --version
```

---

## 1.2 第一个命令

### 查看版本

```bash
standx --version
```

**预期输出：**
```
standx 0.3.1
```

### 查看帮助

```bash
standx --help
```

**预期输出：**
```
OpenClaw-first AI Agent trading toolkit

Usage: standx [OPTIONS] <COMMAND>

Commands:
  config    Configuration management
  auth      Authentication management
  market    Market data (public)
  account   Account information (authenticated)
  order     Order management (authenticated)
  trade     Trade history (authenticated)
  leverage  Leverage management (authenticated)
  margin    Margin management (authenticated)
  stream    Real-time data stream
  help      Print this message or the help of the given subcommand(s)

Options:
  -c, --config <CONFIG>    Configuration file path
      --endpoint <BASE_URL>
                           StandX REST/WebSocket base URL
  -o, --output <OUTPUT>    Output format [default: table] [possible values: table, json, csv, quiet]
  -v, --verbose            Verbose output
  -q, --quiet              Quiet mode
      --openclaw           OpenClaw mode - optimized for AI Agent execution
      --dry-run            Dry run - show what would be executed without executing
      --yes                Auto-confirm dangerous operations (skip prompts)
  -h, --help               Print help
  -V, --version            Print version
```

---

## 1.3 配置初始化

### 初始化配置

```bash
standx config init
```

**预期输出：**
```
✅ Configuration initialized at /home/user/.config/standx/config.toml
```

### 查看配置

```bash
standx config show
```

**预期输出：**
```
Configuration:
  base_url: https://perps.standx.com
  output_format: table
  default_symbol: BTC-USD
```

### 修改配置

```bash
# 设置默认交易对
standx config set default_symbol ETH-USD

# 持久化 StandX API endpoint（写入前会校验并规范化）
standx config set base_url https://canary-perps.standx.org

# 验证
standx config get default_symbol
# 输出: ETH-USD
```

`--config` 可以指向配置文件或配置目录。Endpoint 优先级为：
`--endpoint` > `STANDX_BASE_URL` > 配置文件中的 `base_url` > 生产默认值。
根级 HTTPS base 会自动推导 `/ws-stream/v1` 和 `/ws-api/v1`；无效值会直接
报错，不会回退到生产环境。仅本地 loopback 测试允许明文 HTTP。

---

## 1.4 无需认证的命令

以下命令不需要认证即可使用：

### 查看交易对列表

```bash
standx market symbols
```

**预期输出：**
```
┌─────────┬───────────┬───────────┬─────────────┬─────────────┐
│ Symbol  │ Base      │ Quote     │ Min Order   │ Max Leverage│
├─────────┼───────────┼───────────┼─────────────┼─────────────┤
│ BTC-USD │ BTC       │ DUSD      │ 0.0001      │ 40          │
│ ETH-USD │ ETH       │ DUSD      │ 0.001       │ 40          │
│ SOL-USD │ SOL       │ DUSD      │ 0.01        │ 40          │
│ XRP-USD │ XRP       │ DUSD      │ 1           │ 40          │
└─────────┴───────────┴───────────┴─────────────┴─────────────┘
```

### 查看行情

```bash
standx market ticker BTC-USD
```

**预期输出：**
```
┌─────────┬────────────┬────────────┬────────────┬─────────────┐
│ Symbol  │ Mark Price │ Index Price│ Last Price │ Funding Rate│
├─────────┼────────────┼────────────┼────────────┼─────────────┤
│ BTC-USD │ 63127.37   │ 63126.67   │ 63115.80   │ 0.00001250  │
└─────────┴────────────┴────────────┴────────────┴─────────────┘
```

---

## 1.5 测试检查清单

- [ ] 安装成功，`standx --version` 显示版本号
- [ ] `standx --help` 显示所有子命令
- [ ] `standx config init` 成功创建配置文件
- [ ] `standx config show` 显示默认配置
- [ ] `standx config set/get` 可以修改和读取配置
- [ ] `standx market symbols` 返回交易对列表
- [ ] `standx market ticker BTC-USD` 返回行情数据

---

## 下一步

- 需要认证功能？阅读 [02-authentication.md](02-authentication.md)
- 查看市场数据详情？阅读 [03-market-data.md](03-market-data.md)
- 了解输出格式？阅读 [09-output-formats.md](09-output-formats.md)

---

*文档版本: 0.3.1*  
*最后更新: 2026-02-26*
