//! protocol#64（v0.5.0）`computer_id` 一致性场景 #10–#21 的 Rust 实现。
//!
//! 场景表见 `a2c-smcp-protocol/docs/specification/room-model.md` §一致性测试场景
//! （记号：`A` = Agent，`C1`/`C2` = 两台不同 Computer，`R1`/`R2` = 两个房间，
//! `X`/`Y` = 两个合法且不同的 `computer_id`）。
//!
//! | # | 前置 | 动作 | 期望 |
//! |---|---|---|---|
//! | 10 | `R1` 空 | `C1` join，`computer_id` 缺失/大写/无连字符/非 UUID/nil/max/带换行或空白 | `400`，未入房、无广播 |
//! | 11 | `R1` 空 | `A` join 并携带 `computer_id`（任意值，含 `null`）| `400`，未入房 |
//! | 12 | `A` 在 `R1` | `C1` join（`X`）| 空 ack；`A` 收 `enter(computer=C1, computer_id=X)`；`list_room` 中 `C1.computer_id=X`、`A` 无该键 |
//! | 13 | `C1` 在 `R1`（`X`）| 同一会话以 `Y` 再 join `R1` | `403`；仍在 `R1`、记录仍 `X`、无广播 |
//! | 14 | `C1` 在 `R1`（`X`）| 另一会话 `C2` 以同一 `X` join `R2` | 空 ack（不做重复检测）|
//! | 15 | `A`、`C1` 在 `R1`（`X`）| `C1` leave | `A` 收 `leave(computer=C1, computer_id=X)` |
//! | 16 | `A`、`C1` 在 `R1`（`X`），`R2` 空 | `C1` join `R2` | `R1` 收 `leave(X)`，`R2` 收 `enter(X)` |
//! | 17 | `C1` 在 `R1`（`X`）| 同一会话以 `X` 再 join `R1` | 空 ack（幂等），无广播 |
//! | 18 | `C1` 在 `R1`（`X`）| leave → 同一会话以 `Y` join `R1` | `403`（记录不因 leave 清除）；不在任何房 |
//! | 19 | `C1` 在 `R1`（`X`），`C2` 在 `R2` | `C1` 同一会话以 `Y` join `R2` | `403`（**非** `4101`）；仍在 `R1`、`R1` 无 leave |
//! | 20 | `A` 在 `R1` | `A` join `R2` 并携带 `computer_id` | `400`（**非** `4106`）；仍在 `R1` |
//! | 21 | `C1` 在 `R1`（`X`）| `A` join `R1` | `C1` 收 `enter(agent=A)` **无** `computer_id` 键（不得为 `null`）|

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

/// 两个合法且不同的 `computer_id`（小写规范 UUID）。
const X: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";
const Y: &str = "1d9c8a62-3e4b-4c5d-9f6a-7b8c9d0e1f20";

/// 「无广播」断言前的静默窗口。
const QUIET_WINDOW: Duration = Duration::from_millis(400);

const NIL_UUID: &str = "00000000-0000-0000-0000-000000000000";
const MAX_UUID: &str = "ffffffff-ffff-ffff-ffff-ffffffffffff";

// ---------------------------------------------------------------------------
// 场景辅助
// ---------------------------------------------------------------------------

/// 通知记录器（enter / leave 原始载荷）。
#[derive(Clone, Default)]
struct NotifyLog {
    enter: Arc<StdMutex<Vec<Value>>>,
    leave: Arc<StdMutex<Vec<Value>>>,
}

impl NotifyLog {
    fn enter(&self) -> Vec<Value> {
        self.enter.lock().unwrap().clone()
    }

    fn leave(&self) -> Vec<Value> {
        self.leave.lock().unwrap().clone()
    }

    fn enter_count(&self) -> usize {
        self.enter.lock().unwrap().len()
    }

    fn leave_count(&self) -> usize {
        self.leave.lock().unwrap().len()
    }
}

