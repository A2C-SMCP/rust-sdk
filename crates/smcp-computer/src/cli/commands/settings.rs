/*!
* 文件名: settings.rs
* 作者: JQQ
* 创建日期: 2026/06/10
* 最后修改日期: 2026/06/10
* 版权: 2023 JQQ. All rights reserved.
* 依赖: serde_json
* 描述: `settings` 命令 handler（show / get / set / edit）
*       Settings command handlers.
*
* 对标 Python `a2c_smcp/computer/cli/commands/settings.py`：读经 [`super::resolved_settings`]（merged，含 policy
* first-source-wins）或单层 [`load_settings_file`]；写经 [`apply_write`]（**数组整体替换**；删键仅经 DELETE
* 哨兵，`null` 写 JSON null，§5.4）+ store 持锁原子写。
*
* - **scope**：`user` / `project` / `local` 可写；`flag` / `policy` **只读**（set/edit 拒绝，退出码 1）；
*   `merged`（默认 show）= 五层合并视图。#98：project/local 锚定进程 cwd（`cwd` 注入接缝，`None` → 进程 cwd）。
* - settings.json **无 version 字段**（复刻 CC passthrough，§5）；写不注 version/保护头。
* - `edit` 用 `$EDITOR` 打开该层文件；保存后的 reconcile（mark_skills_dirty）由 REPL（#54）承担。
*/

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use super::{
    err_flat as err, ok_msg, print_json, resolved_settings_with_errors, EXIT_OK, EXIT_USER_ERROR,
};
use crate::settings::scope::{
    apply_write, load_settings_file, resolve_cwd, user_settings_path, workdir_local_settings_path,
    workdir_project_settings_path, EnvMap, WriteValue,
};
use crate::settings::store::{atomic_write_settings_json, with_settings_lock};
use crate::settings::{
    resolve_policy_settings, validate_settings, SettingsScope, SettingsValidationError,
};

// 可读 scope（show/get）/ readable scopes；可写 scope（set/edit）/ writable scopes。
const READABLE: [&str; 6] = ["user", "project", "local", "flag", "policy", "merged"];
const WRITABLE: [&str; 3] = ["user", "project", "local"];

/// REPL 改这些字段后须触发去抖 emit（接 SKL watcher/reconciler）/ keys whose change triggers a debounced emit。
pub const EMIT_KEYS: [&str; 4] = [
    "enabledPlugins",
    "enabledMcpjsonServers",
    "disabledMcpjsonServers",
    "enableAllProjectMcpServers",
];

/// `settings set/edit` 命中 emit 关键字段判定（REPL 据此调 `mark_skills_dirty`）/ is this an emit-triggering key。
pub fn is_emit_key(key: &str) -> bool {
    EMIT_KEYS.contains(&key)
}

/// 解析可写 scope 的 settings.json 路径 + enum；flag/policy/unknown → `Err`（对标 Python `_writable_path`）。
///
/// #98：project/local 锚定 `cwd`（`None` → 进程 cwd）；进程 cwd 不可读 → `Err`（罕见）。
fn writable_path(
    scope: &str,
    cwd: Option<&Path>,
    env: Option<&EnvMap>,
) -> Result<(PathBuf, SettingsScope), String> {
    match scope {
        "user" => Ok((user_settings_path(env), SettingsScope::User)),
        "project" | "local" => {
            let base = resolve_cwd(cwd).ok_or_else(|| {
                format!("scope {scope:?} anchor unavailable (process cwd unreadable)")
            })?;
            if scope == "project" {
                Ok((workdir_project_settings_path(&base), SettingsScope::Project))
            } else {
                Ok((workdir_local_settings_path(&base), SettingsScope::Local))
            }
        }
        _ => Err(format!(
            "scope {scope:?} is read-only (writable: user|project|local)"
        )),
    }
}

