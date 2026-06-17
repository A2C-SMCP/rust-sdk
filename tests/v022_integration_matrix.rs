//! REL-01 (#24) —— workspace v0.2.2 集成测试矩阵 / workspace v0.2.2 integration test matrix。
//!
//! 跨 crate（smcp / agent / computer / server-core / server-hyper）以**真实** socket.io
//! （`socketioxide` 服务端 + `tf-rust-socketio` 客户端）+ **真实** stdio MCP 子进程，覆盖协议
//! 0.2.0 / 0.2.1 / 0.2.2 全功能面，作为收尾发布（REL-02）的门槛。
//!
//! 运行 / Run：`cargo test-e2e`（= `cargo test --workspace --features e2e -- --ignored`）。
//! 缺省（`cargo test-ws` / `cargo test-all`）整文件经 `#![cfg(feature = "e2e")]` cfg-out，零开销。
//!
//! 驱动模型 / Driving model：
//! - **Computer 侧**用真实 [`smcp_computer::computer::Computer`]（能正确回 ACK，并经 INT-03 #72 共享
//!   真实 manager + 注入 `ComputerHandlerOps` 消费 resources/skill/blob/cancel）。
//! - **Agent 侧**用裸 `tf-rust-socketio` 客户端发 `client:*` 事件、断言 flat ack（错误码 / meta /
//!   ErrorPayload 结构）——裸客户端无法在 `on` 回调里回 ACK，故只当请求发起方，不当 Computer。
//! - **握手三态 / WS-only** 单独用启用版本握手中间件的 [`smcp_server_hyper::HyperServer`] + 裸 HTTP
//!   断言（与转发场景解耦：转发用无握手的裸 server，聚焦 relay 语义）。
//!
//! Python 参考对标 / Mirrors Python e2e：`test_v02_full_flow` / `test_v02_skill_blob_e2e` /
//! `test_v02_tool_call_binary_e2e` / `test_version_handshake_conftest_server` / 隔离硬化。
//!
//! 覆盖矩阵 / Coverage matrix（issue #24 的 11 类场景）：
//! 1. 版本握手三态  → [`handshake_tristate_http`] + [`handshake_compatible_connects`]
//! 2. WS-only 拒绝   → [`ws_only_rejected_without_version`]
//! 3. get_resources → [`get_resources_transparent_passthrough`] + [`get_resources_unknown_server_4014`]
//!    3b. get_desktop（窗口聚合视图：仅 `window://` + `window` 精确过滤 + `desktop_size` 截断）
//!    → [`get_desktop_window_filter_and_size`]（WIN-01/02 端到端，对照 get_resources 透传）
//! 4. SKILL 发现/读取 → [`skills_discovery_and_read`] + [`skill_traversal_rejected_4017`]
//! 5. blob drain    → [`tool_call_binary_blob_roundtrip`]（分块 offset/eof/sha256 重组）
//! 6. tool_call 二进制 → [`tool_call_binary_blob_roundtrip`]（`_meta.a2c_blob_handle` 旁路）
//! 7. tool_call 取消/超时 → [`tool_call_cancel_fireforget_and_broadcast`] + [`tool_call_timeout_marks_meta`]
//! 8. in-flight disconnect → [`originator_disconnect_server_survives`]
//! 9. flat ErrorPayload → [`tool_call_unknown_computer_flat_404`] + 各场景 flat 断言
//! 10. marketplace strict / 11. 治理层 → 见文末 `governance_coverage` 文档（crate 级覆盖，对齐 Python
//!     在 CLI/unit 层而非 socket.io e2e 层测治理）。
#![cfg(feature = "e2e")]

// harness 置于同名子目录（cargo 不把 `tests/<dir>/` 内文件当独立测试二进制），用 `#[path]` 显式挂载。
#[path = "v022_integration_matrix/harness.rs"]
mod harness;

use serde_json::{json, Value};
use tempfile::TempDir;

use smcp::{
    events, is_protocol_error_payload, AgentCallData, GetBlobReq, GetBlobRet, GetDesktopReq,
    GetResourcesReq, GetSkillReq, GetSkillsReq, ReqId, Role, ToolCallReq, PROTOCOL_VERSION,
};

use harness::{
    agent_client, deep_find, emit_call, http_get, join, spawn_computer, to_hex, HandshakeServer,
    RelayServer, AGENT, COMPUTER, MCP_NAME, NS, OFFICE, SECRET,
};

// ───────────────────────────── 1 & 2：版本握手 / Version handshake ─────────────────────────────

