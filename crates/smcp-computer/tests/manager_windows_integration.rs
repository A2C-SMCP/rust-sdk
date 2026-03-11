//! Integration tests for MCPServerManager window resource aggregation.
//!
//! Requires Node.js installed. Run with:
//! ```
//! cargo test --package smcp-computer --test manager_windows_integration -- --ignored --nocapture
//! ```

use smcp_computer::mcp_clients::model::{
    make_resource, MCPServerConfig, ResourceContents, StdioServerConfig, StdioServerParameters,
};
use smcp_computer::mcp_clients::MCPServerManager;
use std::collections::HashMap;
use std::time::Duration;

/// Path to the echo MCP server relative to workspace root
fn echo_server_path() -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    format!("{}/../../tests/echo-mcp-server/index.js", manifest_dir)
}

fn stdio_config(name: &str) -> MCPServerConfig {
    MCPServerConfig::Stdio(StdioServerConfig {
        name: name.to_string(),
        disabled: false,
        forbidden_tools: vec![],
        tool_meta: HashMap::new(),
        default_tool_meta: None,
        vrl: None,
        server_parameters: StdioServerParameters {
            command: "node".to_string(),
            args: vec![echo_server_path()],
            env: HashMap::new(),
            cwd: None,
        },
    })
}

async fn setup_manager() -> MCPServerManager {
    let manager = MCPServerManager::new();
    manager
        .initialize(vec![stdio_config("echo-server")])
        .await
        .expect("initialize failed");
    let result = tokio::time::timeout(Duration::from_secs(15), manager.start_all()).await;
    result
        .expect("start_all timed out")
        .expect("start_all failed");
    manager
}

#[tokio::test]
#[ignore] // 需要 Node.js 运行时；手动验证: cargo test -p smcp-computer --test manager_windows_integration -- --ignored --nocapture
async fn test_manager_list_all_windows() {
    let manager = setup_manager().await;

    let windows = manager.list_all_windows(None).await;
    assert!(!windows.is_empty(), "expected at least one window resource");

    // All returned resources should have window:// URI
    for (server_name, resource) in &windows {
        assert_eq!(server_name, "echo-server");
        assert!(
            resource.uri.starts_with("window://"),
            "expected window:// URI, got: {}",
            resource.uri
        );
    }

    // Should have exactly 2 window resources (status + logs), not the file:// one
    assert_eq!(windows.len(), 2, "expected 2 window resources");

    manager.stop_all().await.ok();
}

#[tokio::test]
#[ignore] // 需要 Node.js 运行时；手动验证: cargo test -p smcp-computer --test manager_windows_integration -- --ignored --nocapture
async fn test_manager_list_all_windows_with_filter() {
    let manager = setup_manager().await;

    let status_uri = "window://echo.mcp.test/status?priority=10";
    let windows = manager.list_all_windows(Some(status_uri)).await;
    assert_eq!(windows.len(), 1, "expected exactly 1 filtered result");
    assert_eq!(windows[0].1.uri.as_str(), status_uri);

    // Non-matching URI should return empty
    let windows = manager.list_all_windows(Some("window://nonexistent")).await;
    assert!(windows.is_empty(), "expected empty for non-matching URI");

    manager.stop_all().await.ok();
}

#[tokio::test]
#[ignore] // 需要 Node.js 运行时；手动验证: cargo test -p smcp-computer --test manager_windows_integration -- --ignored --nocapture
async fn test_manager_get_windows_details() {
    let manager = setup_manager().await;

    let details = manager.get_windows_details(None).await;
    assert!(
        !details.is_empty(),
        "expected at least one window with details"
    );

    for (server_name, resource, detail) in &details {
        assert_eq!(server_name, "echo-server");
        assert!(resource.uri.starts_with("window://"));
        assert!(
            !detail.contents.is_empty(),
            "expected non-empty contents for {}",
            resource.uri
        );
    }

    assert_eq!(details.len(), 2, "expected 2 window details");

    // Verify actual content text to ensure end-to-end JSON serialization correctness
    let status_detail = details
        .iter()
        .find(|(_, r, _)| r.uri.as_str() == "window://echo.mcp.test/status?priority=10")
        .expect("expected status window in details");
    let status_content = &status_detail.2.contents[0];
    match status_content {
        ResourceContents::TextResourceContents { text, .. } => {
            assert_eq!(text, "System status: OK");
        }
        other => panic!("expected TextResourceContents, got: {:?}", other),
    }

    manager.stop_all().await.ok();
}

#[tokio::test]
#[ignore] // 需要 Node.js 运行时；手动验证: cargo test -p smcp-computer --test manager_windows_integration -- --ignored --nocapture
async fn test_manager_get_window_detail_single() {
    let manager = setup_manager().await;

    let windows = manager.list_all_windows(None).await;
    assert!(!windows.is_empty());

    let (server_name, resource) = &windows[0];
    let detail = manager
        .get_window_detail(server_name, resource.clone())
        .await
        .expect("get_window_detail failed");
    assert!(!detail.contents.is_empty(), "expected non-empty contents");

    manager.stop_all().await.ok();
}

#[tokio::test]
#[ignore] // 需要 Node.js 运行时；手动验证: cargo test -p smcp-computer --test manager_windows_integration -- --ignored --nocapture
async fn test_manager_get_window_detail_unknown_server() {
    let manager = setup_manager().await;

    let resource = make_resource(
        "window://echo.mcp.test/status?priority=10",
        "Status Window",
        None,
        Some("text/plain".into()),
    );

    let result = manager
        .get_window_detail("nonexistent-server", resource)
        .await;
    assert!(result.is_err(), "expected error for unknown server");

    manager.stop_all().await.ok();
}
