//! 版本握手工具 / Version-handshake utilities (UTIL-01 #58)。
//!
//! 纯函数工具集，供客户端（Agent/Computer）在连接前后复用：
//! - [`build_handshake_url`]：在连接 URL 注入 `a2c_version`（丢弃调用方自带值，防版本漂移）；
//! - [`apply_polling_first_guard`] / [`enforce_polling_first`]：保证 **polling-first**
//!   传输顺序（0.2.2 MUST，避免 WS-only 直连绕过服务端 HTTP 版本校验）；
//! - [`extract_4008_payload`]：从服务端拒绝 body 解析出 4008 [`ErrorPayload`]；
//! - [`build_protocol_version_error`]：把 4008 负载映射为强类型 [`ProtocolVersionError`]。
//!
//! 对标 Python 参考实现 / Mirrors the Python reference: `a2c_smcp/utils/handshake.py`。
//! 协议依据 / Protocol: `a2c-smcp-protocol` versioning.md §连接握手流程。
//!
//! > 说明：版本号承载于**连接 URL query**（而非 auth dict / header），以便服务端 HTTP 中间件
//! > 在 Socket.IO 业务层之前完成版本校验。HS-02 (#22) 客户端接入待 tf-rust-socketio 0.8.1
//! > （暴露 WS close code 以支持 4900 主动分支）后落地；本模块为其前置纯工具。

use url::Url;

use crate::{ErrorCode, ErrorPayload, ProtocolVersionError};

/// 握手版本查询参数键 / Handshake version query-param key。
///
/// 对标 Python `A2C_VERSION_QUERY_KEY`。
pub const A2C_VERSION_QUERY_KEY: &str = "a2c_version";

/// 默认握手传输顺序 / Default handshake transport order（**polling-first**，0.2.2 MUST）。
///
/// polling 必须在首位：首个握手走 HTTP polling，才能在版本不兼容时读到 HTTP 400 + 4008 body
/// （WS-only 直连会绕过服务端 HTTP 中间件校验）。对标 Python `DEFAULT_HANDSHAKE_TRANSPORTS`。
pub const DEFAULT_HANDSHAKE_TRANSPORTS: [&str; 2] = ["polling", "websocket"];

/// 在连接 URL 上注入/覆盖 `a2c_version` query / Inject (overriding) `a2c_version` on the URL。
///
/// 保留其它 query pair（按原顺序），**丢弃**调用方自带的 `a2c_version`（版本权威只来自 SDK 的
/// [`crate::PROTOCOL_VERSION`]，防漂移）。对标 Python `build_handshake_url`。
///
/// # Errors
/// `base_url` 非法 URL 时返回 [`url::ParseError`]。
pub fn build_handshake_url(
    base_url: &str,
    protocol_version: &str,
) -> Result<String, url::ParseError> {
    let mut url = Url::parse(base_url)?;
    // 先取出除 a2c_version 外的既有 query pair（owned 副本），稍后整体重写。
    let preserved: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| k != A2C_VERSION_QUERY_KEY)
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    {
        let mut qp = url.query_pairs_mut();
        qp.clear();
        for (k, v) in &preserved {
            qp.append_pair(k, v);
        }
        qp.append_pair(A2C_VERSION_QUERY_KEY, protocol_version);
    }
    Ok(url.to_string())
}

/// 保证传输顺序 polling-first / Ensure polling-first transport order。
///
/// 始终把 `polling` 置于首位（保留其它传输的相对顺序、去重）；空列表回退到
/// [`DEFAULT_HANDSHAKE_TRANSPORTS`]。对标 Python `enforce_polling_first`。
pub fn enforce_polling_first(transports: &[String]) -> Vec<String> {
    if transports.is_empty() {
        return DEFAULT_HANDSHAKE_TRANSPORTS
            .iter()
            .map(|s| s.to_string())
            .collect();
    }
    let mut out: Vec<String> = Vec::with_capacity(transports.len() + 1);
    out.push("polling".to_string());
    for t in transports {
        if t != "polling" && !out.iter().any(|x| x == t) {
            out.push(t.clone());
        }
    }
    out
}

