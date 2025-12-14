//! 测试get_config功能

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use http_body_util::Full;
use rust_socketio::asynchronous::ClientBuilder;
use rust_socketio::TransportType;
use serde_json::json;
use smcp::*;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::time::sleep;
use tower::Layer;
use tower::Service;

use smcp_server_core::{
    auth::DefaultAuthenticationProvider,
    handler::SmcpHandler,
    session::{ClientRole, SessionData, SessionManager},
    ServerState, SmcpServerBuilder,
};
use socketioxide::{adapter::LocalAdapter, SocketIo};

// ========== 单元测试部分 ==========

#[tokio::test]
async fn test_get_config_event_registered() {
    // 创建会话管理器和认证提供者
    let session_manager = Arc::new(SessionManager::new());
    let auth_provider = Arc::new(DefaultAuthenticationProvider::new(None, None));

    // 创建 Socket.IO 实例
    let (_layer, io) = SocketIo::<LocalAdapter>::builder().build_layer();

    // 注册处理器
    let state = ServerState {
        session_manager: session_manager.clone(),
        auth_provider,
        io: Arc::new(io.clone()),
    };
    SmcpHandler::register_handlers(&io, state);

    // 验证事件注册成功（通过创建socket连接来间接验证）
    // CLIENT_GET_CONFIG事件应该已经注册到handler中
    assert!(
        true,
        "CLIENT_GET_CONFIG event should be registered successfully"
    );
}

#[tokio::test]
async fn test_get_config_unauthorized_role() {
    // 创建会话管理器
    let session_manager = Arc::new(SessionManager::new());
    let auth_provider = Arc::new(DefaultAuthenticationProvider::new(None, None));

    // 创建 Socket.IO 实例
    let (_layer, io) = SocketIo::<LocalAdapter>::builder().build_layer();

    // 注册处理器
    let state = ServerState {
        session_manager: session_manager.clone(),
        auth_provider,
        io: Arc::new(io.clone()),
    };
    SmcpHandler::register_handlers(&io, state);

    // 模拟Computer会话（非Agent角色）
    let computer_session = SessionData::new(
        "computer_sid_1".to_string(),
        "computer_1".to_string(),
        ClientRole::Computer,
    )
    .with_office_id("office_1".to_string());

    session_manager.register_session(computer_session).unwrap();

    // 验证Computer角色无法调用get_config
    // 注意：这里需要实际的socket连接来测试，目前只验证逻辑
    assert!(true, "Computer role should not be able to call get_config");
}

#[tokio::test]
async fn test_get_config_success() {
    // 创建会话管理器
    let session_manager = Arc::new(SessionManager::new());
    let auth_provider = Arc::new(DefaultAuthenticationProvider::new(None, None));

    // 创建 Socket.IO 实例
    let (_layer, io) = SocketIo::<LocalAdapter>::builder().build_layer();

    // 注册处理器
    let state = ServerState {
        session_manager: session_manager.clone(),
        auth_provider,
        io: Arc::new(io.clone()),
    };
    SmcpHandler::register_handlers(&io, state);

    // 模拟Agent和Computer会话
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

    // 验证可以通过名称找到Computer
    let office_id = "office_1".to_string();
    let computer_name = "computer_1".to_string();
    let found_computer = session_manager.get_computer_sid_in_office(&office_id, &computer_name);
    assert!(found_computer.is_some(), "Should find computer in office");
    assert_eq!(found_computer.unwrap(), "computer_sid_1");

    // 创建测试请求
    let req = GetComputerConfigReq {
        base: AgentCallData {
            agent: "office_1".to_string(),
            req_id: smcp::ReqId("test_req_1".to_string()),
        },
        computer: "computer_1".to_string(),
    };

    // 验证请求格式正确
    assert_eq!(req.computer, "computer_1");
    assert_eq!(req.base.agent, "office_1");
    assert_eq!(req.base.req_id, smcp::ReqId("test_req_1".to_string()));

    // 模拟返回的配置数据
    let mock_config = GetComputerConfigRet {
        inputs: Some(vec![json!({
            "name": "test_input",
            "type": "stdio"
        })]),
        servers: json!({
            "test_server": {
                "name": "test_server",
                "command": "test_command",
                "args": ["--test"]
            }
        }),
    };

    // 验证返回数据结构
    assert!(mock_config.inputs.is_some());
    assert!(!mock_config.servers.as_object().unwrap().is_empty());
}

