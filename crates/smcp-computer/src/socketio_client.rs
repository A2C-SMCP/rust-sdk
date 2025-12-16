/*!
* 文件名: socketio_client
* 作者: JQQ
* 创建日期: 2025/12/16
* 最后修改日期: 2025/12/16
* 版权: 2023 JQQ. All rights reserved.
* 依赖: rust_socketio, tokio, serde
* 描述: SMCP Computer的Socket.IO客户端实现 / Socket.IO client implementation for SMCP Computer
*/

use crate::errors::{ComputerError, ComputerResult};
use crate::mcp_clients::manager::MCPServerManager;
use futures_util::FutureExt;
use rust_socketio::{
    asynchronous::{Client, ClientBuilder},
    Event, Payload, TransportType,
};
use serde_json::Value;
use smcp::{
    SMCP_NAMESPACE,
    events::{
        CLIENT_GET_CONFIG, CLIENT_GET_DESKTOP, CLIENT_GET_TOOLS, 
        SERVER_JOIN_OFFICE, SERVER_LEAVE_OFFICE, 
        CLIENT_TOOL_CALL, SERVER_UPDATE_CONFIG,
        SERVER_UPDATE_DESKTOP, SERVER_UPDATE_TOOL_LIST,
    },
    GetComputerConfigReq, GetComputerConfigRet, GetDesktopReq, GetDesktopRet,
    GetToolsReq, GetToolsRet, ToolCallReq,
};
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, error, info};

/// SMCP Computer Socket.IO客户端
/// SMCP Computer Socket.IO client
pub struct SmcpComputerClient {
    /// Socket.IO客户端实例 / Socket.IO client instance
    client: Client,
    /// Computer名称 / Computer name
    computer_name: String,
    /// 当前所在的office ID / Current office ID
    office_id: Arc<RwLock<Option<String>>>,
}

impl SmcpComputerClient {
    /// 创建新的Socket.IO客户端
    /// Create a new Socket.IO client
    pub async fn new(
        url: &str,
        manager: Arc<Mutex<MCPServerManager>>,
        computer_name: String,
    ) -> ComputerResult<Self> {
        let office_id = Arc::new(RwLock::new(None));
        let manager_clone = manager.clone();
        let computer_name_clone = computer_name.clone();
        let office_id_clone = office_id.clone();

        // 使用ClientBuilder注册事件处理器
        // Use ClientBuilder to register event handlers
        let client = ClientBuilder::new(url)
            .namespace(SMCP_NAMESPACE)
            .transport_type(TransportType::Websocket)
            .on_any(move |event, payload, client| {
                // 只处理自定义事件
                // Only handle custom events
                let event_str = match event {
                    Event::Custom(s) => s,
                    _ => return async {}.boxed(),
                };

                match event_str.as_str() {
                    CLIENT_TOOL_CALL => {
                        let manager = manager_clone.clone();
                        let computer_name = computer_name_clone.clone();
                        let office_id = office_id_clone.clone();
                        let client_clone = client.clone();
                        let payload_clone = payload.clone();
                        
                        async move {
                            match Self::handle_tool_call_with_ack(payload, manager, computer_name, office_id, client_clone).await {
                                Ok((ack_id, response)) => {
                                    if let Some(id) = ack_id {
                                        if let Err(e) = client.ack_with_id(id, response).await {
                                            error!("Failed to send ack: {}", e);
                                        }
                                    }
                                }
                                Err(e) => {
                                    error!("Error handling tool call: {}", e);
                                    // 尝试返回错误响应 / Try to return error response
                                    if let Ok((ack_id, _)) = Self::extract_ack_id(payload_clone) {
                                        if let Some(id) = ack_id {
                                            let error_response = serde_json::json!({
                                                "isError": true,
                                                "content": [],
                                                "structuredContent": {
                                                    "error": e.to_string(),
                                                    "error_type": "ComputerError"
                                                }
                                            });
                                            let _ = client.ack_with_id(id, error_response).await;
                                        }
                                    }
                                }
                            }
                        }.boxed()
                    }
                    CLIENT_GET_TOOLS => {
                        let manager = manager_clone.clone();
                        let computer_name = computer_name_clone.clone();
                        let office_id = office_id_clone.clone();
                        let client_clone = client.clone();
                        
                        async move {
                            match Self::handle_get_tools_with_ack(payload, manager, computer_name, office_id, client_clone).await {
                                Ok((ack_id, response)) => {
                                    if let Some(id) = ack_id {
                                        if let Err(e) = client.ack_with_id(id, response).await {
                                            error!("Failed to send ack: {}", e);
                                        }
                                    }
                                }
                                Err(e) => {
                                    error!("Error handling get tools: {}", e);
                                }
                            }
                        }.boxed()
                    }
                    CLIENT_GET_CONFIG => {
                        let manager = manager_clone.clone();
                        let computer_name = computer_name_clone.clone();
                        let office_id = office_id_clone.clone();
                        let client_clone = client.clone();
                        
                        async move {
                            match Self::handle_get_config_with_ack(payload, manager, computer_name, office_id, client_clone).await {
                                Ok((ack_id, response)) => {
                                    if let Some(id) = ack_id {
                                        if let Err(e) = client.ack_with_id(id, response).await {
                                            error!("Failed to send ack: {}", e);
                                        }
                                    }
                                }
                                Err(e) => {
                                    error!("Error handling get config: {}", e);
                                }
                            }
                        }.boxed()
                    }
                    CLIENT_GET_DESKTOP => {
                        let manager = manager_clone.clone();
                        let computer_name = computer_name_clone.clone();
                        let office_id = office_id_clone.clone();
                        let client_clone = client.clone();
                        
                        async move {
                            match Self::handle_get_desktop_with_ack(payload, manager, computer_name, office_id, client_clone).await {
                                Ok((ack_id, response)) => {
                                    if let Some(id) = ack_id {
                                        if let Err(e) = client.ack_with_id(id, response).await {
                                            error!("Failed to send ack: {}", e);
                                        }
                                    }
                                }
                                Err(e) => {
                                    error!("Error handling get desktop: {}", e);
                                }
                            }
                        }.boxed()
                    }
                    _ => {
                        debug!("Unhandled event: {}", event_str);
                        async {}.boxed()
                    }
                }
            })
            .connect()
            .await
            .map_err(|e| ComputerError::SocketIoError(format!("Failed to connect: {}", e)))?;

        info!(
            "Connected to SMCP server at {} with computer name: {}",
            url, computer_name
        );

        Ok(Self {
            client,
            computer_name,
            office_id,
        })
    }

