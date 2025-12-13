//! SMCP 协议处理器 / SMCP protocol handler

use crate::auth::{AuthError, AuthenticationProvider};
use crate::session::{ClientRole, SessionData, SessionError, SessionManager};
use serde_json::Value;
use smcp::*;
use socketioxide::{
    extract::{AckSender, Data, SocketRef, State},
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
}

/// SMCP 事件处理器
pub struct SmcpHandler;

impl SmcpHandler {
    /// 注册所有事件处理器
    pub fn register_handlers(io: &SocketIo) {
        // 注册命名空间和连接处理器
        io.ns(SMCP_NAMESPACE, |socket: SocketRef, state: State<ServerState>| async move {
            if let Err(e) = Self::on_connect(socket.clone(), socketioxide::extract::State(state.clone())).await {
                error!("on_connect failed: {}", e);
                return;
            }

            // 连接时注册所有事件处理器
            Self::handle_connection(socket, state)
        });
    }

    /// 处理连接并注册事件处理器
    fn handle_connection(socket: SocketRef, _state: State<ServerState>) {
        // 注册各种事件处理器
        socket.on_disconnect(|socket: SocketRef, state: State<ServerState>| async move {
            Self::on_disconnect(socket, state).await
        });

        socket.on(smcp::events::SERVER_JOIN_OFFICE, move |socket: SocketRef, Data::<EnterOfficeReq>(data), ack: AckSender, state: State<ServerState>| {
            async move {
                let result = Self::on_server_join_office(socket, data, state).await;
                let _ = ack.send(&result);
            }
        });

        socket.on(smcp::events::SERVER_LEAVE_OFFICE, move |socket: SocketRef, Data::<LeaveOfficeReq>(data), ack: AckSender, state: State<ServerState>| {
            async move {
                let result = Self::on_server_leave_office(socket, data, state).await;
                let _ = ack.send(&result);
            }
        });

        socket.on(smcp::events::SERVER_TOOL_CALL_CANCEL, move |socket: SocketRef, Data::<AgentCallData>(data), state: State<ServerState>| {
            async move {
                Self::on_server_tool_call_cancel(socket, data, state).await
            }
        });

        socket.on(smcp::events::SERVER_UPDATE_CONFIG, move |socket: SocketRef, Data::<UpdateComputerConfigReq>(data), state: State<ServerState>| {
            async move {
                Self::on_server_update_config(socket, data, state).await
            }
        });

        socket.on(smcp::events::SERVER_UPDATE_TOOL_LIST, move |socket: SocketRef, Data::<UpdateComputerConfigReq>(data), state: State<ServerState>| {
            async move {
                Self::on_server_update_tool_list(socket, data, state).await
            }
        });

        socket.on(smcp::events::CLIENT_TOOL_CALL, move |socket: SocketRef, Data::<ToolCallReq>(data), ack: AckSender, state: State<ServerState>| {
            async move {
                let result = Self::on_client_tool_call(socket, data, state).await;
                let _ = ack.send(&result);
            }
        });

        socket.on(smcp::events::CLIENT_GET_TOOLS, move |socket: SocketRef, Data::<GetToolsReq>(data), ack: AckSender, state: State<ServerState>| {
            async move {
                let result = Self::on_client_get_tools(socket, data, state).await;
                let _ = ack.send(&result);
            }
        });

        socket.on(smcp::events::CLIENT_GET_DESKTOP, move |socket: SocketRef, Data::<GetDesktopReq>(data), ack: AckSender, state: State<ServerState>| {
            async move {
                let result = Self::on_client_get_desktop(socket, data, state).await;
                let _ = ack.send(&result);
            }
        });

        socket.on(smcp::events::SERVER_UPDATE_DESKTOP, move |socket: SocketRef, Data::<UpdateComputerConfigReq>(data), state: State<ServerState>| {
            async move {
                Self::on_server_update_desktop(socket, data, state).await
            }
        });

        socket.on(smcp::events::SERVER_LIST_ROOM, move |socket: SocketRef, Data::<ListRoomReq>(data), ack: AckSender, state: State<ServerState>| {
            async move {
                let result = Self::on_server_list_room(socket, data, state).await;
                let _ = ack.send(&result);
            }
        });
    }

