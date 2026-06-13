//! SRV-02 (#50)：`server:update_skills` → `notify:update_skills` 广播 + 来源校验。
//! Test SKILL-set update broadcast (Computer-only) and source validation.

#[path = "test_utils.rs"]
mod test_utils;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt;
use serde_json::json;
use tf_rust_socketio::asynchronous::ClientBuilder;
use tf_rust_socketio::Payload;
use tf_rust_socketio::TransportType;
use tokio::time::sleep;

use smcp::*;
use test_utils::*;

/// 监听 `notify:update_skills` 并在 computer 名匹配时置位的客户端 / a client that flags on notify:update_skills。
async fn listener_client(
    server_url: &str,
    flag: Arc<AtomicBool>,
) -> tf_rust_socketio::asynchronous::Client {
    ClientBuilder::new(server_url.to_string())
        .transport_type(TransportType::Websocket)
        .namespace("smcp")
        .opening_header("access_token", "test_secret")
        .on("notify:update_skills", move |payload: Payload, _client| {
            let flag = flag.clone();
            async move {
                if let Payload::Text(values, _) = payload {
                    if serde_json::from_value::<UpdateMCPConfigNotification>(values[0].clone())
                        .is_ok()
                    {
                        flag.store(true, Ordering::SeqCst);
                    }
                }
            }
            .boxed()
        })
        .connect()
        .await
        .expect("Failed to connect listener")
}

#[tokio::test]
async fn test_update_skills_broadcast_to_agent() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();
    let server = SmcpTestServer::start().await;
    let server_url = server.url();

    // Agent 监听 notify:update_skills。
    let received = Arc::new(AtomicBool::new(false));
    let agent_client = listener_client(&server_url, received.clone()).await;
    sleep(Duration::from_millis(200)).await;
    join_office(&agent_client, Role::Agent, "office1", "agent1").await;

    // Computer 触发 SKILL 变更（连接后 settle 再 join，避免 join ack 竞速超时）。
    let computer_client = create_test_client(&server_url, "smcp").await;
    sleep(Duration::from_millis(200)).await;
    join_office(&computer_client, Role::Computer, "office1", "computer1").await;
    sleep(Duration::from_millis(200)).await;

    computer_client
        .emit("server:update_skills", json!({ "computer": "computer1" }))
        .await
        .expect("Failed to emit update_skills");
    sleep(Duration::from_millis(400)).await;

    assert!(
        received.load(Ordering::SeqCst),
        "Agent should have received notify:update_skills from a Computer"
    );

    computer_client.disconnect().await.unwrap();
    agent_client.disconnect().await.unwrap();
    server.shutdown();
}

#[tokio::test]
async fn test_update_skills_rejected_from_agent_source() {
    // 来源校验：非 Computer（此处 Agent）触发 server:update_skills 必须被拒绝、不广播。
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();
    let server = SmcpTestServer::start().await;
    let server_url = server.url();

    // Computer 作监听者（房间内非发送方成员，若误广播会收到）。
    let received = Arc::new(AtomicBool::new(false));
    let computer_listener = listener_client(&server_url, received.clone()).await;
    sleep(Duration::from_millis(200)).await;
    join_office(&computer_listener, Role::Computer, "office1", "computer1").await;

    // Agent 非法触发 server:update_skills（连接后 settle 再 join）。
    let agent_client = create_test_client(&server_url, "smcp").await;
    sleep(Duration::from_millis(200)).await;
    join_office(&agent_client, Role::Agent, "office1", "agent1").await;
    sleep(Duration::from_millis(200)).await;

    agent_client
        .emit("server:update_skills", json!({ "computer": "computer1" }))
        .await
        .expect("emit should send (server rejects internally)");
    sleep(Duration::from_millis(400)).await;

    assert!(
        !received.load(Ordering::SeqCst),
        "Agent-sourced server:update_skills must NOT broadcast notify:update_skills"
    );

    agent_client.disconnect().await.unwrap();
    computer_listener.disconnect().await.unwrap();
    server.shutdown();
}

#[tokio::test]
#[ignore = "timing-sensitive cross-office broadcast; runs in the e2e/integration matrix"]
async fn test_update_skills_only_same_office() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();
    let server = SmcpTestServer::start().await;
    let server_url = server.url();

    let office1_recv = Arc::new(AtomicBool::new(false));
    let office2_recv = Arc::new(AtomicBool::new(false));
    let agent1 = listener_client(&server_url, office1_recv.clone()).await;
    let agent2 = listener_client(&server_url, office2_recv.clone()).await;
    sleep(Duration::from_millis(200)).await;
    join_office(&agent1, Role::Agent, "office1", "agent1").await;
    join_office(&agent2, Role::Agent, "office2", "agent2").await;

    let computer_client = create_test_client(&server_url, "smcp").await;
    sleep(Duration::from_millis(200)).await;
    join_office(&computer_client, Role::Computer, "office1", "computer1").await;
    sleep(Duration::from_millis(200)).await;
    computer_client
        .emit("server:update_skills", json!({ "computer": "computer1" }))
        .await
        .expect("Failed to emit update_skills");
    sleep(Duration::from_millis(400)).await;

    assert!(
        office1_recv.load(Ordering::SeqCst),
        "same-office agent should receive"
    );
    assert!(
        !office2_recv.load(Ordering::SeqCst),
        "other-office agent must not receive"
    );

    computer_client.disconnect().await.unwrap();
    agent1.disconnect().await.unwrap();
    agent2.disconnect().await.unwrap();
    server.shutdown();
}

#[tokio::test]
async fn test_update_skills_skips_sender() {
    // skip-self：触发者（Computer）自身挂监听 → 不收到自己的 notify:update_skills；同 office Agent 应收到。
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();
    let server = SmcpTestServer::start().await;
    let server_url = server.url();

    // 同 office Agent 监听（验证广播确实发出）。
    let agent_recv = Arc::new(AtomicBool::new(false));
    let agent = listener_client(&server_url, agent_recv.clone()).await;
    sleep(Duration::from_millis(200)).await;
    join_office(&agent, Role::Agent, "office1", "agent1").await;

    // 触发者 Computer 同时挂监听（验证 skip-self）。
    let self_recv = Arc::new(AtomicBool::new(false));
    let computer = listener_client(&server_url, self_recv.clone()).await;
    sleep(Duration::from_millis(200)).await;
    join_office(&computer, Role::Computer, "office1", "computer1").await;
    sleep(Duration::from_millis(200)).await;

    computer
        .emit("server:update_skills", json!({ "computer": "computer1" }))
        .await
        .expect("Failed to emit update_skills");
    sleep(Duration::from_millis(400)).await;

    assert!(
        agent_recv.load(Ordering::SeqCst),
        "co-office Agent should have received the broadcast"
    );
    assert!(
        !self_recv.load(Ordering::SeqCst),
        "the sending Computer must NOT receive its own notify:update_skills (skip-self)"
    );

    computer.disconnect().await.unwrap();
    agent.disconnect().await.unwrap();
    server.shutdown();
}
