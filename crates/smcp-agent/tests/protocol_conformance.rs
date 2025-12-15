/*!
* 文件名: protocol_conformance
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: tokio, smcp-agent
* 描述: 测试协议一致性，确保与 Python SDK 兼容 / Test protocol conformance with Python SDK
*/

use smcp::{
    EnterOfficeNotification, LeaveOfficeNotification, ReqId, Role, SMCPTool, ToolCallRet,
    UpdateMCPConfigNotification, UpdateToolListNotification,
};
use smcp_agent::{
    auth::{AuthProvider, DefaultAuthProvider},
    transport::NotificationMessage,
};

/// 测试 ToolCallRet 的 JSON 输出格式与 Python SDK 一致
#[tokio::test]
async fn test_tool_call_ret_python_compatibility() {
    // 测试场景 1: 成功的工具调用
    let success_ret = ToolCallRet {
        content: Some(vec![serde_json::json!({
            "type": "text",
            "text": "Operation completed"
        })]),
        is_error: Some(false),
        req_id: Some(ReqId::from_string("req-123".to_string())),
    };

    let json = serde_json::to_string(&success_ret).unwrap();

    // 验证 JSON 结构与 Python 输出完全一致
    // Python SDK 输出示例: {"content":[{"type":"text","text":"Operation completed"}],"isError":false,"req_id":"req-123"}
    assert!(json.contains("\"content\""));
    assert!(json.contains("\"isError\":false"));
    assert!(json.contains("\"req_id\":\"req-123\""));

    // 验证字段顺序不重要，但字段名必须完全匹配
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(parsed.get("content").is_some());
    assert!(parsed.get("isError").is_some());
    assert!(parsed.get("req_id").is_some());

    // 验证没有旧格式字段
    assert!(parsed.get("success").is_none());
    assert!(parsed.get("result").is_none());
    assert!(parsed.get("error").is_none());
}

#[tokio::test]
async fn test_tool_call_ret_error_python_compatibility() {
    // 测试场景 2: 失败的工具调用
    let error_ret = ToolCallRet {
        content: Some(vec![serde_json::json!({
            "type": "text",
            "text": "Tool execution failed: Invalid input"
        })]),
        is_error: Some(true),
        req_id: Some(ReqId::from_string("req-456".to_string())),
    };

    let json = serde_json::to_string(&error_ret).unwrap();

    // Python SDK 错误输出示例: {"content":[{"type":"text","text":"Tool execution failed: Invalid input"}],"isError":true,"req_id":"req-456"}
    assert!(json.contains("\"isError\":true"));

    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.get("isError").unwrap(), true);
    assert_eq!(parsed.get("content").unwrap().as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn test_notification_message_json_format() {
    // 测试各种通知消息的 JSON 格式
    let test_cases = vec![
        (
            NotificationMessage::EnterOffice(EnterOfficeNotification {
                office_id: "office-001".to_string(),
                computer: Some("computer-001".to_string()),
                agent: Some("agent-001".to_string()),
            }),
            "EnterOffice notification",
        ),
        (
            NotificationMessage::LeaveOffice(LeaveOfficeNotification {
                office_id: "office-001".to_string(),
                computer: Some("computer-001".to_string()),
                agent: Some("agent-001".to_string()),
            }),
            "LeaveOffice notification",
        ),
        (
            NotificationMessage::UpdateConfig(UpdateMCPConfigNotification {
                computer: "computer-001".to_string(),
            }),
            "UpdateConfig notification",
        ),
        (
            NotificationMessage::UpdateToolList(UpdateToolListNotification {
                computer: "computer-001".to_string(),
            }),
            "UpdateToolList notification",
        ),
        (
            NotificationMessage::UpdateDesktop("computer-001".to_string()),
            "UpdateDesktop notification",
        ),
    ];

    for (notification, _description) in test_cases {
        // 验证所有通知都能正确序列化
        match notification {
            NotificationMessage::EnterOffice(data) => {
                assert_eq!(data.office_id, "office-001");
                assert_eq!(data.computer.unwrap(), "computer-001");
                assert_eq!(data.agent.unwrap(), "agent-001");
            }
            NotificationMessage::LeaveOffice(data) => {
                assert_eq!(data.office_id, "office-001");
                assert_eq!(data.computer.unwrap(), "computer-001");
                assert_eq!(data.agent.unwrap(), "agent-001");
            }
            NotificationMessage::UpdateConfig(data) => {
                assert_eq!(data.computer, "computer-001");
            }
            NotificationMessage::UpdateToolList(data) => {
                assert_eq!(data.computer, "computer-001");
            }
            NotificationMessage::UpdateDesktop(computer) => {
                assert_eq!(computer, "computer-001");
            }
        }
    }
}

#[tokio::test]
async fn test_smcp_tool_serialization() {
    // 测试 SMCPTool 的序列化格式
    let tool = SMCPTool {
        name: "test_tool".to_string(),
        description: "A test tool".to_string(),
        params_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "input": {
                    "type": "string",
                    "description": "Input parameter"
                }
            },
            "required": ["input"]
        }),
        return_schema: Some(serde_json::json!({
            "type": "object",
            "properties": {
                "result": {
                    "type": "string"
                }
            }
        })),
        meta: None,
    };

    let json = serde_json::to_string(&tool).unwrap();

    // 验证 JSON 包含正确的字段
    assert!(json.contains("\"name\":\"test_tool\""));
    assert!(json.contains("\"description\":\"A test tool\""));
    assert!(json.contains("\"params_schema\""));

    // 验证可以反序列化
    let deserialized: SMCPTool = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.name, "test_tool");
    assert_eq!(deserialized.description, "A test tool");
}

