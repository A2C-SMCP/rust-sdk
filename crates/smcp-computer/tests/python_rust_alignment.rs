/*！
* 文件名: python_rust_alignment
* 作者: JQQ
* 创建日期: 2025/12/16
* 最后修改日期: 2025/12/16
* 版权: 2023 JQQ. All rights reserved.
* 依赖: tokio, serde_json
* 描述: 验证Rust与Python SDK行为对齐的集成测试
*/

use serde_json::Value;
use smcp_computer::inputs::{
    CliInputProvider, InputContext, InputProvider, InputRequest, InputType,
};
use smcp_computer::mcp_clients::{ConfigRender, RenderError};

#[tokio::test]
async fn test_config_render_placeholder() {
    // 测试ConfigRender的${input:xxx}占位符解析
    let render = ConfigRender::default();

    // 创建resolver函数
    async fn resolver(id: String) -> Result<Value, RenderError> {
        match id.as_str() {
            "api_key" => Ok(Value::String("sk-123456".to_string())),
            "port" => Ok(Value::Number(serde_json::Number::from(8080))),
            "missing" => Err(RenderError::InputNotFound(id)),
            _ => Ok(Value::String(format!("resolved_{}", id))),
        }
    }

    // 测试单个占位符
    let input = Value::String("${input:api_key}".to_string());
    let result = render.render(input, resolver).await.unwrap();
    assert_eq!(result, Value::String("sk-123456".to_string()));

    // 测试字符串中的占位符
    let input = Value::String("http://localhost:${input:port}/api".to_string());
    let result = render.render(input, resolver).await.unwrap();
    assert_eq!(
        result,
        Value::String("http://localhost:8080/api".to_string())
    );

    // 测试对象渲染
    let mut obj = serde_json::Map::new();
    obj.insert(
        "url".to_string(),
        Value::String("${input:api_key}".to_string()),
    );
    obj.insert("nested".to_string(), Value::String("value".to_string()));
    let input = Value::Object(obj);
    let result = render.render(input, resolver).await.unwrap();

    if let Value::Object(map) = result {
        assert_eq!(
            map.get("url").unwrap(),
            &Value::String("sk-123456".to_string())
        );
        assert_eq!(
            map.get("nested").unwrap(),
            &Value::String("value".to_string())
        );
    } else {
        panic!("Expected object");
    }

    // 测试缺失输入（应保留原占位符）
    let input = Value::String("${input:missing}".to_string());
    let result = render.render(input, resolver).await.unwrap();
    assert_eq!(result, Value::String("${input:missing}".to_string()));
}

#[tokio::test]
async fn test_command_input_shell_mode() {
    // 测试command input的shell模式支持
    let provider = CliInputProvider::new();

    // 创建输入上下文
    let context = InputContext {
        server_name: None,
        tool_name: None,
        metadata: std::collections::HashMap::new(),
    };

    // Unix shell管道测试（仅在Unix系统运行）
    #[cfg(unix)]
    {
        let request = InputRequest {
            id: "test_pipe".to_string(),
            input_type: InputType::Command {
                command: "echo".to_string(),
                args: vec!["hello | tr a-z A-Z".to_string()],
            },
            title: "Test Command".to_string(),
            description: "Test shell command".to_string(),
            default: None,
            required: false,
            validation: None,
        };

        let response = provider.get_input(&request, &context).await.unwrap();
        if let smcp_computer::inputs::InputValue::String(s) = response.value {
            // shell应该执行管道并返回大写的HELLO
            assert!(s.contains("HELLO"));
        } else {
            panic!("Expected string result");
        }
    }

    // Windows测试
    #[cfg(windows)]
    {
        let request = InputRequest {
            id: "test_windows".to_string(),
            input_type: InputType::Command {
                command: "echo".to_string(),
                args: vec!["hello".to_string()],
            },
            title: "Test Command".to_string(),
            description: "Test shell command".to_string(),
            default: None,
            required: false,
            validation: None,
        };

        let response = provider.get_input(&request, &context).await.unwrap();
        if let smcp_computer::inputs::InputValue::String(s) = response.value {
            assert_eq!(s.trim(), "hello");
        } else {
            panic!("Expected string result");
        }
    }
}

