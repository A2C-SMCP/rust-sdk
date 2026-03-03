---
name: release
description: 管理 workspace 统一版本号、cargo-release 升版和 git-cliff 生成 CHANGELOG。当用户需要升版、发布 crate 或生成变更日志时使用。
---

# Release — Workspace 版本管理

本项目采用 **workspace 版本继承** 模式：版本号只在根 `Cargo.toml` 的 `[workspace.package].version` 声明一次，所有子 crate 通过 `version.workspace = true` 继承。升版时改一处，全局同步。

## 第 1 步：确认当前版本状态

读取根 [`Cargo.toml`](../../Cargo.toml) 中的版本声明：

```toml
[workspace.package]
version = "0.1.0"   # ← 唯一的版本源
```

验证所有子 crate 均使用继承（而非硬编码版本号）：

```bash
grep -r 'version.workspace = true' crates/*/Cargo.toml
```

若某个子 crate 仍写着 `version = "x.y.z"`，需改为 `version.workspace = true`。

## 第 2 步：升版

根据变更性质选择级别：

```bash
cargo release patch   # 0.1.0 → 0.1.1（bug fix）
cargo release minor   # 0.1.0 → 0.2.0（新功能，向后兼容）
cargo release major   # 0.1.0 → 1.0.0（破坏性变更）
```

`cargo-release` 会自动完成：
1. 修改 `[workspace.package].version`
2. 同步更新 `[workspace.dependencies]` 中内部 crate 的 `version` 字段
3. 创建 commit 和 git tag

行为由根 `Cargo.toml` 中的 [`[workspace.metadata.release]`](../../Cargo.toml) 控制。

如果只想预览而不实际执行：

```bash
cargo release patch --dry-run
```

## 第 3 步：生成 CHANGELOG

使用 git-cliff 基于 Conventional Commits 自动生成：

```bash
git cliff -o CHANGELOG.md            # 全量生成
git cliff --unreleased --prepend CHANGELOG.md  # 仅追加未发布的变更
```

分组规则定义在 [`cliff.toml`](../../cliff.toml) 中——`feat` 归入 Features、`fix` 归入 Bug Fixes，以此类推。

## 第 4 步：发布到 crates.io

发布前先 dry-run 验证（按依赖顺序）：

```bash
cargo publish --dry-run -p smcp
cargo publish --dry-run -p smcp-agent
cargo publish --dry-run -p smcp-computer
cargo publish --dry-run -p smcp-server-core
cargo publish --dry-run -p smcp-server-hyper
cargo publish --dry-run               # 根包 a2c-smcp
```

顺序很重要：`smcp` 是基础依赖，必须先发布；`smcp-server-hyper` 依赖 `smcp-server-core`，也需要在其之后。完整的 CI 发布流程见 [`.github/workflows/publish.yml`](../../.github/workflows/publish.yml)。

## 新增子 crate 时的检查清单

1. 子 crate `Cargo.toml` 使用 `version.workspace = true`
2. 根 `Cargo.toml` 的 `[workspace.dependencies]` 添加对应条目（带 `path` 和 `version`）
3. 其他 crate 引用该依赖时使用 `workspace = true`
4. 发布流程中补充对应的 `cargo publish` 步骤
