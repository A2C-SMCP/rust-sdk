/**
* 文件名: auth_error_real_transport
* 作者: Claude Code
* 创建日期: 2026-07-18
* 描述: AUTH-01 真实传输可达性验证（protocol Discussion #34）
*
* 背景：python-sdk 工程师核实其 mcp SDK 的 streamable_http 在 `tools/call` 遇 401/403 时把异常抛进
* 传输任务组 → 拆连接、`call_tool` 挂起，反应式分类点生产不可达；疑 rust 同款。
*
* 本测试给出 rust 侧**真实传输**判据（非合成 `MCPClientError`）：握手放行、仅 `tools/call` 返
* 401/403，观察 `call_tool` 是 Err（可分类）还是挂起。结论与 python 不同：
* - rmcp **不挂起**，transport 错误经 WorkerTransport 同步回灌 pending 请求 → 调用方拿到 `Err`；
* - 403 / 裸 401 的 Display 携带状态码 → `classify_auth_error` 可达 4007 / 4006；
* - **但** 401 带 `WWW-Authenticate` 时 rmcp 短路成 `StreamableHttpError::AuthRequired`，
*   Display 为字面量 `"Auth required"`（`streamable_http_client.rs:57-58`），**不含**任何状态码/
*   判别子串 → 当前 `classify_auth_error` 返 `None` → 4006 漏报。而 RFC 6750/9728 要求受保护资源
*   在 401 上**必须**带 `WWW-Authenticate`，故这恰是 OAuth 生产最常见形态。
*/
use std::collections::HashMap;
use std::convert::Infallible;
use std::time::Duration;

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

use http_body_util::StreamBody;
use hyper::body::Frame;
use smcp::ErrorCode;
use smcp_computer::mcp_clients::auth_error::classify_auth_error;
use smcp_computer::mcp_clients::http_client::HttpMCPClient;
use smcp_computer::mcp_clients::model::*;
use smcp_computer::mcp_clients::sse_client::SseMCPClient;
use std::sync::Arc;
use tokio::sync::Mutex;

type BoxBody = http_body_util::combinators::BoxBody<Bytes, Infallible>;

fn full_body(s: impl Into<Bytes>) -> BoxBody {
    Full::new(s.into()).map_err(|never| match never {}).boxed()
}

fn empty_body() -> BoxBody {
    full_body(Bytes::new())
}

/// 握手放行、仅 `tools/call` 返指定状态码的 Streamable HTTP mock。
/// `with_www_authenticate` 控制是否带 `WWW-Authenticate`（rmcp 401 短路的触发条件）。
async fn auth_reject_handler(
    req: Request<hyper::body::Incoming>,
    reject_status: StatusCode,
    with_www_authenticate: bool,
) -> Result<Response<BoxBody>, Infallible> {
    if req.method() != Method::POST {
        return Ok(Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .body(empty_body())
            .unwrap());
    }

    let body_bytes = req.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value =
        serde_json::from_slice(&body_bytes).unwrap_or(serde_json::json!({}));
    let method = body.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let id = body.get("id").cloned().unwrap_or(serde_json::json!(0));

    if method.starts_with("notifications/") {
        return Ok(Response::builder()
            .status(StatusCode::ACCEPTED)
            .body(empty_body())
            .unwrap());
    }

    // 关键：仅 tools/call 被拒，握手/列表全放行 —— 复现「已连接但调用需授权」的生产形态。
    if method == "tools/call" {
        let mut builder = Response::builder()
            .status(reject_status)
            .header("Content-Type", "text/plain");
        if with_www_authenticate {
            builder = builder.header("WWW-Authenticate", "Bearer realm=\"mcp\"");
        }
        return Ok(builder
            .body(full_body(
                reject_status.canonical_reason().unwrap_or("denied"),
            ))
            .unwrap());
    }

    let result = match method {
        "initialize" => serde_json::json!({
            "protocolVersion": "2024-11-05",
            "serverInfo": { "name": "auth-mock", "version": "0.1.0" },
            "capabilities": { "tools": {} }
        }),
        "tools/list" => serde_json::json!({
            "tools": [{
                "name": "protected",
                "description": "Requires upstream auth",
                "inputSchema": { "type": "object" }
            }]
        }),
        _ => serde_json::json!({}),
    };

    let resp_json = serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result });
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json");
    if method == "initialize" {
        builder = builder.header("mcp-session-id", "auth-session-001");
    }
    Ok(builder
        .body(full_body(serde_json::to_vec(&resp_json).unwrap()))
        .unwrap())
}

async fn spawn_auth_reject_mock(reject_status: StatusCode, with_www_authenticate: bool) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let io = TokioIo::new(stream);
            tokio::spawn(async move {
                let service = service_fn(move |req| async move {
                    auth_reject_handler(req, reject_status, with_www_authenticate).await
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, service)
                    .await;
            });
        }
    });
    port
}

