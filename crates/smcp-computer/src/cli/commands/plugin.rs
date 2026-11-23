/*!
* 文件名: plugin.rs
* 作者: JQQ
* 创建日期: 2026/06/10
* 最后修改日期: 2026/06/10
* 版权: 2023 JQQ. All rights reserved.
* 依赖: serde_json, console
* 描述: `plugin` 命令 handler（install / uninstall / enable / disable / list / info / gc）
*       Plugin command handlers.
*
* 对标 Python `a2c_smcp/computer/cli/commands/plugin.py`：与 marketplace 同范式——handler 取显式资源
* （`registry` / `home` / `env`）+ flags，返回退出码（0 成功 / 1 用户错 / 2 网络错），包裹
* [`install_plugin`] / [`uninstall_plugin`] / [`enable_plugin`](crate::settings::installer::enable_plugin) / [`disable_plugin`] 四动词 +
* [`gc_plugins`]。MCP 注入回调统一由 [`McpInstallHooks`]（Rust 把 Python 的四独立可选回调收进一个 trait）注入：
* REPL 经 [`super::CliMcpHooks`] 从活跃 `Computer` 装配；Typer 非交互（#54）传 `None`（ledger-only，MCP 挂载
* 延到下次 REPL boot 经批准框落地）。
*
* **scope 契约**（installer §4.3）：enable/disable 从 ledger **逐 scope** 读安装 scope 传入，**绝不**默认 `user`，
* 否则写错层、与 live 态背离。多 scope 记录 → 逐 scope 处理。enable 在每条记录的 register 之前先经
* `inject_inputs(installPath)` 把 plugin-scoped inputs 注入池（install_plugin 内部已注入，enable 须 handler 显式补）。
*
* 启动期 MCP 批准框（`run_mcp_approval`）属 REPL/boot 范畴，由 CLI-03（#54）实现。
*/

use std::collections::HashSet;
use std::path::Path;

use serde_json::{json, Map, Value};

use crate::mcp_clients::model::BundleId;

use super::{
    file_store, msg_dim, msg_err, ok_msg, print_json, resolved_settings, Confirm, EXIT_OK,
    EXIT_USER_ERROR,
};
use crate::settings::installer::{
    disable_plugin, enable_plugin_with_reclaim, install_plugin, uninstall_plugin, DisableOptions,
    EnableOptions, EnableReclaimContext, InstallOptions, McpInstallHooks, PluginInstallError,
    UninstallOptions,
};
use crate::settings::reconciler::{gc_plugins, list_orphan_plugins, McpTeardown};
use crate::settings::scope::EnvMap;
use crate::settings::store::{load_installed_plugins, load_installed_plugins_intent};
use crate::settings::InstalledPluginRecord;
use crate::skills::SkillRegistry;

// ── 输出辅助 / output helpers ────────────────────────────────────────────────
/// 对标 Python `_err(..., error_code=...)`：JSON 形态为 `{error, message}` / structured plugin error。
fn plugin_err(msg: &str, json_output: bool, code: i32, error_code: Option<&str>) -> i32 {
    if json_output {
        print_json(&json!({ "error": error_code.unwrap_or("error"), "message": msg }));
    } else {
        msg_err(msg);
    }
    code
}

/// enable 失败的稳定 error_code：先经 [`PluginInstallError::retained_source_deep`] 剥掉
/// `EnableFailedRetained` 诊断包装再分类——包装只补充「既有启用状态已保留」，不改变 error_code。
fn enable_failure_error_code(err: &PluginInstallError) -> Option<&'static str> {
    match err.retained_source_deep() {
        PluginInstallError::Conflict(_) => Some("mcp_server_name_conflict"),
        PluginInstallError::NameConflict(_) => Some("plugin_name_conflict"),
        _ => None,
    }
}

// ── 内部辅助 / internal helpers ──────────────────────────────────────────────
/// `<plugin>@<marketplace>` → `(plugin, marketplace)`；格式非法 → `None`。
fn split_plugin_id(plugin_id: &str) -> Option<(&str, &str)> {
    let (plugin, marketplace) = plugin_id.split_once('@')?;
    (!plugin.is_empty() && !marketplace.is_empty()).then_some((plugin, marketplace))
}

/// 读 `installed_plugins.json` 中某 plugin 的全部 scope 记录 / all install records of a plugin。
fn installed_records(
    home: &Path,
    env: Option<&EnvMap>,
    plugin_id: &str,
) -> Vec<InstalledPluginRecord> {
    load_installed_plugins(Some(home), env)
        .account
        .plugins
        .get(plugin_id)
        .cloned()
        .unwrap_or_default()
}

/// 五层合并视图的 `enabledPlugins` 映射（`id → bool`）/ merged enabledPlugins map。
///
/// v0.3.0：调用方仅把 `== Some(true)` 视为启用（absent / `false` 均未启用）。#98：project/local 锚定 `cwd`（`None` → 进程 cwd）。
fn enabled_plugins_view(cwd: Option<&Path>, env: Option<&EnvMap>) -> Map<String, Value> {
    match resolved_settings(cwd, env, None).get("enabledPlugins") {
        Some(Value::Object(map)) => map.clone(),
        _ => Map::new(),
    }
}

/// JSON 字符串数组 → `"a, b"`（空 → `"—"`）/ join a JSON string array for display。
fn join_str_array(value: &Value) -> String {
    match value.as_array() {
        Some(arr) if !arr.is_empty() => arr
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(", "),
        _ => "—".to_string(),
    }
}

fn scope_of(rec: &InstalledPluginRecord) -> &str {
    rec.extra
        .get("scope")
        .and_then(Value::as_str)
        .unwrap_or("user")
}

