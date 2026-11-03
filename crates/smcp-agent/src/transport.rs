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
    CloseReason, Event, Payload, TransportType,
};
use tokio::sync::{mpsc, oneshot, watch, Mutex};
use tracing::{debug, error, info};

/// 事件处理器类型
pub type EventHandler = Box<dyn FnMut(Payload, Client) + Send + Sync>;

/// 传输层（namespace）生命周期事件——Agent 侧重连回房的**触发源**（#219）。
///
/// The ordered lifecycle stream carries the close reason needed for membership recovery:
/// transport loss retains intent, while explicit disconnect or a server kick clears it.
/// It is separate from the coalesced call-invalidation snapshot, which only retains session
/// epochs and retirement. Both are updated by the same real namespace callbacks.
///
/// 事件按投递顺序经无界通道交给持有者；`Event::Connect` 与 `on_close_with_session` 两条注册路径均由
/// 内核在 namespace 生命周期节点实际派发（`on_any` **不**接收这两类事件）。
#[derive(Debug, Clone)]
pub enum TransportLifecycle {
    /// namespace 连接建立（含自动重连后的新会话）。`epoch` = [`Client::session_epoch`]。
    Connected {
        /// 本次会话的内核 epoch / the kernel session epoch of this session.
        epoch: u64,
    },
    /// namespace 断开。`epoch` 为该会话的内核 epoch（供陈旧 Close 判定）。
    Closed {
        /// 关闭原因 / the socket.io close reason.
        reason: CloseReason,
        /// 断开会话的内核 epoch / the dying session's kernel epoch.
        epoch: u64,
    },
}

/// 从 `on_close_with_session` 的线载荷解析关闭原因 / parse the close reason from the wire payload.
///
/// 内核在 `callback_close` 处以 `CloseReason::as_str()` 的三选一常量投递原因。解析失败**按
/// `TransportClose` 兜底**：三个常量由内核集中投递，实际不可解析属理论上不可达；而兜底方向取
/// 「保留意图」——丢意图会让用户在静默断线后失去自愈能力（须手工重入），多保留一次意图最多在
/// 下次重连时多一次幂等重放。
fn close_reason_from_payload(payload: &Payload) -> CloseReason {
    let as_text = |value: &str| match value {
        "io server disconnect" => Some(CloseReason::IOServerDisconnect),
        "io client disconnect" => Some(CloseReason::IOClientDisconnect),
        "transport close" => Some(CloseReason::TransportClose),
        _ => None,
    };
    match payload {
        Payload::Text(values, _) => values
            .iter()
            .filter_map(|value| value.as_str())
            .find_map(as_text)
            .unwrap_or(CloseReason::TransportClose),
        #[allow(deprecated)]
        Payload::String(value, _) => as_text(value).unwrap_or(CloseReason::TransportClose),
        Payload::Binary(_, _) => CloseReason::TransportClose,
    }
}

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
    /// Persistent session invalidation, shared by all calls and lifecycle callbacks.
    call_state_tx: Arc<watch::Sender<CallSessionState>>,
    call_state_rx: watch::Receiver<CallSessionState>,
}

/// A close remains observable even when reconnect overtakes a waiting call. Recording the
/// greatest closed epoch also makes delayed old Close callbacks harmless to newer calls.
#[derive(Clone, Copy, Debug, Default)]
struct CallSessionState {
    latest_epoch: u64,
    closed_through: Option<u64>,
    retired: bool,
}

impl CallSessionState {
    fn connected(&mut self, epoch: u64) {
        self.latest_epoch = self.latest_epoch.max(epoch);
    }

    fn closed(&mut self, epoch: u64) {
        self.closed_through = Some(self.closed_through.map_or(epoch, |old| old.max(epoch)));
    }

