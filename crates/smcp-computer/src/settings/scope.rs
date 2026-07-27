/*!
* 文件名: scope.rs
* 作者: JQQ
* 创建日期: 2026/06/02
* 最后修改日期: 2026/06/02
* 版权: 2023 JQQ. All rights reserved.
* 依赖: serde_json, smcp::utils::path, tracing
* 描述: 五级 scope 路径解析 + 读/写两套合并语义 + 进程 cwd 锚定 project/local 解析（#98）
*       Five-level scope path resolution + read/write merge customizers + process-cwd-anchored resolution.
*/

//! 五级 scope 路径解析 + 读/写两套合并语义 + 进程 cwd 锚定 project/local 解析（#98）。
//! Five-level scope path resolution + read/write merge customizers + process-cwd-anchored resolution.
//!
//! 对标 Python 参考实现 / Mirrors the Python reference: `a2c_smcp/computer/settings/scope.py`
//! （SDK 设计 `docs/design-0.2.1-cli-marketplace-ux.md` §5.0 / §5.1 / §5.4）。
//!
//! 这是 installer（SET-05）、mcp_config 批准门控（SET-06）以及 CLI settings 子命令（CLI-02）读取
//! **有效 settings** 的唯一入口。
//!
//! ## 合并语义（⚠️ 读 / 写相反，照 Claude Code `settingsMergeCustomizer`）/ Merge semantics
//!
//! **读合并**（启动加载、多 scope 叠加，low → high）/ Read merge (startup, low → high):
//! - object/map → **递归深合并**（逐层按 key 取并集，仅同名叶子由高 scope 覆盖；嵌套对象继续向下、
//!   **不整体替换**——`extraKnownMarketplaces[].source` 关键用例）/ deep recursive merge.
//! - array → **拼接去重** `uniq([*low, *high])`（低 scope 在前）/ concat + dedup, low first.
//! - scalar → 高 scope 整体覆盖 / scalar overwrite (highest scope wins).
//!
//! **写回单 scope**（CLI 改单文件、语义相反）/ Single-scope write-back (inverse):
//! - array → **直接替换**（不拼接）/ replace wholesale.
//! - 删字段 → [`WriteValue::Delete`] 哨兵（= 删 key）/ deletion via the sentinel.
//!
//! ## scope 分层（low → high，§5.1）/ Scope layering
//!
//! user（主）< project（进程 cwd 单根）< local（进程 cwd 单根）< flag < policy。#98（对齐 protocol#10 /
//! python-sdk#116）：能力发现层已移除，`Computer` 不再持有 workspace——project/local **无条件**锚定进程 cwd
//! （`<cwd>/.tfrobot/settings[.local].json`），`enabledPlugins` / `extraKnownMarketplaces` 经常规 project/local
//! 层进入，无需专门的能力并集层。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use smcp::utils::path::resolve_xdg_first;

use super::schema::{validate_settings, SettingsScope, SettingsValidationError};

// ---------------------------------------------------------------------------
// 路径常量 / Path constants
// ---------------------------------------------------------------------------
/// user scope 配置目录的 XDG 变量名 / XDG env var for the user-scope config dir。
pub const XDG_CONFIG_HOME_ENV: &str = "XDG_CONFIG_HOME";
/// `$HOME` 环境变量名（user config dir 回退用）/ `$HOME` env var (user config dir fallback)。
const HOME_ENV: &str = "HOME";

/// workspace 工作目录下的治理子目录名 / Per-workdir governance dir name。
pub const TFROBOT_DIRNAME: &str = ".tfrobot";
/// project scope 文件名（入 git）/ project-scope filename (git-tracked)。
pub const PROJECT_SETTINGS_FILENAME: &str = "settings.json";
/// local scope 文件名（不入 git）/ local-scope filename (git-ignored)。
pub const LOCAL_SETTINGS_FILENAME: &str = "settings.local.json";
/// user scope 文件名 / user-scope filename。
pub const USER_SETTINGS_FILENAME: &str = "settings.json";

/// user config dir 的 a2c 子目录名（`$XDG_CONFIG_HOME/a2c` 或 `~/.config/a2c`）/ a2c subdir。
const A2C_CONFIG_SUBDIR: &str = "a2c";

/// 环境映射（注入式，便于确定性测试）/ Injectable env mapping for deterministic tests。
pub type EnvMap = std::collections::HashMap<String, String>;

/// 从注入映射或进程环境取变量 / Look up a var from an injected map or the process env。
fn env_lookup(env: Option<&EnvMap>, key: &str) -> Option<String> {
    match env {
        Some(m) => m.get(key).cloned(),
        None => std::env::var(key).ok(),
    }
}

