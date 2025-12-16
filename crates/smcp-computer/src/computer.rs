/*!
* 文件名: computer
* 作者: JQQ
* 创建日期: 2025/12/16
* 最后修改日期: 2025/12/16
* 版权: 2023 JQQ. All rights reserved.
* 依赖: tokio, async-trait, serde, tracing
* 描述: Computer核心模块实现 / Core Computer module implementation
*/

use std::collections::HashMap;
use std::sync::{Arc, Weak};
use tokio::sync::{RwLock, Mutex};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info};
use chrono::{DateTime, Utc};

use crate::errors::{ComputerError, ComputerResult};
use crate::mcp_clients::{
    manager::MCPServerManager,
    model::{MCPServerConfig, MCPServerInput, CallToolResult, Tool},
};
use crate::inputs::handler::InputHandler;
use crate::socketio_client::SmcpComputerClient;

/// 工具调用历史记录 / Tool call history record
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRecord {
    /// 时间戳 / Timestamp
    pub timestamp: DateTime<Utc>,
    /// 请求ID / Request ID
    pub req_id: String,
    /// 服务器名称 / Server name
    pub server: String,
    /// 工具名称 / Tool name
    pub tool: String,
    /// 参数 / Parameters
    pub parameters: serde_json::Value,
    /// 超时时间 / Timeout
    pub timeout: Option<f64>,
    /// 是否成功 / Success
    pub success: bool,
    /// 错误信息 / Error message
    pub error: Option<String>,
}

/// Session trait - 用于抽象不同的交互环境（CLI、GUI、Web）
/// Session trait - Abstract different interaction environments (CLI, GUI, Web)
#[async_trait]
pub trait Session: Send + Sync {
    /// 解析输入值 / Resolve input value
    async fn resolve_input(&self, input: &MCPServerInput) -> ComputerResult<serde_json::Value>;
    
    /// 获取会话ID / Get session ID
    fn session_id(&self) -> &str;
}

/// 默认的静默Session实现 / Default silent session implementation
pub struct SilentSession {
    id: String,
}

impl SilentSession {
    /// 创建新的静默Session / Create new silent session
    pub fn new(id: impl Into<String>) -> Self {
        Self { id: id.into() }
    }
}

#[async_trait]
impl Session for SilentSession {
    async fn resolve_input(&self, input: &MCPServerInput) -> ComputerResult<serde_json::Value> {
        // 静默Session只使用默认值 / Silent session only uses default values
        match input {
            MCPServerInput::PromptString(input) => {
                Ok(serde_json::Value::String(
                    input.default.clone().unwrap_or_else(|| String::new())
                ))
            }
            MCPServerInput::PickString(input) => {
                Ok(serde_json::Value::String(
                    input.default.clone().unwrap_or_else(|| {
                        input.options.first().cloned().unwrap_or_default()
                    })
                ))
            }
            MCPServerInput::Command(_input) => {
                // 命令输入在静默模式下返回空 / Command input returns empty in silent mode
                Ok(serde_json::Value::Null)
            }
        }
    }

    fn session_id(&self) -> &str {
        &self.id
    }
}

/// Computer核心结构体 / Core Computer struct
pub struct Computer<S: Session> {
    /// 计算机名称 / Computer name
    name: String,
    /// MCP服务器管理器 / MCP server manager
    mcp_manager: Arc<RwLock<Option<MCPServerManager>>>,
    /// 输入定义映射 / Input definitions map (id -> input)
    inputs: RwLock<HashMap<String, MCPServerInput>>,
    /// MCP服务器配置映射 / MCP server configurations map (name -> config)
    mcp_servers: RwLock<HashMap<String, MCPServerConfig>>,
    /// 输入处理器 / Input handler
    input_handler: Arc<RwLock<InputHandler>>,
    /// 自动连接标志 / Auto connect flag
    auto_connect: bool,
    /// 自动重连标志 / Auto reconnect flag
    auto_reconnect: bool,
    /// 工具调用历史 / Tool call history
    tool_history: Arc<Mutex<Vec<ToolCallRecord>>>,
    /// Session实例 / Session instance
    session: S,
    /// Socket.IO客户端引用 / Socket.IO client reference
    socketio_client: Arc<RwLock<Option<Weak<SmcpComputerClient>>>>,
    /// 确认回调函数 / Confirmation callback function
    confirm_callback: Option<Arc<dyn Fn(&str, &str, &str, &serde_json::Value) -> bool + Send + Sync>>,
}

