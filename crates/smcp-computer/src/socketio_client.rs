/*!
* 文件名: socketio_client
* 作者: JQQ
* 创建日期: 2025/12/16
* 最后修改日期: 2025/12/16
* 版权: 2023 JQQ. All rights reserved.
* 依赖: tf_rust_socketio, tokio, serde
* 描述: SMCP Computer的Socket.IO客户端实现 / Socket.IO client implementation for SMCP Computer
*/

use crate::desktop::{organize_desktop, WindowInfo};
use crate::errors::{ComputerError, ComputerResult};
use crate::mcp_clients::manager::MCPServerManager;
use crate::mcp_clients::model::MCPServerInput;
use futures_util::FutureExt;
use serde_json::Value;
use smcp::{
    events::{
        CLIENT_GET_CONFIG, CLIENT_GET_DESKTOP, CLIENT_GET_TOOLS, CLIENT_TOOL_CALL,
        SERVER_JOIN_OFFICE, SERVER_LEAVE_OFFICE, SERVER_UPDATE_CONFIG, SERVER_UPDATE_DESKTOP,
        SERVER_UPDATE_TOOL_LIST,
    },
    GetComputerConfigReq, GetComputerConfigRet, GetDesktopReq, GetDesktopRet, GetToolsReq,
    GetToolsRet, ToolCallReq, SMCP_NAMESPACE,
};
use std::collections::HashMap;
use std::sync::Arc;
use tf_rust_socketio::{
    asynchronous::{Client, ClientBuilder},
    Event, Payload, TransportType,
};
use tokio::sync::RwLock;
use tracing::{debug, error, info};

/// 默认鉴权 HTTP header 键名 /
/// Default auth HTTP header key name.
///
/// 与 A2C-SMCP 协议 auth-agnostic 立场一致：部署方自行决定 header 名；
/// 此处默认值匹配 TuringFocus 生态（`access_token`，下划线）。
/// Aligns with A2C-SMCP's auth-agnostic stance; default matches the
/// TuringFocus deployment (`access_token`, underscored).
pub const DEFAULT_AUTH_HEADER_NAME: &str = "access_token";

/// SMCP Computer Socket.IO客户端 Builder /
/// Builder for the SMCP Computer Socket.IO client.
///
/// 通过 Builder 配置握手期的 namespace、鉴权 header 名、自定义 HTTP headers 等。
/// Configure handshake-time namespace, auth header name, custom headers, etc.
pub struct SmcpComputerClientBuilder {
    url: String,
    manager: Arc<RwLock<Option<MCPServerManager>>>,
    computer_name: String,
    inputs: Arc<RwLock<HashMap<String, MCPServerInput>>>,
    auth_secret: Option<String>,
    auth_header_name: Option<String>,
    namespace: Option<String>,
    headers: Option<HashMap<String, String>>,
}

impl SmcpComputerClientBuilder {
    /// 创建 Builder（必填项） / Create a new builder (required fields).
    pub fn new(
        url: impl Into<String>,
        manager: Arc<RwLock<Option<MCPServerManager>>>,
        computer_name: impl Into<String>,
        inputs: Arc<RwLock<HashMap<String, MCPServerInput>>>,
    ) -> Self {
        Self {
            url: url.into(),
            manager,
            computer_name: computer_name.into(),
            inputs,
            auth_secret: None,
            auth_header_name: None,
            namespace: None,
            headers: None,
        }
    }

    /// 鉴权密钥（写入 `auth_header_name` 指定的 header）。
    /// Auth secret (written into the header named by `auth_header_name`).
    pub fn auth_secret(mut self, secret: impl Into<String>) -> Self {
        self.auth_secret = Some(secret.into());
        self
    }

    /// 自定义鉴权 HTTP header 名；未设置时默认 [`DEFAULT_AUTH_HEADER_NAME`]。
    /// Customize the auth HTTP header name; defaults to
    /// [`DEFAULT_AUTH_HEADER_NAME`] when not set.
    pub fn auth_header_name(mut self, name: impl Into<String>) -> Self {
        self.auth_header_name = Some(name.into());
        self
    }

    /// 自定义 Socket.IO 应用层 namespace；未设置时默认 [`SMCP_NAMESPACE`] (`/smcp`)。
    /// Customize the Socket.IO application-layer namespace; defaults to
    /// [`SMCP_NAMESPACE`] (`/smcp`) when not set.
    pub fn namespace(mut self, ns: impl Into<String>) -> Self {
        self.namespace = Some(ns.into());
        self
    }

    /// 附加任意 HTTP upgrade header（如 TF 生态路由 `X-TF-RobotId`）。
    /// Attach arbitrary HTTP upgrade headers (e.g. TF ecosystem routing headers).
    pub fn headers(mut self, headers: HashMap<String, String>) -> Self {
        self.headers = Some(headers);
        self
    }

