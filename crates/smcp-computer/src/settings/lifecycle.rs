/*!
* 文件名: lifecycle.rs
* 作者: JQQ
* 创建日期: 2026/06/30
* 最后修改日期: 2026/06/30
* 版权: 2023 JQQ. All rights reserved.
* 依赖: serde_json, crate::settings::{installer,reconciler,scope,store}, crate::skills::{staging,...}
* 描述: marketplace 生命周期高层编排（add / refresh / remove）非 CLI 收口（#94）
*       Non-CLI marketplace lifecycle orchestration (add / refresh / remove).
*/

//! marketplace 高层编排核心（**非 CLI**、返回结构化结果）/ marketplace orchestration core。
//!
//! 协议依据 / Protocol: a2c-smcp-protocol §10.x（marketplace add/refresh/remove 编排、clone/对账、卸载级联）。
//!
//! ## 为什么独立成模块（#94 北极星 = #93 三层划分）
//!
//! marketplace add/refresh/remove 的 **stage + prune** 高层编排此前只活在
//! [`crate::cli::commands::marketplace`]（`#[cfg(feature = "cli")]`），且返回 CLI 退出码 `i32`——把"治理层
//! 高层入口"与"CLI 表现层"耦死，GUI/Tauri 产品 client 必须启用 `cli` feature 才能驱动 marketplace。本模块把
//! 该编排抬到 **非 CLI** 层，返回结构化 [`GovernanceError`] / Outcome：
//!
//! - [`Computer`](crate::computer::Computer) 级方法（`add_marketplace` 等）= 取写锁 + 调本模块 + `mark_skills_dirty`。
//! - CLI handler = 信任门（user-scope，**不**属 `skill_home` 治理边界）+ 结构化结果 → 退出码映射。
//!
//! 两者是**兄弟薄封装**，共享本模块这一份编排真相（DRY）。本模块**不**碰 trust（user settings，§16
//! `known_marketplaces.json` 不带 trusted 字段）、**不**碰 `mark_skills_dirty`（去抖器属 `Computer` 运行态）。

use std::path::Path;

use serde_json::{json, Map, Value};

use crate::settings::installer::{
    uninstall_plugin, McpInstallHooks, PluginInstallError, UninstallOptions,
};
use crate::settings::reconciler::{prune_marketplaces, SkillGovernanceStore};
use crate::settings::scope::EnvMap;
use crate::settings::store::{
    load_installed_plugins, load_known_marketplaces, update_known_marketplaces,
    FileSkillGovernanceStore, SettingsStoreError,
};
use crate::settings::{is_valid_git_url, is_valid_marketplace_name, KnownMarketplaceEntry};
use crate::skills::{
    normalize_repo_shorthand, stage_marketplace_skills, MarketplaceStageOptions, SkillRegistry,
    GITHUB_HOST,
};

