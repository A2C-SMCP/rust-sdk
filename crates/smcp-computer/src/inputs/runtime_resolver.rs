/*!
* 文件名: runtime_resolver.rs
* 作者: JQQ
* 创建日期: 2026/07/10
* 最后修改日期: 2026/07/10
* 版权: 2023 JQQ. All rights reserved.
* 依赖: async-trait, serde_json, mcp_clients::model, inputs::{resolver,secret_store}
* 描述: D1（#107 S5 / #112）运行期 input/secret 注入契约：SDK 不落盘明文值/secret，运行期由 client 注入。
*       D1 runtime input/secret injection contract; SDK never persists plaintext, client injects at runtime.
*/

//! D1 运行期 resolver 契约（#107 S5 / #112）。
//!
//! **边界**（父 #107 D1）：input **定义**留 SDK-owned config（协议 §2.1，见 `settings::config::InputDefsView`）；
//! resolved **值 / secret 明文** 移出 SDK——运行期由 client 经 [`InputValueResolver`] / [`SecretValueResolver`]
//! 注入（协议 §2.2 / §6）。这两个 trait 即 `RuntimeOptions.input_resolver` / `secret_resolver` 契约本体，
//! 经 `Computer::with_input_resolver` / `with_secret_resolver` 注入。
//!
//! **结构化缺失**：input 未解析且无默认值 → [`InputResolutionError::Missing`]（**非仅日志**），由 client 决定补录
//! UI，绝不静默用空串。
//!
//! **keyring 降级为 resolver 实现**：[`SecretStore`](super::secret_store::SecretStore)（OS keyring）在 D1 下不再属
//! SDK-owned config，而是 [`SecretValueResolver`] 的一种 **opt-in** 实现（[`KeyringSecretResolver`]）。明文 value
//! store 已于 S5 **硬退役**——SDK 不再落盘任何 input 明文值。

use async_trait::async_trait;
use serde_json::Value;

use crate::mcp_clients::model::MCPServerInput;

use super::resolver::env_var_name;
use super::secret_store::SecretStore;

/// input 种类（供 client 区分补录 UI / 结构化错误分流）/ input kind for client UI & error triage。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    /// 非密钥（promptString / pickString / command）/ non-secret。
    Value,
    /// 密钥（`password:true`）/ secret。
    Secret,
}

impl std::fmt::Display for InputKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InputKind::Value => write!(f, "value"),
            InputKind::Secret => write!(f, "secret"),
        }
    }
}

/// D1 结构化 input 解析错误（**非仅日志**，供 client 驱动补录）/ structured input-resolution error。
#[derive(Debug, Clone, thiserror::Error)]
pub enum InputResolutionError {
    /// 必填 input/secret 未解析且无默认值——client 须经 resolver 或环境变量补录 / required input unresolved。
    #[error(
        "required {kind} input '{id}' is unresolved; supply it via a runtime resolver or set env {env_hint}"
    )]
    Missing {
        /// input id（解析后，可能带 plugin 前缀）/ resolved input id。
        id: String,
        /// 种类（value / secret）/ kind。
        kind: InputKind,
        /// 环境变量补录名（`A2C_INPUT_<ID>`）/ env fallback var name。
        env_hint: String,
    },
    /// client resolver 侧硬失败（区别于"未提供"的 `Ok(None)`）/ client resolver hard failure。
    #[error("resolver failed for input '{id}': {reason}")]
    ResolverFailed {
        /// input id / input id。
        id: String,
        /// 失败原因 / failure reason。
        reason: String,
    },
}

impl InputResolutionError {
    /// 构造 [`InputResolutionError::Missing`]（自动派生 `A2C_INPUT_<ID>` 补录名）/ build a Missing error。
    pub fn missing(id: impl Into<String>, kind: InputKind) -> Self {
        let id = id.into();
        let env_hint = env_var_name(&id);
        Self::Missing { id, kind, env_hint }
    }

    /// 构造 [`InputResolutionError::ResolverFailed`] / build a ResolverFailed error。
    pub fn resolver_failed(id: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::ResolverFailed {
            id: id.into(),
            reason: reason.into(),
        }
    }
}

/// input 值解析 provider（client 注入；= `RuntimeOptions.input_resolver`）/ client input resolver。
///
/// 语义：`Ok(Some(v))` 命中 → 采用；`Ok(None)` 未提供 → 走后续回退（env / session / 默认值）；`Err` client 侧硬错
/// → 上抛结构化错误。仅用于**非密钥** input（promptString / pickString）；密钥走 [`SecretValueResolver`]。
#[async_trait]
pub trait InputValueResolver: Send + Sync {
    /// 解析非密钥 input 值 / resolve a non-secret input value。
    async fn resolve_input(
        &self,
        def: &MCPServerInput,
    ) -> Result<Option<Value>, InputResolutionError>;
}

