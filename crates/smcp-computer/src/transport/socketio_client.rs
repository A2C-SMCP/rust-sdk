/*!
* 文件名: socketio_client.rs
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: rust_socketio, serde_json
* 描述: SMCP Computer Socket.IO客户端 / SMCP Computer Socket.IO client
*/

use crate::core::ComputerCore;
use crate::errors::{ComputerError, ComputerResult};
use rust_socketio::asynchronous::{ClientBuilder, Client};
use rust_socketio::Payload;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Weak};
use tokio::sync::{Mutex, oneshot};
use std::time::Duration;
use tokio::time::timeout;
use futures_util::FutureExt;

/// SMCP命名空间 / SMCP namespace
pub const SMCP_NAMESPACE: &str = "/smcp";

/// Socket.IO事件常量 / Socket.IO event constants
pub const TOOL_CALL_EVENT: &str = "client:tool_call";
pub const GET_TOOLS_EVENT: &str = "client:get_tools";
pub const GET_CONFIG_EVENT: &str = "client:get_config";
pub const GET_DESKTOP_EVENT: &str = "client:get_desktop";
pub const UPDATE_TOOL_LIST_EVENT: &str = "client:update_tool_list";
pub const UPDATE_CONFIG_EVENT: &str = "client:update_config";
pub const UPDATE_DESKTOP_EVENT: &str = "client:update_desktop";
pub const JOIN_OFFICE_EVENT: &str = "client:join_office";
pub const LEAVE_OFFICE_EVENT: &str = "client:leave_office";

/// SMCP Computer客户端 / SMCP Computer client
pub struct SmcpComputerClient {
    /// Socket.IO客户端 / Socket.IO client
    client: Arc<Mutex<Client>>,
    /// Computer核心的弱引用 / Weak reference to Computer core
    computer: Weak<ComputerCore>,
    /// 当前办公室ID / Current office ID
    office_id: Arc<Mutex<Option<String>>>,
    /// 响应发送器集合 / Response senders
    response_senders: Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>,
}

impl SmcpComputerClient {
    /// 创建新的客户端 / Create new client
    pub async fn new(url: &str, computer: Arc<ComputerCore>) -> ComputerResult<Self> {
        let computer_weak = Arc::downgrade(&computer);
        let office_id_clone = Arc::new(Mutex::new(None));
        
        // 工具调用事件处理器
        let computer_weak_handler = computer_weak.clone();
        let office_id_clone_handler = office_id_clone.clone();
        let tool_call_handler = move |payload: Payload, client: Client| {
            let computer_weak = computer_weak_handler.clone();
            let office_id_clone = office_id_clone_handler.clone();
            async move {
                if let Payload::Text(values, _) = payload {
                    if let Some(data) = values.first() {
                        if let Ok(response) = handle_tool_call(data.clone(), computer_weak, office_id_clone).await {
                            // 发送响应
                            let _ = client.emit("tool_call_response", Payload::Text(vec![response], None)).await;
                        }
                    }
                }
            }.boxed()
        };
        
        let client = ClientBuilder::new(url)
            .namespace(SMCP_NAMESPACE)
            .on(TOOL_CALL_EVENT, tool_call_handler)
            .connect()
            .await
            .map_err(|e| ComputerError::ConnectionError(e.to_string()))?;
        
        Ok(Self {
            client: Arc::new(Mutex::new(client)),
            computer: computer_weak,
            office_id: office_id_clone,
            response_senders: Arc::new(Mutex::new(HashMap::new())),
        })
    }
    
    /// 连接到服务器 / Connect to server
    pub async fn connect(&self) -> ComputerResult<()> {
        // Client is already connected via builder.connect()
        Ok(())
    }
    
    /// 断开连接 / Disconnect
    pub async fn disconnect(&self) -> ComputerResult<()> {
        let client = self.client.lock().await;
        client.disconnect().await
            .map_err(|e| ComputerError::ConnectionError(e.to_string()))?;
        Ok(())
    }
    
