/*!
* 文件名: executor.rs
* 作者: JQQ
* 创建日期: 2026/07/10
* 最后修改日期: 2026/07/10
* 版权: 2023 JQQ. All rights reserved.
* 依赖: serde_json, settings::{scope::{apply_write, WriteValue}, store::{with_settings_lock, atomic_write_settings_json}}, config::write_target
* 描述: #107 S3（#110）—— 写计划执行器：把 S2 `resolve_write_target` 产出的 `Vec<WritePlan>` 落盘。
*       Executor: applies the `Vec<WritePlan>` produced by S2 `resolve_write_target` to disk.
*
* 兑现 S2 文档化的**执行器契约**（write_target.rs 模块头）/ honors the executor contract documented in S2:
*   1. **no-change 跳过落盘**：`apply_write` 对**缺失父键**会物化空对象（如 `{"servers":{}}`），**不是**干净 noop。
*      故本执行器不做字节级 `updated == existing` 比较（那样仍会把 `{"servers":{}}` 写进从未声明该实体的 scope），
*      而是**语义比对**——剥离两侧的纯空对象脚手架后相等则**跳过写**。这样 Remove 的 fan-out 不会在从未声明
*      该实体的 scope 凭空建空 `{"servers":{}}` 文件。
*   2. **StringSetInsert/Remove = 读-改-写去重**：insert 去重（成员已在 → noop）、对缺失字段 insert 则新建数组；
*      remove 对缺失成员/缺失字段为 noop。二者精确判 noop（无需脚手架剥离）。
*
* 原子性 / atomicity：每条 plan 在**目标文件旁车锁**下做 read→compute→(skip|write)（复用 store 的
* `with_settings_lock` + `atomic_write_settings_json`，与 installer/mcp_config 门控同一套 RMW 原语）。
* 同批多条 plan 命中同一文件时**顺序**执行——后者持锁重读，见前者已落盘的结果。
*
* 读**原始**文件（非 `load_settings_file` 校验视图）：写回必须逐字节保真原有内容，校验会在写回时**剥字段**
* （如 policy-only 字段、未知键）反致数据损坏。目标文件存在但 JSON 损坏 → `CorruptTarget` 硬错（**不覆盖**、
* 不静默清盘），对齐「覆盖前先看目标」的安全原则。
*/

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::super::scope::{apply_write, WriteValue};
use super::super::store::{atomic_write_settings_json, with_settings_lock};
use super::write_target::{WritePlan, WriteTargetOp};

/// 执行错误（结构化）/ structured executor errors。
#[derive(Debug, Clone, PartialEq)]
pub enum ExecutorError {
    /// 拿不到目标文件旁车锁 / could not acquire the sidecar lock。
    Lock {
        /// 目标文件。
        file: PathBuf,
        /// 底层原因。
        reason: String,
    },
    /// 读/写 I/O 失败 / read or write I/O failure。
    Io {
        /// 目标文件。
        file: PathBuf,
        /// 底层原因。
        reason: String,
    },
    /// 目标文件存在但非合法 JSON object → 拒绝覆盖（保护用户数据）/ target exists but is not a JSON object。
    CorruptTarget {
        /// 目标文件。
        file: PathBuf,
        /// 底层原因。
        reason: String,
    },
    /// 内部不变量违反（`WriteTargetOp::Value` 顶层非 Object）/ internal invariant violated。
    Internal {
        /// 说明。
        reason: String,
    },
}

impl std::fmt::Display for ExecutorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecutorError::Lock { file, reason } => {
                write!(f, "could not lock {}: {reason}", file.display())
            }
            ExecutorError::Io { file, reason } => {
                write!(f, "I/O error on {}: {reason}", file.display())
            }
            ExecutorError::CorruptTarget { file, reason } => {
                write!(
                    f,
                    "refusing to overwrite corrupt target {}: {reason}",
                    file.display()
                )
            }
            ExecutorError::Internal { reason } => {
                write!(f, "executor invariant violated: {reason}")
            }
        }
    }
}

impl std::error::Error for ExecutorError {}

