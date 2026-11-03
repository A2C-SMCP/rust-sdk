//! #245 / #253：settings 原始 JSON 的宽容读（`settings edit` 写后 WARN 专用）。
//!
//! **写路径全局约定（#253）**：除显式例外——`config::validate::migrate_config`（有意有损规范化）、
//! `config::crud::save_config`（整文档 import/duplicate）与 `settings_edit`（外部编辑器直接写文件）——
//! 所有 settings 文件 writer 均经 `settings::raw_write::update_raw_settings` 做 **strict raw RMW**：
//! 只改目标键、保留其余字段（含未知键 / 被校验过滤字段 / 非法兄弟条目）、corrupt 或目标容器畸形
//! **fail-closed**（拒绝写盘、原文件逐字节不变）。
//!
//! **A+（2026-11-03 用户拍板）**：project scope 的 `readRoots` 写入期**保留 + WARN**，读面
//! trusted-scope-only 拒绝保持不动；本模块不再提供任何主动删除 `readRoots` 的助手。
//!
//! `config/executor.rs` 的 corrupt root 已 fail-closed（`CorruptTarget`）；其目标容器畸形校验、
//! 统一 helper 复用**以及 project `readRoots` 剔除的 A+ 对齐**留 PR-B（v0.5.1）。
//!
//! 本模块的 [`read_raw_map`] 是**宽容读**（畸形 / 非对象按缺失处理），仅供 `settings edit` 写后
//! WARN 判断；**不得**用于写路径——写路径请用 `raw_write::read_strict_map`。

use std::path::Path;

use serde_json::{Map, Value};

/// 读取 settings 原始 JSON object（宽容读；`settings edit` 写后 WARN 专用）。
///
/// 缺失 → `None`；空文件 → `Some(空 map)`；畸形 JSON / 非对象 → `None`（按缺失处理，错误由
/// 加载 / 校验路径呈现）。
#[cfg_attr(not(feature = "cli"), allow(dead_code))]
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
