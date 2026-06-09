/**
* 文件名: model
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: serde, async-trait
* 描述: MCP客户端相关的数据模型定义
*/
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

// Re-export MCP protocol types from rmcp
pub use rmcp::model::{
    Annotated, CallToolResult, Content, RawContent, RawResource, RawTextContent,
    ReadResourceResult, Resource, ResourceContents, Tool, ToolAnnotations,
};

// 常量定义 / Constants definition
pub const A2C_TOOL_META: &str = "a2c_tool_meta";
pub const A2C_VRL_TRANSFORMED: &str = "a2c_vrl_transformed";

// 类型别名 / Type aliases
pub type ServerName = String;
pub type ToolName = String;

/// MCP工具元数据 / MCP tool metadata
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolMeta {
    /// 是否自动使用 / Whether to auto-apply
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_apply: Option<bool>,
    /// 工具别名 / Tool alias
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// 工具标签 / Tool tags
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    /// 返回值字段映射 / Return value field mapping
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ret_object_mapper: Option<HashMap<String, String>>,
}

impl ToolMeta {
    /// 创建空的工具元数据 / Create empty tool metadata
    pub fn new() -> Self {
        Self {
            auto_apply: None,
            alias: None,
            tags: None,
            ret_object_mapper: None,
        }
    }
}

impl Default for ToolMeta {
    fn default() -> Self {
        Self::new()
    }
}

/// MCP服务器配置基类 / Base MCP server configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type")]
pub enum MCPServerConfig {
    /// STDIO类型服务器 / STDIO type server
    #[serde(alias = "stdio", alias = "STDIO")]
    Stdio(StdioServerConfig),
    /// SSE类型服务器 / SSE type server
    #[serde(alias = "sse", alias = "SSE")]
    Sse(SseServerConfig),
    /// HTTP类型服务器 / HTTP type server
    #[serde(alias = "http", alias = "HTTP")]
    Http(HttpServerConfig),
}

impl MCPServerConfig {
    /// 获取服务器名称 / Get server name
    pub fn name(&self) -> &str {
        match self {
            MCPServerConfig::Stdio(config) => &config.name,
            MCPServerConfig::Sse(config) => &config.name,
            MCPServerConfig::Http(config) => &config.name,
        }
    }

    /// 获取是否禁用标志 / Get disabled flag
    pub fn disabled(&self) -> bool {
        match self {
            MCPServerConfig::Stdio(config) => config.disabled,
            MCPServerConfig::Sse(config) => config.disabled,
            MCPServerConfig::Http(config) => config.disabled,
        }
    }

    /// 获取禁用工具列表 / Get forbidden tools list
    pub fn forbidden_tools(&self) -> &[String] {
        match self {
            MCPServerConfig::Stdio(config) => &config.forbidden_tools,
            MCPServerConfig::Sse(config) => &config.forbidden_tools,
            MCPServerConfig::Http(config) => &config.forbidden_tools,
        }
    }

    /// 获取工具元数据映射 / Get tool metadata mapping
    pub fn tool_meta(&self) -> &HashMap<ToolName, ToolMeta> {
        match self {
            MCPServerConfig::Stdio(config) => &config.tool_meta,
            MCPServerConfig::Sse(config) => &config.tool_meta,
            MCPServerConfig::Http(config) => &config.tool_meta,
        }
    }

    /// 获取默认工具元数据 / Get default tool metadata
    pub fn default_tool_meta(&self) -> Option<&ToolMeta> {
        match self {
            MCPServerConfig::Stdio(config) => config.default_tool_meta.as_ref(),
            MCPServerConfig::Sse(config) => config.default_tool_meta.as_ref(),
            MCPServerConfig::Http(config) => config.default_tool_meta.as_ref(),
        }
    }

    /// 获取VRL脚本 / Get VRL script
    pub fn vrl(&self) -> Option<&str> {
        match self {
            MCPServerConfig::Stdio(config) => config.vrl.as_deref(),
            MCPServerConfig::Sse(config) => config.vrl.as_deref(),
            MCPServerConfig::Http(config) => config.vrl.as_deref(),
        }
    }

    /// 获取 VS Code 风格 envFile 路径 / Get VS Code-style envFile path
    pub fn env_file(&self) -> Option<&str> {
        match self {
            MCPServerConfig::Stdio(config) => config.env_file.as_deref(),
            MCPServerConfig::Sse(config) => config.env_file.as_deref(),
            MCPServerConfig::Http(config) => config.env_file.as_deref(),
        }
    }
}

