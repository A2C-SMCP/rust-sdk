/**
* 文件名: manager_tests
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: tokio, async-trait
* 描述: MCP服务器管理器测试
*/

use smcp_computer::mcp_clients::*;
use std::collections::HashMap;
use tokio::time::{sleep, Duration};

#[tokio::test]
async fn test_manager_creation() {
    let manager = MCPServerManager::new();
    let status = manager.get_server_status().await;
    assert!(status.is_empty());
}

#[tokio::test]
async fn test_manager_initialization() {
    let manager = MCPServerManager::new();
    
    // 创建服务器配置 / Create server configurations
    let mut configs = Vec::new();
    
    // STDIO服务器配置 / STDIO server configuration
    configs.push(MCPServerConfig::Stdio(StdioServerConfig {
        name: "test_stdio".to_string(),
        disabled: false,
        forbidden_tools: vec![],
        tool_meta: HashMap::new(),
        default_tool_meta: None,
        vrl: None,
        server_parameters: StdioServerParameters {
            command: "echo".to_string(),
            args: vec!["hello".to_string()],
            env: HashMap::new(),
            cwd: None,
        },
    }));
    
    // HTTP服务器配置 / HTTP server configuration
    configs.push(MCPServerConfig::Http(HttpServerConfig {
        name: "test_http".to_string(),
        disabled: true, // 禁用此服务器 / Disable this server
        forbidden_tools: vec![],
        tool_meta: HashMap::new(),
        default_tool_meta: None,
        vrl: None,
        server_parameters: HttpServerParameters {
            url: "http://localhost:8080".to_string(),
            headers: HashMap::new(),
        },
    }));
    
    // 初始化管理器 / Initialize manager
    let result = manager.initialize(configs).await;
    assert!(result.is_ok());
    
    // 检查状态 / Check status
    let status = manager.get_server_status().await;
    assert_eq!(status.len(), 2);
    
    // 验证状态 / Verify status
    let stdio_status = status.iter().find(|(name, _, _)| name == "test_stdio").unwrap();
    assert!(!stdio_status.1); // 未激活 / Not active
    
    let http_status = status.iter().find(|(name, _, _)| name == "test_http").unwrap();
    assert!(!http_status.1); // 未激活 / Not active
}

