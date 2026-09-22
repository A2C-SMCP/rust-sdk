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
use serde_json::json;
use smcp::*;
use tf_rust_socketio::asynchronous::ClientBuilder;
use tf_rust_socketio::Payload;
use tf_rust_socketio::TransportType;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::time::sleep;
use tower::{Layer, Service};

use smcp_server_core::{DefaultAuthenticationProvider, SmcpServerBuilder};

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
    ClientBuilder::new(server_url)
        .transport_type(TransportType::Websocket)
        .namespace(namespace)
        .auth(serde_json::json!({"token": "test_secret"}))
        .connect()
        .await
        .expect("Failed to connect client")
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
    ClientBuilder::new(server_url)
        .transport_type(TransportType::Websocket)
        .namespace(namespace)
        .auth(serde_json::json!({"token": "test_secret"}))
        .on(event, handler)
        .connect()
        .await
        .expect("Failed to connect client")
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
