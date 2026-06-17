//! SMCP 协议处理器 / SMCP protocol handler

use crate::auth::{AuthError, AuthenticationProvider};
use crate::session::{ClientRole, SessionData, SessionError, SessionManager};
use futures_util::StreamExt;
use serde_json::Value;
use smcp::*;
use socketioxide::{
    extract::{AckSender, Data, SocketRef, TryData},
    SocketIo,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use thiserror::Error;
use tokio::sync::Notify;
use tracing::{error, info, warn};

/// 处理器错误类型
#[derive(Error, Debug)]
pub enum HandlerError {
    #[error("Authentication error: {0}")]
    Auth(#[from] AuthError),
    #[error("Session error: {0}")]
    Session(#[from] SessionError),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Timeout error: {0}")]
    Timeout(String),
    #[error("Invalid request: {0}")]
    InvalidRequest(String),
    /// 房间/角色隔离拒绝 / Room/role isolation rejection.
    ///
    /// 对标 Python `SMCPNamespaceError`（见 `server/namespace.py`）。属安全不变量——以**运行期**
    /// `Result` 表达（**非** `debug_assert!`），故 release build（无 `debug_assert!`）下隔离同样硬化。
    /// 投递语义：`client:*` 路由命中此变体时**不投递协议 ack**（镜像 Python `raise`，发起方侧自行超时，
    /// 不泄露 Computer 存在性、不造非协议错误码），见 [`SmcpHandler::relay_client_call`]。
    #[error("Isolation rejected: {0}")]
    Isolation(String),
}

impl HandlerError {
    /// 获取错误码 / Get error code
    pub fn error_code(&self) -> i32 {
        match self {
            HandlerError::Auth(_) => smcp::error_codes::UNAUTHORIZED,
            HandlerError::Session(e) => e.error_code(),
            HandlerError::Json(_) => smcp::error_codes::BAD_REQUEST,
            HandlerError::Timeout(_) => smcp::error_codes::TIMEOUT,
            HandlerError::InvalidRequest(_) => smcp::error_codes::BAD_REQUEST,
            // 隔离拒绝仅作内部诊断码，永不进 client:* ack（路由层直接丢弃，不投递）。
            HandlerError::Isolation(_) => smcp::error_codes::FORBIDDEN,
        }
    }

    /// 转换为 flat 错误负载 / Convert to a flat error payload
    ///
    /// 协议 0.2.2：错误负载统一为 flat [`smcp::ErrorPayload`]（顶层 `code`/`message`），禁止嵌套
    /// `{"error": {...}}` envelope。本方法服务于 `server:*` **管理事件** ack——其码空间是传输/管理层
    /// [`smcp::error_codes`]（400/401/500/4101…），不受协议谓词约束。
    ///
    /// ✅ SRV-01 (#47) 已收敛 `client:*` 路径：`client:*` ack **永不**承载裸 `HandlerError`——
    /// 目标未命中经 [`smcp::build_computer_not_found_error`]（[`smcp::ErrorCode::NotFound`] 404）以
    /// bare flat ErrorPayload 投递，隔离拒绝则不投递 ack（见 [`Self::relay_client_call`]）。因此
    /// Agent 端按 [`smcp::is_protocol_error_payload`] 判定的协议级闭集（404/4006–4018）不会被本方法
    /// 的传输层码污染。
    pub fn to_error_payload(&self) -> smcp::ErrorPayload {
        smcp::ErrorPayload::new(i64::from(self.error_code()), self.to_string())
    }
}

impl serde::Serialize for HandlerError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        // 使用 flat 错误负载格式 / Use the flat error payload shape
        self.to_error_payload().serialize(serializer)
    }
}

/// 在途断连信号注册表 / In-flight disconnect signal registry.
///
/// 对标 Python `SMCPNamespace._inflight_disconnect_signals`（#100 Phase 1）。每个在途
/// [`SmcpHandler::relay_client_call`] 针对其**目标 Computer SID** 登记一个独立 [`Notify`]；目标
/// `on_disconnect` 时唤醒所有以该 SID 为目标的信号，使在途 relay 立即放弃等待、回 flat 404，而非
/// 空等满 timeout（socketioxide `emit_with_ack` 的等待原语不监听连接掉线）。
///
/// 语义：目标 Computer 中途消失 == 不存在，与「目标未命中」（#47 / `build_computer_not_found_error`）
/// 同一 flat `ErrorPayload(404)`，**不**泄露存在性、**不**新增错误码（协议 0.2.2：Server MAY 静默丢弃）。
#[derive(Default)]
pub struct InflightDisconnectRegistry {
    /// 目标 Computer SID -> 该 SID 当前在途的断连信号集合。
    signals: Mutex<HashMap<String, Vec<Arc<Notify>>>>,
}

impl std::fmt::Debug for InflightDisconnectRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 不打印 Notify（无 Debug）；仅暴露当前登记的目标数，便于诊断。
        let targets = self
            .signals
            .lock()
            .map(|m| m.len())
            .unwrap_or_else(|e| e.into_inner().len());
        f.debug_struct("InflightDisconnectRegistry")
            .field("targets", &targets)
            .finish()
    }
}

impl InflightDisconnectRegistry {
    /// 登记并返回一个针对 `computer_sid` 的在途断连信号。
    /// Register and return a fresh disconnect signal targeting `computer_sid`.
    fn register(&self, computer_sid: &str) -> Arc<Notify> {
        let notify = Arc::new(Notify::new());
        self.lock()
            .entry(computer_sid.to_string())
            .or_default()
            .push(notify.clone());
        notify
    }

    /// 注销指定信号（幂等）；桶空则回收。Discard a specific signal (idempotent); reap empty buckets.
    fn discard(&self, computer_sid: &str, notify: &Arc<Notify>) {
        let mut guard = self.lock();
        if let Some(bucket) = guard.get_mut(computer_sid) {
            bucket.retain(|n| !Arc::ptr_eq(n, notify));
            if bucket.is_empty() {
                guard.remove(computer_sid);
            }
        }
    }

    /// 唤醒以 `computer_sid` 为目标的全部在途断连信号（幂等）。
    /// Wake all in-flight signals targeting `computer_sid` (idempotent).
    ///
    /// 用 [`Notify::notify_one`]（每个信号恰一个等待者）：即便在 relay 进入 `notified().await` 之前
    /// fire，permit 也会被暂存，等待者随后立即就绪——闭合「解析 SID 与登记信号之间目标已断连」的竞速窗。
    fn fire(&self, computer_sid: &str) {
        // 先拷出桶再 notify，避免在持锁期间触发任何回调路径。
        let bucket = self.lock().get(computer_sid).cloned();
        if let Some(bucket) = bucket {
            for notify in bucket {
                notify.notify_one();
            }
        }
    }

    /// 取锁并自动从 poison 中恢复——隔离守卫 MUST NOT crash（临界区无 panic 路径，恢复仅为兜底）。
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Vec<Arc<Notify>>>> {
        self.signals.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// RAII 守卫：relay 从任意路径退出（含 `?`/早返回/panic 展开）时自动注销其在途断连信号。
/// RAII guard: deregister a relay's in-flight signal on any exit path.
struct InflightSignalGuard<'a> {
    registry: &'a InflightDisconnectRegistry,
    computer_sid: &'a str,
    notify: Arc<Notify>,
}

impl Drop for InflightSignalGuard<'_> {
    fn drop(&mut self) {
        self.registry.discard(self.computer_sid, &self.notify);
    }
}

