//! #148 —— 高层 disconnect / MCP 起停边界的**真链路**守护。
//!
//! 用一个**裸 socketioxide recording relay**（ACK `server:join_office` 使 Computer 达 joined，
//! 记录 connect / disconnect / `server:update_tool_list` 的 `{computer}`）替代「仅断言 lifecycle
//! 或内部 `Option`」——直接在 relay 侧观察 transport 与协议事件。这是 issue 点名的防假绿判据：
//! 现有 `config_runtime_regression.rs::disconnect` 仅测「未连接（client 已 None）」幂等，transport
//! 从未真正断开。
//!
//! 关键判别力：当前 `disconnect_socketio()`/`shutdown()` 仅把 `socketio_client` 置 `None`——
//! `tf_rust-socketio` 的 `Client` 背后还有 reader 后台任务持克隆，Drop 用户句柄**不会**关 transport，
//! 故 relay 在秒级内观察不到 disconnect；只有显式 `Client::disconnect()`（发 DISCONNECT 包）才触发。
//!
//! Problem 2 需 Node.js（`tests/echo-mcp-server`），故 `#[ignore]`：
//!   cargo test --package smcp-computer --test socketio_boundary_test -- --ignored --nocapture

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use http_body_util::Full;
use hyper::body::Bytes;
use serde_json::{json, Value};
use socketioxide::extract::{AckSender, Data, SocketRef};
use socketioxide::{SocketIo, SocketIoBuilder};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::{oneshot, Notify};
use tokio::time::{sleep, timeout};
use tower::Layer;

use smcp_computer::computer::{Computer, ConnectOptions, SilentSession};
use smcp_computer::mcp_clients::bundle_id::resolve_bundle_id;
use smcp_computer::mcp_clients::model::{
    MCPServerConfig, StdioServerConfig, StdioServerParameters,
};
use smcp_computer::mcp_clients::{HttpServerConfig, HttpServerParameters};
use smcp_computer::LifecycleState;

// #149 约定：common/mod.rs 是 cli-gated，非 cli 测试经 `#[path]` 引独立文件。
#[path = "common/blocking_tool_mock.rs"]
mod blocking_tool_mock;

// ---------------------------------------------------------------------------
// recording relay
// ---------------------------------------------------------------------------

/// relay 观测面：计数 + 收到的 `server:update_tool_list` 的 computer 名集合。
#[derive(Default)]
struct RelayObs {
    connect: AtomicU32,
    disconnect: AtomicU32,
    /// 每条收到的 `server:update_tool_list` 的 `{computer}` 名（按到达顺序）。
    tool_list_computers: Mutex<Vec<String>>,
}

impl RelayObs {
    fn tool_list_count_for(&self, computer: &str) -> usize {
        self.tool_list_computers
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.as_str() == computer)
            .count()
    }
}