/// 场景 1：版本握手三态（HTTP 层，逐字段对齐 Python `middleware.py` + Rust `version_handshake.rs`）。
///
/// - 缺失 `a2c_version`  → HTTP 400 + flat `code=400`，**无** `X-A2C-Error-Code` header。
/// - 非法 `a2c_version`  → HTTP 400 + flat `code=400`，**无** header。
/// - 不兼容（0.1.0）     → HTTP 400 + `X-A2C-Error-Code: 4008` + flat `code=4008` + 顶层 4 个版本字段。
/// - 兼容（PROTOCOL_VERSION）→ 放行（engine.io 握手返回 200，非 400）。
#[tokio::test]
#[ignore = "e2e: REL-01 v0.2.2 matrix; run via cargo test-e2e"]
async fn handshake_tristate_http() {
    let server = HandshakeServer::start().await;
    let base = format!("{}/socket.io/?EIO=4&transport=polling", server.http());

    // 缺失
    let (status, hdr, body) = http_get(&base).await;
    assert_eq!(status, 400, "missing a2c_version 应 400, body={body}");
    assert_eq!(body["code"], 400);
    assert!(hdr.is_none(), "missing 不应带 X-A2C-Error-Code");
    assert!(body.get("error").is_none(), "flat，无嵌套 envelope");

    // 非法
    let (status, hdr, body) = http_get(&format!("{base}&a2c_version=not-a-version")).await;
    assert_eq!(status, 400, "invalid a2c_version 应 400");
    assert_eq!(body["code"], 400);
    assert!(hdr.is_none());

    // 不兼容
    let (status, hdr, body) = http_get(&format!("{base}&a2c_version=0.1.0")).await;
    assert_eq!(status, 400, "incompatible 应 400");
    assert_eq!(body["code"], 4008, "不兼容应回 4008, body={body}");
    assert_eq!(
        hdr.as_deref(),
        Some("4008"),
        "mismatch 应带 X-A2C-Error-Code: 4008"
    );
    assert_eq!(body["server_version"], PROTOCOL_VERSION);
    assert_eq!(body["client_version"], "0.1.0");
    assert_eq!(body["min_supported"], "0.2.0");
    assert_eq!(body["max_supported"], "0.2.999");

    // 兼容 → 放行（200）
    let (status, hdr, _body) = http_get(&format!("{base}&a2c_version={PROTOCOL_VERSION}")).await;
    assert_eq!(status, 200, "兼容版本应放行返回 engine.io 握手 200");
    assert!(hdr.is_none(), "放行不应带错误 header");

    server.shutdown();
}

/// 场景 1（续）：兼容版本的客户端经握手中间件后能真正建立 socket.io 连接并 join_office。
#[tokio::test]
#[ignore = "e2e: REL-01 v0.2.2 matrix; run via cargo test-e2e"]
async fn handshake_compatible_connects() {
    use tf_rust_socketio::asynchronous::ClientBuilder;
    use tf_rust_socketio::TransportType;

    let server = HandshakeServer::start().await;
    // 连接 URL 携带兼容 a2c_version（对齐 client 侧 HS-02 握手）。
    let url = format!("{}/?a2c_version={}", server.http(), PROTOCOL_VERSION);
    let client = ClientBuilder::new(url)
        .transport_type(TransportType::Websocket)
        .namespace(NS)
        .auth(serde_json::json!({"token": SECRET}))
        .connect()
        .await
        .expect("兼容版本应连接成功");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    join(&client, Role::Agent, OFFICE, AGENT).await;
    client.disconnect().await.unwrap();
    server.shutdown();
}

/// 场景 2：WS-only 握手拒绝。hyper 承载下握手中间件按「路径 + query `a2c_version`」统一拦截，
/// 天然覆盖 websocket-upgrade 入口——服务端能对升级请求返回 HTTP 400 denial-response，**无需**发
/// WS close 4900（4900 仅供无法返回 denial 的承载栈）。故断言：
/// - 强制 websocket 传输、缺 a2c_version 的裸客户端 → connect 失败（被 400 拒绝）。
/// - 裸 HTTP 模拟 websocket-upgrade GET、缺 a2c_version → HTTP 400 + flat `code=400`。
#[tokio::test]
#[ignore = "e2e: REL-01 v0.2.2 matrix; run via cargo test-e2e"]
async fn ws_only_rejected_without_version() {
    use tf_rust_socketio::asynchronous::ClientBuilder;
    use tf_rust_socketio::TransportType;

    let server = HandshakeServer::start().await;

    // 强制 websocket 传输、URL 不带 a2c_version → 握手被拒，connect 返回 Err。
    let res = ClientBuilder::new(server.http())
        .transport_type(TransportType::Websocket)
        .namespace(NS)
        .auth(serde_json::json!({"token": SECRET}))
        .connect()
        .await;
    assert!(res.is_err(), "WS-only 且缺 a2c_version 应被握手中间件拒绝");

    // 裸 HTTP 模拟 websocket transport 入口、缺版本 → 400（证明 gate 覆盖 WS 升级路径，非仅 polling）。
    let ws_entry = format!("{}/socket.io/?EIO=4&transport=websocket", server.http());
    let (status, hdr, body) = http_get(&ws_entry).await;
    assert_eq!(
        status, 400,
        "websocket 入口缺版本应 400（denial-response，非 4900 close）"
    );
    assert_eq!(body["code"], 400);
    assert!(hdr.is_none());

    server.shutdown();
}

// ───────────────────────── 3：get_resources 透明转发 / transparent forward ─────────────────────────

/// 场景 3：Computer 透明转发 MCP `resources/list`——窗口与非窗口资源**都**返回（与 desktop 仅
/// `window://` 不同），对齐 Python `get_resources` 透传语义。
#[tokio::test]
#[ignore = "e2e: REL-01 v0.2.2 matrix; run via cargo test-e2e"]
async fn get_resources_transparent_passthrough() {
    let td = TempDir::new().unwrap();
    let server = RelayServer::start().await;
    let computer = spawn_computer(&server.url(), OFFICE, COMPUTER, &td, None).await;
    let agent = agent_client(&server.url()).await;
    join(&agent, Role::Agent, OFFICE, AGENT).await;

    let req = GetResourcesReq {
        base: AgentCallData {
            agent: AGENT.into(),
            req_id: ReqId("res-1".into()),
        },
        computer: COMPUTER.into(),
        mcp_server: MCP_NAME.into(),
        cursor: None,
    };
    let body = emit_call(&agent, events::CLIENT_GET_RESOURCES, json!(req)).await;
    assert!(
        body.get("code").is_none(),
        "get_resources 不应回错误: {body}"
    );

    let resources = body["resources"].as_array().cloned().unwrap_or_default();
    let uris: Vec<&str> = resources.iter().filter_map(|r| r["uri"].as_str()).collect();
    assert!(
        uris.iter().any(|u| u.starts_with("window://")),
        "应含 window:// 资源, got: {uris:?}"
    );
    assert!(
        uris.iter().any(|u| u.starts_with("file://")),
        "透明转发：非 window 资源也应返回（区别于 desktop）, got: {uris:?}"
    );

    computer.shutdown().await.unwrap();
    agent.disconnect().await.unwrap();
    server.shutdown();
}

