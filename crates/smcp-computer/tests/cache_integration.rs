/**
* 文件名: cache_integration
* 作者: JQQ
* 创建日期: 2025/12/16
* 最后修改日期: 2025/12/16
* 版权: 2023 JQQ. All rights reserved.
* 依赖: tokio, serde_json, std::collections::HashMap
* 描述: 缓存功能的集成测试
*/
use smcp_computer::computer::{Computer, SilentSession};
use smcp_computer::mcp_clients::model::{MCPServerInput, PromptStringInput, PickStringInput, CommandInput};
use tokio::time::{timeout, Duration};

#[tokio::test]
async fn test_cache_with_multiple_inputs() {
    let session = SilentSession::new("test");
    let computer = Computer::new(
        "test_computer",
        session,
        None,
        None,
        true,
        true,
    );
    
    // 添加多种类型的 inputs / Add multiple types of inputs
    let inputs = vec![
        MCPServerInput::PromptString(PromptStringInput {
            id: "prompt1".to_string(),
            description: "Prompt input 1".to_string(),
            default: Some("default1".to_string()),
            password: Some(false),
        }),
        MCPServerInput::PickString(PickStringInput {
            id: "pick1".to_string(),
            description: "Pick input 1".to_string(),
            options: vec!["opt1".to_string(), "opt2".to_string()],
            default: Some("opt1".to_string()),
        }),
        MCPServerInput::Command(CommandInput {
            id: "cmd1".to_string(),
            description: "Command input 1".to_string(),
            command: "echo test".to_string(),
            args: None,
        }),
    ];
    
    for input in inputs {
        computer.add_or_update_input(input).await.unwrap();
    }
    
    // 设置不同类型的缓存值 / Set different types of cache values
    computer.set_input_value("prompt1", serde_json::Value::String("cached_prompt".to_string())).await.unwrap();
    computer.set_input_value("pick1", serde_json::Value::String("opt2".to_string())).await.unwrap();
    computer.set_input_value("cmd1", serde_json::Value::String("cached_output".to_string())).await.unwrap();
    
    // 验证所有缓存值 / Verify all cache values
    let values = computer.list_input_values().await.unwrap();
    assert_eq!(values.len(), 3);
    assert_eq!(values.get("prompt1"), Some(&serde_json::Value::String("cached_prompt".to_string())));
    assert_eq!(values.get("pick1"), Some(&serde_json::Value::String("opt2".to_string())));
    assert_eq!(values.get("cmd1"), Some(&serde_json::Value::String("cached_output".to_string())));
}

