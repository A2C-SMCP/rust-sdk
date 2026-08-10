#![doc = include_str!("../docs/oauth-host-integration.md")]

mod flow;

pub use flow::OAuthFlow;
pub(crate) use flow::{OAuthFlowCommand, OAuthFlowDriver, OAuthFlowTerminal};

use crate::inputs::SecretValueResolver;
use crate::mcp_clients::bundle_id::BundleId;
use crate::mcp_clients::model::{MCPServerInput, PromptStringInput};
use crate::status::RuntimeStatus;
use crate::weak_registry::WeakRegistry;
use async_trait::async_trait;
use futures_util::StreamExt;
use http::{HeaderMap, HeaderName, HeaderValue};
use rmcp::transport::auth::{AuthorizationMetadata, OAuthClientConfig};
use rmcp::transport::streamable_http_client::{
    StreamableHttpClient, StreamableHttpError, StreamableHttpPostResponse,
};
use rmcp::transport::{
    AuthClient, AuthError as RmcpAuthError, AuthorizationManager, ClientCredentialsConfig,
    CredentialStore, JwtSigningAlgorithm, OAuthHttpClient, OAuthHttpClientError,
    OAuthHttpClientFuture, OAuthHttpRedirectPolicy, OAuthHttpRequest, StateStore,
    StoredAuthorizationState, StoredCredentials,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock, RwLock as StdRwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tokio::sync::{Mutex, OwnedRwLockReadGuard, RwLock};
use tokio_util::sync::CancellationToken;
use tracing::instrument::WithSubscriber;
use url::Url;

const OAUTH_HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const DISCOVERY_CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);
const AUTHORIZATION_STATE_TTL: Duration = Duration::from_secs(10 * 60);
const MAX_OAUTH_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_MACHINE_SCOPE_UPGRADES: usize = 3;
const MCP_SESSION_ID_HEADER: &str = "Mcp-Session-Id";
const MCP_PROTOCOL_VERSION_HEADER: &str = "MCP-Protocol-Version";
const DISCOVERY_PROTOCOL_VERSION: &str = "2024-11-05";

/// Record type stored through [`OAuthCredentialStore`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub enum OAuthCredentialRecordKind {
    /// Serialized OAuth client registration and token credentials.
    Credentials,
    /// Core-owned issuer index and encrypted active-credential snapshot.
    ///
    /// Keeping the active snapshot in this single record lets hosts atomically replace the
    /// credential set while retaining the issuer list needed for network-free cleanup.
    IssuerIndex,
}

/// Stable, bundle-aware key supplied to a host-provided OAuth credential store.
///
/// `tenant` and `principal` remain host runtime context: a multi-tenant store can prepend them to
/// [`Self::stable_id`] without putting deployment identity into serializable MCP configuration.
/// The key separates bundle, canonical protected resource, authorization server, grant/client
/// fingerprint, and SDK record kind; hosts must preserve every dimension rather than keying only
/// by MCP display name or resource URL.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub struct OAuthCredentialKey {
    pub bundle_id: BundleId,
    pub resource: String,
    pub issuer: Option<String>,
    pub grant_fingerprint: String,
    pub record_kind: OAuthCredentialRecordKind,
}

impl OAuthCredentialKey {
    /// Deterministic, non-secret identifier suitable for keyring/database keys.
    ///
    /// Multi-tenant hosts should namespace this value with trusted tenant/principal context captured
    /// when constructing their store. Callback parameters and serialized MCP configuration are not
    /// trusted sources for that context.
    pub fn stable_id(&self) -> String {
        let mut digest = Sha256::new();
        digest.update(self.bundle_id.as_str().as_bytes());
        digest.update(b"\0");
        digest.update(self.resource.as_bytes());
        digest.update(b"\0");
        digest.update(self.issuer.as_deref().unwrap_or("<none>").as_bytes());
        digest.update(b"\0");
        digest.update(self.grant_fingerprint.as_bytes());
        digest.update(b"\0");
        digest.update(match self.record_kind {
            OAuthCredentialRecordKind::Credentials => b"credentials".as_slice(),
            OAuthCredentialRecordKind::IssuerIndex => b"issuer-index".as_slice(),
        });
        format!("mcp-oauth-{:x}", digest.finalize())
    }
}

/// Failure returned by a host OAuth credential store.
///
/// Variants intentionally carry no backend payload so a vault error cannot accidentally echo a
/// credential, tenant identifier, or provider response through SDK diagnostics.
#[derive(Debug, Clone, Copy, Error, PartialEq, Eq)]
pub enum OAuthCredentialStoreError {
    #[error("OAuth credential store is unavailable")]
    Unavailable,
    #[error("OAuth credential store operation failed")]
    OperationFailed,
}

/// Host-injected storage for opaque serialized OAuth credentials.
///
/// Values contain tokens and MUST be encrypted at rest by persistent implementations. The SDK
/// defaults to [`InMemoryOAuthCredentialStore`] and never probes an OS keyring on its own.
///
/// One store instance may serve every OAuth MCP owned by a [`crate::computer::Computer`]. Methods
/// can be called concurrently and implementations must provide their own synchronization. A store
/// must not log or expose `value`; configured client secrets and private keys remain inputs resolved
/// by [`SecretValueResolver`] and are not part of the stored envelope.
///
/// Pending PKCE/CSRF state and host callback routing are separate concerns: this trait only stores
/// credentials after authorization. Returning an error fails the OAuth operation; the SDK does not
/// silently fall back to memory after a host store has been injected.
///
/// [`save`](Self::save) must replace one key atomically: when it returns an error, any value that
/// existed for that key before the call must remain readable. The coordinator relies on this
/// single-key guarantee to keep a previously authorized credential intact when a reauthorization
/// commit fails.
#[async_trait]
pub trait OAuthCredentialStore: Send + Sync {
    async fn load(
        &self,
        key: &OAuthCredentialKey,
    ) -> Result<Option<String>, OAuthCredentialStoreError>;
    async fn save(
        &self,
        key: &OAuthCredentialKey,
        value: &str,
    ) -> Result<(), OAuthCredentialStoreError>;
    async fn delete(&self, key: &OAuthCredentialKey) -> Result<(), OAuthCredentialStoreError>;
}

/// Default keyed process-memory OAuth credential store.
#[derive(Default)]
pub struct InMemoryOAuthCredentialStore {
    entries: RwLock<HashMap<OAuthCredentialKey, String>>,
}

#[async_trait]
impl OAuthCredentialStore for InMemoryOAuthCredentialStore {
    async fn load(
        &self,
        key: &OAuthCredentialKey,
    ) -> Result<Option<String>, OAuthCredentialStoreError> {
        Ok(self.entries.read().await.get(key).cloned())
    }

    async fn save(
        &self,
        key: &OAuthCredentialKey,
        value: &str,
    ) -> Result<(), OAuthCredentialStoreError> {
        self.entries
            .write()
            .await
            .insert(key.clone(), value.to_string());
        Ok(())
    }

    async fn delete(&self, key: &OAuthCredentialKey) -> Result<(), OAuthCredentialStoreError> {
        self.entries.write().await.remove(key);
        Ok(())
    }
}

/// Candidate-only rmcp credential store used until an interactive flow wins the terminal race.
///
/// Token exchange and DCR may mutate this store freely; the host store is touched only by the
/// coordinator's serialized commit step.
#[derive(Clone, Default)]
struct StagedCredentialStore {
    credentials: Arc<RwLock<Option<StoredCredentials>>>,
}

#[async_trait]
impl CredentialStore for StagedCredentialStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, RmcpAuthError> {
        Ok(self.credentials.read().await.clone())
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), RmcpAuthError> {
        *self.credentials.write().await = Some(credentials);
        Ok(())
    }

    async fn clear(&self) -> Result<(), RmcpAuthError> {
        *self.credentials.write().await = None;
        Ok(())
    }
}

struct OAuthResourceLifecycle {
    generation: AtomicU64,
    request_gate: Arc<RwLock<()>>,
}

type OAuthLifecycleKey = (usize, BundleId, String, String);
type OAuthLifecycleRegistry = WeakRegistry<OAuthLifecycleKey, OAuthResourceLifecycle>;
static OAUTH_RESOURCE_LIFECYCLES: OnceLock<OAuthLifecycleRegistry> = OnceLock::new();

fn oauth_lifecycle_registry() -> &'static OAuthLifecycleRegistry {
    OAUTH_RESOURCE_LIFECYCLES.get_or_init(WeakRegistry::default)
}

/// Process-local PKCE/CSRF state with deterministic expiry and explicit deletion.
///
/// rmcp's default in-memory store has no TTL and cannot be addressed by the SDK's
/// `clear_oauth` facade. Owning the store keeps pending authorization state in the
/// same lifecycle as [`OAuthCoordinator`].
#[derive(Clone)]
struct ExpiringStateStore {
    states: Arc<RwLock<HashMap<String, ExpiringAuthorizationState>>>,
    ttl: Duration,
}

struct ExpiringAuthorizationState {
    state: StoredAuthorizationState,
    claimed_for_exchange: bool,
}

impl ExpiringStateStore {
    fn new(ttl: Duration) -> Self {
        Self {
            states: Arc::new(RwLock::new(HashMap::new())),
            ttl,
        }
    }

    fn is_expired(&self, entry: &ExpiringAuthorizationState, now: u64) -> bool {
        !entry.claimed_for_exchange
            && now.saturating_sub(entry.state.created_at) > self.ttl.as_secs()
    }

    fn now_epoch_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    async fn claim_for_exchange(&self, csrf_token: &str) -> Option<StoredAuthorizationState> {
        let now = Self::now_epoch_secs();
        let mut states = self.states.write().await;
        if states
            .get(csrf_token)
            .is_some_and(|entry| self.is_expired(entry, now))
        {
            states.remove(csrf_token);
            return None;
        }
        let entry = states.get_mut(csrf_token)?;
        entry.claimed_for_exchange = true;
        Some(entry.state.clone())
    }

    async fn release_exchange_claim(&self, csrf_token: &str) {
        if let Some(entry) = self.states.write().await.get_mut(csrf_token) {
            entry.claimed_for_exchange = false;
        }
    }
}

#[async_trait]
impl StateStore for ExpiringStateStore {
    async fn save(
        &self,
        csrf_token: &str,
        state: StoredAuthorizationState,
    ) -> Result<(), RmcpAuthError> {
        let now = Self::now_epoch_secs();
        let mut states = self.states.write().await;
        states.retain(|_, existing| !self.is_expired(existing, now));
        states.insert(
            csrf_token.to_string(),
            ExpiringAuthorizationState {
                state,
                claimed_for_exchange: false,
            },
        );
        Ok(())
    }

    async fn load(
        &self,
        csrf_token: &str,
    ) -> Result<Option<StoredAuthorizationState>, RmcpAuthError> {
        let now = Self::now_epoch_secs();
        let mut states = self.states.write().await;
        if states
            .get(csrf_token)
            .is_some_and(|entry| self.is_expired(entry, now))
        {
            states.remove(csrf_token);
            Ok(None)
        } else {
            Ok(states.get(csrf_token).map(|entry| entry.state.clone()))
        }
    }

    async fn delete(&self, csrf_token: &str) -> Result<(), RmcpAuthError> {
        self.states.write().await.remove(csrf_token);
        Ok(())
    }
}

fn oauth_resource_lifecycle(
    credential_store: &Arc<dyn OAuthCredentialStore>,
    bundle_id: &BundleId,
    resource: &str,
    mode_fingerprint: &str,
) -> Arc<OAuthResourceLifecycle> {
    let store_identity = Arc::as_ptr(credential_store) as *const () as usize;
    let slot = (
        store_identity,
        bundle_id.clone(),
        resource.to_string(),
        mode_fingerprint.to_string(),
    );
    oauth_lifecycle_registry().get_or_insert_with(slot, || OAuthResourceLifecycle {
        generation: AtomicU64::new(0),
        request_gate: Arc::new(RwLock::new(())),
    })
}

/// OAuth HTTP adapter that cleans up a session accidentally created by rmcp's
/// metadata-discovery initialize probe.
struct DiscoveryCleanupOAuthHttpClient {
    follow_redirects: reqwest::Client,
    stop_redirects: reqwest::Client,
    protected_resource_redirects: Option<reqwest::Client>,
    protected_resource: Option<Url>,
    protected_resource_headers: HeaderMap,
    protected_resource_metadata_urls: StdRwLock<HashSet<String>>,
    admitted_resource_metadata: Option<AdmittedResourceMetadata>,
}

#[derive(Clone)]
struct AdmittedResourceMetadata {
    url: Url,
    resource: String,
    validated: Arc<AtomicBool>,
}

#[derive(Deserialize)]
struct AdmittedResourceMetadataDocument {
    resource: Option<String>,
    authorization_server: Option<String>,
    authorization_servers: Option<Vec<String>>,
    scopes_supported: Option<Vec<String>>,
}

impl DiscoveryCleanupOAuthHttpClient {
    #[cfg(test)]
    fn new() -> Result<Self, RmcpAuthError> {
        Self::new_for_protected_resource(None, HeaderMap::new(), None)
    }

    fn with_protected_resource_headers(
        resource: &str,
        mut protected_resource_headers: HeaderMap,
        admitted_resource_metadata: Option<AdmittedResourceMetadata>,
    ) -> Result<Self, RmcpAuthError> {
        protected_resource_headers.remove(http::header::AUTHORIZATION);
        let resource = Url::parse(resource)
            .map_err(|error| RmcpAuthError::InternalError(error.to_string()))?;
        Self::new_for_protected_resource(
            Some(resource),
            protected_resource_headers,
            admitted_resource_metadata,
        )
    }

    fn new_for_protected_resource(
        protected_resource: Option<Url>,
        protected_resource_headers: HeaderMap,
        admitted_resource_metadata: Option<AdmittedResourceMetadata>,
    ) -> Result<Self, RmcpAuthError> {
        let follow_redirects = reqwest::Client::builder()
            .timeout(OAUTH_HTTP_TIMEOUT)
            .build()
            .map_err(|error| RmcpAuthError::InternalError(error.to_string()))?;
        let stop_redirects = reqwest::Client::builder()
            .timeout(OAUTH_HTTP_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| RmcpAuthError::InternalError(error.to_string()))?;
        let protected_resource_redirects = protected_resource
            .as_ref()
            .map(|resource| {
                let resource = resource.clone();
                reqwest::Client::builder()
                    .timeout(OAUTH_HTTP_TIMEOUT)
                    .redirect(reqwest::redirect::Policy::custom(move |attempt| {
                        if same_origin(&resource, attempt.url()) {
                            if attempt.previous().len() >= 10 {
                                attempt.error("too many same-origin redirects")
                            } else {
                                attempt.follow()
                            }
                        } else {
                            attempt.stop()
                        }
                    }))
                    .build()
                    .map_err(|error| RmcpAuthError::InternalError(error.to_string()))
            })
            .transpose()?;
        Ok(Self {
            follow_redirects,
            stop_redirects,
            protected_resource_redirects,
            protected_resource,
            protected_resource_headers,
            protected_resource_metadata_urls: StdRwLock::new(HashSet::new()),
            admitted_resource_metadata,
        })
    }

    fn admitted_resource_matches(expected: &str, actual: &str) -> bool {
        let (Ok(expected), Ok(actual)) = (Url::parse(expected), Url::parse(actual)) else {
            return false;
        };
        expected == actual
    }

    fn observe_admitted_resource_metadata(
        &self,
        request_url: &Url,
        status: reqwest::StatusCode,
        body: &[u8],
    ) {
        let Some(expected) = self
            .admitted_resource_metadata
            .as_ref()
            .filter(|expected| &expected.url == request_url && status.is_success())
        else {
            return;
        };
        let valid = serde_json::from_slice::<AdmittedResourceMetadataDocument>(body)
            .ok()
            .and_then(|metadata| {
                // Reading every rmcp-recognized PRM field keeps this admission proof aligned with
                // rmcp's typed document rather than accepting a body whose resource happens to
                // parse while another known field has the wrong JSON type.
                let _known_fields = (
                    metadata.authorization_server,
                    metadata.authorization_servers,
                    metadata.scopes_supported,
                );
                metadata.resource
            })
            .is_some_and(|resource| Self::admitted_resource_matches(&expected.resource, &resource));
        if valid {
            expected.validated.store(true, Ordering::Release);
        }
    }

    fn is_protected_resource_request(&self, request_url: &Url) -> bool {
        let Some(resource) = self.protected_resource.as_ref() else {
            return false;
        };
        if !same_origin(resource, request_url) {
            return false;
        }
        request_url == resource
            || request_url
                .path()
                .starts_with("/.well-known/oauth-protected-resource")
            || self
                .protected_resource_metadata_urls
                .read()
                .expect("protected resource metadata URL lock poisoned")
                .contains(request_url.as_str())
    }

    fn remember_resource_metadata_url(&self, response_url: &Url, headers: &HeaderMap) {
        for value in headers.get_all(http::header::WWW_AUTHENTICATE) {
            let Ok(value) = value.to_str() else {
                continue;
            };
            let lowercase = value.to_ascii_lowercase();
            let Some(position) = lowercase.find("resource_metadata=") else {
                continue;
            };
            let raw = value[position + "resource_metadata=".len()..].trim_start();
            let candidate = if let Some(quoted) = raw.strip_prefix('"') {
                quoted.split('"').next()
            } else {
                raw.split([',', ' ']).next()
            };
            let Some(candidate) = candidate.filter(|candidate| !candidate.is_empty()) else {
                continue;
            };
            let Ok(candidate) = response_url.join(candidate) else {
                continue;
            };
            if same_origin(
                self.protected_resource
                    .as_ref()
                    .expect("protected resource was checked above"),
                &candidate,
            ) {
                self.protected_resource_metadata_urls
                    .write()
                    .expect("protected resource metadata URL lock poisoned")
                    .insert(candidate.to_string());
            }
        }
    }

    async fn delete_discovery_session(
        client: reqwest::Client,
        url: reqwest::Url,
        session_id: String,
        protected_resource_headers: HeaderMap,
    ) {
        let cleanup = client
            .delete(url)
            .headers(protected_resource_headers)
            .header(MCP_SESSION_ID_HEADER, session_id)
            .header(MCP_PROTOCOL_VERSION_HEADER, DISCOVERY_PROTOCOL_VERSION)
            .send();
        match tokio::time::timeout(DISCOVERY_CLEANUP_TIMEOUT, cleanup).await {
            Ok(Ok(response)) if response.status().is_success() => {}
            Ok(Ok(response)) => {
                tracing::warn!(
                    status = %response.status(),
                    "OAuth discovery MCP session cleanup was rejected"
                );
            }
            Ok(Err(error)) => {
                tracing::warn!(error = %error, "failed to clean up OAuth discovery MCP session");
            }
            Err(_) => {
                tracing::warn!("OAuth discovery MCP session cleanup timed out");
            }
        }
    }
}

impl OAuthHttpClient for DiscoveryCleanupOAuthHttpClient {
    fn execute(&self, request: OAuthHttpRequest) -> OAuthHttpClientFuture<'_> {
        Box::pin(async move {
            let OAuthHttpRequest {
                request,
                redirect_policy,
                timeout,
                ..
            } = request;
            let is_discovery_probe = request.method() == http::Method::POST
                && request.headers().contains_key(MCP_PROTOCOL_VERSION_HEADER);
            let mut request = reqwest::Request::try_from(request)
                .map_err(|error| OAuthHttpClientError::new(error.to_string()))?;
            *request.timeout_mut() = timeout;
            let request_url = request.url().clone();
            let is_protected_resource = self.is_protected_resource_request(&request_url);
            if is_protected_resource {
                for (name, value) in &self.protected_resource_headers {
                    if !request.headers().contains_key(name) {
                        request.headers_mut().insert(name.clone(), value.clone());
                    }
                }
            }
            let client = if is_protected_resource {
                match redirect_policy {
                    OAuthHttpRedirectPolicy::Follow => self
                        .protected_resource_redirects
                        .as_ref()
                        .unwrap_or(&self.stop_redirects),
                    _ => &self.stop_redirects,
                }
            } else {
                match redirect_policy {
                    OAuthHttpRedirectPolicy::Follow => &self.follow_redirects,
                    OAuthHttpRedirectPolicy::Stop => &self.stop_redirects,
                    _ => &self.stop_redirects,
                }
            };
            let response = client
                .execute(request)
                .await
                .map_err(|error| OAuthHttpClientError::new(error.to_string()))?;
            let response_status = response.status();
            if is_protected_resource {
                self.remember_resource_metadata_url(&request_url, response.headers());
            }
            let discovery_session = is_discovery_probe
                .then(|| {
                    response
                        .headers()
                        .get(MCP_SESSION_ID_HEADER)
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_owned)
                })
                .flatten();
            let cleanup_task = discovery_session.map(|session_id| {
                tokio::spawn(Self::delete_discovery_session(
                    self.stop_redirects.clone(),
                    request_url.clone(),
                    session_id,
                    if is_protected_resource {
                        self.protected_resource_headers.clone()
                    } else {
                        HeaderMap::new()
                    },
                ))
            });

            let mut builder = http::Response::builder()
                .status(response.status())
                .version(response.version());
            for (name, value) in response.headers() {
                builder = builder.header(name, value);
            }
            let body_result = async {
                let mut body = Vec::new();
                let mut body_stream = response.bytes_stream();
                while let Some(chunk) = body_stream.next().await {
                    let chunk =
                        chunk.map_err(|error| OAuthHttpClientError::new(error.to_string()))?;
                    if chunk.len() > MAX_OAUTH_RESPONSE_BYTES.saturating_sub(body.len()) {
                        return Err(OAuthHttpClientError::new(format!(
                            "OAuth HTTP response body exceeds {MAX_OAUTH_RESPONSE_BYTES} bytes"
                        )));
                    }
                    body.extend_from_slice(&chunk);
                }
                Ok(body)
            }
            .await;

            if let Some(cleanup_task) = cleanup_task {
                let _ = cleanup_task.await;
            }
            let body = body_result?;
            self.observe_admitted_resource_metadata(&request_url, response_status, &body);

            builder
                .body(body)
                .map_err(|error| OAuthHttpClientError::new(error.to_string()))
        })
    }
}

