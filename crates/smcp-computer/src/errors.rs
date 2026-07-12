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
    #[error("Input not found: {input_id}")]
    /// 输入项未找到 / Input not found
    InputNotFound { input_id: String },

    #[error("Server {server_name} is not active")]
    /// 服务器未激活 / Server not active
    ServerNotActive { server_name: String },

    #[error("MCP server not found: {0}")]
    /// 目标 MCP Server 未注册（`get_resources` → 处理器映射 4014）/ target MCP server not registered
    /// (`get_resources` → handler maps 4014)。对标 Python `MCPServerNotFoundError`。
    McpServerNotFound(String),

    #[error("MCP capability '{capability}' not supported by server '{server_name}'")]
    /// MCP Server 未声明所需 capability（`get_resources` → 处理器映射 4015）/ required capability not
    /// declared (`get_resources` → handler maps 4015)。对标 Python `MCPCapabilityNotSupportedError`。
    /// 结构化分流字段：`server_name`（值 = **bundle_id**，协议 0.3.0 #18）+ `capability` 供 handler 直接平铺为
    /// flat ErrorPayload 顶层 `mcp_server`/`capability`（`with_mcp_server`/`with_capability`），无需再解析字符串。
    McpCapabilityNotSupported {
        /// 目标 MCP Server 的 bundle_id（顶层 `mcp_server`）。
        server_name: String,
        /// 缺失的 capability 名（顶层 `capability`，如 `"resources"`）。
        capability: String,
    },

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

    #[error("Protocol version mismatch: {0}")]
    /// 协议版本握手不匹配 / Protocol-version handshake mismatch (HS-02 #22)。
    ///
    /// 连接 URL 携带 [`smcp::PROTOCOL_VERSION`] 后，服务端版本握手判定不兼容：HTTP 400 body
    /// 中携带 4008 [`smcp::ErrorPayload`]（polling 握手），经
    /// [`smcp::utils::handshake::build_protocol_version_error`] 映射为强类型
    /// [`smcp::ProtocolVersionError`]。镜像 Python `a2c_smcp/computer/socketio/client.py`。
    ProtocolVersionMismatch(#[from] smcp::ProtocolVersionError),

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

    #[error("Invalid state: {0}")]
    /// 无效状态 / Invalid state
    InvalidState(String),

    #[error("Render error: {0}")]
    /// 渲染错误 / Render error
    RenderError(String),

    #[error("Input resolution error: {0}")]
    /// D1 运行期 input/secret 解析错误（#112 S5）：必填 input 未解析且无默认值 → 结构化错误（**非仅日志**），
    /// 由 client 经 `RuntimeOptions.input_resolver` / `secret_resolver` 补录。SDK 不落盘明文值/secret。
    /// Structured input-resolution error surfaced instead of silently defaulting to an empty string。
    InputResolution(#[from] crate::inputs::runtime_resolver::InputResolutionError),

    #[error("Config persistence error: {0}")]
    /// #113 S6：SDK-owned config CRUD 落盘失败（只读 origin / synthesized bundled / 文件锁 / I/O / 损坏文件）。
    /// 消息由 [`crate::settings::config::ConfigCrudError`] 的 Display 派生——**只含写目标 / 路径 / 原因，无 secret
    /// 值**（落盘的是原始 `${input:*}` 引用，D1/§4.6.6）。runtime mutate（add_or_update/remove_server）经此报错。
    /// Config-layer persistence failure surfaced from `update_config`; carries no secret values。
    ConfigPersist(String),
}

impl From<Box<dyn std::error::Error + Send + Sync>> for ComputerError {
    fn from(err: Box<dyn std::error::Error + Send + Sync>) -> Self {
        ComputerError::RuntimeError(err.to_string())
    }
}

impl From<crate::mcp_clients::RenderError> for ComputerError {
    fn from(err: crate::mcp_clients::RenderError) -> Self {
        ComputerError::RenderError(err.to_string())
    }
}

impl ComputerError {
    /// 获取错误码 / Get error code
    /// 参考 A2C-SMCP 协议错误码规范 / Reference A2C-SMCP protocol error code spec
    pub fn error_code(&self) -> i32 {
        match self {
            // 工具相关错误 / Tool related errors
            ComputerError::ToolExecutionTimeout { .. } => 4004, // TOOL_TIMEOUT

            // 输入相关错误 / Input related errors
            ComputerError::InputNotFound { .. } => 404, // NOT_FOUND

            // 服务器相关错误 / Server related errors
            ComputerError::ServerNotActive { .. } => 404, // NOT_FOUND

            // MCP get_resources 路由错误 / MCP get_resources routing errors
            ComputerError::McpServerNotFound(_) => smcp::ErrorCode::McpServerNotFound.code(), // 4014
            ComputerError::McpCapabilityNotSupported { .. } => {
                smcp::ErrorCode::McpCapabilityNotSupported.code() // 4015
            }

            // 语法/验证错误 / Syntax/Validation errors
            ComputerError::VrlSyntaxError { .. } => 400, // BAD_REQUEST
            ComputerError::ValidationError(_) => 400,    // BAD_REQUEST
            ComputerError::InvalidConfiguration(_) => 400, // BAD_REQUEST
            ComputerError::RenderError(_) => 400,        // BAD_REQUEST
            ComputerError::InputResolution(_) => 400, // BAD_REQUEST（client 须补录 input/secret）

            // 连接错误 / Connection errors
            ComputerError::ConnectionError(_) => 500, // INTERNAL_ERROR
            ComputerError::TransportError(_) => 500,  // INTERNAL_ERROR
            ComputerError::SocketIoError(_) => 500,   // INTERNAL_ERROR

            // 协议版本握手不匹配 / Protocol version mismatch (4008)
            ComputerError::ProtocolVersionMismatch(_) => {
                smcp::ErrorCode::ProtocolVersionMismatch.code()
            }

            // 超时错误 / Timeout errors
            ComputerError::TimeoutError(_) => 408, // TIMEOUT

            // 权限错误 / Permission errors
            ComputerError::PermissionError(_) => 403, // FORBIDDEN

            // 协议错误 / Protocol errors
            ComputerError::ProtocolError(_) => 500, // INTERNAL_ERROR

            // 状态错误 / State errors
            ComputerError::InvalidState(_) => 400, // BAD_REQUEST

            // MCP客户端错误 / MCP client errors
            ComputerError::McpClientError(e) => e.error_code(),

            // IO和序列化错误 / IO and serialization errors
            ComputerError::IoError(_) => 500, // INTERNAL_ERROR
            ComputerError::SerializationError(_) => 400, // BAD_REQUEST

            // 运行时错误 / Runtime errors
            ComputerError::RuntimeError(_) => 500, // INTERNAL_ERROR

            // #113 S6：config 落盘错误（只读 origin / synthesized / I/O）/ config persistence errors
            ComputerError::ConfigPersist(_) => 400, // BAD_REQUEST（写目标不可写 / 非法实体）
        }
    }
}

impl McpClientError {
    /// 获取错误码 / Get error code
    pub fn error_code(&self) -> i32 {
        match self {
            McpClientError::NotConnected => 500,        // INTERNAL_ERROR
            McpClientError::ConnectionFailed(_) => 500, // INTERNAL_ERROR
            McpClientError::ConnectionError(_) => 500,  // INTERNAL_ERROR
            McpClientError::ToolCallFailed(_) => 4003,  // TOOL_EXECUTION_FAILED
            McpClientError::InvalidState(_) => 400,     // BAD_REQUEST
            McpClientError::ProcessError(_) => 500,     // INTERNAL_ERROR
            McpClientError::TimeoutError(_) => 408,     // TIMEOUT
            McpClientError::ProtocolError(_) => 500,    // INTERNAL_ERROR
            McpClientError::ToolError(_) => 4003,       // TOOL_EXECUTION_FAILED
            McpClientError::ConfigError(_) => 400,      // BAD_REQUEST
            McpClientError::InternalError(_) => 500,    // INTERNAL_ERROR
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_protocol_version_mismatch_from_payload() {
        // HS-02 #22: 4008 body → ProtocolVersionError → ComputerError::ProtocolVersionMismatch
        // 经 #[from] 自动转换；error_code() 返回 4008；Display 透传诊断字段。
        let body = r#"{"code":4008,"message":"Protocol version mismatch","server_version":"0.3.0","client_version":"0.2.0","min_supported":"0.3.0","max_supported":"0.3.999"}"#;
        let payload = smcp::utils::handshake::extract_4008_payload(body).expect("4008 应被识别");
        let pve = smcp::utils::handshake::build_protocol_version_error(&payload);
        let err: ComputerError = pve.into();
        assert_eq!(err.error_code(), 4008);
        match err {
            ComputerError::ProtocolVersionMismatch(e) => {
                assert_eq!(e.server_version.as_deref(), Some("0.3.0"));
                assert_eq!(e.client_version.as_deref(), Some("0.2.0"));
                let s = e.to_string();
                assert!(s.contains("server=0.3.0"));
                assert!(s.contains("client=0.2.0"));
            }
            other => panic!("expected ProtocolVersionMismatch, got {other:?}"),
        }
    }
}
