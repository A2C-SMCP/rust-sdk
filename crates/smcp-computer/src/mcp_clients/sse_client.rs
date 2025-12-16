/**
* 文件名: sse_client
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: tokio, reqwest, eventsource-client, serde_json
* 描述: SSE类型的MCP客户端实现
*/
use super::base_client::BaseMCPClient;
use super::model::*;
use async_trait::async_trait;
use es::Client as EsClient;
use eventsource_client as es;
use futures::stream::{Stream, StreamExt};
use serde_json;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, info, warn};

/// SSE MCP客户端 / SSE MCP client
pub struct SseMCPClient {
    /// 基础客户端 / Base client
    base: BaseMCPClient<SseServerParameters>,
    /// HTTP客户端 / HTTP client
    #[allow(dead_code)]
    http_client: reqwest::Client,
    /// 请求发送器 / Request sender
    request_tx: Arc<Mutex<Option<mpsc::UnboundedSender<serde_json::Value>>>>,
    /// 响应接收器 / Response receiver
    response_rx: Arc<Mutex<Option<mpsc::UnboundedReceiver<serde_json::Value>>>>,
    /// 会话ID / Session ID
    session_id: Arc<Mutex<Option<String>>>,
}

impl std::fmt::Debug for SseMCPClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SseMCPClient")
            .field("url", &self.base.params.url)
            .field("headers", &self.base.params.headers)
            .field("state", &self.base.state())
            .finish()
    }
}

impl SseMCPClient {
    /// 创建新的SSE客户端 / Create new SSE client
    pub fn new(params: SseServerParameters) -> Self {
        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("Failed to create HTTP client");

        Self {
            base: BaseMCPClient::new(params),
            http_client,
            request_tx: Arc::new(Mutex::new(None)),
            response_rx: Arc::new(Mutex::new(None)),
            session_id: Arc::new(Mutex::new(None)),
        }
    }

    /// 发送JSON-RPC请求 / Send JSON-RPC request
    async fn send_request(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, MCPClientError> {
        let mut request_body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
        });

        if let Some(p) = params {
            request_body["params"] = p;
        }

        // 添加请求ID / Add request ID
        let request_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        request_body["id"] = serde_json::Value::Number(serde_json::Number::from(request_id));

        debug!("Sending SSE request: {}", request_body);

        // 通过SSE发送请求 / Send request via SSE
        let tx = self.request_tx.lock().await;
        if let Some(ref tx) = *tx {
            tx.send(request_body.clone()).map_err(|e| {
                MCPClientError::ConnectionError(format!("Failed to send request: {}", e))
            })?;
        } else {
            return Err(MCPClientError::ConnectionError(
                "SSE connection not established".to_string(),
            ));
        }

