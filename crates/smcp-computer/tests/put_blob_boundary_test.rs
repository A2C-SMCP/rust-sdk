//! #195 —— `client:put_blob` 上行写入的**真链路**守护（v0.4.0）。
//!
//! 与 `socketio_boundary_test.rs`（用户 #207/#208 WIP）隔离的独立文件：用**裸 socketioxide
//! recording relay**（ACK `server:join_office` 使 Computer 达 joined + 捕获 Computer 的
//! [`SocketRef`]），测试侧经 `emit_with_ack` 以真实入站事件驱动 `client:put_blob` 分块上行——
//! 覆盖真实 socket.io dispatch → [`SmcpComputerClient`] handler → [`BlobUploadStore`] 落盘的整条链路
//! （单元级 handler 测试直接调 handler，绕过 dispatch / ack 帧收发）。
//!
//! 对拍判据（python#196 迁移）：首块 ack 键集 = {chunk_offset, req_id, upload_id}；末块 ack 键集 =
//! {chunk_offset, landing_path, req_id, sha256, total_size, upload_id}；落盘字节 == 上传字节。

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use http_body_util::Full;
use hyper::body::Bytes;
use serde_json::{json, Value};
use socketioxide::extract::{AckSender, Data, SocketRef};
use socketioxide::SocketIo;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::time::sleep;
use tower::Layer;

use smcp_computer::computer::{Computer, ConnectOptions, SilentSession};

const OBS_WAIT: Duration = Duration::from_secs(5);

/// relay 观测面：计数 + Computer 命名的 `server:update_tool_list` 列表。
#[derive(Default)]
struct RelayObs {
    connect: AtomicU32,
}