/// 创建带通知记录的客户端。
async fn create_recorder(server_url: &str) -> (Client, NotifyLog) {
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

/// 以房内 Agent 身份列出房内会话（`server:list_room`）。
async fn list_sessions(agent: &Client, agent_name: &str, office_id: &str) -> Vec<Value> {
    let reply = emit_request(
        agent,
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

/// 房内某成员的 `SessionInfo`。
fn session_of<'a>(sessions: &'a [Value], name: &str) -> &'a Value {
    sessions
        .iter()
        .find(|s| s["name"] == name)
        .unwrap_or_else(|| panic!("list_room 应包含成员 {name}，实得 {sessions:?}"))
}

/// 构造 join 载荷；`computer_id` 用 `Value` 以便显式发送 `null`。
fn join_payload(role: &str, name: &str, office_id: &str, computer_id: Option<Value>) -> Value {
    let mut payload = json!({"role": role, "name": name, "office_id": office_id});
    if let Some(value) = computer_id {
        payload["computer_id"] = value;
    }
    payload
}

/// Agent 入房（MUST NOT 携带 `computer_id`）。
async fn join_agent(client: &Client, name: &str, office_id: &str) {
    join_office(client, smcp::Role::Agent, office_id, name).await;
}

/// Computer 入房并断言成功（显式身份）。
async fn join_computer(client: &Client, name: &str, office_id: &str, computer_id: &str) {
    let ack = emit_join_for_ack(
        client,
        join_payload("computer", name, office_id, Some(json!(computer_id))),
    )
    .await;
    assert_empty_ack(&ack, "Computer join_office");
}

/// 轮询等待条件成立。
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

// ---------------------------------------------------------------------------
// 场景 #10 / #11：400（形态与 Agent 携带）
// ---------------------------------------------------------------------------

/// 场景 #10：`computer_id` 缺失 / 大写 / 无连字符 / 非 UUID / nil / max / 带换行 / 带空白 ⇒ `400`；
/// 未入房、无广播。
#[tokio::test]
async fn scenario_10_invalid_computer_id_is_rejected_with_400() {
    let server = SmcpTestServer::start().await;
    let url = server.url();

    let (agent, log) = create_recorder(&url).await;
    join_agent(&agent, "A", "R1").await;

    let computer = create_test_client(&url, SMCP_NAMESPACE).await;
    let invalid_payloads = [
        ("missing", join_payload("computer", "C1", "R1", None)),
        (
            "uppercase",
            join_payload("computer", "C1", "R1", Some(json!(X.to_uppercase()))),
        ),
        (
            "no-hyphens",
            join_payload("computer", "C1", "R1", Some(json!(X.replace('-', "")))),
        ),
        (
            "non-uuid",
            join_payload("computer", "C1", "R1", Some(json!("not-a-uuid"))),
        ),
        (
            "nil",
            join_payload("computer", "C1", "R1", Some(json!(NIL_UUID))),
        ),
        (
            "max",
            join_payload("computer", "C1", "R1", Some(json!(MAX_UUID))),
        ),
        (
            "trailing-newline",
            join_payload("computer", "C1", "R1", Some(json!(format!("{X}\n")))),
        ),
        (
            "leading-whitespace",
            join_payload("computer", "C1", "R1", Some(json!(format!(" {X}")))),
        ),
        (
            "trailing-whitespace",
            join_payload("computer", "C1", "R1", Some(json!(format!("{X} ")))),
        ),
        (
            "null",
            join_payload("computer", "C1", "R1", Some(Value::Null)),
        ),
    ];

    for (label, payload) in invalid_payloads {
        let ack = emit_join_for_ack(&computer, payload).await;
        assert_eq!(
            ack["code"], 400,
            "非法 computer_id（{label}）必须回 400，实得 {ack}"
        );
    }

    // 未入房、无广播。
    sleep(QUIET_WINDOW).await;
    assert_eq!(log.enter_count(), 0, "被拒的 join 不得产生 enter 广播");
    assert_eq!(log.leave_count(), 0);
    let sessions = list_sessions(&agent, "A", "R1").await;
    assert_eq!(sessions.len(), 1, "R1 只应有 A，实得 {sessions:?}");
    assert_eq!(sessions[0]["name"], "A");

    server.shutdown();
}

/// 场景 #11：Agent 携带 `computer_id`（任意值，**含 `null`**）⇒ `400`；未入房。
#[tokio::test]
async fn scenario_11_agent_with_computer_id_is_rejected_with_400() {
    let server = SmcpTestServer::start().await;
    let url = server.url();

    let (observer, _log) = create_recorder(&url).await;
    join_agent(&observer, "A", "R1").await;

    // 注意：用**新的** Agent 会话发送（同一连接改 role 会走别的 403 路径）。
    for (label, value) in [
        ("value", json!(X)),
        ("null", Value::Null),
        ("empty", json!("")),
    ] {
        let attacker = create_test_client(&url, SMCP_NAMESPACE).await;
        let ack =
            emit_join_for_ack(&attacker, join_payload("agent", "A2", "R1", Some(value))).await;
        assert_eq!(
            ack["code"], 400,
            "Agent 携带 computer_id（{label}，含 null）必须回 400，实得 {ack}"
        );
        let sessions = list_sessions(&observer, "A", "R1").await;
        assert_eq!(
            sessions.len(),
            1,
            "被拒的 Agent 不得入房，实得 {sessions:?}"
        );
    }

    server.shutdown();
}

// ---------------------------------------------------------------------------
// 场景 #12 / #21：回显与 Agent 键省略
// ---------------------------------------------------------------------------

/// 场景 #12：`A` 在 `R1`，`C1` 带 `X` 入房 ⇒ 空 ack；`enter(computer=C1, computer_id=X)`；
/// `list_room` 中 `C1.computer_id = X`、`A` 无 `computer_id` 键。
#[tokio::test]
async fn scenario_12_computer_identity_is_echoed_on_enter_and_list_room() {
    let server = SmcpTestServer::start().await;
    let url = server.url();

    let (agent, log) = create_recorder(&url).await;
    join_agent(&agent, "A", "R1").await;

    let computer = create_test_client(&url, SMCP_NAMESPACE).await;
    join_computer(&computer, "C1", "R1", X).await;

    assert!(
        wait_until(Duration::from_secs(3), || log.enter_count() == 1).await,
        "A 应收到 C1 的 enter 广播"
    );
    let enter = log.enter();
    assert_eq!(enter[0]["computer"], "C1");
    assert_eq!(enter[0]["computer_id"], X, "enter 通知必须回显身份");

    let sessions = list_sessions(&agent, "A", "R1").await;
    assert_eq!(
        session_of(&sessions, "C1")["computer_id"],
        X,
        "C1 的 SessionInfo 必须回显身份"
    );
    assert!(
        session_of(&sessions, "A").get("computer_id").is_none(),
        "Agent 的 SessionInfo 必须省略 computer_id 键（不得为 null）：{sessions:?}"
    );

    server.shutdown();
}

/// 场景 #21：`C1` 在 `R1`（`X`），`A` 入房 ⇒ `C1` 收到的 `enter(agent=A)` **无** `computer_id` 键。
#[tokio::test]
async fn scenario_21_agent_enter_notification_omits_computer_id() {
    let server = SmcpTestServer::start().await;
    let url = server.url();

    let (computer, log) = create_recorder(&url).await;
    join_computer(&computer, "C1", "R1", X).await;

    let agent = create_test_client(&url, SMCP_NAMESPACE).await;
    join_agent(&agent, "A", "R1").await;

    assert!(
        wait_until(Duration::from_secs(3), || log.enter_count() == 1).await,
        "C1 应收到 A 的 enter 广播"
    );
    let enter = log.enter();
    assert_eq!(enter[0]["agent"], "A");
    assert!(
        enter[0].get("computer_id").is_none(),
        "Agent 成员的 enter 通知必须省略 computer_id 键（不得为 null）：{enter:?}"
    );

    // 生产路径补充：Agent 成员的 **leave** 通知同样省略该键。
    leave_office(&agent, "R1").await;
    assert!(
        wait_until(Duration::from_secs(3), || log.leave_count() == 1).await,
        "C1 应收到 A 的 leave 广播"
    );
    let leave = log.leave();
    assert_eq!(leave[0]["agent"], "A");
    assert!(
        leave[0].get("computer_id").is_none(),
        "Agent 成员的 leave 通知必须省略 computer_id 键（不得为 null）：{leave:?}"
    );

    server.shutdown();
}

// ---------------------------------------------------------------------------
// 场景 #13 / #17：同一连接改值 403 / 同值幂等
// ---------------------------------------------------------------------------

/// 场景 #13：同一会话以 `Y` 再 join `R1` ⇒ `403`；仍在 `R1`、记录仍 `X`、无广播。
#[tokio::test]
async fn scenario_13_same_session_changing_computer_id_is_rejected_with_403() {
    let server = SmcpTestServer::start().await;
    let url = server.url();

    let (agent, log) = create_recorder(&url).await;
    join_agent(&agent, "A", "R1").await;

    let computer = create_test_client(&url, SMCP_NAMESPACE).await;
    join_computer(&computer, "C1", "R1", X).await;
    assert!(wait_until(Duration::from_secs(3), || log.enter_count() == 1).await);
    let enters_before = log.enter_count();

    let ack = emit_join_for_ack(
        &computer,
        join_payload("computer", "C1", "R1", Some(json!(Y))),
    )
    .await;
    assert_eq!(
        ack["code"], 403,
        "同会话改 computer_id 必须回 403，实得 {ack}"
    );

    sleep(QUIET_WINDOW).await;
    assert_eq!(log.enter_count(), enters_before, "403 不得产生广播");
    assert_eq!(log.leave_count(), 0);
    let sessions = list_sessions(&agent, "A", "R1").await;
    assert_eq!(
        session_of(&sessions, "C1")["computer_id"],
        X,
        "记录必须保持首次的值 X"
    );

    server.shutdown();
}

/// 场景 #17：同一会话以**同一** `X` 再 join `R1` ⇒ 空 ack（幂等），无广播。
#[tokio::test]
async fn scenario_17_same_computer_id_rejoin_is_idempotent() {
    let server = SmcpTestServer::start().await;
    let url = server.url();

    let (agent, log) = create_recorder(&url).await;
    join_agent(&agent, "A", "R1").await;

    let computer = create_test_client(&url, SMCP_NAMESPACE).await;
    join_computer(&computer, "C1", "R1", X).await;
    assert!(wait_until(Duration::from_secs(3), || log.enter_count() == 1).await);

    let ack = emit_join_for_ack(
        &computer,
        join_payload("computer", "C1", "R1", Some(json!(X))),
    )
    .await;
    assert_empty_ack(&ack, "同一身份幂等重入");

    sleep(QUIET_WINDOW).await;
    assert_eq!(log.enter_count(), 1, "幂等重入 MUST NOT 重复广播");
    assert_eq!(log.leave_count(), 0);

    server.shutdown();
}

// ---------------------------------------------------------------------------
// 场景 #14：协议不做 computer_id 冲突检测
// ---------------------------------------------------------------------------

/// 场景 #14：`C1` 在 `R1`（`X`），另一会话 `C2` 以同一 `X` join `R2` ⇒ 空 ack。
#[tokio::test]
async fn scenario_14_cloned_computer_id_is_not_rejected() {
    let server = SmcpTestServer::start().await;
    let url = server.url();

    let c1 = create_test_client(&url, SMCP_NAMESPACE).await;
    join_computer(&c1, "C1", "R1", X).await;

    let c2 = create_test_client(&url, SMCP_NAMESPACE).await;
    join_computer(&c2, "C2", "R2", X).await;

    server.shutdown();
}

// ---------------------------------------------------------------------------
// 场景 #15 / #16：leave / 换房回显
// ---------------------------------------------------------------------------

/// 场景 #15：`C1` 显式 leave ⇒ `A` 收到 `leave(computer=C1, computer_id=X)`。
#[tokio::test]
async fn scenario_15_leave_notification_echoes_computer_id() {
    let server = SmcpTestServer::start().await;
    let url = server.url();

    let (agent, log) = create_recorder(&url).await;
    join_agent(&agent, "A", "R1").await;

    let computer = create_test_client(&url, SMCP_NAMESPACE).await;
    join_computer(&computer, "C1", "R1", X).await;
    assert!(wait_until(Duration::from_secs(3), || log.enter_count() == 1).await);

    leave_office(&computer, "R1").await;

    assert!(
        wait_until(Duration::from_secs(3), || log.leave_count() == 1).await,
        "A 应收到 C1 的 leave 广播"
    );
    let leave = log.leave();
    assert_eq!(leave[0]["computer"], "C1");
    assert_eq!(leave[0]["computer_id"], X, "leave 通知必须回显身份");

    server.shutdown();
}

/// 场景 #16：`C1` 自动换房（`R1` → `R2`）⇒ `R1` 收 `leave(X)`、`R2` 收 `enter(X)`。
#[tokio::test]
async fn scenario_16_auto_switch_echoes_computer_id_on_both_sides() {
    let server = SmcpTestServer::start().await;
    let url = server.url();

    let (a1, log_r1) = create_recorder(&url).await;
    join_agent(&a1, "A1", "R1").await;
    let (a2, log_r2) = create_recorder(&url).await;
    join_agent(&a2, "A2", "R2").await;

    let computer = create_test_client(&url, SMCP_NAMESPACE).await;
    join_computer(&computer, "C1", "R1", X).await;
    assert!(wait_until(Duration::from_secs(3), || log_r1.enter_count() == 1).await);

    join_computer(&computer, "C1", "R2", X).await;

    assert!(
        wait_until(Duration::from_secs(3), || log_r1.leave_count() == 1).await,
        "R1 应收到 C1 的 leave"
    );
    assert!(
        wait_until(Duration::from_secs(3), || log_r2.enter_count() == 1).await,
        "R2 应收到 C1 的 enter"
    );
    assert_eq!(
        log_r1.leave()[0]["computer_id"],
        X,
        "R1 的 leave 必须回显身份"
    );
    assert_eq!(
        log_r2.enter()[0]["computer_id"],
        X,
        "R2 的 enter 必须回显身份"
    );

    server.shutdown();
}

// ---------------------------------------------------------------------------
// 场景 #18 / #19 / #20：记录跨 leave 存续与校验顺序
// ---------------------------------------------------------------------------

/// 场景 #18：`C1` leave 后同一会话以 `Y` 重新 join ⇒ `403`（记录不因 leave 清除）；当前不在任何房。
#[tokio::test]
async fn scenario_18_identity_record_survives_leave() {
    let server = SmcpTestServer::start().await;
    let url = server.url();

    let (agent, _log) = create_recorder(&url).await;
    join_agent(&agent, "A", "R1").await;

    let computer = create_test_client(&url, SMCP_NAMESPACE).await;
    join_computer(&computer, "C1", "R1", X).await;
    leave_office(&computer, "R1").await;

    let ack = emit_join_for_ack(
        &computer,
        join_payload("computer", "C1", "R1", Some(json!(Y))),
    )
    .await;
    assert_eq!(
        ack["code"], 403,
        "leave 不清除身份记录：同会话改值必须 403，实得 {ack}"
    );

    let sessions = list_sessions(&agent, "A", "R1").await;
    assert_eq!(sessions.len(), 1, "被拒的 C1 不得入房，实得 {sessions:?}");

    server.shutdown();
}

/// 场景 #19：`C1` 在 `R1`（`X`），同一会话以 `Y` join `R2` ⇒ `403`（**非** `4101`）；仍在 `R1`、无 leave。
#[tokio::test]
async fn scenario_19_identity_mismatch_precedes_seat_check() {
    let server = SmcpTestServer::start().await;
    let url = server.url();

    let (agent, log) = create_recorder(&url).await;
    join_agent(&agent, "A", "R1").await;

    let c1 = create_test_client(&url, SMCP_NAMESPACE).await;
    join_computer(&c1, "C1", "R1", X).await;
    assert!(wait_until(Duration::from_secs(3), || log.enter_count() == 1).await);

    let c2 = create_test_client(&url, SMCP_NAMESPACE).await;
    join_computer(&c2, "C2", "R2", Y).await;

    let ack = emit_join_for_ack(&c1, join_payload("computer", "C1", "R2", Some(json!(Y)))).await;
    assert_eq!(
        ack["code"], 403,
        "改值必须先于席位检查（403 而非 4101），实得 {ack}"
    );

    sleep(QUIET_WINDOW).await;
    assert_eq!(log.leave_count(), 0, "403 不得产生 leave 广播");
    let sessions = list_sessions(&agent, "A", "R1").await;
    assert!(
        sessions.iter().any(|s| s["name"] == "C1"),
        "C1 必须仍在 R1，实得 {sessions:?}"
    );

    server.shutdown();
}

/// 场景 #20：`A` 在 `R1`，join `R2` 并携带 `computer_id` ⇒ `400`（**非** `4106`）；仍在 `R1`。
#[tokio::test]
async fn scenario_20_agent_with_computer_id_is_400_not_4106() {
    let server = SmcpTestServer::start().await;
    let url = server.url();

    let (agent, _log) = create_recorder(&url).await;
    join_agent(&agent, "A", "R1").await;

    let ack = emit_join_for_ack(&agent, join_payload("agent", "A", "R2", Some(json!(X)))).await;
    assert_eq!(
        ack["code"], 400,
        "Agent 携带 computer_id 必须先于房间语义（400 而非 4106），实得 {ack}"
    );

    let sessions = list_sessions(&agent, "A", "R1").await;
    assert_eq!(sessions.len(), 1, "A 必须仍在 R1，实得 {sessions:?}");

    server.shutdown();
}

/// 记录时机补充（`400`/`403` 之后、`4106`/`4101` 之前）：首次 join 被 **`4101`** 拒绝后，
/// 该会话的 `computer_id` 记录**仍然保留**——随后同一会话换一个值去一个空房也会 `403`。
#[tokio::test]
async fn identity_record_survives_seat_rejection() {
    let server = SmcpTestServer::start().await;
    let url = server.url();

    let c1 = create_test_client(&url, SMCP_NAMESPACE).await;
    join_computer(&c1, "C1", "R1", X).await;

    // C2 首次 join 撞 4101（R1 的 Computer 席已占）——记录在席位检查**之前**发生。
    let c2 = create_test_client(&url, SMCP_NAMESPACE).await;
    let rejection =
        emit_join_for_ack(&c2, join_payload("computer", "C2", "R1", Some(json!(Y)))).await;
    assert_eq!(
        rejection["code"], 4101,
        "席位已占必须回 4101，实得 {rejection}"
    );

    // 同一会话改值去空房：若记录未保留，这里会成功；协议要求 403。
    let ack = emit_join_for_ack(
        &c2,
        join_payload(
            "computer",
            "C2",
            "R2",
            Some(json!("2e0d9b73-4f5c-4d6e-8a7b-9c0d1e2f3a40")),
        ),
    )
    .await;
    assert_eq!(
        ack["code"], 403,
        "4101 拒绝前已记录 Y：同会话改值必须 403，实得 {ack}"
    );

    server.shutdown();
}

/// 回显补充（4 处通知生产点之一）：Computer **断连**触发的 `leave` 同样回显身份
/// （room-model #15「`C1` leave `R1`（**或断连**）」）。
#[tokio::test]
async fn disconnect_notification_echoes_computer_id() {
    let server = SmcpTestServer::start().await;
    let url = server.url();

    let (agent, log) = create_recorder(&url).await;
    join_agent(&agent, "A", "R1").await;

    let computer = create_test_client(&url, SMCP_NAMESPACE).await;
    join_computer(&computer, "C1", "R1", X).await;
    assert!(wait_until(Duration::from_secs(3), || log.enter_count() == 1).await);

    computer.disconnect().await.expect("disconnect computer");

    assert!(
        wait_until(Duration::from_secs(3), || log.leave_count() == 1).await,
        "A 应收到断连触发的 leave 广播"
    );
    let leave = log.leave();
    assert_eq!(leave[0]["computer"], "C1");
    assert_eq!(leave[0]["computer_id"], X, "断连 leave 必须回显身份");

    server.shutdown();
}