/// 场景 3（续）：未知 MCP server 名 → flat `ErrorPayload(4014)`（McpServerNotFound）。
#[tokio::test]
#[ignore = "e2e: REL-01 v0.2.2 matrix; run via cargo test-e2e"]
async fn get_resources_unknown_server_4014() {
    let td = TempDir::new().unwrap();
    let server = RelayServer::start().await;
    let computer = spawn_computer(&server.url(), OFFICE, COMPUTER, &td, None).await;
    let agent = agent_client(&server.url()).await;
    join(&agent, Role::Agent, OFFICE, AGENT).await;

    let req = GetResourcesReq {
        base: AgentCallData {
            agent: AGENT.into(),
            req_id: ReqId("res-x".into()),
        },
        computer: COMPUTER.into(),
        mcp_server: "does-not-exist".into(),
        cursor: None,
    };
    let body = emit_call(&agent, events::CLIENT_GET_RESOURCES, json!(req)).await;
    assert!(
        is_protocol_error_payload(&body),
        "未知 server 应回 flat ErrorPayload: {body}"
    );
    assert_eq!(body["code"], 4014, "未知 MCP server 应 4014, got: {body}");

    computer.shutdown().await.unwrap();
    agent.disconnect().await.unwrap();
    server.shutdown();
}

// ─────────────────── 3b：get_desktop 窗口聚合视图 / window-only desktop view ───────────────────

/// 场景 3b（WIN-01/02 端到端）：`client:get_desktop` 的**窗口聚合视图**——区别于 get_resources 的透明
/// 转发，desktop **仅**聚合 `window://` 资源（过滤 `file://` 等非窗口），并验证两个可选参数：
/// - `window`：**精确匹配**某个 window URI，仅返回该窗口；不存在则返回空（非错误）。
/// - `desktop_size`：全局截断返回条数。
///
/// 复用 v022 MCP 的 3 个资源：2 个 `window://`（status / logs）+ 1 个 `file://`（readme）。每条 desktop
/// 渲染为 `"<uri>\n\n<body>"`（见 `smcp_computer::desktop::organize::render_desktop_item`）。
#[tokio::test]
#[ignore = "e2e: REL-01 v0.2.2 matrix; run via cargo test-e2e"]
async fn get_desktop_window_filter_and_size() {
    let td = TempDir::new().unwrap();
    let server = RelayServer::start().await;
    let computer = spawn_computer(&server.url(), OFFICE, COMPUTER, &td, None).await;
    let agent = agent_client(&server.url()).await;
    join(&agent, Role::Agent, OFFICE, AGENT).await;

    // GetDesktopRet.desktops 取为 Vec<String>（emit_call 已 flat 解包外层 args 数组）。
    let desktops = |body: &Value| -> Vec<String> {
        body["desktops"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };

    // (a) 无 window 过滤 → 仅 2 个 window://（status + logs）；file:// readme 被排除（desktop ≠ get_resources）。
    let req = GetDesktopReq {
        base: AgentCallData {
            agent: AGENT.into(),
            req_id: ReqId("desk-all".into()),
        },
        computer: COMPUTER.into(),
        desktop_size: None,
        window: None,
    };
    let body = emit_call(&agent, events::CLIENT_GET_DESKTOP, json!(req)).await;
    assert!(body.get("code").is_none(), "get_desktop 不应回错误: {body}");
    let all = desktops(&body);
    assert_eq!(
        all.len(),
        2,
        "应聚合 2 个 window://（status+logs）, got: {all:?}"
    );
    assert!(
        all.iter().any(|d| d.contains("System status: OK")),
        "应含 status 窗口正文: {all:?}"
    );
    assert!(
        all.iter().any(|d| d.contains("Log entry 1")),
        "应含 logs 窗口正文: {all:?}"
    );
    assert!(
        all.iter()
            .all(|d| !d.contains("This is the v022 readme content")),
        "desktop 仅聚合 window://，应排除 file:// readme: {all:?}"
    );
    assert!(
        all.iter().all(|d| !d.contains("file://")),
        "desktop 渲染不应含 file:// 资源: {all:?}"
    );

    // (b) 指定 window 精确匹配 → 仅返回该窗口（status），不混入 logs；回显被选 window URI。
    let req = GetDesktopReq {
        base: AgentCallData {
            agent: AGENT.into(),
            req_id: ReqId("desk-one".into()),
        },
        computer: COMPUTER.into(),
        desktop_size: None,
        window: Some("window://v022.mcp.test/status?priority=10".into()),
    };
    let body = emit_call(&agent, events::CLIENT_GET_DESKTOP, json!(req)).await;
    assert!(
        body.get("code").is_none(),
        "带 window 的 get_desktop 不应回错误: {body}"
    );
    let one = desktops(&body);
    assert_eq!(one.len(), 1, "指定 window 应仅返回 1 个窗口, got: {one:?}");
    assert!(
        one[0].contains("System status: OK"),
        "应为 status 窗口正文: {one:?}"
    );
    assert!(
        one[0].contains("window://v022.mcp.test/status"),
        "应回显被选 window URI: {one:?}"
    );
    assert!(!one[0].contains("Log entry"), "不应混入 logs 窗口: {one:?}");

    // (c) 指定不存在的 window URI（精确匹配，非前缀）→ 返回空 desktops（非错误）。
    let req = GetDesktopReq {
        base: AgentCallData {
            agent: AGENT.into(),
            req_id: ReqId("desk-none".into()),
        },
        computer: COMPUTER.into(),
        desktop_size: None,
        window: Some("window://v022.mcp.test/does-not-exist".into()),
    };
    let body = emit_call(&agent, events::CLIENT_GET_DESKTOP, json!(req)).await;
    assert!(
        body.get("code").is_none(),
        "不存在 window 不应回错误（应空列表）: {body}"
    );
    assert!(
        desktops(&body).is_empty(),
        "不存在的 window 应返回空 desktops: {body}"
    );

    // (d) desktop_size 全局截断 → 2 个窗口截断为 1。
    let req = GetDesktopReq {
        base: AgentCallData {
            agent: AGENT.into(),
            req_id: ReqId("desk-size".into()),
        },
        computer: COMPUTER.into(),
        desktop_size: Some(1),
        window: None,
    };
    let body = emit_call(&agent, events::CLIENT_GET_DESKTOP, json!(req)).await;
    assert!(
        body.get("code").is_none(),
        "带 size 的 get_desktop 不应回错误: {body}"
    );
    let capped = desktops(&body);
    assert_eq!(
        capped.len(),
        1,
        "desktop_size=1 应截断为 1 条, got: {capped:?}"
    );

    computer.shutdown().await.unwrap();
    agent.disconnect().await.unwrap();
    server.shutdown();
}

// ───────────────────────────── 4：SKILL 发现 / 读取 / SKILL channel ─────────────────────────────

/// 在 skill_home 写一个 user 源 SKILL（`<home>/user/<name>/SKILL.md`）。
fn write_user_skill(td: &TempDir, name: &str, description: &str, body: &str) {
    let dir = td.path().join("skills").join("user").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\n{body}"),
    )
    .unwrap();
}

