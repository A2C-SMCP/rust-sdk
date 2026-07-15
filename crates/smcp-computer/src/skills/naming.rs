/*!
* 文件名: naming.rs
* 作者: JQQ
* 创建日期: 2026/06/03
* 最后修改日期: 2026/06/03
* 版权: 2023 JQQ. All rights reserved.
* 依赖: smcp::skill_name
* 描述: Computer 端 SKILL 命名桥接 + source→name 合成链（对标 Python computer/skills/naming.py）
*       Computer-side SKILL naming bridge + source→name synthesis facade.
*/

//! Computer 端 SKILL 命名模块 / Computer-side SKILL naming。
//!
//! 协议依据 / Protocol: a2c-smcp-protocol `skill.md` §1（`A2CSkillRef` 命名、段数消歧）；
//! `error-handling.md` §4016（非法 name）。
//! 对标 Python 参考实现 / Mirrors the Python reference: `a2c_smcp/computer/skills/naming.py`。
//!
//! **单一事实源 / Single source of truth**：命名的 lexer / 字符集 / 合成规则全部由协议 crate
//! [`smcp::skill_name`]（SMCP-05）承担——本模块**不重复实现解析规则**，只做两件 Computer 侧的事：
//!
//! 1. **桥接**：把协议 lexer 的类型与函数 re-export 到 `skills` 子系统的统一导入面，使 registry /
//!    staging / sources 从 `crate::skills::naming` 单点引入，而非各自散落 `use smcp::skill_name::*`。
//! 2. **source → name 合成链**：[`synthesize_name`] 按本地 source 模型 [`SkillNameSpec`] 统一生成
//!    规范化 `A2CSkillRef.name`，消除各调用点散落的 `format!("{plugin}:{skill}")` 之类字符串拼接。
//!
//! 与 SMCP-05 互通保证 / Interop with SMCP-05：[`synthesize_name`] 产出的 name 必可被
//! [`parse_skill_name`] 解回**同一** [`SkillNameKind`] 与分段（round-trip），二者共享同一套规则。

// 协议级 lexer / 合成 / 错误类型——单点 re-export，Computer 子系统经此统一引入。
// Protocol-level lexer / synthesis / error types — re-exported as the subsystem's single import face.
pub use smcp::skill_name::{
    is_valid_skill_name, parse_skill_name, synthesize_marketplace_name, synthesize_mcp_name,
    synthesize_user_name, ParsedSkillName, SkillNameError, SkillNameKind, MAX_SEGMENT_LEN,
    MCP_SEGMENT, SEPARATOR,
};

/// Computer 侧 source → name 合成输入 / Computer-side source spec for name synthesis。
///
/// 把本地 source 模型（与协议 `A2CSkillRef.source` ∈ user/marketplace/mcp 对齐）绑定到协议合成函数，
/// 是 registry / staging / sources 生成 `A2CSkillRef.name` 的**唯一**结构化入口。三形态分别对应
/// 1 / 2 / 3 段裸名。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillNameSpec<'a> {
    /// user 源：1 段 `<skill>`。
    User {
        /// leaf skill 段（严格 kebab）。
        skill: &'a str,
    },
    /// marketplace 源：2 段 `<plugin>:<skill>`。
    Marketplace {
        /// plugin 段（严格 kebab）。
        plugin: &'a str,
        /// leaf skill 段（严格 kebab）。
        skill: &'a str,
    },
    /// mcp 源：3 段 `mcp:<bundle_id>:<skill>`（skill.md §1.3：`<server>` 段 = `bundle_id` **原样**）。
    Mcp {
        /// MCP Server 的 `bundle_id`（唯一身份，**原样**进段、不规范化；**非** display 名）。
        bundle_id: &'a str,
        /// leaf skill 段（严格 kebab）。
        skill: &'a str,
    },
}

impl SkillNameSpec<'_> {
    /// 本规格对应的 source 形态 / The source shape of this spec。
    pub fn kind(&self) -> SkillNameKind {
        match self {
            SkillNameSpec::User { .. } => SkillNameKind::User,
            SkillNameSpec::Marketplace { .. } => SkillNameKind::Marketplace,
            SkillNameSpec::Mcp { .. } => SkillNameKind::Mcp,
        }
    }
}

