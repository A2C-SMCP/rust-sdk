---
description: 测试原则
---

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