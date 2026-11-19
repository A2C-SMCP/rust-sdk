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
//! - **同名不同源拒绝**（runtime-contract §5.14 / #294）：声明面 marketplace source 与账本记录不一致时
//!   **拒绝**——不注销 SKILL、不删 clone 树、不 stage/pull；旧注册、旧物化内容与旧运行时能力保持 active。
//!   拒绝归 `failed` 并记 `reason="marketplace_source_conflict"` 的结构化 ERROR；换源必须显式
//!   [`prune_marketplaces`] / lifecycle remove 后 re-add，且既有 `<plugin>@<name>` 意图引用须先移除
//!   （§5.14 ④，由 #272 的 remove/add 语义执行）。同源判定为精确 JSON `Value` 相等（canonical 归一化归
//!   #272）；已注册名的新声明 source 不可克隆时同样拒绝（`reason="marketplace_source_unusable"`，fail-closed）。
//!   **前置契约**：拒绝分支不 stage——本函数只保留调用方传入的 `SkillRegistry` 现状。用空 Registry 做启动
//!   对账的调用方须先跑 [`recover_marketplace_skills`](super::recovery::recover_marketplace_skills) 或传入
//!   既有 Registry，否则被拒 marketplace 的旧 SKILL 不会出现在该 Registry（账本与 clone 树不受影响）。
//! - **失败降级铁律**（§7.2）：git clone/pull 失败 → 记 ERROR、不阻断其余、对 Agent 不可见（不入 Registry）；
//!   本模块**不**向调用方抛 git/IO 异常（沿用 [`stage_marketplace_skills`] 的吞错降级）。§5.14 拒绝**不是**
//!   clone/pull 失败：旧能力保持 active、不降级。
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

use crate::mcp_clients::model::BundleId;
use crate::settings::redaction::{git_url_for_display, REDACTED_GIT_SOURCE};
use crate::settings::schema::{is_valid_enabled_plugin_key, is_valid_marketplace_name};
use crate::settings::scope::EnvMap;
use crate::skills::home::{marketplace_skill_dir, SOURCE_MARKETPLACE};
use crate::skills::registry::SkillRegistry;
use crate::skills::sources::marketplace_clone_url;
use crate::skills::staging::{
    external_plugin_dir, stage_marketplace_skills, KnownMarketplaceRecorder,
    MarketplaceStageOptions, DEFAULT_GIT_TIMEOUT, EXTERNAL_PLUGINS_NS,
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
    /// `name → entry`（内存内 IndexMap 保插入序；**文件 reload 经 serde_json Map 不保证原序**——不得把
    /// 该顺序当 SKILL 所有权仲裁依据，已装 plugin 的源优先）/ in-memory ordered records.
    #[serde(default)]
    pub marketplaces: IndexMap<String, KnownMarketplaceEntry>,
}

/// `installed_plugins.json` 单条安装记录 / one plugin install record。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct InstalledPluginRecord {
    /// plugin 物化落点。删除策略：**仅当**位于该 plugin 独占外部 clone
    /// `<home>/marketplace/.plugins/<mp>/<plugin>` 内才删（`remove_plugin_install_path`）；相对源位于共享
    /// catalog clone 的路径保留，归 marketplace remove/prune 清理（#271）/ install path。
    #[serde(
        default,
        rename = "installPath",
        skip_serializing_if = "Option::is_none"
    )]
    pub install_path: Option<String>,
    /// plugin 声明依赖的 MCP server **bundle_id**（纯数组，无 name/provenance）/ declared MCP dependency bundle_ids。
    ///
    /// #139/D3：语义 = 「plugin 声明的 MCP 依赖」（依赖关系，非所有关系）。**不存 name**——防 drift（显式
    /// `bundleId` 的 server 改名后账本名即过期）；display 名从 `installPath` 实时解析。**不存 provenance**——
    /// 「字段降级但消费者没跟上」正是档④ 成因模式，删除才是可靠护栏。gc/uninstall 停摘名单经**回收判据**
    /// （无其他 plugin 依赖 ∧ 非用户声明）过滤后才 teardown。旧格式 `bundledMcpServers: ["<name>"]` 检测即
    /// 丢弃 + 从 `installedPlugins` 意图重建（见 [`rematerialize_missing_ledger_records`](super::recovery::rematerialize_missing_ledger_records)），**无 name→id 映射**。
    #[serde(default, rename = "mcpServers", skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<BundleId>,
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
    /// 停止 / 摘除给定 bundled MCP server（按 **bundle_id** 寻址）/ Tear down by bundle_id。
    ///
    /// #139/R4：由 `Vec<String>`（name）改 `Vec<BundleId>`——第二条 name 停摘链收口；同名两 server 精确停摘。
    async fn teardown(&self, servers: Vec<BundleId>);
}

// ===========================================================================
// 对账报告 + 选项 / Reconcile report + options
// ===========================================================================
/// 一次 [`reconcile`] 的结果（镜像 CC reconcile 返回；**无 removed** —— additive-only）/ Reconcile report。
///
/// 各 marketplace 名互斥地归入 installed / updated / up_to_date / failed / skipped 之一；
/// `registered_skills` 汇总本次成功注册 / 刷新的全部 SKILL 名（供 watcher / emit diff）。
///
/// `failed` 是一个并集：clone/pull 失败（降级）**或** §5.14 source 拒绝（同名不同源 / 声明 source 不可克隆；
/// 旧注册保留、不降级）；后者 reason 只在 ERROR 日志的结构化字段中
/// （`marketplace_source_conflict` / `marketplace_source_unusable`），本结构不加字段以保 API 稳定。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// 本次新 clone 的 marketplace / newly cloned。
    pub installed: Vec<String>,
    /// autoUpdate pull / 显式 refresh 的 pull / updated（source 冲突项不 pull，见 [`reconcile`]）。
    pub updated: Vec<String>,
    /// 声明且已在、未刷新 / declared and present, not refreshed。
    pub up_to_date: Vec<String>,
    /// 本次**声明但未应用**的 marketplace，两种原因并列：① stage 后 clone 树仍缺失（clone/pull 失败，
    /// 可重试，已降级）；② §5.14 同名不同源 / 声明 source 不可克隆（**不可重试**的 validation 冲突，旧注册、
    /// 旧内容、旧能力保持 active、**不**降级）。② 的 reason 只在 ERROR 日志的结构化字段中。
    ///
    /// 调用方 MUST NOT 把本列表当作「该 marketplace 的旧注册已失效」：② 的旧注册仍 active（**前提**：调用方
    /// 传入的 `SkillRegistry` 已含旧物化，或启动时已先跑 recovery——拒绝分支不 stage，空 Registry 下旧 SKILL
    /// 不会出现）；也 MUST NOT 对 ② 做无界重试（须先修配置并显式 remove + re-add）。
    /// / declared but not applied: clone failure (retryable, degraded) or §5.14 source conflict
    /// (non-retryable validation; old registration stays active only if the caller supplied the existing
    /// Registry or ran recovery first — the rejection path never stages).
    pub failed: Vec<String>,
    /// 预留（trust 门控落 #68 / strict 延后）/ reserved。§5.14 拒绝 MUST NOT 占用本桶。
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
    /// 声明面 source 与账本冲突的项**不** pull（§5.14 拒绝优先，见 [`reconcile`]）。
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

/// `installPath` 是否位于该 plugin 独占的外部 clone `<home>/marketplace/.plugins/<mp>/<plugin>`
/// （含 `git-subdir` 的后代子目录）/ whether installPath lives in the plugin-exclusive external clone。
///
/// 供删除谓词与 uninstall 的「剩余 scope 是否仍引用该 clone」判定共用。谓词为纯词法（[`is_within`] 分量级 +
/// `..` 折叠），与 `installPath` 的生成方式一致，且路径可能已不存在。空/非法 marketplace 或 plugin 名
/// （含 gc 中无 `@` 的 pid）→ `false`（无法识别独占根，保守保留，漏删优于误删）。
pub(crate) fn is_external_plugin_install_path(
    install_path: &Path,
    home: &Path,
    marketplace: &str,
    plugin: &str,
) -> bool {
    if install_path.as_os_str().is_empty()
        || !is_valid_marketplace_name(marketplace)
        || !is_valid_marketplace_name(plugin)
    {
        return false;
    }
    is_within(
        install_path,
        &external_plugin_dir(home, marketplace, plugin),
    )
}

