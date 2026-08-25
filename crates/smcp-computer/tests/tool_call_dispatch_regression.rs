//! Regression suite for Socket.IO inbound dispatch while a tool call is pending.
//!
//! All four tests deliberately enter through the real socketioxide relay and
//! `tf-rust-socketio` `on_any` path. The fake MCP client is injected only at the
//! final MCP boundary, where it provides deterministic start/release/cancel signals.
//! Since tf-rust-socketio 0.9.0 dispatches independent events concurrently, the
//! suite verifies delivery and the tool-call/cancel causal boundary rather than
//! imposing a cross-event completion order.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use http_body_util::Full;
use hyper::body::Bytes;
use rmcp::model::{CallToolResult, ReadResourceResult, Resource, Tool};
use serde_json::{json, Value};
use smcp::{AgentCallData, ReqId, ToolCallReq};
use smcp_computer::computer::{Computer, ConnectOptions, SilentSession};
use smcp_computer::mcp_clients::bundle_id::resolve_bundle_id;
use smcp_computer::mcp_clients::manager::ClientFactory;
use smcp_computer::mcp_clients::model::{
    CancellableCallOutcome, ClientState, Content, MCPClientError, MCPClientProtocol,
    MCPServerConfig, StdioServerConfig, StdioServerParameters,
};
use socketioxide::extract::{AckSender, Data, SocketRef};
use socketioxide::SocketIo;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, Notify};
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tower::Layer;

const COMPUTER: &str = "dispatch-computer";
const AGENT: &str = "dispatch-agent";
const OFFICE: &str = "dispatch-office";
const TOOL: &str = "controlled";
const EXPOSED_TOOL: &str = "dispatch-fake__controlled";

#[derive(Debug, Clone, PartialEq, Eq)]
enum FakeEvent {
    Started(String),
    NaturalCompletion(String),
    Cancelled(String),
    Dropped(String),
}

struct ActiveGuard {
    active: Arc<AtomicUsize>,
    events: mpsc::UnboundedSender<FakeEvent>,
    call: String,
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
        let _ = self.events.send(FakeEvent::Dropped(self.call.clone()));
    }
}

struct ControlledClient {
    release: Arc<Notify>,
    events: mpsc::UnboundedSender<FakeEvent>,
    active: Arc<AtomicUsize>,
    cancelled: Arc<AtomicUsize>,
    natural: Arc<AtomicUsize>,
}

impl ControlledClient {
    fn call_name(params: &Value) -> String {
        params
            .get("call")
            .and_then(Value::as_str)
            .unwrap_or("unnamed")
            .to_string()
    }

    fn is_fast(params: &Value) -> bool {
        params.get("fast").and_then(Value::as_bool) == Some(true)
    }

    fn started(&self, call: &str) -> ActiveGuard {
        self.active.fetch_add(1, Ordering::SeqCst);
        let _ = self.events.send(FakeEvent::Started(call.to_string()));
        ActiveGuard {
            active: Arc::clone(&self.active),
            events: self.events.clone(),
            call: call.to_string(),
        }
    }
}

#[async_trait::async_trait]
impl MCPClientProtocol for ControlledClient {
    fn state(&self) -> ClientState {
        ClientState::Connected
    }

    async fn connect(&self) -> Result<(), MCPClientError> {
        Ok(())
    }

    async fn disconnect(&self) -> Result<(), MCPClientError> {
        Ok(())
    }

    async fn list_tools(&self) -> Result<Vec<Tool>, MCPClientError> {
        let schema: serde_json::Map<String, Value> =
            serde_json::from_value(json!({"type": "object"})).unwrap();
        Ok(vec![Tool::new(
            TOOL.to_string(),
            "controlled asynchronous tool",
            Arc::new(schema),
        )])
    }

    async fn call_tool(
        &self,
        _tool_name: &str,
        params: Value,
    ) -> Result<CallToolResult, MCPClientError> {
        let call = Self::call_name(&params);
        let _guard = self.started(&call);
        if !Self::is_fast(&params) {
            self.release.notified().await;
        }
        self.natural.fetch_add(1, Ordering::SeqCst);
        let _ = self.events.send(FakeEvent::NaturalCompletion(call.clone()));
        Ok(CallToolResult::success(vec![Content::text(format!(
            "completed:{call}"
        ))]))
    }

