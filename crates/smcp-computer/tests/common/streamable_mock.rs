//! #149 共享 Streamable HTTP mock —— `bundle_id_addressing_conformance` /
//! `auth_error_real_transport` / `python_rust_alignment::test_auto_reconnect_semantics` 共用。
//!
//! 合并自上述三测试此前各自滚的同构 mock 脚手架（`full_body`/`empty_body`/`*_handler`/`spawn_*`），
//! 差异收敛进 [`MockOpts`] 三开关。**非 cli 门控**：本文件不依赖 `cli` feature，经各测试文件
//! `#[path = "common/streamable_mock.rs"] mod streamable_mock;` 引入，故 `test-ws`（无 cli）下亦可编译。
//!
//! 行为保真要点（验收「行为不变含 WWW-Authenticate 开关」）：
//! - 默认 [`MockOpts::default()`] 逐字复现 `bundle_id_addressing_conformance` 原 mock（403 / 无 WWW-Authenticate /
//!   有 `resources/list`）；
//! - `auth_error_real_transport` 的 HTTP 段用可配 `reject_status` + `with_www_authenticate` 开关，**完整保留**
//!   401 + `WWW-Authenticate` 触发 rmcp `AuthRequired` 短路这一形态（不可简化掉）。
#![allow(dead_code)]

use std::convert::Infallible;

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

pub type BoxBody = http_body_util::combinators::BoxBody<Bytes, Infallible>;

pub fn full_body(s: impl Into<Bytes>) -> BoxBody {
    Full::new(s.into()).map_err(|never| match never {}).boxed()
}

pub fn empty_body() -> BoxBody {
    full_body(Bytes::new())
}

/// Streamable HTTP mock 行为开关。
///
/// - `reject_status`：`tools/call` 返回的状态码（寻址对拍用 403；AUTH-01 用 401/403）。
/// - `with_www_authenticate`：拒绝响应是否带 `WWW-Authenticate` 头（rmcp 401 短路成 `AuthRequired` 的触发条件）。
/// - `expose_resources`：是否声明 resources capability 并响应 `resources/list`（寻址对拍需；AUTH-01 不需）。
#[derive(Debug, Clone, Copy)]
pub struct MockOpts {
    pub reject_status: StatusCode,
    pub with_www_authenticate: bool,
    pub expose_resources: bool,
}

impl Default for MockOpts {
    fn default() -> Self {
        Self {
            reject_status: StatusCode::FORBIDDEN,
            with_www_authenticate: false,
            expose_resources: true,
        }
    }
}

/// 握手放行、仅 `tools/call` 返 [`MockOpts::reject_status`] 的 Streamable HTTP mock handler。
async fn streamable_handler(
    req: Request<hyper::body::Incoming>,
    opts: MockOpts,
) -> Result<Response<BoxBody>, Infallible> {
    if req.method() != Method::POST {
        // 非-POST（rmcp 打开的 GET SSE 通知流）→ 405。rmcp 据此立即降级为
        // `ServerDoesNotSupportSse`（`streamable_http_client.rs:429`），不耗超时——见 #149 第 3 项结论。
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

    // 仅 tools/call 被拒 —— 复现「已连接但调用需授权」的生产形态。
    if method == "tools/call" {
        let mut builder = Response::builder()
            .status(opts.reject_status)
            .header("Content-Type", "text/plain");
        if opts.with_www_authenticate {
            builder = builder.header("WWW-Authenticate", "Bearer realm=\"mcp\"");
        }
        return Ok(builder
            .body(full_body(
                opts.reject_status.canonical_reason().unwrap_or("denied"),
            ))
            .unwrap());
    }

    let capabilities = if opts.expose_resources {
        serde_json::json!({ "tools": {}, "resources": {} })
    } else {
        serde_json::json!({ "tools": {} })
    };
    let result = match method {
        "initialize" => serde_json::json!({
            "protocolVersion": "2024-11-05",
            "serverInfo": { "name": "streamable-mock", "version": "0.1.0" },
            "capabilities": capabilities,
        }),
        "tools/list" => serde_json::json!({
            "tools": [{
                "name": "protected",
                "description": "Requires upstream auth",
                "inputSchema": { "type": "object" }
            }]
        }),
        "resources/list" if opts.expose_resources => serde_json::json!({
            "resources": [{
                "uri": "window://streamable-mock/main",
                "name": "main window",
                "mimeType": "text/plain"
            }]
        }),
        _ => serde_json::json!({}),
    };

    let resp_json = serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result });
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json");
    if method == "initialize" {
        builder = builder.header("mcp-session-id", "streamable-mock-session-001");
    }
    Ok(builder
        .body(full_body(serde_json::to_vec(&resp_json).unwrap()))
        .unwrap())
}

/// 起一台 Streamable HTTP mock，返回监听端口（`127.0.0.1:0` 随机端口）。
pub async fn spawn_streamable_mock(opts: MockOpts) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let io = TokioIo::new(stream);
            tokio::spawn(async move {
                let service =
                    service_fn(move |req| async move { streamable_handler(req, opts).await });
                // reqwest 0.13 may optimistically reuse a just-closed test connection under
                // parallel load; advertise close explicitly so auth status assertions observe
                // the intended 401/403 response instead of a transient IncompleteMessage.
                let mut builder = hyper::server::conn::http1::Builder::new();
                builder.keep_alive(false);
                let _ = builder.serve_connection(io, service).await;
            });
        }
    });
    port
}
