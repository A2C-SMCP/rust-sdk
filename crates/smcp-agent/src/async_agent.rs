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
    protocol_error::raise_for_error_payload,
    request_builders::{
        build_get_desktop_request, build_get_resources_request, build_get_skill_request,
        build_get_skills_request, build_get_tools_request, build_tool_call_request,
    },
    response::{ensure_req_id, parse_get_resources_response},
    skill_consume::{parse_get_skill_response, parse_get_skills_response},
    transport::{NotificationMessage, SocketIoTransport},
};
use smcp::{
    events::*, A2CSkillRef, AgentCallData, EnterOfficeReq, GetResourcesRet, GetSkillRet,
    LeaveOfficeReq, ListRoomReq, ReqId, Role, SMCPTool, SessionInfo, SMCP_NAMESPACE,
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
        let auto_fetch_tools = self.config.auto_fetch_tools;

        let notification_task = tokio::spawn(async move {
            info!("Notification processing task started");
            while let Some(notification) = notification_rx.recv().await {
                info!("Agent received notification in task: {:?}", notification);
                match notification {
                    NotificationMessage::EnterOffice(data) => {
                        info!("Processing EnterOffice event: {:?}", data);

                        // Python 的自动行为：收到 enter_office 后自动触发 get_tools
                        // Auto behavior: fetch tools when computer enters office
                        if auto_fetch_tools {
                            if let Some(ref computer) = data.computer {
                                debug!("Auto fetching tools for computer: {}", computer);
                                match agent_clone.get_tools(computer).await {
                                    Ok(tools) => {
                                        info!(
                                            "Auto fetched {} tools for computer: {}",
                                            tools.len(),
                                            computer
                                        );
                                        if let Some(ref handler) = event_handler {
                                            let _ = handler
                                                .on_tools_received(computer, tools, &agent_clone)
                                                .await;
                                        }
                                    }
                                    Err(e) => {
                                        warn!(
                                            "Failed to auto fetch tools for computer {}: {}",
                                            computer, e
                                        );
                                    }
                                }
                            }
                        }

                        info!(
                            "Checking event_handler: is_some = {}",
                            event_handler.is_some()
                        );
                        if let Some(ref handler) = event_handler {
                            info!("Calling on_computer_enter_office handler");
                            let _ = handler.on_computer_enter_office(data, &agent_clone).await;
                        } else {
                            info!("No event handler configured for on_computer_enter_office");
                        }
                    }
                    NotificationMessage::LeaveOffice(data) => {
                        if let Some(ref handler) = event_handler {
                            let _ = handler.on_computer_leave_office(data, &agent_clone).await;
                        }
                    }
                    NotificationMessage::UpdateConfig(data) => {
                        // Python 的自动行为：收到 update_config 后自动触发 get_tools
                        // Auto behavior: fetch tools when config is updated
                        if auto_fetch_tools {
                            match agent_clone.get_tools(&data.computer).await {
                                Ok(tools) => {
                                    debug!(
                                        "Auto fetched {} tools on config update for: {}",
                                        tools.len(),
                                        data.computer
                                    );
                                    if let Some(ref handler) = event_handler {
                                        let _ = handler
                                            .on_tools_received(&data.computer, tools, &agent_clone)
                                            .await;
                                    }
                                }
                                Err(e) => {
                                    warn!(
                                        "Failed to auto fetch tools on config update for {}: {}",
                                        data.computer, e
                                    );
                                }
                            }
                        }

                        if let Some(ref handler) = event_handler {
                            let _ = handler.on_computer_update_config(data, &agent_clone).await;
                        }
                    }
                    NotificationMessage::UpdateToolList(data) => {
                        // Python 的自动行为：收到 update_tool_list 后自动触发 get_tools
                        // Auto behavior: fetch tools when tool list is updated
                        if auto_fetch_tools {
                            match agent_clone.get_tools(&data.computer).await {
                                Ok(tools) => {
                                    debug!(
                                        "Auto fetched {} tools on tool list update for: {}",
                                        tools.len(),
                                        data.computer
                                    );
                                    if let Some(ref handler) = event_handler {
                                        let _ = handler
                                            .on_tools_received(&data.computer, tools, &agent_clone)
                                            .await;
                                    }
                                }
                                Err(e) => {
                                    warn!(
                                        "Failed to auto fetch tools on tool list update for {}: {}",
                                        data.computer, e
                                    );
                                }
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
                    NotificationMessage::UpdateSkills(computer) => {
                        // v0.2.1 自动行为（对标 Python `_on_skills_updated`）：收到 notify:update_skills
                        // 后自动重拉 get_skills，再派发 on_skills_received hook。
                        // 错误隔离：重拉失败仅告警；hook 抛错独立捕获、不污染通知循环（对标 Python 双层 try）。
                        match agent_clone.get_skills(&computer).await {
                            Ok(skills) => {
                                info!(
                                    "Skills refreshed from computer {}: count={}",
                                    computer,
                                    skills.len()
                                );
                                if let Some(ref handler) = event_handler {
                                    if let Err(e) = handler
                                        .on_skills_received(&computer, skills, &agent_clone)
                                        .await
                                    {
                                        error!(
                                            "on_skills_received hook raised for computer {}: {}",
                                            computer, e
                                        );
                                    }
                                }
                            }
                            Err(e) => {
                                warn!("Failed to refresh skills for computer {}: {}", computer, e);
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
        let req = build_get_tools_request(&agent_config.agent, computer);
        let req_id = req.base.req_id.clone();

        debug!("Getting tools from computer: {}", computer);

        let transport = self.transport.read().await;
        let transport = transport
            .as_ref()
            .ok_or_else(|| SmcpAgentError::connection("Not connected".to_string()))?;
        let data = serde_json::to_value(&req)?;
        let response = transport
            .call(CLIENT_GET_TOOLS, data, self.config.default_timeout)
            .await?;

        // flat ErrorPayload → 协议错误（#47↔#34：对端未命中/能力错误经 ack 回 flat error）
        raise_for_error_payload(&response)?;

        // 验证 req_id（全 crate 单点收敛，见 response::ensure_req_id）
        ensure_req_id(&response, req_id.as_str())?;

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
        let req = build_get_desktop_request(&agent_config.agent, computer, size, window.as_deref());
        let req_id = req.base.req_id.clone();

        debug!("Getting desktop from computer: {}", computer);

        let transport = self.transport.read().await;
        let transport = transport
            .as_ref()
            .ok_or_else(|| SmcpAgentError::connection("Not connected".to_string()))?;
        let data = serde_json::to_value(&req)?;
        let response = transport
            .call(CLIENT_GET_DESKTOP, data, self.config.default_timeout)
            .await?;

        // flat ErrorPayload → 协议错误（#47↔#34）
        raise_for_error_payload(&response)?;

        // 验证 req_id（全 crate 单点收敛，见 response::ensure_req_id）
        ensure_req_id(&response, req_id.as_str())?;

        let desktops: Vec<String> =
            serde_json::from_value(response.get("desktops").cloned().unwrap_or_default())?;

        info!(
            "Received {} desktops from computer: {}",
            desktops.len(),
            computer
        );
        Ok(desktops)
    }

    /// 获取指定 Computer 上某 MCP Server 的资源列表（v0.2.0）/ Get a MCP Server's resource list。
    ///
    /// 透明转发 MCP `resources/list`：SDK **不**自动翻页——`cursor` 由调用方控制，首次传 `None`，
    /// 响应含 `next_cursor` 时由调用方决定是否带该 cursor 继续请求（协议指南 §5.3 #3）。flat
    /// ErrorPayload（`4014` MCP Server 未注册 / `4015` 未声明 `resources` 能力）经 ack 透传为协议
    /// 错误，不吞错。对标 Python `a2c_smcp/agent/client.py::get_resources`。
    pub async fn get_resources(
        &self,
        computer: &str,
        mcp_server: &str,
        cursor: Option<String>,
    ) -> Result<GetResourcesRet> {
        let agent_config = self.auth_provider.get_agent_config();
        let req = build_get_resources_request(
            &agent_config.agent,
            computer,
            mcp_server,
            cursor.as_deref(),
        );
        let req_id = req.base.req_id.clone();

        debug!(
            "Getting resources from computer {}, mcp_server={}, cursor={:?}",
            computer, mcp_server, cursor
        );

        let transport = self.transport.read().await;
        let transport = transport
            .as_ref()
            .ok_or_else(|| SmcpAgentError::connection("Not connected".to_string()))?;
        let data = serde_json::to_value(&req)?;
        let response = transport
            .call(CLIENT_GET_RESOURCES, data, self.config.default_timeout)
            .await?;

        // 响应编排（flat ErrorPayload 4014/4015 透传 + req_id 校验 + 整页解析）单点收敛于纯函数，
        // 与 get_skills 同构、可独立单测（见 response::parse_get_resources_response）。
        let ret = parse_get_resources_response(&response, req_id.as_str())?;
        info!(
            "Received {} resources from computer: {} (next_cursor={:?})",
            ret.resources.len(),
            computer,
            ret.next_cursor
        );
        Ok(ret)
    }

    /// 获取指定 Computer 的 SKILL 清单（v0.2.1）/ Get a Computer's SKILL inventory。
    ///
    /// 发起 `client:get_skills`，返回轻量 [`A2CSkillRef`] 列表（**不含** SKILL.md body；body 经
    /// [`Self::get_skill`] 按需拉取）。flat ErrorPayload（如 `4014`）经 ack 透传为协议错误。
    /// 对标 Python `a2c_smcp/agent/client.py::get_skills`。
    pub async fn get_skills(&self, computer: &str) -> Result<Vec<A2CSkillRef>> {
        let agent_config = self.auth_provider.get_agent_config();
        let req = build_get_skills_request(&agent_config.agent, computer);
        let req_id = req.base.req_id.clone();

        debug!("Getting skills from computer: {}", computer);

        let transport = self.transport.read().await;
        let transport = transport
            .as_ref()
            .ok_or_else(|| SmcpAgentError::connection("Not connected".to_string()))?;
        let data = serde_json::to_value(&req)?;
        let response = transport
            .call(CLIENT_GET_SKILLS, data, self.config.default_timeout)
            .await?;

        let skills = parse_get_skills_response(&response, req_id.as_str())?;
        info!(
            "Received {} skills from computer: {}",
            skills.len(),
            computer
        );
        Ok(skills)
    }

    /// 获取 SKILL 包内单个资源（v0.2.1）/ Get a single in-package SKILL resource。
    ///
    /// 发起 `client:get_skill`；`rel_path` 缺省由 Computer 解析为包根 `SKILL.md` 入口，否则 MUST
    /// 相对、无 `..`、无绝对路径（沙箱在 Computer 端强制）。返回的 [`GetSkillRet`] 中 `body` 与
    /// `blob_handle` **恰一存在**（经 [`GetSkillRet::resource`] 校验访问）：
    /// - 文本且可内联 → `body` 直接可读（[`smcp::SkillResource::Inline`]）；
    /// - 二进制或过大文本 → `blob_handle` **原样返回**，由调用方经 `client:get_blob` 自取字节。
    ///
    /// 注：文本 MIME 的 `blob_handle` 自动 drain 回填 `body`（Python parity）依赖 `drain_blob`
    /// （AGT-03 / #38），本期不实现；#38 落地后在此小幅接线即可。
    /// 对标 Python `a2c_smcp/agent/client.py::get_skill`（drain 回填段除外）。
    pub async fn get_skill(
        &self,
        computer: &str,
        name: &str,
        rel_path: Option<&str>,
    ) -> Result<GetSkillRet> {
        let agent_config = self.auth_provider.get_agent_config();
        let req = build_get_skill_request(&agent_config.agent, computer, name, rel_path);
        let req_id = req.base.req_id.clone();

        debug!(
            "Getting skill {:?} rel_path={:?} from computer: {}",
            name, rel_path, computer
        );

        let transport = self.transport.read().await;
        let transport = transport
            .as_ref()
            .ok_or_else(|| SmcpAgentError::connection("Not connected".to_string()))?;
        let data = serde_json::to_value(&req)?;
        let response = transport
            .call(CLIENT_GET_SKILL, data, self.config.default_timeout)
            .await?;

        let ret = parse_get_skill_response(&response, req_id.as_str())?;
        info!("Received skill {:?} from computer: {}", name, computer);
        Ok(ret)
    }

    /// 调用工具
    pub async fn tool_call(
        &self,
        computer: &str,
        tool_name: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let agent_config = self.auth_provider.get_agent_config();
        let req = build_tool_call_request(
            &agent_config.agent,
            computer,
            tool_name,
            params,
            self.config.tool_call_timeout as i32,
        );
        let req_id_for_cancel = req.base.req_id.clone();

        debug!("Calling tool {} on computer: {}", tool_name, computer);

        let transport = self.transport.read().await;
        let transport = transport
            .as_ref()
            .ok_or_else(|| SmcpAgentError::connection("Not connected".to_string()))?;
        let data = serde_json::to_value(&req)?;

        match transport
            .call(CLIENT_TOOL_CALL, data, self.config.tool_call_timeout)
            .await
        {
            Ok(response) => {
                // flat ErrorPayload → 协议错误（如 4006/4007 授权、404 未命中）；正常 CallToolResult
                // （含 isError 的工具执行失败）无顶层协议 code，原样透传。
                raise_for_error_payload(&response)?;
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

        // 验证 req_id（全 crate 单点收敛，见 response::ensure_req_id）
        ensure_req_id(&response, req_id.as_str())?;

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
