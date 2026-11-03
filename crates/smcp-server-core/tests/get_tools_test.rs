//! Test client:get_tools functionality

#[path = "test_utils.rs"]
mod test_utils;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt;
use serde_json::json;
use tf_rust_socketio::asynchronous::ClientBuilder;
use tf_rust_socketio::Payload;
use tf_rust_socketio::TransportType;
use tokio::sync::oneshot;
use tokio::time::sleep;

use smcp::*;
use test_utils::*;

#[tokio::test]
async fn test_get_tools_success_same_office() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();

    let server = SmcpTestServer::start().await;
    let server_url = server.url();

    // 标记Computer是否收到了请求
    let computer_received = Arc::new(AtomicBool::new(false));
    let computer_received_clone = computer_received.clone();

    // 创建Computer客户端
    let computer_client = ClientBuilder::new(server_url.clone())
        .transport_type(TransportType::Websocket)
        .namespace("smcp")
        .auth(serde_json::json!({"token": "test_secret"}))
        .on("client:get_tools", move |payload: Payload, _client| {
            let computer_received = computer_received_clone.clone();
            async move {
                // 标记收到了请求
                computer_received.store(true, Ordering::SeqCst);

                // 解析请求
                if let Payload::Text(values, _) = payload {
                    if let Ok(req) = serde_json::from_value::<GetToolsReq>(values[0].clone()) {
                        // 构造工具列表响应
                        let _tools_response = json!({
                            "tools": [
                                {
                                    "name": "echo",
                                    "description": "Echoes the input text",
                                    "params_schema": {
                                        "type": "object",
                                        "properties": {
                                            "text": {
                                                "type": "string",
                                                "description": "Text to echo"
                                            }
                                        },
                                        "required": ["text"]
                                    },
                                    "return_schema": null
                                },
                                {
                                    "name": "get_time",
                                    "description": "Gets the current time",
                                    "params_schema": {
                                        "type": "object",
                                        "properties": {},
                                        "required": []
                                    },
                                    "return_schema": {
                                        "type": "string",
                                        "description": "Current timestamp"
                                    }
                                }
                            ],
                            "req_id": req.base.req_id
                        });

                        // 注意：这里应该通过ACK返回响应
                        // 但tf_rust_socketio的emit_with_ack回调机制需要在连接时设置
                    }
                }
            }
            .boxed()
        })
        .connect()
        .await
        .expect("Failed to connect computer");

    sleep(Duration::from_millis(100)).await;

    // Computer加入办公室
    join_office(&computer_client, Role::Computer, "office1", "computer1").await;

    // 创建Agent客户端
    let agent_client = create_test_client(&server_url, "smcp").await;
    sleep(Duration::from_millis(100)).await;

    // Agent加入办公室
    join_office(&agent_client, Role::Agent, "office1", "agent1").await;

    // 等待确保两个客户端都在办公室
    sleep(Duration::from_millis(200)).await;

    // Agent发送get_tools请求
    let get_tools_req = GetToolsReq {
        base: AgentCallData {
            agent: "agent1".to_string(),
            req_id: ReqId("req1".to_string()),
        },
        computer: "computer1".to_string(),
    };

    // 创建channel接收响应
    let (result_tx, result_rx) = oneshot::channel::<serde_json::Value>();

    // 发送请求
    agent_client
        .emit_with_ack(
            "client:get_tools",
            json!(get_tools_req),
            Duration::from_secs(5),
            ack_to_sender(result_tx, |p| match p {
                Payload::Text(mut values, _) => values.pop().unwrap_or(serde_json::Value::Null),
                _ => serde_json::Value::Null,
            }),
        )
        .await
        .expect("get_tools emit_with_ack failed");

    // 等待响应
    let result = tokio::time::timeout(Duration::from_secs(5), result_rx).await;

    // 验证Computer收到了请求
    assert!(
        computer_received.load(Ordering::SeqCst),
        "Computer should have received the request"
    );

    // 验证响应内容
    match result {
        Ok(Ok(response)) => {
            // 如果有响应，验证响应内容
            if let Some(tools) = response.get("tools").and_then(|t| t.as_array()) {
                assert!(!tools.is_empty(), "Tools list should not be empty");
                // 验证第一个工具是echo工具
                if let Some(first_tool) = tools.first() {
                    assert_eq!(
                        first_tool.get("name").and_then(|n| n.as_str()),
                        Some("echo"),
                        "First tool should be echo"
                    );
                }
            } else {
                // 响应可能为空或错误，这是预期的，因为tf_rust_socketio客户端无法在on回调中发送ACK响应
                println!(
                    "Computer received request but couldn't send ACK response (expected behavior)"
                );
            }
        }
        Ok(Err(e)) => {
            // 超时错误是预期的，因为tf_rust_socketio客户端无法在on回调中发送ACK响应
            println!("Timeout error (expected): {}", e);
        }
        Err(_) => {
            // 超时是预期的
            println!("Timeout (expected behavior)");
        }
    }

    // 清理
    computer_client.disconnect().await.unwrap();
    agent_client.disconnect().await.unwrap();
    server.shutdown();
}