    /// 处理连接事件
    async fn on_connect(socket: SocketRef, state: State<ServerState>) -> Result<(), HandlerError> {
        info!("SocketIO Client {} connecting to {}...", socket.id, SMCP_NAMESPACE);

        // 获取请求头进行认证
        let headers = socket.req_parts().headers.clone();
        let auth_data = socket.req_parts().extensions.get::<Value>();

        // 认证
        state.auth_provider
            .authenticate(&headers, auth_data)
            .await?;

        info!("SocketIO Client {} connected successfully to {}", socket.id, SMCP_NAMESPACE);
        Ok(())
    }

    /// 处理断开连接事件
    async fn on_disconnect(socket: SocketRef, state: State<ServerState>) {
        info!("SocketIO Client {} disconnecting from {}...", socket.id, SMCP_NAMESPACE);

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

        info!("SocketIO Client {} disconnected from {}", socket.id, SMCP_NAMESPACE);
    }

    /// 处理加入办公室事件
    async fn on_server_join_office(
        socket: SocketRef,
        data: EnterOfficeReq,
        state: State<ServerState>,
    ) -> Result<(bool, Option<String>), HandlerError> {
        let sid = socket.id.to_string();
        
        // 获取或创建会话
        let session = match state.session_manager.get_session(&sid) {
            Some(s) => s,
            None => {
                // 从认证信息或请求数据中获取角色和名称
                let role = ClientRole::from(data.role.clone());
                let name = data.name.clone();
                
                let new_session = SessionData::new(sid.clone(), name, role)
                    .with_office_id(data.office_id.clone());
                
                state.session_manager.register_session(new_session.clone())?;
                new_session
            }
        };

        // 检查并加入房间
        Self::handle_join_room(socket.clone(), &session, &data.office_id, &state).await?;

        // 更新会话的办公室 ID
        state.session_manager.update_office_id(&sid, Some(data.office_id.clone()))?;

        // 构建通知数据
        let notification_data = if session.role == ClientRole::Computer {
            EnterOfficeNotification {
                office_id: data.office_id.clone(),
                computer: Some(session.name),
                agent: None,
            }
        } else {
            EnterOfficeNotification {
                office_id: data.office_id.clone(),
                computer: None,
                agent: Some(session.name),
            }
        };

        // 广播加入消息
        let _ = socket
            .within(data.office_id.clone())
            .emit(smcp::events::NOTIFY_ENTER_OFFICE, &notification_data)
            .await;

        Ok((true, None))
    }

    /// 处理离开办公室事件
    async fn on_server_leave_office(
        socket: SocketRef,
        data: LeaveOfficeReq,
        state: State<ServerState>,
    ) -> Result<(bool, Option<String>), HandlerError> {
        let sid = socket.id.to_string();
        
        // 获取会话
        let session = state.session_manager.get_session(&sid)
            .ok_or_else(|| HandlerError::Session(SessionError::NotFound(sid.clone())))?;

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
        state.session_manager.update_office_id(&sid, None)?;
        socket.leave(data.office_id.clone());

        Ok((true, None))
    }

    /// 处理工具调用取消事件
    async fn on_server_tool_call_cancel(
        socket: SocketRef,
        data: AgentCallData,
        _state: State<ServerState>,
    ) {
        // 广播取消通知
        let _ = socket.emit(smcp::events::NOTIFY_TOOL_CALL_CANCEL, &data);
    }

    /// 处理配置更新事件
    async fn on_server_update_config(
        socket: SocketRef,
        data: UpdateComputerConfigReq,
        _state: State<ServerState>,
    ) {
        // 广播配置更新通知
        let notification = UpdateMCPConfigNotification {
            computer: data.computer,
        };
        
        let _ = socket.emit(smcp::events::NOTIFY_UPDATE_CONFIG, &notification);
    }

