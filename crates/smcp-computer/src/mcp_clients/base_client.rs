/**
* 文件名: base_client
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: tokio, async-trait, serde_json
* 描述: MCP客户端基础抽象类，提供状态管理和会话生命周期管理
*/
use super::model::*;
use crate::errors::ComputerError;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::{watch, Mutex, RwLock};
use tokio::task::JoinHandle;
use tokio::time::{timeout, Duration};
use tracing::{debug, error, info, warn};

/// MCP客户端基础实现 / Base MCP client implementation
pub struct BaseMCPClient<P> {
    /// 服务器参数 / Server parameters
    pub params: P,
    /// 当前状态 / Current state
    state: Arc<RwLock<ClientState>>,
    /// 状态变化通知 / State change notification
    state_notifier: watch::Sender<ClientState>,
    /// 会话保持任务句柄 / Session keep-alive task handle
    keep_alive_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
    /// 关闭信号 / Shutdown signal
    shutdown_tx: Arc<Mutex<Option<watch::Sender<bool>>>>,
    /// 状态变化回调 / State change callback
    state_change_callback: Option<Box<dyn Fn(ClientState, ClientState) + Send + Sync>>,
}

impl<P> BaseMCPClient<P>
where
    P: Send + Sync + 'static + std::clone::Clone,
{
    /// 创建新的基础客户端 / Create new base client
    pub fn new(params: P) -> Self {
        let (state_tx, _) = watch::channel(ClientState::Initialized);
        let state = Arc::new(RwLock::new(ClientState::Initialized));
        let (shutdown_tx, _) = watch::channel(false);

        Self {
            params,
            state,
            state_notifier: state_tx,
            keep_alive_handle: Arc::new(Mutex::new(None)),
            shutdown_tx: Arc::new(Mutex::new(Some(shutdown_tx))),
            state_change_callback: None,
        }
    }

    /// 设置状态变化回调 / Set state change callback
    pub fn set_state_change_callback<F>(&mut self, callback: F)
    where
        F: Fn(ClientState, ClientState) + Send + Sync + 'static,
    {
        self.state_change_callback = Some(Box::new(callback));
    }

    /// 获取当前状态 / Get current state
    pub async fn get_state(&self) -> ClientState {
        *self.state.read().await
    }

    /// 获取状态变化通知器 / Get state change notifier
    pub fn get_state_notifier(&self) -> watch::Receiver<ClientState> {
        self.state_notifier.subscribe()
    }

    /// 更新状态 / Update state
    pub async fn update_state(&self, new_state: ClientState) {
        let mut state = self.state.write().await;
        let old_state = *state;
        *state = new_state;

        // 通知状态变化 / Notify state change
        let _ = self.state_notifier.send(new_state);

        // 调用回调 / Call callback
        if let Some(ref callback) = self.state_change_callback {
            callback(old_state, new_state);
        }

        debug!("State transition: {} -> {}", old_state, new_state);
    }

    /// 启动会话保持任务 / Start session keep-alive task
    #[allow(dead_code)]
    async fn start_keep_alive<T>(&self, session_creator: impl Fn(P) -> T + Send + Sync + 'static)
    where
        T: std::future::Future<Output = Result<(), MCPClientError>> + Send + 'static,
    {
        let params = self.params.clone();
        let mut shutdown_rx = self.create_shutdown_receiver().await;
        let state = self.state.clone();

        let handle = tokio::spawn(async move {
            debug!("Session keep-alive task started");

            // 创建会话 / Create session
            let session_future = session_creator(params);

            tokio::select! {
                result = session_future => {
                    match result {
                        Ok(_) => {
                            debug!("Session completed successfully");
                            { *state.write().await = ClientState::Disconnected; }
                        }
                        Err(e) => {
                            error!("Session failed: {}", e);
                            { *state.write().await = ClientState::Error; }
                        }
                    }
                }
                shutdown_rx = shutdown_rx.changed() => {
                    if shutdown_rx.is_ok() {
                        debug!("Session keep-alive task received shutdown signal");
                    }
                }
            }

            debug!("Session keep-alive task ended");
        });

        *self.keep_alive_handle.lock().await = Some(handle);
    }

    /// 停止会话保持任务 / Stop session keep-alive task
    async fn stop_keep_alive(&self) -> Result<(), ComputerError> {
        // 发送关闭信号 / Send shutdown signal
        let mut shutdown_tx = self.shutdown_tx.lock().await;
        if let Some(tx) = shutdown_tx.take() {
            let _ = tx.send(true);
        }

        // 等待任务结束 / Wait for task to end
        let mut handle = self.keep_alive_handle.lock().await;
        if let Some(h) = handle.take() {
            match h.await {
                Ok(_) => debug!("Keep-alive task stopped successfully"),
                Err(e) => warn!("Keep-alive task stopped with error: {}", e),
            }
        }

        // 重新创建关闭信号 / Recreate shutdown signal
        let (tx, _) = watch::channel(false);
        *shutdown_tx = Some(tx);

        Ok(())
    }

    /// 创建关闭信号接收器 / Create shutdown signal receiver
    #[allow(dead_code)]
    async fn create_shutdown_receiver(&self) -> watch::Receiver<bool> {
        let shutdown_tx = self.shutdown_tx.lock().await;
        shutdown_tx.as_ref().unwrap().subscribe()
    }

    /// 检查是否可以连接 / Check if can connect
    pub async fn can_connect(&self) -> bool {
        matches!(
            self.get_state().await,
            ClientState::Initialized | ClientState::Disconnected
        )
    }

    /// 检查是否可以断开 / Check if can disconnect
    pub async fn can_disconnect(&self) -> bool {
        matches!(self.get_state().await, ClientState::Connected)
    }

    /// 执行带超时的操作 / Execute operation with timeout
    #[allow(dead_code)]
    async fn execute_with_timeout<F, T>(
        &self,
        future: F,
        timeout_secs: u64,
    ) -> Result<T, MCPClientError>
    where
        F: std::future::Future<Output = Result<T, MCPClientError>>,
    {
        match timeout(Duration::from_secs(timeout_secs), future).await {
            Ok(result) => result,
            Err(_) => Err(MCPClientError::TimeoutError(format!(
                "Operation timed out after {} seconds",
                timeout_secs
            ))),
        }
    }
}

#[async_trait]
impl<P> MCPClientProtocol for BaseMCPClient<P>
where
    P: Send + Sync + Clone + 'static,
{
    fn state(&self) -> ClientState {
        // 使用 try_read 避免阻塞
        if let Ok(state_guard) = self.state.try_read() {
            *state_guard
        } else {
            // 如果锁被占用，返回一个默认值或尝试阻塞读取
            // 在测试环境中，我们通常可以假设锁不会被长时间占用
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async { self.get_state().await })
            })
        }
    }

    async fn connect(&self) -> Result<(), MCPClientError> {
        if !self.can_connect().await {
            return Err(MCPClientError::ConnectionError(format!(
                "Cannot connect in state: {}",
                self.get_state().await
            )));
        }

        self.update_state(ClientState::Connected).await;
        info!("Connected successfully");
        Ok(())
    }

    async fn disconnect(&self) -> Result<(), MCPClientError> {
        if !self.can_disconnect().await {
            return Err(MCPClientError::ConnectionError(format!(
                "Cannot disconnect in state: {}",
                self.get_state().await
            )));
        }

        self.stop_keep_alive()
            .await
            .map_err(|e| MCPClientError::Other(e.to_string()))?;
        self.update_state(ClientState::Disconnected).await;
        info!("Disconnected successfully");
        Ok(())
    }

    async fn list_tools(&self) -> Result<Vec<Tool>, MCPClientError> {
        if self.get_state().await != ClientState::Connected {
            return Err(MCPClientError::ConnectionError("Not connected".to_string()));
        }
        // 基础实现返回空列表，子类需要重写
        // Base implementation returns empty list, subclasses need to override
        Ok(vec![])
    }

    async fn call_tool(
        &self,
        _tool_name: &str,
        _params: serde_json::Value,
    ) -> Result<CallToolResult, MCPClientError> {
        if self.get_state().await != ClientState::Connected {
            return Err(MCPClientError::ConnectionError("Not connected".to_string()));
        }
        // 基础实现返回错误，子类需要重写
        // Base implementation returns error, subclasses need to override
        Err(MCPClientError::ProtocolError("Not implemented".to_string()))
    }

    async fn list_windows(&self) -> Result<Vec<Resource>, MCPClientError> {
        if self.get_state().await != ClientState::Connected {
            return Err(MCPClientError::ConnectionError("Not connected".to_string()));
        }
        // 基础实现返回空列表，子类需要重写
        // Base implementation returns empty list, subclasses need to override
        Ok(vec![])
    }

    async fn get_window_detail(
        &self,
        _resource: Resource,
    ) -> Result<ReadResourceResult, MCPClientError> {
        if self.get_state().await != ClientState::Connected {
            return Err(MCPClientError::ConnectionError("Not connected".to_string()));
        }
        // 基础实现返回错误，子类需要重写
        // Base implementation returns error, subclasses need to override
        Err(MCPClientError::ProtocolError("Not implemented".to_string()))
    }

    async fn subscribe_window(&self, _resource: Resource) -> Result<(), MCPClientError> {
        if self.get_state().await != ClientState::Connected {
            return Err(MCPClientError::ConnectionError("Not connected".to_string()));
        }
        // 基础实现返回错误，子类需要重写
        // Base implementation returns error, subclasses need to override
        Err(MCPClientError::ProtocolError("Not implemented".to_string()))
    }

    async fn unsubscribe_window(&self, _resource: Resource) -> Result<(), MCPClientError> {
        if self.get_state().await != ClientState::Connected {
            return Err(MCPClientError::ConnectionError("Not connected".to_string()));
        }
        // 基础实现返回错误，子类需要重写
        // Base implementation returns error, subclasses need to override
        Err(MCPClientError::ProtocolError("Not implemented".to_string()))
    }
}

