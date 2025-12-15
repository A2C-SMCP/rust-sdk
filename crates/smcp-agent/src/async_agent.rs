/*!
* 文件名: async_agent
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: None
* 描述: SMCP异步Agent实现 / SMCP asynchronous Agent implementation
*/

use crate::{
    auth::AuthProvider,
    config::SmcpAgentConfig,
    error::{Result, SmcpAgentError},
    events::AsyncAgentEventHandler,
    transport::{NotificationMessage, SocketIoTransport},
};
use smcp::{
    events::*, AgentCallData, EnterOfficeReq, GetDesktopReq, GetToolsReq, LeaveOfficeReq,
    ListRoomReq, ReqId, Role, SMCPTool, SessionInfo, ToolCallReq, SMCP_NAMESPACE,
};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

/// 异步SMCP Agent
pub struct AsyncSmcpAgent {
    transport: Arc<RwLock<Option<SocketIoTransport>>>,
    auth_provider: Arc<dyn AuthProvider>,
    event_handler: Option<Arc<dyn AsyncAgentEventHandler>>,
    config: SmcpAgentConfig,
    tools_cache: Arc<RwLock<HashMap<String, Vec<SMCPTool>>>>,
    notification_task: Option<tokio::task::JoinHandle<()>>,
}

impl AsyncSmcpAgent {
    /// 创建新的Agent实例
    pub fn new(auth_provider: impl AuthProvider + 'static, config: SmcpAgentConfig) -> Self {
        Self {
            transport: Arc::new(RwLock::new(None)),
            auth_provider: Arc::new(auth_provider),
            event_handler: None,
            config,
            tools_cache: Arc::new(RwLock::new(HashMap::new())),
            notification_task: None,
        }
    }

    /// 设置事件处理器
    pub fn with_event_handler(mut self, handler: impl AsyncAgentEventHandler + 'static) -> Self {
        self.event_handler = Some(Arc::new(handler));
        self
    }

    /// 连接到服务器
    pub async fn connect(&mut self, url: &str) -> Result<()> {
        let auth = self.auth_provider.get_connection_auth();
        let headers = self.auth_provider.get_connection_headers();

        // 创建transport并获取通知接收器
        let (transport, mut notification_rx) =
            SocketIoTransport::connect_with_handlers(url, SMCP_NAMESPACE, auth, headers).await?;

        // 启动通知处理任务
        let event_handler = self.event_handler.clone();
        let agent_clone = self.clone();

        let notification_task = tokio::spawn(async move {
            while let Some(notification) = notification_rx.recv().await {
                match notification {
                    NotificationMessage::EnterOffice(data) => {
                        // Python 的自动行为：收到 enter_office 后自动触发 get_tools
                        if let Some(ref computer) = data.computer {
                            if let Ok(tools) = agent_clone.get_tools(computer).await {
                                if let Some(ref handler) = event_handler {
                                    let _ = handler
                                        .on_tools_received(computer, tools, &agent_clone)
                                        .await;
                                }
                            }
                        }

                        if let Some(ref handler) = event_handler {
                            let _ = handler.on_computer_enter_office(data, &agent_clone).await;
                        }
                    }
                    NotificationMessage::LeaveOffice(data) => {
                        if let Some(ref handler) = event_handler {
                            let _ = handler.on_computer_leave_office(data, &agent_clone).await;
                        }
                    }
                    NotificationMessage::UpdateConfig(data) => {
                        // Python 的自动行为：收到 update_config 后自动触发 get_tools
                        if let Ok(tools) = agent_clone.get_tools(&data.computer).await {
                            if let Some(ref handler) = event_handler {
                                let _ = handler
                                    .on_tools_received(&data.computer, tools, &agent_clone)
                                    .await;
                            }
                        }

                        if let Some(ref handler) = event_handler {
                            let _ = handler.on_computer_update_config(data, &agent_clone).await;
                        }
                    }
                    NotificationMessage::UpdateToolList(data) => {
                        // Python 的自动行为：收到 update_tool_list 后自动触发 get_tools
                        if let Ok(tools) = agent_clone.get_tools(&data.computer).await {
                            if let Some(ref handler) = event_handler {
                                let _ = handler
                                    .on_tools_received(&data.computer, tools, &agent_clone)
                                    .await;
                            }
                        }
                    }
                    NotificationMessage::UpdateDesktop(computer) => {
                        // Python 的自动行为：收到 update_desktop 后自动触发 get_desktop
                        if let Ok(desktops) = agent_clone.get_desktop(&computer, None, None).await {
                            if let Some(ref handler) = event_handler {
                                let _ = handler
                                    .on_desktop_updated(&computer, desktops, &agent_clone)
                                    .await;
                            }
                        }
                    }
                }
            }
        });

        self.notification_task = Some(notification_task);
        *self.transport.write().await = Some(transport);

        info!("Connected to SMCP server at {}", url);
        Ok(())
    }

