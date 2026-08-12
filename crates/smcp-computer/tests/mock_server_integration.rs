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
use serde_json::Value;
use socketioxide::extract::{AckSender, Data, SocketRef};
use socketioxide::SocketIo;
use tokio::net::TcpListener;
use tokio::sync::{oneshot, Mutex, Notify};
use tower::Layer;
use tracing_subscriber::fmt::MakeWriter;

use smcp_computer::computer::{Computer, ConnectOptions, SilentSession};
use smcp_computer::errors::ComputerError;
use smcp_computer::mcp_clients::http_client::HttpMCPClient;
use smcp_computer::mcp_clients::model::*;
use smcp_computer::mcp_clients::sse_client::SseMCPClient;
use smcp_computer::mcp_clients::MCPServerManager;
use smcp_computer::oauth::{
    InMemoryOAuthCredentialStore, OAuthBeginRequest, OAuthCallback, OAuthCancellation,
    OAuthCancellationReason, OAuthCredentialKey, OAuthCredentialRecordKind, OAuthCredentialStore,
    OAuthCredentialStoreError, OAuthError, OAuthFlowOutcome, OAuthProtocolError, OAuthStatus,
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
    fail_next_credential_delete: AtomicBool,
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
        if self
            .fail_next_credential_delete
            .swap(false, Ordering::SeqCst)
        {
            return Err(OAuthCredentialStoreError::OperationFailed);
        }
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

async fn spawn_tool_list_recording_relay(
    updates: Arc<AtomicUsize>,
) -> (String, oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (layer, io) = SocketIo::new_layer();
    io.ns("/smcp", move |socket: SocketRef| {
        let updates = Arc::clone(&updates);
        async move {
            socket.on(
                "server:join_office",
                |_socket: SocketRef, _data: Data<Value>, ack: AckSender| async move {
                    let _ = ack.send(&(true, None::<String>));
                },
            );
            socket.on(
                "server:update_tool_list",
                move |_socket: SocketRef, _data: Data<Value>| {
                    let updates = Arc::clone(&updates);
                    async move {
                        updates.fetch_add(1, Ordering::SeqCst);
                    }
                },
            );
        }
    });

    let fallback = tower::service_fn(|_request: Request<hyper::body::Incoming>| async move {
        Ok::<_, Infallible>(Response::new(Full::<Bytes>::new(Bytes::new())))
    });
    let service = layer.layer(fallback);
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { break };
                    let service = hyper_util::service::TowerToHyperService::new(service.clone());
                    tokio::spawn(async move {
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(TokioIo::new(stream), service)
                            .with_upgrades()
                            .await;
                    });
                }
            }
        }
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    (format!("http://{address}"), shutdown_tx)
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

#[derive(Clone, Copy)]
enum AuthGateMode {
    BareUnauthorized,
    Basic,
    BearerWithoutMetadata,
    BearerWithInvalidMetadata,
    Forbidden,
    StaticBearerRejected,
}

struct AuthGateState {
    mode: AuthGateMode,
    base_url: String,
    mcp_requests: AtomicUsize,
    metadata_requests: AtomicUsize,
    saw_static_authorization: AtomicBool,
}