    async fn call_tool_cancellable(
        &self,
        _tool_name: &str,
        params: Value,
        cancel: CancellationToken,
    ) -> Result<CancellableCallOutcome, MCPClientError> {
        let call = Self::call_name(&params);
        let _guard = self.started(&call);
        if Self::is_fast(&params) {
            self.natural.fetch_add(1, Ordering::SeqCst);
            let _ = self.events.send(FakeEvent::NaturalCompletion(call.clone()));
            return Ok(CancellableCallOutcome::Completed(CallToolResult::success(
                vec![Content::text(format!("completed:{call}"))],
            )));
        }

        tokio::select! {
            _ = self.release.notified() => {
                self.natural.fetch_add(1, Ordering::SeqCst);
                let _ = self.events.send(FakeEvent::NaturalCompletion(call.clone()));
                Ok(CancellableCallOutcome::Completed(CallToolResult::success(vec![
                    Content::text(format!("completed:{call}")),
                ])))
            }
            _ = cancel.cancelled() => {
                self.cancelled.fetch_add(1, Ordering::SeqCst);
                let _ = self.events.send(FakeEvent::Cancelled(call));
                Ok(CancellableCallOutcome::Cancelled)
            }
        }
    }

    async fn list_windows(&self) -> Result<Vec<Resource>, MCPClientError> {
        Ok(vec![])
    }

    async fn list_resources_page(
        &self,
        _cursor: Option<String>,
    ) -> Result<(Vec<Resource>, Option<String>), MCPClientError> {
        Ok((vec![], None))
    }

    async fn get_window_detail(
        &self,
        _resource: Resource,
    ) -> Result<ReadResourceResult, MCPClientError> {
        Err(MCPClientError::ProtocolError("not used".into()))
    }

    async fn subscribe_window(&self, _resource: Resource) -> Result<(), MCPClientError> {
        Ok(())
    }

    async fn unsubscribe_window(&self, _resource: Resource) -> Result<(), MCPClientError> {
        Ok(())
    }
}

struct FakeControl {
    client: Arc<ControlledClient>,
    events: mpsc::UnboundedReceiver<FakeEvent>,
}

impl FakeControl {
    fn new() -> Self {
        let (events_tx, events) = mpsc::unbounded_channel();
        Self {
            client: Arc::new(ControlledClient {
                release: Arc::new(Notify::new()),
                events: events_tx,
                active: Arc::new(AtomicUsize::new(0)),
                cancelled: Arc::new(AtomicUsize::new(0)),
                natural: Arc::new(AtomicUsize::new(0)),
            }),
            events,
        }
    }

