/*!
* 文件名: value_store.rs
* 作者: JQQ
* 创建日期: 2026/06/04
* 最后修改日期: 2026/06/04
* 版权: 2023 JQQ. All rights reserved.
* 依赖: serde_json, smcp::utils::{atomic_io,path}, dirs
* 描述: 非密钥 input 值的明文持久化（XDG state，原子写 + 0600）。
*       Plaintext persistence for non-secret input values (XDG state, atomic + 0600).
*/

//! 非密钥 input 值的明文持久化（§9.3）。
//!
//! 对标 Python 治理层资产 `a2c_smcp/computer/inputs/value_store.py`（v0.2.1 #65）。
//! 对标 VS Code：promptString / pickString 这类**非密钥**输入首次解析后落明文 state，跨重启复用、不再
//! prompt。密钥（`password:true`）**绝不**进此文件——走 [`SecretStore`](super::secret_store::SecretStore)
//! 的 OS keyring（§9.3 安全不变量）。文件位于 `$XDG_STATE_HOME/a2c/input-values.json`（XDG-first，回退
//! `~/.local/state/a2c`），`0600` 权限、原子写、应 gitignore。
//!
//! 键空间：**全局** `<id>`（与「Input id 全局唯一」一致；无 workspace 维度）。

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use smcp::utils::atomic_io::atomic_write_bytes_mode;
use smcp::utils::path::resolve_xdg_first;

use crate::settings::scope::EnvMap;

/// `$XDG_STATE_HOME` 环境变量名 / XDG state home env var。
pub const XDG_STATE_HOME_ENV: &str = "XDG_STATE_HOME";
/// a2c state 子目录名 / a2c state subdir。
const A2C_STATE_SUBDIR: &str = "a2c";
/// 明文 value store 文件名 / plaintext value-store filename。
pub const VALUE_STORE_FILENAME: &str = "input-values.json";

fn env_lookup(env: Option<&EnvMap>, key: &str) -> Option<String> {
    match env {
        Some(e) => e.get(key).cloned(),
        None => std::env::var(key).ok(),
    }
}

