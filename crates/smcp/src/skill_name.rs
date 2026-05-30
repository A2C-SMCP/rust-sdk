//! SKILL 命名合成与 lexer（v0.2.1 协议，裸名模型）/ SKILL name synthesis & lexer。
//!
//! 协议依据 / Protocol: a2c-smcp-protocol `skill.md` §1（命名）；`error-handling.md` §4016。
//! 对标 Python 参考实现 / Mirrors the Python reference: `a2c_smcp/computer/skills/naming.py`。
//!
//! A2C-SMCP 用**全局唯一的合成 name** 作为协议主键。自 0.2.1 起 name **跨工具对齐裸名**
//! （放弃旧版「强制前缀化」），按 source 分三形态、靠**段数**消歧：
//!
//! | Source       | name 形态                  | 段数 |
//! |--------------|----------------------------|------|
//! | user         | `<skill>`                  | 1    |
//! | marketplace  | `<plugin>:<skill>`         | 2    |
//! | mcp          | `mcp:<server>:<skill>`     | 3    |
//!
//! - `:` 是协议层 reserved separator；`mcp` 是唯一保留字面首段前缀的 source（与 2 段 marketplace 区分）。
//! - 字符集 / charsets（skill.md §1.4）：
//!     - `<skill>` / `<plugin>` 段为**严格 kebab**（`[a-z0-9]` + 单连字符分隔，不以 `-` 始末、无连续 `--`、长 1–64）。
//!     - mcp `<server>` 段为规范化字符集 `[A-Za-z0-9_-]`（大小写保留，长 1–64）。
//! - 非法 name → [`SkillNameError`]，由 `client:get_skill` 处理器映射为协议 `4016`。

use crate::ErrorCode;

/// 段最大长度（skill.md §1.4：各段 1–64）/ Max per-segment length.
pub const MAX_SEGMENT_LEN: usize = 64;

/// 协议层 reserved separator / Protocol-reserved separator.
pub const SEPARATOR: char = ':';

/// mcp source 专属字面首段 / Literal leading segment reserved for the mcp source.
pub const MCP_SEGMENT: &str = "mcp";

/// SKILL name 的 source 形态 / The source shape of a SKILL name。
///
/// 序列化为小写裸值（`"user"` / `"marketplace"` / `"mcp"`），对齐 Python `Literal` 取值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillNameKind {
    /// user 源：1 段裸名 `<skill>`。
    User,
    /// marketplace 源：2 段 `<plugin>:<skill>`。
    Marketplace,
    /// mcp 源：3 段 `mcp:<server>:<skill>`。
    Mcp,
}

impl std::fmt::Display for SkillNameKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            SkillNameKind::User => "user",
            SkillNameKind::Marketplace => "marketplace",
            SkillNameKind::Mcp => "mcp",
        };
        f.write_str(s)
    }
}

/// lexer 解析结果 / Result of the name lexer。
///
/// `kind` 标明 source 形态；按形态填充对应段（`skill` 恒有；`plugin` 仅 marketplace；`server` 仅 mcp）。
/// Agent 仍 **MUST** 把 name 当不透明字符串——本结构仅供 Computer 内部（Registry / staging）使用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSkillName {
    /// 原始 name（回显）/ The raw name (echoed back).
    pub raw: String,
    /// source 形态 / source shape.
    pub kind: SkillNameKind,
    /// leaf skill 段（恒有）/ leaf skill segment (always present).
    pub skill: String,
    /// plugin 段（仅 marketplace）/ plugin segment (marketplace only).
    pub plugin: Option<String>,
    /// server 段（仅 mcp）/ server segment (mcp only).
    pub server: Option<String>,
}

/// SKILL name 格式非法 / SKILL name is malformed。
///
/// 映射协议 `4016 Invalid Skill Name`（`details.name` 透传非法 name）。用 [`SkillNameError::error_code`]
/// 取协议码、[`SkillNameError::to_error_payload`] 直接构造 flat [`crate::ErrorPayload`]。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid skill name {name:?}: {reason}")]
pub struct SkillNameError {
    /// 触发错误的非法 name / The offending name.
    pub name: String,
    /// 人类可读的失败原因 / Human-readable failure reason.
    pub reason: String,
}

