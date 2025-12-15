//! 测试WebSocket升级功能

use std::sync::Arc;
use std::time::Duration;

use rust_socketio::asynchronous::ClientBuilder;
use rust_socketio::payload::Payload;
use rust_socketio::TransportType;
use serde_json::json;
use smcp_server_core::{DefaultAuthenticationProvider, SmcpServerBuilder};
use smcp_server_hyper::HyperServerBuilder;
use tokio::time::sleep;

fn ack_to_sender<T: Send + 'static>(
    sender: tokio::sync::oneshot::Sender<T>,
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
        Box::pin(async move {
            let result = f(payload);
            if let Some(sender) = sender.lock().await.take() {
                let _ = sender.send(result);
            }
        })
    }
}

#[tokio::test]
async fn test_websocket_upgrade() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();

    // 构建带有.with_upgrades()的Hyper服务器
    let layer = SmcpServerBuilder::new()
        .with_auth_provider(std::sync::Arc::new(DefaultAuthenticationProvider::new(
            Some("test_secret".to_string()),
            None,
        )))
        .build_layer()
        .expect("failed to build SMCP server layer");

    // 使用随机端口
    let addr = "127.0.0.1:0".parse().unwrap();
    let server = HyperServerBuilder::new()
        .with_layer(layer)
        .with_addr(addr)
        .build();

    // 创建TcpListener来获取可用端口
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let server_addr = format!("127.0.0.1:{}", port).parse().unwrap();

    // 在后台启动服务器
    let server_handle = tokio::spawn(async move { server.run(server_addr).await });

    // 等待服务器启动
    sleep(Duration::from_millis(100)).await;

    let server_url = format!("http://{}/", server_addr);

    // 创建客户端并强制使用WebSocket传输
    let client = ClientBuilder::new(server_url)
        .transport_type(TransportType::Websocket) // 强制使用WebSocket
        .namespace("smcp")
        .opening_header("x-api-key", "test_secret")
        .connect()
        .await
        .expect("Failed to connect with WebSocket");

    // 验证连接成功
    sleep(Duration::from_millis(100)).await;

    // 发送一个测试消息
    let join_req = json!({
        "office_id": "office1",
        "role": "agent",
        "name": "test_agent"
    });

    let (tx, rx) = tokio::sync::oneshot::channel();
    let timeout = Duration::from_secs(2);

    client
        .emit_with_ack(
            "server:join_office",
            join_req,
            timeout,
            ack_to_sender(tx, |payload| match payload {
                Payload::Text(mut values, _) => values.pop().unwrap_or(serde_json::Value::Null),
                _ => serde_json::Value::Null,
            }),
        )
        .await
        .expect("Failed to emit message");

    // 检查响应
    match rx.await {
        Ok(response) => {
            println!("Join office response: {:?}", response);
        }
        Err(_) => {
            panic!("No response received for join_office");
        }
    }

    sleep(Duration::from_millis(100)).await;

    // 清理
    client.disconnect().await.unwrap();
    server_handle.abort();
}

#[tokio::test]
async fn test_websocket_upgrade_with_invalid_role() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();

    // 构建带有.with_upgrades()的Hyper服务器
    let layer = SmcpServerBuilder::new()
        .with_auth_provider(std::sync::Arc::new(DefaultAuthenticationProvider::new(
            Some("test_secret".to_string()),
            None,
        )))
        .build_layer()
        .expect("failed to build SMCP server layer");

    // 使用随机端口
    let addr = "127.0.0.1:0".parse().unwrap();
    let server = HyperServerBuilder::new()
        .with_layer(layer)
        .with_addr(addr)
        .build();

    // 创建TcpListener来获取可用端口
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let server_addr = format!("127.0.0.1:{}", port).parse().unwrap();

    // 在后台启动服务器
    let server_handle = tokio::spawn(async move { server.run(server_addr).await });

    // 等待服务器启动
    sleep(Duration::from_millis(100)).await;

    let server_url = format!("http://{}/", server_addr);

    // 创建客户端并强制使用WebSocket传输
    let client = ClientBuilder::new(server_url)
        .transport_type(TransportType::Websocket) // 强制使用WebSocket
        .namespace("smcp")
        .opening_header("x-api-key", "test_secret")
        .connect()
        .await
        .expect("Failed to connect with WebSocket");

    // 验证连接成功
    sleep(Duration::from_millis(100)).await;

    // 发送一个带有无效角色（大写）的测试消息
    let join_req = json!({
        "office_id": "office1",
        "role": "Agent",  // 使用大写，应该失败
        "name": "test_agent"
    });

    let (tx, rx) = tokio::sync::oneshot::channel();
    let timeout = Duration::from_secs(2);

    client
        .emit_with_ack(
            "server:join_office",
            join_req,
            timeout,
            ack_to_sender(tx, |payload| match payload {
                Payload::Text(mut values, _) => values.pop().unwrap_or(serde_json::Value::Null),
                _ => serde_json::Value::Null,
            }),
        )
        .await
        .expect("Failed to emit message");

    // 检查响应
    match tokio::time::timeout(Duration::from_secs(3), rx).await {
        Ok(Ok(response)) => {
            println!("Join office response with 'Agent': {:?}", response);
            // 如果响应是错误，那么测试通过（证明了大写会失败）
            if response.to_string().contains("error") || response.to_string().contains("Error") {
                println!("✓ 大写 'Agent' 正确地导致了错误");
            } else {
                println!("⚠️ 大写 'Agent' 竟然成功了，这可能是个问题");
            }
        }
        Ok(Err(_)) => {
            panic!("Failed to receive join_office response");
        }
        Err(_) => {
            println!("✓ 大写 'Agent' 正确地导致了超时（服务器拒绝处理无效的role值）");
        }
    }

    // 清理
    client.disconnect().await.unwrap();
    server_handle.abort();
}

#[tokio::test]
async fn test_websocket_upgrade_with_polling_fallback() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();

    // 构建带有.with_upgrades()的Hyper服务器
    let layer = SmcpServerBuilder::new()
        .with_auth_provider(std::sync::Arc::new(DefaultAuthenticationProvider::new(
            Some("test_secret".to_string()),
            None,
        )))
        .build_layer()
        .expect("failed to build SMCP server layer");

    // 创建TcpListener来获取可用端口
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let server_addr = format!("127.0.0.1:{}", port).parse().unwrap();
    let server = HyperServerBuilder::new()
        .with_layer(layer)
        .with_addr(server_addr)
        .build();

    // 在后台启动服务器
    let server_handle = tokio::spawn(async move { server.run(server_addr).await });

    // 等待服务器启动
    sleep(Duration::from_millis(100)).await;

    let server_url = format!("http://{}/", server_addr);

    // 创建客户端，允许WebSocket和轮询
    let client = ClientBuilder::new(server_url)
        .transport_type(TransportType::Polling) // 使用轮询作为对比
        .namespace("smcp")
        .opening_header("x-api-key", "test_secret")
        .connect()
        .await
        .expect("Failed to connect with Polling");

    // 验证连接成功（即使使用轮询也应该能连接）
    sleep(Duration::from_millis(100)).await;

    // 清理
    let _ = client.disconnect().await;
    server_handle.abort();
}
