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
use rust_socketio::asynchronous::ClientBuilder;
use rust_socketio::Payload;
use rust_socketio::TransportType;
use serde_json::json;
use smcp::*;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::time::sleep;
use tower::{Layer, Service};

use smcp_server_core::{DefaultAuthenticationProvider, SmcpServerBuilder};

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
        format!("http://{}/", self.addr)
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
pub fn ack_to_sender<T: Send + 'static>(
    sender: oneshot::Sender<T>,
    f: impl Fn(Payload) -> T + Send + Sync + 'static,
) -> impl FnMut(
    Payload,
    rust_socketio::asynchronous::Client,
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
) -> rust_socketio::asynchronous::Client {
    ClientBuilder::new(server_url)
        .transport_type(TransportType::Websocket)
        .namespace(namespace)
        .opening_header("x-api-key", "test_secret")
        .connect()
        .await
        .expect("Failed to connect client")
}

/// 创建带事件处理器的测试客户端
pub async fn create_client_with_handler<F>(
    server_url: &str,
    namespace: &str,
    event: &str,
    handler: F,
) -> rust_socketio::asynchronous::Client
where
    F: FnMut(
            Payload,
            rust_socketio::asynchronous::Client,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        + 'static
        + Send
        + Sync,
{
    ClientBuilder::new(server_url)
        .transport_type(TransportType::Websocket)
        .namespace(namespace)
        .opening_header("x-api-key", "test_secret")
        .on(event, handler)
        .connect()
        .await
        .expect("Failed to connect client")
}

/// 创建原子布尔标记的处理器
pub fn create_atomic_handler(
    flag: Arc<AtomicBool>,
) -> impl FnMut(
    Payload,
    rust_socketio::asynchronous::Client,
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
    client: &rust_socketio::asynchronous::Client,
    role: Role,
    office_id: &str,
    name: &str,
) {
    let join_req = EnterOfficeReq {
        office_id: office_id.to_string(),
        role,
        name: name.to_string(),
    };

    client
        .emit("server:join_office", json!(join_req))
        .await
        .expect("Failed to emit join_office");

    // 等待加入完成
    sleep(Duration::from_millis(100)).await;
}

/// 离开办公室的辅助函数
pub async fn leave_office(client: &rust_socketio::asynchronous::Client, office_id: &str) {
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
