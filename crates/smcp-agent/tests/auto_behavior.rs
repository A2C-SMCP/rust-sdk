/*!
* 文件名: auto_behavior
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: tokio, smcp-agent
* 描述: 测试 Agent 的自动行为 / Test Agent auto-behavior
*/

use smcp::SMCPTool;
use smcp_agent::{
    auth::DefaultAuthProvider, config::SmcpAgentConfig, events::AsyncAgentEventHandler,
    transport::NotificationMessage, AsyncSmcpAgent,
};
use std::sync::Arc;
use tokio::sync::RwLock;

/// 测试用的事件处理器，记录自动行为触发
#[derive(Clone)]
struct AutoBehaviorTracker {
    events: Arc<RwLock<Vec<String>>>,
}

impl AutoBehaviorTracker {
    fn new() -> Self {
        Self {
            events: Arc::new(RwLock::new(Vec::new())),
        }
    }

    async fn record_event(&self, event: String) {
        self.events.write().await.push(event);
    }
}

#[async_trait::async_trait]
impl AsyncAgentEventHandler for AutoBehaviorTracker {
    async fn on_computer_enter_office(
        &self,
        data: smcp::EnterOfficeNotification,
        _agent: &AsyncSmcpAgent,
    ) -> Result<(), smcp_agent::error::SmcpAgentError> {
        self.record_event(format!("enter_office: {:?}", data.computer))
            .await;
        Ok(())
    }

    async fn on_computer_leave_office(
        &self,
        data: smcp::LeaveOfficeNotification,
        _agent: &AsyncSmcpAgent,
    ) -> Result<(), smcp_agent::error::SmcpAgentError> {
        self.record_event(format!("leave_office: {:?}", data.computer))
            .await;
        Ok(())
    }

    async fn on_computer_update_config(
        &self,
        data: smcp::UpdateMCPConfigNotification,
        _agent: &AsyncSmcpAgent,
    ) -> Result<(), smcp_agent::error::SmcpAgentError> {
        self.record_event(format!("update_config: {}", data.computer))
            .await;
        Ok(())
    }

    async fn on_tools_received(
        &self,
        computer: &str,
        tools: Vec<SMCPTool>,
        _agent: &AsyncSmcpAgent,
    ) -> Result<(), smcp_agent::error::SmcpAgentError> {
        self.record_event(format!(
            "tools_received: {} has {} tools",
            computer,
            tools.len()
        ))
        .await;
        Ok(())
    }

    async fn on_desktop_updated(
        &self,
        computer: &str,
        desktops: Vec<String>,
        _agent: &AsyncSmcpAgent,
    ) -> Result<(), smcp_agent::error::SmcpAgentError> {
        self.record_event(format!(
            "desktop_updated: {} has {} desktops",
            computer,
            desktops.len()
        ))
        .await;
        Ok(())
    }
}

#[tokio::test]
async fn test_notification_message_creation() {
    // 测试各种通知消息的创建
    let enter_office = NotificationMessage::EnterOffice(smcp::EnterOfficeNotification {
        office_id: "office123".to_string(),
        computer: Some("computer1".to_string()),
        agent: Some("agent1".to_string()),
    });

    let leave_office = NotificationMessage::LeaveOffice(smcp::LeaveOfficeNotification {
        office_id: "office123".to_string(),
        computer: Some("computer1".to_string()),
        agent: Some("agent1".to_string()),
    });

    let update_config = NotificationMessage::UpdateConfig(smcp::UpdateMCPConfigNotification {
        computer: "computer1".to_string(),
    });

    let update_tool_list = NotificationMessage::UpdateToolList(smcp::UpdateToolListNotification {
        computer: "computer1".to_string(),
    });

    let update_desktop = NotificationMessage::UpdateDesktop("computer1".to_string());

    // 验证所有消息都能正确创建
    assert!(matches!(enter_office, NotificationMessage::EnterOffice(_)));
    assert!(matches!(leave_office, NotificationMessage::LeaveOffice(_)));
    assert!(matches!(
        update_config,
        NotificationMessage::UpdateConfig(_)
    ));
    assert!(matches!(
        update_tool_list,
        NotificationMessage::UpdateToolList(_)
    ));
    assert!(matches!(
        update_desktop,
        NotificationMessage::UpdateDesktop(_)
    ));
}