// ---------------------------------------------------------------------------
// 错误 / Errors
// ---------------------------------------------------------------------------
/// marketplace/plugin 生命周期编排失败（**结构化、非退出码**）/ Structured governance lifecycle failure。
///
/// CLI handler 据各变体映射退出码（`CloneFailed` → 网络错 2，其余 → 用户错 1）+ 拼装用户面文案；GUI/Tauri
/// client 直接消费结构化变体。携带的数据（git_url / name）足以让消费方自定义文案。
#[derive(Debug, thiserror::Error)]
pub enum GovernanceError {
    /// 既非合法 git URL 也非 `owner/repo` 简写 / not a well-formed git URL or `owner/repo` shorthand。
    #[error("not a well-formed git url or owner/repo shorthand: {0:?}")]
    InvalidUrl(String),
    /// 无法从 URL 派生合法 marketplace 名（须显式指定）/ cannot derive a valid marketplace name。
    #[error("cannot derive a valid marketplace name from {0:?}")]
    InvalidName(String),
    /// marketplace 名已存在（add 拒绝覆盖）/ marketplace name already exists。
    #[error("marketplace name conflict: {0:?} already exists")]
    DuplicateMarketplace(String),
    /// 未知 marketplace（refresh/remove 目标不存在）/ unknown marketplace。
    #[error("unknown marketplace: {0:?}")]
    UnknownMarketplace(String),
    /// clone/refresh 失败（stage 降级未落 `known_marketplaces`）/ clone or refresh failed。
    #[error("clone/refresh failed for {0:?} (see logs)")]
    CloneFailed(String),
    /// plugin 卸载级联失败（remove marketplace 级联期）/ plugin uninstall cascade failed。
    #[error(transparent)]
    Plugin(#[from] PluginInstallError),
    /// 治理账本持久化失败（锁 / I/O）/ governance ledger persistence failed。
    #[error(transparent)]
    Store(#[from] SettingsStoreError),
}

// ---------------------------------------------------------------------------
// 结构化结果 / Structured outcomes
// ---------------------------------------------------------------------------
/// `add_marketplace` 成功结果 / successful add outcome。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketplaceAddOutcome {
    /// 解析后的 marketplace 名 / resolved marketplace name。
    pub name: String,
    /// 归一后的 git URL / normalized git URL。
    pub url: String,
    /// 注册的 SKILL 名（`no_clone` → 空）/ registered SKILL names (empty when `no_clone`)。
    pub skills: Vec<String>,
    /// 仅注册意图、未 clone（`--no-clone`）/ registered intent only, not cloned。
    pub no_clone: bool,
}

/// 单个 marketplace 的 refresh 结果分类 / per-marketplace refresh classification。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshStatus {
    /// commitSha 变化（拉到新内容）/ commit changed。
    Updated,
    /// commitSha 未变 / unchanged。
    Unchanged,
    /// 目标 marketplace 不在 `known_marketplaces`（视为失败）/ target not known (treated as failed)。
    Missing,
}

impl RefreshStatus {
    /// 稳定串形（对齐既有 CLI/JSON 文案）/ stable string form。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            RefreshStatus::Updated => "updated",
            RefreshStatus::Unchanged => "unchanged",
            RefreshStatus::Missing => "missing",
        }
    }
}

/// `refresh_marketplaces` 的逐 marketplace 行 / one row per marketplace refreshed。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketplaceRefreshRow {
    /// marketplace 名 / marketplace name。
    pub name: String,
    /// 刷新分类 / refresh classification。
    pub status: RefreshStatus,
    /// 注册的 SKILL 数 / number of SKILLs registered。
    pub skills: usize,
}

/// `remove_marketplace` 成功结果 / successful remove outcome。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketplaceRemoveOutcome {
    /// 被移除的 marketplace 名 / removed marketplace name。
    pub name: String,
    /// 实际 prune 的 marketplace 名 / actually-pruned marketplace names。
    pub pruned: Vec<String>,
    /// 级联卸载的 plugin id / cascaded-uninstalled plugin ids。
    pub uninstalled_plugins: Vec<String>,
    /// 是否保留 installed plugin 记录为孤儿 / kept installed plugin records as orphans。
    pub kept_plugins: bool,
}

// ---------------------------------------------------------------------------
// 选项 / Params
// ---------------------------------------------------------------------------
/// `add_marketplace` 编排参数（**不含** trust / confirm / json——那些是 CLI 表现层）/ add params。
#[derive(Default)]
pub struct AddMarketplaceParams<'a> {
    /// 显式 marketplace 名（缺省经 [`default_marketplace_name`] 派生）/ explicit name。
    pub name: Option<&'a str>,
    /// 物化记录的 autoUpdate / autoUpdate flag。
    pub auto_update: bool,
    /// 仅注册意图、不 clone（§4.2 debug 用）/ register intent only。
    pub no_clone: bool,
}

/// `remove_marketplace` 编排参数 / remove params。
#[derive(Default)]
pub struct RemoveMarketplaceParams<'a> {
    /// 仅 prune clone、保留 installed plugin 记录（=孤儿）/ keep installed plugin records as orphans。
    pub keep_plugins: bool,
    /// 级联卸载所需 MCP 注入回调（提供 `remove_server`）/ MCP hooks for cascade uninstall。
    pub hooks: Option<&'a dyn McpInstallHooks>,
}

