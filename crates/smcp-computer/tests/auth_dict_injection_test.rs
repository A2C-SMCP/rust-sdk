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
use std::time::{Duration, Instant};

use http_body_util::Full;
use hyper::body::Bytes;
use serde_json::{json, Value};
use socketioxide::extract::{AckSender, Data, SocketRef, TryData};
use socketioxide::handler::ConnectHandler;
use socketioxide::SocketIo;
use tokio::io::copy_bidirectional;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, Barrier, RwLock};
use tokio::task::AbortHandle;
use tokio::time::{sleep, timeout};
use tower::Layer;

use smcp_computer::computer::{Computer, ConnectOptions, SilentSession, SocketIoAuthProvider};
use smcp_computer::errors::ComputerError;
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

/// Start a real Engine.IO server whose Socket.IO namespace middleware never completes.
/// This isolates the post-transport/pre-namespace timeout branch without relying on wall-clock
/// sleeps or a mock client.
async fn start_stalled_namespace_server() -> (String, Arc<AtomicUsize>, oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let active_transports = Arc::new(AtomicUsize::new(0));

    let (layer, io) = SocketIo::new_layer();
    let middleware =
        || async { std::future::pending::<Result<(), std::convert::Infallible>>().await };
    io.ns("/smcp", (|| {}).with(middleware));

    let fallback = tower::service_fn(|_req: hyper::Request<hyper::body::Incoming>| async move {
        Ok::<_, std::convert::Infallible>(hyper::Response::new(Full::<Bytes>::new(Bytes::new())))
    });
    let service = layer.layer(fallback);
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
    let task_active_transports = Arc::clone(&active_transports);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    if let Ok((stream, _)) = accepted {
                        let active_transports = Arc::clone(&task_active_transports);
                        active_transports.fetch_add(1, Ordering::SeqCst);
                        let tio = hyper_util::rt::TokioIo::new(stream);
                        let svc = hyper_util::service::TowerToHyperService::new(service.clone());
                        tokio::spawn(async move {
                            let _ = hyper::server::conn::http1::Builder::new()
                                .serve_connection(tio, svc)
                                .with_upgrades()
                                .await;
                            active_transports.fetch_sub(1, Ordering::SeqCst);
                        });
                    }
                }
                _ = &mut shutdown_rx => break,
            }
        }
    });

    (format!("http://{addr}"), active_transports, shutdown_tx)
}

/// 可事件驱动观测 CONNECT auth、并通过关闭真实 TCP 连接触发底层自动重连的测试服务。
/// Event-driven CONNECT-auth capture server that can trigger transport auto-reconnect by closing
/// real TCP connections.
struct ReconnectCaptureServer {
    url: String,
    auth_rx: mpsc::UnboundedReceiver<Value>,
    join_rx: mpsc::UnboundedReceiver<Value>,
    join_ack_rx: mpsc::UnboundedReceiver<usize>,
    update_rx: mpsc::UnboundedReceiver<(String, Value)>,
    leave_rx: mpsc::UnboundedReceiver<Value>,
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

    async fn next_leave(&mut self) -> Value {
        timeout(Duration::from_secs(10), self.leave_rx.recv())
            .await
            .expect("timed out waiting for server:leave_office")
            .expect("capture server stopped before receiving leave_office")
    }

