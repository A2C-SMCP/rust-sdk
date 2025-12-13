use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt;
use rust_socketio::TransportType;
use rust_socketio::asynchronous::ClientBuilder;
use rust_socketio::Payload;
use socketioxide::extract::Data;
use socketioxide::extract::SocketRef;
use socketioxide::SocketIo;
use tokio::net::TcpListener;
use tokio::sync::{oneshot, Notify};
use tokio::time::timeout;

async fn find_available_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap().port()
}

struct TestServer {
    addr: SocketAddr,
    shutdown_tx: oneshot::Sender<()>,
}

impl TestServer {
    async fn start() -> Self {
        let port = find_available_port().await;
        let addr: SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();

        let (svc, io) = SocketIo::new_svc();

        io.ns("/", |s: SocketRef| {
            tracing::info!("server: socket connected");
            s.on("smcp_ping", |s: SocketRef, _data: Data<serde_json::Value>| {
                tracing::info!("server: got smcp_ping");
                s.emit("smcp_pong", "pong").ok();
            });
        });

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
                            let svc = svc.clone();

                            tokio::spawn(async move {
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

        TestServer {
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
async fn test_socketioxide_and_rust_socketio_interop() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .try_init();

    let server = TestServer::start().await;
    let server_url = server.url();

    let pong_received = Arc::new(AtomicBool::new(false));
    let pong_received_clone = pong_received.clone();
    let pong_notify = Arc::new(Notify::new());
    let pong_notify_clone = pong_notify.clone();

    let connected_notify = Arc::new(Notify::new());
    let connected_notify_clone = connected_notify.clone();

    let client = ClientBuilder::new(&server_url)
        .namespace("/")
        .transport_type(TransportType::Websocket)
        .on("connect", move |_payload: Payload, _client| {
            let connected_notify = connected_notify_clone.clone();
            async move {
                tracing::info!("client: connected");
                connected_notify.notify_one();
            }
            .boxed()
        })
        .on("smcp_pong", move |_payload: Payload, _client| {
            let pong_received = pong_received_clone.clone();
            let notify = pong_notify_clone.clone();
            async move {
                tracing::info!("client: got smcp_pong");
                pong_received.store(true, Ordering::SeqCst);
                notify.notify_one();
            }
            .boxed()
        })
        .on("error", |err, _| {
            async move {
                eprintln!("Socket.IO error: {:?}", err);
            }
            .boxed()
        })
        .connect()
        .await
        .expect("Failed to connect to server");

    let _ = timeout(Duration::from_secs(5), connected_notify.notified()).await;

    client
        .emit("smcp_ping", serde_json::json!("ping"))
        .await
        .expect("Failed to emit ping");

    let result = timeout(Duration::from_secs(5), pong_notify.notified()).await;

    assert!(result.is_ok(), "Timeout waiting for pong response");
    assert!(
        pong_received.load(Ordering::SeqCst),
        "Should have received pong"
    );

    client.disconnect().await.expect("Failed to disconnect");
    server.shutdown();
}
