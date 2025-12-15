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

    /// 获取连接时的HTTP头部
    fn get_connection_headers(&self) -> HashMap<String, String> {
        let mut headers = HashMap::new();
        if let Some(api_key) = self.get_api_key() {
            headers.insert("x-api-key".to_string(), api_key);
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
}

impl DefaultAuthProvider {
    pub fn new(agent: String, office_id: String) -> Self {
        Self {
            config: AgentConfig { agent, office_id },
            api_key: None,
        }
    }

    pub fn with_api_key(mut self, api_key: String) -> Self {
        self.api_key = Some(api_key);
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

        // 测试头部生成 / Test header generation
        let headers = auth_with_key.get_connection_headers();
        assert_eq!(headers.get("x-api-key").unwrap(), "test-key");
    }
}