// ── handlers ─────────────────────────────────────────────────────────────────
/// [`plugin_install`] 选项 / install options。
pub struct PluginInstallOptions<'a> {
    /// 物化记录 scope（`user|project|local`，默认 `user`）/ install-record scope。
    pub scope: &'a str,
    /// project/local scope 的锚定目录（#98：进程 cwd；`user` scope 不需要）/ anchor dir for project/local。
    pub project_path: Option<&'a str>,
    /// git tag/SHA 锁版本（`--version`）/ version override。
    pub version: Option<&'a str>,
    /// MCP 注入回调（REPL 装配；Typer 非交互 `None` = ledger-only）/ MCP hooks。
    pub hooks: Option<&'a dyn McpInstallHooks>,
    /// 结构化输出 / JSON output。
    pub json_output: bool,
}

impl Default for PluginInstallOptions<'_> {
    fn default() -> Self {
        Self {
            scope: "user",
            project_path: None,
            version: None,
            hooks: None,
            json_output: false,
        }
    }
}

/// 安装单个 plugin（外来 MCP 同名硬抛、原子失败，§10.6）/ install a plugin (foreign name conflict hard-throws)。
pub async fn plugin_install(
    registry: &mut SkillRegistry,
    home: &Path,
    env: Option<&EnvMap>,
    plugin_id: &str,
    opts: PluginInstallOptions<'_>,
) -> i32 {
    let json_output = opts.json_output;
    if split_plugin_id(plugin_id).is_none() {
        return plugin_err(
            &format!("invalid plugin id {plugin_id:?} (expected '<plugin>@<marketplace>')"),
            json_output,
            EXIT_USER_ERROR,
            None,
        );
    }
    let record = match install_plugin(
        plugin_id,
        registry,
        home,
        InstallOptions {
            scope: Some(opts.scope),
            project_path: opts.project_path,
            version: opts.version,
            refresh: false,
            timeout: None,
            env,
        },
        opts.hooks,
    )
    .await
    {
        Ok(record) => record,
        // §10.6 硬抛、退出码 1 + JSON error_code（无 rename/force 逃生口）。
        Err(PluginInstallError::Conflict(e)) => {
            return plugin_err(
                &e.to_string(),
                json_output,
                EXIT_USER_ERROR,
                Some("mcp_server_name_conflict"),
            )
        }
        // #275：跨 marketplace 同名 plugin（安装层唯一性）→ 结构化 error_code。
        Err(PluginInstallError::NameConflict(e)) => {
            return plugin_err(
                &e.to_string(),
                json_output,
                EXIT_USER_ERROR,
                Some("plugin_name_conflict"),
            )
        }
        Err(e) => return plugin_err(&e.to_string(), json_output, EXIT_USER_ERROR, None),
    };

    let servers = record.mcp_servers.clone();
    if json_output {
        print_json(&json!({
            "installed": plugin_id,
            "scope": record.extra.get("scope").cloned().unwrap_or(Value::Null),
            "bundledMcpServers": servers,
        }));
        return EXIT_OK;
    }
    let detail = if servers.is_empty() {
        String::new()
    } else {
        format!(" ({} MCP server(s) mounted)", servers.len())
    };
    ok_msg(&format!("installed {plugin_id:?}{detail}"))
}

/// 卸载单个 plugin（删 plugin 独占外部 clone，共享 catalog 不删 + 注销 skills + 级联停摘 bundled server +
/// 删账本）/ uninstall a plugin。
///
/// `non_plugin_bundle_ids` = 回收判据「非用户声明」数据源（`origin != plugin` 的 bundle_id 全集，含
/// durable/flag/embed）。**MUST 由持有 `Computer` 的调用方经
/// `Computer::non_plugin_declared_bundle_ids` 供给**——传空集会让用户/宿主自有 server 被连坐停摘（#139）。
/// `hooks == None` 的路径不发生停摘，该集未被读取。
#[allow(clippy::too_many_arguments)]
pub async fn plugin_uninstall(
    registry: &mut SkillRegistry,
    home: &Path,
    env: Option<&EnvMap>,
    plugin_id: &str,
    keep_servers: bool,
    non_plugin_bundle_ids: &HashSet<BundleId>,
    hooks: Option<&dyn McpInstallHooks>,
    json_output: bool,
) -> i32 {
    match uninstall_plugin(
        plugin_id,
        registry,
        home,
        UninstallOptions {
            scope: None,
            keep_servers,
            env,
        },
        non_plugin_bundle_ids,
        hooks,
    )
    .await
    {
        Ok(true) => {}
        Ok(false) => {
            return plugin_err(
                &format!("plugin {plugin_id:?} not installed (no-op)"),
                json_output,
                EXIT_USER_ERROR,
                None,
            )
        }
        Err(e) => return plugin_err(&e.to_string(), json_output, EXIT_USER_ERROR, None),
    }
    if json_output {
        print_json(&json!({ "uninstalled": plugin_id, "keptServers": keep_servers }));
        return EXIT_OK;
    }
    ok_msg(&format!(
        "uninstalled {plugin_id:?}{}",
        if keep_servers { " (servers kept)" } else { "" }
    ))
}

/// 该记录的 enable 写盘锚点：记录自带 `projectPath` 优先；缺失（如 rematerialize 后记录恒为
/// `scope=user`、无 projectPath）→ 回退调用方 `project_path_hint`（REPL 传进程 cwd）。该锚点同时用于
/// 失败诊断的合并视图（`user<project<local`），避免因读不到 local/project 层而误报未保留。
fn record_enable_project_path<'a>(
    rec: &'a InstalledPluginRecord,
    project_path_hint: Option<&'a str>,
) -> Option<&'a str> {
    rec.extra
        .get("projectPath")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or(project_path_hint)
}

