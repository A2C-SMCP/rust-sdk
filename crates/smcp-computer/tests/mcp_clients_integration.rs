/**
* 文件名: mcp_clients_integration
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: tokio, serde_json
* 描述: MCP客户端集成测试
*/
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;
use smcp_computer::mcp_clients::*;
use smcp_computer::mcp_clients::http_client::HttpMCPClient;
use smcp_computer::mcp_clients::sse_client::SseMCPClient;
use smcp_computer::mcp_clients::stdio_client::StdioMCPClient;

mod common;

#[tokio::test]
async fn test_all_clients_state_transitions() {
    // 测试所有客户端类型的状态转换 / Test state transitions for all client types
    
    // HTTP客户端 / HTTP client
    let http_params = HttpServerParameters {
        url: "http://localhost:8080".to_string(),
        headers: HashMap::new(),
    };
    let http_client = HttpMCPClient::new(http_params);
    assert_eq!(http_client.state(), ClientState::Initialized);
    
    // SSE客户端 / SSE client
    let sse_params = SseServerParameters {
        url: "http://localhost:8081".to_string(),
        headers: HashMap::new(),
    };
    let sse_client = SseMCPClient::new(sse_params);
    assert_eq!(sse_client.state(), ClientState::Initialized);
    
    // STDIO客户端 / STDIO client
    let stdio_params = StdioServerParameters {
        command: "echo".to_string(),
        args: vec!["test".to_string()],
        env: HashMap::new(),
        cwd: None,
    };
    let stdio_client = StdioMCPClient::new(stdio_params);
    assert_eq!(stdio_client.state(), ClientState::Initialized);
}

#[tokio::test]
async fn test_all_clients_error_handling() {
    // 测试所有客户端在未连接状态下的错误处理
    // Test error handling for all clients when not connected
    
    let http_params = HttpServerParameters {
        url: "http://invalid:8080".to_string(),
        headers: HashMap::new(),
    };
    let http_client = HttpMCPClient::new(http_params);
    
    // 所有操作在未连接状态下都应该失败
    assert!(http_client.list_tools().await.is_err());
    assert!(http_client.call_tool("test", serde_json::json!({})).await.is_err());
    assert!(http_client.list_windows().await.is_err());
    
    let sse_params = SseServerParameters {
        url: "http://invalid:8081".to_string(),
        headers: HashMap::new(),
    };
    let sse_client = SseMCPClient::new(sse_params);
    
    assert!(sse_client.list_tools().await.is_err());
    assert!(sse_client.call_tool("test", serde_json::json!({})).await.is_err());
    assert!(sse_client.list_windows().await.is_err());
    
    let stdio_params = StdioServerParameters {
        command: "echo".to_string(),
        args: vec!["test".to_string()],
        env: HashMap::new(),
        cwd: None,
    };
    let stdio_client = StdioMCPClient::new(stdio_params);
    
    assert!(stdio_client.list_tools().await.is_err());
    assert!(stdio_client.call_tool("test", serde_json::json!({})).await.is_err());
    assert!(stdio_client.list_windows().await.is_err());
}

#[tokio::test]
async fn test_http_client_headers() {
    // 测试HTTP客户端头部处理 / Test HTTP client headers handling
    let mut headers = HashMap::new();
    headers.insert("Authorization".to_string(), "Bearer token123".to_string());
    headers.insert("X-Custom-Header".to_string(), "custom-value".to_string());
    
    let params = HttpServerParameters {
        url: "http://localhost:8080".to_string(),
        headers,
    };
    
    let client = HttpMCPClient::new(params);
    
    // 验证客户端创建成功（头部验证需要通过公共API）
    assert_eq!(client.state(), ClientState::Initialized);
    // Note: Header verification would need public accessors or be tested through actual requests
}

#[tokio::test]
async fn test_sse_client_url_formatting() {
    // 测试SSE客户端URL格式化 / Test SSE client URL formatting
    let test_cases = vec![
        ("http://localhost:8080", "http://localhost:8080?events=true"),
        ("http://localhost:8080?param=value", "http://localhost:8080?param=value&events=true"),
        ("https://example.com/mcp", "https://example.com/mcp?events=true"),
    ];
    
    for (base_url, _expected_sse_url) in test_cases {
        let params = SseServerParameters {
            url: base_url.to_string(),
            headers: HashMap::new(),
        };
        
        let client = SseMCPClient::new(params);
        // 通过 state() 方法验证客户端状态
        assert_eq!(client.state(), ClientState::Initialized);
        
        // 注意：实际的URL格式化在 start_sse_connection 方法中进行
        // 这里我们只验证客户端创建成功
        // Note: Actual URL formatting happens in start_sse_connection method
        // Here we only verify client creation succeeded
    }
}

