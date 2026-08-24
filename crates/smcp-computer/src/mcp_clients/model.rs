/**
* 文件名: model
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: serde, async-trait
* 描述: MCP客户端相关的数据模型定义
*/
use serde::de::{Error as _, IgnoredAny};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::HashMap;
use std::fmt;
use thiserror::Error;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

// Re-export MCP protocol types from rmcp
pub use rmcp::model::{
    CallToolResult, ContentBlock as Content, ReadResourceResult, Resource, ResourceContents, Tool,
    ToolAnnotations,
};

// 常量定义 / Constants definition
pub const A2C_TOOL_META: &str = "a2c_tool_meta";
pub const A2C_VRL_TRANSFORMED: &str = "a2c_vrl_transformed";

// 类型别名 / Type aliases
/// MCP Server 的 **display 名**（给人看、允许碰撞、**非身份**）/ display name (may collide; NOT identity)。
///
/// #130：**有意**保持 `String`——display 名混用无害，不值得付 newtype 的 `.0` / `.as_str()` 噪声。
/// 身份请用 [`BundleId`]（**不同型**，混用即编译红）。
pub type ServerName = String;
pub type ToolName = String;
/// 聚合后暴露给 LLM 的工具名 `{bundle_id}__{alias ?? 原始名}` / aggregated exposed tool name。
///
/// #130：同 [`ServerName`]，本轮**有意**保持 `String`。
pub type ExposedToolName = String;

/// MCP Server 唯一标识（BundleID，**构造即校验**）/ MCP Server unique identity (valid by construction)。
///
/// #130：由 `pub type BundleId = String`（与 [`ServerName`] 对编译器**完全同型**）改为协议 crate 的
/// **newtype**——权威定义与合法性判据同处 [`smcp::utils::bundle_id`]，使 wire / SKILL / computer / agent
/// 共用同一类型与同一权威。缺省生成算法仍在 [`super::bundle_id`]。
pub use smcp::utils::bundle_id::BundleId;

/// MCP Server 运行期变化通知的种类（#106）/ Kind of a runtime MCP server change notification。
///
/// 由各 MCP 客户端（stdio 经 rmcp `ClientHandler`；sse/http 经其常驻通知流）在收到服务器主动通知时构造，
/// 经 [`ClientNotifyCtx`] 的 channel 上报给 Computer 的单消费者任务，触发对应的 emit / 回拉链。
/// 生产端**只发 channel、不做任何 peer 请求**（避免在通知回调上下文里重入；见 stdio handler 注释）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpChangeKind {
    /// `notifications/tools/list_changed` —— 工具集变化 / tool set changed。
    ToolListChanged,
    /// `notifications/resources/list_changed` —— 资源集变化（window:// / skill:// 需消费方重枚举）。
    ResourceListChanged,
    /// `notifications/resources/updated` —— 指定 URI 内容更新 / a specific resource's content updated。
    ResourceUpdated { uri: String },
}

/// 携带来源 server 身份的 MCP 变化通知 / An MCP change notification tagged with its origin server。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerNotification {
    /// 触发变化的 MCP Server 唯一身份 `bundle_id`（= manager 各映射的键）/ origin server's bundle_id。
    ///
    /// #127：改携 `bundle_id`（此前为 display 名，且注释误称其为「manager 映射的 key」——manager 的键
    /// 一直是 `bundle_id`）。定向重挂（`resources/updated{skill://…}` → 单 server restage）据此寻址；
    /// 用 display 名则同名 server 之间无从区分。
    pub server: BundleId,
    /// 变化种类 / change kind。
    pub kind: McpChangeKind,
}

/// 注入给单个 MCP 客户端的通知上报接缝（#106）/ per-client notification-forwarding seam。
///
/// `client_factory` 在创建客户端时注入：`bundle_id` 让客户端能给通知打上来源标签（客户端本身不知道自己
/// 的身份——见 [`super::utils::client_factory`]），`tx` 是喂给 Computer 单消费者任务的发送端。
#[derive(Debug, Clone)]
pub struct ClientNotifyCtx {
    /// 该客户端对应的 MCP Server 唯一身份 `bundle_id`（#127；非 display 名）/ this client's bundle_id。
    pub bundle_id: BundleId,
    /// 变化通知发送端（Computer 侧持有接收端）/ change-notification sender。
    pub tx: mpsc::UnboundedSender<McpServerNotification>,
}

