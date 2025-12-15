/*!
* 文件名: manager.rs
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: tokio, async-trait
* 描述: MCP服务器管理器实现 / MCP server manager implementation
*/

use crate::errors::{ComputerError, ComputerResult};
use crate::mcp_clients::base::{McpClient, McpTool, McpResource};
use crate::mcp_clients::client_factory;
use crate::mcp_clients::model::{MCPServerConfig, ToolMeta};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// 事件处理器trait / Event handler trait
#[async_trait]
pub trait ManagerEventHandler: Send + Sync {
    /// 处理工具列表变更 / Handle tool list change
    async fn on_tools_changed(&self, server_name: &str, tools: Vec<String>);
    
    /// 处理资源列表变更 / Handle resource list change
    async fn on_resources_changed(&self, server_name: &str, resources: Vec<String>);
}

/// MCP服务器管理器 / MCP server manager
pub struct McpServerManager {
    /// 服务器配置 / Server configurations
    servers_config: Arc<RwLock<HashMap<String, MCPServerConfig>>>,
    /// 活动客户端 / Active clients
    active_clients: Arc<RwLock<HashMap<String, Box<dyn McpClient>>>>,
    /// 工具映射 / Tool mapping (tool_name -> server_name)
    tool_mapping: Arc<RwLock<HashMap<String, String>>>,
    /// 别名映射 / Alias mapping (alias -> (server_name, original_name))
    alias_mapping: Arc<RwLock<HashMap<String, (String, String)>>>,
    /// 禁用工具集合 / Disabled tools
    disabled_tools: Arc<RwLock<std::collections::HashSet<String>>>,
    /// 自动连接标志 / Auto connect flag
    auto_connect: bool,
    /// 自动重连标志 / Auto reconnect flag
    auto_reconnect: bool,
    /// 事件处理器 / Event handler
    event_handler: Option<Box<dyn ManagerEventHandler>>,
}

impl McpServerManager {
    /// 创建新的管理器 / Create new manager
    pub fn new(
        auto_connect: bool,
        auto_reconnect: bool,
        event_handler: Option<Box<dyn ManagerEventHandler>>,
    ) -> Self {
        Self {
            servers_config: Arc::new(RwLock::new(HashMap::new())),
            active_clients: Arc::new(RwLock::new(HashMap::new())),
            tool_mapping: Arc::new(RwLock::new(HashMap::new())),
            alias_mapping: Arc::new(RwLock::new(HashMap::new())),
            disabled_tools: Arc::new(RwLock::new(std::collections::HashSet::new())),
            auto_connect,
            auto_reconnect,
            event_handler,
        }
    }
    
    /// 初始化管理器 / Initialize manager
    pub async fn initialize(&self, servers: Vec<MCPServerConfig>) -> ComputerResult<()> {
        // 停止所有现有客户端
        self.stop_all().await?;
        
        // 清空所有状态
        self.clear_all().await;
        
        // 添加新配置
        {
            let mut config = self.servers_config.write().await;
            for server in servers {
                config.insert(server.name().to_string(), server);
            }
        }
        
        // 如果启用自动连接，启动所有客户端
        if self.auto_connect {
            self.start_all().await?;
        }
        
        // 刷新工具映射
        self.refresh_tool_mapping().await?;
        
        Ok(())
    }
    
    /// 添加或更新服务器 / Add or update server
    pub async fn add_or_update_server(&self, config: MCPServerConfig) -> ComputerResult<()> {
        let server_name = config.name().to_string();
        
        // 检查服务器是否已存在且处于活动状态
        let is_active = {
            let clients = self.active_clients.read().await;
            clients.contains_key(&server_name)
        };
        
        if is_active {
            if self.auto_reconnect {
                // 重启服务器
                self.restart_server(&server_name).await?;
            } else {
                return Err(ComputerError::RuntimeError(
                    format!("Server {} is active. Stop it before updating config", server_name)
                ));
            }
        }
        
        // 更新配置
        {
            let mut config_map = self.servers_config.write().await;
            config_map.insert(server_name.clone(), config);
        }
        
        // 如果启用自动连接且服务器未禁用，启动客户端
        if self.auto_connect {
            let config = self.servers_config.read().await;
            if let Some(server_config) = config.get(&server_name) {
                if !server_config.is_disabled() {
                    self.start_client(&server_name).await?;
                }
            }
        }
        
        // 刷新工具映射
        self.refresh_tool_mapping().await?;
        
        Ok(())
    }
    