/// 驱动一次真实 401/403 tool_call，返回 `call_tool` 的**保型** `MCPClientError`。
/// time-box 区分「返回 Err」与「挂起」——python 侧正是挂在这里。
///
/// 直接返回类型化错误（而非 flatten 成 Display 串再重包 `ProtocolError(msg)`），让分类器走**真实构造
/// 路径**——#150 反假绿：测过滤器（字符串 marker）≠ 测数据源（rmcp 保型错误）。
async fn call_and_capture_err(
    reject_status: StatusCode,
    with_www_authenticate: bool,
) -> MCPClientError {
    let port = spawn_auth_reject_mock(reject_status, with_www_authenticate).await;
    let client = HttpMCPClient::new(HttpServerParameters {
        url: format!("http://127.0.0.1:{}", port),
        headers: HashMap::new(),
    });
    client.connect().await.expect("handshake must pass");

    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        client.call_tool("protected", serde_json::json!({})),
    )
    .await;

    match outcome {
        Err(_) => panic!(
            "HANG: call_tool did not resolve within 5s on upstream {} (python-style unreachable)",
            reject_status
        ),
        Ok(Ok(r)) => panic!("unexpected Ok result on {}: {:?}", reject_status, r),
        Ok(Err(e)) => e,
    }
}

/// 403 → Display 携 `403 Forbidden` → 分类 4007。真实传输下**可达**。
#[tokio::test]
async fn test_http_real_403_reaches_4007() {
    let e = call_and_capture_err(StatusCode::FORBIDDEN, false).await;
    let msg = e.to_string();
    println!("[HTTP 403] {}", msg);

    let lower = msg.to_lowercase();
    assert!(
        lower.contains("403") || lower.contains("forbidden"),
        "403 must carry status in Display, got: {}",
        msg
    );
    assert_eq!(
        classify_auth_error(&e),
        Some(ErrorCode::ToolAuthorizationFailed),
        "403 must classify as 4007, got none for: {}",
        msg
    );
}

/// 裸 401（无 `WWW-Authenticate`）→ 走 `error_for_status` → Display 携 `401 Unauthorized` → 4006 可达。
#[tokio::test]
async fn test_http_real_401_without_www_authenticate_reaches_4006() {
    let e = call_and_capture_err(StatusCode::UNAUTHORIZED, false).await;
    let msg = e.to_string();
    println!("[HTTP 401 no-WWW-Authenticate] {}", msg);

    let lower = msg.to_lowercase();
    assert!(
        lower.contains("401") || lower.contains("unauthorized"),
        "bare 401 must carry status in Display, got: {}",
        msg
    );
    assert_eq!(
        classify_auth_error(&e),
        Some(ErrorCode::ToolAuthorizationRequired),
        "bare 401 must classify as 4006, got none for: {}",
        msg
    );
}

/// 401 + `WWW-Authenticate`（RFC 6750/9728 要求的标准形态）→ rmcp 短路成
/// `AuthRequired`，Display 仅 `"Auth required"`，不含状态码。
///
/// #150 修复后：分类器对保型 `ToolCallError(ServiceError::TransportSend(..))` 做结构化 downcast
/// 命中 `AuthRequired` → 4006 可达（与裸 401 一致）。Display 见证（事实①②）保留为"为何字符串匹配失效"
/// 的文档——即令 fallback 字符串表永不命中此形态，结构化路径仍兜住。
#[tokio::test]
async fn test_http_real_401_with_www_authenticate_reaches_4006() {
    let e = call_and_capture_err(StatusCode::UNAUTHORIZED, true).await;
    let msg = e.to_string();
    println!("[HTTP 401 +WWW-Authenticate] {}", msg);

    // 事实①：不挂起——rmcp 把 transport 错误同步回灌调用方（与 python 挂起形成对比）。
    // 事实②：Display 为 rmcp 字面量 "Auth required"，**不含** 401/unauthorized（故字符串 fallback 永不命中）。
    assert!(
        msg.contains("Auth required"),
        "expected rmcp AuthRequired short-circuit, got: {}",
        msg
    );
    let lower = msg.to_lowercase();
    assert!(
        !lower.contains("401") && !lower.contains("unauthorized"),
        "status code unexpectedly present — gap may be fixed upstream, revisit: {}",
        msg
    );

    // 事实③（#150 修复后）：结构化 downcast 命中 AuthRequired → 4006 可达。
    assert_eq!(
        classify_auth_error(&e),
        Some(ErrorCode::ToolAuthorizationRequired),
        "401 + WWW-Authenticate must classify as 4006 via structured downcast, got: {}",
        msg
    );
}

// ============================================================
// SSE 传输（手写客户端，非 rmcp）——同一问题的第二条路径
// ============================================================

type SseFrameSender = tokio::sync::mpsc::UnboundedSender<Result<Frame<Bytes>, Infallible>>;

