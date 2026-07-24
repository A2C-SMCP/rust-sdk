/*!
* 文件名: governance.rs
* 作者: JQQ
* 创建日期: 2026/07/13
* 最后修改日期: 2026/07/13
* 版权: 2023 JQQ. All rights reserved.
* 依赖: serde, serde_json, sha2, thiserror, settings::config::snapshot, settings::store, skills::manifest
* 描述: #124 —— 高层 API-only Marketplace/plugin governance snapshot/inventory（SDK-facing、不进协议 wire）。
*/

//! # 高层治理快照 / High-level governance snapshot（#124）
//!
//! 面向集成 `smcp-computer` 的 GUI/Tauri client（诉求 TFRC-61）：**只经 [`Computer`] 与本模块公开 DTO**
//! 即可查询 Marketplace/plugin 治理状态（可用 / 已安装-禁用 / 已安装-启用 / 降级），**无需** `cli` feature、
//! **无需**理解或读取 `settings::store` / `settings::scope` / `skills::manifest` 的账本 / 意图 / 派生缓存 /
//! scope 合并 / 文件布局。
//!
//! ## 边界（与 [`crate::inventory`]/#97 同定位）
//!
//! - **SDK-facing、不进 Agent-facing `client:*` wire**：治理读投影不加入任何协议事件数据结构。
//! - **只读**：不隐式 clone/refresh、不改 ledger/settings、不挂载能力、不访问网络。
//! - **本实例上下文**：经 [`Computer`] 注入的 `skill_home` / `config_env` / config directory 解析，
//!   绝不回退混入宿主进程 env/home（对齐 [`Computer::list_mcp_servers_with_metadata`]）。
//! - **权威源**：`installedPlugins` 意图为**安装权威**（#102/#104）；派生 ledger 仅供 install_path /
//!   bundled MCP / version 等**详情**——ledger 有记录但不在意图 → **不报 installed**（避免陈旧误判）。
//! - **per-item 韧性**：单个 marketplace catalog / plugin 物化损坏 → 该项 `Degraded` + 结构化 diagnostic，
//!   **不**吞成空列表、**不**影响其他正常项。
//!
//! [`Computer`]: crate::computer::Computer
//! [`Computer::list_mcp_servers_with_metadata`]: crate::computer::Computer::list_mcp_servers_with_metadata

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::settings::config::snapshot::{resolve_snapshot, SnapshotArgs};
use crate::settings::redaction::{
    git_url_for_display, redact_git_urls_in_text, untrusted_name_for_display,
};
use crate::settings::schema::is_valid_marketplace_name;
use crate::settings::scope::EnvMap;
use crate::settings::store::{load_installed_plugins, load_known_marketplaces};
use crate::skills::frontmatter::parse_skill_frontmatter;
use crate::skills::manifest::{
    check_strict_conflict, entry_is_strict, enumerate_bundled_server_files, iter_plugin_entries,
    plugin_root_base, read_marketplace_manifest, read_plugin_metadata, resolve_plugin_version,
    resolve_skill_override_dirs, MARKETPLACE_MANIFEST_DIR, PLUGIN_MANIFEST,
};
use crate::skills::naming::{synthesize_name, SkillNameSpec};
use crate::skills::sources::{resolve_plugin_source, ResolvedPluginSource};
use crate::skills::staging::{SKILLS_SUBDIR, SKILL_MD};

/// 每实体来源 scope（provenance）——re-export，使 consumer 从高层拿到 scope 枚举、不 import `settings::*`。
pub use crate::settings::config::ProvenanceScope;

/// governance 快照 schema 版本（独立于协议版本）/ snapshot schema version。
pub const GOVERNANCE_SNAPSHOT_VERSION: u32 = 1;

// ===========================================================================
// 错误 / Error
// ===========================================================================

/// 治理查询错误 / governance query error。
///
/// **per-item 损坏走 [`GovernanceDiagnostic`] 不算 query error**（见模块头「per-item 韧性」）；本类型仅承载
/// 少见的整体失败（预留）。故实践中 [`Computer`](crate::computer::Computer) 的治理读方法多返回 `Ok`。
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum GovernanceQueryError {
    /// 内部错误（预留：整体查询失败）/ internal query failure。
    #[error("governance query failed: {0}")]
    Internal(String),
}

// ===========================================================================
// DTO
// ===========================================================================

/// 内容摘要 revision（`"sha256:<hex>"`）/ content-derived governance revision。
///
/// **仅对磁盘派生的治理内容**（marketplaces + plugin core 字段）做确定性摘要，**排除** live 叠加字段
/// （`bundled_skills` / `materialized_mcp_servers`）——保证「无变化时稳定、治理生命周期变更后变化」。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GovernanceRevision(pub String);

/// 结构化 per-item 诊断（无 secret）/ structured per-item diagnostic (no secrets)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GovernanceDiagnostic {
    /// 稳定诊断码（如 `catalog_unreadable` / `install_path_missing` / `missing_ledger_record`）。
    pub code: String,
    /// 人类可读说明（不含 secret 值）/ human-readable, no secrets。
    pub message: String,
}

impl GovernanceDiagnostic {
    fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: redact_git_urls_in_text(&message.into()),
        }
    }
}

fn public_optional_text(value: Option<String>) -> Option<String> {
    value.map(|text| redact_git_urls_in_text(&text))
}

fn public_texts(values: impl IntoIterator<Item = String>) -> Vec<String> {
    values
        .into_iter()
        .map(|text| untrusted_name_for_display(&text))
        .collect()
}

fn sanitize_declared_capabilities(capabilities: &mut DeclaredCapabilities) {
    capabilities.version = public_optional_text(capabilities.version.take());
    capabilities.description = public_optional_text(capabilities.description.take());
    capabilities.mcp_servers = public_texts(std::mem::take(&mut capabilities.mcp_servers));
    capabilities.skills = public_texts(std::mem::take(&mut capabilities.skills));
}

fn sanitize_plugin_snapshot(plugin: &mut PluginSnapshot) {
    plugin.id = untrusted_name_for_display(&plugin.id);
    plugin.plugin = untrusted_name_for_display(&plugin.plugin);
    plugin.marketplace = untrusted_name_for_display(&plugin.marketplace);
    plugin.name = public_optional_text(plugin.name.take());
    plugin.version = public_optional_text(plugin.version.take());
    plugin.install_scope = public_optional_text(plugin.install_scope.take());
    plugin.install_path = public_optional_text(plugin.install_path.take());
    plugin.bundled_mcp_servers = public_texts(std::mem::take(&mut plugin.bundled_mcp_servers));
    if let Some(declared) = &mut plugin.declared {
        sanitize_declared_capabilities(declared);
    }
}

/// marketplace 咨询判定 / advisory governance decision。
///
/// 从 `trusted`/`blocked`/`strict` 派生的**只读展示态**——这些设置当前 SDK 声明但**未强制执行**（enforcement
/// 属安装期 / 未来工作），故本判定仅供 UI 展示，不构成运行期门控。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum GovernanceDecision {
    /// 允许（默认；或已 trusted）/ allowed。
    Allowed,
    /// 阻断（在 `blockedMarketplaces`）/ blocked。
    Blocked,
    /// 受限（strict 模式且未 trusted）/ restricted。
    Restricted,
}

