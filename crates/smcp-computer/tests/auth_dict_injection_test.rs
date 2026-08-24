/*!
 * AUTH-DICT #85/#86 + #204：Socket.IO CONNECT auth 与自动重连会话恢复端到端测试。
 * End-to-end tests for Socket.IO CONNECT auth and automatic reconnect session recovery.
 *
 * #86 起连接面鉴权**唯一**走 Socket.IO `auth` dict（HTTP header 鉴权已退役）。本测试用一个
 * **裸 socketioxide** capture server 在 connect handler 用 `TryData<Value>` 提取器**逐字段**断言
 * 客户端实际放到 CONNECT `auth` 字段的负载（比「连接成功」更锐利），验证：
 *   1. `auth_payload` → 落入 Socket.IO CONNECT auth dict（server 从 auth dict 读到 token）；
 *   2. 4900→polling 重连（`fetch_4008_via_polling`）同样重放 auth dict（不退化为无鉴权）；
 *   3. 真实 TCP 中断后的 namespace 自动重连会重放 Office join，失败时清除假已加入状态。
 *
 * 真实 SMCP server 读 auth dict 的回归由 server-core / e2e 套件覆盖；此处聚焦 on-wire 负载本身。
 */

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use http_body_util::Full;
use hyper::body::Bytes;
use serde_json::{json, Value};
use socketioxide::extract::{AckSender, Data, SocketRef, TryData};
use socketioxide::SocketIo;
use tokio::io::copy_bidirectional;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, Barrier, RwLock};
use tokio::task::AbortHandle;
use tokio::time::{sleep, timeout};
use tower::Layer;

use smcp_computer::computer::{Computer, ConnectOptions, SilentSession, SocketIoAuthProvider};
use smcp_computer::mcp_clients::manager::MCPServerManager;
use smcp_computer::mcp_clients::model::MCPServerInput;
use smcp_computer::socketio_client::SmcpComputerClientBuilder;
use smcp_computer::{ComputerEvent, LifecycleState};

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

/// 可事件驱动观测 CONNECT auth、并通过关闭真实 TCP 连接触发底层自动重连的测试服务。
/// Event-driven CONNECT-auth capture server that can trigger transport auto-reconnect by closing
/// real TCP connections.
struct ReconnectCaptureServer {
    url: String,
    auth_rx: mpsc::UnboundedReceiver<Value>,
    join_rx: mpsc::UnboundedReceiver<Value>,
    active_connections: Arc<AtomicUsize>,
    namespace_socket: Arc<Mutex<Option<SocketRef>>>,
    connection_tasks: Arc<Mutex<Vec<AbortHandle>>>,
    backend_shutdown_tx: oneshot::Sender<()>,
    proxy_shutdown_tx: oneshot::Sender<()>,
}

impl ReconnectCaptureServer {
    async fn next_auth(&mut self) -> Value {
        timeout(Duration::from_secs(10), self.auth_rx.recv())
            .await
            .expect("timed out waiting for Socket.IO CONNECT auth")
            .expect("capture server stopped before receiving auth")
    }

    async fn next_join(&mut self) -> Value {
        timeout(Duration::from_secs(10), self.join_rx.recv())
            .await
            .expect("timed out waiting for server:join_office")
            .expect("capture server stopped before receiving join_office")
    }

