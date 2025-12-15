## Computer Socket.IO 客户端模块概览

`a2c_smcp/computer/socketio` 目录下的代码，主要实现了 **Computer 侧的 SMCP 协议 Socket.IO 客户端**，用于把本地 `Computer` 与中心信令服务器（Server 端 SMCP Namespace）连接起来，让 Agent 能够：

- **加入/离开 Office（房间）**，建立 Agent-Computer-Server 三方的会话上下文；
- **远程调用 Computer 侧 MCP 工具**（工具编排在 Computer 内部完成）；
- **获取当前可用工具列表**，用于 Agent 端做工具选择与规划；
- **获取当前桌面布局信息**（基于 MCP window:// 资源聚合的 Desktop 视图）；
- **拉取最新 MCP 配置**，同步 Computer 侧的 Server & inputs 配置。

在 SMCP 协议的整体架构中：

- Server 端：负责 Socket.IO 信令与房间管理（见 `a2c_smcp/server` 模块）；
- Agent 端：通过 Agent Client 与 Server 通信，发起工具调用等请求（见 `a2c_smcp/agent` 模块）；
- Computer 端：本模块提供 `SMCPComputerClient`，把 `Computer` 与 Server 连接，并代理所有与计算相关的能力（MCP 工具、桌面、配置）。

---

## 核心类：`SMCPComputerClient`

### 角色与职责

`SMCPComputerClient` 继承自 `socketio.AsyncClient`，在原生客户端基础上增加了 **SMCP 协议相关行为约束与能力封装**：

- 约束：限制 Computer 侧 **不能** 发送 `notify:*` 和 `client:*` 前缀事件，避免打破 SMCP 事件职责划分；
- 绑定：在构造时绑定一个 `Computer` 实例，并通过 weakref 回写 `computer.socketio_client`；
- 事件注册：监听 SMCP Namespace 下的几类事件：
  - `TOOL_CALL_EVENT` → `on_tool_call`
  - `GET_TOOLS_EVENT` → `on_get_tools`
  - `GET_CONFIG_EVENT` → `on_get_config`
  - `GET_DESKTOP_EVENT` → `on_get_desktop`
- 上报：提供若干便捷方法主动向 Server 上报状态变更：
  - `emit_update_config` / `update_config`
  - `emit_update_tool_list`
  - `emit_refresh_desktop`

### 构造与绑定

```python
client = SMCPComputerClient(computer=computer, **socketio_kwargs)
```

- `computer: Computer` 必填，用于：
  - 在工具调用时，委托给 `computer.aexecute_tool`；
  - 在获取配置时，访问 `computer.mcp_servers` 与 `computer.inputs`；
  - 在获取桌面时，调用 `computer.get_desktop`；
- 构造函数内会：
  - 调用 `super().__init__` 初始化底层 `AsyncClient`；
  - 将自身设置到 `computer.socketio_client`（weakref，避免循环引用）；
  - 在 `SMCP_NAMESPACE` 下注册一系列事件处理函数。

---

## 事件发送约束：`emit`

重写了 `emit` 方法，在调用真正的 `AsyncClient.emit` 之前做前缀校验：

- 禁止：
  - `event.startswith("notify:")` → 抛出异常：Computer 端不允许发送 `notify:*` 事件；
  - `event.startswith("client:")` → 抛出异常：Computer 端不允许主动发起 `client:*` 事件；

这一约束对应 SMCP 协议事件分工：

- `notify:*`：由 Server 端发起，用于通知 Agent / Computer；
- `client:*`：由 Client 角色（Agent / Computer）发起，但按协议 Computer 只允许发起少数定义好的 `client:*`/`server:*` 级别事件；
- `agent:*` / `server:*`：在当前 Computer 侧通常不会直接使用，更多由 Server 端或 Agent 端承担。

在继续扩展时，如果需要新增 Computer 可发送的事件，应优先沿用已有的常量（例如 `UPDATE_CONFIG_EVENT` 等），而不是直接写裸字符串事件名。

---

## Office（房间）管理：`join_office` 与 `leave_office`

