/**
* 文件名: input_tests
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: tokio, async-trait
* 描述: 输入系统测试
*/

use smcp_computer::inputs::*;
use std::collections::HashMap;
use std::env;
use std::time::Duration;
use tokio::time::timeout;

#[tokio::test]
async fn test_input_value_conversions() {
    // 测试各种类型的转换 / Test conversions of various types
    let string_val: InputValue = "test".into();
    assert_eq!(string_val, InputValue::String("test".to_string()));
    
    let bool_val: InputValue = true.into();
    assert_eq!(bool_val, InputValue::Bool(true));
    
    let number_val: InputValue = 42i64.into();
    assert_eq!(number_val, InputValue::Number(42));
    
    let float_val: InputValue = 3.14f64.into();
    assert_eq!(float_val, InputValue::Float(3.14));
}

#[tokio::test]
async fn test_input_context() {
    let ctx = InputContext::new()
        .with_server_name("test_server".to_string())
        .with_tool_name("test_tool".to_string())
        .with_metadata("key1".to_string(), "value1".to_string())
        .with_metadata("key2".to_string(), "value2".to_string());
    
    assert_eq!(ctx.server_name, Some("test_server".to_string()));
    assert_eq!(ctx.tool_name, Some("test_tool".to_string()));
    assert_eq!(ctx.metadata.len(), 2);
    assert_eq!(ctx.metadata.get("key1"), Some(&"value1".to_string()));
    assert_eq!(ctx.metadata.get("key2"), Some(&"value2".to_string()));
}

#[tokio::test]
async fn test_environment_input_provider() {
    let provider = EnvironmentInputProvider::new().with_prefix("TEST_".to_string());
    
    // 设置测试环境变量 / Set test environment variable
    env::set_var("TEST_INPUT_VALUE", "test_value");
    
    let request = InputRequest {
        id: "input_value".to_string(),
        input_type: InputType::String { 
            password: None, 
            min_length: None, 
            max_length: None 
        },
        title: "Test Input".to_string(),
        description: "Test Description".to_string(),
        default: None,
        required: true,
        validation: None,
    };
    
    let context = InputContext::new();
    
    // 获取输入 / Get input
    let result = provider.get_input(&request, &context).await;
    assert!(result.is_ok());
    
    let response = result.unwrap();
    assert_eq!(response.id, "input_value");
    assert_eq!(response.value, InputValue::String("test_value".to_string()));
    assert!(!response.cancelled);
    
    // 清理环境变量 / Clean up environment variable
    env::remove_var("TEST_INPUT_VALUE");
}

#[tokio::test]
async fn test_environment_input_provider_with_context() {
    let provider = EnvironmentInputProvider::new();
    
    // 设置带上下文的环境变量 / Set environment variable with context
    env::set_var("A2C_SMCP_SECRET_KEY", "secret_value");
    env::set_var("A2C_SMCP_SECRET_KEY_TEST_SERVER", "server_value");
    env::set_var("A2C_SMCP_SECRET_KEY_TEST_SERVER_TEST_TOOL", "tool_value");
    
    let request = InputRequest {
        id: "secret_key".to_string(),
        input_type: InputType::String { 
            password: Some(true), 
            min_length: None, 
            max_length: None 
        },
        title: "Secret Key".to_string(),
        description: "Enter secret key".to_string(),
        default: None,
        required: true,
        validation: None,
    };
    
    // 测试不同上下文的优先级 / Test priority of different contexts
    let context1 = InputContext::new();
    let result1 = provider.get_input(&request, &context1).await;
    assert!(result1.is_ok());
    assert_eq!(result1.unwrap().value, InputValue::String("secret_value".to_string()));
    
    let context2 = InputContext::new().with_server_name("test_server".to_string());
    let result2 = provider.get_input(&request, &context2).await;
    assert!(result2.is_ok());
    assert_eq!(result2.unwrap().value, InputValue::String("server_value".to_string()));
    
    let context3 = InputContext::new()
        .with_server_name("test_server".to_string())
        .with_tool_name("test_tool".to_string());
    let result3 = provider.get_input(&request, &context3).await;
    assert!(result3.is_ok());
    assert_eq!(result3.unwrap().value, InputValue::String("tool_value".to_string()));
    
    // 清理环境变量 / Clean up environment variables
    env::remove_var("A2C_SMCP_SECRET_KEY");
    env::remove_var("A2C_SMCP_SECRET_KEY_TEST_SERVER");
    env::remove_var("A2C_SMCP_SECRET_KEY_TEST_SERVER_TEST_TOOL");
}

