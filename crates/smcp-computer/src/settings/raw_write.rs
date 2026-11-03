//! #253：settings 文件 strict raw RMW（读原始 JSON、只改目标键、corrupt fail-closed）共享助手。
//!
//! 与 [`read_roots::read_raw_map`](super::read_roots::read_raw_map)（宽容读，仅供 `settings edit`
//! 写后 WARN 判断）分离：
//! 本模块的读**区分缺失与损坏**——缺失/空文件按空对象起步；畸形 JSON 或根非对象返回
//! [`RawWriteError::Corrupt`]，调用方必须**拒绝写盘、保留原文件逐字节不变**。
//!
//! 写入语义：
//! - 锁内 read → mutate → compare → write；`mutate` 返回 Err ⇒ 零写盘。
//! - `changed` / `wrote` = mutate 后与原始语义内容不同（无变化不写盘、不扰 mtime）。
//! - **A+（2026-11-03 用户拍板）**：project scope 的 `readRoots` **保留原键 + WARN**，不做任何
//!   写入期剔除；读面 `trusted-scope-only` 拒绝保持不动。主动删除用户数据与 raw 保真目标相反。
//!
//! 注意：即使 no-op / corrupt，[`with_settings_lock`](super::store::with_settings_lock) 仍会创建父目录与
//! `.lock` 旁车文件——「原文件不变」指 settings 数据文件本身；corrupt 文件**不备份、不删除**。

use std::io;
#[cfg(feature = "cli")]
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::schema::FIELD_READ_ROOTS;
use super::store::{atomic_write_settings_json, with_settings_lock, SettingsStoreError};

/// strict raw 写入错误 / strict raw-write errors。
#[derive(Debug, thiserror::Error)]
pub(crate) enum RawWriteError {
    /// settings 文件是畸形 JSON 或根非对象（fail-closed，拒绝覆盖）。
    #[error(
        "refusing to modify {path}: existing settings file is corrupt ({reason}); \
         fix the JSON or remove the file, then retry"
    )]
    Corrupt {
        /// 出错文件 / offending file。
        path: PathBuf,
        /// 解析失败原因 / parse failure reason。
        reason: String,
    },
    /// 目标键已存在但容器类型不符（如 `enabledPlugins` 非对象），拒绝静默替换。
    #[error(
        "refusing to modify {path}: field {key:?} has an unexpected type \
         (expected {expected}); fix or remove that field, then retry"
    )]
    MalformedTarget {
        /// 出错文件 / offending file。
        path: PathBuf,
        /// 目标键 / target key。
        key: String,
        /// 期望类型 / expected container kind。
        expected: &'static str,
    },
    /// 旁车锁不可得 / sidecar lock unavailable。
    #[error(transparent)]
    Store(#[from] SettingsStoreError),
    /// 写盘 I/O 失败 / write I/O failure。
    #[error("settings write I/O error on {path}: {source}")]
    Io {
        /// 出错文件 / offending file。
        path: PathBuf,
        /// 底层错误 / underlying error。
        #[source]
        source: io::Error,
    },
}

/// 一次 raw 更新的结果 / Outcome of one raw update。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RawUpdateOutcome {
    /// `mutate` 是否改变语义内容（installer 据此决定是否 bump config revision）。
    pub(crate) changed: bool,
    /// 是否实际写盘。A+ 后无写入期清理/跳过策略，当前恒等于 `changed`；保留独立字段表达
    /// 「语义变化」与「实际落盘」两个概念，未来若引入清理/条件跳过可分化。
    pub(crate) wrote: bool,
}

impl RawWriteError {
    /// 映射为公共错误类型 [`SettingsStoreError`]，避免把本私有类型暴露到公开 API。
    pub(crate) fn into_store(self) -> SettingsStoreError {
        let message = self.to_string();
        match self {
            RawWriteError::Store(e) => e,
            RawWriteError::Io { path, source } => SettingsStoreError::Io { path, source },
            RawWriteError::Corrupt { path, .. } | RawWriteError::MalformedTarget { path, .. } => {
                SettingsStoreError::Io {
                    path,
                    source: io::Error::new(io::ErrorKind::InvalidData, message),
                }
            }
        }
    }

