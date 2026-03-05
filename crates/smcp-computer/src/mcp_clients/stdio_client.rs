/**
* 文件名: stdio_client
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: tokio, rmcp
* 描述: STDIO类型的MCP客户端实现，委托 rmcp SDK
*/
use super::base_client::BaseMCPClient;
use super::model::*;
use super::{ResourceCache, SubscriptionManager};
use crate::desktop::window_uri::{is_window_uri, WindowURI};
use async_trait::async_trait;
use rmcp::model::{
    CallToolRequestParam, ClientInfo, Implementation, ReadResourceRequestParam,
    SubscribeRequestParam, UnsubscribeRequestParam,
};
use rmcp::service::{RunningService, ServiceExt};
use rmcp::transport::TokioChildProcess;
use rmcp::RoleClient;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::{ChildStderr, Command};
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

/// STDIO MCP客户端 / STDIO MCP client
pub struct StdioMCPClient {
    /// 基础客户端 / Base client
    base: BaseMCPClient<StdioServerParameters>,
    /// rmcp 运行服务 / rmcp running service
    running_service: Arc<Mutex<Option<RunningService<RoleClient, ClientInfo>>>>,
    /// 子进程 stderr / Child process stderr
    child_stderr: Arc<Mutex<Option<ChildStderr>>>,
    /// 订阅管理器 / Subscription manager
    subscription_manager: SubscriptionManager,
    /// 资源缓存 / Resource cache
    resource_cache: ResourceCache,
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
            running_service: Arc::new(Mutex::new(None)),
            child_stderr: Arc::new(Mutex::new(None)),
            subscription_manager: SubscriptionManager::new(),
            resource_cache: ResourceCache::new(Duration::from_secs(60)),
        }
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

    /// 检查资源是否已缓存
    pub async fn has_cache(&self, uri: &str) -> bool {
        self.resource_cache.contains(uri).await
    }

    /// 获取缓存大小
    pub async fn cache_size(&self) -> usize {
        self.resource_cache.size().await
    }

    /// 清理过期的缓存
    pub async fn cleanup_cache(&self) -> usize {
        self.resource_cache.cleanup_expired().await
    }

    /// 清空所有缓存
    pub async fn clear_cache(&self) {
        self.resource_cache.clear().await
    }

    /// 获取所有缓存的 URI 列表
    pub async fn cache_keys(&self) -> Vec<String> {
        self.resource_cache.keys().await
    }

    /// 获取 running service 的 guard，验证 service 可用
    /// Get running service guard, verifying service is available
    async fn get_service(
        &self,
    ) -> Result<
        tokio::sync::MutexGuard<'_, Option<RunningService<RoleClient, ClientInfo>>>,
        MCPClientError,
    > {
        let guard = self.running_service.lock().await;
        if guard.is_none() {
            return Err(MCPClientError::ConnectionError(
                "Service not available".to_string(),
            ));
        }
        Ok(guard)
    }
}

#[async_trait]
impl MCPClientProtocol for StdioMCPClient {
    fn state(&self) -> ClientState {
        self.base.state()
    }

