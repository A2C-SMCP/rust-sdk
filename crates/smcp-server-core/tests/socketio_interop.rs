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

use futures_util::future::BoxFuture;
use http_body_util::Full;
use hyper::body::Bytes;
use smcp_server_core::{DefaultAuthenticationProvider, SessionManager, SmcpServerBuilder};
use tower::{Layer, Service};

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
                                            Ok::<_, std::convert::Infallible>(hyper::Response::new(Full::<Bytes>::from(Bytes::new())))
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

fn ack_to_sender<T: Send + 'static>(
    sender: oneshot::Sender<T>,
    f: impl Fn(Payload) -> T + Send + Sync + 'static,
) -> impl FnMut(Payload, rust_socketio::asynchronous::Client) -> BoxFuture<'static, ()> + Send + Sync {
    let sender = Arc::new(tokio::sync::Mutex::new(Some(sender)));
    let f = Arc::new(f);
    move |payload: Payload, _client| {
        let sender = sender.clone();
        let f = f.clone();
        async move {
            let sender = sender.lock().await.take();
            if let Some(sender) = sender {
                let _ = sender.send(f(payload));
            }
        }
        .boxed()
    }
}

#[tokio::test]
async fn test_smcp_handler_join_list_leave_and_invalid_get_tools() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .try_init();

    let server = SmcpTestServer::start().await;
    let server_url = server.url();

    let connected_notify = Arc::new(Notify::new());
    let connected_notify_clone = connected_notify.clone();

    let client = ClientBuilder::new(&server_url)
        .namespace(smcp::SMCP_NAMESPACE)
        .transport_type(TransportType::Websocket)
        .opening_header("x-api-key", "test_secret")
        .on("connect", move |_payload: Payload, _client| {
            let connected_notify = connected_notify_clone.clone();
            async move {
                connected_notify.notify_one();
            }
            .boxed()
        })
        .connect()
        .await
        .expect("Failed to connect to SMCP server");

    let _ = timeout(Duration::from_secs(5), connected_notify.notified()).await;

    let (join_tx, join_rx) = oneshot::channel::<serde_json::Value>();
    client
        .emit_with_ack(
            smcp::events::SERVER_JOIN_OFFICE,
            serde_json::json!({
                "role": "computer",
                "name": "c1",
                "office_id": "office1"
            }),
            Duration::from_secs(2),
            ack_to_sender(join_tx, |p| match p {
                Payload::Text(mut values) => values.pop().unwrap_or(serde_json::Value::Null),
                Payload::String(s) => serde_json::Value::String(s),
                _ => serde_json::Value::Null,
            }),
        )
        .await
        .expect("join_office emit_with_ack failed");
    let join_payload = timeout(Duration::from_secs(5), join_rx).await.unwrap().unwrap();
    assert!(join_payload.to_string().contains("Ok"));

    let (list_tx, list_rx) = oneshot::channel::<serde_json::Value>();
    client
        .emit_with_ack(
            smcp::events::SERVER_LIST_ROOM,
            serde_json::json!({
                "agent": "agent1",
                "req_id": "rid1",
                "office_id": "office1"
            }),
            Duration::from_secs(2),
            ack_to_sender(list_tx, |p| match p {
                Payload::Text(mut values) => values.pop().unwrap_or(serde_json::Value::Null),
                Payload::String(s) => serde_json::from_str(&s).unwrap_or(serde_json::Value::String(s)),
                _ => serde_json::Value::Null,
            }),
        )
        .await
        .expect("list_room emit_with_ack failed");

    let list_raw = timeout(Duration::from_secs(5), list_rx)
        .await
        .unwrap()
        .unwrap();

    // ack 可能是 [ {"Ok": {...}} ] 的数组包裹形式
    let list_raw = match list_raw {
        serde_json::Value::Array(mut a) if a.len() == 1 => a.pop().unwrap_or(serde_json::Value::Null),
        v => v,
    };

    let list_payload = if let Some(ok) = list_raw.get("Ok") {
        ok.clone()
    } else if let Some(err) = list_raw.get("Err") {
        panic!("list_room returned Err: {}", err);
    } else {
        list_raw
    };

    let list_ret: smcp::ListRoomRet = serde_json::from_value(list_payload.clone())
        .unwrap_or_else(|e| panic!("failed to deserialize ListRoomRet from payload: {list_payload}. err: {e}"));
    assert_eq!(list_ret.req_id.as_str(), "rid1");
    assert!(list_ret.sessions.iter().any(|s| s.name == "c1"));

    let (get_tools_tx, get_tools_rx) = oneshot::channel::<serde_json::Value>();
    client
        .emit_with_ack(
            smcp::events::CLIENT_GET_TOOLS,
            serde_json::json!({
                "agent": "agent1",
                "req_id": "rid2",
                "computer": "c1"
            }),
            Duration::from_secs(2),
            ack_to_sender(get_tools_tx, |p| match p {
                Payload::Text(mut values) => values.pop().unwrap_or(serde_json::Value::Null),
                Payload::String(s) => serde_json::Value::String(s),
                _ => serde_json::Value::Null,
            }),
        )
        .await
        .expect("get_tools emit_with_ack failed");
    let get_tools_payload = timeout(Duration::from_secs(5), get_tools_rx).await.unwrap().unwrap();
    assert!(
        get_tools_payload.to_string().contains("Message forwarding not yet implemented"),
        "unexpected get_tools response: {}",
        get_tools_payload
    );

    let (leave_tx, leave_rx) = oneshot::channel::<serde_json::Value>();
    client
        .emit_with_ack(
            smcp::events::SERVER_LEAVE_OFFICE,
            serde_json::json!({ "office_id": "office1" }),
            Duration::from_secs(2),
            ack_to_sender(leave_tx, |p| match p {
                Payload::Text(mut values) => values.pop().unwrap_or(serde_json::Value::Null),
                Payload::String(s) => serde_json::Value::String(s),
                _ => serde_json::Value::Null,
            }),
        )
        .await
        .expect("leave_office emit_with_ack failed");
    let leave_payload = timeout(Duration::from_secs(5), leave_rx).await.unwrap().unwrap();
    assert!(leave_payload.to_string().contains("Ok"));

    client.disconnect().await.expect("Failed to disconnect");
    server.shutdown();
}

#[test]
fn test_session_manager_default_is_new() {
    let manager = SessionManager::default();
    assert_eq!(manager.get_stats().total, 0);
}

#[test]
fn test_smcp_req_id_helpers_from_integration_test() {
    let req_id = smcp::ReqId::from_string("abc".to_string());
    assert_eq!(req_id.as_str(), "abc");
    let req_id2 = smcp::ReqId::default();
    assert!(!req_id2.as_str().is_empty());
}
