<!--
* 文件名: smcp-agent-implementation-plan
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: None
* 描述: SMCP Rust Agent（smcp-agent crate）实现计划与验收清单
-->

# SMCP Rust Agent（`smcp-agent`）实现计划

本文档用于指导在 Rust SDK 中实现/完善 `smcp-agent` 模块，目标是**完整复现** `examples/python/a2c_smcp/agent` 的行为与语义（以 Python 行为为准），并按 Rust 最佳实践提供可维护的 SDK。

## 1. 背景与范围

- **协议基准**：`examples/python/a2c_smcp/smcp.py` 与 `examples/python/a2c_smcp/agent/*`
- **通信方式**：Socket.IO
- **实现策略**：先互通、再补齐；Agent 侧实现要求不做“最小”，而是按 Python 参考实现做“可用 SDK”。

### 1.1 三端角色

- **Server**：Rust（强绑定 `socketioxide`），负责转发、广播、会话管理与鉴权。
- **Agent**：Rust（本计划重点），作为客户端连接 Server，发起 tool_call/get_tools/get_desktop 等。
- **Computer**：可由 Python 或 Rust 实现，需与 Rust Server 互通。

## 2. 关键结论（最终选型）

### 2.1 Server/Client 技术栈

- **Server**：`socketioxide`（唯一 Socket.IO Server 实现，不可替换），承载层可插拔（Hyper 默认 adapter）。
- **Agent/Computer Client**：使用 **vendor 的 `rust-socketio`**（原因：项目内已实现并覆盖了 Server->Client ACK 能力，且本协议大量依赖 `call` 等待 ack）。

> 说明：`socketioxide` 目前定位为 Server 实现，并不提供可用的 Socket.IO Client SDK。

### 2.2 工程约束

- handler 内只处理 **JSON payload（`serde_json`）**，不引入二进制分支。
- 所有 ack/转发等待必须加 **timeout**，避免请求悬挂。

## 3. Python Agent 行为契约（Rust 必须对齐）

本节为“硬约束”，Rust 端实现不得随意偏离。

### 3.1 命名空间与事件（示例，不完整）

- 命名空间：`/smcp`
- Agent 发起：
  - `server:join_office` / `server:leave_office`
  - `client:get_tools` / `client:get_desktop` / `client:tool_call`
  - `server:list_room`
  - `server:tool_call_cancel`
- Agent 监听（Server 广播）：
  - `notify:enter_office`：收到后自动拉一次 tools
  - `notify:update_config`：收到后自动拉一次 tools
  - `notify:update_desktop`：默认自动拉一次 desktop（建议可配置开关）
  - `notify:leave_office`

### 3.2 `req_id` 生成规则

- Python：`uuid.uuid4().hex`
- Rust：统一采用 UUID v4（建议输出 hex 字符串以完全对齐 Python）。

### 3.3 超时与取消（tool_call）

- `client:tool_call` 使用 `call` 等待 ack。
- 若超时：
  - 发送 `server:tool_call_cancel`，payload 为 `{ agent, req_id }`
  - 返回一个错误结果（Python 返回 MCP `CallToolResult`，`isError=true`，文本包含 `req_id`）

### 3.4 `req_id` 强校验

- `get_tools` / `get_desktop` / `list_room`：响应必须满足 `response.req_id == request.req_id`，不一致视为协议错误。

## 4. Rust 模块设计（推荐）

> Rust 不建议照搬 Python 的多继承结构，建议采用“核心 async + 可选 sync facade”的方式。

### 4.1 crate 划分

- `crates/smcp`：协议与类型层（Milestone 1）
  - `SMCP_NAMESPACE`、事件常量（与 `smcp.py` 完全一致）
  - payload types：`AgentCallData/ToolCallReq/GetToolsReq/Ret/EnterOfficeReq/LeaveOfficeReq/...`
  - `req_id` 统一生成策略

- `crates/smcp-agent`：Agent SDK（本计划）
  - 对外：`AsyncSmcpAgent`（或 `SmcpAgent`）+ `SyncSmcpAgent`（feature）
  - 内部：transport（socketio client）、notify handlers、状态缓存、错误类型

### 4.2 `smcp-agent` 内部模块建议

- `auth.rs`
  - header api-key（对齐 Python DefaultAuthenticationProvider 思路）
  - 预留 Socket.IO `auth` payload
- `transport/socketio.rs`
  - 仅收发 `serde_json::Value`
  - 提供 `emit_json` 与 `call_json(timeout)`
- `client.rs`
  - 高层 API：`connect/join_office/leave_office/tool_call/get_tools/get_desktop/list_room/cancel_tool_call`
- `handlers.rs`
  - 注册 `notify:*` 并分发到用户回调
  - enter_office/update_config：自动 `get_tools`；update_desktop：自动 `get_desktop`
