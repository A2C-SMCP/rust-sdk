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

use std::path::Path;

use serde_json::{json, Map, Value};

use super::{
    err_flat as err, msg_dim, ok_msg, print_json, Confirm, EXIT_NETWORK_ERROR, EXIT_OK,
    EXIT_USER_ERROR,
};
use crate::settings::installer::McpInstallHooks;
use crate::settings::lifecycle::{
    add_marketplace, marketplace_name_taken, refresh_marketplaces, remove_marketplace,
    resolve_marketplace_add_request, AddMarketplaceParams, GovernanceError, MarketplaceRefreshRow,
    MarketplaceRemoveOutcome, RefreshStatus, RemoveMarketplaceParams,
};
use crate::settings::raw_write;
use crate::settings::redaction::{
    git_url_for_display, known_git_source_for_display, redact_git_urls_in_text,
    untrusted_name_for_display,
};
use crate::settings::schema::FIELD_TRUSTED_MARKETPLACES;
use crate::settings::scope::{load_settings_file, user_settings_path, EnvMap};
use crate::settings::store::{
    load_installed_plugins, load_known_marketplaces, update_known_marketplaces,
};
use crate::settings::{KnownMarketplaceEntry, KnownMarketplaces, SettingsScope};
use crate::skills::SkillRegistry;

// `marketplace add/refresh/remove` 的 stage+prune 高层编排已抬到非 CLI 的 [`crate::settings::lifecycle`]（#94）；
// URL 归一 / 名派生纯函数随之迁移，此处再导出以保持既有引用路径 / re-export relocated pure helpers。
pub use crate::settings::lifecycle::{default_marketplace_name, normalize_marketplace_url};

