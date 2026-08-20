//! #106：MCP 运行期变化通知——生产者转发 + 「刷新后新增工具可见」集成测试。
//!
//! 用真实的**可变工具集** stdio MCP 子进程（`tests/mutable-mcp-server`）驱动：调 `set_phase(1)` 令 server
//! 运行期新增 `dyn_tool` 并发出 `notifications/tools/list_changed` + `notifications/resources/list_changed`。
//! 验证两件事：
//!   1. 生产者转发：注入的 change channel 收到 `ToolListChanged` 与 `ResourceListChanged`
//!      （证明 stdio `A2cClientHandler` 接线成功；该 handler 与 HTTP 客户端共享，故同时覆盖 HTTP 转发通路）。
//!   2. 坑1 修复：`list_available_tools` 迭代 `tool_mapping`——**未刷新前**运行期新增的 `dyn_tool` 不可见；
//!      调 `refresh_tool_mapping`（即消费者任务在 tools/list_changed 时所做）**之后**才浮现。
//!
//! 需要 Node.js；`#[ignore]`，手动/CI 经 `--ignored` 运行。
//! cargo test -p smcp-computer --test mcp_change_notifications -- --ignored --nocapture

use std::collections::HashMap;
use std::time::Duration;

use smcp_computer::computer::{Computer, SilentSession};
use smcp_computer::mcp_clients::model::{
    MCPServerConfig, McpChangeKind, McpServerNotification, StdioServerConfig, StdioServerParameters,
};
use smcp_computer::mcp_clients::MCPServerManager;
use smcp_computer::ComputerEvent;
use tokio::sync::mpsc;
use tokio::time::Instant;

fn mutable_server_path() -> String {
    format!(
        "{}/../../tests/mutable-mcp-server/index.js",
        env!("CARGO_MANIFEST_DIR")
    )
}

fn stdio_config(name: &str) -> MCPServerConfig {
    MCPServerConfig::Stdio(StdioServerConfig::new(
        name,
        StdioServerParameters {
            command: "node".to_string(),
            args: vec![mutable_server_path()],
            env: HashMap::new(),
            cwd: None,
        },
    ))
}

fn tool_names(tools: &[smcp_computer::mcp_clients::model::Tool]) -> Vec<String> {
    tools.iter().map(|t| t.name.to_string()).collect()
}

/// 轮询 change channel 直到同时收到 ToolListChanged 与 ResourceListChanged，或超时。
async fn wait_for_tool_and_resource(
    rx: &mut mpsc::UnboundedReceiver<McpServerNotification>,
) -> (bool, bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let (mut tools, mut resources) = (false, false);
    while !(tools && resources) {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(n)) => match n.kind {
                McpChangeKind::ToolListChanged => tools = true,
                McpChangeKind::ResourceListChanged => resources = true,
                McpChangeKind::ResourceUpdated { .. } => {}
            },
            _ => break,
        }
    }
    (tools, resources)
}

#[tokio::test]
#[ignore] // 需要 Node.js 运行时
async fn tools_list_changed_forwarded_and_refresh_reveals_new_tool() {
    let (tx, mut rx) = mpsc::unbounded_channel();

    let manager = MCPServerManager::new();
    // 关键：change sender 必须在 client 启动前注入，start_all 据此为客户端携带 ClientNotifyCtx。
    manager.set_change_sender(tx).await;
    manager
        .initialize(vec![stdio_config("mutable")])
        .await
        .expect("initialize failed");
    tokio::time::timeout(Duration::from_secs(15), manager.start_all())
        .await
        .expect("start_all timed out")
        .expect("start_all failed");

    // 初始（phase 0）：仅 set_phase，无 dyn_tool。
    let names0 = tool_names(&manager.list_available_tools().await);
    let set_phase = names0
        .iter()
        .find(|name| name.ends_with("__set_phase"))
        .expect("初始应有 BundleID 前缀化的 set_phase")
        .clone();
    assert!(
        !names0.iter().any(|name| name.ends_with("__dyn_tool")),
        "phase 0 不应有 dyn_tool，实得 {names0:?}"
    );

    // 运行期切到 phase 1：server 新增 dyn_tool 并主动发 tools/list_changed + resources/list_changed。
    manager
        .execute_tool(&set_phase, serde_json::json!({ "phase": 1 }), None)
        .await
        .expect("set_phase call failed");

    // 生产者转发验证：change channel 收到两类通知。
    let (got_tools, got_resources) = wait_for_tool_and_resource(&mut rx).await;
    assert!(
        got_tools,
        "未从 change channel 收到 ToolListChanged（stdio handler 未转发？）"
    );
    assert!(
        got_resources,
        "未从 change channel 收到 ResourceListChanged（stdio handler 未转发？）"
    );

    // 坑1 复现：未刷新 tool_mapping 前，运行期新增的 dyn_tool 在 list_available_tools 不可见。
    let names_before = tool_names(&manager.list_available_tools().await);
    assert!(
        !names_before.iter().any(|name| name.ends_with("__dyn_tool")),
        "未刷新前 dyn_tool 不应可见（复现坑1），实得 {names_before:?}"
    );

    // 坑1 修复：刷新后（消费者任务在 tools/list_changed 时所做的动作）dyn_tool 浮现。
    manager
        .refresh_tool_mapping()
        .await
        .expect("refresh_tool_mapping failed");
    let names_after = tool_names(&manager.list_available_tools().await);
    assert!(
        names_after.iter().any(|name| name.ends_with("__dyn_tool")),
        "刷新后 dyn_tool 应可见（坑1 修复），实得 {names_after:?}"
    );

    let _ = manager.stop_all().await;
}

