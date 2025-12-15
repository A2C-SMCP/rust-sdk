## MCP Clients 模块概览

本模块负责在 IDE4AI Computer 侧统一管理与 MCP Server 的连接与调用能力，包括：

- **连接封装**：基于 MCP Python SDK，对 STDIO / SSE / Streamable HTTP 三种连接方式做统一封装。
- **服务器配置模型**：通过 `MCPServerConfig` 与 `MCPServerInput` 等模型描述每个 MCP Server 的连接参数、禁用工具、元信息等。
- **客户端状态管理**：使用异步状态机管理每个 MCP Client 的生命周期（初始化、连接、断开、错误）。
- **工具与窗口聚合**：在 `MCPServerManager` 内聚合所有 MCP Server 的工具列表和 window:// 资源，并对外提供统一查询与调用接口。
- **VRL 结果转换**：支持在配置中为 MCP Server 定义 VRL 脚本，对工具返回值进行二次转换，并通过 meta 透传给上层 UI。

后续工程师在继续开发该模块时，主要会涉及以下几类工作：

- 新增一种 MCP 连接方式（例如 WebSocket）、
- 扩展 MCP Server 配置（增加字段或新的 Server 类型）、
- 在 `MCPServerManager` 中增加新的聚合/查询能力、
- 调整与 `Computer` 主模块的集成方式。

下面按文件说明核心职责与扩展方式。

---

## 配置模型：`model.py`

### MCPServerConfig 族

`model.py` 定义了 MCP Server 端的配置模型，是整个模块的“配置中心”：

- **`BaseMCPServerConfig`**：
  - 字段：
    - `name`: MCP Server 名称，作为唯一标识；
    - `disabled`: 是否禁用；
    - `forbidden_tools`: 禁用的工具列表；
    - `tool_meta`: 每个工具的元信息配置（`ToolMeta`）；
    - `default_tool_meta`: 默认的工具元信息，按需与具体工具的 `tool_meta` 进行浅合并；
    - `vrl`: 可选 VRL 脚本，用于对工具返回值进行统一转换；
  - 特性：
    - `frozen=True`，配置一旦初始化后不可修改，方便在运行期安全复用；
    - 自定义 `__hash__`，以 `name` 作为 set/dict 的去重与索引键。

- **具体配置类型**：
  - `StdioServerConfig`：`type="stdio"`，包含 `server_parameters: StdioServerParameters`；
  - `SseServerConfig`：`type="sse"`，包含 `server_parameters: SseServerParameters`；
  - `StreamableHttpServerConfig`：`type="streamable"`，包含 `server_parameters: StreamableHttpParameters`；
  - 这三种通过 `MCPServerConfig` TypeAlias 统一起来，方便 `Computer` 与 `MCPServerManager` 做泛型处理。

**VRL 语法校验**：

- `vrl` 字段带有 `field_validator("vrl")`：
  - 初始化时若配置了脚本，则通过 `VRLRuntime.check_syntax` 做语法校验；
  - 校验失败会抛出 `ValueError`，保证运行期不会因为脚本错误导致崩溃；
  - 这意味着：**工程师在变更 VRL 脚本时，一定要跑一次单测/集成测，避免语法错误直接进配置仓库**。

### 工具元信息：`ToolMeta`

- 作用：为 MCP 工具补充 IDE 侧渲染与交互所需的附加信息，例如：
  - `auto_apply`: 是否允许自动执行（绕过二次确认）；
  - `alias`: 工具别名，用于解决不同 Server 之间的同名工具冲突；
  - `tags`: 标签，方便在前端进行分组和筛选；
  - `ret_object_mapper`: 字段映射表，用于对返回结果结构做简单映射（适合轻量、无逻辑的转换）。
- 所有这些配置最终都会在 `MCPServerManager.acall_tool / available_tools` 中被合并并注入到 MCP 标准的 `meta` 字段中，供上层 UI 使用。

### 输入定义：`MCPServerInput*`

