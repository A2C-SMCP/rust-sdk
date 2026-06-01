//! SMCP 协议处理器 / SMCP protocol handler

use crate::auth::{AuthError, AuthenticationProvider};
use crate::session::{ClientRole, SessionData, SessionError, SessionManager};
use futures_util::StreamExt;
use serde_json::Value;
use smcp::*;
use socketioxide::{
    extract::{AckSender, Data, SocketRef},
    SocketIo,
};
use std::sync::Arc;
use thiserror::Error;
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

/// 服务器状态
#[derive(Clone, Debug)]
pub struct ServerState {
    /// 会话管理器
    pub session_manager: Arc<SessionManager>,
    /// 认证提供者
    pub auth_provider: Arc<dyn AuthenticationProvider>,
    /// SocketIo 实例引用，用于跨 socket 通信
    pub io: Arc<SocketIo>,
}

/// SMCP 事件处理器
pub struct SmcpHandler;

impl SmcpHandler {
    /// 注册所有事件处理器
    pub fn register_handlers(io: &SocketIo, state: ServerState) {
        // 注册命名空间和连接处理器
        io.ns(SMCP_NAMESPACE, move |socket: SocketRef| {
            let state = state.clone();
            async move {
                if let Err(e) = Self::on_connect(socket.clone(), &state).await {
                    error!("on_connect failed: {}", e);
                    return;
                }

                // 连接时注册所有事件处理器
                Self::handle_connection(socket, state)
            }
        });
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
    }

    /// 处理连接事件
    async fn on_connect(socket: SocketRef, state: &ServerState) -> Result<(), HandlerError> {
        info!(
            "SocketIO Client {} connecting to {}...",
            socket.id, SMCP_NAMESPACE
        );

        // 获取请求头进行认证
        let headers = socket.req_parts().headers.clone();
        let auth_data = socket.req_parts().extensions.get::<Value>();

        // 认证
        state
            .auth_provider
            .authenticate(&headers, auth_data)
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
    /// - 发起方非 Agent / 发起方会话已断连 → `Err`（隔离拒绝）：调用点**不投递协议 ack**
    ///   （镜像 Python `raise` 语义，发起方侧自行超时；不泄露 Computer 存在性、不造非协议错误码）。
    ///
    /// 在途断连竞速（目标 Computer 中途消失）归 SRV-04 (#56)，此处不处理。
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

        // 角色隔离：仅 Agent 可发起 client:* 调用 / Role isolation: only Agents issue client:* calls
        if session.role != ClientRole::Agent {
            return Err(HandlerError::InvalidRequest(
                "Only agents can issue client:* calls".to_string(),
            ));
        }

        // 发起方无 office → 无从在任何 office 内定位目标 → flat 404（诚实 + 不泄露 + 不挂起）
        let Some(office_id) = session.office_id else {
            return Ok(serde_json::to_value(build_computer_not_found_error(
                computer_name,
            ))?);
        };

        // office-scoped 解析目标 Computer SID：跨 office 目标天然不可达 → 404（不泄露存在性）
        let Some(computer_sid) = state
            .session_manager
            .get_computer_sid_in_office(&office_id, computer_name)
        else {
            return Ok(serde_json::to_value(build_computer_not_found_error(
                computer_name,
            ))?);
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
            return Ok(serde_json::to_value(build_computer_not_found_error(
                computer_name,
            ))?);
        };

        // 转发并等待首个 ack / Relay and await the first ack
        let ack_result = target_socket.emit_with_ack(event, data);
        let response = match tokio::time::timeout(timeout, async move {
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
        })
        .await
        {
            Ok(Ok(response)) => response,
            Ok(Err(e)) => {
                return Err(HandlerError::Timeout(format!(
                    "Failed to get response from computer: {e}"
                )))
            }
            Err(_) => {
                return Err(HandlerError::Timeout(format!(
                    "{event} timed out after {}s",
                    timeout.as_secs()
                )))
            }
        };

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

        // 权限校验：只能查询自己所在的办公室
        let session_office_id = match session.office_id {
            Some(id) => id,
            None => {
                warn!("Session {} not in any office", sid);
                return ListRoomRet {
                    sessions: vec![],
                    req_id: data.base.req_id,
                };
            }
        };

        if session_office_id != data.office_id {
            warn!(
                "Session {} trying to list room {} but in office {}",
                sid, data.office_id, session_office_id
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