    async fn wait_for_active_connections(&self, expected: usize) {
        timeout(Duration::from_secs(10), async {
            loop {
                if self.active_connections.load(Ordering::SeqCst) == expected {
                    return;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {expected} active connections"));
    }

    async fn assert_no_additional_auth(&mut self) {
        assert!(
            timeout(Duration::from_millis(1500), self.auth_rx.recv())
                .await
                .is_err(),
            "an already-retired client unexpectedly reconnected"
        );
    }

    fn force_namespace_disconnect(&self) {
        self.namespace_socket
            .lock()
            .unwrap()
            .clone()
            .expect("capture server must have a namespace socket")
            .disconnect()
            .expect("server namespace disconnect");
    }

    fn force_network_disconnect(&self) {
        let handles = std::mem::take(&mut *self.connection_tasks.lock().unwrap());
        assert!(
            !handles.is_empty(),
            "capture server must have at least one TCP connection to interrupt"
        );
        for handle in handles {
            handle.abort();
        }
    }

    fn shutdown(self) {
        let _ = self.proxy_shutdown_tx.send(());
        let _ = self.backend_shutdown_tx.send(());
    }
}

async fn start_reconnect_capture_server(
    reject_join_attempt: Option<usize>,
    delayed_join_attempt: Option<usize>,
) -> ReconnectCaptureServer {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_addr = backend_listener.local_addr().unwrap();
    let (auth_tx, auth_rx) = mpsc::unbounded_channel();
    let (join_tx, join_rx) = mpsc::unbounded_channel();
    let join_attempts = Arc::new(AtomicUsize::new(0));
    let active_connections = Arc::new(AtomicUsize::new(0));
    let namespace_socket = Arc::new(Mutex::new(None));

    let (layer, io) = SocketIo::new_layer();
    let namespace_active_connections = Arc::clone(&active_connections);
    let connected_namespace_socket = Arc::clone(&namespace_socket);
    io.ns(
        "/smcp",
        move |socket: SocketRef, TryData(auth): TryData<Value>| {
            let auth_tx = auth_tx.clone();
            let join_tx = join_tx.clone();
            let join_attempts = Arc::clone(&join_attempts);
            let active_connections = Arc::clone(&namespace_active_connections);
            let namespace_socket = Arc::clone(&connected_namespace_socket);
            async move {
                active_connections.fetch_add(1, Ordering::SeqCst);
                *namespace_socket.lock().unwrap() = Some(socket.clone());
                socket.on_disconnect({
                    let active_connections = Arc::clone(&active_connections);
                    move |_socket: SocketRef| {
                        let active_connections = Arc::clone(&active_connections);
                        async move {
                            active_connections.fetch_sub(1, Ordering::SeqCst);
                        }
                    }
                });
                if let Ok(value) = auth {
                    let _ = auth_tx.send(value);
                }
                socket.on(
                    "server:join_office",
                    move |_socket: SocketRef, Data::<Value>(data), ack: AckSender| {
                        let join_tx = join_tx.clone();
                        let join_attempts = Arc::clone(&join_attempts);
                        async move {
                            let attempt = join_attempts.fetch_add(1, Ordering::SeqCst) + 1;
                            let _ = join_tx.send(data);
                            if delayed_join_attempt == Some(attempt) {
                                sleep(Duration::from_millis(750)).await;
                            }
                            let success = reject_join_attempt != Some(attempt);
                            let message = (!success).then(|| "rejoin rejected".to_string());
                            let _ = ack.send(&(success, message));
                        }
                    },
                );
            }
        },
    );

    let fallback = tower::service_fn(|_req: hyper::Request<hyper::body::Incoming>| async move {
        Ok::<_, std::convert::Infallible>(hyper::Response::new(Full::<Bytes>::new(Bytes::new())))
    });
    let service = layer.layer(fallback);

    let (backend_shutdown_tx, mut backend_shutdown_rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                accepted = backend_listener.accept() => {
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
                _ = &mut backend_shutdown_rx => break,
            }
        }
    });

    // 客户端只连接 TCP proxy。中止 copy_bidirectional 任务会同时关闭 client/backend socket，
    // 从而制造真实网络断开，同时保持 Socket.IO 服务端可立即接受自动重连。
    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    let connection_tasks = Arc::new(Mutex::new(Vec::new()));
    let task_handles = Arc::clone(&connection_tasks);
    let (proxy_shutdown_tx, mut proxy_shutdown_rx) = oneshot::channel::<()>();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                accepted = proxy_listener.accept() => {
                    if let Ok((mut downstream, _)) = accepted {
                        let task = tokio::spawn(async move {
                            if let Ok(mut upstream) = TcpStream::connect(backend_addr).await {
                                let _ = copy_bidirectional(&mut downstream, &mut upstream).await;
                            }
                        });
                        task_handles.lock().unwrap().push(task.abort_handle());
                    }
                }
                _ = &mut proxy_shutdown_rx => break,
            }
        }
    });

    ReconnectCaptureServer {
        url: format!("http://{proxy_addr}"),
        auth_rx,
        join_rx,
        active_connections,
        namespace_socket,
        connection_tasks,
        backend_shutdown_tx,
        proxy_shutdown_tx,
    }
}

