//! Regression coverage for payload validation and Socket.IO room namespaces (#223).

#[path = "test_utils.rs"]
mod test_utils;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use smcp::{events, AgentCallData, LeaveOfficeReq, ListRoomReq, ReqId, Role, SMCP_NAMESPACE};
use test_utils::{
    ack_to_sender, create_client_with_handler, create_test_client, join_office, SmcpTestServer,
};
use tf_rust_socketio::Payload;
use tokio::sync::oneshot;

fn ack_value(payload: Payload) -> serde_json::Value {
    match payload {
        Payload::Text(mut values, _) => match values.pop().unwrap_or(serde_json::Value::Null) {
            serde_json::Value::Array(mut items) if items.len() == 1 && items[0].is_object() => {
                items.remove(0)
            }
            value => value,
        },
        _ => serde_json::Value::Null,
    }
}

async fn emit_with_ack(
    client: &tf_rust_socketio::asynchronous::Client,
    event: &str,
    data: serde_json::Value,
) -> serde_json::Value {
    let (tx, rx) = oneshot::channel();
    client
        .emit_with_ack(
            event,
            data,
            Duration::from_secs(5),
            ack_to_sender(tx, ack_value),
        )
        .await
        .expect("emit_with_ack failed");
    tokio::time::timeout(Duration::from_secs(5), rx)
        .await
        .expect("ack timeout")
        .expect("ack callback dropped")
}

#[tokio::test]
async fn malformed_ack_payloads_return_flat_bad_request() {
    let server = SmcpTestServer::start().await;
    let client = create_test_client(&server.url(), SMCP_NAMESPACE).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    let events = [
        events::SERVER_JOIN_OFFICE,
        events::SERVER_LEAVE_OFFICE,
        events::SERVER_LIST_ROOM,
        events::CLIENT_TOOL_CALL,
        events::CLIENT_GET_TOOLS,
        events::CLIENT_GET_DESKTOP,
        events::CLIENT_GET_CONFIG,
        events::CLIENT_GET_SKILLS,
        events::CLIENT_GET_SKILL,
        events::CLIENT_GET_BLOB,
        events::CLIENT_PUT_BLOB,
        events::CLIENT_GET_RESOURCES,
    ];

    for event in events {
        let response = emit_with_ack(&client, event, json!([{}])).await;
        assert_eq!(response["code"], 400, "{event} must reject malformed input");
        assert_eq!(response["message"], "Invalid request payload");
        assert!(response.get("details").is_none());
    }

    client.disconnect().await.unwrap();
    server.shutdown();
}

#[tokio::test]
async fn list_room_after_leaving_returns_not_in_room() {
    let server = SmcpTestServer::start().await;
    let client = create_test_client(&server.url(), SMCP_NAMESPACE).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    join_office(&client, Role::Agent, "office-a", "agent").await;

    let leave = LeaveOfficeReq {
        office_id: "office-a".to_string(),
    };
    let _ = emit_with_ack(&client, events::SERVER_LEAVE_OFFICE, json!(leave)).await;

    let list = ListRoomReq {
        base: AgentCallData {
            agent: "agent".to_string(),
            req_id: ReqId("list-after-leave".to_string()),
        },
        office_id: "office-a".to_string(),
    };
    let response = emit_with_ack(&client, events::SERVER_LIST_ROOM, json!(list)).await;
    assert_eq!(response["code"], 4103);
    assert_eq!(response["message"], "Session is not in an office");
    assert!(response.get("details").is_none());

    client.disconnect().await.unwrap();
    server.shutdown();
}

#[tokio::test]
async fn cross_room_list_is_flat_4104_without_peer_details() {
    let server = SmcpTestServer::start().await;
    let client = create_test_client(&server.url(), SMCP_NAMESPACE).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    join_office(&client, Role::Agent, "office-a", "agent").await;

    let list = ListRoomReq {
        base: AgentCallData {
            agent: "agent".to_string(),
            req_id: ReqId("cross-room".to_string()),
        },
        office_id: "office-b".to_string(),
    };
    let response = emit_with_ack(&client, events::SERVER_LIST_ROOM, json!(list)).await;
    assert_eq!(response["code"], 4104);
    assert_eq!(response["message"], "Cross-room access denied");
    assert_eq!(response["details"], json!({"office_id": "office-b"}));

    client.disconnect().await.unwrap();
    server.shutdown();
}

