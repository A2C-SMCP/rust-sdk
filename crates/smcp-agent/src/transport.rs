/*!
* 文件名: transport
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: rust_socketio, tokio
* 描述: SMCP Agent传输层实现 / SMCP Agent transport layer implementation
*/

use crate::error::{Result, SmcpAgentError};
use futures_util::FutureExt;
use rust_socketio::{
    asynchronous::{Client, ClientBuilder},
    Event, Payload,
};
use serde_json::Value;
use smcp::events::*;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{oneshot, Mutex};
use tracing::{debug, error, info};

/// 事件处理器类型
pub type EventHandler = Box<dyn FnMut(Payload, Client) + Send + Sync>;

/// Socket.IO传输层
pub struct SocketIoTransport {
    client: Client,
    namespace: String,
}

impl SocketIoTransport {
    /// 创建新的传输层实例
    pub async fn connect(
        url: &str,
        namespace: &str,
        auth: Option<Value>,
        headers: HashMap<String, String>,
    ) -> Result<Self> {
        let mut builder = ClientBuilder::new(url);

        // 设置命名空间
        if !namespace.is_empty() {
            builder = builder.namespace(namespace);
        }

        // 设置认证信息
        if let Some(auth_data) = auth {
            builder = builder.auth(auth_data);
        }

        // 设置头部
        for (key, value) in headers {
            builder = builder.opening_header(key, value);
        }

        // 连接服务器
        let client = builder
            .connect()
            .await
            .map_err(|e| SmcpAgentError::connection(format!("Failed to connect: {}", e)))?;

        info!(
            "Connected to SMCP server at {} with namespace {}",
            url, namespace
        );

        Ok(Self {
            client,
            namespace: namespace.to_string(),
        })
    }

