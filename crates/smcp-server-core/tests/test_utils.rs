/**
* 文件名: test_utils
* 作者: JQQ
* 创建日期: 2025/1/14
* 最后修改日期: 2025/1/14
* 版权: 2025 JQQ. All rights reserved.
* 依赖: None
* 描述: SMCP服务器测试共享工具模块
*/
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt;
use http_body_util::Full;
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use smcp::*;
use tf_rust_socketio::asynchronous::ClientBuilder;
use tf_rust_socketio::TransportType;
use tf_rust_socketio::{Event, Payload};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::time::sleep;
use tower::{Layer, Service};

use smcp_server_core::{DefaultAuthenticationProvider, SmcpServerBuilder};

/// 从入站 Payload 中取出单个 JSON 载荷（剥 `[payload]` 单元素包装；JSON 字符串元素解析回对象）。
#[allow(dead_code)]
pub fn payload_json(payload: &Payload) -> Option<Value> {
    match payload {
        Payload::Text(values, _) => {
            let first = values.first()?;
            let value = match first {
                Value::Array(items) if items.len() == 1 => items.first()?.clone(),
                Value::String(text) => serde_json::from_str(text).ok()?,
                other => other.clone(),
            };
            Some(value)
        }
        _ => None,
    }
}

/// 创建**可应答 `client:get_tools`** 的 Computer 桩：回 `{"tools": [], "req_id": <回显>}`。
///
/// 用于路由断言——「目标 Computer 可达」必须由对端**真实应答**证明，而非仅凭「没收到 404」；
/// 「不可达（他房 / 已离房）⇒ flat 404」由服务端即刻应答，无需桩参与。
#[allow(dead_code)]
pub async fn create_computer_stub(server_url: &str) -> tf_rust_socketio::asynchronous::Client {
    let ready = Arc::new(tokio::sync::Notify::new());
    let signal = ready.clone();

    let client = ClientBuilder::new(server_url)
        .transport_type(TransportType::Websocket)
        .namespace(SMCP_NAMESPACE)
        .auth(json!({"token": "test_secret"}))
        .on(Event::Connect, move |_, _| {
            let signal = signal.clone();
            Box::pin(async move {
                signal.notify_one();
            })
        })
        .on(events::CLIENT_GET_TOOLS, move |payload, client| {
            Box::pin(async move {
                let ack_id = match &payload {
                    Payload::Text(_, ack_id) => *ack_id,
                    #[allow(deprecated)]
                    Payload::String(_, ack_id) => *ack_id,
                    Payload::Binary(_, _) => None,
                };
                let req_id = payload_json(&payload)
                    .and_then(|v| v.get("req_id").and_then(|r| r.as_str()).map(String::from));
                if let Some(id) = ack_id {
                    let _ = client
                        .ack_with_id(id, json!({"tools": [], "req_id": req_id}))
                        .await;
                }
            })
        })
        .connect()
        .await
        .expect("computer stub connect failed");

    tokio::time::timeout(Duration::from_secs(5), ready.notified())
        .await
        .expect("computer stub namespace connect timeout");

    client
}

/// 断言成功 ack 是协议规定的**空 ack**（线格式：**零参** ACK，拆封后为 `[]`）。
///
/// 协议依据：error-handling.md —— `server:join_office` / `server:leave_office` 成功回**空 ack**
/// （v0.5.0 前为 `(bool, str | None)` 元组，已废除）。python-socketio 参考实现在 handler 返回 `None`
/// 时发出的正是零参 ACK `[]`（`_handle_event_internal`：`if r is None: data = []`），故本断言**钉死
/// `[]`**——`[null]`（socketioxide `ack.send(&())` 经 `to_value` 包成 1-tuple 的旧形态）逐字节不符，
/// 属本次要修的跨 SDK 偏差（#226 P1-6）。断言写成 `assert_eq!` 而非「二者皆可」的容忍式，
/// 正是为了让该偏差再也无法被静默放过。
///
/// 2026-09 起 server 侧由 `EmptyAck`（`serialize_tuple(0)`）产出零参 ACK；本助手是测试侧的**单一权威**
/// 判据（对标 Python `tests/room_acks.py::assert_empty_ack`），避免数十个站点各自手写、各自漂移。
pub fn assert_empty_ack(payload: &serde_json::Value, action: &str) {
    assert_eq!(
        payload,
        &serde_json::json!([]),
        "{action} 成功应回零参空 ack `[]`；实得 {payload}。若为 `[null]`，说明服务端仍在用 \
         ack.send(&())（多出一个 null 实参）；若为 flat ErrorPayload，说明本端把失败当成了成功。"
    );
}