/// 服务器状态
#[derive(Clone, Debug)]
pub struct ServerState {
    /// 会话管理器
    pub session_manager: Arc<SessionManager>,
    /// 认证提供者
    pub auth_provider: Arc<dyn AuthenticationProvider>,
    /// SocketIo 实例引用，用于跨 socket 通信
    pub io: Arc<SocketIo>,
    /// 在途断连信号注册表（#56 SRV-04）：目标 Computer 飞行中断连时唤醒在途 relay。
    /// In-flight disconnect signals: wake relays whose target Computer dies mid-flight.
    pub inflight_disconnect: Arc<InflightDisconnectRegistry>,
}

/// SMCP 事件处理器
pub struct SmcpHandler;

impl SmcpHandler {
    /// 注册所有事件处理器
    pub fn register_handlers(io: &SocketIo, state: ServerState) {
        // 注册命名空间和连接处理器
        // #86：用 `TryData<Value>` 提取器拿 Socket.IO CONNECT `auth` dict（连接面鉴权唯一来源）。
        io.ns(
            SMCP_NAMESPACE,
            move |socket: SocketRef, TryData(auth): TryData<Value>| {
                let state = state.clone();
                async move {
                    if let Err(e) = Self::on_connect(socket.clone(), auth.ok(), &state).await {
                        // #86：鉴权/握手失败 → **主动断开** socket，真正拒绝连接，而非仅 log+return
                        // 留下「半开、无业务 handler」的 socket（后者会让无效鉴权静默旁路）。
                        // Auth/handshake failure → disconnect the socket (real rejection), instead of
                        // leaving a half-open handler-less socket that silently bypasses auth.
                        warn!(
                            "on_connect rejected ({}); disconnecting socket {}",
                            e, socket.id
                        );
                        let _ = socket.disconnect();
                        return;
                    }

                    // 连接时注册所有事件处理器
                    Self::handle_connection(socket, state)
                }
            },
        );
    }

    /// 处理连接并注册事件处理器
    fn handle_connection(socket: SocketRef, state: ServerState) {
        // 注册各种事件处理器
        socket.on_disconnect({
            let state = state.clone();
            move |socket: SocketRef| {
                let state = state.clone();
                async move { Self::on_disconnect(socket, state).await }
            }
        });

        let state_join = state.clone();
        socket.on(
            smcp::events::SERVER_JOIN_OFFICE,
            move |socket: SocketRef, Data::<EnterOfficeReq>(data), ack: AckSender| async move {
                let result = Self::on_server_join_office(socket, data, state_join.clone()).await;
                let _ = ack.send(&result);
            },
        );

        let state_leave = state.clone();
        socket.on(
            smcp::events::SERVER_LEAVE_OFFICE,
            move |socket: SocketRef, Data::<LeaveOfficeReq>(data), ack: AckSender| async move {
                let result = Self::on_server_leave_office(socket, data, state_leave.clone()).await;
                let _ = ack.send(&result);
            },
        );

        let state_tool_call_cancel = state.clone();
        socket.on(
            smcp::events::SERVER_TOOL_CALL_CANCEL,
            move |socket: SocketRef, Data::<AgentCallData>(data)| async move {
                Self::on_server_tool_call_cancel(socket, data, state_tool_call_cancel.clone()).await
            },
        );

        let state_update_config = state.clone();
        socket.on(
            smcp::events::SERVER_UPDATE_CONFIG,
            move |socket: SocketRef, Data::<UpdateComputerConfigReq>(data)| async move {
                Self::on_server_update_config(socket, data, state_update_config.clone()).await
            },
        );

        let state_update_tool_list = state.clone();
        socket.on(
            smcp::events::SERVER_UPDATE_TOOL_LIST,
            move |socket: SocketRef, Data::<UpdateComputerConfigReq>(data)| async move {
                Self::on_server_update_tool_list(socket, data, state_update_tool_list.clone()).await
            },
        );

        let state_tool_call = state.clone();
        socket.on(
            smcp::events::CLIENT_TOOL_CALL,
            move |socket: SocketRef, Data::<ToolCallReq>(data), ack: AckSender| async move {
                match Self::on_client_tool_call(socket, data, state_tool_call.clone()).await {
                    Ok(payload) => {
                        let _ = ack.send(&payload);
                    }
                    // 隔离拒绝（发起方非 Agent / 会话已断连）：镜像 Python，不投递协议 ack（发起方侧自行超时）
                    Err(e) => warn!("client:tool_call relay rejected, no ack: {e}"),
                }
            },
        );

        let state_get_tools = state.clone();
        socket.on(
            smcp::events::CLIENT_GET_TOOLS,
            move |socket: SocketRef, Data::<GetToolsReq>(data), ack: AckSender| async move {
                match Self::on_client_get_tools(socket, data, state_get_tools.clone()).await {
                    Ok(payload) => {
                        let _ = ack.send(&payload);
                    }
                    Err(e) => warn!("client:get_tools relay rejected, no ack: {e}"),
                }
            },
        );

        let state_get_desktop = state.clone();
        socket.on(
            smcp::events::CLIENT_GET_DESKTOP,
            move |socket: SocketRef, Data::<GetDesktopReq>(data), ack: AckSender| async move {
                match Self::on_client_get_desktop(socket, data, state_get_desktop.clone()).await {
                    Ok(payload) => {
                        let _ = ack.send(&payload);
                    }
                    Err(e) => warn!("client:get_desktop relay rejected, no ack: {e}"),
                }
            },
        );

        let state_get_config = state.clone();
        socket.on(
            smcp::events::CLIENT_GET_CONFIG,
            move |socket: SocketRef, Data::<GetComputerConfigReq>(data), ack: AckSender| async move {
                match Self::on_client_get_config(socket, data, state_get_config.clone()).await {
                    Ok(payload) => {
                        let _ = ack.send(&payload);
                    }
                    Err(e) => warn!("client:get_config relay rejected, no ack: {e}"),
                }
            },
        );

        let state_update_desktop = state.clone();
        socket.on(
            smcp::events::SERVER_UPDATE_DESKTOP,
            move |socket: SocketRef, Data::<UpdateComputerConfigReq>(data)| async move {
                Self::on_server_update_desktop(socket, data, state_update_desktop.clone()).await
            },
        );

        let state_list_room = state.clone();
        socket.on(
            smcp::events::SERVER_LIST_ROOM,
            move |socket: SocketRef, Data::<ListRoomReq>(data), ack: AckSender| async move {
                let result = Self::on_server_list_room(socket, data, state_list_room.clone()).await;
                let _ = ack.send(&result);
            },
        );

        // ── SKILL 通道（SRV-02 #50）/ SKILL channel ──────────────────────────
        let state_get_skills = state.clone();
        socket.on(
            smcp::events::CLIENT_GET_SKILLS,
            move |socket: SocketRef, Data::<GetSkillsReq>(data), ack: AckSender| async move {
                match Self::on_client_get_skills(socket, data, state_get_skills.clone()).await {
                    Ok(payload) => {
                        let _ = ack.send(&payload);
                    }
                    Err(e) => warn!("client:get_skills relay rejected, no ack: {e}"),
                }
            },
        );

        let state_get_skill = state.clone();
        socket.on(
            smcp::events::CLIENT_GET_SKILL,
            move |socket: SocketRef, Data::<GetSkillReq>(data), ack: AckSender| async move {
                match Self::on_client_get_skill(socket, data, state_get_skill.clone()).await {
                    Ok(payload) => {
                        let _ = ack.send(&payload);
                    }
                    Err(e) => warn!("client:get_skill relay rejected, no ack: {e}"),
                }
            },
        );

        let state_get_blob = state.clone();
        socket.on(
            smcp::events::CLIENT_GET_BLOB,
            move |socket: SocketRef, Data::<GetBlobReq>(data), ack: AckSender| async move {
                match Self::on_client_get_blob(socket, data, state_get_blob.clone()).await {
                    Ok(payload) => {
                        let _ = ack.send(&payload);
                    }
                    Err(e) => warn!("client:get_blob relay rejected, no ack: {e}"),
                }
            },
        );

        let state_get_resources = state.clone();
        socket.on(
            smcp::events::CLIENT_GET_RESOURCES,
            move |socket: SocketRef, Data::<GetResourcesReq>(data), ack: AckSender| async move {
                match Self::on_client_get_resources(socket, data, state_get_resources.clone()).await
                {
                    Ok(payload) => {
                        let _ = ack.send(&payload);
                    }
                    Err(e) => warn!("client:get_resources relay rejected, no ack: {e}"),
                }
            },
        );

        let state_update_skills = state.clone();
        socket.on(
            smcp::events::SERVER_UPDATE_SKILLS,
            move |socket: SocketRef, Data::<UpdateComputerConfigReq>(data)| async move {
                Self::on_server_update_skills(socket, data, state_update_skills.clone()).await
            },
        );
    }