/// 应用 polling-first 守卫 / Apply the polling-first guard。
///
/// `None`（未指定）→ [`DEFAULT_HANDSHAKE_TRANSPORTS`]；否则经 [`enforce_polling_first`] 规整。
/// 若调用方传入 WS-only（首位非 polling 或不含 polling），会被强制改为 polling-first，避免
/// WS-only 直连绕过服务端版本校验。对标 Python `apply_polling_first_guard`（其额外记 warn 日志；
/// 本实现不引入日志依赖，调用方可对比返回值与入参以判定是否被修正）。
pub fn apply_polling_first_guard(transports: Option<&[String]>) -> Vec<String> {
    match transports {
        None => DEFAULT_HANDSHAKE_TRANSPORTS
            .iter()
            .map(|s| s.to_string())
            .collect(),
        Some(ts) => enforce_polling_first(ts),
    }
}

/// 从服务端拒绝 body 提取 4008 负载 / Extract a 4008 payload from a server rejection body。
///
/// `body` 解析为 flat [`ErrorPayload`] 且 `code == 4008`（[`ErrorCode::ProtocolVersionMismatch`]）
/// 时返回 `Some`；否则 `None`（非 4008 / 非法 JSON → 调用方应原样上抛原始错误，绝不误判）。
///
/// 对标 Python `extract_4008_payload`（其从异常链取已解析 body；Rust 侧输入为 tf-rust-socketio
/// 0.8.0 `Error::IncompleteResponseFromEngineIo(HttpErrorWithBody { body, .. })` 的 `body` 串）。
pub fn extract_4008_payload(body: &str) -> Option<ErrorPayload> {
    let payload: ErrorPayload = serde_json::from_str(body).ok()?;
    if payload.code == i64::from(ErrorCode::ProtocolVersionMismatch.code()) {
        Some(payload)
    } else {
        None
    }
}

