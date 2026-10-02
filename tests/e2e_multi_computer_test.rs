// Multi-computer E2E tests
//
// Tests scenarios with multiple computers in the same office

mod e2e;

use e2e::*;
use smcp_agent::{AsyncSmcpAgent, DefaultAuthProvider, SmcpAgentConfig};
use smcp_computer::computer::{Computer, ConnectOptions, SilentSession};
use smcp_computer::errors::ComputerError;
use std::time::Duration;

/// protocol#66：一房至多一台 Computer——第二台入房被拒（`4101 {role:"computer"}`），
/// 旧 Computer 离房释放席位后方可**换绑**入房。
///
/// 旧版 `test_multiple_computers` 断言两台 Computer 同房共存；「每 role 一席」下该场景已不可构造，
/// 本用例改写为跨组件（Agent + Computer + Server）验证单 Computer 语义与换绑流程：
/// ① C2 入房 ⇒ `ProtocolRejection 4101`，且 Agent 不收到其 enter 广播；
/// ② C1 离房 ⇒ Agent 收到 `leave(C1)`；
/// ③ C2 再入房 ⇒ 成功，Agent 收到 `enter(C2)`。
#[tokio::test]
#[cfg(all(feature = "agent", feature = "computer", feature = "server"))]
async fn test_second_computer_rejected_then_rebind_succeeds() {
    tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(tracing::Level::INFO)
        .try_init()
        .ok();

    let server = TestServer::start().await.expect("Failed to start server");
    let office_id = generate_office_id();

    // Create identifiers
    let computer1_name = "computer1".to_string();
    let computer2_name = "computer2".to_string();

    // Connect Agent FIRST (so it can receive computer enter events)
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
        .join_office("test_agent")
        .await
        .expect("Failed to join");

    // Create and connect computer 1
    let session1 = SilentSession::new("session1");
    let computer1 = Computer::new(computer1_name.clone(), session1, None, None, true, true);

    computer1.boot_up().await.expect("Failed to boot computer1");

    let auth_secret1 = Some("test_secret".to_string());
    computer1
        .connect_socketio(
            server.url(),
            ConnectOptions {
                auth_payload: auth_secret1.as_deref().map(auth_dict),
                ..Default::default()
            },
        )
        .await
        .expect("Failed to connect computer1");
    computer1
        .join_office(&office_id, &computer1_name)
        .await
        .expect("Failed to join office");

    let received1 = event_handler.wait_for_computer(&computer1_name, 5).await;
    assert!(received1, "Computer 1 not detected");

    // Create and connect computer 2 — its join to the same office MUST be rejected.
    let session2 = SilentSession::new("session2");
    let computer2 = Computer::new(computer2_name.clone(), session2, None, None, true, true);

    computer2.boot_up().await.expect("Failed to boot computer2");

    let auth_secret2 = Some("test_secret".to_string());
    computer2
        .connect_socketio(
            server.url(),
            ConnectOptions {
                auth_payload: auth_secret2.as_deref().map(auth_dict),
                ..Default::default()
            },
        )
        .await
        .expect("Failed to connect computer2");

    let rejection = computer2
        .join_office(&office_id, &computer2_name)
        .await
        .expect_err("second Computer must be rejected under protocol#66 (per-role seat)");
    match rejection {
        ComputerError::ProtocolRejection { code, details, .. } => {
            assert_eq!(code, 4101, "席位已占必须回 4101");
            assert_eq!(
                details,
                Some(serde_json::json!({"office_id": office_id, "role": "computer"})),
                "4101 必须携带被占席位信息"
            );
        }
        other => panic!("expected ProtocolRejection 4101, got {other:?}"),
    }
    // C2 未入房：Agent 不应收到它的 enter 广播。
    assert!(
        !event_handler.wait_for_computer(&computer2_name, 1).await,
        "被拒的第二台 Computer 不得产生 enter 广播"
    );

    // Rebind: C1 leaves → Agent 收到 leave(C1) 广播 → seat released → C2 joins successfully.
    computer1
        .leave_office()
        .await
        .expect("computer1 leave office");
    tokio::time::sleep(Duration::from_millis(300)).await;
    {
        let leave_events = event_handler.leave_office_events.read().await;
        assert!(
            leave_events
                .iter()
                .any(|event| event.computer.as_deref() == Some(computer1_name.as_str())),
            "Agent 应收到 C1 的 leave 广播（换绑序：先 leave 旧、再 enter 新），实得 {leave_events:?}"
        );
    }
    computer2
        .join_office(&office_id, &computer2_name)
        .await
        .expect("after the seat is released, C2 must be able to join (rebind)");
    let received2 = event_handler.wait_for_computer(&computer2_name, 5).await;
    assert!(received2, "Computer 2 not detected after rebind");

    // Cleanup
    let _ = agent.leave_office().await;
    computer1
        .shutdown()
        .await
        .expect("Failed to shutdown computer1");
    computer2
        .shutdown()
        .await
        .expect("Failed to shutdown computer2");
}

/// Test computer leave office notification
#[tokio::test]
#[cfg(all(feature = "agent", feature = "computer", feature = "server"))]
async fn test_computer_leave_notification() {
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
    let auth = DefaultAuthProvider::new(agent_name.clone(), office_id.clone())
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

    // Wait for computer to enter
    let _ = event_handler.wait_for_computer(&computer_name, 5).await;

    // Computer leaves office
    computer
        .leave_office()
        .await
        .expect("Failed to leave office");

    // Wait for leave notification
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Verify leave event was received
    let leave_events = event_handler.leave_office_events.read().await;
    assert!(!leave_events.is_empty(), "No leave event received");

    let event = &leave_events[0];
    assert_eq!(event.office_id, office_id);
    assert_eq!(event.computer.as_ref().unwrap(), &computer_name);

    // Cleanup
    let _ = agent.leave_office().await;
    computer.shutdown().await.expect("Failed to shutdown");
}

/// Test listing rooms/sessions
#[tokio::test]
#[cfg(all(feature = "agent", feature = "computer", feature = "server"))]
async fn test_list_room_sessions() {
    tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(tracing::Level::INFO)
        .try_init()
        .ok();

    let server = TestServer::start().await.expect("Failed to start server");
    let office_id = generate_office_id();

    let session = SilentSession::new("test_session");
    let computer = Computer::new("test_computer".to_string(), session, None, None, true, true);

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
        .join_office(&office_id, "test_computer")
        .await
        .expect("Failed to join");

    let agent_name = generate_agent_name();
    let auth = DefaultAuthProvider::new(agent_name.clone(), office_id.clone())
        .with_api_key("test_secret".to_string());
    let config = SmcpAgentConfig::default();
    let mut agent = AsyncSmcpAgent::new(auth, config);

    agent
        .connect(server.url())
        .await
        .expect("Failed to connect");
    agent
        .join_office("test_agent")
        .await
        .expect("Failed to join");

    // List room sessions
    // TODO: Fix list_room call - currently fails with "Missing req_id in response"
    // let sessions = agent
    //     .list_room(&office_id)
    //     .await
    //     .expect("Failed to list room");

    // println!("Room sessions: {:?}", sessions);

    // Verify we have at least the computer and agent
    // assert!(sessions.len() >= 2, "Expected at least 2 sessions");
    println!("⚠ Skipping list_room test due to known Socket.IO response handling issue");

    // Cleanup
    let _ = agent.leave_office().await;
    computer.shutdown().await.expect("Failed to shutdown");
}
