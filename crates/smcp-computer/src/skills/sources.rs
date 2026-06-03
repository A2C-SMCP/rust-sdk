/*!
* 文件名: sources.rs
* 作者: JQQ
* 创建日期: 2026/06/03
* 最后修改日期: 2026/06/03
* 版权: 2023 JQQ. All rights reserved.
* 依赖: serde_json, crate::settings::is_valid_git_url, thiserror
* 描述: SKILL marketplace source 解析：plugin source 5 类 + 简写糖归一化（对标 Python computer/skills/sources.py）
*       SKILL marketplace source resolution: plugin source (5 kinds) + shorthand normalization.
*/

//! SKILL marketplace source 解析 / SKILL marketplace source resolution。
//!
//! 协议依据 / Protocol: tfrobot-marketplace `docs/marketplace/protocol-v1.md` §5（Plugin source 5 类 disjoint union +
//! github/cnb 简写糖 + git-subdir sparse clone）、§3.2（`metadata.pluginRoot` 使裸名 `"x"` 等价
//! `"./<pluginRoot>/x"`）。对标 Python 参考实现 / Mirrors `a2c_smcp/computer/skills/sources.py`。
//!
//! 本模块**纯解析、零 I/O、零 git**：把 `marketplace.json` 中每个 plugin 条目的 `source` 字段（`str`
//! 相对路径 / 5 类 source 对象之一）归一化为供 staging（SKL-04）执行的克隆 / 定位规格：
//! - [`LocalPluginSource`]：相对路径（含裸名经 `pluginRoot` 解析），指向 **marketplace clone 内**子目录；
//! - [`GitCloneSpec`]：需独立 `git clone` 的源（`url` / `github` / `cnb` / `git-subdir`），`github` /
//!   `cnb` 的 `owner/repo` 简写糖归一化为 `https://<host>/<repo>.git`，`git-subdir` 额外携带 `subdir`。
//!
//! Marketplace source（用户添加 marketplace 的 `{type:"git", url}`，与 plugin source **两套独立
//! schema**，disjoint union）的克隆 url 提取见 [`marketplace_clone_url`]。git url 形态判定复用
//! settings 层既有 [`crate::settings::is_valid_git_url`]（与 Python 引用同一 `is_valid_git_url`）。

use serde_json::{Map, Value};

use crate::settings::is_valid_git_url;

// ---------------------------------------------------------------------------
// 常量 / Constants
// ---------------------------------------------------------------------------
/// github 简写糖归一化目标 host / GitHub shorthand normalization host（协议 §5.2.1）。
pub const GITHUB_HOST: &str = "github.com";
/// cnb 简写糖归一化目标 host / CNB shorthand normalization host（协议 §5.2.4）。
pub const CNB_HOST: &str = "cnb.cool";
/// 裸名经 `metadata.pluginRoot` 解析的默认前缀 / Default pluginRoot for bare-name sources（协议 §3.2）。
pub const DEFAULT_PLUGIN_ROOT: &str = "./plugins";

// ---------------------------------------------------------------------------
// 错误 / Error
// ---------------------------------------------------------------------------
/// plugin / marketplace source 形态非法 / Malformed plugin or marketplace source。
///
/// 由 staging 捕获并按失败降级铁律处理（记 ERROR、该源不入 Registry、不阻断其余、不向 Agent 硬报错）。
/// 本错误**不**自带协议码——marketplace source 解析失败属本地配置问题，非协议错误码语义。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid skill source {raw}: {reason}")]
pub struct SkillSourceError {
    /// 触发错误的原始 source（紧凑回显）/ The offending source (compact echo)。
    pub raw: String,
    /// 人类可读的失败原因 / Human-readable failure reason。
    pub reason: String,
}

impl SkillSourceError {
    fn new(raw: &str, reason: impl Into<String>) -> Self {
        Self {
            raw: raw.to_string(),
            reason: reason.into(),
        }
    }
}

