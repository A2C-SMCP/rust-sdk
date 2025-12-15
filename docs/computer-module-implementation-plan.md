---
description: Rust Computer 模块实施方案与开发规划
---

# Rust Computer 模块实施方案与开发规划

## 0. 文档目的与范围

本文件用于指导工程师在 `crates/smcp-computer` 中实现 A2C-SMCP 的 **Computer 模块**。

- 目标是 **复刻 Python SDK 的能力与语义**，并在 Rust 侧重点保障进程生命周期稳定性、可观测性与可测试性。

本方案以仓库内 Python 参考实现与协议行为为准，避免随意改动协议或臆造语义。

## 1. 权威参考（必须对齐）

### 1.1 Python 参考实现

- `examples/python/a2c_smcp/computer/computer.py`
  - Computer 主体：MCP Server 生命周期、工具聚合、工具调用（含二次确认）、Desktop(window://) 聚合、inputs 管理。

- `examples/python/a2c_smcp/computer/mcp_clients/manager.py`
  - 多 MCP 管理器：tool mapping、alias、forbidden_tools、冲突报错策略、auto_connect/auto_reconnect。

- `examples/python/docs/computer/inputs/inputs.md`
  - Inputs 子系统设计：`BaseInputResolver`/`InputResolver`、`ConfigRender` 的占位符与惰性解析语义。

- `examples/python/a2c_smcp/computer/cli/main.py`
  - CLI 入口：run 模式 + interactive loop。

### 1.2 Rust 现有协议与转发行为

- `crates/smcp-server-core/src/handler.rs`
  - Server 侧对 `CLIENT_TOOL_CALL/CLIENT_GET_TOOLS/CLIENT_GET_DESKTOP/CLIENT_GET_CONFIG` 使用 ACK 转发到目标 Computer。

## 2. 设计原则（实现时必须遵守）

- **协议语义对齐 Python**
  - 行为、错误边界、字段形状尽量与 Python JSON 表达一致。

- **强生命周期安全**
  - stdio 子进程必须可控可回收，避免进程逃逸、僵尸进程、后台任务泄漏。

- **可替换的交互后端**
  - inputs 的“交互”与核心逻辑解耦，未来可替换为 GUI/Tauri。

- **强可测试性**
  - REPL 必须可用 PTY/expect 风格进行稳定 e2e 测试。

## 3. crate 结构规划（建议目录）

> 以下是推荐拆分，便于分层测试与替换实现。

建议在 `crates/smcp-computer/src` 下按如下拆分：

- `core/`
  - `computer.rs`: `ComputerCore`（对齐 Python `Computer` 主职责）
  - `events.rs`: tool/desktop/config 变更事件（供 transport 订阅上报）
  - `types.rs`: `ToolCallRecord` 等（最近 N 条历史）

- `manager/`
  - `manager.rs`: `McpServerManager`（对齐 Python `MCPServerManager`）
  - `errors.rs`: `ToolNameDuplicatedError` 等

- `mcp_clients/`
  - `mod.rs`: `McpClient` trait + `client_factory`
  - `stdio.rs`: `StdioMcpClient`
  - `sse.rs`: `SseMcpClient`
  - `streamable_http.rs`: `StreamableHttpMcpClient`

- `inputs/`
  - `model.rs`: `McpServerInput` / `McpServerConfig` / `ToolMeta`（serde 模型，字段对齐 Python）
  - `resolver.rs`: resolver trait + cache
  - `render.rs`: `ConfigRender`（`${input:<id>}` 递归渲染，惰性解析）
  - `cli_resolver.rs`: CLI 交互式 inputs 解析实现

- `transport/`
  - `socketio_client.rs`: `SmcpComputerClient`（连接 SMCP_NAMESPACE，响应 ACK 请求，上报 tool list/desktop 更新）

- `cli/`
  - `app.rs`: CLI 命令入口（run / connect / ...）
  - `repl.rs`: REPL 主循环（pyexpect 级交互要求）
  - `commands/`: servers/tools/inputs/desktop 等子命令实现

## 4. 核心接口与职责（建议签名级别的约定）

### 4.1 `McpClient` trait（对齐 Python `MCPClientProtocol`）

所有传输（stdio/sse/http）必须实现统一接口，供 `McpServerManager` 聚合。

- `connect()` / `disconnect()`
- `list_tools()` / `call_tool(tool, params)`
- `list_windows()` / `read_window(resource)`
- `state()`

### 4.2 `McpServerManager`（工具聚合与冲突处理核心）

对齐 Python `manager.py` 的关键语义：

- 状态：
  - servers_config / active_clients
  - tool_mapping
  - alias_mapping
  - disabled_tools
  - auto_connect / auto_reconnect

- 行为：
  - `initialize(servers)`：清理旧连接->重建->刷新 tool mapping
  - `add_or_update_server(cfg)`：支持热更新；auto_reconnect 时可自动重启
  - `remove_server(name)`：停止并移除
  - `refresh_tool_mapping()`：
    - 若同名工具出现于多个 server：**直接报错**（`ToolNameDuplicatedError`）
    - 错误信息必须包含建议：使用 `ToolMeta.alias` 解决冲突
  - `validate_tool_call(tool_name, params)`：
    - disabled_tools 检查
    - alias -> original tool 解析

- ToolMeta 合并：
  - 浅合并 default_tool_meta 与 specific tool_meta（specific 覆盖 default；不使用 None 覆盖已有字段）

### 4.3 `ComputerCore`（对齐 Python `Computer`）

`ComputerCore` 聚合 inputs+manager，并与 transport 通过事件/回调解耦。

- 生命周期：`boot_up()` / `shutdown()`（支持 async context）
- 工具：
  - `get_available_tools()`：MCP Tool -> SMCPTool（meta 序列化规则对齐 Python）
  - `execute_tool(req_id, tool_name, params, timeout)`：
    - `manager.validate_tool_call`
    - 合并 ToolMeta
    - auto_apply/confirm_callback（如需要）
    - 写 ToolCallRecord（最近 N 条）
- Desktop：聚合 window:// resources（对齐 Python 的 window_uri 策略）
- Inputs：定义与当前值缓存 CRUD（对齐 Python）
- 动态 server 更新：
  - `add_or_update_server(cfg_raw)`：内部 `ConfigRender` -> `InputResolver` 惰性解析 -> validate -> manager

## 5. stdio（子进程）治理方案（最高风险、最高优先级）

### 5.1 风险清单

进程逃逸、僵尸进程、stderr/stdout 泄漏任务、未回收的 inflight tool call。

### 5.2 进程组与关闭策略（推荐实现要求）

stdio 必须具备“不可绕过”的关闭流程。

关闭顺序（建议）：

1. 标记 closing，拒绝新请求
2. 取消所有 inflight（CancellationToken）
3. 关闭 stdin（提示对端退出）
4. 向进程组发送 SIGTERM，等待 grace（例如 2s）
5. 超时则 SIGKILL 进程组
6. `wait()` 回收子进程
7. await stdout/stderr pump tasks（确保无后台任务泄漏）

### 5.3 超时与取消（必须一次到位）

`call_tool(..., timeout)` 使用 `tokio::select!`：

- result 完成
- timeout
- cancel token

- cancel 来源：
  - 来自 Server 的 `SERVER_TOOL_CALL_CANCEL` / `NOTIFY_TOOL_CALL_CANCEL`。

### 5.4 可测试性要求

必须有一个“假 MCP stdio server”用于集成测试，验证 start/stop 后无残留进程与任务。

## 6. Inputs 子系统（可迁移到 Tauri）

### 6.1 数据结构（对齐 Python `model.py`）

Inputs 子系统设计：`BaseInputResolver`/`InputResolver`、`ConfigRender` 的占位符与惰性解析语义。

- `McpServerInput`：
  - `promptString`（支持 password、default）
  - `pickString`（options、default）
  - `command`（command、args）

- `ToolMeta`：
  - `auto_apply`
  - `alias`
  - `tags`
  - `ret_object_mapper`
  - extra allow（使用 `#[serde(flatten)]` 处理动态字段）

- `MCPServerConfig` 基类：
  - `name`: 服务器名称（唯一标识）
  - `disabled`: 是否禁用
  - `forbidden_tools`: 禁用工具列表
  - `tool_meta`: 按工具名的元数据映射
  - `default_tool_meta`: 默认元数据
  - `vrl`: VRL脚本（可选，用于返回值转换）

### 6.2 `ConfigRender` 规则（必须对齐 Python）

Inputs 子系统设计：`BaseInputResolver`/`InputResolver`、`ConfigRender` 的占位符与惰性解析语义。

- 占位符：`${input:<id>}`
- 递归渲染：dict/list/str
- 特殊规则：
  - 若字符串“只包含一个占位符且无其它字符”：返回 resolver 的原始值类型（允许 object/number/bool）
  - 否则：将 resolver 值 stringify 并替换到字符串中

### 6.3 Resolver 分层（建议）

- `CliInputResolver`：REPL 下交互输入
- `EnvInputResolver`：从环境变量读取（无交互）
- `CompositeResolver`：env -> cache -> cli

这样未来接入 Tauri 只需实现新的 resolver。

## 7. 工具重名冲突策略（必须与 Python 一致）

- 同名工具出现于多个 MCP Server：直接报错，要求用户配置 alias。

必须实现：

- alias mapping：`alias -> (server, original_tool)`
- forbidden_tools：同时匹配 alias 与 original 名称
- default_tool_meta + specific tool_meta 浅合并

## 8. CLI（REPL）与“pyexpect 级”交互/测试

### 8.1 交互要求

REPL 必须做到后台事件输出不破坏输入体验；同时输出必须可被 expect 稳定断言。

推荐输出规范：

- 固定 prompt：`a2c> `
- 所有可测试输出额外输出 JSON Lines（推荐固定前缀）：
  - `@a2c {"type":"server_status",...}`
  - `@a2c {"type":"tool_list_changed",...}`
  - `@a2c {"type":"tool_call_result",...}`

测试只断言 `@a2c` 行，避免颜色/表格导致不稳定。

### 8.2 REPL 命令集（建议最小可用集合）

- `connect --url ... --office ... --name ...`
- `servers list/start/stop/restart/add/remove`
- `tools list/call/conflicts`
- `inputs load/list/value set/value clear/value list`
- `desktop list/show <window_uri>`
- `quit` / `exit`

### 8.3 e2e 测试（PTY/expect）

必须使用 PTY 启动 CLI，否则 readline 类库在 pipe 下行为不稳定。

建议依赖：`portable-pty` 或 `expectrl`。

最小 e2e 用例：

1. spawn `smcp-computer run ...` -> 等待 `a2c> `
2. `servers list` -> expect `@a2c {"type":"server_status"...}`
3. `inputs ...` -> expect `@a2c {"type":"inputs_loaded"...}`
4. `tools call ...` -> expect `@a2c {"type":"tool_call_result"...}`
5. `quit` -> 进程退出
6. 验证无残留 stdio 子进程（可在测试中通过内部计数/句柄回收证明）

## 9. VRL 集成方案（Vector Remap Language）

### 9.1 VRL 支持范围

中文：VRL 用于对 MCP 工具返回值进行动态转换和格式化。

- 依赖：`vrl` crate（Vector 的开源实现，纯 Rust）
- 特性：作为可选 feature，默认不启用以减少依赖
- 验证：配置时进行语法检查，运行时动态编译

### 9.2 集成点设计

- 配置层：`MCPServerConfig.vrl: Option<String>`
- 执行层：`McpServerManager.call_tool()` 内部
- 存储层：转换结果存入 `CallToolResult.meta["a2c_vrl_transformed"]`

```rust
// 伪代码示例
pub async fn call_tool(&self, tool_name: &str, params: Value) -> Result<CallToolResult> {
    let result = client.call_tool(tool_name, params).await?;
    
    // VRL 转换（如果配置了）
    if let Some(vrl_script) = &config.vrl {
        if let Ok(transformed) = execute_vrl(vrl_script, &result, tool_name, params) {
            result.meta.insert("a2c_vrl_transformed".to_string(), 
                               serde_json::to_string(&transformed)?);
        }
    }
    
    Ok(result)
}
```

### 9.3 错误处理

- 语法错误：配置加载时失败，明确提示
- 运行时错误：记录警告，不影响原始结果返回
- 性能考虑：VRL 执行应有超时限制（建议 5 秒）

## 10. 循环引用处理策略

### 10.1 问题场景

Python 使用 `weakref` 避免 Computer ↔ SocketIOClient 的循环引用。Rust 需要类似机制。

### 10.2 Rust 实现方案

```rust
// 在 ComputerCore 中
pub struct ComputerCore {
    // 使用 Weak 引用持有客户端
    socketio_client: Option<Weak<SmcpComputerClient>>,
    // 其他字段...
}

// 在 SmcpComputerClient 中
pub struct SmcpComputerClient {
    // 使用 Arc 持有 Computer
    computer: Arc<ComputerCore>,
    // 其他字段...
}
```

### 10.3 生命周期管理

- Computer 启动时创建客户端，通过 `Arc::downgrade()` 保存 Weak 引用
- 客户端事件回调时，通过 `Weak::upgrade()` 获取强引用
- 如果 upgrade 失败（已被释放），静默跳过上报

## 11. WindowURI 过滤与缓存机制

### 11.1 WindowURI 识别规则

```rust
pub fn is_window_uri(uri: &str) -> bool {
    uri.starts_with("window://")
}
```

### 11.2 缓存增量更新逻辑

```rust
pub struct DesktopManager {
    // 缓存上次的 WindowURI 集合
    windows_cache: HashSet<String>,
}

impl DesktopManager {
    pub async fn handle_resource_change(&mut self, notification: ResourceNotification) {
        match notification {
            ResourceListChangedNotification => {
                let new_windows = self.collect_window_uris().await;
                if new_windows != self.windows_cache {
                    self.emit_refresh_desktop().await;
                    self.windows_cache = new_windows;
                }
            },
            ResourceUpdatedNotification { uri } if is_window_uri(&uri) => {
                // 单个窗口更新，立即刷新
                self.emit_refresh_desktop().await;
            },
            _ => {} // 忽略非 window:// 资源
        }
    }
}
```

## 12. 并发安全设计

### 12.1 锁选型原则

- Python `asyncio.Lock` → Rust `tokio::sync::Mutex`
- 读多写少场景使用 `tokio::sync::RwLock`
- 避免阻塞运行时，不用 std::sync::Mutex

### 12.2 关键共享数据

```rust
// 工具调用历史（线程安全）
pub struct ToolCallHistory {
    records: Arc<Mutex<VecDeque<ToolCallRecord>>>,
}

// MCP 管理器状态
pub struct McpServerManager {
    servers_config: Arc<RwLock<HashMap<String, ServerConfig>>>,
    active_clients: Arc<Mutex<HashMap<String, Arc<dyn McpClient>>>>,
    tool_mapping: Arc<RwLock<HashMap<String, String>>>,
}
```

## 13. 错误处理体系

### 13.1 自定义错误类型

```rust
#[derive(Debug, thiserror::Error)]
pub enum ComputerError {
    #[error("Tool name duplicated: {tool_name} in servers: {servers:?}")]
    ToolNameDuplicated { 
        tool_name: String, 
        servers: Vec<String> 
    },
    
    #[error("Input not found: {input_id}")]
    InputNotFound { input_id: String },
    
    #[error("Server {server_name} is not active")]
    ServerNotActive { server_name: String },
    
    #[error("VRL syntax error: {message}")]
    VrlSyntaxError { message: String },
    
    #[error("Tool execution timeout after {timeout}s")]
    ToolExecutionTimeout { timeout: u64 },
}
```

### 13.2 错误传播策略

- 内部错误使用 `?` 传播
- 对外 API 返回 `Result<T, ComputerError>`
- 错误信息包含足够上下文便于调试

## 14. streamable/http 与 sse 支持计划

中文：在接口层与 manager/core 完全一致，作为额外 client 实现并行推进。

要求：

- `list_tools/call_tool/list_windows/read_window` 语义一致
- 错误边界与重连策略对齐 Python

## 15. 里程碑与验收标准

### Milestone 1：协议闭环 + stdio 基础可用

- stdio client 可启动/停止且无泄漏
- Computer 可响应 Server 的 `CLIENT_GET_TOOLS`/`CLIENT_TOOL_CALL` ACK
- CLI REPL 可用 + 基础 PTY e2e

验收：

- 反复 start/stop（>=100 次）无僵尸进程、无任务泄漏（用测试证明）

### Milestone 2：多 MCP 聚合 + 冲突策略对齐 Python

- 多 server 聚合
- 工具冲突直接报错 + alias 建议
- forbidden_tools 与 default_tool_meta 合并逻辑对齐

### Milestone 3：取消/超时/重连（生产化稳定性）

- tool call timeout 与 cancel token
- 响应 `SERVER_TOOL_CALL_CANCEL`/相关通知
- auto_reconnect 语义对齐 Python

### Milestone 4：CLI 体验增强（pyexpect 级）

- 输出规范稳定（JSONL + prompt 恢复）
- 更完整命令集与补全
- 可选升级 TUI（ratatui）

## 16. 开发注意事项

- 不要随意修改协议字段/事件名；以 Python 实现与 `smcp` crate 的事件常量为准。

- stdio 相关代码必须优先写“关闭与回收”，再写功能。

- VRL 集成作为可选 feature，确保核心功能不依赖外部库。

- 所有共享状态必须考虑并发安全，使用适当的异步原语。

- 错误处理要提供足够的上下文，便于跨语言调试。

- 测试覆盖必须包含进程生命周期、并发场景、错误边界。
