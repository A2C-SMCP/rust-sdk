/**
* 文件名: http_client
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: tokio, reqwest, serde_json
* 描述: HTTP类型的MCP客户端实现
*/
use super::base_client::BaseMCPClient;
use super::model::*;
use super::{ResourceCache, SubscriptionManager};
use crate::desktop::window_uri::{is_window_uri, WindowURI};
use async_trait::async_trait;
use reqwest::Client;
use serde_json;
use std::time::Duration;
use tracing::{debug, info, warn};

/// HTTP MCP客户端 / HTTP MCP client
pub struct HttpMCPClient {
    /// 基础客户端 / Base client
    base: BaseMCPClient<HttpServerParameters>,
    /// HTTP客户端 / HTTP client
    http_client: Client,
    /// 会话ID / Session ID
    session_id: std::sync::Arc<tokio::sync::Mutex<Option<String>>>,
    /// 订阅管理器 / Subscription manager
    subscription_manager: SubscriptionManager,
    /// 资源缓存 / Resource cache
    resource_cache: ResourceCache,
}

impl std::fmt::Debug for HttpMCPClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpMCPClient")
            .field("url", &self.base.params.url)
            .field("headers", &self.base.params.headers)
            .field("state", &self.base.state())
            .finish()
    }
}

impl HttpMCPClient {
    /// 创建新的HTTP客户端 / Create new HTTP client
    pub fn new(params: HttpServerParameters) -> Self {
        let http_client = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("Failed to create HTTP client");

        Self {
            base: BaseMCPClient::new(params),
            http_client,
            session_id: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            subscription_manager: SubscriptionManager::new(),
            resource_cache: ResourceCache::new(Duration::from_secs(60)), // 默认 60 秒 TTL
        }
    }

    /// 发送JSON-RPC请求 / Send JSON-RPC request
    async fn send_request(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, MCPClientError> {
        let url = &self.base.params.url;

        let mut request_body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
        });

        if let Some(p) = params {
            request_body["params"] = p;
        }

        // 添加请求ID / Add request ID
        request_body["id"] = serde_json::Value::Number(serde_json::Number::from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64,
        ));

        debug!("Sending HTTP request to {}: {}", url, request_body);

        let mut request = self.http_client.post(url);

        // 添加headers / Add headers
        for (key, value) in &self.base.params.headers {
            request = request.header(key, value);
        }

        // 添加content-type / Add content-type
        request = request.header("Content-Type", "application/json");

        let response =
            request.json(&request_body).send().await.map_err(|e| {
                MCPClientError::ConnectionError(format!("HTTP request failed: {}", e))
            })?;

        if !response.status().is_success() {
            return Err(MCPClientError::ConnectionError(format!(
                "HTTP error: {}",
                response.status()
            )));
        }

        let response_body: serde_json::Value = response.json().await.map_err(|e| {
            MCPClientError::ProtocolError(format!("Failed to parse response: {}", e))
        })?;

        debug!("Received HTTP response: {}", response_body);

        Ok(response_body)
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

        info!("HTTP session initialized successfully");
        Ok(())
    }

    // ========== 订阅管理 API / Subscription Management API ==========

    /// 检查是否已订阅指定资源
    pub async fn is_subscribed(&self, uri: &str) -> bool {
        self.subscription_manager.is_subscribed(uri).await
    }

    /// 获取所有订阅的 URI 列表
    pub async fn get_subscriptions(&self) -> Vec<String> {
        self.subscription_manager.get_subscriptions().await
    }

    /// 获取订阅数量
    pub async fn subscription_count(&self) -> usize {
        self.subscription_manager.subscription_count().await
    }

    // ========== 资源缓存 API / Resource Cache API ==========

    /// 获取缓存的资源数据
    pub async fn get_cached_resource(&self, uri: &str) -> Option<serde_json::Value> {
        self.resource_cache.get(uri).await
    }

    /// 检查是否有缓存
    pub async fn has_cache(&self, uri: &str) -> bool {
        self.resource_cache.contains(uri).await
    }

    /// 获取缓存大小
    pub async fn cache_size(&self) -> usize {
        self.resource_cache.size().await
    }

    /// 清理过期缓存
    pub async fn cleanup_cache(&self) -> usize {
        self.resource_cache.cleanup_expired().await
    }

    /// 获取所有缓存的 URI 列表
    pub async fn cache_keys(&self) -> Vec<String> {
        self.resource_cache.keys().await
    }

    /// 清空所有缓存
    pub async fn clear_cache(&self) {
        self.resource_cache.clear().await
    }
}