    /// 移除服务器 / Remove server
    pub async fn remove_server(&self, server_name: &str) -> ComputerResult<()> {
        // 停止客户端
        self.stop_client(server_name).await?;
        
        // 移除配置
        {
            let mut config = self.servers_config.write().await;
            config.remove(server_name);
        }
        
        // 刷新工具映射
        self.refresh_tool_mapping().await?;
        
        Ok(())
    }
    
    /// 启动所有客户端 / Start all clients
    pub async fn start_all(&self) -> ComputerResult<()> {
        let config = self.servers_config.read().await;
        for (server_name, server_config) in config.iter() {
            if !server_config.is_disabled() {
                if let Err(e) = self.start_client(server_name).await {
                    eprintln!("Failed to start server {}: {}", server_name, e);
                }
            }
        }
        Ok(())
    }
    
    /// 启动单个客户端 / Start single client
    pub async fn start_client(&self, server_name: &str) -> ComputerResult<()> {
        let config = {
            let config_map = self.servers_config.read().await;
            config_map.get(server_name).cloned()
                .ok_or_else(|| ComputerError::RuntimeError(format!("Unknown server: {}", server_name)))?
        };
        
        if config.is_disabled() {
            return Err(ComputerError::RuntimeError(format!("Cannot start disabled server: {}", server_name)));
        }
        
        // 检查是否已经启动
        {
            let clients = self.active_clients.read().await;
            if clients.contains_key(server_name) {
                return Ok(());
            }
        }
        
        // 创建客户端
        let mut client = client_factory(config)
            .map_err(ComputerError::McpClientError)?;
        
        // 连接
        client.connect().await
            .map_err(|e| ComputerError::ConnectionError(e.to_string()))?;
        
        // 添加到活动客户端
        {
            let mut clients = self.active_clients.write().await;
            clients.insert(server_name.to_string(), client);
        }
        
        // 刷新工具映射
        self.refresh_tool_mapping().await?;
        
        Ok(())
    }
    
    /// 停止单个客户端 / Stop single client
    pub async fn stop_client(&self, server_name: &str) -> ComputerResult<()> {
        let client = {
            let mut clients = self.active_clients.write().await;
            clients.remove(server_name)
        };
        
        if let Some(mut client) = client {
            client.disconnect().await
                .map_err(|e| ComputerError::ConnectionError(e.to_string()))?;
        }
        
        // 刷新工具映射
        self.refresh_tool_mapping().await?;
        
        Ok(())
    }
    
    /// 停止所有客户端 / Stop all clients
    pub async fn stop_all(&self) -> ComputerResult<()> {
        let clients: Vec<_> = {
            let clients = self.active_clients.read().await;
            clients.keys().cloned().collect()
        };
        
        for server_name in clients {
            if let Err(e) = self.stop_client(&server_name).await {
                eprintln!("Failed to stop server {}: {}", server_name, e);
            }
        }
        
        Ok(())
    }
    
    /// 重启服务器 / Restart server
    async fn restart_server(&self, server_name: &str) -> ComputerResult<()> {
        self.stop_client(server_name).await?;
        
        let config = {
            let config_map = self.servers_config.read().await;
            config_map.get(server_name).cloned()
                .ok_or_else(|| ComputerError::RuntimeError(format!("Server {} not found", server_name)))?
        };
        
        if !config.is_disabled() {
            self.start_client(server_name).await?;
        }
        
        Ok(())
    }
    