    /// 加入办公室 / Join office
    pub async fn join_office(&self, office_id: &str, computer_name: &str) -> ComputerResult<()> {
        *self.office_id.lock().await = Some(office_id.to_string());
        
        let req = json!({
            "office_id": office_id,
            "role": "computer",
            "name": computer_name
        });
        
        // 使用onshot channel来等待响应
        let (tx, rx) = oneshot::channel();
        let req_id = uuid::Uuid::new_v4().to_string();
        
        // 保存发送器
        {
            let mut senders = self.response_senders.lock().await;
            senders.insert(req_id.clone(), tx);
        }
        
        // 定义回调函数
        let response_senders = self.response_senders.clone();
        let req_id_clone = req_id.clone();
        let callback = move |payload: Payload, _client: Client| {
            let senders = response_senders.clone();
            let id = req_id_clone.clone();
            async move {
                if let Payload::Text(values, _) = payload {
                    if let Some(response) = values.first() {
                        let mut senders = senders.lock().await;
                        if let Some(tx) = senders.remove(&id) {
                            let _ = tx.send(response.clone());
                        }
                    }
                }
            }.boxed()
        };
        
        // 发送带ACK的请求
        let client = self.client.lock().await;
        client.emit_with_ack(
            JOIN_OFFICE_EVENT,
            Payload::Text(vec![req], None),
            Duration::from_secs(10),
            callback
        ).await
            .map_err(|e| ComputerError::TransportError(e.to_string()))?;
        
        // 等待响应
        match timeout(Duration::from_secs(10), rx).await {
            Ok(Ok(response)) => {
                // 检查响应
                if let Value::Array(arr) = response {
                    if arr.len() >= 2 {
                        if let Value::Bool(success) = &arr[0] {
                            if !success {
                                *self.office_id.lock().await = None;
                                let error = arr[1].as_str().unwrap_or("未知错误");
                                return Err(ComputerError::RuntimeError(format!("加入办公室失败: {}", error)));
                            }
                        }
                    }
                }
                Ok(())
            }
            Ok(Err(_)) => Err(ComputerError::RuntimeError("加入办公室失败：响应通道关闭".to_string())),
            Err(_) => {
                *self.office_id.lock().await = None;
                Err(ComputerError::RuntimeError("加入办公室失败：超时".to_string()))
            }
        }
    }
    
    /// 离开办公室 / Leave office
    pub async fn leave_office(&self, office_id: &str) -> ComputerResult<()> {
        let req = json!({
            "office_id": office_id
        });
        
        let client = self.client.lock().await;
        client.emit(LEAVE_OFFICE_EVENT, Payload::Text(vec![req], None))
            .await
            .map_err(|e| ComputerError::TransportError(e.to_string()))?;
        
        *self.office_id.lock().await = None;
        Ok(())
    }
    
    /// 发射更新工具列表事件 / Emit update tool list event
    pub async fn emit_update_tool_list(&self) -> ComputerResult<()> {
        let office_id = self.office_id.lock().await;
        if let Some(_office_id) = office_id.as_ref() {
            if let Some(computer) = self.computer.upgrade() {
                let req = json!({
                    "computer": computer.name()
                });
                
                let client = self.client.lock().await;
                client.emit(UPDATE_TOOL_LIST_EVENT, Payload::Text(vec![req], None))
                    .await
                    .map_err(|e| ComputerError::TransportError(e.to_string()))?;
            }
        }
        Ok(())
    }
    
    /// 发射更新配置事件 / Emit update config event
    pub async fn emit_update_config(&self) -> ComputerResult<()> {
        let office_id = self.office_id.lock().await;
        if let Some(_office_id) = office_id.as_ref() {
            if let Some(computer) = self.computer.upgrade() {
                let req = json!({
                    "computer": computer.name()
                });
                
                let client = self.client.lock().await;
                client.emit(UPDATE_CONFIG_EVENT, Payload::Text(vec![req], None))
                    .await
                    .map_err(|e| ComputerError::TransportError(e.to_string()))?;
            }
        }
        Ok(())
    }
    
    /// 发射刷新桌面事件 / Emit refresh desktop event
    pub async fn emit_refresh_desktop(&self) -> ComputerResult<()> {
        let office_id = self.office_id.lock().await;
        if let Some(_office_id) = office_id.as_ref() {
            if let Some(computer) = self.computer.upgrade() {
                let req = json!({
                    "computer": computer.name()
                });
                
                let client = self.client.lock().await;
                client.emit(UPDATE_DESKTOP_EVENT, Payload::Text(vec![req], None))
                    .await
                    .map_err(|e| ComputerError::TransportError(e.to_string()))?;
            }
        }
        Ok(())
    }
    
}

/// 处理工具调用 / Handle tool call
async fn handle_tool_call(
    data: Value,
    computer: Weak<ComputerCore>,
    _office_id: Arc<Mutex<Option<String>>>,
) -> ComputerResult<Value> {
    let computer = computer.upgrade()
        .ok_or_else(|| ComputerError::RuntimeError("Computer已被释放".to_string()))?;
    
    // 验证请求
    let req_id = data.get("req_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ComputerError::RuntimeError("缺少req_id".to_string()))?;
    
    let tool_name = data.get("tool_name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ComputerError::RuntimeError("缺少tool_name".to_string()))?;
    
    let params = data.get("params")
        .cloned()
        .unwrap_or(Value::Object(Default::default()));
    
    let timeout = data.get("timeout")
        .and_then(|v| v.as_f64());
    
    // 执行工具
    let result = computer.execute_tool(req_id, tool_name, params, timeout).await?;
    
    // 返回结果
    Ok(json!({
        "isError": result.get("isError").unwrap_or(&Value::Bool(false)),
        "content": result.get("content").unwrap_or(&Value::Null),
        "meta": result.get("meta").unwrap_or(&Value::Null)
    }))
}