    /// 加入Office（Socket.IO Room）
    /// Join an Office (Socket.IO Room)
    pub async fn join_office(&self, office_id: &str) -> ComputerResult<()> {
        debug!("Joining office: {}", office_id);
        
        // 先设置office_id
        // Set office_id first
        *self.office_id.write().await = Some(office_id.to_string());

        let req_data = serde_json::json!({
            "office_id": office_id,
            "role": "computer",
            "name": self.computer_name
        });

        // 使用call方法等待服务器响应
        // Use call method to wait for server response
        match self.call(SERVER_JOIN_OFFICE, req_data, Some(10)).await {
            Ok(response) => {
                // 服务器返回的是 (bool, Option<String>) 元组序列化后的数组
                // Server returns serialized array of (bool, Option<String>) tuple
                debug!("Join office response: {:?}", response);
                
                // 检查响应是否包含嵌套数组
                // Check if response contains nested array
                let actual_response = if response.len() == 1 {
                    if let Some(arr) = response.get(0).and_then(|v| v.as_array()) {
                        arr.to_vec()
                    } else {
                        response
                    }
                } else {
                    response
                };
                
                if actual_response.len() >= 1 {
                    if let Some(success) = actual_response.get(0).and_then(|v| v.as_bool()) {
                        if success {
                            info!("Successfully joined office: {}", office_id);
                            Ok(())
                        } else {
                            // 加入失败，重置office_id / Reset office_id on failure
                            *self.office_id.write().await = None;
                            let error_msg = actual_response.get(1)
                                .and_then(|v| v.as_str())
                                .unwrap_or("Unknown error");
                            Err(ComputerError::SocketIoError(format!("Failed to join office: {}", error_msg)))
                        }
                    } else {
                        *self.office_id.write().await = None;
                        Err(ComputerError::SocketIoError(format!("Invalid response format from server: {:?}", actual_response)))
                    }
                } else {
                    *self.office_id.write().await = None;
                    Err(ComputerError::SocketIoError("Empty response from server".to_string()))
                }
            }
            Err(e) => {
                *self.office_id.write().await = None;
                Err(e)
            }
        }
    }

