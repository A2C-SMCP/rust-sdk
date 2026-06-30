/*!
* 文件名: installer.rs
* 作者: JQQ
* 创建日期: 2026/06/05
* 最后修改日期: 2026/06/05
* 版权: 2023 JQQ. All rights reserved.
* 依赖: serde_json, chrono, async-trait, smcp (utils::path), crate::settings::{schema,scope,store,reconciler}, crate::skills::{home,manifest,registry,staging}
* 描述: Plugin 显式生命周期 install / uninstall / enable / disable（SET-05 #69）
*       Plugin explicit lifecycle install / uninstall / enable / disable.
*/

//! Plugin 显式生命周期：install / uninstall / enable / disable / Plugin explicit lifecycle。
//!
//! 协议依据 / Protocol: a2c-smcp-protocol §9.x（plugin 生命周期 / MCP 外来同名冲突 / uninstall 级联），
//! marketplace-v1 §4（plugin entry / strict / version）/ §7.2 / §10.6（MCP 同名硬抛、无逃生口）。
//! 对标 Python `a2c_smcp/computer/settings/installer.py`。
//!
//! 本模块是 [`crate::settings::reconciler`] 的**兄弟**——reconciler 做 additive-only 对账 + 孤儿
//! gc/prune（[`crate::settings::reconciler::gc_plugins`]），本模块做**显式单 plugin** 增删启停，写
//! `installed_plugins.json`（正是 `gc_plugins` 读取的账本）。复用
//! [`stage_marketplace_skills`]（skill 注册）+ [`locate_plugin_root`](crate::skills::staging)（plugin 根
//! 定位/clone）+ [`crate::skills::manifest`]（marketplace.json / plugin.json / mcp-servers 解析）。
//!
//! ## 与 Python 的设计对齐 / Design alignment（忠实对标 + 习惯化适配）
//!
//! Python 用 4 个独立可选回调（`existing_server_names` / `register_server` / `remove_server` /
//! `inject_inputs`）+ `_require_existing_names_guard` 强制「给了 register 必给 existing」（否则冲突闸门以
//! `existing=∅` 静默旁路，违 §10.6）。Rust 收敛为**单一注入 trait** [`McpInstallHooks`]：四操作同属一对象，
//! `existing_server_names` 与 `register_server` 在类型层即成对——**结构性地**杜绝 §10.6 静默旁路（Python 运行时
//! 护栏在此变为编译期保证，故无需移植该护栏函数）。`hooks = None` ⇒ **ledger-only**（`existing=∅`、不注册/不摘除/
//! 不注入；单测 / 无 server 场景）。回调真正接线（`Computer.aadd_or_aupdate_server` 等）由 CLI 集成层（#48）承担。
//!
//! 另一差异：Rust [`stage_marketplace_skills`] **失败降级**（返回 `Vec`、不抛），故 skill 注册阶段不触发
//! 补偿回滚；回滚由 `inject_inputs` / `register_server` 注入回调的失败驱动（与 Python 的回滚路径同语义）。
//!
//! ## 边界（文档化，非缺陷）/ Documented boundaries
//! - 操作 **live session**：跨重启重挂 bundled server + `installed × enabled` 交集归 reconcile 接线层。
//! - disable 的 skill orphan 为**内存态**（Registry 不落盘），同会话廉价复原；跨重启靠 reconcile 重建。
//!   uninstall 仅注销**活跃** SKILL；先 disable（orphan）再 uninstall 会残留内存孤儿条目，进程重启即清。
//! - **非原子的 disable**：先写 settings 再摘 server / orphan skill；`remove_server` 抛错留半态——靠 reconcile 兜底。

use std::collections::{BTreeMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Map, Value};

use crate::mcp_clients::model::MCPServerConfig;
use crate::settings::reconciler::{safe_rmtree, InstalledPluginRecord};
use crate::settings::schema::{is_valid_enabled_plugin_key, SettingsScope};
use crate::settings::scope::{
    apply_write, load_settings_file, user_settings_path, workdir_local_settings_path,
    workdir_project_settings_path, EnvMap, WriteValue,
};
use crate::settings::store::{
    self, load_installed_plugins, load_known_marketplaces, update_installed_plugins,
    InstalledPluginsFile, SettingsStoreError,
};
use crate::skills::home::{marketplace_skill_dir, SOURCE_MARKETPLACE};
use crate::skills::manifest::{
    check_strict_conflict, find_plugin_entry, load_bundled_servers, plugin_root_base,
    read_marketplace_manifest, read_plugin_metadata, resolve_plugin_version, PluginManifestError,
};
use crate::skills::registry::SkillRegistry;
use crate::skills::staging::{
    locate_plugin_root, stage_marketplace_skills, MarketplaceStageOptions, SkillStagingError,
    DEFAULT_GIT_TIMEOUT,
};

// ---------------------------------------------------------------------------
// 错误 / Errors
// ---------------------------------------------------------------------------
/// 注入的 MCP 回调失败（register / remove / inject_inputs）/ An injected MCP hook failed。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("mcp hook failed: {0}")]
pub struct McpHookError(pub String);

/// bundled MCP server 与**外来**同名 server 冲突（§7.2/§10.6）/ Foreign MCP server name conflict。
///
/// **硬抛、原子失败、无 rename/force 逃生口**（name 即身份）。判定排除 plugin 自有（命中其
/// `bundledMcpServers` 记录）→ 幂等再物化不算冲突。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("MCP server name conflict: {0}")]
pub struct McpServerNameConflictError(pub String);