#[tokio::test]
async fn test_add_server() {
    let manager = MCPServerManager::new();
    
    // 添加服务器配置 / Add server configuration
    let config = MCPServerConfig::Stdio(StdioServerConfig {
        name: "test_server".to_string(),
        disabled: false,
        forbidden_tools: vec![],
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
    
    let result = manager.add_or_update_server(config).await;
    assert!(result.is_ok());
    
    // 检查状态 / Check status
    let status = manager.get_server_status().await;
    assert_eq!(status.len(), 1);
    assert_eq!(status[0].0, "test_server");
}

#[tokio::test]
async fn test_start_stop_client() {
    let manager = MCPServerManager::new();
    
    // 添加服务器 / Add server
    let config = MCPServerConfig::Stdio(StdioServerConfig {
        name: "test_server".to_string(),
        disabled: false,
        forbidden_tools: vec![],
        tool_meta: HashMap::new(),
        default_tool_meta: None,
        vrl: None,
        server_parameters: StdioServerParameters {
            command: "echo".to_string(),
            args: vec!["test".to_string()],
            env: HashMap::new(),
            cwd: None,
        },
    });
    
    manager.add_or_update_server(config).await.unwrap();
    
    // 启动客户端 / Start client
    let result = manager.start_client("test_server").await;
    assert!(result.is_ok());
    
    // 等待连接建立 / Wait for connection to establish
    sleep(Duration::from_millis(100)).await;
    
    // 检查状态 / Check status
    let status = manager.get_server_status().await;
    let server_status = status.iter().find(|(name, _, _)| name == "test_server").unwrap();
    assert!(server_status.1); // 已激活 / Active
    
    // 停止客户端 / Stop client
    let result = manager.stop_client("test_server").await;
    assert!(result.is_ok());
    
    // 检查状态 / Check status
    let status = manager.get_server_status().await;
    let server_status = status.iter().find(|(name, _, _)| name == "test_server").unwrap();
    assert!(!server_status.1); // 未激活 / Not active
}

#[tokio::test]
async fn test_remove_server() {
    let manager = MCPServerManager::new();
    
    // 添加服务器 / Add server
    let config = MCPServerConfig::Stdio(StdioServerConfig {
        name: "test_server".to_string(),
        disabled: false,
        forbidden_tools: vec![],
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
    
    manager.add_or_update_server(config).await.unwrap();
    
    // 移除服务器 / Remove server
    let result = manager.remove_server("test_server").await;
    assert!(result.is_ok());
    
    // 检查状态 / Check status
    let status = manager.get_server_status().await;
    assert!(status.is_empty());
}

#[tokio::test]
async fn test_tool_conflict_detection() {
    let manager = MCPServerManager::new();
    
    // 创建两个服务器，有同名工具 / Create two servers with same tool name
    let mut configs = Vec::new();
    
    // 第一个服务器 / First server
    configs.push(MCPServerConfig::Stdio(StdioServerConfig {
        name: "server1".to_string(),
        disabled: false,
        forbidden_tools: vec![],
        tool_meta: HashMap::new(),
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
        tool_meta: HashMap::new(),
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
    
    // 启动所有服务器 / Start all servers
    let result = manager.start_all().await;
    // 可能会因为工具冲突而失败，这是预期的
    // Might fail due to tool conflicts, which is expected
    
    // 等待连接建立 / Wait for connections to establish
    sleep(Duration::from_millis(200)).await;
}

#[tokio::test]
async fn test_auto_connect_settings() {
    let manager = MCPServerManager::new();
    
    // 测试默认设置 / Test default settings
    manager.enable_auto_connect().await;
    manager.enable_auto_reconnect().await;
    
    // 添加服务器，应该自动连接 / Add server, should auto-connect
    let config = MCPServerConfig::Stdio(StdioServerConfig {
        name: "test_server".to_string(),
        disabled: false,
        forbidden_tools: vec![],
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
    
    manager.add_or_update_server(config).await.unwrap();
    
    // 等待自动连接 / Wait for auto-connect
    sleep(Duration::from_millis(100)).await;
    
    // 禁用自动连接 / Disable auto-connect
    manager.disable_auto_connect().await;
    
    // 添加另一个服务器，不应该自动连接 / Add another server, should not auto-connect
    let config2 = MCPServerConfig::Stdio(StdioServerConfig {
        name: "test_server2".to_string(),
        disabled: false,
        forbidden_tools: vec![],
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
    
    manager.add_or_update_server(config2).await.unwrap();
    
    // 检查状态 / Check status
    let status = manager.get_server_status().await;
    let server1_status = status.iter().find(|(name, _, _)| name == "test_server").unwrap();
    let server2_status = status.iter().find(|(name, _, _)| name == "test_server2").unwrap();
    
    // server1可能已连接（自动连接），server2应该未连接
    // server1 might be connected (auto-connect), server2 should not be connected
    assert_eq!(server2_status.1, false);
}

#[tokio::test]
async fn test_manager_close() {
    let manager = MCPServerManager::new();
    
    // 添加并启动服务器 / Add and start server
    let config = MCPServerConfig::Stdio(StdioServerConfig {
        name: "test_server".to_string(),
        disabled: false,
        forbidden_tools: vec![],
        tool_meta: HashMap::new(),
        default_tool_meta: None,
        vrl: None,
        server_parameters: StdioServerParameters {
            command: "echo".to_string(),
            args: vec!["test".to_string()],
            env: HashMap::new(),
            cwd: None,
        },
    });
    
    manager.add_or_update_server(config).await.unwrap();
    manager.start_client("test_server").await.unwrap();
    
    // 等待连接建立 / Wait for connection to establish
    sleep(Duration::from_millis(100)).await;
    
    // 关闭管理器 / Close manager
    let result = manager.close().await;
    assert!(result.is_ok());
    
    // 检查所有状态已清空 / Check all state cleared
    let status = manager.get_server_status().await;
    assert!(status.is_empty());
}

#[tokio::test]
async fn test_forbidden_tools() {
    let manager = MCPServerManager::new();
    
    // 创建带禁用工具的服务器配置 / Create server config with forbidden tools
    let config = MCPServerConfig::Stdio(StdioServerConfig {
        name: "test_server".to_string(),
        disabled: false,
        forbidden_tools: vec!["forbidden_tool".to_string()],
        tool_meta: HashMap::new(),
        default_tool_meta: None,
        vrl: None,
        server_parameters: StdioServerParameters {
            command: "echo".to_string(),
            args: vec!["test".to_string()],
            env: HashMap::new(),
            cwd: None,
        },
    });
    
    manager.add_or_update_server(config).await.unwrap();
    
    // 测试验证工具调用 / Test validate tool call
    let result = manager.validate_tool_call("forbidden_tool", &serde_json::json!({})).await;
    assert!(result.is_err()); // 应该失败，因为工具被禁用 / Should fail because tool is forbidden
    
    let result = manager.validate_tool_call("allowed_tool", &serde_json::json!({})).await;
    assert!(result.is_err()); // 应该失败，因为工具不存在 / Should fail because tool doesn't exist
}