    /// 处理连接事件
    ///
    /// `auth`：Socket.IO CONNECT `auth` dict（#86 连接面鉴权唯一来源，由 `.ns()` 的
    /// `TryData<Value>` 提取器解码得到；HTTP header 不再参与鉴权）。
    async fn on_connect(
        socket: SocketRef,
        auth: Option<Value>,
        state: &ServerState,
    ) -> Result<(), HandlerError> {
        info!(
            "SocketIO Client {} connecting to {}...",
            socket.id, SMCP_NAMESPACE
        );

        // #86：用 CONNECT auth dict 鉴权（headers 仅供需要路由头的自定义 provider 参考）。
        let headers = socket.req_parts().headers.clone();

        // 认证
        state
            .auth_provider
            .authenticate(&headers, auth.as_ref())
            .await?;

        info!(
            "SocketIO Client {} connected successfully to {}",
            socket.id, SMCP_NAMESPACE
        );
        Ok(())
    }

    /// 处理断开连接事件
    async fn on_disconnect(socket: SocketRef, state: ServerState) {
        info!(
            "SocketIO Client {} disconnecting from {}...",
            socket.id, SMCP_NAMESPACE
        );

        // 清理会话
        let sid = socket.id.to_string();
        if let Some(session) = state.session_manager.unregister_session(&sid) {
            // 如果在房间内，广播离开消息
            if let Some(office_id) = session.office_id {
                let notification = if session.role == ClientRole::Computer {
                    LeaveOfficeNotification {
                        office_id: office_id.clone(),
                        computer: Some(session.name),
                        agent: None,
                    }
                } else {
                    LeaveOfficeNotification {
                        office_id: office_id.clone(),
                        computer: None,
                        agent: Some(session.name),
                    }
                };

                let _ = socket
                    .within(office_id)
                    .emit(smcp::events::NOTIFY_LEAVE_OFFICE, &notification)
                    .await;
            }
        }

        // 在途断连守卫（#56 SRV-04）：先做完上面的会话/name 清理（关闭 TOCTOU 微窗，对齐 Python
        // 「先 super().on_disconnect 再 fire」），再唤醒所有以本 sid 为目标的在途 relay——它们随即
        // 放弃等待、回 flat 404，而非空等满 timeout。Wake every in-flight relay targeting this sid.
        state.inflight_disconnect.fire(&sid);

        info!(
            "SocketIO Client {} disconnected from {}",
            socket.id, SMCP_NAMESPACE
        );
    }

    /// 处理加入办公室事件
    async fn on_server_join_office(
        socket: SocketRef,
        data: EnterOfficeReq,
        state: ServerState,
    ) -> (bool, Option<String>) {
        info!("on_server_join_office called with data: {:?}", data);

        let sid = socket.id.to_string();
        let requested_role = ClientRole::from(data.role.clone());
        let requested_name = data.name.clone();

        // 获取或创建会话
        let session = match state.session_manager.get_session(&sid) {
            Some(s) => {
                // 检查角色/状态一致性
                if s.role != requested_role {
                    return (
                        false,
                        Some(format!(
                            "Role mismatch: existing session has role {:?}, but requested {:?}",
                            s.role, requested_role
                        )),
                    );
                }

                if s.name != requested_name {
                    return (
                        false,
                        Some(format!(
                            "Name mismatch: existing session has name '{}', but requested '{}'",
                            s.name, requested_name
                        )),
                    );
                }

                s
            }
            None => {
                // 从握手 URL query 提取协商到的协议版本（仅记录用于诊断/展示，
                // 兼容性已由 HTTP 握手中间件保证）。提取逻辑对齐 Python `_extract_a2c_version`。
                //
                // 时机说明：Python 在 on_connect 即记录 a2c_version；Rust 会话在 join 时才懒建，
                // 故在此（会话创建处）记录。二者 list_room 结果等价——`req_parts().uri.query()`
                // 反映的是该连接的握手请求 URI，连接存续期间稳定（已由集成测试
                // `test_list_room_reports_a2c_version` 固化验证）。
                let a2c_version = smcp::utils::handshake::extract_a2c_version(
                    socket.req_parts().uri.query().unwrap_or(""),
                );
                // 创建新会话
                let new_session = SessionData::new(sid.clone(), requested_name, requested_role)
                    .with_a2c_version(a2c_version);

                if let Err(e) = state.session_manager.register_session(new_session.clone()) {
                    return (false, Some(format!("Failed to register session: {}", e)));
                }
                new_session
            }
        };

        // 检查并加入房间
        if let Err(e) =
            Self::handle_join_room(socket.clone(), &session, &data.office_id, &state).await
        {
            error!("handle_join_room failed: {}", e);
            return (false, Some(format!("Failed to join room: {}", e)));
        }

        // 更新会话的办公室 ID（在成功加入房间后）
        if let Err(e) = state
            .session_manager
            .update_office_id(&sid, Some(data.office_id.clone()))
        {
            return (false, Some(format!("Failed to update office_id: {}", e)));
        }

        // 构建通知数据
        let session_name = session.name.clone();
        let notification_data = if session.role == ClientRole::Computer {
            EnterOfficeNotification {
                office_id: data.office_id.clone(),
                computer: Some(session_name.clone()),
                agent: None,
            }
        } else {
            EnterOfficeNotification {
                office_id: data.office_id.clone(),
                computer: None,
                agent: Some(session_name.clone()),
            }
        };

        let result = socket
            .to(data.office_id.clone())
            .emit(smcp::events::NOTIFY_ENTER_OFFICE, &notification_data)
            .await;

        if let Err(e) = result {
            warn!("Failed to broadcast NOTIFY_ENTER_OFFICE: {}", e);
        }

        (true, None)
    }

    /// 处理离开办公室事件
    async fn on_server_leave_office(
        socket: SocketRef,
        data: LeaveOfficeReq,
        state: ServerState,
    ) -> (bool, Option<String>) {
        let sid = socket.id.to_string();

        // 获取会话
        let session = match state.session_manager.get_session(&sid) {
            Some(s) => s,
            None => return (false, Some(format!("Session not found: {}", sid))),
        };

        // 构建离开通知
        let notification = if session.role == ClientRole::Computer {
            LeaveOfficeNotification {
                office_id: data.office_id.clone(),
                computer: Some(session.name),
                agent: None,
            }
        } else {
            LeaveOfficeNotification {
                office_id: data.office_id.clone(),
                computer: None,
                agent: Some(session.name),
            }
        };

        // 广播离开消息
        let _ = socket
            .within(data.office_id.clone())
            .emit(smcp::events::NOTIFY_LEAVE_OFFICE, &notification)
            .await;

        // 更新会话
        if let Err(e) = state.session_manager.update_office_id(&sid, None) {
            return (false, Some(format!("Failed to update office_id: {}", e)));
        }
        socket.leave(data.office_id.clone());

        (true, None)
    }