/// 将 4008 [`ErrorPayload`] 映射为 [`ProtocolVersionError`] / Map a 4008 payload to the typed error。
///
/// 透传顶层平铺的 4 个版本字段与 `message`。对标 Python `build_protocol_version_error`。
pub fn build_protocol_version_error(payload: &ErrorPayload) -> ProtocolVersionError {
    ProtocolVersionError {
        client_version: payload.client_version.clone(),
        server_version: payload.server_version.clone(),
        min_supported: payload.min_supported.clone(),
        max_supported: payload.max_supported.clone(),
        message: payload.message.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(u: &str) -> std::collections::HashMap<String, String> {
        Url::parse(u).unwrap().query_pairs().into_owned().collect()
    }

    #[test]
    fn test_build_handshake_url_injects_and_preserves() {
        // 无 query → 注入
        let out = build_handshake_url("http://h:3000/", "0.2.0").unwrap();
        assert_eq!(pairs(&out).get("a2c_version").unwrap(), "0.2.0");

        // 有 query → 保留其它 pair + 注入
        let out = build_handshake_url("http://h:3000/smcp?foo=bar&x=1", "0.2.0").unwrap();
        let p = pairs(&out);
        assert_eq!(p.get("a2c_version").unwrap(), "0.2.0");
        assert_eq!(p.get("foo").unwrap(), "bar");
        assert_eq!(p.get("x").unwrap(), "1");
    }

    #[test]
    fn test_build_handshake_url_drops_caller_supplied_version() {
        // 调用方自带 a2c_version 必须被丢弃、替换为权威值，且只剩一个
        let out = build_handshake_url("http://h/?a2c_version=9.9.9&keep=1", "0.2.0").unwrap();
        assert_eq!(pairs(&out).get("a2c_version").unwrap(), "0.2.0");
        assert_eq!(pairs(&out).get("keep").unwrap(), "1");
        let count = Url::parse(&out)
            .unwrap()
            .query_pairs()
            .filter(|(k, _)| k == "a2c_version")
            .count();
        assert_eq!(count, 1, "a2c_version 不可重复");
    }

    #[test]
    fn test_build_handshake_url_invalid() {
        assert!(build_handshake_url("not a url", "0.2.0").is_err());
    }

    #[test]
    fn test_enforce_polling_first() {
        assert_eq!(
            enforce_polling_first(&["websocket".to_string()]),
            vec!["polling".to_string(), "websocket".to_string()]
        );
        assert_eq!(
            enforce_polling_first(&["polling".to_string(), "websocket".to_string()]),
            vec!["polling".to_string(), "websocket".to_string()]
        );
        // ws-first → 被前置；去重
        assert_eq!(
            enforce_polling_first(&[
                "websocket".to_string(),
                "polling".to_string(),
                "websocket".to_string()
            ]),
            vec!["polling".to_string(), "websocket".to_string()]
        );
        // 空 → 默认
        assert_eq!(
            enforce_polling_first(&[]),
            vec!["polling".to_string(), "websocket".to_string()]
        );
    }

    #[test]
    fn test_apply_polling_first_guard() {
        // None → 默认 polling-first
        assert_eq!(
            apply_polling_first_guard(None),
            vec!["polling".to_string(), "websocket".to_string()]
        );
        // WS-only → 强制改 polling-first
        let ws_only = vec!["websocket".to_string()];
        assert_eq!(
            apply_polling_first_guard(Some(&ws_only)),
            vec!["polling".to_string(), "websocket".to_string()]
        );
    }

    #[test]
    fn test_extract_4008_payload() {
        // 4008 → Some，且 4 版本字段透传
        let body = r#"{"code":4008,"message":"Protocol version mismatch","server_version":"0.2.0","client_version":"0.1.0","min_supported":"0.2.0","max_supported":"0.2.999"}"#;
        let p = extract_4008_payload(body).expect("应识别 4008");
        assert_eq!(p.code, 4008);
        assert_eq!(p.server_version.as_deref(), Some("0.2.0"));
        assert_eq!(p.client_version.as_deref(), Some("0.1.0"));

        // code=400（缺失/非法）→ None（不误判为版本错误）
        assert!(extract_4008_payload(r#"{"code":400,"message":"Missing a2c_version"}"#).is_none());
        // 其它协议码 → None
        assert!(extract_4008_payload(r#"{"code":404,"message":"nope"}"#).is_none());
        // 非法 JSON → None
        assert!(extract_4008_payload("not json").is_none());
    }

    #[test]
    fn test_build_protocol_version_error() {
        let body = r#"{"code":4008,"message":"Protocol version mismatch","server_version":"0.2.0","client_version":"0.1.0","min_supported":"0.2.0","max_supported":"0.2.999"}"#;
        let payload = extract_4008_payload(body).unwrap();
        let err = build_protocol_version_error(&payload);
        assert_eq!(err.client_version.as_deref(), Some("0.1.0"));
        assert_eq!(err.server_version.as_deref(), Some("0.2.0"));
        assert_eq!(err.min_supported.as_deref(), Some("0.2.0"));
        assert_eq!(err.max_supported.as_deref(), Some("0.2.999"));
        // Display 含诊断
        let s = err.to_string();
        assert!(s.contains("Protocol version mismatch"));
        assert!(s.contains("client=0.1.0"));
        assert!(s.contains("server=0.2.0"));
    }

    #[test]
    fn test_version_mismatch_roundtrip() {
        use crate::ProtocolVersion;
        let client = ProtocolVersion::new(0, 1, 0);
        let server = ProtocolVersion::new(0, 2, 0);
        let payload = ErrorPayload::version_mismatch(&client, &server);
        assert_eq!(payload.code, 4008);
        assert_eq!(payload.server_version.as_deref(), Some("0.2.0"));
        assert_eq!(payload.client_version.as_deref(), Some("0.1.0"));
        assert_eq!(payload.min_supported.as_deref(), Some("0.2.0"));
        assert_eq!(payload.max_supported.as_deref(), Some("0.2.999"));
        // flat 序列化：顶层含 4 字段、无嵌套 envelope
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["code"], 4008);
        assert_eq!(json["server_version"], "0.2.0");
        assert!(json.get("error").is_none());
        // 反向：序列化串 → extract_4008_payload 读回，逐字段一致
        let s = serde_json::to_string(&payload).unwrap();
        let back = extract_4008_payload(&s).unwrap();
        assert_eq!(back, payload);
    }
}