/// 落盘一批写计划（顺序、各自持锁 RMW）/ apply a batch of write plans (sequential, per-file locked RMW).
///
/// 任一条失败即返回错误、**不回滚**已落盘的前序条目（调用方 `update_config` 先全量消解、后统一执行，
/// 已把「消解期错误」前移到不落盘阶段；执行期错误仅剩 I/O / 锁 / 损坏文件这类外部故障）。
pub fn execute_write_plans(plans: &[WritePlan]) -> Result<(), ExecutorError> {
    // pre-flight：落盘前先**只读**探测每个目标文件（corrupt / IO），任一失败即零落盘返回——把可预检故障
    // （尤其损坏的既有目标）挡在**首次写之前**，显著收窄「顺序多文件写、后序失败不回滚前序」的半落盘窗口。
    // 非真原子性（best-effort 早探测）：探测与实际写之间文件仍可能被外部改动，故 `execute_one` 内持锁重读为准。
    for plan in plans {
        read_raw_object(plan.file.as_path())?;
    }
    for plan in plans {
        execute_one(plan)?;
    }
    Ok(())
}

fn execute_one(plan: &WritePlan) -> Result<(), ExecutorError> {
    let path = plan.file.as_path();
    with_settings_lock(path, || -> Result<(), ExecutorError> {
        let existing = read_raw_object(path)?;
        let next = match &plan.op {
            WriteTargetOp::Value(wv) => apply_value_op(&existing, wv)?,
            WriteTargetOp::StringSetInsert { field, value } => {
                string_set_insert(&existing, field, value)
            }
            WriteTargetOp::StringSetRemove { field, value } => {
                string_set_remove(&existing, field, value)
            }
        };
        match next {
            None => Ok(()), // no-change → 不落盘
            Some(updated) => {
                atomic_write_settings_json(path, &Value::Object(updated)).map_err(|e| {
                    ExecutorError::Io {
                        file: path.to_path_buf(),
                        reason: e.to_string(),
                    }
                })
            }
        }
    })
    .map_err(|e| ExecutorError::Lock {
        file: path.to_path_buf(),
        reason: e.to_string(),
    })?
}

/// 读**原始** JSON object（无校验）：缺失/空 → 空 map；损坏/非 object → `CorruptTarget`（不覆盖）。
pub(crate) fn read_raw_object(path: &Path) -> Result<Map<String, Value>, ExecutorError> {
    if !path.exists() {
        return Ok(Map::new());
    }
    let text = std::fs::read_to_string(path).map_err(|e| ExecutorError::Io {
        file: path.to_path_buf(),
        reason: e.to_string(),
    })?;
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    let value: Value = serde_json::from_str(&text).map_err(|e| ExecutorError::CorruptTarget {
        file: path.to_path_buf(),
        reason: e.to_string(),
    })?;
    match value {
        Value::Object(map) => Ok(map),
        _ => Err(ExecutorError::CorruptTarget {
            file: path.to_path_buf(),
            reason: "top-level JSON is not an object".to_string(),
        }),
    }
}

/// `WriteTargetOp::Value`：`apply_write` 后**语义比对**（剥空对象脚手架）判 no-change。
///
/// 顶层 `WriteValue` 由 S2 恒构造为 `Object`（`{servers:{name:leaf}}` / `{enabledPlugins:{id:leaf}}`）；
/// 非 Object 属内部不变量违反。
fn apply_value_op(
    existing: &Map<String, Value>,
    wv: &WriteValue,
) -> Result<Option<Map<String, Value>>, ExecutorError> {
    let updates: BTreeMap<String, WriteValue> = match wv {
        WriteValue::Object(map) => map.clone(),
        _ => {
            return Err(ExecutorError::Internal {
                reason: "top-level WriteTargetOp::Value must be a WriteValue::Object".to_string(),
            })
        }
    };
    let updated = apply_write(existing, &updates);
    // 精确 no-change 判定：`updated` 相对 `existing` 是否**仅多出本次写新物化的空对象脚手架**。
    // （字节级 `updated == existing` 不够——`apply_write` 会把缺失父键物化成 `{"servers":{}}`；而对称地
    //  剥两侧空对象又会误跳真实删除，例如磁盘存 `{"servers":{"a":{}}}` 时 `Remove("a")` 应删却被判等。）
    if is_no_change(&updated, existing) {
        Ok(None)
    } else {
        Ok(Some(updated))
    }
}

/// 向字符串数组字段插入成员（去重）；已在 → `None`（noop）/ dedup insert。
fn string_set_insert(
    existing: &Map<String, Value>,
    field: &str,
    value: &str,
) -> Option<Map<String, Value>> {
    let target = Value::String(value.to_string());
    let mut arr: Vec<Value> = existing
        .get(field)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if arr.contains(&target) {
        return None; // 已在 → noop
    }
    arr.push(target);
    let mut updated = existing.clone();
    updated.insert(field.to_string(), Value::Array(arr));
    Some(updated)
}

