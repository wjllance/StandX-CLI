# StandX CLI 测试指南

本文档是仓库自动化测试的唯一说明入口。历史测试计划和一次性测试报告不作为当前覆盖率或发布状态的依据。

## 测试边界

- `standx-maker`：纯策略、风险、账本、状态转换和恢复决策。测试必须确定性运行，不依赖网络、时钟、环境变量或文件系统。
- `standx-sdk`：协议模型、认证、HTTP/WebSocket 客户端和传输健康。部分测试会在本机绑定 loopback 端口。
- `standx-cli`：参数解析、配置、I/O 编排、输出和 live gate。部分恢复和命令测试使用 Mockito 或本地 TCP/WebSocket 服务。
- `crates/standx-cli/tests/cli_contract_tests.rs`：CLI 进程级契约测试。所有 I/O 都通过 `--endpoint` 指向 loopback test server，不访问公共 StandX API。

自动化测试不得放置 live 订单、执行 inventory exit、断开生产流或平仓。生产 canary 必须使用对应 runbook 并获得针对该次操作的明确授权。

## 标准验证

Maker、安全或 live 路径变更在交付前运行：

```bash
HOME=/tmp/standx-test-home CARGO_HOME=~/.cargo cargo test --workspace --offline
cargo clippy --workspace --all-targets --offline -- -D warnings
cargo fmt --all -- --check
python3 -m py_compile scripts/openobserve_dashboard.py
```

受限沙箱可能禁止 Mockito 或本地 TCP/WebSocket listener，表现为 `Operation not permitted`。这类环境失败不能替代完整验证；应在允许 loopback listener 的环境重跑同一命令。

## 测试目标

列出全部测试：

```bash
cargo test --workspace --offline -- --list
```

运行各 crate 的 library 测试：

```bash
cargo test -p standx-maker --offline --lib
cargo test -p standx-sdk --offline --lib
cargo test -p standx-cli --offline --lib
```

运行 CLI 进程级测试：

```bash
cargo test -p standx-cli --offline --test cli_contract_tests
```

模型、错误和解析单元测试跟随 owner module，分别由 `standx-sdk` 和 `standx-cli` 的 library test target 编译。CLI contract target 会绑定 loopback 端口，但不需要凭证，也没有生产 API fallback。

## 快速本地反馈

只验证纯 maker 决策：

```bash
cargo test -p standx-maker --offline --lib
```

只做编译检查、不执行可能绑定端口的测试：

```bash
cargo test --workspace --offline --no-run
```

只运行某个测试：

```bash
cargo test -p standx-maker test_name --offline
cargo test -p standx-cli module::test_name --offline
```

## 测试设计要求

- 策略、风险、账本和状态转换优先写在 `standx-maker`，使用 typed inputs/outputs 和纯测试。
- SDK wire contract 可以使用 loopback server，但不得访问生产 API。
- CLI 命令测试必须通过 `--endpoint` 指向测试服务，断言明确的退出码和输出契约。
- 不得以输出包含 `Error` 作为成功条件。
- 凭证依赖测试必须显式隔离，不能进入默认离线测试路径。
- WS/REST fill 顺序、重复成交、部分成交、generation invalidation、freeze/cleanup/recovery、残留订单和 reconciliation timeout 等安全语义必须保留回归测试。
- 新增安全测试要做 mutation 验证：故意破坏实现并确认测试转红，再恢复实现。

## Maker 变更的额外要求

涉及 maker、安全或 live 路径时：

1. 先运行相关的确定性测试。
2. 再运行完整标准验证。
3. 由未编写该改动的 reviewer 做一次以高严重度缺陷为目标的对抗审查。
4. 只接受可复现的问题；先写出具体路径并用测试复现，再修复。
5. 新机制默认关闭，并用测试固定关闭时与旧行为等价。

## 手动检查

不需要凭证的 release smoke：

```bash
cargo build --release
./target/release/standx --version
./target/release/standx --help
./target/release/standx config show
```

公共行情、认证、账户、下单、WebSocket 和 maker live 检查不属于默认离线测试。使用 `docs/` 下对应说明或 runbook，并遵守显式授权边界。
