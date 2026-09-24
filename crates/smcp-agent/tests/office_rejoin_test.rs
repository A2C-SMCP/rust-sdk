/*!
* 文件名: office_rejoin_test
* 作者: JQQ
* 描述: Agent「自动重连后恢复 Office 成员关系」的**真设施**端到端测试（#219）/ Real-facility
*       end-to-end tests for office-membership recovery after an automatic reconnect.
*
* 为什么必须跑真设施（Phase 4 触发器 ②）：本特性的承重契约是**传输层**的——Socket.IO 房间成员关系
* 随会话销毁、自动重连产生新 SID、以及 `server:join_office` 在恢复路径上被拒后的有界退避。把
* transport 或 socket 换成 mock 就等于把被测契约本身换掉（同义反复）。
*
* 故此处起一台**裸 socketioxide** 服务端（真实 Socket.IO 栈）+ 一层 TCP proxy，用**真实 TCP 中断**
* 触发客户端底层自动重连；服务端侧的房间裁决语义可控（按到达序号接受 / 以指定协议码拒绝 / 延迟回
* ack），从而在同一套真设施上稳定复现四类恢复路径：
*
*   ① 重连后重放 `server:join_office` 并恢复已确认成员关系；
*   ② 撞上瞬态冲突（`4101` / `4105`）⇒ 退避重试后恢复；
*   ③ 预算耗尽 / 永久拒绝（`4106`）⇒ 清空意图、状态回退到 `Connected`、派发 lost 回调，且不再重试；
*   ④ 在途回房被第二次断连打断 ⇒ 陈旧 ack 不得落账（generation 守卫）。
*
* 另覆盖两条边界：无意图时不重放；Agent 换房须先显式退房（本地 `4106`，且**零请求**打到服务端）。
*
* 与「真实 SMCP 服务端」的入房 ack 契约测试（`room_ack_wiring_test.rs`）互为补充：那边钉协议报文与
* 错误码接线，这边钉**恢复路径的状态机**。
*/

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use http_body_util::Full;
use hyper::body::Bytes;
use serde_json::{json, Value};
use socketioxide::extract::{AckSender, Data, SocketRef, TryData};
use socketioxide::handler::ConnectHandler;
use socketioxide::SocketIo;
use tokio::io::copy_bidirectional;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::AbortHandle;
use tokio::time::{sleep, timeout};
use tower::Layer;

use smcp_agent::auth::{AgentConfig, AuthProvider};
use smcp_agent::{
    AsyncAgentEventHandler, AsyncSmcpAgent, DefaultAuthProvider, OfficeMembershipState,
    SmcpAgentConfig, SmcpAgentError,
};

/// 服务端对单次 `server:join_office` 的裁决 / the capture server's verdict for one join attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JoinOutcome {
    /// 回空 ack（协议 v0.5.0 的成功形态）。
    Accept,
    /// 回 flat `ErrorPayload`，`code` 取该协议码。
    Reject(i64),
}

/// 按「`server:join_office` 的到达序号（自 1 起）」给出裁决的策略。
type JoinPolicy = Arc<dyn Fn(usize) -> JoinOutcome + Send + Sync>;

