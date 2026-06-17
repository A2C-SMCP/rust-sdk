/*!
 * AUTH-DICT #85/#86：Socket.IO CONNECT `auth` dict 注入端到端测试。
 * End-to-end tests for Socket.IO CONNECT `auth` dict injection.
 *
 * #86 起连接面鉴权**唯一**走 Socket.IO `auth` dict（HTTP header 鉴权已退役）。本测试用一个
 * **裸 socketioxide** capture server 在 connect handler 用 `TryData<Value>` 提取器**逐字段**断言
 * 客户端实际放到 CONNECT `auth` 字段的负载（比「连接成功」更锐利），验证：
 *   1. `auth_payload` → 落入 Socket.IO CONNECT auth dict（server 从 auth dict 读到 token）；
 *   2. 4900→polling 重连（`fetch_4008_via_polling`）同样重放 auth dict（不退化为无鉴权）。
 *
 * 真实 SMCP server 读 auth dict 的回归由 server-core / e2e 套件覆盖；此处聚焦 on-wire 负载本身。
 */

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use http_body_util::Full;
use hyper::body::Bytes;
use serde_json::{json, Value};
use socketioxide::extract::TryData;
use socketioxide::SocketIo;
use tokio::net::TcpListener;
use tokio::sync::{oneshot, RwLock};
use tokio::time::sleep;
use tower::Layer;

use smcp_computer::mcp_clients::manager::MCPServerManager;
use smcp_computer::mcp_clients::model::MCPServerInput;
use smcp_computer::socketio_client::SmcpComputerClientBuilder;

/// 服务端握手期捕获到的 Socket.IO CONNECT `auth` 负载 / The captured CONNECT `auth` payload.
#[derive(Default)]
struct Captured {
    auth: Mutex<Option<Value>>,
}

fn empty_manager() -> Arc<RwLock<Option<MCPServerManager>>> {
    Arc::new(RwLock::new(None))
}

fn empty_inputs() -> Arc<RwLock<HashMap<String, MCPServerInput>>> {
    Arc::new(RwLock::new(HashMap::new()))
}

/// 轮询直到 `probe()` 返回 `Some` 或 ~2s 超时（替代固定 sleep，消除时序脆弱、握手一就绪即返回）。
/// Poll until `probe()` yields `Some` or ~2s elapses — replaces fixed sleeps so the assertion
/// fires as soon as the handshake settles and isn't flaky under CI load.
async fn wait_for<T>(mut probe: impl FnMut() -> Option<T>) -> Option<T> {
    for _ in 0..100 {
        if let Some(value) = probe() {
            return Some(value);
        }
        sleep(Duration::from_millis(20)).await;
    }
    probe()
}

/// 启动裸 socketioxide+hyper capture server，返回 (url, shutdown_tx)。
/// `io.ns("/smcp")` 与 SMCP server 的 `io.ns(SMCP_NAMESPACE)` 一致，使 Computer 默认 `/smcp` 可连。
async fn start_capture_server(captured: Arc<Captured>) -> (String, oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();

    let (layer, io) = SocketIo::new_layer();
    io.ns("/smcp", move |TryData(auth): TryData<Value>| {
        let captured = captured.clone();
        async move {
            // CONNECT auth dict（解码成功才记录；无 auth 时 TryData 给 Err，captured 维持 None）。
            if let Ok(value) = auth {
                *captured.auth.lock().unwrap() = Some(value);
            }
        }
    });

    // 关键：layer 只叠一次后整体 clone 给每条连接（polling 跨多次 HTTP 请求维持 engine.io 会话）。
    let fallback = tower::service_fn(|_req: hyper::Request<hyper::body::Incoming>| async move {
        Ok::<_, std::convert::Infallible>(hyper::Response::new(Full::<Bytes>::new(Bytes::new())))
    });
    let service = layer.layer(fallback);

    let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    if let Ok((stream, _)) = accepted {
                        let tio = hyper_util::rt::TokioIo::new(stream);
                        let svc = hyper_util::service::TowerToHyperService::new(service.clone());
                        tokio::spawn(async move {
                            let _ = hyper::server::conn::http1::Builder::new()
                                .serve_connection(tio, svc)
                                .with_upgrades()
                                .await;
                        });
                    }
                }
                _ = &mut shutdown_rx => break,
            }
        }
    });

    sleep(Duration::from_millis(120)).await;
    (format!("http://{addr}"), shutdown_tx)
}

/// #86 验收①：`auth_payload` 注入 Socket.IO CONNECT `auth` 字段，server 从 auth dict 读到 token。
#[tokio::test]
async fn auth_payload_is_injected_into_socketio_connect_auth_dict() {
    let _ = tracing_subscriber::fmt::try_init();
    let captured = Arc::new(Captured::default());
    let (url, shutdown) = start_capture_server(captured.clone()).await;

    let client = SmcpComputerClientBuilder::new(
        url.as_str(),
        empty_manager(),
        "auth-dict-computer",
        empty_inputs(),
    )
    .namespace("/smcp")
    .auth_payload(json!({ "token": "jwt-abc-123" }))
    .connect()
    .await
    .expect("connect with auth_payload should succeed");

    let auth = wait_for(|| captured.auth.lock().unwrap().clone()).await;
    assert_eq!(
        auth,
        Some(json!({ "token": "jwt-abc-123" })),
        "server must read the token from the Socket.IO CONNECT auth dict"
    );

    client.disconnect().await.ok();
    let _ = shutdown.send(());
}

/// #86 验收②：4900→polling 重连必须重放 auth dict。直接驱动 `connect_and_classify` 的重连原语
/// `fetch_4008_via_polling`：它在 polling 上应用同一 auth 负载后连接 capture server——裸 socketioxide
/// 会"意外成功"→断开→返回 fallback PVE（此处忽略），但断开前 connect handler 已捕获 auth，
/// 足以证明重连路径携带 auth dict（不退化为无鉴权）。
///
/// ⚠️ 覆盖边界（刻意）：本测试直击重连原语，**未经** production 入口 `connect_and_classify`
/// （仅 4900 分支才调本原语）；`connect_and_classify` 把 `auth` 透传给本原语这一行直传
/// （`smcp-client-transport/src/lib.rs`）靠代码审查 + 本测试对原语本身的实证共同守住。
#[tokio::test]
async fn reconnect_4008_polling_replays_auth_dict() {
    let _ = tracing_subscriber::fmt::try_init();
    let captured = Arc::new(Captured::default());
    let (url, shutdown) = start_capture_server(captured.clone()).await;

    let _pve = smcp_client_transport::fetch_4008_via_polling(
        &url,
        "/smcp",
        Some(json!({ "token": "jwt-reconnect-xyz" })),
        HashMap::new(),
    )
    .await;

    let auth = wait_for(|| captured.auth.lock().unwrap().clone()).await;
    assert_eq!(
        auth,
        Some(json!({ "token": "jwt-reconnect-xyz" })),
        "the 4900->polling reconnect must replay the auth dict"
    );

    let _ = shutdown.send(());
}
