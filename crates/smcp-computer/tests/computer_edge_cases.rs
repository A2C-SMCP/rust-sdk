use smcp_computer::{
    computer::{Computer, SilentSession},
    mcp_clients::model::{
        MCPServerConfig, MCPServerInput, McpChangeKind, McpServerNotification, PromptStringInput,
        StdioServerConfig, StdioServerParameters,
    },
};
/**
* 文件名: computer_edge_cases
* 作者: JQQ
* 创建日期: 2025/12/16
* 最后修改日期: 2025/12/16
* 版权: 2023 JQQ. All rights reserved.
* 依赖: tokio, smcp-computer
* 描述: Computer模块边界条件和并发测试
*/
use std::collections::HashMap;
use std::sync::Arc;
use tempfile::TempDir;

/// INT-01 #68：boot_up 起 FS 副作用（建 ~/.a2c/.blobspool + watch ~/.a2c/skills）→ 隔离到 TempDir。
/// Isolate boot_up's FS side-effects to a TempDir so tests never touch the real home。
fn isolate_boot(c: Computer<SilentSession>, td: &TempDir) -> Computer<SilentSession> {
    // #113 S6：add/remove_server 现落盘到 config_dir（缺省进程 cwd）→ 隔离到 TempDir，避免污染仓库工作树。
    c.with_skill_home(td.path().join("skills"))
        .with_blob_cache_root(td.path().join("blob"))
        .with_config_dir(td.path().join("config"))
}

