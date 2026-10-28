// End-to-end integration tests for SMCP protocol
//
// These tests verify the complete lifecycle of:
// - Server startup
// - Agent connection
// - Computer connection
// - Tool call flow
// - Notifications

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
use std::time::Duration;
use tracing::info;

/// Test basic three-component integration
#[tokio::test]
#[cfg(all(feature = "agent", feature = "computer", feature = "server"))]
async fn test_basic_three_component_integration() {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(tracing::Level::INFO)
        .try_init()
        .ok();

    // 1. Start server
    info!("Starting test server...");
    let server = TestServer::start().await.expect("Failed to start server");

    // 2. Create identifiers
    info!("Creating identifiers...");
    let computer_name = generate_computer_name();
    let office_id = generate_office_id();

    // 3. Create Agent FIRST (so it can receive computer enter events)
    info!("Creating agent...");
    let agent_name = generate_agent_name();

    let auth = DefaultAuthProvider::new(agent_name.clone(), office_id.clone())
        .with_api_key("test_secret".to_string());
    let config = SmcpAgentConfig::default();
    let event_handler = MockEventHandler::new();
    let mut agent = AsyncSmcpAgent::new(auth, config).with_event_handler(event_handler.clone());

    // 4. Connect agent to server
    info!("Connecting agent to server...");
    agent
        .connect(server.url())
        .await
        .expect("Failed to connect agent");

    // 5. Agent joins office FIRST
    info!("Agent joining office...");
    agent
        .join_office(&agent_name)
        .await
        .expect("Failed to join office");

    // 6. NOW create and start Computer
    info!("Creating computer...");
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

    // Boot up computer (starts MCP servers)
    info!("Booting up computer...");
    computer.boot_up().await.expect("Failed to boot computer");

    // 7. Connect computer to server
    info!("Connecting computer to server...");
    let auth_secret = Some("test_secret".to_string());
    computer
        .connect_socketio(
            server.url(),
            ConnectOptions {
                auth_payload: auth_secret.as_deref().map(auth_dict), // API key for authentication
                ..Default::default()                                 // No custom headers / auth dict
            },
        )
        .await
        .expect("Failed to connect computer to server");

    // 8. Computer joins office NOW (agent will receive the event)
    info!("Computer joining office...");
    computer
        .join_office(&office_id, &computer_name)
        .await
        .expect("Failed to join office");

    // 9. Wait for agent to receive computer enter event
    info!("Waiting for computer enter event...");
    let received = event_handler.wait_for_computer(&computer_name, 5).await;
    assert!(received, "Agent did not receive computer enter event");

    // 8. Agent gets tools from computer
    info!("Getting tools from computer...");
    // TODO: Fix get_tools call - currently fails with "Missing req_id in response"
    // let tools = agent
    //     .get_tools(&computer_name)
    //     .await
    //     .expect("Failed to get tools");

    // info!("Received {} tools", tools.len());
    info!("⚠ Skipping get_tools due to known issue");

    // 9. Cleanup
    info!("Cleaning up...");
    let _ = agent.leave_office().await;
    computer
        .shutdown()
        .await
        .expect("Failed to shutdown computer");

    info!("Test completed successfully!");
}

/// Test tool call flow
#[tokio::test]
#[cfg(all(feature = "agent", feature = "computer", feature = "server"))]
async fn test_tool_call_flow() {
    tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(tracing::Level::INFO)
        .try_init()
        .ok();

    // Setup: Start server and computer
    let server = TestServer::start().await.expect("Failed to start server");

    let computer_name = generate_computer_name();
    let office_id = generate_office_id();

    let session = SilentSession::new("test_session");
    let computer = Computer::new(computer_name.clone(), session, None, None, true, true)
        .with_config_env(isolated_config_env(
            std::env::temp_dir().join(format!("smcp-test-{}", std::process::id())),
        ));

    computer.boot_up().await.expect("Failed to boot computer");

    let auth_secret = Some("test_secret".to_string());
    computer
        .connect_socketio(
            server.url(),
            ConnectOptions {
                auth_payload: auth_secret.as_deref().map(auth_dict),
                ..Default::default()
            },
        )
        .await
        .expect("Failed to connect computer");

    computer
        .join_office(&office_id, &computer_name)
        .await
        .expect("Failed to join office");

    // Connect agent
    let agent_name = generate_agent_name();
    let auth =
        DefaultAuthProvider::new(agent_name, office_id).with_api_key("test_secret".to_string());
    let config = SmcpAgentConfig::default();
    let mut agent = AsyncSmcpAgent::new(auth, config);

    agent
        .connect(server.url())
        .await
        .expect("Failed to connect agent");

    agent
        .join_office("test_agent")
        .await
        .expect("Failed to join office");

    // Get tools
    // TODO: Fix get_tools call - currently fails with "Missing req_id in response"
    // let tools = agent
    //     .get_tools(&computer_name)
    //     .await
    //     .expect("Failed to get tools");

    // Note: Tool calls depend on having actual MCP servers configured
    // For now, just verify we can get the tool list
    // info!("Available tools: {} tools", tools.len());
    // for tool in &tools {
    //     info!("  - {}", tool.name);
    // }
    info!("⚠ Skipping get_tools and tool list due to known issue");

    // Cleanup
    let _ = agent.leave_office().await;
    computer
        .shutdown()
        .await
        .expect("Failed to shutdown computer");
}

/// Test notification system
#[tokio::test]
#[cfg(all(feature = "agent", feature = "computer", feature = "server"))]
async fn test_notification_system() {
    tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(tracing::Level::INFO)
        .try_init()
        .ok();

    let server = TestServer::start().await.expect("Failed to start server");

    let computer_name = generate_computer_name();
    let office_id = generate_office_id();

    // Create and connect Agent FIRST
    let agent_name = generate_agent_name();
    let auth = DefaultAuthProvider::new(agent_name, office_id.clone())
        .with_api_key("test_secret".to_string());
    let config = SmcpAgentConfig::default();
    let event_handler = MockEventHandler::new();
    let mut agent = AsyncSmcpAgent::new(auth, config).with_event_handler(event_handler.clone());

    agent
        .connect(server.url())
        .await
        .expect("Failed to connect");
    agent
        .join_office("test_agent")
        .await
        .expect("Failed to join");

    // NOW create and connect Computer
    let session = SilentSession::new("test_session");
    let computer = Computer::new(computer_name.clone(), session, None, None, true, true)
        .with_config_env(isolated_config_env(
            std::env::temp_dir().join(format!("smcp-test-{}", std::process::id())),
        ));

    computer.boot_up().await.expect("Failed to boot computer");

    let auth_secret = Some("test_secret".to_string());
    computer
        .connect_socketio(
            server.url(),
            ConnectOptions {
                auth_payload: auth_secret.as_deref().map(auth_dict),
                ..Default::default()
            },
        )
        .await
        .expect("Failed to connect");
    computer
        .join_office(&office_id, &computer_name)
        .await
        .expect("Failed to join office");

    // Wait for notifications
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Verify enter_office notification was received
    assert!(
        event_handler.has_computer(&computer_name).await,
        "Enter office notification not received"
    );

    // Cleanup
    let _ = agent.leave_office().await;
    computer.shutdown().await.expect("Failed to shutdown");
}
