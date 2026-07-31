# StandX CLI 发布流程

发布流程只保留一个版本真源，并把版本、标签和构建产物的一致性作为硬门禁。

## 版本真源

- `crates/standx-cli/Cargo.toml` 是 CLI 版本号的唯一真源。
- `Cargo.lock` 是生成的依赖锁文件，由发布脚本同步。
- `CHANGELOG.md` 的 `Unreleased` 段是下一版本发布说明的唯一真源。
- README、OpenClaw skill 和其他说明文件不得复制当前版本号。

## 准备发布 PR

日常变更应把面向用户的说明加入 `CHANGELOG.md` 的 `Unreleased` 段。发布时，
从最新 `main` 创建独立分支并运行：

```bash
python3 scripts/release.py prepare patch
```

也可使用 `minor` 或 `major`。脚本会：

1. 读取 `crates/standx-cli/Cargo.toml` 的当前版本。
2. 拒绝 `Cargo.lock` 与 manifest 已经漂移的状态。
3. 拒绝空的 `Unreleased` 或重复的目标版本 section。
4. 同步更新 manifest 和 lockfile。
5. 把 `Unreleased` 内容提升为带日期的版本 section，再留下新的空
   `Unreleased`。

检查 diff 后按正常流程提交发布 PR。PR 的 CI 会运行发布脚本单测和版本一致性检查。

## 发布稳定版

发布 PR 合入并且 `main` CI 通过后，只从 `main` 触发：

```bash
gh workflow run release.yml --ref main -f version=X.Y.Z
```

不要提前在 GitHub UI 创建 tag 或 Release。`Stable Release` workflow 会在同一次
运行中完成测试、三平台构建、打包、GitHub Release 和 Homebrew 更新，因此不依赖
`GITHUB_TOKEN` 再触发第二条 workflow。

发布门禁会拒绝：

- 非 `main` ref。
- 非稳定 `X.Y.Z` 输入。
- 输入版本、Cargo version、`Cargo.lock` 或 changelog 不一致。
- 已存在的 tag 或 Release，包括构建期间发生的竞争创建。
- 原生构建产物的 `standx --version` 与 Cargo version 不一致。
- workspace tests、fmt、Clippy、安装脚本测试或任一平台构建失败。

发布说明由以下命令从 changelog 精确提取：

```bash
python3 scripts/release.py notes --version X.Y.Z
```

## 验证命令

```bash
python3 -m unittest scripts/test_release.py
python3 scripts/release.py verify

cargo test --workspace --locked --offline
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked --offline -- -D warnings
sh -n install.sh
sh scripts/test_install.sh
```

如需额外验证本机构建：

```bash
cargo build --release --locked
python3 scripts/release.py verify --binary target/release/standx
```

## Pre-release

现有 `vX.Y.Z-rc.N` tag 流程保持不变：tag 的基础版本必须与 Cargo version 一致，
然后 `auto-prerelease` 构建并上传产物，不更新 Homebrew。

## 错误发布恢复

不要静默移动或覆盖已经公开的稳定 tag。若公开资产来自错误提交，保留审计记录并
前向发布下一个 patch 版本。
