/*!
* 文件名: tests
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: None
* 描述: SMCP Agent测试 / SMCP Agent tests
*/

#[cfg(test)]
mod unit_tests {
    use crate::{
        auth::{AuthProvider, DefaultAuthProvider},
        config::SmcpAgentConfig,
        error::SmcpAgentError,
        AsyncSmcpAgent,
    };
    use smcp::ReqId;

    #[test]
    fn test_req_id_format() {
        // 测试ReqId格式是否与Python一致（无连字符的hex）
        let req_id = ReqId::new();
        let id_str = req_id.as_str();

        // 应该是32个字符的hex字符串（无连字符）
        assert_eq!(id_str.len(), 32);
        assert!(!id_str.contains('-'));

        // 验证是有效的hex
        assert!(id_str.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_auth_provider() {
        let auth = DefaultAuthProvider::new("test-agent".to_string(), "test-office".to_string());

        let config = auth.get_agent_config();
        assert_eq!(config.agent, "test-agent");
        assert_eq!(config.office_id, "test-office");

        // 测试带API key的版本
        let auth_with_key = auth.with_api_key("test-key".to_string());
        assert_eq!(auth_with_key.get_api_key().unwrap(), "test-key");

        // 测试头部生成
        let headers = auth_with_key.get_connection_headers();
        assert_eq!(headers.get("x-api-key").unwrap(), "test-key");
    }

    #[test]
    fn test_agent_config() {
        let config = SmcpAgentConfig::new()
            .with_default_timeout(10)
            .with_tool_call_timeout(30)
            .with_auto_fetch_desktop(false)
            .with_max_retries(5)
            .with_reconnect_interval(2000);

        assert_eq!(config.default_timeout, 10);
        assert_eq!(config.tool_call_timeout, 30);
        assert!(!config.auto_fetch_desktop);
        assert_eq!(config.max_retries, 5);
        assert_eq!(config.reconnect_interval, 2000);
    }

    #[test]
    fn test_error_types() {
        let err = SmcpAgentError::invalid_event("notify:test");
        assert!(matches!(err, SmcpAgentError::InvalidEvent { .. }));

        let err = SmcpAgentError::authentication("Invalid token");
        assert!(matches!(err, SmcpAgentError::Authentication(_)));

        let err = SmcpAgentError::ReqIdMismatch {
            expected: "abc".to_string(),
            actual: "def".to_string(),
        };
        assert!(matches!(err, SmcpAgentError::ReqIdMismatch { .. }));
    }

    #[tokio::test]
    async fn test_async_agent_creation() {
        let auth = DefaultAuthProvider::new("test-agent".to_string(), "test-office".to_string());

        let config = SmcpAgentConfig::new();
        let _agent = AsyncSmcpAgent::new(auth, config);

        // 验证Agent创建成功
        // 注意：这里不能测试连接，因为没有实际的服务器
        // Agent创建成功即可，私有字段测试需要通过公共接口
        // Agent成功创建
    }
}
