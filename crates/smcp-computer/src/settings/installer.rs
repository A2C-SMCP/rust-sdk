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
    self, load_installed_plugins, load_installed_plugins_intent, load_known_marketplaces,
    update_installed_plugins, update_installed_plugins_intent, InstalledPluginsFile,
    SettingsStoreError,
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
    /// project/local scope 的锚定目录（#98：进程 cwd；`user` scope 不需要）/ anchor dir for project/local。
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
    /// project/local scope 的锚定目录（#98：进程 cwd；`user` scope 不需要）/ anchor dir for project/local。
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
    /// project/local scope 的锚定目录（#98：进程 cwd；`user` scope 不需要）/ anchor dir for project/local。
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
                    "scope {scope:?} requires project_path (process cwd anchor)"
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

/// 对指定 scope 的 `enabledPlugins[<plugin_id>]` 施加一次写更新（持锁原子 RMW）/ apply one enabledPlugins write。
///
/// 复用 store 旁车锁 + 原子写 + scope 的 [`load_settings_file`] / [`apply_write`]（仅改该 key、不毁兄弟）。
/// settings.json 是人编意图层 → 无写保护头（[`store::atomic_write_settings_json`]）。
fn apply_enabled_plugin_write(
    plugin_id: &str,
    wv: WriteValue,
    scope: &str,
    project_path: Option<&str>,
    env: Option<&EnvMap>,
) -> Result<bool, PluginInstallError> {
    let (path, scope_enum) = settings_path_for_scope(scope, project_path, env)?;
    let mut inner: BTreeMap<String, WriteValue> = BTreeMap::new();
    inner.insert(plugin_id.to_string(), wv);
    let mut updates: BTreeMap<String, WriteValue> = BTreeMap::new();
    updates.insert("enabledPlugins".to_string(), WriteValue::Object(inner));

    // 外层 `?`：锁失败（[`SettingsStoreError`]）；内层 `?`：写 I/O（[`io::Error`]）。
    // 返回 `changed`＝enabledPlugins 内容**真变**（#115 R1，方案 A）：内容未变（幂等 re-enable /
    // 重复 disable）→ **跳过写盘**（不扰 mtime、无文件 churn），供 Computer 层据「实际写盘结果」
    // **只在真变时** bump config revision + 通知 robot——false-negative 安全（写了就是真变）。
    let changed = store::with_settings_lock(&path, || -> io::Result<bool> {
        let (existing, _errors) = load_settings_file(&path, scope_enum);
        let updated = apply_write(&existing, &updates);
        if updated == existing {
            return Ok(false); // 内容未变 → 不触碰文件。
        }
        store::atomic_write_settings_json(&path, &Value::Object(updated))?;
        Ok(true)
    })??;
    Ok(changed)
}

/// 写 `enabledPlugins[<plugin_id>] = value`（enable/disable 用）/ Write the enable flag。
fn write_enabled_plugin(
    plugin_id: &str,
    value: bool,
    scope: &str,
    project_path: Option<&str>,
    env: Option<&EnvMap>,
) -> Result<bool, PluginInstallError> {
    apply_enabled_plugin_write(
        plugin_id,
        WriteValue::Set(Value::Bool(value)),
        scope,
        project_path,
        env,
    )
}

/// 删除 `enabledPlugins[<plugin_id>]` 键（uninstall 清启用意图条目，协议 §2.4）/ Delete the enable flag key。
///
/// 清除后避免「uninstall → reinstall」残留 `true` 令新装绕过 `installed_disabled` 直接激活。**仅当该键实际存在
/// 才写盘**——从未 enable 的 plugin 卸载时不触碰 settings.json（保 Computer 层 `env=None` 卸载对 `~/.config`
/// 无副作用，见 computer.rs enabledPlugins 测试约定）。
fn clear_enabled_plugin(
    plugin_id: &str,
    scope: &str,
    project_path: Option<&str>,
    env: Option<&EnvMap>,
) -> Result<(), PluginInstallError> {
    let (path, scope_enum) = settings_path_for_scope(scope, project_path, env)?;
    store::with_settings_lock(&path, || -> io::Result<()> {
        let (existing, _errors) = load_settings_file(&path, scope_enum);
        let present = existing
            .get("enabledPlugins")
            .and_then(|v| v.get(plugin_id))
            .is_some();
        if !present {
            return Ok(()); // 键不存在 → 无需写盘（不触碰文件）。
        }
        let mut inner: BTreeMap<String, WriteValue> = BTreeMap::new();
        inner.insert(plugin_id.to_string(), WriteValue::Delete);
        let mut updates: BTreeMap<String, WriteValue> = BTreeMap::new();
        updates.insert("enabledPlugins".to_string(), WriteValue::Object(inner));
        let updated = apply_write(&existing, &updates);
        store::atomic_write_settings_json(&path, &Value::Object(updated))
    })??;
    Ok(())
}