impl SkillNameError {
    fn new(name: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            reason: reason.into(),
        }
    }

    /// 映射的协议错误码（恒为 [`ErrorCode::SkillNameInvalid`] = `4016`）/ Mapped protocol error code.
    pub fn error_code(&self) -> ErrorCode {
        ErrorCode::SkillNameInvalid
    }

    /// 构造对应的 flat [`crate::ErrorPayload`]（`code=4016`，`details.name` = 非法 name）。
    ///
    /// 对齐协议 error-handling.md §4016：code-specific 字段 `name` 置于 `details` 子对象。
    pub fn to_error_payload(&self) -> crate::ErrorPayload {
        crate::ErrorPayload::new(i64::from(self.error_code().code()), self.to_string())
            .with_detail("name", self.name.clone())
    }
}

/// 段是否为严格 kebab 且长度合规 / Whether a segment is strict kebab within length bounds。
///
/// 等价正则 `^[a-z0-9]+(?:-[a-z0-9]+)*$` + 长度 ≤ [`MAX_SEGMENT_LEN`]：仅小写 alnum 与单连字符，
/// 不以 `-` 始末、无连续 `--`、非空。手写校验以避免引入 `regex` 依赖。
fn is_strict_kebab(segment: &str) -> bool {
    let bytes = segment.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_SEGMENT_LEN {
        return false;
    }
    if bytes[0] == b'-' || bytes[bytes.len() - 1] == b'-' {
        return false;
    }
    let mut prev_hyphen = false;
    for &b in bytes {
        match b {
            b'a'..=b'z' | b'0'..=b'9' => prev_hyphen = false,
            b'-' => {
                if prev_hyphen {
                    return false; // 连续 `--`
                }
                prev_hyphen = true;
            }
            _ => return false,
        }
    }
    true
}

