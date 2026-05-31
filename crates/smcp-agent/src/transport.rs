/*!
* 文件名: transport
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: tf_rust_socketio, tokio
* 描述: SMCP Agent传输层实现 / SMCP Agent transport layer implementation
*/

use crate::error::{Result, SmcpAgentError};
use futures_util::FutureExt;
use serde_json::Value;
use smcp::events::*;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tf_rust_engineio::Error as EngineError;
use tf_rust_socketio::{
    asynchronous::{Client, ClientBuilder},
    Error as SocketError, Event, Payload, TransportType,
};
use tokio::sync::{mpsc, oneshot, Mutex};
use tracing::{debug, error, info, warn};

/// 事件处理器类型
pub type EventHandler = Box<dyn FnMut(Payload, Client) + Send + Sync>;

/// 连接错误的版本握手分类 / Classification of a connect error for version handshake (HS-02 #22)。
///
/// 纯函数 [`classify_connect_error`] 的产物，便于在不构造真实网络错误的前提下单测分类逻辑。
/// Produced by the pure [`classify_connect_error`] so the branch logic is unit-testable without a
/// live network error.
#[derive(Debug)]
enum ConnectErrorKind {
    /// polling 握手收到 HTTP 400 + 4008 body → 已解析出强类型版本错误。
    /// Polling handshake got HTTP 400 + a 4008 body → typed version error already parsed.
    ProtocolVersion(smcp::ProtocolVersionError),
    /// WS 握手被 4900 close code 拒绝（无结构化 body）→ 需改 polling 取权威 4008。
    /// WS handshake rejected with the 4900 close code (no structured body) → must re-fetch the
    /// authoritative 4008 over polling.
    WsRejected4900,
    /// 其它错误 → 走通用连接错误路径 / Anything else → generic connection error path。
    Other,
}