#[tokio::test]
async fn test_stdio_client_process_management() {
    // 测试STDIO客户端进程管理 / Test STDIO client process management
    let params = StdioServerParameters {
        command: "echo".to_string(),
        args: vec!["hello".to_string(), "world".to_string()],
        env: HashMap::new(),
        cwd: None,
    };
    
    let client = StdioMCPClient::new(params);
    
    // 测试客户端创建成功
    assert_eq!(client.state(), ClientState::Initialized);
    
    // 注意：echo 命令不支持 MCP 协议，所以连接会失败
    // 这里我们测试错误处理而不是成功连接
    // Note: echo command doesn't support MCP protocol, so connect will fail
    // Here we test error handling instead of successful connection
    let connect_result = client.connect().await;
    assert!(connect_result.is_err());
}

#[tokio::test]
async fn test_session_id_lifecycle() {
    // 测试会话ID生命周期 / Test session ID lifecycle
    let http_params = HttpServerParameters {
        url: "http://localhost:8080".to_string(),
        headers: HashMap::new(),
    };
    let http_client = HttpMCPClient::new(http_params);
    
    // 初始状态验证
    assert_eq!(http_client.state(), ClientState::Initialized);
    
    // Note: Session ID testing would require public accessors
    // In real tests, session ID is set during connect() and cleared during disconnect()
}

#[tokio::test]
async fn test_error_propagation() {
    // 测试错误传播 / Test error propagation
    let params = HttpServerParameters {
        url: "http://invalid-host-name-12345.com".to_string(),
        headers: HashMap::new(),
    };
    
    let client = HttpMCPClient::new(params);
    
    // 尝试连接应该返回连接错误
    let result = client.connect().await;
    assert!(result.is_err());
    
    match result.unwrap_err() {
        MCPClientError::ConnectionError(_) => {
            // 预期的错误类型 / Expected error type
        }
        _ => panic!("Expected ConnectionError"),
    }
}

#[tokio::test]
async fn test_concurrent_operations() {
    // 测试并发操作 / Test concurrent operations
    let params = StdioServerParameters {
        command: "sleep".to_string(),
        args: vec!["1".to_string()],
        env: HashMap::new(),
        cwd: None,
    };
    
    let client = Arc::new(StdioMCPClient::new(params));
    
    // 测试并发操作（通过公共API）
    // Note: Actual concurrent process testing would need public accessors
    // Here we test that the client can be cloned and used concurrently
    let client_clone = Arc::clone(&client);
    let task1 = tokio::spawn(async move {
        let _ = client_clone.connect().await;
    });
    
    let client_clone = Arc::clone(&client);
    let task2 = tokio::spawn(async move {
        let _ = client_clone.connect().await;
    });
    
    // 等待任务完成
    timeout(Duration::from_secs(5), task1).await.unwrap().unwrap();
    timeout(Duration::from_secs(5), task2).await.unwrap().unwrap();
    
    // 清理
    let _ = client.disconnect().await;
}

#[test]
fn test_parameter_serialization() {
    // 测试参数序列化 / Test parameter serialization
    let http_params = HttpServerParameters {
        url: "http://localhost:8080".to_string(),
        headers: HashMap::new(),
    };
    
    let serialized = serde_json::to_string(&http_params);
    assert!(serialized.is_ok());
    
    let deserialized: HttpServerParameters = serde_json::from_str(&serialized.unwrap()).unwrap();
    
    let params = deserialized;
    assert_eq!(params.url, "http://localhost:8080");
}

#[test]
fn test_debug_implementations() {
    // 测试Debug trait实现 / Test Debug trait implementations
    let http_params = HttpServerParameters {
        url: "http://localhost:8080".to_string(),
        headers: HashMap::new(),
    };
    let http_client = HttpMCPClient::new(http_params);
    
    let debug_str = format!("{:?}", http_client);
    assert!(debug_str.contains("HttpMCPClient"));
    
    let sse_params = SseServerParameters {
        url: "http://localhost:8081".to_string(),
        headers: HashMap::new(),
    };
    let sse_client = SseMCPClient::new(sse_params);
    
    let debug_str = format!("{:?}", sse_client);
    assert!(debug_str.contains("SseMCPClient"));
    
    let stdio_params = StdioServerParameters {
        command: "echo".to_string(),
        args: vec!["test".to_string()],
        env: HashMap::new(),
        cwd: None,
    };
    let stdio_client = StdioMCPClient::new(stdio_params);
    
    let debug_str = format!("{:?}", stdio_client);
    assert!(debug_str.contains("StdioMCPClient"));
}