impl<S: Session> Computer<S> {
    /// 创建新的Computer实例 / Create new Computer instance
    pub fn new(
        name: impl Into<String>,
        session: S,
        inputs: Option<HashMap<String, MCPServerInput>>,
        mcp_servers: Option<HashMap<String, MCPServerConfig>>,
        auto_connect: bool,
        auto_reconnect: bool,
    ) -> Self {
        let name = name.into();
        let inputs = inputs.unwrap_or_default();
        let mcp_servers = mcp_servers.unwrap_or_default();
        
        Self {
            name,
            mcp_manager: Arc::new(RwLock::new(None)),
            inputs: RwLock::new(inputs),
            mcp_servers: RwLock::new(mcp_servers),
            input_handler: Arc::new(RwLock::new(InputHandler::new())),
            auto_connect,
            auto_reconnect,
            tool_history: Arc::new(Mutex::new(Vec::new())),
            session,
            socketio_client: Arc::new(RwLock::new(None)),
            confirm_callback: None,
        }
    }

    /// 设置确认回调函数 / Set confirmation callback function
    pub fn with_confirm_callback<F>(mut self, callback: F) -> Self 
    where
        F: Fn(&str, &str, &str, &serde_json::Value) -> bool + Send + Sync + 'static,
    {
        self.confirm_callback = Some(Arc::new(callback));
        self
    }

    /// 启动Computer / Boot up the computer
    pub async fn boot_up(&self) -> ComputerResult<()> {
        info!("Starting Computer: {}", self.name);
        
        // 创建MCP服务器管理器 / Create MCP server manager
        let manager = MCPServerManager::new();
        
        // 渲染并验证服务器配置 / Render and validate server configurations
        let servers = self.mcp_servers.read().await;
        let mut validated_servers = Vec::new();
        
        for (_name, server_config) in servers.iter() {
            match self.render_server_config(server_config).await {
                Ok(validated) => validated_servers.push(validated),
                Err(e) => {
                    error!("Failed to render server config {}: {}", server_config.name(), e);
                    // 保留原配置作为回退 / Keep original config as fallback
                    validated_servers.push(server_config.clone());
                }
            }
        }
        
        // 初始化管理器 / Initialize manager
        manager.initialize(validated_servers).await?;
        
        // 设置管理器到实例 / Set manager to instance
        *self.mcp_manager.write().await = Some(manager);
        
        info!("Computer {} started successfully", self.name);
        Ok(())
    }

    /// 渲染服务器配置 / Render server configuration
    async fn render_server_config(&self, config: &MCPServerConfig) -> ComputerResult<MCPServerConfig> {
        // TODO: 实现配置渲染逻辑 / TODO: Implement config rendering logic
        // 这里需要实现类似Python版本的配置渲染功能
        // This needs to implement config rendering similar to Python version
        Ok(config.clone())
    }

    /// 动态添加或更新服务器配置 / Add or update server configuration dynamically
    pub async fn add_or_update_server(&self, server: MCPServerConfig) -> ComputerResult<()> {
        // 确保管理器已初始化 / Ensure manager is initialized
        {
            let mut manager_guard = self.mcp_manager.write().await;
            if manager_guard.is_none() {
                *manager_guard = Some(MCPServerManager::new());
            }
        }

        // 渲染并验证配置 / Render and validate configuration
        let validated = self.render_server_config(&server).await?;
        
        // 添加到管理器 / Add to manager
        let manager = self.mcp_manager.read().await;
        if let Some(ref manager) = *manager {
            manager.add_or_update_server(validated).await?;
        }
        
        // 更新本地配置映射 / Update local configuration map
        {
            let mut servers = self.mcp_servers.write().await;
            servers.insert(server.name().to_string(), server);
        }
        
        Ok(())
    }

    /// 移除服务器配置 / Remove server configuration
    pub async fn remove_server(&self, server_name: &str) -> ComputerResult<()> {
        let manager = self.mcp_manager.read().await;
        if let Some(ref manager) = *manager {
            manager.remove_server(server_name).await?;
        }
        
        // 从本地配置映射移除 / Remove from local configuration map
        {
            let mut servers = self.mcp_servers.write().await;
            servers.remove(server_name);
        }
        
        Ok(())
    }