fn policy(f: impl Fn(usize) -> JoinOutcome + Send + Sync + 'static) -> JoinPolicy {
    Arc::new(f)
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

/// 可事件驱动观测 CONNECT auth 与 `server:join_office`、并能用**真实 TCP 中断**触发自动重连的服务端。
struct RejoinCaptureServer {
    url: String,
    auth_rx: mpsc::UnboundedReceiver<Value>,
    join_rx: mpsc::UnboundedReceiver<Value>,
    calls_rx: mpsc::UnboundedReceiver<()>,
    /// 房间事件到达顺序（`"join"` / `"leave"`）——用于断言并发 join/leave 与服务端的全序一致。
    room_events_rx: mpsc::UnboundedReceiver<&'static str>,
    connection_tasks: Arc<Mutex<Vec<AbortHandle>>>,
    backend_shutdown_tx: oneshot::Sender<()>,
    proxy_shutdown_tx: oneshot::Sender<()>,
}

impl RejoinCaptureServer {
    /// 等下一次 namespace CONNECT（每次自动重连都会产生一次）。
    async fn next_auth(&mut self) -> Value {
        timeout(Duration::from_secs(15), self.auth_rx.recv())
            .await
            .expect("timed out waiting for Socket.IO CONNECT")
            .expect("capture server stopped before receiving auth")
    }

    /// 等下一次 `server:join_office` 载荷。
    async fn next_join(&mut self) -> Value {
        timeout(Duration::from_secs(15), self.join_rx.recv())
            .await
            .expect("timed out waiting for server:join_office")
            .expect("capture server stopped before receiving join_office")
    }

    /// 等下一次房间事件（`"join"` / `"leave"`）并返回其名称——按**到达顺序**。
    async fn next_room_event(&mut self) -> &'static str {
        timeout(Duration::from_secs(15), self.room_events_rx.recv())
            .await
            .expect("timed out waiting for a room event")
            .expect("capture server stopped before receiving a room event")
    }

    /// 反证：在给定时窗内**不得**再收到任何 `server:join_office`（用于「不再重试」的断言）。
    async fn assert_no_join_for(&mut self, window: Duration) {
        assert!(
            timeout(window, self.join_rx.recv()).await.is_err(),
            "expected no further server:join_office attempts"
        );
    }

    /// 中止当前所有客户端 TCP 连接（同时关掉 client / backend 两侧 socket）——制造**真实**网络断开，
    /// 同时让 Socket.IO 服务端可立即接受自动重连。（tf-rust-socketio 不为此提供测试接口，故走 proxy。）
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

/// 起一台 capture server：裸 socketioxide 后端 + TCP proxy（客户端只连 proxy）。
async fn start_rejoin_capture_server(
    policy: JoinPolicy,
    delay_attempt: Option<(usize, Duration)>,
) -> RejoinCaptureServer {
    start_capture_server(policy, delay_attempt, false).await
}

async fn start_capture_server(
    policy: JoinPolicy,
    delay_attempt: Option<(usize, Duration)>,
    disconnect_on_reconnect: bool,
) -> RejoinCaptureServer {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_addr = backend_listener.local_addr().unwrap();
    let (auth_tx, auth_rx) = mpsc::unbounded_channel();
    let (join_tx, join_rx) = mpsc::unbounded_channel();
    let (calls_tx, calls_rx) = mpsc::unbounded_channel();
    let connections = Arc::new(AtomicUsize::new(0));
    let (room_events_tx, room_events_rx) = mpsc::unbounded_channel();
    let join_attempts = Arc::new(AtomicUsize::new(0));

    let (layer, io) = SocketIo::new_layer();
    io.ns(
        "/smcp",
        move |_socket: SocketRef, TryData(auth): TryData<Value>| {
            let auth_tx = auth_tx.clone();
            let calls_tx = calls_tx.clone();
            let connections = connections.clone();
            let join_tx = join_tx.clone();
            let room_events_tx = room_events_tx.clone();
            let join_attempts = Arc::clone(&join_attempts);
            let policy = Arc::clone(&policy);
            async move {
                if let Ok(value) = auth {
                    let _ = auth_tx.send(value);
                }
                if disconnect_on_reconnect && connections.fetch_add(1, Ordering::SeqCst) > 0 {
                    _socket.disconnect().unwrap();
                    return;
                }
                // 故意不返回 ACK：用于验证旧连接关闭时在途调用被唤醒。
                _socket.on("client:get_tools", move || {
                    let calls_tx = calls_tx.clone();
                    async move {
                        let _ = calls_tx.send(());
                    }
                });
                // 房间事件顺序记录器的**独立副本**：下面两个 handler 各持一份（互不移动对方）。
                let leave_events_tx = room_events_tx.clone();
                _socket.on(
                    "server:join_office",
                    move |_socket: SocketRef, Data::<Value>(data), ack: AckSender| {
                        let join_tx = join_tx.clone();
                        let room_events_tx = room_events_tx.clone();
                        let join_attempts = Arc::clone(&join_attempts);
                        let policy = Arc::clone(&policy);
                        async move {
                            let attempt = join_attempts.fetch_add(1, Ordering::SeqCst) + 1;
                            let _ = join_tx.send(data.clone());
                            let _ = room_events_tx.send("join");
                            if let Some((nth, delay)) = delay_attempt {
                                if nth == attempt {
                                    sleep(delay).await;
                                }
                            }
                            match policy(attempt) {
                                JoinOutcome::Accept => {
                                    let _ = ack.send(&ZeroArgAck);
                                }
                                JoinOutcome::Reject(code) => {
                                    // flat ErrorPayload（协议 v0.5.0 的失败形态）：`code` 顶层平铺，
                                    // code-specific 上下文进 `details`。
                                    let _ = ack.send(&json!({
                                        "code": code,
                                        "message": "rejoin rejected",
                                        "details": {
                                            "office_id": data
                                                .get("office_id")
                                                .cloned()
                                                .unwrap_or(Value::Null),
                                        },
                                    }));
                                }
                            }
                        }
                    },
                );
                // 真实服务端对 `server:leave_office` 恒回空 ack（协议 v0.5.0：有/无房均幂等成功）。
                // 必须一并接线：否则该事件**无人 ack**，客户端会一直等（tf-rust-socketio 的 ack 超时
                // 只在 ack 到达时判过期），测试将挂死——这正是被测 SDK 需要自带真 deadline 的原因。
                _socket.on(
                    "server:leave_office",
                    move |_socket: SocketRef, Data::<Value>(_data), ack: AckSender| {
                        let room_events_tx = leave_events_tx.clone();
                        async move {
                            let _ = room_events_tx.send("leave");
                            let _ = ack.send(&ZeroArgAck);
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

    let proxy = start_tcp_proxy(backend_addr).await;
    RejoinCaptureServer {
        url: proxy.url,
        auth_rx,
        join_rx,
        calls_rx,
        room_events_rx,
        connection_tasks: proxy.connection_tasks,
        backend_shutdown_tx,
        proxy_shutdown_tx: proxy.shutdown_tx,
    }
}

struct TcpProxy {
    url: String,
    connection_tasks: Arc<Mutex<Vec<AbortHandle>>>,
    active_connections: Arc<AtomicUsize>,
    connection_closed: Arc<tokio::sync::Notify>,
    shutdown_tx: oneshot::Sender<()>,
}

/// 跟踪真实 TCP 存活数：取消任务、EOF、错误三种退出均计入连接释放。
struct ActiveConnection(Arc<AtomicUsize>, Arc<tokio::sync::Notify>);

impl Drop for ActiveConnection {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
        self.1.notify_one();
    }
}

async fn start_tcp_proxy(backend_addr: std::net::SocketAddr) -> TcpProxy {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let connection_tasks = Arc::new(Mutex::new(Vec::new()));
    let task_handles = Arc::clone(&connection_tasks);
    let active_connections = Arc::new(AtomicUsize::new(0));
    let active = Arc::clone(&active_connections);
    let connection_closed = Arc::new(tokio::sync::Notify::new());
    let closed = Arc::clone(&connection_closed);
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    if let Ok((mut downstream, _)) = accepted {
                        active.fetch_add(1, Ordering::SeqCst);
                        let connection = ActiveConnection(Arc::clone(&active), Arc::clone(&closed));
                        let task = tokio::spawn(async move {
                            let _connection = connection;
                            if let Ok(mut upstream) = TcpStream::connect(backend_addr).await {
                                let _ = copy_bidirectional(&mut downstream, &mut upstream).await;
                            }
                        });
                        task_handles.lock().unwrap().push(task.abort_handle());
                    }
                }
                _ = &mut shutdown_rx => break,
            }
        }
    });
    TcpProxy {
        url: format!("http://{addr}"),
        connection_tasks,
        active_connections,
        connection_closed,
        shutdown_tx,
    }
}

/// 外层取消发生在 namespace 等待期时，也必须释放此次尝试的真实 TCP 连接。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_namespace_connect_releases_uncommitted_transport() {
    let mut agent = agent_for("agent-cancel", "office-cancel", default_config());
    cancel_stalled_connect(&mut agent).await;
    assert_eq!(
        agent.office_membership(),
        OfficeMembershipState::Disconnected
    );
}

