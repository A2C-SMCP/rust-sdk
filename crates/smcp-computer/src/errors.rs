/*!
* 文件名: errors.rs
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: thiserror
* 描述: Computer模块的错误定义 / Error definitions for Computer module
*/

use thiserror::Error;

/// Computer模块的Result类型别名 / Result type alias for Computer module
pub type ComputerResult<T> = Result<T, ComputerError>;

/// Computer模块的错误类型 / Error type for Computer module
#[derive(Debug, Error)]
pub enum ComputerError {
    #[error("Tool name duplicated: {tool_name} in servers: {servers:?}")]
    /// 工具名称重复 / Tool name duplicated
    ToolNameDuplicated {
        tool_name: String,
        servers: Vec<String>,
    },

    #[error("Input not found: {input_id}")]
    /// 输入项未找到 / Input not found
    InputNotFound { input_id: String },

    #[error("Server {server_name} is not active")]
    /// 服务器未激活 / Server not active
    ServerNotActive { server_name: String },

    #[error("VRL syntax error: {message}")]
    /// VRL语法错误 / VRL syntax error
    VrlSyntaxError { message: String },

    #[error("Tool execution timeout after {timeout}s")]
    /// 工具执行超时 / Tool execution timeout
    ToolExecutionTimeout { timeout: u64 },

    #[error("MCP client error: {0}")]
    /// MCP客户端错误 / MCP client error
    McpClientError(#[from] McpClientError),

    #[error("IO error: {0}")]
    /// IO错误 / IO error
    IoError(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    /// 序列化错误 / Serialization error
    SerializationError(#[from] serde_json::Error),

    #[error("Transport error: {0}")]
    /// 传输层错误 / Transport error
    TransportError(String),

    #[error("Invalid configuration: {0}")]
    /// 无效配置 / Invalid configuration
    InvalidConfiguration(String),

    #[error("Connection error: {0}")]
    /// 连接错误 / Connection error
    ConnectionError(String),

    #[error("Runtime error: {0}")]
    /// 运行时错误 / Runtime error
    RuntimeError(String),

    #[error("Permission error: {0}")]
    /// 权限错误 / Permission error
    PermissionError(String),

    #[error("Timeout error: {0}")]
    /// 超时错误 / Timeout error
    TimeoutError(String),

    #[error("Protocol error: {0}")]
    /// 协议错误 / Protocol error
    ProtocolError(String),

    #[error("Socket.IO error: {0}")]
    /// Socket.IO错误 / Socket.IO error
    SocketIoError(String),

    #[error("Validation error: {0}")]
    /// 验证错误 / Validation error
    ValidationError(String),
}

impl From<Box<dyn std::error::Error + Send + Sync>> for ComputerError {
    fn from(err: Box<dyn std::error::Error + Send + Sync>) -> Self {
        ComputerError::RuntimeError(err.to_string())
    }
}

/// MCP客户端错误 / MCP client error
#[derive(Debug, Error)]
pub enum McpClientError {
    #[error("Not connected to server")]
    /// 未连接到服务器 / Not connected
    NotConnected,

    #[error("Connection failed: {0}")]
    /// 连接失败 / Connection failed
    ConnectionFailed(String),

    #[error("Connection error: {0}")]
    /// 连接错误 / Connection error
    ConnectionError(String),

    #[error("Tool call failed: {0}")]
    /// 工具调用失败 / Tool call failed
    ToolCallFailed(String),

    #[error("Invalid state: {0}")]
    /// 无效状态 / Invalid state
    InvalidState(String),

    #[error("Process error: {0}")]
    /// 进程错误 / Process error
    ProcessError(String),

    #[error("Timeout error: {0}")]
    /// 超时错误 / Timeout error
    TimeoutError(String),

    #[error("Protocol error: {0}")]
    /// 协议错误 / Protocol error
    ProtocolError(String),

    #[error("Tool error: {0}")]
    /// 工具错误 / Tool error
    ToolError(String),

    #[error("Config error: {0}")]
    /// 配置错误 / Config error
    ConfigError(String),

    #[error("Internal error: {0}")]
    /// 内部错误 / Internal error
    InternalError(String),
}