    /// 离开Office
    /// Leave an Office
    pub async fn leave_office(&self, office_id: &str) -> ComputerResult<()> {
        debug!("Leaving office: {}", office_id);
        
        let req_data = serde_json::json!({
            "office_id": office_id
        });

        self.emit(SERVER_LEAVE_OFFICE, req_data).await?;
        *self.office_id.write().await = None;
        
        info!("Left office: {}", office_id);
        Ok(())
    }

    /// 发送配置更新通知
    /// Emit config update notification
    pub async fn emit_update_config(&self) -> ComputerResult<()> {
        let office_id = self.office_id.read().await;
        if office_id.is_some() {
            let req_data = serde_json::json!({
                "computer": self.computer_name
            });
            self.emit(SERVER_UPDATE_CONFIG, req_data).await?;
            info!("Emitted config update notification");
        }
        Ok(())
    }

    /// 发送工具列表更新通知
    /// Emit tool list update notification
    pub async fn emit_update_tool_list(&self) -> ComputerResult<()> {
        let office_id = self.office_id.read().await;
        if office_id.is_some() {
            let req_data = serde_json::json!({
                "computer": self.computer_name
            });
            self.emit(SERVER_UPDATE_TOOL_LIST, req_data).await?;
            info!("Emitted tool list update notification");
        }
        Ok(())
    }

    /// 发送桌面更新通知
    /// Emit desktop update notification
    pub async fn emit_update_desktop(&self) -> ComputerResult<()> {
        let office_id = self.office_id.read().await;
        if office_id.is_some() {
            let req_data = serde_json::json!({
                "computer": self.computer_name
            });
            self.emit(SERVER_UPDATE_DESKTOP, req_data).await?;
            info!("Emitted desktop update notification");
        }
        Ok(())
    }

    /// 发送事件（不等待响应）
    /// Emit event without waiting for response
    async fn emit(&self, event: &str, data: Value) -> ComputerResult<()> {
        debug!("Emitting event: {}", event);
        
        self.client
            .emit(event, Payload::Text(vec![data], None))
            .await
            .map_err(|e| ComputerError::SocketIoError(format!("Failed to emit {}: {}", event, e)))
    }

