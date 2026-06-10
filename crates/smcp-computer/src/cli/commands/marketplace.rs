/*!
* 文件名: marketplace.rs
* 作者: JQQ
* 创建日期: 2026/06/10
* 最后修改日期: 2026/06/10
* 版权: 2023 JQQ. All rights reserved.
* 依赖: serde_json, console
* 描述: `marketplace` 命令 handler（add / list / info / remove / refresh / set）
*       Marketplace command handlers.
*
* 对标 Python `a2c_smcp/computer/cli/commands/marketplace.py`：handler 取显式资源（`registry` / `home`
* / `env`）+ flags，返回退出码（0 成功 / 1 用户错 / 2 网络错），包裹既有后端——克隆/对账
* [`stage_marketplace_skills`]、物化读写 [`load_known_marketplaces`] 等、卸载级联 [`uninstall_plugin`] +
* [`prune_marketplaces`]。Trust（§10.5）首见经 [`Confirm`] 回调（REPL=session y/N；Typer 非交互无 confirm
* 且无 `--trust` → 退出码 1），批准后持久化到 user scope `trustedMarketplaces`（CC 模型，§16：
* `known_marketplaces.json` **不**带 trusted 字段）。
*/

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::{json, Map, Value};

use super::{
    file_store, msg_dim, msg_err, ok_msg, print_json, Confirm, EXIT_NETWORK_ERROR, EXIT_OK,
    EXIT_USER_ERROR,
};
use crate::settings::installer::{uninstall_plugin, McpInstallHooks, UninstallOptions};
use crate::settings::reconciler::{prune_marketplaces, SkillGovernanceStore};
use crate::settings::schema::FIELD_TRUSTED_MARKETPLACES;
use crate::settings::scope::{
    apply_write, load_settings_file, user_settings_path, EnvMap, WriteValue,
};
use crate::settings::store::{
    atomic_write_settings_json, load_installed_plugins, load_known_marketplaces,
    update_known_marketplaces, with_settings_lock, SettingsStoreError,
};
use crate::settings::{
    is_valid_git_url, is_valid_marketplace_name, KnownMarketplaceEntry, KnownMarketplaces,
    SettingsScope,
};
use crate::skills::{
    normalize_repo_shorthand, stage_marketplace_skills, MarketplaceStageOptions, SkillRegistry,
    GITHUB_HOST,
};

// ── 输出辅助 / output helpers ────────────────────────────────────────────────
fn err(msg: &str, json_output: bool, code: i32) -> i32 {
    if json_output {
        print_json(&json!({ "error": msg }));
    } else {
        msg_err(msg);
    }
    code
}

fn map_store_err(e: SettingsStoreError) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

// ── 读视图辅助 / read-view helpers ───────────────────────────────────────────
fn load_mps(home: &Path, env: Option<&EnvMap>) -> KnownMarketplaces {
    load_known_marketplaces(Some(home), env).account
}

fn source_url(rec: &KnownMarketplaceEntry) -> Option<&str> {
    rec.source.get("url").and_then(Value::as_str)
}

fn read_string_array(map: &Map<String, Value>, key: &str) -> Vec<String> {
    map.get(key)
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

// ── URL / name 归一（CLI 层补齐：Python 在 marketplace.py 本地实现）/ normalization ──
/// 归一 marketplace git URL：完整 URL 原样；裸 `owner/repo` 简写按 GitHub 糖展开 / normalize a git URL。
pub fn normalize_marketplace_url(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if is_valid_git_url(raw) {
        return Some(raw.to_string());
    }
    normalize_repo_shorthand(raw, GITHUB_HOST).ok()
}

/// 从 git URL 末段派生严格-kebab marketplace 名（去 `.git`、非字母数字折叠为 `-`）/ derive a kebab name。
pub fn default_marketplace_name(url: &str) -> Option<String> {
    let tail = url.trim_end_matches('/');
    let seg = tail.rsplit(['/', ':']).next().unwrap_or(tail);
    let seg = seg.strip_suffix(".git").unwrap_or(seg);

    // `re.sub(r"[^a-z0-9]+", "-", seg.lower()).strip("-")`：小写后非 [a-z0-9] 连续段折叠为单 `-`。
    let mut slug = String::new();
    let mut prev_dash = false;
    for ch in seg.to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            prev_dash = false;
        } else if !prev_dash {
            slug.push('-');
            prev_dash = true;
        }
    }
    let slug = slug.trim_matches('-').to_string();
    (!slug.is_empty() && is_valid_marketplace_name(&slug)).then_some(slug)
}