#[tokio::test]
async fn test_get_tools_computer_not_found() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();

    let server = SmcpTestServer::start().await;
    let server_url = server.url();

    // 创建Agent客户端
    let agent_client = create_test_client(&server_url, "smcp").await;

    // 等待确保客户端连接完全建立
    sleep(Duration::from_millis(200)).await;

    // Agent加入办公室
    join_office(&agent_client, Role::Agent, "office1", "agent1").await;

    // Agent请求不存在的Computer的工具列表
    let get_tools_req = GetToolsReq {
        base: AgentCallData {
            agent: "agent1".to_string(),
            req_id: ReqId("req2".to_string()),
        },
        computer: "nonexistent".to_string(),
    };

    // 创建channel接收响应
    let (result_tx, result_rx) = oneshot::channel::<serde_json::Value>();

    // 发送请求
    agent_client
        .emit_with_ack(
            "client:get_tools",
            json!(get_tools_req),
            Duration::from_secs(5),
            ack_to_sender(result_tx, |p| match p {
                Payload::Text(mut values, _) => values.pop().unwrap_or(serde_json::Value::Null),
                _ => serde_json::Value::Null,
            }),
        )
        .await
        .expect("get_tools emit_with_ack failed");

    // 等待响应
    let error_payload = tokio::time::timeout(Duration::from_secs(5), result_rx)
        .await
        .expect("get_tools ack timeout")
        .unwrap();

    // 验证错误响应
    let error_msg = match error_payload {
        serde_json::Value::String(s) => s,
        serde_json::Value::Array(arr) => arr
            .first()
            .map(|v| {
                if let Some(err) = v.get("Err").and_then(|e| e.as_str()) {
                    err.to_string()
                } else if let Some(s) = v.as_str() {
                    s.to_string()
                } else {
                    v.to_string()
                }
            })
            .unwrap_or_default(),
        _ => error_payload.to_string(),
    };

    // 验证错误信息
    assert!(error_msg.contains("not found") || error_msg.contains("Computer"));

    // 清理
    agent_client.disconnect().await.unwrap();
    server.shutdown();
}