    /// 处理工具调用取消事件
    async fn on_server_tool_call_cancel(
        socket: SocketRef,
        data: AgentCallData,
        state: ServerState,
    ) {
        let sid = socket.id.to_string();
        let session = match state.session_manager.get_session(&sid) {
            Some(s) => s,
            None => {
                warn!("SERVER_TOOL_CALL_CANCEL from unknown session sid={}", sid);
                return;
            }
        };

        // Python 侧语义：向 office(room) 广播并跳过自己
        // 这里沿用 socketioxide 的 to(room) 语义：从当前 socket 触发时，会自动排除自身。

        // 角色断言：取消工具调用通常由 Agent 发起
        if session.role != ClientRole::Agent {
            warn!(
                "SERVER_TOOL_CALL_CANCEL role mismatch: expected Agent, got {:?}, sid={}",
                session.role, sid
            );
            return;
        }

        let office_id = match session.office_id {
            Some(ref office_id) => office_id.clone(),
            None => {
                warn!(
                    "SERVER_TOOL_CALL_CANCEL but session not in office, sid={}",
                    sid
                );
                return;
            }
        };

        if let Err(e) = socket
            .to(office_id)
            .emit(smcp::events::NOTIFY_TOOL_CALL_CANCEL, &data)
            .await
        {
            warn!("Failed to broadcast NOTIFY_TOOL_CALL_CANCEL: {}", e);
        }
    }

    /// 处理配置更新事件
    async fn on_server_update_config(
        socket: SocketRef,
        data: UpdateComputerConfigReq,
        state: ServerState,
    ) {
        let sid = socket.id.to_string();
        let session = match state.session_manager.get_session(&sid) {
            Some(s) => s,
            None => {
                warn!("SERVER_UPDATE_CONFIG from unknown session sid={}", sid);
                return;
            }
        };

        // 角色断言：配置更新通常由 Computer 发起
        if session.role != ClientRole::Computer {
            warn!(
                "SERVER_UPDATE_CONFIG role mismatch: expected Computer, got {:?}, sid={}",
                session.role, sid
            );
            return;
        }

        let office_id = match session.office_id {
            Some(ref office_id) => office_id.clone(),
            None => {
                warn!(
                    "SERVER_UPDATE_CONFIG but session not in office, sid={}",
                    sid
                );
                return;
            }
        };

        // 广播配置更新通知（向 office 广播并跳过自己）
        let notification = UpdateMCPConfigNotification {
            computer: data.computer.clone(),
        };

        let office_id_clone = office_id.clone();
        let computer_clone = data.computer.clone();
        info!(
            "Broadcasting NOTIFY_UPDATE_CONFIG to room '{}' from computer '{}' (sid: {})",
            office_id_clone, computer_clone, sid
        );

        if let Err(e) = socket
            .to(office_id.clone())
            .emit(smcp::events::NOTIFY_UPDATE_CONFIG, &notification)
            .await
        {
            warn!("Failed to broadcast NOTIFY_UPDATE_CONFIG: {}", e);
        } else {
            info!(
                "Successfully broadcasted NOTIFY_UPDATE_CONFIG to room '{}'",
                office_id
            );
        }
    }

    /// 处理工具列表更新事件
    async fn on_server_update_tool_list(
        socket: SocketRef,
        data: UpdateComputerConfigReq,
        state: ServerState,
    ) {
        let sid = socket.id.to_string();
        let session = match state.session_manager.get_session(&sid) {
            Some(s) => s,
            None => {
                warn!("SERVER_UPDATE_TOOL_LIST from unknown session sid={}", sid);
                return;
            }
        };

        // 角色断言：工具列表更新通常由 Computer 发起
        if session.role != ClientRole::Computer {
            warn!(
                "SERVER_UPDATE_TOOL_LIST role mismatch: expected Computer, got {:?}, sid={}",
                session.role, sid
            );
            return;
        }

        let office_id = match session.office_id {
            Some(ref office_id) => office_id.clone(),
            None => {
                warn!(
                    "SERVER_UPDATE_TOOL_LIST but session not in office, sid={}",
                    sid
                );
                return;
            }
        };

        // 广播工具列表更新通知（向 office 广播并跳过自己）
        let notification = UpdateToolListNotification {
            computer: data.computer,
        };

        if let Err(e) = socket
            .to(office_id)
            .emit(smcp::events::NOTIFY_UPDATE_TOOL_LIST, &notification)
            .await
        {
            warn!("Failed to broadcast NOTIFY_UPDATE_TOOL_LIST: {}", e);
        }
    }

    /// 处理 SKILL 集合更新事件（SRV-02 #50）/ broadcast `server:update_skills` → `notify:update_skills`.
    ///
    /// Computer 在 SKILL 集合变化（增/删/物化更新）时触发；Server **仅**在来源为 Computer 时向其所在 office
    /// 广播 `notify:update_skills`（跳过自己），Agent 据此自动重拉 `client:get_skills`（仿既有 `notify:update_*`
    /// 自动刷新模式）。非法来源（非 Computer / 无 office）被拒绝、不广播。对标 Python `on_server_update_skills`；
    /// 载荷复用 `UpdateComputerConfigReq`（仅 `computer` 字段，三事件族共用）。
    async fn on_server_update_skills(
        socket: SocketRef,
        data: UpdateComputerConfigReq,
        state: ServerState,
    ) {
        let sid = socket.id.to_string();
        let session = match state.session_manager.get_session(&sid) {
            Some(s) => s,
            None => {
                warn!("SERVER_UPDATE_SKILLS from unknown session sid={}", sid);
                return;
            }
        };

        // 仅 Computer 可上报 SKILL 变更（非法来源拒绝）。
        if session.role != ClientRole::Computer {
            warn!(
                "SERVER_UPDATE_SKILLS role mismatch: expected Computer, got {:?}, sid={}",
                session.role, sid
            );
            return;
        }

        let office_id = match session.office_id {
            Some(ref office_id) => office_id.clone(),
            None => {
                warn!(
                    "SERVER_UPDATE_SKILLS but session not in office, sid={}",
                    sid
                );
                return;
            }
        };

        // 广播 SKILL 更新通知（向 office 广播并跳过自己）；载荷复用 `{computer}` 形态。
        let notification = UpdateMCPConfigNotification {
            computer: data.computer,
        };
        if let Err(e) = socket
            .to(office_id)
            .emit(smcp::events::NOTIFY_UPDATE_SKILLS, &notification)
            .await
        {
            warn!("Failed to broadcast NOTIFY_UPDATE_SKILLS: {}", e);
        }
    }

    /// 构造 bare flat `ErrorPayload(404)`（Computer 未命中 / 中途消失）的 ack 负载。
    /// 复用 [`smcp::build_computer_not_found_error`]（与 #47/#92 同 payload，不泄露存在性、不新增错误码）。
    fn computer_not_found_value(computer_name: &str) -> Result<Value, HandlerError> {
        Ok(serde_json::to_value(build_computer_not_found_error(
            computer_name,
        ))?)
    }

