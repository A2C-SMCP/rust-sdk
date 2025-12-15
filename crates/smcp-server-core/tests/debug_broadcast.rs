//! Debug test to verify broadcast reception

#[path = "test_utils.rs"]
mod test_utils;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt;
use rust_socketio::asynchronous::ClientBuilder;
use rust_socketio::TransportType;
use serde_json::json;
use tokio::time::sleep;

use smcp::*;
use test_utils::*;

#[tokio::test]
async fn debug_agent_receives_broadcast() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();

    let server = SmcpTestServer::start().await;
    let server_url = server.url();

    // 标记Agent是否收到了通知
    let agent_received = Arc::new(AtomicBool::new(false));
    let agent_received_clone = agent_received.clone();

    // 创建Agent客户端（监听所有事件）
    let agent_client = ClientBuilder::new(server_url.clone())
        .transport_type(TransportType::Websocket)
        .namespace(SMCP_NAMESPACE)
        .opening_header("x-api-key", "test_secret")
        .on("message", move |_payload, _client| {
            println!("Agent received message event");
            async move {}.boxed()
        })
        .on_any(move |event, _payload, _client| {
            let agent_received = agent_received_clone.clone();
            async move {
                println!("Agent received event: {}", event);
                if event.to_string() == "notify:update_config" {
                    agent_received.store(true, Ordering::SeqCst);
                }
            }
            .boxed()
        })
        .connect()
        .await
        .expect("Failed to connect agent");

    sleep(Duration::from_millis(100)).await;

    // Agent加入办公室
    join_office(&agent_client, Role::Agent, "office1", "agent1").await;

    // 等待确保Agent完全加入办公室
    sleep(Duration::from_millis(500)).await;

    // 创建Computer客户端
    let computer_client = create_test_client(&server_url, SMCP_NAMESPACE).await;
    
    // 等待确保Computer客户端连接完全建立
    sleep(Duration::from_millis(200)).await;
    
    join_office(&computer_client, Role::Computer, "office1", "computer1").await;

    // 等待确保Computer也加入办公室
    sleep(Duration::from_millis(500)).await;

    // Computer触发配置更新
    let update_config_req = json!({});

    println!("Computer sending update_config...");
    computer_client
        .emit("server:update_config", update_config_req)
        .await
        .expect("Failed to emit update_config");

    // 等待广播传播
    sleep(Duration::from_millis(500)).await;

    // 验证Agent收到了通知
    println!(
        "Agent received notification: {}",
        agent_received.load(Ordering::SeqCst)
    );

    // 清理
    computer_client.disconnect().await.unwrap();
    agent_client.disconnect().await.unwrap();
    server.shutdown();
}