/// 测试用的SMCP服务器
pub struct SmcpTestServer {
    pub addr: SocketAddr,
    shutdown_tx: oneshot::Sender<()>,
}

impl SmcpTestServer {
    /// 启动测试服务器
    pub async fn start() -> Self {
        let port = find_available_port().await;
        let addr: SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();

        let layer = SmcpServerBuilder::new()
            .with_auth_provider(Arc::new(DefaultAuthenticationProvider::new(
                Some("test_secret".to_string()),
                None,
            )))
            .build_layer()
            .expect("failed to build SMCP server layer");

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let listener = TcpListener::bind(addr).await.unwrap();
        let actual_addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let mut shutdown_rx = shutdown_rx;
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        if let Ok((stream, _)) = result {
                            let io = TokioIo::new(stream);
                            let layer = layer.clone();

                            tokio::spawn(async move {
                                let svc = tower::service_fn(|req| {
                                    let layer = layer.clone();
                                    async move {
                                        let svc = tower::service_fn(|_req| async move {
                                            Ok::<_, std::convert::Infallible>(hyper::Response::new(Full::new(hyper::body::Bytes::new())))
                                        });
                                        let mut svc = layer.layer.layer(svc);
                                        svc.call(req).await
                                    }
                                });

                                let svc = hyper_util::service::TowerToHyperService::new(svc);
                                let _ = hyper::server::conn::http1::Builder::new()
                                    .serve_connection(io, svc)
                                    .with_upgrades()
                                    .await;
                            });
                        }
                    }
                    _ = &mut shutdown_rx => {
                        break;
                    }
                }
            }
        });

        // 等待服务器启动
        sleep(Duration::from_millis(100)).await;

        SmcpTestServer {
            addr: actual_addr,
            shutdown_tx,
        }
    }

    /// 获取服务器URL
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// 关闭服务器
    pub fn shutdown(self) {
        let _ = self.shutdown_tx.send(());
    }
}

/// 查找可用端口
pub async fn find_available_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap().port()
}

