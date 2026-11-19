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
use url::Url;

use crate::settings::installer::{
    uninstall_plugin, McpInstallHooks, PluginInstallError, UninstallOptions,
};
use crate::settings::reconciler::{prune_marketplaces, SkillGovernanceStore};
use crate::settings::redaction::{
    git_source_for_error, known_git_source_for_display, redact_git_urls_in_text,
    untrusted_name_for_display, REDACTED_GIT_SOURCE,
};
use crate::settings::scope::{resolve_settings, EnvMap, ResolveSettingsArgs};
use crate::settings::store::{
    installed_marketplace_order, load_installed_plugins, load_installed_plugins_intent,
    load_known_marketplaces, resolve_install_location, update_known_marketplaces_checked,
    FileSkillGovernanceStore, SettingsStoreError,
};
use crate::settings::{is_valid_git_url, is_valid_marketplace_name, KnownMarketplaceEntry};
use crate::skills::staging::{
    commit_prepared_marketplace, git_remote_origin, materialize_marketplace_clone,
    prepare_marketplace_add, refresh_marketplace_clone, same_source_url,
    MarketplaceMaterializeOptions, DEFAULT_GIT_TIMEOUT,
};
use crate::skills::{
    compare_marketplace_sources, marketplace_skill_dir, normalize_repo_shorthand,
    read_marketplace_manifest, MarketplaceSourceRelation, MarketplaceStageError, SkillRegistry,
    GITHUB_HOST,
};

// ---------------------------------------------------------------------------
// 错误 / Errors
// ---------------------------------------------------------------------------
/// marketplace/plugin 生命周期编排失败（**结构化、非退出码**）/ Structured governance lifecycle failure。
///
/// CLI handler 据各变体映射退出码（`CloneFailed` → 网络错 2，其余 → 用户错 1）+ 拼装用户面文案；GUI/Tauri
/// client 直接消费结构化变体。所有字符串载荷均可安全进入公开 `Debug` / `Display`：URL 型输入先脱敏，
/// 名称错误只携名称，clone 错误只携 marketplace 名。
#[derive(Debug, thiserror::Error)]
pub enum GovernanceError {
    /// 既非合法 git URL 也非 `owner/repo` 简写 / not a well-formed git URL or `owner/repo` shorthand。
    #[error("not a well-formed git url or owner/repo shorthand: {0:?}")]
    InvalidUrl(String),
    /// 非法 marketplace 名（显式期望名不合严格 kebab）/ invalid marketplace name。
    #[error("invalid marketplace name: {0:?}")]
    InvalidName(String),
    /// 清单名缺失/非法、期望名不匹配等 #272 校验失败 / manifest-identity validation failure。
    #[error(transparent)]
    Validation(#[from] MarketplaceStageError),
    /// marketplace 名已存在（同名不同源，add 拒绝覆盖；提示 remove 旧源）/ name already exists。
    #[error(
        "marketplace name conflict: {0:?} is already registered from a different source; remove it first"
    )]
    DuplicateMarketplace(String),
    /// 同名但无法证明是否同源（fail-closed）/ same name, source equivalence unverifiable。
    #[error(
        "cannot verify whether this source matches the registered source for {0:?}; refusing (fail-closed)"
    )]
    SourceEquivalenceUnknown(String),
    /// 同一 source 已用另一注册名注册（拒绝一源多名）/ source already registered under another name。
    #[error(
        "this source is already registered as marketplace {existing:?}; remove it before adding under another name"
    )]
    SourceAlreadyRegistered { existing: String },
    /// remove 后同名 re-add，但仍存在 `<plugin>@<name>` 意图引用 / dangling plugin references。
    #[error(
        "marketplace name {name:?} is still referenced by installed or enabled plugins: {plugin_ids:?}"
    )]
    MarketplaceNameReferenced {
        name: String,
        plugin_ids: Vec<String>,
    },
    /// 未知 marketplace（refresh/remove 目标不存在）/ unknown marketplace。
    #[error("unknown marketplace: {0:?}")]
    UnknownMarketplace(String),
    /// clone/refresh 失败，载荷为 marketplace 名（stage 降级未落 `known_marketplaces`）/ clone failed。
    #[error("clone/refresh failed for marketplace {0:?} (see logs)")]
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
    /// 本次是否实际改变了状态（同源重复 add 幂等 → `false`）/ whether anything actually changed。
    pub changed: bool,
    /// 本次是否**新建**了注册（幂等 / 自愈路径 → `false`）/ whether this call created the registration。
    ///
    /// CLI 在 trust 写失败时只回滚 `created == true` 的注册，绝不 prune 既有注册。
    pub created: bool,
    /// 幂等 add 是否顺带更新了账本旗标（如 `--auto-update`）/ whether flags were updated on an idempotent add。
    pub flags_updated: bool,
}

/// 单个 marketplace 的 refresh 结果分类 / per-marketplace refresh classification。
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshStatus {
    /// commit 变化（拉到新内容）/ commit changed。
    Updated,
    /// commit 未变 / unchanged。
    Unchanged,
    /// 目标 marketplace 不在 `known_marketplaces`（视为失败）/ target not known (treated as failed)。
    Missing,
    /// 来料清单名与基线不一致 / 缺失 / 非法 → 拒绝，旧注册与内容保留（#272）。
    Rejected,
}

impl RefreshStatus {
    /// 稳定串形（对齐既有 CLI/JSON 文案）/ stable string form。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            RefreshStatus::Updated => "updated",
            RefreshStatus::Unchanged => "unchanged",
            RefreshStatus::Missing => "missing",
            RefreshStatus::Rejected => "rejected",
        }
    }
}

/// refresh 行的结构化诊断类别 / structured refresh diagnostic codes。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshDiagnosticCode {
    /// 来料清单名与基线不一致（拒绝）。
    ManifestNameChanged,
    /// 来料清单顶层 `name` 缺失（拒绝 / legacy 保留旧名）。
    ManifestNameMissing,
    /// 来料清单顶层 `name` 非法（拒绝 / legacy 保留旧名）。
    ManifestNameInvalid,
    /// 来料清单存在但不可读/损坏（保留旧内容；给行诊断，避免静默 unchanged）。
    ManifestUnreadable,
    /// legacy 注册首次 refresh 记录基线（清单名与注册名不同；advisory）。
    LegacyManifestNameMismatch,
    /// legacy 注册 refresh 遇清单缺失顶层 name（保留旧名；advisory）。
    LegacyManifestNameMissing,
    /// legacy 注册 refresh 遇清单顶层 name 非法（保留旧名；advisory）。
    LegacyManifestNameInvalid,
}

impl RefreshDiagnosticCode {
    /// 稳定串形 / stable string form。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            RefreshDiagnosticCode::ManifestNameChanged => "manifest_name_changed",
            RefreshDiagnosticCode::ManifestNameMissing => "manifest_name_missing",
            RefreshDiagnosticCode::ManifestNameInvalid => "manifest_name_invalid",
            RefreshDiagnosticCode::ManifestUnreadable => "manifest_unreadable",
            RefreshDiagnosticCode::LegacyManifestNameMismatch => "legacy_manifest_name_mismatch",
            RefreshDiagnosticCode::LegacyManifestNameMissing => "legacy_manifest_name_missing",
            RefreshDiagnosticCode::LegacyManifestNameInvalid => "legacy_manifest_name_invalid",
        }
    }
}

