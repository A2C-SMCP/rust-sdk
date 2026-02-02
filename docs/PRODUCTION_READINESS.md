# A2C-SMCP Rust SDK 生产就绪优化指南

> **版本**: 1.0
> **状态**: 草案
> **最后更新**: 2026-01-31
> **目标**: 完成剩余 5% 工作，达到生产可用状态

---

## 目录

1. [概述](#概述)
2. [P0 - 阻塞生产的关键问题](#p0---阻塞生产的关键问题)
3. [P1 - 功能完整性](#p1---功能完整性)
4. [P2 - 稳定性与可靠性](#p2---稳定性与可靠性)
5. [P3 - 可观测性与运维](#p3---可观测性与运维)
6. [验收检查清单](#验收检查清单)

---

## 概述

### 当前状态

| 模块 | 完成度 | 状态 |
|------|--------|------|
| smcp (协议定义) | 100% | ✅ 就绪 |
| smcp-server-core | 98% | ⚠️ 需优化 |
| smcp-agent | 95% | ⚠️ 需完善 |
| smcp-computer | 92% | ⚠️ 需完善 |
| smcp-server-hyper | 100% | ✅ 就绪 |

### 优先级说明

- **P0**: 阻塞生产部署，必须在发布前完成
- **P1**: 影响核心功能，建议在发布前完成
- **P2**: 影响稳定性，可在发布后迭代
- **P3**: 增强功能，按需实现

---

## P0 - 阻塞生产的关键问题

### P0-1: 配置渲染逻辑实现

**问题描述**:
`Computer::render_server_config` 方法未实现，导致无法使用 `${input_id}` 动态变量替换 MCP Server 配置。

**影响范围**:
- 无法动态注入 API Key、Token 等敏感配置
- 无法使用 `MCPServerInput` 的 `command` 类型动态获取配置值

**文件位置**: `crates/smcp-computer/src/computer.rs:271-279`

**当前代码**:
```rust
async fn render_server_config(
    &self,
    config: &MCPServerConfig,
) -> ComputerResult<MCPServerConfig> {
    // TODO: 实现配置渲染逻辑 / TODO: Implement config rendering logic
    Ok(config.clone())
}
```

**实现方案**:

```rust
/// 渲染服务器配置，替换 ${input_id} 占位符
async fn render_server_config(
    &self,
    config: &MCPServerConfig,
) -> ComputerResult<MCPServerConfig> {
    // 1. 序列化配置为 JSON
    let config_json = serde_json::to_string(config)?;

    // 2. 查找所有 ${...} 占位符
    let re = regex::Regex::new(r"\$\{([^}]+)\}").unwrap();
    let mut rendered = config_json.clone();

    for cap in re.captures_iter(&config_json) {
        let placeholder = &cap[0];  // ${input_id}
        let input_id = &cap[1];     // input_id

        // 3. 获取输入值（优先从缓存，其次从 Session 解析）
        let value = self.resolve_input_value(input_id).await?;

        // 4. 替换占位符
        let value_str = match value {
            serde_json::Value::String(s) => s,
            other => other.to_string(),
        };
        rendered = rendered.replace(placeholder, &value_str);
    }

    // 5. 反序列化回配置结构
    let rendered_config: MCPServerConfig = serde_json::from_str(&rendered)?;
    Ok(rendered_config)
}

/// 解析输入值
async fn resolve_input_value(&self, input_id: &str) -> ComputerResult<serde_json::Value> {
    // 优先检查缓存
    if let Some(cached) = self.get_input_value(input_id).await? {
        return Ok(cached);
    }

    // 获取输入定义
    let input = self.get_input(input_id).await?
        .ok_or_else(|| ComputerError::ValidationError(
            format!("Input '{}' not found", input_id)
        ))?;

    // 通过 Session 解析
    let value = self.session.resolve_input(&input).await?;

    // 缓存结果
    self.set_input_value(input_id, value.clone()).await?;

    Ok(value)
}
```

**依赖添加** (`Cargo.toml`):
```toml
regex = "1"
```

**验收标准**:
- [ ] 支持 `${input_id}` 语法替换
- [ ] 支持嵌套引用 `${input1}/${input2}`
- [ ] 循环引用检测并报错
- [ ] 单元测试覆盖

**预计工时**: 4h

---

### P0-2: Agent 自动行为恢复

**问题描述**:
Agent 收到 `notify:enter_office` 后不会自动调用 `get_tools`，与 Python SDK 行为不一致。

**影响范围**:
- Agent 无法自动发现新加入的 Computer 工具
- 需要手动调用 `get_tools`，用户体验差

**文件位置**: `crates/smcp-agent/src/async_agent.rs:77-88`

**当前代码**:
```rust
NotificationMessage::EnterOffice(data) => {
    info!("Processing EnterOffice event: {:?}", data);

    // TODO: Python 的自动行为：收到 enter_office 后自动触发 get_tools
    // 暂时禁用，因为 get_tools 可能会阻塞
    // if let Some(ref computer) = data.computer {
    //     if let Ok(tools) = agent_clone.get_tools(computer).await {
    //         ...
    //     }
    // }
    ...
}
```

**实现方案**:

1. 添加配置开关:

```rust
// crates/smcp-agent/src/config.rs
#[derive(Debug, Clone)]
pub struct SmcpAgentConfig {
    // ... 现有字段 ...

    /// 收到 Computer 加入通知后是否自动获取工具列表
    /// 默认: true (与 Python SDK 行为一致)
    pub auto_fetch_tools_on_enter: bool,

    /// 收到配置/工具列表更新通知后是否自动刷新
    /// 默认: true
    pub auto_refresh_on_update: bool,
}

impl Default for SmcpAgentConfig {
    fn default() -> Self {
        Self {
            // ... 现有默认值 ...
            auto_fetch_tools_on_enter: true,
            auto_refresh_on_update: true,
        }
    }
}
```

2. 修改通知处理逻辑:

```rust
// crates/smcp-agent/src/async_agent.rs
NotificationMessage::EnterOffice(data) => {
    info!("Processing EnterOffice event: {:?}", data);

    // 自动获取工具（异步执行，不阻塞通知处理）
    if agent_clone.config.auto_fetch_tools_on_enter {
        if let Some(ref computer) = data.computer {
            let agent = agent_clone.clone();
            let computer_name = computer.clone();
            let handler = event_handler.clone();

            // 使用 spawn 避免阻塞
            tokio::spawn(async move {
                match agent.get_tools(&computer_name).await {
                    Ok(tools) => {
                        info!("Auto-fetched {} tools from {}", tools.len(), computer_name);
                        if let Some(ref h) = handler {
                            let _ = h.on_tools_received(&computer_name, tools, &agent).await;
                        }
                    }
                    Err(e) => {
                        warn!("Failed to auto-fetch tools from {}: {}", computer_name, e);
                    }
                }
            });
        }
    }

    // 触发用户事件处理器
    if let Some(ref handler) = event_handler {
        let _ = handler.on_computer_enter_office(data, &agent_clone).await;
    }
}
```

**验收标准**:
- [ ] 默认行为与 Python SDK 一致
- [ ] 可通过配置禁用自动行为
- [ ] 自动获取不阻塞主通知处理流程
- [ ] 失败时仅记录警告，不影响其他流程

**预计工时**: 2h

---

## P1 - 功能完整性

### P1-1: 标准化错误码

**问题描述**:
当前错误信息为自由文本，未遵循协议规范定义的错误码体系。

**影响范围**:
- 客户端无法根据错误码进行精确的错误处理
- 日志分析和监控告警困难

**协议规范** (`specification/error-handling.md`):

| 代码 | 名称 | 场景 |
|------|------|------|
| 400 | Bad Request | 数据校验失败 |
| 401 | Unauthorized | 认证失败 |
| 403 | Forbidden | 权限违规 |
| 404 | Not Found | 资源不存在 |
| 408 | Timeout | 请求超时 |
| 500 | Internal Error | 内部错误 |
| 4001 | Tool Not Found | 工具不存在 |
| 4002 | Tool Disabled | 工具被禁用 |
| 4003 | Tool Execution Failed | 工具执行失败 |
| 4004 | Tool Timeout | 工具执行超时 |
| 4101 | Room Full | 房间已有 Agent |
| 4103 | Not In Room | 未加入房间 |
| 4104 | Cross Room Access | 跨房间访问 |

**实现方案**:

1. 创建错误码模块:

```rust
// crates/smcp/src/error.rs (新文件)
use serde::{Deserialize, Serialize};

/// SMCP 标准错误码
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u16)]
pub enum SmcpErrorCode {
    // 通用错误 (400-500)
    BadRequest = 400,
    Unauthorized = 401,
    Forbidden = 403,
    NotFound = 404,
    Timeout = 408,
    InternalError = 500,

    // 工具相关错误 (4001-4005)
    ToolNotFound = 4001,
    ToolDisabled = 4002,
    ToolExecutionFailed = 4003,
    ToolTimeout = 4004,
    ToolRequiresConfirmation = 4005,

    // 房间管理错误 (4101-4104)
    RoomFull = 4101,
    RoomNotFound = 4102,
    NotInRoom = 4103,
    CrossRoomAccess = 4104,
}

/// SMCP 标准错误响应
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmcpError {
    pub error: SmcpErrorDetail,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmcpErrorDetail {
    pub code: u16,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

impl SmcpError {
    pub fn new(code: SmcpErrorCode, message: impl Into<String>) -> Self {
        Self {
            error: SmcpErrorDetail {
                code: code as u16,
                message: message.into(),
                details: None,
            },
        }
    }

    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.error.details = Some(details);
        self
    }
}

// 便捷构造函数
impl SmcpError {
    pub fn tool_not_found(tool_name: &str) -> Self {
        Self::new(
            SmcpErrorCode::ToolNotFound,
            format!("Tool '{}' not found", tool_name),
        ).with_details(serde_json::json!({ "tool_name": tool_name }))
    }

    pub fn room_full(office_id: &str) -> Self {
        Self::new(
            SmcpErrorCode::RoomFull,
            "Room already has an agent",
        ).with_details(serde_json::json!({ "office_id": office_id }))
    }

    // ... 其他便捷方法
}
```

2. 在 Server Handler 中使用:

```rust
// crates/smcp-server-core/src/handler.rs
use smcp::error::{SmcpError, SmcpErrorCode};

// 修改返回类型
async fn on_client_tool_call(
    socket: SocketRef,
    data: ToolCallReq,
    state: ServerState,
) -> Result<Value, SmcpError> {  // 使用 SmcpError
    // ...

    // 工具不存在时返回标准错误
    let computer_sid = state
        .session_manager
        .get_computer_sid_in_office(&office_id, &data.computer)
        .ok_or_else(|| SmcpError::new(
            SmcpErrorCode::NotFound,
            format!("Computer '{}' not found in office", data.computer),
        ))?;

    // ...
}
```

**验收标准**:
- [ ] 所有错误返回标准 `SmcpError` 格式
- [ ] 错误码与协议规范一致
- [ ] `details` 字段不包含敏感信息
- [ ] 单元测试覆盖所有错误码

**预计工时**: 4h

---

### P1-2: MCP 客户端稳定性增强

**问题描述**:
MCP 客户端连接管理相对简单，缺少重连和健康检查机制。

**影响范围**:
- 网络抖动导致连接断开后不会自动恢复
- 无法感知 MCP Server 进程崩溃

**实现方案**:

1. 添加健康检查:

```rust
// crates/smcp-computer/src/mcp_clients/base_client.rs

/// 客户端健康检查配置
#[derive(Debug, Clone)]
pub struct HealthCheckConfig {
    /// 检查间隔
    pub interval: Duration,
    /// 超时时间
    pub timeout: Duration,
    /// 最大连续失败次数
    pub max_failures: u32,
}

impl Default for HealthCheckConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(30),
            timeout: Duration::from_secs(5),
            max_failures: 3,
        }
    }
}

/// 启动健康检查任务
pub fn start_health_check(
    client: Arc<dyn MCPClientProtocol>,
    config: HealthCheckConfig,
    on_unhealthy: impl Fn() + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut failure_count = 0u32;

        loop {
            tokio::time::sleep(config.interval).await;

            // 尝试 ping (使用 list_tools 作为健康检查)
            let result = tokio::time::timeout(
                config.timeout,
                client.list_tools(),
            ).await;

            match result {
                Ok(Ok(_)) => {
                    failure_count = 0;
                }
                Ok(Err(e)) => {
                    failure_count += 1;
                    warn!("Health check failed: {}", e);
                }
                Err(_) => {
                    failure_count += 1;
                    warn!("Health check timeout");
                }
            }

            if failure_count >= config.max_failures {
                error!("Client unhealthy after {} failures", failure_count);
                on_unhealthy();
                break;
            }
        }
    })
}
```

2. 添加自动重连:

```rust
// crates/smcp-computer/src/mcp_clients/manager.rs

impl MCPServerManager {
    /// 启动自动重连任务
    pub async fn enable_auto_reconnect(&self, interval: Duration) {
        let clients = self.active_clients.clone();
        let configs = self.servers_config.clone();
        let self_ref = self.clone();  // 需要实现 Clone

        tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;

                let client_states: Vec<_> = {
                    let clients_guard = clients.read().await;
                    clients_guard.iter()
                        .map(|(name, client)| (name.clone(), client.state()))
                        .collect()
                };

                for (name, state) in client_states {
                    if state == ClientState::Disconnected || state == ClientState::Error {
                        info!("Attempting to reconnect: {}", name);
                        if let Err(e) = self_ref.restart_server(&name).await {
                            warn!("Reconnect failed for {}: {}", name, e);
                        }
                    }
                }
            }
        });
    }
}
```

**验收标准**:
- [ ] 连接断开后自动重连
- [ ] 健康检查检测死连接
- [ ] 重连失败有退避策略
- [ ] 状态变更有日志记录

**预计工时**: 6h

---

### P1-3: GetComputerConfigRet 完整实现

**问题描述**:
`client:get_config` 返回的配置信息可能不完整。

**影响范围**:
- Agent 无法获取 Computer 的完整配置视图

**实现方案**:

```rust
// crates/smcp-computer/src/computer.rs

/// 获取完整配置（用于响应 client:get_config）
pub async fn get_config(&self) -> ComputerResult<GetComputerConfigRet> {
    // 1. 收集所有 inputs
    let inputs: Vec<serde_json::Value> = self.list_inputs().await?
        .into_iter()
        .map(|input| serde_json::to_value(input).unwrap())
        .collect();

    // 2. 收集所有 servers（渲染后的配置，但隐藏敏感信息）
    let servers = self.mcp_servers.read().await;
    let mut servers_map = serde_json::Map::new();

    for (name, config) in servers.iter() {
        // 渲染配置
        let rendered = self.render_server_config(config).await?;

        // 转换为 JSON 并脱敏
        let mut config_json = serde_json::to_value(&rendered)?;
        sanitize_sensitive_fields(&mut config_json);

        servers_map.insert(name.clone(), config_json);
    }

    Ok(GetComputerConfigRet {
        inputs: if inputs.is_empty() { None } else { Some(inputs) },
        servers: serde_json::Value::Object(servers_map),
    })
}

/// 脱敏处理：移除或掩码敏感字段
fn sanitize_sensitive_fields(value: &mut serde_json::Value) {
    if let serde_json::Value::Object(map) = value {
        // 处理环境变量中的敏感信息
        if let Some(serde_json::Value::Object(env)) = map.get_mut("env") {
            for (key, val) in env.iter_mut() {
                let key_lower = key.to_lowercase();
                if key_lower.contains("key")
                    || key_lower.contains("secret")
                    || key_lower.contains("token")
                    || key_lower.contains("password")
                {
                    *val = serde_json::Value::String("***REDACTED***".to_string());
                }
            }
        }

        // 递归处理
        for (_, v) in map.iter_mut() {
            sanitize_sensitive_fields(v);
        }
    }
}
```

**验收标准**:
- [ ] 返回完整的 inputs 和 servers 配置
- [ ] 敏感信息已脱敏
- [ ] 与 Python SDK 返回格式一致

**预计工时**: 3h

---

## P2 - 稳定性与可靠性

### P2-1: 工具调用取消机制

**问题描述**:
超时后发送取消请求，但工具执行可能仍在进行。

**实现方案**:

```rust
// crates/smcp-computer/src/computer.rs

use tokio_util::sync::CancellationToken;

/// 执行工具调用（支持取消）
pub async fn execute_tool_cancellable(
    &self,
    req_id: &str,
    tool_name: &str,
    parameters: serde_json::Value,
    timeout: Option<f64>,
    cancel_token: CancellationToken,
) -> ComputerResult<CallToolResult> {
    let manager = self.mcp_manager.read().await;
    let manager = manager.as_ref()
        .ok_or_else(|| ComputerError::InvalidState("Not initialized".into()))?;

    let (server_name, tool_name) = manager
        .validate_tool_call(tool_name, &parameters)
        .await?;

    let timeout_duration = timeout.map(Duration::from_secs_f64);

    tokio::select! {
        // 正常执行路径
        result = manager.call_tool(&server_name, &tool_name, parameters, timeout_duration) => {
            result
        }
        // 取消路径
        _ = cancel_token.cancelled() => {
            warn!("Tool call cancelled: {} (req_id={})", tool_name, req_id);
            Ok(CallToolResult {
                content: vec![Content::Text {
                    text: format!("Tool call cancelled (req_id={})", req_id),
                }],
                is_error: true,
                meta: Some(HashMap::from([
                    ("cancelled".to_string(), serde_json::json!(true)),
                ])),
            })
        }
    }
}
```

**验收标准**:
- [ ] 取消请求能中断正在执行的工具
- [ ] 取消后返回明确的取消状态
- [ ] 取消不影响其他工具调用

**预计工时**: 4h

---

### P2-2: 资源订阅与实时更新

**问题描述**:
SSE 客户端的资源订阅功能未完全实现。

**文件位置**: `crates/smcp-computer/src/mcp_clients/subscription_manager.rs`

**实现方案**:

```rust
// crates/smcp-computer/src/mcp_clients/subscription_manager.rs

use std::collections::HashMap;
use tokio::sync::{broadcast, RwLock};

pub struct SubscriptionManager {
    /// URI -> 订阅者广播通道
    subscriptions: RwLock<HashMap<String, broadcast::Sender<ResourceUpdate>>>,
}

#[derive(Debug, Clone)]
pub struct ResourceUpdate {
    pub uri: String,
    pub content: Option<ReadResourceResult>,
    pub is_deleted: bool,
}

impl SubscriptionManager {
    pub fn new() -> Self {
        Self {
            subscriptions: RwLock::new(HashMap::new()),
        }
    }

    /// 订阅资源更新
    pub async fn subscribe(&self, uri: &str) -> broadcast::Receiver<ResourceUpdate> {
        let mut subs = self.subscriptions.write().await;

        if let Some(tx) = subs.get(uri) {
            tx.subscribe()
        } else {
            let (tx, rx) = broadcast::channel(16);
            subs.insert(uri.to_string(), tx);
            rx
        }
    }

    /// 发布资源更新
    pub async fn publish(&self, update: ResourceUpdate) {
        let subs = self.subscriptions.read().await;
        if let Some(tx) = subs.get(&update.uri) {
            let _ = tx.send(update);
        }
    }

    /// 取消订阅
    pub async fn unsubscribe(&self, uri: &str) {
        let mut subs = self.subscriptions.write().await;
        subs.remove(uri);
    }
}
```

**验收标准**:
- [ ] 支持订阅 `window://` 资源
- [ ] 资源变更时推送更新
- [ ] 正确处理取消订阅

**预计工时**: 4h

---

## P3 - 可观测性与运维

### P3-1: 结构化日志增强

**实现方案**:

```rust
// 添加 tracing span 支持
use tracing::{instrument, Span};

impl<S: Session> Computer<S> {
    #[instrument(
        skip(self, parameters),
        fields(
            req_id = %req_id,
            tool = %tool_name,
            server = tracing::field::Empty,
        )
    )]
    pub async fn execute_tool(
        &self,
        req_id: &str,
        tool_name: &str,
        parameters: serde_json::Value,
        timeout: Option<f64>,
    ) -> ComputerResult<CallToolResult> {
        let (server_name, _) = self.validate_tool_call(tool_name, &parameters).await?;

        // 记录服务器名称
        Span::current().record("server", &server_name);

        // ... 执行逻辑
    }
}
```

**依赖**:
```toml
tracing = { version = "0.1", features = ["attributes"] }
tracing-subscriber = { version = "0.3", features = ["json"] }
```

**验收标准**:
- [ ] 关键操作有 span 追踪
- [ ] 支持 JSON 格式日志输出
- [ ] 日志包含 req_id 关联

**预计工时**: 2h

---

### P3-2: Metrics 指标收集

**实现方案**:

```rust
// crates/smcp-computer/src/metrics.rs (新文件)

use once_cell::sync::Lazy;
use prometheus::{
    register_counter_vec, register_histogram_vec,
    CounterVec, HistogramVec,
};

/// 工具调用计数器
pub static TOOL_CALLS_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    register_counter_vec!(
        "smcp_tool_calls_total",
        "Total number of tool calls",
        &["server", "tool", "status"]
    ).unwrap()
});

/// 工具调用延迟直方图
pub static TOOL_CALL_DURATION: Lazy<HistogramVec> = Lazy::new(|| {
    register_histogram_vec!(
        "smcp_tool_call_duration_seconds",
        "Tool call duration in seconds",
        &["server", "tool"],
        vec![0.01, 0.05, 0.1, 0.5, 1.0, 5.0, 10.0, 30.0]
    ).unwrap()
});

/// 活跃连接数
pub static ACTIVE_CONNECTIONS: Lazy<prometheus::IntGauge> = Lazy::new(|| {
    prometheus::register_int_gauge!(
        "smcp_active_connections",
        "Number of active Socket.IO connections"
    ).unwrap()
});
```

**验收标准**:
- [ ] 收集工具调用 QPS
- [ ] 收集调用延迟分布
- [ ] 收集连接状态

**预计工时**: 3h

---

## 验收检查清单

### 功能验收

- [ ] **P0-1**: 配置渲染 `${input_id}` 替换正常工作
- [ ] **P0-2**: Agent 自动获取工具行为与 Python SDK 一致
- [ ] **P1-1**: 错误返回标准 SMCP 格式
- [ ] **P1-2**: 连接断开后自动重连
- [ ] **P1-3**: `get_config` 返回完整配置

### 兼容性验收

- [ ] Rust Agent 可与 Python Server 通信
- [ ] Rust Server 可与 Python Agent 通信
- [ ] Rust Computer 可与 Python Agent 通信
- [ ] 三种 MCP 传输模式均可工作 (stdio/SSE/HTTP)

### 性能验收

- [ ] 工具调用 P99 延迟 < 100ms (不含工具执行时间)
- [ ] 单 Server 支持 100+ 并发连接
- [ ] 内存无明显泄漏 (24h 压测)

### 测试验收

- [ ] 单元测试覆盖率 > 70%
- [ ] E2E 测试通过 (`cargo test-e2e`)
- [ ] 与 Python SDK 互操作测试通过

---

## 附录: 任务工时汇总

| 任务 | 优先级 | 预计工时 | 负责人 |
|-----|--------|---------|--------|
| P0-1 配置渲染 | P0 | 4h | |
| P0-2 Agent 自动行为 | P0 | 2h | |
| P1-1 错误码标准化 | P1 | 4h | |
| P1-2 MCP 客户端稳定性 | P1 | 6h | |
| P1-3 GetConfig 完整实现 | P1 | 3h | |
| P2-1 工具调用取消 | P2 | 4h | |
| P2-2 资源订阅 | P2 | 4h | |
| P3-1 结构化日志 | P3 | 2h | |
| P3-2 Metrics 指标 | P3 | 3h | |
| **总计** | | **32h** | |

**建议排期**:
- **Week 1**: 完成 P0 全部 + P1-1 (10h)
- **Week 2**: 完成 P1-2, P1-3 (9h)
- **Week 3**: 完成 P2 全部 (8h)
- **Week 4**: 完成 P3 + 验收测试 (5h + 测试)

---

## 变更记录

| 版本 | 日期 | 变更内容 | 作者 |
|-----|------|---------|------|
| 1.0 | 2026-01-31 | 初稿 | Claude |
