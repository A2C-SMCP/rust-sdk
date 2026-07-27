/*!
* 文件名: repl.rs
* 作者: JQQ
* 创建日期: 2026/06/10
* 最后修改日期: 2026/06/10
* 版权: 2023 JQQ. All rights reserved.
* 依赖: async-trait, console
* 描述: REPL governance 命令分发（marketplace/plugin/settings/skill）/ REPL governance dispatch.
*
* 对标 Python 各命令模块的 `repl_dispatch`：把 REPL 行解析为 flags → 调 handler（绑定活跃 `Computer` 的
* registry/home/session）；变更后触发去抖 emit（`mark_skills_dirty`）。Rust 经 `Computer::skill_registry_arc`
* 取写锁拿 `&mut SkillRegistry`，`CliMcpHooks` 装配 MCP 注入回调，`ReplConfirm`/`ReplTeardown` 走 stdin 交互。
*
 * **锁序（#106 后）**：本路径取 `skill_registry.write` → 经 hooks 调 `add_or_update_server`/`remove_server`
 * 取 `mcp_manager` 锁。运行期 MCP 变化消费者（McpChangeReactor）已并发接线、取 `mcp_manager.read` →
 * `skill_registry.write`（相反序）；为消除 ABBA，`add_or_update_server` 惰性初始化改为「先 read 探测、仅 None 才
 * write」，使 post-boot 本路径退化为 `skill_registry.write` → `mcp_manager.read`，与消费者读锁读读相容（详见
 * computer.rs `add_or_update_server` / `restage_mcp_skills` 注释）；写锁在 `mark_skills_dirty` 前均显式 `drop`。
*/

use std::path::Path;

use async_trait::async_trait;

use super::commands::marketplace::{
    marketplace_add, marketplace_info, marketplace_list, marketplace_refresh, marketplace_remove,
    marketplace_set, MarketplaceAddOptions, MarketplaceRemoveOptions,
};
use super::commands::plugin::{
    plugin_disable, plugin_enable, plugin_gc, plugin_info, plugin_install, plugin_list,
    plugin_uninstall, PluginInstallOptions,
};
use super::commands::settings::{
    is_emit_key, settings_edit, settings_get, settings_set, settings_show,
};
use super::commands::skill::{skill_info, skill_list};
use super::commands::{flag_value, msg_warn, CliMcpHooks, Confirm, EXIT_OK};
use super::progress::clone_spinner;
use crate::computer::{Computer, Session};
use crate::settings::reconciler::McpTeardown;

// ── 交互回调（stdin）/ interactive callbacks via stdin ────────────────────────
async fn read_line_async(prompt: String) -> String {
    tokio::task::spawn_blocking(move || {
        use std::io::Write;
        print!("{prompt}");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        line.trim().to_lowercase()
    })
    .await
    .unwrap_or_default()
}

/// REPL y/N 确认（信任 / 移除 / gc）/ REPL y/N confirm。
struct ReplConfirm;

#[async_trait]
impl Confirm for ReplConfirm {
    async fn confirm(&self, target: &str) -> bool {
        let ans = read_line_async(format!("⚠ confirm: {target}\n  Continue? [y/N]: ")).await;
        matches!(ans.as_str(), "y" | "yes")
    }
}

/// gc 批量停摘 bundled server / batch teardown for gc。
struct ReplTeardown<'a, S: Session> {
    comp: &'a Computer<S>,
}

#[async_trait]
impl<S: Session> McpTeardown for ReplTeardown<'_, S> {
    async fn teardown(&self, servers: Vec<crate::mcp_clients::model::BundleId>) {
        for id in servers {
            // #113 S6：gc 停摘 bundled server 走**运行期卸载**（不删 config 声明）——bundled 归属 ledger、非用户 config。
            // #139/#141：按 **bundle_id** 精确停摘——经合并后的 `unmount_server(&BundleId)`（R4）。
            if let Err(e) = self.comp.unmount_server(&id).await {
                tracing::warn!(bundle_id = %id.as_str(), error = %e, "gc teardown: unmount failed");
            }
        }
    }
}

