# a2c-smcp
The official Rust SDK for A2C-SMCP

---

# A2C-SMCP Rust SDK

A Rust implementation of the A2C-SMCP protocol, providing Agent, Computer, and Server components for building intelligent agent systems with tool execution capabilities.

## 🚀 Quick Start

Add to your `Cargo.toml`:

```toml
[dependencies]
a2c-smcp = { version = "0.1.0", features = ["agent", "computer"] }
```

Or use all features:

```toml
[dependencies]
a2c-smcp = { version = "0.1.0", features = ["full"] }
```

## 📦 Features

- **agent** - Agent client for connecting to SMCP servers and calling tools
- **computer** - Computer client for managing MCP servers and desktop resources  
- **server** - Server implementation with Socket.IO support
- **full** - Enables all features

## 📋 Project Structure

This is a **real workspace** with a main package that aggregates sub-crates:

```
rust-sdk/
├── src/              # Main package entry point (re-exports based on features)
├── tests/            # Cross-crate integration tests
├── crates/
│   ├── smcp/         # Core protocol types
│   ├── smcp-agent/   # Agent implementation
│   ├── smcp-computer/# Computer implementation
│   ├── smcp-server-core/    # Server core logic
│   └── smcp-server-hyper/   # Hyper adapter for server
└── Cargo.toml        # Workspace + main package configuration
```

本仓库目标：使用 Rust 实现 A2C-SMCP 协议，并对齐 `python-sdk` 的能力边界与使用体验。

本 README 给出 Rust SDK 的技术选型与实现路线，并明确当前版本的能力边界。


## 1. 背景与协议轮廓（来自 python-sdk）

Python 参考实现中，A2C-SMCP 的最核心抽象是三大模块：

- **Computer**：管理 MCP Servers、聚合工具列表与桌面资源；并负责接收来自 Agent 的工具调用请求、执行工具并返回结果，同时向 Server 上报更新。
- **Server**：中心信令服务，负责会话管理、转发调用、广播通知。
- **Agent**：业务侧智能体客户端，通过协议事件调用 Computer 的工具。

传输层：`python-sdk` 选择 **Socket.IO** 做实时通信（带 namespace 与 room），并定义了 `SMCP_NAMESPACE = /smcp`。

事件命名规范（来自 `python-sdk/a2c_smcp/smcp.py`）：

- `client:*`：由 Agent 发起、由 Server 转发到特定 Computer 执行（例如 `client:tool_call`、`client:get_tools`、`client:get_desktop`）。
- `server:*`：由 Computer/Agent 发起，Server 负责执行并转换为通知（例如 join/leave office、update desktop/config、cancel tool call）。
- `notify:*`：只由 Server 发出，向 room 广播状态变更（例如 enter/leave office、update config/tool list/desktop）。

Rust 版本以 `smcp.py` 的事件与数据结构为“权威源”，优先保证互通与行为一致，再逐步补齐 Computer/CLI/Desktop 等高级能力。


## 2. Rust 技术选型

### 2.1 异步运行时
- **Tokio**
- 选择理由：
  - Rust 网络生态与 Socket.IO/WebSocket/HTTP client-server 基本都以 Tokio 为默认运行时。
  - 便于统一 Server 与 Agent/Computer 客户端的并发模型。


### 2.2 Server 端框架：Socket.IO 紧绑定 + HTTP 承载层可插拔
Python 版本最小集成示例是 `FastAPI + python-socketio(ASGI)`。从 SMCP 视角：

- **实时通信层固定使用 Socket.IO**（namespace/room/ack/notify）。
- **消息格式当前只支持 JSON**（`serde_json`）。
- **HTTP 不等于必须提供 REST API**；它主要作为 Socket.IO 的承载监听器，用于握手、升级（WebSocket）、以及 long-polling 回退。

为了保持“开源协议 SDK”依赖最小且方便使用者集成：

- **Socket.IO Server：socketioxide**（紧绑定，不可替换）。
 - `socketioxide` 是当前 Rust 生态中唯一成熟的 Socket.IO Server 实现，SDK 直接依赖它。
 - 它通过 Tower Layer/Service 模式工作，天然支持与多种 HTTP 框架集成。
