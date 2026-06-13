//! SRV-02 (#50)：四个新 `client:*` relay handler 注册 + 经统一 relay 转发。
//! Each of client:get_skills/get_skill/get_blob/get_resources is registered and routes through the
//! unified relay — an unknown Computer yields a flat ErrorPayload(404) ack (proves registration + routing).

#[path = "test_utils.rs"]
mod test_utils;

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