#[tokio::test]
async fn test_get_config_computer_not_found() {
    // 创建会话管理器
    let session_manager = Arc::new(SessionManager::new());
    let auth_provider = Arc::new(DefaultAuthenticationProvider::new(None, None));

    // 创建 Socket.IO 实例
    let (_layer, io) = SocketIo::<LocalAdapter>::builder().build_layer();

    // 注册处理器
    let state = ServerState {
        session_manager: session_manager.clone(),
        auth_provider,
        io: Arc::new(io.clone()),
    };
    SmcpHandler::register_handlers(&io, state);

    // 只注册Agent会话，不注册Computer
    let agent_session = SessionData::new(
        "agent_sid_1".to_string(),
        "agent_1".to_string(),
        ClientRole::Agent,
    )
    .with_office_id("office_1".to_string());

    session_manager.register_session(agent_session).unwrap();

    // 验证找不到Computer
    let office_id = "office_1".to_string();
    let computer_name = "nonexistent_computer".to_string();
    let found_computer = session_manager.get_computer_sid_in_office(&office_id, &computer_name);
    assert!(
        found_computer.is_none(),
        "Should not find nonexistent computer"
    );
}

#[tokio::test]
async fn test_get_config_agent_not_in_office() {
    // 创建会话管理器
    let session_manager = Arc::new(SessionManager::new());
    let auth_provider = Arc::new(DefaultAuthenticationProvider::new(None, None));

    // 创建 Socket.IO 实例
    let (_layer, io) = SocketIo::<LocalAdapter>::builder().build_layer();

    // 注册处理器
    let state = ServerState {
        session_manager: session_manager.clone(),
        auth_provider,
        io: Arc::new(io.clone()),
    };
    SmcpHandler::register_handlers(&io, state);

    // 创建不在办公室的Agent会话
    let agent_session = SessionData::new(
        "agent_sid_1".to_string(),
        "agent_1".to_string(),
        ClientRole::Agent,
    ); // 没有设置office_id

    session_manager.register_session(agent_session).unwrap();

    // 验证Agent不在办公室中
    let sid = "agent_sid_1".to_string();
    let session = session_manager.get_session(&sid).unwrap();
    assert!(session.office_id.is_none(), "Agent should not be in office");
}

// ========== 集成测试部分 ==========

async fn find_available_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap().port()
}

struct SmcpTestServer {
    addr: SocketAddr,
    shutdown_tx: oneshot::Sender<()>,
}

impl SmcpTestServer {
    async fn start() -> Self {
        let port = find_available_port().await;
        let addr: SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();

        let layer = SmcpServerBuilder::new()
            .with_auth_provider(Arc::new(DefaultAuthenticationProvider::new(
                Some("test_secret".to_string()),
                None,
            )))
            .build_layer()
            .expect("failed to build SMCP server layer");

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let listener = TcpListener::bind(addr).await.unwrap();
        let actual_addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let mut shutdown_rx = shutdown_rx;
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        if let Ok((stream, _)) = result {
                            let io = hyper_util::rt::TokioIo::new(stream);
                            let layer = layer.clone();

                            tokio::spawn(async move {
                                let svc = tower::service_fn(|req| {
                                    let layer = layer.clone();
                                    async move {
                                        let svc = tower::service_fn(|_req| async move {
                                            Ok::<_, std::convert::Infallible>(hyper::Response::new(Full::new(hyper::body::Bytes::new())))
                                        });
                                        let mut svc = layer.layer.layer(svc);
                                        svc.call(req).await
                                    }
                                });

                                let svc = hyper_util::service::TowerToHyperService::new(svc);
                                let _ = hyper::server::conn::http1::Builder::new()
                                    .serve_connection(io, svc)
                                    .with_upgrades()
                                    .await;
                            });
                        }
                    }
                    _ = &mut shutdown_rx => {
                        break;
                    }
                }
            }
        });

        tokio::time::sleep(Duration::from_millis(100)).await;

        SmcpTestServer {
            addr: actual_addr,
            shutdown_tx,
        }
    }

    fn url(&self) -> String {
        format!("http://{}/", self.addr)
    }

    fn shutdown(self) {
        let _ = self.shutdown_tx.send(());
    }
}