/// marketplace 状态 / marketplace status。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum MarketplaceStatus {
    /// 账本已注册但未克隆 / 仅意图（无 catalog 可读）/ known (registered, not cloned)。
    Known,
    /// 已克隆且 catalog 可读 / available (cloned, catalog readable)。
    Available,
    /// 加载 / catalog 出错 / degraded。
    Degraded,
}

/// 一个 marketplace 的高层治理投影 / one marketplace governance projection。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct MarketplaceSnapshot {
    /// marketplace 名 / name。
    pub name: String,
    /// 记录的 git 源 URL（`source.url`）/ recorded source URL。
    pub source_url: Option<String>,
    /// 是否在 `trustedMarketplaces` / trusted。
    pub trusted: bool,
    /// 是否在 `blockedMarketplaces` / blocked。
    pub blocked: bool,
    /// 是否 strict 模式（`strictKnownMarketplaces`，全局）/ strict mode。
    pub strict: bool,
    /// 咨询判定 / advisory decision。
    pub decision: GovernanceDecision,
    /// 克隆落地目录 / install location。
    pub install_location: Option<String>,
    /// 记录的 commit SHA / commit SHA。
    pub commit_sha: Option<String>,
    /// 最近一次实际克隆 / 拉取时间（ISO-8601）/ last updated。
    pub last_updated: Option<String>,
    /// 是否自动更新 / auto-update。
    pub auto_update: bool,
    /// 状态 / status。
    pub status: MarketplaceStatus,
    /// 属本 marketplace 的**已安装**（意图权威）plugin id / installed plugin ids (intent-authoritative)。
    pub plugin_ids: Vec<String>,
    /// catalog 声明的**可用** plugin id（含已安装）/ available plugin ids from catalog。
    pub available_plugin_ids: Vec<String>,
    /// 结构化诊断 / per-item diagnostics。
    pub diagnostics: Vec<GovernanceDiagnostic>,
}

/// plugin 状态 / plugin status。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum PluginStatus {
    /// catalog 有但未安装 / available (in catalog, not installed)。
    Available,
    /// 已安装但未启用 / installed & disabled。
    InstalledDisabled,
    /// 已安装且启用 / installed & enabled。
    InstalledEnabled,
    /// 物化 / manifest 损坏 / degraded。
    Degraded,
}

/// plugin **目录声明能力**（安装前可预览；来自 marketplace clone 内省）/ catalog-declared capabilities。
///
/// 与「安装后**实际物化**能力」（[`PluginSnapshot::bundled_mcp_servers`] / `materialized_mcp_servers`）
/// **语义正交**：本类型是 catalog/manifest **声明**的、安装前即可读的静态能力；后者是 install 后 ledger 记录
/// 的实际物化 / live 已挂载子集。二者刻意分离——不得用同一字段承载（#125 验收 2）。
///
/// 承载它的 [`PluginSnapshot::declared`] 用 `Option` 区分「未知」与「确实无」；本类型的 `mcp_servers` /
/// `skills` **空 vec** 表示「已内省且明确声明无该类能力」（≠ 未知，未知由外层 `None` 承载）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct DeclaredCapabilities {
    /// 声明版本（`entry.version` → `plugin.json.version`）/ declared version。
    pub version: Option<String>,
    /// 声明描述（`entry.description` → `plugin.json.description`）/ declared description。
    pub description: Option<String>,
    /// 目录声明的 bundled MCP server 名（clone 内 `mcp-servers/*.json` 文件名 stem，排序）。空 = 明确声明无。
    pub mcp_servers: Vec<String>,
    /// 目录声明的 bundled skill 名（`<plugin>:<skill>`，clone 内 `skills/<skill>/SKILL.md`，排序）。空 = 明确声明无。
    pub skills: Vec<String>,
}

/// 一个 plugin 的高层治理投影 / one plugin governance projection。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct PluginSnapshot {
    /// plugin id：`<plugin>@<marketplace>` / plugin id。
    pub id: String,
    /// plugin 名（`<plugin>` 段）/ plugin name segment。
    pub plugin: String,
    /// 所属 marketplace 名 / marketplace name。
    pub marketplace: String,
    /// 展示名（默认 = plugin 段；可后续从 manifest 富化）/ display name。
    pub name: Option<String>,
    /// 版本（ledger `version` → plugin.json `version` fallback）/ version。
    pub version: Option<String>,
    /// 状态 / status。
    pub status: PluginStatus,
    /// 是否已安装（**意图权威**）/ installed (intent-authoritative)。
    pub installed: bool,
    /// 是否启用 / enabled。
    pub enabled: bool,
    /// 启用态的胜出 scope（provenance）/ winning enablement scope。
    pub enablement_scope: Option<ProvenanceScope>,
    /// 安装 scope（ledger `scope`）/ install scope。
    pub install_scope: Option<String>,
    /// 物化落地目录（ledger `installPath`）/ install path。
    pub install_path: Option<String>,
    /// 咨询判定（随所属 marketplace）/ advisory decision (follows marketplace)。
    pub decision: GovernanceDecision,
    /// bundled MCP server 名 / bundled MCP server names。
    pub bundled_mcp_servers: Vec<String>,
    /// bundled skill 名（`<plugin>:<skill>`，live registry 叠加）/ bundled skill names (live)。
    pub bundled_skills: Vec<String>,
    /// 当前已物化的 bundled MCP server 子集（live 叠加）/ currently materialized bundled servers (live)。
    pub materialized_mcp_servers: Vec<String>,
    /// **目录声明能力**（安装前预览；与上面「实际物化」字段正交）/ catalog-declared capabilities。
    ///
    /// **`Some(caps)` = 已从 marketplace clone 内省**（`caps.mcp_servers`/`skills` 空 = 明确声明无能力）；
    /// **`None` = 未知**——remote(git)-source 未随 marketplace 克隆入 catalog / 无 catalog 条目 / local root
    /// 缺失不可内省。用 `Option` 而非空数组，精确区分「未知」与「确实无」（#125 验收 2）。
    pub declared: Option<DeclaredCapabilities>,
    /// 结构化诊断（含降级原因，即「最近一次结构化错误」）/ per-item diagnostics。
    pub diagnostics: Vec<GovernanceDiagnostic>,
}

/// 统一治理快照 / unified governance snapshot。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct GovernanceSnapshot {
    /// 内容摘要 revision（排除 live 叠加）/ content-derived revision (excludes live overlay)。
    pub revision: GovernanceRevision,
    /// marketplaces（按 name 排序）/ marketplaces (sorted by name)。
    pub marketplaces: Vec<MarketplaceSnapshot>,
    /// plugins（按 id 排序）/ plugins (sorted by id)。
    pub plugins: Vec<PluginSnapshot>,
    /// 快照级诊断（整体问题；per-item 走各项 `diagnostics`）/ snapshot-level diagnostics。
    pub diagnostics: Vec<GovernanceDiagnostic>,
}

