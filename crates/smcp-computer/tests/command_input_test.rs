/*!
* 文件名: command_input_test
* 作者: JQQ
* 创建日期: 2025/12/16
* 最后修改日期: 2025/12/16
* 版权: 2023 JQQ. All rights reserved.
* 依赖: tokio, serde_json
* 描述: 测试 Command 输入类型的命令执行功能
*/

use smcp_computer::computer::{Computer, SilentSession, Session};
use smcp_computer::mcp_clients::model::{MCPServerInput, CommandInput};
use std::collections::HashMap;

#[tokio::test]
async fn test_command_input_execution() {
    let session = SilentSession::new("test");
    
    // 测试简单命令 / Test simple command
    let command_input = MCPServerInput::Command(CommandInput {
        id: "test_cmd".to_string(),
        description: "Test command".to_string(),
        command: "echo hello world".to_string(),
        args: None,
    });
    
    let result = session.resolve_input(&command_input).await.unwrap();
    assert_eq!(result, serde_json::Value::String("hello world".to_string()));
}

#[tokio::test]
async fn test_command_input_with_args() {
    let session = SilentSession::new("test");
    
    // 测试带参数的命令 / Test command with arguments
    let mut args = HashMap::new();
    args.insert("arg1".to_string(), "hello".to_string());
    args.insert("arg2".to_string(), "world".to_string());
    
    let command_input = MCPServerInput::Command(CommandInput {
        id: "test_cmd_args".to_string(),
        description: "Test command with args".to_string(),
        command: "echo".to_string(),
        args: Some(args),
    });
    
    let result = session.resolve_input(&command_input).await.unwrap();
    assert_eq!(result, serde_json::Value::String("hello world".to_string()));
}

#[tokio::test]
async fn test_command_input_failure() {
    let session = SilentSession::new("test");
    
    // 测试不存在的命令 / Test non-existent command
    let command_input = MCPServerInput::Command(CommandInput {
        id: "test_fail".to_string(),
        description: "Test failing command".to_string(),
        command: "nonexistent_command_12345".to_string(),
        args: None,
    });
    
    let result = session.resolve_input(&command_input).await;
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("Failed to execute command"));
}

#[tokio::test]
async fn test_command_input_with_computer() {
    let session = SilentSession::new("test");
    let computer = Computer::new(
        "test_computer",
        session,
        None,
        None,
        true,
        true,
    );
    
    // 添加命令输入 / Add command input
    let command_input = MCPServerInput::Command(CommandInput {
        id: "pwd_cmd".to_string(),
        description: "Get current directory".to_string(),
        command: "pwd".to_string(),
        args: None,
    });
    
    computer.add_or_update_input(command_input).await.unwrap();
    
    // 验证输入已添加 / Verify input is added
    let retrieved = computer.get_input("pwd_cmd").await.unwrap();
    assert!(retrieved.is_some());
    
    match retrieved.unwrap() {
        MCPServerInput::Command(input) => {
            assert_eq!(input.id, "pwd_cmd");
            assert_eq!(input.command, "pwd");
        }
        _ => panic!("Expected Command input"),
    }
}

#[tokio::test]
async fn test_command_input_complex() {
    let session = SilentSession::new("test");
    
    // 测试复杂命令 / Test complex command
    let command_input = MCPServerInput::Command(CommandInput {
        id: "complex_cmd".to_string(),
        description: "Complex command with pipes".to_string(),
        command: "echo 'test line 1\nline 2' | wc -l".to_string(),
        args: None,
    });
    
    let result = session.resolve_input(&command_input).await.unwrap();
    // Should return 2 (number of lines)
    assert_eq!(result, serde_json::Value::String("2".to_string()));
}