/// 写 `enabledPlugins[<plugin_id>] = true`**仅当该键在该 scope 缺席**（迁移用；不覆盖用户显式 true/false）/ set-if-absent。
fn set_enabled_true_if_absent(
    plugin_id: &str,
    scope: &str,
    project_path: Option<&str>,
    env: Option<&EnvMap>,
) -> Result<(), PluginInstallError> {
    let (path, scope_enum) = settings_path_for_scope(scope, project_path, env)?;
    store::with_settings_lock(&path, || -> io::Result<()> {
        let (existing, _errors) = load_settings_file(&path, scope_enum);
        let present = existing
            .get("enabledPlugins")
            .and_then(|v| v.get(plugin_id))
            .is_some();
        if present {
            return Ok(()); // 已有显式值（true / 用户 disable 的 false）→ 不覆盖。
        }
        let mut inner: BTreeMap<String, WriteValue> = BTreeMap::new();
        inner.insert(plugin_id.to_string(), WriteValue::Set(Value::Bool(true)));
        let mut updates: BTreeMap<String, WriteValue> = BTreeMap::new();
        updates.insert("enabledPlugins".to_string(), WriteValue::Object(inner));
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
// 物化原语（install 与账本重建共享）/ shared materialization primitives
// ---------------------------------------------------------------------------
/// 物化前置产物（install 与账本重建 [`materialize_plugin_record`] 共享）/ shared materialization output。
///
/// 由 [`materialize_plugin`] 产出：解析 marketplace 源 → 定位 plugin root（可 git clone）→ 载入 bundled
/// servers → 解析版本。承载 install 下游（冲突闸 / 注册 / 账本）与账本重建所需的全部字段；`entry` / `manifest`
/// / `plugin_manifest` 等 prologue-only 中间态不逃逸（`resolved_version` 已在 [`materialize_plugin`] 内先算）。
struct MaterializedPlugin {
    /// plugin 名（pid `@` 前段）/ plugin name。
    plugin: String,
    /// marketplace 名（pid `@` 后段）/ marketplace name。
    marketplace: String,
    /// marketplace git source（供 skills staging 复用）/ marketplace source。
    source: Value,
    /// plugin 物化落点（bundled server 从此解析 + 账本 `installPath`）/ install path。
    plugin_root: PathBuf,
    /// 解析出的 bundled MCP server 配置 / bundled MCP server configs。
    servers: Vec<MCPServerConfig>,
    /// 版本回退（git HEAD / catalog sha，账本 `commitSha`）/ version fallback sha。
    version_fallback: Option<String>,
    /// 最终解析版本（账本 `version`）/ resolved version。
    resolved_version: Option<String>,
}

/// 物化 plugin 前置：解析源 → 定位 root（`refresh=false` 离线复用既有 clone）→ 载入 bundled servers →
/// 解析版本 / Resolve source, locate root, load bundled servers, resolve version。
///
/// install 与账本重建（[`materialize_plugin_record`]）复用此原语，**杜绝两条物化路径漂移**——重建出的
/// `installPath` / bundled 名集必与 install 当初写入的一致。`strict_check=true` 时执行 §4.4 strict 冲突门
/// （install 用；账本重建免检，见 [`materialize_plugin_record`]）。
///
/// # Errors
/// marketplace 未添加 / catalog 未 clone / manifest 畸形 / plugin 不在清单 / 定位失败（源不可达）/ bundled
/// server JSON 畸形 / strict 冲突 → [`PluginInstallError`]。
#[allow(clippy::too_many_arguments)]
async fn materialize_plugin(
    plugin_id: &str,
    home: &Path,
    env: Option<&EnvMap>,
    refresh: bool,
    timeout: Duration,
    version_override: Option<&str>,
    strict_check: bool,
) -> Result<MaterializedPlugin, PluginInstallError> {
    let (plugin, marketplace) = split_plugin_id(plugin_id)?;
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
        refresh,
        timeout,
        env,
    )
    .await?;
    // plugin.json 读一次复用（strict 检测 + version 解析共用）/ read once, reuse。
    let plugin_manifest = read_plugin_metadata(&plugin_root);
    if strict_check {
        // strict mode 冲突检测（§4.4）：早检——挂 server / 注册 skill 前拦截，保证原子失败。
        check_strict_conflict(entry, &plugin_manifest)?;
    }
    let servers = load_bundled_servers(&plugin_root)?;
    // entry 借用自 manifest、不可逃逸 → 在此算 resolved_version（借用检查强制，非仅风格）。
    let resolved_version = version_override
        .map(String::from)
        .or_else(|| resolve_plugin_version(entry, &plugin_manifest, version_fallback.as_deref()));

    Ok(MaterializedPlugin {
        plugin,
        marketplace,
        source,
        plugin_root,
        servers,
        version_fallback,
        resolved_version,
    })
}

/// 由物化产物构造账本派生记录 + 写入（同 scope 替换、异 scope 保留数组合并）/ Build & upsert the ledger record.
///
/// install 与账本重建复用。`extra` 组装 scope/projectPath?/version?/commitSha?/installedAt/lastUpdated；
/// `installedAt` 取当前时刻（账本重建时即重建时刻，原值不可复原）。返回写入的记录。
///
/// # Errors
/// 账本写失败（锁 / I/O）→ [`PluginInstallError`]。
fn write_ledger_record(
    home: &Path,
    plugin_id: &str,
    scope: &str,
    project_path: Option<&str>,
    m: &MaterializedPlugin,
    env: Option<&EnvMap>,
) -> Result<InstalledPluginRecord, PluginInstallError> {
    let bundled_names: Vec<String> = m.servers.iter().map(|c| c.name().to_string()).collect();
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

    let mut extra: Map<String, Value> = Map::new();
    extra.insert("scope".into(), Value::String(scope.to_string()));
    if let Some(pp) = project_path.filter(|s| !s.is_empty()) {
        extra.insert(
            "projectPath".into(),
            Value::String(Path::new(pp).to_string_lossy().into_owned()),
        );
    }
    if let Some(v) = &m.resolved_version {
        extra.insert("version".into(), Value::String(v.clone()));
    }
    if let Some(sha) = &m.version_fallback {
        extra.insert("commitSha".into(), Value::String(sha.clone()));
    }
    extra.insert("installedAt".into(), Value::String(now.clone()));
    extra.insert("lastUpdated".into(), Value::String(now));

    let record = InstalledPluginRecord {
        install_path: Some(m.plugin_root.to_string_lossy().into_owned()),
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
    Ok(record)
}

/// 从 `installedPlugins` 意图为单个 plugin **重物化账本派生缓存**（`installPath` + `bundledMcpServers`）/ rebuild ledger record from intent.
///
/// v0.3.0 conformance §63（账本删除无损）：账本 `installed_plugins.json` 被外部删除/损坏后，boot / reconcile
/// 据 `installedPlugins` 意图重建其派生缓存，使 enabled plugin 的 bundled server 与归属重现。复用
/// `materialize_plugin`（`refresh=false` 离线复用既有 clone）+ `write_ledger_record`，故重建的 `installPath`
/// / bundled 名集与 install 当初一致。
///
/// **只写账本**：不写 `installedPlugins` 意图（本就是重建依据）、不写 `enabledPlugins`、不 stage skills（phase 1
/// [`recover_marketplace_skills`](crate::settings::recovery::recover_marketplace_skills) 负责）、不挂 server
/// （phase 2 经 hooks 负责）。**不得**以 `install_plugin(hooks=None)` 实现——install 末尾 `mark_orphan` 会反向
/// orphan phase 1 刚复活的 enabled skill。
///
/// **两条 intent-inherent 降级**：① 记录固定写 `scope="user"`（意图是扁平集、无 scope，原 project/local scope
/// 不可复原——与「跨重启可靠启用写 user scope」指引一致）；② `installedAt` 为重建时刻（原值丢失）。
/// **`strict_check=false`**（与 phase 1 `stage_one_plugin` 的再检查不对称是刻意）：pid 已在装机时过 strict，
/// `refresh=false` 复用同一 manifest，重建期不重复门控（避免 catalog 漂移令恢复误失败）。
///
/// # Errors
/// 见 `materialize_plugin` / `write_ledger_record`（源不可达 / manifest 畸形 / 账本写失败）。调用方（boot
/// 恢复 [`rematerialize_missing_ledger_records`](crate::settings::recovery::rematerialize_missing_ledger_records)）
/// 遇 `Err` 应降级记录、不阻断其余恢复。
pub async fn materialize_plugin_record(
    plugin_id: &str,
    home: &Path,
    env: Option<&EnvMap>,
    timeout: Duration,
) -> Result<InstalledPluginRecord, PluginInstallError> {
    let m = materialize_plugin(plugin_id, home, env, false, timeout, None, false).await?;
    write_ledger_record(home, plugin_id, "user", None, &m, env)
}

// ---------------------------------------------------------------------------
// install / uninstall / enable / disable
// ---------------------------------------------------------------------------
/// 显式安装单个 plugin = **物化 + 登记 `installed_disabled`，不激活**（v0.3.0，协议 §2.4）/ Install (staged, inactive)。
///
/// v0.3.0：install 与 enable 分离。install **只**物化并写声明式安装意图，**不**激活能力——其 SKILL 注册后即
/// `orphan`（不进 `get_skills`），bundled MCP server **不**挂、inputs **不**注入、**不**写 `enabledPlugins`
/// （absent = 未启用 → `installed_disabled`）。激活全部交给 [`enable_plugin`]。
///
/// 顺序：① 解析 id + mp source；② 要求 catalog 已 clone、读 marketplace.json 定位 entry；③ `locate_plugin_root`
/// 定位 plugin 根（必要时 clone）；④ strict 冲突检测 + [`load_bundled_servers`]（注册前畸形即抛）；⑤ **★冲突闸门**
/// （外来 MCP 同名硬抛、零变更——满足「install 拒绝 foreign name conflict」，虽本步不挂载）；⑥ stage skills →
/// 立即 `mark_orphan`（不投影）；⑦ **config-first 写 `installedPlugins` 全局安装意图**（权威）→ ⑧ 写账本
/// （派生缓存）。
///
/// # Errors
/// 见 [`PluginInstallError`]（冲突 / 前置 / manifest / 定位 / 意图 / 账本）。
pub async fn install_plugin(
    plugin_id: &str,
    registry: &mut SkillRegistry,
    home: &Path,
    options: InstallOptions<'_>,
    hooks: Option<&dyn McpInstallHooks>,
) -> Result<InstalledPluginRecord, PluginInstallError> {
    let scope = options.scope.unwrap_or("user");
    let timeout = options.timeout.unwrap_or(DEFAULT_GIT_TIMEOUT);
    let env = options.env;

    // ①-④：解析 mp source + 定位 plugin 根（必要时 clone）+ strict 冲突检测 + 载入 bundled servers（注册前
    // 畸形即抛 → 原子前置）。install 与账本重建 [`materialize_plugin_record`] 共享此原语，杜绝物化路径漂移。
    let m = materialize_plugin(
        plugin_id,
        home,
        env,
        options.refresh,
        timeout,
        options.version,
        true,
    )
    .await?;

    // ⑤：★冲突闸门（零变更）。owned = 自有同名白名单（上次记录的 bundledMcpServers）。
    let owned = bundled_servers_of(home, plugin_id, env);
    let existing = hooks
        .map(McpInstallHooks::existing_server_names)
        .unwrap_or_default();
    conflict_check(&m.servers, &existing, &owned)?;

    // —— 过闸：物化但**不激活**（v0.3.0，协议 §2.4：install 落 `installed_disabled`）——
    // 不挂 server、不注入 inputs（延到 [`enable_plugin`]）；skills 注册后立即 orphan（不进 `get_skills`）。
    let filter: HashSet<String> = std::iter::once(m.plugin.clone()).collect();
    // skills 注册：复用既有 clone（refresh 透传）；失败降级（返回 Vec、不抛，见 #49）。计数供成功日志。
    let staged_skills = stage_marketplace_skills(
        &m.marketplace,
        &m.source,
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
    // installed_disabled 末态：本 plugin 全部 skill 置 orphan（镜像 [`disable_plugin`]；enable 再翻活）。
    for name in plugin_skill_names(registry, &m.marketplace, &m.plugin) {
        registry.mark_orphan(&name);
    }

    // ⑦：★config-first 先写 `installedPlugins` 全局安装意图（权威）
    let pid_intent = plugin_id.to_string();
    update_installed_plugins_intent(
        move |file| {
            file.account.installed_plugins.insert(pid_intent);
        },
        Some(home),
        env,
    )?;

    // ⑧：★再写账本（派生缓存，仅全成功；与账本重建复用 [`write_ledger_record`]）
    let record = write_ledger_record(home, plugin_id, scope, options.project_path, &m, env)?;
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
/// （`<home>/marketplace/<mp>/.../<plugin>`），`safe_rmtree` 删的是该 marketplace 共享 git 工作树的子目录
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
            // 账本无记录：正常即「未安装」no-op。但若 `installedPlugins` 意图仍残留该 pid（config-first 下 install
            // 写意图后写账本失败留下的悬挂条目）→ 收敛：从意图移除 + 清 enabledPlugins（否则 list/info 会永久显示
            // 「已安装但无详情」，仅重装同 pid 才自愈；且 gc 的孤儿判定=账本∉意图，与此相反、清不掉）。
            let dangling = load_installed_plugins_intent(Some(home), env)
                .account
                .installed_plugins
                .contains(plugin_id);
            if dangling {
                let pid_intent = plugin_id.to_string();
                update_installed_plugins_intent(
                    move |file| {
                        file.account.installed_plugins.shift_remove(&pid_intent);
                    },
                    Some(home),
                    env,
                )?;
                // best-effort 清 user scope 残留旗（存在性守卫：无旗则不触碰文件）。
                if let Err(e) = clear_enabled_plugin(plugin_id, "user", None, env) {
                    tracing::warn!(plugin = plugin_id, error = %e, "uninstall: clear dangling enabledPlugins failed");
                }
                tracing::info!(
                    plugin = plugin_id,
                    "uninstall: converged dangling install-intent (no ledger record)"
                );
                return Ok(true);
            }
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

    // 清启用意图前先收集 targeted scope（§2.4：uninstall 清 `enabledPlugins` 条目，避免 reinstall 残留旗）。
    let scopes_to_clear: Vec<(String, Option<String>)> = targeted
        .iter()
        .map(|r| {
            (
                r.extra
                    .get("scope")
                    .and_then(Value::as_str)
                    .unwrap_or("user")
                    .to_string(),
                r.extra
                    .get("projectPath")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            )
        })
        .collect();

    let pid = plugin_id.to_string();
    let scope_owned = options.scope.map(String::from);
    let ledger_after = update_installed_plugins(
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

    // 该 pid 已无任何 scope 账本记录 → 从全局安装意图 `installedPlugins` 移除（config-first 权威随之收敛）。
    if !ledger_after.account.plugins.contains_key(plugin_id) {
        let pid_intent = plugin_id.to_string();
        update_installed_plugins_intent(
            move |file| {
                file.account.installed_plugins.shift_remove(&pid_intent);
            },
            Some(home),
            env,
        )?;
    }

    // 清 targeted scope 的 `enabledPlugins[id]` 残留旗（best-effort：清理性，失败不回退已完成的卸载）。
    for (scope, project_path) in &scopes_to_clear {
        if let Err(e) = clear_enabled_plugin(plugin_id, scope, project_path.as_deref(), env) {
            tracing::warn!(plugin = plugin_id, scope = %scope, error = %e, "uninstall: clear enabledPlugins failed (stale flag may 残留)");
        }
    }

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
/// # Returns
/// `Ok(changed)`＝`enabledPlugins[id]` 是否真的从 true→false（#115 R1）；已禁用再禁用 → `Ok(false)`。
///
/// # Errors
/// id 非法 / settings 写失败 / `remove_server` 失败 → [`PluginInstallError`]。
pub async fn disable_plugin(
    plugin_id: &str,
    registry: &mut SkillRegistry,
    home: &Path,
    options: DisableOptions<'_>,
    hooks: Option<&dyn McpInstallHooks>,
) -> Result<bool, PluginInstallError> {
    let (plugin, marketplace) = split_plugin_id(plugin_id)?;
    let scope = options.scope.unwrap_or("user");
    // `changed`＝enabledPlugins 真的从 true→false（#115 R1）；已 disable 再 disable → false。server 停摘 /
    // skill orphan 幂等、仍无条件执行（不因 no-op 而漏兜底），仅**配置内容**变化由 `changed` 表达上抛。
    let changed = write_enabled_plugin(plugin_id, false, scope, options.project_path, options.env)?;
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
    Ok(changed)
}

/// 启用单个 plugin（廉价复原，**无需重 clone/重装**）/ Enable = cheap restore (no re-clone)。
///
/// ① 从物化记录的 `installPath` 重解析 bundled servers → **★冲突预检（先于 settings 写 → enable 原子）**；
/// ② 写 `enabledPlugins[id]=true`；③ 复活 skills（re-stage：register_or_update 翻活孤儿，refresh=false 复用
/// clone）；④ 重挂 servers。未安装 → 拒。
///
/// ⚠️ **scope 契约**：同 [`disable_plugin`]。
///
/// # Returns
/// `Ok(changed)`＝`enabledPlugins[id]` 是否真的从非-true→true（#115 R1）；幂等 re-enable → `Ok(false)`。
///
/// # Errors
/// id 非法 / 未安装 / manifest 畸形 / 冲突 / settings 写失败 / 注入失败 → [`PluginInstallError`]。
pub async fn enable_plugin(
    plugin_id: &str,
    registry: &mut SkillRegistry,
    home: &Path,
    options: EnableOptions<'_>,
    hooks: Option<&dyn McpInstallHooks>,
) -> Result<bool, PluginInstallError> {
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

    // ② 写 enabledPlugins[id]=true（冲突预检通过后）。`changed`＝真的从非-true→true（#115 R1）；
    //    幂等 re-enable（已 true）→ false。skill 复活 / server 重挂幂等、仍无条件跑（半态修复），仅
    //    **配置内容**变化由 `changed` 上抛供 Computer 决定是否 bump config revision + 通知 robot。
    let changed = write_enabled_plugin(plugin_id, true, scope, options.project_path, env)?;

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
    Ok(changed)
}

// ---------------------------------------------------------------------------
// v0.3.0 一次性迁移 / one-time migration
// ---------------------------------------------------------------------------
/// 把 v0.2.x「装即活跃」账本迁移到 v0.3.0 模型（`installedPlugins` 意图 + `enabledPlugins=true`）/ one-time migration。
///
/// **幂等靠 `installed_plugins_intent.json` 是否存在**：缺失 → 迁移（① 从账本 keys 回填 `installedPlugins` 意图；
/// ② 每条记录按其 scope 写 `enabledPlugins=true`，**仅 absent 处**，不覆盖用户显式 `false`）→ 落意图文件即标记
/// 完成；存在 → 跳过（返回 `false`）。boot 首步调用（reconcile 之前，见 `Computer::boot_up`），保住存量用户「升级
/// 前 active」的 plugin 在 v0.3.0（absent = 未启用）下不熄灯。返回是否执行了迁移。
///
/// # Errors
/// 意图 / settings 写失败（锁 / I/O）→ [`PluginInstallError`]。逐记录的 enabledPlugins 写为 best-effort（非法
/// scope 等仅 WARN 跳过、不中断整体迁移）。
pub fn migrate_ledger_to_intent_once(
    home: &Path,
    env: Option<&EnvMap>,
) -> Result<bool, PluginInstallError> {
    // 已有意图文件 → 迁移已跑过（或本就是 v0.3.0 新装），跳过。
    if store::installed_plugins_intent_path(Some(home), env).exists() {
        return Ok(false);
    }
    let ledger = load_installed_plugins(Some(home), env).account;
    let pids: Vec<String> = ledger.plugins.keys().cloned().collect();

    // ① 回填意图（即使账本为空，也落一个空意图文件以标记「迁移已跑」，避免下次 boot 重判）。
    let pids_for_intent = pids.clone();
    update_installed_plugins_intent(
        move |file| {
            for pid in pids_for_intent {
                file.account.installed_plugins.insert(pid);
            }
        },
        Some(home),
        env,
    )?;

    // ② 每记录 scope 写 enabledPlugins=true（仅 absent；保用户 disable）。best-effort。
    for (pid, records) in &ledger.plugins {
        for rec in records {
            let scope = rec
                .extra
                .get("scope")
                .and_then(Value::as_str)
                .unwrap_or("user");
            let project_path = rec.extra.get("projectPath").and_then(Value::as_str);
            if let Err(e) = set_enabled_true_if_absent(pid, scope, project_path, env) {
                tracing::warn!(plugin = %pid, scope = %scope, error = %e, "migration: set enabledPlugins=true failed (skipped)");
            }
        }
    }
    if !pids.is_empty() {
        tracing::info!(
            migrated = pids.len(),
            "migrated v0.2.x ledger → installedPlugins intent + enabledPlugins=true"
        );
    }
    // true 表示实际迁移了 ≥1 存量 plugin；空账本仍落意图文件标记完成，但返回 false。
    Ok(!pids.is_empty())
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
    async fn install_only_disabled_inactive_and_records_intent() {
        // v0.3.0：install = installed_disabled——skills orphan（不进 get_skills）、server 不挂、不写
        // enabledPlugins，但登记 installedPlugins 意图 + 写账本（派生缓存）。
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
        let record = install_plugin(
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
        // 不激活：skill orphan（resolve 排除孤儿）+ server 不挂。
        assert!(
            reg.resolve("audit:code-review").is_none(),
            "install 不激活 skills（orphan）"
        );
        assert!(
            hooks.registered.lock().unwrap().is_empty(),
            "install 不挂 bundled server（延到 enable）"
        );
        // 不写 enabledPlugins（absent = 未启用）。
        assert_eq!(enabled_flag(&env, "audit@acme"), None);
        // installedPlugins 意图登记该 id（权威 install-set）。
        let intent = crate::settings::store::load_installed_plugins_intent(Some(&home), Some(&env))
            .account
            .installed_plugins;
        assert!(
            intent.contains("audit@acme"),
            "install config-first 写 installedPlugins 意图"
        );
        // 账本记录（派生缓存）：scope/installPath/bundledMcpServers/installedAt。
        assert_eq!(record.bundled_mcp_servers, vec!["audit-mcp".to_string()]);
        assert!(record.install_path.is_some());
        assert_eq!(
            record.extra.get("scope").and_then(Value::as_str),
            Some("user")
        );
        assert!(record.extra.contains_key("installedAt"));
        assert_eq!(ledger_records(&home, &env, "audit@acme").len(), 1);
        assert!(installed_plugins_path(Some(&home), Some(&env)).is_file());
    }

    #[tokio::test]
    async fn enable_after_install_activates_skills_and_servers() {
        // v0.3.0：install 惰性；enable 才把 skills 与 bundled server 一并点亮。
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
        install_plugin(
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
        assert!(
            reg.resolve("audit:code-review").is_none(),
            "install 后 orphan"
        );

        let hooks = RecordingHooks::new();
        enable_plugin(
            "audit@acme",
            &mut reg,
            &home,
            EnableOptions {
                env: Some(&env),
                ..Default::default()
            },
            Some(&hooks),
        )
        .await
        .unwrap();
        assert!(
            reg.resolve("audit:code-review").is_some(),
            "enable 后 skill 活跃"
        );
        assert_eq!(
            *hooks.registered.lock().unwrap(),
            vec!["audit-mcp".to_string()],
            "enable 挂 bundled server"
        );
        assert_eq!(enabled_flag(&env, "audit@acme"), Some(true));
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

    // 注：v0.3.0 起 install **不**挂 server（延到 enable），故原 `install_rollback_on_register_failure` 已移除；
    // server 挂载失败的原子回滚由 `enable_rollback_on_register_failure_redisables_and_reorphans` 覆盖。

    // ---- uninstall ----------------------------------------------------------
    #[tokio::test]
    async fn uninstall_removes_skills_ledger_servers_intent_and_enabled() {
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
        // enable 激活（skill 活跃 + server 挂），再卸载验证全链摘除。
        enable_plugin(
            "audit@acme",
            &mut reg,
            &home,
            EnableOptions {
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
        // installedPlugins 意图移除 + enabledPlugins 条目清除（避免 reinstall 残留旗）。
        assert!(
            !crate::settings::store::load_installed_plugins_intent(Some(&home), Some(&env))
                .account
                .installed_plugins
                .contains("audit@acme")
        );
        assert_eq!(
            enabled_flag(&env, "audit@acme"),
            None,
            "uninstall 清 enabledPlugins 条目"
        );
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
        assert!(
            reg.resolve("audit:code-review").is_none(),
            "install 后 orphan（installed_disabled）"
        );

        // enable：激活 skill + 挂 server + enabledPlugins=true。
        enable_plugin(
            "audit@acme",
            &mut reg,
            &home,
            EnableOptions {
                env: Some(&env),
                ..Default::default()
            },
            Some(&hooks),
        )
        .await
        .unwrap();
        assert!(reg.resolve("audit:code-review").is_some(), "enable 后活跃");

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

        // #115 R1（方案 A）：幂等 re-enable（已启用再 enable）→ enabledPlugins 内容未变 → 返回 changed=false，
        // 供 Computer 层不虚假 bump config revision（步骤 ③④ 的 skill 复活 / server 重挂仍幂等跑，仅**内容**
        // delta 由 changed 表达）。对齐 disable 的对称语义（首次真变=true）。
        let hooks3 = RecordingHooks::new();
        let changed = enable_plugin(
            "audit@acme",
            &mut reg,
            &home,
            EnableOptions {
                env: Some(&env),
                ..Default::default()
            },
            Some(&hooks3),
        )
        .await
        .unwrap();
        assert!(
            !changed,
            "幂等 re-enable（已启用）→ changed=false（不虚假 bump）"
        );
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

    // ---- v0.3.0 一次性迁移 / migration --------------------------------------
    /// 直接写一条 v0.2.x 式账本记录（scope，无 installPath/bundled）/ seed a v0.2.x-style ledger record。
    fn seed_ledger_record(home: &Path, pid: &str, scope: &str, env: &EnvMap) {
        let pid = pid.to_string();
        let mut extra = Map::new();
        extra.insert("scope".to_string(), Value::String(scope.to_string()));
        store::update_installed_plugins(
            move |file| {
                file.account.plugins.insert(
                    pid,
                    vec![InstalledPluginRecord {
                        install_path: None,
                        bundled_mcp_servers: Vec::new(),
                        extra,
                    }],
                );
            },
            Some(home),
            Some(env),
        )
        .unwrap();
    }

    #[test]
    fn write_enabled_plugin_returns_changed_only_on_real_content_change() {
        // #115 R1（方案 A）：`apply_enabled_plugin_write` 据**实际写盘**返回 changed——首写真变=true、
        // 幂等重写同值=false（不写盘）。这是 Computer「只在真变时 bump config revision + 通知 robot」的
        // 授权信号源，false-negative 安全（写了即真变）。
        let tmp = TempDir::new().unwrap();
        let cfg_home = tmp.path().join("cfg");
        fs::create_dir_all(&cfg_home).unwrap();
        let env: EnvMap = std::iter::once((
            "XDG_CONFIG_HOME".to_string(),
            cfg_home.to_string_lossy().into_owned(),
        ))
        .collect();

        assert!(
            write_enabled_plugin("a@mp", true, "user", None, Some(&env)).unwrap(),
            "首次写 true → 真变"
        );
        assert!(
            !write_enabled_plugin("a@mp", true, "user", None, Some(&env)).unwrap(),
            "幂等重写同值 true → 无变化（no-op）"
        );
        assert!(
            write_enabled_plugin("a@mp", false, "user", None, Some(&env)).unwrap(),
            "翻到 false → 真变"
        );
        assert!(
            !write_enabled_plugin("a@mp", false, "user", None, Some(&env)).unwrap(),
            "重复 disable（false→false）→ 无变化（no-op）"
        );
        assert!(
            write_enabled_plugin("a@mp", true, "user", None, Some(&env)).unwrap(),
            "再翻 true → 真变"
        );
    }

    #[tokio::test]
    async fn migrate_ledger_to_intent_backfills_enables_once_and_preserves_user_disable() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let cfg_home = tmp.path().join("cfg");
        fs::create_dir_all(&cfg_home).unwrap();
        let env: EnvMap = std::iter::once((
            "XDG_CONFIG_HOME".to_string(),
            cfg_home.to_string_lossy().into_owned(),
        ))
        .collect();

        // v0.2.x 态：两条账本记录、无意图文件、无 enabledPlugins；用户已显式 disable lint@acme。
        seed_ledger_record(&home, "audit@acme", "user", &env);
        seed_ledger_record(&home, "lint@acme", "user", &env);
        write_enabled_plugin("lint@acme", false, "user", None, Some(&env)).unwrap();
        assert!(!store::installed_plugins_intent_path(Some(&home), Some(&env)).exists());

        // 迁移。
        assert!(migrate_ledger_to_intent_once(&home, Some(&env)).unwrap());
        // 意图回填两者。
        let intent = store::load_installed_plugins_intent(Some(&home), Some(&env))
            .account
            .installed_plugins;
        assert!(intent.contains("audit@acme") && intent.contains("lint@acme"));
        // enabledPlugins：audit absent → 迁为 true；lint 保用户 false（不覆盖）。
        assert_eq!(enabled_flag(&env, "audit@acme"), Some(true));
        assert_eq!(
            enabled_flag(&env, "lint@acme"),
            Some(false),
            "迁移不覆盖用户显式 disable"
        );

        // 幂等：意图文件已存在 → 二次迁移 no-op。
        assert!(!migrate_ledger_to_intent_once(&home, Some(&env)).unwrap());
    }

    #[tokio::test]
    async fn migrate_empty_ledger_marks_done_without_rerun() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let env = EnvMap::new();
        // 无账本 → 迁移落空意图文件、返回 false（无存量），但标记完成（下次跳过）。
        assert!(!migrate_ledger_to_intent_once(&home, Some(&env)).unwrap());
        assert!(store::installed_plugins_intent_path(Some(&home), Some(&env)).exists());
        assert!(!migrate_ledger_to_intent_once(&home, Some(&env)).unwrap());
    }

    // 🟡2 回归：config-first 下 install 写 intent 后写账本失败留悬挂 intent 条目 → uninstall 须收敛（否则永久残留）。
    #[tokio::test]
    async fn uninstall_converges_dangling_intent_without_ledger_record() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let env = EnvMap::new();
        // 模拟悬挂态：intent 有 pid、账本无记录。
        store::update_installed_plugins_intent(
            |f| {
                f.account.installed_plugins.insert("audit@acme".to_string());
            },
            Some(&home),
            Some(&env),
        )
        .unwrap();

        let mut reg = SkillRegistry::new();
        let removed = uninstall_plugin(
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
        assert!(removed, "悬挂 intent 应被收敛（返回 true）");
        assert!(
            !store::load_installed_plugins_intent(Some(&home), Some(&env))
                .account
                .installed_plugins
                .contains("audit@acme")
        );

        // 再次卸载 → 真 no-op。
        assert!(!uninstall_plugin(
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
        .unwrap());
    }
}