    /// 处理客户端工具调用事件
    /// 统一 `client:*` 事件路由 / Generic `client:*` event router.
    ///
    /// 收敛 office/role 隔离校验、Computer SID 解析、flat `ErrorPayload` 透传，让每个 `client:*`
    /// handler 缩到一行。对标 Python `server/namespace.py::_relay_client_call`。
    ///
    /// 投递到 ack 的负载为 **bare** 形态（成功 Ret 原样 或 flat [`smcp::ErrorPayload`]），**无**
    /// `{"Ok"/"Err"}` / `{"error":{...}}` 嵌套 envelope（协议 0.2.2：`client:*` ack 协议级错误
    /// MUST 为 flat ErrorPayload）。
    ///
    /// - 目标 Computer 在**发起方 office 内**未命中（含跨 office：office-scoped 查找天然不可达，
    ///   不泄露存在性）/ 发起方无 office / 目标 SID 已断连 → 返回 flat `ErrorPayload(404)`（经 ack 投递）。
    /// - 目标 ack 为协议级 flat ErrorPayload（[`smcp::is_protocol_error_payload`]）→ 原样透传；
    ///   成功响应同样 **bare 透传**，**不**剥 `"result"`（旧实现的 `map.remove("result")` 是 bug：
    ///   Computer 发送 bare `CallToolResult`，剥取把成功结果误抹为 `Null`）。
    /// - 发起方非 Agent → [`HandlerError::Isolation`]（隔离拒绝）；发起方会话已断连 →
    ///   [`HandlerError::Session`]：两者调用点均**不投递协议 ack**（镜像 Python `raise` 语义，
    ///   发起方侧自行超时；不泄露 Computer 存在性、不造非协议错误码）。
    ///
    /// ## 在途断连容错（#56 SRV-04）/ In-flight disconnect tolerance
    ///
    /// - **目标 Computer 飞行中断连**：socketioxide `emit_with_ack` 的等待原语不监听掉线，目标在途
    ///   断连会空等满 `timeout`。故把 ack 流与 [`InflightDisconnectRegistry`] 的目标断连信号竞速
    ///   （对标 Python `_relay_client_call` #100）——断连先到 → 立即回 flat `ErrorPayload(404)`
    ///   （目标中途消失 == 不存在，与 #47/#92 同一 payload）。真超时仍原样回 [`HandlerError::Timeout`]，
    ///   **绝不**转 404（守住「超时 ≠ 不存在」语义区分）。
    /// - **原发起方（Agent）飞行中断连**：relay 完成后复查发起方会话——已断连 → 静默丢弃
    ///   （返回 [`HandlerError::Session`]，与前置检查同变体；调用点不投 ack；协议 0.2.2：Server MAY
    ///   不 ack、不投 ErrorPayload）。session 不可达全程经受控 `Result` 收编，**MUST NOT** panic。
    async fn relay_client_call<T: serde::Serialize + ?Sized>(
        socket: &SocketRef,
        computer_name: &str,
        data: &T,
        event: &str,
        state: &ServerState,
        timeout: tokio::time::Duration,
    ) -> Result<Value, HandlerError> {
        // 发起方（Agent）会话 / Originator (Agent) session
        let sid = socket.id.to_string();
        let session = state
            .session_manager
            .get_session(&sid)
            .ok_or_else(|| HandlerError::Session(SessionError::NotFound(sid.clone())))?;

        // 角色隔离：仅 Agent 可发起 client:* 调用 / Role isolation: only Agents issue client:* calls.
        // 运行期 `Result`（**非** `debug_assert!`）→ release build 下隔离不变量同样硬化。
        if session.role != ClientRole::Agent {
            return Err(HandlerError::Isolation(
                "only agents may issue client:* calls".to_string(),
            ));
        }

        // 发起方无 office → 无从在任何 office 内定位目标 → flat 404（诚实 + 不泄露 + 不挂起）
        let Some(office_id) = session.office_id else {
            return Self::computer_not_found_value(computer_name);
        };

        // office-scoped 解析目标 Computer SID：跨 office 目标天然不可达 → 404（不泄露存在性）
        let Some(computer_sid) = state
            .session_manager
            .get_computer_sid_in_office(&office_id, computer_name)
        else {
            return Self::computer_not_found_value(computer_name);
        };

        // 目标 socket：SID 已解析但 socket 不在 → 目标已断连，按「不存在」回 404
        let target_socket = match computer_sid.parse() {
            Ok(parsed) => state
                .io
                .of(SMCP_NAMESPACE)
                .and_then(|op| op.get_socket(parsed)),
            Err(_) => None,
        };
        let Some(target_socket) = target_socket else {
            return Self::computer_not_found_value(computer_name);
        };

        // ── 在途断连守卫（#56 SRV-04）登记 ──────────────────────────────────────────
        // 针对目标 Computer SID 登记一个断连信号；目标飞行中断连时 `on_disconnect` 会 fire 它。
        // Drop guard 兜底注销，覆盖所有 return 路径（panic-safe）。
        let disconnect = state.inflight_disconnect.register(&computer_sid);
        let _signal_guard = InflightSignalGuard {
            registry: &state.inflight_disconnect,
            computer_sid: &computer_sid,
            notify: disconnect.clone(),
        };

        // TOCTOU 复查：解析目标 SID 与登记信号之间目标可能已断连（其信号 fire 早于本次 register →
        // 永不会唤醒我们）。`on_disconnect` 内 `unregister_session` 早于 `fire`，且 register/fire 经
        // 注册表 Mutex 串行化——故复查**我方 session_manager**（权威同步态）即可气密闭合竞速窗，
        // **不**依赖 socketioxide 摘除 socket 的内部时序。对齐 Python `get_sid_by_name is None` 复查。
        if state.session_manager.get_session(&computer_sid).is_none() {
            return Self::computer_not_found_value(computer_name);
        }

        // 转发并把「首个 ack」与「目标断连信号」竞速 / Relay, racing the first ack vs disconnect.
        let ack_result = target_socket.emit_with_ack(event, data);
        let ack_fut = async move {
            match ack_result {
                Ok(stream) => {
                    let mut pinned = Box::pin(stream);
                    match pinned.next().await {
                        Some((_, response)) => response,
                        None => Ok(Value::Null),
                    }
                }
                Err(_) => Ok(Value::Null),
            }
        };
        let raced = tokio::time::timeout(timeout, async {
            tokio::select! {
                // biased：就绪的响应优先于断连信号（目标响应后又断连时仍取回有效结果，
                // 对齐 Python `if call_task in done` 先判定）。
                biased;
                resp = ack_fut => Some(resp),
                _ = disconnect.notified() => None,
            }
        })
        .await;

        let response = match raced {
            // 真超时：原样回 Timeout，**绝不**转 404（守住「超时 ≠ 不存在」语义区分，对齐 Python）。
            Err(_) => {
                return Err(HandlerError::Timeout(format!(
                    "{event} timed out after {}s",
                    timeout.as_secs()
                )))
            }
            // 目标飞行中断连先到：目标中途消失 == 不存在，回 flat 404（与 #47/#92 同一 payload；
            // 协议 0.2.2：Server MAY 静默丢弃、不新增错误码）。
            Ok(None) => return Self::computer_not_found_value(computer_name),
            Ok(Some(Ok(response))) => response,
            Ok(Some(Err(e))) => {
                return Err(HandlerError::Timeout(format!(
                    "Failed to get response from computer: {e}"
                )))
            }
        };

        // 原发起方（Agent）在途断连容错：relay 等待期间发起方可能已断连。此时静默丢弃——不向已消失的
        // AckSender 投递（协议 0.2.2：Server MAY 不 ack、不投 ErrorPayload）。显式受控 `Result` 让调用点
        // 走 no-ack 分支，而非依赖 ack.send 的隐式 no-op；session 不可达不 panic。与前置发起方检查
        // （`get_session().ok_or_else(NotFound)`）同变体，语义自洽（会话存活性 ≠ 角色授权拒绝）。
        if state.session_manager.get_session(&sid).is_none() {
            return Err(HandlerError::Session(SessionError::NotFound(sid.clone())));
        }

        // bare 透传：成功 Ret 或 flat ErrorPayload 原样返回（无嵌套 envelope、无 "result" 剥取）。
        Ok(response)
    }

