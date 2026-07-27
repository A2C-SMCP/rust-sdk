/*!
* 文件名: skill_consume.rs
* 作者: JQQ
* 创建日期: 2026/06/02
* 最后修改日期: 2026/06/02
* 版权: 2023 JQQ. All rights reserved.
* 依赖: smcp, serde_json
* 描述: Agent SKILL 消费响应解析纯函数 / Pure SKILL-consume response parsers
*/

//! Agent SKILL 通道消费响应解析（`client:get_skills` / `client:get_skill` 的 ack）。
//! 对标 Python `a2c_smcp/agent/client.py::get_skills` / `get_skill` 的响应处理段。
//!
//! 抽成无 I/O 纯函数，供 async/sync Agent 共享并可独立单测（不依赖 Socket.IO 传输）：
//! 统一 `raise_for_error_payload`（flat ErrorPayload → 协议错误）+ `req_id` 回显校验
//! （与 `get_tools` / `get_desktop` 同款约定）+ 结构反序列化。
//!
//! `get_skill` 的 body/blob_handle 分支：inline `body`（文本 MIME）由 [`smcp::GetSkillRet::resource`]
//! 返回 [`smcp::SkillResource::Inline`] 直接可读。注：文本 MIME 的 `blob_handle` 自动 drain 回填 `body`
//! 由 AGT-03 #38 在 [`crate::async_agent::AsyncSmcpAgent::get_skill`] 接线（此纯解析器只还原线上形态，
//! 不发起 I/O）；二进制 `blob_handle` 仍**原样保留**，由调用方经 `client:get_blob` 自取字节。

use serde_json::Value;
use smcp::{A2CSkillRef, GetSkillRet};

use crate::error::Result;
use crate::protocol_error::raise_for_error_payload;
use crate::response::ensure_req_id;

/// 解析 `client:get_skills` 响应为 SKILL 引用列表 / parse a `client:get_skills` ack into skill refs。
///
/// 流程：flat ErrorPayload（如 `4014`）→ 协议错误；`req_id` 回显校验；缺失 `skills` 视作空列表
/// （对齐 Python `response.get("skills", [])`，区别于 `get_tools` 的 `unwrap_or_default`）。
pub fn parse_get_skills_response(
    response: &Value,
    expected_req_id: &str,
) -> Result<Vec<A2CSkillRef>> {
    raise_for_error_payload(response)?;
    ensure_req_id(response, expected_req_id)?;
    let skills = match response.get("skills") {
        Some(v) => serde_json::from_value(v.clone())?,
        None => Vec::new(),
    };
    Ok(skills)
}