    /// 建立 Socket.IO 连接。 / Establish the Socket.IO connection.
    pub async fn connect(self) -> ComputerResult<SmcpComputerClient> {
        let namespace = self.namespace.unwrap_or_else(|| SMCP_NAMESPACE.to_string());
        let auth_header_name = self
            .auth_header_name
            .unwrap_or_else(|| DEFAULT_AUTH_HEADER_NAME.to_string());
        SmcpComputerClient::connect_internal(
            self.url,
            self.manager,
            self.computer_name,
            self.inputs,
            self.auth_secret,
            auth_header_name,
            namespace,
            self.headers,
        )
        .await
    }
}

/// SMCP Computer Socket.IO客户端
/// SMCP Computer Socket.IO client
pub struct SmcpComputerClient {
    /// Socket.IO客户端实例 / Socket.IO client instance
    client: Client,
    /// Computer名称 / Computer name
    computer_name: String,
    /// 当前所在的office ID / Current office ID
    office_id: Arc<RwLock<Option<String>>>,
    /// 输入定义映射 / Input definitions map
    #[allow(dead_code)]
    inputs: Arc<RwLock<HashMap<String, MCPServerInput>>>,
    /// 实际握手使用的 Socket.IO namespace / Socket.IO namespace used during handshake
    namespace: String,
    /// 实际写入 HTTP header 的鉴权 key 名 / Auth HTTP header key name used on the wire
    auth_header_name: String,
}

impl SmcpComputerClient {
    /// 创建新的Socket.IO客户端（向后兼容入口） /
    /// Create a new Socket.IO client (backward-compatible entry point).
    ///
    /// 内部委托给 [`SmcpComputerClientBuilder`]，默认 `auth_header_name = "access_token"`，
    /// `namespace = "/smcp"`。需要自定义这两项的调用方请直接使用 Builder。
    /// Internally delegates to [`SmcpComputerClientBuilder`]; callers needing
    /// to customize these fields should use the Builder directly.
    pub async fn new(
        url: &str,
        manager: Arc<RwLock<Option<MCPServerManager>>>,
        computer_name: String,
        auth_secret: Option<String>,
        inputs: Arc<RwLock<HashMap<String, MCPServerInput>>>,
        headers: Option<HashMap<String, String>>,
    ) -> ComputerResult<Self> {
        let mut b = SmcpComputerClientBuilder::new(url, manager, computer_name, inputs);
        if let Some(secret) = auth_secret {
            b = b.auth_secret(secret);
        }
        if let Some(h) = headers {
            b = b.headers(h);
        }
        b.connect().await
    }

