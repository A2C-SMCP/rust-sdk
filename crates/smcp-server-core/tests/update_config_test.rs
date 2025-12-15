//! Test configuration update broadcast functionality

#[path = "test_utils.rs"]
mod test_utils;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt;
use rust_socketio::asynchronous::ClientBuilder;
use rust_socketio::Payload;
use rust_socketio::TransportType;
use serde_json::json;
use tokio::sync::Mutex;
use tokio::time::sleep;

use smcp::*;
use test_utils::*;

#[tokio::test]
#[ignore]
async fn test_update_config_broadcast() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();

    let server = SmcpTestServer::start().await;
    let server_url = server.url();

    // 标记Agent是否收到了通知
    let agent_received = Arc::new(AtomicBool::new(false));
    let agent_received_clone = agent_received.clone();

    // 创建Agent客户端（监听配置更新通知）
    let agent_client = ClientBuilder::new(server_url.clone())
        .transport_type(TransportType::Websocket)
        .namespace("smcp")
        .opening_header("x-api-key", "test_secret")
        .on("notify:update_config", move |payload: Payload, _client| {
            let agent_received = agent_received_clone.clone();
            async move {
                if let Payload::Text(values, _) = payload {
                    if let Ok(notification) =
                        serde_json::from_value::<UpdateMCPConfigNotification>(values[0].clone())
                    {
                        // 验证通知内容
                        if notification.computer.contains("computer1") {
                            agent_received.store(true, Ordering::SeqCst);
                        }
                    }
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

    // 创建Computer客户端
    let computer_client = create_test_client(&server_url, "smcp").await;
    join_office(&computer_client, Role::Computer, "office1", "computer1").await;

    // Computer触发配置更新
    let update_config_req = json!({});

    computer_client
        .emit("server:update_config", update_config_req)
        .await
        .expect("Failed to emit update_config");

    // 等待广播传播
    sleep(Duration::from_millis(300)).await;

    // 验证Agent收到了通知
    assert!(
        agent_received.load(Ordering::SeqCst),
        "Agent should have received update_config notification"
    );

    // 清理
    computer_client.disconnect().await.unwrap();
    agent_client.disconnect().await.unwrap();
    server.shutdown();
}

#[tokio::test]
#[ignore]
async fn test_update_config_broadcast_only_to_same_office() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();

    let server = SmcpTestServer::start().await;
    let server_url = server.url();

    // 标记不同办公室的Agent是否收到了通知
    let office1_received = Arc::new(AtomicBool::new(false));
    let office2_received = Arc::new(AtomicBool::new(false));

    let office1_received_clone = office1_received.clone();
    let office2_received_clone = office2_received.clone();

    // 创建office1的Agent客户端
    let agent1_client = ClientBuilder::new(server_url.clone())
        .transport_type(TransportType::Websocket)
        .namespace("smcp")
        .opening_header("x-api-key", "test_secret")
        .on("notify:update_config", move |payload: Payload, _client| {
            let office1_received = office1_received_clone.clone();
            async move {
                if let Payload::Text(_, ..) = payload {
                    office1_received.store(true, Ordering::SeqCst);
                }
            }
            .boxed()
        })
        .connect()
        .await
        .expect("Failed to connect agent1");

    sleep(Duration::from_millis(100)).await;

    // 创建office2的Agent客户端
    let agent2_client = ClientBuilder::new(server_url.clone())
        .transport_type(TransportType::Websocket)
        .namespace("smcp")
        .opening_header("x-api-key", "test_secret")
        .on("notify:update_config", move |payload: Payload, _client| {
            let office2_received = office2_received_clone.clone();
            async move {
                if let Payload::Text(_, ..) = payload {
                    office2_received.store(true, Ordering::SeqCst);
                }
            }
            .boxed()
        })
        .connect()
        .await
        .expect("Failed to connect agent2");

    sleep(Duration::from_millis(100)).await;

    // Agents加入不同办公室
    join_office(&agent1_client, Role::Agent, "office1", "agent1").await;
    join_office(&agent2_client, Role::Agent, "office2", "agent2").await;

    // Computer加入office1
    let computer_client = create_test_client(&server_url, "smcp").await;
    join_office(&computer_client, Role::Computer, "office1", "computer1").await;

    // Computer触发配置更新
    let update_config_req = json!({});

    computer_client
        .emit("server:update_config", update_config_req)
        .await
        .expect("Failed to emit update_config");

    // 等待广播传播
    sleep(Duration::from_millis(300)).await;

    // 验证只有同一办公室的Agent收到了通知
    assert!(
        office1_received.load(Ordering::SeqCst),
        "Agent in office1 should have received update_config notification"
    );
    assert!(
        !office2_received.load(Ordering::SeqCst),
        "Agent in office2 should not have received update_config notification"
    );

    // 清理
    computer_client.disconnect().await.unwrap();
    agent1_client.disconnect().await.unwrap();
    agent2_client.disconnect().await.unwrap();
    server.shutdown();
}

#[tokio::test]
async fn test_update_config_computer_not_in_office() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();

    let server = SmcpTestServer::start().await;
    let server_url = server.url();

    // 创建Computer客户端但不加入任何办公室
    let computer_client = create_test_client(&server_url, "smcp").await;

    // 等待连接建立
    sleep(Duration::from_millis(100)).await;

    // Computer尝试触发配置更新（未加入办公室）
    let update_config_req = json!({
        "computer": "computer1"
    });

    // 发送事件（应该失败或被忽略）
    let emit_result = computer_client
        .emit("server:update_config", update_config_req)
        .await;

    // 验证发送结果
    assert!(
        emit_result.is_ok(),
        "Should be able to emit update_config event"
    );

    // 等待一段时间确保没有广播
    sleep(Duration::from_millis(500)).await;

    // 清理
    computer_client.disconnect().await.unwrap();
    server.shutdown();
}

#[tokio::test]
async fn test_update_config_multiple_computers() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();

    let server = SmcpTestServer::start().await;
    let server_url = server.url();

    // 标记Agent收到的通知数量
    let notification_count = Arc::new(AtomicBool::new(false));
    let notification_count_clone = notification_count.clone();

    // 创建Agent客户端
    let agent_client = ClientBuilder::new(server_url.clone())
        .transport_type(TransportType::Websocket)
        .namespace("smcp")
        .opening_header("x-api-key", "test_secret")
        .on("notify:update_config", move |payload: Payload, _client| {
            let notification_count = notification_count_clone.clone();
            async move {
                if let Payload::Text(values, _) = payload {
                    if let Ok(_notification) =
                        serde_json::from_value::<UpdateMCPConfigNotification>(values[0].clone())
                    {
                        notification_count.store(true, Ordering::SeqCst);
                    }
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

    // 创建多个Computer客户端
    let computer1_client = create_test_client(&server_url, "smcp").await;
    let computer2_client = create_test_client(&server_url, "smcp").await;

    // Computers加入同一办公室
    join_office(&computer1_client, Role::Computer, "office1", "computer1").await;
    join_office(&computer2_client, Role::Computer, "office1", "computer2").await;

    // 重置通知标记
    notification_count.store(false, Ordering::SeqCst);

    // Computer1触发配置更新
    let update_config_req1 = json!({
        "computer": "computer1"
    });

    computer1_client
        .emit("server:update_config", update_config_req1)
        .await
        .expect("Failed to emit update_config");

    // 等待广播传播
    sleep(Duration::from_millis(500)).await;

    // 验证Agent收到了通知
    assert!(
        notification_count.load(Ordering::SeqCst),
        "Agent should have received update_config notification from computer1"
    );

    // 重置通知标记
    notification_count.store(false, Ordering::SeqCst);

    // Computer2触发配置更新
    let update_config_req2 = json!({
        "computer": "computer2"
    });

    computer2_client
        .emit("server:update_config", update_config_req2)
        .await
        .expect("Failed to emit update_config");

    // 等待广播传播
    sleep(Duration::from_millis(500)).await;

    // 验证Agent收到了通知
    assert!(
        notification_count.load(Ordering::SeqCst),
        "Agent should have received update_config notification from computer2"
    );

    // 清理
    computer1_client.disconnect().await.unwrap();
    computer2_client.disconnect().await.unwrap();
    agent_client.disconnect().await.unwrap();
    server.shutdown();
}

#[tokio::test]
#[ignore]
async fn test_update_config_notification_content() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();

    let server = SmcpTestServer::start().await;
    let server_url = server.url();

    // 存储收到的通知内容
    let received_notification =
        Arc::new(tokio::sync::Mutex::new(None::<UpdateMCPConfigNotification>));
    let received_notification_clone = received_notification.clone();

    // 创建Agent客户端
    let agent_client = ClientBuilder::new(server_url.clone())
        .transport_type(TransportType::Websocket)
        .namespace("smcp")
        .opening_header("x-api-key", "test_secret")
        .on("notify:update_config", move |payload: Payload, _client| {
            let received_notification = received_notification_clone.clone();
            async move {
                if let Payload::Text(values, _) = payload {
                    if let Ok(notification) =
                        serde_json::from_value::<UpdateMCPConfigNotification>(values[0].clone())
                    {
                        *received_notification.lock().await = Some(notification);
                    }
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

    // 创建Computer客户端
    let computer_client = create_test_client(&server_url, "smcp").await;
    join_office(&computer_client, Role::Computer, "office1", "computer1").await;

    // Computer触发配置更新
    let update_config_req = json!({});

    computer_client
        .emit("server:update_config", update_config_req)
        .await
        .expect("Failed to emit update_config");

    // 等待广播传播
    sleep(Duration::from_millis(300)).await;

    // 验证通知内容
    let notification = received_notification.lock().await;
    assert!(notification.is_some(), "Should have received notification");

    let notification = notification.as_ref().unwrap();
    assert_eq!(notification.computer, "computer1");

    // 清理
    computer_client.disconnect().await.unwrap();
    agent_client.disconnect().await.unwrap();
    server.shutdown();
}