/// [`Computer::list_plugins`](crate::computer::Computer::list_plugins) 过滤项 / list-plugins options。
///
/// **input 型**（consumer 构造），故**非** `#[non_exhaustive]`——consumer 须能以字面量 / [`Default`] 构造。
/// 后续新增过滤维度应保持 `Default`，consumer 用 `..Default::default()` 平滑吸收。
#[derive(Debug, Clone, Default)]
pub struct ListPluginsOptions {
    /// 是否包含仅 catalog 可用（未安装）的 plugin / include available (not-installed) catalog plugins。
    pub include_available: bool,
    /// 仅列某 marketplace 的 plugin / restrict to one marketplace。
    pub marketplace: Option<String>,
}

impl ListPluginsOptions {
    /// 追加 catalog 可用（未安装）项 / include available plugins。
    #[must_use]
    pub fn with_available(mut self, include: bool) -> Self {
        self.include_available = include;
        self
    }

    /// 限定某 marketplace / restrict to a marketplace。
    #[must_use]
    pub fn for_marketplace(mut self, name: impl Into<String>) -> Self {
        self.marketplace = Some(name.into());
        self
    }
}

// ===========================================================================
// Runtime 叠加 / runtime overlay（不入 revision）
// ===========================================================================

/// 轻量 live 叠加：从 [`Computer`](crate::computer::Computer) 的活跃状态采集，**不入 revision**。
#[derive(Debug, Clone, Default)]
pub(crate) struct GovernanceRuntimeOverlay {
    /// plugin_id → 活跃 bundled skill 名 / active bundled skill names by plugin id。
    pub bundled_skills_by_plugin: BTreeMap<String, Vec<String>>,
    /// 当前已物化的 MCP server **展示名**集 / currently materialized server display names。
    ///
    /// 有意用**名**而非 `bundle_id`：唯一消费者是与 ledger `bundled_mcp_servers`（持久化的 plugin-manifest
    /// 名字段）求交集，两侧须同域。仅治理展示、不做寻址。
    pub materialized_mcp_servers: BTreeSet<String>,
}

// ===========================================================================
// Builder 入参 / builder args
// ===========================================================================

/// 纯 builder 入参（镜像 [`SnapshotArgs`] 的注入接缝）/ pure-builder inputs (mirror resolver seams)。
#[derive(Default)]
pub(crate) struct GovernanceArgs<'a> {
    /// project/local 锚定工作目录；`None` → 进程 cwd / project anchor。
    pub cwd: Option<&'a Path>,
    /// 环境映射（解析 user config dir / skill home）；`None` → 进程环境 / env map。
    pub env: Option<&'a EnvMap>,
    /// SKILL Home（marketplace/plugin 意图 + 账本根）；`None` → env 解析 / home。
    pub home: Option<&'a Path>,
    /// policy `managed-mcp.json` 覆盖路径（测试隔离用）/ managed mcp path override。
    pub managed_mcp_path: Option<&'a Path>,
    /// 平台标识 / platform override。
    pub platform: Option<&'a str>,
    /// policy scope settings 原始视图 / raw policy settings。
    pub policy_settings: Option<&'a Map<String, Value>>,
}

// ===========================================================================
// 纯 builder / pure builder
// ===========================================================================