/// 场景 4：`client:get_skills` 发现 user 源 SKILL（4 必选字段）；`client:get_skill` 取 SKILL.md
/// 入口回 frontmatter 剥离后的 body（文本内联，无 blob_handle）。
#[tokio::test]
#[ignore = "e2e: REL-01 v0.2.2 matrix; run via cargo test-e2e"]
async fn skills_discovery_and_read() {
    let td = TempDir::new().unwrap();
    write_user_skill(
        &td,
        "my-helper",
        "matrix helper skill",
        "HELPER-BODY-CONTENT",
    );

    let server = RelayServer::start().await;
    let computer = spawn_computer(&server.url(), OFFICE, COMPUTER, &td, None).await;
    let agent = agent_client(&server.url()).await;
    join(&agent, Role::Agent, OFFICE, AGENT).await;

    // get_skills
    let req = GetSkillsReq {
        base: AgentCallData {
            agent: AGENT.into(),
            req_id: ReqId("sk-1".into()),
        },
        computer: COMPUTER.into(),
    };
    let body = emit_call(&agent, events::CLIENT_GET_SKILLS, json!(req)).await;
    assert!(body.get("code").is_none(), "get_skills 不应回错误: {body}");
    let skills = body["skills"].as_array().cloned().unwrap_or_default();
    let helper = skills
        .iter()
        .find(|s| s["name"] == json!("my-helper"))
        .unwrap_or_else(|| panic!("应发现 user 源 SKILL my-helper, got: {skills:?}"));
    // 4 必选字段齐全 / required 4 present。
    assert_eq!(helper["source"], json!("user"));
    assert_eq!(helper["description"], json!("matrix helper skill"));
    assert!(
        helper["path"].as_str().is_some_and(|p| !p.is_empty()),
        "path 必选非空"
    );
    assert!(helper["name"].as_str().is_some());

    // get_skill（缺省 rel_path → SKILL.md 入口）→ frontmatter 剥离后 body 内联。
    let req = GetSkillReq {
        base: AgentCallData {
            agent: AGENT.into(),
            req_id: ReqId("sk-2".into()),
        },
        computer: COMPUTER.into(),
        name: "my-helper".into(),
        rel_path: None,
    };
    let body = emit_call(&agent, events::CLIENT_GET_SKILL, json!(req)).await;
    assert!(body.get("code").is_none(), "get_skill 不应回错误: {body}");
    let text = body["body"].as_str().unwrap_or("");
    assert!(
        text.contains("HELPER-BODY-CONTENT"),
        "应回 SKILL.md body, got: {body}"
    );
    assert!(
        !text.contains("name: my-helper"),
        "frontmatter 应被剥离, got: {text}"
    );
    assert!(
        body["blob_handle"].is_null(),
        "文本内联不应带 blob_handle: {body}"
    );

    computer.shutdown().await.unwrap();
    agent.disconnect().await.unwrap();
    server.shutdown();
}

