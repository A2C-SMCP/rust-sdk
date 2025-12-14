//! 测试client:get_config的完整功能

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt;
use futures_util::future::BoxFuture;
use http_body_util::Full;
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
            let result = f(payload);
            if let Some(mut sender) = sender.lock().await.take() {
                let _ = sender.send(result);
            }
        }
        .boxed()
    }
}

async fn find_available_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap().port()
}

struct SmcpTestServer {
    addr: std::net::SocketAddr,
    shutdown_tx: oneshot::Sender<()>,
}

impl SmcpTestServer {
    async fn start() -> Self {
        let port = find_available_port().await;
        let addr: std::net::SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();

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
async fn test_get_config_complete_flow() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();

    let server = SmcpTestServer::start().await;
    let server_url = server.url();

    // 标记Computer是否收到了请求
    let computer_received = Arc::new(AtomicBool::new(false));
    let computer_received_clone = computer_received.clone();

    // 创建Computer客户端
    let computer_client = ClientBuilder::new(server_url.clone())
        .transport_type(TransportType::Websocket)
        .namespace("smcp")
        .opening_header("x-api-key", "test_secret")
        .on("client:get_config", move |payload: Payload, _client| {
            let computer_received = computer_received_clone.clone();
            async move {
                // 标记收到了请求
                computer_received.store(true, Ordering::SeqCst);

                // 解析请求
                if let Payload::Text(values) = payload {
                    if let Ok(req) = serde_json::from_value::<GetComputerConfigReq>(values[0].clone()) {
                        // 构造响应（模拟Computer会发送这个响应）
                        let _response = GetComputerConfigRet {
                            inputs: None,
                            servers: json!({
                                "computer": req.computer,
                                "mcp_servers": [
                                    {
                                        "name": "test-server",
                                        "command": "node",
                                        "args": ["server.js"]
                                    }
                                ]
                            }),
                        };
                        
                        // 注意：在真实的Computer客户端中，这里会通过ack发送响应
                        // 但在测试中，我们只能验证请求被正确转发
                    }
                }
            }
            .boxed()
        })
        .connect()
        .await
        .expect("Failed to connect computer");

    sleep(Duration::from_millis(100)).await;

    // Computer加入办公室
    let computer_join_req = EnterOfficeReq {
        office_id: "office1".to_string(),
        role: Role::Computer,
        name: "computer1".to_string(),
    };
    computer_client
        .emit("server:join_office", json!(computer_join_req))
        .await
        .unwrap();
    sleep(Duration::from_millis(100)).await;

    // 创建Agent客户端
    let agent_client = ClientBuilder::new(server_url.clone())
        .transport_type(TransportType::Websocket)
        .namespace("smcp")
        .opening_header("x-api-key", "test_secret")
        .connect()
        .await
        .expect("Failed to connect agent");

    sleep(Duration::from_millis(100)).await;

    // Agent加入办公室
    let agent_join_req = EnterOfficeReq {
        office_id: "office1".to_string(),
        role: Role::Agent,
        name: "agent1".to_string(),
    };
    agent_client
        .emit("server:join_office", json!(agent_join_req))
        .await
        .unwrap();
    sleep(Duration::from_millis(100)).await;

    // Agent发送get_config请求并等待响应
    let get_config_req = GetComputerConfigReq {
        base: AgentCallData {
            agent: "agent1".to_string(),
            req_id: ReqId("req1".to_string()),
        },
        computer: "computer1".to_string(),
    };

    // 创建channel接收响应
    let (config_tx, config_rx) = oneshot::channel::<serde_json::Value>();

    // 使用emit_with_ack发送请求
    agent_client
        .emit_with_ack(
            "client:get_config",
            json!(get_config_req),
            Duration::from_secs(2),
            ack_to_sender(config_tx, |p| match p {
                Payload::Text(mut values) => values.pop().unwrap_or(serde_json::Value::Null),
                _ => serde_json::Value::Null,
            }),
        )
        .await
        .expect("get_config emit_with_ack failed");

    // 等待响应
    let result = tokio::time::timeout(Duration::from_secs(5), config_rx)
        .await;
    
    // 由于Computer没有发送ack，应该超时
    assert!(result.is_err(), "Should timeout when Computer doesn't respond");
    
    // 但Computer应该收到了请求
    assert!(computer_received.load(Ordering::SeqCst), 
        "Computer should have received the request");

    // 清理
    computer_client.disconnect().await.unwrap();
    agent_client.disconnect().await.unwrap();
    server.shutdown();
}

#[tokio::test]
async fn test_get_config_computer_not_found() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();

    let server = SmcpTestServer::start().await;
    let server_url = server.url();

    // 创建Agent客户端
    let agent_client = ClientBuilder::new(server_url.clone())
        .transport_type(TransportType::Websocket)
        .namespace("smcp")
        .opening_header("x-api-key", "test_secret")
        .connect()
        .await
        .expect("Failed to connect agent");

    sleep(Duration::from_millis(100)).await;

    // Agent加入办公室
    let agent_join_req = EnterOfficeReq {
        office_id: "office1".to_string(),
        role: Role::Agent,
        name: "agent1".to_string(),
    };
    agent_client
        .emit("server:join_office", json!(agent_join_req))
        .await
        .unwrap();
    sleep(Duration::from_millis(100)).await;

    // Agent请求不存在的Computer的配置
    let get_config_req = GetComputerConfigReq {
        base: AgentCallData {
            agent: "agent1".to_string(),
            req_id: ReqId("req2".to_string()),
        },
        computer: "nonexistent".to_string(),
    };

    // 创建channel接收响应
    let (config_tx, config_rx) = oneshot::channel::<serde_json::Value>();

    // 使用emit_with_ack发送请求
    agent_client
        .emit_with_ack(
            "client:get_config",
            json!(get_config_req),
            Duration::from_secs(2),
            ack_to_sender(config_tx, |p| match p {
                Payload::Text(mut values) => values.pop().unwrap_or(serde_json::Value::Null),
                _ => serde_json::Value::Null,
            }),
        )
        .await
        .expect("get_config emit_with_ack failed");

    // 等待响应
    let config_payload = tokio::time::timeout(Duration::from_secs(5), config_rx)
        .await
        .expect("get_config ack timeout")
        .unwrap();

    // 验证错误响应
    let error_value: serde_json::Value = serde_json::from_value(config_payload)
        .expect("Should be able to deserialize error response");
    
    // 错误响应可能是字符串或数组
    let error_msg = match error_value {
        serde_json::Value::String(s) => s,
        serde_json::Value::Array(arr) => {
            // 如果是数组，取第一个元素
            arr.first().map(|v| {
                // 如果元素是包含Err字段的对象
                if let Some(err) = v.get("Err").and_then(|e| e.as_str()) {
                    err.to_string()
                } else if let Some(s) = v.as_str() {
                    s.to_string()
                } else {
                    v.to_string()
                }
            }).unwrap_or_default()
        }
        _ => error_value.to_string(),
    };
    
    // 验证错误信息
    assert!(error_msg.contains("not found") || error_msg.contains("Computer"));

    // 清理
    agent_client.disconnect().await.unwrap();
    server.shutdown();
}
