/*!
* 文件名: mod.rs
* 作者: JQQ
* 创建日期: 2026/06/01
* 最后修改日期: 2026/06/01
* 版权: 2023 JQQ. All rights reserved.
* 依赖: serde, serde_json, regex
* 描述: Computer 治理层 settings 模块（意图层 settings.json 结构与字段级容错校验）
*       Computer governance-layer settings module (intent-layer settings.json schema & validation)
*
* ## 写路径约定（#253）
*
* 除显式例外——`config::validate::migrate_config`（有意有损规范化）、`config::crud::save_config`
* （整文档 import/duplicate）与 `settings edit`（外部编辑器直接写文件）——所有 settings 文件写入均经
* `raw_write::update_raw_settings` 做 strict raw RMW：只改目标键、保留其余字段、corrupt 或目标容器
* 畸形 fail-closed（拒绝写盘、原文件逐字节不变）。
*
* **A+（2026-11-03 用户拍板）**：project scope 的 `readRoots` 写入期**保留 + WARN**，读面
* trusted-scope-only 拒绝保持不动；不做任何主动删除。
*
* `config::executor` 的 corrupt root 已 fail-closed（`CorruptTarget`）；其目标容器畸形校验、统一
* helper 复用**以及 project `readRoots` 剔除的 A+ 对齐**留 PR-B（v0.5.1）。
*/

pub mod config;
pub mod installer;
pub mod lifecycle;
pub mod mcp_config;
pub mod policy;
pub(crate) mod raw_write;
pub(crate) mod read_roots;
pub mod reconciler;
pub mod recovery;
pub(crate) mod redaction;
pub mod schema;
pub mod scope;
pub mod store;

pub use installer::{
    disable_plugin, enable_plugin, install_plugin, materialize_plugin_record, uninstall_plugin,
    DisableOptions, EnableOptions, InstallOptions, McpHookError, McpInstallHooks,
    McpServerNameConflictError, PluginInstallError, UninstallOptions,
};

pub use lifecycle::{
    add_marketplace, default_marketplace_name, marketplace_name_taken, normalize_marketplace_url,
    refresh_marketplaces, remove_marketplace, resolve_marketplace_identity, AddMarketplaceParams,
    GovernanceError, MarketplaceAddOutcome, MarketplaceIdentity, MarketplaceRefreshRow,
    MarketplaceRemoveOutcome, RefreshStatus, RemoveMarketplaceParams,
};

pub use mcp_config::{
    approve_all_project_mcp, approve_mcp_server, deny_mcp_server, gate_mcp_servers,
    load_mcp_config_file, managed_mcp_config_path, mcp_server_status, resolve_mcp_config,
    user_mcp_config_path, workdir_mcp_config_path, workdir_mcp_local_config_path,
    McpApprovalStatus, McpConfigError, RawMcpConfigFile, ResolveMcpConfigArgs, ResolvedMcpConfig,
    ResolvedMcpServer, MANAGED_MCP_FILENAME, MCP_CONFIG_FILENAME, MCP_LOCAL_CONFIG_FILENAME,
};

pub use policy::{
    default_policy_sources, load_macos_plist, load_managed_settings, load_remote_policy,
    load_windows_registry, resolve_policy_settings, PolicySource, LINUX_MANAGED_DIR,
    MACOS_MANAGED_DIR, MACOS_PLIST_PATH, MANAGED_SETTINGS_DROPIN_DIRNAME,
    MANAGED_SETTINGS_FILENAME, REMOTE_POLICY_BASE_ENV, WINDOWS_MANAGED_DIR, WINDOWS_POLICY_SUBKEY,
    WINDOWS_POLICY_VALUE_NAME,
};

pub use reconciler::{
    declared_marketplace_names, declared_plugin_ids, gc_plugins, list_orphan_marketplaces,
    list_orphan_plugins, prune_marketplaces, reconcile, InstalledPluginRecord, InstalledPlugins,
    KnownMarketplaceEntry, KnownMarketplaces, McpTeardown, ReconcileOptions, ReconcileReport,
    SkillGovernanceStore,
};

pub use recovery::{
    collect_enabled_bundled_servers, recover_marketplace_skills,
    rematerialize_missing_ledger_records, BundledServerRecord, GovernanceRecoveryReport,
};

pub use schema::{
    is_valid_enabled_plugin_key, is_valid_git_url, is_valid_marketplace_name, validate_settings,
    ComputerSettings, GitSource, MarketplaceEntry, PermissionsBlock, SettingsScope,
    SettingsValidationError, BOOL_FIELDS, FIELD_READ_ROOTS, POLICY_ONLY_FIELDS,
    STRING_ARRAY_FIELDS, TRUSTED_SCOPE_ONLY_FIELDS,
};
pub use scope::{
    apply_write, load_settings_file, merge_layers, merge_read, resolve_settings,
    resolve_user_config_dir, user_settings_path, workdir_local_settings_path,
    workdir_project_settings_path, workdir_settings_dir, EnvMap, ResolveSettingsArgs,
    ResolvedSettings, WriteValue, LOCAL_SETTINGS_FILENAME, PROJECT_SETTINGS_FILENAME,
    TFROBOT_DIRNAME, USER_SETTINGS_FILENAME, XDG_CONFIG_HOME_ENV,
};
// protocol#64：`identity` 与 settings 共用同一 user-config 子目录常量（避免两处字面量漂移）。
pub(crate) use scope::A2C_CONFIG_SUBDIR;
// #142（隔离审查 🟡10）：补 `update_installed_plugins_intent` —— 它与 `update_installed_plugins` 是同族
// **孪生**写入口，此前漏 re-export ⇒ 外部只能绕 `settings::store::` 模块路径取用，两者可见形状不一致。
// 纯 re-export，零行为变更。
pub use store::{
    empty_installed_plugins, empty_known_marketplaces, installed_plugins_path,
    known_marketplaces_path, load_installed_plugins, load_known_marketplaces,
    save_installed_plugins, save_known_marketplaces, update_installed_plugins,
    update_installed_plugins_intent, update_known_marketplaces, FileSkillGovernanceStore,
    InstalledPluginsFile, KnownMarketplacesFile, SettingsStoreError, INSTALLED_PLUGINS_FILENAME,
    KNOWN_MARKETPLACES_FILENAME, MATERIALIZED_VERSION, WRITE_PROTECTION_HEADER,
};