/// 读单 scope（或 merged）settings dict **连同校验错误**；进程 cwd 不可读时 project/local → 空层 / read one scope。
///
/// #98：project/local 锚定 `cwd`（`None` → 进程 cwd）。
///
/// #143/#145：**恒带出 errors**（无吞错误包装）—— scope 越权会静默丢字段（policy-only / 审批门 enable 方向
/// 判据），`settings show`/`get` 是用户排查「我的 settings 莫名不生效」时**最先跑**的命令，若吞错误则诊断回路
/// 断裂、`get` 还会答「not set in scope」主动误导。呈现由调用方统一走 stderr（`format_settings_errors`）。
/// 协议 §3「响亮失败」。对拍 python `_read_scope_with_errors`（python#157 已删吞错误包装，rust 同构）。
fn read_scope_with_errors(
    scope: &str,
    env: Option<&EnvMap>,
    cwd: Option<&Path>,
    flag_path: Option<&Path>,
) -> Option<(Map<String, Value>, Vec<SettingsValidationError>)> {
    match scope {
        "merged" => {
            let rs = resolved_settings_with_errors(cwd, env, flag_path);
            Some((rs.settings, rs.errors))
        }
        "user" => Some(load_settings_file(
            &user_settings_path(env),
            SettingsScope::User,
        )),
        "project" | "local" => {
            let base = resolve_cwd(cwd);
            let (path, enum_) = if scope == "project" {
                (
                    base.map(|b| workdir_project_settings_path(&b)),
                    SettingsScope::Project,
                )
            } else {
                (
                    base.map(|b| workdir_local_settings_path(&b)),
                    SettingsScope::Local,
                )
            };
            // cwd 不可读 → 空层（不 panic）。
            Some(match path {
                Some(p) => load_settings_file(&p, enum_),
                None => (Map::new(), Vec::new()),
            })
        }
        "flag" => match flag_path {
            Some(fp) => Some(load_settings_file(fp, SettingsScope::Flag)),
            None => Some((Map::new(), Vec::new())),
        },
        // #166：policy 层经 validate_settings 做字段级容错校验（类型错等），与 merged 路径
        // resolve_settings 对 policy 层的处理同构（scope.rs L401-408）。
        // 对拍 python-sdk#161。
        "policy" => {
            let raw = resolve_policy_settings(env, None, None);
            let (cleaned, errors) =
                validate_settings(&Value::Object(raw), SettingsScope::Policy, None);
            Some((cleaned, errors))
        }
        _ => None,
    }
}

fn editor_from_env(env: Option<&EnvMap>, key: &str) -> Option<String> {
    // 对标 Python `(env or os.environ).get(key)`：env 提供（即便空）→ 仅查 env，不回退进程环境。
    let raw = match env {
        Some(map) => map.get(key).cloned(),
        None => std::env::var(key).ok(),
    };
    // 空串视为未设置（对标 Python `or` 链把 "" 当 falsy → 穿透到下一候选）。
    raw.filter(|s| !s.is_empty())
}

// ── handlers ─────────────────────────────────────────────────────────────────
/// 展示某 scope 的 settings（默认 merged 合并视图，恒输出 JSON）/ show settings of a scope。
pub fn settings_show(
    env: Option<&EnvMap>,
    scope: &str,
    cwd: Option<&Path>,
    flag_path: Option<&Path>,
    json_output: bool,
) -> i32 {
    if !READABLE.contains(&scope) {
        return err(
            &format!("unknown scope {scope:?} (expected {})", READABLE.join("|")),
            json_output,
            EXIT_USER_ERROR,
        );
    }
    // READABLE 预检后 read_scope_with_errors 恒 Some；None 仅在未知 scope（防御）。
    let Some((data, errors)) = read_scope_with_errors(scope, env, cwd, flag_path) else {
        return err(
            &format!("unknown scope {scope:?}"),
            json_output,
            EXIT_USER_ERROR,
        );
    };
    // #143：scope 越权被过滤的字段在此有解释（打 stderr，不污染 stdout 的 JSON）。
    for line in super::format_settings_errors(&errors) {
        eprintln!("{line}");
    }
    print_json(&Value::Object(data));
    EXIT_OK
}

/// 读取单字段（默认 merged 视图，恒输出 JSON）/ read a single field。
pub fn settings_get(
    env: Option<&EnvMap>,
    key: &str,
    scope: &str,
    cwd: Option<&Path>,
    flag_path: Option<&Path>,
    json_output: bool,
) -> i32 {
    if !READABLE.contains(&scope) {
        return err(
            &format!("unknown scope {scope:?} (expected {})", READABLE.join("|")),
            json_output,
            EXIT_USER_ERROR,
        );
    }
    // READABLE 预检后 read_scope_with_errors 恒 Some；None 仅在未知 scope（防御）。
    let Some((data, errors)) = read_scope_with_errors(scope, env, cwd, flag_path) else {
        return err(
            &format!("unknown scope {scope:?}"),
            json_output,
            EXIT_USER_ERROR,
        );
    };
    // #145：越权字段会被过滤出 data —— 若不呈现，下面的 "not set in scope" 会**主动误导**
    // （文件里明明写了，却被安全策略过滤谎报成「你没配」）。故在判 key 命中之前先经 stderr 解释。
    // 对齐 settings_show 与 python-sdk#157（emit 置于 key 判空之前）；stdout JSON 契约不变。
    for line in super::format_settings_errors(&errors) {
        eprintln!("{line}");
    }
    let Some(value) = data.get(key) else {
        return err(
            &format!("key {key:?} not set in scope {scope:?}"),
            json_output,
            EXIT_USER_ERROR,
        );
    };
    print_json(&json!({ key: value }));
    EXIT_OK
}

