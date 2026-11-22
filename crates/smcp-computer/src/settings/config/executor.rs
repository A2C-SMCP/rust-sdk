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
*   3. **project `readRoots` A+（#292）**：project settings 写入期**不剔除**该键——盘上既有键保留 + WARN；显式
*      Set/Delete 命中则**不写新值、保留旧键** + WARN；读面 trusted-scope-only 拒绝保持不动。
*   4. **目标容器畸形批前 fail-closed（#292）**：计划下钻的每层 object 容器与 string-set 字段须与盘上类型兼容
*      （缺失或类型匹配才写；非对象 / 非数组 → `CorruptTarget`，复用 `settings::raw_write` 的容器 guard）。
*
* 原子性 / atomicity：每条 plan 在**目标文件旁车锁**下做 read→compute→(skip|write)（复用 store 的
* `with_settings_lock` + `atomic_write_settings_json`，与 installer/mcp_config 门控同一套 RMW 原语）。
* 同批多条 plan 命中同一文件时**顺序**执行——后者持锁重读，见前者已落盘的结果。
*
* 读**原始**文件（非 `load_settings_file` 校验视图）：写回必须逐字节保真原有内容，校验会在写回时**剥字段**
* （如 policy-only 字段、未知键）反致数据损坏。目标文件存在但 JSON 损坏**或目标容器类型不符** → `CorruptTarget`
* 硬错（**不覆盖**、不静默清盘），对齐「覆盖前先看目标」的安全原则。
*/

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::super::raw_write::{
    emit_project_read_roots_warning, ensure_array_container, ensure_object_container,
    project_read_roots_warning, project_read_roots_write_ignored_warning, read_strict_map,
    RawWriteError,
};
use super::super::schema::{SettingsScope, FIELD_READ_ROOTS};
use super::super::scope::{apply_write, WriteValue, PROJECT_SETTINGS_FILENAME};
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
    /// 目标文件存在但非合法 JSON object，**或目标容器（object/array）已存在且类型不符** → 拒绝覆盖
    /// （保护用户数据）/ target is not a JSON object, or an existing target container has an unexpected type。
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
    // pre-flight：落盘前先**只读**探测每个目标文件（corrupt / IO / 目标容器类型），任一失败即零落盘返回——
    // 把可预检故障（损坏的既有目标、畸形容器）挡在**首次写之前**，显著收窄「顺序多文件写、后序失败不回滚
    // 前序」的半落盘窗口。逐 plan 对**盘上现状**校验，不模拟同批前序 plan 的执行结果。
    // 非真原子性（best-effort 早探测）：探测与实际写之间文件仍可能被外部改动，故 `execute_one` 内持锁重读为准。
    // #292：校验经 [`validate_write_plan`] 单一入口，与 `config::import` 零写 preflight 共用。
    for plan in plans {
        let existing = read_raw_object(plan.file.as_path())?;
        validate_write_plan(plan, &existing)?;
    }
    for plan in plans {
        execute_one(plan)?;
    }
    Ok(())
}

fn execute_one(plan: &WritePlan) -> Result<(), ExecutorError> {
    execute_one_with(plan, emit_project_read_roots_warning)
}

