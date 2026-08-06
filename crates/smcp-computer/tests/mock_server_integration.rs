/**
* 文件名: mock_server_integration
* 作者: Claude Code
* 创建日期: 2026-03-03
* 描述: HTTP/SSE MCP 客户端 mock server 集成测试
*
* 覆盖 happy path: connect → initialize → list_tools → call_tool → disconnect
* 无需 feature gate 或 #[ignore]，随 `cargo test` 正常运行。
*/
use std::collections::HashMap;
use std::convert::Infallible;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Bytes, Frame};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, Notify};
use tracing_subscriber::fmt::MakeWriter;

use smcp_computer::computer::{Computer, SilentSession};
use smcp_computer::inputs::{InputResolutionError, SecretValueResolver};
use smcp_computer::mcp_clients::http_client::HttpMCPClient;
use smcp_computer::mcp_clients::model::*;
use smcp_computer::mcp_clients::sse_client::SseMCPClient;
use smcp_computer::mcp_clients::MCPServerManager;
use smcp_computer::oauth::{
    InMemoryOAuthCredentialStore, OAuthBeginRequest, OAuthCallback, OAuthCancellation,
    OAuthCancellationReason, OAuthClientMode, OAuthClientRegistration, OAuthCredentialKey,
    OAuthCredentialRecordKind, OAuthCredentialStore, OAuthCredentialStoreError, OAuthError,
    OAuthFlowOutcome, OAuthOptions, OAuthStatus,
};
use smcp_computer::ComputerEvent;
use tempfile::TempDir;

// ============================================================
// BoxBody helper
// ============================================================
type BoxBody = http_body_util::combinators::BoxBody<Bytes, Infallible>;

#[derive(Clone, Default)]
struct CapturedLogs(Arc<StdMutex<Vec<u8>>>);

struct CapturedLogWriter(Arc<StdMutex<Vec<u8>>>);

impl Write for CapturedLogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("log capture lock poisoned")
            .extend(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for CapturedLogs {
    type Writer = CapturedLogWriter;

    fn make_writer(&'a self) -> Self::Writer {
        CapturedLogWriter(Arc::clone(&self.0))
    }
}

impl CapturedLogs {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("log capture lock poisoned")).into_owned()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CredentialStoreOperation {
    Load(OAuthCredentialKey),
    Save(OAuthCredentialKey),
    Delete(OAuthCredentialKey),
}

#[derive(Default)]
struct RecordingOAuthCredentialStore {
    entries: Mutex<HashMap<OAuthCredentialKey, String>>,
    operations: Mutex<Vec<CredentialStoreOperation>>,
    fail_next_credential_save: AtomicBool,
}

impl RecordingOAuthCredentialStore {
    async fn operations(&self) -> Vec<CredentialStoreOperation> {
        self.operations.lock().await.clone()
    }

    async fn credential_entries(&self) -> HashMap<OAuthCredentialKey, String> {
        self.entries
            .lock()
            .await
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()
    }
}

#[async_trait]
impl OAuthCredentialStore for RecordingOAuthCredentialStore {
    async fn load(
        &self,
        key: &OAuthCredentialKey,
    ) -> Result<Option<String>, OAuthCredentialStoreError> {
        self.operations
            .lock()
            .await
            .push(CredentialStoreOperation::Load(key.clone()));
        Ok(self.entries.lock().await.get(key).cloned())
    }

    async fn save(
        &self,
        key: &OAuthCredentialKey,
        value: &str,
    ) -> Result<(), OAuthCredentialStoreError> {
        self.operations
            .lock()
            .await
            .push(CredentialStoreOperation::Save(key.clone()));
        if key.record_kind == OAuthCredentialRecordKind::IssuerIndex
            && self.fail_next_credential_save.swap(false, Ordering::SeqCst)
        {
            return Err(OAuthCredentialStoreError::OperationFailed);
        }
        self.entries
            .lock()
            .await
            .insert(key.clone(), value.to_string());
        Ok(())
    }

    async fn delete(&self, key: &OAuthCredentialKey) -> Result<(), OAuthCredentialStoreError> {
        self.operations
            .lock()
            .await
            .push(CredentialStoreOperation::Delete(key.clone()));
        self.entries.lock().await.remove(key);
        Ok(())
    }
}

#[derive(Default)]
struct DelayedLoadOAuthCredentialStore {
    inner: InMemoryOAuthCredentialStore,
    delay_next_load: AtomicBool,
    load_started: Notify,
    release_load: Notify,
}

#[async_trait]
impl OAuthCredentialStore for DelayedLoadOAuthCredentialStore {
    async fn load(
        &self,
        key: &OAuthCredentialKey,
    ) -> Result<Option<String>, OAuthCredentialStoreError> {
        if self.delay_next_load.swap(false, Ordering::SeqCst) {
            self.load_started.notify_one();
            self.release_load.notified().await;
        }
        self.inner.load(key).await
    }

    async fn save(
        &self,
        key: &OAuthCredentialKey,
        value: &str,
    ) -> Result<(), OAuthCredentialStoreError> {
        self.inner.save(key, value).await
    }

    async fn delete(&self, key: &OAuthCredentialKey) -> Result<(), OAuthCredentialStoreError> {
        self.inner.delete(key).await
    }
}

fn full_body(s: impl Into<Bytes>) -> BoxBody {
    Full::new(s.into()).map_err(|never| match never {}).boxed()
}

fn empty_body() -> BoxBody {
    full_body(Bytes::new())
}

// ============================================================
// Bind random port helper
// ============================================================
async fn bind_random() -> (TcpListener, u16) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    (listener, port)
}

async fn wait_for_count(counter: &AtomicUsize, minimum: usize, message: &str) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while counter.load(Ordering::SeqCst) < minimum {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{message}"));
}

// ============================================================
// JSON-RPC response builders
// ============================================================
fn jsonrpc_response(id: &serde_json::Value, result: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result
    })
}

fn initialize_result() -> serde_json::Value {
    serde_json::json!({
        "protocolVersion": "2024-11-05",
        "serverInfo": { "name": "mock-server", "version": "0.1.0" },
        "capabilities": { "tools": {} }
    })
}

fn tools_list_result() -> serde_json::Value {
    serde_json::json!({
        "tools": [
            {
                "name": "echo",
                "description": "Echo input",
                "inputSchema": { "type": "object", "properties": { "message": { "type": "string" } } }
            },
            {
                "name": "add",
                "description": "Add two numbers",
                "inputSchema": { "type": "object", "properties": { "a": { "type": "number" }, "b": { "type": "number" } } }
            }
        ]
    })
}

fn tool_call_result(tool_name: &str, args: &serde_json::Value) -> serde_json::Value {
    let text = if tool_name == "echo" {
        args.get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("(empty)")
            .to_string()
    } else if tool_name == "add" {
        let a = args.get("a").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let b = args.get("b").and_then(|v| v.as_f64()).unwrap_or(0.0);
        format!("{}", a + b)
    } else {
        format!("unknown tool: {}", tool_name)
    };
    serde_json::json!({
        "content": [{ "type": "text", "text": text }]
    })
}

fn shutdown_result() -> serde_json::Value {
    serde_json::json!({})
}

// ============================================================
// HTTP Mock Server
// ============================================================
struct MockHttpState {
    use_sse_response: bool,
    recorded_headers: Mutex<Vec<(String, String)>>,
    recorded_authorization: Mutex<Vec<(Method, bool)>>,
}

async fn http_mock_handler(
    req: Request<hyper::body::Incoming>,
    state: Arc<MockHttpState>,
) -> Result<Response<BoxBody>, Infallible> {
    // Record headers for assertion
    {
        state.recorded_authorization.lock().await.push((
            req.method().clone(),
            req.headers()
                .get("authorization")
                .is_some_and(|value| value == "Bearer static-token"),
        ));
        let mut recorded = state.recorded_headers.lock().await;
        for (k, v) in req.headers() {
            if let Ok(val) = v.to_str() {
                recorded.push((k.to_string(), val.to_string()));
            }
        }
    }

    if req.method() != Method::POST {
        return Ok(Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .body(empty_body())
            .unwrap());
    }

    let body_bytes = req.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = match serde_json::from_slice(&body_bytes) {
        Ok(v) => v,
        Err(_) => {
            return Ok(Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(full_body("bad json"))
                .unwrap());
        }
    };

    let method = body.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let id = body.get("id").cloned().unwrap_or(serde_json::json!(0));

    // notifications/initialized → 202 no body
    if method.starts_with("notifications/") {
        return Ok(Response::builder()
            .status(StatusCode::ACCEPTED)
            .body(empty_body())
            .unwrap());
    }

    let result = match method {
        "initialize" => initialize_result(),
        "tools/list" => tools_list_result(),
        "tools/call" => {
            let params = body.get("params").cloned().unwrap_or(serde_json::json!({}));
            let tool_name = params
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("unknown");
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or(serde_json::json!({}));
            tool_call_result(tool_name, &args)
        }
        "shutdown" | "exit" => shutdown_result(),
        _ => serde_json::json!({}),
    };

    let resp_json = jsonrpc_response(&id, result);
    let resp_bytes = serde_json::to_vec(&resp_json).unwrap();

    if state.use_sse_response {
        // Return as SSE text/event-stream
        let sse_body = format!(
            "event: message\ndata: {}\n\n",
            String::from_utf8_lossy(&resp_bytes)
        );
        let mut builder = Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "text/event-stream");
        if method == "initialize" {
            builder = builder.header("mcp-session-id", "test-session-001");
        }
        Ok(builder.body(full_body(sse_body)).unwrap())
    } else {
        let mut builder = Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/json");
        if method == "initialize" {
            builder = builder.header("mcp-session-id", "test-session-001");
        }
        Ok(builder.body(full_body(resp_bytes)).unwrap())
    }
}

async fn spawn_http_mock(use_sse_response: bool) -> (u16, Arc<MockHttpState>) {
    let (listener, port) = bind_random().await;
    let state = Arc::new(MockHttpState {
        use_sse_response,
        recorded_headers: Mutex::new(Vec::new()),
        recorded_authorization: Mutex::new(Vec::new()),
    });
    let state_clone = state.clone();

    tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => break,
            };
            let io = TokioIo::new(stream);
            let st = state_clone.clone();
            tokio::spawn(async move {
                let service = service_fn(move |req| {
                    let st = st.clone();
                    async move { http_mock_handler(req, st).await }
                });
                let mut builder = hyper::server::conn::http1::Builder::new();
                builder.keep_alive(false);
                let _ = builder.serve_connection(io, service).await;
            });
        }
    });

    (port, state)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OAuthFixtureMode {
    ClientCredentials,
    ClientCredentialsBasic,
    PreregisteredPublic,
    PreregisteredPublicOidc,
    PreregisteredConfidential,
    Dynamic,
    ClientMetadataDocument,
}

struct OAuthMockState {
    base_url: String,
    resource: StdMutex<String>,
    mode: OAuthFixtureMode,
    mcp_response_sse: bool,
    token_expires_in: u64,
    discovery_response_delay_ms: AtomicU64,
    registration_response_delay_ms: AtomicU64,
    token_response_delay_ms: AtomicU64,
    block_next_mcp_response: AtomicBool,
    discovery_started: Notify,
    registration_started: Notify,
    token_started: Notify,
    mcp_response_started: Notify,
    release_mcp_response: Notify,
    total_requests: AtomicUsize,
    discovery_requests: AtomicUsize,
    token_requests: AtomicUsize,
    registration_requests: AtomicUsize,
    authorized_mcp_requests: AtomicUsize,
    challenge_tools_write_remaining: AtomicUsize,
    reject_authorized_remaining: AtomicUsize,
    reject_token_remaining: AtomicUsize,
    protected_custom_header_requests: AtomicUsize,
    authorization_custom_header_requests: AtomicUsize,
    token_forms: Mutex<Vec<HashMap<String, String>>>,
}

impl OAuthMockState {
    fn resource(&self) -> String {
        self.resource
            .lock()
            .expect("OAuth fixture resource lock poisoned")
            .clone()
    }

    fn set_resource(&self, resource: String) {
        *self
            .resource
            .lock()
            .expect("OAuth fixture resource lock poisoned") = resource;
    }
}

async fn delay_oauth_discovery(state: &OAuthMockState) {
    state.discovery_requests.fetch_add(1, Ordering::SeqCst);
    state.discovery_started.notify_one();
    let delay_ms = state.discovery_response_delay_ms.load(Ordering::SeqCst);
    if delay_ms > 0 {
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
    }
}