`MCPServerInput` 系列用于定义 MCP Server 配置中的“动态变量输入”，与 `Computer.inputs` 和 inputs 子系统集成：

- `MCPServerPromptStringInput`：用户输入自由文本，可选 `default` 和 `password`；
- `MCPServerPickStringInput`：用户从枚举列表中选择，支持 `options` 和 `default`；
- `MCPServerCommandInput`：通过执行命令获取值（例如调用 IDE 内部命令或脚本）；
- 通过 `id` 作为全局唯一标识，在 `Computer` 中会交给 `InputResolver` 按需解析（惰性解析）；
- 工程师在新增/修改 MCP Server 配置时，应优先复用这些输入定义，保持交互模式一致。

### 客户端协议抽象：`MCPClientProtocol`

- 这是一个 `Protocol`，抽象约定了 Manager 与具体 Client 之间的交互接口：
  - `state`: 使用 `STATES` 枚举表示当前状态；
  - `aconnect` / `adisconnect`: 管理连接生命周期；
  - `list_tools`：列出工具列表；
  - `call_tool`：调用指定工具；
  - `list_windows` / `get_window_detail`：window:// 资源的查询与详情读取。
- 所有具体的 Client（STDIO / SSE / HTTP）都需要实现这个协议（通常是继承自 `BaseMCPClient` 并补齐部分方法）。

---

## 客户端基类与状态机：`base_client.py`

`BaseMCPClient` 是所有具体 MCP 客户端的抽象基类，负责：

- 统一维护一个 MCP `ClientSession` 实例；
- 管理异步上下文与生命周期（通过 `AsyncExitStack` 与自定义 `A2CAsyncMachine`）；
- 通过状态机 `STATES` + `TRANSITIONS` 控制连接/断开/错误流转；
- 对外暴露常用的异步方法（`aconnect`、`adisconnect`、`async_session` 等），供 `MCPServerManager` 使用。

### 状态机设计

- `STATES`: `initialized` / `connected` / `disconnected` / `error`；
- `TRANSITIONS`: 基于 `transitions` 库的异步扩展，定义了：
  - `aconnect`: initialized → connected；
  - `adisconnect`: connected → disconnected；
  - `aerror`: 任意 → error；
  - `ainitialize`: 任意 → initialized；
- 每个触发器都可以挂载 `prepare` / `conditions` / `before` / `after` 钩子，`BaseMCPClient` 默认实现了核心逻辑并通过 `_state_change_callback` 对外通知。

### 会话与保活

- `_create_async_session`: 抽象方法，由子类实现，负责：
  - 构建具体的 IO 连接（STDIO/SSE/HTTP）；
  - 通过 `AsyncExitStack` 压栈上下文，保证关闭时资源正确释放；
- `_keep_alive_task`: 在单独的 asyncio 任务中运行，会：
  - 创建 `ClientSession` 并设置 `_create_session_success_event` / `_failure_event`；
  - 通过永远阻塞的 `Event().wait()` 保持上下文存活，直到任务被 cancel；
  - 在 `finally` 中关闭 `AsyncExitStack` 并清理 `_async_session` 与 `_initialize_result`。

**注意（扩展建议）**：

- 若新增一种 Client 实现（例如 WebSocket）：
  - 必须继承 `BaseMCPClient`，实现 `_create_async_session`，并确保所有上下文都压入 `_aexit_stack`；
  - 遵守 `MCPClientProtocol` 的接口约束，特别是 `list_tools` / `call_tool` / `list_windows` / `get_window_detail`。

---

## 具体客户端实现：STDIO / SSE / HTTP

这三类客户端都非常薄，只负责把连接参数与 `ClientSession` 正确拼装起来：

- **`StdioMCPClient` (`stdio_client.py`)**：
  - 使用 `stdio_client(self.params)` 建立子进程标准输入输出流；
  - 将 `stdout` / `stdin` 交给 `ClientSession`；
  - 支持可选 `message_handler`，用于处理 Server 端通知（例如工具/资源变更）。