/// Event-driven cancellation decorator for provider operations owned by one interactive flow.
struct CancellableOAuthHttpClient {
    inner: Arc<dyn OAuthHttpClient>,
    cancellation: CancellationToken,
}

impl CancellableOAuthHttpClient {
    fn new(inner: Arc<dyn OAuthHttpClient>, cancellation: CancellationToken) -> Self {
        Self {
            inner,
            cancellation,
        }
    }
}

impl OAuthHttpClient for CancellableOAuthHttpClient {
    fn execute(&self, request: OAuthHttpRequest) -> OAuthHttpClientFuture<'_> {
        Box::pin(async move {
            tokio::select! {
                biased;
                result = self.inner.execute(request) => result,
                _ = self.cancellation.cancelled() => {
                    Err(OAuthHttpClientError::new("OAuth flow cancelled"))
                }
            }
        })
    }
}

fn same_origin(left: &Url, right: &Url) -> bool {
    left.scheme() == right.scheme()
        && left.host_str() == right.host_str()
        && left.port_or_known_default() == right.port_or_known_default()
}

/// Runs every authenticated transport request under a no-op tracing subscriber.
///
/// rmcp 2.2 emits token endpoint response details while refreshing. Poll-scoped
/// instrumentation prevents authorization codes, token extensions, and hostile
/// `error_description` values from reaching an embedding application's subscriber.
#[derive(Clone)]
pub(crate) struct SensitiveAuthClient {
    inner: AuthClient<reqwest::Client>,
}

impl SensitiveAuthClient {
    fn new(inner: AuthClient<reqwest::Client>) -> Self {
        Self { inner }
    }
}

impl StreamableHttpClient for SensitiveAuthClient {
    type Error = reqwest::Error;

    fn post_message(
        &self,
        uri: Arc<str>,
        message: rmcp::model::ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> impl Future<Output = Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>>>
           + Send
           + '_ {
        self.inner
            .post_message(uri, message, session_id, auth_header, custom_headers)
            .with_subscriber(tracing::subscriber::NoSubscriber::default())
    }

    fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> impl Future<Output = Result<(), StreamableHttpError<Self::Error>>> + Send + '_ {
        self.inner
            .delete_session(uri, session_id, auth_header, custom_headers)
            .with_subscriber(tracing::subscriber::NoSubscriber::default())
    }

    fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> impl Future<
        Output = Result<
            futures_util::stream::BoxStream<'static, Result<sse_stream::Sse, sse_stream::Error>>,
            StreamableHttpError<Self::Error>,
        >,
    > + Send
           + '_ {
        self.inner
            .get_stream(uri, session_id, last_event_id, auth_header, custom_headers)
            .with_subscriber(tracing::subscriber::NoSubscriber::default())
    }
}

/// OAuth configuration attached to an HTTP MCP server.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OAuthOptions {
    /// Canonical RFC 8707 resource indicator.
    ///
    /// When omitted, the Streamable HTTP MCP endpoint is used for backward compatibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub client_name: Option<String>,
    pub mode: OAuthClientMode,
}

impl Default for OAuthOptions {
    fn default() -> Self {
        Self {
            resource: None,
            scopes: Vec::new(),
            client_name: None,
            mode: OAuthClientMode::AuthorizationCode {
                registration: OAuthClientRegistration::Dynamic,
            },
        }
    }
}

impl OAuthOptions {
    pub(crate) fn effective_resource(&self, mcp_endpoint: &str) -> Result<String, OAuthError> {
        canonical_resource_identity(self.resource.as_deref().unwrap_or(mcp_endpoint))
    }
}

/// Interactive or machine-to-machine OAuth flow.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum OAuthClientMode {
    AuthorizationCode {
        #[serde(flatten)]
        registration: OAuthClientRegistration,
    },
    ClientCredentialsSecret {
        client_id: String,
        client_secret_input: String,
    },
    ClientCredentialsPrivateKeyJwt {
        client_id: String,
        private_key_input: String,
        #[serde(default = "default_jwt_algorithm")]
        algorithm: String,
        #[serde(default)]
        token_endpoint_audience: Option<String>,
    },
}

fn default_jwt_algorithm() -> String {
    "RS256".to_string()
}

fn bearer_insufficient_scope(header: &str) -> Option<String> {
    let challenges = http_auth::parse_challenges(header).ok()?;
    challenges.into_iter().find_map(|challenge| {
        if !challenge.scheme.eq_ignore_ascii_case("bearer") {
            return None;
        }
        let mut insufficient_scope = false;
        let mut scope = None;
        for (name, value) in challenge.params {
            let value = value.to_unescaped();
            if name.eq_ignore_ascii_case("error") && value == "insufficient_scope" {
                insufficient_scope = true;
            } else if name.eq_ignore_ascii_case("scope") && !value.trim().is_empty() {
                scope = Some(value);
            }
        }
        insufficient_scope.then_some(scope).flatten()
    })
}

/// How an authorization-code client is identified.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "registration",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum OAuthClientRegistration {
    Dynamic,
    Preregistered {
        client_id: String,
        #[serde(default)]
        client_secret_input: Option<String>,
    },
    ClientMetadataDocument {
        url: String,
    },
}

/// Parameters supplied by the embedding application when it starts authorization.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OAuthBeginRequest {
    /// Host-owned callback target.
    ///
    /// Accepted forms are HTTPS, loopback HTTP, or a reverse-domain private-use URI such as
    /// `com.example.app:/oauth/callback` for native applications.
    pub redirect_uri: String,
    #[serde(default)]
    pub required_scope: Option<String>,
}

/// Browser launch information. The SDK never opens the browser itself.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OAuthLaunch {
    pub authorization_url: String,
    pub state: String,
}

impl std::fmt::Debug for OAuthLaunch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthLaunch")
            .field("authorization_url", &"[REDACTED]")
            .field("state", &"[REDACTED]")
            .finish()
    }
}

/// Values parsed by the embedding application's callback listener.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OAuthCallback {
    pub code: String,
    pub state: String,
    #[serde(default)]
    pub issuer: Option<String>,
}

impl std::fmt::Debug for OAuthCallback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthCallback")
            .field("code", &"[REDACTED]")
            .field("state", &"[REDACTED]")
            .field("issuer", &self.issuer)
            .finish()
    }
}

/// Host-reported termination of a pending browser authorization.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum OAuthCancellationReason {
    /// The authorization server returned `access_denied`.
    AccessDenied,
    /// The authorization server returned another OAuth error.
    ///
    /// Raw provider error strings and `error_description` values remain outside the SDK so they
    /// cannot be retained or echoed through ordinary error and logging paths.
    AuthorizationError,
    /// The user or embedding application cancelled the flow.
    Cancelled,
    /// The embedding application's callback deadline elapsed.
    Timeout,
}

/// Structured cancellation input for a flow previously started by [`OAuthBeginRequest`].
///
/// The host must return the exact state it received from [`OAuthLaunch`]. Arbitrary provider error
/// descriptions are deliberately excluded so they cannot be retained or echoed by SDK errors.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OAuthCancellation {
    pub state: String,
    #[serde(default)]
    pub issuer: Option<String>,
    pub reason: OAuthCancellationReason,
}

impl std::fmt::Debug for OAuthCancellation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthCancellation")
            .field("state", &"[REDACTED]")
            .field("issuer", &self.issuer)
            .field("reason", &self.reason)
            .finish()
    }
}

/// Observable authorization state for a server.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "state",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum OAuthStatus {
    Unauthorized,
    AuthorizationPending,
    Authorized { scopes: Vec<String> },
    ReauthorizationRequired { required_scope: String },
    Error { message: String },
}

/// Structured result of completing or terminating an interactive authorization flow.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "outcome",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum OAuthFlowOutcome {
    /// The authorization code was exchanged and credentials were stored.
    Authorized { scopes: Vec<String> },
    /// The host or authorization server terminated the flow without replacing credentials.
    ///
    /// `status` can remain [`OAuthStatus::Authorized`] when a scope-upgrade flow was cancelled and
    /// earlier credentials are still usable.
    Terminated {
        reason: OAuthCancellationReason,
        status: OAuthStatus,
    },
}

fn normalize_host_cancellation(
    result: Result<OAuthFlowOutcome, OAuthError>,
    reason: Option<OAuthCancellationReason>,
) -> Result<OAuthFlowOutcome, OAuthError> {
    match (result, reason) {
        (Ok(OAuthFlowOutcome::Terminated { status, .. }), Some(reason)) => {
            Ok(OAuthFlowOutcome::Terminated { reason, status })
        }
        (result, _) => result,
    }
}

/// Stable, non-sensitive category for failures reported by the underlying OAuth protocol stack.
///
/// Provider response bodies, URLs, tokens, client identifiers, and upstream error strings are
/// intentionally discarded at the rmcp boundary. Hosts can use this category for control flow and
/// diagnostics without accidentally exposing authorization-server data through `Display`,
/// `Debug`, or an error source chain.
#[derive(Debug, Clone, Copy, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum OAuthProtocolError {
    #[error("OAuth authorization is required")]
    AuthorizationRequired,
    #[error("OAuth authorization failed")]
    AuthorizationFailed,
    #[error("OAuth token exchange failed")]
    TokenExchangeFailed,
    #[error("OAuth token refresh failed")]
    TokenRefreshFailed,
    #[error("OAuth HTTP request failed")]
    Http,
    #[error("OAuth provider rejected the request")]
    Provider,
    #[error("OAuth metadata validation failed")]
    Metadata,
    #[error("OAuth authorization server does not support PKCE S256")]
    PkceUnsupported,
    #[error("OAuth URL is invalid")]
    InvalidUrl,
    #[error("OAuth authorization is not supported by the server")]
    NoAuthorizationSupport,
    #[error("OAuth internal operation failed")]
    Internal,
    #[error("OAuth token type is invalid")]
    InvalidTokenType,
    #[error("OAuth token has expired")]
    TokenExpired,
    #[error("OAuth scope is invalid")]
    InvalidScope,
    #[error("OAuth client registration failed")]
    RegistrationFailed,
    #[error("OAuth authorization has insufficient scope")]
    InsufficientScope,
    #[error("OAuth authorization server issuer validation failed")]
    IssuerMismatch,
    #[error("OAuth client credentials exchange failed")]
    ClientCredentials,
    #[error("OAuth JWT signing failed")]
    JwtSigning,
    #[error("OAuth protocol operation failed")]
    Other,
}