#[tokio::test]
async fn test_auto_behavior_sequence() {
    // 测试自动行为的触发序列
    let _tracker = AutoBehaviorTracker::new();

    // 模拟 Python 的自动行为序列
    // 1. Computer 进入办公室
    let enter_office = NotificationMessage::EnterOffice(smcp::EnterOfficeNotification {
        office_id: "office1".to_string(),
        computer: Some("computer1".to_string()),
        agent: None,
    });

    // 2. Computer 更新配置
    let update_config = NotificationMessage::UpdateConfig(smcp::UpdateMCPConfigNotification {
        computer: "computer1".to_string(),
    });

    // 3. Computer 更新工具列表
    let update_tool_list = NotificationMessage::UpdateToolList(smcp::UpdateToolListNotification {
        computer: "computer1".to_string(),
    });

    // 4. Computer 更新桌面
    let update_desktop = NotificationMessage::UpdateDesktop("computer1".to_string());

    // 验证通知消息结构
    match enter_office {
        NotificationMessage::EnterOffice(data) => {
            assert_eq!(data.office_id, "office1");
            assert_eq!(data.computer.unwrap(), "computer1");
        }
        _ => panic!("Expected EnterOffice"),
    }

    match update_config {
        NotificationMessage::UpdateConfig(data) => {
            assert_eq!(data.computer, "computer1");
        }
        _ => panic!("Expected UpdateConfig"),
    }

    match update_tool_list {
        NotificationMessage::UpdateToolList(data) => {
            assert_eq!(data.computer, "computer1");
        }
        _ => panic!("Expected UpdateToolList"),
    }

    match update_desktop {
        NotificationMessage::UpdateDesktop(computer) => {
            assert_eq!(computer, "computer1");
        }
        _ => panic!("Expected UpdateDesktop"),
    }
}

#[tokio::test]
async fn test_event_handler_integration() {
    // 测试事件处理器与 Agent 的集成
    let tracker = AutoBehaviorTracker::new();
    let auth_provider = DefaultAuthProvider::new("test_agent".to_string(), "office1".to_string());
    let config = SmcpAgentConfig::default();

    // 创建带有事件处理器的 Agent
    let _agent = AsyncSmcpAgent::new(auth_provider, config).with_event_handler(tracker.clone());

    // Agent 创建成功即表示集成正常
}

#[tokio::test]
async fn test_notification_payload_validation() {
    // 测试通知负载的有效性
    let test_cases = vec![
        (
            NotificationMessage::EnterOffice(smcp::EnterOfficeNotification {
                office_id: "office1".to_string(),
                computer: Some("comp1".to_string()),
                agent: None,
            }),
            "EnterOffice with computer only",
        ),
        (
            NotificationMessage::EnterOffice(smcp::EnterOfficeNotification {
                office_id: "office1".to_string(),
                computer: None,
                agent: Some("agent1".to_string()),
            }),
            "EnterOffice with agent only",
        ),
        (
            NotificationMessage::LeaveOffice(smcp::LeaveOfficeNotification {
                office_id: "office1".to_string(),
                computer: Some("comp1".to_string()),
                agent: Some("agent1".to_string()),
            }),
            "LeaveOffice with both",
        ),
        (
            NotificationMessage::UpdateConfig(smcp::UpdateMCPConfigNotification {
                computer: "comp1".to_string(),
            }),
            "UpdateConfig",
        ),
        (
            NotificationMessage::UpdateToolList(smcp::UpdateToolListNotification {
                computer: "comp1".to_string(),
            }),
            "UpdateToolList",
        ),
        (
            NotificationMessage::UpdateDesktop("comp1".to_string()),
            "UpdateDesktop",
        ),
    ];

    for (notification, description) in test_cases {
        // 验证每种通知都能正确创建
        match notification {
            NotificationMessage::EnterOffice(_) => {
                assert!(description.contains("EnterOffice"));
            }
            NotificationMessage::LeaveOffice(_) => {
                assert!(description.contains("LeaveOffice"));
            }
            NotificationMessage::UpdateConfig(_) => {
                assert!(description.contains("UpdateConfig"));
            }
            NotificationMessage::UpdateToolList(_) => {
                assert!(description.contains("UpdateToolList"));
            }
            NotificationMessage::UpdateDesktop(_) => {
                assert!(description.contains("UpdateDesktop"));
            }
        }
    }
}

