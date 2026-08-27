//! #208 可阻塞 Streamable HTTP mock —— `tools/call` 在 `Arc<Notify>` 上等待（由测试控制释放），
//! 制造「跨 Engine.IO 心跳窗口的长工具调用」，供 socketio_boundary_test.rs 的 4 个边界测试使用。
//!
//! 独立自包含文件（非 cli 门控，经 `#[path]` 引入，同 streamable_mock 约定）：**不**并入
//! `streamable_mock` —— 该文件正处于 #200 ToolMeta 的未提交改动中，保持互不干扰。
//!
//! 行为以 `streamable_mock.rs` 为蓝本（rmcp 兼容面）：非-POST（rmcp 的 GET SSE 通知流）→ 405，
//! rmcp 据此立即降级不耗超时（#149）；`notifications/*` → 202；`initialize` / `tools/list` 正常讲。
#![allow(dead_code)]

use std::convert::Infallible;
use std::sync::Arc;

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::sync::Notify;

pub type BoxBody = http_body_util::combinators::BoxBody<Bytes, Infallible>;

pub fn full_body(s: impl Into<Bytes>) -> BoxBody {
    Full::new(s.into()).map_err(|never| match never {}).boxed()
}

pub fn empty_body() -> BoxBody {
    full_body(Bytes::new())
}

/// Streamable HTTP handler：`tools/call` 在 `release.notified()` 上阻塞（可释放），其余同
/// streamable_mock 的放行面（initialize / tools/list / notifications 202 / 非-POST 405）。
async fn blocking_handler(
    req: Request<hyper::body::Incoming>,
    release: Arc<Notify>,
) -> Result<Response<BoxBody>, Infallible> {
    if req.method() != Method::POST {
        // GET SSE 通知流（rmcp 打开）→ 405：rmcp 立即降级为 ServerDoesNotSupportSse（#149）。
        return Ok(Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .header("content-type", "text/plain")
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

    if method == "tools/call" {
        // 阻塞直至测试释放 —— 唯一与 streamable_mock 的差异点。
        release.notified().await;
        let result = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "content": [{"type": "text", "text": "blocking released"}],
                "isError": false
            }
        });
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .body(full_body(serde_json::to_vec(&result).unwrap()))
            .unwrap());
    }

    let result = match method {
        "initialize" => serde_json::json!({
            "protocolVersion": "2024-11-05",
            "serverInfo": { "name": "blocking-mock", "version": "0.1.0" },
            "capabilities": { "tools": {} },
        }),
        "tools/list" => serde_json::json!({
            "tools": [{
                "name": "waiting",
                "description": "Blocks until the test releases it (#208)",
                "inputSchema": { "type": "object" }
            }]
        }),
        _ => serde_json::json!({}),
    };
    // 信封与 streamable_mock 一致：裸 result 包进 JSON-RPC message；`_` 分支返回空
    // result（rmcp 视为方法未支持的响应）。
    let resp_json = serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result });
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json");
    if method == "initialize" {
        // rmcp 2.2 要求 initialize 响应携带 mcp-session-id（除非客户端 stateless 配置）；
        // 同 streamable_mock 的行内约定。
        builder = builder.header("mcp-session-id", "blocking-mock-session-001");
    }
    Ok(builder
        .body(full_body(serde_json::to_vec(&resp_json).unwrap()))
        .unwrap())
}

/// 启动 blocking mock，返回 `(port, release)`。
pub async fn spawn_blocking_tool_mock() -> (u16, Arc<Notify>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let release: Arc<Notify> = Arc::new(Notify::new());

    let release_for_task = release.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let release = release_for_task.clone();
            let io = TokioIo::new(stream);
            let service = service_fn(move |req| {
                let release = release.clone();
                async move { blocking_handler(req, release).await }
            });
            tokio::spawn(async move {
                let mut builder = hyper::server::conn::http1::Builder::new();
                builder.keep_alive(false);
                let _ = builder.serve_connection(io, service).await;
            });
        }
    });

    (port, release)
}