async fn oauth_http_mock_handler(
    req: Request<hyper::body::Incoming>,
    state: Arc<OAuthMockState>,
) -> Result<Response<BoxBody>, Infallible> {
    state.total_requests.fetch_add(1, Ordering::SeqCst);
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    if req
        .headers()
        .get("x-tenant-id")
        .is_some_and(|value| value == "tenant-157")
    {
        if path == "/mcp" || path.starts_with("/.well-known/oauth-protected-resource") {
            state
                .protected_custom_header_requests
                .fetch_add(1, Ordering::SeqCst);
        } else {
            state
                .authorization_custom_header_requests
                .fetch_add(1, Ordering::SeqCst);
        }
    }
    let authorized = req
        .headers()
        .get("authorization")
        .is_some_and(|value| value == "Bearer oauth-e2e-token");
    let basic_authorized = req
        .headers()
        .get("authorization")
        .is_some_and(|value| value == "Basic b2F1dGgtZTJlLWNsaWVudDpvYXV0aC1lMmUtc2VjcmV0");
    let body_bytes = req.into_body().collect().await.unwrap().to_bytes();

    if method == Method::GET && path == "/.well-known/oauth-protected-resource/mcp" {
        delay_oauth_discovery(&state).await;
        return Ok(Response::builder()
            .header("Content-Type", "application/json")
            .body(full_body(
                serde_json::json!({
                    "resource": state.resource(),
                    "authorization_servers": [&state.base_url],
                    "scopes_supported": ["tools.read"],
                })
                .to_string(),
            ))
            .unwrap());
    }
    if method == Method::GET && path == "/.well-known/oauth-protected-resource" {
        delay_oauth_discovery(&state).await;
        return Ok(Response::builder()
            .header("Content-Type", "application/json")
            .body(full_body(
                serde_json::json!({
                    "resource": state.resource(),
                    "authorization_servers": [&state.base_url],
                    "scopes_supported": ["tools.read"],
                })
                .to_string(),
            ))
            .unwrap());
    }
    if method == Method::GET
        && matches!(
            path.as_str(),
            "/.well-known/oauth-authorization-server"
                | "/.well-known/oauth-authorization-server/mcp"
        )
    {
        delay_oauth_discovery(&state).await;
        if state.mode == OAuthFixtureMode::PreregisteredPublicOidc {
            return Ok(Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(empty_body())
                .unwrap());
        }
        let token_auth_methods = if state.mode == OAuthFixtureMode::ClientCredentialsBasic {
            serde_json::json!(["client_secret_basic"])
        } else {
            serde_json::json!(["none", "client_secret_post"])
        };
        return Ok(Response::builder()
            .header("Content-Type", "application/json")
            .body(full_body(
                serde_json::json!({
                    "issuer": state.base_url,
                    "authorization_endpoint": format!("{}/authorize", state.base_url),
                    "token_endpoint": format!("{}/token", state.base_url),
                    "registration_endpoint": format!("{}/register", state.base_url),
                    "response_types_supported": ["code"],
                    "grant_types_supported": ["authorization_code", "client_credentials"],
                    "token_endpoint_auth_methods_supported": token_auth_methods,
                    "code_challenge_methods_supported": ["S256"],
                    "client_id_metadata_document_supported": true,
                    "authorization_response_iss_parameter_supported": true,
                })
                .to_string(),
            ))
            .unwrap());
    }
    if method == Method::GET && path.ends_with("/.well-known/openid-configuration") {
        return Ok(Response::builder()
            .header("Content-Type", "application/json")
            .body(full_body(
                serde_json::json!({
                    "issuer": state.base_url,
                    "authorization_endpoint": format!("{}/authorize", state.base_url),
                    "token_endpoint": format!("{}/token", state.base_url),
                    "response_types_supported": ["code"],
                    "grant_types_supported": ["authorization_code"],
                    "token_endpoint_auth_methods_supported": ["none"],
                    "code_challenge_methods_supported": ["S256"],
                })
                .to_string(),
            ))
            .unwrap());
    }
    if method == Method::POST && path == "/register" {
        state.registration_requests.fetch_add(1, Ordering::SeqCst);
        state.registration_started.notify_one();
        let delay_ms = state.registration_response_delay_ms.load(Ordering::SeqCst);
        if delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }
        let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        if body["token_endpoint_auth_method"] != "none"
            || body["response_types"] != serde_json::json!(["code"])
        {
            return Ok(Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(empty_body())
                .unwrap());
        }
        return Ok(Response::builder()
            .header("Content-Type", "application/json")
            .body(full_body(
                serde_json::json!({
                    "client_id": "oauth-dcr-client",
                    "client_name": "A2C Computer",
                    "redirect_uris": body["redirect_uris"],
                })
                .to_string(),
            ))
            .unwrap());
    }
    if method == Method::POST && path == "/token" {
        let form: HashMap<String, String> = url::form_urlencoded::parse(&body_bytes)
            .into_owned()
            .collect();
        let expected_client_id = match state.mode {
            OAuthFixtureMode::ClientCredentials | OAuthFixtureMode::ClientCredentialsBasic => {
                "oauth-e2e-client"
            }
            OAuthFixtureMode::PreregisteredPublic
            | OAuthFixtureMode::PreregisteredPublicOidc
            | OAuthFixtureMode::PreregisteredConfidential => "oauth-code-client",
            OAuthFixtureMode::Dynamic => "oauth-dcr-client",
            OAuthFixtureMode::ClientMetadataDocument => "https://client.example/oauth-client.json",
        };
        let valid_grant = match state.mode {
            OAuthFixtureMode::ClientCredentials => {
                form.get("grant_type").map(String::as_str) == Some("client_credentials")
                    && form.get("client_secret").map(String::as_str) == Some("oauth-e2e-secret")
            }
            OAuthFixtureMode::ClientCredentialsBasic => {
                form.get("grant_type").map(String::as_str) == Some("client_credentials")
                    && !form.contains_key("client_secret")
                    && basic_authorized
            }
            _ => {
                form.get("grant_type").map(String::as_str) == Some("authorization_code")
                    && form.get("code").map(String::as_str) == Some("authorization-code")
                    && form
                        .get("code_verifier")
                        .is_some_and(|value| !value.is_empty())
                    && match state.mode {
                        OAuthFixtureMode::PreregisteredConfidential => {
                            form.get("client_secret").map(String::as_str)
                                == Some("oauth-e2e-secret")
                        }
                        _ => !form.contains_key("client_secret"),
                    }
            }
        };
        let valid_client_id = if state.mode == OAuthFixtureMode::ClientCredentialsBasic {
            !form.contains_key("client_id")
        } else {
            form.get("client_id").map(String::as_str) == Some(expected_client_id)
        };
        if !valid_client_id
            || !valid_grant
            || form.get("resource").map(String::as_str) != Some(state.resource().as_str())
        {
            return Ok(Response::builder()
                .status(StatusCode::UNAUTHORIZED)
                .body(empty_body())
                .unwrap());
        }
        let request_index = state.token_requests.fetch_add(1, Ordering::SeqCst);
        state.token_started.notify_one();
        if state
            .reject_token_remaining
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
            return Ok(Response::builder()
                .status(StatusCode::UNAUTHORIZED)
                .body(empty_body())
                .unwrap());
        }
        state.token_forms.lock().await.push(form.clone());
        let token_response_delay_ms = state.token_response_delay_ms.load(Ordering::SeqCst);
        if token_response_delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(token_response_delay_ms)).await;
        }
        let granted_scope = form.get("scope").cloned().unwrap_or_else(|| {
            if !matches!(
                state.mode,
                OAuthFixtureMode::ClientCredentials | OAuthFixtureMode::ClientCredentialsBasic
            ) && request_index > 0
            {
                "tools.read tools.write".to_string()
            } else {
                "tools.read".to_string()
            }
        });
        return Ok(Response::builder()
            .header("Content-Type", "application/json")
            .body(full_body(
                serde_json::json!({
                    "access_token": "oauth-e2e-token",
                    "token_type": "Bearer",
                    "expires_in": state.token_expires_in,
                    "scope": granted_scope,
                })
                .to_string(),
            ))
            .unwrap());
    }
    let reject_authorized = path == "/mcp"
        && authorized
        && state
            .reject_authorized_remaining
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok();
    if path == "/mcp" && (!authorized || reject_authorized) {
        return Ok(Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header(
                "WWW-Authenticate",
                format!(
                    "Bearer resource_metadata=\"{}/.well-known/oauth-protected-resource/mcp\"",
                    state.base_url
                ),
            )
            .body(empty_body())
            .unwrap());
    }
    if method == Method::GET && path == "/mcp" {
        return Ok(Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .body(empty_body())
            .unwrap());
    }
    if method == Method::DELETE && path == "/mcp" {
        return Ok(Response::new(empty_body()));
    }
    if method != Method::POST || path != "/mcp" {
        return Ok(Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(empty_body())
            .unwrap());
    }

    state.authorized_mcp_requests.fetch_add(1, Ordering::SeqCst);
    let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    let rpc_method = body
        .get("method")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let challenge_tools_write = rpc_method == "tools/call"
        && state
            .challenge_tools_write_remaining
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok();
    if challenge_tools_write {
        return Ok(Response::builder()
            .status(StatusCode::FORBIDDEN)
            .header(
                "WWW-Authenticate",
                r#"Bearer error="insufficient_scope", scope="tools.write""#,
            )
            .body(empty_body())
            .unwrap());
    }
    if rpc_method.starts_with("notifications/") {
        return Ok(Response::builder()
            .status(StatusCode::ACCEPTED)
            .body(empty_body())
            .unwrap());
    }
    if state.block_next_mcp_response.swap(false, Ordering::SeqCst) {
        state.mcp_response_started.notify_one();
        state.release_mcp_response.notified().await;
    }
    let id = body.get("id").cloned().unwrap_or(serde_json::json!(0));
    let result = match rpc_method {
        "initialize" => initialize_result(),
        "tools/list" => tools_list_result(),
        "tools/call" => tool_call_result(
            body.pointer("/params/name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown"),
            body.pointer("/params/arguments")
                .unwrap_or(&serde_json::Value::Null),
        ),
        _ => serde_json::json!({}),
    };
    let response = jsonrpc_response(&id, result);
    if state.mcp_response_sse {
        Ok(Response::builder()
            .header("Content-Type", "text/event-stream")
            .header("mcp-session-id", "oauth-e2e-session")
            .body(full_body(format!(
                "event: message\ndata: {}\n\n",
                serde_json::to_string(&response).unwrap()
            )))
            .unwrap())
    } else {
        Ok(Response::builder()
            .header("Content-Type", "application/json")
            .header("mcp-session-id", "oauth-e2e-session")
            .body(full_body(serde_json::to_vec(&response).unwrap()))
            .unwrap())
    }
}

async fn spawn_oauth_http_mock(
    mode: OAuthFixtureMode,
    challenge_tools_write_count: usize,
) -> (u16, Arc<OAuthMockState>) {
    spawn_oauth_http_mock_with_expiry(mode, challenge_tools_write_count, 3600).await
}

async fn spawn_oauth_http_mock_with_expiry(
    mode: OAuthFixtureMode,
    challenge_tools_write_count: usize,
    token_expires_in: u64,
) -> (u16, Arc<OAuthMockState>) {
    spawn_oauth_http_mock_with_options(mode, challenge_tools_write_count, token_expires_in, false)
        .await
}

async fn spawn_oauth_http_sse_mock(
    mode: OAuthFixtureMode,
    challenge_tools_write_count: usize,
) -> (u16, Arc<OAuthMockState>) {
    spawn_oauth_http_mock_with_options(mode, challenge_tools_write_count, 3600, true).await
}

async fn spawn_oauth_http_mock_with_options(
    mode: OAuthFixtureMode,
    challenge_tools_write_count: usize,
    token_expires_in: u64,
    mcp_response_sse: bool,
) -> (u16, Arc<OAuthMockState>) {
    let (listener, port) = bind_random().await;
    let base_url = format!("http://127.0.0.1:{port}");
    let state = Arc::new(OAuthMockState {
        resource: StdMutex::new(format!("{base_url}/mcp")),
        base_url,
        mode,
        mcp_response_sse,
        token_expires_in,
        discovery_response_delay_ms: AtomicU64::new(0),
        registration_response_delay_ms: AtomicU64::new(0),
        token_response_delay_ms: AtomicU64::new(0),
        block_next_mcp_response: AtomicBool::new(false),
        discovery_started: Notify::new(),
        registration_started: Notify::new(),
        token_started: Notify::new(),
        mcp_response_started: Notify::new(),
        release_mcp_response: Notify::new(),
        total_requests: AtomicUsize::new(0),
        discovery_requests: AtomicUsize::new(0),
        token_requests: AtomicUsize::new(0),
        registration_requests: AtomicUsize::new(0),
        authorized_mcp_requests: AtomicUsize::new(0),
        challenge_tools_write_remaining: AtomicUsize::new(challenge_tools_write_count),
        reject_authorized_remaining: AtomicUsize::new(0),
        reject_token_remaining: AtomicUsize::new(0),
        protected_custom_header_requests: AtomicUsize::new(0),
        authorization_custom_header_requests: AtomicUsize::new(0),
        token_forms: Mutex::new(Vec::new()),
    });
    let server_state = Arc::clone(&state);
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let state = Arc::clone(&server_state);
            tokio::spawn(async move {
                let service =
                    service_fn(move |request| oauth_http_mock_handler(request, Arc::clone(&state)));
                let mut builder = hyper::server::conn::http1::Builder::new();
                builder.keep_alive(false);
                let _ = builder
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    (port, state)
}

struct OAuthTestSecretResolver;

#[async_trait]
impl SecretValueResolver for OAuthTestSecretResolver {
    async fn resolve_secret(
        &self,
        def: &MCPServerInput,
    ) -> Result<Option<String>, InputResolutionError> {
        Ok((def.id() == "oauth-e2e-secret-input").then(|| "oauth-e2e-secret".to_string()))
    }
}

struct CountingOAuthTestSecretResolver {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl SecretValueResolver for CountingOAuthTestSecretResolver {
    async fn resolve_secret(
        &self,
        def: &MCPServerInput,
    ) -> Result<Option<String>, InputResolutionError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok((def.id() == "oauth-e2e-secret-input").then(|| "oauth-e2e-secret".to_string()))
    }
}

struct FailOnCallOAuthTestSecretResolver {
    calls: AtomicUsize,
    fail_on_call: usize,
}

struct BlockingOnCallOAuthTestSecretResolver {
    calls: AtomicUsize,
    block_on_call: usize,
    started: Notify,
    release: Notify,
}

#[async_trait]
impl SecretValueResolver for BlockingOnCallOAuthTestSecretResolver {
    async fn resolve_secret(
        &self,
        def: &MCPServerInput,
    ) -> Result<Option<String>, InputResolutionError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if call == self.block_on_call {
            self.started.notify_one();
            self.release.notified().await;
        }
        Ok((def.id() == "oauth-e2e-secret-input").then(|| "oauth-e2e-secret".to_string()))
    }
}

#[async_trait]
impl SecretValueResolver for FailOnCallOAuthTestSecretResolver {
    async fn resolve_secret(
        &self,
        def: &MCPServerInput,
    ) -> Result<Option<String>, InputResolutionError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if call == self.fail_on_call {
            return Err(InputResolutionError::ResolverFailed {
                id: def.id().to_string(),
                reason: "injected OAuth resolver failure".to_string(),
            });
        }
        Ok((def.id() == "oauth-e2e-secret-input").then(|| "oauth-e2e-secret".to_string()))
    }
}

// ============================================================
// SSE Mock Server
// ============================================================
type SseFrameSender = tokio::sync::mpsc::UnboundedSender<Result<Frame<Bytes>, Infallible>>;

struct MockSseState {
    sse_tx: Mutex<Option<SseFrameSender>>,
    post_json_response: bool,
}

async fn sse_mock_handler(
    req: Request<hyper::body::Incoming>,
    state: Arc<MockSseState>,
) -> Result<Response<BoxBody>, Infallible> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    if method == Method::GET && path == "/sse" {
        // SSE endpoint: send endpoint event then keep alive
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Result<Frame<Bytes>, Infallible>>();

        // Send endpoint event immediately
        let endpoint_event = "event: endpoint\ndata: /messages\n\n";
        let _ = tx.send(Ok(Frame::data(Bytes::from(endpoint_event))));

        // Store tx for POST handler to push responses
        *state.sse_tx.lock().await = Some(tx);

        let stream = futures_util::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        });
        let body = StreamBody::new(stream);
        let boxed: BoxBody = http_body_util::BodyExt::boxed(body);

        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "text/event-stream")
            .header("Cache-Control", "no-cache")
            .header("Connection", "keep-alive")
            .body(boxed)
            .unwrap());
    }

    if method == Method::POST && path == "/messages" {
        let body_bytes = req.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = match serde_json::from_slice(&body_bytes) {
            Ok(v) => v,
            Err(_) => {
                return Ok(Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .body(full_body("bad json"))
                    .unwrap());
            }
        };

        let rpc_method = body.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let id = body.get("id").cloned().unwrap_or(serde_json::json!(0));

        // notifications → 202
        if rpc_method.starts_with("notifications/") {
            return Ok(Response::builder()
                .status(StatusCode::ACCEPTED)
                .body(empty_body())
                .unwrap());
        }

        let result = match rpc_method {
            "initialize" => initialize_result(),
            "tools/list" => tools_list_result(),
            "tools/call" => {
                let params = body.get("params").cloned().unwrap_or(serde_json::json!({}));
                let tool_name = params
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("unknown");
                let args = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or(serde_json::json!({}));
                tool_call_result(tool_name, &args)
            }
            "shutdown" | "exit" => shutdown_result(),
            _ => serde_json::json!({}),
        };

        let resp_json = jsonrpc_response(&id, result);

        if state.post_json_response {
            // Return JSON directly via POST response
            let resp_bytes = serde_json::to_vec(&resp_json).unwrap();
            return Ok(Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", "application/json")
                .body(full_body(resp_bytes))
                .unwrap());
        }

        // Push response via SSE stream
        let sse_event = format!(
            "event: message\ndata: {}\n\n",
            serde_json::to_string(&resp_json).unwrap()
        );
        if let Some(ref tx) = *state.sse_tx.lock().await {
            let _ = tx.send(Ok(Frame::data(Bytes::from(sse_event))));
        }

        // POST returns 202 when response is pushed via SSE
        return Ok(Response::builder()
            .status(StatusCode::ACCEPTED)
            .body(empty_body())
            .unwrap());
    }

    Ok(Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(full_body("not found"))
        .unwrap())
}

async fn spawn_sse_mock(post_json_response: bool) -> (u16, Arc<MockSseState>) {
    let (listener, port) = bind_random().await;
    let state = Arc::new(MockSseState {
        sse_tx: Mutex::new(None),
        post_json_response,
    });
    let state_clone = state.clone();

    tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => break,
            };
            let io = TokioIo::new(stream);
            let st = state_clone.clone();
            tokio::spawn(async move {
                let service = service_fn(move |req| {
                    let st = st.clone();
                    async move { sse_mock_handler(req, st).await }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, service)
                    .await;
            });
        }
    });

    (port, state)
}

// ============================================================
// Test 1: HTTP happy path (JSON responses)
// ============================================================
#[tokio::test]
async fn test_http_happy_path_json() {
    let (port, _state) = spawn_http_mock(false).await;

    let client = HttpMCPClient::new(HttpServerParameters {
        url: format!("http://127.0.0.1:{}", port),
        headers: HashMap::new(),
    });

    // connect (initialize + notifications/initialized)
    client.connect().await.unwrap();
    assert_eq!(client.state(), ClientState::Connected);

    // list_tools
    let tools = client.list_tools().await.unwrap();
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0].name, "echo");
    assert_eq!(tools[1].name, "add");

    // call_tool
    let result = client
        .call_tool("echo", serde_json::json!({"message": "hello"}))
        .await
        .unwrap();
    assert_eq!(result.content.len(), 1);
    assert_eq!(
        result.content[0]
            .as_text()
            .expect("expected text content")
            .text,
        "hello"
    );

    // disconnect
    client.disconnect().await.unwrap();
    assert_eq!(client.state(), ClientState::Disconnected);
}

#[tokio::test]
async fn test_streamable_http_static_authorization_header_reaches_every_http_method() {
    let (port, state) = spawn_http_mock(false).await;
    let client = HttpMCPClient::new(HttpServerParameters {
        url: format!("http://127.0.0.1:{port}"),
        headers: HashMap::from([(
            "Authorization".to_string(),
            "Bearer static-token".to_string(),
        )]),
    });

    client.connect().await.unwrap();
    assert_eq!(client.list_tools().await.unwrap().len(), 2);
    tokio::time::sleep(Duration::from_millis(100)).await;
    client.disconnect().await.unwrap();

    let requests = state.recorded_authorization.lock().await;
    assert!(
        requests.iter().all(|(_, authorized)| *authorized),
        "static Authorization must be present on every request: {requests:?}"
    );
    for method in [Method::POST, Method::GET, Method::DELETE] {
        assert!(
            requests.iter().any(|(observed, _)| observed == method),
            "expected a real {method} request, got {requests:?}"
        );
    }
}