    async fn connect(&self) -> Result<(), MCPClientError> {
        if !self.base.can_connect().await {
            return Err(MCPClientError::ConnectionError(format!(
                "Cannot connect in state: {}",
                self.base.get_state().await
            )));
        }

        let params = &self.base.params;

        let mut cmd = Command::new(&params.command);
        cmd.args(&params.args);
        for (key, value) in &params.env {
            cmd.env(key, value);
        }
        if let Some(cwd) = &params.cwd {
            cmd.current_dir(cwd);
        }

        debug!("Starting command: {} {:?}", params.command, params.args);

        let (transport, stderr) = TokioChildProcess::builder(cmd)
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                MCPClientError::ConnectionError(format!("Failed to start process: {}", e))
            })?;

        *self.child_stderr.lock().await = stderr;

        let client_info = ClientInfo {
            protocol_version: Default::default(),
            capabilities: Default::default(),
            client_info: Implementation {
                name: "a2c-smcp-rust".to_string(),
                title: None,
                version: env!("CARGO_PKG_VERSION").to_string(),
                icons: None,
                website_url: None,
            },
        };

        let service = client_info
            .serve(transport)
            .await
            .map_err(|e| MCPClientError::ConnectionError(format!("Initialize failed: {}", e)))?;

        *self.running_service.lock().await = Some(service);
        self.base.update_state(ClientState::Connected).await;
        info!("STDIO client connected successfully");

        Ok(())
    }

    async fn disconnect(&self) -> Result<(), MCPClientError> {
        if !self.base.can_disconnect().await {
            return Err(MCPClientError::ConnectionError(format!(
                "Cannot disconnect in state: {}",
                self.base.get_state().await
            )));
        }

        let service = self.running_service.lock().await.take();
        if let Some(service) = service {
            match service.cancel().await {
                Ok(reason) => {
                    debug!("Service stopped with reason: {:?}", reason);
                }
                Err(e) => {
                    error!("Error stopping service: {}", e);
                }
            }
        }

        // 清理 stderr handle
        *self.child_stderr.lock().await = None;

        self.base.update_state(ClientState::Disconnected).await;
        info!("STDIO client disconnected successfully");

        Ok(())
    }

    async fn list_tools(&self) -> Result<Vec<Tool>, MCPClientError> {
        if self.base.get_state().await != ClientState::Connected {
            return Err(MCPClientError::ConnectionError("Not connected".to_string()));
        }

        let guard = self.get_service().await?;
        let service = guard.as_ref().unwrap();

        let tools = service
            .list_all_tools()
            .await
            .map_err(|e| MCPClientError::ProtocolError(format!("List tools error: {}", e)))?;

        info!("Found {} tools", tools.len());
        Ok(tools)
    }

    async fn call_tool(
        &self,
        tool_name: &str,
        params: serde_json::Value,
    ) -> Result<CallToolResult, MCPClientError> {
        if self.base.get_state().await != ClientState::Connected {
            return Err(MCPClientError::ConnectionError("Not connected".to_string()));
        }

        let guard = self.get_service().await?;
        let service = guard.as_ref().unwrap();

        let result = service
            .call_tool(CallToolRequestParam {
                name: tool_name.to_string().into(),
                arguments: params.as_object().cloned(),
            })
            .await
            .map_err(|e| MCPClientError::ProtocolError(format!("Call tool error: {}", e)))?;

        Ok(result)
    }

    async fn list_windows(&self) -> Result<Vec<Resource>, MCPClientError> {
        if self.base.get_state().await != ClientState::Connected {
            return Err(MCPClientError::ConnectionError("Not connected".to_string()));
        }

        let guard = self.get_service().await?;
        let service = guard.as_ref().unwrap();

        let all_resources = service
            .list_all_resources()
            .await
            .map_err(|e| MCPClientError::ProtocolError(format!("List resources error: {}", e)))?;

        // 过滤 window:// 资源并按 priority 排序
        let mut filtered_resources: Vec<(Resource, i32)> = Vec::new();

        for resource in all_resources {
            if !is_window_uri(&resource.uri) {
                continue;
            }

            let priority = if let Ok(uri) = WindowURI::new(&resource.uri) {
                uri.priority().unwrap_or(0)
            } else {
                0
            };

            filtered_resources.push((resource, priority));
        }

        filtered_resources.sort_by(|a, b| b.1.cmp(&a.1));

        Ok(filtered_resources.into_iter().map(|(r, _)| r).collect())
    }

    async fn get_window_detail(
        &self,
        resource: Resource,
    ) -> Result<ReadResourceResult, MCPClientError> {
        if self.base.get_state().await != ClientState::Connected {
            return Err(MCPClientError::ConnectionError("Not connected".to_string()));
        }

        let guard = self.get_service().await?;
        let service = guard.as_ref().unwrap();

        let result = service
            .read_resource(ReadResourceRequestParam {
                uri: resource.uri.clone(),
            })
            .await
            .map_err(|e| MCPClientError::ProtocolError(format!("Read resource error: {}", e)))?;

        Ok(result)
    }

    async fn subscribe_window(&self, resource: Resource) -> Result<(), MCPClientError> {
        if self.base.get_state().await != ClientState::Connected {
            return Err(MCPClientError::ConnectionError("Not connected".to_string()));
        }

        let guard = self.get_service().await?;
        let service = guard.as_ref().unwrap();

        service
            .subscribe(SubscribeRequestParam {
                uri: resource.uri.clone(),
            })
            .await
            .map_err(|e| {
                MCPClientError::ProtocolError(format!("Subscribe resource error: {}", e))
            })?;

        drop(guard);

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

        let guard = self.get_service().await?;
        let service = guard.as_ref().unwrap();

        service
            .unsubscribe(UnsubscribeRequestParam {
                uri: resource.uri.clone(),
            })
            .await
            .map_err(|e| {
                MCPClientError::ProtocolError(format!("Unsubscribe resource error: {}", e))
            })?;

        drop(guard);

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
    use std::collections::HashMap;

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

        let result = client.call_tool("test_tool", serde_json::json!({})).await;
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

        let resource = make_resource("window://123", "Test Window", None, None);

        let result = client.get_window_detail(resource).await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            MCPClientError::ConnectionError(_)
        ));
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

        // 设置为已连接状态
        client.base.update_state(ClientState::Connected).await;

        // 断开连接
        let _ = client.disconnect().await;

        // 验证 running_service 被清理
        let guard = client.running_service.lock().await;
        assert!(guard.is_none());
        drop(guard);

        // 验证状态变为已断开
        assert_eq!(client.base.get_state().await, ClientState::Disconnected);
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

        let debug_str = format!("{:?}", client);
        assert!(debug_str.contains("StdioMCPClient"));
    }
}
