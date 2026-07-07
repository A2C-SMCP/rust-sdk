/*!
* 文件名: reconciler.rs
* 作者: JQQ
* 创建日期: 2026/06/03
* 最后修改日期: 2026/06/03
* 版权: 2023 JQQ. All rights reserved.
* 依赖: smcp::utils::path, skills::staging, skills::registry, settings::schema
* 描述: SKILL reconciler（启动对账 additive-only + 孤儿清理 marketplace prune / plugin gc）
*       SKILL reconciler (startup reconcile additive-only + orphan cleanup).
*/

//! Reconciler：启动对账（additive-only 只增不删）+ 孤儿清理（marketplace prune / plugin gc）。
//!
//! 对标 Python 治理层资产 `a2c_smcp/computer/settings/reconciler.py`（v0.2.1 #62）。
//! SDK 设计 / Design: python-sdk `docs/design-0.2.1-cli-marketplace-ux.md` §7.1（启动流程）/ §7.2（失败降级）/
//! §7.3（显式 sync 与孤儿清理）；父工单 #53「reconciler additive-only」决策。
//!
//! 核心契约 / Core contract（§7.1）：
//! - **先合并单一声明视图再对账**（不是逐 scope）：调用方经 [`resolve_settings`] 把所有 scope 合成单一视图
//!   （`declared`）后传入；只取 `enabledPlugins` / `extraKnownMarketplaces` 两键作对账输入。
//! - **additive-only 只增不删**：对账返回只有 `{installed, updated, up_to_date, failed, skipped}`、**无 removed**。
//!   「声明没、物化有」的条目**完全不动**（绝不自动清理）；孤儿留待 §7.3 显式 [`prune_marketplaces`] /
//!   [`gc_plugins`] 清。
//! - **失败降级铁律**（§7.2）：git clone/pull 失败 → 记 ERROR、不阻断其余、对 Agent 不可见（不入 Registry）；
//!   本模块**不**向调用方抛 git/IO 异常（沿用 [`stage_marketplace_skills`] 的吞错降级）。
//!
//! 存储接缝 / Store seam（#67 / #68 边界）：本模块经 [`SkillGovernanceStore`] 读/改
//! `known_marketplaces.json` 与 `installed_plugins.json`，**不**自持久化逻辑——原子写 + 恢复归 **#67 SET-04**，
//! 真实实现的注入（含同一对象复用为 staging 记录器）归 **#68 INT-01**。bundled MCP server 的起停经
//! [`gc_plugins`] 的 [`McpTeardown`] 回调注入，真正接线（MCP manager）由 computer.py 集成承担。
//!
//! 并发 / Concurrency：[`reconcile`] **串行** stage 各 marketplace（不并发），遵守 [`stage_marketplace_skills`]
//! 文档化的同步 `file_lock` 阻塞约束。
//!
//! [`resolve_settings`]: crate::settings::scope::resolve_settings

use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use indexmap::{IndexMap, IndexSet};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use smcp::utils::path::{is_within, normalize_lexical};

use crate::settings::schema::{is_valid_enabled_plugin_key, is_valid_marketplace_name};
use crate::settings::scope::EnvMap;
use crate::skills::home::{marketplace_skill_dir, SOURCE_MARKETPLACE};
use crate::skills::registry::SkillRegistry;
use crate::skills::staging::{
    stage_marketplace_skills, KnownMarketplaceRecorder, MarketplaceStageOptions,
    DEFAULT_GIT_TIMEOUT, EXTERNAL_PLUGINS_NS,
};

// ===========================================================================
// 存储模型 + 接缝 / Store model + seam
// ===========================================================================
/// `known_marketplaces.json` 单条 marketplace 记录 / one known-marketplace entry。
///
/// 仅显式建模 reconciler 需读回的 `source`（sourceChanged 判定）；其余字段（installLocation / commitSha /
/// autoUpdate / lastUpdated 等）经 `extra` 透传保真，归 store（#67）拥有。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct KnownMarketplaceEntry {
    /// marketplace git 源（`{type, url}`）；sourceChanged 判定依据 / the recorded git source。
    #[serde(default)]
    pub source: Value,
    /// 其余字段透传（store 拥有）/ passthrough fields owned by the store。
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `known_marketplaces.json` 全量 / the whole known-marketplaces file。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KnownMarketplaces {
    /// `name → entry`（保物化顺序）/ ordered marketplace records。
    #[serde(default)]
    pub marketplaces: IndexMap<String, KnownMarketplaceEntry>,
}

/// `installed_plugins.json` 单条安装记录 / one plugin install record。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct InstalledPluginRecord {
    /// plugin 物化落点（仅词法位于 SKILL Home 内才删）/ install path。
    #[serde(
        default,
        rename = "installPath",
        skip_serializing_if = "Option::is_none"
    )]
    pub install_path: Option<String>,
    /// 随附 bundled MCP server 名（gc 时经 teardown 停摘）/ bundled MCP server names。
    #[serde(
        default,
        rename = "bundledMcpServers",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub bundled_mcp_servers: Vec<String>,
    /// 其余字段透传（store 拥有）/ passthrough fields owned by the store。
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `installed_plugins.json` 全量 / the whole installed-plugins file。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InstalledPlugins {
    /// `pid → 安装记录列表`（保物化顺序）/ ordered install records by plugin id。
    #[serde(default)]
    pub plugins: IndexMap<String, Vec<InstalledPluginRecord>>,
}

/// `installed_plugins_intent.json` 全量：**全局安装意图（权威）** / the global install-intent file。
///
/// v0.3.0（协议 §2.4）：install/uninstall 的权威写入入口。区别于 [`InstalledPlugins`] 账本——账本是
/// materialization 派生缓存（`installPath` / `bundledMcpServers` 等），可从本意图重建；本意图记「应装哪些」，
/// 是 boot 活跃集（= 已安装 ∧ 启用）中「已安装」维度的唯一权威来源。删账本无损、删本意图才真丢安装集。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledPluginsIntent {
    /// 已安装 `<plugin>@<marketplace>` 集合（保序）/ ordered set of installed plugin ids。
    #[serde(default, rename = "installedPlugins")]
    pub installed_plugins: IndexSet<String>,
}

