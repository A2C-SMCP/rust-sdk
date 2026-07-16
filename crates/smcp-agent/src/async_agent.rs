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
        build_get_blob_request, build_get_desktop_request, build_get_resources_request,
        build_get_skill_request, build_get_skills_request, build_get_tools_request,
        build_tool_call_cancel, build_tool_call_request,
    },
    response::{ensure_req_id, parse_get_blob_response, parse_get_resources_response},
    skill_consume::{parse_get_skill_response, parse_get_skills_response},
    transport::{NotificationMessage, SocketIoTransport},
};
use smcp::{
    events::*, A2CSkillRef, AgentCallData, EnterOfficeReq, GetBlobRet, GetResourcesRet,
    GetSkillRet, LeaveOfficeReq, ListRoomReq, ReqId, Role, SMCPTool, SessionInfo, SMCP_NAMESPACE,
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
                        // #106 三段式（预清 → 回拉 → 重加，对标 python#127）：先派发预清回调
                        // on_computer_update_tool_list，让加法式下游消费方清空该 computer 的旧工具视图，再自动
                        // 重拉 get_tools → on_tools_received 重加——使运行期**移除 / 同名换 schema**正确生效
                        // （否则纯回拉+加法式 merge 会残留旧定义）。预清失败独立捕获、不阻断后续回拉。
                        //
                        // 预清与回拉**成对**，故一并 gate 在 auto_fetch_tools 内：若消费方显式关闭 auto_fetch，
                        // 则既不预清也不回拉（由消费方自管），避免「清空却因不回拉而永不重填 → 工具凭空消失」。
                        if auto_fetch_tools {
                            if let Some(ref handler) = event_handler {
                                let _ = handler
                                    .on_computer_update_tool_list(data.clone(), &agent_clone)
                                    .await;
                            }
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
            .call(CLIENT_GET_TOOLS, data, self.config.get_timeout)
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
            .call(CLIENT_GET_DESKTOP, data, self.config.get_timeout)
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
    /// `mcp_server`：目标 MCP Server 的 **bundle_id**（**非** display 名；协议 0.3.0 §身份正交性 #18）。
    /// Computer 按 bundle_id 直查、不经 name 解析 ⇒ 传 display 名必得 `4014`。bundle_id 取自
    /// `client:get_config` 响应的 `servers` map 键（typed 消费方法由 #136 补齐）。
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
            .call(CLIENT_GET_RESOURCES, data, self.config.get_timeout)
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
            .call(CLIENT_GET_SKILLS, data, self.config.get_timeout)
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
    /// 文本 MIME 的 `blob_handle` 自动经 `drain_blob` 拉回并 UTF-8 解码回填 `body`，对调用方透明；
    /// 文本性判定经单一权威 [`mime_is_textual`]（协议 §6.4(2)）。对标 Python
    /// `a2c_smcp/agent/client.py::get_skill`。
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
            .call(CLIENT_GET_SKILL, data, self.config.get_timeout)
            .await?;

        let mut ret = parse_get_skill_response(&response, req_id.as_str())?;

        // AGT-03 #38：文本 MIME 的 `blob_handle` 自动 drain 回填 `body`（Python parity）。
        // Computer 对「文本但超内联预算」走句柄；Agent 拉回并 UTF-8 解码即得 body，对调用方透明。
        // 二进制句柄（image/png 等）**保持原样**，由调用方按二进制经 get_blob/drain 自取。
        if ret.body.is_none() {
            if let Some(handle) = ret.blob_handle.clone() {
                if mime_is_textual(ret.mime_type.as_deref()) {
                    match self.drain_blob_bytes(computer, &handle).await {
                        Ok((bytes, _mime)) => match String::from_utf8(bytes) {
                            Ok(text) => {
                                ret.body = Some(text);
                                ret.blob_handle = None;
                            }
                            // 非 UTF-8：保留 blob_handle 让调用方按二进制处理（保守回退，对齐 Python）。
                            Err(_) => debug!(
                                "get_skill {:?} textual mime but body not UTF-8; keeping blob_handle",
                                name
                            ),
                        },
                        Err(e) => warn!(
                            "get_skill blob backfill failed for handle={}: {}; keeping blob_handle",
                            handle, e
                        ),
                    }
                }
            }
        }

        info!("Received skill {:?} from computer: {}", name, computer);
        Ok(ret)
    }

    /// 通用二进制单块拉取（v0.2.1，AGT-03 #38）/ Pull one binary chunk via `client:get_blob`。
    ///
    /// 直接对应协议 `client:get_blob`：按 `chunk_offset`（资源字节绝对偏移，缺省 0）+ `max_chunk_bytes`
    /// （客户建议单块上限，Computer clamp 至 `BlobThresholds.chunk_max_bytes`）取一块，返回 [`GetBlobRet`]
    /// （含 `total_size`/`sha256`/`eof`/base64 `blob`）。flat ErrorPayload（`4018` invalid_handle/
    /// forbidden/gone/range）经 ack 透传为协议错误。多块重组/校验/重试请用 [`Self::drain_blob_bytes`]。
    /// 对标 Python `a2c_smcp/agent/client.py::get_blob`。
    pub async fn get_blob(
        &self,
        computer: &str,
        blob_handle: &str,
        chunk_offset: Option<u64>,
        max_chunk_bytes: Option<u64>,
    ) -> Result<GetBlobRet> {
        let agent_config = self.auth_provider.get_agent_config();
        let req = build_get_blob_request(
            &agent_config.agent,
            computer,
            blob_handle,
            chunk_offset,
            max_chunk_bytes,
        );
        let req_id = req.base.req_id.clone();

        debug!(
            "Getting blob chunk from computer {}, handle={}, offset={:?}",
            computer, blob_handle, chunk_offset
        );

        let transport = self.transport.read().await;
        let transport = transport
            .as_ref()
            .ok_or_else(|| SmcpAgentError::connection("Not connected".to_string()))?;
        let data = serde_json::to_value(&req)?;
        let response = transport
            .call(CLIENT_GET_BLOB, data, self.config.get_timeout)
            .await?;

        let ret = parse_get_blob_response(&response, req_id.as_str())?;
        Ok(ret)
    }

    /// 经 `client:get_blob` 拉取并重组某句柄的全量字节（AGT-03 #38）/ drain & reassemble all bytes。
    ///
    /// 复用 UTIL-02 [`smcp::utils::blob::drain_blob`]（串行多块拉取 + 跨块源漂移重读 + 全量 `sha256`
    /// 自证 + 4018 处置），注入「以本 Agent transport 发 `client:get_blob`」的单块 `call` 闭包。
    /// 传输层错误（超时/断连/序列化）经 out-of-band 暂存**保真**上抛（[`SmcpAgentError::Timeout`] 等，
    /// 不被压成协议码）；拉取期协议错误（4018/漂移/解码）经 [`SmcpAgentError::Blob`] 传播。返回
    /// `(bytes, mime_type)`。供 [`Self::get_skill`] 文本回填与 tool_call 二进制旁路（AGT-04 #41）共用。
    /// 对标 Python `client.py` 的 `_make_blob_call` + `drain_blob`。
    pub(crate) async fn drain_blob_bytes(
        &self,
        computer: &str,
        blob_handle: &str,
    ) -> Result<(Vec<u8>, String)> {
        use smcp::utils::blob::{drain_blob, BlobChunkRequest, DrainBlobOptions};

        let agent = self.auth_provider.get_agent_config().agent.clone();
        let get_timeout = self.config.get_timeout;
        // drain 的 `call` 仅能回 ErrorPayload，无法表达传输层错误；用 out-of-band 暂存保真
        // SmcpAgentError（超时/断连/序列化），drain 失败后优先返回它，回退才映射 BlobTransferError。
        let stash: Arc<std::sync::Mutex<Option<SmcpAgentError>>> =
            Arc::new(std::sync::Mutex::new(None));

        let call = |chunk: BlobChunkRequest| {
            let agent = agent.clone();
            let stash = stash.clone();
            async move {
                let req = build_get_blob_request(
                    &agent,
                    &chunk.computer,
                    &chunk.blob_handle,
                    Some(chunk.chunk_offset),
                    Some(chunk.max_chunk_bytes),
                );
                let data = match serde_json::to_value(&req) {
                    Ok(v) => v,
                    Err(e) => {
                        *stash.lock().unwrap() = Some(SmcpAgentError::from(e));
                        return Err(smcp::ErrorPayload::new(
                            -1,
                            "serialize get_blob request failed",
                        ));
                    }
                };
                let guard = self.transport.read().await;
                let transport = match guard.as_ref() {
                    Some(t) => t,
                    None => {
                        *stash.lock().unwrap() =
                            Some(SmcpAgentError::connection("Not connected".to_string()));
                        return Err(smcp::ErrorPayload::new(-1, "not connected"));
                    }
                };
                match transport.call(CLIENT_GET_BLOB, data, get_timeout).await {
                    // flat ErrorPayload（4018 等）→ 交 drain 分类（4018→NotAccessible，余→Protocol）。
                    Ok(resp) if smcp::is_protocol_error_payload(&resp) => {
                        Err(error_payload_from_value(&resp))
                    }
                    Ok(resp) => match serde_json::from_value::<GetBlobRet>(resp) {
                        Ok(ret) => Ok(ret),
                        Err(e) => {
                            *stash.lock().unwrap() = Some(SmcpAgentError::from(e));
                            Err(smcp::ErrorPayload::new(-1, "malformed GetBlobRet"))
                        }
                    },
                    Err(e) => {
                        *stash.lock().unwrap() = Some(e);
                        Err(smcp::ErrorPayload::new(
                            -1,
                            "transport error during get_blob",
                        ))
                    }
                }
            }
        };

        match drain_blob(call, computer, blob_handle, DrainBlobOptions::default()).await {
            Ok(pair) => Ok(pair),
            Err(blob_err) => {
                // 传输层错误优先保真返回；否则映射拉取期协议错误（4018/漂移/解码）。
                if let Some(te) = stash.lock().unwrap().take() {
                    return Err(te);
                }
                Err(SmcpAgentError::from(blob_err))
            }
        }
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
                // AGT-04 #41（⚠️ BREAKING）：消费 CallToolResult 二进制旁路——遍历 content item 检测
                // `_meta.a2c_blob_handle` 并 drain 回填，否则超内联预算的大二进制会静默变空。无句柄路径不变。
                let response = self
                    .resolve_tool_call_binary_sideband(computer, response)
                    .await;
                info!("Tool call successful: {} on {}", tool_name, computer);
                Ok(response)
            }
            Err(SmcpAgentError::Timeout) => {
                warn!(
                    "Tool call timeout, cancelling: {} on {}",
                    tool_name, computer
                );
                // 发送取消请求（复用统一取消载体 builder：req_id==原 tool_call req_id）
                let cancel_data =
                    build_tool_call_cancel(&agent_config.agent, req_id_for_cancel.as_str());
                let cancel_value = serde_json::to_value(cancel_data)?;
                if let Err(e) = transport.emit(SERVER_TOOL_CALL_CANCEL, cancel_value).await {
                    error!("Failed to send cancel request: {}", e);
                }

                // 返回超时错误（结果级 meta.a2c_timeout=true → Agent 三态分类归 TimedOut，#92 P1）
                Ok(local_timeout_call_result(req_id_for_cancel.as_str()))
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

    /// 解析并回填 `client:tool_call` 结果的二进制旁路（AGT-04 #41，⚠️ BREAKING）/ resolve tool_call binary sideband。
    ///
    /// 三段式（见 [`crate::blob_sideband`] 模块文档）：
    /// 1. **extract**——遍历 `CallToolResult.content` 收集带 `_meta.a2c_blob_handle` 的 item 索引+句柄；
    /// 2. **drain**——逐句柄经 [`Self::drain_blob_bytes`] 拉回全量字节（多块 + sha256 自证 + 4018 处置）；
    /// 3. **inject**——把字节回填到对应 content item 内联载体并清理 `_meta` 旁路键（消费后与小尺寸内联无异）。
    ///
    /// 无句柄的 content item 路径**完全不变**（无句柄时直接早返回原值）。容错（对齐 Python
    /// `_resolve_tool_call_binary_sideband`）：任一 item 的 drain 失败仅 `warn` + **保留**其
    /// `_meta.a2c_blob_handle` 不回填，继续处理其余 item（不整体失败、不早退）——调用方仍可据残留句柄
    /// 自行 [`Self::get_blob`] 兜底。
    async fn resolve_tool_call_binary_sideband(
        &self,
        computer: &str,
        mut response: serde_json::Value,
    ) -> serde_json::Value {
        let handles = crate::blob_sideband::extract_sideband_handles(&response);
        if handles.is_empty() {
            return response; // 无旁路句柄：路径不变（含全部非二进制 tool_call）。
        }
        for (idx, handle) in handles {
            match self.drain_blob_bytes(computer, &handle).await {
                Ok((bytes, _mime)) => {
                    if let Some(item) = response
                        .get_mut("content")
                        .and_then(serde_json::Value::as_array_mut)
                        .and_then(|arr| arr.get_mut(idx))
                    {
                        crate::blob_sideband::inject_payload_into_content_item(item, &bytes);
                    }
                }
                Err(e) => warn!(
                    "tool_call binary sideband drain failed for handle={}: {}; keeping _meta.a2c_blob_handle intact",
                    handle, e
                ),
            }
        }
        response
    }

    /// 取消一次在途工具调用（AGT-05 #44）/ Cancel an in-flight tool call.
    ///
    /// 发送 `server:tool_call_cancel`（**fire-and-forget，无 ack**）：`req_id` **MUST**==被取消的原
    /// `client:tool_call` 的 req_id（唯一定位在途调用）。Server 收后仅向房间广播 `notify:tool_call_cancel`、
    /// **不**回执——故本方法用 `emit`（**非** `call`）不等待 ack，交付传输层即返回 `Ok(())`；ack 缺席是
    /// 协议合规预期，**MUST NOT** 当作失败。
    ///
    /// 取消是否真正中断由 Computer 侧协作式处理（INT-02 #70）；Agent 随后从原 `client:tool_call` 的 ack
    /// 拿到取消态 `CallToolResult`（结果级 `a2c_cancelled=true`），用 [`classify_tool_call_outcome`]
    /// 区分取消 / 超时 / 失败。
    ///
    /// 注：取消载体 `AgentCallData` 仅 `{agent, req_id}`，**不含** reason 字段——取消原因
    /// （`a2c_cancel_reason`）由 Computer 写在结果级 meta，非 Agent 发送（故本方法无 `reason` 参数）。
    /// 调用方需自行持有原 tool_call 的 `req_id`（由另一上下文触发取消，与阻塞中的 tool_call 并行）。
    pub async fn tool_call_cancel(&self, req_id: &str) -> Result<()> {
        let agent_config = self.auth_provider.get_agent_config();
        let cancel = build_tool_call_cancel(&agent_config.agent, req_id);
        let data = serde_json::to_value(cancel)?;

        let transport = self.transport.read().await;
        let transport = transport
            .as_ref()
            .ok_or_else(|| SmcpAgentError::connection("Not connected".to_string()))?;

        // fire-and-forget：emit 不等待 ack（仅表示「已交给传输层发出」），契合 server:tool_call_cancel 无 ack 语义。
        transport.emit(SERVER_TOOL_CALL_CANCEL, data).await?;
        debug!("Sent server:tool_call_cancel for req_id={}", req_id);
        Ok(())
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

/// MIME 是否「文本类」（决定 `get_skill` 句柄是否自动 drain 回填 `body`）/ is this MIME textual?
///
/// 经单一权威 [`smcp::utils::mime::is_text_mime`] 实施协议 §6.4(2) 三分支（`text/*` ∨ `+json/+xml/+yaml`
/// 后缀 ∨ essence 白名单），与 Computer 铸造期 `is_text` 路由及当前 Python `a2c_smcp/agent/client.py`
/// 的 `is_text_mime` **同源同判**（跨 SDK 一致：同一 Computer 响应在两 SDK 给调用方相同结果）。
/// 旧实现仅 `starts_with("text/")`，会漏判 `application/json|yaml|toml` 等超内联预算的文本（python-sdk
/// #105 盲区）；现已收敛。非文本（真二进制）一律保持 `blob_handle`，由调用方按需经 `get_blob` 自取。
fn mime_is_textual(mime: Option<&str>) -> bool {
    mime.map(smcp::utils::mime::is_text_mime).unwrap_or(false)
}

/// 从已判定为 flat 协议错误的 `Value` 宽松构造 [`smcp::ErrorPayload`]，交 `drain_blob` 分类。
/// 保留 `code`/`message`/`details`（`classify_fatal` 仅据 `code` + `details.reason` 分流），
/// 缺字段宽松回退（不 panic），等价于直接 `from_value` 但容忍缺 `message`。
fn error_payload_from_value(v: &serde_json::Value) -> smcp::ErrorPayload {
    let code = v
        .get("code")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(-1);
    let message = v
        .get("message")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    let mut payload = smcp::ErrorPayload::new(code, message);
    payload.details = v.get("details").cloned();
    payload
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

/// 构造 Agent **本地**超时兜底的 `CallToolResult`-shape 响应（P1 #92）。
///
/// 写结果级 `meta.a2c_timeout=true`（协议规范 wire key `meta`，键用单一权威常量
/// [`smcp::tool_meta::A2C_TIMEOUT_KEY`]），使 [`crate::response::classify_tool_call_outcome`] 把该响应归类为
/// `TimedOut`（而非 `Failed`），与 Computer 侧超时态语义对齐。纯函数，便于单测（本仓 transport
/// 无 mock 接缝，约定抽纯 helper 单测）。
fn local_timeout_call_result(req_id: &str) -> serde_json::Value {
    let mut v = serde_json::json!({
        "content": [{
            "type": "text",
            "text": format!("工具调用超时 / Tool call timeout, req_id={}", req_id)
        }],
        "isError": true,
    });
    let mut meta = serde_json::Map::new();
    meta.insert(
        smcp::tool_meta::A2C_TIMEOUT_KEY.to_string(),
        serde_json::Value::Bool(true),
    );
    v["meta"] = serde_json::Value::Object(meta);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #81：`get_skill` 文本句柄自动 drain 的 MIME 判定遵循协议 §6.4(2) 三分支，经单一权威
    /// `smcp::utils::mime::is_text_mime`（与当前 Python `is_text_mime` parity）/ §6.4(2) textual gate。
    #[test]
    fn test_mime_is_textual_follows_spec_64_2() {
        // 分支 1：text/*（含 charset 参数）。
        assert!(mime_is_textual(Some("text/plain")));
        assert!(mime_is_textual(Some("text/markdown")));
        assert!(mime_is_textual(Some("text/plain; charset=utf-8")));
        // 分支 2：+json / +xml / +yaml 后缀（旧 starts_with("text/") 漏掉）。
        assert!(mime_is_textual(Some("image/svg+xml")));
        assert!(mime_is_textual(Some("application/vnd.api+json")));
        // 分支 3：application/* 文本 essence 白名单（旧 starts_with("text/") 漏掉 → python-sdk #105 盲区）。
        assert!(mime_is_textual(Some("application/json")));
        assert!(mime_is_textual(Some("application/json; charset=utf-8")));
        assert!(mime_is_textual(Some("application/xml")));
        assert!(mime_is_textual(Some("application/yaml")));
        assert!(mime_is_textual(Some("application/toml")));
        // 真二进制 / 缺省一律非文本（保持 blob_handle）。
        assert!(!mime_is_textual(Some("application/octet-stream")));
        assert!(!mime_is_textual(Some("image/png")));
        assert!(!mime_is_textual(Some("")));
        assert!(!mime_is_textual(None));
    }

    /// #92 P1：本地超时兜底结果写结果级 `meta.a2c_timeout=true`，使三态分类归 `TimedOut`（非 `Failed`）。
    #[test]
    fn test_local_timeout_result_classifies_as_timed_out() {
        use crate::response::{classify_tool_call_outcome, ToolCallOutcome};
        let v = local_timeout_call_result("rid-42");
        // 结果级标记落协议规范的 wire key `meta`。
        assert_eq!(
            v["meta"][smcp::tool_meta::A2C_TIMEOUT_KEY],
            serde_json::json!(true)
        );
        assert_eq!(v["isError"], serde_json::json!(true));
        assert!(
            v["content"][0]["text"].as_str().unwrap().contains("rid-42"),
            "超时文案应含 req_id"
        );
        // 关键：被三态分类器归为 TimedOut（之前无 meta 时归 Failed）。
        assert_eq!(classify_tool_call_outcome(&v), ToolCallOutcome::TimedOut);
    }
}
