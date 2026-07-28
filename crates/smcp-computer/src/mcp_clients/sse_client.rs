/**
* 文件名: sse_client
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: tokio, reqwest, eventsource-client, serde_json
* 描述: SSE类型的MCP客户端实现
*/
use super::base_client::BaseMCPClient;
use super::model::*;
use super::{ResourceCache, SubscriptionManager};
use async_trait::async_trait;
use es::Client as EsClient;
use eventsource_client as es;
use futures::stream::{Stream, StreamExt};
use serde_json;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, info, warn};

/// SSE MCP客户端 / SSE MCP client
pub struct SseMCPClient {
    /// 基础客户端 / Base client
    base: BaseMCPClient<SseServerParameters>,
    /// HTTP客户端 / HTTP client
    http_client: reqwest::Client,
    /// 请求发送器 / Request sender
    request_tx: Arc<Mutex<Option<mpsc::UnboundedSender<serde_json::Value>>>>,
    /// 响应接收器 / Response receiver
    response_rx: Arc<Mutex<Option<mpsc::UnboundedReceiver<serde_json::Value>>>>,
    /// 会话ID / Session ID
    session_id: Arc<Mutex<Option<String>>>,
    /// initialize 时缓存的 server `resources` 能力声明（INT-04 #78）。`None` = 未初始化/未知；
    /// 供 `list_resources_page` 的 4015 预检，使 sse 的能力语义与 stdio 一致。
    /// Cached `resources` capability from initialize; drives the 4015 pre-check in `list_resources_page`.
    capabilities_resources: Arc<Mutex<Option<bool>>>,
    /// SSE 服务器告知的 POST 端点 URL
    endpoint_url: Arc<Mutex<Option<String>>>,
    /// 订阅管理器 / Subscription manager
    subscription_manager: SubscriptionManager,
    /// 资源缓存 / Resource cache
    resource_cache: ResourceCache,
    /// 资源更新通知发送器 / Resource update notification sender
    update_tx: Arc<Mutex<Option<mpsc::UnboundedSender<ResourceUpdate>>>>,
    /// 运行期变化通知上报接缝（#106，None=不转发）/ runtime change-notification seam。
    notify: Option<ClientNotifyCtx>,
}

/// 资源更新通知
#[derive(Debug, Clone)]
pub struct ResourceUpdate {
    /// 资源 URI
    pub uri: String,
    /// 新数据
    pub data: serde_json::Value,
    /// 版本号
    pub version: u64,
}

/// 把一条 SSE 事件 JSON 分类为可转发的 MCP 变化通知（#106）。纯函数，供 SSE 流任务调用 + 单测覆盖。
///
/// - `Some(kind)`：三类标准 MCP 通知（`notifications/tools/list_changed` /
///   `notifications/resources/list_changed` / `notifications/resources/updated`）及本仓历史非标准命名
///   `resources/update` —— 应转发给 Computer 消费者任务；
/// - `None`：非通知（JSON-RPC 响应 / 带 id 的服务器请求）或未识别的通知 method —— 不转发到 A2C channel。
///
/// 修复要点：取代历史 `method.contains("update")` 松散匹配 + 把带 method 的消息误投进 response 通道
/// （破坏请求/响应关联）的做法——仅精确识别的通知才转发，且不再污染 response 通道。
pub(crate) fn classify_sse_notification(value: &serde_json::Value) -> Option<McpChangeKind> {
    let method = value.get("method")?.as_str()?;
    match method {
        "notifications/tools/list_changed" => Some(McpChangeKind::ToolListChanged),
        "notifications/resources/list_changed" => Some(McpChangeKind::ResourceListChanged),
        "notifications/resources/updated" | "resources/update" => {
            let uri = value
                .get("params")
                .and_then(|p| p.get("uri"))
                .and_then(|u| u.as_str())
                .unwrap_or_default()
                .to_string();
            Some(McpChangeKind::ResourceUpdated { uri })
        }
        _ => None,
    }
}

impl std::fmt::Debug for SseMCPClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SseMCPClient")
            .field("url", &self.base.params.url)
            .field("headers", &self.base.params.headers)
            .field("state", &self.base.state())
            .finish()
    }
}

impl SseMCPClient {
    /// 创建新的SSE客户端 / Create new SSE client
    pub fn new(params: SseServerParameters) -> Self {
        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("Failed to create HTTP client");

        Self {
            base: BaseMCPClient::new(params),
            http_client,
            request_tx: Arc::new(Mutex::new(None)),
            response_rx: Arc::new(Mutex::new(None)),
            session_id: Arc::new(Mutex::new(None)),
            capabilities_resources: Arc::new(Mutex::new(None)),
            endpoint_url: Arc::new(Mutex::new(None)),
            subscription_manager: SubscriptionManager::new(),
            resource_cache: ResourceCache::new(Duration::from_secs(60)), // 默认 60 秒 TTL
            update_tx: Arc::new(Mutex::new(None)),
            notify: None,
        }
    }

    /// 注入运行期变化通知上报接缝（#106）/ attach the runtime change-notification seam。
    ///
    /// 由 [`client_factory`](super::utils::client_factory) 在 manager 启动客户端时调用；须在 `connect` 前设置
    /// （SSE 常驻流任务据此把服务器主动通知转成 [`McpServerNotification`] 上报）。
    pub fn with_notify(mut self, notify: Option<ClientNotifyCtx>) -> Self {
        self.notify = notify;
        self
    }