// ── 行解析辅助 / line-parse helpers ──────────────────────────────────────────
fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

fn positionals(args: &[String]) -> Vec<String> {
    args.iter()
        .filter(|a| !a.starts_with("--"))
        .cloned()
        .collect()
}

fn split_plugin_id(plugin_id: &str) -> Option<(String, String)> {
    let (plugin, marketplace) = plugin_id.split_once('@')?;
    (!plugin.is_empty() && !marketplace.is_empty())
        .then(|| (plugin.to_string(), marketplace.to_string()))
}

// ── marketplace ──────────────────────────────────────────────────────────────
pub async fn dispatch_marketplace<S: Session>(comp: &Computer<S>, parts: &[&str]) {
    let sub = parts[1].to_lowercase();
    let args: Vec<String> = parts[2..].iter().map(|s| s.to_string()).collect();
    let json = has_flag(&args, "--json");
    let pos = positionals(&args);
    let home = comp.skill_home();
    let reg_arc = comp.skill_registry_arc();
    let confirm = ReplConfirm;

    match sub.as_str() {
        "add" => {
            let Some(url) = pos.first() else {
                msg_warn("usage: marketplace add <git-url> [--name N] [--trust] [--auto-update] [--no-clone]");
                return;
            };
            let no_clone = has_flag(&args, "--no-clone");
            let name = flag_value(&args, "--name");
            let mut registry = reg_arc.write().await;
            let _spinner = clone_spinner("Cloning marketplace...", !json && !no_clone);
            let code = marketplace_add(
                &mut registry,
                &home,
                None,
                url,
                MarketplaceAddOptions {
                    name: name.as_deref(),
                    trust: has_flag(&args, "--trust"),
                    auto_update: has_flag(&args, "--auto-update"),
                    no_clone,
                    confirm: Some(&confirm),
                    json_output: json,
                },
            )
            .await;
            drop(_spinner);
            drop(registry);
            if code == EXIT_OK {
                comp.mark_skills_dirty();
            }
        }
        "list" => {
            marketplace_list(&home, None, json);
        }
        "info" => {
            let Some(name) = pos.first() else {
                msg_warn("usage: marketplace info <name>");
                return;
            };
            marketplace_info(&home, None, name, json);
        }
        "remove" | "rm" => {
            let Some(name) = pos.first() else {
                msg_warn("usage: marketplace remove <name> [--keep-plugins]");
                return;
            };
            let hooks = CliMcpHooks::new(comp, None, None).await;
            // #139：marketplace 级联卸载的回收判据数据源（全集）——传空集会连坐用户/宿主自有 server。
            let non_plugin = comp.non_plugin_declared_bundle_ids(&home, None);
            let mut registry = reg_arc.write().await;
            let code = marketplace_remove(
                &mut registry,
                &home,
                None,
                name,
                MarketplaceRemoveOptions {
                    keep_plugins: has_flag(&args, "--keep-plugins"),
                    confirm: Some(&confirm),
                    hooks: Some(&hooks),
                    json_output: json,
                },
                &non_plugin,
            )
            .await;
            drop(registry);
            if code == EXIT_OK {
                comp.mark_skills_dirty();
            }
        }
        "refresh" => {
            let target = pos.first().map(String::as_str).unwrap_or("all").to_string();
            let mut registry = reg_arc.write().await;
            let _spinner = clone_spinner("Refreshing marketplaces...", !json);
            let code = marketplace_refresh(&mut registry, &home, None, &target, json).await;
            drop(_spinner);
            drop(registry);
            if code == EXIT_OK {
                comp.mark_skills_dirty();
            }
        }
        "set" => {
            if pos.len() < 2 || !pos[1].contains('=') {
                msg_warn("usage: marketplace set <name> auto-update=<bool>");
                return;
            }
            let (key, value) = pos[1].split_once('=').unwrap_or((pos[1].as_str(), ""));
            marketplace_set(&home, None, &pos[0], key, value, json);
        }
        other => msg_warn(&format!("unknown marketplace subcommand: {other}")),
    }
}

