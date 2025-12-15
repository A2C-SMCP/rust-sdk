/*!
* 文件名: mod
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: None
* 描述: SMCP Agent测试共享工具模块 / SMCP Agent test common utilities module
*/

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use smcp::{
    EnterOfficeNotification, LeaveOfficeNotification, SMCPTool, UpdateMCPConfigNotification,
};
use tokio::sync::Mutex;
use tracing::debug;

use smcp_agent::{AsyncSmcpAgent, DefaultAuthProvider, SmcpAgentConfig};

/// 测试用的Agent事件处理器，用于捕获事件
#[derive(Debug, Default, Clone)]
#[allow(dead_code)]
pub struct TestEventHandler {
    /// 收到的Computer进入办公室事件
    pub computer_enter_events: Arc<Mutex<Vec<EnterOfficeNotification>>>,
    /// 收到的Computer离开办公室事件
    pub computer_leave_events: Arc<Mutex<Vec<LeaveOfficeNotification>>>,
    /// 收到的Computer更新配置事件
    pub computer_update_events: Arc<Mutex<Vec<UpdateMCPConfigNotification>>>,
    /// 收到的工具列表
    #[allow(clippy::type_complexity)]
    pub tools_received: Arc<Mutex<Vec<(String, Vec<SMCPTool>)>>>,
}

impl TestEventHandler {
    /// 创建新的测试事件处理器
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self::default()
    }

    /// 清空所有事件
    #[allow(dead_code)]
    pub async fn clear(&self) {
        self.computer_enter_events.lock().await.clear();
        self.computer_leave_events.lock().await.clear();
        self.computer_update_events.lock().await.clear();
        self.tools_received.lock().await.clear();
    }

    /// 等待事件到达
    #[allow(dead_code)]
    pub async fn wait_for_events(&self, count: usize, timeout: Duration) -> bool {
        // 如果等待0个事件，立即返回false
        if count == 0 {
            return false;
        }

        let start = tokio::time::Instant::now();
        while start.elapsed() < timeout {
            let total_events = self.computer_enter_events.lock().await.len()
                + self.computer_leave_events.lock().await.len()
                + self.computer_update_events.lock().await.len()
                + self.tools_received.lock().await.len();
            if total_events >= count {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }
}

#[async_trait::async_trait]
impl smcp_agent::AsyncAgentEventHandler for TestEventHandler {
    async fn on_computer_enter_office(
        &self,
        data: EnterOfficeNotification,
        _agent: &AsyncSmcpAgent,
    ) -> Result<(), smcp_agent::SmcpAgentError> {
        debug!("Received computer enter office event: {:?}", data);
        self.computer_enter_events.lock().await.push(data);
        Ok(())
    }

    async fn on_computer_leave_office(
        &self,
        data: LeaveOfficeNotification,
        _agent: &AsyncSmcpAgent,
    ) -> Result<(), smcp_agent::SmcpAgentError> {
        debug!("Received computer leave office event: {:?}", data);
        self.computer_leave_events.lock().await.push(data);
        Ok(())
    }

    async fn on_computer_update_config(
        &self,
        data: UpdateMCPConfigNotification,
        _agent: &AsyncSmcpAgent,
    ) -> Result<(), smcp_agent::SmcpAgentError> {
        debug!("Received computer update config event: {:?}", data);
        self.computer_update_events.lock().await.push(data);
        Ok(())
    }

    async fn on_tools_received(
        &self,
        computer: &str,
        tools: Vec<SMCPTool>,
        _agent: &AsyncSmcpAgent,
    ) -> Result<(), smcp_agent::SmcpAgentError> {
        debug!("Received tools from {}: {:?}", computer, tools);
        self.tools_received
            .lock()
            .await
            .push((computer.to_string(), tools));
        Ok(())
    }

    async fn on_desktop_updated(
        &self,
        computer: &str,
        desktops: Vec<String>,
        _agent: &AsyncSmcpAgent,
    ) -> Result<(), smcp_agent::SmcpAgentError> {
        debug!("Received desktop from {}: {:?}", computer, desktops);
        Ok(())
    }
}

/// 创建测试用的默认工具列表
#[allow(dead_code)]
pub fn create_test_tools() -> Vec<SMCPTool> {
    vec![
        SMCPTool {
            name: "echo".to_string(),
            description: "Echo the input text / 回显输入的文本".to_string(),
            params_schema: json!({
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                        "description": "Text to echo / 要回显的文本"
                    }
                },
                "required": ["text"]
            }),
            return_schema: None,
            meta: None,
        },
        SMCPTool {
            name: "add".to_string(),
            description: "Add two numbers / 计算两个数的和".to_string(),
            params_schema: json!({
                "type": "object",
                "properties": {
                    "a": {"type": "number"},
                    "b": {"type": "number"}
                },
                "required": ["a", "b"]
            }),
            return_schema: Some(json!({
                "type": "object",
                "properties": {
                    "result": {
                        "type": "number",
                        "description": "Sum of a and b / a和b的和"
                    }
                }
            })),
            meta: None,
        },
    ]
}

/// 创建测试用的Agent配置
#[allow(dead_code)]
pub fn create_test_agent_config() -> SmcpAgentConfig {
    SmcpAgentConfig::new()
        .with_default_timeout(5)
        .with_tool_call_timeout(5)
        .with_reconnect_interval(100)
        .with_max_retries(3)
}

/// 测试夹具：创建Agent实例
#[allow(dead_code)]
pub fn create_test_agent(agent_id: &str, office_id: &str) -> AsyncSmcpAgent {
    let auth = DefaultAuthProvider::new(agent_id.to_string(), office_id.to_string());
    let config = create_test_agent_config();
    AsyncSmcpAgent::new(auth, config)
}

/// 测试夹具：创建带事件处理器的Agent实例
#[allow(dead_code)]
pub fn create_test_agent_with_handler(
    agent_id: &str,
    office_id: &str,
    handler: TestEventHandler,
) -> AsyncSmcpAgent {
    let auth = DefaultAuthProvider::new(agent_id.to_string(), office_id.to_string());
    let config = create_test_agent_config();
    AsyncSmcpAgent::new(auth, config).with_event_handler(handler)
}
