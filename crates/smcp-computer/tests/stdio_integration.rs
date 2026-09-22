//! Integration tests for StdioMCPClient with a real MCP server.
//!
//! Requires Node.js installed. Run with:
//! ```
//! cargo test --package smcp-computer --test stdio_integration -- --ignored
//! ```

use smcp_computer::mcp_clients::model::{
    ClientState, MCPClientError, MCPClientProtocol, StdioInitializationPhase, StdioServerParameters,
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

#[cfg(unix)]
fn shell_params(script: &str) -> StdioServerParameters {
    StdioServerParameters {
        command: "sh".to_string(),
        args: vec!["-c".to_string(), script.to_string()],
        env: HashMap::new(),
        cwd: None,
    }
}

#[cfg(unix)]
#[tokio::test]
async fn test_stdio_spawn_failure_is_structured() {
    let params = StdioServerParameters {
        command: "a2c-command-that-does-not-exist".to_string(),
        args: Vec::new(),
        env: HashMap::new(),
        cwd: None,
    };
    let client = StdioMCPClient::new(params);

    let error = client.connect().await.expect_err("spawn should fail");
    let display = error.to_string();
    let MCPClientError::StdioInitialization(error) = error else {
        panic!("expected structured stdio initialization error");
    };
    assert_eq!(
        error.diagnostic().phase,
        StdioInitializationPhase::SpawnFailed
    );
    assert!(error.diagnostic().stderr_tail.is_empty());
    assert!(display.contains("Connection error: Failed to start process"));
    assert!(display.contains("Failed to start process"));
}

#[cfg(unix)]
#[tokio::test]
async fn test_stdio_early_exit_captures_bounded_redacted_stderr() {
    let mut params = shell_params(
        "printf 'port already in use token=SHOULD_NOT_LEAK\\n'; i=0; while [ $i -lt 20000 ]; do printf x >&2; i=$((i+1)); done; exit 7",
    );
    params.env.insert(
        "OFFICE_SECRET".to_string(),
        "ENV_SECRET_SHOULD_NOT_LEAK".to_string(),
    );
    params.args[1] = "i=0; while [ $i -lt 20000 ]; do printf x >&2; i=$((i+1)); done; printf 'port already in use token=SHOULD_NOT_LEAK ENV_SECRET_SHOULD_NOT_LEAK AWS_SECRET_ACCESS_KEY=AWS_SHOULD_NOT_LEAK Authorization: Bearer AUTH_SHOULD_NOT_LEAK --api-key FLAG_SHOULD_NOT_LEAK\\n' >&2; exit 7".to_string();

    let client = StdioMCPClient::new(params);
    let error = client.connect().await.expect_err("child should exit early");
    let MCPClientError::StdioInitialization(error) = error else {
        panic!("expected structured stdio initialization error");
    };
    let diagnostic = error.diagnostic();
    assert_eq!(
        diagnostic.phase,
        StdioInitializationPhase::ProcessExitedBeforeInitialize
    );
    assert_eq!(diagnostic.exit_code, Some(7));
    assert_eq!(diagnostic.exit_success, Some(false));
    assert!(
        diagnostic.stderr_tail.len()
            <= smcp_computer::mcp_clients::stdio_client::MAX_STDIO_STDERR_BYTES
    );
    assert!(!diagnostic.stderr_tail.contains("SHOULD_NOT_LEAK"));
    assert!(!diagnostic
        .stderr_tail
        .contains("ENV_SECRET_SHOULD_NOT_LEAK"));
    assert!(!diagnostic.stderr_tail.contains("AWS_SHOULD_NOT_LEAK"));
    assert!(!diagnostic.stderr_tail.contains("AUTH_SHOULD_NOT_LEAK"));
    assert!(!diagnostic.stderr_tail.contains("FLAG_SHOULD_NOT_LEAK"));
    assert!(diagnostic.stderr_tail.contains("<redacted>"));
}

#[cfg(unix)]
#[tokio::test]
async fn test_stdio_redaction_handles_sensitive_value_split_across_reads() {
    let client = StdioMCPClient::new(shell_params(
        "i=0; while [ $i -lt 4095 ]; do printf x >&2; i=$((i+1)); done; printf 'token=' >&2; printf 'SPLIT_SHOULD_NOT_LEAK\\n' >&2; exit 7",
    ));
    let error = client.connect().await.expect_err("child should exit early");
    let MCPClientError::StdioInitialization(error) = error else {
        panic!("expected structured stdio initialization error");
    };
    assert!(!error
        .diagnostic()
        .stderr_tail
        .contains("SPLIT_SHOULD_NOT_LEAK"));
    assert!(error.diagnostic().stderr_tail.contains("<redacted>"));
}

#[cfg(unix)]
#[tokio::test]
async fn test_stdio_initialize_timeout_is_structured() {
    let client = StdioMCPClient::new_with_connect_timeout_secs(shell_params("sleep 5"), Some(1));
    let error = client.connect().await.expect_err("child should time out");
    let MCPClientError::StdioInitialization(error) = error else {
        panic!("expected structured stdio initialization error");
    };
    assert_eq!(
        error.diagnostic().phase,
        StdioInitializationPhase::InitializeTimeout
    );
    assert!(error.to_string().contains("after 1s"));
}

#[cfg(unix)]
#[tokio::test]
async fn test_stdio_protocol_error_is_distinguished() {
    // `exec sleep 30` 让子进程在应答后**继续存活**：判别「协议错误」要求服务端先读到 JSON-RPC error
    // 再谈进程退出。若子进程应答完立即退出，「读到 error」与「观测到 stdout EOF / 进程退出」成为竞速，
    // 结果会在 `InitializeProtocolError` 与 `ProcessExitedBeforeInitialize` 之间抖动（负载下实测可复现，
    // 这也是历史上 CI 给本套件加 `--test-threads=1` 的成因）。保持存活即消除该竞速，使断言确定性成立，
    // 同时顺带覆盖「拿到协议错误后清理（kill + 有界 stderr 抽取）」路径。
    let client = StdioMCPClient::new(shell_params(
        "printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32600,\"message\":\"bad initialize token=PROVIDER_SECRET_SHOULD_NOT_LEAK\"}}'; exec sleep 30",
    ));
    let error = client
        .connect()
        .await
        .expect_err("protocol response should fail");
    let MCPClientError::StdioInitialization(error) = error else {
        panic!("expected structured stdio initialization error");
    };
    assert_eq!(
        error.diagnostic().phase,
        StdioInitializationPhase::InitializeProtocolError
    );
    assert!(!error
        .to_string()
        .contains("PROVIDER_SECRET_SHOULD_NOT_LEAK"));
}

#[cfg(unix)]
#[tokio::test]
async fn test_stdio_connection_closed_while_child_alive_is_distinguished() {
    let client = StdioMCPClient::new(shell_params("exec 1>&-; sleep 2"));
    let error = client
        .connect()
        .await
        .expect_err("closed stdout should fail initialization");
    let MCPClientError::StdioInitialization(error) = error else {
        panic!("expected structured stdio initialization error");
    };
    let diagnostic = error.diagnostic();
    assert_eq!(
        diagnostic.phase,
        StdioInitializationPhase::InitializeConnectionClosed
    );
    assert_eq!(diagnostic.exit_code, None);
    assert_eq!(diagnostic.exit_success, None);
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

#[cfg(unix)]
#[tokio::test]
async fn test_stdio_custom_connect_timeout_reports_effective_value() {
    let params = StdioServerParameters {
        command: "sh".to_string(),
        args: vec!["-c".to_string(), "sleep 5 & wait".to_string()],
        env: HashMap::new(),
        cwd: None,
    };

    let client = StdioMCPClient::new_with_connect_timeout_secs(params, Some(1));
    let result = tokio::time::timeout(Duration::from_secs(4), client.connect())
        .await
        .expect("custom STDIO timeout should fire before the outer guard");

    match result {
        Err(MCPClientError::StdioInitialization(error)) => {
            assert_eq!(
                error.diagnostic().phase,
                StdioInitializationPhase::InitializeTimeout
            );
            assert!(error.to_string().contains("after 1s"));
        }
        other => panic!("expected a custom STDIO timeout, got {other:?}"),
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