impl From<RmcpAuthError> for OAuthProtocolError {
    fn from(error: RmcpAuthError) -> Self {
        match error {
            RmcpAuthError::AuthorizationRequired => Self::AuthorizationRequired,
            RmcpAuthError::AuthorizationFailed(_) => Self::AuthorizationFailed,
            RmcpAuthError::TokenExchangeFailed(_) => Self::TokenExchangeFailed,
            RmcpAuthError::TokenRefreshFailed(_) => Self::TokenRefreshFailed,
            RmcpAuthError::HttpError(_) => Self::Http,
            RmcpAuthError::OAuthError(_) => Self::Provider,
            RmcpAuthError::MetadataError(_) => Self::Metadata,
            RmcpAuthError::PkceUnsupported => Self::PkceUnsupported,
            RmcpAuthError::UrlError(_) => Self::InvalidUrl,
            RmcpAuthError::NoAuthorizationSupport => Self::NoAuthorizationSupport,
            RmcpAuthError::InternalError(_) => Self::Internal,
            RmcpAuthError::InvalidTokenType(_) => Self::InvalidTokenType,
            RmcpAuthError::TokenExpired => Self::TokenExpired,
            RmcpAuthError::InvalidScope(_) => Self::InvalidScope,
            RmcpAuthError::RegistrationFailed(_) => Self::RegistrationFailed,
            RmcpAuthError::InsufficientScope { .. } => Self::InsufficientScope,
            RmcpAuthError::AuthorizationServerMismatch { .. }
            | RmcpAuthError::AuthorizationServerMissingIssuer { .. } => Self::IssuerMismatch,
            RmcpAuthError::ClientCredentialsError(_) => Self::ClientCredentials,
            RmcpAuthError::JwtSigningError(_) => Self::JwtSigning,
            _ => Self::Other,
        }
    }
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum OAuthError {
    #[error("server does not have OAuth configured")]
    NotConfigured,
    #[error("OAuth is not supported for this transport")]
    UnsupportedTransport,
    #[error("OAuth callback state does not match the active authorization request")]
    StateMismatch,
    #[error("OAuth callback issuer does not match the active authorization request")]
    IssuerMismatch,
    #[error("OAuth authorization request has expired")]
    AuthorizationExpired,
    #[error("OAuth authorization flow was cancelled")]
    AuthorizationCancelled,
    #[error("OAuth authorization flow did not drain before the lifecycle deadline")]
    DrainTimeout,
    #[error("a different OAuth authorization request is already pending")]
    AuthorizationAlreadyPending,
    #[error("provider cancellation reasons require state and issuer validation")]
    InvalidCancellationReason,
    #[error("OAuth secret input '{0}' was not provided")]
    MissingSecret(String),
    #[error("unsupported JWT signing algorithm '{0}'")]
    UnsupportedSigningAlgorithm(String),
    #[error("invalid OAuth redirect URI: {0}")]
    InvalidRedirectUri(String),
    #[error("OAuth cannot be combined with a static Authorization header")]
    ConflictingAuthorizationHeader,
    #[error("the explicit OAuth policy requires OAuth options")]
    ExplicitPolicyRequiresOptions,
    #[error("the disabled authentication policy cannot contain OAuth options")]
    DisabledPolicyWithOptions,
    #[error("OAuth protocol error: {0}")]
    Protocol(#[from] OAuthProtocolError),
}

impl From<RmcpAuthError> for OAuthError {
    fn from(error: RmcpAuthError) -> Self {
        Self::Protocol(error.into())
    }
}

struct PendingAuthorization {
    launch: OAuthLaunch,
    request: OAuthBeginRequest,
    requested_scopes: Vec<String>,
    generation: u64,
    candidate: SensitiveAuthClient,
    staged_store: StagedCredentialStore,
    metadata: AuthorizationMetadata,
    issuer: String,
}

enum AuthorizationFlowState {
    Idle,
    Pending(Box<PendingAuthorization>),
    /// Retains only the opaque identity needed to classify one late callback.
    ///
    /// Expired flows are terminal and must not participate in active-flow gates for subsequent
    /// protected-resource 401/403 observations.
    Expired {
        state: String,
    },
}

/// Thin rmcp adapter over the host-provided, bundle-aware credential store.
#[derive(Clone)]
struct ScopedCredentialStore {
    bundle_id: BundleId,
    resource: String,
    mode_fingerprint: String,
    issuer: Arc<RwLock<Option<String>>>,
    backend: Arc<dyn OAuthCredentialStore>,
    known_issuers: Arc<StdMutex<HashSet<Option<String>>>>,
    lifecycle: Arc<OAuthResourceLifecycle>,
}

#[derive(Serialize, Deserialize)]
struct StoredCredentialEnvelope {
    version: u8,
    mode_fingerprint: String,
    credentials: StoredCredentials,
}

#[derive(Serialize, Deserialize)]
struct StoredCredentialIndex {
    version: u8,
    issuers: Vec<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    active: Option<StoredActiveCredential>,
}

#[derive(Clone, Serialize, Deserialize)]
struct StoredActiveCredential {
    issuer: Option<String>,
    credentials: StoredCredentials,
}

impl ScopedCredentialStore {
    fn new(
        bundle_id: BundleId,
        resource: String,
        mode_fingerprint: String,
        backend: Arc<dyn OAuthCredentialStore>,
    ) -> Self {
        let lifecycle =
            oauth_resource_lifecycle(&backend, &bundle_id, &resource, &mode_fingerprint);
        Self {
            bundle_id,
            resource,
            mode_fingerprint,
            issuer: Arc::new(RwLock::new(None)),
            backend,
            known_issuers: Arc::new(StdMutex::new(HashSet::from([None]))),
            lifecycle,
        }
    }

    async fn set_issuer(&self, issuer: Option<String>) -> Result<(), RmcpAuthError> {
        self.persist_issuer_index_with(issuer.clone()).await?;
        self.known_issuers
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(issuer.clone());
        *self.issuer.write().await = issuer;
        Ok(())
    }

    fn key_for_issuer(&self, issuer: Option<String>) -> OAuthCredentialKey {
        OAuthCredentialKey {
            bundle_id: self.bundle_id.clone(),
            resource: self.resource.clone(),
            issuer,
            grant_fingerprint: self.mode_fingerprint.clone(),
            record_kind: OAuthCredentialRecordKind::Credentials,
        }
    }

    fn index_key(&self) -> OAuthCredentialKey {
        OAuthCredentialKey {
            bundle_id: self.bundle_id.clone(),
            resource: self.resource.clone(),
            issuer: None,
            grant_fingerprint: self.mode_fingerprint.clone(),
            record_kind: OAuthCredentialRecordKind::IssuerIndex,
        }
    }

    async fn key(&self) -> OAuthCredentialKey {
        self.key_for_issuer(self.issuer.read().await.clone())
    }

    fn backend_error(_: OAuthCredentialStoreError) -> RmcpAuthError {
        RmcpAuthError::InternalError("OAuth credential store operation failed".to_string())
    }

    async fn persisted_index(&self) -> Result<StoredCredentialIndex, RmcpAuthError> {
        let encoded = self
            .backend
            .load(&self.index_key())
            .await
            .map_err(Self::backend_error)?;
        let Some(encoded) = encoded else {
            return Ok(StoredCredentialIndex {
                version: 1,
                issuers: Vec::new(),
                active: None,
            });
        };
        let index = serde_json::from_str::<StoredCredentialIndex>(&encoded)
            .map_err(|_| RmcpAuthError::InternalError("OAuth issuer index is invalid".into()))?;
        if index.version != 1 {
            return Err(RmcpAuthError::InternalError(
                "OAuth issuer index version is unsupported".into(),
            ));
        }
        Ok(index)
    }

    async fn persisted_issuers(&self) -> Result<HashSet<Option<String>>, RmcpAuthError> {
        Ok(self.persisted_index().await?.issuers.into_iter().collect())
    }

    async fn persist_issuer_index_with(
        &self,
        additional: Option<String>,
    ) -> Result<(), RmcpAuthError> {
        let mut index = self.persisted_index().await?;
        let mut issuers: HashSet<Option<String>> = index.issuers.drain(..).collect();
        issuers.extend(
            self.known_issuers
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .iter()
                .cloned(),
        );
        issuers.insert(additional);
        let mut issuers: Vec<Option<String>> = issuers.into_iter().collect();
        issuers.sort();
        let encoded = serde_json::to_string(&StoredCredentialIndex {
            version: 1,
            issuers,
            active: index.active,
        })
        .map_err(|_| RmcpAuthError::InternalError("OAuth issuer index encoding failed".into()))?;
        self.backend
            .save(&self.index_key(), &encoded)
            .await
            .map_err(Self::backend_error)
    }

    /// Commit a fully prepared candidate credential to the durable slot.
    ///
    /// The issuer index may safely contain an unused issuer if the credential write fails. The
    /// active issuer and generation move only after the backend has atomically replaced the
    /// credential key, so a failed scope upgrade continues to address the old credential.
    async fn commit_candidate(
        &self,
        issuer: String,
        credentials: StoredCredentials,
    ) -> Result<(), RmcpAuthError> {
        let mut index = self.persisted_index().await?;
        let mut issuers: HashSet<Option<String>> = index.issuers.drain(..).collect();
        issuers.extend(
            self.known_issuers
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .iter()
                .cloned(),
        );
        issuers.insert(Some(issuer.clone()));
        let mut issuers: Vec<_> = issuers.into_iter().collect();
        issuers.sort();
        let encoded = serde_json::to_string(&StoredCredentialIndex {
            version: 1,
            issuers,
            active: Some(StoredActiveCredential {
                issuer: Some(issuer.clone()),
                credentials,
            }),
        })
        .map_err(|_| RmcpAuthError::InternalError("OAuth issuer index encoding failed".into()))?;
        self.backend
            .save(&self.index_key(), &encoded)
            .await
            .map_err(Self::backend_error)?;
        self.known_issuers
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(Some(issuer.clone()));
        *self.issuer.write().await = Some(issuer);
        self.lifecycle.generation.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    async fn overwrite_granted_scopes(&self, scopes: Vec<String>) -> Result<(), RmcpAuthError> {
        if let Some(mut credentials) = self.load().await? {
            credentials.granted_scopes = scopes;
            self.save(credentials).await?;
        }
        Ok(())
    }

    async fn locally_authorized_status(
        &self,
        options: &OAuthOptions,
    ) -> Result<Option<OAuthStatus>, RmcpAuthError> {
        let index = self.persisted_index().await?;
        let mut candidates = Vec::new();
        if let Some(active) = index.active {
            candidates.push(active.credentials);
        } else {
            for issuer in index.issuers {
                let Some(encoded) = self
                    .backend
                    .load(&self.key_for_issuer(issuer))
                    .await
                    .map_err(Self::backend_error)?
                else {
                    continue;
                };
                if let Ok(envelope) = serde_json::from_str::<StoredCredentialEnvelope>(&encoded) {
                    if envelope.version == 1 && envelope.mode_fingerprint == self.mode_fingerprint {
                        candidates.push(envelope.credentials);
                    }
                }
            }
        }
        let mut authorized = candidates.into_iter().filter(|credentials| {
            credentials.token_response.is_some()
                && stored_client_matches(options, &credentials.client_id)
        });
        let Some(credentials) = authorized.next() else {
            return Ok(None);
        };
        if authorized.next().is_some() {
            return Ok(None);
        }
        let scopes = if credentials.granted_scopes.is_empty() {
            options.scopes.clone()
        } else {
            credentials.granted_scopes
        };
        Ok(Some(OAuthStatus::Authorized { scopes }))
    }
}

pub(crate) async fn locally_stored_oauth_status(
    bundle_id: BundleId,
    resource: String,
    options: &OAuthOptions,
    backend: Arc<dyn OAuthCredentialStore>,
) -> Result<Option<OAuthStatus>, OAuthError> {
    let store = ScopedCredentialStore::new(
        bundle_id,
        canonical_resource_identity(&resource)?,
        oauth_mode_fingerprint(options),
        backend,
    );
    store
        .locally_authorized_status(options)
        .await
        .map_err(OAuthError::from)
}

#[async_trait]
impl CredentialStore for ScopedCredentialStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, RmcpAuthError> {
        let issuer = self.issuer.read().await.clone();
        let index = self.persisted_index().await?;
        if let Some(active) = index.active {
            if active.issuer == issuer {
                return Ok(Some(active.credentials));
            }
        }
        let key = self.key().await;
        let encoded = self.backend.load(&key).await.map_err(Self::backend_error)?;
        let Some(encoded) = encoded else {
            return Ok(None);
        };
        match serde_json::from_str::<StoredCredentialEnvelope>(&encoded) {
            Ok(envelope)
                if envelope.version == 1 && envelope.mode_fingerprint == self.mode_fingerprint =>
            {
                Ok(Some(envelope.credentials))
            }
            Ok(_) => {
                self.backend
                    .delete(&key)
                    .await
                    .map_err(Self::backend_error)?;
                Ok(None)
            }
            Err(_) => Err(RmcpAuthError::InternalError(
                "stored OAuth credentials are invalid".to_string(),
            )),
        }
    }

    async fn save(&self, mut credentials: StoredCredentials) -> Result<(), RmcpAuthError> {
        // rmcp 2.2 restores the token but not its current_scopes field. A refresh response
        // may omit `scope`; preserve the last grant instead of overwriting it with empty.
        if credentials.granted_scopes.is_empty() {
            if let Some(existing) = self.load().await? {
                credentials.granted_scopes = existing.granted_scopes;
            }
        }
        let issuer = self.issuer.read().await.clone();
        let mut index = self.persisted_index().await?;
        let mut issuers: HashSet<Option<String>> = index.issuers.drain(..).collect();
        issuers.insert(issuer.clone());
        let mut issuers: Vec<_> = issuers.into_iter().collect();
        issuers.sort();
        let encoded = serde_json::to_string(&StoredCredentialIndex {
            version: 1,
            issuers,
            active: Some(StoredActiveCredential {
                issuer,
                credentials,
            }),
        })
        .map_err(|_| RmcpAuthError::InternalError("OAuth issuer index encoding failed".into()))?;
        self.backend
            .save(&self.index_key(), &encoded)
            .await
            .map_err(Self::backend_error)?;
        self.lifecycle.generation.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    async fn clear(&self) -> Result<(), RmcpAuthError> {
        let mut issuers = self
            .known_issuers
            .lock()
            .expect("OAuth issuer registry poisoned")
            .clone();
        issuers.extend(self.persisted_issuers().await?);
        for issuer in issuers {
            self.backend
                .delete(&self.key_for_issuer(issuer))
                .await
                .map_err(Self::backend_error)?;
        }
        self.backend
            .delete(&self.index_key())
            .await
            .map_err(Self::backend_error)?;
        self.lifecycle.generation.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
}

pub(crate) async fn clear_stored_oauth_credentials(
    bundle_id: &BundleId,
    resource: &str,
    options: &OAuthOptions,
    credential_store: Arc<dyn OAuthCredentialStore>,
) -> Result<(), OAuthError> {
    let resource = canonical_resource_identity(resource)?;
    let store = ScopedCredentialStore::new(
        bundle_id.clone(),
        resource,
        oauth_mode_fingerprint(options),
        credential_store,
    );
    let _request_guard = store.lifecycle.request_gate.write().await;
    // `clear` filters the resource locator to this OAuth mode/client/scope slot.
    store.clear().await?;
    Ok(())
}

/// Per-server OAuth state machine shared by the manager and HTTP transport.
pub(crate) struct OAuthCoordinator {
    resource: String,
    options: OAuthOptions,
    client: SensitiveAuthClient,
    store: ScopedCredentialStore,
    resolver: Option<Arc<dyn SecretValueResolver>>,
    status: OAuthStatusState,
    granted_scopes: Arc<RwLock<Vec<String>>>,
    authorization_flow: Arc<Mutex<AuthorizationFlowState>>,
    machine_scope_upgrades: Mutex<HashMap<String, usize>>,
    machine_scope_upgrade_gate: Arc<Mutex<()>>,
    request_gate: Arc<RwLock<()>>,
    state_store: ExpiringStateStore,
    oauth_http_client: Arc<dyn OAuthHttpClient>,
    transport_http_client: reqwest::Client,
}

pub(crate) struct OAuthCoordinatorContext {
    bundle_id: BundleId,
    credential_store: Arc<dyn OAuthCredentialStore>,
    events: Option<Arc<RuntimeStatus>>,
    admitted_resource_metadata_url: Option<Url>,
}

impl OAuthCoordinatorContext {
    pub(crate) fn new(
        bundle_id: BundleId,
        credential_store: Arc<dyn OAuthCredentialStore>,
        events: Option<Arc<RuntimeStatus>>,
    ) -> Self {
        Self {
            bundle_id,
            credential_store,
            events,
            admitted_resource_metadata_url: None,
        }
    }

    pub(crate) fn with_admitted_resource_metadata_url(mut self, url: Option<Url>) -> Self {
        self.admitted_resource_metadata_url = url;
        self
    }
}

#[derive(Clone)]
struct OAuthStatusState {
    current: Arc<RwLock<OAuthStatus>>,
    bundle_id: BundleId,
    events: Option<Arc<RuntimeStatus>>,
}

impl OAuthStatusState {
    fn new(bundle_id: BundleId, initial: OAuthStatus, events: Option<Arc<RuntimeStatus>>) -> Self {
        if let Some(events) = events.as_ref() {
            events.update_oauth_status(bundle_id.clone(), initial.clone());
        }
        Self {
            current: Arc::new(RwLock::new(initial)),
            bundle_id,
            events,
        }
    }

    async fn get(&self) -> OAuthStatus {
        self.current.read().await.clone()
    }

    async fn set(&self, next: OAuthStatus) {
        let mut current = self.current.write().await;
        if *current == next {
            return;
        }
        *current = next.clone();
        // Publish while the state lock still serializes writers so concurrent transitions cannot
        // reach the host event stream in the reverse order from the queryable coordinator state.
        if let Some(events) = self.events.as_ref() {
            events.update_oauth_status(self.bundle_id.clone(), next);
        }
    }
}

pub(crate) struct OAuthRequestGuard {
    generation: u64,
    _guard: OwnedRwLockReadGuard<()>,
}

impl OAuthRequestGuard {
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }
}

impl OAuthCoordinator {
    pub(crate) async fn new(
        context: OAuthCoordinatorContext,
        mcp_endpoint: &str,
        resource: &str,
        options: OAuthOptions,
        resolver: Option<Arc<dyn SecretValueResolver>>,
        http_client: reqwest::Client,
        protected_resource_headers: HeaderMap,
    ) -> Result<Self, OAuthError> {
        let admitted_resource_metadata =
            context.admitted_resource_metadata_url.clone().map(|url| {
                let validated = Arc::new(AtomicBool::new(false));
                (
                    AdmittedResourceMetadata {
                        url,
                        resource: resource.to_string(),
                        validated: Arc::clone(&validated),
                    },
                    validated,
                )
            });
        let admitted_resource_metadata_validated = admitted_resource_metadata
            .as_ref()
            .map(|(_, validated)| Arc::clone(validated));
        let oauth_http_client = Arc::new(
            DiscoveryCleanupOAuthHttpClient::with_protected_resource_headers(
                mcp_endpoint,
                protected_resource_headers,
                admitted_resource_metadata.map(|(metadata, _)| metadata),
            )?,
        );
        Self::new_with_oauth_http_client_and_admission(
            context,
            resource,
            options,
            resolver,
            http_client,
            oauth_http_client,
            admitted_resource_metadata_validated,
        )
        .await
    }

    #[cfg(test)]
    async fn new_with_oauth_http_client(
        context: OAuthCoordinatorContext,
        resource: &str,
        options: OAuthOptions,
        resolver: Option<Arc<dyn SecretValueResolver>>,
        http_client: reqwest::Client,
        oauth_http_client: Arc<dyn OAuthHttpClient>,
    ) -> Result<Self, OAuthError> {
        Self::new_with_oauth_http_client_and_admission(
            context,
            resource,
            options,
            resolver,
            http_client,
            oauth_http_client,
            None,
        )
        .await
    }

    async fn new_with_oauth_http_client_and_admission(
        context: OAuthCoordinatorContext,
        resource: &str,
        options: OAuthOptions,
        resolver: Option<Arc<dyn SecretValueResolver>>,
        http_client: reqwest::Client,
        oauth_http_client: Arc<dyn OAuthHttpClient>,
        admitted_resource_metadata_validated: Option<Arc<AtomicBool>>,
    ) -> Result<Self, OAuthError> {
        let OAuthCoordinatorContext {
            bundle_id,
            credential_store,
            events,
            admitted_resource_metadata_url: _,
        } = context;
        let resource = canonical_resource_identity(resource)?;
        let store = ScopedCredentialStore::new(
            bundle_id.clone(),
            resource.clone(),
            oauth_mode_fingerprint(&options),
            credential_store,
        );
        let request_gate = Arc::clone(&store.lifecycle.request_gate);
        let initialization_guard = request_gate.write().await;
        let mut manager = AuthorizationManager::new_with_oauth_http_client(
            &resource,
            Arc::clone(&oauth_http_client),
        )
        .await?;
        manager.set_credential_store(store.clone());
        let state_store = ExpiringStateStore::new(AUTHORIZATION_STATE_TTL);
        manager.set_state_store(state_store.clone());
        let metadata = manager.discover_metadata().await?;
        if let Some(validated) = admitted_resource_metadata_validated.as_ref() {
            // rmcp 2.2 intentionally falls back to derived legacy endpoints when RFC 8414/OIDC
            // discovery fails. That is useful for proactive compatibility but is not evidence
            // strong enough to admit Auto OAuth. Both standardized discovery documents must have
            // succeeded, and RFC 8414/OIDC metadata requires an issuer.
            if !validated.load(Ordering::Acquire) || metadata.issuer.is_none() {
                return Err(OAuthError::Protocol(OAuthProtocolError::Metadata));
            }
        }
        validate_authorization_metadata(
            &metadata,
            matches!(options.mode, OAuthClientMode::AuthorizationCode { .. }),
        )?;
        store
            .set_issuer(Some(authorization_server_credential_identity(&metadata)?))
            .await?;
        manager.set_metadata(metadata);
        let mut stored = store.load().await?;
        if stored
            .as_ref()
            .is_some_and(|credentials| !stored_client_matches(&options, &credentials.client_id))
        {
            store.clear().await?;
            stored = None;
        }
        let restored = stored
            .as_ref()
            .is_some_and(|credentials| credentials.token_response.is_some());
        let mut restored_scopes = stored
            .as_ref()
            .map(|credentials| credentials.granted_scopes.clone())
            .unwrap_or_default();
        if restored && restored_scopes.is_empty() {
            restored_scopes = options.scopes.clone();
        }
        if restored {
            manager.initialize_from_store().await?;
            configure_restored_authorization_client(
                &mut manager,
                &resource,
                &options,
                &restored_scopes,
                stored
                    .as_ref()
                    .map(|credentials| credentials.client_id.as_str())
                    .expect("restored credentials were checked above"),
                resolver.as_ref(),
            )
            .await?;
        }
        let initial_status = if restored {
            match manager
                .get_access_token()
                .with_subscriber(tracing::subscriber::NoSubscriber::default())
                .await
            {
                Ok(_) => {
                    if let Some(credentials) = store.load().await? {
                        if !credentials.granted_scopes.is_empty() {
                            restored_scopes = credentials.granted_scopes;
                        }
                    }
                    OAuthStatus::Authorized {
                        scopes: restored_scopes.clone(),
                    }
                }
                Err(_) => OAuthStatus::Error {
                    message: "stored OAuth credential could not be refreshed".to_string(),
                },
            }
        } else {
            OAuthStatus::Unauthorized
        };
        let client = SensitiveAuthClient::new(AuthClient::new(http_client.clone(), manager));
        drop(initialization_guard);
        let status = OAuthStatusState::new(bundle_id, initial_status, events);
        Ok(Self {
            resource,
            options,
            client,
            store,
            resolver,
            status,
            granted_scopes: Arc::new(RwLock::new(restored_scopes)),
            authorization_flow: Arc::new(Mutex::new(AuthorizationFlowState::Idle)),
            machine_scope_upgrades: Mutex::new(HashMap::new()),
            machine_scope_upgrade_gate: Arc::new(Mutex::new(())),
            request_gate,
            state_store,
            oauth_http_client,
            transport_http_client: http_client,
        })
    }

    pub(crate) fn http_client(&self) -> SensitiveAuthClient {
        self.client.clone()
    }

    pub(crate) fn credential_generation(&self) -> u64 {
        self.store.lifecycle.generation.load(Ordering::Acquire)
    }

    pub(crate) async fn drive_flow(self: Arc<Self>, mut driver: OAuthFlowDriver) {
        let cancellation = driver.cancellation();
        let terminal = driver.terminal();
        let request = driver.request().clone();
        let begin = self.begin_with_cancellation(request, cancellation.clone());
        tokio::pin!(begin);
        let begin_result = tokio::select! {
            biased;
            result = &mut begin => Some(result),
            _ = cancellation.cancelled() => None,
        };

        let launch = match begin_result {
            Some(Ok(launch)) if !cancellation.is_cancelled() => launch,
            Some(Err(error)) if !cancellation.is_cancelled() => {
                let result = self
                    .finalize_non_retryable_result(Err(error), &terminal)
                    .await;
                driver.finish(result);
                return;
            }
            _ => {
                let result = self
                    .finish_claimed_cancellation(driver.host_cancellation_reason())
                    .await;
                driver.finish(result);
                return;
            }
        };
        let expected_issuer = match self.state_store.load(&launch.state).await {
            Ok(Some(state)) => state.expected_issuer,
            Ok(None) => {
                driver.finish(Err(OAuthError::AuthorizationExpired));
                return;
            }
            Err(error) => {
                driver.finish(Err(error.into()));
                return;
            }
        };
        if !driver.publish_launch(Ok(launch.clone()), expected_issuer) {
            let result = self
                .finish_claimed_cancellation(driver.host_cancellation_reason())
                .await;
            driver.finish(result);
            return;
        }

        let result = loop {
            tokio::select! {
                biased;
                command = driver.next_command() => {
                    match command {
                        Some(OAuthFlowCommand::Complete { callback, response }) => {
                            let result = self
                                .complete_with_cancellation(
                                    callback,
                                    cancellation.clone(),
                                    Some(terminal.clone()),
                                )
                                .await;
                            let retryable = matches!(
                                result,
                                Err(OAuthError::StateMismatch | OAuthError::IssuerMismatch)
                            );
                            if retryable {
                                let _ = response.send(result);
                                continue;
                            }
                            let result = self
                                .finalize_non_retryable_result(result, &terminal)
                                .await;
                            driver.finish(result.clone());
                            let _ = response.send(result.clone());
                            break result;
                        }
                        Some(OAuthFlowCommand::CancelCallback { cancellation, response }) => {
                            let result = self
                                .cancel_with_terminal(cancellation, Some(terminal.clone()))
                                .await;
                            let retryable = matches!(
                                result,
                                Err(OAuthError::StateMismatch | OAuthError::IssuerMismatch)
                            );
                            if retryable {
                                let _ = response.send(result);
                                continue;
                            }
                            driver.finish(result.clone());
                            let _ = response.send(result.clone());
                            break result;
                        }
                        None => {
                            break Err(OAuthError::AuthorizationExpired);
                        }
                    }
                }
                _ = cancellation.cancelled() => {
                    let reason = driver.host_cancellation_reason();
                    let result = self
                        .cancel_with_terminal(OAuthCancellation {
                            state: launch.state.clone(),
                            issuer: None,
                            reason,
                        }, Some(terminal.clone()))
                        .await;
                    break normalize_host_cancellation(result, Some(reason));
                }
            }
        };
        driver.finish(result);
    }

    pub(crate) async fn prepare_request(&self) -> Result<OAuthRequestGuard, OAuthError> {
        if !matches!(self.options.mode, OAuthClientMode::AuthorizationCode { .. }) {
            self.ensure_machine_authorized().await?;
        }
        let guard = Arc::clone(&self.request_gate).read_owned().await;
        let manager = self.client.inner.auth_manager.lock().await;
        let access_token = manager
            .get_access_token()
            .with_subscriber(tracing::subscriber::NoSubscriber::default())
            .await;
        drop(manager);
        if let Err(error) = access_token {
            if matches!(&error, RmcpAuthError::TokenRefreshFailed(_)) {
                self.status
                    .set(OAuthStatus::Error {
                        message: "OAuth access token refresh failed".to_string(),
                    })
                    .await;
            }
            return Err(error.into());
        }
        Ok(OAuthRequestGuard {
            generation: self.credential_generation(),
            _guard: guard,
        })
    }

    pub(crate) async fn status(&self) -> OAuthStatus {
        let _request_guard = self.request_gate.write().await;
        if matches!(self.status.get().await, OAuthStatus::AuthorizationPending) {
            let _ = self.expire_invalid_authorization_flow().await;
        }
        if matches!(
            self.status.get().await,
            OAuthStatus::Authorized { .. } | OAuthStatus::Error { .. }
        ) {
            let access_token = self
                .client
                .inner
                .auth_manager
                .lock()
                .await
                .get_access_token()
                .with_subscriber(tracing::subscriber::NoSubscriber::default())
                .await;
            match access_token {
                Ok(_) => match self.store.load().await {
                    Ok(Some(credentials)) => {
                        let scopes = if credentials.granted_scopes.is_empty() {
                            self.granted_scopes.read().await.clone()
                        } else {
                            credentials.granted_scopes
                        };
                        *self.granted_scopes.write().await = scopes.clone();
                        self.status.set(OAuthStatus::Authorized { scopes }).await;
                    }
                    Ok(None) | Err(_) => {
                        self.status
                            .set(OAuthStatus::Error {
                                message: "OAuth credential state is unavailable".to_string(),
                            })
                            .await;
                    }
                },
                Err(_) => {
                    self.status
                        .set(OAuthStatus::Error {
                            message: "OAuth access token is unavailable".to_string(),
                        })
                        .await;
                }
            }
        }
        self.status.get().await
    }

    async fn resolve_secret(&self, id: &str) -> Result<String, OAuthError> {
        resolve_secret_from(self.resolver.as_ref(), id).await
    }

    /// Configure and exchange a client-credentials grant when that mode is selected.
    pub(crate) async fn ensure_machine_authorized(&self) -> Result<(), OAuthError> {
        let _request_guard = self.request_gate.write().await;
        if !matches!(self.options.mode, OAuthClientMode::AuthorizationCode { .. }) {
            let manager = self.client.inner.auth_manager.lock().await;
            let token_available = manager
                .get_access_token()
                .with_subscriber(tracing::subscriber::NoSubscriber::default())
                .await
                .is_ok();
            let scopes = match self.store.load().await? {
                Some(credentials) if !credentials.granted_scopes.is_empty() => {
                    credentials.granted_scopes
                }
                _ => self.granted_scopes.read().await.clone(),
            };
            let has_required_scopes = self
                .options
                .scopes
                .iter()
                .all(|required| scopes.iter().any(|granted| granted == required));
            if token_available && has_required_scopes {
                *self.granted_scopes.write().await = scopes.clone();
                if !matches!(
                    self.status.get().await,
                    OAuthStatus::ReauthorizationRequired { .. }
                ) {
                    self.status.set(OAuthStatus::Authorized { scopes }).await;
                }
                return Ok(());
            }
            drop(manager);
            let mut requested_scopes = scopes;
            for required in &self.options.scopes {
                if !requested_scopes.iter().any(|scope| scope == required) {
                    requested_scopes.push(required.clone());
                }
            }
            let was_authorized = matches!(
                self.status.get().await,
                OAuthStatus::Authorized { .. } | OAuthStatus::ReauthorizationRequired { .. }
            );
            let result = self.exchange_machine_with_scopes(requested_scopes).await;
            if result.is_err() {
                self.status
                    .set(OAuthStatus::Error {
                        message: if was_authorized {
                            "OAuth access token refresh failed".to_string()
                        } else {
                            "OAuth client credentials exchange failed".to_string()
                        },
                    })
                    .await;
            }
            return result;
        }
        Ok(())
    }

    async fn exchange_machine_with_scopes(
        &self,
        requested_scopes: Vec<String>,
    ) -> Result<(), OAuthError> {
        let config = match &self.options.mode {
            OAuthClientMode::ClientCredentialsPrivateKeyJwt {
                client_id,
                private_key_input,
                algorithm,
                token_endpoint_audience,
            } => ClientCredentialsConfig::PrivateKeyJwt {
                client_id: client_id.clone(),
                signing_key: self.resolve_secret(private_key_input).await?.into_bytes(),
                signing_algorithm: parse_algorithm(algorithm)?,
                token_endpoint_audience: token_endpoint_audience.clone(),
                scopes: requested_scopes.clone(),
                resource: Some(self.resource.clone()),
            },
            OAuthClientMode::ClientCredentialsSecret {
                client_id,
                client_secret_input,
            } => ClientCredentialsConfig::ClientSecret {
                client_id: client_id.clone(),
                client_secret: self.resolve_secret(client_secret_input).await?,
                scopes: requested_scopes.clone(),
                resource: Some(self.resource.clone()),
            },
            OAuthClientMode::AuthorizationCode { .. } => return Ok(()),
        };
        let manager = spawn_client_credentials_exchange(
            self.resource.clone(),
            self.store.clone(),
            Arc::clone(&self.oauth_http_client),
            config,
        )
        .await
        .map_err(|_| OAuthError::Protocol(OAuthProtocolError::Internal))?
        .map_err(|_| OAuthError::Protocol(OAuthProtocolError::ClientCredentials))?;
        let mut scopes = manager.get_current_scopes().await;
        if scopes.is_empty() {
            scopes = requested_scopes;
        }
        self.store.overwrite_granted_scopes(scopes.clone()).await?;
        *self.granted_scopes.write().await = scopes.clone();
        *self.client.inner.auth_manager.lock().await = manager;
        self.status.set(OAuthStatus::Authorized { scopes }).await;
        Ok(())
    }

    async fn handle_insufficient_scope(&self, required_scope: String, expected_generation: u64) {
        let reauthorization = OAuthStatus::ReauthorizationRequired {
            required_scope: required_scope.clone(),
        };
        let _request_guard = self.request_gate.write().await;
        let _upgrade_guard = self.machine_scope_upgrade_gate.lock().await;
        if self.credential_generation() != expected_generation {
            return;
        }
        if self.expire_invalid_authorization_flow().await.is_err() {
            return;
        }
        if matches!(
            *self.authorization_flow.lock().await,
            AuthorizationFlowState::Pending(_)
        ) {
            return;
        }
        if matches!(self.options.mode, OAuthClientMode::AuthorizationCode { .. }) {
            self.status.set(reauthorization).await;
            return;
        }
        let normalized_scopes = normalize_required_scopes(&required_scope);
        if normalized_scopes.is_empty() {
            self.status.set(reauthorization).await;
            return;
        }
        self.status.set(reauthorization.clone()).await;
        let reserved = {
            let mut attempts = self.machine_scope_upgrades.lock().await;
            reserve_machine_scope_upgrade(&mut attempts, &required_scope)
        };
        let Some(required_scopes) = reserved else {
            return;
        };
        let mut scopes = match self.store.load().await {
            Ok(Some(credentials)) if !credentials.granted_scopes.is_empty() => {
                credentials.granted_scopes
            }
            _ => self.granted_scopes.read().await.clone(),
        };
        for required in required_scopes {
            if !scopes.iter().any(|granted| granted == &required) {
                scopes.push(required);
            }
        }
        let _ = self.exchange_machine_with_scopes(scopes).await;
        // A token endpoint accepting the expanded scope does not prove that the
        // protected resource will accept it. Keep the token provisional until a
        // subsequent MCP request succeeds against the resource server.
        self.status.set(reauthorization).await;
    }

    #[cfg(test)]
    pub(crate) async fn begin(
        &self,
        request: OAuthBeginRequest,
    ) -> Result<OAuthLaunch, OAuthError> {
        self.begin_with_cancellation(request, CancellationToken::new())
            .await
    }

    async fn begin_with_cancellation(
        &self,
        request: OAuthBeginRequest,
        cancellation: CancellationToken,
    ) -> Result<OAuthLaunch, OAuthError> {
        let OAuthClientMode::AuthorizationCode { registration } = &self.options.mode else {
            return Err(OAuthError::UnsupportedTransport);
        };
        validate_redirect_uri(&request.redirect_uri)?;
        let _request_guard = self.request_gate.write().await;
        let _lifecycle_guard = self.machine_scope_upgrade_gate.lock().await;
        self.expire_invalid_authorization_flow().await?;
        let mut authorization_flow = self.authorization_flow.lock().await;
        if let AuthorizationFlowState::Pending(existing) = &*authorization_flow {
            return if existing.request == request {
                Ok(existing.launch.clone())
            } else {
                Err(OAuthError::AuthorizationAlreadyPending)
            };
        }
        let staged_store = StagedCredentialStore::default();
        let cancellable_http_client: Arc<dyn OAuthHttpClient> =
            Arc::new(CancellableOAuthHttpClient::new(
                Arc::clone(&self.oauth_http_client),
                cancellation.clone(),
            ));
        let mut manager = AuthorizationManager::new_with_oauth_http_client(
            &self.resource,
            cancellable_http_client,
        )
        .await?;
        manager.set_credential_store(staged_store.clone());
        manager.set_state_store(self.state_store.clone());
        let metadata = manager.discover_metadata().await?;
        validate_authorization_metadata(&metadata, true)?;
        let issuer = authorization_server_credential_identity(&metadata)?;
        manager.set_metadata(metadata.clone());
        // Issue #176: when no explicit scopes are configured, adopt the discovered scope set
        // via rmcp's MCP-aligned selection (401 WWW-Authenticate → PRM scopes_supported → AS
        // scopes_supported). rmcp may append `offline_access` when the AS advertises it; this
        // is intentional for MCP refresh-token flows and only applies to the auto-derived path
        // (explicitly configured scopes are respected as-is). `select_scopes` also includes the
        // AS-metadata tier, which MCP 2026-07-28 strictly excludes — accepted here because rmcp
        // keeps its PRM-derived scope fields private and the AS tier is safer than requesting no
        // scope at all (the bug). Must run after `set_metadata` so the AS tier is visible.
        let effective_scopes: Vec<String> = if self.options.scopes.is_empty() {
            manager.select_scopes(None, &[])
        } else {
            self.options.scopes.clone()
        };
        let effective_scope_refs: Vec<&str> = effective_scopes.iter().map(String::as_str).collect();
        if effective_scopes.is_empty() {
            tracing::warn!(
                "OAuth authorization will request no scope: configuration omitted scopes and \
                 discovery found none in the 401 challenge, protected resource metadata, or \
                 authorization server metadata; business tools may return 401 scope errors"
            );
        }
        match registration {
            OAuthClientRegistration::Dynamic => {
                manager
                    .register_client(
                        self.options
                            .client_name
                            .as_deref()
                            .unwrap_or("A2C Computer"),
                        &request.redirect_uri,
                        &effective_scope_refs,
                    )
                    .await
                    .map_err(|_| OAuthError::Protocol(OAuthProtocolError::RegistrationFailed))?;
            }
            OAuthClientRegistration::Preregistered {
                client_id,
                client_secret_input,
            } => {
                let mut config = OAuthClientConfig::new(client_id, &request.redirect_uri)
                    .with_scopes(effective_scopes.clone());
                if let Some(input) = client_secret_input {
                    config = config.with_client_secret(self.resolve_secret(input).await?);
                }
                manager.configure_client(config)?;
            }
            OAuthClientRegistration::ClientMetadataDocument { url } => {
                let valid_url = Url::parse(url).ok().is_some_and(|url| {
                    url.scheme() == "https" && url.host_str().is_some() && url.path() != "/"
                });
                if !valid_url {
                    return Err(OAuthError::Protocol(OAuthProtocolError::RegistrationFailed));
                }
                let supported = metadata
                    .additional_fields
                    .get("client_id_metadata_document_supported")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                if !supported {
                    return Err(OAuthError::Protocol(OAuthProtocolError::RegistrationFailed));
                }
                manager.configure_client(
                    OAuthClientConfig::new(url, &request.redirect_uri)
                        .with_scopes(effective_scopes.clone()),
                )?;
            }
        }
        let requested_scopes = if let Some(required) = request.required_scope.as_deref() {
            let mut upgraded = match self.store.load().await? {
                Some(credentials) if !credentials.granted_scopes.is_empty() => {
                    credentials.granted_scopes
                }
                _ => self.granted_scopes.read().await.clone(),
            };
            for scope in required.split_whitespace() {
                if !upgraded.iter().any(|current| current == scope) {
                    upgraded.push(scope.to_string());
                }
            }
            upgraded
        } else {
            effective_scopes.clone()
        };
        let requested_scope_refs: Vec<&str> = requested_scopes.iter().map(String::as_str).collect();
        let authorization_url = manager.get_authorization_url(&requested_scope_refs).await?;
        let state = Url::parse(&authorization_url)
            .ok()
            .and_then(|url| {
                url.query_pairs()
                    .find(|(key, _)| key == "state")
                    .map(|(_, value)| value.into_owned())
            })
            .ok_or(OAuthError::Protocol(OAuthProtocolError::Internal))?;
        let launch = OAuthLaunch {
            authorization_url,
            state,
        };
        let generation = self
            .store
            .lifecycle
            .generation
            .fetch_add(1, Ordering::AcqRel)
            + 1;
        *authorization_flow = AuthorizationFlowState::Pending(Box::new(PendingAuthorization {
            launch: launch.clone(),
            request,
            requested_scopes,
            generation,
            candidate: SensitiveAuthClient::new(AuthClient::new(
                self.transport_http_client.clone(),
                manager,
            )),
            staged_store,
            metadata,
            issuer,
        }));
        self.status.set(OAuthStatus::AuthorizationPending).await;
        if cancellation.is_cancelled() {
            let state = launch.state.clone();
            *authorization_flow = AuthorizationFlowState::Idle;
            drop(authorization_flow);
            self.state_store.delete(&state).await?;
            self.restore_status_after_termination().await?;
            return Err(OAuthError::AuthorizationCancelled);
        }
        Ok(launch)
    }

    fn validate_callback_issuer(
        stored_state: &StoredAuthorizationState,
        issuer: Option<&str>,
        require_provider_issuer: bool,
    ) -> Result<(), OAuthError> {
        let Some(callback_issuer) = issuer else {
            return if require_provider_issuer && stored_state.require_issuer {
                Err(OAuthError::IssuerMismatch)
            } else {
                Ok(())
            };
        };
        let Some(expected_issuer) = stored_state.expected_issuer.as_deref() else {
            return Err(OAuthError::IssuerMismatch);
        };
        validate_secure_url(callback_issuer, "authorization issuer")
            .map_err(|_| OAuthError::IssuerMismatch)?;
        validate_secure_url(expected_issuer, "authorization issuer")
            .map_err(|_| OAuthError::IssuerMismatch)?;
        (callback_issuer == expected_issuer)
            .then_some(())
            .ok_or(OAuthError::IssuerMismatch)
    }

    async fn expire_invalid_authorization_flow(&self) -> Result<bool, OAuthError> {
        let mut authorization_flow = self.authorization_flow.lock().await;
        let AuthorizationFlowState::Pending(active) = &*authorization_flow else {
            return Ok(false);
        };
        let state_is_valid = active.generation == self.credential_generation()
            && self.state_store.load(&active.launch.state).await?.is_some();
        if state_is_valid {
            return Ok(false);
        }

        let state = active.launch.state.clone();
        self.state_store.delete(&state).await?;
        *authorization_flow = AuthorizationFlowState::Expired { state };
        drop(authorization_flow);
        self.restore_status_after_termination().await?;
        Ok(true)
    }

    async fn restore_status_after_termination(&self) -> Result<OAuthStatus, OAuthError> {
        restore_authorization_status(
            &self.store,
            &self.options.scopes,
            &self.granted_scopes,
            &self.status,
        )
        .await
    }

    async fn restore_after_failed_authorization(&self, state: &str) -> Result<(), OAuthError> {
        self.state_store.delete(state).await?;
        let restored = self.restore_status_after_termination().await?;
        if !matches!(restored, OAuthStatus::Authorized { .. }) {
            self.status
                .set(OAuthStatus::Error {
                    message: "OAuth authorization code exchange failed".to_string(),
                })
                .await;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) async fn complete(
        &self,
        callback: OAuthCallback,
    ) -> Result<OAuthFlowOutcome, OAuthError> {
        self.complete_with_cancellation(callback, CancellationToken::new(), None)
            .await
    }

    async fn finish_claimed_cancellation(
        &self,
        reason: OAuthCancellationReason,
    ) -> Result<OAuthFlowOutcome, OAuthError> {
        let state = {
            let mut flow = self.authorization_flow.lock().await;
            let state = match &*flow {
                AuthorizationFlowState::Pending(active) => Some(active.launch.state.clone()),
                AuthorizationFlowState::Expired { state } => Some(state.clone()),
                AuthorizationFlowState::Idle => None,
            };
            *flow = AuthorizationFlowState::Idle;
            state
        };
        let mut cleanup_failed = false;
        if let Some(state) = state {
            cleanup_failed = self.state_store.delete(&state).await.is_err();
        }
        let status = match self.restore_status_after_termination().await {
            Ok(status) if !cleanup_failed => status,
            Ok(_) | Err(_) => {
                let status = OAuthStatus::Error {
                    message: "OAuth cancellation cleanup failed".to_string(),
                };
                self.status.set(status.clone()).await;
                status
            }
        };
        Ok(OAuthFlowOutcome::Terminated { reason, status })
    }

    async fn finalize_non_retryable_result(
        &self,
        result: Result<OAuthFlowOutcome, OAuthError>,
        terminal: &OAuthFlowTerminal,
    ) -> Result<OAuthFlowOutcome, OAuthError> {
        if let Some(reason) = terminal.claim_non_cancellation_or_reason() {
            self.finish_claimed_cancellation(reason).await
        } else {
            result
        }
    }

    async fn complete_with_cancellation(
        &self,
        callback: OAuthCallback,
        cancellation: CancellationToken,
        terminal: Option<OAuthFlowTerminal>,
    ) -> Result<OAuthFlowOutcome, OAuthError> {
        let request_guard = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                return self.finish_claimed_cancellation(OAuthCancellationReason::Cancelled).await;
            }
            guard = Arc::clone(&self.request_gate).write_owned() => guard,
        };
        let credential_guard = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                drop(request_guard);
                return self.finish_claimed_cancellation(OAuthCancellationReason::Cancelled).await;
            }
            guard = Arc::clone(&self.machine_scope_upgrade_gate).lock_owned() => guard,
        };
        let _request_guard = request_guard;
        let _credential_guard = credential_guard;
        self.complete_after_gates(callback, cancellation, terminal)
            .await
    }