struct MockSseAuthState {
    sse_tx: Mutex<Option<SseFrameSender>>,
    reject_status: StatusCode,
}

/// GET /sse 放行并投 endpoint 事件；POST /messages 对 `tools/call` 返 401/403，其余放行。
async fn sse_auth_handler(
    req: Request<hyper::body::Incoming>,
    state: Arc<MockSseAuthState>,
) -> Result<Response<BoxBody>, Infallible> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    if method == Method::GET && path == "/sse" {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Result<Frame<Bytes>, Infallible>>();
        let _ = tx.send(Ok(Frame::data(Bytes::from("event: endpoint\ndata: /messages\n\n"))));
        *state.sse_tx.lock().await = Some(tx);
        let stream = futures_util::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        });
        let boxed: BoxBody = http_body_util::BodyExt::boxed(StreamBody::new(stream));
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "text/event-stream")
            .header("Cache-Control", "no-cache")
            .body(boxed)
            .unwrap());
    }

    if method == Method::POST && path == "/messages" {
        let body_bytes = req.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value =
            serde_json::from_slice(&body_bytes).unwrap_or(serde_json::json!({}));
        let rpc_method = body.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let id = body.get("id").cloned().unwrap_or(serde_json::json!(0));

        if rpc_method.starts_with("notifications/") {
            return Ok(Response::builder()
                .status(StatusCode::ACCEPTED)
                .body(empty_body())
                .unwrap());
        }

        // 仅 tools/call 被拒（带 WWW-Authenticate，与 HTTP 用例同形态）。
        if rpc_method == "tools/call" {
            return Ok(Response::builder()
                .status(state.reject_status)
                .header("WWW-Authenticate", "Bearer realm=\"mcp\"")
                .body(full_body(
                    state.reject_status.canonical_reason().unwrap_or("denied"),
                ))
                .unwrap());
        }

        let result = match rpc_method {
            "initialize" => serde_json::json!({
                "protocolVersion": "2024-11-05",
                "serverInfo": { "name": "auth-mock", "version": "0.1.0" },
                "capabilities": { "tools": {} }
            }),
            "tools/list" => serde_json::json!({ "tools": [] }),
            _ => serde_json::json!({}),
        };
        let resp_json = serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result });
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/json")
            .body(full_body(serde_json::to_vec(&resp_json).unwrap()))
            .unwrap());
    }

    Ok(Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(full_body("not found"))
        .unwrap())
}

async fn spawn_sse_auth_mock(reject_status: StatusCode) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let state = Arc::new(MockSseAuthState {
        sse_tx: Mutex::new(None),
        reject_status,
    });
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let io = TokioIo::new(stream);
            let st = state.clone();
            tokio::spawn(async move {
                let service = service_fn(move |req| {
                    let st = st.clone();
                    async move { sse_auth_handler(req, st).await }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, service)
                    .await;
            });
        }
    });
    port
}

async fn sse_call_and_capture_err(reject_status: StatusCode) -> MCPClientError {
    let port = spawn_sse_auth_mock(reject_status).await;
    let client = SseMCPClient::new(SseServerParameters {
        url: format!("http://127.0.0.1:{}/sse", port),
        headers: HashMap::new(),
    });
    client.connect().await.expect("SSE handshake must pass");

    let outcome = tokio::time::timeout(
        Duration::from_secs(10),
        client.call_tool("protected", serde_json::json!({})),
    )
    .await;

    match outcome {
        Err(_) => panic!("HANG: SSE call_tool did not resolve on upstream {}", reject_status),
        Ok(Ok(r)) => panic!("unexpected Ok on {}: {:?}", reject_status, r),
        Ok(Err(e)) => e,
    }
}

/// SSE（手写客户端）：401 即便带 `WWW-Authenticate` 也自建错误串携状态码 → 4006 可达。
/// 与 HTTP 传输的 rmcp 短路形成对照——同一部署形态下两传输**分类结果不一致**。
#[tokio::test]
async fn test_sse_real_401_reaches_4006() {
    let e = sse_call_and_capture_err(StatusCode::UNAUTHORIZED).await;
    let msg = e.to_string();
    println!("[SSE 401 +WWW-Authenticate] {}", msg);

    assert_eq!(
        classify_auth_error(&e),
        Some(ErrorCode::ToolAuthorizationRequired),
        "SSE 401 must classify as 4006, got none for: {}",
        msg
    );
}

/// SSE：403 → 4007 可达。
#[tokio::test]
async fn test_sse_real_403_reaches_4007() {
    let e = sse_call_and_capture_err(StatusCode::FORBIDDEN).await;
    let msg = e.to_string();
    println!("[SSE 403 +WWW-Authenticate] {}", msg);

    assert_eq!(
        classify_auth_error(&e),
        Some(ErrorCode::ToolAuthorizationFailed),
        "SSE 403 must classify as 4007, got none for: {}",
        msg
    );
}