/// STDIO服务器配置 / STDIO server configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StdioServerConfig {
    /// 服务器名称 / Server name
    pub name: ServerName,
    /// 是否禁用 / Whether disabled
    #[serde(default)]
    pub disabled: bool,
    /// 禁用工具列表 / Forbidden tools list
    #[serde(default)]
    pub forbidden_tools: Vec<ToolName>,
    /// 工具元数据 / Tool metadata
    #[serde(default)]
    pub tool_meta: HashMap<ToolName, ToolMeta>,
    /// 默认工具元数据 / Default tool metadata
    pub default_tool_meta: Option<ToolMeta>,
    /// VRL脚本 / VRL script
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vrl: Option<String>,
    /// VS Code 风格 envFile：spawn 时从 `.env` 加载 `KEY=VALUE` 进 stdio server 的 `env`，显式 env 同名项
    /// 覆盖 envFile（显式胜，§9.1）。SDK 加性字段（待协议追认），仅 Computer 本地 spawn 消费；非 stdio 忽略 + WARN。
    /// VS Code-parity envFile: at spawn, load KEY=VALUE from .env into a stdio server's env (explicit env wins).
    #[serde(
        default,
        rename = "envFile",
        alias = "env_file",
        skip_serializing_if = "Option::is_none"
    )]
    pub env_file: Option<String>,
    /// STDIO服务器参数 / STDIO server parameters
    pub server_parameters: StdioServerParameters,
}

/// SSE服务器配置 / SSE server configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SseServerConfig {
    /// 服务器名称 / Server name
    pub name: ServerName,
    /// 是否禁用 / Whether disabled
    #[serde(default)]
    pub disabled: bool,
    /// 禁用工具列表 / Forbidden tools list
    #[serde(default)]
    pub forbidden_tools: Vec<ToolName>,
    /// 工具元数据 / Tool metadata
    #[serde(default)]
    pub tool_meta: HashMap<ToolName, ToolMeta>,
    /// 默认工具元数据 / Default tool metadata
    pub default_tool_meta: Option<ToolMeta>,
    /// VRL脚本 / VRL script
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vrl: Option<String>,
    /// VS Code 风格 envFile（仅对 stdio 生效；置于 sse 配置上 spawn 时记 WARN 并忽略，§9.1）。
    /// VS Code-parity envFile (only effective for stdio; on sse it is ignored with a WARN at spawn).
    #[serde(
        default,
        rename = "envFile",
        alias = "env_file",
        skip_serializing_if = "Option::is_none"
    )]
    pub env_file: Option<String>,
    /// SSE服务器参数 / SSE server parameters
    pub server_parameters: SseServerParameters,
}

/// HTTP服务器配置 / HTTP server configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HttpServerConfig {
    /// 服务器名称 / Server name
    pub name: ServerName,
    /// 是否禁用 / Whether disabled
    #[serde(default)]
    pub disabled: bool,
    /// 禁用工具列表 / Forbidden tools list
    #[serde(default)]
    pub forbidden_tools: Vec<ToolName>,
    /// 工具元数据 / Tool metadata
    #[serde(default)]
    pub tool_meta: HashMap<ToolName, ToolMeta>,
    /// 默认工具元数据 / Default tool metadata
    pub default_tool_meta: Option<ToolMeta>,
    /// VRL脚本 / VRL script
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vrl: Option<String>,
    /// VS Code 风格 envFile（仅对 stdio 生效；置于 http 配置上 spawn 时记 WARN 并忽略，§9.1）。
    /// VS Code-parity envFile (only effective for stdio; on http it is ignored with a WARN at spawn).
    #[serde(
        default,
        rename = "envFile",
        alias = "env_file",
        skip_serializing_if = "Option::is_none"
    )]
    pub env_file: Option<String>,
    /// HTTP服务器参数 / HTTP server parameters
    pub server_parameters: HttpServerParameters,
}

fn null_to_empty_map<'de, D>(deserializer: D) -> Result<HashMap<String, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt = Option::<HashMap<String, String>>::deserialize(deserializer)?;
    Ok(opt.unwrap_or_default())
}

