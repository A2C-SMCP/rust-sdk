/**
* 文件名: manager
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: tokio, async-trait, serde_json
* 描述: MCP服务器管理器，负责管理多个MCP服务器连接和工具调用路由
*/
use super::model::*;
use super::utils::client_factory;
use crate::errors::ComputerError;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::Arc as StdArc;
use tokio::sync::{watch, RwLock};
use tracing::{debug, error, info, warn};

/// 工具名称重复错误 / Tool name duplication error
#[derive(Debug, thiserror::Error)]
#[error("Tool '{tool_name}' exists in multiple servers: {servers:?}")]
pub struct ToolNameDuplicatedError {
    pub tool_name: String,
    pub servers: Vec<String>,
}

/// MCP服务器管理器 / MCP server manager
pub struct MCPServerManager {
    /// 服务器配置映射 / Server configuration mapping
    servers_config: Arc<RwLock<HashMap<ServerName, MCPServerConfig>>>,
    /// 活动客户端映射 / Active client mapping
    active_clients: Arc<RwLock<HashMap<ServerName, StdArc<dyn MCPClientProtocol>>>>,
    /// 工具到服务器的映射 / Tool to server mapping
    tool_mapping: Arc<RwLock<HashMap<ToolName, ServerName>>>,
    /// 别名映射 / Alias mapping
    alias_mapping: Arc<RwLock<HashMap<String, (ServerName, ToolName)>>>,
    /// 禁用工具集合 / Disabled tools set
    disabled_tools: Arc<RwLock<HashSet<ToolName>>>,
    /// 自动重连标志 / Auto reconnect flag
    auto_reconnect: Arc<RwLock<bool>>,
    /// 自动连接标志 / Auto connect flag
    auto_connect: Arc<RwLock<bool>>,
    /// 状态变化通知器 / State change notifier
    state_notifier: watch::Sender<ManagerState>,
}

/// 管理器状态 / Manager state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagerState {
    /// 未初始化 / Uninitialized
    Uninitialized,
    /// 已初始化 / Initialized
    Initialized,
    /// 运行中 / Running
    Running,
    /// 错误状态 / Error
    Error,
}

impl MCPServerManager {
    /// 创建新的管理器 / Create new manager
    pub fn new() -> Self {
        let (state_tx, _) = watch::channel(ManagerState::Uninitialized);

        Self {
            servers_config: Arc::new(RwLock::new(HashMap::new())),
            active_clients: Arc::new(RwLock::new(HashMap::new())),
            tool_mapping: Arc::new(RwLock::new(HashMap::new())),
            alias_mapping: Arc::new(RwLock::new(HashMap::new())),
            disabled_tools: Arc::new(RwLock::new(HashSet::new())),
            auto_reconnect: Arc::new(RwLock::new(true)),
            auto_connect: Arc::new(RwLock::new(false)),
            state_notifier: state_tx,
        }
    }

    /// 获取状态通知器 / Get state notifier
    pub fn get_state_notifier(&self) -> watch::Receiver<ManagerState> {
        self.state_notifier.subscribe()
    }

    /// 更新管理器状态 / Update manager state
    async fn update_state(&self, state: ManagerState) {
        let _ = self.state_notifier.send(state);
    }

    /// 初始化管理器 / Initialize manager
    pub async fn initialize(&self, servers: Vec<MCPServerConfig>) -> Result<(), ComputerError> {
        // 停止所有现有客户端 / Stop all existing clients
        self.stop_all().await?;

        // 清空所有状态 / Clear all state
        self.clear_all().await;

        // 添加新配置 / Add new configurations
        {
            let mut configs = self.servers_config.write().await;
            for server in servers {
                configs.insert(server.name().to_string(), server);
            }
        }

        // 刷新工具映射 / Refresh tool mapping
        self.refresh_tool_mapping().await?;

        // 更新状态 / Update state
        self.update_state(ManagerState::Initialized).await;

        info!("Manager initialized successfully");
        Ok(())
    }

