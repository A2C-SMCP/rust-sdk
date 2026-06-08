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
        CLIENT_GET_CONFIG, CLIENT_GET_DESKTOP, CLIENT_GET_RESOURCES, CLIENT_GET_TOOLS,
        CLIENT_TOOL_CALL, SERVER_JOIN_OFFICE, SERVER_LEAVE_OFFICE, SERVER_UPDATE_CONFIG,
        SERVER_UPDATE_DESKTOP, SERVER_UPDATE_SKILLS, SERVER_UPDATE_TOOL_LIST,
    },
    GetComputerConfigReq, GetComputerConfigRet, GetDesktopReq, GetDesktopRet, GetResourcesReq,
    GetResourcesRet, GetToolsReq, GetToolsRet, ToolCallReq, SMCP_NAMESPACE,
};
use std::collections::HashMap;
use std::sync::Arc;
use tf_rust_socketio::{
    asynchronous::{Client, ClientBuilder},
    Event, Payload, TransportType,
};
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

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
                CLIENT_GET_RESOURCES => {
                    let manager = manager_clone.clone();
                    let computer_name = computer_name_clone.clone();

                    async move {
                        match Self::handle_get_resources_with_ack(payload, manager, computer_name)
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
                                error!("Error handling get resources: {}", e);
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

    /// 发送 SKILL 集合更新通知（`server:update_skills` → Server 广播 `notify:update_skills`，SRV-02 #50）
    /// Emit SKILL-set update notification (INT-01 #68; handler/broadcast refined by SRV-02/#72)
    pub async fn emit_update_skills(&self) -> ComputerResult<()> {
        let office_id = self.office_id.read().await;
        if office_id.is_some() {
            let req_data = serde_json::json!({
                "computer": self.computer_name
            });
            self.emit(SERVER_UPDATE_SKILLS, req_data).await?;
            info!("Emitted SKILL update notification");
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

    /// 处理资源发现事件（带 ACK 响应）/ Handle get_resources event (with ACK).
    ///
    /// 透明转发指定 MCP Server 的 `resources/list`：单页透传（cursor 入参/`next_cursor` 出参原样转发），
    /// **不**聚合、**不**做 scheme/元数据过滤、**不**返回 `resourceTemplates`。错误经 ACK 第一参回传
    /// **flat ErrorPayload**（禁止嵌套 envelope）：未知 `mcp_server` → 4014（顶层平铺 `mcp_server_name`）；
    /// 目标 server 未声明 `resources` 能力 → 4015（顶层平铺 `mcp_server_name` + `capability`）。对齐
    /// Python `on_get_resources`（RES-01 #30，协议 0.2.0 `client:get_resources`）。
    async fn handle_get_resources_with_ack(
        payload: Payload,
        manager: Arc<RwLock<Option<MCPServerManager>>>,
        computer_name: String,
    ) -> ComputerResult<(Option<i32>, Value)> {
        let (ack_id, req) = Self::extract_ack_and_parse::<GetResourcesReq>(payload)?;

        // 验证 computer_name（Server 路由已保证请求来自同一 office，无需验证 agent 字段）
        if computer_name != req.computer {
            return Err(ComputerError::ValidationError(format!(
                "Computer name mismatch: expected {}, got {}",
                computer_name, req.computer
            )));
        }

        // 单页透传 MCP `resources/list` / single-page passthrough。
        let result = {
            let manager_guard = manager.read().await;
            match manager_guard.as_ref() {
                Some(mgr) => {
                    mgr.list_resources(&req.mcp_server, req.cursor.clone())
                        .await
                }
                None => {
                    return Err(ComputerError::InvalidState(
                        "MCP Manager not initialized".to_string(),
                    ));
                }
            }
        };

        match result {
            Ok((resources, next_cursor)) => {
                let response = GetResourcesRet {
                    resources: resources.iter().map(to_a2c_resource).collect(),
                    next_cursor,
                    req_id: Some(req.base.req_id),
                };
                info!(
                    "Returned {} resource(s) from '{}' for agent {}",
                    response.resources.len(),
                    req.mcp_server,
                    req.base.agent
                );
                Ok((ack_id, serde_json::to_value(response)?))
            }
            // 未知 mcp_server → 4014 flat ErrorPayload（ACK 第一参回传）。
            Err(ComputerError::McpServerNotFound(server)) => {
                warn!(
                    "client:get_resources references unregistered MCP server '{}'",
                    server
                );
                let payload = smcp::ErrorPayload::from_error_code(
                    smcp::ErrorCode::McpServerNotFound,
                    "MCP Server not registered",
                )
                .with_mcp_server_name(server);
                Ok((ack_id, serde_json::to_value(payload)?))
            }
            // capability 不支持 → 4015 flat ErrorPayload。
            Err(ComputerError::McpCapabilityNotSupported {
                server_name,
                capability,
            }) => {
                warn!(
                    "client:get_resources MCP server '{}' does not support '{}' capability",
                    server_name, capability
                );
                let payload = smcp::ErrorPayload::from_error_code(
                    smcp::ErrorCode::McpCapabilityNotSupported,
                    "MCP Server does not support the requested capability",
                )
                .with_mcp_server_name(server_name)
                .with_capability(capability);
                Ok((ack_id, serde_json::to_value(payload)?))
            }
            Err(other) => Err(other),
        }
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

/// 将 MCP `Resource`（rmcp `Annotated<RawResource>`）转换为 A2C 协议层 [`smcp::A2CResource`]（snake_case
/// mirror）/ Convert an MCP `Resource` to the A2C protocol-level `A2CResource`。
///
/// 元数据分工 / metadata partition：MCP 标准 annotations（`priority`/`audience`/`last_modified`）→
/// `annotations`；rmcp `_meta`（A2C 扩展，如 `fullscreen`）原样搬运到 `_meta`。`audience` 的
/// `Role::{User,Assistant}` 映射到 [`smcp::ResourceAudience`]；`last_modified` 的 `DateTime<Utc>`
/// 序列化为 RFC3339（ISO 8601）字符串。对齐 Python `_to_a2c_resource`（RES-01 #30）。
pub(crate) fn to_a2c_resource(resource: &crate::mcp_clients::model::Resource) -> smcp::A2CResource {
    let annotations = resource
        .annotations
        .as_ref()
        .map(|ann| smcp::ResourceAnnotations {
            audience: ann.audience.as_ref().map(|roles| {
                roles
                    .iter()
                    .map(|role| match role {
                        rmcp::model::Role::User => smcp::ResourceAudience::User,
                        rmcp::model::Role::Assistant => smcp::ResourceAudience::Assistant,
                    })
                    .collect()
            }),
            priority: ann.priority,
            last_modified: ann.last_modified.map(|dt| dt.to_rfc3339()),
        })
        // 与 Python `_to_a2c_resource` 的 `if ann:` 守卫对齐：三字段全 None 时折叠为 None，避免线格式
        // 产出空 `"annotations": {}`（rmcp 把入参 `"annotations": {}` 解析为 `Some(全 None)`）造成
        // 跨-SDK 字节分歧 / fold to None when all sub-fields are None, mirroring Python's truthiness guard.
        .filter(|a| a.audience.is_some() || a.priority.is_some() || a.last_modified.is_some());

    // rmcp `_meta`（`Meta(JsonObject)`）→ `serde_json::Value::Object`，原样搬运 A2C 扩展字段。
    let meta = resource
        .meta
        .as_ref()
        .map(|m| serde_json::Value::Object(m.0.clone()));

    smcp::A2CResource {
        uri: Some(resource.uri.clone()),
        name: Some(resource.name.clone()),
        description: resource.description.clone(),
        mime_type: resource.mime_type.clone(),
        size: resource.size.map(u64::from),
        annotations,
        meta,
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

    // ===== RES-01 #30: to_a2c_resource 转换器 =====

    #[test]
    fn test_to_a2c_resource_full() {
        use crate::mcp_clients::model::{Annotated, RawResource};
        use rmcp::model::{Annotations, Meta, Role};

        let mut raw = RawResource::new("window://app/w1", "Win");
        raw.description = Some("desc".into());
        raw.mime_type = Some("text/plain".into());
        raw.size = Some(42);
        let mut m = serde_json::Map::new();
        m.insert("fullscreen".into(), serde_json::Value::Bool(true));
        raw.meta = Some(Meta(m));
        // last_modified 设为定值，覆盖 DateTime<Utc> → RFC3339 映射分支。
        let dt: chrono::DateTime<chrono::Utc> = "2025-01-02T03:04:05Z".parse().unwrap();
        let ann = Annotations {
            audience: Some(vec![Role::Assistant]),
            priority: Some(0.7),
            last_modified: Some(dt),
        };
        let a2c = to_a2c_resource(&Annotated::new(raw, Some(ann)));

        assert_eq!(a2c.uri.as_deref(), Some("window://app/w1"));
        assert_eq!(a2c.name.as_deref(), Some("Win"));
        assert_eq!(a2c.description.as_deref(), Some("desc"));
        assert_eq!(a2c.mime_type.as_deref(), Some("text/plain"));
        assert_eq!(a2c.size, Some(42u64));
        let out_ann = a2c.annotations.expect("annotations preserved");
        assert_eq!(out_ann.priority, Some(0.7));
        assert_eq!(
            out_ann.audience,
            Some(vec![smcp::ResourceAudience::Assistant])
        );
        // RFC3339（ISO 8601）字符串，UTC 以 `+00:00` 结尾。
        assert_eq!(
            out_ann.last_modified.as_deref(),
            Some("2025-01-02T03:04:05+00:00")
        );
        // _meta（A2C 扩展，如 fullscreen）原样搬运。
        assert_eq!(
            a2c.meta.unwrap()["fullscreen"],
            serde_json::Value::Bool(true)
        );
    }

    #[test]
    fn test_to_a2c_resource_empty_annotations_folded() {
        use crate::mcp_clients::model::{Annotated, RawResource};
        use rmcp::model::Annotations;

        // MCP server 主动发回 `"annotations": {}` → rmcp 解析为 Some(全 None)。转换器须折叠为 None，
        // 与 Python `if ann:` 守卫字节对齐（否则线格式残留空 `"annotations": {}`）。
        let raw = RawResource::new("custom://a/b", "R");
        let a2c = to_a2c_resource(&Annotated::new(raw, Some(Annotations::default())));
        assert!(
            a2c.annotations.is_none(),
            "all-None annotations must fold to None"
        );
    }

    #[test]
    fn test_to_a2c_resource_minimal() {
        // 裸资源（无 annotations / _meta）：仅 uri / name，annotations/meta 缺省。
        let res = crate::mcp_clients::model::make_resource("custom://x/y", "Y", None, None);
        let a2c = to_a2c_resource(&res);
        assert_eq!(a2c.uri.as_deref(), Some("custom://x/y"));
        assert_eq!(a2c.name.as_deref(), Some("Y"));
        assert!(a2c.annotations.is_none());
        assert!(a2c.meta.is_none());
        assert!(a2c.description.is_none());
    }

    // ===== RES-01 #30: on_get_resources 透明转发 handler =====

    /// 构造 `client:get_resources` 的 wire 形态 payload（flatten 后 agent/req_id 顶层）。
    fn get_resources_payload(
        computer: &str,
        mcp_server: &str,
        cursor: Option<&str>,
        ack: i32,
    ) -> Payload {
        let mut obj = json!({
            "agent": "agent-1",
            "req_id": "req-1",
            "computer": computer,
            "mcp_server": mcp_server,
        });
        if let Some(c) = cursor {
            obj.as_object_mut()
                .unwrap()
                .insert("cursor".into(), json!(c));
        }
        Payload::Text(vec![obj], Some(ack))
    }

    fn mock(
        pages: Vec<Vec<crate::mcp_clients::model::Resource>>,
        cap_fail: bool,
    ) -> crate::mcp_clients::manager::test_support::MockSkillClient {
        crate::mcp_clients::manager::test_support::MockSkillClient {
            pages,
            fail: false,
            cap_fail,
            read_text: String::new(),
        }
    }

    #[tokio::test]
    async fn test_get_resources_single_page_passthrough() {
        use crate::mcp_clients::manager::{test_support::inject, MCPServerManager};
        use crate::mcp_clients::model::make_resource;

        let manager = MCPServerManager::new();
        inject(
            &manager,
            "srv-1",
            mock(
                vec![vec![
                    make_resource("window://app/w1", "W1", None, None),
                    make_resource("custom://x/y", "Y", None, None),
                ]],
                false,
            ),
        )
        .await;
        let manager = Arc::new(RwLock::new(Some(manager)));

        let (ack, value) = SmcpComputerClient::handle_get_resources_with_ack(
            get_resources_payload("comp-1", "srv-1", None, 7),
            manager,
            "comp-1".to_string(),
        )
        .await
        .unwrap();

        assert_eq!(ack, Some(7));
        let ret: smcp::GetResourcesRet = serde_json::from_value(value).unwrap();
        // 单页透传：2 个资源，无下一页；不含 resourceTemplates（仅 resources）。
        assert_eq!(ret.resources.len(), 2);
        assert_eq!(ret.next_cursor, None);
        assert_eq!(ret.resources[0].uri.as_deref(), Some("window://app/w1"));
        assert_eq!(ret.req_id.unwrap().0, "req-1");
    }

    #[tokio::test]
    async fn test_get_resources_cursor_passthrough() {
        use crate::mcp_clients::manager::{test_support::inject, MCPServerManager};
        use crate::mcp_clients::model::make_resource;

        let manager = MCPServerManager::new();
        inject(
            &manager,
            "srv-1",
            mock(
                vec![
                    vec![make_resource("res://0", "r0", None, None)],
                    vec![make_resource("res://1", "r1", None, None)],
                ],
                false,
            ),
        )
        .await;
        let manager = Arc::new(RwLock::new(Some(manager)));

        // 首页：cursor=None → next_cursor 透传为 "1"。
        let (_, v0) = SmcpComputerClient::handle_get_resources_with_ack(
            get_resources_payload("comp-1", "srv-1", None, 1),
            manager.clone(),
            "comp-1".to_string(),
        )
        .await
        .unwrap();
        let p0: smcp::GetResourcesRet = serde_json::from_value(v0).unwrap();
        assert_eq!(p0.next_cursor.as_deref(), Some("1"));
        assert_eq!(p0.resources[0].uri.as_deref(), Some("res://0"));

        // 次页：cursor="1" 入参透传 → 末页 next_cursor=None。
        let (_, v1) = SmcpComputerClient::handle_get_resources_with_ack(
            get_resources_payload("comp-1", "srv-1", Some("1"), 2),
            manager,
            "comp-1".to_string(),
        )
        .await
        .unwrap();
        let p1: smcp::GetResourcesRet = serde_json::from_value(v1).unwrap();
        assert_eq!(p1.next_cursor, None);
        assert_eq!(p1.resources[0].uri.as_deref(), Some("res://1"));
    }

    #[tokio::test]
    async fn test_get_resources_unknown_server_4014() {
        use crate::mcp_clients::manager::MCPServerManager;

        let manager = Arc::new(RwLock::new(Some(MCPServerManager::new())));
        let (ack, value) = SmcpComputerClient::handle_get_resources_with_ack(
            get_resources_payload("comp-1", "missing", None, 3),
            manager,
            "comp-1".to_string(),
        )
        .await
        .unwrap();

        // flat ErrorPayload 4014（经 ACK 第一参回传），顶层平铺 mcp_server_name。
        assert_eq!(ack, Some(3));
        let err: smcp::ErrorPayload = serde_json::from_value(value).unwrap();
        assert_eq!(err.code, 4014);
        assert_eq!(err.mcp_server_name.as_deref(), Some("missing"));
        // 无嵌套 envelope：顶层即 code。
        assert!(err.capability.is_none());
    }

    #[tokio::test]
    async fn test_get_resources_capability_not_supported_4015() {
        use crate::mcp_clients::manager::{test_support::inject, MCPServerManager};

        let manager = MCPServerManager::new();
        inject(&manager, "srv-1", mock(vec![], true)).await;
        let manager = Arc::new(RwLock::new(Some(manager)));

        let (_, value) = SmcpComputerClient::handle_get_resources_with_ack(
            get_resources_payload("comp-1", "srv-1", None, 9),
            manager,
            "comp-1".to_string(),
        )
        .await
        .unwrap();

        // flat ErrorPayload 4015，顶层平铺 mcp_server_name + capability。
        let err: smcp::ErrorPayload = serde_json::from_value(value).unwrap();
        assert_eq!(err.code, 4015);
        assert_eq!(err.mcp_server_name.as_deref(), Some("srv-1"));
        assert_eq!(err.capability.as_deref(), Some("resources"));
    }

    #[tokio::test]
    async fn test_get_resources_computer_name_mismatch() {
        use crate::mcp_clients::manager::MCPServerManager;

        let manager = Arc::new(RwLock::new(Some(MCPServerManager::new())));
        // computer_name ≠ req.computer → ValidationError（不进入转发）。
        let result = SmcpComputerClient::handle_get_resources_with_ack(
            get_resources_payload("other-comp", "srv-1", None, 5),
            manager,
            "comp-1".to_string(),
        )
        .await;
        assert!(matches!(result, Err(ComputerError::ValidationError(_))));
    }
}
