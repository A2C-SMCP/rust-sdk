//! 测试WebSocket升级功能

use std::time::Duration;

use rust_socketio::asynchronous::ClientBuilder;
use rust_socketio::TransportType;
use serde_json::json;
use smcp_server_core::{DefaultAuthenticationProvider, SmcpServerBuilder};
use smcp_server_hyper::HyperServerBuilder;
use tokio::time::sleep;

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
    let server_handle = tokio::spawn(async move {
        server.run(server_addr).await
    });

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
        "role": "Agent",
        "name": "test_agent"
    });

    client
        .emit("server:join_office", join_req)
        .await
        .expect("Failed to emit message");

    sleep(Duration::from_millis(100)).await;

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
    let server_handle = tokio::spawn(async move {
        server.run(server_addr).await
    });

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
    client.disconnect().await.unwrap();
    server_handle.abort();
}