impl ClientNotifyCtx {
    /// 构造一条 [`McpServerNotification`] 并非阻塞发送（channel 关闭时静默丢弃）/ build & send, drop on closed。
    pub fn notify(&self, kind: McpChangeKind) {
        let _ = self.tx.send(McpServerNotification {
            server: self.bundle_id.clone(),
            kind,
        });
    }
}

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
    ///
    /// `streamable` = 协议 §9.1 / Python `StreamableHttpServerConfig` 的**规范判别符**（跨 SDK 一致）；
    /// `http`/`HTTP` 为 Rust 历史别名。#113 S6 落盘时归一化写 `streamable`，故读端须接受之（否则重启不回读）。
    #[serde(alias = "http", alias = "HTTP", alias = "streamable")]
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

    /// 获取**显式** `bundle_id`（若配置了）/ Get the **explicit** bundle_id if configured。
    ///
    /// 返回 `None` 表示未显式配置——此时**唯一身份**须经 [`super::bundle_id::resolve_bundle_id`]（或
    /// [`derive_bundle_id`](super::bundle_id::derive_bundle_id)）从 `name` 缺省生成。**恒有值的身份**用
    /// [`resolve_bundle_id`](super::bundle_id::resolve_bundle_id)，本访问器只暴露原始显式字段（如用于落盘保真）。
    ///
    /// #130：返回 [`BundleId`] 而非 `&str`——身份不在此处退化为字符串（退化即混用的起点）。
    #[must_use]
    pub fn bundle_id(&self) -> Option<&BundleId> {
        match self {
            MCPServerConfig::Stdio(config) => config.bundle_id.as_ref(),
            MCPServerConfig::Sse(config) => config.bundle_id.as_ref(),
            MCPServerConfig::Http(config) => config.bundle_id.as_ref(),
        }
    }

    /// 设置 `bundle_id` 字段（**derive-on-load 物化用**，非回写配置源）/ set the bundle_id field。
    ///
    /// 协议 0.3.0 §connection-identity = **raw**：缺省生成须用**未渲染**连接身份。Computer 在 render 后把从
    /// **raw config**（占位字面）派生的 `bundle_id` stamp 到渲染后配置上，使 manager 不再从渲染后连接身份派生
    /// （避免无名 server 的 `${input:*}` 轮换致 bundle_id / exposed_tool_name 漂移）。仅改内存投影，**不写 mcp.json**。
    pub fn set_bundle_id(&mut self, bundle_id: Option<BundleId>) {
        match self {
            MCPServerConfig::Stdio(config) => config.bundle_id = bundle_id,
            MCPServerConfig::Sse(config) => config.bundle_id = bundle_id,
            MCPServerConfig::Http(config) => config.bundle_id = bundle_id,
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
///
/// `#[non_exhaustive]`：跨 crate 禁结构体字面量构造，须经 [`StdioServerConfig::new`]（协议 0.3.0
/// bundle_id 已算 breaking，一步到位杜绝未来加字段 source-break 外部消费者，rust-sdk#117）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
pub struct StdioServerConfig {
    /// 服务器名称（人类可读，非唯一身份）/ Server name (human-readable, not the unique identity)。
    pub name: ServerName,
    /// MCP Server 唯一标识（BundleID）。省略时由 `name` 经确定性算法缺省生成（[`super::bundle_id`]，
    /// **derive-on-load、不回写 mcp.json**）。
    ///
    /// #130：显式非法值（含 `.` / `__` / 空）在 **serde 反序列化的字段级**即判废（[`BundleId`] 构造即校验），
    /// **不再**是"注册边界报错"——`resolve_key` 的校验分支已随之删除。**无长度上限**（协议 §BundleID 未设，
    /// 由 `smcp` 的 `valid_bundle_id_has_no_length_cap` 专测守护），故不存在"越界"一说。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_id: Option<BundleId>,
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
///
/// `#[non_exhaustive]`：跨 crate 须经 [`SseServerConfig::new`]（见 [`StdioServerConfig`]，rust-sdk#117）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
pub struct SseServerConfig {
    /// 服务器名称（人类可读，非唯一身份）/ Server name (human-readable, not the unique identity)。
    pub name: ServerName,
    /// MCP Server 唯一标识（BundleID），省略时缺省生成（见 [`StdioServerConfig::bundle_id`]）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_id: Option<BundleId>,
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
///
/// `#[non_exhaustive]`：跨 crate 须经 [`HttpServerConfig::new`]（见 [`StdioServerConfig`]，rust-sdk#117）。
///
/// Without a static `Authorization` header, connections are anonymous-first and OAuth is admitted
/// only after a standards-compliant Bearer challenge and validated metadata. OAuth resource,
/// scopes, authorization server, and dynamic client registration are protocol-derived. The
/// removed `oauth`, `authPolicy`, and `auth_policy` serialized fields are rejected with a migration
/// diagnostic.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
pub struct HttpServerConfig {
    /// 服务器名称（人类可读，非唯一身份）/ Server name (human-readable, not the unique identity)。
    pub name: ServerName,
    /// MCP Server 唯一标识（BundleID），省略时缺省生成（见 [`StdioServerConfig::bundle_id`]）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_id: Option<BundleId>,
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
    // Removed OAuth configuration keys remain represented only as private deserialization guards.
    // This preserves the model's intentional tolerance for unrelated extension keys while making
    // these security-sensitive legacy keys fail loudly instead of being ignored by serde.
    #[serde(
        default,
        rename = "oauth",
        skip_serializing,
        deserialize_with = "reject_removed_oauth_config"
    )]
    _removed_oauth: (),
    #[serde(
        default,
        rename = "authPolicy",
        alias = "auth_policy",
        skip_serializing,
        deserialize_with = "reject_removed_auth_policy"
    )]
    _removed_auth_policy: (),
    /// HTTP服务器参数 / HTTP server parameters
    pub server_parameters: HttpServerParameters,
}

