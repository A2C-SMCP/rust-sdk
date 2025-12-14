use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt;
use http_body_util::Full;
use rust_socketio::asynchronous::ClientBuilder;
use rust_socketio::Payload;
use rust_socketio::TransportType;
use serde_json::json;
use smcp::*;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::time::sleep;
use tower::Layer;
use tower::Service;

use smcp_server_core::{DefaultAuthenticationProvider, SmcpServerBuilder};

async fn find_available_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap().port()
}

struct SmcpTestServer {
    addr: SocketAddr,
    shutdown_tx: oneshot::Sender<()>,
}

impl SmcpTestServer {
    async fn start() -> Self {
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
                            let io = hyper_util::rt::TokioIo::new(stream);
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

        tokio::time::sleep(Duration::from_millis(100)).await;

        SmcpTestServer {
            addr: actual_addr,
            shutdown_tx,
        }
    }

    fn url(&self) -> String {
        format!("http://{}/", self.addr)
    }

    fn shutdown(self) {
        let _ = self.shutdown_tx.send(());
    }
}

#[tokio::test]
async fn test_computer_switch_room_broadcasts_leave_notification() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();

    let server = SmcpTestServer::start().await;
    let server_url = server.url();

    // 创建监听器来跟踪接收到的通知
    let leave_received = Arc::new(AtomicBool::new(false));
    let enter_received = Arc::new(AtomicBool::new(false));

    let leave_received_clone = leave_received.clone();
    let enter_received_clone = enter_received.clone();

    // 创建第一个客户端（监听者）
    let client1 = ClientBuilder::new(server_url.clone())
        .transport_type(TransportType::Websocket)
        .namespace("smcp")
        .opening_header("x-api-key", "test_secret")
        .on("notify:leave_office", move |payload: Payload, _client| {
            let leave_received = leave_received_clone.clone();
            async move {
                if let Payload::Text(values) = payload {
                    if let Ok(text) = serde_json::to_string(&values[0]) {
                        if text.contains("computer1") && text.contains("office1") {
                            leave_received.store(true, Ordering::SeqCst);
                        }
                    }
                }
            }
            .boxed()
        })
        .on("notify:enter_office", move |payload: Payload, _client| {
            let enter_received = enter_received_clone.clone();
            async move {
                if let Payload::Text(values) = payload {
                    if let Ok(text) = serde_json::to_string(&values[0]) {
                        if text.contains("computer1") && text.contains("office2") {
                            enter_received.store(true, Ordering::SeqCst);
                        }
                    }
                }
            }
            .boxed()
        })
        .connect()
        .await
        .expect("Connection failed");

    // 等待连接建立
    sleep(Duration::from_millis(100)).await;

    // 让client1加入office1
    let join_req1 = EnterOfficeReq {
        office_id: "office1".to_string(),
        role: Role::Computer,
        name: "computer1".to_string(),
    };

    client1
        .emit("server:join_office", json!(join_req1))
        .await
        .unwrap();
    sleep(Duration::from_millis(100)).await;

    // 创建第二个客户端（在office1中监听）
    let client2 = ClientBuilder::new(server_url.clone())
        .transport_type(TransportType::Websocket)
        .namespace("smcp")
        .opening_header("x-api-key", "test_secret")
        .connect()
        .await
        .expect("Connection failed");

    sleep(Duration::from_millis(100)).await;

    // 让client2加入office1作为agent
    let join_req2 = EnterOfficeReq {
        office_id: "office1".to_string(),
        role: Role::Agent,
        name: "agent1".to_string(),
    };

    client2
        .emit("server:join_office", json!(join_req2))
        .await
        .unwrap();
    sleep(Duration::from_millis(100)).await;

    // 现在，computer1从office1切换到office2
    let join_req3 = EnterOfficeReq {
        office_id: "office2".to_string(),
        role: Role::Computer,
        name: "computer1".to_string(),
    };

    client1
        .emit("server:join_office", json!(join_req3))
        .await
        .unwrap();
    sleep(Duration::from_millis(200)).await;

    // 验证client2（在office1中）收到了leave_office通知
    assert!(
        leave_received.load(Ordering::SeqCst),
        "Expected leave_office notification not received"
    );

    // 清理
    client1.disconnect().await.unwrap();
    client2.disconnect().await.unwrap();
    server.shutdown();
}