#[async_trait]
impl MCPClientProtocol for HttpMCPClient {
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

        // 初始化会话 / Initialize session
        self.initialize_session().await?;

        // 更新状态 / Update state
        self.base.update_state(ClientState::Connected).await;
        info!("HTTP client connected successfully");

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

        // 清理会话ID / Clear session ID
        *self.session_id.lock().await = None;

        // 更新状态 / Update state
        self.base.update_state(ClientState::Disconnected).await;
        info!("HTTP client disconnected successfully");

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

        // 支持分页获取资源 / Support pagination for resources
        let mut all_resources = Vec::new();
        let mut cursor: Option<String> = None;

        loop {
            let params = cursor.as_ref().map(|c| serde_json::json!({ "cursor": c }));

            let response = self.send_request("resources/list", params).await?;

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
                        if let Ok(parsed_resource) =
                            serde_json::from_value::<Resource>(resource.clone())
                        {
                            all_resources.push(parsed_resource);
                        }
                    }
                }

                // 检查是否有下一页 / Check if there's a next page
                cursor = result
                    .get("nextCursor")
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

    async fn subscribe_window(&self, resource: Resource) -> Result<(), MCPClientError> {
        if self.base.get_state().await != ClientState::Connected {
            return Err(MCPClientError::ConnectionError("Not connected".to_string()));
        }

        let params = serde_json::json!({
            "uri": resource.uri
        });

        let response = self
            .send_request("resources/subscribe", Some(params))
            .await?;

        if let Some(error) = response.get("error") {
            return Err(MCPClientError::ProtocolError(format!(
                "Subscribe resource error: {}",
                error
            )));
        }

        // 订阅成功后，更新本地订阅状态
        let _ = self
            .subscription_manager
            .add_subscription(resource.uri.clone())
            .await;

        // 立即获取并缓存资源数据
        match self.get_window_detail(resource.clone()).await {
            Ok(result) => {
                if !result.contents.is_empty() {
                    if let Ok(json_value) = serde_json::to_value(&result.contents[0]) {
                        self.resource_cache
                            .set(resource.uri.clone(), json_value, None)
                            .await;
                        info!("Subscribed and cached: {}", resource.uri);
                    }
                }
            }
            Err(e) => {
                warn!("Failed to fetch resource data after subscription: {:?}", e);
            }
        }

        Ok(())
    }

    async fn unsubscribe_window(&self, resource: Resource) -> Result<(), MCPClientError> {
        if self.base.get_state().await != ClientState::Connected {
            return Err(MCPClientError::ConnectionError("Not connected".to_string()));
        }

        let params = serde_json::json!({
            "uri": resource.uri
        });

        let response = self
            .send_request("resources/unsubscribe", Some(params))
            .await?;

        if let Some(error) = response.get("error") {
            return Err(MCPClientError::ProtocolError(format!(
                "Unsubscribe resource error: {}",
                error
            )));
        }

        // 取消订阅成功后，移除本地订阅状态
        let _ = self
            .subscription_manager
            .remove_subscription(&resource.uri)
            .await;

        // 清理缓存
        self.resource_cache.remove(&resource.uri).await;
        info!("Unsubscribed and removed cache: {}", resource.uri);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    #[tokio::test]
    async fn test_http_client_creation() {
        let params = HttpServerParameters {
            url: "http://localhost:8080".to_string(),
            headers: HashMap::new(),
        };

        let client = HttpMCPClient::new(params);
        assert_eq!(client.state(), ClientState::Initialized);
        assert_eq!(client.base.params.url, "http://localhost:8080");
    }

    #[tokio::test]
    async fn test_http_client_with_headers() {
        let mut headers = HashMap::new();
        headers.insert("Authorization".to_string(), "Bearer token123".to_string());
        headers.insert("Content-Type".to_string(), "application/json".to_string());

        let params = HttpServerParameters {
            url: "http://localhost:8080".to_string(),
            headers,
        };

        let client = HttpMCPClient::new(params);
        assert_eq!(
            client.base.params.headers.get("Authorization"),
            Some(&"Bearer token123".to_string())
        );
    }

    #[tokio::test]
    async fn test_session_id_management() {
        let params = HttpServerParameters {
            url: "http://localhost:8080".to_string(),
            headers: HashMap::new(),
        };

        let client = HttpMCPClient::new(params);

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
    async fn test_send_request_format() {
        let params = HttpServerParameters {
            url: "http://localhost:8080".to_string(),
            headers: HashMap::new(),
        };

        let client = HttpMCPClient::new(params);

        // 注意：这个测试需要一个 mock HTTP 服务器来实际验证请求格式
        // Note: This test would need a mock HTTP server to actually verify request format
        // 这里我们只验证请求构建逻辑不会 panic
        // Here we only verify that request building doesn't panic

        let method = "test/method";
        let params = Some(json!({"param1": "value1"}));

        // 由于没有实际的服务器，这个测试会失败，但我们可以验证错误处理
        // Since there's no actual server, this test will fail, but we can verify error handling
        let result = client.send_request(method, params).await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            MCPClientError::ConnectionError(_)
        ));
    }

    #[tokio::test]
    async fn test_connect_state_checks() {
        let params = HttpServerParameters {
            url: "http://localhost:8080".to_string(),
            headers: HashMap::new(),
        };

        let client = HttpMCPClient::new(params);

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
        let params = HttpServerParameters {
            url: "http://localhost:8080".to_string(),
            headers: HashMap::new(),
        };

        let client = HttpMCPClient::new(params);

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
        let params = HttpServerParameters {
            url: "http://localhost:8080".to_string(),
            headers: HashMap::new(),
        };

        let client = HttpMCPClient::new(params);

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
        let params = HttpServerParameters {
            url: "http://localhost:8080".to_string(),
            headers: HashMap::new(),
        };

        let client = HttpMCPClient::new(params);

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
        let params = HttpServerParameters {
            url: "http://localhost:8080".to_string(),
            headers: HashMap::new(),
        };

        let client = HttpMCPClient::new(params);

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
        let params = HttpServerParameters {
            url: "http://localhost:8080".to_string(),
            headers: HashMap::new(),
        };

        let client = HttpMCPClient::new(params);

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
        let params = HttpServerParameters {
            url: "http://localhost:8080".to_string(),
            headers: HashMap::new(),
        };

        let client = HttpMCPClient::new(params);

        // 由于没有实际服务器，初始化会失败，但我们可以验证请求格式
        let result = client.initialize_session().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_disconnect_cleanup() {
        let params = HttpServerParameters {
            url: "http://localhost:8080".to_string(),
            headers: HashMap::new(),
        };

        let client = HttpMCPClient::new(params);

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
    async fn test_error_handling_in_list_tools() {
        let params = HttpServerParameters {
            url: "http://localhost:8080".to_string(),
            headers: HashMap::new(),
        };

        let client = HttpMCPClient::new(params);

        // 模拟已连接状态
        client.base.update_state(ClientState::Connected).await;

        // 尝试列出工具（会因为连接失败而返回错误）
        let result = client.list_tools().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_error_handling_in_call_tool() {
        let params = HttpServerParameters {
            url: "http://localhost:8080".to_string(),
            headers: HashMap::new(),
        };

        let client = HttpMCPClient::new(params);

        // 模拟已连接状态
        client.base.update_state(ClientState::Connected).await;

        // 尝试调用工具（会因为连接失败而返回错误）
        let result = client
            .call_tool("test_tool", json!({"param": "value"}))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_http_client_debug_format() {
        let params = HttpServerParameters {
            url: "http://localhost:8080".to_string(),
            headers: HashMap::new(),
        };

        let client = HttpMCPClient::new(params);

        // 验证 Debug trait 实现
        let debug_str = format!("{:?}", client);
        assert!(debug_str.contains("HttpMCPClient"));
    }
}
