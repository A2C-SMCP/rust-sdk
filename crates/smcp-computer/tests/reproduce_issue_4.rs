/**
 * Issue #4 回归测试: SSE/HTTP MCP 客户端 JSON-RPC 请求不符合规范
 * Issue #4 regression test: SSE/HTTP MCP client JSON-RPC requests not spec-compliant
 *
 * 验证两个修复点:
 * 1. notification 不应携带 `id` 字段 (JSON-RPC 2.0 规范)
 * 2. `tools/list` 等方法应传递空 params `{}`
 *
 * 使用内置的严格 mock server，无需外部依赖。
 * Uses a built-in strict mock server, no external dependencies needed.
 *
 * 运行: cargo test --package smcp-computer --test reproduce_issue_4 -- --nocapture
 */
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;

use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Bytes, Frame};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use smcp_computer::mcp_clients::http_client::HttpMCPClient;
use smcp_computer::mcp_clients::model::*;
use smcp_computer::mcp_clients::sse_client::SseMCPClient;

// ============================================================
// BoxBody helper
// ============================================================
type BoxBody = http_body_util::combinators::BoxBody<Bytes, Infallible>;

fn full_body(s: impl Into<Bytes>) -> BoxBody {
    Full::new(s.into()).map_err(|never| match never {}).boxed()
}

fn empty_body() -> BoxBody {
    full_body(Bytes::new())
}

async fn bind_random() -> (TcpListener, u16) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    (listener, port)
}

// ============================================================
// JSON-RPC helpers
// ============================================================
fn jsonrpc_response(id: &serde_json::Value, result: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result
    })
}

fn jsonrpc_error(id: &serde_json::Value, code: i64, message: &str) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}

fn initialize_result() -> serde_json::Value {
    serde_json::json!({
        "protocolVersion": "2024-11-05",
        "serverInfo": { "name": "strict-mock", "version": "0.1.0" },
        "capabilities": { "tools": {} }
    })
}

fn tools_list_result() -> serde_json::Value {
    serde_json::json!({
        "tools": [{
            "name": "test_tool",
            "description": "A test tool",
            "inputSchema": { "type": "object", "properties": {} }
        }]
    })
}

// ============================================================
// Strict SSE Mock Server
//
// 模拟真实 MCP SDK (如 Python) 的严格行为:
// - notification 带 `id` → 返回 -32602 错误响应
// - 这个错误响应会污染 SSE response channel，导致后续请求响应错位
// ============================================================
type SseFrameSender = tokio::sync::mpsc::UnboundedSender<Result<Frame<Bytes>, Infallible>>;

/// 记录 mock server 收到的请求，用于断言
#[derive(Debug, Clone)]
struct RecordedRequest {
    method: String,
    has_id: bool,
    has_params: bool,
}

struct StrictSseState {
    sse_tx: Mutex<Option<SseFrameSender>>,
    /// 记录收到的所有请求 / Record all received requests
    requests: Mutex<Vec<RecordedRequest>>,
}

async fn strict_sse_handler(
    req: Request<hyper::body::Incoming>,
    state: Arc<StrictSseState>,
) -> Result<Response<BoxBody>, Infallible> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    if method == Method::GET && path == "/sse" {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Result<Frame<Bytes>, Infallible>>();

        let endpoint_event = "event: endpoint\ndata: /messages\n\n";
        let _ = tx.send(Ok(Frame::data(Bytes::from(endpoint_event))));

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

        let rpc_method = body
            .get("method")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string();
        let has_id = body.get("id").is_some();
        let has_params = body.get("params").is_some();

        // 记录请求 / Record request
        state.requests.lock().await.push(RecordedRequest {
            method: rpc_method.clone(),
            has_id,
            has_params,
        });

        let is_notification = rpc_method.starts_with("notifications/");

        // 严格行为: notification 带 id → 返回 JSON-RPC 错误
        // Strict behavior: notification with id → return JSON-RPC error
        // 这模拟了 Python MCP SDK 的真实行为
        if is_notification && has_id {
            let id = body.get("id").cloned().unwrap_or(serde_json::json!(null));
            let error_resp = jsonrpc_error(&id, -32602, "Notification should not have an id");
            // 将错误通过 SSE 推送 — 这会污染 response channel
            let sse_event = format!(
                "event: message\ndata: {}\n\n",
                serde_json::to_string(&error_resp).unwrap()
            );
            if let Some(ref tx) = *state.sse_tx.lock().await {
                let _ = tx.send(Ok(Frame::data(Bytes::from(sse_event))));
            }
            return Ok(Response::builder()
                .status(StatusCode::ACCEPTED)
                .body(empty_body())
                .unwrap());
        }

        // 正常 notification → 202 无响应体
        if is_notification {
            return Ok(Response::builder()
                .status(StatusCode::ACCEPTED)
                .body(empty_body())
                .unwrap());
        }

        let id = body.get("id").cloned().unwrap_or(serde_json::json!(0));

        let result = match rpc_method.as_str() {
            "initialize" => initialize_result(),
            "tools/list" => tools_list_result(),
            "shutdown" | "exit" => serde_json::json!({}),
            _ => serde_json::json!({}),
        };

        let resp_json = jsonrpc_response(&id, result);
        let sse_event = format!(
            "event: message\ndata: {}\n\n",
            serde_json::to_string(&resp_json).unwrap()
        );
        if let Some(ref tx) = *state.sse_tx.lock().await {
            let _ = tx.send(Ok(Frame::data(Bytes::from(sse_event))));
        }

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

async fn spawn_strict_sse_mock() -> (u16, Arc<StrictSseState>) {
    let (listener, port) = bind_random().await;
    let state = Arc::new(StrictSseState {
        sse_tx: Mutex::new(None),
        requests: Mutex::new(Vec::new()),
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
                    async move { strict_sse_handler(req, st).await }
                });
                let mut builder = hyper::server::conn::http1::Builder::new();
                builder.keep_alive(false);
                let _ = builder.serve_connection(io, service).await;
            });
        }
    });

    (port, state)
}