// ── trust 持久化（user scope settings.json）/ trust persistence ────────────────
// 键空间 = marketplace **name**（设计 §5.3.1）；prompt 仍展示 url 供用户判断来源。
fn load_trusted(env: Option<&EnvMap>) -> Vec<String> {
    let (existing, _errors) = load_settings_file(&user_settings_path(env), SettingsScope::User);
    read_string_array(&existing, FIELD_TRUSTED_MARKETPLACES)
}

fn record_trust(name: &str, env: Option<&EnvMap>) -> std::io::Result<()> {
    let path = user_settings_path(env);
    with_settings_lock(&path, || -> std::io::Result<()> {
        let (existing, _errors) = load_settings_file(&path, SettingsScope::User);
        let mut current = read_string_array(&existing, FIELD_TRUSTED_MARKETPLACES);
        if !current.iter().any(|n| n == name) {
            current.push(name.to_string());
        }
        let updates = BTreeMap::from([(
            FIELD_TRUSTED_MARKETPLACES.to_string(),
            WriteValue::Set(json!(current)),
        )]);
        atomic_write_settings_json(&path, &apply_write(&existing, &updates))
    })
    .map_err(map_store_err)?
}

fn revoke_trust(name: &str, env: Option<&EnvMap>) -> std::io::Result<()> {
    let path = user_settings_path(env);
    with_settings_lock(&path, || -> std::io::Result<()> {
        let (existing, _errors) = load_settings_file(&path, SettingsScope::User);
        let current: Vec<String> = read_string_array(&existing, FIELD_TRUSTED_MARKETPLACES)
            .into_iter()
            .filter(|n| n != name)
            .collect();
        let updates = BTreeMap::from([(
            FIELD_TRUSTED_MARKETPLACES.to_string(),
            WriteValue::Set(json!(current)),
        )]);
        atomic_write_settings_json(&path, &apply_write(&existing, &updates))
    })
    .map_err(map_store_err)?
}

// ── handlers ─────────────────────────────────────────────────────────────────
/// [`marketplace_add`] 选项 / add options。
#[derive(Default)]
pub struct MarketplaceAddOptions<'a> {
    /// 显式 marketplace 名（缺省经 [`default_marketplace_name`] 派生）/ explicit name。
    pub name: Option<&'a str>,
    /// 非交互信任旗（无 confirm 时**强制**）/ trust flag。
    pub trust: bool,
    /// 物化记录的 autoUpdate / autoUpdate flag。
    pub auto_update: bool,
    /// 仅注册意图、不 clone（`--no-clone`，§4.2 debug 用）/ register intent only。
    pub no_clone: bool,
    /// 首见信任确认回调（REPL session y/N；Typer 非交互 `None`）/ first-trust confirm callback。
    pub confirm: Option<&'a dyn Confirm>,
    /// 结构化输出 / JSON output。
    pub json_output: bool,
}