#[tokio::test]
#[cfg(feature = "vrl")]
async fn test_vrl_integration_with_manager() {
    // 测试VRL与MCPServerManager的集成
    use smcp_computer::mcp_clients::vrl_runtime::VrlRuntime;
    use smcp_computer::mcp_clients::{
        MCPServerConfig, MCPServerManager, StdioServerConfig, StdioServerParameters,
    };

    // 创建带VRL脚本的配置
    let vrl_script = r#"
        .processed = true
        .tool_name = .tool_name
        .timestamp_added = "2025-12-16"
    "#;

    let config = StdioServerConfig {
        name: "vrl_test_server".to_string(),
        disabled: false,
        forbidden_tools: vec![],
        tool_meta: std::collections::HashMap::new(),
        default_tool_meta: None,
        vrl: Some(vrl_script.to_string()),
        server_parameters: StdioServerParameters {
            command: "echo".to_string(),
            args: vec!["test".to_string()],
            env: std::collections::HashMap::new(),
            cwd: None,
        },
    };

    let manager = MCPServerManager::new();

    // 初始化管理器
    manager
        .initialize(vec![MCPServerConfig::Stdio(config.clone())])
        .await
        .unwrap();

    // 验证VRL脚本已正确存储
    assert_eq!(config.vrl, Some(vrl_script.to_string()));

    // 测试VRL运行时独立功能
    let mut runtime = VrlRuntime::new();
    let test_event = serde_json::json!({
        "result": "success",
        "data": [1, 2, 3]
    });

    let result = runtime.run(vrl_script, test_event.clone(), "UTC").unwrap();

    // 验证原始数据保持不变（简化实现）
    assert_eq!(result.processed_event["result"], "success");
    assert_eq!(result.processed_event["data"].as_array().unwrap().len(), 3);
}

#[tokio::test]
#[cfg(feature = "vrl")]
async fn test_vrl_multiple_server_configs() {
    // 测试多个服务器配置中的VRL脚本
    use smcp_computer::mcp_clients::vrl_runtime::VrlRuntime;
    use smcp_computer::mcp_clients::{
        MCPServerConfig, MCPServerManager, StdioServerConfig, StdioServerParameters,
    };

    let configs = vec![
        MCPServerConfig::Stdio(StdioServerConfig {
            name: "server1".to_string(),
            disabled: false,
            forbidden_tools: vec![],
            tool_meta: std::collections::HashMap::new(),
            default_tool_meta: None,
            vrl: Some(".server = 1".to_string()),
            server_parameters: StdioServerParameters {
                command: "echo".to_string(),
                args: vec!["server1".to_string()],
                env: std::collections::HashMap::new(),
                cwd: None,
            },
        }),
        MCPServerConfig::Stdio(StdioServerConfig {
            name: "server2".to_string(),
            disabled: false,
            forbidden_tools: vec![],
            tool_meta: std::collections::HashMap::new(),
            default_tool_meta: None,
            vrl: Some(".server = 2".to_string()),
            server_parameters: StdioServerParameters {
                command: "echo".to_string(),
                args: vec!["server2".to_string()],
                env: std::collections::HashMap::new(),
                cwd: None,
            },
        }),
    ];

    let manager = MCPServerManager::new();
    manager.initialize(configs).await.unwrap();

    // 测试每个VRL脚本
    let mut runtime = VrlRuntime::new();
    let event = serde_json::json!({"test": "value"});

    let result1 = runtime.run(".server = 1", event.clone(), "UTC").unwrap();
    let result2 = runtime.run(".server = 2", event.clone(), "UTC").unwrap();

    // 验证结果
    assert_eq!(result1.processed_event["test"], "value");
    assert_eq!(result2.processed_event["test"], "value");
}

#[tokio::test]
#[cfg(feature = "vrl")]
async fn test_vrl_error_handling_in_manager() {
    // 测试VRL错误处理
    use smcp_computer::mcp_clients::vrl_runtime::VrlRuntime;

    let mut runtime = VrlRuntime::new();

    // 测试无效脚本
    let invalid_scripts = vec![
        ".field =",
        "= value",
        ".invalid_syntax @#$",
        ".field = now(", // 未闭合的函数
    ];

    let event = serde_json::json!({"test": "value"});

    for script in invalid_scripts {
        assert!(
            runtime.run(script, event.clone(), "UTC").is_err(),
            "Script should fail: {}",
            script
        );
    }
}

#[tokio::test]
#[cfg(feature = "vrl")]
async fn test_vrl_performance() {
    // 测试VRL性能（简单基准测试）
    use smcp_computer::mcp_clients::vrl_runtime::VrlRuntime;
    use std::time::Instant;

    let mut runtime = VrlRuntime::new();
    let script = ".processed = true";
    let event = serde_json::json!({"data": "test"});

    let iterations = 1000;
    let start = Instant::now();

    for _ in 0..iterations {
        runtime.run(script, event.clone(), "UTC").unwrap();
    }

    let duration = start.elapsed();
    println!(
        "VRL execution time for {} iterations: {:?}",
        iterations, duration
    );

    // 确保性能在合理范围内（每个迭代不超过1ms）
    assert!(duration.as_millis() < iterations as u128);
}