/// 解析 `client:get_skill` 响应为资源载体 / parse a `client:get_skill` ack into the resource carrier。
///
/// 流程：flat ErrorPayload（如 `4016` / `4017`）→ 协议错误；`req_id` 回显校验；整包反序列化为
/// [`GetSkillRet`]（`body` 与 `blob_handle` 恰一存在，由 [`GetSkillRet::resource`] 校验访问）。
/// 不在此处强制 exactly-one——以 Computer 端为权威，消费方按需经 `.resource()` 校验。
pub fn parse_get_skill_response(response: &Value, expected_req_id: &str) -> Result<GetSkillRet> {
    raise_for_error_payload(response)?;
    ensure_req_id(response, expected_req_id)?;
    let ret: GetSkillRet = serde_json::from_value(response.clone())?;
    Ok(ret)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::SmcpAgentError;
    use serde_json::json;
    use smcp::SkillResource;

    // ── get_skills ─────────────────────────────────────────────────────────
    #[test]
    fn test_get_skills_happy_path() {
        let resp = json!({
            "skills": [
                {
                    "name": "my-helper",
                    "source": "user",
                    "path": "/abs/skills/my-helper",
                    "description": "A helper"
                },
                {
                    "name": "acme:audit",
                    "source": "marketplace:acme-skills",
                    "path": "/abs/skills/acme-audit",
                    "description": "Audit"
                }
            ],
            "req_id": "R1"
        });
        let skills = parse_get_skills_response(&resp, "R1").unwrap();
        assert_eq!(skills.len(), 2);
        assert_eq!(skills[0].name, "my-helper");
        assert_eq!(skills[1].source, "marketplace:acme-skills");
    }

    #[test]
    fn test_get_skills_missing_skills_is_empty() {
        // 对齐 Python response.get("skills", [])：缺 skills 键 → 空列表，而非反序列化报错
        let resp = json!({ "req_id": "R1" });
        let skills = parse_get_skills_response(&resp, "R1").unwrap();
        assert!(skills.is_empty());
    }

    #[test]
    fn test_get_skills_flat_error_4014_raises_protocol_error() {
        let resp = json!({
            "code": 4014,
            "message": "mcp server not found",
            "mcp_server": "tfrobot-tools"
        });
        let err = parse_get_skills_response(&resp, "R1").unwrap_err();
        // flat ErrorPayload → 协议错误（经 From<SmcpProtocolError> 转 SmcpAgentError）
        assert!(matches!(err, SmcpAgentError::Protocol(_)), "got {err:?}");
    }

    #[test]
    fn test_get_skills_req_id_mismatch() {
        let resp = json!({ "skills": [], "req_id": "OTHER" });
        let err = parse_get_skills_response(&resp, "R1").unwrap_err();
        assert!(matches!(err, SmcpAgentError::ReqIdMismatch { .. }));
    }

    #[test]
    fn test_get_skills_missing_req_id_is_internal_error() {
        let resp = json!({ "skills": [] });
        let err = parse_get_skills_response(&resp, "R1").unwrap_err();
        assert!(matches!(err, SmcpAgentError::Internal(_)), "got {err:?}");
    }

    // ── get_skill ──────────────────────────────────────────────────────────
    #[test]
    fn test_get_skill_inline_body_text_mime_directly_readable() {
        // 文本 MIME inline body：body 直接可读（本期范围核心）
        let resp = json!({
            "name": "my-helper",
            "rel_path": "SKILL.md",
            "mime_type": "text/markdown",
            "total_size": 11,
            "sha256": "deadbeef",
            "body": "# Hello\nhi",
            "req_id": "R2"
        });
        let ret = parse_get_skill_response(&resp, "R2").unwrap();
        assert!(ret.is_exclusive());
        match ret.resource().unwrap() {
            SkillResource::Inline(body) => assert_eq!(body, "# Hello\nhi"),
            other => panic!("expected inline, got {other:?}"),
        }
    }

    #[test]
    fn test_get_skill_blob_handle_branch_preserved() {
        // blob_handle 分支：原样保留（drain 回填留待 AGT-03 / #38），不在本期触发拉取
        let resp = json!({
            "name": "big-skill",
            "rel_path": "assets/logo.png",
            "mime_type": "image/png",
            "total_size": 99999,
            "sha256": "cafef00d",
            "blob_handle": "blob:abc123",
            "req_id": "R2"
        });
        let ret = parse_get_skill_response(&resp, "R2").unwrap();
        assert!(ret.is_exclusive());
        assert_eq!(ret.blob_handle.as_deref(), Some("blob:abc123"));
        assert!(ret.body.is_none());
        match ret.resource().unwrap() {
            SkillResource::Blob(h) => assert_eq!(h, "blob:abc123"),
            other => panic!("expected blob, got {other:?}"),
        }
    }

    #[test]
    fn test_get_skill_flat_error_4017_raises_with_reason() {
        let resp = json!({
            "code": 4017,
            "message": "resource not accessible",
            "details": { "reason": "traversal", "rel_path": "../etc/passwd" }
        });
        let err = parse_get_skill_response(&resp, "R2").unwrap_err();
        match err {
            SmcpAgentError::Protocol(p) => {
                assert_eq!(p.code, 4017);
                assert_eq!(p.reason.as_deref(), Some("traversal"));
            }
            other => panic!("expected protocol error, got {other:?}"),
        }
    }

    #[test]
    fn test_get_skill_req_id_mismatch() {
        let resp = json!({ "name": "x", "body": "y", "req_id": "OTHER" });
        let err = parse_get_skill_response(&resp, "R2").unwrap_err();
        assert!(matches!(err, SmcpAgentError::ReqIdMismatch { .. }));
    }
}
