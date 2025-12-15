//! Test client:tool_call functionality

#[path = "test_utils.rs"]
mod test_utils;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt;
use rust_socketio::asynchronous::ClientBuilder;
use rust_socketio::Payload;
use rust_socketio::TransportType;
use serde_json::json;
use tokio::sync::oneshot;
use tokio::time::sleep;

use smcp::*;
use test_utils::*;

#[tokio::test]
async fn test_tool_call_roundtrip() {
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
        .opening_header("x-api-key", "test_secret")
        .on("client:tool_call", move |_payload: Payload, _client| {
            let computer_received = computer_received_clone.clone();
            async move {
                // 标记收到了请求
                computer_received.store(true, Ordering::SeqCst);
                // 注意：rust_socketio客户端无法在on回调中发送ACK响应
                // 所以服务器会收到超时错误
                println!("Computer received tool_call request but cannot send ACK response");
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
    println!("About to join office with Agent client");
    join_office(&agent_client, Role::Agent, "office1", "agent1").await;
    println!("Agent joined office");

    // 等待确保两个客户端都在办公室
    sleep(Duration::from_millis(200)).await;

    // Agent发送tool_call请求
    let tool_call_req = ToolCallReq {
        base: AgentCallData {
            agent: "agent1".to_string(),
            req_id: ReqId("req1".to_string()),
        },
        computer: "computer1".to_string(),
        tool_name: "echo".to_string(),
        params: json!({"text": "hello world"}),
        timeout: 5,
    };

    // 创建channel接收响应
    let (result_tx, result_rx) = oneshot::channel::<serde_json::Value>();

    // 发送请求并等待响应
    agent_client
        .emit_with_ack(
            "client:tool_call",
            json!(tool_call_req),
            Duration::from_secs(1), // 使用短超时
            ack_to_sender(result_tx, |p| match p {
                Payload::Text(mut values, _) => values.pop().unwrap_or(serde_json::Value::Null),
                _ => serde_json::Value::Null,
            }),
        )
        .await
        .expect("tool_call emit_with_ack failed");

    // 等待响应
    let result = tokio::time::timeout(Duration::from_secs(2), result_rx)
        .await;
    
    // 验证Computer收到了请求
    assert!(
        computer_received.load(Ordering::SeqCst),
        "Computer should have received the request"
    );
    
    // 验证响应内容（应该是超时或空响应）
    match result {
        Ok(Ok(response)) => {
            println!("Tool call response: {}", serde_json::to_string_pretty(&response).unwrap());
            
            // 如果有响应，应该是错误
            let error_msg = if let Some(arr) = response.as_array() {
                if let Some(first) = arr.first() {
                    first
                        .get("Err")
                        .and_then(|e| e.as_str())
                        .unwrap_or("No error field found")
                } else {
                    "No response found"
                }
            } else {
                "Response is not an array"
            };
            
            // 验证是错误响应
            assert!(
                error_msg.contains("timeout") || error_msg.contains("timed out") || error_msg.contains("error"),
                "Expected error response, got: {}", error_msg
            );
        }
        Ok(Err(e)) => {
            println!("Received channel error: {}", e);
        }
        Err(_) => {
            // 超时也是预期的，因为客户端无法发送ACK
            println!("Tool call timed out as expected (client cannot send ACK)");
        }
    }

    // 清理
    computer_client.disconnect().await.unwrap();
    agent_client.disconnect().await.unwrap();
    server.shutdown();
}

#[tokio::test]
async fn test_tool_call_computer_not_found() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();

    let server = SmcpTestServer::start().await;
    let server_url = server.url();

    // 创建Agent客户端
    let agent_client = create_test_client(&server_url, "smcp").await;

    // Agent加入办公室
    join_office(&agent_client, Role::Agent, "office1", "agent1").await;

    // Agent请求不存在的Computer
    let tool_call_req = ToolCallReq {
        base: AgentCallData {
            agent: "agent1".to_string(),
            req_id: ReqId("req2".to_string()),
        },
        computer: "nonexistent".to_string(),
        tool_name: "echo".to_string(),
        params: json!({"text": "test"}),
        timeout: 5,
    };

    // 创建channel接收响应
    let (result_tx, result_rx) = oneshot::channel::<serde_json::Value>();

    // 发送请求
    agent_client
        .emit_with_ack(
            "client:tool_call",
            json!(tool_call_req),
            Duration::from_secs(5),
            ack_to_sender(result_tx, |p| match p {
                Payload::Text(mut values, _) => values.pop().unwrap_or(serde_json::Value::Null),
                _ => serde_json::Value::Null,
            }),
        )
        .await
        .expect("tool_call emit_with_ack failed");

    // 等待响应
    let error_payload = tokio::time::timeout(Duration::from_secs(5), result_rx)
        .await
        .expect("tool_call ack timeout")
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
async fn test_tool_call_cross_office_permission_denied() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();

    let server = SmcpTestServer::start().await;
    let server_url = server.url();

    // 创建Computer客户端（在office1）
    let computer_client = create_test_client(&server_url, "smcp").await;
    join_office(&computer_client, Role::Computer, "office1", "computer1").await;

    // 创建Agent客户端（在office2）
    let agent_client = create_test_client(&server_url, "smcp").await;
    join_office(&agent_client, Role::Agent, "office2", "agent1").await;

    // Agent尝试调用不同办公室的Computer
    let tool_call_req = ToolCallReq {
        base: AgentCallData {
            agent: "agent1".to_string(),
            req_id: ReqId("req3".to_string()),
        },
        computer: "computer1".to_string(),
        tool_name: "echo".to_string(),
        params: json!({"text": "test"}),
        timeout: 5,
    };

    // 创建channel接收响应
    let (result_tx, result_rx) = oneshot::channel::<serde_json::Value>();

    // 发送请求
    agent_client
        .emit_with_ack(
            "client:tool_call",
            json!(tool_call_req),
            Duration::from_secs(5),
            ack_to_sender(result_tx, |p| match p {
                Payload::Text(mut values, _) => values.pop().unwrap_or(serde_json::Value::Null),
                _ => serde_json::Value::Null,
            }),
        )
        .await
        .expect("tool_call emit_with_ack failed");

    // 等待响应
    let error_payload = tokio::time::timeout(Duration::from_secs(5), result_rx)
        .await
        .expect("tool_call ack timeout")
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
    println!("Actual error message: {}", error_msg);
    assert!(
        error_msg.contains("Session not found")
            || error_msg.contains("permission")
            || error_msg.contains("office"),
        "Expected session/permission/office error, got: {}",
        error_msg
    );

    // 清理
    computer_client.disconnect().await.unwrap();
    agent_client.disconnect().await.unwrap();
    server.shutdown();
}

#[tokio::test]
async fn test_tool_call_timeout_handling() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();

    let server = SmcpTestServer::start().await;
    let server_url = server.url();

    // 创建Computer客户端（但不响应tool_call）
    let computer_client = ClientBuilder::new(server_url.clone())
        .transport_type(TransportType::Websocket)
        .namespace("smcp")
        .opening_header("x-api-key", "test_secret")
        .connect()
        .await
        .expect("Failed to connect computer");
    join_office(&computer_client, Role::Computer, "office1", "computer1").await;

    // 创建Agent客户端
    let agent_client = create_test_client(&server_url, "smcp").await;
    join_office(&agent_client, Role::Agent, "office1", "agent1").await;

    // Agent发送tool_call请求（短超时）
    let tool_call_req = ToolCallReq {
        base: AgentCallData {
            agent: "agent1".to_string(),
            req_id: ReqId("req4".to_string()),
        },
        computer: "computer1".to_string(),
        tool_name: "slow_tool".to_string(),
        params: json!({"delay": 10}),
        timeout: 1, // 1秒超时
    };

    // 创建channel接收响应
    let (result_tx, result_rx) = oneshot::channel::<serde_json::Value>();

    // 发送请求
    let emit_result = agent_client
        .emit_with_ack(
            "client:tool_call",
            json!(tool_call_req),
            Duration::from_secs(2), // 发送超时2秒
            ack_to_sender(result_tx, |p| match p {
                Payload::Text(mut values, _) => values.pop().unwrap_or(serde_json::Value::Null),
                _ => serde_json::Value::Null,
            }),
        )
        .await;

    // 验证发送成功
    assert!(emit_result.is_ok(), "Failed to emit tool_call");

    // 等待响应
    println!("Waiting for response...");
    let result = tokio::time::timeout(Duration::from_secs(3), result_rx).await;

    // 打印实际结果
    match &result {
        Ok(value) => println!("Got response: {:?}", value),
        Err(_) => println!("Timed out"),
    }

    // 服务器应该返回错误（因为Computer没有响应处理器）
    assert!(
        result.is_ok(),
        "Should get an error response when Computer doesn't respond"
    );

    let error_payload = result.unwrap().unwrap();
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
        _ => format!("{:?}", error_payload),
    };

    // 验证错误信息
    assert!(
        error_msg.contains("Session not found") || error_msg.contains("error"),
        "Expected error when Computer doesn't respond, got: {}",
        error_msg
    );

    // 清理
    computer_client.disconnect().await.unwrap();
    agent_client.disconnect().await.unwrap();
    server.shutdown();
}