// ---------------------------------------------------------------------------
// 解析结果 / Resolved specs
// ---------------------------------------------------------------------------
/// 需独立 `git clone` 的 plugin 源规格 / Spec for a plugin source requiring its own `git clone`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitCloneSpec {
    /// 归一化后的 git URL（`github` / `cnb` 简写糖已展开）/ normalized git URL。
    pub url: String,
    /// 分支或 tag（可选）/ branch or tag (optional)。对应协议 `ref` 字段（避 Rust 关键字改名 `git_ref`）。
    pub git_ref: Option<String>,
    /// 完整 40 字符 commit SHA（可选，小写归一）/ full 40-char commit SHA (optional, lowercased)。
    pub sha: Option<String>,
    /// 非 `None` → `git-subdir` sparse clone，plugin 根 = `<clone>/<subdir>`（仓库相对，无 `..` / 非绝对）。
    pub subdir: Option<String>,
}

/// 相对路径 plugin 源 / Relative-path plugin source（指向 **marketplace clone 内**子目录，无需独立 clone）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalPluginSource {
    /// 归一化的仓库相对路径（去前导 `./`、POSIX 分隔、无 `..`）；裸名已由 `pluginRoot` 前缀展开。
    pub rel_path: String,
}

/// plugin source 解析结果（disjoint union，协议 §5.3）/ Resolved plugin source。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedPluginSource {
    /// 需独立 clone 的 git 源 / a git source needing its own clone。
    Git(GitCloneSpec),
    /// marketplace clone 内的相对路径源 / a relative-path source within the marketplace clone。
    Local(LocalPluginSource),
}

// ---------------------------------------------------------------------------
// 内部辅助 / internal helpers
// ---------------------------------------------------------------------------
/// 紧凑回显一个 JSON 值（用于错误 `raw`）/ Compact echo of a JSON value (for error `raw`)。
fn value_display(value: &Value) -> String {
    value.to_string()
}

/// `owner/repo` 两段简写糖判定 / Whether `s` is an `owner/repo` shorthand。
///
/// 等价正则 `^[^\s/]+/[^\s/]+$` 且非完整 URL（无 `://`）/ 非 scp-like（无 `@`）：恰两段，段内非空、
/// 无空白。手写校验以与协议 crate `skill_name` 同范式（不引入额外正则全局态）。
fn is_repo_shorthand(s: &str) -> bool {
    if s.contains("://") || s.contains('@') {
        return false;
    }
    let mut parts = s.split('/');
    let (Some(owner), Some(repo), None) = (parts.next(), parts.next(), parts.next()) else {
        return false; // 非恰两段
    };
    [owner, repo]
        .iter()
        .all(|seg| !seg.is_empty() && !seg.chars().any(char::is_whitespace))
}

/// 完整 40 字符 hex commit SHA 判定 / Whether `s` is a full 40-char hex SHA。
fn is_full_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// 取必填字符串字段（缺失 / 非字符串 / 空 → 抛）/ Require a non-empty string field。
fn require_str(map: &Map<String, Value>, key: &str, raw: &str) -> Result<String, SkillSourceError> {
    match map.get(key).and_then(Value::as_str) {
        Some(v) if !v.trim().is_empty() => Ok(v.trim().to_string()),
        _ => Err(SkillSourceError::new(
            raw,
            format!("required field '{key}' missing or not a non-empty string"),
        )),
    }
}

/// 提取可选 `ref` / `sha`（`sha` 须为完整 40 字符 hex，小写归一）/ Extract optional `ref` / `sha`。
///
/// `null` / 缺失 → `None`；present 但非 string / 空（`ref`）或非 40-hex（`sha`）→ 抛。
///
/// 安全前瞻 / Security note：`ref` **内容**本层**不**校验（仅校验非空，纯解析、与 Python parity）。下游
/// staging（SKL-04）以 `git clone --branch <ref>` / sparse-checkout `<subdir>` 拼接命令行时 **MUST** 用
/// `--` 终止符或白名单校验 `ref` / `subdir`，防 `--upload-pack=…` 之类 git 参数注入——本层定位为「纯解析、
/// 零 git」，刻意不在此加校验以免破坏 parity。
fn extract_ref_sha(
    map: &Map<String, Value>,
    raw: &str,
) -> Result<(Option<String>, Option<String>), SkillSourceError> {
    let git_ref = match map.get("ref") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Some(_) => {
            return Err(SkillSourceError::new(
                raw,
                "'ref' must be a non-empty string when present",
            ))
        }
    };
    let sha = match map.get("sha") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if is_full_sha(s.trim()) => Some(s.trim().to_lowercase()),
        Some(_) => {
            return Err(SkillSourceError::new(
                raw,
                "'sha' must be a full 40-char hex commit SHA when present",
            ))
        }
    };
    Ok((git_ref, sha))
}