在 SMCP 协议中，一个 **Office** 对应 Socket.IO 的一个 Room，用于绑定一组 Agent 与一个 Computer 的会话上下文。Computer 侧的管理逻辑：

### `join_office(office_id: str)`

- 作用：加入指定 Office，并将内部 `self.office_id` 与该房间绑定；
- 流程：
  1. 先设置 `self.office_id = office_id`，避免在服务器广播事件前，本地仍是 `None` 导致断言失败；
  2. 调用 `self.call(JOIN_OFFICE_EVENT, EnterOfficeReq(...))`，同步等待服务器响应；
  3. 按返回结果判断是否加入成功：
     - 若返回 `(success, error_msg)` 且 `success` 为 `False`，重置 `office_id` 并抛异常；
     - 若返回为空或无效，同样视为失败；
  4. 任意异常情况下都会重置 `self.office_id`，保证状态一致性。

### `leave_office(office_id: str)`

- 作用：离开指定 Office，告知 Server 不再参与该房间通信；
- 实现：
  - 发送 `LEAVE_OFFICE_EVENT`，携带 `LeaveOfficeReq(office_id=office_id)`；
  - 无论结果如何，本地都会将 `self.office_id` 置为 `None`。

**注意**：

- 后续所有 Agent 相关事件处理（如工具调用、获取配置等）都会通过断言校验：
  - `self.office_id == data["agent"]`
  - `self.computer.name == data["computer"]`
- 若上下文不匹配，会立即触发 `assert`，用于暴露协议使用错误或潜在的安全问题。

---

## 配置与工具/桌面更新事件

### `emit_update_config` / `update_config`

- 使用场景：
  - 当 Computer 侧 MCP 配置（Server 或 Inputs）发生变化时，需要通知 Server，让 Agent 可以主动刷新；
- 区别：
  - `emit_update_config`：仅在已加入 Office 时发送（`self.office_id` 不为 None）；
  - `update_config`：无条件发送，适用于不关心当前是否已经 join 的场景；
- 负载：
  - 都发送 `UPDATE_CONFIG_EVENT`，携带 `UpdateComputerConfigReq(computer=self.computer.name)`。

### `emit_update_tool_list`

- 使用场景：
  - 当 MCP 工具列表发生变化时（通常由 `MCPServerManager` 通过回调触发），需要通知 Server；
- 逻辑：
  - 仅在 `self.office_id` 不为 None 时发送；
  - 发送 `UPDATE_TOOL_LIST_EVENT` + `UpdateComputerConfigReq(computer=...)`；
  - Server 侧会广播 `notify:update_tool_list` 给所有相关 Agent。

### `emit_refresh_desktop`

- 使用场景：
  - 当 MCP window:// 资源列表或内容发生变化时（例如某个 MCP Server 的 window 资源更新），通知 Server 刷新桌面；
- 逻辑：
  - 同样仅在已加入 Office 时发送；
  - 发送 `UPDATE_DESKTOP_EVENT` + `UpdateComputerConfigReq(computer=...)`；
  - Server 侧会广播 `notify:update_desktop`，Agent 端据此决定是否重新拉取桌面布局。

在 `Computer` 模块中，`_on_manager_change` 会根据 MCP 通知类型（`ToolListChangedNotification` / `ResourceListChangedNotification` / `ResourceUpdatedNotification`）决定在什么时机调用这些方法。

---

## 远程调用工具：`on_tool_call`

### 事件流概览

1. Agent 端通过 Server 发起工具调用请求；
2. Server 在 SMCP Namespace 下向 Computer 侧发送 `TOOL_CALL_EVENT`，负载符合 `ToolCallReq`；
3. `SMCPComputerClient` 收到事件后，执行 `on_tool_call`：
   - 校验房间与 Computer 标识是否一致；
   - 调用 `self.computer.aexecute_tool(...)` 实际触发 MCP 工具；
   - 将返回的 `CallToolResult` 序列化为 JSON 可序列化的 dict，返回给 Server；
4. Server 再将结果返回给 Agent 端。

### 关键实现要点