#[tokio::test]
async fn test_cache_concurrent_access() {
    let session = SilentSession::new("test");
    let computer = Computer::new(
        "test_computer",
        session,
        None,
        None,
        true,
        true,
    );
    
    // 添加 input / Add input
    let input = MCPServerInput::PromptString(PromptStringInput {
        id: "concurrent_input".to_string(),
        description: "Concurrent test input".to_string(),
        default: None,
        password: Some(false),
    });
    computer.add_or_update_input(input).await.unwrap();
    
    // 使用 Arc 来共享 Computer 实例 / Use Arc to share Computer instance
    use std::sync::Arc;
    let computer = Arc::new(computer);
    
    // 并发设置和获取缓存 / Concurrent set and get cache
    let computer_clone = computer.clone();
    let set_task = tokio::spawn(async move {
        for i in 0..10 {
            let value = serde_json::Value::String(format!("value_{}", i));
            computer_clone.set_input_value("concurrent_input", value).await.unwrap();
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });
    
    let computer_clone = computer.clone();
    let get_task = tokio::spawn(async move {
        let mut values = Vec::new();
        for _ in 0..10 {
            if let Some(value) = computer_clone.get_input_value("concurrent_input").await.unwrap() {
                values.push(value);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        values
    });
    
    // 等待任务完成 / Wait for tasks to complete
    timeout(Duration::from_secs(5), set_task).await.unwrap().unwrap();
    let retrieved_values = timeout(Duration::from_secs(5), get_task).await.unwrap().unwrap();
    
    // 验证至少获取到一些值 / Verify at least some values were retrieved
    assert!(!retrieved_values.is_empty());
}

#[tokio::test]
async fn test_cache_persistence_across_operations() {
    let session = SilentSession::new("test");
    let computer = Computer::new(
        "test_computer",
        session,
        None,
        None,
        true,
        true,
    );
    
    // 添加 input / Add input
    let input = MCPServerInput::PromptString(PromptStringInput {
        id: "persistent_input".to_string(),
        description: "Persistent test input".to_string(),
        default: None,
        password: Some(false),
    });
    computer.add_or_update_input(input).await.unwrap();
    
    // 设置缓存 / Set cache
    let test_value = serde_json::Value::String("persistent_value".to_string());
    computer.set_input_value("persistent_input", test_value.clone()).await.unwrap();
    
    // 执行各种操作 / Perform various operations
    let inputs = computer.list_inputs().await.unwrap();
    assert!(!inputs.is_empty());
    
    let input_def = computer.get_input("persistent_input").await.unwrap();
    assert!(input_def.is_some());
    
    // 验证缓存仍然存在 / Verify cache still exists
    let retrieved = computer.get_input_value("persistent_input").await.unwrap();
    assert_eq!(retrieved, Some(test_value));
}

#[tokio::test]
async fn test_cache_with_complex_values() {
    let session = SilentSession::new("test");
    let computer = Computer::new(
        "test_computer",
        session,
        None,
        None,
        true,
        true,
    );
    
    // 添加 inputs / Add inputs
    let inputs = vec![
        ("string_input", MCPServerInput::PromptString(PromptStringInput {
            id: "string_input".to_string(),
            description: "String input".to_string(),
            default: None,
            password: Some(false),
        })),
        ("number_input", MCPServerInput::PromptString(PromptStringInput {
            id: "number_input".to_string(),
            description: "Number input".to_string(),
            default: None,
            password: Some(false),
        })),
        ("float_input", MCPServerInput::PromptString(PromptStringInput {
            id: "float_input".to_string(),
            description: "Float input".to_string(),
            default: None,
            password: Some(false),
        })),
        ("bool_input", MCPServerInput::PromptString(PromptStringInput {
            id: "bool_input".to_string(),
            description: "Bool input".to_string(),
            default: None,
            password: Some(false),
        })),
    ];
    
    for (_id, input) in inputs {
        computer.add_or_update_input(input).await.unwrap();
    }
    
    // 设置复杂值 / Set complex values
    let complex_values = vec![
        ("string_input", serde_json::Value::String("complex string with spaces".to_string())),
        ("number_input", serde_json::Value::Number(serde_json::Number::from(123456789))),
        ("float_input", serde_json::Value::Number(serde_json::Number::from_f64(std::f64::consts::PI).unwrap())),
        ("bool_input", serde_json::Value::Bool(true)),
    ];
    
    for (id, value) in complex_values {
        computer.set_input_value(id, value).await.unwrap();
    }
    
    // 验证复杂值 / Verify complex values
    let values = computer.list_input_values().await.unwrap();
    assert_eq!(values.len(), 4);
    
    assert_eq!(
        values.get("string_input"),
        Some(&serde_json::Value::String("complex string with spaces".to_string()))
    );
    assert_eq!(
        values.get("number_input"),
        Some(&serde_json::Value::Number(serde_json::Number::from(123456789)))
    );
    assert_eq!(
        values.get("float_input"),
        Some(&serde_json::Value::Number(serde_json::Number::from_f64(std::f64::consts::PI).unwrap()))
    );
    assert_eq!(
        values.get("bool_input"),
        Some(&serde_json::Value::Bool(true))
    );
}

#[tokio::test]
async fn test_cache_edge_cases() {
    let session = SilentSession::new("test");
    let computer = Computer::new(
        "test_computer",
        session,
        None,
        None,
        true,
        true,
    );
    
    // 测试空缓存 / Test empty cache
    let empty_values = computer.list_input_values().await.unwrap();
    assert!(empty_values.is_empty());
    
    // 测试获取不存在的缓存 / Test getting non-existent cache
    let non_existent = computer.get_input_value("non_existent").await.unwrap();
    assert!(non_existent.is_none());
    
    // 测试删除不存在的缓存 / Test removing non-existent cache
    let removed = computer.remove_input_value("non_existent").await.unwrap();
    assert!(!removed);
    
    // 测试对不存在的 input 设置缓存 / Test setting cache for non-existent input
    let set_result = computer.set_input_value("non_existent", serde_json::Value::String("value".to_string())).await.unwrap();
    assert!(!set_result);
    
    // 添加 input 后测试 / Test after adding input
    let input = MCPServerInput::PromptString(PromptStringInput {
        id: "edge_case_input".to_string(),
        description: "Edge case input".to_string(),
        default: None,
        password: Some(false),
    });
    computer.add_or_update_input(input).await.unwrap();
    
    // 测试清空特定缓存 / Test clearing specific cache
    computer.set_input_value("edge_case_input", serde_json::Value::String("test".to_string())).await.unwrap();
    assert!(computer.get_input_value("edge_case_input").await.unwrap().is_some());
    
    computer.clear_input_values(Some("edge_case_input")).await.unwrap();
    assert!(computer.get_input_value("edge_case_input").await.unwrap().is_none());
    
    // 测试清空所有缓存 / Test clearing all cache
    computer.set_input_value("edge_case_input", serde_json::Value::String("test2".to_string())).await.unwrap();
    computer.clear_input_values(None).await.unwrap();
    let all_values = computer.list_input_values().await.unwrap();
    assert!(all_values.is_empty());
}