/// 场景 4（续）：`client:get_skill` 的 `rel_path` 路径穿越（`../escape`）→ flat `ErrorPayload(4017)`
/// （SkillResourceNotAccessible），沙箱在 Computer 解析时强制。
#[tokio::test]
#[ignore = "e2e: REL-01 v0.2.2 matrix; run via cargo test-e2e"]
async fn skill_traversal_rejected_4017() {
    let td = TempDir::new().unwrap();
    write_user_skill(&td, "my-helper", "matrix helper skill", "BODY");

    let server = RelayServer::start().await;
    let computer = spawn_computer(&server.url(), OFFICE, COMPUTER, &td, None).await;
    let agent = agent_client(&server.url()).await;
    join(&agent, Role::Agent, OFFICE, AGENT).await;

    let req = GetSkillReq {
        base: AgentCallData {
            agent: AGENT.into(),
            req_id: ReqId("sk-esc".into()),
        },
        computer: COMPUTER.into(),
        name: "my-helper".into(),
        rel_path: Some("../../../etc/passwd".into()),
    };
    let body = emit_call(&agent, events::CLIENT_GET_SKILL, json!(req)).await;
    assert!(
        is_protocol_error_payload(&body),
        "穿越应回 flat ErrorPayload: {body}"
    );
    assert_eq!(body["code"], 4017, "路径穿越应 4017, got: {body}");

    computer.shutdown().await.unwrap();
    agent.disconnect().await.unwrap();
    server.shutdown();
}

// ──────────────────── 5 & 6：blob drain + tool_call 二进制 round-trip ────────────────────

/// 场景 5+6：超内联预算的二进制 tool_call 结果走 `_meta.a2c_blob_handle` 旁路，Agent 经
/// `client:get_blob` 分块拉取（offset/eof/sha256/total_size）重组，字节与 sha256 自证一致。
///
/// 注入极小 `inline_budget` 让 4 KiB image 必走 blob，并设小 `chunk_max_bytes` 触发多块重组。
#[tokio::test]
#[ignore = "e2e: REL-01 v0.2.2 matrix; run via cargo test-e2e"]
async fn tool_call_binary_blob_roundtrip() {
    use base64::Engine;
    use sha2::{Digest, Sha256};
    use smcp_computer::blob::thresholds::BlobThresholds;

    const IMG_BYTES: u64 = 4096;
    let td = TempDir::new().unwrap();
    let server = RelayServer::start().await;
    let thresholds = BlobThresholds {
        inline_budget: 256, // < 4 KiB → 必走 blob 旁路
        too_large_cap: 64 * 1024 * 1024,
        chunk_max_bytes: 1024, // 4 KiB / 1 KiB ≈ 4 块 → 验证分块重组
    };
    let computer = spawn_computer(&server.url(), OFFICE, COMPUTER, &td, Some(thresholds)).await;
    let agent = agent_client(&server.url()).await;
    join(&agent, Role::Agent, OFFICE, AGENT).await;

    // 1) tool_call gen_image → 结果含 _meta.a2c_blob_handle（data 被清空）。
    let req = ToolCallReq {
        base: AgentCallData {
            agent: AGENT.into(),
            req_id: ReqId("img-1".into()),
        },
        computer: COMPUTER.into(),
        tool_name: "gen_image".into(),
        params: json!({ "bytes": IMG_BYTES }),
        timeout: 30,
    };
    let body = emit_call(&agent, "client:tool_call", json!(req)).await;
    assert!(body.get("code").is_none(), "tool_call 不应回错误: {body}");
    let handle = deep_find(&body, "a2c_blob_handle")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("超预算二进制结果应带 a2c_blob_handle, got: {body}"))
        .to_string();

    // 2) client:get_blob 分块拉取 + 重组（`break` 带出末块 total/sha，避免哑初值）。
    let mut acc: Vec<u8> = Vec::new();
    let mut offset: u64 = 0;
    let mut chunks = 0;
    let (total, sha) = loop {
        let req = GetBlobReq {
            base: AgentCallData {
                agent: AGENT.into(),
                req_id: ReqId(format!("blob-{offset}")),
            },
            computer: COMPUTER.into(),
            blob_handle: handle.clone(),
            chunk_offset: Some(offset),
            max_chunk_bytes: None,
        };
        let r = emit_call(&agent, events::CLIENT_GET_BLOB, json!(req)).await;
        assert!(r.get("code").is_none(), "get_blob 不应回错误: {r}");
        let ret: GetBlobRet = serde_json::from_value(r).expect("GetBlobRet 解析");
        let chunk = base64::engine::general_purpose::STANDARD
            .decode(ret.blob.as_bytes())
            .expect("blob chunk base64");
        assert_eq!(ret.chunk_offset, offset, "chunk_offset 应回显请求 offset");
        acc.extend_from_slice(&chunk);
        offset += chunk.len() as u64;
        chunks += 1;
        if ret.eof {
            assert_eq!(offset, ret.total_size, "eof ⟺ offset == total_size");
            break (ret.total_size, ret.sha256);
        }
        assert!(chunks < 1000, "防御：分块循环未在合理块数内 eof");
    };

    // 3) 完整性：长度 / sha256 / 确定性字节模式三重自证。
    assert_eq!(total, IMG_BYTES, "总字节应为 {IMG_BYTES}");
    assert_eq!(acc.len() as u64, total);
    assert!(
        chunks >= 2,
        "小 chunk_max_bytes 下应多块（实测 {chunks} 块）"
    );
    let mut h = Sha256::new();
    h.update(&acc);
    assert_eq!(
        to_hex(&h.finalize()),
        sha,
        "重组 sha256 应与 GetBlobRet.sha256 一致"
    );
    for (i, b) in acc.iter().enumerate() {
        assert_eq!(*b, ((i * 31 + 7) & 0xff) as u8, "第 {i} 字节确定性模式不符");
    }

    computer.shutdown().await.unwrap();
    agent.disconnect().await.unwrap();
    server.shutdown();
}