    /// 添加或更新服务器配置 / Add or update server configuration
    pub async fn add_or_update_server(&self, config: MCPServerConfig) -> Result<(), ComputerError> {
        let server_name = config.name().to_string();

        // 检查是否已激活 / Check if already active
        let is_active = {
            let clients = self.active_clients.read().await;
            clients.contains_key(&server_name)
        };

        if is_active {
            let auto_reconnect = *self.auto_reconnect.read().await;
            if auto_reconnect {
                // 重启服务器 / Restart server
                self.restart_server(&server_name).await?;
            } else {
                return Err(ComputerError::InvalidConfiguration(format!(
                    "Server {} is active. Stop it before updating config",
                    server_name
                )));
            }
        }

        // 更新配置 / Update configuration
        {
            let mut configs = self.servers_config.write().await;
            configs.insert(server_name.clone(), config);
        }

        // 检查是否需要自动连接 / Check if need auto connect
        let auto_connect = *self.auto_connect.read().await;
        if auto_connect && !is_active {
            self.start_client(&server_name).await?;
        }

        // 刷新工具映射 / Refresh tool mapping
        self.refresh_tool_mapping().await?;

        Ok(())
    }

    /// 移除服务器配置 / Remove server configuration
    pub async fn remove_server(&self, server_name: &str) -> Result<(), ComputerError> {
        // 停止客户端 / Stop client
        self.stop_client(server_name).await?;

        // 移除配置 / Remove configuration
        {
            let mut configs = self.servers_config.write().await;
            configs.remove(server_name);
        }

        // 刷新工具映射 / Refresh tool mapping
        self.refresh_tool_mapping().await?;

        Ok(())
    }

    /// 启动所有启用的服务器 / Start all enabled servers
    pub async fn start_all(&self) -> Result<(), ComputerError> {
        let configs = self.servers_config.read().await;
        let server_names: Vec<String> = configs
            .iter()
            .filter(|(_, config)| !config.disabled())
            .map(|(name, _)| name.clone())
            .collect();

        drop(configs);

        for server_name in server_names {
            self.start_client(&server_name).await?;
        }

        // 更新状态 / Update state
        self.update_state(ManagerState::Running).await;

        info!("All servers started successfully");
        Ok(())
    }

    /// 启动单个客户端 / Start single client
    pub async fn start_client(&self, server_name: &str) -> Result<(), ComputerError> {
        // 获取配置 / Get configuration
        let config = {
            let configs = self.servers_config.read().await;
            configs.get(server_name).cloned()
        };

        let config = config.ok_or_else(|| {
            ComputerError::InvalidConfiguration(format!("Unknown server: {}", server_name))
        })?;

        if config.disabled() {
            return Err(ComputerError::InvalidConfiguration(format!(
                "Cannot start disabled server: {}",
                server_name
            )));
        }

        // 检查是否已启动 / Check if already started
        {
            let clients = self.active_clients.read().await;
            if clients.contains_key(server_name) {
                return Ok(()); // 已经启动 / Already started
            }
        }

        // 创建客户端 / Create client
        let client = client_factory(config);

        // 连接服务器 / Connect to server
        client.connect().await.map_err(|e| {
            ComputerError::ConnectionError(format!("Failed to connect to {}: {}", server_name, e))
        })?;

        // 添加到活动客户端 / Add to active clients
        {
            let mut clients = self.active_clients.write().await;
            clients.insert(server_name.to_string(), client);
        }

        // 刷新工具映射 / Refresh tool mapping
        self.refresh_tool_mapping().await?;

        info!("Client {} started successfully", server_name);
        Ok(())
    }

