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
* / `env`）+ flags，返回退出码（0 成功 / 1 用户错 / 2 网络错）。
*
* **#94 后**：add/refresh/remove 的 stage+prune 高层编排已抬到非 CLI 的 [`crate::settings::lifecycle`]，
* handler **薄化**为：信任门（user-scope）+ 结构化 [`GovernanceError`] / Outcome → 退出码映射；list/info/set
* 仍直接读写物化账本（[`load_known_marketplaces`] 等）。Trust（§10.5）首见经 [`Confirm`] 回调（REPL=session
* y/N；Typer 非交互无 confirm 且无 `--trust` → 退出码 1），批准后持久化到 user scope `trustedMarketplaces`
* （CC 模型，§16：`known_marketplaces.json` **不**带 trusted 字段，故 trust **不**属 `skill_home` 治理边界）。
*/

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::{json, Map, Value};

use super::{
    err_flat as err, msg_dim, ok_msg, print_json, Confirm, EXIT_NETWORK_ERROR, EXIT_OK,
    EXIT_USER_ERROR,
};
use crate::settings::installer::McpInstallHooks;
use crate::settings::lifecycle::{
    marketplace_name_taken, refresh_marketplaces, register_or_stage_marketplace,
    remove_marketplace, resolve_marketplace_identity, AddMarketplaceParams, GovernanceError,
    MarketplaceRefreshRow, MarketplaceRemoveOutcome, RefreshStatus, RemoveMarketplaceParams,
};
use crate::settings::redaction::{
    git_url_for_display, redact_git_urls_in_text, untrusted_name_for_display,
};
use crate::settings::schema::FIELD_TRUSTED_MARKETPLACES;
use crate::settings::scope::{
    apply_write, load_settings_file, user_settings_path, EnvMap, WriteValue,
};
use crate::settings::store::{
    atomic_write_settings_json, load_installed_plugins, load_known_marketplaces,
    update_known_marketplaces, with_settings_lock, SettingsStoreError,
};
use crate::settings::{KnownMarketplaceEntry, KnownMarketplaces, SettingsScope};
use crate::skills::SkillRegistry;

// `marketplace add/refresh/remove` 的 stage+prune 高层编排已抬到非 CLI 的 [`crate::settings::lifecycle`]（#94）；
// URL 归一 / 名派生纯函数随之迁移，此处再导出以保持既有引用路径 / re-export relocated pure helpers。
pub use crate::settings::lifecycle::{default_marketplace_name, normalize_marketplace_url};

// ── 输出辅助 / output helpers ────────────────────────────────────────────────
fn map_store_err(e: SettingsStoreError) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

/// 身份解析错误的 CLI 文案。保持纯函数，使终端文本与 JSON `error` 字段共用同一安全边界。
fn identity_error_message(error: &GovernanceError) -> String {
    match error {
        GovernanceError::InvalidUrl(source) => {
            format!("not a well-formed git url or owner/repo shorthand: {source:?}")
        }
        GovernanceError::InvalidName(name) if name.is_empty() => {
            "cannot derive a valid marketplace name; pass --name with a lowercase kebab-case name"
                .to_string()
        }
        GovernanceError::InvalidName(name) => format!(
            "invalid marketplace name {name:?}; expected 1-64 lowercase kebab-case characters"
        ),
        other => other.to_string(),
    }
}

fn unknown_marketplace_message(name: &str) -> String {
    format!(
        "unknown marketplace: {:?}",
        untrusted_name_for_display(name)
    )
}

fn unknown_key_message(key: &str) -> String {
    format!(
        "unknown key {:?} (only 'auto-update' supported)",
        untrusted_name_for_display(key)
    )
}

