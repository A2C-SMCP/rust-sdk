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

/// 默认鉴权 HTTP header 键名 / Default auth HTTP header key name.
///
/// 与 A2C-SMCP 协议 auth-agnostic 立场一致；默认匹配 TuringFocus 生态
/// (`access_token`，下划线)，可通过 [`DefaultAuthProvider::with_auth_header_name`]
/// 或自定义 [`AuthProvider`] 实现覆盖。
pub const DEFAULT_AUTH_HEADER_NAME: &str = "access_token";

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

    /// 获取连接时的认证数据
    fn get_connection_auth(&self) -> Option<serde_json::Value> {
        None
    }

    /// 鉴权 HTTP header 键名；默认 [`DEFAULT_AUTH_HEADER_NAME`]。
    /// Auth HTTP header key name; defaults to [`DEFAULT_AUTH_HEADER_NAME`].
    fn get_auth_header_name(&self) -> &str {
        DEFAULT_AUTH_HEADER_NAME
    }

    /// 获取连接时的HTTP头部
    fn get_connection_headers(&self) -> HashMap<String, String> {
        let mut headers = HashMap::new();
        if let Some(api_key) = self.get_api_key() {
            headers.insert(self.get_auth_header_name().to_string(), api_key);
        }
        headers
    }

    /// 获取API密钥（可选）
    fn get_api_key(&self) -> Option<String> {
        None
    }
}

/// 默认认证提供者实现
#[derive(Debug, Clone)]
pub struct DefaultAuthProvider {
    config: AgentConfig,
    api_key: Option<String>,
    auth_header_name: Option<String>,
}

impl DefaultAuthProvider {
    pub fn new(agent: String, office_id: String) -> Self {
        Self {
            config: AgentConfig { agent, office_id },
            api_key: None,
            auth_header_name: None,
        }
    }

    pub fn with_api_key(mut self, api_key: String) -> Self {
        self.api_key = Some(api_key);
        self
    }

    /// 自定义鉴权 HTTP header 键名；未设置时默认 [`DEFAULT_AUTH_HEADER_NAME`]。
    /// Customize auth HTTP header key name; defaults to
    /// [`DEFAULT_AUTH_HEADER_NAME`] when not set.
    pub fn with_auth_header_name(mut self, name: impl Into<String>) -> Self {
        self.auth_header_name = Some(name.into());
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

    fn get_auth_header_name(&self) -> &str {
        self.auth_header_name
            .as_deref()
            .unwrap_or(DEFAULT_AUTH_HEADER_NAME)
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

        // 默认鉴权 header 键应为 access_token（A2C-SMCP 协议 auth-agnostic，
        // TF 生态实际使用 access_token）。
        // Default auth header key should be `access_token`.
        let headers = auth_with_key.get_connection_headers();
        assert_eq!(headers.get("access_token").unwrap(), "test-key");
        assert!(
            !headers.contains_key("x-api-key"),
            "Default header should not be the deprecated 'x-api-key'"
        );
    }

    #[test]
    fn test_auth_provider_custom_header_name() {
        // 自定义鉴权 header key 必须真实生效。
        // Custom auth header key must take effect.
        let auth = DefaultAuthProvider::new("test-agent".to_string(), "test-office".to_string())
            .with_api_key("legacy-secret".to_string())
            .with_auth_header_name("x-legacy-key");

        assert_eq!(auth.get_auth_header_name(), "x-legacy-key");

        let headers = auth.get_connection_headers();
        assert_eq!(headers.get("x-legacy-key").unwrap(), "legacy-secret");
        assert!(!headers.contains_key("access_token"));
    }
}