    /// 处理工具调用事件（经统一 [`Self::relay_client_call`] 转发）。
    async fn on_client_tool_call(
        socket: SocketRef,
        data: ToolCallReq,
        state: ServerState,
    ) -> Result<Value, HandlerError> {
        Self::relay_client_call(
            &socket,
            &data.computer,
            &data,
            smcp::events::CLIENT_TOOL_CALL,
            &state,
            tokio::time::Duration::from_secs(30),
        )
        .await
    }

    /// 处理获取工具列表事件（经统一 [`Self::relay_client_call`] 转发）。
    async fn on_client_get_tools(
        socket: SocketRef,
        data: GetToolsReq,
        state: ServerState,
    ) -> Result<Value, HandlerError> {
        Self::relay_client_call(
            &socket,
            &data.computer,
            &data,
            smcp::events::CLIENT_GET_TOOLS,
            &state,
            tokio::time::Duration::from_secs(30),
        )
        .await
    }

    /// 处理获取桌面信息事件（经统一 [`Self::relay_client_call`] 转发）。
    async fn on_client_get_desktop(
        socket: SocketRef,
        data: GetDesktopReq,
        state: ServerState,
    ) -> Result<Value, HandlerError> {
        Self::relay_client_call(
            &socket,
            &data.computer,
            &data,
            smcp::events::CLIENT_GET_DESKTOP,
            &state,
            tokio::time::Duration::from_secs(30),
        )
        .await
    }

    /// 处理获取计算机配置事件（经统一 [`Self::relay_client_call`] 转发）。
    async fn on_client_get_config(
        socket: SocketRef,
        data: GetComputerConfigReq,
        state: ServerState,
    ) -> Result<Value, HandlerError> {
        Self::relay_client_call(
            &socket,
            &data.computer,
            &data,
            smcp::events::CLIENT_GET_CONFIG,
            &state,
            tokio::time::Duration::from_secs(30),
        )
        .await
    }

    /// 透明转发 `client:get_skills` 至目标 Computer（SKILL 轻量元数据清单）/ relay `client:get_skills`.
    async fn on_client_get_skills(
        socket: SocketRef,
        data: GetSkillsReq,
        state: ServerState,
    ) -> Result<Value, HandlerError> {
        Self::relay_client_call(
            &socket,
            &data.computer,
            &data,
            smcp::events::CLIENT_GET_SKILLS,
            &state,
            tokio::time::Duration::from_secs(30),
        )
        .await
    }

    /// 透明转发 `client:get_skill` 至目标 Computer（单 SKILL 详情/资源；4016/4017 flat 透传）/ relay `client:get_skill`.
    async fn on_client_get_skill(
        socket: SocketRef,
        data: GetSkillReq,
        state: ServerState,
    ) -> Result<Value, HandlerError> {
        Self::relay_client_call(
            &socket,
            &data.computer,
            &data,
            smcp::events::CLIENT_GET_SKILL,
            &state,
            tokio::time::Duration::from_secs(30),
        )
        .await
    }

    /// 透明转发 `client:get_blob` 至目标 Computer（Server 不重组，逐 ack 透传分块）/ relay `client:get_blob`.
    async fn on_client_get_blob(
        socket: SocketRef,
        data: GetBlobReq,
        state: ServerState,
    ) -> Result<Value, HandlerError> {
        Self::relay_client_call(
            &socket,
            &data.computer,
            &data,
            smcp::events::CLIENT_GET_BLOB,
            &state,
            tokio::time::Duration::from_secs(30),
        )
        .await
    }

    /// 透明转发 `client:get_resources` 至目标 Computer（含 cursor 翻页；4014/4015 flat 透传）/ relay `client:get_resources`.
    async fn on_client_get_resources(
        socket: SocketRef,
        data: GetResourcesReq,
        state: ServerState,
    ) -> Result<Value, HandlerError> {
        Self::relay_client_call(
            &socket,
            &data.computer,
            &data,
            smcp::events::CLIENT_GET_RESOURCES,
            &state,
            tokio::time::Duration::from_secs(30),
        )
        .await
    }

    /// 处理桌面更新事件
    async fn on_server_update_desktop(
        socket: SocketRef,
        data: UpdateComputerConfigReq,
        state: ServerState,
    ) {
        let sid = socket.id.to_string();
        let session = match state.session_manager.get_session(&sid) {
            Some(s) => s,
            None => {
                warn!("SERVER_UPDATE_DESKTOP from unknown session sid={}", sid);
                return;
            }
        };

        // 角色断言：桌面更新通常由 Computer 发起
        if session.role != ClientRole::Computer {
            warn!(
                "SERVER_UPDATE_DESKTOP role mismatch: expected Computer, got {:?}, sid={}",
                session.role, sid
            );
            return;
        }

        let office_id = match session.office_id {
            Some(ref office_id) => office_id.clone(),
            None => {
                warn!(
                    "SERVER_UPDATE_DESKTOP but session not in office, sid={}",
                    sid
                );
                return;
            }
        };

        // 广播桌面更新通知（向 office 广播并跳过自己）
        let notification = UpdateMCPConfigNotification {
            computer: data.computer,
        };

        if let Err(e) = socket
            .to(office_id)
            .emit(smcp::events::NOTIFY_UPDATE_DESKTOP, &notification)
            .await
        {
            warn!("Failed to broadcast NOTIFY_UPDATE_DESKTOP: {}", e);
        }
    }

    /// 处理列出房间事件
    async fn on_server_list_room(
        socket: SocketRef,
        data: ListRoomReq,
        state: ServerState,
    ) -> ListRoomRet {
        // 获取发起者会话信息
        let sid = socket.id.to_string();
        let session = match state.session_manager.get_session(&sid) {
            Some(s) => s,
            None => {
                warn!("List room from unknown session sid={}", sid);
                return ListRoomRet {
                    sessions: vec![],
                    req_id: data.base.req_id,
                };
            }
        };

        // 房间隔离：仅可查询自己所在的 office。判定下沉到纯函数 [`Self::list_room_authorized`]
        // （运行期 `Result`/分支，**非** `debug_assert!`）→ release build 下隔离同样硬化，对标
        // Python `-O` 回归 `test_isolation_hardening`：断言被剥离时跨房间访问仍 MUST 拒绝、不泄露
        // 另一房间会话。无 office（None）同样不授权。
        if !Self::list_room_authorized(session.office_id.as_deref(), &data.office_id) {
            warn!(
                "list_room isolation rejected: session {} (office {:?}) requested room {}",
                sid, session.office_id, data.office_id
            );
            return ListRoomRet {
                sessions: vec![],
                req_id: data.base.req_id,
            };
        }

        // 获取指定办公室的所有会话
        let sessions = state
            .session_manager
            .get_sessions_in_office(&data.office_id);

        // 转换为 SessionInfo 列表
        let session_infos: Vec<SessionInfo> = sessions
            .into_iter()
            .map(|s| SessionInfo {
                sid: s.sid,
                name: s.name,
                role: s.role.into(),
                office_id: s.office_id.unwrap_or_default(),
                a2c_version: s.a2c_version,
            })
            .collect();

        ListRoomRet {
            sessions: session_infos,
            req_id: data.base.req_id,
        }
    }

