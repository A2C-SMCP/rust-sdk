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
* [`install_plugin`] / [`uninstall_plugin`] / [`enable_plugin`] / [`disable_plugin`] 四动词 +
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

use std::path::Path;

use serde_json::{json, Map, Value};

use super::{
    file_store, msg_dim, msg_err, ok_msg, print_json, resolved_settings, Confirm, EXIT_OK,
    EXIT_USER_ERROR,
};
use crate::settings::installer::{
    disable_plugin, enable_plugin, install_plugin, uninstall_plugin, DisableOptions, EnableOptions,
    InstallOptions, McpInstallHooks, PluginInstallError, UninstallOptions,
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
        Err(e) => return plugin_err(&e.to_string(), json_output, EXIT_USER_ERROR, None),
    };

    let servers = record.bundled_mcp_servers.clone();
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

/// 卸载单个 plugin（删 installPath 树 + 注销 skills + 级联停摘 bundled server + 删账本）/ uninstall a plugin。
pub async fn plugin_uninstall(
    registry: &mut SkillRegistry,
    home: &Path,
    env: Option<&EnvMap>,
    plugin_id: &str,
    keep_servers: bool,
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

/// 启用单个 plugin（廉价复原：重挂 server + 复活 skills）/ enable a plugin (cheap restore)。
///
/// scope 契约：从 ledger 逐 scope 读；每条记录 register 之前先 `inject_inputs(installPath)`。
pub async fn plugin_enable(
    registry: &mut SkillRegistry,
    home: &Path,
    env: Option<&EnvMap>,
    plugin_id: &str,
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
        let res = enable_plugin(
            plugin_id,
            registry,
            home,
            EnableOptions {
                scope: Some(scope_of(rec)),
                project_path: rec.extra.get("projectPath").and_then(Value::as_str),
                timeout: None,
                env,
            },
            hooks,
        )
        .await;
        match res {
            Ok(()) => {}
            Err(PluginInstallError::Conflict(e)) => {
                return plugin_err(
                    &e.to_string(),
                    json_output,
                    EXIT_USER_ERROR,
                    Some("mcp_server_name_conflict"),
                )
            }
            Err(e) => return plugin_err(&e.to_string(), json_output, EXIT_USER_ERROR, None),
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
pub async fn plugin_disable(
    registry: &mut SkillRegistry,
    home: &Path,
    env: Option<&EnvMap>,
    plugin_id: &str,
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
            .flat_map(|r| r.bundled_mcp_servers.iter().cloned())
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
        if !rec.bundled_mcp_servers.is_empty() {
            println!(
                "  {prefix}bundledMcpServers: {}",
                rec.bundled_mcp_servers.join(", ")
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
#[allow(clippy::too_many_arguments)]
pub async fn plugin_gc(
    registry: &mut SkillRegistry,
    home: &Path,
    env: Option<&EnvMap>,
    _cwd: Option<&Path>,
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
    let removed = gc_plugins(&orphans, registry, home, &store, teardown).await;
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
                        bundled_mcp_servers: vec!["figma-mcp".to_string()],
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
            plugin_disable(&mut registry, dir.path(), Some(&env), "x@y", None, true).await,
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
                            bundled_mcp_servers: vec![],
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
                        bundled_mcp_servers: vec![],
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