/// 逐 marketplace refresh 诊断（已脱敏、可展示）/ per-row refresh diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketplaceRefreshDiagnostic {
    /// 结构化类别 / structured code。
    pub code: RefreshDiagnosticCode,
    /// 可展示文案 / display message。
    pub message: String,
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
    /// 拒绝 / legacy advisory 的结构化诊断 / structured diagnostic.
    pub diagnostic: Option<MarketplaceRefreshDiagnostic>,
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
    /// **期望清单名**（D6 B：仅作校验，不覆盖清单 `name`）/ expected manifest name (validation only)。
    ///
    /// #272 起注册名恒为 marketplace.json 顶层 `name`；本字段不匹配时返回 `Validation` 错误。
    pub name: Option<&'a str>,
    /// 物化记录的 autoUpdate / autoUpdate flag。
    pub auto_update: bool,
    /// 仅注册意图、不 clone（#272 起要求已有 catalog + 显式期望名）/ register intent only。
    pub no_clone: bool,
    /// 引用守卫的 cwd 锚（project/local `enabledPlugins`；`None` → 进程 cwd）。
    pub config_cwd: Option<&'a Path>,
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
///
/// **#272 legacy 工具**：`add_marketplace` 不再使用 URL 派生名（注册名取清单顶层 `name`）；本函数
/// 保留供存量诊断 / 迁移工具使用（release notes 标注行为未变、用途收窄）。
///
/// #285：仅对 [`is_valid_git_url`] 认可且 known display 非占位符的源派生；派生段含 `:`/`@`/`%`
/// （userinfo 形态与 percent-encoded 残形）时返回 `None`。因此 hostless scheme、`host:repo`、
/// `SSH://host/repo`、`file:/abs/repo` 等旧可派生形态不再产出名称（公开 API 行为收窄，见 PR /
/// release notes）。
#[must_use]
pub fn default_marketplace_name(url: &str) -> Option<String> {
    let raw = url.trim();
    // #285：仅对合法 Git URL 派生；hostless/畸形（含公开函数直调）不得从 path slug 凭据。
    if !is_valid_git_url(raw) {
        return None;
    }
    // #285：与展示面同口径——known display 为占位符的源不得派生名称（scp/file 的 userinfo
    // 形态；`file:///tmp/user@host/repo` 会保守拒绝，与 #279 一致）。
    if known_git_source_for_display(raw) == REDACTED_GIT_SOURCE {
        return None;
    }
    let segment = if raw.contains("://") {
        let parsed = Url::parse(raw).ok()?;
        if !matches!(parsed.scheme(), "ssh" | "git" | "http" | "https" | "file") {
            return None;
        }
        parsed
            .path_segments()?
            .rfind(|part| !part.is_empty())?
            .to_string()
    } else {
        let (_, path) = raw.rsplit_once(':')?;
        path.split(['?', '#'])
            .next()
            .unwrap_or_default()
            .trim_end_matches('/')
            .rsplit('/')
            .find(|part| !part.is_empty())?
            .to_string()
    };
    let seg = segment.strip_suffix(".git").unwrap_or(&segment);
    // #285：派生段含 userinfo 形态（`:`/`@`）或 percent-encoded 残形（`%`）时 fail-closed。
    if seg.contains([':', '@', '%']) {
        return None;
    }

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

/// add 请求（归一 URL + 期望清单名）/ Add request: normalized URL + expected manifest name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketplaceAddRequest {
    /// 归一后的 git URL / normalized git URL。
    pub url: String,
    /// 期望清单名（D6 B：仅校验，不派生、不覆盖）/ expected manifest name。
    pub expected_name: Option<String>,
}

/// 归一 URL 并校验期望名单（**纯函数、不触盘、不派生名**）/ Normalize URL + validate expected name。
///
/// #272：注册名必须来自清单顶层 `name`，本函数只做 URL 归一与期望名语法校验。
///
/// # Errors
/// URL 非法 → [`GovernanceError::InvalidUrl`]；期望名非法 → [`GovernanceError::InvalidName`]。
pub fn resolve_marketplace_add_request(
    git_url: &str,
    expected_name: Option<&str>,
) -> Result<MarketplaceAddRequest, GovernanceError> {
    let url = normalize_marketplace_url(git_url)
        .ok_or_else(|| GovernanceError::InvalidUrl(git_source_for_error(git_url)))?;
    let expected_name = match expected_name {
        Some(name) if is_valid_marketplace_name(name) => Some(name.to_string()),
        Some(name) => return Err(GovernanceError::InvalidName(redact_git_urls_in_text(name))),
        None => None,
    };
    Ok(MarketplaceAddRequest { url, expected_name })
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
/// 注册认领结果 / registration claim result。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegistrationClaim {
    /// 同名同源已注册（幂等；`flags_updated` = 本次顺带更新了 autoUpdate 等旗标）。
    Idempotent {
        /// 本次是否更新了账本旗标（决定事件/回显）。
        flags_updated: bool,
    },
    /// 本次插入账本 / inserted by this call。
    Inserted,
}

/// 引用守卫：列出指向 `name` 的 `<plugin>@<name>` 意图引用（install intent ∪ 合并 settings）/ intent refs。
///
/// 不论 `enabledPlugins` 值为 true / false 都算意图引用（fail-closed，协议 §5.14 remove/re-add 守卫）。
fn marketplace_intent_references(
    home: &Path,
    env: Option<&EnvMap>,
    config_cwd: Option<&Path>,
    name: &str,
) -> Vec<String> {
    let suffix = format!("@{name}");
    let mut refs: Vec<String> = load_installed_plugins_intent(Some(home), env)
        .account
        .installed_plugins
        .iter()
        .filter(|id| id.ends_with(&suffix))
        .cloned()
        .collect();
    let cwd = config_cwd
        .map(Path::to_path_buf)
        .or_else(|| std::env::current_dir().ok());
    let settings = resolve_settings(ResolveSettingsArgs {
        cwd: cwd.as_deref(),
        env,
        flag_settings_path: None,
        policy_settings: None,
    });
    if let Some(Value::Object(map)) = settings.settings.get("enabledPlugins") {
        for key in map.keys() {
            if key.ends_with(&suffix) && !refs.iter().any(|r| r == key) {
                refs.push(key.clone());
            }
        }
    }
    refs.sort();
    refs.dedup();
    refs.into_iter()
        .map(|id| untrusted_name_for_display(&id))
        .collect()
}

