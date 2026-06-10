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
use smcp::{GetBlobRet, GetResourcesRet};

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

/// 解析 `client:get_blob` 单块响应为 [`GetBlobRet`] / parse a `client:get_blob` ack into one chunk。
///
/// 与 [`parse_get_resources_response`] 同款约定：flat ErrorPayload（`4018` `details.reason` ∈
/// `invalid_handle`/`forbidden`/`gone`/`range`）→ 协议错误；`req_id` 回显校验；整包反序列化为
/// [`GetBlobRet`]（含 `total_size`/`sha256`/`chunk_offset`/`eof`/base64 `blob`）。无 I/O，供 async/sync
/// Agent 共享并独立单测（不依赖 Socket.IO 传输）。对标 Python `a2c_smcp/agent/client.py::get_blob` 响应段。
///
/// 注：多块拉取的 drain 适配器**不**经此（每块自有 fresh `req_id`、靠 ack 关联，且漂移/完整性由
/// `sha256` 自证），仅 [`crate::async_agent::AsyncSmcpAgent::get_blob`] 单块公开方法用此校验回显。
pub(crate) fn parse_get_blob_response(
    response: &Value,
    expected_req_id: &str,
) -> Result<GetBlobRet> {
    raise_for_error_payload(response)?;
    ensure_req_id(response, expected_req_id)?;
    let ret: GetBlobRet = serde_json::from_value(response.clone())?;
    Ok(ret)
}

/// `client:tool_call` 结果三态（+成功）/ tri-state of a tool_call result (AGT-05 #44).
///
/// 据**结果级** A2C 标记在 `isError` 之上进一步分流，供 Agent 调用方区分「取消 / 超时 / 普通失败」：
/// - [`ToolCallOutcome::Cancelled`]：被 `notify:tool_call_cancel` 取消（`a2c_cancelled=true`）；
/// - [`ToolCallOutcome::TimedOut`]：执行超时（`a2c_timeout=true`）；
/// - [`ToolCallOutcome::Failed`]：其它工具级失败（`isError=true` 但无取消/超时标记）；
/// - [`ToolCallOutcome::Completed`]：成功。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCallOutcome {
    /// 成功（`isError` 非 true，无取消/超时标记）。
    Completed,
    /// 被取消（结果级 `a2c_cancelled=true`）。
    Cancelled,
    /// 超时（结果级 `a2c_timeout=true`）。
    TimedOut,
    /// 普通工具级失败（`isError=true`，无取消/超时标记）。
    Failed,
}

/// 据结果级 A2C 标记把 `client:tool_call` 响应分类为三态（+成功）（AGT-05 #44）。
///
/// **宽松读取**结果级 meta 的两种线上 key：`meta`（smcp `ToolCallRet` 形态）与 `_meta`（rmcp
/// `CallToolResult` / MCP 约定形态——Rust Computer 即经此发出取消/超时标记，见 INT-02 #70）。协议
/// data-structures.md §234 规定 consumer **SHOULD** 对 `meta`/`_meta` 兜底读取，故二者任一命中即生效。
///
/// 优先级：取消 > 超时 > 失败 > 成功（取消/超时是对 `isError` 的语义细化，故先判定）。纯函数，无 I/O，
/// 供 async/sync Agent 共享并独立单测（对标 Python agent 三态区分逻辑）。
pub fn classify_tool_call_outcome(response: &Value) -> ToolCallOutcome {
    if result_meta_flag(response, smcp::tool_meta::A2C_CANCELLED_KEY) {
        ToolCallOutcome::Cancelled
    } else if result_meta_flag(response, smcp::tool_meta::A2C_TIMEOUT_KEY) {
        ToolCallOutcome::TimedOut
    } else if response
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        ToolCallOutcome::Failed
    } else {
        ToolCallOutcome::Completed
    }
}