    /// 清空所有状态 / Clear all state
    async fn clear_all(&self) {
        self.servers_config.write().await.clear();
        self.active_clients.write().await.clear();
        self.tool_mapping.write().await.clear();
        self.alias_mapping.write().await.clear();
        self.disabled_tools.write().await.clear();
    }
    
    /// 刷新工具映射 / Refresh tool mapping
    pub async fn refresh_tool_mapping(&self) -> ComputerResult<()> {
        // 清空现有映射
        self.tool_mapping.write().await.clear();
        self.alias_mapping.write().await.clear();
        self.disabled_tools.write().await.clear();
        
        // 收集所有工具
        let mut tool_sources: HashMap<String, Vec<String>> = HashMap::new();
        
        let clients = self.active_clients.read().await;
        let config_map = self.servers_config.read().await;
        
        for (server_name, client) in clients.iter() {
            let config = match config_map.get(server_name) {
                Some(c) => c,
                None => continue,
            };
            
            // 获取工具列表
            let tools = match client.list_tools().await {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("Error listing tools for {}: {}", server_name, e);
                    continue;
                }
            };
            
            for tool in tools {
                let original_name = tool.name.clone();
                
                // 获取合并后的工具元数据
                let tool_meta = self.get_merged_tool_meta(config, &original_name);
                
                // 确定显示名称（优先使用别名）
                let display_name = tool_meta
                    .and_then(|m| m.alias)
                    .unwrap_or_else(|| original_name.clone());
                
                // 如果使用了别名，更新别名映射
                if display_name != original_name {
                    let mut alias_map = self.alias_mapping.write().await;
                    alias_map.insert(display_name.clone(), (server_name.clone(), original_name.clone()));
                }
                
                // 添加到工具源映射
                tool_sources.entry(display_name.clone())
                    .or_default()
                    .push(server_name.clone());
                
                // 检查是否为禁用工具
                let forbidden_tools = config.forbidden_tools();
                if forbidden_tools.contains(&display_name) || forbidden_tools.contains(&original_name) {
                    let mut disabled = self.disabled_tools.write().await;
                    disabled.insert(display_name);
                }
            }
        }
        
        // 构建最终映射（检查冲突）
        for (tool, sources) in tool_sources {
            if sources.len() > 1 {
                return Err(ComputerError::RuntimeError(
                    format!("Tool '{}' exists in multiple servers: {:?}. Please use the 'alias' feature in ToolMeta to resolve conflicts.", 
                           tool, sources)
                ));
            }
            
            let mut tool_map = self.tool_mapping.write().await;
            tool_map.insert(tool, sources.into_iter().next().unwrap());
        }
        
        // 通知事件处理器
        if let Some(handler) = &self.event_handler {
            let tools: Vec<String> = self.tool_mapping.read().await.keys().cloned().collect();
            handler.on_tools_changed("all", tools).await;
        }
        