/// 在账本文件锁内认领注册名（#272「注册提交点」）/ Claim the registration name under the ledger lock。
///
/// 锁内完成：同名同源 → 幂等；同名不可判定同源 → fail-closed；同源旧名 → 拒绝；残留引用 → 拒绝；
/// 否则插入账本（`manifestNameBaseline = name`）。clone 在锁外，锁内不信任任何预检。
#[allow(clippy::too_many_arguments)]
fn claim_marketplace_registration(
    home: &Path,
    env: Option<&EnvMap>,
    final_dir: &Path,
    name: &str,
    source: &Value,
    auto_update: bool,
    config_cwd: Option<&Path>,
    skip_reference_guard: bool,
) -> Result<RegistrationClaim, GovernanceError> {
    let target = resolve_install_location(final_dir);
    let decision = update_known_marketplaces_checked(Some(home), env, |file| {
        let mut persist = false;
        let decision: Result<RegistrationClaim, GovernanceError> = (|| {
            if let Some(existing) = file.account.marketplaces.get(name) {
                // 同名：只有可证明同源才幂等；不同 / 无法判定一律 fail-closed 拒绝（§5.14）。
                if compare_marketplace_sources(&existing.source, source)
                    != MarketplaceSourceRelation::Same
                {
                    return Err(GovernanceError::DuplicateMarketplace(name.to_string()));
                }
                // 幂等 add 也应用本次 `--auto-update`（不静默丢弃用户意图）。
                let needs_auto = auto_update
                    && existing.extra.get("autoUpdate").and_then(Value::as_bool) != Some(true);
                if needs_auto {
                    if let Some(entry) = file.account.marketplaces.get_mut(name) {
                        entry
                            .extra
                            .insert("autoUpdate".to_string(), Value::Bool(true));
                    }
                    persist = true;
                }
                return Ok(RegistrationClaim::Idempotent {
                    flags_updated: needs_auto,
                });
            }
            if let Some(existing) = file
                .account
                .marketplaces
                .iter()
                .find(|(n, e)| {
                    n.as_str() != name
                        && compare_marketplace_sources(&e.source, source)
                            == MarketplaceSourceRelation::Same
                })
                .map(|(n, _)| n.clone())
            {
                return Err(GovernanceError::SourceAlreadyRegistered {
                    existing: untrusted_name_for_display(&existing),
                });
            }
            // no_clone 已用 catalog origin 证明同源：重注册是恢复既有绑定，不是换源，豁免引用守卫
            // （§5.14 只要求「同名不同源」拒绝）。
            if !skip_reference_guard {
                let refs = marketplace_intent_references(home, env, config_cwd, name);
                if !refs.is_empty() {
                    return Err(GovernanceError::MarketplaceNameReferenced {
                        name: name.to_string(),
                        plugin_ids: refs,
                    });
                }
            }
            let mut extra = Map::new();
            extra.insert("installLocation".to_string(), Value::String(target.clone()));
            if auto_update {
                extra.insert("autoUpdate".to_string(), Value::Bool(true));
            }
            file.account.marketplaces.insert(
                name.to_string(),
                KnownMarketplaceEntry {
                    source: source.clone(),
                    manifest_name_baseline: Some(name.to_string()),
                    extra,
                },
            );
            Ok(RegistrationClaim::Inserted)
        })();
        if matches!(decision, Ok(RegistrationClaim::Inserted)) {
            persist = true;
        }
        (decision, persist)
    })?;
    decision
}

/// swap 失败补偿：删除本次刚插入的账本项（仅当 source 仍相同）/ Compensate a failed registration swap。
fn rollback_marketplace_registration(
    home: &Path,
    env: Option<&EnvMap>,
    name: &str,
    source: &Value,
) {
    let _ = update_known_marketplaces_checked(Some(home), env, |file| {
        if let Some(existing) = file.account.marketplaces.get(name) {
            if compare_marketplace_sources(&existing.source, source)
                == MarketplaceSourceRelation::Same
            {
                file.account.marketplaces.shift_remove(name);
                return ((), true);
            }
        }
        ((), false)
    });
}

/// 添加 marketplace（clone/读清单定名 + 锁内认领 + swap + 注册）——非 CLI 高层入口 / add a marketplace。
///
/// 信任门由 CLI/产品 client 在调用前自理（user-scope 决策，不属 `skill_home` 治理边界）。
///
/// # Errors
/// 见 [`GovernanceError`]（非法 URL/名、清单名校验失败、同名冲突、clone/账本失败）。
pub async fn add_marketplace(
    registry: &mut SkillRegistry,
    home: &Path,
    env: Option<&EnvMap>,
    git_url: &str,
    params: AddMarketplaceParams<'_>,
) -> Result<MarketplaceAddOutcome, GovernanceError> {
    let request = resolve_marketplace_add_request(git_url, params.name)?;
    let source = json!({ "type": "git", "url": request.url.clone() });
    if params.no_clone {
        let Some(expected) = request.expected_name.clone() else {
            return Err(MarketplaceStageError::ManifestNameMissing.into());
        };
        let final_dir = marketplace_skill_dir(home, &expected, &[]);
        let manifest = read_marketplace_manifest(&final_dir)
            .map_err(|_| GovernanceError::Validation(MarketplaceStageError::ManifestUnreadable))?;
        let name = crate::skills::manifest::marketplace_manifest_name(&manifest).map_err(|e| {
            GovernanceError::Validation(match e {
                crate::skills::manifest::ManifestNameError::Missing => {
                    MarketplaceStageError::ManifestNameMissing
                }
                crate::skills::manifest::ManifestNameError::Invalid => {
                    MarketplaceStageError::ManifestNameInvalid
                }
            })
        })?;
        if name != expected {
            return Err(MarketplaceStageError::ExpectedNameMismatch {
                expected,
                actual: name,
            }
            .into());
        }
        let Some(origin) = git_remote_origin(&final_dir, DEFAULT_GIT_TIMEOUT, env).await else {
            return Err(GovernanceError::SourceEquivalenceUnknown(name));
        };
        let origin_source = json!({ "type": "git", "url": origin.clone() });
        if !same_source_url(&request.url, &origin)
            && compare_marketplace_sources(&origin_source, &source)
                != MarketplaceSourceRelation::Same
        {
            return Err(GovernanceError::SourceEquivalenceUnknown(name));
        }
        let claim = claim_marketplace_registration(
            home,
            env,
            &final_dir,
            &name,
            &source,
            params.auto_update,
            params.config_cwd,
            // 同源已证（catalog origin）：恢复既有绑定，豁免引用守卫。
            true,
        )?;
        let flags_updated = match claim {
            RegistrationClaim::Idempotent { flags_updated } => flags_updated,
            RegistrationClaim::Inserted => false,
        };
        return Ok(MarketplaceAddOutcome {
            name,
            url: request.url,
            skills: Vec::new(),
            no_clone: true,
            changed: matches!(claim, RegistrationClaim::Inserted),
            created: matches!(claim, RegistrationClaim::Inserted),
            flags_updated,
        });
    }

    let prepared =
        prepare_marketplace_add(&source, request.expected_name.as_deref(), home, None, env)
            .await
            .map_err(|e| match e {
                // clone 失败保留网络错分类（CLI → 退出码 2）；注册名尚未知，载荷为空串。
                MarketplaceStageError::CloneFailed { marketplace } => {
                    GovernanceError::CloneFailed(marketplace)
                }
                other => GovernanceError::Validation(other),
            })?;
    let name = prepared.manifest_name.clone();
    let final_dir = marketplace_skill_dir(home, &name, &[]);
    let claim = claim_marketplace_registration(
        home,
        env,
        &final_dir,
        &name,
        &source,
        params.auto_update,
        params.config_cwd,
        // 普通 add：强制引用守卫（remove/re-add 不得静默重绑）。
        false,
    )?;
    let store = make_store(home, env);
    match claim {
        RegistrationClaim::Idempotent { flags_updated } => {
            // 同源重复 add 幂等；仅当 clone 树缺失时自愈（swap staging 并重新物化）。
            if final_dir.exists() {
                return Ok(MarketplaceAddOutcome {
                    name,
                    url: request.url,
                    skills: Vec::new(),
                    no_clone: false,
                    changed: false,
                    created: false,
                    flags_updated,
                });
            }
            let committed = commit_prepared_marketplace(prepared, home)?;
            let skills = materialize_marketplace_clone(
                &name,
                &source,
                &committed.dir,
                registry,
                home,
                MarketplaceMaterializeOptions {
                    auto_update: params.auto_update,
                    changed: true,
                    refresh: false,
                    env,
                    recorder: Some(store.as_recorder()),
                    manifest_name_baseline: None,
                },
            )
            .await;
            committed.discard_trash();
            Ok(MarketplaceAddOutcome {
                name,
                url: request.url,
                skills,
                no_clone: false,
                changed: true,
                created: false,
                flags_updated,
            })
        }
        RegistrationClaim::Inserted => {
            let committed = match commit_prepared_marketplace(prepared, home) {
                Ok(c) => c,
                Err(e) => {
                    rollback_marketplace_registration(home, env, &name, &source);
                    return Err(e.into());
                }
            };
            let skills = materialize_marketplace_clone(
                &name,
                &source,
                &committed.dir,
                registry,
                home,
                MarketplaceMaterializeOptions {
                    auto_update: params.auto_update,
                    changed: true,
                    refresh: false,
                    env,
                    recorder: Some(store.as_recorder()),
                    manifest_name_baseline: Some(&name),
                },
            )
            .await;
            committed.discard_trash();
            Ok(MarketplaceAddOutcome {
                name,
                url: request.url,
                skills,
                no_clone: false,
                changed: true,
                created: true,
                flags_updated: false,
            })
        }
    }
}