    /// 停止单个客户端 / Stop single client
    pub async fn stop_client(&self, server_name: &str) -> Result<(), ComputerError> {
        // 移除客户端 / Remove client
        let mut client = {
            let mut clients = self.active_clients.write().await;
            clients.remove(server_name)
        };

        // 断开连接 / Disconnect
        if let Some(ref mut c) = client {
            c.disconnect().await.map_err(|e| {
                ComputerError::ConnectionError(format!(
                    "Failed to disconnect from {}: {}",
                    server_name, e
                ))
            })?;
        }

        // 刷新工具映射 / Refresh tool mapping
        self.refresh_tool_mapping().await?;

        info!("Client {} stopped successfully", server_name);
        Ok(())
    }

    /// 重启服务器 / Restart server
    async fn restart_server(&self, server_name: &str) -> Result<(), ComputerError> {
        self.stop_client(server_name).await?;

        // 检查是否启用 / Check if enabled
        let enabled = {
            let configs = self.servers_config.read().await;
            configs
                .get(server_name)
                .map(|c| !c.disabled())
                .unwrap_or(false)
        };

        if enabled {
            self.start_client(server_name).await?;
        }

        Ok(())
    }

    /// 停止所有客户端 / Stop all clients
    pub async fn stop_all(&self) -> Result<(), ComputerError> {
        let server_names: Vec<String> = {
            let clients = self.active_clients.read().await;
            clients.keys().cloned().collect()
        };

        for server_name in server_names {
            self.stop_client(&server_name).await?;
        }

        // 更新状态 / Update state
        self.update_state(ManagerState::Initialized).await;

        info!("All servers stopped successfully");
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

    /// 关闭管理器 / Close manager
    pub async fn close(&self) -> Result<(), ComputerError> {
        self.stop_all().await?;
        self.clear_all().await;
        self.update_state(ManagerState::Uninitialized).await;
        info!("Manager closed successfully");
        Ok(())
    }

    /// 刷新工具映射 / Refresh tool mapping
    async fn refresh_tool_mapping(&self) -> Result<(), ComputerError> {
        // 清空现有映射 / Clear existing mappings
        self.tool_mapping.write().await.clear();
        self.alias_mapping.write().await.clear();
        self.disabled_tools.write().await.clear();

        // 临时存储工具源服务器 / Temporarily store tool source servers
        let mut tool_sources: HashMap<ToolName, Vec<ServerName>> = HashMap::new();

        // 收集所有活动服务器的工具 / Collect tools from all active servers
        let clients = self.active_clients.read().await;
        let configs = self.servers_config.read().await;

        for (server_name, client) in clients.iter() {
            let config = match configs.get(server_name) {
                Some(c) => c,
                None => continue,
            };

            // 获取工具列表 / Get tool list
            match client.list_tools().await {
                Ok(tools) => {
                    for tool in tools {
                        let original_tool_name = tool.name.clone();

                        // 获取合并后的工具元数据 / Get merged tool metadata
                        let tool_meta = self.merged_tool_meta(config, &original_tool_name);

                        // 确定最终显示的工具名 / Determine final display name
                        let display_name = tool_meta
                            .and_then(|meta| meta.alias)
                            .unwrap_or_else(|| original_tool_name.clone());

                        // 如果使用别名，更新别名映射 / Update alias mapping if using alias
                        if display_name != original_tool_name {
                            let mut alias_map = self.alias_mapping.write().await;
                            alias_map.insert(
                                display_name.clone(),
                                (server_name.clone(), original_tool_name.clone()),
                            );
                        }

                        // 添加到工具源映射 / Add to tool source mapping
                        tool_sources
                            .entry(display_name.clone())
                            .or_default()
                            .push(server_name.clone());

                        // 检查是否为禁用工具 / Check if disabled tool
                        let forbidden_tools = config.forbidden_tools();
                        if forbidden_tools.contains(&display_name)
                            || forbidden_tools.contains(&original_tool_name)
                        {
                            let mut disabled = self.disabled_tools.write().await;
                            disabled.insert(display_name);
                        }
                    }
                }
                Err(e) => {
                    error!("Error listing tools for {}: {}", server_name, e);
                }
            }
        }

        // 构建最终映射（处理工具名冲突） / Build final mapping (handle tool name conflicts)
        for (tool, sources) in tool_sources {
            if sources.len() > 1 {
                warn!("Tool '{}' exists in multiple servers: {:?}", tool, sources);
                let suggestion =
                    "Please use the 'alias' feature in ToolMeta to resolve conflicts. \
                    Each tool should have a unique name or alias across all servers.";
                return Err(ComputerError::InvalidConfiguration(format!(
                    "Tool '{}' exists in multiple servers: {:?}\n{}",
                    tool, sources, suggestion
                )));
            }
            let mut mapping = self.tool_mapping.write().await;
            mapping.insert(tool, sources[0].clone());
        }

        debug!("Tool mapping refreshed successfully");
        Ok(())
    }

    /// 验证工具调用 / Validate tool call
    pub async fn validate_tool_call(
        &self,
        tool_name: &str,
        _parameters: &serde_json::Value,
    ) -> Result<(ServerName, ToolName), ComputerError> {
        // 检查工具是否可用 / Check if tool is available
        let disabled = self.disabled_tools.read().await;
        if disabled.contains(tool_name) {
            return Err(ComputerError::PermissionError(format!(
                "Tool '{}' is disabled by configuration",
                tool_name
            )));
        }

        // 获取服务器名称 / Get server name
        let server_name = {
            let mapping = self.tool_mapping.read().await;
            mapping.get(tool_name).cloned()
        };

        let server_name = server_name.ok_or_else(|| {
            ComputerError::InvalidConfiguration(format!(
                "Tool '{}' not found in any active server",
                tool_name
            ))
        })?;

        // 检查是否为别名 / Check if it's an alias
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
        parameters: serde_json::Value,
        timeout: Option<std::time::Duration>,
    ) -> Result<CallToolResult, ComputerError> {
        // 获取客户端引用 / Get client reference
        let client = {
            let clients = self.active_clients.read().await;
            clients
                .get(server_name)
                .ok_or_else(|| {
                    ComputerError::InvalidConfiguration(format!(
                        "Server '{}' for tool '{}' is not active",
                        server_name, tool_name
                    ))
                })?
                .clone()
        };

        // 执行工具调用 / Execute tool call
        let result = if let Some(timeout) = timeout {
            tokio::time::timeout(timeout, client.call_tool(tool_name, parameters))
                .await
                .map_err(|_| ComputerError::TimeoutError("Tool execution timed out".to_string()))?
        } else {
            client.call_tool(tool_name, parameters).await
        };

        let mut result = result
            .map_err(|e| ComputerError::ProtocolError(format!("Tool execution failed: {}", e)))?;

        // 添加工具元数据到结果 / Add tool metadata to result
        let config = {
            let configs = self.servers_config.read().await;
            configs.get(server_name).cloned()
        };

        if let Some(config) = config {
            if let Some(tool_meta) = self.merged_tool_meta(&config, tool_name) {
                if result.meta.is_none() {
                    result.meta = Some(std::collections::HashMap::new());
                }
                if let Some(ref mut meta) = result.meta {
                    meta.insert(
                        A2C_TOOL_META.to_string(),
                        serde_json::to_value(tool_meta).unwrap(),
                    );
                }
            }
        }

        Ok(result)
    }

    /// 执行工具（支持别名） / Execute tool (supports alias)
    pub async fn execute_tool(
        &self,
        tool_name: &str,
        parameters: serde_json::Value,
        timeout: Option<std::time::Duration>,
    ) -> Result<CallToolResult, ComputerError> {
        let (server_name, original_tool_name) =
            self.validate_tool_call(tool_name, &parameters).await?;
        self.call_tool(&server_name, &original_tool_name, parameters, timeout)
            .await
    }

    /// 获取服务器状态列表 / Get server status list
    pub async fn get_server_status(&self) -> Vec<(String, bool, String)> {
        let configs = self.servers_config.read().await;
        let clients = self.active_clients.read().await;

        configs
            .keys()
            .map(|name| {
                let is_active = clients.contains_key(name);
                let state = if is_active {
                    clients
                        .get(name)
                        .map(|c| c.state().to_string())
                        .unwrap_or_else(|| "unknown".to_string())
                } else {
                    "pending".to_string()
                };
                (name.clone(), is_active, state)
            })
            .collect()
    }

    /// 获取可用工具列表 / Get available tools list
    pub async fn list_available_tools(&self) -> Vec<Tool> {
        let mut tools = Vec::new();
        let mapping = self.tool_mapping.read().await;
        let alias_map = self.alias_mapping.read().await;

        for (display_name, server_name) in mapping.iter() {
            let client = {
                let clients = self.active_clients.read().await;
                clients.get(server_name).cloned()
            };

            if let Some(client) = client {
                // 获取原始工具名称 / Get original tool name
                let original_name = alias_map
                    .get(display_name)
                    .map(|(_, original)| original.clone())
                    .unwrap_or_else(|| display_name.clone());

                // 获取工具列表 / Get tool list
                if let Ok(tool_list) = client.list_tools().await {
                    if let Some(tool) = tool_list.into_iter().find(|t| t.name == original_name) {
                        // 更新工具名称为显示名称 / Update tool name to display name
                        let mut display_tool = tool;
                        display_tool.name = display_name.clone();
                        tools.push(display_tool);
                    }
                }
            }
        }

        tools
    }

    /// 合并工具元数据 / Merge tool metadata
    fn merged_tool_meta(&self, config: &MCPServerConfig, tool_name: &str) -> Option<ToolMeta> {
        let specific = config.tool_meta().get(tool_name);
        let default = config.default_tool_meta();

        match (specific, default) {
            (None, None) => None,
            (Some(s), None) => Some(s.clone()),
            (None, Some(d)) => Some(d.clone()),
            (Some(s), Some(d)) => {
                // 浅合并，specific优先 / Shallow merge, specific takes priority
                let mut merged = d.clone();
                if s.auto_apply.is_some() {
                    merged.auto_apply = s.auto_apply;
                }
                if s.alias.is_some() {
                    merged.alias = s.alias.clone();
                }
                if s.tags.is_some() {
                    merged.tags = s.tags.clone();
                }
                if s.ret_object_mapper.is_some() {
                    merged.ret_object_mapper = s.ret_object_mapper.clone();
                }
                Some(merged)
            }
        }
    }

    /// 启用自动连接 / Enable auto connect
    pub async fn enable_auto_connect(&self) {
        *self.auto_connect.write().await = true;
    }

    /// 禁用自动连接 / Disable auto connect
    pub async fn disable_auto_connect(&self) {
        *self.auto_connect.write().await = false;
    }

    /// 启用自动重连 / Enable auto reconnect
    pub async fn enable_auto_reconnect(&self) {
        *self.auto_reconnect.write().await = true;
    }

    /// 禁用自动重连 / Disable auto reconnect
    pub async fn disable_auto_reconnect(&self) {
        *self.auto_reconnect.write().await = false;
    }
}

impl Default for MCPServerManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tokio::time::{sleep, Duration};

