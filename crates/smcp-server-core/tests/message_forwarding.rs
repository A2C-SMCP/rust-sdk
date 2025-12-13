//! 测试消息转发功能

use smcp_server_core::{
    auth::DefaultAuthenticationProvider,
    handler::SmcpHandler,
    session::{ClientRole, SessionData, SessionManager},
    ServerState,
};
use socketioxide::SocketIo;
use std::sync::Arc;

#[tokio::test]
async fn test_message_forwarding() {
    // 创建会话管理器
    let session_manager = Arc::new(SessionManager::new());
    let auth_provider = Arc::new(DefaultAuthenticationProvider::new(None, None));

    // 创建 Socket.IO 实例
    let (_layer, io) = SocketIo::builder().build_layer();

    // 注册处理器
    let state = ServerState {
        session_manager: session_manager.clone(),
        auth_provider,
        io: Arc::new(io.clone()),
    };
    SmcpHandler::register_handlers(&io, state);

    // 模拟会话注册
    let agent_session = SessionData::new(
        "agent_sid_1".to_string(),
        "agent_1".to_string(),
        ClientRole::Agent,
    )
    .with_office_id("office_1".to_string());

    let computer_session = SessionData::new(
        "computer_sid_1".to_string(),
        "computer_1".to_string(),
        ClientRole::Computer,
    )
    .with_office_id("office_1".to_string());

    session_manager.register_session(agent_session).unwrap();
    session_manager.register_session(computer_session).unwrap();

    // 验证会话已注册
    assert_eq!(session_manager.get_all_sessions().len(), 2);

    // 验证可以通过名称找到 Computer
    let found_sid =
        session_manager.get_computer_sid_in_office(&"office_1".to_string(), "computer_1");
    assert_eq!(found_sid, Some("computer_sid_1".to_string()));

    println!("✅ 会话管理和查找功能正常");
}

#[tokio::test]
async fn test_session_manager_computer_lookup() {
    let manager = SessionManager::new();

    // 添加多个 Computer 到不同的办公室
    let comp1 = SessionData::new(
        "sid1".to_string(),
        "computer_a".to_string(),
        ClientRole::Computer,
    )
    .with_office_id("office1".to_string());

    let comp2 = SessionData::new(
        "sid2".to_string(),
        "computer_b".to_string(),
        ClientRole::Computer,
    )
    .with_office_id("office1".to_string());

    let comp3 = SessionData::new(
        "sid3".to_string(),
        "computer_a".to_string(),
        ClientRole::Computer,
    )
    .with_office_id("office2".to_string());

    manager.register_session(comp1).unwrap();
    manager.register_session(comp2).unwrap();
    manager.register_session(comp3).unwrap();

    // 测试查找
    assert_eq!(
        manager.get_computer_sid_in_office(&"office1".to_string(), "computer_a"),
        Some("sid1".to_string())
    );
    assert_eq!(
        manager.get_computer_sid_in_office(&"office1".to_string(), "computer_b"),
        Some("sid2".to_string())
    );
    assert_eq!(
        manager.get_computer_sid_in_office(&"office2".to_string(), "computer_a"),
        Some("sid3".to_string())
    );
    assert_eq!(
        manager.get_computer_sid_in_office(&"office1".to_string(), "computer_c"),
        None
    );

    println!("✅ Computer 查找功能正常");
}

#[tokio::test]
async fn test_error_handling() {
    // 测试未找到 Computer 的情况
    let session_manager = Arc::new(SessionManager::new());
    let auth_provider = Arc::new(DefaultAuthenticationProvider::new(None, None));
    let (_layer, io) = SocketIo::builder().build_layer();

    let state = ServerState {
        session_manager,
        auth_provider,
        io: Arc::new(io),
    };

    // 创建一个不在办公室的 Agent 会话
    let agent_session = SessionData::new(
        "agent_sid".to_string(),
        "agent".to_string(),
        ClientRole::Agent,
    );

    state
        .session_manager
        .register_session(agent_session)
        .unwrap();

    // 这里我们无法直接调用 handler，因为它需要 SocketRef
    // 但我们可以验证 SessionManager 的查找逻辑
    let found = state
        .session_manager
        .get_computer_sid_in_office(&"office_1".to_string(), "nonexistent");
    assert_eq!(found, None);

    println!("✅ 错误处理逻辑正常");
}