    /// 真正的连接实现 / Actual connection implementation.
    ///
    /// 私有 helper，参数已由 [`SmcpComputerClientBuilder::connect`] 解析过默认值。
    /// Private helper; defaults already resolved by
    /// [`SmcpComputerClientBuilder::connect`].
    #[allow(clippy::too_many_arguments)]
    async fn connect_internal(
        url: String,
        manager: Arc<RwLock<Option<MCPServerManager>>>,
        computer_name: String,
        inputs: Arc<RwLock<HashMap<String, MCPServerInput>>>,
        auth_secret: Option<String>,
        auth_header_name: String,
        namespace: String,
        headers: Option<HashMap<String, String>>,
    ) -> ComputerResult<Self> {
        let office_id = Arc::new(RwLock::new(None));
        let manager_clone = manager.clone();
        let computer_name_clone = computer_name.clone();
        let office_id_clone = office_id.clone();
        let inputs_clone = inputs.clone();

        // HS-02 #22: 在连接 URL 注入权威 a2c_version（丢弃调用方自带值，防版本漂移），
        // 使服务端 HTTP 握手中间件能在 Socket.IO 业务层之前完成版本协商。
        // HS-02 #22: inject the authoritative a2c_version into the connection URL so the server's
        // HTTP handshake middleware can negotiate the version before the Socket.IO layer.
        let handshake_url =
            smcp::utils::handshake::build_handshake_url(&url, smcp::PROTOCOL_VERSION).map_err(
                |e| ComputerError::ConnectionError(format!("Invalid handshake URL: {e}")),
            )?;

        // 汇总握手 HTTP headers（鉴权 header + 自定义 headers），用于首连与 4900 改 polling 重连。
        // Collect handshake HTTP headers (auth + custom) for the primary connect and the 4900 retry.
        let mut handshake_headers: HashMap<String, String> = HashMap::new();
        if let Some(secret) = auth_secret {
            handshake_headers.insert(auth_header_name.clone(), secret);
        }
        if let Some(custom_headers) = headers {
            for (key, value) in custom_headers {
                handshake_headers.insert(key, value);
            }
        }

        // 使用ClientBuilder注册事件处理器
        // Use ClientBuilder to register event handlers
        let mut builder = ClientBuilder::new(&handshake_url).namespace(namespace.clone());

        // HS-02 #22: polling-first（先 HTTP polling 握手，可被服务端 400+4008 body 拦截，
        // 失败时再升级 WebSocket）。⚠️ 不可用 WS-only（TransportType::Websocket）——会绕过服务端
        // HTTP 版本握手中间件，使版本不兼容无法被感知。
        // HS-02 #22: polling-first (HTTP polling handshake can be intercepted by the server's
        // 400 + 4008 body, then upgrades to WebSocket). MUST NOT use WS-only — it bypasses the
        // server's HTTP version handshake gate.
        builder = builder.transport_type(TransportType::Any);

        // 添加握手 HTTP headers / Add handshake HTTP headers.
        // Safety: opening_header 底层使用 http::HeaderValue::from_bytes() 做 RFC 7230 校验，
        // 会拒绝包含 \r\n 等控制字符的恶意输入，无需额外防御 header injection。
        // Safety: opening_header internally uses http::HeaderValue::from_bytes() for RFC 7230
        // validation, rejecting \r\n and other control characters. No extra injection defense needed.
        for (key, value) in &handshake_headers {
            builder = builder.opening_header(key.as_str(), value.as_str());
        }

        // 注册事件处理器（on_any 消费并返回 builder）/ Register handlers (on_any consumes & returns builder)。
        builder = builder.on_any(move |event, payload, client| {
            // 只处理自定义事件
            // Only handle custom events
            let event_str = match event {
                Event::Custom(s) => s,
                _ => return async {}.boxed(),
            };

            match event_str.as_str() {
                CLIENT_TOOL_CALL => {
                    let manager = manager_clone.clone();
                    let computer_name = computer_name_clone.clone();
                    let office_id = office_id_clone.clone();
                    let client_clone = client.clone();
                    let payload_clone = payload.clone();

                    async move {
                        match Self::handle_tool_call_with_ack(
                            payload,
                            manager,
                            computer_name,
                            office_id,
                            client_clone,
                        )
                        .await
                        {
                            Ok((ack_id, response)) => {
                                if let Some(id) = ack_id {
                                    if let Err(e) = client.ack_with_id(id, response).await {
                                        error!("Failed to send ack: {}", e);
                                    }
                                }
                            }
                            Err(e) => {
                                error!("Error handling tool call: {}", e);
                                // 尝试返回错误响应 / Try to return error response
                                if let Ok((Some(id), _)) = Self::extract_ack_id(payload_clone) {
                                    let error_response = serde_json::json!({
                                        "isError": true,
                                        "content": [],
                                        "structuredContent": {
                                            "error": e.to_string(),
                                            "error_type": "ComputerError"
                                        }
                                    });
                                    let _ = client.ack_with_id(id, error_response).await;
                                }
                            }
                        }
                    }
                    .boxed()
                }
                CLIENT_GET_TOOLS => {
                    let manager = manager_clone.clone();
                    let computer_name = computer_name_clone.clone();
                    let office_id = office_id_clone.clone();
                    let client_clone = client.clone();

                    async move {
                        match Self::handle_get_tools_with_ack(
                            payload,
                            manager,
                            computer_name,
                            office_id,
                            client_clone,
                        )
                        .await
                        {
                            Ok((ack_id, response)) => {
                                if let Some(id) = ack_id {
                                    if let Err(e) = client.ack_with_id(id, response).await {
                                        error!("Failed to send ack: {}", e);
                                    }
                                }
                            }
                            Err(e) => {
                                error!("Error handling get tools: {}", e);
                            }
                        }
                    }
                    .boxed()
                }
                CLIENT_GET_CONFIG => {
                    let manager = manager_clone.clone();
                    let computer_name = computer_name_clone.clone();
                    let office_id = office_id_clone.clone();
                    let client_clone = client.clone();
                    let inputs = inputs_clone.clone();

                    async move {
                        match Self::handle_get_config_with_ack(
                            payload,
                            manager,
                            computer_name,
                            office_id,
                            client_clone,
                            inputs,
                        )
                        .await
                        {
                            Ok((ack_id, response)) => {
                                if let Some(id) = ack_id {
                                    if let Err(e) = client.ack_with_id(id, response).await {
                                        error!("Failed to send ack: {}", e);
                                    }
                                }
                            }
                            Err(e) => {
                                error!("Error handling get config: {}", e);
                            }
                        }
                    }
                    .boxed()
                }
                CLIENT_GET_DESKTOP => {
                    let manager = manager_clone.clone();
                    let computer_name = computer_name_clone.clone();
                    let office_id = office_id_clone.clone();
                    let client_clone = client.clone();

                    async move {
                        match Self::handle_get_desktop_with_ack(
                            payload,
                            manager,
                            computer_name,
                            office_id,
                            client_clone,
                        )
                        .await
                        {
                            Ok((ack_id, response)) => {
                                if let Some(id) = ack_id {
                                    if let Err(e) = client.ack_with_id(id, response).await {
                                        error!("Failed to send ack: {}", e);
                                    }
                                }
                            }
                            Err(e) => {
                                error!("Error handling get desktop: {}", e);
                            }
                        }
                    }
                    .boxed()
                }
                _ => {
                    debug!("Unhandled event: {}", event_str);
                    async {}.boxed()
                }
            }
        });

        // 连接服务器（polling-first 已设；分类版本握手错误，4900 时改 polling 取 4008）
        // Connect (polling-first already set; classify version-handshake errors; on 4900 re-fetch
        // the authoritative 4008 over polling).
        let client = match smcp_client_transport::connect_and_classify(
            builder,
            &handshake_url,
            &namespace,
            None,
            handshake_headers,
        )
        .await
        {
            Ok(client) => client,
            Err(smcp_client_transport::ConnectError::ProtocolVersion(pve)) => {
                return Err(ComputerError::ProtocolVersionMismatch(pve));
            }
            Err(smcp_client_transport::ConnectError::Connection(msg)) => {
                return Err(ComputerError::SocketIoError(msg));
            }
        };

        // 等待一小段时间确保 Socket.IO namespace 连接完全建立
        // Wait a short time to ensure Socket.IO namespace connection is fully established
        // Socket.IO 有两个连接阶段：Transport 层和 Namespace 层
        // Socket.IO has two connection phases: Transport layer and Namespace layer
        // connect() 只保证 Transport 层连接，namespace 连接是异步的
        // connect() only guarantees Transport layer connection, namespace connection is async
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        info!(
            "Connected to SMCP server at {} with computer name: {}",
            url, computer_name
        );

        Ok(Self {
            client,
            computer_name,
            office_id,
            inputs,
            namespace,
            auth_header_name,
        })
    }

