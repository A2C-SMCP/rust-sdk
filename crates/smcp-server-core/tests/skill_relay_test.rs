//! SRV-02 (#50) / protocol#65：五个 `client:*` relay handler 注册 + 经统一 relay 转发。
//! Each of client:get_skills/get_skill/get_blob/resolve_path/get_resources is registered and routes
//! through the unified relay — an unknown Computer yields a flat ErrorPayload(404) ack (proves
//! registration + routing).

#[path = "test_utils.rs"]
mod test_utils;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tf_rust_socketio::asynchronous::Client;
use tf_rust_socketio::Payload;
use tokio::sync::oneshot;
use tokio::time::sleep;

use smcp::*;
use test_utils::*;

/// 发一个 `client:*` 并断言 ack 为 bare flat `ErrorPayload(404)`（Computer 不存在）/ assert a flat 404 ack。
async fn assert_flat_404(agent: &Client, event: &str, payload: Value) {
    let (tx, rx) = oneshot::channel::<Value>();
    agent
        .emit_with_ack(
            event,
            payload,
            Duration::from_secs(5),
            ack_to_sender(tx, |p| match p {
                Payload::Text(mut values, _) => values.pop().unwrap_or(Value::Null),
                _ => Value::Null,
            }),
        )
        .await
        .unwrap_or_else(|e| panic!("{event}: emit_with_ack failed: {e}"));

    let ack = tokio::time::timeout(Duration::from_secs(5), rx)
        .await
        .unwrap_or_else(|_| panic!("{event}: ack timeout (handler not registered?)"))
        .unwrap();
    // socketio ack 以 args 数组 [<v>] 投递，先解包。
    let body = ack
        .as_array()
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or(ack);
    assert_eq!(
        body.get("code").and_then(Value::as_i64),
        Some(404),
        "{event}: expected flat ErrorPayload(404), got {body}"
    );
    assert!(
        body.get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .contains("not found"),
        "{event}: expected not-found message, got {body}"
    );
    assert!(
        body.get("Err").is_none(),
        "{event}: must be bare flat ErrorPayload, not an {{\"Err\":..}} envelope"
    );
}

#[tokio::test]
async fn test_skill_channel_relays_route_flat_404() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();
    let server = SmcpTestServer::start().await;
    let server_url = server.url();

    let agent = create_test_client(&server_url, "smcp").await;
    sleep(Duration::from_millis(200)).await;
    join_office(&agent, Role::Agent, "office1", "agent1").await;

    let base = || AgentCallData {
        agent: "agent1".to_string(),
        req_id: ReqId("r1".to_string()),
    };

    // client:get_skills
    assert_flat_404(
        &agent,
        "client:get_skills",
        json!(GetSkillsReq {
            base: base(),
            computer: "nonexistent".to_string(),
        }),
    )
    .await;

    // client:get_skill
    assert_flat_404(
        &agent,
        "client:get_skill",
        json!(GetSkillReq {
            base: base(),
            computer: "nonexistent".to_string(),
            name: "some-skill".to_string(),
            rel_path: None,
        }),
    )
    .await;

    // client:get_blob
    assert_flat_404(
        &agent,
        "client:get_blob",
        json!(GetBlobReq {
            base: base(),
            computer: "nonexistent".to_string(),
            blob_handle: "opaque-handle".to_string(),
            chunk_offset: None,
            max_chunk_bytes: None,
        }),
    )
    .await;

    // client:resolve_path（v0.5.0，protocol#65）
    assert_flat_404(
        &agent,
        "client:resolve_path",
        json!(ResolvePathReq {
            base: base(),
            computer: "nonexistent".to_string(),
            path: "/tmp/x".to_string(),
        }),
    )
    .await;

    // client:get_resources
    assert_flat_404(
        &agent,
        "client:get_resources",
        json!(GetResourcesReq {
            base: base(),
            computer: "nonexistent".to_string(),
            mcp_server: "some-server".to_string(),
            cursor: None,
        }),
    )
    .await;

    agent.disconnect().await.unwrap();
    server.shutdown();
}