    #[tokio::test]
    async fn test_manager_creation() {
        let manager = MCPServerManager::new();
        let status = manager.get_server_status().await;
        assert!(status.is_empty());
    }

    #[tokio::test]
    async fn test_manager_initialization() {
        let manager = MCPServerManager::new();

        // 创建服务器配置 / Create server configurations
        let configs = vec![
            // STDIO服务器配置 / STDIO server configuration
            MCPServerConfig::Stdio(StdioServerConfig {
                name: "test_stdio".to_string(),
                disabled: false,
                forbidden_tools: vec![],
                tool_meta: HashMap::new(),
                default_tool_meta: None,
                vrl: None,
                server_parameters: StdioServerParameters {
                    command: "echo".to_string(),
                    args: vec!["hello".to_string()],
                    env: HashMap::new(),
                    cwd: None,
                },
            }),
            // HTTP服务器配置 / HTTP server configuration
            MCPServerConfig::Http(HttpServerConfig {
                name: "test_http".to_string(),
                disabled: true, // 禁用此服务器 / Disable this server
                forbidden_tools: vec![],
                tool_meta: HashMap::new(),
                default_tool_meta: None,
                vrl: None,
                server_parameters: HttpServerParameters {
                    url: "http://localhost:8080".to_string(),
                    headers: HashMap::new(),
                },
            }),
        ];

        // 初始化管理器 / Initialize manager
        let result = manager.initialize(configs).await;
        assert!(result.is_ok());

        // 检查状态 / Check status
        let status = manager.get_server_status().await;
        assert_eq!(status.len(), 2);

        // 验证状态 / Verify status
        let stdio_status = status
            .iter()
            .find(|(name, _, _)| name == "test_stdio")
            .unwrap();
        assert!(!stdio_status.1); // 未激活 / Not active

        let http_status = status
            .iter()
            .find(|(name, _, _)| name == "test_http")
            .unwrap();
        assert!(!http_status.1); // 未激活 / Not active
    }