    /// 发送JSON-RPC请求 / Send JSON-RPC request
    async fn send_request(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, MCPClientError> {
        let mut request_body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
        });

        if let Some(p) = params {
            request_body["params"] = p;
        }

        // 通知类消息不需要等待响应 / Notifications don't need a response
        let is_notification = method.starts_with("notifications/");

        // 仅非 notification 时添加请求ID / Only add request ID for non-notifications
        if !is_notification {
            let request_id = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64;
            request_body["id"] = serde_json::Value::Number(serde_json::Number::from(request_id));
        }

        debug!("Sending SSE request: {}", request_body);

        // 通过SSE发送请求 / Send request via SSE
        let tx = self.request_tx.lock().await;
        if let Some(ref tx) = *tx {
            tx.send(request_body.clone()).map_err(|e| {
                MCPClientError::ConnectionError(format!("Failed to send request: {}", e))
            })?;
        } else {
            return Err(MCPClientError::ConnectionError(
                "SSE connection not established".to_string(),
            ));
        }
        drop(tx);

        if is_notification {
            return Ok(serde_json::json!({}));
        }

        // 等待响应（带超时）/ Wait for response (with timeout)
        let mut rx = self.response_rx.lock().await;
        if let Some(ref mut receiver) = *rx {
            match tokio::time::timeout(Duration::from_secs(30), receiver.recv()).await {
                Ok(Some(response)) => {
                    debug!("Received SSE response: {}", response);
                    Ok(response)
                }
                Ok(None) => Err(MCPClientError::ConnectionError(
                    "Response channel closed".to_string(),
                )),
                Err(_) => Err(MCPClientError::TimeoutError(
                    "SSE response timed out after 30s".to_string(),
                )),
            }
        } else {
            Err(MCPClientError::ConnectionError(
                "Response channel not established".to_string(),
            ))
        }
    }

    /// 启动SSE连接 / Start SSE connection
    async fn start_sse_connection(&self) -> Result<(), MCPClientError> {
        let url = &self.base.params.url;

        // 直接使用原始 URL，不拼接 ?events=true
        let mut builder = es::ClientBuilder::for_url(url)
            .map_err(|e| MCPClientError::ConnectionError(format!("Invalid SSE URL: {:?}", e)))?;

        // 添加headers / Add headers
        for (key, value) in &self.base.params.headers {
            builder = builder.header(key, value).map_err(|e| {
                MCPClientError::ConnectionError(format!("Failed to add header {}: {:?}", key, e))
            })?;
        }

        // 创建通信通道 / Create communication channels
        let (request_tx, request_rx) = mpsc::unbounded_channel::<serde_json::Value>();
        let (response_tx, response_rx) = mpsc::unbounded_channel::<serde_json::Value>();

        *self.request_tx.lock().await = Some(request_tx);
        *self.response_rx.lock().await = Some(response_rx);

        // 克隆资源缓存和更新通知发送器，用于 SSE 事件处理
        let resource_cache = self.resource_cache.clone();
        let update_tx = self.update_tx.clone();
        let endpoint_url = self.endpoint_url.clone();
        let base_url = url.clone();
        let http_client = self.http_client.clone();
        let headers = self.base.params.headers.clone();
        // #106：把服务器主动通知上报给 Computer 消费者任务（None=不转发）。
        let notify = self.notify.clone();

        // 启动SSE事件处理任务 / Start SSE event handling task
        // Avoid constructing a TLS connector (and loading the platform certificate store) for
        // plain-HTTP endpoints. Besides being unnecessary work, concurrent macOS keychain reads
        // can fail with errSecIO (-36) when many local SSE clients start together.
        let stream: Pin<Box<dyn Stream<Item = Result<es::SSE, es::Error>> + Send + Sync>> =
            if url::Url::parse(url).is_ok_and(|parsed| parsed.scheme() == "http") {
                builder.build_http().stream()
            } else {
                builder.build().stream()
            };

        tokio::spawn(async move {
            let mut stream = Box::pin(stream);
            let mut request_rx = Box::pin(request_rx);

            loop {
                tokio::select! {
                    // 处理SSE事件 / Handle SSE events
                    Some(event_result) = stream.next() => {
                        match event_result {
                            Ok(event) => {
                                debug!("Received SSE event: {:?}", event);

                                match event {
                                    es::SSE::Event(event_data) => {
                                        // 处理 endpoint 事件 / Handle endpoint event
                                        if event_data.event_type == "endpoint" {
                                            let endpoint = event_data.data.trim();
                                            // 支持相对路径 / Support relative paths
                                            let resolved = resolve_endpoint_url(&base_url, endpoint);
                                            info!("SSE endpoint resolved: {}", resolved);
                                            *endpoint_url.lock().await = Some(resolved);
                                            continue;
                                        }

                                        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&event_data.data) {
                                            // 分类 MCP 服务器→客户端消息（#106）：已识别的变化通知转发 A2C channel、
                                            // 绝不投 response_tx（修历史误投通知进响应通道 → 请求/响应关联错乱）；
                                            // 未识别的 notifications/* 忽略；其余（响应 / 带 id 的服务器请求）维持路由。
                                            match classify_sse_notification(&value) {
                                                Some(kind) => {
                                                    // resources/updated：保留既有内部缓存刷新（历史 resources/update 携带 data 时）。
                                                    if let McpChangeKind::ResourceUpdated { uri } = &kind {
                                                        if !uri.is_empty() {
                                                            if let Some(data) = value.get("params").and_then(|p| p.get("data")) {
                                                                let _ = resource_cache.refresh(uri, data.clone()).await;
                                                                if let Some(tx) = update_tx.lock().await.as_ref() {
                                                                    let _ = tx.send(ResourceUpdate {
                                                                        uri: uri.clone(),
                                                                        data: data.clone(),
                                                                        version: 1,
                                                                    });
                                                                }
                                                            }
                                                        }
                                                    }
                                                    debug!("SSE MCP notification forwarded: {:?}", kind);
                                                    if let Some(n) = &notify {
                                                        n.notify(kind);
                                                    }
                                                }
                                                None => {
                                                    let is_unhandled_notification = value
                                                        .get("method")
                                                        .and_then(|m| m.as_str())
                                                        .map(|m| m.starts_with("notifications/"))
                                                        .unwrap_or(false);
                                                    if is_unhandled_notification {
                                                        debug!("SSE: ignoring unhandled MCP notification");
                                                    } else {
                                                        // JSON-RPC 响应 或 带 id 的服务器请求 → response channel（维持历史）。
                                                        let _ = response_tx.send(value);
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    es::SSE::Comment(_) => {
                                        debug!("Received SSE comment");
                                    }
                                    es::SSE::Connected(_) => {
                                        debug!("SSE connection established");
                                    }
                                }
                            }
                            Err(e) => {
                                error!("SSE event error: {:?}", e);
                                break;
                            }
                        }
                    }

                    // 处理请求发送 / Handle request sending via HTTP POST
                    Some(request) = request_rx.recv() => {
                        debug!("Sending request via HTTP POST: {}", request);

                        let post_url = match endpoint_url.lock().await.clone() {
                            Some(url) => url,
                            None => {
                                error!("No endpoint URL available for POST request");
                                continue;
                            }
                        };

                        let mut req = http_client.post(&post_url)
                            .header("Content-Type", "application/json");

                        // 添加用户配置的 headers
                        for (key, value) in &headers {
                            req = req.header(key, value);
                        }

                        match req.json(&request).send().await {
                            Ok(resp) => {
                                if resp.status().is_success() {
                                    // 检查 Content-Type，如果是 JSON 直接解析为响应
                                    let ct = resp.headers()
                                        .get("content-type")
                                        .and_then(|v| v.to_str().ok())
                                        .unwrap_or("")
                                        .to_string();

                                    if ct.contains("application/json") {
                                        match resp.json::<serde_json::Value>().await {
                                            Ok(json_resp) => {
                                                let _ = response_tx.send(json_resp);
                                            }
                                            Err(e) => {
                                                error!("Failed to parse POST JSON response: {}", e);
                                                let error_json = serde_json::json!({
                                                    "jsonrpc": "2.0",
                                                    "error": {
                                                        "code": -32603,
                                                        "message": format!("Failed to parse POST JSON response: {}", e)
                                                    }
                                                });
                                                let _ = response_tx.send(error_json);
                                            }
                                        }
                                    }
                                    // 如果是 SSE 响应，数据会通过 SSE stream 返回，无需在此处理
                                } else {
                                    let status = resp.status();
                                    error!("POST request failed with status: {}", status);
                                    let error_json = serde_json::json!({
                                        "jsonrpc": "2.0",
                                        "error": {
                                            "code": -32603,
                                            "message": format!("POST request failed with status: {}", status)
                                        }
                                    });
                                    let _ = response_tx.send(error_json);
                                }
                            }
                            Err(e) => {
                                // Log full error chain
                                let mut error_msg = format!("Failed to send POST request: {}", e);
                                {
                                    use std::error::Error as StdError;
                                    let mut source = e.source();
                                    while let Some(cause) = source {
                                        error_msg.push_str(&format!("\n  Caused by: {}", cause));
                                        source = cause.source();
                                    }
                                }
                                error!("{}", error_msg);
                                let error_json = serde_json::json!({
                                    "jsonrpc": "2.0",
                                    "error": {
                                        "code": -32603,
                                        "message": error_msg
                                    }
                                });
                                let _ = response_tx.send(error_json);
                            }
                        }
                    }
                }
            }
        });

        Ok(())
    }

    /// 初始化会话 / Initialize session
    async fn initialize_session(&self) -> Result<(), MCPClientError> {
        let params = serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "tools": {},
                "resources": {}
            },
            "clientInfo": {
                "name": "a2c-smcp-rust",
                "version": "0.1.0"
            }
        });

        let response = self.send_request("initialize", Some(params)).await?;

        // 检查响应 / Check response
        if let Some(error) = response.get("error") {
            return Err(MCPClientError::ProtocolError(format!(
                "Initialize error: {}",
                error
            )));
        }

        // 无 `result` 即无合法 InitializeResult → 视为初始化失败（与 stdio rmcp 握手"无 InitializeResult
        // 即连接失败"对齐），消除"已连接但能力未捕获 → 误判 4015"的窄路径（#78 fix-review）。
        let Some(result) = response.get("result") else {
            return Err(MCPClientError::ProtocolError(
                "Initialize response missing 'result' (no valid InitializeResult)".to_string(),
            ));
        };
        if let Some(session_id) = result.get("sessionId").and_then(|v| v.as_str()) {
            *self.session_id.lock().await = Some(session_id.to_string());
        }
        // 缓存 server 的 `resources` 能力声明，供 list_resources_page 的 4015 预检（INT-04 #78）。
        *self.capabilities_resources.lock().await =
            Some(super::utils::server_declares_resources(result));

        // 发送initialized通知 / Send initialized notification
        self.send_request("notifications/initialized", Some(serde_json::json!({})))
            .await?;

        info!("SSE session initialized successfully");
        Ok(())
    }

    /// server 是否声明 `resources` 能力（initialize 时缓存）。未知/未初始化 → `false`（默认拒绝，对齐
    /// stdio `peer_info` 预检）。Whether the server declared `resources`; unknown → false (default-deny).
    async fn supports_resources(&self) -> bool {
        self.capabilities_resources.lock().await.unwrap_or(false)
    }

    // ========== 订阅管理 API / Subscription Management API ==========

    /// 检查是否已订阅指定资源
    pub async fn is_subscribed(&self, uri: &str) -> bool {
        self.subscription_manager.is_subscribed(uri).await
    }

    /// 获取所有订阅的 URI 列表
    pub async fn get_subscriptions(&self) -> Vec<String> {
        self.subscription_manager.get_subscriptions().await
    }

    /// 获取订阅数量
    pub async fn subscription_count(&self) -> usize {
        self.subscription_manager.subscription_count().await
    }

    // ========== 资源缓存 API / Resource Cache API ==========

    /// 获取缓存的资源数据
    pub async fn get_cached_resource(&self, uri: &str) -> Option<serde_json::Value> {
        self.resource_cache.get(uri).await
    }

    /// 检查资源是否已缓存
    pub async fn has_cache(&self, uri: &str) -> bool {
        self.resource_cache.contains(uri).await
    }

    /// 获取缓存大小
    pub async fn cache_size(&self) -> usize {
        self.resource_cache.size().await
    }

    /// 清理过期的缓存
    pub async fn cleanup_cache(&self) -> usize {
        self.resource_cache.cleanup_expired().await
    }

    /// 获取所有缓存的 URI 列表
    pub async fn cache_keys(&self) -> Vec<String> {
        self.resource_cache.keys().await
    }

    // ========== 资源更新订阅 API / Resource Update Subscription API ==========

    /// 订阅资源更新通知
    ///
    /// 返回一个 receiver，可以用于接收资源更新通知
    pub async fn subscribe_to_updates(&self) -> mpsc::UnboundedReceiver<ResourceUpdate> {
        let (tx, rx) = mpsc::unbounded_channel();
        *self.update_tx.lock().await = Some(tx);
        rx
    }
}