/// 刷新 marketplace（先 fetch 校验来料清单，再 ff / 重 clone 提交；逐 marketplace 对账分类）/ refresh。
///
/// #272：提交前用 `git show FETCH_HEAD:<manifest>` 校验清单名（shallow clone 会先 `--unshallow` 以支持
/// ff）；改名/缺失/非法 → [`RefreshStatus::Rejected`] + 诊断，旧注册/内容/能力保留。网络/I/O 失败保持
/// 既有 `unchanged` 语义。
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
        // #275：已装 plugin 的 marketplace 先 stage（installed intent 顺序），其余后置；避免仅添加的
        // 源凭 known_marketplaces 载入序抢占同名 SKILL 所有权。
        let priority = installed_marketplace_order(Some(home), env);
        let mut names: Vec<String> = priority
            .into_iter()
            .filter(|m| mps.marketplaces.contains_key(m))
            .collect();
        for name in mps.marketplaces.keys() {
            if !names.contains(name) {
                names.push(name.clone());
            }
        }
        names
    } else {
        vec![target.to_string()]
    };

    let mut rows: Vec<MarketplaceRefreshRow> = Vec::new();
    for nm in &names {
        let Some(rec) = mps.marketplaces.get(nm) else {
            rows.push(MarketplaceRefreshRow {
                name: untrusted_name_for_display(nm),
                status: RefreshStatus::Missing,
                skills: 0,
                diagnostic: None,
            });
            continue;
        };
        let baseline = rec.manifest_name_baseline.clone();
        let source = rec.source.clone();
        let auto = rec
            .extra
            .get("autoUpdate")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let store = make_store(home, env);
        let outcome = refresh_marketplace_clone(
            nm,
            &source,
            registry,
            home,
            baseline.as_deref(),
            auto,
            DEFAULT_GIT_TIMEOUT,
            env,
            Some(store.as_recorder()),
        )
        .await;
        let (status, diagnostic) =
            match &outcome.failure {
                None => {
                    let status = if outcome.changed {
                        RefreshStatus::Updated
                    } else {
                        RefreshStatus::Unchanged
                    };
                    let diagnostic = outcome.advisory.as_ref().map(|(kind, message)| {
                        MarketplaceRefreshDiagnostic {
                            code: match kind {
                                crate::skills::staging::ManifestAdvisory::Mismatch => {
                                    RefreshDiagnosticCode::LegacyManifestNameMismatch
                                }
                                crate::skills::staging::ManifestAdvisory::Missing => {
                                    RefreshDiagnosticCode::LegacyManifestNameMissing
                                }
                                crate::skills::staging::ManifestAdvisory::Invalid => {
                                    RefreshDiagnosticCode::LegacyManifestNameInvalid
                                }
                            },
                            message: redact_git_urls_in_text(message),
                        }
                    });
                    (status, diagnostic)
                }
                Some(failure @ MarketplaceStageError::ManifestNameChanged { .. }) => (
                    RefreshStatus::Rejected,
                    Some(MarketplaceRefreshDiagnostic {
                        code: RefreshDiagnosticCode::ManifestNameChanged,
                        message: failure.to_string(),
                    }),
                ),
                Some(failure @ MarketplaceStageError::ManifestNameMissing) => (
                    RefreshStatus::Rejected,
                    Some(MarketplaceRefreshDiagnostic {
                        code: RefreshDiagnosticCode::ManifestNameMissing,
                        message: failure.to_string(),
                    }),
                ),
                Some(failure @ MarketplaceStageError::ManifestNameInvalid) => (
                    RefreshStatus::Rejected,
                    Some(MarketplaceRefreshDiagnostic {
                        code: RefreshDiagnosticCode::ManifestNameInvalid,
                        message: failure.to_string(),
                    }),
                ),
                // 清单存在但损坏：旧内容保留（状态维持 unchanged），但必须给出可见诊断。
                Some(failure @ MarketplaceStageError::ManifestUnreadable) => (
                    RefreshStatus::Unchanged,
                    Some(MarketplaceRefreshDiagnostic {
                        code: RefreshDiagnosticCode::ManifestUnreadable,
                        message: failure.to_string(),
                    }),
                ),
                Some(failure) => {
                    // 网络 / I/O 失败保持既有 `unchanged` 语义（逐行汇报、不整体报错）。
                    tracing::error!(
                        marketplace = %untrusted_name_for_display(nm),
                        error = %failure,
                        "marketplace refresh failed"
                    );
                    (RefreshStatus::Unchanged, None)
                }
            };
        rows.push(MarketplaceRefreshRow {
            name: untrusted_name_for_display(nm),
            status,
            skills: outcome.registered.len(),
            diagnostic,
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
    non_plugin_bundle_ids: &std::collections::HashSet<crate::mcp_clients::model::BundleId>,
) -> Result<MarketplaceRemoveOutcome, GovernanceError> {
    if !marketplace_name_taken(home, env, name) {
        return Err(GovernanceError::UnknownMarketplace(
            untrusted_name_for_display(name),
        ));
    }

    let valid_name = is_valid_marketplace_name(name);
    let mut uninstalled: Vec<String> = Vec::new();
    if !params.keep_plugins && valid_name {
        let suffix = format!("@{name}");
        let installed = load_installed_plugins(Some(home), env).account;
        let victims: Vec<String> = installed
            .plugins
            .keys()
            .filter(|pid| pid.ends_with(&suffix))
            .cloned()
            .collect();
        // #139 回收判据数据源由调用方（`Computer::remove_marketplace`）经
        // `non_plugin_declared_bundle_ids` 供给**全集**（durable + flag + embed + 实例 config_dir/config_env）——
        // 本自由函数无 Computer 实例，MUST NOT 自行以残缺声明面重算（漏 embed/flag/config_dir ⇒ 连坐用户/
        // 宿主自有 server，#139「永不连坐」）。
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
                non_plugin_bundle_ids,
                params.hooks,
            )
            .await;
            if matches!(res, Ok(true)) {
                uninstalled.push(pid.clone());
            }
        }
    }

    let store = make_store(home, env);
    let pruned = if valid_name {
        prune_marketplaces(&[name.to_string()], registry, home, &store)
    } else {
        // 手编账本可能含非法 key。它不能安全映射为文件系统路径，也不能原样传入 reconciler tracing；
        // 仅按精确原始 key 删除账本记录，公开 outcome 使用脱敏展示值，plugin 记录留待显式 GC。
        let drop_name = name.to_string();
        store.update_known_marketplaces(&mut |data| {
            data.marketplaces.shift_remove(&drop_name);
        });
        vec![untrusted_name_for_display(name)]
    };
    Ok(MarketplaceRemoveOutcome {
        name: untrusted_name_for_display(name),
        pruned,
        uninstalled_plugins: uninstalled,
        kept_plugins: params.keep_plugins || !valid_name,
    })
}