/// 启动裸 socketioxide+hyper recording relay，返回 `(url, shutdown_tx)`。
///
/// - `/smcp` namespace（与 SMCP server 一致，Computer 默认可连）；
/// - ACK `server:join_office` 为 `(true, None)`——与真实 server `on_server_join_office` 返回同构，
///   使 Computer 的 `call`（emit-with-ack）解析为成功、达 joined 状态（`leave_office` 走 `emit` 无 ack）；
/// - 记录 connect / disconnect / `server:update_tool_list`。
async fn start_relay(obs: Arc<RelayObs>) -> (String, oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let (layer, io) = SocketIo::new_layer();
    io.ns("/smcp", {
        let obs = obs.clone();
        move |socket: SocketRef| {
            obs.connect.fetch_add(1, Ordering::SeqCst);

            socket.on_disconnect({
                let obs = obs.clone();
                move |_s: SocketRef| {
                    let obs = obs.clone();
                    async move {
                        obs.disconnect.fetch_add(1, Ordering::SeqCst);
                    }
                }
            });

            // ACK join 成功（mirror 真实 server 的 `(bool, Option<String>)` = `[true, null]`）。
            socket.on(
                "server:join_office",
                move |_s: SocketRef, _d: Data<Value>, ack: AckSender| async move {
                    let _ = ack.send(&(true, None::<String>));
                },
            );

            // #148 核心：记录 Computer 主动发的 `server:update_tool_list`（computer 名）。
            socket.on("server:update_tool_list", {
                let obs = obs.clone();
                move |_s: SocketRef, Data::<Value>(data)| {
                    let obs = obs.clone();
                    async move {
                        if let Some(c) = data.get("computer").and_then(|v| v.as_str()) {
                            obs.tool_list_computers.lock().unwrap().push(c.to_string());
                        }
                    }
                }
            });
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

    sleep(Duration::from_millis(150)).await;
    (format!("http://{addr}"), shutdown_tx)
}

/// #208 relay：与 [`start_relay`] 同构，但 Engine.IO ping 可配（issue 复现参数 300ms/700ms），
/// 并在命名空间连接时捕获 Computer 的 [`SocketRef`] —— 测试侧经 `emit_with_ack` 驱动
/// `client:tool_call` / `notify:tool_call_cancel`，即以真实入站事件在「长调用 pending」期间的
/// 分发时序为观测面（单 reader 阻塞期 PING/cancel 排队的判别点）。
async fn start_relay_pinged(
    obs: Arc<RelayObs>,
    sockets: Arc<Mutex<Option<SocketRef>>>,
    ping_interval: Duration,
    ping_timeout: Duration,
) -> (String, oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    // `SocketIo::new_layer()` 即 `SocketIoBuilder::new().build_layer()`；ping 参数为 #208 复现值。
    let (layer, io) = SocketIoBuilder::new()
        .ping_interval(ping_interval)
        .ping_timeout(ping_timeout)
        .build_layer();
    io.ns("/smcp", {
        let obs = obs.clone();
        move |socket: SocketRef| {
            *sockets.lock().unwrap() = Some(socket.clone());
            obs.connect.fetch_add(1, Ordering::SeqCst);

            socket.on_disconnect({
                let obs = obs.clone();
                move |_s: SocketRef| {
                    let obs = obs.clone();
                    async move {
                        obs.disconnect.fetch_add(1, Ordering::SeqCst);
                    }
                }
            });

            // ACK join 成功（mirror 真实 server 的 `(bool, Option<String>)` = `[true, null]`）。
            socket.on(
                "server:join_office",
                move |_s: SocketRef, _d: Data<Value>, ack: AckSender| async move {
                    let _ = ack.send(&(true, None::<String>));
                },
            );

            socket.on("server:update_tool_list", {
                let obs = obs.clone();
                move |_s: SocketRef, Data::<Value>(data)| {
                    let obs = obs.clone();
                    async move {
                        if let Some(c) = data.get("computer").and_then(|v| v.as_str()) {
                            obs.tool_list_computers.lock().unwrap().push(c.to_string());
                        }
                    }
                }
            });
        }
    });

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

    sleep(Duration::from_millis(150)).await;
    (format!("http://{addr}"), shutdown_tx)
}

/// 轮询 `cond` 直到为真或超时（消除固定 sleep 的时序脆弱）。
async fn wait_until<F, Fut>(mut cond: F, timeout: Duration) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if cond().await {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        sleep(Duration::from_millis(50)).await;
    }
}

const OBS_WAIT: Duration = Duration::from_secs(3);

/// 隔离一台待 boot 的 Computer（skill_home / blob / config_dir 全注入 tempdir，绝不污染 ~/.a2c）。
/// 镜像 `config_runtime_regression.rs::isolate_boot`。
fn isolate(c: Computer<SilentSession>, td: &TempDir) -> Computer<SilentSession> {
    c.with_skill_home(td.path().join("skills"))
        .with_blob_cache_root(td.path().join("blob"))
        .with_config_dir(td.path().join("config"))
}

fn echo_server_path() -> String {
    format!(
        "{}/../../tests/echo-mcp-server/index.js",
        env!("CARGO_MANIFEST_DIR")
    )
}

fn echo_config() -> MCPServerConfig {
    MCPServerConfig::Stdio(StdioServerConfig::new(
        "echo",
        StdioServerParameters {
            command: "node".to_string(),
            args: vec![echo_server_path()],
            env: HashMap::new(),
            cwd: None,
        },
    ))
}

// ---------------------------------------------------------------------------
// Problem 1：高层 disconnect / shutdown 真关 transport（默认套件，无外部依赖）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn disconnect_socketio_closes_transport_relay_observes() {
    let _ = tracing_subscriber::fmt().try_init();
    let obs = Arc::new(RelayObs::default());
    let (url, shutdown) = start_relay(obs.clone()).await;

    let td = TempDir::new().unwrap();
    let computer = isolate(
        Computer::new("c1", SilentSession::new("s"), None, None, false, false),
        &td,
    );
    computer.boot_up().await.expect("boot");
    computer
        .connect_socketio(&url, ConnectOptions::default())
        .await
        .expect("connect");
    // connect 真实到达 relay（不止 lifecycle）。
    assert!(
        wait_until(
            || async { obs.connect.load(Ordering::SeqCst) >= 1 },
            OBS_WAIT
        )
        .await,
        "relay 未观察到 connect"
    );
    computer.join_office("o", "c1").await.expect("join");

    // 高层 disconnect：relay 必须在有限时间内观察到 transport disconnect。
    // 当前红：置 None 不关 transport，reader 后台任务持克隆 ⇒ disconnect 恒 0。
    computer
        .disconnect_socketio()
        .await
        .expect("disconnect_socketio Ok");
    assert!(
        wait_until(
            || async { obs.disconnect.load(Ordering::SeqCst) >= 1 },
            OBS_WAIT
        )
        .await,
        "relay 未观察到 transport disconnect —— disconnect_socketio 没关 transport"
    );

    // 幂等：重复 disconnect 返回 Ok，且不产生新的 disconnect 事件。
    let after_first = obs.disconnect.load(Ordering::SeqCst);
    assert!(
        computer.disconnect_socketio().await.is_ok(),
        "重复 disconnect 应 Ok"
    );
    sleep(Duration::from_millis(500)).await;
    assert_eq!(
        obs.disconnect.load(Ordering::SeqCst),
        after_first,
        "重复 disconnect 应幂等（无新 disconnect 事件）"
    );
    assert_eq!(
        computer.lifecycle_state(),
        LifecycleState::Started,
        "disconnect 后 lifecycle 回 Started"
    );

    let _ = shutdown.send(());
}

#[tokio::test]
async fn shutdown_closes_transport_relay_observes() {
    let _ = tracing_subscriber::fmt().try_init();
    let obs = Arc::new(RelayObs::default());
    let (url, shutdown) = start_relay(obs.clone()).await;

    let td = TempDir::new().unwrap();
    let computer = isolate(
        Computer::new("c2", SilentSession::new("s"), None, None, false, false),
        &td,
    );
    computer.boot_up().await.expect("boot");
    computer
        .connect_socketio(&url, ConnectOptions::default())
        .await
        .expect("connect");
    assert!(
        wait_until(
            || async { obs.connect.load(Ordering::SeqCst) >= 1 },
            OBS_WAIT
        )
        .await,
        "relay 未观察到 connect"
    );
    computer.join_office("o", "c2").await.expect("join");

    // shutdown 也必须真关 transport（与 disconnect_socketio 同根 bug）。
    computer.shutdown().await.expect("shutdown");
    assert!(
        wait_until(
            || async { obs.disconnect.load(Ordering::SeqCst) >= 1 },
            OBS_WAIT
        )
        .await,
        "relay 未观察到 transport disconnect —— shutdown 没关 transport"
    );
    assert_eq!(
        computer.lifecycle_state(),
        LifecycleState::Shutdown,
        "shutdown 终态"
    );

    let _ = shutdown.send(());
}

// ---------------------------------------------------------------------------
// Problem 2：显式 MCP start/stop 同步 `server:update_tool_list`（#[ignore]，需 Node.js）
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "需 Node.js（tests/echo-mcp-server）：cargo test --package smcp-computer --test socketio_boundary_test -- --ignored"]
async fn start_stop_mcp_syncs_tool_list_when_joined() {
    let _ = tracing_subscriber::fmt().try_init();
    let obs = Arc::new(RelayObs::default());
    let (url, shutdown) = start_relay(obs.clone()).await;

    let cfg = echo_config();
    let bid = resolve_bundle_id(&cfg);
    let mut servers = HashMap::new();
    servers.insert("echo".to_string(), cfg);

    let td = TempDir::new().unwrap();
    let computer = isolate(
        Computer::new(
            "echo-comp",
            SilentSession::new("s"),
            None,
            Some(servers),
            false,
            false,
        ),
        &td,
    );
    computer.boot_up().await.expect("boot");
    computer
        .connect_socketio(&url, ConnectOptions::default())
        .await
        .expect("connect");
    computer.join_office("o", "echo-comp").await.expect("join");

    // 显式 start（不直接调 emit_update_tool_list）：joined 下 SDK 必须自动发 server:update_tool_list。
    // 当前红：start 只 bump revision，不发 ⇒ relay 对该 computer 计数 0。
    computer.start_mcp_client(&bid).await.expect("start mcp");
    assert!(
        wait_until(
            || async { obs.tool_list_count_for("echo-comp") >= 1 },
            Duration::from_secs(15),
        )
        .await,
        "start_mcp_client 后 relay 未收到 server:update_tool_list"
    );

    // 显式 stop：relay 应再收到一条。
    computer.stop_mcp_client(&bid).await.expect("stop mcp");
    assert!(
        wait_until(
            || async { obs.tool_list_count_for("echo-comp") >= 2 },
            Duration::from_secs(15),
        )
        .await,
        "stop_mcp_client 后 relay 未收到第二条 server:update_tool_list"
    );

    computer.shutdown().await.ok();
    let _ = shutdown.send(());
}

