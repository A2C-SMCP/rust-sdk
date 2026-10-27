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
        bundle_id: "test_bundle".to_string(),
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
    assert_eq!(tool.bundle_id, deserialized.bundle_id);
    assert_eq!(tool.description, deserialized.description);
    assert_eq!(tool.params_schema, deserialized.params_schema);
    assert_eq!(tool.return_schema, deserialized.return_schema);
    assert!(deserialized.meta.is_none());
}

/// 协议 0.3.0 D1（#136）：`SMCPTool` 必带 `bundle_id`（required，snake_case wire 字段），
/// 位于 `name` 之后——Agent 据此归属工具，**禁止**切 exposed name 的 `__` 前缀反推。
#[test]
fn test_smcp_tool_bundle_id_wire_field() {
    let tool = SMCPTool {
        name: "everything__echo".to_string(),
        bundle_id: "everything".to_string(),
        description: "echo".to_string(),
        params_schema: serde_json::json!({"type": "object"}),
        return_schema: None,
        meta: None,
    };

    let json: serde_json::Value = serde_json::to_value(&tool).unwrap();
    // wire 字段名 = snake_case `bundle_id`，值 = 解析后 bundle_id（非从 `name` 切 `__` 得来）。
    assert_eq!(json["bundle_id"], serde_json::json!("everything"));

    // 声明序须与协议一致：name, bundle_id, description, ...（供跨 SDK 逐字节对拍）。
    let serialized = serde_json::to_string(&tool).unwrap();
    let name_pos = serialized.find("\"name\"").unwrap();
    let bundle_pos = serialized.find("\"bundle_id\"").unwrap();
    let desc_pos = serialized.find("\"description\"").unwrap();
    assert!(
        name_pos < bundle_pos && bundle_pos < desc_pos,
        "字段序应为 name < bundle_id < description，实际: {serialized}"
    );
}

#[test]
fn test_enter_office_notification() {
    let notification = EnterOfficeNotification {
        office_id: "office123".to_string(),
        computer: Some("computer1".to_string()),
        agent: None,
        computer_id: None,
    };

    let json = serde_json::to_string(&notification).unwrap();
    let deserialized: EnterOfficeNotification = serde_json::from_str(&json).unwrap();
    assert_eq!(notification.office_id, deserialized.office_id);
    assert_eq!(notification.computer, deserialized.computer);
    assert!(deserialized.agent.is_none());
}

/// protocol#64（v0.5.0）`computer_id` 的 wire 形状：spec 见 `data-structures.md`
/// §EnterOfficeReq / §EnterOfficeNotification / §LeaveOfficeNotification / §SessionInfo。
///
/// PR1 只保证类型与形状——服务端强制校验、Computer 身份生成/持久化在后续 PR。
/// 规范要点：`role="computer"` 时 MUST 携带；`role="agent"` 时 MUST NOT 出现该键
/// （包括 `null`）；成员为 Agent 的通知同样省略键。
const SAMPLE_COMPUTER_ID: &str = "7c9e6679-7425-40de-944b-e07fc1f90ae7";

#[test]
fn test_enter_office_req_computer_id_wire_shape() {
    // Agent 生产构造恒为 None ⇒ 序列化必须缺键，而不是输出 null。
    let agent_req = EnterOfficeReq {
        role: Role::Agent,
        name: "agent-1".to_string(),
        office_id: "office-1".to_string(),
        computer_id: None,
    };
    let json = serde_json::to_value(&agent_req).unwrap();
    assert!(
        json.get("computer_id").is_none(),
        "Agent 入房 MUST NOT 携带 computer_id 键（含 null），实际: {json}"
    );

    // PR1 升级窗口的核心兼容场景：旧 Computer（role="computer" 且无该键）的报文仍可反序列化。
    let legacy_computer = r#"{"role":"computer","name":"computer-1","office_id":"office-1"}"#;
    let de: EnterOfficeReq = serde_json::from_str(legacy_computer).unwrap();
    assert!(de.computer_id.is_none());

    // Agent 旧报文同样容缺。
    let legacy_agent = r#"{"role":"agent","name":"agent-1","office_id":"office-1"}"#;
    let de: EnterOfficeReq = serde_json::from_str(legacy_agent).unwrap();
    assert!(de.computer_id.is_none());

    // Computer 携带规范 UUID ⇒ 原样上行并可往返。
    let computer_req = EnterOfficeReq {
        role: Role::Computer,
        name: "computer-1".to_string(),
        office_id: "office-1".to_string(),
        computer_id: Some(SAMPLE_COMPUTER_ID.to_string()),
    };
    let json = serde_json::to_value(&computer_req).unwrap();
    assert_eq!(json["computer_id"], SAMPLE_COMPUTER_ID);
    let back: EnterOfficeReq = serde_json::from_value(json).unwrap();
    assert_eq!(back.computer_id.as_deref(), Some(SAMPLE_COMPUTER_ID));
}