/// 从字符串数组字段移除成员；字段/成员缺失 → `None`（noop）/ noop-on-missing remove。
fn string_set_remove(
    existing: &Map<String, Value>,
    field: &str,
    value: &str,
) -> Option<Map<String, Value>> {
    let target = Value::String(value.to_string());
    let arr = existing.get(field).and_then(Value::as_array)?; // 字段缺失 → noop
    if !arr.contains(&target) {
        return None; // 成员缺失 → noop
    }
    let filtered: Vec<Value> = arr.iter().filter(|x| **x != target).cloned().collect();
    let mut updated = existing.clone();
    updated.insert(field.to_string(), Value::Array(filtered));
    Some(updated)
}

/// no-change 判定：剥掉 `updated` 中**本次写新物化的空对象脚手架**后是否等于 `existing`。
///
/// 「新物化脚手架」= 某键在 `updated` 递归为空对象、且其对应键在 `existing` **缺失或非对象**——即
/// `apply_write` 对缺失父键凭空造出的 `{"servers":{}}`。这类键剥掉后若与 `existing` 相等 → 无实质变化。
/// 反之，`existing` 里**已存在**的（哪怕是空对象值的）键**绝不剥**——保证「磁盘已声明该实体」时的删除/改写
/// 被如实判为变化（修复对称剥离会误跳真实删除的缺陷）。
fn is_no_change(updated: &Map<String, Value>, existing: &Map<String, Value>) -> bool {
    strip_fresh_scaffold(updated, existing) == *existing
}