- **HTTP 承载层默认：Hyper**（最小依赖/最通用）。
- **HTTP 承载层可替换**：`socketioxide` 可作为 Tower Layer 嵌入任何 Tower 兼容框架（Axum/Salvo/Viz 等），使用者可以在自己的项目中选择框架。


### 2.3 Socket 客户端（Agent/Computer 模块）
Agent/Computer 需要连接 Server，并支持：

- connect with headers/auth
- emit/call（ack）
- on notify events
- room（office_id）管理

Rust 侧采用：

- **rust_socketio（客户端）**
 - 注意：`socketioxide` 是纯 Server 端实现，不提供客户端功能。Agent/Computer 作为客户端需要使用 `rust_socketio` crate。
  - 支持点（来自 docs.rs）：
    - **namespace**：`ClientBuilder::namespace("/smcp")`；但**一个 socket 只能连接一个 namespace**，多 namespace 需要多个 socket。
    - **ack + timeout**：`emit_with_ack(event, data, Duration, callback)`，可按每次调用设置超时。
    - **reconnect/backoff**：提供开关与参数：
      - `reconnect(true)` / `reconnect_on_disconnect(true)`
      - `reconnect_delay(min, max)`（最小/最大重连间隔）
      - `max_reconnect_attempts(n)`（最大重试次数）
    - **headers/auth**：支持 `opening_header(k, v)` 与 `auth(json!)`，可对齐 Python 端的 header api-key 与 auth payload。
  - 注意点：
    - async 版本需要开启 feature `async`，且文档标注当前 async 实现处于 beta，接口可能变化。
   - 需要在正式开发前验证 `rust_socketio` 与 `socketioxide` 的互通性（见 `tests/e2e/`）。


### 2.4 序列化 / 类型校验
Python 版大量使用 TypedDict/Pydantic 做校验。Rust 端遵循“一切从简”原则：优先保证协议载荷能稳定反序列化为结构化数据。

- **serde + serde_json**：作为 wire format 的默认实现（与 Socket.IO JSON payload 最契合），并承担“反序列化即结构校验”的职责。
- **类型建模策略**：
  - 协议结构体使用 `#[derive(Serialize, Deserialize)]`
  - 事件 payload 尽量用强类型，而不是 `serde_json::Value`
- **类型边界划分**（方案 C）：
 - `smcp` crate：只放**协议层公共类型**（事件常量、`AgentCallData`、`ToolCallReq`、`GetToolsReq/Ret`、`EnterOfficeReq` 等跨角色共享的协议结构）
 - `smcp-computer` crate：放 Computer 专属配置类型（如 `MCPServerConfig`、`MCPServerStdioConfig` 等）
 - `smcp-agent` crate：放 Agent 专属类型（如 `AgentEventHandler`）

说明：工具侧的 `params_schema/return_schema` 在本 SDK 中以 MCP Tools 的 schema 为准，当前仅做透传，不在 SDK 内生成或做额外 schema 校验。


### 2.5 错误处理
- **thiserror**：定义 `SmcpError` 等错误枚举。
- **anyhow**：应用层（CLI/示例）快速聚合错误。
- 错误边界：
  - SDK 层对外暴露稳定的 `Result<T, SmcpError>`
  - CLI/示例使用 `anyhow::Result<()>` 即可。


### 2.6 日志与可观测性
- **tracing + tracing-subscriber**
- 原因：异步场景的结构化日志更适合排查事件流（尤其是 room 广播、ack 超时、重连）。


### 2.7 CLI（Computer 模块）
Python 版 Computer 侧提供交互式 CLI（添加 server、start/stop、status、socket connect/join、notify update）。

Rust 端采用：

- **clap**：命令行参数解析（只负责 args/subcommands，不负责交互能力）。
- **pexpect 级交互**：使用 **expectrl** 实现“spawn + PTY + expect/send”风格的交互控制；比直接用管道读写的 subprocess 方式更适合做强交互 CLI。
- **颜色与终端能力**：
  - 轻量彩色输出：`owo-colors`（或兼容生态的 `anstyle` 体系）
  - 终端事件与渲染基础：`crossterm`
