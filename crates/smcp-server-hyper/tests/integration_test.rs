//! SMCP Server 集成测试
//! 
//! 测试真实的 HTTP/WebSocket 连接和 SMCP 协议交互

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use http::HeaderMap;
use rust_socketio::{asynchronous::{Client, ClientBuilder}, Payload};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::time::sleep;

use smcp_server_core::{auth::{AuthError, AuthenticationProvider}, SmcpServerBuilder};
use smcp_server_hyper::HyperServerBuilder;

/// No-op authentication provider for tests
#[derive(Debug)]
struct NoAuthProvider;

#[async_trait::async_trait]
impl AuthenticationProvider for NoAuthProvider {
    async fn authenticate(
        &self,
        _headers: &HeaderMap,
        _auth: Option<&serde_json::Value>,
    ) -> Result<(), AuthError> {
        Ok(())
    }
}

/// 测试服务器配置
struct TestServer {
    addr: SocketAddr,
    _handle: tokio::task::JoinHandle<()>,
}

impl TestServer {
    async fn new() -> Self {
        // Use fixed port 3000 for Node.js test
        let addr = "127.0.0.1:3000".parse().unwrap();

        // 构建服务器层（使用无认证的提供者）
        let layer = SmcpServerBuilder::new()
            .with_auth_provider(Arc::new(NoAuthProvider))
            .build_layer()
            .expect("Failed to build SMCP layer");

        // 创建服务器
        let server = HyperServerBuilder::new()
            .with_layer(layer)
            .with_addr(addr)
            .build();

        // 在后台运行服务器
        let handle = tokio::spawn(async move {
            if let Err(e) = server.run(addr).await {
                eprintln!("Server error: {}", e);
            }
        });

        // 等待服务器启动
        sleep(Duration::from_millis(100)).await;

        Self { addr, _handle: handle }
    }
}

/// 创建 Socket.IO 客户端连接
async fn create_client(addr: SocketAddr, namespace: &str) -> Client {
    let url = format!("http://localhost:{}", addr.port());
    
    ClientBuilder::new(url)
        .namespace(namespace)
        .connect()
        .await
        .expect("Failed to connect client")
}

/// Helper function to emit and wait for ack (but ignore response)
async fn emit_event(
    client: &Client,
    event: &str,
    data: Value,
) -> Result<(), Box<dyn std::error::Error>> {
    let callback = move |_payload: Payload, _client: Client| {
        async move {}.boxed()
    };
    
    client
        .emit_with_ack(event, Payload::Text(vec![data]), Duration::from_secs(5), callback)
        .await?;
    Ok(())
}

/// Create a client with event handlers
async fn create_client_with_handlers<F>(
    addr: SocketAddr,
    namespace: &str,
    event: &str,
    handler: F,
) -> Client
where
    F: FnMut(Payload, Client) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + 'static + Send + Sync,
{
    let url = format!("http://localhost:{}", addr.port());
    
    ClientBuilder::new(url)
        .namespace(namespace)
        .on(event, handler)
        .connect()
        .await
        .expect("Failed to connect client")
}

#[tokio::test]
async fn test_server_basic_http_endpoints() {
    let server = TestServer::new().await;
    
    // 测试根路径
    let client = reqwest::Client::new();
    let response = client
        .get(format!("http://localhost:{}/", server.addr.port()))
        .send()
        .await
        .expect("Failed to send request");
    
    assert_eq!(response.status(), 200);
    let text = response.text().await.expect("Failed to read response");
    assert_eq!(text, "SMCP Server is running");
    
    // 测试健康检查端点
    let response = client
        .get(format!("http://localhost:{}/health", server.addr.port()))
        .send()
        .await
        .expect("Failed to send request");
    
    assert_eq!(response.status(), 200);
    let json: Value = response.json().await.expect("Failed to parse JSON");
    assert_eq!(json["status"], "ok");
}

#[tokio::test]
async fn test_socketio_connection() {
    let server = TestServer::new().await;
    
    // 创建客户端连接到 /smcp 命名空间
    let _client = create_client(server.addr, "/smcp").await;
    
    // 如果没有 panic，说明连接成功
    // RawClient 不提供 is_connected 方法，但连接失败会抛出异常
}

#[tokio::test]
async fn test_agent_computer_join_office() {
    // Initialize tracing for this test
    tracing_subscriber::fmt::init();
    
    let server = TestServer::new().await;
    
    // 共享数据用于验证
    let events = Arc::new(Mutex::new(Vec::<String>::new()));
    let events_clone = events.clone();
    
    // 创建 Agent 客户端（带事件处理器）
    let events_clone = events.clone();
    let agent_client = create_client_with_handlers(
        server.addr, 
        "/smcp",
        "notify_enter_office",  // Try underscore instead of colon
        move |payload, _client| {
            println!("!!! Agent received notify_enter_office event!");
            let events = events_clone.clone();
            Box::pin(async move {
                if let Payload::Text(data) = payload {
                    if let Some(first) = data.first() {
                        events.lock().await.push(first.to_string());
                    }
                }
            })
        }
    ).await;
    
    // Wait for connection to be fully established
    sleep(Duration::from_millis(500)).await;
    
    // Agent 加入办公室
    let join_data = json!({
        "role": "agent",
        "name": "test-agent",
        "office_id": "test-office-1"
    });
    
    emit_event(&agent_client, "server:join_office", join_data).await.unwrap();
    
    // 等待 agent 完全加入房间
    sleep(Duration::from_millis(300)).await;
    
    // Computer 加入同一办公室
    let computer_client = create_client(server.addr, "/smcp").await;
    let computer_join_data = json!({
        "role": "computer",
        "name": "test-computer",
        "office_id": "test-office-1"
    });
    
    emit_event(&computer_client, "server:join_office", computer_join_data).await.unwrap();
    
    // 等待通知传播
    sleep(Duration::from_millis(100)).await;
    
    // 验证 Agent 收到了 Computer 进入的通知
    let events = events.lock().await;
    assert!(!events.is_empty());
    
    // 解析通知内容
    let notification: Value = serde_json::from_str(&events[0]).expect("Failed to parse notification");
    assert_eq!(notification["office_id"], "test-office-1");
    assert_eq!(notification["computer"], "test-computer");
}