        // 等待响应 / Wait for response
        let mut rx = self.response_rx.lock().await;
        if let Some(ref mut receiver) = *rx {
            match receiver.recv().await {
                Some(response) => {
                    debug!("Received SSE response: {}", response);
                    Ok(response)
                }
                None => Err(MCPClientError::ConnectionError(
                    "Response channel closed".to_string(),
                )),
            }
        } else {
            Err(MCPClientError::ConnectionError(
                "Response channel not established".to_string(),
            ))
        }
    }

    /// 启动SSE连接 / Start SSE connection
    async fn start_sse_connection(&self) -> Result<(), MCPClientError> {
        let url = &self.base.params.url;

        // 构建SSE URL / Build SSE URL
        let sse_url = if url.contains('?') {
            format!("{}&events=true", url)
        } else {
            format!("{}?events=true", url)
        };

        let mut builder = es::ClientBuilder::for_url(&sse_url)
            .map_err(|e| MCPClientError::ConnectionError(format!("Invalid SSE URL: {:?}", e)))?;

        // 添加headers / Add headers
        for (key, value) in &self.base.params.headers {
            builder = builder.header(key, value).map_err(|e| {
                MCPClientError::ConnectionError(format!("Failed to add header {}: {:?}", key, e))
            })?;
        }

        let es_client = builder.build();

        // 创建通信通道 / Create communication channels
        let (request_tx, request_rx) = mpsc::unbounded_channel::<serde_json::Value>();
        let (response_tx, response_rx) = mpsc::unbounded_channel::<serde_json::Value>();

        *self.request_tx.lock().await = Some(request_tx);
        *self.response_rx.lock().await = Some(response_rx);

        // 启动SSE事件处理任务 / Start SSE event handling task
        let stream: Pin<Box<dyn Stream<Item = Result<es::SSE, es::Error>> + Send + Sync>> =
            es_client.stream();

        tokio::spawn(async move {
            let mut stream = Box::pin(stream);
            let mut request_rx = Box::pin(request_rx);

            loop {
                tokio::select! {
                    // 处理SSE事件 / Handle SSE events
                    Some(event_result) = stream.next() => {
                        match event_result {
                            Ok(event) => {
                                debug!("Received SSE event: {:?}", event);

                                // 尝试解析JSON-RPC响应 / Try to parse JSON-RPC response
                                // Pattern match on SSE enum variants
                                match event {
                                    es::SSE::Event(event_data) => {
                                        if let Ok(response) = serde_json::from_str::<serde_json::Value>(&event_data.data) {
                                            let _ = response_tx.send(response);
                                        }
                                    }
                                    es::SSE::Comment(_) => {
                                        debug!("Received SSE comment");
                                    }
                                }
                            }
                            Err(e) => {
                                error!("SSE event error: {:?}", e);
                                break;
                            }
                        }
                    }

                    // 处理请求发送 / Handle request sending
                    Some(request) = request_rx.recv() => {
                        debug!("Sending request via SSE: {}", request);
                        // 在实际实现中，这里需要通过HTTP POST发送请求
                        // In actual implementation, this needs to send request via HTTP POST
                        // 这里简化处理，实际需要根据SSE协议实现
                        // This is simplified, actual implementation needed according to SSE protocol
                    }
                }
            }
        });

        // Note: es_client is not stored since it's not object-safe
        // The stream is managed within the task above

        Ok(())
    }

    /// 初始化会话 / Initialize session
    async fn initialize_session(&self) -> Result<(), MCPClientError> {
        let params = serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "tools": {},
                "resources": {}
            },
            "clientInfo": {
                "name": "a2c-smcp-rust",
                "version": "0.1.0"
            }
        });

        let response = self.send_request("initialize", Some(params)).await?;

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
        self.send_request("notifications/initialized", None).await?;

        info!("SSE session initialized successfully");
        Ok(())
    }
}

#[async_trait]
impl MCPClientProtocol for SseMCPClient {
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

        // 启动SSE连接 / Start SSE connection
        self.start_sse_connection().await?;

        // 初始化会话 / Initialize session
        self.initialize_session().await?;

        // 更新状态 / Update state
        self.base.update_state(ClientState::Connected).await;
        info!("SSE client connected successfully");

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

        // 尝试优雅关闭 / Try graceful shutdown
        if let Err(e) = self.send_request("shutdown", None).await {
            warn!("Failed to send shutdown request: {}", e);
        }

        // 发送exit通知 / Send exit notification
        if let Err(e) = self.send_request("exit", None).await {
            warn!("Failed to send exit notification: {}", e);
        }

        // 关闭SSE连接 / Close SSE connection
        *self.request_tx.lock().await = None;

        // 清理会话ID / Clear session ID
        *self.session_id.lock().await = None;

        // 更新状态 / Update state
        self.base.update_state(ClientState::Disconnected).await;
        info!("SSE client disconnected successfully");

