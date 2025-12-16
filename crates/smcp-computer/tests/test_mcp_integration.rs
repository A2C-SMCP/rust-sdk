use smcp_computer::mcp_clients::http_client::HttpMCPClient;
use smcp_computer::mcp_clients::sse_client::SseMCPClient;
use smcp_computer::mcp_clients::stdio_client::StdioMCPClient;
use smcp_computer::mcp_clients::*;
/**
* 文件名: test_mcp_integration
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: tokio, smcp-computer
* 描述: MCP客户端集成测试 / MCP client integration tests
*/
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::timeout;
use tracing::info;

#[tokio::test]
async fn test_stdio_client_with_echo_server() {
    // 初始化日志 / Initialize logging
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();

    info!("开始STDIO客户端与echo服务器测试");

    // 使用 yes 命令配合 head 模拟一个简单的 MCP 服务器
    // Use yes command with head to simulate a simple MCP server
    let params = StdioServerParameters {
        command: "sh".to_string(),
        args: vec![
            "-c".to_string(),
            "echo '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"tools\":[]}}'; cat".to_string(),
        ],
        env: HashMap::new(),
        cwd: None,
    };

    let client = StdioMCPClient::new(params);

    // 测试客户端创建 / Test client creation
    assert_eq!(client.state(), ClientState::Initialized);

    // 测试连接成功（模拟服务器输出了有效的初始化响应）
    // Test connection succeeds (mock server outputs valid init response)
    let connect_result = client.connect().await;
    assert!(
        connect_result.is_ok(),
        "Connection should succeed with valid JSON response"
    );

    // 测试 list_tools 返回空列表（因为 cat 只是回显请求，不是真正的 MCP 服务器）
    // Test list_tools returns empty list (because cat just echoes requests, not a real MCP server)
    let list_tools_result = client.list_tools().await;
    assert!(list_tools_result.is_ok(), "List tools should complete");
    let tools = list_tools_result.unwrap();
    assert!(
        tools.is_empty(),
        "Tools list should be empty for mock server"
    );

    info!("STDIO客户端与echo服务器测试完成");
}

#[tokio::test]
async fn test_http_client_connection_timeout() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();

    info!("开始HTTP客户端连接超时测试");

    // 使用一个不存在的地址测试超时处理
    // Use a non-existent address to test timeout handling
    let params = HttpServerParameters {
        url: "http://192.0.2.1:8080".to_string(), // RFC 5737 test address
        headers: HashMap::new(),
    };

    let client = HttpMCPClient::new(params);

    // 测试连接超时 / Test connection timeout
    let connect_result = timeout(Duration::from_secs(35), client.connect()).await;
    assert!(
        connect_result.is_ok(),
        "Connection should timeout within 35 seconds"
    );

    let result = connect_result.unwrap();
    assert!(
        result.is_err(),
        "Connection to non-existent address should fail"
    );

    info!("HTTP客户端连接超时测试完成");
}

#[tokio::test]
async fn test_sse_client_invalid_url() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();

    info!("开始SSE客户端无效URL测试");

    // 测试各种无效URL格式 / Test various invalid URL formats
    let invalid_urls = vec!["not-a-url", "ftp://example.com", "http://", ""];

    for url in invalid_urls {
        let params = SseServerParameters {
            url: url.to_string(),
            headers: HashMap::new(),
        };

        let client = SseMCPClient::new(params);

        // 验证客户端创建成功 / Verify client creation succeeds
        assert_eq!(client.state(), ClientState::Initialized);

        // 连接应该失败 / Connection should fail
        let connect_result = client.connect().await;
        assert!(
            connect_result.is_err(),
            "Connection should fail with invalid URL: {}",
            url
        );
    }

    info!("SSE客户端无效URL测试完成");
}

#[tokio::test]
async fn test_client_parameter_serialization() {
    // 测试参数序列化和反序列化 / Test parameter serialization and deserialization
    let http_params = HttpServerParameters {
        url: "https://example.com/mcp".to_string(),
        headers: {
            let mut h = HashMap::new();
            h.insert("Authorization".to_string(), "Bearer token123".to_string());
            h.insert("X-API-Key".to_string(), "key456".to_string());
            h
        },
    };

    // 序列化 / Serialize
    let serialized = serde_json::to_string(&http_params);
    assert!(serialized.is_ok(), "HTTP parameters should serialize");

    // 反序列化 / Deserialize
    let deserialized: HttpServerParameters = serde_json::from_str(&serialized.unwrap()).unwrap();
    assert_eq!(deserialized.url, "https://example.com/mcp");
    assert_eq!(deserialized.headers.len(), 2);
    assert_eq!(
        deserialized.headers.get("Authorization").unwrap(),
        "Bearer token123"
    );

    // 测试SSE参数 / Test SSE parameters
    let sse_params = SseServerParameters {
        url: "https://example.com/sse".to_string(),
        headers: HashMap::new(),
    };

    let serialized = serde_json::to_string(&sse_params);
    assert!(serialized.is_ok(), "SSE parameters should serialize");

    // 测试STDIO参数 / Test STDIO parameters
    let stdio_params = StdioServerParameters {
        command: "node".to_string(),
        args: vec![
            "server.js".to_string(),
            "--port".to_string(),
            "3000".to_string(),
        ],
        env: {
            let mut e = HashMap::new();
            e.insert("NODE_ENV".to_string(), "test".to_string());
            e
        },
        cwd: Some("/app".to_string()),
    };

    let serialized = serde_json::to_string(&stdio_params);
    assert!(serialized.is_ok(), "STDIO parameters should serialize");

    let deserialized: StdioServerParameters = serde_json::from_str(&serialized.unwrap()).unwrap();
    assert_eq!(deserialized.command, "node");
    assert_eq!(deserialized.args.len(), 3);
    assert_eq!(deserialized.env.get("NODE_ENV").unwrap(), "test");
    assert_eq!(deserialized.cwd.unwrap(), "/app");
}