#[tokio::test]
async fn test_list_room_sessions() {
    let server = TestServer::new().await;
    
    // 创建多个客户端
    let agent1 = create_client(server.addr, "/smcp").await;
    let computer1 = create_client(server.addr, "/smcp").await;
    let computer2 = create_client(server.addr, "/smcp").await;
    
    // Agent 加入办公室
    let join_data = json!({
        "role": "agent",
        "name": "agent-1",
        "office_id": "office-list-test"
    });
    emit_event(&agent1, "server:join_office", join_data).await.unwrap();
    
    // Computers 加入办公室
    let comp1_data = json!({
        "role": "computer",
        "name": "computer-1",
        "office_id": "office-list-test"
    });
    emit_event(&computer1, "server:join_office", comp1_data).await.unwrap();
    
    let comp2_data = json!({
        "role": "computer",
        "name": "computer-2",
        "office_id": "office-list-test"
    });
    emit_event(&computer2, "server:join_office", comp2_data).await.unwrap();
    
    // 等待所有客户端加入完成
    sleep(Duration::from_millis(100)).await;
    
    // 列出房间会话
    let list_data = json!({
        "office_id": "office-list-test",
        "req_id": "test-req-1"
    });
    
    emit_event(&agent1, "server:list_room", list_data).await.unwrap();
    
    // 简化测试：只验证事件发送成功，不验证返回数据
    // TODO: 添加 ack 响应验证后可以检查会话列表
}

#[tokio::test]
async fn test_computer_name_conflict() {
    let server = TestServer::new().await;
    
    // 第一个 Computer 加入
    let computer1 = create_client(server.addr, "/smcp").await;
    let join_data1 = json!({
        "role": "computer",
        "name": "same-name",
        "office_id": "office-conflict-test"
    });
    
    emit_event(&computer1, "server:join_office", join_data1).await.unwrap();
    // TODO: 添加 ack 验证后检查返回值
    
    // 第二个 Computer 尝试使用相同名称加入
    let computer2 = create_client(server.addr, "/smcp").await;
    let join_data2 = json!({
        "role": "computer",
        "name": "same-name",
        "office_id": "office-conflict-test"
    });
    
    emit_event(&computer2, "server:join_office", join_data2).await.unwrap();
    // TODO: 添加 ack 验证后检查返回值是否为 false
    
    // 不同名称应该可以加入
    let computer3 = create_client(server.addr, "/smcp").await;
    let join_data3 = json!({
        "role": "computer",
        "name": "different-name",
        "office_id": "office-conflict-test"
    });
    
    emit_event(&computer3, "server:join_office", join_data3).await.unwrap();
    // TODO: 添加 ack 验证后检查返回值是否为 true
}

#[tokio::test]
async fn test_computer_leave_office_notification() {
    let server = TestServer::new().await;
    
    // 共享事件记录
    let events = Arc::new(Mutex::new(Vec::<Value>::new()));
    let events_clone = events.clone();
    
    // 创建 Agent 监听离场通知
    let events_clone = events.clone();
    let agent = create_client_with_handlers(
        server.addr,
        "/smcp",
        "notify:leave_office",
        move |payload, _client| {
            let events = events_clone.clone();
            Box::pin(async move {
                if let Payload::Text(data) = payload {
                    if let Some(first) = data.first() {
                        if let Ok(json) = serde_json::from_str::<Value>(&first.to_string()) {
                            events.lock().await.push(json);
                        }
                    }
                }
            })
        }
    ).await;
    
    // Agent 加入办公室
    let agent_join = json!({
        "role": "agent",
        "name": "agent-leave",
        "office_id": "office-leave-test"
    });
    emit_event(&agent, "server:join_office", agent_join).await.unwrap();
    
    // Computer 加入办公室
    let computer = create_client(server.addr, "/smcp").await;
    let comp_join = json!({
        "role": "computer",
        "name": "computer-leave",
        "office_id": "office-leave-test"
    });
    emit_event(&computer, "server:join_office", comp_join).await.unwrap();
    
    // 等待加入完成
    sleep(Duration::from_millis(50)).await;
    
    // Computer 离开办公室
    let leave_data = json!({
        "office_id": "office-leave-test"
    });
    emit_event(&computer, "server:leave_office", leave_data).await.unwrap();
    
    // 等待通知传播
    sleep(Duration::from_millis(100)).await;
    
    // 验证 Agent 收到了离场通知
    let events = events.lock().await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["office_id"], "office-leave-test");
    assert_eq!(events[0]["computer"], "computer-leave");
}