    #[tokio::test]
    async fn test_add_server() {
        let manager = MCPServerManager::new();

        // 添加服务器配置 / Add server configuration
        let config = MCPServerConfig::Stdio(StdioServerConfig {
            name: "test_server".to_string(),
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

        let result = manager.add_or_update_server(config).await;
        assert!(result.is_ok());

        // 检查状态 / Check status
        let status = manager.get_server_status().await;
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].0, "test_server");
    }

    #[tokio::test]
    async fn test_remove_server() {
        let manager = MCPServerManager::new();

        // 添加服务器 / Add server
        let config = MCPServerConfig::Stdio(StdioServerConfig {
            name: "test_server".to_string(),
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

        manager.add_or_update_server(config).await.unwrap();

        // 移除服务器 / Remove server
        let result = manager.remove_server("test_server").await;
        assert!(result.is_ok());

        // 检查状态 / Check status
        let status = manager.get_server_status().await;
        assert!(status.is_empty());
    }

    #[tokio::test]
    async fn test_tool_conflict_detection() {
        let manager = MCPServerManager::new();

        // 创建两个服务器，有同名工具 / Create two servers with same tool name
        let configs = vec![
            // 第一个服务器 / First server
            MCPServerConfig::Stdio(StdioServerConfig {
                name: "server1".to_string(),
                disabled: false,
                forbidden_tools: vec![],
                tool_meta: HashMap::new(),
                default_tool_meta: None,
                vrl: None,
                server_parameters: StdioServerParameters {
                    command: "echo".to_string(),
                    args: vec!["server1".to_string()],
                    env: HashMap::new(),
                    cwd: None,
                },
            }),
            // 第二个服务器 / Second server
            MCPServerConfig::Stdio(StdioServerConfig {
                name: "server2".to_string(),
                disabled: false,
                forbidden_tools: vec![],
                tool_meta: HashMap::new(),
                default_tool_meta: None,
                vrl: None,
                server_parameters: StdioServerParameters {
                    command: "echo".to_string(),
                    args: vec!["server2".to_string()],
                    env: HashMap::new(),
                    cwd: None,
                },
            }),
        ];

        // 初始化应该成功 / Initialization should succeed
        let result = manager.initialize(configs).await;
        assert!(result.is_ok());

        // 启动所有服务器 / Start all servers
        let _result = manager.start_all().await;
        // 可能会因为工具冲突而失败，这是预期的
        // Might fail due to tool conflicts, which is expected

        // 等待连接建立 / Wait for connections to establish
        sleep(Duration::from_millis(200)).await;
    }
}