/// STDIO服务器参数 / STDIO server parameters
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StdioServerParameters {
    /// 命令 / Command
    pub command: String,
    /// 参数 / Arguments
    #[serde(default)]
    pub args: Vec<String>,
    /// 环境变量 / Environment variables
    #[serde(default, deserialize_with = "null_to_empty_map")]
    pub env: HashMap<String, String>,
    /// 工作目录 / Working directory
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// SSE服务器参数 / SSE server parameters
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SseServerParameters {
    /// URL / URL
    pub url: String,
    /// Headers / Headers
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

/// HTTP服务器参数 / HTTP server parameters
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HttpServerParameters {
    /// URL / URL
    pub url: String,
    /// Headers / Headers
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

/// MCP服务器输入项基类 / Base MCP server input configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type")]
pub enum MCPServerInput {
    /// 字符串输入 / String input
    PromptString(PromptStringInput),
    /// 选择输入 / Pick string input
    PickString(PickStringInput),
    /// 命令输入 / Command input
    Command(CommandInput),
}

impl MCPServerInput {
    /// 获取输入ID / Get input ID
    pub fn id(&self) -> &str {
        match self {
            MCPServerInput::PromptString(input) => &input.id,
            MCPServerInput::PickString(input) => &input.id,
            MCPServerInput::Command(input) => &input.id,
        }
    }

    /// 获取输入描述 / Get input description
    pub fn description(&self) -> &str {
        match self {
            MCPServerInput::PromptString(input) => &input.description,
            MCPServerInput::PickString(input) => &input.description,
            MCPServerInput::Command(input) => &input.description,
        }
    }

    /// 获取默认值 / Get default value
    pub fn default(&self) -> Option<serde_json::Value> {
        match self {
            MCPServerInput::PromptString(input) => input
                .default
                .as_ref()
                .map(|s| serde_json::Value::String(s.clone())),
            MCPServerInput::PickString(input) => input
                .default
                .as_ref()
                .map(|s| serde_json::Value::String(s.clone())),
            MCPServerInput::Command(_input) => {
                // Command 类型不支持默认值
                // Command type doesn't support default values
                None
            }
        }
    }
}

/// 字符串输入类型 / String input type
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromptStringInput {
    /// 输入ID / Input ID
    pub id: String,
    /// 描述 / Description
    pub description: String,
    /// 默认值 / Default value
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// 是否为密码 / Whether password
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<bool>,
}

/// 选择输入类型 / Pick string input type
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PickStringInput {
    /// 输入ID / Input ID
    pub id: String,
    /// 描述 / Description
    pub description: String,
    /// 选项 / Options
    #[serde(default)]
    pub options: Vec<String>,
    /// 默认值 / Default value
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

/// 命令输入类型 / Command input type
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommandInput {
    /// 输入ID / Input ID
    pub id: String,
    /// 描述 / Description
    pub description: String,
    /// 命令 / Command
    pub command: String,
    /// 参数 / Arguments
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<HashMap<String, String>>,
}

/// 健康检查配置 / Health check configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HealthCheckConfig {
    /// 健康检查间隔（秒）/ Health check interval in seconds
    #[serde(default = "default_health_check_interval")]
    pub interval_secs: u64,
    /// 超时时间（秒）/ Timeout in seconds
    #[serde(default = "default_health_check_timeout")]
    pub timeout_secs: u64,
    /// 是否启用健康检查 / Whether to enable health check
    #[serde(default = "default_health_check_enabled")]
    pub enabled: bool,
}

fn default_health_check_interval() -> u64 {
    30
}

fn default_health_check_timeout() -> u64 {
    5
}

fn default_health_check_enabled() -> bool {
    true
}

impl Default for HealthCheckConfig {
    fn default() -> Self {
        Self {
            interval_secs: 30,
            timeout_secs: 5,
            enabled: true,
        }
    }
}

/// 重连策略 / Reconnect policy
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReconnectPolicy {
    /// 是否启用自动重连 / Whether to enable auto reconnect
    #[serde(default = "default_reconnect_enabled")]
    pub enabled: bool,
    /// 最大重试次数（0表示无限重试）/ Max retry count (0 means infinite)
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    /// 初始延迟时间（毫秒）/ Initial delay in milliseconds
    #[serde(default = "default_initial_delay_ms")]
    pub initial_delay_ms: u64,
    /// 最大延迟时间（毫秒）/ Max delay in milliseconds
    #[serde(default = "default_max_delay_ms")]
    pub max_delay_ms: u64,
    /// 退避因子（延迟时间乘数）/ Backoff factor (delay multiplier)
    #[serde(default = "default_backoff_factor")]
    pub backoff_factor: f64,
}

fn default_reconnect_enabled() -> bool {
    true
}

fn default_max_retries() -> u32 {
    5
}

fn default_initial_delay_ms() -> u64 {
    1000
}

fn default_max_delay_ms() -> u64 {
    30000
}

fn default_backoff_factor() -> f64 {
    2.0
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            max_retries: 5,
            initial_delay_ms: 1000,
            max_delay_ms: 30000,
            backoff_factor: 2.0,
        }
    }
}

impl ReconnectPolicy {
    /// 计算下次重试的延迟时间 / Calculate delay for next retry
    pub fn calculate_delay(&self, retry_count: u32) -> std::time::Duration {
        let delay_ms = (self.initial_delay_ms as f64 * self.backoff_factor.powi(retry_count as i32))
            .min(self.max_delay_ms as f64) as u64;
        std::time::Duration::from_millis(delay_ms)
    }

    /// 检查是否应该继续重试 / Check if should continue retry
    pub fn should_retry(&self, retry_count: u32) -> bool {
        self.enabled && (self.max_retries == 0 || retry_count < self.max_retries)
    }
}

/// 健康检查结果 / Health check result
#[derive(Debug, Clone)]
pub struct HealthCheckResult {
    /// 是否健康 / Is healthy
    pub is_healthy: bool,
    /// 检查时间 / Check time
    pub checked_at: std::time::Instant,
    /// 错误信息（如果有）/ Error message if any
    pub error: Option<String>,
    /// 响应时间（毫秒）/ Response time in milliseconds
    pub response_time_ms: Option<u64>,
}

/// 可取消工具调用的结果 / Outcome of a cancellable tool call.
///
/// 区分「正常完成」（含工具级 `isError`）与「被显式取消」（`notify:tool_call_cancel` 触发，
/// 在途调用已就地中断）。取消的**协议态**（结果级 `meta.a2c_cancelled`）由上层 `Computer` 用
/// SMCP-07 helper 统一写入——本层只表达控制流结果，不预先构造取消态 `CallToolResult`。
#[derive(Debug)]
pub enum CancellableCallOutcome {
    /// 正常完成（可能是工具级失败 `isError=true`）/ Completed (possibly a tool-level error).
    Completed(CallToolResult),
    /// 被显式取消：在途调用已就地中断；rmcp 传输已尽力向远端补发 MCP `notifications/cancelled`。
    /// Explicitly cancelled: in-flight call interrupted; rmcp transports best-effort emit `notifications/cancelled`.
    Cancelled,
}

/// MCP客户端协议trait / MCP client protocol trait
#[async_trait::async_trait]
pub trait MCPClientProtocol: Send + Sync {
    /// 获取客户端状态 / Get client state
    fn state(&self) -> ClientState;

    /// 连接MCP服务器 / Connect to MCP server
    async fn connect(&self) -> Result<(), MCPClientError>;

    /// 断开连接 / Disconnect
    async fn disconnect(&self) -> Result<(), MCPClientError>;

    /// 获取可用工具列表 / Get available tools list
    async fn list_tools(&self) -> Result<Vec<Tool>, MCPClientError>;

    /// 调用工具 / Call tool
    async fn call_tool(
        &self,
        tool_name: &str,
        params: serde_json::Value,
    ) -> Result<CallToolResult, MCPClientError>;

    /// 可被取消的工具调用（INT-02 #70 取消最后一公里）/ Cancellable tool call.
    ///
    /// `cancel` 触发（`Computer::acancel_tool` ← `notify:tool_call_cancel`）时就地中断在途调用并返回
    /// [`CancellableCallOutcome::Cancelled`]，使原 `client:tool_call` 的 ack 能迅速回填取消态响应。
    ///
    /// **默认实现**用 `select!` 竞速 [`Self::call_tool`] 与 `cancel.cancelled()`：取消胜出即 drop 在途
    /// 调用 future（就地中断——已满足协议 0.2.2 MUST：Agent 迅速拿到取消态响应），但**不**向远端补发 MCP
    /// `notifications/cancelled`。rmcp 传输（stdio）**覆盖**本方法，经 `RequestHandle` 暴露的 `request_id`
    /// best-effort 补发 `notifications/cancelled`（MCP 取消为协作式，SHOULD 而非 MUST）。HTTP/SSE 自研
    /// JSON-RPC 客户端沿用默认就地中断：其 `send_request` 的 id 为内部时间戳、外露需侵入式改造，对无状态
    /// 请求补发价值有限，列为后续项（见 #70 验收降级说明）。
    ///
    /// `biased`：在途调用与取消同时就绪时**优先**取真实结果（对齐 Python `if call_task in done` 先判定）。
    async fn call_tool_cancellable(
        &self,
        tool_name: &str,
        params: serde_json::Value,
        cancel: CancellationToken,
    ) -> Result<CancellableCallOutcome, MCPClientError> {
        tokio::select! {
            biased;
            res = self.call_tool(tool_name, params) => res.map(CancellableCallOutcome::Completed),
            _ = cancel.cancelled() => Ok(CancellableCallOutcome::Cancelled),
        }
    }

    /// 列出窗口资源 / List window resources
    async fn list_windows(&self) -> Result<Vec<Resource>, MCPClientError>;

    /// 单页透传 MCP `resources/list`（v0.2 `client:get_resources`）/ Single-page passthrough of
    /// MCP `resources/list` for the v0.2 `client:get_resources` forward path。
    ///
    /// 与 [`list_windows`](Self::list_windows) 严格独立：保持单页语义、不做 scheme 过滤、不订阅、不穷举翻页、
    /// 不返回 `resourceTemplates`；`cursor` 透传（首页传 `None`）。未声明 `resources` 能力 →
    /// [`MCPClientError::CapabilityNotSupported`]（上层映射 4015）。
    /// Strictly independent from `list_windows`: single-page, no scheme filter, no subscription, no
    /// pagination exhaustion, no resourceTemplates; cursor passed through (None for first page).
    async fn list_resources_page(
        &self,
        cursor: Option<String>,
    ) -> Result<(Vec<Resource>, Option<String>), MCPClientError>;

    /// 获取窗口详情 / Get window detail
    async fn get_window_detail(
        &self,
        resource: Resource,
    ) -> Result<ReadResourceResult, MCPClientError>;

    /// 订阅窗口资源更新 / Subscribe to window resource updates
    async fn subscribe_window(&self, resource: Resource) -> Result<(), MCPClientError>;

    /// 取消订阅窗口资源更新 / Unsubscribe from window resource updates
    async fn unsubscribe_window(&self, resource: Resource) -> Result<(), MCPClientError>;

    /// 执行健康检查 / Perform health check
    /// 默认实现通过检查状态和尝试 list_tools 来验证连接
    /// Default implementation checks state and tries list_tools to verify connection
    async fn health_check(&self) -> HealthCheckResult {
        let start = std::time::Instant::now();

        // 首先检查状态 / First check state
        if self.state() != ClientState::Connected {
            return HealthCheckResult {
                is_healthy: false,
                checked_at: start,
                error: Some(format!("Client state is {:?}, not Connected", self.state())),
                response_time_ms: None,
            };
        }

        // 尝试调用 list_tools 来验证连接 / Try calling list_tools to verify connection
        match tokio::time::timeout(std::time::Duration::from_secs(5), self.list_tools()).await {
            Ok(Ok(_)) => {
                let elapsed = start.elapsed();
                HealthCheckResult {
                    is_healthy: true,
                    checked_at: start,
                    error: None,
                    response_time_ms: Some(elapsed.as_millis() as u64),
                }
            }
            Ok(Err(e)) => HealthCheckResult {
                is_healthy: false,
                checked_at: start,
                error: Some(format!("Health check failed: {}", e)),
                response_time_ms: None,
            },
            Err(_) => HealthCheckResult {
                is_healthy: false,
                checked_at: start,
                error: Some("Health check timed out".to_string()),
                response_time_ms: None,
            },
        }
    }
}

/// 客户端状态 / Client state
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClientState {
    /// 已初始化 / Initialized
    Initialized,
    /// 已连接 / Connected
    Connected,
    /// 已断开 / Disconnected
    Disconnected,
    /// 错误状态 / Error
    Error,
}

impl fmt::Display for ClientState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClientState::Initialized => write!(f, "initialized"),
            ClientState::Connected => write!(f, "connected"),
            ClientState::Disconnected => write!(f, "disconnected"),
            ClientState::Error => write!(f, "error"),
        }
    }
}