fn reject_removed_oauth_config<'de, D>(deserializer: D) -> Result<(), D::Error>
where
    D: Deserializer<'de>,
{
    let _ = IgnoredAny::deserialize(deserializer)?;
    Err(D::Error::custom(
        "the 'oauth' HTTP server configuration field is no longer supported; remove it to use automatic OAuth negotiation",
    ))
}

fn reject_removed_auth_policy<'de, D>(deserializer: D) -> Result<(), D::Error>
where
    D: Deserializer<'de>,
{
    let _ = IgnoredAny::deserialize(deserializer)?;
    Err(D::Error::custom(
        "the 'authPolicy'/'auth_policy' HTTP server configuration field is no longer supported; remove it to use automatic OAuth negotiation",
    ))
}

impl StdioServerConfig {
    /// 构造一个 stdio server 配置（其余字段取默认；`#[non_exhaustive]` 下跨 crate 唯一构造入口）。
    ///
    /// 缺省：`bundle_id = None`（触发缺省生成）、`disabled = false`、`forbidden_tools`/`tool_meta` 为空、
    /// `default_tool_meta`/`vrl`/`env_file` 为 `None`。字段均 `pub`，构造后可按需赋值。
    pub fn new(name: impl Into<ServerName>, server_parameters: StdioServerParameters) -> Self {
        Self {
            name: name.into(),
            bundle_id: None,
            disabled: false,
            forbidden_tools: Vec::new(),
            tool_meta: HashMap::new(),
            default_tool_meta: None,
            vrl: None,
            env_file: None,
            server_parameters,
        }
    }
}

impl SseServerConfig {
    /// 构造一个 SSE server 配置（其余字段取默认；见 [`StdioServerConfig::new`]）。
    pub fn new(name: impl Into<ServerName>, server_parameters: SseServerParameters) -> Self {
        Self {
            name: name.into(),
            bundle_id: None,
            disabled: false,
            forbidden_tools: Vec::new(),
            tool_meta: HashMap::new(),
            default_tool_meta: None,
            vrl: None,
            env_file: None,
            server_parameters,
        }
    }
}

impl HttpServerConfig {
    /// 构造一个 streamable-HTTP server 配置（其余字段取默认；见 [`StdioServerConfig::new`]）。
    pub fn new(name: impl Into<ServerName>, server_parameters: HttpServerParameters) -> Self {
        Self {
            name: name.into(),
            bundle_id: None,
            disabled: false,
            forbidden_tools: Vec::new(),
            tool_meta: HashMap::new(),
            default_tool_meta: None,
            vrl: None,
            env_file: None,
            _removed_oauth: (),
            _removed_auth_policy: (),
            server_parameters,
        }
    }
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

