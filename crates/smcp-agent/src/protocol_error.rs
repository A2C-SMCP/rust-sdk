/*!
* 文件名: protocol_error.rs
* 作者: JQQ
* 创建日期: 2026/06/01
* 最后修改日期: 2026/06/01
* 版权: 2023 JQQ. All rights reserved.
* 依赖: smcp, serde_json, thiserror
* 描述: Agent 端协议错误解析 / Agent-side protocol error parsing
*/

//! Agent 端协议错误 / Agent-side protocol errors。
//!
//! A2C-SMCP 协议级错误经 Socket.IO ack 第一参以 **flat ErrorPayload** 回传（无嵌套 envelope）。
//! 对标 Python `a2c_smcp/agent/errors.py`。
//!
//! 命名说明：本模块名为 `protocol_error`（而非 issue 文案的 `errors`），以避免与本 crate 既有的
//! [`crate::error`]（传输层 `SmcpAgentError`）混淆——二者职责不同：`SmcpAgentError` 是 SDK
//! 传输/调用层错误，[`SmcpProtocolError`] 是从对端 ack 解析出的**协议级**错误。

use serde_json::Value;

/// A2C-SMCP 协议级错误（flat ErrorPayload）/ A2C-SMCP protocol-level error (flat ErrorPayload)。
///
/// 当 Agent SDK 在 Socket.IO ack 中识别到 flat ErrorPayload（顶层含 `code` 且属协议错误码闭集）
/// 时产出。覆盖 / Covers:
/// - `client:get_resources`：`4014` / `4015`（顶层平铺 `mcp_server_name` / `capability`）。
/// - `client:get_skill[s]`：`4014` 复用 / `4016` Invalid Name / `4017` Resource Not Accessible
///   （v0.2.1 `details.reason`）。
/// - `client:get_blob`：`4018 Blob Not Accessible`（v0.2.1 `details.reason`）。
///
/// `details` 是诊断容器，Agent **MUST NOT** 透传给最终用户（防泄露）。
// 不派生 `Eq`：`details` 持 [`serde_json::Value`]（含 `f64`，非 `Eq`）。
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("[{code}] {message}")]
pub struct SmcpProtocolError {
    /// 原始 flat ErrorPayload 整包 / the raw flat ErrorPayload object。
    ///
    /// 保留**未显式建模**的顶层字段（前向兼容：未来协议码可能新增顶层分流字段），对齐 Python
    /// `SMCPProtocolError.payload`；下列便捷字段为其常用子集的类型化提取。
    pub payload: serde_json::Map<String, Value>,
    /// 协议错误码（缺省 / 非整数时回退 `-1`，对齐 Python `int(payload.get("code", -1))`）。
    pub code: i64,
    /// 人类可读错误描述（缺省回退空串）/ human-readable message (defaults to empty).
    pub message: String,
    /// 4014 / 4015 顶层分流字段 / top-level code-specific field (4014 / 4015)。
    pub mcp_server_name: Option<String>,
    /// 4015 顶层分流：缺失的 capability 名 / top-level for 4015: missing capability。
    pub capability: Option<String>,
    /// 诊断容器（4016 / 4017 / 4018 的 code-specific 字段下沉于此）/ diagnostic container。
    pub details: serde_json::Map<String, Value>,
    /// 4017 / 4018 共用的 `details.reason`（**开放枚举**：未知值原样保留、不 panic）。
    /// `details.reason` shared by 4017 / 4018; an open enum — unknown values are preserved verbatim.
    pub reason: Option<String>,
}

impl SmcpProtocolError {
    /// 从已判定为协议错误负载的 `Value` 宽松提取字段（缺省回退，镜像 Python `dict.get` 语义）。
    /// Leniently extract fields from a value already known to be a protocol error payload.
    fn from_value(response: &Value) -> Self {
        let obj = response.as_object();
        let get = |key: &str| obj.and_then(|o| o.get(key));
        let details = get("details")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let reason = details
            .get("reason")
            .and_then(Value::as_str)
            .map(str::to_string);
        Self {
            payload: obj.cloned().unwrap_or_default(),
            code: get("code").and_then(Value::as_i64).unwrap_or(-1),
            message: get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            mcp_server_name: get("mcp_server_name")
                .and_then(Value::as_str)
                .map(str::to_string),
            capability: get("capability")
                .and_then(Value::as_str)
                .map(str::to_string),
            details,
            reason,
        }
    }
}