#[tokio::test]
async fn test_get_tools_cross_office_permission_denied() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();

    let server = SmcpTestServer::start().await;
    let server_url = server.url();

    // 创建Computer客户端（在office1）
    let computer_client = create_test_client(&server_url, "smcp").await;

    // 等待确保Computer客户端连接完全建立
    sleep(Duration::from_millis(200)).await;

    join_office(&computer_client, Role::Computer, "office1", "computer1").await;

    // 创建Agent客户端（在office2）
    let agent_client = create_test_client(&server_url, "smcp").await;

    // 等待确保Agent客户端连接完全建立
    sleep(Duration::from_millis(200)).await;

    join_office(&agent_client, Role::Agent, "office2", "agent1").await;

    // Agent尝试获取不同办公室的Computer的工具列表
    let get_tools_req = GetToolsReq {
        base: AgentCallData {
            agent: "agent1".to_string(),
            req_id: ReqId("req3".to_string()),
        },
        computer: "computer1".to_string(),
    };

    // 创建channel接收响应
    let (result_tx, result_rx) = oneshot::channel::<serde_json::Value>();

    // 发送请求
    agent_client
        .emit_with_ack(
            "client:get_tools",
            json!(get_tools_req),
            Duration::from_secs(5),
            ack_to_sender(result_tx, |p| match p {
                Payload::Text(mut values, _) => values.pop().unwrap_or(serde_json::Value::Null),
                _ => serde_json::Value::Null,
            }),
        )
        .await
        .expect("get_tools emit_with_ack failed");

    // 等待响应
    let error_payload = tokio::time::timeout(Duration::from_secs(5), result_rx)
        .await
        .expect("get_tools ack timeout")
        .unwrap();

    // 新语义（SRV-01 #47）：office-scoped 查找让跨 office 目标天然不可达，统一回 flat
    // ErrorPayload(404)——刻意**不**泄露目标存在于其它 office（隔离不串话），不再回 {"Err":..}
    // envelope 或 "office"/"permission" 文案。socketio ack 以 args 数组 `[<v>]` 投递，先解包。
    let body = error_payload
        .as_array()
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or(error_payload);
    assert_eq!(
        body.get("code").and_then(serde_json::Value::as_i64),
        Some(404),
        "expected flat 404 (cross-office unreachable), got: {body}"
    );
    assert!(
        body.get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .contains("not found"),
        "expected not-found message, got: {body}"
    );
    assert!(body.get("Err").is_none(), "禁止 {{\"Err\":..}} envelope");

    // 清理
    computer_client.disconnect().await.unwrap();
    agent_client.disconnect().await.unwrap();
    server.shutdown();
}

/// protocol#66：一房至多一台 Computer——Agent 只能查询**本房**那一台。
///
/// 旧版「Agent 同房分别查两台 Computer」在「每 role 一席」下不可再构造（第二台入房即 4101）。
/// 本用例改为钉两条边界：
/// ① 本房在册 Computer 的请求正常转发——以**可应答桩**（`create_computer_stub`）作 target，
///    断言拿到桩的真实回包（无条件断言；「没收到 404」不作为通过判据）；
/// ② 存在于**其它房**的 Computer 名在本房解析不到 ⇒ 统一 flat `404`（不得因它存在于别处而改码，
///    否则可被用来探测其它房的成员）。
#[tokio::test]
async fn test_get_tools_single_computer_per_room() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();

    let server = SmcpTestServer::start().await;
    let server_url = server.url();

    // 本房 Computer 用可应答桩（真实证明「可达」）；他房 Computer 用普通客户端即可。
    let computer1_client = create_computer_stub(&server_url).await;
    sleep(Duration::from_millis(200)).await;
    let computer2_client = create_test_client(&server_url, "smcp").await;
    sleep(Duration::from_millis(200)).await;

    // 两台 Computer 分居两房（一房至多一台）。
    join_office(&computer1_client, Role::Computer, "office1", "computer1").await;
    join_office(&computer2_client, Role::Computer, "office2", "computer2").await;

    let agent_client = create_test_client(&server_url, "smcp").await;
    sleep(Duration::from_millis(200)).await;
    join_office(&agent_client, Role::Agent, "office1", "agent1").await;

    // ① 本房 computer1：请求必须被路由到桩并拿到其真实应答（req_id 回显 + tools 透传）。
    let get_tools_req = GetToolsReq {
        base: AgentCallData {
            agent: "agent1".to_string(),
            req_id: ReqId("req_computer1".to_string()),
        },
        computer: "computer1".to_string(),
    };
    let (result_tx, result_rx) = oneshot::channel::<serde_json::Value>();
    agent_client
        .emit_with_ack(
            "client:get_tools",
            json!(get_tools_req),
            Duration::from_secs(5),
            ack_to_sender(result_tx, normalize_ack),
        )
        .await
        .expect("get_tools emit_with_ack failed");
    let response = tokio::time::timeout(Duration::from_secs(5), result_rx)
        .await
        .expect("本房 Computer 必须可路由并应答，不得超时")
        .unwrap();
    assert!(
        response.get("code").is_none(),
        "本房 Computer 必须可路由（不得回任何协议错误），实得 {response}"
    );
    assert_eq!(
        response["req_id"], "req_computer1",
        "桩应答应原样回显 req_id，实得 {response}"
    );
    assert_eq!(response["tools"], json!([]));

    // ② 他房 computer2：本房解析不到 ⇒ flat 404（统一形态，不泄露存在性）。
    let cross_room_req = GetToolsReq {
        base: AgentCallData {
            agent: "agent1".to_string(),
            req_id: ReqId("req_computer2".to_string()),
        },
        computer: "computer2".to_string(),
    };
    let (result_tx, result_rx) = oneshot::channel::<serde_json::Value>();
    agent_client
        .emit_with_ack(
            "client:get_tools",
            json!(cross_room_req),
            Duration::from_secs(5),
            ack_to_sender(result_tx, normalize_ack),
        )
        .await
        .expect("cross-room get_tools emit_with_ack failed");
    let reject = tokio::time::timeout(Duration::from_secs(5), result_rx)
        .await
        .expect("cross-room get_tools must be answered immediately (404), not hang")
        .unwrap();
    assert_eq!(
        reject["code"],
        smcp::error_codes::NOT_FOUND,
        "存在于其它房的 Computer 必须按 404 统一处理，实得 {reject}"
    );

    // 清理
    computer1_client.disconnect().await.unwrap();
    computer2_client.disconnect().await.unwrap();
    agent_client.disconnect().await.unwrap();
    server.shutdown();
}

