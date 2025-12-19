/**
* 文件名: stdio_client
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: tokio, serde_json
* 描述: STDIO类型的MCP客户端实现
*/
use super::base_client::BaseMCPClient;
use super::model::*;
use crate::desktop::window_uri::{WindowURI, is_window_uri};
use async_trait::async_trait;
use serde_json;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

/// STDIO MCP客户端 / STDIO MCP client
pub struct StdioMCPClient {
    /// 基础客户端 / Base client
    base: BaseMCPClient<StdioServerParameters>,
    /// 子进程 / Child process
    child_process: Arc<Mutex<Option<Child>>>,
    /// 会话ID / Session ID
    session_id: Arc<Mutex<Option<String>>>,
}

impl std::fmt::Debug for StdioMCPClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StdioMCPClient")
            .field("command", &self.base.params.command)
            .field("args", &self.base.params.args)
            .field("state", &self.base.state())
            .finish()
    }
}

impl StdioMCPClient {
    /// 创建新的STDIO客户端 / Create new STDIO client
    pub fn new(params: StdioServerParameters) -> Self {
        Self {
            base: BaseMCPClient::new(params),
            child_process: Arc::new(Mutex::new(None)),
            session_id: Arc::new(Mutex::new(None)),
        }
    }

    /// 启动子进程 / Start child process
    async fn start_child_process(
        &self,
        params: &StdioServerParameters,
    ) -> Result<Child, MCPClientError> {
        let mut cmd = Command::new(&params.command);

        // 设置参数 / Set arguments
        cmd.args(&params.args);

        // 设置环境变量 / Set environment variables
        for (key, value) in &params.env {
            cmd.env(key, value);
        }

        // 设置工作目录 / Set working directory
        if let Some(cwd) = &params.cwd {
            cmd.current_dir(cwd);
        }

        // 配置stdio / Configure stdio
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        debug!("Starting command: {} {:?}", params.command, params.args);

        let child = cmd.spawn().map_err(|e| {
            MCPClientError::ConnectionError(format!("Failed to start process: {}", e))
        })?;

        Ok(child)
    }

    /// 发送JSON-RPC请求 / Send JSON-RPC request
    /// 发送通知（不需要响应） / Send notification (no response expected)
    async fn send_notification(
        &self,
        notification: &serde_json::Value,
    ) -> Result<(), MCPClientError> {
        let mut child = self.child_process.lock().await;
        if let Some(ref mut process) = *child {
            if let Some(stdin) = process.stdin.as_mut() {
                let notification_str = serde_json::to_string(notification)?;
                use tokio::io::AsyncWriteExt;
                stdin.write_all(notification_str.as_bytes()).await?;
                stdin.write_all(b"\n").await?;
                stdin.flush().await?;

                debug!("Sent notification: {}", notification_str);
                info!("Sent notification to MCP server: {}", notification_str);
                return Ok(());
            }
        }
        Err(MCPClientError::ConnectionError(
            "Process not available".to_string(),
        ))
    }

    async fn send_request(
        &self,
        request: &serde_json::Value,
    ) -> Result<serde_json::Value, MCPClientError> {
        let mut child = self.child_process.lock().await;
        if let Some(ref mut process) = *child {
            if let Some(stdin) = process.stdin.as_mut() {
                let request_str = serde_json::to_string(request)?;
                use tokio::io::AsyncWriteExt;
                stdin.write_all(request_str.as_bytes()).await?;
                stdin.write_all(b"\n").await?;
                stdin.flush().await?;

                debug!("Sent request: {}", request_str);
                info!("Sent request to MCP server: {}", request_str);

                // 读取响应 / Read response
                if let Some(stdout) = process.stdout.as_mut() {
                    let mut reader = BufReader::new(stdout);
                    let mut line = String::new();

                    info!("Waiting for response from MCP server...");

                    // 添加超时以防止无限阻塞
                    return match tokio::time::timeout(
                        std::time::Duration::from_secs(30),
                        reader.read_line(&mut line),
                    )
                    .await
                    {
                        Ok(Ok(0)) => {
                            error!("Process closed stdout without response");
                            Err(MCPClientError::ConnectionError(
                                "Process closed stdout".to_string(),
                            ))
                        }
                        Ok(Ok(_)) => {
                            info!("Received raw response: {}", line.trim());
                            debug!("Received response: {}", line.trim());
                            let response: serde_json::Value = serde_json::from_str(line.trim())
                                .map_err(|e| {
                                    error!("Failed to parse JSON response: {}", e);
                                    MCPClientError::ProtocolError(format!("Invalid JSON: {}", e))
                                })?;
                            info!("Parsed JSON response: {}", response);
                            Ok(response)
                        }
                        Ok(Err(e)) => Err(MCPClientError::ConnectionError(format!(
                            "Failed to read response: {}",
                            e
                        ))),
                        Err(_) => Err(MCPClientError::TimeoutError(
                            "No response received within timeout".to_string(),
                        )),
                    };
                }
            }
        }

        Err(MCPClientError::ConnectionError(
            "Process not running".to_string(),
        ))
    }

