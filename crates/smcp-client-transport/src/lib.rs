//! Shared client-side Socket.IO connect + version-handshake error handling
//! 共享的客户端 Socket.IO 连接 + 版本握手错误处理
//!
//! Both the SMCP Agent and the SMCP Computer connect to the SMCP Server using
//! 两者（SMCP Agent 与 SMCP Computer）都通过 `tf-rust-socketio` 连接到 SMCP Server，
//! `tf-rust-socketio`. Connecting may fail because the server rejects the client's
//! 连接时可能因服务端拒绝客户端协议版本（4008/4900 握手）而失败。
//! protocol version (4008 / 4900 handshake). This crate centralises the logic to
//! 本 crate 集中实现了对连接错误的分类，以及在 WebSocket 握手被拒（4900）时
//! classify those connect errors and to fetch the authoritative 4008 payload via a
//! 通过单次受控的 polling 重连来获取权威 4008 负载的逻辑，
//! single controlled polling reconnect, so the two clients share one implementation.
//! 使两个客户端复用同一份实现。

use std::collections::HashMap;

use tf_rust_engineio::Error as EngineError;
use tf_rust_socketio::asynchronous::{Client, ClientBuilder};
use tf_rust_socketio::{Error as SocketError, TransportType};
use tracing::warn;

/// Result of classifying a connect error into version-handshake categories.
/// 将连接错误分类为版本握手类别的结果。
#[derive(Debug)]
pub enum ConnectErrorKind {
    /// An authoritative 4008 protocol-version payload was recovered from the error.
    /// 从错误中恢复出权威的 4008 协议版本负载。
    ProtocolVersion(smcp::ProtocolVersionError),
    /// The WebSocket handshake was closed with the version-rejected close code (4900).
    /// WebSocket 握手以版本拒绝关闭码（4900）被关闭。
    WsRejected4900,
    /// Any other (non-version) connect error.
    /// 任何其他（非版本）连接错误。
    Other,
}