- 参数断言：
  - `self.office_id == data["agent"]`，确保 Agent 与当前房间一致；
  - `self.computer.name == data["computer"]`，确保请求落在正确的 Computer 上；

- 工具调用委托：
  - `self.computer.aexecute_tool` 内部会：
    - 基于 `MCPServerManager.avalidate_tool_call` 解析工具别名/Server 对应关系；
    - 综合 `ToolMeta.auto_apply` 与可选二次确认回调，决定是否需要人工确认；
    - 最终通过 `MCPServerManager.acall_tool` 调用 MCP Server；
    - 记录调用历史，便于调试与观测。

- 返回值处理：
  - 正常结果：`CallToolResult.model_dump(mode="json")`；
  - 异常兜底：捕获所有异常，构造 `CallToolResult(isError=True, structuredContent={"error": ..., "error_type": ...}, content=[])` 再序列化。

**扩展建议**：

- 若需要对工具调用做额外审计或打点，优先考虑在 `Computer.aexecute_tool` 中扩展，而不是在 `on_tool_call` 里直接嵌入业务逻辑，以保持 Socket.IO 客户端的“协议适配”职责纯粹。

---

## 获取工具列表：`on_get_tools`

### 事件流概览

1. Agent 端通过 Server 请求当前可用工具列表（例如进入一个新的对话或刷新工具栏时）；
2. Server 向 Computer 发送 `GET_TOOLS_EVENT`，负载符合 `GetToolsReq`；
3. `SMCPComputerClient` 执行 `on_get_tools`：
   - 校验房间与 Computer 标识；
   - 调用 `self.computer.aget_available_tools()` 获取 `SMCPTool` 列表；
   - 返回 `GetToolsRet(tools=..., req_id=...)`；
4. Server 将结果转发给 Agent。

### 关键点

- `aget_available_tools` 内部会：
  - 向 `MCPServerManager.available_tools` 拉取 MCP 标准的 `Tool` 列表；
  - 注入 `ToolMeta` / VRL 等 meta 信息；
  - 转为 SMCP 协议中的 `SMCPTool` 结构。

从使用方角度，Agent 端拿到的就是一份已经包含 MCP  + A2C 扩展元信息的工具定义列表，可以直接用于 UI 渲染与策略规划。

---

## 获取桌面布局：`on_get_desktop`

### 事件流概览

1. Agent 端希望查看当前 Computer 的“桌面”（由 MCP window 资源组织而成），向 Server 发起请求；
2. Server 下发 `GET_DESKTOP_EVENT` 给 Computer，负载符合 `GetDeskTopReq`；
3. `SMCPComputerClient` 执行 `on_get_desktop`：
   - 校验房间与 Computer 标识；
   - 从请求中取出 `desktop_size` 与 `window`（单窗口过滤）；
   - 调用 `self.computer.get_desktop(size=size, window_uri=window_uri)`；
   - 返回 `GetDeskTopRet(desktops=..., req_id=...)`。

### 与 MCP Clients 的协作

- `Computer.get_desktop` 会依托 `MCPServerManager.list_windows / get_windows_details` 能力：
  - 从所有活动 MCP Server 中收集 window:// 资源；
  - 读取资源详情，并通过 `organize_desktop` 聚合成桌面视图；
  - 最终封装为 `Desktop` 结构返回给 Agent。

通过这种分层设计，Socket.IO 客户端只负责搬运与校验请求/响应，不直接关心 window 资源的来源与组织方式。

---

## 获取配置：`on_get_config`

### 事件流概览

1. Agent 端需要同步当前 Computer 的 MCP 配置（服务器与 inputs），例如：
   - 首次连接；
   - 收到 `notify:update_config` 通知后主动刷新；
