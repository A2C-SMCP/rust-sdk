//! protocol#66「一房至多一台 Computer」——room-model.md §一致性测试场景 全部 9 条的 Rust 实现。
//!
//! 场景表（`a2c-smcp-protocol/docs/specification/room-model.md`，记号：`A` = Agent，
//! `C1`/`C2` = 两台不同 Computer，`R1`/`R2` = 两个房间）：
//!
//! | # | 前置 | 动作 | 期望 |
//! |---|---|---|---|
//! | 1 | `A`、`C1` 在 `R1` | `C2` join `R1` | `4101` + `details {office_id, role:"computer"}`；成员不变、无 `notify:*` |
//! | 2 | 同 #1 且 `C2` 与 `C1` 同名 | `C2` join `R1` | 同 #1（不得回 `4105`）|
//! | 3 | `C1` 在 `R1` | `C1`（同一会话）再次 join `R1` | 空 ack；不重复广播 `notify:enter_office` |
//! | 4 | `C1` 在 `R1`，`C2` 在 `R2` | `C2` join `R1` | `4101`；`C2` 仍在 `R2`，`R2` 无 leave 广播（校验先于副作用）|
//! | 5 | `A`、`C1` 在 `R1` | `C1` leave → `C2` join `R1` | `A` 依次收到 leave(C1)、enter(C2)；路由到 C2 可达、到 C1 回 `404` |
//! | 6 | `A`、`C1` 在 `R1`，`C2` 在 `R2` | `C1` join `R2` | `4101`；`C1` 仍在 `R1` |
//! | 7 | `A` 在 `R1` | 另一 Agent join `R1` | `4101` + `details {office_id, role:"agent"}` |
//! | 8 | `R1` 为空 | `C1`、`C2` **并发** join `R1` | 恰一者空 ack，另一者 `4101 {role:"computer"}` |
//! | 9 | `A`、`C1` 在 `R1`，`R2` 空 | `C1` join `R2` | 空 ack；`R1` 收 leave(C1)，`R2` 收 enter(C1) |
//!
//! 说明（偏差显式化）：场景 #9 的 `R2` 在协议表中为「空」，但空房无接收方可供观测
//! `notify:enter_office` 广播。本实现先在 `R2` 放一名**观察者 Agent**（A2）——每 role 一席下
//! Agent 席与 Computer 席相互独立，`R2` 的 Computer 席仍为空，场景语义（席位空 ⇒ 换房成功 +
//! 双向广播）不变。

#[path = "test_utils.rs"]
mod test_utils;

use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use serde_json::{json, Value};
use tf_rust_socketio::asynchronous::{Client, ClientBuilder};
use tf_rust_socketio::{Event, TransportType};
use tokio::time::sleep;

use smcp::{events, SMCP_NAMESPACE};
use test_utils::*;

// ---------------------------------------------------------------------------
// 场景测试辅助
// ---------------------------------------------------------------------------

/// 通知记录器：分别记录 `notify:enter_office` / `notify:leave_office` 的载荷原文。
#[derive(Clone, Default)]
struct NotifyLog {
    enter: Arc<StdMutex<Vec<Value>>>,
    leave: Arc<StdMutex<Vec<Value>>>,
}

impl NotifyLog {
    fn enter_count(&self) -> usize {
        self.enter.lock().unwrap().len()
    }

    fn leave_count(&self) -> usize {
        self.leave.lock().unwrap().len()
    }
}

