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
//! | mcp          | `mcp:<bundle_id>:<skill>`  | 3    |
//!
//! - `:` 是协议层 reserved separator；`mcp` 是唯一保留字面首段前缀的 source（与 2 段 marketplace 区分）。
//! - 字符集 / charsets（skill.md §1.4）：
//!     - `<skill>` / `<plugin>` 段为**严格 kebab**（`[a-z0-9]` + 单连字符分隔，不以 `-` 始末、无连续 `--`、长 1–64）。
//!     - mcp `<server>` 段 **= 该 Server 的 `bundle_id` 原样**（§1.3）：非空、`[A-Za-z0-9_-]`、无 `.`、
//!       无连续 `__`、**无长度上限**（§1.4 表中该行未列「1–64」，仅 user / marketplace / `<skill>` 段有）。
//!       合成与解析共用同一判据 [`crate::utils::bundle_id::is_valid_bundle_id`]（单一权威，勿另写等价谓词）。
//! - 非法 name → [`SkillNameError`]，由 `client:get_skill` 处理器映射为协议 `4016`。
//!
//! **`<server>` 段为何取 `bundle_id`（rust-sdk#127，supersede #18 的正交结论）**：post-BundleID，MCP Server
//! 的 `name` 是纯 display、**允许碰撞、永不做键/寻址**，A2C server 的唯一身份是 `bundle_id`。取 display 名
//! 会让两个 `name` 巧合相同、`bundle_id` 不同的**合法共存** Server 撞出同一个 `mcp:<name>:<skill>`，迫使
//! §1.5 拒绝第二注册者、令一个合法 SKILL 对 Agent **隐身**；取 `bundle_id`（缺省生成恒有值 + no-double-open
//! 保证唯一）则 mcp 形态 name **构造上不碰撞**，且 SKILL 的 server 身份与 `get_config.servers` key /
//! `get_resources.mcp_server` / `4014`·`4015` 的 `meta.mcp_server` **全协议统一**。

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