// ---------------------------------------------------------------------------
// 身份解析（纯函数）/ Identity resolution (pure)
// ---------------------------------------------------------------------------
/// 归一 marketplace git URL：完整 URL 原样；裸 `owner/repo` 简写按 GitHub 糖展开 / normalize a git URL。
#[must_use]
pub fn normalize_marketplace_url(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if is_valid_git_url(raw) {
        return Some(raw.to_string());
    }
    normalize_repo_shorthand(raw, GITHUB_HOST).ok()
}

/// 从 git URL 末段派生严格-kebab marketplace 名（去 `.git`、非字母数字折叠为 `-`）/ derive a kebab name。
#[must_use]
pub fn default_marketplace_name(url: &str) -> Option<String> {
    let tail = url.trim_end_matches('/');
    let seg = tail.rsplit(['/', ':']).next().unwrap_or(tail);
    let seg = seg.strip_suffix(".git").unwrap_or(seg);

    // `re.sub(r"[^a-z0-9]+", "-", seg.lower()).strip("-")`：小写后非 [a-z0-9] 连续段折叠为单 `-`。
    let mut slug = String::new();
    let mut prev_dash = false;
    for ch in seg.to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            prev_dash = false;
        } else if !prev_dash {
            slug.push('-');
            prev_dash = true;
        }
    }
    let slug = slug.trim_matches('-').to_string();
    (!slug.is_empty() && is_valid_marketplace_name(&slug)).then_some(slug)
}

/// 解析后的 marketplace 身份（归一 URL + 合法名）/ resolved marketplace identity。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketplaceIdentity {
    /// 合法 marketplace 名 / valid marketplace name。
    pub name: String,
    /// 归一后的 git URL / normalized git URL。
    pub url: String,
}

/// 把原始 `git_url` + 可选显式名解析为 [`MarketplaceIdentity`]（归一 + 派生 + 校验）/ resolve identity。
///
/// **纯函数**（不触盘）：CLI 信任门与 [`Computer`](crate::computer::Computer) 级 API 共用，确保两路同一身份语义。
///
/// # Errors
/// URL 非法 → [`GovernanceError::InvalidUrl`]；无法派生/非法名 → [`GovernanceError::InvalidName`]。
pub fn resolve_marketplace_identity(
    git_url: &str,
    explicit_name: Option<&str>,
) -> Result<MarketplaceIdentity, GovernanceError> {
    let url = normalize_marketplace_url(git_url)
        .ok_or_else(|| GovernanceError::InvalidUrl(git_url.to_string()))?;
    let name = explicit_name
        .map(str::to_string)
        .or_else(|| default_marketplace_name(&url))
        .filter(|n| is_valid_marketplace_name(n))
        .ok_or_else(|| GovernanceError::InvalidName(git_url.to_string()))?;
    Ok(MarketplaceIdentity { name, url })
}

// ---------------------------------------------------------------------------
// 内部辅助 / Internal helpers
// ---------------------------------------------------------------------------
/// 由 `home` / `env` 装配文件式治理存储（prune/recorder 共用）/ assemble the file governance store。
fn make_store(home: &Path, env: Option<&EnvMap>) -> FileSkillGovernanceStore {
    match env {
        Some(e) => FileSkillGovernanceStore::with_env(home, e.clone()),
        None => FileSkillGovernanceStore::new(home),
    }
}

/// `known_marketplaces.json` 是否已含该名 / whether the ledger already knows this marketplace name。
#[must_use]
pub fn marketplace_name_taken(home: &Path, env: Option<&EnvMap>, name: &str) -> bool {
    load_known_marketplaces(Some(home), env)
        .account
        .marketplaces
        .contains_key(name)
}