async fn cancel_stalled_connect(agent: &mut AsyncSmcpAgent) {
    let (proxy, _shutdown, entered) = start_stalled_namespace_server_with_signal().await;
    {
        let connecting = agent.connect(&proxy.url);
        tokio::pin!(connecting);
        tokio::select! {
            _ = entered.notified() => {},
            result = &mut connecting => panic!("stalled namespace unexpectedly completed: {result:?}"),
            _ = sleep(Duration::from_secs(120)) => panic!("namespace middleware was never reached"),
        }
        // 从已到达 namespace 中间件开始计时，避免启动/握手延迟导致尚未分配资源就取消的假绿。
        assert!(timeout(Duration::from_secs(1), &mut connecting)
            .await
            .is_err());
    }
    timeout(Duration::from_secs(2), async {
        while proxy.active_connections.load(Ordering::SeqCst) != 0 {
            proxy.connection_closed.notified().await;
        }
    })
    .await
    .expect("cancelled connect must release all uncommitted TCP streams");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_replacement_keeps_the_committed_connection_and_membership() {
    let server = start_rejoin_capture_server(policy(|_| JoinOutcome::Accept), None).await;
    let mut agent = agent_for("agent-keep-cancel", "office-keep-cancel", default_config());
    agent.connect(&server.url).await.unwrap();
    agent.join_office("agent-keep-cancel").await.unwrap();
    cancel_stalled_connect(&mut agent).await;
    assert_eq!(
        agent.confirmed_office_id().as_deref(),
        Some("office-keep-cancel")
    );
    agent.leave_office().await.unwrap();
    agent.join_office("agent-keep-cancel").await.unwrap();
    assert_eq!(
        agent.confirmed_office_id().as_deref(),
        Some("office-keep-cancel")
    );
    server.shutdown();
}

struct LeaveOnMembershipLoss(mpsc::UnboundedSender<Result<(), SmcpAgentError>>);

#[async_trait::async_trait]
impl AsyncAgentEventHandler for LeaveOnMembershipLoss {
    async fn on_office_membership_lost(
        &self,
        _office: &str,
        _reason: &str,
        agent: &AsyncSmcpAgent,
    ) -> Result<(), SmcpAgentError> {
        let _ = self.0.send(agent.leave_office().await);
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zero_budget_attempts_once_and_loss_hook_can_reenter_office_operations() {
    let mut server = start_rejoin_capture_server(
        policy(|attempt| {
            if attempt == 1 {
                JoinOutcome::Accept
            } else {
                JoinOutcome::Reject(4101)
            }
        }),
        None,
    )
    .await;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut agent = agent_for(
        "agent-hook",
        "office-hook",
        default_config().with_office_rejoin_budget_secs(0),
    )
    .with_event_handler(LeaveOnMembershipLoss(tx));
    agent.connect(&server.url).await.unwrap();
    agent.join_office("agent-hook").await.unwrap();
    server.next_join().await;
    server.force_network_disconnect();
    server.next_join().await;
    timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("loss callback must not hold the office operation lock")
        .unwrap()
        .unwrap();
    assert_eq!(agent.office_membership(), OfficeMembershipState::Connected);
    server.assert_no_join_for(Duration::from_millis(1200)).await;
    server.shutdown();
}

/// 显式入房等 ACK 占用操作门期间，恢复预算到期后不能继续发起重试。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejoin_retry_does_not_start_after_budget_spent_waiting_for_join() {
    let mut server = start_rejoin_capture_server(
        policy(|attempt| {
            if attempt == 1 {
                JoinOutcome::Accept
            } else {
                JoinOutcome::Reject(4101)
            }
        }),
        Some((3, Duration::from_secs(3))),
    )
    .await;
    let config = default_config().with_office_rejoin_budget_secs(2);
    let mut agent = agent_for("agent-deadline", "office-deadline", config);
    agent.connect(&server.url).await.unwrap();
    agent.join_office("agent-deadline").await.unwrap();
    server.next_join().await;
    server.force_network_disconnect();
    server.next_join().await; // 首次恢复被拒，进入 1s 退避。
    let explicit = {
        let agent = agent.clone();
        tokio::spawn(async move { agent.join_office("agent-deadline").await })
    };
    server.next_join().await; // 显式入房等 3s 后被拒，generation 不变。
    assert!(explicit.await.unwrap().is_err());
    server.assert_no_join_for(Duration::from_millis(500)).await;
    server.shutdown();
}

/// 轮询直到状态满足谓词（替代固定 sleep，时序一就绪即返回）。
///
/// 另起一台「transport 可达、但 `/smcp` namespace 中间件**永不完成**」的服务端，用于验证
/// `connect()` 的后置条件失败路径（namespace 会话未建立 ⇒ 如实报错并回滚）。
async fn start_stalled_namespace_server() -> (TcpProxy, oneshot::Sender<()>) {
    let (proxy, shutdown, _) = start_stalled_namespace_server_with_signal().await;
    (proxy, shutdown)
}

async fn start_stalled_namespace_server_with_signal(
) -> (TcpProxy, oneshot::Sender<()>, Arc<tokio::sync::Notify>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let (layer, io) = SocketIo::new_layer();
    // namespace 中间件挂起 ⇒ 服务端永不下发 namespace CONNECT 帧 ⇒ 客户端 `Event::Connect` 不触发。
    let entered = Arc::new(tokio::sync::Notify::new());
    let middleware_entered = Arc::clone(&entered);
    let middleware = move || {
        let entered = Arc::clone(&middleware_entered);
        async move {
            entered.notify_one();
            std::future::pending::<Result<(), std::convert::Infallible>>().await
        }
    };
    io.ns("/smcp", (|| {}).with(middleware));

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

    (start_tcp_proxy(addr).await, shutdown_tx, entered)
}

/// 轮询直到状态满足谓词（替代固定 sleep，时序一就绪即返回）。
async fn wait_until(mut probe: impl FnMut() -> bool, what: &str) {
    timeout(Duration::from_secs(15), async {
        loop {
            if probe() {
                return;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

fn agent_for(name: &str, office: &str, config: SmcpAgentConfig) -> AsyncSmcpAgent {
    let auth = DefaultAuthProvider::new(name.to_string(), office.to_string());
    AsyncSmcpAgent::new(auth, config)
}

fn default_config() -> SmcpAgentConfig {
    SmcpAgentConfig::new()
        .with_default_timeout(5)
        .with_office_rejoin_timeout(3)
}

/// 记录 `on_office_membership_lost` 派发的处理器 / handler recording membership-loss notifications.
#[derive(Default, Clone)]
struct LossRecorder {
    losses: Arc<Mutex<Vec<(String, String)>>>,
}

impl LossRecorder {
    fn count(&self) -> usize {
        self.losses.lock().unwrap().len()
    }

    fn offices(&self) -> Vec<String> {
        self.losses
            .lock()
            .unwrap()
            .iter()
            .map(|(office, _)| office.clone())
            .collect()
    }
}

#[async_trait::async_trait]
impl AsyncAgentEventHandler for LossRecorder {
    async fn on_office_membership_lost(
        &self,
        office_id: &str,
        reason: &str,
        _agent: &AsyncSmcpAgent,
    ) -> Result<(), SmcpAgentError> {
        self.losses
            .lock()
            .unwrap()
            .push((office_id.to_string(), reason.to_string()));
        Ok(())
    }
}

fn joined(office: &str) -> OfficeMembershipState {
    OfficeMembershipState::JoinedOffice {
        office_id: office.to_string(),
    }
}

/// ① 真实 TCP 中断 ⇒ 底层自动重连 ⇒ 重放 `server:join_office` ⇒ 成员关系恢复。
///
/// 用**多线程**运行时（生产同款）跑本用例：`connect()` 的「返回时首个 Connected 已绑定」这一后置
/// 条件只在多线程下才真的可能被违反（单线程 runtime 里放行信号与绑定同处一个任务、无并发窗口）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transport_reconnect_replays_join_and_restores_membership() {
    let _ = tracing_subscriber::fmt::try_init();
    let mut server = start_rejoin_capture_server(policy(|_| JoinOutcome::Accept), None).await;
    let mut agent = agent_for("agent-rejoin", "office-rejoin", default_config());
    agent.connect(&server.url).await.expect("connect agent");
    // 后置条件：`connect()` 返回时成员状态**已绑定**（否则紧随其后的 `join_office` 会捕获尚未绑定的
    // 会话标识，合法 ack 被会话校验丢弃）。
    assert_eq!(
        agent.office_membership(),
        OfficeMembershipState::Connected,
        "connect() 返回时首个 Connected 的状态绑定必须已完成"
    );
    let _ = server.next_auth().await;

    agent
        .join_office("agent-rejoin")
        .await
        .expect("initial join");
    assert_eq!(server.next_join().await["office_id"], "office-rejoin");
    assert_eq!(agent.office_membership(), joined("office-rejoin"));
    assert_eq!(
        agent.confirmed_office_id().as_deref(),
        Some("office-rejoin")
    );

    server.force_network_disconnect();

    // 自动重连：先重放 CONNECT auth，再重放入房。
    let _ = server.next_auth().await;
    let replay = server.next_join().await;
    assert_eq!(replay["office_id"], "office-rejoin");
    assert_eq!(replay["name"], "agent-rejoin");
    assert_eq!(replay["role"], "agent");
    wait_until(
        || agent.office_membership() == joined("office-rejoin"),
        "office membership restored after reconnect",
    )
    .await;

    server.shutdown();
}

/// ② 重连重放撞上**瞬态冲突**（`4101`）⇒ 有界退避重试后在预算内恢复。
#[tokio::test]
async fn transient_conflict_on_replay_is_retried_and_recovers() {
    let _ = tracing_subscriber::fmt::try_init();
    // 第 2 次 join（重连后的重放）以 4101 拒绝，第 3 次（退避重试）接受。
    let mut server = start_rejoin_capture_server(
        policy(|attempt| {
            if attempt == 2 {
                JoinOutcome::Reject(4101)
            } else {
                JoinOutcome::Accept
            }
        }),
        None,
    )
    .await;
    let mut agent = agent_for("agent-retry", "office-retry", default_config());
    agent.connect(&server.url).await.expect("connect agent");
    let _ = server.next_auth().await;
    agent
        .join_office("agent-retry")
        .await
        .expect("initial join");
    let _ = server.next_join().await;

    server.force_network_disconnect();
    let _ = server.next_auth().await;

    // 重放（第 2 次）被 4101 拒绝 ⇒ SDK 必须退避后再来一次（第 3 次）——这一次被接受。
    let rejected = server.next_join().await;
    let rejected_at = tokio::time::Instant::now();
    assert_eq!(rejected["office_id"], "office-retry");
    let retried = server.next_join().await;
    let retried_at = tokio::time::Instant::now();
    assert_eq!(
        retried["office_id"], "office-retry",
        "瞬态冲突后必须重放（单次尝试正是本特性的成因）"
    );
    // 退避曲线首档 1s：两次重放之间必须真的等过退避间隔（否则「退避」名不副实，成了紧循环重试）。
    let gap = retried_at - rejected_at;
    assert!(
        gap >= Duration::from_millis(900),
        "瞬态冲突后的重放 MUST 等过退避间隔（首档 1s），实测间隔 {gap:?}"
    );
    wait_until(
        || agent.confirmed_office_id().as_deref() == Some("office-retry"),
        "membership restored by the retried replay",
    )
    .await;

    server.shutdown();
}

/// ③ 预算耗尽 ⇒ 清空意图、状态回退 `Connected`、派发 lost 回调，且**不再**重试。
#[tokio::test]
async fn exhausted_budget_falls_back_to_connected_and_stops_retrying() {
    let _ = tracing_subscriber::fmt::try_init();
    // 首次入房接受；此后的每次重放都以 4105 拒绝。
    let mut server = start_rejoin_capture_server(
        policy(|attempt| {
            if attempt >= 2 {
                JoinOutcome::Reject(4105)
            } else {
                JoinOutcome::Accept
            }
        }),
        None,
    )
    .await;
    let recorder = LossRecorder::default();
    let config = default_config()
        .with_office_rejoin_timeout(1)
        // 预算 2s + 1s→2s 退避 ⇒ 恰好一次重试后耗尽：第 1 次重放后 `elapsed(<1s) + 1s ≤ 2s` 通过，
        // 第 2 次重放后 `elapsed(≥1s) + 2s > 2s` 判预算耗尽（单次尝试为下限，绝不无界重试）。
        .with_office_rejoin_budget_secs(2);
    let mut agent =
        agent_for("agent-budget", "office-budget", config).with_event_handler(recorder.clone());
    agent.connect(&server.url).await.expect("connect agent");
    let _ = server.next_auth().await;
    agent
        .join_office("agent-budget")
        .await
        .expect("initial join");
    let _ = server.next_join().await;

    server.force_network_disconnect();
    let _ = server.next_auth().await;
    // 第 2 次（重放）与第 3 次（唯一一次退避重试）都被拒。
    let _ = server.next_join().await;
    let _ = server.next_join().await;

    wait_until(
        || agent.office_membership() == OfficeMembershipState::Connected,
        "membership falls back to Connected after budget exhaustion",
    )
    .await;
    assert_eq!(
        agent.confirmed_office_id(),
        None,
        "回房失败后 MUST NOT 继续宣称在房"
    );

    // 派发「失去成员关系」回调（不静默）：恰一次、带房号。
    wait_until(|| recorder.count() == 1, "membership-lost hook").await;
    assert_eq!(recorder.offices(), vec!["office-budget".to_string()]);

    // 意图已清空 ⇒ 不得再有重放（负向断言）。
    server.assert_no_join_for(Duration::from_secs(2)).await;

    server.shutdown();
}

/// ③b 永久拒绝（`4106`）**不**重试：重试不改变结果，须立即回退并报错。
#[tokio::test]
async fn permanent_rejection_is_not_retried() {
    let _ = tracing_subscriber::fmt::try_init();
    let mut server = start_rejoin_capture_server(
        policy(|attempt| {
            if attempt == 2 {
                JoinOutcome::Reject(4106)
            } else {
                JoinOutcome::Accept
            }
        }),
        None,
    )
    .await;
    let recorder = LossRecorder::default();
    let mut agent = agent_for("agent-perm", "office-perm", default_config())
        .with_event_handler(recorder.clone());
    agent.connect(&server.url).await.expect("connect agent");
    let _ = server.next_auth().await;
    agent.join_office("agent-perm").await.expect("initial join");
    let _ = server.next_join().await;

    server.force_network_disconnect();
    let _ = server.next_auth().await;
    let _ = server.next_join().await;

    wait_until(
        || agent.office_membership() == OfficeMembershipState::Connected,
        "membership falls back to Connected after a permanent rejection",
    )
    .await;
    wait_until(|| recorder.count() == 1, "membership-lost hook").await;
    server.assert_no_join_for(Duration::from_secs(2)).await;

    server.shutdown();
}

/// ④ 在途回房被**第二次断连**打断 ⇒ 不回假在线、新会话另行重放并恢复。
///
/// 覆盖面说明（诚实标注）：真实 TCP 中断会同时掐掉 ack 的通路，故本用例断言的是**端到端行为**——
/// 第二次断连后不得残留「已入房」的假象、不得误报「失去成员关系」、新会话能自行恢复。「迟到成功 ack
/// 被丢弃」这条**提交点守卫本身**由 `office.rs::commit_join_is_discarded_when_the_session_was_replaced`
/// 系列单测钉死（去掉世代/epoch 校验即转红）。
#[tokio::test]
async fn in_flight_rejoin_is_aborted_by_the_next_disconnect() {
    let _ = tracing_subscriber::fmt::try_init();
    // 第 2 次 join 的 ack 延迟 1.5s 后才回（制造在途窗口），且延迟后**接受**——若陈旧结果被误采纳，
    // 状态会在旧会话上宣称在房。
    let mut server = start_rejoin_capture_server(
        policy(|_| JoinOutcome::Accept),
        Some((2, Duration::from_millis(1500))),
    )
    .await;
    let recorder = LossRecorder::default();
    let mut agent = agent_for("agent-stale", "office-stale", default_config())
        .with_event_handler(recorder.clone());
    agent.connect(&server.url).await.expect("connect agent");
    let _ = server.next_auth().await;
    agent
        .join_office("agent-stale")
        .await
        .expect("initial join");
    let _ = server.next_join().await;

    // 第一次断连 ⇒ 重连并发出在途重放（第 2 次 join，ack 被服务端拖延）。
    server.force_network_disconnect();
    let _ = server.next_auth().await;
    let in_flight = server.next_join().await;
    assert_eq!(in_flight["office_id"], "office-stale");

    // 在途期间再断一次：该重放所属会话已死 ⇒ 其（迟到的）成功 ack 不得落账。
    server.force_network_disconnect();
    wait_until(
        || agent.confirmed_office_id().is_none(),
        "membership withdrawn when the in-flight rejoin's session dies",
    )
    .await;

    // 新会话（第 3 次）重放并恢复。
    let _ = server.next_auth().await;
    let fresh = server.next_join().await;
    assert_eq!(fresh["office_id"], "office-stale");
    wait_until(
        || agent.confirmed_office_id().as_deref() == Some("office-stale"),
        "membership restored by the fresh session's replay",
    )
    .await;
    assert_eq!(
        recorder.count(),
        0,
        "被断连打断的回房不构成「失去」——新会话仍在自愈"
    );

    server.shutdown();
}

/// ❌B2 后置条件失败路径：namespace 会话未建立时，`connect()` 必须**如实失败并回滚**，
/// 不得放行「可继续使用的半初始化状态」。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn namespace_connect_timeout_fails_and_rolls_back() {
    let _ = tracing_subscriber::fmt::try_init();
    let (proxy, _shutdown) = start_stalled_namespace_server().await;
    let mut agent = agent_for("agent-stalled", "office-stalled", default_config());

    let error = agent
        .connect(&proxy.url)
        .await
        .expect_err("namespace 会话未建立时 connect MUST 失败，而非放行半初始化状态");
    assert!(
        matches!(error, SmcpAgentError::Connection(_)),
        "必须是连接类错误: {error:?}"
    );

    wait_until(
        || proxy.active_connections.load(Ordering::SeqCst) == 0,
        "failed connection must close all TCP streams",
    )
    .await;

    // 回滚：成员状态为 Disconnected，且槽位已清空（后续房间操作如实报「未连接」）。
    assert_eq!(
        agent.office_membership(),
        OfficeMembershipState::Disconnected
    );
    let join = agent
        .join_office("agent-stalled")
        .await
        .expect_err("回滚后不得仍能在该连接上发起房间请求");
    assert!(matches!(join, SmcpAgentError::Connection(_)), "{join:?}");
}

/// ❌B3 回归：已连接后再次 `connect()` **握手失败** ⇒ MUST NOT 破坏既有连接。
///
/// `connect()` 是**事务性**的：在后置条件成立之前不触碰既有槽位与后台 task，故失败的新尝试只丢弃本次
/// 新建的资源。本用例断言既有连接仍可用（房间请求照常）且其生命周期仍被消费（真实 TCP 中断后仍自动回房）。
#[tokio::test]
async fn failed_reconnect_keeps_the_existing_connection_usable() {
    let _ = tracing_subscriber::fmt::try_init();
    let mut server = start_rejoin_capture_server(policy(|_| JoinOutcome::Accept), None).await;
    let mut agent = agent_for("agent-keep", "office-keep", default_config());
    agent.connect(&server.url).await.expect("connect agent");
    let _ = server.next_auth().await;
    agent.join_office("agent-keep").await.expect("initial join");
    let _ = server.next_join().await;
    assert_eq!(agent.office_membership(), joined("office-keep"));

    // 不可达地址：握手阶段即失败（既有状态自始至终未被触碰）。
    agent
        .connect("http://127.0.0.1:9")
        .await
        .expect_err("不可达地址 MUST 连接失败");
    assert_eq!(
        agent.office_membership(),
        joined("office-keep"),
        "失败的新连接 MUST NOT 动既有成员状态"
    );

    // 既有连接仍可用：房间请求照常（同一会话、同一服务端）。
    agent.leave_office().await.expect("既有连接仍可发房间请求");
    agent
        .join_office("agent-keep")
        .await
        .expect("既有连接仍可重新入房");
    assert_eq!(server.next_join().await["office_id"], "office-keep");

    // 既有连接的生命周期仍被消费：真实 TCP 中断后仍会重放回房（证明旧生命周期 task 未被误停）。
    server.force_network_disconnect();
    let _ = server.next_auth().await;
    assert_eq!(server.next_join().await["office_id"], "office-keep");
    wait_until(
        || agent.confirmed_office_id().as_deref() == Some("office-keep"),
        "rejoin still works on the preserved connection",
    )
    .await;

    server.shutdown();
}

/// ❌B3 回归：已连接后再次 `connect()` **namespace 超时** ⇒ MUST NOT 破坏既有连接。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn namespace_timeout_on_reconnect_keeps_the_existing_connection_usable() {
    let _ = tracing_subscriber::fmt::try_init();
    let mut server = start_rejoin_capture_server(policy(|_| JoinOutcome::Accept), None).await;
    let mut agent = agent_for(
        "agent-keep-stalled",
        "office-keep-stalled",
        default_config(),
    );
    agent.connect(&server.url).await.expect("connect agent");
    let _ = server.next_auth().await;
    agent
        .join_office("agent-keep-stalled")
        .await
        .expect("initial join");
    let _ = server.next_join().await;

    // transport 可达、但 namespace 永不建立 ⇒ 新尝试超时失败；既有连接与成员状态必须原封不动。
    let (stalled_proxy, _stalled_shutdown) = start_stalled_namespace_server().await;
    agent
        .connect(&stalled_proxy.url)
        .await
        .expect_err("namespace 未建立时新的 connect MUST 失败");
    assert_eq!(
        agent.office_membership(),
        joined("office-keep-stalled"),
        "失败的新连接 MUST NOT 动既有成员状态"
    );
    agent.leave_office().await.expect("既有连接仍可发房间请求");
    assert_eq!(
        agent.office_membership(),
        OfficeMembershipState::Connected,
        "既有连接上退房成功 ⇒ 会话仍被服务端接受"
    );

    server.shutdown();
}

/// ❌B4 回归：并发 `leave_office` 必须与在途 `join_office` 在**操作门内**全序化。
///
/// 清意图若发生在取门之前，退房就能插进在途 join 的「发请求 → 落账」之间：那会作废一次**合法**的入房
/// 裁决（或反向造成「服务端已退房、本地仍报在房」）。本用例把第 2 次 join 的 ack 延迟 500ms 制造在途
/// 窗口再并发退房，断言：① 在途 join 的裁决**不被作废**（返回 Ok）；② 服务端最后收到的房间操作是
/// `leave`，且本地状态与之一致（不在房）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_leave_does_not_invalidate_an_in_flight_join() {
    let _ = tracing_subscriber::fmt::try_init();
    let mut server = start_rejoin_capture_server(
        policy(|_| JoinOutcome::Accept),
        Some((2, Duration::from_millis(500))),
    )
    .await;
    let mut agent = agent_for("agent-order", "office-order", default_config());
    agent.connect(&server.url).await.expect("connect agent");
    let _ = server.next_auth().await;
    agent
        .join_office("agent-order")
        .await
        .expect("initial join");
    assert_eq!(server.next_room_event().await, "join");

    // 第 2 次 join：服务端延迟 ack ⇒ 它**持门**等待。
    let joining = {
        let agent = agent.clone();
        tokio::spawn(async move { agent.join_office("agent-order").await })
    };
    assert_eq!(
        server.next_room_event().await,
        "join",
        "必须先观测到在途 join 到达服务端"
    );

    // 并发退房：必须在在途 join 完成后才清意图 / 发请求（否则会作废其合法裁决）。
    agent.leave_office().await.expect("concurrent leave");
    joining
        .await
        .expect("join task")
        .expect("在途 join 的合法裁决 MUST NOT 被并发退房作废");

    assert_eq!(
        server.next_room_event().await,
        "leave",
        "退房请求必须排在在途 join 之后到达服务端"
    );
    assert_eq!(
        agent.office_membership(),
        OfficeMembershipState::Connected,
        "服务端最后一次房间操作是 leave ⇒ 本地不得仍宣称在房"
    );

    server.shutdown();
}

/// 边界：从未入房（无意图）时，重连**不得**重放任何 join。
#[tokio::test]
async fn reconnect_without_intent_does_not_replay() {
    let _ = tracing_subscriber::fmt::try_init();
    let mut server = start_rejoin_capture_server(policy(|_| JoinOutcome::Accept), None).await;
    let mut agent = agent_for("agent-no-intent", "office-no-intent", default_config());
    agent.connect(&server.url).await.expect("connect agent");
    assert_eq!(agent.office_membership(), OfficeMembershipState::Connected);
    let _ = server.next_auth().await;

    server.force_network_disconnect();
    let _ = server.next_auth().await;
    server.assert_no_join_for(Duration::from_secs(2)).await;
    assert_eq!(agent.confirmed_office_id(), None);

    server.shutdown();
}

/// `office_id` 可在运行期切换的 provider（模拟「Agent 换房」的调用方）。
///
/// `AuthProvider::get_agent_config` 返回**引用**，故配置必须存放在稳定位置——用固定数组 + 原子下标，
/// 而不是锁内可变值（后者无法返回引用）。
struct SwitchableAuthProvider {
    configs: [AgentConfig; 2],
    index: Arc<AtomicUsize>,
}

impl SwitchableAuthProvider {
    /// 返回 `(provider, 切换柄)`——provider 会被移入 Agent，故切换柄单独交回测试。
    fn new(agent: &str, first: &str, second: &str) -> (Self, Arc<AtomicUsize>) {
        let index = Arc::new(AtomicUsize::new(0));
        (
            Self {
                configs: [
                    AgentConfig {
                        agent: agent.to_string(),
                        office_id: first.to_string(),
                    },
                    AgentConfig {
                        agent: agent.to_string(),
                        office_id: second.to_string(),
                    },
                ],
                index: Arc::clone(&index),
            },
            index,
        )
    }
}

impl AuthProvider for SwitchableAuthProvider {
    fn get_agent_config(&self) -> &AgentConfig {
        &self.configs[self.index.load(Ordering::SeqCst)]
    }
}

/// 边界（协议 room-model §Agent 加入规则 1）：Agent 换房 MUST 先显式退房。
///
/// 本地以 canonical `4106` 快速失败（`details.office_id` = **当前**所在房），且**零请求**打到服务端；
/// 随后显式 `leave_office` → `join_office` 的两步语义得以为继（不适用 Computer 的自动换房规则）。
#[tokio::test]
async fn room_switch_requires_an_explicit_leave_first() {
    let _ = tracing_subscriber::fmt::try_init();
    let mut server = start_rejoin_capture_server(policy(|_| JoinOutcome::Accept), None).await;
    let (auth, office_switch) =
        SwitchableAuthProvider::new("agent-switch", "office-one", "office-two");
    let mut agent = AsyncSmcpAgent::new(auth, default_config());
    agent.connect(&server.url).await.expect("connect agent");
    let _ = server.next_auth().await;
    agent
        .join_office("agent-switch")
        .await
        .expect("join first office");
    assert_eq!(server.next_join().await["office_id"], "office-one");

    // 调用方把配置切到另一个房后直接入房 ⇒ 本地 4106，且不得发出任何请求。
    office_switch.store(1, Ordering::SeqCst);
    let rejection = agent
        .join_office("agent-switch")
        .await
        .expect_err("换房必须先显式退房（协议 room-model §Agent 加入规则 1）");
    match &rejection {
        SmcpAgentError::Protocol(protocol) => {
            assert_eq!(protocol.code, 4106, "{protocol:?}");
            assert_eq!(protocol.message, "Agent already in another room");
            assert_eq!(
                protocol.details.get("office_id").and_then(Value::as_str),
                Some("office-one"),
                "4106 的 details.office_id 必须是**当前**所在房，而非被拒的目标房"
            );
        }
        other => panic!("expected protocol 4106 rejection, got {other:?}"),
    }
    server.assert_no_join_for(Duration::from_secs(1)).await;

    // 显式两步：先退旧房，再入新房。
    agent.leave_office().await.expect("explicit leave");
    agent
        .join_office("agent-switch")
        .await
        .expect("join second office after leave");
    assert_eq!(server.next_join().await["office_id"], "office-two");
    assert_eq!(agent.confirmed_office_id().as_deref(), Some("office-two"));

    server.shutdown();
}

/// 在途首次入房与连接替换必须全序：先取得的操作门覆盖请求、ACK 和落账。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connection_replacement_waits_for_in_flight_join() {
    let mut server = start_rejoin_capture_server(
        policy(|_| JoinOutcome::Accept),
        Some((1, Duration::from_millis(700))),
    )
    .await;
    let mut agent = agent_for("agent-publish", "office-publish", default_config());
    agent.connect(&server.url).await.unwrap();
    server.next_auth().await;
    let joining = {
        let agent = agent.clone();
        tokio::spawn(async move { agent.join_office("agent-publish").await })
    };
    server.next_join().await; // join 已发出，仍在等待 ACK
    agent.connect(&server.url).await.unwrap();
    joining
        .await
        .unwrap()
        .expect("replacement must not invalidate an in-flight join");
    let replay = server.next_join().await;
    assert_eq!(replay["office_id"], "office-publish");
    wait_until(
        || agent.confirmed_office_id().is_some(),
        "replacement membership",
    )
    .await;
    server.shutdown();
}

/// 主动关闭旧连接应立即唤醒在途非房间调用，不能继续等待永不到达的 ACK。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replacing_connection_interrupts_old_in_flight_calls() {
    let mut server = start_rejoin_capture_server(policy(|_| JoinOutcome::Accept), None).await;
    let mut agent = agent_for("agent-call", "office-call", default_config());
    agent.connect(&server.url).await.unwrap();
    let calling = {
        let agent = agent.clone();
        tokio::spawn(async move { agent.get_tools("computer").await })
    };
    timeout(Duration::from_secs(5), server.calls_rx.recv())
        .await
        .unwrap()
        .unwrap();
    agent.connect(&server.url).await.unwrap();
    let error = timeout(Duration::from_secs(1), calling)
        .await
        .expect("retired connection must wake in-flight calls")
        .unwrap()
        .unwrap_err();
    assert!(matches!(error, SmcpAgentError::Connection(_)), "{error:?}");
    server.shutdown();
}

/// 真实 Socket.IO 重连后立即被服务端断开，最终必须保持 Disconnected。
/// 回调乱序的确定性排列由状态机单测补充，避免靠调度概率验证守卫。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_disconnect_on_reconnect_does_not_leave_a_connected_session() {
    let mut server = start_capture_server(policy(|_| JoinOutcome::Accept), None, true).await;
    let mut agent = agent_for("agent-kicked", "office-kicked", default_config());
    agent.connect(&server.url).await.unwrap();
    server.next_auth().await;
    agent.join_office("agent-kicked").await.unwrap();
    server.next_join().await;
    server.force_network_disconnect();
    server.next_auth().await;
    wait_until(
        || agent.office_membership() == OfficeMembershipState::Disconnected,
        "server disconnect must be reflected",
    )
    .await;
    // 给已经派生的生命周期回调执行机会，检查没有迟到 Connect 复活状态。
    sleep(Duration::from_millis(500)).await;
    assert_eq!(
        agent.office_membership(),
        OfficeMembershipState::Disconnected
    );
    assert_eq!(agent.confirmed_office_id(), None);
    server.shutdown();
}

