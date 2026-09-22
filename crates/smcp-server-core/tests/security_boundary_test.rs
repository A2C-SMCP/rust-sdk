//! Regression coverage for payload validation and Socket.IO room namespaces (#223).

#[path = "test_utils.rs"]
mod test_utils;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use smcp::{events, AgentCallData, LeaveOfficeReq, ListRoomReq, ReqId, Role, SMCP_NAMESPACE};
use test_utils::{
    ack_to_sender, assert_empty_ack, create_client_with_handler, create_test_client, join_office,
    SmcpTestServer,
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
async fn join_room_conflicts_return_distinct_flat_codes() {
    let server = SmcpTestServer::start().await;
    let server_url = server.url();
    let first_agent = create_test_client(&server_url, SMCP_NAMESPACE).await;
    let second_agent = create_test_client(&server_url, SMCP_NAMESPACE).await;
    let moving_agent = create_test_client(&server_url, SMCP_NAMESPACE).await;
    let first_computer = create_test_client(&server_url, SMCP_NAMESPACE).await;
    let second_computer = create_test_client(&server_url, SMCP_NAMESPACE).await;

    join_office(&first_agent, Role::Agent, "office-a", "first").await;
    join_office(&moving_agent, Role::Agent, "office-b", "moving").await;
    join_office(&first_computer, Role::Computer, "office-a", "shared").await;

    // 4101：目标房已有 Agent。文案与 details **逐字**对齐协议 error-handling.md §Room Full 与
    // python-sdk `_ROOM_REJECTION_MESSAGES` / `build_room_rejection_error`（#226 P1-5）。
    let full = emit_with_ack(
        &second_agent,
        events::SERVER_JOIN_OFFICE,
        json!({"role": "agent", "name": "second", "office_id": "office-a"}),
    )
    .await;
    assert_eq!(full["code"], 4101);
    assert_eq!(full["message"], "Room already has an agent");
    assert_eq!(full["details"], json!({"office_id": "office-a"}));

    // 4106：Agent 已在其它房。`details.office_id` 报的是会话**当前**所在房（office-b），
    // **不是**被拒的目标房（office-c）——报错目标房会让客户端误判自己身在何处。
    let already_in_room = emit_with_ack(
        &moving_agent,
        events::SERVER_JOIN_OFFICE,
        json!({"role": "agent", "name": "moving", "office_id": "office-c"}),
    )
    .await;
    assert_eq!(already_in_room["code"], 4106);
    assert_eq!(already_in_room["message"], "Agent already in another room");
    assert_eq!(already_in_room["details"], json!({"office_id": "office-b"}));

    // 4105：房内已有**同 role 同名**会话（跨 role 同名是允许的，故此处用两台 Computer）。
    let name_conflict = emit_with_ack(
        &second_computer,
        events::SERVER_JOIN_OFFICE,
        json!({"role": "computer", "name": "shared", "office_id": "office-a"}),
    )
    .await;
    assert_eq!(name_conflict["code"], 4105);
    assert_eq!(name_conflict["message"], "Name already taken in room");
    assert_eq!(
        name_conflict["details"],
        json!({"office_id": "office-a", "role": "computer"})
    );

    // 拒绝载荷**只**含 canonical 文案与自身上下文：不得出现内部错误类名 / `"Session error: "` 前缀
    // （历史实现直接序列化 `SessionError` 的 Display 上 wire）。
    for payload in [&full, &already_in_room, &name_conflict] {
        let serialized = payload.to_string();
        assert!(
            !serialized.contains("Session error"),
            "拒绝载荷不得泄露内部错误类名: {serialized}"
        );
    }

    first_agent.disconnect().await.unwrap();
    second_agent.disconnect().await.unwrap();
    moving_agent.disconnect().await.unwrap();
    first_computer.disconnect().await.unwrap();
    second_computer.disconnect().await.unwrap();
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
    // canonical 文案逐字对齐协议 error-handling.md §Not In Room 与 python-sdk `_ROOM_REJECTION_MESSAGES`。
    assert_eq!(response["message"], "Not in any room");
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
async fn attacker_office_id_cannot_disturb_another_offices_members() {
    // #226 假绿清单第 1 条的替代，以及它的**诚实边界说明**。
    //
    // 旧版 `office_id_cannot_target_a_peer_sid_room` 断言「office_id = 对端 SID 打不进对端的 SID 私房」。
    // 该断言在本栈**恒真**：socketioxide **不会**把连接加入以其 SID 命名的房（唯一房间写入口是
    // `Socket::join`，全仓无 `socket.join(sid)`；自动入 SID 私房的是 python-socketio），故「打不进」
    // 不依赖任何本仓代码——把 `office:` 前缀回退成裸 `office_id` 也照样绿。已实测确认该结论：
    // 摘掉前缀后旧断言仍为绿，故它不是 `#223` 想验的那条回归。
    //
    // 因此本用例改为断言**可观测且与本仓代码相关**的不变量：攻击者用任意构造的 `office_id`
    // （含对端 SID、含形如目标房名的字符串）加入后，
    //   ① 不会收到目标房成员的广播；② 不出现在目标房的成员表里；③ 目标房成员表不变。
    // 这三条会随「房间作用域 / 广播目标取会话状态」的实现回退而变红（例如把广播目标改成取自载荷）。
    //
    // `office:` 前缀本身的价值是**跨实现确定性**（python-socketio 有 SID 私房、本栈没有，前缀使两种
    // 栈的房名空间都与 SID 解耦），其形式化断言由单元测试
    // `handler::tests::test_office_room_is_disjoint_from_socket_sid_namespace` 承担——那条**是**判别性的
    // （去掉前缀即红）。
    let server = SmcpTestServer::start().await;
    let server_url = server.url();

    let notification_received = Arc::new(AtomicBool::new(false));
    let notification_flag = notification_received.clone();
    // victim 在 office `target` 内**作为成员**接收本房广播；攻击者若挤进同一房间，会看到同一标记。
    let attacker = create_client_with_handler(
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
    let victim = create_test_client(&server_url, SMCP_NAMESPACE).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    join_office(&victim, Role::Computer, "target", "victim").await;
    // 构造：把目标房的**房间名形态**当作自己的 office_id（房间名是可猜的常量前缀 + 房号，故客户端
    // 完全可以把「另一个房的房间名」当作自己的 office_id 提交——这正是要证明无害的构造）。
    join_office(&attacker, Role::Agent, "office:target", "attacker").await;

    // victim 的广播只投给 office `target` 的成员。
    victim
        .emit(events::SERVER_UPDATE_CONFIG, json!({"computer": "victim"}))
        .await
        .expect("victim broadcast failed");

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !notification_received.load(Ordering::SeqCst),
        "office_id=\"office:target\" 不得让攻击者落进 office `target` 的房间并收到其广播"
    );

    // 反向核对：攻击者自己的房间确实存在且不自洽地等于目标房——`list_room` 的权威视图里，
    // office `target` 只应有 victim（房间名空间不同 ⇒ 成员互不可见）。
    let list = ListRoomReq {
        base: AgentCallData {
            agent: "victim".to_string(),
            req_id: ReqId("room-members".to_string()),
        },
        office_id: "target".to_string(),
    };
    let observer = create_test_client(&server_url, SMCP_NAMESPACE).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    join_office(&observer, Role::Agent, "target", "observer").await;
    let members = emit_with_ack(&observer, events::SERVER_LIST_ROOM, json!(list)).await;
    let names = members["sessions"]
        .as_array()
        .expect("list_room must return sessions")
        .iter()
        .filter_map(|session| session["name"].as_str())
        .collect::<Vec<_>>();
    assert!(
        !names.contains(&"attacker"),
        "攻击者不得出现在 office `target` 的成员表内: {names:?}"
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
    assert_empty_ack(&response, "server:leave_office");

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
    assert_empty_ack(&response, "server:leave_office");

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !victim_notified.load(Ordering::SeqCst),
        "a session without an office must not broadcast using the payload office"
    );

    // #226 假绿清单第 3 条：旧版唯一断言是「没有广播」，把成员收敛整段删掉照样绿（idle 客户端的
    // `socket.rooms()` 为空，所有 leave 分支都是死路）。故补一条**可判别**的收敛断言：idle 客户端
    // 退房后必须仍**不是**该房成员——目标房成员后续的入场广播不得送达它。
    let entered_office_received = Arc::new(AtomicBool::new(false));
    let room_flag = entered_office_received.clone();
    let idle = create_client_with_handler(
        &server_url,
        SMCP_NAMESPACE,
        events::NOTIFY_ENTER_OFFICE,
        move |_, _| {
            let flag = room_flag.clone();
            Box::pin(async move {
                flag.store(true, Ordering::SeqCst);
            })
        },
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let leave_again = emit_with_ack(
        &idle,
        events::SERVER_LEAVE_OFFICE,
        json!(LeaveOfficeReq {
            office_id: "office-b".to_string(),
        }),
    )
    .await;
    assert_empty_ack(&leave_again, "server:leave_office（无会话连接）");

    // 让 office-b 发生一次成员变更：该房成员会收到 notify:enter_office。无房会话 MUST NOT 收到。
    let late = create_test_client(&server_url, SMCP_NAMESPACE).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    join_office(&late, Role::Computer, "office-b", "late").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !entered_office_received.load(Ordering::SeqCst),
        "无房会话退房后 MUST NOT 是任何房的成员（漂移收敛）"
    );

    late.disconnect().await.unwrap();
    idle.disconnect().await.unwrap();
    victim.disconnect().await.unwrap();
    server.shutdown();
}