#[cfg(test)]
mod tests {
    // 注意：本模块是**非 CLI** 层，测试**不得**依赖 cli-gated 的 `crate::cli::commands::test_env`，否则
    // `cargo test-ws`（默认特性、无 cli）将无法编译——而这恰会反噬本特性"GUI 无需 cli feature"的目标。
    // marketplace 账本（known_marketplaces.json）落在 `home`（skill_home）内，故 `env = None` 即完全 hermetic。
    use super::*;
    use crate::settings::store::update_known_marketplaces;
    use std::sync::{Arc, Mutex};
    use tempfile::tempdir;
    use tracing::instrument::WithSubscriber;

    #[derive(Clone, Default)]
    struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

    impl CapturedLogs {
        fn contents(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    impl std::io::Write for CapturedLogs {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLogs {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

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
        // #285：hostless 不得归一为合法 URL，也不得回落成 owner/repo 简写。
        assert_eq!(
            normalize_marketplace_url("ssh:///alice:PW_LEAK@example.com"),
            None
        );
        assert_eq!(
            normalize_marketplace_url("ssh:/alice:PW_LEAK@example.com"),
            None
        );
        assert_eq!(normalize_marketplace_url("ssh:/foo"), None);
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
        assert_eq!(
            default_marketplace_name(
                "https://user:PW_SECRET@example.com/acme/repo.git?token=QUERY#FRAGMENT"
            ),
            Some("repo".to_string())
        );
        assert_eq!(
            default_marketplace_name("https://user:PW_SECRET@example.com?token=QUERY#FRAGMENT"),
            None
        );
        // #285：hostless 公开函数直调也不得从 path 派生（防凭据 slug）。
        assert_eq!(
            default_marketplace_name("ssh:///alice:PW_LEAK@example.com"),
            None
        );
        assert_eq!(
            default_marketplace_name("ssh:/alice:PW_LEAK@example.com"),
            None
        );
        assert_eq!(
            default_marketplace_name("git:///alice:PW_LEAK@example.com"),
            None
        );
        assert_eq!(
            default_marketplace_name("ssh://example.com/org/repo.git"),
            Some("repo".to_string())
        );
        // #285 复审：合法 URL 但派生段含 userinfo 形态 → None（防 file:/scp-like 凭据 slug）。
        assert_eq!(
            default_marketplace_name("file:///alice:PW_LEAK@example.com"),
            None
        );
        assert_eq!(
            default_marketplace_name("git@host:alice:PW_LEAK@example.com"),
            None
        );
        // scp 分支 rsplit(':') 只看最后一段：无 `@` 的 user:password 尾部与 percent-encoded 变体。
        assert_eq!(default_marketplace_name("git@host:alice:PW_LEAK"), None);
        assert_eq!(
            default_marketplace_name("git@host:alice%3APW_LEAK%40example.com"),
            None
        );
        assert_eq!(default_marketplace_name("file:///alice:PW_LEAK"), None);
        assert_eq!(
            default_marketplace_name("file:///alice%3APW_LEAK%40example.com"),
            None
        );
        assert_eq!(
            default_marketplace_name("file:///abs/path/repo"),
            Some("repo".to_string())
        );
        // 收窄集合 pin（公开 API 行为变更）：旧实现可派生、新守卫拒绝。
        assert_eq!(default_marketplace_name("host:repo"), None);
        assert_eq!(default_marketplace_name("SSH://host/repo"), None);
        assert_eq!(default_marketplace_name("file:/abs/repo"), None);
    }

    #[test]
    fn resolve_add_request_normalizes_url_and_expected_name() {
        // 非法 URL → InvalidUrl。
        assert!(matches!(
            resolve_marketplace_add_request("not a url", None),
            Err(GovernanceError::InvalidUrl(_))
        ));
        // 合法 URL 归一；无期望名（注册名由清单决定）→ None。
        let request = resolve_marketplace_add_request("acme/skills", None).unwrap();
        assert_eq!(request.url, "https://github.com/acme/skills.git");
        assert!(request.expected_name.is_none());
        // 显式名 = 期望清单名（仅校验，不覆盖）。
        let request = resolve_marketplace_add_request("acme/skills", Some("custom")).unwrap();
        assert_eq!(request.expected_name.as_deref(), Some("custom"));
        assert_eq!(request.url, "https://github.com/acme/skills.git");
    }

    #[tokio::test]
    async fn invalid_explicit_name_never_exposes_git_credentials_or_touches_disk() {
        const SECRET_URL: &str =
            "https://cnb:FAKE_TOKEN@example.com/org/repo.git?token=QUERY#token=FRAGMENT";
        let dir = tempdir().unwrap();
        let mut registry = SkillRegistry::new();
        let err = add_marketplace(
            &mut registry,
            dir.path(),
            None,
            SECRET_URL,
            AddMarketplaceParams {
                name: Some("Turingfocus"),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();

        assert!(matches!(
            &err,
            GovernanceError::InvalidName(name) if name == "Turingfocus"
        ));
        let public_forms = format!("{err}\n{err:?}");
        for secret in ["cnb", "FAKE_TOKEN", "QUERY", "FRAGMENT", SECRET_URL] {
            assert!(
                !public_forms.contains(secret),
                "public error form leaked {secret:?}: {public_forms}"
            );
        }
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            0,
            "identity validation must fail before any filesystem write"
        );
    }

    #[test]
    fn credentialed_explicit_name_and_pathless_default_are_safe_errors() {
        let explicit_errors = [
            "key=https://user:PW_SECRET@example.com/name",
            "x=https://public.example/a=https://user2:PW_TWO@secret.example/repo.git",
            "key=用户@example.com:org/repo.git",
            "x=https://example.com/r.git?token=;QUERY_SECRET",
            "x=https://example.com/r.git?token=QUERY_QUOTE'LEAK_SECRET",
            "x=https://alice:PW_ONE'PW_TWO@example.com/repo.git",
            "x=alice@my_host:org/repo.git",
            "x=用户@例子.公司:org/repo.git",
        ]
        .map(|name| resolve_marketplace_add_request("acme/skills", Some(name)).unwrap_err());

        for error in explicit_errors {
            let rendered = format!("{error}\n{error:?}");
            for secret in [
                "user",
                "PW_SECRET",
                "QUERY",
                "FRAGMENT",
                "user2",
                "PW_TWO",
                "用户",
                "QUERY_SECRET",
                "QUERY_QUOTE",
                "LEAK_SECRET",
                "PW_ONE",
                "alice",
            ] {
                assert!(!rendered.contains(secret), "{rendered}");
            }
        }
    }

    #[test]
    fn invalid_url_payload_is_safe_for_display_and_debug() {
        for raw in [
            "cnb:FAKE_TOKEN@example.com/org/repo.git",
            "ftp://cnb:FAKE_TOKEN",
            "ssh:alice:PW_LEAK@example.com",
            "git:alice:PW_LEAK@host/repo.git",
            "ssh:alice%3APW_LEAK%40example.com",
            "ssh:/alice:PW_LEAK@host",
            "ssh:///alice:PW_LEAK@host",
            "git:///alice:PW_LEAK@host",
            "file:alice:PW_LEAK@host",
        ] {
            let err = resolve_marketplace_add_request(raw, Some("valid-name")).unwrap_err();
            assert!(matches!(
                &err,
                GovernanceError::InvalidUrl(source) if source == "<redacted-git-source>"
            ));
            let public_forms = format!("{err}\n{err:?}");
            for secret in ["cnb", "FAKE_TOKEN", "alice", "PW_LEAK"] {
                assert!(!public_forms.contains(secret), "{public_forms}");
            }
        }
    }

    #[tokio::test]
    async fn add_no_clone_requires_existing_catalog_and_matching_manifest() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let mut registry = SkillRegistry::new();
        let repo = dir.path().join("repo");
        let source = build_repo(&repo, "skills");
        let url = source["url"].as_str().unwrap().to_string();

        // ① 无 catalog + 显式期望名 → validation（D5：不凭空占名）。
        let err = add_marketplace(
            &mut registry,
            home,
            Some(&test_env(home)),
            &url,
            AddMarketplaceParams {
                name: Some("skills"),
                no_clone: true,
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
        assert!(
            matches!(
                &err,
                GovernanceError::Validation(MarketplaceStageError::ManifestUnreadable)
            ),
            "{err:?}"
        );
        assert!(!marketplace_name_taken(home, None, "skills"));

        // ② 无期望名 → validation（无法在注册前确定清单名）。
        let err = add_marketplace(
            &mut registry,
            home,
            Some(&test_env(home)),
            &url,
            AddMarketplaceParams {
                no_clone: true,
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
        assert!(
            matches!(
                &err,
                GovernanceError::Validation(MarketplaceStageError::ManifestNameMissing)
            ),
            "{err:?}"
        );

        // ③ 已有 catalog（账本丢失后重建）：先正常 add 再删账本 → no_clone 只回填账本。
        add_marketplace(
            &mut registry,
            home,
            Some(&test_env(home)),
            &url,
            AddMarketplaceParams::default(),
        )
        .await
        .unwrap();
        let ledger = crate::settings::store::known_marketplaces_path(Some(home), None);
        std::fs::remove_file(&ledger).unwrap();
        let outcome = add_marketplace(
            &mut registry,
            home,
            Some(&test_env(home)),
            &url,
            AddMarketplaceParams {
                name: Some("skills"),
                no_clone: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(outcome.name, "skills");
        assert!(outcome.no_clone && outcome.changed);
        assert!(outcome.skills.is_empty());
        assert!(marketplace_name_taken(home, None, "skills"));
        let rec = load_known_marketplaces(Some(home), None)
            .account
            .marketplaces
            .get("skills")
            .cloned()
            .unwrap();
        assert_eq!(rec.manifest_name_baseline.as_deref(), Some("skills"));
    }

    #[tokio::test]
    async fn add_same_name_different_source_is_rejected_and_same_source_is_idempotent() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let mut registry = SkillRegistry::new();
        let repo_a = dir.path().join("repo-a");
        let repo_b = dir.path().join("repo-b");
        // 仓库名不同、清单名相同（都以清单 name 为唯一注册键）。
        let src_a = build_repo(&repo_a, "skills");
        let src_b = build_repo(&repo_b, "skills");
        let url_a = src_a["url"].as_str().unwrap().to_string();
        let url_b = src_b["url"].as_str().unwrap().to_string();

        let first = add_marketplace(
            &mut registry,
            home,
            None,
            &url_a,
            AddMarketplaceParams::default(),
        )
        .await
        .unwrap();
        assert!(first.changed);
        assert_eq!(first.name, "skills");

        // 同源重复 add → 幂等（无写入、内容不变）。
        let again = add_marketplace(
            &mut registry,
            home,
            None,
            &url_a,
            AddMarketplaceParams::default(),
        )
        .await
        .unwrap();
        assert!(!again.changed);
        assert!(again.skills.is_empty());

        // 同名不同源 → DuplicateMarketplace，既有注册与内容保持不变。
        let err = add_marketplace(
            &mut registry,
            home,
            None,
            &url_b,
            AddMarketplaceParams::default(),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&err, GovernanceError::DuplicateMarketplace(n) if n == "skills"),
            "{err:?}"
        );
        let rec = load_known_marketplaces(Some(home), None)
            .account
            .marketplaces
            .get("skills")
            .cloned()
            .unwrap();
        assert_eq!(rec.source["url"].as_str(), Some(url_a.as_str()));
        assert_eq!(rec.manifest_name_baseline.as_deref(), Some("skills"));
        assert!(registry.contains("audit:code-review"));
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
                RemoveMarketplaceParams::default(),
                &std::collections::HashSet::new(),
            )
            .await,
            Err(GovernanceError::UnknownMarketplace(n)) if n == "ghost"
        ));
    }

    #[tokio::test]
    async fn unknown_marketplace_and_refresh_target_are_safe_public_values() {
        let dir = tempdir().unwrap();
        let mut registry = SkillRegistry::new();
        let target = "x=https://user:PW_SECRET@example.com/repo.git";

        let error = remove_marketplace(
            &mut registry,
            dir.path(),
            None,
            target,
            RemoveMarketplaceParams::default(),
            &std::collections::HashSet::new(),
        )
        .await
        .unwrap_err();
        let rows = refresh_marketplaces(&mut registry, dir.path(), None, target).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, RefreshStatus::Missing);

        for rendered in [
            format!("{error}\n{error:?}"),
            format!("{:?}\n{}", rows[0], rows[0].name),
        ] {
            for secret in ["user", "PW_SECRET"] {
                assert!(!rendered.contains(secret), "{rendered}");
            }
        }
    }

    #[tokio::test]
    async fn remove_hand_edited_invalid_key_deletes_exact_record_without_log_leak() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let raw_name = "x=https://alice:PW_REMOVE@example.com/repo.git";
        update_known_marketplaces(
            |data| {
                data.account.marketplaces.insert(
                    raw_name.to_string(),
                    KnownMarketplaceEntry {
                        manifest_name_baseline: None,
                        source: json!({"type": "git", "url": "https://example.com/repo.git"}),
                        extra: Map::new(),
                    },
                );
            },
            Some(home),
            None,
        )
        .unwrap();

        let logs = CapturedLogs::default();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_writer(logs.clone())
            .finish();
        let mut registry = SkillRegistry::new();
        let outcome = remove_marketplace(
            &mut registry,
            home,
            None,
            raw_name,
            RemoveMarketplaceParams::default(),
            &std::collections::HashSet::new(),
        )
        .with_subscriber(subscriber)
        .await
        .unwrap();

        assert!(!marketplace_name_taken(home, None, raw_name));
        assert!(outcome.kept_plugins, "非法 key 不得映射到级联删除路径");
        let rendered = format!("{outcome:?}\n{}", logs.contents());
        for secret in ["alice", "PW_REMOVE"] {
            assert!(!rendered.contains(secret), "{rendered}");
        }
    }

    #[tokio::test]
    async fn remove_after_ledger_seed_prunes_entry() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let mut registry = SkillRegistry::new();
        update_known_marketplaces(
            |data| {
                data.account.marketplaces.insert(
                    "skills".to_string(),
                    KnownMarketplaceEntry {
                        manifest_name_baseline: None,
                        source: json!({"type": "git", "url": "https://example.com/skills.git"}),
                        extra: Map::new(),
                    },
                );
            },
            Some(home),
            None,
        )
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
            &std::collections::HashSet::new(),
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

    // ── 真实 git fixture + 记录式 hooks（级联卸载 / refresh 分类测试用）─────────────
    fn git(args: &[&str], cwd: &Path) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", cwd)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("git available");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// 构造 marketplace 源仓库（audit plugin：1 skill + 清单顶层 `marketplace` 名）→ git source value。
    fn build_repo(repo: &Path, marketplace: &str) -> Value {
        std::fs::create_dir_all(repo.join(".tfrobot-plugin")).unwrap();
        std::fs::write(
            repo.join(".tfrobot-plugin/marketplace.json"),
            format!(
                r#"{{"name": "{marketplace}", "plugins": [{{"name": "audit", "source": "./plugins/audit"}}]}}"#
            ),
        )
        .unwrap();
        let skill = repo.join("plugins/audit/skills/code-review");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: code-review\ndescription: review code\n---\nbody",
        )
        .unwrap();
        git(&["init", "-q"], repo);
        git(&["add", "-A"], repo);
        git(&["commit", "-qm", "init"], repo);
        json!({"type": "git", "url": format!("file://{}", repo.display())})
    }

