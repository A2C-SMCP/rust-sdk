/*!
* 文件名: secret_store.rs
* 作者: JQQ
* 创建日期: 2026/06/04
* 最后修改日期: 2026/06/04
* 版权: 2023 JQQ. All rights reserved.
* 依赖: keyring（target-conditional：apple-native / windows-native / sync-secret-service）
* 描述: password:true input 的 OS keyring 封装；不可用时安全降级（绝不落明文）。
*       OS keyring wrapper for secret inputs; degrades safely (never plaintext) when unavailable.
*/

//! 密钥（`password:true` input）的 OS keyring 封装（§9.3）。
//!
//! 对标 Python 治理层资产 `a2c_smcp/computer/inputs/secret_store.py`（v0.2.1 #65）。
//! 对标 VS Code SecretStorage：`keyring` 自动选 macOS Keychain / Windows Credential Manager / Linux
//! Secret Service。
//!
//! `keyring` 为 **target-conditional 依赖**（apple-native / windows-native / sync-secret-service），不支持的
//! 平台编译为 always-unavailable stub。运行期再经 [`keyring_available`] 探测后端（容器 / CI / 无 Secret
//! Service → 不可用），不可用时 [`SecretStore`] `get→None` / `set→false`（**绝不**降级落明文，§9.3）。
//!
//! 键空间：service=`a2c-computer`、key=**全局 `<id>`**（与「Input id 全局唯一」一致）。
//!
//! 可测试性：[`SecretStore`] 持可注入的 [`KeyringBackend`]（真实 [`OsKeyring`] / 测试用内存 fake /
//! always-unavailable），令降级与命中两条路径均可确定性单测、不碰真实钥匙串。

use std::sync::atomic::{AtomicU8, Ordering};

/// keyring service 名（VS Code-style 全局命名空间）/ keyring service name。
pub const SERVICE_NAME: &str = "a2c-computer";

/// 探测用的占位 key（绝不存在的条目，用于判定后端是否可用）/ probe key for backend availability。
const PROBE_KEY: &str = "__a2c_keyring_probe__";

/// 当前 target 是否有 keyring 后端 / whether a keyring backend is compiled for this target。
const KEYRING_COMPILED: bool = cfg!(any(
    target_os = "macos",
    target_os = "windows",
    target_os = "linux",
    target_os = "freebsd",
    target_os = "openbsd"
));

/// OS keyring 后端抽象（可注入：真实 / fake / 不可用）/ pluggable keyring backend seam。
pub trait KeyringBackend: Send + Sync {
    /// 后端是否可用（容器 / CI / 缺后端 → false）/ whether the backend is usable。
    fn available(&self) -> bool;
    /// 读密钥；不可用或未存 → `None` / get secret。
    fn get(&self, service: &str, id: &str) -> Option<String>;
    /// 写密钥，返回是否成功；不可用 → `false` / set secret。
    fn set(&self, service: &str, id: &str, value: &str) -> bool;
    /// 删除密钥，返回是否删除发生 / delete secret。
    fn delete(&self, service: &str, id: &str) -> bool;
}

/// 真实 OS keyring 后端（支持平台用 `keyring` crate，其它平台 always-unavailable）/ the real OS backend。
#[derive(Debug, Default, Clone, Copy)]
pub struct OsKeyring;

#[cfg(any(
    target_os = "macos",
    target_os = "windows",
    target_os = "linux",
    target_os = "freebsd",
    target_os = "openbsd"
))]
impl KeyringBackend for OsKeyring {
    fn available(&self) -> bool {
        use keyring::{Entry, Error};
        match Entry::new(SERVICE_NAME, PROBE_KEY) {
            // NoEntry = 后端工作但 key 不存在（可用）；其它错误（PlatformFailure / NoStorageAccess）= 不可用。
            Ok(entry) => matches!(entry.get_password(), Ok(_) | Err(Error::NoEntry)),
            Err(_) => false,
        }
    }

    fn get(&self, service: &str, id: &str) -> Option<String> {
        keyring::Entry::new(service, id).ok()?.get_password().ok()
    }

    fn set(&self, service: &str, id: &str, value: &str) -> bool {
        keyring::Entry::new(service, id)
            .and_then(|e| e.set_password(value))
            .is_ok()
    }

    fn delete(&self, service: &str, id: &str) -> bool {
        keyring::Entry::new(service, id)
            .and_then(|e| e.delete_credential())
            .is_ok()
    }
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "windows",
    target_os = "linux",
    target_os = "freebsd",
    target_os = "openbsd"
)))]
impl KeyringBackend for OsKeyring {
    fn available(&self) -> bool {
        false
    }
    fn get(&self, _service: &str, _id: &str) -> Option<String> {
        None
    }
    fn set(&self, _service: &str, _id: &str, _value: &str) -> bool {
        false
    }
    fn delete(&self, _service: &str, _id: &str) -> bool {
        false
    }
}

/// 可用性探测缓存（0=未探测，1=false，2=true）/ availability probe cache。
static AVAILABILITY: AtomicU8 = AtomicU8::new(0);