/// MCP客户端错误 / MCP client error
#[derive(Debug, Error)]
pub enum MCPClientError {
    /// 连接错误 / Connection error
    #[error("Connection error: {0}")]
    ConnectionError(String),
    /// 协议错误 / Protocol error
    #[error("Protocol error: {0}")]
    ProtocolError(String),
    /// MCP Server 未声明所需 capability（如 `resources`）→ 上层映射 4015 /
    /// MCP Server did not declare the required capability (e.g. `resources`) → mapped to 4015 upstream.
    #[error("Capability not supported: {0}")]
    CapabilityNotSupported(String),
    /// IO错误 / IO error
    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),
    /// JSON错误 / JSON error
    #[error("JSON error: {0}")]
    JsonError(#[from] serde_json::Error),
    /// 超时错误 / Timeout error
    #[error("Timeout error: {0}")]
    TimeoutError(String),
    /// 其他错误 / Other error
    #[error("Other error: {0}")]
    Other(String),
}

/// 便捷函数：创建 Resource / Convenience: create a Resource
pub fn make_resource(
    uri: impl Into<String>,
    name: impl Into<String>,
    description: Option<String>,
    mime_type: Option<String>,
) -> Resource {
    use rmcp::model::AnnotateAble;
    let mut raw = RawResource::new(uri, name);
    raw.description = description;
    raw.mime_type = mime_type;
    raw.no_annotation()
}

