/*!
* 文件名: env_segment.rs
* 作者: JQQ
* 创建日期: 2026/07/18
* 版权: 2023 JQQ. All rights reserved.
* 依赖: std（无 regex——纯 char 迭代）
* 描述: input 环境变量命名（ENV_SEGMENT）单一权威 / Input env var naming (ENV_SEGMENT), single source of truth.
*/

//! input 环境变量命名（ENV_SEGMENT）单一权威 / Input env var naming, single source of truth.
//!
//! 协议依据 / Protocol：a2c-smcp-protocol `docs/guides/computer-mcp-config-guide.md`
//! §「环境变量命名规则（双端统一规范）」（PROTO-5 / Discussion #23 F4-F5）。
//!
//! **存在意义**：env 名派生 **MUST** 逐字节确定、各 SDK（Python / Rust）产出同一结果——运维写在 CI 里
//! 的那一份 env 配置双端通用是硬门槛。对标 Python `a2c_smcp/utils/env_segment.py`。
//!
//! 形态 / Shape：`A2C_SMCP_<ENV_SEGMENT(input_id)>[_<ENV_SEGMENT(bundle_id)>][_<ENV_SEGMENT(tool_name)>]`。
//!
//! 🔴 **与 [`super::bundle_id::is_bundle_id_char`] / `normalize_name` 的关键差异（勿复用后者）**：
//! ENV_SEGMENT **不**折叠连续 `_`、**不**裁首尾 `[_-]`（`normalize_name` 两者都做）。误复用会让
//! `a_b`/`a__b` 坍缩、`_lead_` 变 `lead`。二者是**两个不同函数**，各自服务不同规范面。
//!
//! **0.3.0 硬切（F5）**：历史前缀 `A2C_INPUT_` + `upper()` 已废止，**无双读、无过渡期**。旧 `upper()`
//! 会让 `figma-token`/`figma_token`/`Figma_Token` 三者静默坍缩到同一变量名——F4「保留大小写」正为消灭之。

use std::collections::BTreeMap;
use std::fmt;

/// input 环境变量前缀（0.3.0 起）/ input env var prefix (since 0.3.0)。
///
/// 历史 `A2C_INPUT_` 前缀 F5 终审硬切废止，无双读、无过渡期。
pub const A2C_ENV_PREFIX: &str = "A2C_SMCP_";

/// 两个及以上不同 input id 映射到**同一完整 env 变量名**（ENV_SEGMENT 非单射，如 `a-b` 与 `a_b`）/ collision。
///
/// 规范要求注册期**硬错误**（F4）：此前是静默串味、后写的赢（含 `password:true` 密钥）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvNameCollisionError {
    /// `完整 env 变量名 → 撞在一起的 input id 集`（仅含 `>1` 的分组）/ env var name → colliding ids。
    pub collisions: BTreeMap<String, Vec<String>>,
}

impl fmt::Display for EnvNameCollisionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let detail = self
            .collisions
            .iter()
            .map(|(name, ids)| format!("{ids:?} → {name:?}"))
            .collect::<Vec<_>>()
            .join("; ");
        write!(
            f,
            "input ids collide on the same env var name, rename one to disambiguate: {detail}"
        )
    }
}

impl std::error::Error for EnvNameCollisionError {}

/// 检出映射到同一**完整 env 名**的 input id 分组 / group input ids colliding on the full env name。
///
/// 返回 `{env_var_name: [撞在一起的 input_id, ...]}`，仅含 **>1** 的分组；无冲突则空。检测面 = **完整 env 名**
/// （F4）：某段 ENV_SEGMENT 相同但完整名不同的情形**无害**，MUST NOT 报错（按段判会误拒）。
///
/// 🔴 **接线 server/tool 段时本函数 MUST 同步扩形**：当前只吃裸 id（live 路径只有 id 段 ⇒ 裸 id 集即
/// 「全部活跃 env 名」）。一旦 bundle_id 段接入 live，活跃 env 名成 (id × bundle_id) 的积，本函数须跟进。
#[must_use]
pub fn detect_env_name_collisions<I, S>(input_ids: I) -> BTreeMap<String, Vec<String>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut by_name: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for id in input_ids {
        let id = id.as_ref().to_string();
        let name = env_var_name(&id, None, None);
        by_name.entry(name).or_default().push(id);
    }
    by_name.retain(|_, ids| {
        ids.sort();
        ids.dedup();
        ids.len() > 1
    });
    by_name
}