    /// 处理加入房间的逻辑
    async fn handle_join_room(
        socket: SocketRef,
        session: &SessionData,
        office_id: &str,
        state: &ServerState,
    ) -> Result<(), HandlerError> {
        info!(
            "handle_join_room called: sid={}, office_id={}, role={:?}",
            socket.id, office_id, session.role
        );

        match Self::validate_join_room(session, office_id, state)? {
            JoinRoomDecision::Noop => {
                info!("Noop decision for sid={}", socket.id);
                Ok(())
            }
            JoinRoomDecision::Join => {
                info!("Joining room '{}' for sid={}", office_id, socket.id);
                socket.join(office_id.to_string());
                Ok(())
            }
            JoinRoomDecision::LeaveAndJoin { leave_office } => {
                info!(
                    "Leaving room '{}' and joining '{}' for sid={}",
                    leave_office, office_id, socket.id
                );

                // 构建离开通知（Python语义：切换房间前需要通知旧房间）
                let leave_notification = if session.role == ClientRole::Computer {
                    LeaveOfficeNotification {
                        office_id: leave_office.clone(),
                        computer: Some(session.name.clone()),
                        agent: None,
                    }
                } else {
                    LeaveOfficeNotification {
                        office_id: leave_office.clone(),
                        computer: None,
                        agent: Some(session.name.clone()),
                    }
                };

                // 向旧房间广播离开消息
                let _ = socket
                    .within(leave_office.clone())
                    .emit(smcp::events::NOTIFY_LEAVE_OFFICE, &leave_notification)
                    .await;

                socket.leave(leave_office);
                socket.join(office_id.to_string());
                Ok(())
            }
        }
    }

    /// `server:list_room` 房间隔离判定（纯函数）。/ Room-isolation predicate for `server:list_room`.
    ///
    /// 返回发起方（其 office 为 `session_office`）是否获准查询 `requested_office`——仅限本房间。
    /// 无 office（`None`）一律不授权。**运行期**判定（**非** `debug_assert!`/`cfg(debug_assertions)`），
    /// 故 release build 下隔离不变量同样硬化；对标 Python `-O` 隔离回归 `test_isolation_hardening`。
    fn list_room_authorized(session_office: Option<&str>, requested_office: &str) -> bool {
        session_office == Some(requested_office)
    }

    fn validate_join_room(
        session: &SessionData,
        office_id: &str,
        state: &ServerState,
    ) -> Result<JoinRoomDecision, HandlerError> {
        match session.role {
            ClientRole::Agent => {
                if let Some(current_office) = &session.office_id {
                    if current_office != office_id {
                        return Err(HandlerError::Session(SessionError::AgentAlreadyInRoom(
                            current_office.clone(),
                        )));
                    }
                    warn!(
                        "Agent sid: {} already in room: {}. 正在重复加入房间",
                        session.sid, current_office
                    );
                    return Ok(JoinRoomDecision::Noop);
                }

                if state
                    .session_manager
                    .has_agent_in_office(&office_id.to_string())
                {
                    return Err(HandlerError::Session(SessionError::AgentAlreadyExists));
                }
            }
            ClientRole::Computer => {
                if let Some(current_office) = &session.office_id {
                    if current_office != office_id {
                        if state
                            .session_manager
                            .has_computer_in_office(&office_id.to_string(), &session.name)
                        {
                            return Err(HandlerError::Session(
                                SessionError::ComputerAlreadyExists(
                                    session.name.clone(),
                                    office_id.to_string(),
                                ),
                            ));
                        }
                        return Ok(JoinRoomDecision::LeaveAndJoin {
                            leave_office: current_office.clone(),
                        });
                    }
                    warn!(
                        "Computer sid: {} already in room: {}. 正在重复加入房间",
                        session.sid, current_office
                    );
                    return Ok(JoinRoomDecision::Noop);
                }

                if state
                    .session_manager
                    .has_computer_in_office(&office_id.to_string(), &session.name)
                {
                    return Err(HandlerError::Session(SessionError::ComputerAlreadyExists(
                        session.name.clone(),
                        office_id.to_string(),
                    )));
                }
            }
        }

        Ok(JoinRoomDecision::Join)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum JoinRoomDecision {
    Noop,
    Join,
    LeaveAndJoin { leave_office: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::DefaultAuthenticationProvider;
    use serde_json;

    fn create_test_state() -> ServerState {
        let (_layer, io) = SocketIo::builder().build_layer();
        ServerState {
            session_manager: Arc::new(SessionManager::new()),
            auth_provider: Arc::new(DefaultAuthenticationProvider::new(
                Some("test_secret".to_string()),
                None,
            )),
            io: Arc::new(io),
            inflight_disconnect: Arc::new(InflightDisconnectRegistry::default()),
        }
    }

    #[tokio::test]
    async fn test_agent_join_office() {
        let (_layer, io) = SocketIo::builder().build_layer();
        let state = ServerState {
            session_manager: Arc::new(SessionManager::new()),
            auth_provider: Arc::new(DefaultAuthenticationProvider::new(
                Some("test_secret".to_string()),
                None,
            )),
            io: Arc::new(io.clone()),
            inflight_disconnect: Arc::new(InflightDisconnectRegistry::default()),
        };

        // 注册处理器
        SmcpHandler::register_handlers(&io, state.clone());

        // 测试逻辑需要实际的 Socket.IO 客户端连接
        // 这里只做基本的单元测试
        assert_eq!(state.session_manager.get_stats().total, 0);
    }

    #[test]
    fn test_handler_error_serialize() {
        // 协议 0.2.2：HandlerError 序列化为 flat ErrorPayload（顶层 code/message），
        // 禁止嵌套 {"error":{...}} envelope。断言 flat 形态，使回退到嵌套时测试失败。
        let err = HandlerError::InvalidRequest("bad".to_string());
        let v: serde_json::Value = serde_json::to_value(&err).unwrap();

        assert_eq!(v["code"], smcp::error_codes::BAD_REQUEST); // InvalidRequest → 400
        let message = v["message"].as_str().expect("message 应为顶层字符串");
        assert!(message.contains("Invalid request"));
        assert!(message.contains("bad"));
        assert!(v.get("error").is_none(), "禁止嵌套 envelope"); // 回退到 {"error":{...}} 即失败
    }

    // ── #56 SRV-04：在途断连信号注册表 ──────────────────────────────────────────────

    #[tokio::test]
    async fn test_inflight_signal_fire_wakes_waiter() {
        // 目标断连 fire 后，在途 relay 的 notified() MUST 立即就绪（即便先 fire 后 await——
        // notify_one 暂存 permit，闭合解析 SID 与登记之间的竞速窗）。
        let reg = InflightDisconnectRegistry::default();
        let notify = reg.register("computer-sid");
        reg.fire("computer-sid");
        tokio::time::timeout(std::time::Duration::from_millis(200), notify.notified())
            .await
            .expect("fire 后 notified 应立即就绪");
    }

    #[tokio::test]
    async fn test_inflight_signal_fire_wakes_all_for_same_target() {
        // 同一目标的多个在途 relay：fire 一次 MUST 全部唤醒。
        let reg = InflightDisconnectRegistry::default();
        let a = reg.register("computer-sid");
        let b = reg.register("computer-sid");
        reg.fire("computer-sid");
        for n in [a, b] {
            tokio::time::timeout(std::time::Duration::from_millis(200), n.notified())
                .await
                .expect("同目标全部在途信号均应被唤醒");
        }
    }

    #[tokio::test]
    async fn test_inflight_signal_discard_then_fire_is_noop() {
        // discard 后该信号已脱离注册表：fire 不应唤醒它（无 permit），且对空桶 fire 不 panic。
        let reg = InflightDisconnectRegistry::default();
        let notify = reg.register("computer-sid");
        reg.discard("computer-sid", &notify);
        reg.fire("computer-sid"); // 桶已回收：幂等、不 panic
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), notify.notified())
                .await
                .is_err(),
            "discard 后的信号不应被 fire 唤醒"
        );
    }