/// 解析高层治理快照（读，无写；per-item 韧性）/ resolve the governance snapshot (read-only, resilient)。
///
/// 复用 [`resolve_snapshot`] 取 marketplace known / strict·trusted·blocked / plugin **installed intent**
/// （权威）/ enabled + winning scope；再直读 store 富化 extra 字段（installLocation/commitSha/version/…）与
/// catalog（available plugin）。`overlay` 提供 live bundled skills / materialization，**不参与 revision**。
pub(crate) fn resolve_governance_snapshot(
    args: GovernanceArgs<'_>,
    overlay: &GovernanceRuntimeOverlay,
) -> GovernanceSnapshot {
    let GovernanceArgs {
        cwd,
        env,
        home,
        managed_mcp_path,
        platform,
        policy_settings,
    } = args;

    // --- 复用统一 config 快照（marketplace/plugin/enablement + winning scope）---
    let cfg = resolve_snapshot(SnapshotArgs {
        cwd,
        env,
        home,
        managed_mcp_path,
        platform,
        policy_settings,
        ..Default::default()
    });

    // 权威安装意图集（authoritative install set）。
    let intent_ids: BTreeSet<String> = cfg.plugins.installed.iter().map(|p| p.id.clone()).collect();
    // 启用态 + winning scope。
    let enabled_map: BTreeMap<String, (bool, ProvenanceScope)> = cfg
        .plugins
        .enabled
        .iter()
        .map(|e| (e.id.clone(), (e.enabled, e.origin)))
        .collect();
    let trusted: BTreeSet<&str> = cfg.marketplace.trusted.iter().map(String::as_str).collect();
    let blocked: BTreeSet<&str> = cfg.marketplace.blocked.iter().map(String::as_str).collect();
    let strict = cfg.marketplace.strict.unwrap_or(false);

    // --- 富字段直读 store ---
    let mk_file = load_known_marketplaces(home, env);
    let ledger = load_installed_plugins(home, env);

    // --- marketplaces（按 name 排序，确定性）---
    let mut mk_entries: Vec<(&String, &crate::settings::KnownMarketplaceEntry)> =
        mk_file.account.marketplaces.iter().collect();
    mk_entries.sort_by(|a, b| a.0.cmp(b.0));

    let mut marketplaces: Vec<MarketplaceSnapshot> = Vec::with_capacity(mk_entries.len());
    let mut decision_by_mp: BTreeMap<String, GovernanceDecision> = BTreeMap::new();
    // 每 mp 的可用 catalog id（供 plugin 阶段派生 available 项）。
    let mut available_by_mp: BTreeMap<String, Vec<String>> = BTreeMap::new();
    // #125：plugin id → (目录声明能力, probe 诊断, root_broken)。marketplace 阶段一次性内省，plugin 阶段挂载。
    #[allow(clippy::type_complexity)]
    let mut declared_by_id: BTreeMap<
        String,
        (
            Option<DeclaredCapabilities>,
            Vec<GovernanceDiagnostic>,
            bool,
        ),
    > = BTreeMap::new();

    for (name, entry) in mk_entries {
        let source_url = entry
            .source
            .get("url")
            .and_then(Value::as_str)
            .map(str::to_string);
        let install_location = extra_str(&entry.extra, "installLocation");
        let commit_sha = extra_str(&entry.extra, "commitSha");
        let last_updated = extra_str(&entry.extra, "lastUpdated");
        let auto_update = entry
            .extra
            .get("autoUpdate")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let is_trusted = trusted.contains(name.as_str());
        let is_blocked = blocked.contains(name.as_str());
        let decision = derive_decision(is_trusted, is_blocked, strict);
        decision_by_mp.insert(name.clone(), decision);

        // 属本 mp 的已安装（意图权威）。
        let suffix = format!("@{name}");
        let mut plugin_ids: Vec<String> = intent_ids
            .iter()
            .filter(|id| id.ends_with(&suffix))
            .cloned()
            .collect();
        plugin_ids.sort();

        // catalog（available）+ status。
        let mut diagnostics: Vec<GovernanceDiagnostic> = Vec::new();
        let (status, available_plugin_ids) = match install_location.as_deref() {
            None => (MarketplaceStatus::Known, Vec::new()),
            Some(loc) => match read_marketplace_manifest(Path::new(loc)) {
                Ok(manifest) => {
                    // #125：读 catalog 时对每个条目内省目录声明能力（local source only，无网络），
                    // 存入 `declared_by_id` 供 plugin 阶段挂载。仍产出排序后的 available id 集。
                    let base = plugin_root_base(&manifest);
                    let loc_path = Path::new(loc);
                    let mut ids: Vec<String> = Vec::new();
                    for e in iter_plugin_entries(&manifest) {
                        let Some(pn) = e.get("name").and_then(Value::as_str) else {
                            continue;
                        };
                        let pn = pn.trim();
                        if !is_valid_marketplace_name(pn) {
                            diagnostics.push(GovernanceDiagnostic::new(
                                "plugin_entry_invalid",
                                "plugin entry name must be lowercase kebab-case (1-64 characters)",
                            ));
                            continue;
                        }
                        let id = format!("{pn}@{name}");
                        declared_by_id.insert(
                            id.clone(),
                            probe_declared_capabilities(loc_path, &base, pn, e),
                        );
                        ids.push(id);
                    }
                    // 排序：与 `plugin_ids` 一致、DTO 输出确定（不依赖 catalog 文件内条目排布）。
                    ids.sort();
                    (MarketplaceStatus::Available, ids)
                }
                Err(e) => {
                    diagnostics.push(GovernanceDiagnostic::new(
                        "catalog_unreadable",
                        e.to_string(),
                    ));
                    (MarketplaceStatus::Degraded, Vec::new())
                }
            },
        };
        available_by_mp.insert(name.clone(), available_plugin_ids.clone());

        marketplaces.push(MarketplaceSnapshot {
            name: untrusted_name_for_display(name),
            source_url: source_url.map(|url| git_url_for_display(&url)),
            trusted: is_trusted,
            blocked: is_blocked,
            strict,
            decision,
            install_location: public_optional_text(install_location),
            commit_sha: public_optional_text(commit_sha),
            last_updated: public_optional_text(last_updated),
            auto_update,
            status,
            plugin_ids: public_texts(plugin_ids),
            available_plugin_ids: public_texts(available_plugin_ids),
            diagnostics,
        });
    }

    // --- plugins（installed from intent + available from catalog）---
    let mut plugins: Vec<PluginSnapshot> = Vec::new();

    // (a) 已安装（意图权威）。
    for id in &intent_ids {
        let (plugin, marketplace) = split_plugin_id(id);
        let record = ledger.account.plugins.get(id).and_then(|v| v.first());
        let mut diagnostics: Vec<GovernanceDiagnostic> = Vec::new();
        let mut degraded = false;

        let install_path = record.and_then(|r| r.install_path.clone());
        // #139：ledger 现存 bundle_id（身份键）。governance 展示面沿用 `Vec<String>` 承载 bundle_id 字面量，
        // 与下方 `materialized_mcp_servers` overlay（亦 bundle_id 域）求交集。
        let bundled_mcp_servers = record
            .map(|r| {
                r.mcp_servers
                    .iter()
                    .map(|b| b.as_str().to_string())
                    .collect()
            })
            .unwrap_or_default();
        let install_scope = record.and_then(|r| extra_str(&r.extra, "scope"));
        let version = resolve_installed_version(record, install_path.as_deref());

        match record {
            None => {
                degraded = true;
                diagnostics.push(GovernanceDiagnostic::new(
                    "missing_ledger_record",
                    "plugin is in install intent but has no derived ledger record",
                ));
            }
            Some(_) => {
                if let Some(p) = install_path.as_deref() {
                    if !Path::new(p).exists() {
                        degraded = true;
                        diagnostics.push(GovernanceDiagnostic::new(
                            "install_path_missing",
                            "recorded install path does not exist on disk",
                        ));
                    } else if let Some(diag) = plugin_manifest_diagnostic(p) {
                        // plugin.json 损坏：**不吞错**，挂 diagnostic。不翻 Degraded——plugin.json 属可选
                        // 元数据（skills/mcp 来自各自目录），损坏仅影响 version/元数据、不必然使 plugin 失效。
                        diagnostics.push(diag);
                    }
                }
            }
        }

        let (enabled, enablement_scope) = match enabled_map.get(id) {
            Some((e, s)) => (*e, Some(*s)),
            None => (false, None),
        };
        let status = if degraded {
            PluginStatus::Degraded
        } else if enabled {
            PluginStatus::InstalledEnabled
        } else {
            PluginStatus::InstalledDisabled
        };
        let decision = decision_by_mp
            .get(marketplace)
            .copied()
            .unwrap_or(GovernanceDecision::Allowed);
        // #125：挂目录声明能力（informative；status/diagnostic 仍由上面 install_path 健康逻辑决定，
        // 零行为变更——catalog clone 破损不 degrade 已装 plugin）。无 catalog 条目 → None。
        let declared = declared_by_id.get(id).and_then(|(caps, _, _)| caps.clone());

        plugins.push(PluginSnapshot {
            id: id.clone(),
            plugin: plugin.to_string(),
            marketplace: marketplace.to_string(),
            name: Some(plugin.to_string()),
            version,
            status,
            installed: true,
            enabled,
            enablement_scope,
            install_scope,
            install_path,
            decision,
            bundled_mcp_servers,
            bundled_skills: Vec::new(),
            materialized_mcp_servers: Vec::new(),
            declared,
            diagnostics,
        });
    }

    // (b) 仅 catalog 可用（未在意图）。
    for (mp, ids) in &available_by_mp {
        for id in ids {
            if intent_ids.contains(id) {
                continue;
            }
            let (plugin, _marketplace) = split_plugin_id(id);
            let decision = decision_by_mp
                .get(mp)
                .copied()
                .unwrap_or(GovernanceDecision::Allowed);
            // #125：挂目录声明能力 + probe 诊断；catalog 破损（local root 缺失）→ Degraded（验收 3）。
            let (declared, diagnostics, status) = match declared_by_id.get(id) {
                Some((caps, diags, root_broken)) => (
                    caps.clone(),
                    diags.clone(),
                    if *root_broken {
                        PluginStatus::Degraded
                    } else {
                        PluginStatus::Available
                    },
                ),
                None => (None, Vec::new(), PluginStatus::Available),
            };
            plugins.push(PluginSnapshot {
                id: id.clone(),
                plugin: plugin.to_string(),
                marketplace: mp.clone(),
                name: Some(plugin.to_string()),
                version: None,
                status,
                installed: false,
                enabled: false,
                enablement_scope: None,
                install_scope: None,
                install_path: None,
                decision,
                bundled_mcp_servers: Vec::new(),
                bundled_skills: Vec::new(),
                materialized_mcp_servers: Vec::new(),
                declared,
                diagnostics,
            });
        }
    }

    plugins.sort_by(|a, b| a.id.cmp(&b.id));
    let overlay_plugin_ids: Vec<String> = plugins.iter().map(|plugin| plugin.id.clone()).collect();
    for plugin in &mut plugins {
        sanitize_plugin_snapshot(plugin);
    }

    // --- revision = 磁盘核心投影摘要（**在填 overlay 之前**计算 → 排除 live 叠加）---
    let revision = compute_revision(&marketplaces, &plugins);

    // --- 填 live overlay（不影响已算好的 revision）---
    for (p, raw_id) in plugins.iter_mut().zip(overlay_plugin_ids) {
        if let Some(skills) = overlay.bundled_skills_by_plugin.get(&raw_id) {
            p.bundled_skills = public_texts(skills.clone());
        }
        p.materialized_mcp_servers = p
            .bundled_mcp_servers
            .iter()
            .filter(|s| overlay.materialized_mcp_servers.contains(*s))
            .cloned()
            .collect();
    }

    GovernanceSnapshot {
        revision,
        marketplaces,
        plugins,
        diagnostics: Vec::new(),
    }
}

