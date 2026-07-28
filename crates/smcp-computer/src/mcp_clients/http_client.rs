/**
* 文件名: http_client
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: tokio, reqwest, serde_json
* 描述: HTTP类型的MCP客户端实现
*/
use super::base_client::BaseMCPClient;
use super::model::*;
use super::stdio_client::A2cClientHandler;
use super::{ResourceCache, SubscriptionManager};
use crate::inputs::SecretValueResolver;
use crate::oauth::{
    clear_stored_oauth_credentials, OAuthBeginRequest, OAuthCallback, OAuthCoordinator, OAuthError,
    OAuthLaunch, OAuthOptions, OAuthRequestGuard, OAuthStatus,
};
use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use rmcp::model::{
    CallToolRequest, CancelledNotificationParam, ClientRequest, PaginatedRequestParams,
    ReadResourceRequestParams, ServerResult, SubscribeRequestParams, UnsubscribeRequestParams,
};
use rmcp::service::{PeerRequestOptions, RequestHandle, RunningService, ServiceExt};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::RoleClient;
use std::sync::{Arc, RwLock as StdRwLock};
use std::time::Duration;
use tokio::sync::{Mutex, OnceCell};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

/// HTTP 客户端连接超时时间（秒）/ Connect timeout for HTTP client (seconds)
const CONNECT_TIMEOUT_SECS: u64 = 30;

/// HTTP MCP客户端 / HTTP MCP client
///
/// #106：改用 rmcp 官方 [`StreamableHttpClientTransport`] + [`RunningService`]，与 stdio 客户端共享同一
/// `A2cClientHandler` 通知接缝。方法体退化为 `peer` 委托（同 stdio），删去了手写的 JSON-RPC POST /
/// 会话管理 / SSE 解析——rmcp 传输负责 Streamable HTTP 的会话（Mcp-Session-Id）、SSE 响应流、GET 通知流与重连。
/// A2C 业务语义（auth 4006/4007 分流、VRL、tool_meta、window://、skill://）仍在 manager 层，不受影响。
pub struct HttpMCPClient {
    /// 基础客户端 / Base client
    base: BaseMCPClient<HttpServerParameters>,
    /// rmcp 运行服务（Streamable HTTP 传输）/ rmcp running service (Streamable HTTP transport)
    running_service: Arc<Mutex<Option<RunningService<RoleClient, A2cClientHandler>>>>,
    /// 订阅管理器 / Subscription manager
    subscription_manager: SubscriptionManager,
    /// 资源缓存 / Resource cache
    resource_cache: ResourceCache,
    /// 运行期变化通知上报接缝（#106，None=不转发）/ runtime change-notification seam。
    notify: StdRwLock<Option<ClientNotifyCtx>>,
    oauth_options: Option<OAuthOptions>,
    secret_resolver: Option<Arc<dyn SecretValueResolver>>,
    persist_oauth_credentials: bool,
    oauth: OnceCell<Arc<OAuthCoordinator>>,
}

impl std::fmt::Debug for HttpMCPClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpMCPClient")
            .field("url", &self.base.params.url)
            .field(
                "header_names",
                &self.base.params.headers.keys().collect::<Vec<_>>(),
            )
            .field("state", &self.base.state())
            .finish()
    }
}

impl HttpMCPClient {
    /// 创建新的HTTP客户端 / Create new HTTP client
    pub fn new(params: HttpServerParameters) -> Self {
        Self {
            base: BaseMCPClient::new(params),
            running_service: Arc::new(Mutex::new(None)),
            subscription_manager: SubscriptionManager::new(),
            resource_cache: ResourceCache::new(Duration::from_secs(60)), // 默认 60 秒 TTL
            notify: StdRwLock::new(None),
            oauth_options: None,
            secret_resolver: None,
            persist_oauth_credentials: true,
            oauth: OnceCell::new(),
        }
    }

    /// 注入运行期变化通知上报接缝（#106）/ attach the runtime change-notification seam。
    ///
    /// 由 [`client_factory`](super::utils::client_factory) 在 manager 启动客户端时调用；须在 `connect` 前设置
    /// （`connect` 据此构造 `A2cClientHandler` 传给 `.serve()`）。
    pub fn with_notify(mut self, notify: Option<ClientNotifyCtx>) -> Self {
        *self.notify.get_mut().expect("HTTP notify lock poisoned") = notify;
        self
    }