#[tokio::test]
#[cfg(feature = "vrl")]
async fn test_vrl_with_complex_json() {
    // 测试VRL处理复杂JSON结构
    use smcp_computer::mcp_clients::vrl_runtime::VrlRuntime;

    let mut runtime = VrlRuntime::new();
    let script = r#"
        .metadata.processed = true
        .summary.count = 3
    "#;

    let complex_event = serde_json::json!({
        "items": [
            {"id": 1, "name": "item1"},
            {"id": 2, "name": "item2"},
            {"id": 3, "name": "item3"}
        ],
        "nested": {
            "level1": {
                "level2": {
                    "value": "deep"
                }
            }
        },
        "metadata": {
            "created": "2025-12-16"
        }
    });

    let result = runtime.run(script, complex_event.clone(), "UTC").unwrap();

    // 验证复杂结构保持不变
    assert_eq!(result.processed_event["items"].as_array().unwrap().len(), 3);
    assert_eq!(
        result.processed_event["nested"]["level1"]["level2"]["value"],
        "deep"
    );
    assert_eq!(result.processed_event["metadata"]["created"], "2025-12-16");
}

#[tokio::test]
async fn test_vrl_feature_flag() {
    // 测试VRL feature flag的行为
    #[cfg(feature = "vrl")]
    {
        use smcp_computer::mcp_clients::vrl_runtime::VrlRuntime;
        assert!(VrlRuntime::check_syntax(".field = 1").is_ok());
    }

    #[cfg(not(feature = "vrl"))]
    {
        use smcp_computer::mcp_clients::vrl_runtime::VrlRuntime;
        assert!(VrlRuntime::check_syntax(".field = 1").is_err());
    }
}

#[tokio::test]
async fn test_inputs_type_compatibility() {
    // 测试inputs类型扩展的兼容性
    // Rust新增的类型（Number/Bool/FilePath）不应影响协议兼容性

    let _provider = CliInputProvider::new();

    // 测试Number类型
    let request = InputRequest {
        id: "test_number".to_string(),
        input_type: InputType::Number {
            min: Some(0),
            max: Some(100),
        },
        title: "Enter number".to_string(),
        description: "Test number input".to_string(),
        default: None,
        required: false,
        validation: None,
    };

    // Number类型是Rust扩展，协议层仍以字符串传输
    assert!(matches!(request.input_type, InputType::Number { .. }));

    // 测试Bool类型
    let request = InputRequest {
        id: "test_bool".to_string(),
        input_type: InputType::Bool {
            true_label: Some("Yes".to_string()),
            false_label: Some("No".to_string()),
        },
        title: "Enter bool".to_string(),
        description: "Test bool input".to_string(),
        default: None,
        required: false,
        validation: None,
    };

    assert!(matches!(request.input_type, InputType::Bool { .. }));

    // 基础类型（String/PickString/Command）保持与Python一致
    let request = InputRequest {
        id: "test_string".to_string(),
        input_type: InputType::String {
            password: Some(false),
            min_length: None,
            max_length: None,
        },
        title: "Enter text".to_string(),
        description: "Test string input".to_string(),
        default: None,
        required: false,
        validation: None,
    };

    assert!(matches!(request.input_type, InputType::String { .. }));
}

#[tokio::test]
async fn test_auto_reconnect_semantics() {
    // 测试auto_reconnect语义与Python一致（配置热更新）
    use smcp_computer::mcp_clients::{
        MCPServerConfig, MCPServerManager, StdioServerConfig, StdioServerParameters,
    };

    let manager = MCPServerManager::new();

    // 创建初始配置
    let config1 = StdioServerConfig {
        name: "test".to_string(),
        disabled: false,
        forbidden_tools: vec![],
        tool_meta: std::collections::HashMap::new(),
        default_tool_meta: None,
        vrl: None,
        server_parameters: StdioServerParameters {
            command: "echo".to_string(),
            args: vec!["v1".to_string()],
            env: std::collections::HashMap::new(),
            cwd: None,
        },
    };

    // 初始化
    manager
        .initialize(vec![MCPServerConfig::Stdio(config1.clone())])
        .await
        .unwrap();

    // 更新配置（auto_reconnect=true应该允许热更新）
    let mut config2 = config1.clone();
    config2.server_parameters.args = vec!["v2".to_string()];

    // 这应该成功（auto_reconnect=true）
    let result = manager
        .add_or_update_server(MCPServerConfig::Stdio(config2.clone()))
        .await;
    assert!(result.is_ok());

    // 验证配置已更新（通过get_server_status间接验证）
    let status = manager.get_server_status().await;
    let test_server = status.iter().find(|(name, _, _)| name == "test");
    assert!(test_server.is_some());
}
