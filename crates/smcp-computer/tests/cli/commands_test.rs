// Enable the cli feature for tests
#![cfg(feature = "cli")]

/*!
* 文件名: commands_test.rs
* 作者: JQQ
* 创建日期: 2025/12/16
* 最后修改日期: 2025/12/16
* 版权: 2023 JQQ. All rights reserved.
* 依赖: tokio, smcp-computer, futures
* 描述: CLI 命令集成测试 / CLI command integration tests
*/

use std::time::Duration;
use tokio::time::timeout;
use common::{
    create_command_handler, 
    create_uninitialized_command_handler,
    create_test_server_config_file,
    create_test_inputs_config_file,
    create_test_complete_config_file,
    with_timeout,
};

mod common;

/// 测试完整的命令工作流 / Test complete command workflow
#[tokio::test]
async fn test_complete_command_workflow() {
    let mut handler = create_uninitialized_command_handler().await;
    
    // 1. 测试初始状态 / Test initial state
    assert!(handler.show_status().await.is_ok());
    
    // 2. 添加服务器配置 / Add server config
    let server_config = r#"
    {
        "type": "stdio",
        "name": "workflow_test_server",
        "disabled": false,
        "forbidden_tools": [],
        "tool_meta": {},
        "default_tool_meta": null,
        "vrl": null,
        "server_parameters": {
            "command": "echo",
            "args": ["workflow"],
            "env": {},
            "cwd": null
        }
    }
    "#;
    assert!(handler.add_server(server_config).await.is_ok());
    
    // 3. 从文件加载 inputs / Load inputs from file
    let inputs_file = create_test_inputs_config_file();
    assert!(handler.load_inputs(inputs_file.path()).await.is_ok());
    
    // 4. 显示 MCP 配置 / Show MCP config
    assert!(handler.show_mcp_config().await.is_ok());
    
    // 5. 列出 inputs / List inputs
    assert!(handler.list_inputs().await.is_ok());
    
    // 6. 移除服务器 / Remove server
    assert!(handler.remove_server("workflow_test_server").await.is_ok());
}