// ===========================================================================
// 内部辅助 / internal helpers
// ===========================================================================

/// `trusted`/`blocked`/`strict` → 咨询判定 / advisory decision。
fn derive_decision(trusted: bool, blocked: bool, strict: bool) -> GovernanceDecision {
    if blocked {
        GovernanceDecision::Blocked
    } else if strict && !trusted {
        GovernanceDecision::Restricted
    } else {
        GovernanceDecision::Allowed
    }
}

/// `<plugin>@<marketplace>` 拆分（无 `@` → marketplace 为空串）/ split plugin id。
fn split_plugin_id(id: &str) -> (&str, &str) {
    id.split_once('@').unwrap_or((id, ""))
}

/// 已物化 plugin 的 `plugin.json` 健康：**存在但非合法 JSON 对象** → diagnostic（缺失属正常 → `None`）。
///
/// **不吞错**（验收 8）：install_path 存在但 `plugin.json` 损坏时挂 `plugin_manifest_unreadable`——但**不**强制
/// 翻 `Degraded`，因 plugin.json 为可选元数据（skills 来自 `skills/`、MCP 来自 `mcp-servers/`），损坏仅影响
/// version/元数据。缺失 `plugin.json` 是合法状态（返 `None`）。
fn plugin_manifest_diagnostic(install_path: &str) -> Option<GovernanceDiagnostic> {
    let manifest = Path::new(install_path)
        .join(MARKETPLACE_MANIFEST_DIR)
        .join(PLUGIN_MANIFEST);
    if !manifest.is_file() {
        return None;
    }
    let parses_as_object = std::fs::read_to_string(&manifest)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .is_some_and(|v| v.is_object());
    if parses_as_object {
        None
    } else {
        Some(GovernanceDiagnostic::new(
            "plugin_manifest_unreadable",
            "plugin.json is present but is not readable as a JSON object",
        ))
    }
}

/// 取 extra map 中的字符串字段（非 str → None）/ string field from extra map。
fn extra_str(extra: &Map<String, Value>, key: &str) -> Option<String> {
    extra.get(key).and_then(Value::as_str).map(str::to_string)
}

/// 从 marketplace clone 内省一个 catalog plugin 条目的**目录声明能力**（#125，仅 local source，无网络）。
///
/// 返回 `(declared, diagnostics, root_broken)`：
/// - **local source 且 root 存在** → `(Some(caps), diags, false)`：`mcp_servers` 取 clone 内 `mcp-servers/*.json`
///   文件名 stem、`skills` 取 `skills/<skill>/SKILL.md`（+ override 目录）合成 `<plugin>:<skill>`；
///   `version`/`description` 取 `entry` → `plugin.json`；plugin.json 存在但损坏 → 挂 `plugin_manifest_unreadable`
///   （不 degrade，可选元数据）。
/// - **local source 但 root 缺失** → `(None, [plugin_root_missing], true)`：catalog 声明 local plugin 但 clone 内
///   目录不在 → 破损；调用方对 available plugin 翻 `Degraded`（验收 3）。
/// - **remote(git) source** → `(None, [], false)`：实体在 install 时才独立 clone，安装前**合法未知**（无诊断）。
/// - **无 source** → `(None, [], false)`（沿用 #124 无 source catalog 条目的既有行为，不扰动）；
///   **source 解析失败** → `(None, [plugin_source_unresolved], false)`（不吞错、不 degrade）。
///
/// **只读、无网络、无隐式 clone**——严守 governance 快照契约（remote source 绝不触发 git）。
fn probe_declared_capabilities(
    catalog_dir: &Path,
    root_base: &str,
    plugin_name: &str,
    entry: &Map<String, Value>,
) -> (
    Option<DeclaredCapabilities>,
    Vec<GovernanceDiagnostic>,
    bool,
) {
    let Some(raw_source) = entry.get("source") else {
        return (None, Vec::new(), false);
    };
    let local = match resolve_plugin_source(raw_source, root_base) {
        Ok(ResolvedPluginSource::Local(l)) => l,
        // remote：实体不在 marketplace clone 内，安装前无法内省 → 合法未知。
        Ok(ResolvedPluginSource::Git(_)) => return (None, Vec::new(), false),
        Err(e) => {
            return (
                None,
                vec![GovernanceDiagnostic::new(
                    "plugin_source_unresolved",
                    format!("plugin catalog source is malformed: {e}"),
                )],
                false,
            );
        }
    };
    let plugin_root = catalog_dir.join(&local.rel_path);
    if !plugin_root.is_dir() {
        return (
            None,
            vec![GovernanceDiagnostic::new(
                "plugin_root_missing",
                "catalog declares a local-source plugin but its root is absent in the marketplace clone",
            )],
            true,
        );
    }

    let metadata = read_plugin_metadata(&plugin_root);
    let version = resolve_plugin_version(entry, &metadata, None);
    let description =
        extra_str(entry, "description").or_else(|| extra_str(&metadata, "description"));

    let mut diagnostics: Vec<GovernanceDiagnostic> = Vec::new();
    // plugin.json 存在但损坏：不吞错，挂诊断（version/description 已尽力从 entry 取）；不 degrade（可选元数据）。
    let root_str = plugin_root.to_string_lossy();
    if let Some(diag) = plugin_manifest_diagnostic(&root_str) {
        diagnostics.push(diag);
    }
    // strict=false 且 plugin.json 声明组件 → 该 plugin 安装期硬失败（marketplace-v1 §4.4），声明预览如实提示
    // （不 degrade：这是可用性告警，非结构损坏；consumer 据此知「此项虽有声明但装不进」）。
    if check_strict_conflict(entry, &metadata).is_err() {
        diagnostics.push(GovernanceDiagnostic::new(
            "strict_conflict",
            "strict=false but plugin.json declares components; this plugin would fail to install (marketplace-v1 §4.4)",
        ));
    }

    // 目录声明的 bundled MCP server（文件名 stem，排序去重）。
    let mut mcp_servers: Vec<String> = enumerate_bundled_server_files(&plugin_root)
        .iter()
        .filter_map(|p| p.file_stem().and_then(|s| s.to_str()).map(str::to_string))
        .collect();
    mcp_servers.sort();
    mcp_servers.dedup();

    // 目录声明的 bundled skill（`<plugin>:<skill>`）：skills 约定容器 + override 目录（与 staging 同源判定）。
    let mut skill_dirs = vec![plugin_root.join(SKILLS_SUBDIR)];
    skill_dirs.extend(resolve_skill_override_dirs(
        entry,
        &metadata,
        &plugin_root,
        entry_is_strict(entry),
    ));
    let mut skills = probe_declared_skill_names(plugin_name, &skill_dirs);
    skills.sort();
    skills.dedup();

    (
        Some(DeclaredCapabilities {
            version,
            description,
            mcp_servers,
            skills,
        }),
        diagnostics,
        false,
    )
}