#[tokio::test]
async fn test_environment_input_provider_types() {
    let provider = EnvironmentInputProvider::new();
    
    // 测试字符串类型 / Test string type
    env::set_var("A2C_SMCP_STRING_INPUT", "hello");
    
    let request = InputRequest {
        id: "string_input".to_string(),
        input_type: InputType::String { 
            password: None, 
            min_length: None, 
            max_length: None 
        },
        title: "String Input".to_string(),
        description: "Test string".to_string(),
        default: None,
        required: true,
        validation: None,
    };
    
    let result = provider.get_input(&request, &InputContext::new()).await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap().value, InputValue::String("hello".to_string()));
    
    // 测试数字类型 / Test number type
    env::set_var("A2C_SMCP_NUMBER_INPUT", "42");
    
    let request = InputRequest {
        id: "number_input".to_string(),
        input_type: InputType::Number { min: None, max: None },
        title: "Number Input".to_string(),
        description: "Test number".to_string(),
        default: None,
        required: true,
        validation: None,
    };
    
    let result = provider.get_input(&request, &InputContext::new()).await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap().value, InputValue::Number(42));
    
    // 测试布尔类型 / Test boolean type
    env::set_var("A2C_SMCP_BOOL_INPUT", "true");
    
    let request = InputRequest {
        id: "bool_input".to_string(),
        input_type: InputType::Bool { 
            true_label: None, 
            false_label: None 
        },
        title: "Bool Input".to_string(),
        description: "Test boolean".to_string(),
        default: None,
        required: true,
        validation: None,
    };
    
    let result = provider.get_input(&request, &InputContext::new()).await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap().value, InputValue::Bool(true));
    
    // 清理环境变量 / Clean up environment variables
    env::remove_var("A2C_SMCP_STRING_INPUT");
    env::remove_var("A2C_SMCP_NUMBER_INPUT");
    env::remove_var("A2C_SMCP_BOOL_INPUT");
}

#[tokio::test]
async fn test_composite_input_provider() {
    // 创建组合提供者 / Create composite provider
    let provider = CompositeInputProvider::new()
        .add_provider(Box::new(EnvironmentInputProvider::new()))
        .add_provider(Box::new(TestInputProvider::new("default_value".to_string())));
    
    let request = InputRequest {
        id: "test_input".to_string(),
        input_type: InputType::String { 
            password: None, 
            min_length: None, 
            max_length: None 
        },
        title: "Test Input".to_string(),
        description: "Test description".to_string(),
        default: None,
        required: false,
        validation: None,
    };
    
    // 环境变量不存在，应该使用第二个提供者 / Environment variable doesn't exist, should use second provider
    let result = provider.get_input(&request, &InputContext::new()).await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap().value, InputValue::String("default_value".to_string()));
    
    // 设置环境变量，应该使用第一个提供者 / Set environment variable, should use first provider
    env::set_var("A2C_SMCP_TEST_INPUT", "env_value");
    
    let result = provider.get_input(&request, &InputContext::new()).await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap().value, InputValue::String("env_value".to_string()));
    
    // 清理环境变量 / Clean up environment variable
    env::remove_var("A2C_SMCP_TEST_INPUT");
}

#[tokio::test]
async fn test_input_handler() {
    let handler = InputHandler::new();
    
    // 测试缓存功能 / Test cache functionality
    let request = InputRequest {
        id: "test_input".to_string(),
        input_type: InputType::String { 
            password: None, 
            min_length: None, 
            max_length: None 
        },
        title: "Test Input".to_string(),
        description: "Test description".to_string(),
        default: Some(InputValue::String("default".to_string())),
        required: false,
        validation: None,
    };
    
    let context = InputContext::new();
    
    // 第一次获取 / First get
    let result1 = handler.get_input(request.clone(), context.clone()).await;
    assert!(result1.is_ok());
    
    // 清除缓存 / Clear cache
    handler.clear_cache().await;
    
    // 第二次获取 / Second get
    let result2 = handler.get_input(request, context).await;
    assert!(result2.is_ok());
    
    // 结果应该相同 / Results should be the same
    assert_eq!(result1.unwrap().value, result2.unwrap().value);
}

