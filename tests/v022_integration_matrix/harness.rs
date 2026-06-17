//! REL-01 (#24) v0.2.2 集成矩阵共享 harness / shared harness for the v0.2.2 integration matrix。
//!
//! 提供：
//! - [`RelayServer`]：无版本握手的裸 socketioxide+hyper server（转发场景，聚焦 relay 语义）。
//! - [`HandshakeServer`]：启用版本握手中间件的 [`smcp_server_hyper::HyperServer`]（握手 / WS-only 场景）。
//! - [`spawn_computer`]：真实 [`Computer`] + v022 stdio MCP 子进程，注入 TempDir skill_home/blob 隔离。
//! - 裸 `tf-rust-socketio` Agent 客户端 + ack/emit 辅助 + flat 解包 + 递归 JSON 取值 + hex。
//!
//! 端口均取 `127.0.0.1:0` 临时口；每测试独立 server + Computer + TempDir，可并行。

use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt;
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::Request;
use hyper_util::rt::{TokioExecutor, TokioIo};
use serde_json::{json, Value};
use tempfile::TempDir;
use tf_rust_socketio::asynchronous::{Client, ClientBuilder};
use tf_rust_socketio::{Payload, TransportType};
use tokio::net::TcpListener;
use tokio::sync::{oneshot, Mutex};
use tokio::time::sleep;
use tower::Layer;

use smcp::Role;
use smcp_computer::blob::thresholds::BlobThresholds;
use smcp_computer::computer::{Computer, ConnectOptions, SilentSession};
use smcp_computer::mcp_clients::model::{
    MCPServerConfig, StdioServerConfig, StdioServerParameters,
};
use smcp_server_core::{DefaultAuthenticationProvider, SmcpServerBuilder};
use smcp_server_hyper::HyperServerBuilder;

/// 测试共享密钥（#86：放入 Socket.IO auth dict `token` 字段，对齐 DefaultAuthenticationProvider 默认）。
pub const SECRET: &str = "test_secret";
/// 裸 `tf-rust-socketio` 客户端连接的 namespace。**不带前导 `/`**：tf-rust-socketio 会归一为
/// `/smcp`（= 服务端 `io.ns(SMCP_NAMESPACE)`）；显式传 `/smcp` 反而连不上默认命名空间（实测 ack 超时）。
/// Raw-client namespace WITHOUT leading slash — tf-rust-socketio normalizes it to `/smcp`.
pub const NS: &str = "smcp";
/// 注册的 MCP server 名 / registered MCP server name。
pub const MCP_NAME: &str = "v022";
/// 默认 office / agent / computer 名。
pub const OFFICE: &str = "office-matrix";
pub const AGENT: &str = "agent1";
pub const COMPUTER: &str = "computer1";

/// v022 MCP server 入口绝对路径（CWD 无关）。
fn mcp_server_path() -> String {
    format!(
        "{}/tests/v022-mcp-server/index.js",
        env!("CARGO_MANIFEST_DIR")
    )
}

// ─────────────────────────── 裸 server（无握手）/ raw server (no handshake) ───────────────────────────

/// 无版本握手的裸 socketioxide+hyper 测试 server（转发场景）。
pub struct RelayServer {
    pub addr: SocketAddr,
    shutdown_tx: oneshot::Sender<()>,
}

impl RelayServer {
    /// 启动并返回（绑定临时端口，等待短暂就绪）。
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let layer = SmcpServerBuilder::new()
            .with_auth_provider(Arc::new(DefaultAuthenticationProvider::new(
                Some(SECRET.to_string()),
                None,
            )))
            .build_layer()
            .expect("build SMCP layer");

        let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();