    async fn wait_for(&mut self, expected: FakeEvent, timeout: Duration) -> bool {
        tokio::time::timeout(timeout, async {
            while let Some(event) = self.events.recv().await {
                if event == expected {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false)
    }
}

#[derive(Default)]
struct RelayObs {
    connect: AtomicU32,
    disconnect: AtomicU32,
    join: AtomicU32,
    sids: Mutex<Vec<String>>,
    socket: Mutex<Option<SocketRef>>,
    client_packets: Mutex<Vec<String>>,
    connected: Notify,
}

#[derive(Default)]
struct ClientWireObserver {
    http_request: Vec<u8>,
    websocket_frames: Vec<u8>,
    websocket: bool,
}

impl ClientWireObserver {
    fn observe(&mut self, bytes: &[u8], obs: &RelayObs) {
        if self.websocket {
            self.websocket_frames.extend_from_slice(bytes);
            self.drain_websocket_frames(obs);
            return;
        }

        self.http_request.extend_from_slice(bytes);
        let Some(header_end) = self
            .http_request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|index| index + 4)
        else {
            return;
        };
        let headers =
            String::from_utf8_lossy(&self.http_request[..header_end]).to_ascii_lowercase();
        if headers.contains("upgrade: websocket") {
            self.websocket = true;
            self.websocket_frames
                .extend_from_slice(&self.http_request[header_end..]);
            self.drain_websocket_frames(obs);
        }
        self.http_request.clear();
    }

    fn drain_websocket_frames(&mut self, obs: &RelayObs) {
        loop {
            if self.websocket_frames.len() < 2 {
                return;
            }
            let second = self.websocket_frames[1];
            let masked = second & 0x80 != 0;
            let mut header_len = 2;
            let mut payload_len = usize::from(second & 0x7f);
            if payload_len == 126 {
                if self.websocket_frames.len() < 4 {
                    return;
                }
                payload_len = usize::from(u16::from_be_bytes([
                    self.websocket_frames[2],
                    self.websocket_frames[3],
                ]));
                header_len = 4;
            } else if payload_len == 127 {
                if self.websocket_frames.len() < 10 {
                    return;
                }
                let len = u64::from_be_bytes(self.websocket_frames[2..10].try_into().unwrap());
                let Ok(len) = usize::try_from(len) else {
                    self.websocket_frames.clear();
                    return;
                };
                payload_len = len;
                header_len = 10;
            }
            let mask_len = if masked { 4 } else { 0 };
            let frame_len = header_len + mask_len + payload_len;
            if self.websocket_frames.len() < frame_len {
                return;
            }

            let payload_start = header_len + mask_len;
            let mut payload = self.websocket_frames[payload_start..frame_len].to_vec();
            if masked {
                let mask: [u8; 4] = self.websocket_frames[header_len..payload_start]
                    .try_into()
                    .unwrap();
                for (index, byte) in payload.iter_mut().enumerate() {
                    *byte ^= mask[index % 4];
                }
            }
            if let Ok(payload) = String::from_utf8(payload) {
                if payload.starts_with("43/smcp,") {
                    obs.client_packets.lock().unwrap().push(payload);
                }
            }
            self.websocket_frames.drain(..frame_len);
        }
    }
}

struct RelayShutdown {
    relay: oneshot::Sender<()>,
    proxy: oneshot::Sender<()>,
}

impl RelayShutdown {
    fn send(self, _value: ()) -> Result<(), ()> {
        let relay = self.relay.send(());
        let proxy = self.proxy.send(());
        if relay.is_ok() && proxy.is_ok() {
            Ok(())
        } else {
            Err(())
        }
    }
}

async fn proxy_connection(mut client: TcpStream, mut relay: TcpStream, obs: Arc<RelayObs>) {
    let (mut client_read, mut client_write) = client.split();
    let (mut relay_read, mut relay_write) = relay.split();
    let upstream = async {
        let mut observer = ClientWireObserver::default();
        let mut buffer = [0_u8; 8192];
        loop {
            let read = client_read.read(&mut buffer).await?;
            if read == 0 {
                break;
            }
            observer.observe(&buffer[..read], &obs);
            relay_write.write_all(&buffer[..read]).await?;
        }
        Ok::<(), std::io::Error>(())
    };
    let downstream = async {
        tokio::io::copy(&mut relay_read, &mut client_write).await?;
        Ok::<(), std::io::Error>(())
    };
    let _ = tokio::try_join!(upstream, downstream);
}

impl RelayObs {
    async fn current_socket(&self) -> SocketRef {
        loop {
            let notified = self.connected.notified();
            if let Some(socket) = self.socket.lock().unwrap().clone() {
                return socket;
            }
            notified.await;
        }
    }

    fn ack_packet_count(&self, ack_id: i64) -> usize {
        let prefix = format!("43/smcp,{ack_id}");
        self.client_packets
            .lock()
            .unwrap()
            .iter()
            .flat_map(|payload| payload.split('\u{1e}'))
            .filter(|packet| packet.starts_with(&prefix))
            .count()
    }
}

async fn start_relay(
    obs: Arc<RelayObs>,
    ping_interval: Duration,
    ping_timeout: Duration,
) -> (String, RelayShutdown) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let (layer, io) = SocketIo::builder()
        .ping_interval(ping_interval)
        .ping_timeout(ping_timeout)
        .ack_timeout(Duration::from_secs(8))
        .build_layer();

    io.ns("/smcp", {
        let obs = Arc::clone(&obs);
        move |socket: SocketRef| {
            obs.connect.fetch_add(1, Ordering::SeqCst);
            obs.sids.lock().unwrap().push(socket.id.to_string());
            *obs.socket.lock().unwrap() = Some(socket.clone());
            obs.connected.notify_waiters();

            socket.on_disconnect({
                let obs = Arc::clone(&obs);
                move |_socket: SocketRef| {
                    obs.disconnect.fetch_add(1, Ordering::SeqCst);
                }
            });

            socket.on("server:join_office", {
                let obs = Arc::clone(&obs);
                move |_socket: SocketRef, _data: Data<Value>, ack: AckSender| {
                    obs.join.fetch_add(1, Ordering::SeqCst);
                    let _ = ack.send(&(true, None::<String>));
                }
            });
        }
    });

    let fallback = tower::service_fn(|_req: hyper::Request<hyper::body::Incoming>| async move {
        Ok::<_, Infallible>(hyper::Response::new(Full::<Bytes>::new(Bytes::new())))
    });
    let service = layer.layer(fallback);
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    if let Ok((stream, _)) = accepted {
                        let io = hyper_util::rt::TokioIo::new(stream);
                        let service = hyper_util::service::TowerToHyperService::new(service.clone());
                        tokio::spawn(async move {
                            let _ = hyper::server::conn::http1::Builder::new()
                                .serve_connection(io, service)
                                .with_upgrades()
                                .await;
                        });
                    }
                }
                _ = &mut shutdown_rx => break,
            }
        }
    });

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    let (proxy_shutdown_tx, mut proxy_shutdown_rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                accepted = proxy_listener.accept() => {
                    if let Ok((client, _)) = accepted {
                        if let Ok(relay) = TcpStream::connect(addr).await {
                            let obs = Arc::clone(&obs);
                            tokio::spawn(proxy_connection(client, relay, obs));
                        }
                    }
                }
                _ = &mut proxy_shutdown_rx => break,
            }
        }
    });
    sleep(Duration::from_millis(100)).await;
    (
        format!("http://{proxy_addr}"),
        RelayShutdown {
            relay: shutdown_tx,
            proxy: proxy_shutdown_tx,
        },
    )
}

