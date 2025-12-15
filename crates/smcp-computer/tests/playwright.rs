/**
* 文件名: playwright
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: tokio, smcp-computer
* 描述: Playwright MCP服务器E2E测试 / Playwright MCP server E2E tests
*/
use smcp_computer::mcp_clients::stdio_client::StdioMCPClient;
use smcp_computer::mcp_clients::MCPClientProtocol;
use tracing::info;
use serde_json::json;

#[cfg_attr(not(feature = "e2e"), ignore)]
#[tokio::test]
async fn test_playwright_mcp_server_basic_connection() {
    // Initialize tracing for test output
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .try_init();
    
    info!("开始Playwright MCP服务器基础连接测试");
    
    // 创建Playwright MCP客户端
    let params = smcp_computer::mcp_clients::StdioServerParameters {
        command: "npx".to_string(),
        args: vec!["@playwright/mcp@latest".to_string()],
        env: std::collections::HashMap::new(),
        cwd: None,
    };
    
    let client = StdioMCPClient::new(params);
    
    // 测试连接
    let connect_result = client.connect().await;
    assert!(connect_result.is_ok(), "Failed to connect: {:?}", connect_result.err());
    
    // 连接后自动初始化
    // connect() 方法会自动处理初始化
    
    // 测试列出工具
    let tools_result = client.list_tools().await;
    assert!(tools_result.is_ok(), "Failed to list tools: {:?}", tools_result.err());
    
    let tools = tools_result.unwrap();
    info!("可用工具数量: {}", tools.len());
    
    // 验证Playwright特有的工具存在
    let tool_names: Vec<String> = tools.iter().map(|t| t.name.clone()).collect();
    assert!(tool_names.contains(&"browser_navigate".to_string()), 
           "Expected browser_navigate tool not found");
    
    // 清理
    let _ = client.disconnect().await;
    
    info!("Playwright MCP服务器基础连接测试完成");
}

#[cfg_attr(not(feature = "e2e"), ignore)]
#[tokio::test]
async fn test_playwright_mcp_server_tool_execution() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .try_init();
    
    info!("开始Playwright MCP服务器工具执行测试");
    
    let params = smcp_computer::mcp_clients::StdioServerParameters {
        command: "npx".to_string(),
        args: vec!["@playwright/mcp@latest".to_string()],
        env: std::collections::HashMap::new(),
        cwd: None,
    };
    
    let client = StdioMCPClient::new(params);
    
    client.connect().await.expect("Failed to connect to server");
    
    // 测试导航工具
    let navigate_args = json!({
        "url": "https://example.com"
    });
    
    let result = client.call_tool("browser_navigate", navigate_args).await;
    assert!(result.is_ok(), "Failed to call browser_navigate: {:?}", result.err());
    
    let response = result.unwrap();
    assert!(!response.is_error, "Navigation should succeed");
    info!("导航响应: {:?}", response.content);
    
    // 清理
    let _ = client.disconnect().await;
    
    info!("Playwright MCP服务器工具执行测试完成");
}