fn projection_has_property(
    tools: &[smcp_computer::mcp_clients::model::Tool],
    tool_suffix: &str,
    property: &str,
) -> bool {
    tools.iter().any(|tool| {
        tool.name.as_ref().ends_with(tool_suffix)
            && serde_json::to_value(tool)
                .ok()
                .and_then(|value| {
                    value
                        .pointer(&format!("/inputSchema/properties/{property}"))
                        .cloned()
                })
                .is_some()
    })
}

async fn next_capability_revision(
    events: &mut tokio::sync::broadcast::Receiver<ComputerEvent>,
) -> u64 {
    let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("timed out waiting for runtime event")
        .expect("runtime event channel closed");
    match event {
        ComputerEvent::CapabilityRevisionBumped { revision } => revision,
        other => panic!("expected CapabilityRevisionBumped, got {other:?}"),
    }
}

/// #196：真实 stdio `tools/list_changed` 必须在工具投影真实变化后推进本地 runtime revision/event；
/// schema-only 变化同样属于能力变化，重复相同投影则不得误报。
#[tokio::test]
#[ignore] // 需要 Node.js 运行时
async fn tools_list_changed_bumps_runtime_only_when_projection_changes() {
    let temp = tempfile::TempDir::new().expect("temporary isolation root");
    let mut servers = HashMap::new();
    servers.insert("mutable".to_string(), stdio_config("mutable"));
    let env: HashMap<String, String> = std::iter::once((
        "XDG_CONFIG_HOME".to_string(),
        temp.path().join("xdg").to_string_lossy().into_owned(),
    ))
    .collect();
    let computer = Computer::new(
        "issue-196",
        SilentSession::new("test"),
        None,
        Some(servers),
        false,
        false,
    )
    .with_skill_home(temp.path().join("skills"))
    .with_blob_cache_root(temp.path().join("blob"))
    .with_config_dir(temp.path().join("project"))
    .with_config_env(env)
    .with_confirm_callback(|_, _, _, _| true);

    computer.boot_up().await.expect("boot");
    computer
        .start_all_mcp_clients()
        .await
        .expect("start mutable MCP server");
    let initial = computer.status().await;
    assert_eq!(initial.tools, 1);
    let set_phase = computer
        .get_available_tools()
        .await
        .expect("initial tools")
        .into_iter()
        .find(|tool| tool.name.as_ref().ends_with("__set_phase"))
        .expect("set_phase tool")
        .name
        .to_string();
    let mut events = computer.subscribe_events();

    computer
        .execute_tool(
            "add",
            &set_phase,
            serde_json::json!({"phase": 1}),
            Some(5.0),
        )
        .await
        .expect("set phase 1");
    let add_revision = next_capability_revision(&mut events).await;
    let after_add = computer.status().await;
    assert_eq!(add_revision, initial.capability_revision + 1);
    assert_eq!(after_add.capability_revision, add_revision);
    assert_eq!(after_add.tools, 2, "event must follow route commit");
    assert!(projection_has_property(
        &computer.get_available_tools().await.expect("phase 1 tools"),
        "__dyn_tool",
        "x"
    ));

    computer
        .execute_tool(
            "schema",
            &set_phase,
            serde_json::json!({"phase": 2}),
            Some(5.0),
        )
        .await
        .expect("set phase 2");
    let schema_revision = next_capability_revision(&mut events).await;
    let after_schema = computer.status().await;
    assert_eq!(schema_revision, add_revision + 1);
    assert_eq!(after_schema.capability_revision, schema_revision);
    assert_eq!(after_schema.tools, 2, "schema-only change keeps tool count");
    assert!(projection_has_property(
        &computer.get_available_tools().await.expect("phase 2 tools"),
        "__dyn_tool",
        "y"
    ));

    computer
        .execute_tool(
            "same",
            &set_phase,
            serde_json::json!({"phase": 2}),
            Some(5.0),
        )
        .await
        .expect("repeat phase 2");
    assert!(
        tokio::time::timeout(Duration::from_millis(500), events.recv())
            .await
            .is_err(),
        "identical projection must not publish a capability event"
    );
    assert_eq!(computer.capability_revision(), schema_revision);

    computer
        .execute_tool(
            "remove",
            &set_phase,
            serde_json::json!({"phase": 3}),
            Some(5.0),
        )
        .await
        .expect("set phase 3");
    let remove_revision = next_capability_revision(&mut events).await;
    let after_remove = computer.status().await;
    assert_eq!(remove_revision, schema_revision + 1);
    assert_eq!(after_remove.capability_revision, remove_revision);
    assert_eq!(after_remove.tools, 1, "event must expose removed route");

    computer.shutdown().await.expect("shutdown");
}
