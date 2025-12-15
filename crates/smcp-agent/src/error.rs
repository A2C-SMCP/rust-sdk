/*!
* 文件名: error
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: thiserror
* 描述: SMCP Agent错误类型定义 / SMCP Agent error type definitions
*/

use thiserror::Error;

/// SMCP Agent错误类型
#[derive(Error, Debug)]
pub enum SmcpAgentError {
    #[error("网络错误: {0}")]
    Network(#[from] Box<rust_socketio::Error>),

    #[error("超时错误")]
    Timeout,

    #[error("协议错误: req_id不匹配 (期望: {expected}, 实际: {actual})")]
    ReqIdMismatch { expected: String, actual: String },

    #[error("序列化错误: {0}")]
    Serialization(#[from] Box<serde_json::Error>),

    #[error("无效事件: {event}")]
    InvalidEvent { event: String },

    #[error("认证错误: {0}")]
    Authentication(String),

    #[error("连接错误: {0}")]
    Connection(String),

    #[error("内部错误: {0}")]
    Internal(String),
}

impl SmcpAgentError {
    pub fn invalid_event(event: impl Into<String>) -> Self {
        Self::InvalidEvent {
            event: event.into(),
        }
    }

    pub fn authentication(msg: impl Into<String>) -> Self {
        Self::Authentication(msg.into())
    }

    pub fn connection(msg: impl Into<String>) -> Self {
        Self::Connection(msg.into())
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        Self::Internal(msg.into())
    }
}

// 手动实现From trait以保持兼容性 / Manual From implementations for compatibility
impl From<rust_socketio::Error> for SmcpAgentError {
    fn from(err: rust_socketio::Error) -> Self {
        Self::Network(Box::new(err))
    }
}

impl From<serde_json::Error> for SmcpAgentError {
    fn from(err: serde_json::Error) -> Self {
        Self::Serialization(Box::new(err))
    }
}

pub type Result<T> = std::result::Result<T, SmcpAgentError>;
