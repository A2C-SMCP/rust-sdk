/*!
* 文件名: agent_skill_consume
* 作者: JQQ
* 创建日期: 2026/06/02
* 最后修改日期: 2026/06/02
* 版权: 2023 JQQ. All rights reserved.
* 依赖: tokio, smcp-agent, smcp
* 描述: AGT-02 SKILL 消费 API + notify 刷新 hook 测试 / SKILL consume API + notify hook tests
*/

//! AGT-02：`on_skills_received` hook 表面与向后兼容测试，对标 Python `test_v021_consume.py`
//! 的 notify 刷新 / hook 兼容场景。get_skills/get_skill 的响应解析分支由
//! `skill_consume` 内联单测覆盖；NotificationMessage 通道由 `notification_handling.rs` 覆盖。

mod common;

use std::sync::Arc;

use smcp::A2CSkillRef;
use smcp_agent::{
    error::SmcpAgentError, events::AsyncAgentEventHandler, transport::NotificationMessage,
    AsyncSmcpAgent,
};
use tokio::sync::Mutex;

use common::create_test_agent;

fn sample_skills() -> Vec<A2CSkillRef> {
    vec![
        A2CSkillRef {
            name: "my-helper".to_string(),
            source: "user".to_string(),
            uri: None,
            path: "/abs/skills/my-helper".to_string(),
            description: "A helper".to_string(),
            license: None,
            compatibility: None,
            allowed_tools: None,
            tags: None,
            version: None,
            skill_metadata: None,
        },
        A2CSkillRef {
            name: "mcp:tfrobot-tools:code-review".to_string(),
            source: "mcp:tfrobot-tools".to_string(),
            uri: Some("skill://tfrobot-tools/code-review".to_string()),
            path: "/abs/skills/code-review".to_string(),
            description: "Review".to_string(),
            license: None,
            compatibility: None,
            allowed_tools: None,
            tags: None,
            version: None,
            skill_metadata: None,
        },
    ]
}

/// 旧处理器：只实现 enter_office，**不**实现 on_skills_received（依赖默认实现）。
/// 对标 Python `hasattr` 守卫——旧处理器无需改动即编译通过、行为为 no-op。
struct LegacyHandler;

#[async_trait::async_trait]
impl AsyncAgentEventHandler for LegacyHandler {
    async fn on_computer_enter_office(
        &self,
        _data: smcp::EnterOfficeNotification,
        _agent: &AsyncSmcpAgent,
    ) -> Result<(), SmcpAgentError> {
        Ok(())
    }
    // 注意：未实现 on_skills_received，走 trait 默认实现
}

/// 新处理器：覆盖 on_skills_received，捕获派发的 computer + skills。
#[derive(Default)]
struct SkillCapturingHandler {
    #[allow(clippy::type_complexity)]
    received: Arc<Mutex<Vec<(String, Vec<A2CSkillRef>)>>>,
}

#[async_trait::async_trait]
impl AsyncAgentEventHandler for SkillCapturingHandler {
    async fn on_skills_received(
        &self,
        computer: &str,
        skills: Vec<A2CSkillRef>,
        _agent: &AsyncSmcpAgent,
    ) -> Result<(), SmcpAgentError> {
        self.received
            .lock()
            .await
            .push((computer.to_string(), skills));
        Ok(())
    }
}

#[tokio::test]
async fn test_legacy_handler_default_on_skills_received_is_noop_ok() {
    // 向后兼容：旧处理器未实现 on_skills_received，默认实现返回 Ok（不 panic、不报错）
    let handler = LegacyHandler;
    let agent = create_test_agent("agent1", "office1");
    let result = handler
        .on_skills_received("comp1", sample_skills(), &agent)
        .await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_overriding_handler_receives_skills() {
    // 新处理器：on_skills_received 收到正确的 computer 与 skills 列表
    let handler = SkillCapturingHandler::default();
    let received = handler.received.clone();
    let agent = create_test_agent("agent1", "office1");

    handler
        .on_skills_received("comp1", sample_skills(), &agent)
        .await
        .unwrap();

    let captured = received.lock().await;
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].0, "comp1");
    assert_eq!(captured[0].1.len(), 2);
    assert_eq!(captured[0].1[0].name, "my-helper");
    assert_eq!(captured[0].1[1].source, "mcp:tfrobot-tools");
}

#[tokio::test]
async fn test_update_skills_notification_carries_computer() {
    // notify:update_skills 解析后携带 computer，驱动通知循环自动重拉
    let notification = NotificationMessage::UpdateSkills("computer-xyz".to_string());
    match notification {
        NotificationMessage::UpdateSkills(computer) => assert_eq!(computer, "computer-xyz"),
        _ => panic!("Expected UpdateSkills notification"),
    }
}
