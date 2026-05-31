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
    Network(#[from] Box<tf_rust_socketio::Error>),

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

    /// 协议版本握手不匹配 / Protocol-version handshake mismatch (HS-02 #22)。
    ///
    /// 连接 URL 携带 [`smcp::PROTOCOL_VERSION`] 后，服务端版本握手判定不兼容：HTTP 400 body 中
    /// 携带 4008 [`smcp::ErrorPayload`]（polling 握手），经
    /// [`smcp::utils::handshake::build_protocol_version_error`] 映射为强类型
    /// [`smcp::ProtocolVersionError`]。镜像 Python `a2c_smcp/agent/client.py` 抛 `ProtocolVersionError`。
    #[error("协议版本不匹配: {0}")]
    ProtocolVersionMismatch(#[from] smcp::ProtocolVersionError),

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
impl From<tf_rust_socketio::Error> for SmcpAgentError {
    fn from(err: tf_rust_socketio::Error) -> Self {
        Self::Network(Box::new(err))
    }
}

impl From<serde_json::Error> for SmcpAgentError {
    fn from(err: serde_json::Error) -> Self {
        Self::Serialization(Box::new(err))
    }
}

pub type Result<T> = std::result::Result<T, SmcpAgentError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_types() {
        let err = SmcpAgentError::invalid_event("notify:test");
        assert!(matches!(err, SmcpAgentError::InvalidEvent { .. }));

        let err = SmcpAgentError::authentication("Invalid token");
        assert!(matches!(err, SmcpAgentError::Authentication(_)));

        let err = SmcpAgentError::ReqIdMismatch {
            expected: "abc".to_string(),
            actual: "def".to_string(),
        };
        assert!(matches!(err, SmcpAgentError::ReqIdMismatch { .. }));
    }

    #[test]
    fn test_protocol_version_mismatch_from_payload() {
        // HS-02 #22: 4008 body → ProtocolVersionError → SmcpAgentError::ProtocolVersionMismatch
        // 经 #[from] 自动转换；Display 透传诊断字段。
        let body = r#"{"code":4008,"message":"Protocol version mismatch","server_version":"0.3.0","client_version":"0.2.0","min_supported":"0.3.0","max_supported":"0.3.999"}"#;
        let payload = smcp::utils::handshake::extract_4008_payload(body).expect("4008 应被识别");
        let pve = smcp::utils::handshake::build_protocol_version_error(&payload);
        let err: SmcpAgentError = pve.into();
        match err {
            SmcpAgentError::ProtocolVersionMismatch(e) => {
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