/// 按 plugin 文件归属删除 `installPath`：**仅**当其位于该 plugin 独占的外部 clone
/// `<home>/marketplace/.plugins/<mp>/<plugin>`（含 `git-subdir` 的后代子目录）时，删除**整个 clone 根**并返回
/// `true`；其余路径保留并返回 `false`（#271 方案 A）。
///
/// 相对源（`source: "./plugins/x"`）的 `installPath` 落在**共享 catalog clone** `<home>/marketplace/<mp>/...`：
/// 该树是 marketplace 级缓存——兄弟插件、剩余 scope 与重装都依赖它，归 marketplace remove/prune/gc 清理，
/// uninstall / plugin-gc MUST NOT 删除。
///
/// 实际删除仍由 [`safe_rmtree`] 的 canonicalize + Home 包含守卫兜底（磁盘上 ext root 为 symlink 时拒删）。
pub(crate) fn remove_plugin_install_path(
    install_path: &Path,
    home: &Path,
    marketplace: &str,
    plugin: &str,
) -> bool {
    if !is_external_plugin_install_path(install_path, home, marketplace, plugin) {
        tracing::debug!(
            path = %install_path.display(),
            marketplace,
            plugin,
            "keeping installPath (shared marketplace catalog / non-exclusive path)"
        );
        return false;
    }
    safe_rmtree(&external_plugin_dir(home, marketplace, plugin), home);
    true
}

/// 从 Registry 注销某 marketplace（可选限定单 plugin）的 SKILL / Unregister a marketplace's SKILLs。
///
/// 按 `A2CSkillRef.source == "marketplace:<mp>"` 反查（marketplace SKILL name 形如 `<plugin>:<skill>`，
/// `plugin` 给定时再按 `<plugin>:` 前缀过滤）。**含孤儿**：installed_disabled plugin 的 SKILL 在 Registry
/// 中保留为 orphan（不投影但占名），prune / gc（显式删除入口）时必须一并注销，否则同名跨源守卫会阻止换源。
/// **§5.14 同名不同源拒绝路径 MUST NOT 调用本函数**——旧注册的 SKILL（含孤儿）必须保持可用。
fn unregister_marketplace_skills(
    registry: &mut SkillRegistry,
    marketplace: &str,
    plugin: Option<&str>,
) {
    let want_source = format!("{SOURCE_MARKETPLACE}:{marketplace}");
    let prefix = plugin.map(|p| format!("{p}:"));
    let names = registry.names_for_source(&want_source, prefix.as_deref().unwrap_or(""), true);
    for name in names {
        registry.unregister(&name);
    }
    // #275：注销该源（或该 plugin×源）后，相关跨源冲突记录随之失效，避免后续 add/refresh 重发 stale 诊断。
    let conflict_prefix = prefix.unwrap_or_default();
    registry.clear_source_conflicts_where(|c| {
        c.name.starts_with(&conflict_prefix)
            && (c.kept_source == want_source || c.rejected_source == want_source)
    });
}

// ===========================================================================
// 启动对账（additive-only）/ Startup reconcile (additive-only)
// ===========================================================================
/// 「同名不同源」拒绝的稳定 reason 字段值（结构化 ERROR 日志；MUST NOT 复用为 `skipped`）。
const REASON_SOURCE_CONFLICT: &str = "marketplace_source_conflict";

/// 已注册名对应的声明 source 不可用（与账本同形但非合法 git source）时的 fail-closed reason。
///
/// 两侧「同样不可用」无法证明同源（§5.14「无法确定时 MUST fail-closed」）→ 视为注册被拒。
const REASON_SOURCE_UNUSABLE: &str = "marketplace_source_unusable";

/// marketplace source Value → 安全展示值（日志/诊断边界）。
///
/// 只取 `url` 子字段并经 [`git_url_for_display`] 脱敏（userinfo / query / fragment 一律移除）；
/// 非对象或缺 `url` → [`REDACTED_GIT_SOURCE`] fail-closed。**不可**对整个 JSON 文本做
/// `redact_git_urls_in_text`：token 以空白划分，尾随 `"}` 会并入 URL 破坏展示。
fn source_for_log(source: &Value) -> String {
    source
        .get("url")
        .and_then(Value::as_str)
        .map(git_url_for_display)
        .unwrap_or_else(|| REDACTED_GIT_SOURCE.to_string())
}