#[tokio::test]
#[ignore = "需 Node.js（tests/echo-mcp-server）"]
async fn start_stop_mcp_no_sync_when_not_joined() {
    let _ = tracing_subscriber::fmt().try_init();
    let obs = Arc::new(RelayObs::default());
    let (url, shutdown) = start_relay(obs.clone()).await;

    let cfg = echo_config();
    let bid = resolve_bundle_id(&cfg);
    let mut servers = HashMap::new();
    servers.insert("echo".to_string(), cfg);

    let td = TempDir::new().unwrap();
    let computer = isolate(
        Computer::new(
            "echo-comp-nj",
            SilentSession::new("s"),
            None,
            Some(servers),
            false,
            false,
        ),
        &td,
    );
    computer.boot_up().await.expect("boot");
    computer
        .connect_socketio(&url, ConnectOptions::default())
        .await
        .expect("connect");
    // 故意不 join：office_id None ⇒ 不应发旧 Office 消息。

    computer.start_mcp_client(&bid).await.expect("start mcp");
    // 给潜在 emit 一个窗口；joined guard 应阻止发信 ⇒ 计数仍 0。
    sleep(Duration::from_secs(1)).await;
    assert_eq!(
        obs.tool_list_count_for("echo-comp-nj"),
        0,
        "未加入 Office 时 start_mcp_client 不应发 server:update_tool_list"
    );

    computer.stop_mcp_client(&bid).await.expect("stop mcp");
    sleep(Duration::from_secs(1)).await;
    assert_eq!(
        obs.tool_list_count_for("echo-comp-nj"),
        0,
        "未加入 Office 时 stop_mcp_client 不应发 server:update_tool_list"
    );

    computer.shutdown().await.ok();
    let _ = shutdown.send(());
}

