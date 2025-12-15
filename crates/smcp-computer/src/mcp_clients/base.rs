/*!
* 文件名: base.rs
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: async-trait, serde_json
* 描述: MCP客户端基础trait / MCP client base trait
*/

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

/// MCP客户端状态 / MCP client state
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpClientState {
    /// 已初始化 / Initialized
    Initialized,
    /// 已连接 / Connected
    Connected,
    /// 已断开 / Disconnected
    Disconnected,
    /// 错误状态 / Error
    Error,
}

impl fmt::Display for McpClientState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            McpClientState::Initialized => write!(f, "initialized"),
            McpClientState::Connected => write!(f, "connected"),
            McpClientState::Disconnected => write!(f, "disconnected"),
            McpClientState::Error => write!(f, "error"),
        }
    }
}

/// MCP工具定义 / MCP tool definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpTool {
    /// 工具名称 / Tool name
    pub name: String,
    /// 工具描述 / Tool description
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// 输入模式 / Input schema
    pub input_schema: Value,
    /// 输出模式 / Output schema
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
    /// 元数据 / Metadata
    #[serde(flatten)]
    pub meta: Value,
}

/// MCP资源定义 / MCP resource definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpResource {
    /// 资源URI / Resource URI
    pub uri: String,
    /// 资源名称 / Resource name
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// 资源描述 / Resource description
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// MIME类型 / MIME type
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// 元数据 / Metadata
    #[serde(flatten)]
    pub meta: Value,
}

/// MCP工具调用结果 / MCP tool call result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpCallToolResult {
    /// 是否错误 / Is error
    #[serde(rename = "isError")]
    pub is_error: bool,
    /// 内容列表 / Content list
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<McpContent>>,
    /// 元数据 / Metadata
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

/// MCP内容块 / MCP content block
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum McpContent {
    /// 文本内容 / Text content
    #[serde(rename = "text")]
    Text {
        text: String,
    },
    /// 图片内容 / Image content
    #[serde(rename = "image")]
    Image {
        data: String,
        mime_type: String,
    },
    /// 资源内容 / Resource content
    #[serde(rename = "resource")]
    Resource {
        uri: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        blob: Option<String>,
    },
}

/// MCP客户端trait / MCP client trait
#[async_trait]
pub trait McpClient: Send + Sync {
    /// 获取客户端状态 / Get client state
    fn state(&self) -> McpClientState;

    /// 连接到MCP服务器 / Connect to MCP server
    async fn connect(&mut self) -> Result<(), crate::errors::McpClientError>;

    /// 断开连接 / Disconnect
    async fn disconnect(&mut self) -> Result<(), crate::errors::McpClientError>;

    /// 获取可用工具列表 / Get available tools
    async fn list_tools(&self) -> Result<Vec<McpTool>, crate::errors::McpClientError>;

    /// 调用工具 / Call tool
    async fn call_tool(
        &self,
        tool_name: &str,
        params: Value,
    ) -> Result<McpCallToolResult, crate::errors::McpClientError>;

    /// 列出窗口资源 / List window resources
    async fn list_windows(&self) -> Result<Vec<McpResource>, crate::errors::McpClientError>;

    /// 获取窗口详情 / Get window detail
    async fn get_window_detail(
        &self,
        resource: &McpResource,
    ) -> Result<McpCallToolResult, crate::errors::McpClientError>;
}
