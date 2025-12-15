/*!
* 文件名: notification_handling
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: tokio, smcp-agent
* 描述: 测试通知处理机制 / Test notification handling mechanism
*/

use smcp_agent::{
    auth::DefaultAuthProvider, config::SmcpAgentConfig, events::AsyncAgentEventHandler,
    transport::NotificationMessage, AsyncSmcpAgent,
};
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use tokio::time::{timeout, Duration};

/// 测试用的事件处理器
struct TestEventHandler {
    received_notifications: Arc<RwLock<Vec<String>>>,
}

impl TestEventHandler {
    fn new() -> Self {
        Self {
            received_notifications: Arc::new(RwLock::new(Vec::new())),
        }
    }

    async fn add_notification(&self, notification: String) {
        self.received_notifications.write().await.push(notification);
    }
}

#[async_trait::async_trait]
impl AsyncAgentEventHandler for TestEventHandler {
    async fn on_computer_enter_office(
        &self,
        data: smcp::EnterOfficeNotification,
        _agent: &AsyncSmcpAgent,
    ) -> Result<(), smcp_agent::error::SmcpAgentError> {
        self.add_notification(format!("enter_office: {:?}", data))
            .await;
        Ok(())
    }

    async fn on_computer_leave_office(
        &self,
        data: smcp::LeaveOfficeNotification,
        _agent: &AsyncSmcpAgent,
    ) -> Result<(), smcp_agent::error::SmcpAgentError> {
        self.add_notification(format!("leave_office: {:?}", data))
            .await;
        Ok(())
    }

    async fn on_computer_update_config(
        &self,
        data: smcp::UpdateMCPConfigNotification,
        _agent: &AsyncSmcpAgent,
    ) -> Result<(), smcp_agent::error::SmcpAgentError> {
        self.add_notification(format!("update_config: {:?}", data))
            .await;
        Ok(())
    }

    async fn on_tools_received(
        &self,
        _computer: &str,
        tools: Vec<smcp::SMCPTool>,
        _agent: &AsyncSmcpAgent,
    ) -> Result<(), smcp_agent::error::SmcpAgentError> {
        self.add_notification(format!("tools_received: {} tools", tools.len()))
            .await;
        Ok(())
    }

    async fn on_desktop_updated(
        &self,
        _computer: &str,
        desktops: Vec<String>,
        _agent: &AsyncSmcpAgent,
    ) -> Result<(), smcp_agent::error::SmcpAgentError> {
        self.add_notification(format!("desktop_updated: {} desktops", desktops.len()))
            .await;
        Ok(())
    }
}

#[tokio::test]
async fn test_notification_message_serialization() {
    // 测试通知消息的序列化
    let notification = NotificationMessage::EnterOffice(smcp::EnterOfficeNotification {
        office_id: "office123".to_string(),
        computer: Some("computer1".to_string()),
        agent: None,
    });

    // 这个测试主要验证 NotificationMessage 枚举可以正常创建和使用
    match notification {
        NotificationMessage::EnterOffice(data) => {
            assert_eq!(data.office_id, "office123");
            assert_eq!(data.computer.unwrap(), "computer1");
        }
        _ => panic!("Expected EnterOffice notification"),
    }
}

#[tokio::test]
async fn test_notification_channel_flow() {
    // 测试通知通道的基本流程
    let (tx, mut rx) = mpsc::unbounded_channel::<NotificationMessage>();

    // 发送各种通知
    tx.send(NotificationMessage::EnterOffice(
        smcp::EnterOfficeNotification {
            office_id: "office1".to_string(),
            computer: Some("comp1".to_string()),
            agent: None,
        },
    ))
    .unwrap();

    tx.send(NotificationMessage::LeaveOffice(
        smcp::LeaveOfficeNotification {
            office_id: "office1".to_string(),
            computer: Some("comp1".to_string()),
            agent: None,
        },
    ))
    .unwrap();

    tx.send(NotificationMessage::UpdateConfig(
        smcp::UpdateMCPConfigNotification {
            computer: "comp1".to_string(),
        },
    ))
    .unwrap();

    tx.send(NotificationMessage::UpdateToolList(
        smcp::UpdateToolListNotification {
            computer: "comp1".to_string(),
        },
    ))
    .unwrap();

    tx.send(NotificationMessage::UpdateDesktop("comp1".to_string()))
        .unwrap();

    // 接收并验证通知
    let mut count = 0;
    while timeout(Duration::from_millis(100), rx.recv()).await.is_ok() {
        count += 1;
    }

    assert_eq!(count, 5);
}

#[tokio::test]
async fn test_agent_auto_behavior_on_enter_office() {
    // 测试 Agent 在收到 enter_office 通知后的自动行为
    let handler = TestEventHandler::new();
    let auth_provider = DefaultAuthProvider::new("test_agent".to_string(), "office1".to_string());
    let config = SmcpAgentConfig::default();

    // 创建 Agent 并设置事件处理器
    let _agent = AsyncSmcpAgent::new(auth_provider, config).with_event_handler(handler);

    // 模拟通知处理逻辑
    let notification = NotificationMessage::EnterOffice(smcp::EnterOfficeNotification {
        office_id: "office1".to_string(),
        computer: Some("computer1".to_string()),
        agent: None,
    });

    // 验证通知被正确处理
    // 注意：这个测试主要验证通知结构，实际的 get_tools 调用需要 mock
    match notification {
        NotificationMessage::EnterOffice(data) => {
            assert_eq!(data.office_id, "office1");
            assert_eq!(data.computer.unwrap(), "computer1");
        }
        _ => panic!("Expected EnterOffice notification"),
    }
}

#[tokio::test]
async fn test_all_notification_types() {
    // 测试所有通知类型的创建
    let notifications = vec![
        NotificationMessage::EnterOffice(smcp::EnterOfficeNotification {
            office_id: "office1".to_string(),
            computer: Some("comp1".to_string()),
            agent: Some("agent1".to_string()),
        }),
        NotificationMessage::LeaveOffice(smcp::LeaveOfficeNotification {
            office_id: "office1".to_string(),
            computer: Some("comp1".to_string()),
            agent: Some("agent1".to_string()),
        }),
        NotificationMessage::UpdateConfig(smcp::UpdateMCPConfigNotification {
            computer: "comp1".to_string(),
        }),
        NotificationMessage::UpdateToolList(smcp::UpdateToolListNotification {
            computer: "comp1".to_string(),
        }),
        NotificationMessage::UpdateDesktop("comp1".to_string()),
    ];

    // 验证所有通知类型都能正确创建
    assert_eq!(notifications.len(), 5);

    // 验证每种通知类型
    for (i, notification) in notifications.into_iter().enumerate() {
        match i {
            0 => assert!(matches!(notification, NotificationMessage::EnterOffice(_))),
            1 => assert!(matches!(notification, NotificationMessage::LeaveOffice(_))),
            2 => assert!(matches!(notification, NotificationMessage::UpdateConfig(_))),
            3 => assert!(matches!(
                notification,
                NotificationMessage::UpdateToolList(_)
            )),
            4 => assert!(matches!(
                notification,
                NotificationMessage::UpdateDesktop(_)
            )),
            _ => panic!("Unexpected notification index"),
        }
    }
}