    fn invalidates(&self, epoch: u64) -> bool {
        self.retired
            || self.latest_epoch > epoch
            || self.closed_through.is_some_and(|closed| closed >= epoch)
    }
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
        let (call_state_tx, call_state_rx) = watch::channel(CallSessionState::default());
        let call_state_tx = Arc::new(call_state_tx);
        let client = Self::connect_polling_first(
            &handshake_url,
            namespace,
            auth,
            headers,
            call_state_tx.clone(),
        )
        .await?;

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
                call_state_tx,
                call_state_rx,
            },
            rx,
        ))
    }

    /// 创建新的传输层实例并注册事件处理器（不订阅生命周期事件）。
    ///
    /// 等价于 `lifecycle = None` 的
    /// [`connect_with_handlers_and_lifecycle`](Self::connect_with_handlers_and_lifecycle)。
    pub async fn connect_with_handlers(
        url: &str,
        namespace: &str,
        auth: Option<Value>,
        headers: HashMap<String, String>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<NotificationMessage>)> {
        Self::connect_with_handlers_and_lifecycle(url, namespace, auth, headers, None).await
    }

    /// 创建新的传输层实例并注册事件处理器，**同时订阅 namespace 生命周期事件**（#219）。
    ///
    /// Call invalidation is always registered; `lifecycle` optionally also forwards events
    /// to the Agent membership state machine.
    pub async fn connect_with_handlers_and_lifecycle(
        url: &str,
        namespace: &str,
        auth: Option<Value>,
        headers: HashMap<String, String>,
        lifecycle: Option<mpsc::UnboundedSender<TransportLifecycle>>,
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

        let (call_state_tx, call_state_rx) = watch::channel(CallSessionState::default());
        let call_state_tx = Arc::new(call_state_tx);

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

        let observes_lifecycle = lifecycle.is_some();
        builder = Self::with_call_lifecycle(builder, call_state_tx.clone(), lifecycle);

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

        // 订阅生命周期的 Agent 自行等 namespace 就绪。此路径必须立即移交 Client，不能在
        // 创建成功和交给未提交连接守卫之间留下可取消的等待窗口。
        if !observes_lifecycle {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        info!(
            "Connected to SMCP server at {} with namespace {} and handlers",
            url, namespace
        );

        Ok((
            Self {
                client,
                namespace: namespace.to_string(),
                call_state_tx,
                call_state_rx,
            },
            rx,
        ))
    }

    /// polling-first 连接（无事件处理器）/ polling-first connect (no event handlers)。
    ///
    /// Installs the same call-lifetime callbacks as the notification-enabled constructor,
    /// then performs the shared version-handshake classification.
    async fn connect_polling_first(
        handshake_url: &str,
        namespace: &str,
        auth: Option<Value>,
        headers: HashMap<String, String>,
        call_state: Arc<watch::Sender<CallSessionState>>,
    ) -> Result<Client> {
        let mut builder =
            Self::with_call_lifecycle(ClientBuilder::new(handshake_url), call_state, None);

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

    /// Subscribe to actual namespace callbacks; on_any receives only application events.
    /// Update call invalidation before forwarding membership events so recovery cannot
    /// strand the notification consumer on a call from the previous session.
    fn with_call_lifecycle(
        builder: ClientBuilder,
        state: Arc<watch::Sender<CallSessionState>>,
        lifecycle: Option<mpsc::UnboundedSender<TransportLifecycle>>,
    ) -> ClientBuilder {
        let connect_state = state.clone();
        let connect_lifecycle = lifecycle.clone();
        builder
            .on(Event::Connect, move |_payload, client| {
                let epoch = client.session_epoch();
                connect_state.send_modify(|state| state.connected(epoch));
                if let Some(tx) = &connect_lifecycle {
                    let _ = tx.send(TransportLifecycle::Connected { epoch });
                }
                async {}.boxed()
            })
            .on_close_with_session(move |payload, epoch, _client| {
                state.send_modify(|state| state.closed(epoch));
                if let Some(tx) = &lifecycle {
                    let _ = tx.send(TransportLifecycle::Closed {
                        reason: close_reason_from_payload(&payload),
                        epoch,
                    });
                }
                async {}.boxed()
            })
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

        let epoch = self.client.session_epoch();
        let mut state = self.call_state_rx.clone();
        let lost = || SmcpAgentError::connection("connection lost during call");
        if state.borrow().invalidates(epoch) {
            return Err(lost());
        }
        // The fence covers both sending and waiting for ACK. A boolean reset on Connect
        // could miss Close -> Connect while this future was not scheduled.
        let response = tokio::select! {
            biased;
            _ = state.wait_for(|state| state.invalidates(epoch)) => return Err(lost()),
            response = async {
                self.client.emit_with_ack(
                    event, Payload::from(vec![data]), Duration::from_secs(timeout_secs), callback,
                ).await?;
                rx.await.map_err(|_| SmcpAgentError::Timeout)
            } => response,
        };
        // ACK and lifecycle callbacks run concurrently. Never publish an old-session ACK
        // merely because its receiver became ready before the close callback ran.
        if state.borrow().invalidates(epoch) || self.client.session_epoch() != epoch {
            return Err(lost());
        }
        match response? {
            Payload::Text(values, _) => extract_ack_value(values),
            #[allow(deprecated)]
            Payload::String(value, _) => serde_json::from_str(&value)
                .map(flatten_ack_arg)
                .map_err(SmcpAgentError::from),
            Payload::Binary(_, _) => Err(SmcpAgentError::internal("Binary response not supported")),
        }
    }

    /// 断开连接
    pub async fn disconnect(self) -> Result<()> {
        self.close().await
    }

    /// 关闭共享 transport。释放 Arc 不会停止底层持有 Client 克隆的轮询任务。
    pub(crate) async fn close(&self) -> Result<()> {
        debug!("Disconnecting from server");
        self.call_state_tx.send_modify(|state| state.retired = true);
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

/// 从 socket.io ack 的 `Payload::Text` values 提取单个响应实参 / extract the single ack arg.
///
/// **根因修复（#82）**：socket.io ack 数据在网线上恒以 args 数组 `[<value>]` 投递——tf-rust-socketio
/// `handle_ack` 用 `Payload::from(String)` 把整帧 args 数组 JSON 文本解析成**单元素** `Vec<Value>`，
/// 其唯一元素即 args 数组本身（`Value::Array`）。故须：① 取该单元素；② 再经 [`flatten_ack_arg`]
/// 拆一层 args 数组取首个实参（A2C ack 恒单实参）。缺第 ② 步则下游 `ensure_req_id` 在数组上
/// `.get("req_id")` → `None` → 误报 “Missing req_id”。与集成矩阵 harness 的 `flat()` 同义（该助手
/// 正是靠此解一层才让矩阵全绿，掩盖了高层 SDK 路径缺同等拆封的本 bug）。
fn extract_ack_value(values: Vec<Value>) -> Result<Value> {
    let arg = values
        .into_iter()
        .next()
        .ok_or_else(|| SmcpAgentError::internal("Empty response"))?;
    Ok(flatten_ack_arg(arg))
}

/// 拆 socket.io ack 外层 args 数组取首个实参 / unwrap the outer socket.io ack args array。
///
/// ack 恒以 `[<value>]` 投递，取首个实参即响应本体；非数组 / 空数组**原样返回**（防御：理论不应
/// 出现，空数组交下游报缺字段而非在此 panic）。语义等同矩阵 harness 的 `flat()`。
fn flatten_ack_arg(value: Value) -> Value {
    match value {
        Value::Array(mut args) if !args.is_empty() => args.swap_remove(0),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::response::{classify_tool_call_outcome, ensure_req_id, ToolCallOutcome};
    use serde_json::json;

    #[tokio::test]
    async fn call_session_fence_survives_coalesced_reconnect_and_late_close() {
        let (tx, mut old_call) = watch::channel(CallSessionState::default());
        tx.send_modify(|state| state.connected(1));
        old_call.borrow_and_update();
        // Both transitions happen before the old waiter is polled: watch exposes only
        // the latest snapshot, so a resettable boolean would lose this disconnect.
        tx.send_modify(|state| state.closed(1));
        tx.send_modify(|state| state.connected(2));
        tokio::time::timeout(
            Duration::from_millis(100),
            old_call.wait_for(|s| s.invalidates(1)),
        )
        .await
        .unwrap()
        .unwrap();
        let mut new_call = tx.subscribe();
        tx.send_modify(|state| state.closed(1));
        tx.send_modify(|state| state.connected(1));
        assert!(
            !new_call.borrow().invalidates(2),
            "old callbacks cancelled a new call"
        );
        tx.send_modify(|state| state.closed(2));
        tx.send_modify(|state| state.connected(2));
        assert!(
            new_call.borrow().invalidates(2),
            "late Connect reopened a closed session"
        );
        tx.send_modify(|state| state.retired = true);
        tx.send_modify(|state| state.connected(3));
        tokio::time::timeout(
            Duration::from_millis(100),
            new_call.wait_for(|s| s.invalidates(3)),
        )
        .await
        .unwrap()
        .unwrap();
    }

    // #82：socket.io ack 数据在网线上恒以 args 数组 `[<value>]` 投递——tf-rust-socketio `handle_ack`
    // 用 `Payload::from(String)` 把整帧 args 数组 JSON 文本解析成**单元素** `Vec<Value>`，其唯一元素
    // 即 args 数组本身（`Value::Array`）。`extract_ack_value` MUST 拆该外层数组返回内层响应对象，
    // 否则下游 `ensure_req_id` 在数组上 `.get("req_id")` → `None` → 误报 “Missing req_id”。
    // `transport.call` 的 I/O 边界（SocketIoTransport 为具体 struct）不可单测，提取逻辑经此纯函数覆盖。

    #[test]
    fn extract_ack_unwraps_socketio_args_array() {
        // 原帧形状：Payload::Text 的单元素就是 args 数组 `[{...}]`（见上方注释）。
        let values = vec![json!([{ "req_id": "R1", "tools": [{ "name": "echo" }] }])];
        let v = extract_ack_value(values).expect("should extract inner response object");
        // 拆封后是对象，req_id 回显校验通过（修复前在数组上 .get("req_id")=None → Missing req_id）。
        assert!(v.is_object(), "expected inner object, got {v}");
        assert!(
            ensure_req_id(&v, "R1").is_ok(),
            "echoed req_id must validate after unwrap: {v}"
        );
        assert_eq!(v["tools"][0]["name"], "echo");
    }

    #[test]
    fn extract_ack_tool_call_result_not_double_wrapped() {
        // tool_call 同源：不校验 req_id 故不直接报错，但畸形多套一层数组会让结果级 meta 分类与
        // binary sideband（按 content 数组遍历句柄）全部落空。拆封后须为对象、content 可达。
        let values =
            vec![json!([{ "content": [{ "type": "text", "text": "ok" }], "isError": false }])];
        let v = extract_ack_value(values).expect("should extract inner response object");
        assert!(v.is_object(), "expected inner object, got {v}");
        assert_eq!(classify_tool_call_outcome(&v), ToolCallOutcome::Completed);
        assert!(
            v.get("content").and_then(Value::as_array).is_some(),
            "content must be reachable as array (binary sideband 依赖): {v}"
        );
    }

    #[test]
    fn extract_ack_empty_values_is_error() {
        // 空 values（无 ack 实参）→ internal “Empty response”。
        let err = extract_ack_value(vec![]).unwrap_err();
        assert!(matches!(err, SmcpAgentError::Internal(_)), "got {err:?}");
    }

    #[test]
    fn extract_ack_non_array_arg_passthrough() {
        // 防御：理论上 ack 实参恒为数组；万一已是对象（非标准帧）则原样返回，不过度拆封。
        let v = extract_ack_value(vec![json!({ "req_id": "R2" })]).expect("ok");
        assert_eq!(v, json!({ "req_id": "R2" }));
    }

    #[test]
    fn extract_ack_empty_args_array_passthrough() {
        // 空 args 数组 `[]` 原样返回（交下游报缺字段，不 panic、不静默成功）。
        let v = extract_ack_value(vec![json!([])]).expect("ok");
        assert_eq!(v, json!([]));
    }
}