/// 探测 OS keyring 是否可用（结果缓存，`force_recheck` 可重探）/ Probe keyring availability (cached)。
///
/// 判定：本 target 编译了 keyring 后端 **且** 运行期后端非 fail（容器 / 无 Secret Service 时不可用）。
pub fn keyring_available(force_recheck: bool) -> bool {
    if !KEYRING_COMPILED {
        return false;
    }
    if !force_recheck {
        match AVAILABILITY.load(Ordering::Relaxed) {
            1 => return false,
            2 => return true,
            _ => {}
        }
    }
    let avail = OsKeyring.available();
    AVAILABILITY.store(if avail { 2 } else { 1 }, Ordering::Relaxed);
    avail
}

/// `password:true` input 值的 OS keyring 存取；keyring 不可用时安全降级 / OS keyring store for secrets。
pub struct SecretStore {
    service: String,
    backend: Box<dyn KeyringBackend>,
}

impl SecretStore {
    /// 用真实 OS keyring 后端构造 / construct with the real OS backend。
    pub fn new() -> Self {
        Self {
            service: SERVICE_NAME.to_string(),
            backend: Box::new(OsKeyring),
        }
    }

    /// 注入后端构造（测试 / 替身）/ construct with an injected backend (tests)。
    pub fn with_backend(service: impl Into<String>, backend: Box<dyn KeyringBackend>) -> Self {
        Self {
            service: service.into(),
            backend,
        }
    }

    /// 后端是否可用 / whether the backend is usable。
    pub fn available(&self) -> bool {
        self.backend.available()
    }

    /// 从 keyring 取密钥；不可用或未存 → `None` / Get secret。
    pub fn get(&self, input_id: &str) -> Option<String> {
        if !self.backend.available() {
            return None;
        }
        self.backend.get(&self.service, input_id)
    }

    /// 写密钥进 keyring，返回是否成功；不可用 → `false`（**绝不**降级落明文）/ Store secret。
    pub fn set(&self, input_id: &str, value: &str) -> bool {
        if !self.backend.available() {
            return false;
        }
        self.backend.set(&self.service, input_id, value)
    }

    /// 从 keyring 删除密钥，返回是否删除发生 / Delete secret。
    pub fn delete(&self, input_id: &str) -> bool {
        if !self.backend.available() {
            return false;
        }
        self.backend.delete(&self.service, input_id)
    }
}

impl Default for SecretStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// 内存 fake keyring（可用，确定性）/ in-memory fake keyring (available, deterministic)。
    #[derive(Default)]
    struct FakeKeyring {
        store: Mutex<HashMap<String, String>>,
    }

    impl KeyringBackend for FakeKeyring {
        fn available(&self) -> bool {
            true
        }
        fn get(&self, _service: &str, id: &str) -> Option<String> {
            self.store.lock().unwrap().get(id).cloned()
        }
        fn set(&self, _service: &str, id: &str, value: &str) -> bool {
            self.store
                .lock()
                .unwrap()
                .insert(id.to_string(), value.to_string());
            true
        }
        fn delete(&self, _service: &str, id: &str) -> bool {
            self.store.lock().unwrap().remove(id).is_some()
        }
    }

    /// always-unavailable 后端（模拟容器 / 无 Secret Service）/ always-unavailable backend。
    struct UnavailableKeyring;
    impl KeyringBackend for UnavailableKeyring {
        fn available(&self) -> bool {
            false
        }
        fn get(&self, _: &str, _: &str) -> Option<String> {
            None
        }
        fn set(&self, _: &str, _: &str, _: &str) -> bool {
            false
        }
        fn delete(&self, _: &str, _: &str) -> bool {
            false
        }
    }

    #[test]
    fn available_backend_roundtrips() {
        let store = SecretStore::with_backend(SERVICE_NAME, Box::new(FakeKeyring::default()));
        assert!(store.available());
        assert_eq!(store.get("figma_token"), None);
        assert!(store.set("figma_token", "s3cr3t"));
        assert_eq!(store.get("figma_token").as_deref(), Some("s3cr3t"));
        assert!(store.delete("figma_token"));
        assert!(!store.delete("figma_token")); // 二次删除未发生 / second delete is a no-op
        assert_eq!(store.get("figma_token"), None);
    }

    #[test]
    fn unavailable_backend_degrades_never_plaintext() {
        let store = SecretStore::with_backend(SERVICE_NAME, Box::new(UnavailableKeyring));
        assert!(!store.available());
        // get → None / set → false / delete → false（绝不落明文）
        assert_eq!(store.get("x"), None);
        assert!(!store.set("x", "v"));
        assert!(!store.delete("x"));
    }

    #[test]
    fn keyring_available_returns_false_off_supported_platform() {
        // 本机为支持平台（macOS），故仅断言函数不 panic 且与 KEYRING_COMPILED 一致地可调用。
        let _ = keyring_available(true);
        if !KEYRING_COMPILED {
            assert!(!keyring_available(false));
        }
    }
}