/// 解析 endpoint URL，支持相对路径
fn resolve_endpoint_url(base_url: &str, endpoint: &str) -> String {
    // 如果是绝对 URL 直接返回
    if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
        return endpoint.to_string();
    }
    // 用 url crate 解析相对路径
    if let Ok(base) = url::Url::parse(base_url) {
        if let Ok(resolved) = base.join(endpoint) {
            return resolved.to_string();
        }
    }
    // fallback: 简单拼接
    format!("{}{}", base_url.trim_end_matches('/'), endpoint)
}

#[async_trait]
impl MCPClientProtocol for SseMCPClient {
    fn state(&self) -> ClientState {
        self.base.state()
    }

    async fn connect(&self) -> Result<(), MCPClientError> {
        // 检查是否可以连接 / Check if can connect
        if !self.base.can_connect().await {
            return Err(MCPClientError::ConnectionError(format!(
                "Cannot connect in state: {}",
                self.base.get_state().await
            )));
        }

        // 启动SSE连接 / Start SSE connection
        self.start_sse_connection().await?;

        // 等待 endpoint URL 就绪 / Wait for endpoint URL to be ready
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if self.endpoint_url.lock().await.is_some() {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(MCPClientError::TimeoutError(
                    "Timed out waiting for SSE endpoint event".to_string(),
                ));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        // 初始化会话 / Initialize session
        self.initialize_session().await?;

        // 更新状态 / Update state
        self.base.update_state(ClientState::Connected).await;
        info!("SSE client connected successfully");

        Ok(())
    }

    async fn disconnect(&self) -> Result<(), MCPClientError> {
        // 检查是否可以断开 / Check if can disconnect
        if !self.base.can_disconnect().await {
            return Err(MCPClientError::ConnectionError(format!(
                "Cannot disconnect in state: {}",
                self.base.get_state().await
            )));
        }

        // 尝试优雅关闭 / Try graceful shutdown
        if let Err(e) = self.send_request("shutdown", None).await {
            warn!("Failed to send shutdown request: {}", e);
        }

        // 发送exit通知 / Send exit notification
        if let Err(e) = self.send_request("exit", None).await {
            warn!("Failed to send exit notification: {}", e);
        }

        // 关闭SSE连接 / Close SSE connection
        *self.request_tx.lock().await = None;

        // 清理会话ID / Clear session ID
        *self.session_id.lock().await = None;

        // 清理 endpoint URL
        *self.endpoint_url.lock().await = None;

        // 更新状态 / Update state
        self.base.update_state(ClientState::Disconnected).await;
        info!("SSE client disconnected successfully");

        Ok(())
    }

    async fn list_tools(&self) -> Result<Vec<Tool>, MCPClientError> {
        if self.base.get_state().await != ClientState::Connected {
            return Err(MCPClientError::ConnectionError("Not connected".to_string()));
        }

        let response = self
            .send_request("tools/list", Some(serde_json::json!({})))
            .await?;

        if let Some(error) = response.get("error") {
            return Err(MCPClientError::ProtocolError(format!(
                "List tools error: {}",
                error
            )));
        }

        if let Some(result) = response.get("result") {
            if let Some(tools) = result.get("tools").and_then(|v| v.as_array()) {
                let mut tool_list = Vec::new();
                for tool in tools {
                    if let Ok(parsed_tool) = serde_json::from_value::<Tool>(tool.clone()) {
                        tool_list.push(parsed_tool);
                    }
                }
                return Ok(tool_list);
            }
        }

        Ok(vec![])
    }

    async fn call_tool(
        &self,
        tool_name: &str,
        params: serde_json::Value,
    ) -> Result<CallToolResult, MCPClientError> {
        if self.base.get_state().await != ClientState::Connected {
            return Err(MCPClientError::ConnectionError("Not connected".to_string()));
        }

        let call_params = serde_json::json!({
            "name": tool_name,
            "arguments": params
        });

        let response = self.send_request("tools/call", Some(call_params)).await?;

        if let Some(error) = response.get("error") {
            return Err(MCPClientError::ProtocolError(format!(
                "Call tool error: {}",
                error
            )));
        }

        if let Some(result) = response.get("result") {
            let call_result: CallToolResult = serde_json::from_value(result.clone())?;
            return Ok(call_result);
        }

        Err(MCPClientError::ProtocolError(
            "Invalid response".to_string(),
        ))
    }

    async fn list_windows(&self) -> Result<Vec<Resource>, MCPClientError> {
        if self.base.get_state().await != ClientState::Connected {
            return Err(MCPClientError::ConnectionError("Not connected".to_string()));
        }

        // SSE 客户端目前不支持分页，直接获取所有资源
        let response = self
            .send_request("resources/list", Some(serde_json::json!({})))
            .await?;

        if let Some(error) = response.get("error") {
            return Err(MCPClientError::ProtocolError(format!(
                "List resources error: {}",
                error
            )));
        }

        let mut all_resources = Vec::new();
        if let Some(result) = response.get("result") {
            if let Some(resources) = result.get("resources").and_then(|v| v.as_array()) {
                for resource in resources {
                    if let Ok(parsed_resource) =
                        serde_json::from_value::<Resource>(resource.clone())
                    {
                        all_resources.push(parsed_resource);
                    }
                }
            }
        }

        // 过滤 window:// 资源并按 priority 降序排序（v0.2 元数据下沉，逻辑共享）
        // Filter window:// resources and sort by priority desc (shared v0.2 metadata sink).
        Ok(crate::desktop::metadata::filter_and_sort_window_resources(
            all_resources,
        ))
    }

    async fn list_resources_page(
        &self,
        cursor: Option<String>,
    ) -> Result<(Vec<Resource>, Option<String>), MCPClientError> {
        if self.base.get_state().await != ClientState::Connected {
            return Err(MCPClientError::ConnectionError("Not connected".to_string()));
        }

        // 4015 能力预检：server 未声明 `resources` → CapabilityNotSupported（上层映射 4015），与 stdio 一致。
        // INT-04 #78：initialize 时缓存 `capabilities.resources`，三传输统一 4015 语义，不再随传输退化为
        // ProtocolError。Capability gate mirroring stdio: undeclared `resources` ⇒ 4015 (not ProtocolError).
        if !self.supports_resources().await {
            return Err(MCPClientError::CapabilityNotSupported(
                "resources".to_string(),
            ));
        }

        // 单页透传：cursor 进/出，不聚合、不过滤、不返回 resourceTemplates。
        // Single-page passthrough (cursor in/out, no aggregation/filter/resourceTemplates).
        let params = Some(match cursor.as_ref() {
            Some(c) => serde_json::json!({ "cursor": c }),
            None => serde_json::json!({}),
        });
        let response = self.send_request("resources/list", params).await?;
        if let Some(error) = response.get("error") {
            return Err(MCPClientError::ProtocolError(format!(
                "List resources error: {}",
                error
            )));
        }

        let mut resources = Vec::new();
        let mut next_cursor = None;
        if let Some(result) = response.get("result") {
            if let Some(arr) = result.get("resources").and_then(|v| v.as_array()) {
                for resource in arr {
                    if let Ok(parsed) = serde_json::from_value::<Resource>(resource.clone()) {
                        resources.push(parsed);
                    }
                }
            }
            next_cursor = result
                .get("nextCursor")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
        }
        Ok((resources, next_cursor))
    }

    async fn get_window_detail(
        &self,
        resource: Resource,
    ) -> Result<ReadResourceResult, MCPClientError> {
        if self.base.get_state().await != ClientState::Connected {
            return Err(MCPClientError::ConnectionError("Not connected".to_string()));
        }

        let params = serde_json::json!({
            "uri": resource.uri
        });

        let response = self.send_request("resources/read", Some(params)).await?;

        if let Some(error) = response.get("error") {
            return Err(MCPClientError::ProtocolError(format!(
                "Read resource error: {}",
                error
            )));
        }

        if let Some(result) = response.get("result") {
            let read_result: ReadResourceResult = serde_json::from_value(result.clone())?;
            return Ok(read_result);
        }

        Err(MCPClientError::ProtocolError(
            "Invalid response".to_string(),
        ))
    }

    async fn subscribe_window(&self, resource: Resource) -> Result<(), MCPClientError> {
        if self.base.get_state().await != ClientState::Connected {
            return Err(MCPClientError::ConnectionError("Not connected".to_string()));
        }

        let params = serde_json::json!({
            "uri": resource.uri
        });

        let response = self
            .send_request("resources/subscribe", Some(params))
            .await?;

        if let Some(error) = response.get("error") {
            return Err(MCPClientError::ProtocolError(format!(
                "Subscribe resource error: {}",
                error
            )));
        }

        let _ = self
            .subscription_manager
            .add_subscription(resource.uri.clone())
            .await;

        match self.get_window_detail(resource.clone()).await {
            Ok(result) => {
                if !result.contents.is_empty() {
                    if let Ok(json_value) = serde_json::to_value(&result.contents[0]) {
                        self.resource_cache
                            .set(resource.uri.clone(), json_value, None)
                            .await;
                        info!("Subscribed and cached: {}", resource.uri);
                    }
                }
            }
            Err(e) => {
                warn!("Failed to fetch resource data after subscription: {:?}", e);
            }
        }

        Ok(())
    }

    async fn unsubscribe_window(&self, resource: Resource) -> Result<(), MCPClientError> {
        if self.base.get_state().await != ClientState::Connected {
            return Err(MCPClientError::ConnectionError("Not connected".to_string()));
        }

        let params = serde_json::json!({
            "uri": resource.uri
        });

        let response = self
            .send_request("resources/unsubscribe", Some(params))
            .await?;

        if let Some(error) = response.get("error") {
            return Err(MCPClientError::ProtocolError(format!(
                "Unsubscribe resource error: {}",
                error
            )));
        }

        let _ = self
            .subscription_manager
            .remove_subscription(&resource.uri)
            .await;

        self.resource_cache.remove(&resource.uri).await;
        info!("Unsubscribed and removed cache: {}", resource.uri);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── #106：SSE 通知分类器单测（先红后绿：修复前 method.contains("update") 松散匹配 + 误投 response）──

    #[test]
    fn classify_tools_list_changed() {
        let v = json!({ "jsonrpc": "2.0", "method": "notifications/tools/list_changed" });
        assert_eq!(
            classify_sse_notification(&v),
            Some(McpChangeKind::ToolListChanged)
        );
    }

    #[test]
    fn classify_resources_list_changed() {
        let v = json!({ "jsonrpc": "2.0", "method": "notifications/resources/list_changed" });
        assert_eq!(
            classify_sse_notification(&v),
            Some(McpChangeKind::ResourceListChanged)
        );
    }

    #[test]
    fn classify_resources_updated_carries_uri() {
        let v = json!({
            "jsonrpc": "2.0",
            "method": "notifications/resources/updated",
            "params": { "uri": "window://desk/1" }
        });
        assert_eq!(
            classify_sse_notification(&v),
            Some(McpChangeKind::ResourceUpdated {
                uri: "window://desk/1".to_string()
            })
        );
    }

    #[test]
    fn classify_legacy_resources_update_still_supported() {
        // 历史非标准命名 `resources/update` 保留兼容（映射为 ResourceUpdated）。
        let v = json!({ "method": "resources/update", "params": { "uri": "skill://s/1" } });
        assert_eq!(
            classify_sse_notification(&v),
            Some(McpChangeKind::ResourceUpdated {
                uri: "skill://s/1".to_string()
            })
        );
    }

    #[test]
    fn classify_response_and_server_request_are_none() {
        // JSON-RPC 响应（无 method）→ None（走 response 通道，不转发）。
        assert_eq!(
            classify_sse_notification(&json!({ "jsonrpc": "2.0", "id": 1, "result": {} })),
            None
        );
        // 带 id 的服务器请求（如 sampling）→ None（不误判为变化通知）。
        assert_eq!(
            classify_sse_notification(
                &json!({ "jsonrpc": "2.0", "id": 2, "method": "sampling/createMessage" })
            ),
            None
        );
        // 未识别的 notifications/*（如 progress）→ None（不转发、由调用方忽略）。
        assert_eq!(
            classify_sse_notification(&json!({ "method": "notifications/progress" })),
            None
        );
        // 历史松散匹配的陷阱：含 "update" 但非标准的其它 method 不再被误判为资源更新。
        assert_eq!(
            classify_sse_notification(&json!({ "method": "some/other_update_thing" })),
            None
        );
    }
    use std::collections::HashMap;

    #[test]
    fn test_resolve_endpoint_url_absolute() {
        let result = resolve_endpoint_url(
            "http://localhost:8081/sse",
            "https://other.example.com/messages",
        );
        assert_eq!(result, "https://other.example.com/messages");
    }

    #[test]
    fn test_resolve_endpoint_url_relative() {
        let result = resolve_endpoint_url("http://localhost:8081/sse", "/messages");
        assert_eq!(result, "http://localhost:8081/messages");
    }

    #[test]
    fn test_resolve_endpoint_url_relative_path() {
        let result = resolve_endpoint_url("http://localhost:8081/api/sse", "messages");
        assert_eq!(result, "http://localhost:8081/api/messages");
    }

    #[tokio::test]
    async fn test_sse_client_creation() {
        let params = SseServerParameters {
            url: "http://localhost:8081".to_string(),
            headers: HashMap::new(),
        };

        let client = SseMCPClient::new(params);
        assert_eq!(client.state(), ClientState::Initialized);
        assert_eq!(client.base.params.url, "http://localhost:8081");
    }

    #[tokio::test]
    async fn test_sse_client_with_headers() {
        let mut headers = HashMap::new();
        headers.insert("Authorization".to_string(), "Bearer token123".to_string());
        headers.insert("Accept".to_string(), "text/event-stream".to_string());

        let params = SseServerParameters {
            url: "http://localhost:8081".to_string(),
            headers,
        };

        let client = SseMCPClient::new(params);
        assert_eq!(
            client.base.params.headers.get("Authorization"),
            Some(&"Bearer token123".to_string())
        );
    }

    #[tokio::test]
    async fn test_session_id_management() {
        let params = SseServerParameters {
            url: "http://localhost:8081".to_string(),
            headers: HashMap::new(),
        };

        let client = SseMCPClient::new(params);

        // 初始会话ID应该为空
        let session_id = client.session_id.lock().await;
        assert!(session_id.is_none());
        drop(session_id);

        // 设置会话ID
        *client.session_id.lock().await = Some("session123".to_string());
        let session_id = client.session_id.lock().await;
        assert_eq!(session_id.as_ref().unwrap(), "session123");
    }

    #[tokio::test]
    async fn test_endpoint_url_management() {
        let params = SseServerParameters {
            url: "http://localhost:8081".to_string(),
            headers: HashMap::new(),
        };

        let client = SseMCPClient::new(params);

        // 初始 endpoint URL 应该为空
        assert!(client.endpoint_url.lock().await.is_none());

        // 设置 endpoint URL
        *client.endpoint_url.lock().await = Some("http://localhost:8081/messages".to_string());
        assert_eq!(
            client.endpoint_url.lock().await.as_ref().unwrap(),
            "http://localhost:8081/messages"
        );
    }

    #[tokio::test]
    async fn test_send_request_without_connection() {
        let params = SseServerParameters {
            url: "http://localhost:8081".to_string(),
            headers: HashMap::new(),
        };

        let client = SseMCPClient::new(params);

        let method = "test/method";
        let params = Some(json!({"param1": "value1"}));

        let result = client.send_request(method, params).await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            MCPClientError::ConnectionError(_)
        ));
    }

