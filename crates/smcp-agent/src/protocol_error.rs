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

/// 「无法判定」时的兜底文案（既非空 ack、也非可识别 flat ErrorPayload）。
///
/// 注意它**不再**表示「空响应失败」——自协议 v0.5.0 起空 ack 是**成功**；真正落进本文案的是形状
/// 不认识的响应（如已废除的 `(bool, str | None)` 元组形态）。
///
/// ⚠️ 该串是**用户可见**的兜底文案，且与 Python 参考实现**逐字对齐**——Python
/// `a2c_smcp/utils/office.py::NO_RESPONSE_MESSAGE` 是**双语**串
/// （`"服务器未返回结果 / No response from server"`）。历史实现只取了英文半句却仍声称「逐字对齐」，
/// 任何跨 SDK 的报文 / fixture 对照都会红（#226 复审 🔴1）。改文案必须两边同时改。
pub const NO_RESPONSE_MESSAGE: &str = "服务器未返回结果 / No response from server";

/// A2C-SMCP 协议级错误（flat ErrorPayload）/ A2C-SMCP protocol-level error (flat ErrorPayload)。
///
/// 当 Agent SDK 在 Socket.IO ack 中识别到 flat ErrorPayload（顶层含 `code` 且属协议错误码闭集）
/// 时产出。覆盖 / Covers:
/// - `client:get_resources`：`4014` / `4015`（顶层平铺 `mcp_server` / `capability`）。
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
    pub mcp_server: Option<String>,
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
            mcp_server: get("mcp_server")
                .and_then(Value::as_str)
                .map(str::to_string),
            capability: get("capability")
                .and_then(Value::as_str)
                .map(str::to_string),
            details,
            reason,
        }
    }

    /// 无法判定 ack 形状时的兜底构造（无码：`code = -1`）。
    ///
    /// 用于 [`parse_room_ack`] 的第三分歧——形状不认识（既非空 ack、也非顶层含 `code` 的 flat
    /// ErrorPayload）。**宁严勿宽**：把它判成失败，而不是静默当成功（后者会把未迁移的调用方缺陷藏起来）。
    pub fn indeterminate() -> Self {
        Self {
            payload: serde_json::Map::new(),
            code: -1,
            message: NO_RESPONSE_MESSAGE.to_string(),
            mcp_server: None,
            capability: None,
            details: serde_json::Map::new(),
            reason: None,
        }
    }

    /// 由**本端主动构造**的 flat [`smcp::ErrorPayload`] 产出协议错误。
    ///
    /// 用途：SDK 在**本地前置校验**阶段产生的拒绝（如 Agent 换房时的 `4106 Already In Room`）必须与
    /// 「服务端经 ack 回传的拒绝」对调用方**同形**——同一个 [`SmcpProtocolError`]、同一套字段提取、
    /// 同一份 canonical 文案（文案由 [`smcp::build_room_rejection_error`] 提供）。故此处复用内部的
    /// `from_value` 而非另写字段映射，避免两条构造路径漂移。
    ///
    /// Build a protocol error from a locally constructed flat payload, reusing the same field
    /// extraction as the ack-parsing path so the two cannot drift apart.
    pub fn from_error_payload(payload: &smcp::ErrorPayload) -> Self {
        match serde_json::to_value(payload) {
            Ok(value) => Self::from_value(&value),
            // `ErrorPayload` 恒可序列化；万一失败则退化为「无法判定」（`code = -1`），绝不 panic。
            Err(_) => Self::indeterminate(),
        }
    }
}