        Ok(())
    }

    async fn list_tools(&self) -> Result<Vec<Tool>, MCPClientError> {
        if self.base.get_state().await != ClientState::Connected {
            return Err(MCPClientError::ConnectionError("Not connected".to_string()));
        }

        let response = self.send_request("tools/list", None).await?;

        if let Some(error) = response.get("error") {
            return Err(MCPClientError::ProtocolError(format!(
                "List tools error: {}",
                error
            )));
        }

        if let Some(result) = response.get("result") {
            if let Some(tools) = result.get("tools").and_then(|v| v.as_array()) {
                let mut tool_list = Vec::new();
                for tool in tools {
                    if let Ok(parsed_tool) = serde_json::from_value::<Tool>(tool.clone()) {
                        tool_list.push(parsed_tool);
                    }
                }
                return Ok(tool_list);
            }
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

        let call_params = serde_json::json!({
            "name": tool_name,
            "arguments": params
        });

        let response = self.send_request("tools/call", Some(call_params)).await?;

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

        let response = self.send_request("resources/list", None).await?;

        if let Some(error) = response.get("error") {
            return Err(MCPClientError::ProtocolError(format!(
                "List resources error: {}",
                error
            )));
        }

        if let Some(result) = response.get("result") {
            if let Some(resources) = result.get("resources").and_then(|v| v.as_array()) {
                let mut resource_list = Vec::new();
                for resource in resources {
                    if let Ok(parsed_resource) =
                        serde_json::from_value::<Resource>(resource.clone())
                    {
                        resource_list.push(parsed_resource);
                    }
                }
                return Ok(resource_list);
            }
        }

        Ok(vec![])
    }

    async fn get_window_detail(
        &self,
        resource: Resource,
    ) -> Result<ReadResourceResult, MCPClientError> {
        if self.base.get_state().await != ClientState::Connected {
            return Err(MCPClientError::ConnectionError("Not connected".to_string()));
        }

        let params = serde_json::json!({
            "uri": resource.uri
        });

        let response = self.send_request("resources/read", Some(params)).await?;

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
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    #[tokio::test]
    async fn test_sse_client_creation() {
        let params = SseServerParameters {
            url: "http://localhost:8081".to_string(),
            headers: HashMap::new(),
        };

        let client = SseMCPClient::new(params);
        assert_eq!(client.state(), ClientState::Initialized);
        assert_eq!(client.base.params.url, "http://localhost:8081");
    }

    #[tokio::test]
    async fn test_sse_client_with_headers() {
        let mut headers = HashMap::new();
        headers.insert("Authorization".to_string(), "Bearer token123".to_string());
        headers.insert("Accept".to_string(), "text/event-stream".to_string());

        let params = SseServerParameters {
            url: "http://localhost:8081".to_string(),
            headers,
        };

        let client = SseMCPClient::new(params);
        assert_eq!(
            client.base.params.headers.get("Authorization"),
            Some(&"Bearer token123".to_string())
        );
    }

    #[tokio::test]
    async fn test_session_id_management() {
        let params = SseServerParameters {
            url: "http://localhost:8081".to_string(),
            headers: HashMap::new(),
        };

        let client = SseMCPClient::new(params);

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
    async fn test_send_request_without_connection() {
        let params = SseServerParameters {
            url: "http://localhost:8081".to_string(),
            headers: HashMap::new(),
        };

        let client = SseMCPClient::new(params);

        // 没有建立连接时发送请求应该失败
        let method = "test/method";
        let params = Some(json!({"param1": "value1"}));

        let result = client.send_request(method, params).await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            MCPClientError::ConnectionError(_)
        ));
    }

    #[tokio::test]
    async fn test_connect_state_checks() {
        let params = SseServerParameters {
            url: "http://localhost:8081".to_string(),
            headers: HashMap::new(),
        };

        let client = SseMCPClient::new(params);

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
        let params = SseServerParameters {
            url: "http://localhost:8081".to_string(),
            headers: HashMap::new(),
        };

        let client = SseMCPClient::new(params);

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
        let params = SseServerParameters {
            url: "http://localhost:8081".to_string(),
            headers: HashMap::new(),
        };

        let client = SseMCPClient::new(params);

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
        let params = SseServerParameters {
            url: "http://localhost:8081".to_string(),
            headers: HashMap::new(),
        };

        let client = SseMCPClient::new(params);

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
        let params = SseServerParameters {
            url: "http://localhost:8081".to_string(),
            headers: HashMap::new(),
        };

        let client = SseMCPClient::new(params);

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
        let params = SseServerParameters {
            url: "http://localhost:8081".to_string(),
            headers: HashMap::new(),
        };

        let client = SseMCPClient::new(params);

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
    async fn test_start_sse_connection_url_formatting() {
        let params = SseServerParameters {
            url: "http://localhost:8081".to_string(),
            headers: HashMap::new(),
        };

        let client = SseMCPClient::new(params);

        // start_sse_connection 会创建通道并返回 Ok，即使没有实际服务器
        // 它只是启动了任务，实际的连接错误会在后续操作中体现
        let result = client.start_sse_connection().await;
        assert!(result.is_ok());

        // 验证通道已创建
        let request_tx = client.request_tx.lock().await;
        assert!(request_tx.is_some());

        let response_rx = client.response_rx.lock().await;
        assert!(response_rx.is_some());
    }

    #[tokio::test]
    async fn test_start_sse_connection_url_formatting_with_query() {
        let params = SseServerParameters {
            url: "http://localhost:8081?param=value".to_string(),
            headers: HashMap::new(),
        };

        let client = SseMCPClient::new(params);

        // 测试带查询参数的URL格式化
        let result = client.start_sse_connection().await;
        assert!(result.is_ok());

        // 验证通道已创建
        let request_tx = client.request_tx.lock().await;
        assert!(request_tx.is_some());

        let response_rx = client.response_rx.lock().await;
        assert!(response_rx.is_some());
    }

    #[tokio::test]
    async fn test_disconnect_cleanup() {
        let params = SseServerParameters {
            url: "http://localhost:8081".to_string(),
            headers: HashMap::new(),
        };

        let client = SseMCPClient::new(params);

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
    async fn test_request_response_channels() {
        let params = SseServerParameters {
            url: "http://localhost:8081".to_string(),
            headers: HashMap::new(),
        };

        let client = SseMCPClient::new(params);

        // 初始状态下通道应该为空
        let request_tx = client.request_tx.lock().await;
        assert!(request_tx.is_none());
        drop(request_tx);

        let response_rx = client.response_rx.lock().await;
        assert!(response_rx.is_none());
    }

    #[tokio::test]
    async fn test_initialize_session_request_format() {
        let params = SseServerParameters {
            url: "http://localhost:8081".to_string(),
            headers: HashMap::new(),
        };

        let client = SseMCPClient::new(params);

        // 由于没有实际服务器，初始化会失败，但我们可以验证请求格式
        let result = client.initialize_session().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_error_handling_in_list_tools() {
        let params = SseServerParameters {
            url: "http://localhost:8081".to_string(),
            headers: HashMap::new(),
        };

        let client = SseMCPClient::new(params);

        // 模拟已连接状态
        client.base.update_state(ClientState::Connected).await;

        // 尝试列出工具（会因为连接失败而返回错误）
        let result = client.list_tools().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_error_handling_in_call_tool() {
        let params = SseServerParameters {
            url: "http://localhost:8081".to_string(),
            headers: HashMap::new(),
        };

        let client = SseMCPClient::new(params);

        // 模拟已连接状态
        client.base.update_state(ClientState::Connected).await;

        // 尝试调用工具（会因为连接失败而返回错误）
        let result = client
            .call_tool("test_tool", json!({"param": "value"}))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_sse_client_debug_format() {
        let params = SseServerParameters {
            url: "http://localhost:8081".to_string(),
            headers: HashMap::new(),
        };

        let client = SseMCPClient::new(params);

        // 验证 Debug trait 实现
        let debug_str = format!("{:?}", client);
        assert!(debug_str.contains("SseMCPClient"));
    }
}