/// 启用单个 plugin（旧签名 wrapper，下游兼容）：无运行期权威——existing-wins 退回
/// `hooks.mounted_servers()`、诊断只看文件三层（写后失败仍按 attempt-only 清理 hook 侧实例）。
/// SDK 内带 `Computer` 的调用方请用 [`plugin_enable_with_paths`]。
pub async fn plugin_enable(
    registry: &mut SkillRegistry,
    home: &Path,
    env: Option<&EnvMap>,
    plugin_id: &str,
    hooks: Option<&dyn McpInstallHooks>,
    json_output: bool,
) -> i32 {
    plugin_enable_with_paths(
        registry,
        home,
        env,
        plugin_id,
        None,
        None,
        None,
        hooks,
        json_output,
    )
    .await
}

/// 启用单个 plugin（带运行期权威的完整路径；#286）/ enable a plugin with runtime authority.
///
/// scope 契约：从 ledger 逐 scope 读；每条记录 register 之前先 `inject_inputs(installPath)`。
/// `project_path_hint`：记录缺 `projectPath` 时的**写盘/诊断**锚点（REPL/一次性子命令传 cwd）；
/// 记录自带 `projectPath` 时以其为准（两者必须同源——诊断不得随 cwd 漂移）。
/// `resolved_settings`：运行期五层 settings 合并视图的 **provider**（`Computer::resolved_runtime_settings_at`）——
/// 接收逐记录诊断锚点、覆盖 flag/policy 的 `enabledPlugins`；**MUST 现算**（pre/post 两时点，见
/// `EnableReclaimContext`）。
/// `non_plugin_bundle_ids`：写后失败对本次新挂做 §4.9.1-2 三档判定的「非用户声明」数据源，MUST 由持有
/// `Computer` 的调用方供给（REPL）；一次性子命令无活跃 server（hooks=None）→ `None`。
#[allow(clippy::too_many_arguments)] // CLI handler：显式资源 + flags（同 plugin_disable 风格）
pub async fn plugin_enable_with_paths(
    registry: &mut SkillRegistry,
    home: &Path,
    env: Option<&EnvMap>,
    plugin_id: &str,
    project_path_hint: Option<&str>,
    resolved_settings: Option<&crate::settings::installer::ResolvedSettingsProvider<'_>>,
    non_plugin_bundle_ids: Option<&HashSet<BundleId>>,
    hooks: Option<&dyn McpInstallHooks>,
    json_output: bool,
) -> i32 {
    let records = installed_records(home, env, plugin_id);
    if records.is_empty() {
        return plugin_err(
            &format!("plugin {plugin_id:?} not installed (install it first)"),
            json_output,
            EXIT_USER_ERROR,
            None,
        );
    }
    let mut scopes: Vec<Value> = Vec::new();
    for rec in &records {
        // 挂载前先把 plugin-scoped inputs 注入池（install_plugin 内部已注入，enable 须显式补）。
        if let (Some(hooks), Some(ip)) =
            (hooks, rec.install_path.as_deref().filter(|s| !s.is_empty()))
        {
            if let Err(e) = hooks.inject_inputs(Path::new(ip)).await {
                return plugin_err(&e.to_string(), json_output, EXIT_USER_ERROR, None);
            }
        }
        // 写盘与诊断 MUST 用同一个逐记录锚点（记录 `projectPath` 优先，缺省回退 hint）。
        let rec_project_path = record_enable_project_path(rec, project_path_hint);
        let res = enable_plugin_with_reclaim(
            plugin_id,
            registry,
            home,
            EnableOptions {
                scope: Some(scope_of(rec)),
                project_path: rec_project_path,
                timeout: None,
                env,
            },
            EnableReclaimContext {
                gate_project_path: rec_project_path,
                resolved_settings,
                // CLI 无 manager 快照注入 → 回退 CliMcpHooks::mounted_servers（其既有集为运行期已挂载）。
                mounted_bundle_ids: None,
                non_plugin_bundle_ids,
            },
            hooks,
        )
        .await;
        match res {
            Ok(_changed) => {}
            // Discussion#82：prior-true 失败会被 `EnableFailedRetained` 包装；打印外层文本以携带
            // 「既有启用状态已保留」提示，但 error_code 按剥壳后的内层变体分类（与未包装时一致）。
            Err(e) => {
                return plugin_err(
                    &e.to_string(),
                    json_output,
                    EXIT_USER_ERROR,
                    enable_failure_error_code(&e),
                )
            }
        }
        scopes.push(json!(scope_of(rec)));
    }
    if json_output {
        print_json(&json!({ "enabled": plugin_id, "scopes": scopes }));
        return EXIT_OK;
    }
    ok_msg(&format!("enabled {plugin_id:?}"))
}