fn redact_public_value(value: &Value) -> Value {
    match value {
        Value::String(text) => Value::String(redact_git_urls_in_text(text)),
        Value::Array(items) => Value::Array(items.iter().map(redact_public_value).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| (redact_git_urls_in_text(key), redact_public_value(value)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn display_plugin_ids(plugin_ids: impl IntoIterator<Item = String>) -> Vec<String> {
    plugin_ids
        .into_iter()
        .map(|plugin_id| untrusted_name_for_display(&plugin_id))
        .collect()
}

fn marketplace_list_row(name: &str, rec: &KnownMarketplaceEntry, trusted: bool) -> Value {
    json!({
        "name": untrusted_name_for_display(name),
        "url": display_source_url(rec),
        "trusted": trusted,
        "autoUpdate": rec.extra.get("autoUpdate").and_then(Value::as_bool).unwrap_or(false),
        "lastUpdated": rec.extra.get("lastUpdated").map_or(Value::Null, redact_public_value),
        "commitSha": rec.extra.get("commitSha").map_or(Value::Null, redact_public_value),
    })
}

/// `marketplace remove` 的 JSON 输出 / remove JSON output。
///
/// 抽成纯函数：键名（`removed`/`pruned`/`uninstalledPlugins`/`keptPlugins`）是**跨 SDK 兼容契约**，
/// 经独立单测锁定 camelCase 防漂移（handler 内联 `json!` 无法在不捕获 stdout 下断言）。
fn remove_outcome_json(outcome: &MarketplaceRemoveOutcome) -> Value {
    json!({
        "removed": untrusted_name_for_display(&outcome.name),
        "pruned": outcome.pruned.iter().map(|name| untrusted_name_for_display(name)).collect::<Vec<_>>(),
        "uninstalledPlugins": display_plugin_ids(outcome.uninstalled_plugins.clone()),
        "keptPlugins": outcome.kept_plugins,
    })
}

/// `marketplace refresh` 的逐行 JSON / refresh per-row JSON。
///
/// 同上键名契约（`name`/`status`/`skills`；`missing` 行省 `skills`）。
fn refresh_rows_json(rows: &[MarketplaceRefreshRow]) -> Vec<Value> {
    rows.iter()
        .map(|r| match r.status {
            RefreshStatus::Missing => {
                json!({ "name": untrusted_name_for_display(&r.name), "status": "missing" })
            }
            _ => json!({
                "name": untrusted_name_for_display(&r.name),
                "status": r.status.as_str(),
                "skills": r.skills
            }),
        })
        .collect()
}

// ── 读视图辅助 / read-view helpers ───────────────────────────────────────────
fn load_mps(home: &Path, env: Option<&EnvMap>) -> KnownMarketplaces {
    load_known_marketplaces(Some(home), env).account
}

fn source_url(rec: &KnownMarketplaceEntry) -> Option<&str> {
    rec.source.get("url").and_then(Value::as_str)
}

/// 账本 URL 的 CLI 安全展示值；JSON 与文本输出必须共用，避免某种输出模式绕过脱敏。
fn display_source_url(rec: &KnownMarketplaceEntry) -> Option<String> {
    source_url(rec).map(git_url_for_display)
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
///
/// **薄封装**：信任门（user-scope，§10.5/§11）属 CLI 表现层，其余 stage/no-clone 编排委托非 CLI 的
/// `register_or_stage_marketplace`；本 handler 仅做信任决策 + 结构化结果 → 退出码映射。
pub async fn marketplace_add(
    registry: &mut SkillRegistry,
    home: &Path,
    env: Option<&EnvMap>,
    git_url: &str,
    opts: MarketplaceAddOptions<'_>,
) -> i32 {
    let json_output = opts.json_output;
    // 解析身份（归一 URL + 派生/校验名）——信任门与 stage 共用同一身份语义。
    let identity = match resolve_marketplace_identity(git_url, opts.name) {
        Ok(id) => id,
        Err(e @ GovernanceError::InvalidUrl(_)) | Err(e @ GovernanceError::InvalidName(_)) => {
            return err(&identity_error_message(&e), json_output, EXIT_USER_ERROR);
        }
        Err(e) => return err(&e.to_string(), json_output, EXIT_USER_ERROR),
    };
    let mp_name = identity.name.clone();
    let display_url = git_url_for_display(&identity.url);

    // 重名校验先于信任提示（保留既有时序：重名直接拒、不弹 confirm、不记 trust）。
    if marketplace_name_taken(home, env, &mp_name) {
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
                        "untrusted marketplace {mp_name:?} ({display_url}); pass --trust to confirm non-interactively"
                    ),
                    json_output,
                    EXIT_USER_ERROR,
                )
            }
            Some(confirm) => {
                if !load_trusted(env).iter().any(|n| n == &mp_name)
                    && !confirm.confirm(&display_url).await
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

    // 编排（register-or-stage）委托 lifecycle，结构化结果 → 退出码 / 文案。
    match register_or_stage_marketplace(
        registry,
        home,
        env,
        &identity,
        &AddMarketplaceParams {
            name: opts.name,
            auto_update: opts.auto_update,
            no_clone: opts.no_clone,
        },
    )
    .await
    {
        Ok(outcome) if outcome.no_clone => ok_msg(&format!(
            "registered marketplace intent {mp_name:?} (no clone)"
        )),
        Ok(outcome) => {
            if json_output {
                print_json(&json!({
                    "added": outcome.name,
                    "url": git_url_for_display(&outcome.url),
                    "skills": outcome.skills.len()
                }));
                return EXIT_OK;
            }
            ok_msg(&format!(
                "added {mp_name:?} — cloned, {} skill(s) found",
                outcome.skills.len()
            ))
        }
        Err(GovernanceError::CloneFailed(name)) => err(
            &format!("clone/refresh failed for marketplace {name:?} (see logs)"),
            json_output,
            EXIT_NETWORK_ERROR,
        ),
        // no-clone 账本写失败（[`GovernanceError::Store`]）等 → 用户错。
        Err(e) => err(
            &format!("failed to record marketplace intent: {e}"),
            json_output,
            EXIT_USER_ERROR,
        ),
    }
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
            .map(|(nm, rec)| marketplace_list_row(nm, rec, is_trusted(nm)))
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
        let display_name = untrusted_name_for_display(nm);
        let url = display_source_url(rec).unwrap_or_default();
        let sha = rec
            .extra
            .get("commitSha")
            .and_then(Value::as_str)
            .map(redact_git_urls_in_text)
            .unwrap_or_default();
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
            "  {display_name}  ·  {url}  ·  trusted={}  ·  auto-update={auto}  ·  commit={sha_disp}",
            if is_trusted(nm) { "✓" } else { "—" },
        );
    }
    EXIT_OK
}

/// marketplace 详情：URL / clone 路径 / commit / plugins[] / auto_update / trusted。
pub fn marketplace_info(home: &Path, env: Option<&EnvMap>, name: &str, json_output: bool) -> i32 {
    let mps = load_mps(home, env);
    let display_name = untrusted_name_for_display(name);
    let Some(rec) = mps.marketplaces.get(name) else {
        return err(
            &unknown_marketplace_message(name),
            json_output,
            EXIT_USER_ERROR,
        );
    };

    let installed = load_installed_plugins(Some(home), env).account;
    let suffix = format!("@{name}");
    let plugins = display_plugin_ids(
        installed
            .plugins
            .keys()
            .filter(|pid| pid.ends_with(&suffix))
            .cloned(),
    );
    let trusted = load_trusted(env).iter().any(|t| t == name);

    let info = json!({
        "name": display_name.clone(),
        "url": display_source_url(rec),
        "installLocation": rec.extra.get("installLocation").map_or(Value::Null, redact_public_value),
        "commitSha": rec.extra.get("commitSha").map_or(Value::Null, redact_public_value),
        "autoUpdate": rec.extra.get("autoUpdate").and_then(Value::as_bool).unwrap_or(false),
        "trusted": trusted,
        "lastUpdated": rec.extra.get("lastUpdated").map_or(Value::Null, redact_public_value),
        "installedPlugins": plugins,
    });
    if json_output {
        print_json(&info);
        return EXIT_OK;
    }
    println!("Marketplace · {display_name}");
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
///
/// **薄封装**：未知校验 + confirm 闸门 + trust 撤销（user-scope）属 CLI 表现层，级联卸载 + prune 编排委托非
/// CLI 的 [`remove_marketplace`]；本 handler 仅做结构化结果 → 退出码映射。
/// `non_plugin_bundle_ids`：#139 回收判据「非用户声明」数据源（`origin != plugin` 全集）——MUST 由持有
/// `Computer` 的调用方经 `Computer::non_plugin_declared_bundle_ids` 供给；空集会连坐用户/宿主自有 server。
/// `opts.hooks == None` 的路径不停摘，该集未被读取。
pub async fn marketplace_remove(
    registry: &mut SkillRegistry,
    home: &Path,
    env: Option<&EnvMap>,
    name: &str,
    opts: MarketplaceRemoveOptions<'_>,
    non_plugin_bundle_ids: &std::collections::HashSet<crate::mcp_clients::model::BundleId>,
) -> i32 {
    let json_output = opts.json_output;
    let display_name = untrusted_name_for_display(name);
    // 未知校验先于 confirm（保留既有时序：未知直接拒、不弹 confirm）。
    if !marketplace_name_taken(home, env, name) {
        return err(
            &unknown_marketplace_message(name),
            json_output,
            EXIT_USER_ERROR,
        );
    }
    if let Some(confirm) = opts.confirm {
        if !confirm.confirm(&display_name).await {
            return err("aborted by user", json_output, EXIT_USER_ERROR);
        }
    }

    let outcome = match remove_marketplace(
        registry,
        home,
        env,
        name,
        RemoveMarketplaceParams {
            keep_plugins: opts.keep_plugins,
            hooks: opts.hooks,
        },
        non_plugin_bundle_ids,
    )
    .await
    {
        Ok(o) => o,
        Err(e) => return err(&e.to_string(), json_output, EXIT_USER_ERROR),
    };

    // 撤销信任，避免 trustedMarketplaces 只增不减（user scope）。best-effort：对标 Python（marketplace.py:309
    // 无错误处理）——写失败仅告警、**不**把已完成的卸载/prune 级联翻成用户错（destructive 工作已落地）。
    if let Err(e) = revoke_trust(name, env) {
        msg_dim(&format!(
            "warning: failed to revoke trust for {display_name:?}: {e}"
        ));
    }

    if json_output {
        print_json(&remove_outcome_json(&outcome));
        return EXIT_OK;
    }
    let detail = if !outcome.uninstalled_plugins.is_empty() {
        format!(
            " (uninstalled {} plugin(s))",
            outcome.uninstalled_plugins.len()
        )
    } else if outcome.kept_plugins {
        " (plugins kept as orphans)".to_string()
    } else {
        String::new()
    };
    ok_msg(&format!("removed marketplace {display_name:?}{detail}"))
}

/// `git pull` 失败则全量重 clone；与缓存 plugin 集合对账 + 失败汇总（§10.4）/ refresh marketplaces。
///
/// **薄封装**：逐 marketplace 对账编排委托非 CLI 的 [`refresh_marketplaces`]；本 handler 仅把结构化行
/// 渲染为 JSON / 文本（refresh 永远 `EXIT_OK`，失败逐行以 `missing` 汇报）。
pub async fn marketplace_refresh(
    registry: &mut SkillRegistry,
    home: &Path,
    env: Option<&EnvMap>,
    target: &str,
    json_output: bool,
) -> i32 {
    let rows = refresh_marketplaces(registry, home, env, target).await;

    if json_output {
        print_json(&json!(refresh_rows_json(&rows)));
        return EXIT_OK;
    }
    for r in &rows {
        let mark = match r.status {
            RefreshStatus::Updated => "✓",
            RefreshStatus::Unchanged => "·",
            RefreshStatus::Missing => "✗",
        };
        println!(
            "  {mark} {}  ({})",
            untrusted_name_for_display(&r.name),
            r.status.as_str()
        );
    }
    let updated = rows
        .iter()
        .filter(|r| r.status == RefreshStatus::Updated)
        .count();
    let failed = rows
        .iter()
        .filter(|r| r.status == RefreshStatus::Missing)
        .count();
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
    let display_name = untrusted_name_for_display(name);
    if key != "auto-update" {
        return err(&unknown_key_message(key), json_output, EXIT_USER_ERROR);
    }
    let val = matches!(
        value.trim().to_lowercase().as_str(),
        "true" | "1" | "yes" | "on"
    );
    if !load_mps(home, env).marketplaces.contains_key(name) {
        return err(
            &unknown_marketplace_message(name),
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
        print_json(&json!({ "name": display_name, "autoUpdate": val }));
        return EXIT_OK;
    }
    ok_msg(&format!("{display_name:?} auto-update set to {val}"))
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
        target: std::sync::Mutex<Option<String>>,
    }

    impl MockConfirm {
        fn new(answer: bool) -> Self {
            Self {
                answer,
                called: std::sync::atomic::AtomicBool::new(false),
                target: std::sync::Mutex::new(None),
            }
        }
        fn was_called(&self) -> bool {
            self.called.load(std::sync::atomic::Ordering::SeqCst)
        }
        fn target(&self) -> Option<String> {
            self.target.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Confirm for MockConfirm {
        async fn confirm(&self, target: &str) -> bool {
            self.called.store(true, std::sync::atomic::Ordering::SeqCst);
            *self.target.lock().unwrap() = Some(target.to_string());
            self.answer
        }
    }

    // URL 归一 / 名派生纯函数的单测随实现迁移到 settings::lifecycle（#94），不在此重复。

    #[test]
    fn identity_error_cli_message_never_echoes_credentialed_url() {
        const SECRET_URL: &str = "https://cnb:FAKE_TOKEN@example.com/org/repo.git";
        let error = resolve_marketplace_identity(SECRET_URL, Some("Turingfocus")).unwrap_err();
        let message = identity_error_message(&error);
        assert!(message.contains("Turingfocus"));
        assert!(!message.contains("cnb"));
        assert!(!message.contains("FAKE_TOKEN"));
        assert!(!message.contains(SECRET_URL));
    }

    #[test]
    fn identity_error_cli_message_redacts_url_embedded_in_explicit_name() {
        let error = resolve_marketplace_identity(
            "acme/skills",
            Some("key=https://user:PW_SECRET@example.com/name"),
        )
        .unwrap_err();
        let message = identity_error_message(&error);
        assert!(message.contains("key="));
        for secret in ["user", "PW_SECRET"] {
            assert!(!message.contains(secret), "{message}");
        }
    }

    #[test]
    fn identity_error_cli_message_for_pathless_url_contains_no_credentials() {
        let error = resolve_marketplace_identity(
            "https://user:PW_SECRET@example.com?token=QUERY#FRAGMENT",
            None,
        )
        .unwrap_err();
        let message = identity_error_message(&error);
        assert!(message.contains("cannot derive a valid marketplace name"));
        for secret in ["user", "PW_SECRET", "QUERY", "FRAGMENT"] {
            assert!(!message.contains(secret), "{message}");
        }
    }

    #[test]
    fn unknown_name_and_key_cli_messages_never_echo_credentials() {
        for message in [
            unknown_marketplace_message(
                "x=https://public.example/a=https://user2:PW_TWO@secret.example/repo.git",
            ),
            unknown_marketplace_message(
                "x=https://example.com/r.git?token=QUERY_QUOTE'LEAK_SECRET",
            ),
            unknown_marketplace_message("x=https://alice:PW_ONE'PW_TWO@example.com/repo.git"),
            unknown_marketplace_message("key=用户@example.com:org/repo.git"),
            unknown_key_message("https://user:PW_SECRET@example.com/repo.git"),
        ] {
            for secret in [
                "user",
                "PW_SECRET",
                "user2",
                "PW_TWO",
                "用户",
                "QUERY_QUOTE",
                "LEAK_SECRET",
                "PW_ONE",
            ] {
                assert!(!message.contains(secret), "{message}");
            }
        }
    }

    #[test]
    fn ledger_url_display_is_credential_free_for_text_and_json_paths() {
        let rec = KnownMarketplaceEntry {
            source: json!({
                "type": "git",
                "url": "https://cnb:FAKE_TOKEN@example.com/org/repo.git?token=QUERY#FRAGMENT"
            }),
            extra: Map::new(),
        };
        let displayed = display_source_url(&rec).unwrap();
        assert_eq!(displayed, "https://example.com/org/repo.git");
        for secret in ["cnb", "FAKE_TOKEN", "QUERY", "FRAGMENT"] {
            assert!(!displayed.contains(secret));
        }
    }

    #[test]
    fn hand_edited_ledger_identifiers_are_safe_for_text_and_json() {
        let rec = KnownMarketplaceEntry {
            source: json!({"type": "git", "url": "https://example.com/repo.git"}),
            extra: Map::from_iter([
                (
                    "commitSha".to_string(),
                    json!("https://alice:PW_META@example.com/r.git?token=;META_SECRET"),
                ),
                (
                    "lastUpdated".to_string(),
                    json!({
                        "x=https://alice:PW_KEY@example.com/r.git": {
                            "source": "git@secret.example:org/repo.git"
                        }
                    }),
                ),
            ]),
        };
        let row = marketplace_list_row("x=https://alice:PW_NAME@example.com/repo.git", &rec, false);
        let plugin_ids =
            display_plugin_ids(["x=https://alice:PW_PLUGIN@example.com/repo.git@safe".to_string()]);
        let remove = remove_outcome_json(&MarketplaceRemoveOutcome {
            name: "x=https://alice:PW_REMOVE@example.com/repo.git".to_string(),
            pruned: vec!["git@secret.example:PW_PRUNED".to_string()],
            uninstalled_plugins: vec![
                "x=https://alice:PW_UNINSTALL@example.com/repo.git@safe".to_string()
            ],
            kept_plugins: false,
        });
        let refresh = refresh_rows_json(&[MarketplaceRefreshRow {
            name: "x=https://alice:PW_REFRESH@example.com/repo.git".to_string(),
            status: RefreshStatus::Missing,
            skills: 0,
        }]);
        let rendered = format!(
            "{}\n{}\n{}\n{}\n{}",
            serde_json::to_string(&row).unwrap(),
            row["name"],
            plugin_ids.join(", "),
            serde_json::to_string(&remove).unwrap(),
            serde_json::to_string(&refresh).unwrap(),
        );
        for secret in [
            "alice",
            "PW_META",
            "META_SECRET",
            "PW_KEY",
            "secret.example",
            "PW_NAME",
            "PW_PLUGIN",
            "PW_REMOVE",
            "PW_PRUNED",
            "PW_UNINSTALL",
            "PW_REFRESH",
        ] {
            assert!(!rendered.contains(secret), "{rendered}");
        }
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
            &std::collections::HashSet::new(),
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
            &std::collections::HashSet::new(),
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
    async fn add_interactive_confirm_receives_credential_free_url() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let env = test_env(home);
        let mut registry = SkillRegistry::new();
        let confirm = MockConfirm::new(false);
        let code = marketplace_add(
            &mut registry,
            home,
            Some(&env),
            "https://cnb:FAKE_TOKEN@example.com/org/repo.git?token=QUERY#FRAGMENT",
            MarketplaceAddOptions {
                name: Some("valid-name"),
                confirm: Some(&confirm),
                json_output: true,
                ..Default::default()
            },
        )
        .await;
        assert_eq!(code, EXIT_USER_ERROR);
        assert_eq!(
            confirm.target().as_deref(),
            Some("https://example.com/org/repo.git")
        );
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
        assert!(load_mps(home, Some(&env))
            .marketplaces
            .contains_key("skills"));
    }

    // ── 🟡10：refresh handler 退出码 + JSON 键名 parity（跨 SDK 兼容契约）───────────
    #[tokio::test]
    async fn refresh_handler_empty_home_is_exit_ok() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let env = test_env(home);
        let mut registry = SkillRegistry::new();
        // 无 marketplace → 空行 → refresh 永远 EXIT_OK。
        let code = marketplace_refresh(&mut registry, home, Some(&env), "all", true).await;
        assert_eq!(code, EXIT_OK);
    }

    #[test]
    fn remove_outcome_json_key_parity() {
        let outcome = MarketplaceRemoveOutcome {
            name: "skills".to_string(),
            pruned: vec!["skills".to_string()],
            uninstalled_plugins: vec!["audit@skills".to_string()],
            kept_plugins: false,
        };
        let v = remove_outcome_json(&outcome);
        // camelCase 键锁定（跨 SDK parity，防漂移）。
        assert_eq!(v["removed"], json!("skills"));
        assert_eq!(v["pruned"], json!(["skills"]));
        assert_eq!(v["uninstalledPlugins"], json!(["audit@skills"]));
        assert_eq!(v["keptPlugins"], json!(false));
        assert_eq!(v.as_object().unwrap().len(), 4, "仅这 4 个键");
    }

    #[test]
    fn refresh_rows_json_key_parity() {
        let rows = vec![
            MarketplaceRefreshRow {
                name: "a".to_string(),
                status: RefreshStatus::Updated,
                skills: 3,
            },
            MarketplaceRefreshRow {
                name: "b".to_string(),
                status: RefreshStatus::Missing,
                skills: 0,
            },
        ];
        let v = refresh_rows_json(&rows);
        // Updated 行：name/status/skills。
        assert_eq!(v[0]["name"], json!("a"));
        assert_eq!(v[0]["status"], json!("updated"));
        assert_eq!(v[0]["skills"], json!(3));
        // Missing 行：name/status（省 skills）。
        assert_eq!(v[1]["name"], json!("b"));
        assert_eq!(v[1]["status"], json!("missing"));
        assert!(v[1].get("skills").is_none(), "missing 行省 skills");
    }
}