// ---------------------------------------------------------------------------
// 编排：add / refresh / remove
// ---------------------------------------------------------------------------
/// 注册意图（`no_clone`）或 stage（clone + 注册 SKILL）一个**已解析身份**的 marketplace / register-or-stage。
///
/// 本函数是 add 的"变更核心"：身份解析与重名校验由调用方先行（CLI 在信任门前、Computer 级在本模块
/// [`add_marketplace`] 内），以保留"重名先于信任提示"的既有时序。
///
/// # Errors
/// `no_clone` 写账本失败 → [`GovernanceError::Store`]；clone/stage 降级未落账 → [`GovernanceError::CloneFailed`]。
///
/// 仅 `pub(crate)`：这是"已解析身份 + 已重名校验"后的**变更核心建筑块**，公开会诱使调用方跳过 dup 校验
/// （footgun）。crate 内由 [`add_marketplace`]（Computer 路）与 CLI handler（信任门后）组合调用；外部消费方
/// 用 [`add_marketplace`] / [`Computer::add_marketplace`](crate::computer::Computer::add_marketplace)。
pub(crate) async fn register_or_stage_marketplace(
    registry: &mut SkillRegistry,
    home: &Path,
    env: Option<&EnvMap>,
    identity: &MarketplaceIdentity,
    params: &AddMarketplaceParams<'_>,
) -> Result<MarketplaceAddOutcome, GovernanceError> {
    let MarketplaceIdentity { name, url } = identity;

    if params.no_clone {
        // 仅注册意图（不推荐，§4.2 debug 用）：记 known_marketplaces 但不 clone/stage。
        let name_for_write = name.clone();
        let url_for_write = url.clone();
        let auto = params.auto_update;
        update_known_marketplaces(
            move |file| {
                let mut extra = Map::new();
                extra.insert("installLocation".to_string(), json!(""));
                extra.insert("autoUpdate".to_string(), json!(auto));
                file.account.marketplaces.insert(
                    name_for_write,
                    KnownMarketplaceEntry {
                        source: json!({ "type": "git", "url": url_for_write }),
                        extra,
                    },
                );
            },
            Some(home),
            env,
        )?;
        return Ok(MarketplaceAddOutcome {
            name: name.clone(),
            url: url.clone(),
            skills: Vec::new(),
            no_clone: true,
        });
    }

    let source = json!({ "type": "git", "url": url });
    let store = make_store(home, env);
    let registered = stage_marketplace_skills(
        name,
        &source,
        registry,
        home,
        MarketplaceStageOptions {
            plugin_filter: None,
            auto_update: params.auto_update,
            refresh: false,
            timeout: None,
            env,
            recorder: Some(store.as_recorder()),
        },
    )
    .await;

    // 成功判定：stage 失败降级（不抛、返回空 + 不写 known_marketplaces）；据物化记录是否落盘判定。
    if !marketplace_name_taken(home, env, name) {
        return Err(GovernanceError::CloneFailed(url.clone()));
    }
    Ok(MarketplaceAddOutcome {
        name: name.clone(),
        url: url.clone(),
        skills: registered,
        no_clone: false,
    })
}

/// 添加 marketplace（解析身份 + 重名校验 + register-or-stage）——非 CLI 高层入口 / add a marketplace。
///
/// 信任门由 CLI/产品 client 在调用前自理（user-scope 决策，不属 `skill_home` 治理边界）。
///
/// # Errors
/// 见 [`GovernanceError`]（非法 URL/名、重名、clone 失败、账本写失败）。
pub async fn add_marketplace(
    registry: &mut SkillRegistry,
    home: &Path,
    env: Option<&EnvMap>,
    git_url: &str,
    params: AddMarketplaceParams<'_>,
) -> Result<MarketplaceAddOutcome, GovernanceError> {
    let identity = resolve_marketplace_identity(git_url, params.name)?;
    if marketplace_name_taken(home, env, &identity.name) {
        return Err(GovernanceError::DuplicateMarketplace(identity.name));
    }
    register_or_stage_marketplace(registry, home, env, &identity, &params).await
}