/// mcp `<server>` 段是否合规 / Whether the mcp `<server>` segment is valid（`[A-Za-z0-9_-]{1,64}`）。
fn is_valid_mcp_server_segment(segment: &str) -> bool {
    let len = segment.len();
    if len == 0 || len > MAX_SEGMENT_LEN {
        return false;
    }
    segment
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// MCP `<server>` 段规范化 / Normalize the MCP `<server>` segment（非 `[A-Za-z0-9_-]` → `_`，大小写保留）。
///
/// 与 Claude Code `normalizeNameForMCP()` 通用规则等价；**不**实现其 `"claude.ai "` 前缀折叠特例
/// （Anthropic 平台特化，违反 A2C 协议中立性）。协议依据 skill.md §1.3。
///
/// 仅做字符替换，**不**校验结果长度；空串 / 超长由 [`synthesize_mcp_name`] 在合成时判废。
pub fn normalize_mcp_server_segment(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// SKILL name lexer：段数消歧 + 逐段字符集校验 / Lexer: segment-count disambiguation + per-segment charset。
///
/// 协议依据 skill.md §1.4 消歧规则：
/// - 段数 ∉ {1, 2, 3} → 非法
/// - 1 段 → user（缺 `:` 的裸名**合法**，不得因缺 `:` 报错）
/// - 2 段 → marketplace `<plugin>:<skill>`
/// - 3 段 → mcp，首段 **MUST** 字面 `mcp`
/// - 任一段不符字符集 → 非法
///
/// # Errors
/// name 格式非法时返回 [`SkillNameError`]（映射协议 `4016`）。
pub fn parse_skill_name(name: &str) -> Result<ParsedSkillName, SkillNameError> {
    let segments: Vec<&str> = name.split(SEPARATOR).collect();
    match segments.len() {
        1 => {
            let skill = segments[0];
            if !is_strict_kebab(skill) {
                return Err(SkillNameError::new(
                    name,
                    "user name must be a strict-kebab 1-segment bare name",
                ));
            }
            Ok(ParsedSkillName {
                raw: name.to_string(),
                kind: SkillNameKind::User,
                skill: skill.to_string(),
                plugin: None,
                server: None,
            })
        }
        2 => {
            let (plugin, skill) = (segments[0], segments[1]);
            if !is_strict_kebab(plugin) || !is_strict_kebab(skill) {
                return Err(SkillNameError::new(
                    name,
                    "marketplace name must be <plugin>:<skill> with strict-kebab segments",
                ));
            }
            Ok(ParsedSkillName {
                raw: name.to_string(),
                kind: SkillNameKind::Marketplace,
                skill: skill.to_string(),
                plugin: Some(plugin.to_string()),
                server: None,
            })
        }
        3 => {
            let (head, server, skill) = (segments[0], segments[1], segments[2]);
            if head != MCP_SEGMENT {
                return Err(SkillNameError::new(
                    name,
                    "3-segment names are reserved for the mcp source (first segment must be 'mcp')",
                ));
            }
            if !is_valid_mcp_server_segment(server) {
                return Err(SkillNameError::new(
                    name,
                    "mcp <server> segment must match [A-Za-z0-9_-]{1,64}",
                ));
            }
            if !is_strict_kebab(skill) {
                return Err(SkillNameError::new(
                    name,
                    "mcp <skill> leaf must be strict kebab",
                ));
            }
            Ok(ParsedSkillName {
                raw: name.to_string(),
                kind: SkillNameKind::Mcp,
                skill: skill.to_string(),
                plugin: None,
                server: Some(server.to_string()),
            })
        }
        n => Err(SkillNameError::new(
            name,
            format!("segment count {n} not in {{1, 2, 3}}"),
        )),
    }
}

/// 非抛出版校验 / Non-raising validity check（便于 `client:get_skill` 入参快速门控）。
pub fn is_valid_skill_name(name: &str) -> bool {
    parse_skill_name(name).is_ok()
}

/// 合成 user 源裸名 / Synthesize a bare user-source name：`<skill>`（1 段）。
///
/// # Errors
/// `skill` 非严格 kebab 时返回 [`SkillNameError`]。
pub fn synthesize_user_name(skill: &str) -> Result<String, SkillNameError> {
    if !is_strict_kebab(skill) {
        return Err(SkillNameError::new(
            skill,
            "user <skill> must be strict kebab",
        ));
    }
    Ok(skill.to_string())
}

/// 合成 marketplace 源裸名 / Synthesize a marketplace name：`<plugin>:<skill>`（2 段）。
///
/// # Errors
/// `plugin` / `skill` 非严格 kebab 时返回 [`SkillNameError`]。
pub fn synthesize_marketplace_name(plugin: &str, skill: &str) -> Result<String, SkillNameError> {
    let combined = format!("{plugin}{SEPARATOR}{skill}");
    if !is_strict_kebab(plugin) {
        return Err(SkillNameError::new(
            combined,
            "marketplace <plugin> must be strict kebab",
        ));
    }
    if !is_strict_kebab(skill) {
        return Err(SkillNameError::new(
            combined,
            "marketplace <skill> must be strict kebab",
        ));
    }
    Ok(combined)
}

/// 合成 mcp 源 name / Synthesize an mcp-source name：`mcp:<normalized-server>:<skill>`（3 段）。
///
/// `server` 先经 [`normalize_mcp_server_segment`] 规范化；规范化后长度 = 0 或 > 64 → 判废。
///
/// # Errors
/// 规范化 server 越界或 `skill` 非严格 kebab 时返回 [`SkillNameError`]。
pub fn synthesize_mcp_name(server: &str, skill: &str) -> Result<String, SkillNameError> {
    let normalized = normalize_mcp_server_segment(server);
    if !is_valid_mcp_server_segment(&normalized) {
        return Err(SkillNameError::new(
            format!("{MCP_SEGMENT}{SEPARATOR}{normalized}{SEPARATOR}{skill}"),
            "normalized mcp <server> must match [A-Za-z0-9_-]{1,64} (empty or >64 rejected)",
        ));
    }
    if !is_strict_kebab(skill) {
        return Err(SkillNameError::new(
            format!("{MCP_SEGMENT}{SEPARATOR}{normalized}{SEPARATOR}{skill}"),
            "mcp <skill> leaf must be strict kebab",
        ));
    }
    Ok(format!(
        "{MCP_SEGMENT}{SEPARATOR}{normalized}{SEPARATOR}{skill}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_valid_user_marketplace_mcp() {
        // 1 段 = user 裸名
        let u = parse_skill_name("code-review").unwrap();
        assert_eq!(u.kind, SkillNameKind::User);
        assert_eq!(u.skill, "code-review");
        assert!(u.plugin.is_none() && u.server.is_none());

        // 2 段 = marketplace
        let m = parse_skill_name("my-plugin:do-thing").unwrap();
        assert_eq!(m.kind, SkillNameKind::Marketplace);
        assert_eq!(m.plugin.as_deref(), Some("my-plugin"));
        assert_eq!(m.skill, "do-thing");
        assert!(m.server.is_none());

        // 3 段 = mcp（首段字面 mcp；server 保留大小写 / 下划线）
        let c = parse_skill_name("mcp:My_Server-1:fetch-url").unwrap();
        assert_eq!(c.kind, SkillNameKind::Mcp);
        assert_eq!(c.server.as_deref(), Some("My_Server-1"));
        assert_eq!(c.skill, "fetch-url");
        assert!(c.plugin.is_none());
    }

    #[test]
    fn test_parse_invalid_cases_map_to_4016() {
        let invalid = [
            "",                  // 段数 0（split 得 [""] → 空 skill 非 kebab）
            "a:b:c:d",           // 段数 4
            "notmcp:srv:skill",  // 3 段首段 ≠ mcp
            "Bad-Case",          // 含大写（user 段非 kebab）
            "has_underscore",    // user 段不允许下划线
            "-leading",          // 前导连字符
            "trailing-",         // 尾随连字符
            "double--hyphen",    // 连续连字符
            "mcp:srv:Bad",       // mcp skill 段非 kebab
            "mcp:bad server:ok", // mcp server 段含空格
        ];
        for name in invalid {
            let err = parse_skill_name(name).expect_err(&format!("{name:?} 应非法"));
            assert_eq!(err.error_code(), ErrorCode::SkillNameInvalid);
            assert!(!is_valid_skill_name(name));
            // 4016 ErrorPayload：code=4016 + details.name 透传
            let payload = err.to_error_payload();
            assert_eq!(payload.code, 4016);
            assert_eq!(payload.details.unwrap()["name"], name);
        }
    }

    #[test]
    fn test_synthesize_roundtrip() {
        // user
        let n = synthesize_user_name("solo-skill").unwrap();
        assert_eq!(n, "solo-skill");
        assert_eq!(parse_skill_name(&n).unwrap().kind, SkillNameKind::User);

        // marketplace
        let n = synthesize_marketplace_name("plug", "skill-a").unwrap();
        assert_eq!(n, "plug:skill-a");
        let p = parse_skill_name(&n).unwrap();
        assert_eq!(p.kind, SkillNameKind::Marketplace);
        assert_eq!(p.plugin.as_deref(), Some("plug"));

        // mcp（server 规范化：含点/空格 → 下划线）
        let n = synthesize_mcp_name("srv.example com", "do-it").unwrap();
        assert_eq!(n, "mcp:srv_example_com:do-it");
        let p = parse_skill_name(&n).unwrap();
        assert_eq!(p.kind, SkillNameKind::Mcp);
        assert_eq!(p.server.as_deref(), Some("srv_example_com"));
        assert_eq!(p.skill, "do-it");
    }

    #[test]
    fn test_synthesize_rejects_bad_segments() {
        assert!(synthesize_user_name("Bad").is_err());
        assert!(synthesize_marketplace_name("ok", "Bad").is_err());
        assert!(synthesize_marketplace_name("Bad", "ok").is_err());
        // server 规范化后为空（全非法字符且……实际会变成等长下划线，不会空）；用真正空串触发
        assert!(synthesize_mcp_name("", "ok").is_err());
        assert!(synthesize_mcp_name("srv", "Bad").is_err());
    }

    #[test]
    fn test_normalize_mcp_server_segment() {
        assert_eq!(normalize_mcp_server_segment("Already_OK-1"), "Already_OK-1");
        assert_eq!(normalize_mcp_server_segment("a.b/c d"), "a_b_c_d");
        assert_eq!(normalize_mcp_server_segment("café"), "caf_"); // 非 ASCII → _
    }
}