/// 创建**带通知记录**的客户端（同时订阅 enter / leave 两类广播）。
async fn create_recorder_client(server_url: &str) -> (Client, NotifyLog) {
    let log = NotifyLog::default();
    let enter_log = log.enter.clone();
    let leave_log = log.leave.clone();

    let ready = Arc::new(tokio::sync::Notify::new());
    let signal = ready.clone();

    let client = ClientBuilder::new(server_url)
        .transport_type(TransportType::Websocket)
        .namespace(SMCP_NAMESPACE)
        .auth(json!({"token": "test_secret"}))
        .on(Event::Connect, move |_, _| {
            let signal = signal.clone();
            Box::pin(async move {
                signal.notify_one();
            })
        })
        .on(events::NOTIFY_ENTER_OFFICE, move |payload, _| {
            let enter_log = enter_log.clone();
            Box::pin(async move {
                if let Some(value) = payload_json(&payload) {
                    enter_log.lock().unwrap().push(value);
                }
            })
        })
        .on(events::NOTIFY_LEAVE_OFFICE, move |payload, _| {
            let leave_log = leave_log.clone();
            Box::pin(async move {
                if let Some(value) = payload_json(&payload) {
                    leave_log.lock().unwrap().push(value);
                }
            })
        })
        .connect()
        .await
        .expect("recorder client connect failed");

    tokio::time::timeout(Duration::from_secs(5), ready.notified())
        .await
        .expect("recorder namespace connect timeout");

    (client, log)
}

