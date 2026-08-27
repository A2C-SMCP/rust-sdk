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
use super::bundle_id::BundleId;
use super::model::*;
use super::stdio_client::A2cClientHandler;
use super::{ResourceCache, SubscriptionManager};
use crate::oauth::{
    clear_stored_oauth_credentials, locally_stored_oauth_status, InMemoryOAuthCredentialStore,
    OAuthBeginRequest, OAuthCoordinator, OAuthCoordinatorContext, OAuthCredentialStore, OAuthError,
    OAuthFlow, OAuthFlowOutcome, OAuthOptions, OAuthProtocolError, OAuthRequestGuard, OAuthStatus,
};
use crate::status::RuntimeStatus;
use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use rmcp::model::{
    CallToolRequest, CancelledNotificationParam, ClientRequest, ListResourcesRequest,
    ListResourcesResult, ListToolsRequest, ListToolsResult, PaginatedRequestParams,
    ReadResourceRequest, ReadResourceRequestParams, ServerResult, SubscribeRequest,
    SubscribeRequestParams, UnsubscribeRequest, UnsubscribeRequestParams,
};
use rmcp::service::{PeerRequestOptions, RequestHandle, RunningService, ServiceExt};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{streamable_http_client::StreamableHttpError, StreamableHttpClientTransport};
use rmcp::RoleClient;
use std::sync::{Arc, Mutex as StdMutex, RwLock as StdRwLock};
use std::time::Duration;
use tokio::sync::{Mutex, OnceCell};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

/// HTTP 客户端连接超时时间（秒）/ Connect timeout for HTTP client (seconds)
const CONNECT_TIMEOUT_SECS: u64 = 30;

#[derive(Debug, Clone, PartialEq, Eq)]
enum ChallengeAdmission {
    BearerWithMetadata(String),
    BearerWithoutMetadata,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AnonymousInitializeDisposition {
    RetryWithOAuth,
    Fail(HttpAuthenticationError),
    OAuthInitializationFailed(OAuthError),
}

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
    oauth_options: OAuthOptions,
    oauth_bundle_id: BundleId,
    oauth_credential_store: Arc<dyn OAuthCredentialStore>,
    oauth_events: Option<Arc<RuntimeStatus>>,
    oauth: OnceCell<Arc<OAuthCoordinator>>,
    oauth_flow: StdMutex<Option<OAuthFlow>>,
    #[cfg(test)]
    test_root_certificates: Vec<reqwest::Certificate>,
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
            oauth_options: OAuthOptions::default(),
            oauth_bundle_id: BundleId::try_from("standalone-http-oauth")
                .expect("static standalone OAuth bundle ID must be valid"),
            oauth_credential_store: Arc::new(InMemoryOAuthCredentialStore::default()),
            oauth_events: None,
            oauth: OnceCell::new(),
            oauth_flow: StdMutex::new(None),
            #[cfg(test)]
            test_root_certificates: Vec::new(),
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

    /// Reset this standalone client to a private process-memory OAuth credential store.
    ///
    /// OAuth credentials are already in memory by default. This method remains as an explicit
    /// isolation convenience for callers that otherwise inject a shared store.
    pub fn with_ephemeral_oauth_credentials(mut self) -> Self {
        self.oauth_credential_store = Arc::new(InMemoryOAuthCredentialStore::default());
        self
    }

    /// Attach the manager-resolved bundle identity and host credential store.
    pub(crate) fn with_oauth_context(
        mut self,
        bundle_id: BundleId,
        credential_store: Arc<dyn OAuthCredentialStore>,
        events: Option<Arc<RuntimeStatus>>,
    ) -> Self {
        self.oauth_bundle_id = bundle_id;
        self.oauth_credential_store = credential_store;
        self.oauth_events = events;
        self
    }

    pub(crate) fn set_notify(&self, notify: Option<ClientNotifyCtx>) {
        *self.notify.write().expect("HTTP notify lock poisoned") = notify;
    }

    #[cfg(test)]
    pub(crate) fn with_test_root_certificates(
        mut self,
        certificates: Vec<reqwest::Certificate>,
    ) -> Self {
        self.test_root_certificates = certificates;
        self
    }

    pub(crate) fn oauth_callback_configured(&self) -> bool {
        self.oauth.get().is_some()
    }

    fn has_static_authorization(&self) -> bool {
        self.base
            .params
            .headers
            .keys()
            .any(|key| key.eq_ignore_ascii_case("authorization"))
    }