/// SKILL 治理存储接缝（#67 SET-04 实现真 store、#68 INT-01 注入）/ governance store seam。
///
/// reconciler 经本 trait 读/改两份账本，**不**自持久化（原子写 + 恢复归 #67）。同一对象经
/// [`as_recorder`](Self::as_recorder) 复用为 staging 的 [`KnownMarketplaceRecorder`]，保证 stage 写入
/// 与 reconcile 读回同一份 `known_marketplaces.json`。RMW 闭包同步执行（遵守 store 的 `file_lock` 约束）。
pub trait SkillGovernanceStore: KnownMarketplaceRecorder {
    /// 读取 `known_marketplaces.json` 快照 / Load a snapshot。
    fn load_known_marketplaces(&self) -> KnownMarketplaces;
    /// RMW 修改 `known_marketplaces.json` / Read-modify-write。
    fn update_known_marketplaces(&self, mutate: &mut dyn FnMut(&mut KnownMarketplaces));
    /// 读取 `installed_plugins.json` 快照 / Load a snapshot。
    fn load_installed_plugins(&self) -> InstalledPlugins;
    /// RMW 修改 `installed_plugins.json` / Read-modify-write。
    fn update_installed_plugins(&self, mutate: &mut dyn FnMut(&mut InstalledPlugins));
    /// 读取 `installed_plugins_intent.json`（全局安装意图，v0.3.0）快照 / Load the install-intent snapshot。
    ///
    /// 默认返回空（供 test 替身无痛适配）；真实 store（[`FileSkillGovernanceStore`](crate::settings::store::FileSkillGovernanceStore)）覆盖之。
    fn load_installed_plugins_intent(&self) -> InstalledPluginsIntent {
        InstalledPluginsIntent::default()
    }
    /// RMW 修改 `installed_plugins_intent.json` / Read-modify-write the install intent。默认 no-op（见上）。
    fn update_installed_plugins_intent(
        &self,
        _mutate: &mut dyn FnMut(&mut InstalledPluginsIntent),
    ) {
    }
    /// 向上转型为 staging 记录器（避免依赖 trait upcasting 语言特性）/ upcast to the staging recorder。
    fn as_recorder(&self) -> &dyn KnownMarketplaceRecorder;
}

/// bundled MCP server 停摘回调接缝（真正接线由 computer.py 集成承担，本模块只触发）/ MCP teardown seam。
#[async_trait::async_trait]
pub trait McpTeardown: Send + Sync {
    /// 停止 / 摘除给定 bundled MCP server / Tear down the given bundled MCP servers。
    async fn teardown(&self, servers: Vec<String>);
}

// ===========================================================================
// 对账报告 + 选项 / Reconcile report + options
// ===========================================================================
/// 一次 [`reconcile`] 的结果（镜像 CC reconcile 返回；**无 removed** —— additive-only）/ Reconcile report。
///
/// 各 marketplace 名互斥地归入 installed / updated / up_to_date / failed / skipped 之一；
/// `registered_skills` 汇总本次成功注册 / 刷新的全部 SKILL 名（供 watcher / emit diff）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// 本次新 clone 的 marketplace / newly cloned。
    pub installed: Vec<String>,
    /// autoUpdate pull / sourceChanged 重 clone / updated。
    pub updated: Vec<String>,
    /// 声明且已在、未刷新 / declared and present, not refreshed。
    pub up_to_date: Vec<String>,
    /// 声明但 stage 后 clone 树仍缺失（clone 失败，已降级）/ failed (degraded)。
    pub failed: Vec<String>,
    /// 预留（trust 门控落 #68 / strict 延后）/ reserved。
    pub skipped: Vec<String>,
    /// 本次成功注册 / 刷新的全部 SKILL 名 / all registered skill names。
    pub registered_skills: Vec<String>,
}

/// [`reconcile`] 选项 / reconcile options。
#[derive(Default)]
pub struct ReconcileOptions<'a> {
    /// git 子进程 / 物化文件路径解析环境（`None` → 进程环境）/ git env。
    pub env: Option<&'a EnvMap>,
    /// `true` = 显式 sync：对**全部**已存在 clone 走 pull（不止 autoUpdate 项）；仍 additive-only。
    pub refresh: bool,
    /// 单次 git 操作超时（`None` → [`DEFAULT_GIT_TIMEOUT`]）/ per-op git timeout。
    pub timeout: Option<Duration>,
}

// ===========================================================================
// 声明视图提取 / Declared-view extraction
// ===========================================================================
/// 从单一声明视图取合法 marketplace 声明 / Extract valid marketplace declarations（保声明顺序）。
///
/// 跳过非法名（非 strict-kebab）/ 非对象条目（记 WARN）；返回 `(name, decl)` 列表。
fn declared_marketplaces(declared: &Map<String, Value>) -> Vec<(String, Map<String, Value>)> {
    let Some(Value::Object(raw)) = declared.get("extraKnownMarketplaces") else {
        return Vec::new();
    };
    let mut out: Vec<(String, Map<String, Value>)> = Vec::new();
    for (name, decl) in raw {
        if !is_valid_marketplace_name(name) {
            tracing::warn!(
                marketplace = %name,
                "reconcile: invalid marketplace name in extraKnownMarketplaces, skipped"
            );
            continue;
        }
        let Value::Object(decl_obj) = decl else {
            tracing::warn!(
                marketplace = %name,
                "reconcile: marketplace declaration is not an object, skipped"
            );
            continue;
        };
        out.push((name.clone(), decl_obj.clone()));
    }
    out
}