    /// 更新inputs定义 / Update inputs definition
    pub async fn update_inputs(&self, inputs: HashMap<String, MCPServerInput>) -> ComputerResult<()> {
        *self.inputs.write().await = inputs;
        
        // 重新创建输入处理器 / Recreate input handler
        {
            let mut handler = self.input_handler.write().await;
            *handler = InputHandler::new();
        }
        
        Ok(())
    }

    /// 添加或更新单个input / Add or update single input
    pub async fn add_or_update_input(&self, input: MCPServerInput) -> ComputerResult<()> {
        let input_id = input.id();
        {
            let mut inputs = self.inputs.write().await;
            inputs.insert(input_id.to_string(), input);
        }
        
        // 清除相关缓存 / Clear related cache
        // TODO: 实现InputHandler的缓存清除功能
        
        Ok(())
    }

    /// 移除input / Remove input
    pub async fn remove_input(&self, input_id: &str) -> ComputerResult<bool> {
        let removed = {
            let mut inputs = self.inputs.write().await;
            inputs.remove(input_id).is_some()
        };
        
        if removed {
            // TODO: 清除缓存 / Clear cache
        }
        
        Ok(removed)
    }

    /// 获取input定义 / Get input definition
    pub async fn get_input(&self, input_id: &str) -> ComputerResult<Option<MCPServerInput>> {
        let inputs = self.inputs.read().await;
        Ok(inputs.get(input_id).cloned())
    }

    /// 列出所有inputs / List all inputs
    pub async fn list_inputs(&self) -> ComputerResult<Vec<MCPServerInput>> {
        let inputs = self.inputs.read().await;
        Ok(inputs.values().cloned().collect())
    }

    /// 获取输入值 / Get input value
    pub async fn get_input_value(&self, input_id: &str) -> ComputerResult<Option<serde_json::Value>> {
        // TODO: 实现从InputHandler获取缓存值的功能
        Ok(None)
    }

    /// 设置输入值 / Set input value
    pub async fn set_input_value(&self, input_id: &str, value: serde_json::Value) -> ComputerResult<bool> {
        // 检查input是否存在 / Check if input exists
        {
            let inputs = self.inputs.read().await;
            if !inputs.contains_key(input_id) {
                return Ok(false);
            }
        }
        
        // TODO: 实现InputHandler的缓存设置功能
        
        Ok(true)
    }

    /// 移除输入值 / Remove input value
    pub async fn remove_input_value(&self, input_id: &str) -> ComputerResult<bool> {
        // TODO: 实现InputHandler的缓存删除功能
        Ok(false)
    }

    /// 列出所有输入值 / List all input values
    pub async fn list_input_values(&self) -> ComputerResult<HashMap<String, serde_json::Value>> {
        // TODO: 实现InputHandler的缓存列表功能
        Ok(HashMap::new())
    }

    /// 清空输入值缓存 / Clear input value cache
    pub async fn clear_input_values(&self, input_id: Option<&str>) -> ComputerResult<()> {
        // TODO: 实现InputHandler的缓存清除功能
        Ok(())
    }

    /// 获取可用工具列表 / Get available tools list
    pub async fn get_available_tools(&self) -> ComputerResult<Vec<Tool>> {
        let manager = self.mcp_manager.read().await;
        if let Some(ref manager) = *manager {
            let tools: Vec<Tool> = manager.list_available_tools().await;
            // TODO: 转换为SMCPTool格式 / TODO: Convert to SMCPTool format
            // 这里需要实现工具格式转换
            // This needs to implement tool format conversion
            Ok(tools)
        } else {
            Err(ComputerError::InvalidState("Computer not initialized".to_string()))
        }
    }