/// 证明 `on_client_resolve_path` 真正以 `client:resolve_path` 转发（若复制粘贴成 `client:get_blob`
/// 本用例会红：Computer 桩收不到事件、Agent 拿不到 ack），且请求载荷与 ack 裸透传。
#[tokio::test]
async fn test_resolve_path_relay_forwards_event_and_payload() {
    let server = SmcpTestServer::start().await;
    let server_url = server.url();

    let received = Arc::new(Mutex::new(None::<Value>));
    let received_clone = Arc::clone(&received);
    let computer = create_client_with_handler(
        &server_url,
        "smcp",
        "client:resolve_path",
        move |payload, client| {
            let received = Arc::clone(&received_clone);
            Box::pin(async move {
                let ack_id = match &payload {
                    Payload::Text(_, ack_id) => *ack_id,
                    #[allow(deprecated)]
                    Payload::String(_, ack_id) => *ack_id,
                    Payload::Binary(_, _) => None,
                };
                let Some(value) = payload_json(&payload) else {
                    return;
                };
                *received.lock().unwrap() = Some(value.clone());
                if let Some(id) = ack_id {
                    let _ = client
                        .ack_with_id(
                            id,
                            json!({
                                "canonical_path": value["path"],
                                "blob_handle": "opaque-handle",
                                "mime_type": "text/plain",
                                "total_size": 0,
                                "sha256": "abc123",
                                "mtime": 0,
                                "req_id": value["req_id"],
                            }),
                        )
                        .await;
                }
            })
        },
    )
    .await;
    join_office(&computer, Role::Computer, "office1", "computer1").await;

    let agent = create_test_client(&server_url, "smcp").await;
    sleep(Duration::from_millis(100)).await;
    join_office(&agent, Role::Agent, "office1", "agent1").await;
    sleep(Duration::from_millis(100)).await;

    let req = ResolvePathReq {
        base: AgentCallData {
            agent: "agent1".to_string(),
            req_id: ReqId("rp1".to_string()),
        },
        computer: "computer1".to_string(),
        path: "/tmp/authorized.txt".to_string(),
    };
    let (tx, rx) = oneshot::channel::<Value>();
    agent
        .emit_with_ack(
            "client:resolve_path",
            json!(req),
            Duration::from_secs(5),
            ack_to_sender(tx, |p| match p {
                Payload::Text(mut values, _) => values.pop().unwrap_or(Value::Null),
                _ => Value::Null,
            }),
        )
        .await
        .expect("emit resolve_path");
    let ack = tokio::time::timeout(Duration::from_secs(5), rx)
        .await
        .expect("ack timeout")
        .unwrap();
    let body = ack
        .as_array()
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or(ack);
    assert_eq!(body["canonical_path"], "/tmp/authorized.txt");
    assert_eq!(body["req_id"], "rp1");

    let forwarded = received
        .lock()
        .unwrap()
        .clone()
        .expect("computer stub must receive client:resolve_path");
    assert_eq!(forwarded["path"], "/tmp/authorized.txt");
    assert_eq!(forwarded["computer"], "computer1");
    assert_eq!(forwarded["req_id"], "rp1");
    // PR3：转发请求必须保留 `agent`（PR1 复审遗留）。
    assert_eq!(forwarded["agent"], "agent1");

    agent.disconnect().await.unwrap();
    computer.disconnect().await.unwrap();
    server.shutdown();
}

/// PR3：Computer 回传的 flat `4020` 必须**逐字**透传给 Agent（code / message / details.reason；
/// 不二次包裹、不丢字段）。
#[tokio::test]
async fn test_resolve_path_relay_passes_flat_4020_verbatim() {
    let server = SmcpTestServer::start().await;
    let server_url = server.url();

    let computer = create_client_with_handler(
        &server_url,
        "smcp",
        "client:resolve_path",
        move |payload, client| {
            Box::pin(async move {
                let ack_id = match &payload {
                    Payload::Text(_, ack_id) => *ack_id,
                    #[allow(deprecated)]
                    Payload::String(_, ack_id) => *ack_id,
                    Payload::Binary(_, _) => None,
                };
                if let Some(id) = ack_id {
                    let _ = client
                        .ack_with_id(
                            id,
                            json!({
                                "code": 4020,
                                "message": "Path not accessible",
                                "details": {"reason": "forbidden"}
                            }),
                        )
                        .await;
                }
            })
        },
    )
    .await;
    join_office(&computer, Role::Computer, "office1", "computer1").await;

    let agent = create_test_client(&server_url, "smcp").await;
    sleep(Duration::from_millis(100)).await;
    join_office(&agent, Role::Agent, "office1", "agent1").await;
    sleep(Duration::from_millis(100)).await;

    let req = ResolvePathReq {
        base: AgentCallData {
            agent: "agent1".to_string(),
            req_id: ReqId("rp-4020".to_string()),
        },
        computer: "computer1".to_string(),
        path: "/tmp/outside.txt".to_string(),
    };
    let (tx, rx) = oneshot::channel::<Value>();
    agent
        .emit_with_ack(
            "client:resolve_path",
            json!(req),
            Duration::from_secs(5),
            ack_to_sender(tx, |p| match p {
                Payload::Text(mut values, _) => values.pop().unwrap_or(Value::Null),
                _ => Value::Null,
            }),
        )
        .await
        .expect("emit resolve_path");
    let ack = tokio::time::timeout(Duration::from_secs(5), rx)
        .await
        .expect("ack timeout")
        .unwrap();
    let body = ack
        .as_array()
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or(ack);
    assert_eq!(body["code"], 4020);
    assert_eq!(body["message"], "Path not accessible");
    assert_eq!(body["details"]["reason"], "forbidden");
    // 嵌套包装会被 Agent 解析成成功（#226 P0-1 同类）——此处钉死顶层 code。
    assert!(
        body.get("data").is_none(),
        "flat payload must not be wrapped"
    );

    agent.disconnect().await.unwrap();
    computer.disconnect().await.unwrap();
    server.shutdown();
}
