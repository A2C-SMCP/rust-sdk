/*!
* 文件名: response.rs
* 作者: JQQ
* 创建日期: 2026/06/02
* 最后修改日期: 2026/06/02
* 版权: 2023 JQQ. All rights reserved.
* 依赖: serde_json
* 描述: Agent 响应校验共享 helper / Shared response-validation helpers
*/

//! Agent 端响应校验共享 helper（与 `request_builders` 对称：一个构造请求、一个校验响应）。
//!
//! 当前承载 `client:*` ack 的 `req_id` 回显校验——`get_tools` / `get_desktop` / `list_room` /
//! SKILL 消费（`skill_consume`）均经此单点收敛，避免「错误处理逻辑重复」（DRY）——以及
//! `get_resources` 响应解析（[`parse_get_resources_response`]，与 `skill_consume` 同款纯函数约定）。

use serde_json::Value;
use smcp::GetResourcesRet;

use crate::error::{Result, SmcpAgentError};
use crate::protocol_error::raise_for_error_payload;

/// 校验响应回显的 `req_id` 与请求一致 / validate the echoed `req_id` matches the request。
///
/// 缺失 → [`SmcpAgentError::internal`]；不一致 → [`SmcpAgentError::ReqIdMismatch`]。语义对标
/// Python 各消费方法的 `if response.get("req_id") != req["req_id"]: raise`，为全 crate 单一真源。
pub(crate) fn ensure_req_id(response: &Value, expected: &str) -> Result<()> {
    let actual = response
        .get("req_id")
        .and_then(Value::as_str)
        .ok_or_else(|| SmcpAgentError::internal("Missing req_id in response"))?;
    if actual != expected {
        return Err(SmcpAgentError::ReqIdMismatch {
            expected: expected.to_string(),
            actual: actual.to_string(),
        });
    }
    Ok(())
}

/// 解析 `client:get_resources` 响应为资源页 / parse a `client:get_resources` ack into a resource page。
///
/// 流程对齐兄弟消费方（`skill_consume`）：flat ErrorPayload（`4014` 未注册 / `4015` 无 `resources`
/// 能力）→ 协议错误；`req_id` 回显校验；整页反序列化为 [`GetResourcesRet`]（`resources` 经
/// `#[serde(default)]` 缺省空、`next_cursor`/`req_id` 可选）。无 I/O，供 async/sync Agent 共享并可
/// 独立单测（不依赖 Socket.IO 传输）。对标 Python `a2c_smcp/agent/client.py::get_resources` 响应段。
pub(crate) fn parse_get_resources_response(
    response: &Value,
    expected_req_id: &str,
) -> Result<GetResourcesRet> {
    raise_for_error_payload(response)?;
    ensure_req_id(response, expected_req_id)?;
    let ret: GetResourcesRet = serde_json::from_value(response.clone())?;
    Ok(ret)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_ensure_req_id_ok() {
        assert!(ensure_req_id(&json!({ "req_id": "R1", "x": 1 }), "R1").is_ok());
    }

    #[test]
    fn test_ensure_req_id_missing_is_internal_error() {
        let err = ensure_req_id(&json!({ "x": 1 }), "R1").unwrap_err();
        assert!(matches!(err, SmcpAgentError::Internal(_)), "got {err:?}");
    }

    #[test]
    fn test_ensure_req_id_mismatch() {
        let err = ensure_req_id(&json!({ "req_id": "OTHER" }), "R1").unwrap_err();
        match err {
            SmcpAgentError::ReqIdMismatch { expected, actual } => {
                assert_eq!(expected, "R1");
                assert_eq!(actual, "OTHER");
            }
            other => panic!("expected ReqIdMismatch, got {other:?}"),
        }
    }

    // ── parse_get_resources_response ─────────────────────────────────────────
    // 覆盖 get_resources 方法本体的编排（错误透传 / req_id 校验 / 整页解析）——transport.call
    // 是不可单测的 I/O 边界（SocketIoTransport 为具体 struct），其余编排经此纯函数覆盖。

    #[test]
    fn test_get_resources_happy_full_page() {
        // 满页：多资源 + next_cursor + req_id 回显（Python parity：满页带 next_cursor）。
        let resp = json!({
            "resources": [
                { "uri": "file:///a.txt", "name": "a", "_meta": { "k": "v" } },
                { "uri": "file:///b.txt", "name": "b" }
            ],
            "next_cursor": "CUR2",
            "req_id": "R1"
        });
        let ret = parse_get_resources_response(&resp, "R1").unwrap();
        assert_eq!(ret.resources.len(), 2);
        assert_eq!(ret.resources[0].uri.as_deref(), Some("file:///a.txt"));
        assert_eq!(ret.next_cursor.as_deref(), Some("CUR2"));
        assert_eq!(ret.req_id.as_ref().map(smcp::ReqId::as_str), Some("R1"));
    }

    #[test]
    fn test_get_resources_happy_last_page() {
        // 末页：无 resources/next_cursor 键 → 缺省空列表 + None 游标（Python parity：仅非空才带）。
        let resp = json!({ "req_id": "R2" });
        let ret = parse_get_resources_response(&resp, "R2").unwrap();
        assert!(ret.resources.is_empty());
        assert!(ret.next_cursor.is_none());
        assert_eq!(ret.req_id.as_ref().map(smcp::ReqId::as_str), Some("R2"));
    }

    #[test]
    fn test_get_resources_flat_error_4014_raises_protocol_error() {
        // flat ErrorPayload（4014 MCP Server 未注册）→ 协议错误，不被吞、不二次 unwrap。
        let resp = json!({
            "code": 4014,
            "message": "MCP Server not registered",
            "mcp_server_name": "srv-x"
        });
        let err = parse_get_resources_response(&resp, "R1").unwrap_err();
        assert!(matches!(err, SmcpAgentError::Protocol(_)), "got {err:?}");
    }

    #[test]
    fn test_get_resources_flat_error_4015_raises_protocol_error() {
        // flat ErrorPayload（4015 未声明 resources 能力，含 capability）→ 协议错误。
        let resp = json!({
            "code": 4015,
            "message": "capability not supported",
            "mcp_server_name": "srv-x",
            "capability": "resources"
        });
        let err = parse_get_resources_response(&resp, "R1").unwrap_err();
        assert!(matches!(err, SmcpAgentError::Protocol(_)), "got {err:?}");
    }

    #[test]
    fn test_get_resources_req_id_mismatch() {
        // req_id 不匹配 → ReqIdMismatch（先 raise_for_error_payload 后 ensure_req_id 的顺序）。
        let resp = json!({ "req_id": "OTHER", "resources": [] });
        let err = parse_get_resources_response(&resp, "R1").unwrap_err();
        assert!(
            matches!(err, SmcpAgentError::ReqIdMismatch { .. }),
            "got {err:?}"
        );
    }
}