    /// Validate invariants that cannot be expressed by the enum's wire shape alone.
    pub fn validate(&self) -> Result<(), String> {
        match self {
            MCPServerInput::PickString(input) => input.validate(),
            _ => Ok(()),
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickStringInput {
    /// 输入ID / Input ID
    pub id: String,
    /// 描述 / Description
    pub description: String,
    /// 选项 / Options
    pub options: Vec<PickStringOption>,
    /// 默认值 / Default value
    pub default: Option<String>,
}

impl PickStringInput {
    /// Validate the PickString definition while deliberately allowing duplicate labels and values.
    pub fn validate(&self) -> Result<(), String> {
        if self.options.is_empty() {
            return Err("PickString options must contain at least one item".to_string());
        }
        for (index, option) in self.options.iter().enumerate() {
            if option.label.is_empty() {
                return Err(format!("PickString option {index} label must not be empty"));
            }
            if option.value.is_empty() {
                return Err(format!("PickString option {index} value must not be empty"));
            }
        }
        if let Some(default) = &self.default {
            if !self.options.iter().any(|option| option.value == *default) {
                return Err(format!(
                    "PickString default {default:?} must match at least one option value"
                ));
            }
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct PickStringInputWire {
    id: String,
    description: String,
    #[serde(default)]
    options: Vec<PickStringOption>,
    #[serde(skip_serializing_if = "Option::is_none")]
    default: Option<String>,
}

impl Serialize for PickStringInput {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.validate().map_err(serde::ser::Error::custom)?;
        PickStringInputWire {
            id: self.id.clone(),
            description: self.description.clone(),
            options: self.options.clone(),
            default: self.default.clone(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for PickStringInput {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = PickStringInputWire::deserialize(deserializer)?;
        let input = Self {
            id: wire.id,
            description: wire.description,
            options: wire.options,
            default: wire.default,
        };
        input.validate().map_err(serde::de::Error::custom)?;
        Ok(input)
    }
}

/// A PickString choice with an independent display label and runtime value.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PickStringOption {
    /// Human-readable label shown by clients.
    pub label: String,
    /// Stable value inserted into the rendered MCP server configuration.
    pub value: String,
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

    /// 设置 live 状态变化回调（#186 逐 MCP runtime 状态事件接线）/ set live state-change callback。
    ///
    /// **默认空实现**——仅传输类（stdio/sse/http）经
    /// [`BaseMCPClient`](crate::mcp_clients::base_client::BaseMCPClient) 委托覆写；未接线时
    /// live `ClientState` 变化（进程自退、传输断连等）不产生状态事件（管理器 remember 路径
    /// 之外的变化须靠本回调触发 `MCPServerManager::fire_projected_if_changed`）。
    fn set_state_change_callback(
        &self,
        _callback: Box<dyn Fn(ClientState, ClientState) + Send + Sync>,
    ) {
    }

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

/// 用户对 MCP Server 的启动意图 / User-requested MCP server activation state.
///
/// 该状态与传输连接正交：OAuth 授权尚未完成时，Server 仍可保持 `Started`，但连接状态为
/// [`MCPServerConnectionState::AuthorizationRequired`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MCPServerActivationState {
    /// 未启动或已被显式停止 / Not started or explicitly stopped.
    Stopped,
    /// 已接受启动请求 / Start request has been accepted.
    Started,
}

impl fmt::Display for MCPServerActivationState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stopped => write!(f, "stopped"),
            Self::Started => write!(f, "started"),
        }
    }
}

/// MCP Server 的数据面连接状态 / MCP server data-plane connection state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MCPServerConnectionState {
    /// 当前没有连接 / No current connection.
    Disconnected,
    /// 正在建立连接 / Establishing a connection.
    Connecting,
    /// 已连接，可提供 MCP 能力 / Connected and able to provide MCP capabilities.
    Connected,
    /// 连接被 OAuth 授权前置条件阻塞 / Connection is blocked on OAuth authorization.
    AuthorizationRequired,
    /// 最近一次连接尝试失败 / The latest connection attempt failed.
    Error,
}

impl fmt::Display for MCPServerConnectionState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disconnected => write!(f, "disconnected"),
            Self::Connecting => write!(f, "connecting"),
            Self::Connected => write!(f, "connected"),
            Self::AuthorizationRequired => write!(f, "authorization_required"),
            Self::Error => write!(f, "error"),
        }
    }
}

