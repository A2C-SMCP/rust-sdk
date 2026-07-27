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
    git_source_for_error, redact_git_urls_in_text, untrusted_name_for_display,
};
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
/// client 直接消费结构化变体。所有字符串载荷均可安全进入公开 `Debug` / `Display`：URL 型输入先脱敏，
/// 名称错误只携名称，clone 错误只携 marketplace 名。
#[derive(Debug, thiserror::Error)]
pub enum GovernanceError {
    /// 既非合法 git URL 也非 `owner/repo` 简写 / not a well-formed git URL or `owner/repo` shorthand。
    #[error("not a well-formed git url or owner/repo shorthand: {0:?}")]
    InvalidUrl(String),
    /// 非法 marketplace 名；空串表示未提供显式名且 URL 无法派生合法名 / invalid marketplace name。
    #[error("invalid marketplace name: {0:?}")]
    InvalidName(String),
    /// marketplace 名已存在（add 拒绝覆盖）/ marketplace name already exists。
    #[error("marketplace name conflict: {0:?} already exists")]
    DuplicateMarketplace(String),
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
    let raw = url.trim();
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
/// URL 非法 → [`GovernanceError::InvalidUrl`]（安全展示值）；无法派生/非法名 →
/// [`GovernanceError::InvalidName`]（显式名安全展示值；无显式名时为空串）。
pub fn resolve_marketplace_identity(
    git_url: &str,
    explicit_name: Option<&str>,
) -> Result<MarketplaceIdentity, GovernanceError> {
    let url = normalize_marketplace_url(git_url)
        .ok_or_else(|| GovernanceError::InvalidUrl(git_source_for_error(git_url)))?;
    let name = match explicit_name {
        Some(name) if is_valid_marketplace_name(name) => name.to_string(),
        Some(name) => return Err(GovernanceError::InvalidName(redact_git_urls_in_text(name))),
        None => default_marketplace_name(&url)
            .ok_or_else(|| GovernanceError::InvalidName(String::new()))?,
    };
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
        return Err(GovernanceError::CloneFailed(name.clone()));
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
                name: untrusted_name_for_display(nm),
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
            name: untrusted_name_for_display(nm),
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
        .map(|name| resolve_marketplace_identity("acme/skills", Some(name)).unwrap_err());

        let derived = resolve_marketplace_identity(
            "https://user:PW_SECRET@example.com?token=QUERY#FRAGMENT",
            None,
        )
        .unwrap_err();
        assert!(matches!(&derived, GovernanceError::InvalidName(name) if name.is_empty()));

        for error in explicit_errors.into_iter().chain(std::iter::once(derived)) {
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
        ] {
            let err = resolve_marketplace_identity(raw, Some("valid-name")).unwrap_err();
            assert!(matches!(
                &err,
                GovernanceError::InvalidUrl(source) if source == "<redacted-git-source>"
            ));
            let public_forms = format!("{err}\n{err:?}");
            assert!(!public_forms.contains("cnb"));
            assert!(!public_forms.contains("FAKE_TOKEN"));
        }
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

    /// 构造 marketplace 源仓库（audit plugin：1 skill）→ git source value。
    fn build_repo(repo: &Path) -> Value {
        std::fs::create_dir_all(repo.join(".tfrobot-plugin")).unwrap();
        std::fs::write(
            repo.join(".tfrobot-plugin/marketplace.json"),
            r#"{"plugins": [{"name": "audit", "source": "./plugins/audit"}]}"#,
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
        let mk = || AddMarketplaceParams {
            no_clone: true,
            ..Default::default()
        };
        // 两个 marketplace：skills（待移除，下挂 audit@skills 带 bundled server）+ my-skills（须保留 x@my-skills）。
        add_marketplace(&mut registry, home, None, "acme/skills", mk())
            .await
            .unwrap();
        add_marketplace(&mut registry, home, None, "acme/my-skills", mk())
            .await
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
        let source = build_repo(&repo);
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
            None,
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

    // ---- 🟡3a：尾段仅 ".git" → 派生空名 → InvalidName（纯函数级触发）----
    #[test]
    fn resolve_identity_dotgit_only_tail_is_invalid_name() {
        assert!(matches!(
            resolve_marketplace_identity("https://example.com/.git", None),
            Err(GovernanceError::InvalidName(name)) if name.is_empty()
        ));
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
        assert!(matches!(
            err,
            GovernanceError::CloneFailed(name) if name == "acme"
        ));
        // 降级铁律：未落 known_marketplaces。
        assert!(!marketplace_name_taken(home, None, "acme"));
    }
}