        Ok(())
    }
    
    /// 验证工具调用 / Validate tool call
    pub async fn validate_tool_call(&self, tool_name: &str, _params: &Value) -> ComputerResult<(String, String)> {
        // 检查工具是否被禁用
        {
            let disabled = self.disabled_tools.read().await;
            if disabled.contains(tool_name) {
                return Err(ComputerError::RuntimeError(format!("Tool '{}' is disabled by configuration", tool_name)));
            }
        }
        
        // 获取服务器名称
        let server_name = {
            let tool_map = self.tool_mapping.read().await;
            tool_map.get(tool_name).cloned()
                .ok_or_else(|| ComputerError::RuntimeError(format!("Tool '{}' not found in any active server", tool_name)))?
        };
        
        // 如果是别名，获取原始工具名
        let original_tool_name = {
            let alias_map = self.alias_mapping.read().await;
            if let Some((_, original)) = alias_map.get(tool_name) {
                original.clone()
            } else {
                tool_name.to_string()
            }
        };
        
        Ok((server_name, original_tool_name))
    }
    
    /// 调用工具 / Call tool
    pub async fn call_tool(
        &self,
        server_name: &str,
        tool_name: &str,
        params: Value,
        timeout: Option<f64>,
    ) -> ComputerResult<Value> {
        // 获取客户端引用
        let clients = self.active_clients.read().await;
        let client = clients.get(server_name)
            .ok_or_else(|| ComputerError::RuntimeError(format!("Server '{}' is not active", server_name)))?;
        
        // 执行工具调用
        let result = if let Some(timeout) = timeout {
            tokio::time::timeout(
                std::time::Duration::from_secs_f64(timeout),
                client.call_tool(tool_name, params)
            ).await
                .map_err(|_| ComputerError::RuntimeError("Tool execution timeout".to_string()))?
                .map_err(|e| ComputerError::RuntimeError(e.to_string()))?
        } else {
            client.call_tool(tool_name, params).await
                .map_err(|e| ComputerError::RuntimeError(e.to_string()))?
        };
        
        // 转换为JSON值
        serde_json::to_value(result)
            .map_err(ComputerError::SerializationError)
    }
    
    /// 获取可用工具 / Get available tools
    pub async fn get_available_tools(&self) -> ComputerResult<Vec<McpTool>> {
        let mut result = Vec::new();
        let clients = self.active_clients.read().await;
        
        for client in clients.values() {
            let tools = client.list_tools().await
                .map_err(|e| ComputerError::RuntimeError(e.to_string()))?;
            result.extend(tools);
        }
        
        Ok(result)
    }
    
    /// 列出窗口资源 / List windows
    pub async fn list_windows(&self) -> ComputerResult<Vec<(String, McpResource)>> {
        let mut result = Vec::new();
        let clients = self.active_clients.read().await;
        
        for (server_name, client) in clients.iter() {
            let windows = client.list_windows().await
                .map_err(|e| ComputerError::RuntimeError(e.to_string()))?;
            for window in windows {
                result.push((server_name.clone(), window));
            }
        }
        
        Ok(result)
    }
    
    /// 获取服务器状态 / Get server status
    pub async fn get_server_status(&self) -> Vec<(String, bool, String)> {
        let mut result = Vec::new();
        let config_map = self.servers_config.read().await;
        let clients = self.active_clients.read().await;
        
        for (server_name, _config) in config_map.iter() {
            let active = clients.contains_key(server_name);
            let state = if active {
                clients.get(server_name)
                    .map(|c| c.state().to_string())
                    .unwrap_or_else(|| "unknown".to_string())
            } else {
                "disconnected".to_string()
            };
            
            result.push((server_name.clone(), active, state));
        }
        
        result
    }
    
    /// 获取合并后的工具元数据 / Get merged tool metadata
    fn get_merged_tool_meta(&self, config: &MCPServerConfig, tool_name: &str) -> Option<ToolMeta> {
        let specific = config.tool_meta().get(tool_name);
        let default = config.default_tool_meta();
        
        match (specific, default) {
            (Some(specific), Some(default)) => {
                // 浅合并，specific优先
                let mut merged = default.clone();
                if specific.auto_apply.is_some() {
                    merged.auto_apply = specific.auto_apply;
                }
                if specific.alias.is_some() {
                    merged.alias = specific.alias.clone();
                }
                if specific.tags.is_some() {
                    merged.tags = specific.tags.clone();
                }
                if specific.ret_object_mapper.is_some() {
                    merged.ret_object_mapper = specific.ret_object_mapper.clone();
                }
                // 合并额外字段
                for (k, v) in &specific.extra {
                    merged.extra.insert(k.clone(), v.clone());
                }
                Some(merged)
            }
            (Some(specific), None) => Some(specific.clone()),
            (None, Some(default)) => Some(default.clone()),
            (None, None) => None,
        }
    }
    
    /// 关闭管理器 / Close manager
    pub async fn close(&self) -> ComputerResult<()> {
        self.stop_all().await?;
        self.clear_all().await;
        Ok(())
    }
}