// ---------------------------------------------------------------------------
// Problem 3：#208 长工具调用阻塞入站循环 —— 心跳 / pending cancel / no-op / teardown
// ---------------------------------------------------------------------------
//
// 判别力（防假绿）：走**真实 socketioxide relay（短 ping 300ms/700ms）+ 真实 on_any 分发 +
// 可阻塞 Streamable HTTP mock 工具**。长调用 pending 期间 Engine.IO PING 与 cancel 事件的
// 可达性，恰是 tf-rust-socketio 单 reader 行内 await 回调（client.rs:508）的观测面；
// issue #208 因果对照即此形态（改调度边界后 20/20 绿）。
//
// 工具暴露名 = `{bundle_id}__{tool_name}`（manager.rs:6423）：bundle "blocking" + mock 工具
// "waiting" → "blocking__waiting"。

/// #208 测试基座：boot → connect → join → start blocking mock，返回
/// `(computer, relay_obs, sockets, release, shutdown_tx)`。
async fn setup_pending_computer() -> (
    Computer<SilentSession>,
    Arc<RelayObs>,
    Arc<Mutex<Option<SocketRef>>>,
    Arc<Notify>,
    oneshot::Sender<()>,
) {
    let obs = Arc::new(RelayObs::default());
    let sockets: Arc<Mutex<Option<SocketRef>>> = Arc::new(Mutex::new(None));
    let (port, release) = blocking_tool_mock::spawn_blocking_tool_mock().await;
    let (url, shutdown) = start_relay_pinged(
        obs.clone(),
        sockets.clone(),
        Duration::from_millis(300),
        Duration::from_millis(700),
    )
    .await;

    let cfg = MCPServerConfig::Http(HttpServerConfig::new(
        "blocking",
        HttpServerParameters {
            url: format!("http://127.0.0.1:{port}"),
            headers: HashMap::new(),
        },
    ));
    let bid = resolve_bundle_id(&cfg);
    let mut servers = HashMap::new();
    servers.insert("blocking".to_string(), cfg);

    let td = TempDir::new().unwrap();
    let computer = isolate(
        Computer::new(
            "c208",
            SilentSession::new("s"),
            None,
            Some(servers),
            false,
            false,
        ),
        &td,
    );
    computer.boot_up().await.expect("boot");
    computer
        .connect_socketio(&url, ConnectOptions::default())
        .await
        .expect("connect");
    computer.join_office("o", "c208").await.expect("join");
    computer.start_mcp_client(&bid).await.expect("start mcp");

    (computer, obs, sockets, release, shutdown)
}

