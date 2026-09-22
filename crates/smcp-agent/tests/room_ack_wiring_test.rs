/*!
* 文件名: room_ack_wiring_test.rs
* 作者: JQQ
* 描述: Agent 侧房间 ack 消费的**端到端接线**测试（#226 复审 🔴3）/ End-to-end wiring tests for
*       agent-side room-ack consumption。
*
* 背景：本 PR 的头条行为变更是「`join_office` / `leave_office` 由 fire-and-forget `emit` 改为等 ack，
* 并由 `parse_room_ack` 按协议三分歧裁决」。在补齐本文件之前，全仓能观测这一行为的只有
* `protocol_error.rs` 的**纯函数**单测；唯一能把 `AsyncSmcpAgent` 接到真实服务端的地方
* （`rust_server_integration.rs`）只 `build_layer()` 后断言 `stats.total == 0`，**从不连接**。
* 后果：把 `call` 回退成 `emit` 没有任何测试会红——而「被 `4101` 拒绝却报成功」正是本 PR 要消灭的
* 缺陷形态。
*
* 本文件起一台**真实** hyper 服务端（含版本握手中间件、真实 Socket.IO 栈），用真实 Agent 端到端钉死：
* ① 入房失败（`4101`）⇒ `Err(Protocol{code: 4101})`，且 `details.office_id` 到达调用方；
* ② 成功路径（空 ack）⇒ `Ok(())`；
* ③ 未入房的 `client:*` ⇒ flat `404`；
* ④ `leave_office` 幂等成功；⑤ 退房后 `list_room` ⇒ `4103`（错误检查先行于 `req_id` 校验）。
*/

use std::net::SocketAddr;
use std::time::Duration;

use smcp_agent::{AsyncSmcpAgent, DefaultAuthProvider, SmcpAgentConfig, SmcpAgentError};
use smcp_server_core::SmcpServerBuilder;
use smcp_server_hyper::HyperServer;

/// 测试服务器共享密钥（服务端 `admin_secret` 与 Agent `api_key` 必须一致）。
const TEST_SECRET: &str = "test_secret";

/// 取一个当前空闲的本地端口（`bind(0)` → 读回端口 → 释放）。
async fn free_port() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    listener.local_addr().expect("local_addr").port()
}

/// 起一台 in-process SMCP 服务端，返回其 base URL（`http://127.0.0.1:<port>`）。
async fn spawn_smcp_server() -> String {
    let port = free_port().await;
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().expect("valid addr");

    let layer = SmcpServerBuilder::new()
        .with_default_auth(Some(TEST_SECRET.to_string()), None)
        .build_layer()
        .expect("build server layer");

    tokio::spawn(async move {
        if let Err(e) = HyperServer::new().with_layer(layer).run(addr).await {
            // 测试进程结束前 listener 一直存活；仅在异常退出时记录。
            eprintln!("in-process smcp server stopped: {e}");
        }
    });

    // 就绪探测：TCP connect 成功即认为已开始 accept（避免固定 sleep 的 flakiness）。
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    format!("http://127.0.0.1:{port}")
}

/// 建一个已配好密钥、指向固定 office 的 Agent（未连接）。
fn agent_for(name: &str, office: &str) -> AsyncSmcpAgent {
    let auth = DefaultAuthProvider::new(name.to_string(), office.to_string())
        .with_api_key(TEST_SECRET.to_string());
    let config = SmcpAgentConfig::new()
        .with_default_timeout(5)
        .with_tool_call_timeout(5);
    AsyncSmcpAgent::new(auth, config)
}

/// 从 `SmcpAgentError` 取出协议码（非协议错误返回 `None`）。
fn protocol_code(error: &SmcpAgentError) -> Option<i64> {
    match error {
        SmcpAgentError::Protocol(protocol) => Some(protocol.code),
        _ => None,
    }
}

#[tokio::test]
async fn test_join_office_surfaces_server_rejection_and_empty_ack_success() {
    let url = spawn_smcp_server().await;

    // ① 第一个 Agent 入房成功：服务端回**空 ack** ⇒ `Ok(())`（同时证明「等 ack」不会把成功误判成失败）。
    let mut first = agent_for("agent-one", "office-a");
    first.connect(&url).await.expect("first agent connect");
    first
        .join_office("agent-one")
        .await
        .expect("空 ack 必须判成功（协议 v0.5.0：成功 = 空 ack）");

    // ② 第二个 Agent 入同一房：服务端回 flat `ErrorPayload(4101)` ⇒ **必须**是 Err，而不是
    //    fire-and-forget 时代那种「日志写入房成功 + `Ok(())`」。
    let mut second = agent_for("agent-two", "office-a");
    second.connect(&url).await.expect("second agent connect");
    let rejection = second
        .join_office("agent-two")
        .await
        .expect_err("目标房已有 Agent（4101）时 join_office MUST 返回 Err");
    assert_eq!(
        protocol_code(&rejection),
        Some(4101),
        "拒绝码必须原样到达调用方（结构化分流，而非字符串日志）: {rejection:?}"
    );
    let SmcpAgentError::Protocol(protocol) = &rejection else {
        unreachable!("protocol_code 已断言为 Protocol 变体")
    };
    assert_eq!(protocol.message, "Room already has an agent");
    assert_eq!(
        protocol.details.get("office_id").and_then(|v| v.as_str()),
        Some("office-a"),
        "details.office_id 必须承载目标房（调用方据此分流）: {protocol:?}"
    );

    // ③ `403`（身份声明冲突）也要能被 Agent 读成**拒绝**（#226 复审 🔴4 的「接线」要求）：同一连接
    //    以另一个 name 再次入房 ⇒ 服务端按 events.md §server:join_office 判 403，Agent 必须拿到
    //    `Err(Protocol{403})`，而不是把它当成一次成功入房。
    let identity_conflict = first
        .join_office("another-name")
        .await
        .expect_err("同连接改 name 声明 MUST 被 403 拒绝");
    assert_eq!(
        protocol_code(&identity_conflict),
        Some(403),
        "403 必须落在协议错误闭集内并被读成拒绝: {identity_conflict:?}"
    );

    // ④ `client:*` 契约：第二个 Agent 从未真正入房 ⇒ 目标 Computer 无从定位 ⇒ flat 404
    //    （既不是内部错误，也不是静默成功）。
    let lookup = second
        .get_tools("does-not-exist")
        .await
        .expect_err("未入房 Agent 的 client:get_tools MUST 返回协议错误");
    assert_eq!(protocol_code(&lookup), Some(404), "{lookup:?}");

    // ⑤ 退房同样等 ack：无房时也是**幂等成功**（空 ack）⇒ `Ok(())`。
    second
        .leave_office()
        .await
        .expect("leave_office 在有/无房时均为空 ack 幂等成功");
    first
        .leave_office()
        .await
        .expect("已入房 Agent 的 leave_office 必须拿到空 ack");

    // ⑥ 退房后 `list_room` ⇒ `4103`。这条同时把 #226 P0-1 的第 3 条钉死：错误检查 MUST 先于
    //    `req_id` 校验，否则结构化拒绝会被误报成内部错误 `Missing req_id in response`。
    let list_error = first
        .list_room("office-a")
        .await
        .expect_err("无房会话 list_room MUST 返回 4103");
    assert_eq!(protocol_code(&list_error), Some(4103), "{list_error:?}");
    assert!(
        list_error.to_string().contains("Not in any room"),
        "4103 必须是 canonical 文案，而不是 `Missing req_id in response`: {list_error}"
    );
}