    fn build_http_headers(&self) -> Result<HeaderMap, OAuthError> {
        let mut header_map = HeaderMap::new();
        for (key, value) in &self.base.params.headers {
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
        #[cfg(test)]
        if !self.test_root_certificates.is_empty() {
            builder = builder.tls_certs_only(self.test_root_certificates.clone());
        }
        builder
            .default_headers(header_map)
            .build()
            .map_err(|_| OAuthError::Protocol(crate::oauth::OAuthProtocolError::Internal))
    }

    fn build_http_client(&self) -> Result<reqwest::Client, OAuthError> {
        self.build_http_client_with_headers(self.build_http_headers()?)
    }

    async fn initialize_oauth(
        &self,
        admitted_resource_metadata_url: Option<url::Url>,
    ) -> Result<&Arc<OAuthCoordinator>, OAuthError> {
        let options = self.oauth_options.clone();
        self.oauth
            .get_or_try_init(|| async {
                let protected_resource_headers = self.build_http_headers()?;
                let resource = options.effective_resource(&self.base.params.url)?;
                let context = OAuthCoordinatorContext::new(
                    self.oauth_bundle_id.clone(),
                    Arc::clone(&self.oauth_credential_store),
                    self.oauth_events.clone(),
                )
                .with_admitted_resource_metadata_url(admitted_resource_metadata_url);
                #[cfg(test)]
                let context =
                    context.with_test_root_certificates(self.test_root_certificates.clone());
                OAuthCoordinator::new(
                    context,
                    &self.base.params.url,
                    &resource,
                    options,
                    None,
                    self.build_http_client_with_headers(protected_resource_headers.clone())?,
                    protected_resource_headers,
                )
                .await
                .map(Arc::new)
            })
            .await
    }

    async fn oauth(&self) -> Result<&Arc<OAuthCoordinator>, OAuthError> {
        self.oauth.get().ok_or(OAuthError::NotConfigured)
    }

    fn classify_challenge(header: &str) -> ChallengeAdmission {
        let Ok(challenges) = http_auth::parse_challenges(header) else {
            return ChallengeAdmission::Unsupported;
        };
        let mut saw_bearer = false;
        for challenge in challenges {
            if !challenge.scheme.eq_ignore_ascii_case("bearer") {
                continue;
            }
            saw_bearer = true;
            if challenge.params.iter().any(|(name, value)| {
                name.eq_ignore_ascii_case("resource_metadata")
                    && !value.to_unescaped().trim().is_empty()
            }) {
                let resource_metadata = challenge
                    .params
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case("resource_metadata"))
                    .map(|(_, value)| value.to_unescaped())
                    .expect("resource_metadata was checked above");
                return ChallengeAdmission::BearerWithMetadata(resource_metadata);
            }
        }
        if saw_bearer {
            ChallengeAdmission::BearerWithoutMetadata
        } else {
            ChallengeAdmission::Unsupported
        }
    }

    fn admitted_resource_metadata_url(&self, value: &str) -> Option<url::Url> {
        let endpoint = url::Url::parse(&self.base.params.url).ok()?;
        let metadata = url::Url::parse(value)
            .or_else(|_| endpoint.join(value))
            .ok()?;
        let same_origin = endpoint.scheme() == metadata.scheme()
            && endpoint.host_str() == metadata.host_str()
            && endpoint.port_or_known_default() == metadata.port_or_known_default();
        if !same_origin || metadata.fragment().is_some() {
            return None;
        }

        Some(metadata)
    }

    fn initialize_streamable_error(
        error: &rmcp::service::ClientInitializeError,
    ) -> Option<&StreamableHttpError<reqwest::Error>> {
        let rmcp::service::ClientInitializeError::TransportError { error, .. } = error else {
            return None;
        };
        error
            .error
            .downcast_ref::<StreamableHttpError<reqwest::Error>>()
    }

    fn oauth_discovery_failed(error: &OAuthError) -> bool {
        matches!(
            error,
            OAuthError::Protocol(
                OAuthProtocolError::Http
                    | OAuthProtocolError::Provider
                    | OAuthProtocolError::Metadata
                    | OAuthProtocolError::PkceUnsupported
                    | OAuthProtocolError::InvalidUrl
                    | OAuthProtocolError::NoAuthorizationSupport
                    | OAuthProtocolError::IssuerMismatch
            )
        )
    }

    fn initialize_requires_user_authorization(
        error: &rmcp::service::ClientInitializeError,
    ) -> bool {
        matches!(
            Self::initialize_streamable_error(error),
            Some(
                StreamableHttpError::AuthRequired(_)
                    | StreamableHttpError::Auth(
                        rmcp::transport::auth::AuthError::AuthorizationRequired
                            | rmcp::transport::auth::AuthError::TokenRefreshFailed(_)
                            | rmcp::transport::auth::AuthError::TokenExpired
                    )
            )
        )
    }