/// 解析 user scope 配置目录 / Resolve the user-scope config dir。
///
/// 优先 `$XDG_CONFIG_HOME/a2c`（须为绝对路径，否则按 XDG 规范忽略），回退 `~/.config/a2c`。
/// XDG-first 解析复用 [`smcp::utils::path::resolve_xdg_first`]（与 SKILL Home 同范式）。
/// `env` 为 `None` 时读进程环境（生产）；测试可注入隔离映射（含 `HOME` / `XDG_CONFIG_HOME`）。
///
/// Prefers `$XDG_CONFIG_HOME/a2c` (must be absolute), falls back to `~/.config/a2c`. With `env =
/// None` the process environment is read; tests inject an isolated map to stay hermetic.
///
/// 跨-SDK 注意 / Cross-SDK note：HOME 回退尊重注入的 `env`（便于 hermetic 测试）；Python `Path.home()`
/// 始终读进程 HOME、忽略注入 env。生产路径（`env = None`）两端一致，差异仅在测试注入场景。
pub fn resolve_user_config_dir(env: Option<&EnvMap>) -> PathBuf {
    let xdg = env_lookup(env, XDG_CONFIG_HOME_ENV);
    // 回退基目录：注入映射或进程的 `$HOME`；皆缺时留 `~` 交由 `resolve_xdg_first` 的 expanduser 处理。
    let home = env_lookup(env, HOME_ENV).unwrap_or_else(|| "~".to_string());
    let fallback = PathBuf::from(home).join(".config").join(A2C_CONFIG_SUBDIR);
    resolve_xdg_first(xdg.as_deref(), &[A2C_CONFIG_SUBDIR], &fallback)
}

/// user scope settings.json 路径 / Path to the user-scope settings.json。
pub fn user_settings_path(env: Option<&EnvMap>) -> PathBuf {
    resolve_user_config_dir(env).join(USER_SETTINGS_FILENAME)
}

/// 工作目录治理子目录 `<workdir>/.tfrobot` / The per-workdir `.tfrobot` dir。
pub fn workdir_settings_dir(workdir: &Path) -> PathBuf {
    workdir.join(TFROBOT_DIRNAME)
}

/// project scope 路径 `<workdir>/.tfrobot/settings.json`（入 git）/ project-scope path。
pub fn workdir_project_settings_path(workdir: &Path) -> PathBuf {
    workdir_settings_dir(workdir).join(PROJECT_SETTINGS_FILENAME)
}

/// local scope 路径 `<workdir>/.tfrobot/settings.local.json`（不入 git）/ local-scope path。
pub fn workdir_local_settings_path(workdir: &Path) -> PathBuf {
    workdir_settings_dir(workdir).join(LOCAL_SETTINGS_FILENAME)
}

// ---------------------------------------------------------------------------
// 合并原语 / Merge primitives
// ---------------------------------------------------------------------------
/// 去重保序（首次出现胜出）/ Order-preserving dedup (first occurrence wins)。
fn dedup_preserve_order(low: &[Value], high: &[Value]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::with_capacity(low.len() + high.len());
    for item in low.iter().chain(high.iter()) {
        if !out.contains(item) {
            out.push(item.clone());
        }
    }
    out
}

/// 读合并 customizer（深合并 / 数组拼接去重 / 标量高覆盖）/ Read-merge customizer。
///
/// 与 Claude Code `settingsMergeCustomizer` 等价：customizer 只特判数组（低在前拼接去重），其余交给
/// 默认递归深合并；标量 / 类型不一致由高 scope 覆盖（既定语义）。#98：跨目录能力层已移除，故不再收集
/// 同名覆盖冲突（原冲突诊断仅服务能力层 WARN）。
pub fn merge_read(low: &Value, high: &Value) -> Value {
    match (low, high) {
        (Value::Object(lo), Value::Object(ho)) => {
            let mut merged = lo.clone();
            for (key, hv) in ho {
                match merged.get(key).cloned() {
                    Some(lv) => {
                        merged.insert(key.clone(), merge_read(&lv, hv));
                    }
                    None => {
                        merged.insert(key.clone(), hv.clone());
                    }
                }
            }
            Value::Object(merged)
        }
        // 数组：低在前拼接去重（既定语义）。
        (Value::Array(la), Value::Array(ha)) => Value::Array(dedup_preserve_order(la, ha)),
        // 标量 / 类型不一致：高 scope 覆盖。
        _ => high.clone(),
    }
}