2. Server 向 Computer 下发 `GET_CONFIG_EVENT`，负载符合 `GetComputerConfigReq`；
3. `SMCPComputerClient` 执行 `on_get_config`：
   - 校验房间与 Computer 标识；
   - 遍历 `self.computer.mcp_servers`：
     - 调用 `cfg.model_dump(mode="json")`；
     - 使用 `TypeAdapter(SMCPServerConfigDict).validate_python(...)` 转换并校验为 SMCP 协议定义结构；
     - 按 `cfg.name` 作为 key 写入 `servers` 字典；
   - 遍历 `self.computer.inputs`：
     - 序列化为 dict；
     - 使用 `TypeAdapter(MCPServerInput).validate_python(...)` 严格校验；
   - 最终使用 `TypeAdapter(GetComputerConfigRet).validate_python(...)` 构造返回对象。

### 设计要点

- **强校验**：
  - 所有转换都通过 Pydantic 的 `TypeAdapter` 严格校验，这保证：
    - 一旦 Server/Agent 端的协议结构变更不兼容，本地会尽早暴露错误；
    - 避免“默默丢字段”或“字段类型悄悄变更”这类隐性问题。

- **只读视图**：
  - `Computer.mcp_servers` 与 `Computer.inputs` 都以不可变视图（tuple）对外暴露，确保配置在运行时不会被 Socket.IO 客户端直接修改。

在后续扩展配置结构时（例如为 ServerConfig 增加新字段），记得同步更新 SMCP 协议模型与测试，保证两端一致。

---

## 与其他模块的关系与扩展建议

### 与 `a2c_smcp/server` 模块

- Server 模块实现了 SMCP Namespace 的事件处理逻辑：
  - 负责 `JOIN_OFFICE_EVENT` / `LEAVE_OFFICE_EVENT` 等房间管理；
  - 负责在 `client:update_config` / `client:update_tool_list` / `client:update_desktop` 之后广播对应的 `notify:*` 事件；
  - 负责在 Agent 与 Computer 之间转发工具调用、工具列表、桌面与配置请求/响应。

`SMCPComputerClient` 是上述逻辑在 Computer 端的“镜像”，两者通过共享的事件名与数据结构解耦。

### 与 `a2c_smcp/agent` 模块

- Agent 模块有自己的 `SMCPAgentClient`，扮演与 `SMCPComputerClient` 对称的角色：
  - 发起工具调用；
  - 拉取工具列表、桌面与配置；
- Computer 与 Agent 不直接通信，而是都只面对 Server，通过 Room 实现“一 Agent + 一 Computer”的会话绑定。

### 与 `a2c_smcp/computer/mcp_clients` 模块

- `SMCPComputerClient` 不直接调用 MCP Server，而是完全委托给 `Computer`：
  - 工具调用 → `Computer.aexecute_tool`（内部用 `MCPServerManager` 管理与调用）；
  - 工具列表 → `Computer.aget_available_tools`；
  - 桌面 → `Computer.get_desktop`；
  - 配置 → `Computer.mcp_servers` / `Computer.inputs`；

这样可以保持：

- Socket.IO 层只关心“如何与 Server 说话”；
- MCP 层只关心“如何与 MCP Servers 说话”；
- `Computer` 负责在两者之间做业务整合与策略控制（例如二次确认、工具元信息、VRL 转换等）。

---

## 开发与扩展建议

在继续开发 Computer Socket.IO 客户端相关能力时，可以参考以下原则：

- **边界清晰**：尽量把“协议适配”（事件名、数据包结构校验）与“业务逻辑”（工具选择、二次确认、VRL 转换等）分开，前者放在 Socket.IO 客户端，后者放在 `Computer` 或更上层模块。
- **强类型与双向校验**：所有跨进程/跨网络的数据，优先通过 Pydantic 模型或 `TypeAdapter` 做双向校验，保证协议演进可控。
- **房间上下文一致性**：新事件若依赖 Agent/Computer 身份，务必参照现有实现，增加 `office_id` 与 `computer` 字段校验，避免“串线”。
- **事件命名遵循 SMCP 约定**：新增事件时尽量使用统一前缀（例如 `client:...` / `notify:...` / `server:...` / `agent:...`），并在 Server 端与 Agent 端同步更新常量与文档。

通过保持上述分层和约束，可以让 Socket.IO 通信层在协议不断扩展的情况下，依然保持清晰、稳定、易于调试。