/// 添加新 marketplace（首次 trust y/N，默认 eager clone）/ add a marketplace。
pub async fn marketplace_add(
    registry: &mut SkillRegistry,
    home: &Path,
    env: Option<&EnvMap>,
    git_url: &str,
    opts: MarketplaceAddOptions<'_>,
) -> i32 {
    let json_output = opts.json_output;
    let Some(url) = normalize_marketplace_url(git_url) else {
        return err(
            &format!("not a well-formed git url or owner/repo shorthand: {git_url:?}"),
            json_output,
            EXIT_USER_ERROR,
        );
    };
    let mp_name = match opts
        .name
        .map(str::to_string)
        .or_else(|| default_marketplace_name(&url))
    {
        Some(n) if is_valid_marketplace_name(&n) => n,
        _ => {
            return err(
                &format!("cannot derive a valid marketplace name from {git_url:?}; pass --name"),
                json_output,
                EXIT_USER_ERROR,
            )
        }
    };

    if load_mps(home, env).marketplaces.contains_key(&mp_name) {
        return err(
            &format!("marketplace name conflict: {mp_name:?} already exists"),
            json_output,
            EXIT_USER_ERROR,
        );
    }

    // trust 门（§10.5/§11，#95 收紧）：非交互（confirm None）→ `--trust` 无条件强制（陈旧 user-scope trust
    // **不豁免**）；交互（confirm Some，REPL）→ 已 trusted 的 name 跳过重复 prompt（仅 desync 态可达），否则弹 confirm。
    if !opts.trust {
        match opts.confirm {
            None => {
                return err(
                    &format!(
                        "untrusted marketplace {mp_name:?} ({url}); pass --trust to confirm non-interactively"
                    ),
                    json_output,
                    EXIT_USER_ERROR,
                )
            }
            Some(confirm) => {
                if !load_trusted(env).iter().any(|n| n == &mp_name)
                    && !confirm.confirm(&url).await
                {
                    return err("aborted by user (untrusted)", json_output, EXIT_USER_ERROR);
                }
            }
        }
    }
    if let Err(e) = record_trust(&mp_name, env) {
        return err(
            &format!("failed to record trust for {mp_name:?}: {e}"),
            json_output,
            EXIT_USER_ERROR,
        );
    }

    if opts.no_clone {
        // 仅注册意图（不推荐，§4.2 debug 用）：记 known_marketplaces 但不 clone/stage。
        let name_for_write = mp_name.clone();
        let url_for_write = url.clone();
        let auto = opts.auto_update;
        let res = update_known_marketplaces(
            move |file| {
                let mut extra = Map::new();
                extra.insert("installLocation".to_string(), json!(""));
                extra.insert("autoUpdate".to_string(), json!(auto));
                file.account.marketplaces.insert(
                    name_for_write,
                    KnownMarketplaceEntry {
                        source: json!({ "type": "git", "url": url_for_write }),
                        extra,
                    },
                );
            },
            Some(home),
            env,
        );
        if let Err(e) = res {
            return err(
                &format!("failed to record marketplace intent: {e}"),
                json_output,
                EXIT_USER_ERROR,
            );
        }
        return ok_msg(&format!(
            "registered marketplace intent {mp_name:?} (no clone)"
        ));
    }

    let source = json!({ "type": "git", "url": url });
    let store = file_store(home, env);
    let registered = stage_marketplace_skills(
        &mp_name,
        &source,
        registry,
        home,
        MarketplaceStageOptions {
            plugin_filter: None,
            auto_update: opts.auto_update,
            refresh: false,
            timeout: None,
            env,
            recorder: Some(store.as_recorder()),
        },
    )
    .await;

    // 成功判定：stage 失败降级（不抛、返回空 + 不写 known_marketplaces）；据物化记录是否落盘判定。
    if !load_mps(home, env).marketplaces.contains_key(&mp_name) {
        return err(
            &format!("clone/refresh failed for {url:?} (see logs)"),
            json_output,
            EXIT_NETWORK_ERROR,
        );
    }

    if json_output {
        print_json(&json!({ "added": mp_name, "url": url, "skills": registered.len() }));
        return EXIT_OK;
    }
    ok_msg(&format!(
        "added {mp_name:?} — cloned, {} skill(s) found",
        registered.len()
    ))
}