    /// 映射为 [`io::Error`]（CLI marketplace 信任写入使用）。
    #[cfg(feature = "cli")]
    pub(crate) fn into_io_error(self) -> io::Error {
        match self {
            RawWriteError::Io { path, source } => {
                io::Error::new(source.kind(), format!("{}: {source}", path.display()))
            }
            RawWriteError::Store(e) => io::Error::other(e.to_string()),
            other => io::Error::new(io::ErrorKind::InvalidData, other.to_string()),
        }
    }
}

/// strict 读取 settings 原始 JSON object。
///
/// 缺失 / 空 / 纯空白 → 空 map；畸形 JSON 或根非对象 → [`RawWriteError::Corrupt`]；其它 I/O
/// （含非 UTF-8）→ [`RawWriteError::Io`]。
pub(crate) fn read_strict_map(path: &Path) -> Result<Map<String, Value>, RawWriteError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(source) => {
            return Err(RawWriteError::Io {
                path: path.to_path_buf(),
                source,
            })
        }
    };
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err(RawWriteError::Corrupt {
            path: path.to_path_buf(),
            reason: "root is not a JSON object".to_string(),
        }),
        Err(e) => Err(RawWriteError::Corrupt {
            path: path.to_path_buf(),
            reason: e.to_string(),
        }),
    }
}

/// project scope 写入时若盘上仍有 `readRoots`，返回用户可见告警文案（A+：保留 + WARN）。
pub(crate) fn project_read_roots_warning(path: &Path, map: &Map<String, Value>) -> Option<String> {
    map.contains_key(FIELD_READ_ROOTS).then(|| {
        format!(
            "project scope readRoots is ignored at read time (trusted-scope-only); \
             key preserved on disk at {}",
            path.display()
        )
    })
}

fn warn_project_read_roots(path: &Path, map: &Map<String, Value>) {
    if let Some(message) = project_read_roots_warning(path, map) {
        tracing::warn!(
            field = FIELD_READ_ROOTS,
            path = %path.display(),
            "{}",
            message
        );
        // CLI 未安装 tracing subscriber；写入期 WARN 必须对用户可见（A+）。
        // stderr 关闭/管道断裂时不得 panic：用 writeln! 并忽略结果。
        #[cfg(feature = "cli")]
        let _ = writeln!(std::io::stderr(), "warning: {message}");
    }
}

/// 持锁执行 strict raw read-modify-write。
///
/// `project_scope` 仅由 project scope 写入方置 true：若盘上仍有 `readRoots`，**保留并 WARN**
/// （A+），不做任何剔除。`mutate` 只应改目标键，返回 Err 表示拒绝写盘（fail-closed）。
pub(crate) fn update_raw_settings(
    path: &Path,
    project_scope: bool,
    mutate: impl FnOnce(&mut Map<String, Value>) -> Result<(), RawWriteError>,
) -> Result<RawUpdateOutcome, RawWriteError> {
    with_settings_lock(path, || -> Result<RawUpdateOutcome, RawWriteError> {
        let original = read_strict_map(path)?;
        let mut next = original.clone();
        mutate(&mut next)?;
        let changed = next != original;
        if project_scope {
            warn_project_read_roots(path, &next);
        }
        let wrote = next != original;
        if wrote {
            atomic_write_settings_json(path, &Value::Object(next)).map_err(|source| {
                RawWriteError::Io {
                    path: path.to_path_buf(),
                    source,
                }
            })?;
        }
        Ok(RawUpdateOutcome { changed, wrote })
    })
    .map_err(RawWriteError::Store)?
}

fn malformed(path: &Path, key: &str, expected: &'static str) -> RawWriteError {
    tracing::warn!(
        path = %path.display(),
        key,
        expected,
        "settings target field has unexpected type; refusing to write"
    );
    RawWriteError::MalformedTarget {
        path: path.to_path_buf(),
        key: key.to_string(),
        expected,
    }
}