// ───────────────────────────── 7：tool_call 取消 / 超时 meta ─────────────────────────────

/// 场景 7a：`server:tool_call_cancel` 的**服务端协议契约**——fire-and-forget（无 ack）+ Server
/// 广播 `notify:tool_call_cancel{agent, req_id}` 到房间（排除发起方），由独立观察者 Computer 收到。
///
/// 说明（端到端取消的传输边界）：Computer 侧的结果级 `meta.a2c_cancelled` 由 `call_tool_cancellable`
/// 的 `select!` 令牌竞速保证，并在 crate 级单测（manager `test_call_tool_cancellable_cancelled` /
/// INT-02 #70）覆盖。但 `tf-rust-socketio` 单连接的入站 handler **顺序派发**：在途 `client:tool_call`
/// 占用接收循环直至完成，故 `notify:tool_call_cancel` 要等其结束才被处理——协作式取消按契约「远端是否
/// 真正停止不保证」落到**下一次**调用。因此本矩阵在传输层断言**可达成**的协议契约（无 ack + 广播），
/// 结果级 `a2c_cancelled` 归 crate 级；内部超时态见 [`tool_call_timeout_marks_meta`]（端到端可达成）。
#[tokio::test]
#[ignore = "e2e: REL-01 v0.2.2 matrix; run via cargo test-e2e"]
async fn tool_call_cancel_fireforget_and_broadcast() {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use futures_util::FutureExt;
    use tf_rust_socketio::asynchronous::ClientBuilder;
    use tf_rust_socketio::{Payload, TransportType};

    let td = TempDir::new().unwrap();
    let server = RelayServer::start().await;
    let computer = spawn_computer(&server.url(), OFFICE, COMPUTER, &td, None).await;

    // 观察者 Computer：捕获 notify:tool_call_cancel 广播载体。房间每 office 仅允许 1 个 Agent（可多
    // Computer），且 `socket.to(office)` 广播**排除发起方**，故以 Computer 角色作独立观察者接广播。
    let seen: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let seen_cb = seen.clone();
    let observer = ClientBuilder::new(server.url())
        .transport_type(TransportType::Websocket)
        .namespace(NS)
        .auth(serde_json::json!({"token": SECRET}))
        .on(events::NOTIFY_TOOL_CALL_CANCEL, move |p: Payload, _c| {
            let seen = seen_cb.clone();
            async move {
                if let Payload::Text(mut vals, _) = p {
                    *seen.lock().unwrap() = Some(vals.pop().unwrap_or(Value::Null));
                }
            }
            .boxed()
        })
        .connect()
        .await
        .expect("observer connect");
    tokio::time::sleep(Duration::from_millis(200)).await;
    join(&observer, Role::Computer, OFFICE, "computer-observer").await;

    let agent = agent_client(&server.url()).await;
    join(&agent, Role::Agent, OFFICE, AGENT).await;

    // 起一个在途 sleep 工具（fire-and-forget；只需让其在 Computer 侧在途）。
    let tool_req = ToolCallReq {
        base: AgentCallData {
            agent: AGENT.into(),
            req_id: ReqId("cancel-req".into()),
        },
        computer: COMPUTER.into(),
        tool_name: "sleep".into(),
        params: json!({ "ms": 1500 }),
        timeout: 30,
    };
    agent
        .emit("client:tool_call", json!(tool_req))
        .await
        .expect("tool_call emit");
    tokio::time::sleep(Duration::from_millis(400)).await;

    // 1) server:tool_call_cancel 是 fire-and-forget：经 emit_with_ack 探测 → **无 ack**（超时）。
    let (ctx, crx) = tokio::sync::oneshot::channel::<Value>();
    let ctx = Arc::new(tokio::sync::Mutex::new(Some(ctx)));
    agent
        .emit_with_ack(
            events::SERVER_TOOL_CALL_CANCEL,
            json!({ "agent": AGENT, "req_id": "cancel-req" }),
            Duration::from_secs(1),
            move |p: Payload, _c| {
                let ctx = ctx.clone();
                async move {
                    let v = match p {
                        Payload::Text(mut vals, _) => vals.pop().unwrap_or(Value::Null),
                        _ => Value::Null,
                    };
                    if let Some(tx) = ctx.lock().await.take() {
                        let _ = tx.send(v);
                    }
                }
                .boxed()
            },
        )
        .await
        .expect("cancel emit");
    let acked = tokio::time::timeout(Duration::from_millis(1500), crx).await;
    assert!(
        acked.is_err(),
        "server:tool_call_cancel 应 fire-and-forget（无 ack）"
    );

    // 2) 观察者 Computer 应收到 notify:tool_call_cancel 广播，载体回显 {agent, req_id}。
    tokio::time::sleep(Duration::from_millis(400)).await;
    let payload = seen
        .lock()
        .unwrap()
        .clone()
        .expect("观察者应收到 notify:tool_call_cancel 广播");
    assert_eq!(
        deep_find(&payload, "req_id").and_then(Value::as_str),
        Some("cancel-req"),
        "广播应回显 req_id, got: {payload}"
    );
    assert_eq!(
        deep_find(&payload, "agent").and_then(Value::as_str),
        Some(AGENT)
    );

    computer.shutdown().await.unwrap();
    agent.disconnect().await.unwrap();
    observer.disconnect().await.unwrap();
    server.shutdown();
}

