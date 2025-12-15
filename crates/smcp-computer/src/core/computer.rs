/*!
* 文件名: computer.rs
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: tokio, async-trait
* 描述: Computer核心实现 / Computer core implementation
*/

use crate::core::events::{ComputerEvent, EventEmitter};
use crate::core::types::{ToolCallHistory, ToolCallRecord};
use crate::errors::{ComputerError, ComputerResult};
use crate::inputs::{ConfigRender, InputResolver};
use crate::manager::manager::{McpServerManager, ManagerEventHandler};
use crate::mcp_clients::model::{MCPServerConfig, MCPServerInput};
use crate::transport::SmcpComputerClient;
use async_trait::async_trait;
use chrono::Utc;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::{Arc, Weak};
use tokio::sync::{RwLock, Mutex};

/// Computer核心 / Computer core
pub struct ComputerCore {
    /// 计算机名称 / Computer name
    name: String,
    /// MCP服务器管理器 / MCP server manager
    manager: Arc<McpServerManager>,
    /// 输入解析器 / Input resolver
    input_resolver: Arc<dyn InputResolver>,
    /// 配置渲染器 / Configuration renderer
    config_render: ConfigRender,
    /// Socket.IO客户端弱引用 / Weak reference to Socket.IO client
    socketio_client: Arc<RwLock<Option<Weak<SmcpComputerClient>>>>,
    /// 工具调用历史 / Tool call history
    tool_history: Arc<Mutex<ToolCallHistory>>,
    /// 窗口缓存 / Window cache
    windows_cache: Arc<RwLock<HashSet<String>>>,
    /// 事件发射器 / Event emitter
    event_emitter: Arc<Mutex<Option<Box<dyn EventEmitter + Send + Sync>>>>,
}

impl ComputerCore {
    /// 创建新的Computer核心 / Create new Computer core
    pub fn new(
        name: String,
        _inputs: Vec<MCPServerInput>,
        _mcp_servers: Vec<MCPServerConfig>,
        auto_connect: bool,
        auto_reconnect: bool,
        input_resolver: Arc<dyn InputResolver>,
    ) -> Self {
        // 创建事件处理器
        let event_handler = Box::new(ComputerEventHandler::new());
        
        // 创建管理器
        let manager = Arc::new(McpServerManager::new(
            auto_connect,
            auto_reconnect,
            Some(event_handler),
        ));
        
        Self {
            name,
            manager,
            input_resolver,
            config_render: ConfigRender::new(),
            socketio_client: Arc::new(RwLock::new(None)),
            tool_history: Arc::new(Mutex::new(ToolCallHistory::default())),
            windows_cache: Arc::new(RwLock::new(HashSet::new())),
            event_emitter: Arc::new(Mutex::new(None)),
        }
    }
    
    /// 启动计算机 / Boot up computer
    pub async fn boot_up(&self) -> ComputerResult<()> {
        // 获取MCP服务器配置
        let servers = {
            // TODO: 从配置加载服务器列表
            Vec::new()
        };
        
        // 初始化管理器
        self.manager.initialize(servers).await?;
        
        Ok(())
    }
    
    /// 关闭计算机 / Shutdown computer
    pub async fn shutdown(&self) -> ComputerResult<()> {
        self.manager.close().await?;
        
        // 清理Socket.IO客户端引用
        *self.socketio_client.write().await = None;
        
        Ok(())
    }
    
    /// 设置Socket.IO客户端 / Set Socket.IO client
    pub async fn set_socketio_client(&self, client: Arc<SmcpComputerClient>) {
        *self.socketio_client.write().await = Some(Arc::downgrade(&client));
    }
    
    /// 获取计算机名称 / Get computer name
    pub fn name(&self) -> &str {
        &self.name
    }
    
    /// 添加或更新服务器 / Add or update server
    pub async fn add_or_update_server(&self, server: MCPServerConfig) -> ComputerResult<()> {
        // 渲染配置
        let rendered = self.render_server_config(&server).await?;
        
        // 添加到管理器
        self.manager.add_or_update_server(rendered).await?;
        
        // 发射配置变更事件
        self.emit_event(ComputerEvent::ConfigChanged {
            server_name: Some(server.name().to_string()),
        }).await;
        
        Ok(())
    }
    
    /// 移除服务器 / Remove server
    pub async fn remove_server(&self, server_name: &str) -> ComputerResult<()> {
        self.manager.remove_server(server_name).await?;
        
        // 发射配置变更事件
        self.emit_event(ComputerEvent::ConfigChanged {
            server_name: Some(server_name.to_string()),
        }).await;
        
        Ok(())
    }
    
    /// 获取可用工具 / Get available tools
    pub async fn get_available_tools(&self) -> ComputerResult<Vec<Value>> {
        let tools = self.manager.get_available_tools().await?;
        
        // 转换为SMCP工具格式
        let mut result = Vec::new();
        for tool in tools {
            let smcp_tool = self.convert_mcp_tool(tool).await?;
            result.push(serde_json::to_value(smcp_tool)?);
        }
        
        Ok(result)
    }
    