/// 取（必要时物化）object 型目标键。
pub(crate) fn object_field_mut<'a>(
    map: &'a mut Map<String, Value>,
    key: &str,
    path: &Path,
) -> Result<&'a mut Map<String, Value>, RawWriteError> {
    map.entry(key.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    match map.get_mut(key) {
        Some(Value::Object(obj)) => Ok(obj),
        _ => Err(malformed(path, key, "object")),
    }
}

/// 取 object 型目标键；缺失返回 `None`（不物化），类型不符 → fail-closed。
#[cfg_attr(not(feature = "cli"), allow(dead_code))]
pub(crate) fn object_field_opt_mut<'a>(
    map: &'a mut Map<String, Value>,
    key: &str,
    path: &Path,
) -> Result<Option<&'a mut Map<String, Value>>, RawWriteError> {
    match map.get_mut(key) {
        None => Ok(None),
        Some(Value::Object(obj)) => Ok(Some(obj)),
        Some(_) => Err(malformed(path, key, "object")),
    }
}

/// 取（必要时物化）array 型目标键。
pub(crate) fn array_field_mut<'a>(
    map: &'a mut Map<String, Value>,
    key: &str,
    path: &Path,
) -> Result<&'a mut Vec<Value>, RawWriteError> {
    map.entry(key.to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    match map.get_mut(key) {
        Some(Value::Array(arr)) => Ok(arr),
        _ => Err(malformed(path, key, "array")),
    }
}

/// 取 array 型目标键；缺失返回 `None`（不物化），类型不符 → fail-closed。
#[cfg_attr(not(feature = "cli"), allow(dead_code))]
pub(crate) fn array_field_opt_mut<'a>(
    map: &'a mut Map<String, Value>,
    key: &str,
    path: &Path,
) -> Result<Option<&'a mut Vec<Value>>, RawWriteError> {
    match map.get_mut(key) {
        None => Ok(None),
        Some(Value::Array(arr)) => Ok(Some(arr)),
        Some(_) => Err(malformed(path, key, "array")),
    }
}

/// 设置 bool 型目标键；缺失 → 写入；已是同值 → 无变化；非 bool（含 null）→ fail-closed。
pub(crate) fn set_bool_field(
    map: &mut Map<String, Value>,
    key: &str,
    value: bool,
    path: &Path,
) -> Result<(), RawWriteError> {
    match map.get(key) {
        None => {
            map.insert(key.to_string(), Value::Bool(value));
            Ok(())
        }
        Some(Value::Bool(current)) => {
            if *current != value {
                map.insert(key.to_string(), Value::Bool(value));
            }
            Ok(())
        }
        Some(_) => Err(malformed(path, key, "bool")),
    }
}