    /// Keep OAuth credentials only for this process instead of using the OS credential vault.
    pub fn with_ephemeral_oauth_credentials(mut self) -> Self {
        self.persist_oauth_credentials = false;
        self
    }

    pub(crate) fn set_notify(&self, notify: Option<ClientNotifyCtx>) {
        *self.notify.write().expect("HTTP notify lock poisoned") = notify;
    }

    /// Configure OAuth for this remote MCP client.
    pub fn with_oauth(
        mut self,
        options: OAuthOptions,
        resolver: Option<Arc<dyn SecretValueResolver>>,
    ) -> Self {
        self.oauth_options = Some(options);
        self.secret_resolver = resolver;
        self
    }

    fn build_http_headers(&self) -> Result<HeaderMap, OAuthError> {
        let mut header_map = HeaderMap::new();
        for (key, value) in &self.base.params.headers {
            if key.eq_ignore_ascii_case("authorization") && self.oauth_options.is_some() {
                return Err(OAuthError::Protocol(
                    rmcp::transport::AuthError::InternalError(
                        "OAuth and a static Authorization header cannot be configured together"
                            .to_string(),
                    ),
                ));
            }
            match (
                HeaderName::from_bytes(key.as_bytes()),
                HeaderValue::from_str(value),
            ) {
                (Ok(name), Ok(val)) => {
                    header_map.insert(name, val);
                }
                _ => warn!(header = %key, "skipping invalid HTTP header"),
            }
        }
        Ok(header_map)
    }

