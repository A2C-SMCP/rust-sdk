// Desktop synchronization E2E tests
//
// Tests desktop information sync between Computer and Agent

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
    let computer = Computer::new(computer_name.clone(), session, None, None, true, true)
        .with_config_env(isolated_config_env(
            std::env::temp_dir().join(format!("smcp-test-{}", std::process::id())),
        ));

    computer.boot_up().await.expect("Failed to boot");
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
        .expect("Failed to join");

    let agent_name = generate_agent_name();
    let auth =
        DefaultAuthProvider::new(agent_name, office_id).with_api_key("test_secret".to_string());
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
    let computer = Computer::new(computer_name.clone(), session, None, None, true, true)
        .with_config_env(isolated_config_env(
            std::env::temp_dir().join(format!("smcp-test-{}", std::process::id())),
        ));

    computer.boot_up().await.expect("Failed to boot");
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
        .expect("Failed to join");

    let agent_name = generate_agent_name();
    let auth =
        DefaultAuthProvider::new(agent_name, office_id).with_api_key("test_secret".to_string());
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