/// secret 解析 provider（client 注入；= `RuntimeOptions.secret_resolver`）/ client secret resolver。
///
/// 语义同 [`InputValueResolver`]，仅用于 `password:true` input，返回 secret 明文（SDK 不落盘、仅运行期在内存渲染）。
#[async_trait]
pub trait SecretValueResolver: Send + Sync {
    /// 解析 `password:true` input 的 secret 明文 / resolve a secret's plaintext。
    async fn resolve_secret(
        &self,
        def: &MCPServerInput,
    ) -> Result<Option<String>, InputResolutionError>;
}

/// 基于 OS keyring 的 [`SecretValueResolver`]（D1：keyring = resolver 的一种实现，**opt-in**）/ keyring-backed resolver。
///
/// 客户端可 `Computer::with_secret_resolver(Arc::new(KeyringSecretResolver::new()))` 让 secret 走 OS keyring；不注入
/// 则 keyring 完全不参与解析（对齐 D1「keyring 不属 SDK-owned config」）。
pub struct KeyringSecretResolver {
    store: SecretStore,
}

impl KeyringSecretResolver {
    /// 用真实 OS keyring 构造 / construct with the real OS keyring。
    pub fn new() -> Self {
        Self {
            store: SecretStore::new(),
        }
    }

    /// 注入 [`SecretStore`]（测试 / 部署替身）/ construct with an injected store (tests)。
    pub fn with_store(store: SecretStore) -> Self {
        Self { store }
    }
}

impl Default for KeyringSecretResolver {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SecretValueResolver for KeyringSecretResolver {
    async fn resolve_secret(
        &self,
        def: &MCPServerInput,
    ) -> Result<Option<String>, InputResolutionError> {
        // keyring 为同步阻塞调用；条目短、与既有 `SecretStore` 用法一致，不额外 `spawn_blocking`。keyring 不可用时
        // `get` 返回 `None`（绝不降级落明文，见 secret_store §9.3），此处即视为「未提供」走后续回退/结构化缺失。
        Ok(self.store.get(def.id()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inputs::secret_store::{KeyringBackend, SecretStore, SERVICE_NAME};
    use crate::mcp_clients::model::PromptStringInput;
    use std::collections::HashMap;
    use std::sync::Mutex;

    fn secret_input(id: &str) -> MCPServerInput {
        MCPServerInput::PromptString(PromptStringInput {
            id: id.to_string(),
            description: String::new(),
            default: None,
            password: Some(true),
        })
    }

    /// 内存 fake keyring（可用，确定性）/ in-memory fake keyring。
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
    fn missing_error_carries_kind_and_env_hint() {
        let err = InputResolutionError::missing("figma_token", InputKind::Secret);
        match &err {
            InputResolutionError::Missing { id, kind, env_hint } => {
                assert_eq!(id, "figma_token");
                assert_eq!(*kind, InputKind::Secret);
                assert_eq!(env_hint, "A2C_INPUT_FIGMA_TOKEN");
            }
            other => panic!("expected Missing, got {other:?}"),
        }
        // Display 面向 client 补录：含 id + 种类 + env 补录名。
        let msg = err.to_string();
        assert!(msg.contains("figma_token"));
        assert!(msg.contains("secret"));
        assert!(msg.contains("A2C_INPUT_FIGMA_TOKEN"));
    }

    #[tokio::test]
    async fn keyring_resolver_hits_available_backend() {
        let backend = FakeKeyring::default();
        backend.set(SERVICE_NAME, "tok", "kr-secret");
        let resolver = KeyringSecretResolver::with_store(SecretStore::with_backend(
            SERVICE_NAME,
            Box::new(backend),
        ));
        assert_eq!(
            resolver.resolve_secret(&secret_input("tok")).await.unwrap(),
            Some("kr-secret".to_string())
        );
        // 未存的 id → None（走后续回退/结构化缺失，非硬错）。
        assert_eq!(
            resolver
                .resolve_secret(&secret_input("absent"))
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn keyring_resolver_degrades_to_none_when_unavailable() {
        let resolver = KeyringSecretResolver::with_store(SecretStore::with_backend(
            SERVICE_NAME,
            Box::new(UnavailableKeyring),
        ));
        // keyring 不可用 → None（绝不 panic、绝不落明文）。
        assert_eq!(
            resolver.resolve_secret(&secret_input("tok")).await.unwrap(),
            None
        );
    }
}