/// 禁用单个 plugin = 整 plugin 下线（停摘 bundled server + 隐藏 skills；物化层保留可一键复原）/ disable a plugin。
/// `non_plugin_bundle_ids`：同 [`plugin_uninstall`]——回收判据「非用户声明」数据源，MUST 由持有 `Computer`
/// 的调用方供给；传空集会连坐用户/宿主自有 server（#139）。
pub async fn plugin_disable(
    registry: &mut SkillRegistry,
    home: &Path,
    env: Option<&EnvMap>,
    plugin_id: &str,
    non_plugin_bundle_ids: &HashSet<BundleId>,
    hooks: Option<&dyn McpInstallHooks>,
    json_output: bool,
) -> i32 {
    let records = installed_records(home, env, plugin_id);
    if records.is_empty() {
        return plugin_err(
            &format!("plugin {plugin_id:?} not installed (no-op)"),
            json_output,
            EXIT_USER_ERROR,
            None,
        );
    }
    let mut scopes: Vec<Value> = Vec::new();
    for rec in &records {
        if let Err(e) = disable_plugin(
            plugin_id,
            registry,
            home,
            DisableOptions {
                scope: Some(scope_of(rec)),
                project_path: rec.extra.get("projectPath").and_then(Value::as_str),
                env,
            },
            non_plugin_bundle_ids,
            hooks,
        )
        .await
        {
            return plugin_err(&e.to_string(), json_output, EXIT_USER_ERROR, None);
        }
        scopes.push(json!(scope_of(rec)));
    }
    if json_output {
        print_json(&json!({ "disabled": plugin_id, "scopes": scopes }));
        return EXIT_OK;
    }
    ok_msg(&format!(
        "disabled {plugin_id:?} (whole plugin offline; enable to restore)"
    ))
}

/// 列出 installed plugin（默认 enabled；`available` 含 disabled）/ list installed plugins。
pub fn plugin_list(
    home: &Path,
    env: Option<&EnvMap>,
    cwd: Option<&Path>,
    available: bool,
    json_output: bool,
) -> i32 {
    let installed = load_installed_plugins(Some(home), env).account;
    let intent = load_installed_plugins_intent(Some(home), env).account;
    let enabled_map = enabled_plugins_view(cwd, env);

    let mut rows: Vec<Value> = Vec::new();
    for (pid, records) in &installed.plugins {
        // v0.3.0：权威「已安装」= installedPlugins 意图；不在意图的账本记录 = 陈旧派生缓存（已 uninstall），跳过。
        if !intent.installed_plugins.contains(pid) {
            continue;
        }
        // v0.3.0：仅 enabledPlugins==true 视为启用（absent / false 均未启用，installed_disabled）。
        let enabled = enabled_map.get(pid) == Some(&Value::Bool(true));
        if !enabled && !available {
            continue;
        }
        let mut scopes: Vec<String> = records.iter().map(|r| scope_of(r).to_string()).collect();
        scopes.sort();
        scopes.dedup();
        let mut bundled: Vec<String> = records
            .iter()
            .flat_map(|r| r.mcp_servers.iter().map(|b| b.as_str().to_string()))
            .collect();
        bundled.sort();
        bundled.dedup();
        rows.push(json!({
            "id": pid,
            "enabled": enabled,
            "scopes": scopes,
            "bundledMcpServers": bundled,
        }));
    }
    rows.sort_by(|a, b| {
        a["id"]
            .as_str()
            .unwrap_or("")
            .cmp(b["id"].as_str().unwrap_or(""))
    });

    if json_output {
        print_json(&json!(rows));
        return EXIT_OK;
    }
    if rows.is_empty() {
        msg_dim("No plugins installed. Install one: plugin install <plugin>@<marketplace>");
        return EXIT_OK;
    }
    println!("Plugins:");
    for r in &rows {
        println!(
            "  {}  ·  enabled={}  ·  scopes={}  ·  mcp={}",
            r["id"].as_str().unwrap_or(""),
            if r["enabled"].as_bool().unwrap_or(false) {
                "✓"
            } else {
                "—"
            },
            join_str_array(&r["scopes"]),
            join_str_array(&r["bundledMcpServers"]),
        );
    }
    EXIT_OK
}