    /// 记录式 `McpInstallHooks` 替身：记录 `remove_server` 调用（级联卸载断言用）/ recording hooks。
    struct RecordingHooks {
        removed: std::sync::Mutex<Vec<String>>,
    }
    #[async_trait::async_trait]
    impl McpInstallHooks for RecordingHooks {
        fn existing_servers(
            &self,
        ) -> std::collections::HashMap<
            crate::mcp_clients::model::BundleId,
            crate::mcp_clients::model::ServerName,
        > {
            std::collections::HashMap::new()
        }
        async fn register_server(
            &self,
            _cfg: crate::mcp_clients::model::MCPServerConfig,
        ) -> Result<(), crate::settings::installer::McpHookError> {
            Ok(())
        }
        async fn remove_server(
            &self,
            id: &crate::mcp_clients::model::BundleId,
        ) -> Result<(), crate::settings::installer::McpHookError> {
            self.removed.lock().unwrap().push(id.as_str().to_string());
            Ok(())
        }
    }

    /// 直接向 `installed_plugins.json` 写一条记录（绕过 install，专测 remove 级联）/ seed an install record。
    fn seed_installed(home: &Path, pid: &str, bundled: &[&str]) {
        let pid = pid.to_string();
        let bundled: Vec<crate::mcp_clients::model::BundleId> = bundled
            .iter()
            .map(|s| crate::mcp_clients::model::BundleId::try_from(s.to_string()).unwrap())
            .collect();
        crate::settings::store::update_installed_plugins(
            move |file| {
                file.account.plugins.insert(
                    pid,
                    vec![crate::settings::reconciler::InstalledPluginRecord {
                        install_path: None,
                        mcp_servers: bundled,
                        extra: Map::new(),
                    }],
                );
            },
            Some(home),
            None,
        )
        .unwrap();
    }