/// 扫 skill 容器目录，产出目录声明的 SKILL 名（`<plugin>:<skill>`）/ Enumerate declared bundled skill names。
///
/// 与 staging（`scan_and_register_plugin_skills`）**同源准入**，使 `declared.skills` = 该 plugin 安装时**实际
/// 会物化**的 skill 集（避免安装前/后关联误差）：skill 包 = 含直接 `SKILL.md` **且 frontmatter 有非空
/// `description`** 的一级子目录（缺 description → staging 跳过不注册，故此处亦不计入）；name 经协议合成
/// [`synthesize_name`]（合成失败的目录跳过，与 staging 一致）。
fn probe_declared_skill_names(plugin_name: &str, skill_dirs: &[std::path::PathBuf]) -> Vec<String> {
    let mut names = Vec::new();
    for dir in skill_dirs {
        let Ok(rd) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in rd.filter_map(Result::ok) {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            // SKILL.md 存在 + frontmatter 非空 description（与 staging 准入一致，否则安装期不注册）。
            let Ok(text) = std::fs::read_to_string(path.join(SKILL_MD)) else {
                continue;
            };
            let has_description = parse_skill_frontmatter(&text)
                .get("description")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.trim().is_empty());
            if !has_description {
                continue;
            }
            let Some(basename) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if let Ok(name) = synthesize_name(SkillNameSpec::Marketplace {
                plugin: plugin_name,
                skill: basename,
            }) {
                names.push(name);
            }
        }
    }
    names
}