/// 从 `updated` 剥去「对应键在 `existing` 缺失/非对象、且自身递归为空」的空对象键 / strip freshly-materialized scaffolding。
fn strip_fresh_scaffold(
    updated: &Map<String, Value>,
    existing: &Map<String, Value>,
) -> Map<String, Value> {
    let empty = Map::new();
    let mut out = Map::new();
    for (k, uv) in updated {
        match uv {
            Value::Object(uo) => {
                let eo = match existing.get(k) {
                    Some(Value::Object(m)) => Some(m),
                    _ => None,
                };
                let stripped = strip_fresh_scaffold(uo, eo.unwrap_or(&empty));
                // 仅当「该键在 existing 缺失/非对象」且「剥后为空」→ 本次新物化脚手架、丢弃；否则保留。
                if stripped.is_empty() && eo.is_none() {
                    continue;
                }
                out.insert(k.clone(), Value::Object(stripped));
            }
            other => {
                out.insert(k.clone(), other.clone());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::mcp_config::{
        user_mcp_config_path, workdir_mcp_config_path, workdir_mcp_local_config_path,
    };
    use crate::settings::scope::{workdir_local_settings_path, EnvMap};
    use serde_json::json;
    use std::path::Path;
    use tempfile::TempDir;

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    fn read_json(path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn xdg_env(tmp: &TempDir) -> EnvMap {
        std::iter::once((
            "XDG_CONFIG_HOME".to_string(),
            tmp.path().join("xdg").to_string_lossy().into_owned(),
        ))
        .collect()
    }

    /// `{servers: {name: Set(cfg)}}` 顶层写 / a top-level server upsert op。
    fn server_set(name: &str, cfg: Value) -> WriteTargetOp {
        let inner: BTreeMap<String, WriteValue> =
            std::iter::once((name.to_string(), WriteValue::Set(cfg))).collect();
        let servers: BTreeMap<String, WriteValue> =
            std::iter::once(("servers".to_string(), WriteValue::Object(inner))).collect();
        WriteTargetOp::Value(WriteValue::Object(servers))
    }

    /// `{servers: {name: Delete}}` 顶层删 / a top-level server delete op。
    fn server_delete(name: &str) -> WriteTargetOp {
        let inner: BTreeMap<String, WriteValue> =
            std::iter::once((name.to_string(), WriteValue::Delete)).collect();
        let servers: BTreeMap<String, WriteValue> =
            std::iter::once(("servers".to_string(), WriteValue::Object(inner))).collect();
        WriteTargetOp::Value(WriteValue::Object(servers))
    }

    #[test]
    fn value_upsert_creates_then_idempotent_skip() {
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        let path = workdir_mcp_config_path(&wd);
        let plan = WritePlan {
            scope: crate::settings::schema::SettingsScope::Project,
            file: path.clone(),
            op: server_set("srv", json!({"type": "stdio"})),
        };
        // 首次：文件不存在 → 新建。
        execute_write_plans(std::slice::from_ref(&plan)).unwrap();
        assert_eq!(
            read_json(&path),
            json!({"servers": {"srv": {"type": "stdio"}}})
        );
        // 记录 mtime，再次执行相同 plan → 语义无变化 → 跳过写（文件不被触碰）。
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        execute_write_plans(std::slice::from_ref(&plan)).unwrap();
        let after = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(before, after, "幂等 upsert 应跳过写、不触碰文件");
    }

    #[test]
    fn remove_absent_scope_does_not_create_empty_scaffold_file() {
        // 契约核心：Remove 的 fan-out 对**从未声明该实体**的 scope 不得凭空建空 `{"servers":{}}` 文件。
        let tmp = TempDir::new().unwrap();
        let env = xdg_env(&tmp);
        let wd = tmp.path().join("wd");
        // srv 只在 user scope 声明。
        write(
            &user_mcp_config_path(Some(&env)),
            r#"{"servers": {"srv": {"type": "stdio"}}}"#,
        );
        let project = workdir_mcp_config_path(&wd);
        let local = workdir_mcp_local_config_path(&wd);
        assert!(!project.exists() && !local.exists());
        // fan-out：user 删（真删）、project/local 删（从未声明 → 纯 noop）。
        let plans = vec![
            WritePlan {
                scope: crate::settings::schema::SettingsScope::User,
                file: user_mcp_config_path(Some(&env)),
                op: server_delete("srv"),
            },
            WritePlan {
                scope: crate::settings::schema::SettingsScope::Project,
                file: project.clone(),
                op: server_delete("srv"),
            },
            WritePlan {
                scope: crate::settings::schema::SettingsScope::Local,
                file: local.clone(),
                op: server_delete("srv"),
            },
        ];
        execute_write_plans(&plans).unwrap();
        // user：srv 已删。
        assert_eq!(
            read_json(&user_mcp_config_path(Some(&env))),
            json!({"servers": {}})
        );
        // project/local：从未声明 → 未凭空建文件。
        assert!(
            !project.exists(),
            "project 从未声明 srv → 不得建空脚手架文件"
        );
        assert!(!local.exists(), "local 从未声明 srv → 不得建空脚手架文件");
    }

    #[test]
    fn value_delete_existing_writes_and_preserves_siblings() {
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        let path = workdir_mcp_config_path(&wd);
        write(
            &path,
            r#"{"servers": {"a": {"type": "stdio"}, "b": {"type": "http"}}}"#,
        );
        let plan = WritePlan {
            scope: crate::settings::schema::SettingsScope::Project,
            file: path.clone(),
            op: server_delete("a"),
        };
        execute_write_plans(std::slice::from_ref(&plan)).unwrap();
        // a 删、兄弟 b 保留。
        assert_eq!(
            read_json(&path),
            json!({"servers": {"b": {"type": "http"}}})
        );
    }

    #[test]
    fn string_set_insert_dedups_and_creates_field() {
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        let path = workdir_local_settings_path(&wd);
        let plan = WritePlan {
            scope: crate::settings::schema::SettingsScope::Local,
            file: path.clone(),
            op: WriteTargetOp::StringSetInsert {
                field: "disabledMcpjsonServers".into(),
                value: "srv".into(),
            },
        };
        // 字段缺失 → 新建数组。
        execute_write_plans(std::slice::from_ref(&plan)).unwrap();
        assert_eq!(read_json(&path), json!({"disabledMcpjsonServers": ["srv"]}));
        // 再插同值 → 去重 → 跳过写（不触碰文件）。
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        execute_write_plans(std::slice::from_ref(&plan)).unwrap();
        let after = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(before, after, "重复 insert 去重后应跳过写");
    }

    #[test]
    fn string_set_remove_noop_on_missing_field_and_member() {
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        let path = workdir_local_settings_path(&wd);
        let plan = WritePlan {
            scope: crate::settings::schema::SettingsScope::Local,
            file: path.clone(),
            op: WriteTargetOp::StringSetRemove {
                field: "disabledMcpjsonServers".into(),
                value: "srv".into(),
            },
        };
        // 文件/字段皆缺失 → noop → 不建文件。
        execute_write_plans(std::slice::from_ref(&plan)).unwrap();
        assert!(!path.exists(), "字段缺失 remove → noop、不建文件");
        // 字段存在但成员缺失 → noop（不触碰）。
        write(&path, r#"{"disabledMcpjsonServers": ["other"]}"#);
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        execute_write_plans(std::slice::from_ref(&plan)).unwrap();
        let after = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(before, after, "成员缺失 remove → 跳过写");
        // 成员存在 → 移除并落盘。
        write(&path, r#"{"disabledMcpjsonServers": ["srv", "keep"]}"#);
        execute_write_plans(std::slice::from_ref(&plan)).unwrap();
        assert_eq!(
            read_json(&path),
            json!({"disabledMcpjsonServers": ["keep"]})
        );
    }

    #[test]
    fn corrupt_target_is_refused_not_clobbered() {
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        let path = workdir_mcp_config_path(&wd);
        write(&path, "{not valid json");
        let plan = WritePlan {
            scope: crate::settings::schema::SettingsScope::Project,
            file: path.clone(),
            op: server_set("srv", json!({"type": "stdio"})),
        };
        let err = execute_write_plans(std::slice::from_ref(&plan)).unwrap_err();
        assert!(matches!(err, ExecutorError::CorruptTarget { .. }));
        // 损坏文件保持原样，不被覆盖。
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{not valid json");
    }

    #[test]
    fn remove_server_with_empty_object_value_actually_deletes() {
        // 🟡-1 回归：磁盘存 `{"servers":{"a":{}}}`（server 值恰为空对象，手编可达），Remove("a") 必须真删——
        // 精确 no-change 判定不得因「apply_write 结果 `{"servers":{}}` 剥空后为 `{}`」而误判无变化跳过。
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        let path = workdir_mcp_config_path(&wd);
        write(&path, r#"{"servers": {"a": {}, "b": {"type": "http"}}}"#);
        let plan = WritePlan {
            scope: crate::settings::schema::SettingsScope::Project,
            file: path.clone(),
            op: server_delete("a"),
        };
        execute_write_plans(std::slice::from_ref(&plan)).unwrap();
        assert_eq!(
            read_json(&path),
            json!({"servers": {"b": {"type": "http"}}}),
            "空对象值的 server 也必须被真删（不得因空对象脚手架剥离而误跳过）"
        );
    }

    #[test]
    fn remove_ghost_when_servers_already_empty_skips_write() {
        // 精确 no-change 的对偶：磁盘已是 `{"servers":{}}`，Remove 不存在的 server → 结果同为 `{"servers":{}}`，
        // 无实质变化 → 跳过写（不得因 `existing` 含空对象 `servers` 而误判有变化去重写同内容）。
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        let path = workdir_mcp_config_path(&wd);
        write(&path, "{\n  \"servers\": {}\n}\n");
        let plan = WritePlan {
            scope: crate::settings::schema::SettingsScope::Project,
            file: path.clone(),
            op: server_delete("ghost"),
        };
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        execute_write_plans(std::slice::from_ref(&plan)).unwrap();
        let after = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(before, after, "结果无实质变化 → 应跳过写、不触碰文件");
    }

    #[test]
    fn preflight_corrupt_mid_batch_target_prevents_any_write() {
        // 🟡-2 回归：多文件批量写中，若后序目标损坏，pre-flight 应在**首次写之前**整批返错 → 前序目标零改动。
        let tmp = TempDir::new().unwrap();
        let env = xdg_env(&tmp);
        let wd = tmp.path().join("wd");
        // user 有效目标（含 srv）、project 目标损坏。
        write(
            &user_mcp_config_path(Some(&env)),
            r#"{"servers": {"srv": {"type": "stdio"}}}"#,
        );
        let project = workdir_mcp_config_path(&wd);
        write(&project, "{corrupt");
        let plans = vec![
            WritePlan {
                scope: crate::settings::schema::SettingsScope::User,
                file: user_mcp_config_path(Some(&env)),
                op: server_delete("srv"),
            },
            WritePlan {
                scope: crate::settings::schema::SettingsScope::Project,
                file: project.clone(),
                op: server_delete("srv"),
            },
        ];
        let err = execute_write_plans(&plans).unwrap_err();
        assert!(matches!(err, ExecutorError::CorruptTarget { .. }));
        // user 目标未被改动（pre-flight 在首次写前就拦截）。
        assert_eq!(
            read_json(&user_mcp_config_path(Some(&env))),
            json!({"servers": {"srv": {"type": "stdio"}}}),
            "pre-flight 应在任何落盘前拦截 → 前序有效目标零改动"
        );
    }
}