/// 单一声明视图里全部合法 marketplace 名 / All valid declared marketplace names（供孤儿判定）。
pub fn declared_marketplace_names(declared: &Map<String, Value>) -> HashSet<String> {
    declared_marketplaces(declared)
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}

/// 所有**声明过**的 plugin id（`<plugin>@<mp>`，含 `false` 禁用项）/ All declared plugin ids。
///
/// 用于孤儿判定——`false` = 声明禁用（**非**孤儿），仅 key 完全缺失才算孤儿。
pub fn declared_plugin_ids(declared: &Map<String, Value>) -> HashSet<String> {
    let Some(Value::Object(raw)) = declared.get("enabledPlugins") else {
        return HashSet::new();
    };
    raw.keys()
        .filter(|k| is_valid_enabled_plugin_key(k))
        .cloned()
        .collect()
}

/// 某 marketplace 下**启用**（`true`）的 plugin 名集合 / Enabled (`true`) plugin names for a marketplace。
///
/// `enabledPlugins` key 形如 `<plugin>@<mp>`；返回去掉 `@<mp>` 后缀的 `<plugin>` 集合，作
/// [`stage_marketplace_skills`] 的 plugin_filter（缺启用项 → 空集 → 仅 clone catalog、不注册 SKILL）。
fn enabled_plugin_names_for(marketplace: &str, declared: &Map<String, Value>) -> HashSet<String> {
    let Some(Value::Object(raw)) = declared.get("enabledPlugins") else {
        return HashSet::new();
    };
    let suffix = format!("@{marketplace}");
    let mut names: HashSet<String> = HashSet::new();
    for (key, enabled) in raw {
        if matches!(enabled, Value::Bool(true))
            && is_valid_enabled_plugin_key(key)
            && key.ends_with(&suffix)
        {
            names.insert(key[..key.len() - suffix.len()].to_string());
        }
    }
    names
}

// ===========================================================================
// 安全删除 / Guarded removal
// ===========================================================================
/// 仅当 `path`（**解析符号链接后**）位于 SKILL Home 内才递归删除 / rmtree only within SKILL Home。
///
/// `path` 不存在 → no-op；越界 → 记 ERROR + 拒删（不抛）。删除失败 → 记 WARN（不阻断后续清理）。
///
/// 安全 / Security：先 `canonicalize` **双侧**再做包含判定（解析符号链接），故 Home 内指向盘外的 symlink
/// （如 `installPath` 落点被植入 `home/.../evil -> /etc`）会被解析到盘外 → 拒删。这对齐 Python
/// `_safe_rmtree` 的 `resolve()` 语义与本仓库 [`sandbox`](crate::skills::sandbox) 的「符号链接逃逸」防御纵深
/// ——与纯词法的 [`is_within`]（只折叠 `..`、不解析 symlink）不同。canonicalize 双侧同时消解 macOS
/// `/var`→`/private/var` 等前缀分叉；已 `exists` 故几乎不失败，失败时词法兜底。
pub(crate) fn safe_rmtree(path: &Path, home: &Path) {
    // 不存在 → no-op（先短路：避免对缺失路径误报越界 + 无谓 ERROR，且符号链接需经 exists 跟随解析）。
    if !path.exists() {
        return;
    }
    let real_path = std::fs::canonicalize(path).unwrap_or_else(|_| normalize_lexical(path));
    let real_home = std::fs::canonicalize(home).unwrap_or_else(|_| normalize_lexical(home));
    if !is_within(&real_path, &real_home) {
        tracing::error!(
            path = %path.display(),
            home = %home.display(),
            "reconcile: refusing to remove path outside SKILL Home"
        );
        return;
    }
    if let Err(e) = std::fs::remove_dir_all(path) {
        tracing::warn!(path = %path.display(), error = %e, "reconcile: failed to remove");
    }
}

/// 从 Registry 注销某 marketplace（可选限定单 plugin）的 SKILL / Unregister a marketplace's SKILLs。
///
/// 按 `A2CSkillRef.source == "marketplace:<mp>"` 反查（marketplace SKILL name 形如 `<plugin>:<skill>`，
/// `plugin` 给定时再按 `<plugin>:` 前缀过滤）。additive-only 模型下 marketplace clone 树不上 watcher，
/// 其 SKILL 在 prune/gc 前恒活跃，故经 [`SkillRegistry::active_refs`] 枚举即可。
fn unregister_marketplace_skills(
    registry: &mut SkillRegistry,
    marketplace: &str,
    plugin: Option<&str>,
) {
    let want_source = format!("{SOURCE_MARKETPLACE}:{marketplace}");
    let prefix = plugin.map(|p| format!("{p}:"));
    let names: Vec<String> = registry
        .active_refs()
        .into_iter()
        .filter(|r| r.source == want_source)
        .filter(|r| {
            prefix
                .as_ref()
                .is_none_or(|pre| r.name.starts_with(pre.as_str()))
        })
        .map(|r| r.name)
        .collect();
    for name in names {
        registry.unregister(&name);
    }
}