async fn make_computer(
    url: &str,
    fake: Arc<ControlledClient>,
) -> (Computer<SilentSession>, tempfile::TempDir) {
    let config = MCPServerConfig::Stdio(StdioServerConfig::new(
        "dispatch-fake",
        StdioServerParameters {
            command: "unused".to_string(),
            args: vec![],
            env: HashMap::new(),
            cwd: None,
        },
    ));
    let bundle_id = resolve_bundle_id(&config);
    let mut servers = HashMap::new();
    servers.insert("dispatch-fake".to_string(), config);
    let factory: ClientFactory = Arc::new(move |_config, _notify| fake.clone());

    let temp = tempfile::TempDir::new().unwrap();
    let computer = Computer::new(
        COMPUTER,
        SilentSession::new("dispatch-test"),
        None,
        Some(servers),
        false,
        false,
    )
    .with_client_factory(factory)
    .with_skill_home(temp.path().join("skills"))
    .with_blob_cache_root(temp.path().join("blob"))
    .with_config_dir(temp.path().join("config"));

    computer.boot_up().await.expect("boot");
    computer
        .start_mcp_client(&bundle_id)
        .await
        .expect("start fake MCP client");
    computer
        .connect_socketio(url, ConnectOptions::default())
        .await
        .expect("connect Socket.IO");
    computer
        .join_office(OFFICE, COMPUTER)
        .await
        .expect("join office");
    (computer, temp)
}

fn tool_request(req_id: &str, call: &str, fast: bool) -> ToolCallReq {
    ToolCallReq {
        base: AgentCallData {
            agent: AGENT.to_string(),
            req_id: ReqId(req_id.to_string()),
        },
        computer: COMPUTER.to_string(),
        tool_name: EXPOSED_TOOL.to_string(),
        params: json!({"call": call, "fast": fast}),
        timeout: 30,
    }
}