    async fn complete_after_gates(
        &self,
        callback: OAuthCallback,
        cancellation: CancellationToken,
        terminal: Option<OAuthFlowTerminal>,
    ) -> Result<OAuthFlowOutcome, OAuthError> {
        let mut authorization_flow = self.authorization_flow.lock().await;
        if let AuthorizationFlowState::Expired { state } = &*authorization_flow {
            if state != &callback.state {
                return Err(OAuthError::StateMismatch);
            }
            *authorization_flow = AuthorizationFlowState::Idle;
            return Err(OAuthError::AuthorizationExpired);
        }
        let AuthorizationFlowState::Pending(active) = &*authorization_flow else {
            return Err(OAuthError::StateMismatch);
        };
        if active.launch.state != callback.state {
            return Err(OAuthError::StateMismatch);
        }
        let stored_state = if active.generation == self.credential_generation() {
            self.state_store.claim_for_exchange(&callback.state).await
        } else {
            None
        };
        if active.generation != self.credential_generation() || stored_state.is_none() {
            let callback_state = callback.state.clone();
            *authorization_flow = AuthorizationFlowState::Idle;
            drop(authorization_flow);
            self.state_store.delete(&callback_state).await?;
            self.restore_status_after_termination().await?;
            return Err(OAuthError::AuthorizationExpired);
        }
        if let Err(error) = Self::validate_callback_issuer(
            stored_state
                .as_ref()
                .expect("active authorization state was checked above"),
            callback.issuer.as_deref(),
            true,
        ) {
            self.state_store
                .release_exchange_claim(&callback.state)
                .await;
            return Err(error);
        }
        let requested_scopes = active.requested_scopes.clone();
        let callback_state = callback.state.clone();
        let candidate = active.candidate.clone();
        let staged_store = active.staged_store.clone();
        let metadata = active.metadata.clone();
        let issuer = active.issuer.clone();
        drop(authorization_flow);

        let result = async {
            let exchange = spawn_code_exchange(candidate, callback);
            tokio::pin!(exchange);
            let exchange_result = tokio::select! {
                biased;
                result = &mut exchange => Some(result),
                _ = cancellation.cancelled() => None,
            };

            match (exchange_result, cancellation.is_cancelled()) {
                (_, true) | (None, false) => {
                    self.state_store.delete(&callback_state).await?;
                    let status = self.restore_status_after_termination().await?;
                    Ok(OAuthFlowOutcome::Terminated {
                        reason: OAuthCancellationReason::Cancelled,
                        status,
                    })
                }
                (Some(exchange_result), false) => match exchange_result {
                    Ok(mut scopes) => {
                        if scopes.is_empty() {
                            scopes = requested_scopes;
                        }
                        let mut credentials = staged_store
                            .load()
                            .await?
                            .ok_or(OAuthError::Protocol(OAuthProtocolError::Internal))?;
                        if credentials.granted_scopes.is_empty() {
                            credentials.granted_scopes = scopes.clone();
                        }

                        let prepared_scopes = scopes.clone();
                        let prepared_client_id = credentials.client_id.clone();
                        let prepared = {
                            let prepare = async {
                                let mut manager = AuthorizationManager::new_with_oauth_http_client(
                                    &self.resource,
                                    Arc::clone(&self.oauth_http_client),
                                )
                                .await?;
                                manager.set_credential_store(staged_store.clone());
                                manager.set_state_store(self.state_store.clone());
                                manager.set_metadata(metadata);
                                manager.initialize_from_store().await?;
                                configure_restored_authorization_client(
                                    &mut manager,
                                    &self.resource,
                                    &self.options,
                                    &prepared_scopes,
                                    &prepared_client_id,
                                    self.resolver.as_ref(),
                                )
                                .await?;

                                // Refreshes after installation must persist to the durable store.
                                // This setter cannot fail and all fallible manager preparation is
                                // complete.
                                manager.set_credential_store(self.store.clone());
                                Ok::<_, OAuthError>(manager)
                            };
                            tokio::pin!(prepare);
                            tokio::select! {
                                biased;
                                _ = cancellation.cancelled() => {
                                    return self
                                        .finish_claimed_cancellation(
                                            OAuthCancellationReason::Cancelled,
                                        )
                                        .await;
                                }
                                result = &mut prepare => result,
                            }
                        };

                        match prepared {
                            Ok(manager) => {
                                let completion_won = terminal
                                    .as_ref()
                                    .is_none_or(OAuthFlowTerminal::try_claim_completion);
                                if !completion_won {
                                    return self
                                        .finish_claimed_cancellation(
                                            OAuthCancellationReason::Cancelled,
                                        )
                                        .await;
                                }
                                // This is the single durable commit point. The credential backend's
                                // per-key atomic-save contract keeps the old value intact on error;
                                // after success only infallible in-memory publication remains.
                                if let Err(error) = self
                                    .store
                                    .commit_candidate(issuer, credentials)
                                    .await
                                    .map_err(OAuthError::from)
                                {
                                    self.restore_after_failed_authorization(&callback_state)
                                        .await?;
                                    Err(error)
                                } else {
                                    *self.client.inner.auth_manager.lock().await = manager;
                                    *self.granted_scopes.write().await = scopes.clone();
                                    self.status
                                        .set(OAuthStatus::Authorized {
                                            scopes: scopes.clone(),
                                        })
                                        .await;
                                    Ok(OAuthFlowOutcome::Authorized { scopes })
                                }
                            }
                            Err(error) => {
                                self.restore_after_failed_authorization(&callback_state)
                                    .await?;
                                Err(error)
                            }
                        }
                    }
                    Err(error) => {
                        self.restore_after_failed_authorization(&callback_state)
                            .await?;
                        Err(error)
                    }
                },
            }
        }
        .await;
        *self.authorization_flow.lock().await = AuthorizationFlowState::Idle;
        result
    }