/// 列出所有已知 marketplace（trusted / clone 状态 / 上次刷新 / auto_update）/ list known marketplaces。
pub fn marketplace_list(home: &Path, env: Option<&EnvMap>, json_output: bool) -> i32 {
    let mps = load_mps(home, env);
    let trusted = load_trusted(env);
    let is_trusted = |nm: &str| trusted.iter().any(|t| t == nm);

    if json_output {
        let rows: Vec<Value> = mps
            .marketplaces
            .iter()
            .map(|(nm, rec)| {
                json!({
                    "name": nm,
                    "url": source_url(rec),
                    "trusted": is_trusted(nm),
                    "autoUpdate": rec.extra.get("autoUpdate").and_then(Value::as_bool).unwrap_or(false),
                    "lastUpdated": rec.extra.get("lastUpdated").cloned().unwrap_or(Value::Null),
                    "commitSha": rec.extra.get("commitSha").cloned().unwrap_or(Value::Null),
                })
            })
            .collect();
        print_json(&json!(rows));
        return EXIT_OK;
    }

    if mps.marketplaces.is_empty() {
        msg_dim("No marketplaces. Add one: marketplace add <git-url>");
        return EXIT_OK;
    }
    println!("Marketplaces:");
    for (nm, rec) in &mps.marketplaces {
        let url = source_url(rec).unwrap_or("");
        let sha = rec
            .extra
            .get("commitSha")
            .and_then(Value::as_str)
            .unwrap_or("");
        let sha_disp = if sha.is_empty() {
            "—".to_string()
        } else {
            sha.chars().take(10).collect()
        };
        let auto = if rec
            .extra
            .get("autoUpdate")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            "on"
        } else {
            "off"
        };
        println!(
            "  {nm}  ·  {url}  ·  trusted={}  ·  auto-update={auto}  ·  commit={sha_disp}",
            if is_trusted(nm) { "✓" } else { "—" },
        );
    }
    EXIT_OK
}

/// marketplace 详情：URL / clone 路径 / commit / plugins[] / auto_update / trusted。
pub fn marketplace_info(home: &Path, env: Option<&EnvMap>, name: &str, json_output: bool) -> i32 {
    let mps = load_mps(home, env);
    let Some(rec) = mps.marketplaces.get(name) else {
        return err(
            &format!("unknown marketplace: {name:?}"),
            json_output,
            EXIT_USER_ERROR,
        );
    };

    let installed = load_installed_plugins(Some(home), env).account;
    let suffix = format!("@{name}");
    let plugins: Vec<String> = installed
        .plugins
        .keys()
        .filter(|pid| pid.ends_with(&suffix))
        .cloned()
        .collect();
    let trusted = load_trusted(env).iter().any(|t| t == name);

    let info = json!({
        "name": name,
        "url": source_url(rec),
        "installLocation": rec.extra.get("installLocation").cloned().unwrap_or(Value::Null),
        "commitSha": rec.extra.get("commitSha").cloned().unwrap_or(Value::Null),
        "autoUpdate": rec.extra.get("autoUpdate").and_then(Value::as_bool).unwrap_or(false),
        "trusted": trusted,
        "lastUpdated": rec.extra.get("lastUpdated").cloned().unwrap_or(Value::Null),
        "installedPlugins": plugins,
    });
    if json_output {
        print_json(&info);
        return EXIT_OK;
    }
    println!("Marketplace · {name}");
    for key in [
        "url",
        "installLocation",
        "commitSha",
        "autoUpdate",
        "trusted",
        "lastUpdated",
    ] {
        println!("  {key}: {}", info[key]);
    }
    println!(
        "  installedPlugins: {}",
        if plugins.is_empty() {
            "—".to_string()
        } else {
            plugins.join(", ")
        }
    );
    EXIT_OK
}

