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

use std::collections::{HashMap, HashSet};
use std::io;
#[cfg(feature = "cli")]
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Map, Value};

use crate::mcp_clients::bundle_id::resolve_bundle_id;
use crate::mcp_clients::model::{BundleId, MCPServerConfig, ServerName};
use crate::settings::raw_write;
use crate::settings::reconciler::{
    is_external_plugin_install_path, remove_plugin_install_path, InstalledPluginRecord,
};
use crate::settings::schema::{is_valid_enabled_plugin_key, SettingsScope, FIELD_ENABLED_PLUGINS};
use crate::settings::scope::{
    load_settings_file, user_settings_path, workdir_local_settings_path,
    workdir_project_settings_path, EnvMap,
};
use crate::settings::store::{
    self, load_installed_plugins, load_installed_plugins_intent, load_known_marketplaces,
    update_installed_plugins, update_installed_plugins_intent, InstalledPluginsFile,
    SettingsStoreError,
};
use crate::skills::home::{marketplace_skill_dir, SOURCE_MARKETPLACE};
use crate::skills::manifest::{
    check_strict_conflict, entry_declares_component, entry_is_strict, find_plugin_entry,
    load_bundled_servers, manifest_declared_components, plugin_root_base,
    read_marketplace_manifest, read_plugin_metadata, resolve_plugin_version, PluginManifestError,
};
use crate::skills::registry::{SkillRegistry, SkillSourceConflict};
use crate::skills::staging::{
    has_projectable_skill, locate_plugin_root, plugin_skill_container_dirs,
    stage_marketplace_skills, MarketplaceStageOptions, SkillStagingError, DEFAULT_GIT_TIMEOUT,
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

/// 同名 plugin 跨 marketplace 冲突 / Cross-marketplace plugin-name conflict。
///
/// 协议依据 skill.md §1.5 / conformance-tests.md:365：安装层按 `<plugin>` 裸名跨 marketplace 全局唯一，
/// 拒绝第二个；已装集合与先到 SKILL 的 `source` 保持不变。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginNameConflictError {
    /// 本次被拒绝的 `<plugin>@<marketplace>` / requested pid。
    pub requested_pid: String,
    /// 裸 plugin 名（身份）/ bare plugin name。
    pub plugin: String,
    /// 冲突类别 / conflict kind。
    pub kind: PluginNameConflictKind,
}

/// 同名 plugin 冲突类别 / Kind of plugin-name conflict。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PluginNameConflictKind {
    /// 已装集合（`installedPlugins` 意图）内已存在同名 plugin。
    Installed {
        /// 已安装的冲突 pid（`<plugin>@<marketplace>`）。
        installed_pid: String,
    },
    /// Skill Registry 中同名 SKILL 已被其它 source 占用（保留先到者）。
    SkillSource {
        /// 冲突明细 / conflict details。
        conflicts: Vec<SkillSourceConflict>,
    },
}

impl std::error::Error for PluginNameConflictError {}

impl std::fmt::Display for PluginNameConflictError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.kind {
            PluginNameConflictKind::Installed { installed_pid } => write!(
                f,
                "plugin name conflict: cannot use plugin {:?}: plugin name {:?} is already \
                 installed as {:?} (plugin names are unique across marketplaces). Run \
                 'plugin uninstall {}' first if you want to switch marketplaces.",
                self.requested_pid, self.plugin, installed_pid, installed_pid
            ),
            PluginNameConflictKind::SkillSource { conflicts } => {
                let mut names: Vec<&str> = conflicts.iter().map(|c| c.name.as_str()).collect();
                names.sort_unstable();
                names.dedup();
                let shown = names.iter().take(3).copied().collect::<Vec<_>>().join(", ");
                let more = names.len().saturating_sub(3);
                let kept = conflicts
                    .first()
                    .map(|c| c.kept_source.as_str())
                    .unwrap_or("another source");
                write!(
                    f,
                    "plugin name conflict: cannot register skills of plugin {:?}: skill(s) {} \
                     already owned by source {:?} (first registrant wins). Remove the conflicting \
                     marketplace or plugin (e.g. 'marketplace remove <marketplace>' or \
                     'plugin uninstall <plugin>@<marketplace>') and retry.",
                    self.requested_pid,
                    if more > 0 {
                        format!("{shown} (+{more} more)")
                    } else {
                        shown
                    },
                    kept
                )
            }
        }
    }
}