    async fn classify_anonymous_initialize_error(
        &self,
        error: &rmcp::service::ClientInitializeError,
    ) -> Option<AnonymousInitializeDisposition> {
        let streamable = Self::initialize_streamable_error(error)?;
        if self.has_static_authorization() {
            return match streamable {
                StreamableHttpError::AuthRequired(_)
                | StreamableHttpError::InsufficientScope(_) => {
                    Some(AnonymousInitializeDisposition::Fail(
                        HttpAuthenticationError::StaticCredentialsRejected,
                    ))
                }
                StreamableHttpError::UnexpectedServerResponse(message)
                    if message.starts_with("HTTP 401 ") || message.starts_with("HTTP 403 ") =>
                {
                    Some(AnonymousInitializeDisposition::Fail(
                        HttpAuthenticationError::StaticCredentialsRejected,
                    ))
                }
                _ => None,
            };
        }
        match streamable {
            StreamableHttpError::AuthRequired(required) => {
                match Self::classify_challenge(&required.www_authenticate_header) {
                    ChallengeAdmission::BearerWithMetadata(resource_metadata) => {
                        let Some(metadata_url) =
                            self.admitted_resource_metadata_url(&resource_metadata)
                        else {
                            return Some(AnonymousInitializeDisposition::Fail(
                                HttpAuthenticationError::OAuthDiscoveryFailed,
                            ));
                        };
                        match self.initialize_oauth(Some(metadata_url)).await {
                            Ok(_) => Some(AnonymousInitializeDisposition::RetryWithOAuth),
                            Err(error) if Self::oauth_discovery_failed(&error) => {
                                Some(AnonymousInitializeDisposition::Fail(
                                    HttpAuthenticationError::OAuthDiscoveryFailed,
                                ))
                            }
                            Err(error) => Some(
                                AnonymousInitializeDisposition::OAuthInitializationFailed(error),
                            ),
                        }
                    }
                    ChallengeAdmission::BearerWithoutMetadata => {
                        // MCP 2026-07-28: a challenge without `resource_metadata` MUST fall back
                        // to well-known discovery. Admit without a protected resource metadata
                        // URL and let rmcp's discovery chain (PRM well-known → RFC 8414/OIDC at
                        // the endpoint origin) run; the coordinator rejects metadata that only
                        // the legacy hardcoded-endpoint fallback could produce (no issuer).
                        match self.initialize_oauth(None).await {
                            Ok(_) => Some(AnonymousInitializeDisposition::RetryWithOAuth),
                            Err(error) if Self::oauth_discovery_failed(&error) => {
                                Some(AnonymousInitializeDisposition::Fail(
                                    HttpAuthenticationError::OAuthDiscoveryFailed,
                                ))
                            }
                            Err(error) => Some(
                                AnonymousInitializeDisposition::OAuthInitializationFailed(error),
                            ),
                        }
                    }
                    ChallengeAdmission::Unsupported => Some(AnonymousInitializeDisposition::Fail(
                        HttpAuthenticationError::UnsupportedChallenge,
                    )),
                }
            }
            StreamableHttpError::InsufficientScope(_) => Some(
                AnonymousInitializeDisposition::Fail(HttpAuthenticationError::Forbidden),
            ),
            // rmcp 2.2 preserves challenged 401/403 structurally, but represents an unchallenged
            // status in this stable message form. Keep the compatibility mapping isolated here.
            StreamableHttpError::UnexpectedServerResponse(message)
                if message.starts_with("HTTP 401 ") =>
            {
                Some(AnonymousInitializeDisposition::Fail(
                    HttpAuthenticationError::Unauthorized,
                ))
            }
            StreamableHttpError::UnexpectedServerResponse(message)
                if message.starts_with("HTTP 403 ") =>
            {
                Some(AnonymousInitializeDisposition::Fail(
                    HttpAuthenticationError::Forbidden,
                ))
            }
            _ => None,
        }
    }

    async fn serve_with_oauth(
        &self,
    ) -> Result<RunningService<RoleClient, A2cClientHandler>, MCPClientError> {
        let oauth = self.oauth().await.map_err(|e| {
            MCPClientError::ConnectionError(format!("OAuth initialization failed: {e}"))
        })?;
        oauth.ensure_machine_authorized().await.map_err(|e| {
            MCPClientError::ConnectionError(format!("OAuth authorization failed: {e}"))
        })?;
        let oauth_request = match oauth.prepare_request().await {
            Ok(request) => request,
            Err(error) => {
                let interactive_authorization_required = matches!(
                    oauth.status().await,
                    OAuthStatus::Unauthorized
                        | OAuthStatus::AuthorizationPending
                        | OAuthStatus::ReauthorizationRequired { .. }
                );
                if interactive_authorization_required {
                    return Err(MCPClientError::HttpAuthentication(
                        HttpAuthenticationError::OAuthRequired,
                    ));
                }
                return Err(MCPClientError::ConnectionError(format!(
                    "OAuth request preparation failed: {error}"
                )));
            }
        };
        let oauth_generation = oauth_request.generation();
        let config = StreamableHttpClientTransportConfig::with_uri(self.base.params.url.clone());
        let transport = StreamableHttpClientTransport::with_client(oauth.http_client(), config);
        let handler = A2cClientHandler::new(
            self.notify
                .read()
                .expect("HTTP notify lock poisoned")
                .clone(),
        );
        let service = tokio::time::timeout(
            Duration::from_secs(CONNECT_TIMEOUT_SECS),
            handler.serve(transport),
        )
        .await
        .map_err(|_| {
            MCPClientError::TimeoutError(format!(
                "HTTP connect timed out after {}s",
                CONNECT_TIMEOUT_SECS
            ))
        })?;
        drop(oauth_request);
        match &service {
            Ok(_) => oauth.observe_service_success(oauth_generation).await,
            Err(error) => {
                let authorization_rejected = Self::initialize_requires_user_authorization(error);
                oauth
                    .observe_initialize_error(error, oauth_generation)
                    .await;
                if authorization_rejected
                    && matches!(oauth.status().await, OAuthStatus::Unauthorized)
                {
                    return Err(MCPClientError::HttpAuthentication(
                        HttpAuthenticationError::OAuthRequired,
                    ));
                }
            }
        }
        service.map_err(|e| MCPClientError::ConnectionError(format!("Initialize failed: {e}")))
    }

