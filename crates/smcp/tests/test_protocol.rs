use smcp::*;

#[test]
fn test_req_id_serialization() {
    let req_id = ReqId::new();
    let json = serde_json::to_string(&req_id).unwrap();
    let deserialized: ReqId = serde_json::from_str(&json).unwrap();
    assert_eq!(req_id, deserialized);
}

#[test]
fn test_agent_call_data() {
    let data = AgentCallData {
        agent: "test_agent".to_string(),
        req_id: ReqId::new(),
    };

    let json = serde_json::to_string(&data).unwrap();
    let deserialized: AgentCallData = serde_json::from_str(&json).unwrap();
    assert_eq!(data.agent, deserialized.agent);
    assert_eq!(data.req_id, deserialized.req_id);
}

#[test]
fn test_tool_call_req() {
    let req = ToolCallReq {
        base: AgentCallData {
            agent: "agent1".to_string(),
            req_id: ReqId::from_string("test-req-123".to_string()),
        },
        computer: "computer1".to_string(),
        tool_name: "test_tool".to_string(),
        params: serde_json::json!({"arg1": "value1", "arg2": 42}),
        timeout: 30,
    };

    let json = serde_json::to_string(&req).unwrap();
    let deserialized: ToolCallReq = serde_json::from_str(&json).unwrap();
    assert_eq!(req.base.agent, deserialized.base.agent);
    assert_eq!(req.computer, deserialized.computer);
    assert_eq!(req.tool_name, deserialized.tool_name);
    assert_eq!(req.params, deserialized.params);
    assert_eq!(req.timeout, deserialized.timeout);
}

#[test]
fn test_smcp_tool() {
    let tool = SMCPTool {
        name: "test_tool".to_string(),
        description: "A test tool".to_string(),
        params_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "input": {"type": "string"}
            }
        }),
        return_schema: Some(serde_json::json!({"type": "string"})),
        meta: None,
    };

    let json = serde_json::to_string(&tool).unwrap();
    let deserialized: SMCPTool = serde_json::from_str(&json).unwrap();
    assert_eq!(tool.name, deserialized.name);
    assert_eq!(tool.description, deserialized.description);
    assert_eq!(tool.params_schema, deserialized.params_schema);
    assert_eq!(tool.return_schema, deserialized.return_schema);
    assert!(deserialized.meta.is_none());
}

#[test]
fn test_enter_office_notification() {
    let notification = EnterOfficeNotification {
        office_id: "office123".to_string(),
        computer: Some("computer1".to_string()),
        agent: None,
    };

    let json = serde_json::to_string(&notification).unwrap();
    let deserialized: EnterOfficeNotification = serde_json::from_str(&json).unwrap();
    assert_eq!(notification.office_id, deserialized.office_id);
    assert_eq!(notification.computer, deserialized.computer);
    assert!(deserialized.agent.is_none());
}

#[test]
fn test_all_events_constants() {
    // 验证所有事件常量都已定义
    assert_eq!(events::CLIENT_GET_TOOLS, "client:get_tools");
    assert_eq!(events::CLIENT_GET_CONFIG, "client:get_config");
    assert_eq!(events::CLIENT_GET_DESKTOP, "client:get_desktop");
    assert_eq!(events::CLIENT_TOOL_CALL, "client:tool_call");

    assert_eq!(events::SERVER_JOIN_OFFICE, "server:join_office");
    assert_eq!(events::SERVER_LEAVE_OFFICE, "server:leave_office");
    assert_eq!(events::SERVER_UPDATE_CONFIG, "server:update_config");
    assert_eq!(events::SERVER_UPDATE_TOOL_LIST, "server:update_tool_list");
    assert_eq!(events::SERVER_UPDATE_DESKTOP, "server:update_desktop");
    assert_eq!(events::SERVER_TOOL_CALL_CANCEL, "server:tool_call_cancel");
    assert_eq!(events::SERVER_LIST_ROOM, "server:list_room");

    assert_eq!(events::NOTIFY_TOOL_CALL_CANCEL, "notify:tool_call_cancel");
    assert_eq!(events::NOTIFY_ENTER_OFFICE, "notify:enter_office");
    assert_eq!(events::NOTIFY_LEAVE_OFFICE, "notify:leave_office");
    assert_eq!(events::NOTIFY_UPDATE_CONFIG, "notify:update_config");
    assert_eq!(events::NOTIFY_UPDATE_TOOL_LIST, "notify:update_tool_list");
    assert_eq!(events::NOTIFY_UPDATE_DESKTOP, "notify:update_desktop");

    assert_eq!(events::NOTIFY_PREFIX, "notify:");
}