-（可选）更强交互体验：
  - TUI：`ratatui`（基于 `crossterm`）
  - 行编辑/补全：`reedline`/`rustyline`（如需类似 shell 的输入体验）


### 2.8 测试策略
- **单元测试**：协议结构体序列化/反序列化、事件路由与权限校验。
- **集成测试**：起一个内嵌 Server（Socket.IO），用 Agent/Computer 客户端对打，覆盖：
  - join office → notify enter_office
  - get_tools / tool_call 的转发与 ack
  - update_desktop 广播与 Agent 拉取
- **端到端（e2e）**：对齐 python 版的测试思路。

#### 目录与文件组织（真实 Workspace 规范）

> **重要**：本仓库采用**真实 workspace**（同时有 `[workspace]` 和 `[package]` 段）。
> 根目录包 `a2c-smcp` 作为主入口，可以包含 `src/` 和 `tests/` 目录。
- **单元测试（unit tests）**
  - 放置位置：各 crate 的 `src/**` 内 `#[cfg(test)] mod tests { ... }`。
  - 适用范围：纯函数/结构体方法、序列化/反序列化、错误映射、事件名称与 payload 组装等。
  - 组织规范：
    - 每个模块自己带测试，避免依赖真实网络/真实进程。
    - 使用“表驱动”测试（`cases: Vec<(input, expected)>`）来覆盖边界条件。
    - 公共测试工具函数放到 `src/test_utils.rs` 或 `src/test_utils/mod.rs`（仅在 `cfg(test)` 下编译）。

- **集成测试（integration tests）**
  - 放置位置：
    - 根目录 `tests/`：跨 crate 联合测试（如 Agent + Computer + Server）
    - 各 crate 的 `tests/` 目录：单个 crate 的 API 测试    - 文件名按场景：`join_leave.rs`、`tool_call_ack.rs`、`socketio_interop.rs`。
    - 文件名按场景：`full_stack.rs`、`agent_computer.rs`、`socketio_interop.rs`。
    - 测试函数按行为：`test_full_stack_integration()`。
  - 约束建议：
    - 网络端口使用 `127.0.0.1:0` 自动分配，避免 CI 冲突。
    - 用超时（`tokio::time::timeout`）包裹等待，避免卡死。
    - 共享 fixtures 放到 `tests/common/mod.rs`。
    - 使用 `skip_if_no_feature!` 宏根据 features 跳过测试。    - 用超时（`tokio::time::timeout`）包裹等待，避免卡死。
    - 共享 fixtures 放到 crate 内的 `tests/common/mod.rs`。
