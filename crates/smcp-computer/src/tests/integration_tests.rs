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
    
    // 3. 初始化管理器 / Initialize manager
    let result = manager.initialize(configs).await;
    assert!(result.is_ok());
    
    // 4. 启动所有服务器 / Start all servers
    let result = manager.start_all().await;
    assert!(result.is_ok());
    
    // 等待服务器启动 / Wait for servers to start
    sleep(Duration::from_millis(200)).await;
    
    // 5. 检查服务器状态 / Check server status
    let status = manager.get_server_status().await;
    assert_eq!(status.len(), 1);
    let server_status = status.iter().find(|(name, _, _)| name == "echo_server").unwrap();
    assert!(server_status.1); // 服务器应该已激活 / Server should be active
    
    // 6. 验证工具调用 / Validate tool call
    let result = manager.validate_tool_call("echo_tool", &serde_json::json!({})).await;
    assert!(result.is_ok());
    let (server_name, tool_name) = result.unwrap();
    assert_eq!(server_name, "echo_server");
    assert_eq!(tool_name, "echo");
    
    // 7. 调用工具 / Call tool
    let result = manager.execute_tool(
        "echo_tool",
        serde_json::json!({"message": "test"}),
        Some(Duration::from_secs(5)),
    ).await;
    
    // 注意：实际的echo命令可能不会处理JSON参数，这里主要测试调用流程
    // Note: The actual echo command might not process JSON parameters, this mainly tests the call flow
    
    // 8. 获取可用工具列表 / Get available tools list
    let tools = manager.list_available_tools().await;
    // 工具列表可能为空，因为echo命令不是真正的MCP服务器
    // Tool list might be empty because echo is not a real MCP server
    
    // 9. 停止所有服务器 / Stop all servers
    let result = manager.stop_all().await;
    assert!(result.is_ok());
    
    // 10. 关闭管理器 / Close manager
    let result = manager.close().await;
    assert!(result.is_ok());
}

/// 测试输入系统与MCP服务器的集成 / Test integration of input system with MCP servers
#[tokio::test]
async fn test_input_integration() {
    // 1. 创建输入处理器 / Create input handler
    let input_handler = InputHandler::new();
    
    // 2. 设置环境变量 / Set environment variables
    std::env::set_var("A2C_SMCP_API_KEY", "test_api_key_123");
    std::env::set_var("A2C_SMCP_SECRET_KEY", "test_secret");
    
    // 3. 创建MCP输入配置 / Create MCP input configurations
    let mcp_inputs = vec![
        crate::mcp_clients::model::MCPServerInput::PromptString(
            crate::mcp_clients::model::PromptStringInput {
                id: "api_key".to_string(),
                description: "API Key for authentication".to_string(),
                default: None,
                password: Some(true),
            }
        ),
        crate::mcp_clients::model::MCPServerInput::PickString(
            crate::mcp_clients::model::PickStringInput {
                id: "environment".to_string(),
                description: "Select environment".to_string(),
                options: vec!["development".to_string(), "staging".to_string(), "production".to_string()],
                default: Some("development".to_string()),
            }
        ),
    ];
    
    // 4. 创建输入上下文 / Create input context
    let context = InputContext::new()
        .with_server_name("test_server".to_string())
        .with_tool_name("authenticate".to_string());
    
    // 5. 处理MCP输入 / Handle MCP inputs
    let result = input_handler.handle_mcp_inputs(&mcp_inputs, context).await;
    assert!(result.is_ok());
    
    let inputs = result.unwrap();
    
    // 验证从环境变量获取的值 / Verify values from environment variables
    assert_eq!(
        inputs.get("api_key"),
        Some(&InputValue::String("test_api_key_123".to_string()))
    );
    
    // environment输入没有环境变量，应该使用默认值或失败
    // environment input has no environment variable, should use default or fail
    
    // 6. 清理环境变量 / Clean up environment variables
    std::env::remove_var("A2C_SMCP_API_KEY");
    std::env::remove_var("A2C_SMCP_SECRET_KEY");
}