/// 解析**房间事件 ack**（`server:join_office` / `server:leave_office`）→ 裁决。
///
/// 房间事件的成功响应是**空 ack**、失败是 flat [`smcp::ErrorPayload`]（协议 v0.5.0 废除
/// `(bool, str | None)` 元组形态），判定规则与 Python 参考实现
/// `a2c_smcp/utils/office.py::parse_join_ack` **逐条一致**：
///
/// | ack 形状 | 结果 |
/// |---|---|
/// | 空 ack（零参 ACK 拆封后的 `[]`，或 1-tuple 形态拆封后的 `null`）| `Ok(())` |
/// | 顶层含 `code` 的对象（flat ErrorPayload）| `Err`，带 `code` / `message` / `details` |
/// | 其余任何形状 | `Err`（`code = -1`，文案 [`NO_RESPONSE_MESSAGE`]）|
///
/// **未知码同样算拒绝**（宁严勿宽）：协议未来新增码时，旧 SDK MUST fail-safe 到「被拒」，绝不静默
/// 假装成功。故此处**不**复用闭集谓词 [`smcp::is_protocol_error_payload`]（它只认已建模的码），
/// 而以「顶层含 `code`」为判据。
///
/// **参数顺序亦是契约**：`code` 存在即拒绝，故调用方 MUST 在 `req_id` 校验**之前**调用本函数——
/// ErrorPayload 不带 `req_id`，先查 `req_id` 会把结构化拒绝误报成「响应 req_id 不匹配」。对标 Python
/// `client.py::get_computers_in_office` 的同款约束。
///
/// Resolve a room-event ack: empty ack means success; a top-level `code` means a structured
/// rejection (unknown codes included, fail-safe); any other shape is an indeterminate failure.
// 与同模块 [`raise_for_error_payload`] 同一约定：`SmcpProtocolError` 携带 code/message/details 等诊断
// 字段，体积超 clippy `result_large_err` 阈值；按项目既有做法局部 `#[allow]` 而非装箱——装箱会给每个
// 调用点（含服务端 ack 判定的热路径）增加一次解包成本，收益不成比例。
#[allow(clippy::result_large_err)]
pub fn parse_room_ack(response: &Value) -> Result<(), SmcpProtocolError> {
    if response.is_null() || response.as_array().is_some_and(Vec::is_empty) {
        return Ok(());
    }
    if response.get("code").is_some() {
        return Err(SmcpProtocolError::from_value(response));
    }
    Err(SmcpProtocolError::indeterminate())
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
        for code in [404, 4006, 4007, 4008, 4014, 4015, 4016, 4017, 4018, 4019] {
            let err = raise_for_error_payload(&json!({"code": code, "message": "boom"}))
                .expect_err(&format!("code {code} 应判定为协议错误"));
            assert_eq!(err.code, code);
            assert_eq!(err.message, "boom");
        }
    }

    /// #226 P0-1：房间事件的拒绝码 MUST 被判定为协议错误。
    ///
    /// 历史实现里 `ErrorCode::from_code` 不含 400 / 403 / 500 / 4101–4106 ⇒ `is_protocol_error_payload`
    /// 对这批码恒 `false` ⇒ 本函数返回 `Ok(())`，于是**结构化拒绝被当作成功**：Agent 收到「房间已有
    /// Agent」却报成功，此后所有 `client:*` 调用从一个从未进入的房发起。本测试钉死该闭集不得再缺这九个码。
    #[test]
    fn test_room_management_codes_are_protocol_errors() {
        for code in [400, 403, 500, 4101, 4102, 4103, 4104, 4105, 4106] {
            let err = raise_for_error_payload(&json!({"code": code, "message": "room rejected"}))
                .expect_err(&format!(
                    "房间/通用码 {code} 必须被判为协议错误，否则拒绝会被读成成功"
                ));
            assert_eq!(
                err.code, code,
                "房间/通用码 {code} 必须被判为协议错误，否则拒绝会被读成成功"
            );
            assert_eq!(err.message, "room rejected");
        }
    }

    /// 房间事件 ack 的三分歧（空 ack / flat ErrorPayload / 无法判定），逐条对齐 Python
    /// `utils/office.py::parse_join_ack`。
    #[test]
    fn test_parse_room_ack_three_way_verdict() {
        // 空 ack 的两种线格式：零参 ACK 拆封后的 `[]`，与 1-tuple 形态拆封后的 `null` ⇒ 成功。
        assert!(parse_room_ack(&json!([])).is_ok());
        assert!(parse_room_ack(&Value::Null).is_ok());

        // flat ErrorPayload ⇒ 结构化拒绝，code / message / details 齐备（调用方可按码分流）。
        let rejected = parse_room_ack(&json!({
            "code": 4105,
            "message": "Name already taken in room",
            "details": {"office_id": "office-a", "role": "computer"}
        }))
        .unwrap_err();
        assert_eq!(rejected.code, 4105);
        assert_eq!(rejected.message, "Name already taken in room");
        assert_eq!(
            rejected.details.get("role").and_then(Value::as_str),
            Some("computer")
        );

        // **未建模的码也算拒绝**（宁严勿宽）：未来协议新增码时旧 SDK 必须 fail-safe 到「被拒」。
        let unknown = parse_room_ack(&json!({"code": 4299, "message": "future code"})).unwrap_err();
        assert_eq!(unknown.code, 4299);

        // 形状不认识（已废除的 `(bool, str | None)` 元组）⇒ 无法判定，**不**当成功。
        let indeterminate = parse_room_ack(&json!([true, null])).unwrap_err();
        assert_eq!(indeterminate.code, -1);
        assert_eq!(indeterminate.message, NO_RESPONSE_MESSAGE);
    }

    /// #226 复审 🔴1：「无法判定」兜底文案必须与 Python 参考实现**逐字**一致。
    ///
    /// Python `a2c_smcp/utils/office.py::NO_RESPONSE_MESSAGE` 是**双语**串；历史实现只保留了英文
    /// 半句，却在注释里声称已对齐——跨 SDK 报文对照（以及任何以文案为判据的 fixture）都会红。
    /// 本断言把字面值钉死，改文案必须两边同时改。
    #[test]
    fn test_no_response_message_matches_python_reference_verbatim() {
        assert_eq!(
            NO_RESPONSE_MESSAGE,
            "服务器未返回结果 / No response from server"
        );
    }

    #[test]
    fn test_4014_4015_top_level_fields_extracted() {
        let err = raise_for_error_payload(&json!({
            "code": 4015,
            "message": "capability not supported",
            "mcp_server": "docs-server",
            "capability": "resources"
        }))
        .unwrap_err();
        assert_eq!(err.mcp_server.as_deref(), Some("docs-server"));
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
            "mcp_server": "srv",
            "future_top_level": {"k": 1}
        }))
        .unwrap_err();
        // 便捷字段照常提取
        assert_eq!(err.mcp_server.as_deref(), Some("srv"));
        // 原始整包保留全部顶层字段（含未建模的）
        assert_eq!(err.payload.get("code").and_then(Value::as_i64), Some(4015));
        assert_eq!(
            err.payload.get("mcp_server").and_then(Value::as_str),
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