/// MCP Server 的正交运行时状态 / Orthogonal MCP server runtime status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MCPServerRuntimeStatus {
    /// 稳定身份键 / Stable identity key.
    pub bundle_id: BundleId,
    /// 展示名称 / Display name.
    pub name: ServerName,
    /// 控制面启动意图 / Control-plane activation intent.
    pub activation: MCPServerActivationState,
    /// 数据面连接状态 / Data-plane connection state.
    pub connection: MCPServerConnectionState,
}

impl MCPServerRuntimeStatus {
    /// 是否已接受启动请求 / Whether activation has been requested and retained.
    pub fn is_started(&self) -> bool {
        self.activation == MCPServerActivationState::Started
    }

    /// 是否已连接并可提供 MCP 能力 / Whether the data plane is connected.
    pub fn is_connected(&self) -> bool {
        self.connection == MCPServerConnectionState::Connected
    }
}

/// MCP客户端错误 / MCP client error
#[derive(Debug, Error)]
pub enum MCPClientError {
    /// 连接错误 / Connection error
    #[error("Connection error: {0}")]
    ConnectionError(String),
    /// Structured HTTP authentication negotiation result.
    #[error("HTTP authentication error: {0}")]
    HttpAuthentication(#[from] HttpAuthenticationError),
    /// 协议错误 / Protocol error
    #[error("Protocol error: {0}")]
    ProtocolError(String),
    /// MCP Server 未声明所需 capability（如 `resources`）→ 上层映射 4015 /
    /// MCP Server did not declare the required capability (e.g. `resources`) → mapped to 4015 upstream.
    #[error("Capability not supported: {0}")]
    CapabilityNotSupported(String),
    /// 上游工具调用错误（**保型**，供 AUTH-01 结构化分类）/ Upstream tool-call error, type preserved.
    ///
    /// 与 `ProtocolError(String)` 的区别：保留 rmcp [`rmcp::ServiceError`] 原始类型，使
    /// [`classify_auth_error`](crate::mcp_clients::auth_error::classify_auth_error) 能对
    /// `ServiceError::TransportSend` 做结构化 downcast（如 `StreamableHttpError::AuthRequired` → 4006），
    /// 不再依赖 rmcp `Display` 字面量——后者对最规范的 401+`WWW-Authenticate` 仅产出无状态码的
    /// `"Auth required"`，导致字符串分类器漏报（#150）。
    #[error("Call tool error: {0}")]
    ToolCallError(rmcp::ServiceError),
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

/// #161：逐 server 窗口枚举失败的**稳定错误类别**（小闭集，从 [`MCPClientError`] 投影）。
///
/// 供下游（如 tfrobot-client TFRC-75）在不解析错误文案的前提下区分「capability 缺失」「认证过期
/// （可引导用户重新授权）」「传输失败」等可操作状态。**进程内诊断类别，非 wire 错误码**——与
/// `smcp::ErrorCode::McpCapabilityNotSupported`（4015）语义对应但不绑数值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum WindowEnumerationErrorCategory {
    /// Server 未声明 `resources` capability（`CapabilityNotSupported` → 4015 语义，INT-04 #78
    /// 三传输统一）。与「成功空集」的区别即窗口枚举诊断的核心分叉。
    MissingResourcesCapability,
    /// 连接态/传输断开（枚举前 state 检查或传输失败）。
    Connection,
    /// HTTP 认证协商失败（OAuth 过期等——下游唯一「可引导用户操作恢复」的失败类）。
    Authentication,
    /// Server 返回协议错误/畸形响应。
    Protocol,
    /// 超时。
    Timeout,
    /// 其余（IO / JSON / 工具调用类等）。
    Other,
}

impl From<&MCPClientError> for WindowEnumerationErrorCategory {
    fn from(e: &MCPClientError) -> Self {
        match e {
            MCPClientError::CapabilityNotSupported(_) => Self::MissingResourcesCapability,
            MCPClientError::ConnectionError(_) => Self::Connection,
            MCPClientError::HttpAuthentication(_) => Self::Authentication,
            MCPClientError::ProtocolError(_) => Self::Protocol,
            MCPClientError::TimeoutError(_) => Self::Timeout,
            // `list_windows` 路径不产生 ToolCallError；Io/Json/Other 归 Other。
            MCPClientError::ToolCallError(_)
            | MCPClientError::IoError(_)
            | MCPClientError::JsonError(_)
            | MCPClientError::Other(_) => Self::Other,
        }
    }
}

/// #161：单个 server 的枚举失败诊断。**身份键 = `bundle_id`**（寻址/去重一律用它——协议
/// §身份正交性：`name` 允许碰撞）；`server_name` 仅展示。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowEnumerationFailure {
    pub bundle_id: BundleId,
    pub server_name: ServerName,
    pub category: WindowEnumerationErrorCategory,
    /// 安全消息 = [`MCPClientError`] 的 `Display`（该错误族不携带凭据——参见
    /// `HttpAuthenticationError` 的保安全设计注释）。
    pub message: String,
}

