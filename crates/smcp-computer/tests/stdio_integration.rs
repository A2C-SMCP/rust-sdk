//! Integration tests for StdioMCPClient with a real MCP server.
//!
//! Requires Node.js installed. Run with:
//! ```
//! cargo test --package smcp-computer --test stdio_integration -- --ignored
//! ```

use smcp_computer::mcp_clients::model::{
    ClientState, MCPClientError, MCPClientProtocol, StdioServerParameters,
};
use smcp_computer::mcp_clients::stdio_client::StdioMCPClient;
use std::collections::HashMap;
use std::time::Duration;

/// Path to the echo MCP server relative to workspace root
fn echo_server_path() -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    format!("{}/../../tests/echo-mcp-server/index.js", manifest_dir)
}

/// Path to the stderr-flood MCP server relative to workspace root
fn stderr_flood_server_path() -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    format!(
        "{}/../../tests/stderr-flood-mcp-server/index.js",
        manifest_dir
    )
}

#[tokio::test]
#[ignore] // Requires Node.js
async fn test_stdio_client_lifecycle() {
    let params = StdioServerParameters {
        command: "node".to_string(),
        args: vec![echo_server_path()],
        env: HashMap::new(),
        cwd: None,
    };

    let client = StdioMCPClient::new(params);
    assert_eq!(client.state(), ClientState::Initialized);

    // Connect with safety timeout
    let connect_result = tokio::time::timeout(Duration::from_secs(15), client.connect()).await;
    let connect_result = connect_result.expect("connect timed out (outer)");
    connect_result.expect("connect failed");
    assert_eq!(client.state(), ClientState::Connected);

    // List tools
    let tools = client.list_tools().await.expect("list_tools failed");
    assert!(!tools.is_empty(), "expected at least one tool");
    let echo_tool = tools.iter().find(|t| t.name == "echo");
    assert!(echo_tool.is_some(), "expected 'echo' tool");

    // Call echo tool
    let result = client
        .call_tool("echo", serde_json::json!({"message": "hello world"}))
        .await
        .expect("call_tool failed");

    let content = &result.content;
    assert!(!content.is_empty(), "expected non-empty content");

    // Disconnect
    client.disconnect().await.expect("disconnect failed");
    assert_eq!(client.state(), ClientState::Disconnected);
}

#[tokio::test]
#[ignore] // Requires Node.js
async fn test_stdio_connect_timeout_with_bad_server() {
    // Use `cat` which reads stdin but never writes MCP responses
    let params = StdioServerParameters {
        command: "cat".to_string(),
        args: vec![],
        env: HashMap::new(),
        cwd: None,
    };

    let client = StdioMCPClient::new(params);

    let result = tokio::time::timeout(Duration::from_secs(35), client.connect()).await;

    match result {
        Ok(Err(MCPClientError::TimeoutError(_))) => {
            // Expected: inner timeout fires
        }
        Ok(Err(e)) => {
            // Also acceptable: some other connection error
            eprintln!("Got non-timeout error (acceptable): {}", e);
        }
        Ok(Ok(())) => panic!("expected connect to fail with cat"),
        Err(_) => panic!("outer timeout fired, inner timeout should have fired first"),
    }
}

/// Regression test for https://github.com/A2C-SMCP/rust-sdk/issues/10
///
/// Verifies that a child process writing >128 KB to stderr does NOT deadlock
/// the connect() call. Before the fix, the pipe buffer (typically 64 KB) would
/// fill up, blocking the child's write() syscall and stalling the entire
/// async event loop.
#[tokio::test]
#[ignore] // Requires Node.js
async fn test_stdio_stderr_flood_no_deadlock() {
    let params = StdioServerParameters {
        command: "node".to_string(),
        args: vec![stderr_flood_server_path()],
        env: HashMap::new(),
        cwd: None,
    };

    let client = StdioMCPClient::new(params);
    assert_eq!(client.state(), ClientState::Initialized);

    // This would hang (and eventually timeout) before the fix because the
    // child blocks on stderr write and never responds to MCP initialize.
    let connect_result = tokio::time::timeout(Duration::from_secs(15), client.connect()).await;
    let connect_result =
        connect_result.expect("connect timed out — stderr pipe is likely blocked (Issue #10)");
    connect_result.expect("connect failed");
    assert_eq!(client.state(), ClientState::Connected);

    // Verify the server is functional after the stderr flood
    let tools = client.list_tools().await.expect("list_tools failed");
    assert!(tools.is_empty(), "stderr-flood server has no tools");

    // Clean disconnect
    client.disconnect().await.expect("disconnect failed");
    assert_eq!(client.state(), ClientState::Disconnected);
}
