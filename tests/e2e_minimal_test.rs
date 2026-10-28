// Minimal E2E test for SMCP protocol

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
#[cfg(feature = "server")]
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
    let computer = Computer::new(computer_name, session, None, None, true, true).with_config_env(
        isolated_config_env(std::env::temp_dir().join(format!("smcp-test-{}", std::process::id()))),
    );

    computer.boot_up().await.expect("Failed to boot");

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