#[tokio::test]
async fn test_streamable_http_oauth_client_credentials_end_to_end() {
    let captured_logs = CapturedLogs::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(captured_logs.clone())
        .finish();
    let _subscriber_guard = tracing::subscriber::set_default(subscriber);
    let (port, state) = spawn_oauth_http_mock(OAuthFixtureMode::ClientCredentials, 0).await;
    let client = HttpMCPClient::new(HttpServerParameters {
        url: format!("http://127.0.0.1:{port}/mcp"),
        headers: HashMap::new(),
    })
    .with_oauth(
        OAuthOptions {
            resource: None,
            scopes: vec!["tools.read".to_string()],
            client_name: None,
            mode: OAuthClientMode::ClientCredentialsSecret {
                client_id: "oauth-e2e-client".to_string(),
                client_secret_input: "oauth-e2e-secret-input".to_string(),
            },
        },
        Some(Arc::new(OAuthTestSecretResolver)),
    )
    .with_ephemeral_oauth_credentials();

    client.connect().await.unwrap();
    let tools = client.list_tools().await.unwrap();
    assert_eq!(tools.len(), 2);
    let result = client
        .call_tool("echo", serde_json::json!({"message": "oauth works"}))
        .await
        .unwrap();
    assert_eq!(
        result.content[0]
            .as_text()
            .expect("expected text content")
            .text,
        "oauth works"
    );
    client.disconnect().await.unwrap();

    assert_eq!(state.token_requests.load(Ordering::SeqCst), 1);
    assert!(
        state.authorized_mcp_requests.load(Ordering::SeqCst) >= 3,
        "initialize, tools/list, and tools/call must carry the OAuth token"
    );
    let logs = captured_logs.text();
    for sensitive in ["oauth-e2e-secret", "oauth-e2e-token"] {
        assert!(
            !logs.contains(sensitive),
            "client secret and access token must not reach tracing output"
        );
    }
}

#[tokio::test]
async fn test_oauth_custom_headers_stay_on_protected_resource_requests() {
    let (port, state) = spawn_oauth_http_mock(OAuthFixtureMode::ClientCredentials, 0).await;
    let client = HttpMCPClient::new(HttpServerParameters {
        url: format!("http://127.0.0.1:{port}/mcp"),
        headers: HashMap::from([("X-Tenant-Id".to_string(), "tenant-157".to_string())]),
    })
    .with_oauth(
        OAuthOptions {
            resource: None,
            scopes: vec!["tools.read".to_string()],
            client_name: None,
            mode: OAuthClientMode::ClientCredentialsSecret {
                client_id: "oauth-e2e-client".to_string(),
                client_secret_input: "oauth-e2e-secret-input".to_string(),
            },
        },
        Some(Arc::new(OAuthTestSecretResolver)),
    )
    .with_ephemeral_oauth_credentials();

    client.connect().await.unwrap();
    client.list_tools().await.unwrap();
    client.disconnect().await.unwrap();

    assert!(
        state
            .protected_custom_header_requests
            .load(Ordering::SeqCst)
            >= 3,
        "resource discovery and authenticated MCP requests must carry custom headers"
    );
    assert_eq!(
        state
            .authorization_custom_header_requests
            .load(Ordering::SeqCst),
        0,
        "authorization metadata and token requests must not receive resource headers"
    );
}

#[tokio::test]
async fn test_streamable_http_client_credentials_supports_client_secret_basic() {
    let (port, state) = spawn_oauth_http_mock(OAuthFixtureMode::ClientCredentialsBasic, 0).await;
    let client = HttpMCPClient::new(HttpServerParameters {
        url: format!("http://127.0.0.1:{port}/mcp"),
        headers: HashMap::new(),
    })
    .with_oauth(
        OAuthOptions {
            resource: None,
            scopes: vec!["tools.read".to_string()],
            client_name: None,
            mode: OAuthClientMode::ClientCredentialsSecret {
                client_id: "oauth-e2e-client".to_string(),
                client_secret_input: "oauth-e2e-secret-input".to_string(),
            },
        },
        Some(Arc::new(OAuthTestSecretResolver)),
    )
    .with_ephemeral_oauth_credentials();

    client.connect().await.unwrap();
    assert_eq!(client.list_tools().await.unwrap().len(), 2);
    client.disconnect().await.unwrap();

    assert_eq!(state.token_requests.load(Ordering::SeqCst), 1);
    let forms = state.token_forms.lock().await;
    assert!(!forms[0].contains_key("client_id"));
    assert!(!forms[0].contains_key("client_secret"));
}

#[tokio::test]
async fn test_streamable_http_client_credentials_renews_expired_token_without_reconnect() {
    let (port, state) =
        spawn_oauth_http_mock_with_expiry(OAuthFixtureMode::ClientCredentials, 0, 31).await;
    let client = HttpMCPClient::new(HttpServerParameters {
        url: format!("http://127.0.0.1:{port}/mcp"),
        headers: HashMap::new(),
    })
    .with_oauth(
        OAuthOptions {
            resource: None,
            scopes: vec!["tools.read".to_string()],
            client_name: None,
            mode: OAuthClientMode::ClientCredentialsSecret {
                client_id: "oauth-e2e-client".to_string(),
                client_secret_input: "oauth-e2e-secret-input".to_string(),
            },
        },
        Some(Arc::new(OAuthTestSecretResolver)),
    )
    .with_ephemeral_oauth_credentials();

    client.connect().await.unwrap();
    assert_eq!(state.token_requests.load(Ordering::SeqCst), 1);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(client.list_tools().await.unwrap().len(), 2);
    assert_eq!(
        state.token_requests.load(Ordering::SeqCst),
        2,
        "an expired client-secret token must be renewed without reconnecting"
    );
    client.disconnect().await.unwrap();
}

#[tokio::test]
async fn test_machine_begin_oauth_facade_has_no_network_or_secret_side_effects() {
    let (port, state) = spawn_oauth_http_mock(OAuthFixtureMode::ClientCredentials, 0).await;
    let bundle_id = BundleId::try_from("oauth-machine-begin").unwrap();
    let manager = MCPServerManager::new();
    let secret_calls = Arc::new(AtomicUsize::new(0));
    manager
        .set_secret_resolver(Some(Arc::new(CountingOAuthTestSecretResolver {
            calls: Arc::clone(&secret_calls),
        })))
        .await;
    let mut config = HttpServerConfig::new(
        "oauth-machine-begin",
        HttpServerParameters {
            url: format!("http://127.0.0.1:{port}/mcp"),
            headers: HashMap::new(),
        },
    );
    config.bundle_id = Some(bundle_id.clone());
    config.oauth = Some(OAuthOptions {
        resource: None,
        scopes: vec!["tools.read".to_string()],
        client_name: None,
        mode: OAuthClientMode::ClientCredentialsSecret {
            client_id: "oauth-e2e-client".to_string(),
            client_secret_input: "oauth-e2e-secret-input".to_string(),
        },
    });
    manager
        .add_or_update_server(MCPServerConfig::Http(config))
        .await
        .unwrap();

    assert!(matches!(
        manager
            .begin_oauth(
                &bundle_id,
                OAuthBeginRequest {
                    redirect_uri: "not-a-redirect-uri".to_string(),
                    required_scope: Some("tools.write".to_string()),
                },
            )
            .await,
        Err(OAuthError::UnsupportedTransport)
    ));
    assert_eq!(state.total_requests.load(Ordering::SeqCst), 0);
    assert_eq!(secret_calls.load(Ordering::SeqCst), 0);
    manager.close().await.unwrap();
}

#[tokio::test]
async fn test_machine_insufficient_scope_retries_are_bounded_end_to_end() {
    let (port, state) = spawn_oauth_http_mock(OAuthFixtureMode::ClientCredentials, 4).await;
    let bundle_id = BundleId::try_from("oauth-machine").unwrap();
    let manager = MCPServerManager::new();
    manager
        .set_secret_resolver(Some(Arc::new(OAuthTestSecretResolver)))
        .await;
    let mut config = HttpServerConfig::new(
        "oauth-machine",
        HttpServerParameters {
            url: format!("http://127.0.0.1:{port}/mcp"),
            headers: HashMap::new(),
        },
    );
    config.bundle_id = Some(bundle_id.clone());
    config.oauth = Some(OAuthOptions {
        resource: None,
        scopes: vec!["tools.read".to_string()],
        client_name: None,
        mode: OAuthClientMode::ClientCredentialsSecret {
            client_id: "oauth-e2e-client".to_string(),
            client_secret_input: "oauth-e2e-secret-input".to_string(),
        },
    });
    manager
        .add_or_update_server(MCPServerConfig::Http(config))
        .await
        .unwrap();
    manager.start_client_by_id(&bundle_id).await.unwrap();

    for attempt in 0..4 {
        let result = manager
            .call_tool(
                bundle_id.as_str(),
                "echo",
                serde_json::json!({"message": "scope challenge"}),
                None,
            )
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true));
        let status = manager.oauth_status(&bundle_id).await.unwrap();
        assert_eq!(
            status,
            OAuthStatus::ReauthorizationRequired {
                required_scope: "tools.write".to_string()
            },
            "replacement token {attempt} is provisional until the resource accepts it"
        );
    }
    assert_eq!(
        state.token_requests.load(Ordering::SeqCst),
        4,
        "initial grant plus exactly three bounded scope upgrades"
    );
    let recovered = manager
        .call_tool(
            bundle_id.as_str(),
            "echo",
            serde_json::json!({"message": "scope recovered"}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(recovered.is_error, None);
    assert!(matches!(
        manager.oauth_status(&bundle_id).await.unwrap(),
        OAuthStatus::Authorized { .. }
    ));
    assert_eq!(
        state.token_requests.load(Ordering::SeqCst),
        4,
        "a successful protected request must not bypass the retry bound"
    );
    manager.close().await.unwrap();
}

fn authorization_code_options(mode: OAuthFixtureMode) -> OAuthOptions {
    let registration = match mode {
        OAuthFixtureMode::PreregisteredPublic | OAuthFixtureMode::PreregisteredPublicOidc => {
            OAuthClientRegistration::Preregistered {
                client_id: "oauth-code-client".to_string(),
                client_secret_input: None,
            }
        }
        OAuthFixtureMode::PreregisteredConfidential => OAuthClientRegistration::Preregistered {
            client_id: "oauth-code-client".to_string(),
            client_secret_input: Some("oauth-e2e-secret-input".to_string()),
        },
        OAuthFixtureMode::Dynamic => OAuthClientRegistration::Dynamic,
        OAuthFixtureMode::ClientMetadataDocument => {
            OAuthClientRegistration::ClientMetadataDocument {
                url: "https://client.example/oauth-client.json".to_string(),
            }
        }
        OAuthFixtureMode::ClientCredentials | OAuthFixtureMode::ClientCredentialsBasic => {
            panic!("client credentials is not an authorization-code registration")
        }
    };
    OAuthOptions {
        resource: None,
        scopes: vec!["tools.read".to_string()],
        client_name: Some("A2C Computer".to_string()),
        mode: OAuthClientMode::AuthorizationCode { registration },
    }
}

async fn authorization_code_manager(
    mode: OAuthFixtureMode,
    challenge_tools_write_once: bool,
) -> (MCPServerManager, BundleId, Arc<OAuthMockState>) {
    let (port, state) = spawn_oauth_http_mock(mode, usize::from(challenge_tools_write_once)).await;
    let bundle_id = BundleId::try_from("oauth-code").unwrap();
    let manager = MCPServerManager::new();
    manager
        .set_secret_resolver(Some(Arc::new(OAuthTestSecretResolver)))
        .await;
    let mut config = HttpServerConfig::new(
        "oauth-code",
        HttpServerParameters {
            url: format!("http://127.0.0.1:{port}/mcp"),
            headers: HashMap::new(),
        },
    );
    config.bundle_id = Some(bundle_id.clone());
    config.oauth = Some(authorization_code_options(mode));
    manager
        .add_or_update_server(MCPServerConfig::Http(config))
        .await
        .unwrap();
    (manager, bundle_id, state)
}

async fn configure_authorization_code_manager(
    base_url: &str,
    bundle_id: BundleId,
    store: Arc<dyn OAuthCredentialStore>,
) -> MCPServerManager {
    let manager = MCPServerManager::with_oauth_credential_store(store);
    let mut config = HttpServerConfig::new(
        bundle_id.as_str(),
        HttpServerParameters {
            url: format!("{base_url}/mcp"),
            headers: HashMap::new(),
        },
    );
    config.bundle_id = Some(bundle_id);
    config.oauth = Some(authorization_code_options(
        OAuthFixtureMode::PreregisteredPublic,
    ));
    manager
        .add_or_update_server(MCPServerConfig::Http(config))
        .await
        .unwrap();
    manager
}

fn authorization_code_server_config(base_url: &str, bundle_id: BundleId) -> MCPServerConfig {
    let mut config = HttpServerConfig::new(
        bundle_id.as_str(),
        HttpServerParameters {
            url: format!("{base_url}/mcp"),
            headers: HashMap::new(),
        },
    );
    config.bundle_id = Some(bundle_id);
    config.oauth = Some(authorization_code_options(
        OAuthFixtureMode::PreregisteredPublic,
    ));
    MCPServerConfig::Http(config)
}

async fn configure_authorization_code_computer(
    base_url: &str,
    bundle_ids: &[BundleId],
    store: Arc<dyn OAuthCredentialStore>,
) -> (Computer<SilentSession>, TempDir) {
    let temp_dir = TempDir::new().unwrap();
    let servers = bundle_ids
        .iter()
        .cloned()
        .map(|bundle_id| {
            (
                bundle_id.to_string(),
                authorization_code_server_config(base_url, bundle_id),
            )
        })
        .collect();
    let computer = Computer::new(
        "oauth-store-computer",
        SilentSession::new("oauth-store-session"),
        None,
        Some(servers),
        false,
        false,
    )
    .with_oauth_credential_store(store)
    .with_confirm_callback(|_, _, _, _| true)
    .with_skill_home(temp_dir.path().join("skills"))
    .with_blob_cache_root(temp_dir.path().join("blob"))
    .with_config_dir(temp_dir.path().join("config"));
    computer.boot_up().await.unwrap();
    (computer, temp_dir)
}

struct CallbackRoute {
    tenant: String,
    cli_session: String,
    computer_id: String,
    bundle_id: BundleId,
    user_id: String,
    expires_at: Instant,
}

struct GatewayCallback {
    code: String,
    state: String,
    issuer: Option<String>,
    untrusted_business_ids: HashMap<String, String>,
}

struct RoutedCallback {
    tenant: String,
    computer_id: String,
    bundle_id: BundleId,
    callback: OAuthCallback,
}

#[derive(Default)]
struct CloudFlowHarness {
    routes: HashMap<String, CallbackRoute>,
    user_inboxes: HashMap<String, Vec<String>>,
    cli_inboxes: HashMap<String, Vec<RoutedCallback>>,
}

impl CloudFlowHarness {
    fn register_route(&mut self, state: String, route: CallbackRoute) {
        assert!(
            self.routes.insert(state, route).is_none(),
            "OAuth state routes must be unique"
        );
        tracing::info!("registered one-time OAuth callback route");
    }

    fn send_authorization_link(&mut self, user_id: &str, authorization_url: String) {
        self.user_inboxes
            .entry(user_id.to_string())
            .or_default()
            .push(authorization_url);
        tracing::info!("delivered OAuth authorization link to target user");
    }

    fn route_callback(
        &mut self,
        callback: GatewayCallback,
        now: Instant,
    ) -> Result<(), &'static str> {
        let GatewayCallback {
            code,
            state,
            issuer,
            untrusted_business_ids,
        } = callback;
        drop(untrusted_business_ids);
        let route = self
            .routes
            .remove(&state)
            .ok_or("unknown-or-replayed-state")?;
        if now >= route.expires_at {
            tracing::warn!("rejected expired OAuth callback route");
            return Err("expired-state");
        }
        self.cli_inboxes
            .entry(route.cli_session)
            .or_default()
            .push(RoutedCallback {
                tenant: route.tenant,
                computer_id: route.computer_id,
                bundle_id: route.bundle_id,
                callback: OAuthCallback {
                    code,
                    state,
                    issuer,
                },
            });
        tracing::info!("routed OAuth callback to original CLI session");
        Ok(())
    }

    fn take_cli_callback(&mut self, cli_session: &str) -> Option<RoutedCallback> {
        self.cli_inboxes.get_mut(cli_session)?.pop()
    }
}

async fn authorize_manager(
    manager: &MCPServerManager,
    bundle_id: &BundleId,
    state: &OAuthMockState,
    required_scope: Option<String>,
) -> String {
    let is_scope_upgrade = required_scope.is_some();
    let launch = manager
        .begin_oauth(
            bundle_id,
            OAuthBeginRequest {
                redirect_uri: "http://127.0.0.1:9876/callback".to_string(),
                required_scope,
            },
        )
        .await
        .unwrap();
    let authorization_url = url::Url::parse(&launch.authorization_url).unwrap();
    let query: HashMap<String, String> = authorization_url.query_pairs().into_owned().collect();
    assert_eq!(
        query.get("code_challenge_method").map(String::as_str),
        Some("S256")
    );
    assert!(query
        .get("code_challenge")
        .is_some_and(|value| !value.is_empty()));
    assert_eq!(query.get("resource"), Some(&state.resource()));
    if is_scope_upgrade {
        let requested_scopes = query.get("scope").cloned().unwrap_or_default();
        assert!(requested_scopes
            .split_whitespace()
            .any(|scope| scope == "tools.read"));
        assert!(requested_scopes
            .split_whitespace()
            .any(|scope| scope == "tools.write"));
    }
    manager
        .complete_oauth(
            bundle_id,
            OAuthCallback {
                code: "authorization-code".to_string(),
                state: launch.state,
                issuer: Some(state.base_url.clone()),
            },
        )
        .await
        .unwrap();
    query.get("client_id").cloned().unwrap()
}