/// 刷新 marketplace（`git pull` 失败则全量重 clone；逐 marketplace 对账分类）/ refresh marketplaces。
///
/// `target == "all"` → 全部已知 marketplace；否则单个目标。未知目标 → [`RefreshStatus::Missing`] 行
/// （**不**整体报错，对齐既有 CLI 语义：refresh 永远成功，失败逐行汇报）。
pub async fn refresh_marketplaces(
    registry: &mut SkillRegistry,
    home: &Path,
    env: Option<&EnvMap>,
    target: &str,
) -> Vec<MarketplaceRefreshRow> {
    let mps = load_known_marketplaces(Some(home), env).account;
    let names: Vec<String> = if target == "all" {
        mps.marketplaces.keys().cloned().collect()
    } else {
        vec![target.to_string()]
    };

    let mut rows: Vec<MarketplaceRefreshRow> = Vec::new();
    for nm in &names {
        let Some(rec) = mps.marketplaces.get(nm) else {
            rows.push(MarketplaceRefreshRow {
                name: nm.clone(),
                status: RefreshStatus::Missing,
                skills: 0,
            });
            continue;
        };
        let before = rec.extra.get("commitSha").cloned();
        let source = rec.source.clone();
        let auto = rec
            .extra
            .get("autoUpdate")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let store = make_store(home, env);
        let registered = stage_marketplace_skills(
            nm,
            &source,
            registry,
            home,
            MarketplaceStageOptions {
                plugin_filter: None,
                auto_update: auto,
                refresh: true,
                timeout: None,
                env,
                recorder: Some(store.as_recorder()),
            },
        )
        .await;
        let after_sha = load_known_marketplaces(Some(home), env)
            .account
            .marketplaces
            .get(nm)
            .and_then(|r| r.extra.get("commitSha").cloned());
        let status = if after_sha != before {
            RefreshStatus::Updated
        } else {
            RefreshStatus::Unchanged
        };
        rows.push(MarketplaceRefreshRow {
            name: nm.clone(),
            status,
            skills: registered.len(),
        });
    }
    rows
}

/// 移除 marketplace（默认级联卸载其下 installed plugin + prune clone；`keep_plugins` 仅 prune）/ remove。
///
/// trust 撤销由 CLI 在调用后自理（user-scope，best-effort，不属本编排）。
///
/// # Errors
/// 未知 marketplace → [`GovernanceError::UnknownMarketplace`]。级联卸载内部失败按既有语义吞掉（best-effort，
/// 仅统计 `Ok(true)`），不向上抛断。
pub async fn remove_marketplace(
    registry: &mut SkillRegistry,
    home: &Path,
    env: Option<&EnvMap>,
    name: &str,
    params: RemoveMarketplaceParams<'_>,
) -> Result<MarketplaceRemoveOutcome, GovernanceError> {
    if !marketplace_name_taken(home, env, name) {
        return Err(GovernanceError::UnknownMarketplace(name.to_string()));
    }

    let mut uninstalled: Vec<String> = Vec::new();
    if !params.keep_plugins {
        let suffix = format!("@{name}");
        let installed = load_installed_plugins(Some(home), env).account;
        let victims: Vec<String> = installed
            .plugins
            .keys()
            .filter(|pid| pid.ends_with(&suffix))
            .cloned()
            .collect();
        for pid in &victims {
            let res = uninstall_plugin(
                pid,
                registry,
                home,
                UninstallOptions {
                    scope: None,
                    keep_servers: false,
                    env,
                },
                params.hooks,
            )
            .await;
            if matches!(res, Ok(true)) {
                uninstalled.push(pid.clone());
            }
        }
    }

    let store = make_store(home, env);
    let pruned = prune_marketplaces(&[name.to_string()], registry, home, &store);
    Ok(MarketplaceRemoveOutcome {
        name: name.to_string(),
        pruned,
        uninstalled_plugins: uninstalled,
        kept_plugins: params.keep_plugins,
    })
}