- **端到端测试（e2e tests）**
  - 放置位置：根目录 `tests/e2e/`（如果需要更慢、更依赖环境的测试）。
  - 适用范围：跨进程/跨组件的真实链路（例如启动 Computer 管理 MCP stdio server）。
  - 组织规范：
    - 依赖外部二进制（如 `npx`、真实 MCP server）要做可跳过策略。
    - 产物（临时目录、日志）统一写到 `target/tmp/<test_name>/`。    ```
  - 运行方式：`cargo test -p smcp-e2e-tests`
  - 组织规范：
    - 依赖外部二进制（如 `npx`、真实 MCP server）要做可跳过策略（例如环境变量开关）。
    - 产物（临时目录、日志）统一写到 `target/tmp/<test_name>/`。

## 3. 已确定的技术约束（Design Decisions）

### 3.1 传输层：必须 Socket.IO
本 SDK 的实时通信层**固定使用 Socket.IO**（对齐 `python-sdk` 的语义：namespace/room/ack/notify）。不考虑替换为 WebSocket/gRPC 等其它传输。

### 3.2 HTTP 承载层：最小依赖 + 可插拔
Socket.IO 在工程实现上需要一个 HTTP 监听器作为承载（握手、升级、long-polling）。为了保证依赖最小并方便使用者集成：

- 默认使用 **Hyper** 作为承载层（最小依赖/最通用）。
- SDK 设计应将“承载层”抽象为可替换接口/适配层：
  - 使用者可以在自己的项目中选择 Axum/Actix/Salvo 等框架，并把请求转发给 Socket.IO handler。
  - 本 SDK 不强制绑定任何具体 Web 框架。

### 3.3 消息格式：仅支持 JSON
当前版本**只支持 JSON payload**（`serde_json`），不支持二进制消息与大对象流式传输。后续若要支持图片/资源流，应通过独立通道或资源接口设计，而非在本版本内扩展。


## 4. Rust 端实现路线

路线按“先互通、再补齐”，避免一开始就把 Computer/CLI/Desktop 全部做完。

### 4.1 Milestone 1：协议与类型层（smcp）
- 定义 `SMCP_NAMESPACE` 与全部事件常量（与 `smcp.py` 对齐）
- 定义核心 payload：
  - `AgentCallData`、`ToolCallReq`、`GetToolsReq/Ret`
  - `EnterOfficeReq`、`LeaveOfficeReq`
  - `Update*Notification`、`ListRoomReq/Ret`
- 统一 `req_id` 生成策略（UUID）


### 4.2 Milestone 2：Server 最小实现（转发 + 广播）
- **核心原则：Socket.IO 层紧绑定 socketioxide，HTTP 承载层可插拔**
  - Server 核心逻辑依赖 Tokio + `socketioxide` + 协议类型；`socketioxide` 是唯一的 Socket.IO 实现，不可替换。
 - `socketioxide` 通过 Tower Layer/Service 模式工作，天然支持与 Axum/Salvo/Hyper 等框架集成。
  - 提供一个最小默认承载实现（Hyper），并将其作为“示例/默认 adapter”。使用者可以选择其他 Tower 兼容框架。
- 会话管理：sid ↔ name ↔ role ↔ office_id（类似 python `BaseNamespace`）
- 事件与语义：
  - `server:join_office` / `server:leave_office` → 广播 `notify:*`
  - `client:get_tools` / `client:get_desktop` / `client:tool_call` → 转发到指定 Computer 并等待 ack
  - `server:update_desktop` / `server:update_config` / `server:update_tool_list` → 广播 `notify:update_*`
- 鉴权：先实现 header api-key（对齐 Python `DefaultAuthenticationProvider`），后续再扩展。
- 工程化约束：
  - 统一对 ack/转发等待加 timeout，避免请求悬挂。
  - handler 内只处理 JSON payload（`serde_json`），不引入二进制分支。


### 4.3 Milestone 3：Agent 客户端最小实现
- connect（headers/auth）
- join_office
- emit_tool_call（带 timeout + cancel）
- 订阅 `notify:*` 并提供回调接口


### 4.4 Milestone 4：Computer 客户端最小实现
- 提供 get_tools、tool_call、get_desktop 的事件处理（被 Server call）
- 支持上报 update_desktop/tool_list/config


## 5. 与 MCP 的关系（对齐 python-sdk）

Python 版在 Agent 侧返回 `mcp.types::CallToolResult` 风格的数据结构，并在 Computer 侧管理多种 MCP Server（stdio/sse/streamable）。

Rust 端先把 SMCP 的“信令与工具调用转发”跑通；MCP Server 管理（stdio/sse）按分层实现：

- `computer::mcp_manager`：进程管理、连接管理
- `computer::tool_registry`：工具聚合与去重（解决 tool name 冲突，可对齐 `ToolMeta.alias` 思路）
- `computer::desktop`：window:// 资源聚合（后续迭代）


---

## 下一步

- 把 crate 分层落到代码结构：`smcp`（协议/类型） + `smcp-server-core`（会话/路由/鉴权） + `smcp-server-hyper`（默认承载适配）
- 做一轮最小互通 PoC：起 `smcp-server-hyper` + 一个最小 Computer + 一个最小 Agent，覆盖 join/get_tools/tool_call/notify
- 再决定是否提供额外的可选集成 crate（例如 `server-axum`），但不改变核心依赖最小与承载层可替换原则。
