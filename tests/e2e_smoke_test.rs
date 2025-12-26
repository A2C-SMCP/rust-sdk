// Smoke test for SMCP E2E - basic functionality verification

mod e2e;

use e2e::*;

/// Test: Server can start and bind to a port
#[tokio::test]
#[cfg(feature = "server")]
async fn smoke_test_server_starts() {
    let server = TestServer::start().await.unwrap();
    assert!(server.addr.port() > 0);
    println!("✓ Server started on {}", server.url());
}

/// Test: Computer can boot up
#[tokio::test]
#[cfg(feature = "computer")]
async fn smoke_test_computer_boots() {
    use smcp_computer::computer::{Computer, SilentSession};

    let session = SilentSession::new("test");
    let computer = Computer::new("test", session, None, None, true, true);
    computer.boot_up().await.unwrap();
    computer.shutdown().await.unwrap();
    println!("✓ Computer booted and shutdown successfully");
}

/// Test: Agent can be created
#[tokio::test]
#[cfg(feature = "agent")]
async fn smoke_test_agent_creation() {
    use smcp_agent::{AsyncSmcpAgent, DefaultAuthProvider, SmcpAgentConfig};

    let auth = DefaultAuthProvider::new("test".to_string(), "office".to_string())
        .with_api_key("test_secret".to_string());
    let config = SmcpAgentConfig::default();
    let _agent = AsyncSmcpAgent::new(auth, config);
    println!("✓ Agent created successfully");
}

/// Test: Helper functions work
#[tokio::test]
async fn smoke_test_helpers() {
    let office = generate_office_id();
    let agent = generate_agent_name();
    let computer = generate_computer_name();

    assert!(office.starts_with("test_office_"));
    assert!(agent.starts_with("test_agent_"));
    assert!(computer.starts_with("test_computer_"));
    println!("✓ Helper functions work");
}