- **`SseMCPClient` (`sse_client.py`)**：
  - 使用 `sse_client(**self.params.model_dump(mode="python"))` 建立 SSE 连接；
  - `params` 类型为 `SseServerParameters`；

- **`HttpMCPClient` (`http_client.py`)**：
  - 使用 `streamablehttp_client(**self.params.model_dump(mode="python"))` 创建双向流；
  - 特别注意：必须使用 `mode="python"`，避免例如 `timedelta` 被错误序列化为字符串；
  - 同样将流封装进 `ClientSession` 并支持 `message_handler`。

所有这些具体 Client 都通过 `client_factory` 统一创建。

---

## 客户端工厂：`utils.py`

`client_factory(config: MCPServerConfig, message_handler: MessageHandlerFnT | None)` 是整个模块的实例化入口：

- 根据 `config` 的具体类型选择不同 Client：
  - `StdioServerConfig` → `StdioMCPClient`；
  - `SseServerConfig` → `SseMCPClient`；
  - `StreamableHttpServerConfig` → `HttpMCPClient`；
- 工程师在新增 Server 类型时，需要：
  - 在 `model.py` 中新增对应的 `*ServerConfig`；
  - 在 `base_client.py` 的基础上实现具体 `XXXMCPClient`；
  - 最后在 `utils.client_factory` 中增加一个 `match case` 分支。

---

## 管理器：`MCPServerManager` 的职责与数据流

`MCPServerManager` 是 `mcp_clients` 模块的核心，对上层（`Computer` 与 CLI）暴露统一的管理接口，对下层维护每个 MCP Client 的生命周期。

### 内部状态

- `_servers_config`: `{server_name -> MCPServerConfig}`，当前所有配置；
- `_active_clients`: `{server_name -> MCPClientProtocol}`，已启动的客户端；
- `_tool_mapping`: `{tool_name -> server_name}`，工具到服务器的映射；
- `_alias_mapping`: `{alias -> (server_name, original_tool_name)}`，别名映射；
- `_disabled_tools`: 被标记禁用的工具集合；
- `_auto_connect` / `_auto_reconnect`: 是否自动启动/重启；
- `_message_handler`: 透传给具体 MCP Client，用于处理 MCP Server 推送的通知；
- `_lock`: `asyncio.Lock`，保护上述状态在并发场景下的一致性。

### 生命周期管理

- `ainitialize(servers)`：
  - 关闭并清理全部旧客户端与配置；
  - 按传入的 `servers` 重建 `_servers_config`；
  - 根据 `_auto_connect` 决定是否为每个启用的 Server 启动 Client；
  - 最后根据所有活动 Client 的工具列表刷新 `_tool_mapping` 与 `_alias_mapping`。

- `aadd_or_aupdate_server(config)` / `aremove_server(name)`：
  - 支持在运行期增减 Server；
  - 内部会重建工具映射，若发生工具重名冲突会抛出 `ToolNameDuplicatedError` 并回滚配置；
  - 更新时若 Server 正在运行且 `auto_reconnect=True`，会先停止再基于新配置重启。

- `astart_all` / `astart_client` / `astop_client` / `aclose`：
  - 分别用于批量/单个启动与停止；
  - 内部始终走 `_astart_client` / `_astop_client`，保持一致的错误处理与映射刷新逻辑。

### 工具调用与 VRL 集成

- `acall_tool(server_name, tool_name, parameters, timeout)`：
  - 从 `_active_clients` 中取出对应 Client，调用其 `call_tool`；
  - 基于 `MCPServerConfig.tool_meta` 与 `default_tool_meta` 合并出 `ToolMeta`，并注入到返回的 `CallToolResult.meta[A2C_TOOL_META]`；
  - 若当前 `config.vrl` 不为空：
    - 将 `CallToolResult` 序列化为 dict，并附加 `tool_name`、`parameters`；
    - 通过 `VRLRuntime.run` 执行脚本，使用系统时区（通过 `tzlocal` 获取，失败回退 UTC）；
    - 将 `processed_event` 压缩成 JSON 字符串，放入 `result.meta[A2C_VRL_TRANSFORMED]`；
    - 若脚本执行失败，仅记录 warning，不影响原始结果返回。