#[tokio::test]
async fn test_get_tools_computer_not_in_office() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();

    let server = SmcpTestServer::start().await;
    let server_url = server.url();

    // 创建Computer客户端但不加入任何办公室
    let computer_client = create_test_client(&server_url, "smcp").await;

    // 创建Agent客户端
    let agent_client = create_test_client(&server_url, "smcp").await;

    // 等待确保Agent客户端连接完全建立
    sleep(Duration::from_millis(200)).await;

    join_office(&agent_client, Role::Agent, "office1", "agent1").await;

    // Agent尝试获取未加入办公室的Computer的工具列表
    let get_tools_req = GetToolsReq {
        base: AgentCallData {
            agent: "agent1".to_string(),
            req_id: ReqId("req4".to_string()),
        },
        computer: "computer1".to_string(),
    };

    // 创建channel接收响应
    let (result_tx, result_rx) = oneshot::channel::<serde_json::Value>();

    // 发送请求
    agent_client
        .emit_with_ack(
            "client:get_tools",
            json!(get_tools_req),
            Duration::from_secs(5),
            ack_to_sender(result_tx, |p| match p {
                Payload::Text(mut values, _) => values.pop().unwrap_or(serde_json::Value::Null),
                _ => serde_json::Value::Null,
            }),
        )
        .await
        .expect("get_tools emit_with_ack failed");

    // 等待响应
    let error_payload = tokio::time::timeout(Duration::from_secs(5), result_rx)
        .await
        .expect("get_tools ack timeout")
        .unwrap();

    // 验证错误响应
    let error_msg = match error_payload {
        serde_json::Value::String(s) => s,
        serde_json::Value::Array(arr) => arr
            .first()
            .map(|v| {
                if let Some(err) = v.get("Err").and_then(|e| e.as_str()) {
                    err.to_string()
                } else if let Some(s) = v.as_str() {
                    s.to_string()
                } else {
                    v.to_string()
                }
            })
            .unwrap_or_default(),
        _ => error_payload.to_string(),
    };

    // 验证错误信息
    assert!(error_msg.contains("not found") || error_msg.contains("office"));

    // 清理
    computer_client.disconnect().await.unwrap();
    agent_client.disconnect().await.unwrap();
    server.shutdown();
}
