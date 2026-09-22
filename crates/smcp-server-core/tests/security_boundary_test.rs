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

/// 以**多个实参**发出请求（Socket.IO 事件可以携带 `[event, arg0, arg1, …]`）。
///
/// 仅用于「有效载荷 + 多余实参」这类**实参个数**用例：`Vec<Value>` 经 `Into<Payload>` 变成
/// `Payload::Text(vec![…])`，即多个线上实参，而不是把它们打包进单个数组实参。
async fn emit_with_ack_args(
    client: &tf_rust_socketio::asynchronous::Client,
    event: &str,
    args: Vec<serde_json::Value>,
) -> serde_json::Value {
    let (tx, rx) = oneshot::channel();
    client
        .emit_with_ack(
            event,
            args,
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

    // 说明（#226 复审 🔴2）：本用例**不**再声称覆盖成员收敛——它用的连接从未入过任何房，
    // `socket.rooms()` 恒空，把 `converge_socket_rooms` 整段删掉也照样绿（本次实测确认）。
    // 「无房会话退房后不是任何房成员」的**判别性**覆盖交给
    // [`leave_office_converges_socket_room_membership`]：那条用例先让客户端**真的**进房、再退房，
    // 并带正对照证明广播确实会送达该房成员。
    idle.disconnect().await.unwrap();
    victim.disconnect().await.unwrap();
    server.shutdown();
}

/// #226 复审 🔴2：`converge_socket_rooms`（协议 `events.md` §server:leave_office 的 SHOULD 收敛路径）
/// 此前**零判别性覆盖**——原用例的客户端从未入过任何房，摘掉收敛调用仍全绿。
///
/// 判别性构造：先让 `leaver` **真的**在 `office-b` 里（正对照：此时 `late` 入场广播必须送达它，
/// 否则下面的负断言恒真），再让 `leaver` 退房（会话无房 ⇒ socket MUST 收敛出 `office:office-b`），
/// 然后让新的成员进 `office-b`——`leaver` **不得**再收到该房的 `notify:enter_office`。
/// 摘掉 `converge_socket_rooms` 即红（socket 会留在 `office:office-b` 里继续收广播）。
#[tokio::test]
async fn leave_office_converges_socket_room_membership() {
    let server = SmcpTestServer::start().await;
    let server_url = server.url();

    let entered_received = Arc::new(AtomicBool::new(false));
    let flag = entered_received.clone();
    let leaver = create_client_with_handler(
        &server_url,
        SMCP_NAMESPACE,
        events::NOTIFY_ENTER_OFFICE,
        move |_, _| {
            let flag = flag.clone();
            Box::pin(async move {
                flag.store(true, Ordering::SeqCst);
            })
        },
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    join_office(&leaver, Role::Agent, "office-b", "leaver").await;

    // ── 正对照：leaver 此刻是 office-b 成员 ⇒ 后来的成员入场广播 MUST 送达它 ──────────────
    let late = create_test_client(&server_url, SMCP_NAMESPACE).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    join_office(&late, Role::Computer, "office-b", "late").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        entered_received.swap(false, Ordering::SeqCst),
        "正对照失败：房内成员未收到 notify:enter_office，负断言将失去判别力"
    );

    // ── 退房：会话无房 ⇒ socket MUST 收敛出 office:office-b ────────────────────────────
    let leave = emit_with_ack(
        &leaver,
        events::SERVER_LEAVE_OFFICE,
        json!(LeaveOfficeReq {
            office_id: "office-b".to_string(),
        }),
    )
    .await;
    assert_empty_ack(&leave, "server:leave_office");
    tokio::time::sleep(Duration::from_millis(300)).await;

    // ── 负断言：office-b 再次发生成员变更时，已退房的 leaver MUST NOT 收到其广播 ──────────
    let newcomer = create_test_client(&server_url, SMCP_NAMESPACE).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    join_office(&newcomer, Role::Agent, "office-b", "newcomer").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !entered_received.load(Ordering::SeqCst),
        "退房后会话 MUST NOT 仍是 office:office-b 的成员（漂移收敛：socket 仍留在房里即红）"
    );

    newcomer.disconnect().await.unwrap();
    late.disconnect().await.unwrap();
    leaver.disconnect().await.unwrap();
    server.shutdown();
}