    /// 加入Office（Socket.IO Room）
    /// Join an Office (Socket.IO Room)
    pub async fn join_office(&self, office_id: &str) -> ComputerResult<()> {
        debug!("Joining office: {}", office_id);

        // 先设置office_id
        // Set office_id first
        *self.office_id.write().await = Some(office_id.to_string());

        let req_data = serde_json::json!({
            "office_id": office_id,
            "role": "computer",
            "name": self.computer_name
        });

        // 使用call方法等待服务器响应
        // Use call method to wait for server response
        match self.call(SERVER_JOIN_OFFICE, req_data, Some(10)).await {
            Ok(response) => {
                // 服务器返回的是 (bool, Option<String>) 元组序列化后的数组
                // Server returns serialized array of (bool, Option<String>) tuple
                debug!("Join office response: {:?}", response);

                // 检查响应是否包含嵌套数组
                // Check if response contains nested array
                let actual_response = if response.len() == 1 {
                    if let Some(arr) = response.first().and_then(|v| v.as_array()) {
                        arr.to_vec()
                    } else {
                        response
                    }
                } else {
                    response
                };

                if !actual_response.is_empty() {
                    if let Some(success) = actual_response.first().and_then(|v| v.as_bool()) {
                        if success {
                            info!("Successfully joined office: {}", office_id);
                            Ok(())
                        } else {
                            // 加入失败，重置office_id / Reset office_id on failure
                            *self.office_id.write().await = None;
                            let error_msg = actual_response
                                .get(1)
                                .and_then(|v| v.as_str())
                                .unwrap_or("Unknown error");
                            Err(ComputerError::SocketIoError(format!(
                                "Failed to join office: {}",
                                error_msg
                            )))
                        }
                    } else {
                        *self.office_id.write().await = None;
                        Err(ComputerError::SocketIoError(format!(
                            "Invalid response format from server: {:?}",
                            actual_response
                        )))
                    }
                } else {
                    *self.office_id.write().await = None;
                    Err(ComputerError::SocketIoError(
                        "Empty response from server".to_string(),
                    ))
                }
            }
            Err(e) => {
                *self.office_id.write().await = None;
                Err(e)
            }
        }
    }

    /// 获取当前Office ID / Get current Office ID
    pub async fn get_current_office_id(&self) -> ComputerResult<String> {
        let office_id = self.office_id.read().await;
        match office_id.as_ref() {
            Some(id) => Ok(id.clone()),
            None => Err(ComputerError::InvalidState(
                "Not currently in any office".to_string(),
            )),
        }
    }

    /// 离开Office
    /// Leave an Office
    pub async fn leave_office(&self, office_id: &str) -> ComputerResult<()> {
        debug!("Leaving office: {}", office_id);

        let req_data = serde_json::json!({
            "office_id": office_id
        });

        self.emit(SERVER_LEAVE_OFFICE, req_data).await?;
        *self.office_id.write().await = None;

        info!("Left office: {}", office_id);
        Ok(())
    }

    /// 发送配置更新通知
    /// Emit config update notification
    pub async fn emit_update_config(&self) -> ComputerResult<()> {
        let office_id = self.office_id.read().await;
        if office_id.is_some() {
            let req_data = serde_json::json!({
                "computer": self.computer_name
            });
            self.emit(SERVER_UPDATE_CONFIG, req_data).await?;
            info!("Emitted config update notification");
        }
        Ok(())
    }