- `model.rs`
  - `tools_cache`（按 computer 分组）
  - `known_sessions`/`known_computers`
- `error.rs`
  - `SmcpAgentError`：网络错误、超时、协议错误（req_id mismatch）、序列化错误等
- `config.rs`
  - 超时默认值、是否自动拉 desktop 等开关

## 5. Feature 设计：同时支持 async 与 sync

### 5.1 Feature 建议

- `async`：默认启用
  - 暴露 async API
- `sync`：可选
  - 在 async 实现外包一层阻塞 facade（内部自建 `tokio::runtime::Runtime`）

### 5.2 关键原则

- **async 为核心实现**：协议逻辑只写一份。
- **sync 只做封装**：同步方法内部 `runtime.block_on(async_fn)`。
- sync feature 默认关闭，避免把 runtime/阻塞语义强加给所有使用者。

## 6. 实施里程碑与详细步骤（可交付迭代）

### Sprint 1：补齐协议与类型层（`smcp`）

- 目标
  - 对齐 `smcp.py` 的 namespace、事件常量、payload 类型、req_id 规则。
- 产物
  - `SMCP_NAMESPACE` 与事件常量（**值必须一致**）
  - payload structs（`serde::{Serialize, Deserialize}`）
  - `req_id` 工具（UUID v4 hex）
- 验收
  - payload JSON roundtrip 单测
  - 事件常量对齐检查（snapshot 或断言）

### Sprint 2：Transport 层打通（vendor `rust-socketio`）

- 目标
  - 提供稳定的 `connect/emit/call(ack)` JSON-only API，并对所有等待加 timeout。
- 产物
  - `transport/socketio.rs`
  - `call_json`：带 timeout，超时返回 `SmcpAgentError::Timeout`
- 验收
  - 能连接本仓库 `smcp-server-hyper`（或测试 server）
  - 最小 `call -> ack` 成功用例

### Sprint 3：Async Agent 高层能力（核心）

- 目标
  - 完整复现 Python Agent 的主动调用能力。
- 产物
  - `join_office/leave_office`
  - `get_tools/get_desktop/list_room`
  - `tool_call(timeout -> cancel)`
  - `cancel_tool_call(req_id)`（显式 API）
- 验收
  - `req_id` mismatch 立即报错
  - tool_call 超时会 emit cancel，并返回错误结果（包含 req_id）

### Sprint 4：notify 订阅 + 回调 + 自动拉取

- 目标
  - 完整复现 Python 的事件订阅行为与副作用（自动拉 tools/desktop）。
- 产物
  - `notify:enter_office`：触发 handler + 自动 `get_tools` + `on_tools_received`
  - `notify:update_config`：自动刷新 tools
  - `notify:update_desktop`：默认自动 `get_desktop`（建议可配置开关）
  - `notify:leave_office`
- 验收
  - Computer 进入 office 后，Agent 自动拉 tools 并触发回调
  - update_config 后同样自动拉 tools
  - update_desktop 通知触发自动拉 desktop

### Sprint 5：Sync facade（feature=`sync`）

- 目标
  - 提供与 Python `sync_client` 类似的阻塞 API。
- 产物
  - `SyncSmcpAgent`：内部持有 `tokio::runtime::Runtime` + `AsyncSmcpAgent`
- 验收
  - sync 冒烟测试：connect/join_office/get_tools/tool_call

## 7. 测试策略（必须覆盖）

### 7.1 单元测试

- 类型层：payload 序列化/反序列化、req_id 格式
- 业务层：
  - tool_call 超时路径：断言会发送 cancel
  - req_id mismatch：断言返回协议错误

### 7.2 集成测试（Rust Agent ↔ Rust Server）

- 启动 `smcp-server-hyper` 测试 server fixture
- 测试：connect/join_office/list_room
- 若具备 computer mock：补 get_tools/tool_call

### 7.3 跨语言互通回归（建议 checklist）

- Rust Agent ↔ Python Computer
- Python Agent ↔ Rust Server

检查点：
- namespace/事件名一致
- payload 字段名一致
- ack 语义一致（timeout/cancel/req_id 校验）
- notify 行为与副作用一致（自动拉取）

## 8. 风险点与注意事项

- vendor `rust-socketio` 的 Server->Client ACK 能力“有用例覆盖但未生产验证”，出现可疑问题要及时反馈并协同排查。
- Socket.IO 的 `call/ack` 与重连场景容易引入悬挂：务必统一 timeout，并在 Drop/shutdown 时取消后台任务。

---

## 附：参考文件

- `examples/python/a2c_smcp/smcp.py`
- `examples/python/a2c_smcp/agent/base.py`
- `examples/python/a2c_smcp/agent/client.py`
- `examples/python/a2c_smcp/agent/sync_client.py`
