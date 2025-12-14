---
description: 在 rust-sdk 中通过 git submodule + [patch.crates-io] vendor rust_socketio（用于补丁版 ACK 能力）
---

# 背景与目标

本手册用于在 `rust-sdk` 仓库中以 **git submodule** 的方式引入你们 fork 的 `rust-socketio` 源码，并通过 Cargo 的 **`[patch.crates-io]`** 机制，让整个 workspace 在编译时使用该补丁版 `rust_socketio`。

目标：
- 业务 crate 的依赖写法保持为 `rust_socketio = { version = "0.6", features = ["async"] }`（或 workspace 统一依赖）。
- 编译时实际使用 `vendor/rust-socketio` 中的源码（包含你们需要的 Server→Client ACK 扩展，例如 PR #493 引入的 `ack()` 能力）。
- 可稳定用于生产：依赖版本可控、可回滚、可审计。

# 适用范围

- 适用于本仓库 workspace（`/Users/JQQ/RustroverProjects/rust-sdk`）内所有依赖 `rust_socketio` 的 crate。

# 前置准备

- 你们已经在组织内 fork 了上游仓库（例如 `1c3t3a/rust-socketio`），并将所需补丁合入 fork（比如 cherry-pick PR #493 的提交）。
- 你们 fork 的仓库根目录应当是一个 Cargo crate（包含 `Cargo.toml`），crate 名称仍然为 `rust_socketio`。

# 一、引入 git submodule

在仓库根目录执行以下步骤：

## 1. 创建 vendor 目录

确保存在目录：

- `vendor/`

## 2. 添加 submodule

将你们 fork 的 `rust-socketio` 作为 submodule 添加到：

- `vendor/rust-socketio`

示例命令（请将 URL 换成你们自己的）：

```bash
git submodule add <YOUR_FORK_GIT_URL> vendor/rust-socketio
```

建议：
- 优先使用 **SSH URL**（便于企业内部权限管理）。
- submodule 固定在一个具体 commit 上，确保生产可重复构建。

## 3. 初始化/更新 submodule（给新克隆的开发者/CI 用）

```bash
git submodule update --init --recursive
```

# 二、用 [patch.crates-io] 覆盖 rust_socketio

在 **workspace 根 `Cargo.toml`** 中增加 patch 段落。

> 注意：必须在 workspace 根（本仓库顶层 `Cargo.toml`），否则可能只对单个 crate 生效。

## 推荐写法（覆盖 crates.io 的 rust_socketio）

```toml
[patch.crates-io]
rust_socketio = { path = "vendor/rust-socketio" }
```

说明：
- `path` 必须指向 vendored crate 的 `Cargo.toml` 所在目录。
- crate 名称必须匹配（这里是 `rust_socketio`）。

# 三、依赖声明保持不变

你们现有 workspace 里已经有：

```toml
rust_socketio = { version = "0.6", features = ["async"] }
```

保持不动即可。Cargo 会在解析时用 `[patch.crates-io]` 将其替换为本地路径。

# 四、验证是否生效

建议做两类验证：

## 1) 编译验证

- `cargo build` 能通过
- 相关测试能通过（尤其是涉及 Socket.IO 的 integration tests）

## 2) 依赖来源验证

确认 `rust_socketio` 的来源来自 `vendor/rust-socketio`。

常见做法：
- 通过 `cargo tree` 检查 `rust_socketio` 指向的路径/来源
- 或在 CI 中增加一条检测脚本，确保没有走 crates.io 的 `rust_socketio`

# 五、升级、回滚与维护策略

## 1) 升级补丁版 rust_socketio

升级本质是：
- 在 `vendor/rust-socketio` submodule 中切换到新的 commit（例如合入新补丁或上游更新）。

建议流程：
- 在 fork 仓库上合并/挑选提交
- 在主仓库更新 submodule 指针
- 跑完整测试

## 2) 回滚

回滚非常简单：
- 将 submodule 指针回退到上一个稳定 commit

## 3) 与上游合并后的迁移

当上游 `rust_socketio` 官方版本合并并发布了你们需要的功能后：
- 评估切回 crates.io 官方版本（移除 `[patch.crates-io]`）
- 同时移除 submodule（或保留用于应急）

# 六、CI 与交付建议

建议在 CI 中加入：

- 使用 `--locked`：避免隐式修改 `Cargo.lock`
- 若你们需要更强一致性，可加入 `--frozen`

并确保 CI 拉取 submodule：

- `git submodule update --init --recursive`

# 七、许可证与合规注意事项

Vendor 源码意味着你们仓库中包含第三方代码。

建议：
- 保留 vendored 仓库内的 `LICENSE` / `NOTICE` 文件
- 在企业合规流程中记录：你们使用的是哪个 commit（submodule 指针即可作为证据）

# 常见问题（FAQ）

## Q1: 为什么不用 `git = "..."` 直接依赖？

- 生产环境中 `git = "..."` 通常可行，但更依赖网络与外部可用性。
- submodule + path patch 让源码完全落在仓库内，构建更稳定、更可审计。

## Q2: 可以只对某个 crate 生效吗？

可以把 `[patch.crates-io]` 放到单个 crate 的 `Cargo.toml`，但不建议。
你们这是 workspace 级别能力扩展，放在根 `Cargo.toml` 最清晰、最一致。