/// The synchronous facade must keep its runtime alive while idle, replay membership
/// after a real transport loss, and allow explicit leave to cancel recovery intent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_facade_replays_and_leave_cancels_recovery() {
    for reject_replay in [false, true] {
        let mut server = start_rejoin_capture_server(
            policy(move |attempt| {
                if reject_replay && attempt > 1 {
                    JoinOutcome::Reject(4101)
                } else {
                    JoinOutcome::Accept
                }
            }),
            None,
        )
        .await;
        let url = server.url.clone();
        let (ready_tx, ready_rx) = oneshot::channel();
        let (leave_tx, leave_rx) = oneshot::channel();
        let (left_tx, left_rx) = oneshot::channel();
        let (finish_tx, finish_rx) = oneshot::channel();
        let worker = tokio::task::spawn_blocking(move || {
            let auth = DefaultAuthProvider::new("sync-rejoin".into(), "sync-office".into());
            let mut agent = smcp_agent::SyncSmcpAgent::new(auth, default_config()).unwrap();
            agent.connect(&url).unwrap();
            agent.join_office("sync-rejoin").unwrap();
            assert_eq!(agent.confirmed_office_id().as_deref(), Some("sync-office"));
            ready_tx.send(()).unwrap();
            // No active block_on: background recovery must still run on the owned runtime.
            leave_rx.blocking_recv().unwrap();
            agent.leave_office().unwrap();
            assert_eq!(agent.office_membership(), OfficeMembershipState::Connected);
            left_tx.send(()).unwrap();
            finish_rx.blocking_recv().unwrap();
        });
        timeout(Duration::from_secs(10), ready_rx)
            .await
            .unwrap()
            .unwrap();
        server.next_auth().await;
        server.next_join().await;
        assert_eq!(server.next_room_event().await, "join");
        server.force_network_disconnect();
        server.next_auth().await;
        let replay = server.next_join().await;
        assert_eq!(replay["office_id"], "sync-office");
        assert_eq!(replay["name"], "sync-rejoin");
        assert_eq!(server.next_room_event().await, "join");
        leave_tx.send(()).unwrap();
        timeout(Duration::from_secs(10), left_rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(server.next_room_event().await, "leave");
        server.assert_no_join_for(Duration::from_secs(1)).await;
        finish_tx.send(()).unwrap();
        timeout(Duration::from_secs(10), worker)
            .await
            .unwrap()
            .unwrap();
        server.shutdown();
    }
}
