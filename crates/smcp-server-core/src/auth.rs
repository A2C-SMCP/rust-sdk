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
    async fn authenticate(&self, headers: &HeaderMap, auth: Option<&serde_json::Value>) -> Result<(), AuthError>;
}

/// 默认认证提供者，提供基础的认证逻辑实现
/// Default authentication provider, provides basic authentication logic implementation
#[derive(Debug, Clone)]
pub struct DefaultAuthenticationProvider {
    /// 管理员密钥 / Admin secret
    admin_secret: Option<String>,
    /// API 密钥字段名 / API key field name
    api_key_name: String,
}

impl DefaultAuthenticationProvider {
    /// 创建新的默认认证提供者
    /// Create new default authentication provider
    ///
    /// # Arguments
    /// * `admin_secret` - 管理员密钥 / Admin secret
    /// * `api_key_name` - API 密钥字段名，默认为 "x-api-key" / API key field name, defaults to "x-api-key"
    pub fn new(admin_secret: Option<String>, api_key_name: Option<String>) -> Self {
        Self {
            admin_secret,
            api_key_name: api_key_name.unwrap_or_else(|| "x-api-key".to_string()),
        }
    }
}

#[async_trait]
impl AuthenticationProvider for DefaultAuthenticationProvider {
    async fn authenticate(&self, headers: &HeaderMap, _auth: Option<&serde_json::Value>) -> Result<(), AuthError> {
        // 从 headers 中提取 API 密钥
        // Extract API key from headers
        let api_key = headers
            .get(self.api_key_name.as_str())
            .and_then(|value| value.to_str().ok())
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
    use http::HeaderValue;

    #[tokio::test]
    async fn test_default_auth_success() {
        let auth = DefaultAuthenticationProvider::new(Some("secret123".to_string()), None);
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("secret123"));

        let result = auth.authenticate(&headers, None).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_default_auth_missing_key() {
        let auth = DefaultAuthenticationProvider::new(Some("secret123".to_string()), None);
        let headers = HeaderMap::new();

        let result = auth.authenticate(&headers, None).await;
        assert!(matches!(result, Err(AuthError::MissingApiKey)));
    }

    #[tokio::test]
    async fn test_default_auth_invalid_key() {
        let auth = DefaultAuthenticationProvider::new(Some("secret123".to_string()), None);
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("wrong"));

        let result = auth.authenticate(&headers, None).await;
        assert!(matches!(result, Err(AuthError::InvalidApiKey)));
    }

    #[tokio::test]
    async fn test_default_auth_no_admin_secret() {
        let auth = DefaultAuthenticationProvider::new(None, None);
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("anykey"));

        let result = auth.authenticate(&headers, None).await;
        assert!(matches!(result, Err(AuthError::InvalidApiKey)));
    }
}
