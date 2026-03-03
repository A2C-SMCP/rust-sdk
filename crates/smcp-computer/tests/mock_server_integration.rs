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
}

async fn http_mock_handler(
    req: Request<hyper::body::Incoming>,
    state: Arc<MockHttpState>,
) -> Result<Response<BoxBody>, Infallible> {
    // Record headers for assertion
    {
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
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, service)
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
    match &result.content[0] {
        Content::Text { text } => assert_eq!(text, "hello"),
        _ => panic!("expected text content"),
    }

    // disconnect
    client.disconnect().await.unwrap();
    assert_eq!(client.state(), ClientState::Disconnected);
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
    match &result.content[0] {
        Content::Text { text } => assert_eq!(text, "7"),
        _ => panic!("expected text content"),
    }

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
    match &result.content[0] {
        Content::Text { text } => assert_eq!(text, "sse-test"),
        _ => panic!("expected text content"),
    }

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
    match &result.content[0] {
        Content::Text { text } => assert_eq!(text, "30"),
        _ => panic!("expected text content"),
    }

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
