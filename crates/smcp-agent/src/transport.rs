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
use tf_rust_socketio::{
    asynchronous::{Client, ClientBuilder},
    Event, Payload, TransportType,
};
use tokio::sync::{mpsc, oneshot, watch, Mutex};
use tracing::{debug, error, info, warn};

/// 事件处理器类型
pub type EventHandler = Box<dyn FnMut(Payload, Client) + Send + Sync>;

/// 通知事件消息
#[derive(Debug, Clone)]
pub enum NotificationMessage {
    EnterOffice(smcp::EnterOfficeNotification),
    LeaveOffice(smcp::LeaveOfficeNotification),
    UpdateConfig(smcp::UpdateMCPConfigNotification),
    UpdateToolList(smcp::UpdateToolListNotification),
    UpdateDesktop(String), // computer name
    UpdateSkills(String),  // computer name（notify:update_skills，v0.2.1）
}

/// Socket.IO传输层
pub struct SocketIoTransport {
    client: Client,
    namespace: String,
    /// 断连信号发送端（AGT-05 #44 in-flight disconnect 容错）。`connect_with_handlers` 的 `on_any`
    /// 收到底层 `Event::Close`/`Event::Error` 时 `send(true)`，使在途 [`Self::call`] 立即放弃等待——
    /// 协议 0.2.2：Agent **MUST NOT** 靠 ack 超时判定断连，须用 disconnect/connect_error 事件。
    /// `Arc` 持有以保活（watch::Sender 非 Clone）：发送端存活则 [`Self::call`] 的 `changed()` 不会因
    /// 发送端析构而误判断连（`connect` 无处理器路径据此保持惰性而非常断）。仅作 RAII 保活、构造后不再读取。
    #[allow(dead_code)]
    disconnect_tx: Arc<watch::Sender<bool>>,
    /// 断连信号接收端（粘滞：watch 保留最新值，关闭「断连早于 call」竞速窗）/ sticky disconnect receiver。
    disconnect_rx: watch::Receiver<bool>,
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

        // 无处理器路径：断连信号保持惰性（发送端经 Arc 保活、永不 send，call 仅靠 ack/timeout）。
        // 断连事件容错需 on_any（见 connect_with_handlers），故此路径不提供——agent 走 connect_with_handlers。
        let (disconnect_tx, disconnect_rx) = watch::channel(false);

