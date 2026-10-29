//! #245 / #248：`readRoots` 写入期清理的共享 raw 助手。
//!
//! 本模块服务于**已迁移**的 writer：installer（锁内 no-op / 真写）、CLI `settings set/edit`
//! （持锁或复用 [`drop_read_roots_from_file`]）以及 flag 诊断的原始文件读取。写入只删除
//! `readRoots` 这一个键，不回写校验视图（避免静默删除 landingRoot / enabledMcpjsonServers /
//! 非法 permissions 等被过滤字段）。
//!
//! 其它 writer（config executor / marketplace / mcp_config）仍各自处理校验视图；统一收口见架构师
//! follow-up Issue，本模块**不**声称覆盖全部写路径。

use std::path::Path;

use serde_json::{Map, Value};

use super::schema::FIELD_READ_ROOTS;
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
/// 返回是否删除；缺失 / 畸形 → `Ok(false)`；锁 / IO 错误 → `Err(String)`。
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