    /// 执行工具调用 / Execute tool call
    pub async fn execute_tool(
        &self,
        req_id: &str,
        tool_name: &str,
        parameters: serde_json::Value,
        timeout: Option<f64>,
    ) -> ComputerResult<CallToolResult> {
        let manager = self.mcp_manager.read().await;
        if let Some(ref manager) = *manager {
            // 验证工具调用 / Validate tool call
            let (server_name, tool_name) = manager.validate_tool_call(tool_name, &parameters).await?;
            let server_name = server_name.to_string();
            let tool_name = tool_name.to_string();
            
            let timestamp = Utc::now();
            let mut success = false;
            let mut error_msg = None;
            let result: CallToolResult;
            
            // 检查是否需要确认 / Check if confirmation is needed
            // TODO: 需要实现获取工具元数据的方法
            let need_confirm = true; // 暂时默认需要确认
            
            // 准备参数，只在实际调用时clone / Prepare parameters, only clone when actually calling
            let parameters_for_call = parameters.clone();
            
            if need_confirm {
                if let Some(ref callback) = self.confirm_callback {
                    let confirmed = callback(req_id, &server_name, &tool_name, &parameters);
                    if confirmed {
                        let timeout_duration = timeout.map(|t| std::time::Duration::from_secs_f64(t));
                        result = manager.call_tool(&server_name, &tool_name, parameters_for_call, timeout_duration).await?;
                        success = !result.is_error;
                    } else {
                        result = CallToolResult {
                            content: vec![crate::mcp_clients::model::Content::Text {
                                text: "工具调用二次确认被拒绝，请稍后再试".to_string(),
                            }],
                            is_error: false,
                            meta: None,
                        };
                    }
                } else {
                    result = CallToolResult {
                        content: vec![crate::mcp_clients::model::Content::Text {
                            text: "当前工具需要调用前进行二次确认，但客户端目前没有实现二次确认回调方法".to_string(),
                        }],
                        is_error: true,
                        meta: None,
                    };
                    error_msg = Some("No confirmation callback".to_string());
                }
            } else {
                let timeout_duration = timeout.map(|t| std::time::Duration::from_secs_f64(t));
                result = manager.call_tool(&server_name, &tool_name, parameters_for_call, timeout_duration).await?;
                success = !result.is_error;
            }
            
            if result.is_error {
                error_msg = result.content.iter()
                    .find_map(|c| match c {
                        crate::mcp_clients::model::Content::Text { text } => Some(text.clone()),
                        _ => None,
                    });
            }
            
            // 记录历史 / Record history
            let record = ToolCallRecord {
                timestamp,
                req_id: req_id.to_string(),
                server: server_name,
                tool: tool_name,
                parameters,
                timeout,
                success,
                error: error_msg,
            };
            
            {
                let mut history = self.tool_history.lock().await;
                history.push(record);
                // 保持最近10条记录 / Keep last 10 records
                if history.len() > 10 {
                    history.remove(0);
                }
            }
            
            Ok(result)
        } else {
            Err(ComputerError::InvalidState("Computer not initialized".to_string()))
        }
    }

    /// 获取工具调用历史 / Get tool call history
    pub async fn get_tool_history(&self) -> ComputerResult<Vec<ToolCallRecord>> {
        let history = self.tool_history.lock().await;
        Ok(history.clone())
    }

    /// 设置Socket.IO客户端 / Set Socket.IO client
    pub async fn set_socketio_client(&self, client: Arc<SmcpComputerClient>) {
        let mut socketio_ref = self.socketio_client.write().await;
        *socketio_ref = Some(Arc::downgrade(&client));
    }

    /// 关闭Computer / Shutdown computer
    pub async fn shutdown(&self) -> ComputerResult<()> {
        info!("Shutting down Computer: {}", self.name);
        
        let mut manager_guard = self.mcp_manager.write().await;
        if let Some(manager) = manager_guard.take() {
            manager.stop_all().await?;
        }
        
        // 清除Socket.IO客户端引用 / Clear Socket.IO client reference
        {
            let mut socketio_ref = self.socketio_client.write().await;
            *socketio_ref = None;
        }
        
        info!("Computer {} shutdown successfully", self.name);
        Ok(())
    }
}

// 实现Clone以供内部使用 / Implement Clone for internal use
impl<S: Session + Clone> Clone for Computer<S> {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            mcp_manager: Arc::clone(&self.mcp_manager),
            inputs: RwLock::new(HashMap::new()), // Note: 不复制运行时状态 / Don't copy runtime state
            mcp_servers: RwLock::new(HashMap::new()),
            input_handler: Arc::clone(&self.input_handler),
            auto_connect: self.auto_connect,
            auto_reconnect: self.auto_reconnect,
            tool_history: Arc::clone(&self.tool_history),
            session: self.session.clone(),
            socketio_client: Arc::clone(&self.socketio_client),
            confirm_callback: self.confirm_callback.clone(),
        }
    }
}

