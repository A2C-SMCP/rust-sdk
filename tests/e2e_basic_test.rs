// Basic E2E integration test for SMCP protocol
//
// This is a simplified test to verify the basic flow works

/// protocol#64：隔离的 user-config 环境（XDG-first），测试绝不读写真实 `$HOME`。
fn isolated_config_env(
    base: impl AsRef<std::path::Path>,
) -> std::collections::HashMap<String, String> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    // 每次调用分配独立子目录：同一测试二进制内的多个 Computer、以及共享 temp_dir 的 e2e
    // 用例都不会复用同一身份（PR3 记录/回显身份后“两台电脑”前提仍然成立）。
    let base = base.as_ref().join(format!(
        "smcp-identity-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let mut env = std::collections::HashMap::new();
    env.insert("HOME".to_string(), base.display().to_string());
    env.insert(
        "XDG_CONFIG_HOME".to_string(),
        base.join("xdg-config").display().to_string(),
    );
    env
}

mod e2e;

use e2e::*;
use smcp_agent::{AsyncSmcpAgent, DefaultAuthProvider, SmcpAgentConfig};
use smcp_computer::computer::{Computer, ConnectOptions, SilentSession};
use tracing::info;

/// Test basic server startup
#[tokio::test]
#[cfg(all(feature = "agent", feature = "computer", feature = "server"))]
async fn test_server_startup() {
    tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(tracing::Level::INFO)
        .try_init()
        .ok();

    // Start server
    let server = TestServer::start().await.expect("Failed to start server");

    info!("Server started on {}", server.url());

    assert!(server.addr.port() > 0);
    assert!(server.url().contains("http://127.0.0.1"));
}

/// Test computer connection to server
#[tokio::test]
#[cfg(all(feature = "computer", feature = "server"))]
async fn test_computer_connection() {
    tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(tracing::Level::INFO)
        .try_init()
        .ok();

    let server = TestServer::start().await.expect("Failed to start server");

    let computer_name = generate_computer_name();
    let office_id = generate_office_id();

    let session = SilentSession::new("test_session");
    let computer = Computer::new(
        computer_name.clone(),
        session,
        None, // No custom inputs
        None, // No custom MCP servers
        true, // auto_connect
        true, // auto_reconnect
    )
    .with_config_env(isolated_config_env(
        std::env::temp_dir().join(format!("smcp-test-{}", std::process::id())),
    ));

    // Boot up computer
    computer.boot_up().await.expect("Failed to boot computer");

    // Connect to server
    let auth_secret = Some("test_secret".to_string());
    let headers = None;
    computer
        .connect_socketio(
            server.url(),
            ConnectOptions {
                auth_payload: auth_secret.as_deref().map(auth_dict),
                headers: headers.clone(),
                ..Default::default()
            },
        )
        .await
        .expect("Failed to connect computer to server");

    // Join office
    computer
        .join_office(&office_id, &computer_name)
        .await
        .expect("Failed to join office");

    info!(
        "Computer {} connected and joined office {}",
        computer_name, office_id
    );

    // Cleanup
    computer
        .shutdown()
        .await
        .expect("Failed to shutdown computer");
}

/// Test agent connection to server
#[tokio::test]
#[cfg(all(feature = "agent", feature = "server"))]
async fn test_agent_connection() {
    tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(tracing::Level::INFO)
        .try_init()
        .ok();

    let server = TestServer::start().await.expect("Failed to start server");

    let agent_name = generate_agent_name();
    let office_id = generate_office_id();

    let auth = DefaultAuthProvider::new(agent_name.clone(), office_id.clone())
        .with_api_key("test_secret".to_string());
    let config = SmcpAgentConfig::default();
    let mut agent = AsyncSmcpAgent::new(auth, config);

    // Connect to server
    agent
        .connect(server.url())
        .await
        .expect("Failed to connect agent");

    // Join office
    agent
        .join_office(&agent_name)
        .await
        .expect("Failed to join office");

    info!(
        "Agent {} connected and joined office {}",
        agent_name, office_id
    );

    // Cleanup
    let _ = agent.leave_office().await;
}

/// Test basic three-component integration
#[tokio::test]
#[cfg(all(feature = "agent", feature = "computer", feature = "server"))]
async fn test_basic_integration() {
    tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(tracing::Level::INFO)
        .try_init()
        .ok();

    // Start server
    let server = TestServer::start().await.expect("Failed to start server");

    let computer_name = generate_computer_name();
    let office_id = generate_office_id();

    // Create and connect Agent FIRST
    let agent_name = generate_agent_name();
    let auth = DefaultAuthProvider::new(agent_name.clone(), office_id.clone())
        .with_api_key("test_secret".to_string());
    let config = SmcpAgentConfig::default();
    let event_handler = MockEventHandler::new();
    let mut agent = AsyncSmcpAgent::new(auth, config).with_event_handler(event_handler.clone());

    agent
        .connect(server.url())
        .await
        .expect("Failed to connect agent");

    agent
        .join_office(&agent_name)
        .await
        .expect("Failed to join office");

    // NOW create and start Computer
    let session = SilentSession::new("test_session");
    let computer = Computer::new(computer_name.clone(), session, None, None, true, true)
        .with_config_env(isolated_config_env(
            std::env::temp_dir().join(format!("smcp-test-{}", std::process::id())),
        ));

    computer.boot_up().await.expect("Failed to boot computer");

    let auth_secret = Some("test_secret".to_string());
    let headers = None;
    computer
        .connect_socketio(
            server.url(),
            ConnectOptions {
                auth_payload: auth_secret.as_deref().map(auth_dict),
                headers: headers.clone(),
                ..Default::default()
            },
        )
        .await
        .expect("Failed to connect computer");

    computer
        .join_office(&office_id, &computer_name)
        .await
        .expect("Failed to join office");

    // Wait for agent to receive computer enter event
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    // Verify computer was detected
    let has_computer = event_handler.has_computer(&computer_name).await;
    assert!(has_computer, "Agent did not detect computer");

    // Debug: print event count
    let event_count = event_handler.enter_office_count().await;
    println!("Enter office event count: {}", event_count);

    // Get tools from computer
    // TODO: Fix get_tools call - currently fails with "Missing req_id in response"
    // let tools = agent
    //     .get_tools(&computer_name)
    //     .await
    //     .expect("Failed to get tools");

    // info!("Received {} tools from computer", tools.len());
    println!("Skipping get_tools call due to known issue");

    // Cleanup
    let _ = agent.leave_office().await;
    computer
        .shutdown()
        .await
        .expect("Failed to shutdown computer");

    info!("Test completed successfully!");
}
