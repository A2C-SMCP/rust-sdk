---
name: release
description: 管理 workspace 统一版本号、cargo-release 升版和 git-cliff 生成 CHANGELOG。当用户需要升版、发布 crate 或生成变更日志时使用。
arguments-hint: "<patch|minor|major> <release|rc> — 第一个参数: 升级策略 patch(bug fix)/minor(新功能)/major(破坏性变更); 第二个参数: release(正式版)/rc(预发版)"
---

# Release — Workspace 版本管理

本项目采用 **workspace 版本继承** 模式：版本号只在根 `Cargo.toml` 的 `[workspace.package].version` 声明一次，所有子 crate 通过 `version.workspace = true` 继承。

本地职责是**升版 + 打 tag + 推送**，实际发布由 GitHub Actions 流水线完成。

## 第 1 步：确认当前版本状态

读取根 [`Cargo.toml`](../../Cargo.toml) 中的版本声明：

```toml
[workspace.package]
version = "0.1.1"   # ← 唯一的版本源
```

验证所有子 crate 均使用继承（而非硬编码版本号）：

```bash
grep -r 'version.workspace = true' crates/*/Cargo.toml
```

若某个子 crate 仍写着 `version = "x.y.z"`，需改为 `version.workspace = true`。

## 第 2 步：使用 cargo-release 升版

`cargo-release` 仅用于管理版本号，**不执行 `cargo publish`**（`publish = false` 已在配置中设定）。

根据变更性质选择级别：

```bash
cargo release patch --execute   # 0.1.1 → 0.1.2（bug fix）
cargo release minor --execute   # 0.1.1 → 0.2.0（新功能，向后兼容）
cargo release major --execute   # 0.1.1 → 1.0.0（破坏性变更）
```

`cargo-release` 会自动完成：
1. 修改 `[workspace.package].version`
2. 同步更新 `[workspace.dependencies]` 中内部 crate 的 `version` 字段
3. 创建 release commit 和 git tag（如 `v0.1.2`）
4. 推送 commit 和 tag 到远程

行为由根 `Cargo.toml` 中的 [`[workspace.metadata.release]`](../../Cargo.toml) 控制。

预览模式（默认不加 `--execute` 即为 dry-run）：

```bash
cargo release patch
```

## 第 3 步：生成 CHANGELOG

使用 git-cliff 基于 Conventional Commits 自动生成：

```bash
git cliff -o CHANGELOG.md            # 全量生成
git cliff --unreleased --prepend CHANGELOG.md  # 仅追加未发布的变更
```

分组规则定义在 [`cliff.toml`](../../cliff.toml) 中——`feat` 归入 Features、`fix` 归入 Bug Fixes，以此类推。

## 第 4 步：在 GitHub 上触发发布

推送 tag 后，在 GitHub 上创建 Release 即可触发 CI 发布流水线：

```bash
# 方式一：通过 gh CLI 创建 Release（推荐）
gh release create v0.1.2 --title "v0.1.2" --generate-notes

# 方式二：手动触发 workflow
gh workflow run "Publish to crates.io"
```

流水线会按依赖顺序自动发布所有 crate，完整定义见 [`.github/workflows/publish.yml`](../../.github/workflows/publish.yml)。

> **定序必须按依赖树核对，不靠印象**：cargo publish 会校验全部依赖（**含 dev-dependencies**）
> 在 registry 中已有索引版本。v0.3.2 事故：smcp-server-hyper 排在 smcp-agent/smcp-computer
> 之后（后者 dev-deps 引用 `smcp-server-hyper ^x.y.z`）→ 三个 crate 未发布。核对方式：
> `grep -l 'smcp-server-hyper' crates/*/Cargo.toml` 找出谁依赖谁，再回看 workflow 步骤顺序。

本地可提前用 dry-run 验证打包是否正常：

```bash
cargo publish --dry-run -p smcp
```

## 第 5 步：发布验收与失败恢复

触发发布后，轮询 workflow 运行状态直到完成：

```bash
# 查看最近的 workflow 运行
gh run list --workflow=publish.yml --limit=1

# 监视运行状态（会阻塞直到完成）
gh run watch <run-id>
```

**⚠️ job success ≠ 发布成功**。publish.yml 每个发布步骤带 `|| true`（容忍
already-exists 以便重跑），真实失败也会被吞掉、workflow 仍绿（v0.2.2/v0.2.3/v0.3.2
三次实测）。**必须逐 crate 验收 crates.io**（API 不传 User-Agent 会被 403 拒绝 → 误判
"未发布"）：

```bash
for c in a2c-smcp smcp smcp-agent smcp-client-transport smcp-computer \
         smcp-server-core smcp-server-hyper; do
  code=$(curl -s -o /dev/null -w '%{http_code}' \
    -H 'User-Agent: a2c-smcp-release-check' \
    "https://crates.io/api/v1/crates/$c/<version>")
  echo "$c <version> -> $code"   # 全部应为 200
done
```

- 若**全部 200**：向用户确认发布完成并附上 Release 链接。
- 若有 404/失败：**优先修复流水线后重跑**，不要绕道本地手工 `cargo publish`
  （crates.io 版本不可删且失败不留在 CI 痕迹里，只作当次兜底）：

```bash
# 1. 用全量日志定位真因（--log-failed 抓不到——每步 exit 0，必须全量 grep）
gh run view <run-id> --log > /tmp/publish.log && grep -n 'error' /tmp/publish.log

# 2. 修 publish.yml（依赖序 / wait-for-index curl 补 User-Agent）→ push main（develop 同步）

# 3. 重跑（已在 registry 的 crate 会 "already exists" 被 || true 跳过，自动补发缺的）
gh workflow run publish.yml --ref main
```

- **rust-cache ENOENT 是噪声，不是发布失败**：Validate job 的 Post Cache 步骤在
  `target/package/<crate>-<ver>/tests/{target,trybuild}`（cargo package 剔除的目录）
  上 `opendir` 扑空，页面 4 条红 annotation 但 job 仍绿，每版必现。勿据此判失败。

## 新增子 crate 时的检查清单

1. 子 crate `Cargo.toml` 使用 `version.workspace = true`
2. 根 `Cargo.toml` 的 `[workspace.dependencies]` 添加对应条目（带 `path` 和 `version`）
3. 其他 crate 引用该依赖时使用 `workspace = true`
4. [`.github/workflows/publish.yml`](../../.github/workflows/publish.yml) 中补充对应的 publish 步骤，且**放对位置**：必须出现在所有依赖它的 crate（含仅 dev-deps 引用）发布步骤**之前**