    /// 初始化会话 / Initialize session
    async fn initialize_session(&self) -> Result<(), MCPClientError> {
        let init_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "tools": {},
                    "resources": {}
                },
                "clientInfo": {
                    "name": "a2c-smcp-rust",
                    "version": "0.1.0"
                }
            }
        });

        let response = self.send_request(&init_request).await?;

        // 检查响应 / Check response
        if let Some(error) = response.get("error") {
            return Err(MCPClientError::ProtocolError(format!(
                "Initialize error: {}",
                error
            )));
        }

        if let Some(result) = response.get("result") {
            if let Some(session_id) = result.get("sessionId").and_then(|v| v.as_str()) {
                *self.session_id.lock().await = Some(session_id.to_string());
            }
        }

        // 发送initialized通知 / Send initialized notification
        let initialized_notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        });

        // 通知不需要响应 / Notifications don't need response
        self.send_notification(&initialized_notification).await?;

        info!("Session initialized successfully");
        Ok(())
    }
}

#[async_trait]
impl MCPClientProtocol for StdioMCPClient {
    fn state(&self) -> ClientState {
        self.base.state()
    }

    async fn connect(&self) -> Result<(), MCPClientError> {
        // 检查是否可以连接 / Check if can connect
        if !self.base.can_connect().await {
            return Err(MCPClientError::ConnectionError(format!(
                "Cannot connect in state: {}",
                self.base.get_state().await
            )));
        }

        // 获取参数 / Get parameters
        let params = self.base.params.clone();

        // 启动子进程 / Start child process
        let child = self.start_child_process(&params).await?;
        *self.child_process.lock().await = Some(child);

        // 初始化会话 / Initialize session
        self.initialize_session().await?;

        // 更新状态 / Update state
        self.base.update_state(ClientState::Connected).await;
        info!("STDIO client connected successfully");

        Ok(())
    }

    async fn disconnect(&self) -> Result<(), MCPClientError> {
        // 检查是否可以断开 / Check if can disconnect
        if !self.base.can_disconnect().await {
            return Err(MCPClientError::ConnectionError(format!(
                "Cannot disconnect in state: {}",
                self.base.get_state().await
            )));
        }

        // 停止子进程 / Stop child process
        let mut child = self.child_process.lock().await;
        if let Some(mut process) = child.take() {
            // 尝试优雅关闭 / Try graceful shutdown
            let shutdown_request = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "shutdown"
            });

            // 直接写入而不调用 send_request 以避免死锁
            if let Some(stdin) = process.stdin.as_mut() {
                let request_str = serde_json::to_string(&shutdown_request)?;
                use tokio::io::AsyncWriteExt;
                if let Err(e) = stdin.write_all(request_str.as_bytes()).await {
                    warn!("Failed to send shutdown request: {}", e);
                } else {
                    let _ = stdin.write_all(b"\n").await;
                    let _ = stdin.flush().await;
                }
            }