    fn is_installed(home: &Path, pid: &str) -> bool {
        load_installed_plugins(Some(home), None)
            .account
            .plugins
            .contains_key(pid)
    }

    // ---- 🔴1：remove_marketplace 级联卸载（keep_plugins=false 默认主路径）+ 跨 mp 隔离 ----
    #[tokio::test]
    async fn remove_marketplace_cascade_uninstalls_and_isolates_other_marketplace() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let mut registry = SkillRegistry::new();
        // 两个 marketplace：skills（待移除，下挂 audit@skills 带 bundled server）+ my-skills（须保留 x@my-skills）。
        // 直接播种账本（remove 不要求 clone 存在）。
        update_known_marketplaces(
            |data| {
                for name in ["skills", "my-skills"] {
                    data.account.marketplaces.insert(
                        name.to_string(),
                        KnownMarketplaceEntry {
                            manifest_name_baseline: None,
                            source: json!({
                                "type": "git",
                                "url": format!("https://example.com/{name}.git")
                            }),
                            extra: Map::new(),
                        },
                    );
                }
            },
            Some(home),
            None,
        )
        .unwrap();
        seed_installed(home, "audit@skills", &["audit-mcp"]);
        seed_installed(home, "x@my-skills", &["x-mcp"]); // 后缀 @my-skills，不应被 @skills 命中

        let hooks = RecordingHooks {
            removed: std::sync::Mutex::new(Vec::new()),
        };
        let outcome = remove_marketplace(
            &mut registry,
            home,
            None,
            "skills",
            RemoveMarketplaceParams {
                keep_plugins: false,
                hooks: Some(&hooks),
            },
            &std::collections::HashSet::new(),
        )
        .await
        .unwrap();

        // ① 级联卸载命中 audit@skills；② bundled server 经 hook 摘除；③ prune 命中 skills。
        assert_eq!(
            outcome.uninstalled_plugins,
            vec!["audit@skills".to_string()]
        );
        assert!(!outcome.kept_plugins);
        assert_eq!(
            *hooks.removed.lock().unwrap(),
            vec!["audit-mcp".to_string()]
        );
        assert_eq!(outcome.pruned, vec!["skills".to_string()]);
        // 账本：audit@skills 已删、known_marketplaces["skills"] 已 prune。
        assert!(!is_installed(home, "audit@skills"));
        assert!(!marketplace_name_taken(home, None, "skills"));
        // 负向：x@my-skills（@ 锚点防跨 mp 误删）仍在；my-skills 仍在。
        assert!(
            is_installed(home, "x@my-skills"),
            "@ 锚点应防跨 marketplace 误删"
        );
        assert!(marketplace_name_taken(home, None, "my-skills"));
    }

    // ---- 🔴2：refresh_marketplaces 的 Unchanged / Updated 分类（target=="all" + commitSha 对比）----
    #[tokio::test]
    async fn refresh_marketplaces_classifies_unchanged_then_updated() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let repo = dir.path().join("repo");
        let source = build_repo(&repo, "acme");
        let url = source
            .get("url")
            .and_then(Value::as_str)
            .unwrap()
            .to_string();
        let mut registry = SkillRegistry::new();

        // 真实 clone 落账。
        add_marketplace(
            &mut registry,
            home,
            Some(&test_env(home)),
            &url,
            AddMarketplaceParams {
                name: Some("acme"),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        // refresh("all")：源未变 → commitSha 不变 → Unchanged。
        let rows = refresh_marketplaces(&mut registry, home, None, "all").await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "acme");
        assert_eq!(
            rows[0].status,
            RefreshStatus::Unchanged,
            "源未变应 Unchanged"
        );

        // 源仓库再提交一笔 → commitSha 变 → refresh 应 Updated。
        std::fs::write(
            repo.join("plugins/audit/skills/code-review/SKILL.md"),
            "---\nname: code-review\ndescription: v2\n---\nbody2",
        )
        .unwrap();
        git(&["add", "-A"], &repo);
        git(&["commit", "-qm", "v2"], &repo);
        let rows2 = refresh_marketplaces(&mut registry, home, None, "all").await;
        assert_eq!(
            rows2[0].status,
            RefreshStatus::Updated,
            "源更新后应 Updated"
        );
    }