/// 归一化仓库相对路径：POSIX 分隔、去 `.`、禁 `..` / 绝对、非空 / Normalize a repo-relative path。
fn norm_rel_segments(rel: &str, raw: &str) -> Result<String, SkillSourceError> {
    let normalized = rel.replace('\\', "/");
    let parts: Vec<&str> = normalized
        .split('/')
        .filter(|seg| !seg.is_empty() && *seg != ".")
        .collect();
    if parts.contains(&"..") {
        return Err(SkillSourceError::new(raw, "path must not contain '..'"));
    }
    if parts.is_empty() {
        return Err(SkillSourceError::new(
            raw,
            "path is empty / resolves to repo root",
        ));
    }
    Ok(parts.join("/"))
}

/// 解析相对路径 plugin 源（含裸名经 `pluginRoot` 解析）/ Resolve a relative-path (or bare-name) source。
///
/// - `"./x/y"` → 仓库根相对 `x/y`（协议 §5.1：必须以 `./` 开头、禁 `..`）；
/// - 裸名 `"x"`（无 `/`、无前导 `./`）→ `<pluginRoot>/x`（协议 §3.2 `metadata.pluginRoot`）。
fn resolve_relative(raw: &str, plugin_root: &str) -> Result<LocalPluginSource, SkillSourceError> {
    let s = raw.trim();
    let rel = if let Some(rest) = s.strip_prefix("./") {
        rest.to_string()
    } else if !s.contains('/') && !s.is_empty() && s != "." && s != ".." {
        // 裸名：经 metadata.pluginRoot 前缀展开（pluginRoot 自身可带前导 ./）。
        format!("{}/{s}", norm_rel_segments(plugin_root, raw)?)
    } else {
        return Err(SkillSourceError::new(
            raw,
            "relative plugin source must start with './' or be a bare plugin name",
        ));
    };
    Ok(LocalPluginSource {
        rel_path: norm_rel_segments(&rel, raw)?,
    })
}

// ---------------------------------------------------------------------------
// 公开 API / public API
// ---------------------------------------------------------------------------
/// `owner/repo` 简写糖归一化为 `https://<host>/<repo>.git` / Normalize an `owner/repo` shorthand。
///
/// 用于 `github`（host=[`GITHUB_HOST`]）与 `cnb`（host=[`CNB_HOST`]）两类（归一化逻辑同构）。
///
/// # Errors
/// `repo` 非 `owner/repo` 两段简写（如携带 scheme / scp-like `@`）时返回 [`SkillSourceError`]。
pub fn normalize_repo_shorthand(repo: &str, host: &str) -> Result<String, SkillSourceError> {
    let trimmed = repo.trim();
    if !is_repo_shorthand(trimmed) {
        return Err(SkillSourceError::new(
            repo,
            format!("'repo' must be an '<owner>/<repo>' shorthand, not a full URL (got '{repo}')"),
        ));
    }
    let r = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    Ok(format!("https://{host}/{r}.git"))
}

