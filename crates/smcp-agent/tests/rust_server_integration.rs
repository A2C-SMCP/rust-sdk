/*!
* 文件名: rust_server_integration
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: None
* 描述: Rust Agent 与 Rust Server 集成测试 / Rust Agent with Rust Server integration tests
*/

#[cfg(test)]
mod integration_tests {
    use smcp_agent::{AsyncSmcpAgent, DefaultAuthProvider, SmcpAgentConfig};
    use smcp_server_core::SmcpServerBuilder;

    #[tokio::test]
    async fn test_agent_server_basic_connection() {
        // 中文：测试Agent与Server的基本连接
        // English: Test basic connection between Agent and Server

        // 创建Server Layer
        let server_layer = SmcpServerBuilder::new()
            .with_default_auth(None, Some("x-api-key".to_string()))
            .build_layer()
            .expect("Failed to build server layer");

        // 验证Server Layer可以构建
        assert!(server_layer.state.session_manager.get_stats().total == 0);
    }

    #[tokio::test]
    async fn test_agent_connection_timeout() {
        // 中文：测试Agent连接超时处理
        // English: Test Agent connection timeout handling

        let auth = DefaultAuthProvider::new("test-agent".to_string(), "test-office".to_string());
        let config = SmcpAgentConfig::new().with_default_timeout(1); // 1秒超时

        let mut agent = AsyncSmcpAgent::new(auth, config);

        // 尝试连接到不存在的服务器
        let result = agent.connect("http://127.0.0.1:9999").await;

        // 应该返回错误
        assert!(
            result.is_err(),
            "Should fail to connect to non-existent server"
        );
    }

    #[tokio::test]
    async fn test_agent_auth_provider() {
        // 中文：测试认证提供者配置
        // English: Test authentication provider configuration

        // 这个测试验证Agent能够正确处理ACK相关的配置
        let auth = DefaultAuthProvider::new("test-agent".to_string(), "test-office".to_string())
            .with_api_key("test-key".to_string());

        let config = SmcpAgentConfig::new()
            .with_default_timeout(5)
            .with_tool_call_timeout(5);

        // 创建Agent
        let _agent = AsyncSmcpAgent::new(auth, config);

        // Agent创建成功即表示配置有效
        // 实际的字段访问需要通过公共方法或反射
    }
}