        Ok((
            Self {
                client,
                namespace: namespace.to_string(),
                disconnect_tx: Arc::new(disconnect_tx),
                disconnect_rx,
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

        // AGT-05 #44：断连信号。on_any 收到底层 Event::Close/Error → send(true)，使在途 call 立即
        // 放弃等待（不靠 ack 超时）。watch 粘滞保留最新值，关闭「断连早于 call」竞速窗。
        let (disconnect_tx, disconnect_rx) = watch::channel(false);
        let disconnect_tx = Arc::new(disconnect_tx);
        let handler_disconnect_tx = disconnect_tx.clone();

        builder = builder.on_any(move |event, payload, _client| {
            // 断连信号随底层连接状态翻转（协议 0.2.2 in-flight disconnect 容错——MUST NOT 靠 ack 超时）：
            // Close/Error → true（断连）；Connect → false（重连恢复，清除粘滞断连位，避免重连后 call 误判）。
            // call() 用 wait_for(|v| *v) 只认 true，故 Connect→false 的中间值不会误触发在途 call。
            match &event {
                Event::Close | Event::Error => {
                    let _ = handler_disconnect_tx.send(true);
                }
                Event::Connect => {
                    let _ = handler_disconnect_tx.send(false);
                }
                _ => {}
            }
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
                    NOTIFY_UPDATE_SKILLS => {
                        // v0.2.1：notify:update_skills 仅携带 {"computer": ...}，触发自动重拉 get_skills
                        // v0.2.1: notify:update_skills carries only {"computer": ...}; triggers a
                        // get_skills auto-refresh（与 UpdateDesktop 同款轻量载荷解析）。
                        if let Payload::Text(values, _) = payload {
                            if let Some(value) = values.into_iter().next() {
                                if let Ok(notification) =
                                    serde_json::from_value::<serde_json::Value>(value)
                                {
                                    // 空串 computer 视作缺失（对齐 Python `if not computer`）：
                                    // .filter 让空串落入下方 else 告警跳过，不派发空 computer 的重拉。
                                    if let Some(computer) = notification
                                        .get("computer")
                                        .and_then(|v| v.as_str())
                                        .filter(|s| !s.is_empty())
                                    {
                                        info!(
                                            "Skills update notification for computer: {}",
                                            computer
                                        );
                                        let _ = tx.send(NotificationMessage::UpdateSkills(
                                            computer.to_string(),
                                        ));
                                    } else {
                                        // 对标 Python：缺 computer 字段则告警跳过
                                        tracing::warn!(
                                            "UPDATE_SKILLS notification missing 'computer'"
                                        );
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
        let client = match smcp_client_transport::connect_and_classify(
            builder,
            &handshake_url,
            namespace,
            auth,
            headers,
        )
        .await
        {
            Ok(client) => client,
            Err(smcp_client_transport::ConnectError::ProtocolVersion(pve)) => {
                return Err(SmcpAgentError::ProtocolVersionMismatch(pve));
            }
            Err(smcp_client_transport::ConnectError::Connection(msg)) => {
                return Err(SmcpAgentError::connection(msg));
            }
        };

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
                disconnect_tx,
                disconnect_rx,
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

        match smcp_client_transport::connect_and_classify(
            builder,
            handshake_url,
            namespace,
            auth,
            headers,
        )
        .await
        {
            Ok(client) => Ok(client),
            Err(smcp_client_transport::ConnectError::ProtocolVersion(pve)) => {
                Err(SmcpAgentError::ProtocolVersionMismatch(pve))
            }
            Err(smcp_client_transport::ConnectError::Connection(msg)) => {
                Err(SmcpAgentError::connection(msg))
            }
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

        // AGT-05 #44：把 ack 等待与断连信号竞速——Agent MUST NOT 靠 ack 超时判定断连（协议 0.2.2
        // in-flight disconnect 容错）。`wait_for(|v| *v)` 只在断连位为 true 时就绪：粘滞——若进入前已断连
        // 立即就绪（关闭「断连早于 call」竞速窗）；且忽略重连时 Connect→false 的中间值，不误触发。
        //
        // 测试覆盖说明：本竞速逻辑依赖真实 socket 事件（Event::Close/Error），按项目"无 mock transport"
        // 约定（SocketIoTransport 为具体 struct）无法单测；其端到端覆盖（mid-call 杀连接）随 #72 socketio
        // 接线一并补 e2e。逻辑正确性：粘滞读 + Err（发送端析构=传输析构）亦视为断连。
        let mut disconnect_rx = self.disconnect_rx.clone();

        tokio::select! {
            // biased：ack 与断连同时就绪时优先取真实响应（避免已到达的结果被误判为断连）。
            biased;
            recv = rx => match recv {
                // 从响应中提取JSON数据
                Ok(Payload::Text(values, _)) => values
                    .into_iter()
                    .next()
                    .ok_or_else(|| SmcpAgentError::internal("Empty response")),
                #[allow(deprecated)]
                Ok(Payload::String(s, _)) => {
                    // 尝试解析字符串为JSON
                    serde_json::from_str(&s).map_err(SmcpAgentError::from)
                }
                Ok(Payload::Binary(_, _)) => {
                    Err(SmcpAgentError::internal("Binary response not supported"))
                }
                Err(_) => {
                    error!("Timeout while calling event: {}", event);
                    Err(SmcpAgentError::Timeout)
                }
            },
            // 底层 socket 断连 / 连接错误（on_any 收到 Event::Close/Error 置 true）→ 立即判定断连，
            // 不空等满 ack 超时。`wait_for` 返回 Err（发送端析构 = 传输析构）同样视为断连。
            _ = disconnect_rx.wait_for(|disconnected| *disconnected) => {
                warn!("Connection lost during in-flight call: {}", event);
                Err(SmcpAgentError::connection(
                    "connection lost (disconnect/connect_error) during call",
                ))
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