/// #226 复审 🔴4：`403`（身份声明冲突）此前在本仓**整个错误码零覆盖**。
///
/// 逐条对齐 Python `tests/unit_tests/server/test_room_event_acks.py`：`role` 半、`name` 半、
/// 无 `details`，外加「同身份的重复入房幂等成功」对照。协议 §各错误码标准字段总表没有 `403` 行，
/// 文案是双 SDK 自拟——恰是最该被钉住的字符串。
#[tokio::test]
async fn identity_claim_mismatch_returns_flat_403_without_details() {
    let server = SmcpTestServer::start().await;
    let server_url = server.url();
    let client = create_test_client(&server_url, SMCP_NAMESPACE).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    join_office(&client, Role::Agent, "office-a", "agent").await;

    // ① role 半：同一连接声明 `computer`，与既有会话的 `agent` 不符 ⇒ 403。
    let role_mismatch = emit_with_ack(
        &client,
        events::SERVER_JOIN_OFFICE,
        json!({"role": "computer", "name": "agent", "office_id": "office-a"}),
    )
    .await;
    assert_eq!(role_mismatch["code"], 403);
    assert_eq!(
        role_mismatch["message"],
        "Role or name mismatch with existing session"
    );
    assert!(
        role_mismatch.get("details").is_none(),
        "403 无 code-specific 字段，不得携带 details: {role_mismatch}"
    );

    // ② name 半：role 不变、name 变 ⇒ 同样 403（身份在一次连接内不可变更）。
    let name_mismatch = emit_with_ack(
        &client,
        events::SERVER_JOIN_OFFICE,
        json!({"role": "agent", "name": "someone-else", "office_id": "office-a"}),
    )
    .await;
    assert_eq!(name_mismatch["code"], 403);
    assert_eq!(
        name_mismatch["message"],
        "Role or name mismatch with existing session"
    );
    assert!(name_mismatch.get("details").is_none());

    // ③ 对照：身份**完全一致**的重复入房 MUST 幂等成功（空 ack）——证明 403 不是「重复 join 就拒」，
    // 也证明前两次拒绝没有污染会话状态（否则这里会因 name 预留冲突而 4105）。
    let same_identity = emit_with_ack(
        &client,
        events::SERVER_JOIN_OFFICE,
        json!({"role": "agent", "name": "agent", "office_id": "office-a"}),
    )
    .await;
    assert_empty_ack(&same_identity, "同身份重复 server:join_office");

    client.disconnect().await.unwrap();
    server.shutdown();
}