/// 测试服务器管理命令 / Test server management commands
#[tokio::test]
async fn test_server_management_commands() {
    let mut handler = create_uninitialized_command_handler().await;
    
    // 添加多个服务器 / Add multiple servers
    let servers = vec![
        ("server1", "echo", "arg1"),
        ("server2", "echo", "arg2"),
        ("server3", "echo", "arg3"),
    ];
    
    for (name, command, arg) in &servers {
        let config = format!(r#"
        {{
            "type": "stdio",
            "name": "{}",
            "disabled": false,
            "forbidden_tools": [],
            "tool_meta": {{}},
            "default_tool_meta": null,
            "vrl": null,
            "server_parameters": {{
                "command": "{}",
                "args": ["{}"],
                "env": {{}},
                "cwd": null
            }}
        }}
        "#, name, command, arg);
        
        assert!(handler.add_server(&config).await.is_ok());
    }
    
    // 移除服务器 / Remove servers
    assert!(handler.remove_server("server2").await.is_ok());
    assert!(handler.remove_server("non_existent").await.is_ok()); // 应该成功即使不存在
    
    // 尝试启动（未初始化） / Try to start (uninitialized)
    assert!(handler.start_client("all").await.is_ok());
    assert!(handler.start_client("server1").await.is_ok());
    
    // 停止服务器 / Stop servers
    assert!(handler.stop_client("all").await.is_ok());
    assert!(handler.stop_client("server1").await.is_ok());
}

/// 测试文件加载命令 / Test file loading commands
#[tokio::test]
async fn test_file_loading_commands() {
    let mut handler = create_uninitialized_command_handler().await;
    
    // 从文件加载单个服务器 / Load single server from file
    let server_file = create_test_server_config_file();
    let config_path = format!("@{}", server_file.path().display());
    assert!(handler.add_server(&config_path).await.is_ok());
    
    // 从文件加载 inputs / Load inputs from file
    let inputs_file = create_test_inputs_config_file();
    assert!(handler.load_inputs(inputs_file.path()).await.is_ok());
    
    // 加载完整配置 / Load complete config
    let config_file = create_test_complete_config_file();
    assert!(handler.load_config(config_file.path()).await.is_ok());
}

/// 测试错误处理 / Test error handling
#[tokio::test]
async fn test_command_error_handling() {
    let mut handler = create_uninitialized_command_handler().await;
    
    // 无效 JSON / Invalid JSON
    assert!(handler.add_server("{ invalid json }").await.is_err());
    
    // 无效服务器类型 / Invalid server type
    assert!(handler.add_server(r#"{"type": "invalid", "name": "test"}"#).await.is_err());
    
    // 不存在的文件 / Non-existent file
    assert!(handler.load_inputs("/non/existent/file.json").await.is_err());
    
    // 空文件路径 / Empty file path
    let config_path = "@/non/existent/file.json";
    assert!(handler.add_server(config_path).await.is_err());
}

/// 测试历史记录功能 / Test history functionality
#[tokio::test]
async fn test_history_functionality() {
    let handler = create_command_handler().await;
    
    // 初始历史为空 / Initial history is empty
    assert!(handler.show_history(Some(5)).await.is_ok());
    
    // 测试不同数量的历史记录 / Test different history counts
    assert!(handler.show_history(None).await.is_ok()); // 默认 10 条
    assert!(handler.show_history(Some(0)).await.is_ok());
    assert!(handler.show_history(Some(100)).await.is_ok()); // 超过实际数量
}

/// 测试工具列表功能 / Test tools listing functionality
#[tokio::test]
async fn test_tools_listing() {
    let handler = create_command_handler().await;
    
    // 列出工具（可能为空） / List tools (might be empty)
    let result = handler.list_tools().await;
    assert!(result.is_ok());
}

/// 测试桌面信息获取 / Test desktop info retrieval
#[tokio::test]
async fn test_desktop_info() {
    let handler = create_command_handler().await;
    
    // 测试不同参数组合 / Test different parameter combinations
    assert!(handler.get_desktop(None, None).await.is_ok());
    assert!(handler.get_desktop(Some(5), None).await.is_ok());
    assert!(handler.get_desktop(None, Some("test://uri")).await.is_ok());
    assert!(handler.get_desktop(Some(10), Some("test://uri")).await.is_ok());
}

/// 测试 SocketIO 连接命令 / Test SocketIO connection commands
#[tokio::test]
async fn test_socketio_commands() {
    let mut handler = create_uninitialized_command_handler().await;
    
    // 测试连接参数 / Test connection parameters
    assert!(handler.connect_socketio(
        "http://localhost:3000",
        "/test",
        &Some("auth_token".to_string()),
        &Some("header:value".to_string()),
    ).await.is_ok());
    
    // 测试空参数 / Test empty parameters
    assert!(handler.connect_socketio(
        "http://localhost:3000",
        "/test",
        &None,
        &None,
    ).await.is_ok());
}

/// 测试并发操作 / Test concurrent operations
#[tokio::test]
async fn test_concurrent_operations() {
    let server_configs: Vec<String> = (0..5).map(|i| {
        format!(r#"
        {{
            "type": "stdio",
            "name": "concurrent_server_{}",
            "disabled": false,
            "forbidden_tools": [],
            "tool_meta": {{}},
            "default_tool_meta": null,
            "vrl": null,
            "server_parameters": {{
                "command": "echo",
                "args": ["{}"],
                "env": {{}},
                "cwd": null
            }}
        }}
        "#, i, i)
    }).collect();
    
    // 为每个配置创建独立的 handler / Create separate handler for each config
    let futures: Vec<_> = server_configs.into_iter()
        .map(|config| {
            async move {
                let mut handler = create_uninitialized_command_handler().await;
                timeout(Duration::from_secs(5), handler.add_server(&config)).await
            }
        })
        .collect();
    
    let results = futures::future::join_all(futures).await;
    for result in results {
        assert!(result.is_ok()); // 超时检查
        assert!(result.unwrap().is_ok()); // 操作成功
    }
}

/// 测试大量配置 / Test large configurations
#[tokio::test]
async fn test_large_configurations() {
    let mut handler = create_uninitialized_command_handler().await;
    
    // 创建大量 inputs / Create many inputs
    let mut inputs_json = String::from("[");
    for i in 0..100 {
        if i > 0 {
            inputs_json.push_str(",");
        }
        inputs_json.push_str(&format!(r#"
        {{
            "type": "prompt_string",
            "id": "large_input_{}",
            "description": "Large test input {}",
            "default": "default_{}",
            "password": false
        }}
        "#, i, i, i));
    }
    inputs_json.push_str("]");
    
    // 写入临时文件 / Write to temp file
    let temp_file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(temp_file.path(), inputs_json).unwrap();
    
    // 加载并验证 / Load and verify
    let result = with_timeout!(handler.load_inputs(temp_file.path())).await;
    assert!(result.is_ok());
    
    // 列出 inputs / List inputs
    let result = with_timeout!(handler.list_inputs()).await;
    assert!(result.is_ok());
}