    /// 发送工具列表更新通知
    /// Emit tool list update notification
    pub async fn emit_update_tool_list(&self) -> ComputerResult<()> {
        let office_id = self.office_id.read().await;
        if office_id.is_some() {
            let req_data = serde_json::json!({
                "computer": self.computer_name
            });
            self.emit(SERVER_UPDATE_TOOL_LIST, req_data).await?;
            info!("Emitted tool list update notification");
        }
        Ok(())
    }

    /// 发送桌面更新通知
    /// Emit desktop update notification
    pub async fn emit_update_desktop(&self) -> ComputerResult<()> {
        let office_id = self.office_id.read().await;
        if office_id.is_some() {
            let req_data = serde_json::json!({
                "computer": self.computer_name
            });
            self.emit(SERVER_UPDATE_DESKTOP, req_data).await?;
            info!("Emitted desktop update notification");
        }
        Ok(())
    }

    /// 发送事件（不等待响应）
    /// Emit event without waiting for response
    async fn emit(&self, event: &str, data: Value) -> ComputerResult<()> {
        // 检查事件名 policy / Check event name policy
        if event.starts_with("notify:") || event.starts_with("client:") {
            return Err(ComputerError::InvalidState(
                format!(
                    "Computer 不允许发送 notify:* 或 client:* 事件 / Computer cannot send notify:* or client:* events: {}",
                    event
                )
            ));
        }

        debug!("Emitting event: {}", event);

        self.client
            .emit(event, Payload::Text(vec![data], None))
            .await
            .map_err(|e| ComputerError::SocketIoError(format!("Failed to emit {}: {}", event, e)))
    }

