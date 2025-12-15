/*!
* 文件名: errors.rs
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: thiserror
* 描述: MCP服务器管理器的错误定义 / Error definitions for MCP server manager
*/

use thiserror::Error;

/// MCP服务器管理器错误 / MCP server manager error
#[derive(Debug, Error)]
pub enum ManagerError {
    #[error("Tool name duplicated: {tool_name} in servers: {servers:?}")]
    /// 工具名称重复 / Tool name duplicated
    ToolNameDuplicated {
        tool_name: String,
        servers: Vec<String>,
    },

    #[error("Server not found: {server_name}")]
    /// 服务器未找到 / Server not found
    ServerNotFound { server_name: String },

    #[error("Server already exists: {server_name}")]
    /// 服务器已存在 / Server already exists
    ServerAlreadyExists { server_name: String },

    #[error("Tool not found: {tool_name}")]
    /// 工具未找到 / Tool not found
    ToolNotFound { tool_name: String },

    #[error("Tool is disabled: {tool_name}")]
    /// 工具已禁用 / Tool is disabled
    ToolDisabled { tool_name: String },

    #[error("Invalid server configuration: {0}")]
    /// 无效的服务器配置 / Invalid server configuration
    InvalidConfiguration(String),

    #[error("Client error: {0}")]
    /// 客户端错误 / Client error
    ClientError(String),
}
