//! #267 S4 —— MCP client teardown 耗时上界（CI-safe）。
//!
//! 覆盖 #254 遗留的 S4：对端不响应时，stdio/HTTP 的 `service.cancel()` 与 SSE 的
//! shutdown/exit 投递都不得把 teardown 拖长。E4 实测（dev-0.5.0 @a8bdfb6）：
//! - HTTP 服务器停 POST（rmcp 会话清理 DELETE 无响应）→ disconnect **5.0s**
//! - SSE 服务器不回包（lifecycle POST 挂起）→ disconnect **2.0s**
//! - stdio 子进程忽略 stdin → disconnect 6.3ms（已天然有界，`stdio_integration.rs` 留回归栅栏）
//!
//! 本文件用**确定性阻塞**的 in-process mock（`pending`/`Notify` 闸门），不靠 sleep 竞态。

#[path = "common/streamable_mock.rs"]
mod streamable_mock;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use smcp_computer::mcp_clients::http_client::HttpMCPClient;
use smcp_computer::mcp_clients::model::{ClientState, HttpServerParameters, MCPClientProtocol};
use streamable_mock::{spawn_streamable_mock_with_handle, MockOpts};

/// 生产上限 1s（`MCP_SERVICE_CLOSE_TIMEOUT`）；并行 CI 下断言留 1.5s 裕量（审查 🟡：1.5s 总界对
/// 1s 生产等待太紧）。仍远低于 E4 修复前的 5s 基线。
const DISCONNECT_BOUND: Duration = Duration::from_millis(2500);

/// HTTP：会话清理 DELETE 永不响应时，disconnect 必须在上限内返回，且**清理不丢**——
/// 由分离任务继续完成（#267 S4 复审要求：不能只测耗时）。
#[tokio::test]
async fn http_disconnect_is_bounded_when_session_cleanup_hangs() {
    let mock = spawn_streamable_mock_with_handle(MockOpts {
        hang_session_cleanup: true,
        ..MockOpts::default()
    })
    .await;
    let client = HttpMCPClient::new(HttpServerParameters {
        url: format!("http://127.0.0.1:{}", mock.port),
        headers: HashMap::new(),
    });
    tokio::time::timeout(Duration::from_secs(10), client.connect())
        .await
        .expect("connect timed out")
        .expect("connect failed");

    let started = Instant::now();
    tokio::time::timeout(Duration::from_secs(10), client.disconnect())
        .await
        .expect("disconnect must not hang on a hung session-cleanup DELETE")
        .expect("disconnect failed");
    let elapsed = started.elapsed();

    assert_eq!(client.state(), ClientState::Disconnected);
    assert!(
        elapsed < DISCONNECT_BOUND,
        "disconnect must be bounded (1s outer bound; E4 pre-fix 5s), took {elapsed:?}"
    );

    // 超时分支不得把清理一并丢弃：DELETE 仍阻塞（mock 未释放）⇒ 分离任务确实在跑。
    // 轮询到 1 而非瞬时硬断言：分离任务要先完成 rmcp 的 Cancelled drain（上限 2s）才发 DELETE
    //（审查 🟡：drain 在极端负载下可能晚于 1s 外层超时）。
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while mock.cleanup_active() != 1 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        mock.cleanup_active(),
        1,
        "detached session cleanup must still be in flight after the bounded disconnect"
    );

    // 生产语义：disconnect 返回后立即重连、旧 DELETE 仍在途（审查 🟡：必须在释放 cleanup 之前）。
    // 服务已不可再用：状态机拒绝请求；client 未被旧 service 卡死——可重新 connect。
    assert!(
        client.list_tools().await.is_err(),
        "disconnected client must reject requests"
    );
    tokio::time::timeout(Duration::from_secs(10), client.connect())
        .await
        .expect("reconnect timed out")
        .expect("client must remain usable after bounded teardown");
    client.disconnect().await.expect("second disconnect failed");

    mock.release_cleanup();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while mock.cleanup_active() != 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        mock.cleanup_active(),
        0,
        "detached session cleanups must complete after the server releases the DELETE"
    );
}