- `aexecute_tool(tool_name, parameters, timeout)`：
  - 在不指定 Server 的情况下调用工具，支持使用 `alias`；
  - 通过 `avalidate_tool_call` 解析 alias，并完成各种校验（工具是否存在、是否被禁用等）；
  - 内部最终仍然委托给 `acall_tool` 执行。

### 工具与窗口聚合

- `available_tools()`：
  - 遍历 `_tool_mapping` 与 `_active_clients`，按 Server 缓存工具列表；
  - 为每个 `Tool` 注入合并后的 `ToolMeta` 至其 `meta[A2C_TOOL_META]`；
  - 以异步生成器返回所有可用工具，供 `Computer.aget_available_tools` 消费并转换为 `SMCPTool`。

- `list_windows(window_uri: str | None)` 与 `get_windows_details(window_uri: str | None)`：
  - 按当前所有活跃 Client 调用其 `list_windows` / `get_window_detail`；
  - 返回带有 `server_name` 的列表，方便上层做聚合与过滤（例如桌面布局）。

### 开发注意事项

- 在 Manager 中增加新能力时，应尽量：
  - 不直接暴露底层 Client 的细节，而是通过 `MCPClientProtocol` 做抽象；
  - 保持所有对 `_servers_config` / `_active_clients` 等可变状态的操作都在 `async with self._lock` 内完成；
  - 对可能较慢的 I/O 操作（例如遍历所有窗口获取详情）尽量先复制快照，再在锁外执行，避免长时间持锁。

---

## 与 Computer 模块的集成关系

`a2c_smcp/computer/computer.py` 中的 `Computer` 类是 IDE4AI 的“本地电脑大脑”，`mcp_clients` 模块则是它与 MCP Server 的桥梁。主要集成点：

- 在 `Computer.__init__` 中：
  - 通过 `_inputs: set[MCPServerInput]` 与 `_mcp_servers: set[MCPServerConfig]` 持有所有配置；
  - 输入解析使用 `InputResolver`，与 `MCPServerInput*` 保持一一对应；

- 在 `boot_up` 流程中：
  - 构造 `MCPServerManager` 实例，并设置 `auto_connect` / `auto_reconnect` 与 `_on_manager_change` 回调；
  - 对每个 `MCPServerConfig` 执行：
    - `model_dump(mode="json")` → 利用 `ConfigRender` 解析其中的动态变量占位符 → `model_validate` 重建不可变对象；
  - 最终调用 `mcp_manager.ainitialize(validated_servers)` 完成所有 Server 的整体初始化。

- 工具相关：
  - `aget_available_tools`：调用 `mcp_manager.available_tools()`，并将 MCP `Tool` 转换为 SMCP 协议中的 `SMCPTool`；
  - `aexecute_tool`：
    - 使用 `mcp_manager.avalidate_tool_call` 解析 alias / 校验工具存在性；
    - 再结合 `ToolMeta.auto_apply` 与可选的二次确认回调，决定是否直接调用 `mcp_manager.acall_tool`；
    - 同时记录调用历史（`ToolCallRecord`），用于后续调试与展示。

- window:// 相关：
  - `get_desktop` 等方法基于 `mcp_manager.list_windows` / `get_windows_details` 的能力，构建桌面布局。

从工程角度看：

- `Computer` 更偏“业务协调层”，不直接关注 MCP 连接细节；
- `mcp_clients` 更偏“基础设施层”，专注于连接管理与协议聚合；
- 两者之间通过：配置模型 + Manager API 解耦，便于独立演进。

---

## 典型调用流程梳理

以 IDE 启动一个带 MCP 能力的 `Computer` 为例，整体流程大致如下：

1. **构造配置**：
   - 通过配置文件或代码创建若干 `MCPServerConfig` 实例（可以含 VRL、tool_meta 等）；
   - 同时创建一组 `MCPServerInput`，定义可能用到的动态变量；