    /// 创建新的传输层实例并注册事件处理器
    pub async fn connect_with_handlers(
        url: &str,
        namespace: &str,
        auth: Option<Value>,
        headers: HashMap<String, String>,
        event_handler: Option<Arc<dyn crate::events::AsyncAgentEventHandler>>,
        config: crate::config::SmcpAgentConfig,
        tools_cache: Arc<tokio::sync::RwLock<HashMap<String, Vec<smcp::SMCPTool>>>>,
    ) -> Result<Self> {
        let mut builder = ClientBuilder::new(url);

        // 注册on_any处理器来捕获所有事件
        let handler_clone = event_handler.clone();
        let config_clone = config.clone();
        let tools_cache_clone = tools_cache.clone();

        builder = builder.on_any(move |event, payload, _client| {
            let event_str = match event {
                Event::Custom(s) => s,
                _ => return Box::pin(async {}),
            };

            // 只处理notify事件
            if !event_str.starts_with("notify:") {
                return Box::pin(async {});
            }

            let handler = handler_clone.clone();
            let config = config_clone.clone();
            let _tools_cache = tools_cache_clone.clone();

            Box::pin(async move {
                match event_str.as_str() {
                    NOTIFY_ENTER_OFFICE => {
                        if let Payload::Text(values, _) = payload {
                            if let Some(value) = values.into_iter().next() {
                                if let Ok(notification) =
                                    serde_json::from_value::<smcp::EnterOfficeNotification>(value)
                                {
                                    info!("Computer entered office: {:?}", notification);

                                    // 调用事件处理器
                                    if let Some(ref h) = handler {
                                        // 创建一个临时的agent实例用于调用处理器
                                        // 注意：这里简化处理，实际需要传递正确的agent引用
                                        let _ = h
                                            .on_computer_enter_office(
                                                notification,
                                                &crate::AsyncSmcpAgent::new(
                                                    crate::auth::DefaultAuthProvider::new(
                                                        "dummy".to_string(),
                                                        "dummy".to_string(),
                                                    ),
                                                    config,
                                                ),
                                            )
                                            .await;
                                    }
                                }
                            }
                        }
                    }
                    NOTIFY_LEAVE_OFFICE => {
                        if let Payload::Text(values, _) = payload {
                            if let Some(value) = values.into_iter().next() {
                                if let Ok(notification) =
                                    serde_json::from_value::<smcp::LeaveOfficeNotification>(value)
                                {
                                    info!("Computer left office: {:?}", notification);
                                }
                            }
                        }
                    }
                    NOTIFY_UPDATE_CONFIG => {
                        if let Payload::Text(values, _) = payload {
                            if let Some(value) = values.into_iter().next() {
                                if let Ok(notification) = serde_json::from_value::<
                                    smcp::UpdateMCPConfigNotification,
                                >(value)
                                {
                                    info!("Computer updated config: {:?}", notification);
                                }
                            }
                        }
                    }
                    NOTIFY_UPDATE_DESKTOP => {
                        if let Payload::Text(values, _) = payload {
                            if let Some(value) = values.into_iter().next() {
                                if let Ok(notification) =
                                    serde_json::from_value::<serde_json::Value>(value)
                                {
                                    if let Some(computer) =
                                        notification.get("computer").and_then(|v| v.as_str())
                                    {
                                        info!(
                                            "Desktop update notification for computer: {}",
                                            computer
                                        );
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            })
        });

        // 设置命名空间
        if !namespace.is_empty() {
            builder = builder.namespace(namespace);
        }

        // 设置认证信息
        if let Some(auth_data) = auth {
            builder = builder.auth(auth_data);
        }

        // 设置头部
        for (key, value) in headers {
            builder = builder.opening_header(key, value);
        }

        // 连接服务器
        let client = builder
            .connect()
            .await
            .map_err(|e| SmcpAgentError::connection(format!("Failed to connect: {}", e)))?;

        info!(
            "Connected to SMCP server at {} with namespace {} and handlers",
            url, namespace
        );

        Ok(Self {
            client,
            namespace: namespace.to_string(),
        })
    }

    /// 发送事件（不等待响应）
    pub async fn emit(&self, event: &str, data: Value) -> Result<()> {
        debug!("Emitting event: {}", event);

        self.client
            .emit(event, Payload::from(vec![data]))
            .await
            .map_err(SmcpAgentError::from)
    }

    /// 发送事件并等待响应
    pub async fn call(&self, event: &str, data: Value, timeout_secs: u64) -> Result<Value> {
        debug!("Calling event: {} with timeout {}s", event, timeout_secs);

        let (tx, rx) = oneshot::channel();
        let tx = Arc::new(Mutex::new(Some(tx)));

        let callback = move |payload: Payload, _client: Client| {
            if let Some(tx_opt) = tx.try_lock().ok().and_then(|mut m| m.take()) {
                let _ = tx_opt.send(payload);
            }
            async {}.boxed()
        };

        self.client
            .emit_with_ack(
                event,
                Payload::from(vec![data]),
                Duration::from_secs(timeout_secs),
                callback,
            )
            .await?;

        match rx.await {
            Ok(response) => {
                // 从响应中提取JSON数据
                match response {
                    Payload::Text(values, _) => {
                        if let Some(value) = values.into_iter().next() {
                            Ok(value)
                        } else {
                            Err(SmcpAgentError::internal("Empty response"))
                        }
                    }
                    #[allow(deprecated)]
                    Payload::String(s, _) => {
                        // 尝试解析字符串为JSON
                        serde_json::from_str(&s).map_err(SmcpAgentError::from)
                    }
                    Payload::Binary(_, _) => {
                        Err(SmcpAgentError::internal("Binary response not supported"))
                    }
                }
            }
            Err(_) => {
                error!("Timeout while calling event: {}", event);
                Err(SmcpAgentError::Timeout)
            }
        }
    }

    /// 断开连接
    pub async fn disconnect(self) -> Result<()> {
        debug!("Disconnecting from server");
        self.client.disconnect().await.map_err(SmcpAgentError::from)
    }

    /// 获取当前连接的命名空间
    pub fn namespace(&self) -> &str {
        &self.namespace
    }
}

impl Default for SocketIoTransport {
    fn default() -> Self {
        // 创建一个未连接的占位符
        // 注意：这实际上不能使用，因为Client::new()需要参数
        // 这里只是为了满足Default trait的要求
        panic!("SocketIoTransport must be created via connect() method");
    }
}