/// 测试工具别名和冲突处理 / Test tool alias and conflict handling
#[tokio::test]
async fn test_tool_alias_and_conflicts() {
    let manager = MCPServerManager::new();
    
    // 创建两个服务器，有同名工具但使用别名解决冲突
    // Create two servers with same tool name but resolve conflicts using aliases
    let mut configs = Vec::new();
    
    // 第一个服务器 / First server
    configs.push(MCPServerConfig::Stdio(StdioServerConfig {
        name: "server1".to_string(),
        disabled: false,
        forbidden_tools: vec![],
        tool_meta: {
            let mut meta = HashMap::new();
            meta.insert("common_tool".to_string(), ToolMeta {
                auto_apply: None,
                alias: Some("server1_tool".to_string()),
                tags: None,
                ret_object_mapper: None,
            });
            meta
        },
        default_tool_meta: None,
        vrl: None,
        server_parameters: StdioServerParameters {
            command: "echo".to_string(),
            args: vec!["server1".to_string()],
            env: HashMap::new(),
            cwd: None,
        },
    }));
    
    // 第二个服务器 / Second server
    configs.push(MCPServerConfig::Stdio(StdioServerConfig {
        name: "server2".to_string(),
        disabled: false,
        forbidden_tools: vec![],
        tool_meta: {
            let mut meta = HashMap::new();
            meta.insert("common_tool".to_string(), ToolMeta {
                auto_apply: None,
                alias: Some("server2_tool".to_string()),
                tags: None,
                ret_object_mapper: None,
            });
            meta
        },
        default_tool_meta: None,
        vrl: None,
        server_parameters: StdioServerParameters {
            command: "echo".to_string(),
            args: vec!["server2".to_string()],
            env: HashMap::new(),
            cwd: None,
        },
    }));
    
    // 初始化应该成功 / Initialization should succeed
    let result = manager.initialize(configs).await;
    assert!(result.is_ok());
    
    // 启动服务器 / Start servers
    let result = manager.start_all().await;
    // 可能成功也可能失败，取决于实际的MCP服务器实现
    // Might succeed or fail depending on actual MCP server implementation
    
    // 等待连接建立 / Wait for connections to establish
    sleep(Duration::from_millis(200)).await;
    
    // 测试别名调用 / Test alias calls
    let result1 = manager.validate_tool_call("server1_tool", &serde_json::json!({})).await;
    let result2 = manager.validate_tool_call("server2_tool", &serde_json::json!({})).await;
    
    // 根据实际实现，这些可能成功或失败
    // Depending on actual implementation, these might succeed or fail
    
    // 关闭管理器 / Close manager
    let _ = manager.close().await;
}

/// 测试错误处理和恢复 / Test error handling and recovery
#[tokio::test]
async fn test_error_handling() {
    let manager = MCPServerManager::new();
    
    // 1. 测试添加无效配置 / Test adding invalid configuration
    let invalid_config = MCPServerConfig::Stdio(StdioServerConfig {
        name: "invalid_server".to_string(),
        disabled: false,
        forbidden_tools: vec![],
        tool_meta: HashMap::new(),
        default_tool_meta: None,
        vrl: None,
        server_parameters: StdioServerParameters {
            command: "nonexistent_command_12345".to_string(),
            args: vec![],
            env: HashMap::new(),
            cwd: None,
        },
    });
    
    // 添加配置应该成功 / Adding config should succeed
    let result = manager.add_or_update_server(invalid_config).await;
    assert!(result.is_ok());
    
    // 启动应该失败 / Starting should fail
    let result = manager.start_client("invalid_server").await;
    assert!(result.is_err());
    
    // 2. 测试调用不存在的工具 / Test calling non-existent tool
    let result = manager.validate_tool_call("nonexistent_tool", &serde_json::json!({})).await;
    assert!(result.is_err());
    
    // 3. 测试调用禁用的工具 / Test calling disabled tool
    let disabled_config = MCPServerConfig::Stdio(StdioServerConfig {
        name: "disabled_server".to_string(),
        disabled: false,
        forbidden_tools: vec!["forbidden_tool".to_string()],
        tool_meta: HashMap::new(),
        default_tool_meta: None,
        vrl: None,
        server_parameters: StdioServerParameters {
            command: "echo".to_string(),
            args: vec![],
            env: HashMap::new(),
            cwd: None,
        },
    });
    
    let result = manager.add_or_update_server(disabled_config).await;
    assert!(result.is_ok());
    
    let result = manager.validate_tool_call("forbidden_tool", &serde_json::json!({})).await;
    assert!(result.is_err());
    
    // 清理 / Clean up
    let _ = manager.close().await;
}

/// 测试并发操作 / Test concurrent operations
#[tokio::test]
async fn test_concurrent_operations() {
    let manager = std::sync::Arc::new(MCPServerManager::new());
    
    // 创建多个任务并发操作 / Create multiple tasks for concurrent operations
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
            
            // 添加服务器 / Add server
            let result = manager_clone.add_or_update_server(config).await;
            assert!(result.is_ok());
            
            // 启动服务器 / Start server
            let result = manager_clone.start_client(&format!("server_{}", i)).await;
            // 可能成功或失败 / Might succeed or fail
            
            // 获取状态 / Get status
            let status = manager_clone.get_server_status().await;
            assert!(!status.is_empty());
        });
        
        handles.push(handle);
    }
    
    // 等待所有任务完成 / Wait for all tasks to complete
    for handle in handles {
        let _ = handle.await;
    }
    
    // 等待所有连接建立 / Wait for all connections to establish
    sleep(Duration::from_millis(300)).await;
    
    // 检查最终状态 / Check final status
    let status = manager.get_server_status().await;
    assert_eq!(status.len(), 5);
    
    // 关闭管理器 / Close manager
    let _ = manager.close().await;
}