2. **创建 Computer 实例**：
   - `computer = Computer(name, inputs=..., mcp_servers=..., auto_connect=True, auto_reconnect=True, ...)`；

3. **启动**：
   - 调用 `await computer.boot_up()` 或直接通过 `async with Computer(...)`；
   - 内部会：解析配置 → 重建 `MCPServerConfig` → 调用 `MCPServerManager.ainitialize` → 启动所有 Client；

4. **列出工具**：
   - `tools = await computer.aget_available_tools()`；
   - 这里每个工具都带有注入后的 `meta`（包括 `A2C_TOOL_META`）；

5. **调用工具**：
   - `result = await computer.aexecute_tool(req_id, tool_name_or_alias, params, timeout=...)`；
   - 内部会根据 `ToolMeta.auto_apply` 决定是否触发二次确认；
   - 最终调用 `MCPServerManager.acall_tool`，应用 VRL，返回 `CallToolResult`；

6. **桌面聚合**（可选）：
   - `desktop = await computer.get_desktop(...)`；
   - 底层通过 `MCPServerManager` 拉取所有 window:// 资源并聚合为 `Desktop` 视图。

---

## 常见扩展场景与建议

### 1. 新增一种 MCP 连接方式

如果需要支持新的传输方式（例如 WebSocket）：

- 在 `model.py` 中：
  - 新增 `WebsocketServerConfig(BaseMCPServerConfig)`，包含对应的参数模型；
  - 将其并入 `MCPServerConfig` TypeAlias；

- 在 `base_client.py` 基础上：
  - 新增 `WebsocketMCPClient(BaseMCPClient[WebsocketServerParameters])`；
  - 实现 `_create_async_session`，确保所有上下文都使用 `_aexit_stack` 管理；

- 在 `utils.client_factory` 中为 `WebsocketServerConfig` 增加一个 `match case`；

- 根据需要补充单元测试与集成测试，确保：
  - 状态机迁移正常（初始化 → 连接 → 断开）；
  - 工具列表与调用链路与现有三个实现保持一致。

### 2. 扩展 ToolMeta 或 VRL 能力

- 新增字段：
  - 在 `ToolMeta` 中增加对应字段；
  - 在 `MCPServerManager._merged_tool_meta` 的使用方（例如前端、`Computer`）根据需要消费这些字段；

- VRL 相关：
  - 避免把复杂业务逻辑全部塞进 VRL，更适合用来做“结构标准化”和“字段重命名”；
  - 若 VRL 需要依赖上下文信息，可以在 `acall_tool` 中通过 `event[...] = ...` 注入额外字段；
  - 注意 meta 中只存储 JSON 字符串，消费方需要主动 `json.loads`。

### 3. 在 Manager 中新增聚合查询

- 例如：希望提供“按 Server 维度统计工具数量”的接口：
  - 建议新增一个只读方法，例如 `get_tool_stats()`；
  - 实现时复用当前 `_tool_mapping` 与 `_servers_config`，避免重新遍历所有 Client；
  - 确保不会修改内部状态，也不持有锁过长时间。

---

## 总结

`a2c_smcp/computer/mcp_clients` 模块是 Computer 侧所有 MCP 能力的基础设施抽象：

- 通过 `MCPServerConfig` 与 `MCPServerInput` 描述“要连谁、怎么连、有哪些动态变量”；
- 通过 `BaseMCPClient` + 多种具体 Client 适配不同传输方式；
- 通过 `MCPServerManager` 聚合多 Server 的工具与 window 资源，并统一管理生命周期；
- 通过 VRL 与 ToolMeta 为上层 UI 提供足够灵活的展示与交互能力。

在继续开发时，可以优先思考：

- 需求是新增“连接方式”？“配置字段”？“工具/窗口聚合的查询维度”？
- 哪一层最适合承载这个变化：配置模型、Client 实现、Manager 逻辑，还是 `Computer` 的业务协调层？

按上述分层设计进行扩展，可以尽量减少对现有代码的影响，并保持良好的可测试性与可维护性。

