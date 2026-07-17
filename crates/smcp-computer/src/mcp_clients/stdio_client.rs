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
use async_trait::async_trait;
use rmcp::model::{
    CallToolRequest, CallToolRequestParam, CancelledNotificationParam, ClientInfo, ClientRequest,
    Implementation, PaginatedRequestParam, ReadResourceRequestParam,
    ResourceUpdatedNotificationParam, ServerResult, SubscribeRequestParam, UnsubscribeRequestParam,
};
use rmcp::service::{
    NotificationContext, PeerRequestOptions, RequestHandle, RunningService, ServiceExt,
};
use rmcp::transport::TokioChildProcess;
use rmcp::{ClientHandler, RoleClient};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

/// STDIO 客户端连接超时时间（秒）
/// Connect timeout for STDIO client (seconds)
const CONNECT_TIMEOUT_SECS: u64 = 30;

/// 库级别默认 cwd 子目录名 / Library-level default cwd subdirectory name
const DEFAULT_CWD_DIR_NAME: &str = ".a2c-smcp";

/// 解析子进程工作目录：优先使用显式配置，否则 fallback 到 ~/.a2c-smcp
/// Resolve child process cwd: use explicit value if provided, otherwise fallback to ~/.a2c-smcp
fn resolve_cwd(explicit_cwd: Option<&String>) -> Option<std::path::PathBuf> {
    if let Some(cwd) = explicit_cwd {
        return Some(std::path::PathBuf::from(cwd));
    }
    dirs::home_dir().map(|home| home.join(DEFAULT_CWD_DIR_NAME))
}

/// STDIO 客户端的 rmcp `ClientHandler`（#106）：把服务器主动通知转成 [`McpServerNotification`] 上报 channel。
///
/// 替换 rmcp 裸 `ClientInfo`（其 `ClientHandler` 三个 `on_*` 均 no-op，导致 tools/resources 变化被收到即丢弃）。
/// 回调**只发 channel、不做任何 peer 请求**——刷新/emit 由 event-loop 外的 Computer 消费者任务承担，
/// 从根上规避"在通知回调里内联 list_tools"的会话级重入风险（虽 rmcp 0.11 的通知在 event loop 外的 detached
/// task 执行、内联本不会死锁，但解耦更稳、且可串行去抖突发通知）。`get_info` 返回构造时的 `ClientInfo`。
#[derive(Clone)]
pub(crate) struct A2cClientHandler {
    info: ClientInfo,
    notify: Option<ClientNotifyCtx>,
}

impl A2cClientHandler {
    /// 用 A2C 默认 `ClientInfo`（name=`a2c-smcp-rust`, version=CARGO_PKG_VERSION）+ 可选通知接缝构造。
    /// 供 stdio/http 两个 rmcp 客户端共享（#106）。
    pub(crate) fn new(notify: Option<ClientNotifyCtx>) -> Self {
        Self {
            info: ClientInfo {
                protocol_version: Default::default(),
                capabilities: Default::default(),
                client_info: Implementation {
                    name: "a2c-smcp-rust".to_string(),
                    title: None,
                    version: env!("CARGO_PKG_VERSION").to_string(),
                    icons: None,
                    website_url: None,
                },
            },
            notify,
        }
    }
}

impl ClientHandler for A2cClientHandler {
    fn get_info(&self) -> ClientInfo {
        self.info.clone()
    }

    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        if let Some(n) = &self.notify {
            debug!(bundle_id = %n.bundle_id, "MCP tools/list_changed received");
            n.notify(McpChangeKind::ToolListChanged);
        }
    }

    async fn on_resource_list_changed(&self, _context: NotificationContext<RoleClient>) {
        if let Some(n) = &self.notify {
            debug!(bundle_id = %n.bundle_id, "MCP resources/list_changed received");
            n.notify(McpChangeKind::ResourceListChanged);
        }
    }

    async fn on_resource_updated(
        &self,
        params: ResourceUpdatedNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        if let Some(n) = &self.notify {
            debug!(bundle_id = %n.bundle_id, uri = %params.uri, "MCP resources/updated received");
            n.notify(McpChangeKind::ResourceUpdated { uri: params.uri });
        }
    }
}

