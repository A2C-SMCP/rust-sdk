/*!
* 文件名: auth_error
* 作者: JQQ
* 创建日期: 2026/06/08
* 最后修改日期: 2026/06/08
* 版权: 2023 JQQ. All rights reserved.
* 依赖: rmcp, serde_json, smcp
* 描述: MCP 上游授权错误归类与透传（AUTH-01 #23）/ MCP upstream authorization error mapping.
*/
//! MCP 上游授权错误归类与脱敏透传（AUTH-01 #23）。
//!
//! 协议 `error-handling.md` §403：Computer 调 MCP 工具因上游授权失败时，**以
//! `CallToolResult(isError=true, meta={error_code, mcp_server, auth_hint})` 返回**（结果级 `meta`，
//! wire `_meta`），使 Agent 区分「工具坏了」与「需用户授权」——**不**走路由级 flat ErrorPayload。
//!
//! 本模块为**纯函数**（无 I/O，可独立单测）：
//! - [`classify_auth_error`](crate::mcp_clients::auth_error::classify_auth_error)：按 §423 决策表把上游错误（现仅字符串化于 [`MCPClientError`]）尽力归类为
//!   `4006`（需授权）/ `4007`（授权失败）/ `None`（非授权，保持通用错误路径）。仅 transport 级 `Err`，
//!   不解析 `Ok(CallToolResult{isError:true})` 的 content 文本（best-effort 边界）。
//! - [`sanitize_auth_hint`](crate::mcp_clients::auth_error::sanitize_auth_hint)：按 §454「auth_hint 安全边界」MUST NOT 清单脱敏（删敏感键 + URL 去 query/fragment）。
//! - [`build_default_auth_hint`](crate::mcp_clients::auth_error::build_default_auth_hint) / [`build_auth_error_result`](crate::mcp_clients::auth_error::build_auth_error_result)：构造协议形状的授权错误结果。
//!
//! Python 参考实现**不存在**（仅有 `ErrorCode` 枚举），Rust 领先实现，依据为协议决策表。

use rmcp::model::{CallToolResult, Content, Meta};
use rmcp::transport::streamable_http_client::StreamableHttpError;
use serde_json::{json, Value};
use smcp::tool_meta::{AUTH_ERROR_CODE_KEY, AUTH_HINT_KEY, AUTH_MCP_SERVER_KEY};
use smcp::ErrorCode;

use super::model::MCPClientError;

/// §423「授权失败 / 权限不足 / 凭证失效」→ `4007` 的判别子串（小写比较）。
/// 403 / scope 不足 / token 过期+刷新失败 / revoked / invalid_grant 等「曾授权但当前不可用」。
const FAILED_MARKERS: &[&str] = &[
    "403",
    "forbidden",
    "insufficient scope",
    "insufficient_scope",
    "access denied",
    "permission denied",
    "invalid_grant",
    "token expired",
    "expired token",
    "refresh failed",
    "refresh_token",
    "revoked",
];

/// §423「需要授权 / 未认证」→ `4006` 的判别子串（小写比较）。
/// 401 / 无凭证 / 未登录等「从未授权或需重新授权」。
const REQUIRED_MARKERS: &[&str] = &[
    "401",
    "unauthorized",
    "unauthenticated",
    "not authenticated",
    "authentication required",
    "login required",
    "missing credentials",
    "no credentials",
];

/// 泛授权关键词：命中但不可判别 4006 vs 4007 时，按 §439-440 兜底 `4006`（提示重走授权流程稳妥）。
const AMBIGUOUS_MARKERS: &[&str] = &["authoriz", "oauth", "www-authenticate", "bearer", "scope"];

/// §454 auth_hint MUST NOT 暴露的敏感键（小写**子串**匹配，宁可错删不可泄漏）。
/// 覆盖访问凭证 / OAuth 流程参数 / 客户端凭证 / 用户敏感数据。
const SENSITIVE_HINT_KEY_MARKERS: &[&str] = &[
    "token",
    "secret",
    "credential",
    "password",
    "passwd",
    "client_id",
    "client_assertion",
    "assertion",
    "code_verifier",
    "code_challenge",
    // `"code"` 子串亦会删 device-flow `user_code`/`verification_code`（有意，§461 禁 `code`；
    // 「宁可错删」）。未来若支持设备流 hint，需在此为 user_code 单独豁免。
    "code",
    "state",
    "nonce",
    "otp",
    "totp",
    "api_key",
    "apikey",
];