async fn wait_for_lifecycle(
    events: &mut tokio::sync::broadcast::Receiver<ComputerEvent>,
    expected: LifecycleState,
) {
    timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(ComputerEvent::LifecycleChanged { state }) = events.recv().await {
                if state == expected {
                    return;
                }
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for lifecycle {expected}"));
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

/// #201：同一个动态 Provider 必须用于首连和每次真实网络断线后的自动重连；调用严格串行。
#[tokio::test]
async fn dynamic_auth_provider_refreshes_each_real_network_reconnect_serially() {
    let _ = tracing_subscriber::fmt::try_init();
    let mut server = start_reconnect_capture_server(None, None).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let active = Arc::new(AtomicUsize::new(0));
    let max_active = Arc::new(AtomicUsize::new(0));

    let provider: SocketIoAuthProvider = {
        let calls = Arc::clone(&calls);
        let active = Arc::clone(&active);
        let max_active = Arc::clone(&max_active);
        Arc::new(move || {
            let attempt = calls.fetch_add(1, Ordering::SeqCst);
            let active = Arc::clone(&active);
            let max_active = Arc::clone(&max_active);
            Box::pin(async move {
                let concurrent = active.fetch_add(1, Ordering::SeqCst) + 1;
                max_active.fetch_max(concurrent, Ordering::SeqCst);
                tokio::task::yield_now().await;
                active.fetch_sub(1, Ordering::SeqCst);

                let token = match attempt {
                    0 => "token-a",
                    1 => "token-b",
                    2 => "token-c",
                    _ => "unexpected-extra-token",
                };
                json!({ "token": token })
            })
        })
    };

    let temp = tempfile::TempDir::new().unwrap();
    let computer = Computer::new(
        "dynamic-auth-computer",
        SilentSession::new("dynamic-auth-session"),
        None,
        None,
        false,
        false,
    )
    .with_skill_home(temp.path().join("skills"))
    .with_blob_cache_root(temp.path().join("blob"))
    .with_config_dir(temp.path().join("config"));
    computer.boot_up().await.expect("boot computer");
    computer
        .connect_socketio(
            &server.url,
            ConnectOptions {
                auth_provider: Some(provider),
                // Provider takes precedence over a simultaneously configured static payload.
                auth_payload: Some(json!({ "token": "stale-static-token" })),
                ..Default::default()
            },
        )
        .await
        .expect("connect with dynamic auth provider");

    assert_eq!(server.next_auth().await, json!({ "token": "token-a" }));

    server.force_network_disconnect();
    assert_eq!(server.next_auth().await, json!({ "token": "token-b" }));

    server.force_network_disconnect();
    assert_eq!(server.next_auth().await, json!({ "token": "token-c" }));

    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert_eq!(max_active.load(Ordering::SeqCst), 1);

    computer.shutdown().await.expect("shutdown computer");
    server.shutdown();
}

/// #204：真实 TCP 中断后，namespace 自动重连必须重放已确认的 Office join，并恢复公开状态。
#[tokio::test]
async fn automatic_network_reconnect_rejoins_office_and_restores_lifecycle() {
    let _ = tracing_subscriber::fmt::try_init();
    let mut server = start_reconnect_capture_server(None, None).await;
    let temp = tempfile::TempDir::new().unwrap();
    let computer = Computer::new(
        "office-rejoin-computer",
        SilentSession::new("office-rejoin-session"),
        None,
        None,
        false,
        false,
    )
    .with_skill_home(temp.path().join("skills"))
    .with_blob_cache_root(temp.path().join("blob"))
    .with_config_dir(temp.path().join("config"));
    computer.boot_up().await.expect("boot computer");
    computer
        .connect_socketio(
            &server.url,
            ConnectOptions {
                auth_payload: Some(json!({ "token": "office-rejoin-token" })),
                ..Default::default()
            },
        )
        .await
        .expect("connect computer");
    // No server-channel receive or fixed sleep is needed: connect returns namespace-ready.
    assert_eq!(computer.lifecycle_state(), LifecycleState::Connected);
    computer
        .join_office("office-rejoin", "office-rejoin-computer")
        .await
        .expect("immediate initial join");
    assert_eq!(
        server.next_auth().await,
        json!({ "token": "office-rejoin-token" })
    );
    assert_eq!(server.next_join().await["office_id"], "office-rejoin");
    assert_eq!(computer.lifecycle_state(), LifecycleState::JoinedOffice);

    let mut events = computer.subscribe_events();
    server.force_network_disconnect();
    wait_for_lifecycle(&mut events, LifecycleState::Connecting).await;
    assert_eq!(server.next_auth().await["token"], "office-rejoin-token");
    assert_eq!(server.next_join().await["office_id"], "office-rejoin");
    wait_for_lifecycle(&mut events, LifecycleState::JoinedOffice).await;

    let socketio = computer.get_socketio_client();
    let socketio = socketio.read().await;
    assert_eq!(
        socketio.as_ref().unwrap().get_office_id().await.as_deref(),
        Some("office-rejoin")
    );
    drop(socketio);

    computer.shutdown().await.expect("shutdown computer");
    server.shutdown();
}

/// #204：自动重加入被服务端拒绝时，不得保留 `JoinedOffice` 或旧 Office ID。
#[tokio::test]
async fn rejected_automatic_rejoin_clears_confirmed_office_state() {
    let _ = tracing_subscriber::fmt::try_init();
    let mut server = start_reconnect_capture_server(Some(2), None).await;
    let temp = tempfile::TempDir::new().unwrap();
    let computer = Computer::new(
        "office-rejoin-rejected",
        SilentSession::new("office-rejoin-rejected-session"),
        None,
        None,
        false,
        false,
    )
    .with_skill_home(temp.path().join("skills"))
    .with_blob_cache_root(temp.path().join("blob"))
    .with_config_dir(temp.path().join("config"));
    computer.boot_up().await.expect("boot computer");
    computer
        .connect_socketio(
            &server.url,
            ConnectOptions {
                auth_payload: Some(json!({ "token": "office-rejoin-rejected-token" })),
                ..Default::default()
            },
        )
        .await
        .expect("connect computer");
    let _ = server.next_auth().await;
    computer
        .join_office("office-rejoin-rejected", "office-rejoin-rejected")
        .await
        .expect("initial join");
    let _ = server.next_join().await;

    let mut events = computer.subscribe_events();
    server.force_network_disconnect();
    wait_for_lifecycle(&mut events, LifecycleState::Connecting).await;
    let _ = server.next_auth().await;
    let _ = server.next_join().await;
    wait_for_lifecycle(&mut events, LifecycleState::Connected).await;

    let socketio = computer.get_socketio_client();
    let socketio = socketio.read().await;
    assert_eq!(socketio.as_ref().unwrap().get_office_id().await, None);
    drop(socketio);

    computer.shutdown().await.expect("shutdown computer");
    server.shutdown();
}

/// #204：切换 Office 的 ACK 等待/拒绝期间，最后一次已确认的 Office 与 lifecycle 保持一致。
#[tokio::test]
async fn rejected_explicit_office_switch_preserves_last_confirmed_membership() {
    let _ = tracing_subscriber::fmt::try_init();
    let mut server = start_reconnect_capture_server(Some(2), Some(2)).await;
    let temp = tempfile::TempDir::new().unwrap();
    let computer = Computer::new(
        "office-switch-rejected",
        SilentSession::new("office-switch-rejected-session"),
        None,
        None,
        false,
        false,
    )
    .with_skill_home(temp.path().join("skills"))
    .with_blob_cache_root(temp.path().join("blob"))
    .with_config_dir(temp.path().join("config"));
    computer.boot_up().await.expect("boot computer");
    computer
        .connect_socketio(&server.url, ConnectOptions::default())
        .await
        .expect("connect computer");
    let _ = server.next_auth().await;
    computer
        .join_office("office-a", "office-switch-rejected")
        .await
        .expect("initial join");
    let _ = server.next_join().await;

    let client = {
        let socketio = computer.get_socketio_client();
        let client = socketio.read().await.as_ref().unwrap().clone();
        client
    };
    let switch_client = Arc::clone(&client);
    let switching = tokio::spawn(async move { switch_client.join_office("office-b").await });
    assert_eq!(server.next_join().await["office_id"], "office-b");

    assert_eq!(computer.lifecycle_state(), LifecycleState::JoinedOffice);
    assert_eq!(client.get_office_id().await.as_deref(), Some("office-a"));
    assert!(switching.await.unwrap().is_err());
    assert_eq!(computer.lifecycle_state(), LifecycleState::JoinedOffice);
    assert_eq!(client.get_office_id().await.as_deref(), Some("office-a"));

    computer.shutdown().await.expect("shutdown computer");
    server.shutdown();
}

/// #204：连续两次断线会失效第一条连接的延迟 ACK；只有最新连接可以提交 Office 状态。
#[tokio::test]
async fn consecutive_disconnects_ignore_stale_rejoin_ack() {
    let _ = tracing_subscriber::fmt::try_init();
    let mut server = start_reconnect_capture_server(None, Some(2)).await;
    let temp = tempfile::TempDir::new().unwrap();
    let computer = Computer::new(
        "office-rejoin-generation",
        SilentSession::new("office-rejoin-generation-session"),
        None,
        None,
        false,
        false,
    )
    .with_skill_home(temp.path().join("skills"))
    .with_blob_cache_root(temp.path().join("blob"))
    .with_config_dir(temp.path().join("config"));
    computer.boot_up().await.expect("boot computer");
    computer
        .connect_socketio(&server.url, ConnectOptions::default())
        .await
        .expect("connect computer");
    let _ = server.next_auth().await;
    computer
        .join_office("office-generation", "office-rejoin-generation")
        .await
        .expect("initial join");
    let _ = server.next_join().await;

    let mut events = computer.subscribe_events();
    server.force_network_disconnect();
    wait_for_lifecycle(&mut events, LifecycleState::Connecting).await;
    let _ = server.next_auth().await;
    assert_eq!(server.next_join().await["office_id"], "office-generation");

    // The second join ACK is still delayed. Invalidate that connection and allow the next
    // namespace generation to perform the sole state-restoring join.
    server.force_network_disconnect();
    let _ = server.next_auth().await;
    assert_eq!(server.next_join().await["office_id"], "office-generation");
    wait_for_lifecycle(&mut events, LifecycleState::JoinedOffice).await;
    sleep(Duration::from_millis(900)).await;

    assert_eq!(computer.lifecycle_state(), LifecycleState::JoinedOffice);
    let socketio = computer.get_socketio_client();
    let socketio = socketio.read().await;
    assert_eq!(
        socketio.as_ref().unwrap().get_office_id().await.as_deref(),
        Some("office-generation")
    );
    drop(socketio);
    assert!(
        timeout(Duration::from_millis(250), server.join_rx.recv())
            .await
            .is_err(),
        "a stale Connect task must not emit a duplicate join"
    );

    computer.shutdown().await.expect("shutdown computer");
    server.shutdown();
}

/// #204：手工 teardown 必须立即失效延迟中的自动重加入，且旧 client Arc 不得复活 Office。
#[tokio::test]
async fn disconnect_aborts_inflight_rejoin_and_clears_retained_client_state() {
    let _ = tracing_subscriber::fmt::try_init();
    let mut server = start_reconnect_capture_server(None, Some(2)).await;
    let temp = tempfile::TempDir::new().unwrap();
    let computer = Computer::new(
        "office-rejoin-disconnect",
        SilentSession::new("office-rejoin-disconnect-session"),
        None,
        None,
        false,
        false,
    )
    .with_skill_home(temp.path().join("skills"))
    .with_blob_cache_root(temp.path().join("blob"))
    .with_config_dir(temp.path().join("config"));
    computer.boot_up().await.expect("boot computer");
    computer
        .connect_socketio(&server.url, ConnectOptions::default())
        .await
        .expect("connect computer");
    let _ = server.next_auth().await;
    computer
        .join_office("office-disconnect", "office-rejoin-disconnect")
        .await
        .expect("initial join");
    let _ = server.next_join().await;

    let retained_client = {
        let socketio = computer.get_socketio_client();
        let client = socketio.read().await.as_ref().unwrap().clone();
        client
    };
    let mut events = computer.subscribe_events();
    server.force_network_disconnect();
    wait_for_lifecycle(&mut events, LifecycleState::Connecting).await;
    let _ = server.next_auth().await;
    let _ = server.next_join().await;

    computer
        .disconnect_socketio()
        .await
        .expect("manual disconnect while rejoin ACK is delayed");
    assert_eq!(computer.lifecycle_state(), LifecycleState::Started);
    assert_eq!(retained_client.get_office_id().await, None);

    // Let the server-side delayed ACK fire. The aborted, invalidated task must not commit it.
    sleep(Duration::from_millis(900)).await;
    assert_eq!(computer.lifecycle_state(), LifecycleState::Started);
    assert_eq!(retained_client.get_office_id().await, None);

    computer.shutdown().await.expect("shutdown computer");
    server.shutdown();
}

/// #204：被直接替换的公开 client Arc teardown 不得覆盖当前 Computer lifecycle。
#[tokio::test]
async fn stale_client_disconnect_is_idempotent_and_cannot_overwrite_new_connection_state() {
    let _ = tracing_subscriber::fmt::try_init();
    let mut server = start_reconnect_capture_server(None, None).await;
    let temp = tempfile::TempDir::new().unwrap();
    let computer = Computer::new(
        "office-stale-client",
        SilentSession::new("office-stale-client-session"),
        None,
        None,
        false,
        false,
    )
    .with_skill_home(temp.path().join("skills"))
    .with_blob_cache_root(temp.path().join("blob"))
    .with_config_dir(temp.path().join("config"));
    computer.boot_up().await.expect("boot computer");
    computer
        .connect_socketio(&server.url, ConnectOptions::default())
        .await
        .expect("connect first client");
    let _ = server.next_auth().await;
    computer
        .join_office("office-old", "office-stale-client")
        .await
        .expect("join with first client");
    let _ = server.next_join().await;
    let old_client = {
        let socketio = computer.get_socketio_client();
        let client = socketio.read().await.as_ref().unwrap().clone();
        client
    };

    // Replacing a live client is supported by the public API. Installation must retire the old
    // transport and revoke its lifecycle lease before publishing the replacement.
    computer
        .connect_socketio(&server.url, ConnectOptions::default())
        .await
        .expect("connect replacement client");
    let _ = server.next_auth().await;
    computer
        .join_office("office-new", "office-stale-client")
        .await
        .expect("join with replacement client");
    let _ = server.next_join().await;
    assert_eq!(computer.lifecycle_state(), LifecycleState::JoinedOffice);

    // connect_socketio must retire the previous namespace itself. Dropping/replacing the public
    // Arc is insufficient because tf-rust-socketio's reader retains an internal Client clone.
    server.wait_for_active_connections(1).await;
    assert_eq!(old_client.get_office_id().await, None);

    old_client
        .disconnect()
        .await
        .expect("replaced client teardown is idempotent");
    assert_eq!(computer.lifecycle_state(), LifecycleState::JoinedOffice);
    let current = computer.get_socketio_client();
    let current = current.read().await;
    assert_eq!(
        current.as_ref().unwrap().get_office_id().await.as_deref(),
        Some("office-new")
    );
    drop(current);

    old_client
        .disconnect()
        .await
        .expect("stale client disconnect remains idempotent");
    assert_eq!(computer.lifecycle_state(), LifecycleState::JoinedOffice);

    computer.shutdown().await.expect("shutdown computer");
    server.shutdown();
}

/// #204：公开 Builder 创建的 standalone client 在装入 Computer 时也必须接管 lifecycle。
#[tokio::test]
async fn public_set_socketio_client_binds_standalone_lifecycle_and_retires_previous_transport() {
    let _ = tracing_subscriber::fmt::try_init();
    let mut server = start_reconnect_capture_server(None, None).await;
    let temp = tempfile::TempDir::new().unwrap();
    let computer = Computer::new(
        "office-public-set",
        SilentSession::new("office-public-set-session"),
        None,
        None,
        false,
        false,
    )
    .with_skill_home(temp.path().join("skills"))
    .with_blob_cache_root(temp.path().join("blob"))
    .with_config_dir(temp.path().join("config"));
    computer.boot_up().await.expect("boot computer");
    computer
        .connect_socketio(&server.url, ConnectOptions::default())
        .await
        .expect("connect old Computer-owned client");
    let _ = server.next_auth().await;
    computer
        .join_office("office-public-old", "office-public-set")
        .await
        .expect("join old client");
    let _ = server.next_join().await;
    let old_client = {
        let socketio = computer.get_socketio_client();
        let client = socketio.read().await.as_ref().unwrap().clone();
        client
    };

    let standalone = Arc::new(
        SmcpComputerClientBuilder::new(
            &server.url,
            empty_manager(),
            "office-public-set",
            empty_inputs(),
        )
        .connect()
        .await
        .expect("connect standalone client"),
    );
    let _ = server.next_auth().await;
    computer
        .set_socketio_client(Arc::clone(&standalone))
        .await
        .expect("install standalone client");

    server.wait_for_active_connections(1).await;
    assert_eq!(old_client.get_office_id().await, None);
    assert_eq!(computer.lifecycle_state(), LifecycleState::Connected);

    computer
        .join_office("office-public-new", "office-public-set")
        .await
        .expect("join through installed standalone client");
    let _ = server.next_join().await;
    assert_eq!(computer.lifecycle_state(), LifecycleState::JoinedOffice);
    assert_eq!(
        standalone.get_office_id().await.as_deref(),
        Some("office-public-new")
    );

    old_client
        .disconnect()
        .await
        .expect("retired old client remains idempotent");
    assert_eq!(computer.lifecycle_state(), LifecycleState::JoinedOffice);

    computer
        .leave_office()
        .await
        .expect("leave standalone office");
    assert_eq!(computer.lifecycle_state(), LifecycleState::Connected);
    assert_eq!(standalone.get_office_id().await, None);

    computer.shutdown().await.expect("shutdown computer");
    server.shutdown();
}

/// #204：调用方取消安装 Future 后，owned transaction 仍须完成唯一 client 的发布。
#[tokio::test]
async fn cancelled_install_future_completes_owned_transaction_without_orphan_transport() {
    let _ = tracing_subscriber::fmt::try_init();
    let mut server = start_reconnect_capture_server(None, None).await;
    let temp = tempfile::TempDir::new().unwrap();
    let computer = Computer::new(
        "office-cancelled-connect",
        SilentSession::new("office-cancelled-connect-session"),
        None,
        None,
        false,
        false,
    )
    .with_skill_home(temp.path().join("skills"))
    .with_blob_cache_root(temp.path().join("blob"))
    .with_config_dir(temp.path().join("config"));
    computer.boot_up().await.expect("boot computer");

    let candidate = Arc::new(
        SmcpComputerClientBuilder::new(
            &server.url,
            empty_manager(),
            "office-cancelled-connect",
            empty_inputs(),
        )
        .connect()
        .await
        .expect("connect candidate"),
    );
    let _ = server.next_auth().await;

    let socketio_slot = computer.get_socketio_client();
    let slot_guard = socketio_slot.write().await;
    let connecting_computer = computer.clone();
    let connecting =
        tokio::spawn(async move { connecting_computer.set_socketio_client(candidate).await });
    sleep(Duration::from_millis(50)).await;
    connecting.abort();
    assert!(connecting.await.unwrap_err().is_cancelled());

    let mut events = computer.subscribe_events();
    drop(slot_guard);
    wait_for_lifecycle(&mut events, LifecycleState::Connected).await;
    server.wait_for_active_connections(1).await;
    assert!(computer.get_socketio_client().read().await.is_some());

    computer.shutdown().await.expect("shutdown computer");
    server.shutdown();
}

/// #204：同一个 client Arc 不能同时属于两个 Computer。
#[tokio::test]
async fn public_set_rejects_cross_computer_client_aliasing_without_touching_owner() {
    let _ = tracing_subscriber::fmt::try_init();
    let mut server = start_reconnect_capture_server(None, None).await;
    let temp_a = tempfile::TempDir::new().unwrap();
    let temp_b = tempfile::TempDir::new().unwrap();
    let computer_a = Computer::new(
        "office-owner-a",
        SilentSession::new("office-owner-a-session"),
        None,
        None,
        false,
        false,
    )
    .with_skill_home(temp_a.path().join("skills"))
    .with_blob_cache_root(temp_a.path().join("blob"))
    .with_config_dir(temp_a.path().join("config"));
    let computer_b = Computer::new(
        "office-owner-b",
        SilentSession::new("office-owner-b-session"),
        None,
        None,
        false,
        false,
    )
    .with_skill_home(temp_b.path().join("skills"))
    .with_blob_cache_root(temp_b.path().join("blob"))
    .with_config_dir(temp_b.path().join("config"));
    computer_a.boot_up().await.expect("boot computer A");
    computer_b.boot_up().await.expect("boot computer B");

    let client = Arc::new(
        SmcpComputerClientBuilder::new(
            &server.url,
            empty_manager(),
            "office-owner-a",
            empty_inputs(),
        )
        .connect()
        .await
        .expect("connect standalone client"),
    );
    let _ = server.next_auth().await;
    computer_a
        .set_socketio_client(Arc::clone(&client))
        .await
        .expect("install into owner A");
    assert!(computer_b
        .set_socketio_client(Arc::clone(&client))
        .await
        .is_err());
    computer_b.shutdown().await.expect("shutdown computer B");
    assert!(computer_b
        .set_socketio_client(Arc::clone(&client))
        .await
        .is_err());

    computer_a
        .join_office("office-owner-a", "office-owner-a")
        .await
        .expect("owner A remains operational");
    let _ = server.next_join().await;
    assert_eq!(computer_a.lifecycle_state(), LifecycleState::JoinedOffice);
    assert_eq!(computer_b.lifecycle_state(), LifecycleState::Shutdown);
    server.wait_for_active_connections(1).await;

    computer_a.shutdown().await.expect("shutdown computer A");
    server.shutdown();
}

/// #204：两个 Computer 并发安装同一 standalone Arc 时，client-local claim 必须恰好放行一个。
#[tokio::test]
async fn concurrent_cross_computer_install_atomically_claims_exactly_one_owner() {
    let _ = tracing_subscriber::fmt::try_init();
    let mut server = start_reconnect_capture_server(None, None).await;
    let temp_a = tempfile::TempDir::new().unwrap();
    let temp_b = tempfile::TempDir::new().unwrap();
    let computer_a = Computer::new(
        "office-claim-a",
        SilentSession::new("office-claim-a-session"),
        None,
        None,
        false,
        false,
    )
    .with_skill_home(temp_a.path().join("skills"))
    .with_blob_cache_root(temp_a.path().join("blob"))
    .with_config_dir(temp_a.path().join("config"));
    let computer_b = Computer::new(
        "office-claim-b",
        SilentSession::new("office-claim-b-session"),
        None,
        None,
        false,
        false,
    )
    .with_skill_home(temp_b.path().join("skills"))
    .with_blob_cache_root(temp_b.path().join("blob"))
    .with_config_dir(temp_b.path().join("config"));
    computer_a.boot_up().await.expect("boot computer A");
    computer_b.boot_up().await.expect("boot computer B");

    let client = Arc::new(
        SmcpComputerClientBuilder::new(
            &server.url,
            empty_manager(),
            "office-claim",
            empty_inputs(),
        )
        .connect()
        .await
        .expect("connect standalone client"),
    );
    let _ = server.next_auth().await;

    let barrier = Arc::new(Barrier::new(3));
    let task_a = {
        let barrier = Arc::clone(&barrier);
        let client = Arc::clone(&client);
        let computer = computer_a.clone();
        tokio::spawn(async move {
            barrier.wait().await;
            computer.set_socketio_client(client).await
        })
    };
    let task_b = {
        let barrier = Arc::clone(&barrier);
        let client = Arc::clone(&client);
        let computer = computer_b.clone();
        tokio::spawn(async move {
            barrier.wait().await;
            computer.set_socketio_client(client).await
        })
    };
    barrier.wait().await;
    let result_a = task_a.await.expect("installer A task");
    let result_b = task_b.await.expect("installer B task");

    assert_eq!(
        usize::from(result_a.is_ok()) + usize::from(result_b.is_ok()),
        1
    );
    assert_ne!(
        computer_a.get_socketio_client().read().await.is_some(),
        computer_b.get_socketio_client().read().await.is_some()
    );
    if result_a.is_ok() {
        computer_a.shutdown().await.expect("shutdown winner A");
        assert!(computer_b.set_socketio_client(client).await.is_err());
        computer_b.shutdown().await.expect("shutdown loser B");
    } else {
        computer_b.shutdown().await.expect("shutdown winner B");
        assert!(computer_a.set_socketio_client(client).await.is_err());
        computer_a.shutdown().await.expect("shutdown loser A");
    }
    server.shutdown();
}

/// #204：server 仅踢出 namespace 时，后续 replacement 仍必须关闭旧 Engine.IO transport。
#[tokio::test]
async fn replacement_closes_transport_after_server_namespace_disconnect() {
    let _ = tracing_subscriber::fmt::try_init();
    let mut server = start_reconnect_capture_server(None, None).await;
    let temp = tempfile::TempDir::new().unwrap();
    let computer = Computer::new(
        "office-server-disconnect",
        SilentSession::new("office-server-disconnect-session"),
        None,
        None,
        false,
        false,
    )
    .with_skill_home(temp.path().join("skills"))
    .with_blob_cache_root(temp.path().join("blob"))
    .with_config_dir(temp.path().join("config"));
    computer.boot_up().await.expect("boot computer");
    computer
        .connect_socketio(&server.url, ConnectOptions::default())
        .await
        .expect("connect old client");
    let _ = server.next_auth().await;

    server.force_namespace_disconnect();
    server.wait_for_active_connections(0).await;

    let replacement = Arc::new(
        SmcpComputerClientBuilder::new(
            &server.url,
            empty_manager(),
            "office-server-disconnect",
            empty_inputs(),
        )
        .connect()
        .await
        .expect("connect replacement"),
    );
    let _ = server.next_auth().await;
    computer
        .set_socketio_client(replacement)
        .await
        .expect("replace namespace-disconnected client");
    server.wait_for_active_connections(1).await;

    // Interrupt every proxy connection. Exactly the replacement may reconnect: before the fix,
    // the old wrapper returned early after namespace Close and emitted a second CONNECT auth.
    server.force_network_disconnect();
    let _ = server.next_auth().await;
    server.assert_no_additional_auth().await;
    server.wait_for_active_connections(1).await;

    computer.shutdown().await.expect("shutdown computer");
    server.shutdown();
}

/// #204：shutdown 后公开安装入口必须拒绝并关闭候选 transport。
#[tokio::test]
async fn public_set_after_shutdown_rejects_and_retires_candidate() {
    let _ = tracing_subscriber::fmt::try_init();
    let mut server = start_reconnect_capture_server(None, None).await;
    let temp = tempfile::TempDir::new().unwrap();
    let computer = Computer::new(
        "office-shutdown-set",
        SilentSession::new("office-shutdown-set-session"),
        None,
        None,
        false,
        false,
    )
    .with_skill_home(temp.path().join("skills"))
    .with_blob_cache_root(temp.path().join("blob"))
    .with_config_dir(temp.path().join("config"));
    computer.boot_up().await.expect("boot computer");

    let candidate = Arc::new(
        SmcpComputerClientBuilder::new(
            &server.url,
            empty_manager(),
            "office-shutdown-set",
            empty_inputs(),
        )
        .connect()
        .await
        .expect("connect candidate"),
    );
    let _ = server.next_auth().await;
    computer.shutdown().await.expect("shutdown computer");
    assert!(computer.set_socketio_client(candidate).await.is_err());
    server.wait_for_active_connections(0).await;
    assert!(computer.get_socketio_client().read().await.is_none());
    assert_eq!(computer.lifecycle_state(), LifecycleState::Shutdown);
    server.shutdown();
}

/// #204：shutdown 已进入终态但尚未清槽时，同 Arc 的幂等 fast path 也必须拒绝。
#[tokio::test]
async fn same_client_set_is_rejected_after_cancelled_shutdown_leaves_slot_populated() {
    let _ = tracing_subscriber::fmt::try_init();
    let mut server = start_reconnect_capture_server(None, None).await;
    let temp = tempfile::TempDir::new().unwrap();
    let computer = Computer::new(
        "office-cancelled-shutdown",
        SilentSession::new("office-cancelled-shutdown-session"),
        None,
        None,
        false,
        false,
    )
    .with_skill_home(temp.path().join("skills"))
    .with_blob_cache_root(temp.path().join("blob"))
    .with_config_dir(temp.path().join("config"));
    computer.boot_up().await.expect("boot computer");
    computer
        .connect_socketio(&server.url, ConnectOptions::default())
        .await
        .expect("connect client");
    let _ = server.next_auth().await;

    let slot = computer.get_socketio_client();
    let slot_guard = slot.write().await;
    let client = Arc::clone(slot_guard.as_ref().expect("installed client"));
    let shutting_down = {
        let computer = computer.clone();
        tokio::spawn(async move { computer.shutdown().await })
    };
    assert_eq!(
        wait_for(|| { (computer.lifecycle_state() == LifecycleState::Shutdown).then_some(()) })
            .await,
        Some(())
    );
    shutting_down.abort();
    assert!(shutting_down.await.unwrap_err().is_cancelled());
    drop(slot_guard);

    assert!(computer
        .set_socketio_client(Arc::clone(&client))
        .await
        .is_err());
    client.disconnect().await.expect("cleanup retained client");
    server.shutdown();
}

/// #204：旧 client 在底层 reconnect backoff 中被替换时，晚到的 namespace 必须立即退役。
#[tokio::test]
async fn replacement_retires_old_client_already_inside_reconnect_backoff() {
    let _ = tracing_subscriber::fmt::try_init();
    let mut server = start_reconnect_capture_server(None, None).await;
    let temp = tempfile::TempDir::new().unwrap();
    let computer = Computer::new(
        "office-replace-backoff",
        SilentSession::new("office-replace-backoff-session"),
        None,
        None,
        false,
        false,
    )
    .with_skill_home(temp.path().join("skills"))
    .with_blob_cache_root(temp.path().join("blob"))
    .with_config_dir(temp.path().join("config"));
    computer.boot_up().await.expect("boot computer");
    computer
        .connect_socketio(&server.url, ConnectOptions::default())
        .await
        .expect("connect old client");
    let _ = server.next_auth().await;
    computer
        .join_office("office-backoff-old", "office-replace-backoff")
        .await
        .expect("join old client");
    let _ = server.next_join().await;

    let mut events = computer.subscribe_events();
    server.force_network_disconnect();
    wait_for_lifecycle(&mut events, LifecycleState::Connecting).await;
    computer
        .connect_socketio(&server.url, ConnectOptions::default())
        .await
        .expect("replace while old client is backing off");
    let _ = server.next_auth().await;
    computer
        .join_office("office-backoff-new", "office-replace-backoff")
        .await
        .expect("join replacement client");
    let _ = server.next_join().await;

    // 0.8.1's already-entered retry loop will make one late CONNECT despite Manual. The retiring
    // callback closes it inline and the server converges back to the replacement only.
    let _ = server.next_auth().await;
    sleep(Duration::from_millis(250)).await;
    server.wait_for_active_connections(1).await;
    assert_eq!(computer.lifecycle_state(), LifecycleState::JoinedOffice);

    computer.shutdown().await.expect("shutdown computer");
    server.shutdown();
}

#[test]
fn connect_options_debug_redacts_auth_material() {
    let provider: SocketIoAuthProvider = Arc::new(|| Box::pin(async { json!({ "secret": 2 }) }));
    let options = ConnectOptions {
        auth_provider: Some(provider),
        auth_payload: Some(json!({ "token": "must-not-leak" })),
        ..Default::default()
    };

    let rendered = format!("{options:?}");
    assert!(rendered.contains("<provider>"));
    assert!(rendered.contains("<redacted>"));
    assert!(!rendered.contains("must-not-leak"));
}