/// 创建ACK回调函数
#[allow(dead_code)]
pub fn ack_to_sender<T: Send + 'static>(
    sender: oneshot::Sender<T>,
    f: impl Fn(Payload) -> T + Send + Sync + 'static,
) -> impl Fn(
    Payload,
    tf_rust_socketio::asynchronous::Client,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
       + Send
       + Sync {
    let sender = Arc::new(tokio::sync::Mutex::new(Some(sender)));
    let f = Arc::new(f);
    move |payload: Payload, _client| {
        let sender = sender.clone();
        let f = f.clone();
        async move {
            let result = f(payload);
            if let Some(sender) = sender.lock().await.take() {
                let _ = sender.send(result);
            }
        }
        .boxed()
    }
}

/// 创建测试客户端
pub async fn create_test_client(
    server_url: &str,
    namespace: &str,
) -> tf_rust_socketio::asynchronous::Client {
    let ready = Arc::new(tokio::sync::Notify::new());
    let signal = ready.clone();
    let client = ClientBuilder::new(server_url)
        .on(tf_rust_socketio::Event::Connect, move |_, _| {
            let signal = signal.clone();
            Box::pin(async move {
                signal.notify_one();
            })
        })
        .transport_type(TransportType::Websocket)
        .namespace(namespace)
        .auth(serde_json::json!({"token": "test_secret"}))
        .connect()
        .await
        .expect("Failed to connect client");
    tokio::time::timeout(Duration::from_secs(5), ready.notified())
        .await
        .expect("namespace connect timeout");
    client
}

/// 创建带事件处理器的测试客户端
#[allow(dead_code)]
pub async fn create_client_with_handler<F>(
    server_url: &str,
    namespace: &str,
    event: &str,
    handler: F,
) -> tf_rust_socketio::asynchronous::Client
where
    F: Fn(
            Payload,
            tf_rust_socketio::asynchronous::Client,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        + 'static
        + Send
        + Sync,
{
    let ready = Arc::new(tokio::sync::Notify::new());
    let signal = ready.clone();
    let client = ClientBuilder::new(server_url)
        .on(tf_rust_socketio::Event::Connect, move |_, _| {
            let signal = signal.clone();
            Box::pin(async move {
                signal.notify_one();
            })
        })
        .transport_type(TransportType::Websocket)
        .namespace(namespace)
        .auth(serde_json::json!({"token": "test_secret"}))
        .on(event, handler)
        .connect()
        .await
        .expect("Failed to connect client");
    tokio::time::timeout(Duration::from_secs(5), ready.notified())
        .await
        .expect("namespace connect timeout");
    client
}

/// 创建原子布尔标记的处理器
#[allow(dead_code)]
pub fn create_atomic_handler(
    flag: Arc<AtomicBool>,
) -> impl Fn(
    Payload,
    tf_rust_socketio::asynchronous::Client,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
       + Send
       + Sync {
    move |payload: Payload, _client| {
        let flag = flag.clone();
        Box::pin(async move {
            if let Payload::Text(_, _) = payload {
                flag.store(true, Ordering::SeqCst);
            }
        })
    }
}

/// 加入办公室的辅助函数
pub async fn join_office(
    client: &tf_rust_socketio::asynchronous::Client,
    role: Role,
    office_id: &str,
    name: &str,
) {
    let join_req = json!({
        "role": role.to_string(),
        "office_id": office_id,
        "name": name
    });

    // 使用 emit_with_ack 确保服务器处理了请求
    let (result_tx, result_rx) = oneshot::channel::<serde_json::Value>();

    // CI 环境下增加超时时间
    let ack_timeout = Duration::from_secs(30);

    client
        .emit_with_ack(
            "server:join_office",
            json!(join_req),
            ack_timeout,
            ack_to_sender(result_tx, |p| match p {
                Payload::Text(mut values, _) => values.pop().unwrap_or(serde_json::Value::Null),
                _ => serde_json::Value::Null,
            }),
        )
        .await
        .expect("join_office emit_with_ack failed");

    // 等待响应
    let result = tokio::time::timeout(ack_timeout, result_rx)
        .await
        .unwrap_or_else(|_| {
            panic!(
                "join_office ack timeout after {:?} for role={:?}, office={}, name={}",
                ack_timeout, role, office_id, name
            )
        })
        .unwrap();

    // 成功回空 ack（线格式为零参 ACK `[]`）；失败回 flat ErrorPayload。
    if result.get("code").is_some() {
        panic!("Failed to join office: {}", result);
    }
    assert_empty_ack(&result, "join_office");
}

/// ack 载荷归一化：剥掉 Socket.IO ack 的实参包装，得到「单个载荷值」。
///
/// 三种形态统一：单参 ack 的 `payload`、被 1-tuple 包裹的 `[payload]`（socketioxide 的包裹形态）、
/// 以及零参空 ack `[]`（原样保留供 [`assert_empty_ack`] 比对）。
#[allow(dead_code)]
pub fn normalize_ack(payload: Payload) -> serde_json::Value {
    match payload {
        Payload::Text(mut values, _) => match values.pop().unwrap_or_default() {
            serde_json::Value::Array(mut args) if args.len() == 1 => args.pop().unwrap_or_default(),
            value => value,
        },
        _ => serde_json::Value::Null,
    }
}

/// 发出任意**有 ack** 的请求并把归一化后的 ack 载荷**原样**返回（不做任何裁决断言）。
///
/// 供拒绝类与幂等重入类场景读原始 ack；需要「成功且非空」语义的调用方自行断言载荷形状。
#[allow(dead_code)]
pub async fn emit_request(
    client: &tf_rust_socketio::asynchronous::Client,
    event: &str,
    data: serde_json::Value,
) -> serde_json::Value {
    let (result_tx, result_rx) = oneshot::channel::<serde_json::Value>();

    // CI 环境下增加超时时间（与 `join_office` 同款）。
    let ack_timeout = Duration::from_secs(30);

    client
        .emit_with_ack(
            event,
            data,
            ack_timeout,
            ack_to_sender(result_tx, normalize_ack),
        )
        .await
        .unwrap_or_else(|e| panic!("{event} emit_with_ack failed: {e}"));

    tokio::time::timeout(ack_timeout, result_rx)
        .await
        .unwrap_or_else(|_| panic!("{event} ack timeout"))
        .unwrap()
}

/// 发送 `server:join_office` 并把 ack 载荷**原样**返回（成功 = 零参空 ack `[]`；失败 = flat ErrorPayload）。
///
/// 与 [`join_office`]（断言成功、被拒即 panic）互补：本助手**不**做裁决断言，供拒绝类场景
/// （`4101` / `4106` / `403`…）、幂等重入（需与 `[]` 比对）与「无重广播」等场景读原始 ack。
#[allow(dead_code)]
pub async fn emit_join_for_ack(
    client: &tf_rust_socketio::asynchronous::Client,
    join_req: serde_json::Value,
) -> serde_json::Value {
    emit_request(client, "server:join_office", join_req).await
}

/// 离开办公室的辅助函数
#[allow(dead_code)]
pub async fn leave_office(client: &tf_rust_socketio::asynchronous::Client, office_id: &str) {
    let leave_req = json!({
        "office_id": office_id
    });

    client
        .emit("server:leave_office", leave_req)
        .await
        .expect("Failed to emit leave_office");

    // 等待离开完成
    sleep(Duration::from_millis(100)).await;
}