/// [`marketplace_remove`] 选项 / remove options。
#[derive(Default)]
pub struct MarketplaceRemoveOptions<'a> {
    /// 仅 prune clone、保留 installed plugin 记录（=孤儿）/ keep installed plugin records as orphans。
    pub keep_plugins: bool,
    /// 移除确认回调（接收 name）/ removal confirm callback (receives name)。
    pub confirm: Option<&'a dyn Confirm>,
    /// 级联卸载所需 MCP 注入回调（提供 `remove_server`）/ MCP hooks for cascade uninstall。
    pub hooks: Option<&'a dyn McpInstallHooks>,
    /// 结构化输出 / JSON output。
    pub json_output: bool,
}

/// 移除 marketplace。默认级联卸载其下 installed plugin（含 MCP server）；`keep_plugins` 仅 prune clone。
pub async fn marketplace_remove(
    registry: &mut SkillRegistry,
    home: &Path,
    env: Option<&EnvMap>,
    name: &str,
    opts: MarketplaceRemoveOptions<'_>,
) -> i32 {
    let json_output = opts.json_output;
    if !load_mps(home, env).marketplaces.contains_key(name) {
        return err(
            &format!("unknown marketplace: {name:?}"),
            json_output,
            EXIT_USER_ERROR,
        );
    }
    if let Some(confirm) = opts.confirm {
        if !confirm.confirm(name).await {
            return err("aborted by user", json_output, EXIT_USER_ERROR);
        }
    }

    let mut uninstalled: Vec<String> = Vec::new();
    if !opts.keep_plugins {
        let suffix = format!("@{name}");
        let installed = load_installed_plugins(Some(home), env).account;
        let victims: Vec<String> = installed
            .plugins
            .keys()
            .filter(|pid| pid.ends_with(&suffix))
            .cloned()
            .collect();
        for pid in &victims {
            let res = uninstall_plugin(
                pid,
                registry,
                home,
                UninstallOptions {
                    scope: None,
                    keep_servers: false,
                    env,
                },
                opts.hooks,
            )
            .await;
            if matches!(res, Ok(true)) {
                uninstalled.push(pid.clone());
            }
        }
    }

    let store = file_store(home, env);
    let pruned = prune_marketplaces(&[name.to_string()], registry, home, &store);
    // 撤销信任，避免 trustedMarketplaces 只增不减（user scope）。best-effort：对标 Python（marketplace.py:309
    // 无错误处理）——写失败仅告警、**不**把已完成的卸载/prune 级联翻成用户错（destructive 工作已落地）。
    if let Err(e) = revoke_trust(name, env) {
        msg_dim(&format!("warning: failed to revoke trust for {name:?}: {e}"));
    }

    if json_output {
        print_json(&json!({
            "removed": name,
            "pruned": pruned,
            "uninstalledPlugins": uninstalled,
            "keptPlugins": opts.keep_plugins,
        }));
        return EXIT_OK;
    }
    let detail = if !uninstalled.is_empty() {
        format!(" (uninstalled {} plugin(s))", uninstalled.len())
    } else if opts.keep_plugins {
        " (plugins kept as orphans)".to_string()
    } else {
        String::new()
    };
    ok_msg(&format!("removed marketplace {name:?}{detail}"))
}