/// #161：结构化窗口枚举结果——进程内 SDK API 的返回体（**不上 wire、不持久化、无 UI/Robot/
/// Manager 字段**，验收⑥）。由 `MCPServerManager::list_windows_with_diagnostics` 产出，供下游
/// 区分「成功空集 / capability 缺失 / 全部失败 / 部分成功」四态（经 [`status`](Self::status)）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowEnumerationReport {
    /// 枚举成功的窗口：`(bundle_id, 展示名, resource)`，与 manager 的
    /// `list_windows_with_identity` 同形（后者即本字段的投影）。
    /// 部分失败**不丢**其余 server 的窗口。
    pub windows: Vec<(BundleId, ServerName, Resource)>,
    /// 本次尝试枚举的活跃 server 总数（= `active_clients` 快照长度）。
    pub servers_attempted: usize,
    /// **通过 resources 能力门的 server 数** = `servers_attempted` − `failures` 中 category 为
    /// [`MissingResourcesCapability`](WindowEnumerationErrorCategory::MissingResourcesCapability)
    /// 的条数。近似语义：client 层连接检查先于能力检查，连接失败的 server 其能力声明不可确知，
    /// 按「未因能力缺失被排除」计（不扣减）。
    pub servers_with_resources_capability: usize,
    /// 逐 server 失败（每 server 至多一条；成功 server 不出现）。
    pub failures: Vec<WindowEnumerationFailure>,
}

/// #161：枚举结果四态（+正常成功）推导，规则全序且全覆盖。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum WindowEnumerationStatus {
    /// 全部成功且有窗口。
    Success,
    /// 全部成功但窗口为空（**含 `servers_attempted == 0`**：零活跃 server；消费方可再查
    /// `servers_attempted` 细分「没有 server」与「有 server 但零窗口」）。
    SuccessEmpty,
    /// 所有 server 均未声明 `resources` capability（失败非空且全为能力缺失）。
    AllServersMissingCapability,
    /// 全部失败（零成功且非纯能力缺失——含混合失败类别）。
    AllServersFailed,
    /// 部分成功（≥1 server 成功 且 ≥1 失败）；其余 server 的窗口保留在 `windows`。
    PartialSuccess,
}

impl WindowEnumerationReport {
    /// 四态推导。成功 server 数 = `servers_attempted` − `failures.len()`（每 server 至多一条
    /// 失败，恒 ≥ 0）：failures 空 → `Success`/`SuccessEmpty`（按 `windows` 是否为空）；零成功
    /// 且失败全为能力缺失 → `AllServersMissingCapability`；零成功其余 → `AllServersFailed`；
    /// 有成功有失败 → `PartialSuccess`。
    pub fn status(&self) -> WindowEnumerationStatus {
        let succeeded = self.servers_attempted.saturating_sub(self.failures.len());
        if self.failures.is_empty() {
            return if self.windows.is_empty() {
                WindowEnumerationStatus::SuccessEmpty
            } else {
                WindowEnumerationStatus::Success
            };
        }
        if succeeded == 0 {
            let all_missing = self
                .failures
                .iter()
                .all(|f| f.category == WindowEnumerationErrorCategory::MissingResourcesCapability);
            return if all_missing {
                WindowEnumerationStatus::AllServersMissingCapability
            } else {
                WindowEnumerationStatus::AllServersFailed
            };
        }
        WindowEnumerationStatus::PartialSuccess
    }
}