/// 写单字段（user|project|local；flag/policy 只读）/ write a single field。
///
/// 值解析：JSON 优先（`true` / `["a","b"]` / `{...}`），失败回退字面字符串。写经 `apply_write`——对象递归
/// 深合并、**数组整体替换**；`null` 写 JSON null（删键仅经 DELETE 哨兵，§5.4）。
pub fn settings_set(
    env: Option<&EnvMap>,
    key: &str,
    value: &str,
    scope: &str,
    cwd: Option<&Path>,
    json_output: bool,
) -> i32 {
    if !WRITABLE.contains(&scope) {
        return err(
            &format!(
                "scope {scope:?} is read-only (writable: {})",
                WRITABLE.join("|")
            ),
            json_output,
            EXIT_USER_ERROR,
        );
    }
    let (path, enum_) = match writable_path(scope, cwd, env) {
        Ok(pair) => pair,
        Err(e) => return err(&e, json_output, EXIT_USER_ERROR),
    };

    // JSON 优先，失败回退字面字符串（对标 Python json.loads + 回退）。
    let parsed: Value =
        serde_json::from_str(value).unwrap_or_else(|_| Value::String(value.to_string()));
    // `null` → 写 JSON `null`（**非**删键）：与 Python 逐字一致——`json.loads("null")` 得 `None`，
    // 经 `apply_write` 落 `result[key] = None`（apply_write **仅**对 DELETE 哨兵删 key，§5.4）。Rust 同构：
    // `From<Value>` 把 `Null → Set(null)`。删键能力留给显式 `WriteValue::Delete`（未来 `settings unset`），
    // 不在 `settings set <key> null` 单边引入跨-SDK 分叉（北极星：禁止静默 diverge Python）。
    let updates: BTreeMap<String, WriteValue> =
        BTreeMap::from([(key.to_string(), WriteValue::from(parsed.clone()))]);

    let locked = with_settings_lock(&path, || -> std::io::Result<()> {
        let (existing, _errors) = load_settings_file(&path, enum_);
        atomic_write_settings_json(&path, &apply_write(&existing, &updates))
    });
    match locked {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return err(&format!("write failed: {e}"), json_output, EXIT_USER_ERROR),
        Err(e) => {
            return err(
                &format!("settings lock failed: {e}"),
                json_output,
                EXIT_USER_ERROR,
            )
        }
    }

    if json_output {
        print_json(&json!({ "scope": scope, "key": key, "value": parsed }));
        return EXIT_OK;
    }
    ok_msg(&format!("set {key} in {scope} scope"))
}

