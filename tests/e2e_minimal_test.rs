// Minimal E2E test for SMCP protocol

mod e2e;

use e2e::*;
use smcp_agent::{AsyncSmcpAgent, DefaultAuthProvider, SmcpAgentConfig};
use smcp_computer::computer::{Computer, SilentSession};
use std::time::Duration;
use tracing::info;

/// Test basic server startup
#[tokio::test]
#[cfg(all(feature = "server"))]
async fn test_minimal_server_startup() {
    tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(tracing::Level::WARN)
        .try_init()
        .ok();

    let server = TestServer::start().await.expect("Failed to start server");

    info!("Server started on {}", server.url());
    assert!(server.addr.port() > 0);
}

/// Test computer connection
#[tokio::test]
#[cfg(all(feature = "computer", feature = "server"))]
async fn test_minimal_computer_connection() {
    tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(tracing::Level::WARN)
        .try_init()
        .ok();

    let server = TestServer::start().await.expect("Failed to start server");

    let computer_name = "test_computer";
    let office_id = "test_office";

    let session = SilentSession::new("test_session");
    let computer = Computer::new(computer_name, session, None, None, true, true);

    computer.boot_up().await.expect("Failed to boot");

    let auth_secret = Some("test_secret".to_string());
    let headers = None;
    computer
        .connect_socketio(server.url(), "/smcp", &auth_secret, &headers)
        .await
        .expect("Failed to connect");

    computer
        .join_office(office_id, computer_name)
        .await
        .expect("Failed to join office");

    info!("Computer connected successfully");

    computer.shutdown().await.expect("Failed to shutdown");
}

/// Test agent connection
#[tokio::test]
#[cfg(all(feature = "agent", feature = "server"))]
async fn test_minimal_agent_connection() {
    tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(tracing::Level::WARN)
        .try_init()
        .ok();

    let server = TestServer::start().await.expect("Failed to start server");

    let agent_name = "test_agent".to_string();
    let office_id = "test_office".to_string();

    // 设置 API key 以通过服务器认证
    // Set API key to pass server authentication
    let auth = DefaultAuthProvider::new(agent_name.clone(), office_id)
        .with_api_key("test_secret".to_string());
    let config = SmcpAgentConfig::default();
    let mut agent = AsyncSmcpAgent::new(auth, config);

    agent
        .connect(server.url())
        .await
        .expect("Failed to connect agent");

    agent
        .join_office(&agent_name)
        .await
        .expect("Failed to join office");

    info!("Agent connected successfully");

    let _ = agent.leave_office().await;
}