/// 执行单条计划；`emit_warning` 是 WARN 发射接缝——生产路径为 tracing + cli stderr
/// （[`emit_project_read_roots_warning`]），测试注入捕获器以 mutation-lock 发射接线。
fn execute_one_with(
    plan: &WritePlan,
    emit_warning: impl Fn(&Path, &str),
) -> Result<(), ExecutorError> {
    let path = plan.file.as_path();
    with_settings_lock(path, || -> Result<(), ExecutorError> {
        let existing = read_raw_object(path)?;
        let prepared = prepare_write_plan(plan, &existing)?;
        if let Some(message) =
            read_roots_policy_warning(plan, &existing, prepared.touched_read_roots)
        {
            emit_warning(path, &message);
        }
        let next = match prepared.op {
            PreparedOp::Skip => None,
            PreparedOp::Value(updates) => apply_value_updates(&existing, &updates),
            PreparedOp::StringSetInsert { field, value } => {
                string_set_insert(&existing, &field, &value)
            }
            PreparedOp::StringSetRemove { field, value } => {
                string_set_remove(&existing, &field, &value)
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

/// 准备后的执行计划（readRoots 策略剥离 + 目标容器校验后）/ prepared plan after policy stripping.
#[derive(Debug)]
struct Prepared {
    /// 实际执行的操作（`Skip` = 无需任何写入）。
    op: PreparedOp,
    /// 计划是否显式命中 project `readRoots`（被忽略，仅用于 WARN）。
    touched_read_roots: bool,
}

/// 策略剥离后的写操作 / the effective write op after policy stripping.
#[derive(Debug)]
enum PreparedOp {
    /// 无需写入（readRoots-only 或剥离后为空）。
    Skip,
    /// 顶层 object 更新（`WriteValue::Object` 的内容）。
    Value(BTreeMap<String, WriteValue>),
    /// 字符串数组字段插入成员。
    StringSetInsert { field: String, value: String },
    /// 字符串数组字段移除成员。
    StringSetRemove { field: String, value: String },
}

/// 是否为 project scope 的 `settings.json`（#245/#292：`readRoots` 策略仅作用于该文件）。
fn is_project_settings(scope: SettingsScope, path: &Path) -> bool {
    scope == SettingsScope::Project
        && path.file_name().and_then(|name| name.to_str()) == Some(PROJECT_SETTINGS_FILENAME)
}

/// 策略剥离 + 目标容器预检：preflight 与 `execute_one`（锁内重读）共用。
///
/// 顺序固定为**先剥离后校验**：project settings 的显式 `readRoots` 子项被剔出更新树（盘上旧键由
/// `apply_write` 克隆保留），因此不会被当作待写容器校验，也不会阻塞无关写入。
fn prepare_write_plan(
    plan: &WritePlan,
    existing: &Map<String, Value>,
) -> Result<Prepared, ExecutorError> {
    let path = plan.file.as_path();
    let project_settings = is_project_settings(plan.scope, path);
    match &plan.op {
        WriteTargetOp::Value(wv) => {
            let mut updates: BTreeMap<String, WriteValue> = match wv {
                WriteValue::Object(map) => map.clone(),
                // 顶层 `WriteValue` 由 S2 恒构造为 `Object`；非 Object 属内部不变量违反。
                _ => {
                    return Err(ExecutorError::Internal {
                        reason: "top-level WriteTargetOp::Value must be a WriteValue::Object"
                            .to_string(),
                    })
                }
            };
            let touched_read_roots = project_settings && updates.remove(FIELD_READ_ROOTS).is_some();
            validate_object_updates(path, existing, &updates)?;
            let op = if updates.is_empty() {
                PreparedOp::Skip
            } else {
                PreparedOp::Value(updates)
            };
            Ok(Prepared {
                op,
                touched_read_roots,
            })
        }
        WriteTargetOp::StringSetInsert { field, value } => {
            if project_settings && field == FIELD_READ_ROOTS {
                return Ok(Prepared {
                    op: PreparedOp::Skip,
                    touched_read_roots: true,
                });
            }
            ensure_array_container(existing, field, path)
                .map_err(|e| map_raw_write_error(path, e))?;
            Ok(Prepared {
                op: PreparedOp::StringSetInsert {
                    field: field.clone(),
                    value: value.clone(),
                },
                touched_read_roots: false,
            })
        }
        WriteTargetOp::StringSetRemove { field, value } => {
            if project_settings && field == FIELD_READ_ROOTS {
                return Ok(Prepared {
                    op: PreparedOp::Skip,
                    touched_read_roots: true,
                });
            }
            ensure_array_container(existing, field, path)
                .map_err(|e| map_raw_write_error(path, e))?;
            Ok(Prepared {
                op: PreparedOp::StringSetRemove {
                    field: field.clone(),
                    value: value.clone(),
                },
                touched_read_roots: false,
            })
        }
    }
}

/// #292：目标容器预检的只读投影——`execute_write_plans` 批前探测与
/// `config::import::preflight_mcp_import` 零写预测**共用**同一校验（`prepare_write_plan` 含 project
/// `readRoots` 策略剥离 + 逐层 object/string-set 容器 guard）。
///
/// 丢弃私有 `Prepared`，不把执行计划细节暴露给调用方（避免 `private_interfaces` 告警）。
pub(crate) fn validate_write_plan(
    plan: &WritePlan,
    existing: &Map<String, Value>,
) -> Result<(), ExecutorError> {
    prepare_write_plan(plan, existing).map(|_| ())
}

/// 逐层校验 object 型更新树：每个 `WriteValue::Object` 容器键在盘上必须缺失或已是 object。
///
/// `Set`（显式整体替换）与 `Delete`（删键）是刻意逃逸口，不做容器约束；父容器类型不符已在下钻处拦截，
/// 因此 `apply_write` 不会静默物化新对象覆盖标量/数组。
fn validate_object_updates(
    path: &Path,
    existing: &Map<String, Value>,
    updates: &BTreeMap<String, WriteValue>,
) -> Result<(), ExecutorError> {
    for (key, update) in updates {
        if let WriteValue::Object(sub) = update {
            ensure_object_container(existing, key, path)
                .map_err(|e| map_raw_write_error(path, e))?;
            if let Some(Value::Object(existing_sub)) = existing.get(key) {
                validate_object_updates(path, existing_sub, sub)?;
            }
        }
    }
    Ok(())
}

/// raw_write 容器 guard 错误 → executor 公共错误（不新增公开枚举变体，#292）。
///
/// 容器 guard（[`ensure_object_container`] / [`ensure_array_container`]）只会返回 `MalformedTarget`；
/// 其余变体属内部不变量违反，防御性映射为 `Internal`。
fn map_raw_write_error(path: &Path, error: RawWriteError) -> ExecutorError {
    match error {
        RawWriteError::MalformedTarget { key, expected, .. } => ExecutorError::CorruptTarget {
            file: path.to_path_buf(),
            reason: format!("field {key:?} has an unexpected type (expected {expected})"),
        },
        other => ExecutorError::Internal {
            reason: format!("unexpected container-guard error: {other}"),
        },
    }
}

/// 选择 project `readRoots` 告警文案（#292 A+）：**仅 project settings.json 适用**；显式写被忽略 /
/// 盘上旧键被保留，每 plan 至多一条。
///
/// 非 project settings 目标（user/local settings、project mcp.json）上的同名键是**合法且读面生效**的
/// （reader 只拒绝 project scope），不得借用「project scope ignored」文案——否则会误导用户以为授权失效。
fn read_roots_policy_warning(
    plan: &WritePlan,
    existing: &Map<String, Value>,
    touched_read_roots: bool,
) -> Option<String> {
    let path = plan.file.as_path();
    if !is_project_settings(plan.scope, path) {
        return None;
    }
    if touched_read_roots {
        let key_present = existing.contains_key(FIELD_READ_ROOTS);
        Some(project_read_roots_write_ignored_warning(path, key_present))
    } else {
        project_read_roots_warning(path, existing)
    }
}

/// 读**原始** JSON object（无校验，复用 raw_write 的 strict 读）：缺失/空 → 空 map；损坏/非 object →
/// `CorruptTarget`（不覆盖）；I/O（含非 UTF-8）→ `Io`。
pub(crate) fn read_raw_object(path: &Path) -> Result<Map<String, Value>, ExecutorError> {
    read_strict_map(path).map_err(|error| match error {
        RawWriteError::Corrupt { path, reason } => {
            ExecutorError::CorruptTarget { file: path, reason }
        }
        RawWriteError::Io { path, source } => ExecutorError::Io {
            file: path,
            reason: source.to_string(),
        },
        // strict 读当前不会产出以下变体；显式映射保持穷尽性，避免未来误分类为 I/O。
        RawWriteError::MalformedTarget { key, expected, .. } => ExecutorError::Internal {
            reason: format!(
                "unexpected malformed-target error from strict read: field {key:?} (expected {expected})"
            ),
        },
        RawWriteError::Store(e) => ExecutorError::Lock {
            file: path.to_path_buf(),
            reason: e.to_string(),
        },
    })
}

/// `WriteTargetOp::Value` 应用：`apply_write` 后**语义比对**（剥空对象脚手架）判 no-change。
fn apply_value_updates(
    existing: &Map<String, Value>,
    updates: &BTreeMap<String, WriteValue>,
) -> Option<Map<String, Value>> {
    let updated = apply_write(existing, updates);
    // 精确 no-change 判定：`updated` 相对 `existing` 是否**仅多出本次写新物化的空对象脚手架**。
    // （字节级 `updated == existing` 不够——`apply_write` 会把缺失父键物化成 `{"servers":{}}`；而对称地
    //  剥两侧空对象又会误跳真实删除，例如磁盘存 `{"servers":{"a":{}}}` 时 `Remove("a")` 应删却被判等。）
    if is_no_change(&updated, existing) {
        None
    } else {
        Some(updated)
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
    use crate::settings::scope::{
        user_settings_path, workdir_local_settings_path, workdir_project_settings_path, EnvMap,
    };
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

    /// 决定本计划应发的 project `readRoots` 告警文案（与 `execute_one` 同一判定路径）。
    ///
    /// 直接断言纯选择函数而非 tracing 捕获：后者依赖全局 callsite interest 缓存，在全量并行测试下
    /// 会偶发丢事件（#292 实测）；发射通道（tracing + cli stderr）由 `raw_write` 与 CLI 集成测试覆盖。
    fn policy_warning_for(plan: &WritePlan) -> Option<String> {
        let existing = read_raw_object(plan.file.as_path()).unwrap();
        let prepared = prepare_write_plan(plan, &existing).unwrap();
        read_roots_policy_warning(plan, &existing, prepared.touched_read_roots)
    }

    /// 经生产执行路径（`execute_one_with`，注入捕获器）执行单条计划并返回捕获到的告警。
    fn execute_and_capture(plan: &WritePlan) -> Vec<String> {
        let captured = std::sync::Mutex::new(Vec::new());
        execute_one_with(plan, |_, message| {
            captured.lock().unwrap().push(message.to_string());
        })
        .unwrap();
        captured.into_inner().unwrap()
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
    fn project_settings_set_read_roots_only_is_ignored_and_file_untouched() {
        // #292（A+）：显式 Set project readRoots → 不写新值、不落盘、产生 write-ignored WARN。
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        let path = workdir_project_settings_path(&wd);
        write(&path, r#"{"strictKnownMarketplaces":true}"#);
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        let before_bytes = std::fs::read(&path).unwrap();
        let inner: BTreeMap<String, WriteValue> =
            std::iter::once(("readRoots".to_string(), WriteValue::Set(json!(["/data"])))).collect();
        let plan = WritePlan {
            scope: crate::settings::schema::SettingsScope::Project,
            file: path.clone(),
            op: WriteTargetOp::Value(WriteValue::Object(inner)),
        };
        let warning = policy_warning_for(&plan).expect("显式 Set 必须产生 write-ignored 告警");
        assert!(warning.contains("readRoots"), "{warning}");
        assert!(warning.contains("write ignored"), "{warning}");
        execute_write_plans(std::slice::from_ref(&plan)).unwrap();
        let saved = read_json(&path);
        assert!(saved.get("readRoots").is_none(), "{saved}");
        assert_eq!(saved["strictKnownMarketplaces"], json!(true));
        let after = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(before, after, "被忽略的写不得触碰文件");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before_bytes,
            "文件字节不得变化"
        );
    }

    #[test]
    fn project_scope_write_plan_preserves_existing_read_roots() {
        // #292 A+：任意 project settings 写计划不再剔除盘上既有的 readRoots（保留 + WARN）。
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        let path = workdir_project_settings_path(&wd);
        write(
            &path,
            r#"{"readRoots":["/data"],"enabledPlugins":{"p@m":false}}"#,
        );
        let inner: BTreeMap<String, WriteValue> =
            std::iter::once(("p@m".to_string(), WriteValue::Set(json!(true)))).collect();
        let outer: BTreeMap<String, WriteValue> =
            std::iter::once(("enabledPlugins".to_string(), WriteValue::Object(inner))).collect();
        let plan = WritePlan {
            scope: crate::settings::schema::SettingsScope::Project,
            file: path.clone(),
            op: WriteTargetOp::Value(WriteValue::Object(outer)),
        };
        let warning =
            policy_warning_for(&plan).expect("盘上有 readRoots 时必须产生 preserved 告警");
        assert!(warning.contains("preserved"), "{warning}");
        execute_write_plans(std::slice::from_ref(&plan)).unwrap();
        let saved = read_json(&path);
        assert_eq!(
            saved["readRoots"],
            json!(["/data"]),
            "project readRoots 必须原值保留：{saved}"
        );
        assert_eq!(saved["enabledPlugins"]["p@m"], json!(true));
    }

    #[test]
    fn project_settings_read_roots_delete_only_does_not_create_file() {
        // Delete readRoots 命中不存在的文件 → 策略跳过、不得凭空建 `{}`；仍产生 write-ignored WARN。
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        let path = workdir_project_settings_path(&wd);
        let inner: BTreeMap<String, WriteValue> =
            std::iter::once(("readRoots".to_string(), WriteValue::Delete)).collect();
        let plan = WritePlan {
            scope: crate::settings::schema::SettingsScope::Project,
            file: path.clone(),
            op: WriteTargetOp::Value(WriteValue::Object(inner)),
        };
        let warning =
            policy_warning_for(&plan).expect("Delete readRoots 必须产生 write-ignored 告警");
        assert!(warning.contains("write ignored"), "{warning}");
        execute_write_plans(std::slice::from_ref(&plan)).unwrap();
        assert!(!path.exists(), "readRoots-only plan 不得凭空建文件");
    }

    #[test]
    fn project_settings_read_roots_delete_is_ignored_and_key_preserved() {
        // #292：显式 Delete project readRoots → 被忽略、旧键保留、文件字节不变。
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        let path = workdir_project_settings_path(&wd);
        write(&path, r#"{"readRoots":["/data"]}"#);
        let before_bytes = std::fs::read(&path).unwrap();
        let inner: BTreeMap<String, WriteValue> =
            std::iter::once(("readRoots".to_string(), WriteValue::Delete)).collect();
        let plan = WritePlan {
            scope: crate::settings::schema::SettingsScope::Project,
            file: path.clone(),
            op: WriteTargetOp::Value(WriteValue::Object(inner)),
        };
        let warning =
            policy_warning_for(&plan).expect("Delete readRoots 必须产生 write-ignored 告警");
        assert!(warning.contains("write ignored"), "{warning}");
        execute_write_plans(std::slice::from_ref(&plan)).unwrap();
        let saved = read_json(&path);
        assert_eq!(
            saved["readRoots"],
            json!(["/data"]),
            "Delete 被忽略、旧键保留：{saved}"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before_bytes,
            "文件字节不得变化"
        );
    }

    #[test]
    fn project_scope_no_change_plan_preserves_existing_read_roots() {
        // #292：op 语义无变化（remove 不存在的成员）不再为清旧键强制重写——键保留、mtime/字节不变。
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        let path = workdir_project_settings_path(&wd);
        write(
            &path,
            r#"{"readRoots":["/data"],"disabledMcpjsonServers":["other"]}"#,
        );
        let before_bytes = std::fs::read(&path).unwrap();
        let before_mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
        let plan = WritePlan {
            scope: crate::settings::schema::SettingsScope::Project,
            file: path.clone(),
            op: WriteTargetOp::StringSetRemove {
                field: "disabledMcpjsonServers".to_string(),
                value: "absent".to_string(),
            },
        };
        let warning =
            policy_warning_for(&plan).expect("盘上有 readRoots 时必须产生 preserved 告警");
        assert!(warning.contains("preserved"), "{warning}");
        execute_write_plans(std::slice::from_ref(&plan)).unwrap();
        let saved = read_json(&path);
        assert_eq!(
            saved["readRoots"],
            json!(["/data"]),
            "project readRoots 必须保留：{saved}"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before_bytes,
            "文件字节不得变化"
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            before_mtime,
            "no-change 不得重写文件"
        );
    }

    #[test]
    fn project_settings_read_roots_with_sibling_writes_sibling_only() {
        // 同一计划内 readRoots 子项被剥离，sibling 键照常写入；readRoots 不得落盘。
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        let path = workdir_project_settings_path(&wd);
        let mut updates: BTreeMap<String, WriteValue> = BTreeMap::new();
        updates.insert("readRoots".to_string(), WriteValue::Set(json!(["/data"])));
        updates.insert(
            "strictKnownMarketplaces".to_string(),
            WriteValue::Set(json!(true)),
        );
        let plan = WritePlan {
            scope: crate::settings::schema::SettingsScope::Project,
            file: path.clone(),
            op: WriteTargetOp::Value(WriteValue::Object(updates)),
        };
        let warning =
            policy_warning_for(&plan).expect("计划命中 readRoots 必须产生 write-ignored 告警");
        assert!(warning.contains("write ignored"), "{warning}");
        execute_write_plans(std::slice::from_ref(&plan)).unwrap();
        let saved = read_json(&path);
        assert!(saved.get("readRoots").is_none(), "{saved}");
        assert_eq!(saved["strictKnownMarketplaces"], json!(true));
    }

    #[test]
    fn project_settings_string_set_read_roots_insert_is_ignored() {
        // StringSetInsert 命中 project readRoots → 整条计划跳过、旧键保留、字节不变。
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        let path = workdir_project_settings_path(&wd);
        write(&path, r#"{"readRoots":["/old"]}"#);
        let before_bytes = std::fs::read(&path).unwrap();
        let plan = WritePlan {
            scope: crate::settings::schema::SettingsScope::Project,
            file: path.clone(),
            op: WriteTargetOp::StringSetInsert {
                field: "readRoots".to_string(),
                value: "/new".to_string(),
            },
        };
        let warning = policy_warning_for(&plan).expect("StringSetInsert readRoots 必须产生告警");
        assert!(warning.contains("write ignored"), "{warning}");
        execute_write_plans(std::slice::from_ref(&plan)).unwrap();
        let saved = read_json(&path);
        assert_eq!(saved["readRoots"], json!(["/old"]), "{saved}");
        assert_eq!(std::fs::read(&path).unwrap(), before_bytes);
    }

    #[test]
    fn project_settings_string_set_read_roots_remove_is_ignored() {
        // StringSetRemove 命中 project readRoots → 同样跳过、旧键保留。
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        let path = workdir_project_settings_path(&wd);
        write(&path, r#"{"readRoots":["/old"]}"#);
        let before_bytes = std::fs::read(&path).unwrap();
        let plan = WritePlan {
            scope: crate::settings::schema::SettingsScope::Project,
            file: path.clone(),
            op: WriteTargetOp::StringSetRemove {
                field: "readRoots".to_string(),
                value: "/old".to_string(),
            },
        };
        let warning = policy_warning_for(&plan).expect("StringSetRemove readRoots 必须产生告警");
        assert!(warning.contains("write ignored"), "{warning}");
        execute_write_plans(std::slice::from_ref(&plan)).unwrap();
        let saved = read_json(&path);
        assert_eq!(saved["readRoots"], json!(["/old"]), "{saved}");
        assert_eq!(std::fs::read(&path).unwrap(), before_bytes);
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
    fn object_merge_into_non_object_target_fails_closed() {
        // #292：目标容器类型不符（enabledPlugins 是字符串）→ 批前 CorruptTarget，不再静默物化覆盖。
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        let path = workdir_local_settings_path(&wd);
        write(&path, r#"{"enabledPlugins":"oops"}"#);
        let before_bytes = std::fs::read(&path).unwrap();
        let inner: BTreeMap<String, WriteValue> =
            std::iter::once(("p@m".to_string(), WriteValue::Set(json!(true)))).collect();
        let outer: BTreeMap<String, WriteValue> =
            std::iter::once(("enabledPlugins".to_string(), WriteValue::Object(inner))).collect();
        let plan = WritePlan {
            scope: crate::settings::schema::SettingsScope::Local,
            file: path.clone(),
            op: WriteTargetOp::Value(WriteValue::Object(outer)),
        };
        let err = execute_write_plans(std::slice::from_ref(&plan)).unwrap_err();
        match err {
            ExecutorError::CorruptTarget { file, reason } => {
                assert_eq!(file, path);
                assert!(reason.contains("enabledPlugins"), "{reason}");
                assert!(reason.contains("object"), "{reason}");
            }
            other => panic!("expected CorruptTarget, got {other:?}"),
        }
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before_bytes,
            "畸形目标不得被改写"
        );
    }

    #[test]
    fn nested_object_merge_into_non_object_fails_closed() {
        // 每层 object 容器都预检：嵌套层 `servers.a` 是标量 → fail-closed，兄弟 b 原样保留。
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        let path = workdir_mcp_config_path(&wd);
        write(&path, r#"{"servers":{"a":"scalar","b":{"type":"stdio"}}}"#);
        let before_bytes = std::fs::read(&path).unwrap();
        let a_inner: BTreeMap<String, WriteValue> =
            std::iter::once(("nested".to_string(), WriteValue::Set(json!(1)))).collect();
        let servers_inner: BTreeMap<String, WriteValue> =
            std::iter::once(("a".to_string(), WriteValue::Object(a_inner))).collect();
        let outer: BTreeMap<String, WriteValue> =
            std::iter::once(("servers".to_string(), WriteValue::Object(servers_inner))).collect();
        let plan = WritePlan {
            scope: crate::settings::schema::SettingsScope::Project,
            file: path.clone(),
            op: WriteTargetOp::Value(WriteValue::Object(outer)),
        };
        let err = execute_write_plans(std::slice::from_ref(&plan)).unwrap_err();
        match err {
            ExecutorError::CorruptTarget { reason, .. } => {
                assert!(reason.contains("\"a\""), "reason 须点名键：{reason}");
                assert!(reason.contains("object"), "{reason}");
            }
            other => panic!("expected CorruptTarget, got {other:?}"),
        }
        assert_eq!(std::fs::read(&path).unwrap(), before_bytes);
        assert_eq!(read_json(&path)["servers"]["b"], json!({"type": "stdio"}));
    }

    #[test]
    fn string_set_insert_non_array_target_fails_closed() {
        // 旧行为：非数组当空数组覆盖；新行为：批前 CorruptTarget，文件逐字节不变。
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        let path = workdir_local_settings_path(&wd);
        write(&path, r#"{"disabledMcpjsonServers":"oops"}"#);
        let before_bytes = std::fs::read(&path).unwrap();
        let plan = WritePlan {
            scope: crate::settings::schema::SettingsScope::Local,
            file: path.clone(),
            op: WriteTargetOp::StringSetInsert {
                field: "disabledMcpjsonServers".to_string(),
                value: "srv".to_string(),
            },
        };
        let err = execute_write_plans(std::slice::from_ref(&plan)).unwrap_err();
        match err {
            ExecutorError::CorruptTarget { reason, .. } => {
                assert!(reason.contains("disabledMcpjsonServers"), "{reason}");
                assert!(reason.contains("array"), "{reason}");
            }
            other => panic!("expected CorruptTarget, got {other:?}"),
        }
        assert_eq!(std::fs::read(&path).unwrap(), before_bytes);
    }

    #[test]
    fn string_set_remove_non_array_target_fails_closed() {
        // 旧行为：静默 no-op；新行为：批前 CorruptTarget。
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        let path = workdir_local_settings_path(&wd);
        write(&path, r#"{"disabledMcpjsonServers":"oops"}"#);
        let before_bytes = std::fs::read(&path).unwrap();
        let plan = WritePlan {
            scope: crate::settings::schema::SettingsScope::Local,
            file: path.clone(),
            op: WriteTargetOp::StringSetRemove {
                field: "disabledMcpjsonServers".to_string(),
                value: "srv".to_string(),
            },
        };
        let err = execute_write_plans(std::slice::from_ref(&plan)).unwrap_err();
        match err {
            ExecutorError::CorruptTarget { reason, .. } => {
                assert!(reason.contains("disabledMcpjsonServers"), "{reason}");
                assert!(reason.contains("array"), "{reason}");
            }
            other => panic!("expected CorruptTarget, got {other:?}"),
        }
        assert_eq!(std::fs::read(&path).unwrap(), before_bytes);
    }

    #[test]
    fn preflight_malformed_container_mid_batch_prevents_any_write() {
        // 批前验证：后序计划目标容器畸形 → 前序有效目标零改动（对齐 corrupt 文件批测语义）。
        let tmp = TempDir::new().unwrap();
        let env = xdg_env(&tmp);
        let wd = tmp.path().join("wd");
        let user = user_mcp_config_path(Some(&env));
        write(&user, r#"{"servers": {"srv": {"type": "stdio"}}}"#);
        let user_bytes = std::fs::read(&user).unwrap();
        let local = workdir_local_settings_path(&wd);
        write(&local, r#"{"disabledMcpjsonServers":"oops"}"#);
        let local_bytes = std::fs::read(&local).unwrap();
        let plans = vec![
            WritePlan {
                scope: crate::settings::schema::SettingsScope::User,
                file: user.clone(),
                op: server_delete("srv"),
            },
            WritePlan {
                scope: crate::settings::schema::SettingsScope::Local,
                file: local.clone(),
                op: WriteTargetOp::StringSetInsert {
                    field: "disabledMcpjsonServers".to_string(),
                    value: "srv".to_string(),
                },
            },
        ];
        let err = execute_write_plans(&plans).unwrap_err();
        assert!(matches!(err, ExecutorError::CorruptTarget { .. }));
        assert_eq!(
            std::fs::read(&user).unwrap(),
            user_bytes,
            "pre-flight 应在任何落盘前拦截 → 前序有效目标零改动"
        );
        assert_eq!(std::fs::read(&local).unwrap(), local_bytes);
    }

    #[test]
    fn project_mcp_config_plan_is_not_subject_to_read_roots_policy() {
        // 文件名护栏：project scope 的 mcp.json 不适用 project settings 的 readRoots 策略。
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        let path = workdir_mcp_config_path(&wd);
        write(&path, r#"{"readRoots":["/stray"],"servers":{}}"#);
        let plan = WritePlan {
            scope: crate::settings::schema::SettingsScope::Project,
            file: path.clone(),
            op: server_set("srv", json!({"type": "stdio"})),
        };
        assert!(
            policy_warning_for(&plan).is_none(),
            "project mcp.json 上的 readRoots 不得触发 project settings 策略告警"
        );
        let warnings = execute_and_capture(&plan);
        assert!(
            warnings.is_empty(),
            "执行路径不得发射 readRoots 告警：{warnings:?}"
        );
        let saved = read_json(&path);
        assert_eq!(saved["readRoots"], json!(["/stray"]), "{saved}");
        assert_eq!(saved["servers"]["srv"], json!({"type": "stdio"}));
    }

    #[test]
    fn non_project_settings_with_read_roots_does_not_warn() {
        // 回归（审查 🔴）：local settings 的 readRoots 在读面**是生效的**（受信 scope），
        // 无关写不得借用「project scope ignored」文案误导用户。
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        let path = workdir_local_settings_path(&wd);
        write(&path, r#"{"readRoots":["/data"],"keep":1}"#);
        let inner: BTreeMap<String, WriteValue> = std::iter::once((
            "strictKnownMarketplaces".to_string(),
            WriteValue::Set(json!(true)),
        ))
        .collect();
        let plan = WritePlan {
            scope: crate::settings::schema::SettingsScope::Local,
            file: path.clone(),
            op: WriteTargetOp::Value(WriteValue::Object(inner)),
        };
        assert!(
            policy_warning_for(&plan).is_none(),
            "local settings 不得产生 project scope readRoots 告警"
        );
        let warnings = execute_and_capture(&plan);
        assert!(
            warnings.is_empty(),
            "执行路径不得发射 readRoots 告警：{warnings:?}"
        );
        let saved = read_json(&path);
        assert_eq!(saved["readRoots"], json!(["/data"]), "{saved}");
        assert_eq!(saved["strictKnownMarketplaces"], json!(true));
    }

    #[test]
    fn user_settings_read_roots_write_is_allowed_and_silent() {
        // scope 半闸 mutation-lock：user settings 与 project settings 同名（settings.json），
        // 但 scope=User 时 readRoots 合法、必须原样写入且不得产生 project 策略告警。
        let tmp = TempDir::new().unwrap();
        let env = xdg_env(&tmp);
        let path = user_settings_path(Some(&env));
        write(&path, r#"{"keep":1}"#);
        let inner: BTreeMap<String, WriteValue> =
            std::iter::once(("readRoots".to_string(), WriteValue::Set(json!(["/x"])))).collect();
        let plan = WritePlan {
            scope: crate::settings::schema::SettingsScope::User,
            file: path.clone(),
            op: WriteTargetOp::Value(WriteValue::Object(inner)),
        };
        assert!(
            policy_warning_for(&plan).is_none(),
            "user settings 不得产生 project scope readRoots 告警"
        );
        let warnings = execute_and_capture(&plan);
        assert!(
            warnings.is_empty(),
            "执行路径不得发射 readRoots 告警：{warnings:?}"
        );
        let saved = read_json(&path);
        assert_eq!(saved["readRoots"], json!(["/x"]), "{saved}");
        assert_eq!(saved["keep"], json!(1));
    }

    #[test]
    fn execute_one_binds_production_emitter() {
        // 源锚守卫（同 `mcp_clients/sse_client.rs` 先例）：生产路径必须绑定真实发射器
        // （tracing + cli stderr），而非测试闭包。发射副作用在并行 lib 测试中不能确定性观测
        // （全局 callsite interest 会被并发 dispatcher 过滤），故用源码锚锁定默认绑定。
        //
        // 🔴 必须只看 `#[cfg(test)]` 之前的生产段：needle 也出现在本测试的字面量里，整文件
        // `contains` 会恒真（#293 架构师复审 🔴）。
        let src = include_str!("executor.rs");
        let production = src
            .split_once("#[cfg(test)]")
            .map_or(src, |(production, _)| production);
        assert!(
            production.contains("execute_one_with(plan, emit_project_read_roots_warning)"),
            "execute_one 必须绑定生产 WARN 发射器"
        );
    }

    #[test]
    fn execute_one_emits_read_roots_policy_warning() {
        // 发射接线 mutation-lock：删除 `execute_one_with` 内的 `emit_warning` 调用本测试即红。
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        let path = workdir_project_settings_path(&wd);
        write(
            &path,
            r#"{"readRoots":["/data"],"enabledPlugins":{"p@m":false}}"#,
        );
        let inner: BTreeMap<String, WriteValue> =
            std::iter::once(("p@m".to_string(), WriteValue::Set(json!(true)))).collect();
        let outer: BTreeMap<String, WriteValue> =
            std::iter::once(("enabledPlugins".to_string(), WriteValue::Object(inner))).collect();
        let plan = WritePlan {
            scope: crate::settings::schema::SettingsScope::Project,
            file: path.clone(),
            op: WriteTargetOp::Value(WriteValue::Object(outer)),
        };

        let captured = std::sync::Mutex::new(Vec::new());
        execute_one_with(&plan, |_, message| {
            captured.lock().unwrap().push(message.to_string());
        })
        .unwrap();
        let captured = captured.into_inner().unwrap();
        assert_eq!(captured.len(), 1, "每 plan 至多一条 readRoots 告警");
        assert!(captured[0].contains("readRoots"), "{}", captured[0]);
        assert!(captured[0].contains("preserved"), "{}", captured[0]);
        // 告警不阻碍实际写入：sibling 落盘、readRoots 保留。
        let saved = read_json(&path);
        assert_eq!(saved["readRoots"], json!(["/data"]), "{saved}");
        assert_eq!(saved["enabledPlugins"]["p@m"], json!(true));
    }

    #[test]
    fn non_project_read_roots_write_is_allowed() {
        // 防过度剔除：user/local 等受信 scope 经 executor 写 readRoots 照常落盘。
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        let path = workdir_local_settings_path(&wd);
        let inner: BTreeMap<String, WriteValue> =
            std::iter::once(("readRoots".to_string(), WriteValue::Set(json!(["/x"])))).collect();
        let plan = WritePlan {
            scope: crate::settings::schema::SettingsScope::Local,
            file: path.clone(),
            op: WriteTargetOp::Value(WriteValue::Object(inner)),
        };
        execute_write_plans(std::slice::from_ref(&plan)).unwrap();
        let saved = read_json(&path);
        assert_eq!(saved["readRoots"], json!(["/x"]), "{saved}");
    }

    #[test]
    fn project_settings_plan_without_read_roots_does_not_warn() {
        // 噪声护栏：盘上无 readRoots 的普通 project settings 计划不得产生 readRoots 告警。
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        let path = workdir_project_settings_path(&wd);
        write(&path, r#"{"keep":1}"#);
        let inner: BTreeMap<String, WriteValue> = std::iter::once((
            "strictKnownMarketplaces".to_string(),
            WriteValue::Set(json!(true)),
        ))
        .collect();
        let plan = WritePlan {
            scope: crate::settings::schema::SettingsScope::Project,
            file: path.clone(),
            op: WriteTargetOp::Value(WriteValue::Object(inner)),
        };
        assert!(
            policy_warning_for(&plan).is_none(),
            "盘上无 readRoots 且计划未命中时不得产生告警"
        );
        execute_write_plans(std::slice::from_ref(&plan)).unwrap();
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