/// 用 `$EDITOR` 打开该层 settings.json（保存后的 reconcile 由 REPL 承担）/ open the scope file in `$EDITOR`。
///
/// flag/policy 只读 → 退出码 1。缺文件先建空骨架 `{}`。
pub fn settings_edit(
    env: Option<&EnvMap>,
    scope: &str,
    cwd: Option<&Path>,
    editor: Option<&str>,
    json_output: bool,
) -> i32 {
    if !WRITABLE.contains(&scope) {
        return err(
            &format!(
                "scope {scope:?} is read-only (editable: {})",
                WRITABLE.join("|")
            ),
            json_output,
            EXIT_USER_ERROR,
        );
    }
    let (path, _enum) = match writable_path(scope, cwd, env) {
        Ok(pair) => pair,
        Err(e) => return err(&e, json_output, EXIT_USER_ERROR),
    };

    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return err(
                &format!("cannot create {}: {e}", parent.display()),
                json_output,
                EXIT_USER_ERROR,
            );
        }
    }
    if !path.exists() {
        if let Err(e) = std::fs::write(&path, "{\n}\n") {
            return err(
                &format!("cannot create {}: {e}", path.display()),
                json_output,
                EXIT_USER_ERROR,
            );
        }
    }

    let edit_cmd = editor
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| editor_from_env(env, "EDITOR"))
        .or_else(|| editor_from_env(env, "VISUAL"))
        .unwrap_or_else(|| "vi".to_string());
    let mut tokens = edit_cmd.split_whitespace();
    let Some(program) = tokens.next() else {
        return err("empty editor command", json_output, EXIT_USER_ERROR);
    };
    let status = std::process::Command::new(program)
        .args(tokens)
        .arg(&path)
        .status();
    match status {
        Ok(s) if s.success() => ok_msg(&format!("edited {scope} settings ({})", path.display())),
        Ok(s) => err(&format!("editor failed: {s}"), json_output, EXIT_USER_ERROR),
        Err(e) => err(&format!("editor failed: {e}"), json_output, EXIT_USER_ERROR),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::commands::test_env;
    use tempfile::tempdir;

    #[test]
    fn emit_keys_detected() {
        assert!(is_emit_key("enabledPlugins"));
        assert!(is_emit_key("enableAllProjectMcpServers"));
        assert!(!is_emit_key("trustedMarketplaces"));
    }

    #[test]
    fn show_unknown_scope_is_user_error() {
        let dir = tempdir().unwrap();
        let env = test_env(dir.path());
        assert_eq!(
            settings_show(Some(&env), "bogus", None, None, true),
            EXIT_USER_ERROR
        );
    }

    #[test]
    fn set_read_only_scope_rejected() {
        let dir = tempdir().unwrap();
        let env = test_env(dir.path());
        // flag / policy 只读。
        assert_eq!(
            settings_set(Some(&env), "k", "1", "flag", None, true),
            EXIT_USER_ERROR
        );
        assert_eq!(
            settings_set(Some(&env), "k", "1", "policy", None, true),
            EXIT_USER_ERROR
        );
    }

    #[test]
    fn set_project_anchors_cwd_and_succeeds() {
        // #98：project scope 不再要求 active workdir——锚定注入 cwd 写 <cwd>/.tfrobot/settings.json。
        let dir = tempdir().unwrap();
        let env = test_env(dir.path());
        let wd = dir.path().join("wd");
        assert_eq!(
            settings_set(Some(&env), "k", "1", "project", Some(&wd), true),
            EXIT_OK
        );
        let data = read_scope_with_errors("project", Some(&env), Some(&wd), None)
            .unwrap()
            .0;
        assert_eq!(data.get("k"), Some(&json!(1)));
        assert!(workdir_project_settings_path(&wd).exists());
    }

    #[test]
    fn set_then_get_roundtrip_user_scope() {
        let dir = tempdir().unwrap();
        let env = test_env(dir.path());
        // 写 JSON 数值。
        assert_eq!(
            settings_set(Some(&env), "maxConcurrency", "7", "user", None, true),
            EXIT_OK
        );
        // 直读 user scope 验证落盘。
        let data = read_scope_with_errors("user", Some(&env), None, None)
            .unwrap()
            .0;
        assert_eq!(data.get("maxConcurrency"), Some(&json!(7)));
        // get 命中 → 0；缺失 key → 1。
        assert_eq!(
            settings_get(Some(&env), "maxConcurrency", "user", None, None, true),
            EXIT_OK
        );
        assert_eq!(
            settings_get(Some(&env), "nope", "user", None, None, true),
            EXIT_USER_ERROR
        );
    }

    #[test]
    fn set_null_writes_json_null_not_delete() {
        let dir = tempdir().unwrap();
        let env = test_env(dir.path());
        settings_set(Some(&env), "foo", "\"bar\"", "user", None, true);
        assert_eq!(
            read_scope_with_errors("user", Some(&env), None, None)
                .unwrap()
                .0
                .get("foo"),
            Some(&json!("bar"))
        );
        // settings set foo null → 写 JSON null（**非**删键），逐字对齐 Python（apply_write 仅 DELETE 哨兵删键）。
        assert_eq!(
            settings_set(Some(&env), "foo", "null", "user", None, true),
            EXIT_OK
        );
        assert_eq!(
            read_scope_with_errors("user", Some(&env), None, None)
                .unwrap()
                .0
                .get("foo"),
            Some(&Value::Null)
        );
    }

    #[test]
    fn set_array_replaces_wholesale() {
        let dir = tempdir().unwrap();
        let env = test_env(dir.path());
        settings_set(Some(&env), "list", "[1,2,3]", "user", None, true);
        // 数组整体替换（非拼接）。
        settings_set(Some(&env), "list", "[9]", "user", None, true);
        assert_eq!(
            read_scope_with_errors("user", Some(&env), None, None)
                .unwrap()
                .0
                .get("list"),
            Some(&json!([9]))
        );
    }

    #[test]
    fn flag_scope_without_path_is_empty() {
        let dir = tempdir().unwrap();
        let env = test_env(dir.path());
        let data = read_scope_with_errors("flag", Some(&env), None, None)
            .unwrap()
            .0;
        assert!(data.is_empty());
    }

    #[test]
    fn editor_from_env_treats_empty_as_unset() {
        let mut env = std::collections::HashMap::new();
        // 空 EDITOR → 视为未设置（穿透回退链，对标 Python `or` falsy）。
        env.insert("EDITOR".to_string(), String::new());
        assert_eq!(editor_from_env(Some(&env), "EDITOR"), None);
        env.insert("EDITOR".to_string(), "nano".to_string());
        assert_eq!(
            editor_from_env(Some(&env), "EDITOR"),
            Some("nano".to_string())
        );
    }

    #[test]
    fn edit_read_only_scopes_rejected() {
        let dir = tempdir().unwrap();
        let env = test_env(dir.path());
        assert_eq!(
            settings_edit(Some(&env), "flag", None, None, true),
            EXIT_USER_ERROR
        );
        assert_eq!(
            settings_edit(Some(&env), "policy", None, None, true),
            EXIT_USER_ERROR
        );
    }

    #[test]
    fn edit_creates_skeleton_and_runs_editor() {
        let dir = tempdir().unwrap();
        let env = test_env(dir.path());
        // editor="true" 无害退出 0；缺文件先建空骨架 `{}`。
        let code = settings_edit(Some(&env), "user", None, Some("true"), true);
        assert_eq!(code, EXIT_OK);
        let path = user_settings_path(Some(&env));
        assert!(path.exists());
        assert!(std::fs::read_to_string(&path).unwrap().contains('{'));
    }

    // ── #166: policy scope validate ──────────────────────────────────────────

    /// #166：`validate_settings` + Policy scope 对类型错字段检测并过滤，与 merged 路径（scope.rs L401-408）同构。
    #[test]
    fn policy_scope_detects_type_errors() {
        let raw = json!({"allowedMcpServers": "not-a-list", "trustedMarketplaces": ["mp"]});
        let (cleaned, errors) = validate_settings(&raw, SettingsScope::Policy, None);
        // allowedMcpServers 应为 array，给 string → 被过滤
        assert!(!cleaned.contains_key("allowedMcpServers"));
        // 同 dict 中合法字段照常保留
        assert_eq!(cleaned.get("trustedMarketplaces"), Some(&json!(["mp"])));
        // errors 包含 allowedMcpServers 类型错
        assert!(errors.iter().any(|e| e.field == "allowedMcpServers"));
        assert!(errors.iter().any(|e| e.reason.contains("expected array")));
    }

    /// #166：经 `resolve_policy_settings`（注入 source）+ `validate_settings` 的**实际代码路径**组合，
    /// 验证 `read_scope_with_errors("policy", ...)` 修复不会被回退。
    #[test]
    fn policy_scope_resolve_then_validate_composition() {
        use crate::settings::PolicySource;

        let bad = json!({"allowedMcpServers": "not-a-list", "trustedMarketplaces": ["mp"]});
        let source = PolicySource {
            name: "test".into(),
            priority: 1,
            loader: Box::new(move || match &bad {
                Value::Object(m) => Some(m.clone()),
                _ => None,
            }),
        };
        let raw = resolve_policy_settings(None, None, Some(vec![source]));
        // 注入的 source 返回类型错 dict → raw 必非空
        assert!(!raw.is_empty());
        let (cleaned, errors) = validate_settings(&Value::Object(raw), SettingsScope::Policy, None);
        // allowedMcpServers 类型错被过滤
        assert!(!cleaned.contains_key("allowedMcpServers"));
        assert_eq!(cleaned.get("trustedMarketplaces"), Some(&json!(["mp"])));
        assert!(errors.iter().any(|e| e.field == "allowedMcpServers"));
    }

    /// #166：policy 全字段合法 → 无错误，全部保留。
    #[test]
    fn policy_scope_valid_fields_no_errors() {
        let raw = json!({"allowedMcpServers": ["a"], "trustedMarketplaces": ["mp"]});
        let (cleaned, errors) = validate_settings(&raw, SettingsScope::Policy, None);
        assert_eq!(cleaned.get("allowedMcpServers"), Some(&json!(["a"])));
        assert_eq!(cleaned.get("trustedMarketplaces"), Some(&json!(["mp"])));
        assert!(errors.is_empty());
    }
}
