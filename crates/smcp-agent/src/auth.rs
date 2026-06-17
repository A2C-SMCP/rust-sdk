/*!
* 文件名: auth
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: None
* 描述: SMCP Agent认证提供者 / SMCP Agent authentication provider
*/

use std::collections::HashMap;

/// 默认鉴权字段名（Socket.IO CONNECT `auth` dict 内的键）/ Default auth field name within the
/// Socket.IO CONNECT `auth` dict.
///
/// #86：连接面鉴权走 Socket.IO `auth` dict（不再用 HTTP header）。A2C-SMCP auth-agnostic，
/// 默认 `token`（对齐 server 默认），可通过 [`DefaultAuthProvider::with_auth_field_name`] 覆盖。
/// #86: connection auth lives in the Socket.IO `auth` dict (no HTTP header); defaults to `token`.
pub const DEFAULT_AUTH_FIELD_NAME: &str = "token";

/// Agent配置信息
#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub agent: String,
    pub office_id: String,
}

/// 认证提供者trait
pub trait AuthProvider: Send + Sync {
    /// 获取Agent配置
    fn get_agent_config(&self) -> &AgentConfig;

    /// 获取API密钥（可选）/ API key (optional).
    fn get_api_key(&self) -> Option<String> {
        None
    }

    /// auth dict 内密钥字段名；默认 [`DEFAULT_AUTH_FIELD_NAME`] (`token`)。
    /// Key field name within the auth dict; defaults to [`DEFAULT_AUTH_FIELD_NAME`].
    fn get_auth_field_name(&self) -> &str {
        DEFAULT_AUTH_FIELD_NAME
    }

    /// 连接时的 Socket.IO `auth` dict（#86 连接面鉴权唯一信道）。
    /// 默认把 [`Self::get_api_key`] 包成 `{ get_auth_field_name(): api_key }`。
    /// Connection-time Socket.IO `auth` dict (sole connection-auth channel since #86).
    fn get_connection_auth(&self) -> Option<serde_json::Value> {
        self.get_api_key().map(|key| {
            let mut map = serde_json::Map::new();
            map.insert(
                self.get_auth_field_name().to_string(),
                serde_json::Value::String(key),
            );
            serde_json::Value::Object(map)
        })
    }

    /// 连接时的路由 HTTP headers（**非鉴权**；默认空，自定义 provider 可加 `X-TF-*` 等路由头）。
    /// Connection-time routing HTTP headers (NOT auth; empty by default).
    fn get_connection_headers(&self) -> HashMap<String, String> {
        HashMap::new()
    }
}

/// 默认认证提供者实现
#[derive(Debug, Clone)]
pub struct DefaultAuthProvider {
    config: AgentConfig,
    api_key: Option<String>,
    auth_field_name: Option<String>,
}

impl DefaultAuthProvider {
    pub fn new(agent: String, office_id: String) -> Self {
        Self {
            config: AgentConfig { agent, office_id },
            api_key: None,
            auth_field_name: None,
        }
    }

    pub fn with_api_key(mut self, api_key: String) -> Self {
        self.api_key = Some(api_key);
        self
    }

    /// 自定义 auth dict 内密钥字段名；未设置时默认 [`DEFAULT_AUTH_FIELD_NAME`] (`token`)。
    /// Customize the auth-dict key field name; defaults to [`DEFAULT_AUTH_FIELD_NAME`].
    pub fn with_auth_field_name(mut self, name: impl Into<String>) -> Self {
        self.auth_field_name = Some(name.into());
        self
    }
}

impl AuthProvider for DefaultAuthProvider {
    fn get_agent_config(&self) -> &AgentConfig {
        &self.config
    }

    fn get_api_key(&self) -> Option<String> {
        self.api_key.clone()
    }

    fn get_auth_field_name(&self) -> &str {
        self.auth_field_name
            .as_deref()
            .unwrap_or(DEFAULT_AUTH_FIELD_NAME)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_auth_provider() {
        let auth = DefaultAuthProvider::new("test-agent".to_string(), "test-office".to_string());

        let config = auth.get_agent_config();
        assert_eq!(config.agent, "test-agent");
        assert_eq!(config.office_id, "test-office");

        // 测试带API key的版本 / Test with API key
        let auth_with_key = auth.with_api_key("test-key".to_string());
        assert_eq!(auth_with_key.get_api_key().unwrap(), "test-key");

        // #86：api_key 进 Socket.IO auth dict（默认字段 `token`），**不**进 HTTP header。
        // #86: the api_key goes into the Socket.IO auth dict (default field `token`), NOT a header.
        let auth_dict = auth_with_key.get_connection_auth().unwrap();
        assert_eq!(auth_dict, serde_json::json!({ "token": "test-key" }));
        assert!(
            auth_with_key.get_connection_headers().is_empty(),
            "Default connection headers must be routing-only (no auth header)"
        );
    }

    #[test]
    fn test_auth_provider_custom_field_name() {
        // 自定义 auth dict 密钥字段名必须真实生效。
        // Custom auth-dict field name must take effect.
        let auth = DefaultAuthProvider::new("test-agent".to_string(), "test-office".to_string())
            .with_api_key("legacy-secret".to_string())
            .with_auth_field_name("x-legacy-key");

        assert_eq!(auth.get_auth_field_name(), "x-legacy-key");

        let auth_dict = auth.get_connection_auth().unwrap();
        assert_eq!(
            auth_dict,
            serde_json::json!({ "x-legacy-key": "legacy-secret" })
        );
        assert!(auth.get_connection_headers().is_empty());
    }
}
