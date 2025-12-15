/*!
* 文件名: model.rs
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: serde, async-trait
* 描述: MCP客户端模型定义 / MCP client model definitions
*/

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A2C工具元数据键 / A2C tool metadata key
pub const A2C_TOOL_META: &str = "a2c_tool_meta";
/// A2C VRL转换结果键 / A2C VRL transformed result key
pub const A2C_VRL_TRANSFORMED: &str = "a2c_vrl_transformed";

/// 工具元数据 / Tool metadata
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ToolMeta {
    /// 是否自动应用 / Auto apply
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_apply: Option<bool>,
    /// 工具别名 / Tool alias
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// 工具标签 / Tool tags
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    /// 返回值对象映射器 / Return object mapper
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ret_object_mapper: Option<HashMap<String, String>>,
    /// 其他扩展字段 / Other extension fields
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}


/// MCP服务器配置基类 / Base MCP server configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaseMCPServerConfig {
    /// 服务器名称 / Server name
    pub name: String,
    /// 是否禁用 / Disabled
    #[serde(default)]
    pub disabled: bool,
    /// 禁用工具列表 / Forbidden tools
    #[serde(default)]
    pub forbidden_tools: Vec<String>,
    /// 工具元数据 / Tool metadata
    #[serde(default)]
    pub tool_meta: HashMap<String, ToolMeta>,
    /// 默认工具元数据 / Default tool metadata
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_tool_meta: Option<ToolMeta>,
    /// VRL脚本 / VRL script
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vrl: Option<String>,
}

/// Stdio服务器配置 / Stdio server configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename = "stdio")]
pub struct StdioServerConfig {
    /// 基础配置 / Base configuration
    #[serde(flatten)]
    pub base: BaseMCPServerConfig,
    /// 服务器参数 / Server parameters
    pub server_parameters: StdioServerParameters,
}

/// SSE服务器配置 / SSE server configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename = "sse")]
pub struct SseServerConfig {
    /// 基础配置 / Base configuration
    #[serde(flatten)]
    pub base: BaseMCPServerConfig,
    /// 服务器参数 / Server parameters
    pub server_parameters: SseServerParameters,
}

/// Streamable HTTP服务器配置 / Streamable HTTP server configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename = "streamable")]
pub struct StreamableHttpServerConfig {
    /// 基础配置 / Base configuration
    #[serde(flatten)]
    pub base: BaseMCPServerConfig,
    /// 服务器参数 / Server parameters
    pub server_parameters: StreamableHttpParameters,
}

/// MCP服务器配置枚举 / MCP server configuration enum
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum MCPServerConfig {
    /// Stdio配置 / Stdio config
    #[serde(rename = "stdio")]
    Stdio(StdioServerConfig),
    /// SSE配置 / SSE config
    #[serde(rename = "sse")]
    Sse(SseServerConfig),
    /// Streamable HTTP配置 / Streamable HTTP config
    #[serde(rename = "streamable")]
    StreamableHttp(StreamableHttpServerConfig),
}

impl MCPServerConfig {
    /// 获取服务器名称 / Get server name
    pub fn name(&self) -> &str {
        match self {
            MCPServerConfig::Stdio(config) => &config.base.name,
            MCPServerConfig::Sse(config) => &config.base.name,
            MCPServerConfig::StreamableHttp(config) => &config.base.name,
        }
    }

    /// 获取基础配置 / Get base configuration
    pub fn base(&self) -> &BaseMCPServerConfig {
        match self {
            MCPServerConfig::Stdio(config) => &config.base,
            MCPServerConfig::Sse(config) => &config.base,
            MCPServerConfig::StreamableHttp(config) => &config.base,
        }
    }

    /// 是否禁用 / Is disabled
    pub fn is_disabled(&self) -> bool {
        self.base().disabled
    }

    /// 获取禁用工具列表 / Get forbidden tools
    pub fn forbidden_tools(&self) -> &[String] {
        &self.base().forbidden_tools
    }

    /// 获取工具元数据 / Get tool metadata
    pub fn tool_meta(&self) -> &HashMap<String, ToolMeta> {
        &self.base().tool_meta
    }

    /// 获取默认工具元数据 / Get default tool metadata
    pub fn default_tool_meta(&self) -> Option<&ToolMeta> {
        self.base().default_tool_meta.as_ref()
    }

    /// 获取VRL脚本 / Get VRL script
    pub fn vrl(&self) -> Option<&str> {
        self.base().vrl.as_deref()
    }
}

/// Stdio服务器参数 / Stdio server parameters
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StdioServerParameters {
    /// 命令 / Command
    pub command: String,
    /// 参数 / Arguments
    #[serde(default)]
    pub args: Vec<String>,
    /// 环境变量 / Environment variables
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// 工作目录 / Working directory
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// SSE服务器参数 / SSE server parameters
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SseServerParameters {
    /// URL / URL
    pub url: String,
    /// 头部 / Headers
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

/// Streamable HTTP服务器参数 / Streamable HTTP server parameters
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamableHttpParameters {
    /// URL / URL
    pub url: String,
    /// 头部 / Headers
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

/// MCP服务器输入基类 / Base MCP server input
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MCPServerInputBase {
    /// 输入ID / Input ID
    pub id: String,
    /// 描述 / Description
    pub description: String,
}

/// 字符串输入 / String input
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename = "promptString")]
pub struct MCPServerPromptStringInput {
    /// 基础信息 / Base information
    #[serde(flatten)]
    pub base: MCPServerInputBase,
    /// 默认值 / Default value
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// 是否密码 / Is password
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<bool>,
}

/// 选择输入 / Pick input
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename = "pickString")]
pub struct MCPServerPickStringInput {
    /// 基础信息 / Base information
    #[serde(flatten)]
    pub base: MCPServerInputBase,
    /// 选项 / Options
    pub options: Vec<String>,
    /// 默认值 / Default value
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

/// 命令输入 / Command input
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename = "command")]
pub struct MCPServerCommandInput {
    /// 基础信息 / Base information
    #[serde(flatten)]
    pub base: MCPServerInputBase,
    /// 命令 / Command
    pub command: String,
    /// 参数 / Arguments
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<HashMap<String, String>>,
}

/// MCP服务器输入枚举 / MCP server input enum
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum MCPServerInput {
    /// 字符串输入 / String input
    #[serde(rename = "promptString")]
    PromptString(MCPServerPromptStringInput),
    /// 选择输入 / Pick input
    #[serde(rename = "pickString")]
    PickString(MCPServerPickStringInput),
    /// 命令输入 / Command input
    #[serde(rename = "command")]
    Command(MCPServerCommandInput),
}

impl MCPServerInput {
    /// 获取输入ID / Get input ID
    pub fn id(&self) -> &str {
        match self {
            MCPServerInput::PromptString(input) => &input.base.id,
            MCPServerInput::PickString(input) => &input.base.id,
            MCPServerInput::Command(input) => &input.base.id,
        }
    }

    /// 获取描述 / Get description
    pub fn description(&self) -> &str {
        match self {
            MCPServerInput::PromptString(input) => &input.base.description,
            MCPServerInput::PickString(input) => &input.base.description,
            MCPServerInput::Command(input) => &input.base.description,
        }
    }
}