/// 用于管理器变更通知的trait / Trait for manager change notification
#[async_trait]
pub trait ManagerChangeHandler: Send + Sync {
    /// 处理管理器变更 / Handle manager change
    async fn on_change(&self, message: ManagerChangeMessage) -> ComputerResult<()>;
}

/// 管理器变更消息 / Manager change message
#[derive(Debug, Clone)]
pub enum ManagerChangeMessage {
    /// 工具列表变更 / Tool list changed
    ToolListChanged,
    /// 资源列表变更 / Resource list changed,
    ResourceListChanged { windows: Vec<String> },
    /// 资源更新 / Resource updated
    ResourceUpdated { uri: String },
}

#[async_trait]
impl<S: Session> ManagerChangeHandler for Computer<S> {
    async fn on_change(&self, message: ManagerChangeMessage) -> ComputerResult<()> {
        match message {
            ManagerChangeMessage::ToolListChanged => {
                debug!("Tool list changed, notifying Socket.IO client");
                let socketio_ref = self.socketio_client.read().await;
                if let Some(ref weak_client) = *socketio_ref {
                    if let Some(client) = weak_client.upgrade() as Option<Arc<SmcpComputerClient>> {
                        client.emit_update_tool_list().await?;
                    }
                }
            }
            ManagerChangeMessage::ResourceListChanged { windows } => {
                debug!("Resource list changed, checking for window updates");
                // TODO: 实现窗口变更检测逻辑 / TODO: Implement window change detection logic
            }
            ManagerChangeMessage::ResourceUpdated { uri } => {
                debug!("Resource updated: {}", uri);
                // TODO: 检查是否为window://资源 / TODO: Check if it's a window:// resource
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_clients::model::{StdioServerConfig, StdioServerParameters, PromptStringInput, PickStringInput, CommandInput, MCPServerConfig, MCPServerInput};

    #[tokio::test]
    async fn test_computer_creation() {
        let session = SilentSession::new("test");
        let computer = Computer::new(
            "test_computer",
            session,
            None,
            None,
            true,
            true,
        );
        
        assert_eq!(computer.name, "test_computer");
        assert!(computer.auto_connect);
        assert!(computer.auto_reconnect);
    }

    #[tokio::test]
    async fn test_computer_with_initial_inputs_and_servers() {
        let session = SilentSession::new("test");
        let mut inputs = HashMap::new();
        inputs.insert("input1".to_string(), MCPServerInput::PromptString(PromptStringInput {
            id: "input1".to_string(),
            description: "Test input".to_string(),
            default: Some("default".to_string()),
            password: Some(false),
        }));
        
        let mut servers = HashMap::new();
        servers.insert("server1".to_string(), MCPServerConfig::Stdio(StdioServerConfig {
            name: "server1".to_string(),
            disabled: false,
            forbidden_tools: vec![],
            tool_meta: std::collections::HashMap::new(),
            default_tool_meta: None,
            vrl: None,
            server_parameters: StdioServerParameters {
                command: "echo".to_string(),
                args: vec![],
                env: std::collections::HashMap::new(),
                cwd: None,
            },
        }));
        
        let computer = Computer::new(
            "test_computer",
            session,
            Some(inputs),
            Some(servers),
            false,
            false,
        );
        
        // 验证初始inputs / Verify initial inputs
        let inputs = computer.list_inputs().await.unwrap();
        assert_eq!(inputs.len(), 1);
        match &inputs[0] {
            MCPServerInput::PromptString(input) => {
                assert_eq!(input.id, "input1");
                assert_eq!(input.description, "Test input");
            }
            _ => panic!("Expected PromptString input"),
        }
    }

    #[tokio::test]
    async fn test_input_management() {
        let session = SilentSession::new("test");
        let computer = Computer::new(
            "test_computer",
            session,
            None,
            None,
            true,
            true,
        );
        
        // 测试添加input / Test adding input
        let input = MCPServerInput::PromptString(PromptStringInput {
            id: "test_input".to_string(),
            description: "Test input".to_string(),
            default: Some("default".to_string()),
            password: Some(false),
        });
        
        computer.add_or_update_input(input.clone()).await.unwrap();
        
        // 验证input已添加 / Verify input is added
        let retrieved = computer.get_input("test_input").await.unwrap();
        assert!(retrieved.is_some());
        
        // 测试列出所有inputs / Test listing all inputs
        let inputs = computer.list_inputs().await.unwrap();
        assert_eq!(inputs.len(), 1);
        
        // 测试更新input / Test updating input
        let updated_input = MCPServerInput::PromptString(PromptStringInput {
            id: "test_input".to_string(),
            description: "Updated description".to_string(),
            default: Some("new_default".to_string()),
            password: Some(true),
        });
        computer.add_or_update_input(updated_input).await.unwrap();
        
        let retrieved = computer.get_input("test_input").await.unwrap().unwrap();
        match retrieved {
            MCPServerInput::PromptString(input) => {
                assert_eq!(input.description, "Updated description");
                assert_eq!(input.default, Some("new_default".to_string()));
                assert_eq!(input.password, Some(true));
            }
            _ => panic!("Expected PromptString input"),
        }
        
        // 测试移除input / Test removing input
        let removed = computer.remove_input("test_input").await.unwrap();
        assert!(removed);
        
        let retrieved = computer.get_input("test_input").await.unwrap();
        assert!(retrieved.is_none());
        
        // 测试移除不存在的input / Test removing non-existent input
        let removed = computer.remove_input("non_existent").await.unwrap();
        assert!(!removed);
    }

    #[tokio::test]
    async fn test_multiple_input_types() {
        let session = SilentSession::new("test");
        let computer = Computer::new(
            "test_computer",
            session,
            None,
            None,
            true,
            true,
        );
        
        // 添加不同类型的inputs / Add different types of inputs
        let prompt_input = MCPServerInput::PromptString(PromptStringInput {
            id: "prompt".to_string(),
            description: "Prompt input".to_string(),
            default: None,
            password: Some(false),
        });
        
        let pick_input = MCPServerInput::PickString(PickStringInput {
            id: "pick".to_string(),
            description: "Pick input".to_string(),
            options: vec!["option1".to_string(), "option2".to_string()],
            default: Some("option1".to_string()),
        });
        
        let command_input = MCPServerInput::Command(CommandInput {
            id: "command".to_string(),
            description: "Command input".to_string(),
            command: "ls".to_string(),
            args: None,
        });
        
        computer.add_or_update_input(prompt_input).await.unwrap();
        computer.add_or_update_input(pick_input).await.unwrap();
        computer.add_or_update_input(command_input).await.unwrap();
        
        let inputs = computer.list_inputs().await.unwrap();
        assert_eq!(inputs.len(), 3);
        
        // 验证每个input类型 / Verify each input type
        let input_types: std::collections::HashSet<_> = inputs.iter()
            .map(|input| match input {
                MCPServerInput::PromptString(_) => "prompt",
                MCPServerInput::PickString(_) => "pick",
                MCPServerInput::Command(_) => "command",
            })
            .collect();
        
        assert!(input_types.contains("prompt"));
        assert!(input_types.contains("pick"));
        assert!(input_types.contains("command"));
    }

    #[tokio::test]
    async fn test_server_management() {
        let session = SilentSession::new("test");
        let computer = Computer::new(
            "test_computer",
            session,
            None,
            None,
            true,
            true,
        );
        
        // 添加服务器配置 / Add server configuration
        let server_config = MCPServerConfig::Stdio(StdioServerConfig {
            name: "test_server".to_string(),
            disabled: false,
            forbidden_tools: vec![],
            tool_meta: std::collections::HashMap::new(),
            default_tool_meta: None,
            vrl: None,
            server_parameters: StdioServerParameters {
                command: "echo".to_string(),
                args: vec!["hello".to_string()],
                env: std::collections::HashMap::new(),
                cwd: None,
            },
        });
        
        computer.add_or_update_server(server_config.clone()).await.unwrap();
        
        // 注意：由于MCPServerManager是私有的，我们通过添加重复的服务器来测试更新
        // Note: Since MCPServerManager is private, we test updates by adding duplicate servers
        let updated_config = MCPServerConfig::Stdio(StdioServerConfig {
            name: "test_server".to_string(),
            disabled: true, // 更新为禁用状态 / Update to disabled state
            forbidden_tools: vec!["tool1".to_string()],
            tool_meta: std::collections::HashMap::new(),
            default_tool_meta: None,
            vrl: None,
            server_parameters: StdioServerParameters {
                command: "echo".to_string(),
                args: vec!["updated".to_string()],
                env: std::collections::HashMap::new(),
                cwd: None,
            },
        });
        
        computer.add_or_update_server(updated_config).await.unwrap();
        
        // 移除服务器 / Remove server
        computer.remove_server("test_server").await.unwrap();
    }

    #[tokio::test]
    async fn test_session_trait() {
        // 测试SilentSession的行为 / Test SilentSession behavior
        let session = SilentSession::new("test_session");
        assert_eq!(session.session_id(), "test_session");
        
        // 测试PromptString输入解析 / Test PromptString input resolution
        let prompt_input = MCPServerInput::PromptString(PromptStringInput {
            id: "test".to_string(),
            description: "Test".to_string(),
            default: Some("default_value".to_string()),
            password: Some(false),
        });
        
        let result = session.resolve_input(&prompt_input).await.unwrap();
        assert_eq!(result, serde_json::Value::String("default_value".to_string()));
        
        // 测试无默认值的PromptString / Test PromptString without default
        let no_default_input = MCPServerInput::PromptString(PromptStringInput {
            id: "test2".to_string(),
            description: "Test2".to_string(),
            default: None,
            password: Some(false),
        });
        
        let result = session.resolve_input(&no_default_input).await.unwrap();
        assert_eq!(result, serde_json::Value::String("".to_string()));
        
        // 测试PickString输入解析 / Test PickString input resolution
        let pick_input = MCPServerInput::PickString(PickStringInput {
            id: "pick".to_string(),
            description: "Pick".to_string(),
            options: vec!["opt1".to_string(), "opt2".to_string()],
            default: Some("opt2".to_string()),
        });
        
        let result = session.resolve_input(&pick_input).await.unwrap();
        assert_eq!(result, serde_json::Value::String("opt2".to_string()));
        
        // 测试Command输入解析 / Test Command input resolution
        let command_input = MCPServerInput::Command(CommandInput {
            id: "cmd".to_string(),
            description: "Command".to_string(),
            command: "echo".to_string(),
            args: None,
        });
        
        let result = session.resolve_input(&command_input).await.unwrap();
        assert_eq!(result, serde_json::Value::Null);
    }

    #[tokio::test]
    async fn test_tool_call_history() {
        let session = SilentSession::new("test");
        let computer = Computer::new(
            "test_computer",
            session,
            None,
            None,
            true,
            true,
        );
        
        // 初始历史应该为空 / Initial history should be empty
        let history = computer.get_tool_history().await.unwrap();
        assert!(history.is_empty());
        
        // 注意：实际的工具调用需要MCP服务器，这里只测试历史记录的结构
        // Note: Actual tool calls need MCP server, here we only test history structure
    }

    #[tokio::test]
    async fn test_confirmation_callback() {
        let session = SilentSession::new("test");
        let computer = Computer::new(
            "test_computer",
            session,
            None,
            None,
            true,
            true,
        );
        
        // 设置确认回调 / Set confirmation callback
        let callback_called = Arc::new(Mutex::new(false));
        let callback_called_clone = callback_called.clone();
        
        let computer = computer.with_confirm_callback(move |_req_id, _server, _tool, _params| {
            // 使用tokio::block_on在同步回调中执行异步操作
            // Use tokio::block_in_async to execute async operations in sync callback
            let rt = tokio::runtime::Handle::current();
            rt.block_on(async {
                let mut called = callback_called_clone.lock().await;
                *called = true;
            });
            true // 确认 / Confirm
        });
        
        // 回调已设置，但实际测试需要MCP服务器
        // Callback is set, but actual testing needs MCP server
    }

    #[tokio::test]
    async fn test_computer_shutdown() {
        let session = SilentSession::new("test");
        let computer = Computer::new(
            "test_computer",
            session,
            None,
            None,
            true,
            true,
        );
        
        // 测试关闭未初始化的Computer / Test shutting down uninitialized computer
        computer.shutdown().await.unwrap();
        
        // 测试关闭已初始化的Computer / Test shutting down initialized computer
        computer.boot_up().await.unwrap();
        computer.shutdown().await.unwrap();
    }
}