/// 启动裸 socketioxide+hyper recording relay，捕获 Computer 的 [`SocketRef`] 供测试侧
/// `emit_with_ack` 驱动 `client:put_blob`。返回 `(url, computer_socket, shutdown_tx)`。
async fn start_relay(
    obs: Arc<RelayObs>,
) -> (String, Arc<Mutex<Option<SocketRef>>>, oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let computer_socket: Arc<Mutex<Option<SocketRef>>> = Arc::new(Mutex::new(None));

    let (layer, io) = SocketIo::new_layer();
    io.ns("/smcp", {
        let obs = obs.clone();
        let computer_socket = computer_socket.clone();
        move |socket: SocketRef| {
            obs.connect.fetch_add(1, Ordering::SeqCst);
            *computer_socket.lock().unwrap() = Some(socket.clone());

            // ACK join 成功（mirror 真实 server 的 `(bool, Option<String>)` = `[true, null]`）。
            socket.on(
                "server:join_office",
                move |_s: SocketRef, _d: Data<Value>, ack: AckSender| async move {
                    let _ = ack.send(&(true, None::<String>));
                },
            );
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
    (format!("http://{addr}"), computer_socket, shutdown_tx)
}

async fn wait_until<F>(mut cond: F, timeout_dur: Duration) -> bool
where
    F: FnMut() -> bool,
{
    let deadline = tokio::time::Instant::now() + timeout_dur;
    while tokio::time::Instant::now() < deadline {
        if cond() {
            return true;
        }
        sleep(Duration::from_millis(25)).await;
    }
    false
}

/// relay 侧以 `emit_with_ack` 驱动单个 `client:put_blob` 块，返回 ack 首参裸 JSON。
///
/// socketioxide [`socket::Socket::emit_with_ack`] 的 `AckStream` 实现 [`std::future::Future`]
/// （输出 `Result<V, AckError>`）；ack 载体可能是 args 数组（`[<payload>]`）或裸 payload——
/// 兼容展开取首参。
async fn drive_chunk(socket: &SocketRef, payload: Value) -> Value {
    let stream = socket
        .emit_with_ack::<Value, Value>("client:put_blob", &payload)
        .expect("emit_with_ack ok");
    let raw = stream.await.expect("ack resolved");
    match raw {
        Value::Array(mut args) => args.drain(..).next().unwrap_or(Value::Null),
        other => other,
    }
}

fn b64(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

#[tokio::test]
async fn put_blob_over_real_socketio_roundtrip() {
    let _ = tracing_subscriber::fmt().try_init();
    let obs = Arc::new(RelayObs::default());
    let (url, computer_socket, _shutdown) = start_relay(obs.clone()).await;

    let tmp = TempDir::new().unwrap();
    let computer = Computer::new("c1", SilentSession::new("s"), None, None, false, false)
        .with_skill_home(tmp.path().join("home"))
        .with_blob_cache_root(tmp.path().join("blob"))
        .with_landing_root(tmp.path().join("landing"));
    computer.boot_up().await.expect("boot");
    computer
        .connect_socketio(&url, ConnectOptions::default())
        .await
        .expect("connect");
    assert!(
        wait_until(|| obs.connect.load(Ordering::SeqCst) >= 1, OBS_WAIT).await,
        "relay 未观察到 connect"
    );
    computer.join_office("o", "c1").await.expect("join");
    // join 完成后 handler 类（computer_ops）由连接层接好——就绪由 join ack 保证。

    let socket = computer_socket
        .lock()
        .unwrap()
        .clone()
        .expect("computer socket");
    let data: Vec<u8> = (0..10240).map(|i| (i % 251) as u8).collect();
    let sha = smcp::utils::hash::sha256_hex(&data);
    let chunk: usize = 256;

    // 首块（eof=false）→ ack：仅 {chunk_offset, req_id, upload_id}。
    let first_payload = json!({
        "agent": "a", "req_id": "r1", "computer": "c1",
        "chunk_offset": 0, "eof": false,
        "total_size": data.len(), "sha256": sha,
        "name_hint": "big.bin",
        "blob": b64(&data[0..chunk]),
    });
    let ack1 = drive_chunk(&socket, first_payload).await;
    let keys1: std::collections::BTreeSet<&str> = ack1
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys1,
        ["chunk_offset", "req_id", "upload_id"]
            .into_iter()
            .collect()
    );
    let upload_id = ack1["upload_id"].as_str().unwrap().to_string();
    assert_eq!(upload_id.len(), 32);

    // 中间块（ack-paced 顺序）。
    let mut offset = chunk;
    while offset + chunk < data.len() {
        let p = json!({
            "agent": "a", "req_id": "r2", "computer": "c1",
            "upload_id": upload_id, "chunk_offset": offset, "eof": false,
            "blob": b64(&data[offset..offset + chunk]),
        });
        let ack = drive_chunk(&socket, p).await;
        assert_eq!(ack["upload_id"], json!(upload_id));
        offset += chunk;
    }

    // 末块（eof=true）→ ack 含 landing_path/total_size/sha256；落盘字节自证。
    let final_payload = json!({
        "agent": "a", "req_id": "r3", "computer": "c1",
        "upload_id": upload_id, "chunk_offset": offset, "eof": true,
        "blob": b64(&data[offset..]),
    });
    let ack_final = drive_chunk(&socket, final_payload).await;
    let keys2: std::collections::BTreeSet<&str> = ack_final
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys2,
        [
            "chunk_offset",
            "landing_path",
            "req_id",
            "sha256",
            "total_size",
            "upload_id"
        ]
        .into_iter()
        .collect()
    );
    assert_eq!(ack_final["total_size"], json!(data.len()));
    assert_eq!(ack_final["sha256"], json!(sha));
    let landing_path = ack_final["landing_path"].as_str().unwrap().to_string();
    let name = std::path::Path::new(&landing_path)
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    assert!(
        name.starts_with(&upload_id),
        "安全名必须以 upload_id 起头：{name}"
    );
    // 落盘字节 == 上传字节。
    assert_eq!(std::fs::read(&landing_path).unwrap(), data);
    // `.a2c-upload` 无残留 `.part`。
    let part_dir = tmp.path().join("landing").join(".a2c-upload");
    assert_eq!(std::fs::read_dir(part_dir).unwrap().count(), 0);

    // store fail-closed 已由单元层覆盖（unset root → 4019 forbidden），此处不重复。
    // 防假绿判据：确实走了 store（多块会话），而非命中 unhandled 分支——ack 键集与落盘字节已证。
}