/// 按 low → high 顺序折叠多层 settings（读合并）/ Fold multiple layers low → high (read merge)。
///
/// `layers` 为已校验的各 scope 视图，**低优先级在前**
/// （如 `[capability, user, project, local, flag, policy]`）。
pub fn merge_layers(layers: &[Map<String, Value>]) -> Map<String, Value> {
    let mut result = Value::Object(Map::new());
    for layer in layers {
        result = merge_read(&result, &Value::Object(layer.clone()));
    }
    match result {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

/// 写回单 scope 的更新值 / A single-scope write-back update value。
///
/// 与读合并 **语义相反**（数组整体替换、可删 key），用于 CLI `settings set` / `settings unset`。
/// - [`WriteValue::Set`]：标量 / 数组 / 类型切换——**整体替换**（数组不拼接）。
/// - [`WriteValue::Object`]：递归合并——**保留兄弟键**、支持嵌套删除。
/// - [`WriteValue::Delete`]：删除该 key（对齐 Python `DELETE` 哨兵）。
#[derive(Debug, Clone, PartialEq)]
pub enum WriteValue {
    /// 删除该 key / remove the key。
    Delete,
    /// 设为该值（整体替换）/ set to this value (replace wholesale)。
    Set(Value),
    /// 递归合并子对象（保留兄弟键）/ recurse into a sub-object (siblings preserved)。
    Object(BTreeMap<String, WriteValue>),
}

impl From<Value> for WriteValue {
    /// 由 [`Value`] 构造写更新：对象 → 递归 [`WriteValue::Object`]（保留兄弟键，对齐读写一致的深合并
    /// 语义），其余 → [`WriteValue::Set`]。删除语义无法由 [`Value`] 表达，调用方显式用
    /// [`WriteValue::Delete`]（如 CLI `settings unset`）。
    fn from(value: Value) -> Self {
        match value {
            Value::Object(obj) => WriteValue::Object(
                obj.into_iter()
                    .map(|(k, v)| (k, WriteValue::from(v)))
                    .collect(),
            ),
            other => WriteValue::Set(other),
        }
    }
}

/// 写回单 scope（数组整体替换 + [`WriteValue::Delete`] 删 key，§5.4 写语义）/ Single-scope write-back。
///
/// 与 [`merge_read`] **语义相反**：数组**不拼接、直接替换**；[`WriteValue::Delete`] 的 key 被删除；
/// 嵌套对象继续递归（`settings set a.b.c` 只动该叶子，不毁同级兄弟键）。返回**新 Map**（不就地改）。
pub fn apply_write(
    existing: &Map<String, Value>,
    updates: &BTreeMap<String, WriteValue>,
) -> Map<String, Value> {
    let mut result = existing.clone();
    for (key, update) in updates {
        match update {
            WriteValue::Delete => {
                result.remove(key);
            }
            WriteValue::Set(value) => {
                result.insert(key.clone(), value.clone());
            }
            WriteValue::Object(sub) => {
                // 既有对象继续递归；否则从空对象起（标量 / 缺失 → 物化为新对象）。
                // 跨-SDK 注意：Python 对应分支在父键缺失/标量时 `result[key] = value`，会把 DELETE
                // 哨兵原样塞进结果致后续 `json.dumps` 抛 `TypeError`（unset 不存在的嵌套键即崩）；本
                // 实现递归物化 → Delete 对缺失键为 noop、优雅产出（可能新建空对象）。安全行为由
                // `test_apply_write_nested_object_on_absent_parent_materializes` 锁定。
                let base = match result.get(key) {
                    Some(Value::Object(obj)) => obj.clone(),
                    _ => Map::new(),
                };
                result.insert(key.clone(), Value::Object(apply_write(&base, sub)));
            }
        }
    }
    result
}

// ---------------------------------------------------------------------------
// 文件加载 / File loading
// ---------------------------------------------------------------------------
/// 读取并字段级容错校验单个 settings 文件 / Load + field-level-validate one settings file。
///
/// 缺失文件 → `({}, [])`；JSON 损坏 → `({}, [error(<file>)])`（整文件回退空、**不**备份、**不**清盘，
/// 照 CC「invalid file 不用但留盘」，§5.6）；正常 → [`validate_settings`] 结果。
pub fn load_settings_file(
    path: &Path,
    scope: SettingsScope,
) -> (Map<String, Value>, Vec<SettingsValidationError>) {
    if !path.exists() {
        return (Map::new(), Vec::new());
    }
    let source = path.to_string_lossy();
    let raw_text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(exc) => {
            tracing::warn!(path = %source, error = %exc, "Settings file unreadable, ignored (kept on disk)");
            return (
                Map::new(),
                vec![SettingsValidationError {
                    scope,
                    field: "<file>".to_string(),
                    reason: format!("unreadable: {exc}"),
                    source_path: Some(source.into_owned()),
                }],
            );
        }
    };
    match serde_json::from_str::<Value>(&raw_text) {
        Ok(raw) => validate_settings(&raw, scope, Some(&source)),
        Err(exc) => {
            tracing::warn!(path = %source, error = %exc, "Settings file corrupt JSON, ignored (kept on disk)");
            (
                Map::new(),
                vec![SettingsValidationError {
                    scope,
                    field: "<file>".to_string(),
                    reason: format!("corrupt JSON: {exc}"),
                    source_path: Some(source.into_owned()),
                }],
            )
        }
    }
}

// ---------------------------------------------------------------------------
// 解析编排 / Resolution orchestration
// ---------------------------------------------------------------------------
/// 解析结果 / The resolved settings result。
///
/// `settings` 为六层合并后的最终视图；`errors` 汇总各层字段级校验错误（不阻断启动，供 `settings show`
/// / 诊断命令呈现，§5.6），已按出现顺序去重。
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedSettings {
    /// 六层合并后的最终视图 / the merged view。
    pub settings: Map<String, Value>,
    /// 去重后的字段级校验错误 / deduped field-level validation errors。
    pub errors: Vec<SettingsValidationError>,
}

/// 按出现顺序去重错误 / Order-preserving dedup。
///
/// #98 后各层各读一份独立文件，天然不再产生重复错误；此去重现为**防御性**保留（对齐
/// python-sdk#116 `_dedup_errors` 的处置）。
fn dedup_errors(errors: Vec<SettingsValidationError>) -> Vec<SettingsValidationError> {
    let mut out: Vec<SettingsValidationError> = Vec::with_capacity(errors.len());
    for e in errors {
        if !out.contains(&e) {
            out.push(e);
        }
    }
    out
}

/// 解析 project/local 锚定目录：注入 `cwd` 优先，否则进程 cwd / Resolve the project/local anchor dir。
///
/// #98：镜像本模块 `env` 注入范式。`None`（生产）→ `std::env::current_dir()`（SDK 本地部署行为）；
/// 进程 cwd 不可读 → `None`（调用方将 project/local 判空，不 panic）。`Some(p)`（测试注入）→ 原样。
pub(crate) fn resolve_cwd(cwd: Option<&Path>) -> Option<PathBuf> {
    match cwd {
        Some(p) => Some(p.to_path_buf()),
        None => std::env::current_dir().ok(),
    }
}

/// [`resolve_settings`] 的入参（镜像 Python keyword-only 参数）/ Inputs for [`resolve_settings`]。
///
/// 用 `..Default::default()` 省略缺省字段：
/// `resolve_settings(ResolveSettingsArgs { cwd: Some(&dir), ..Default::default() })`。
#[derive(Debug, Default)]
pub struct ResolveSettingsArgs<'a> {
    /// project/local 锚定的工作目录；`None` → 进程 cwd（`std::env::current_dir()`）/ project/local anchor,
    /// `None` → process cwd。#98：`Computer` 不再持有 workspace，project/local 锚定进程 cwd（SDK 本地部署
    /// 行为）。此字段是**测试注入接缝**（镜像 `env` 语义）；生产恒传 `None`。它**不是** workspace 概念——
    /// 无存储、非可选“空闲”态，恒解析到一个真实目录。
    pub cwd: Option<&'a Path>,
    /// 环境映射（解析 user config dir），`None` → 进程环境 / env mapping, `None` → process env。
    pub env: Option<&'a EnvMap>,
    /// `--settings <file>` 指定文件 / the `--settings` flag file, if any。
    pub flag_settings_path: Option<&'a Path>,
    /// policy scope 原始视图（来自 policy first-source-wins 结果，在此按 policy scope 校验）。
    pub policy_settings: Option<&'a Map<String, Value>>,
}