        // 关键：socket.io layer **只叠一次**后整体 clone 给每条连接——polling（Computer 默认
        // `TransportType::Any` 起手）需跨多次 HTTP 请求维持 engine.io 会话；若每请求新叠 layer
        // 会断会话连续性（实测 Computer connect → EngineIO Error）。对齐 socketio_client_test 的
        // 已验证 server 形态。Build the layered service ONCE and clone per connection.
        let fallback =
            tower::service_fn(|_req: hyper::Request<hyper::body::Incoming>| async move {
                Ok::<_, std::convert::Infallible>(hyper::Response::new(Full::new(Bytes::new())))
            });
        let service = layer.layer.layer(fallback);

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        if let Ok((stream, _)) = result {
                            let io = TokioIo::new(stream);
                            let svc = hyper_util::service::TowerToHyperService::new(service.clone());
                            tokio::spawn(async move {
                                let _ = hyper::server::conn::http1::Builder::new()
                                    .serve_connection(io, svc)
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
        Self { addr, shutdown_tx }
    }

    /// 连接 URL（`http://127.0.0.1:<port>`）。
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// 关闭 server。
    pub fn shutdown(self) {
        let _ = self.shutdown_tx.send(());
    }
}

// ─────────────────── 握手 server（启用版本握手中间件）/ handshake-enabled server ───────────────────

/// 启用版本握手中间件的 [`HyperServer`]（握手三态 / WS-only 场景）。
pub struct HandshakeServer {
    addr: SocketAddr,
    handle: tokio::task::JoinHandle<()>,
}

impl HandshakeServer {
    /// 启动（取临时端口后由 `HyperServer::run` 绑定；握手中间件默认启用）。
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let layer = SmcpServerBuilder::new()
            .with_auth_provider(Arc::new(DefaultAuthenticationProvider::new(
                Some(SECRET.to_string()),
                None,
            )))
            .build_layer()
            .expect("build SMCP layer");
        let server = HyperServerBuilder::new()
            .with_layer(layer)
            .with_addr(addr)
            .build();

        let handle = tokio::spawn(async move {
            let _ = server.run(addr).await;
        });
        sleep(Duration::from_millis(180)).await;
        Self { addr, handle }
    }

    /// HTTP 基址（`http://127.0.0.1:<port>`）。
    pub fn http(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// 关闭 server。
    pub fn shutdown(self) {
        self.handle.abort();
    }
}

// ──────────────────────────── 真实 Computer / real Computer ────────────────────────────

/// 构建真实 [`Computer`]（注册 v022 stdio MCP）+ boot_up + 连接 + join_office。
///
/// 注入 TempDir skill_home/blob 隔离 boot_up FS 副作用（INT-01 #68），并置
/// `A2C_SKILL_WATCH_POLLING=1` 用确定性轮询 watcher（规避多线程下 watcher 偶发 SIGABRT）。
pub async fn spawn_computer(
    server_url: &str,
    office: &str,
    name: &str,
    td: &TempDir,
    thresholds: Option<BlobThresholds>,
) -> Computer<SilentSession> {
    // 确定性 watcher：进程级幂等置位，从不复位 → 并发读安全。
    std::env::set_var("A2C_SKILL_WATCH_POLLING", "1");

    let mut servers = HashMap::new();
    servers.insert(
        MCP_NAME.to_string(),
        MCPServerConfig::Stdio(StdioServerConfig {
            name: MCP_NAME.into(),
            disabled: false,
            forbidden_tools: vec![],
            tool_meta: HashMap::new(),
            default_tool_meta: None,
            vrl: None,
            env_file: None,
            server_parameters: StdioServerParameters {
                command: "node".into(),
                args: vec![mcp_server_path()],
                env: HashMap::new(),
                cwd: None,
            },
        }),
    );

    let mut computer = Computer::new(
        name,
        SilentSession::new("matrix-session"),
        None,
        Some(servers),
        true,
        true,
    )
    .with_skill_home(td.path().join("skills"))
    .with_blob_cache_root(td.path().join("blob"));
    if let Some(t) = thresholds {
        computer = computer.with_blob_thresholds(t);
    }

    computer.boot_up().await.expect("computer boot_up");
    // boot_up 仅注册配置（manager auto_connect 默认 false），需显式启动 stdio MCP 子进程，
    // 否则 get_resources/tool_call 命中未连接服务 → 4014。
    computer
        .start_mcp_client("all")
        .await
        .expect("start MCP servers");
    computer
        .connect_socketio(
            server_url,
            ConnectOptions {
                auth_payload: Some(serde_json::json!({"token": SECRET})),
                namespace: NS.to_string(),
                ..Default::default()
            },
        )
        .await
        .expect("computer connect_socketio");
    computer
        .join_office(office, name)
        .await
        .expect("computer join_office");
    // 等 join 在房间内稳定可见。
    sleep(Duration::from_millis(250)).await;
    computer
}

// ──────────────────────────── 裸 Agent 客户端 / raw Agent client ────────────────────────────

/// 连接一个裸 Agent 客户端（websocket，鉴权走 auth dict `token` 字段；无 a2c_version——裸 server 不 gate）。
pub async fn agent_client(server_url: &str) -> Client {
    let client = ClientBuilder::new(server_url.to_string())
        .transport_type(TransportType::Websocket)
        .namespace(NS)
        .auth(serde_json::json!({"token": SECRET}))
        .connect()
        .await
        .expect("agent connect");
    // 命名空间 CONNECT 需短暂落定后 emit 才会被服务端收到（与既有 e2e 同款 settle 等待）。
    sleep(Duration::from_millis(200)).await;
    client
}

/// ack 回调：把单参 ack（`Payload::Text` 末元素）经 oneshot 投出。
fn ack_cb(
    tx: oneshot::Sender<Value>,
) -> impl FnMut(Payload, Client) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync {
    let tx = Arc::new(Mutex::new(Some(tx)));
    move |p: Payload, _c: Client| {
        let tx = tx.clone();
        async move {
            let v = match p {
                Payload::Text(mut vals, _) => vals.pop().unwrap_or(Value::Null),
                _ => Value::Null,
            };
            if let Some(tx) = tx.lock().await.take() {
                let _ = tx.send(v);
            }
        }
        .boxed()
    }
}

/// join_office（断言成功）。
pub async fn join(client: &Client, role: Role, office: &str, name: &str) {
    let (tx, rx) = oneshot::channel::<Value>();
    client
        .emit_with_ack(
            "server:join_office",
            json!({ "role": role.to_string(), "office_id": office, "name": name }),
            Duration::from_secs(15),
            ack_cb(tx),
        )
        .await
        .expect("join emit");
    let v = tokio::time::timeout(Duration::from_secs(15), rx)
        .await
        .expect("join ack timeout")
        .expect("join ack channel");
    let ok = v
        .as_array()
        .and_then(|a| a.first())
        .and_then(Value::as_bool)
        .unwrap_or(false);
    assert!(
        ok,
        "join_office 失败 role={role:?} office={office} name={name}: {v}"
    );
}

/// 发 `client:*` 请求并返回 flat 解包后的 ack（对象级 ErrorPayload / Ret）。
pub async fn emit_call(client: &Client, event: &str, payload: Value) -> Value {
    let (tx, rx) = oneshot::channel::<Value>();
    client
        .emit_with_ack(event, payload, Duration::from_secs(20), ack_cb(tx))
        .await
        .expect("emit_with_ack");
    let v = tokio::time::timeout(Duration::from_secs(20), rx)
        .await
        .expect("ack timeout")
        .expect("ack channel");
    flat(v)
}

// ──────────────────────────── 纯函数辅助 / pure helpers ────────────────────────────

/// socketio ack 以 args 数组 `[<v>]` 投递时解一层；否则原样返回。
pub fn flat(v: Value) -> Value {
    v.as_array().and_then(|a| a.first()).cloned().unwrap_or(v)
}

/// 递归在 JSON 中找首个 key（绕过结果级 meta 线 key 分歧 `_meta`/`meta` 等形状不确定）。
pub fn deep_find<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    match v {
        Value::Object(map) => {
            if let Some(found) = map.get(key) {
                return Some(found);
            }
            map.values().find_map(|val| deep_find(val, key))
        }
        Value::Array(arr) => arr.iter().find_map(|item| deep_find(item, key)),
        _ => None,
    }
}

/// 字节 → 小写 hex（避免引入 hex crate）。
pub fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// 裸 HTTP GET（hyper legacy client）→ (status u16, X-A2C-Error-Code header, JSON body)。
pub async fn http_get(url: &str) -> (u16, Option<String>, Value) {
    let client: hyper_util::client::legacy::Client<_, Full<Bytes>> =
        hyper_util::client::legacy::Client::builder(TokioExecutor::new()).build_http();
    let req = Request::builder()
        .uri(url)
        .body(Full::new(Bytes::new()))
        .expect("build request");
    let resp = client.request(req).await.expect("http get");
    let status = resp.status().as_u16();
    let hdr = resp
        .headers()
        .get("x-a2c-error-code")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("collect body")
        .to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, hdr, json)
}