/// 等 relay 捕获到 Computer 的 SocketRef（join 之后必然可达）。
async fn captured_socket(sockets: &Arc<Mutex<Option<SocketRef>>>) -> SocketRef {
    wait_until(|| async { sockets.lock().unwrap().is_some() }, OBS_WAIT)
        .await
        .then(|| sockets.lock().unwrap().clone())
        .flatten()
        .expect("relay 未捕获 Computer SocketRef")
}

#[tokio::test]
async fn tool_call_pending_heartbeat_survives() {
    let _ = tracing_subscriber::fmt().try_init();
    let (computer, obs, sockets, release, shutdown) = setup_pending_computer().await;
    let sock = captured_socket(&sockets).await;

    let req = json!({
        "agent": "a1", "req_id": "hb-1", "computer": "c208",
        "tool_name": "blocking__waiting", "params": {}, "timeout": 0
    });
    // timeout=0 → Computer 无超时（handle_tool_call_with_ack 语义），工具只等本测试释放。
    let ack_fut = sock
        .emit_with_ack::<Value, Value>("client:tool_call", &req)
        .expect("emit_with_ack 失败");

    // 工具 pending（未释放）跨越 ping_interval+ping_timeout（300+700ms）：不得心跳超时断连/重连
    // （#207 现场：alimama_login 121s、断连后工具完成才重连）。
    sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        obs.connect.load(Ordering::SeqCst),
        1,
        "长调用期间不得重连（connect 计数应稳定为 1）"
    );
    assert_eq!(
        obs.disconnect.load(Ordering::SeqCst),
        0,
        "长调用期间不得被心跳超时断开（#207 alimama_login 现场形态）"
    );

    // 释放 → 工具自然完成，正常补发成功 ACK。
    release.notify_waiters();
    let ack = timeout(Duration::from_secs(5), ack_fut)
        .await
        .expect("ack 未在 5s 内到达")
        .expect("ack err");
    assert_eq!(
        ack.get("isError").and_then(|v| v.as_bool()),
        Some(false),
        "工具应成功: {ack}"
    );
    assert_eq!(
        computer.lifecycle_state(),
        LifecycleState::JoinedOffice,
        "全过程连接/办公室状态应保持"
    );

    computer.shutdown().await.ok();
    let _ = shutdown.send(());
}