/// 解析五层 settings 视图（user / project / local / flag / policy）/ Resolve the five-layer view。
///
/// #98（对齐 protocol#10 / python-sdk#116）：能力发现层已移除。project/local **无条件**锚定进程 cwd
/// （`cwd` 注入接缝，`None` → `std::env::current_dir()`；cwd 不可读则该两层判空——不 panic）：读
/// `<cwd>/.tfrobot/settings[.local].json`。`enabledPlugins` / `extraKnownMarketplaces` 经常规
/// project/local 层进入，无需专门的能力并集层。优先级低→高 = user < project < local < flag < policy。
pub fn resolve_settings(args: ResolveSettingsArgs) -> ResolvedSettings {
    let ResolveSettingsArgs {
        cwd,
        env,
        flag_settings_path,
        policy_settings,
    } = args;

    let mut errors: Vec<SettingsValidationError> = Vec::new();

    // user（主）/ user (primary)。
    let (user_layer, user_errors) =
        load_settings_file(&user_settings_path(env), SettingsScope::User);
    errors.extend(user_errors);

    // project/local：无条件锚定进程 cwd（cwd 不可读 → 两层判空）/ anchored to process cwd。
    let (project_layer, local_layer) = match resolve_cwd(cwd) {
        Some(base) => {
            let (project_layer, proj_errors) = load_settings_file(
                &workdir_project_settings_path(&base),
                SettingsScope::Project,
            );
            let (local_layer, local_errors) =
                load_settings_file(&workdir_local_settings_path(&base), SettingsScope::Local);
            errors.extend(proj_errors);
            errors.extend(local_errors);
            (project_layer, local_layer)
        }
        None => (Map::new(), Map::new()),
    };

    // flag（`--settings`）/ flag。
    let flag_layer = match flag_settings_path {
        Some(path) => {
            let (flag_layer, flag_errors) = load_settings_file(path, SettingsScope::Flag);
            errors.extend(flag_errors);
            flag_layer
        }
        None => Map::new(),
    };

    // policy（最高）/ policy (highest) — 在此按 policy scope 校验 first-source-wins 结果。
    let empty_policy = Map::new();
    let policy_raw = policy_settings.unwrap_or(&empty_policy);
    let (policy_layer, policy_errors) = validate_settings(
        &Value::Object(policy_raw.clone()),
        SettingsScope::Policy,
        None,
    );
    errors.extend(policy_errors);

    let merged = merge_layers(&[
        user_layer,
        project_layer,
        local_layer,
        flag_layer,
        policy_layer,
    ]);

    ResolvedSettings {
        settings: merged,
        errors: dedup_errors(errors),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;
    use tempfile::TempDir;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    fn write_json(path: &Path, value: &Value) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_string(value).unwrap()).unwrap();
    }

    /// 指向无 settings.json 的隔离 config dir + 隔离 HOME，确保 user 层为空、不触进程环境。
    fn empty_user_env(tmp: &Path) -> EnvMap {
        let mut env = HashMap::new();
        env.insert(
            XDG_CONFIG_HOME_ENV.to_string(),
            tmp.join("empty-config").to_string_lossy().into_owned(),
        );
        env.insert(
            HOME_ENV.to_string(),
            tmp.join("empty-home").to_string_lossy().into_owned(),
        );
        env
    }

    // ---- 读 merge 矩阵 / read-merge matrix ----
    #[test]
    fn test_read_scalar_high_wins() {
        let merged = merge_read(
            &json!({"strictKnownMarketplaces": false}),
            &json!({"strictKnownMarketplaces": true}),
        );
        assert_eq!(merged, json!({"strictKnownMarketplaces": true}));
    }

    #[test]
    fn test_read_array_concat_dedup_low_first() {
        let merged = merge_read(
            &json!({"trustedMarketplaces": ["a", "b"]}),
            &json!({"trustedMarketplaces": ["b", "c"]}),
        );
        assert_eq!(merged, json!({"trustedMarketplaces": ["a", "b", "c"]}));
    }

    #[test]
    fn test_read_object_leaf_override_keeps_siblings() {
        // local false 覆盖 project true；project 独有 bar 保留。
        let merged = merge_read(
            &json!({"enabledPlugins": {"foo@mp": true, "bar@mp": true}}),
            &json!({"enabledPlugins": {"foo@mp": false}}),
        );
        assert_eq!(
            merged,
            json!({"enabledPlugins": {"foo@mp": false, "bar@mp": true}})
        );
    }

    #[test]
    fn test_read_nested_object_deep_merge_source_preserved() {
        // 关键用例：高 scope 只给 autoUpdate，嵌套 source 不被整体替换。
        let merged = merge_read(
            &json!({"extraKnownMarketplaces": {"mp": {"source": {"type": "git", "url": "u"}, "autoUpdate": false}}}),
            &json!({"extraKnownMarketplaces": {"mp": {"autoUpdate": true}}}),
        );
        assert_eq!(
            merged,
            json!({"extraKnownMarketplaces": {"mp": {"source": {"type": "git", "url": "u"}, "autoUpdate": true}}})
        );
    }

    #[test]
    fn test_read_key_only_in_low_preserved() {
        assert_eq!(
            merge_read(&json!({"a": 1}), &json!({"b": 2})),
            json!({"a": 1, "b": 2})
        );
    }

    #[test]
    fn test_read_type_mismatch_high_wins() {
        // 类型不一致（low=object/high=scalar、low=array/high=object）→ 高 scope 整体覆盖。
        assert_eq!(
            merge_read(&json!({"x": {"a": 1}}), &json!({"x": 5})),
            json!({"x": 5})
        );
        assert_eq!(
            merge_read(&json!({"x": [1, 2]}), &json!({"x": {"k": 1}})),
            json!({"x": {"k": 1}})
        );
    }

    #[test]
    fn test_merge_layers_fold_low_to_high() {
        let layers = [
            obj(json!({"strictKnownMarketplaces": false, "trustedMarketplaces": ["a"]})),
            obj(json!({"strictKnownMarketplaces": true, "trustedMarketplaces": ["b"]})),
            obj(json!({"trustedMarketplaces": ["c"]})),
        ];
        let merged = merge_layers(&layers);
        assert_eq!(merged["strictKnownMarketplaces"], json!(true));
        assert_eq!(merged["trustedMarketplaces"], json!(["a", "b", "c"]));
    }

    #[test]
    fn test_merge_read_deep_merges_and_concats_arrays() {
        // 深合并对象、数组低在前拼接去重、标量高覆盖（#98 后 merge_read 不再收集冲突）。
        let merged = merge_read(
            &json!({"x": 1, "arr": ["a"], "obj": {"k": 1, "keep": 9}}),
            &json!({"x": 2, "arr": ["b"], "obj": {"k": 2}}),
        );
        assert_eq!(merged["x"], json!(2)); // 标量高覆盖
        assert_eq!(merged["arr"], json!(["a", "b"])); // 数组拼接去重
        assert_eq!(merged["obj"], json!({"k": 2, "keep": 9})); // 对象深合并
    }

    // ---- 写 merge / write-merge ----
    #[test]
    fn test_write_array_replaced_not_concatenated() {
        let existing = obj(json!({"trustedMarketplaces": ["a", "b"]}));
        let updates: BTreeMap<String, WriteValue> = [(
            "trustedMarketplaces".to_string(),
            WriteValue::Set(json!(["c"])),
        )]
        .into_iter()
        .collect();
        assert_eq!(
            Value::Object(apply_write(&existing, &updates)),
            json!({"trustedMarketplaces": ["c"]})
        );
    }

    #[test]
    fn test_write_delete_sentinel_removes_key() {
        let existing = obj(json!({"strictKnownMarketplaces": true, "trustedMarketplaces": ["a"]}));
        let updates: BTreeMap<String, WriteValue> =
            [("strictKnownMarketplaces".to_string(), WriteValue::Delete)]
                .into_iter()
                .collect();
        assert_eq!(
            Value::Object(apply_write(&existing, &updates)),
            json!({"trustedMarketplaces": ["a"]})
        );
    }

    #[test]
    fn test_write_delete_missing_key_is_noop() {
        let existing = obj(json!({"a": 1}));
        let updates: BTreeMap<String, WriteValue> = [("missing".to_string(), WriteValue::Delete)]
            .into_iter()
            .collect();
        assert_eq!(
            Value::Object(apply_write(&existing, &updates)),
            json!({"a": 1})
        );
    }

    #[test]
    fn test_write_nested_object_keeps_siblings() {
        let existing = obj(json!({"enabledPlugins": {"foo@mp": true, "bar@mp": true}}));
        let sub: BTreeMap<String, WriteValue> =
            [("foo@mp".to_string(), WriteValue::Set(json!(false)))]
                .into_iter()
                .collect();
        let updates: BTreeMap<String, WriteValue> =
            [("enabledPlugins".to_string(), WriteValue::Object(sub))]
                .into_iter()
                .collect();
        assert_eq!(
            Value::Object(apply_write(&existing, &updates)),
            json!({"enabledPlugins": {"foo@mp": false, "bar@mp": true}})
        );
    }

    #[test]
    fn test_write_nested_delete_removes_leaf_keeps_siblings() {
        // From<Value> 不能表达删除，调用方显式 Delete：unset enabledPlugins.foo@mp。
        let existing = obj(json!({"enabledPlugins": {"foo@mp": true, "bar@mp": true}}));
        let sub: BTreeMap<String, WriteValue> = [("foo@mp".to_string(), WriteValue::Delete)]
            .into_iter()
            .collect();
        let updates: BTreeMap<String, WriteValue> =
            [("enabledPlugins".to_string(), WriteValue::Object(sub))]
                .into_iter()
                .collect();
        assert_eq!(
            Value::Object(apply_write(&existing, &updates)),
            json!({"enabledPlugins": {"bar@mp": true}})
        );
    }

    #[test]
    fn test_write_does_not_mutate_inputs() {
        let existing = obj(json!({"trustedMarketplaces": ["a"]}));
        let updates: BTreeMap<String, WriteValue> = [(
            "trustedMarketplaces".to_string(),
            WriteValue::Set(json!(["c"])),
        )]
        .into_iter()
        .collect();
        apply_write(&existing, &updates);
        assert_eq!(
            Value::Object(existing),
            json!({"trustedMarketplaces": ["a"]})
        ); // 原对象不变
    }

    #[test]
    fn test_write_value_from_object_recurses() {
        // From<Value>：对象 → 递归 Object（深合并保留兄弟），标量 → Set。
        assert_eq!(
            WriteValue::from(json!(["c"])),
            WriteValue::Set(json!(["c"]))
        );
        let existing = obj(json!({"permissions": {"a": 1, "b": 2}}));
        let updates: BTreeMap<String, WriteValue> =
            [("permissions".to_string(), WriteValue::from(json!({"a": 9})))]
                .into_iter()
                .collect();
        assert_eq!(
            Value::Object(apply_write(&existing, &updates)),
            json!({"permissions": {"a": 9, "b": 2}})
        );
    }

    #[test]
    fn test_apply_write_nested_object_on_absent_parent_materializes() {
        // 父键缺失时写嵌套 Object：递归从空对象起；对缺失叶子的 Delete = noop → 物化空对象（不崩）。
        // 对标 Python 此场景会把 DELETE 哨兵塞进结果致 json.dumps 抛 TypeError——本实现优雅产出。
        let existing = obj(json!({"strictKnownMarketplaces": true}));
        let sub: BTreeMap<String, WriteValue> = [("foo@mp".to_string(), WriteValue::Delete)]
            .into_iter()
            .collect();
        let updates: BTreeMap<String, WriteValue> =
            [("enabledPlugins".to_string(), WriteValue::Object(sub))]
                .into_iter()
                .collect();
        assert_eq!(
            Value::Object(apply_write(&existing, &updates)),
            json!({"strictKnownMarketplaces": true, "enabledPlugins": {}})
        );
    }

    // ---- 路径解析 / path resolution ----
    #[test]
    fn test_user_config_dir_xdg_first() {
        let tmp = TempDir::new().unwrap();
        let xdg = tmp.path().join("xdg-config");
        let mut env = HashMap::new();
        env.insert(
            XDG_CONFIG_HOME_ENV.to_string(),
            xdg.to_string_lossy().into_owned(),
        );
        assert_eq!(resolve_user_config_dir(Some(&env)), xdg.join("a2c"));
    }

    #[test]
    fn test_user_config_dir_fallback_home() {
        let tmp = TempDir::new().unwrap();
        let mut env = HashMap::new();
        // 相对 XDG_CONFIG_HOME 按规范忽略 → 回退 <HOME>/.config/a2c。
        env.insert(XDG_CONFIG_HOME_ENV.to_string(), "relative".to_string());
        env.insert(
            HOME_ENV.to_string(),
            tmp.path().to_string_lossy().into_owned(),
        );
        assert_eq!(
            resolve_user_config_dir(Some(&env)),
            tmp.path().join(".config").join("a2c")
        );
    }

    #[test]
    fn test_user_settings_path() {
        let tmp = TempDir::new().unwrap();
        let xdg = tmp.path().join("c");
        let mut env = HashMap::new();
        env.insert(
            XDG_CONFIG_HOME_ENV.to_string(),
            xdg.to_string_lossy().into_owned(),
        );
        assert_eq!(
            user_settings_path(Some(&env)),
            xdg.join("a2c").join("settings.json")
        );
    }

    #[test]
    fn test_workdir_paths() {
        let wd = Path::new("/some/workdir");
        assert_eq!(
            workdir_project_settings_path(wd),
            PathBuf::from("/some/workdir/.tfrobot/settings.json")
        );
        assert_eq!(
            workdir_local_settings_path(wd),
            PathBuf::from("/some/workdir/.tfrobot/settings.local.json")
        );
    }

    // ---- cwd-anchored 解析 / resolution（#98：能力层已移除，project/local 锚定进程 cwd）----
    #[test]
    fn test_resolve_settings_anchors_project_local_at_cwd() {
        // project/local 锚定注入 cwd；local 覆盖 project；能力字段（enabledPlugins）经常规 project 层进入，
        // 无需专门的能力并集层（对齐 python-sdk#116）。
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        write_json(
            &workdir_project_settings_path(&wd),
            &json!({"strictKnownMarketplaces": true, "enabledPlugins": {"p@mp": true}}),
        );
        write_json(
            &workdir_local_settings_path(&wd),
            &json!({"strictKnownMarketplaces": false, "trustedMarketplaces": ["m"]}),
        );
        let env = empty_user_env(tmp.path());
        let resolved = resolve_settings(ResolveSettingsArgs {
            cwd: Some(&wd),
            env: Some(&env),
            ..Default::default()
        });
        // local 覆盖 project（高 scope 赢）。
        assert_eq!(resolved.settings["strictKnownMarketplaces"], json!(false));
        assert_eq!(resolved.settings["trustedMarketplaces"], json!(["m"]));
        // 能力字段经 project 层进入（无专门能力层）。
        assert_eq!(resolved.settings["enabledPlugins"], json!({"p@mp": true}));
    }

    #[test]
    fn test_extra_known_marketplaces_deep_merge_at_cwd() {
        // extraKnownMarketplaces（dict）在 cwd 的 project/local 层深合并：键并集，同名叶子高层胜。
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        write_json(
            &workdir_project_settings_path(&wd),
            &json!({"extraKnownMarketplaces": {"mp1": {"source": {"type": "git", "url": "git@h:a.git"}}}}),
        );
        write_json(
            &workdir_local_settings_path(&wd),
            &json!({"extraKnownMarketplaces": {"mp2": {"source": {"type": "git", "url": "git@h:b.git"}}}}),
        );
        let env = empty_user_env(tmp.path());
        let resolved = resolve_settings(ResolveSettingsArgs {
            cwd: Some(&wd),
            env: Some(&env),
            ..Default::default()
        });
        let names: std::collections::BTreeSet<&str> = resolved.settings["extraKnownMarketplaces"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(names, ["mp1", "mp2"].into_iter().collect());
    }

    #[test]
    fn test_policy_highest_priority_and_validated() {
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        write_json(
            &workdir_project_settings_path(&wd),
            &json!({"strictKnownMarketplaces": false}),
        );
        let env = empty_user_env(tmp.path());
        let policy = obj(json!({"strictKnownMarketplaces": true, "allowedMcpServers": ["srv"]}));
        let resolved = resolve_settings(ResolveSettingsArgs {
            cwd: Some(&wd),
            env: Some(&env),
            policy_settings: Some(&policy),
            ..Default::default()
        });
        // policy 最高优先级覆盖 project。
        assert_eq!(resolved.settings["strictKnownMarketplaces"], json!(true));
        // policy-only 字段在 policy scope 合法保留。
        assert_eq!(resolved.settings["allowedMcpServers"], json!(["srv"]));
    }

    #[test]
    fn test_policy_only_field_dropped_outside_policy_scope() {
        // POLICY_ONLY 字段仅 policy layer 生效：project 写 allowedMcpServers 在 SET-01 校验阶段被丢。
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        write_json(
            &workdir_project_settings_path(&wd),
            &json!({"allowedMcpServers": ["leak"]}),
        );
        let env = empty_user_env(tmp.path());
        let resolved = resolve_settings(ResolveSettingsArgs {
            cwd: Some(&wd),
            env: Some(&env),
            ..Default::default()
        });
        assert!(!resolved.settings.contains_key("allowedMcpServers"));
        assert!(resolved
            .errors
            .iter()
            .any(|e| e.field == "allowedMcpServers"));
    }

    #[test]
    fn test_corrupt_settings_file_yields_error_not_crash() {
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        let path = workdir_project_settings_path(&wd);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{not valid json").unwrap();
        let env = empty_user_env(tmp.path());
        let resolved = resolve_settings(ResolveSettingsArgs {
            cwd: Some(&wd),
            env: Some(&env),
            ..Default::default()
        });
        assert!(resolved.settings.is_empty()); // 损坏文件回退空、不崩
        assert!(resolved.errors.iter().any(|e| e.field == "<file>"));
    }

    #[test]
    #[cfg(unix)]
    fn test_load_settings_file_unreadable_yields_file_error() {
        // 路径存在但 read_to_string 失败（用目录冒充文件）→ <file> 错误、回退空、不崩。
        // 覆盖「不可读但非 corrupt JSON」早退分支；Windows 行为不同，故 unix 门控。
        let tmp = TempDir::new().unwrap();
        let dir_as_file = tmp.path().join("settings.json");
        std::fs::create_dir_all(&dir_as_file).unwrap();
        let (cleaned, errors) = load_settings_file(&dir_as_file, SettingsScope::User);
        assert!(cleaned.is_empty());
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].field, "<file>");
    }

    #[test]
    fn test_flag_layer_overrides_user_and_project() {
        // flag 层（--settings <file>）优先级高于 project，仅低于 policy。
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        write_json(
            &workdir_project_settings_path(&wd),
            &json!({"strictKnownMarketplaces": false, "trustedMarketplaces": ["proj"]}),
        );
        let flag_file = tmp.path().join("flag-settings.json");
        std::fs::write(
            &flag_file,
            serde_json::to_string(
                &json!({"strictKnownMarketplaces": true, "trustedMarketplaces": ["flag"]}),
            )
            .unwrap(),
        )
        .unwrap();
        let env = empty_user_env(tmp.path());
        let resolved = resolve_settings(ResolveSettingsArgs {
            cwd: Some(&wd),
            env: Some(&env),
            flag_settings_path: Some(&flag_file),
            ..Default::default()
        });
        assert_eq!(resolved.settings["strictKnownMarketplaces"], json!(true)); // flag 标量覆盖 project
                                                                               // 数组拼接去重（低 project 在前 + 高 flag 在后）。
        assert_eq!(
            resolved.settings["trustedMarketplaces"],
            json!(["proj", "flag"])
        );
    }

    #[test]
    fn test_apply_write_round_trip_through_resolve() {
        // apply_write 写回 project 文件 → resolve 读回一致；DELETE 写入能删键。
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        let existing =
            obj(json!({"strictKnownMarketplaces": false, "trustedMarketplaces": ["old"]}));
        let updates: BTreeMap<String, WriteValue> = [
            ("strictKnownMarketplaces".to_string(), WriteValue::Delete),
            (
                "trustedMarketplaces".to_string(),
                WriteValue::Set(json!(["new"])),
            ),
        ]
        .into_iter()
        .collect();
        let written = apply_write(&existing, &updates);
        write_json(&workdir_project_settings_path(&wd), &Value::Object(written));
        let env = empty_user_env(tmp.path());
        let resolved = resolve_settings(ResolveSettingsArgs {
            cwd: Some(&wd),
            env: Some(&env),
            ..Default::default()
        });
        assert!(!resolved.settings.contains_key("strictKnownMarketplaces")); // DELETE 写入后键消失
        assert_eq!(resolved.settings["trustedMarketplaces"], json!(["new"])); // Set 替换生效
    }
}