/// 解析 `marketplace.json` plugin 条目的 `source`（5 类 disjoint union）/ Resolve a plugin entry `source`。
///
/// 协议 §5.1 五类：相对路径（`str`）/ `url` / `github` / `git-subdir` / `cnb`。对象形态靠内层 `source`
/// 字段作 union discriminator（协议 §5.3，**非**递归嵌套）。
///
/// `plugin_root` 为裸名相对路径的前缀（来自 `metadata.pluginRoot`，默认 [`DEFAULT_PLUGIN_ROOT`]）。
///
/// # Errors
/// 形态非法 / 缺必填字段 / discriminator 未知时返回 [`SkillSourceError`]。
pub fn resolve_plugin_source(
    raw: &Value,
    plugin_root: &str,
) -> Result<ResolvedPluginSource, SkillSourceError> {
    // 字符串形态 → 相对路径源。
    if let Some(s) = raw.as_str() {
        return Ok(ResolvedPluginSource::Local(resolve_relative(
            s,
            plugin_root,
        )?));
    }
    let raw_display = value_display(raw);
    let Some(map) = raw.as_object() else {
        return Err(SkillSourceError::new(
            &raw_display,
            "plugin source must be a relative-path string or a source object",
        ));
    };

    match map.get("source").and_then(Value::as_str) {
        Some("url") => {
            let url = require_str(map, "url", &raw_display)?;
            if !is_valid_git_url(&url) {
                return Err(SkillSourceError::new(
                    &raw_display,
                    format!("'url' is not a well-formed git url (got '{url}')"),
                ));
            }
            let (git_ref, sha) = extract_ref_sha(map, &raw_display)?;
            Ok(ResolvedPluginSource::Git(GitCloneSpec {
                url,
                git_ref,
                sha,
                subdir: None,
            }))
        }
        Some("github") => {
            let url =
                normalize_repo_shorthand(&require_str(map, "repo", &raw_display)?, GITHUB_HOST)?;
            let (git_ref, sha) = extract_ref_sha(map, &raw_display)?;
            Ok(ResolvedPluginSource::Git(GitCloneSpec {
                url,
                git_ref,
                sha,
                subdir: None,
            }))
        }
        Some("cnb") => {
            let url = normalize_repo_shorthand(&require_str(map, "repo", &raw_display)?, CNB_HOST)?;
            let (git_ref, sha) = extract_ref_sha(map, &raw_display)?;
            Ok(ResolvedPluginSource::Git(GitCloneSpec {
                url,
                git_ref,
                sha,
                subdir: None,
            }))
        }
        Some("git-subdir") => {
            let raw_url = require_str(map, "url", &raw_display)?;
            // url 亦接受 GitHub 简写 owner/repo 或 SSH URL（协议 §5.2.3）。
            let url = if is_valid_git_url(&raw_url) {
                raw_url
            } else {
                normalize_repo_shorthand(&raw_url, GITHUB_HOST)?
            };
            let subdir = norm_rel_segments(&require_str(map, "path", &raw_display)?, &raw_display)?;
            let (git_ref, sha) = extract_ref_sha(map, &raw_display)?;
            Ok(ResolvedPluginSource::Git(GitCloneSpec {
                url,
                git_ref,
                sha,
                subdir: Some(subdir),
            }))
        }
        _ => {
            let shown = map.get("source").map_or("null".to_string(), value_display);
            Err(SkillSourceError::new(
                &raw_display,
                format!("unknown plugin source discriminator {shown} (expect url|github|git-subdir|cnb)"),
            ))
        }
    }
}