async fn auth_gate_handler(
    request: Request<hyper::body::Incoming>,
    state: Arc<AuthGateState>,
) -> Result<Response<BoxBody>, Infallible> {
    let path = request.uri().path().to_string();
    if path.starts_with("/.well-known/") {
        state.metadata_requests.fetch_add(1, Ordering::SeqCst);
        return Ok(Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(empty_body())
            .unwrap());
    }
    if path != "/mcp" || request.method() != Method::POST {
        return Ok(Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(empty_body())
            .unwrap());
    }
    state.mcp_requests.fetch_add(1, Ordering::SeqCst);
    if request
        .headers()
        .get("authorization")
        .is_some_and(|value| value == "Bearer static-token")
    {
        state.saw_static_authorization.store(true, Ordering::SeqCst);
    }
    let response = match state.mode {
        AuthGateMode::BareUnauthorized => Response::builder().status(StatusCode::UNAUTHORIZED),
        AuthGateMode::Basic => Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header("WWW-Authenticate", r#"Basic realm="legacy""#),
        AuthGateMode::BearerWithoutMetadata => Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header("WWW-Authenticate", r#"Bearer realm="mcp""#),
        AuthGateMode::BearerWithInvalidMetadata | AuthGateMode::StaticBearerRejected => {
            Response::builder().status(StatusCode::UNAUTHORIZED).header(
                "WWW-Authenticate",
                format!(
                    "Bearer resource_metadata=\"{}/.well-known/oauth-protected-resource/mcp\"",
                    state.base_url
                ),
            )
        }
        AuthGateMode::Forbidden => Response::builder().status(StatusCode::FORBIDDEN),
    };
    Ok(response.body(empty_body()).unwrap())
}

async fn spawn_auth_gate(mode: AuthGateMode) -> (u16, Arc<AuthGateState>) {
    let (listener, port) = bind_random().await;
    let state = Arc::new(AuthGateState {
        mode,
        base_url: format!("http://127.0.0.1:{port}"),
        mcp_requests: AtomicUsize::new(0),
        metadata_requests: AtomicUsize::new(0),
        saw_static_authorization: AtomicBool::new(false),
    });
    let server_state = Arc::clone(&state);
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let state = Arc::clone(&server_state);
            tokio::spawn(async move {
                let service =
                    service_fn(move |request| auth_gate_handler(request, Arc::clone(&state)));
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    (port, state)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OAuthFixtureMode {
    Dynamic,
}

struct OAuthMockState {
    base_url: String,
    resource: StdMutex<String>,
    mcp_response_sse: bool,
    token_expires_in: u64,
    discovery_response_delay_ms: AtomicU64,
    registration_response_delay_ms: AtomicU64,
    token_response_delay_ms: AtomicU64,
    disable_authorization_metadata: AtomicBool,
    omit_authorization_issuer: AtomicBool,
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
    anonymous_initialize_requests: AtomicUsize,
    authorized_initialize_requests: AtomicUsize,
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
    let body_bytes = req.into_body().collect().await.unwrap().to_bytes();

    if method == Method::POST && path == "/mcp" {
        let rpc_method = serde_json::from_slice::<serde_json::Value>(&body_bytes)
            .ok()
            .and_then(|body| {
                body.get("method")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            });
        if rpc_method.as_deref() == Some("initialize") {
            if authorized {
                state
                    .authorized_initialize_requests
                    .fetch_add(1, Ordering::SeqCst);
            } else {
                state
                    .anonymous_initialize_requests
                    .fetch_add(1, Ordering::SeqCst);
            }
        }
    }

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
        if state.disable_authorization_metadata.load(Ordering::SeqCst) {
            return Ok(Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(empty_body())
                .unwrap());
        }
        let mut metadata = serde_json::json!({
            "authorization_endpoint": format!("{}/authorize", state.base_url),
            "token_endpoint": format!("{}/token", state.base_url),
            "registration_endpoint": format!("{}/register", state.base_url),
            "response_types_supported": ["code"],
            "grant_types_supported": ["authorization_code", "client_credentials"],
            "token_endpoint_auth_methods_supported": ["none", "client_secret_post"],
            "code_challenge_methods_supported": ["S256"],
            "client_id_metadata_document_supported": true,
            "authorization_response_iss_parameter_supported": true,
        });
        if !state.omit_authorization_issuer.load(Ordering::SeqCst) {
            metadata["issuer"] = serde_json::json!(state.base_url);
        }
        return Ok(Response::builder()
            .header("Content-Type", "application/json")
            .body(full_body(metadata.to_string()))
            .unwrap());
    }
    if method == Method::GET && path.ends_with("/.well-known/openid-configuration") {
        if state.disable_authorization_metadata.load(Ordering::SeqCst) {
            return Ok(Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(empty_body())
                .unwrap());
        }
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
        let valid_grant = form.get("grant_type").map(String::as_str) == Some("authorization_code")
            && form.get("code").map(String::as_str) == Some("authorization-code")
            && form
                .get("code_verifier")
                .is_some_and(|value| !value.is_empty())
            && !form.contains_key("client_secret");
        let valid_client_id = form.get("client_id").map(String::as_str) == Some("oauth-dcr-client");
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
            if request_index > 0 {
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
    _mode: OAuthFixtureMode,
    challenge_tools_write_count: usize,
    token_expires_in: u64,
    mcp_response_sse: bool,
) -> (u16, Arc<OAuthMockState>) {
    let (listener, port) = bind_random().await;
    let base_url = format!("http://127.0.0.1:{port}");
    let state = Arc::new(OAuthMockState {
        resource: StdMutex::new(format!("{base_url}/mcp")),
        base_url,
        mcp_response_sse,
        token_expires_in,
        discovery_response_delay_ms: AtomicU64::new(0),
        registration_response_delay_ms: AtomicU64::new(0),
        token_response_delay_ms: AtomicU64::new(0),
        disable_authorization_metadata: AtomicBool::new(false),
        omit_authorization_issuer: AtomicBool::new(false),
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
        anonymous_initialize_requests: AtomicUsize::new(0),
        authorized_initialize_requests: AtomicUsize::new(0),
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

async fn assert_oauth_callback_not_configured(manager: &MCPServerManager, bundle_id: &BundleId) {
    assert!(matches!(
        manager
            .complete_oauth(
                bundle_id,
                OAuthCallback {
                    code: "unexpected-code".to_string(),
                    state: "unexpected-state".to_string(),
                    issuer: None,
                },
            )
            .await,
        Err(OAuthError::NotConfigured)
    ));
    assert!(matches!(
        manager
            .cancel_oauth(
                bundle_id,
                OAuthCancellation {
                    state: "unexpected-state".to_string(),
                    issuer: None,
                    reason: OAuthCancellationReason::Cancelled,
                },
            )
            .await,
        Err(OAuthError::NotConfigured)
    ));
}

#[tokio::test]
async fn test_public_http_manager_connects_without_creating_oauth_state() {
    let (port, _state) = spawn_http_mock(false).await;
    let bundle_id = BundleId::try_from("public-http-auto").unwrap();
    let manager = MCPServerManager::new();
    let mut config = HttpServerConfig::new(
        "public-http-auto",
        HttpServerParameters {
            url: format!("http://127.0.0.1:{port}/mcp"),
            headers: HashMap::new(),
        },
    );
    config.bundle_id = Some(bundle_id.clone());
    manager
        .add_or_update_server(MCPServerConfig::Http(config))
        .await
        .unwrap();

    assert_oauth_callback_not_configured(&manager, &bundle_id).await;
    manager.start_client_by_id(&bundle_id).await.unwrap();
    assert_oauth_callback_not_configured(&manager, &bundle_id).await;
    assert!(matches!(
        manager.oauth_status(&bundle_id).await,
        Err(OAuthError::NotConfigured)
    ));
    assert!(matches!(
        manager
            .create_oauth_flow(
                &bundle_id,
                OAuthBeginRequest {
                    redirect_uri: "https://callback.example.test/oauth/callback".to_string(),
                    required_scope: None,
                }
            )
            .await,
        Err(OAuthError::NotConfigured)
    ));
    manager.close().await.unwrap();
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

async fn auto_auth_gate_result(
    mode: AuthGateMode,
    static_authorization: bool,
) -> (HttpAuthenticationError, Arc<AuthGateState>) {
    let (port, state) = spawn_auth_gate(mode).await;
    let bundle_id = BundleId::try_from(format!("auto-auth-{port}")).unwrap();
    let headers = if static_authorization {
        HashMap::from([(
            "Authorization".to_string(),
            "Bearer static-token".to_string(),
        )])
    } else {
        HashMap::new()
    };
    let mut config = HttpServerConfig::new(
        "auto-auth-gate",
        HttpServerParameters {
            url: format!("http://127.0.0.1:{port}/mcp"),
            headers,
        },
    );
    config.bundle_id = Some(bundle_id.clone());
    let manager = MCPServerManager::new();
    manager
        .add_or_update_server(MCPServerConfig::Http(config))
        .await
        .unwrap();
    let error = manager.start_client_by_id(&bundle_id).await.unwrap_err();
    assert_oauth_callback_not_configured(&manager, &bundle_id).await;
    let ComputerError::HttpAuthentication(error) = error else {
        panic!("expected structured HTTP authentication error, got {error:?}");
    };
    (error, state)
}

#[tokio::test]
async fn test_auto_http_auth_distinguishes_non_oauth_failures() {
    let cases = [
        (
            AuthGateMode::BareUnauthorized,
            HttpAuthenticationError::Unauthorized,
        ),
        (
            AuthGateMode::Basic,
            HttpAuthenticationError::UnsupportedChallenge,
        ),
        (
            AuthGateMode::BearerWithoutMetadata,
            HttpAuthenticationError::OAuthDiscoveryFailed,
        ),
        (AuthGateMode::Forbidden, HttpAuthenticationError::Forbidden),
    ];
    for (mode, expected) in cases {
        let (actual, state) = auto_auth_gate_result(mode, false).await;
        assert_eq!(actual, expected);
        assert_eq!(state.metadata_requests.load(Ordering::SeqCst), 0);
    }

    let (error, state) =
        auto_auth_gate_result(AuthGateMode::BearerWithInvalidMetadata, false).await;
    assert_eq!(error, HttpAuthenticationError::OAuthDiscoveryFailed);
    assert!(state.metadata_requests.load(Ordering::SeqCst) > 0);
}

#[tokio::test]
async fn test_static_authorization_rejection_never_falls_back_to_oauth() {
    let (error, state) = auto_auth_gate_result(AuthGateMode::StaticBearerRejected, true).await;
    assert_eq!(error, HttpAuthenticationError::StaticCredentialsRejected);
    assert!(state.saw_static_authorization.load(Ordering::SeqCst));
    assert_eq!(state.metadata_requests.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn test_auto_oauth_admits_only_after_validated_discovery_and_reuses_cancellation() {
    let (port, state) = spawn_oauth_http_mock(OAuthFixtureMode::Dynamic, 0).await;
    let bundle_id = BundleId::try_from("oauth-auto-discovery").unwrap();
    let manager = MCPServerManager::new();
    let mut config = HttpServerConfig::new(
        "oauth-auto-discovery",
        HttpServerParameters {
            url: format!("http://127.0.0.1:{port}/mcp"),
            headers: HashMap::new(),
        },
    );
    config.bundle_id = Some(bundle_id.clone());
    manager
        .add_or_update_server(MCPServerConfig::Http(config))
        .await
        .unwrap();

    assert!(matches!(
        manager.oauth_status(&bundle_id).await,
        Err(OAuthError::NotConfigured)
    ));
    assert!(matches!(
        manager.start_client_by_id(&bundle_id).await,
        Err(ComputerError::HttpAuthentication(
            HttpAuthenticationError::OAuthRequired
        ))
    ));
    assert_eq!(
        manager.oauth_status(&bundle_id).await.unwrap(),
        OAuthStatus::Unauthorized
    );
    assert!(state.discovery_requests.load(Ordering::SeqCst) > 0);
    assert_eq!(state.registration_requests.load(Ordering::SeqCst), 0);
    assert_eq!(state.token_requests.load(Ordering::SeqCst), 0);
    assert_eq!(
        state.anonymous_initialize_requests.load(Ordering::SeqCst),
        1
    );
    assert_eq!(
        state.authorized_initialize_requests.load(Ordering::SeqCst),
        0
    );

    let flow = manager
        .create_oauth_flow(
            &bundle_id,
            OAuthBeginRequest {
                redirect_uri: "https://callback.example.test/oauth/callback".to_string(),
                required_scope: None,
            },
        )
        .await
        .unwrap();
    let launch = flow.launch().await.unwrap();
    assert_eq!(state.registration_requests.load(Ordering::SeqCst), 1);
    let outcome = manager
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
    assert!(matches!(
        outcome,
        OAuthFlowOutcome::Terminated {
            reason: OAuthCancellationReason::Cancelled,
            status: OAuthStatus::Unauthorized
        }
    ));
    manager.clear_oauth(&bundle_id).await.unwrap();
}

#[tokio::test]
async fn test_interactive_flow_revalidates_automatic_admission_metadata() {
    let (manager, bundle_id, state) =
        authorization_code_manager(OAuthFixtureMode::Dynamic, false).await;
    state
        .omit_authorization_issuer
        .store(true, Ordering::SeqCst);

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
    assert!(matches!(
        flow.launch().await,
        Err(OAuthError::Protocol(OAuthProtocolError::Metadata))
    ));
    assert_eq!(state.registration_requests.load(Ordering::SeqCst), 0);

    let (manager, bundle_id, state) =
        authorization_code_manager(OAuthFixtureMode::Dynamic, false).await;
    state.set_resource(format!("{}/different-resource", state.base_url));
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
    assert!(matches!(
        flow.launch().await,
        Err(OAuthError::Protocol(OAuthProtocolError::Metadata))
    ));
    assert_eq!(state.registration_requests.load(Ordering::SeqCst), 0);
}

async fn configure_automatic_manager_without_start(
    base_url: &str,
    bundle_id: BundleId,
    store: Arc<dyn OAuthCredentialStore>,
) -> MCPServerManager {
    let manager = MCPServerManager::with_oauth_credential_store(store);
    manager
        .add_or_update_server(authorization_code_server_config(base_url, bundle_id))
        .await
        .unwrap();
    manager
}

#[tokio::test]
async fn test_automatic_oauth_restores_and_starts_in_one_call_after_manager_rebuild() {
    let (_port, state) = spawn_oauth_http_mock(OAuthFixtureMode::Dynamic, 0).await;
    let store: Arc<dyn OAuthCredentialStore> = Arc::new(InMemoryOAuthCredentialStore::default());
    let bundle_id = BundleId::try_from("oauth-auto-persistent-rebuild").unwrap();
    let first = configure_authorization_code_manager(
        &state.base_url,
        bundle_id.clone(),
        Arc::clone(&store),
    )
    .await;
    authorize_manager(&first, &bundle_id, &state, None).await;
    first.close().await.unwrap();
    drop(first);

    let anonymous_before = state.anonymous_initialize_requests.load(Ordering::SeqCst);
    let authorized_before = state.authorized_initialize_requests.load(Ordering::SeqCst);
    let restored =
        configure_automatic_manager_without_start(&state.base_url, bundle_id.clone(), store).await;
    restored.start_client_by_id(&bundle_id).await.unwrap();

    assert!(matches!(
        restored.oauth_status(&bundle_id).await.unwrap(),
        OAuthStatus::Authorized { .. }
    ));
    assert_eq!(
        state.anonymous_initialize_requests.load(Ordering::SeqCst),
        anonymous_before + 1
    );
    assert_eq!(
        state.authorized_initialize_requests.load(Ordering::SeqCst),
        authorized_before + 1
    );
    assert!(restored.get_server_status().await[0].2);
    restored.close().await.unwrap();
}

#[tokio::test]
async fn test_automatic_oauth_rejected_persisted_token_requires_user_in_one_start() {
    let (_port, state) = spawn_oauth_http_mock(OAuthFixtureMode::Dynamic, 0).await;
    let store: Arc<dyn OAuthCredentialStore> = Arc::new(InMemoryOAuthCredentialStore::default());
    let bundle_id = BundleId::try_from("oauth-auto-rejected-persisted").unwrap();
    let first = configure_authorization_code_manager(
        &state.base_url,
        bundle_id.clone(),
        Arc::clone(&store),
    )
    .await;
    authorize_manager(&first, &bundle_id, &state, None).await;
    first.close().await.unwrap();
    drop(first);

    state.reject_authorized_remaining.store(1, Ordering::SeqCst);
    let restored =
        configure_automatic_manager_without_start(&state.base_url, bundle_id.clone(), store).await;
    assert!(matches!(
        restored.start_client_by_id(&bundle_id).await,
        Err(ComputerError::HttpAuthentication(
            HttpAuthenticationError::OAuthRequired
        ))
    ));
    assert_eq!(
        restored.oauth_status(&bundle_id).await.unwrap(),
        OAuthStatus::Unauthorized
    );
    let runtime = restored.get_server_runtime_statuses().await;
    assert_eq!(runtime.len(), 1);
    assert_eq!(
        runtime[0].activation,
        MCPServerActivationState::Started,
        "an accepted start must survive the OAuth challenge"
    );
    assert_eq!(
        runtime[0].connection,
        MCPServerConnectionState::AuthorizationRequired
    );
    assert!(restored.get_server_status().await[0].2);
    assert_eq!(
        restored.get_server_status().await[0].3,
        "authorization_required"
    );

    assert!(restored.stop_client_by_id(&bundle_id).await.unwrap());
    let stopped = restored.get_server_runtime_statuses().await;
    assert_eq!(stopped[0].activation, MCPServerActivationState::Stopped);
    assert_eq!(
        stopped[0].connection,
        MCPServerConnectionState::Disconnected
    );
    assert!(!restored.stop_client_by_id(&bundle_id).await.unwrap());
    restored.close().await.unwrap();
}

#[tokio::test]
async fn test_empty_scopes_adopt_prm_scopes_supported_for_initial_authorization() {
    // Issue #176: automatic negotiation has no configured scopes, so the authorization flow must
    // adopt the scope published via Protected Resource Metadata (RFC 9728) instead of requesting
    // no scope at all. Without this, providers that publish scopes via PRM (e.g. Atlassian's 22
    // scopes) grant a token lacking business scopes, so business tools return
    // `401 scope does not match` while basic tools still work.
    let (port, state) = spawn_oauth_http_mock(OAuthFixtureMode::Dynamic, 0).await;
    let bundle_id = BundleId::try_from("oauth-empty-scopes").unwrap();
    let manager = MCPServerManager::new();
    let mut config = HttpServerConfig::new(
        "oauth-empty-scopes",
        HttpServerParameters {
            url: format!("http://127.0.0.1:{port}/mcp"),
            headers: HashMap::new(),
        },
    );
    config.bundle_id = Some(bundle_id.clone());
    manager
        .add_or_update_server(MCPServerConfig::Http(config))
        .await
        .unwrap();
    assert!(matches!(
        manager.start_client_by_id(&bundle_id).await,
        Err(ComputerError::HttpAuthentication(
            HttpAuthenticationError::OAuthRequired
        ))
    ));

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

    // The authorization URL must request the PRM-published scope. Under the bug, the `scope`
    // query parameter is absent entirely because `self.options.scopes` (empty) was used directly.
    let authorization_url = url::Url::parse(&launch.authorization_url).unwrap();
    let query: HashMap<String, String> = authorization_url.query_pairs().into_owned().collect();
    assert_eq!(
        query.get("scope").map(String::as_str),
        Some("tools.read"),
        "empty scope config must adopt the PRM-published scope, not request no scope"
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
        panic!("expected authorized outcome, got {outcome:?}");
    };
    assert!(
        scopes.iter().any(|scope| scope == "tools.read"),
        "granted scopes must include the PRM-published scope: {scopes:?}"
    );
}

async fn authorization_code_manager(
    _mode: OAuthFixtureMode,
    challenge_tools_write_once: bool,
) -> (MCPServerManager, BundleId, Arc<OAuthMockState>) {
    let (port, state) = spawn_oauth_http_mock(
        OAuthFixtureMode::Dynamic,
        usize::from(challenge_tools_write_once),
    )
    .await;
    let bundle_id = BundleId::try_from("oauth-code").unwrap();
    let manager = MCPServerManager::new();
    let mut config = HttpServerConfig::new(
        "oauth-code",
        HttpServerParameters {
            url: format!("http://127.0.0.1:{port}/mcp"),
            headers: HashMap::new(),
        },
    );
    config.bundle_id = Some(bundle_id.clone());
    manager
        .add_or_update_server(MCPServerConfig::Http(config))
        .await
        .unwrap();
    assert!(matches!(
        manager.start_client_by_id(&bundle_id).await,
        Err(ComputerError::HttpAuthentication(
            HttpAuthenticationError::OAuthRequired
        ))
    ));
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
    config.bundle_id = Some(bundle_id.clone());
    manager
        .add_or_update_server(MCPServerConfig::Http(config))
        .await
        .unwrap();
    match manager.start_client_by_id(&bundle_id).await {
        Ok(()) | Err(ComputerError::HttpAuthentication(HttpAuthenticationError::OAuthRequired)) => {
        }
        Err(error) => panic!("automatic OAuth admission failed for {bundle_id}: {error:?}"),
    }
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
    for bundle_id in bundle_ids {
        match computer.start_mcp_client(bundle_id).await {
            Ok(())
            | Err(ComputerError::HttpAuthentication(HttpAuthenticationError::OAuthRequired)) => {}
            Err(error) => panic!("automatic OAuth admission failed for {bundle_id}: {error:?}"),
        }
    }
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
    if matches!(
        manager.oauth_status(bundle_id).await,
        Err(OAuthError::NotConfigured)
    ) {
        assert!(matches!(
            manager.start_client_by_id(bundle_id).await,
            Err(ComputerError::HttpAuthentication(
                HttpAuthenticationError::OAuthRequired
            ))
        ));
    }
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
        authorization_code_manager(OAuthFixtureMode::Dynamic, false).await;
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
async fn test_streamable_http_automatic_dcr_end_to_end() {
    let (manager, bundle_id, state) =
        authorization_code_manager(OAuthFixtureMode::Dynamic, false).await;
    let client_id = authorize_manager(&manager, &bundle_id, &state, None).await;
    assert_eq!(client_id, "oauth-dcr-client");
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
    assert_eq!(state.registration_requests.load(Ordering::SeqCst), 1);
    manager.close().await.unwrap();
}

#[tokio::test]
async fn test_computer_oauth_event_lag_resynchronizes_through_public_status_query() {
    let (_port, state) = spawn_oauth_http_mock(OAuthFixtureMode::Dynamic, 0).await;
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
    let (_port, state) = spawn_oauth_http_mock(OAuthFixtureMode::Dynamic, 1).await;
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
async fn test_authorization_code_oauth_covers_sse_responses_403_and_401() {
    let (port, state) = spawn_oauth_http_sse_mock(OAuthFixtureMode::Dynamic, 1).await;
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
        authorization_code_manager(OAuthFixtureMode::Dynamic, false).await;
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
    let runtime = manager.get_server_runtime_statuses().await;
    assert_eq!(runtime[0].activation, MCPServerActivationState::Started);
    assert_eq!(
        runtime[0].connection,
        MCPServerConnectionState::AuthorizationRequired
    );
    assert_tool_call_is_blocked_before_http(&manager, &bundle_id, &state).await;

    authorize_manager(&manager, &bundle_id, &state, None).await;
    manager.start_client_by_id(&bundle_id).await.unwrap();
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
async fn test_computer_clear_oauth_commits_capability_event_and_joined_tool_list_update_once() {
    let (_port, state) = spawn_oauth_http_mock(OAuthFixtureMode::Dynamic, 0).await;
    let bundle_id = BundleId::try_from("oauth-clear-capability").unwrap();
    let (computer, _temp_dir) = configure_authorization_code_computer(
        &state.base_url,
        std::slice::from_ref(&bundle_id),
        Arc::new(InMemoryOAuthCredentialStore::default()),
    )
    .await;
    authorize_computer(&computer, &bundle_id, &state).await;
    computer.start_mcp_client(&bundle_id).await.unwrap();
    let tools_before = computer.get_available_tools().await.unwrap();
    assert!(tools_before
        .iter()
        .any(|tool| tool.name.ends_with("__echo")));

    let tool_list_updates = Arc::new(AtomicUsize::new(0));
    let (relay_url, relay_shutdown) =
        spawn_tool_list_recording_relay(Arc::clone(&tool_list_updates)).await;
    computer
        .connect_socketio(&relay_url, ConnectOptions::default())
        .await
        .unwrap();
    computer
        .join_office("oauth-clear-office", "oauth-store-computer")
        .await
        .unwrap();

    let mut events = computer.subscribe_events();
    let revision_before = computer.capability_revision();
    computer.clear_oauth(&bundle_id).await.unwrap();

    assert_eq!(
        events.recv().await.unwrap(),
        ComputerEvent::OAuthStatusChanged {
            bundle_id: bundle_id.clone(),
            status: OAuthStatus::Unauthorized,
        }
    );
    let capability_event = events.recv().await.unwrap();
    assert!(matches!(
        capability_event,
        ComputerEvent::CapabilityRevisionBumped { revision }
            if revision == revision_before + 1
    ));
    assert_eq!(computer.capability_revision(), revision_before + 1);
    let runtime = computer.get_server_runtime_statuses().await;
    let runtime = runtime
        .iter()
        .find(|status| status.bundle_id == bundle_id)
        .unwrap();
    assert_eq!(runtime.activation, MCPServerActivationState::Started);
    assert_eq!(
        runtime.connection,
        MCPServerConnectionState::AuthorizationRequired
    );
    assert_eq!(
        computer.oauth_status(&bundle_id).await.unwrap(),
        OAuthStatus::Unauthorized
    );
    assert!(computer.get_available_tools().await.unwrap().is_empty());
    assert!(matches!(
        computer.get_resources(bundle_id.as_str(), None).await,
        Err(ComputerError::McpServerNotFound(ref id)) if id == bundle_id.as_str()
    ));
    tokio::time::timeout(Duration::from_secs(2), async {
        while tool_list_updates.load(Ordering::SeqCst) != 1 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("clear_oauth did not emit server:update_tool_list");

    let revision_after_first = computer.capability_revision();
    computer.clear_oauth(&bundle_id).await.unwrap();
    assert_eq!(computer.capability_revision(), revision_after_first);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), events.recv())
            .await
            .is_err()
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(tool_list_updates.load(Ordering::SeqCst), 1);

    assert!(computer.stop_mcp_client(&bundle_id).await.unwrap());
    let runtime = computer.get_server_runtime_statuses().await;
    let runtime = runtime
        .iter()
        .find(|status| status.bundle_id == bundle_id)
        .unwrap();
    assert_eq!(runtime.activation, MCPServerActivationState::Stopped);
    assert_eq!(runtime.connection, MCPServerConnectionState::Disconnected);

    computer.shutdown().await.unwrap();
    let _ = relay_shutdown.send(());
}

#[tokio::test]
async fn test_computer_clear_oauth_store_failure_does_not_publish_capability_change() {
    let (_port, state) = spawn_oauth_http_mock(OAuthFixtureMode::Dynamic, 0).await;
    let bundle_id = BundleId::try_from("oauth-clear-store-failure").unwrap();
    let store = Arc::new(RecordingOAuthCredentialStore::default());
    let (computer, _temp_dir) = configure_authorization_code_computer(
        &state.base_url,
        std::slice::from_ref(&bundle_id),
        store.clone(),
    )
    .await;
    authorize_computer(&computer, &bundle_id, &state).await;
    computer.start_mcp_client(&bundle_id).await.unwrap();
    let tool_list_updates = Arc::new(AtomicUsize::new(0));
    let (relay_url, relay_shutdown) =
        spawn_tool_list_recording_relay(Arc::clone(&tool_list_updates)).await;
    computer
        .connect_socketio(&relay_url, ConnectOptions::default())
        .await
        .unwrap();
    computer
        .join_office("oauth-clear-failure-office", "oauth-store-computer")
        .await
        .unwrap();
    let mut events = computer.subscribe_events();
    let revision_before = computer.capability_revision();
    store
        .fail_next_credential_delete
        .store(true, Ordering::SeqCst);

    assert!(computer.clear_oauth(&bundle_id).await.is_err());
    assert_eq!(computer.capability_revision(), revision_before);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), events.recv())
            .await
            .is_err()
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(tool_list_updates.load(Ordering::SeqCst), 0);
    let runtime = computer.get_server_runtime_statuses().await;
    let runtime = runtime
        .iter()
        .find(|status| status.bundle_id == bundle_id)
        .unwrap();
    assert_eq!(runtime.activation, MCPServerActivationState::Started);
    assert_eq!(runtime.connection, MCPServerConnectionState::Connected);
    assert!(matches!(
        computer.oauth_status(&bundle_id).await.unwrap(),
        OAuthStatus::Authorized { .. }
    ));
    assert!(!computer.get_available_tools().await.unwrap().is_empty());

    computer.shutdown().await.unwrap();
    let _ = relay_shutdown.send(());
}

#[tokio::test]
async fn test_authorization_code_cancellation_validates_callback_and_cleans_pending_state() {
    let (manager, bundle_id, state) =
        authorization_code_manager(OAuthFixtureMode::Dynamic, false).await;
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
        authorization_code_manager(OAuthFixtureMode::Dynamic, false).await;
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
        authorization_code_manager(OAuthFixtureMode::Dynamic, false).await;
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
    let (port, state) = spawn_oauth_http_mock(OAuthFixtureMode::Dynamic, 0).await;
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
async fn test_cold_start_prelaunch_cancel_restores_persisted_authorized_status() {
    let (port, state) = spawn_oauth_http_mock(OAuthFixtureMode::Dynamic, 0).await;
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
        authorization_code_manager(OAuthFixtureMode::Dynamic, false).await;
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
        authorization_code_manager(OAuthFixtureMode::Dynamic, false).await;
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
    let (port, state) = spawn_oauth_http_mock(OAuthFixtureMode::Dynamic, 0).await;
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
    let replaced = manager.get_server_runtime_statuses().await;
    assert_eq!(replaced[0].activation, MCPServerActivationState::Started);
    assert_eq!(
        replaced[0].connection,
        MCPServerConnectionState::Disconnected,
        "replacement retires the old OAuth data plane without losing activation intent"
    );
    assert!(matches!(
        first.cancel(OAuthCancellationReason::Cancelled).await,
        Ok(OAuthFlowOutcome::Terminated { .. })
    ));
    state.discovery_response_delay_ms.store(0, Ordering::SeqCst);
    assert!(matches!(
        manager.start_client_by_id(&bundle_id).await,
        Err(ComputerError::HttpAuthentication(
            HttpAuthenticationError::OAuthRequired
        ))
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
    let (port, state) = spawn_oauth_http_mock(OAuthFixtureMode::Dynamic, 0).await;
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
        authorization_code_manager(OAuthFixtureMode::Dynamic, false).await;
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
    let (port, state) = spawn_oauth_http_mock(OAuthFixtureMode::Dynamic, 0).await;
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
        Err(OAuthError::NotConfigured)
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
    let (port, state) = spawn_oauth_http_mock(OAuthFixtureMode::Dynamic, 0).await;
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
async fn test_computer_shutdown_preempts_compat_complete_during_exchange() {
    let (port, state) = spawn_oauth_http_mock(OAuthFixtureMode::Dynamic, 0).await;
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
        authorization_code_manager(OAuthFixtureMode::Dynamic, false).await;
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
        authorization_code_manager(OAuthFixtureMode::Dynamic, false).await;
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
    let (_port, state) = spawn_oauth_http_mock(OAuthFixtureMode::Dynamic, 0).await;
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
    let (_port, state) = spawn_oauth_http_mock(OAuthFixtureMode::Dynamic, 0).await;
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
    let (_port, state) = spawn_oauth_http_mock(OAuthFixtureMode::Dynamic, 0).await;
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
    let (_port, state) = spawn_oauth_http_mock(OAuthFixtureMode::Dynamic, 0).await;
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
    let (_port, state) = spawn_oauth_http_mock(OAuthFixtureMode::Dynamic, 0).await;
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
    assert!(matches!(
        restored.oauth_status(&bundle_id).await.unwrap(),
        OAuthStatus::Authorized { .. }
    ));
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
    let (_port, state) = spawn_oauth_http_mock(OAuthFixtureMode::Dynamic, 0).await;
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
        authorization_code_manager(OAuthFixtureMode::Dynamic, true).await;
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