/// 便捷函数：检查 CallToolResult 是否为错误 / Convenience: check if CallToolResult is error
pub fn is_call_tool_error(result: &CallToolResult) -> bool {
    result.is_error.unwrap_or(false)
}

/// 便捷函数：从 Content 中提取文本 / Convenience: extract text from Content
pub fn content_as_text(content: &Content) -> Option<&str> {
    content.as_text().map(|t| t.text.as_str())
}

/// 便捷函数：从 ResourceContents 中提取文本 / Convenience: extract text from ResourceContents
pub fn resource_contents_as_text(rc: &ResourceContents) -> Option<&str> {
    match rc {
        ResourceContents::TextResourceContents { text, .. } => Some(text.as_str()),
        _ => None,
    }
}

/// `client:get_skill` 响应的服务侧校验模型（v0.2.1）/ Server-side validation model for the
/// `client:get_skill` response。
///
/// 协议 `data-structures.md §GetSkillRet` / `skill.md §9` 规定 `body` 与 `blob_handle` **恰一存在**
/// （exactly one）：文本且 ≤ 内联预算 → `body`；二进制或过大文本 → `blob_handle`。Computer 在返回前
/// 调用 [`GetSkillRet::validate`] 强制该不变量（服务侧自校验）。`smcp` crate 的线缆结构为其镜像。
/// The protocol mandates exactly one of `body` / `blob_handle`; [`GetSkillRet::validate`] enforces it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GetSkillRet {
    /// SKILL 名（消歧后的 `A2CSkillRef` 名）/ SKILL name (disambiguated `A2CSkillRef` name)。
    pub name: String,
    /// 包根内的相对路径 / Relative path within the skill root。
    pub rel_path: String,
    /// MIME 类型 / MIME type。
    pub mime_type: String,
    /// 资源总字节数 / Total size in bytes。
    pub total_size: u64,
    /// 内容 sha256（hex）/ Content sha256 (hex)。
    pub sha256: String,
    /// 关联请求 id / Correlating request id。
    pub req_id: String,
    /// 内联文本正文（与 `blob_handle` 恰一）/ Inline text body (exactly one of body/blob_handle)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// 二进制旁路句柄（与 `body` 恰一）/ Binary sideband handle (exactly one of body/blob_handle)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob_handle: Option<String>,
}

