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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Bytes, Frame};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
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
    OAuthOptions, OAuthStatus,
};
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
}

impl RecordingOAuthCredentialStore {
    async fn operations(&self) -> Vec<CredentialStoreOperation> {
        self.operations.lock().await.clone()
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
    mode: OAuthFixtureMode,
    mcp_response_sse: bool,
    token_expires_in: u64,
    total_requests: AtomicUsize,
    token_requests: AtomicUsize,
    registration_requests: AtomicUsize,
    authorized_mcp_requests: AtomicUsize,
    challenge_tools_write_remaining: AtomicUsize,
    reject_authorized_remaining: AtomicUsize,
    protected_custom_header_requests: AtomicUsize,
    authorization_custom_header_requests: AtomicUsize,
    token_forms: Mutex<Vec<HashMap<String, String>>>,
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
        return Ok(Response::builder()
            .header("Content-Type", "application/json")
            .body(full_body(
                serde_json::json!({
                    "resource": format!("{}/mcp", state.base_url),
                    "authorization_servers": [&state.base_url],
                    "scopes_supported": ["tools.read"],
                })
                .to_string(),
            ))
            .unwrap());
    }
    if method == Method::GET && path == "/.well-known/oauth-protected-resource" {
        return Ok(Response::builder()
            .header("Content-Type", "application/json")
            .body(full_body(
                serde_json::json!({
                    "resource": format!("{}/mcp", state.base_url),
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
            || form.get("resource").map(String::as_str)
                != Some(format!("{}/mcp", state.base_url).as_str())
        {
            return Ok(Response::builder()
                .status(StatusCode::UNAUTHORIZED)
                .body(empty_body())
                .unwrap());
        }
        state.token_forms.lock().await.push(form.clone());
        let request_index = state.token_requests.fetch_add(1, Ordering::SeqCst);
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
    let state = Arc::new(OAuthMockState {
        base_url: format!("http://127.0.0.1:{port}"),
        mode,
        mcp_response_sse,
        token_expires_in,
        total_requests: AtomicUsize::new(0),
        token_requests: AtomicUsize::new(0),
        registration_requests: AtomicUsize::new(0),
        authorized_mcp_requests: AtomicUsize::new(0),
        challenge_tools_write_remaining: AtomicUsize::new(challenge_tools_write_count),
        reject_authorized_remaining: AtomicUsize::new(0),
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
    manager
        .complete_oauth(&delivery.bundle_id, delivery.callback)
        .await
        .unwrap();
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

    host.register_route(
        "expired-state".to_string(),
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
                state: "expired-state".to_string(),
                issuer: Some(mock_state.base_url.clone()),
                untrusted_business_ids: HashMap::new(),
            },
            now,
        ),
        Err("expired-state")
    ));
    assert!(host.take_cli_callback(TARGET_CLI).is_none());
    manager.close().await.unwrap();

    let logs = captured_logs.text();
    for sensitive in [
        AUTHORIZATION_CODE,
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

    manager
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
        manager.oauth_status(&bundle_id).await.unwrap(),
        OAuthStatus::Unauthorized
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
    manager
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
    manager.close().await.unwrap();
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
                        && key.record_kind == OAuthCredentialRecordKind::Credentials
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
