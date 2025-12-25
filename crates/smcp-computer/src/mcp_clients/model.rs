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

    /// 列出窗口资源 / List window resources
    async fn list_windows(&self) -> Result<Vec<Resource>, MCPClientError>;

    /// 获取窗口详情 / Get window detail
    async fn get_window_detail(
        &self,
        resource: Resource,
    ) -> Result<ReadResourceResult, MCPClientError>;

    /// 订阅窗口资源更新 / Subscribe to window resource updates
    async fn subscribe_window(&self, resource: Resource) -> Result<(), MCPClientError>;

    /// 取消订阅窗口资源更新 / Unsubscribe from window resource updates
    async fn unsubscribe_window(&self, resource: Resource) -> Result<(), MCPClientError>;
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

/// MCP工具定义 / MCP tool definition
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Tool {
    /// 工具名称 / Tool name
    pub name: String,
    /// 工具描述 / Tool description
    pub description: String,
    /// 输入模式 / Input schema
    #[serde(rename = "inputSchema")]
    pub input_schema: serde_json::Value,
    /// 工具注解 / Tool annotations
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<ToolAnnotations>,
    /// 工具元数据 / Tool metadata
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<HashMap<String, serde_json::Value>>,
}

/// 工具注解 / Tool annotations
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolAnnotations {
    /// 标题 / Title
    pub title: String,
    /// 是否只读 / Read only hint
    #[serde(rename = "readOnlyHint")]
    pub read_only_hint: bool,
    /// 是否破坏性 / Destructive hint
    #[serde(rename = "destructiveHint")]
    pub destructive_hint: bool,
    /// 开放世界提示 / Open world hint
    #[serde(rename = "openWorldHint")]
    pub open_world_hint: bool,
}

/// 资源定义 / Resource definition
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Resource {
    /// URI / URI
    pub uri: String,
    /// 名称 / Name
    pub name: String,
    /// 描述 / Description
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// MIME类型 / MIME type
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

/// 工具调用结果 / Tool call result
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CallToolResult {
    /// 内容 / Content
    pub content: Vec<Content>,
    /// 是否为错误 / Is error
    #[serde(default)]
    pub is_error: bool,
    /// 元数据 / Metadata
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<HashMap<String, serde_json::Value>>,
}

/// 内容块 / Content block
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum Content {
    /// 文本内容 / Text content
    #[serde(rename = "text")]
    Text { text: String },
    /// 图片内容 / Image content
    #[serde(rename = "image")]
    Image { data: String, mime_type: String },
    /// 资源内容 / Resource content
    #[serde(rename = "resource")]
    Resource {
        uri: String,
        mime_type: Option<String>,
    },
}

/// 读取资源结果 / Read resource result
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReadResourceResult {
    /// 内容 / Contents
    pub contents: Vec<TextResourceContents>,
}

/// 文本资源内容 / Text resource contents
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TextResourceContents {
    /// URI / URI
    pub uri: String,
    /// 文本内容 / Text content
    pub text: String,
    /// MIME类型 / MIME type
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

/// 列出资源结果 / List resources result（支持分页）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ListResourcesResult {
    /// 资源列表 / Resource list
    pub resources: Vec<Resource>,
    /// 下一页游标 / Next page cursor
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}