// ===========================================================================
// 启动对账（additive-only）/ Startup reconcile (additive-only)
// ===========================================================================
/// 对账声明的 marketplace 与物化 clone 树（additive-only）/ Reconcile declared vs materialized。
///
/// 四分支（§7.1 step 3）/ Four branches:
/// - **missing**（declared∖materialized）→ clone（[`stage_marketplace_skills`] 缺失即 clone）。
/// - **sourceChanged**（`record.source != decl.source`）→ 先 `rmtree` 旧 clone 树（含外部 plugin 树）再重 clone
///   （reconciler 兜底：staging 仅 pull 不换库）。
/// - **autoUpdate**（`decl.autoUpdate == true` 或显式 `refresh`）→ `git pull`。
/// - **orphan**（materialized∖declared）→ **完全不动**（不进循环；靠 [`prune_marketplaces`] 显式清）。
///
/// 每个 declared marketplace 物化其 `enabledPlugins` 中**启用**的 plugin 的 SKILL；禁用 / 未声明 plugin 不注册。
/// 失败降级：stage 吞错返回空 → 本函数据 clone 树是否存在判 `failed`。
pub async fn reconcile(
    registry: &mut SkillRegistry,
    home: &Path,
    declared: &Map<String, Value>,
    store: &dyn SkillGovernanceStore,
    options: ReconcileOptions<'_>,
) -> ReconcileReport {
    let timeout = options.timeout.unwrap_or(DEFAULT_GIT_TIMEOUT);
    let declared_mps = declared_marketplaces(declared);
    // 对账前快照物化状态（stage 会在循环内经 recorder 回写，故先取基线分类）。
    let known = store.load_known_marketplaces();
    let materialized = &known.marketplaces;

    let mut report = ReconcileReport::default();
    let empty_source = Value::Object(Map::new());
    let ext_root = home.join(SOURCE_MARKETPLACE).join(EXTERNAL_PLUGINS_NS);

    for (name, decl) in &declared_mps {
        let source = decl.get("source").cloned().unwrap_or(Value::Null);
        let record = materialized.get(name);
        let source_changed = record.is_some_and(|r| r.source != source);
        let clone_dir = marketplace_skill_dir(home, name, &[]);

        if source_changed {
            // 源已变更：旧 clone 是「别的仓库」，必须 wipe 后重 clone（stage 只 pull 不换库）。
            tracing::info!(
                marketplace = %name,
                "reconcile: marketplace source changed, wiping stale clone for re-clone"
            );
            unregister_marketplace_skills(registry, name, None);
            safe_rmtree(&clone_dir, home);
            safe_rmtree(&ext_root.join(name), home);
        }

        let present_before = clone_dir.exists();
        let auto_update = decl
            .get("autoUpdate")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let do_refresh = auto_update || options.refresh;
        let plugin_filter = enabled_plugin_names_for(name, declared);

        // Python: source if isinstance(source, Mapping) else {} —— 非对象 source 传空对象。
        let source_for_stage = if source.is_object() {
            source.clone()
        } else {
            empty_source.clone()
        };
        let names = stage_marketplace_skills(
            name,
            &source_for_stage,
            registry,
            home,
            MarketplaceStageOptions {
                plugin_filter: Some(&plugin_filter),
                auto_update,
                refresh: do_refresh,
                timeout: Some(timeout),
                env: options.env,
                recorder: Some(store.as_recorder()),
            },
        )
        .await;
        report.registered_skills.extend(names);

        if !clone_dir.exists() {
            report.failed.push(name.clone());
        } else if source_changed {
            report.updated.push(name.clone());
        } else if !present_before {
            report.installed.push(name.clone());
        } else if do_refresh {
            report.updated.push(name.clone());
        } else {
            report.up_to_date.push(name.clone());
        }
    }

    report
}

// ===========================================================================
// 孤儿清理 / Orphan cleanup（§7.3，additive-only 唯一删除入口）
// ===========================================================================
/// 列出"所有 scope 都不再声明"的孤儿 marketplace（物化有、声明无）/ List orphan marketplaces。
///
/// 返回 `known_marketplaces.json` 中 key ∉ 声明集 的 marketplace 名（保持物化顺序）。
pub fn list_orphan_marketplaces(
    declared: &Map<String, Value>,
    store: &dyn SkillGovernanceStore,
) -> Vec<String> {
    let declared_names = declared_marketplace_names(declared);
    let known = store.load_known_marketplaces();
    known
        .marketplaces
        .keys()
        .filter(|name| !declared_names.contains(*name))
        .cloned()
        .collect()
}

/// 清理孤儿 marketplace（clone 树 + 外部 plugin 树 + known_marketplaces 条目 + Registry SKILL）/ Prune。
///
/// y/N 确认交 CLI 层（#68）；本函数只**执行**清理（`names` 应为 [`list_orphan_marketplaces`] 的子集，
/// 经用户确认后传入）。非法名跳过；删除越界守卫见 [`safe_rmtree`]。**不**触碰 `installed_plugins.json`
/// （plugin 账本归 [`gc_plugins`]）。返回实际清理的 marketplace 名列表。
pub fn prune_marketplaces(
    names: &[String],
    registry: &mut SkillRegistry,
    home: &Path,
    store: &dyn SkillGovernanceStore,
) -> Vec<String> {
    let ext_root = home.join(SOURCE_MARKETPLACE).join(EXTERNAL_PLUGINS_NS);
    let mut removed: Vec<String> = Vec::new();
    for name in names {
        if !is_valid_marketplace_name(name) {
            tracing::warn!(marketplace = %name, "prune: invalid marketplace name, skipped");
            continue;
        }
        unregister_marketplace_skills(registry, name, None);
        safe_rmtree(&marketplace_skill_dir(home, name, &[]), home);
        safe_rmtree(&ext_root.join(name), home);

        let drop_name = name.clone();
        store.update_known_marketplaces(&mut |data| {
            data.marketplaces.shift_remove(&drop_name);
        });
        removed.push(name.clone());
        tracing::info!(marketplace = %name, "prune: removed orphan marketplace");
    }
    removed
}

/// 列出孤儿 plugin：账本有记录、但 pid **不在 `installedPlugins` 安装意图**（= 陈旧派生缓存）/ List orphan plugins。
///
/// v0.3.0（协议 §2.4）：孤儿判定改用**安装意图**而非 `enabledPlugins`——install 不再写 `enabledPlugins`，
/// `installed_disabled`（已装未启用）plugin 仍是**合法安装、非孤儿**，绝不能被 gc 误删。正常操作下 install/uninstall
/// 同步写意图与账本，故孤儿仅出现于账本被外部改动 / 迁移残留等。`_declared` 保留仅为 API 兼容（v0.3.0 起不参与判定）。
/// 返回保持物化顺序。
pub fn list_orphan_plugins(
    _declared: &Map<String, Value>,
    store: &dyn SkillGovernanceStore,
) -> Vec<String> {
    let intent = store.load_installed_plugins_intent();
    let installed = store.load_installed_plugins();
    installed
        .plugins
        .keys()
        .filter(|pid| !intent.installed_plugins.contains(*pid))
        .cloned()
        .collect()
}