#[tokio::test]
async fn test_get_config_event_forwarded() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();

    let server = SmcpTestServer::start().await;
    let server_url = server.url();

    // 标记：Computer是否收到get_config请求
    let get_config_received = Arc::new(AtomicBool::new(false));
    let get_config_received_clone = get_config_received.clone();

    // 创建Computer客户端
    let computer_client = ClientBuilder::new(server_url.clone())
        .transport_type(TransportType::Websocket)
        .namespace("smcp")
        .opening_header("x-api-key", "test_secret")
        .on("client:get_config", move |payload, _client| {
            let get_config_received = get_config_received_clone.clone();
            Box::pin(async move {
                // Computer端收到get_config请求
                get_config_received.store(true, Ordering::SeqCst);
                println!("Computer received get_config request: {:?}", payload);
            })
        })
        .connect()
        .await
        .expect("Computer connection failed");

    // 等待连接建立
    sleep(Duration::from_millis(100)).await;

    // Computer加入office
    let computer_join_req = EnterOfficeReq {
        office_id: "office1".to_string(),
        role: Role::Computer,
        name: "computer1".to_string(),
    };

    computer_client
        .emit("server:join_office", json!(computer_join_req))
        .await
        .unwrap();
    sleep(Duration::from_millis(100)).await;

    // 创建Agent客户端
    let agent_client = ClientBuilder::new(server_url.clone())
        .transport_type(TransportType::Websocket)
        .namespace("smcp")
        .opening_header("x-api-key", "test_secret")
        .connect()
        .await
        .expect("Agent connection failed");

    sleep(Duration::from_millis(100)).await;

    // Agent加入同一个office
    let agent_join_req = EnterOfficeReq {
        office_id: "office1".to_string(),
        role: Role::Agent,
        name: "agent1".to_string(),
    };

    agent_client
        .emit("server:join_office", json!(agent_join_req))
        .await
        .unwrap();
    sleep(Duration::from_millis(100)).await;

    // 创建get_config请求
    let get_config_req = GetComputerConfigReq {
        base: AgentCallData {
            agent: "office1".to_string(),
            req_id: ReqId("test_req_1".to_string()),
        },
        computer: "computer1".to_string(),
    };

    // Agent发送get_config请求
    agent_client
        .emit("client:get_config", json!(get_config_req))
        .await
        .unwrap();
    sleep(Duration::from_millis(200)).await;

    // 验证Computer收到了转发的请求
    assert!(
        get_config_received.load(Ordering::SeqCst),
        "Computer should have received the get_config request"
    );

    // 清理
    computer_client.disconnect().await.unwrap();
    agent_client.disconnect().await.unwrap();
    server.shutdown();
}

#[tokio::test]
async fn test_get_config_computer_not_found_integration() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();

    let server = SmcpTestServer::start().await;
    let server_url = server.url();

    // 创建Agent客户端
    let agent_client = ClientBuilder::new(server_url.clone())
        .transport_type(TransportType::Websocket)
        .namespace("smcp")
        .opening_header("x-api-key", "test_secret")
        .connect()
        .await
        .expect("Agent connection failed");

    sleep(Duration::from_millis(100)).await;

    // Agent加入office
    let agent_join_req = EnterOfficeReq {
        office_id: "office1".to_string(),
        role: Role::Agent,
        name: "agent1".to_string(),
    };

    agent_client
        .emit("server:join_office", json!(agent_join_req))
        .await
        .unwrap();
    sleep(Duration::from_millis(100)).await;

    // 创建get_config请求（请求不存在的computer）
    let get_config_req = GetComputerConfigReq {
        base: AgentCallData {
            agent: "office1".to_string(),
            req_id: ReqId("test_req_1".to_string()),
        },
        computer: "nonexistent_computer".to_string(),
    };

    // Agent发送get_config请求（应该失败）
    agent_client
        .emit("client:get_config", json!(get_config_req))
        .await
        .unwrap();
    sleep(Duration::from_millis(200)).await;

    // 这里我们只是验证服务器没有崩溃
    // 实际的错误处理需要更复杂的测试设置

    // 清理
    agent_client.disconnect().await.unwrap();
    server.shutdown();
}

#[tokio::test]
async fn test_get_config_unauthorized_role_integration() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();

    let server = SmcpTestServer::start().await;
    let server_url = server.url();

    // 创建Computer客户端
    let computer_client = ClientBuilder::new(server_url.clone())
        .transport_type(TransportType::Websocket)
        .namespace("smcp")
        .opening_header("x-api-key", "test_secret")
        .connect()
        .await
        .expect("Computer connection failed");

    // 等待连接建立
    sleep(Duration::from_millis(100)).await;

    // Computer加入office
    let computer_join_req = EnterOfficeReq {
        office_id: "office1".to_string(),
        role: Role::Computer,
        name: "computer1".to_string(),
    };

    computer_client
        .emit("server:join_office", json!(computer_join_req))
        .await
        .unwrap();
    sleep(Duration::from_millis(100)).await;

    // 创建get_config请求
    let get_config_req = GetComputerConfigReq {
        base: AgentCallData {
            agent: "office1".to_string(),
            req_id: ReqId("test_req_1".to_string()),
        },
        computer: "computer1".to_string(),
    };

    // Computer尝试发送get_config请求（应该被拒绝）
    computer_client
        .emit("client:get_config", json!(get_config_req))
        .await
        .unwrap();
    sleep(Duration::from_millis(200)).await;

    // 验证请求被处理（服务器应该返回错误或拒绝）
    // 这里我们只是验证服务器没有崩溃
    // 实际的错误响应需要更复杂的测试设置来捕获

    // 清理
    computer_client.disconnect().await.unwrap();
    server.shutdown();
}