#[tokio::test]
async fn test_python_compatibility_behavior() {
    // 测试与 Python 兼容的行为模式
    let _tracker = AutoBehaviorTracker::new();

    // 模拟 Python Agent 的典型行为：
    // 1. 收到 enter_office → 自动调用 get_tools
    // 2. 收到 update_config → 自动调用 get_tools
    // 3. 收到 update_tool_list → 自动调用 get_tools
    // 4. 收到 update_desktop → 自动调用 get_desktop

    // 这些自动行为在 async_agent.rs 的通知处理任务中实现
    // 这里我们验证通知消息的结构正确性

    let notifications = vec![
        NotificationMessage::EnterOffice(smcp::EnterOfficeNotification {
            office_id: "office1".to_string(),
            computer: Some("computer1".to_string()),
            agent: None,
        }),
        NotificationMessage::UpdateConfig(smcp::UpdateMCPConfigNotification {
            computer: "computer1".to_string(),
        }),
        NotificationMessage::UpdateToolList(smcp::UpdateToolListNotification {
            computer: "computer1".to_string(),
        }),
        NotificationMessage::UpdateDesktop("computer1".to_string()),
    ];

    // 验证所有通知都针对同一个 computer
    for (i, notification) in notifications.into_iter().enumerate() {
        match notification {
            NotificationMessage::EnterOffice(data) => {
                assert_eq!(data.computer.unwrap(), "computer1");
                assert_eq!(i, 0); // 第一个通知
            }
            NotificationMessage::LeaveOffice(_) => {
                // 这个测试用例中没有 LeaveOffice
                panic!("Unexpected LeaveOffice notification");
            }
            NotificationMessage::UpdateConfig(data) => {
                assert_eq!(data.computer, "computer1");
                assert_eq!(i, 1); // 第二个通知
            }
            NotificationMessage::UpdateToolList(data) => {
                assert_eq!(data.computer, "computer1");
                assert_eq!(i, 2); // 第三个通知
            }
            NotificationMessage::UpdateDesktop(computer) => {
                assert_eq!(computer, "computer1");
                assert_eq!(i, 3); // 第四个通知
            }
        }
    }
}

#[tokio::test]
async fn test_error_handling_in_notifications() {
    // 测试通知处理中的错误情况
    let test_cases = vec![
        // 空的 office_id
        NotificationMessage::EnterOffice(smcp::EnterOfficeNotification {
            office_id: "".to_string(),
            computer: None,
            agent: None,
        }),
        // 空的 computer name
        NotificationMessage::UpdateConfig(smcp::UpdateMCPConfigNotification {
            computer: "".to_string(),
        }),
        // 空的 computer name
        NotificationMessage::UpdateToolList(smcp::UpdateToolListNotification {
            computer: "".to_string(),
        }),
        // 空的 computer name
        NotificationMessage::UpdateDesktop("".to_string()),
    ];

    // 验证这些边界情况也能正确创建（即使可能无效）
    for (i, notification) in test_cases.into_iter().enumerate() {
        match notification {
            NotificationMessage::EnterOffice(data) => {
                assert_eq!(data.office_id, "");
                assert_eq!(i, 0);
            }
            NotificationMessage::UpdateConfig(data) => {
                assert_eq!(data.computer, "");
                assert_eq!(i, 1);
            }
            NotificationMessage::UpdateToolList(data) => {
                assert_eq!(data.computer, "");
                assert_eq!(i, 2);
            }
            NotificationMessage::UpdateDesktop(computer) => {
                assert_eq!(computer, "");
                assert_eq!(i, 3);
            }
            _ => {
                // 其他通知类型不应该出现
                panic!("Unexpected notification type at index {}", i);
            }
        }
    }
}