    fn build_http_client_with_headers(
        &self,
        header_map: HeaderMap,
    ) -> Result<reqwest::Client, OAuthError> {
        let mut builder =
            reqwest::Client::builder().timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS));
        if self.oauth_options.is_some() {
            builder = builder.redirect(reqwest::redirect::Policy::custom(|attempt| {
                let same_origin = attempt.previous().last().is_some_and(|previous| {
                    previous.scheme() == attempt.url().scheme()
                        && previous.host_str() == attempt.url().host_str()
                        && previous.port_or_known_default() == attempt.url().port_or_known_default()
                });
                if !same_origin {
                    attempt.stop()
                } else if attempt.previous().len() >= 10 {
                    attempt.error("too many same-origin redirects")
                } else {
                    attempt.follow()
                }
            }));
        }
        builder.default_headers(header_map).build().map_err(|e| {
            OAuthError::Protocol(rmcp::transport::AuthError::InternalError(e.to_string()))
        })
    }

    fn build_http_client(&self) -> Result<reqwest::Client, OAuthError> {
        self.build_http_client_with_headers(self.build_http_headers()?)
    }

    async fn oauth(&self) -> Result<&Arc<OAuthCoordinator>, OAuthError> {
        let options = self
            .oauth_options
            .clone()
            .ok_or(OAuthError::NotConfigured)?;
        self.oauth
            .get_or_try_init(|| async {
                let protected_resource_headers = self.build_http_headers()?;
                OAuthCoordinator::new(
                    &self.base.params.url,
                    options,
                    self.secret_resolver.clone(),
                    self.build_http_client_with_headers(protected_resource_headers.clone())?,
                    protected_resource_headers,
                    self.persist_oauth_credentials,
                )
                .await
                .map(Arc::new)
            })
            .await
    }

    pub(crate) async fn oauth_status(&self) -> Result<OAuthStatus, OAuthError> {
        Ok(self.oauth().await?.status().await)
    }

    pub(crate) async fn begin_oauth(
        &self,
        request: OAuthBeginRequest,
    ) -> Result<OAuthLaunch, OAuthError> {
        let Some(options) = self.oauth_options.as_ref() else {
            return Err(OAuthError::NotConfigured);
        };
        if !matches!(
            options.mode,
            crate::oauth::OAuthClientMode::AuthorizationCode { .. }
        ) {
            return Err(OAuthError::UnsupportedTransport);
        }
        self.oauth().await?.begin(request).await
    }

    pub(crate) async fn complete_oauth(&self, callback: OAuthCallback) -> Result<(), OAuthError> {
        self.oauth().await?.complete(callback).await
    }

    pub(crate) async fn clear_oauth(&self) -> Result<(), OAuthError> {
        if self.oauth_options.is_none() {
            return Err(OAuthError::NotConfigured);
        }
        if let Some(oauth) = self.oauth.get() {
            oauth.clear().await
        } else {
            clear_stored_oauth_credentials(
                &self.base.params.url,
                self.oauth_options
                    .as_ref()
                    .expect("OAuth options were checked above"),
                self.persist_oauth_credentials,
            )
            .await
        }
    }

    async fn prepare_oauth_request(&self) -> Result<Option<OAuthRequestGuard>, MCPClientError> {
        if self.oauth_options.is_none() {
            return Ok(None);
        }
        let oauth = self.oauth().await.map_err(|error| {
            MCPClientError::Other(format!("OAuth request preparation failed: {error}"))
        })?;
        let guard = oauth.prepare_request().await.map_err(|error| {
            MCPClientError::Other(format!("OAuth request preparation failed: {error}"))
        })?;
        Ok(Some(guard))
    }

    async fn observe_oauth_service_result<T>(
        &self,
        result: &Result<T, rmcp::ServiceError>,
        expected_generation: Option<u64>,
    ) {
        let Some(expected_generation) = expected_generation else {
            return;
        };
        if let Ok(oauth) = self.oauth().await {
            match result {
                Ok(_) => oauth.observe_service_success(expected_generation).await,
                Err(error) => {
                    oauth
                        .observe_service_error(error, expected_generation)
                        .await;
                }
            }
        }
    }

    async fn observe_oauth_service_error(
        &self,
        error: &rmcp::ServiceError,
        expected_generation: Option<u64>,
    ) {
        let Some(expected_generation) = expected_generation else {
            return;
        };
        if let Ok(oauth) = self.oauth().await {
            oauth
                .observe_service_error(error, expected_generation)
                .await;
        }
    }

    async fn observe_oauth_service_success(&self, expected_generation: Option<u64>) {
        let Some(expected_generation) = expected_generation else {
            return;
        };
        if let Ok(oauth) = self.oauth().await {
            oauth.observe_service_success(expected_generation).await;
        }
    }

    /// 获取 running service 的 guard，验证 service 可用（同 stdio 客户端语义）。
    /// Get running service guard, verifying service is available.
    async fn get_service(
        &self,
    ) -> Result<
        tokio::sync::MutexGuard<'_, Option<RunningService<RoleClient, A2cClientHandler>>>,
        MCPClientError,
    > {
        let guard = self.running_service.lock().await;
        if guard.is_none() {
            return Err(MCPClientError::ConnectionError(
                "Service not available".to_string(),
            ));
        }
        Ok(guard)
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

    /// 检查是否有缓存
    pub async fn has_cache(&self, uri: &str) -> bool {
        self.resource_cache.contains(uri).await
    }

    /// 获取缓存大小
    pub async fn cache_size(&self) -> usize {
        self.resource_cache.size().await
    }

    /// 清理过期缓存
    pub async fn cleanup_cache(&self) -> usize {
        self.resource_cache.cleanup_expired().await
    }

    /// 获取所有缓存的 URI 列表
    pub async fn cache_keys(&self) -> Vec<String> {
        self.resource_cache.keys().await
    }

    /// 清空所有缓存
    pub async fn clear_cache(&self) {
        self.resource_cache.clear().await
    }
}

#[async_trait]
impl MCPClientProtocol for HttpMCPClient {
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