    #[tokio::test]
    async fn test_connect_state_checks() {
        let params = SseServerParameters {
            url: "http://localhost:8081".to_string(),
            headers: HashMap::new(),
        };

        let client = SseMCPClient::new(params);

        // 在已连接状态下尝试连接应该失败
        client.base.update_state(ClientState::Connected).await;
        let result = client.connect().await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            MCPClientError::ConnectionError(_)
        ));
    }

    #[tokio::test]
    async fn test_disconnect_state_checks() {
        let params = SseServerParameters {
            url: "http://localhost:8081".to_string(),
            headers: HashMap::new(),
        };

        let client = SseMCPClient::new(params);

        // 在未连接状态下尝试断开应该失败
        let result = client.disconnect().await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            MCPClientError::ConnectionError(_)
        ));
    }

    #[tokio::test]
    async fn test_list_tools_requires_connection() {
        let params = SseServerParameters {
            url: "http://localhost:8081".to_string(),
            headers: HashMap::new(),
        };

        let client = SseMCPClient::new(params);

        let result = client.list_tools().await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            MCPClientError::ConnectionError(_)
        ));
    }

    #[tokio::test]
    async fn test_call_tool_requires_connection() {
        let params = SseServerParameters {
            url: "http://localhost:8081".to_string(),
            headers: HashMap::new(),
        };

        let client = SseMCPClient::new(params);

        let result = client.call_tool("test_tool", json!({})).await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            MCPClientError::ConnectionError(_)
        ));
    }

    #[tokio::test]
    async fn test_list_windows_requires_connection() {
        let params = SseServerParameters {
            url: "http://localhost:8081".to_string(),
            headers: HashMap::new(),
        };

        let client = SseMCPClient::new(params);

        let result = client.list_windows().await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            MCPClientError::ConnectionError(_)
        ));
    }

    #[tokio::test]
    async fn test_list_resources_page_4015_when_capability_absent() {
        // INT-04 #78：连接后若 server 未声明 `resources` 能力 → 在发请求前返回 CapabilityNotSupported
        // （上层映射 4015），与 stdio 一致，不再退化为 ProtocolError。门控先于网络发送。
        let params = SseServerParameters {
            url: "http://localhost:8081".to_string(),
            headers: HashMap::new(),
        };
        let client = SseMCPClient::new(params);
        client.base.update_state(ClientState::Connected).await;
        *client.capabilities_resources.lock().await = Some(false);

        let err = client.list_resources_page(None).await.unwrap_err();
        assert!(
            matches!(err, MCPClientError::CapabilityNotSupported(ref c) if c == "resources"),
            "expected CapabilityNotSupported(resources), got {err:?}"
        );
    }

    #[tokio::test]
    async fn test_list_resources_page_passes_gate_when_capability_present() {
        // 声明了 `resources` → 预检通过；后续因无真实 SSE 通道而失败于 ConnectionError（非 4015），
        // 证明能力门控不误伤支持 resources 的 server。
        let params = SseServerParameters {
            url: "http://localhost:8081".to_string(),
            headers: HashMap::new(),
        };
        let client = SseMCPClient::new(params);
        client.base.update_state(ClientState::Connected).await;
        *client.capabilities_resources.lock().await = Some(true);

        let err = client.list_resources_page(None).await.unwrap_err();
        assert!(
            matches!(err, MCPClientError::ConnectionError(_)),
            "gate should pass for declared resources; got {err:?}"
        );
    }

    #[tokio::test]
    async fn test_get_window_detail_requires_connection() {
        let params = SseServerParameters {
            url: "http://localhost:8081".to_string(),
            headers: HashMap::new(),
        };

        let client = SseMCPClient::new(params);

        let resource = make_resource("window://123", "Test Window", None, None);

        let result = client.get_window_detail(resource).await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            MCPClientError::ConnectionError(_)
        ));
    }

    #[tokio::test]
    async fn test_start_sse_connection_url_formatting() {
        let params = SseServerParameters {
            url: "http://localhost:8081".to_string(),
            headers: HashMap::new(),
        };

        let client = SseMCPClient::new(params);

        let result = client.start_sse_connection().await;
        assert!(result.is_ok());

        // 验证通道已创建
        let request_tx = client.request_tx.lock().await;
        assert!(request_tx.is_some());

        let response_rx = client.response_rx.lock().await;
        assert!(response_rx.is_some());
    }

    #[tokio::test]
    async fn test_start_sse_connection_url_formatting_with_query() {
        let params = SseServerParameters {
            url: "http://localhost:8081?param=value".to_string(),
            headers: HashMap::new(),
        };

        let client = SseMCPClient::new(params);

        let result = client.start_sse_connection().await;
        assert!(result.is_ok());

        let request_tx = client.request_tx.lock().await;
        assert!(request_tx.is_some());

        let response_rx = client.response_rx.lock().await;
        assert!(response_rx.is_some());
    }

    #[tokio::test]
    async fn test_disconnect_cleanup() {
        let params = SseServerParameters {
            url: "http://localhost:8081".to_string(),
            headers: HashMap::new(),
        };

        let client = SseMCPClient::new(params);

        // 设置会话ID 和 endpoint URL
        *client.session_id.lock().await = Some("session123".to_string());
        *client.endpoint_url.lock().await = Some("http://localhost:8081/messages".to_string());

        // 设置为已连接状态
        client.base.update_state(ClientState::Connected).await;

        let _ = client.disconnect().await;

        // 验证清理
        assert!(client.session_id.lock().await.is_none());
        assert!(client.endpoint_url.lock().await.is_none());
        assert_eq!(client.base.get_state().await, ClientState::Disconnected);
    }

    #[tokio::test]
    async fn test_request_response_channels() {
        let params = SseServerParameters {
            url: "http://localhost:8081".to_string(),
            headers: HashMap::new(),
        };

        let client = SseMCPClient::new(params);

        let request_tx = client.request_tx.lock().await;
        assert!(request_tx.is_none());
        drop(request_tx);

        let response_rx = client.response_rx.lock().await;
        assert!(response_rx.is_none());
    }

    #[tokio::test]
    async fn test_initialize_session_request_format() {
        let params = SseServerParameters {
            url: "http://localhost:8081".to_string(),
            headers: HashMap::new(),
        };

        let client = SseMCPClient::new(params);

        let result = client.initialize_session().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_error_handling_in_list_tools() {
        let params = SseServerParameters {
            url: "http://localhost:8081".to_string(),
            headers: HashMap::new(),
        };

        let client = SseMCPClient::new(params);

        client.base.update_state(ClientState::Connected).await;

        let result = client.list_tools().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_error_handling_in_call_tool() {
        let params = SseServerParameters {
            url: "http://localhost:8081".to_string(),
            headers: HashMap::new(),
        };

        let client = SseMCPClient::new(params);

        client.base.update_state(ClientState::Connected).await;

        let result = client
            .call_tool("test_tool", json!({"param": "value"}))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_sse_client_debug_format() {
        let params = SseServerParameters {
            url: "http://localhost:8081".to_string(),
            headers: HashMap::new(),
        };

        let client = SseMCPClient::new(params);

        let debug_str = format!("{:?}", client);
        assert!(debug_str.contains("SseMCPClient"));
    }
}