/// 清理孤儿 plugin（installPath 树 + installed_plugins 条目 + Registry SKILL + bundled MCP）/ GC plugins。
///
/// y/N 确认交 CLI 层（#68）；`plugin_ids` 应为 [`list_orphan_plugins`] 的子集（用户确认后传入）。每条记录的
/// `installPath` 仅在词法位于 SKILL Home 内才删（[`safe_rmtree`]）；`bundledMcpServers` 汇总后经 `mcp_teardown`
/// 回调停/摘（真正接线由 computer.py 集成承担，本模块只触发回调）。返回实际清理的 plugin id 列表。
pub async fn gc_plugins(
    plugin_ids: &[String],
    registry: &mut SkillRegistry,
    home: &Path,
    store: &dyn SkillGovernanceStore,
    mcp_teardown: Option<&dyn McpTeardown>,
) -> Vec<String> {
    let installed = store.load_installed_plugins();
    let mut removed: Vec<String> = Vec::new();
    for pid in plugin_ids {
        let Some(records) = installed.plugins.get(pid) else {
            continue;
        };
        let mut bundled: Vec<String> = Vec::new();
        for rec in records {
            if let Some(install_path) = rec.install_path.as_deref().filter(|s| !s.is_empty()) {
                safe_rmtree(Path::new(install_path), home);
            }
            bundled.extend(rec.bundled_mcp_servers.iter().cloned());
        }

        // pid = "<plugin>@<mp>"；无 '@' → marketplace 空（不注销）。
        let (plugin, marketplace) = pid.split_once('@').unwrap_or((pid.as_str(), ""));
        if !marketplace.is_empty() {
            unregister_marketplace_skills(registry, marketplace, Some(plugin));
        }

        if let Some(teardown) = mcp_teardown {
            if !bundled.is_empty() {
                teardown.teardown(bundled.clone()).await;
            }
        }

        let drop_pid = pid.clone();
        store.update_installed_plugins(&mut |data| {
            data.plugins.shift_remove(&drop_pid);
        });
        removed.push(pid.clone());
        tracing::info!(plugin = %pid, bundled = ?bundled, "gc: removed orphan plugin");
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::staging::KnownMarketplaceRecord;
    use smcp::A2CSkillRef;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use tempfile::TempDir;

    // ---- 内存 store 替身 / in-memory store fake -------------------------------
    #[derive(Default)]
    struct MemStore {
        known: Mutex<KnownMarketplaces>,
        installed: Mutex<InstalledPlugins>,
        intent: Mutex<InstalledPluginsIntent>,
    }

    impl KnownMarketplaceRecorder for MemStore {
        fn record(&self, record: KnownMarketplaceRecord<'_>) {
            let mut known = self.known.lock().unwrap();
            let entry = known
                .marketplaces
                .entry(record.name.to_string())
                .or_default();
            entry.source = record.source.clone();
            entry.extra.insert(
                "installLocation".into(),
                Value::String(record.install_location.to_string_lossy().into_owned()),
            );
            if let Some(sha) = record.commit_sha {
                entry
                    .extra
                    .insert("commitSha".into(), Value::String(sha.to_string()));
            }
            entry
                .extra
                .insert("autoUpdate".into(), Value::Bool(record.auto_update));
        }
    }

    impl SkillGovernanceStore for MemStore {
        fn load_known_marketplaces(&self) -> KnownMarketplaces {
            self.known.lock().unwrap().clone()
        }
        fn update_known_marketplaces(&self, mutate: &mut dyn FnMut(&mut KnownMarketplaces)) {
            mutate(&mut self.known.lock().unwrap());
        }
        fn load_installed_plugins(&self) -> InstalledPlugins {
            self.installed.lock().unwrap().clone()
        }
        fn update_installed_plugins(&self, mutate: &mut dyn FnMut(&mut InstalledPlugins)) {
            mutate(&mut self.installed.lock().unwrap());
        }
        fn load_installed_plugins_intent(&self) -> InstalledPluginsIntent {
            self.intent.lock().unwrap().clone()
        }
        fn update_installed_plugins_intent(
            &self,
            mutate: &mut dyn FnMut(&mut InstalledPluginsIntent),
        ) {
            mutate(&mut self.intent.lock().unwrap());
        }
        fn as_recorder(&self) -> &dyn KnownMarketplaceRecorder {
            self
        }
    }

    // ---- teardown 替身 / teardown fake ---------------------------------------
    struct RecordingTeardown {
        torn: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl McpTeardown for RecordingTeardown {
        async fn teardown(&self, servers: Vec<String>) {
            self.torn.lock().unwrap().extend(servers);
        }
    }

    fn mp_ref(name: &str, marketplace: &str, path: &Path) -> A2CSkillRef {
        A2CSkillRef {
            name: name.to_string(),
            source: format!("marketplace:{marketplace}"),
            uri: None,
            path: path.to_string_lossy().into_owned(),
            description: "d".to_string(),
            license: None,
            compatibility: None,
            allowed_tools: None,
            version: None,
            skill_metadata: None,
        }
    }

    fn declared(json: Value) -> Map<String, Value> {
        json.as_object().unwrap().clone()
    }

    // ---- 纯声明视图提取 / declared-view extraction ----------------------------
    #[test]
    fn declared_views_skip_invalid_and_disabled() {
        let d = declared(serde_json::json!({
            "extraKnownMarketplaces": {
                "acme": {"source": {"type": "git", "url": "u"}},
                "Bad Name": {"source": {}},          // 非法名 → 跳过
                "weird": "not-an-object"               // 非对象 → 跳过
            },
            "enabledPlugins": {
                "audit@acme": true,
                "lint@acme": false,                    // 禁用：仍是「已声明」（非孤儿）
                "fmt@other": true
            }
        }));
        assert_eq!(
            declared_marketplace_names(&d),
            HashSet::from(["acme".to_string()])
        );
        // declared_plugin_ids 含 false 项（仅 key 缺失才算孤儿）。
        assert_eq!(
            declared_plugin_ids(&d),
            HashSet::from([
                "audit@acme".to_string(),
                "lint@acme".to_string(),
                "fmt@other".to_string()
            ])
        );
        // enabled (true) plugin names for acme：仅 audit（lint=false 排除，fmt@other 后缀不符）。
        assert_eq!(
            enabled_plugin_names_for("acme", &d),
            HashSet::from(["audit".to_string()])
        );
    }

    // ---- safe_rmtree 越界守卫 / guarded removal ------------------------------
    #[test]
    fn safe_rmtree_refuses_outside_home() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("f"), b"x").unwrap();
        // outside 不在 home 内 → 拒删，目录仍在。
        safe_rmtree(&outside, &home);
        assert!(outside.exists(), "盘外目录不应被删");

        let inside = home.join("marketplace").join("acme");
        std::fs::create_dir_all(&inside).unwrap();
        safe_rmtree(&inside, &home);
        assert!(!inside.exists(), "home 内目录应被删");
    }

    // ---- unregister by source / plugin prefix --------------------------------
    #[test]
    fn unregister_marketplace_skills_filters_by_source_and_plugin() {
        let mut reg = SkillRegistry::new();
        let p = Path::new("/tmp/x");
        reg.register(mp_ref("audit:code-review", "acme", p));
        reg.register(mp_ref("audit:lint", "acme", p));
        reg.register(mp_ref("other:thing", "acme", p));
        reg.register(mp_ref("audit:x", "beta", p)); // 不同 marketplace

        // 限定 plugin=audit @ acme → 仅注销 audit:* of acme。
        unregister_marketplace_skills(&mut reg, "acme", Some("audit"));
        assert!(!reg.contains("audit:code-review"));
        assert!(!reg.contains("audit:lint"));
        assert!(reg.contains("other:thing"), "其它 plugin 保留");
        assert!(reg.contains("audit:x"), "其它 marketplace 保留");

        // 不限定 plugin → 注销 acme 全部。
        unregister_marketplace_skills(&mut reg, "acme", None);
        assert!(!reg.contains("other:thing"));
        assert!(reg.contains("audit:x"));
    }

    // ---- 孤儿 marketplace：list + prune --------------------------------------
    #[test]
    fn list_and_prune_orphan_marketplaces() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let store = MemStore::default();
        // 物化两个 marketplace；声明只剩 acme → beta 为孤儿。
        for mp in ["acme", "beta"] {
            let dir = marketplace_skill_dir(&home, mp, &[]);
            std::fs::create_dir_all(&dir).unwrap();
            store.record(KnownMarketplaceRecord {
                name: mp,
                source: &serde_json::json!({"type": "git", "url": mp}),
                install_location: &dir,
                commit_sha: Some("deadbeef"),
                auto_update: false,
                changed: true,
            });
        }
        let d = declared(serde_json::json!({
            "extraKnownMarketplaces": {"acme": {"source": {"type": "git", "url": "acme"}}}
        }));
        assert_eq!(
            list_orphan_marketplaces(&d, &store),
            vec!["beta".to_string()]
        );

        // registry 里 beta 的 SKILL 应被 prune 注销，clone 树被删，store 条目移除。
        let mut reg = SkillRegistry::new();
        reg.register(mp_ref("p:s", "beta", &home));
        let removed = prune_marketplaces(&["beta".to_string()], &mut reg, &home, &store);
        assert_eq!(removed, vec!["beta".to_string()]);
        assert!(!reg.contains("p:s"));
        assert!(!marketplace_skill_dir(&home, "beta", &[]).exists());
        assert!(!store
            .load_known_marketplaces()
            .marketplaces
            .contains_key("beta"));
        assert!(store
            .load_known_marketplaces()
            .marketplaces
            .contains_key("acme"));
    }

    // ---- 孤儿 plugin：list + gc（含 bundled MCP teardown）---------------------
    #[tokio::test]
    async fn list_and_gc_orphan_plugins_with_teardown() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let install_path = home.join("marketplace").join(".plugins").join("audit");
        std::fs::create_dir_all(&install_path).unwrap();

        let store = MemStore::default();
        store.update_installed_plugins(&mut |data| {
            data.plugins.insert(
                "audit@acme".to_string(),
                vec![InstalledPluginRecord {
                    install_path: Some(install_path.to_string_lossy().into_owned()),
                    bundled_mcp_servers: vec!["audit-mcp".to_string()],
                    extra: Map::new(),
                }],
            );
            data.plugins.insert(
                "lint@acme".to_string(),
                vec![InstalledPluginRecord::default()],
            );
        });

        // v0.3.0：安装意图只含 lint@acme → 账本里的 audit@acme 无对应意图 = 孤儿（陈旧派生缓存）。
        // （enabledPlugins 不再参与孤儿判定：installed_disabled 仍是合法安装，绝不 gc。）
        store.update_installed_plugins_intent(&mut |i| {
            i.installed_plugins.insert("lint@acme".to_string());
        });
        let d = declared(serde_json::json!({}));
        assert_eq!(
            list_orphan_plugins(&d, &store),
            vec!["audit@acme".to_string()]
        );

        let mut reg = SkillRegistry::new();
        reg.register(mp_ref("audit:code-review", "acme", &home));
        reg.register(mp_ref("lint:fmt", "acme", &home));
        let teardown = RecordingTeardown {
            torn: Mutex::new(Vec::new()),
        };
        let removed = gc_plugins(
            &["audit@acme".to_string()],
            &mut reg,
            &home,
            &store,
            Some(&teardown),
        )
        .await;
        assert_eq!(removed, vec!["audit@acme".to_string()]);
        // audit plugin 的 SKILL 注销、lint 保留。
        assert!(!reg.contains("audit:code-review"));
        assert!(reg.contains("lint:fmt"));
        // installPath 树删除、store 条目移除、bundled MCP 被 teardown。
        assert!(!install_path.exists());
        assert!(!store
            .load_installed_plugins()
            .plugins
            .contains_key("audit@acme"));
        assert_eq!(
            *teardown.torn.lock().unwrap(),
            vec!["audit-mcp".to_string()]
        );
    }

    // ---- reconcile 四分支（真实 git，离线 file://）/ real-git integration ------
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

    fn make_marketplace_repo(root: &Path, url_skill_desc: &str) -> PathBuf {
        let repo = root.join("repo");
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
            format!("---\nname: code-review\ndescription: {url_skill_desc}\n---\nbody"),
        )
        .unwrap();
        git(&["init", "-q"], &repo);
        git(&["add", "-A"], &repo);
        git(&["commit", "-qm", "init"], &repo);
        repo
    }

    #[tokio::test]
    async fn reconcile_installs_then_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let repo = make_marketplace_repo(tmp.path(), "review code");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let url = format!("file://{}", repo.display());
        let d = declared(serde_json::json!({
            "extraKnownMarketplaces": {"acme": {"source": {"type": "git", "url": url}}},
            "enabledPlugins": {"audit@acme": true}
        }));
        let store = MemStore::default();
        let mut reg = SkillRegistry::new();

        // 首次：missing → installed + 注册 SKILL。
        let r1 = reconcile(&mut reg, &home, &d, &store, ReconcileOptions::default()).await;
        assert_eq!(r1.installed, vec!["acme".to_string()]);
        assert!(r1.updated.is_empty() && r1.up_to_date.is_empty() && r1.failed.is_empty());
        assert_eq!(r1.registered_skills, vec!["audit:code-review".to_string()]);
        assert!(reg.contains("audit:code-review"));

        // 二次：声明且已在、未刷新 → up_to_date（幂等，不重 clone）。
        let r2 = reconcile(&mut reg, &home, &d, &store, ReconcileOptions::default()).await;
        assert_eq!(r2.up_to_date, vec!["acme".to_string()]);
        assert!(r2.installed.is_empty() && r2.updated.is_empty());
        assert_eq!(r2.registered_skills, vec!["audit:code-review".to_string()]);
    }

    #[tokio::test]
    async fn reconcile_source_changed_reclones() {
        let tmp = TempDir::new().unwrap();
        let repo_a = make_marketplace_repo(&tmp.path().join("a"), "from A");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let url_a = format!("file://{}", repo_a.display());
        let store = MemStore::default();
        let mut reg = SkillRegistry::new();

        let d_a = declared(serde_json::json!({
            "extraKnownMarketplaces": {"acme": {"source": {"type": "git", "url": url_a}}},
            "enabledPlugins": {"audit@acme": true}
        }));
        let r1 = reconcile(&mut reg, &home, &d_a, &store, ReconcileOptions::default()).await;
        assert_eq!(r1.installed, vec!["acme".to_string()]);

        // 换源（另一个 repo，url 不同）→ sourceChanged → wipe + 重 clone → updated。
        let repo_b = make_marketplace_repo(&tmp.path().join("b"), "from B");
        let url_b = format!("file://{}", repo_b.display());
        let d_b = declared(serde_json::json!({
            "extraKnownMarketplaces": {"acme": {"source": {"type": "git", "url": url_b}}},
            "enabledPlugins": {"audit@acme": true}
        }));
        let r2 = reconcile(&mut reg, &home, &d_b, &store, ReconcileOptions::default()).await;
        assert_eq!(r2.updated, vec!["acme".to_string()]);
        assert!(r2.installed.is_empty() && r2.up_to_date.is_empty());
        // 新内容生效（描述来自 repo_b）。
        assert_eq!(
            reg.resolve("audit:code-review").unwrap().description,
            "from B"
        );
    }

    #[tokio::test]
    async fn reconcile_failed_when_clone_unreachable() {
        // 失败降级铁律（§7.2）：clone 失败 → 不入 Registry、归 report.failed、不抛。
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let bad_url = format!("file://{}/nonexistent-repo", tmp.path().display());
        let d = declared(serde_json::json!({
            "extraKnownMarketplaces": {"acme": {"source": {"type": "git", "url": bad_url}}},
            "enabledPlugins": {"audit@acme": true}
        }));
        let store = MemStore::default();
        let mut reg = SkillRegistry::new();

        let r = reconcile(&mut reg, &home, &d, &store, ReconcileOptions::default()).await;
        assert_eq!(
            r.failed,
            vec!["acme".to_string()],
            "clone 失败应归入 failed"
        );
        assert!(r.installed.is_empty() && r.updated.is_empty() && r.up_to_date.is_empty());
        assert!(r.registered_skills.is_empty(), "失败源不注册 SKILL");
        assert!(reg.is_empty(), "失败源对 Agent 不可见（不入 Registry）");
    }

    #[tokio::test]
    async fn reconcile_refresh_marks_updated() {
        // do_refresh 分支（refresh=true / autoUpdate）：已存在 clone 走 pull → updated。
        let tmp = TempDir::new().unwrap();
        let repo = make_marketplace_repo(tmp.path(), "review code");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let url = format!("file://{}", repo.display());
        let d = declared(serde_json::json!({
            "extraKnownMarketplaces": {"acme": {"source": {"type": "git", "url": url}}},
            "enabledPlugins": {"audit@acme": true}
        }));
        let store = MemStore::default();
        let mut reg = SkillRegistry::new();

        let r1 = reconcile(&mut reg, &home, &d, &store, ReconcileOptions::default()).await;
        assert_eq!(r1.installed, vec!["acme".to_string()]);

        // 显式 sync：refresh=true → 对已存在 clone pull → updated（非 sourceChanged、非 installed）。
        let r2 = reconcile(
            &mut reg,
            &home,
            &d,
            &store,
            ReconcileOptions {
                refresh: true,
                ..Default::default()
            },
        )
        .await;
        assert_eq!(
            r2.updated,
            vec!["acme".to_string()],
            "refresh 应归入 updated"
        );
        assert!(r2.installed.is_empty() && r2.up_to_date.is_empty());
    }

    #[tokio::test]
    async fn reconcile_leaves_orphan_untouched() {
        // additive-only：materialized∖declared 的孤儿 → reconcile 完全不动（不进循环）。
        let tmp = TempDir::new().unwrap();
        let repo = make_marketplace_repo(tmp.path(), "review code");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let url = format!("file://{}", repo.display());
        let store = MemStore::default();
        let mut reg = SkillRegistry::new();

        // 预置孤儿 beta：store 记录 + clone 树 + registry SKILL（声明里没有 beta）。
        let beta_dir = marketplace_skill_dir(&home, "beta", &[]);
        std::fs::create_dir_all(&beta_dir).unwrap();
        store.record(KnownMarketplaceRecord {
            name: "beta",
            source: &serde_json::json!({"type": "git", "url": "beta"}),
            install_location: &beta_dir,
            commit_sha: Some("deadbeef"),
            auto_update: false,
            changed: true,
        });
        reg.register(mp_ref("legacy:s", "beta", &beta_dir));

        let d = declared(serde_json::json!({
            "extraKnownMarketplaces": {"acme": {"source": {"type": "git", "url": url}}},
            "enabledPlugins": {"audit@acme": true}
        }));
        let r = reconcile(&mut reg, &home, &d, &store, ReconcileOptions::default()).await;

        // acme 正常 installed；beta 三处（store / registry / clone 树）全不动。
        assert_eq!(r.installed, vec!["acme".to_string()]);
        assert!(
            !r.failed.contains(&"beta".to_string()) && !r.updated.contains(&"beta".to_string())
        );
        assert!(store
            .load_known_marketplaces()
            .marketplaces
            .contains_key("beta"));
        assert!(reg.contains("legacy:s"), "孤儿 SKILL 不应被 reconcile 注销");
        assert!(beta_dir.exists(), "孤儿 clone 树不应被 reconcile 删除");
    }

    #[tokio::test]
    async fn gc_without_teardown_callback() {
        // mcp_teardown = None：仍删 plugin + store 条目，不 panic。
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let install_path = home.join("marketplace").join(".plugins").join("audit");
        std::fs::create_dir_all(&install_path).unwrap();
        let store = MemStore::default();
        store.update_installed_plugins(&mut |data| {
            data.plugins.insert(
                "audit@acme".to_string(),
                vec![InstalledPluginRecord {
                    install_path: Some(install_path.to_string_lossy().into_owned()),
                    bundled_mcp_servers: vec!["audit-mcp".to_string()],
                    extra: Map::new(),
                }],
            );
        });
        let mut reg = SkillRegistry::new();
        let removed = gc_plugins(&["audit@acme".to_string()], &mut reg, &home, &store, None).await;
        assert_eq!(removed, vec!["audit@acme".to_string()]);
        assert!(!install_path.exists());
        assert!(!store
            .load_installed_plugins()
            .plugins
            .contains_key("audit@acme"));
    }

    #[tokio::test]
    async fn gc_skips_unknown_pid() {
        // 未知 pid → continue（不在 installed）→ 返回空、store 不变。
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let store = MemStore::default();
        let mut reg = SkillRegistry::new();
        let removed = gc_plugins(
            &["ghost@nowhere".to_string()],
            &mut reg,
            &home,
            &store,
            None,
        )
        .await;
        assert!(removed.is_empty());
        assert!(store.load_installed_plugins().plugins.is_empty());
    }

    #[tokio::test]
    async fn gc_pid_without_marketplace() {
        // pid 无 '@' → marketplace 空、不尝试注销 → 仍从 store 移除。
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let store = MemStore::default();
        store.update_installed_plugins(&mut |data| {
            data.plugins.insert(
                "local-only".to_string(),
                vec![InstalledPluginRecord::default()],
            );
        });
        let mut reg = SkillRegistry::new();
        let removed = gc_plugins(&["local-only".to_string()], &mut reg, &home, &store, None).await;
        assert_eq!(removed, vec!["local-only".to_string()]);
        assert!(!store
            .load_installed_plugins()
            .plugins
            .contains_key("local-only"));
    }

    #[test]
    fn prune_skips_invalid_name() {
        // 非法 marketplace 名 → 跳过、返回空。
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let store = MemStore::default();
        let mut reg = SkillRegistry::new();
        let removed = prune_marketplaces(&["Bad Name".to_string()], &mut reg, &home, &store);
        assert!(removed.is_empty(), "非法名不应被清理");
    }

    #[cfg(unix)]
    #[test]
    fn safe_rmtree_refuses_symlink_escape() {
        // Home 内指向盘外的 symlink → canonicalize 解析到盘外 → 拒删（对齐 Python resolve() + sandbox.rs）。
        use std::os::unix::fs::symlink;
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let mp = home.join("marketplace");
        std::fs::create_dir_all(&mp).unwrap();
        // 盘外目标（含一份文件，便于断言未被删）。
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keep"), b"x").unwrap();
        // home 内的逃逸 symlink。
        let evil = mp.join("evil");
        symlink(&outside, &evil).unwrap();

        safe_rmtree(&evil, &home);
        assert!(outside.join("keep").exists(), "符号链接逃逸目标不应被删");
        assert!(outside.exists());
    }
}