/// STDIO MCP客户端 / STDIO MCP client
pub struct StdioMCPClient {
    /// 基础客户端 / Base client
    base: BaseMCPClient<StdioServerParameters>,
    /// rmcp 运行服务 / rmcp running service
    running_service: Arc<Mutex<Option<RunningService<RoleClient, A2cClientHandler>>>>,
    /// stderr 消费任务 / Background task draining child stderr
    stderr_drain_task: Arc<Mutex<Option<JoinHandle<()>>>>,
    /// 订阅管理器 / Subscription manager
    subscription_manager: SubscriptionManager,
    /// 资源缓存 / Resource cache
    resource_cache: ResourceCache,
    /// 运行期变化通知上报接缝（#106，None=不转发）/ runtime change-notification seam。
    notify: Option<ClientNotifyCtx>,
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
            stderr_drain_task: Arc::new(Mutex::new(None)),
            subscription_manager: SubscriptionManager::new(),
            resource_cache: ResourceCache::new(Duration::from_secs(60)),
            notify: None,
        }
    }

    /// 注入运行期变化通知上报接缝（#106）/ attach the runtime change-notification seam。
    ///
    /// 由 [`client_factory`](super::utils::client_factory) 在 manager 启动客户端时调用；须在 `connect` 前设置
    /// （`connect` 据此构造 `A2cClientHandler` 传给 `.serve()`）。
    pub fn with_notify(mut self, notify: Option<ClientNotifyCtx>) -> Self {
        self.notify = notify;
        self
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
        tokio::sync::MutexGuard<'_, Option<RunningService<RoleClient, A2cClientHandler>>>,
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
        if let Some(cwd) = resolve_cwd(params.cwd.as_ref()) {
            if !cwd.exists() {
                if let Err(e) = std::fs::create_dir_all(&cwd) {
                    warn!("Failed to create default cwd {:?}: {}", cwd, e);
                }
            }
            cmd.current_dir(&cwd);
            debug!("Child process cwd: {:?}", cwd);
        }

        debug!("Starting command: {} {:?}", params.command, params.args);

        let (transport, stderr) = TokioChildProcess::builder(cmd)
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                MCPClientError::ConnectionError(format!("Failed to start process: {}", e))
            })?;

        // Spawn background task to drain stderr, preventing pipe buffer deadlock
        let stderr_task = stderr.map(|stderr| {
            let cmd_name = params.command.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncBufReadExt, BufReader};
                let reader = BufReader::new(stderr);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    debug!(target: "mcp_stderr", "[{}] {}", cmd_name, line);
                }
            })
        });
        *self.stderr_drain_task.lock().await = stderr_task;

        // #106：用自定义 handler 取代裸 ClientInfo，使运行期 tools/resources 变化通知被转发给 Computer 消费者。
        let handler = A2cClientHandler::new(self.notify.clone());

        let service = tokio::time::timeout(
            Duration::from_secs(CONNECT_TIMEOUT_SECS),
            handler.serve(transport),
        )
        .await
        .map_err(|_| {
            MCPClientError::TimeoutError(format!(
                "STDIO connect timed out after {}s",
                CONNECT_TIMEOUT_SECS
            ))
        })?
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

        // 终止 stderr 消费任务 / Abort stderr drain task
        if let Some(handle) = self.stderr_drain_task.lock().await.take() {
            handle.abort();
        }

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

    /// 可取消 tool_call（INT-02 #70「最后一公里」的 **full-feasible** rmcp 路径）。
    ///
    /// rmcp 0.11 经 [`RequestHandle`] 暴露了客户端 `request_id`（Python 官方 SDK 不暴露——
    /// modelcontextprotocol/python-sdk#1410/#1419，故 Python 侧只能本地中断不补发；Rust 在此**领先**）。
    /// 因此用低层 [`Peer::send_request_with_option`](rmcp::service::Peer::send_request_with_option) 下发
    /// `tools/call` 以捕获 `request_id`，再把「等待响应（`rx`）」与「取消信号」`select!` 竞速：
    /// - 响应先到 → [`CancellableCallOutcome::Completed`]；
    /// - 取消先到 → 经捕获的 `request_id` best-effort 补发 MCP `notifications/cancelled`（time-box 2s，
    ///   防 teardown 卡住），返回 [`CancellableCallOutcome::Cancelled`]。MCP 取消为**协作式**：远端**可忽略**该
    ///   通知跑完，不作硬保证（协议 SHOULD）。`rx` 与 `peer` 由 `RequestHandle` 拆解后各自独立持有，
    ///   使两分支互不消费 `self`。
    async fn call_tool_cancellable(
        &self,
        tool_name: &str,
        params: serde_json::Value,
        cancel: CancellationToken,
    ) -> Result<CancellableCallOutcome, MCPClientError> {
        if self.base.get_state().await != ClientState::Connected {
            return Err(MCPClientError::ConnectionError("Not connected".to_string()));
        }

        let guard = self.get_service().await?;

        // 低层下发以捕获 rmcp 分配的 request_id（高层 service.call_tool 不暴露 id）。
        let request = ClientRequest::CallToolRequest(CallToolRequest {
            method: Default::default(),
            params: CallToolRequestParam {
                name: tool_name.to_string().into(),
                arguments: params.as_object().cloned(),
            },
            extensions: Default::default(),
        });
        // 内联 service 借用：`send_request_with_option` 返回后即结束对 guard 的借用。
        let handle: RequestHandle<RoleClient> = guard
            .as_ref()
            .unwrap()
            .send_request_with_option(request, PeerRequestOptions::no_options())
            .await
            .map_err(|e| MCPClientError::ProtocolError(format!("Call tool error: {}", e)))?;
        // handle 已 owned（id/rx/peer 均独立于 service）→ 立即释放 RunningService 互斥锁，放开同一 stdio
        // server 上的并发在途调用/取消（可取消调用可长/无界，绝不应在 select! 全程持锁）。RunningService
        // 由 self.running_service 的 Arc<Mutex> 保活，drop guard 仅释放锁、不析构 service，peer 仍可用。
        drop(guard);
        let request_id = handle.id.clone();
        // 拆解：rx（等待响应）与 peer（取消补发）各自独立，避免 await_response/cancel 互相消费 handle。
        let RequestHandle { rx, peer, .. } = handle;

        tokio::select! {
            biased;
            resp = rx => match resp {
                Ok(Ok(ServerResult::CallToolResult(r))) => Ok(CancellableCallOutcome::Completed(r)),
                Ok(Ok(_)) => Err(MCPClientError::ProtocolError(
                    "Unexpected response variant for tools/call".to_string(),
                )),
                Ok(Err(e)) => Err(MCPClientError::ProtocolError(format!("Call tool error: {}", e))),
                Err(_) => Err(MCPClientError::ConnectionError("MCP transport closed".to_string())),
            },
            _ = cancel.cancelled() => {
                // best-effort 协作式取消：补发 notifications/cancelled（远端可忽略）；time-box 防 teardown 卡死。
                let notify = peer.notify_cancelled(CancelledNotificationParam {
                    request_id,
                    reason: Some(smcp::tool_meta::A2C_DEFAULT_CANCEL_REASON.to_string()),
                });
                if tokio::time::timeout(Duration::from_secs(2), notify).await.is_err() {
                    warn!("emit MCP notifications/cancelled timed out (best-effort, ignored)");
                }
                Ok(CancellableCallOutcome::Cancelled)
            }
        }
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

        // 过滤 window:// 资源并按 priority 降序排序（v0.2 元数据下沉，逻辑共享）
        // Filter window:// resources and sort by priority desc (shared v0.2 metadata sink).
        Ok(crate::desktop::metadata::filter_and_sort_window_resources(
            all_resources,
        ))
    }

    async fn list_resources_page(
        &self,
        cursor: Option<String>,
    ) -> Result<(Vec<Resource>, Option<String>), MCPClientError> {
        if self.base.get_state().await != ClientState::Connected {
            return Err(MCPClientError::ConnectionError("Not connected".to_string()));
        }

        let guard = self.get_service().await?;
        let service = guard.as_ref().unwrap();

        // 能力校验：未声明 `resources` → CapabilityNotSupported（上层映射 4015）。
        // Capability gate: no `resources` → CapabilityNotSupported (mapped to 4015 upstream).
        let supports_resources = service
            .peer_info()
            .map(|info| info.capabilities.resources.is_some())
            .unwrap_or(false);
        if !supports_resources {
            return Err(MCPClientError::CapabilityNotSupported(
                "resources".to_string(),
            ));
        }

        // 单页透传：cursor 进/出，不聚合、不过滤、不返回 resourceTemplates。
        // Single-page passthrough: cursor in/out, no aggregation/filter, no resourceTemplates.
        let param = cursor.map(|c| PaginatedRequestParam { cursor: Some(c) });
        let result = service
            .list_resources(param)
            .await
            .map_err(|e| MCPClientError::ProtocolError(format!("List resources error: {}", e)))?;

        Ok((result.resources, result.next_cursor))
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

        // 验证 stderr_drain_task 被清理
        let guard = client.stderr_drain_task.lock().await;
        assert!(guard.is_none());
        drop(guard);

        // 验证状态变为已断开
        assert_eq!(client.base.get_state().await, ClientState::Disconnected);
    }

    #[test]
    fn test_resolve_cwd_explicit_value() {
        let explicit = "/tmp/my-cwd".to_string();
        let result = resolve_cwd(Some(&explicit));
        assert_eq!(result, Some(std::path::PathBuf::from("/tmp/my-cwd")));
    }

    #[test]
    fn test_resolve_cwd_defaults_to_a2c_smcp() {
        let result = resolve_cwd(None);
        assert!(result.is_some(), "resolve_cwd(None) should return Some");
        let path = result.unwrap();
        assert!(
            path.ends_with(DEFAULT_CWD_DIR_NAME),
            "default cwd should end with {}, got {:?}",
            DEFAULT_CWD_DIR_NAME,
            path
        );
    }

    #[test]
    fn test_resolve_cwd_returns_path_under_home() {
        let result = resolve_cwd(None);
        let home = dirs::home_dir().expect("HOME should be available in test");
        let path = result.unwrap();
        assert_eq!(path, home.join(DEFAULT_CWD_DIR_NAME));
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
