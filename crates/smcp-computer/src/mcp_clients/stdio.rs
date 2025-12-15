/*!
* 文件名: stdio.rs
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: tokio, serde_json
* 描述: MCP stdio客户端实现 / MCP stdio client implementation
*/

use crate::errors::McpClientError;
use crate::mcp_clients::base::{McpClient, McpClientState, McpTool, McpResource, McpCallToolResult};
use crate::mcp_clients::model::StdioServerParameters;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, RwLock};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

/// Stdio MCP客户端 / Stdio MCP client
pub struct StdioMcpClient {
    /// 服务器参数 / Server parameters
    params: StdioServerParameters,
    /// 客户端状态 / Client state
    state: Arc<RwLock<McpClientState>>,
    /// 子进程 / Child process
    child: Arc<Mutex<Option<Child>>>,
    /// 取消令牌 / Cancellation token
    cancel_token: CancellationToken,
    /// 请求ID计数器 / Request ID counter
    request_id: Arc<Mutex<u64>>,
    /// 等待响应的请求 / Pending requests
    pending_requests: Arc<Mutex<HashMap<u64, tokio::sync::oneshot::Sender<Value>>>>,
}

impl StdioMcpClient {
    /// 创建新的客户端 / Create new client
    pub fn new(params: StdioServerParameters) -> Self {
        Self {
            params,
            state: Arc::new(RwLock::new(McpClientState::Initialized)),
            child: Arc::new(Mutex::new(None)),
            cancel_token: CancellationToken::new(),
            request_id: Arc::new(Mutex::new(0)),
            pending_requests: Arc::new(Mutex::new(HashMap::new())),
        }
    }
    
    /// 发送请求 / Send request
    async fn send_request(&self, method: &str, params: Value) -> Result<Value, McpClientError> {
        let id = {
            let mut request_id = self.request_id.lock().await;
            *request_id += 1;
            *request_id
        };
        
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });
        
        // 创建响应接收器
        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let mut pending = self.pending_requests.lock().await;
            pending.insert(id, tx);
        }
        
        // 发送请求
        {
            let mut child = self.child.lock().await;
            if let Some(ref mut child) = *child {
                if let Some(mut stdin) = child.stdin.take() {
                    use tokio::io::AsyncWriteExt;
                    let request_str = serde_json::to_string(&request)
                        .map_err(|e| McpClientError::ConnectionError(e.to_string()))?;
                    stdin.write_all(request_str.as_bytes()).await
                        .map_err(|e| McpClientError::ConnectionError(e.to_string()))?;
                    stdin.write_all(b"\n").await
                        .map_err(|e| McpClientError::ConnectionError(e.to_string()))?;
                    stdin.flush().await
                        .map_err(|e| McpClientError::ConnectionError(e.to_string()))?;
                    // Put stdin back
                    child.stdin = Some(stdin);
                } else {
                    return Err(McpClientError::ConnectionError("Stdin not available".to_string()));
                }
            } else {
                return Err(McpClientError::NotConnected);
            }
        }
        
        // 等待响应
        match timeout(std::time::Duration::from_secs(30), rx).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(_)) => Err(McpClientError::ConnectionError("Response channel closed".to_string())),
            Err(_) => {
                // 超时，移除待处理请求
                let mut pending = self.pending_requests.lock().await;
                pending.remove(&id);
                Err(McpClientError::ConnectionError("Request timeout".to_string()))
            }
        }
    }
    
    /// 启动消息处理循环 / Start message processing loop
    async fn start_message_loop(&self) -> Result<(), McpClientError> {
        let child = self.child.clone();
        let pending_requests = self.pending_requests.clone();
        let cancel_token = self.cancel_token.clone();
        let state = self.state.clone();
        
        tokio::spawn(async move {
            let stdout = {
                let mut child_guard = child.lock().await;
                if let Some(ref mut child) = *child_guard {
                    child.stdout.take()
                } else {
                    None
                }
            };
            
            let stdout = if let Some(stdout) = stdout {
                stdout
            } else {
                return;
            };
            
            let mut reader = BufReader::new(stdout);
            
            loop {
                let mut line = String::new();
                tokio::select! {
                    _ = cancel_token.cancelled() => {
                        break;
                    }
                    line_result = reader.read_line(&mut line) => {
                        match line_result {
                            Ok(0) => {
                                // EOF
                                *state.write().await = McpClientState::Disconnected;
                                break;
                            }
                            Ok(_) => {
                                // Remove trailing newline
                                if line.ends_with('\n') {
                                    line.pop();
                                    if line.ends_with('\r') {
                                        line.pop();
                                    }
                                }
                                if let Ok(response) = serde_json::from_str::<Value>(&line) {
                                    if let Some(id) = response.get("id").and_then(|v| v.as_u64()) {
                                        let mut pending = pending_requests.lock().await;
                                        if let Some(tx) = pending.remove(&id) {
                                            let _ = tx.send(response);
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                eprintln!("Error reading from stdout: {}", e);
                                *state.write().await = McpClientState::Error;
                                break;
                            }
                        }
                    }
                }
            }
        });
        
        Ok(())
    }
    
    /// 初始化MCP会话 / Initialize MCP session
    async fn initialize(&self) -> Result<(), McpClientError> {
        let init_params = json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "tools": {},
                "resources": {
                    "subscribe": true
                }
            },
            "clientInfo": {
                "name": "smcp-computer",
                "version": env!("CARGO_PKG_VERSION")
            }
        });
        
        let _response = self.send_request("initialize", init_params).await?;
        
        // 发送initialized通知
        let _ = self.send_request("notifications/initialized", json!({})).await;
        
        Ok(())
    }
}