    async fn serve_anonymously(
        &self,
    ) -> Result<RunningService<RoleClient, A2cClientHandler>, MCPClientError> {
        let config = StreamableHttpClientTransportConfig::with_uri(self.base.params.url.clone());
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
        let service = tokio::time::timeout(
            Duration::from_secs(CONNECT_TIMEOUT_SECS),
            handler.serve(transport),
        )
        .await
        .map_err(|_| {
            MCPClientError::TimeoutError(format!(
                "HTTP connect timed out after {}s",
                CONNECT_TIMEOUT_SECS
            ))
        })?;
        match service {
            Ok(service) => Ok(service),
            Err(error) => match self.classify_anonymous_initialize_error(&error).await {
                Some(AnonymousInitializeDisposition::RetryWithOAuth) => {
                    self.serve_with_oauth().await
                }
                Some(AnonymousInitializeDisposition::Fail(error)) => {
                    Err(MCPClientError::HttpAuthentication(error))
                }
                Some(AnonymousInitializeDisposition::OAuthInitializationFailed(error)) => {
                    Err(MCPClientError::ConnectionError(format!(
                        "OAuth initialization failed: {error}"
                    )))
                }
                None => Err(MCPClientError::ConnectionError(format!(
                    "Initialize failed: {error}"
                ))),
            },
        }
    }

    pub(crate) async fn oauth_status(&self) -> Result<OAuthStatus, OAuthError> {
        let cancelling = self
            .oauth_flow
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .filter(|flow| flow.is_cancelling() && !flow.is_terminal())
            .cloned();
        if let Some(flow) = cancelling {
            return match flow.wait_terminal().await? {
                OAuthFlowOutcome::Authorized { scopes } => Ok(OAuthStatus::Authorized { scopes }),
                OAuthFlowOutcome::Terminated { status, .. } => Ok(status),
            };
        }
        Ok(self.oauth().await?.status().await)
    }

    pub(crate) async fn create_oauth_flow(
        self: &Arc<Self>,
        request: OAuthBeginRequest,
    ) -> Result<OAuthFlow, OAuthError> {
        if self.oauth.get().is_none() {
            return Err(OAuthError::NotConfigured);
        }
        let (flow, mut driver) = crate::oauth::OAuthFlow::new(request.clone());
        {
            let mut active = self
                .oauth_flow
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if let Some(existing) = active.as_ref().filter(|flow| !flow.is_terminal()) {
                return if existing.request() == &request {
                    Ok(existing.clone())
                } else {
                    Err(OAuthError::AuthorizationAlreadyPending)
                };
            }
            *active = Some(flow.clone());
        }

        let weak = Arc::downgrade(self);
        let flow_id = flow.id();
        tokio::spawn(async move {
            let Some(client) = weak.upgrade() else {
                driver.finish(Err(OAuthError::AuthorizationExpired));
                return;
            };
            let cancellation = driver.cancellation();
            let coordinator = tokio::select! {
                biased;
                result = client.oauth() => result.cloned(),
                _ = cancellation.cancelled() => {
                    let status = client.baseline_oauth_status().await;
                    driver.finish(Ok(OAuthFlowOutcome::Terminated {
                        reason: driver.host_cancellation_reason(),
                        status,
                    }));
                    client.remove_oauth_flow(flow_id);
                    return;
                }
            };
            match coordinator {
                Ok(coordinator) => coordinator.drive_flow(driver).await,
                Err(_error) if cancellation.is_cancelled() => {
                    driver.finish(Ok(OAuthFlowOutcome::Terminated {
                        reason: driver.host_cancellation_reason(),
                        status: client.baseline_oauth_status().await,
                    }));
                }
                Err(error) => driver.finish(Err(error)),
            }
            client.remove_oauth_flow(flow_id);
        });
        Ok(flow)
    }

    pub(crate) async fn cancel_and_drain_oauth_flow(&self) -> Result<(), OAuthError> {
        let flow = self
            .oauth_flow
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        if let Some(flow) = flow.filter(|flow| !flow.is_terminal()) {
            tokio::time::timeout(
                Duration::from_secs(2),
                flow.cancel(crate::oauth::OAuthCancellationReason::Cancelled),
            )
            .await
            .map_err(|_| OAuthError::DrainTimeout)??;
        }
        Ok(())
    }

    pub(crate) fn active_oauth_flow(&self) -> Result<OAuthFlow, OAuthError> {
        self.oauth_flow
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .filter(|flow| !flow.is_terminal())
            .cloned()
            .ok_or(OAuthError::StateMismatch)
    }

    fn remove_oauth_flow(&self, id: uuid::Uuid) {
        let mut active = self
            .oauth_flow
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if active.as_ref().is_some_and(|flow| flow.id() == id) {
            *active = None;
        }
    }

    async fn baseline_oauth_status(&self) -> OAuthStatus {
        if let Some(status) = self
            .oauth_events
            .as_ref()
            .and_then(|events| events.latest_oauth_status(&self.oauth_bundle_id))
        {
            return status;
        }
        let Ok(resource) = self.oauth_options.effective_resource(&self.base.params.url) else {
            return OAuthStatus::Unauthorized;
        };
        locally_stored_oauth_status(
            self.oauth_bundle_id.clone(),
            resource,
            &self.oauth_options,
            Arc::clone(&self.oauth_credential_store),
        )
        .await
        .ok()
        .flatten()
        .unwrap_or(OAuthStatus::Unauthorized)
    }