#[tokio::test]
async fn test_input_handler_batch() {
    let handler = InputHandler::new();
    
    let requests = vec![
        InputRequest {
            id: "input1".to_string(),
            input_type: InputType::String { 
                password: None, 
                min_length: None, 
                max_length: None 
            },
            title: "Input 1".to_string(),
            description: "First input".to_string(),
            default: Some(InputValue::String("default1".to_string())),
            required: false,
            validation: None,
        },
        InputRequest {
            id: "input2".to_string(),
            input_type: InputType::Number { min: None, max: None },
            title: "Input 2".to_string(),
            description: "Second input".to_string(),
            default: Some(InputValue::Number(42)),
            required: false,
            validation: None,
        },
    ];
    
    let context = InputContext::new();
    
    // 批量获取输入 / Batch get inputs
    let results = handler.get_inputs(requests, context).await;
    assert!(results.is_ok());
    
    let responses = results.unwrap();
    assert_eq!(responses.len(), 2);
    assert_eq!(responses[0].id, "input1");
    assert_eq!(responses[1].id, "input2");
}

#[tokio::test]
async fn test_mcp_input_conversion() {
    let handler = InputHandler::new();
    
    // 测试字符串输入转换 / Test string input conversion
    let mcp_input = crate::mcp_clients::model::MCPServerInput::PromptString(
        crate::mcp_clients::model::PromptStringInput {
            id: "test_prompt".to_string(),
            description: "Test prompt".to_string(),
            default: Some("default".to_string()),
            password: Some(false),
        }
    );
    
    let request = handler.create_request_from_mcp_input(&mcp_input, None);
    assert_eq!(request.id, "test_prompt");
    assert_eq!(request.title, "Test prompt");
    assert_eq!(request.description, "Test prompt");
    assert!(request.required);
    
    match request.input_type {
        InputType::String { password, .. } => {
            assert_eq!(password, Some(false));
        }
        _ => panic!("Expected string input type"),
    }
    
    // 测试选择输入转换 / Test pick string input conversion
    let mcp_input = crate::mcp_clients::model::MCPServerInput::PickString(
        crate::mcp_clients::model::PickStringInput {
            id: "test_pick".to_string(),
            description: "Test pick".to_string(),
            options: vec!["option1".to_string(), "option2".to_string()],
            default: Some("option1".to_string()),
        }
    );
    
    let request = handler.create_request_from_mcp_input(&mcp_input, None);
    assert_eq!(request.id, "test_pick");
    
    match request.input_type {
        InputType::PickString { options, .. } => {
            assert_eq!(options, vec!["option1".to_string(), "option2".to_string()]);
        }
        _ => panic!("Expected pick string input type"),
    }
}

// 测试用的输入提供者 / Test input provider
struct TestInputProvider {
    default_value: String,
}

impl TestInputProvider {
    fn new(default_value: String) -> Self {
        Self { default_value }
    }
}

#[async_trait::async_trait]
impl InputProvider for TestInputProvider {
    async fn get_input(&self, request: &InputRequest, _context: &InputContext) -> InputResult<InputResponse> {
        Ok(InputResponse {
            id: request.id.clone(),
            value: InputValue::String(self.default_value.clone()),
            cancelled: false,
        })
    }
}

#[tokio::test]
async fn test_input_validation() {
    let handler = InputHandler::new();
    
    // 测试正则表达式验证 / Test regex validation
    let request = InputRequest {
        id: "email_input".to_string(),
        input_type: InputType::String { 
            password: None, 
            min_length: None, 
            max_length: None 
        },
        title: "Email Input".to_string(),
        description: "Enter email".to_string(),
        default: None,
        required: true,
        validation: Some(InputValidationRule::Regex {
            pattern: r"^[^@]+@[^@]+\.[^@]+$".to_string(),
            message: Some("Invalid email format".to_string()),
        }),
    };
    
    // 这个测试需要实际的CLI输入，在实际测试中可能需要模拟
    // This test requires actual CLI input, might need mocking in real tests
    
    // 测试长度验证 / Test length validation
    let request = InputRequest {
        id: "length_input".to_string(),
        input_type: InputType::String { 
            password: None, 
            min_length: Some(5), 
            max_length: Some(10) 
        },
        title: "Length Input".to_string(),
        description: "Enter text".to_string(),
        default: Some(InputValue::String("12345".to_string())),
        required: false,
        validation: None,
    };
    
    let context = InputContext::new();
    let result = handler.get_input(request, context).await;
    assert!(result.is_ok());
}
