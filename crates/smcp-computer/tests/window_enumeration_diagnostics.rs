//! #161 窗口枚举能力门传输层接线验证（真实 Streamable HTTP 传输）。
//!
//! 背景：三传输 `list_windows` 此前无 resources 能力预检（预检先例只在 `list_resources_page`，
//! INT-04 #78），未声明 `resources` capability 的 server 在窗口枚举路径报 `ProtocolError`（或被
//! mock 解析成空集 `Ok(vec![])`），与其它失败不可区分——manager 聚合层因此无法产出「capability
//! 缺失」诊断（#161 根因之一）。
//!
//! 本文件用**真实传输**（共享 Streamable HTTP mock，#149）锁死接线：
//! - `expose_resources: false` → initialize 只声明 `tools` → `list_windows` 必须
//!   `Err(CapabilityNotSupported)`（且**非** `ProtocolError`、非空集 `Ok`）；
//! - `expose_resources: true`（default）→ 预检放行 → 返回 mock 的 `window://streamable-mock/main`。
//!
//! stdio / SSE 传输的预检块与各自已测的 `list_resources_page` 逐字同源（INT-04 同款拷贝），
//! 不在本文件重复覆盖（stdio 需真子进程、SSE 需另造 SSE mock，成本不成比例）。
use std::collections::HashMap;
use std::time::Duration;

// #149：共享 Streamable HTTP mock（非 cli 门控，test-ws 可编译）。
#[path = "common/streamable_mock.rs"]
mod streamable_mock;
use streamable_mock::{spawn_streamable_mock, MockOpts};

use hyper::StatusCode;
use smcp_computer::mcp_clients::http_client::HttpMCPClient;
use smcp_computer::mcp_clients::model::*;

/// 起一台给定 `expose_resources` 的 mock 并完成握手，返回已连接 client。
/// time-box 包裹连接，防 rmcp 行为回归时 CI 无限挂（对齐 bundle_id_addressing_conformance 约定）。
async fn connected_client(expose_resources: bool) -> HttpMCPClient {
    let port = spawn_streamable_mock(MockOpts {
        reject_status: StatusCode::FORBIDDEN,
        with_www_authenticate: false,
        expose_resources,
        ..Default::default()
    })
    .await;
    let client = HttpMCPClient::new(HttpServerParameters {
        url: format!("http://127.0.0.1:{}", port),
        headers: HashMap::new(),
    });
    let connected = tokio::time::timeout(Duration::from_secs(5), client.connect()).await;
    match connected {
        Err(_) => panic!("HANG: connect did not resolve within 5s"),
        Ok(Err(e)) => panic!("handshake must pass, got: {}", e),
        Ok(Ok(())) => (),
    }
    client
}

/// #161：未声明 `resources` capability 的 server 上 `list_windows` 报
/// `CapabilityNotSupported`（能力门信号），而非 `ProtocolError` / 空集 `Ok`——
/// 这是 manager 聚合层区分「capability 缺失」与「成功空集」的传输层前提。
#[tokio::test]
async fn http_list_windows_without_capability_reports_capability_error_161() {
    let client = connected_client(false).await;

    let outcome = tokio::time::timeout(Duration::from_secs(5), client.list_windows()).await;
    match outcome {
        Err(_) => panic!("HANG: list_windows did not resolve within 5s"),
        Ok(Ok(windows)) => panic!(
            "capability gate missing: no-capability server returned Ok({:?}) \
             (mock answers resources/list with empty result), expected CapabilityNotSupported",
            windows
        ),
        Ok(Err(e)) => {
            assert!(
                matches!(e, MCPClientError::CapabilityNotSupported(_)),
                "expected CapabilityNotSupported (capability gate), got: {} [{:?}]",
                e,
                e
            );
        }
    }
}

/// 对照（防假绿）：声明 `resources` capability 的 server 预检放行，正常返回 mock 窗口。
#[tokio::test]
async fn http_list_windows_with_capability_returns_windows_161() {
    let client = connected_client(true).await;

    let windows = client
        .list_windows()
        .await
        .expect("list_windows must pass the capability gate and succeed");
    assert_eq!(
        windows.len(),
        1,
        "mock exposes exactly one window resource, got: {:?}",
        windows
    );
    assert_eq!(windows[0].uri.as_str(), "window://streamable-mock/main");
}