/// 以房内 Agent 身份列出房内会话（`server:list_room`），返回 `sessions` 数组。
async fn list_sessions(agent_client: &Client, agent_name: &str, office_id: &str) -> Vec<Value> {
    let reply = emit_request(
        agent_client,
        events::SERVER_LIST_ROOM,
        json!({"agent": agent_name, "req_id": "req-list", "office_id": office_id}),
    )
    .await;
    reply
        .get("sessions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_else(|| panic!("list_room 应返回 sessions 数组，实得 {reply}"))
}

/// 房内会话的 `(name, role)` 集合（顺序无关）。
fn session_names(sessions: &[Value]) -> Vec<(String, String)> {
    let mut names: Vec<(String, String)> = sessions
        .iter()
        .map(|s| {
            (
                s["name"].as_str().unwrap_or_default().to_string(),
                s["role"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    names.sort();
    names
}

/// 轮询等待条件成立（真设施下广播是异步到达的；固定 sleep 要么慢要么脆）。
async fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if cond() {
            return true;
        }
        sleep(Duration::from_millis(20)).await;
    }
    cond()
}

/// 「无广播」断言前的静默窗口：给足服务端处理 + 网络投递的时间。
const QUIET_WINDOW: Duration = Duration::from_millis(400);

// ---------------------------------------------------------------------------
// 场景 #1 / #2
// ---------------------------------------------------------------------------

/// 场景 #1：`C2` join 已有 `C1` 的 `R1` ⇒ `4101 {office_id, role:"computer"}`；成员不变、无广播。
#[tokio::test]
async fn scenario_1_second_computer_rejected_members_unchanged() {
    let server = SmcpTestServer::start().await;
    let url = server.url();

    let (agent, log) = create_recorder_client(&url).await;
    let computer1 = create_test_client(&url, SMCP_NAMESPACE).await;
    let computer2 = create_test_client(&url, SMCP_NAMESPACE).await;

    join_office(&agent, smcp::Role::Agent, "R1", "A").await;
    join_office(&computer1, smcp::Role::Computer, "R1", "C1").await;
    assert!(
        wait_until(Duration::from_secs(3), || log.enter_count() == 1).await,
        "A 应先收到 C1 的 enter 广播"
    );

    let rejection = emit_join_for_ack(
        &computer2,
        json!({"role": "computer", "name": "C2", "office_id": "R1"}),
    )
    .await;
    assert_eq!(rejection["code"], smcp::error_codes::ROOM_FULL);
    assert_eq!(rejection["message"], "Room already has a computer");
    assert_eq!(
        rejection["details"],
        json!({"office_id": "R1", "role": "computer"})
    );

    // 成员不变 + 无任何 notify:* 广播。
    sleep(QUIET_WINDOW).await;
    assert_eq!(log.enter_count(), 1, "不得有新的 enter 广播");
    assert_eq!(log.leave_count(), 0, "不得有 leave 广播");
    assert_eq!(
        session_names(&list_sessions(&agent, "A", "R1").await),
        vec![
            ("A".to_string(), "agent".to_string()),
            ("C1".to_string(), "computer".to_string()),
        ],
        "R1 成员必须保持不变"
    );

    server.shutdown();
}

/// 场景 #2：`C2` 与 `C1` **同名** ⇒ 同 #1（`4101`，不得回 `4105`）。
///
/// 附 **4101 载荷字节级断言**（camel 序 = `ErrorPayload` 结构体字段序；`details` 内
/// `office_id` / `role` 序与 Python 参考实现一致）——对称 SDK 对拍时逐字节可比。
#[tokio::test]
async fn scenario_2_same_name_second_computer_returns_4101_not_4105() {
    let server = SmcpTestServer::start().await;
    let url = server.url();

    let agent = create_test_client(&url, SMCP_NAMESPACE).await;
    let computer1 = create_test_client(&url, SMCP_NAMESPACE).await;
    let computer2 = create_test_client(&url, SMCP_NAMESPACE).await;

    join_office(&agent, smcp::Role::Agent, "R1", "A").await;
    join_office(&computer1, smcp::Role::Computer, "R1", "same-name").await;

    let rejection = emit_join_for_ack(
        &computer2,
        json!({"role": "computer", "name": "same-name", "office_id": "R1"}),
    )
    .await;

    // 语义断言：4101 + computer 席 + 双键 details；且**绝不**是预留码 4105。
    assert_eq!(rejection["code"], smcp::error_codes::ROOM_FULL);
    assert_ne!(rejection["code"], smcp::error_codes::NAME_CONFLICT);
    assert_eq!(rejection["message"], "Room already has a computer");
    assert_eq!(
        rejection["details"],
        json!({"office_id": "R1", "role": "computer"})
    );
    // 4101 载荷**字节级**钉死在 `smcp` crate 的 `room_rejection_payload_is_byte_stable`
    // （该处直接控制 `ErrorPayload` 的序列化出口，可对 canonical 串逐字节比对；本层经客户端
    // 栈解析后键序已被归一，无法承载字节级断言）。

    server.shutdown();
}

// ---------------------------------------------------------------------------
// 场景 #3
// ---------------------------------------------------------------------------

/// 场景 #3：同一会话再次 join `R1` ⇒ 空 ack（幂等），**不**重复广播 `notify:enter_office`。
#[tokio::test]
async fn scenario_3_idempotent_rejoin_does_not_rebroadcast() {
    let server = SmcpTestServer::start().await;
    let url = server.url();

    let (agent, log) = create_recorder_client(&url).await;
    let computer1 = create_test_client(&url, SMCP_NAMESPACE).await;

    join_office(&agent, smcp::Role::Agent, "R1", "A").await;
    join_office(&computer1, smcp::Role::Computer, "R1", "C1").await;
    assert!(
        wait_until(Duration::from_secs(3), || log.enter_count() == 1).await,
        "A 应先收到 C1 的进入广播"
    );
    let enters_before = log.enter_count();

    // 同一会话重复 join：空 ack，且不得再广播。
    let ack = emit_join_for_ack(
        &computer1,
        json!({"role": "computer", "name": "C1", "office_id": "R1"}),
    )
    .await;
    assert_empty_ack(&ack, "同一会话重复 server:join_office");

    sleep(QUIET_WINDOW).await;
    assert_eq!(
        log.enter_count(),
        enters_before,
        "幂等重入 MUST NOT 重复广播 notify:enter_office（场景 #3）"
    );
    assert_eq!(log.leave_count(), 0);

    server.shutdown();
}

// ---------------------------------------------------------------------------
// 场景 #4 / #6
// ---------------------------------------------------------------------------

/// 场景 #4：`C2` 在 `R2`，目标 `R1` 席位被占 ⇒ `4101`；`C2` **仍在** `R2`，`R2` 无 leave 广播
/// （校验必须先于副作用）。
#[tokio::test]
async fn scenario_4_rejected_switch_keeps_old_room_and_no_leave_broadcast() {
    let server = SmcpTestServer::start().await;
    let url = server.url();

    let agent1 = create_test_client(&url, SMCP_NAMESPACE).await;
    let computer1 = create_test_client(&url, SMCP_NAMESPACE).await;
    let (agent2, log2) = create_recorder_client(&url).await;
    let computer2 = create_test_client(&url, SMCP_NAMESPACE).await;

    join_office(&agent1, smcp::Role::Agent, "R1", "A1").await;
    join_office(&computer1, smcp::Role::Computer, "R1", "C1").await;
    join_office(&agent2, smcp::Role::Agent, "R2", "A2").await;
    join_office(&computer2, smcp::Role::Computer, "R2", "C2").await;
    assert!(
        wait_until(Duration::from_secs(3), || log2.enter_count() == 1).await,
        "A2 应先收到 C2 的进入广播"
    );

    let rejection = emit_join_for_ack(
        &computer2,
        json!({"role": "computer", "name": "C2", "office_id": "R1"}),
    )
    .await;
    assert_eq!(rejection["code"], smcp::error_codes::ROOM_FULL);
    assert_eq!(
        rejection["details"],
        json!({"office_id": "R1", "role": "computer"})
    );

    // C2 仍在 R2；R2 未收到任何 leave 广播（拒绝发生在自动退房的**副作用之前**）。
    sleep(QUIET_WINDOW).await;
    assert_eq!(log2.leave_count(), 0, "被拒的换房不得向旧房广播 leave");
    assert_eq!(
        session_names(&list_sessions(&agent2, "A2", "R2").await),
        vec![
            ("A2".to_string(), "agent".to_string()),
            ("C2".to_string(), "computer".to_string()),
        ],
        "C2 必须仍留在 R2"
    );

    server.shutdown();
}

/// 场景 #6：`C1` 在 `R1`、`C2` 在 `R2`；`C1` join `R2`（席位被占）⇒ `4101`；`C1` **仍在** `R1`。
#[tokio::test]
async fn scenario_6_switch_into_occupied_room_keeps_current_membership() {
    let server = SmcpTestServer::start().await;
    let url = server.url();

    let (agent1, log1) = create_recorder_client(&url).await;
    let computer1 = create_test_client(&url, SMCP_NAMESPACE).await;
    let agent2 = create_test_client(&url, SMCP_NAMESPACE).await;
    let computer2 = create_test_client(&url, SMCP_NAMESPACE).await;

    join_office(&agent1, smcp::Role::Agent, "R1", "A1").await;
    join_office(&computer1, smcp::Role::Computer, "R1", "C1").await;
    join_office(&agent2, smcp::Role::Agent, "R2", "A2").await;
    join_office(&computer2, smcp::Role::Computer, "R2", "C2").await;
    assert!(
        wait_until(Duration::from_secs(3), || log1.enter_count() == 1).await,
        "A1 应先收到 C1 的进入广播"
    );

    let rejection = emit_join_for_ack(
        &computer1,
        json!({"role": "computer", "name": "C1", "office_id": "R2"}),
    )
    .await;
    assert_eq!(rejection["code"], smcp::error_codes::ROOM_FULL);
    assert_eq!(
        rejection["details"],
        json!({"office_id": "R2", "role": "computer"})
    );

    sleep(QUIET_WINDOW).await;
    assert_eq!(log1.leave_count(), 0, "被拒的换房不得向 R1 广播 leave");
    assert_eq!(
        session_names(&list_sessions(&agent1, "A1", "R1").await),
        vec![
            ("A1".to_string(), "agent".to_string()),
            ("C1".to_string(), "computer".to_string()),
        ],
        "C1 必须仍留在 R1"
    );

    server.shutdown();
}

// ---------------------------------------------------------------------------
// 场景 #5（换绑）
// ---------------------------------------------------------------------------

/// 场景 #5：`C1` 离房 → `C2` 入房 ⇒ `A` 依次收到 `leave(C1)`、`enter(C2)`；
/// 此后路由 `computer=C2` 可达、`computer=C1` 回 `404`。
#[tokio::test]
async fn scenario_5_rebind_broadcast_order_and_routing() {
    let server = SmcpTestServer::start().await;
    let url = server.url();

    let (agent, log) = create_recorder_client(&url).await;
    let computer1 = create_test_client(&url, SMCP_NAMESPACE).await;
    let computer2 = create_computer_stub(&url).await;

    join_office(&agent, smcp::Role::Agent, "R1", "A").await;
    join_office(&computer1, smcp::Role::Computer, "R1", "C1").await;
    assert!(
        wait_until(Duration::from_secs(3), || log.enter_count() == 1).await,
        "A 应先收到 C1 的进入广播"
    );

    // C1 显式离房 → 广播 leave(C1)。
    leave_office(&computer1, "R1").await;
    assert!(
        wait_until(Duration::from_secs(3), || log.leave_count() == 1).await,
        "A 应收到 C1 的离开广播"
    );

    // 席位释放后 C2 入房 → 广播 enter(C2)。
    join_office(&computer2, smcp::Role::Computer, "R1", "C2").await;
    assert!(
        wait_until(Duration::from_secs(3), || log.enter_count() == 2).await,
        "A 应收到 C2 的进入广播"
    );

    // 依次：先 leave(C1) 再 enter(C2)（不会出现两台同时在房的中间态）。
    let leave_payload = log.leave.lock().unwrap()[0].clone();
    assert_eq!(leave_payload["computer"], "C1");
    assert_eq!(leave_payload["office_id"], "R1");
    let second_enter = log.enter.lock().unwrap()[1].clone();
    assert_eq!(second_enter["computer"], "C2");
    assert_eq!(second_enter["office_id"], "R1");

    // 路由：computer=C2 可达（桩真实应答）；computer=C1 已不在房 ⇒ 404。
    let routed = emit_request(
        &agent,
        events::CLIENT_GET_TOOLS,
        json!({"agent": "A", "req_id": "req-to-c2", "computer": "C2"}),
    )
    .await;
    assert_eq!(
        routed["req_id"], "req-to-c2",
        "C2 应可路由并原样应答，实得 {routed}"
    );
    assert_eq!(routed["tools"], json!([]));

    let gone = emit_request(
        &agent,
        events::CLIENT_GET_TOOLS,
        json!({"agent": "A", "req_id": "req-to-c1", "computer": "C1"}),
    )
    .await;
    assert_eq!(
        gone["code"],
        smcp::error_codes::NOT_FOUND,
        "已离房的 C1 必须回 404（不得泄露存在于其它房），实得 {gone}"
    );

    server.shutdown();
}

// ---------------------------------------------------------------------------
// 场景 #7
// ---------------------------------------------------------------------------

/// 场景 #7：`R1` 已有 Agent，第二个 Agent join ⇒ `4101 {office_id, role:"agent"}`。
#[tokio::test]
async fn scenario_7_second_agent_rejected_with_role_detail() {
    let server = SmcpTestServer::start().await;
    let url = server.url();

    let agent1 = create_test_client(&url, SMCP_NAMESPACE).await;
    let agent2 = create_test_client(&url, SMCP_NAMESPACE).await;

    join_office(&agent1, smcp::Role::Agent, "R1", "A1").await;

    let rejection = emit_join_for_ack(
        &agent2,
        json!({"role": "agent", "name": "A2", "office_id": "R1"}),
    )
    .await;
    assert_eq!(rejection["code"], smcp::error_codes::ROOM_FULL);
    assert_eq!(rejection["message"], "Room already has an agent");
    assert_eq!(
        rejection["details"],
        json!({"office_id": "R1", "role": "agent"})
    );

    server.shutdown();
}

// ---------------------------------------------------------------------------
// 场景 #8
// ---------------------------------------------------------------------------

/// 场景 #8：空房上 `C1`、`C2` **并发** join ⇒ 恰一者空 ack、另一者 `4101 {role:"computer"}`；
/// 房内至多一台 Computer（席位检查 + 占席原子）。
///
/// 注：集成层单轮并发只证明「结果恒为恰一者成功」这一不变量；真正的交错覆盖在
/// `session.rs` 的 200 轮屏障并发单测（`test_concurrent_distinct_agents_cannot_share_one_room` /
/// `test_concurrent_same_name_computers_collide_exactly_once`）。
#[tokio::test]
async fn scenario_8_concurrent_joins_admit_exactly_one_computer() {
    let server = SmcpTestServer::start().await;
    let url = server.url();

    let computer1 = create_test_client(&url, SMCP_NAMESPACE).await;
    let computer2 = create_test_client(&url, SMCP_NAMESPACE).await;

    let (ack1, ack2) = tokio::join!(
        emit_join_for_ack(
            &computer1,
            json!({"role": "computer", "name": "C1", "office_id": "R1"})
        ),
        emit_join_for_ack(
            &computer2,
            json!({"role": "computer", "name": "C2", "office_id": "R1"})
        ),
    );

    let successes = [&ack1, &ack2]
        .iter()
        .filter(|ack| ack.as_array().is_some_and(|args| args.is_empty()))
        .count();
    let seat_rejections = [&ack1, &ack2]
        .iter()
        .filter(|ack| {
            ack["code"] == smcp::error_codes::ROOM_FULL && ack["details"]["role"] == "computer"
        })
        .count();
    assert_eq!(
        (successes, seat_rejections),
        (1, 1),
        "并发加入空房必须恰一者成功、另一者 4101{{role:computer}}（ack1={ack1}，ack2={ack2}）"
    );
    for ack in [&ack1, &ack2] {
        assert_empty_ack_or(ack, "并发 join：成功侧");
    }

    // 房内至多一台 Computer：事后由观察 Agent 入房清点。
    let observer = create_test_client(&url, SMCP_NAMESPACE).await;
    join_office(&observer, smcp::Role::Agent, "R1", "A").await;
    let sessions = list_sessions(&observer, "A", "R1").await;
    let computers = sessions
        .iter()
        .filter(|s| s["role"].as_str() == Some("computer"))
        .count();
    assert_eq!(computers, 1, "R1 至多一台 Computer，实得 {sessions:?}");

    server.shutdown();
}

/// 成功侧必须是空 ack；失败侧（`4101`）不作断言。
fn assert_empty_ack_or(ack: &Value, action: &str) {
    if ack.get("code").is_some() {
        return; // 失败侧由调用方的席位断言覆盖。
    }
    assert_empty_ack(ack, action);
}

// ---------------------------------------------------------------------------
// 场景 #9
// ---------------------------------------------------------------------------

/// 场景 #9：`C1` 从 `R1` 换到 `R2`（空 Computer 席）⇒ 空 ack；`R1` 收 `leave(C1)`、
/// `R2` 收 `enter(C1)`（正向自动换房广播）。
#[tokio::test]
async fn scenario_9_auto_switch_broadcasts_both_sides() {
    let server = SmcpTestServer::start().await;
    let url = server.url();

    let (agent1, log1) = create_recorder_client(&url).await;
    let computer1 = create_test_client(&url, SMCP_NAMESPACE).await;
    // 观察者 A2 常驻 R2（见文件头「偏差显式化」：R2 的 Computer 席仍为空）。
    let (agent2, log2) = create_recorder_client(&url).await;

    join_office(&agent1, smcp::Role::Agent, "R1", "A1").await;
    join_office(&computer1, smcp::Role::Computer, "R1", "C1").await;
    join_office(&agent2, smcp::Role::Agent, "R2", "A2").await;
    assert!(
        wait_until(Duration::from_secs(3), || log1.enter_count() == 1).await,
        "A1 应先收到 C1 的进入广播"
    );

    let ack = emit_join_for_ack(
        &computer1,
        json!({"role": "computer", "name": "C1", "office_id": "R2"}),
    )
    .await;
    assert_empty_ack(&ack, "Computer 自动换房");

    assert!(
        wait_until(Duration::from_secs(3), || log1.leave_count() == 1
            && log2.enter_count() == 1)
        .await,
        "R1 应收到 leave(C1)，R2 应收到 enter(C1)（A1.leave={}，A2.enter={}）",
        log1.leave_count(),
        log2.enter_count()
    );
    assert_eq!(log1.leave.lock().unwrap()[0]["computer"], "C1");
    assert_eq!(log2.enter.lock().unwrap()[0]["computer"], "C1");
    assert_eq!(log2.enter.lock().unwrap()[0]["office_id"], "R2");

    server.shutdown();
}