/// plugin install/enable/uninstall/disable 失败 / Plugin lifecycle failure。
///
/// [`PluginInstallError::Conflict`] 单独建模 §10.6「外来 MCP 同名硬抛」——调用方可 `matches!` 精确识别
/// （对标 Python 的独立 `MCPServerNameConflictError` 异常类型，无逃生口）。
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PluginInstallError {
    /// 前置失败（marketplace 未添加 / catalog 未 clone / entry 缺失 / 非法 id / 非法 scope）。
    #[error("{0}")]
    Precondition(String),
    /// 同名 plugin 跨 marketplace 冲突（安装层拒绝、零配置变更）。
    #[error(transparent)]
    NameConflict(#[from] PluginNameConflictError),
    /// 外来 MCP server 同名冲突（硬抛、无逃生口，§10.6）。
    #[error(transparent)]
    Conflict(#[from] McpServerNameConflictError),
    /// enable 时 plugin 可投影内容不可用（安装根缺失 / 声明内容无法投影 / 预检期望 SKILL 但零注册）；
    /// 失败前已回滚 `installed_disabled`（#274，runtime-contract §2.4）。
    #[error("plugin {plugin_id:?} cannot be enabled: {detail}")]
    ContentUnavailable {
        /// plugin id（`<plugin>@<marketplace>`）
        plugin_id: String,
        /// 稳定原因码 + 细节（`install_path_missing:` / `declared_server_missing:` /
        /// `no_projectable_content:` / `skills_not_staged:`）
        detail: String,
    },
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
    /// 当前已注册 server：`bundle_id → display 名`（冲突闸门/依赖预检输入）/ registered servers by bundle_id。
    ///
    /// #139/R4：由 `existing_server_names() -> HashSet<String>` 改按 **bundle_id 寻址**（display 名仅供
    /// 「依赖已满足」诊断呈现，非身份键）。`register_server` 与本方法结构性成对，杜绝 §10.6 静默旁路。
    fn existing_servers(&self) -> HashMap<BundleId, ServerName>;
    /// 注册 / 更新一个 server（含 `${input:}` 渲染）/ register or update a server。
    ///
    /// # Errors
    /// 注册失败 → [`McpHookError`]（触发 install 补偿回滚）。
    async fn register_server(&self, cfg: MCPServerConfig) -> Result<(), McpHookError>;
    /// 注册一个 server，携带它归属的 plugin_id（`<plugin>@<marketplace>`；`None` = 非 plugin-bound）/
    /// register with the owning plugin id.
    ///
    /// §5.11（a2c-smcp-protocol v0.3.1）：plugin-bound server 配置里的裸 `${input:<id>}` 须按
    /// `<plugin>@<marketplace>/<id>` → 全局 `<id>`（同 kind）序解析。`plugin_id` 即该归属，由 enable/reconcile
    /// （已知 plugin）透传；SDK 据此派生 `PluginScope`（`crate::inputs::plugin_pool::PluginScope`，内部类型）注入渲染路径。
    ///
    /// **默认实现忽略 `plugin_id`**、退化为 [`register_server`](Self::register_server)（外部 hook 未覆盖时维持
    /// 旧行为：plugin 裸 input 仅按全局解析）。SDK 自带 hook 覆盖此方法以启用 scoped 解析；外部 client 欲启用，
    /// 覆盖此方法、在内部把 `plugin_id` 交给 scope-aware mount 路径即可，无需新建公开 API（分叉二裁决）。
    ///
    /// # Errors
    /// 同 [`register_server`](Self::register_server)。
    async fn register_server_with_input_scope(
        &self,
        cfg: MCPServerConfig,
        plugin_id: Option<&str>,
    ) -> Result<(), McpHookError> {
        let _ = plugin_id;
        self.register_server(cfg).await
    }
    /// 停止并摘除一个 server（按 **bundle_id** 寻址）/ stop & remove a server by bundle_id。
    ///
    /// #139/R4：由 `name: &str` 改 `id: &BundleId`——非对称合理（register 需完整 cfg 才能挂；remove 只需身份
    /// 就能摘）。同名两 server（显式异 bundle_id）从此可精确停摘，不再字典序最小误摘。
    ///
    /// # Errors
    /// 摘除失败 → [`McpHookError`]（uninstall/disable 上抛；install 回滚中仅 WARN）。
    async fn remove_server(&self, id: &BundleId) -> Result<(), McpHookError>;
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
/// 经 `raw_write::update_raw_settings` 做 strict raw RMW：只改该 key、不毁兄弟、不回写校验视图；
/// corrupt / `enabledPlugins` 非对象 → fail-closed。settings.json 是人编意图层 → 无写保护头。
fn apply_enabled_plugin_write(
    plugin_id: &str,
    value: bool,
    scope: &str,
    project_path: Option<&str>,
    env: Option<&EnvMap>,
) -> Result<bool, PluginInstallError> {
    let (path, scope_enum) = settings_path_for_scope(scope, project_path, env)?;
    // #253：strict raw RMW（共享 helper）——corrupt fail-closed、只改 enabledPlugins[pid]；
    // project scope 的 readRoots 保留 + WARN（A+，不主动删除）。返回的 `changed` 仍是
    // 「enabledPlugins 内容真变」，供 Computer 层只在真变时 bump config revision（#115 R1，方案 A）。
    let outcome =
        raw_write::update_raw_settings(&path, scope_enum == SettingsScope::Project, |map| {
            let plugins = raw_write::object_field_mut(map, FIELD_ENABLED_PLUGINS, &path)?;
            plugins.insert(plugin_id.to_string(), Value::Bool(value));
            Ok(())
        })
        .map_err(|e| PluginInstallError::Store(e.into_store()))?;
    tracing::debug!(
        plugin = plugin_id,
        changed = outcome.changed,
        wrote = outcome.wrote,
        "enabledPlugins raw write"
    );
    Ok(outcome.changed)
}

/// 写 `enabledPlugins[<plugin_id>] = value`（enable/disable 用）/ Write the enable flag。
fn write_enabled_plugin(
    plugin_id: &str,
    value: bool,
    scope: &str,
    project_path: Option<&str>,
    env: Option<&EnvMap>,
) -> Result<bool, PluginInstallError> {
    apply_enabled_plugin_write(plugin_id, value, scope, project_path, env)
}

/// 删除 `enabledPlugins[<plugin_id>]` 键（uninstall 清启用意图条目，协议 §2.4）/ Delete the enable flag key。
///
/// 清除后避免「uninstall → reinstall」残留 `true` 令新装绕过 `installed_disabled` 直接激活。**仅当该键实际存在
/// 才写盘**——从未 enable 的 plugin 卸载时不触碰 settings.json（保 Computer 层 `env=None` 卸载对 `~/.config`
/// 无副作用，见 computer.rs enabledPlugins 测试约定）。project scope 的 `readRoots` 保留 + WARN（A+）。
fn clear_enabled_plugin(
    plugin_id: &str,
    scope: &str,
    project_path: Option<&str>,
    env: Option<&EnvMap>,
) -> Result<(), PluginInstallError> {
    let (path, scope_enum) = settings_path_for_scope(scope, project_path, env)?;
    // #253：strict raw RMW；enabledPlugins 容器畸形 → fail-closed（不再静默覆盖）。
    // 键不存在 / 从未 enable → 不触碰 settings.json；project readRoots 保留 + WARN（A+）。
    raw_write::update_raw_settings(&path, scope_enum == SettingsScope::Project, |map| {
        let Some(plugins) = raw_write::object_field_opt_mut(map, FIELD_ENABLED_PLUGINS, &path)?
        else {
            return Ok(());
        };
        plugins.remove(plugin_id);
        Ok(())
    })
    .map_err(|e| PluginInstallError::Store(e.into_store()))?;
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
    // #253：strict raw RMW；已有显式值（true / 用户 disable 的 false / 非法兄弟值）一律不覆盖。
    raw_write::update_raw_settings(&path, scope_enum == SettingsScope::Project, |map| {
        let plugins = raw_write::object_field_mut(map, FIELD_ENABLED_PLUGINS, &path)?;
        plugins
            .entry(plugin_id.to_string())
            .or_insert(Value::Bool(true));
        Ok(())
    })
    .map_err(|e| PluginInstallError::Store(e.into_store()))?;
    Ok(())
}

/// 某 plugin 当前**活跃**的 marketplace SKILL name / A plugin's currently-active marketplace SKILL names。
///
/// 过滤 `source == "marketplace:<mp>"` 且 `name` 以 `<plugin>:` 起；仅枚举活跃集（排除孤儿）。
fn plugin_skill_names(registry: &SkillRegistry, marketplace: &str, plugin: &str) -> Vec<String> {
    let want_source = format!("{SOURCE_MARKETPLACE}:{marketplace}");
    let prefix = format!("{plugin}:");
    registry.names_for_source(&want_source, &prefix, false)
}

/// 某 plugin 的 marketplace SKILL name（**含孤儿**，uninstall 清名用）/ All SKILL names incl. orphans。
fn plugin_skill_names_all(
    registry: &SkillRegistry,
    marketplace: &str,
    plugin: &str,
) -> Vec<String> {
    let want_source = format!("{SOURCE_MARKETPLACE}:{marketplace}");
    let prefix = format!("{plugin}:");
    registry.names_for_source(&want_source, &prefix, true)
}

/// 安装层唯一性前置（协议 skill.md §1.5 / conformance-tests.md:365）：已装集合
/// （`installedPlugins` 意图）中不得存在同裸名、不同 marketplace 的 pid。
///
/// 仅读意图文件——失败零变更（不 clone / 不 staging / 不写 intent/ledger/settings、不改 registry）。
fn ensure_bare_plugin_not_installed(
    plugin: &str,
    requested_pid: &str,
    home: &Path,
    env: Option<&EnvMap>,
) -> Result<(), PluginInstallError> {
    let intent = load_installed_plugins_intent(Some(home), env);
    for pid in &intent.account.installed_plugins {
        if pid == requested_pid {
            continue;
        }
        if let Some((installed_plugin, _)) = pid.split_once('@') {
            if installed_plugin == plugin {
                return Err(PluginNameConflictError {
                    requested_pid: requested_pid.to_string(),
                    plugin: plugin.to_string(),
                    kind: PluginNameConflictKind::Installed {
                        installed_pid: pid.clone(),
                    },
                }
                .into());
            }
        }
    }
    Ok(())
}

/// #275：清除「未安装 marketplace 源」对目标 plugin 的 SKILL 占名（installed intent 优先）。
///
/// 仅在已装集合内无同裸名冲突（前置门已过）时调用：此时其他 marketplace 源的同名 SKILL 必来自
/// 未安装/仅添加的 plugin，可安全让位给本次安装/启用的源（修复 CLI 离线重建把 add-only 源当 owner）。
fn clear_uninstalled_marketplace_skill_names(
    registry: &mut SkillRegistry,
    home: &Path,
    env: Option<&EnvMap>,
    requested_source: &str,
    skill_prefix: &str,
) {
    let intent = load_installed_plugins_intent(Some(home), env);
    // pid 粒度保护：只有「该 skill 所属 plugin 的 pid 确在 installedPlugins」才保留占名。
    // marketplace 粒度会把同源其它已装 plugin（如 mp-a 的 beta）误当保护伞，挡住 alpha@mp-b。
    let installed_pids: HashSet<String> =
        intent.account.installed_plugins.iter().cloned().collect();
    let marketplace_prefix = format!("{SOURCE_MARKETPLACE}:");
    let stale: Vec<String> = registry
        .all_refs()
        .into_iter()
        .filter(|(r, _)| {
            if !r.name.starts_with(skill_prefix) || r.source == requested_source {
                return false;
            }
            let Some(mp) = r.source.strip_prefix(&marketplace_prefix) else {
                return false;
            };
            let Some((plugin, _)) = r.name.split_once(':') else {
                return false;
            };
            !installed_pids.contains(&format!("{plugin}@{mp}"))
        })
        .map(|(r, _)| r.name)
        .collect();
    for name in stale {
        registry.unregister(&name);
    }
}

/// #275：清除某 plugin × marketplace 的跨 source 冲突记录（uninstall 后 stale 不应再发诊断）。
fn clear_source_conflicts_for_plugin(
    registry: &mut SkillRegistry,
    plugin: &str,
    marketplace_source: &str,
) {
    let prefix = format!("{plugin}:");
    registry.clear_source_conflicts_where(|c| {
        c.name.starts_with(&prefix)
            && (c.kept_source == marketplace_source || c.rejected_source == marketplace_source)
    });
}

/// 是否属于「本 plugin × 本 source」的跨 source 冲突 / Whether a conflict belongs to this plugin+source。
fn is_own_source_conflict(conflict: &SkillSourceConflict, want_source: &str, prefix: &str) -> bool {
    conflict.rejected_source == want_source && conflict.name.starts_with(prefix)
}

/// 取走「本 plugin × 本 source」的跨 source 冲突（清旧 + 供 stage 后归因）/ Take this plugin's conflicts。
fn take_own_source_conflicts(
    registry: &mut SkillRegistry,
    want_source: &str,
    prefix: &str,
) -> Vec<SkillSourceConflict> {
    let conflicts: Vec<SkillSourceConflict> = registry
        .source_conflicts()
        .iter()
        .filter(|c| is_own_source_conflict(c, want_source, prefix))
        .cloned()
        .collect();
    registry.clear_source_conflicts_where(|c| is_own_source_conflict(c, want_source, prefix));
    conflicts
}

// ---------------------------------------------------------------------------
// enable 内容预检与回滚 / enable content preflight & rollback（#274）
// ---------------------------------------------------------------------------
/// enable 预检结果 / enable content preflight result。
#[derive(Default)]
struct EnablePreflight {
    /// 跨可用记录合并（按 bundle_id 去重）的 bundled server 配置
    servers: Vec<MCPServerConfig>,
    /// 任一可用记录存在可投影 SKILL 包 → staging 必须至少注册一个
    expect_skills: bool,
    /// 可用安装根（诊断用）
    usable_roots: Vec<PathBuf>,
    /// 跨记录 bundle_id 去重：仅**同根 + 同配置**（多 scope 指向同一根）允许合并；异根/异配置仍按
    /// 身份坍缩硬错（保留 base 语义）。
    seen: HashMap<BundleId, (PathBuf, MCPServerConfig)>,
}

/// enable 前置内容检查（纯读、零写盘）：**∀ 记录**安装根存在 ∧ 声明内容可投影。
///
/// 任一记录不可物化其声明内容 → 整体 [`PluginInstallError::ContentUnavailable`]（#274；与 Python
/// `ledger_entry_fully_materialized` 的 ∀ 语义对齐——enable 聚合全部记录的 bundled servers 进同一投影）。
/// **声明态判据**：entry / plugin.json 声明 `skills`/`mcpServers`，或账本记录声明 `mcpServers`，而实际零投影
/// → 失败；真正未声明任何内容的空插件 → 放行（无内容可投影不是失败）。catalog manifest 不可读时回退
/// plugin.json（strict=true）。
fn preflight_enable_content(
    plugin_id: &str,
    plugin: &str,
    marketplace: &str,
    home: &Path,
    records: &[InstalledPluginRecord],
    existing: &HashMap<BundleId, ServerName>,
) -> Result<EnablePreflight, PluginInstallError> {
    // catalog entry 供 skills 覆写目录发现；catalog/manifest 不可读 → 回退 plugin.json。
    let catalog_dir = marketplace_skill_dir(home, marketplace, &[]);
    let manifest = read_marketplace_manifest(&catalog_dir).ok();
    let entry = manifest
        .as_ref()
        .and_then(|m| find_plugin_entry(m, plugin))
        .cloned();
    let empty_entry = Map::new();

    let mut preflight = EnablePreflight::default();
    let mut reasons: Vec<String> = Vec::new();
    for rec in records {
        let Some(root) = rec
            .install_path
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(Path::new)
        else {
            reasons.push("install_path_missing: record has no installPath".to_string());
            continue;
        };
        if !root.is_dir() {
            reasons.push(format!(
                "install_path_missing: recorded install root does not exist: {}",
                root.display()
            ));
            continue;
        }
        let loaded = load_bundled_servers(root)?;
        let actual: HashSet<BundleId> = loaded.iter().map(resolve_bundle_id).collect();
        let plugin_manifest = read_plugin_metadata(root);
        let strict = entry.as_ref().is_none_or(entry_is_strict);
        // #274 复审 🟡6：strict=false + plugin.json 声明组件 = 畸形（§4.4），预检与 staging 同口径硬错，
        // 不得把它误判成「真空白插件」。
        if let Some(entry_map) = entry.as_ref() {
            check_strict_conflict(entry_map, &plugin_manifest)?;
        }
        let declared_components = manifest_declared_components(&plugin_manifest);
        // strict=false → entry 为权威组件集，plugin.json 的组件声明不参与（§4.4）。
        let declared_skills = entry
            .as_ref()
            .is_some_and(|e| entry_declares_component(e, "skills"))
            || (strict && declared_components.contains(&"skills"));
        // server 声明只有两条真实来源：账本记录 `mcpServers` 与（strict）plugin.json 的 `mcpServers`
        // 组件；entry 级 `mcpServers` 无加载路径（bundled server 仅从 `<root>/mcp-servers/*.json` 读），
        // 不计为声明。
        let declared_servers =
            !rec.mcp_servers.is_empty() || (strict && declared_components.contains(&"mcpServers"));
        if let Some(missing) = rec.mcp_servers.iter().find(|b| !actual.contains(*b)) {
            reasons.push(format!(
                "declared_server_missing: recorded bundled server {:?} not projectable under {}",
                missing.as_str(),
                root.display()
            ));
            continue;
        }
        if declared_servers && loaded.is_empty() {
            reasons.push(format!(
                "declared_server_missing: declared bundled servers not projectable under {}",
                root.display()
            ));
            continue;
        }
        let entry_ref = entry.as_ref().unwrap_or(&empty_entry);
        let skill_dirs = plugin_skill_container_dirs(entry_ref, &plugin_manifest, root, strict);
        let has_skills = has_projectable_skill(&skill_dirs);
        if declared_skills && !has_skills {
            reasons.push(format!(
                "no_projectable_content: declared SKILL packages not projectable under {}",
                root.display()
            ));
            continue;
        }
        // 真空白插件（未声明任何组件、也无文件）→ 放行：无内容可投影不是失败。
        // 单记录内 bundle_id 身份坍缩仍硬错；跨记录重复由下方按 bundle_id 去重。
        conflict_check(&loaded, existing, plugin_id)?;
        for cfg in loaded {
            let bid = resolve_bundle_id(&cfg);
            match preflight.seen.get(&bid) {
                None => {
                    preflight
                        .seen
                        .insert(bid, (root.to_path_buf(), cfg.clone()));
                    preflight.servers.push(cfg);
                }
                Some((seen_root, seen_cfg)) if seen_root.as_path() == root && seen_cfg == &cfg => {
                    // 多 scope 指向同一根 + 同一配置：同一 server 的重复加载 → 去重（非身份坍缩）。
                }
                Some(_) => {
                    return Err(McpServerNameConflictError(format!(
                        "bundled server {:?} appears in multiple install records with different \
                         install root or config (identity collapse across records)",
                        bid.as_str()
                    ))
                    .into());
                }
            }
        }
        preflight.expect_skills |= has_skills;
        preflight.usable_roots.push(root.to_path_buf());
    }

    // ∀ 记录：任一记录不可物化其声明内容 → 整体失败（不留部分投影；对齐 Python 全量物化判据）。
    if !reasons.is_empty() {
        return Err(PluginInstallError::ContentUnavailable {
            plugin_id: plugin_id.to_string(),
            detail: reasons.join("; "),
        });
    }
    Ok(preflight)
}

/// 把该 plugin 当前活跃 SKILL 重新置 orphan（enable 失败回滚的一致性末态）。
fn orphan_plugin_skills(registry: &mut SkillRegistry, marketplace: &str, plugin: &str) {
    for name in plugin_skill_names(registry, marketplace, plugin) {
        registry.mark_orphan(&name);
    }
}

/// 读目标 scope 的 `enabledPlugins[<id>]` 原始布尔值（缺失/损坏 → `None`）。
fn enabled_plugin_raw_value(
    plugin_id: &str,
    scope: &str,
    project_path: Option<&str>,
    env: Option<&EnvMap>,
) -> Option<bool> {
    let (path, scope_enum) = settings_path_for_scope(scope, project_path, env).ok()?;
    let (map, _) = load_settings_file(&path, scope_enum);
    map.get(FIELD_ENABLED_PLUGINS)?.get(plugin_id)?.as_bool()
}

/// 预检失败（尚未写 `enabledPlugins=true`）：仅当目标 scope 当前显式 true 时回滚 false，
/// 避免失败调用无谓创建 settings 文件/键。
fn rollback_preflight_enable(
    plugin_id: &str,
    scope: &str,
    project_path: Option<&str>,
    env: Option<&EnvMap>,
) {
    if enabled_plugin_raw_value(plugin_id, scope, project_path, env) != Some(true) {
        return;
    }
    if let Err(e) = write_enabled_plugin(plugin_id, false, scope, project_path, env) {
        tracing::warn!(plugin = plugin_id, error = %e, "enable preflight rollback: re-disable write failed (ledger may 残留 enabled=true)");
    }
}

/// 已写 `enabledPlugins=true` 后的失败：恢复**调用前原值**（absent → 删键；true/false 原样回写），
/// 不得把原 absent 升级成显式 false（会反向覆盖其他 scope 的 true，§2.4 / #274 复审 🟡2）。
fn restore_enabled_plugin(
    plugin_id: &str,
    scope: &str,
    project_path: Option<&str>,
    env: Option<&EnvMap>,
    prior: Option<bool>,
) {
    let res = match prior {
        Some(v) => write_enabled_plugin(plugin_id, v, scope, project_path, env).map(|_| ()),
        None => clear_enabled_plugin(plugin_id, scope, project_path, env),
    };
    if let Err(e) = res {
        tracing::warn!(plugin = plugin_id, error = %e, "enable rollback: restore enabledPlugins failed (ledger may 残留 true)");
    }
}

/// 该 pid 是否在任一（账本记录涉及的）scope 仍有效启用：按 `user < project < local` 优先级合并各 scope
/// 的 raw `enabledPlugins`（显式值覆盖下层；缺省不覆盖）。policy/managed 只读层不参与——SDK 不写它们。
fn plugin_enabled_in_any_scope(
    plugin_id: &str,
    records: &[InstalledPluginRecord],
    env: Option<&EnvMap>,
) -> bool {
    let mut effective: Option<bool> = None;
    for scope in ["user", "project", "local"] {
        if scope == "user" {
            if let Some(v) = enabled_plugin_raw_value(plugin_id, scope, None, env) {
                effective = Some(v);
            }
            continue;
        }
        let project_path = records.iter().find_map(|r| {
            let rec_scope = r
                .extra
                .get("scope")
                .and_then(Value::as_str)
                .unwrap_or("user");
            (rec_scope == scope)
                .then(|| r.extra.get("projectPath").and_then(Value::as_str))
                .flatten()
        });
        let Some(pp) = project_path else { continue };
        if let Some(v) = enabled_plugin_raw_value(plugin_id, scope, Some(pp), env) {
            effective = Some(v);
        }
    }
    effective == Some(true)
}

/// enable 失败后回滚 SKILL 投影：仅当该 pid 在**任何 scope 都不再启用**时才 orphan 全部活跃 SKILL；
/// 仍有 scope 为 true 时保持原状（不得制造「enabled=true 零投影」，也不得反向打掉其他 scope 的投影；
/// #274 复审 🔴1）。
fn rollback_plugin_skills_on_failure(
    registry: &mut SkillRegistry,
    marketplace: &str,
    plugin: &str,
    plugin_id: &str,
    records: &[InstalledPluginRecord],
    env: Option<&EnvMap>,
) {
    if plugin_enabled_in_any_scope(plugin_id, records, env) {
        tracing::info!(
            plugin = plugin_id,
            "enable rollback: another scope keeps plugin enabled; SKILL projection kept"
        );
        return;
    }
    orphan_plugin_skills(registry, marketplace, plugin);
}

/// 该 plugin 物化记录里全部声明依赖 **bundle_id**（跨 scope 记录并集）/ All declared dependency bundle_ids。
fn bundled_servers_of(home: &Path, plugin_id: &str, env: Option<&EnvMap>) -> HashSet<BundleId> {
    let installed = load_installed_plugins(Some(home), env);
    let mut out: HashSet<BundleId> = HashSet::new();
    if let Some(records) = installed.account.plugins.get(plugin_id) {
        for rec in records {
            out.extend(rec.mcp_servers.iter().cloned());
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

/// bundled server 依赖模型预检（#139/D3，§7.2/§10.6）/ Bundled-server dependency-model precheck。
///
/// **心智转变（D3）**：plugin ↔ MCP server 是**依赖关系**（以 bundle_id 声明），非所有关系。故：
/// - **硬错误 fail-fast**：同一 plugin manifest 内两 server 归一**同 bundle_id**（身份坍缩，不可自动消歧
///   ——须显式设不同 `bundleId`）。这是唯一保留的硬门（协议 §身份正交性）。
/// - **不再阻止**：声明的 bundle_id 本地已有（`existing` 命中）= **依赖已满足** → 仅诊断提示、交 reconcile，
///   **不**抛错、**不**阻止安装（推翻旧「外来同名硬抛」——name 允许碰撞、永不做键）。
fn conflict_check(
    servers: &[MCPServerConfig],
    existing: &HashMap<BundleId, ServerName>,
    plugin_id: &str,
) -> Result<(), McpServerNameConflictError> {
    let mut seen: HashSet<BundleId> = HashSet::new();
    for cfg in servers {
        let bid = resolve_bundle_id(cfg);
        if !seen.insert(bid.clone()) {
            return Err(McpServerNameConflictError(format!(
                "two servers in plugin {plugin_id:?} resolve to the same bundle_id {:?} (identity \
                 collapse); set an explicit distinct bundleId to disambiguate.",
                bid.as_str()
            )));
        }
        // D3：同 bundle_id 本地已有 = 依赖已满足，非冲突——仅提示（不阻止、交 reconcile）。
        if let Some(existing_name) = existing.get(&bid) {
            tracing::info!(
                plugin = plugin_id,
                bundle_id = %bid.as_str(),
                existing = %existing_name,
                "#139/D3: declared MCP dependency already satisfied by an existing server; not remounting"
            );
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
    // #139：账本存 bundle_id（身份键），非 display 名——防 drift（显式 bundleId 的 server 改名后账本不过期）。
    let bundled: Vec<BundleId> = m.servers.iter().map(resolve_bundle_id).collect();
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
        mcp_servers: bundled,
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
/// 顺序：⓪ **★安装层唯一性闸门**（#275：`installedPlugins` 意图内同裸名 `<plugin>` 跨 marketplace 唯一；
/// 命中即拒绝、零变更，且先于 materialize）→ ① 解析 id + mp source；② 要求 catalog 已 clone、读
/// marketplace.json 定位 entry；③ `locate_plugin_root` 定位 plugin 根（必要时 clone）；④ strict 冲突检测 +
/// [`load_bundled_servers`]（注册前畸形即抛）；⑤ **★冲突闸门**（外来 MCP 同名硬抛、零变更——满足
/// 「install 拒绝 foreign name conflict」，虽本步不挂载）；⑥ stage skills → 立即 `mark_orphan`（不投影；
/// 若本 plugin 的 SKILL 已被其它 source 占名则恢复 registry 快照并拒绝，add-first 边界 fail-closed）；
/// ⑦ **config-first 写 `installedPlugins` 全局安装意图**（权威，锁内二次同名复核）→ ⑧ 写账本（派生缓存）。
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

    // ⓪ #275 安装层唯一性前置（协议 skill.md §1.5 / conformance-tests.md:365）：已装集合内同一裸名
    //    `<plugin>` 只能来自一个 marketplace。必须在 [`materialize_plugin`] 之前，保证拒绝时零变更。
    let (plugin, _marketplace) = split_plugin_id(plugin_id)?;
    ensure_bare_plugin_not_installed(&plugin, plugin_id, home, env)?;

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

    // ⑤：★依赖预检（零变更，#139/D3）。existing = 现注册 `bundle_id → display 名`（仅供「依赖已满足」诊断）。
    let existing = hooks
        .map(McpInstallHooks::existing_servers)
        .unwrap_or_default();
    conflict_check(&m.servers, &existing, plugin_id)?;

    // —— 过闸：物化但**不激活**（v0.3.0，协议 §2.4：install 落 `installed_disabled`）——
    // 不挂 server、不注入 inputs（延到 [`enable_plugin`]）；skills 注册后立即 orphan（不进 `get_skills`）。
    let filter: HashSet<String> = std::iter::once(m.plugin.clone()).collect();
    // #275：stage 前快照 + 让未安装 marketplace 源让位（installed intent 优先）+ 清本 plugin/source
    // 旧冲突记录；stage 后仍出现的跨源冲突（如 user/mcp 占名或并发抢跑）fail-closed，恢复快照。
    let want_source = format!("{SOURCE_MARKETPLACE}:{}", m.marketplace);
    let skill_prefix = format!("{}:", m.plugin);
    // 快照成本 O(n)（registry 条目数）；管理面低频操作，可接受。
    let snapshot = registry.clone();
    clear_uninstalled_marketplace_skill_names(registry, home, env, &want_source, &skill_prefix);
    let _ = take_own_source_conflicts(registry, &want_source, &skill_prefix);
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
    let source_conflicts = take_own_source_conflicts(registry, &want_source, &skill_prefix);
    if !source_conflicts.is_empty() {
        *registry = snapshot;
        return Err(PluginNameConflictError {
            requested_pid: plugin_id.to_string(),
            plugin: m.plugin.clone(),
            kind: PluginNameConflictKind::SkillSource {
                conflicts: source_conflicts,
            },
        }
        .into());
    }
    // installed_disabled 末态：本 plugin 全部 skill 置 orphan（镜像 [`disable_plugin`]；enable 再翻活）。
    for name in plugin_skill_names(registry, &m.marketplace, &m.plugin) {
        registry.mark_orphan(&name);
    }

    // ⑦：★config-first 先写 `installedPlugins` 全局安装意图（权威）。
    //    #275 TOCTOU 硬化：锁内再查一次同裸名（前置门在 materialize 之前，防另一进程在窗口内抢先安装）；
    //    命中则**放弃落盘**（intent 逐字节不变），返回后恢复 registry 快照并报冲突（账本尚未写）。
    let pid_intent = plugin_id.to_string();
    let bare_for_intent = plugin.clone();
    let mut raced_with: Option<String> = None;
    crate::settings::store::try_update_installed_plugins_intent(
        |file| {
            for other in &file.account.installed_plugins {
                if other != &pid_intent {
                    if let Some((installed_plugin, _)) = other.split_once('@') {
                        if installed_plugin == bare_for_intent.as_str() {
                            raced_with = Some(other.clone());
                            return false;
                        }
                    }
                }
            }
            file.account.installed_plugins.insert(pid_intent.clone());
            true
        },
        Some(home),
        env,
    )?;
    if let Some(installed_pid) = raced_with {
        *registry = snapshot;
        return Err(PluginNameConflictError {
            requested_pid: plugin_id.to_string(),
            plugin,
            kind: PluginNameConflictKind::Installed { installed_pid },
        }
        .into());
    }

    // ⑧：★再写账本（派生缓存，仅全成功；与账本重建复用 [`write_ledger_record`]）
    let record = write_ledger_record(home, plugin_id, scope, options.project_path, &m, env)?;
    tracing::info!(
        plugin = plugin_id,
        skills_staged = staged_skills.len(),
        "installed plugin"
    );
    Ok(record)
}

/// 卸载单个 plugin（删 plugin 独占外部 clone + 注销 skills + 级联 stop+remove bundled server + 删账本记录）/ Uninstall。
///
/// `scope=None` 删该 id 全部记录；指定 scope 仅删该 scope 记录。未安装 / 无匹配 → `false`（no-op）。
///
/// ⚠️ **文件归属（#271 方案 A）**：只有 **plugin 独占的外部 clone** `<home>/marketplace/.plugins/<mp>/<plugin>`
/// 会被删除（`remove_plugin_install_path`），且仅在**无 retained 账本记录**的 installPath 仍引用该 clone 时
/// （指定 scope 卸载保留其余 scope 仍依赖的 clone）。相对源（`source: "./plugins/x"`）的 `installPath` 位于
/// **共享 catalog clone**
/// `<home>/marketplace/<mp>/...`——该树是 marketplace 级缓存，兄弟插件、剩余 scope 与重装都依赖它，MUST NOT 在此删除；
/// 归 `marketplace remove` / reconcile / prune 清理。
///
/// 协议 §4.9.1-3 的「停摘自足」指停摘名单在账本移除**之前**取得且仅依赖账本字段，与 installPath 树是否删除无关。
///
/// # Errors
/// id 非法 / `remove_server` 失败 / 账本写失败 → [`PluginInstallError`]。
pub async fn uninstall_plugin(
    plugin_id: &str,
    registry: &mut SkillRegistry,
    home: &Path,
    options: UninstallOptions<'_>,
    non_plugin_bundle_ids: &HashSet<BundleId>,
    hooks: Option<&dyn McpInstallHooks>,
) -> Result<bool, PluginInstallError> {
    let (plugin, marketplace) = split_plugin_id(plugin_id)?;
    let marketplace_source = format!("{SOURCE_MARKETPLACE}:{marketplace}");
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
                    // #253：CLI 未安装 tracing subscriber——best-effort 清理失败必须对用户可见，
                    // 否则 stale flag 会静默残留（下次重装继承 true）。
                    #[cfg(feature = "cli")]
                    let _ = writeln!(
                        std::io::stderr(),
                        "warning: failed to clear enabledPlugins[{plugin_id}] in user scope: {e}; settings file left unchanged"
                    );
                }
                // #275 复审 🔴：dangling-intent 路径同样须注销 registry 条目（含 install stage 后留下
                // 的 orphan），否则「按 uninstall 指引换源」在本进程内仍是死路（跨源守卫会拒绝新源）。
                for name in plugin_skill_names_all(registry, &marketplace, &plugin) {
                    registry.unregister(&name);
                }
                clear_source_conflicts_for_plugin(registry, &plugin, &marketplace_source);
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

    let in_scope = |r: &InstalledPluginRecord| {
        options.scope.is_none() || r.extra.get("scope").and_then(Value::as_str) == options.scope
    };
    let targeted: Vec<&InstalledPluginRecord> = records.iter().filter(|r| in_scope(r)).collect();
    if targeted.is_empty() {
        tracing::info!(plugin = plugin_id, scope = ?options.scope, "uninstall: no record in scope (no-op)");
        return Ok(false);
    }
    let retained: Vec<&InstalledPluginRecord> = records.iter().filter(|r| !in_scope(r)).collect();

    // #271：按「引用」判定而非记录数——仅当**没有 retained 记录**的 installPath 仍落在该 plugin 独占外部
    // clone `<home>/marketplace/.plugins/<mp>/<plugin>` 内时才删除。跨 scope 源类型变化（如 user 为 Git 源、
    // project 为相对源）下，按记录数门控会在先卸 user 时误保留 ext clone，随后卸 project 又不再命中 → 永久泄漏。
    let ext_clone_still_referenced = retained.iter().any(|r| {
        r.install_path
            .as_deref()
            .filter(|s| !s.is_empty())
            .is_some_and(|ip| {
                is_external_plugin_install_path(Path::new(ip), home, &marketplace, &plugin)
            })
    });

    // 停摘自足：依赖名单在账本记录移除**之前**取得，且仅依赖账本自身字段（与 installPath 树是否删除无关）。
    let mut deps: Vec<BundleId> = Vec::new();
    for rec in &targeted {
        if let Some(ip) = rec.install_path.as_deref().filter(|s| !s.is_empty()) {
            if !ext_clone_still_referenced {
                // #271：只删 plugin 独占外部 clone；共享 catalog 树保留（归 marketplace 级清理）。
                remove_plugin_install_path(Path::new(ip), home, &marketplace, &plugin);
            }
        }
        for id in &rec.mcp_servers {
            if !deps.contains(id) {
                deps.push(id.clone());
            }
        }
    }

    // #275：注销 active **与 orphan**（installed_disabled）条目——按 uninstall 指引换源后，名字须在本进程
    // 内立即释放，否则后续 install/enable 仍被 registry 守卫拒绝。
    for name in plugin_skill_names_all(registry, &marketplace, &plugin) {
        registry.unregister(&name);
    }
    clear_source_conflicts_for_plugin(registry, &plugin, &marketplace_source);

    if !options.keep_servers {
        if let Some(h) = hooks {
            // #139/§4.9.1-2 回收判据：无其他 plugin 依赖 ∧ 非用户声明。用户/宿主自有 server 永不连坐。
            let other =
                crate::settings::reconciler::other_plugin_mcp_deps(&installed.account, plugin_id);
            let mut reclaim = crate::settings::reconciler::reclaimable_mcp_deps(
                &deps,
                &other,
                non_plugin_bundle_ids,
            );
            reclaim.sort();
            for id in &reclaim {
                h.remove_server(id).await?;
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
            #[cfg(feature = "cli")]
            let _ = writeln!(
                std::io::stderr(),
                "warning: failed to clear enabledPlugins[{plugin_id}] in {scope} scope: {e}; settings file left unchanged"
            );
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
    non_plugin_bundle_ids: &HashSet<BundleId>,
    hooks: Option<&dyn McpInstallHooks>,
) -> Result<bool, PluginInstallError> {
    let (plugin, marketplace) = split_plugin_id(plugin_id)?;
    let scope = options.scope.unwrap_or("user");
    // `changed`＝enabledPlugins 真的从 true→false（#115 R1）；已 disable 再 disable → false。server 停摘 /
    // skill orphan 幂等、仍无条件执行（不因 no-op 而漏兜底），仅**配置内容**变化由 `changed` 表达上抛。
    let changed = write_enabled_plugin(plugin_id, false, scope, options.project_path, options.env)?;
    if let Some(h) = hooks {
        // #139/§4.9.1-2 回收判据：只停摘无其他 plugin 依赖 ∧ 非用户声明者（用户/宿主自有 server 永不连坐）。
        let deps: Vec<BundleId> = bundled_servers_of(home, plugin_id, options.env)
            .into_iter()
            .collect();
        let installed = load_installed_plugins(Some(home), options.env);
        let other =
            crate::settings::reconciler::other_plugin_mcp_deps(&installed.account, plugin_id);
        let mut reclaim =
            crate::settings::reconciler::reclaimable_mcp_deps(&deps, &other, non_plugin_bundle_ids);
        reclaim.sort();
        for id in &reclaim {
            h.remove_server(id).await?;
        }
    }
    orphan_plugin_skills(registry, &marketplace, &plugin);
    tracing::info!(
        plugin = plugin_id,
        scope,
        "disabled plugin (servers detached, skills orphaned)"
    );
    Ok(changed)
}

/// 启用单个 plugin（廉价复原，**无需重 clone/重装**）/ Enable = cheap restore (no re-clone)。
///
/// ① **∀ 记录内容预检（零写盘）**：每条记录的 `installPath` 存在、其**声明内容**（entry / plugin.json /
/// 账本声明的 skills·mcpServers）可投影；任一记录不可物化 → 整体
/// [`PluginInstallError::ContentUnavailable`] 且保持禁用（#274）。真空白插件（未声明任何组件）放行——
/// 无内容可投影不是失败。② 写 `enabledPlugins[id]=true`；③ 复活 skills（re-stage：register_or_update
/// 翻活孤儿，refresh=false 复用 clone）——预检期望有 SKILL 但本次零注册（staging 被吞的失败）同样回滚为
/// 失败；④ 重挂 servers（失败原子回滚）。未安装 → 拒。
///
/// **不做自动恢复**：内容缺失不触发 re-materialize / re-clone；补救方式是重新安装（与 Python 异常语义对齐）。
///
/// **失败回滚（#274）**：恢复目标 scope 的 `enabledPlugins` **调用前原值**（absent → 删键，不升级成显式
/// false）；仅当该 pid **在任何 scope 都不再启用**时才 orphan 本 plugin 的活跃 SKILL（其他 scope 仍 true 时
/// 保留其投影）。⚠️ 已知边界：失败回滚**不摘除**此前已挂的 bundled server（enable 缺 `non_plugin_bundle_ids`
/// 判据，盲摘会连坐用户/宿主 server）——运行期清理由 reconcile 兜底，跟进见 #286。
///
/// ⚠️ **scope 契约**：同 [`disable_plugin`]。
///
/// # Returns
/// `Ok(changed)`＝`enabledPlugins[id]` 是否真的从非-true→true（#115 R1）；幂等 re-enable → `Ok(false)`。
///
/// # Errors
/// id 非法 / 未安装 / 内容不可投影（[`PluginInstallError::ContentUnavailable`]）/ manifest 畸形 / 冲突 /
/// settings 写失败 / 注入失败 → [`PluginInstallError`]。任何失败均回滚 `enabledPlugins` 与 SKILL 投影到
/// 调用前状态。
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

    // ⓪ #275 enable 防御：存量双装下同裸名已存在其它已装 pid → 写 enabledPlugins 之前拒绝（fail-closed，
    //    不自动挑选赢家；指引用户 uninstall 其中一个）。
    ensure_bare_plugin_not_installed(&plugin, plugin_id, home, env)?;

    // ① 内容预检（纯读、零写盘）：∀ 记录安装根存在 ∧ 声明内容可投影；任一记录不可物化 → 整体失败并回滚
    //    （#274：不得留 `enabled=true` 空投影，runtime-contract §2.4）。
    let existing = hooks
        .map(McpInstallHooks::existing_servers)
        .unwrap_or_default();
    let preflight =
        match preflight_enable_content(plugin_id, &plugin, &marketplace, home, &records, &existing)
        {
            Ok(p) => p,
            Err(e) => {
                rollback_preflight_enable(plugin_id, scope, options.project_path, env);
                rollback_plugin_skills_on_failure(
                    registry,
                    &marketplace,
                    &plugin,
                    plugin_id,
                    &records,
                    env,
                );
                return Err(e);
            }
        };
    let EnablePreflight {
        servers,
        expect_skills,
        usable_roots,
        ..
    } = preflight;

    // 回滚基线：本 scope `enabledPlugins` **调用前原值**（absent / true / false），失败时精确还原
    // （#274 复审 🟡2：原 absent 不得被升级成显式 false）。
    let prior_enabled = enabled_plugin_raw_value(plugin_id, scope, options.project_path, env);

    // ② 写 enabledPlugins[id]=true（预检 + 冲突通过后）。`changed`＝真的从非-true→true（#115 R1）；
    //    幂等 re-enable（已 true）→ false。skill 复活 / server 重挂幂等、仍无条件跑（半态修复），仅
    //    **配置内容**变化由 `changed` 上抛供 Computer 决定是否 bump config revision + 通知 robot。
    let changed = write_enabled_plugin(plugin_id, true, scope, options.project_path, env)?;

    // ③ 复活 skills（re-stage：register_or_update 翻活孤儿；复用既有 clone）。resolve/staging 失败均回滚，
    //    避免「标记已启用但零投影」。staging 的返回值＝本次实际注册的 SKILL 数——用于捕获被吞的 per-plugin 失败。
    let (source, _commit_sha) = match resolve_marketplace_source(&marketplace, home, env) {
        Ok(v) => v,
        Err(e) => {
            restore_enabled_plugin(plugin_id, scope, options.project_path, env, prior_enabled);
            rollback_plugin_skills_on_failure(
                registry,
                &marketplace,
                &plugin,
                plugin_id,
                &records,
                env,
            );
            return Err(e);
        }
    };
    let filter: HashSet<String> = std::iter::once(plugin.clone()).collect();
    // #275：让未安装 marketplace 源让位（installed intent 优先）；stage 后仍出现的跨源冲突
    // （如 user/mcp 占名或并发抢跑）→ fail-closed，恢复快照并回滚 enabledPlugins。
    let want_source = format!("{SOURCE_MARKETPLACE}:{marketplace}");
    let skill_prefix = format!("{plugin}:");
    let snapshot = registry.clone();
    clear_uninstalled_marketplace_skill_names(registry, home, env, &want_source, &skill_prefix);
    let _ = take_own_source_conflicts(registry, &want_source, &skill_prefix);
    let staged = stage_marketplace_skills(
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
    let source_conflicts = take_own_source_conflicts(registry, &want_source, &skill_prefix);
    if !source_conflicts.is_empty() {
        *registry = snapshot;
        // 恢复调用前原值（#274 复审 🟡2）；`changed == false`（原已 true）不改写。
        if changed {
            restore_enabled_plugin(plugin_id, scope, options.project_path, env, prior_enabled);
        }
        rollback_plugin_skills_on_failure(
            registry,
            &marketplace,
            &plugin,
            plugin_id,
            &records,
            env,
        );
        return Err(PluginNameConflictError {
            requested_pid: plugin_id.to_string(),
            plugin: plugin.clone(),
            kind: PluginNameConflictKind::SkillSource {
                conflicts: source_conflicts,
            },
        }
        .into());
    }
    if expect_skills && staged.is_empty() {
        // 恢复 staging 前的 registry 快照（#274 复审 🟡4），再按「是否仍有 scope 启用」决定 SKILL 投影。
        *registry = snapshot;
        restore_enabled_plugin(plugin_id, scope, options.project_path, env, prior_enabled);
        rollback_plugin_skills_on_failure(
            registry,
            &marketplace,
            &plugin,
            plugin_id,
            &records,
            env,
        );
        return Err(PluginInstallError::ContentUnavailable {
            plugin_id: plugin_id.to_string(),
            detail: format!(
                "skills_not_staged: declared SKILL packages under {usable_roots:?} produced zero registered skills"
            ),
        });
    }

    // ④ 重挂 servers——**原子回滚**（#94 点 4 / §10.6 回滚契约）：与 install 把账本写在最后不同，enable 先写
    //    `enabledPlugins=true`（步骤 ②）再挂 server，故 `register_server` 失败会留"标记已启用但 server 未挂"的
    //    半态——Sub-B 的 boot 恢复会据此复活半装 plugin。失败 → 摘除本次已挂 server + 恢复调用前
    //    `enabledPlugins` 原值 +（无任何 scope 仍启用时）orphan 本 plugin skills。
    if let Some(h) = hooks {
        // #139：回滚按 **bundle_id** 寻址——挂前取身份（cfg 随后 move 入 register_server）。同 plugin 内两个
        // 同名、显式异 bundle_id 的 server，回滚时精确摘本次已挂那条，不再字典序最小误摘（消解回滚破原子性）。
        let mut remounted: Vec<BundleId> = Vec::new();
        for cfg in servers {
            let bid = resolve_bundle_id(&cfg);
            if let Err(e) = h
                .register_server_with_input_scope(cfg, Some(plugin_id))
                .await
            {
                for done in &remounted {
                    if let Err(re) = h.remove_server(done).await {
                        tracing::warn!(bundle_id = %done.as_str(), error = %re, "enable rollback: remove_server failed");
                    }
                }
                // ⚠️ 已知窄边界：若**这一步**回写也失败（settings I/O 在回滚中途再挂），账本可能停在步骤 ② 的
                //    `enabledPlugins=true`，server 已摘 + skills 视其他 scope 而定，末态即降级前要消灭的半启用态。
                //    属"降级之上的再降级"，不再有更可靠的兜底——Sub-B 的 boot reconcile 须容此残窗（勿假设回滚后
                //    账本必为原值）。原 hook 错（`e`）优先上抛；回写错仅 WARN（避免淹没根因）。
                restore_enabled_plugin(plugin_id, scope, options.project_path, env, prior_enabled);
                rollback_plugin_skills_on_failure(
                    registry,
                    &marketplace,
                    &plugin,
                    plugin_id,
                    &records,
                    env,
                );
                return Err(e.into());
            }
            remounted.push(bid);
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
/// #139 迁移：检测并**丢弃**旧格式 `bundledMcpServers`（display-name 数组）/ discard the legacy name array。
///
/// 账本字段 `bundledMcpServers`（`Vec<String>` 名）已换为 `mcpServers`（`Vec<BundleId>`，F1 纯 bundle_id 数组）。
/// 旧键会被 [`InstalledPluginRecord`] 的 `#[serde(flatten)] extra` **静默吸收、并在下次写盘时原样回写**——本函数
/// 显式丢弃之，避免过期的 name 数组永久寄生。
///
/// **MUST NOT 写 name→bundle_id 映射**（那正是本 Epic 在拆的 name-join；依据协议 §4.9「删除它无损」）：丢弃后由
/// [`rematerialize_missing_ledger_records`](super::recovery::rematerialize_missing_ledger_records) 从
/// `installedPlugins` **意图**重建。旧名可能根本不是合法 `BundleId`（含 `.`/CJK/`__`），映射亦不可能可靠。
///
/// 每条被丢弃的记录发一条 WARN（静默丢弃盘上数据会让人措手不及）。返回被丢弃的记录数（幂等：再跑返回 0）。
///
/// **协议 §4.9.1-4：检测到旧格式 MUST 整条丢弃**（**非**仅剥 `bundledMcpServers` 键）。若只剥键、保留
/// `installPath`，[`rematerialize_missing_ledger_records`](super::recovery::rematerialize_missing_ledger_records)
/// 会因「派生缓存健在」（installPath 非空）**跳过**该记录，`mcpServers` 永空 ⇒ 此后 uninstall/disable/gc 的
/// teardown reclaim 恒空、已挂 bundled server **永不停摘（泄漏至进程重启）**。整条丢弃后，reconcile 对启用
/// plugin 从 `installedPlugins` 意图重物化派生缓存（含正确的 `mcpServers` bundle_id）。与 python
/// `store.py::_drop_legacy_mcp_dep_records` 逐字同构（判据取「**含** `bundledMcpServers` 键」）。
pub fn discard_legacy_bundled_mcp_servers(home: &Path, env: Option<&EnvMap>) -> usize {
    const LEGACY_KEY: &str = "bundledMcpServers";
    let installed = load_installed_plugins(Some(home), env);
    let stale: Vec<(String, Value)> = installed
        .account
        .plugins
        .iter()
        .flat_map(|(pid, recs)| {
            recs.iter()
                .filter_map(move |r| r.extra.get(LEGACY_KEY).map(|v| (pid.clone(), v.clone())))
        })
        .collect();
    if stale.is_empty() {
        return 0;
    }
    for (pid, v) in &stale {
        tracing::warn!(
            plugin = %pid, discarded = %v,
            "#139 migration: discarding legacy 'bundledMcpServers' ledger record (display-name array); the ledger \
             now stores 'mcpServers' as bundle_ids. No name→bundle_id mapping is performed by design — the whole \
             record is dropped and rebuilt from the installedPlugins intent (§4.9.1-4)."
        );
    }
    let n = stale.len();
    let _ = update_installed_plugins(
        move |data: &mut InstalledPluginsFile| {
            // 整条丢弃含旧键的记录；丢空的 pid 整个移除 ⇒ rematerialize 判「账本缺记录」→ 从意图重建。
            data.account.plugins.retain(|_pid, recs| {
                recs.retain(|r| !r.extra.contains_key(LEGACY_KEY));
                !recs.is_empty()
            });
        },
        Some(home),
        env,
    );
    n
}

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
        // #139：conflict_check 契约变更——`existing` 命中不再是冲突，而是「依赖已满足」（仅诊断、放行）；
        // 唯一保留的硬门 = 同一 plugin manifest 内两 server 塌成同一 bundle_id（身份坍缩）。
        let bid = resolve_bundle_id(&servers[0]);
        // 本地已有同 bundle_id（依赖已满足）→ 放行（推翻旧「外来同名硬抛」）。
        let existing: HashMap<BundleId, ServerName> =
            std::iter::once((bid.clone(), "dup".to_string())).collect();
        assert!(conflict_check(&servers, &existing, "audit@acme").is_ok());
        // 本地无该 bundle_id → 放行。
        assert!(conflict_check(&servers, &HashMap::new(), "audit@acme").is_ok());
        // 同一 manifest 内两 server 塌成同一 bundle_id → 硬抛（唯一保留硬门）。
        let collided = vec![servers[0].clone(), servers[0].clone()];
        assert!(conflict_check(&collided, &HashMap::new(), "audit@acme").is_err());
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

    /// 取 `git rev-parse HEAD`（#278 refresh 锚点：证明 pull 真执行）。
    fn git_head(dir: &Path) -> String {
        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", dir)
            .output()
            .expect("git available");
        assert!(
            out.status.success(),
            "git rev-parse HEAD: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
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
        let (home, env) = setup_home_with_catalog(tmp, source.clone()).await;
        (home, env, source)
    }

    /// 用给定 marketplace 源仓库建 home（预 clone catalog + 写 known_marketplaces.json），返回 (home, env)。
    ///
    /// `env` 恒注入 `XDG_CONFIG_HOME=<tmp>/cfg`：uninstall 的 `clear_enabled_plugin` 会经
    /// `raw_write::update_raw_settings` 创建父目录/锁文件——不隔离会落到开发者真实 `$HOME/.config`。
    async fn setup_home_with_catalog(tmp: &TempDir, source: Value) -> (PathBuf, EnvMap) {
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let cfg_home = tmp.path().join("cfg");
        fs::create_dir_all(&cfg_home).unwrap();
        let env: EnvMap = std::iter::once((
            "XDG_CONFIG_HOME".to_string(),
            cfg_home.to_string_lossy().into_owned(),
        ))
        .collect();
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
        (home, env)
    }

    /// 构造自定义 marketplace 源仓库：`entries` = (plugin 名, source Value)。字符串 source（相对路径）会自动
    /// 在 `<repo>/<rel>/skills/<plugin>-skill/SKILL.md` 落一份技能；返回 `{"type":"git","url":"file://…"}`。
    fn build_marketplace_repo_with_entries(repo: &Path, entries: &[(&str, Value)]) -> Value {
        fs::create_dir_all(repo.join(".tfrobot-plugin")).unwrap();
        let plugins: Vec<Value> = entries
            .iter()
            .map(|(name, source)| serde_json::json!({"name": name, "source": source}))
            .collect();
        fs::write(
            repo.join(".tfrobot-plugin/marketplace.json"),
            serde_json::to_string(&serde_json::json!({"plugins": plugins})).unwrap(),
        )
        .unwrap();
        for (name, source) in entries {
            if let Some(rel) = source.as_str() {
                let rel = rel.trim_start_matches("./");
                let skill = repo.join(rel).join("skills").join(format!("{name}-skill"));
                fs::create_dir_all(&skill).unwrap();
                fs::write(
                    skill.join("SKILL.md"),
                    format!("---\nname: {name}-skill\ndescription: {name}\n---\nbody"),
                )
                .unwrap();
            }
        }
        git(&["init", "-q"], repo);
        git(&["add", "-A"], repo);
        git(&["commit", "-qm", "init"], repo);
        serde_json::json!({"type": "git", "url": format!("file://{}", repo.display())})
    }

    /// 构造独立 plugin git 仓库（Git 源插件），返回 `file://…` URL。
    fn build_plugin_git_repo(repo: &Path) -> String {
        let skill = repo.join("skills/code-review");
        fs::create_dir_all(&skill).unwrap();
        fs::write(
            skill.join("SKILL.md"),
            "---\nname: code-review\ndescription: review code\n---\nbody",
        )
        .unwrap();
        git(&["init", "-q"], repo);
        git(&["add", "-A"], repo);
        git(&["commit", "-qm", "init"], repo);
        format!("file://{}", repo.display())
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
        fn existing_servers(&self) -> HashMap<BundleId, ServerName> {
            self.existing
                .iter()
                .filter_map(|n| BundleId::try_from(n.clone()).ok().map(|b| (b, n.clone())))
                .collect()
        }
        async fn register_server(&self, cfg: MCPServerConfig) -> Result<(), McpHookError> {
            if self.fail_register.as_deref() == Some(cfg.name()) {
                return Err(McpHookError(format!("boom on {}", cfg.name())));
            }
            self.registered.lock().unwrap().push(cfg.name().to_string());
            Ok(())
        }
        async fn remove_server(&self, id: &BundleId) -> Result<(), McpHookError> {
            self.removed.lock().unwrap().push(id.as_str().to_string());
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
        assert_eq!(
            record.mcp_servers,
            vec![BundleId::try_from("audit-mcp".to_string()).unwrap()]
        );
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

    /// #139/D3 **心智转变**（推翻旧「外来同名硬抛」）：plugin ↔ MCP server 是**依赖关系**、非所有关系。
    /// 同 `bundle_id` 本地已有 =「**依赖已满足**」⇒ **不阻止安装**、仅诊断提示、交 reconcile。
    /// 依据：协议 §身份正交性——name 允许碰撞、永不做键；旧硬门把「撞名」误判为「冲突」。
    ///
    /// （原测 `install_foreign_name_conflict_is_hard_fail_zero_change` 断言的正是被本 Issue 废止的契约。）
    #[tokio::test]
    async fn install_with_existing_same_bundle_id_is_dependency_satisfied_139() {
        let tmp = TempDir::new().unwrap();
        let (home, env, _src) = setup_installed_catalog(&tmp, true).await;
        let mut reg = SkillRegistry::new();
        // existing 已有同 bundle_id 的 audit-mcp（非本 plugin 带入）→ 依赖已满足，**不再**硬抛。
        let hooks = RecordingHooks::new().with_existing(&["audit-mcp"]);
        let res = install_plugin(
            "audit@acme",
            &mut reg,
            &home,
            InstallOptions {
                env: Some(&env),
                ..Default::default()
            },
            Some(&hooks),
        )
        .await;
        assert!(
            res.is_ok(),
            "同 bundle_id 本地已有 = 依赖已满足，MUST NOT 阻止安装（D3）：{res:?}"
        );
        // 装成即落账本（v0.3.0：install 物化但不激活——不挂 server、不注入 inputs）。
        assert!(!ledger_records(&home, &env, "audit@acme").is_empty());
        assert!(
            hooks.registered.lock().unwrap().is_empty(),
            "install 不激活：不挂 server（延到 enable）"
        );
    }

    /// #139：**唯一保留的硬门**——同一 plugin manifest 内两 server 归一**同 bundle_id**（身份坍缩，
    /// 不可自动消歧，须显式设不同 `bundleId`）。与「同 bundle_id 本地已有 → 放行」正交。
    #[test]
    fn conflict_check_hard_fails_on_intra_manifest_bundle_id_collapse_139() {
        let mk = |name: &str, bid: &str| -> MCPServerConfig {
            serde_json::from_value(serde_json::json!({
                "type": "stdio",
                "name": name,
                "bundle_id": bid,
                "server_parameters": {"command": "c"},
            }))
            .unwrap()
        };
        // 两条显式同 bundle_id → 身份坍缩 → fail-fast。
        let err = conflict_check(
            &[mk("alpha", "same"), mk("beta", "same")],
            &HashMap::new(),
            "p@mp",
        )
        .unwrap_err();
        assert!(
            err.0.contains("same bundle_id"),
            "身份坍缩 MUST fail-fast，实际: {}",
            err.0
        );
        // 反面：同 display 名、**异** bundle_id → 合法共存（name 非身份），MUST NOT 报错。
        assert!(
            conflict_check(
                &[mk("dup", "id-a"), mk("dup", "id-b")],
                &HashMap::new(),
                "p@mp"
            )
            .is_ok(),
            "同名异 bundle_id 是合法共存，不得误判冲突"
        );
    }

    /// #139/§4.9.1-4 迁移：旧格式记录（含 `bundledMcpServers` 键 + 非空 installPath）MUST **整条丢弃**
    /// （非仅剥键）——否则 `rematerialize_missing_ledger_records` 因「派生缓存健在」跳过、`mcpServers` 永空、
    /// teardown reclaim 永久失效。整条丢弃后 rematerialize 判「账本缺记录」→ 从意图重建。幂等。
    #[tokio::test]
    async fn discard_legacy_drops_whole_record_not_just_key_139() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let env = EnvMap::new();
        // seed 旧格式记录：installPath 非空 + extra 含 bundledMcpServers（name 数组）。
        update_installed_plugins(
            |data| {
                let mut extra = Map::new();
                extra.insert("scope".into(), serde_json::json!("user"));
                extra.insert(
                    "bundledMcpServers".into(),
                    serde_json::json!(["Legacy.Name", "another"]),
                );
                data.account.plugins.insert(
                    "audit@acme".into(),
                    vec![InstalledPluginRecord {
                        install_path: Some("/p".into()),
                        mcp_servers: vec![],
                        extra,
                    }],
                );
            },
            Some(home),
            Some(&env),
        )
        .unwrap();

        let n = discard_legacy_bundled_mcp_servers(home, Some(&env));
        assert_eq!(n, 1, "命中一条旧格式记录");
        let after = load_installed_plugins(Some(home), Some(&env)).account;
        assert!(
            after.plugins.get("audit@acme").is_none(),
            "含旧键的记录 MUST 整条丢弃（pid 移除）——仅剥键会让 rematerialize 跳过、mcpServers 永空"
        );
        // 幂等：无旧键 → 0。
        assert_eq!(discard_legacy_bundled_mcp_servers(home, Some(&env)), 0);
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
        let install_path = PathBuf::from(record.install_path.clone().unwrap());
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
            &HashSet::new(),
            Some(&hooks),
        )
        .await
        .unwrap();
        assert!(removed);
        assert!(reg.resolve("audit:code-review").is_none());
        assert!(ledger_records(&home, &env, "audit@acme").is_empty());
        assert!(
            install_path.exists(),
            "相对源 installPath（共享 catalog）MUST 在卸载后保留（#271 方案 A）"
        );
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
            &HashSet::new(),
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
            &HashSet::new(),
            Some(&hooks),
        )
        .await
        .unwrap();
        assert!(hooks.removed.lock().unwrap().is_empty());
    }

    // ---- #271：相对源卸载保留共享 catalog / 多 scope / 可重装 -------------------
    /// 父子/同源目录：卸载 alpha 不得删除 beta（`./plugin/nested`）与 gamma（同 `./plugin`）依赖的文件。
    #[tokio::test]
    async fn uninstall_relative_source_keeps_shared_catalog_siblings_271() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        let source = build_marketplace_repo_with_entries(
            &repo,
            &[
                ("alpha", serde_json::json!("./plugin")),
                ("beta", serde_json::json!("./plugin/nested")),
                ("gamma", serde_json::json!("./plugin")),
            ],
        );
        let (home, env) = setup_home_with_catalog(&tmp, source).await;
        let mut reg = SkillRegistry::new();
        let mut paths = HashMap::new();
        for pid in ["alpha@acme", "beta@acme", "gamma@acme"] {
            let rec = install_plugin(
                pid,
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
            paths.insert(pid.to_string(), PathBuf::from(rec.install_path.unwrap()));
        }
        assert_eq!(
            paths["alpha@acme"], paths["gamma@acme"],
            "alpha/gamma 同源 → 同一 installPath"
        );
        assert!(
            paths["beta@acme"].starts_with(&paths["alpha@acme"]),
            "beta 位于 alpha 子树"
        );

        let removed = uninstall_plugin(
            "alpha@acme",
            &mut reg,
            &home,
            UninstallOptions {
                env: Some(&env),
                ..Default::default()
            },
            &HashSet::new(),
            None,
        )
        .await
        .unwrap();
        assert!(removed);

        assert!(
            paths["alpha@acme"].exists(),
            "共享 catalog 树 MUST NOT 被卸载删除（#271 方案 A）"
        );
        assert!(
            paths["beta@acme"]
                .join("skills/beta-skill/SKILL.md")
                .exists(),
            "父子目录：beta 文件必须保留"
        );
        assert!(!ledger_records(&home, &env, "beta@acme").is_empty());
        assert!(!ledger_records(&home, &env, "gamma@acme").is_empty());
        assert!(ledger_records(&home, &env, "alpha@acme").is_empty());
    }

    /// 不相交目录对照：卸载 alpha 后 beta 文件保留；alpha 的 catalog 树同样保留（归 marketplace 级清理）。
    #[tokio::test]
    async fn uninstall_relative_source_disjoint_control_271() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        let source = build_marketplace_repo_with_entries(
            &repo,
            &[
                ("alpha", serde_json::json!("./plugins/alpha")),
                ("beta", serde_json::json!("./plugins/beta")),
            ],
        );
        let (home, env) = setup_home_with_catalog(&tmp, source).await;
        let mut reg = SkillRegistry::new();
        let mut paths = HashMap::new();
        for pid in ["alpha@acme", "beta@acme"] {
            let rec = install_plugin(
                pid,
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
            paths.insert(pid.to_string(), PathBuf::from(rec.install_path.unwrap()));
        }

        uninstall_plugin(
            "alpha@acme",
            &mut reg,
            &home,
            UninstallOptions {
                env: Some(&env),
                ..Default::default()
            },
            &HashSet::new(),
            None,
        )
        .await
        .unwrap();

        assert!(paths["alpha@acme"].exists());
        assert!(paths["beta@acme"]
            .join("skills/beta-skill/SKILL.md")
            .exists());
        assert!(!ledger_records(&home, &env, "beta@acme").is_empty());
    }

    /// 多 scope：仅卸 user 时，project 记录与其依赖的 catalog 文件必须保留（能力面拆除不在本单）。
    #[tokio::test]
    async fn uninstall_user_scope_keeps_project_record_and_catalog_271() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        let source =
            build_marketplace_repo_with_entries(&repo, &[("audit", serde_json::json!("./plugin"))]);
        let (home, env) = setup_home_with_catalog(&tmp, source).await;
        let project = tmp.path().join("proj");
        fs::create_dir_all(&project).unwrap();

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
        install_plugin(
            "audit@acme",
            &mut reg,
            &home,
            InstallOptions {
                scope: Some("project"),
                project_path: Some(project.to_str().unwrap()),
                env: Some(&env),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
        assert_eq!(ledger_records(&home, &env, "audit@acme").len(), 2);

        uninstall_plugin(
            "audit@acme",
            &mut reg,
            &home,
            UninstallOptions {
                scope: Some("user"),
                env: Some(&env),
                ..Default::default()
            },
            &HashSet::new(),
            None,
        )
        .await
        .unwrap();

        let recs = ledger_records(&home, &env, "audit@acme");
        assert_eq!(recs.len(), 1, "仅 user scope 记录被移除");
        assert_eq!(
            recs[0].extra.get("scope").and_then(Value::as_str),
            Some("project")
        );
        let ip = recs[0].install_path.as_deref().unwrap();
        assert!(
            Path::new(ip).exists(),
            "剩余 project scope 依赖的 catalog 文件 MUST 保留"
        );
        assert!(
            load_installed_plugins_intent(Some(&home), Some(&env))
                .account
                .installed_plugins
                .contains("audit@acme"),
            "仍有账本记录 → 安装意图保留"
        );
    }

    /// 卸载相对源插件后可重装（默认 refresh=false 与 refresh=true 两路）。
    #[tokio::test]
    async fn reinstall_after_uninstall_relative_source_succeeds_271() {
        let tmp = TempDir::new().unwrap();
        let (home, env, _src) = setup_installed_catalog(&tmp, false).await;
        let mut reg = SkillRegistry::new();

        let first = install_plugin(
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
        let install_path = PathBuf::from(first.install_path.unwrap());
        let removed = uninstall_plugin(
            "audit@acme",
            &mut reg,
            &home,
            UninstallOptions {
                env: Some(&env),
                ..Default::default()
            },
            &HashSet::new(),
            None,
        )
        .await
        .unwrap();
        assert!(removed, "首次卸载应实际移除");
        assert!(
            ledger_records(&home, &env, "audit@acme").is_empty(),
            "卸载后账本已清"
        );
        assert!(
            install_path.exists(),
            "共享 catalog installPath 保留（旧实现此处已被删除）"
        );
        // 旧实现：catalog 子树被删 → "plugin root not found after source resolution"。
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
        .expect("默认重装应成功");
        let removed_again = uninstall_plugin(
            "audit@acme",
            &mut reg,
            &home,
            UninstallOptions {
                env: Some(&env),
                ..Default::default()
            },
            &HashSet::new(),
            None,
        )
        .await
        .unwrap();
        assert!(removed_again, "二次卸载应实际移除");
        assert!(ledger_records(&home, &env, "audit@acme").is_empty());
        assert!(install_path.exists(), "二次卸载后 catalog 仍保留");
        install_plugin(
            "audit@acme",
            &mut reg,
            &home,
            InstallOptions {
                refresh: true,
                env: Some(&env),
                ..Default::default()
            },
            None,
        )
        .await
        .expect("refresh=true 重装应成功");
    }

    /// #278：0.5.1 之前版本卸载已删的 catalog 子树无法可靠自愈——重装失败错误必须携带恢复指引
    /// （B 方案：不自动迁移，只给命令 + 文档）。install 在 materialize 阶段即失败（不会走到 staging
    /// 的 git pull），`refresh` 标志只影响 staging，不改变本入口错误——这里验证两条入口都报同一提示。
    #[tokio::test]
    async fn install_missing_catalog_subtree_reports_recovery_hint_278() {
        let tmp = TempDir::new().unwrap();
        let (home, env, _src) = setup_installed_catalog(&tmp, false).await;
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

        // 精确复现 0.5.0 破坏态：旧卸载同时删除账本/安装意图与共享 catalog 子树。
        // 0.5.1 的 uninstall 已不再删共享子树（#271），故先 uninstall 再手删子树。
        let removed = uninstall_plugin(
            "audit@acme",
            &mut reg,
            &home,
            UninstallOptions {
                env: Some(&env),
                ..Default::default()
            },
            &HashSet::new(),
            None,
        )
        .await
        .unwrap();
        assert!(removed, "首次卸载应实际移除账本记录");
        let rel = home.join("marketplace/acme/plugins/audit");
        assert!(
            rel.exists(),
            "#271 之后 uninstall 不得再删共享 catalog 子树"
        );
        fs::remove_dir_all(&rel).unwrap();

        for refresh in [false, true] {
            let err = install_plugin(
                "audit@acme",
                &mut reg,
                &home,
                InstallOptions {
                    refresh,
                    env: Some(&env),
                    ..Default::default()
                },
                None,
            )
            .await
            .expect_err("catalog 子树缺失时重装应失败");
            let msg = err.to_string();
            assert!(
                msg.contains("marketplace remove <name>"),
                "refresh={refresh} 须给级联移除命令: {msg}"
            );
            assert!(
                msg.contains("marketplace add <git-url> --trust --name <name>"),
                "refresh={refresh} 须给带 --name 的重加命令: {msg}"
            );
            assert!(
                msg.contains("plugin enable <plugin>@<name>"),
                "refresh={refresh} 须提醒 re-enable（install 不激活）: {msg}"
            );
            assert!(
                msg.contains(
                    "https://github.com/A2C-SMCP/rust-sdk/blob/v0.5.1/docs/computer/marketplace-0.5.1-migration.md"
                ),
                "refresh={refresh} 须给出可打开的文档 URL: {msg}"
            );
        }
    }

    /// #278 文档论断锚点：`marketplace refresh`（stage refresh=true）**不恢复**被本地删除的
    /// catalog 子树——`git pull --ff-only` 保留本地删除。为排除「refresh 根本没跑」的假绿，
    /// 上游推进一个不触碰被删路径的 commit：断言 clone HEAD 前进到该 commit（证明 pull 执行），
    /// 且被删子树仍缺失。（仅 pull 失败触发全量 re-clone 时才可能恢复，故文档写「无法可靠自愈」。）
    /// 此用例直接驱动 staging 层，不依赖 install 的 materialize 失败路径。
    #[tokio::test]
    async fn refresh_true_pulls_upstream_but_keeps_deleted_catalog_subtree_278() {
        let tmp = TempDir::new().unwrap();
        let (home, env, source) = setup_installed_catalog(&tmp, false).await;
        let catalog = home.join("marketplace/acme");
        let rel = catalog.join("plugins/audit");
        fs::remove_dir_all(&rel).unwrap();
        let head_before = git_head(&catalog);

        // 上游推进：新增与 plugins/audit 无关的文件并提交（pull 可成功且不触碰被删路径）。
        let src_repo = tmp.path().join("repo");
        fs::write(src_repo.join("UPSTREAM_CHANGELOG.md"), "advance\n").unwrap();
        git(&["add", "-A"], &src_repo);
        git(&["commit", "-qm", "upstream advance"], &src_repo);
        let upstream_head = git_head(&src_repo);
        assert_ne!(head_before, upstream_head, "夹具应制造上游新提交");

        let mut reg = SkillRegistry::new();
        let skills = stage_marketplace_skills(
            "acme",
            &source,
            &mut reg,
            &home,
            MarketplaceStageOptions {
                refresh: true,
                env: Some(&env),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(
            git_head(&catalog),
            upstream_head,
            "refresh 必须真实执行 git pull（clone HEAD 前进到上游），否则本用例为假绿"
        );
        assert!(
            !rel.exists(),
            "pull 成功后本地删除仍保留——refresh 不得恢复被删 catalog 子树（文档论断锚点）"
        );
        assert!(
            !skills.iter().any(|s| s.starts_with("audit:")),
            "缺失子树不应注册技能: {skills:?}"
        );
    }

    /// 外部 Git clone：卸载仍删除 plugin 独占的 `.plugins/<mp>/<plugin>`，不碰共享 catalog。
    #[tokio::test]
    async fn uninstall_external_git_plugin_removes_clone_root_271() {
        let tmp = TempDir::new().unwrap();
        let plugin_repo = tmp.path().join("plugin-repo");
        let url = build_plugin_git_repo(&plugin_repo);
        let repo = tmp.path().join("repo");
        let source = build_marketplace_repo_with_entries(
            &repo,
            &[("audit", serde_json::json!({"source": "url", "url": url}))],
        );
        let (home, env) = setup_home_with_catalog(&tmp, source).await;
        let mut reg = SkillRegistry::new();
        let rec = install_plugin(
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
        let ip = PathBuf::from(rec.install_path.unwrap());
        let ext_root = home.join("marketplace/.plugins/acme/audit");
        assert_eq!(ip, ext_root, "Git 源 installPath = 独占外部 clone 根");
        assert!(ext_root.exists());

        uninstall_plugin(
            "audit@acme",
            &mut reg,
            &home,
            UninstallOptions {
                env: Some(&env),
                ..Default::default()
            },
            &HashSet::new(),
            None,
        )
        .await
        .unwrap();

        assert!(!ext_root.exists(), "独占外部 clone MUST 随卸载删除");
        assert!(
            home.join("marketplace/acme").exists(),
            "共享 catalog MUST NOT 因外部 plugin 卸载删除"
        );
        assert!(ledger_records(&home, &env, "audit@acme").is_empty());
    }

    /// 多 scope × Git 源：仅卸 user 时其余 scope 仍依赖同一外部 clone，不得删除；全卸尽后才删。
    #[tokio::test]
    async fn uninstall_git_plugin_user_scope_keeps_project_clone_271() {
        let tmp = TempDir::new().unwrap();
        let plugin_repo = tmp.path().join("plugin-repo");
        let url = build_plugin_git_repo(&plugin_repo);
        let repo = tmp.path().join("repo");
        let source = build_marketplace_repo_with_entries(
            &repo,
            &[("audit", serde_json::json!({"source": "url", "url": url}))],
        );
        let (home, env) = setup_home_with_catalog(&tmp, source).await;
        let project = tmp.path().join("proj");
        fs::create_dir_all(&project).unwrap();
        let ext_root = home.join("marketplace/.plugins/acme/audit");

        let mut reg = SkillRegistry::new();
        for opts in [
            InstallOptions {
                env: Some(&env),
                ..Default::default()
            },
            InstallOptions {
                scope: Some("project"),
                project_path: Some(project.to_str().unwrap()),
                env: Some(&env),
                ..Default::default()
            },
        ] {
            install_plugin("audit@acme", &mut reg, &home, opts, None)
                .await
                .unwrap();
        }
        assert!(ext_root.exists());

        uninstall_plugin(
            "audit@acme",
            &mut reg,
            &home,
            UninstallOptions {
                scope: Some("user"),
                env: Some(&env),
                ..Default::default()
            },
            &HashSet::new(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(ledger_records(&home, &env, "audit@acme").len(), 1);
        assert!(
            ext_root.exists(),
            "剩余 project scope 仍依赖同一外部 clone → MUST 保留"
        );

        uninstall_plugin(
            "audit@acme",
            &mut reg,
            &home,
            UninstallOptions {
                env: Some(&env),
                ..Default::default()
            },
            &HashSet::new(),
            None,
        )
        .await
        .unwrap();
        assert!(!ext_root.exists(), "全部 scope 移除后外部 clone 删除");
    }

    /// 跨 scope 源类型变化：user 装 Git 源（ext clone）、project 装相对源（catalog）。先卸 user 时 retained
    /// project 记录**不引用** ext clone → 必须删除，不得因按「记录数」门控而永久泄漏。
    #[tokio::test]
    async fn uninstall_user_git_scope_with_project_relative_does_not_leak_ext_clone_271() {
        let tmp = TempDir::new().unwrap();
        let plugin_repo = tmp.path().join("plugin-repo");
        let url = build_plugin_git_repo(&plugin_repo);
        let repo = tmp.path().join("repo");
        let source_git = build_marketplace_repo_with_entries(
            &repo,
            &[("audit", serde_json::json!({"source": "url", "url": url}))],
        );
        let (home, env) = setup_home_with_catalog(&tmp, source_git.clone()).await;
        let project = tmp.path().join("proj");
        fs::create_dir_all(&project).unwrap();
        let ext_root = home.join("marketplace/.plugins/acme/audit");

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
        assert!(ext_root.exists(), "Git 源 user 记录落在独占 ext clone");

        // 上游 marketplace 改为相对源并 refresh（模拟跨 scope 源类型变化）。
        fs::write(
            repo.join(".tfrobot-plugin/marketplace.json"),
            r#"{"plugins":[{"name":"audit","source":"./plugin"}]}"#,
        )
        .unwrap();
        let skill = repo.join("plugin/skills/audit-skill");
        fs::create_dir_all(&skill).unwrap();
        fs::write(
            skill.join("SKILL.md"),
            "---\nname: audit-skill\ndescription: audit\n---\nbody",
        )
        .unwrap();
        git(&["add", "-A"], &repo);
        git(&["commit", "-qm", "relative"], &repo);
        let mut throwaway = SkillRegistry::new();
        stage_marketplace_skills(
            "acme",
            &source_git,
            &mut throwaway,
            &home,
            MarketplaceStageOptions {
                refresh: true,
                ..Default::default()
            },
        )
        .await;

        install_plugin(
            "audit@acme",
            &mut reg,
            &home,
            InstallOptions {
                scope: Some("project"),
                project_path: Some(project.to_str().unwrap()),
                env: Some(&env),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
        let recs = ledger_records(&home, &env, "audit@acme");
        assert_eq!(recs.len(), 2);
        let project_path = recs
            .iter()
            .find(|r| r.extra.get("scope").and_then(Value::as_str) == Some("project"))
            .and_then(|r| r.install_path.clone())
            .unwrap();
        assert!(
            Path::new(&project_path).starts_with(home.join("marketplace/acme")),
            "project 记录应落在共享 catalog：{project_path}"
        );

        // 先卸 user：retained project 不引用 ext clone → ext clone 删除（不永久泄漏）。
        let removed = uninstall_plugin(
            "audit@acme",
            &mut reg,
            &home,
            UninstallOptions {
                scope: Some("user"),
                env: Some(&env),
                ..Default::default()
            },
            &HashSet::new(),
            None,
        )
        .await
        .unwrap();
        assert!(removed);
        assert!(
            !ext_root.exists(),
            "retained 记录不引用 ext clone → 先卸 user 时即删除（#276 复审 🟡1）"
        );
        let recs = ledger_records(&home, &env, "audit@acme");
        assert_eq!(recs.len(), 1);
        assert!(
            Path::new(&project_path).exists(),
            "project 的 catalog 路径保留"
        );

        // 再卸尽：catalog 仍保留。
        uninstall_plugin(
            "audit@acme",
            &mut reg,
            &home,
            UninstallOptions {
                env: Some(&env),
                ..Default::default()
            },
            &HashSet::new(),
            None,
        )
        .await
        .unwrap();
        assert!(ledger_records(&home, &env, "audit@acme").is_empty());
        assert!(Path::new(&project_path).exists());
    }

    /// 真实 git-subdir 源：install → uninstall 删除整个独占 clone 根，共享 catalog 保留。
    #[tokio::test]
    async fn uninstall_git_subdir_plugin_removes_clone_root_271() {
        let tmp = TempDir::new().unwrap();
        let plugin_repo = tmp.path().join("plugin-repo");
        let skill = plugin_repo.join("pkg/sub/skills/code-review");
        fs::create_dir_all(&skill).unwrap();
        fs::write(
            skill.join("SKILL.md"),
            "---\nname: code-review\ndescription: review code\n---\nbody",
        )
        .unwrap();
        git(&["init", "-q"], &plugin_repo);
        // file:// 部分克隆（--filter=tree:0 --sparse）依赖服务端 allowFilter。
        git(&["config", "uploadpack.allowFilter", "true"], &plugin_repo);
        git(&["add", "-A"], &plugin_repo);
        git(&["commit", "-qm", "init"], &plugin_repo);
        let url = format!("file://{}", plugin_repo.display());
        let repo = tmp.path().join("repo");
        let source = build_marketplace_repo_with_entries(
            &repo,
            &[(
                "audit",
                serde_json::json!({"source": "git-subdir", "url": url, "path": "pkg/sub"}),
            )],
        );
        let (home, env) = setup_home_with_catalog(&tmp, source).await;
        let ext_root = home.join("marketplace/.plugins/acme/audit");

        let mut reg = SkillRegistry::new();
        let rec = install_plugin(
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
        let ip = PathBuf::from(rec.install_path.unwrap());
        assert_eq!(
            ip,
            ext_root.join("pkg/sub"),
            "git-subdir installPath 为 clone 根后代"
        );

        uninstall_plugin(
            "audit@acme",
            &mut reg,
            &home,
            UninstallOptions {
                env: Some(&env),
                ..Default::default()
            },
            &HashSet::new(),
            None,
        )
        .await
        .unwrap();
        assert!(!ext_root.exists(), "git-subdir → 删除整个独占 clone 根");
        assert!(
            home.join("marketplace/acme").exists(),
            "共享 catalog 不因外部 plugin 卸载删除"
        );
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
            &HashSet::new(),
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
            &HashSet::new(),
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
            &HashSet::new(),
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

    // ---- #274：enable 内容缺失 fail-fast + 回滚 ---------------------------------
    fn cfg_env(tmp: &TempDir, mut env: EnvMap) -> EnvMap {
        let cfg_home = tmp.path().join("cfg");
        fs::create_dir_all(&cfg_home).unwrap();
        env.insert(
            "XDG_CONFIG_HOME".to_string(),
            cfg_home.to_string_lossy().into_owned(),
        );
        env
    }

    #[tokio::test]
    async fn enable_skills_only_missing_root_fails_and_disables_274() {
        let tmp = TempDir::new().unwrap();
        let (home, env, _src) = setup_installed_catalog(&tmp, false).await;
        let env = cfg_env(&tmp, env);
        let mut reg = SkillRegistry::new();
        let rec = install_plugin(
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
        let root = PathBuf::from(rec.install_path.unwrap());
        fs::remove_dir_all(&root).unwrap();

        let hooks = RecordingHooks::new();
        let err = enable_plugin(
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
        .unwrap_err();
        assert!(
            matches!(err, PluginInstallError::ContentUnavailable { .. }),
            "got {err:?}"
        );
        assert!(
            err.to_string().contains("install_path_missing"),
            "诊断须含稳定原因码：{err}"
        );
        assert_ne!(
            enabled_flag(&env, "audit@acme"),
            Some(true),
            "不得保留 enabled=true 空投影"
        );
        assert!(reg.resolve("audit:code-review").is_none());
        assert!(hooks.registered.lock().unwrap().is_empty());
        assert!(
            !ledger_records(&home, &env, "audit@acme").is_empty(),
            "记录仍在 = installed_disabled"
        );
    }

    #[tokio::test]
    async fn enable_server_record_missing_root_fails_274() {
        let tmp = TempDir::new().unwrap();
        let (home, env, _src) = setup_installed_catalog(&tmp, true).await;
        let env = cfg_env(&tmp, env);
        let mut reg = SkillRegistry::new();
        let rec = install_plugin(
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
        fs::remove_dir_all(Path::new(rec.install_path.as_deref().unwrap())).unwrap();

        let hooks = RecordingHooks::new();
        let err = enable_plugin(
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
        .unwrap_err();
        assert!(matches!(err, PluginInstallError::ContentUnavailable { .. }));
        assert!(hooks.registered.lock().unwrap().is_empty());
        assert_ne!(enabled_flag(&env, "audit@acme"), Some(true));
    }

    #[tokio::test]
    async fn enable_missing_root_isolated_to_plugin_274() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        let source = build_marketplace_repo_with_entries(
            &repo,
            &[
                ("alpha", serde_json::json!("./plugin")),
                ("beta", serde_json::json!("./plugin/nested")),
            ],
        );
        let (home, env) = setup_home_with_catalog(&tmp, source).await;
        let mut reg = SkillRegistry::new();
        let mut paths = HashMap::new();
        for pid in ["alpha@acme", "beta@acme"] {
            let rec = install_plugin(
                pid,
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
            paths.insert(pid.to_string(), PathBuf::from(rec.install_path.unwrap()));
        }
        fs::remove_dir_all(&paths["beta@acme"]).unwrap();

        let err = enable_plugin(
            "beta@acme",
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
        assert!(matches!(err, PluginInstallError::ContentUnavailable { .. }));
        assert_ne!(enabled_flag(&env, "beta@acme"), Some(true));
        assert!(!ledger_records(&home, &env, "beta@acme").is_empty());

        // 兄弟插件不受影响：alpha 根仍在 → enable 成功。
        enable_plugin(
            "alpha@acme",
            &mut reg,
            &home,
            EnableOptions {
                env: Some(&env),
                ..Default::default()
            },
            None,
        )
        .await
        .expect("兄弟插件应不受影响");
        assert!(reg.resolve("alpha:alpha-skill").is_some());
        assert_eq!(enabled_flag(&env, "alpha@acme"), Some(true));
        assert!(paths["alpha@acme"].exists());
    }

    #[tokio::test]
    async fn enable_multi_scope_any_missing_fails_274() {
        // ∀ 判据（#276 复审口径，对齐 Python `ledger_entry_fully_materialized`）：enable 聚合全部记录的
        // bundled servers 进同一投影，因此**任一**记录不可物化其声明内容 → 整体失败 + 回滚。
        let tmp = TempDir::new().unwrap();
        let (home, env, _src) = setup_installed_catalog(&tmp, false).await;
        let env = cfg_env(&tmp, env);
        let mut reg = SkillRegistry::new();
        let rec = install_plugin(
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
        let good_root = PathBuf::from(rec.install_path.unwrap());
        let good_s = good_root.to_string_lossy().into_owned();
        let missing_s = tmp.path().join("gone").to_string_lossy().into_owned();
        // 追加一条 install_path 缺失的 project 记录：账本多记录、部分缺失。
        {
            let missing_s = missing_s.clone();
            store::update_installed_plugins(
                move |data| {
                    let mut extra = Map::new();
                    extra.insert("scope".to_string(), Value::String("project".to_string()));
                    data.account.plugins.get_mut("audit@acme").unwrap().push(
                        InstalledPluginRecord {
                            install_path: Some(missing_s),
                            mcp_servers: Vec::new(),
                            extra,
                        },
                    );
                },
                Some(&home),
                Some(&env),
            )
            .unwrap();
        }

        // 一部分记录缺根 → 整体 Err，且零写（不得留 enabled=true）。
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
        assert!(matches!(err, PluginInstallError::ContentUnavailable { .. }));
        assert!(
            err.to_string().contains("install_path_missing"),
            "应指明缺失记录：{err}"
        );
        assert_ne!(
            enabled_flag(&env, "audit@acme"),
            Some(true),
            "多 scope 任一缺失 → MUST NOT 启用"
        );
        assert!(reg.resolve("audit:code-review").is_none());
        assert_eq!(ledger_records(&home, &env, "audit@acme").len(), 2);

        // 移除损坏记录（恢复单条健康记录）→ 全部记录健康 → enable 成功。
        {
            let good_s = good_s.clone();
            store::update_installed_plugins(
                move |data| {
                    data.account
                        .plugins
                        .get_mut("audit@acme")
                        .unwrap()
                        .retain(|r| r.install_path.as_deref() == Some(good_s.as_str()));
                },
                Some(&home),
                Some(&env),
            )
            .unwrap();
        }
        enable_plugin(
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
        .expect("全部记录健康时应成功");
        assert!(reg.resolve("audit:code-review").is_some());
        assert_eq!(enabled_flag(&env, "audit@acme"), Some(true));
        assert!(good_root.exists());
    }

    #[tokio::test]
    async fn enable_stale_true_with_missing_root_rolls_back_274() {
        let tmp = TempDir::new().unwrap();
        let (home, env, _src) = setup_installed_catalog(&tmp, false).await;
        let env = cfg_env(&tmp, env);
        let mut reg = SkillRegistry::new();
        let rec = install_plugin(
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
        let root = PathBuf::from(rec.install_path.unwrap());
        // 先正常启用（flag=true、skill 活跃），再损坏根。
        enable_plugin(
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
        .unwrap();
        assert!(reg.resolve("audit:code-review").is_some());
        fs::remove_dir_all(&root).unwrap();

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
        assert!(matches!(err, PluginInstallError::ContentUnavailable { .. }));
        assert_eq!(
            enabled_flag(&env, "audit@acme"),
            Some(false),
            "陈旧 enabled=true 必须回滚为 false"
        );
        assert!(
            reg.resolve("audit:code-review").is_none(),
            "skills 重新 orphan"
        );
    }

    #[tokio::test]
    async fn enable_declared_skills_missing_fails_274() {
        // 声明态判据：plugin.json 显式声明 skills 组件 → 实际零投影必须失败。
        let tmp = TempDir::new().unwrap();
        let (home, env, _src) = setup_installed_catalog(&tmp, false).await;
        let env = cfg_env(&tmp, env);
        let mut reg = SkillRegistry::new();
        let rec = install_plugin(
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
        let root = PathBuf::from(rec.install_path.unwrap());
        fs::create_dir_all(root.join(".tfrobot-plugin")).unwrap();
        fs::write(
            root.join(".tfrobot-plugin/plugin.json"),
            r#"{"skills": ["skills"]}"#,
        )
        .unwrap();
        fs::remove_dir_all(root.join("skills")).unwrap();

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
        assert!(matches!(err, PluginInstallError::ContentUnavailable { .. }));
        assert!(
            err.to_string().contains("no_projectable_content"),
            "声明 skills 但零投影：{err}"
        );
        assert_ne!(enabled_flag(&env, "audit@acme"), Some(true));
    }

    #[tokio::test]
    async fn enable_truly_empty_plugin_succeeds_274() {
        // 「真空白插件」（未声明任何组件、无 skills/servers 文件）→ 无内容可投影不是失败。
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join(".tfrobot-plugin")).unwrap();
        fs::write(
            repo.join(".tfrobot-plugin/marketplace.json"),
            r#"{"plugins":[{"name":"empty","source":"./empty"}]}"#,
        )
        .unwrap();
        fs::create_dir_all(repo.join("empty")).unwrap();
        fs::write(repo.join("empty/README.md"), "empty plugin").unwrap();
        git(&["init", "-q"], &repo);
        git(&["add", "-A"], &repo);
        git(&["commit", "-qm", "init"], &repo);
        let source =
            serde_json::json!({"type": "git", "url": format!("file://{}", repo.display())});
        let (home, env) = setup_home_with_catalog(&tmp, source).await;

        let mut reg = SkillRegistry::new();
        install_plugin(
            "empty@acme",
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
        enable_plugin(
            "empty@acme",
            &mut reg,
            &home,
            EnableOptions {
                env: Some(&env),
                ..Default::default()
            },
            None,
        )
        .await
        .expect("未声明内容的空插件应允许启用");
        assert_eq!(enabled_flag(&env, "empty@acme"), Some(true));
    }

    /// 🔴1：另一 scope 仍启用时，本 scope 失败回滚 MUST NOT orphan 其 SKILL 投影
    /// （否则「合并 enabled=true + 零 SKILL」）。
    #[tokio::test]
    async fn enable_failing_scope_keeps_other_scope_enabled_and_skills_274() {
        let tmp = TempDir::new().unwrap();
        let plugin_repo = tmp.path().join("plugin-repo");
        let url = build_plugin_git_repo(&plugin_repo);
        let repo = tmp.path().join("repo");
        let source_git = build_marketplace_repo_with_entries(
            &repo,
            &[("audit", serde_json::json!({"source": "url", "url": url}))],
        );
        let (home, env) = setup_home_with_catalog(&tmp, source_git.clone()).await;
        let project = tmp.path().join("proj");
        fs::create_dir_all(&project).unwrap();
        let mut reg = SkillRegistry::new();

        // user：Git 源（外部 clone）。
        let user_rec = install_plugin(
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
        let ext_root = PathBuf::from(user_rec.install_path.unwrap());

        // 上游改为相对源并 refresh 后装 project scope。
        fs::write(
            repo.join(".tfrobot-plugin/marketplace.json"),
            r#"{"plugins":[{"name":"audit","source":"./plugin"}]}"#,
        )
        .unwrap();
        let skill = repo.join("plugin/skills/audit-skill");
        fs::create_dir_all(&skill).unwrap();
        fs::write(
            skill.join("SKILL.md"),
            "---\nname: audit-skill\ndescription: audit\n---\nbody",
        )
        .unwrap();
        git(&["add", "-A"], &repo);
        git(&["commit", "-qm", "relative"], &repo);
        let mut throwaway = SkillRegistry::new();
        stage_marketplace_skills(
            "acme",
            &source_git,
            &mut throwaway,
            &home,
            MarketplaceStageOptions {
                refresh: true,
                ..Default::default()
            },
        )
        .await;
        install_plugin(
            "audit@acme",
            &mut reg,
            &home,
            InstallOptions {
                scope: Some("project"),
                project_path: Some(project.to_str().unwrap()),
                env: Some(&env),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();

        // 先让 project scope 正常启用（skills 活跃、project flag=true）。
        enable_plugin(
            "audit@acme",
            &mut reg,
            &home,
            EnableOptions {
                scope: Some("project"),
                project_path: Some(project.to_str().unwrap()),
                env: Some(&env),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
        assert!(reg.resolve("audit:audit-skill").is_some());
        assert_eq!(
            enabled_plugin_raw_value(
                "audit@acme",
                "project",
                Some(project.to_str().unwrap()),
                Some(&env)
            ),
            Some(true)
        );

        // 破坏 user 记录根 → enable(user) 预检失败。
        fs::remove_dir_all(&ext_root).unwrap();
        let err = enable_plugin(
            "audit@acme",
            &mut reg,
            &home,
            EnableOptions {
                scope: Some("user"),
                project_path: Some(project.to_str().unwrap()),
                env: Some(&env),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, PluginInstallError::ContentUnavailable { .. }));
        // 其他 scope 仍 true → SKILL 投影 MUST 保留（不得 orphan）。
        assert_eq!(
            enabled_plugin_raw_value(
                "audit@acme",
                "project",
                Some(project.to_str().unwrap()),
                Some(&env)
            ),
            Some(true),
            "project scope 的 enabled=true 不得被失败回滚打掉"
        );
        assert!(
            reg.resolve("audit:audit-skill").is_some(),
            "仍有 scope 启用 → SKILL MUST 保持可解析（🔴1）"
        );
        assert_eq!(
            enabled_plugin_raw_value("audit@acme", "user", None, Some(&env)),
            None,
            "失败 scope 原 absent → 保持 absent"
        );
    }

    /// 🟡10：多 scope 同根 + 同配置的 bundled server 去重（不误判身份坍缩）。
    #[tokio::test]
    async fn enable_multi_scope_same_root_server_deduped_274() {
        let tmp = TempDir::new().unwrap();
        let (home, env, _src) = setup_installed_catalog(&tmp, true).await;
        let env = cfg_env(&tmp, env);
        let mut reg = SkillRegistry::new();
        let rec = install_plugin(
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
        let root_s = rec.install_path.unwrap();
        // 第二条记录指向同一根（多 scope 常态）。
        {
            let root_s = root_s.clone();
            store::update_installed_plugins(
                move |data| {
                    let mut extra = Map::new();
                    extra.insert("scope".to_string(), Value::String("project".to_string()));
                    data.account.plugins.get_mut("audit@acme").unwrap().push(
                        InstalledPluginRecord {
                            install_path: Some(root_s),
                            mcp_servers: Vec::new(),
                            extra,
                        },
                    );
                },
                Some(&home),
                Some(&env),
            )
            .unwrap();
        }

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
        .expect("同根+同配置去重后应成功");
        assert_eq!(
            *hooks.registered.lock().unwrap(),
            vec!["audit-mcp".to_string()],
            "同一 server 只挂一次"
        );
        assert_eq!(enabled_flag(&env, "audit@acme"), Some(true));
    }

    /// 🟡5/🟡10：跨记录同 bundle_id 但**异根/异配置** → 仍按身份坍缩硬错（不静默首胜）。
    #[tokio::test]
    async fn enable_multi_scope_conflicting_server_config_errors_274() {
        let tmp = TempDir::new().unwrap();
        let (home, env, _src) = setup_installed_catalog(&tmp, true).await;
        let env = cfg_env(&tmp, env);
        let mut reg = SkillRegistry::new();
        let rec = install_plugin(
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
        let root = PathBuf::from(rec.install_path.unwrap());
        let alt = tmp.path().join("alt-root");
        fs::create_dir_all(alt.join("skills/code-review")).unwrap();
        fs::copy(
            root.join("skills/code-review/SKILL.md"),
            alt.join("skills/code-review/SKILL.md"),
        )
        .unwrap();
        fs::create_dir_all(alt.join("mcp-servers")).unwrap();
        fs::write(
            alt.join("mcp-servers/audit-mcp.json"),
            r#"{"type":"stdio","name":"audit-mcp","server_parameters":{"command":"node-alt"}}"#,
        )
        .unwrap();
        {
            let alt_s = alt.to_string_lossy().into_owned();
            store::update_installed_plugins(
                move |data| {
                    let mut extra = Map::new();
                    extra.insert("scope".to_string(), Value::String("project".to_string()));
                    data.account.plugins.get_mut("audit@acme").unwrap().push(
                        InstalledPluginRecord {
                            install_path: Some(alt_s),
                            mcp_servers: vec![BundleId::try_from("audit-mcp".to_string()).unwrap()],
                            extra,
                        },
                    );
                },
                Some(&home),
                Some(&env),
            )
            .unwrap();
        }

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
        assert!(
            matches!(err, PluginInstallError::Conflict(_)),
            "异根/异配置同 bundle_id 应硬错：{err:?}"
        );
        assert_ne!(enabled_flag(&env, "audit@acme"), Some(true));
    }

    /// 🟡11：entry 级 `skills` 声明存在、实际零投影 → 失败（声明态判据覆盖 entry 来源）。
    #[tokio::test]
    async fn enable_entry_declared_skills_missing_fails_274() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join(".tfrobot-plugin")).unwrap();
        fs::write(
            repo.join(".tfrobot-plugin/marketplace.json"),
            r#"{"plugins":[{"name":"audit","source":"./plugin","skills":["./custom-skills"]}]}"#,
        )
        .unwrap();
        fs::create_dir_all(repo.join("plugin")).unwrap();
        fs::write(repo.join("plugin/README.md"), "no conventional skills").unwrap();
        git(&["init", "-q"], &repo);
        git(&["add", "-A"], &repo);
        git(&["commit", "-qm", "init"], &repo);
        let source =
            serde_json::json!({"type": "git", "url": format!("file://{}", repo.display())});
        let (home, env) = setup_home_with_catalog(&tmp, source).await;
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
        assert!(matches!(err, PluginInstallError::ContentUnavailable { .. }));
        assert!(
            err.to_string().contains("no_projectable_content"),
            "entry 声明 skills 而零投影：{err}"
        );
        assert_ne!(enabled_flag(&env, "audit@acme"), Some(true));
    }

    #[tokio::test]
    async fn enable_declared_server_files_deleted_fails_274() {
        let tmp = TempDir::new().unwrap();
        let (home, env, _src) = setup_installed_catalog(&tmp, true).await;
        let env = cfg_env(&tmp, env);
        let mut reg = SkillRegistry::new();
        let rec = install_plugin(
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
        let root = PathBuf::from(rec.install_path.unwrap());
        fs::remove_dir_all(root.join("mcp-servers")).unwrap();

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
        assert!(matches!(err, PluginInstallError::ContentUnavailable { .. }));
        assert!(
            err.to_string().contains("declared_server_missing"),
            "声明 server 不可投影：{err}"
        );
        assert_ne!(enabled_flag(&env, "audit@acme"), Some(true));
    }

    #[tokio::test]
    async fn enable_staging_zero_registration_rolls_back_274() {
        let tmp = TempDir::new().unwrap();
        let (home, env, _src) = setup_installed_catalog(&tmp, false).await;
        let env = cfg_env(&tmp, env);
        let mut reg = SkillRegistry::new();
        let rec = install_plugin(
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
        let root = PathBuf::from(rec.install_path.unwrap());
        // 文件在（预检 expect_skills），但缺 frontmatter description → staging 零注册。
        fs::write(
            root.join("skills/code-review/SKILL.md"),
            "---\nname: code-review\n---\nbody",
        )
        .unwrap();

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
        assert!(matches!(err, PluginInstallError::ContentUnavailable { .. }));
        assert!(
            err.to_string().contains("skills_not_staged"),
            "期望有 SKILL 却零注册：{err}"
        );
        assert_eq!(
            enabled_flag(&env, "audit@acme"),
            None,
            "原 absent 精确还原为 absent（🟡2，不升级成显式 false）"
        );
        assert!(reg.resolve("audit:code-review").is_none());
    }

    #[tokio::test]
    async fn enable_marketplace_unregistered_rolls_back_274() {
        let tmp = TempDir::new().unwrap();
        let (home, env, _src) = setup_installed_catalog(&tmp, false).await;
        let env = cfg_env(&tmp, env);
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
        update_known_marketplaces(
            |f| {
                f.account.marketplaces.shift_remove("acme");
            },
            Some(&home),
            Some(&env),
        )
        .unwrap();

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
        assert_eq!(
            enabled_flag(&env, "audit@acme"),
            None,
            "写 true 后 resolve 失败须恢复原值 absent（🟡2）"
        );
    }

    #[tokio::test]
    async fn enable_all_records_without_install_path_fails_274() {
        let tmp = TempDir::new().unwrap();
        let (home, env, _src) = setup_installed_catalog(&tmp, false).await;
        let env = cfg_env(&tmp, env);
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
        store::update_installed_plugins(
            |data| {
                for r in data.account.plugins.get_mut("audit@acme").unwrap() {
                    r.install_path = None;
                }
            },
            Some(&home),
            Some(&env),
        )
        .unwrap();

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
        assert!(matches!(err, PluginInstallError::ContentUnavailable { .. }));
        assert_ne!(enabled_flag(&env, "audit@acme"), Some(true));
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
                        mcp_servers: Vec::new(),
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

    #[test]
    fn project_enable_noop_preserves_read_roots_253() {
        // A+：project scope 写入保留 readRoots（不主动删除）；同值写 → 完全 no-op（不扰盘）。
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        let path = workdir_project_settings_path(&wd);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{
                "readRoots": ["/data"],
                "landingRoot": "/landing",
                "enabledMcpjsonServers": ["srv"],
                "permissions": "invalid-not-object",
                "keepUnknown": {"k": 1},
                "enabledPlugins": {"a@mp": true}
            }"#,
        )
        .unwrap();
        let before = fs::read(&path).unwrap();
        let before_mtime = fs::metadata(&path).unwrap().modified().unwrap();
        let changed = write_enabled_plugin("a@mp", true, "project", wd.to_str(), None).unwrap();
        assert!(!changed, "同值写 → enabledPlugins 无变化");
        assert_eq!(fs::read(&path).unwrap(), before, "no-op 不得重写");
        assert_eq!(
            fs::metadata(&path).unwrap().modified().unwrap(),
            before_mtime
        );
        let saved: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            saved["readRoots"],
            serde_json::json!(["/data"]),
            "A+：readRoots 必须保留"
        );
        // 其他字段（含被校验过滤的 project 字段）原值保留。
        assert_eq!(saved["landingRoot"], serde_json::json!("/landing"));
        assert_eq!(saved["enabledMcpjsonServers"], serde_json::json!(["srv"]));
        assert_eq!(
            saved["permissions"],
            serde_json::json!("invalid-not-object")
        );
        assert_eq!(saved["keepUnknown"], serde_json::json!({"k": 1}));
        assert_eq!(saved["enabledPlugins"]["a@mp"], serde_json::json!(true));
        // 幂等：再次 enable 无改动。
        assert!(!write_enabled_plugin("a@mp", true, "project", wd.to_str(), None).unwrap());
    }

    #[test]
    fn project_enable_real_write_preserves_other_filtered_fields() {
        // #245：真写盘（enabledPlugins 缺目标键）同样必须 raw 保真，不得删除其他被过滤字段。
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        let path = workdir_project_settings_path(&wd);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{
                "readRoots": ["/data"],
                "landingRoot": "/landing",
                "enabledMcpjsonServers": ["srv"],
                "permissions": "invalid-not-object",
                "enabledPlugins": {"other@mp": true}
            }"#,
        )
        .unwrap();
        let changed = write_enabled_plugin("new@mp", true, "project", wd.to_str(), None).unwrap();
        assert!(changed, "enabledPlugins 缺目标键 → 真写盘");
        let saved: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            saved["readRoots"],
            serde_json::json!(["/data"]),
            "A+：readRoots 必须保留"
        );
        assert_eq!(saved["landingRoot"], serde_json::json!("/landing"));
        assert_eq!(saved["enabledMcpjsonServers"], serde_json::json!(["srv"]));
        assert_eq!(
            saved["permissions"],
            serde_json::json!("invalid-not-object")
        );
        assert_eq!(saved["enabledPlugins"]["other@mp"], serde_json::json!(true));
        assert_eq!(saved["enabledPlugins"]["new@mp"], serde_json::json!(true));
    }

    #[test]
    fn project_clear_enabled_plugin_real_write_preserves_filtered_fields() {
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        let path = workdir_project_settings_path(&wd);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{"readRoots":["/data"],"landingRoot":"/landing","enabledPlugins":{"a@mp":true}}"#,
        )
        .unwrap();
        clear_enabled_plugin("a@mp", "project", wd.to_str(), None).unwrap();
        let saved: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            saved["readRoots"],
            serde_json::json!(["/data"]),
            "A+：readRoots 必须保留"
        );
        assert_eq!(saved["landingRoot"], serde_json::json!("/landing"));
        assert!(saved["enabledPlugins"].get("a@mp").is_none(), "{saved}");
    }

    #[test]
    fn project_set_enabled_if_absent_real_write_preserves_filtered_fields() {
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        let path = workdir_project_settings_path(&wd);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{"readRoots":["/data"],"landingRoot":"/landing","enabledPlugins":{}}"#,
        )
        .unwrap();
        set_enabled_true_if_absent("a@mp", "project", wd.to_str(), None).unwrap();
        let saved: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            saved["readRoots"],
            serde_json::json!(["/data"]),
            "A+：readRoots 必须保留"
        );
        assert_eq!(saved["landingRoot"], serde_json::json!("/landing"));
        assert_eq!(saved["enabledPlugins"]["a@mp"], serde_json::json!(true));
    }

    // ---- #253：corrupt / 目标容器畸形 fail-closed；非法兄弟条目保留 -----------------
    fn xdg_only(tmp: &TempDir) -> EnvMap {
        std::iter::once((
            "XDG_CONFIG_HOME".to_string(),
            tmp.path().join("cfg").to_string_lossy().into_owned(),
        ))
        .collect()
    }

    fn settings_mtime(path: &Path) -> std::time::SystemTime {
        fs::metadata(path).unwrap().modified().unwrap()
    }

    #[test]
    fn enable_plugin_refuses_corrupt_settings_and_keeps_bytes_253() {
        let tmp = TempDir::new().unwrap();
        let env = xdg_only(&tmp);
        let path = user_settings_path(Some(&env));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{not json").unwrap();
        let before = fs::read(&path).unwrap();
        assert!(write_enabled_plugin("a@mp", true, "user", None, Some(&env)).is_err());
        assert_eq!(
            fs::read(&path).unwrap(),
            before,
            "corrupt 文件必须逐字节不变"
        );
    }

    #[test]
    fn enable_plugin_refuses_non_object_enabled_plugins_253() {
        let tmp = TempDir::new().unwrap();
        let env = xdg_only(&tmp);
        let path = user_settings_path(Some(&env));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, r#"{"enabledPlugins":"oops","keepUnknown":1}"#).unwrap();
        let before = fs::read(&path).unwrap();
        assert!(write_enabled_plugin("a@mp", true, "user", None, Some(&env)).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn enable_plugin_preserves_invalid_sibling_entries_253() {
        let tmp = TempDir::new().unwrap();
        let env = xdg_only(&tmp);
        let path = user_settings_path(Some(&env));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{"enabledPlugins":{"bad key":42,"other@mp":true},"keepUnknown":{"k":1}}"#,
        )
        .unwrap();
        assert!(write_enabled_plugin("a@mp", true, "user", None, Some(&env)).unwrap());
        let saved: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved["enabledPlugins"]["bad key"], serde_json::json!(42));
        assert_eq!(saved["enabledPlugins"]["other@mp"], serde_json::json!(true));
        assert_eq!(saved["enabledPlugins"]["a@mp"], serde_json::json!(true));
        assert_eq!(saved["keepUnknown"], serde_json::json!({"k": 1}));
    }

    #[test]
    fn clear_and_set_if_absent_refuse_non_object_enabled_plugins_253() {
        let tmp = TempDir::new().unwrap();
        let env = xdg_only(&tmp);
        let path = user_settings_path(Some(&env));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, r#"{"enabledPlugins":[]}"#).unwrap();
        let before = fs::read(&path).unwrap();
        assert!(clear_enabled_plugin("a@mp", "user", None, Some(&env)).is_err());
        assert!(set_enabled_true_if_absent("a@mp", "user", None, Some(&env)).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn clear_enabled_plugin_keeps_empty_object_and_preserves_siblings_253() {
        let tmp = TempDir::new().unwrap();
        let env = xdg_only(&tmp);
        let path = user_settings_path(Some(&env));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{"enabledPlugins":{"a@mp":true,"b@mp":false},"keep":1}"#,
        )
        .unwrap();
        clear_enabled_plugin("a@mp", "user", None, Some(&env)).unwrap();
        let saved: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(saved["enabledPlugins"].get("a@mp").is_none());
        assert_eq!(saved["enabledPlugins"]["b@mp"], serde_json::json!(false));
        clear_enabled_plugin("b@mp", "user", None, Some(&env)).unwrap();
        let saved: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved["enabledPlugins"], serde_json::json!({}));
        assert_eq!(saved["keep"], serde_json::json!(1));
    }

    #[test]
    fn enable_plugin_noop_does_not_touch_mtime_253() {
        let tmp = TempDir::new().unwrap();
        let env = xdg_only(&tmp);
        let path = user_settings_path(Some(&env));
        assert!(write_enabled_plugin("a@mp", true, "user", None, Some(&env)).unwrap());
        let before_mtime = settings_mtime(&path);
        let before_bytes = fs::read(&path).unwrap();
        assert!(!write_enabled_plugin("a@mp", true, "user", None, Some(&env)).unwrap());
        assert_eq!(settings_mtime(&path), before_mtime);
        assert_eq!(fs::read(&path).unwrap(), before_bytes);
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
            &HashSet::new(),
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
            &HashSet::new(),
            None,
        )
        .await
        .unwrap());
    }

    // ---- #275：跨 marketplace 同名 plugin 唯一性 --------------------------

    /// 构造含指定 plugin 的 marketplace 源仓库（plugin 名可变，供跨 marketplace 同名用）。
    fn build_plugin_repo(repo: &Path, plugin: &str, description: &str) -> Value {
        fs::create_dir_all(repo.join(".tfrobot-plugin")).unwrap();
        fs::write(
            repo.join(".tfrobot-plugin/marketplace.json"),
            format!(r#"{{"plugins": [{{"name": "{plugin}", "source": "./plugins/{plugin}"}}]}}"#),
        )
        .unwrap();
        let skill = repo.join(format!("plugins/{plugin}/skills/code-review"));
        fs::create_dir_all(&skill).unwrap();
        fs::write(
            skill.join("SKILL.md"),
            format!("---\nname: code-review\ndescription: {description}\n---\nbody"),
        )
        .unwrap();
        git(&["init", "-q"], repo);
        git(&["add", "-A"], repo);
        git(&["commit", "-qm", "init"], repo);
        serde_json::json!({"type": "git", "url": format!("file://{}", repo.display())})
    }

    /// 一个 marketplace 含多个 plugin（R1：同源其它已装 plugin 不得当保护伞）。
    fn build_multi_plugin_repo(repo: &Path, plugins: &[(&str, &str)]) -> Value {
        fs::create_dir_all(repo.join(".tfrobot-plugin")).unwrap();
        let entries: Vec<String> = plugins
            .iter()
            .map(|(plugin, _)| format!(r#"{{"name": "{plugin}", "source": "./plugins/{plugin}"}}"#))
            .collect();
        fs::write(
            repo.join(".tfrobot-plugin/marketplace.json"),
            format!(r#"{{"plugins": [{}]}}"#, entries.join(",")),
        )
        .unwrap();
        for (plugin, description) in plugins {
            let skill = repo.join(format!("plugins/{plugin}/skills/code-review"));
            fs::create_dir_all(&skill).unwrap();
            fs::write(
                skill.join("SKILL.md"),
                format!("---\nname: code-review\ndescription: {description}\n---\nbody"),
            )
            .unwrap();
        }
        git(&["init", "-q"], repo);
        git(&["add", "-A"], repo);
        git(&["commit", "-qm", "init"], repo);
        serde_json::json!({"type": "git", "url": format!("file://{}", repo.display())})
    }

    /// 预 clone marketplace catalog + 写 known_marketplaces.json（install/enable 前置）。
    async fn seed_marketplace(home: &Path, env: &EnvMap, name: &str, source: &Value) {
        let mut throwaway = SkillRegistry::new();
        stage_marketplace_skills(
            name,
            source,
            &mut throwaway,
            home,
            MarketplaceStageOptions {
                refresh: true,
                ..Default::default()
            },
        )
        .await;
        let src = source.clone();
        let mp = name.to_string();
        update_known_marketplaces(
            move |file| {
                file.account.marketplaces.insert(
                    mp,
                    crate::settings::reconciler::KnownMarketplaceEntry {
                        source: src,
                        extra: Map::new(),
                    },
                );
            },
            Some(home),
            Some(env),
        )
        .unwrap();
    }

    /// 两个 marketplace（mp-a / mp-b，各含同名 plugin `alpha`）→ (home, env, src_a, src_b)。
    async fn setup_cross_marketplace(tmp: &TempDir) -> (PathBuf, EnvMap, Value, Value) {
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let cfg_home = tmp.path().join("cfg");
        fs::create_dir_all(&cfg_home).unwrap();
        let mut env = EnvMap::new();
        env.insert(
            "XDG_CONFIG_HOME".to_string(),
            cfg_home.to_string_lossy().into_owned(),
        );
        let src_a = build_plugin_repo(&tmp.path().join("mp-a"), "alpha", "from mp-a");
        let src_b = build_plugin_repo(&tmp.path().join("mp-b"), "alpha", "from mp-b");
        seed_marketplace(&home, &env, "mp-a", &src_a).await;
        seed_marketplace(&home, &env, "mp-b", &src_b).await;
        (home, env, src_a, src_b)
    }

    fn seed_intent(home: &Path, env: &EnvMap, pids: &[&str]) {
        let pids: Vec<String> = pids.iter().map(|s| s.to_string()).collect();
        crate::settings::store::update_installed_plugins_intent(
            move |file| {
                for pid in pids {
                    file.account.installed_plugins.insert(pid);
                }
            },
            Some(home),
            Some(env),
        )
        .unwrap();
    }

    fn seed_ledger_record_with_path(home: &Path, env: &EnvMap, pid: &str, install_path: &Path) {
        let pid = pid.to_string();
        let install_path = install_path.to_string_lossy().into_owned();
        crate::settings::store::update_installed_plugins(
            move |file| {
                file.account.plugins.insert(
                    pid,
                    vec![InstalledPluginRecord {
                        install_path: Some(install_path),
                        mcp_servers: Vec::new(),
                        extra: Map::new(),
                    }],
                );
            },
            Some(home),
            Some(env),
        )
        .unwrap();
    }

    fn plugin_root(home: &Path, marketplace: &str, plugin: &str) -> PathBuf {
        marketplace_skill_dir(home, marketplace, &[])
            .join("plugins")
            .join(plugin)
    }

    fn plugin_name_conflict(err: &PluginInstallError) -> &PluginNameConflictError {
        match err {
            PluginInstallError::NameConflict(e) => e,
            other => panic!("expected NameConflict, got {other:?}"),
        }
    }

    /// 序 1（conformance-tests.md:365 原复现序）：已装 `alpha@mp-a` 后 install `alpha@mp-b` →
    /// 安装层拒绝，零变更（不 clone mp-b、不写 intent/ledger/settings、registry 不动）。
    #[tokio::test]
    async fn install_rejects_duplicate_bare_plugin_before_materialize_zero_change() {
        let tmp = TempDir::new().unwrap();
        let (home, env, _src_a, _src_b) = setup_cross_marketplace(&tmp).await;
        let mut reg = SkillRegistry::new();
        install_plugin(
            "alpha@mp-a",
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
        enable_plugin(
            "alpha@mp-a",
            &mut reg,
            &home,
            EnableOptions {
                env: Some(&env),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();

        // 删掉 mp-b 预 clone：若前置门在 materialize 之后，install 会尝试重新 clone 并留下目录。
        let mp_b_dir = marketplace_skill_dir(&home, "mp-b", &[]);
        fs::remove_dir_all(&mp_b_dir).unwrap();

        let intent_path =
            crate::settings::store::installed_plugins_intent_path(Some(&home), Some(&env));
        let ledger_path = installed_plugins_path(Some(&home), Some(&env));
        let settings_path = user_settings_path(Some(&env));
        let intent_before = fs::read(&intent_path).unwrap();
        let ledger_before = fs::read(&ledger_path).unwrap();
        let settings_before = fs::read(&settings_path).unwrap();
        let refs_before = reg.all_refs();

        let err = install_plugin(
            "alpha@mp-b",
            &mut reg,
            &home,
            InstallOptions {
                env: Some(&env),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap_err();
        let conflict = plugin_name_conflict(&err);
        assert_eq!(conflict.plugin, "alpha");
        assert!(matches!(
            &conflict.kind,
            PluginNameConflictKind::Installed { installed_pid } if installed_pid == "alpha@mp-a"
        ));

        // 零变更：不 clone、不写三处持久化、registry 不动。
        assert!(!mp_b_dir.exists(), "前置门必须在 materialize/clone 之前");
        assert_eq!(fs::read(&intent_path).unwrap(), intent_before);
        assert_eq!(fs::read(&ledger_path).unwrap(), ledger_before);
        assert_eq!(fs::read(&settings_path).unwrap(), settings_before);
        assert_eq!(reg.all_refs(), refs_before);
        assert_eq!(
            reg.resolve("alpha:code-review").unwrap().source,
            "marketplace:mp-a"
        );
    }

    /// 同 pid 重装不属于跨 marketplace 冲突，仍幂等允许。
    #[tokio::test]
    async fn reinstall_same_pid_is_not_rejected() {
        let tmp = TempDir::new().unwrap();
        let (home, env, _src_a, _src_b) = setup_cross_marketplace(&tmp).await;
        let mut reg = SkillRegistry::new();
        install_plugin(
            "alpha@mp-a",
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
        install_plugin(
            "alpha@mp-a",
            &mut reg,
            &home,
            InstallOptions {
                env: Some(&env),
                ..Default::default()
            },
            None,
        )
        .await
        .expect("同 pid 重装不得按跨 marketplace 冲突拒绝");
        let intent = crate::settings::store::load_installed_plugins_intent(Some(&home), Some(&env))
            .account
            .installed_plugins;
        assert_eq!(intent.iter().filter(|p| *p == "alpha@mp-a").count(), 1);
        assert_eq!(ledger_records(&home, &env, "alpha@mp-a").len(), 1);
    }

    /// 序 4（存量双装）：intent 中两个同名 pid 并存 → enable 任一都 fail-closed，指引先 uninstall。
    #[tokio::test]
    async fn enable_rejects_legacy_duplicate_bare_plugin() {
        let tmp = TempDir::new().unwrap();
        let (home, env, _src_a, _src_b) = setup_cross_marketplace(&tmp).await;
        seed_intent(&home, &env, &["alpha@mp-a", "alpha@mp-b"]);
        seed_ledger_record_with_path(
            &home,
            &env,
            "alpha@mp-a",
            &plugin_root(&home, "mp-a", "alpha"),
        );
        seed_ledger_record_with_path(
            &home,
            &env,
            "alpha@mp-b",
            &plugin_root(&home, "mp-b", "alpha"),
        );

        let mut reg = SkillRegistry::new();
        let err = enable_plugin(
            "alpha@mp-b",
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
        let conflict = plugin_name_conflict(&err);
        assert!(matches!(
            &conflict.kind,
            PluginNameConflictKind::Installed { installed_pid } if installed_pid == "alpha@mp-a"
        ));
        // 写 enabledPlugins 之前失败：零变更。
        assert_eq!(enabled_flag(&env, "alpha@mp-b"), None);
        assert_eq!(enabled_flag(&env, "alpha@mp-a"), None);
    }

    fn user_skill(name: &str) -> smcp::A2CSkillRef {
        smcp::A2CSkillRef {
            name: name.to_string(),
            source: "user".to_string(),
            uri: None,
            path: format!("/abs/skills/{name}"),
            description: "d".to_string(),
            license: None,
            compatibility: None,
            allowed_tools: None,
            tags: None,
            version: None,
            skill_metadata: None,
        }
    }

    /// add-only 的 marketplace 源不应挡住已安装来源（installed intent 优先）：install mp-b 时清掉
    /// 未安装 mp-a 的占名，mp-b 成为 owner。
    #[tokio::test]
    async fn install_overrides_add_only_marketplace_owner() {
        let tmp = TempDir::new().unwrap();
        let (home, env, src_a, _src_b) = setup_cross_marketplace(&tmp).await;
        let mut reg = SkillRegistry::new();
        stage_marketplace_skills(
            "mp-a",
            &src_a,
            &mut reg,
            &home,
            MarketplaceStageOptions::default(),
        )
        .await;
        assert_eq!(
            reg.resolve("alpha:code-review").unwrap().source,
            "marketplace:mp-a"
        );

        install_plugin(
            "alpha@mp-b",
            &mut reg,
            &home,
            InstallOptions {
                env: Some(&env),
                ..Default::default()
            },
            None,
        )
        .await
        .expect("已安装来源应胜过仅 add 的 marketplace 占名");
        let (kept, orphaned) = reg
            .all_refs()
            .into_iter()
            .find(|(r, _)| r.name == "alpha:code-review")
            .unwrap();
        assert_eq!(kept.source, "marketplace:mp-b");
        assert!(orphaned);
        let intent = crate::settings::store::load_installed_plugins_intent(Some(&home), Some(&env))
            .account
            .installed_plugins;
        assert!(intent.contains("alpha@mp-b"));
    }

    /// 非 marketplace 源（user）占名仍 fail-closed：清掉 add-only 源不等于放弃全局唯一性。
    #[tokio::test]
    async fn install_fails_closed_on_non_marketplace_skill_conflict() {
        let tmp = TempDir::new().unwrap();
        let (home, env, _src_a, _src_b) = setup_cross_marketplace(&tmp).await;
        let mut reg = SkillRegistry::new();
        assert!(reg.register(user_skill("alpha:code-review")));

        let err = install_plugin(
            "alpha@mp-b",
            &mut reg,
            &home,
            InstallOptions {
                env: Some(&env),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap_err();
        let conflict = plugin_name_conflict(&err);
        assert!(matches!(
            &conflict.kind,
            PluginNameConflictKind::SkillSource { .. }
        ));
        assert!(
            !crate::settings::store::installed_plugins_intent_path(Some(&home), Some(&env))
                .exists(),
            "fail-closed 不写安装意图"
        );
        assert_eq!(reg.resolve("alpha:code-review").unwrap().source, "user");
    }

    /// add-only 的 marketplace 源不应挡住已安装来源：enable mp-b 时清掉未安装 mp-a 的占名。
    #[tokio::test]
    async fn enable_overrides_add_only_marketplace_owner() {
        let tmp = TempDir::new().unwrap();
        let (home, env, src_a, _src_b) = setup_cross_marketplace(&tmp).await;
        seed_intent(&home, &env, &["alpha@mp-b"]);
        seed_ledger_record_with_path(
            &home,
            &env,
            "alpha@mp-b",
            &plugin_root(&home, "mp-b", "alpha"),
        );
        let mut reg = SkillRegistry::new();
        stage_marketplace_skills(
            "mp-a",
            &src_a,
            &mut reg,
            &home,
            MarketplaceStageOptions::default(),
        )
        .await;

        enable_plugin(
            "alpha@mp-b",
            &mut reg,
            &home,
            EnableOptions {
                env: Some(&env),
                ..Default::default()
            },
            None,
        )
        .await
        .expect("已安装来源应胜过仅 add 的 marketplace 占名");
        assert_eq!(enabled_flag(&env, "alpha@mp-b"), Some(true));
        assert_eq!(
            reg.resolve("alpha:code-review").unwrap().source,
            "marketplace:mp-b"
        );
    }

    async fn setup_mixed_marketplaces(tmp: &TempDir) -> (PathBuf, EnvMap, Value) {
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let cfg = tmp.path().join("cfg");
        fs::create_dir_all(&cfg).unwrap();
        let mut env = EnvMap::new();
        env.insert(
            "XDG_CONFIG_HOME".to_string(),
            cfg.to_string_lossy().into_owned(),
        );
        let src_a = build_multi_plugin_repo(
            &tmp.path().join("mp-a"),
            &[("alpha", "from mp-a"), ("beta", "beta mp-a")],
        );
        let src_b = build_plugin_repo(&tmp.path().join("mp-b"), "alpha", "from mp-b");
        seed_marketplace(&home, &env, "mp-a", &src_a).await;
        seed_marketplace(&home, &env, "mp-b", &src_b).await;
        (home, env, src_a)
    }

    /// R1（install）：mp-a 装了 **beta** 但 alpha 未装 → mp-a 的 alpha 不得挡住 mp-b 的 alpha。
    #[tokio::test]
    async fn install_alpha_ignores_same_marketplace_installed_beta() {
        let tmp = TempDir::new().unwrap();
        let (home, env, src_a) = setup_mixed_marketplaces(&tmp).await;
        let mut reg = SkillRegistry::new();
        stage_marketplace_skills(
            "mp-a",
            &src_a,
            &mut reg,
            &home,
            MarketplaceStageOptions::default(),
        )
        .await;
        install_plugin(
            "beta@mp-a",
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

        install_plugin(
            "alpha@mp-b",
            &mut reg,
            &home,
            InstallOptions {
                env: Some(&env),
                ..Default::default()
            },
            None,
        )
        .await
        .expect("R1：同源其它已装 plugin 不得挡住 alpha@mp-b");
        let (kept, _) = reg
            .all_refs()
            .into_iter()
            .find(|(r, _)| r.name == "alpha:code-review")
            .unwrap();
        assert_eq!(kept.source, "marketplace:mp-b");
        let intent = crate::settings::store::load_installed_plugins_intent(Some(&home), Some(&env))
            .account
            .installed_plugins;
        assert!(intent.contains("beta@mp-a") && intent.contains("alpha@mp-b"));
    }

    /// R1（enable）：mp-a 装了 **beta** 但 alpha 未装 → enable mp-b 的 alpha 不得被 mp-a 占名挡住。
    #[tokio::test]
    async fn enable_alpha_ignores_same_marketplace_installed_beta() {
        let tmp = TempDir::new().unwrap();
        let (home, env, src_a) = setup_mixed_marketplaces(&tmp).await;
        seed_intent(&home, &env, &["beta@mp-a", "alpha@mp-b"]);
        seed_ledger_record_with_path(
            &home,
            &env,
            "alpha@mp-b",
            &plugin_root(&home, "mp-b", "alpha"),
        );
        let mut reg = SkillRegistry::new();
        stage_marketplace_skills(
            "mp-a",
            &src_a,
            &mut reg,
            &home,
            MarketplaceStageOptions::default(),
        )
        .await;

        enable_plugin(
            "alpha@mp-b",
            &mut reg,
            &home,
            EnableOptions {
                env: Some(&env),
                ..Default::default()
            },
            None,
        )
        .await
        .expect("R1：同源其它已装 plugin 不得挡住 enable alpha@mp-b");
        assert_eq!(enabled_flag(&env, "alpha@mp-b"), Some(true));
        assert_eq!(
            reg.resolve("alpha:code-review").unwrap().source,
            "marketplace:mp-b"
        );
    }

    /// 非 marketplace 源（user）占名仍 fail-closed：enable 恢复 enabledPlugins 原值（absent → 删键）+
    /// registry 快照。
    #[tokio::test]
    async fn enable_rolls_back_enabled_flag_on_non_marketplace_skill_conflict() {
        let tmp = TempDir::new().unwrap();
        let (home, env, _src_a, _src_b) = setup_cross_marketplace(&tmp).await;
        seed_intent(&home, &env, &["alpha@mp-b"]);
        seed_ledger_record_with_path(
            &home,
            &env,
            "alpha@mp-b",
            &plugin_root(&home, "mp-b", "alpha"),
        );
        let mut reg = SkillRegistry::new();
        assert!(reg.register(user_skill("alpha:code-review")));

        let err = enable_plugin(
            "alpha@mp-b",
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
        let conflict = plugin_name_conflict(&err);
        assert!(matches!(
            &conflict.kind,
            PluginNameConflictKind::SkillSource { .. }
        ));
        assert_eq!(
            enabled_flag(&env, "alpha@mp-b"),
            None,
            "原 absent 精确还原为 absent（#274 🟡2）"
        );
        assert_eq!(reg.resolve("alpha:code-review").unwrap().source, "user");
    }

    /// uninstall 须注销 orphan（installed_disabled）条目，让「按指引卸载后可换源」在本进程闭环。
    #[tokio::test]
    async fn uninstall_removes_orphan_skill_entries() {
        let tmp = TempDir::new().unwrap();
        let (home, env, _src_a, _src_b) = setup_cross_marketplace(&tmp).await;
        let mut reg = SkillRegistry::new();
        install_plugin(
            "alpha@mp-b",
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
        assert!(reg.is_orphan("alpha:code-review"));

        uninstall_plugin(
            "alpha@mp-b",
            &mut reg,
            &home,
            UninstallOptions {
                env: Some(&env),
                ..Default::default()
            },
            &HashSet::new(),
            None,
        )
        .await
        .unwrap();
        assert!(reg.source_conflicts().is_empty());
        assert!(
            !reg.contains("alpha:code-review"),
            "uninstall 应注销 active + orphan 条目"
        );
    }

    /// 复审 🔴 回归：dangling-intent 早退路径也必须清 registry（含 orphan），否则「按 uninstall
    /// 指引换源」在本进程内仍被跨源守卫挡住。
    #[tokio::test]
    async fn uninstall_dangling_intent_clears_orphan_skills_and_allows_source_switch() {
        let tmp = TempDir::new().unwrap();
        let (home, env, src_a, _src_b) = setup_cross_marketplace(&tmp).await;
        let mut reg = SkillRegistry::new();
        // 模拟 install alpha@mp-a 写 intent 成功、写 ledger 失败：skills 已 stage 并 mark_orphan。
        stage_marketplace_skills(
            "mp-a",
            &src_a,
            &mut reg,
            &home,
            MarketplaceStageOptions::default(),
        )
        .await;
        assert!(reg.mark_orphan("alpha:code-review"));
        seed_intent(&home, &env, &["alpha@mp-a"]);

        assert!(uninstall_plugin(
            "alpha@mp-a",
            &mut reg,
            &home,
            UninstallOptions {
                env: Some(&env),
                ..Default::default()
            },
            &HashSet::new(),
            None,
        )
        .await
        .unwrap());
        assert!(
            !reg.contains("alpha:code-review"),
            "dangling uninstall 必须注销 active + orphan 条目"
        );

        // 同进程换源：install alpha@mp-b 成功（不再被 mp-a orphan 占名）。
        install_plugin(
            "alpha@mp-b",
            &mut reg,
            &home,
            InstallOptions {
                env: Some(&env),
                ..Default::default()
            },
            None,
        )
        .await
        .expect("uninstall 后换源 install 不得再被占名阻塞");
        let (kept, orphaned) = reg
            .all_refs()
            .into_iter()
            .find(|(r, _)| r.name == "alpha:code-review")
            .unwrap();
        assert_eq!(kept.source, "marketplace:mp-b");
        assert!(orphaned, "install 后仍为 installed_disabled（orphan）");
    }
}