// ── plugin ───────────────────────────────────────────────────────────────────
pub async fn dispatch_plugin<S: Session>(comp: &Computer<S>, parts: &[&str]) {
    let sub = parts[1].to_lowercase();
    let args: Vec<String> = parts[2..].iter().map(|s| s.to_string()).collect();
    let json = has_flag(&args, "--json");
    let pos = positionals(&args);
    let home = comp.skill_home();
    let reg_arc = comp.skill_registry_arc();
    // #98：project/local scope 锚定进程 cwd（`Computer` 不再持有 workspace）。
    let cwd = std::env::current_dir().ok();

    match sub.as_str() {
        "install" => {
            let Some(id) = pos.first() else {
                msg_warn("usage: plugin install <plugin>@<marketplace> [--version V] [--scope S]");
                return;
            };
            let Some((plugin, marketplace)) = split_plugin_id(id) else {
                msg_warn("invalid plugin id (expected '<plugin>@<marketplace>')");
                return;
            };
            let scope = flag_value(&args, "--scope").unwrap_or_else(|| "user".to_string());
            let version = flag_value(&args, "--version");
            let project_path = if scope == "project" || scope == "local" {
                cwd.as_deref().and_then(Path::to_str)
            } else {
                None
            };
            let hooks = CliMcpHooks::new(comp, Some(plugin), Some(marketplace)).await;
            let mut registry = reg_arc.write().await;
            let _spinner = clone_spinner("Installing plugin...", !json);
            let code = plugin_install(
                &mut registry,
                &home,
                None,
                id,
                PluginInstallOptions {
                    scope: &scope,
                    project_path,
                    version: version.as_deref(),
                    hooks: Some(&hooks),
                    json_output: json,
                },
            )
            .await;
            drop(_spinner);
            drop(registry);
            if code == EXIT_OK {
                comp.mark_skills_dirty();
            }
        }
        "uninstall" => {
            let Some(id) = pos.first() else {
                msg_warn("usage: plugin uninstall <plugin>@<marketplace> [--keep-servers]");
                return;
            };
            let hooks = CliMcpHooks::new(comp, None, None).await;
            // #139：回收判据数据源（durable + flag + embed 的 `origin != plugin` 全集）——传空集会连坐
            // 用户/宿主自有 server。
            let non_plugin = comp.non_plugin_declared_bundle_ids(&home, None);
            let mut registry = reg_arc.write().await;
            let code = plugin_uninstall(
                &mut registry,
                &home,
                None,
                id,
                has_flag(&args, "--keep-servers"),
                &non_plugin,
                Some(&hooks),
                json,
            )
            .await;
            drop(registry);
            if code == EXIT_OK {
                comp.mark_skills_dirty();
            }
        }
        "enable" => {
            let Some(id) = pos.first() else {
                msg_warn("usage: plugin enable <plugin>@<marketplace>");
                return;
            };
            let Some((plugin, marketplace)) = split_plugin_id(id) else {
                msg_warn("invalid plugin id (expected '<plugin>@<marketplace>')");
                return;
            };
            let hooks = CliMcpHooks::new(comp, Some(plugin), Some(marketplace)).await;
            let mut registry = reg_arc.write().await;
            let code = plugin_enable(&mut registry, &home, None, id, Some(&hooks), json).await;
            drop(registry);
            if code == EXIT_OK {
                comp.mark_skills_dirty();
            }
        }
        "disable" => {
            let Some(id) = pos.first() else {
                msg_warn("usage: plugin disable <plugin>@<marketplace>");
                return;
            };
            let hooks = CliMcpHooks::new(comp, None, None).await;
            // #139：回收判据数据源（同 uninstall）——传空集会连坐用户/宿主自有 server。
            let non_plugin = comp.non_plugin_declared_bundle_ids(&home, None);
            let mut registry = reg_arc.write().await;
            let code = plugin_disable(
                &mut registry,
                &home,
                None,
                id,
                &non_plugin,
                Some(&hooks),
                json,
            )
            .await;
            drop(registry);
            if code == EXIT_OK {
                comp.mark_skills_dirty();
            }
        }
        "list" => {
            plugin_list(
                &home,
                None,
                cwd.as_deref(),
                has_flag(&args, "--available"),
                json,
            );
        }
        "info" => {
            let Some(id) = pos.first() else {
                msg_warn("usage: plugin info <plugin>@<marketplace>");
                return;
            };
            plugin_info(&home, None, id, cwd.as_deref(), json);
        }
        "gc" => {
            let confirm = ReplConfirm;
            let teardown = ReplTeardown { comp };
            // #139：回收判据数据源（同 uninstall）——传空集会连坐用户/宿主自有 server。
            let non_plugin = comp.non_plugin_declared_bundle_ids(&home, None);
            let mut registry = reg_arc.write().await;
            let code = plugin_gc(
                &mut registry,
                &home,
                None,
                cwd.as_deref(),
                &non_plugin,
                Some(&teardown),
                Some(&confirm),
                json,
            )
            .await;
            drop(registry);
            if code == EXIT_OK {
                comp.mark_skills_dirty();
            }
        }
        other => msg_warn(&format!("unknown plugin subcommand: {other}")),
    }
}