/// `git pull` 失败则全量重 clone；与缓存 plugin 集合对账 + 失败汇总（§10.4）/ refresh marketplaces。
pub async fn marketplace_refresh(
    registry: &mut SkillRegistry,
    home: &Path,
    env: Option<&EnvMap>,
    target: &str,
    json_output: bool,
) -> i32 {
    let mps = load_mps(home, env);
    let names: Vec<String> = if target == "all" {
        mps.marketplaces.keys().cloned().collect()
    } else {
        vec![target.to_string()]
    };

    let mut rows: Vec<Value> = Vec::new();
    for nm in &names {
        let Some(rec) = mps.marketplaces.get(nm) else {
            rows.push(json!({ "name": nm, "status": "missing" }));
            continue;
        };
        let before = rec.extra.get("commitSha").cloned();
        let source = rec.source.clone();
        let auto = rec
            .extra
            .get("autoUpdate")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let store = file_store(home, env);
        let registered = stage_marketplace_skills(
            nm,
            &source,
            registry,
            home,
            MarketplaceStageOptions {
                plugin_filter: None,
                auto_update: auto,
                refresh: true,
                timeout: None,
                env,
                recorder: Some(store.as_recorder()),
            },
        )
        .await;
        let after_sha = load_mps(home, env)
            .marketplaces
            .get(nm)
            .and_then(|r| r.extra.get("commitSha").cloned());
        let status = if after_sha != before {
            "updated"
        } else {
            "unchanged"
        };
        rows.push(json!({ "name": nm, "status": status, "skills": registered.len() }));
    }

    if json_output {
        print_json(&json!(rows));
        return EXIT_OK;
    }
    for r in &rows {
        let mark = match r["status"].as_str() {
            Some("updated") => "✓",
            Some("unchanged") => "·",
            _ => "✗",
        };
        println!(
            "  {mark} {}  ({})",
            r["name"].as_str().unwrap_or(""),
            r["status"].as_str().unwrap_or("")
        );
    }
    let updated = rows.iter().filter(|r| r["status"] == "updated").count();
    let failed = rows.iter().filter(|r| r["status"] == "missing").count();
    msg_dim(&format!(
        "{} marketplace(s) · {updated} updated · {failed} failed",
        rows.len()
    ));
    EXIT_OK
}