/// 检出即抛 [`EnvNameCollisionError`]（注册期 fail-fast）/ detect and raise, for registration-time fail-fast。
///
/// # Errors
/// 存在两个及以上 input id 坍缩到同一完整 env 名时返回 [`EnvNameCollisionError`]（提示含撞上的名 + 全部肇事 id）。
pub fn raise_on_env_name_collisions<I, S>(input_ids: I) -> Result<(), EnvNameCollisionError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let collisions = detect_env_name_collisions(input_ids);
    if collisions.is_empty() {
        Ok(())
    } else {
        Err(EnvNameCollisionError { collisions })
    }
}

/// `ENV_SEGMENT(s)`：按 **Unicode 码点**迭代（`chars()`，MUST NOT 按 UTF-8 字节或 grapheme），
/// 非 `[A-Za-z0-9_]` 码点 → `_`；**保留大小写**；**不**折叠连续 `_`、**不**裁首尾 / normalize one segment.
///
/// MUST 用显式 ASCII 字符类（`is_ascii_alphanumeric() || c == '_'`），MUST NOT 用 Unicode `\w`
/// （各语言 Unicode `\w` 集合不一致）。与 python `re.sub(r"[^A-Za-z0-9_]", "_", s)` 逐字节一致。
#[must_use]
pub fn env_segment(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// 组合完整 input 环境变量名 / compose the full input env var name。
///
/// 形态：`A2C_SMCP_<seg(input_id)>[_<seg(bundle_id)>][_<seg(tool_name)>]`；段缺省则整段省略
/// （含其前导 `_`）。**server 上下文段用 `bundle_id`（运行期唯一身份），MUST NOT 用 display name**——
/// 同名 server 会串用彼此的解析值（D2）。
#[must_use]
pub fn env_var_name(input_id: &str, bundle_id: Option<&str>, tool_name: Option<&str>) -> String {
    let mut out = String::from(A2C_ENV_PREFIX);
    out.push_str(&env_segment(input_id));
    if let Some(b) = bundle_id {
        out.push('_');
        out.push_str(&env_segment(b));
    }
    if let Some(t) = tool_name {
        out.push('_');
        out.push_str(&env_segment(t));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_segment_case_preserved_no_fold_no_trim() {
        // 保留大小写、不折叠、不裁首尾（与 normalize_name 相反）。
        assert_eq!(env_segment("MyServer"), "MyServer");
        assert_eq!(env_segment("a--b"), "a__b");
        assert_eq!(env_segment("_lead_trail_"), "_lead_trail_");
        assert_eq!(
            env_segment("frontend@team/figma_token"),
            "frontend_team_figma_token"
        );
    }

    #[test]
    fn env_segment_per_codepoint() {
        // CJK 2 码点 → 2 个 '_'；astral emoji 单码点 → 单个 '_'。
        assert_eq!(env_segment("令牌"), "__");
        assert_eq!(env_segment("a😀b"), "a_b");
    }

    #[test]
    fn env_var_name_composition_and_optional_segments() {
        assert_eq!(env_var_name("api_key", None, None), "A2C_SMCP_api_key");
        assert_eq!(
            env_var_name("api_key", Some("feishu-mcp"), None),
            "A2C_SMCP_api_key_feishu_mcp"
        );
        assert_eq!(
            env_var_name("token", Some("MyServer"), Some("auth")),
            "A2C_SMCP_token_MyServer_auth"
        );
    }

    #[test]
    fn collision_pair_maps_to_same_name() {
        // '-' 与 '_' 同映射（非单射）→ 完整名相同 ⇒ 注册期须 fail-fast（由消费者检测）。
        assert_eq!(
            env_var_name("a-b", None, None),
            env_var_name("a_b", None, None)
        );
        // 对照对：保留大小写 ⇒ MyServer / myserver 不坍缩。
        assert_ne!(
            env_var_name("token", Some("MyServer"), None),
            env_var_name("token", Some("myserver"), None)
        );
    }

    #[test]
    fn detect_collisions_positive_and_negative() {
        // 坍缩对 → 报（完整名 A2C_SMCP_a_b 下含两 id）。
        let hit = detect_env_name_collisions(["a-b", "a_b"]);
        assert_eq!(hit.len(), 1);
        assert_eq!(
            hit["A2C_SMCP_a_b"],
            vec!["a-b".to_string(), "a_b".to_string()]
        );
        assert!(raise_on_env_name_collisions(["a-b", "a_b"]).is_err());
        // 多 id 但完整名各异 → **不**误报（负向）。
        assert!(detect_env_name_collisions(["a", "b", "c"]).is_empty());
        assert!(raise_on_env_name_collisions(["a", "b", "c"]).is_ok());
        // 同段坍缩但完整名分叉（bundle_id 段分开）→ 无害，MUST NOT 报。
        assert!(detect_env_name_collisions(["a-b_x", "a_b_y"]).is_empty());
    }
}