// ── settings ─────────────────────────────────────────────────────────────────
pub fn dispatch_settings<S: Session>(comp: &Computer<S>, parts: &[&str], flag_path: Option<&Path>) {
    let sub = parts[1].to_lowercase();
    let args: Vec<String> = parts[2..].iter().map(|s| s.to_string()).collect();
    let json = has_flag(&args, "--json");
    let pos = positionals(&args);
    let scope = flag_value(&args, "--scope");
    // #98：project/local scope 锚定进程 cwd（`Computer` 不再持有 workspace）。
    let cwd = std::env::current_dir().ok();

    match sub.as_str() {
        "show" => {
            settings_show(
                None,
                scope.as_deref().unwrap_or("merged"),
                cwd.as_deref(),
                flag_path,
                json,
            );
        }
        "get" => {
            let Some(key) = pos.first() else {
                msg_warn(
                    "usage: settings get <key> [--scope user|project|local|flag|policy|merged]",
                );
                return;
            };
            settings_get(
                None,
                key,
                scope.as_deref().unwrap_or("merged"),
                cwd.as_deref(),
                flag_path,
                json,
            );
        }
        "set" => {
            // key = 首个 positional，value = 其余 positional 重建（含空格 JSON）。基于**剥过 flag 的
            // positional**（非 parts 原序）→ 不论 `--scope` 在 key 前后均正确分离（修 review #5 错位）。
            if pos.len() < 2 {
                msg_warn("usage: settings set <key> <value> [--scope user|project|local]");
                return;
            }
            let key = &pos[0];
            let value = pos[1..].join(" ");
            let code = settings_set(
                None,
                key,
                &value,
                scope.as_deref().unwrap_or("user"),
                cwd.as_deref(),
                json,
            );
            if code == EXIT_OK && is_emit_key(key) {
                comp.mark_skills_dirty();
            }
        }
        "edit" => {
            let code = settings_edit(
                None,
                scope.as_deref().unwrap_or("user"),
                cwd.as_deref(),
                None,
                json,
            );
            // 编辑可能改 enabledPlugins/MCP 字段 → 触发去抖 emit。
            if code == EXIT_OK {
                comp.mark_skills_dirty();
            }
        }
        other => msg_warn(&format!("unknown settings subcommand: {other}")),
    }
}

// ── skill ────────────────────────────────────────────────────────────────────
pub async fn dispatch_skill<S: Session>(comp: &Computer<S>, parts: &[&str]) {
    let sub = parts[1].to_lowercase();
    let args: Vec<String> = parts[2..].iter().map(|s| s.to_string()).collect();
    let json = has_flag(&args, "--json");
    let pos = positionals(&args);
    let registry = comp.skill_registry_arc();
    let registry = registry.read().await;

    match sub.as_str() {
        "list" => {
            let source = flag_value(&args, "--source");
            skill_list(&registry, source.as_deref(), json);
        }
        "info" => {
            let Some(name) = pos.first() else {
                msg_warn("usage: skill info <name>");
                return;
            };
            skill_info(&registry, name, json);
        }
        other => msg_warn(&format!("unknown skill subcommand: {other}")),
    }
}