// ============================================================
// Strict HTTP Mock Server (同样的严格行为)
// ============================================================
struct StrictHttpState {
    requests: Mutex<Vec<RecordedRequest>>,
}

async fn strict_http_handler(
    req: Request<hyper::body::Incoming>,
    state: Arc<StrictHttpState>,
) -> Result<Response<BoxBody>, Infallible> {
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

    let rpc_method = body
        .get("method")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    let has_id = body.get("id").is_some();
    let has_params = body.get("params").is_some();

    state.requests.lock().await.push(RecordedRequest {
        method: rpc_method.clone(),
        has_id,
        has_params,
    });

    let is_notification = rpc_method.starts_with("notifications/");

    // 严格: notification 带 id → 返回错误
    if is_notification && has_id {
        let id = body.get("id").cloned().unwrap_or(serde_json::json!(null));
        let error_resp = jsonrpc_error(&id, -32602, "Notification should not have an id");
        let resp_bytes = serde_json::to_vec(&error_resp).unwrap();
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/json")
            .body(full_body(resp_bytes))
            .unwrap());
    }

    // 正常 notification → 202
    if is_notification {
        return Ok(Response::builder()
            .status(StatusCode::ACCEPTED)
            .body(empty_body())
            .unwrap());
    }

    let id = body.get("id").cloned().unwrap_or(serde_json::json!(0));

    let result = match rpc_method.as_str() {
        "initialize" => initialize_result(),
        "tools/list" => tools_list_result(),
        "shutdown" | "exit" => serde_json::json!({}),
        _ => serde_json::json!({}),
    };

    let resp_json = jsonrpc_response(&id, result);
    let resp_bytes = serde_json::to_vec(&resp_json).unwrap();

    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json");
    if rpc_method == "initialize" {
        builder = builder.header("mcp-session-id", "strict-session-001");
    }
    Ok(builder.body(full_body(resp_bytes)).unwrap())
}

async fn spawn_strict_http_mock() -> (u16, Arc<StrictHttpState>) {
    let (listener, port) = bind_random().await;
    let state = Arc::new(StrictHttpState {
        requests: Mutex::new(Vec::new()),
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
                    async move { strict_http_handler(req, st).await }
                });
                let mut builder = hyper::server::conn::http1::Builder::new();
                builder.keep_alive(false);
                let _ = builder.serve_connection(io, service).await;
            });
        }
    });

    (port, state)
}

// ============================================================
// Tests
// ============================================================

/// Issue #4 回归: SSE 客户端 notification 不应带 id，tools/list 应有 params
#[tokio::test]
async fn test_issue4_sse_notification_no_id_and_tools_list_has_params() {
    let (port, state) = spawn_strict_sse_mock().await;

    let client = SseMCPClient::new(SseServerParameters {
        url: format!("http://127.0.0.1:{}/sse", port),
        headers: HashMap::new(),
    });

    // connect (initialize + notifications/initialized)
    client.connect().await.expect("connect should succeed");
    assert_eq!(client.state(), ClientState::Connected);

    // list_tools — 如果 notification 带了 id，服务端的错误响应会导致这里拿到错误
    let tools = client
        .list_tools()
        .await
        .expect("tools/list should succeed (not receive -32602 error)");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "test_tool");

    client.disconnect().await.ok();

    // 验证请求记录 / Verify request records
    let requests = state.requests.lock().await;

    // notifications/initialized 不应有 id
    let notif = requests
        .iter()
        .find(|r| r.method == "notifications/initialized")
        .expect("should have sent notifications/initialized");
    assert!(
        !notif.has_id,
        "notifications/initialized must NOT have id field"
    );

    // tools/list 应有 params
    let tools_req = requests
        .iter()
        .find(|r| r.method == "tools/list")
        .expect("should have sent tools/list");
    assert!(
        tools_req.has_params,
        "tools/list should have params field (empty object)"
    );

    // initialize 应有 id
    let init = requests
        .iter()
        .find(|r| r.method == "initialize")
        .expect("should have sent initialize");
    assert!(init.has_id, "initialize must have id field");
}

/// Issue #4 回归: HTTP 客户端同样验证
#[tokio::test]
async fn test_issue4_http_notification_no_id_and_tools_list_has_params() {
    let (port, state) = spawn_strict_http_mock().await;

    let client = HttpMCPClient::new(HttpServerParameters {
        url: format!("http://127.0.0.1:{}", port),
        headers: HashMap::new(),
    });

    client.connect().await.expect("connect should succeed");
    assert_eq!(client.state(), ClientState::Connected);

    let tools = client
        .list_tools()
        .await
        .expect("tools/list should succeed");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "test_tool");

    client.disconnect().await.ok();

    let requests = state.requests.lock().await;

    let notif = requests
        .iter()
        .find(|r| r.method == "notifications/initialized")
        .expect("should have sent notifications/initialized");
    assert!(
        !notif.has_id,
        "notifications/initialized must NOT have id field"
    );

    let tools_req = requests
        .iter()
        .find(|r| r.method == "tools/list")
        .expect("should have sent tools/list");
    assert!(
        tools_req.has_params,
        "tools/list should have params field (empty object)"
    );
}