#[cfg(test)]
mod tests {
    // 注意：本模块是**非 CLI** 层，测试**不得**依赖 cli-gated 的 `crate::cli::commands::test_env`，否则
    // `cargo test-ws`（默认特性、无 cli）将无法编译——而这恰会反噬本特性"GUI 无需 cli feature"的目标。
    // marketplace 账本（known_marketplaces.json）落在 `home`（skill_home）内，故 `env = None` 即完全 hermetic。
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn normalize_url_passthrough_and_shorthand() {
        assert_eq!(
            normalize_marketplace_url("https://github.com/acme/skills.git"),
            Some("https://github.com/acme/skills.git".to_string())
        );
        assert_eq!(
            normalize_marketplace_url("acme/skills"),
            Some("https://github.com/acme/skills.git".to_string())
        );
        assert_eq!(normalize_marketplace_url("not a url"), None);
    }

    #[test]
    fn default_name_derivation() {
        assert_eq!(
            default_marketplace_name("https://github.com/acme/My_Skills.git"),
            Some("my-skills".to_string())
        );
        assert_eq!(
            default_marketplace_name("git@github.com:acme/cool-repo.git"),
            Some("cool-repo".to_string())
        );
    }

    #[test]
    fn resolve_identity_maps_errors() {
        // 非法 URL → InvalidUrl。
        assert!(matches!(
            resolve_marketplace_identity("not a url", None),
            Err(GovernanceError::InvalidUrl(_))
        ));
        // 合法 URL + 派生名。
        let id = resolve_marketplace_identity("acme/skills", None).unwrap();
        assert_eq!(id.name, "skills");
        assert_eq!(id.url, "https://github.com/acme/skills.git");
        // 显式名覆盖派生。
        let id2 = resolve_marketplace_identity("acme/skills", Some("custom")).unwrap();
        assert_eq!(id2.name, "custom");
    }

    #[tokio::test]
    async fn add_no_clone_registers_intent_and_returns_outcome() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let mut registry = SkillRegistry::new();

        let outcome = add_marketplace(
            &mut registry,
            home,
            None,
            "acme/skills",
            AddMarketplaceParams {
                no_clone: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(outcome.name, "skills");
        assert!(outcome.no_clone);
        assert!(outcome.skills.is_empty());
        assert!(marketplace_name_taken(home, None, "skills"));
    }

    #[tokio::test]
    async fn add_duplicate_is_structured_error() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let mut registry = SkillRegistry::new();
        let mk = || AddMarketplaceParams {
            no_clone: true,
            ..Default::default()
        };
        add_marketplace(&mut registry, home, None, "acme/skills", mk())
            .await
            .unwrap();
        // 同名再加（不同 owner，相同派生名）→ DuplicateMarketplace。
        assert!(matches!(
            add_marketplace(&mut registry, home, None, "other/skills", mk()).await,
            Err(GovernanceError::DuplicateMarketplace(n)) if n == "skills"
        ));
    }

    #[tokio::test]
    async fn remove_unknown_is_structured_error() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let mut registry = SkillRegistry::new();
        assert!(matches!(
            remove_marketplace(
                &mut registry,
                home,
                None,
                "ghost",
                RemoveMarketplaceParams::default()
            )
            .await,
            Err(GovernanceError::UnknownMarketplace(n)) if n == "ghost"
        ));
    }

    #[tokio::test]
    async fn remove_after_no_clone_add_prunes_entry() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let mut registry = SkillRegistry::new();
        add_marketplace(
            &mut registry,
            home,
            None,
            "acme/skills",
            AddMarketplaceParams {
                no_clone: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(marketplace_name_taken(home, None, "skills"));

        let outcome = remove_marketplace(
            &mut registry,
            home,
            None,
            "skills",
            RemoveMarketplaceParams {
                keep_plugins: true,
                hooks: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(outcome.name, "skills");
        assert!(outcome.kept_plugins);
        assert!(!marketplace_name_taken(home, None, "skills"));
    }

    #[tokio::test]
    async fn refresh_unknown_target_yields_missing_row() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let mut registry = SkillRegistry::new();
        let rows = refresh_marketplaces(&mut registry, home, None, "ghost").await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "ghost");
        assert_eq!(rows[0].status, RefreshStatus::Missing);
    }
}