/// A high-level connect error returned to callers.
/// 返回给调用方的高层连接错误。
///
/// Callers map this onto their own crate-specific error type.
/// 调用方将其映射到各自 crate 特定的错误类型。
#[derive(Debug)]
pub enum ConnectError {
    /// The server rejected the client's protocol version (authoritative 4008).
    /// 服务端拒绝了客户端的协议版本（权威 4008）。
    ProtocolVersion(smcp::ProtocolVersionError),
    /// A generic connection failure with a human-readable message.
    /// 带有可读消息的通用连接失败。
    Connection(String),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::ProtocolVersion(pve) => {
                write!(f, "protocol version mismatch: {pve}")
            }
            ConnectError::Connection(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for ConnectError {}

/// Classify a connect error into version-handshake categories.
/// 将连接错误分类为版本握手类别。
///
/// `tf_rust_socketio::Error` wraps engine.io errors; we inspect the wrapped error
/// `tf_rust_socketio::Error` 封装 engine.io 错误；我们检查被封装的错误以检测
/// to detect the 4008 (HTTP 400 + body) and 4900 (WebSocket close) handshake signals.
/// 4008（HTTP 400 + body）与 4900（WebSocket 关闭）握手信号。
///
/// Pure (no I/O, no side effects); matched in order, mirroring the Python client.
/// 纯函数（不触网、无副作用），按顺序匹配，镜像 Python 客户端握手处理。
pub fn classify_connect_error(err: &SocketError) -> ConnectErrorKind {
    if let SocketError::IncompleteResponseFromEngineIo(engine_err) = err {
        match engine_err {
            EngineError::HttpErrorWithBody { status, body } if *status == 400 => {
                if let Some(payload) = smcp::utils::handshake::extract_4008_payload(body) {
                    return ConnectErrorKind::ProtocolVersion(
                        smcp::utils::handshake::build_protocol_version_error(&payload),
                    );
                }
            }
            EngineError::WebsocketClosed { code, .. }
                if i32::from(*code) == smcp::WS_VERSION_HANDSHAKE_REJECTED_CLOSE_CODE =>
            {
                return ConnectErrorKind::WsRejected4900;
            }
            _ => {}
        }
    }
    ConnectErrorKind::Other
}

/// Connect using the supplied builder and classify any version-handshake errors.
/// 使用给定的 builder 连接，并对版本握手错误进行分类。
///
/// On success returns the connected client. On a 4008 error returns a typed
/// 成功时返回已连接的客户端。遇到 4008 错误时返回带类型的协议版本错误；
/// protocol-version error; on a 4900 WebSocket rejection it performs a single
/// 遇到 4900 WebSocket 拒绝时，执行单次受控的 polling 重连以获取权威 4008 负载；
/// controlled polling reconnect to recover the authoritative 4008 payload; any
/// 其他错误则返回通用连接错误。
/// other failure is reported as a generic connection error.
pub async fn connect_and_classify(
    builder: ClientBuilder,
    handshake_url: &str,
    namespace: &str,
    auth: Option<serde_json::Value>,
    headers: HashMap<String, String>,
) -> Result<Client, ConnectError> {
    match builder.connect().await {
        Ok(client) => Ok(client),
        Err(e) => match classify_connect_error(&e) {
            ConnectErrorKind::ProtocolVersion(pve) => Err(ConnectError::ProtocolVersion(pve)),
            ConnectErrorKind::WsRejected4900 => {
                warn!(
                    "WebSocket handshake rejected with close code {} (protocol version); \
                     doing a single controlled polling re-fetch for the authoritative 4008 body",
                    smcp::WS_VERSION_HANDSHAKE_REJECTED_CLOSE_CODE
                );
                let pve = fetch_4008_via_polling(handshake_url, namespace, auth, headers).await;
                Err(ConnectError::ProtocolVersion(pve))
            }
            ConnectErrorKind::Other => {
                Err(ConnectError::Connection(format!("Failed to connect: {e}")))
            }
        },
    }
}

/// Build the fallback protocol-version error used when an authoritative 4008
/// 当无法获取权威 4008 负载时使用的回退协议版本错误构造器。
/// payload cannot be obtained.
///
/// Single source of truth for the 4900-fallback message so the close code is not
/// 作为 4900 回退消息的唯一来源，避免硬编码关闭码字面量或重复消息字符串。
/// hardcoded and the literal is not duplicated.
fn fallback_4900_error() -> smcp::ProtocolVersionError {
    smcp::ProtocolVersionError::new(format!(
        "WebSocket handshake rejected with close code {} (protocol version); authoritative 4008 unavailable",
        smcp::WS_VERSION_HANDSHAKE_REJECTED_CLOSE_CODE
    ))
}

/// Single controlled polling reconnect to obtain the authoritative 4008 payload.
/// 单次受控的 polling 重连以获取权威 4008 负载。
///
/// This performs exactly one reconnect over `TransportType::Polling` (no loop).
/// 仅通过 `TransportType::Polling` 执行一次重连（无循环）。
///
/// `#[doc(hidden)] pub`：仅为跨 crate 集成测试暴露（验证 #85 的 4900→polling 重连**重放 auth dict**），
/// 非稳定公共 API，调用方请走 [`connect_and_classify`]。Exposed (doc-hidden) only so cross-crate
/// integration tests can assert the 4900→polling reconnect replays the `auth` dict (#85); not a
/// stable public API — callers should use [`connect_and_classify`].
#[doc(hidden)]
pub async fn fetch_4008_via_polling(
    handshake_url: &str,
    namespace: &str,
    auth: Option<serde_json::Value>,
    headers: HashMap<String, String>,
) -> smcp::ProtocolVersionError {
    let mut builder = ClientBuilder::new(handshake_url).transport_type(TransportType::Polling);
    if !namespace.is_empty() {
        builder = builder.namespace(namespace);
    }
    if let Some(auth_value) = auth {
        builder = builder.auth(auth_value);
    }
    for (key, value) in headers {
        builder = builder.opening_header(key, value);
    }

    match builder.connect().await {
        Ok(client) => {
            // Unexpected success; we cannot extract a payload. Disconnect the live
            // 意外成功；无法提取负载。断开存活的客户端以避免泄漏连接及其衍生的重连任务，
            // client to avoid leaking the connection (and its spawned reconnect task)
            // before synthesizing a generic fallback error.
            // 然后合成一个通用回退错误。
            let _ = client.disconnect().await;
            fallback_4900_error()
        }
        Err(e) => match classify_connect_error(&e) {
            ConnectErrorKind::ProtocolVersion(pve) => pve,
            _ => fallback_4900_error(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a socket.io connect error wrapping the given engine.io error.
    /// 构造一个包裹给定 engine.io 错误的 socket.io 连接错误。
    ///
    /// `tf_rust_engineio::Error` is `#[non_exhaustive]`, so external crates cannot
    /// construct its variants directly through the public constructor — but the
    /// `SocketError::from(EngineError)` (`IncompleteResponseFromEngineIo`) `#[from]`
    /// path is available and is exactly what `classify_connect_error` inspects.
    fn socket_err(engine: EngineError) -> SocketError {
        SocketError::from(engine)
    }

    #[test]
    fn classify_http_400_with_4008_is_protocol_version() {
        let body = r#"{"code":4008,"message":"Protocol version mismatch","server_version":"0.3.0","client_version":"0.2.0","min_supported":"0.3.0","max_supported":"0.3.999"}"#;
        let err = socket_err(EngineError::HttpErrorWithBody {
            status: 400,
            body: body.to_string(),
        });
        match classify_connect_error(&err) {
            ConnectErrorKind::ProtocolVersion(pve) => {
                assert_eq!(pve.server_version.as_deref(), Some("0.3.0"));
                assert_eq!(pve.client_version.as_deref(), Some("0.2.0"));
            }
            other => panic!("expected ProtocolVersion, got {other:?}"),
        }
    }

    #[test]
    fn classify_http_400_without_4008_is_other() {
        let err = socket_err(EngineError::HttpErrorWithBody {
            status: 400,
            body: r#"{"code":400,"message":"Missing a2c_version"}"#.to_string(),
        });
        assert!(matches!(
            classify_connect_error(&err),
            ConnectErrorKind::Other
        ));
    }

    #[test]
    fn classify_ws_closed_4900_is_ws_rejected() {
        let err = socket_err(EngineError::WebsocketClosed {
            code: smcp::WS_VERSION_HANDSHAKE_REJECTED_CLOSE_CODE as u16,
            reason: "protocol version".to_string(),
        });
        assert!(matches!(
            classify_connect_error(&err),
            ConnectErrorKind::WsRejected4900
        ));
    }

    #[test]
    fn classify_ws_closed_other_is_other() {
        let err = socket_err(EngineError::WebsocketClosed {
            code: 1000,
            reason: "normal".to_string(),
        });
        assert!(matches!(
            classify_connect_error(&err),
            ConnectErrorKind::Other
        ));
    }

    #[test]
    fn classify_non_400_is_other() {
        let err = socket_err(EngineError::HttpErrorWithBody {
            status: 500,
            body: r#"{"code":4008}"#.to_string(),
        });
        assert!(matches!(
            classify_connect_error(&err),
            ConnectErrorKind::Other
        ));
    }
}