/// 把上游 MCP 工具调用错误尽力归类为协议授权错误码 / Best-effort classify an upstream tool-call
/// error into a protocol authorization code。
///
/// 判别顺序：先 [`classify_structured`]（保型 downcast，覆盖最规范的 `401 + WWW-Authenticate`，
/// #150），再退化到 `Display` 小写子串 marker 表（覆盖 SSE 自建错误串 / `McpError` 等非保型路径）。
/// `Some(4006/4007)` → 调用方应改以授权 `CallToolResult` 透传；`None` → 非授权（5xx / 网络 / 其它，
/// §435），保持通用错误路径。
///
/// **边界（best-effort）**：仅作用于 `Err(MCPClientError)`（transport 级）；上游若把授权失败作为
/// `Ok(CallToolResult{isError:true})` 返回（stdio/SSE 常见），不解析其 content 文本、不重分类——按
/// 原样透传。`isError` 内容解析留后续。
pub fn classify_auth_error(err: &MCPClientError) -> Option<ErrorCode> {
    // #150：保型优先——结构化 downcast 命中 `AuthRequired` → 4006（字符串表对 "Auth required" 永不命中）。
    if let Some(code) = classify_structured(err) {
        return Some(code);
    }

    let msg = err.to_string().to_lowercase();

    // 先判「授权失败（4007）」：403/scope 等先于泛 auth 命中，避免被兜底成 4006。
    if FAILED_MARKERS.iter().any(|m| msg.contains(m)) {
        return Some(ErrorCode::ToolAuthorizationFailed);
    }
    if REQUIRED_MARKERS.iter().any(|m| msg.contains(m)) {
        return Some(ErrorCode::ToolAuthorizationRequired);
    }
    // 泛授权关键词但不可判别 → §439-440 兜底 4006。
    if AMBIGUOUS_MARKERS.iter().any(|m| msg.contains(m)) {
        return Some(ErrorCode::ToolAuthorizationRequired);
    }
    None
}

/// 结构化判别（#150）：对保型 [`MCPClientError::ToolCallError`] 直接按型匹配，**不依赖 rmcp `Display`
/// 字面量**。
///
/// 仅处理最规范的 `401 + WWW-Authenticate`：rmcp 在 `error_for_status()` 之前短路成
/// `StreamableHttpError::AuthRequired`，其 `Display` 为无状态码的 `"Auth required"`，三张字符串 marker
/// 表全不命中（[`classify_auth_error`] 的退化路径对此形态**永不**产出 4006，违反 §425/§432 MUST）。
/// 本函数对该保型错误做 downcast 命中 → `4006`（与裸 401 一致）。
///
/// 其余形态（`403` / 裸 `401` 经 `error_for_status` 携状态码；`McpError`；非流式 HTTP 传输）回退到
/// [`classify_auth_error`] 的字符串 marker 路径，故此函数仅特判 `AuthRequired` 一种变体。
fn classify_structured(err: &MCPClientError) -> Option<ErrorCode> {
    let MCPClientError::ToolCallError(service_err) = err else {
        return None;
    };
    // 仅 transport 发送错误携带可 downcast 的底层类型；`McpError` / `Timeout` 等无保型底层 → 回退字符串路径。
    let rmcp::ServiceError::TransportSend(dte) = service_err else {
        return None;
    };
    // 直接对内层 `Box<dyn Error + Send + Sync>` 做 downcast，**免绑 transport `TypeId`**——
    // `DynamicTransportError::downcast::<T, R>` 还要校验 transport 具体类型，过严且耦合；此处只需知道
    // 底层是否为 streamable-HTTP 的 `AuthRequired`。
    dte.error
        .downcast_ref::<StreamableHttpError<reqwest::Error>>()
        .and_then(|she| match she {
            StreamableHttpError::AuthRequired(_) => Some(ErrorCode::ToolAuthorizationRequired),
            _ => None,
        })
}

