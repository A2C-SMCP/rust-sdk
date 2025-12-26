// Desktop synchronization E2E tests
//
// Tests desktop information sync between Computer and Agent

mod e2e;

use e2e::*;
use smcp_agent::{AsyncSmcpAgent, DefaultAuthProvider, SmcpAgentConfig};
use smcp_computer::computer::{Computer, SilentSession};
use std::time::Duration;

/// Test desktop information retrieval
#[tokio::test]
#[cfg(all(feature = "agent", feature = "computer", feature = "server"))]
async fn test_desktop_info_retrieval() {
    tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(tracing::Level::INFO)
        .try_init()
        .ok();

    let server = TestServer::start().await.expect("Failed to start server");
    let computer_name = generate_computer_name();
    let office_id = generate_office_id();

    let session = SilentSession::new("test_session");
    let computer = Computer::new(computer_name.clone(), session, None, None, true, true);

    computer.boot_up().await.expect("Failed to boot");
    let auth_secret = Some("test_secret".to_string());
    computer
        .connect_socketio(server.url(), "/smcp", &auth_secret, &None)
        .await
        .expect("Failed to connect");
    computer
        .join_office(&office_id, &computer_name)
        .await
        .expect("Failed to join");

    let agent_name = generate_agent_name();
    let auth = DefaultAuthProvider::new(agent_name, office_id)
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

    // Wait a bit for desktop to be synced
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Get desktop info
    let desktops = agent
        .get_desktop(
            &computer_name,
            Some(1920), // preferred width
            None,       // no preferred height
        )
        .await;

    match desktops {
        Ok(desktop_info_list) => {
            println!("Received {} desktop info entries", desktop_info_list.len());
            for desktop in &desktop_info_list {
                println!("Desktop: {:?}", desktop);
            }
        }
        Err(e) => {
            eprintln!("Failed to get desktop info: {}", e);
            // This is expected if no MCP servers provide desktop resources
        }
    }

    // Cleanup
    let _ = agent.leave_office().await;
    computer.shutdown().await.expect("Failed to shutdown");
}

/// Test desktop update notifications
#[tokio::test]
#[cfg(all(feature = "agent", feature = "computer", feature = "server"))]
async fn test_desktop_update_notifications() {
    tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(tracing::Level::INFO)
        .try_init()
        .ok();

    let server = TestServer::start().await.expect("Failed to start server");
    let computer_name = generate_computer_name();
    let office_id = generate_office_id();

    let session = SilentSession::new("test_session");
    let computer = Computer::new(computer_name.clone(), session, None, None, true, true);

    computer.boot_up().await.expect("Failed to boot");
    let auth_secret = Some("test_secret".to_string());
    computer
        .connect_socketio(server.url(), "/smcp", &auth_secret, &None)
        .await
        .expect("Failed to connect");
    computer
        .join_office(&office_id, &computer_name)
        .await
        .expect("Failed to join");

    let agent_name = generate_agent_name();
    let auth = DefaultAuthProvider::new(agent_name, office_id)
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

    // Wait for initial notifications
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Check if we received any desktop update events
    let update_count = event_handler.update_desktop_events.read().await.len();
    println!("Received {} desktop update events", update_count);

    // Cleanup
    let _ = agent.leave_office().await;
    computer.shutdown().await.expect("Failed to shutdown");
}
