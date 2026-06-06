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
use super::vrl_runtime::VrlRuntime;
use crate::errors::ComputerError;
use crate::skills::{McpResource, SkillResourceManager, SkillStagingError};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::Arc as StdArc;
use tokio::sync::{watch, RwLock};
use tracing::{debug, error, info, warn};

/// `list_skill_resources` 单 server 翻页上限（防御非终止 cursor）/ per-server page cap for
/// `list_skill_resources` (guards a non-terminating cursor)。对标 Python `_MAX_SKILL_LIST_PAGES`。
const MAX_SKILL_LIST_PAGES: usize = 1000;

/// SKILL 资源 URI scheme 前缀 / SKILL resource URI scheme prefix。
const SKILL_URI_PREFIX: &str = "skill://";

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
    /// 健康检查配置 / Health check configuration
    health_check_config: Arc<RwLock<HealthCheckConfig>>,
    /// 重连策略 / Reconnect policy
    reconnect_policy: Arc<RwLock<ReconnectPolicy>>,
    /// 健康监控任务句柄 / Health monitor task handle
    health_monitor_handle: Arc<RwLock<Option<tokio::task::JoinHandle<()>>>>,
    /// 重试计数器（服务器名 -> 重试次数）/ Retry counters (server name -> retry count)
    retry_counts: Arc<RwLock<HashMap<ServerName, u32>>>,
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
            health_check_config: Arc::new(RwLock::new(HealthCheckConfig::default())),
            reconnect_policy: Arc::new(RwLock::new(ReconnectPolicy::default())),
            health_monitor_handle: Arc::new(RwLock::new(None)),
            retry_counts: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// 使用自定义配置创建管理器 / Create manager with custom configuration
    pub fn with_config(
        health_check_config: HealthCheckConfig,
        reconnect_policy: ReconnectPolicy,
    ) -> Self {
        let (state_tx, _) = watch::channel(ManagerState::Uninitialized);

        Self {
            servers_config: Arc::new(RwLock::new(HashMap::new())),
            active_clients: Arc::new(RwLock::new(HashMap::new())),
            tool_mapping: Arc::new(RwLock::new(HashMap::new())),
            alias_mapping: Arc::new(RwLock::new(HashMap::new())),
            disabled_tools: Arc::new(RwLock::new(HashSet::new())),
            auto_reconnect: Arc::new(RwLock::new(reconnect_policy.enabled)),
            auto_connect: Arc::new(RwLock::new(false)),
            state_notifier: state_tx,
            health_check_config: Arc::new(RwLock::new(health_check_config)),
            reconnect_policy: Arc::new(RwLock::new(reconnect_policy)),
            health_monitor_handle: Arc::new(RwLock::new(None)),
            retry_counts: Arc::new(RwLock::new(HashMap::new())),
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
                            .unwrap_or_else(|| original_tool_name.to_string());

                        // 如果使用别名，更新别名映射 / Update alias mapping if using alias
                        let original_tool_name_str = original_tool_name.to_string();
                        if display_name != original_tool_name_str {
                            let mut alias_map = self.alias_mapping.write().await;
                            alias_map.insert(
                                display_name.clone(),
                                (server_name.clone(), original_tool_name_str.clone()),
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
                            || forbidden_tools.contains(&original_tool_name_str)
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
                    result.meta = Some(rmcp::model::Meta::new());
                }
                if let Some(ref mut meta) = result.meta {
                    meta.insert(
                        A2C_TOOL_META.to_string(),
                        serde_json::to_value(tool_meta).unwrap(),
                    );
                }
            }

            // VRL转换 / VRL transformation
            if let Some(vrl_script) = config.vrl() {
                // 获取原始参数用于VRL处理
                // Note: 这里需要从调用栈获取原始参数，暂时使用空对象
                let parameters = serde_json::json!({});

                // 创建VRL事件，包含工具调用结果和元数据
                let mut event = serde_json::to_value(&result).unwrap_or_default();
                if let Value::Object(ref mut map) = event {
                    map.insert(
                        "tool_name".to_string(),
                        Value::String(tool_name.to_string()),
                    );
                    map.insert("parameters".to_string(), parameters);
                }

                // 执行VRL转换
                let mut runtime = VrlRuntime::new();
                match runtime.run(vrl_script, event, "UTC") {
                    Ok(vrl_result) => {
                        // 将转换后的结果存储到meta中
                        if result.meta.is_none() {
                            result.meta = Some(rmcp::model::Meta::new());
                        }
                        if let Some(ref mut meta) = result.meta {
                            // 将转换后的结果序列化为JSON字符串
                            if let Ok(transformed_json) =
                                serde_json::to_string(&vrl_result.processed_event)
                            {
                                meta.insert(
                                    A2C_VRL_TRANSFORMED.to_string(),
                                    Value::String(transformed_json),
                                );
                            }
                        }
                        debug!(
                            "VRL转换成功 / VRL transformation succeeded for tool '{}'",
                            tool_name
                        );
                    }
                    Err(e) => {
                        warn!(
                            "VRL转换失败 / VRL transformation failed for tool '{}': {}. 原始结果将正常返回 / Original result will be returned normally.",
                            tool_name, e
                        );
                    }
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

    /// 获取所有服务器配置（用于 GetComputerConfigRet）
    /// Get all server configurations (for GetComputerConfigRet)
    /// 返回格式：{ server_name: { type, status, disabled, ... } }
    /// Returns format: { server_name: { type, status, disabled, ... } }
    pub async fn get_server_configs(&self) -> serde_json::Value {
        let configs = self.servers_config.read().await;
        let clients = self.active_clients.read().await;

        let mut result = serde_json::Map::new();

        for (name, config) in configs.iter() {
            let is_active = clients.contains_key(name);
            let state = if is_active {
                clients
                    .get(name)
                    .map(|c| c.state().to_string())
                    .unwrap_or_else(|| "unknown".to_string())
            } else {
                "pending".to_string()
            };

            // 构建服务器配置信息 / Build server config info
            let mut server_info = serde_json::Map::new();

            // 添加类型信息 / Add type info
            let server_type = match config {
                MCPServerConfig::Stdio(_) => "stdio",
                MCPServerConfig::Sse(_) => "sse",
                MCPServerConfig::Http(_) => "http",
            };
            server_info.insert(
                "type".to_string(),
                serde_json::Value::String(server_type.to_string()),
            );

            // 添加状态信息 / Add status info
            server_info.insert("status".to_string(), serde_json::Value::String(state));
            server_info.insert("is_active".to_string(), serde_json::Value::Bool(is_active));
            server_info.insert(
                "disabled".to_string(),
                serde_json::Value::Bool(config.disabled()),
            );

            // 添加禁用工具列表 / Add forbidden tools list
            let forbidden_tools: Vec<serde_json::Value> = config
                .forbidden_tools()
                .iter()
                .map(|t| serde_json::Value::String(t.clone()))
                .collect();
            server_info.insert(
                "forbidden_tools".to_string(),
                serde_json::Value::Array(forbidden_tools),
            );

            // 添加工具元数据 / Add tool metadata
            if let Ok(tool_meta_json) = serde_json::to_value(config.tool_meta()) {
                server_info.insert("tool_meta".to_string(), tool_meta_json);
            }

            // 添加默认工具元数据 / Add default tool metadata
            if let Some(default_meta) = config.default_tool_meta() {
                if let Ok(default_meta_json) = serde_json::to_value(default_meta) {
                    server_info.insert("default_tool_meta".to_string(), default_meta_json);
                }
            }

            // 添加 VRL 脚本（如果有）/ Add VRL script if present
            if let Some(vrl) = config.vrl() {
                server_info.insert(
                    "vrl".to_string(),
                    serde_json::Value::String(vrl.to_string()),
                );
            }

            // 添加服务器参数（根据类型）/ Add server parameters based on type
            match config {
                MCPServerConfig::Stdio(stdio_config) => {
                    if let Ok(params_json) = serde_json::to_value(&stdio_config.server_parameters) {
                        server_info.insert("server_parameters".to_string(), params_json);
                    }
                }
                MCPServerConfig::Sse(sse_config) => {
                    if let Ok(params_json) = serde_json::to_value(&sse_config.server_parameters) {
                        server_info.insert("server_parameters".to_string(), params_json);
                    }
                }
                MCPServerConfig::Http(http_config) => {
                    if let Ok(params_json) = serde_json::to_value(&http_config.server_parameters) {
                        server_info.insert("server_parameters".to_string(), params_json);
                    }
                }
            }

            result.insert(name.clone(), serde_json::Value::Object(server_info));
        }

        serde_json::Value::Object(result)
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
                        display_tool.name = display_name.clone().into();

                        // 合并工具元数据 / Merge tool metadata
                        let config = {
                            let configs = self.servers_config.read().await;
                            configs.get(server_name).cloned()
                        };
                        if let Some(config) = config {
                            if let Some(tool_meta) = self.merged_tool_meta(&config, &original_name)
                            {
                                if display_tool.meta.is_none() {
                                    display_tool.meta = Some(rmcp::model::Meta::new());
                                }
                                if let Some(ref mut meta) = display_tool.meta {
                                    meta.insert(
                                        A2C_TOOL_META.to_string(),
                                        serde_json::to_value(tool_meta).unwrap(),
                                    );
                                }
                            }
                        }

                        tools.push(display_tool);
                    }
                }
            }
        }

        tools
    }

    /// 列出所有窗口资源 / List all window resources
    /// 聚合所有活动客户端的 window:// 资源，可选按 URI 完全匹配过滤
    /// Aggregates window:// resources from all active clients, optionally filtered by exact URI match
    pub async fn list_all_windows(&self, window_uri: Option<&str>) -> Vec<(ServerName, Resource)> {
        let clients: Vec<(String, StdArc<dyn MCPClientProtocol>)> = {
            let clients_guard = self.active_clients.read().await;
            clients_guard
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        };

        let mut results = Vec::new();
        for (server_name, client) in clients {
            match client.list_windows().await {
                Ok(windows) => {
                    for resource in windows {
                        if let Some(uri_filter) = window_uri {
                            if resource.uri.as_str() != uri_filter {
                                continue;
                            }
                        }
                        results.push((server_name.clone(), resource));
                    }
                }
                Err(e) => {
                    warn!(
                        "Failed to list windows from server '{}': {}",
                        server_name, e
                    );
                }
            }
        }
        results
    }

    /// 枚举活跃 MCP Server 的 `skill://` 资源（附 server 归属），**完整消费 cursor 翻页直至末尾**。
    /// Enumerate `skill://` resources from active MCP servers (with owning server), exhausting cursor pages。
    ///
    /// 与 [`list_resources_page`](MCPClientProtocol::list_resources_page)（单页、Agent 控制翻页）不同：
    /// SKILL 物化由 Computer 主导，须拿到**全量** `skill://` 集合，故在此完整消费翻页（协议 skill.md §12）。
    /// 未声明 `resources` 能力或枚举出错的 server **跳过**（记 ERROR、不中断其余），对齐「SKILL 通道不使用
    /// 4015——无 resources 能力的 server 在物化阶段即被排除」（skill.md §1.5）。
    /// Unlike `list_resources_page` (single-page, Agent-driven): Computer-driven SKILL materialization needs
    /// the full `skill://` set, so pages are exhausted here. Servers lacking `resources` or erroring are skipped.
    ///
    /// `server_name` 给定则仅枚举该 server（用于 ResourceListChanged 单 server 重枚举）。
    pub async fn list_skill_resources(
        &self,
        server_name: Option<&str>,
    ) -> Vec<(ServerName, Resource)> {
        let clients: Vec<(String, StdArc<dyn MCPClientProtocol>)> = {
            let guard = self.active_clients.read().await;
            guard
                .iter()
                .filter(|(k, _)| server_name.is_none() || server_name == Some(k.as_str()))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        };

        let mut results = Vec::new();
        for (sname, client) in clients {
            let mut cursor: Option<String> = None;
            let mut pages = 0usize;
            loop {
                match client.list_resources_page(cursor.clone()).await {
                    Ok((page, next)) => {
                        for resource in page {
                            if resource.uri.starts_with(SKILL_URI_PREFIX) {
                                results.push((sname.clone(), resource));
                            }
                        }
                        pages += 1;
                        match next {
                            Some(c) => cursor = Some(c),
                            None => break,
                        }
                        if pages >= MAX_SKILL_LIST_PAGES {
                            error!(
                                "list_skill_resources: server '{}' exceeded {} pages \
                                 (non-terminating cursor?); aborting enumeration for this server",
                                sname, MAX_SKILL_LIST_PAGES
                            );
                            break;
                        }
                    }
                    Err(e) => {
                        // 未声明 resources 能力 / 连接异常 / 翻页失败 → 跳过该 server，不阻断其余。
                        error!("Error listing skill resources for '{}': {}", sname, e);
                        break;
                    }
                }
            }
        }
        results
    }

    /// 单页透传指定 MCP Server 的 `resources/list`（v0.2 `client:get_resources` 路由层）。
    /// Single-page passthrough of a named server's `resources/list` (the v0.2 `client:get_resources` router)。
    ///
    /// 仅作透传：定位命名 client → 调 [`list_resources_page`](MCPClientProtocol::list_resources_page)，
    /// 不做 scheme/元数据过滤、不聚合，翻页由调用方经 cursor 控制。未注册 server →
    /// [`ComputerError::McpServerNotFound`]（映射 4014）；无 `resources` 能力 →
    /// [`ComputerError::McpCapabilityNotSupported`]（映射 4015）。
    pub async fn list_resources(
        &self,
        server_name: &str,
        cursor: Option<String>,
    ) -> Result<(Vec<Resource>, Option<String>), ComputerError> {
        let client = {
            let clients = self.active_clients.read().await;
            clients.get(server_name).cloned()
        };
        let client =
            client.ok_or_else(|| ComputerError::McpServerNotFound(server_name.to_string()))?;

        match client.list_resources_page(cursor).await {
            Ok(pair) => Ok(pair),
            Err(MCPClientError::CapabilityNotSupported(cap)) => {
                Err(ComputerError::McpCapabilityNotSupported {
                    server_name: server_name.to_string(),
                    capability: cap,
                })
            }
            Err(e) => Err(ComputerError::ProtocolError(format!(
                "list_resources '{server_name}': {e}"
            ))),
        }
    }

    /// 获取所有窗口资源的详情 / Get details of all window resources
    /// 复用 list_all_windows 聚合窗口列表，再逐个获取内容
    /// Reuses list_all_windows to aggregate window list, then fetches each detail
    pub async fn get_windows_details(
        &self,
        window_uri: Option<&str>,
    ) -> Vec<(ServerName, Resource, ReadResourceResult)> {
        let windows = self.list_all_windows(window_uri).await;

        let clients = self.active_clients.read().await;
        let mut results = Vec::new();
        for (server_name, resource) in windows {
            if let Some(client) = clients.get(&server_name) {
                match client.get_window_detail(resource.clone()).await {
                    Ok(detail) => {
                        results.push((server_name, resource, detail));
                    }
                    Err(e) => {
                        warn!(
                            "Failed to get window detail for '{}' from server '{}': {}",
                            resource.uri, server_name, e
                        );
                    }
                }
            }
        }
        results
    }

    /// 获取单个窗口资源的详情 / Get detail of a single window resource
    /// 通过 server_name 定位客户端，委托调用 get_window_detail
    /// Locates client by server_name and delegates to get_window_detail
    pub async fn get_window_detail(
        &self,
        server_name: &str,
        resource: Resource,
    ) -> Result<ReadResourceResult, ComputerError> {
        let client = {
            let clients = self.active_clients.read().await;
            clients.get(server_name).cloned().ok_or_else(|| {
                ComputerError::InvalidState(format!("Server '{}' not connected", server_name))
            })?
        };
        client
            .get_window_detail(resource)
            .await
            .map_err(|e| ComputerError::ProtocolError(format!("Get window detail error: {}", e)))
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

    /// 设置健康检查配置 / Set health check configuration
    pub async fn set_health_check_config(&self, config: HealthCheckConfig) {
        *self.health_check_config.write().await = config;
    }

    /// 获取健康检查配置 / Get health check configuration
    pub async fn get_health_check_config(&self) -> HealthCheckConfig {
        self.health_check_config.read().await.clone()
    }

    /// 设置重连策略 / Set reconnect policy
    pub async fn set_reconnect_policy(&self, policy: ReconnectPolicy) {
        *self.reconnect_policy.write().await = policy;
    }

    /// 获取重连策略 / Get reconnect policy
    pub async fn get_reconnect_policy(&self) -> ReconnectPolicy {
        self.reconnect_policy.read().await.clone()
    }

    /// 启动健康监控 / Start health monitoring
    /// 定期检查所有活动客户端的健康状态，并在检测到故障时自动重连
    /// Periodically checks health of all active clients and auto-reconnects on failure
    pub async fn start_health_monitor(&self) {
        // 先停止现有的监控任务 / Stop existing monitor task first
        self.stop_health_monitor().await;

        let health_config = self.health_check_config.clone();
        let reconnect_policy = self.reconnect_policy.clone();
        let active_clients = self.active_clients.clone();
        let _servers_config = self.servers_config.clone();
        let retry_counts = self.retry_counts.clone();
        let auto_reconnect = self.auto_reconnect.clone();

        let handle = tokio::spawn(async move {
            loop {
                let config = health_config.read().await.clone();
                if !config.enabled {
                    // 健康检查禁用，等待一段时间后重新检查配置
                    // Health check disabled, wait and re-check config
                    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                    continue;
                }

                // 获取所有活动客户端 / Get all active clients
                let clients: Vec<(String, StdArc<dyn MCPClientProtocol>)> = {
                    let clients_guard = active_clients.read().await;
                    clients_guard
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect()
                };

                // 对每个客户端执行健康检查 / Perform health check on each client
                for (server_name, client) in clients {
                    let check_result = tokio::time::timeout(
                        std::time::Duration::from_secs(config.timeout_secs),
                        client.health_check(),
                    )
                    .await;

                    let is_healthy = match check_result {
                        Ok(result) => result.is_healthy,
                        Err(_) => {
                            warn!("Health check timed out for server: {}", server_name);
                            false
                        }
                    };

                    if !is_healthy {
                        warn!("Server {} is unhealthy", server_name);

                        // 检查是否启用自动重连 / Check if auto-reconnect is enabled
                        let should_reconnect = *auto_reconnect.read().await;
                        if !should_reconnect {
                            continue;
                        }

                        let policy = reconnect_policy.read().await.clone();
                        let mut retries = retry_counts.write().await;
                        let retry_count = retries.entry(server_name.clone()).or_insert(0);

                        if policy.should_retry(*retry_count) {
                            let delay = policy.calculate_delay(*retry_count);
                            info!(
                                "Attempting to reconnect {} (retry {}/{}), delay {:?}",
                                server_name,
                                *retry_count + 1,
                                if policy.max_retries == 0 {
                                    "∞".to_string()
                                } else {
                                    policy.max_retries.to_string()
                                },
                                delay
                            );

                            tokio::time::sleep(delay).await;

                            // 尝试断开并重新连接 / Try disconnect and reconnect
                            if let Err(e) = client.disconnect().await {
                                warn!("Failed to disconnect {}: {}", server_name, e);
                            }

                            match client.connect().await {
                                Ok(_) => {
                                    info!("Successfully reconnected to {}", server_name);
                                    // 重置重试计数 / Reset retry count
                                    *retry_count = 0;
                                }
                                Err(e) => {
                                    error!("Failed to reconnect to {}: {}", server_name, e);
                                    *retry_count += 1;
                                }
                            }
                        } else {
                            error!(
                                "Max retries ({}) reached for server {}. Giving up.",
                                policy.max_retries, server_name
                            );
                            // 可以考虑从活动客户端中移除 / Consider removing from active clients
                        }
                    } else {
                        // 健康检查通过，重置重试计数 / Health check passed, reset retry count
                        let mut retries = retry_counts.write().await;
                        retries.remove(&server_name);
                        debug!("Server {} is healthy", server_name);
                    }
                }

                // 等待下一次健康检查 / Wait for next health check
                tokio::time::sleep(std::time::Duration::from_secs(config.interval_secs)).await;
            }
        });

        *self.health_monitor_handle.write().await = Some(handle);
        info!("Health monitor started");
    }

    /// 停止健康监控 / Stop health monitoring
    pub async fn stop_health_monitor(&self) {
        if let Some(handle) = self.health_monitor_handle.write().await.take() {
            handle.abort();
            info!("Health monitor stopped");
        }
    }

    /// 检查单个服务器的健康状态 / Check health of a single server
    pub async fn check_server_health(&self, server_name: &str) -> Option<HealthCheckResult> {
        let clients = self.active_clients.read().await;
        if let Some(client) = clients.get(server_name) {
            let config = self.health_check_config.read().await;
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(config.timeout_secs),
                client.health_check(),
            )
            .await;

            match result {
                Ok(health_result) => Some(health_result),
                Err(_) => Some(HealthCheckResult {
                    is_healthy: false,
                    checked_at: std::time::Instant::now(),
                    error: Some("Health check timed out".to_string()),
                    response_time_ms: None,
                }),
            }
        } else {
            None
        }
    }

    /// 检查所有服务器的健康状态 / Check health of all servers
    pub async fn check_all_health(&self) -> HashMap<String, HealthCheckResult> {
        let mut results = HashMap::new();
        let clients: Vec<(String, StdArc<dyn MCPClientProtocol>)> = {
            let clients_guard = self.active_clients.read().await;
            clients_guard
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        };

        let config = self.health_check_config.read().await.clone();

        for (server_name, client) in clients {
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(config.timeout_secs),
                client.health_check(),
            )
            .await;

            let health_result = match result {
                Ok(hr) => hr,
                Err(_) => HealthCheckResult {
                    is_healthy: false,
                    checked_at: std::time::Instant::now(),
                    error: Some("Health check timed out".to_string()),
                    response_time_ms: None,
                },
            };

            results.insert(server_name, health_result);
        }

        results
    }

    /// 获取重试计数 / Get retry counts
    pub async fn get_retry_counts(&self) -> HashMap<String, u32> {
        self.retry_counts.read().await.clone()
    }

    /// 重置特定服务器的重试计数 / Reset retry count for a specific server
    pub async fn reset_retry_count(&self, server_name: &str) {
        self.retry_counts.write().await.remove(server_name);
    }

    /// 重置所有重试计数 / Reset all retry counts
    pub async fn reset_all_retry_counts(&self) {
        self.retry_counts.write().await.clear();
    }
}

