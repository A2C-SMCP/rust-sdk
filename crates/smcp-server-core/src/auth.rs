//! 认证接口抽象定义 / Authentication interface abstract definition

use async_trait::async_trait;
use http::HeaderMap;
use thiserror::Error;

/// 认证错误类型
#[derive(Error, Debug, serde::Serialize)]
pub enum AuthError {
    #[error("Missing API key")]
    MissingApiKey,
    #[error("Invalid API key")]
    InvalidApiKey,
    #[error("Authentication failed: {0}")]
    Failed(String),
}

/// 认证提供者抽象 trait
/// Authentication provider abstract trait
#[async_trait]
pub trait AuthenticationProvider: Send + Sync + 'static + std::fmt::Debug {
    /// 认证连接请求
    /// Authenticate connection request
    ///
    /// # Arguments
    /// * `headers` - HTTP 请求头 / HTTP request headers
    /// * `auth` - 原始认证数据 / Raw authentication data
    ///
    /// # Returns
    /// 认证是否成功 / Whether authentication succeeded
    async fn authenticate(
        &self,
        headers: &HeaderMap,
        auth: Option<&serde_json::Value>,
    ) -> Result<(), AuthError>;
}

/// 默认鉴权字段名（Socket.IO CONNECT `auth` dict 内的键）/ Default auth field name within the
/// Socket.IO CONNECT `auth` dict.
///
/// #86：连接面鉴权统一走 Socket.IO `auth` dict（不再用 HTTP header）。A2C-SMCP 协议 auth-agnostic，
/// 部署方可显式覆盖 `api_key_name`；默认 `token`，对齐 client 侧 `auth_payload({"token": ...})` 与
/// TuringFocus/TFRC token-exchange 契约（AS-38）。
/// #86: connection auth lives in the Socket.IO `auth` dict (no HTTP header). A2C-SMCP is
/// auth-agnostic — operators may override `api_key_name`; defaults to `token`.
pub const DEFAULT_AUTH_FIELD_NAME: &str = "token";

/// 默认认证提供者，提供基础的认证逻辑实现
/// Default authentication provider, provides basic authentication logic implementation
#[derive(Debug, Clone)]
pub struct DefaultAuthenticationProvider {
    /// 管理员密钥 / Admin secret
    admin_secret: Option<String>,
    /// auth dict 内密钥字段名 / Key field name within the auth dict
    api_key_name: String,
}

impl DefaultAuthenticationProvider {
    /// 创建新的默认认证提供者
    /// Create new default authentication provider
    ///
    /// # Arguments
    /// * `admin_secret` - 管理员密钥 / Admin secret
    /// * `api_key_name` - auth dict 内密钥字段名，默认为 [`DEFAULT_AUTH_FIELD_NAME`]
    ///   (`token`) / auth-dict key field name, defaults to
    ///   [`DEFAULT_AUTH_FIELD_NAME`] (`token`)
    pub fn new(admin_secret: Option<String>, api_key_name: Option<String>) -> Self {
        Self {
            admin_secret,
            api_key_name: api_key_name.unwrap_or_else(|| DEFAULT_AUTH_FIELD_NAME.to_string()),
        }
    }
}

#[async_trait]
impl AuthenticationProvider for DefaultAuthenticationProvider {
    async fn authenticate(
        &self,
        _headers: &HeaderMap,
        auth: Option<&serde_json::Value>,
    ) -> Result<(), AuthError> {
        // #86：从 Socket.IO CONNECT `auth` dict 提取密钥（字段 `api_key_name`，默认 `token`）。
        // HTTP header 不再参与连接面鉴权；routing headers（X-TF-*）仍由传输层透传，与鉴权无关。
        // Extract the key from the Socket.IO CONNECT `auth` dict; HTTP headers no longer authenticate.
        let api_key = auth
            .and_then(|value| value.get(self.api_key_name.as_str()))
            .and_then(|value| value.as_str())
            .map(|s| s.to_string());

        let api_key = api_key.ok_or(AuthError::MissingApiKey)?;

        // 检查管理员权限：与配置的管理员密钥比较
        // Check admin permission: compare with configured admin secret
        if let Some(ref admin_secret) = self.admin_secret {
            if api_key.as_str() == admin_secret {
                return Ok(());
            }
        }

        // 这里可以添加其他认证逻辑，如数据库验证等
        // Additional authentication logic can be added here, such as database validation
        Err(AuthError::InvalidApiKey)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn test_default_auth_success() {
        let auth = DefaultAuthenticationProvider::new(Some("secret123".to_string()), None);
        let dict = json!({ "token": "secret123" });

        let result = auth.authenticate(&HeaderMap::new(), Some(&dict)).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_default_auth_missing_key() {
        let auth = DefaultAuthenticationProvider::new(Some("secret123".to_string()), None);

        // auth dict 缺失（None）或无 `token` 字段 → MissingApiKey。
        let result = auth.authenticate(&HeaderMap::new(), None).await;
        assert!(matches!(result, Err(AuthError::MissingApiKey)));

        let empty = json!({});
        let result = auth.authenticate(&HeaderMap::new(), Some(&empty)).await;
        assert!(matches!(result, Err(AuthError::MissingApiKey)));
    }

    #[tokio::test]
    async fn test_default_auth_invalid_key() {
        let auth = DefaultAuthenticationProvider::new(Some("secret123".to_string()), None);
        let dict = json!({ "token": "wrong" });

        let result = auth.authenticate(&HeaderMap::new(), Some(&dict)).await;
        assert!(matches!(result, Err(AuthError::InvalidApiKey)));
    }

    #[tokio::test]
    async fn test_default_auth_no_admin_secret() {
        let auth = DefaultAuthenticationProvider::new(None, None);
        let dict = json!({ "token": "anykey" });

        let result = auth.authenticate(&HeaderMap::new(), Some(&dict)).await;
        assert!(matches!(result, Err(AuthError::InvalidApiKey)));
    }
}
