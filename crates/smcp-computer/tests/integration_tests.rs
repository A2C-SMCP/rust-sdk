/**
* 文件名: integration_tests
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: tokio, async-trait
* 描述: 集成测试，验证整个系统的协同工作
*/

use smcp_computer::mcp_clients::*;
use smcp_computer::inputs::*;
use std::collections::HashMap;
use tokio::time::{sleep, Duration};

/// 测试完整的MCP服务器管理工作流 / Test complete MCP server management workflow
#[tokio::test]
async fn test_complete_workflow() {
    // 1. 创建管理器 / Create manager
    let manager = MCPServerManager::new();
    
    // 2. 准备服务器配置 / Prepare server configurations
    let mut configs = Vec::new();
    
    // STDIO服务器配置 / STDIO server configuration
    configs.push(MCPServerConfig::Stdio(StdioServerConfig {
        name: "echo_server".to_string(),
        disabled: false,
        forbidden_tools: vec![],
        tool_meta: {
            let mut meta = HashMap::new();
            meta.insert("echo".to_string(), ToolMeta {
                auto_apply: Some(true),
                alias: Some("echo_tool".to_string()),
                tags: Some(vec!["utility".to_string()]),
                ret_object_mapper: None,
            });
            meta
        },
        default_tool_meta: Some(ToolMeta {
            auto_apply: Some(false),
            alias: None,
            tags: Some(vec!["default".to_string()]),
            ret_object_mapper: None,
        }),
        vrl: None,
        server_parameters: StdioServerParameters {
            command: "echo".to_string(),
            args: vec!["Hello from MCP server".to_string()],
            env: HashMap::new(),
            cwd: None,
        },
    }));
    
    // HTTP服务器配置 / HTTP server configuration
    configs.push(MCPServerConfig::Http(HttpServerConfig {
        name: "http_server".to_string(),
        disabled: true, // 禁用以避免实际连接 / Disable to avoid actual connection
        forbidden_tools: vec![],
        tool_meta: HashMap::new(),
        default_tool_meta: None,
        vrl: None,
        server_parameters: HttpServerParameters {
            url: "http://localhost:8080".to_string(),
            headers: HashMap::new(),
        },
    }));
    
    // 3. 初始化管理器 / Initialize manager
    let result = manager.initialize(configs).await;
    assert!(result.is_ok(), "Failed to initialize manager: {:?}", result.err());
    
    // 4. 检查初始状态 / Check initial status
    let status = manager.get_server_status().await;
    assert_eq!(status.len(), 2);
    
    // 5. 启动所有服务器 / Start all servers
    let result = manager.start_all().await;
    assert!(result.is_ok(), "Failed to start servers: {:?}", result.err());
    
    // 6. 等待连接建立 / Wait for connections to establish
    sleep(Duration::from_millis(200)).await;
    
    // 7. 检查运行状态 / Check running status
    let status = manager.get_server_status().await;
    let echo_status = status.iter().find(|(name, _, _)| name == "echo_server").unwrap();
    assert!(echo_status.1); // echo_server 应该已激活 / echo_server should be active
    
    let http_status = status.iter().find(|(name, _, _)| name == "http_server").unwrap();
    assert!(!http_status.1); // http_server 应该未激活（被禁用）/ http_server should not be active (disabled)
    
    // 8. 停止所有服务器 / Stop all servers
    let result = manager.stop_all().await;
    assert!(result.is_ok(), "Failed to stop servers: {:?}", result.err());
    
    // 9. 检查最终状态 / Check final status
    let status = manager.get_server_status().await;
    for (_, active, _) in status {
        assert!(!active); // 所有服务器都应该未激活 / All servers should be inactive
    }
    
    // 10. 关闭管理器 / Close manager
    let result = manager.close().await;
    assert!(result.is_ok(), "Failed to close manager: {:?}", result.err());
}