    pub(crate) async fn clear_oauth(&self) -> Result<(), OAuthError> {
        self.cancel_and_drain_oauth_flow().await?;
        if let Some(oauth) = self.oauth.get() {
            oauth.clear().await
        } else {
            let resource = self
                .oauth_options
                .effective_resource(&self.base.params.url)?;
            clear_stored_oauth_credentials(
                &self.oauth_bundle_id,
                &resource,
                &self.oauth_options,
                Arc::clone(&self.oauth_credential_store),
            )
            .await?;
            if let Some(events) = self.oauth_events.as_ref() {
                events.update_oauth_status(self.oauth_bundle_id.clone(), OAuthStatus::Unauthorized);
            }
            Ok(())
        }
    }

    async fn prepare_oauth_request(&self) -> Result<Option<OAuthRequestGuard>, MCPClientError> {
        if self.oauth.get().is_none() {
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

    fn set_state_change_callback(
        &self,
        callback: Box<dyn Fn(ClientState, ClientState) + Send + Sync>,
    ) {
        self.base.set_state_change_callback(callback);
    }

    async fn connect(&self) -> Result<(), MCPClientError> {
        // 检查是否可以连接 / Check if can connect
        if !self.base.can_connect().await {
            return Err(MCPClientError::ConnectionError(format!(
                "Cannot connect in state: {}",
                self.base.get_state().await
            )));
        }

        let use_oauth = self.oauth.get().is_some();
        let service = if use_oauth {
            self.serve_with_oauth().await?
        } else {
            self.serve_anonymously().await?
        };

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

        let oauth_request = self.prepare_oauth_request().await?;
        let oauth_generation = oauth_request.as_ref().map(OAuthRequestGuard::generation);

        // #178：分页循环内 guard 仅覆盖「下发请求」（`send_request_with_option` 返回 owned handle
        // 即释放），响应等待（`rx.await`）全程无锁——挂起的远端不再阻塞同 server 的
        // connect/disconnect（对齐 `call_tool_cancellable` 参考模式）。
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let guard = self.get_service().await?;
            let request = ClientRequest::ListToolsRequest(ListToolsRequest::with_param(
                PaginatedRequestParams::default().with_cursor(cursor.clone()),
            ));
            let handle: RequestHandle<RoleClient> = match guard
                .as_ref()
                .unwrap()
                .send_request_with_option(request, PeerRequestOptions::no_options())
                .await
            {
                Ok(handle) => handle,
                Err(error) => {
                    drop(oauth_request);
                    self.observe_oauth_service_error(&error, oauth_generation)
                        .await;
                    return Err(MCPClientError::ProtocolError(format!(
                        "List tools error: {}",
                        error
                    )));
                }
            };
            drop(guard);
            let page: ListToolsResult = match handle.rx.await {
                Ok(Ok(ServerResult::ListToolsResult(r))) => r,
                Ok(Ok(_)) => {
                    drop(oauth_request);
                    let e = rmcp::ServiceError::UnexpectedResponse;
                    self.observe_oauth_service_error(&e, oauth_generation).await;
                    return Err(MCPClientError::ProtocolError(format!(
                        "List tools error: {}",
                        e
                    )));
                }
                Ok(Err(e)) => {
                    drop(oauth_request);
                    self.observe_oauth_service_error(&e, oauth_generation).await;
                    return Err(MCPClientError::ProtocolError(format!(
                        "List tools error: {}",
                        e
                    )));
                }
                Err(_) => {
                    drop(oauth_request);
                    let e = rmcp::ServiceError::TransportClosed;
                    self.observe_oauth_service_error(&e, oauth_generation).await;
                    return Err(MCPClientError::ProtocolError(format!(
                        "List tools error: {}",
                        e
                    )));
                }
            };
            tools.extend(page.tools);
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        drop(oauth_request);
        self.observe_oauth_service_success(oauth_generation).await;

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

        let oauth_request = self.prepare_oauth_request().await?;
        let oauth_generation = oauth_request.as_ref().map(OAuthRequestGuard::generation);

        // #178：guard 仅覆盖「下发请求」，响应等待（rx.await）无锁（对齐 call_tool_cancellable）。
        let guard = self.get_service().await?;
        let request = ClientRequest::CallToolRequest(CallToolRequest::new(
            super::utils::call_tool_request_params(tool_name, params),
        ));
        let handle: RequestHandle<RoleClient> = match guard
            .as_ref()
            .unwrap()
            .send_request_with_option(request, PeerRequestOptions::no_options())
            .await
        {
            Ok(handle) => handle,
            Err(error) => {
                drop(oauth_request);
                self.observe_oauth_service_error(&error, oauth_generation)
                    .await;
                return Err(MCPClientError::ToolCallError(error));
            }
        };
        drop(guard);

        match handle.rx.await {
            Ok(Ok(ServerResult::CallToolResult(r))) => {
                drop(oauth_request);
                self.observe_oauth_service_success(oauth_generation).await;
                Ok(r)
            }
            Ok(Ok(_)) => {
                drop(oauth_request);
                // 与原高层 `call_tool` 的 `map_err(MCPClientError::ToolCallError)` 逐字保持：
                // 非 CallToolResult 变体 → ToolCallError(UnexpectedResponse)。
                Err(MCPClientError::ToolCallError(
                    rmcp::ServiceError::UnexpectedResponse,
                ))
            }
            Ok(Err(e)) => {
                drop(oauth_request);
                self.observe_oauth_service_error(&e, oauth_generation).await;
                Err(MCPClientError::ToolCallError(e))
            }
            Err(_) => {
                drop(oauth_request);
                let e = rmcp::ServiceError::TransportClosed;
                self.observe_oauth_service_error(&e, oauth_generation).await;
                Err(MCPClientError::ToolCallError(e))
            }
        }
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
                // 本地取消结果不等待远端通知：rmcp 2.2 streamable worker 的 outbound 为
                // 串行队列（长调用未完成时 notifications/cancelled 排在最后），若行内 await
                // 会使取消 ACK 延迟到长调用结束或 2s 上限（#208 验收窗口 1s 内必达）。
                // 通知按 best-effort 剥离执行（task 自持 2s 上限），本地立即返回 Cancelled。
                // ⚠️ 串行队列限制下，长调用（>2s）的远端 notifications/cancelled 实际
                // 不可达（2s 超时后丢弃）——本地取消即时生效，远端为纯 best-effort。
                let notify_peer = peer;
                tokio::spawn(async move {
                    let notify = notify_peer.notify_cancelled(CancelledNotificationParam::new(
                        Some(request_id),
                        Some(smcp::tool_meta::A2C_DEFAULT_CANCEL_REASON.to_string()),
                    ));
                    if tokio::time::timeout(Duration::from_secs(2), notify).await.is_err() {
                        warn!("emit MCP notifications/cancelled timed out (best-effort, ignored)");
                    }
                });
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

        // 能力校验：未声明 `resources` → CapabilityNotSupported（上层映射 4015）；改用 rmcp `peer_info`
        // （initialize 握手结果），三传输 4015 语义统一，无需再手动缓存 capabilities（INT-04 #78 一致）。
        // #161：与下方 `list_resources_page` 共用同一能力门——窗口枚举聚合层（manager
        // `list_windows_with_diagnostics`）依赖此信号区分「capability 缺失」与「成功空集」。
        let supports_resources = service
            .peer_info()
            .map(|info| info.capabilities.resources.is_some())
            .unwrap_or(false);
        if !supports_resources {
            return Err(MCPClientError::CapabilityNotSupported(
                "resources".to_string(),
            ));
        }
        drop(guard);

        let oauth_request = self.prepare_oauth_request().await?;
        let oauth_generation = oauth_request.as_ref().map(OAuthRequestGuard::generation);

        // #178：分页循环内 guard 仅覆盖「下发请求」，响应等待（rx.await）无锁。
        let mut all_resources = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let guard = self.get_service().await?;
            let request = ClientRequest::ListResourcesRequest(ListResourcesRequest::with_param(
                PaginatedRequestParams::default().with_cursor(cursor.clone()),
            ));
            let handle: RequestHandle<RoleClient> = match guard
                .as_ref()
                .unwrap()
                .send_request_with_option(request, PeerRequestOptions::no_options())
                .await
            {
                Ok(handle) => handle,
                Err(error) => {
                    drop(oauth_request);
                    self.observe_oauth_service_error(&error, oauth_generation)
                        .await;
                    return Err(MCPClientError::ProtocolError(format!(
                        "List resources error: {}",
                        error
                    )));
                }
            };
            drop(guard);
            let page: ListResourcesResult = match handle.rx.await {
                Ok(Ok(ServerResult::ListResourcesResult(r))) => r,
                Ok(Ok(_)) => {
                    drop(oauth_request);
                    let e = rmcp::ServiceError::UnexpectedResponse;
                    self.observe_oauth_service_error(&e, oauth_generation).await;
                    return Err(MCPClientError::ProtocolError(format!(
                        "List resources error: {}",
                        e
                    )));
                }
                Ok(Err(e)) => {
                    drop(oauth_request);
                    self.observe_oauth_service_error(&e, oauth_generation).await;
                    return Err(MCPClientError::ProtocolError(format!(
                        "List resources error: {}",
                        e
                    )));
                }
                Err(_) => {
                    drop(oauth_request);
                    let e = rmcp::ServiceError::TransportClosed;
                    self.observe_oauth_service_error(&e, oauth_generation).await;
                    return Err(MCPClientError::ProtocolError(format!(
                        "List resources error: {}",
                        e
                    )));
                }
            };
            all_resources.extend(page.resources);
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        drop(oauth_request);
        self.observe_oauth_service_success(oauth_generation).await;

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
        drop(guard);

        // 单页透传：cursor 进/出，不聚合、不过滤、不返回 resourceTemplates。
        let param = cursor.map(|c| PaginatedRequestParams::default().with_cursor(Some(c)));
        let oauth_request = self.prepare_oauth_request().await?;
        let oauth_generation = oauth_request.as_ref().map(OAuthRequestGuard::generation);

        // #178：guard 仅覆盖「下发请求」，响应等待（rx.await）无锁。
        let guard = self.get_service().await?;
        let request = match param {
            Some(p) => ClientRequest::ListResourcesRequest(ListResourcesRequest::with_param(p)),
            None => ClientRequest::ListResourcesRequest(ListResourcesRequest::default()),
        };
        let handle: RequestHandle<RoleClient> = match guard
            .as_ref()
            .unwrap()
            .send_request_with_option(request, PeerRequestOptions::no_options())
            .await
        {
            Ok(handle) => handle,
            Err(error) => {
                drop(oauth_request);
                self.observe_oauth_service_error(&error, oauth_generation)
                    .await;
                return Err(MCPClientError::ProtocolError(format!(
                    "List resources error: {}",
                    error
                )));
            }
        };
        drop(guard);

        let result: ListResourcesResult = match handle.rx.await {
            Ok(Ok(ServerResult::ListResourcesResult(r))) => r,
            Ok(Ok(_)) => {
                drop(oauth_request);
                let e = rmcp::ServiceError::UnexpectedResponse;
                self.observe_oauth_service_error(&e, oauth_generation).await;
                return Err(MCPClientError::ProtocolError(format!(
                    "List resources error: {}",
                    e
                )));
            }
            Ok(Err(e)) => {
                drop(oauth_request);
                self.observe_oauth_service_error(&e, oauth_generation).await;
                return Err(MCPClientError::ProtocolError(format!(
                    "List resources error: {}",
                    e
                )));
            }
            Err(_) => {
                drop(oauth_request);
                let e = rmcp::ServiceError::TransportClosed;
                self.observe_oauth_service_error(&e, oauth_generation).await;
                return Err(MCPClientError::ProtocolError(format!(
                    "List resources error: {}",
                    e
                )));
            }
        };
        drop(oauth_request);
        self.observe_oauth_service_success(oauth_generation).await;

        Ok((result.resources, result.next_cursor))
    }

    async fn get_window_detail(
        &self,
        resource: Resource,
    ) -> Result<ReadResourceResult, MCPClientError> {
        if self.base.get_state().await != ClientState::Connected {
            return Err(MCPClientError::ConnectionError("Not connected".to_string()));
        }

        let oauth_request = self.prepare_oauth_request().await?;
        let oauth_generation = oauth_request.as_ref().map(OAuthRequestGuard::generation);

        // #178：guard 仅覆盖「下发请求」，响应等待（rx.await）无锁。
        let guard = self.get_service().await?;
        let request = ClientRequest::ReadResourceRequest(ReadResourceRequest::new(
            ReadResourceRequestParams::new(resource.uri.clone()),
        ));
        let handle: RequestHandle<RoleClient> = match guard
            .as_ref()
            .unwrap()
            .send_request_with_option(request, PeerRequestOptions::no_options())
            .await
        {
            Ok(handle) => handle,
            Err(error) => {
                drop(oauth_request);
                self.observe_oauth_service_error(&error, oauth_generation)
                    .await;
                return Err(MCPClientError::ProtocolError(format!(
                    "Read resource error: {}",
                    error
                )));
            }
        };
        drop(guard);

        let result: ReadResourceResult = match handle.rx.await {
            Ok(Ok(ServerResult::ReadResourceResult(r))) => r,
            Ok(Ok(_)) => {
                drop(oauth_request);
                let e = rmcp::ServiceError::UnexpectedResponse;
                self.observe_oauth_service_error(&e, oauth_generation).await;
                return Err(MCPClientError::ProtocolError(format!(
                    "Read resource error: {}",
                    e
                )));
            }
            Ok(Err(e)) => {
                drop(oauth_request);
                self.observe_oauth_service_error(&e, oauth_generation).await;
                return Err(MCPClientError::ProtocolError(format!(
                    "Read resource error: {}",
                    e
                )));
            }
            Err(_) => {
                drop(oauth_request);
                let e = rmcp::ServiceError::TransportClosed;
                self.observe_oauth_service_error(&e, oauth_generation).await;
                return Err(MCPClientError::ProtocolError(format!(
                    "Read resource error: {}",
                    e
                )));
            }
        };
        drop(oauth_request);
        self.observe_oauth_service_success(oauth_generation).await;

        Ok(result)
    }

    async fn subscribe_window(&self, resource: Resource) -> Result<(), MCPClientError> {
        if self.base.get_state().await != ClientState::Connected {
            return Err(MCPClientError::ConnectionError("Not connected".to_string()));
        }

        let oauth_request = self.prepare_oauth_request().await?;
        let oauth_generation = oauth_request.as_ref().map(OAuthRequestGuard::generation);

        // #178：guard 仅覆盖「下发请求」，响应等待（rx.await）无锁（订阅成功响应为 EmptyResult）。
        let guard = self.get_service().await?;
        let request = ClientRequest::SubscribeRequest(SubscribeRequest::new(
            SubscribeRequestParams::new(resource.uri.clone()),
        ));
        let handle: RequestHandle<RoleClient> = match guard
            .as_ref()
            .unwrap()
            .send_request_with_option(request, PeerRequestOptions::no_options())
            .await
        {
            Ok(handle) => handle,
            Err(error) => {
                drop(oauth_request);
                self.observe_oauth_service_error(&error, oauth_generation)
                    .await;
                return Err(MCPClientError::ProtocolError(format!(
                    "Subscribe resource error: {}",
                    error
                )));
            }
        };
        drop(guard);

        match handle.rx.await {
            Ok(Ok(ServerResult::EmptyResult(_))) => {
                drop(oauth_request);
                self.observe_oauth_service_success(oauth_generation).await;
            }
            Ok(Ok(_)) => {
                drop(oauth_request);
                let e = rmcp::ServiceError::UnexpectedResponse;
                self.observe_oauth_service_error(&e, oauth_generation).await;
                return Err(MCPClientError::ProtocolError(format!(
                    "Subscribe resource error: {}",
                    e
                )));
            }
            Ok(Err(e)) => {
                drop(oauth_request);
                self.observe_oauth_service_error(&e, oauth_generation).await;
                return Err(MCPClientError::ProtocolError(format!(
                    "Subscribe resource error: {}",
                    e
                )));
            }
            Err(_) => {
                drop(oauth_request);
                let e = rmcp::ServiceError::TransportClosed;
                self.observe_oauth_service_error(&e, oauth_generation).await;
                return Err(MCPClientError::ProtocolError(format!(
                    "Subscribe resource error: {}",
                    e
                )));
            }
        }

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

        let oauth_request = self.prepare_oauth_request().await?;
        let oauth_generation = oauth_request.as_ref().map(OAuthRequestGuard::generation);

        // #178：guard 仅覆盖「下发请求」，响应等待（rx.await）无锁。
        let guard = self.get_service().await?;
        let request = ClientRequest::UnsubscribeRequest(UnsubscribeRequest::new(
            UnsubscribeRequestParams::new(resource.uri.clone()),
        ));
        let handle: RequestHandle<RoleClient> = match guard
            .as_ref()
            .unwrap()
            .send_request_with_option(request, PeerRequestOptions::no_options())
            .await
        {
            Ok(handle) => handle,
            Err(error) => {
                drop(oauth_request);
                self.observe_oauth_service_error(&error, oauth_generation)
                    .await;
                return Err(MCPClientError::ProtocolError(format!(
                    "Unsubscribe resource error: {}",
                    error
                )));
            }
        };
        drop(guard);

        match handle.rx.await {
            Ok(Ok(ServerResult::EmptyResult(_))) => {
                drop(oauth_request);
                self.observe_oauth_service_success(oauth_generation).await;
            }
            Ok(Ok(_)) => {
                drop(oauth_request);
                let e = rmcp::ServiceError::UnexpectedResponse;
                self.observe_oauth_service_error(&e, oauth_generation).await;
                return Err(MCPClientError::ProtocolError(format!(
                    "Unsubscribe resource error: {}",
                    e
                )));
            }
            Ok(Err(e)) => {
                drop(oauth_request);
                self.observe_oauth_service_error(&e, oauth_generation).await;
                return Err(MCPClientError::ProtocolError(format!(
                    "Unsubscribe resource error: {}",
                    e
                )));
            }
            Err(_) => {
                drop(oauth_request);
                let e = rmcp::ServiceError::TransportClosed;
                self.observe_oauth_service_error(&e, oauth_generation).await;
                return Err(MCPClientError::ProtocolError(format!(
                    "Unsubscribe resource error: {}",
                    e
                )));
            }
        }

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

    #[test]
    fn challenge_admission_requires_bearer_resource_metadata() {
        assert_eq!(
            HttpMCPClient::classify_challenge(
                r#"Bearer realm="mcp", resource_metadata="https://mcp.example/.well-known/oauth-protected-resource""#,
            ),
            ChallengeAdmission::BearerWithMetadata(
                "https://mcp.example/.well-known/oauth-protected-resource".to_string()
            )
        );
        assert_eq!(
            HttpMCPClient::classify_challenge(r#"Basic realm="legacy""#),
            ChallengeAdmission::Unsupported
        );
        assert_eq!(
            HttpMCPClient::classify_challenge(r#"Digest realm="legacy", Bearer realm="mcp""#),
            ChallengeAdmission::BearerWithoutMetadata
        );
        assert_eq!(
            HttpMCPClient::classify_challenge(
                r#"Basic realm="legacy", Bearer resource_metadata="https://mcp.example/prm""#,
            ),
            ChallengeAdmission::BearerWithMetadata("https://mcp.example/prm".to_string())
        );
    }

    #[test]
    fn invalid_challenge_is_never_admitted() {
        assert_eq!(
            HttpMCPClient::classify_challenge("Bearer resource_metadata=\""),
            ChallengeAdmission::Unsupported
        );
        assert_eq!(
            HttpMCPClient::classify_challenge(""),
            ChallengeAdmission::Unsupported
        );
    }

    #[test]
    fn http_client_starts_in_anonymous_first_mode() {
        let params = HttpServerParameters {
            url: "https://mcp.example/mcp".to_string(),
            headers: HashMap::new(),
        };
        let client = HttpMCPClient::new(params);
        assert!(!client.oauth_callback_configured());
        assert!(client.oauth.get().is_none());
        assert_eq!(client.oauth_options, OAuthOptions::default());
    }

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