            // 发送exit通知 / Send exit notification
            let exit_notification = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "exit"
            });

            if let Some(stdin) = process.stdin.as_mut() {
                let request_str = serde_json::to_string(&exit_notification)?;
                use tokio::io::AsyncWriteExt;
                if let Err(e) = stdin.write_all(request_str.as_bytes()).await {
                    warn!("Failed to send exit notification: {}", e);
                } else {
                    let _ = stdin.write_all(b"\n").await;
                    let _ = stdin.flush().await;
                }
            }

            // 释放锁，然后等待进程退出
            drop(child);

            // 等待进程退出或强制杀死 / Wait for process exit or force kill
            match tokio::time::timeout(std::time::Duration::from_secs(5), process.wait()).await {
                Ok(Ok(status)) => {
                    debug!("Process exited with status: {}", status);
                }
                Ok(Err(e)) => {
                    error!("Error waiting for process: {}", e);
                }
                Err(_) => {
                    warn!("Process did not exit within timeout, killing it");
                    if let Err(e) = process.kill().await {
                        error!("Failed to kill process: {}", e);
                    }
                }
            }
        } else {
            // 没有进程时也要释放锁
            drop(child);
        }

        // 清理会话ID / Clear session ID
        *self.session_id.lock().await = None;

        // 更新状态 / Update state
        self.base.update_state(ClientState::Disconnected).await;
        info!("STDIO client disconnected successfully");

        Ok(())
    }

    async fn list_tools(&self) -> Result<Vec<Tool>, MCPClientError> {
        if self.base.get_state().await != ClientState::Connected {
            return Err(MCPClientError::ConnectionError("Not connected".to_string()));
        }

        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/list"
        });

        let response = self.send_request(&request).await?;
        info!("Received list_tools response: {}", response);

        if let Some(error) = response.get("error") {
            return Err(MCPClientError::ProtocolError(format!(
                "List tools error: {}",
                error
            )));
        }

        if let Some(result) = response.get("result") {
            info!("Result field: {}", result);
            if let Some(tools) = result.get("tools").and_then(|v| v.as_array()) {
                info!("Found {} tools", tools.len());
                let mut tool_list = Vec::new();
                for (i, tool) in tools.iter().enumerate() {
                    info!("Tool {}: {}", i, tool);
                    if let Ok(parsed_tool) = serde_json::from_value::<Tool>(tool.clone()) {
                        tool_list.push(parsed_tool);
                    } else {
                        warn!("Failed to parse tool {}: {}", i, tool);
                    }
                }
                return Ok(tool_list);
            } else {
                warn!("No tools array found in result");
            }
        } else {
            warn!("No result field found in response");
        }

        Ok(vec![])
    }

    async fn call_tool(
        &self,
        tool_name: &str,
        params: serde_json::Value,
    ) -> Result<CallToolResult, MCPClientError> {
        if self.base.get_state().await != ClientState::Connected {
            return Err(MCPClientError::ConnectionError("Not connected".to_string()));
        }

        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": tool_name,
                "arguments": params
            }
        });

        let response = self.send_request(&request).await?;

        if let Some(error) = response.get("error") {
            return Err(MCPClientError::ProtocolError(format!(
                "Call tool error: {}",
                error
            )));
        }

        if let Some(result) = response.get("result") {
            let call_result: CallToolResult = serde_json::from_value(result.clone())?;
            return Ok(call_result);
        }

        Err(MCPClientError::ProtocolError(
            "Invalid response".to_string(),
        ))
    }

    async fn list_windows(&self) -> Result<Vec<Resource>, MCPClientError> {
        if self.base.get_state().await != ClientState::Connected {
            return Err(MCPClientError::ConnectionError("Not connected".to_string()));
        }

        // 支持分页获取资源 / Support pagination for resources
        let mut all_resources = Vec::new();
        let mut cursor: Option<String> = None;

        loop {
            let mut request = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 5,
                "method": "resources/list"
            });

            // 添加分页参数 / Add pagination parameter
            if let Some(ref c) = cursor {
                request["params"] = serde_json::json!({ "cursor": c });
            }

            let response = self.send_request(&request).await?;

            if let Some(error) = response.get("error") {
                return Err(MCPClientError::ProtocolError(format!(
                    "List resources error: {}",
                    error
                )));
            }

            if let Some(result) = response.get("result") {
                // 解析资源列表 / Parse resource list
                if let Some(resources) = result.get("resources").and_then(|v| v.as_array()) {
                    for resource in resources {
                        if let Ok(parsed_resource) = serde_json::from_value::<Resource>(resource.clone()) {
                            all_resources.push(parsed_resource);
                        }
                    }
                }

                // 检查是否有下一页 / Check if there's a next page
                cursor = result.get("nextCursor")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());

                if cursor.is_none() {
                    break;
                }
            } else {
                break;
            }
        }

        // 过滤 window:// 资源并按 priority 排序 / Filter window:// resources and sort by priority
        let mut filtered_resources: Vec<(Resource, i32)> = Vec::new();
        
        for resource in all_resources {
            if !is_window_uri(&resource.uri) {
                continue;
            }

            // 解析 priority / Parse priority
            let priority = if let Ok(uri) = WindowURI::new(&resource.uri) {
                uri.priority().unwrap_or(0)
            } else {
                0
            };

            filtered_resources.push((resource, priority));
        }

        // 按 priority 降序排序 / Sort by priority in descending order
        filtered_resources.sort_by(|a, b| b.1.cmp(&a.1));

        // 返回仅包含 Resource 的列表 / Return list containing only Resource
        Ok(filtered_resources.into_iter().map(|(r, _)| r).collect())
    }

    async fn get_window_detail(
        &self,
        resource: Resource,
    ) -> Result<ReadResourceResult, MCPClientError> {
        if self.base.get_state().await != ClientState::Connected {
            return Err(MCPClientError::ConnectionError("Not connected".to_string()));
        }

        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "resources/read",
            "params": {
                "uri": resource.uri
            }
        });

        let response = self.send_request(&request).await?;

        if let Some(error) = response.get("error") {
            return Err(MCPClientError::ProtocolError(format!(
                "Read resource error: {}",
                error
            )));
        }

        if let Some(result) = response.get("result") {
            let read_result: ReadResourceResult = serde_json::from_value(result.clone())?;
            return Ok(read_result);
        }

        Err(MCPClientError::ProtocolError(
            "Invalid response".to_string(),
        ))
    }

    async fn subscribe_window(
        &self,
        resource: Resource,
    ) -> Result<(), MCPClientError> {
        if self.base.get_state().await != ClientState::Connected {
            return Err(MCPClientError::ConnectionError("Not connected".to_string()));
        }

        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "resources/subscribe",
            "params": {
                "uri": resource.uri
            }
        });

        let response = self.send_request(&request).await?;

        if let Some(error) = response.get("error") {
            return Err(MCPClientError::ProtocolError(format!(
                "Subscribe resource error: {}",
                error
            )));
        }

        Ok(())
    }

    async fn unsubscribe_window(
        &self,
        resource: Resource,
    ) -> Result<(), MCPClientError> {
        if self.base.get_state().await != ClientState::Connected {
            return Err(MCPClientError::ConnectionError("Not connected".to_string()));
        }

        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 8,
            "method": "resources/unsubscribe",
            "params": {
                "uri": resource.uri
            }
        });

        let response = self.send_request(&request).await?;

        if let Some(error) = response.get("error") {
            return Err(MCPClientError::ProtocolError(format!(
                "Unsubscribe resource error: {}",
                error
            )));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;
    use tokio::time::{sleep, Duration};

    #[tokio::test]
    async fn test_stdio_client_creation() {
        let params = StdioServerParameters {
            command: "echo".to_string(),
            args: vec!["hello".to_string()],
            env: HashMap::new(),
            cwd: None,
        };

        let client = StdioMCPClient::new(params);
        assert_eq!(client.state(), ClientState::Initialized);
        assert_eq!(client.base.params.command, "echo");
    }

    #[tokio::test]
    async fn test_stdio_client_with_env() {
        let mut env = HashMap::new();
        env.insert("TEST_VAR".to_string(), "test_value".to_string());
        env.insert("PATH".to_string(), "/usr/bin".to_string());

        let params = StdioServerParameters {
            command: "echo".to_string(),
            args: vec!["test".to_string()],
            env,
            cwd: Some("/tmp".to_string()),
        };

        let client = StdioMCPClient::new(params);
        assert_eq!(
            client.base.params.env.get("TEST_VAR"),
            Some(&"test_value".to_string())
        );
        assert_eq!(client.base.params.cwd, Some("/tmp".to_string()));
    }

    #[tokio::test]
    async fn test_session_id_management() {
        let params = StdioServerParameters {
            command: "echo".to_string(),
            args: vec!["test".to_string()],
            env: HashMap::new(),
            cwd: None,
        };

        let client = StdioMCPClient::new(params);

        // 初始会话ID应该为空 / Initial session ID should be None
        let session_id = client.session_id.lock().await;
        assert!(session_id.is_none());
        drop(session_id);

        // 设置会话ID / Set session ID
        *client.session_id.lock().await = Some("session123".to_string());
        let session_id = client.session_id.lock().await;
        assert_eq!(session_id.as_ref().unwrap(), "session123");
    }

    #[tokio::test]
    async fn test_start_child_process_with_echo() {
        let params = StdioServerParameters {
            command: "echo".to_string(),
            args: vec!["hello world".to_string()],
            env: HashMap::new(),
            cwd: None,
        };

        let client = StdioMCPClient::new(params);

        // 启动子进程
        let result = client.start_child_process(&client.base.params).await;
        assert!(result.is_ok());

        // 子进程应该成功启动
        let mut child = result.unwrap();

        // 等待一小段时间让进程运行
        sleep(Duration::from_millis(100)).await;

        // 尝试杀死进程（清理）
        let _ = child.kill().await;
    }

    #[tokio::test]
    async fn test_start_child_process_with_invalid_command() {
        let params = StdioServerParameters {
            command: "nonexistent_command_12345".to_string(),
            args: vec![],
            env: HashMap::new(),
            cwd: None,
        };

        let client = StdioMCPClient::new(params.clone());

        // 启动不存在的命令应该失败
        let result = client.start_child_process(&params).await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            MCPClientError::ConnectionError(_)
        ));
    }

    #[tokio::test]
    async fn test_send_request_without_process() {
        let params = StdioServerParameters {
            command: "echo".to_string(),
            args: vec!["test".to_string()],
            env: HashMap::new(),
            cwd: None,
        };

        let client = StdioMCPClient::new(params);

        // 没有进程时发送请求应该失败
        let request = json!({"jsonrpc": "2.0", "method": "test"});
        let result = client.send_request(&request).await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            MCPClientError::ConnectionError(_)
        ));
    }

    #[tokio::test]
    async fn test_connect_state_checks() {
        let params = StdioServerParameters {
            command: "echo".to_string(),
            args: vec!["test".to_string()],
            env: HashMap::new(),
            cwd: None,
        };

        let client = StdioMCPClient::new(params);

        // 在已连接状态下尝试连接应该失败
        client.base.update_state(ClientState::Connected).await;
        let result = client.connect().await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            MCPClientError::ConnectionError(_)
        ));
    }

    #[tokio::test]
    async fn test_disconnect_state_checks() {
        let params = StdioServerParameters {
            command: "echo".to_string(),
            args: vec!["test".to_string()],
            env: HashMap::new(),
            cwd: None,
        };

        let client = StdioMCPClient::new(params);

        // 在未连接状态下尝试断开应该失败
        let result = client.disconnect().await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            MCPClientError::ConnectionError(_)
        ));
    }

    #[tokio::test]
    async fn test_list_tools_requires_connection() {
        let params = StdioServerParameters {
            command: "echo".to_string(),
            args: vec!["test".to_string()],
            env: HashMap::new(),
            cwd: None,
        };

        let client = StdioMCPClient::new(params);

        // 未连接状态下调用 list_tools 应该失败
        let result = client.list_tools().await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            MCPClientError::ConnectionError(_)
        ));
    }

    #[tokio::test]
    async fn test_call_tool_requires_connection() {
        let params = StdioServerParameters {
            command: "echo".to_string(),
            args: vec!["test".to_string()],
            env: HashMap::new(),
            cwd: None,
        };

        let client = StdioMCPClient::new(params);

        // 未连接状态下调用 call_tool 应该失败
        let result = client.call_tool("test_tool", json!({})).await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            MCPClientError::ConnectionError(_)
        ));
    }

    #[tokio::test]
    async fn test_list_windows_requires_connection() {
        let params = StdioServerParameters {
            command: "echo".to_string(),
            args: vec!["test".to_string()],
            env: HashMap::new(),
            cwd: None,
        };

        let client = StdioMCPClient::new(params);

        // 未连接状态下调用 list_windows 应该失败
        let result = client.list_windows().await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            MCPClientError::ConnectionError(_)
        ));
    }

    #[tokio::test]
    async fn test_get_window_detail_requires_connection() {
        let params = StdioServerParameters {
            command: "echo".to_string(),
            args: vec!["test".to_string()],
            env: HashMap::new(),
            cwd: None,
        };

        let client = StdioMCPClient::new(params);

        let resource = Resource {
            uri: "window://123".to_string(),
            name: "Test Window".to_string(),
            description: None,
            mime_type: None,
        };

        // 未连接状态下调用 get_window_detail 应该失败
        let result = client.get_window_detail(resource).await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            MCPClientError::ConnectionError(_)
        ));
    }

    #[tokio::test]
    async fn test_initialize_session_request_format() {
        let params = StdioServerParameters {
            command: "echo".to_string(),
            args: vec!["test".to_string()],
            env: HashMap::new(),
            cwd: None,
        };

        let client = StdioMCPClient::new(params);

        // 由于 echo 不会返回有效的 JSON-RPC 响应，初始化会失败
        let result = client.initialize_session().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_disconnect_cleanup() {
        let params = StdioServerParameters {
            command: "echo".to_string(),
            args: vec!["test".to_string()],
            env: HashMap::new(),
            cwd: None,
        };

        let client = StdioMCPClient::new(params);

        // 设置会话ID
        *client.session_id.lock().await = Some("session123".to_string());

        // 设置为已连接状态
        client.base.update_state(ClientState::Connected).await;

        // 断开连接（即使失败也应该清理会话ID）
        let _ = client.disconnect().await;

        // 验证会话ID被清理
        let session_id = client.session_id.lock().await;
        assert!(session_id.is_none());

        // 验证状态变为已断开
        assert_eq!(client.base.get_state().await, ClientState::Disconnected);
    }

    #[tokio::test]
    async fn test_child_process_cleanup() {
        let params = StdioServerParameters {
            command: "sleep".to_string(),
            args: vec!["10".to_string()],
            env: HashMap::new(),
            cwd: None,
        };

        let client = StdioMCPClient::new(params.clone());

        // 启动一个长时间运行的进程
        let child = client.start_child_process(&params).await.unwrap();
        *client.child_process.lock().await = Some(child);

        // 设置为已连接状态（这样 disconnect 才会清理进程）
        client.base.update_state(ClientState::Connected).await;

        // 验证进程正在运行
        let child_guard = client.child_process.lock().await;
        assert!(child_guard.is_some());
        drop(child_guard);

        // 断开连接应该清理进程
        let _ = client.disconnect().await;

        // 验证进程被清理
        let child_guard = client.child_process.lock().await;
        assert!(child_guard.is_none());
    }

    #[tokio::test]
    async fn test_error_handling_in_list_tools() {
        let params = StdioServerParameters {
            command: "echo".to_string(),
            args: vec!["test".to_string()],
            env: HashMap::new(),
            cwd: None,
        };

        let client = StdioMCPClient::new(params);

        // 模拟已连接状态
        client.base.update_state(ClientState::Connected).await;

        // 尝试列出工具（会因为没有有效的 MCP 服务器而返回错误）
        let result = client.list_tools().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_error_handling_in_call_tool() {
        let params = StdioServerParameters {
            command: "echo".to_string(),
            args: vec!["test".to_string()],
            env: HashMap::new(),
            cwd: None,
        };

        let client = StdioMCPClient::new(params);

        // 模拟已连接状态
        client.base.update_state(ClientState::Connected).await;

        // 尝试调用工具（会因为没有有效的 MCP 服务器而返回错误）
        let result = client
            .call_tool("test_tool", json!({"param": "value"}))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_start_child_process_with_working_directory() {
        let params = StdioServerParameters {
            command: "pwd".to_string(),
            args: vec![],
            env: HashMap::new(),
            cwd: Some("/tmp".to_string()),
        };

        let client = StdioMCPClient::new(params.clone());

        // 启动子进程并设置工作目录
        let result = client.start_child_process(&params).await;
        assert!(result.is_ok());

        let mut child = result.unwrap();

        // 等待进程完成
        let _ = child.wait().await;
    }

    #[tokio::test]
    async fn test_stdio_client_debug_format() {
        let params = StdioServerParameters {
            command: "echo".to_string(),
            args: vec!["test".to_string()],
            env: HashMap::new(),
            cwd: None,
        };

        let client = StdioMCPClient::new(params);

        // 验证 Debug trait 实现
        let debug_str = format!("{:?}", client);
        assert!(debug_str.contains("StdioMCPClient"));
    }
}