#[tokio::test]
async fn tool_call_cancel_consumed_while_pending() {
    let _ = tracing_subscriber::fmt().try_init();
    let (computer, _obs, sockets, release, shutdown) = setup_pending_computer().await;
    let sock = captured_socket(&sockets).await;

    let req = json!({
        "agent": "a1", "req_id": "c-1", "computer": "c208",
        "tool_name": "blocking__waiting", "params": {}, "timeout": 0
    });
    let ack_fut = sock
        .emit_with_ack::<Value, Value>("client:tool_call", &req)
        .expect("emit_with_ack 失败");

    // 确认工具已进入 pending（reader 已收到并（修复后）spawn），再广播取消。
    sleep(Duration::from_millis(300)).await;
    sock.emit(
        "notify:tool_call_cancel",
        &json!({"agent": "a1", "req_id": "c-1"}),
    )
    .expect("emit cancel");

    // 取消 ACK 必须在窗口内到达且**唯一**：isError=true + meta.a2c_cancelled / a2c_cancel_reason。
    let ack = timeout(Duration::from_secs(1), ack_fut)
        .await
        .expect("取消 ACK 未在 1s 内到达（pending 期间 cancel 事件排在长调用之后）")
        .expect("ack err");
    let meta = ack
        .get("meta")
        .unwrap_or_else(|| panic!("取消态应有 result-level meta: {ack}"));
    assert_eq!(
        meta.get("a2c_cancelled").and_then(|v| v.as_bool()),
        Some(true),
        "{ack}"
    );
    assert_eq!(
        meta.get("a2c_cancel_reason").and_then(|v| v.as_str()),
        Some("agent_requested"),
        "{ack}"
    );
    assert_eq!(
        ack.get("isError").and_then(|v| v.as_bool()),
        Some(true),
        "{ack}"
    );
    assert_eq!(
        computer.lifecycle_state(),
        LifecycleState::JoinedOffice,
        "取消全程连接保持；取消 ACK 可达即已取消（工具未自然完成）"
    );

    // 收尾：释放工具兜底（若取消已覆盖则无影响），防残留阻塞任务。
    release.notify_waiters();
    computer.shutdown().await.ok();
    let _ = shutdown.send(());
}

#[tokio::test]
async fn tool_call_cancel_unknown_req_id_noop() {
    let _ = tracing_subscriber::fmt().try_init();
    let (computer, _obs, sockets, release, shutdown) = setup_pending_computer().await;
    let sock = captured_socket(&sockets).await;

    let req = json!({
        "agent": "a1", "req_id": "u-1", "computer": "c208",
        "tool_name": "blocking__waiting", "params": {}, "timeout": 0
    });
    let mut ack_fut = Box::pin(
        sock.emit_with_ack::<Value, Value>("client:tool_call", &req)
            .expect("emit_with_ack 失败"),
    );

    sleep(Duration::from_millis(300)).await;
    // 未知 req_id → 幂等 no-op：不 panic、无 ACK，工具照常运行。
    sock.emit(
        "notify:tool_call_cancel",
        &json!({"agent": "a1", "req_id": "nope"}),
    )
    .expect("emit cancel");
    assert!(
        timeout(Duration::from_millis(500), &mut ack_fut)
            .await
            .is_err(),
        "未知 req_id 的取消不得提前产生 ACK"
    );

    // 释放 → 唯一成功 ACK（未被取消）。
    release.notify_waiters();
    let ack = timeout(Duration::from_secs(5), &mut ack_fut)
        .await
        .expect("ack 未在 5s 内到达")
        .expect("ack err");
    assert_eq!(
        ack.get("isError").and_then(|v| v.as_bool()),
        Some(false),
        "未知 req_id 取消应为 no-op，工具正常成功: {ack}"
    );
    assert_eq!(computer.lifecycle_state(), LifecycleState::JoinedOffice);

    computer.shutdown().await.ok();
    let _ = shutdown.send(());
}

#[tokio::test]
async fn disconnect_aborts_pending_tool_task() {
    let _ = tracing_subscriber::fmt().try_init();
    let (computer, obs, sockets, release, shutdown) = setup_pending_computer().await;
    let sock = captured_socket(&sockets).await;

    let req = json!({
        "agent": "a1", "req_id": "d-1", "computer": "c208",
        "tool_name": "blocking__waiting", "params": {}, "timeout": 0
    });
    let ack_fut = sock
        .emit_with_ack::<Value, Value>("client:tool_call", &req)
        .expect("emit_with_ack 失败");

    sleep(Duration::from_millis(300)).await; // 工具已 pending
    computer.disconnect_socketio().await.expect("disconnect");
    assert!(
        wait_until(
            || async { obs.disconnect.load(Ordering::SeqCst) >= 1 },
            OBS_WAIT
        )
        .await,
        "relay 未观察到 transport disconnect"
    );
    assert_eq!(
        computer.lifecycle_state(),
        LifecycleState::Started,
        "disconnect 后 lifecycle 回 Started"
    );

    // 释放后不得有延迟 ACK：disconnect 时在途任务应被 abort（无任务 → 无 ack 可投）。
    release.notify_waiters();
    let ack = timeout(Duration::from_secs(2), ack_fut).await;
    assert!(!matches!(&ack, Ok(Ok(_))), "断连后不应有延迟 ACK: {ack:?}");

    let _ = shutdown.send(());
}