    #[test]
    fn test_inflight_signal_fire_unknown_target_is_noop() {
        // 无任何在途请求时对未知/已断连目标 fire：幂等、不 panic（on_disconnect 对每个掉线 sid 都会 fire）。
        let reg = InflightDisconnectRegistry::default();
        reg.fire("never-registered");
    }

    #[test]
    fn test_isolation_error_maps_to_forbidden_and_is_flat() {
        // 隔离拒绝错误码归入 FORBIDDEN（仅内部诊断，永不进 client:* ack）；序列化仍为 flat 形态。
        let err = HandlerError::Isolation("only agents".to_string());
        assert_eq!(err.error_code(), smcp::error_codes::FORBIDDEN);
        let v: serde_json::Value = serde_json::to_value(&err).unwrap();
        assert_eq!(v["code"], smcp::error_codes::FORBIDDEN);
        assert!(v.get("error").is_none(), "禁止嵌套 envelope");
    }

    #[test]
    fn test_list_room_authorized_is_runtime_isolation_invariant() {
        // 对标 Python `-O` 回归 test_isolation_hardening：判定是运行期纯函数（**非** debug_assert!），
        // 故 release build 下同样硬化。跨房间 / 无 office MUST 拒绝；仅同房间放行。
        assert!(
            !SmcpHandler::list_room_authorized(Some("office_A"), "office_B"),
            "跨房间 MUST 拒绝（不得泄露另一房间会话）"
        );
        assert!(
            !SmcpHandler::list_room_authorized(None, "office_B"),
            "无 office MUST 拒绝"
        );
        assert!(
            SmcpHandler::list_room_authorized(Some("office1"), "office1"),
            "同房间应放行"
        );
    }

    #[test]
    fn test_validate_join_room_agent_already_in_other_room() {
        let state = create_test_state();
        let session = SessionData::new("sid1".to_string(), "a".to_string(), ClientRole::Agent)
            .with_office_id("office1".to_string());

        let err = SmcpHandler::validate_join_room(&session, "office2", &state).unwrap_err();
        assert!(matches!(
            err,
            HandlerError::Session(SessionError::AgentAlreadyInRoom(o)) if o == "office1"
        ));
    }

    #[test]
    fn test_validate_join_room_agent_already_exists() {
        let state = create_test_state();
        let office_id = "office1".to_string();
        let existing_agent = SessionData::new(
            "sid_agent".to_string(),
            "agent1".to_string(),
            ClientRole::Agent,
        )
        .with_office_id(office_id.clone());
        state
            .session_manager
            .register_session(existing_agent)
            .unwrap();

        let new_agent = SessionData::new(
            "sid_new".to_string(),
            "agent2".to_string(),
            ClientRole::Agent,
        );
        let err = SmcpHandler::validate_join_room(&new_agent, &office_id, &state).unwrap_err();
        assert!(matches!(
            err,
            HandlerError::Session(SessionError::AgentAlreadyExists)
        ));
    }

    #[test]
    fn test_validate_join_room_computer_duplicate_name_in_office() {
        let state = create_test_state();
        let office_id = "office1".to_string();
        let existing = SessionData::new(
            "sid_c1".to_string(),
            "computer1".to_string(),
            ClientRole::Computer,
        )
        .with_office_id(office_id.clone());
        state.session_manager.register_session(existing).unwrap();

        let new_same_name = SessionData::new(
            "sid_c2".to_string(),
            "computer1".to_string(),
            ClientRole::Computer,
        );
        let err = SmcpHandler::validate_join_room(&new_same_name, &office_id, &state).unwrap_err();
        assert!(matches!(
            err,
            HandlerError::Session(SessionError::ComputerAlreadyExists(name, office))
                if name == "computer1" && office == "office1"
        ));
    }

    #[test]
    fn test_validate_join_room_computer_switch_room() {
        let state = create_test_state();
        let session = SessionData::new(
            "sid_c".to_string(),
            "computer1".to_string(),
            ClientRole::Computer,
        )
        .with_office_id("office_old".to_string());

        let decision = SmcpHandler::validate_join_room(&session, "office_new", &state).unwrap();
        assert_eq!(
            decision,
            JoinRoomDecision::LeaveAndJoin {
                leave_office: "office_old".to_string()
            }
        );
    }

    #[test]
    fn test_validate_join_room_same_room_noop() {
        let state = create_test_state();
        let session = SessionData::new("sid".to_string(), "c".to_string(), ClientRole::Computer)
            .with_office_id("office1".to_string());

        let decision = SmcpHandler::validate_join_room(&session, "office1", &state).unwrap();
        assert_eq!(decision, JoinRoomDecision::Noop);
    }

    #[test]
    fn test_validate_join_room_agent_first_time_join() {
        let state = create_test_state();
        let session = SessionData::new("sid_agent".to_string(), "a".to_string(), ClientRole::Agent);

        let decision = SmcpHandler::validate_join_room(&session, "office1", &state).unwrap();
        assert_eq!(decision, JoinRoomDecision::Join);
    }

    #[test]
    fn test_validate_join_room_computer_first_time_join() {
        let state = create_test_state();
        let session = SessionData::new(
            "sid_computer".to_string(),
            "computer1".to_string(),
            ClientRole::Computer,
        );

        let decision = SmcpHandler::validate_join_room(&session, "office1", &state).unwrap();
        assert_eq!(decision, JoinRoomDecision::Join);
    }

    #[test]
    fn test_enter_office_notification_computer() {
        let computer_name = "computer1".to_string();
        let office_id = "office1".to_string();

        let notification = EnterOfficeNotification {
            office_id: office_id.clone(),
            computer: Some(computer_name.clone()),
            agent: None,
        };

        assert_eq!(notification.office_id, office_id);
        assert_eq!(notification.computer, Some(computer_name));
        assert_eq!(notification.agent, None);
    }

    #[test]
    fn test_enter_office_notification_agent() {
        let agent_name = "agent1".to_string();
        let office_id = "office1".to_string();

        let notification = EnterOfficeNotification {
            office_id: office_id.clone(),
            computer: None,
            agent: Some(agent_name.clone()),
        };

        assert_eq!(notification.office_id, office_id);
        assert_eq!(notification.computer, None);
        assert_eq!(notification.agent, Some(agent_name));
    }

    #[test]
    fn test_leave_office_notification_computer() {
        let computer_name = "computer1".to_string();
        let office_id = "office1".to_string();

        let notification = LeaveOfficeNotification {
            office_id: office_id.clone(),
            computer: Some(computer_name.clone()),
            agent: None,
        };

        assert_eq!(notification.office_id, office_id);
        assert_eq!(notification.computer, Some(computer_name));
        assert_eq!(notification.agent, None);
    }

    #[test]
    fn test_update_tool_list_notification() {
        let computer_name = "computer1".to_string();

        let notification = UpdateToolListNotification {
            computer: computer_name.clone(),
        };

        assert_eq!(notification.computer, computer_name);
    }

    #[test]
    fn test_update_mcp_config_notification() {
        let computer_name = "computer1".to_string();

        let notification = UpdateMCPConfigNotification {
            computer: computer_name.clone(),
        };

        assert_eq!(notification.computer, computer_name);
    }

    #[test]
    fn test_notification_serialization() {
        // 验证通知类型序列化正确性
        let tool_list_notification = UpdateToolListNotification {
            computer: "computer1".to_string(),
        };

        let json = serde_json::to_string(&tool_list_notification).unwrap();
        assert!(json.contains("\"computer\":\"computer1\""));

        let mcp_config_notification = UpdateMCPConfigNotification {
            computer: "computer1".to_string(),
        };

        let json = serde_json::to_string(&mcp_config_notification).unwrap();
        assert!(json.contains("\"computer\":\"computer1\""));
    }
}