#[tokio::test]
async fn office_id_cannot_target_a_peer_sid_room() {
    let server = SmcpTestServer::start().await;
    let server_url = server.url();

    let notification_received = Arc::new(AtomicBool::new(false));
    let notification_flag = notification_received.clone();
    let victim = create_client_with_handler(
        &server_url,
        SMCP_NAMESPACE,
        events::NOTIFY_UPDATE_CONFIG,
        move |_, _| {
            let flag = notification_flag.clone();
            Box::pin(async move {
                flag.store(true, Ordering::SeqCst);
            })
        },
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let observer = create_test_client(&server_url, SMCP_NAMESPACE).await;
    join_office(&victim, Role::Computer, "office-a", "victim").await;
    join_office(&observer, Role::Agent, "office-a", "observer").await;

    let list_request = ListRoomReq {
        base: AgentCallData {
            agent: "observer".to_string(),
            req_id: ReqId("sid-lookup".to_string()),
        },
        office_id: "office-a".to_string(),
    };
    let room = emit_with_ack(&observer, events::SERVER_LIST_ROOM, json!(list_request)).await;
    let room = room
        .as_array()
        .and_then(|values| values.first())
        .filter(|value| value.get("sessions").is_some())
        .cloned()
        .unwrap_or(room);
    let victim_sid = room["sessions"]
        .as_array()
        .and_then(|sessions| {
            sessions.iter().find_map(|session| {
                (session["name"] == "victim")
                    .then(|| session["sid"].as_str().map(str::to_owned))
                    .flatten()
            })
        })
        .expect("list_room must expose the peer sid to an in-room observer");

    let attacker = create_test_client(&server_url, SMCP_NAMESPACE).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    join_office(&attacker, Role::Computer, &victim_sid, "attacker").await;
    attacker
        .emit(
            events::SERVER_UPDATE_CONFIG,
            json!({"computer": "attacker"}),
        )
        .await
        .expect("attacker broadcast failed");

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !notification_received.load(Ordering::SeqCst),
        "office_id=<peer sid> must not reach the peer's private SID room"
    );

    attacker.disconnect().await.unwrap();
    observer.disconnect().await.unwrap();
    victim.disconnect().await.unwrap();
    server.shutdown();
}

#[tokio::test]
async fn leave_office_uses_session_room_not_payload_room() {
    let server = SmcpTestServer::start().await;
    let server_url = server.url();
    let victim_notified = Arc::new(AtomicBool::new(false));
    let victim_notified_flag = victim_notified.clone();
    let victim = create_client_with_handler(
        &server_url,
        SMCP_NAMESPACE,
        events::NOTIFY_LEAVE_OFFICE,
        move |_, _| {
            let flag = victim_notified_flag.clone();
            Box::pin(async move {
                flag.store(true, Ordering::SeqCst);
            })
        },
    )
    .await;
    let attacker = create_test_client(&server_url, SMCP_NAMESPACE).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    join_office(&victim, Role::Agent, "office-b", "victim").await;
    join_office(&attacker, Role::Agent, "office-a", "attacker").await;

    let response = emit_with_ack(
        &attacker,
        events::SERVER_LEAVE_OFFICE,
        json!(LeaveOfficeReq {
            office_id: "office-b".to_string(),
        }),
    )
    .await;
    assert_eq!(
        response,
        json!([null]),
        "successful leave must use an empty ack"
    );

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !victim_notified.load(Ordering::SeqCst),
        "a mismatched payload office must not receive a leave notification"
    );

    let list = ListRoomReq {
        base: AgentCallData {
            agent: "attacker".to_string(),
            req_id: ReqId("leave-session-room".to_string()),
        },
        office_id: "office-a".to_string(),
    };
    let list_response = emit_with_ack(&attacker, events::SERVER_LIST_ROOM, json!(list)).await;
    assert_eq!(list_response["code"], 4103);

    attacker.disconnect().await.unwrap();
    victim.disconnect().await.unwrap();
    server.shutdown();
}

#[tokio::test]
async fn leave_office_without_session_room_is_idempotent_and_does_not_broadcast() {
    let server = SmcpTestServer::start().await;
    let server_url = server.url();
    let victim_notified = Arc::new(AtomicBool::new(false));
    let victim_notified_flag = victim_notified.clone();
    let victim = create_client_with_handler(
        &server_url,
        SMCP_NAMESPACE,
        events::NOTIFY_LEAVE_OFFICE,
        move |_, _| {
            let flag = victim_notified_flag.clone();
            Box::pin(async move {
                flag.store(true, Ordering::SeqCst);
            })
        },
    )
    .await;
    let idle = create_test_client(&server_url, SMCP_NAMESPACE).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    join_office(&victim, Role::Agent, "office-b", "victim").await;

    let response = emit_with_ack(
        &idle,
        events::SERVER_LEAVE_OFFICE,
        json!(LeaveOfficeReq {
            office_id: "office-b".to_string(),
        }),
    )
    .await;
    assert_eq!(
        response,
        json!([null]),
        "successful leave must use an empty ack"
    );

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !victim_notified.load(Ordering::SeqCst),
        "a session without an office must not broadcast using the payload office"
    );

    idle.disconnect().await.unwrap();
    victim.disconnect().await.unwrap();
    server.shutdown();
}