    /// 发送事件并等待响应
    /// Emit event and wait for response
    async fn call(
        &self,
        event: &str,
        data: Value,
        timeout_secs: Option<u64>,
    ) -> ComputerResult<Vec<Value>> {
        // 检查事件名 policy / Check event name policy
        if event.starts_with("notify:") || event.starts_with("client:") {
            return Err(ComputerError::InvalidState(
                format!(
                    "Computer 不允许发送 notify:* 或 client:* 事件 / Computer cannot send notify:* or client:* events: {}",
                    event
                )
            ));
        }

        let timeout = std::time::Duration::from_secs(timeout_secs.unwrap_or(30));
        debug!("Calling event: {} with timeout {:?}", event, timeout);

        let (tx, rx) = tokio::sync::oneshot::channel();
        let tx = Arc::new(std::sync::Mutex::new(Some(tx)));

        let callback = move |payload: Payload, _client: Client| {
            if let Some(tx_opt) = tx.try_lock().ok().and_then(|mut m| m.take()) {
                let _ = tx_opt.send(payload);
            }
            async {}.boxed()
        };

        self.client
            .emit_with_ack(event, Payload::Text(vec![data], None), timeout, callback)
            .await
            .map_err(|e| {
                ComputerError::SocketIoError(format!("Failed to call {}: {}", event, e))
            })?;

        // 使用 tokio::time::timeout 来确保 rx.await 不会无限期等待
        // Use tokio::time::timeout to ensure rx.await doesn't wait forever
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(response)) => {
                // 从响应中提取JSON数据 / Extract JSON data from response
                match response {
                    Payload::Text(values, _) => {
                        debug!("Received response: {:?}", values);
                        Ok(values)
                    }
                    #[allow(deprecated)]
                    Payload::String(s, _) => {
                        // 尝试解析字符串为JSON数组
                        // Try to parse string as JSON array
                        let parsed: Vec<Value> = serde_json::from_str(&s).map_err(|e| {
                            ComputerError::SocketIoError(format!("Failed to parse response: {}", e))
                        })?;
                        debug!("Received parsed response: {:?}", parsed);
                        Ok(parsed)
                    }
                    Payload::Binary(_, _) => Err(ComputerError::SocketIoError(
                        "Binary response not supported".to_string(),
                    )),
                }
            }
            Ok(Err(_)) => {
                error!("Channel closed while calling event: {}", event);
                Err(ComputerError::SocketIoError(
                    "Channel closed while waiting for response".to_string(),
                ))
            }
            Err(_) => {
                error!("Timeout while calling event: {}", event);
                Err(ComputerError::SocketIoError(
                    "Timeout while waiting for response".to_string(),
                ))
            }
        }
    }

    /// 处理工具调用事件（带ACK响应）
    /// Handle tool call event (with ACK response)
    async fn handle_tool_call_with_ack(
        payload: Payload,
        manager: Arc<RwLock<Option<MCPServerManager>>>,
        computer_name: String,
        _office_id: Arc<RwLock<Option<String>>>,
        _client: Client,
    ) -> ComputerResult<(Option<i32>, Value)> {
        let (ack_id, req) = Self::extract_ack_and_parse::<ToolCallReq>(payload)?;

        // 验证 computer_name（Server 路由已保证请求来自同一 office，无需验证 agent 字段）
        // Validate computer_name (Server routing ensures request is from same office, no need to validate agent field)
        if computer_name != req.computer {
            return Err(ComputerError::ValidationError(format!(
                "Computer name mismatch: expected {}, got {}",
                computer_name, req.computer
            )));
        }

        // 执行工具调用 / Execute tool call
        let result = {
            let manager_guard = manager.read().await;
            match manager_guard.as_ref() {
                Some(mgr) => {
                    mgr.execute_tool(
                        &req.tool_name,
                        req.params,
                        Some(std::time::Duration::from_secs(req.timeout as u64)),
                    )
                    .await?
                }
                None => {
                    return Err(ComputerError::InvalidState(
                        "MCP Manager not initialized".to_string(),
                    ));
                }
            }
        };

        let result_value =
            serde_json::to_value(result).map_err(ComputerError::SerializationError)?;

        info!("Tool call executed successfully: {}", req.tool_name);
        Ok((ack_id, result_value))
    }

    /// 处理获取工具列表事件（带ACK响应）
    /// Handle get tools event (with ACK response)
    async fn handle_get_tools_with_ack(
        payload: Payload,
        manager: Arc<RwLock<Option<MCPServerManager>>>,
        computer_name: String,
        _office_id: Arc<RwLock<Option<String>>>,
        _client: Client,
    ) -> ComputerResult<(Option<i32>, Value)> {
        let (ack_id, req) = Self::extract_ack_and_parse::<GetToolsReq>(payload)?;

        // 验证 computer_name（Server 路由已保证请求来自同一 office，无需验证 agent 字段）
        // Validate computer_name (Server routing ensures request is from same office, no need to validate agent field)
        if computer_name != req.computer {
            return Err(ComputerError::ValidationError(format!(
                "Computer name mismatch: expected {}, got {}",
                computer_name, req.computer
            )));
        }

        // 获取工具列表 / Get tools list
        let tools: Vec<smcp::SMCPTool> = {
            let manager_guard = manager.read().await;
            match manager_guard.as_ref() {
                Some(mgr) => {
                    // 转换Tool为SMCPTool
                    // Convert Tool to SMCPTool
                    let tool_list = mgr.list_available_tools().await;
                    tool_list
                        .into_iter()
                        .map(convert_tool_to_smcp_tool)
                        .collect()
                }
                None => {
                    return Err(ComputerError::InvalidState(
                        "MCP Manager not initialized".to_string(),
                    ));
                }
            }
        };

        let response = GetToolsRet {
            tools: tools.clone(),
            req_id: req.base.req_id,
        };

        info!(
            "Returned {} tools for agent {}",
            tools.len(),
            req.base.agent
        );
        Ok((ack_id, serde_json::to_value(response)?))
    }

    /// 处理获取配置事件（带ACK响应）
    /// Handle get config event (with ACK response)
    async fn handle_get_config_with_ack(
        payload: Payload,
        manager: Arc<RwLock<Option<MCPServerManager>>>,
        computer_name: String,
        _office_id: Arc<RwLock<Option<String>>>,
        _client: Client,
        inputs: Arc<RwLock<HashMap<String, MCPServerInput>>>,
    ) -> ComputerResult<(Option<i32>, Value)> {
        let (ack_id, req) = Self::extract_ack_and_parse::<GetComputerConfigReq>(payload)?;

        // 验证 computer_name（Server 路由已保证请求来自同一 office，无需验证 agent 字段）
        // Validate computer_name (Server routing ensures request is from same office, no need to validate agent field)
        if computer_name != req.computer {
            return Err(ComputerError::ValidationError(format!(
                "Computer name mismatch: expected {}, got {}",
                computer_name, req.computer
            )));
        }

        // 获取配置 / Get config
        let servers = {
            let manager_guard = manager.read().await;
            match manager_guard.as_ref() {
                Some(mgr) => {
                    // 获取完整服务器配置（不只是状态）
                    // Get complete server configurations (not just status)
                    mgr.get_server_configs().await
                }
                None => {
                    return Err(ComputerError::InvalidState(
                        "MCP Manager not initialized".to_string(),
                    ));
                }
            }
        };

        // 获取输入定义 / Get input definitions
        // 将 HashMap<String, MCPServerInput> 转换为 Vec<serde_json::Value>
        // Convert HashMap<String, MCPServerInput> to Vec<serde_json::Value>
        let inputs_data = {
            let inputs_guard = inputs.read().await;
            if inputs_guard.is_empty() {
                None
            } else {
                let inputs_vec: Vec<serde_json::Value> = inputs_guard
                    .values()
                    .filter_map(|input| serde_json::to_value(input).ok())
                    .collect();
                if inputs_vec.is_empty() {
                    None
                } else {
                    Some(inputs_vec)
                }
            }
        };

        let response = GetComputerConfigRet {
            servers,
            inputs: inputs_data,
        };

        info!("Returned config for agent {}", req.base.agent);
        Ok((ack_id, serde_json::to_value(response)?))
    }

    /// 处理获取桌面事件（带ACK响应）
    /// Handle get desktop event (with ACK response)
    async fn handle_get_desktop_with_ack(
        payload: Payload,
        manager: Arc<RwLock<Option<MCPServerManager>>>,
        computer_name: String,
        _office_id: Arc<RwLock<Option<String>>>,
        _client: Client,
    ) -> ComputerResult<(Option<i32>, Value)> {
        let (ack_id, req) = Self::extract_ack_and_parse::<GetDesktopReq>(payload)?;

        // 验证 computer_name（Server 路由已保证请求来自同一 office，无需验证 agent 字段）
        // Validate computer_name (Server routing ensures request is from same office, no need to validate agent field)
        if computer_name != req.computer {
            return Err(ComputerError::ValidationError(format!(
                "Computer name mismatch: expected {}, got {}",
                computer_name, req.computer
            )));
        }

        // 获取桌面窗口信息 / Get desktop window info
        let desktops = {
            let mgr_guard = manager.read().await;
            if let Some(mgr) = mgr_guard.as_ref() {
                let raw_windows = mgr.get_windows_details(req.window.as_deref()).await;
                let windows: Vec<WindowInfo> = raw_windows
                    .into_iter()
                    .map(|(server_name, resource, read_result)| {
                        WindowInfo::new(server_name, resource, read_result)
                    })
                    .collect();
                organize_desktop(windows, req.desktop_size.map(|s| s as usize), &[])
            } else {
                Vec::new()
            }
        };

        let response = GetDesktopRet {
            desktops: Some(desktops),
            req_id: req.base.req_id,
        };

        info!("Returned desktop for agent {}", req.base.agent);
        Ok((ack_id, serde_json::to_value(response)?))
    }

    /// 从payload中提取ack_id并解析数据
    /// Extract ack_id from payload and parse data
    fn extract_ack_and_parse<T: serde::de::DeserializeOwned>(
        payload: Payload,
    ) -> ComputerResult<(Option<i32>, T)> {
        match payload {
            Payload::Text(mut values, ack_id) => {
                if let Some(value) = values.pop() {
                    let req =
                        serde_json::from_value(value).map_err(ComputerError::SerializationError)?;
                    Ok((ack_id, req))
                } else {
                    Err(ComputerError::ProtocolError("Empty payload".to_string()))
                }
            }
            #[allow(deprecated)]
            Payload::String(s, ack_id) => {
                let req = serde_json::from_str(&s).map_err(ComputerError::SerializationError)?;
                Ok((ack_id, req))
            }
            Payload::Binary(_, _) => Err(ComputerError::SocketIoError(
                "Binary payload not supported".to_string(),
            )),
        }
    }

    /// 仅提取ack_id（用于错误处理）
    /// Extract ack_id only (for error handling)
    fn extract_ack_id(payload: Payload) -> ComputerResult<(Option<i32>, ())> {
        match payload {
            Payload::Text(_, ack_id) => Ok((ack_id, ())),
            #[allow(deprecated)]
            Payload::String(_, ack_id) => Ok((ack_id, ())),
            Payload::Binary(_, _) => Ok((None, ())),
        }
    }

    /// 断开连接
    /// Disconnect from server
    pub async fn disconnect(self) -> ComputerResult<()> {
        debug!("Disconnecting from server");
        self.client
            .disconnect()
            .await
            .map_err(|e| ComputerError::SocketIoError(format!("Failed to disconnect: {}", e)))?;
        info!("Disconnected from server");
        Ok(())
    }

    /// 获取当前office ID
    /// Get current office ID
    pub async fn get_office_id(&self) -> Option<String> {
        self.office_id.read().await.clone()
    }

    /// 获取连接的 URL
    /// Get connected URL
    pub fn get_url(&self) -> String {
        // 由于 tf_rust_socketio 的 Client 没有 uri() 方法，返回默认值
        // Since tf_rust_socketio Client doesn't have uri() method, return default
        "unknown".to_string()
    }

    /// 获取握手时使用的 Socket.IO namespace（实际配置值，非字面量）。
    /// Get the Socket.IO namespace used at handshake time (the actual
    /// configured value, not a hardcoded literal).
    pub fn get_namespace(&self) -> String {
        self.namespace.clone()
    }

    /// 获取实际写入 HTTP header 的鉴权 key 名（默认 [`DEFAULT_AUTH_HEADER_NAME`]）。
    /// Get the auth HTTP header key name actually used on the wire (defaults to
    /// [`DEFAULT_AUTH_HEADER_NAME`]).
    pub fn get_auth_header_name(&self) -> &str {
        &self.auth_header_name
    }
}