impl GetSkillRet {
    /// 校验 `body` 与 `blob_handle` 恰一存在（XOR）/ Enforce exactly-one-of(body, blob_handle)。
    ///
    /// 两者皆有 / 皆无 → `Err`，附协议出处。对标 Python `GetSkillRet._check_body_blob_xor`。
    pub fn validate(&self) -> Result<(), String> {
        match (self.body.is_some(), self.blob_handle.is_some()) {
            (true, false) | (false, true) => Ok(()),
            (true, true) => Err(
                "GetSkillRet MUST carry exactly one of 'body' / 'blob_handle' (got both); \
                 protocol data-structures.md §GetSkillRet / skill.md §9"
                    .to_string(),
            ),
            (false, false) => Err(
                "GetSkillRet MUST carry exactly one of 'body' / 'blob_handle' (got neither); \
                 protocol data-structures.md §GetSkillRet / skill.md §9"
                    .to_string(),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_call_tool_error() {
        let ok_result = CallToolResult::success(vec![Content::text("ok")]);
        assert!(!is_call_tool_error(&ok_result));

        let err_result = CallToolResult::error(vec![Content::text("fail")]);
        assert!(is_call_tool_error(&err_result));
    }

    #[test]
    fn test_content_as_text() {
        let content = Content::text("hello");
        assert_eq!(content_as_text(&content), Some("hello"));
    }

    #[test]
    fn test_resource_contents_as_text() {
        let rc = ResourceContents::TextResourceContents {
            uri: "test://uri".to_string(),
            mime_type: None,
            text: "some text".to_string(),
            meta: None,
        };
        assert_eq!(resource_contents_as_text(&rc), Some("some text"));

        let blob = ResourceContents::BlobResourceContents {
            uri: "test://uri".to_string(),
            mime_type: None,
            blob: "base64data".to_string(),
            meta: None,
        };
        assert_eq!(resource_contents_as_text(&blob), None);
    }

    #[test]
    fn test_make_resource() {
        let resource = make_resource("window://test", "Test", Some("desc".into()), None);
        assert_eq!(resource.raw.uri, "window://test");
        assert_eq!(resource.raw.name, "Test");
        assert_eq!(resource.raw.description, Some("desc".into()));
        assert!(resource.raw.mime_type.is_none());
    }

    // ---- #74 INT-04：envFile 字段解析（envFile alias + env_file 名）----

    #[test]
    fn test_env_file_alias_camel_and_snake() {
        // VS Code 风格 camelCase `envFile`
        let camel = serde_json::json!({
            "type": "stdio",
            "name": "srv",
            "default_tool_meta": null,
            "envFile": ".env.prod",
            "server_parameters": { "command": "echo", "args": [] }
        });
        let cfg: MCPServerConfig = serde_json::from_value(camel).unwrap();
        assert_eq!(cfg.env_file(), Some(".env.prod"));

        // 名 `env_file`（populate_by_name 等价）
        let snake = serde_json::json!({
            "type": "stdio",
            "name": "srv",
            "default_tool_meta": null,
            "env_file": ".env.dev",
            "server_parameters": { "command": "echo", "args": [] }
        });
        let cfg2: MCPServerConfig = serde_json::from_value(snake).unwrap();
        assert_eq!(cfg2.env_file(), Some(".env.dev"));

        // 缺省 → None；序列化回 camelCase `envFile`
        let bare = serde_json::json!({
            "type": "stdio",
            "name": "srv",
            "default_tool_meta": null,
            "server_parameters": { "command": "echo", "args": [] }
        });
        let cfg3: MCPServerConfig = serde_json::from_value(bare).unwrap();
        assert_eq!(cfg3.env_file(), None);
        let round = serde_json::to_value(&cfg2).unwrap();
        assert_eq!(round["envFile"], serde_json::json!(".env.dev"));
        assert!(round.get("env_file").is_none());
    }

    // ---- #74 INT-04：GetSkillRet body/blob_handle 恰一互斥校验 ----

    fn skill_ret(body: Option<&str>, handle: Option<&str>) -> GetSkillRet {
        GetSkillRet {
            name: "marketplace:demo:skill".into(),
            rel_path: "SKILL.md".into(),
            mime_type: "text/markdown".into(),
            total_size: 42,
            sha256: "ab12".into(),
            req_id: "req-1".into(),
            body: body.map(str::to_string),
            blob_handle: handle.map(str::to_string),
        }
    }

    #[test]
    fn test_get_skill_ret_xor_valid() {
        assert!(skill_ret(Some("hello"), None).validate().is_ok());
        assert!(skill_ret(None, Some("blob:abc")).validate().is_ok());
    }

    #[test]
    fn test_get_skill_ret_xor_both_or_neither_rejected() {
        let both = skill_ret(Some("hello"), Some("blob:abc")).validate();
        assert!(both.is_err());
        assert!(both.unwrap_err().contains("both"));

        let neither = skill_ret(None, None).validate();
        assert!(neither.is_err());
        assert!(neither.unwrap_err().contains("neither"));
    }
}