/// 测试输入系统集成 / Test input system integration
#[tokio::test]
async fn test_input_system_integration() {
    // 创建输入上下文 / Create input context
    let ctx = InputContext::new()
        .with_server_name("test_server".to_string())
        .with_tool_name("test_tool".to_string())
        .with_metadata("env".to_string(), "test".to_string());
    
    // 测试环境变量输入提供者 / Test environment variable input provider
    let provider = EnvironmentInputProvider::new()
        .with_prefix("TEST_".to_string());
    
    // 设置测试环境变量 / Set test environment variables
    std::env::set_var("TEST_INPUT_VALUE", "test_value");
    std::env::set_var("TEST_OTHER_VALUE", "other_value");
    
    // 创建输入请求 / Create input request
    let request = InputRequest {
        id: "test_input".to_string(),
        input_type: InputType::String { password: None, min_length: None, max_length: None },
        title: "Test Input".to_string(),
        description: "Test input description".to_string(),
        default: None,
        required: false,
        validation: None,
    };
    
    // 获取输入值 / Get input values
    let response = provider.get_input(&request, &ctx).await;
    
    // 验证结果 / Verify results
    assert!(response.is_ok());
    // 环境变量提供者应该返回环境变量的值
    // The environment provider should return the environment variable value
    
    // 清理环境变量 / Clean up environment variables
    std::env::remove_var("TEST_INPUT_VALUE");
    std::env::remove_var("TEST_OTHER_VALUE");
}

/// 测试错误处理 / Test error handling
#[tokio::test]
async fn test_error_handling() {
    let manager = MCPServerManager::new();
    
    // 尝试启动不存在的服务器 / Try to start non-existent server
    let result = manager.start_client("non_existent").await;
    assert!(result.is_err());
    
    // 尝试停止不存在的服务器 / Try to stop non-existent server
    let result = manager.stop_client("non_existent").await;
    assert!(result.is_err());
    
    // 尝试移除不存在的服务器 / Try to remove non-existent server
    let result = manager.remove_server("non_existent").await;
    assert!(result.is_err());
}

/// 测试并发操作 / Test concurrent operations
#[tokio::test]
async fn test_concurrent_operations() {
    let manager = std::sync::Arc::new(MCPServerManager::new());
    
    // 添加多个服务器 / Add multiple servers
    let mut handles = Vec::new();
    
    for i in 0..5 {
        let manager_clone = manager.clone();
        let handle = tokio::spawn(async move {
            let config = MCPServerConfig::Stdio(StdioServerConfig {
                name: format!("server_{}", i),
                disabled: false,
                forbidden_tools: vec![],
                tool_meta: HashMap::new(),
                default_tool_meta: None,
                vrl: None,
                server_parameters: StdioServerParameters {
                    command: "echo".to_string(),
                    args: vec![format!("server_{}", i)],
                    env: HashMap::new(),
                    cwd: None,
                },
            });
            
            manager_clone.add_or_update_server(config).await
        });
        
        handles.push(handle);
    }
    
    // 等待所有操作完成 / Wait for all operations to complete
    for handle in handles {
        let result = handle.await.unwrap();
        assert!(result.is_ok());
    }
    
    // 检查状态 / Check status
    let status = manager.get_server_status().await;
    assert_eq!(status.len(), 5);
    
    // 并发启动 / Concurrent start
    let mut handles = Vec::new();
    
    for i in 0..5 {
        let manager_clone = manager.clone();
        let server_name = format!("server_{}", i);
        let handle = tokio::spawn(async move {
            manager_clone.start_client(&server_name).await
        });
        
        handles.push(handle);
    }
    
    // 等待所有启动完成 / Wait for all starts to complete
    for handle in handles {
        let result = handle.await.unwrap();
        assert!(result.is_ok());
    }
    
    // 等待连接建立 / Wait for connections to establish
    sleep(Duration::from_millis(200)).await;
    
    // 关闭管理器 / Close manager
    manager.close().await.unwrap();
}