/// Stable result categories for HTTP authentication negotiation.
///
/// Challenge values and provider response bodies are intentionally not retained so credentials
/// and provider diagnostics cannot leak through `Debug` or `Display`.
#[derive(Debug, Clone, Copy, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum HttpAuthenticationError {
    /// A valid OAuth protected-resource and authorization-server relationship was discovered.
    #[error("OAuth authorization is required")]
    OAuthRequired,
    /// A configured static Authorization header was rejected.
    #[error("static Authorization credentials were rejected")]
    StaticCredentialsRejected,
    /// The server requested Basic, Digest, or another unsupported authentication scheme.
    #[error("the server requested an unsupported HTTP authentication scheme")]
    UnsupportedChallenge,
    /// A Bearer challenge was present but OAuth metadata discovery or validation failed.
    #[error("OAuth metadata discovery failed")]
    OAuthDiscoveryFailed,
    /// The server returned 401 without a usable authentication challenge.
    #[error("the server rejected the anonymous request without a usable authentication challenge")]
    Unauthorized,
    /// The server denied the request without a valid OAuth insufficient-scope challenge.
    #[error("the server denied the HTTP MCP request")]
    Forbidden,
}

/// 便捷函数：创建 Resource / Convenience: create a Resource
pub fn make_resource(
    uri: impl Into<String>,
    name: impl Into<String>,
    description: Option<String>,
    mime_type: Option<String>,
) -> Resource {
    let mut resource = Resource::new(uri, name);
    resource.description = description;
    resource.mime_type = mime_type;
    resource
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
    fn pick_string_label_value_round_trip_and_duplicates_are_valid() {
        let raw = serde_json::json!({
            "id": "region",
            "type": "PickString",
            "description": "Region",
            "options": [
                {"label": "China", "value": "cn"},
                {"label": "China", "value": "cn"}
            ],
            "default": "cn"
        });
        let input: MCPServerInput = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(serde_json::to_value(input).unwrap(), raw);
    }

    #[test]
    fn pick_string_rejects_legacy_and_invalid_definitions() {
        let cases = [
            serde_json::json!({
                "id": "region", "type": "PickString", "description": "Region",
                "options": ["cn"], "default": "cn"
            }),
            serde_json::json!({
                "id": "region", "type": "PickString", "description": "Region",
                "options": []
            }),
            serde_json::json!({
                "id": "region", "type": "PickString", "description": "Region",
                "options": [{"label": "", "value": "cn"}]
            }),
            serde_json::json!({
                "id": "region", "type": "PickString", "description": "Region",
                "options": [{"label": "China", "value": ""}]
            }),
            serde_json::json!({
                "id": "region", "type": "PickString", "description": "Region",
                "options": [{"label": "China", "value": "cn"}], "default": "eu"
            }),
        ];
        for raw in cases {
            assert!(serde_json::from_value::<MCPServerInput>(raw).is_err());
        }
    }

    #[test]
    fn http_oauth_configuration_is_automatic_only() {
        let automatic = serde_json::json!({
            "name": "remote",
            "disabled": false,
            "forbidden_tools": [],
            "tool_meta": {},
            "default_tool_meta": null,
            "vrl": null,
            "server_parameters": {
                "url": "https://mcp.example/mcp",
                "headers": {}
            }
        });
        let automatic: HttpServerConfig = serde_json::from_value(automatic).unwrap();
        let encoded = serde_json::to_value(&automatic).unwrap();
        assert!(encoded.get("authPolicy").is_none());
        assert!(encoded.get("auth_policy").is_none());
        assert!(encoded.get("oauth").is_none());

        for (field, value) in [
            ("authPolicy", serde_json::json!("auto")),
            ("auth_policy", serde_json::json!("auto")),
            ("oauth", serde_json::json!({})),
        ] {
            let mut rejected = encoded.clone();
            rejected[field] = value;
            let error = serde_json::from_value::<HttpServerConfig>(rejected).unwrap_err();
            assert!(
                error.to_string().contains("no longer supported"),
                "unexpected error for {field}: {error}"
            );
        }
    }

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
        assert_eq!(resource.uri, "window://test");
        assert_eq!(resource.name, "Test");
        assert_eq!(resource.description, Some("desc".into()));
        assert!(resource.mime_type.is_none());
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