/// SKILL staging 接缝实现（#74 INT-04）：把 manager 的 rmcp-typed 枚举/读取适配成 staging 层
/// 解耦类型 [`McpResource`] + 字节，供 [`stage_mcp_skills`](crate::skills::stage_mcp_skills) 物化消费。
/// The SKILL staging seam: adapts the manager's rmcp-typed enumeration/read into staging's decoupled
/// [`McpResource`] + bytes, consumed by `stage_mcp_skills`。
#[async_trait::async_trait]
impl SkillResourceManager for MCPServerManager {
    async fn list_skill_resources(
        &self,
        server_name: Option<&str>,
    ) -> Result<Vec<(String, McpResource)>, SkillStagingError> {
        let pairs = MCPServerManager::list_skill_resources(self, server_name).await;
        let mut out = Vec::with_capacity(pairs.len());
        for (sname, resource) in pairs {
            // rmcp `Resource._meta`（`Option<Meta(JsonObject)>`）→ staging 的 `Map<String, Value>`。
            let meta = resource.meta.clone().map(|m| m.0).unwrap_or_default();
            out.push((
                sname,
                McpResource {
                    uri: resource.uri.clone(),
                    meta,
                },
            ));
        }
        Ok(out)
    }

    async fn read_resource(&self, server: &str, uri: &str) -> Result<Vec<u8>, SkillStagingError> {
        // 复用 manager 的通用 read（`get_window_detail` 实为通用 `resources/read`，命名沿用历史）。
        // Reuse the manager's generic read (`get_window_detail` is a generic `resources/read`).
        let resource = make_resource(uri, uri, None, None);
        let result = self
            .get_window_detail(server, resource)
            .await
            .map_err(|e| {
                SkillStagingError(format!("read_resource '{uri}' from '{server}': {e}"))
            })?;

        // 拼接 content blocks → 字节：文本按 UTF-8，二进制按 base64（MCP 标准编码）解码。
        // Concatenate content blocks → bytes: text as UTF-8, blob as standard-base64.
        let mut bytes = Vec::new();
        for content in result.contents {
            match content {
                ResourceContents::TextResourceContents { text, .. } => {
                    bytes.extend_from_slice(text.as_bytes());
                }
                ResourceContents::BlobResourceContents { blob, .. } => {
                    use base64::Engine as _;
                    let decoded = base64::engine::general_purpose::STANDARD
                        .decode(blob.as_bytes())
                        .map_err(|e| SkillStagingError(format!("base64 decode '{uri}': {e}")))?;
                    bytes.extend_from_slice(&decoded);
                }
            }
        }
        Ok(bytes)
    }
}