#[tokio::test]
async fn test_client_error_handling() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();

    info!("开始客户端错误处理测试");

    // 测试HTTP客户端错误处理 / Test HTTP client error handling
    let http_client = HttpMCPClient::new(HttpServerParameters {
        url: "http://invalid-url".to_string(),
        headers: HashMap::new(),
    });

    // 在未连接状态下调用操作应该失败
    // Operations should fail when not connected
    assert!(http_client.list_tools().await.is_err());
    assert!(http_client
        .call_tool("test", serde_json::json!({}))
        .await
        .is_err());
    assert!(http_client.list_windows().await.is_err());

    // 测试SSE客户端错误处理 / Test SSE client error handling
    let sse_client = SseMCPClient::new(SseServerParameters {
        url: "http://invalid-url".to_string(),
        headers: HashMap::new(),
    });

    assert!(sse_client.list_tools().await.is_err());
    assert!(sse_client
        .call_tool("test", serde_json::json!({}))
        .await
        .is_err());
    assert!(sse_client.list_windows().await.is_err());

    // 测试STDIO客户端错误处理 / Test STDIO client error handling
    let stdio_client = StdioMCPClient::new(StdioServerParameters {
        command: "non-existent-command".to_string(),
        args: vec![],
        env: HashMap::new(),
        cwd: None,
    });

    assert!(stdio_client.list_tools().await.is_err());
    assert!(stdio_client
        .call_tool("test", serde_json::json!({}))
        .await
        .is_err());
    assert!(stdio_client.list_windows().await.is_err());

    info!("客户端错误处理测试完成");
}

#[tokio::test]
async fn test_client_concurrent_creation() {
    // 测试并发创建客户端 / Test concurrent client creation
    let mut handles = Vec::new();

    for i in 0..10 {
        let handle = tokio::spawn(async move {
            // HTTP客户端 / HTTP client
            let http_client = HttpMCPClient::new(HttpServerParameters {
                url: format!("http://example{}.com", i),
                headers: HashMap::new(),
            });
            assert_eq!(http_client.state(), ClientState::Initialized);

            // SSE客户端 / SSE client
            let sse_client = SseMCPClient::new(SseServerParameters {
                url: format!("http://example{}.com/sse", i),
                headers: HashMap::new(),
            });
            assert_eq!(sse_client.state(), ClientState::Initialized);

            // STDIO客户端 / STDIO client
            let stdio_client = StdioMCPClient::new(StdioServerParameters {
                command: format!("command{}", i),
                args: vec![],
                env: HashMap::new(),
                cwd: None,
            });
            assert_eq!(stdio_client.state(), ClientState::Initialized);

            i
        });

        handles.push(handle);
    }

    // 等待所有任务完成 / Wait for all tasks
    for (expected_index, handle) in handles.into_iter().enumerate() {
        let result = timeout(Duration::from_secs(5), handle).await;
        assert!(result.is_ok(), "Task should complete quickly");
        assert_eq!(result.unwrap().unwrap(), expected_index); // Each task returns its index
    }
}

#[test]
fn test_client_debug_implementations() {
    // 测试Debug trait实现 / Test Debug trait implementations
    let http_client = HttpMCPClient::new(HttpServerParameters {
        url: "http://example.com".to_string(),
        headers: HashMap::new(),
    });

    let debug_str = format!("{:?}", http_client);
    assert!(debug_str.contains("HttpMCPClient"));

    let sse_client = SseMCPClient::new(SseServerParameters {
        url: "http://example.com/sse".to_string(),
        headers: HashMap::new(),
    });

    let debug_str = format!("{:?}", sse_client);
    assert!(debug_str.contains("SseMCPClient"));

    let stdio_client = StdioMCPClient::new(StdioServerParameters {
        command: "echo".to_string(),
        args: vec!["hello".to_string()],
        env: HashMap::new(),
        cwd: None,
    });

    let debug_str = format!("{:?}", stdio_client);
    assert!(debug_str.contains("StdioMCPClient"));
}