        let config = StreamableHttpClientTransportConfig::with_uri(self.base.params.url.clone());
        let (service, oauth_request) = if self.oauth_options.is_some() {
            let oauth = self.oauth().await.map_err(|e| {
                MCPClientError::ConnectionError(format!("OAuth initialization failed: {e}"))
            })?;
            oauth.ensure_machine_authorized().await.map_err(|e| {
                MCPClientError::ConnectionError(format!("OAuth authorization failed: {e}"))
            })?;
            let oauth_request = oauth.prepare_request().await.map_err(|e| {
                MCPClientError::ConnectionError(format!("OAuth request preparation failed: {e}"))
            })?;
            let transport = StreamableHttpClientTransport::with_client(oauth.http_client(), config);
            let handler = A2cClientHandler::new(
                self.notify
                    .read()
                    .expect("HTTP notify lock poisoned")
                    .clone(),
            );
            (
                tokio::time::timeout(
                    Duration::from_secs(CONNECT_TIMEOUT_SECS),
                    handler.serve(transport),
                )
                .await,
                Some(oauth_request),
            )
        } else {
            let transport = StreamableHttpClientTransport::with_client(
                self.build_http_client().map_err(|e| {
                    MCPClientError::ConnectionError(format!("HTTP client build failed: {e}"))
                })?,
                config,
            );
            let handler = A2cClientHandler::new(
                self.notify
                    .read()
                    .expect("HTTP notify lock poisoned")
                    .clone(),
            );
            (
                tokio::time::timeout(
                    Duration::from_secs(CONNECT_TIMEOUT_SECS),
                    handler.serve(transport),
                )
                .await,
                None,
            )
        };

        let service = service.map_err(|_| {
            MCPClientError::TimeoutError(format!(
                "HTTP connect timed out after {}s",
                CONNECT_TIMEOUT_SECS
            ))
        })?;
        let oauth_generation = oauth_request.as_ref().map(OAuthRequestGuard::generation);
        drop(oauth_request);
        if let Some(oauth_generation) = oauth_generation {
            if let Ok(oauth) = self.oauth().await {
                match &service {
                    Ok(_) => oauth.observe_service_success(oauth_generation).await,
                    Err(error) => {
                        oauth
                            .observe_initialize_error(error, oauth_generation)
                            .await;
                    }
                }
            }
        }
        let service = service
            .map_err(|e| MCPClientError::ConnectionError(format!("Initialize failed: {}", e)))?;

        *self.running_service.lock().await = Some(service);
        self.base.update_state(ClientState::Connected).await;
        info!("HTTP client connected successfully");

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

        // rmcp 传输 cancel 负责优雅关闭（含 DELETE session）/ transport cancel handles graceful shutdown.
        let service = self.running_service.lock().await.take();
        if let Some(service) = service {
            match service.cancel().await {
                Ok(reason) => debug!("Service stopped with reason: {:?}", reason),
                Err(e) => error!("Error stopping service: {}", e),
            }
        }

        self.base.update_state(ClientState::Disconnected).await;
        info!("HTTP client disconnected successfully");