/// 解析 `~`（回退基目录）：注入 env 的 `$HOME` 优先，否则进程 home / resolve the home base dir。
fn home_base(env: Option<&EnvMap>) -> PathBuf {
    if let Some(h) = env_lookup(env, "HOME") {
        if !h.trim().is_empty() {
            return PathBuf::from(h);
        }
    }
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// 解析明文 value store 路径 / Resolve the plaintext value-store path。
///
/// 优先 `$XDG_STATE_HOME/a2c/input-values.json`（`XDG_STATE_HOME` 须为绝对路径，否则按 XDG 规范忽略），
/// 回退 `~/.local/state/a2c/input-values.json`。复用 [`resolve_xdg_first`]（与 settings user config dir 同范式）。
pub fn resolve_value_store_path(env: Option<&EnvMap>) -> PathBuf {
    let xdg = env_lookup(env, XDG_STATE_HOME_ENV);
    let fallback = home_base(env)
        .join(".local")
        .join("state")
        .join(A2C_STATE_SUBDIR);
    resolve_xdg_first(xdg.as_deref(), &[A2C_STATE_SUBDIR], &fallback).join(VALUE_STORE_FILENAME)
}

/// 非密钥 input 值的明文存储（`{id: value}` 扁平 map），原子写 + `0600` / plaintext value store。
pub struct ValueStore {
    path: PathBuf,
}

impl ValueStore {
    /// 按 env 解析路径构造 / construct with the env-resolved path。
    pub fn new(env: Option<&EnvMap>) -> Self {
        Self {
            path: resolve_value_store_path(env),
        }
    }

    /// 用显式路径构造（测试）/ construct with an explicit path (tests)。
    pub fn with_path(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// store 文件路径 / the store path。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 读取全量 map；文件不存在 / 畸形 JSON → `{}`（field-tolerant，不抛）/ Load the whole map。
    pub fn load(&self) -> Map<String, Value> {
        let raw = match std::fs::read_to_string(&self.path) {
            Ok(s) => s,
            Err(_) => return Map::new(),
        };
        match serde_json::from_str::<Value>(&raw) {
            Ok(Value::Object(m)) => m,
            _ => {
                tracing::warn!(path = %self.path.display(), "value store JSON malformed, treating as empty");
                Map::new()
            }
        }
    }

    /// 按 id 取值；不存在 → `None` / Get value by id。
    pub fn get(&self, input_id: &str) -> Option<Value> {
        self.load().get(input_id).cloned()
    }

    /// 写入 / 覆盖 id 的值（原子写 + `0600`）/ Set value for id (atomic + 0600)。
    pub fn set(&self, input_id: &str, value: Value) -> std::io::Result<()> {
        let mut data = self.load();
        data.insert(input_id.to_string(), value);
        self.atomic_write(&data)
    }

    /// 删除 id 的值，返回是否删除发生 / Delete value for id; whether deletion happened。
    pub fn delete(&self, input_id: &str) -> std::io::Result<bool> {
        let mut data = self.load();
        if data.remove(input_id).is_none() {
            return Ok(false);
        }
        self.atomic_write(&data)?;
        Ok(true)
    }

    fn atomic_write(&self, data: &Map<String, Value>) -> std::io::Result<()> {
        let text = format!(
            "{}\n",
            serde_json::to_string_pretty(data)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?
        );
        // 0600 在临时文件**创建期即设定**（rename 后无宽权限窗口）；非 unix mode 被忽略。value store 虽仅存
        // 非密钥，仍按最小可见性硬化。
        atomic_write_bytes_mode(&self.path, text.as_bytes(), Some(0o600))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    #[test]
    fn resolve_path_prefers_absolute_xdg_state_home() {
        let mut env = EnvMap::new();
        env.insert(XDG_STATE_HOME_ENV.to_string(), "/abs/state".to_string());
        let p = resolve_value_store_path(Some(&env));
        assert_eq!(p, PathBuf::from("/abs/state/a2c/input-values.json"));
    }

    #[test]
    fn resolve_path_falls_back_to_home_local_state() {
        let mut env = EnvMap::new();
        env.insert("HOME".to_string(), "/home/u".to_string());
        // 相对 / 缺失 XDG_STATE_HOME → 回退 ~/.local/state/a2c
        let p = resolve_value_store_path(Some(&env));
        assert_eq!(
            p,
            PathBuf::from("/home/u/.local/state/a2c/input-values.json")
        );
    }

    #[test]
    fn set_get_delete_roundtrip() {
        let dir = TempDir::new().unwrap();
        let store = ValueStore::with_path(dir.path().join("input-values.json"));
        assert_eq!(store.get("region"), None);
        store.set("region", json!("us-east-1")).unwrap();
        assert_eq!(store.get("region"), Some(json!("us-east-1")));
        // 覆盖 / overwrite
        store.set("region", json!("eu-west-1")).unwrap();
        assert_eq!(store.get("region"), Some(json!("eu-west-1")));
        // 删除 / delete
        assert!(store.delete("region").unwrap());
        assert!(!store.delete("region").unwrap()); // 二次删除未发生
        assert_eq!(store.get("region"), None);
    }

    #[test]
    fn load_missing_or_malformed_is_empty() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("input-values.json");
        let store = ValueStore::with_path(&path);
        assert!(store.load().is_empty()); // 缺失 → {}
        std::fs::write(&path, "{ not json").unwrap();
        assert!(store.load().is_empty()); // 畸形 → {}
    }

    #[cfg(unix)]
    #[test]
    fn written_file_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let store = ValueStore::with_path(dir.path().join("input-values.json"));
        store.set("k", json!("v")).unwrap();
        let mode = std::fs::metadata(store.path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
}
