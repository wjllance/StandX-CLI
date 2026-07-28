# StandX CLI 版本更新检查清单

本文档记录了发布新版本时需要更新的所有文件和注意事项。

## 📋 版本更新检查清单

### 核心版本文件 (必须更新)

| 文件 | 位置 | 更新内容 | 示例 |
|------|------|----------|------|
| `crates/standx-cli/Cargo.toml` | workspace 成员 | `version = "x.y.z"` | `version = "1.1.0"` |
| `version.json` | 项目根目录 | `{"version": "x.y.z"}` | `{"version": "1.1.0"}` |

> **注意（2026-07-28 补）**：根 `Cargo.toml` 自 workspace 拆分后不再带 version，
> 二进制版本来自 `crates/standx-cli/Cargo.toml`。而 **release 流水线的版本号取自 git
> tag，不读这两个文件**——v1.0.0 就是在两者仍为 `0.8.0` 的情况下发布的，导致发出去的
> 二进制自报 `standx 0.8.0`。所以打 tag 前必须先对齐这两个文件，`standx --version`
> 才不会说谎。

### 文档文件 (必须更新)

| 文件 | 位置 | 更新内容 |
|------|------|----------|
| `CHANGELOG.md` | 项目根目录 | 添加新版本 section，记录所有变更 |
| `README.md` | 项目根目录 | 如有新功能，更新命令参考部分 |
| `RELEASE_NOTES_vx.y.z.md` | 项目根目录 | 创建新的发布说明文件 |

### Skill 文件 (必须更新)

| 文件 | 位置 | 更新内容 |
|------|------|----------|
| `SKILL.md` | `openclaw/` 或 `skills/standx-cli/openclaw/` | 更新版本号、下载 URL、添加新功能文档 |

### 下载 URL 更新 (必须更新)

在 `SKILL.md` 中更新以下 URL：

```yaml
# Linux x86_64
https://github.com/wjllance/standx-cli/releases/download/vx.y.z/standx-vx.y.z-x86_64-unknown-linux-gnu.tar.gz

# macOS Apple Silicon  
https://github.com/wjllance/standx-cli/releases/download/vx.y.z/standx-vx.y.z-aarch64-apple-darwin.tar.gz
```

## 🔄 版本更新流程

### 1. 准备阶段

- [ ] 确定新版本号 (遵循 Semantic Versioning)
- [ ] 检查所有 PR 是否已合并
- [ ] 运行完整测试: `cargo test`
- [ ] 检查代码格式: `cargo fmt -- --check`
- [ ] 运行静态检查: `cargo clippy -- -D warnings`

### 2. 文件更新阶段

- [ ] 更新 `Cargo.toml` 版本号
- [ ] 更新 `version.json` 版本号
- [ ] 更新 `CHANGELOG.md`
- [ ] 更新 `README.md` (如有新功能)
- [ ] 创建 `RELEASE_NOTES_vx.y.z.md`
- [ ] 更新 `SKILL.md` 版本号和下载 URL

### 3. 验证阶段

- [ ] 构建 Release: `cargo build --release`
- [ ] 验证版本: `./target/release/standx --version`
- [ ] 检查所有文件已提交
- [ ] 创建 PR 进行代码审查

### 4. 发布阶段

- [ ] 合并 PR 到 main 分支
- [ ] 创建 GitHub Release
- [ ] 上传二进制文件
- [ ] 更新 Pre-release 状态 (如适用)
- [ ] 通知用户

## ⚠️ 常见错误

### 错误 1: 忘记更新 Cargo.toml
```
# 错误
version = "0.5.0"  # 旧版本

# 正确
version = "0.6.0"  # 新版本
```

### 错误 2: 下载 URL 版本不匹配
```
# 错误
https://github.com/wjllance/standx-cli/releases/download/v0.5.0/...

# 正确
https://github.com/wjllance/standx-cli/releases/download/v0.6.0/...
```

### 错误 3: CHANGELOG 格式错误
```markdown
# 错误 - 缺少日期
## [0.6.0]

# 正确
## [0.6.0] - 2026-03-01
```

## 📝 版本号规则

### Semantic Versioning

- **MAJOR**: 破坏性变更 (如 API 不兼容)
- **MINOR**: 新功能 (向后兼容)
- **PATCH**: Bug 修复 (向后兼容)

### 示例

| 版本 | 说明 |
|------|------|
| v0.5.0 → v0.6.0 | 新增 Dashboard 功能 (MINOR) |
| v0.6.0 → v0.6.1 | 修复 Dashboard bug (PATCH) |
| v0.6.0 → v1.0.0 | 破坏性 API 变更 (MAJOR) |

## 🔍 验证命令

```bash
# 检查所有版本号
grep -r "version" --include="*.toml" --include="*.json" | grep -E "0\.[0-9]+\.[0-9]+"

# 验证 Cargo.toml
grep "^version" Cargo.toml

# 验证 version.json
cat version.json

# 验证构建版本
cargo build --release
./target/release/standx --version
```

## 📚 相关文档

- [CHANGELOG.md](../CHANGELOG.md)
- [Semantic Versioning](https://semver.org/)

---

*最后更新: 2026-07-28*  
*版本: v1.1.0*