#[test]
fn test_office_notifications_computer_id_wire_shape() {
    // 成员为 Computer ⇒ enter/leave 均带 computer_id。
    let enter = EnterOfficeNotification {
        office_id: "office-1".to_string(),
        computer: Some("computer-1".to_string()),
        computer_id: Some(SAMPLE_COMPUTER_ID.to_string()),
        agent: None,
    };
    let json = serde_json::to_value(&enter).unwrap();
    assert_eq!(json["computer_id"], SAMPLE_COMPUTER_ID);
    let back: EnterOfficeNotification = serde_json::from_value(json).unwrap();
    assert_eq!(back.computer_id.as_deref(), Some(SAMPLE_COMPUTER_ID));

    // 成员为 Agent ⇒ 省略键（不得为 null）。
    let enter_agent = EnterOfficeNotification {
        office_id: "office-1".to_string(),
        computer: None,
        computer_id: None,
        agent: Some("agent-1".to_string()),
    };
    let json = serde_json::to_value(&enter_agent).unwrap();
    assert!(
        json.get("computer_id").is_none(),
        "Agent 成员的 enter 通知 MUST 省略 computer_id 键，实际: {json}"
    );

    let leave = LeaveOfficeNotification {
        office_id: "office-1".to_string(),
        computer: Some("computer-1".to_string()),
        computer_id: Some(SAMPLE_COMPUTER_ID.to_string()),
        agent: None,
    };
    let json = serde_json::to_value(&leave).unwrap();
    assert_eq!(json["computer_id"], SAMPLE_COMPUTER_ID);
    let back: LeaveOfficeNotification = serde_json::from_value(json).unwrap();
    assert_eq!(back.computer_id.as_deref(), Some(SAMPLE_COMPUTER_ID));

    let leave_agent = LeaveOfficeNotification {
        office_id: "office-1".to_string(),
        computer: None,
        computer_id: None,
        agent: Some("agent-1".to_string()),
    };
    let json = serde_json::to_value(&leave_agent).unwrap();
    assert!(
        json.get("computer_id").is_none(),
        "Agent 成员的 leave 通知 MUST 省略 computer_id 键，实际: {json}"
    );
}

#[test]
fn test_session_info_computer_id_wire_shape() {
    // None ⇒ 省略键（对齐协议 NotRequired：缺键，而非 null）。
    let agent_info = SessionInfo {
        sid: "sid-1".to_string(),
        name: "agent-1".to_string(),
        role: Role::Agent,
        office_id: "office-1".to_string(),
        a2c_version: None,
        computer_id: None,
    };
    let json = serde_json::to_value(&agent_info).unwrap();
    assert!(
        json.get("computer_id").is_none(),
        "SessionInfo 为 None 时必须省略 computer_id 键，实际: {json}"
    );

    // Some ⇒ 带上并往返。
    let computer_info = SessionInfo {
        sid: "sid-2".to_string(),
        name: "computer-1".to_string(),
        role: Role::Computer,
        office_id: "office-1".to_string(),
        a2c_version: None,
        computer_id: Some(SAMPLE_COMPUTER_ID.to_string()),
    };
    let json = serde_json::to_value(&computer_info).unwrap();
    assert_eq!(json["computer_id"], SAMPLE_COMPUTER_ID);
    let back: SessionInfo = serde_json::from_value(json).unwrap();
    assert_eq!(back.computer_id.as_deref(), Some(SAMPLE_COMPUTER_ID));

    // 旧报文（无该键）⇒ None。
    let legacy = r#"{"sid":"sid-3","name":"c2","role":"computer","office_id":"office-1"}"#;
    let de: SessionInfo = serde_json::from_str(legacy).unwrap();
    assert!(de.computer_id.is_none());
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