#[async_trait]
impl McpClient for StdioMcpClient {
    fn state(&self) -> McpClientState {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                self.state.read().await.clone()
            })
        })
    }
    
    async fn connect(&mut self) -> Result<(), McpClientError> {
        // 构建命令
        let mut cmd = Command::new(&self.params.command);
        cmd.args(&self.params.args);
        cmd.envs(&self.params.env);
        if let Some(cwd) = &self.params.cwd {
            cmd.current_dir(cwd);
        }
        
        // 配置stdio
        cmd.stdin(Stdio::piped())
           .stdout(Stdio::piped())
           .stderr(Stdio::piped());
        
        // 启动进程
        let child = cmd.spawn()
            .map_err(|e| McpClientError::ConnectionError(format!("Failed to spawn process: {}", e)))?;
        
        {
            let mut child_guard = self.child.lock().await;
            *child_guard = Some(child);
        }
        
        // 更新状态
        *self.state.write().await = McpClientState::Connected;
        
        // 启动消息处理循环
        self.start_message_loop().await?;
        
        // 初始化MCP会话
        self.initialize().await?;
        
        Ok(())
    }
    
    async fn disconnect(&mut self) -> Result<(), McpClientError> {
        // 取消所有操作
        self.cancel_token.cancel();
        
        // 关闭子进程
        let mut child_guard = self.child.lock().await;
        if let Some(mut child) = child_guard.take() {
            // 尝试优雅关闭
            if let Err(e) = child.kill().await {
                eprintln!("Failed to kill child process: {}", e);
            }
            
            // 等待进程结束
            if let Err(e) = child.wait().await {
                eprintln!("Failed to wait for child process: {}", e);
            }
        }
        
        // 清理待处理请求
        let mut pending = self.pending_requests.lock().await;
        for (_, tx) in pending.drain() {
            let _ = tx.send(json!({"error": {"code": -32000, "message": "Disconnected"}}));
        }
        
        // 更新状态
        *self.state.write().await = McpClientState::Disconnected;
        
        Ok(())
    }
    
    async fn list_tools(&self) -> Result<Vec<McpTool>, McpClientError> {
        let response = self.send_request("tools/list", json!({})).await?;
        
        if let Some(tools) = response.get("result").and_then(|r| r.get("tools")).and_then(|t| t.as_array()) {
            let mut result = Vec::new();
            for tool in tools {
                if let Ok(mcp_tool) = serde_json::from_value::<McpTool>(tool.clone()) {
                    result.push(mcp_tool);
                }
            }
            Ok(result)
        } else {
            Err(McpClientError::InvalidState("Invalid response format".to_string()))
        }
    }
    
    async fn call_tool(&self, tool_name: &str, params: Value) -> Result<McpCallToolResult, McpClientError> {
        let request_params = json!({
            "name": tool_name,
            "arguments": params
        });
        
        let response = self.send_request("tools/call", request_params).await?;
        
        if let Some(result) = response.get("result") {
            serde_json::from_value(result.clone())
                .map_err(|e| McpClientError::InvalidState(format!("Invalid response format: {}", e)))
        } else {
            Err(McpClientError::InvalidState("Invalid response format".to_string()))
        }
    }
    
    async fn list_windows(&self) -> Result<Vec<McpResource>, McpClientError> {
        let response = self.send_request("resources/list", json!({})).await?;
        
        if let Some(resources) = response.get("result").and_then(|r| r.get("resources")).and_then(|r| r.as_array()) {
            let mut result = Vec::new();
            for resource in resources {
                if let Ok(mcp_resource) = serde_json::from_value::<McpResource>(resource.clone()) {
                    // 只返回window://协议的资源
                    if mcp_resource.uri.starts_with("window://") {
                        result.push(mcp_resource);
                    }
                }
            }
            Ok(result)
        } else {
            Err(McpClientError::InvalidState("Invalid response format".to_string()))
        }
    }
    
    async fn get_window_detail(&self, resource: &McpResource) -> Result<McpCallToolResult, McpClientError> {
        let request_params = json!({
            "uri": resource.uri
        });
        
        let response = self.send_request("resources/read", request_params).await?;
        
        if let Some(result) = response.get("result") {
            serde_json::from_value(result.clone())
                .map_err(|e| McpClientError::InvalidState(format!("Invalid response format: {}", e)))
        } else {
            Err(McpClientError::InvalidState("Invalid response format".to_string()))
        }
    }
}