/// plugin 详情：scope / installPath / version / commitSha / enabled / bundledMcpServers / installedAt。
pub fn plugin_info(
    home: &Path,
    env: Option<&EnvMap>,
    plugin_id: &str,
    cwd: Option<&Path>,
    json_output: bool,
) -> i32 {
    // v0.3.0：权威「已安装」= installedPlugins 意图（账本记录仅供详情展示）。
    let intent = load_installed_plugins_intent(Some(home), env).account;
    if !intent.installed_plugins.contains(plugin_id) {
        return plugin_err(
            &format!("plugin {plugin_id:?} not installed"),
            json_output,
            EXIT_USER_ERROR,
            None,
        );
    }
    let records = installed_records(home, env, plugin_id);
    // v0.3.0：仅 enabledPlugins==true 视为启用（absent / false 均未启用）。
    let enabled = enabled_plugins_view(cwd, env).get(plugin_id) == Some(&Value::Bool(true));

    if json_output {
        print_json(&json!({ "id": plugin_id, "enabled": enabled, "records": records }));
        return EXIT_OK;
    }
    println!("Plugin · {plugin_id}");
    println!("  enabled: {}", if enabled { "✓" } else { "—" });
    let multi = records.len() > 1;
    for rec in &records {
        let prefix = if multi {
            format!("[{}] ", scope_of(rec))
        } else {
            String::new()
        };
        if let Some(ip) = &rec.install_path {
            println!("  {prefix}installPath: {ip}");
        }
        if !rec.mcp_servers.is_empty() {
            println!(
                "  {prefix}bundledMcpServers: {}",
                rec.mcp_servers
                    .iter()
                    .map(|b| b.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        for key in ["scope", "version", "commitSha", "installedAt"] {
            if let Some(v) = rec.extra.get(key) {
                println!("  {prefix}{key}: {v}");
            }
        }
    }
    EXIT_OK
}

/// 清理孤儿 plugin（账本有记录、但 pid 不在 `installedPlugins` 安装意图 = 陈旧派生缓存）/ GC orphan plugins。
///
/// v0.3.0：孤儿判定基于**安装意图**（非 `enabledPlugins`）——`installed_disabled` 仍是合法安装、绝不 gc。
///
/// `non_plugin_bundle_ids`：同 [`plugin_uninstall`]——回收判据「非用户声明」数据源，MUST 由持有 `Computer`
/// 的调用方供给；传空集会连坐用户/宿主自有 server（#139）。`teardown == None` 的路径不发生停摘。
#[allow(clippy::too_many_arguments)]
pub async fn plugin_gc(
    registry: &mut SkillRegistry,
    home: &Path,
    env: Option<&EnvMap>,
    _cwd: Option<&Path>,
    non_plugin_bundle_ids: &HashSet<BundleId>,
    teardown: Option<&dyn McpTeardown>,
    confirm: Option<&dyn Confirm>,
    json_output: bool,
) -> i32 {
    let store = file_store(home, env);
    let ledger_count = load_installed_plugins(Some(home), env)
        .account
        .plugins
        .len();
    // v0.3.0：孤儿判定读 store 的 installedPlugins 意图；declared/enabledPlugins 已不参与（传空占位）。
    let orphans = list_orphan_plugins(&Map::new(), &store);
    // 安全阀（防数据丢失）：若「孤儿 == 全部账本记录」且账本非空 → 视为 intent 未初始化 / 迁移未落地 / 损坏空
    // （intent 文件缺失或损坏被恢复为空皆落此），**拒绝** gc、绝不一次清空所有已装 plugin。正常态 intent 与账本
    // 同步（install/uninstall 双写），孤儿至多是**子集**；「全量孤儿」必为异常。正常迁移已在 CLI 分发 / `boot_up`
    // 跑过，此阀仅在迁移失败 / 文件损坏时兜底。
    if ledger_count > 0 && orphans.len() == ledger_count {
        return plugin_err(
            "install intent empty/unmigrated while ledger is non-empty; refusing gc to avoid deleting all installed plugins — run 'a2c-computer run' to migrate, then retry",
            json_output,
            EXIT_USER_ERROR,
            None,
        );
    }
    if orphans.is_empty() {
        if json_output {
            print_json(&json!({ "removed": [] }));
        } else {
            msg_dim("No orphan plugins.");
        }
        return EXIT_OK;
    }
    if let Some(confirm) = confirm {
        if !confirm.confirm(&orphans.join(", ")).await {
            return plugin_err("aborted by user", json_output, EXIT_USER_ERROR, None);
        }
    }
    let removed = gc_plugins(
        &orphans,
        registry,
        home,
        &store,
        non_plugin_bundle_ids,
        teardown,
    )
    .await;
    if json_output {
        print_json(&json!({ "removed": removed }));
        return EXIT_OK;
    }
    ok_msg(&format!(
        "gc removed {} orphan plugin(s): {}",
        removed.len(),
        if removed.is_empty() {
            "—".to_string()
        } else {
            removed.join(", ")
        }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::commands::test_env;
    use crate::settings::scope::user_settings_path;
    use crate::settings::store::update_installed_plugins;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn seed_plugin(home: &Path, env: &HashMap<String, String>, pid: &str, scope: &str) {
        // v0.3.0：权威「已安装」= installedPlugins 意图（否则 list/info 视为未装）。
        crate::settings::store::update_installed_plugins_intent(
            |file| {
                file.account.installed_plugins.insert(pid.to_string());
            },
            Some(home),
            Some(env),
        )
        .unwrap();
        update_installed_plugins(
            |file| {
                file.account.plugins.insert(
                    pid.to_string(),
                    vec![InstalledPluginRecord {
                        install_path: Some(home.join("x").to_string_lossy().into_owned()),
                        mcp_servers: vec![crate::mcp_clients::model::BundleId::try_from(
                            "figma-mcp".to_string(),
                        )
                        .unwrap()],
                        extra: Map::from_iter([("scope".to_string(), json!(scope))]),
                    }],
                );
            },
            Some(home),
            Some(env),
        )
        .unwrap();
    }

    #[test]
    fn split_id_rejects_malformed() {
        assert_eq!(split_plugin_id("figma@acme"), Some(("figma", "acme")));
        assert_eq!(split_plugin_id("figma"), None);
        assert_eq!(split_plugin_id("@acme"), None);
        assert_eq!(split_plugin_id("figma@"), None);
    }

    #[test]
    fn record_enable_project_path_prefers_record_then_hint() {
        // #286/D：记录缺 projectPath → 回退 REPL cwd；记录有值（非空）→ 记录优先；空串视同缺失。
        let make = |pp: Option<&str>| InstalledPluginRecord {
            install_path: None,
            mcp_servers: Vec::new(),
            extra: {
                let mut m = Map::new();
                m.insert("scope".to_string(), json!("user"));
                if let Some(pp) = pp {
                    m.insert("projectPath".to_string(), json!(pp));
                }
                m
            },
        };
        assert_eq!(
            record_enable_project_path(&make(None), Some("/cwd")),
            Some("/cwd"),
            "记录无 projectPath → 回退调用方锚点"
        );
        assert_eq!(
            record_enable_project_path(&make(Some("/rec")), Some("/cwd")),
            Some("/rec"),
            "记录有 projectPath → 记录优先"
        );
        assert_eq!(
            record_enable_project_path(&make(Some("")), Some("/cwd")),
            Some("/cwd"),
            "空串 projectPath 视同缺失"
        );
        assert_eq!(record_enable_project_path(&make(None), None), None);
    }

    /// Discussion#82：CLI enable 预检失败零写零回收；写后失败按 `project_path_hint` 写盘/恢复锚点，
    /// 并清理本次新挂 server。`resolved_settings` 的诊断接线由 installer 单测覆盖。
    #[tokio::test]
    async fn plugin_enable_with_paths_preflight_and_write_after_82() {
        use crate::mcp_clients::model::{BundleId as TestBundleId, MCPServerConfig, ServerName};
        use crate::settings::installer::McpHookError;
        use std::sync::{Arc, Mutex};

        struct RecordingHooks {
            removed: Mutex<Vec<String>>,
            fail_register: bool,
        }
        #[async_trait::async_trait]
        impl McpInstallHooks for RecordingHooks {
            fn existing_servers(&self) -> HashMap<TestBundleId, ServerName> {
                HashMap::new()
            }
            async fn register_server(&self, cfg: MCPServerConfig) -> Result<(), McpHookError> {
                if self.fail_register {
                    return Err(McpHookError(format!("boom on {}", cfg.name())));
                }
                Ok(())
            }
            async fn remove_server(&self, id: &TestBundleId) -> Result<(), McpHookError> {
                self.removed.lock().unwrap().push(id.as_str().to_string());
                Ok(())
            }
        }

        // A) 预检失败（installPath 缺失）：零写零回收——即使提供了 non_plugin 判据也不得摘。
        let dir_a = tempdir().unwrap();
        let env_a = test_env(dir_a.path());
        seed_plugin(dir_a.path(), &env_a, "figma@acme", "user");
        let hooks_a = RecordingHooks {
            removed: Mutex::new(Vec::new()),
            fail_register: false,
        };
        let code = plugin_enable_with_paths(
            &mut SkillRegistry::new(),
            dir_a.path(),
            Some(&env_a),
            "figma@acme",
            None,
            None,
            Some(&HashSet::new()),
            Some(&hooks_a),
            true,
        )
        .await;
        assert_eq!(code, EXIT_USER_ERROR);
        assert!(
            hooks_a.removed.lock().unwrap().is_empty(),
            "预检失败零写零回收（Discussion#82）"
        );

        // B) 写后失败（register 失败）+ project scope 无 projectPath：hint 决定写盘/恢复锚点；
        //    失败后 project key 精确删键、本次新挂 server 撤除。hint 未接线时写盘前置失败，
        //    project settings 不会存在 → 下面的读取会 panic（测试对接线敏感）。
        let dir_b = tempdir().unwrap();
        let env_b = test_env(dir_b.path());
        let proj = dir_b.path().join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        seed_real_enable_fixture(dir_b.path(), &env_b, "project");
        let hooks_b = RecordingHooks {
            removed: Mutex::new(Vec::new()),
            fail_register: true,
        };
        // provider 接线守护：记录收到的诊断锚点，证明 CLI 未把 resolved_settings 置 None、
        // 且锚点 = 操作 project_path（而非宿主默认）。
        let seen_anchors: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
        let provider = {
            let seen = Arc::clone(&seen_anchors);
            move |anchor: Option<&str>| {
                seen.lock().unwrap().push(anchor.map(str::to_string));
                let mut ep = Map::new();
                ep.insert("figma@acme".to_string(), Value::Bool(false));
                let mut m = Map::new();
                m.insert("enabledPlugins".to_string(), Value::Object(ep));
                m
            }
        };
        let code = plugin_enable_with_paths(
            &mut SkillRegistry::new(),
            dir_b.path(),
            Some(&env_b),
            "figma@acme",
            proj.to_str(),
            Some(&provider),
            Some(&HashSet::new()),
            Some(&hooks_b),
            true,
        )
        .await;
        assert_eq!(code, EXIT_USER_ERROR);
        let want_anchor = proj.to_string_lossy().into_owned();
        let anchors = seen_anchors.lock().unwrap().clone();
        assert!(
            !anchors.is_empty(),
            "CLI 必须把 resolved_settings provider 传给 installer（接线守护）"
        );
        assert!(
            anchors
                .iter()
                .all(|a| a.as_deref() == Some(want_anchor.as_str())),
            "provider 诊断锚点必须是操作 project_path：{anchors:?}"
        );
        let settings = crate::settings::scope::workdir_project_settings_path(&proj);
        let v: Value = serde_json::from_str(
            &std::fs::read_to_string(&settings)
                .unwrap_or_else(|e| panic!("hint 未接线：project settings 未写 ({e})")),
        )
        .unwrap();
        assert!(
            v.get("enabledPlugins")
                .and_then(|m| m.get("figma@acme"))
                .is_none(),
            "写后失败 MUST 恢复 project pre-state（原 absent → 删键）"
        );
        assert_eq!(
            *hooks_b.removed.lock().unwrap(),
            vec!["figma-mcp".to_string()],
            "本次新挂 server 必须清理"
        );
    }

    /// 真实 enable 夹具：install 树（skill + bundled server）+ 本地 git marketplace（acme/figma）
    /// + 账本记录（installPath = `<home>/x`，声明 `figma-mcp`）。
    fn seed_real_enable_fixture(
        home: &Path,
        env: &HashMap<String, String>,
        scope: &str,
    ) -> PathBuf {
        let root = home.join("x");
        let skill = root.join("skills/code-review");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: code-review\ndescription: d\n---\nbody",
        )
        .unwrap();
        let sd = root.join("mcp-servers");
        std::fs::create_dir_all(&sd).unwrap();
        std::fs::write(
            sd.join("figma-mcp.json"),
            r#"{"type":"stdio","name":"figma-mcp","server_parameters":{"command":"node"}}"#,
        )
        .unwrap();

        let repo = home.join("repo-acme");
        std::fs::create_dir_all(repo.join(".tfrobot-plugin")).unwrap();
        std::fs::write(
            repo.join(".tfrobot-plugin/marketplace.json"),
            r#"{"name":"acme","plugins":[{"name":"figma","source":"./plugins/figma"}]}"#,
        )
        .unwrap();
        let rskill = repo.join("plugins/figma/skills/code-review");
        std::fs::create_dir_all(&rskill).unwrap();
        std::fs::write(
            rskill.join("SKILL.md"),
            "---\nname: code-review\ndescription: d\n---\nbody",
        )
        .unwrap();
        for args in [
            &["init", "-q"][..],
            &["add", "-A"][..],
            &["commit", "-qm", "init"][..],
        ] {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("HOME", &repo)
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
        let url = format!("file://{}", repo.display());
        crate::settings::store::update_known_marketplaces(
            |f| {
                f.account.marketplaces.insert(
                    "acme".to_string(),
                    crate::settings::KnownMarketplaceEntry {
                        manifest_name_baseline: None,
                        source: json!({"type": "git", "url": url}),
                        extra: Map::new(),
                    },
                );
            },
            Some(home),
            Some(env),
        )
        .unwrap();
        seed_plugin(home, env, "figma@acme", scope);
        root
    }

    /// CLI 剥壳（Discussion#82 / 架构师确认）：`EnableFailedRetained` 包装不得改变内层 error_code；
    /// prior-false 原变体直接分类。覆盖 ContentUnavailable / NameConflict / Mcp 冲突三类。
    #[test]
    fn enable_failure_error_code_unwraps_retained_variant_82() {
        use crate::settings::installer::{
            McpServerNameConflictError, PluginInstallError, PluginNameConflictError,
            PluginNameConflictKind,
        };

        let unavailable = PluginInstallError::ContentUnavailable {
            plugin_id: "figma@acme".to_string(),
            detail: "install_path_missing: root".to_string(),
        };
        // 未包装：直接原变体，无 error_code。
        assert_eq!(enable_failure_error_code(&unavailable), None);
        // 包装：剥壳后仍是 ContentUnavailable，无 error_code；Display 携带 retained 提示。
        let wrapped_unavailable = PluginInstallError::EnableFailedRetained {
            source: Box::new(unavailable),
        };
        assert_eq!(enable_failure_error_code(&wrapped_unavailable), None);
        assert!(wrapped_unavailable
            .to_string()
            .contains("existing enabled state retained"));

        // NameConflict（#275 跨 marketplace 同名）：剥壳后保留 plugin_name_conflict。
        let name_conflict = PluginInstallError::NameConflict(PluginNameConflictError {
            requested_pid: "figma@acme".to_string(),
            plugin: "figma".to_string(),
            kind: PluginNameConflictKind::Installed {
                installed_pid: "figma@other".to_string(),
            },
        });
        assert_eq!(
            enable_failure_error_code(&name_conflict),
            Some("plugin_name_conflict")
        );
        let wrapped_name = PluginInstallError::EnableFailedRetained {
            source: Box::new(name_conflict),
        };
        assert_eq!(
            enable_failure_error_code(&wrapped_name),
            Some("plugin_name_conflict")
        );

        // Mcp 同名冲突（§10.6）：剥壳后保留 mcp_server_name_conflict。
        let conflict =
            PluginInstallError::Conflict(McpServerNameConflictError("figma-mcp".to_string()));
        assert_eq!(
            enable_failure_error_code(&conflict),
            Some("mcp_server_name_conflict")
        );
        let wrapped_conflict = PluginInstallError::EnableFailedRetained {
            source: Box::new(conflict),
        };
        assert_eq!(
            enable_failure_error_code(&wrapped_conflict),
            Some("mcp_server_name_conflict")
        );
    }

    #[tokio::test]
    async fn install_invalid_id_is_user_error() {
        let dir = tempdir().unwrap();
        let env = test_env(dir.path());
        let mut registry = SkillRegistry::new();
        let code = plugin_install(
            &mut registry,
            dir.path(),
            Some(&env),
            "no-at-sign",
            PluginInstallOptions {
                json_output: true,
                ..Default::default()
            },
        )
        .await;
        assert_eq!(code, EXIT_USER_ERROR);
    }

    #[tokio::test]
    async fn uninstall_and_enable_not_installed_are_user_errors() {
        let dir = tempdir().unwrap();
        let env = test_env(dir.path());
        let mut registry = SkillRegistry::new();
        assert_eq!(
            plugin_uninstall(
                &mut registry,
                dir.path(),
                Some(&env),
                "x@y",
                false,
                &HashSet::new(),
                None,
                true
            )
            .await,
            EXIT_USER_ERROR
        );
        assert_eq!(
            plugin_enable(&mut registry, dir.path(), Some(&env), "x@y", None, true).await,
            EXIT_USER_ERROR
        );
        assert_eq!(
            plugin_disable(
                &mut registry,
                dir.path(),
                Some(&env),
                "x@y",
                &HashSet::new(),
                None,
                true
            )
            .await,
            EXIT_USER_ERROR
        );
    }

    #[test]
    fn list_and_info_reflect_enabled_and_scope() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let env = test_env(home);
        seed_plugin(home, &env, "figma@acme", "user");

        // cwd 注入 tempdir（无 .tfrobot）→ project/local 判空，隔离真实进程 cwd（接缝确定性）。
        let cwd = Some(home);
        // v0.3.0：已装（在 installedPlugins 意图）→ list/info 均 EXIT_OK；enabledPlugins 缺省 = installed_disabled
        // （info enabled=false、默认 list 不展示，须 --available）。
        assert_eq!(plugin_list(home, Some(&env), cwd, true, true), EXIT_OK);
        assert_eq!(
            plugin_info(home, Some(&env), "figma@acme", cwd, true),
            EXIT_OK
        );

        // 写 enabledPlugins[figma@acme]=false → 默认 list（available=false）应跳过。
        let usp = user_settings_path(Some(&env));
        std::fs::create_dir_all(usp.parent().unwrap()).unwrap();
        std::fs::write(
            &usp,
            serde_json::to_string(&json!({ "enabledPlugins": { "figma@acme": false } })).unwrap(),
        )
        .unwrap();
        let view = enabled_plugins_view(cwd, Some(&env));
        assert_eq!(view.get("figma@acme"), Some(&Value::Bool(false)));
    }

    #[tokio::test]
    async fn gc_no_orphans_is_ok() {
        let dir = tempdir().unwrap();
        let env = test_env(dir.path());
        let mut registry = SkillRegistry::new();
        // cwd 注入 tempdir → project/local 判空，隔离真实进程 cwd（接缝确定性）。
        let code = plugin_gc(
            &mut registry,
            dir.path(),
            Some(&env),
            Some(dir.path()),
            &HashSet::new(),
            None,
            None,
            true,
        )
        .await;
        assert_eq!(code, EXIT_OK);
    }

    #[tokio::test]
    async fn gc_collects_orphan_when_installed_but_not_in_intent() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let env = test_env(home);
        // v0.3.0：孤儿 = 账本有记录、但 pid **不在** installedPlugins 意图。keeper@acme 账本+意图都有（合法已装）；
        // ghost@acme 仅账本、不在意图（真孤儿）。**部分**孤儿（非全量）→ 安全阀放行 → gc 清 ghost、留 keeper。
        update_installed_plugins(
            |file| {
                for pid in ["keeper@acme", "ghost@acme"] {
                    file.account.plugins.insert(
                        pid.to_string(),
                        vec![InstalledPluginRecord {
                            install_path: Some(home.join(pid).to_string_lossy().into_owned()),
                            mcp_servers: vec![],
                            extra: Map::from_iter([("scope".to_string(), json!("user"))]),
                        }],
                    );
                }
            },
            Some(home),
            Some(&env),
        )
        .unwrap();
        // 意图只含 keeper@acme（迁移已跑）→ ghost@acme 为真孤儿。
        crate::settings::store::update_installed_plugins_intent(
            |f| {
                f.account
                    .installed_plugins
                    .insert("keeper@acme".to_string());
            },
            Some(home),
            Some(&env),
        )
        .unwrap();
        let mut registry = SkillRegistry::new();
        // cwd 注入 tempdir → project/local 判空，隔离真实进程 cwd（接缝确定性）。
        let code = plugin_gc(
            &mut registry,
            home,
            Some(&env),
            Some(home),
            &HashSet::new(),
            None,
            None,
            true,
        )
        .await;
        assert_eq!(code, EXIT_OK);
        // ghost 被清、keeper 保留。
        assert!(installed_records(home, Some(&env), "ghost@acme").is_empty());
        assert!(
            !installed_records(home, Some(&env), "keeper@acme").is_empty(),
            "非孤儿 keeper 不得被删"
        );
    }

    /// 只写账本、不写 intent（模拟未迁移 v0.2.x 态）+ 不跑迁移 → gc **不**得删已装 plugin。
    fn seed_ledger_only(
        home: &Path,
        env: &HashMap<String, String>,
        pid: &str,
        install_path: &Path,
    ) {
        std::fs::create_dir_all(install_path).unwrap();
        update_installed_plugins(
            |file| {
                file.account.plugins.insert(
                    pid.to_string(),
                    vec![InstalledPluginRecord {
                        install_path: Some(install_path.to_string_lossy().into_owned()),
                        mcp_servers: vec![],
                        extra: Map::from_iter([("scope".to_string(), json!("user"))]),
                    }],
                );
            },
            Some(home),
            Some(env),
        )
        .unwrap();
    }

    // 🔴 回归：v0.2.x 账本 + intent 文件缺失（迁移未跑）→ gc **拒绝**、绝不静默删已装 plugin（数据丢失防线）。
    #[tokio::test]
    async fn gc_refuses_when_intent_missing_but_ledger_nonempty() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let env = test_env(home);
        let install_path = home.join("plug");
        seed_ledger_only(home, &env, "legacy@acme", &install_path);
        assert!(
            !crate::settings::store::installed_plugins_intent_path(Some(home), Some(&env)).exists()
        );

        let mut registry = SkillRegistry::new();
        let code = plugin_gc(
            &mut registry,
            home,
            Some(&env),
            Some(home),
            &HashSet::new(),
            None,
            None,
            true,
        )
        .await;
        // 安全阀：拒绝（用户错），账本记录仍在、物化树未删。
        assert_eq!(code, EXIT_USER_ERROR);
        assert!(
            !installed_records(home, Some(&env), "legacy@acme").is_empty(),
            "未迁移时 gc 不得删已装 plugin（防数据丢失）"
        );
        assert!(install_path.exists(), "物化树不得被删");
    }

    // 迁移后 intent 含已装 plugin → 非孤儿 → gc 保留（🔴 修复的正向验证）。
    #[tokio::test]
    async fn gc_after_migration_keeps_installed_plugin() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let env = test_env(home);
        let install_path = home.join("plug");
        seed_ledger_only(home, &env, "legacy@acme", &install_path);
        // 模拟 CLI 分发 / boot 的一次性迁移。
        crate::settings::installer::migrate_ledger_to_intent_once(home, Some(&env)).unwrap();

        let mut registry = SkillRegistry::new();
        let code = plugin_gc(
            &mut registry,
            home,
            Some(&env),
            Some(home),
            &HashSet::new(),
            None,
            None,
            true,
        )
        .await;
        assert_eq!(code, EXIT_OK);
        assert!(
            !installed_records(home, Some(&env), "legacy@acme").is_empty(),
            "迁移后 intent 含该 plugin → 非孤儿 → gc 保留"
        );
        assert!(install_path.exists());
    }
}