    /// 发送事件并等待响应
    /// Emit event and wait for response
    async fn call(&self, event: &str, data: Value, timeout_secs: Option<u64>) -> ComputerResult<Vec<Value>> {
        let timeout = std::time::Duration::from_secs(timeout_secs.unwrap_or(30));
        debug!("Calling event: {} with timeout {:?}", event, timeout);
        
        let (tx, rx) = tokio::sync::oneshot::channel();
        let tx = Arc::new(std::sync::Mutex::new(Some(tx)));

        let callback = move |payload: Payload, _client: Client| {
            if let Some(tx_opt) = tx.try_lock().ok().and_then(|mut m| m.take()) {
                let _ = tx_opt.send(payload);
            }
            async {}.boxed()
        };

        self.client
            .emit_with_ack(
                event,
                Payload::Text(vec![data], None),
                timeout,
                callback,
            )
            .await
            .map_err(|e| ComputerError::SocketIoError(format!("Failed to call {}: {}", event, e)))?;

        // 使用 tokio::time::timeout 来确保 rx.await 不会无限期等待
        // Use tokio::time::timeout to ensure rx.await doesn't wait forever
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(response)) => {
                // 从响应中提取JSON数据 / Extract JSON data from response
                match response {
                    Payload::Text(values, _) => {
                        debug!("Received response: {:?}", values);
                        Ok(values)
                    },
                    #[allow(deprecated)]
                    Payload::String(s, _) => {
                        // 尝试解析字符串为JSON数组
                        // Try to parse string as JSON array
                        let parsed: Vec<Value> = serde_json::from_str(&s)
                            .map_err(|e| ComputerError::SocketIoError(format!("Failed to parse response: {}", e)))?;
                        debug!("Received parsed response: {:?}", parsed);
                        Ok(parsed)
                    }
                    Payload::Binary(_, _) => {
                        Err(ComputerError::SocketIoError("Binary response not supported".to_string()))
                    }
                }
            }
            Ok(Err(_)) => {
                error!("Channel closed while calling event: {}", event);
                Err(ComputerError::SocketIoError("Channel closed while waiting for response".to_string()))
            }
            Err(_) => {
                error!("Timeout while calling event: {}", event);
                Err(ComputerError::SocketIoError("Timeout while waiting for response".to_string()))
            }
        }
    }

    /// 处理工具调用事件（带ACK响应）
    /// Handle tool call event (with ACK response)
    async fn handle_tool_call_with_ack(
        payload: Payload,
        manager: Arc<Mutex<MCPServerManager>>,
        computer_name: String,
        office_id: Arc<RwLock<Option<String>>>,
        _client: Client,
    ) -> ComputerResult<(Option<i32>, Value)> {
        let (ack_id, req) = Self::extract_ack_and_parse::<ToolCallReq>(payload)?;
        
        // 验证office_id和computer_name
        // Validate office_id and computer_name
        let current_office_id = office_id.read().await;
        if current_office_id.as_ref() != Some(&req.base.agent) {
            return Err(ComputerError::ValidationError(format!(
                "Office ID mismatch: expected {:?}, got {}",
                current_office_id, req.base.agent
            )));
        }
        if computer_name != req.computer {
            return Err(ComputerError::ValidationError(format!(
                "Computer name mismatch: expected {}, got {}",
                computer_name, req.computer
            )));
        }

        // 执行工具调用 / Execute tool call
        let result = {
            let manager = manager.lock().await;
            manager.execute_tool(
                &req.tool_name,
                req.params,
                Some(std::time::Duration::from_secs(req.timeout as u64)),
            ).await?
        };

        let result_value = serde_json::to_value(result)
            .map_err(|e| ComputerError::SerializationError(e))?;

        info!("Tool call executed successfully: {}", req.tool_name);
        Ok((ack_id, result_value))
    }

    /// 处理获取工具列表事件（带ACK响应）
    /// Handle get tools event (with ACK response)
    async fn handle_get_tools_with_ack(
        payload: Payload,
        manager: Arc<Mutex<MCPServerManager>>,
        computer_name: String,
        office_id: Arc<RwLock<Option<String>>>,
        _client: Client,
    ) -> ComputerResult<(Option<i32>, Value)> {
        let (ack_id, req) = Self::extract_ack_and_parse::<GetToolsReq>(payload)?;
        
        // 验证office_id和computer_name
        // Validate office_id and computer_name
        let current_office_id = office_id.read().await;
        if current_office_id.as_ref() != Some(&req.base.agent) {
            return Err(ComputerError::ValidationError(format!(
                "Office ID mismatch: expected {:?}, got {}",
                current_office_id, req.base.agent
            )));
        }
        if computer_name != req.computer {
            return Err(ComputerError::ValidationError(format!(
                "Computer name mismatch: expected {}, got {}",
                computer_name, req.computer
            )));
        }

        // 获取工具列表 / Get tools list
        let tools: Vec<smcp::SMCPTool> = {
            let manager = manager.lock().await;
            // 转换Tool为SMCPTool
            // Convert Tool to SMCPTool
            let tool_list = manager.list_available_tools().await;
            tool_list.into_iter().map(|tool| smcp::SMCPTool {
                name: tool.name,
                description: tool.description,
                params_schema: tool.input_schema,
                return_schema: None,
                meta: None,
            }).collect()
        };

        let response = GetToolsRet {
            tools: tools.clone(),
            req_id: req.base.req_id,
        };

        info!("Returned {} tools for agent {}", tools.len(), req.base.agent);
        Ok((ack_id, serde_json::to_value(response)?))
    }

    /// 处理获取配置事件（带ACK响应）
    /// Handle get config event (with ACK response)
    async fn handle_get_config_with_ack(
        payload: Payload,
        manager: Arc<Mutex<MCPServerManager>>,
        computer_name: String,
        office_id: Arc<RwLock<Option<String>>>,
        _client: Client,
    ) -> ComputerResult<(Option<i32>, Value)> {
        let (ack_id, req) = Self::extract_ack_and_parse::<GetComputerConfigReq>(payload)?;
        
        // 验证office_id和computer_name
        // Validate office_id and computer_name
        let current_office_id = office_id.read().await;
        if current_office_id.as_ref() != Some(&req.base.agent) {
            return Err(ComputerError::ValidationError(format!(
                "Office ID mismatch: expected {:?}, got {}",
                current_office_id, req.base.agent
            )));
        }
        if computer_name != req.computer {
            return Err(ComputerError::ValidationError(format!(
                "Computer name mismatch: expected {}, got {}",
                computer_name, req.computer
            )));
        }

        // 获取配置 / Get config
        let servers = {
            let manager = manager.lock().await;
            // 获取服务器状态并转换为配置格式
            // Get server status and convert to config format
            let status = manager.get_server_status().await;
            serde_json::json!(status)
        };
        let inputs = None; // 暂时返回None / Return None for now

        let response = GetComputerConfigRet {
            servers,
            inputs,
        };

        info!("Returned config for agent {}", req.base.agent);
        Ok((ack_id, serde_json::to_value(response)?))
    }

    /// 处理获取桌面事件（带ACK响应）
    /// Handle get desktop event (with ACK response)
    async fn handle_get_desktop_with_ack(
        payload: Payload,
        _manager: Arc<Mutex<MCPServerManager>>,
        computer_name: String,
        office_id: Arc<RwLock<Option<String>>>,
        _client: Client,
    ) -> ComputerResult<(Option<i32>, Value)> {
        let (ack_id, req) = Self::extract_ack_and_parse::<GetDesktopReq>(payload)?;
        
        // 验证office_id和computer_name
        // Validate office_id and computer_name
        let current_office_id = office_id.read().await;
        if current_office_id.as_ref() != Some(&req.base.agent) {
            return Err(ComputerError::ValidationError(format!(
                "Office ID mismatch: expected {:?}, got {}",
                current_office_id, req.base.agent
            )));
        }
        if computer_name != req.computer {
            return Err(ComputerError::ValidationError(format!(
                "Computer name mismatch: expected {}, got {}",
                computer_name, req.computer
            )));
        }

        // 获取桌面 / Get desktop
        // TODO: 实现实际的桌面捕获逻辑
        // TODO: Implement actual desktop capture logic
        let desktops = Vec::<String>::new(); // 暂时返回空列表 / Return empty list for now

        let response = GetDesktopRet {
            desktops: Some(desktops),
            req_id: req.base.req_id,
        };

        info!("Returned desktop for agent {}", req.base.agent);
        Ok((ack_id, serde_json::to_value(response)?))
    }

    /// 从payload中提取ack_id并解析数据
    /// Extract ack_id from payload and parse data
    fn extract_ack_and_parse<T: serde::de::DeserializeOwned>(
        payload: Payload,
    ) -> ComputerResult<(Option<i32>, T)> {
        match payload {
            Payload::Text(mut values, ack_id) => {
                if let Some(value) = values.pop() {
                    let req = serde_json::from_value(value)
                        .map_err(|e| ComputerError::SerializationError(e))?;
                    Ok((ack_id, req))
                } else {
                    Err(ComputerError::ProtocolError("Empty payload".to_string()))
                }
            }
            #[allow(deprecated)]
            Payload::String(s, ack_id) => {
                let req = serde_json::from_str(&s)
                    .map_err(|e| ComputerError::SerializationError(e))?;
                Ok((ack_id, req))
            }
            Payload::Binary(_, _) => {
                Err(ComputerError::SocketIoError("Binary payload not supported".to_string()))
            }
        }
    }

    /// 仅提取ack_id（用于错误处理）
    /// Extract ack_id only (for error handling)
    fn extract_ack_id(payload: Payload) -> ComputerResult<(Option<i32>, ())> {
        match payload {
            Payload::Text(_, ack_id) => Ok((ack_id, ())),
            #[allow(deprecated)]
            Payload::String(_, ack_id) => Ok((ack_id, ())),
            Payload::Binary(_, _) => Ok((None, ())),
        }
    }

    /// 断开连接
    /// Disconnect from server
    pub async fn disconnect(self) -> ComputerResult<()> {
        debug!("Disconnecting from server");
        self.client
            .disconnect()
            .await
            .map_err(|e| ComputerError::SocketIoError(format!("Failed to disconnect: {}", e)))?;
        info!("Disconnected from server");
        Ok(())
    }

    /// 获取当前office ID
    /// Get current office ID
    pub async fn get_office_id(&self) -> Option<String> {
        self.office_id.read().await.clone()
    }
}