    /// 加入办公室
    pub async fn join_office(&self, agent_name: &str) -> Result<()> {
        let office_id = &self.auth_provider.get_agent_config().office_id;
        let req = EnterOfficeReq {
            role: Role::Agent,
            name: agent_name.to_string(),
            office_id: office_id.clone(),
        };

        let transport = self.transport.read().await;
        let transport = transport
            .as_ref()
            .ok_or_else(|| SmcpAgentError::connection("Not connected".to_string()))?;
        let data = serde_json::to_value(req)?;
        transport.emit(SERVER_JOIN_OFFICE, data).await?;

        info!("Joined office: {}", office_id);
        Ok(())
    }

    /// 离开办公室
    pub async fn leave_office(&self) -> Result<()> {
        let office_id = &self.auth_provider.get_agent_config().office_id;
        let req = LeaveOfficeReq {
            office_id: office_id.clone(),
        };

        let transport = self.transport.read().await;
        let transport = transport
            .as_ref()
            .ok_or_else(|| SmcpAgentError::connection("Not connected".to_string()))?;
        let data = serde_json::to_value(req)?;
        transport.emit(SERVER_LEAVE_OFFICE, data).await?;

        info!("Left office: {}", office_id);
        Ok(())
    }

    /// 获取指定Computer的工具列表
    pub async fn get_tools(&self, computer: &str) -> Result<Vec<SMCPTool>> {
        let agent_config = self.auth_provider.get_agent_config();
        let req_id = ReqId::new();
        let req = GetToolsReq {
            base: AgentCallData {
                agent: agent_config.agent.clone(),
                req_id: req_id.clone(),
            },
            computer: computer.to_string(),
        };

        debug!("Getting tools from computer: {}", computer);

        let transport = self.transport.read().await;
        let transport = transport
            .as_ref()
            .ok_or_else(|| SmcpAgentError::connection("Not connected".to_string()))?;
        let data = serde_json::to_value(req)?;
        let response = transport
            .call(CLIENT_GET_TOOLS, data, self.config.default_timeout)
            .await?;

        // 验证req_id
        let response_req_id: String = response
            .get("req_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| SmcpAgentError::internal("Missing req_id in response"))?
            .to_string();

        if response_req_id != req_id.as_str() {
            return Err(SmcpAgentError::ReqIdMismatch {
                expected: req_id.as_str().to_string(),
                actual: response_req_id,
            });
        }

        let tools: Vec<SMCPTool> =
            serde_json::from_value(response.get("tools").cloned().unwrap_or_default())?;

        // 更新缓存
        self.tools_cache
            .write()
            .await
            .insert(computer.to_string(), tools.clone());

        info!("Received {} tools from computer: {}", tools.len(), computer);
        Ok(tools)
    }

    /// 获取指定Computer的桌面信息
    pub async fn get_desktop(
        &self,
        computer: &str,
        size: Option<i32>,
        window: Option<String>,
    ) -> Result<Vec<String>> {
        let agent_config = self.auth_provider.get_agent_config();
        let req_id = ReqId::new();
        let mut req = GetDesktopReq {
            base: AgentCallData {
                agent: agent_config.agent.clone(),
                req_id: req_id.clone(),
            },
            computer: computer.to_string(),
            desktop_size: None,
            window: None,
        };

        if let Some(s) = size {
            req.desktop_size = Some(s);
        }
        if let Some(w) = window {
            req.window = Some(w);
        }

        debug!("Getting desktop from computer: {}", computer);

        let transport = self.transport.read().await;
        let transport = transport
            .as_ref()
            .ok_or_else(|| SmcpAgentError::connection("Not connected".to_string()))?;
        let data = serde_json::to_value(req)?;
        let response = transport
            .call(CLIENT_GET_DESKTOP, data, self.config.default_timeout)
            .await?;

        // 验证req_id
        let response_req_id: String = response
            .get("req_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| SmcpAgentError::internal("Missing req_id in response"))?
            .to_string();

        if response_req_id != req_id.as_str() {
            return Err(SmcpAgentError::ReqIdMismatch {
                expected: req_id.as_str().to_string(),
                actual: response_req_id,
            });
        }

        let desktops: Vec<String> =
            serde_json::from_value(response.get("desktops").cloned().unwrap_or_default())?;

        info!(
            "Received {} desktops from computer: {}",
            desktops.len(),
            computer
        );
        Ok(desktops)
    }

    /// 调用工具
    pub async fn tool_call(
        &self,
        computer: &str,
        tool_name: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let agent_config = self.auth_provider.get_agent_config();
        let req_id = ReqId::new();
        let req_id_for_cancel = req_id.clone();
        let req = ToolCallReq {
            base: AgentCallData {
                agent: agent_config.agent.clone(),
                req_id,
            },
            computer: computer.to_string(),
            tool_name: tool_name.to_string(),
            params,
            timeout: self.config.tool_call_timeout as i32,
        };

        debug!("Calling tool {} on computer: {}", tool_name, computer);

        let transport = self.transport.read().await;
        let transport = transport
            .as_ref()
            .ok_or_else(|| SmcpAgentError::connection("Not connected".to_string()))?;
        let data = serde_json::to_value(req.clone())?;

        match transport
            .call(CLIENT_TOOL_CALL, data, self.config.tool_call_timeout)
            .await
        {
            Ok(response) => {
                info!("Tool call successful: {} on {}", tool_name, computer);
                Ok(response)
            }
            Err(SmcpAgentError::Timeout) => {
                warn!(
                    "Tool call timeout, cancelling: {} on {}",
                    tool_name, computer
                );
                // 发送取消请求
                let cancel_data = AgentCallData {
                    agent: agent_config.agent.clone(),
                    req_id: req_id_for_cancel.clone(),
                };
                let cancel_value = serde_json::to_value(cancel_data)?;
                if let Err(e) = transport.emit(SERVER_TOOL_CALL_CANCEL, cancel_value).await {
                    error!("Failed to send cancel request: {}", e);
                }

                // 返回超时错误
                Ok(serde_json::json!({
                    "content": [{
                        "type": "text",
                        "text": format!("工具调用超时 / Tool call timeout, req_id={}", req_id_for_cancel.as_str())
                    }],
                    "isError": true
                }))
            }
            Err(e) => {
                error!(
                    "Tool call failed: {} on {}, error: {}",
                    tool_name, computer, e
                );
                Err(e)
            }
        }
    }

    /// 列出房间内的所有会话
    pub async fn list_room(&self, office_id: &str) -> Result<Vec<SessionInfo>> {
        let agent_config = self.auth_provider.get_agent_config();
        let req_id = ReqId::new();
        let req = ListRoomReq {
            base: AgentCallData {
                agent: agent_config.agent.clone(),
                req_id: req_id.clone(),
            },
            office_id: office_id.to_string(),
        };

        debug!("Listing sessions in office: {}", office_id);

        let transport = self.transport.read().await;
        let transport = transport
            .as_ref()
            .ok_or_else(|| SmcpAgentError::connection("Not connected".to_string()))?;
        let data = serde_json::to_value(req)?;
        let response = transport
            .call(SERVER_LIST_ROOM, data, self.config.default_timeout)
            .await?;

        // 验证req_id
        let response_req_id: String = response
            .get("req_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| SmcpAgentError::internal("Missing req_id in response"))?
            .to_string();

        if response_req_id != req_id.as_str() {
            return Err(SmcpAgentError::ReqIdMismatch {
                expected: req_id.as_str().to_string(),
                actual: response_req_id,
            });
        }

        let sessions: Vec<SessionInfo> =
            serde_json::from_value(response.get("sessions").cloned().unwrap_or_default())?;

        info!(
            "Listed {} sessions in office: {}",
            sessions.len(),
            office_id
        );
        Ok(sessions)
    }
}

// 实现Clone以便在事件处理器中使用
impl Clone for AsyncSmcpAgent {
    fn clone(&self) -> Self {
        Self {
            transport: self.transport.clone(),
            auth_provider: self.auth_provider.clone(),
            event_handler: self.event_handler.clone(),
            config: self.config.clone(),
            tools_cache: self.tools_cache.clone(),
            notification_task: None, // Note: 任务句柄不克隆，因为它是特定于实例的
        }
    }
}