/// 若 `response` 是 flat ErrorPayload（顶层 `code` 属协议错误码闭集）→ `Err(SmcpProtocolError)`；
/// 否则 `Ok(())`。与 server 端共用 [`smcp::is_protocol_error_payload`] 谓词，避免双重启发式漂移。
///
/// 协议依据 / Protocol: error-handling.md —— 无嵌套 envelope，禁止二次 unwrap。
/// 对标 Python `a2c_smcp/agent/errors.py::raise_for_error_payload`（Rust 改 `raise` 为 `Result`）。
// SmcpProtocolError 携带 code/message/details 等诊断字段，体积超 clippy result_large_err 阈值；
// 按项目既有约定（同 smcp::ErrorPayload 消费链）局部 #[allow] 而非装箱——装箱会增加调用点解包成本。
#[allow(clippy::result_large_err)]
pub fn raise_for_error_payload(response: &Value) -> Result<(), SmcpProtocolError> {
    if smcp::is_protocol_error_payload(response) {
        Err(SmcpProtocolError::from_value(response))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_non_error_payload_is_ok() {
        // 非协议错误码（200 / 未知 9999）或缺 code → Ok
        assert!(raise_for_error_payload(&json!({"code": 200, "message": "ok"})).is_ok());
        assert!(raise_for_error_payload(&json!({"code": 9999})).is_ok());
        assert!(raise_for_error_payload(&json!({"tools": []})).is_ok());
        // 嵌套 envelope 不被识别为协议错误（禁止二次 unwrap 的防线）
        assert!(raise_for_error_payload(&json!({"error": {"code": 404}})).is_ok());
    }

    #[test]
    fn test_all_protocol_codes_raise() {
        for code in [404, 4006, 4007, 4008, 4014, 4015, 4016, 4017, 4018] {
            let err = raise_for_error_payload(&json!({"code": code, "message": "boom"}))
                .expect_err(&format!("code {code} 应判定为协议错误"));
            assert_eq!(err.code, code);
            assert_eq!(err.message, "boom");
        }
    }

    #[test]
    fn test_4014_4015_top_level_fields_extracted() {
        let err = raise_for_error_payload(&json!({
            "code": 4015,
            "message": "capability not supported",
            "mcp_server_name": "docs-server",
            "capability": "resources"
        }))
        .unwrap_err();
        assert_eq!(err.mcp_server_name.as_deref(), Some("docs-server"));
        assert_eq!(err.capability.as_deref(), Some("resources"));
        assert!(err.reason.is_none());
    }

    #[test]
    fn test_4017_4018_details_reason_extracted_open_enum() {
        // 已知 reason
        let err = raise_for_error_payload(&json!({
            "code": 4017,
            "message": "not accessible",
            "details": {"reason": "traversal", "rel_path": "../etc"}
        }))
        .unwrap_err();
        assert_eq!(err.reason.as_deref(), Some("traversal"));
        assert_eq!(
            err.details.get("rel_path").and_then(Value::as_str),
            Some("../etc")
        );

        // 未知 reason：原样保留、不 panic（开放枚举兜底）
        let err = raise_for_error_payload(&json!({
            "code": 4018,
            "message": "blob",
            "details": {"reason": "future_unknown_reason"}
        }))
        .unwrap_err();
        assert_eq!(err.reason.as_deref(), Some("future_unknown_reason"));
    }

    #[test]
    fn test_lenient_missing_message_and_code() {
        // code 属协议集但缺 message → message 回退空串（镜像 Python get("message","")），不 panic
        let err = raise_for_error_payload(&json!({"code": 4016})).unwrap_err();
        assert_eq!(err.code, 4016);
        assert_eq!(err.message, "");
        assert!(err.details.is_empty());
    }

    #[test]
    fn test_raw_payload_preserved_for_forward_compat() {
        // 未显式建模的顶层字段（未来协议码新增）经 payload 完整保留，前向兼容（对齐 Python self.payload）
        let err = raise_for_error_payload(&json!({
            "code": 4015,
            "message": "x",
            "mcp_server_name": "srv",
            "future_top_level": {"k": 1}
        }))
        .unwrap_err();
        // 便捷字段照常提取
        assert_eq!(err.mcp_server_name.as_deref(), Some("srv"));
        // 原始整包保留全部顶层字段（含未建模的）
        assert_eq!(err.payload.get("code").and_then(Value::as_i64), Some(4015));
        assert_eq!(
            err.payload.get("mcp_server_name").and_then(Value::as_str),
            Some("srv")
        );
        assert_eq!(err.payload.get("future_top_level"), Some(&json!({"k": 1})));
    }

    #[test]
    fn test_display_format() {
        let err = raise_for_error_payload(&json!({"code": 404, "message": "Computer not found"}))
            .unwrap_err();
        assert_eq!(err.to_string(), "[404] Computer not found");
    }
}
