/*!
* 文件名: commands.rs
* 作者: JQQ
* 创建日期: 2025/12/16
* 最后修改日期: 2025/12/16
* 版权: 2023 JQQ. All rights reserved.
* 依赖: console, serde_json
* 描述: CLI命令处理器 / CLI command handlers
*/

use crate::computer::{Computer, SilentSession};
use crate::errors::ComputerError;
use crate::mcp_clients::model::{MCPServerConfig, MCPServerInput};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum CommandError {
    #[error("Invalid command: {0}")]
    InvalidCommand(String),
    #[error("Parse error: {0}")]
    ParseError(String),
    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    JsonError(#[from] serde_json::Error),
    #[error("Computer error: {0}")]
    ComputerError(#[from] ComputerError),
}

pub struct CommandHandler {
    pub computer: Computer<SilentSession>,
}

impl CommandHandler {
    pub fn new(computer: Computer<SilentSession>) -> Self {
        Self { computer }
    }

    /// 显示帮助信息
    pub fn show_help(&self) {
        println!("可用命令 / Commands:");
        println!();
        println!("  status                    查看服务器状态 / show server status");
        println!("  tools                     列出可用工具 / list tools");
        println!("  mcp                       显示当前 MCP 配置 / show current MCP config");
        println!("  server add <json|@file>   添加或更新 MCP 配置 / add or update config");
        println!("  server rm <name>          移除 MCP 配置 / remove config");
        println!("  start <name>|all          启动客户端 / start client(s)");
        println!("  stop <name>|all           停止客户端 / stop client(s)");
        println!("  inputs load <@file>       从文件加载 inputs 定义 / load inputs");
        println!("  inputs add <json|@file>   添加 input 定义 / add input definition");
        println!("  inputs update <json|@file> 更新 input 定义 / update input definition");
        println!("  inputs rm <id>            移除 input 定义 / remove input definition");
        println!("  inputs get <id>           获取 input 定义 / get input definition");
        println!("  inputs list               查看当前inputs的定义 / show inputs");
        println!("  inputs value list         列出当前 inputs 的缓存值 / list current cached input values");
        println!("  inputs value get <id>     获取指定 id 的值 / get cached value by id");
        println!("  inputs value set <id>     设置指定 id 的值 / set cached value");
        println!("  inputs value rm <id>      删除指定 id 的值 / remove cached value");
        println!("  inputs value clear        清空全部缓存 / clear all cached values");
        println!("  tc <json|@file>           使用与 Socket.IO 一致的 JSON 结构调试工具");
        println!("  desktop [size] [uri]      获取当前桌面窗口组合 / get current desktop");
        println!("  history [n]               显示最近的工具调用历史 / show recent history");
        println!("  socket connect [url]      连接 Socket.IO / connect to Socket.IO");
        println!("  socket join <office> <name>  加入房间 / join office");
        println!("  socket leave              离开房间 / leave office");
        println!("  notify update             触发配置更新通知 / emit config updated");
        println!("  render <json|@file>       测试渲染（占位符解析）");
        println!("  quit | exit               退出 / quit");
    }

    /// 显示服务器状态
    pub async fn show_status(&self) -> Result<(), CommandError> {
        println!("服务器状态 / Server Status:");

        // 获取 MCP Manager 状态
        if self.computer.is_mcp_manager_initialized().await {
            // 获取服务器状态列表
            let server_status = self.computer.get_server_status().await;
            let active_count = server_status
                .iter()
                .filter(|(_, active, _)| *active)
                .count();

            println!("  MCP Manager: 已初始化 / Initialized");
            println!("  Active Servers: {}", active_count);

            // 显示每个服务器的状态
            for (name, active, state) in server_status {
                let status = if active {
                    "运行中 / Running"
                } else {
                    "已停止 / Stopped"
                };
                println!("    - {}: {} ({})", name, status, state);
            }

            // 获取可用工具数量
            match self.computer.get_available_tools().await {
                Ok(tools) => println!("  Available Tools: {}", tools.len()),
                Err(_) => println!("  Available Tools: 获取失败 / Failed to get"),
            }
        } else {
            println!("  MCP Manager: 未初始化 / Not initialized");
            println!("  Active Servers: 0");
            println!("  Available Tools: 0");
        }

        Ok(())
    }

    /// 列出可用工具
    pub async fn list_tools(&self) -> Result<(), CommandError> {
        match self.computer.get_available_tools().await {
            Ok(tools) => {
                println!("可用工具 / Available Tools:");
                for tool in tools {
                    println!("  - {}", tool.name);
                }
            }
            Err(e) => {
                return Err(CommandError::ComputerError(ComputerError::TransportError(
                    e.to_string(),
                )));
            }
        }
        Ok(())
    }

    /// 显示 MCP 配置
    pub async fn show_mcp_config(&self) -> Result<(), CommandError> {
        // 获取服务器配置
        let servers = self.computer.list_mcp_servers().await;

        // 获取 inputs
        let inputs = self.computer.list_inputs().await?;

        let config = json!({
            "servers": servers,
            "inputs": inputs
        });

        println!("当前 MCP 配置 / Current MCP Config:");
        println!("{}", serde_json::to_string_pretty(&config)?);

        Ok(())
    }

    /// 添加或更新服务器配置
    pub async fn add_server(&mut self, config_str: &str) -> Result<(), CommandError> {
        let config: Value = if let Some(path) = config_str.strip_prefix('@') {
            let content = std::fs::read_to_string(path)?;
            serde_json::from_str(&content)?
        } else {
            serde_json::from_str(config_str)?
        };

        // 将 JSON 转换为 MCPServerConfig
        let server_config: MCPServerConfig = serde_json::from_value(config)?;

        // 添加或更新服务器配置
        self.computer.add_or_update_server(server_config).await?;

        println!("✅ 服务器配置已添加/更新 / Server config added/updated");

        Ok(())
    }

    #[cfg(test)]
    pub async fn add_server_debug(&mut self, config_str: &str) -> Result<(), CommandError> {
        let config: Value = if let Some(path) = config_str.strip_prefix('@') {
            let content = std::fs::read_to_string(path)?;
            serde_json::from_str(&content)?
        } else {
            serde_json::from_str(config_str)?
        };

        // 将 JSON 转换为 MCPServerConfig
        match serde_json::from_value::<MCPServerConfig>(config.clone()) {
            Ok(server_config) => {
                println!("JSON parsed successfully: {:?}", server_config);
                self.computer.add_or_update_server(server_config).await?;
                println!("✅ 服务器配置已添加/更新 / Server config added/updated");
            }
            Err(e) => {
                println!("JSON parse error: {:?}", e);
                println!("JSON was: {}", serde_json::to_string_pretty(&config)?);
                return Err(CommandError::JsonError(e));
            }
        }

        Ok(())
    }

    /// 移除服务器配置
    pub async fn remove_server(&mut self, name: &str) -> Result<(), CommandError> {
        // 移除服务器
        self.computer.remove_server(name).await?;
        println!("已移除服务器配置 '{}' / Removed server config", name);
        Ok(())
    }

    /// 启动客户端
    pub async fn start_client(&self, target: &str) -> Result<(), CommandError> {
        match self.computer.start_mcp_client(target).await {
            Ok(()) => {
                if target == "all" {
                    println!("✅ 所有服务器启动完成 / All servers started");
                } else {
                    println!(
                        "✅ 服务器 '{}' 启动完成 / Server '{}' started",
                        target, target
                    );
                }
            }
            Err(e) => {
                println!("❌ 启动服务器失败: {} / Failed to start server: {}", e, e);
            }
        }
        Ok(())
    }

    /// 停止客户端
    pub async fn stop_client(&self, target: &str) -> Result<(), CommandError> {
        match self.computer.stop_mcp_client(target).await {
            Ok(()) => {
                if target == "all" {
                    println!("✅ 所有服务器停止完成 / All servers stopped");
                } else {
                    println!(
                        "✅ 服务器 '{}' 停止完成 / Server '{}' stopped",
                        target, target
                    );
                }
            }
            Err(e) => {
                println!("❌ 停止服务器失败: {} / Failed to stop server: {}", e, e);
            }
        }
        Ok(())
    }

    /// 加载 inputs 配置
    pub async fn load_inputs(&mut self, path: &Path) -> Result<(), CommandError> {
        let content = std::fs::read_to_string(path)?;
        let inputs_value: Value = serde_json::from_str(&content)?;

        // 将 JSON 转换为 Vec<MCPServerInput>
        let inputs_array: Vec<Value> = serde_json::from_value(inputs_value)?;
        let mut inputs_map = HashMap::new();

        for input_value in inputs_array {
            let input: MCPServerInput = serde_json::from_value(input_value)?;
            inputs_map.insert(input.id().to_string(), input);
        }

        // 更新 inputs
        self.computer.update_inputs(inputs_map).await?;

        println!("✅ 已加载 Inputs 配置 / Inputs loaded");

        Ok(())
    }

    /// 列出 inputs 定义
    pub async fn list_inputs(&self) -> Result<(), CommandError> {
        let inputs = self.computer.list_inputs().await?;

        println!("当前 Inputs 定义 / Current Inputs:");
        for input in inputs {
            println!("  - {}", input.id());
        }

        Ok(())
    }

    /// 连接 SocketIO
    pub async fn connect_socketio(
        &mut self,
        url: &str,
        namespace: &str,
        auth: &Option<String>,
        headers: &Option<String>,
    ) -> Result<(), CommandError> {
        self.computer
            .connect_socketio(url, namespace, auth, headers)
            .await?;
        println!("✅ 已连接到 Socket.IO: {} / Connected to Socket.IO", url);
        Ok(())
    }

    /// 断开 SocketIO 连接 / Disconnect SocketIO
    pub async fn disconnect_socketio(&mut self) -> Result<(), CommandError> {
        self.computer.disconnect_socketio().await?;
        println!("✅ 已断开 Socket.IO 连接 / Disconnected from Socket.IO");
        Ok(())
    }

    /// 加载服务器配置
    pub async fn load_config(&mut self, path: &Path) -> Result<(), CommandError> {
        let content = std::fs::read_to_string(path)?;
        let config: Value = serde_json::from_str(&content)?;

        // 解析服务器配置数组
        if let Some(servers_array) = config.get("servers").and_then(|v| v.as_array()) {
            for server_value in servers_array {
                let server_config: MCPServerConfig = serde_json::from_value(server_value.clone())?;
                self.computer.add_or_update_server(server_config).await?;
            }
        }

        // 解析 inputs 配置
        if let Some(inputs_array) = config.get("inputs").and_then(|v| v.as_array()) {
            let mut inputs_map = HashMap::new();
            for input_value in inputs_array {
                let input: MCPServerInput = serde_json::from_value(input_value.clone())?;
                inputs_map.insert(input.id().to_string(), input);
            }
            self.computer.update_inputs(inputs_map).await?;
        }

        println!("✅ 已加载 Servers 配置 / Servers loaded");

        Ok(())
    }

    /// 获取桌面信息
    pub async fn get_desktop(
        &self,
        size: Option<u32>,
        uri: Option<&str>,
    ) -> Result<(), CommandError> {
        // TODO: 实现获取桌面信息 - 需要等待 desktop 模块实现
        let desktop = json!({
            "windows": [],
            "size": size,
            "uri": uri
        });

        println!("{}", serde_json::to_string_pretty(&desktop)?);
        Ok(())
    }

    /// 显示历史记录
    pub async fn show_history(&self, n: Option<usize>) -> Result<(), CommandError> {
        let history = self.computer.get_tool_history().await?;

        println!("最近工具调用历史 / Recent Tool Call History:");

        if history.is_empty() {
            println!("  (暂无记录 / No records yet)");
        } else {
            let limit = n.unwrap_or(10).min(history.len());
            let start_idx = history.len().saturating_sub(limit);

            for (i, record) in history.iter().skip(start_idx).enumerate() {
                println!(
                    "  {}. [{}] {}::{} - {}{}",
                    i + 1,
                    record.timestamp.format("%Y-%m-%d %H:%M:%S UTC"),
                    record.server,
                    record.tool,
                    if record.success {
                        "成功 / Success"
                    } else {
                        "失败 / Failed"
                    },
                    if let Some(ref error) = record.error {
                        format!(" - {}", error)
                    } else {
                        String::new()
                    }
                );
            }
        }

        Ok(())
    }

    /// 获取输入定义 / Get input definition
    pub async fn get_input_definition(
        &self,
        id: &str,
    ) -> Result<Option<MCPServerInput>, CommandError> {
        Ok(self.computer.get_input(id).await?)
    }

    /// 列出所有输入值 / List all input values
    pub async fn list_input_values(
        &self,
    ) -> Result<HashMap<String, serde_json::Value>, CommandError> {
        Ok(self.computer.list_input_values().await?)
    }

    /// 获取输入值 / Get input value
    pub async fn get_input_value(
        &self,
        id: &str,
    ) -> Result<Option<serde_json::Value>, CommandError> {
        Ok(self.computer.get_input_value(id).await?)
    }

    /// 设置输入值 / Set input value
    pub async fn set_input_value(
        &self,
        id: &str,
        value: &serde_json::Value,
    ) -> Result<bool, CommandError> {
        Ok(self.computer.set_input_value(id, value.clone()).await?)
    }

    /// 删除输入值 / Remove input value
    pub async fn remove_input_value(&self, id: &str) -> Result<bool, CommandError> {
        Ok(self.computer.remove_input_value(id).await?)
    }

    /// 测试渲染（占位符解析）
    pub async fn render_config(&self, config_str: &str) -> Result<(), CommandError> {
        use crate::mcp_clients::render::ConfigRender;

        // 解析配置
        let config: Value = if let Some(path) = config_str.strip_prefix('@') {
            let content = std::fs::read_to_string(path)?;
            serde_json::from_str(&content)?
        } else {
            serde_json::from_str(config_str)?
        };

        // 创建渲染器
        let render = ConfigRender::default();

        // 创建解析器函数
        let resolver = |id: String| async move {
            match self.computer.get_input_value(&id).await {
                Ok(Some(value)) => Ok(value),
                Ok(None) => Err(crate::mcp_clients::render::RenderError::InputNotFound(id)),
                Err(_e) => Err(crate::mcp_clients::render::RenderError::InputNotFound(id)),
            }
        };

        // 执行渲染
        match render.render(config, resolver).await {
            Ok(rendered) => {
                println!("渲染结果 / Rendered result:");
                println!("{}", serde_json::to_string_pretty(&rendered)?);
            }
            Err(e) => {
                eprintln!("渲染失败 / Render failed: {}", e);
            }
        }

        Ok(())
    }

    /// 工具调用调试 / Tool call debug
    pub async fn debug_tool_call(&self, tool_call_str: &str) -> Result<(), CommandError> {
        // 解析工具调用请求
        let tool_call: Value = if let Some(path) = tool_call_str.strip_prefix('@') {
            let content = std::fs::read_to_string(path)?;
            serde_json::from_str(&content)?
        } else {
            serde_json::from_str(tool_call_str)?
        };

        // 提取必需字段
        let req_id = tool_call
            .get("req_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                CommandError::InvalidCommand("缺少 req_id 字段 / Missing req_id field".to_string())
            })?;

        let tool_name = tool_call
            .get("tool_name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                CommandError::InvalidCommand(
                    "缺少 tool_name 字段 / Missing tool_name field".to_string(),
                )
            })?;

        let parameters = tool_call
            .get("params")
            .unwrap_or(&Value::Object(serde_json::Map::new()))
            .clone();

        let timeout = tool_call.get("timeout").and_then(|v| v.as_f64());

        // 检查 MCP Manager 是否已初始化
        if !self.computer.is_mcp_manager_initialized().await {
            println!("警告 / Warning: MCP 管理器未初始化。请先添加并启动服务器 (server add/start) / MCP manager not initialized. Add and start a server first.");
            return Ok(());
        }

        // 执行工具调用
        match self
            .computer
            .execute_tool(req_id, tool_name, parameters, timeout)
            .await
        {
            Ok(result) => {
                println!("工具调用成功 / Tool call succeeded:");
                println!("{}", serde_json::to_string_pretty(&result)?);
            }
            Err(e) => {
                eprintln!("工具调用失败 / Tool call failed: {}", e);
            }
        }

        Ok(())
    }

    /// 加入 Socket.IO 房间 / Join Socket.IO room
    pub async fn join_socket_room(
        &self,
        office_id: &str,
        computer_name: &str,
    ) -> Result<(), CommandError> {
        self.computer.join_office(office_id, computer_name).await?;
        println!("✅ 已加入房间 / Joined office: {}", office_id);
        Ok(())
    }

    /// 离开 Socket.IO 房间 / Leave Socket.IO room
    pub async fn leave_socket_room(&self) -> Result<(), CommandError> {
        self.computer.leave_office().await?;
        println!("✅ 已离开房间 / Left office");
        Ok(())
    }

    /// 发送配置更新通知 / Send config update notification
    pub async fn notify_config_update(&self) -> Result<(), CommandError> {
        self.computer.emit_update_config().await?;
        println!("✅ 配置更新通知已发送 / Config update notification sent");
        Ok(())
    }

    /// 添加或更新输入 / Add or update input
    pub async fn add_input(&mut self, input_str: &str) -> Result<(), CommandError> {
        // 解析输入
        let input_value: Value = if let Some(path) = input_str.strip_prefix('@') {
            let content = std::fs::read_to_string(path)?;
            serde_json::from_str(&content)?
        } else {
            serde_json::from_str(input_str)?
        };

        // 支持单个或数组
        if let Some(array) = input_value.as_array() {
            for item in array {
                let input: MCPServerInput = serde_json::from_value(item.clone())?;
                self.computer.add_or_update_input(input).await?;
            }
        } else {
            let input: MCPServerInput = serde_json::from_value(input_value)?;
            self.computer.add_or_update_input(input).await?;
        }

        println!("Input(s) 已添加/更新 / Added/Updated");
        Ok(())
    }

    /// 更新输入 / Update input
    pub async fn update_input(&mut self, input_str: &str) -> Result<(), CommandError> {
        // 解析输入
        let input_value: Value = if let Some(path) = input_str.strip_prefix('@') {
            let content = std::fs::read_to_string(path)?;
            serde_json::from_str(&content)?
        } else {
            serde_json::from_str(input_str)?
        };

        // 支持单个或数组
        if let Some(array) = input_value.as_array() {
            for item in array {
                let input: MCPServerInput = serde_json::from_value(item.clone())?;
                self.computer.add_or_update_input(input).await?;
            }
        } else {
            let input: MCPServerInput = serde_json::from_value(input_value)?;
            self.computer.add_or_update_input(input).await?;
        }

        println!("Input(s) 已添加/更新 / Added/Updated");
        Ok(())
    }

    /// 移除输入定义 / Remove input definition
    pub async fn remove_input_def(&mut self, id: &str) -> Result<bool, CommandError> {
        let removed = self.computer.remove_input(id).await?;
        if removed {
            println!("已移除 / Removed");
        } else {
            println!("不存在的 id / Not found");
        }
        Ok(removed)
    }

    /// 获取输入定义 / Get input definition
    pub async fn get_input_def(&self, id: &str) -> Result<(), CommandError> {
        match self.computer.get_input(id).await? {
            Some(input) => {
                println!("Input '{}':", id);
                println!("{}", serde_json::to_string_pretty(&input)?);
            }
            None => {
                println!("不存在的 id / Not found: {}", id);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computer::SilentSession;
    use std::io::Write;
    use tempfile::NamedTempFile;

    /// 创建测试用的 Computer 实例 / Create test Computer instance
    async fn create_test_computer() -> Computer<SilentSession> {
        Computer::new(
            "test_computer",
            SilentSession::new("test_session"),
            None,
            None,
            false,
            false,
        )
    }

    #[tokio::test]
    async fn test_show_help() {
        let computer = create_test_computer().await;
        let handler = CommandHandler::new(computer);

        // 测试帮助信息不会崩溃 / Test help doesn't crash
        handler.show_help();
    }

    #[tokio::test]
    async fn test_show_status_uninitialized() {
        let computer = create_test_computer().await;
        let handler = CommandHandler::new(computer);

        // 测试未初始化状态 / Test uninitialized state
        let result = handler.show_status().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_add_server_with_json() {
        let computer = create_test_computer().await;
        let mut handler = CommandHandler::new(computer);

        // 测试添加服务器配置 / Test adding server config
        let json_config = r#"
{
    "type": "Stdio",
    "name": "test_stdio",
    "disabled": false,
    "forbidden_tools": [],
    "tool_meta": {},
    "default_tool_meta": null,
    "vrl": null,
    "server_parameters": {
        "command": "echo",
        "args": ["hello"],
        "env": {},
        "cwd": null
    }
}
"#;

        let result = handler.add_server_debug(json_config).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_add_server_invalid_json() {
        let computer = create_test_computer().await;
        let mut handler = CommandHandler::new(computer);

        // 测试无效 JSON / Test invalid JSON
        let invalid_json = "{ invalid json }";

        let result = handler.add_server(invalid_json).await;
        assert!(result.is_err());
        matches!(result.unwrap_err(), CommandError::JsonError(_));
    }

    #[tokio::test]
    async fn test_add_server_from_file() -> Result<(), std::io::Error> {
        let computer = create_test_computer().await;
        let mut handler = CommandHandler::new(computer);

        // 创建临时配置文件 / Create temp config file
        let mut temp_file = NamedTempFile::new()?;
        writeln!(
            temp_file,
            r#"
{{
    "type": "Stdio",
    "name": "test_from_file",
    "disabled": false,
    "forbidden_tools": [],
    "tool_meta": {{}},
    "default_tool_meta": null,
    "vrl": null,
    "server_parameters": {{
        "command": "echo",
        "args": ["hello"],
        "env": {{}},
        "cwd": null
    }}
}}
        "#
        )?;

        let config_path = format!("@{}", temp_file.path().display());
        let result = handler.add_server(&config_path).await;
        assert!(result.is_ok());

        Ok(())
    }

    #[tokio::test]
    async fn test_remove_server() {
        let computer = create_test_computer().await;
        let mut handler = CommandHandler::new(computer);

        // 测试移除服务器（即使不存在也应该成功） / Test removing server
        let result = handler.remove_server("non_existent").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_start_stop_client_uninitialized() {
        let computer = create_test_computer().await;
        let handler = CommandHandler::new(computer);

        // 测试未初始化时启动客户端 / Test starting client when uninitialized
        let result = handler.start_client("test").await;
        assert!(result.is_ok());

        // 测试停止所有客户端 / Test stopping all clients
        let result = handler.stop_client("all").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_load_inputs() -> Result<(), std::io::Error> {
        let computer = create_test_computer().await;
        let mut handler = CommandHandler::new(computer);

        // 创建临时 inputs 文件 / Create temp inputs file
        let mut temp_file = NamedTempFile::new()?;
        writeln!(
            temp_file,
            r#"
[
    {{
        "type": "PromptString",
        "id": "test_input",
        "description": "Test input",
        "default": "default_value",
        "password": false
    }}
]
        "#
        )?;

        let result = handler.load_inputs(temp_file.path()).await;
        assert!(result.is_ok());

        Ok(())
    }

    #[tokio::test]
    async fn test_list_inputs_empty() {
        let computer = create_test_computer().await;
        let handler = CommandHandler::new(computer);

        // 测试列出空的 inputs / Test listing empty inputs
        let result = handler.list_inputs().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_show_history_empty() {
        let computer = create_test_computer().await;
        let handler = CommandHandler::new(computer);

        // 测试显示空历史 / Test showing empty history
        let result = handler.show_history(Some(5)).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_get_desktop() {
        let computer = create_test_computer().await;
        let handler = CommandHandler::new(computer);

        // 测试获取桌面信息 / Test getting desktop info
        let result = handler.get_desktop(Some(10), Some("test://uri")).await;
        assert!(result.is_ok());
    }

    
    #[tokio::test]
    async fn test_load_config() -> Result<(), std::io::Error> {
        let computer = create_test_computer().await;
        let mut handler = CommandHandler::new(computer);

        // 创建完整配置文件 / Create complete config file
        let mut temp_file = NamedTempFile::new()?;
        writeln!(
            temp_file,
            r#"
{{
    "servers": [
        {{
            "type": "Stdio",
            "name": "test_server",
            "disabled": false,
            "forbidden_tools": [],
            "tool_meta": {{}},
            "default_tool_meta": null,
            "vrl": null,
            "server_parameters": {{
                "command": "echo",
                "args": ["test"],
                "env": {{}},
                "cwd": null
            }}
        }}
    ],
    "inputs": [
        {{
            "type": "PromptString",
            "id": "test_input",
            "description": "Test input",
            "default": "default",
            "password": false
        }}
    ]
}}
        "#
        )?;

        let result = handler.load_config(temp_file.path()).await;
        assert!(result.is_ok());

        Ok(())
    }

    // 表驱动测试示例 / Table-driven test example
    #[tokio::test]
    async fn test_add_server_validation() {
        let computer = create_test_computer().await;
        let mut handler = CommandHandler::new(computer);

        let test_cases = vec![
            // (json, should_succeed, description)
            (
                r#"{"type": "Stdio", "name": "test"}"#,
                false,
                "Missing required fields",
            ),
            (
                r#"{"type": "Invalid", "name": "test"}"#,
                false,
                "Invalid server type",
            ),
            (r#""not a json""#, false, "Not a JSON object"),
            (
                r#"{"type": "Stdio", "name": "", "server_parameters": {}}"#,
                false,
                "Empty name",
            ),
        ];

        for (json, should_succeed, description) in test_cases {
            let result = handler.add_server(json).await;
            if should_succeed {
                assert!(result.is_ok(), "Should succeed: {}", description);
            } else {
                assert!(result.is_err(), "Should fail: {}", description);
            }
        }
    }
}