/// 设置 per-source 旗，目前仅 `auto-update=<bool>` / set a per-source flag (auto-update only for v0.2.1)。
pub fn marketplace_set(
    home: &Path,
    env: Option<&EnvMap>,
    name: &str,
    key: &str,
    value: &str,
    json_output: bool,
) -> i32 {
    if key != "auto-update" {
        return err(
            &format!("unknown key {key:?} (only 'auto-update' supported)"),
            json_output,
            EXIT_USER_ERROR,
        );
    }
    let val = matches!(
        value.trim().to_lowercase().as_str(),
        "true" | "1" | "yes" | "on"
    );
    if !load_mps(home, env).marketplaces.contains_key(name) {
        return err(
            &format!("unknown marketplace: {name:?}"),
            json_output,
            EXIT_USER_ERROR,
        );
    }

    let name_for_write = name.to_string();
    let res = update_known_marketplaces(
        move |file| {
            if let Some(entry) = file.account.marketplaces.get_mut(&name_for_write) {
                entry.extra.insert("autoUpdate".to_string(), json!(val));
            }
        },
        Some(home),
        env,
    );
    if let Err(e) = res {
        return err(
            &format!("failed to update marketplace: {e}"),
            json_output,
            EXIT_USER_ERROR,
        );
    }
    if json_output {
        print_json(&json!({ "name": name, "autoUpdate": val }));
        return EXIT_OK;
    }
    ok_msg(&format!("{name:?} auto-update set to {val}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::commands::test_env;
    use async_trait::async_trait;
    use tempfile::tempdir;

    /// 测试用确认回调：恒返回固定答案，记录是否被调用 / mock Confirm with a fixed answer。
    struct MockConfirm {
        answer: bool,
        called: std::sync::atomic::AtomicBool,
    }

    impl MockConfirm {
        fn new(answer: bool) -> Self {
            Self {
                answer,
                called: std::sync::atomic::AtomicBool::new(false),
            }
        }
        fn was_called(&self) -> bool {
            self.called.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl Confirm for MockConfirm {
        async fn confirm(&self, _target: &str) -> bool {
            self.called.store(true, std::sync::atomic::Ordering::SeqCst);
            self.answer
        }
    }

    #[test]
    fn normalize_url_passthrough_and_shorthand() {
        assert_eq!(
            normalize_marketplace_url("https://github.com/acme/skills.git"),
            Some("https://github.com/acme/skills.git".to_string())
        );
        // owner/repo 简写 → GitHub 糖。
        assert_eq!(
            normalize_marketplace_url("acme/skills"),
            Some("https://github.com/acme/skills.git".to_string())
        );
        assert_eq!(normalize_marketplace_url("not a url"), None);
    }

    #[test]
    fn default_name_derivation() {
        assert_eq!(
            default_marketplace_name("https://github.com/acme/My_Skills.git"),
            Some("my-skills".to_string())
        );
        assert_eq!(
            default_marketplace_name("git@github.com:acme/cool-repo.git"),
            Some("cool-repo".to_string())
        );
    }

    #[tokio::test]
    async fn add_non_interactive_requires_trust() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let env = test_env(home);
        let mut registry = SkillRegistry::new();
        // 非交互（confirm None）且无 --trust → 退出码 1，且不落 known_marketplaces。
        let code = marketplace_add(
            &mut registry,
            home,
            Some(&env),
            "acme/skills",
            MarketplaceAddOptions {
                json_output: true,
                ..Default::default()
            },
        )
        .await;
        assert_eq!(code, EXIT_USER_ERROR);
        assert!(load_mps(home, Some(&env)).marketplaces.is_empty());
        // 也未记录 trust。
        assert!(load_trusted(Some(&env)).is_empty());
    }

    #[tokio::test]
    async fn add_with_trust_records_trust_name() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let env = test_env(home);
        let mut registry = SkillRegistry::new();
        // 带 --trust + --no-clone（避免真实 git clone）：注册意图 + 持久化 trust name。
        let code = marketplace_add(
            &mut registry,
            home,
            Some(&env),
            "acme/skills",
            MarketplaceAddOptions {
                trust: true,
                no_clone: true,
                json_output: true,
                ..Default::default()
            },
        )
        .await;
        assert_eq!(code, EXIT_OK);
        // trust 以 name（非 url）落 user settings。
        assert_eq!(load_trusted(Some(&env)), vec!["skills".to_string()]);
        // known_marketplaces 记录意图。
        assert!(load_mps(home, Some(&env))
            .marketplaces
            .contains_key("skills"));
    }

    #[tokio::test]
    async fn add_duplicate_name_conflicts() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let env = test_env(home);
        let mut registry = SkillRegistry::new();
        let base = || MarketplaceAddOptions {
            trust: true,
            no_clone: true,
            json_output: true,
            ..Default::default()
        };
        assert_eq!(
            marketplace_add(&mut registry, home, Some(&env), "acme/skills", base()).await,
            EXIT_OK
        );
        // 同名再加 → 冲突退出码 1。
        assert_eq!(
            marketplace_add(&mut registry, home, Some(&env), "other/skills", base()).await,
            EXIT_USER_ERROR
        );
    }

    #[test]
    fn set_unknown_key_and_unknown_marketplace() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let env = test_env(home);
        // 未知 key → 1。
        assert_eq!(
            marketplace_set(home, Some(&env), "x", "bad-key", "true", true),
            EXIT_USER_ERROR
        );
        // 未知 marketplace → 1。
        assert_eq!(
            marketplace_set(home, Some(&env), "nope", "auto-update", "true", true),
            EXIT_USER_ERROR
        );
    }

    #[test]
    fn list_empty_is_ok() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let env = test_env(home);
        assert_eq!(marketplace_list(home, Some(&env), true), EXIT_OK);
    }

    #[tokio::test]
    async fn remove_unknown_is_user_error() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let env = test_env(home);
        let mut registry = SkillRegistry::new();
        let code = marketplace_remove(
            &mut registry,
            home,
            Some(&env),
            "ghost",
            MarketplaceRemoveOptions {
                json_output: true,
                ..Default::default()
            },
        )
        .await;
        assert_eq!(code, EXIT_USER_ERROR);
    }

    #[tokio::test]
    async fn remove_revokes_trust() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let env = test_env(home);
        let mut registry = SkillRegistry::new();
        // 先 no-clone 注册 + trust。
        marketplace_add(
            &mut registry,
            home,
            Some(&env),
            "acme/skills",
            MarketplaceAddOptions {
                trust: true,
                no_clone: true,
                json_output: true,
                ..Default::default()
            },
        )
        .await;
        assert_eq!(load_trusted(Some(&env)), vec!["skills".to_string()]);
        // remove → 撤销 trust + prune。
        let code = marketplace_remove(
            &mut registry,
            home,
            Some(&env),
            "skills",
            MarketplaceRemoveOptions {
                keep_plugins: true,
                json_output: true,
                ..Default::default()
            },
        )
        .await;
        assert_eq!(code, EXIT_OK);
        assert!(load_trusted(Some(&env)).is_empty());
    }

    #[tokio::test]
    async fn add_stale_trust_does_not_exempt_non_interactive() {
        // 核心治理不变量（§10.5/§11）：陈旧 user-scope trust **不**豁免一次全新非交互 add——
        // 即便 trustedMarketplaces 已含该 name，confirm=None 且无 --trust 仍须退出码 1。
        let dir = tempdir().unwrap();
        let home = dir.path();
        let env = test_env(home);
        record_trust("skills", Some(&env)).unwrap();
        assert_eq!(load_trusted(Some(&env)), vec!["skills".to_string()]);

        let mut registry = SkillRegistry::new();
        let code = marketplace_add(
            &mut registry,
            home,
            Some(&env),
            "acme/skills",
            MarketplaceAddOptions {
                json_output: true,
                ..Default::default()
            },
        )
        .await;
        assert_eq!(code, EXIT_USER_ERROR);
        assert!(load_mps(home, Some(&env)).marketplaces.is_empty());
    }

    #[tokio::test]
    async fn add_interactive_confirm_declined_aborts() {
        // 交互态：未 trusted 的新源，confirm 返回 false → 中止（退出码 1），且 confirm **被调用**。
        let dir = tempdir().unwrap();
        let home = dir.path();
        let env = test_env(home);
        let mut registry = SkillRegistry::new();
        let confirm = MockConfirm::new(false);
        let code = marketplace_add(
            &mut registry,
            home,
            Some(&env),
            "acme/skills",
            MarketplaceAddOptions {
                confirm: Some(&confirm),
                json_output: true,
                ..Default::default()
            },
        )
        .await;
        assert_eq!(code, EXIT_USER_ERROR);
        assert!(confirm.was_called());
        assert!(load_mps(home, Some(&env)).marketplaces.is_empty());
    }

    #[tokio::test]
    async fn add_interactive_pretrusted_skips_prompt() {
        // 回归守卫（Python test_add_pretrusted_name_interactive_skips_prompt）：交互态下已 trusted 的 name
        // 跳过重复 confirm（仅 desync 态可达）——confirm 即便返回 false 也**不被调用**，add 继续成功。
        let dir = tempdir().unwrap();
        let home = dir.path();
        let env = test_env(home);
        record_trust("skills", Some(&env)).unwrap();

        let mut registry = SkillRegistry::new();
        let confirm = MockConfirm::new(false);
        let code = marketplace_add(
            &mut registry,
            home,
            Some(&env),
            "acme/skills",
            MarketplaceAddOptions {
                no_clone: true,
                confirm: Some(&confirm),
                json_output: true,
                ..Default::default()
            },
        )
        .await;
        assert_eq!(code, EXIT_OK);
        assert!(!confirm.was_called());
        assert!(load_mps(home, Some(&env)).marketplaces.contains_key("skills"));
    }
}