/// 对账声明的 marketplace 与物化 clone 树（additive-only）/ Reconcile declared vs materialized。
///
/// 四分支（§7.1 step 3）/ Four branches:
/// - **missing**（declared∖materialized）→ clone（[`stage_marketplace_skills`] 缺失即 clone）。
/// - **sourceChanged**（`record.source != decl.source`）→ **拒绝**（runtime-contract §5.14）：不注销 SKILL、不
///   `rmtree`、不 stage/pull，归 `failed` 并记 `reason="marketplace_source_conflict"` 的 ERROR；旧注册、旧物化
///   内容与旧运行时能力保持 active。换源必须显式 [`prune_marketplaces`] / lifecycle remove 后 re-add，且既有
///   `<plugin>@<name>` 意图引用须先移除（§5.14 ④，由 #272 的 remove/add 语义执行）。
///   同源判定为精确 JSON `Value` 相等（canonical 归一化归 #272）；已注册名的新声明 source 不可克隆时同样
///   拒绝（`reason="marketplace_source_unusable"`，fail-closed——无法证明同源）。
/// - **autoUpdate**（`decl.autoUpdate == true` 或显式 `refresh`）→ `git pull`（source 冲突项不 pull，拒绝优先）。
/// - **orphan**（materialized∖declared）→ **完全不动**（不进循环；靠 [`prune_marketplaces`] 显式清）。
///
/// 每个 declared marketplace 物化其 `enabledPlugins` 中**启用**的 plugin 的 SKILL；禁用 / 未声明 plugin 不注册。
/// 失败降级：stage 吞错返回空且 clone 树缺失 → 归 `failed`（降级）；若 stage 失败但 clone 树仍在（如 pull 成功
/// 而 manifest 解析失败），沿用既有行为归 `up_to_date` / `updated`（既有表述边界，另行跟进）。§5.14 拒绝同样归
/// `failed`，但它**不是** clone/pull 失败——旧能力保持 active、不降级，见 [`ReconcileReport::failed`]。
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

    for (name, decl) in &declared_mps {
        let source = decl.get("source").cloned().unwrap_or(Value::Null);

        // §5.14 / #294：一名一源；声明面换源 MUST 拒绝（validation），MUST NOT 静默覆盖/替换。
        // 守卫在任何 unregister / rmtree / stage / pull 之前；旧注册、旧物化内容与旧能力保持 active。
        // 精确 `Value` 不等即拒绝（fail-closed）：归一化（同一仓库不同写法）归 #272 的身份语义。
        // 已注册名 + 声明 source 不可用（两侧同为 null / `{}` 等）同样无法证明同源 → fail-closed 拒绝，
        // 否则 stage 空转后会被误报为 up_to_date / updated（看似成功但什么都没应用）。
        let record = materialized.get(name);
        let source_conflict = record.is_some_and(|r| r.source != source);
        let source_unusable = record.is_some() && marketplace_clone_url(&source).is_err();
        if source_conflict || source_unusable {
            let registered = record.map_or(Value::Null, |r| r.source.clone());
            let reason = if source_conflict {
                REASON_SOURCE_CONFLICT
            } else {
                REASON_SOURCE_UNUSABLE
            };
            tracing::error!(
                marketplace = %name,
                reason = reason,
                registered_source = %source_for_log(&registered),
                declared_source = %source_for_log(&source),
                "reconcile: rejected marketplace source (different or unusable source; explicit remove + re-add required)"
            );
            report.failed.push(name.clone());
            continue;
        }

        let clone_dir = marketplace_skill_dir(home, name, &[]);
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
/// 经用户确认后传入）。非法名跳过；删除越界守卫见 `safe_rmtree`。**不**触碰 `installed_plugins.json`
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

/// §4.9.1-2 回收判据「无其他 plugin 依赖 X」项：除 `exclude_pid` 外全部 plugin 声明依赖的并集 /
/// union of every OTHER plugin's declared MCP deps。纯函数、只读账本自身字段（停摘自足）。
///
/// **不含同 pid 的 retained 记录**：`gc` 等整 pid 语义用本入口；`uninstall` 不论 scope 均走
/// [`other_plugin_mcp_deps_with_retained`]（全 scope 移除时传空 slice）；`disable` 不删记录、按整 plugin
/// 下线语义传空（#284）。
#[must_use]
pub fn other_plugin_mcp_deps(installed: &InstalledPlugins, exclude_pid: &str) -> HashSet<BundleId> {
    other_plugin_mcp_deps_with_retained(installed, exclude_pid, &[])
}

/// 同 [`other_plugin_mcp_deps`]，但并入 `exclude_pid` 本次**未被移除**的 `retained_records`（#284，与
/// python#262 同口径）/ Same as above plus this pid's retained (not removed) records。
///
/// 回收判据：X 回收 ⟺ **无任何剩余记录**（其他 plugin 的全部记录 + 同 pid retained 记录）声明依赖 X ∧ X 非
/// 用户声明。指定 scope 卸载只删一个 scope 时，retained 记录仍声明依赖 ⇒ X MUST 保留；整 pid 移除传空 slice。
#[must_use]
pub fn other_plugin_mcp_deps_with_retained(
    installed: &InstalledPlugins,
    exclude_pid: &str,
    retained_records: &[InstalledPluginRecord],
) -> HashSet<BundleId> {
    let mut out: HashSet<BundleId> = HashSet::new();
    for (pid, records) in &installed.plugins {
        if pid == exclude_pid {
            continue;
        }
        for rec in records {
            out.extend(rec.mcp_servers.iter().cloned());
        }
    }
    for rec in retained_records {
        out.extend(rec.mcp_servers.iter().cloned());
    }
    out
}

/// §4.9.1-2 回收判据（纯函数、零落盘状态）：**回收 X ⟺ 无其他 plugin 依赖 X ∧ X 非用户声明** /
/// recycle X ⟺ (no other plugin declares X) ∧ (X is not user-declared)。
///
/// D5 措辞（正式重写）：~~「只收回自己带入且无其他 plugin 仍依赖」~~ → 「只收回**无人再依赖 ∧ 非用户声明**」——
/// 原措辞把时点快照写进长期判据、是泄漏来源。
///
/// - `deps`：本次 disable/uninstall/gc 的 plugin 所声明的依赖（`ledger_mcp_deps_of` 产出）。
/// - `other_deps`：[`other_plugin_mcp_deps`] / [`other_plugin_mcp_deps_with_retained`] 产出。
/// - `non_plugin_declared`：**带 `origin` 的运行期权威配置集中 `origin != plugin` 的 bundle_id 集**
///   （durable scopes + flag `--mcp-config` + embed 构造入参）——Discussion #32 裁决。**MUST 传全集**：
///   退回只读 mcp.json 声明面会让经 flag/embed 挂载的用户/宿主 server 被误判「非用户声明」而**连坐**停摘
///   （#153 遗留缺口形状）。**MUST NOT** 用裸活跃集（无 origin ⇒ 三挂载路径同形）或「活跃集 ∖ 全 plugin
///   声明集」差集（对回收候选恒空 = 死代码，python 已实测证伪）。
///
/// 返回可回收的 bundle_id（保 `deps` 迭代序）。
#[must_use]
pub fn reclaimable_mcp_deps(
    deps: &[BundleId],
    other_deps: &HashSet<BundleId>,
    non_plugin_declared: &HashSet<BundleId>,
) -> Vec<BundleId> {
    deps.iter()
        .filter(|d| !other_deps.contains(*d) && !non_plugin_declared.contains(*d))
        .cloned()
        .collect()
}

/// 清理孤儿 plugin（独占外部 clone + installed_plugins 条目 + Registry SKILL + bundled MCP）/ GC plugins。
///
/// y/N 确认交 CLI 层（#68）；`plugin_ids` 应为 [`list_orphan_plugins`] 的子集（用户确认后传入）。每条记录的
/// `installPath` **仅当位于该 plugin 独占外部 clone `<home>/marketplace/.plugins/<mp>/<plugin>` 内**
/// 才删（`remove_plugin_install_path`）；共享 catalog clone 内的相对源 installPath 保留（归 marketplace
/// remove/prune，见 #271）。`bundledMcpServers` 汇总后经 `mcp_teardown` 回调停/摘（真正接线由 computer.py
/// 集成承担，本模块只触发回调）。返回实际清理的 plugin id 列表。
pub async fn gc_plugins(
    plugin_ids: &[String],
    registry: &mut SkillRegistry,
    home: &Path,
    store: &dyn SkillGovernanceStore,
    non_plugin_bundle_ids: &HashSet<BundleId>,
    mcp_teardown: Option<&dyn McpTeardown>,
) -> Vec<String> {
    let installed = store.load_installed_plugins();
    let mut removed: Vec<String> = Vec::new();
    for pid in plugin_ids {
        let Some(records) = installed.plugins.get(pid) else {
            continue;
        };
        // pid = "<plugin>@<mp>"；无 '@' → marketplace 空（无法识别独占 clone，installPath 保守保留）。
        let (plugin, marketplace) = pid.split_once('@').unwrap_or((pid.as_str(), ""));
        // 停摘自足：依赖名单在账本记录移除**之前**取得，且仅依赖账本自身字段——与 installPath 树是否删除无关
        // （#271 后共享 catalog 内的树不再被本函数删除）。
        let mut deps: Vec<BundleId> = Vec::new();
        for rec in records {
            if let Some(install_path) = rec.install_path.as_deref().filter(|s| !s.is_empty()) {
                // #271：只删 plugin 独占外部 clone；共享 catalog 归 marketplace 级清理。
                remove_plugin_install_path(Path::new(install_path), home, marketplace, plugin);
            }
            for id in &rec.mcp_servers {
                if !deps.contains(id) {
                    deps.push(id.clone());
                }
            }
        }
        // #139/§4.9.1-2 回收判据：无其他 plugin 依赖 ∧ 非用户声明（`non_plugin_bundle_ids` 已含 durable/
        // flag/embed 全非-plugin 路径）。用户/宿主自有 server 永不连坐。
        let other = other_plugin_mcp_deps(&installed, pid);
        let reclaim = reclaimable_mcp_deps(&deps, &other, non_plugin_bundle_ids);

        // 无 '@' → marketplace 空（不注销）。
        if !marketplace.is_empty() {
            unregister_marketplace_skills(registry, marketplace, Some(plugin));
        }

        if let Some(teardown) = mcp_teardown {
            if !reclaim.is_empty() {
                teardown.teardown(reclaim.clone()).await;
            }
        }

        let drop_pid = pid.clone();
        store.update_installed_plugins(&mut |data| {
            data.plugins.shift_remove(&drop_pid);
        });
        removed.push(pid.clone());
        tracing::info!(plugin = %pid, deps = ?deps, reclaimed = ?reclaim, "gc: removed orphan plugin");
    }
    removed
}

#[cfg(test)]
mod recycle_criterion_tests_139 {
    use super::*;

    fn bid(s: &str) -> BundleId {
        BundleId::try_from(s.to_string()).unwrap()
    }
    fn rec_with(ids: &[&str]) -> InstalledPluginRecord {
        InstalledPluginRecord {
            install_path: Some("/p".to_string()),
            mcp_servers: ids.iter().map(|s| bid(s)).collect(),
            extra: Map::new(),
        }
    }

    /// **F1 判据形状**：ledger 序列化为**纯 bundle_id 数组**——无 provenance、无 name、字段名 `mcpServers`。
    /// 与 python-sdk 磁盘格式逐字节一致。
    #[test]
    fn ledger_serializes_mcp_servers_as_pure_bundle_id_array_139() {
        let rec = rec_with(&["acme-audit", "shared-fs"]);
        let v = serde_json::to_value(&rec).unwrap();
        assert_eq!(
            v["mcpServers"],
            serde_json::json!(["acme-audit", "shared-fs"]),
            "F1：纯 bundle_id 数组"
        );
        assert!(
            v.get("bundledMcpServers").is_none(),
            "旧字段名 MUST NOT 出现（检测即丢弃，无 name→id 映射）"
        );
        // 无 provenance / 无 name：数组元素是裸字符串，不是对象。
        assert!(v["mcpServers"]
            .as_array()
            .unwrap()
            .iter()
            .all(Value::is_string));
    }

    /// **§4.9.1-2 四景**（Discussion #32）：回收 X ⟺ 无其他 plugin 依赖 X ∧ X 非用户声明。
    /// ① 无人依赖 ∧ 非用户声明 → 回收；② 其他 plugin 仍依赖 → 保留；③④ 用户/宿主声明（durable/flag/embed，
    /// 含碰撞景：同 bundle_id 既被 plugin 声明又被非-plugin 路径声明）→ **永不连坐**。
    #[test]
    fn reclaimable_criterion_four_scenarios_139() {
        let (x, y, z) = (bid("x"), bid("y"), bid("z"));
        let deps = vec![x.clone(), y.clone(), z.clone()];
        let other: HashSet<BundleId> = [y].into_iter().collect(); // ② B 仍依赖 y
        let non_plugin: HashSet<BundleId> = [z].into_iter().collect(); // ③④ 用户/宿主声明 z
        assert_eq!(
            reclaimable_mcp_deps(&deps, &other, &non_plugin),
            vec![x],
            "只回收「无其他 plugin 依赖 ∧ 非用户声明」者"
        );
    }

    /// **回归（#139 验收）**：A 引入 X → B 依赖 X → 卸载 A（**X 保留**）→ 卸载 B（**X 回收，不泄漏**）。
    #[test]
    fn ab_handoff_keeps_then_reclaims_shared_dep_139() {
        let x = bid("x");
        let mut installed = InstalledPlugins::default();
        installed
            .plugins
            .insert("a@mp".into(), vec![rec_with(&["x"])]);
        installed
            .plugins
            .insert("b@mp".into(), vec![rec_with(&["x"])]);

        // 卸载 A：B 仍声明依赖 X ⇒ 不回收（X 保留，B 不被打断）。
        let other_a = other_plugin_mcp_deps(&installed, "a@mp");
        assert!(other_a.contains(&x));
        assert!(
            reclaimable_mcp_deps(std::slice::from_ref(&x), &other_a, &HashSet::new()).is_empty(),
            "卸载 A 时 X MUST 保留（B 仍依赖）"
        );

        // A 记录移除后卸载 B：无人再依赖 ⇒ 回收（不泄漏成僵尸）。
        installed.plugins.shift_remove("a@mp");
        let other_b = other_plugin_mcp_deps(&installed, "b@mp");
        assert_eq!(
            reclaimable_mcp_deps(std::slice::from_ref(&x), &other_b, &HashSet::new()),
            vec![x],
            "卸载 B 时 X MUST 被回收（无人再依赖，不泄漏）"
        );
    }

    /// **永不连坐**：X 被 plugin 声明，同时经**非-plugin 路径**（user/project/local durable、flag `--mcp-config`、
    /// embed 宿主构造入参 —— 三者在 resolve 输出中皆 `origin != plugin`）声明 ⇒ 卸载该 plugin 时 X **不回收**。
    /// 这是 Discussion #32 裁决「判据锚 origin 权威集」的核心可观测后果（#147 让 embed 进入该集）。
    #[test]
    fn non_plugin_declared_never_collateral_including_embed_139() {
        let x = bid("x");
        // resolve 输出中 origin != plugin 的 bundle_id 集（durable / flag / embed 同源同域）。
        let non_plugin: HashSet<BundleId> = [x.clone()].into_iter().collect();
        assert!(
            reclaimable_mcp_deps(&[x], &HashSet::new(), &non_plugin).is_empty(),
            "碰撞景：origin != plugin ⇒ MUST NOT 回收（用户/宿主自有 server 永不连坐）"
        );
    }

    /// **旧格式检测即丢弃**：磁盘上的 `bundledMcpServers`（name 数组）反序列化后 **MUST NOT** 进入
    /// `mcp_servers`（无 name→id 映射）；它落进 `extra`，由 `discard_legacy_bundled_mcp_servers` 显式丢弃。
    #[test]
    fn legacy_bundled_name_array_is_not_mapped_into_mcp_servers_139() {
        let legacy = serde_json::json!({
            "installPath": "/p",
            "bundledMcpServers": ["Legacy.Name", "another"],
            "scope": "user"
        });
        let rec: InstalledPluginRecord = serde_json::from_value(legacy).unwrap();
        assert!(
            rec.mcp_servers.is_empty(),
            "旧 name 数组 MUST NOT 被映射成 bundle_id（丢弃 + 从意图重建）"
        );
        // 旧键落进 extra（故需 `discard_legacy_bundled_mcp_servers` 显式清理，否则会原样回写）。
        assert!(rec.extra.contains_key("bundledMcpServers"));
    }

    /// `other_plugin_mcp_deps` MUST 排除自身（否则自己的声明会让自己永不可回收 = 泄漏）。
    #[test]
    fn other_plugin_deps_excludes_self_139() {
        let mut installed = InstalledPlugins::default();
        installed
            .plugins
            .insert("a@mp".into(), vec![rec_with(&["only-mine"])]);
        assert!(
            other_plugin_mcp_deps(&installed, "a@mp").is_empty(),
            "排除自身 ⇒ 独占依赖可回收"
        );
    }

    /// **#284（mirror python#262）**：scoped uninstall 的同 pid retained 记录仍声明依赖 ⇒ 计入「仍有依赖者」，
    /// X 不可回收；retained 未声明 X ⇒ 照常回收（判据仍是「无任何剩余记录声明 ∧ 非用户声明」）。
    #[test]
    fn retained_records_are_dependents_284() {
        let x = bid("x");
        let mut installed = InstalledPlugins::default();
        installed
            .plugins
            .insert("a@mp".into(), vec![rec_with(&["x"])]);

        // retained（同 pid 另一 scope）仍声明 x → 不可回收。
        let retained = vec![rec_with(&["x"])];
        let other = other_plugin_mcp_deps_with_retained(&installed, "a@mp", &retained);
        assert!(other.contains(&x), "retained 记录仍声明依赖 ⇒ 计入依赖者");
        assert!(
            reclaimable_mcp_deps(std::slice::from_ref(&x), &other, &HashSet::new()).is_empty(),
            "retained 仍依赖 ⇒ X MUST 保留"
        );

        // retained 未声明 x → 无人再依赖，照常回收。
        let retained_without = vec![rec_with(&[])];
        let other = other_plugin_mcp_deps_with_retained(&installed, "a@mp", &retained_without);
        assert_eq!(
            reclaimable_mcp_deps(std::slice::from_ref(&x), &other, &HashSet::new()),
            vec![x],
            "无任何剩余记录声明 ⇒ X MUST 回收（无泄漏）"
        );
    }

    /// 2 参 `other_plugin_mcp_deps` 等价于 retained 空 slice——整 pid 移除的既有语义不变（#284 additive）。
    #[test]
    fn two_arg_helper_equals_empty_retained_284() {
        let mut installed = InstalledPlugins::default();
        installed
            .plugins
            .insert("a@mp".into(), vec![rec_with(&["x"])]);
        installed
            .plugins
            .insert("b@mp".into(), vec![rec_with(&["y"])]);
        assert_eq!(
            other_plugin_mcp_deps(&installed, "a@mp"),
            other_plugin_mcp_deps_with_retained(&installed, "a@mp", &[]),
            "2 参入口 MUST 委托空 retained"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::staging::KnownMarketplaceRecord;
    use smcp::A2CSkillRef;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;
    use tracing::instrument::WithSubscriber;

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
        async fn teardown(&self, servers: Vec<BundleId>) {
            self.torn
                .lock()
                .unwrap()
                .extend(servers.iter().map(|b| b.as_str().to_string()));
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
            tags: None,
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

    /// #271：删除谓词边界——只有独占外部 clone（含 `git-subdir` 后代）才删；共享 catalog / 兄弟目录 /
    /// 非法段一律保留。
    #[test]
    fn remove_plugin_install_path_boundaries_271() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let ext_root = home.join("marketplace/.plugins/acme/audit");
        let catalog = home.join("marketplace/acme/plugins/audit");
        let elsewhere = home.join("elsewhere");
        for d in [&ext_root, &catalog, &elsewhere] {
            std::fs::create_dir_all(d).unwrap();
        }
        let ext_sub = ext_root.join("pkg/sub");
        std::fs::create_dir_all(&ext_sub).unwrap();
        let sibling = home.join("marketplace/.plugins/acme/audit2");
        std::fs::create_dir_all(&sibling).unwrap();

        // 保留：共享 catalog / 无关路径 / 兄弟 plugin / 空与非法段 / 空路径。
        assert!(!remove_plugin_install_path(
            &catalog, &home, "acme", "audit"
        ));
        assert!(catalog.exists());
        assert!(!remove_plugin_install_path(
            &elsewhere, &home, "acme", "audit"
        ));
        assert!(!remove_plugin_install_path(
            &sibling, &home, "acme", "audit"
        ));
        assert!(sibling.exists());
        assert!(!remove_plugin_install_path(
            Path::new(""),
            &home,
            "acme",
            "audit"
        ));
        assert!(!remove_plugin_install_path(&ext_root, &home, "", "audit"));
        assert!(!remove_plugin_install_path(&ext_root, &home, "acme", ".."));
        assert!(ext_root.exists());
        // `..` 穿越：归一后落在 ext root 外 → 保留（词法归一不放宽删除边界）。
        let traversal = ext_root.join("../../acme/plugins/audit");
        assert!(!remove_plugin_install_path(
            &traversal, &home, "acme", "audit"
        ));

        // 删除：ext 子目录 → 删整个独占 clone 根；共享 catalog 不受影响。
        assert!(remove_plugin_install_path(&ext_sub, &home, "acme", "audit"));
        assert!(!ext_root.exists(), "subdir installPath → 删除独占 clone 根");
        assert!(catalog.exists());

        // 删除：ext 根本身（重建后）。
        std::fs::create_dir_all(&ext_root).unwrap();
        assert!(remove_plugin_install_path(
            &ext_root, &home, "acme", "audit"
        ));
        assert!(!ext_root.exists());

        // 缺失路径：词法命中，safe_rmtree no-op、不 panic。
        assert!(remove_plugin_install_path(
            &ext_root, &home, "acme", "audit"
        ));
    }

    /// #271：ext root 被植入指向 Home 外的 symlink 时，`safe_rmtree` 的 canonicalize 守卫拒删，目标存活。
    #[cfg(unix)]
    #[test]
    fn remove_plugin_install_path_refuses_symlinked_ext_root_escape_271() {
        use std::os::unix::fs::symlink;
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keep.txt"), "keep").unwrap();
        let ext_root = home.join("marketplace/.plugins/acme/audit");
        std::fs::create_dir_all(ext_root.parent().unwrap()).unwrap();
        symlink(&outside, &ext_root).unwrap();

        let _ = remove_plugin_install_path(&ext_root, &home, "acme", "audit");
        assert!(
            outside.join("keep.txt").exists(),
            "symlink 目标（Home 外）MUST NOT 被删除"
        );
        assert!(ext_root.exists(), "symlink 本身保留（拒删越界路径）");
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
        // 独占外部 clone 的正确布局：`.plugins/<mp>/<plugin>`（#271 后按此边界判定删除）。
        let install_path = home
            .join("marketplace")
            .join(".plugins")
            .join("acme")
            .join("audit");
        std::fs::create_dir_all(&install_path).unwrap();

        let store = MemStore::default();
        store.update_installed_plugins(&mut |data| {
            data.plugins.insert(
                "audit@acme".to_string(),
                vec![InstalledPluginRecord {
                    install_path: Some(install_path.to_string_lossy().into_owned()),
                    mcp_servers: vec![BundleId::try_from("audit-mcp".to_string()).unwrap()],
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
            &HashSet::new(),
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
        make_marketplace_repo_with_plugin(root, "audit", url_skill_desc)
    }

    /// 指定 plugin 名的 marketplace 仓库（多 marketplace 用例避免跨源同名 SKILL 冲突）。
    fn make_marketplace_repo_with_plugin(root: &Path, plugin: &str, skill_desc: &str) -> PathBuf {
        let repo = root.join("repo");
        std::fs::create_dir_all(repo.join(".tfrobot-plugin")).unwrap();
        std::fs::write(
            repo.join(".tfrobot-plugin/marketplace.json"),
            format!(r#"{{"plugins": [{{"name": "{plugin}", "source": "./plugins/{plugin}"}}]}}"#),
        )
        .unwrap();
        let skill = repo.join(format!("plugins/{plugin}/skills/code-review"));
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            format!("---\nname: code-review\ndescription: {skill_desc}\n---\nbody"),
        )
        .unwrap();
        git(&["init", "-q"], &repo);
        git(&["add", "-A"], &repo);
        git(&["commit", "-qm", "init"], &repo);
        repo
    }

    /// 同源上游更新：改写 plugin 的 SKILL 描述并提交（`refresh`/`autoUpdate` 收敛用）。
    fn commit_skill_desc(repo: &Path, plugin: &str, skill_desc: &str) {
        let skill = repo.join(format!("plugins/{plugin}/skills/code-review/SKILL.md"));
        std::fs::write(
            skill,
            format!("---\nname: code-review\ndescription: {skill_desc}\n---\nbody"),
        )
        .unwrap();
        git(&["add", "-A"], repo);
        git(&["commit", "-qm", "update"], repo);
    }

    // ---- 日志捕获（结构化诊断断言）/ captured logs ---------------------------
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

    /// `(捕获器, subscriber)`：`fut.with_subscriber(sub).await` 后读 [`CapturedLogs::contents`]。
    fn capture_subscriber() -> (
        CapturedLogs,
        impl tracing::Subscriber + Send + Sync + 'static,
    ) {
        let logs = CapturedLogs::default();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_writer(logs.clone())
            .finish();
        (logs, subscriber)
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
    async fn reconcile_source_changed_rejects_and_preserves_old_registration() {
        let tmp = TempDir::new().unwrap();
        let repo_a = make_marketplace_repo(&tmp.path().join("a"), "from A");
        let repo_b = make_marketplace_repo(&tmp.path().join("b"), "from B");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let url_a = format!("file://{}", repo_a.display());
        let url_b = format!("file://{}", repo_b.display());
        let store = MemStore::default();
        let mut reg = SkillRegistry::new();

        let d_a = declared(serde_json::json!({
            "extraKnownMarketplaces": {"acme": {"source": {"type": "git", "url": url_a}}},
            "enabledPlugins": {"audit@acme": true}
        }));
        let r1 = reconcile(&mut reg, &home, &d_a, &store, ReconcileOptions::default()).await;
        assert_eq!(r1.installed, vec!["acme".to_string()]);

        // 旧态哨兵：catalog clone 树、外部 plugin 树、该源 orphan SKILL。
        let clone_dir = marketplace_skill_dir(&home, "acme", &[]);
        let skill_md = clone_dir.join("plugins/audit/skills/code-review/SKILL.md");
        assert!(std::fs::read_to_string(&skill_md)
            .unwrap()
            .contains("from A"));
        std::fs::write(clone_dir.join("SENTINEL-CLONE"), b"keep").unwrap();
        let ext_dir = home
            .join(SOURCE_MARKETPLACE)
            .join(EXTERNAL_PLUGINS_NS)
            .join("acme");
        std::fs::create_dir_all(&ext_dir).unwrap();
        std::fs::write(ext_dir.join("SENTINEL-EXT"), b"keep").unwrap();
        reg.register(mp_ref("legacy:old", "acme", &clone_dir));

        // 声明面换源（同名不同源）→ §5.14 拒绝；旧注册 / 旧内容 / 旧能力保持不变。
        let d_b = declared(serde_json::json!({
            "extraKnownMarketplaces": {"acme": {"source": {"type": "git", "url": url_b}}},
            "enabledPlugins": {"audit@acme": true}
        }));
        let r2 = reconcile(&mut reg, &home, &d_b, &store, ReconcileOptions::default()).await;
        assert_eq!(r2.failed, vec!["acme".to_string()]);
        assert!(
            r2.installed.is_empty() && r2.updated.is_empty() && r2.up_to_date.is_empty(),
            "拒绝不得落入其它桶：{r2:?}"
        );
        assert!(r2.skipped.is_empty(), "拒绝不得占用 trust 预留桶：{r2:?}");
        assert!(r2.registered_skills.is_empty());

        // 旧能力：registry 旧 SKILL（含 orphan）仍在，描述仍是旧源内容。
        assert_eq!(
            reg.resolve("audit:code-review").unwrap().description,
            "from A"
        );
        assert!(reg.contains("legacy:old"), "旧源 orphan 不得被注销");
        // 旧内容：clone 树未被 wipe / 无新源内容；外部 plugin 树未被删。
        assert!(clone_dir.join("SENTINEL-CLONE").exists());
        assert!(ext_dir.join("SENTINEL-EXT").exists());
        assert!(std::fs::read_to_string(&skill_md)
            .unwrap()
            .contains("from A"));
        // 旧注册：账本 source 仍是 A（未被 recorder 静默改写）。
        assert_eq!(
            store.load_known_marketplaces().marketplaces["acme"].source,
            serde_json::json!({"type": "git", "url": url_a})
        );
    }

    #[tokio::test]
    async fn reconcile_source_conflict_is_idempotent_and_old_source_converges() {
        let tmp = TempDir::new().unwrap();
        let repo_a = make_marketplace_repo(&tmp.path().join("a"), "from A");
        let repo_b = make_marketplace_repo(&tmp.path().join("b"), "from B");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let url_a = format!("file://{}", repo_a.display());
        let url_b = format!("file://{}", repo_b.display());
        let store = MemStore::default();
        let mut reg = SkillRegistry::new();

        let d_a = declared(serde_json::json!({
            "extraKnownMarketplaces": {"acme": {"source": {"type": "git", "url": url_a}}},
            "enabledPlugins": {"audit@acme": true}
        }));
        let d_b = declared(serde_json::json!({
            "extraKnownMarketplaces": {"acme": {"source": {"type": "git", "url": url_b}}},
            "enabledPlugins": {"audit@acme": true}
        }));
        assert_eq!(
            reconcile(&mut reg, &home, &d_a, &store, ReconcileOptions::default())
                .await
                .installed,
            vec!["acme".to_string()]
        );

        // 连续两次声明 B 都拒绝（确定性、不污染后续）。
        for round in 0..2 {
            let r = reconcile(&mut reg, &home, &d_b, &store, ReconcileOptions::default()).await;
            assert_eq!(r.failed, vec!["acme".to_string()], "round {round}");
            assert!(r.updated.is_empty() && r.up_to_date.is_empty());
        }

        // 回到旧源 A → 正常收敛（up_to_date），旧注册未受损。
        let r3 = reconcile(&mut reg, &home, &d_a, &store, ReconcileOptions::default()).await;
        assert_eq!(r3.up_to_date, vec!["acme".to_string()]);
        assert!(r3.failed.is_empty());
        assert_eq!(
            reg.resolve("audit:code-review").unwrap().description,
            "from A"
        );
    }

    #[tokio::test]
    async fn reconcile_source_conflict_wins_over_refresh_and_never_pulls() {
        let tmp = TempDir::new().unwrap();
        let repo_a = make_marketplace_repo(&tmp.path().join("a"), "from A");
        let repo_b = make_marketplace_repo(&tmp.path().join("b"), "from B");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let url_a = format!("file://{}", repo_a.display());
        let url_b = format!("file://{}", repo_b.display());
        let store = MemStore::default();
        let mut reg = SkillRegistry::new();

        let d_a = declared(serde_json::json!({
            "extraKnownMarketplaces": {"acme": {"source": {"type": "git", "url": url_a}}},
            "enabledPlugins": {"audit@acme": true}
        }));
        assert_eq!(
            reconcile(&mut reg, &home, &d_a, &store, ReconcileOptions::default())
                .await
                .installed,
            vec!["acme".to_string()]
        );

        // 旧源上游有新内容：拒绝优先，既不得换到 B，也不得对旧 clone 执行 pull。
        commit_skill_desc(&repo_a, "audit", "from A v2");
        let d_b = declared(serde_json::json!({
            "extraKnownMarketplaces": {"acme": {"source": {"type": "git", "url": url_b}}},
            "enabledPlugins": {"audit@acme": true}
        }));
        let r = reconcile(
            &mut reg,
            &home,
            &d_b,
            &store,
            ReconcileOptions {
                refresh: true,
                ..Default::default()
            },
        )
        .await;
        assert_eq!(r.failed, vec!["acme".to_string()]);
        assert!(r.updated.is_empty() && r.registered_skills.is_empty());
        let skill_md = marketplace_skill_dir(&home, "acme", &[])
            .join("plugins/audit/skills/code-review/SKILL.md");
        let text = std::fs::read_to_string(&skill_md).unwrap();
        assert!(
            text.contains("from A") && !text.contains("from A v2"),
            "拒绝不得触发 pull：{text}"
        );
        assert_eq!(
            reg.resolve("audit:code-review").unwrap().description,
            "from A"
        );
        assert_eq!(
            store.load_known_marketplaces().marketplaces["acme"].source,
            serde_json::json!({"type": "git", "url": url_a})
        );
    }

    #[tokio::test]
    async fn reconcile_source_conflict_fail_closed_on_malformed_declared_source() {
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
        assert_eq!(
            reconcile(&mut reg, &home, &d_a, &store, ReconcileOptions::default())
                .await
                .installed,
            vec!["acme".to_string()]
        );

        // 声明缺失 / null / 非对象 → 无法证明同源 → fail-closed 拒绝，旧态保持。
        for decl in [
            serde_json::json!({}),
            serde_json::json!({"source": null}),
            serde_json::json!({"source": "not-an-object"}),
            serde_json::json!({"source": ["x"]}),
        ] {
            let mut mps = Map::new();
            mps.insert("acme".to_string(), decl.clone());
            let mut raw = Map::new();
            raw.insert("extraKnownMarketplaces".to_string(), Value::Object(mps));
            raw.insert(
                "enabledPlugins".to_string(),
                serde_json::json!({"audit@acme": true}),
            );
            let d = declared(Value::Object(raw));

            let r = reconcile(&mut reg, &home, &d, &store, ReconcileOptions::default()).await;
            assert_eq!(r.failed, vec!["acme".to_string()], "decl={decl}");
            assert!(
                r.installed.is_empty() && r.updated.is_empty() && r.up_to_date.is_empty(),
                "decl={decl}"
            );
            assert_eq!(
                reg.resolve("audit:code-review").unwrap().description,
                "from A",
                "decl={decl}"
            );
            assert_eq!(
                store.load_known_marketplaces().marketplaces["acme"].source,
                serde_json::json!({"type": "git", "url": url_a}),
                "decl={decl}"
            );
        }
    }

    #[tokio::test]
    async fn reconcile_source_conflict_fail_closed_on_unrecorded_registered_source() {
        // 账本记录存在但 source 缺失 / 空对象（无法证明同源）→ fail-closed 拒绝，旧物化保持。
        for ledger_source in [serde_json::json!(null), serde_json::json!({})] {
            let tmp = TempDir::new().unwrap();
            let home = tmp.path().join("home");
            std::fs::create_dir_all(&home).unwrap();
            let store = MemStore::default();
            let mut reg = SkillRegistry::new();

            let clone_dir = marketplace_skill_dir(&home, "acme", &[]);
            std::fs::create_dir_all(&clone_dir).unwrap();
            std::fs::write(clone_dir.join("SENTINEL-CLONE"), b"keep").unwrap();
            reg.register(mp_ref("audit:code-review", "acme", &clone_dir));
            store.record(KnownMarketplaceRecord {
                name: "acme",
                source: &ledger_source,
                install_location: &clone_dir,
                commit_sha: Some("deadbeef"),
                auto_update: false,
                changed: true,
            });

            let repo = make_marketplace_repo(&tmp.path().join("a"), "from A");
            let url_a = format!("file://{}", repo.display());
            let d = declared(serde_json::json!({
                "extraKnownMarketplaces": {"acme": {"source": {"type": "git", "url": url_a}}},
                "enabledPlugins": {"audit@acme": true}
            }));
            let r = reconcile(&mut reg, &home, &d, &store, ReconcileOptions::default()).await;
            assert_eq!(r.failed, vec!["acme".to_string()], "ledger={ledger_source}");
            assert!(
                r.installed.is_empty() && r.updated.is_empty() && r.up_to_date.is_empty(),
                "ledger={ledger_source}"
            );
            assert!(clone_dir.join("SENTINEL-CLONE").exists());
            assert!(reg.contains("audit:code-review"));
            assert_eq!(
                store.load_known_marketplaces().marketplaces["acme"].source,
                ledger_source
            );
        }
    }

    #[tokio::test]
    async fn reconcile_source_conflict_fail_closed_on_equal_unusable_sources() {
        // 账本与声明「同形但都不可用」：无法证明同源 → fail-closed 拒绝，不得因 `==` 放行后把空转
        // stage 误报成 up_to_date / updated。含对象形态（非 git / 非法 url）以钉死 marketplace_clone_url
        // 谓词——防实现被弱化为 `!source.is_object()` 后本用例仍绿。
        for unusable in [
            serde_json::json!(null),
            serde_json::json!({}),
            serde_json::json!({"type": "github", "url": "owner/repo"}),
            serde_json::json!({"type": "git", "url": "bogus"}),
        ] {
            let tmp = TempDir::new().unwrap();
            let home = tmp.path().join("home");
            std::fs::create_dir_all(&home).unwrap();
            let store = MemStore::default();
            let mut reg = SkillRegistry::new();

            let clone_dir = marketplace_skill_dir(&home, "acme", &[]);
            std::fs::create_dir_all(&clone_dir).unwrap();
            std::fs::write(clone_dir.join("SENTINEL-CLONE"), b"keep").unwrap();
            reg.register(mp_ref("audit:code-review", "acme", &clone_dir));
            store.record(KnownMarketplaceRecord {
                name: "acme",
                source: &unusable,
                install_location: &clone_dir,
                commit_sha: Some("deadbeef"),
                auto_update: false,
                changed: true,
            });

            let d = declared(serde_json::json!({
                "extraKnownMarketplaces": {"acme": {"source": unusable}},
                "enabledPlugins": {"audit@acme": true}
            }));
            let (logs, subscriber) = capture_subscriber();
            let r = reconcile(&mut reg, &home, &d, &store, ReconcileOptions::default())
                .with_subscriber(subscriber)
                .await;
            assert_eq!(r.failed, vec!["acme".to_string()], "source={unusable}");
            assert!(
                r.installed.is_empty() && r.updated.is_empty() && r.up_to_date.is_empty(),
                "不得把空转 stage 报成成功：{r:?}"
            );
            assert!(r.skipped.is_empty());
            assert!(r.registered_skills.is_empty());
            assert!(clone_dir.join("SENTINEL-CLONE").exists());
            assert!(reg.contains("audit:code-review"));
            // 账本 source 不得被改写（仍与声明同形的不可用值）。
            assert_eq!(
                store.load_known_marketplaces().marketplaces["acme"].source,
                unusable
            );
            let rendered = logs.contents();
            assert!(
                rendered.contains("marketplace_source_unusable"),
                "reason 应可区分不可用 source：{rendered}"
            );
        }
    }

    #[tokio::test]
    async fn reconcile_source_conflict_with_empty_registry_does_not_stage() {
        // 前置契约：拒绝分支不 stage——空 Registry 调用时旧 SKILL 不会出现（调用方须先跑 recovery 或
        // 传入既有 Registry），但账本与旧 clone 必须原样保持、不得凭空 clone 新源。
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let store = MemStore::default();
        let mut reg = SkillRegistry::new(); // 空 Registry（模拟启动对账未先跑 recovery）

        let clone_dir = marketplace_skill_dir(&home, "acme", &[]);
        std::fs::create_dir_all(&clone_dir).unwrap();
        std::fs::write(clone_dir.join("SENTINEL-CLONE"), b"keep").unwrap();
        let source_a = serde_json::json!({"type": "git", "url": "file:///tmp/does-not-matter-a"});
        store.record(KnownMarketplaceRecord {
            name: "acme",
            source: &source_a,
            install_location: &clone_dir,
            commit_sha: Some("deadbeef"),
            auto_update: false,
            changed: true,
        });

        let d = declared(serde_json::json!({
            "extraKnownMarketplaces": {"acme": {"source": {
                "type": "git", "url": "file:///tmp/does-not-matter-b"
            }}},
            "enabledPlugins": {"audit@acme": true}
        }));
        let r = reconcile(&mut reg, &home, &d, &store, ReconcileOptions::default()).await;

        assert_eq!(r.failed, vec!["acme".to_string()]);
        assert!(
            r.installed.is_empty() && r.updated.is_empty() && r.up_to_date.is_empty(),
            "拒绝分支不得 stage：{r:?}"
        );
        assert!(r.registered_skills.is_empty());
        assert!(reg.is_empty(), "空 Registry 应保持为空（拒绝分支不注册）");
        assert!(clone_dir.join("SENTINEL-CLONE").exists());
        assert_eq!(
            store.load_known_marketplaces().marketplaces["acme"].source,
            source_a
        );
    }

    #[tokio::test]
    async fn reconcile_source_conflict_wins_over_auto_update_and_never_pulls() {
        // 声明级 autoUpdate=true 同样不得绕过 §5.14 拒绝（守卫先于 do_refresh 计算）。
        let tmp = TempDir::new().unwrap();
        let repo_a = make_marketplace_repo(&tmp.path().join("a"), "from A");
        let repo_b = make_marketplace_repo(&tmp.path().join("b"), "from B");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let url_a = format!("file://{}", repo_a.display());
        let url_b = format!("file://{}", repo_b.display());
        let store = MemStore::default();
        let mut reg = SkillRegistry::new();

        let d_a = declared(serde_json::json!({
            "extraKnownMarketplaces": {"acme": {"source": {"type": "git", "url": url_a}}},
            "enabledPlugins": {"audit@acme": true}
        }));
        assert_eq!(
            reconcile(&mut reg, &home, &d_a, &store, ReconcileOptions::default())
                .await
                .installed,
            vec!["acme".to_string()]
        );

        commit_skill_desc(&repo_a, "audit", "from A v2");
        let d_b = declared(serde_json::json!({
            "extraKnownMarketplaces": {"acme": {
                "source": {"type": "git", "url": url_b},
                "autoUpdate": true
            }},
            "enabledPlugins": {"audit@acme": true}
        }));
        let r = reconcile(&mut reg, &home, &d_b, &store, ReconcileOptions::default()).await;
        assert_eq!(r.failed, vec!["acme".to_string()]);
        assert!(r.updated.is_empty() && r.registered_skills.is_empty());
        let skill_md = marketplace_skill_dir(&home, "acme", &[])
            .join("plugins/audit/skills/code-review/SKILL.md");
        let text = std::fs::read_to_string(&skill_md).unwrap();
        assert!(
            text.contains("from A") && !text.contains("from A v2"),
            "autoUpdate 也不得触发 pull：{text}"
        );
        assert_eq!(
            store.load_known_marketplaces().marketplaces["acme"].source,
            serde_json::json!({"type": "git", "url": url_a})
        );
    }

    #[tokio::test]
    async fn reconcile_source_conflict_compares_whole_source_shape() {
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
        assert_eq!(
            reconcile(&mut reg, &home, &d_a, &store, ReconcileOptions::default())
                .await
                .installed,
            vec!["acme".to_string()]
        );

        // 同 url 但形状不同（多余 ref 键）：无法判定同源 → fail-closed 拒绝（归一化归 #272）。
        let d = declared(serde_json::json!({
            "extraKnownMarketplaces": {"acme": {"source": {
                "type": "git", "url": url_a, "ref": "main"
            }}},
            "enabledPlugins": {"audit@acme": true}
        }));
        let r = reconcile(&mut reg, &home, &d, &store, ReconcileOptions::default()).await;
        assert_eq!(r.failed, vec!["acme".to_string()]);
        assert!(r.updated.is_empty() && r.up_to_date.is_empty());
        assert_eq!(
            store.load_known_marketplaces().marketplaces["acme"].source,
            serde_json::json!({"type": "git", "url": url_a})
        );
    }

    #[tokio::test]
    async fn reconcile_source_conflict_logs_stable_reason_and_redacts_credentials() {
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
        assert_eq!(
            reconcile(&mut reg, &home, &d_a, &store, ReconcileOptions::default())
                .await
                .installed,
            vec!["acme".to_string()]
        );

        // 声明面带凭据 URL：日志必须给稳定 reason 且脱敏（归 reject 前置，不发网络请求）。
        // 用不会出现在临时路径 / 系统用户名中的唯一 token，避免 `!contains` 断言误报。
        let secret_url = "https://u294tok:SEKRETQW7@example.com/acme/repo.git";
        let d = declared(serde_json::json!({
            "extraKnownMarketplaces": {"acme": {"source": {"type": "git", "url": secret_url}}},
            "enabledPlugins": {"audit@acme": true}
        }));
        let (logs, subscriber) = capture_subscriber();
        let r = reconcile(&mut reg, &home, &d, &store, ReconcileOptions::default())
            .with_subscriber(subscriber)
            .await;
        assert_eq!(r.failed, vec!["acme".to_string()]);

        let rendered = logs.contents();
        assert!(
            rendered.contains("marketplace_source_conflict"),
            "{rendered}"
        );
        assert!(
            rendered.contains("https://example.com/acme/repo.git"),
            "{rendered}"
        );
        for secret in ["u294tok", "SEKRETQW7"] {
            assert!(!rendered.contains(secret), "日志泄露 {secret}：{rendered}");
        }
    }

    #[tokio::test]
    async fn reconcile_after_explicit_prune_installs_new_source() {
        let tmp = TempDir::new().unwrap();
        let repo_a = make_marketplace_repo(&tmp.path().join("a"), "from A");
        let repo_b = make_marketplace_repo(&tmp.path().join("b"), "from B");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let url_a = format!("file://{}", repo_a.display());
        let url_b = format!("file://{}", repo_b.display());
        let store = MemStore::default();
        let mut reg = SkillRegistry::new();

        let d_a = declared(serde_json::json!({
            "extraKnownMarketplaces": {"acme": {"source": {"type": "git", "url": url_a}}},
            "enabledPlugins": {"audit@acme": true}
        }));
        let d_b = declared(serde_json::json!({
            "extraKnownMarketplaces": {"acme": {"source": {"type": "git", "url": url_b}}},
            "enabledPlugins": {"audit@acme": true}
        }));
        assert_eq!(
            reconcile(&mut reg, &home, &d_a, &store, ReconcileOptions::default())
                .await
                .installed,
            vec!["acme".to_string()]
        );
        assert_eq!(
            reconcile(&mut reg, &home, &d_b, &store, ReconcileOptions::default())
                .await
                .failed,
            vec!["acme".to_string()]
        );

        // 显式 remove（prune）后同名不同源 re-add：新注册成立，内容来自新源。
        let removed = prune_marketplaces(&["acme".to_string()], &mut reg, &home, &store);
        assert_eq!(removed, vec!["acme".to_string()]);
        assert!(!reg.contains("audit:code-review"));
        assert!(!store
            .load_known_marketplaces()
            .marketplaces
            .contains_key("acme"));

        // §5.14 ④ 的 `<plugin>@<name>` 意图引用门属 #272 的 remove/add 语义；此处不引入引用（无
        // `enabledPlugins`），只钉死 reconcile 层「显式 remove 后同名不同源 re-add 成立」。
        let d_b_unreferenced = declared(serde_json::json!({
            "extraKnownMarketplaces": {"acme": {"source": {"type": "git", "url": url_b}}}
        }));
        let r3 = reconcile(
            &mut reg,
            &home,
            &d_b_unreferenced,
            &store,
            ReconcileOptions::default(),
        )
        .await;
        assert_eq!(r3.installed, vec!["acme".to_string()]);
        assert!(r3.failed.is_empty());
        // 新内容已物化（B 的 SKILL 描述）；本声明未启用 plugin，故不产生注册。
        let skill_md = marketplace_skill_dir(&home, "acme", &[])
            .join("plugins/audit/skills/code-review/SKILL.md");
        assert!(std::fs::read_to_string(&skill_md)
            .unwrap()
            .contains("from B"));
        assert!(r3.registered_skills.is_empty());
        assert_eq!(
            store.load_known_marketplaces().marketplaces["acme"].source,
            serde_json::json!({"type": "git", "url": url_b})
        );
    }

    #[tokio::test]
    async fn reconcile_source_conflict_does_not_abort_other_marketplaces() {
        let tmp = TempDir::new().unwrap();
        let repo_a = make_marketplace_repo(&tmp.path().join("a"), "from A");
        let repo_b = make_marketplace_repo(&tmp.path().join("b"), "from B");
        let repo_beta =
            make_marketplace_repo_with_plugin(&tmp.path().join("beta"), "lint", "beta skill");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let url_a = format!("file://{}", repo_a.display());
        let url_b = format!("file://{}", repo_b.display());
        let url_beta = format!("file://{}", repo_beta.display());
        let store = MemStore::default();
        let mut reg = SkillRegistry::new();

        let d_a = declared(serde_json::json!({
            "extraKnownMarketplaces": {"acme": {"source": {"type": "git", "url": url_a}}},
            "enabledPlugins": {"audit@acme": true}
        }));
        assert_eq!(
            reconcile(&mut reg, &home, &d_a, &store, ReconcileOptions::default())
                .await
                .installed,
            vec!["acme".to_string()]
        );

        // 同轮：acme 换源被拒 + 新 beta 正常安装（continue 不得变 break）。
        let d = declared(serde_json::json!({
            "extraKnownMarketplaces": {
                "acme": {"source": {"type": "git", "url": url_b}},
                "beta": {"source": {"type": "git", "url": url_beta}}
            },
            "enabledPlugins": {"audit@acme": true, "lint@beta": true}
        }));
        let r = reconcile(&mut reg, &home, &d, &store, ReconcileOptions::default()).await;
        assert_eq!(r.failed, vec!["acme".to_string()]);
        assert_eq!(r.installed, vec!["beta".to_string()]);
        assert!(
            reg.contains("lint:code-review"),
            "其它 marketplace 应照常安装"
        );
        assert_eq!(
            reg.resolve("audit:code-review").unwrap().description,
            "from A"
        );
    }

    #[tokio::test]
    async fn reconcile_same_source_refresh_updates_content() {
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
        assert_eq!(
            reconcile(&mut reg, &home, &d_a, &store, ReconcileOptions::default())
                .await
                .installed,
            vec!["acme".to_string()]
        );

        // 同源上游更新 → refresh 正常收敛（不得被 §5.14 拒绝误伤）。
        commit_skill_desc(&repo_a, "audit", "from A v2");
        let r = reconcile(
            &mut reg,
            &home,
            &d_a,
            &store,
            ReconcileOptions {
                refresh: true,
                ..Default::default()
            },
        )
        .await;
        assert_eq!(r.updated, vec!["acme".to_string()]);
        assert!(r.failed.is_empty());
        assert_eq!(
            reg.resolve("audit:code-review").unwrap().description,
            "from A v2"
        );
        assert_eq!(
            store.load_known_marketplaces().marketplaces["acme"].source,
            serde_json::json!({"type": "git", "url": url_a})
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
        // 独占外部 clone 的正确布局：`.plugins/<mp>/<plugin>`（#271 后按此边界判定删除）。
        let install_path = home
            .join("marketplace")
            .join(".plugins")
            .join("acme")
            .join("audit");
        std::fs::create_dir_all(&install_path).unwrap();
        let store = MemStore::default();
        store.update_installed_plugins(&mut |data| {
            data.plugins.insert(
                "audit@acme".to_string(),
                vec![InstalledPluginRecord {
                    install_path: Some(install_path.to_string_lossy().into_owned()),
                    mcp_servers: vec![BundleId::try_from("audit-mcp".to_string()).unwrap()],
                    extra: Map::new(),
                }],
            );
        });
        let mut reg = SkillRegistry::new();
        let removed = gc_plugins(
            &["audit@acme".to_string()],
            &mut reg,
            &home,
            &store,
            &HashSet::new(),
            None,
        )
        .await;
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
            &HashSet::new(),
            None,
        )
        .await;
        assert!(removed.is_empty());
        assert!(store.load_installed_plugins().plugins.is_empty());
    }

    #[tokio::test]
    async fn gc_pid_without_marketplace() {
        // pid 无 '@' → marketplace 空、不尝试注销 → 仍从 store 移除；无 marketplace 段 ⇒ 无法识别独占外部
        // clone，installPath 保守保留（#271：漏删优于误删）。
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let install_path = home
            .join("marketplace")
            .join(".plugins")
            .join("acme")
            .join("audit");
        std::fs::create_dir_all(&install_path).unwrap();
        let store = MemStore::default();
        store.update_installed_plugins(&mut |data| {
            data.plugins.insert(
                "local-only".to_string(),
                vec![InstalledPluginRecord {
                    install_path: Some(install_path.to_string_lossy().into_owned()),
                    mcp_servers: Vec::new(),
                    extra: Map::new(),
                }],
            );
        });
        let mut reg = SkillRegistry::new();
        let removed = gc_plugins(
            &["local-only".to_string()],
            &mut reg,
            &home,
            &store,
            &HashSet::new(),
            None,
        )
        .await;
        assert_eq!(removed, vec!["local-only".to_string()]);
        assert!(!store
            .load_installed_plugins()
            .plugins
            .contains_key("local-only"));
        assert!(
            install_path.exists(),
            "无 marketplace 段 → 无法识别独占外部 clone，installPath 保守保留"
        );
    }

    /// #271：相对源 plugin 的 installPath 在共享 catalog 内 → gc MUST NOT 删除（归 marketplace remove/prune）。
    #[tokio::test]
    async fn gc_relative_source_keeps_shared_catalog_271() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let install_path = home
            .join("marketplace")
            .join("acme")
            .join("plugins")
            .join("audit");
        std::fs::create_dir_all(install_path.join("skills/code-review")).unwrap();
        std::fs::write(install_path.join("skills/code-review/SKILL.md"), "x").unwrap();
        let sibling = install_path.parent().unwrap().join("shared-sentinel.txt");
        std::fs::write(&sibling, "keep").unwrap();

        let store = MemStore::default();
        store.update_installed_plugins(&mut |data| {
            data.plugins.insert(
                "audit@acme".to_string(),
                vec![InstalledPluginRecord {
                    install_path: Some(install_path.to_string_lossy().into_owned()),
                    mcp_servers: Vec::new(),
                    extra: Map::new(),
                }],
            );
        });
        let mut reg = SkillRegistry::new();
        reg.register(mp_ref("audit:code-review", "acme", &home));
        let removed = gc_plugins(
            &["audit@acme".to_string()],
            &mut reg,
            &home,
            &store,
            &HashSet::new(),
            None,
        )
        .await;
        assert_eq!(removed, vec!["audit@acme".to_string()]);
        assert!(
            install_path.exists(),
            "共享 catalog 树 MUST NOT 被 plugin gc 删除（#271 方案 A）"
        );
        assert!(sibling.exists(), "兄弟 plugin 共用文件必须保留");
        assert!(!reg.contains("audit:code-review"));
        assert!(!store
            .load_installed_plugins()
            .plugins
            .contains_key("audit@acme"));
    }

    /// #271：git-subdir 的 installPath 是独占外部 clone 的后代 → 删除整个 clone 根（而非只删子目录）。
    #[tokio::test]
    async fn gc_external_git_subdir_record_removes_clone_root_271() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let ext_root = home
            .join("marketplace")
            .join(".plugins")
            .join("acme")
            .join("audit");
        let sub = ext_root.join("pkg").join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(ext_root.join("root-marker.txt"), "x").unwrap();
        std::fs::write(sub.join("sub-marker.txt"), "x").unwrap();

        let store = MemStore::default();
        store.update_installed_plugins(&mut |data| {
            data.plugins.insert(
                "audit@acme".to_string(),
                vec![InstalledPluginRecord {
                    install_path: Some(sub.to_string_lossy().into_owned()),
                    mcp_servers: Vec::new(),
                    extra: Map::new(),
                }],
            );
        });
        let mut reg = SkillRegistry::new();
        let removed = gc_plugins(
            &["audit@acme".to_string()],
            &mut reg,
            &home,
            &store,
            &HashSet::new(),
            None,
        )
        .await;
        assert_eq!(removed, vec!["audit@acme".to_string()]);
        assert!(
            !ext_root.exists(),
            "git-subdir → 删除整个 plugin 独占外部 clone 根"
        );
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