/// 按 §454 脱敏 auth_hint：递归删敏感键、URL 去 query/fragment / sanitize an auth hint per the
/// §454 boundary（drop sensitive keys, strip URL query/fragment）。
///
/// 安全关键：[`build_auth_error_result`] 对任何 hint 强制经此，作为唯一注入门，杜绝凭证经 auth_hint
/// 泄漏给 Agent。键匹配为小写**子串**（宁可错删）；字符串值中**任意内嵌**的 `http(s)://` URL 均截去
/// `?` query 与 `#` fragment（散文保留），杜绝授权 URL 携敏感参数经此外泄（§463）。
pub fn sanitize_auth_hint(raw: &Value) -> Value {
    match raw {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                let kl = k.to_lowercase();
                if SENSITIVE_HINT_KEY_MARKERS.iter().any(|m| kl.contains(m)) {
                    continue; // 删除敏感键
                }
                out.insert(k.clone(), sanitize_auth_hint(v));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(sanitize_auth_hint).collect()),
        Value::String(s) => Value::String(sanitize_urls_in_string(s)),
        other => other.clone(),
    }
}

/// 扫描字符串中**任意内嵌**的 `http(s)://` URL token（以空白分界），截去其 `?` query 与 `#`
/// fragment，scheme 间的散文原样保留 / strip query+fragment from *every* embedded `http(s)://` URL
/// token, leaving surrounding prose intact。
///
/// 整串裸 URL 是其特例；散文型 `"请访问 https://idp/cb?code=X 完成"` 中的授权 URL 同样被剥离（§463
/// 禁止 auth_hint 携任何含敏感 query 的完整 URL）。implicit-flow 把 token 放 fragment，故 `#` 也截。
fn sanitize_urls_in_string(s: &str) -> String {
    // 快速路径：无 scheme → 散文零开销原样返回 / fast path: no scheme, return prose untouched。
    if !s.contains("://") {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = next_url_scheme(rest) {
        out.push_str(&rest[..pos]);
        let after = &rest[pos..];
        // URL token 取到下一个空白为止 / the URL token runs up to the next whitespace。
        let end = after.find(char::is_whitespace).unwrap_or(after.len());
        out.push_str(&strip_query_fragment(&after[..end]));
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

/// 下一个 `http://` / `https://` 出现的字节位置（取最早者）/ byte index of the next URL scheme。
fn next_url_scheme(s: &str) -> Option<usize> {
    match (s.find("http://"), s.find("https://")) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// 截去 URL 的 `?` query 与 `#` fragment（取最早者之前）/ truncate a URL before its `?` query or `#`
/// fragment。
fn strip_query_fragment(url: &str) -> String {
    let mut end = url.len();
    if let Some(i) = url.find('?') {
        end = end.min(i);
    }
    if let Some(i) = url.find('#') {
        end = end.min(i);
    }
    url[..end].to_string()
}

/// Computer 生成的**非敏感**默认 auth_hint（`action` + `message`）/ a non-sensitive default hint。
///
/// `4006` → `user_authorization_required`（首次/重新授权）；`4007` → `token_refresh_required`
/// （重新授权 / 检查权限）。天然脱敏，缺失上游派生 hint 时满足最小可用 UX（§452）。
pub fn build_default_auth_hint(code: ErrorCode) -> Value {
    let (action, message) = match code {
        ErrorCode::ToolAuthorizationFailed => (
            "token_refresh_required",
            "上游授权已失效或权限不足，请重新授权或检查权限后重试。\
             / Upstream authorization failed or is insufficient; please re-authorize or check permissions.",
        ),
        // ToolAuthorizationRequired 及其它兜底
        _ => (
            "user_authorization_required",
            "此工具需要先在 Computer 宿主环境完成上游授权（如 OAuth 登录）后方可调用。\
             / This tool requires completing upstream authorization (e.g. OAuth login) on the Computer host first.",
        ),
    };
    json!({ "action": action, "message": message })
}

/// 构造协议形状的授权错误结果 / build the protocol-shaped authorization error `CallToolResult`。
///
/// `isError=true` + 人类可读 `content` + 结果级 `meta`（wire `_meta`）三键 `error_code`(整数) /
/// `mcp_server` / `auth_hint`。`hint` **强制**经 [`sanitize_auth_hint`]（唯一注入门，§454）。
pub fn build_auth_error_result(mcp_server: &str, code: ErrorCode, hint: Value) -> CallToolResult {
    let text = match code {
        ErrorCode::ToolAuthorizationFailed => {
            format!("调用 `{mcp_server}` 的工具因上游授权失败（凭证失效或权限不足）。")
        }
        _ => format!("调用 `{mcp_server}` 的工具需要先完成上游授权。"),
    };

    let mut result = CallToolResult::error(vec![Content::text(text)]);

    let mut meta = Meta::new();
    meta.insert(AUTH_ERROR_CODE_KEY.to_string(), Value::from(code.code()));
    meta.insert(
        AUTH_MCP_SERVER_KEY.to_string(),
        Value::String(mcp_server.to_string()),
    );
    meta.insert(AUTH_HINT_KEY.to_string(), sanitize_auth_hint(&hint));
    result.meta = Some(meta);

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── classify_auth_error（§423 决策表，尽力归类）─────────────────────────────
    #[test]
    fn test_classify_http_401_required_4006() {
        let e = MCPClientError::ConnectionError("HTTP error: 401 Unauthorized".to_string());
        assert_eq!(
            classify_auth_error(&e),
            Some(ErrorCode::ToolAuthorizationRequired)
        );
    }

    #[test]
    fn test_classify_http_403_failed_4007() {
        let e = MCPClientError::ConnectionError("HTTP error: 403 Forbidden".to_string());
        assert_eq!(
            classify_auth_error(&e),
            Some(ErrorCode::ToolAuthorizationFailed)
        );
    }

    #[test]
    fn test_classify_insufficient_scope_failed_4007() {
        let e = MCPClientError::ProtocolError("Call tool error: insufficient scope".to_string());
        assert_eq!(
            classify_auth_error(&e),
            Some(ErrorCode::ToolAuthorizationFailed)
        );
    }

    #[test]
    fn test_classify_invalid_grant_and_refresh_failed_4007() {
        for s in [
            "invalid_grant",
            "token refresh failed",
            "token expired",
            "revoked",
        ] {
            let e = MCPClientError::ProtocolError(format!("Call tool error: {s}"));
            assert_eq!(
                classify_auth_error(&e),
                Some(ErrorCode::ToolAuthorizationFailed),
                "signal {s:?} should map to 4007"
            );
        }
    }

    #[test]
    fn test_classify_unauthenticated_required_4006() {
        let e = MCPClientError::ProtocolError(
            "Call tool error: authentication required, please login".to_string(),
        );
        assert_eq!(
            classify_auth_error(&e),
            Some(ErrorCode::ToolAuthorizationRequired)
        );
    }

    #[test]
    fn test_classify_ambiguous_oauth_falls_back_to_4006() {
        // 泛 OAuth 关键词但无 401/403 → §439-440 兜底 4006。
        let e = MCPClientError::ProtocolError("OAuth authorization problem".to_string());
        assert_eq!(
            classify_auth_error(&e),
            Some(ErrorCode::ToolAuthorizationRequired)
        );
    }

    #[test]
    fn test_classify_5xx_and_network_are_not_auth() {
        for s in [
            "HTTP error: 500 Internal Server Error",
            "HTTP request failed: connection refused",
            "Timeout error: Tool execution timed out",
            "Tool execution failed: boom",
        ] {
            let e = MCPClientError::ConnectionError(s.to_string());
            assert_eq!(
                classify_auth_error(&e),
                None,
                "signal {s:?} must not be auth"
            );
        }
    }

    // ── classify_structured（#150 保型 downcast，免依赖 rmcp `Display` 字面量）──────
    /// 合成 `ToolCallError(TransportSend(StreamableHttpError::AuthRequired(_)))` 直接验证结构化路径
    /// 命中 4006——亚毫秒级回归栅栏，不依赖真实传输/socket（端到端由 `auth_error_real_transport.rs`
    /// 覆盖；此单测把"结构化路径命中"的判据下沉到 unit 层，且对 rmcp 上游改名/重构会编译红而非静默退化）。
    #[test]
    fn test_classify_structured_auth_required_reaches_4006() {
        use rmcp::transport::streamable_http_client::{AuthRequiredError, StreamableHttpError};
        use rmcp::transport::{DynamicTransportError, StreamableHttpClientTransport};
        use rmcp::RoleClient;

        // 401 + WWW-Authenticate → rmcp 短路成 AuthRequired（Display 无状态码，字符串表永不命中）。
        let she = StreamableHttpError::AuthRequired(AuthRequiredError {
            www_authenticate_header: "Bearer realm=\"mcp\"".to_string(),
        });
        let dte = DynamicTransportError::new::<StreamableHttpClientTransport<reqwest::Client>, RoleClient>(
            she,
        );
        let err = MCPClientError::ToolCallError(rmcp::ServiceError::TransportSend(dte));
        assert_eq!(
            classify_auth_error(&err),
            Some(ErrorCode::ToolAuthorizationRequired),
            "401 + WWW-Authenticate via structured downcast must reach 4006"
        );
    }

    /// 非 `AuthRequired` 的保型 streamable-HTTP transport 错误 → 结构化路径不命中 → 退到字符串 fallback；
    /// 该 Display（"Server does not support SSE"）无 marker → `None`。
    #[test]
    fn test_classify_structured_non_auth_required_falls_through() {
        use rmcp::transport::streamable_http_client::StreamableHttpError;
        use rmcp::transport::{DynamicTransportError, StreamableHttpClientTransport};
        use rmcp::RoleClient;

        let she = StreamableHttpError::<reqwest::Error>::ServerDoesNotSupportSse;
        let dte = DynamicTransportError::new::<StreamableHttpClientTransport<reqwest::Client>, RoleClient>(
            she,
        );
        let err = MCPClientError::ToolCallError(rmcp::ServiceError::TransportSend(dte));
        assert_eq!(
            classify_auth_error(&err),
            None,
            "non-AuthRequired transport error must not be classified as auth"
        );
    }

    // ── sanitize_auth_hint（§454 安全边界）──────────────────────────────────────
    #[test]
    fn test_sanitize_drops_sensitive_keys_and_strips_url_query() {
        let raw = json!({
            "action": "user_authorization_required",
            "message": "请登录 GitHub 后重试",
            "login_url": "https://example.com/login?code=SECRET&state=xyz#access_token=AAA",
            "access_token": "AAA.BBB.CCC",
            "refresh_token": "rrr",
            "code": "abc",
            "state": "xyz",
            "client_secret": "shh",
            "nested": { "client_id": "cid", "note": "keep me", "bearer_token": "zzz" }
        });
        let out = sanitize_auth_hint(&raw);

        // 敏感键全删（含嵌套）。
        for k in [
            "access_token",
            "refresh_token",
            "code",
            "state",
            "client_secret",
        ] {
            assert!(out.get(k).is_none(), "sensitive key {k:?} must be dropped");
        }
        let nested = out.get("nested").unwrap();
        assert!(nested.get("client_id").is_none());
        assert!(nested.get("bearer_token").is_none());
        assert_eq!(nested.get("note").and_then(|v| v.as_str()), Some("keep me"));

        // 允许字段保留。
        assert_eq!(
            out.get("action").and_then(|v| v.as_str()),
            Some("user_authorization_required")
        );
        assert!(out.get("message").is_some());

        // URL 去 query + fragment。
        let url = out.get("login_url").and_then(|v| v.as_str()).unwrap();
        assert_eq!(url, "https://example.com/login");
        assert!(!url.contains('?') && !url.contains('#') && !url.contains("access_token"));
    }

    #[test]
    fn test_sanitize_keeps_prose_message_untouched() {
        let raw = json!({ "message": "Need help? Visit our docs." });
        let out = sanitize_auth_hint(&raw);
        assert_eq!(
            out.get("message").and_then(|v| v.as_str()),
            Some("Need help? Visit our docs.")
        );
    }

    #[test]
    fn test_sanitize_strips_url_embedded_in_prose() {
        // 散文型 message 内嵌授权 URL：URL 的 ?code=/&state= 必须被剥离，散文保留（§463）。
        let raw = json!({
            "message": "请访问 https://idp.example.com/cb?code=AUTHCODE&state=S 完成授权"
        });
        let out = sanitize_auth_hint(&raw);
        let msg = out.get("message").and_then(|v| v.as_str()).unwrap();
        assert_eq!(msg, "请访问 https://idp.example.com/cb 完成授权");
        assert!(!msg.contains("AUTHCODE") && !msg.contains("code=") && !msg.contains('?'));
    }

    #[test]
    fn test_sanitize_strips_implicit_flow_fragment() {
        // implicit-flow 把 token 放 URL fragment（#access_token=）→ 必须截 `#` 之后。
        let raw = json!({ "redirect": "https://app.example.com/cb#access_token=AAA.BBB" });
        let out = sanitize_auth_hint(&raw);
        let url = out.get("redirect").and_then(|v| v.as_str()).unwrap();
        assert_eq!(url, "https://app.example.com/cb");
        assert!(!url.contains('#') && !url.contains("access_token"));
    }

    // ── build_default_auth_hint ────────────────────────────────────────────────
    #[test]
    fn test_default_hint_actions_per_code_and_no_sensitive() {
        let h6 = build_default_auth_hint(ErrorCode::ToolAuthorizationRequired);
        assert_eq!(
            h6.get("action").and_then(|v| v.as_str()),
            Some("user_authorization_required")
        );
        assert!(h6.get("message").is_some());

        let h7 = build_default_auth_hint(ErrorCode::ToolAuthorizationFailed);
        assert_eq!(
            h7.get("action").and_then(|v| v.as_str()),
            Some("token_refresh_required")
        );

        // 默认 hint 经脱敏不应丢字段（天然非敏感）。
        assert_eq!(sanitize_auth_hint(&h6), h6);
    }

    // ── build_auth_error_result（协议形状 + 强制脱敏）────────────────────────────
    #[test]
    fn test_build_result_shape_4006() {
        let hint = build_default_auth_hint(ErrorCode::ToolAuthorizationRequired);
        let r = build_auth_error_result("github-mcp", ErrorCode::ToolAuthorizationRequired, hint);

        assert_eq!(r.is_error, Some(true));
        assert!(!r.content.is_empty());

        let meta = r.meta.as_ref().expect("meta present");
        assert_eq!(
            meta.get(AUTH_ERROR_CODE_KEY).and_then(|v| v.as_i64()),
            Some(4006)
        );
        assert_eq!(
            meta.get(AUTH_MCP_SERVER_KEY).and_then(|v| v.as_str()),
            Some("github-mcp")
        );
        let hint = meta.get(AUTH_HINT_KEY).expect("auth_hint present");
        assert_eq!(
            hint.get("action").and_then(|v| v.as_str()),
            Some("user_authorization_required")
        );
    }

    #[test]
    fn test_build_result_force_sanitizes_caller_hint() {
        // 即便调用方塞入带敏感字段的 hint，build 也必须经 sanitize 注入门。
        let dirty = json!({ "message": "x", "access_token": "LEAK", "code": "c" });
        let r = build_auth_error_result("srv", ErrorCode::ToolAuthorizationFailed, dirty);

        assert_eq!(meta_error_code(&r), 4007);
        let hint = r.meta.as_ref().unwrap().get(AUTH_HINT_KEY).unwrap();
        assert!(
            hint.get("access_token").is_none(),
            "token must not leak via hint"
        );
        assert!(hint.get("code").is_none());
        assert_eq!(hint.get("message").and_then(|v| v.as_str()), Some("x"));
    }

    fn meta_error_code(r: &CallToolResult) -> i64 {
        r.meta
            .as_ref()
            .unwrap()
            .get(AUTH_ERROR_CODE_KEY)
            .and_then(|v| v.as_i64())
            .unwrap()
    }
}
