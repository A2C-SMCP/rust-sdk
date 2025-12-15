# DEV

面向熟练 Rust 工程师的上手指南：只保留本仓库约定与高频命令。

## 一键检查（提交前）

```bash
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

## 常用命令

```bash
# workspace / IDE 解析
cargo metadata --format-version 1

# 快速编译检查
cargo check
cargo check --workspace --all-features

# 运行默认 server（示例承载）
cargo run -p smcp-server-hyper

# 依赖管理（需要 cargo-edit）
cargo add <crate>
cargo add -D <crate>
cargo update
cargo tree
```

## 目录与职责

- `Cargo.toml`
  - workspace + 主包 `a2c-smcp` 配置，包含 features 定义。
- `src/`
  - 主包入口：根据 features 重新导出各子 crate 的 API。
- `tests/`
  - 跨 crate 集成测试：Agent + Computer + Server 联合测试。
  - `tests/common/`：共享测试工具和 fixtures。
- `crates/smcp/`
  - 协议与类型层：只放**协议层公共类型**（事件常量、`AgentCallData`、`ToolCallReq`、`GetToolsReq/Ret`、`EnterOfficeReq` 等跨角色共享的协议结构）。
- `crates/smcp-agent/`
  - Agent 客户端 SDK：connect/auth、join_office、tool_call（ack + timeout + cancel）、订阅 `notify:*`。
  - 包含 Agent 专属类型（如 `AgentEventHandler`）。
  - 依赖 `rust_socketio` 作为 Socket.IO 客户端。
- `crates/smcp-computer/`
  - Computer 客户端 SDK：注册 handler trait，处理 `client:get_tools`/`client:tool_call`/`client:get_desktop`，并支持上报 `server:update_*`。
  - 包含 Computer 专属配置类型（如 `MCPServerConfig`、`MCPServerStdioConfig` 等）。
  - 依赖 `rust_socketio` 作为 Socket.IO 客户端。
- `crates/smcp-server-core/`
  - Server 核心逻辑（会话/路由/鉴权/转发/广播），紧绑定 `socketioxide`。
- `crates/smcp-server-hyper/`
  - 默认 HTTP 承载适配（Hyper），作为示例/默认 adapter。
- `docs/`
  - 设计/协议补充文档。
- `examples/`
  - 可运行的最小示例。

## 对外 API 风格（Decision）

- 当前版本：采用 A（回调/事件驱动）。使用者通过注册 handler trait/回调函数处理事件；SDK 内部负责 spawn 接收循环、分发与 ack。
- 未来增强：如遇到需要更强背压/组合控制的场景，再追加 B（显式 await / stream）形态作为可选接口，不破坏 A 的现有使用方式。

## 测试约定（简要）

- **Unit**：放在各 crate 的 `src/**` 内 `#[cfg(test)]`，不做真实网络/真实进程。
- **Integration**：
  - 根目录 `tests/`：跨 crate 联合测试（如 Agent + Computer + Server）
  - 各 crate 的 `tests/`：单个 crate API 测试
  - 所有等待必须 `tokio::time::timeout`，公共 fixtures 放到 `tests/common/`。
- **E2E**：放在 `tests/e2e/`（如需要），建议做可跳过策略（feature/env），产物落 `target/tmp/<test_name>/`。

## 运行测试

```bash
# 运行所有测试（不包括E2E）
cargo test --workspace

# 运行特定 features 的测试
cargo test --features "agent,computer"

# 运行跨 crate 集成测试
cargo test --test full_stack --features full

# 运行单个 crate 的测试
cargo test -p smcp-server-core

# 运行 E2E 测试（需要 e2e feature）
cargo test --features e2e -- --ignored

# 运行所有测试包括 E2E
cargo test --workspace --features e2e

# 运行特定的 E2E 测试
cargo test --features e2e test_playwright_mcp_server_basic_connection -- --ignored
```

### E2E 测试说明

E2E 测试用于验证与真实的 MCP 服务器的集成，目前包括：

- **Playwright MCP Server 测试**：测试与 `npx @playwright/mcp@latest` 的集成
- 使用 stdio 模式启动服务器
- 测试包括基础连接、工具执行和错误处理

运行 E2E 测试前确保：
1. 已安装 Node.js 和 npm
2. `npx` 命令可用
3. 网络连接正常（用于下载 Playwright MCP 包）

E2E 测试默认被忽略，需要使用 `--features e2e` 和 `-- --ignored` 参数运行。

## 代码覆盖率

需要先安装 cargo-llvm-cov：
```bash
cargo install cargo-llvm-cov
```

### 生成覆盖率报告

```bash
# 运行测试并生成覆盖率报告（显示到终端）
cargo llvm-cov --workspace

# 生成 HTML 覆盖率报告（会在 target/llvm-cov/html/index.html）
cargo llvm-cov --workspace --html

# 生成覆盖率报告并显示未覆盖的行
cargo llvm-cov --workspace --show-missing-lines

# 只对特定 crate 生成覆盖率
cargo llvm-cov -p smcp-server-core --show-missing-lines

# 生成 LCOV 格式报告（用于 CI 集成）
cargo llvm-cov --workspace --lcov --output-path lcov.info
```

### 常用覆盖率组合命令

```bash
# 运行测试并打开 HTML 报告（macOS）
cargo llvm-cov --workspace --html && open target/llvm-cov/html/index.html

# 查看特定文件的覆盖率详情
cargo llvm-cov --workspace --show-missing-lines --file src/lib.rs
```

## vendor/rust-socketio 修改说明 / Vendor rust-socketio Modifications

### 主要修改 / Key Modifications

本仓库使用的是修改版的 `rust-socketio`，主要增强并发 ACK 支持：

1. **并发 ACK 支持 / Concurrent ACK Support**
   - `Payload` 枚举变体新增可选 `ack_id` 字段
   - `Payload::Text`、`Payload::Binary`、`Payload::String` 现在都支持 `ack_id`
   - 新增 `ack_with_id()` 方法用于显式指定 ack_id
   - 新增 `with_ack_id()`、`ack_id()`、`set_ack_id()` 辅助方法

2. **测试增强 / Test Enhancements**
   - 添加 `test_concurrent_ack.rs` 和 `test_async_concurrent_ack.rs`
   - 验证并发 ACK 功能的正确性

### 测试执行与覆盖 / Testing Execution & Coverage

#### 运行 rust-socketio 测试

```bash
# 进入 vendor 目录
cd vendor/rust-socketio

# 运行所有测试
cargo test

# 运行特定模块测试
cargo test test_concurrent_ack
cargo test test_async_concurrent_ack

# 运行带输出信息的测试
cargo test -- --nocapture

# 运行单个测试文件
cargo test --lib test_concurrent_ack
```

#### 生成覆盖率报告

```bash
# 在 vendor/rust-socketio 目录下

# 安装覆盖率工具（如果未安装）
cargo install cargo-llvm-cov

# 生成覆盖率报告
cargo llvm-cov --html

# 查看未覆盖的行
cargo llvm-cov --show-missing-lines

# 只对 socketio crate 生成覆盖率
cargo llvm-cov -p socketio --show-missing-lines

# 生成 LCOV 格式报告
cargo llvm-cov --lcov --output-path lcov.info
```

#### 测试覆盖率最佳实践

```bash
# 一键运行测试并生成 HTML 报告
cargo llvm-cov --workspace --html && open target/llvm-cov/html/index.html

# 只运行修改相关测试并查看覆盖率
cargo llvm-cov --files src/payload.rs src/client/client.rs

# 持续监控覆盖率变化
cargo llvm-cov --summary --output-file coverage.txt
```