    /// 处理工具列表更新事件
    async fn on_server_update_tool_list(
        socket: SocketRef,
        data: UpdateComputerConfigReq,
        _state: State<ServerState>,
    ) {
        // 广播工具列表更新通知
        let notification = UpdateMCPConfigNotification {
            computer: data.computer,
        };
        
        let _ = socket.emit(smcp::events::NOTIFY_UPDATE_TOOL_LIST, &notification);
    }

    /// 处理客户端工具调用事件
    async fn on_client_tool_call(
        _socket: SocketRef,
        _data: ToolCallReq,
        _state: State<ServerState>,
    ) -> Result<Value, HandlerError> {
        // TODO: 实现消息转发
        // 由于 socketioxide 0.16.3 的 API 限制，暂时返回错误
        Err(HandlerError::InvalidRequest("Message forwarding not yet implemented".to_string()))
    }

    /// 处理获取工具列表事件
    async fn on_client_get_tools(
        _socket: SocketRef,
        _data: GetToolsReq,
        _state: State<ServerState>,
    ) -> Result<GetToolsRet, HandlerError> {
        // TODO: 实现消息转发
        // 由于 socketioxide 0.16.3 的 API 限制，暂时返回错误
        Err(HandlerError::InvalidRequest("Message forwarding not yet implemented".to_string()))
    }

    /// 处理获取桌面信息事件
    async fn on_client_get_desktop(
        _socket: SocketRef,
        _data: GetDesktopReq,
        _state: State<ServerState>,
    ) -> Result<GetDesktopRet, HandlerError> {
        // TODO: 实现消息转发
        // 由于 socketioxide 0.16.3 的 API 限制，暂时返回错误
        Err(HandlerError::InvalidRequest("Message forwarding not yet implemented".to_string()))
    }

    /// 处理桌面更新事件
    async fn on_server_update_desktop(
        socket: SocketRef,
        data: UpdateComputerConfigReq,
        _state: State<ServerState>,
    ) {
        // 广播桌面更新通知
        let notification = UpdateMCPConfigNotification {
            computer: data.computer,
        };
        
        let _ = socket.emit(smcp::events::NOTIFY_UPDATE_DESKTOP, &notification);
    }

    /// 处理列出房间事件
    async fn on_server_list_room(
        _socket: SocketRef,
        data: ListRoomReq,
        state: State<ServerState>,
    ) -> Result<ListRoomRet, HandlerError> {
        // 获取指定办公室的所有会话
        let sessions = state.session_manager.get_sessions_in_office(&data.office_id);
        
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

        Ok(ListRoomRet {
            sessions: session_infos,
            req_id: data.base.req_id,
        })
    }

    /// 处理加入房间的逻辑
    async fn handle_join_room(
        socket: SocketRef,
        session: &SessionData,
        office_id: &str,
        state: &State<ServerState>,
    ) -> Result<(), HandlerError> {
        match Self::validate_join_room(session, office_id, &*state)? {
            JoinRoomDecision::Noop => Ok(()),
            JoinRoomDecision::Join => {
                socket.join(office_id.to_string());
                Ok(())
            }
            JoinRoomDecision::LeaveAndJoin { leave_office } => {
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
                            return Err(HandlerError::Session(SessionError::ComputerAlreadyExists(
                                session.name.clone(),
                                office_id.to_string(),
                            )));
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
        ServerState {
            session_manager: Arc::new(SessionManager::new()),
            auth_provider: Arc::new(DefaultAuthenticationProvider::new(
                Some("test_secret".to_string()),
                None,
            )),
        }
    }

    #[tokio::test]
    async fn test_agent_join_office() {
        let state = create_test_state();
        let (_layer, io) = SocketIo::builder()
            .with_state(state.clone())
            .build_layer();
        
        // 注册处理器
        SmcpHandler::register_handlers(&io);
        
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
        state.session_manager.register_session(existing_agent).unwrap();

        let new_agent =
            SessionData::new("sid_new".to_string(), "agent2".to_string(), ClientRole::Agent);
        let err = SmcpHandler::validate_join_room(&new_agent, &office_id, &state).unwrap_err();
        assert!(matches!(err, HandlerError::Session(SessionError::AgentAlreadyExists)));
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
}