        Ok(())
    }

    async fn list_tools(&self) -> Result<Vec<Tool>, MCPClientError> {
        if self.base.get_state().await != ClientState::Connected {
            return Err(MCPClientError::ConnectionError("Not connected".to_string()));
        }

        let guard = self.get_service().await?;
        let service = guard.as_ref().unwrap();

        let oauth_request = self.prepare_oauth_request().await?;
        let oauth_generation = oauth_request.as_ref().map(OAuthRequestGuard::generation);
        let result = service.list_all_tools().await;
        drop(oauth_request);
        self.observe_oauth_service_result(&result, oauth_generation)
            .await;
        let tools = result
            .map_err(|e| MCPClientError::ProtocolError(format!("List tools error: {}", e)))?;

        info!("Found {} tools", tools.len());
        Ok(tools)
    }

    async fn call_tool(
        &self,
        tool_name: &str,
        params: serde_json::Value,
    ) -> Result<CallToolResult, MCPClientError> {
        if self.base.get_state().await != ClientState::Connected {
            return Err(MCPClientError::ConnectionError("Not connected".to_string()));
        }

        let guard = self.get_service().await?;
        let service = guard.as_ref().unwrap();

        let oauth_request = self.prepare_oauth_request().await?;
        let oauth_generation = oauth_request.as_ref().map(OAuthRequestGuard::generation);
        let result = service
            .call_tool(super::utils::call_tool_request_params(tool_name, params))
            .await;
        drop(oauth_request);
        self.observe_oauth_service_result(&result, oauth_generation)
            .await;
        let result = result.map_err(MCPClientError::ToolCallError)?;

        Ok(result)
    }

    /// 可取消 tool_call：与 stdio 客户端同构（低层 `send_request_with_option` 捕获 rmcp `request_id`，
    /// 再把「等待响应」与「取消信号」`select!` 竞速；取消胜出经 `request_id` best-effort 补发
    /// `notifications/cancelled`，time-box 2s）。详见 stdio 客户端同名方法。
    async fn call_tool_cancellable(
        &self,
        tool_name: &str,
        params: serde_json::Value,
        cancel: CancellationToken,
    ) -> Result<CancellableCallOutcome, MCPClientError> {
        if self.base.get_state().await != ClientState::Connected {
            return Err(MCPClientError::ConnectionError("Not connected".to_string()));
        }

        let guard = self.get_service().await?;

        let request = ClientRequest::CallToolRequest(CallToolRequest::new(
            super::utils::call_tool_request_params(tool_name, params),
        ));
        let oauth_request = self.prepare_oauth_request().await?;
        let oauth_generation = oauth_request.as_ref().map(OAuthRequestGuard::generation);
        let handle = guard
            .as_ref()
            .unwrap()
            .send_request_with_option(request, PeerRequestOptions::no_options())
            .await;
        let handle: RequestHandle<RoleClient> = match handle {
            Ok(handle) => handle,
            Err(error) => {
                drop(oauth_request);
                self.observe_oauth_service_error(&error, oauth_generation)
                    .await;
                return Err(MCPClientError::ToolCallError(error));
            }
        };
        drop(guard);
        let request_id = handle.id.clone();
        let RequestHandle { rx, peer, .. } = handle;

        tokio::select! {
            biased;
            resp = rx => {
                drop(oauth_request);
                match resp {
                    Ok(Ok(ServerResult::CallToolResult(r))) => {
                        self.observe_oauth_service_success(oauth_generation).await;
                        Ok(CancellableCallOutcome::Completed(r))
                    },
                    Ok(Ok(_)) => Err(MCPClientError::ProtocolError(
                        "Unexpected response variant for tools/call".to_string(),
                    )),
                    Ok(Err(e)) => {
                        self.observe_oauth_service_error(&e, oauth_generation).await;
                        Err(MCPClientError::ToolCallError(e))
                    },
                    Err(_) => Err(MCPClientError::ConnectionError("MCP transport closed".to_string())),
                }
            },
            _ = cancel.cancelled() => {
                let notify = peer.notify_cancelled(CancelledNotificationParam::new(
                    Some(request_id),
                    Some(smcp::tool_meta::A2C_DEFAULT_CANCEL_REASON.to_string()),
                ));
                if tokio::time::timeout(Duration::from_secs(2), notify).await.is_err() {
                    warn!("emit MCP notifications/cancelled timed out (best-effort, ignored)");
                }
                drop(oauth_request);
                Ok(CancellableCallOutcome::Cancelled)
            }
        }
    }

    async fn list_windows(&self) -> Result<Vec<Resource>, MCPClientError> {
        if self.base.get_state().await != ClientState::Connected {
            return Err(MCPClientError::ConnectionError("Not connected".to_string()));
        }

        let guard = self.get_service().await?;
        let service = guard.as_ref().unwrap();

        let oauth_request = self.prepare_oauth_request().await?;
        let oauth_generation = oauth_request.as_ref().map(OAuthRequestGuard::generation);
        let result = service.list_all_resources().await;
        drop(oauth_request);
        self.observe_oauth_service_result(&result, oauth_generation)
            .await;
        let all_resources = result
            .map_err(|e| MCPClientError::ProtocolError(format!("List resources error: {}", e)))?;

        // 过滤 window:// 资源并按 priority 降序排序（v0.2 元数据下沉，逻辑共享）
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

        let guard = self.get_service().await?;
        let service = guard.as_ref().unwrap();

        // 能力校验：未声明 `resources` → CapabilityNotSupported（上层映射 4015）；改用 rmcp `peer_info`
        // （initialize 握手结果），三传输 4015 语义统一，无需再手动缓存 capabilities（INT-04 #78 一致）。
        let supports_resources = service
            .peer_info()
            .map(|info| info.capabilities.resources.is_some())
            .unwrap_or(false);
        if !supports_resources {
            return Err(MCPClientError::CapabilityNotSupported(
                "resources".to_string(),
            ));
        }

        // 单页透传：cursor 进/出，不聚合、不过滤、不返回 resourceTemplates。
        let param = cursor.map(|c| PaginatedRequestParams::default().with_cursor(Some(c)));
        let oauth_request = self.prepare_oauth_request().await?;
        let oauth_generation = oauth_request.as_ref().map(OAuthRequestGuard::generation);
        let result = service.list_resources(param).await;
        drop(oauth_request);
        self.observe_oauth_service_result(&result, oauth_generation)
            .await;
        let result = result
            .map_err(|e| MCPClientError::ProtocolError(format!("List resources error: {}", e)))?;

        Ok((result.resources, result.next_cursor))
    }

    async fn get_window_detail(
        &self,
        resource: Resource,
    ) -> Result<ReadResourceResult, MCPClientError> {
        if self.base.get_state().await != ClientState::Connected {
            return Err(MCPClientError::ConnectionError("Not connected".to_string()));
        }

        let guard = self.get_service().await?;
        let service = guard.as_ref().unwrap();

        let oauth_request = self.prepare_oauth_request().await?;
        let oauth_generation = oauth_request.as_ref().map(OAuthRequestGuard::generation);
        let result = service
            .read_resource(ReadResourceRequestParams::new(resource.uri.clone()))
            .await;
        drop(oauth_request);
        self.observe_oauth_service_result(&result, oauth_generation)
            .await;
        let result = result
            .map_err(|e| MCPClientError::ProtocolError(format!("Read resource error: {}", e)))?;

        Ok(result)
    }

    async fn subscribe_window(&self, resource: Resource) -> Result<(), MCPClientError> {
        if self.base.get_state().await != ClientState::Connected {
            return Err(MCPClientError::ConnectionError("Not connected".to_string()));
        }

        let guard = self.get_service().await?;
        let service = guard.as_ref().unwrap();

        let oauth_request = self.prepare_oauth_request().await?;
        let oauth_generation = oauth_request.as_ref().map(OAuthRequestGuard::generation);
        let result = service
            .subscribe(SubscribeRequestParams::new(resource.uri.clone()))
            .await;
        drop(oauth_request);
        self.observe_oauth_service_result(&result, oauth_generation)
            .await;
        result.map_err(|e| {
            MCPClientError::ProtocolError(format!("Subscribe resource error: {}", e))
        })?;

        drop(guard);

        // 订阅成功后，更新本地订阅状态
        let _ = self
            .subscription_manager
            .add_subscription(resource.uri.clone())
            .await;

        // 立即获取并缓存资源数据
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

        let guard = self.get_service().await?;
        let service = guard.as_ref().unwrap();

        let oauth_request = self.prepare_oauth_request().await?;
        let oauth_generation = oauth_request.as_ref().map(OAuthRequestGuard::generation);
        let result = service
            .unsubscribe(UnsubscribeRequestParams::new(resource.uri.clone()))
            .await;
        drop(oauth_request);
        self.observe_oauth_service_result(&result, oauth_generation)
            .await;
        result.map_err(|e| {
            MCPClientError::ProtocolError(format!("Unsubscribe resource error: {}", e))
        })?;

        drop(guard);

        // 取消订阅成功后，移除本地订阅状态
        let _ = self
            .subscription_manager
            .remove_subscription(&resource.uri)
            .await;

        // 清理缓存
        self.resource_cache.remove(&resource.uri).await;
        info!("Unsubscribed and removed cache: {}", resource.uri);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    // 注（#106）：本模块原有若干测试直接验证手写 JSON-RPC 层的内部实现（`parse_sse_response` /
    // `send_request` / `session_id` / `capabilities_resources` / `initialize_session`）。该层已随 HTTP 客户端
    // 迁移到 rmcp `StreamableHttpClientTransport` 删除，相关测试一并移除；真实的连接/工具/资源行为改由真链路
    // e2e（真 Streamable HTTP server + 真 rmcp 传输）覆盖。此处保留与传输无关的状态门控/构造/Debug 单测。

    #[tokio::test]
    async fn test_http_client_creation() {
        let params = HttpServerParameters {
            url: "http://localhost:8080".to_string(),
            headers: HashMap::new(),
        };

        let client = HttpMCPClient::new(params);
        assert_eq!(client.state(), ClientState::Initialized);
        assert_eq!(client.base.params.url, "http://localhost:8080");
    }

    #[tokio::test]
    async fn test_http_client_with_headers() {
        let mut headers = HashMap::new();
        headers.insert("Authorization".to_string(), "Bearer token123".to_string());
        headers.insert("Content-Type".to_string(), "application/json".to_string());

        let params = HttpServerParameters {
            url: "http://localhost:8080".to_string(),
            headers,
        };

        let client = HttpMCPClient::new(params);
        assert_eq!(
            client.base.params.headers.get("Authorization"),
            Some(&"Bearer token123".to_string())
        );
    }

    #[tokio::test]
    async fn test_connect_state_checks() {
        let params = HttpServerParameters {
            url: "http://localhost:8080".to_string(),
            headers: HashMap::new(),
        };

        let client = HttpMCPClient::new(params);

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
        let params = HttpServerParameters {
            url: "http://localhost:8080".to_string(),
            headers: HashMap::new(),
        };

        let client = HttpMCPClient::new(params);

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
        let params = HttpServerParameters {
            url: "http://localhost:8080".to_string(),
            headers: HashMap::new(),
        };

        let client = HttpMCPClient::new(params);

        // 未连接状态下调用 list_tools 应该失败
        let result = client.list_tools().await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            MCPClientError::ConnectionError(_)
        ));
    }

    #[tokio::test]
    async fn test_call_tool_requires_connection() {
        let params = HttpServerParameters {
            url: "http://localhost:8080".to_string(),
            headers: HashMap::new(),
        };

        let client = HttpMCPClient::new(params);

        // 未连接状态下调用 call_tool 应该失败
        let result = client.call_tool("test_tool", json!({})).await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            MCPClientError::ConnectionError(_)
        ));
    }

    #[tokio::test]
    async fn test_list_windows_requires_connection() {
        let params = HttpServerParameters {
            url: "http://localhost:8080".to_string(),
            headers: HashMap::new(),
        };

        let client = HttpMCPClient::new(params);

        // 未连接状态下调用 list_windows 应该失败
        let result = client.list_windows().await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            MCPClientError::ConnectionError(_)
        ));
    }

    #[tokio::test]
    async fn test_get_window_detail_requires_connection() {
        let params = HttpServerParameters {
            url: "http://localhost:8080".to_string(),
            headers: HashMap::new(),
        };

        let client = HttpMCPClient::new(params);

        let resource = make_resource("window://123", "Test Window", None, None);

        // 未连接状态下调用 get_window_detail 应该失败
        let result = client.get_window_detail(resource).await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            MCPClientError::ConnectionError(_)
        ));
    }

    #[tokio::test]
    async fn test_disconnect_transitions_state_from_connected() {
        // #106：迁 rmcp 后无 session_id/无真实 service，disconnect 从 Connected 态 take() 到 None → 跳过
        // cancel → 置 Disconnected。验证状态机收尾正确（会话/传输清理由 rmcp service.cancel 负责）。
        let params = HttpServerParameters {
            url: "http://localhost:8080".to_string(),
            headers: HashMap::new(),
        };

        let client = HttpMCPClient::new(params);
        client.base.update_state(ClientState::Connected).await;

        let _ = client.disconnect().await;

        assert_eq!(client.base.get_state().await, ClientState::Disconnected);
    }

    #[tokio::test]
    async fn test_error_handling_in_list_tools() {
        let params = HttpServerParameters {
            url: "http://localhost:8080".to_string(),
            headers: HashMap::new(),
        };

        let client = HttpMCPClient::new(params);

        // 模拟已连接状态
        client.base.update_state(ClientState::Connected).await;

        // 尝试列出工具（会因为连接失败而返回错误）
        let result = client.list_tools().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_error_handling_in_call_tool() {
        let params = HttpServerParameters {
            url: "http://localhost:8080".to_string(),
            headers: HashMap::new(),
        };

        let client = HttpMCPClient::new(params);

        // 模拟已连接状态
        client.base.update_state(ClientState::Connected).await;

        // 尝试调用工具（会因为连接失败而返回错误）
        let result = client
            .call_tool("test_tool", json!({"param": "value"}))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_http_client_debug_format() {
        let params = HttpServerParameters {
            url: "http://localhost:8080".to_string(),
            headers: HashMap::new(),
        };

        let client = HttpMCPClient::new(params);

        // 验证 Debug trait 实现
        let debug_str = format!("{:?}", client);
        assert!(debug_str.contains("HttpMCPClient"));
    }
}