/// 校验目标键可安全做 object 深合并：缺失或已是 object → Ok；其它类型 → fail-closed。
#[cfg_attr(not(feature = "cli"), allow(dead_code))]
pub(crate) fn ensure_object_container(
    map: &Map<String, Value>,
    key: &str,
    path: &Path,
) -> Result<(), RawWriteError> {
    match map.get(key) {
        None | Some(Value::Object(_)) => Ok(()),
        Some(_) => Err(malformed(path, key, "object")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::SystemTime;
    use tempfile::TempDir;

    fn path_of(tmp: &TempDir) -> PathBuf {
        tmp.path().join("settings.json")
    }

    fn write(path: &Path, body: &str) {
        std::fs::write(path, body).unwrap();
    }

    fn mtime(path: &Path) -> SystemTime {
        std::fs::metadata(path).unwrap().modified().unwrap()
    }

    #[test]
    fn missing_and_empty_files_start_from_empty_object() {
        let tmp = TempDir::new().unwrap();
        let path = path_of(&tmp);
        let outcome = update_raw_settings(&path, false, |map| {
            map.insert("k".to_string(), json!(1));
            Ok(())
        })
        .unwrap();
        assert!(outcome.changed && outcome.wrote);
        assert_eq!(
            serde_json::from_str::<Value>(&std::fs::read_to_string(&path).unwrap()).unwrap(),
            json!({"k": 1})
        );

        write(&path, "  \n");
        let outcome = update_raw_settings(&path, false, |map| {
            map.insert("k".to_string(), json!(2));
            Ok(())
        })
        .unwrap();
        assert!(outcome.changed && outcome.wrote);
        assert_eq!(
            serde_json::from_str::<Value>(&std::fs::read_to_string(&path).unwrap()).unwrap(),
            json!({"k": 2})
        );
    }

    #[test]
    fn corrupt_json_and_non_object_root_fail_closed() {
        let tmp = TempDir::new().unwrap();
        let path = path_of(&tmp);
        for body in ["{not json", "[1,2]", "\"scalar\""] {
            write(&path, body);
            let before = std::fs::read(&path).unwrap();
            let err = update_raw_settings(&path, false, |map| {
                map.insert("k".to_string(), json!(1));
                Ok(())
            })
            .unwrap_err();
            assert!(matches!(err, RawWriteError::Corrupt { .. }), "{err}");
            assert_eq!(
                std::fs::read(&path).unwrap(),
                before,
                "corrupt 文件不得变化"
            );
        }
    }

    #[test]
    fn mutate_error_prevents_write() {
        let tmp = TempDir::new().unwrap();
        let path = path_of(&tmp);
        write(&path, r#"{"keep":1}"#);
        let before = std::fs::read(&path).unwrap();
        let err = update_raw_settings(&path, false, |map| {
            map.insert("k".to_string(), json!(1));
            Err(RawWriteError::MalformedTarget {
                path: path.clone(),
                key: "k".to_string(),
                expected: "object",
            })
        })
        .unwrap_err();
        assert!(matches!(err, RawWriteError::MalformedTarget { .. }));
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn noop_skips_write_and_preserves_mtime() {
        let tmp = TempDir::new().unwrap();
        let path = path_of(&tmp);
        write(&path, r#"{"keep":1}"#);
        let before = std::fs::read(&path).unwrap();
        let before_mtime = mtime(&path);
        let outcome = update_raw_settings(&path, false, |_map| Ok(())).unwrap();
        assert!(!outcome.changed && !outcome.wrote);
        assert_eq!(mtime(&path), before_mtime);
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn project_scope_preserves_read_roots_and_reports_warning() {
        // A+：project scope 写入保留 readRoots（不主动删除），并产生 WARN 文案。
        let tmp = TempDir::new().unwrap();
        let path = path_of(&tmp);
        write(&path, r#"{"readRoots":["/x"],"keep":1}"#);
        let before_mtime = mtime(&path);
        let outcome = update_raw_settings(&path, true, |map| {
            map.insert("newKey".to_string(), json!(2));
            Ok(())
        })
        .unwrap();
        assert!(outcome.changed && outcome.wrote);
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved["readRoots"], json!(["/x"]), "readRoots 必须保留");
        assert_eq!(saved["keep"], json!(1));
        assert_eq!(saved["newKey"], json!(2));
        assert_ne!(mtime(&path), before_mtime);

        // 无变化时连 WARN 都不应触发写盘；键仍在盘上。
        write(&path, r#"{"readRoots":["/x"],"keep":1}"#);
        let before = std::fs::read(&path).unwrap();
        let outcome = update_raw_settings(&path, true, |_map| Ok(())).unwrap();
        assert!(!outcome.changed && !outcome.wrote);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let warning = project_read_roots_warning(&path, &read_strict_map(&path).unwrap())
            .expect("readRoots 存在时必须产生 WARN");
        assert!(warning.contains("readRoots"));
        assert!(warning.contains("preserved"));
    }

    #[test]
    fn field_accessors_validate_container_types() {
        let tmp = TempDir::new().unwrap();
        let path = path_of(&tmp);
        let mut map = Map::new();

        // 缺失 → 物化；opt 版本缺失 → None 且不物化。
        assert!(object_field_mut(&mut map, "obj", &path).unwrap().is_empty());
        assert!(map.contains_key("obj"));
        assert!(object_field_opt_mut(&mut map, "missing", &path)
            .unwrap()
            .is_none());
        assert!(!map.contains_key("missing"));
        assert!(array_field_mut(&mut map, "arr", &path).unwrap().is_empty());
        assert!(array_field_opt_mut(&mut map, "missing_arr", &path)
            .unwrap()
            .is_none());

        // 类型不符 → fail-closed（含 opt array 版本）。
        map.insert("bad_obj".to_string(), json!("x"));
        map.insert("bad_arr".to_string(), json!({}));
        map.insert("bad_bool".to_string(), json!("true"));
        map.insert("null_bool".to_string(), Value::Null);
        assert!(matches!(
            object_field_mut(&mut map, "bad_obj", &path),
            Err(RawWriteError::MalformedTarget { .. })
        ));
        assert!(matches!(
            object_field_opt_mut(&mut map, "bad_obj", &path),
            Err(RawWriteError::MalformedTarget { .. })
        ));
        assert!(matches!(
            array_field_mut(&mut map, "bad_arr", &path),
            Err(RawWriteError::MalformedTarget { .. })
        ));
        assert!(matches!(
            array_field_opt_mut(&mut map, "bad_arr", &path),
            Err(RawWriteError::MalformedTarget { .. })
        ));
        assert!(matches!(
            set_bool_field(&mut map, "bad_bool", true, &path),
            Err(RawWriteError::MalformedTarget { .. })
        ));
        assert!(matches!(
            set_bool_field(&mut map, "null_bool", true, &path),
            Err(RawWriteError::MalformedTarget { .. })
        ));
        assert!(matches!(
            ensure_object_container(&map, "bad_obj", &path),
            Err(RawWriteError::MalformedTarget { .. })
        ));
    }

    #[test]
    fn set_bool_field_writes_only_on_change() {
        let tmp = TempDir::new().unwrap();
        let path = path_of(&tmp);
        let mut map = Map::new();
        set_bool_field(&mut map, "b", true, &path).unwrap();
        assert_eq!(map["b"], json!(true));
        set_bool_field(&mut map, "b", true, &path).unwrap();
        assert_eq!(map["b"], json!(true));
        set_bool_field(&mut map, "b", false, &path).unwrap();
        assert_eq!(map["b"], json!(false));
    }

    #[test]
    fn into_store_maps_corrupt_and_malformed_to_invalid_data() {
        let path = PathBuf::from("/tmp/settings.json");
        let corrupt = RawWriteError::Corrupt {
            path: path.clone(),
            reason: "boom".to_string(),
        };
        match corrupt.into_store() {
            SettingsStoreError::Io { path: p, source } => {
                assert_eq!(p, path);
                assert_eq!(source.kind(), io::ErrorKind::InvalidData);
                assert!(source.to_string().contains("boom"));
            }
            other => panic!("unexpected {other:?}"),
        }
        let malformed = RawWriteError::MalformedTarget {
            path: path.clone(),
            key: "enabledPlugins".to_string(),
            expected: "object",
        };
        match malformed.into_store() {
            SettingsStoreError::Io { source, .. } => {
                assert_eq!(source.kind(), io::ErrorKind::InvalidData);
                assert!(source.to_string().contains("enabledPlugins"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[cfg(feature = "cli")]
    #[test]
    fn into_io_error_kinds_and_path() {
        let path = PathBuf::from("/tmp/settings.json");
        let corrupt = RawWriteError::Corrupt {
            path,
            reason: "boom".to_string(),
        }
        .into_io_error();
        assert_eq!(corrupt.kind(), io::ErrorKind::InvalidData);
        let io_err = RawWriteError::Io {
            path: PathBuf::from("/tmp/x"),
            source: io::Error::new(io::ErrorKind::PermissionDenied, "denied"),
        }
        .into_io_error();
        assert_eq!(io_err.kind(), io::ErrorKind::PermissionDenied);
        assert!(
            io_err.to_string().contains("/tmp/x"),
            "io 错误必须保留 path: {io_err}"
        );
    }
}