/// source → name 合成链 / Synthesize a canonical `A2CSkillRef.name` from a local source spec。
///
/// 按 source 形态分派到协议合成函数（[`synthesize_user_name`] / [`synthesize_marketplace_name`] /
/// [`synthesize_mcp_name`]），产出可被 [`parse_skill_name`] 逆解的规范化裸名。registry / staging /
/// sources **MUST** 经此函数生成 name，而非手工拼接，以保证与协议 lexer 单一事实源对齐。
///
/// # Errors
/// 任一段不符协议字符集（严格 kebab；或 mcp `<server>` 段非合法 `bundle_id` 字符集 / 越界）时返回
/// [`SkillNameError`]（映射协议 `4016`）。
pub fn synthesize_name(spec: SkillNameSpec<'_>) -> Result<String, SkillNameError> {
    match spec {
        SkillNameSpec::User { skill } => synthesize_user_name(skill),
        SkillNameSpec::Marketplace { plugin, skill } => synthesize_marketplace_name(plugin, skill),
        SkillNameSpec::Mcp { bundle_id, skill } => synthesize_mcp_name(bundle_id, skill),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 三类 source 合成 → name → parse 回环：kind / 分段一致（与 SMCP-05 互通）。
    #[test]
    fn test_synthesize_name_roundtrips_through_lexer() {
        struct Case<'a> {
            spec: SkillNameSpec<'a>,
            name: &'a str,
            kind: SkillNameKind,
            plugin: Option<&'a str>,
            server: Option<&'a str>,
            skill: &'a str,
        }
        let cases = [
            Case {
                spec: SkillNameSpec::User {
                    skill: "code-review",
                },
                name: "code-review",
                kind: SkillNameKind::User,
                plugin: None,
                server: None,
                skill: "code-review",
            },
            Case {
                spec: SkillNameSpec::Marketplace {
                    plugin: "acme-audit",
                    skill: "audit",
                },
                name: "acme-audit:audit",
                kind: SkillNameKind::Marketplace,
                plugin: Some("acme-audit"),
                server: None,
                skill: "audit",
            },
            Case {
                // mcp `<server>` 段 = bundle_id 原样（skill.md §1.6：display 名 `my.api` 的缺省生成结果）。
                spec: SkillNameSpec::Mcp {
                    bundle_id: "my_api",
                    skill: "csv-aggregator",
                },
                name: "mcp:my_api:csv-aggregator",
                kind: SkillNameKind::Mcp,
                plugin: None,
                server: Some("my_api"),
                skill: "csv-aggregator",
            },
            Case {
                // hash-fallback 情形（CJK display 名）：不透明 bundle_id 同样原样进段。
                spec: SkillNameSpec::Mcp {
                    bundle_id: "bundle_a1b2c3d4e5f60718",
                    skill: "summarize",
                },
                name: "mcp:bundle_a1b2c3d4e5f60718:summarize",
                kind: SkillNameKind::Mcp,
                plugin: None,
                server: Some("bundle_a1b2c3d4e5f60718"),
                skill: "summarize",
            },
        ];
        for c in cases {
            assert_eq!(c.spec.kind(), c.kind);
            let synthesized = synthesize_name(c.spec).expect("合法 spec 应能合成");
            assert_eq!(synthesized, c.name);
            // round-trip：合成结果可被协议 lexer 解回同一 kind 与分段。
            let parsed = parse_skill_name(&synthesized).expect("合成的 name 必合法");
            assert_eq!(parsed.kind, c.kind);
            assert_eq!(parsed.plugin.as_deref(), c.plugin);
            assert_eq!(parsed.server.as_deref(), c.server);
            assert_eq!(parsed.skill, c.skill);
            assert!(is_valid_skill_name(&synthesized));
        }
    }

    /// 非法段（严格 kebab 违例 / mcp `<server>` 非合法 bundle_id 字符集）→ 合成失败并映射 4016。
    #[test]
    fn test_synthesize_name_rejects_invalid_segments_maps_4016() {
        let invalid = [
            SkillNameSpec::User { skill: "Bad-Case" }, // 含大写
            SkillNameSpec::User {
                skill: "has_underscore",
            }, // user 段禁下划线
            SkillNameSpec::Marketplace {
                plugin: "Acme",
                skill: "audit",
            }, // plugin 非 kebab
            SkillNameSpec::Marketplace {
                plugin: "acme",
                skill: "Audit",
            }, // skill 非 kebab
            SkillNameSpec::Mcp {
                bundle_id: "",
                skill: "ok",
            }, // `<server>` 段为空（合法 bundle_id 恒非空，故此仅挡调用方错误）
            SkillNameSpec::Mcp {
                bundle_id: "my.api",
                skill: "ok",
            }, // 传了 display 名而非 bundle_id（含 `.` → 越界，不再被静默规范化）
            SkillNameSpec::Mcp {
                bundle_id: "srv",
                skill: "Bad",
            }, // skill 非 kebab
        ];
        for spec in invalid {
            let err = synthesize_name(spec).expect_err("非法段应合成失败");
            assert_eq!(err.to_error_payload().code, 4016);
        }
    }

    /// is_valid_skill_name 与 parse_skill_name 判定一致（合法 true / 非法 false）。
    #[test]
    fn test_is_valid_consistent_with_parse() {
        for name in ["solo-skill", "plug:skill-a", "mcp:My_Server:do-it"] {
            assert!(is_valid_skill_name(name));
            assert!(parse_skill_name(name).is_ok());
        }
        for name in ["", "a:b:c:d", "notmcp:s:k", "Bad", "mcp:bad server:k"] {
            assert!(!is_valid_skill_name(name));
            assert!(parse_skill_name(name).is_err());
        }
    }
}