/// 把 socket.io 连接错误分类到版本握手语义 / Classify a socket.io connect error (HS-02 #22)。
///
/// 纯函数（不触网、无副作用），按以下顺序匹配，镜像 Python `a2c_smcp` 客户端握手处理：
/// Pure (no I/O, no side effects); matched in order, mirroring the Python client handshake:
/// 1. `IncompleteResponseFromEngineIo(HttpErrorWithBody { status: 400, body })` 且
///    [`extract_4008_payload`](smcp::utils::handshake::extract_4008_payload) 命中 → [`ConnectErrorKind::ProtocolVersion`]；
/// 2. `IncompleteResponseFromEngineIo(WebsocketClosed { code })` 且
///    `code == `[`WS_VERSION_HANDSHAKE_REJECTED_CLOSE_CODE`](smcp::WS_VERSION_HANDSHAKE_REJECTED_CLOSE_CODE)（4900）→ [`ConnectErrorKind::WsRejected4900`]；
/// 3. 其它（含 400 但 body 非 4008）→ [`ConnectErrorKind::Other`]。
fn classify_connect_error(err: &SocketError) -> ConnectErrorKind {
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

/// 通知事件消息
#[derive(Debug, Clone)]
pub enum NotificationMessage {
    EnterOffice(smcp::EnterOfficeNotification),
    LeaveOffice(smcp::LeaveOfficeNotification),
    UpdateConfig(smcp::UpdateMCPConfigNotification),
    UpdateToolList(smcp::UpdateToolListNotification),
    UpdateDesktop(String), // computer name
}

/// Socket.IO传输层
pub struct SocketIoTransport {
    client: Client,
    namespace: String,
}

impl SocketIoTransport {
    /// 创建新的传输层实例
    pub async fn connect(
        url: &str,
        namespace: &str,
        auth: Option<Value>,
        headers: HashMap<String, String>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<NotificationMessage>)> {
        info!(
            "Connecting to SMCP server at {} with namespace {}",
            url, namespace
        );

        // HS-02 #22: 在连接 URL 注入权威 a2c_version（丢弃调用方自带值，防版本漂移），
        // 使服务端 HTTP 握手中间件能在 Socket.IO 业务层之前完成版本协商。
        // HS-02 #22: inject the authoritative a2c_version into the connection URL so the server's
        // HTTP handshake middleware can negotiate the version before the Socket.IO layer.
        let handshake_url =
            smcp::utils::handshake::build_handshake_url(url, smcp::PROTOCOL_VERSION)
                .map_err(|e| SmcpAgentError::connection(format!("Invalid handshake URL: {}", e)))?;

        let (_tx, rx) = mpsc::unbounded_channel();

        // 连接服务器（polling-first，分类版本握手错误）
        let client =
            Self::connect_polling_first(&handshake_url, namespace, auth, headers, None).await?;

        // 等待一小段时间确保 Socket.IO namespace 连接完全建立
        // Wait for Socket.IO namespace connection to be fully established
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        info!(
            "Connected to SMCP server at {} with namespace {}",
            url, namespace
        );

        Ok((
            Self {
                client,
                namespace: namespace.to_string(),
            },
            rx,
        ))
    }

    /// 创建新的传输层实例并注册事件处理器
    pub async fn connect_with_handlers(
        url: &str,
        namespace: &str,
        auth: Option<Value>,
        headers: HashMap<String, String>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<NotificationMessage>)> {
        info!(
            "Connecting to SMCP server at {} with namespace {}",
            url, namespace
        );

        // HS-02 #22: 注入权威 a2c_version（见 [`SocketIoTransport::connect`] 注释）。
        let handshake_url =
            smcp::utils::handshake::build_handshake_url(url, smcp::PROTOCOL_VERSION)
                .map_err(|e| SmcpAgentError::connection(format!("Invalid handshake URL: {}", e)))?;

        let mut builder = ClientBuilder::new(&handshake_url);

        // HS-02 #22: polling-first（先 HTTP polling 握手，可被服务端 400+4008 body 拦截，
        // 失败时再升级 WebSocket）。⚠️ 不可用 WS-only（TransportType::Websocket）——会绕过服务端
        // HTTP 版本握手中间件，使版本不兼容无法被感知。
        // HS-02 #22: polling-first (HTTP polling handshake can be intercepted by the server's
        // 400 + 4008 body, then upgrades to WebSocket). MUST NOT use WS-only
        // (TransportType::Websocket) — it bypasses the server's HTTP version handshake gate.
        builder = builder.transport_type(TransportType::Any);

        // 注册on_any处理器来捕获所有事件
        let (tx, rx) = mpsc::unbounded_channel();
        let tx = Arc::new(tx);

        builder = builder.on_any(move |event, payload, _client| {
            let event_str = match event {
                Event::Custom(s) => s,
                _ => return Box::pin(async {}),
            };

            // 只处理notify事件
            if !event_str.starts_with("notify:") {
                return Box::pin(async {});
            }

            let tx = tx.clone();

            Box::pin(async move {
                match event_str.as_str() {
                    NOTIFY_ENTER_OFFICE => {
                        if let Payload::Text(values, _) = payload {
                            if let Some(value) = values.into_iter().next() {
                                if let Ok(notification) =
                                    serde_json::from_value::<smcp::EnterOfficeNotification>(value)
                                {
                                    info!("Computer entered office: {:?}", notification);
                                    let send_result =
                                        tx.send(NotificationMessage::EnterOffice(notification));
                                    if let Err(e) = send_result {
                                        error!("Failed to send EnterOffice notification: {:?}", e);
                                    } else {
                                        info!(
                                            "Successfully sent EnterOffice notification to agent"
                                        );
                                    }
                                }
                            }
                        }
                    }
                    NOTIFY_LEAVE_OFFICE => {
                        if let Payload::Text(values, _) = payload {
                            if let Some(value) = values.into_iter().next() {
                                if let Ok(notification) =
                                    serde_json::from_value::<smcp::LeaveOfficeNotification>(value)
                                {
                                    info!("Computer left office: {:?}", notification);
                                    let _ = tx.send(NotificationMessage::LeaveOffice(notification));
                                }
                            }
                        }
                    }
                    NOTIFY_UPDATE_CONFIG => {
                        if let Payload::Text(values, _) = payload {
                            if let Some(value) = values.into_iter().next() {
                                if let Ok(notification) = serde_json::from_value::<
                                    smcp::UpdateMCPConfigNotification,
                                >(value)
                                {
                                    info!("Computer updated config: {:?}", notification);
                                    let _ =
                                        tx.send(NotificationMessage::UpdateConfig(notification));
                                }
                            }
                        }
                    }
                    NOTIFY_UPDATE_TOOL_LIST => {
                        if let Payload::Text(values, _) = payload {
                            if let Some(value) = values.into_iter().next() {
                                if let Ok(notification) = serde_json::from_value::<
                                    smcp::UpdateToolListNotification,
                                >(value)
                                {
                                    info!("Computer updated tool list: {:?}", notification);
                                    let _ =
                                        tx.send(NotificationMessage::UpdateToolList(notification));
                                }
                            }
                        }
                    }
                    NOTIFY_UPDATE_DESKTOP => {
                        if let Payload::Text(values, _) = payload {
                            if let Some(value) = values.into_iter().next() {
                                if let Ok(notification) =
                                    serde_json::from_value::<serde_json::Value>(value)
                                {
                                    if let Some(computer) =
                                        notification.get("computer").and_then(|v| v.as_str())
                                    {
                                        info!(
                                            "Desktop update notification for computer: {}",
                                            computer
                                        );
                                        let _ = tx.send(NotificationMessage::UpdateDesktop(
                                            computer.to_string(),
                                        ));
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            })
        });

        // 设置命名空间
        if !namespace.is_empty() {
            builder = builder.namespace(namespace);
        }

        // 设置认证信息（克隆：原值留作 4900 改 polling 重连复用）
        // Set auth (clone: keep the original for the 4900 polling re-fetch)
        if let Some(auth_data) = &auth {
            builder = builder.auth(auth_data.clone());
        }

        // 设置头部
        for (key, value) in &headers {
            builder = builder.opening_header(key.clone(), value.clone());
        }

        // 连接服务器（polling-first 已设；分类版本握手错误，4900 时改 polling 取 4008）
        let client =
            Self::finish_connect(builder, &handshake_url, namespace, auth, headers).await?;

        // 等待一小段时间确保 Socket.IO namespace 连接完全建立
        // Wait for Socket.IO namespace connection to be fully established
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        info!(
            "Connected to SMCP server at {} with namespace {} and handlers",
            url, namespace
        );

        Ok((
            Self {
                client,
                namespace: namespace.to_string(),
            },
            rx,
        ))
    }

    /// polling-first 连接（无事件处理器）/ polling-first connect (no event handlers)。
    ///
    /// 构建 builder（namespace/auth/headers + [`TransportType::Any`]），交由 [`finish_connect`] 完成
    /// 连接与版本握手错误分类。`_handlers` 占位保留扩展位（当前无处理器）。
    async fn connect_polling_first(
        handshake_url: &str,
        namespace: &str,
        auth: Option<Value>,
        headers: HashMap<String, String>,
        _handlers: Option<()>,
    ) -> Result<Client> {
        let mut builder = ClientBuilder::new(handshake_url);

        // HS-02 #22: polling-first（见 [`connect_with_handlers`] 注释）。⚠️ 不可 WS-only。
        builder = builder.transport_type(TransportType::Any);

        if !namespace.is_empty() {
            builder = builder.namespace(namespace);
        }
        if let Some(auth_data) = &auth {
            builder = builder.auth(auth_data.clone());
        }
        for (key, value) in &headers {
            builder = builder.opening_header(key.clone(), value.clone());
        }

        Self::finish_connect(builder, handshake_url, namespace, auth, headers).await
    }

    /// 执行 connect 并按版本握手语义分类错误 / Run connect, classifying version-handshake errors。
    ///
    /// HS-02 #22 受控错误处理（镜像 Python `a2c_smcp` 客户端）：
    /// - `Ok` → 原样返回 client；
    /// - 4008（polling 握手 HTTP 400 + body）→ [`SmcpAgentError::ProtocolVersionMismatch`]；
    /// - 4900（WS close code）→ **不自动重连**；做**一次**受控 polling 重连取权威 4008 body：
    ///   命中 → 该 4008 错误；否则回退到一条仅含 message 的 [`smcp::ProtocolVersionError`]；
    /// - 其它 → 通用连接错误。
    async fn finish_connect(
        builder: ClientBuilder,
        handshake_url: &str,
        namespace: &str,
        auth: Option<Value>,
        headers: HashMap<String, String>,
    ) -> Result<Client> {
        match builder.connect().await {
            Ok(client) => Ok(client),
            Err(e) => match classify_connect_error(&e) {
                ConnectErrorKind::ProtocolVersion(pve) => {
                    Err(SmcpAgentError::ProtocolVersionMismatch(pve))
                }
                ConnectErrorKind::WsRejected4900 => {
                    warn!(
                        "WebSocket handshake rejected with close code {} (protocol version); \
                         doing a single controlled polling re-fetch for the authoritative 4008 body",
                        smcp::WS_VERSION_HANDSHAKE_REJECTED_CLOSE_CODE
                    );
                    Err(SmcpAgentError::ProtocolVersionMismatch(
                        Self::fetch_4008_via_polling(handshake_url, namespace, auth, headers).await,
                    ))
                }
                ConnectErrorKind::Other => Err(SmcpAgentError::connection(format!(
                    "Failed to connect: {}",
                    e
                ))),
            },
        }
    }

    /// 4900 后改 polling 取权威 4008 / After 4900, re-fetch the authoritative 4008 over polling。
    ///
    /// 单次受控重连：用 [`TransportType::Polling`]（同 handshake_url + 同 auth/headers），命中 4008
    /// → 返回该强类型错误；否则返回仅含 message 的回退错误。**绝不循环重试**。
    async fn fetch_4008_via_polling(
        handshake_url: &str,
        namespace: &str,
        auth: Option<Value>,
        headers: HashMap<String, String>,
    ) -> smcp::ProtocolVersionError {
        let mut builder = ClientBuilder::new(handshake_url).transport_type(TransportType::Polling);
        if !namespace.is_empty() {
            builder = builder.namespace(namespace);
        }
        if let Some(auth_data) = auth {
            builder = builder.auth(auth_data);
        }
        for (key, value) in headers {
            builder = builder.opening_header(key, value);
        }
        match builder.connect().await {
            // polling 居然连上了（4008 不再复现）—— 仍按版本拒绝处理，给出诊断性回退。
            Ok(_) => smcp::ProtocolVersionError::new(
                "WebSocket handshake rejected with close code 4900 (protocol version); \
                 authoritative 4008 unavailable",
            ),
            Err(e) => match classify_connect_error(&e) {
                ConnectErrorKind::ProtocolVersion(pve) => pve,
                _ => smcp::ProtocolVersionError::new(
                    "WebSocket handshake rejected with close code 4900 (protocol version); \
                     authoritative 4008 unavailable",
                ),
            },
        }
    }

    /// 发送事件（不等待响应）
    pub async fn emit(&self, event: &str, data: Value) -> Result<()> {
        debug!("Emitting event: {}", event);

        self.client
            .emit(event, Payload::from(vec![data]))
            .await
            .map_err(SmcpAgentError::from)
    }

    /// 发送事件并等待响应
    pub async fn call(&self, event: &str, data: Value, timeout_secs: u64) -> Result<Value> {
        debug!("Calling event: {} with timeout {}s", event, timeout_secs);

        let (tx, rx) = oneshot::channel();
        let tx = Arc::new(Mutex::new(Some(tx)));

        let callback = move |payload: Payload, _client: Client| {
            if let Some(tx_opt) = tx.try_lock().ok().and_then(|mut m| m.take()) {
                let _ = tx_opt.send(payload);
            }
            async {}.boxed()
        };

        self.client
            .emit_with_ack(
                event,
                Payload::from(vec![data]),
                Duration::from_secs(timeout_secs),
                callback,
            )
            .await?;

        match rx.await {
            Ok(response) => {
                // 从响应中提取JSON数据
                match response {
                    Payload::Text(values, _) => {
                        if let Some(value) = values.into_iter().next() {
                            Ok(value)
                        } else {
                            Err(SmcpAgentError::internal("Empty response"))
                        }
                    }
                    #[allow(deprecated)]
                    Payload::String(s, _) => {
                        // 尝试解析字符串为JSON
                        serde_json::from_str(&s).map_err(SmcpAgentError::from)
                    }
                    Payload::Binary(_, _) => {
                        Err(SmcpAgentError::internal("Binary response not supported"))
                    }
                }
            }
            Err(_) => {
                error!("Timeout while calling event: {}", event);
                Err(SmcpAgentError::Timeout)
            }
        }
    }

    /// 断开连接
    pub async fn disconnect(self) -> Result<()> {
        debug!("Disconnecting from server");
        self.client.disconnect().await.map_err(SmcpAgentError::from)
    }

    /// 获取当前连接的命名空间
    pub fn namespace(&self) -> &str {
        &self.namespace
    }
}

impl Default for SocketIoTransport {
    fn default() -> Self {
        // 创建一个未连接的占位符
        // 注意：这实际上不能使用，因为Client::new()需要参数
        // 这里只是为了满足Default trait的要求
        panic!("SocketIoTransport must be created via connect() method");
    }
}

#[cfg(test)]
mod connect_classify_tests {
    use super::*;

    /// 构造一个 socket.io 连接错误（包裹给定 engine.io 错误）。
    /// tf-rust-engineio::Error 是 #[non_exhaustive]，外部无法直接构造其变体；改用 SocketError 的
    /// #[from] 转换路径（IncompleteResponseFromEngineIo）注入，再交给被测的 classify_connect_error。
    fn socket_err(engine: EngineError) -> SocketError {
        SocketError::from(engine)
    }

    #[test]
    fn test_classify_400_with_4008_body_maps_to_protocol_version() {
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
    fn test_classify_400_non_4008_body_is_other() {
        // 400 但 body 非 4008（如 missing/invalid 的 code=400）→ 走通用路径，绝不误判为版本错误。
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
    fn test_classify_ws_4900_maps_to_ws_rejected() {
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
    fn test_classify_ws_other_close_code_is_other() {
        // 非 4900 的 WS close（如 1000 正常关闭）→ 不当作版本拒绝。
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
    fn test_classify_non_400_http_is_other() {
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