/// 已安装 plugin 版本：ledger `version` → plugin.json `version`（若 install_path 存在）/ resolve version。
fn resolve_installed_version(
    record: Option<&crate::settings::InstalledPluginRecord>,
    install_path: Option<&str>,
) -> Option<String> {
    if let Some(r) = record {
        if let Some(v) = extra_str(&r.extra, "version") {
            if !v.trim().is_empty() {
                return Some(v);
            }
        }
    }
    if let Some(p) = install_path {
        let path = Path::new(p);
        if path.exists() {
            let meta = crate::skills::manifest::read_plugin_metadata(path);
            if let Some(v) = meta.get("version").and_then(Value::as_str) {
                let trimmed = v.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
        }
    }
    None
}

/// 磁盘核心投影 → sha256 摘要（经 `to_value` 规范化 map 键序）/ content-derived digest。
fn compute_revision(
    marketplaces: &[MarketplaceSnapshot],
    plugins: &[PluginSnapshot],
) -> GovernanceRevision {
    // 只对 plugin **核心**字段做摘要（此刻 overlay 字段仍为空，序列化即排除 live 叠加）。
    let mut canonical = Map::new();
    canonical.insert(
        "version".to_string(),
        Value::from(GOVERNANCE_SNAPSHOT_VERSION),
    );
    canonical.insert(
        "marketplaces".to_string(),
        serde_json::to_value(marketplaces).expect("marketplaces serializable"),
    );
    canonical.insert(
        "plugins".to_string(),
        serde_json::to_value(plugins).expect("plugins serializable"),
    );
    let bytes = serde_json::to_vec(&Value::Object(canonical))
        .expect("governance projection is serializable to canonical JSON");
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let hex: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    GovernanceRevision(format!("sha256:{hex}"))
}

// ===========================================================================
// 测试 / tests（纯 builder）
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::store::{
        update_installed_plugins, update_installed_plugins_intent, update_known_marketplaces,
    };
    use crate::settings::{InstalledPluginRecord, KnownMarketplaceEntry};
    use serde_json::json;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn xdg_env(td: &TempDir) -> EnvMap {
        std::iter::once((
            "XDG_CONFIG_HOME".to_string(),
            td.path().join("xdg").to_string_lossy().into_owned(),
        ))
        .collect()
    }
    fn no_managed(td: &TempDir) -> PathBuf {
        td.path().join("no-managed.json")
    }
    fn record(install: Option<&Path>, version: &str) -> InstalledPluginRecord {
        let mut extra = Map::new();
        extra.insert("version".to_string(), json!(version));
        InstalledPluginRecord {
            install_path: install.map(|p| p.to_string_lossy().into_owned()),
            mcp_servers: vec![],
            extra,
        }
    }
    fn seed_catalog(dir: &Path, names: &[&str]) {
        let plugins: Vec<Value> = names.iter().map(|n| json!({ "name": n })).collect();
        let p = dir.join(".tfrobot-plugin").join("marketplace.json");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(
            &p,
            serde_json::to_string(&json!({ "plugins": plugins })).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn intent_is_authoritative_stale_ledger_not_installed() {
        let td = TempDir::new().unwrap();
        let env = xdg_env(&td);
        let home = td.path().join("home");
        // ledger 有 z@mp，意图空。
        update_installed_plugins(
            |f| {
                f.account
                    .plugins
                    .insert("z@mp".to_string(), vec![record(Some(td.path()), "1.0")]);
            },
            Some(&home),
            None,
        )
        .unwrap();

        let snap = resolve_governance_snapshot(
            GovernanceArgs {
                cwd: Some(&td.path().join("wd")),
                env: Some(&env),
                home: Some(&home),
                managed_mcp_path: Some(&no_managed(&td)),
                ..Default::default()
            },
            &GovernanceRuntimeOverlay::default(),
        );
        // z@mp 不在意图 → 不得作为 installed 项出现。
        assert!(
            snap.plugins
                .iter()
                .all(|p| !(p.id == "z@mp" && p.installed)),
            "陈旧 ledger 记录不得报 installed"
        );
    }

    #[test]
    fn catalog_corruption_degrades_marketplace_not_query() {
        let td = TempDir::new().unwrap();
        let env = xdg_env(&td);
        let home = td.path().join("home");
        let bad = td.path().join("bad"); // 无 catalog
        std::fs::create_dir_all(&bad).unwrap();
        update_known_marketplaces(
            |f| {
                let mut e = Map::new();
                e.insert("installLocation".to_string(), json!(bad.to_string_lossy()));
                f.account.marketplaces.insert(
                    "mp".to_string(),
                    KnownMarketplaceEntry {
                        source: json!({"type":"git","url":"https://x/y.git"}),
                        extra: e,
                    },
                );
            },
            Some(&home),
            None,
        )
        .unwrap();

        let snap = resolve_governance_snapshot(
            GovernanceArgs {
                cwd: Some(&td.path().join("wd")),
                env: Some(&env),
                home: Some(&home),
                managed_mcp_path: Some(&no_managed(&td)),
                ..Default::default()
            },
            &GovernanceRuntimeOverlay::default(),
        );
        assert_eq!(snap.marketplaces.len(), 1);
        assert_eq!(snap.marketplaces[0].status, MarketplaceStatus::Degraded);
        assert!(!snap.marketplaces[0].diagnostics.is_empty());
    }

    #[test]
    fn hand_edited_governance_dto_never_echoes_git_credentials() {
        let td = TempDir::new().unwrap();
        let env = xdg_env(&td);
        let home = td.path().join("home");
        let catalog = td.path().join("catalog");
        seed_catalog(
            &catalog,
            &["x=https://manifest:PW_PLUGIN@example.com/repo.git"],
        );

        update_known_marketplaces(
            |f| {
                f.account.marketplaces.insert(
                    "x=https://alice:PW_NAME@secret.example/repo.git".to_string(),
                    KnownMarketplaceEntry {
                        source: json!({
                            "type": "git",
                            "url": "https://bob:PW_URL@example.com/r.git?token=QUERY#FRAGMENT"
                        }),
                        extra: Map::from_iter([
                            (
                                "installLocation".to_string(),
                                json!("/tmp/x=https:/carol:PW_PATH@example.com/repo.git"),
                            ),
                            (
                                "commitSha".to_string(),
                                json!("https://dave:PW_SHA@example.com/r.git"),
                            ),
                            (
                                "lastUpdated".to_string(),
                                json!("git@secret.example:PW_UPDATED"),
                            ),
                        ]),
                    },
                );
                f.account.marketplaces.insert(
                    "safe".to_string(),
                    KnownMarketplaceEntry {
                        source: json!({"type": "git", "url": "https://example.com/safe.git"}),
                        extra: Map::from_iter([(
                            "installLocation".to_string(),
                            json!(catalog.to_string_lossy()),
                        )]),
                    },
                );
            },
            Some(&home),
            None,
        )
        .unwrap();
        update_installed_plugins_intent(
            |f| {
                f.account.installed_plugins.insert("audit@safe".to_string());
            },
            Some(&home),
            None,
        )
        .unwrap();
        update_installed_plugins(
            |f| {
                let mut extra = Map::new();
                extra.insert(
                    "version".to_string(),
                    json!("https://eve:PW_VERSION@example.com/r.git"),
                );
                f.account.plugins.insert(
                    "audit@safe".to_string(),
                    vec![InstalledPluginRecord {
                        install_path: Some(
                            "/tmp/x=https:/frank:PW_INSTALL@example.com/repo.git".to_string(),
                        ),
                        mcp_servers: vec![],
                        extra,
                    }],
                );
            },
            Some(&home),
            None,
        )
        .unwrap();

        let snap = resolve_governance_snapshot(
            GovernanceArgs {
                cwd: Some(td.path()),
                env: Some(&env),
                home: Some(&home),
                managed_mcp_path: Some(&no_managed(&td)),
                ..Default::default()
            },
            &GovernanceRuntimeOverlay::default(),
        );
        let rendered = format!("{snap:?}\n{}", serde_json::to_string(&snap).unwrap());
        for secret in [
            "alice",
            "PW_NAME",
            "bob",
            "PW_URL",
            "QUERY",
            "FRAGMENT",
            "carol",
            "PW_PATH",
            "dave",
            "PW_SHA",
            "PW_UPDATED",
            "PW_PLUGIN",
            "eve",
            "PW_VERSION",
            "frank",
            "PW_INSTALL",
        ] {
            assert!(!rendered.contains(secret), "{rendered}");
        }
        let safe = snap
            .marketplaces
            .iter()
            .find(|marketplace| marketplace.name == "safe")
            .unwrap();
        assert!(safe.available_plugin_ids.is_empty());
        assert!(safe
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "plugin_entry_invalid"));
    }

    #[test]
    fn revision_stable_and_excludes_overlay() {
        let td = TempDir::new().unwrap();
        let env = xdg_env(&td);
        let home = td.path().join("home");
        let catalog = td.path().join("catalog");
        seed_catalog(&catalog, &["a"]);
        update_known_marketplaces(
            |f| {
                let mut e = Map::new();
                e.insert(
                    "installLocation".to_string(),
                    json!(catalog.to_string_lossy()),
                );
                f.account.marketplaces.insert(
                    "mp".to_string(),
                    KnownMarketplaceEntry {
                        source: json!({"type":"git","url":"https://x/y.git"}),
                        extra: e,
                    },
                );
            },
            Some(&home),
            None,
        )
        .unwrap();
        update_installed_plugins_intent(
            |f| {
                f.account.installed_plugins.insert("a@mp".to_string());
            },
            Some(&home),
            None,
        )
        .unwrap();
        update_installed_plugins(
            |f| {
                f.account
                    .plugins
                    .insert("a@mp".to_string(), vec![record(Some(td.path()), "1.0")]);
            },
            Some(&home),
            None,
        )
        .unwrap();

        let managed = no_managed(&td);
        let args = || GovernanceArgs {
            cwd: Some(td.path()),
            env: Some(&env),
            home: Some(&home),
            managed_mcp_path: Some(&managed),
            ..Default::default()
        };
        let r_empty =
            resolve_governance_snapshot(args(), &GovernanceRuntimeOverlay::default()).revision;

        let mut overlay = GovernanceRuntimeOverlay::default();
        overlay
            .bundled_skills_by_plugin
            .insert("a@mp".to_string(), vec!["a:hello".to_string()]);
        let snap_overlay = resolve_governance_snapshot(args(), &overlay);
        // overlay 改变 → bundled_skills 出现，但 revision 不变。
        let a = snap_overlay
            .plugins
            .iter()
            .find(|p| p.id == "a@mp")
            .unwrap();
        assert_eq!(a.bundled_skills, vec!["a:hello".to_string()]);
        assert_eq!(
            r_empty, snap_overlay.revision,
            "live overlay 变化不得改动 revision"
        );
    }

    #[test]
    fn decision_derivation() {
        assert_eq!(
            derive_decision(false, false, false),
            GovernanceDecision::Allowed
        );
        assert_eq!(
            derive_decision(true, false, true),
            GovernanceDecision::Allowed
        );
        assert_eq!(
            derive_decision(false, true, false),
            GovernanceDecision::Blocked
        );
        assert_eq!(
            derive_decision(false, false, true),
            GovernanceDecision::Restricted
        );
    }

    // ── #125：probe_declared_capabilities 直测 ────────────────────────────────
    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }
    /// 在 `<catalog>/plugins/foo` 播种含 bundled MCP + skill + plugin.json 的 local plugin 树。
    fn seed_local_plugin(catalog: &Path) {
        let root = catalog.join("plugins").join("foo");
        std::fs::create_dir_all(root.join("mcp-servers")).unwrap();
        std::fs::write(
            root.join("mcp-servers").join("audit-mcp.json"),
            r#"{"name":"audit-mcp","type":"stdio","command":"echo"}"#,
        )
        .unwrap();
        // inputs.json 应被排除，不计入声明。
        std::fs::write(root.join("mcp-servers").join("inputs.json"), "{}").unwrap();
        std::fs::create_dir_all(root.join("skills").join("preview-skill")).unwrap();
        std::fs::write(
            root.join("skills").join("preview-skill").join("SKILL.md"),
            "---\ndescription: x\n---\n",
        )
        .unwrap();
        // 无 SKILL.md 的目录不算 skill 包。
        std::fs::create_dir_all(root.join("skills").join("not-a-skill")).unwrap();
        // 有 SKILL.md 但 frontmatter 缺 description → staging 不注册，declared 亦不计入（同源准入）。
        std::fs::create_dir_all(root.join("skills").join("no-desc-skill")).unwrap();
        std::fs::write(
            root.join("skills").join("no-desc-skill").join("SKILL.md"),
            "---\nname: no-desc\n---\n# no description\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join(".tfrobot-plugin")).unwrap();
        std::fs::write(
            root.join(".tfrobot-plugin").join("plugin.json"),
            r#"{"version":"3.4.5","description":"Foo plugin desc"}"#,
        )
        .unwrap();
    }

    #[test]
    fn probe_local_source_introspects_declared_capabilities() {
        let td = TempDir::new().unwrap();
        seed_local_plugin(td.path());
        let entry = obj(json!({"name": "foo", "source": "./plugins/foo"}));
        let (declared, diags, root_broken) =
            probe_declared_capabilities(td.path(), "./plugins", "foo", &entry);
        assert!(!root_broken && diags.is_empty());
        let caps = declared.expect("local source 应内省出 Some");
        assert_eq!(
            caps.mcp_servers,
            vec!["audit-mcp".to_string()],
            "排除 inputs.json"
        );
        // 仅 preview-skill 计入：not-a-skill（无 SKILL.md）与 no-desc-skill（缺 description）均被排除。
        assert_eq!(caps.skills, vec!["foo:preview-skill".to_string()]);
        assert_eq!(caps.version.as_deref(), Some("3.4.5"));
        assert_eq!(caps.description.as_deref(), Some("Foo plugin desc"));
    }

    #[test]
    fn probe_remote_source_is_unknown_not_empty() {
        let td = TempDir::new().unwrap();
        let entry = obj(json!({"name": "bar", "source": {"source": "github", "repo": "acme/bar"}}));
        let (declared, diags, root_broken) =
            probe_declared_capabilities(td.path(), "./plugins", "bar", &entry);
        assert!(declared.is_none(), "remote source 安装前未知 → None");
        assert!(diags.is_empty() && !root_broken, "remote 非破损、无诊断");
    }

    #[test]
    fn probe_missing_local_root_degrades() {
        let td = TempDir::new().unwrap();
        // catalog 声明 local plugin 但 clone 内目录不存在。
        let entry = obj(json!({"name": "gone", "source": "./plugins/gone"}));
        let (declared, diags, root_broken) =
            probe_declared_capabilities(td.path(), "./plugins", "gone", &entry);
        assert!(declared.is_none());
        assert!(root_broken, "local root 缺失 → 破损（调用方翻 Degraded）");
        assert!(diags.iter().any(|d| d.code == "plugin_root_missing"));
    }

    #[test]
    fn probe_missing_source_is_unknown_without_diagnostic() {
        let td = TempDir::new().unwrap();
        let entry = obj(json!({"name": "x"})); // 无 source（#124 seed_catalog 形态）
        let (declared, diags, root_broken) =
            probe_declared_capabilities(td.path(), "./plugins", "x", &entry);
        assert!(declared.is_none() && diags.is_empty() && !root_broken);
    }

    #[test]
    fn probe_malformed_source_diagnostic_never_echoes_url_credentials() {
        let td = TempDir::new().unwrap();
        for source in [
            json!({
                "source": "url",
                "url": "ftp://cnb:FAKE_TOKEN@example.com/org/repo.git?token=QUERY#FRAGMENT"
            }),
            json!("key=https://cnb:FAKE_TOKEN@example.com/org/repo.git"),
            json!("key=git@example.com:org/repo.git"),
            json!("x=https://public.example/a=https://user2:PW_TWO@secret.example/repo.git"),
            json!("key=用户@example.com:org/repo.git"),
            json!("x=https://example.com/r.git?token=;QUERY_SECRET"),
            json!("x=https://example.com/r.git?token=QUERY_QUOTE'LEAK_SECRET"),
            json!("x=https://alice:PW_ONE'PW_TWO@example.com/repo.git"),
            json!("x=alice@my_host:org/repo.git"),
            json!("x=用户@例子.公司:org/repo.git"),
        ] {
            let entry = obj(json!({"name": "x", "source": source}));
            let (declared, diags, root_broken) =
                probe_declared_capabilities(td.path(), "./plugins", "x", &entry);
            assert!(declared.is_none() && !root_broken);
            let diagnostic = diags
                .iter()
                .find(|d| d.code == "plugin_source_unresolved")
                .expect("malformed source must produce a governance diagnostic");
            let rendered = format!(
                "{}\n{diagnostic:?}\n{}",
                diagnostic.message,
                serde_json::to_string(diagnostic).unwrap()
            );
            for secret in [
                "cnb",
                "FAKE_TOKEN",
                "QUERY",
                "FRAGMENT",
                "git@example.com",
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
    fn probe_strict_false_with_plugin_json_components_flags_conflict() {
        let td = TempDir::new().unwrap();
        // strict=false + plugin.json 声明组件 → 安装期硬失败（marketplace-v1 §4.4）；声明预览挂 strict_conflict。
        let root = td.path().join("plugins").join("s");
        std::fs::create_dir_all(root.join(".tfrobot-plugin")).unwrap();
        std::fs::write(
            root.join(".tfrobot-plugin").join("plugin.json"),
            r#"{"skills":["extra"]}"#,
        )
        .unwrap();
        let entry = obj(json!({"name": "s", "source": "./plugins/s", "strict": false}));
        let (declared, diags, root_broken) =
            probe_declared_capabilities(td.path(), "./plugins", "s", &entry);
        assert!(declared.is_some() && !root_broken, "结构在 → Some、非破损");
        assert!(
            diags.iter().any(|d| d.code == "strict_conflict"),
            "strict=false 声明组件冲突须挂 strict_conflict 诊断"
        );
    }
}