#[tokio::test]
async fn test_join_office_broadcasts_only_to_room() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();

    let server = SmcpTestServer::start().await;
    let server_url = server.url();

    // 创建监听器来跟踪接收到的通知
    let office1_received = Arc::new(AtomicBool::new(false));
    let office2_received = Arc::new(AtomicBool::new(false));
    let global_received = Arc::new(AtomicBool::new(false));

    let office1_received_clone = office1_received.clone();
    let office2_received_clone = office2_received.clone();
    let global_received_clone = global_received.clone();

    // 创建client1（将在office1中）
    let client1 = ClientBuilder::new(server_url.clone())
        .transport_type(TransportType::Websocket)
        .namespace("smcp")
        .opening_header("x-api-key", "test_secret")
        .on("notify:enter_office", move |payload: Payload, _client| {
            let office1_received = office1_received_clone.clone();
            async move {
                if let Payload::Text(values) = payload {
                    if let Ok(text) = serde_json::to_string(&values[0]) {
                        if text.contains("computer1") && text.contains("office1") {
                            office1_received.store(true, Ordering::SeqCst);
                        }
                    }
                }
            }
            .boxed()
        })
        .connect()
        .await
        .expect("Connection failed");

    sleep(Duration::from_millis(100)).await;

    // 创建client2（将在office2中）
    let client2 = ClientBuilder::new(server_url.clone())
        .transport_type(TransportType::Websocket)
        .namespace("smcp")
        .opening_header("x-api-key", "test_secret")
        .on("notify:enter_office", move |payload: Payload, _client| {
            let office2_received = office2_received_clone.clone();
            async move {
                if let Payload::Text(values) = payload {
                    if let Ok(text) = serde_json::to_string(&values[0]) {
                        if text.contains("computer1") {
                            office2_received.store(true, Ordering::SeqCst);
                        }
                    }
                }
            }
            .boxed()
        })
        .connect()
        .await
        .expect("Connection failed");

    sleep(Duration::from_millis(100)).await;

    // 创建client3（全局监听者，不在任何房间）
    let client3 = ClientBuilder::new(server_url.clone())
        .transport_type(TransportType::Websocket)
        .namespace("smcp")
        .opening_header("x-api-key", "test_secret")
        .on("notify:enter_office", move |payload: Payload, _client| {
            let global_received = global_received_clone.clone();
            async move {
                if let Payload::Text(values) = payload {
                    if let Ok(text) = serde_json::to_string(&values[0]) {
                        if text.contains("computer1") {
                            global_received.store(true, Ordering::SeqCst);
                        }
                    }
                }
            }
            .boxed()
        })
        .connect()
        .await
        .expect("Connection failed");

    sleep(Duration::from_millis(100)).await;

    // client1加入office1
    let join_req1 = EnterOfficeReq {
        office_id: "office1".to_string(),
        role: Role::Agent,
        name: "agent1".to_string(),
    };

    client1
        .emit("server:join_office", json!(join_req1))
        .await
        .unwrap();
    sleep(Duration::from_millis(100)).await;

    // client2加入office2
    let join_req2 = EnterOfficeReq {
        office_id: "office2".to_string(),
        role: Role::Agent,
        name: "agent2".to_string(),
    };

    client2
        .emit("server:join_office", json!(join_req2))
        .await
        .unwrap();
    sleep(Duration::from_millis(100)).await;

    // 现在，computer1加入office1
    let computer_join_req = EnterOfficeReq {
        office_id: "office1".to_string(),
        role: Role::Computer,
        name: "computer1".to_string(),
    };

    // 使用新的客户端连接来发送join请求
    let computer_client = ClientBuilder::new(server_url.clone())
        .transport_type(TransportType::Websocket)
        .namespace("smcp")
        .opening_header("x-api-key", "test_secret")
        .connect()
        .await
        .expect("Connection failed");

    sleep(Duration::from_millis(100)).await;

    computer_client
        .emit("server:join_office", json!(computer_join_req))
        .await
        .unwrap();
    sleep(Duration::from_millis(200)).await;

    // 验证只有office1中的客户端收到了通知
    assert!(
        office1_received.load(Ordering::SeqCst),
        "office1 should receive enter_office notification"
    );
    assert!(
        !office2_received.load(Ordering::SeqCst),
        "office2 should not receive notification"
    );
    assert!(
        !global_received.load(Ordering::SeqCst),
        "global listener should not receive notification"
    );

    // 清理
    client1.disconnect().await.unwrap();
    client2.disconnect().await.unwrap();
    client3.disconnect().await.unwrap();
    computer_client.disconnect().await.unwrap();
    server.shutdown();
}