/// 客户端状态机 / Client state machine
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateTransition {
    InitializeToConnected,
    ConnectedToDisconnected,
    AnyToError,
    ErrorToInitialized,
}

impl StateTransition {
    /// 检查状态转换是否有效 / Check if state transition is valid
    pub fn is_valid(from: ClientState, to: ClientState) -> bool {
        matches!(
            (from, to),
            (ClientState::Initialized, ClientState::Connected)
                | (ClientState::Connected, ClientState::Disconnected)
                | (_, ClientState::Error)
                | (ClientState::Error, ClientState::Initialized)
                | (ClientState::Disconnected, ClientState::Connected)
                | (ClientState::Disconnected, ClientState::Initialized)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::time::{sleep, Duration};

    #[tokio::test]
    async fn test_state_transition_validity() {
        assert!(StateTransition::is_valid(
            ClientState::Initialized,
            ClientState::Connected
        ));
        assert!(StateTransition::is_valid(
            ClientState::Connected,
            ClientState::Disconnected
        ));
        assert!(StateTransition::is_valid(
            ClientState::Connected,
            ClientState::Error
        ));
        assert!(StateTransition::is_valid(
            ClientState::Error,
            ClientState::Initialized
        ));
        assert!(!StateTransition::is_valid(
            ClientState::Connected,
            ClientState::Initialized
        ));
    }

    #[tokio::test]
    async fn test_base_client_state_management() {
        let client = BaseMCPClient::new("test");
        assert_eq!(client.get_state().await, ClientState::Initialized);

        // Test state change notification
        let mut rx = client.get_state_notifier();
        assert_eq!(*rx.borrow_and_update(), ClientState::Initialized);
    }

    #[tokio::test]
    async fn test_state_change_callback() {
        let mut client = BaseMCPClient::new("test");
        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_clone = call_count.clone();

        client.set_state_change_callback(move |from, to| {
            call_count_clone.fetch_add(1, Ordering::SeqCst);
            println!("State changed from {} to {}", from, to);
        });

        // 触发状态变化 / Trigger state change
        client.update_state(ClientState::Connected).await;
        assert_eq!(client.get_state().await, ClientState::Connected);
        assert_eq!(call_count.load(Ordering::SeqCst), 1);

        // 再次触发状态变化 / Trigger another state change
        client.update_state(ClientState::Disconnected).await;
        assert_eq!(client.get_state().await, ClientState::Disconnected);
        assert_eq!(call_count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_can_connect() {
        let client = BaseMCPClient::new("test");

        // 初始状态可以连接 / Can connect in initial state
        assert!(client.can_connect().await);

        // 连接后不能再次连接 / Cannot connect after connected
        client.update_state(ClientState::Connected).await;
        assert!(!client.can_connect().await);

        // 断开后可以重新连接 / Can reconnect after disconnect
        client.update_state(ClientState::Disconnected).await;
        assert!(client.can_connect().await);

        // 错误状态下不能连接 / Cannot connect in error state
        client.update_state(ClientState::Error).await;
        assert!(!client.can_connect().await);
    }

    #[tokio::test]
    async fn test_can_disconnect() {
        let client = BaseMCPClient::new("test");

        // 初始状态不能断开 / Cannot disconnect in initial state
        assert!(!client.can_disconnect().await);

        // 连接后可以断开 / Can disconnect after connected
        client.update_state(ClientState::Connected).await;
        assert!(client.can_disconnect().await);

        // 断开后不能再次断开 / Cannot disconnect after disconnected
        client.update_state(ClientState::Disconnected).await;
        assert!(!client.can_disconnect().await);
    }

    #[tokio::test]
    async fn test_create_shutdown_receiver() {
        let client = BaseMCPClient::new("test");

        // 创建关闭信号接收器 / Create shutdown signal receiver
        let mut rx = client.create_shutdown_receiver().await;

        // 初始值应该是 false / Initial value should be false
        assert!(!*rx.borrow_and_update());

        // 发送关闭信号 / Send shutdown signal
        {
            let shutdown_tx = client.shutdown_tx.lock().await;
            if let Some(tx) = shutdown_tx.as_ref() {
                let _ = tx.send(true);
            }
        }

        // 等待信号传播 / Wait for signal propagation
        sleep(Duration::from_millis(100)).await;
        assert!(rx.has_changed().unwrap_or(false));
    }

    #[tokio::test]
    async fn test_execute_with_timeout_success() {
        let client = BaseMCPClient::new("test");

        // 测试成功的操作 / Test successful operation
        let future = async {
            sleep(Duration::from_millis(100)).await;
            Ok::<String, MCPClientError>("success".to_string())
        };

        let result = client.execute_with_timeout(future, 1).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "success");
    }

    #[tokio::test]
    async fn test_execute_with_timeout_failure() {
        let client = BaseMCPClient::new("test");

        // 测试超时的操作 / Test timeout operation
        let future = async {
            sleep(Duration::from_secs(2)).await;
            Ok::<String, MCPClientError>("success".to_string())
        };

        let result = client.execute_with_timeout(future, 1).await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            MCPClientError::TimeoutError(_)
        ));
    }

    #[tokio::test]
    async fn test_start_keep_alive() {
        let client = BaseMCPClient::new("test");

        // 创建一个模拟的会话创建器 / Create a mock session creator
        let session_creator = |_params: &str| async {
            sleep(Duration::from_millis(100)).await;
            Ok::<(), MCPClientError>(())
        };

        // 启动会话保持任务 / Start keep-alive task
        client.start_keep_alive(session_creator).await;

        // 等待一小段时间让任务运行 / Wait a bit for task to run
        sleep(Duration::from_millis(50)).await;

        // 停止会话保持任务 / Stop keep-alive task
        client.stop_keep_alive().await.unwrap();
    }

    #[tokio::test]
    async fn test_start_keep_alive_with_error() {
        let client = BaseMCPClient::new("test");

        // 创建一个会失败的会话创建器 / Create a failing session creator
        let session_creator = |_params: &str| async {
            Err::<(), MCPClientError>(MCPClientError::ConnectionError(
                "Failed to create session".to_string(),
            ))
        };

        // 启动会话保持任务 / Start keep-alive task
        client.start_keep_alive(session_creator).await;

        // 等待任务完成 / Wait for task to complete
        sleep(Duration::from_millis(100)).await;

        // 检查状态是否变为错误 / Check if state changed to error
        assert_eq!(client.get_state().await, ClientState::Error);

        // 停止会话保持任务 / Stop keep-alive task
        client.stop_keep_alive().await.unwrap();
    }

    #[tokio::test]
    async fn test_protocol_connect_state_check() {
        let client = BaseMCPClient::new("test");

        // 在已连接状态下尝试连接应该失败 / Should fail if already connected
        client.update_state(ClientState::Connected).await;
        let result = client.connect().await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            MCPClientError::ConnectionError(_)
        ));
    }

    #[tokio::test]
    async fn test_protocol_disconnect_state_check() {
        let client = BaseMCPClient::new("test");

        // 在未连接状态下尝试断开应该失败 / Should fail if not connected
        let result = client.disconnect().await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            MCPClientError::ConnectionError(_)
        ));
    }

    #[tokio::test]
    async fn test_protocol_methods_require_connection() {
        let client = BaseMCPClient::new("test");

        // 所有方法都应该在未连接状态下失败 / All methods should fail when not connected
        assert!(client.list_tools().await.is_err());
        assert!(client
            .call_tool("test", serde_json::json!({}))
            .await
            .is_err());
        assert!(client.list_windows().await.is_err());
        assert!(client
            .get_window_detail(crate::mcp_clients::Resource {
                uri: "test://".to_string(),
                name: "test".to_string(),
                description: None,
                mime_type: None,
            })
            .await
            .is_err());
    }

    #[tokio::test]
    async fn test_multiple_state_change_listeners() {
        let client = BaseMCPClient::new("test");

        // 创建多个监听器 / Create multiple listeners
        let mut rx1 = client.get_state_notifier();
        let mut rx2 = client.get_state_notifier();
        let mut rx3 = client.get_state_notifier();

        // 更新状态 / Update state
        client.update_state(ClientState::Connected).await;

        // 所有监听器都应该收到通知 / All listeners should receive notification
        assert_eq!(*rx1.borrow_and_update(), ClientState::Connected);
        assert_eq!(*rx2.borrow_and_update(), ClientState::Connected);
        assert_eq!(*rx3.borrow_and_update(), ClientState::Connected);
    }

    #[tokio::test]
    async fn test_client_state_display() {
        assert_eq!(ClientState::Initialized.to_string(), "initialized");
        assert_eq!(ClientState::Connected.to_string(), "connected");
        assert_eq!(ClientState::Disconnected.to_string(), "disconnected");
        assert_eq!(ClientState::Error.to_string(), "error");
    }
}