/// #226 复审 🟡3（**按协议接线**）：无房会话发起 `client:*` ⇒ flat `4103 Not in any room`。
///
/// 协议 `error-handling.md` §Not In Room 的触发时机明列「`client:*` 路由请求、`server:list_room`
/// 等」。此前 `relay_client_call` 对无房来源回的是「目标 Computer 找不到」的 flat `404`，把
/// 「先入房再重试即可」的**可自纠**状态伪装成「换个目标才有用」——调用方据此做出的纠错动作必然是错的。
///
/// 本用例覆盖两条边界：
/// ① **有会话、无房**（join 被拒 / 已退房）⇒ 4103（协议触发态，由 [`smcp::build_room_rejection_error`]
///    单点产出 canonical 文案，且无 `details`）；
/// ② **无会话记录**（从未 join）⇒ **不投递 ack**（协议 0.2.2 Server MAY 不 ack；服务端连「是谁在问」
///    都无从确认，回房间语义的 4103 反而是假装知道对方身份）。两条边界一起钉住，防止任一侧被顺手改掉。
#[tokio::test]
async fn no_room_client_call_returns_flat_4103_but_session_less_does_not_ack() {
    let server = SmcpTestServer::start().await;
    let client = create_test_client(&server.url(), SMCP_NAMESPACE).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    // ② 从未 join 的连接（无会话记录）：MUST NOT 收到任何 ack（发起方侧自行超时）。
    let (tx, rx) = oneshot::channel::<serde_json::Value>();
    client
        .emit_with_ack(
            events::CLIENT_GET_TOOLS,
            json!({"agent": "agent", "req_id": "session-less", "computer": "computer"}),
            Duration::from_secs(1),
            ack_to_sender(tx, ack_value),
        )
        .await
        .expect("emit_with_ack failed");
    assert!(
        tokio::time::timeout(Duration::from_millis(800), rx)
            .await
            .is_err(),
        "无会话连接发起 client:* MUST NOT 收到 ack（协议 0.2.2 允许不 ack）"
    );

    // 构造「有会话、无房」：先入房再退房（`commit_leave` 只清 `office_id`，会话记录仍在）。
    join_office(&client, Role::Agent, "office-a", "agent").await;
    let leave = emit_with_ack(
        &client,
        events::SERVER_LEAVE_OFFICE,
        json!(LeaveOfficeReq {
            office_id: "office-a".to_string(),
        }),
    )
    .await;
    assert_empty_ack(&leave, "server:leave_office");

    // ① 有会话、无 `office_id` ⇒ 无从定位目标 Computer ⇒ flat 4103（协议 §Not In Room）。
    let response = emit_with_ack(
        &client,
        events::CLIENT_GET_TOOLS,
        json!({"agent": "agent", "req_id": "no-room", "computer": "computer"}),
    )
    .await;

    assert_eq!(response["code"], 4103, "{response}");
    // canonical 文案与 `server:list_room` 同一 choke point 产出，逐字对齐协议 §Not In Room。
    assert_eq!(response["message"], "Not in any room");
    assert!(
        response.get("details").is_none(),
        "4103 无 code-specific 字段: {response}"
    );
    assert_ne!(
        response["code"], 404,
        "4103 是「你不在任何房间」（可自纠），不是「这个 Computer 不存在」"
    );

    client.disconnect().await.unwrap();
    server.shutdown();
}

/// #226 复审 🟡9：**有效载荷 + 多余实参 ⇒ flat `400`**（对齐 Python
/// `test_valid_payload_plus_extra_argument_is_rejected`）。
///
/// socketioxide 的解析器对非 tuple-like 的 `T` 只取**首参**、静默丢弃多余实参，故历史实现会
/// 「接受」这类报文。服务端统一改用 1-tuple 提取器（`SingleArg<T>`）后，实参个数不为 1 即解析失败 ⇒
/// 与其他载荷畸形走同一条 `400` 通道。摘掉 1-tuple（回退成 `TryData::<T>`）本用例即红——
/// 例如 `server:join_office` 会以空 ack 成功返回。
#[tokio::test]
async fn valid_payload_plus_extra_argument_returns_flat_bad_request() {
    let server = SmcpTestServer::start().await;
    let client = create_test_client(&server.url(), SMCP_NAMESPACE).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    let cases: [(&str, serde_json::Value); 4] = [
        (
            events::SERVER_JOIN_OFFICE,
            json!({"role": "agent", "name": "agent", "office_id": "office-a"}),
        ),
        (
            events::SERVER_LEAVE_OFFICE,
            json!({"office_id": "office-a"}),
        ),
        (
            events::SERVER_LIST_ROOM,
            json!({"agent": "agent", "req_id": "extra", "office_id": "office-a"}),
        ),
        (
            events::CLIENT_GET_TOOLS,
            json!({"agent": "agent", "req_id": "extra", "computer": "computer"}),
        ),
    ];

    for (event, valid_payload) in cases {
        let response = emit_with_ack_args(
            &client,
            event,
            vec![valid_payload, json!("surplus-argument")],
        )
        .await;
        assert_eq!(
            response["code"], 400,
            "{event}: 有效载荷 + 多余实参 MUST 回 flat 400: {response}"
        );
        assert_eq!(response["message"], "Invalid request payload");
        assert!(response.get("details").is_none());
    }

    client.disconnect().await.unwrap();
    server.shutdown();
}