// ── 输出辅助 / output helpers ────────────────────────────────────────────────
/// 身份解析错误的 CLI 文案。保持纯函数，使终端文本与 JSON `error` 字段共用同一安全边界。
fn identity_error_message(error: &GovernanceError) -> String {
    match error {
        GovernanceError::InvalidUrl(source) => {
            format!("not a well-formed git url or owner/repo shorthand: {source:?}")
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
        .map(|r| {
            let mut row = match r.status {
                RefreshStatus::Missing => {
                    json!({ "name": untrusted_name_for_display(&r.name), "status": "missing" })
                }
                _ => json!({
                    "name": untrusted_name_for_display(&r.name),
                    "status": r.status.as_str(),
                    "skills": r.skills
                }),
            };
            if let Some(diagnostic) = &r.diagnostic {
                row["diagnostic"] = json!({
                    "code": diagnostic.code.as_str(),
                    "message": redact_git_urls_in_text(&diagnostic.message),
                });
            }
            row
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

/// 账本 URL 的 CLI 展示值；JSON 与文本输出必须共用，避免某种输出模式绕过脱敏。
///
/// 账本读取可能含手编/外部写入（不可信），known 展示函数自身按形状 fail-closed：
/// 合法 scp-like 恢复 `host:path`，畸形形态回落严格占位符。
fn display_source_url(rec: &KnownMarketplaceEntry) -> Option<String> {
    source_url(rec).map(known_git_source_for_display)
}

/// 非交互 untrusted 错误文案；错误载荷必须使用严格展示值（#154 边界）。
///
/// #272：注册名在 clone/读清单后才确定，错误面只展示脱敏 URL。内部套一层 [`git_url_for_display`]，
/// 让「错误面走 strict」由本函数自身保证，不依赖调用方预先把值脱敏。
fn untrusted_source_message(raw_url: &str) -> String {
    let strict_url = git_url_for_display(raw_url);
    format!("untrusted marketplace ({strict_url}); pass --trust to confirm non-interactively")
}

/// `marketplace add` 成功 JSON 的 URL 展示值（展示归一，不含凭据）。
fn added_marketplace_display_url(raw_url: &str) -> String {
    known_git_source_for_display(raw_url)
}

/// `marketplace add --json` 成功载荷（键名契约；#272 新增 `noClone` / `alreadyRegistered` / `flagsUpdated`）。
fn add_outcome_json(outcome: &crate::settings::MarketplaceAddOutcome) -> Value {
    json!({
        "added": outcome.name,
        "url": added_marketplace_display_url(&outcome.url),
        "skills": outcome.skills.len(),
        "noClone": outcome.no_clone,
        // 以 `created` 派生：自愈路径（既有注册、clone 缺失后恢复）不是首次注册。
        "alreadyRegistered": !outcome.created,
        "flagsUpdated": outcome.flags_updated,
    })
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
    // #253：strict raw RMW——只改 trustedMarketplaces，保留其余字段（含非法兄弟条目）；
    // 非数组 / 畸形 JSON / 根非对象 → fail-closed（marketplace_add 据此返回用户错误）。
    raw_write::update_raw_settings(&path, false, |map| {
        let arr = raw_write::array_field_mut(map, FIELD_TRUSTED_MARKETPLACES, &path)?;
        if !arr.iter().any(|v| v.as_str() == Some(name)) {
            arr.push(Value::String(name.to_string()));
        }
        Ok(())
    })
    .map_err(raw_write::RawWriteError::into_io_error)?;
    Ok(())
}

fn revoke_trust(name: &str, env: Option<&EnvMap>) -> std::io::Result<()> {
    let path = user_settings_path(env);
    // #253：strict raw RMW；字段/成员缺失 → 无操作（不创建 key）；末次移除保留 `[]`（沿用现状）。
    raw_write::update_raw_settings(&path, false, |map| {
        let Some(arr) = raw_write::array_field_opt_mut(map, FIELD_TRUSTED_MARKETPLACES, &path)?
        else {
            return Ok(());
        };
        arr.retain(|v| v.as_str() != Some(name));
        Ok(())
    })
    .map_err(raw_write::RawWriteError::into_io_error)?;
    Ok(())
}

// ── handlers ─────────────────────────────────────────────────────────────────
/// [`marketplace_add`] 选项 / add options。
#[derive(Default)]
pub struct MarketplaceAddOptions<'a> {
    /// **期望清单名**（仅校验，不覆盖注册名；#272）/ expected manifest name (validation only)。
    pub name: Option<&'a str>,
    /// 非交互信任旗（无 confirm 时**强制**）/ trust flag。
    pub trust: bool,
    /// 物化记录的 autoUpdate / autoUpdate flag。
    pub auto_update: bool,
    /// 仅注册意图、不 clone（`--no-clone`，§4.2 debug 用）/ register intent only。
    pub no_clone: bool,
    /// 首见信任确认回调（REPL session y/N；Typer 非交互 `None`）/ first-trust confirm callback。
    pub confirm: Option<&'a dyn Confirm>,
    /// 引用守卫的 flag 层 settings 文件（`--settings`，已归一；`None` = 无 flag 层，#267）。
    pub flag_settings_path: Option<&'a Path>,
    /// 结构化输出 / JSON output。
    pub json_output: bool,
}

/// 添加新 marketplace（trust 门 → clone/读清单定名 → 注册 → 成功落 trust）/ add a marketplace。
///
/// **薄封装**：#272 起注册名由清单顶层 `name` 决定，信任确认只能展示脱敏 URL；trust 在**注册成功后**
/// 才持久化（失败零残留；trust 写失败则回滚本次注册）。
pub async fn marketplace_add(
    registry: &mut SkillRegistry,
    home: &Path,
    env: Option<&EnvMap>,
    git_url: &str,
    opts: MarketplaceAddOptions<'_>,
) -> i32 {
    let json_output = opts.json_output;
    let request = match resolve_marketplace_add_request(git_url, opts.name) {
        Ok(request) => request,
        Err(e @ GovernanceError::InvalidUrl(_)) | Err(e @ GovernanceError::InvalidName(_)) => {
            return err(&identity_error_message(&e), json_output, EXIT_USER_ERROR);
        }
        Err(e) => return err(&e.to_string(), json_output, EXIT_USER_ERROR),
    };
    // 交互确认属展示面（known 归一）；错误载荷由 untrusted 文案助手内部走严格边界。
    let display_url = known_git_source_for_display(&request.url);

    // trust 门（§10.5/§11，#95 收紧）：非交互（confirm None）→ `--trust` 无条件强制（陈旧 user-scope trust
    // **不豁免**）；交互（confirm Some，REPL）→ 展示脱敏 URL 弹 confirm（注册名尚未知）。
    if !opts.trust {
        match opts.confirm {
            None => {
                return err(
                    &untrusted_source_message(&request.url),
                    json_output,
                    EXIT_USER_ERROR,
                );
            }
            Some(confirm) => {
                if !confirm.confirm(&display_url).await {
                    return err("aborted by user (untrusted)", json_output, EXIT_USER_ERROR);
                }
            }
        }
    }

    match add_marketplace(
        registry,
        home,
        env,
        git_url,
        AddMarketplaceParams {
            name: opts.name,
            auto_update: opts.auto_update,
            no_clone: opts.no_clone,
            config_cwd: None,
            flag_settings_path: opts.flag_settings_path,
            policy_settings: None,
        },
    )
    .await
    {
        Ok(outcome) => {
            // 注册成功后才落 trust；写失败 → 回滚**本次新增**的注册（幂等/自愈不得 prune 既有注册）。
            if let Err(e) = record_trust(&outcome.name, env) {
                if outcome.created {
                    let store = super::file_store(home, env);
                    if outcome.no_clone {
                        // no_clone 的 catalog 在调用前就存在：只撤本次插入的账本条目，绝不删目录。
                        let name = outcome.name.clone();
                        let _ = crate::settings::store::update_known_marketplaces(
                            |file| {
                                file.account.marketplaces.shift_remove(&name);
                            },
                            Some(home),
                            env,
                        );
                    } else {
                        crate::settings::reconciler::prune_marketplaces(
                            std::slice::from_ref(&outcome.name),
                            registry,
                            home,
                            &store,
                        );
                    }
                }
                return err(
                    &format!("failed to record trust for {:?}: {e}", outcome.name),
                    json_output,
                    EXIT_USER_ERROR,
                );
            }
            if json_output {
                print_json(&add_outcome_json(&outcome));
                return EXIT_OK;
            }
            if outcome.no_clone {
                return ok_msg(&format!(
                    "registered marketplace intent {:?} (no clone)",
                    outcome.name
                ));
            }
            if !outcome.created && outcome.changed {
                // 自愈：既有注册、clone 缺失后被重新物化。
                return ok_msg(&format!(
                    "marketplace {:?} already registered (clone restored)",
                    outcome.name
                ));
            }
            if !outcome.created {
                let suffix = if outcome.flags_updated {
                    " (flags updated)"
                } else {
                    ""
                };
                return ok_msg(&format!(
                    "marketplace {:?} already registered (same source){suffix}",
                    outcome.name
                ));
            }
            ok_msg(&format!(
                "added {:?} — cloned, {} skill(s) found",
                outcome.name,
                outcome.skills.len()
            ))
        }
        Err(GovernanceError::CloneFailed(name)) => {
            // #272：add 在读完清单前 clone，失败时注册名尚未知（空串）。
            let message = if name.is_empty() {
                "clone/refresh failed for marketplace source (see logs)".to_string()
            } else {
                format!("clone/refresh failed for marketplace {name:?} (see logs)")
            };
            err(&message, json_output, EXIT_NETWORK_ERROR)
        }
        Err(e) => err(&e.to_string(), json_output, EXIT_USER_ERROR),
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
        // #272：清单名基线（legacy 首次 refresh 记录；#289 迁移依据）。
        "manifestNameBaseline": rec
            .manifest_name_baseline
            .as_deref()
            .map(untrusted_name_for_display),
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
        "manifestNameBaseline",
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
            RefreshStatus::Rejected => "!",
        };
        println!(
            "  {mark} {}  ({})",
            untrusted_name_for_display(&r.name),
            r.status.as_str()
        );
        if let Some(diagnostic) = &r.diagnostic {
            println!("      {}", redact_git_urls_in_text(&diagnostic.message));
        }
    }
    let updated = rows
        .iter()
        .filter(|r| r.status == RefreshStatus::Updated)
        .count();
    let failed = rows
        .iter()
        .filter(|r| r.status == RefreshStatus::Missing)
        .count();
    let rejected = rows
        .iter()
        .filter(|r| r.status == RefreshStatus::Rejected)
        .count();
    msg_dim(&format!(
        "{} marketplace(s) · {updated} updated · {rejected} rejected · {failed} failed",
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

/// `marketplace migrate`：存量注册名迁移（#289）。
///
/// 默认 dry-run（零写入）；`--apply` 写入；`--resume` 续做未完成 journal；`--rollback <ts>` 还原。
/// **离线迁移**：不 clone、不触网；执行前请停止所有 Computer / python 实例。
pub fn marketplace_migrate(
    home: &Path,
    env: Option<&EnvMap>,
    apply: bool,
    update_declarations: bool,
    resume: bool,
    rollback: Option<&str>,
    json_output: bool,
) -> i32 {
    use crate::inputs::secret_store::SecretStore;
    use crate::settings::marketplace_migration::{
        apply_marketplace_name_migration, plan_marketplace_name_migration,
        resume_marketplace_name_migration, rollback_marketplace_name_migration,
        MarketplaceMigrationOptions,
    };

    let secrets = SecretStore::new();
    let cwd = std::env::current_dir().ok();
    let write_operation = apply || resume || rollback.is_some();
    if write_operation {
        eprintln!(
            "warning: marketplace migrate is offline; stop all Computer/python instances before applying"
        );
    }
    if let Some(timestamp) = rollback {
        return match rollback_marketplace_name_migration(home, env, timestamp, &secrets) {
            Ok(report) => {
                if json_output {
                    let mut value = serde_json::to_value(&report).unwrap_or(Value::Null);
                    print_json(redact_migration_json(&mut value));
                } else {
                    for name in &report.restored {
                        ok_msg(&format!(
                            "restored marketplace {:?}",
                            untrusted_name_for_display(name)
                        ));
                    }
                    for warning in &report.warnings {
                        eprintln!("warning: {}", untrusted_name_for_display(warning));
                    }
                }
                if report.warnings.is_empty() {
                    EXIT_OK
                } else {
                    super::EXIT_PARTIAL
                }
            }
            Err(e) => err(
                &format!("marketplace migrate --rollback failed: {e}"),
                json_output,
                EXIT_USER_ERROR,
            ),
        };
    }
    if resume {
        return match resume_marketplace_name_migration(home, env, cwd.as_deref(), &secrets) {
            Ok(report) => emit_migration_report(&report, json_output),
            Err(e) => err(
                &format!("marketplace migrate --resume failed: {e}"),
                json_output,
                EXIT_USER_ERROR,
            ),
        };
    }
    let options = MarketplaceMigrationOptions {
        update_declarations,
    };
    if apply {
        return match apply_marketplace_name_migration(home, env, cwd.as_deref(), options, &secrets)
        {
            Ok(report) => emit_migration_report(&report, json_output),
            Err(e) => err(
                &format!("marketplace migrate --apply failed: {e}"),
                json_output,
                EXIT_USER_ERROR,
            ),
        };
    }
    match plan_marketplace_name_migration(home, env, cwd.as_deref(), options, &secrets) {
        Ok(plan) => emit_migration_plan(&plan, json_output),
        Err(e) => err(
            &format!("marketplace migrate failed: {e}"),
            json_output,
            EXIT_USER_ERROR,
        ),
    }
}

fn emit_migration_plan(
    plan: &crate::settings::marketplace_migration::MarketplaceMigrationPlan,
    json_output: bool,
) -> i32 {
    use crate::settings::marketplace_migration::{MigrationAction, MigrationStatus};
    let status = if !plan.conflicts.is_empty() {
        MigrationStatus::Conflicts
    } else if plan.corrupt.is_some()
        || plan.items.iter().any(|i| {
            !matches!(
                i.action,
                MigrationAction::Rename | MigrationAction::BaselineOnly | MigrationAction::NoOp
            )
        })
        || plan.manual.iter().any(|m| m.effect == "will-be-inactive")
    {
        MigrationStatus::Partial
    } else {
        MigrationStatus::Ok
    };
    if json_output {
        let mut value = json!({
            "dryRun": true,
            "status": status,
            "items": plan.items,
            "conflicts": plan.conflicts,
            "manualReferences": plan.manual,
            "corrupt": plan.corrupt,
        });
        print_json(redact_migration_json(&mut value));
    } else {
        println!("Marketplace registration migration (dry-run):");
        for item in &plan.items {
            let old = untrusted_name_for_display(&item.old_name);
            let new = item
                .new_name
                .as_deref()
                .map(untrusted_name_for_display)
                .unwrap_or_else(|| "—".to_string());
            println!(
                "  {:<9} {} -> {}  source={}  plugins={}  secrets={}",
                action_label(item.action),
                old,
                new,
                item.source,
                item.plugins.len(),
                item.secrets
            );
            if let Some(reason) = item.reason {
                println!("            reason: {}", reason_label(reason));
            }
        }
        for conflict in &plan.conflicts {
            msg_dim(&format!(
                "  conflict  {} ({} -> {})",
                conflict.kind,
                untrusted_name_for_display(&conflict.old_name),
                conflict
                    .new_name
                    .as_deref()
                    .map(untrusted_name_for_display)
                    .unwrap_or_else(|| "—".to_string())
            ));
        }
        for manual in &plan.manual {
            let line = format!(
                "  manual    {} {} ({}) -> {}",
                manual.file,
                untrusted_name_for_display(&manual.pointer),
                manual.effect,
                untrusted_name_for_display(&manual.suggested)
            );
            if manual.effect == "will-be-inactive" {
                eprintln!("warning:{line}");
            } else {
                msg_dim(&line);
            }
        }
        if let Some(corrupt) = &plan.corrupt {
            eprintln!("error: {corrupt}");
        }
        if plan.items.is_empty() && plan.conflicts.is_empty() && plan.corrupt.is_none() {
            msg_dim("nothing to migrate");
        } else if plan.conflicts.is_empty()
            && plan.corrupt.is_none()
            && plan.items.iter().any(|i| {
                matches!(
                    i.action,
                    MigrationAction::Rename | MigrationAction::BaselineOnly
                )
            })
        {
            msg_dim("dry-run: no changes written; rerun with --apply to migrate");
        }
    }
    status_exit(status)
}

fn emit_migration_report(
    report: &crate::settings::marketplace_migration::MarketplaceMigrationReport,
    json_output: bool,
) -> i32 {
    if json_output {
        let mut value = serde_json::to_value(report).unwrap_or(Value::Null);
        print_json(redact_migration_json(&mut value));
    } else {
        println!("Marketplace registration migration:");
        if !report.conflicts.is_empty() {
            eprintln!("error: conflicts detected; no changes were written");
        }
        for item in &report.items {
            let old = untrusted_name_for_display(&item.old_name);
            let new = item
                .new_name
                .as_deref()
                .map(untrusted_name_for_display)
                .unwrap_or_else(|| "—".to_string());
            println!(
                "  {:<9} {} -> {}  source={}  plugins={}  secrets={}",
                action_label(item.action),
                old,
                new,
                item.source,
                item.plugins.len(),
                item.secrets
            );
            if let Some(reason) = item.reason {
                println!("            reason: {}", reason_label(reason));
            }
        }
        for conflict in &report.conflicts {
            msg_dim(&format!(
                "  conflict  {} ({} -> {})",
                conflict.kind,
                untrusted_name_for_display(&conflict.old_name),
                conflict
                    .new_name
                    .as_deref()
                    .map(untrusted_name_for_display)
                    .unwrap_or_else(|| "—".to_string())
            ));
        }
        for manual in &report.manual {
            eprintln!(
                "warning: {} {} ({}) -> {}",
                manual.file,
                untrusted_name_for_display(&manual.pointer),
                manual.effect,
                untrusted_name_for_display(&manual.suggested)
            );
        }
        for warning in &report.warnings {
            eprintln!("warning: {}", untrusted_name_for_display(warning));
        }
        if let Some(backup) = &report.backup {
            msg_dim(&format!(
                "backup: {} (keep it for manual rollback: --rollback {})",
                backup.path, backup.timestamp
            ));
        }
        if let Some(corrupt) = &report.corrupt {
            eprintln!("error: {corrupt}");
        }
        if report.items.is_empty() && report.conflicts.is_empty() && report.corrupt.is_none() {
            msg_dim("nothing to migrate");
        }
    }
    status_exit(report.status())
}

fn status_exit(status: crate::settings::marketplace_migration::MigrationStatus) -> i32 {
    use crate::settings::marketplace_migration::MigrationStatus;
    match status {
        MigrationStatus::Ok => EXIT_OK,
        MigrationStatus::Conflicts => EXIT_USER_ERROR,
        MigrationStatus::Partial => super::EXIT_PARTIAL,
    }
}

fn action_label(action: crate::settings::marketplace_migration::MigrationAction) -> &'static str {
    use crate::settings::marketplace_migration::MigrationAction;
    match action {
        MigrationAction::Rename => "rename",
        MigrationAction::BaselineOnly => "baseline",
        MigrationAction::NoOp => "no-op",
        MigrationAction::Unresolved => "unresolved",
        MigrationAction::Blocked => "blocked",
    }
}

/// 迁移 JSON 展示脱敏：legacy 旧名可能内嵌凭据（#285），所有名称/指针/详情走公开脱敏边界。
fn redact_migration_json(value: &mut Value) -> &mut Value {
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                if matches!(
                    key.as_str(),
                    "oldName" | "newName" | "suggested" | "pointer" | "detail"
                ) {
                    if let Some(text) = child.as_str() {
                        *child = Value::String(untrusted_name_for_display(text));
                    }
                } else if matches!(key.as_str(), "restored" | "warnings") {
                    if let Value::Array(items) = child {
                        for item in items {
                            if let Some(text) = item.as_str() {
                                *item = Value::String(untrusted_name_for_display(text));
                            }
                        }
                    } else if let Some(text) = child.as_str() {
                        *child = Value::String(untrusted_name_for_display(text));
                    }
                } else {
                    redact_migration_json(child);
                }
            }
        }
        Value::Array(items) => {
            for child in items {
                redact_migration_json(child);
            }
        }
        _ => {}
    }
    value
}

fn reason_label(reason: crate::settings::marketplace_migration::MigrationReason) -> String {
    serde_json::to_value(reason)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn migration_json_redacts_names_restored_and_warnings() {
        let mut value = json!({
            "items": [{"oldName": "user:pass@host", "newName": "team-tools", "detail": "x user:pass@host"}],
            "manualReferences": [{"pointer": "/enabledPlugins/alpha@user:pass@host",
                                  "suggested": "alpha@user:pass@host"}],
            "restored": ["user:pass@host"],
            "warnings": ["failed to restore user:pass@host/token"]
        });
        let redacted = redact_migration_json(&mut value).to_string();
        assert!(!redacted.contains("pass"), "credentials leaked: {redacted}");
    }

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

    // ── 真实 git 夹具（#272：注册名取清单 name，no_clone 需要现有 catalog）──────────
    fn git(args: &[&str], cwd: &std::path::Path) {
        let out = std::process::Command::new("git")
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

    /// 构造带顶层 `name` 的 marketplace 源仓库（空 plugin 列表，add 用例足够）。
    fn build_marketplace_repo(repo: &std::path::Path, name: &str) -> String {
        std::fs::create_dir_all(repo.join(".tfrobot-plugin")).unwrap();
        std::fs::write(
            repo.join(".tfrobot-plugin/marketplace.json"),
            json!({"name": name, "plugins": []}).to_string(),
        )
        .unwrap();
        git(&["init", "-q"], repo);
        git(&["add", "-A"], repo);
        git(&["commit", "-qm", "init"], repo);
        format!("file://{}", repo.display())
    }

    #[test]
    fn identity_error_cli_message_never_echoes_credentialed_url() {
        const SECRET_URL: &str = "https://cnb:FAKE_TOKEN@example.com/org/repo.git";
        let error = resolve_marketplace_add_request(SECRET_URL, Some("Turingfocus")).unwrap_err();
        let message = identity_error_message(&error);
        assert!(message.contains("Turingfocus"));
        assert!(!message.contains("cnb"));
        assert!(!message.contains("FAKE_TOKEN"));
        assert!(!message.contains(SECRET_URL));
    }

    #[test]
    fn identity_error_for_hostless_url_contains_no_slug_or_credentials() {
        let error =
            resolve_marketplace_add_request("ssh:///alice:PW_LEAK@example.com", None).unwrap_err();
        let message = identity_error_message(&error);
        assert!(message.contains("<redacted-git-source>"), "{message}");
        for secret in ["alice", "PW_LEAK", "alice-pw-leak-example-com", "ssh:///"] {
            assert!(!message.contains(secret), "{message}");
        }
    }

    #[test]
    fn identity_error_cli_message_redacts_url_embedded_in_explicit_name() {
        let error = resolve_marketplace_add_request(
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
            manifest_name_baseline: None,
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
    fn ledger_scp_like_url_display_restores_host_and_path() {
        let rec = KnownMarketplaceEntry {
            manifest_name_baseline: None,
            source: json!({
                "type": "git",
                "url": "deploy-token@example.com:org/repo.git?token=QUERY#FRAGMENT"
            }),
            extra: Map::new(),
        };
        let displayed = display_source_url(&rec).unwrap();
        assert_eq!(displayed, "example.com:org/repo.git");
        let row = marketplace_list_row("mp", &rec, false);
        assert_eq!(row["url"], "example.com:org/repo.git");
        for secret in ["deploy-token", "QUERY", "FRAGMENT"] {
            assert!(!displayed.contains(secret), "{displayed}");
        }
    }

    #[test]
    fn untrusted_marketplace_error_keeps_strict_display_for_scp_like_source() {
        // 传原始 URL：助手内部必须走 strict，错误载荷不得出现 known 展示的 host:path。
        let message = untrusted_source_message("git@example.com:org/repo.git");
        assert!(message.contains("<redacted-git-source>"), "{message}");
        assert!(!message.contains("example.com"), "{message}");
    }

    #[test]
    fn add_display_targets_restore_scp_like_host_and_path() {
        let request =
            resolve_marketplace_add_request("git@example.com:org/repo.git", None).unwrap();

        // 交互 confirm target 与成功 JSON 都必须展示 host:path，且不含 user@。
        let confirm_target = known_git_source_for_display(&request.url);
        assert_eq!(confirm_target, "example.com:org/repo.git");
        assert!(!confirm_target.contains("git@"));

        let json_url = added_marketplace_display_url(&request.url);
        assert_eq!(json_url, "example.com:org/repo.git");
        assert!(!json_url.contains("git@"));
    }

    #[test]
    fn hand_edited_ledger_identifiers_are_safe_for_text_and_json() {
        let rec = KnownMarketplaceEntry {
            manifest_name_baseline: None,
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
            diagnostic: None,
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
    async fn add_unsafe_source_rejected_without_trust_or_ledger_writes() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let env = test_env(home);
        let mut registry = SkillRegistry::new();
        for raw in [
            // hostless scheme
            "ssh:///alice:PW_LEAK@example.com",
            "ssh:/alice:PW_LEAK@example.com",
            "git:///alice:PW_LEAK@example.com",
            // 合法 URL / scp-like，但派生段含 userinfo 形态（含无 `@` 尾部与 percent-encoded）
            "file:///alice:PW_LEAK@example.com",
            "git@host:alice:PW_LEAK@example.com",
            "git@host:alice:PW_LEAK",
            "git@host:alice%3APW_LEAK%40example.com",
            "file:///alice:PW_LEAK",
            "file:///alice%3APW_LEAK%40example.com",
        ] {
            // --trust 不得绕过身份校验：hostless 必须在信任门与账本写入之前拒绝。
            let code = marketplace_add(
                &mut registry,
                home,
                Some(&env),
                raw,
                MarketplaceAddOptions {
                    trust: true,
                    no_clone: true,
                    json_output: true,
                    ..Default::default()
                },
            )
            .await;
            assert_eq!(code, EXIT_USER_ERROR, "{raw}");
            assert!(load_mps(home, Some(&env)).marketplaces.is_empty(), "{raw}");
            assert!(load_trusted(Some(&env)).is_empty(), "{raw}");
        }
    }

    #[tokio::test]
    async fn add_with_trust_records_trust_name() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let env = test_env(home);
        let mut registry = SkillRegistry::new();
        // #272：注册名来自清单 name（仓库目录名 repo ≠ 清单名 team-tools）；trust 在成功后落盘。
        let url = build_marketplace_repo(&dir.path().join("repo"), "team-tools");
        let code = marketplace_add(
            &mut registry,
            home,
            Some(&env),
            &url,
            MarketplaceAddOptions {
                trust: true,
                json_output: true,
                ..Default::default()
            },
        )
        .await;
        assert_eq!(code, EXIT_OK);
        // trust 以清单名（非 URL、非仓库名）落 user settings。
        assert_eq!(load_trusted(Some(&env)), vec!["team-tools".to_string()]);
        assert!(load_mps(home, Some(&env))
            .marketplaces
            .contains_key("team-tools"));
    }

    #[tokio::test]
    async fn add_failure_does_not_record_trust() {
        // #272：trust 在注册成功后才落盘；clone 失败 → 网络错 + 零 trust / 零账本。
        let dir = tempdir().unwrap();
        let home = dir.path();
        let env = test_env(home);
        let mut registry = SkillRegistry::new();
        let bad = format!("file://{}/nonexistent-repo.git", dir.path().display());
        let code = marketplace_add(
            &mut registry,
            home,
            Some(&env),
            &bad,
            MarketplaceAddOptions {
                trust: true,
                json_output: true,
                ..Default::default()
            },
        )
        .await;
        assert_eq!(code, EXIT_NETWORK_ERROR);
        assert!(load_trusted(Some(&env)).is_empty());
        assert!(load_mps(home, Some(&env)).marketplaces.is_empty());
    }

    #[tokio::test]
    async fn trust_write_failure_rolls_back_new_registration() {
        // #272：trust 在注册成功后落盘；写失败 → 回滚本次新增注册（零残留）。
        let dir = tempdir().unwrap();
        let home = dir.path();
        let env = test_env(home);
        // 让 user settings 写入失败：把 settings.json 做成目录。
        let settings_path = user_settings_path(Some(&env));
        std::fs::create_dir_all(&settings_path).unwrap();
        let mut registry = SkillRegistry::new();
        let url = build_marketplace_repo(&dir.path().join("repo"), "team-tools");
        let code = marketplace_add(
            &mut registry,
            home,
            Some(&env),
            &url,
            MarketplaceAddOptions {
                trust: true,
                json_output: true,
                ..Default::default()
            },
        )
        .await;
        assert_eq!(code, EXIT_USER_ERROR);
        assert!(load_mps(home, Some(&env)).marketplaces.is_empty());
        assert!(!home.join("marketplace/team-tools").exists());
    }

    #[tokio::test]
    async fn trust_write_failure_after_no_clone_keeps_existing_catalog() {
        // #272：no_clone 的 catalog 在调用前已存在；trust 写失败只能撤账本条目，绝不删目录。
        let dir = tempdir().unwrap();
        let home = dir.path();
        let env = test_env(home);
        let url = build_marketplace_repo(&dir.path().join("repo"), "team-tools");
        let mut throwaway = SkillRegistry::new();
        crate::skills::stage_marketplace_skills(
            "team-tools",
            &json!({"type": "git", "url": url}),
            &mut throwaway,
            home,
            crate::skills::MarketplaceStageOptions::default(),
        )
        .await;
        let settings_path = user_settings_path(Some(&env));
        std::fs::create_dir_all(&settings_path).unwrap();
        let mut registry = SkillRegistry::new();
        let code = marketplace_add(
            &mut registry,
            home,
            Some(&env),
            &url,
            MarketplaceAddOptions {
                name: Some("team-tools"),
                no_clone: true,
                trust: true,
                json_output: true,
                ..Default::default()
            },
        )
        .await;
        assert_eq!(code, EXIT_USER_ERROR);
        assert!(load_mps(home, Some(&env)).marketplaces.is_empty());
        assert!(
            home.join("marketplace/team-tools").is_dir(),
            "既有 catalog 不得被回滚删除"
        );
    }

    #[tokio::test]
    async fn add_duplicate_name_conflicts() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let env = test_env(home);
        let mut registry = SkillRegistry::new();
        let base = || MarketplaceAddOptions {
            trust: true,
            json_output: true,
            ..Default::default()
        };
        let url_a = build_marketplace_repo(&dir.path().join("repo-a"), "team-tools");
        let url_b = build_marketplace_repo(&dir.path().join("repo-b"), "team-tools");
        assert_eq!(
            marketplace_add(&mut registry, home, Some(&env), &url_a, base()).await,
            EXIT_OK
        );
        // 同名不同源 → 冲突退出码 1，trust 不新增（第二个名字相同，仍是 team-tools）。
        assert_eq!(
            marketplace_add(&mut registry, home, Some(&env), &url_b, base()).await,
            EXIT_USER_ERROR
        );
        assert_eq!(load_trusted(Some(&env)), vec!["team-tools".to_string()]);
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
        // 先真实 add + trust。
        let url = build_marketplace_repo(&dir.path().join("repo"), "team-tools");
        marketplace_add(
            &mut registry,
            home,
            Some(&env),
            &url,
            MarketplaceAddOptions {
                trust: true,
                json_output: true,
                ..Default::default()
            },
        )
        .await;
        assert_eq!(load_trusted(Some(&env)), vec!["team-tools".to_string()]);
        // remove → 撤销 trust + prune。
        let code = marketplace_remove(
            &mut registry,
            home,
            Some(&env),
            "team-tools",
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
    async fn add_interactive_always_confirms_before_add() {
        // #272：注册名在 clone/读清单后才确定，交互确认只能按脱敏 URL 进行——即便旧 trust 里已有
        // 某个名字，也无法在注册前跳过 prompt。
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

    // ---- #253：trust 写入 raw 保真 / corrupt fail-closed ----------------------
    fn trust_path(env: &EnvMap) -> std::path::PathBuf {
        user_settings_path(Some(env))
    }

    fn write_trust_file(path: &std::path::Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    fn trust_mtime(path: &std::path::Path) -> std::time::SystemTime {
        std::fs::metadata(path).unwrap().modified().unwrap()
    }

    #[test]
    fn record_trust_preserves_unknown_and_invalid_entries_253() {
        let dir = tempdir().unwrap();
        let env = test_env(dir.path());
        let path = trust_path(&env);
        write_trust_file(
            &path,
            r#"{"keepUnknown":{"k":1},"trustedMarketplaces":["ok",42],"permissions":"bad","allowedMcpServers":["policy-only"]}"#,
        );
        record_trust("new", Some(&env)).unwrap();
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved["trustedMarketplaces"], json!(["ok", 42, "new"]));
        assert_eq!(saved["keepUnknown"], json!({"k": 1}));
        assert_eq!(saved["permissions"], json!("bad"));
        assert_eq!(saved["allowedMcpServers"], json!(["policy-only"]));
    }

    #[test]
    fn record_trust_refuses_corrupt_and_non_array_253() {
        let dir = tempdir().unwrap();
        let env = test_env(dir.path());
        let path = trust_path(&env);
        write_trust_file(&path, "{not json");
        let before = std::fs::read(&path).unwrap();
        assert!(record_trust("new", Some(&env)).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        write_trust_file(&path, r#"{"trustedMarketplaces":"skills"}"#);
        let before = std::fs::read(&path).unwrap();
        assert!(record_trust("new", Some(&env)).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn record_trust_duplicate_is_noop_253() {
        let dir = tempdir().unwrap();
        let env = test_env(dir.path());
        let path = trust_path(&env);
        write_trust_file(&path, r#"{"trustedMarketplaces":["skills"]}"#);
        let before_mtime = trust_mtime(&path);
        let before_bytes = std::fs::read(&path).unwrap();
        record_trust("skills", Some(&env)).unwrap();
        assert_eq!(trust_mtime(&path), before_mtime);
        assert_eq!(std::fs::read(&path).unwrap(), before_bytes);
    }

    #[test]
    fn revoke_trust_removes_only_matching_strings_253() {
        let dir = tempdir().unwrap();
        let env = test_env(dir.path());
        let path = trust_path(&env);
        write_trust_file(
            &path,
            r#"{"trustedMarketplaces":["a",42,"b","a"],"keep":1}"#,
        );
        revoke_trust("a", Some(&env)).unwrap();
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved["trustedMarketplaces"], json!([42, "b"]));
        assert_eq!(saved["keep"], json!(1));
    }

    #[test]
    fn revoke_trust_refuses_corrupt_and_non_array_253() {
        let dir = tempdir().unwrap();
        let env = test_env(dir.path());
        let path = trust_path(&env);
        write_trust_file(&path, "{not json");
        let before = std::fs::read(&path).unwrap();
        assert!(revoke_trust("a", Some(&env)).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        write_trust_file(&path, r#"{"trustedMarketplaces":"skills"}"#);
        let before = std::fs::read(&path).unwrap();
        assert!(revoke_trust("a", Some(&env)).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn revoke_trust_repeat_is_noop_253() {
        let dir = tempdir().unwrap();
        let env = test_env(dir.path());
        let path = trust_path(&env);
        write_trust_file(&path, r#"{"trustedMarketplaces":["a","b"],"keep":1}"#);
        revoke_trust("a", Some(&env)).unwrap();
        // 紧凑 fixture：若重复 revoke 无条件重写，会被 pretty 格式化 → 字节变化。
        write_trust_file(&path, r#"{"trustedMarketplaces":["b"],"keep":1}"#);
        let before_mtime = trust_mtime(&path);
        let before_bytes = std::fs::read(&path).unwrap();
        revoke_trust("a", Some(&env)).unwrap();
        assert_eq!(trust_mtime(&path), before_mtime, "重复 revoke 不得写盘");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before_bytes,
            "重复 revoke 不得重写"
        );
    }

    #[test]
    fn revoke_trust_absent_is_noop_and_last_removal_keeps_empty_array_253() {
        let dir = tempdir().unwrap();
        let env = test_env(dir.path());
        let path = trust_path(&env);
        // 文件缺失 → 不创建 key、不报错。
        revoke_trust("a", Some(&env)).unwrap();
        assert!(!path.exists() || std::fs::read_to_string(&path).unwrap().trim().is_empty());
        write_trust_file(&path, r#"{"trustedMarketplaces":["a"]}"#);
        revoke_trust("a", Some(&env)).unwrap();
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved["trustedMarketplaces"], json!([]));
    }

    #[test]
    fn refresh_rows_json_rejected_includes_diagnostic() {
        let rows = vec![MarketplaceRefreshRow {
            name: "team-tools".to_string(),
            status: RefreshStatus::Rejected,
            skills: 0,
            diagnostic: Some(crate::settings::MarketplaceRefreshDiagnostic {
                code: crate::settings::RefreshDiagnosticCode::ManifestNameChanged,
                message: "marketplace manifest name changed".to_string(),
            }),
        }];
        let rows = refresh_rows_json(&rows);
        assert_eq!(rows[0]["status"], "rejected");
        assert_eq!(rows[0]["diagnostic"]["code"], "manifest_name_changed");
        assert!(rows[0]["diagnostic"]["message"]
            .as_str()
            .unwrap()
            .contains("manifest name changed"));
    }

    #[test]
    fn add_outcome_json_keys_and_no_clone_flags() {
        let outcome = crate::settings::MarketplaceAddOutcome {
            name: "team-tools".to_string(),
            url: "https://example.com/team/tools.git".to_string(),
            skills: Vec::new(),
            no_clone: true,
            changed: true,
            created: true,
            flags_updated: false,
        };
        let payload = add_outcome_json(&outcome);
        assert_eq!(payload["added"], "team-tools");
        assert_eq!(payload["url"], "https://example.com/team/tools.git");
        assert_eq!(payload["skills"], 0);
        assert_eq!(payload["noClone"], true);
        assert_eq!(payload["alreadyRegistered"], false);
        assert_eq!(payload["flagsUpdated"], false);

        let idempotent = crate::settings::MarketplaceAddOutcome {
            changed: false,
            created: false,
            flags_updated: true,
            ..outcome.clone()
        };
        let payload = add_outcome_json(&idempotent);
        assert_eq!(payload["alreadyRegistered"], true);
        assert_eq!(payload["flagsUpdated"], true);

        // 自愈：既有注册、clone 缺失后恢复 → 不是首次注册。
        let self_healed = crate::settings::MarketplaceAddOutcome {
            changed: true,
            created: false,
            ..outcome.clone()
        };
        let payload = add_outcome_json(&self_healed);
        assert_eq!(payload["alreadyRegistered"], true);
    }

    #[test]
    fn refresh_rows_json_key_parity() {
        let rows = vec![
            MarketplaceRefreshRow {
                name: "a".to_string(),
                status: RefreshStatus::Updated,
                skills: 3,
                diagnostic: None,
            },
            MarketplaceRefreshRow {
                name: "b".to_string(),
                status: RefreshStatus::Missing,
                skills: 0,
                diagnostic: None,
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