/// 场景 7b：tool_call 内部**超时**三态——Computer 端 `execute_tool_cancellable` 的 `select!` 在 1s
/// 超时分支胜出（工具 sleep 4000ms），结果级 meta `a2c_timeout=true`（与取消/失败态区分）。超时是
/// Computer 内部计时、**不**依赖外部 cancel 事件，故端到端可达成（区别于 7a 的外部取消传输边界）。
#[tokio::test]
#[ignore = "e2e: REL-01 v0.2.2 matrix; run via cargo test-e2e"]
async fn tool_call_timeout_marks_meta() {
    use std::time::{Duration, Instant};

    let td = TempDir::new().unwrap();
    let server = RelayServer::start().await;
    let computer = spawn_computer(&server.url(), OFFICE, COMPUTER, &td, None).await;
    let agent = agent_client(&server.url()).await;
    join(&agent, Role::Agent, OFFICE, AGENT).await;

    let req = ToolCallReq {
        base: AgentCallData {
            agent: AGENT.into(),
            req_id: ReqId("timeout-req".into()),
        },
        computer: COMPUTER.into(),
        tool_name: "sleep".into(),
        params: json!({ "ms": 4000 }),
        timeout: 1, // 1s 工具超时 < 4s sleep → 超时分支胜出
    };
    let started = Instant::now();
    let body = emit_call(&agent, "client:tool_call", json!(req)).await;
    let elapsed = started.elapsed();

    assert!(
        body.get("code").is_none(),
        "tool_call 不应回协议错误: {body}"
    );
    let timed_out = deep_find(&body, "a2c_timeout")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    assert!(timed_out, "超时结果 meta 应 a2c_timeout=true, got: {body}");
    // 1s 超时态远早于 4s 自然完成（证明超时分支胜出而非自然返回）。
    assert!(
        elapsed < Duration::from_millis(3500),
        "超时应早于工具 4s 自然返回（实测 {elapsed:?}）"
    );

    computer.shutdown().await.unwrap();
    agent.disconnect().await.unwrap();
    server.shutdown();
}

// ──────────────── 8 & 9：in-flight disconnect 容错 + flat ErrorPayload ────────────────

/// 场景 9：目标 Computer 未命中 → 经 ack 投递 flat `ErrorPayload(404)`（顶层 code/message +
/// `details.computer_name`，**无** `{"Err":..}` envelope），对齐 SRV-01 #47。
#[tokio::test]
#[ignore = "e2e: REL-01 v0.2.2 matrix; run via cargo test-e2e"]
async fn tool_call_unknown_computer_flat_404() {
    let server = RelayServer::start().await;
    let agent = agent_client(&server.url()).await;
    join(&agent, Role::Agent, OFFICE, AGENT).await;

    let req = ToolCallReq {
        base: AgentCallData {
            agent: AGENT.into(),
            req_id: ReqId("nf-1".into()),
        },
        computer: "nonexistent-computer".into(),
        tool_name: "echo".into(),
        params: json!({ "message": "hi" }),
        timeout: 5,
    };
    let body = emit_call(&agent, "client:tool_call", json!(req)).await;
    assert!(
        is_protocol_error_payload(&body),
        "应回 flat ErrorPayload: {body}"
    );
    assert_eq!(
        body["code"], 404,
        "未命中 Computer 应 flat 404, got: {body}"
    );
    assert_eq!(
        body["details"]["computer_name"],
        json!("nonexistent-computer")
    );
    assert!(body.get("Err").is_none(), "禁止 {{\"Err\":..}} envelope");

    agent.disconnect().await.unwrap();
    server.shutdown();
}

/// 场景 8：发起方 Agent **在途断连** → Server 静默丢弃在途请求、**MUST NOT** crash/hang；
/// 后续全新 Agent 仍可正常 join + list_room（证明注册表无死锁、无 panic 波及）。
///
/// 裸客户端无法在 `on` 回调里回 ACK，故用「不应答的裸 Computer + 发起后立即断连的 Agent」构造在途，
/// 再以健康检查证明 Server 存活——与 server-core `test_originator_disconnect_in_flight_no_crash` 同形。
#[tokio::test]
#[ignore = "e2e: REL-01 v0.2.2 matrix; run via cargo test-e2e"]
async fn originator_disconnect_server_survives() {
    use std::time::Duration;

    let server = RelayServer::start().await;

    // 不应答的裸 Computer（永不回 ack → relay 进入等待）。
    let dead_computer = agent_client(&server.url()).await;
    join(&dead_computer, Role::Computer, OFFICE, COMPUTER).await;

    let agent = agent_client(&server.url()).await;
    join(&agent, Role::Agent, OFFICE, AGENT).await;

    let req = ToolCallReq {
        base: AgentCallData {
            agent: AGENT.into(),
            req_id: ReqId("orig-disc".into()),
        },
        computer: COMPUTER.into(),
        tool_name: "echo".into(),
        params: json!({ "message": "hi" }),
        timeout: 5,
    };
    // fire-and-forget：发出后立即断连发起方。
    agent
        .emit("client:tool_call", json!(req))
        .await
        .expect("emit tool_call");
    tokio::time::sleep(Duration::from_millis(150)).await;
    agent.disconnect().await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    dead_computer.disconnect().await.unwrap();

    // 健康检查：全新 Agent 在新 office 仍可 join + list_room。
    tokio::time::sleep(Duration::from_millis(200)).await;
    let agent2 = agent_client(&server.url()).await;
    join(&agent2, Role::Agent, "office-health", "agent-health").await;
    let list = emit_call(
        &agent2,
        "server:list_room",
        json!({ "agent": "agent-health", "req_id": "health", "office_id": "office-health" }),
    )
    .await;
    assert!(
        list.get("sessions").is_some(),
        "断连后 server 应仍正常响应 list_room: {list}"
    );

    agent2.disconnect().await.unwrap();
    server.shutdown();
}