async fn authorize_computer(
    computer: &Computer<SilentSession>,
    bundle_id: &BundleId,
    state: &OAuthMockState,
) {
    let launch = computer
        .begin_oauth(
            bundle_id,
            OAuthBeginRequest {
                redirect_uri: "http://127.0.0.1:9876/callback".to_string(),
                required_scope: None,
            },
        )
        .await
        .unwrap();
    computer
        .complete_oauth(
            bundle_id,
            OAuthCallback {
                code: "authorization-code".to_string(),
                state: launch.state,
                issuer: Some(state.base_url.clone()),
            },
        )
        .await
        .unwrap();
}

async fn recv_oauth_status_event(
    events: &mut tokio::sync::broadcast::Receiver<ComputerEvent>,
    expected_bundle_id: &BundleId,
) -> OAuthStatus {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let ComputerEvent::OAuthStatusChanged { bundle_id, status } =
                events.recv().await.unwrap()
            {
                if &bundle_id == expected_bundle_id {
                    return status;
                }
            }
        }
    })
    .await
    .expect("timed out waiting for OAuth status event")
}

#[tokio::test]
async fn test_cloud_flow_driver_routes_callback_privately_to_original_cli() {
    const CALLBACK_URI: &str = "https://callback.example.test/oauth/callback";
    const TARGET_TENANT: &str = "tenant-a";
    const TARGET_USER: &str = "user-a";
    const TARGET_CLI: &str = "cli-a";
    const TARGET_COMPUTER: &str = "computer-a";
    const OTHER_USER: &str = "user-b";
    const OTHER_CLI: &str = "cli-b";
    const AUTHORIZATION_CODE: &str = "authorization-code";

    let captured_logs = CapturedLogs::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(captured_logs.clone())
        .finish();
    let _subscriber_guard = tracing::subscriber::set_default(subscriber);
    let (manager, bundle_id, mock_state) =
        authorization_code_manager(OAuthFixtureMode::PreregisteredPublic, false).await;
    let launch = manager
        .begin_oauth(
            &bundle_id,
            OAuthBeginRequest {
                redirect_uri: CALLBACK_URI.to_string(),
                required_scope: None,
            },
        )
        .await
        .unwrap();
    let authorization_url = url::Url::parse(&launch.authorization_url).unwrap();
    let query: HashMap<String, String> = authorization_url.query_pairs().into_owned().collect();
    assert_eq!(
        query.get("redirect_uri").map(String::as_str),
        Some(CALLBACK_URI),
        "headless hosts must use their stable HTTPS callback URI"
    );
    assert!(
        query
            .get("state")
            .is_some_and(|state| state == &launch.state),
        "authorization URL must carry the generated OAuth state"
    );

    let now = Instant::now();
    let mut host = CloudFlowHarness::default();
    host.register_route(
        launch.state.clone(),
        CallbackRoute {
            tenant: TARGET_TENANT.to_string(),
            cli_session: TARGET_CLI.to_string(),
            computer_id: TARGET_COMPUTER.to_string(),
            bundle_id: bundle_id.clone(),
            user_id: TARGET_USER.to_string(),
            expires_at: now + Duration::from_secs(300),
        },
    );
    let target_user = host
        .routes
        .get(&launch.state)
        .expect("registered route must exist")
        .user_id
        .clone();
    host.send_authorization_link(&target_user, launch.authorization_url);
    assert_eq!(host.user_inboxes[TARGET_USER].len(), 1);
    assert!(!host.user_inboxes.contains_key(OTHER_USER));

    host.route_callback(
        GatewayCallback {
            code: AUTHORIZATION_CODE.to_string(),
            state: launch.state.clone(),
            issuer: Some(mock_state.base_url.clone()),
            untrusted_business_ids: HashMap::from([
                ("tenant".to_string(), "attacker-tenant".to_string()),
                ("cli_session".to_string(), OTHER_CLI.to_string()),
                ("computer_id".to_string(), "attacker-computer".to_string()),
                ("bundle_id".to_string(), "attacker-bundle".to_string()),
            ]),
        },
        now,
    )
    .unwrap();

    assert!(
        host.user_inboxes
            .values()
            .flatten()
            .all(|message| !message.contains(AUTHORIZATION_CODE)),
        "authorization codes must never be broadcast to user UI channels"
    );
    assert!(!host.cli_inboxes.contains_key(OTHER_CLI));
    assert!(matches!(
        host.route_callback(
            GatewayCallback {
                code: AUTHORIZATION_CODE.to_string(),
                state: launch.state.clone(),
                issuer: Some(mock_state.base_url.clone()),
                untrusted_business_ids: HashMap::new(),
            },
            now,
        ),
        Err("unknown-or-replayed-state")
    ));

    let delivery = host
        .take_cli_callback(TARGET_CLI)
        .expect("only the original CLI must receive the callback");
    assert_eq!(delivery.tenant, TARGET_TENANT);
    assert_eq!(delivery.computer_id, TARGET_COMPUTER);
    assert_eq!(delivery.bundle_id, bundle_id);
    let outcome = manager
        .complete_oauth(&delivery.bundle_id, delivery.callback)
        .await
        .unwrap();
    let OAuthFlowOutcome::Authorized { scopes } = outcome else {
        panic!("successful cloud callback must authorize the flow");
    };
    assert!(scopes.iter().any(|scope| scope == "tools.read"));
    assert!(matches!(
        manager.oauth_status(&bundle_id).await.unwrap(),
        OAuthStatus::Authorized { .. }
    ));

    manager.start_client_by_id(&bundle_id).await.unwrap();
    let tools = manager.list_available_tools().await;
    assert!(
        tools
            .iter()
            .any(|tool| tool.name.as_ref().ends_with("__echo")),
        "the authorized path must complete initialize and tools/list"
    );
    let result = manager
        .call_tool(
            bundle_id.as_str(),
            "echo",
            serde_json::json!({"message": "cloud flow authorized"}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        result.content[0].as_text().unwrap().text,
        "cloud flow authorized"
    );
    assert!(
        mock_state.authorized_mcp_requests.load(Ordering::SeqCst) >= 3,
        "initialize, tools/list, and tools/call must cross the real HTTP boundary"
    );

    let expired_launch = manager
        .begin_oauth(
            &bundle_id,
            OAuthBeginRequest {
                redirect_uri: CALLBACK_URI.to_string(),
                required_scope: Some("tools.write".to_string()),
            },
        )
        .await
        .unwrap();
    host.register_route(
        expired_launch.state.clone(),
        CallbackRoute {
            tenant: TARGET_TENANT.to_string(),
            cli_session: TARGET_CLI.to_string(),
            computer_id: TARGET_COMPUTER.to_string(),
            bundle_id: bundle_id.clone(),
            user_id: TARGET_USER.to_string(),
            expires_at: now,
        },
    );
    assert!(matches!(
        host.route_callback(
            GatewayCallback {
                code: "expired-code".to_string(),
                state: expired_launch.state.clone(),
                issuer: Some(mock_state.base_url.clone()),
                untrusted_business_ids: HashMap::new(),
            },
            now,
        ),
        Err("expired-state")
    ));
    assert!(host.take_cli_callback(TARGET_CLI).is_none());
    let timeout_outcome = manager
        .cancel_oauth(
            &bundle_id,
            OAuthCancellation {
                state: expired_launch.state.clone(),
                issuer: Some(mock_state.base_url.clone()),
                reason: OAuthCancellationReason::Timeout,
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        timeout_outcome,
        OAuthFlowOutcome::Terminated {
            reason: OAuthCancellationReason::Timeout,
            status: OAuthStatus::Authorized { .. },
        }
    ));
    assert!(matches!(
        manager
            .complete_oauth(
                &bundle_id,
                OAuthCallback {
                    code: "expired-code".to_string(),
                    state: expired_launch.state,
                    issuer: Some(mock_state.base_url.clone()),
                },
            )
            .await,
        Err(OAuthError::StateMismatch)
    ));
    manager.close().await.unwrap();

    let logs = captured_logs.text();
    for sensitive in [
        AUTHORIZATION_CODE,
        "expired-code",
        launch.state.as_str(),
        "oauth-e2e-token",
        "attacker-tenant",
        "attacker-computer",
        "attacker-bundle",
    ] {
        assert!(
            !logs.contains(sensitive),
            "cloud flow secrets and routing identifiers must not reach tracing output"
        );
    }
}

async fn assert_tool_call_is_blocked_before_http(
    manager: &MCPServerManager,
    bundle_id: &BundleId,
    state: &OAuthMockState,
) {
    let requests_before = state.total_requests.load(Ordering::SeqCst);
    let result = manager
        .call_tool(
            bundle_id.as_str(),
            "echo",
            serde_json::json!({"message": "must not reach resource"}),
            None,
        )
        .await;
    assert!(
        result.is_err() || result.is_ok_and(|value| value.is_error == Some(true)),
        "a cleared authorization must reject the call"
    );
    assert_eq!(
        state.total_requests.load(Ordering::SeqCst),
        requests_before,
        "a cleared token must be rejected before any HTTP request"
    );
}

#[tokio::test]
async fn test_streamable_http_authorization_code_registration_matrix_end_to_end() {
    for (mode, expected_client_id, expected_registrations) in [
        (
            OAuthFixtureMode::PreregisteredPublic,
            "oauth-code-client",
            0,
        ),
        (
            OAuthFixtureMode::PreregisteredPublicOidc,
            "oauth-code-client",
            0,
        ),
        (
            OAuthFixtureMode::PreregisteredConfidential,
            "oauth-code-client",
            0,
        ),
        (OAuthFixtureMode::Dynamic, "oauth-dcr-client", 1),
        (
            OAuthFixtureMode::ClientMetadataDocument,
            "https://client.example/oauth-client.json",
            0,
        ),
    ] {
        let (manager, bundle_id, state) = authorization_code_manager(mode, false).await;
        let client_id = authorize_manager(&manager, &bundle_id, &state, None).await;
        assert_eq!(client_id, expected_client_id, "mode={mode:?}");
        manager.start_client_by_id(&bundle_id).await.unwrap();
        let result = manager
            .call_tool(
                bundle_id.as_str(),
                "echo",
                serde_json::json!({"message": "authorization code works"}),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            result.content[0].as_text().unwrap().text,
            "authorization code works"
        );
        assert_eq!(state.token_requests.load(Ordering::SeqCst), 1);
        assert_eq!(
            state.registration_requests.load(Ordering::SeqCst),
            expected_registrations
        );
        manager.close().await.unwrap();
    }
}

#[tokio::test]
async fn test_computer_oauth_event_lag_resynchronizes_through_public_status_query() {
    let (_port, state) = spawn_oauth_http_mock(OAuthFixtureMode::PreregisteredPublic, 0).await;
    let bundle_id = BundleId::try_from("oauth-event-lag-resync").unwrap();
    let (computer, _temp_dir) = configure_authorization_code_computer(
        &state.base_url,
        std::slice::from_ref(&bundle_id),
        Arc::new(InMemoryOAuthCredentialStore::default()),
    )
    .await;
    let mut events = computer.subscribe_events();
    let begin_request = OAuthBeginRequest {
        redirect_uri: "http://127.0.0.1:9876/callback".to_string(),
        required_scope: None,
    };

    // Coordinator construction emits Unauthorized; every cycle then emits Pending followed by
    // Unauthorized. Thirty-three cycles produce 67 events, exceeding the fixed capacity of 64.
    for _ in 0..33 {
        let launch = computer
            .begin_oauth(&bundle_id, begin_request.clone())
            .await
            .unwrap();
        computer
            .cancel_oauth(
                &bundle_id,
                OAuthCancellation {
                    state: launch.state,
                    issuer: None,
                    reason: OAuthCancellationReason::Cancelled,
                },
            )
            .await
            .unwrap();
    }
    assert!(matches!(
        events.recv().await,
        Err(tokio::sync::broadcast::error::RecvError::Lagged(_))
    ));
    assert_eq!(
        computer.oauth_status(&bundle_id).await.unwrap(),
        OAuthStatus::Unauthorized
    );
    computer.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_computer_oauth_events_cover_facades_dedup_403_and_401() {
    let (_port, state) = spawn_oauth_http_mock(OAuthFixtureMode::PreregisteredPublic, 1).await;
    let bundle_id = BundleId::try_from("oauth-events-e2e").unwrap();
    let (computer, _temp_dir) = configure_authorization_code_computer(
        &state.base_url,
        std::slice::from_ref(&bundle_id),
        Arc::new(InMemoryOAuthCredentialStore::default()),
    )
    .await;
    let mut events = computer.subscribe_events();
    let begin_request = OAuthBeginRequest {
        redirect_uri: "http://127.0.0.1:9876/callback".to_string(),
        required_scope: None,
    };

    let launch = computer
        .begin_oauth(&bundle_id, begin_request.clone())
        .await
        .unwrap();
    assert_eq!(
        recv_oauth_status_event(&mut events, &bundle_id).await,
        OAuthStatus::Unauthorized
    );
    assert_eq!(
        recv_oauth_status_event(&mut events, &bundle_id).await,
        OAuthStatus::AuthorizationPending
    );
    let duplicate = computer
        .begin_oauth(&bundle_id, begin_request.clone())
        .await
        .unwrap();
    assert_eq!(duplicate, launch);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), events.recv())
            .await
            .is_err()
    );

    computer
        .complete_oauth(
            &bundle_id,
            OAuthCallback {
                code: "authorization-code".to_string(),
                state: launch.state,
                issuer: Some(state.base_url.clone()),
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        recv_oauth_status_event(&mut events, &bundle_id).await,
        OAuthStatus::Authorized { .. }
    ));

    computer.clear_oauth(&bundle_id).await.unwrap();
    assert_eq!(
        recv_oauth_status_event(&mut events, &bundle_id).await,
        OAuthStatus::Unauthorized
    );
    let cancelled = computer
        .begin_oauth(&bundle_id, begin_request.clone())
        .await
        .unwrap();
    assert_eq!(
        recv_oauth_status_event(&mut events, &bundle_id).await,
        OAuthStatus::AuthorizationPending
    );
    computer
        .cancel_oauth(
            &bundle_id,
            OAuthCancellation {
                state: cancelled.state,
                issuer: None,
                reason: OAuthCancellationReason::Cancelled,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        recv_oauth_status_event(&mut events, &bundle_id).await,
        OAuthStatus::Unauthorized
    );

    authorize_computer(&computer, &bundle_id, &state).await;
    assert_eq!(
        recv_oauth_status_event(&mut events, &bundle_id).await,
        OAuthStatus::AuthorizationPending
    );
    assert!(matches!(
        recv_oauth_status_event(&mut events, &bundle_id).await,
        OAuthStatus::Authorized { .. }
    ));
    computer.start_mcp_client(&bundle_id).await.unwrap();
    let exposed_tool = format!("{}__echo", bundle_id.as_str());
    computer
        .execute_tool(
            "oauth-event-403",
            &exposed_tool,
            serde_json::json!({"message": "scope challenge"}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        recv_oauth_status_event(&mut events, &bundle_id).await,
        OAuthStatus::ReauthorizationRequired {
            required_scope: "tools.write".to_string(),
        }
    );

    let upgrade = computer
        .begin_oauth(
            &bundle_id,
            OAuthBeginRequest {
                required_scope: Some("tools.write".to_string()),
                ..begin_request
            },
        )
        .await
        .unwrap();
    assert_eq!(
        recv_oauth_status_event(&mut events, &bundle_id).await,
        OAuthStatus::AuthorizationPending
    );
    computer
        .complete_oauth(
            &bundle_id,
            OAuthCallback {
                code: "authorization-code".to_string(),
                state: upgrade.state,
                issuer: Some(state.base_url.clone()),
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        recv_oauth_status_event(&mut events, &bundle_id).await,
        OAuthStatus::Authorized { .. }
    ));

    state.reject_authorized_remaining.store(1, Ordering::SeqCst);
    let _ = computer
        .execute_tool(
            "oauth-event-401",
            &exposed_tool,
            serde_json::json!({"message": "reject token"}),
            None,
        )
        .await;
    assert_eq!(
        recv_oauth_status_event(&mut events, &bundle_id).await,
        OAuthStatus::Unauthorized
    );
    computer.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_computer_oauth_event_reports_background_refresh_failure() {
    let (port, state) =
        spawn_oauth_http_mock_with_expiry(OAuthFixtureMode::ClientCredentials, 0, 31).await;
    let bundle_id = BundleId::try_from("oauth-refresh-event").unwrap();
    let mut config = HttpServerConfig::new(
        "oauth-refresh-event",
        HttpServerParameters {
            url: format!("http://127.0.0.1:{port}/mcp"),
            headers: HashMap::new(),
        },
    );
    config.bundle_id = Some(bundle_id.clone());
    config.oauth = Some(OAuthOptions {
        resource: None,
        scopes: vec!["tools.read".to_string()],
        client_name: None,
        mode: OAuthClientMode::ClientCredentialsSecret {
            client_id: "oauth-e2e-client".to_string(),
            client_secret_input: "oauth-e2e-secret-input".to_string(),
        },
    });
    let temp_dir = TempDir::new().unwrap();
    let computer = Computer::new(
        "oauth-refresh-event",
        SilentSession::new("oauth-refresh-event-session"),
        None,
        Some(HashMap::from([(
            bundle_id.to_string(),
            MCPServerConfig::Http(config),
        )])),
        false,
        false,
    )
    .with_secret_resolver(Arc::new(OAuthTestSecretResolver))
    .with_confirm_callback(|_, _, _, _| true)
    .with_skill_home(temp_dir.path().join("skills"))
    .with_blob_cache_root(temp_dir.path().join("blob"))
    .with_config_dir(temp_dir.path().join("config"));
    computer.boot_up().await.unwrap();
    computer.start_mcp_client(&bundle_id).await.unwrap();
    assert_eq!(state.token_requests.load(Ordering::SeqCst), 1);
    let mut events = computer.subscribe_events();
    state.reject_token_remaining.store(1, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_secs(2)).await;

    let exposed_tool = format!("{}__echo", bundle_id.as_str());
    let _ = computer
        .execute_tool(
            "oauth-refresh-failure",
            &exposed_tool,
            serde_json::json!({"message": "refresh"}),
            None,
        )
        .await;
    assert_eq!(
        recv_oauth_status_event(&mut events, &bundle_id).await,
        OAuthStatus::Error {
            message: "OAuth access token refresh failed".to_string(),
        }
    );
    computer.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_explicit_canonical_resource_reaches_authorization_token_and_credential_key() {
    let (port, state) = spawn_oauth_http_mock(OAuthFixtureMode::PreregisteredPublic, 0).await;
    let endpoint = format!("http://127.0.0.1:{port}/mcp");
    let resource = format!("{endpoint}?audience=canonical");
    state.set_resource(resource.clone());
    let bundle_id = BundleId::try_from("oauth-resource-override").unwrap();
    let recording_store = Arc::new(RecordingOAuthCredentialStore::default());
    let store: Arc<dyn OAuthCredentialStore> = recording_store.clone();
    let manager = MCPServerManager::with_oauth_credential_store(store);
    let mut config = HttpServerConfig::new(
        "oauth-resource-override",
        HttpServerParameters {
            url: endpoint.clone(),
            headers: HashMap::new(),
        },
    );
    config.bundle_id = Some(bundle_id.clone());
    let mut options = authorization_code_options(OAuthFixtureMode::PreregisteredPublic);
    options.resource = Some(resource.clone());
    config.oauth = Some(options);
    manager
        .add_or_update_server(MCPServerConfig::Http(config))
        .await
        .unwrap();

    authorize_manager(&manager, &bundle_id, &state, None).await;

    let forms = state.token_forms.lock().await;
    assert_eq!(forms.len(), 1);
    assert_eq!(forms[0].get("resource"), Some(&resource));
    drop(forms);
    let operations = recording_store.operations().await;
    assert!(operations.iter().any(|operation| match operation {
        CredentialStoreOperation::Load(key)
        | CredentialStoreOperation::Save(key)
        | CredentialStoreOperation::Delete(key) => key.resource == resource,
    }));
    assert_ne!(endpoint, resource);
    manager.close().await.unwrap();
}

#[tokio::test]
async fn test_resource_override_preserves_static_authorization_conflict() {
    let (port, state) = spawn_oauth_http_mock(OAuthFixtureMode::PreregisteredPublic, 0).await;
    let bundle_id = BundleId::try_from("oauth-resource-static-conflict").unwrap();
    let manager = MCPServerManager::new();
    let mut config = HttpServerConfig::new(
        "oauth-resource-static-conflict",
        HttpServerParameters {
            url: format!("http://127.0.0.1:{port}/mcp"),
            headers: HashMap::from([(
                "Authorization".to_string(),
                "Bearer static-token".to_string(),
            )]),
        },
    );
    config.bundle_id = Some(bundle_id.clone());
    let mut options = authorization_code_options(OAuthFixtureMode::PreregisteredPublic);
    options.resource = Some(format!("{}/mcp?audience=canonical", state.base_url));
    config.oauth = Some(options);
    manager
        .add_or_update_server(MCPServerConfig::Http(config))
        .await
        .unwrap();

    assert!(matches!(
        manager
            .begin_oauth(
                &bundle_id,
                OAuthBeginRequest {
                    redirect_uri: "http://127.0.0.1:9876/callback".to_string(),
                    required_scope: None,
                },
            )
            .await,
        Err(OAuthError::ConflictingAuthorizationHeader)
    ));
    assert_eq!(state.total_requests.load(Ordering::SeqCst), 0);
    manager.close().await.unwrap();
}

#[tokio::test]
async fn test_authorization_code_oauth_covers_sse_responses_403_and_401() {
    let (port, state) = spawn_oauth_http_sse_mock(OAuthFixtureMode::PreregisteredPublic, 1).await;
    let bundle_id = BundleId::try_from("oauth-code-sse").unwrap();
    let manager = MCPServerManager::new();
    let mut config = HttpServerConfig::new(
        "oauth-code-sse",
        HttpServerParameters {
            url: format!("http://127.0.0.1:{port}/mcp"),
            headers: HashMap::new(),
        },
    );
    config.bundle_id = Some(bundle_id.clone());
    config.oauth = Some(authorization_code_options(
        OAuthFixtureMode::PreregisteredPublic,
    ));
    manager
        .add_or_update_server(MCPServerConfig::Http(config))
        .await
        .unwrap();

    authorize_manager(&manager, &bundle_id, &state, None).await;
    manager.start_client_by_id(&bundle_id).await.unwrap();
    assert!(
        manager
            .list_available_tools()
            .await
            .iter()
            .any(|tool| tool.name.as_ref().ends_with("__echo")),
        "OAuth initialize and tools/list must decode SSE-framed POST responses"
    );

    let challenged = manager
        .call_tool(
            bundle_id.as_str(),
            "echo",
            serde_json::json!({"message": "scope challenge"}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(challenged.is_error, Some(true));
    assert_eq!(
        manager.oauth_status(&bundle_id).await.unwrap(),
        OAuthStatus::ReauthorizationRequired {
            required_scope: "tools.write".to_string()
        }
    );

    authorize_manager(
        &manager,
        &bundle_id,
        &state,
        Some("tools.write".to_string()),
    )
    .await;
    let result = manager
        .call_tool(
            bundle_id.as_str(),
            "echo",
            serde_json::json!({"message": "SSE OAuth works"}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(result.content[0].as_text().unwrap().text, "SSE OAuth works");

    state.reject_authorized_remaining.store(1, Ordering::SeqCst);
    let _ = manager
        .call_tool(
            bundle_id.as_str(),
            "echo",
            serde_json::json!({"message": "reject token"}),
            None,
        )
        .await;
    assert_tool_call_is_blocked_before_http(&manager, &bundle_id, &state).await;
    manager.close().await.unwrap();
}

#[tokio::test]
async fn test_authorization_code_clear_is_bundle_scoped_and_401_never_reuses_old_token() {
    let (manager, bundle_id, state) =
        authorization_code_manager(OAuthFixtureMode::PreregisteredPublic, false).await;
    authorize_manager(&manager, &bundle_id, &state, None).await;
    manager.start_client_by_id(&bundle_id).await.unwrap();
    manager
        .call_tool(
            bundle_id.as_str(),
            "echo",
            serde_json::json!({"message": "authorized"}),
            None,
        )
        .await
        .unwrap();

    manager.clear_oauth(&bundle_id).await.unwrap();
    assert_tool_call_is_blocked_before_http(&manager, &bundle_id, &state).await;

    authorize_manager(&manager, &bundle_id, &state, None).await;
    let peer_manager = MCPServerManager::new();
    let peer_bundle_id = BundleId::try_from("oauth-code-peer").unwrap();
    let mut peer_config = HttpServerConfig::new(
        "oauth-code-peer",
        HttpServerParameters {
            url: format!("{}/mcp", state.base_url),
            headers: HashMap::new(),
        },
    );
    peer_config.bundle_id = Some(peer_bundle_id.clone());
    peer_config.oauth = Some(authorization_code_options(
        OAuthFixtureMode::PreregisteredPublic,
    ));
    peer_manager
        .add_or_update_server(MCPServerConfig::Http(peer_config))
        .await
        .unwrap();
    peer_manager.clear_oauth(&peer_bundle_id).await.unwrap();
    assert!(matches!(
        manager.oauth_status(&bundle_id).await.unwrap(),
        OAuthStatus::Authorized { .. }
    ));
    manager
        .call_tool(
            bundle_id.as_str(),
            "echo",
            serde_json::json!({"message": "peer clear is isolated"}),
            None,
        )
        .await
        .expect("clearing a peer bundle must not revoke this bundle");
    peer_manager.close().await.unwrap();

    state.reject_authorized_remaining.store(1, Ordering::SeqCst);
    let _ = manager
        .call_tool(
            bundle_id.as_str(),
            "echo",
            serde_json::json!({"message": "server rejects token"}),
            None,
        )
        .await;
    assert_tool_call_is_blocked_before_http(&manager, &bundle_id, &state).await;
    manager.close().await.unwrap();
}

#[tokio::test]
async fn test_authorization_code_cancellation_validates_callback_and_cleans_pending_state() {
    let (manager, bundle_id, state) =
        authorization_code_manager(OAuthFixtureMode::PreregisteredPublic, false).await;
    let launch = manager
        .begin_oauth(
            &bundle_id,
            OAuthBeginRequest {
                redirect_uri: "http://127.0.0.1:9876/callback".to_string(),
                required_scope: None,
            },
        )
        .await
        .unwrap();

    assert!(matches!(
        manager
            .cancel_oauth(
                &bundle_id,
                OAuthCancellation {
                    state: "wrong-state".to_string(),
                    issuer: Some(state.base_url.clone()),
                    reason: OAuthCancellationReason::AccessDenied,
                },
            )
            .await,
        Err(OAuthError::StateMismatch)
    ));
    assert_eq!(
        manager.oauth_status(&bundle_id).await.unwrap(),
        OAuthStatus::AuthorizationPending
    );

    assert!(matches!(
        manager
            .complete_oauth(
                &bundle_id,
                OAuthCallback {
                    code: "authorization-code".to_string(),
                    state: launch.state.clone(),
                    issuer: Some("not-an-issuer".to_string()),
                },
            )
            .await,
        Err(OAuthError::IssuerMismatch)
    ));
    assert_eq!(
        manager.oauth_status(&bundle_id).await.unwrap(),
        OAuthStatus::AuthorizationPending
    );

    assert!(matches!(
        manager
            .cancel_oauth(
                &bundle_id,
                OAuthCancellation {
                    state: launch.state.clone(),
                    issuer: None,
                    reason: OAuthCancellationReason::AccessDenied,
                },
            )
            .await,
        Err(OAuthError::IssuerMismatch)
    ));
    assert_eq!(
        manager.oauth_status(&bundle_id).await.unwrap(),
        OAuthStatus::AuthorizationPending
    );

    assert!(matches!(
        manager
            .complete_oauth(
                &bundle_id,
                OAuthCallback {
                    code: "authorization-code".to_string(),
                    state: launch.state.clone(),
                    issuer: Some(format!("{}/", state.base_url)),
                },
            )
            .await,
        Err(OAuthError::IssuerMismatch)
    ));
    assert_eq!(
        manager.oauth_status(&bundle_id).await.unwrap(),
        OAuthStatus::AuthorizationPending
    );

    let outcome = manager
        .cancel_oauth(
            &bundle_id,
            OAuthCancellation {
                state: launch.state,
                issuer: Some(state.base_url.clone()),
                reason: OAuthCancellationReason::AccessDenied,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        outcome,
        OAuthFlowOutcome::Terminated {
            reason: OAuthCancellationReason::AccessDenied,
            status: OAuthStatus::Unauthorized,
        }
    );

    let replacement = manager
        .begin_oauth(
            &bundle_id,
            OAuthBeginRequest {
                redirect_uri: "http://127.0.0.1:9876/callback".to_string(),
                required_scope: None,
            },
        )
        .await
        .expect("cancellation must remove the old pending flow");
    assert!(matches!(
        manager
            .cancel_oauth(
                &bundle_id,
                OAuthCancellation {
                    state: replacement.state.clone(),
                    issuer: Some("https://wrong-issuer.example".to_string()),
                    reason: OAuthCancellationReason::Timeout,
                },
            )
            .await,
        Err(OAuthError::IssuerMismatch)
    ));
    assert_eq!(
        manager.oauth_status(&bundle_id).await.unwrap(),
        OAuthStatus::AuthorizationPending
    );
    let timeout_outcome = manager
        .cancel_oauth(
            &bundle_id,
            OAuthCancellation {
                state: replacement.state,
                issuer: None,
                reason: OAuthCancellationReason::Timeout,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        timeout_outcome,
        OAuthFlowOutcome::Terminated {
            reason: OAuthCancellationReason::Timeout,
            status: OAuthStatus::Unauthorized,
        }
    );
    manager.close().await.unwrap();
}

#[tokio::test]
async fn test_oauth_flow_cancels_before_delayed_discovery_returns() {
    let (manager, bundle_id, state) =
        authorization_code_manager(OAuthFixtureMode::PreregisteredPublic, false).await;
    state
        .discovery_response_delay_ms
        .store(5_000, Ordering::SeqCst);
    let flow = manager
        .create_oauth_flow(
            &bundle_id,
            OAuthBeginRequest {
                redirect_uri: "http://127.0.0.1:9876/callback".to_string(),
                required_scope: None,
            },
        )
        .await
        .unwrap();

    wait_for_count(
        &state.discovery_requests,
        1,
        "real discovery request must be in flight",
    )
    .await;
    let started = Instant::now();
    let outcome = tokio::time::timeout(
        Duration::from_secs(1),
        flow.cancel(OAuthCancellationReason::Cancelled),
    )
    .await
    .expect("cancellation must not wait for provider timeout")
    .unwrap();
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(
        outcome,
        OAuthFlowOutcome::Terminated {
            reason: OAuthCancellationReason::Cancelled,
            status: OAuthStatus::Unauthorized,
        }
    );
    assert!(matches!(
        flow.launch().await,
        Err(OAuthError::AuthorizationCancelled)
    ));
    manager.close().await.unwrap();
}

#[tokio::test]
async fn test_oauth_flow_cancels_delayed_dynamic_registration() {
    let (manager, bundle_id, state) =
        authorization_code_manager(OAuthFixtureMode::Dynamic, false).await;
    state
        .registration_response_delay_ms
        .store(5_000, Ordering::SeqCst);
    let flow = manager
        .create_oauth_flow(
            &bundle_id,
            OAuthBeginRequest {
                redirect_uri: "http://127.0.0.1:9876/callback".to_string(),
                required_scope: None,
            },
        )
        .await
        .unwrap();

    wait_for_count(
        &state.registration_requests,
        1,
        "real DCR request must be in flight",
    )
    .await;
    let outcome = tokio::time::timeout(
        Duration::from_secs(1),
        flow.cancel(OAuthCancellationReason::Timeout),
    )
    .await
    .expect("DCR cancellation must be bounded")
    .unwrap();
    assert_eq!(
        outcome,
        OAuthFlowOutcome::Terminated {
            reason: OAuthCancellationReason::Timeout,
            status: OAuthStatus::Unauthorized,
        }
    );
    manager.close().await.unwrap();
}

#[tokio::test]
async fn test_cancel_wins_delayed_exchange_and_preserves_authorized_scopes() {
    let (manager, bundle_id, state) =
        authorization_code_manager(OAuthFixtureMode::PreregisteredPublic, false).await;
    let initial = manager
        .begin_oauth(
            &bundle_id,
            OAuthBeginRequest {
                redirect_uri: "http://127.0.0.1:9876/callback".to_string(),
                required_scope: None,
            },
        )
        .await
        .unwrap();
    manager
        .complete_oauth(
            &bundle_id,
            OAuthCallback {
                code: "authorization-code".to_string(),
                state: initial.state,
                issuer: Some(state.base_url.clone()),
            },
        )
        .await
        .unwrap();
    assert_eq!(state.token_requests.load(Ordering::SeqCst), 1);

    state.token_response_delay_ms.store(5_000, Ordering::SeqCst);
    let flow = manager
        .create_oauth_flow(
            &bundle_id,
            OAuthBeginRequest {
                redirect_uri: "http://127.0.0.1:9876/callback".to_string(),
                required_scope: Some("tools.write".to_string()),
            },
        )
        .await
        .unwrap();
    let launch = flow.launch().await.unwrap();
    let completing = {
        let flow = flow.clone();
        let issuer = state.base_url.clone();
        tokio::spawn(async move {
            flow.complete(OAuthCallback {
                code: "authorization-code".to_string(),
                state: launch.state,
                issuer: Some(issuer),
            })
            .await
        })
    };
    tokio::time::timeout(Duration::from_secs(1), async {
        while state.token_requests.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("replacement token exchange must start");

    let outcome = tokio::time::timeout(
        Duration::from_secs(1),
        flow.cancel(OAuthCancellationReason::Timeout),
    )
    .await
    .expect("exchange cancellation must not wait for provider timeout")
    .unwrap();
    assert_eq!(
        outcome,
        OAuthFlowOutcome::Terminated {
            reason: OAuthCancellationReason::Timeout,
            status: OAuthStatus::Authorized {
                scopes: vec!["tools.read".to_string()],
            },
        }
    );
    assert_eq!(completing.await.unwrap().unwrap(), outcome);
    assert_eq!(
        manager.oauth_status(&bundle_id).await.unwrap(),
        OAuthStatus::Authorized {
            scopes: vec!["tools.read".to_string()],
        }
    );
    manager.close().await.unwrap();
}

#[tokio::test]
async fn test_compat_cancel_oauth_preempts_delayed_complete_oauth() {
    let (manager, bundle_id, state) =
        authorization_code_manager(OAuthFixtureMode::PreregisteredPublic, false).await;
    state.token_response_delay_ms.store(5_000, Ordering::SeqCst);
    let launch = manager
        .begin_oauth(
            &bundle_id,
            OAuthBeginRequest {
                redirect_uri: "http://127.0.0.1:9876/callback".to_string(),
                required_scope: None,
            },
        )
        .await
        .unwrap();
    let manager = Arc::new(manager);
    let completing = {
        let manager = Arc::clone(&manager);
        let bundle_id = bundle_id.clone();
        let callback_state = launch.state.clone();
        let issuer = state.base_url.clone();
        tokio::spawn(async move {
            manager
                .complete_oauth(
                    &bundle_id,
                    OAuthCallback {
                        code: "authorization-code".to_string(),
                        state: callback_state,
                        issuer: Some(issuer),
                    },
                )
                .await
        })
    };
    state.token_started.notified().await;

    let cancelled = tokio::time::timeout(
        Duration::from_secs(1),
        manager.cancel_oauth(
            &bundle_id,
            OAuthCancellation {
                state: launch.state,
                issuer: None,
                reason: OAuthCancellationReason::Timeout,
            },
        ),
    )
    .await
    .expect("compat cancellation must not queue behind token exchange")
    .unwrap();
    assert_eq!(
        cancelled,
        OAuthFlowOutcome::Terminated {
            reason: OAuthCancellationReason::Timeout,
            status: OAuthStatus::Unauthorized,
        }
    );
    assert_eq!(completing.await.unwrap().unwrap(), cancelled);
    assert_eq!(
        manager.oauth_status(&bundle_id).await.unwrap(),
        OAuthStatus::Unauthorized
    );
    manager.close().await.unwrap();
}

#[tokio::test]
async fn test_failed_candidate_commit_preserves_previous_credential_and_scopes() {
    let (port, state) = spawn_oauth_http_mock(OAuthFixtureMode::PreregisteredPublic, 0).await;
    let bundle_id = BundleId::try_from("oauth-commit-rollback").unwrap();
    let store = Arc::new(RecordingOAuthCredentialStore::default());
    let manager = configure_authorization_code_manager(
        &format!("http://127.0.0.1:{port}"),
        bundle_id.clone(),
        store.clone(),
    )
    .await;

    let initial = manager
        .begin_oauth(
            &bundle_id,
            OAuthBeginRequest {
                redirect_uri: "http://127.0.0.1:9876/callback".to_string(),
                required_scope: None,
            },
        )
        .await
        .unwrap();
    manager
        .complete_oauth(
            &bundle_id,
            OAuthCallback {
                code: "authorization-code".to_string(),
                state: initial.state,
                issuer: Some(state.base_url.clone()),
            },
        )
        .await
        .unwrap();
    let credentials_before = store.credential_entries().await;
    store
        .fail_next_credential_save
        .store(true, Ordering::SeqCst);

    let upgrade = manager
        .begin_oauth(
            &bundle_id,
            OAuthBeginRequest {
                redirect_uri: "http://127.0.0.1:9876/callback".to_string(),
                required_scope: Some("tools.write".to_string()),
            },
        )
        .await
        .unwrap();
    assert!(manager
        .complete_oauth(
            &bundle_id,
            OAuthCallback {
                code: "authorization-code".to_string(),
                state: upgrade.state,
                issuer: Some(state.base_url.clone()),
            },
        )
        .await
        .is_err());

    assert_eq!(store.credential_entries().await, credentials_before);
    assert_eq!(
        manager.oauth_status(&bundle_id).await.unwrap(),
        OAuthStatus::Authorized {
            scopes: vec!["tools.read".to_string()],
        }
    );
    manager.close().await.unwrap();
}

#[tokio::test]
async fn test_failed_candidate_manager_preparation_never_mutates_durable_credential() {
    let (_port, state) =
        spawn_oauth_http_mock(OAuthFixtureMode::PreregisteredConfidential, 0).await;
    let bundle_id = BundleId::try_from("oauth-prepare-rollback").unwrap();
    let store = Arc::new(RecordingOAuthCredentialStore::default());
    let manager = MCPServerManager::with_oauth_credential_store(store.clone());
    manager
        .set_secret_resolver(Some(Arc::new(FailOnCallOAuthTestSecretResolver {
            calls: AtomicUsize::new(0),
            fail_on_call: 4,
        })))
        .await;
    let mut config = match authorization_code_server_config(&state.base_url, bundle_id.clone()) {
        MCPServerConfig::Http(config) => config,
        _ => unreachable!(),
    };
    config.oauth = Some(authorization_code_options(
        OAuthFixtureMode::PreregisteredConfidential,
    ));
    manager
        .add_or_update_server(MCPServerConfig::Http(config))
        .await
        .unwrap();

    let initial = manager
        .begin_oauth(
            &bundle_id,
            OAuthBeginRequest {
                redirect_uri: "http://127.0.0.1:9876/callback".to_string(),
                required_scope: None,
            },
        )
        .await
        .unwrap();
    manager
        .complete_oauth(
            &bundle_id,
            OAuthCallback {
                code: "authorization-code".to_string(),
                state: initial.state,
                issuer: Some(state.base_url.clone()),
            },
        )
        .await
        .unwrap();
    let credentials_before = store.credential_entries().await;

    let upgrade = manager
        .begin_oauth(
            &bundle_id,
            OAuthBeginRequest {
                redirect_uri: "http://127.0.0.1:9876/callback".to_string(),
                required_scope: Some("tools.write".to_string()),
            },
        )
        .await
        .unwrap();
    assert!(manager
        .complete_oauth(
            &bundle_id,
            OAuthCallback {
                code: "authorization-code".to_string(),
                state: upgrade.state,
                issuer: Some(state.base_url.clone()),
            },
        )
        .await
        .is_err());
    assert_eq!(store.credential_entries().await, credentials_before);
    assert_eq!(
        manager.oauth_status(&bundle_id).await.unwrap(),
        OAuthStatus::Authorized {
            scopes: vec!["tools.read".to_string()],
        }
    );
    manager.close().await.unwrap();
}

#[tokio::test]
async fn test_cancel_during_candidate_preparation_prevents_durable_commit() {
    let (_port, state) =
        spawn_oauth_http_mock(OAuthFixtureMode::PreregisteredConfidential, 0).await;
    let bundle_id = BundleId::try_from("oauth-prepare-cancel").unwrap();
    let store = Arc::new(RecordingOAuthCredentialStore::default());
    let resolver = Arc::new(BlockingOnCallOAuthTestSecretResolver {
        calls: AtomicUsize::new(0),
        block_on_call: 4,
        started: Notify::new(),
        release: Notify::new(),
    });
    let manager = MCPServerManager::with_oauth_credential_store(store.clone());
    manager.set_secret_resolver(Some(resolver.clone())).await;
    let mut config = match authorization_code_server_config(&state.base_url, bundle_id.clone()) {
        MCPServerConfig::Http(config) => config,
        _ => unreachable!(),
    };
    config.oauth = Some(authorization_code_options(
        OAuthFixtureMode::PreregisteredConfidential,
    ));
    manager
        .add_or_update_server(MCPServerConfig::Http(config))
        .await
        .unwrap();
    let initial = manager
        .begin_oauth(
            &bundle_id,
            OAuthBeginRequest {
                redirect_uri: "http://127.0.0.1:9876/callback".to_string(),
                required_scope: None,
            },
        )
        .await
        .unwrap();
    manager
        .complete_oauth(
            &bundle_id,
            OAuthCallback {
                code: "authorization-code".to_string(),
                state: initial.state,
                issuer: Some(state.base_url.clone()),
            },
        )
        .await
        .unwrap();
    let before = store.credential_entries().await;

    let flow = manager
        .create_oauth_flow(
            &bundle_id,
            OAuthBeginRequest {
                redirect_uri: "http://127.0.0.1:9876/callback".to_string(),
                required_scope: Some("tools.write".to_string()),
            },
        )
        .await
        .unwrap();
    let launch = flow.launch().await.unwrap();
    let completing = {
        let flow = flow.clone();
        let issuer = state.base_url.clone();
        tokio::spawn(async move {
            flow.complete(OAuthCallback {
                code: "authorization-code".to_string(),
                state: launch.state,
                issuer: Some(issuer),
            })
            .await
        })
    };
    resolver.started.notified().await;
    let outcome = flow.cancel(OAuthCancellationReason::Timeout).await.unwrap();
    resolver.release.notify_one();
    assert_eq!(completing.await.unwrap().unwrap(), outcome);
    assert_eq!(store.credential_entries().await, before);
    assert_eq!(
        outcome,
        OAuthFlowOutcome::Terminated {
            reason: OAuthCancellationReason::Timeout,
            status: OAuthStatus::Authorized {
                scopes: vec!["tools.read".to_string()],
            },
        }
    );
    manager.close().await.unwrap();
}

#[tokio::test]
async fn test_cold_start_prelaunch_cancel_restores_persisted_authorized_status() {
    let (port, state) = spawn_oauth_http_mock(OAuthFixtureMode::PreregisteredPublic, 0).await;
    let bundle_id = BundleId::try_from("oauth-cold-cancel").unwrap();
    let store: Arc<dyn OAuthCredentialStore> = Arc::new(RecordingOAuthCredentialStore::default());
    let first = configure_authorization_code_manager(
        &format!("http://127.0.0.1:{port}"),
        bundle_id.clone(),
        Arc::clone(&store),
    )
    .await;
    let initial = first
        .begin_oauth(
            &bundle_id,
            OAuthBeginRequest {
                redirect_uri: "http://127.0.0.1:9876/callback".to_string(),
                required_scope: None,
            },
        )
        .await
        .unwrap();
    first
        .complete_oauth(
            &bundle_id,
            OAuthCallback {
                code: "authorization-code".to_string(),
                state: initial.state,
                issuer: Some(state.base_url.clone()),
            },
        )
        .await
        .unwrap();
    first.close().await.unwrap();

    let second = configure_authorization_code_manager(
        &format!("http://127.0.0.1:{port}"),
        bundle_id.clone(),
        store,
    )
    .await;
    let discovery_before = state.discovery_requests.load(Ordering::SeqCst);
    state
        .discovery_response_delay_ms
        .store(5_000, Ordering::SeqCst);
    let flow = second
        .create_oauth_flow(
            &bundle_id,
            OAuthBeginRequest {
                redirect_uri: "http://127.0.0.1:9876/callback".to_string(),
                required_scope: Some("tools.write".to_string()),
            },
        )
        .await
        .unwrap();
    wait_for_count(
        &state.discovery_requests,
        discovery_before + 1,
        "cold-start discovery must be in flight",
    )
    .await;
    let outcome = flow
        .cancel(OAuthCancellationReason::Cancelled)
        .await
        .unwrap();
    assert_eq!(
        outcome,
        OAuthFlowOutcome::Terminated {
            reason: OAuthCancellationReason::Cancelled,
            status: OAuthStatus::Authorized {
                scopes: vec!["tools.read".to_string()],
            },
        }
    );
    state.discovery_response_delay_ms.store(0, Ordering::SeqCst);
    assert_eq!(
        second.oauth_status(&bundle_id).await.unwrap(),
        OAuthStatus::Authorized {
            scopes: vec!["tools.read".to_string()],
        }
    );
    second.close().await.unwrap();
}

#[tokio::test]
async fn test_concurrent_terminal_commands_converge_and_terminal_flow_is_not_reused() {
    let (manager, bundle_id, state) =
        authorization_code_manager(OAuthFixtureMode::PreregisteredPublic, false).await;
    let request = OAuthBeginRequest {
        redirect_uri: "http://127.0.0.1:9876/callback".to_string(),
        required_scope: None,
    };
    let flow = manager
        .create_oauth_flow(&bundle_id, request.clone())
        .await
        .unwrap();
    let launch = flow.launch().await.unwrap();
    let callback = OAuthCallback {
        code: "authorization-code".to_string(),
        state: launch.state.clone(),
        issuer: Some(state.base_url.clone()),
    };
    let first_flow = flow.clone();
    let second_flow = flow.clone();
    let (first, second) = tokio::join!(
        first_flow.complete(callback.clone()),
        second_flow.complete(callback)
    );
    assert_eq!(first, second);
    assert!(matches!(first, Ok(OAuthFlowOutcome::Authorized { .. })));
    assert_eq!(state.token_requests.load(Ordering::SeqCst), 1);

    let next = manager
        .create_oauth_flow(&bundle_id, request)
        .await
        .unwrap();
    let next_launch = next.launch().await.unwrap();
    assert_ne!(next_launch.state, launch.state);
    next.cancel(OAuthCancellationReason::Cancelled)
        .await
        .unwrap();
    manager.close().await.unwrap();
}

#[tokio::test]
async fn test_complete_and_provider_cancellation_clones_share_one_terminal_outcome() {
    let (manager, bundle_id, state) =
        authorization_code_manager(OAuthFixtureMode::PreregisteredPublic, false).await;
    let flow = manager
        .create_oauth_flow(
            &bundle_id,
            OAuthBeginRequest {
                redirect_uri: "http://127.0.0.1:9876/callback".to_string(),
                required_scope: None,
            },
        )
        .await
        .unwrap();
    let launch = flow.launch().await.unwrap();
    let completing_flow = flow.clone();
    let cancelling_flow = flow.clone();
    let (completed, cancelled) = tokio::join!(
        completing_flow.complete(OAuthCallback {
            code: "authorization-code".to_string(),
            state: launch.state.clone(),
            issuer: Some(state.base_url.clone()),
        }),
        cancelling_flow.cancel_callback(OAuthCancellation {
            state: launch.state,
            issuer: Some(state.base_url.clone()),
            reason: OAuthCancellationReason::AccessDenied,
        })
    );
    assert_eq!(completed, cancelled);
    assert!(matches!(
        completed,
        Ok(OAuthFlowOutcome::Authorized { .. })
            | Ok(OAuthFlowOutcome::Terminated {
                reason: OAuthCancellationReason::AccessDenied,
                ..
            })
    ));
    manager.close().await.unwrap();
}

#[tokio::test]
async fn test_server_replacement_and_removal_cancel_and_drain_oauth_flows() {
    let (port, state) = spawn_oauth_http_mock(OAuthFixtureMode::PreregisteredPublic, 0).await;
    let bundle_id = BundleId::try_from("oauth-lifecycle-drain").unwrap();
    let manager = configure_authorization_code_manager(
        &format!("http://127.0.0.1:{port}"),
        bundle_id.clone(),
        Arc::new(InMemoryOAuthCredentialStore::default()),
    )
    .await;
    state
        .discovery_response_delay_ms
        .store(5_000, Ordering::SeqCst);

    let first = manager
        .create_oauth_flow(
            &bundle_id,
            OAuthBeginRequest {
                redirect_uri: "http://127.0.0.1:9876/callback".to_string(),
                required_scope: None,
            },
        )
        .await
        .unwrap();
    wait_for_count(
        &state.discovery_requests,
        1,
        "replacement discovery must start",
    )
    .await;
    tokio::time::timeout(
        Duration::from_secs(1),
        manager.add_or_update_server(authorization_code_server_config(
            &state.base_url,
            bundle_id.clone(),
        )),
    )
    .await
    .expect("replacement must drain without provider timeout")
    .unwrap();
    assert!(matches!(
        first.cancel(OAuthCancellationReason::Cancelled).await,
        Ok(OAuthFlowOutcome::Terminated { .. })
    ));

    let second = manager
        .create_oauth_flow(
            &bundle_id,
            OAuthBeginRequest {
                redirect_uri: "http://127.0.0.1:9876/callback".to_string(),
                required_scope: None,
            },
        )
        .await
        .unwrap();
    wait_for_count(&state.discovery_requests, 2, "removal discovery must start").await;
    assert!(tokio::time::timeout(
        Duration::from_secs(1),
        manager.remove_server_by_id(&bundle_id),
    )
    .await
    .expect("removal must drain without provider timeout")
    .unwrap());
    assert!(matches!(
        second.cancel(OAuthCancellationReason::Cancelled).await,
        Ok(OAuthFlowOutcome::Terminated { .. })
    ));
    manager.close().await.unwrap();
}

#[tokio::test]
async fn test_server_replacement_reports_oauth_drain_timeout() {
    let (port, state) = spawn_oauth_http_mock(OAuthFixtureMode::PreregisteredPublic, 0).await;
    let bundle_id = BundleId::try_from("oauth-lifecycle-drain-timeout").unwrap();
    let store = Arc::new(DelayedLoadOAuthCredentialStore::default());
    let manager = Arc::new(
        configure_authorization_code_manager(
            &format!("http://127.0.0.1:{port}"),
            bundle_id.clone(),
            store.clone(),
        )
        .await,
    );
    let flow = manager
        .create_oauth_flow(
            &bundle_id,
            OAuthBeginRequest {
                redirect_uri: "http://127.0.0.1:9876/callback".to_string(),
                required_scope: None,
            },
        )
        .await
        .unwrap();
    flow.launch().await.unwrap();

    store.delay_next_load.store(true, Ordering::SeqCst);
    let replacement = {
        let manager = Arc::clone(&manager);
        let config = authorization_code_server_config(&state.base_url, bundle_id.clone());
        tokio::spawn(async move { manager.add_or_update_server(config).await })
    };
    store.load_started.notified().await;

    let error = tokio::time::timeout(Duration::from_secs(3), replacement)
        .await
        .expect("replacement must return after the OAuth drain deadline")
        .unwrap()
        .expect_err("a timed-out OAuth drain must not report replacement success");
    assert!(error
        .to_string()
        .contains("OAuth authorization flow did not drain"));

    store.release_load.notify_one();
    assert!(matches!(
        flow.cancel(OAuthCancellationReason::Cancelled).await,
        Ok(OAuthFlowOutcome::Terminated { .. })
    ));
}

#[tokio::test]
async fn test_flow_cancel_bypasses_long_running_oauth_request_gate() {
    let (manager, bundle_id, state) =
        authorization_code_manager(OAuthFixtureMode::PreregisteredPublic, false).await;
    let manager = Arc::new(manager);
    authorize_manager(&manager, &bundle_id, &state, None).await;
    manager.start_client_by_id(&bundle_id).await.unwrap();

    let flow = manager
        .create_oauth_flow(
            &bundle_id,
            OAuthBeginRequest {
                redirect_uri: "http://127.0.0.1:9876/callback".to_string(),
                required_scope: Some("tools.write".to_string()),
            },
        )
        .await
        .unwrap();
    let launch = flow.launch().await.unwrap();

    state.block_next_mcp_response.store(true, Ordering::SeqCst);
    let request = {
        let manager = Arc::clone(&manager);
        let bundle_id = bundle_id.clone();
        tokio::spawn(async move {
            manager
                .call_tool(
                    bundle_id.as_str(),
                    "echo",
                    serde_json::json!({"message": "hold request gate"}),
                    None,
                )
                .await
        })
    };
    state.mcp_response_started.notified().await;

    let completing = {
        let flow = flow.clone();
        let issuer = state.base_url.clone();
        tokio::spawn(async move {
            flow.complete(OAuthCallback {
                code: "authorization-code".to_string(),
                state: launch.state,
                issuer: Some(issuer),
            })
            .await
        })
    };
    tokio::task::yield_now().await;
    let cancelled = tokio::time::timeout(
        Duration::from_secs(1),
        flow.cancel(OAuthCancellationReason::Timeout),
    )
    .await
    .expect("direct flow cancellation must not wait for the OAuth request gate");
    assert!(matches!(
        cancelled,
        Ok(OAuthFlowOutcome::Terminated {
            reason: OAuthCancellationReason::Timeout,
            ..
        })
    ));
    assert_eq!(completing.await.unwrap(), cancelled);

    state.release_mcp_response.notify_one();
    request.await.unwrap().unwrap();
    manager.close().await.unwrap();
}

#[tokio::test]
async fn test_replacement_blocks_late_facade_without_recreating_retired_oauth_client() {
    let (port, state) = spawn_oauth_http_mock(OAuthFixtureMode::PreregisteredPublic, 0).await;
    let bundle_id = BundleId::try_from("oauth-replacement-race").unwrap();
    let store = Arc::new(DelayedLoadOAuthCredentialStore::default());
    let manager = Arc::new(
        configure_authorization_code_manager(
            &format!("http://127.0.0.1:{port}"),
            bundle_id.clone(),
            store.clone(),
        )
        .await,
    );
    let flow = manager
        .create_oauth_flow(
            &bundle_id,
            OAuthBeginRequest {
                redirect_uri: "http://127.0.0.1:9876/callback".to_string(),
                required_scope: None,
            },
        )
        .await
        .unwrap();
    let launch = flow.launch().await.unwrap();

    store.delay_next_load.store(true, Ordering::SeqCst);
    let replacing = {
        let manager = Arc::clone(&manager);
        let config = authorization_code_server_config(&state.base_url, bundle_id.clone());
        tokio::spawn(async move { manager.add_or_update_server(config).await })
    };
    store.load_started.notified().await;
    let late_callback = {
        let manager = Arc::clone(&manager);
        let bundle_id = bundle_id.clone();
        let issuer = state.base_url.clone();
        tokio::spawn(async move {
            manager
                .complete_oauth(
                    &bundle_id,
                    OAuthCallback {
                        code: "authorization-code".to_string(),
                        state: launch.state,
                        issuer: Some(issuer),
                    },
                )
                .await
        })
    };
    store.release_load.notify_one();

    replacing.await.unwrap().unwrap();
    assert!(matches!(
        late_callback.await.unwrap(),
        Err(OAuthError::StateMismatch)
    ));
    assert_eq!(state.token_requests.load(Ordering::SeqCst), 0);
    assert!(matches!(
        flow.cancel(OAuthCancellationReason::Cancelled).await,
        Ok(OAuthFlowOutcome::Terminated { .. })
    ));
    manager.close().await.unwrap();
}

#[tokio::test]
async fn test_computer_shutdown_cancels_and_drains_oauth_flow() {
    let (port, state) = spawn_oauth_http_mock(OAuthFixtureMode::PreregisteredPublic, 0).await;
    let bundle_id = BundleId::try_from("oauth-shutdown-drain").unwrap();
    let (computer, _temp_dir) = configure_authorization_code_computer(
        &format!("http://127.0.0.1:{port}"),
        std::slice::from_ref(&bundle_id),
        Arc::new(InMemoryOAuthCredentialStore::default()),
    )
    .await;
    state
        .discovery_response_delay_ms
        .store(5_000, Ordering::SeqCst);
    let flow = computer
        .create_oauth_flow(
            &bundle_id,
            OAuthBeginRequest {
                redirect_uri: "http://127.0.0.1:9876/callback".to_string(),
                required_scope: None,
            },
        )
        .await
        .unwrap();
    wait_for_count(
        &state.discovery_requests,
        1,
        "shutdown discovery must start",
    )
    .await;
    tokio::time::timeout(Duration::from_secs(1), computer.shutdown())
        .await
        .expect("Computer shutdown must not retain OAuth provider timeout")
        .unwrap();
    assert!(matches!(
        flow.cancel(OAuthCancellationReason::Cancelled).await,
        Ok(OAuthFlowOutcome::Terminated { .. })
    ));
}

#[tokio::test]
async fn test_computer_shutdown_preempts_compat_begin_during_discovery() {
    let (port, state) = spawn_oauth_http_mock(OAuthFixtureMode::PreregisteredPublic, 0).await;
    let bundle_id = BundleId::try_from("oauth-shutdown-compat-begin").unwrap();
    let (computer, _temp_dir) = configure_authorization_code_computer(
        &format!("http://127.0.0.1:{port}"),
        std::slice::from_ref(&bundle_id),
        Arc::new(InMemoryOAuthCredentialStore::default()),
    )
    .await;
    let computer = Arc::new(computer);
    state
        .discovery_response_delay_ms
        .store(5_000, Ordering::SeqCst);
    let beginning = {
        let computer = Arc::clone(&computer);
        let bundle_id = bundle_id.clone();
        tokio::spawn(async move {
            computer
                .begin_oauth(
                    &bundle_id,
                    OAuthBeginRequest {
                        redirect_uri: "http://127.0.0.1:9876/callback".to_string(),
                        required_scope: None,
                    },
                )
                .await
        })
    };
    state.discovery_started.notified().await;

    tokio::time::timeout(Duration::from_secs(1), computer.shutdown())
        .await
        .expect("shutdown must not wait for compat begin provider I/O")
        .unwrap();
    assert!(matches!(
        beginning.await.unwrap(),
        Err(OAuthError::AuthorizationCancelled)
    ));
}

#[tokio::test]
async fn test_computer_shutdown_preempts_compat_complete_during_exchange() {
    let (port, state) = spawn_oauth_http_mock(OAuthFixtureMode::PreregisteredPublic, 0).await;
    let bundle_id = BundleId::try_from("oauth-shutdown-compat-complete").unwrap();
    let (computer, _temp_dir) = configure_authorization_code_computer(
        &format!("http://127.0.0.1:{port}"),
        std::slice::from_ref(&bundle_id),
        Arc::new(InMemoryOAuthCredentialStore::default()),
    )
    .await;
    let computer = Arc::new(computer);
    state.token_response_delay_ms.store(5_000, Ordering::SeqCst);
    let launch = computer
        .begin_oauth(
            &bundle_id,
            OAuthBeginRequest {
                redirect_uri: "http://127.0.0.1:9876/callback".to_string(),
                required_scope: None,
            },
        )
        .await
        .unwrap();
    let completing = {
        let computer = Arc::clone(&computer);
        let bundle_id = bundle_id.clone();
        let callback_state = launch.state;
        let issuer = state.base_url.clone();
        tokio::spawn(async move {
            computer
                .complete_oauth(
                    &bundle_id,
                    OAuthCallback {
                        code: "authorization-code".to_string(),
                        state: callback_state,
                        issuer: Some(issuer),
                    },
                )
                .await
        })
    };
    state.token_started.notified().await;

    tokio::time::timeout(Duration::from_secs(1), computer.shutdown())
        .await
        .expect("shutdown must not wait for compat complete provider I/O")
        .unwrap();
    assert!(matches!(
        completing.await.unwrap(),
        Ok(OAuthFlowOutcome::Terminated {
            reason: OAuthCancellationReason::Cancelled,
            ..
        })
    ));
}

#[tokio::test]
async fn test_required_callback_issuer_failure_does_not_consume_pending_flow() {
    let (manager, bundle_id, state) =
        authorization_code_manager(OAuthFixtureMode::PreregisteredPublic, false).await;
    let launch = manager
        .begin_oauth(
            &bundle_id,
            OAuthBeginRequest {
                redirect_uri: "http://127.0.0.1:9876/callback".to_string(),
                required_scope: None,
            },
        )
        .await
        .unwrap();

    assert!(matches!(
        manager
            .complete_oauth(
                &bundle_id,
                OAuthCallback {
                    code: "authorization-code".to_string(),
                    state: launch.state.clone(),
                    issuer: None,
                },
            )
            .await,
        Err(OAuthError::IssuerMismatch)
    ));
    assert_eq!(
        manager.oauth_status(&bundle_id).await.unwrap(),
        OAuthStatus::AuthorizationPending
    );

    let outcome = manager
        .complete_oauth(
            &bundle_id,
            OAuthCallback {
                code: "authorization-code".to_string(),
                state: launch.state,
                issuer: Some(state.base_url.clone()),
            },
        )
        .await
        .unwrap();
    let OAuthFlowOutcome::Authorized { scopes } = outcome else {
        panic!("corrected callback must complete the pending authorization");
    };
    assert!(scopes.iter().any(|scope| scope == "tools.read"));
    manager.close().await.unwrap();
}

#[tokio::test]
async fn test_aborted_complete_oauth_still_reaches_a_terminal_authorized_status() {
    let (manager, bundle_id, state) =
        authorization_code_manager(OAuthFixtureMode::PreregisteredPublic, false).await;
    state.token_response_delay_ms.store(250, Ordering::SeqCst);
    let launch = manager
        .begin_oauth(
            &bundle_id,
            OAuthBeginRequest {
                redirect_uri: "http://127.0.0.1:9876/callback".to_string(),
                required_scope: None,
            },
        )
        .await
        .unwrap();
    let manager = Arc::new(manager);
    let task_manager = Arc::clone(&manager);
    let task_bundle = bundle_id.clone();
    let issuer = state.base_url.clone();
    let complete_task = tokio::spawn(async move {
        task_manager
            .complete_oauth(
                &task_bundle,
                OAuthCallback {
                    code: "authorization-code".to_string(),
                    state: launch.state,
                    issuer: Some(issuer),
                },
            )
            .await
    });

    tokio::time::timeout(Duration::from_secs(2), async {
        while state.token_requests.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("token exchange must start before aborting the caller");
    complete_task.abort();
    assert!(complete_task.await.unwrap_err().is_cancelled());

    let status = tokio::time::timeout(Duration::from_secs(2), manager.oauth_status(&bundle_id))
        .await
        .expect("detached terminal task must release the lifecycle gate")
        .unwrap();
    assert!(matches!(status, OAuthStatus::Authorized { .. }));
    manager.close().await.unwrap();
}

#[tokio::test]
async fn test_cancel_wins_while_failed_completion_restores_baseline() {
    let (_port, state) = spawn_oauth_http_mock(OAuthFixtureMode::PreregisteredPublic, 0).await;
    let store = Arc::new(DelayedLoadOAuthCredentialStore::default());
    let bundle_id = BundleId::try_from("oauth-cancel-failed-completion").unwrap();
    let manager = Arc::new(
        configure_authorization_code_manager(&state.base_url, bundle_id.clone(), store.clone())
            .await,
    );
    let flow = manager
        .create_oauth_flow(
            &bundle_id,
            OAuthBeginRequest {
                redirect_uri: "http://127.0.0.1:9876/callback".to_string(),
                required_scope: None,
            },
        )
        .await
        .unwrap();
    let launch = flow.launch().await.unwrap();
    state.reject_token_remaining.store(1, Ordering::SeqCst);
    store.delay_next_load.store(true, Ordering::SeqCst);
    let completing = {
        let flow = flow.clone();
        let issuer = state.base_url.clone();
        tokio::spawn(async move {
            flow.complete(OAuthCallback {
                code: "authorization-code".to_string(),
                state: launch.state,
                issuer: Some(issuer),
            })
            .await
        })
    };
    store.load_started.notified().await;

    let cancelling = {
        let flow = flow.clone();
        tokio::spawn(async move { flow.cancel(OAuthCancellationReason::Timeout).await })
    };
    tokio::task::yield_now().await;
    store.release_load.notify_one();

    let cancelled = cancelling.await.unwrap().unwrap();
    assert_eq!(
        cancelled,
        OAuthFlowOutcome::Terminated {
            reason: OAuthCancellationReason::Timeout,
            status: OAuthStatus::Unauthorized,
        }
    );
    assert_eq!(completing.await.unwrap().unwrap(), cancelled);
    manager.close().await.unwrap();
}

#[tokio::test]
async fn test_aborted_cancel_oauth_still_reaches_a_terminal_unauthorized_status() {
    let (_port, state) = spawn_oauth_http_mock(OAuthFixtureMode::PreregisteredPublic, 0).await;
    let delayed_store = Arc::new(DelayedLoadOAuthCredentialStore::default());
    let store: Arc<dyn OAuthCredentialStore> = delayed_store.clone();
    let bundle_id = BundleId::try_from("oauth-aborted-cancellation").unwrap();
    let manager =
        configure_authorization_code_manager(&state.base_url, bundle_id.clone(), store).await;
    let launch = manager
        .begin_oauth(
            &bundle_id,
            OAuthBeginRequest {
                redirect_uri: "http://127.0.0.1:9876/callback".to_string(),
                required_scope: None,
            },
        )
        .await
        .unwrap();
    delayed_store.delay_next_load.store(true, Ordering::SeqCst);
    let manager = Arc::new(manager);
    let task_manager = Arc::clone(&manager);
    let task_bundle = bundle_id.clone();
    let cancel_task = tokio::spawn(async move {
        task_manager
            .cancel_oauth(
                &task_bundle,
                OAuthCancellation {
                    state: launch.state,
                    issuer: None,
                    reason: OAuthCancellationReason::Timeout,
                },
            )
            .await
    });

    tokio::time::timeout(
        Duration::from_secs(2),
        delayed_store.load_started.notified(),
    )
    .await
    .expect("credential restoration must start before aborting the caller");
    cancel_task.abort();
    assert!(cancel_task.await.unwrap_err().is_cancelled());
    delayed_store.release_load.notify_one();

    let status = tokio::time::timeout(Duration::from_secs(2), manager.oauth_status(&bundle_id))
        .await
        .expect("detached terminal task must release the lifecycle gate")
        .unwrap();
    assert_eq!(status, OAuthStatus::Unauthorized);

    let replacement = manager
        .begin_oauth(
            &bundle_id,
            OAuthBeginRequest {
                redirect_uri: "http://127.0.0.1:9876/callback".to_string(),
                required_scope: None,
            },
        )
        .await
        .expect("terminal cancellation must not leave an active pending flow");
    manager
        .cancel_oauth(
            &bundle_id,
            OAuthCancellation {
                state: replacement.state,
                issuer: None,
                reason: OAuthCancellationReason::Cancelled,
            },
        )
        .await
        .unwrap();
    manager.close().await.unwrap();
}

#[tokio::test]
async fn test_abandoned_callback_is_rejected_by_replacement_coordinator() {
    let (_port, state) = spawn_oauth_http_mock(OAuthFixtureMode::PreregisteredPublic, 0).await;
    let store: Arc<dyn OAuthCredentialStore> = Arc::new(InMemoryOAuthCredentialStore::default());
    let bundle_id = BundleId::try_from("oauth-replacement-coordinator").unwrap();
    let first = configure_authorization_code_manager(
        &state.base_url,
        bundle_id.clone(),
        Arc::clone(&store),
    )
    .await;
    let abandoned = first
        .begin_oauth(
            &bundle_id,
            OAuthBeginRequest {
                redirect_uri: "https://callback.example.test/oauth/callback".to_string(),
                required_scope: None,
            },
        )
        .await
        .unwrap();
    first.close().await.unwrap();
    drop(first);

    let replacement =
        configure_authorization_code_manager(&state.base_url, bundle_id.clone(), store).await;
    assert!(matches!(
        replacement
            .complete_oauth(
                &bundle_id,
                OAuthCallback {
                    code: "abandoned-code".to_string(),
                    state: abandoned.state.clone(),
                    issuer: Some(state.base_url.clone()),
                },
            )
            .await,
        Err(OAuthError::StateMismatch)
    ));
    let fresh = replacement
        .begin_oauth(
            &bundle_id,
            OAuthBeginRequest {
                redirect_uri: "https://callback.example.test/oauth/callback".to_string(),
                required_scope: None,
            },
        )
        .await
        .unwrap();
    assert_ne!(fresh.state, abandoned.state);
    replacement
        .cancel_oauth(
            &bundle_id,
            OAuthCancellation {
                state: fresh.state,
                issuer: Some(state.base_url.clone()),
                reason: OAuthCancellationReason::Cancelled,
            },
        )
        .await
        .unwrap();
    replacement.close().await.unwrap();
}

#[tokio::test]
async fn test_shared_oauth_store_clear_isolated_by_bundle_id() {
    let (_port, state) = spawn_oauth_http_mock(OAuthFixtureMode::PreregisteredPublic, 0).await;
    let store: Arc<dyn OAuthCredentialStore> = Arc::new(InMemoryOAuthCredentialStore::default());
    let first_bundle = BundleId::try_from("oauth-bundle-a").unwrap();
    let second_bundle = BundleId::try_from("oauth-bundle-b").unwrap();
    let first = configure_authorization_code_manager(
        &state.base_url,
        first_bundle.clone(),
        Arc::clone(&store),
    )
    .await;
    let second = configure_authorization_code_manager(
        &state.base_url,
        second_bundle.clone(),
        Arc::clone(&store),
    )
    .await;

    authorize_manager(&first, &first_bundle, &state, None).await;
    authorize_manager(&second, &second_bundle, &state, None).await;
    first.clear_oauth(&first_bundle).await.unwrap();

    assert_eq!(
        first.oauth_status(&first_bundle).await.unwrap(),
        OAuthStatus::Unauthorized
    );
    assert!(matches!(
        second.oauth_status(&second_bundle).await.unwrap(),
        OAuthStatus::Authorized { .. }
    ));
    first.close().await.unwrap();
    second.close().await.unwrap();
}

#[tokio::test]
async fn test_authorization_code_restores_after_manager_rebuild_with_injected_store() {
    let (_port, state) = spawn_oauth_http_mock(OAuthFixtureMode::PreregisteredPublic, 0).await;
    let store: Arc<dyn OAuthCredentialStore> = Arc::new(InMemoryOAuthCredentialStore::default());
    let bundle_id = BundleId::try_from("oauth-persistent-rebuild").unwrap();

    let first = configure_authorization_code_manager(
        &state.base_url,
        bundle_id.clone(),
        Arc::clone(&store),
    )
    .await;
    authorize_manager(&first, &bundle_id, &state, None).await;
    first.close().await.unwrap();
    drop(first);

    let restored = configure_authorization_code_manager(
        &state.base_url,
        bundle_id.clone(),
        Arc::clone(&store),
    )
    .await;
    assert!(matches!(
        restored.oauth_status(&bundle_id).await.unwrap(),
        OAuthStatus::Authorized { .. }
    ));
    restored.start_client_by_id(&bundle_id).await.unwrap();
    let result = restored
        .call_tool(
            bundle_id.as_str(),
            "echo",
            serde_json::json!({"message": "restored"}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(result.content[0].as_text().unwrap().text, "restored");
    restored.clear_oauth(&bundle_id).await.unwrap();
    restored.close().await.unwrap();
    drop(restored);

    let cleared =
        configure_authorization_code_manager(&state.base_url, bundle_id.clone(), store).await;
    assert_eq!(
        cleared.oauth_status(&bundle_id).await.unwrap(),
        OAuthStatus::Unauthorized
    );
    cleared.close().await.unwrap();
}

#[tokio::test]
async fn test_computer_injected_oauth_store_routes_all_bundles_and_restores_after_rebuild() {
    let (_port, state) = spawn_oauth_http_mock(OAuthFixtureMode::PreregisteredPublic, 0).await;
    let recording_store = Arc::new(RecordingOAuthCredentialStore::default());
    let store: Arc<dyn OAuthCredentialStore> = recording_store.clone();
    let first_bundle = BundleId::try_from("oauth-computer-a").unwrap();
    let second_bundle = BundleId::try_from("oauth-computer-b").unwrap();

    let (first_computer, first_temp_dir) = configure_authorization_code_computer(
        &state.base_url,
        &[first_bundle.clone(), second_bundle.clone()],
        Arc::clone(&store),
    )
    .await;
    authorize_computer(&first_computer, &first_bundle, &state).await;
    authorize_computer(&first_computer, &second_bundle, &state).await;

    let operations = recording_store.operations().await;
    for bundle_id in [&first_bundle, &second_bundle] {
        assert!(
            operations.iter().any(|operation| matches!(
                operation,
                CredentialStoreOperation::Save(key)
                    if key.bundle_id == *bundle_id
                        && key.record_kind == OAuthCredentialRecordKind::IssuerIndex
            )),
            "the host store must receive credentials for {bundle_id}"
        );
    }

    let clear_operation_start = operations.len();
    first_computer.clear_oauth(&first_bundle).await.unwrap();
    let operations = recording_store.operations().await;
    let clear_operations = &operations[clear_operation_start..];
    assert!(
        clear_operations
            .iter()
            .any(|operation| matches!(operation, CredentialStoreOperation::Delete(_))),
        "clear_oauth must delete through the host store"
    );
    assert!(
        clear_operations.iter().all(|operation| match operation {
            CredentialStoreOperation::Load(key)
            | CredentialStoreOperation::Save(key)
            | CredentialStoreOperation::Delete(key) => key.bundle_id == first_bundle,
        }),
        "clearing one bundle must not address another bundle's credential keys"
    );
    assert!(matches!(
        first_computer.oauth_status(&first_bundle).await.unwrap(),
        OAuthStatus::Unauthorized
    ));
    assert!(matches!(
        first_computer.oauth_status(&second_bundle).await.unwrap(),
        OAuthStatus::Authorized { .. }
    ));

    first_computer.shutdown().await.unwrap();
    drop(first_computer);
    drop(first_temp_dir);

    let authorized_requests_before_restore = state.authorized_mcp_requests.load(Ordering::SeqCst);
    let (restored_computer, restored_temp_dir) = configure_authorization_code_computer(
        &state.base_url,
        std::slice::from_ref(&second_bundle),
        Arc::clone(&store),
    )
    .await;
    assert!(matches!(
        restored_computer
            .oauth_status(&second_bundle)
            .await
            .unwrap(),
        OAuthStatus::Authorized { .. }
    ));
    restored_computer
        .start_mcp_client(&second_bundle)
        .await
        .unwrap();
    assert!(
        state.authorized_mcp_requests.load(Ordering::SeqCst) > authorized_requests_before_restore,
        "the rebuilt Computer must restore credentials and initialize over real HTTP"
    );
    restored_computer.clear_oauth(&second_bundle).await.unwrap();
    restored_computer.shutdown().await.unwrap();
    drop(restored_computer);
    drop(restored_temp_dir);

    let operations = recording_store.operations().await;
    assert!(
        operations
            .iter()
            .any(|operation| matches!(operation, CredentialStoreOperation::Load(_)))
            && operations
                .iter()
                .any(|operation| matches!(operation, CredentialStoreOperation::Save(_)))
            && operations
                .iter()
                .any(|operation| matches!(operation, CredentialStoreOperation::Delete(_))),
        "the injected store must receive load, save, and delete operations"
    );
}

#[tokio::test]
async fn test_streamable_http_insufficient_scope_reauthorization_end_to_end() {
    let (manager, bundle_id, state) =
        authorization_code_manager(OAuthFixtureMode::PreregisteredPublic, true).await;
    authorize_manager(&manager, &bundle_id, &state, None).await;
    manager.start_client_by_id(&bundle_id).await.unwrap();

    let first = manager
        .call_tool(
            bundle_id.as_str(),
            "echo",
            serde_json::json!({"message": "first"}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(first.is_error, Some(true));
    assert_eq!(
        manager.oauth_status(&bundle_id).await.unwrap(),
        OAuthStatus::ReauthorizationRequired {
            required_scope: "tools.write".to_string()
        }
    );

    authorize_manager(
        &manager,
        &bundle_id,
        &state,
        Some("tools.write".to_string()),
    )
    .await;
    let second = manager
        .call_tool(
            bundle_id.as_str(),
            "echo",
            serde_json::json!({"message": "second"}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(second.content[0].as_text().unwrap().text, "second");
    let forms = state.token_forms.lock().await;
    assert_eq!(forms.len(), 2);
}

// ============================================================
// Test 2: HTTP happy path (SSE responses)
// ============================================================
#[tokio::test]
async fn test_http_happy_path_sse_response() {
    let (port, _state) = spawn_http_mock(true).await;

    let client = HttpMCPClient::new(HttpServerParameters {
        url: format!("http://127.0.0.1:{}", port),
        headers: HashMap::new(),
    });

    client.connect().await.unwrap();
    assert_eq!(client.state(), ClientState::Connected);

    let tools = client.list_tools().await.unwrap();
    assert_eq!(tools.len(), 2);

    let result = client
        .call_tool("add", serde_json::json!({"a": 3, "b": 4}))
        .await
        .unwrap();
    assert_eq!(
        result.content[0]
            .as_text()
            .expect("expected text content")
            .text,
        "7"
    );

    client.disconnect().await.unwrap();
}

// ============================================================
// Test 3: HTTP session_id from header
// ============================================================
#[tokio::test]
async fn test_http_session_id_from_header() {
    let (port, state) = spawn_http_mock(false).await;

    let client = HttpMCPClient::new(HttpServerParameters {
        url: format!("http://127.0.0.1:{}", port),
        headers: HashMap::new(),
    });

    client.connect().await.unwrap();

    // After connect, list_tools should carry mcp-session-id header
    let _ = client.list_tools().await.unwrap();

    // Check recorded headers contain mcp-session-id
    {
        let headers = state.recorded_headers.lock().await;
        let session_headers: Vec<_> = headers
            .iter()
            .filter(|(k, _)| k == "mcp-session-id")
            .collect();
        // At least one request after initialize should carry the session id
        assert!(
            session_headers.iter().any(|(_, v)| v == "test-session-001"),
            "Expected mcp-session-id header with value test-session-001, got: {:?}",
            session_headers
        );
    } // drop lock before disconnect

    client.disconnect().await.unwrap();
}

// ============================================================
// Test 4: SSE happy path
// ============================================================
#[tokio::test]
async fn test_sse_happy_path() {
    let (port, _state) = spawn_sse_mock(true).await;

    let client = SseMCPClient::new(SseServerParameters {
        url: format!("http://127.0.0.1:{}/sse", port),
        headers: HashMap::new(),
    });

    client.connect().await.unwrap();
    assert_eq!(client.state(), ClientState::Connected);

    let tools = client.list_tools().await.unwrap();
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0].name, "echo");
    assert_eq!(tools[1].name, "add");

    let result = client
        .call_tool("echo", serde_json::json!({"message": "sse-test"}))
        .await
        .unwrap();
    assert_eq!(
        result.content[0]
            .as_text()
            .expect("expected text content")
            .text,
        "sse-test"
    );

    client.disconnect().await.unwrap();
    assert_eq!(client.state(), ClientState::Disconnected);
}

// ============================================================
// Test 5: SSE POST returns JSON directly
// ============================================================
#[tokio::test]
async fn test_sse_post_json_response() {
    let (port, _state) = spawn_sse_mock(true).await;

    let client = SseMCPClient::new(SseServerParameters {
        url: format!("http://127.0.0.1:{}/sse", port),
        headers: HashMap::new(),
    });

    client.connect().await.unwrap();

    // The mock returns application/json directly on POST
    let tools = client.list_tools().await.unwrap();
    assert_eq!(tools.len(), 2);

    let result = client
        .call_tool("add", serde_json::json!({"a": 10, "b": 20}))
        .await
        .unwrap();
    assert_eq!(
        result.content[0]
            .as_text()
            .expect("expected text content")
            .text,
        "30"
    );

    client.disconnect().await.unwrap();
}

// ============================================================
// Test 6: SSE resource update push
// ============================================================
#[tokio::test]
async fn test_sse_resource_update_push() {
    let (port, state) = spawn_sse_mock(true).await;

    let client = SseMCPClient::new(SseServerParameters {
        url: format!("http://127.0.0.1:{}/sse", port),
        headers: HashMap::new(),
    });

    // Subscribe to updates before connecting
    let mut update_rx = client.subscribe_to_updates().await;

    client.connect().await.unwrap();

    // Push a resource update via SSE stream
    {
        let tx_guard = state.sse_tx.lock().await;
        if let Some(ref tx) = *tx_guard {
            let update = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "resources/update",
                "params": {
                    "uri": "window://test-window",
                    "data": { "title": "Updated Window", "content": "new content" }
                }
            });
            let sse_event = format!("event: message\ndata: {}\n\n", update);
            let _ = tx.send(Ok(Frame::data(Bytes::from(sse_event))));
        }
    }

    // Wait for the update to arrive
    let update = tokio::time::timeout(std::time::Duration::from_secs(5), update_rx.recv())
        .await
        .expect("timed out waiting for resource update")
        .expect("update channel closed");

    assert_eq!(update.uri, "window://test-window");
    assert_eq!(
        update.data.get("title").and_then(|v| v.as_str()),
        Some("Updated Window")
    );
    assert_eq!(
        update.data.get("content").and_then(|v| v.as_str()),
        Some("new content")
    );

    client.disconnect().await.unwrap();
}

// ============================================================
// Test 7: SSE POST failure returns error immediately (not 30s timeout)
// ============================================================

/// Mock SSE server where POST /messages returns a configurable error status
async fn spawn_sse_mock_post_error(status_code: StatusCode) -> u16 {
    let (listener, port) = bind_random().await;

    tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => break,
            };
            let io = TokioIo::new(stream);
            let sc = status_code;
            tokio::spawn(async move {
                let service = service_fn(move |req: Request<hyper::body::Incoming>| {
                    async move {
                        let method = req.method().clone();
                        let path = req.uri().path().to_string();

                        // SSE GET endpoint works normally
                        if method == Method::GET && path == "/sse" {
                            let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<
                                Result<Frame<Bytes>, Infallible>,
                            >();
                            let endpoint_event = "event: endpoint\ndata: /messages\n\n";
                            let _ = tx.send(Ok(Frame::data(Bytes::from(endpoint_event))));
                            // Keep the channel alive so SSE stream stays open
                            std::mem::forget(tx);

                            let stream = futures_util::stream::unfold(rx, |mut rx| async move {
                                rx.recv().await.map(|item| (item, rx))
                            });
                            let body = StreamBody::new(stream);
                            let boxed: BoxBody = http_body_util::BodyExt::boxed(body);

                            return Ok::<_, Infallible>(
                                Response::builder()
                                    .status(StatusCode::OK)
                                    .header("Content-Type", "text/event-stream")
                                    .header("Cache-Control", "no-cache")
                                    .body(boxed)
                                    .unwrap(),
                            );
                        }

                        // POST always returns error status
                        if method == Method::POST && path == "/messages" {
                            let _ = req.into_body().collect().await;
                            return Ok(Response::builder()
                                .status(sc)
                                .body(full_body("error"))
                                .unwrap());
                        }

                        Ok(Response::builder()
                            .status(StatusCode::NOT_FOUND)
                            .body(full_body("not found"))
                            .unwrap())
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, service)
                    .await;
            });
        }
    });

    port
}

#[tokio::test]
async fn test_sse_post_failure_returns_error_immediately() {
    let port = spawn_sse_mock_post_error(StatusCode::INTERNAL_SERVER_ERROR).await;

    let client = SseMCPClient::new(SseServerParameters {
        url: format!("http://127.0.0.1:{}/sse", port),
        headers: HashMap::new(),
    });

    let error = tokio::time::timeout(Duration::from_secs(15), client.connect())
        .await
        .expect("POST failure must be reported before the 30s response timeout")
        .expect_err("POST failure must fail initialization");

    assert!(
        matches!(
            error,
            MCPClientError::ProtocolError(ref message) if message.contains("500")
        ),
        "expected the HTTP 500 protocol error, got {error:?}"
    );
}

// ============================================================
// Test 8: SSE POST 403 returns error (not timeout)
// ============================================================
#[tokio::test]
async fn test_sse_post_non_success_status_returns_error() {
    let port = spawn_sse_mock_post_error(StatusCode::FORBIDDEN).await;

    let client = SseMCPClient::new(SseServerParameters {
        url: format!("http://127.0.0.1:{}/sse", port),
        headers: HashMap::new(),
    });

    let start = std::time::Instant::now();
    let result = client.connect().await;
    let elapsed = start.elapsed();

    assert!(result.is_err(), "Expected error, got Ok");
    assert!(
        elapsed.as_secs() < 5,
        "Should fail quickly, not timeout. Took {:?}",
        elapsed
    );
}
