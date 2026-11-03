//! #245 / #253：`readRoots` 写入期清理与 settings 原始读取的共享 raw 助手。
//!
//! **写路径全局约定（#253）**：除显式例外——`config::validate::migrate_config`（有意有损规范化）、
//! `config::crud::save_config`（整文档 import/duplicate）与 `settings_edit`（外部编辑器直接写文件）——
//! 所有 settings 文件 writer 均经 `settings::raw_write::update_raw_settings` 做 **strict raw RMW**：
//! 只改目标键、保留其余字段（含未知键 / 被校验过滤字段 / 非法兄弟条目）、corrupt 或目标容器畸形
//! **fail-closed**（拒绝写盘、原文件逐字节不变）。`config/executor.rs` 已是 strict raw（`CorruptTarget`），
//! 其复用本 helper 留 PR-B（v0.5.1）。
//!
//! 本模块的 [`read_raw_map`] 是**宽容读**（畸形 / 非对象按缺失处理），供 `drop_read_roots_from_file`
//! 使用；**不得**用于新的写路径——写路径请用 `raw_write::read_strict_map`。

use std::path::Path;

use serde_json::{Map, Value};

use super::schema::FIELD_READ_ROOTS;
// 持锁清理仅在 CLI（`settings set/edit`）路径使用；非 cli 构建下不引入。
#[cfg(feature = "cli")]
use super::store::{atomic_write_settings_json, with_settings_lock};

/// 读取 settings 原始 JSON object（raw 保真）。
///
/// 缺失 → `None`；空文件 → `Some(空 map)`；畸形 JSON / 非对象 → `None`（按缺失处理，错误由
/// 加载 / 校验路径呈现）。
pub(crate) fn read_raw_map(path: &Path) -> std::io::Result<Option<Map<String, Value>>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if text.trim().is_empty() {
        return Ok(Some(Map::new()));
    }
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(map)) => Ok(Some(map)),
        // 畸形 / 非对象：按缺失处理（既有容错姿态，错误由加载/校验路径呈现）。
        _ => Ok(None),
    }
}

/// 从原始 map 顶层删除 `readRoots`；返回是否删除。
///
/// **仅**在 project scope 写入语境调用（其它 scope 该键合法，不得删除）。
pub(crate) fn remove_read_roots_raw(map: &mut Map<String, Value>) -> bool {
    let removed = map.remove(FIELD_READ_ROOTS).is_some();
    if removed {
        tracing::warn!(
            field = FIELD_READ_ROOTS,
            "project scope readRoots dropped before persist (trusted-scope-only)"
        );
    }
    removed
}

/// 持锁 raw 保真删除（读取 / 删除 / 回写在同一把 settings 锁内）。
///
/// 返回是否删除；缺失 / 畸形 → `Ok(false)`（**不覆盖、不报错**）；锁 / IO 错误 → `Err(String)`。
///
/// **仅**用于 `settings edit` 的收尾清理（外部编辑器可直接写出任意内容，含 corrupt——这是 #253 的
/// 显式 carve-out）。`settings set` / installer / marketplace / mcp_config 等写路径必须走
/// `raw_write::update_raw_settings`（strict fail-closed），不得调用本函数。
#[cfg(feature = "cli")]
pub(crate) fn drop_read_roots_from_file(path: &Path) -> Result<bool, String> {
    let written = with_settings_lock(path, || -> std::io::Result<bool> {
        let Some(mut map) = read_raw_map(path)? else {
            return Ok(false);
        };
        if !remove_read_roots_raw(&mut map) {
            return Ok(false);
        }
        atomic_write_settings_json(path, &Value::Object(map))?;
        Ok(true)
    });
    match written {
        Ok(Ok(dropped)) => Ok(dropped),
        Ok(Err(e)) => Err(e.to_string()),
        Err(e) => Err(e.to_string()),
    }
}