// ──────────── 12：高层 Agent SDK 查询端到端（#82 回归护栏）/ high-level Agent SDK query e2e ────────────

/// 场景 12：真实 [`smcp_agent::AsyncSmcpAgent`] 走**高层查询方法**（`get_tools` + `list_room`）端到端
/// 必须返回结果，而非 `内部错误: Missing req_id in response`（#82）。
///
/// 此前矩阵的 Agent 侧一律用裸 `tf-rust-socketio` 客户端 + `flat()`（见 [`harness::emit_call`]）绕开
/// 高层 SDK——`flat()` 恰好替调用方拆了 socket.io ack 外层 args 数组，**掩盖**了 `transport.call` 缺
/// 同等拆封的根因 bug（ack 数据恒以 `[<value>]` 投递，`ensure_req_id` 落在数组上 → `Missing req_id`）。
/// 本测试改用**真实 Agent SDK** 驱动 `transport.call` → `ensure_req_id` 全链，端到端护住根因修复
/// （transport `extract_ack_value`/`flatten_ack_arg`）。覆盖两条结构不同的 ack 生产路径：
/// - `get_tools`：**Computer 中继**路径（Server 转发给 Computer 回包）；
/// - `list_room`：**Server 直答**路径（Server 直接回包，不经 Computer）。
/// 二者同走 `transport.call` 拆封，修复前均立即 `Err`，修复后均正确解析。
#[tokio::test]
#[ignore = "e2e: REL-01 v0.2.2 matrix; run via cargo test-e2e"]
async fn high_level_agent_query_methods_unwrap_ack() {
    use smcp_agent::{AsyncSmcpAgent, DefaultAuthProvider, SmcpAgentConfig};

    let td = TempDir::new().unwrap();
    let server = RelayServer::start().await;
    let computer = spawn_computer(&server.url(), OFFICE, COMPUTER, &td, None).await;

    // 真实 Agent SDK（非裸客户端）：#86 起 with_api_key 走 Socket.IO auth dict 默认 `token` 字段，
    // 对齐 RelayServer 的 DefaultAuthenticationProvider(SECRET)；connect → join_office → 走高层查询方法。
    let auth = DefaultAuthProvider::new(AGENT.to_string(), OFFICE.to_string())
        .with_api_key(SECRET.to_string());
    let mut agent = AsyncSmcpAgent::new(auth, SmcpAgentConfig::default());
    agent.connect(&server.url()).await.expect("agent connect");
    agent.join_office(AGENT).await.expect("agent join_office");
    // 等 agent join 在房间内落定（与 spawn_computer 同款 settle）。
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;

    // (a) get_tools（Computer 中继路径）。修复前：立即 Err(internal "Missing req_id")；修复后：返回工具。
    let tools = agent
        .get_tools(COMPUTER)
        .await
        .expect("get_tools 必须成功（#82：拆 socket.io ack 外层 args 数组）");
    assert!(
        tools.iter().any(|t| t.name == "echo"),
        "应返回 v022 MCP 的 echo 工具，实得: {:?}",
        tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>()
    );

    // (b) list_room（Server 直答路径，与 get_tools 的中继路径结构不同，此前同样全坏）：经同一
    // transport.call ack 拆封 + ensure_req_id 解析会话列表（应含本 Agent 自身会话）。
    let sessions = agent
        .list_room(OFFICE)
        .await
        .expect("list_room 必须成功（#82：Server 直答路径同走 ack 拆封）");
    assert!(
        sessions.iter().any(|s| s.name == AGENT),
        "list_room 应含本 Agent 会话，实得: {:?}",
        sessions.iter().map(|s| s.name.as_str()).collect::<Vec<_>>()
    );

    let _ = agent.leave_office().await;
    computer.shutdown().await.unwrap();
    server.shutdown();
}

// ──────────────────────────── 10 & 11：治理层覆盖说明 / governance coverage ────────────────────────────
//
// 场景 10（marketplace strict 冲突）与 11（settings 5 级 scope 合并 / plugin installer install-enable-
// disable / mcp_config 批准门控）是**治理层**能力，其交互面是 CLI / 本地文件物化，**不经** socket.io
// `client:*` 事件流，无法（也不应）在本 socket.io e2e 矩阵里驱动——这与 Python 参考一致：Python 在
// `cli_marketplace` / `cli_plugin_settings` / `installer` / `mcp_config` 等 CLI/unit 套件而非 e2e
// socket.io 层测治理（issue 正文「等价」即指此）。Rust 侧对应 crate 级覆盖：
//   - marketplace strict        → crates/smcp-computer/src/skills/staging.rs（strict staging + 冲突）
//                                 + crates/smcp-computer/src/settings/installer.rs
//   - settings 5 级 scope 合并   → crates/smcp-computer/src/settings/*（scope reconcile 单测）
//   - plugin installer 生命周期  → crates/smcp-computer/src/inputs/plugin_pool.rs + settings/installer.rs
//   - mcp_config 批准门控        → crates/smcp-computer/tests/handshake_config_test.rs + settings 套件
// 本矩阵聚焦协议线（socket.io + stdio MCP）端到端，治理层端到端归 CLI 层 e2e（CLI Epic #48/#51/#54）。