    #[cfg(test)]
    pub(crate) async fn cancel(
        &self,
        cancellation: OAuthCancellation,
    ) -> Result<OAuthFlowOutcome, OAuthError> {
        self.cancel_with_terminal(cancellation, None).await
    }

    async fn cancel_with_terminal(
        &self,
        cancellation: OAuthCancellation,
        terminal: Option<OAuthFlowTerminal>,
    ) -> Result<OAuthFlowOutcome, OAuthError> {
        let mut authorization_flow = self.authorization_flow.lock().await;
        let claimed_reason = terminal
            .as_ref()
            .and_then(OAuthFlowTerminal::cancellation_reason);
        if let AuthorizationFlowState::Expired { state } = &*authorization_flow {
            if state != &cancellation.state {
                return Err(OAuthError::StateMismatch);
            }
            if let Some(reason) = claimed_reason {
                drop(authorization_flow);
                return self.finish_claimed_cancellation(reason).await;
            }
            *authorization_flow = AuthorizationFlowState::Idle;
            return Err(OAuthError::AuthorizationExpired);
        }
        let AuthorizationFlowState::Pending(active) = &*authorization_flow else {
            return Err(OAuthError::StateMismatch);
        };
        if active.launch.state != cancellation.state {
            return Err(OAuthError::StateMismatch);
        }
        let stored_state = self.state_store.load(&cancellation.state).await?;
        if active.generation != self.credential_generation() || stored_state.is_none() {
            if let Some(reason) = claimed_reason {
                drop(authorization_flow);
                return self.finish_claimed_cancellation(reason).await;
            }
            *authorization_flow = AuthorizationFlowState::Idle;
            drop(authorization_flow);
            self.state_store.delete(&cancellation.state).await?;
            self.restore_status_after_termination().await?;
            return Err(OAuthError::AuthorizationExpired);
        }
        let provider_callback = matches!(
            cancellation.reason,
            OAuthCancellationReason::AccessDenied | OAuthCancellationReason::AuthorizationError
        );
        Self::validate_callback_issuer(
            stored_state
                .as_ref()
                .expect("active authorization state was checked above"),
            cancellation.issuer.as_deref(),
            provider_callback,
        )?;
        let reason = if let Some(terminal) = terminal {
            terminal.claim_cancellation(cancellation.reason);
            terminal
                .cancellation_reason()
                .ok_or(OAuthError::AuthorizationExpired)?
        } else {
            cancellation.reason
        };
        drop(authorization_flow);
        self.finish_claimed_cancellation(reason).await
    }

    pub(crate) async fn clear(&self) -> Result<(), OAuthError> {
        self.invalidate_credentials(None).await
    }

    async fn invalidate_credentials(
        &self,
        expected_generation: Option<u64>,
    ) -> Result<(), OAuthError> {
        let _request_guard = self.request_gate.write().await;
        let _upgrade_guard = self.machine_scope_upgrade_gate.lock().await;
        if expected_generation.is_some_and(|generation| self.credential_generation() != generation)
        {
            return Ok(());
        }
        if expected_generation.is_some() {
            self.expire_invalid_authorization_flow().await?;
            if matches!(
                *self.authorization_flow.lock().await,
                AuthorizationFlowState::Pending(_)
            ) {
                return Ok(());
            }
        }
        let mut authorization_flow = self.authorization_flow.lock().await;
        let _manager_guard = self.client.inner.auth_manager.lock().await;
        if let AuthorizationFlowState::Pending(active) = &*authorization_flow {
            let state = active.launch.state.as_str();
            self.state_store.delete(state).await?;
        }
        self.store.clear().await?;
        *authorization_flow = AuthorizationFlowState::Idle;
        self.machine_scope_upgrades.lock().await.clear();
        self.granted_scopes.write().await.clear();
        self.status.set(OAuthStatus::Unauthorized).await;
        Ok(())
    }

    pub(crate) async fn observe_service_error(
        &self,
        error: &rmcp::ServiceError,
        expected_generation: u64,
    ) {
        let rmcp::ServiceError::TransportSend(transport) = error else {
            return;
        };
        self.observe_streamable_error(
            transport
                .error
                .downcast_ref::<rmcp::transport::streamable_http_client::StreamableHttpError<
                    reqwest::Error,
                >>(),
            expected_generation,
        )
        .await;
    }

    pub(crate) async fn observe_service_success(&self, expected_generation: u64) {
        let _request_guard = self.request_gate.write().await;
        if self.credential_generation() != expected_generation
            || matches!(self.options.mode, OAuthClientMode::AuthorizationCode { .. })
            || !matches!(
                self.status.get().await,
                OAuthStatus::ReauthorizationRequired { .. }
            )
        {
            return;
        }
        let scopes = match self.store.load().await {
            Ok(Some(credentials)) if !credentials.granted_scopes.is_empty() => {
                credentials.granted_scopes
            }
            _ => self.granted_scopes.read().await.clone(),
        };
        *self.granted_scopes.write().await = scopes.clone();
        self.status.set(OAuthStatus::Authorized { scopes }).await;
    }

    pub(crate) async fn observe_initialize_error(
        &self,
        error: &rmcp::service::ClientInitializeError,
        expected_generation: u64,
    ) {
        let rmcp::service::ClientInitializeError::TransportError { error, .. } = error else {
            return;
        };
        self.observe_streamable_error(
            error
                .error
                .downcast_ref::<rmcp::transport::streamable_http_client::StreamableHttpError<
                    reqwest::Error,
                >>(),
            expected_generation,
        )
        .await;
    }

    async fn observe_streamable_error(
        &self,
        error: Option<
            &rmcp::transport::streamable_http_client::StreamableHttpError<reqwest::Error>,
        >,
        expected_generation: u64,
    ) {
        let Some(error) = error else {
            return;
        };
        if self.credential_generation() != expected_generation {
            return;
        }
        match error {
            rmcp::transport::streamable_http_client::StreamableHttpError::InsufficientScope(
                insufficient,
            ) => {
                if let Some(required_scope) =
                    bearer_insufficient_scope(&insufficient.www_authenticate_header)
                {
                    self.handle_insufficient_scope(required_scope, expected_generation)
                        .await;
                }
            }
            rmcp::transport::streamable_http_client::StreamableHttpError::AuthRequired(_) => {
                let _ = self.invalidate_credentials(Some(expected_generation)).await;
            }
            rmcp::transport::streamable_http_client::StreamableHttpError::Auth(
                RmcpAuthError::AuthorizationRequired
                | RmcpAuthError::TokenRefreshFailed(_)
                | RmcpAuthError::TokenExpired,
            ) => {
                let _ = self.invalidate_credentials(Some(expected_generation)).await;
            }
            rmcp::transport::streamable_http_client::StreamableHttpError::Auth(
                RmcpAuthError::InsufficientScope { required_scope, .. },
            ) => {
                self.handle_insufficient_scope(required_scope.clone(), expected_generation)
                    .await;
            }
            _ => {}
        }
    }
}

fn normalize_required_scopes(required_scope: &str) -> Vec<String> {
    let mut scopes: Vec<String> = required_scope
        .split_whitespace()
        .map(str::to_string)
        .collect();
    scopes.sort();
    scopes.dedup();
    scopes
}

fn reserve_machine_scope_upgrade(
    attempts: &mut HashMap<String, usize>,
    required_scope: &str,
) -> Option<Vec<String>> {
    let scopes = normalize_required_scopes(required_scope);
    if scopes.is_empty() {
        return None;
    }
    let challenge = scopes.join(" ");
    let total_attempts: usize = attempts.values().sum();
    if total_attempts >= MAX_MACHINE_SCOPE_UPGRADES {
        return None;
    }
    *attempts.entry(challenge).or_default() += 1;
    Some(scopes)
}

fn is_loopback_host(url: &Url) -> bool {
    url.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    })
}

fn validate_secure_url(value: &str, _label: &str) -> Result<Url, OAuthError> {
    let url =
        Url::parse(value).map_err(|_| OAuthError::Protocol(OAuthProtocolError::InvalidUrl))?;
    if url.scheme() != "https" && !(url.scheme() == "http" && is_loopback_host(&url)) {
        return Err(OAuthError::Protocol(OAuthProtocolError::InvalidUrl));
    }
    Ok(url)
}

fn validate_redirect_uri(value: &str) -> Result<(), OAuthError> {
    let url = Url::parse(value)
        .map_err(|_| OAuthError::InvalidRedirectUri("redirect URI is invalid".to_string()))?;
    if url.fragment().is_some() {
        return Err(OAuthError::InvalidRedirectUri(
            "redirect URI must not contain a fragment".to_string(),
        ));
    }
    let secure_web = url.scheme() == "https";
    let loopback_http = url.scheme() == "http" && is_loopback_host(&url);
    let private_use = is_private_use_redirect_uri(&url);
    if !secure_web && !loopback_http && !private_use {
        return Err(OAuthError::InvalidRedirectUri(
            "redirect URI must use HTTPS, loopback HTTP, or a reverse-domain private-use scheme"
                .to_string(),
        ));
    }
    Ok(())
}

fn is_private_use_redirect_uri(url: &Url) -> bool {
    let scheme = url.scheme();
    if matches!(scheme, "http" | "https") {
        return false;
    }
    let labels: Vec<&str> = scheme.split('.').collect();
    let reverse_domain_scheme = labels.len() >= 2
        && labels.iter().all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '-')
        });
    let prefix = format!("{scheme}:/");
    let authority_prefix = format!("{scheme}://");
    reverse_domain_scheme
        && url.host_str().is_none()
        && url.as_str().starts_with(&prefix)
        && !url.as_str().starts_with(&authority_prefix)
        && url.path().starts_with('/')
        && url.path().len() > 1
}

fn canonical_resource_identity(value: &str) -> Result<String, OAuthError> {
    let resource =
        Url::parse(value).map_err(|_| OAuthError::Protocol(OAuthProtocolError::InvalidUrl))?;
    if resource.fragment().is_some() {
        return Err(OAuthError::Protocol(OAuthProtocolError::InvalidUrl));
    }
    Ok(resource.to_string())
}

fn validate_authorization_metadata(
    metadata: &AuthorizationMetadata,
    require_pkce: bool,
) -> Result<(), OAuthError> {
    validate_secure_url(&metadata.authorization_endpoint, "authorization endpoint")?;
    validate_secure_url(&metadata.token_endpoint, "token endpoint")?;
    if let Some(endpoint) = metadata.registration_endpoint.as_deref() {
        validate_secure_url(endpoint, "registration endpoint")?;
    }
    if let Some(issuer) = metadata.issuer.as_deref() {
        validate_secure_url(issuer, "authorization issuer")?;
    }
    if let Some(jwks_uri) = metadata.jwks_uri.as_deref() {
        validate_secure_url(jwks_uri, "JWKS endpoint")?;
    }
    if require_pkce
        && !metadata
            .code_challenge_methods_supported
            .as_ref()
            .is_some_and(|methods| methods.iter().any(|method| method == "S256"))
    {
        return Err(OAuthError::Protocol(OAuthProtocolError::PkceUnsupported));
    }
    Ok(())
}

/// Return a stable credential namespace identity for the authorization server.
///
/// RFC 8414 metadata has an `issuer`, but rmcp intentionally accepts legacy
/// metadata that omits it. Such servers must not all collapse into one
/// `<unknown>` slot: bind credentials to a digest of their validated endpoints.
fn authorization_server_credential_identity(
    metadata: &AuthorizationMetadata,
) -> Result<String, OAuthError> {
    if let Some(issuer) = metadata.issuer.as_deref() {
        return Ok(validate_secure_url(issuer, "authorization issuer")?.to_string());
    }
    let authorization_endpoint =
        validate_secure_url(&metadata.authorization_endpoint, "authorization endpoint")?;
    let token_endpoint = validate_secure_url(&metadata.token_endpoint, "token endpoint")?;
    let mut digest = Sha256::new();
    digest.update(authorization_endpoint.as_str().as_bytes());
    digest.update(b"\0");
    digest.update(token_endpoint.as_str().as_bytes());
    if let Some(registration_endpoint) = metadata.registration_endpoint.as_deref() {
        digest.update(b"\0");
        digest.update(
            validate_secure_url(registration_endpoint, "registration endpoint")?
                .as_str()
                .as_bytes(),
        );
    }
    Ok(format!("legacy-as:{:x}", digest.finalize()))
}

fn oauth_mode_fingerprint(options: &OAuthOptions) -> String {
    let (mode, client_slot) = match &options.mode {
        OAuthClientMode::AuthorizationCode {
            registration: OAuthClientRegistration::Dynamic,
        } => (
            "v1:authorization_code:dynamic",
            options.client_name.as_deref().unwrap_or("A2C Computer"),
        ),
        OAuthClientMode::AuthorizationCode {
            registration: OAuthClientRegistration::Preregistered { client_id, .. },
        } => ("v1:authorization_code:preregistered", client_id.as_str()),
        OAuthClientMode::AuthorizationCode {
            registration: OAuthClientRegistration::ClientMetadataDocument { url },
        } => (
            "v1:authorization_code:client_metadata_document",
            url.as_str(),
        ),
        OAuthClientMode::ClientCredentialsSecret { client_id, .. } => {
            ("v1:client_credentials:secret", client_id.as_str())
        }
        OAuthClientMode::ClientCredentialsPrivateKeyJwt { client_id, .. } => {
            ("v1:client_credentials:private_key_jwt", client_id.as_str())
        }
    };
    let mut scopes = options.scopes.clone();
    scopes.sort();
    scopes.dedup();
    let mut digest = Sha256::new();
    digest.update(client_slot.as_bytes());
    digest.update(b"\0");
    for scope in scopes {
        digest.update(scope.as_bytes());
        digest.update(b"\0");
    }
    format!("{mode}:scopes-{:x}", digest.finalize())
}

fn stored_client_matches(options: &OAuthOptions, stored_client_id: &str) -> bool {
    match &options.mode {
        OAuthClientMode::AuthorizationCode {
            registration: OAuthClientRegistration::Dynamic,
        } => true,
        OAuthClientMode::AuthorizationCode {
            registration: OAuthClientRegistration::Preregistered { client_id, .. },
        }
        | OAuthClientMode::ClientCredentialsSecret { client_id, .. }
        | OAuthClientMode::ClientCredentialsPrivateKeyJwt { client_id, .. } => {
            client_id == stored_client_id
        }
        OAuthClientMode::AuthorizationCode {
            registration: OAuthClientRegistration::ClientMetadataDocument { url },
        } => url == stored_client_id,
    }
}

async fn resolve_secret_from(
    resolver: Option<&Arc<dyn SecretValueResolver>>,
    id: &str,
) -> Result<String, OAuthError> {
    let Some(resolver) = resolver else {
        return Err(OAuthError::MissingSecret(id.to_string()));
    };
    let input = MCPServerInput::PromptString(PromptStringInput {
        id: id.to_string(),
        description: "OAuth credential".to_string(),
        default: None,
        password: Some(true),
    });
    resolver
        .resolve_secret(&input)
        .await
        .map_err(|_| OAuthError::Protocol(OAuthProtocolError::Internal))?
        .ok_or_else(|| OAuthError::MissingSecret(id.to_string()))
}

async fn configure_restored_authorization_client(
    manager: &mut AuthorizationManager,
    resource: &str,
    options: &OAuthOptions,
    granted_scopes: &[String],
    stored_client_id: &str,
    resolver: Option<&Arc<dyn SecretValueResolver>>,
) -> Result<(), OAuthError> {
    let OAuthClientMode::AuthorizationCode { registration } = &options.mode else {
        return Ok(());
    };
    let scopes = if granted_scopes.is_empty() {
        options.scopes.clone()
    } else {
        granted_scopes.to_vec()
    };
    let mut config = OAuthClientConfig::new(stored_client_id, resource).with_scopes(scopes);
    if let OAuthClientRegistration::Preregistered {
        client_secret_input: Some(input),
        ..
    } = registration
    {
        config = config.with_client_secret(resolve_secret_from(resolver, input).await?);
    }
    manager.configure_client(config)?;
    Ok(())
}

async fn restore_authorization_status(
    store: &ScopedCredentialStore,
    fallback_scopes: &[String],
    granted_scopes: &RwLock<Vec<String>>,
    status: &OAuthStatusState,
) -> Result<OAuthStatus, OAuthError> {
    let restored = match store.load().await {
        Ok(restored) => restored,
        Err(error) => {
            status
                .set(OAuthStatus::Error {
                    message: "OAuth credential state is unavailable".to_string(),
                })
                .await;
            return Err(error.into());
        }
    };
    let restored_status = match restored {
        Some(credentials) if credentials.token_response.is_some() => {
            let scopes = if credentials.granted_scopes.is_empty() {
                fallback_scopes.to_vec()
            } else {
                credentials.granted_scopes
            };
            *granted_scopes.write().await = scopes.clone();
            OAuthStatus::Authorized { scopes }
        }
        _ => OAuthStatus::Unauthorized,
    };
    status.set(restored_status.clone()).await;
    Ok(restored_status)
}

/// Run an rmcp OAuth operation with tracing disabled for its entire async lifetime.
///
/// rmcp 2.2 logs authorization codes and full token-exchange results at `debug` level. A tracing
/// dispatcher is thread-local, while a future running on Tokio's multithreaded runtime may migrate
/// between worker threads. We therefore enter `NoSubscriber` on a dedicated blocking thread and
/// drive the future with a current-thread runtime so every poll remains inside that dispatcher.
/// `spawn_blocking` also avoids nesting `block_on` on an async runtime worker. OAuth exchanges are
/// infrequent control-plane operations, so the per-exchange runtime cost is accepted until rmcp no
/// longer emits credential-bearing values.
fn spawn_sensitive_oauth_task<T, E, F, Fut>(task: F) -> tokio::task::JoinHandle<Result<T, E>>
where
    T: Send + 'static,
    E: From<RmcpAuthError> + Send + 'static,
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = Result<T, E>> + 'static,
{
    tokio::task::spawn_blocking(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| {
                E::from(RmcpAuthError::InternalError(
                    "isolated OAuth runtime initialization failed".to_string(),
                ))
            })?;
        let dispatch = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
        tracing::dispatcher::with_default(&dispatch, || runtime.block_on(task()))
    })
}

async fn spawn_code_exchange(
    client: SensitiveAuthClient,
    callback: OAuthCallback,
) -> Result<Vec<String>, OAuthError> {
    spawn_sensitive_oauth_task(move || async move {
        let manager = client.inner.auth_manager.lock().await;
        manager
            .exchange_code_for_token_with_issuer(
                &callback.code,
                &callback.state,
                callback.issuer.as_deref(),
            )
            .await
            .map_err(|_| OAuthError::Protocol(OAuthProtocolError::TokenExchangeFailed))?;
        Ok(manager.get_current_scopes().await)
    })
    .await
    .map_err(|_| OAuthError::Protocol(OAuthProtocolError::Internal))?
}