/// plugin install/enable/uninstall/disable 失败 / Plugin lifecycle failure。
///
/// [`PluginInstallError::Conflict`] 单独建模 §10.6「外来 MCP 同名硬抛」——调用方可 `matches!` 精确识别
/// （对标 Python 的独立 `MCPServerNameConflictError` 异常类型，无逃生口）。
#[derive(Debug, thiserror::Error)]
pub enum PluginInstallError {
    /// 前置失败（marketplace 未添加 / catalog 未 clone / entry 缺失 / 非法 id / 非法 scope）。
    #[error("{0}")]
    Precondition(String),
    /// 外来 MCP server 同名冲突（硬抛、无逃生口，§10.6）。
    #[error(transparent)]
    Conflict(#[from] McpServerNameConflictError),
    /// manifest / mcp-servers 解析或 strict 冲突（注册前畸形即抛）。
    #[error(transparent)]
    Manifest(#[from] PluginManifestError),
    /// plugin 根定位 / clone 失败。
    #[error(transparent)]
    Staging(#[from] SkillStagingError),
    /// 账本持久化失败（锁 / I/O）。
    #[error(transparent)]
    Store(#[from] SettingsStoreError),
    /// 注入 MCP 回调失败（已补偿回滚后上抛）。
    #[error(transparent)]
    Hook(#[from] McpHookError),
    /// settings.json 持锁写 I/O 失败。
    #[error("settings write io error: {0}")]
    Io(#[from] io::Error),
}

// ---------------------------------------------------------------------------
// MCP 注入接缝 / Injected MCP interface（沿用 reconciler `McpTeardown` 的 async-trait 注入风格）
// ---------------------------------------------------------------------------
/// install/enable/uninstall/disable 期 MCP server + inputs 注入接缝 / MCP injection seam。
///
/// CLI 集成层（#48）包 `Computer.aadd_or_aupdate_server`（含 `${input:}` 渲染）/ `aremove_server` /
/// `get_server_status` / `load_plugin_inputs`。`existing_server_names` 与 `register_server` 同属本 trait ⇒
/// **结构性成对**，杜绝 §10.6 冲突闸门静默旁路。`hooks = None` ⇒ ledger-only（见模块文档）。
#[async_trait]
pub trait McpInstallHooks: Send + Sync {
    /// 当前已注册 server 名集合（冲突闸门输入）/ currently-registered server names。
    fn existing_server_names(&self) -> HashSet<String>;
    /// 注册 / 更新一个 server（含 `${input:}` 渲染）/ register or update a server。
    ///
    /// # Errors
    /// 注册失败 → [`McpHookError`]（触发 install 补偿回滚）。
    async fn register_server(&self, cfg: MCPServerConfig) -> Result<(), McpHookError>;
    /// 停止并摘除一个 server / stop & remove a server。
    ///
    /// # Errors
    /// 摘除失败 → [`McpHookError`]（uninstall/disable 上抛；install 回滚中仅 WARN）。
    async fn remove_server(&self, name: &str) -> Result<(), McpHookError>;
    /// 注入 plugin-scoped inputs 入池（在 register 之前，使裸 id 经 D2 前缀回退命中，§9.3）/ inject inputs。
    ///
    /// 默认 no-op（`inputs.json` 缺失 / 无需注入时）/ defaults to no-op。
    ///
    /// # Errors
    /// 注入失败 → [`McpHookError`]（触发 install 补偿回滚；已注入的 inputs 不回滚——悬空前缀 def 无害）。
    async fn inject_inputs(&self, _plugin_root: &Path) -> Result<(), McpHookError> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 选项 / Options
// ---------------------------------------------------------------------------
/// [`install_plugin`] 选项 / install options。
#[derive(Default)]
pub struct InstallOptions<'a> {
    /// 物化记录 scope（`user|project|local`，默认 `user`）/ install-record scope。
    pub scope: Option<&'a str>,
    /// active workdir（`project|local` scope 必需）/ active workdir for project/local scope。
    pub project_path: Option<&'a str>,
    /// 记录版本覆盖（`--version`）；缺省按 entry > plugin.json > commitSha 解析 / version override。
    pub version: Option<&'a str>,
    /// `true` → 已存在 catalog 走 pull、独立 plugin 重 clone（sha-pin 例外）/ refresh mode。
    pub refresh: bool,
    /// 单次 git 操作超时（`None` → [`DEFAULT_GIT_TIMEOUT`]）/ per-op git timeout。
    pub timeout: Option<Duration>,
    /// `$A2C_SKILL_HOME` 等环境覆盖（`None` → 进程环境）/ env overrides。
    pub env: Option<&'a EnvMap>,
}

/// [`uninstall_plugin`] 选项 / uninstall options。
#[derive(Default)]
pub struct UninstallOptions<'a> {
    /// `None` 删该 id 全部记录；指定则仅删该 scope 记录 / scope filter (None = all)。
    pub scope: Option<&'a str>,
    /// 跳过 bundled server 摘除（保留 config）/ keep bundled servers mounted。
    pub keep_servers: bool,
    /// env 覆盖 / env overrides。
    pub env: Option<&'a EnvMap>,
}

/// [`enable_plugin`] 选项 / enable options。
#[derive(Default)]
pub struct EnableOptions<'a> {
    /// 写 `enabledPlugins` 的 scope（须与安装 scope 一致；默认 `user`）/ scope for enabledPlugins write。
    pub scope: Option<&'a str>,
    /// active workdir（`project|local` scope 必需）/ active workdir。
    pub project_path: Option<&'a str>,
    /// 单次 git 操作超时（`None` → [`DEFAULT_GIT_TIMEOUT`]）/ per-op git timeout。
    pub timeout: Option<Duration>,
    /// env 覆盖 / env overrides。
    pub env: Option<&'a EnvMap>,
}

/// [`disable_plugin`] 选项 / disable options。
#[derive(Default)]
pub struct DisableOptions<'a> {
    /// 写 `enabledPlugins` 的 scope（须与安装 scope 一致；默认 `user`）/ scope for enabledPlugins write。
    pub scope: Option<&'a str>,
    /// active workdir（`project|local` scope 必需）/ active workdir。
    pub project_path: Option<&'a str>,
    /// env 覆盖 / env overrides。
    pub env: Option<&'a EnvMap>,
}

// ---------------------------------------------------------------------------
// 内部辅助 / Internal helpers
// ---------------------------------------------------------------------------
/// `<plugin>@<marketplace>` → `(plugin, marketplace)`；非法 key → [`PluginInstallError::Precondition`]。
fn split_plugin_id(plugin_id: &str) -> Result<(String, String), PluginInstallError> {
    if !is_valid_enabled_plugin_key(plugin_id) {
        return Err(PluginInstallError::Precondition(format!(
            "invalid plugin id {plugin_id:?} (expect '<plugin>@<marketplace>', strict kebab, each ≤64)"
        )));
    }
    // 合法 key 必含 `@`（[`is_valid_enabled_plugin_key`] 已保证）/ valid key always has '@'。
    let (plugin, marketplace) = plugin_id.split_once('@').unwrap_or((plugin_id, ""));
    Ok((plugin.to_string(), marketplace.to_string()))
}

/// 解析 enable/disable 写入的 settings.json 路径与 scope 枚举 / Resolve the settings.json path + scope。
///
/// 可写 scope：`user`（默认）/ `project` / `local`；`managed`/`policy`/未知 → 拒（policy 只读，§5.1）。
fn settings_path_for_scope(
    scope: &str,
    project_path: Option<&str>,
    env: Option<&EnvMap>,
) -> Result<(PathBuf, SettingsScope), PluginInstallError> {
    match scope {
        "user" => Ok((user_settings_path(env), SettingsScope::User)),
        "project" | "local" => {
            let wd = project_path.filter(|s| !s.is_empty()).ok_or_else(|| {
                PluginInstallError::Precondition(format!(
                    "scope {scope:?} requires project_path (active workdir)"
                ))
            })?;
            let wd = Path::new(wd);
            if scope == "project" {
                Ok((workdir_project_settings_path(wd), SettingsScope::Project))
            } else {
                Ok((workdir_local_settings_path(wd), SettingsScope::Local))
            }
        }
        _ => Err(PluginInstallError::Precondition(format!(
            "cannot write enabledPlugins to scope {scope:?} (writable: user|project|local)"
        ))),
    }
}

/// 写 `enabledPlugins[<plugin_id>] = value` 到指定 scope 的 settings.json（持锁原子 RMW）/ Write the flag。
///
/// 复用 store 旁车锁 + 原子写 + scope 的 [`load_settings_file`] / [`apply_write`]（仅改该 key、不毁兄弟）。
/// settings.json 是人编意图层 → 无写保护头（[`store::atomic_write_settings_json`]）。
fn write_enabled_plugin(
    plugin_id: &str,
    value: bool,
    scope: &str,
    project_path: Option<&str>,
    env: Option<&EnvMap>,
) -> Result<(), PluginInstallError> {
    let (path, scope_enum) = settings_path_for_scope(scope, project_path, env)?;
    let mut inner: BTreeMap<String, WriteValue> = BTreeMap::new();
    inner.insert(plugin_id.to_string(), WriteValue::Set(Value::Bool(value)));
    let mut updates: BTreeMap<String, WriteValue> = BTreeMap::new();
    updates.insert("enabledPlugins".to_string(), WriteValue::Object(inner));

    // 外层 `?`：锁失败（[`SettingsStoreError`]）；内层 `?`：写 I/O（[`io::Error`]）。
    store::with_settings_lock(&path, || -> io::Result<()> {
        let (existing, _errors) = load_settings_file(&path, scope_enum);
        let updated = apply_write(&existing, &updates);
        store::atomic_write_settings_json(&path, &Value::Object(updated))
    })??;
    Ok(())
}

/// 某 plugin 当前**活跃**的 marketplace SKILL name / A plugin's currently-active marketplace SKILL names。
///
/// 过滤 `source == "marketplace:<mp>"` 且 `name` 以 `<plugin>:` 起；仅枚举活跃集（排除孤儿）。
fn plugin_skill_names(registry: &SkillRegistry, marketplace: &str, plugin: &str) -> Vec<String> {
    let want_source = format!("{SOURCE_MARKETPLACE}:{marketplace}");
    let prefix = format!("{plugin}:");
    registry
        .active_refs()
        .into_iter()
        .filter(|r| r.source == want_source && r.name.starts_with(&prefix))
        .map(|r| r.name)
        .collect()
}

/// 该 plugin 物化记录里全部 `bundledMcpServers` 名（跨 scope 记录并集）/ All recorded bundled server names。
fn bundled_servers_of(home: &Path, plugin_id: &str, env: Option<&EnvMap>) -> HashSet<String> {
    let installed = load_installed_plugins(Some(home), env);
    let mut out: HashSet<String> = HashSet::new();
    if let Some(records) = installed.account.plugins.get(plugin_id) {
        for rec in records {
            out.extend(rec.bundled_mcp_servers.iter().cloned());
        }
    }
    out
}

/// 从 `known_marketplaces.json` 取 marketplace 的 git source + commitSha；未添加 → 拒 / Resolve mp source。
fn resolve_marketplace_source(
    marketplace: &str,
    home: &Path,
    env: Option<&EnvMap>,
) -> Result<(Value, Option<String>), PluginInstallError> {
    let known = load_known_marketplaces(Some(home), env);
    let record = known.account.marketplaces.get(marketplace).ok_or_else(|| {
        PluginInstallError::Precondition(format!(
            "marketplace {marketplace:?} not added (run 'marketplace add' first)"
        ))
    })?;
    if !record.source.is_object() {
        return Err(PluginInstallError::Precondition(format!(
            "marketplace {marketplace:?} has no valid source record"
        )));
    }
    let commit_sha = record
        .extra
        .get("commitSha")
        .and_then(Value::as_str)
        .map(String::from);
    Ok((record.source.clone(), commit_sha))
}

/// bundled server 同名冲突预检（§7.2/§10.6）/ Bundled-server name-conflict precheck。
///
/// 外来同名（`name in existing` 且 `not in owned`）→ [`McpServerNameConflictError`]（硬抛、零变更）；
/// plugin 自有（命中 `owned`）→ 幂等放行。
fn conflict_check(
    servers: &[MCPServerConfig],
    existing: &HashSet<String>,
    owned: &HashSet<String>,
) -> Result<(), McpServerNameConflictError> {
    for cfg in servers {
        let name = cfg.name();
        if existing.contains(name) && !owned.contains(name) {
            return Err(McpServerNameConflictError(format!(
                "{name:?} already exists and is not owned by this plugin. Resolve by 'server rm' the \
                 existing server, or rename it in the plugin's own manifest (no --rename / \
                 --force-override escape hatch: name is identity)."
            )));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// install / uninstall / enable / disable
// ---------------------------------------------------------------------------
/// 显式安装单个 plugin（**原子失败：冲突即抛、不留半装**）/ Install one plugin atomically。
///
/// 顺序（§10.6「预检-先于-变更」）：① 解析 id + mp source；② 要求 catalog 已 clone、读 marketplace.json
/// 定位 entry；③ [`locate_plugin_root`] 定位 plugin 根（必要时 clone）；④ strict 冲突检测 +
/// [`load_bundled_servers`]（注册前畸形即抛）；⑤ **★冲突闸门**（外来同名硬抛、零变更）；⑥-⑦ 过闸后注册
/// servers → 注册 skills，任一失败 → **精确补偿回滚**（仅撤本次新增）→ 上抛、不写账本；⑧ **★最后**写账本。
///
/// # Errors
/// 见 [`PluginInstallError`]（冲突 / 前置 / manifest / 定位 / 注入 / 账本）。
pub async fn install_plugin(
    plugin_id: &str,
    registry: &mut SkillRegistry,
    home: &Path,
    options: InstallOptions<'_>,
    hooks: Option<&dyn McpInstallHooks>,
) -> Result<InstalledPluginRecord, PluginInstallError> {
    let (plugin, marketplace) = split_plugin_id(plugin_id)?;
    let scope = options.scope.unwrap_or("user");
    let timeout = options.timeout.unwrap_or(DEFAULT_GIT_TIMEOUT);
    let env = options.env;
    let (source, commit_sha) = resolve_marketplace_source(&marketplace, home, env)?;

    let catalog_dir = marketplace_skill_dir(home, &marketplace, &[]);
    if !catalog_dir.is_dir() {
        return Err(PluginInstallError::Precondition(format!(
            "marketplace {marketplace:?} catalog not cloned at {} (run 'marketplace add/refresh' first)",
            catalog_dir.display()
        )));
    }

    let manifest = read_marketplace_manifest(&catalog_dir)?;
    let entry = find_plugin_entry(&manifest, &plugin).ok_or_else(|| {
        PluginInstallError::Precondition(format!(
            "plugin {plugin:?} not found in marketplace {marketplace:?} manifest"
        ))
    })?;

    // ①-④：定位 + 解析（注册前，畸形即抛 → 原子前置）
    let root_base = plugin_root_base(&manifest);
    let (plugin_root, version_fallback) = locate_plugin_root(
        &marketplace,
        &plugin,
        entry,
        &catalog_dir,
        &root_base,
        home,
        commit_sha.as_deref(),
        options.refresh,
        timeout,
        env,
    )
    .await?;
    // plugin.json 读一次复用（冲突检测 + version 解析共用）/ read once, reuse。
    let plugin_manifest = read_plugin_metadata(&plugin_root);
    // strict mode 冲突检测（§4.4）：早检——挂 server / 注册 skill 前拦截，保证原子失败。
    check_strict_conflict(entry, &plugin_manifest)?;
    let servers = load_bundled_servers(&plugin_root)?;

    // ⑤：★冲突闸门（零变更）。owned = 自有同名白名单（上次记录的 bundledMcpServers）。
    let owned = bundled_servers_of(home, plugin_id, env);
    let existing = hooks
        .map(McpInstallHooks::existing_server_names)
        .unwrap_or_default();
    conflict_check(&servers, &existing, &owned)?;

    // —— 过闸：开始变更，失败补偿回滚 ——
    // 快照本 plugin 已活跃 skill：补偿只撤**本次新增**，避免重装中途失败误删既有 skill / 摘除 owned server。
    let skills_before: HashSet<String> = plugin_skill_names(registry, &marketplace, &plugin)
        .into_iter()
        .collect();
    let filter: HashSet<String> = std::iter::once(plugin.clone()).collect();
    let mut registered: Vec<String> = Vec::new();
    let mut staged_skills: Vec<String> = Vec::new();
    let mut mutate_err: Option<PluginInstallError> = None;

    'mutate: {
        if let Some(h) = hooks {
            // 注入 plugin-scoped inputs（须在 register 之前，#69 Group A，§9.3 D2）；失败走补偿回滚上抛。
            if let Err(e) = h.inject_inputs(&plugin_root).await {
                mutate_err = Some(e.into());
                break 'mutate;
            }
            for cfg in &servers {
                if let Err(e) = h.register_server(cfg.clone()).await {
                    mutate_err = Some(e.into());
                    break 'mutate;
                }
                registered.push(cfg.name().to_string());
            }
        }
        // skills 注册：复用既有 clone（refresh 透传）；失败降级（返回 Vec、不抛）→ 不触发回滚。
        // 捕获已注册 skill 名供成功日志带计数（观测信号；stage 的「降级不抛」契约见 #49）。
        staged_skills = stage_marketplace_skills(
            &marketplace,
            &source,
            registry,
            home,
            MarketplaceStageOptions {
                plugin_filter: Some(&filter),
                refresh: options.refresh,
                timeout: Some(timeout),
                env,
                ..Default::default()
            },
        )
        .await;
    }

    if let Some(e) = mutate_err {
        // 精确回滚：仅注销本次新增 skill（不在 skills_before）、仅摘除本次新增 server（不在 owned）；再上抛。
        for name in plugin_skill_names(registry, &marketplace, &plugin) {
            if !skills_before.contains(&name) {
                registry.unregister(&name);
            }
        }
        if let Some(h) = hooks {
            for sname in &registered {
                if owned.contains(sname) {
                    continue; // 自有 server（重装前已挂）：保留，不误摘。
                }
                if let Err(re) = h.remove_server(sname).await {
                    tracing::warn!(server = %sname, error = %re, "install rollback: remove_server failed");
                }
            }
        }
        return Err(e);
    }

    // ⑧：★最后写账本（仅全成功）
    let bundled_names: Vec<String> = servers.iter().map(|c| c.name().to_string()).collect();
    let resolved_version = options
        .version
        .map(String::from)
        .or_else(|| resolve_plugin_version(entry, &plugin_manifest, version_fallback.as_deref()));
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

    let mut extra: Map<String, Value> = Map::new();
    extra.insert("scope".into(), Value::String(scope.to_string()));
    if let Some(pp) = options.project_path.filter(|s| !s.is_empty()) {
        extra.insert(
            "projectPath".into(),
            Value::String(Path::new(pp).to_string_lossy().into_owned()),
        );
    }
    if let Some(v) = &resolved_version {
        extra.insert("version".into(), Value::String(v.clone()));
    }
    if let Some(sha) = &version_fallback {
        extra.insert("commitSha".into(), Value::String(sha.clone()));
    }
    extra.insert("installedAt".into(), Value::String(now.clone()));
    extra.insert("lastUpdated".into(), Value::String(now));

    let record = InstalledPluginRecord {
        install_path: Some(plugin_root.to_string_lossy().into_owned()),
        bundled_mcp_servers: bundled_names,
        extra,
    };

    let pid = plugin_id.to_string();
    let scope_owned = scope.to_string();
    let put_record = record.clone();
    // 多 scope 数组：替换同 scope 记录、保留其它 scope（v0.2.1 常见单元素）。
    update_installed_plugins(
        move |data: &mut InstalledPluginsFile| {
            let plugins = &mut data.account.plugins;
            let mut kept: Vec<InstalledPluginRecord> = plugins
                .get(&pid)
                .map(|recs| {
                    recs.iter()
                        .filter(|r| {
                            r.extra.get("scope").and_then(Value::as_str)
                                != Some(scope_owned.as_str())
                        })
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            kept.push(put_record);
            plugins.insert(pid, kept);
        },
        Some(home),
        env,
    )?;
    tracing::info!(
        plugin = plugin_id,
        skills_staged = staged_skills.len(),
        "installed plugin"
    );
    Ok(record)
}

/// 卸载单个 plugin（删 installPath 树 + 注销 skills + 级联 stop+remove bundled server + 删账本记录）/ Uninstall。
///
/// `scope=None` 删该 id 全部记录；指定 scope 仅删该 scope 记录。未安装 / 无匹配 → `false`（no-op）。
///
/// ⚠️ **相对源 plugin 的删除范围**：source 为相对路径时，其 `installPath` 位于**共享 catalog clone 内**
/// （`<home>/marketplace/<mp>/.../<plugin>`），[`safe_rmtree`] 删的是该 marketplace 共享 git 工作树的子目录
/// （与兄弟 `gc_plugins` 同一 `safe_rmtree` 语义，非本函数新引入）；后续 `marketplace refresh` 遇脏树会 fallback
/// 全量重 clone 干净恢复。即「删 plugin 子树」可能动到共享 catalog——勿误判为仅删独立 `.plugins/` 外部树。
///
/// # Errors
/// id 非法 / `remove_server` 失败 / 账本写失败 → [`PluginInstallError`]。
pub async fn uninstall_plugin(
    plugin_id: &str,
    registry: &mut SkillRegistry,
    home: &Path,
    options: UninstallOptions<'_>,
    hooks: Option<&dyn McpInstallHooks>,
) -> Result<bool, PluginInstallError> {
    let (plugin, marketplace) = split_plugin_id(plugin_id)?;
    let env = options.env;
    let installed = load_installed_plugins(Some(home), env);
    let records = match installed.account.plugins.get(plugin_id) {
        Some(r) if !r.is_empty() => r.clone(),
        _ => {
            tracing::info!(plugin = plugin_id, "uninstall: not installed (no-op)");
            return Ok(false);
        }
    };

    let targeted: Vec<&InstalledPluginRecord> = records
        .iter()
        .filter(|r| {
            options.scope.is_none() || r.extra.get("scope").and_then(Value::as_str) == options.scope
        })
        .collect();
    if targeted.is_empty() {
        tracing::info!(plugin = plugin_id, scope = ?options.scope, "uninstall: no record in scope (no-op)");
        return Ok(false);
    }

    let mut bundled: Vec<String> = Vec::new();
    for rec in &targeted {
        if let Some(ip) = rec.install_path.as_deref().filter(|s| !s.is_empty()) {
            safe_rmtree(Path::new(ip), home);
        }
        bundled.extend(rec.bundled_mcp_servers.iter().cloned());
    }

    for name in plugin_skill_names(registry, &marketplace, &plugin) {
        registry.unregister(&name);
    }

    if !options.keep_servers {
        if let Some(h) = hooks {
            for sname in &bundled {
                h.remove_server(sname).await?;
            }
        }
    }

    let pid = plugin_id.to_string();
    let scope_owned = options.scope.map(String::from);
    update_installed_plugins(
        move |data: &mut InstalledPluginsFile| {
            let plugins = &mut data.account.plugins;
            match &scope_owned {
                None => {
                    plugins.shift_remove(&pid);
                }
                Some(sc) => {
                    let remaining: Vec<InstalledPluginRecord> = plugins
                        .get(&pid)
                        .map(|recs| {
                            recs.iter()
                                .filter(|r| {
                                    r.extra.get("scope").and_then(Value::as_str)
                                        != Some(sc.as_str())
                                })
                                .cloned()
                                .collect()
                        })
                        .unwrap_or_default();
                    if remaining.is_empty() {
                        plugins.shift_remove(&pid);
                    } else {
                        plugins.insert(pid, remaining);
                    }
                }
            }
        },
        Some(home),
        env,
    )?;
    tracing::info!(plugin = plugin_id, "uninstalled plugin");
    Ok(true)
}

/// 禁用单个 plugin = 整 plugin 下线（§4.3 决策 #6）/ Disable = take the whole plugin offline。
///
/// ① 写 `enabledPlugins[id]=false`；② 停并摘除其 bundled MCP server；③ 隐藏 skills（mark_orphan，物化层不动）。
/// 区别于 [`uninstall_plugin`]：disable 留 installed 记录、可经 [`enable_plugin`] 一键回滚。
///
/// ⚠️ **scope 契约**：`scope` 须与安装 scope 一致（调用方从上下文传），否则 `enabledPlugins[id]=false` 写错层、
/// 与 live 态背离。真值已在账本 `installed_plugins.json` 每条 record 的 `scope` 内——**CLI 接线层（#48）应据
/// `record.scope` 解析后再传**消除 footgun。本层刻意**不**自动回查（账本可含多 scope 记录、回查有歧义；保 Python
/// parity，由调用方决策）。⚠️ **非原子**：先写 settings 再摘 server / orphan skill；`remove_server` 抛错留半态——靠
/// reconcile 兜底。
///
/// # Errors
/// id 非法 / settings 写失败 / `remove_server` 失败 → [`PluginInstallError`]。
pub async fn disable_plugin(
    plugin_id: &str,
    registry: &mut SkillRegistry,
    home: &Path,
    options: DisableOptions<'_>,
    hooks: Option<&dyn McpInstallHooks>,
) -> Result<(), PluginInstallError> {
    let (plugin, marketplace) = split_plugin_id(plugin_id)?;
    let scope = options.scope.unwrap_or("user");
    write_enabled_plugin(plugin_id, false, scope, options.project_path, options.env)?;
    if let Some(h) = hooks {
        let mut names: Vec<String> = bundled_servers_of(home, plugin_id, options.env)
            .into_iter()
            .collect();
        names.sort();
        for sname in names {
            h.remove_server(&sname).await?;
        }
    }
    for name in plugin_skill_names(registry, &marketplace, &plugin) {
        registry.mark_orphan(&name);
    }
    tracing::info!(
        plugin = plugin_id,
        scope,
        "disabled plugin (servers detached, skills orphaned)"
    );
    Ok(())
}

/// 启用单个 plugin（廉价复原，**无需重 clone/重装**）/ Enable = cheap restore (no re-clone)。
///
/// ① 从物化记录的 `installPath` 重解析 bundled servers → **★冲突预检（先于 settings 写 → enable 原子）**；
/// ② 写 `enabledPlugins[id]=true`；③ 复活 skills（re-stage：register_or_update 翻活孤儿，refresh=false 复用
/// clone）；④ 重挂 servers。未安装 → 拒。
///
/// ⚠️ **scope 契约**：同 [`disable_plugin`]。
///
/// # Errors
/// id 非法 / 未安装 / manifest 畸形 / 冲突 / settings 写失败 / 注入失败 → [`PluginInstallError`]。
pub async fn enable_plugin(
    plugin_id: &str,
    registry: &mut SkillRegistry,
    home: &Path,
    options: EnableOptions<'_>,
    hooks: Option<&dyn McpInstallHooks>,
) -> Result<(), PluginInstallError> {
    let (plugin, marketplace) = split_plugin_id(plugin_id)?;
    let scope = options.scope.unwrap_or("user");
    let timeout = options.timeout.unwrap_or(DEFAULT_GIT_TIMEOUT);
    let env = options.env;
    let installed = load_installed_plugins(Some(home), env);
    let records = match installed.account.plugins.get(plugin_id) {
        Some(r) if !r.is_empty() => r.clone(),
        _ => {
            return Err(PluginInstallError::Precondition(format!(
                "plugin {plugin_id:?} not installed; cannot enable (run 'plugin install' first)"
            )))
        }
    };

    // ① 重解析 bundled servers（不重 clone，从记录 installPath 读）+ 冲突预检（零持久化变更前）。
    let mut servers: Vec<MCPServerConfig> = Vec::new();
    for rec in &records {
        if let Some(ip) = rec.install_path.as_deref().filter(|s| !s.is_empty()) {
            servers.extend(load_bundled_servers(Path::new(ip))?);
        }
    }
    let owned = bundled_servers_of(home, plugin_id, env);
    let existing = hooks
        .map(McpInstallHooks::existing_server_names)
        .unwrap_or_default();
    conflict_check(&servers, &existing, &owned)?;

    // ② 写 enabledPlugins[id]=true（冲突预检通过后）。
    write_enabled_plugin(plugin_id, true, scope, options.project_path, env)?;

    // ③ 复活 skills（re-stage：register_or_update 翻活孤儿；复用既有 clone）。
    let (source, _commit_sha) = resolve_marketplace_source(&marketplace, home, env)?;
    let filter: HashSet<String> = std::iter::once(plugin.clone()).collect();
    stage_marketplace_skills(
        &marketplace,
        &source,
        registry,
        home,
        MarketplaceStageOptions {
            plugin_filter: Some(&filter),
            refresh: false,
            timeout: Some(timeout),
            env,
            ..Default::default()
        },
    )
    .await;

    // ④ 重挂 servers——**原子回滚**（#94 点 4 / §10.6 回滚契约）：与 install 把账本写在最后不同，enable 先写
    //    `enabledPlugins=true`（步骤 ②）再挂 server，故 `register_server` 失败会留"标记已启用但 server 未挂"的
    //    半态——Sub-B 的 boot 恢复会据此复活半装 plugin。失败 → 摘除本次已挂 server + 重新 orphan 本 plugin
    //    skills + 回写 `enabledPlugins=false`（确定性禁用末态，优于精确还原前值：无论前值如何，失败即落定禁用）。
    if let Some(h) = hooks {
        let mut remounted: Vec<String> = Vec::new();
        for cfg in servers {
            let sname = cfg.name().to_string();
            if let Err(e) = h.register_server(cfg).await {
                for done in &remounted {
                    if let Err(re) = h.remove_server(done).await {
                        tracing::warn!(server = %done, error = %re, "enable rollback: remove_server failed");
                    }
                }
                for name in plugin_skill_names(registry, &marketplace, &plugin) {
                    registry.mark_orphan(&name);
                }
                // ⚠️ 已知窄边界：若**这一步**回写也失败（settings I/O 在回滚中途再挂），账本停在步骤 ② 的
                //    `enabledPlugins=true`，server 已摘 + skills 已 orphan，末态即降级前要消灭的半启用态。属
                //    "降级之上的再降级"，不再有更可靠的兜底——Sub-B 的 boot reconcile 须容此残窗（勿假设回滚后账本
                //    必为 false）。原 hook 错（`e`）优先上抛；回写错仅 WARN（避免淹没根因）。
                if let Err(we) =
                    write_enabled_plugin(plugin_id, false, scope, options.project_path, env)
                {
                    tracing::warn!(plugin = plugin_id, error = %we, "enable rollback: re-disable write failed (ledger may残留 enabled=true)");
                }
                return Err(e.into());
            }
            remounted.push(sname);
        }
    }
    tracing::info!(
        plugin = plugin_id,
        scope,
        "enabled plugin (skills recovered, servers remounted)"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use std::sync::Mutex;
    use tempfile::TempDir;

    use crate::settings::store::{installed_plugins_path, update_known_marketplaces};

    // ---- 纯单元 / pure ------------------------------------------------------
    #[test]
    fn split_plugin_id_valid_and_invalid() {
        assert_eq!(
            split_plugin_id("audit@acme").unwrap(),
            ("audit".to_string(), "acme".to_string())
        );
        assert!(matches!(
            split_plugin_id("no-at-sign"),
            Err(PluginInstallError::Precondition(_))
        ));
        assert!(split_plugin_id("Bad_Name@acme").is_err());
    }

    #[test]
    fn settings_path_for_scope_maps_and_rejects() {
        let env: EnvMap =
            std::iter::once(("XDG_CONFIG_HOME".to_string(), "/tmp/x".to_string())).collect();
        let (_p, s) = settings_path_for_scope("user", None, Some(&env)).unwrap();
        assert_eq!(s, SettingsScope::User);
        let (_p, s) = settings_path_for_scope("project", Some("/wd"), Some(&env)).unwrap();
        assert_eq!(s, SettingsScope::Project);
        let (_p, s) = settings_path_for_scope("local", Some("/wd"), Some(&env)).unwrap();
        assert_eq!(s, SettingsScope::Local);
        // project/local 缺 workdir → 拒。
        assert!(settings_path_for_scope("project", None, Some(&env)).is_err());
        // managed/policy/未知 → 拒。
        assert!(settings_path_for_scope("policy", None, Some(&env)).is_err());
        assert!(settings_path_for_scope("managed", None, Some(&env)).is_err());
    }

    #[test]
    fn conflict_check_foreign_vs_owned() {
        let servers = vec![
            serde_json::from_value::<MCPServerConfig>(serde_json::json!({
                "type": "stdio", "name": "dup", "server_parameters": {"command": "go"}
            }))
            .unwrap(),
        ];
        let existing: HashSet<String> = std::iter::once("dup".to_string()).collect();
        // 外来同名 → 硬抛。
        assert!(conflict_check(&servers, &existing, &HashSet::new()).is_err());
        // 自有同名 → 幂等放行。
        let owned: HashSet<String> = std::iter::once("dup".to_string()).collect();
        assert!(conflict_check(&servers, &existing, &owned).is_ok());
        // 不存在 → 放行。
        assert!(conflict_check(&servers, &HashSet::new(), &HashSet::new()).is_ok());
    }

    // ---- 集成：fake catalog 构造 / integration fixtures ---------------------
    // staging 测试同款 git 子进程；测试需本机 git（与既有 staging marketplace 测试一致假设）。
    fn git(args: &[&str], cwd: &Path) {
        let out = Command::new("git")
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

    /// 构造源 git 仓库（marketplace.json + audit plugin：1 skill + 给定 bundled server 名集合），返回 git
    /// source。`servers` 空 = 无 server；文件名即挂载序（见 [`enumerate_bundled_server_files`](crate::skills::manifest)
    /// 的 `sort`）/ build a source repo with the given bundled server names。
    fn build_source_repo_with_servers(repo: &Path, servers: &[&str]) -> Value {
        fs::create_dir_all(repo.join(".tfrobot-plugin")).unwrap();
        fs::write(
            repo.join(".tfrobot-plugin/marketplace.json"),
            r#"{"plugins": [{"name": "audit", "source": "./plugins/audit"}]}"#,
        )
        .unwrap();
        let skill = repo.join("plugins/audit/skills/code-review");
        fs::create_dir_all(&skill).unwrap();
        fs::write(
            skill.join("SKILL.md"),
            "---\nname: code-review\ndescription: review code\n---\nbody",
        )
        .unwrap();
        if !servers.is_empty() {
            let sd = repo.join("plugins/audit/mcp-servers");
            fs::create_dir_all(&sd).unwrap();
            for name in servers {
                fs::write(
                    sd.join(format!("{name}.json")),
                    format!(
                        r#"{{"type":"stdio","name":"{name}","server_parameters":{{"command":"node"}}}}"#
                    ),
                )
                .unwrap();
            }
        }
        git(&["init", "-q"], repo);
        git(&["add", "-A"], repo);
        git(&["commit", "-qm", "init"], repo);
        serde_json::json!({"type": "git", "url": format!("file://{}", repo.display())})
    }

    /// 读 user settings 的 `enabledPlugins[<pid>]` 布尔值（解析 JSON，避免脆弱的子串断言）/ read the enabled flag。
    fn enabled_flag(env: &EnvMap, pid: &str) -> Option<bool> {
        let txt = fs::read_to_string(user_settings_path(Some(env))).ok()?;
        let v: Value = serde_json::from_str(&txt).ok()?;
        v.get("enabledPlugins")?.get(pid)?.as_bool()
    }

    /// 预 clone catalog 进 home + 写 known_marketplaces.json；返回 (home, env, source)。
    async fn setup_installed_catalog(tmp: &TempDir, with_server: bool) -> (PathBuf, EnvMap, Value) {
        setup_installed_catalog_servers(tmp, if with_server { &["audit-mcp"] } else { &[] }).await
    }

    /// 同上但显式给定 bundled server 名集合 / same but with explicit bundled server names。
    async fn setup_installed_catalog_servers(
        tmp: &TempDir,
        servers: &[&str],
    ) -> (PathBuf, EnvMap, Value) {
        let repo = tmp.path().join("repo");
        let source = build_source_repo_with_servers(&repo, servers);
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let env: EnvMap = EnvMap::new();
        // 预 clone catalog（throwaway registry）：install 要求 catalog 已存在。
        let mut throwaway = SkillRegistry::new();
        stage_marketplace_skills(
            "acme",
            &source,
            &mut throwaway,
            &home,
            MarketplaceStageOptions {
                refresh: true,
                ..Default::default()
            },
        )
        .await;
        // 写 known_marketplaces.json（install 经此解析 mp source）。
        let src_clone = source.clone();
        update_known_marketplaces(
            move |file| {
                file.account.marketplaces.insert(
                    "acme".to_string(),
                    crate::settings::reconciler::KnownMarketplaceEntry {
                        source: src_clone,
                        extra: Map::new(),
                    },
                );
            },
            Some(&home),
            Some(&env),
        )
        .unwrap();
        (home, env, source)
    }

    // ---- 注入回调替身 / hook fakes ------------------------------------------
    struct RecordingHooks {
        existing: HashSet<String>,
        registered: Mutex<Vec<String>>,
        removed: Mutex<Vec<String>>,
        fail_register: Option<String>,
    }
    impl RecordingHooks {
        fn new() -> Self {
            Self {
                existing: HashSet::new(),
                registered: Mutex::new(Vec::new()),
                removed: Mutex::new(Vec::new()),
                fail_register: None,
            }
        }
        fn with_existing(mut self, names: &[&str]) -> Self {
            self.existing = names.iter().map(|s| s.to_string()).collect();
            self
        }
        fn failing_register(mut self, name: &str) -> Self {
            self.fail_register = Some(name.to_string());
            self
        }
    }
    #[async_trait]
    impl McpInstallHooks for RecordingHooks {
        fn existing_server_names(&self) -> HashSet<String> {
            self.existing.clone()
        }
        async fn register_server(&self, cfg: MCPServerConfig) -> Result<(), McpHookError> {
            if self.fail_register.as_deref() == Some(cfg.name()) {
                return Err(McpHookError(format!("boom on {}", cfg.name())));
            }
            self.registered.lock().unwrap().push(cfg.name().to_string());
            Ok(())
        }
        async fn remove_server(&self, name: &str) -> Result<(), McpHookError> {
            self.removed.lock().unwrap().push(name.to_string());
            Ok(())
        }
    }

    fn ledger_records(home: &Path, env: &EnvMap, pid: &str) -> Vec<InstalledPluginRecord> {
        load_installed_plugins(Some(home), Some(env))
            .account
            .plugins
            .get(pid)
            .cloned()
            .unwrap_or_default()
    }

    // ---- install ------------------------------------------------------------
    #[tokio::test]
    async fn install_ledger_only_registers_and_records() {
        let tmp = TempDir::new().unwrap();
        let (home, env, _src) = setup_installed_catalog(&tmp, true).await;
        let mut reg = SkillRegistry::new();
        let record = install_plugin(
            "audit@acme",
            &mut reg,
            &home,
            InstallOptions {
                env: Some(&env),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
        // skill 注册。
        assert!(reg.resolve("audit:code-review").is_some());
        // 账本记录：scope/installPath/bundledMcpServers/installedAt。
        assert_eq!(record.bundled_mcp_servers, vec!["audit-mcp".to_string()]);
        assert!(record.install_path.is_some());
        assert_eq!(
            record.extra.get("scope").and_then(Value::as_str),
            Some("user")
        );
        assert!(record.extra.contains_key("installedAt"));
        let recs = ledger_records(&home, &env, "audit@acme");
        assert_eq!(recs.len(), 1);
        assert!(installed_plugins_path(Some(&home), Some(&env)).is_file());
    }

    #[tokio::test]
    async fn install_registers_servers_via_hooks() {
        let tmp = TempDir::new().unwrap();
        let (home, env, _src) = setup_installed_catalog(&tmp, true).await;
        let mut reg = SkillRegistry::new();
        let hooks = RecordingHooks::new();
        install_plugin(
            "audit@acme",
            &mut reg,
            &home,
            InstallOptions {
                env: Some(&env),
                ..Default::default()
            },
            Some(&hooks),
        )
        .await
        .unwrap();
        assert_eq!(
            *hooks.registered.lock().unwrap(),
            vec!["audit-mcp".to_string()]
        );
    }

    #[tokio::test]
    async fn install_foreign_name_conflict_is_hard_fail_zero_change() {
        let tmp = TempDir::new().unwrap();
        let (home, env, _src) = setup_installed_catalog(&tmp, true).await;
        let mut reg = SkillRegistry::new();
        // existing 含 audit-mcp 且非自有 → 冲突硬抛。
        let hooks = RecordingHooks::new().with_existing(&["audit-mcp"]);
        let err = install_plugin(
            "audit@acme",
            &mut reg,
            &home,
            InstallOptions {
                env: Some(&env),
                ..Default::default()
            },
            Some(&hooks),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, PluginInstallError::Conflict(_)));
        // 零变更：未注册 server、未注册 skill、未写账本。
        assert!(hooks.registered.lock().unwrap().is_empty());
        assert!(reg.resolve("audit:code-review").is_none());
        assert!(ledger_records(&home, &env, "audit@acme").is_empty());
    }

    #[tokio::test]
    async fn install_rollback_on_register_failure() {
        let tmp = TempDir::new().unwrap();
        let (home, env, _src) = setup_installed_catalog(&tmp, true).await;
        let mut reg = SkillRegistry::new();
        let hooks = RecordingHooks::new().failing_register("audit-mcp");
        let err = install_plugin(
            "audit@acme",
            &mut reg,
            &home,
            InstallOptions {
                env: Some(&env),
                ..Default::default()
            },
            Some(&hooks),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, PluginInstallError::Hook(_)));
        // register 在 stage 之前失败 → skills 未注册、账本未写；registered 为空（首个就失败）。
        assert!(reg.resolve("audit:code-review").is_none());
        assert!(ledger_records(&home, &env, "audit@acme").is_empty());
    }

    // ---- uninstall ----------------------------------------------------------
    #[tokio::test]
    async fn uninstall_removes_skills_ledger_and_servers() {
        let tmp = TempDir::new().unwrap();
        let (home, env, _src) = setup_installed_catalog(&tmp, true).await;
        let mut reg = SkillRegistry::new();
        let hooks = RecordingHooks::new();
        install_plugin(
            "audit@acme",
            &mut reg,
            &home,
            InstallOptions {
                env: Some(&env),
                ..Default::default()
            },
            Some(&hooks),
        )
        .await
        .unwrap();
        assert!(reg.resolve("audit:code-review").is_some());

        let removed = uninstall_plugin(
            "audit@acme",
            &mut reg,
            &home,
            UninstallOptions {
                env: Some(&env),
                ..Default::default()
            },
            Some(&hooks),
        )
        .await
        .unwrap();
        assert!(removed);
        assert!(reg.resolve("audit:code-review").is_none());
        assert!(ledger_records(&home, &env, "audit@acme").is_empty());
        assert_eq!(
            *hooks.removed.lock().unwrap(),
            vec!["audit-mcp".to_string()]
        );
        // no-op 再卸载。
        let again = uninstall_plugin(
            "audit@acme",
            &mut reg,
            &home,
            UninstallOptions {
                env: Some(&env),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
        assert!(!again);
    }

    #[tokio::test]
    async fn uninstall_keep_servers_skips_remove() {
        let tmp = TempDir::new().unwrap();
        let (home, env, _src) = setup_installed_catalog(&tmp, true).await;
        let mut reg = SkillRegistry::new();
        let hooks = RecordingHooks::new();
        install_plugin(
            "audit@acme",
            &mut reg,
            &home,
            InstallOptions {
                env: Some(&env),
                ..Default::default()
            },
            Some(&hooks),
        )
        .await
        .unwrap();
        uninstall_plugin(
            "audit@acme",
            &mut reg,
            &home,
            UninstallOptions {
                keep_servers: true,
                env: Some(&env),
                ..Default::default()
            },
            Some(&hooks),
        )
        .await
        .unwrap();
        assert!(hooks.removed.lock().unwrap().is_empty());
    }

    // ---- enable / disable ---------------------------------------------------
    #[tokio::test]
    async fn disable_then_enable_toggles_flag_and_skills() {
        let tmp = TempDir::new().unwrap();
        let (home, env, _src) = setup_installed_catalog(&tmp, true).await;
        // enabledPlugins 写 user scope → 经 env 控制 XDG_CONFIG_HOME。
        let cfg_home = tmp.path().join("cfg");
        fs::create_dir_all(&cfg_home).unwrap();
        let mut env = env;
        env.insert(
            "XDG_CONFIG_HOME".to_string(),
            cfg_home.to_string_lossy().into_owned(),
        );

        let mut reg = SkillRegistry::new();
        let hooks = RecordingHooks::new();
        install_plugin(
            "audit@acme",
            &mut reg,
            &home,
            InstallOptions {
                env: Some(&env),
                ..Default::default()
            },
            Some(&hooks),
        )
        .await
        .unwrap();
        assert!(reg.resolve("audit:code-review").is_some());

        // disable：enabledPlugins=false + 摘 server + orphan skill。
        disable_plugin(
            "audit@acme",
            &mut reg,
            &home,
            DisableOptions {
                env: Some(&env),
                ..Default::default()
            },
            Some(&hooks),
        )
        .await
        .unwrap();
        assert!(reg.resolve("audit:code-review").is_none()); // orphaned（resolve 排除孤儿）
        assert_eq!(
            *hooks.removed.lock().unwrap(),
            vec!["audit-mcp".to_string()]
        );
        let user_settings = user_settings_path(Some(&env));
        let txt = fs::read_to_string(&user_settings).unwrap();
        assert!(txt.contains("\"audit@acme\""));
        assert!(txt.contains("false"));

        // enable：enabledPlugins=true + 复活 skill + 重挂 server。
        let hooks2 = RecordingHooks::new();
        enable_plugin(
            "audit@acme",
            &mut reg,
            &home,
            EnableOptions {
                env: Some(&env),
                ..Default::default()
            },
            Some(&hooks2),
        )
        .await
        .unwrap();
        assert!(reg.resolve("audit:code-review").is_some()); // 复活
        assert_eq!(
            *hooks2.registered.lock().unwrap(),
            vec!["audit-mcp".to_string()]
        );
        let txt = fs::read_to_string(&user_settings).unwrap();
        assert!(txt.contains("true"));
    }

    #[tokio::test]
    async fn enable_rollback_on_register_failure_redisables_and_reorphans() {
        // #94 点 4 回滚契约回归：enable 先写 enabledPlugins=true 再挂 server；register_server 失败须原子回滚
        // （回写 false + 重 orphan skills），避免半启用态被 boot 恢复复活。
        let tmp = TempDir::new().unwrap();
        let (home, env, _src) = setup_installed_catalog(&tmp, true).await;
        let cfg_home = tmp.path().join("cfg");
        fs::create_dir_all(&cfg_home).unwrap();
        let mut env = env;
        env.insert(
            "XDG_CONFIG_HOME".to_string(),
            cfg_home.to_string_lossy().into_owned(),
        );

        let mut reg = SkillRegistry::new();
        let hooks = RecordingHooks::new();
        install_plugin(
            "audit@acme",
            &mut reg,
            &home,
            InstallOptions {
                env: Some(&env),
                ..Default::default()
            },
            Some(&hooks),
        )
        .await
        .unwrap();
        // disable → 进入禁用态（enabledPlugins=false、skill orphaned）。
        disable_plugin(
            "audit@acme",
            &mut reg,
            &home,
            DisableOptions {
                env: Some(&env),
                ..Default::default()
            },
            Some(&hooks),
        )
        .await
        .unwrap();
        assert!(reg.resolve("audit:code-review").is_none());

        // enable 但 register_server 注定失败 → Err(Hook) + 原子回滚。
        let failing = RecordingHooks::new().failing_register("audit-mcp");
        let err = enable_plugin(
            "audit@acme",
            &mut reg,
            &home,
            EnableOptions {
                env: Some(&env),
                ..Default::default()
            },
            Some(&failing),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, PluginInstallError::Hook(_)));

        // 回滚末态：skill 重新 orphaned（非活跃）+ enabledPlugins 落定为 false（非半启用 true）。
        assert!(reg.resolve("audit:code-review").is_none());
        assert_eq!(enabled_flag(&env, "audit@acme"), Some(false));
    }

    #[tokio::test]
    async fn enable_rollback_removes_already_remounted_servers() {
        // #94 点 4 回滚契约：多 server 时 register_server 中途失败，须摘除**本次已挂**的 server（不只回写账本）。
        // 文件名 sort 决定挂载序（enumerate_bundled_server_files）：alpha-mcp 先挂成功、zeta-mcp 失败 → 回滚摘 alpha-mcp。
        let tmp = TempDir::new().unwrap();
        let (home, env, _src) =
            setup_installed_catalog_servers(&tmp, &["alpha-mcp", "zeta-mcp"]).await;
        let cfg_home = tmp.path().join("cfg");
        fs::create_dir_all(&cfg_home).unwrap();
        let mut env = env;
        env.insert(
            "XDG_CONFIG_HOME".to_string(),
            cfg_home.to_string_lossy().into_owned(),
        );

        let mut reg = SkillRegistry::new();
        let hooks = RecordingHooks::new();
        install_plugin(
            "audit@acme",
            &mut reg,
            &home,
            InstallOptions {
                env: Some(&env),
                ..Default::default()
            },
            Some(&hooks),
        )
        .await
        .unwrap();
        disable_plugin(
            "audit@acme",
            &mut reg,
            &home,
            DisableOptions {
                env: Some(&env),
                ..Default::default()
            },
            Some(&hooks),
        )
        .await
        .unwrap();

        // enable：第二个 server（zeta-mcp）注册失败 → 回滚摘除已挂的第一个（alpha-mcp）。
        let failing = RecordingHooks::new().failing_register("zeta-mcp");
        let err = enable_plugin(
            "audit@acme",
            &mut reg,
            &home,
            EnableOptions {
                env: Some(&env),
                ..Default::default()
            },
            Some(&failing),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, PluginInstallError::Hook(_)));
        // 已挂的 alpha-mcp 被回滚摘除；zeta-mcp 因 register 失败未进 remounted，故不在 removed。
        assert_eq!(
            *failing.removed.lock().unwrap(),
            vec!["alpha-mcp".to_string()]
        );
        // 账本回写 false（确定性禁用末态）。
        assert_eq!(enabled_flag(&env, "audit@acme"), Some(false));
    }

    #[tokio::test]
    async fn enable_not_installed_is_precondition_error() {
        let tmp = TempDir::new().unwrap();
        let (home, env, _src) = setup_installed_catalog(&tmp, false).await;
        let mut reg = SkillRegistry::new();
        let err = enable_plugin(
            "audit@acme",
            &mut reg,
            &home,
            EnableOptions {
                env: Some(&env),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, PluginInstallError::Precondition(_)));
    }
}
