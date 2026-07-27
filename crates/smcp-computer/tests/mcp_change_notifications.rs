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

use smcp_computer::mcp_clients::model::{
    MCPServerConfig, McpChangeKind, McpServerNotification, StdioServerConfig, StdioServerParameters,
};
use smcp_computer::mcp_clients::MCPServerManager;
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
    assert!(
        names0.contains(&"set_phase".to_string()),
        "初始应有 set_phase"
    );
    assert!(
        !names0.contains(&"dyn_tool".to_string()),
        "phase 0 不应有 dyn_tool，实得 {names0:?}"
    );

    // 运行期切到 phase 1：server 新增 dyn_tool 并主动发 tools/list_changed + resources/list_changed。
    manager
        .execute_tool("set_phase", serde_json::json!({ "phase": 1 }), None)
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
        !names_before.contains(&"dyn_tool".to_string()),
        "未刷新前 dyn_tool 不应可见（复现坑1），实得 {names_before:?}"
    );

    // 坑1 修复：刷新后（消费者任务在 tools/list_changed 时所做的动作）dyn_tool 浮现。
    manager
        .refresh_tool_mapping()
        .await
        .expect("refresh_tool_mapping failed");
    let names_after = tool_names(&manager.list_available_tools().await);
    assert!(
        names_after.contains(&"dyn_tool".to_string()),
        "刷新后 dyn_tool 应可见（坑1 修复），实得 {names_after:?}"
    );

    let _ = manager.stop_all().await;
}