#[tokio::test]
async fn test_computer_concurrent_input_operations() {
    let session = SilentSession::new("test");
    let computer = Arc::new(Computer::new(
        "test_computer",
        session,
        None,
        None,
        false,
        false,
    ));

    // 并发添加多个inputs / Concurrently add multiple inputs
    let mut handles = vec![];
    for i in 0..10 {
        let computer_clone = Arc::clone(&computer);
        let handle = tokio::spawn(async move {
            let input = MCPServerInput::PromptString(PromptStringInput {
                id: format!("input_{}", i),
                description: format!("Input {}", i),
                default: Some(format!("default_{}", i)),
                password: Some(false),
            });

            computer_clone.add_or_update_input(input).await.unwrap();
        });
        handles.push(handle);
    }

    // 等待所有任务完成 / Wait for all tasks to complete
    for handle in handles {
        handle.await.unwrap();
    }

    // 验证所有inputs都被添加 / Verify all inputs were added
    let inputs = computer.list_inputs().await.unwrap();
    assert_eq!(inputs.len(), 10);

    // 并发删除inputs / Concurrently remove inputs
    let mut handles = vec![];
    for i in 0..5 {
        let computer_clone = Arc::clone(&computer);
        let handle = tokio::spawn(async move {
            computer_clone
                .remove_input(&format!("input_{}", i))
                .await
                .unwrap();
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.await.unwrap();
    }

    let inputs = computer.list_inputs().await.unwrap();
    assert_eq!(inputs.len(), 5);
}

#[tokio::test]
async fn test_computer_edge_case_inputs() {
    let session = SilentSession::new("test");
    let computer = Computer::new("test_computer", session, None, None, false, false);

    // 测试空字符串ID / Test empty string ID
    let empty_input = MCPServerInput::PromptString(PromptStringInput {
        id: "".to_string(),
        description: "Empty ID".to_string(),
        default: None,
        password: Some(false),
    });

    computer.add_or_update_input(empty_input).await.unwrap();
    let retrieved = computer.get_input("").await.unwrap();
    assert!(retrieved.is_some());

    // 测试超长ID / Test very long ID
    let long_id = "a".repeat(10000);
    let long_input = MCPServerInput::PromptString(PromptStringInput {
        id: long_id.clone(),
        description: "Long ID".to_string(),
        default: None,
        password: Some(false),
    });

    computer.add_or_update_input(long_input).await.unwrap();
    let retrieved = computer.get_input(&long_id).await.unwrap();
    assert!(retrieved.is_some());

    // 测试特殊字符ID / Test special character ID
    let special_id = "!@#$%^&*()_+-=[]{}|;':\",./<>?".to_string();
    let special_input = MCPServerInput::PromptString(PromptStringInput {
        id: special_id.clone(),
        description: "Special chars".to_string(),
        default: None,
        password: Some(false),
    });

    computer.add_or_update_input(special_input).await.unwrap();
    let retrieved = computer.get_input(&special_id).await.unwrap();
    assert!(retrieved.is_some());

    // 测试Unicode ID / Test Unicode ID
    let unicode_id = "测试输入_🚀_αβγ".to_string();
    let unicode_input = MCPServerInput::PromptString(PromptStringInput {
        id: unicode_id.clone(),
        description: "Unicode".to_string(),
        default: None,
        password: Some(false),
    });

    computer.add_or_update_input(unicode_input).await.unwrap();
    let retrieved = computer.get_input(&unicode_id).await.unwrap();
    assert!(retrieved.is_some());
}

#[tokio::test]
async fn test_computer_edge_case_servers() {
    let td = TempDir::new().unwrap();
    let session = SilentSession::new("test");
    let computer = isolate_boot(
        Computer::new("test_computer", session, None, None, false, false),
        &td,
    );

    computer.boot_up().await.unwrap();

    // 测试空服务器名称 / Test empty server name
    let empty_server = MCPServerConfig::Stdio(StdioServerConfig {
        env_file: None,
        name: "".to_string(),
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

    // 应该能添加空名称服务器
    // Should be able to add empty name server
    computer.add_or_update_server(empty_server).await.unwrap();

    // 测试超长服务器名称 / Test very long server name
    let long_name = "a".repeat(10000);
    let long_server = MCPServerConfig::Stdio(StdioServerConfig {
        env_file: None,
        name: long_name.clone(),
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

    computer.add_or_update_server(long_server).await.unwrap();
    computer.remove_server(&long_name).await.unwrap();

    // 测试特殊字符服务器名称 / Test special character server name
    let special_name = "!@#$%^&*()".to_string();
    let special_server = MCPServerConfig::Stdio(StdioServerConfig {
        env_file: None,
        name: special_name.clone(),
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

    computer.add_or_update_server(special_server).await.unwrap();
    computer.remove_server(&special_name).await.unwrap();

    computer.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_computer_multiple_boot_up() {
    let td = TempDir::new().unwrap();
    let session = SilentSession::new("test");
    let computer = isolate_boot(
        Computer::new("test_computer", session, None, None, false, false),
        &td,
    );

    // 第一次启动 / First boot up
    computer.boot_up().await.unwrap();

    // 第二次启动应该成功（可能重置状态）
    // Second boot up should succeed (might reset state)
    computer.boot_up().await.unwrap();

    // 第三次启动 / Third boot up
    computer.boot_up().await.unwrap();

    computer.shutdown().await.unwrap();

    // 关闭后再次启动 / Boot up after shutdown
    computer.boot_up().await.unwrap();
    computer.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_computer_clone_behavior() {
    let session = SilentSession::new("test");
    let computer = Computer::new("test_computer", session, None, None, false, false);

    // 添加一个input / Add an input
    let input = MCPServerInput::PromptString(PromptStringInput {
        id: "test".to_string(),
        description: "Test".to_string(),
        default: Some("default".to_string()),
        password: Some(false),
    });
    computer.add_or_update_input(input).await.unwrap();

    // 克隆Computer需要Session实现Clone / Clone Computer requires Session to implement Clone
    // SilentSession没有实现Clone，所以不能测试克隆
    // SilentSession doesn't implement Clone, so cannot test cloning
    // let cloned = computer.clone();
    // assert_eq!(computer.name, cloned.name);
}

#[tokio::test]
async fn test_computer_batch_update_inputs() {
    let session = SilentSession::new("test");
    let computer = Computer::new("test_computer", session, None, None, false, false);

    // 准备批量inputs / Prepare batch inputs
    let mut inputs = HashMap::new();
    for i in 0..5 {
        inputs.insert(
            format!("input_{}", i),
            MCPServerInput::PromptString(PromptStringInput {
                id: format!("input_{}", i),
                description: format!("Input {}", i),
                default: Some(format!("default_{}", i)),
                password: Some(false),
            }),
        );
    }

    // 批量更新 / Batch update
    computer.update_inputs(inputs).await.unwrap();

    // 验证所有inputs都被添加 / Verify all inputs were added
    let retrieved_inputs = computer.list_inputs().await.unwrap();
    assert_eq!(retrieved_inputs.len(), 5);

    // 再次批量更新（替换所有）
    // Batch update again (replace all)
    let mut new_inputs = HashMap::new();
    new_inputs.insert(
        "new_input".to_string(),
        MCPServerInput::PromptString(PromptStringInput {
            id: "new_input".to_string(),
            description: "New input".to_string(),
            default: None,
            password: Some(false),
        }),
    );

    computer.update_inputs(new_inputs).await.unwrap();

    let retrieved_inputs = computer.list_inputs().await.unwrap();
    assert_eq!(retrieved_inputs.len(), 1);
    assert_eq!(retrieved_inputs[0].id(), "new_input");
}

#[tokio::test]
async fn test_computer_handle_mcp_notification_no_deps() {
    // #106：未 boot（mcp_manager 为 None）、无 Socket.IO 客户端时，handle_mcp_notification 应对三类通知
    // 均安全 no-op（reactor 的 manager.upgrade()/read() 命中 None → 跳过；emit 无 client → no-op），不 panic。
    let session = SilentSession::new("test");
    let computer = Computer::new("test_computer", session, None, None, false, false);

    computer
        .handle_mcp_notification(McpServerNotification {
            server: "srv".to_string(),
            kind: McpChangeKind::ToolListChanged,
        })
        .await;

    computer
        .handle_mcp_notification(McpServerNotification {
            server: "srv".to_string(),
            kind: McpChangeKind::ResourceListChanged,
        })
        .await;

    computer
        .handle_mcp_notification(McpServerNotification {
            server: "srv".to_string(),
            kind: McpChangeKind::ResourceUpdated {
                uri: "window://1".to_string(),
            },
        })
        .await;
    // 到达此处即证明三类通知在无依赖时均安全返回。
}

#[tokio::test]
async fn test_computer_large_scale_operations() {
    let session = SilentSession::new("test");
    let computer = Computer::new("test_computer", session, None, None, false, false);

    // 添加大量inputs / Add large number of inputs
    let mut inputs = HashMap::new();
    for i in 0..1000 {
        inputs.insert(
            format!("input_{}", i),
            MCPServerInput::PromptString(PromptStringInput {
                id: format!("input_{}", i),
                description: format!("Input {}", i),
                default: Some(format!("default_{}", i)),
                password: Some(i % 2 == 0),
            }),
        );
    }

    computer.update_inputs(inputs).await.unwrap();

    // 验证所有inputs都被正确存储 / Verify all inputs are correctly stored
    let retrieved_inputs = computer.list_inputs().await.unwrap();
    assert_eq!(retrieved_inputs.len(), 1000);

    // 测试随机访问 / Test random access
    for i in [0, 100, 500, 999] {
        let input = computer.get_input(&format!("input_{}", i)).await.unwrap();
        assert!(input.is_some());
    }

    // 批量删除 / Batch delete
    for i in 0..500 {
        computer
            .remove_input(&format!("input_{}", i))
            .await
            .unwrap();
    }

    let remaining_inputs = computer.list_inputs().await.unwrap();
    assert_eq!(remaining_inputs.len(), 500);
}

#[tokio::test]
async fn test_computer_error_edge_cases() {
    let session = SilentSession::new("test");
    let computer = Computer::new("test_computer", session, None, None, false, false);

    // 测试获取不存在工具时的错误类型
    // Test error type when getting non-existent tool
    let result = computer
        .execute_tool(
            "test_req",
            "",
            serde_json::json!({}),
            Some(-1.0), // 负数超时 / Negative timeout
        )
        .await;

    assert!(result.is_err());

    // 测试空工具名称 / Test empty tool name
    let result = computer
        .execute_tool("test_req", "", serde_json::json!({}), None)
        .await;

    assert!(result.is_err());

    // 测试极大超时值 / Test very large timeout value
    let result = computer
        .execute_tool(
            "test_req",
            "non_existent",
            serde_json::json!({}),
            Some(f64::MAX),
        )
        .await;

    assert!(result.is_err());
}
