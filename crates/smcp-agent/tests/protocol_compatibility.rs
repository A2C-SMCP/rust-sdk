/*!
* 文件名: protocol_compatibility
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: None
* 描述: SMCP协议兼容性测试 / SMCP protocol compatibility tests
*/

use smcp::{
    events::*, AgentCallData, EnterOfficeReq, ReqId, Role, SMCPTool, ToolCallReq, SMCP_NAMESPACE,
};
use smcp_agent::{AuthProvider, DefaultAuthProvider};

#[test]
fn test_protocol_serialization_compatibility() {
    // 中文：测试协议序列化与Python SDK兼容性
    // English: Test protocol serialization compatibility with Python SDK

    // 1. 测试ReqId格式（应该是32位hex字符串，不是对象）
    let req_id = ReqId::new();
    let req_id_json = serde_json::to_string(&req_id).unwrap();
    assert!(req_id_json.starts_with('"'));
    assert!(req_id_json.ends_with('"'));
    assert_eq!(req_id_json.len(), 34); // 32 chars + 2 quotes
    assert!(!req_id_json.contains('{'));

    // 2. 测试AgentCallData扁平化（flatten应该不影响字段）
    let call_data = AgentCallData {
        agent: "test-agent".to_string(),
        req_id: ReqId::from_string("1234567890abcdef1234567890abcdef".to_string()),
    };
    let call_data_json = serde_json::to_string(&call_data).unwrap();
    assert!(call_data_json.contains("\"agent\":\"test-agent\""));
    assert!(call_data_json.contains("\"req_id\":\"1234567890abcdef1234567890abcdef\""));

    // 3. 测试Role枚举序列化为小写
    let role_json = serde_json::to_string(&Role::Agent).unwrap();
    assert_eq!(role_json, "\"agent\"");
    let role_json = serde_json::to_string(&Role::Computer).unwrap();
    assert_eq!(role_json, "\"computer\"");

    // 4. 测试EnterOfficeReq结构
    let enter_req = EnterOfficeReq {
        role: Role::Agent,
        name: "Test Agent".to_string(),
        office_id: "test-office".to_string(),
    };
    let enter_req_json = serde_json::to_string(&enter_req).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&enter_req_json).unwrap();
    assert_eq!(parsed["role"], "agent");
    assert_eq!(parsed["name"], "Test Agent");
    assert_eq!(parsed["office_id"], "test-office");

    // 5. 测试SMCPTool结构（确保与Python TypedDict匹配）
    let tool = SMCPTool {
        name: "echo".to_string(),
        description: "Echo tool".to_string(),
        params_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "text": {"type": "string"}
            }
        }),
        return_schema: Some(serde_json::json!({
            "type": "object",
            "properties": {
                "result": {"type": "string"}
            }
        })),
        meta: None,
    };
    let tool_json = serde_json::to_string(&tool).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&tool_json).unwrap();
    assert_eq!(parsed["name"], "echo");
    assert_eq!(parsed["description"], "Echo tool");
    assert!(parsed["params_schema"].is_object());
    assert!(parsed["return_schema"].is_object());
    assert!(!parsed.as_object().unwrap().contains_key("meta")); // None字段应该被跳过

    // 6. 测试ToolCallReq扁平化
    let tool_call = ToolCallReq {
        base: AgentCallData {
            agent: "test-agent".to_string(),
            req_id: ReqId::from_string("1234567890abcdef1234567890abcdef".to_string()),
        },
        computer: "test-computer".to_string(),
        tool_name: "echo".to_string(),
        params: serde_json::json!({"text": "hello"}),
        timeout: 30,
    };
    let tool_call_json = serde_json::to_string(&tool_call).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&tool_call_json).unwrap();
    assert_eq!(parsed["agent"], "test-agent");
    assert_eq!(parsed["computer"], "test-computer");
    assert_eq!(parsed["tool_name"], "echo");
    assert_eq!(parsed["timeout"], 30);
}

#[test]
fn test_auth_provider_headers_format() {
    // 中文：测试认证提供者头部格式与Python一致
    // English: Test auth provider headers format matches Python

    let auth = DefaultAuthProvider::new("test-agent".to_string(), "test-office".to_string())
        .with_api_key("test-api-key".to_string());

    let headers = auth.get_connection_headers();

    // 应该使用x-api-key，不是Authorization
    assert_eq!(headers.get("x-api-key"), Some(&"test-api-key".to_string()));
    assert!(!headers.contains_key("Authorization"));
}

#[test]
fn test_event_names_match_python() {
    // 中文：测试事件名称与Python SDK完全匹配
    // English: Test event names exactly match Python SDK

    // 这些常量必须与Python smcp.py中的定义完全一致
    assert_eq!(CLIENT_GET_TOOLS, "client:get_tools");
    assert_eq!(CLIENT_TOOL_CALL, "client:tool_call");
    assert_eq!(SERVER_JOIN_OFFICE, "server:join_office");
    assert_eq!(NOTIFY_ENTER_OFFICE, "notify:enter_office");
    assert_eq!(SMCP_NAMESPACE, "/smcp");
}