/// 从 marketplace source（`{type:"git", url}`）取克隆 URL / Extract the clone URL from a marketplace source。
///
/// SDK settings 的 marketplace source 仅 git url（不支持 `ref` / `sha`，与 plugin source 的 5 类**两套
/// 独立 schema**，disjoint union）。
///
/// # Errors
/// 非对象 / `type` ≠ `"git"` / `url` 非合法 git url 时返回 [`SkillSourceError`]。
pub fn marketplace_clone_url(source: &Value) -> Result<String, SkillSourceError> {
    let raw_display = value_display(source);
    let Some(map) = source.as_object() else {
        return Err(SkillSourceError::new(
            &raw_display,
            "marketplace source must be an object",
        ));
    };
    if map.get("type").and_then(Value::as_str) != Some("git") {
        let shown = map.get("type").map_or("null".to_string(), value_display);
        return Err(SkillSourceError::new(
            &raw_display,
            format!("marketplace source 'type' must be 'git' (got {shown})"),
        ));
    }
    match map.get("url").and_then(Value::as_str) {
        Some(url) if is_valid_git_url(url) => Ok(url.to_string()),
        _ => {
            let shown = map.get("url").map_or("null".to_string(), value_display);
            Err(SkillSourceError::new(
                &raw_display,
                format!("marketplace source 'url' is not a well-formed git url (got {shown})"),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 40 字符 hex SHA 测试常量。
    const SHA40: &str = "0123456789abcdef0123456789abcdef01234567";

    fn git(spec: ResolvedPluginSource) -> GitCloneSpec {
        match spec {
            ResolvedPluginSource::Git(g) => g,
            ResolvedPluginSource::Local(l) => panic!("expected Git, got Local({l:?})"),
        }
    }

    fn local(spec: ResolvedPluginSource) -> LocalPluginSource {
        match spec {
            ResolvedPluginSource::Local(l) => l,
            ResolvedPluginSource::Git(g) => panic!("expected Local, got Git({g:?})"),
        }
    }

    // ---- 相对路径 / relative-path source ----
    #[test]
    fn test_relative_path_sources() {
        // (raw, plugin_root, expected rel_path)
        let ok = [
            (json!("./my-plugin"), DEFAULT_PLUGIN_ROOT, "my-plugin"),
            (
                json!("./plugins/frontend-design"),
                DEFAULT_PLUGIN_ROOT,
                "plugins/frontend-design",
            ),
            (
                json!("data-toolkit"),
                DEFAULT_PLUGIN_ROOT,
                "plugins/data-toolkit",
            ),
            (
                json!("formatter"),
                "./vendor/plugins",
                "vendor/plugins/formatter",
            ),
        ];
        for (raw, root, expected) in ok {
            let resolved = resolve_plugin_source(&raw, root).expect("应解析为 Local");
            assert_eq!(local(resolved).rel_path, expected);
        }
        // 非法相对源。
        for raw in [json!("./../escape"), json!("foo/bar"), json!("./.")] {
            assert!(resolve_plugin_source(&raw, DEFAULT_PLUGIN_ROOT).is_err());
        }
    }

    // ---- url 源 / url source ----
    #[test]
    fn test_url_source() {
        let spec = git(resolve_plugin_source(
            &json!({"source": "url", "url": "https://example.com/team/p.git"}),
            DEFAULT_PLUGIN_ROOT,
        )
        .unwrap());
        assert_eq!(spec.url, "https://example.com/team/p.git");
        assert!(spec.git_ref.is_none() && spec.sha.is_none() && spec.subdir.is_none());

        // scp-like + ref + 大写 sha（归一为小写）。
        let spec = git(
            resolve_plugin_source(
                &json!({"source": "url", "url": "git@gitlab.com:team/p.git", "ref": "v1.0", "sha": SHA40.to_uppercase()}),
                DEFAULT_PLUGIN_ROOT,
            )
            .unwrap(),
        );
        assert_eq!(spec.git_ref.as_deref(), Some("v1.0"));
        assert_eq!(spec.sha.as_deref(), Some(SHA40));

        // 缺 url / 非法 url → 抛。
        assert!(resolve_plugin_source(&json!({"source": "url"}), DEFAULT_PLUGIN_ROOT).is_err());
        assert!(resolve_plugin_source(
            &json!({"source": "url", "url": "not a url"}),
            DEFAULT_PLUGIN_ROOT
        )
        .is_err());
    }

    // ---- github / cnb 简写糖 / shorthand ----
    #[test]
    fn test_github_cnb_shorthand() {
        let spec = git(resolve_plugin_source(
            &json!({"source": "github", "repo": "turingfocus/tfs-auth-plugin", "ref": "v0.3.1"}),
            DEFAULT_PLUGIN_ROOT,
        )
        .unwrap());
        assert_eq!(
            spec.url,
            "https://github.com/turingfocus/tfs-auth-plugin.git"
        );
        assert_eq!(spec.git_ref.as_deref(), Some("v0.3.1"));

        // 携带 .git 后缀：归一后仍单一 .git。
        let spec = git(resolve_plugin_source(
            &json!({"source": "github", "repo": "owner/repo.git"}),
            DEFAULT_PLUGIN_ROOT,
        )
        .unwrap());
        assert_eq!(spec.url, "https://github.com/owner/repo.git");

        // cnb host。
        let spec = git(resolve_plugin_source(
            &json!({"source": "cnb", "repo": "turingfocus/tfs-auth-plugin"}),
            DEFAULT_PLUGIN_ROOT,
        )
        .unwrap());
        assert_eq!(spec.url, "https://cnb.cool/turingfocus/tfs-auth-plugin.git");

        // 完整 URL / 单段 → 抛。
        for repo in ["https://github.com/owner/repo", "owner-only"] {
            assert!(resolve_plugin_source(
                &json!({"source": "github", "repo": repo}),
                DEFAULT_PLUGIN_ROOT
            )
            .is_err());
        }
    }

    // ---- git-subdir ----
    #[test]
    fn test_git_subdir_source() {
        let spec = git(
            resolve_plugin_source(
                &json!({"source": "git-subdir", "url": "https://github.com/adobe/skills.git", "path": "plugins/creative-cloud/adobe"}),
                DEFAULT_PLUGIN_ROOT,
            )
            .unwrap(),
        );
        assert_eq!(spec.url, "https://github.com/adobe/skills.git");
        assert_eq!(spec.subdir.as_deref(), Some("plugins/creative-cloud/adobe"));

        // url 为简写 → 归一化到 GitHub。
        let spec = git(
            resolve_plugin_source(
                &json!({"source": "git-subdir", "url": "adobe/skills", "path": "plugins/x", "sha": SHA40}),
                DEFAULT_PLUGIN_ROOT,
            )
            .unwrap(),
        );
        assert_eq!(spec.url, "https://github.com/adobe/skills.git");
        assert_eq!(spec.subdir.as_deref(), Some("plugins/x"));

        // path 含 .. / 缺 path → 抛。
        assert!(resolve_plugin_source(
            &json!({"source": "git-subdir", "url": "https://h/r.git", "path": "../escape"}),
            DEFAULT_PLUGIN_ROOT
        )
        .is_err());
        assert!(resolve_plugin_source(
            &json!({"source": "git-subdir", "url": "https://h/r.git"}),
            DEFAULT_PLUGIN_ROOT
        )
        .is_err());
    }

    // ---- 通用错误 / generic errors ----
    #[test]
    fn test_generic_errors() {
        // 未知 discriminator。
        assert!(resolve_plugin_source(
            &json!({"source": "npm", "package": "x"}),
            DEFAULT_PLUGIN_ROOT
        )
        .is_err());
        // 非 string / 非 object。
        assert!(resolve_plugin_source(&json!(12345), DEFAULT_PLUGIN_ROOT).is_err());
        // 坏 sha（非 40-hex）。
        assert!(resolve_plugin_source(
            &json!({"source": "url", "url": "https://h/r.git", "sha": "abc123"}),
            DEFAULT_PLUGIN_ROOT
        )
        .is_err());
        // ref 非字符串。
        assert!(resolve_plugin_source(
            &json!({"source": "url", "url": "https://h/r.git", "ref": 123}),
            DEFAULT_PLUGIN_ROOT
        )
        .is_err());
    }

    // ---- normalize_repo_shorthand standalone ----
    #[test]
    fn test_normalize_repo_shorthand() {
        assert_eq!(
            normalize_repo_shorthand("owner/repo", GITHUB_HOST).unwrap(),
            "https://github.com/owner/repo.git"
        );
        assert_eq!(
            normalize_repo_shorthand("g/p", CNB_HOST).unwrap(),
            "https://cnb.cool/g/p.git"
        );
        // scp-like / 完整 URL → 抛。
        assert!(normalize_repo_shorthand("git@github.com:owner/repo", GITHUB_HOST).is_err());
        assert!(normalize_repo_shorthand("https://github.com/owner/repo", GITHUB_HOST).is_err());
        assert!(normalize_repo_shorthand("owner-only", GITHUB_HOST).is_err());
    }

    // ---- marketplace_clone_url ----
    #[test]
    fn test_marketplace_clone_url() {
        assert_eq!(
            marketplace_clone_url(&json!({"type": "git", "url": "git@github.com:team/skills.git"}))
                .unwrap(),
            "git@github.com:team/skills.git"
        );
        // 错误 type / 非法 url / 非对象 → 抛。
        assert!(marketplace_clone_url(&json!({"type": "github", "url": "owner/repo"})).is_err());
        assert!(marketplace_clone_url(&json!({"type": "git", "url": "bogus"})).is_err());
        assert!(marketplace_clone_url(&json!("https://h/r.git")).is_err());
    }
}