/// 读结果级 meta 的布尔标记，兜底 `meta` 与 `_meta` 两种线上 key（任一为 `true` 即真）。
/// Read a result-level meta bool flag, leniently across both `meta` and `_meta` wire keys.
fn result_meta_flag(response: &Value, key: &str) -> bool {
    ["meta", "_meta"].iter().any(|container| {
        response
            .get(*container)
            .and_then(|m| m.get(key))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    })
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

    // ── AGT-05 #44：tool_call 结果三态区分（classify_tool_call_outcome）──────────────────

    #[test]
    fn test_classify_completed() {
        // 成功：无 isError、无标记 → Completed。
        let resp = json!({ "content": [{ "type": "text", "text": "ok" }], "isError": false });
        assert_eq!(
            classify_tool_call_outcome(&resp),
            ToolCallOutcome::Completed
        );
        // 缺省（无 isError 键）同样视为成功。
        assert_eq!(
            classify_tool_call_outcome(&json!({ "content": [] })),
            ToolCallOutcome::Completed
        );
    }

    #[test]
    fn test_classify_failed_plain_error() {
        // isError=true 但无取消/超时标记 → Failed（普通工具级失败）。
        let resp = json!({ "isError": true, "content": [{ "type": "text", "text": "boom" }] });
        assert_eq!(classify_tool_call_outcome(&resp), ToolCallOutcome::Failed);
    }

    #[test]
    fn test_classify_cancelled_reads_both_meta_and_underscore_meta() {
        // 取消标记落 `meta`（smcp ToolCallRet 形态）→ Cancelled。
        let via_meta = json!({ "isError": true, "meta": { "a2c_cancelled": true } });
        assert_eq!(
            classify_tool_call_outcome(&via_meta),
            ToolCallOutcome::Cancelled
        );
        // 取消标记落 `_meta`（rmcp CallToolResult / Rust Computer 形态，INT-02 #70）→ 同样 Cancelled。
        // 这是取消全链能跑通的关键：Computer 发 `_meta`，Agent MUST 据此识别。
        let via_underscore = json!({ "isError": true, "_meta": { "a2c_cancelled": true } });
        assert_eq!(
            classify_tool_call_outcome(&via_underscore),
            ToolCallOutcome::Cancelled
        );
    }

    #[test]
    fn test_classify_timeout_reads_both_keys() {
        let via_meta = json!({ "isError": true, "meta": { "a2c_timeout": true } });
        assert_eq!(
            classify_tool_call_outcome(&via_meta),
            ToolCallOutcome::TimedOut
        );
        let via_underscore = json!({ "isError": true, "_meta": { "a2c_timeout": true } });
        assert_eq!(
            classify_tool_call_outcome(&via_underscore),
            ToolCallOutcome::TimedOut
        );
    }

    #[test]
    fn test_classify_cancelled_takes_precedence_over_timeout_and_error() {
        // 取消优先于超时（理论上不并存，但优先级须确定）。
        let resp = json!({
            "isError": true,
            "_meta": { "a2c_cancelled": true, "a2c_timeout": true }
        });
        assert_eq!(
            classify_tool_call_outcome(&resp),
            ToolCallOutcome::Cancelled
        );
    }

    // ── AGT-03 #38：parse_get_blob_response（get_blob 单块响应解析）──────────────────

    #[test]
    fn test_get_blob_happy_first_chunk() {
        // 首块：含 total_size/sha256/chunk_offset/eof/base64 blob + req_id 回显。
        let resp = json!({
            "blob_handle": "ts:abc",
            "mime_type": "image/png",
            "total_size": 1024,
            "sha256": "deadbeef",
            "chunk_offset": 0,
            "eof": false,
            "blob": "AAAA",
            "req_id": "R1"
        });
        let ret = parse_get_blob_response(&resp, "R1").unwrap();
        assert_eq!(ret.blob_handle, "ts:abc");
        assert_eq!(ret.total_size, 1024);
        assert_eq!(ret.chunk_offset, 0);
        assert!(!ret.eof);
        assert_eq!(ret.blob, "AAAA");
        assert_eq!(ret.mime_type.as_deref(), Some("image/png"));
    }

    #[test]
    fn test_get_blob_eof_probe_empty_chunk() {
        // EOF probe：offset==total_size 时 Computer 回空块 + eof=true（非 range 错误）。
        let resp = json!({
            "blob_handle": "ts:abc",
            "mime_type": "text/plain",
            "total_size": 5,
            "sha256": "cafef00d",
            "chunk_offset": 5,
            "eof": true,
            "blob": "",
            "req_id": "R2"
        });
        let ret = parse_get_blob_response(&resp, "R2").unwrap();
        assert!(ret.eof);
        assert_eq!(ret.blob, "");
        assert_eq!(ret.chunk_offset, 5);
    }

    #[test]
    fn test_get_blob_flat_error_4018_raises_with_reason() {
        // flat ErrorPayload（4018 句柄失效）→ 协议错误，details.reason 经 open-enum 提取。
        let resp = json!({
            "code": 4018,
            "message": "blob not accessible",
            "details": { "reason": "gone" }
        });
        let err = parse_get_blob_response(&resp, "R1").unwrap_err();
        match err {
            SmcpAgentError::Protocol(p) => {
                assert_eq!(p.code, 4018);
                assert_eq!(p.reason.as_deref(), Some("gone"));
            }
            other => panic!("expected protocol error, got {other:?}"),
        }
    }

    #[test]
    fn test_get_blob_req_id_mismatch() {
        // req_id 不匹配 → ReqIdMismatch（先 raise_for_error_payload 后 ensure_req_id 的顺序）。
        let resp = json!({
            "blob_handle": "ts:abc",
            "total_size": 0,
            "sha256": "x",
            "chunk_offset": 0,
            "eof": true,
            "blob": "",
            "req_id": "OTHER"
        });
        let err = parse_get_blob_response(&resp, "R1").unwrap_err();
        assert!(
            matches!(err, SmcpAgentError::ReqIdMismatch { .. }),
            "got {err:?}"
        );
    }
}