async fn emit_tool_call(socket: SocketRef, req: ToolCallReq) -> Result<Value, String> {
    let ack = socket
        .timeout(Duration::from_secs(8))
        .emit_with_ack::<_, Value>("client:tool_call", &req)
        .map_err(|error| format!("emit failed: {error}"))?;
    ack.await.map_err(|error| format!("ack failed: {error}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn heartbeat_remains_live_during_long_tool_call() {
    let obs = Arc::new(RelayObs::default());
    let (url, relay_shutdown) = start_relay(
        Arc::clone(&obs),
        Duration::from_millis(300),
        Duration::from_millis(700),
    )
    .await;
    let mut control = FakeControl::new();
    let (computer, _temp) = make_computer(&url, Arc::clone(&control.client)).await;
    let initial_socket = obs.current_socket().await;
    let initial_sid = initial_socket.id.to_string();

    let ack_task = tokio::spawn(emit_tool_call(
        initial_socket.clone(),
        tool_request("heartbeat-req", "heartbeat", false),
    ));
    assert!(
        control
            .wait_for(
                FakeEvent::Started("heartbeat".to_string()),
                Duration::from_secs(2)
            )
            .await,
        "controlled tool did not start"
    );

    sleep(Duration::from_secs(3)).await;
    let disconnects_while_pending = obs.disconnect.load(Ordering::SeqCst);
    let connects_while_pending = obs.connect.load(Ordering::SeqCst);
    let joins_while_pending = obs.join.load(Ordering::SeqCst);
    let socket_still_connected = initial_socket.connected();

    control.client.release.notify_waiters();
    let ack = tokio::time::timeout(Duration::from_secs(6), ack_task)
        .await
        .map_err(|_| "ack task timed out".to_string())
        .and_then(|joined| joined.map_err(|error| error.to_string()))
        .and_then(|result| result);
    sleep(Duration::from_millis(500)).await;
    let final_connects = obs.connect.load(Ordering::SeqCst);
    let final_disconnects = obs.disconnect.load(Ordering::SeqCst);
    let final_joins = obs.join.load(Ordering::SeqCst);
    let sids = obs.sids.lock().unwrap().clone();

    let _ = computer.shutdown().await;
    let _ = relay_shutdown.send(());

    assert_eq!(
        disconnects_while_pending, 0,
        "connection disconnected while tool was pending: connects={connects_while_pending}, joins={joins_while_pending}, sid={initial_sid}"
    );
    assert!(
        socket_still_connected,
        "initial SID closed during pending call"
    );
    assert_eq!(
        connects_while_pending, 1,
        "unexpected reconnect while pending"
    );
    assert_eq!(joins_while_pending, 1, "unexpected rejoin while pending");
    assert_eq!(final_connects, 1, "connection generation changed: {sids:?}");
    assert_eq!(
        final_disconnects, 0,
        "connection disconnected before cleanup"
    );
    assert_eq!(final_joins, 1, "office was joined more than once");
    let ack = ack.expect("tool call should receive its final ACK on the original connection");
    assert_ne!(ack.get("isError").and_then(Value::as_bool), Some(true));
    assert_eq!(control.client.active.load(Ordering::SeqCst), 0);
    assert_eq!(control.client.natural.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancellation_is_consumed_while_tool_is_pending() {
    let obs = Arc::new(RelayObs::default());
    let (url, relay_shutdown) = start_relay(
        Arc::clone(&obs),
        Duration::from_secs(60),
        Duration::from_secs(60),
    )
    .await;
    let mut control = FakeControl::new();
    let (computer, _temp) = make_computer(&url, Arc::clone(&control.client)).await;
    let socket = obs.current_socket().await;

    let ack_task = tokio::spawn(emit_tool_call(
        socket.clone(),
        tool_request("cancel-req", "cancel", false),
    ));
    assert!(
        control
            .wait_for(
                FakeEvent::Started("cancel".to_string()),
                Duration::from_secs(2)
            )
            .await,
        "controlled tool did not start"
    );

    socket
        .emit(
            "notify:tool_call_cancel",
            &json!({"agent": AGENT, "req_id": "cancel-req"}),
        )
        .expect("emit cancellation");
    let cancelled_before_release = control
        .wait_for(
            FakeEvent::Cancelled("cancel".to_string()),
            Duration::from_secs(1),
        )
        .await;

    if !cancelled_before_release {
        control.client.release.notify_waiters();
    }
    let ack = tokio::time::timeout(Duration::from_secs(4), ack_task)
        .await
        .map_err(|_| "ack task timed out".to_string())
        .and_then(|joined| joined.map_err(|error| error.to_string()))
        .and_then(|result| result);

    // This is the first server-originated emit_with_ack on the socket, so its
    // Socket.IO ack id is 1. Observe beyond both cancellation and tool release;
    // socketioxide's AckStream itself is one-shot and cannot expose duplicates.
    control.client.release.notify_waiters();
    sleep(Duration::from_millis(500)).await;
    let ack_packet_count = obs.ack_packet_count(1);

    let connection_alive = socket.connected();
    let connects = obs.connect.load(Ordering::SeqCst);
    let disconnects = obs.disconnect.load(Ordering::SeqCst);
    let joins = obs.join.load(Ordering::SeqCst);
    let cancelled_count = control.client.cancelled.load(Ordering::SeqCst);
    let natural_count = control.client.natural.load(Ordering::SeqCst);

    let _ = computer.shutdown().await;
    let _ = relay_shutdown.send(());

    assert!(
        cancelled_before_release,
        "cancel event was not consumed within 1s while the tool was pending"
    );
    assert!(connection_alive, "connection died during cancellation");
    assert_eq!(connects, 1, "unexpected reconnect during cancellation");
    assert_eq!(disconnects, 0, "unexpected disconnect during cancellation");
    assert_eq!(joins, 1, "unexpected rejoin during cancellation");
    assert_eq!(cancelled_count, 1, "CancellationToken should fire once");
    assert_eq!(
        natural_count, 0,
        "tool completed naturally before cancellation"
    );
    assert_eq!(control.client.active.load(Ordering::SeqCst), 0);
    assert_eq!(
        ack_packet_count, 1,
        "cancelled tool call must emit exactly one Socket.IO ACK packet"
    );

    let ack = ack.expect("cancelled tool call should receive one final ACK");
    assert_eq!(ack.get("isError").and_then(Value::as_bool), Some(true));
    assert_eq!(
        ack.pointer("/meta/a2c_cancelled").and_then(Value::as_bool),
        Some(true),
        "missing cancellation metadata: {ack}"
    );
    assert_eq!(
        ack.pointer("/meta/a2c_cancel_reason")
            .and_then(Value::as_str),
        Some("agent_requested"),
        "wrong cancellation reason: {ack}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_and_completed_req_id_cancellation_are_noop() {
    let obs = Arc::new(RelayObs::default());
    let (url, relay_shutdown) = start_relay(
        Arc::clone(&obs),
        Duration::from_secs(60),
        Duration::from_secs(60),
    )
    .await;
    let control = FakeControl::new();
    let (computer, _temp) = make_computer(&url, Arc::clone(&control.client)).await;
    let socket = obs.current_socket().await;

    socket
        .emit(
            "notify:tool_call_cancel",
            &json!({"agent": AGENT, "req_id": "unknown-req"}),
        )
        .expect("emit unknown cancellation");

    // Independent events may complete in either order under 0.9.0, but both
    // must be delivered exactly once. Keep the assertions membership-based.
    let (first_ack, second_ack) = tokio::join!(
        emit_tool_call(
            socket.clone(),
            tool_request("completed-req", "completed-a", true),
        ),
        emit_tool_call(
            socket.clone(),
            tool_request("independent-req", "completed-b", true),
        )
    );
    for ack in [first_ack, second_ack] {
        let ack = ack.expect("independent fast tool call should complete");
        assert_ne!(ack.get("isError").and_then(Value::as_bool), Some(true));
    }

    socket
        .emit(
            "notify:tool_call_cancel",
            &json!({"agent": AGENT, "req_id": "completed-req"}),
        )
        .expect("emit completed cancellation");
    sleep(Duration::from_millis(200)).await;

    assert_eq!(
        control.client.cancelled.load(Ordering::SeqCst),
        0,
        "unknown/completed cancellation must be no-op"
    );
    assert_eq!(
        control.client.natural.load(Ordering::SeqCst),
        2,
        "both independent fast tools should complete exactly once"
    );
    assert_eq!(control.client.active.load(Ordering::SeqCst), 0);
    assert!(
        socket.connected(),
        "no-op cancellation damaged the connection"
    );
    assert_eq!(obs.connect.load(Ordering::SeqCst), 1);
    assert_eq!(obs.disconnect.load(Ordering::SeqCst), 0);
    assert_eq!(obs.join.load(Ordering::SeqCst), 1);

    let _ = computer.shutdown().await;
    let _ = relay_shutdown.send(());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disconnect_and_shutdown_drop_pending_tools_without_delayed_ack() {
    let obs = Arc::new(RelayObs::default());
    let (url, relay_shutdown) = start_relay(
        Arc::clone(&obs),
        Duration::from_secs(60),
        Duration::from_secs(60),
    )
    .await;
    let mut control = FakeControl::new();
    let (computer, _temp) = make_computer(&url, Arc::clone(&control.client)).await;
    let socket = obs.current_socket().await;

    let mut disconnect_ack = tokio::spawn(emit_tool_call(
        socket,
        tool_request("disconnect-req", "disconnect", false),
    ));
    assert!(
        control
            .wait_for(
                FakeEvent::Started("disconnect".to_string()),
                Duration::from_secs(2),
            )
            .await,
        "disconnect lifecycle tool did not start"
    );
    tokio::time::timeout(Duration::from_secs(2), computer.disconnect_socketio())
        .await
        .expect("disconnect timed out")
        .expect("disconnect failed");
    assert!(
        control
            .wait_for(
                FakeEvent::Dropped("disconnect".to_string()),
                Duration::from_secs(1),
            )
            .await,
        "disconnect did not drop pending tool future"
    );
    if let Ok(Ok(Ok(value))) =
        tokio::time::timeout(Duration::from_secs(1), &mut disconnect_ack).await
    {
        panic!("disconnect produced a delayed tool ACK: {value}");
    }
    disconnect_ack.abort();

    computer
        .connect_socketio(&url, ConnectOptions::default())
        .await
        .expect("reconnect after manual disconnect");
    computer
        .join_office(OFFICE, COMPUTER)
        .await
        .expect("rejoin after manual disconnect");
    let shutdown_socket = obs.current_socket().await;
    let mut shutdown_ack = tokio::spawn(emit_tool_call(
        shutdown_socket,
        tool_request("shutdown-req", "shutdown", false),
    ));
    assert!(
        control
            .wait_for(
                FakeEvent::Started("shutdown".to_string()),
                Duration::from_secs(2),
            )
            .await,
        "shutdown lifecycle tool did not start"
    );
    tokio::time::timeout(Duration::from_secs(2), computer.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown failed");
    assert!(
        control
            .wait_for(
                FakeEvent::Dropped("shutdown".to_string()),
                Duration::from_secs(1),
            )
            .await,
        "shutdown did not drop pending tool future"
    );
    if let Ok(Ok(Ok(value))) = tokio::time::timeout(Duration::from_secs(1), &mut shutdown_ack).await
    {
        panic!("shutdown produced a delayed tool ACK: {value}");
    }
    shutdown_ack.abort();

    control.client.release.notify_waiters();
    sleep(Duration::from_millis(200)).await;
    assert_eq!(
        control.client.natural.load(Ordering::SeqCst),
        0,
        "teardown must not leave a tool future that can complete later"
    );
    assert_eq!(control.client.active.load(Ordering::SeqCst), 0);

    let _ = relay_shutdown.send(());
}