impl Default for MCPServerManager {
    fn default() -> Self {
        Self::new()
    }
}

/// 测试支撑：可控分页/失败的假 MCP client + 资源/注入助手，`pub(crate)` 供 manager 与 computer 两处
/// 集成测试共用（单一 mock 真源）/ test-support mock + helpers shared by manager and computer tests。
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// 受控分页的假 MCP client：按 cursor 顺序返回各页，可注入翻页失败 / 能力缺失 / read 文本。
    /// Controllable fake MCP client paginating canned pages, with injectable failure/cap-fail/read text.
    pub(crate) struct MockSkillClient {
        pub(crate) pages: Vec<Vec<Resource>>,
        pub(crate) fail: bool,
        /// `list_resources_page` 返回 `CapabilityNotSupported`（模拟无 `resources` 能力）。
        pub(crate) cap_fail: bool,
        pub(crate) read_text: String,
    }

    #[async_trait::async_trait]
    impl MCPClientProtocol for MockSkillClient {
        fn state(&self) -> ClientState {
            ClientState::Connected
        }
        async fn connect(&self) -> Result<(), MCPClientError> {
            Ok(())
        }
        async fn disconnect(&self) -> Result<(), MCPClientError> {
            Ok(())
        }
        async fn list_tools(&self) -> Result<Vec<Tool>, MCPClientError> {
            Ok(vec![])
        }
        async fn call_tool(
            &self,
            _tool: &str,
            _params: Value,
        ) -> Result<CallToolResult, MCPClientError> {
            Err(MCPClientError::ProtocolError("n/a".into()))
        }
        async fn list_windows(&self) -> Result<Vec<Resource>, MCPClientError> {
            Ok(vec![])
        }
        async fn list_resources_page(
            &self,
            cursor: Option<String>,
        ) -> Result<(Vec<Resource>, Option<String>), MCPClientError> {
            if self.cap_fail {
                return Err(MCPClientError::CapabilityNotSupported("resources".into()));
            }
            if self.fail {
                return Err(MCPClientError::ProtocolError("boom".into()));
            }
            let idx: usize = cursor.as_deref().and_then(|c| c.parse().ok()).unwrap_or(0);
            match self.pages.get(idx) {
                Some(page) => {
                    let next = if idx + 1 < self.pages.len() {
                        Some((idx + 1).to_string())
                    } else {
                        None
                    };
                    Ok((page.clone(), next))
                }
                None => Ok((vec![], None)),
            }
        }
        async fn get_window_detail(
            &self,
            _resource: Resource,
        ) -> Result<ReadResourceResult, MCPClientError> {
            Ok(ReadResourceResult {
                contents: vec![ResourceContents::text(self.read_text.clone(), "skill://x")],
            })
        }
        async fn subscribe_window(&self, _resource: Resource) -> Result<(), MCPClientError> {
            Ok(())
        }
        async fn unsubscribe_window(&self, _resource: Resource) -> Result<(), MCPClientError> {
            Ok(())
        }
    }

    /// 构造带 `_meta.source` 的 `skill://` 资源（mount_dir 固定占位）/ a `skill://` resource with `_meta.source`。
    pub(crate) fn skill_resource(uri: &str, source: Option<&str>) -> Resource {
        skill_resource_mounted(uri, source, "/tmp/mount")
    }

    /// 同上但 mount_dir 取真实路径（供 mounted 物化 happy-path 测试）/ with a real mount_dir for materialization。
    pub(crate) fn skill_resource_mounted(
        uri: &str,
        source: Option<&str>,
        mount_dir: &str,
    ) -> Resource {
        use rmcp::model::{AnnotateAble, Meta};
        let mut raw = RawResource::new(uri, "skill");
        if let Some(src) = source {
            let mut m = serde_json::Map::new();
            m.insert("source".into(), Value::String(src.to_string()));
            m.insert("mount_dir".into(), Value::String(mount_dir.to_string()));
            raw.meta = Some(Meta(m));
        }
        raw.no_annotation()
    }

    /// 把假 client 注入 manager 的 `active_clients` / inject a fake client into the manager。
    pub(crate) async fn inject(manager: &MCPServerManager, name: &str, client: MockSkillClient) {
        manager
            .active_clients
            .write()
            .await
            .insert(name.to_string(), StdArc::new(client));
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
                env_file: None,
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
                env_file: None,
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
            env_file: None,
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
            env_file: None,
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
                env_file: None,
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
                env_file: None,
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

    #[tokio::test]
    async fn test_health_check_config() {
        let manager = MCPServerManager::new();

        // 验证默认配置 / Verify default config
        let config = manager.get_health_check_config().await;
        assert_eq!(config.interval_secs, 30);
        assert_eq!(config.timeout_secs, 5);
        assert!(config.enabled);

        // 更新配置 / Update config
        let new_config = HealthCheckConfig {
            interval_secs: 60,
            timeout_secs: 10,
            enabled: false,
        };
        manager.set_health_check_config(new_config.clone()).await;

        let updated = manager.get_health_check_config().await;
        assert_eq!(updated.interval_secs, 60);
        assert_eq!(updated.timeout_secs, 10);
        assert!(!updated.enabled);
    }

    #[tokio::test]
    async fn test_reconnect_policy() {
        let manager = MCPServerManager::new();

        // 验证默认策略 / Verify default policy
        let policy = manager.get_reconnect_policy().await;
        assert!(policy.enabled);
        assert_eq!(policy.max_retries, 5);
        assert_eq!(policy.initial_delay_ms, 1000);
        assert_eq!(policy.max_delay_ms, 30000);
        assert_eq!(policy.backoff_factor, 2.0);

        // 测试延迟计算 / Test delay calculation
        assert_eq!(policy.calculate_delay(0).as_millis(), 1000);
        assert_eq!(policy.calculate_delay(1).as_millis(), 2000);
        assert_eq!(policy.calculate_delay(2).as_millis(), 4000);
        assert_eq!(policy.calculate_delay(3).as_millis(), 8000);

        // 测试 should_retry / Test should_retry
        assert!(policy.should_retry(0));
        assert!(policy.should_retry(4));
        assert!(!policy.should_retry(5)); // max is 5

        // 测试无限重试 / Test infinite retry
        let infinite_policy = ReconnectPolicy {
            enabled: true,
            max_retries: 0,
            ..Default::default()
        };
        assert!(infinite_policy.should_retry(100));
    }

    #[tokio::test]
    async fn test_retry_counts() {
        let manager = MCPServerManager::new();

        // 初始应该为空 / Should be empty initially
        let counts = manager.get_retry_counts().await;
        assert!(counts.is_empty());

        // 通过内部操作添加重试计数 / Add retry counts through internal operation
        {
            manager
                .retry_counts
                .write()
                .await
                .insert("server1".to_string(), 3);
            manager
                .retry_counts
                .write()
                .await
                .insert("server2".to_string(), 5);
        }

        let counts = manager.get_retry_counts().await;
        assert_eq!(counts.get("server1"), Some(&3));
        assert_eq!(counts.get("server2"), Some(&5));

        // 重置单个服务器 / Reset single server
        manager.reset_retry_count("server1").await;
        let counts = manager.get_retry_counts().await;
        assert!(!counts.contains_key("server1"));
        assert_eq!(counts.get("server2"), Some(&5));

        // 重置所有 / Reset all
        manager.reset_all_retry_counts().await;
        let counts = manager.get_retry_counts().await;
        assert!(counts.is_empty());
    }

    #[tokio::test]
    async fn test_manager_with_custom_config() {
        let health_config = HealthCheckConfig {
            interval_secs: 15,
            timeout_secs: 3,
            enabled: true,
        };
        let reconnect_policy = ReconnectPolicy {
            enabled: true,
            max_retries: 10,
            initial_delay_ms: 500,
            max_delay_ms: 60000,
            backoff_factor: 1.5,
        };

        let manager =
            MCPServerManager::with_config(health_config.clone(), reconnect_policy.clone());

        let got_health = manager.get_health_check_config().await;
        assert_eq!(got_health.interval_secs, 15);
        assert_eq!(got_health.timeout_secs, 3);

        let got_reconnect = manager.get_reconnect_policy().await;
        assert_eq!(got_reconnect.max_retries, 10);
        assert_eq!(got_reconnect.initial_delay_ms, 500);
    }

    #[tokio::test]
    async fn test_merged_tool_meta() {
        let manager = MCPServerManager::new();

        // Case 1: specific only
        let config = MCPServerConfig::Stdio(StdioServerConfig {
            env_file: None,
            name: "s".to_string(),
            disabled: false,
            forbidden_tools: vec![],
            tool_meta: HashMap::from([(
                "tool_a".to_string(),
                ToolMeta {
                    auto_apply: Some(true),
                    alias: None,
                    tags: Some(vec!["tag1".to_string()]),
                    ret_object_mapper: None,
                },
            )]),
            default_tool_meta: None,
            vrl: None,
            server_parameters: StdioServerParameters {
                command: "echo".to_string(),
                args: vec![],
                env: HashMap::new(),
                cwd: None,
            },
        });
        let meta = manager.merged_tool_meta(&config, "tool_a").unwrap();
        assert_eq!(meta.auto_apply, Some(true));
        assert_eq!(meta.tags, Some(vec!["tag1".to_string()]));

        // Case 2: default only
        let config = MCPServerConfig::Stdio(StdioServerConfig {
            env_file: None,
            name: "s".to_string(),
            disabled: false,
            forbidden_tools: vec![],
            tool_meta: HashMap::new(),
            default_tool_meta: Some(ToolMeta {
                auto_apply: Some(false),
                alias: None,
                tags: Some(vec!["default_tag".to_string()]),
                ret_object_mapper: None,
            }),
            vrl: None,
            server_parameters: StdioServerParameters {
                command: "echo".to_string(),
                args: vec![],
                env: HashMap::new(),
                cwd: None,
            },
        });
        let meta = manager.merged_tool_meta(&config, "any_tool").unwrap();
        assert_eq!(meta.auto_apply, Some(false));
        assert_eq!(meta.tags, Some(vec!["default_tag".to_string()]));

        // Case 3: specific + default merge (specific wins)
        let config = MCPServerConfig::Stdio(StdioServerConfig {
            env_file: None,
            name: "s".to_string(),
            disabled: false,
            forbidden_tools: vec![],
            tool_meta: HashMap::from([(
                "tool_a".to_string(),
                ToolMeta {
                    auto_apply: Some(true),
                    alias: None,
                    tags: None,
                    ret_object_mapper: None,
                },
            )]),
            default_tool_meta: Some(ToolMeta {
                auto_apply: Some(false),
                alias: Some("default_alias".to_string()),
                tags: Some(vec!["default_tag".to_string()]),
                ret_object_mapper: None,
            }),
            vrl: None,
            server_parameters: StdioServerParameters {
                command: "echo".to_string(),
                args: vec![],
                env: HashMap::new(),
                cwd: None,
            },
        });
        let meta = manager.merged_tool_meta(&config, "tool_a").unwrap();
        assert_eq!(meta.auto_apply, Some(true)); // specific wins
        assert_eq!(meta.alias, Some("default_alias".to_string())); // from default
        assert_eq!(meta.tags, Some(vec!["default_tag".to_string()])); // from default

        // Case 4: no config
        let config = MCPServerConfig::Stdio(StdioServerConfig {
            env_file: None,
            name: "s".to_string(),
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
        assert!(manager.merged_tool_meta(&config, "tool_a").is_none());
    }

    #[tokio::test]
    async fn test_list_all_windows_empty_manager() {
        let manager = MCPServerManager::new();
        let windows = manager.list_all_windows(None).await;
        assert!(windows.is_empty());
    }

    #[tokio::test]
    async fn test_get_window_detail_server_not_connected() {
        use super::make_resource;
        let manager = MCPServerManager::new();
        let resource = make_resource(
            "window://test/status",
            "Test",
            None,
            Some("text/plain".into()),
        );
        let result = manager.get_window_detail("unknown_server", resource).await;
        assert!(result.is_err());
        match result {
            Err(ComputerError::InvalidState(msg)) => {
                assert!(msg.contains("not connected"));
            }
            other => panic!("Expected InvalidState, got {:?}", other),
        }
    }

    // ---- #74 INT-04：list_skill_resources + SkillResourceManager 接缝 ----
    // Mock / helpers 提升至 `super::test_support`（`pub(crate)`），供 computer.rs 集成测试复用。
    use super::test_support::{inject, skill_resource, MockSkillClient};

    #[tokio::test]
    async fn test_list_skill_resources_filters_and_exhausts_pages() {
        let manager = MCPServerManager::new();
        let pages = vec![
            vec![
                skill_resource("skill://srv/a", Some("mounted")),
                make_resource("window://w", "w", None, None),
            ],
            vec![skill_resource("skill://srv/b", Some("mounted"))],
        ];
        inject(
            &manager,
            "srv",
            MockSkillClient {
                pages,
                fail: false,
                cap_fail: false,
                read_text: "x".into(),
            },
        )
        .await;

        let got = manager.list_skill_resources(None).await;
        let uris: Vec<&str> = got.iter().map(|(_, r)| r.uri.as_str()).collect();
        // window:// 被过滤；两页都被消费 / window:// filtered out; both pages consumed.
        assert_eq!(uris, vec!["skill://srv/a", "skill://srv/b"]);
        assert!(got.iter().all(|(s, _)| s == "srv"));
    }

    #[tokio::test]
    async fn test_skill_resource_manager_trait_meta_and_read_bytes() {
        let manager = MCPServerManager::new();
        inject(
            &manager,
            "srv",
            MockSkillClient {
                pages: vec![vec![skill_resource("skill://srv/a", Some("mounted"))]],
                fail: false,
                cap_fail: false,
                read_text: "hello-bytes".into(),
            },
        )
        .await;

        // 经 SkillResourceManager trait：Resource → McpResource（提取 `_meta`）。
        let pairs = SkillResourceManager::list_skill_resources(&manager, None)
            .await
            .unwrap();
        assert_eq!(pairs.len(), 1);
        let (sname, mcp_res) = &pairs[0];
        assert_eq!(sname, "srv");
        assert_eq!(mcp_res.uri, "skill://srv/a");
        assert_eq!(
            mcp_res.meta.get("source").and_then(|v| v.as_str()),
            Some("mounted")
        );

        // read_resource → 字节（文本 content 拼接为 UTF-8 字节）。
        let bytes = SkillResourceManager::read_resource(&manager, "srv", "skill://srv/a")
            .await
            .unwrap();
        assert_eq!(bytes, b"hello-bytes");
    }

    #[tokio::test]
    async fn test_list_skill_resources_per_server_isolation_and_filter() {
        let manager = MCPServerManager::new();
        inject(
            &manager,
            "bad",
            MockSkillClient {
                pages: vec![],
                fail: true,
                cap_fail: false,
                read_text: String::new(),
            },
        )
        .await;
        inject(
            &manager,
            "good",
            MockSkillClient {
                pages: vec![vec![skill_resource("skill://good/a", Some("mounted"))]],
                fail: false,
                cap_fail: false,
                read_text: String::new(),
            },
        )
        .await;

        // 出错 server 跳过，good 的结果仍在 / erroring server skipped, good's result remains.
        let got = manager.list_skill_resources(None).await;
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "good");
        assert_eq!(got[0].1.uri, "skill://good/a");

        // server_name 过滤：只枚举指定 server / server_name filter narrows enumeration.
        let only_good = manager.list_skill_resources(Some("good")).await;
        assert_eq!(only_good.len(), 1);
        let none = manager.list_skill_resources(Some("missing")).await;
        assert!(none.is_empty());
    }

    // ---- #68：list_resources（get_resources 路由 + 4014/4015 映射）----

    #[tokio::test]
    async fn test_list_resources_unknown_server_4014() {
        let manager = MCPServerManager::new();
        let err = manager.list_resources("nope", None).await.unwrap_err();
        assert_eq!(err.error_code(), 4014);
        assert!(matches!(err, ComputerError::McpServerNotFound(s) if s == "nope"));
    }

    #[tokio::test]
    async fn test_list_resources_capability_not_supported_4015() {
        let manager = MCPServerManager::new();
        inject(
            &manager,
            "srv",
            MockSkillClient {
                pages: vec![],
                fail: false,
                cap_fail: true,
                read_text: String::new(),
            },
        )
        .await;
        let err = manager.list_resources("srv", None).await.unwrap_err();
        assert_eq!(err.error_code(), 4015);
        assert!(matches!(
            err,
            ComputerError::McpCapabilityNotSupported { server_name, capability }
            if server_name == "srv" && capability == "resources"
        ));
    }

    #[tokio::test]
    async fn test_list_resources_single_page_passthrough_cursor() {
        let manager = MCPServerManager::new();
        inject(
            &manager,
            "srv",
            MockSkillClient {
                pages: vec![
                    vec![make_resource("res://a", "a", None, None)],
                    vec![make_resource("res://b", "b", None, None)],
                ],
                fail: false,
                cap_fail: false,
                read_text: String::new(),
            },
        )
        .await;

        // 首页（cursor=None）：返回第 1 页 + next cursor（透传，不聚合第 2 页）。
        let (page1, next1) = manager.list_resources("srv", None).await.unwrap();
        assert_eq!(page1.len(), 1);
        assert_eq!(page1[0].uri, "res://a");
        assert_eq!(next1.as_deref(), Some("1"));

        // 第 2 页（透传 cursor）：返回第 2 页 + 末页（next=None）。
        let (page2, next2) = manager.list_resources("srv", next1).await.unwrap();
        assert_eq!(page2[0].uri, "res://b");
        assert!(next2.is_none());
    }
}
