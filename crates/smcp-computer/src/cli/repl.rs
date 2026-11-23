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
    marketplace_add, marketplace_info, marketplace_list, marketplace_migrate, marketplace_refresh,
    marketplace_remove, marketplace_set, MarketplaceAddOptions, MarketplaceRemoveOptions,
};
use super::commands::plugin::{
    plugin_disable, plugin_enable_with_paths, plugin_gc, plugin_info, plugin_install, plugin_list,
    plugin_uninstall, PluginInstallOptions,
};
use super::commands::settings::{
    is_emit_key, settings_edit, settings_get, settings_set, settings_show,
};
use super::commands::skill::{skill_info, skill_list};
use super::commands::{flag_value, msg_warn, CliMcpHooks, Confirm, EXIT_OK, EXIT_PARTIAL};
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
                msg_warn("usage: marketplace add <git-url> [--name N (expected manifest name)] [--trust] [--auto-update] [--no-clone]");
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
                    // #267：REPL 的 `--settings`（构造期已归一）参与引用守卫 flag 层。
                    flag_settings_path: comp.normalized_flag_settings_path(),
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
        "migrate" => {
            // 写入路径只在非交互 CLI 提供；REPL 仅 dry-run（或明确报 usage），避免与 clap 约束分叉。
            if has_flag(&args, "--apply") {
                msg_warn(
                    "marketplace migrate --apply is only supported in the non-interactive CLI \
                     (run: smcp-computer marketplace migrate --apply)",
                );
                return;
            }
            let has_rollback_flag = args.iter().any(|a| a == "--rollback");
            let rollback = flag_value(&args, "--rollback");
            if has_rollback_flag && rollback.is_none() {
                msg_warn("usage: marketplace migrate --rollback <timestamp>");
                return;
            }
            if rollback.is_some() || has_flag(&args, "--resume") {
                msg_warn(
                    "marketplace migrate --resume/--rollback are only supported in the \
                     non-interactive CLI",
                );
                return;
            }
            let code = marketplace_migrate(
                &home,
                None,
                false,
                has_flag(&args, "--update-declarations"),
                false,
                None,
                json,
            );
            if code == EXIT_OK || code == EXIT_PARTIAL {
                comp.mark_skills_dirty();
            }
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
            // #286：写后失败对本次新挂做 §4.9.1-2 三档判定的「非用户声明」集（同 disable）——
            // MUST 在取 registry 写锁前由 Computer 供给。
            let non_plugin = comp.non_plugin_declared_bundle_ids(&home, None);
            // #286 🟡4：运行期五层 settings（含 flag/policy）provider 供失败诊断（pre∧post 合并视图），
            // 并以**操作锚点**（cwd / 记录 projectPath）解析 project/local 层。
            let resolved_settings =
                |anchor: Option<&str>| comp.resolved_runtime_settings_at(None, anchor);
            let project_path_hint = cwd.as_deref().and_then(Path::to_str);
            let mut registry = reg_arc.write().await;
            let code = plugin_enable_with_paths(
                &mut registry,
                &home,
                None,
                id,
                project_path_hint,
                project_path_hint,
                Some(&resolved_settings),
                Some(&non_plugin),
                Some(&hooks),
                json,
            )
            .await;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computer::SilentSession;
    use crate::settings::store::load_known_marketplaces;
    use std::path::Path;
    use std::process::Command;

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

    fn build_repo(repo: &Path, name: &str) -> String {
        std::fs::create_dir_all(repo.join(".tfrobot-plugin")).unwrap();
        std::fs::write(
            repo.join(".tfrobot-plugin/marketplace.json"),
            serde_json::json!({
                "name": name,
                "plugins": [{"name": "alpha", "source": "./plugins/alpha"}]
            })
            .to_string(),
        )
        .unwrap();
        let skill = repo.join("plugins/alpha/skills/code-review");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: code-review\ndescription: repl flag guard\n---\nbody",
        )
        .unwrap();
        git(&["init", "-q"], repo);
        git(&["add", "-A"], repo);
        git(&["commit", "-qm", "init"], repo);
        format!("file://{}", repo.display())
    }

    /// #267 审查 🟡7：REPL `marketplace add` 的 `--settings`（flag 层）引用守卫接线。
    #[tokio::test]
    async fn repl_marketplace_add_rejects_flag_layer_reference_267() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let mp = format!("repl-flag-guard-267-{}", std::process::id());
        let url = build_repo(&tmp.path().join("repo"), &mp);
        let flag = tmp.path().join("flag-settings.json");
        let mut enabled = serde_json::Map::new();
        enabled.insert(format!("alpha@{mp}"), serde_json::Value::Bool(true));
        let mut flag_settings = serde_json::Map::new();
        flag_settings.insert(
            "enabledPlugins".to_string(),
            serde_json::Value::Object(enabled),
        );
        std::fs::write(&flag, serde_json::Value::Object(flag_settings).to_string()).unwrap();
        let comp = Computer::new(
            "c267repl",
            SilentSession::new("s"),
            None,
            None,
            false,
            false,
        )
        .with_skill_home(home.clone())
        .with_flag_settings_path(&flag)
        .with_blob_cache_root(tmp.path().join("blob"));

        dispatch_marketplace(&comp, &["marketplace", "add", &url, "--trust"]).await;

        assert!(
            load_known_marketplaces(Some(&home), None)
                .account
                .marketplaces
                .is_empty(),
            "REPL add 必须消费 --settings 的引用（flag 层）"
        );
        assert!(
            !home.join("marketplace").join(&mp).exists(),
            "拒绝不得留下 clone"
        );
    }
}
