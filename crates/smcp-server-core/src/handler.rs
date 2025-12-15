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

impl serde::Serialize for HandlerError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
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
                let result = Self::on_client_tool_call(socket, data, state_tool_call.clone()).await;
                let _ = ack.send(&result);
            },
        );

        let state_get_tools = state.clone();
        socket.on(
            smcp::events::CLIENT_GET_TOOLS,
            move |socket: SocketRef, Data::<GetToolsReq>(data), ack: AckSender| async move {
                let result = Self::on_client_get_tools(socket, data, state_get_tools.clone()).await;
                let _ = ack.send(&result);
            },
        );

        let state_get_desktop = state.clone();
        socket.on(
            smcp::events::CLIENT_GET_DESKTOP,
            move |socket: SocketRef, Data::<GetDesktopReq>(data), ack: AckSender| async move {
                let result =
                    Self::on_client_get_desktop(socket, data, state_get_desktop.clone()).await;
                let _ = ack.send(&result);
            },
        );

        let state_get_config = state.clone();
        socket.on(
            smcp::events::CLIENT_GET_CONFIG,
            move |socket: SocketRef, Data::<GetComputerConfigReq>(data), ack: AckSender| async move {
                let result = Self::on_client_get_config(socket, data, state_get_config.clone()).await;
                let _ = ack.send(&result);
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
                // 创建新会话
                let new_session = SessionData::new(sid.clone(), requested_name, requested_role);

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
    async fn on_client_tool_call(
        socket: SocketRef,
        data: ToolCallReq,
        state: ServerState,
    ) -> Result<Value, HandlerError> {
        // 获取 Agent 的会话信息
        let sid = socket.id.to_string();
        let session = state
            .session_manager
            .get_session(&sid)
            .ok_or_else(|| HandlerError::Session(SessionError::NotFound(sid.clone())))?;

        // 验证角色必须是 Agent
        if session.role != ClientRole::Agent {
            return Err(HandlerError::InvalidRequest(
                "Only agents can make tool calls".to_string(),
            ));
        }

        // 验证 Agent 在某个办公室内
        let office_id = session.office_id.ok_or_else(|| {
            HandlerError::InvalidRequest(
                "Agent must be in an office to make tool calls".to_string(),
            )
        })?;

        // 查找目标 Computer 的 sid
        let computer_sid = state
            .session_manager
            .get_computer_sid_in_office(&office_id, &data.computer)
            .ok_or_else(|| {
                HandlerError::InvalidRequest(format!(
                    "Computer '{}' not found in office",
                    data.computer
                ))
            })?;

        // 获取目标 socket
        let target_socket = state
            .io
            .of(SMCP_NAMESPACE)
            .and_then(|op| op.get_socket(computer_sid.parse().unwrap()))
            .ok_or_else(|| {
                HandlerError::InvalidRequest("Target computer socket not found".to_string())
            })?;

        // 转发请求并等待响应
        let timeout = tokio::time::Duration::from_secs(30);
        let ack_result = target_socket.emit_with_ack(smcp::events::CLIENT_TOOL_CALL, &data);

        match tokio::time::timeout(timeout, async move {
            match ack_result {
                Ok(stream) => {
                    let mut pinned = Box::pin(stream);
                    match pinned.next().await {
                        Some((_, response)) => response,
                        None => Ok(serde_json::Value::Null),
                    }
                }
                Err(_) => Ok(serde_json::Value::Null),
            }
        })
        .await
        {
            Ok(Ok(response)) => {
                // 解析响应
                match response {
                    serde_json::Value::Object(mut map) => {
                        // 提取 result 字段
                        let result = map.remove("result").unwrap_or(serde_json::Value::Null);
                        Ok(result)
                    }
                    _ => Ok(response),
                }
            }
            Ok(Err(e)) => Err(HandlerError::Timeout(format!(
                "Failed to get response from computer: {}",
                e
            ))),
            Err(_) => Err(HandlerError::Timeout(
                "Tool call timed out after 30 seconds".to_string(),
            )),
        }
    }

    /// 处理获取工具列表事件
    async fn on_client_get_tools(
        socket: SocketRef,
        data: GetToolsReq,
        state: ServerState,
    ) -> Result<GetToolsRet, HandlerError> {
        // 获取 Agent 的会话信息
        let sid = socket.id.to_string();
        let session = state
            .session_manager
            .get_session(&sid)
            .ok_or_else(|| HandlerError::Session(SessionError::NotFound(sid.clone())))?;

        // 验证角色必须是 Agent
        if session.role != ClientRole::Agent {
            return Err(HandlerError::InvalidRequest(
                "Only agents can get tools".to_string(),
            ));
        }

        // 验证 Agent 在某个办公室内
        let office_id = session.office_id.ok_or_else(|| {
            HandlerError::InvalidRequest("Agent must be in an office to get tools".to_string())
        })?;

        // 查找目标 Computer 的 sid
        let computer_sid = state
            .session_manager
            .get_computer_sid_in_office(&office_id, &data.computer)
            .ok_or_else(|| {
                HandlerError::InvalidRequest(format!(
                    "Computer '{}' not found in office",
                    data.computer
                ))
            })?;

        // 获取目标 socket
        let target_socket = state
            .io
            .of(SMCP_NAMESPACE)
            .and_then(|op| op.get_socket(computer_sid.parse().unwrap()))
            .ok_or_else(|| {
                HandlerError::InvalidRequest("Target computer socket not found".to_string())
            })?;

        // 转发请求并等待响应
        let timeout = tokio::time::Duration::from_secs(30);
        let ack_result = target_socket.emit_with_ack(smcp::events::CLIENT_GET_TOOLS, &data);

        match tokio::time::timeout(timeout, async move {
            match ack_result {
                Ok(stream) => {
                    let mut pinned = Box::pin(stream);
                    match pinned.next().await {
                        Some((_, response)) => response,
                        None => Ok(serde_json::Value::Null),
                    }
                }
                Err(_) => Ok(serde_json::Value::Null),
            }
        })
        .await
        {
            Ok(Ok(response)) => {
                // 解析响应
                serde_json::from_value(response).map_err(|e| {
                    HandlerError::InvalidRequest(format!("Failed to parse response: {}", e))
                })
            }
            Ok(Err(e)) => Err(HandlerError::Timeout(format!(
                "Failed to get response from computer: {}",
                e
            ))),
            Err(_) => Err(HandlerError::Timeout(
                "Get tools timed out after 30 seconds".to_string(),
            )),
        }
    }

    /// 处理获取桌面信息事件
    async fn on_client_get_desktop(
        socket: SocketRef,
        data: GetDesktopReq,
        state: ServerState,
    ) -> Result<GetDesktopRet, HandlerError> {
        // 获取 Agent 的会话信息
        let sid = socket.id.to_string();
        let session = state
            .session_manager
            .get_session(&sid)
            .ok_or_else(|| HandlerError::Session(SessionError::NotFound(sid.clone())))?;

        // 验证角色必须是 Agent
        if session.role != ClientRole::Agent {
            return Err(HandlerError::InvalidRequest(
                "Only agents can get desktop".to_string(),
            ));
        }

        // 验证 Agent 在某个办公室内
        let office_id = session.office_id.ok_or_else(|| {
            HandlerError::InvalidRequest("Agent must be in an office to get desktop".to_string())
        })?;

        // 查找目标 Computer 的 sid
        let computer_sid = state
            .session_manager
            .get_computer_sid_in_office(&office_id, &data.computer)
            .ok_or_else(|| {
                HandlerError::InvalidRequest(format!(
                    "Computer '{}' not found in office",
                    data.computer
                ))
            })?;

        // 获取目标 socket
        let target_socket = state
            .io
            .of(SMCP_NAMESPACE)
            .and_then(|op| op.get_socket(computer_sid.parse().unwrap()))
            .ok_or_else(|| {
                HandlerError::InvalidRequest("Target computer socket not found".to_string())
            })?;

        // 转发请求并等待响应
        let timeout = tokio::time::Duration::from_secs(30);
        let ack_result = target_socket.emit_with_ack(smcp::events::CLIENT_GET_DESKTOP, &data);

        match tokio::time::timeout(timeout, async move {
            match ack_result {
                Ok(stream) => {
                    let mut pinned = Box::pin(stream);
                    match pinned.next().await {
                        Some((_, response)) => response,
                        None => Ok(serde_json::Value::Null),
                    }
                }
                Err(_) => Ok(serde_json::Value::Null),
            }
        })
        .await
        {
            Ok(Ok(response)) => {
                // 解析响应
                serde_json::from_value(response).map_err(|e| {
                    HandlerError::InvalidRequest(format!("Failed to parse response: {}", e))
                })
            }
            Ok(Err(e)) => Err(HandlerError::Timeout(format!(
                "Failed to get response from computer: {}",
                e
            ))),
            Err(_) => Err(HandlerError::Timeout(
                "Get desktop timed out after 30 seconds".to_string(),
            )),
        }
    }

    /// 处理获取计算机配置事件
    async fn on_client_get_config(
        socket: SocketRef,
        data: GetComputerConfigReq,
        state: ServerState,
    ) -> Result<GetComputerConfigRet, HandlerError> {
        // 获取 Agent 的会话信息
        let sid = socket.id.to_string();
        let session = state
            .session_manager
            .get_session(&sid)
            .ok_or_else(|| HandlerError::Session(SessionError::NotFound(sid.clone())))?;

        // 验证角色必须是 Agent
        if session.role != ClientRole::Agent {
            return Err(HandlerError::InvalidRequest(
                "Only agents can get config".to_string(),
            ));
        }

        // 验证 Agent 在某个办公室内
        let office_id = session.office_id.ok_or_else(|| {
            HandlerError::InvalidRequest("Agent must be in an office to get config".to_string())
        })?;

        // 查找目标 Computer 的 sid
        let computer_sid = state
            .session_manager
            .get_computer_sid_in_office(&office_id, &data.computer)
            .ok_or_else(|| {
                HandlerError::InvalidRequest(format!(
                    "Computer '{}' not found in office",
                    data.computer
                ))
            })?;

        // 获取目标 socket
        let target_socket = state
            .io
            .of(SMCP_NAMESPACE)
            .and_then(|op| op.get_socket(computer_sid.parse().unwrap()))
            .ok_or_else(|| {
                HandlerError::InvalidRequest("Target computer socket not found".to_string())
            })?;

        // 转发请求并等待响应
        let timeout = tokio::time::Duration::from_secs(30);
        let ack_result = target_socket.emit_with_ack(smcp::events::CLIENT_GET_CONFIG, &data);

        match tokio::time::timeout(timeout, async move {
            match ack_result {
                Ok(stream) => {
                    let mut pinned = Box::pin(stream);
                    match pinned.next().await {
                        Some((_, response)) => response,
                        None => Ok(serde_json::Value::Null),
                    }
                }
                Err(_) => Ok(serde_json::Value::Null),
            }
        })
        .await
        {
            Ok(Ok(response)) => {
                // 解析响应
                serde_json::from_value(response).map_err(|e| {
                    HandlerError::InvalidRequest(format!("Failed to parse response: {}", e))
                })
            }
            Ok(Err(e)) => Err(HandlerError::Timeout(format!(
                "Failed to get response from computer: {}",
                e
            ))),
            Err(_) => Err(HandlerError::Timeout(
                "Get config timed out after 30 seconds".to_string(),
            )),
        }
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
        let err = HandlerError::InvalidRequest("bad".to_string());
        let json = serde_json::to_string(&err).unwrap();
        assert!(json.contains("Invalid request"));
        assert!(json.contains("bad"));
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