    /// 执行工具 / Execute tool
    pub async fn execute_tool(
        &self,
        req_id: &str,
        tool_name: &str,
        parameters: Value,
        timeout: Option<f64>,
    ) -> ComputerResult<Value> {
        // 验证工具调用
        let (server_name, tool_name) = self.manager.validate_tool_call(tool_name, &parameters).await?;
        
        // 记录调用开始
        let timestamp = Utc::now();
        let mut success = false;
        let mut error = None;
        
        // 执行工具
        let result = match self.manager.call_tool(&server_name, &tool_name, parameters.clone(), timeout).await {
            Ok(result) => {
                success = true;
                result
            }
            Err(e) => {
                error = Some(e.to_string());
                Value::Null
            }
        };
        
        // 记录调用历史
        let record = ToolCallRecord {
            timestamp,
            req_id: req_id.to_string(),
            server: server_name,
            tool: tool_name,
            parameters,
            timeout,
            success,
            error,
        };
        
        {
            let mut history = self.tool_history.lock().await;
            history.push(record);
        }
        
        Ok(result)
    }
    
    /// 获取桌面信息 / Get desktop
    pub async fn get_desktop(&self, _size: Option<usize>, _window_uri: Option<&str>) -> ComputerResult<Vec<Value>> {
        // 获取所有窗口资源
        let windows = self.manager.list_windows().await?;
        
        // 过滤window://协议的资源
        let mut window_uris = Vec::new();
        for (_, resource) in windows {
            if resource.uri.starts_with("window://") {
                window_uris.push(resource.uri.clone());
            }
        }
        
        // 检查是否有变化
        let current_set: HashSet<String> = window_uris.iter().cloned().collect();
        let mut cache = self.windows_cache.write().await;
        if current_set != *cache {
            *cache = current_set.clone();
            
            // 发射桌面变更事件
            drop(cache);
            self.emit_event(ComputerEvent::DesktopChanged {
                window_uris: window_uris.clone(),
            }).await;
        }
        
        // TODO: 组织桌面布局
        // 这里需要实现类似Python版本的organize_desktop逻辑
        Ok(Vec::new())
    }
    
    /// 更新输入定义 / Update inputs
    pub async fn update_inputs(&self, _inputs: Vec<MCPServerInput>) -> ComputerResult<()> {
        // TODO: 实现输入更新逻辑
        Ok(())
    }
    
    /// 获取输入值 / Get input value
    pub async fn get_input_value(&self, input_id: &str) -> ComputerResult<Option<Value>> {
        let value = self.input_resolver.get_cached_value(input_id).await;
        Ok(value.map(|v| v.into()))
    }
    
    /// 设置输入值 / Set input value
    pub async fn set_input_value(&self, input_id: &str, value: Value) -> ComputerResult<bool> {
        let input_value = value.into();
        Ok(self.input_resolver.set_cached_value(input_id, input_value).await)
    }
    
    /// 获取工具调用历史 / Get tool call history
    pub async fn get_tool_history(&self) -> ComputerResult<Vec<ToolCallRecord>> {
        let history = self.tool_history.lock().await;
        Ok(history.get_all())
    }
    
    /// 渲染服务器配置 / Render server configuration
    async fn render_server_config(&self, config: &MCPServerConfig) -> ComputerResult<MCPServerConfig> {
        let config_json = serde_json::to_value(config)?;
        let resolver = self.input_resolver.as_ref();
        let rendered = self.config_render.render(config_json, resolver).await?;
        
        serde_json::from_value(rendered)
            .map_err(ComputerError::SerializationError)
    }
    
    /// 转换MCP工具为SMCP工具 / Convert MCP tool to SMCP tool
    async fn convert_mcp_tool(&self, tool: crate::mcp_clients::base::McpTool) -> ComputerResult<Value> {
        // TODO: 实现工具转换逻辑
        // 需要处理元数据序列化等
        Ok(json!({
            "name": tool.name,
            "description": tool.description,
            "params_schema": tool.input_schema,
            "return_schema": tool.output_schema,
            "meta": tool.meta
        }))
    }
    
    /// 发射事件 / Emit event
    async fn emit_event(&self, event: ComputerEvent) {
        if let Some(emitter) = self.event_emitter.lock().await.as_ref() {
            emitter.emit(event);
        }
    }
    
    /// 设置事件发射器 / Set event emitter
    pub async fn set_event_emitter(&self, emitter: Box<dyn EventEmitter + Send + Sync>) {
        *self.event_emitter.lock().await = Some(emitter);
    }
}

/// Computer事件处理器 / Computer event handler
struct ComputerEventHandler {
    computer: Arc<RwLock<Option<Weak<ComputerCore>>>>,
}

impl ComputerEventHandler {
    fn new() -> Self {
        Self {
            computer: Arc::new(RwLock::new(None)),
        }
    }
    
    /// 设置Computer引用 / Set computer reference
    #[allow(dead_code)]
    async fn set_computer(&self, computer: Arc<ComputerCore>) {
        *self.computer.write().await = Some(Arc::downgrade(&computer));
    }
}

#[async_trait]
impl ManagerEventHandler for ComputerEventHandler {
    async fn on_tools_changed(&self, server_name: &str, tools: Vec<String>) {
        if let Some(computer) = self.computer.read().await.as_ref().and_then(|w| w.upgrade()) {
            // 发射工具列表变更事件
            computer.emit_event(ComputerEvent::ToolListChanged {
                server_name: server_name.to_string(),
                tools,
            }).await;
        }
    }
    
    async fn on_resources_changed(&self, _server_name: &str, resources: Vec<String>) {
        if let Some(computer) = self.computer.read().await.as_ref().and_then(|w| w.upgrade()) {
            // 检查是否有window://资源变化
            let has_window_changes = resources.iter().any(|r| r.starts_with("window://"));
            if has_window_changes {
                // 发射桌面变更事件
                computer.emit_event(ComputerEvent::DesktopChanged {
                    window_uris: resources,
                }).await;
            }
        }
    }
}