/// 将内部 Tool 转换为协议类型 SMCPTool
/// Convert internal Tool to protocol type SMCPTool
pub(crate) fn convert_tool_to_smcp_tool(tool: crate::mcp_clients::model::Tool) -> smcp::SMCPTool {
    let mut meta_map = serde_json::Map::new();

    // 传递 tool.meta 中的所有键值（如 a2c_tool_meta）
    // 值需要序列化为 JSON 字符串，与 Python SDK 对齐
    if let Some(existing_meta) = &tool.meta {
        for (k, v) in existing_meta.iter() {
            let str_val = if v.is_string() {
                v.as_str().unwrap().to_string()
            } else {
                serde_json::to_string(v).unwrap_or_default()
            };
            meta_map.insert(k.clone(), serde_json::Value::String(str_val));
        }
    }

    // 添加 MCP_TOOL_ANNOTATION
    if let Some(annotations) = &tool.annotations {
        if let Ok(json_str) = serde_json::to_string(annotations) {
            meta_map.insert(
                "MCP_TOOL_ANNOTATION".to_string(),
                serde_json::Value::String(json_str),
            );
        }
    }

    let meta = if meta_map.is_empty() {
        None
    } else {
        Some(serde_json::Value::Object(meta_map))
    };

    let description = tool.description.as_deref().unwrap_or("").to_string();
    let params_schema = tool.schema_as_json_value();
    smcp::SMCPTool {
        name: tool.name.to_string(),
        description,
        params_schema,
        return_schema: None,
        meta,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_clients::model::{Tool, ToolAnnotations};
    use serde_json::json;

    fn make_tool(
        meta: Option<serde_json::Map<String, serde_json::Value>>,
        annotations: Option<ToolAnnotations>,
    ) -> Tool {
        use std::sync::Arc;
        let input_schema: serde_json::Map<String, serde_json::Value> =
            serde_json::from_value(json!({"type": "object"})).unwrap();
        Tool {
            name: "test_tool".into(),
            title: None,
            description: Some("A test tool".into()),
            input_schema: Arc::new(input_schema),
            output_schema: None,
            annotations,
            icons: None,
            meta: meta.map(rmcp::model::Meta),
        }
    }

    #[test]
    fn test_tool_to_smcp_tool_with_meta_and_annotations() {
        let mut meta = serde_json::Map::new();
        meta.insert(
            "a2c_tool_meta".to_string(),
            json!({"tags": ["browser"], "priority": 1}),
        );
        let annotations = ToolAnnotations {
            title: Some("Test".to_string()),
            read_only_hint: Some(false),
            destructive_hint: Some(false),
            idempotent_hint: None,
            open_world_hint: Some(false),
        };
        let smcp_tool = convert_tool_to_smcp_tool(make_tool(Some(meta), Some(annotations)));

        let meta_obj = smcp_tool.meta.unwrap();
        let meta_map = meta_obj.as_object().unwrap();
        assert!(meta_map.contains_key("a2c_tool_meta"));
        assert!(meta_map.contains_key("MCP_TOOL_ANNOTATION"));
        // Values should be JSON strings
        assert!(meta_map["a2c_tool_meta"].is_string());
        assert!(meta_map["MCP_TOOL_ANNOTATION"].is_string());
    }

    #[test]
    fn test_tool_to_smcp_tool_only_meta() {
        let mut meta = serde_json::Map::new();
        meta.insert("a2c_tool_meta".to_string(), json!({"tags": ["fs"]}));
        let smcp_tool = convert_tool_to_smcp_tool(make_tool(Some(meta), None));

        let meta_obj = smcp_tool.meta.unwrap();
        let meta_map = meta_obj.as_object().unwrap();
        assert_eq!(meta_map.len(), 1);
        assert!(meta_map.contains_key("a2c_tool_meta"));
    }

    #[test]
    fn test_tool_to_smcp_tool_only_annotations() {
        let annotations = ToolAnnotations {
            title: Some("My Tool".to_string()),
            read_only_hint: Some(true),
            destructive_hint: Some(false),
            idempotent_hint: None,
            open_world_hint: Some(false),
        };
        let smcp_tool = convert_tool_to_smcp_tool(make_tool(None, Some(annotations)));

        let meta_obj = smcp_tool.meta.unwrap();
        let meta_map = meta_obj.as_object().unwrap();
        assert_eq!(meta_map.len(), 1);
        assert!(meta_map.contains_key("MCP_TOOL_ANNOTATION"));
    }

    #[test]
    fn test_tool_to_smcp_tool_no_meta_no_annotations() {
        let smcp_tool = convert_tool_to_smcp_tool(make_tool(None, None));
        assert!(smcp_tool.meta.is_none());
    }

    #[test]
    fn test_tool_to_smcp_tool_string_value_not_double_serialized() {
        let mut meta = serde_json::Map::new();
        meta.insert(
            "simple_key".to_string(),
            serde_json::Value::String("already_a_string".to_string()),
        );
        let smcp_tool = convert_tool_to_smcp_tool(make_tool(Some(meta), None));

        let meta_obj = smcp_tool.meta.unwrap();
        let meta_map = meta_obj.as_object().unwrap();
        // Should be the raw string, not "\"already_a_string\""
        assert_eq!(meta_map["simple_key"].as_str().unwrap(), "already_a_string");
    }
}