fn spawn_client_credentials_exchange(
    resource: String,
    store: ScopedCredentialStore,
    oauth_http_client: Arc<dyn OAuthHttpClient>,
    config: ClientCredentialsConfig,
) -> tokio::task::JoinHandle<Result<AuthorizationManager, RmcpAuthError>> {
    spawn_sensitive_oauth_task(move || async move {
        let mut manager =
            AuthorizationManager::new_with_oauth_http_client(&resource, oauth_http_client).await?;
        manager.set_credential_store(store.clone());
        let metadata = manager.discover_metadata().await?;
        validate_authorization_metadata(&metadata, false).map_err(|_| {
            RmcpAuthError::MetadataError(
                "authorization metadata contains an insecure endpoint".to_string(),
            )
        })?;
        store
            .set_issuer(Some(
                authorization_server_credential_identity(&metadata).map_err(|_| {
                    RmcpAuthError::MetadataError(
                        "authorization server identity is invalid".to_string(),
                    )
                })?,
            ))
            .await?;
        manager.set_metadata(metadata);
        manager.validate_client_credentials_metadata(&config)?;
        manager.configure_client_credentials(&config)?;
        manager.exchange_client_credentials(&config).await?;
        Ok(manager)
    })
}

fn parse_algorithm(value: &str) -> Result<JwtSigningAlgorithm, OAuthError> {
    match value {
        "RS256" => Ok(JwtSigningAlgorithm::RS256),
        "RS384" => Ok(JwtSigningAlgorithm::RS384),
        "RS512" => Ok(JwtSigningAlgorithm::RS512),
        "ES256" => Ok(JwtSigningAlgorithm::ES256),
        "ES384" => Ok(JwtSigningAlgorithm::ES384),
        other => Err(OAuthError::UnsupportedSigningAlgorithm(other.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inputs::InputResolutionError;
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    use http_body_util::{BodyExt, Full};
    use hyper::body::Bytes;
    use hyper::service::service_fn;
    use hyper::{Request, Response, StatusCode};
    use hyper_util::rt::TokioIo;
    use std::convert::Infallible;
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tracing_subscriber::fmt::MakeWriter;

    fn test_bundle_id() -> BundleId {
        BundleId::try_from("oauth-test".to_string()).unwrap()
    }

    fn memory_credential_store() -> Arc<dyn OAuthCredentialStore> {
        Arc::new(InMemoryOAuthCredentialStore::default())
    }

    #[test]
    fn scope_step_up_requires_valid_bearer_insufficient_scope_challenge() {
        assert_eq!(
            bearer_insufficient_scope(r#"Bearer error="insufficient_scope", scope="tools.write""#),
            Some("tools.write".to_string())
        );
        assert_eq!(
            bearer_insufficient_scope(r#"Basic error="insufficient_scope", scope="tools.write""#),
            None
        );
        assert_eq!(
            bearer_insufficient_scope(r#"Bearer scope="tools.write""#),
            None
        );
        assert_eq!(
            bearer_insufficient_scope(r#"Bearer error="insufficient_scope""#),
            None
        );
        assert_eq!(
            bearer_insufficient_scope(r#"Bearer error="INSUFFICIENT_SCOPE", scope="tools.write""#),
            None
        );
    }

    #[test]
    fn automatic_admission_requires_the_metadata_resource_to_match_the_endpoint() {
        assert!(DiscoveryCleanupOAuthHttpClient::admitted_resource_matches(
            "https://mcp.example/mcp",
            "https://MCP.EXAMPLE:443/mcp"
        ));
        assert!(!DiscoveryCleanupOAuthHttpClient::admitted_resource_matches(
            "https://mcp.example/mcp",
            "https://mcp.example/"
        ));
        assert!(!DiscoveryCleanupOAuthHttpClient::admitted_resource_matches(
            "https://mcp.example/mcp",
            "https://other.example/mcp"
        ));
        assert!(
            serde_json::from_value::<AdmittedResourceMetadataDocument>(serde_json::json!({
                "resource": "https://mcp.example/mcp",
                "authorization_servers": "https://auth.example"
            }))
            .is_err()
        );
    }

    fn test_coordinator_context(
        credential_store: Arc<dyn OAuthCredentialStore>,
    ) -> OAuthCoordinatorContext {
        OAuthCoordinatorContext::new(test_bundle_id(), credential_store, None)
    }

    const TEST_EC_PRIVATE_KEY: &str = r#"-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgp9PptiYIX1DoplcU
CrXJICvftS6mTCVk+I+JynptjaShRANCAAT54hAudKCxTrTPlQUCSAHZtmOxl6fL
hSEGx6f7gFfatuW4qJ/SM6W4Yt7BxI4gJ30LDd0WPiDGALXZQYff2g7l
-----END PRIVATE KEY-----"#;

    #[derive(Clone, Default)]
    struct CapturedLogs(Arc<StdMutex<Vec<u8>>>);

    struct CapturedLogWriter(Arc<StdMutex<Vec<u8>>>);

    impl Write for CapturedLogWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("captured log lock poisoned")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'writer> MakeWriter<'writer> for CapturedLogs {
        type Writer = CapturedLogWriter;

        fn make_writer(&'writer self) -> Self::Writer {
            CapturedLogWriter(Arc::clone(&self.0))
        }
    }

    impl CapturedLogs {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().expect("captured log lock poisoned").clone())
                .expect("tracing output must be UTF-8")
        }
    }

    struct JwtTestSecretResolver;

    #[async_trait]
    impl SecretValueResolver for JwtTestSecretResolver {
        async fn resolve_secret(
            &self,
            def: &MCPServerInput,
        ) -> Result<Option<String>, InputResolutionError> {
            Ok((def.id() == "jwt-private-key").then(|| TEST_EC_PRIVATE_KEY.to_string()))
        }
    }

    struct JwtInterceptingOAuthHttpClient {
        delegate: DiscoveryCleanupOAuthHttpClient,
        token_forms: Arc<StdMutex<Vec<HashMap<String, String>>>>,
    }

    impl OAuthHttpClient for JwtInterceptingOAuthHttpClient {
        fn execute(&self, request: OAuthHttpRequest) -> OAuthHttpClientFuture<'_> {
            Box::pin(async move {
                if request.request.uri().host() != Some("issuer.example") {
                    return self.delegate.execute(request).await;
                }

                let response = match (
                    request.request.method().as_str(),
                    request.request.uri().path(),
                ) {
                    ("GET", "/.well-known/oauth-authorization-server") => http::Response::builder()
                        .header("Content-Type", "application/json")
                        .body(
                            serde_json::json!({
                                "issuer": "https://issuer.example",
                                "authorization_endpoint": "https://issuer.example/authorize",
                                "token_endpoint": "https://issuer.example/token",
                                "grant_types_supported": ["client_credentials"],
                                "token_endpoint_auth_methods_supported": ["private_key_jwt"],
                                "token_endpoint_auth_signing_alg_values_supported": ["ES256"]
                            })
                            .to_string()
                            .into_bytes(),
                        )
                        .unwrap(),
                    ("POST", "/token") => {
                        let form: HashMap<String, String> =
                            url::form_urlencoded::parse(request.request.body())
                                .into_owned()
                                .collect();
                        self.token_forms
                            .lock()
                            .expect("JWT token form lock poisoned")
                            .push(form);
                        http::Response::builder()
                            .header("Content-Type", "application/json")
                            .body(
                                serde_json::json!({
                                    "access_token": "jwt-access-token",
                                    "token_type": "Bearer",
                                    "expires_in": 31,
                                    "scope": "tools.read"
                                })
                                .to_string()
                                .into_bytes(),
                            )
                            .unwrap()
                    }
                    _ => http::Response::builder()
                        .status(StatusCode::NOT_FOUND)
                        .body(Vec::new())
                        .unwrap(),
                };
                Ok(response)
            })
        }
    }

    struct TlsFixtureOAuthHttpClient {
        follow_redirects: reqwest::Client,
        stop_redirects: reqwest::Client,
    }

    impl TlsFixtureOAuthHttpClient {
        fn new(certificate: reqwest::Certificate) -> Self {
            let follow_redirects = reqwest::Client::builder()
                .tls_certs_only([certificate.clone()])
                .no_proxy()
                .build()
                .unwrap();
            let stop_redirects = reqwest::Client::builder()
                .tls_certs_only([certificate])
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap();
            Self {
                follow_redirects,
                stop_redirects,
            }
        }
    }

    impl OAuthHttpClient for TlsFixtureOAuthHttpClient {
        fn execute(&self, request: OAuthHttpRequest) -> OAuthHttpClientFuture<'_> {
            Box::pin(async move {
                let OAuthHttpRequest {
                    request,
                    redirect_policy,
                    timeout,
                    ..
                } = request;
                let client = match redirect_policy {
                    OAuthHttpRedirectPolicy::Follow => &self.follow_redirects,
                    _ => &self.stop_redirects,
                };
                let mut request = reqwest::Request::try_from(request)
                    .map_err(|error| OAuthHttpClientError::new(error.to_string()))?;
                *request.timeout_mut() = timeout;
                let response = client
                    .execute(request)
                    .await
                    .map_err(|error| OAuthHttpClientError::new(error.to_string()))?;
                let mut builder = http::Response::builder()
                    .status(response.status())
                    .version(response.version());
                for (name, value) in response.headers() {
                    builder = builder.header(name, value);
                }
                let body = response
                    .bytes()
                    .await
                    .map_err(|error| OAuthHttpClientError::new(error.to_string()))?
                    .to_vec();
                builder
                    .body(body)
                    .map_err(|error| OAuthHttpClientError::new(error.to_string()))
            })
        }
    }

    #[test]
    fn debug_redacts_callback_and_launch() {
        let callback = OAuthCallback {
            code: "secret-code".into(),
            state: "secret-state".into(),
            issuer: Some("https://issuer.example".into()),
        };
        let launch = OAuthLaunch {
            authorization_url: "https://issuer.example/authorize?state=secret".into(),
            state: "secret".into(),
        };
        let cancellation = OAuthCancellation {
            state: "cancel-secret-state".into(),
            issuer: Some("https://issuer.example".into()),
            reason: OAuthCancellationReason::Timeout,
        };
        let text = format!("{callback:?} {launch:?} {cancellation:?}");
        assert!(!text.contains("secret-code"));
        assert!(!text.contains("secret-state"));
        assert!(!text.contains("cancel-secret-state"));
        assert!(!text.contains("authorize?"));
    }

    #[test]
    fn protocol_error_display_and_debug_do_not_expose_provider_details() {
        let marker = "provider-response-secret";
        let error = OAuthError::from(RmcpAuthError::TokenRefreshFailed(marker.into()));

        assert!(matches!(
            error,
            OAuthError::Protocol(OAuthProtocolError::TokenRefreshFailed)
        ));
        assert!(
            !error.to_string().contains(marker),
            "OAuthError Display must not expose provider-controlled details"
        );
        assert!(
            !format!("{error:?}").contains(marker),
            "OAuthError Debug must not expose provider-controlled details"
        );
        let mut source = std::error::Error::source(&error);
        while let Some(current) = source {
            assert!(
                !current.to_string().contains(marker),
                "OAuthError source chain must not expose provider-controlled details"
            );
            source = current.source();
        }
    }

    #[test]
    fn oauth_lifecycle_registry_reclaims_dead_slots() {
        let backend = memory_credential_store();
        let store_identity = Arc::as_ptr(&backend) as *const () as usize;
        let resource_prefix = "https://resource.example/dead-lifecycle-slot/";

        for index in 0..32 {
            let store = ScopedCredentialStore::new(
                test_bundle_id(),
                format!("{resource_prefix}{index}"),
                "registry-reclamation".into(),
                Arc::clone(&backend),
            );
            drop(store);
        }

        let retained = oauth_lifecycle_registry().matching_keys(|(identity, _, resource, _)| {
            *identity == store_identity && resource.starts_with(resource_prefix)
        });
        assert!(
            retained <= 1,
            "dead OAuth lifecycle slots must be reclaimed; retained {retained}"
        );
    }

    #[test]
    fn oauth_config_status_and_outcome_use_camel_case_fields() {
        let options = OAuthOptions {
            resource: None,
            scopes: vec!["tools.read".into()],
            client_name: Some("Desktop".into()),
            mode: OAuthClientMode::ClientCredentialsPrivateKeyJwt {
                client_id: "client".into(),
                private_key_input: "oauth-key".into(),
                algorithm: "RS256".into(),
                token_endpoint_audience: Some("https://issuer.example/token".into()),
            },
        };
        let value = serde_json::to_value(&options).unwrap();
        assert!(value.get("resource").is_none());
        assert_eq!(value["clientName"], "Desktop");
        assert_eq!(value["mode"]["clientId"], "client");
        assert_eq!(value["mode"]["privateKeyInput"], "oauth-key");
        assert_eq!(
            value["mode"]["tokenEndpointAudience"],
            "https://issuer.example/token"
        );
        assert!(value["mode"].get("client_id").is_none());

        let status = serde_json::to_value(OAuthStatus::ReauthorizationRequired {
            required_scope: "tools.write".into(),
        })
        .unwrap();
        assert_eq!(status["requiredScope"], "tools.write");
        assert!(status.get("required_scope").is_none());
        let outcome = serde_json::to_value(OAuthFlowOutcome::Terminated {
            reason: OAuthCancellationReason::AuthorizationError,
            status: OAuthStatus::Unauthorized,
        })
        .unwrap();
        assert_eq!(outcome["outcome"], "terminated");
        assert_eq!(outcome["reason"], "authorizationError");
        assert_eq!(outcome["status"]["state"], "unauthorized");
        assert_eq!(
            serde_json::from_value::<OAuthOptions>(value).unwrap(),
            options
        );

        let mut explicit = options;
        explicit.resource = Some("https://resource.example/canonical".into());
        let explicit_value = serde_json::to_value(&explicit).unwrap();
        assert_eq!(
            explicit_value["resource"],
            "https://resource.example/canonical"
        );
    }

    #[test]
    fn credential_namespace_includes_resource_and_issuer() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let backend = memory_credential_store();
            let a = ScopedCredentialStore::new(
                test_bundle_id(),
                "https://a.example/mcp".into(),
                "mode".into(),
                Arc::clone(&backend),
            );
            let b = ScopedCredentialStore::new(
                test_bundle_id(),
                "https://b.example/mcp".into(),
                "mode".into(),
                backend,
            );
            a.set_issuer(Some("https://issuer.example".into()))
                .await
                .unwrap();
            b.set_issuer(Some("https://issuer.example".into()))
                .await
                .unwrap();
            assert_ne!(a.key().await, b.key().await);
        });
    }

    #[tokio::test]
    async fn canonical_resource_identity_restores_and_clears_equivalent_urls() {
        let first = "https://EXAMPLE.com:443/a/../mcp";
        let equivalent = "https://example.com/mcp";
        assert_eq!(
            canonical_resource_identity(first).unwrap(),
            canonical_resource_identity(equivalent).unwrap()
        );

        let options = OAuthOptions {
            resource: None,
            scopes: vec!["tools.read".into()],
            client_name: None,
            mode: OAuthClientMode::AuthorizationCode {
                registration: OAuthClientRegistration::Preregistered {
                    client_id: "canonical-client".into(),
                    client_secret_input: None,
                },
            },
        };
        let backend = memory_credential_store();
        let stored = ScopedCredentialStore::new(
            test_bundle_id(),
            canonical_resource_identity(first).unwrap(),
            oauth_mode_fingerprint(&options),
            Arc::clone(&backend),
        );
        stored
            .set_issuer(Some("https://issuer.example".into()))
            .await
            .unwrap();
        stored
            .save(StoredCredentials::new(
                "canonical-client".into(),
                None,
                vec!["tools.read".into()],
                None,
            ))
            .await
            .unwrap();

        let restored = ScopedCredentialStore::new(
            test_bundle_id(),
            canonical_resource_identity(equivalent).unwrap(),
            oauth_mode_fingerprint(&options),
            Arc::clone(&backend),
        );
        restored
            .set_issuer(Some("https://issuer.example".into()))
            .await
            .unwrap();
        assert_eq!(
            restored.load().await.unwrap().unwrap().client_id,
            "canonical-client"
        );

        clear_stored_oauth_credentials(
            &test_bundle_id(),
            equivalent,
            &options,
            Arc::clone(&backend),
        )
        .await
        .unwrap();
        assert!(stored.load().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn credential_store_preserves_granted_scopes_when_refresh_omits_scope() {
        let store = ScopedCredentialStore::new(
            test_bundle_id(),
            "https://resource.example/scope-preservation".into(),
            "mode".into(),
            memory_credential_store(),
        );
        store
            .set_issuer(Some("https://issuer.example".into()))
            .await
            .unwrap();
        store
            .save(StoredCredentials::new(
                "client-id".into(),
                None,
                vec!["tools.read".into(), "tools.write".into()],
                None,
            ))
            .await
            .unwrap();
        store
            .save(StoredCredentials::new(
                "client-id".into(),
                None,
                Vec::new(),
                None,
            ))
            .await
            .unwrap();

        assert_eq!(
            store.load().await.unwrap().unwrap().granted_scopes,
            vec!["tools.read", "tools.write"]
        );
    }

    #[tokio::test]
    async fn credential_generation_advances_on_save_and_clear() {
        let resource = "https://resource.example/generation";
        let backend = memory_credential_store();
        let store = ScopedCredentialStore::new(
            test_bundle_id(),
            resource.into(),
            "mode".into(),
            Arc::clone(&backend),
        );
        let peer =
            ScopedCredentialStore::new(test_bundle_id(), resource.into(), "mode".into(), backend);
        assert!(Arc::ptr_eq(&store.lifecycle, &peer.lifecycle));
        let initial = store.lifecycle.generation.load(Ordering::Acquire);

        store
            .save(StoredCredentials::new(
                "client-id".into(),
                None,
                vec!["tools.read".into()],
                None,
            ))
            .await
            .unwrap();
        let after_save = store.lifecycle.generation.load(Ordering::Acquire);
        assert!(after_save > initial);
        assert_eq!(
            peer.lifecycle.generation.load(Ordering::Acquire),
            after_save
        );
        assert_eq!(peer.load().await.unwrap().unwrap().client_id, "client-id");

        store.clear().await.unwrap();
        assert!(store.lifecycle.generation.load(Ordering::Acquire) > after_save);
        assert!(peer.load().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn credential_clear_and_generation_are_isolated_by_oauth_slot() {
        let resource = "https://resource.example/slot-isolation";
        let secret_options = OAuthOptions {
            resource: None,
            scopes: vec!["tools.read".into()],
            client_name: None,
            mode: OAuthClientMode::ClientCredentialsSecret {
                client_id: "client-a".into(),
                client_secret_input: "secret-a".into(),
            },
        };
        let jwt_options = OAuthOptions {
            resource: None,
            scopes: vec!["tools.read".into()],
            client_name: None,
            mode: OAuthClientMode::ClientCredentialsPrivateKeyJwt {
                client_id: "client-b".into(),
                private_key_input: "key-b".into(),
                algorithm: "ES256".into(),
                token_endpoint_audience: None,
            },
        };
        let backend = memory_credential_store();
        let secret_store = ScopedCredentialStore::new(
            test_bundle_id(),
            resource.into(),
            oauth_mode_fingerprint(&secret_options),
            Arc::clone(&backend),
        );
        let jwt_store = ScopedCredentialStore::new(
            test_bundle_id(),
            resource.into(),
            oauth_mode_fingerprint(&jwt_options),
            Arc::clone(&backend),
        );
        assert!(!Arc::ptr_eq(&secret_store.lifecycle, &jwt_store.lifecycle));
        secret_store
            .set_issuer(Some("https://issuer.example".into()))
            .await
            .unwrap();
        jwt_store
            .set_issuer(Some("https://issuer.example".into()))
            .await
            .unwrap();
        secret_store
            .save(StoredCredentials::new(
                "client-a".into(),
                None,
                vec!["tools.read".into()],
                None,
            ))
            .await
            .unwrap();
        jwt_store
            .save(StoredCredentials::new(
                "client-b".into(),
                None,
                vec!["tools.read".into()],
                None,
            ))
            .await
            .unwrap();
        let jwt_generation = jwt_store.lifecycle.generation.load(Ordering::Acquire);

        clear_stored_oauth_credentials(
            &test_bundle_id(),
            resource,
            &secret_options,
            Arc::clone(&backend),
        )
        .await
        .unwrap();

        assert!(secret_store.load().await.unwrap().is_none());
        assert_eq!(
            jwt_store.load().await.unwrap().unwrap().client_id,
            "client-b"
        );
        assert_eq!(
            jwt_store.lifecycle.generation.load(Ordering::Acquire),
            jwt_generation,
            "clearing one OAuth slot must not invalidate a peer slot"
        );
    }

    #[tokio::test]
    async fn issuer_migration_does_not_restore_previous_authorization_server_credentials() {
        let resource = "https://resource.example/issuer-migration";
        let store = ScopedCredentialStore::new(
            test_bundle_id(),
            resource.into(),
            "mode".into(),
            memory_credential_store(),
        );
        let mut first = AuthorizationMetadata::default();
        first.authorization_endpoint = "https://first.example/authorize".into();
        first.token_endpoint = "https://first.example/token".into();
        let mut second = AuthorizationMetadata::default();
        second.authorization_endpoint = "https://second.example/authorize".into();
        second.token_endpoint = "https://second.example/token".into();

        store
            .set_issuer(Some(
                authorization_server_credential_identity(&first).unwrap(),
            ))
            .await
            .unwrap();
        store
            .save(StoredCredentials::new(
                "dynamic-client-from-first".into(),
                None,
                vec!["tools.read".into()],
                None,
            ))
            .await
            .unwrap();
        store
            .set_issuer(Some(
                authorization_server_credential_identity(&second).unwrap(),
            ))
            .await
            .unwrap();

        assert!(store.load().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn authorization_state_store_expires_abandoned_pkce_state() {
        let store = ExpiringStateStore::new(Duration::from_secs(1));
        let expired: StoredAuthorizationState = serde_json::from_value(serde_json::json!({
            "pkce_verifier": "verifier",
            "csrf_token": "state",
            "expected_issuer": null,
            "require_issuer": false,
            "created_at": 1
        }))
        .unwrap();
        store.save("state", expired).await.unwrap();

        assert!(store.load("state").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn claimed_authorization_state_survives_exchange_handoff_only_until_release_or_delete() {
        fn state(csrf_token: &str) -> StoredAuthorizationState {
            serde_json::from_value(serde_json::json!({
                "pkce_verifier": "verifier",
                "csrf_token": csrf_token,
                "expected_issuer": "https://issuer.example",
                "require_issuer": true,
                "created_at": ExpiringStateStore::now_epoch_secs()
            }))
            .unwrap()
        }

        let store = ExpiringStateStore::new(Duration::from_secs(1));
        store.save("claimed", state("claimed")).await.unwrap();
        assert!(store.claim_for_exchange("claimed").await.is_some());
        store
            .states
            .write()
            .await
            .get_mut("claimed")
            .unwrap()
            .state
            .created_at = 1;
        assert!(
            store.load("claimed").await.unwrap().is_some(),
            "an accepted callback must not expire during the rmcp handoff"
        );
        store.delete("claimed").await.unwrap();
        assert!(store.load("claimed").await.unwrap().is_none());

        store.save("released", state("released")).await.unwrap();
        assert!(store.claim_for_exchange("released").await.is_some());
        store.release_exchange_claim("released").await;
        store
            .states
            .write()
            .await
            .get_mut("released")
            .unwrap()
            .state
            .created_at = 1;
        assert!(
            store.load("released").await.unwrap().is_none(),
            "a rejected callback must release the claim and restore TTL enforcement"
        );
    }

    #[test]
    fn stored_client_identity_must_match_fixed_registration() {
        let fixed = OAuthOptions {
            resource: None,
            scopes: Vec::new(),
            client_name: None,
            mode: OAuthClientMode::AuthorizationCode {
                registration: OAuthClientRegistration::Preregistered {
                    client_id: "expected-client".into(),
                    client_secret_input: None,
                },
            },
        };
        let dynamic = OAuthOptions {
            resource: None,
            scopes: Vec::new(),
            client_name: None,
            mode: OAuthClientMode::AuthorizationCode {
                registration: OAuthClientRegistration::Dynamic,
            },
        };

        assert!(stored_client_matches(&fixed, "expected-client"));
        assert!(!stored_client_matches(&fixed, "stale-client"));
        assert!(stored_client_matches(&dynamic, "registered-at-runtime"));
    }

    #[test]
    fn credential_fingerprint_binds_grant_mode_and_scopes() {
        let auth_code = OAuthOptions {
            resource: None,
            scopes: vec!["tools.read".into()],
            client_name: None,
            mode: OAuthClientMode::AuthorizationCode {
                registration: OAuthClientRegistration::Preregistered {
                    client_id: "same-client".into(),
                    client_secret_input: None,
                },
            },
        };
        let mut machine = auth_code.clone();
        machine.mode = OAuthClientMode::ClientCredentialsSecret {
            client_id: "same-client".into(),
            client_secret_input: "secret".into(),
        };
        let mut expanded = machine.clone();
        expanded.scopes.push("tools.write".into());

        assert_ne!(
            oauth_mode_fingerprint(&auth_code),
            oauth_mode_fingerprint(&machine)
        );
        assert_ne!(
            oauth_mode_fingerprint(&machine),
            oauth_mode_fingerprint(&expanded)
        );
    }

    #[test]
    fn secure_url_policy_accepts_native_private_use_redirects_only() {
        assert!(validate_secure_url("https://resource.example/mcp", "resource").is_ok());
        assert!(validate_secure_url("http://127.0.0.1:8080/mcp", "resource").is_ok());
        assert!(validate_secure_url("http://resource.example/mcp", "resource").is_err());
        assert_eq!(
            canonical_resource_identity("https://EXAMPLE.com:443/a/../mcp").unwrap(),
            "https://example.com/mcp"
        );
        assert_eq!(
            canonical_resource_identity("urn:example:mcp-resource").unwrap(),
            "urn:example:mcp-resource"
        );
        assert!(canonical_resource_identity("/relative-resource").is_err());
        assert!(canonical_resource_identity("https://resource.example/mcp#fragment").is_err());
        assert!(validate_redirect_uri("http://localhost:9876/callback").is_ok());
        assert!(validate_redirect_uri("https://desktop.example/callback").is_ok());
        assert!(validate_redirect_uri("com.example.app:/oauth/callback").is_ok());
        assert!(validate_redirect_uri("http://localhost:9876/callback#fragment").is_err());
        assert!(validate_redirect_uri("https://desktop.example/callback#fragment").is_err());
        assert!(validate_redirect_uri("com.example.app:/oauth/callback#fragment").is_err());
        assert!(validate_redirect_uri("file:///tmp/callback").is_err());
        assert!(validate_redirect_uri("custom:/callback").is_err());
        assert!(validate_redirect_uri("com.example.app://attacker.example/callback").is_err());
        assert!(validate_redirect_uri("com.example.app:/").is_err());
        assert!(validate_redirect_uri("http://desktop.example/callback").is_err());
    }

    #[test]
    fn authorization_code_metadata_requires_explicit_s256_pkce() {
        let mut metadata = AuthorizationMetadata::default();
        metadata.authorization_endpoint = "https://issuer.example/authorize".into();
        metadata.token_endpoint = "https://issuer.example/token".into();
        assert!(validate_authorization_metadata(&metadata, true).is_err());

        metadata.code_challenge_methods_supported = Some(vec!["plain".into()]);
        assert!(validate_authorization_metadata(&metadata, true).is_err());

        metadata.code_challenge_methods_supported = Some(vec!["S256".into()]);
        assert!(validate_authorization_metadata(&metadata, true).is_ok());
    }

    #[test]
    fn issuerless_authorization_servers_use_distinct_credential_identities() {
        let mut first = AuthorizationMetadata::default();
        first.authorization_endpoint = "https://first.example/authorize".into();
        first.token_endpoint = "https://first.example/token".into();
        let mut second = AuthorizationMetadata::default();
        second.authorization_endpoint = "https://second.example/authorize".into();
        second.token_endpoint = "https://second.example/token".into();

        let first_identity = authorization_server_credential_identity(&first).unwrap();
        let second_identity = authorization_server_credential_identity(&second).unwrap();

        assert_ne!(first_identity, second_identity);
        assert!(first_identity.starts_with("legacy-as:"));
        assert!(second_identity.starts_with("legacy-as:"));
    }

    #[test]
    fn machine_scope_upgrade_is_bounded_and_deduplicated() {
        let mut attempts = HashMap::new();
        assert_eq!(
            reserve_machine_scope_upgrade(&mut attempts, "write read read"),
            Some(vec!["read".to_string(), "write".to_string()])
        );
        assert!(reserve_machine_scope_upgrade(&mut attempts, "").is_none());
        assert!(reserve_machine_scope_upgrade(&mut attempts, "read write").is_some());
        assert!(reserve_machine_scope_upgrade(&mut attempts, "read write").is_some());
        assert!(reserve_machine_scope_upgrade(&mut attempts, "read write").is_none());
        assert!(reserve_machine_scope_upgrade(&mut attempts, "admin").is_none());
    }

    #[tokio::test]
    async fn oauth_discovery_deletes_probe_session() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let deleted = Arc::new(AtomicUsize::new(0));
        let server_base_url = base_url.clone();
        let server_deleted = Arc::clone(&deleted);
        let server = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let base_url = server_base_url.clone();
                let deleted = Arc::clone(&server_deleted);
                tokio::spawn(async move {
                    let service = service_fn(move |request: Request<hyper::body::Incoming>| {
                        let base_url = base_url.clone();
                        let deleted = Arc::clone(&deleted);
                        async move {
                            let method = request.method().clone();
                            let path = request.uri().path().to_string();
                            let is_discovery_delete = request
                                .headers()
                                .get(MCP_SESSION_ID_HEADER)
                                .is_some_and(|value| value == "discovery-session");
                            let _ = request.into_body().collect().await;
                            let response = match (method.as_str(), path.as_str()) {
                                ("GET", "/mcp") => Response::builder()
                                    .status(StatusCode::METHOD_NOT_ALLOWED)
                                    .body(Full::new(Bytes::new()))
                                    .unwrap(),
                                ("POST", "/mcp") => Response::builder()
                                    .status(StatusCode::UNAUTHORIZED)
                                    .header(MCP_SESSION_ID_HEADER, "discovery-session")
                                    .header(
                                        "WWW-Authenticate",
                                        format!(
                                            "Bearer resource_metadata=\"{base_url}/.well-known/oauth-protected-resource\""
                                        ),
                                    )
                                    .body(Full::new(Bytes::new()))
                                    .unwrap(),
                                ("DELETE", "/mcp") if is_discovery_delete => {
                                    deleted.fetch_add(1, Ordering::SeqCst);
                                    Response::new(Full::new(Bytes::new()))
                                }
                                ("GET", "/.well-known/oauth-protected-resource") => {
                                    Response::new(Full::new(Bytes::from(
                                        serde_json::json!({
                                            "resource": format!("{base_url}/mcp"),
                                            "authorization_servers": [&base_url],
                                        })
                                        .to_string(),
                                    )))
                                }
                                ("GET", "/.well-known/oauth-authorization-server") => {
                                    Response::new(Full::new(Bytes::from(
                                        serde_json::json!({
                                            "issuer": base_url,
                                            "authorization_endpoint": format!("{base_url}/authorize"),
                                            "token_endpoint": format!("{base_url}/token"),
                                            "response_types_supported": ["code"],
                                            "code_challenge_methods_supported": ["S256"],
                                        })
                                        .to_string(),
                                    )))
                                }
                                _ => Response::builder()
                                    .status(StatusCode::NOT_FOUND)
                                    .body(Full::new(Bytes::new()))
                                    .unwrap(),
                            };
                            Ok::<_, Infallible>(response)
                        }
                    });
                    let mut builder = hyper::server::conn::http1::Builder::new();
                    builder.keep_alive(false);
                    let _ = builder
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });

        let resource = format!("{base_url}/mcp");
        let client = Arc::new(DiscoveryCleanupOAuthHttpClient::new().unwrap());
        let manager = AuthorizationManager::new_with_oauth_http_client(&resource, client)
            .await
            .unwrap();
        manager.discover_metadata().await.unwrap();

        assert_eq!(deleted.load(Ordering::SeqCst), 1);
        server.abort();
    }

    #[tokio::test]
    async fn protected_resource_headers_do_not_follow_cross_origin_redirects() {
        let cross_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let cross_url = format!("http://{}", cross_listener.local_addr().unwrap());
        let cross_requests = Arc::new(AtomicUsize::new(0));
        let leaked_headers = Arc::new(AtomicUsize::new(0));
        let cross_server = {
            let cross_requests = Arc::clone(&cross_requests);
            let leaked_headers = Arc::clone(&leaked_headers);
            tokio::spawn(async move {
                loop {
                    let (stream, _) = cross_listener.accept().await.unwrap();
                    let cross_requests = Arc::clone(&cross_requests);
                    let leaked_headers = Arc::clone(&leaked_headers);
                    tokio::spawn(async move {
                        let service = service_fn(move |request: Request<hyper::body::Incoming>| {
                            cross_requests.fetch_add(1, Ordering::SeqCst);
                            if request.headers().contains_key("x-tenant-id") {
                                leaked_headers.fetch_add(1, Ordering::SeqCst);
                            }
                            async move {
                                Ok::<_, Infallible>(
                                    Response::builder()
                                        .status(StatusCode::NOT_FOUND)
                                        .body(Full::new(Bytes::new()))
                                        .unwrap(),
                                )
                            }
                        });
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(TokioIo::new(stream), service)
                            .await;
                    });
                }
            })
        };

        let resource_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let resource_url = format!("http://{}", resource_listener.local_addr().unwrap());
        let resource = format!("{resource_url}/mcp");
        let resource_header_requests = Arc::new(AtomicUsize::new(0));
        let resource_server = {
            let resource_header_requests = Arc::clone(&resource_header_requests);
            let resource_url = resource_url.clone();
            let cross_url = cross_url.clone();
            tokio::spawn(async move {
                loop {
                    let (stream, _) = resource_listener.accept().await.unwrap();
                    let resource_header_requests = Arc::clone(&resource_header_requests);
                    let resource_url = resource_url.clone();
                    let cross_url = cross_url.clone();
                    tokio::spawn(async move {
                        let service = service_fn(move |request: Request<hyper::body::Incoming>| {
                            let resource_header_requests = Arc::clone(&resource_header_requests);
                            let resource_url = resource_url.clone();
                            let cross_url = cross_url.clone();
                            async move {
                                if request.headers().get("x-tenant-id").is_some_and(|value| {
                                    value == HeaderValue::from_static("tenant-157")
                                }) {
                                    resource_header_requests.fetch_add(1, Ordering::SeqCst);
                                }
                                let path = request.uri().path();
                                let response = if path
                                    .starts_with("/.well-known/oauth-protected-resource")
                                {
                                    Response::builder()
                                        .status(StatusCode::FOUND)
                                        .header(
                                            http::header::LOCATION,
                                            format!("{cross_url}/capture"),
                                        )
                                        .body(Full::new(Bytes::new()))
                                        .unwrap()
                                } else {
                                    Response::builder()
                                        .status(StatusCode::UNAUTHORIZED)
                                        .header(
                                            http::header::WWW_AUTHENTICATE,
                                            format!(
                                                "Bearer resource_metadata=\"{resource_url}/.well-known/oauth-protected-resource/mcp\""
                                            ),
                                        )
                                        .body(Full::new(Bytes::new()))
                                        .unwrap()
                                };
                                Ok::<_, Infallible>(response)
                            }
                        });
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(TokioIo::new(stream), service)
                            .await;
                    });
                }
            })
        };

        let mut headers = HeaderMap::new();
        headers.insert("x-tenant-id", HeaderValue::from_static("tenant-157"));
        let client = Arc::new(
            DiscoveryCleanupOAuthHttpClient::with_protected_resource_headers(
                &resource, headers, None,
            )
            .unwrap(),
        );
        let manager = AuthorizationManager::new_with_oauth_http_client(&resource, client)
            .await
            .unwrap();
        let _ = manager.discover_metadata().await;

        assert!(
            resource_header_requests.load(Ordering::SeqCst) > 0,
            "protected-resource discovery must receive configured headers"
        );
        assert_eq!(cross_requests.load(Ordering::SeqCst), 0);
        assert_eq!(leaked_headers.load(Ordering::SeqCst), 0);
        resource_server.abort();
        cross_server.abort();
    }

    #[tokio::test]
    async fn private_key_jwt_exchange_and_coordinator_restart_use_stored_token() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let server_base_url = base_url.clone();
        let server = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let base_url = server_base_url.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |request: Request<hyper::body::Incoming>| {
                        let base_url = base_url.clone();
                        async move {
                            let response =
                                match (request.method().as_str(), request.uri().path()) {
                                    ("POST", "/mcp") => Response::builder()
                                        .status(StatusCode::UNAUTHORIZED)
                                        .header(
                                            "WWW-Authenticate",
                                            format!(
                                                "Bearer resource_metadata=\"{base_url}/.well-known/oauth-protected-resource\""
                                            ),
                                        )
                                        .body(Full::new(Bytes::new()))
                                        .unwrap(),
                                    ("GET", "/.well-known/oauth-protected-resource") => {
                                        Response::new(Full::new(Bytes::from(
                                            serde_json::json!({
                                                "resource": format!("{base_url}/mcp"),
                                                "authorization_servers": ["https://issuer.example"],
                                                "scopes_supported": ["tools.read"]
                                            })
                                            .to_string(),
                                        )))
                                    }
                                    _ => Response::builder()
                                        .status(StatusCode::NOT_FOUND)
                                        .body(Full::new(Bytes::new()))
                                        .unwrap(),
                                };
                            Ok::<_, Infallible>(response)
                        }
                    });
                    let mut builder = hyper::server::conn::http1::Builder::new();
                    builder.keep_alive(false);
                    let _ = builder
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });

        let token_forms = Arc::new(StdMutex::new(Vec::new()));
        let oauth_http_client: Arc<dyn OAuthHttpClient> =
            Arc::new(JwtInterceptingOAuthHttpClient {
                delegate: DiscoveryCleanupOAuthHttpClient::new().unwrap(),
                token_forms: Arc::clone(&token_forms),
            });
        let resource = format!("{base_url}/mcp");
        let options = OAuthOptions {
            resource: None,
            scopes: vec!["tools.read".into()],
            client_name: None,
            mode: OAuthClientMode::ClientCredentialsPrivateKeyJwt {
                client_id: "jwt-client".into(),
                private_key_input: "jwt-private-key".into(),
                algorithm: "ES256".into(),
                token_endpoint_audience: Some("https://audience.example/token".into()),
            },
        };
        let credential_store = memory_credential_store();
        let coordinator = OAuthCoordinator::new_with_oauth_http_client(
            test_coordinator_context(Arc::clone(&credential_store)),
            &resource,
            options.clone(),
            Some(Arc::new(JwtTestSecretResolver)),
            reqwest::Client::new(),
            Arc::clone(&oauth_http_client),
        )
        .await
        .unwrap();

        assert!(matches!(
            coordinator
                .begin(OAuthBeginRequest {
                    redirect_uri: "not-a-redirect-uri".into(),
                    required_scope: Some("tools.write".into()),
                })
                .await,
            Err(OAuthError::UnsupportedTransport)
        ));
        assert!(
            token_forms
                .lock()
                .expect("JWT token form lock poisoned")
                .is_empty(),
            "begin_oauth must have no side effects for machine grants"
        );
        coordinator.ensure_machine_authorized().await.unwrap();
        assert_eq!(
            coordinator.status().await,
            OAuthStatus::Authorized {
                scopes: vec!["tools.read".into()]
            }
        );
        {
            let forms = token_forms.lock().expect("JWT token form lock poisoned");
            assert_eq!(forms.len(), 1);
            let form = &forms[0];
            assert_eq!(
                form.get("grant_type").map(String::as_str),
                Some("client_credentials")
            );
            assert_eq!(
                form.get("client_assertion_type").map(String::as_str),
                Some("urn:ietf:params:oauth:client-assertion-type:jwt-bearer")
            );
            assert_eq!(form.get("scope").map(String::as_str), Some("tools.read"));
            assert_eq!(
                form.get("resource").map(String::as_str),
                Some(resource.as_str())
            );
            assert!(!form.contains_key("client_id"));

            let assertion = form.get("client_assertion").unwrap();
            let segments: Vec<&str> = assertion.split('.').collect();
            assert_eq!(segments.len(), 3);
            let header: serde_json::Value =
                serde_json::from_slice(&URL_SAFE_NO_PAD.decode(segments[0]).unwrap()).unwrap();
            let claims: serde_json::Value =
                serde_json::from_slice(&URL_SAFE_NO_PAD.decode(segments[1]).unwrap()).unwrap();
            assert_eq!(header["alg"], "ES256");
            assert_eq!(claims["iss"], "jwt-client");
            assert_eq!(claims["sub"], "jwt-client");
            assert_eq!(claims["aud"], "https://audience.example/token");
            assert!(claims["exp"].as_u64().unwrap() > claims["iat"].as_u64().unwrap());
            assert!(claims["jti"]
                .as_str()
                .is_some_and(|value| !value.is_empty()));
        }

        let restored = OAuthCoordinator::new_with_oauth_http_client(
            test_coordinator_context(credential_store),
            &resource,
            options,
            Some(Arc::new(JwtTestSecretResolver)),
            reqwest::Client::new(),
            oauth_http_client,
        )
        .await
        .unwrap();
        assert_eq!(
            restored.status().await,
            OAuthStatus::Authorized {
                scopes: vec!["tools.read".into()]
            }
        );
        assert_eq!(
            token_forms
                .lock()
                .expect("JWT token form lock poisoned")
                .len(),
            1,
            "coordinator restart should restore the unexpired token"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
        drop(restored.prepare_request().await.unwrap());
        assert_eq!(
            token_forms
                .lock()
                .expect("JWT token form lock poisoned")
                .len(),
            2,
            "an expired private_key_jwt token must be renewed without reconnecting"
        );
        server.abort();
    }

    #[tokio::test]
    async fn private_key_jwt_token_exchange_completes_over_real_tls() {
        const TLS_CERT_PEM: &[u8] = include_bytes!("../tests/fixtures/oauth_tls_cert.pem");
        const TLS_KEY_PEM: &[u8] = include_bytes!("../tests/fixtures/oauth_tls_key.pem");
        const TLS_CA_CERT_PEM: &[u8] = include_bytes!("../tests/fixtures/oauth_tls_ca_cert.pem");

        let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
        let mut certificate_reader = std::io::BufReader::new(TLS_CERT_PEM);
        let certificate_chain = rustls_pemfile::certs(&mut certificate_reader)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let mut key_reader = std::io::BufReader::new(TLS_KEY_PEM);
        let private_key = rustls_pemfile::private_key(&mut key_reader)
            .unwrap()
            .unwrap();
        let tls_config = tokio_rustls::rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certificate_chain, private_key)
            .unwrap();
        let tls_acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls_config));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!(
            "https://localhost:{}",
            listener.local_addr().unwrap().port()
        );
        let token_requests = Arc::new(AtomicUsize::new(0));
        let server_base_url = base_url.clone();
        let server_token_requests = Arc::clone(&token_requests);
        let server = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let stream = tls_acceptor.accept(stream).await.unwrap();
                let base_url = server_base_url.clone();
                let token_requests = Arc::clone(&server_token_requests);
                tokio::spawn(async move {
                    let service = service_fn(move |request: Request<hyper::body::Incoming>| {
                        let base_url = base_url.clone();
                        let token_requests = Arc::clone(&token_requests);
                        async move {
                            let method = request.method().clone();
                            let path = request.uri().path().to_string();
                            let body = request.into_body().collect().await.unwrap().to_bytes();
                            let response = match (method.as_str(), path.as_str()) {
                                ("GET", "/mcp") => Response::builder()
                                    .status(StatusCode::METHOD_NOT_ALLOWED)
                                    .body(Full::new(Bytes::new()))
                                    .unwrap(),
                                ("POST", "/mcp") => Response::builder()
                                    .status(StatusCode::UNAUTHORIZED)
                                    .header(
                                        http::header::WWW_AUTHENTICATE,
                                        format!(
                                            "Bearer resource_metadata=\"{base_url}/.well-known/oauth-protected-resource\""
                                        ),
                                    )
                                    .body(Full::new(Bytes::new()))
                                    .unwrap(),
                                ("GET", "/.well-known/oauth-protected-resource") => {
                                    Response::builder()
                                        .header(http::header::CONTENT_TYPE, "application/json")
                                        .body(Full::new(Bytes::from(
                                            serde_json::json!({
                                                "resource": format!("{base_url}/mcp"),
                                                "authorization_servers": [&base_url],
                                                "scopes_supported": ["tools.read"],
                                            })
                                            .to_string(),
                                        )))
                                        .unwrap()
                                }
                                ("GET", "/.well-known/oauth-authorization-server") => {
                                    Response::builder()
                                        .header(http::header::CONTENT_TYPE, "application/json")
                                        .body(Full::new(Bytes::from(
                                            serde_json::json!({
                                                "issuer": base_url,
                                                "authorization_endpoint": format!("{base_url}/authorize"),
                                                "token_endpoint": format!("{base_url}/token"),
                                                "grant_types_supported": ["client_credentials"],
                                                "token_endpoint_auth_methods_supported": ["private_key_jwt"],
                                                "token_endpoint_auth_signing_alg_values_supported": ["ES256"],
                                            })
                                            .to_string(),
                                        )))
                                        .unwrap()
                                }
                                ("POST", "/token") => {
                                    let form: HashMap<String, String> =
                                        url::form_urlencoded::parse(&body).into_owned().collect();
                                    assert_eq!(
                                        form.get("client_assertion_type").map(String::as_str),
                                        Some(
                                            "urn:ietf:params:oauth:client-assertion-type:jwt-bearer"
                                        )
                                    );
                                    assert!(form
                                        .get("client_assertion")
                                        .is_some_and(|value| value.split('.').count() == 3));
                                    token_requests.fetch_add(1, Ordering::SeqCst);
                                    Response::builder()
                                        .header(http::header::CONTENT_TYPE, "application/json")
                                        .body(Full::new(Bytes::from(
                                            serde_json::json!({
                                                "access_token": "tls-token",
                                                "token_type": "Bearer",
                                                "expires_in": 3600,
                                                "scope": "tools.read",
                                            })
                                            .to_string(),
                                        )))
                                        .unwrap()
                                }
                                _ => Response::builder()
                                    .status(StatusCode::NOT_FOUND)
                                    .body(Full::new(Bytes::new()))
                                    .unwrap(),
                            };
                            Ok::<_, Infallible>(response)
                        }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });

        let certificate = reqwest::Certificate::from_pem(TLS_CA_CERT_PEM).unwrap();
        let http_client = reqwest::Client::builder()
            .tls_certs_only([certificate.clone()])
            .no_proxy()
            .build()
            .unwrap();
        let oauth_http_client: Arc<dyn OAuthHttpClient> =
            Arc::new(TlsFixtureOAuthHttpClient::new(certificate));
        let resource = format!("{base_url}/mcp");
        let coordinator = OAuthCoordinator::new_with_oauth_http_client(
            test_coordinator_context(memory_credential_store()),
            &resource,
            OAuthOptions {
                resource: None,
                scopes: vec!["tools.read".into()],
                client_name: None,
                mode: OAuthClientMode::ClientCredentialsPrivateKeyJwt {
                    client_id: "jwt-client".into(),
                    private_key_input: "jwt-private-key".into(),
                    algorithm: "ES256".into(),
                    token_endpoint_audience: None,
                },
            },
            Some(Arc::new(JwtTestSecretResolver)),
            http_client,
            oauth_http_client,
        )
        .await
        .unwrap();

        let captured_logs = CapturedLogs::default();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(captured_logs.clone())
            .finish();
        coordinator
            .ensure_machine_authorized()
            .with_subscriber(subscriber)
            .await
            .unwrap();
        assert_eq!(token_requests.load(Ordering::SeqCst), 1);
        let logs = captured_logs.text();
        for sensitive in ["tls-token", TEST_EC_PRIVATE_KEY, "p9PptiYIX1DoplcU"] {
            assert!(
                !logs.contains(sensitive),
                "private key material and access tokens must not reach tracing output"
            );
        }
        server.abort();
    }

    #[tokio::test]
    async fn lifecycle_generation_protects_concurrent_pending_expiry_and_refresh() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let token_requests = Arc::new(AtomicUsize::new(0));
        let server_base_url = base_url.clone();
        let server_token_requests = Arc::clone(&token_requests);
        let server = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let base_url = server_base_url.clone();
                let token_requests = Arc::clone(&server_token_requests);
                tokio::spawn(async move {
                    let service = service_fn(move |request: Request<hyper::body::Incoming>| {
                        let base_url = base_url.clone();
                        let token_requests = Arc::clone(&token_requests);
                        async move {
                            let method = request.method().clone();
                            let path = request.uri().path().to_string();
                            let body = request.into_body().collect().await.unwrap().to_bytes();
                            let response = match (method.as_str(), path.as_str()) {
                                ("GET", "/mcp") => Response::builder()
                                    .status(StatusCode::METHOD_NOT_ALLOWED)
                                    .body(Full::new(Bytes::new()))
                                    .unwrap(),
                                ("POST", "/mcp") => Response::builder()
                                    .status(StatusCode::UNAUTHORIZED)
                                    .header(
                                        "WWW-Authenticate",
                                        format!(
                                            "Bearer resource_metadata=\"{base_url}/.well-known/oauth-protected-resource\""
                                        ),
                                    )
                                    .body(Full::new(Bytes::new()))
                                    .unwrap(),
                                ("DELETE", "/mcp") => {
                                    Response::new(Full::new(Bytes::new()))
                                }
                                ("GET", "/.well-known/oauth-protected-resource") => {
                                    Response::new(Full::new(Bytes::from(
                                        serde_json::json!({
                                            "resource": format!("{base_url}/mcp"),
                                            "authorization_servers": [&base_url],
                                            "scopes_supported": ["tools.read"],
                                        })
                                        .to_string(),
                                    )))
                                }
                                ("GET", "/.well-known/oauth-authorization-server") => {
                                    Response::new(Full::new(Bytes::from(
                                        serde_json::json!({
                                            "issuer": base_url,
                                            "authorization_endpoint": format!("{base_url}/authorize"),
                                            "token_endpoint": format!("{base_url}/token"),
                                            "response_types_supported": ["code"],
                                            "grant_types_supported": ["authorization_code", "refresh_token"],
                                            "code_challenge_methods_supported": ["S256"],
                                        })
                                        .to_string(),
                                    )))
                                }
                                ("POST", "/token") => {
                                    let form: HashMap<String, String> =
                                        url::form_urlencoded::parse(&body).into_owned().collect();
                                    let request_number =
                                        token_requests.fetch_add(1, Ordering::SeqCst);
                                    let token = if form.get("grant_type").map(String::as_str)
                                        == Some("refresh_token")
                                    {
                                        serde_json::json!({
                                            "access_token": "refreshed-token",
                                            "token_type": "Bearer",
                                            "expires_in": 3600,
                                            "scope": "tools.read",
                                        })
                                    } else {
                                        serde_json::json!({
                                            "access_token": "initial-token",
                                            "token_type": "Bearer",
                                            "expires_in": 1,
                                            "refresh_token": "refresh-token",
                                            "scope": "tools.read",
                                        })
                                    };
                                    assert!(request_number < 2);
                                    Response::builder()
                                        .header("Content-Type", "application/json")
                                        .body(Full::new(Bytes::from(token.to_string())))
                                        .unwrap()
                                }
                                _ => Response::builder()
                                    .status(StatusCode::NOT_FOUND)
                                    .body(Full::new(Bytes::new()))
                                    .unwrap(),
                            };
                            Ok::<_, Infallible>(response)
                        }
                    });
                    let mut builder = hyper::server::conn::http1::Builder::new();
                    builder.keep_alive(false);
                    let _ = builder
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });

        let resource = format!("{base_url}/mcp");
        let options = OAuthOptions {
            resource: None,
            scopes: vec!["tools.read".into()],
            client_name: None,
            mode: OAuthClientMode::AuthorizationCode {
                registration: OAuthClientRegistration::Preregistered {
                    client_id: "test-client".into(),
                    client_secret_input: None,
                },
            },
        };
        let credential_store = memory_credential_store();
        let peer_coordinator = OAuthCoordinator::new(
            test_coordinator_context(Arc::clone(&credential_store)),
            &resource,
            &resource,
            options.clone(),
            None,
            reqwest::Client::new(),
            HeaderMap::new(),
        )
        .await
        .unwrap();
        let coordinator = Arc::new(
            OAuthCoordinator::new(
                test_coordinator_context(credential_store),
                &resource,
                &resource,
                options,
                None,
                reqwest::Client::new(),
                HeaderMap::new(),
            )
            .await
            .unwrap(),
        );
        let old_generation = peer_coordinator.credential_generation();
        let begin_request = OAuthBeginRequest {
            redirect_uri: "http://127.0.0.1:9876/callback".into(),
            required_scope: None,
        };
        let (first_begin, concurrent_begin) = tokio::join!(
            coordinator.begin(begin_request.clone()),
            coordinator.begin(begin_request.clone())
        );
        let launch = first_begin.unwrap();
        assert_eq!(
            concurrent_begin.unwrap(),
            launch,
            "concurrent identical begin requests must share one pending authorization"
        );
        assert!(matches!(
            coordinator
                .begin(OAuthBeginRequest {
                    redirect_uri: "http://127.0.0.1:9877/callback".into(),
                    required_scope: Some("tools.write".into()),
                })
                .await,
            Err(OAuthError::AuthorizationAlreadyPending)
        ));
        assert!(coordinator.credential_generation() > old_generation);

        let stale_401 = rmcp::transport::streamable_http_client::StreamableHttpError::Auth(
            RmcpAuthError::AuthorizationRequired,
        );
        peer_coordinator
            .observe_streamable_error(Some(&stale_401), old_generation)
            .await;
        let stale_403 = rmcp::transport::streamable_http_client::StreamableHttpError::Auth(
            RmcpAuthError::InsufficientScope {
                required_scope: "tools.write".into(),
                upgrade_url: None,
            },
        );
        peer_coordinator
            .observe_streamable_error(Some(&stale_403), old_generation)
            .await;
        assert_eq!(
            coordinator.status().await,
            OAuthStatus::AuthorizationPending
        );
        assert!(coordinator
            .state_store
            .load(&launch.state)
            .await
            .unwrap()
            .is_some());
        peer_coordinator.clear().await.unwrap();
        assert!(coordinator
            .state_store
            .load(&launch.state)
            .await
            .unwrap()
            .is_some());
        assert_eq!(coordinator.status().await, OAuthStatus::Unauthorized);
        assert!(coordinator
            .state_store
            .load(&launch.state)
            .await
            .unwrap()
            .is_none());
        let replacement = coordinator.begin(begin_request.clone()).await.unwrap();
        assert_ne!(replacement.state, launch.state);
        peer_coordinator.clear().await.unwrap();
        let launch = coordinator.begin(begin_request.clone()).await.unwrap();
        assert_ne!(
            launch.state, replacement.state,
            "begin must replace a pending launch invalidated by a peer clear"
        );
        assert!(matches!(
            coordinator
                .complete(OAuthCallback {
                    code: "authorization-code".into(),
                    state: replacement.state.clone(),
                    issuer: None,
                })
                .await,
            Err(OAuthError::StateMismatch)
        ));
        assert!(coordinator
            .state_store
            .load(&replacement.state)
            .await
            .unwrap()
            .is_none());
        assert!(coordinator
            .state_store
            .load(&launch.state)
            .await
            .unwrap()
            .is_some());
        assert_eq!(
            coordinator.status().await,
            OAuthStatus::AuthorizationPending
        );
        assert!(coordinator.store.load().await.unwrap().is_none());

        coordinator
            .state_store
            .states
            .write()
            .await
            .get_mut(&launch.state)
            .expect("active OAuth state must exist")
            .state
            .created_at = 1;
        assert!(matches!(
            coordinator
                .complete(OAuthCallback {
                    code: "expired-authorization-code".into(),
                    state: launch.state,
                    issuer: Some(base_url.clone()),
                })
                .await,
            Err(OAuthError::AuthorizationExpired)
        ));
        assert_eq!(coordinator.status().await, OAuthStatus::Unauthorized);
        assert!(coordinator.store.load().await.unwrap().is_none());

        coordinator.clear().await.unwrap();
        let issuer_mismatch = coordinator.begin(begin_request.clone()).await.unwrap();
        let issuer_mismatch_state = issuer_mismatch.state.clone();
        assert!(matches!(
            coordinator
                .complete(OAuthCallback {
                    code: "issuer-mismatch-code".into(),
                    state: issuer_mismatch.state,
                    issuer: Some("https://wrong-issuer.example".into()),
                })
                .await,
            Err(OAuthError::IssuerMismatch)
        ));
        assert!(coordinator
            .state_store
            .load(&issuer_mismatch_state)
            .await
            .unwrap()
            .is_some());
        assert_eq!(
            coordinator.status().await,
            OAuthStatus::AuthorizationPending
        );
        assert!(coordinator.store.load().await.unwrap().is_none());

        coordinator.clear().await.unwrap();
        let launch = coordinator.begin(begin_request).await.unwrap();
        let captured_logs = CapturedLogs::default();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(captured_logs.clone())
            .finish();
        async {
            coordinator
                .complete(OAuthCallback {
                    code: "authorization-code".into(),
                    state: launch.state.clone(),
                    issuer: Some(base_url.clone()),
                })
                .await
                .unwrap();
            let before_refresh = coordinator.credential_generation();
            let refreshed_request = coordinator.prepare_request().await.unwrap();
            let refreshed_generation = refreshed_request.generation();
            assert!(refreshed_generation > before_refresh);
            assert_eq!(token_requests.load(Ordering::SeqCst), 2);
            drop(refreshed_request);
        }
        .with_subscriber(subscriber)
        .await;

        let expired_upgrade = coordinator
            .begin(OAuthBeginRequest {
                redirect_uri: "http://127.0.0.1:9876/callback".into(),
                required_scope: Some("tools.write".into()),
            })
            .await
            .unwrap();
        coordinator
            .state_store
            .states
            .write()
            .await
            .get_mut(&expired_upgrade.state)
            .expect("scope-upgrade OAuth state must exist")
            .state
            .created_at = 1;
        assert!(matches!(
            coordinator.status().await,
            OAuthStatus::Authorized { .. }
        ));
        let expired_upgrade_generation = coordinator.credential_generation();
        coordinator
            .observe_streamable_error(Some(&stale_403), expired_upgrade_generation)
            .await;
        assert_eq!(
            coordinator.status().await,
            OAuthStatus::ReauthorizationRequired {
                required_scope: "tools.write".into(),
            },
            "an expired flow must not suppress a later insufficient-scope observation"
        );
        assert!(matches!(
            coordinator
                .complete(OAuthCallback {
                    code: "expired-upgrade-code".into(),
                    state: expired_upgrade.state.clone(),
                    issuer: Some(base_url.clone()),
                })
                .await,
            Err(OAuthError::AuthorizationExpired)
        ));
        assert_eq!(
            coordinator.status().await,
            OAuthStatus::ReauthorizationRequired {
                required_scope: "tools.write".into(),
            },
            "classifying the late callback must not overwrite newer resource feedback"
        );

        let expired_cancellation = coordinator
            .begin(OAuthBeginRequest {
                redirect_uri: "http://127.0.0.1:9876/callback".into(),
                required_scope: Some("tools.write".into()),
            })
            .await
            .unwrap();
        assert_ne!(expired_cancellation.state, expired_upgrade.state);
        coordinator
            .state_store
            .states
            .write()
            .await
            .get_mut(&expired_cancellation.state)
            .expect("replacement scope-upgrade OAuth state must exist")
            .state
            .created_at = 1;
        assert!(matches!(
            coordinator
                .cancel(OAuthCancellation {
                    state: expired_cancellation.state.clone(),
                    issuer: Some(base_url.clone()),
                    reason: OAuthCancellationReason::Timeout,
                })
                .await,
            Err(OAuthError::AuthorizationExpired)
        ));

        let fresh_upgrade = coordinator
            .begin(OAuthBeginRequest {
                redirect_uri: "http://127.0.0.1:9876/callback".into(),
                required_scope: Some("tools.write".into()),
            })
            .await
            .unwrap();
        assert_ne!(fresh_upgrade.state, expired_cancellation.state);
        assert!(matches!(
            coordinator
                .cancel(OAuthCancellation {
                    state: fresh_upgrade.state,
                    issuer: Some(base_url),
                    reason: OAuthCancellationReason::Cancelled,
                })
                .await
                .unwrap(),
            OAuthFlowOutcome::Terminated {
                status: OAuthStatus::Authorized { .. },
                ..
            }
        ));

        let logs = captured_logs.text();
        for sensitive in [
            "authorization-code",
            "initial-token",
            "refresh-token",
            "refreshed-token",
            launch.state.as_str(),
        ] {
            assert!(
                !logs.contains(sensitive),
                "OAuth code, state, and tokens must not reach tracing output"
            );
        }
        let expired_before_401 = coordinator
            .begin(OAuthBeginRequest {
                redirect_uri: "http://127.0.0.1:9876/callback".into(),
                required_scope: Some("tools.write".into()),
            })
            .await
            .unwrap();
        coordinator
            .state_store
            .states
            .write()
            .await
            .get_mut(&expired_before_401.state)
            .expect("scope-upgrade OAuth state before 401 must exist")
            .state
            .created_at = 1;
        assert!(matches!(
            coordinator.status().await,
            OAuthStatus::Authorized { .. }
        ));
        let refreshed_generation = coordinator.credential_generation();
        let refreshed_401 = rmcp::transport::streamable_http_client::StreamableHttpError::Auth(
            RmcpAuthError::AuthorizationRequired,
        );
        coordinator
            .observe_streamable_error(Some(&refreshed_401), refreshed_generation)
            .await;
        assert_eq!(coordinator.status().await, OAuthStatus::Unauthorized);
        assert!(matches!(
            coordinator
                .complete(OAuthCallback {
                    code: "late-code-after-401".into(),
                    state: expired_before_401.state,
                    issuer: None,
                })
                .await,
            Err(OAuthError::StateMismatch)
        ));
        assert!(coordinator.store.load().await.unwrap().is_none());
        server.abort();
    }
}
