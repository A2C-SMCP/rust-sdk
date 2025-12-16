use smcp_computer::{
    computer::{Computer, Session, SilentSession},
    errors::ComputerError,
    mcp_clients::model::{
        MCPServerConfig, MCPServerInput, PromptStringInput, StdioServerConfig,
        StdioServerParameters,
    },
};
/**
* 文件名: computer_integration
* 作者: JQQ
* 创建日期: 2025/12/16
* 最后修改日期: 2025/12/16
* 版权: 2023 JQQ. All rights reserved.
* 依赖: tokio, smcp-computer
* 描述: Computer模块集成测试
*/
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

/// 测试Session实现 / Test Session implementation
struct TestSession {
    id: String,
    resolved_inputs: Arc<Mutex<HashMap<String, serde_json::Value>>>,
}

impl TestSession {
    fn new(id: &str) -> Self {
        Self {
            id: id.to_string(),
            resolved_inputs: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    async fn set_input(&self, input_id: &str, value: serde_json::Value) {
        let mut inputs = self.resolved_inputs.lock().await;
        inputs.insert(input_id.to_string(), value);
    }
}

#[async_trait::async_trait]
impl Session for TestSession {
    async fn resolve_input(
        &self,
        input: &MCPServerInput,
    ) -> Result<serde_json::Value, ComputerError> {
        let inputs = self.resolved_inputs.lock().await;
        let input_id = input.id();

        if let Some(value) = inputs.get(input_id) {
            Ok(value.clone())
        } else {
            // 回退到默认值 / Fallback to default value
            match input {
                MCPServerInput::PromptString(input) => Ok(serde_json::Value::String(
                    input.default.clone().unwrap_or_default(),
                )),
                MCPServerInput::PickString(input) => Ok(serde_json::Value::String(
                    input
                        .default
                        .clone()
                        .unwrap_or_else(|| input.options.first().cloned().unwrap_or_default()),
                )),
                MCPServerInput::Command(_input) => Ok(serde_json::Value::Null),
            }
        }
    }

    fn session_id(&self) -> &str {
        &self.id
    }
}

#[tokio::test]
async fn test_computer_with_mock_session() {
    let session = TestSession::new("test_session");

    // 设置预期的输入值 / Set expected input values
    session
        .set_input(
            "username",
            serde_json::Value::String("test_user".to_string()),
        )
        .await;
    session
        .set_input(
            "password",
            serde_json::Value::String("test_pass".to_string()),
        )
        .await;

    let mut inputs = HashMap::new();
    inputs.insert(
        "username".to_string(),
        MCPServerInput::PromptString(PromptStringInput {
            id: "username".to_string(),
            description: "Username".to_string(),
            default: None,
            password: Some(false),
        }),
    );

    inputs.insert(
        "password".to_string(),
        MCPServerInput::PromptString(PromptStringInput {
            id: "password".to_string(),
            description: "Password".to_string(),
            default: None,
            password: Some(true),
        }),
    );

    let computer = Computer::new("test_computer", session, Some(inputs), None, false, false);

    // 注意：session是私有字段，无法直接访问
    // Note: session is a private field, cannot access directly
    // 这里只测试输入定义的获取
    // Here we only test getting input definition
    let _username_input = computer.get_input("username").await.unwrap().unwrap();
    // 在实际使用中，输入解析会在工具调用时进行
    // In actual usage, input resolution happens during tool calls
}

#[tokio::test]
async fn test_computer_server_lifecycle() {
    let session = SilentSession::new("test");
    let computer = Computer::new("test_computer", session, None, None, false, false);

    // 测试启动 / Test boot up
    computer.boot_up().await.unwrap();

    // 添加服务器 / Add server
    let server_config = MCPServerConfig::Stdio(StdioServerConfig {
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

    computer.add_or_update_server(server_config).await.unwrap();

    // 获取可用工具（需要实际的服务器才能返回真实工具）
    // Get available tools (needs actual server to return real tools)
    let tools = computer.get_available_tools().await;
    // 由于没有实际的服务器运行，这里会返回空列表或错误
    // Since no actual server is running, this will return empty list or error
    match tools {
        Ok(_) => {}                               // 预期的行为 / Expected behavior
        Err(ComputerError::InvalidState(_)) => {} // 也可能的状态 / Also possible state
        Err(_) => panic!("Unexpected error"),
    }

    // 关闭 / Shutdown
    computer.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_computer_with_confirmation_callback() {
    let session = SilentSession::new("test");
    let computer = Computer::new("test_computer", session, None, None, false, false);

    // 设置确认回调 / Set confirmation callback
    let callback_called = Arc::new(Mutex::new(false));
    let callback_called_clone = callback_called.clone();

    let computer = computer.with_confirm_callback(move |_req_id, _server, _tool, _params| {
        let rt = tokio::runtime::Handle::current();
        rt.block_on(async {
            let mut called = callback_called_clone.lock().await;
            *called = true;
        });
        true // 确认所有工具调用 / Confirm all tool calls
    });

    // 尝试执行工具（需要实际服务器）
    // Try to execute tool (needs actual server)
    let result = computer
        .execute_tool(
            "test_req",
            "non_existent_tool",
            serde_json::json!({}),
            Some(5.0),
        )
        .await;

    // 预期会失败，因为工具不存在
    // Expected to fail because tool doesn't exist
    assert!(result.is_err());
}

#[tokio::test]
async fn test_computer_input_value_management() {
    let session = SilentSession::new("test");
    let mut inputs = HashMap::new();

    inputs.insert(
        "test_input".to_string(),
        MCPServerInput::PromptString(PromptStringInput {
            id: "test_input".to_string(),
            description: "Test input".to_string(),
            default: Some("default".to_string()),
            password: Some(false),
        }),
    );

    let computer = Computer::new("test_computer", session, Some(inputs), None, false, false);

    // 测试获取输入值（当前返回None，因为InputHandler缓存未实现）
    // Test getting input value (currently returns None because InputHandler cache not implemented)
    let value = computer.get_input_value("test_input").await.unwrap();
    assert!(value.is_none());

    // 测试设置输入值
    // Test setting input value
    let set_result = computer
        .set_input_value(
            "test_input",
            serde_json::Value::String("new_value".to_string()),
        )
        .await
        .unwrap();
    assert!(set_result);

    // 测试移除输入值
    // Test removing input value
    let removed = computer.remove_input_value("test_input").await.unwrap();
    assert!(removed); // 成功移除输入值 / Successfully removed input value

    // 测试清空输入值
    // Test clearing input values
    computer.clear_input_values(None).await.unwrap();
    computer
        .clear_input_values(Some("test_input"))
        .await
        .unwrap();
}

#[tokio::test]
async fn test_computer_multiple_servers() {
    let session = SilentSession::new("test");
    let computer = Computer::new("test_computer", session, None, None, false, false);

    computer.boot_up().await.unwrap();

    // 添加多个服务器 / Add multiple servers
    let server1 = MCPServerConfig::Stdio(StdioServerConfig {
        name: "server1".to_string(),
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

    let server2 = MCPServerConfig::Stdio(StdioServerConfig {
        name: "server2".to_string(),
        disabled: false,
        forbidden_tools: vec!["dangerous_tool".to_string()],
        tool_meta: HashMap::new(),
        default_tool_meta: None,
        vrl: None,
        server_parameters: StdioServerParameters {
            command: "cat".to_string(),
            args: vec![],
            env: HashMap::new(),
            cwd: None,
        },
    });

    computer.add_or_update_server(server1).await.unwrap();
    computer.add_or_update_server(server2).await.unwrap();

    // 更新服务器配置 / Update server configuration
    let updated_server1 = MCPServerConfig::Stdio(StdioServerConfig {
        name: "server1".to_string(),
        disabled: true, // 禁用服务器 / Disable server
        forbidden_tools: vec![],
        tool_meta: HashMap::new(),
        default_tool_meta: None,
        vrl: None,
        server_parameters: StdioServerParameters {
            command: "echo".to_string(),
            args: vec!["updated".to_string()],
            env: HashMap::new(),
            cwd: None,
        },
    });

    computer
        .add_or_update_server(updated_server1)
        .await
        .unwrap();

    // 移除一个服务器 / Remove one server
    computer.remove_server("server2").await.unwrap();

    computer.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_computer_error_handling() {
    let session = SilentSession::new("test");
    let computer = Computer::new("test_computer", session, None, None, false, false);

    // 测试未初始化时的错误 / Test errors when not initialized
    let tools_result = computer.get_available_tools().await;
    assert!(tools_result.is_err());
    assert!(matches!(
        tools_result.unwrap_err(),
        ComputerError::InvalidState(_)
    ));

    // 初始化后测试 / Test after initialization
    computer.boot_up().await.unwrap();

    // 尝试移除不存在的服务器 / Try to remove non-existent server
    let result = computer.remove_server("non_existent").await;
    // 应该成功，即使服务器不存在
    // Should succeed even if server doesn't exist
    assert!(result.is_ok());

    // 尝试获取不存在的输入 / Try to get non-existent input
    let input = computer.get_input("non_existent").await.unwrap();
    assert!(input.is_none());

    // 尝试移除不存在的输入 / Try to remove non-existent input
    let removed = computer.remove_input("non_existent").await.unwrap();
    assert!(!removed);
}

#[tokio::test]
async fn test_computer_tool_history() {
    let session = SilentSession::new("test");
    let computer = Computer::new("test_computer", session, None, None, false, false);

    // 初始历史为空 / Initial history is empty
    let history = computer.get_tool_history().await.unwrap();
    assert!(history.is_empty());

    // 执行工具调用会添加历史记录（需要实际服务器）
    // Tool execution adds history (needs actual server)
    computer.boot_up().await.unwrap();

    let _result = computer
        .execute_tool("test_req", "non_existent_tool", serde_json::json!({}), None)
        .await;

    // 即使失败，也可能添加历史记录
    // Even if failed, might add history record
    let history = computer.get_tool_history().await.unwrap();
    // 当前实现中，失败的工具调用不会添加到历史
    // In current implementation, failed tool calls are not added to history
    assert!(history.is_empty());
}