    // ---- 🟡3a：尾段仅 ".git" → URL 派生工具返回 None（legacy helper；add 不再派生名）----
    #[test]
    fn default_name_dotgit_only_tail_is_none() {
        assert_eq!(default_marketplace_name("https://example.com/.git"), None);
    }

    // ---- 🟡3b：clone 不可达（stage 降级未落账）→ CloneFailed ----
    #[tokio::test]
    async fn add_unreachable_source_is_clone_failed() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let mut registry = SkillRegistry::new();
        let bad = format!("file://{}/nonexistent-repo.git", dir.path().display());
        let err = add_marketplace(
            &mut registry,
            home,
            None,
            &bad,
            AddMarketplaceParams {
                name: Some("acme"),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
        // #272：clone 在读完清单前发生，注册名尚未知（空串）；仍须保持网络错分类。
        assert!(matches!(
            &err,
            GovernanceError::CloneFailed(name) if name.is_empty()
        ));
        // 降级铁律：未落 known_marketplaces。
        assert!(!marketplace_name_taken(home, None, "acme"));
    }

    // ---- #275：跨 marketplace 同名 plugin（add-after-enable / refresh-all）----------------

    fn build_plugin_repo(repo: &Path, plugin: &str, description: &str) -> Value {
        std::fs::create_dir_all(repo.join(".tfrobot-plugin")).unwrap();
        // 清单顶层 name 取仓库目录名（调用方按 `<repo-stem>` 注册 marketplace）。
        let marketplace = repo
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("test-marketplace");
        std::fs::write(
            repo.join(".tfrobot-plugin/marketplace.json"),
            format!(
                r#"{{"name": "{marketplace}", "plugins": [{{"name": "{plugin}", "source": "./plugins/{plugin}"}}]}}"#
            ),
        )
        .unwrap();
        let skill = repo.join(format!("plugins/{plugin}/skills/code-review"));
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            format!("---\nname: code-review\ndescription: {description}\n---\nbody"),
        )
        .unwrap();
        git(&["init", "-q"], repo);
        git(&["add", "-A"], repo);
        git(&["commit", "-qm", "init"], repo);
        json!({"type": "git", "url": format!("file://{}", repo.display())})
    }

    fn test_env(dir: &Path) -> EnvMap {
        let cfg = dir.join("cfg");
        std::fs::create_dir_all(&cfg).unwrap();
        let mut env = EnvMap::new();
        env.insert(
            "XDG_CONFIG_HOME".to_string(),
            cfg.to_string_lossy().into_owned(),
        );
        env
    }

    async fn add_mp(
        registry: &mut SkillRegistry,
        home: &Path,
        env: &EnvMap,
        name: &str,
        url: &str,
    ) {
        add_marketplace(
            registry,
            home,
            Some(env),
            url,
            AddMarketplaceParams {
                name: Some(name),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }

    /// 序 2（add-after-enable）：先 enable `alpha@mp-a`，再 add 含同名 plugin 的 mp-b →
    /// add 成功但不得改写先到 SKILL 的 source；冲突入 registry 记录，intent 不新增 mp-b。
    #[tokio::test]
    async fn add_after_enable_preserves_first_plugin_source() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let env = test_env(home);
        let repo_a = dir.path().join("mp-a");
        let repo_b = dir.path().join("mp-b");
        let src_a = build_plugin_repo(&repo_a, "alpha", "from mp-a");
        let src_b = build_plugin_repo(&repo_b, "alpha", "from mp-b");
        let url_a = src_a
            .get("url")
            .and_then(Value::as_str)
            .unwrap()
            .to_string();
        let url_b = src_b
            .get("url")
            .and_then(Value::as_str)
            .unwrap()
            .to_string();

        let mut reg = SkillRegistry::new();
        add_mp(&mut reg, home, &env, "mp-a", &url_a).await;
        crate::settings::installer::install_plugin(
            "alpha@mp-a",
            &mut reg,
            home,
            crate::settings::installer::InstallOptions {
                env: Some(&env),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
        crate::settings::installer::enable_plugin(
            "alpha@mp-a",
            &mut reg,
            home,
            crate::settings::installer::EnableOptions {
                env: Some(&env),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            reg.resolve("alpha:code-review").unwrap().source,
            "marketplace:mp-a"
        );

        add_mp(&mut reg, home, &env, "mp-b", &url_b).await;
        assert_eq!(
            reg.resolve("alpha:code-review").unwrap().source,
            "marketplace:mp-a",
            "add 不得静默改写先到 plugin 的 SKILL source"
        );
        assert!(reg.source_conflicts().iter().any(|c| {
            c.name == "alpha:code-review"
                && c.kept_source == "marketplace:mp-a"
                && c.rejected_source == "marketplace:mp-b"
        }));
        let intent = crate::settings::store::load_installed_plugins_intent(Some(home), Some(&env))
            .account
            .installed_plugins;
        assert!(intent.contains("alpha@mp-a"));
        assert!(!intent.contains("alpha@mp-b"));
    }

    /// 序 3（refresh-all）：两源同名；同源内容 refresh 仍生效（取 mp-a 新 description），
    /// 跨源仍被拒（source 不变），intent/enabled 不变。
    #[tokio::test]
    async fn refresh_all_preserves_first_source_and_updates_same_source() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let env = test_env(home);
        let repo_a = dir.path().join("mp-a");
        let repo_b = dir.path().join("mp-b");
        let src_a = build_plugin_repo(&repo_a, "alpha", "from mp-a");
        let src_b = build_plugin_repo(&repo_b, "alpha", "from mp-b");
        let url_a = src_a
            .get("url")
            .and_then(Value::as_str)
            .unwrap()
            .to_string();
        let url_b = src_b
            .get("url")
            .and_then(Value::as_str)
            .unwrap()
            .to_string();

        let mut reg = SkillRegistry::new();
        add_mp(&mut reg, home, &env, "mp-a", &url_a).await;
        crate::settings::installer::install_plugin(
            "alpha@mp-a",
            &mut reg,
            home,
            crate::settings::installer::InstallOptions {
                env: Some(&env),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
        crate::settings::installer::enable_plugin(
            "alpha@mp-a",
            &mut reg,
            home,
            crate::settings::installer::EnableOptions {
                env: Some(&env),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
        add_mp(&mut reg, home, &env, "mp-b", &url_b).await;

        // 同源内容 refresh：mp-a 更新 description 并 commit。
        std::fs::write(
            repo_a.join("plugins/alpha/skills/code-review/SKILL.md"),
            "---\nname: code-review\ndescription: v2\n---\nbody2",
        )
        .unwrap();
        git(&["add", "-A"], &repo_a);
        git(&["commit", "-qm", "v2"], &repo_a);

        refresh_marketplaces(&mut reg, home, Some(&env), "all").await;
        let kept = reg.resolve("alpha:code-review").unwrap();
        assert_eq!(
            kept.source, "marketplace:mp-a",
            "跨源 refresh 不得改写 source"
        );
        assert_eq!(kept.description, "v2", "同源内容 refresh 仍须生效");
        let intent = crate::settings::store::load_installed_plugins_intent(Some(home), Some(&env))
            .account
            .installed_plugins;
        assert!(intent.contains("alpha@mp-a"));
        assert!(!intent.contains("alpha@mp-b"));
    }
}