/// mcp `<server>` 段是否合规 / Whether the mcp `<server>` segment is valid。
///
/// **判据完全委托** [`crate::utils::bundle_id::is_valid_bundle_id`]——`<server>` 段**就是** `bundle_id`
/// （skill.md §1.3），故其合法性只能有一个来源。此处**不得**另写一份等价谓词：两处一旦漂移（如擅自加
/// 长度上限、或漏禁连续 `__`），mcp 段就会拒绝合法 `bundle_id` → 该 Server 的 SKILL 对 Agent **隐身**，
/// 即本模块要消灭的失效模式原地复活；且与 python-sdk 出线分歧（其 `_is_valid_mcp_server_segment` 同样
/// 委托 `a2c_smcp.utils.bundle_id.is_valid_bundle_id`）。
///
/// 注意 `<server>` 段**不受** [`MAX_SEGMENT_LEN`] 约束——§1.4 表中仅 user / marketplace / `<skill>` 段
/// 明列「1–64」，`<server>` 行只写「= `bundle_id`（§1.3；`[A-Za-z0-9_-]`、无 `.`、无 `__`）」。
fn is_valid_mcp_server_segment(segment: &str) -> bool {
    crate::utils::bundle_id::is_valid_bundle_id(segment)
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
                    "mcp <server> segment must be a valid bundle_id: non-empty, charset [A-Za-z0-9_-], no '.', no '__'",
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

/// 合成 mcp 源 name / Synthesize an mcp-source name：`mcp:<bundle_id>:<skill>`（3 段）。
///
/// `bundle_id` 是该 MCP Server 的**唯一身份**，**原样**进段——不做任何规范化（协议 skill.md §1.3）。
/// 下方 guard 的判据即 [`crate::utils::bundle_id::is_valid_bundle_id`]（**同一权威**，非等价重写），故
/// 合法 `bundle_id` 恒通过；guard 只用于挡住「误传 display 名」这类调用方错误。
///
/// 取 `bundle_id` 而非 display `name` 使 mcp 形态 name **构造上不碰撞**（`bundle_id` 缺省生成恒有值、
/// no-double-open 保证唯一）——取 display 名则两个 `name` 巧合相同、`bundle_id` 不同的合法共存 Server
/// 会撞出同一个 name，迫使其中一个的 SKILL 对 Agent 隐身（§1.5）。
///
/// # Errors
/// `bundle_id` 非法（空 / 含 `[A-Za-z0-9_-]` 之外的字符 / 含连续 `__`）或 `skill` 非严格 kebab 时返回
/// [`SkillNameError`]。**无长度上限**（协议 §BundleID 未设）。
pub fn synthesize_mcp_name(bundle_id: &str, skill: &str) -> Result<String, SkillNameError> {
    if !is_valid_mcp_server_segment(bundle_id) {
        return Err(SkillNameError::new(
            format!("{MCP_SEGMENT}{SEPARATOR}{bundle_id}{SEPARATOR}{skill}"),
            "mcp <server> must be a valid bundle_id (taken verbatim, never normalized): non-empty, charset [A-Za-z0-9_-], no '.', no '__'",
        ));
    }
    if !is_strict_kebab(skill) {
        return Err(SkillNameError::new(
            format!("{MCP_SEGMENT}{SEPARATOR}{bundle_id}{SEPARATOR}{skill}"),
            "mcp <skill> leaf must be strict kebab",
        ));
    }
    Ok(format!(
        "{MCP_SEGMENT}{SEPARATOR}{bundle_id}{SEPARATOR}{skill}"
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

        // mcp（`<server>` = bundle_id，原样进段——不规范化，见 §1.3）
        let n = synthesize_mcp_name("srv_example_com", "do-it").unwrap();
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
        // `<server>` 非合法 bundle_id → 判废（合法 bundle_id 恒通过，故此仅挡调用方错误）
        assert!(synthesize_mcp_name("", "ok").is_err());
        assert!(synthesize_mcp_name("srv", "Bad").is_err());
    }

    /// #127 跨 SDK 对拍：mcp `<server>` 段判据 == `bundle_id` 判据，**不得**另加约束。
    ///
    /// 两个曾漂移的点（隔离审查 🔴，python-sdk#142 `4a5050a` 同源修复）：
    /// - **无长度上限**：§1.4 表中 `<server>` 行未列「1–64」（仅 user / marketplace / `<skill>` 有）、
    ///   §BundleID 亦未设。擅自加 64 上限会拒绝合法 `bundle_id`（显式长值 / 长 display 名的缺省生成
    ///   结果）→ 其 SKILL 对 Agent 隐身，且 Python 侧能正常合成 → 出线跨 SDK 分歧。
    /// - **禁连续 `__`**：§BundleID **MUST NOT** 含 `__`（它是 `bundle_id` 与工具名的保留分隔符）。
    ///   漏禁则 Rust 能合成出 `mcp:a__b:ok`，而 Python lexer 判 4016 → name 跨 SDK 不可解析。
    #[test]
    fn test_mcp_server_segment_predicate_matches_bundle_id_127() {
        // 无长度上限：65 / 256 字符的合法 bundle_id 须能合成、且可被 lexer 解回。
        for len in [MAX_SEGMENT_LEN + 1, 256] {
            let long = "a".repeat(len);
            let n = synthesize_mcp_name(&long, "ok")
                .unwrap_or_else(|e| panic!("长 bundle_id（{len} 字符）应合法: {}", e.reason));
            assert_eq!(n, format!("mcp:{long}:ok"));
            assert_eq!(
                parse_skill_name(&n).unwrap().server.as_deref(),
                Some(&*long)
            );
        }

        // 连续 `__` 非法（合成与解析两侧一致）。
        assert!(synthesize_mcp_name("a__b", "ok").is_err());
        assert!(parse_skill_name("mcp:a__b:ok").is_err());

        // `<skill>` leaf 仍受严格 kebab + 1–64 约束（该段协议**确有**长度上限，勿一并放开）。
        assert!(synthesize_mcp_name("srv", &"a".repeat(MAX_SEGMENT_LEN + 1)).is_err());
    }

    /// #127：mcp `<server>` 段 = `bundle_id` **原样**，合成侧**不再**做任何规范化。
    ///
    /// 本段的判据**就是** `bundle_id` 的判据（同一谓词 [`crate::utils::bundle_id::is_valid_bundle_id`]，
    /// 恒等而非「子集」——判据委托后二者不可能分歧，这正是本次修复的要点）。故越界字符只可能来自
    /// 「调用方传了 display 名而不是 bundle_id」——必须**判废**，不能静默改写成一个与该 server 真实身份
    /// 不符的段：旧的静默规范化正是把两个不同 CJK 名、不同 `bundle_id` 的合法 server 撞成同一个 `___`
    /// 段，从而令其中一个的 SKILL 对 Agent 隐身。
    #[test]
    fn test_synthesize_mcp_takes_bundle_id_verbatim_127() {
        // 越界字符 → Err（旧行为：静默规范化为 `mcp:srv_example_com:do-it` / `mcp:___:summarize`）。
        assert!(synthesize_mcp_name("srv.example com", "do-it").is_err());
        assert!(synthesize_mcp_name("服务器", "summarize").is_err());

        // 合法 bundle_id 原样进段，并恒可被 lexer 解析回同一段（skill.md §1.6 合成示例）。
        for (bundle_id, skill, expected) in [
            (
                "tfrobot-tools",
                "code-review",
                "mcp:tfrobot-tools:code-review",
            ),
            ("my_api", "csv-aggregator", "mcp:my_api:csv-aggregator"),
            ("acme-editor", "format", "mcp:acme-editor:format"),
            (
                "bundle_a1b2c3d4e5f60718",
                "summarize",
                "mcp:bundle_a1b2c3d4e5f60718:summarize",
            ),
        ] {
            let n = synthesize_mcp_name(bundle_id, skill).unwrap();
            assert_eq!(n, expected);
            let p = parse_skill_name(&n).unwrap();
            assert_eq!(p.kind, SkillNameKind::Mcp);
            assert_eq!(p.server.as_deref(), Some(bundle_id));
            assert_eq!(p.skill, skill);
        }
    }
}