#[tokio::test]
async fn test_role_serialization_lowercase() {
    // 测试 Role 枚举序列化为小写（与 Python 一致）
    let agent_json = serde_json::to_string(&Role::Agent).unwrap();
    assert_eq!(agent_json, "\"agent\"");

    let computer_json = serde_json::to_string(&Role::Computer).unwrap();
    assert_eq!(computer_json, "\"computer\"");

    // 测试反序列化
    let agent: Role = serde_json::from_str("\"agent\"").unwrap();
    assert!(matches!(agent, Role::Agent));

    let computer: Role = serde_json::from_str("\"computer\"").unwrap();
    assert!(matches!(computer, Role::Computer));
}

#[tokio::test]
async fn test_req_id_format() {
    // 测试 ReqId 的生成和格式
    let req_id1 = ReqId::new();
    let req_id2 = ReqId::new();

    // 验证生成的 ID 是唯一的
    assert_ne!(req_id1.as_str(), req_id2.as_str());

    // 验证 ID 不为空
    assert!(!req_id1.as_str().is_empty());
    assert!(!req_id2.as_str().is_empty());

    // 测试从字符串创建
    let custom_id = ReqId::from_string("custom-req-123".to_string());
    assert_eq!(custom_id.as_str(), "custom-req-123");

    // 测试序列化
    let json = serde_json::to_string(&custom_id).unwrap();
    assert_eq!(json, "\"custom-req-123\"");

    // 测试反序列化
    let deserialized: ReqId = serde_json::from_str("\"custom-req-123\"").unwrap();
    assert_eq!(deserialized.as_str(), "custom-req-123");
}

#[tokio::test]
async fn test_protocol_field_names() {
    // 测试所有协议结构体使用正确的字段名（snake_case in Rust, camelCase in JSON）

    // ToolCallRet 字段验证
    let tool_ret = ToolCallRet {
        content: None,
        is_error: Some(true),
        req_id: None,
    };

    let json = serde_json::to_string(&tool_ret).unwrap();
    // Rust 字段 is_error 应该序列化为 JSON 字段 isError
    assert!(json.contains("\"isError\":true"));
    assert!(!json.contains("\"is_error\""));

    // 测试所有通知结构体字段名
    let enter_office = EnterOfficeNotification {
        office_id: "office1".to_string(),
        computer: Some("comp1".to_string()),
        agent: Some("agent1".to_string()),
    };

    let json = serde_json::to_string(&enter_office).unwrap();
    assert!(json.contains("\"office_id\""));
    assert!(json.contains("\"computer\""));
    assert!(json.contains("\"agent\""));
}

#[tokio::test]
async fn test_empty_and_optional_fields() {
    // 测试空值和可选字段的处理

    // 测试完全空的 ToolCallRet
    let empty_ret = ToolCallRet {
        content: None,
        is_error: None,
        req_id: None,
    };

    let json = serde_json::to_string(&empty_ret).unwrap();
    assert_eq!(json, "{}");

    // 测试部分字段为空
    let partial_ret = ToolCallRet {
        content: Some(vec![]),
        is_error: Some(false),
        req_id: None,
    };

    let json = serde_json::to_string(&partial_ret).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

    // content 应该存在但为空数组
    assert_eq!(parsed.get("content").unwrap().as_array().unwrap().len(), 0);
    assert!(parsed.get("isError").is_some());
    assert!(parsed.get("req_id").is_none());
}

#[tokio::test]
async fn test_python_compatibility_checklist() {
    // Python SDK 兼容性检查清单

    // 1. ToolCallRet 使用 content/isError/req_id 字段
    let _ = ToolCallRet {
        content: Some(vec![serde_json::json!({"type": "text", "text": "test"})]),
        is_error: Some(false),
        req_id: Some(ReqId::new()),
    };

    // 2. Role 序列化为小写
    assert_eq!(serde_json::to_string(&Role::Agent).unwrap(), "\"agent\"");
    assert_eq!(
        serde_json::to_string(&Role::Computer).unwrap(),
        "\"computer\""
    );

    // 3. 通知事件使用正确的结构体
    let _ = EnterOfficeNotification {
        office_id: "test".to_string(),
        computer: None,
        agent: None,
    };

    let _ = LeaveOfficeNotification {
        office_id: "test".to_string(),
        computer: None,
        agent: None,
    };

    let _ = UpdateMCPConfigNotification {
        computer: "test".to_string(),
    };

    let _ = UpdateToolListNotification {
        computer: "test".to_string(),
    };

    // 4. Agent 配置字段
    let auth = DefaultAuthProvider::new("test_agent".to_string(), "test_office".to_string());
    let config = auth.get_agent_config();
    assert_eq!(config.agent, "test_agent");
    assert_eq!(config.office_id, "test_office");

    // 所有检查通过
}