    async fn wait_for_join_ack(&mut self, expected_attempt: usize) {
        timeout(Duration::from_secs(10), async {
            loop {
                let attempt = self
                    .join_ack_rx
                    .recv()
                    .await
                    .expect("capture server stopped before sending join ACK");
                if attempt == expected_attempt {
                    return;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for join ACK attempt {expected_attempt}"));
    }

    /// 负向断言：在窗口内不得再出现 `server:join_office`（预算耗尽 / 永久拒绝后不再重试）。
    async fn assert_no_join_for(&mut self, window: Duration) {
        assert!(
            timeout(window, self.join_rx.recv()).await.is_err(),
            "expected no further server:join_office attempts"
        );
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

    async fn assert_no_additional_auth_after_convergence(&mut self) {
        // tf-rust-socketio 0.8.1 caps reconnect backoff at 5s. Wait beyond that policy boundary
        // only after the connection count has converged; this preserves the negative assertion
        // without racing the library's longest legitimate retry delay.
        assert!(
            timeout(Duration::from_secs(6), self.auth_rx.recv())
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

/// 零参 Socket.IO ack（线格式 `[]`），与 v0.5.0 服务端 `EmptyAck` 同款。
///
/// socketioxide 的 `AckSender::send` 只接受单个实参，`send(&())` 会被包成 1-tuple（`[null]`）；
/// 直接 `serialize_tuple(0)` 才能产出与 python-socketio 参考实现逐字节一致的零参 ACK。
struct ZeroArgAck;

impl serde::Serialize for ZeroArgAck {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeTuple;
        serializer.serialize_tuple(0)?.end()
    }
}

async fn start_reconnect_capture_server(
    reject_join_attempt: Option<usize>,
    delayed_join_attempt: Option<usize>,
) -> ReconnectCaptureServer {
    start_reconnect_capture_server_with_gate(reject_join_attempt, delayed_join_attempt, None).await
}

async fn start_reconnect_capture_server_with_gate(
    reject_join_attempt: Option<usize>,
    delayed_join_attempt: Option<usize>,
    gate: Option<Arc<tokio::sync::Notify>>,
) -> ReconnectCaptureServer {
    start_reconnect_capture_server_with_rejection(
        reject_join_attempt,
        delayed_join_attempt,
        gate,
        4101,
    )
    .await
}

async fn start_reconnect_capture_server_with_rejection(
    reject_join_attempt: Option<usize>,
    delayed_join_attempt: Option<usize>,
    gate: Option<Arc<tokio::sync::Notify>>,
    rejection_code: i64,
) -> ReconnectCaptureServer {
    start_reconnect_capture_server_with_policy(
        reject_attempt(reject_join_attempt, rejection_code),
        delayed_join_attempt,
        gate,
    )
    .await
}

/// 单次 join 裁决：`Some(code)` = flat ErrorPayload 拒绝；`None` = 零参 ACK 接受。
/// 次数按服务端收到的 `server:join_office` 递增（含显式 join 与自动回放）。
type JoinVerdict = Arc<dyn Fn(usize) -> Option<i64> + Send + Sync>;

fn join_verdict<F>(verdict: F) -> JoinVerdict
where
    F: Fn(usize) -> Option<i64> + Send + Sync + 'static,
{
    Arc::new(verdict)
}

fn reject_attempt(attempt: Option<usize>, code: i64) -> JoinVerdict {
    join_verdict(move |n| (attempt == Some(n)).then_some(code))
}

async fn start_reconnect_capture_server_with_policy(
    verdict: JoinVerdict,
    delayed_join_attempt: Option<usize>,
    gate: Option<Arc<tokio::sync::Notify>>,
) -> ReconnectCaptureServer {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_addr = backend_listener.local_addr().unwrap();
    let (auth_tx, auth_rx) = mpsc::unbounded_channel();
    let (update_tx, update_rx) = mpsc::unbounded_channel();
    let (leave_tx, leave_rx) = mpsc::unbounded_channel();
    let (join_tx, join_rx) = mpsc::unbounded_channel();
    let (join_ack_tx, join_ack_rx) = mpsc::unbounded_channel();
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
            let update_tx = update_tx.clone();
            let leave_tx = leave_tx.clone();
            let gate = gate.clone();
            let join_tx = join_tx.clone();
            let join_ack_tx = join_ack_tx.clone();
            let join_attempts = Arc::clone(&join_attempts);
            let verdict = Arc::clone(&verdict);
            let active_connections = Arc::clone(&namespace_active_connections);
            let namespace_socket = Arc::clone(&connected_namespace_socket);
            async move {
                for event in [
                    smcp::events::SERVER_UPDATE_CONFIG,
                    smcp::events::SERVER_UPDATE_TOOL_LIST,
                    smcp::events::SERVER_UPDATE_SKILLS,
                    smcp::events::SERVER_UPDATE_DESKTOP,
                ] {
                    let update_tx = update_tx.clone();
                    socket.on(event, move |Data::<Value>(value)| {
                        let _ = update_tx.send((event.to_string(), value));
                    });
                }
                socket.on(
                    smcp::events::SERVER_LEAVE_OFFICE,
                    move |Data::<Value>(value)| {
                        let _ = leave_tx.send(value);
                    },
                );
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
                        let gate = gate.clone();
                        let join_ack_tx = join_ack_tx.clone();
                        let join_attempts = Arc::clone(&join_attempts);
                        let verdict = Arc::clone(&verdict);
                        async move {
                            let attempt = join_attempts.fetch_add(1, Ordering::SeqCst) + 1;
                            let _ = join_tx.send(data);
                            if delayed_join_attempt == Some(attempt) {
                                if let Some(gate) = gate {
                                    gate.notified().await;
                                } else {
                                    sleep(Duration::from_millis(750)).await;
                                }
                            }
                            // v0.5.0 房间 ack 契约：成功 = **空 ack**（零参 ACK `[]`），失败 = flat
                            // ErrorPayload（顶层 `code`）。旧 `(success, message)` 元组形态已废除——
                            // 保留它会让客户端走**已删除**的兼容分支，使新增的 flat 拒绝路径零覆盖
                            // （#226 假绿清单第 5 条）。
                            if let Some(rejection_code) = verdict(attempt) {
                                let _ = ack.send(&json!({
                                    "code": rejection_code,
                                    "message": if rejection_code == 4101 { "Room already has a computer" } else { "Room already has an agent" },
                                    "details": { "office_id": "office-rejoin-rejected", "role": "computer" }
                                }));
                            } else {
                                let _ = ack.send(&ZeroArgAck);
                            }
                            let _ = join_ack_tx.send(attempt);
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
        join_ack_rx,
        update_rx,
        leave_rx,
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

#[tokio::test]
async fn configurable_namespace_connect_timeout_closes_stalled_transport() {
    let (url, active_transports, shutdown) = start_stalled_namespace_server().await;
    let result = timeout(Duration::from_secs(5), async move {
        SmcpComputerClientBuilder::new(url, empty_manager(), "namespace-timeout", empty_inputs())
            .namespace_connect_timeout(Duration::from_millis(250))
            .connect()
            .await
    })
    .await
    .expect("configured namespace timeout did not bound connect");
    assert!(matches!(
        result,
        Err(ComputerError::TimeoutError(message))
            if message.contains("namespace connect timed out")
    ));
    assert_eq!(
        wait_for(|| { (active_transports.load(Ordering::SeqCst) == 0).then_some(()) }).await,
        Some(()),
        "namespace timeout must close the underlying Engine.IO transport"
    );

    let _ = shutdown.send(());
}

#[tokio::test]
async fn namespace_readiness_timeout_disconnects_client_returned_by_transport_builder() {
    let (url, active_transports, shutdown) = start_stalled_namespace_server().await;
    let result = timeout(Duration::from_secs(5), async move {
        SmcpComputerClientBuilder::new(
            url,
            empty_manager(),
            "namespace-readiness-timeout",
            empty_inputs(),
        )
        .namespace("/missing")
        .namespace_connect_timeout(Duration::from_millis(250))
        .connect()
        .await
    })
    .await
    .expect("configured namespace readiness timeout did not bound connect");
    assert!(matches!(result, Err(ComputerError::TimeoutError(_))));
    assert_eq!(
        wait_for(|| { (active_transports.load(Ordering::SeqCst) == 0).then_some(()) }).await,
        Some(()),
        "readiness timeout must disconnect the client returned by the transport builder"
    );

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

/// #234：自动回房撞瞬态 4101 时有界退避重试后恢复成员关系（镜像 python#212）。
///
/// 服务端只拒绝第 2 次 join（重连回放）；第 3 次（1s 退避后的重试）接受。断言重试载荷
/// 与回放逐字节一致，且首档退避间隔真实生效。
#[tokio::test]
async fn transient_conflict_on_automatic_rejoin_is_retried_and_recovers() {
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
    let initial_join = server.next_join().await;

    let mut events = computer.subscribe_events();
    server.force_network_disconnect();
    wait_for_lifecycle(&mut events, LifecycleState::Connecting).await;
    let _ = server.next_auth().await;

    // 第 2 次 join = 重连自动回放，被服务端以 4101 拒绝；第 3 次 = 有界退避后的重试。
    let rejected_join = server.next_join().await;
    let rejected_at = Instant::now();
    // #231：退避窗口内标脏——重试路径不得清空 pending_updates，恢复后应补发一次。
    computer
        .emit_update_config()
        .await
        .expect("coalesce config update during rejoin backoff");
    // 未确认成员关系前不得提前 flush（负向断言；否则恢复后的断言会被提前发送掩盖）。
    assert!(
        timeout(Duration::from_millis(200), server.update_rx.recv())
            .await
            .is_err(),
        "pending updates must not flush before membership is confirmed"
    );
    let retried_join = server.next_join().await;
    let retry_delay = rejected_at.elapsed();

    assert_eq!(rejected_join, initial_join);
    assert_eq!(retried_join, rejected_join);
    // 下界锁定「确实退避过」；上界(3s) 容忍测试任务调度延迟，曲线的精确值由纯函数单测锁定。
    assert!(
        retry_delay >= Duration::from_millis(900) && retry_delay < Duration::from_secs(3),
        "first retry must respect the 1s backoff, got {retry_delay:?}"
    );

    wait_for_lifecycle(&mut events, LifecycleState::JoinedOffice).await;
    let socketio = computer.get_socketio_client();
    let socketio = socketio.read().await;
    assert_eq!(
        socketio.as_ref().unwrap().get_office_id().await.as_deref(),
        Some("office-rejoin-rejected")
    );
    drop(socketio);

    // #231：恢复后补发退避期间合并的配置更新（只发一次，载荷是本 Computer）。
    let (event, payload) = timeout(Duration::from_secs(2), server.update_rx.recv())
        .await
        .expect("rejoin must flush the coalesced update")
        .expect("capture server stopped before update");
    assert_eq!(event, smcp::events::SERVER_UPDATE_CONFIG);
    assert_eq!(payload["computer"], "office-rejoin-rejected");

    computer.shutdown().await.expect("shutdown computer");
    server.shutdown();
}

/// 启动带隔离 SKILL/blob/config 目录的 Computer（#234 回房矩阵共用）。
async fn boot_rejoin_computer(name: &str, temp: &tempfile::TempDir) -> Computer<SilentSession> {
    let computer = Computer::new(
        name,
        SilentSession::new(format!("{name}-session")),
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
}

/// #234：回房 ACK 在途时传输层再次失效——连接级中断不得清空 `desired`；
/// 下一次会话必须继续回放并恢复成员关系。
#[tokio::test]
async fn interrupted_rejoin_keeps_intent_for_next_session() {
    let _ = tracing_subscriber::fmt::try_init();
    let gate = Arc::new(tokio::sync::Notify::new());
    let mut server =
        start_reconnect_capture_server_with_gate(None, Some(2), Some(gate.clone())).await;
    let temp = tempfile::TempDir::new().unwrap();
    let computer = boot_rejoin_computer("office-rejoin-interrupted", &temp).await;
    computer
        .connect_socketio(
            &server.url,
            ConnectOptions {
                auth_payload: Some(json!({ "token": "office-rejoin-interrupted-token" })),
                ..Default::default()
            },
        )
        .await
        .expect("connect computer");
    let _ = server.next_auth().await;
    computer
        .join_office("office-rejoin-interrupted", "office-rejoin-interrupted")
        .await
        .expect("initial join");
    let _ = server.next_join().await;

    let mut events = computer.subscribe_events();
    server.force_network_disconnect();
    wait_for_lifecycle(&mut events, LifecycleState::Connecting).await;
    let _ = server.next_auth().await;
    // 第 2 次 join 到达服务端但 ACK 被 gate 扣住 → 客户端在 ACK 在途时再次断网。
    let in_flight = server.next_join().await;
    assert_eq!(in_flight["office_id"], "office-rejoin-interrupted");
    server.force_network_disconnect();

    // 新会话重连：desired 必须存活，自动回放第 3 次 join（不再延迟）并恢复。
    let _ = server.next_auth().await;
    let _ = server.next_join().await;
    wait_for_lifecycle(&mut events, LifecycleState::JoinedOffice).await;
    let socketio = computer.get_socketio_client();
    let socketio = socketio.read().await;
    assert_eq!(
        socketio.as_ref().unwrap().get_office_id().await.as_deref(),
        Some("office-rejoin-interrupted")
    );
    drop(socketio);

    gate.notify_waiters();
    server.assert_no_join_for(Duration::from_secs(2)).await;
    computer.shutdown().await.expect("shutdown computer");
    server.shutdown();
}

/// #234：退避窗口内显式 join 失败且此前无 confirmed 成员关系——显式取消无属主的旧意图，
/// 不留下「get_office_id=None、更新永远排队」的静默状态。
#[tokio::test]
async fn failed_explicit_join_during_rejoin_backoff_cancels_pending_intent() {
    let _ = tracing_subscriber::fmt::try_init();
    let mut server = start_reconnect_capture_server_with_policy(
        join_verdict(|attempt| (attempt >= 2).then_some(4101)),
        None,
        None,
    )
    .await;
    let temp = tempfile::TempDir::new().unwrap();
    let computer = boot_rejoin_computer("office-cancel-computer", &temp).await;
    computer
        .connect_socketio(
            &server.url,
            ConnectOptions {
                auth_payload: Some(json!({ "token": "office-cancel-token" })),
                ..Default::default()
            },
        )
        .await
        .expect("connect computer");
    let _ = server.next_auth().await;
    computer
        .join_office("office-rejoin-cancel", "office-cancel-computer")
        .await
        .expect("initial join");
    let _ = server.next_join().await;

    let mut events = computer.subscribe_events();
    server.force_network_disconnect();
    wait_for_lifecycle(&mut events, LifecycleState::Connecting).await;
    let _ = server.next_auth().await;
    let _ = server.next_join().await; // 第 2 次（auto）被 4101 拒绝，进入 1s 退避

    // 退避窗口内显式换房，同样被 4101 拒绝。自动重试与显式 join 存在调度竞态：
    // 按载荷（office_id）定位显式 join 的那一次，不依赖 1s 退避窗口。
    assert!(
        computer
            .join_office("office-cancel-target", "office-cancel-computer")
            .await
            .is_err(),
        "the explicit join must fail"
    );
    timeout(Duration::from_secs(5), async {
        loop {
            if server.next_join().await["office_id"] == "office-cancel-target" {
                break;
            }
        }
    })
    .await
    .expect("the explicit join must reach the server");

    // 醒来的旧回房任务 stale 退出，不产生后续 join。
    server.assert_no_join_for(Duration::from_secs(2)).await;

    // 再次断线重连：desired 已清空，不再回放。
    server.force_network_disconnect();
    let _ = server.next_auth().await;
    server.assert_no_join_for(Duration::from_secs(1)).await;

    computer.shutdown().await.expect("shutdown computer");
    server.shutdown();
}

/// #234：预算耗尽后回退 `Connected`，且不再发送重试（负向断言）。
#[tokio::test]
async fn exhausted_rejoin_budget_falls_back_to_connected_and_stops_retrying() {
    let _ = tracing_subscriber::fmt::try_init();
    let mut server = start_reconnect_capture_server_with_policy(
        join_verdict(|attempt| (attempt >= 2).then_some(4101)),
        None,
        None,
    )
    .await;
    let temp = tempfile::TempDir::new().unwrap();
    let computer = boot_rejoin_computer("office-rejoin-budget", &temp).await;
    computer
        .connect_socketio(
            &server.url,
            ConnectOptions {
                auth_payload: Some(json!({ "token": "office-rejoin-budget-token" })),
                office_rejoin_budget: Duration::from_secs(2),
                ..Default::default()
            },
        )
        .await
        .expect("connect computer");
    let _ = server.next_auth().await;
    computer
        .join_office("office-rejoin-budget", "office-rejoin-budget")
        .await
        .expect("initial join");
    let _ = server.next_join().await;

    let mut events = computer.subscribe_events();
    server.force_network_disconnect();
    wait_for_lifecycle(&mut events, LifecycleState::Connecting).await;
    let _ = server.next_auth().await;
    // 第 2 次（回放）被拒；在退避中标脏待发更新，终态失败必须清空而绝不补发。
    let _ = server.next_join().await;
    computer
        .emit_update_config()
        .await
        .expect("coalesce config update during exhausted backoff");
    // 第 3 次（1s 退避后）也被拒；下一档 2s 超出 2s 预算。
    let _ = server.next_join().await;

    wait_for_lifecycle(&mut events, LifecycleState::Connected).await;
    let socketio = computer.get_socketio_client();
    let socketio = socketio.read().await;
    assert_eq!(socketio.as_ref().unwrap().get_office_id().await, None);
    drop(socketio);

    // 预算 2s：若误重试，下一次会在 ~3s 发起；窗口取 4s 让该时刻落在窗口中部。
    server.assert_no_join_for(Duration::from_secs(4)).await;
    assert!(
        timeout(Duration::from_millis(500), server.update_rx.recv())
            .await
            .is_err(),
        "terminal rejoin failure must clear pending updates without flushing"
    );
    computer.shutdown().await.expect("shutdown computer");
    server.shutdown();
}

/// #234：`4106` 是永久失败，按码立即回退且不重试（默认 60s 预算下也一样）。
#[tokio::test]
async fn permanent_rejection_is_not_retried() {
    let _ = tracing_subscriber::fmt::try_init();
    let mut server = start_reconnect_capture_server_with_policy(
        join_verdict(|attempt| (attempt == 2).then_some(4106)),
        None,
        None,
    )
    .await;
    let temp = tempfile::TempDir::new().unwrap();
    let computer = boot_rejoin_computer("office-rejoin-permanent", &temp).await;
    computer
        .connect_socketio(
            &server.url,
            ConnectOptions {
                auth_payload: Some(json!({ "token": "office-rejoin-permanent-token" })),
                ..Default::default()
            },
        )
        .await
        .expect("connect computer");
    let _ = server.next_auth().await;
    computer
        .join_office("office-rejoin-permanent", "office-rejoin-permanent")
        .await
        .expect("initial join");
    let _ = server.next_join().await;

    let mut events = computer.subscribe_events();
    server.force_network_disconnect();
    wait_for_lifecycle(&mut events, LifecycleState::Connecting).await;
    let _ = server.next_auth().await;
    let _ = server.next_join().await; // 第 2 次 join 回 4106

    wait_for_lifecycle(&mut events, LifecycleState::Connected).await;
    let socketio = computer.get_socketio_client();
    let socketio = socketio.read().await;
    assert_eq!(socketio.as_ref().unwrap().get_office_id().await, None);
    drop(socketio);

    server.assert_no_join_for(Duration::from_secs(2)).await;
    computer.shutdown().await.expect("shutdown computer");
    server.shutdown();
}

/// #234：`Duration::ZERO` = 单次尝试（Builder setter 链路）。
#[tokio::test]
async fn zero_budget_makes_automatic_rejoin_single_attempt() {
    let _ = tracing_subscriber::fmt::try_init();
    let mut server = start_reconnect_capture_server(Some(2), None).await;
    let client = SmcpComputerClientBuilder::new(
        &server.url,
        empty_manager(),
        "office-rejoin-zero",
        empty_inputs(),
    )
    .office_rejoin_budget(Duration::ZERO)
    .connect()
    .await
    .expect("connect builder client");
    let _ = server.next_auth().await;
    client
        .join_office("office-rejoin-zero")
        .await
        .expect("initial join");
    let _ = server.next_join().await;

    server.force_network_disconnect();
    let _ = server.next_auth().await;
    let _ = server.next_join().await; // 第 2 次 join 被 4101 拒绝

    server.assert_no_join_for(Duration::from_secs(2)).await;
    assert_eq!(client.get_office_id().await, None);
    client.disconnect().await.expect("disconnect");
    server.shutdown();
}

/// #234：退避窗口内显式 `join_office` 不被阻塞（op-lock 分次持有）。
#[tokio::test]
async fn explicit_join_is_not_blocked_during_rejoin_backoff() {
    let _ = tracing_subscriber::fmt::try_init();
    let mut server = start_reconnect_capture_server(Some(2), None).await;
    let temp = tempfile::TempDir::new().unwrap();
    let computer = boot_rejoin_computer("office-rejoin-explicit", &temp).await;
    computer
        .connect_socketio(
            &server.url,
            ConnectOptions {
                auth_payload: Some(json!({ "token": "office-rejoin-explicit-token" })),
                ..Default::default()
            },
        )
        .await
        .expect("connect computer");
    let _ = server.next_auth().await;
    computer
        .join_office("office-rejoin-explicit", "office-rejoin-explicit")
        .await
        .expect("initial join");
    let _ = server.next_join().await;

    let mut events = computer.subscribe_events();
    server.force_network_disconnect();
    wait_for_lifecycle(&mut events, LifecycleState::Connecting).await;
    let _ = server.next_auth().await;
    let _ = server.next_join().await; // 自动回放被 4101 拒绝，进入 1s 退避

    timeout(
        Duration::from_millis(750),
        computer.join_office("office-explicit", "office-rejoin-explicit"),
    )
    .await
    .expect("explicit join must not wait for the backoff window")
    .expect("explicit join succeeds");
    let _ = server.next_join().await; // 显式 join（第 3 次）

    wait_for_lifecycle(&mut events, LifecycleState::JoinedOffice).await;
    let socketio = computer.get_socketio_client();
    let socketio = socketio.read().await;
    assert_eq!(
        socketio.as_ref().unwrap().get_office_id().await.as_deref(),
        Some("office-explicit")
    );
    drop(socketio);

    server.assert_no_join_for(Duration::from_secs(2)).await;
    computer.shutdown().await.expect("shutdown computer");
    server.shutdown();
}

/// #234：`Computer::with_office_rejoin_budget` 覆盖优先于 `ConnectOptions`。
///
/// `ConnectOptions` 传 ZERO（单次），覆盖传 2s：若覆盖失效，只会有第 2 次 join，
/// 第 3 次（1s 退避后）不会出现——本用例会超时失败。
#[tokio::test]
async fn computer_override_takes_precedence_over_connect_options() {
    let _ = tracing_subscriber::fmt::try_init();
    let mut server = start_reconnect_capture_server_with_policy(
        join_verdict(|attempt| (attempt >= 2).then_some(4101)),
        None,
        None,
    )
    .await;
    let temp = tempfile::TempDir::new().unwrap();
    let computer = boot_rejoin_computer("office-rejoin-override", &temp)
        .await
        .with_office_rejoin_budget(Duration::from_secs(2));
    computer
        .connect_socketio(
            &server.url,
            ConnectOptions {
                auth_payload: Some(json!({ "token": "office-rejoin-override-token" })),
                office_rejoin_budget: Duration::ZERO,
                ..Default::default()
            },
        )
        .await
        .expect("connect computer");
    let _ = server.next_auth().await;
    computer
        .join_office("office-rejoin-override", "office-rejoin-override")
        .await
        .expect("initial join");
    let _ = server.next_join().await;

    let mut events = computer.subscribe_events();
    server.force_network_disconnect();
    wait_for_lifecycle(&mut events, LifecycleState::Connecting).await;
    let _ = server.next_auth().await;
    let _ = server.next_join().await; // 第 2 次（回放）被拒
                                      // 覆盖生效才会出现的第 3 次 join（1s 退避后）；ConnectOptions 的 ZERO 若胜出则此步超时。
    let _ = server.next_join().await;

    wait_for_lifecycle(&mut events, LifecycleState::Connected).await;
    // 预算 2s：若覆盖失效后仍在重试，下一次会在 ~3s 发起；窗口取 4s 让该时刻落在窗口中。
    server.assert_no_join_for(Duration::from_secs(4)).await;
    computer.shutdown().await.expect("shutdown computer");
    server.shutdown();
}

/// #234：退避窗口内显式 `leave_office` 不被阻塞，且取走 abort 句柄后不再回放。
/// 走高层 `Computer::leave_office`（CLI `socket leave` 的入口），锁定退避期间退房可取消重试。
#[tokio::test]
async fn explicit_leave_is_not_blocked_during_rejoin_backoff() {
    let _ = tracing_subscriber::fmt::try_init();
    let mut server = start_reconnect_capture_server(Some(2), None).await;
    let temp = tempfile::TempDir::new().unwrap();
    let computer = boot_rejoin_computer("office-rejoin-leave", &temp).await;
    computer
        .connect_socketio(
            &server.url,
            ConnectOptions {
                auth_payload: Some(json!({ "token": "office-rejoin-leave-token" })),
                ..Default::default()
            },
        )
        .await
        .expect("connect computer");
    let _ = server.next_auth().await;
    computer
        .join_office("office-rejoin-leave", "office-rejoin-leave")
        .await
        .expect("initial join");
    let _ = server.next_join().await;

    let mut events = computer.subscribe_events();
    server.force_network_disconnect();
    wait_for_lifecycle(&mut events, LifecycleState::Connecting).await;
    let _ = server.next_auth().await;
    let _ = server.next_join().await; // 自动回放被 4101 拒绝，进入 1s 退避

    timeout(Duration::from_millis(750), computer.leave_office())
        .await
        .expect("explicit leave must not wait for the backoff window")
        .expect("explicit leave succeeds");
    // wire 断言：退房打出的就是回房意图里的目标房（由 office_operation 锁内解析）。
    assert_eq!(
        server.next_leave().await["office_id"],
        "office-rejoin-leave"
    );

    wait_for_lifecycle(&mut events, LifecycleState::Connected).await;
    server.assert_no_join_for(Duration::from_secs(2)).await;
    computer.shutdown().await.expect("shutdown computer");
    server.shutdown();
}

/// #234：并发换房与退房由 `office_operation` 全序化——leave 的 wire 目标必须是
/// 同一临界区内解析出的房号（旧房 A 或新房 B），不能是空值或第三个房号。
#[tokio::test]
async fn leave_during_concurrent_switch_targets_the_locked_office() {
    let _ = tracing_subscriber::fmt::try_init();
    let mut server = start_reconnect_capture_server(None, None).await;
    let temp = tempfile::TempDir::new().unwrap();
    let computer = boot_rejoin_computer("office-concurrent-leave", &temp).await;
    computer
        .connect_socketio(
            &server.url,
            ConnectOptions {
                auth_payload: Some(json!({ "token": "office-concurrent-token" })),
                ..Default::default()
            },
        )
        .await
        .expect("connect computer");
    let _ = server.next_auth().await;
    computer
        .join_office("office-a", "office-concurrent-leave")
        .await
        .expect("initial join");
    let _ = server.next_join().await;

    let (join_result, leave_result) = tokio::join!(
        computer.join_office("office-b", "office-concurrent-leave"),
        computer.leave_office(),
    );
    join_result.expect("concurrent explicit join");
    leave_result.expect("concurrent explicit leave");

    let leave = server.next_leave().await;
    let target = leave["office_id"].as_str().expect("leave office_id");
    assert!(
        target == "office-a" || target == "office-b",
        "leave must target the office resolved under the operation lock, got {target}"
    );

    computer.shutdown().await.expect("shutdown computer");
    server.shutdown();
}

/// #211 回归：退避窗口内二次断线——旧 task 被 Close abort，新会话重新回放并恢复。
#[tokio::test]
async fn second_disconnect_during_rejoin_backoff_rebinds_on_new_session() {
    let _ = tracing_subscriber::fmt::try_init();
    let mut server = start_reconnect_capture_server(Some(2), None).await;
    let temp = tempfile::TempDir::new().unwrap();
    let computer = boot_rejoin_computer("office-rejoin-rebind", &temp).await;
    computer
        .connect_socketio(
            &server.url,
            ConnectOptions {
                auth_payload: Some(json!({ "token": "office-rejoin-rebind-token" })),
                ..Default::default()
            },
        )
        .await
        .expect("connect computer");
    let _ = server.next_auth().await;
    computer
        .join_office("office-rejoin-rebind", "office-rejoin-rebind")
        .await
        .expect("initial join");
    let _ = server.next_join().await;

    let mut events = computer.subscribe_events();
    server.force_network_disconnect();
    wait_for_lifecycle(&mut events, LifecycleState::Connecting).await;
    let _ = server.next_auth().await;
    let _ = server.next_join().await; // 第 2 次 join 被 4101 拒绝，进入 1s 退避

    // 在退避窗口内再次制造真实断网：旧世代不得产生重复加入（Close 的 abort 或世代守卫），
    // 新会话重新回放并恢复。abort 与世代守卫在本观测下不可区分，故只断言「无重复加入」。
    server.force_network_disconnect();
    let _ = server.next_auth().await;
    let _ = server.next_join().await; // 第 3 次 join 在新会话上被接受

    wait_for_lifecycle(&mut events, LifecycleState::JoinedOffice).await;
    let socketio = computer.get_socketio_client();
    let socketio = socketio.read().await;
    assert_eq!(
        socketio.as_ref().unwrap().get_office_id().await.as_deref(),
        Some("office-rejoin-rebind")
    );
    drop(socketio);

    server.assert_no_join_for(Duration::from_secs(2)).await;
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
    server.wait_for_join_ack(2).await;

    assert_eq!(computer.lifecycle_state(), LifecycleState::JoinedOffice);
    let socketio = computer.get_socketio_client();
    let socketio = socketio.read().await;
    assert_eq!(
        socketio.as_ref().unwrap().get_office_id().await.as_deref(),
        Some("office-generation")
    );
    drop(socketio);
    assert!(
        server.join_rx.try_recv().is_err(),
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
    server.wait_for_join_ack(2).await;
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
    server.wait_for_active_connections(1).await;
    server.assert_no_additional_auth_after_convergence().await;

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

    // Teardown cancels the old reconnect owner. An attempt that completed before
    // replacement may already have queued auth, but a late CONNECT is not required.
    // Assert the lifecycle contract rather than depending on the old transport bug.
    server.wait_for_active_connections(1).await;
    while server.auth_rx.try_recv().is_ok() {}
    server.assert_no_additional_auth_after_convergence().await;
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

/// #230: a real namespace handshake/ACK window must not lose any of the four dirty event types.
#[tokio::test]
async fn pending_updates_coalesce_until_join_confirmation_and_clear_on_leave() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let mut server =
        start_reconnect_capture_server_with_gate(None, Some(1), Some(gate.clone())).await;
    let client = Arc::new(
        SmcpComputerClientBuilder::new(&server.url, empty_manager(), "dirty", empty_inputs())
            .auth_payload(json!({"token":"dirty"}))
            .connect()
            .await
            .unwrap(),
    );
    let joining = {
        let client = client.clone();
        tokio::spawn(async move { client.join_office("office-dirty").await })
    };
    server.next_join().await;
    for _ in 0..3 {
        client.emit_update_config().await.unwrap();
        client.emit_update_tool_list().await.unwrap();
        client.emit_update_skills().await.unwrap();
        client.emit_update_desktop().await.unwrap();
    }
    assert!(
        server.update_rx.try_recv().is_err(),
        "unconfirmed membership must not emit"
    );
    gate.notify_one();
    joining.await.unwrap().unwrap();
    let mut events = std::collections::HashSet::new();
    for _ in 0..4 {
        let (event, payload) = timeout(Duration::from_secs(3), server.update_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(payload["computer"], "dirty");
        assert!(events.insert(event), "updates must coalesce");
    }
    assert!(timeout(Duration::from_millis(100), server.update_rx.recv())
        .await
        .is_err());
    client.leave_office("office-dirty").await.unwrap();
    client.emit_update_config().await.unwrap();
    client.join_office("office-new").await.unwrap();
    assert!(
        timeout(Duration::from_millis(100), server.update_rx.recv())
            .await
            .is_err(),
        "old intent leaked into new office"
    );
    client.disconnect().await.unwrap();
    server.shutdown();
}

#[tokio::test]
async fn pending_updates_flush_after_real_transport_rejoin() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let mut server =
        start_reconnect_capture_server_with_gate(None, Some(2), Some(gate.clone())).await;
    let client = SmcpComputerClientBuilder::new(
        &server.url,
        empty_manager(),
        "rejoin-dirty",
        empty_inputs(),
    )
    .auth_payload(json!({"token":"dirty"}))
    .connect()
    .await
    .unwrap();
    client.join_office("office-dirty").await.unwrap();
    server.next_join().await;
    server.force_network_disconnect();
    server.next_join().await;
    client.emit_update_config().await.unwrap();
    client.emit_update_tool_list().await.unwrap();
    client.emit_update_skills().await.unwrap();
    client.emit_update_desktop().await.unwrap();
    assert!(server.update_rx.try_recv().is_err());
    gate.notify_one();
    let mut events = std::collections::HashSet::new();
    for _ in 0..4 {
        let (event, _) = timeout(Duration::from_secs(3), server.update_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(events.insert(event));
    }
    assert_eq!(
        client.get_office_id().await.as_deref(),
        Some("office-dirty")
    );
    client.disconnect().await.unwrap();
    server.shutdown();
}

#[tokio::test]
async fn pending_updates_survive_rejected_same_office_join() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let mut server =
        start_reconnect_capture_server_with_gate(Some(2), Some(2), Some(gate.clone())).await;
    let client = Arc::new(
        SmcpComputerClientBuilder::new(&server.url, empty_manager(), "same-office", empty_inputs())
            .auth_payload(json!({"token":"dirty"}))
            .connect()
            .await
            .unwrap(),
    );
    client.join_office("office-dirty").await.unwrap();
    server.next_join().await;
    let joining = {
        let client = client.clone();
        tokio::spawn(async move { client.join_office("office-dirty").await })
    };
    server.next_join().await;
    client.emit_update_config().await.unwrap();
    client.emit_update_skills().await.unwrap();
    gate.notify_one();
    assert!(joining.await.unwrap().is_err());
    for _ in 0..2 {
        timeout(Duration::from_secs(3), server.update_rx.recv())
            .await
            .unwrap()
            .unwrap();
    }
    assert_eq!(
        client.get_office_id().await.as_deref(),
        Some("office-dirty")
    );
    client.disconnect().await.unwrap();
    server.shutdown();
}

#[tokio::test]
async fn cancelled_join_retains_updates_for_next_confirmation() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let mut server =
        start_reconnect_capture_server_with_gate(None, Some(1), Some(gate.clone())).await;
    let client = Arc::new(
        SmcpComputerClientBuilder::new(
            &server.url,
            empty_manager(),
            "cancel-dirty",
            empty_inputs(),
        )
        .connect()
        .await
        .unwrap(),
    );
    let joining = {
        let client = client.clone();
        tokio::spawn(async move { client.join_office("office-dirty").await })
    };
    server.next_join().await;
    client.emit_update_config().await.unwrap();
    joining.abort();
    assert!(joining.await.unwrap_err().is_cancelled());
    assert_eq!(client.get_office_id().await, None);
    client.emit_update_skills().await.unwrap();
    assert!(server.update_rx.try_recv().is_err());
    gate.notify_one();
    client.join_office("office-dirty").await.unwrap();
    let mut events = std::collections::HashSet::new();
    for _ in 0..2 {
        let (event, _) = timeout(Duration::from_secs(3), server.update_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(events.insert(event));
    }
    client.disconnect().await.unwrap();
    server.shutdown();
}

#[tokio::test]
async fn pending_updates_survive_rejected_cross_office_join() {
    let gate = Arc::new(tokio::sync::Notify::new());
    // 跨房换房被拒：protocol#66 下目标房席位被占的现实拒绝码是 4101（4105 已转预留码）。
    let mut server =
        start_reconnect_capture_server_with_rejection(Some(2), Some(2), Some(gate.clone()), 4101)
            .await;
    let client = Arc::new(
        SmcpComputerClientBuilder::new(
            &server.url,
            empty_manager(),
            "cross-office",
            empty_inputs(),
        )
        .auth_payload(json!({"token":"dirty"}))
        .connect()
        .await
        .unwrap(),
    );
    client.join_office("office-dirty").await.unwrap();
    server.next_join().await;
    let joining = {
        let client = client.clone();
        tokio::spawn(async move { client.join_office("office-conflict").await })
    };
    server.next_join().await;
    client.emit_update_config().await.unwrap();
    client.emit_update_skills().await.unwrap();
    client.emit_update_tool_list().await.unwrap();
    client.emit_update_desktop().await.unwrap();
    client.emit_update_config().await.unwrap();
    assert!(server.update_rx.try_recv().is_err());
    gate.notify_one();
    assert!(matches!(
        joining.await.unwrap(),
        Err(ComputerError::ProtocolRejection { code: 4101, .. })
    ));
    let mut events = std::collections::HashSet::new();
    for _ in 0..4 {
        let (event, _) = timeout(Duration::from_secs(3), server.update_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(events.insert(event), "updates must coalesce");
    }
    assert!(timeout(Duration::from_millis(100), server.update_rx.recv())
        .await
        .is_err());
    assert_eq!(
        client.get_office_id().await.as_deref(),
        Some("office-dirty")
    );
    client.disconnect().await.unwrap();
    server.shutdown();
}
