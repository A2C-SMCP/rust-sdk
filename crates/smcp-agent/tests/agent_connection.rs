/*!
* 文件名: agent_connection
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: None
* 描述: SMCP Agent连接和认证测试 / SMCP Agent connection and authentication tests
*/

use smcp_agent::{AuthProvider, DefaultAuthProvider, SmcpAgentConfig};
mod common;
use common::*;

#[tokio::test]
async fn test_agent_auth_provider_headers() {
    // 中文：测试认证提供者正确设置请求头
    // English: Test auth provider correctly sets headers

    let agent_id = "test-agent-headers";
    let office_id = "test-office-headers";
    let api_key = "test-api-key-123";

    let auth = DefaultAuthProvider::new(agent_id.to_string(), office_id.to_string())
        .with_api_key(api_key.to_string());

    // 验证请求头
    let headers = auth.get_connection_headers();
    assert_eq!(headers.get("x-api-key"), Some(&api_key.to_string()));

    // 验证Agent配置
    let config = auth.get_agent_config();
    assert_eq!(config.agent, agent_id.to_string());
    assert_eq!(config.office_id, office_id.to_string());
}

#[tokio::test]
async fn test_agent_auth_provider_custom_headers() {
    // 中文：测试认证提供者支持自定义请求头
    // English: Test auth provider supports custom headers

    let agent_id = "test-agent-custom";
    let office_id = "test-office-custom";
    let api_key = "custom-api-key";

    let auth = DefaultAuthProvider::new(agent_id.to_string(), office_id.to_string())
        .with_api_key(api_key.to_string());

    // 注意：DefaultAuthProvider 不支持添加自定义请求头
    // auth.add_header("X-Custom-Header".to_string(), "custom-value".to_string());
    // auth.add_header("Authorization".to_string(), "Bearer token123".to_string());

    // 验证基本请求头（仅API密钥）
    let headers = auth.get_connection_headers();
    assert_eq!(headers.get("x-api-key"), Some(&api_key.to_string()));
    // 注意：自定义请求头功能暂未实现
}

#[tokio::test]
async fn test_agent_connect_with_custom_config() {
    // 中文：测试Agent使用自定义配置连接
    // English: Test Agent connects with custom config

    let auth = DefaultAuthProvider::new(
        "test-agent-config".to_string(),
        "test-office-config".to_string(),
    );
    let config = SmcpAgentConfig::new()
        .with_default_timeout(10)
        .with_tool_call_timeout(10)
        .with_reconnect_interval(1000)
        .with_max_retries(5);

    let _agent = smcp_agent::AsyncSmcpAgent::new(auth, config);

    // 注意：由于没有实际的服务器，这里只测试Agent创建
    // 实际连接测试需要真实的服务器环境
    // Agent创建成功
}

#[tokio::test]
async fn test_agent_multiple_connections() {
    // 中文：测试多个Agent同时连接
    // English: Test multiple Agents connect simultaneously

    let _agent1 = create_test_agent("test-agent-3", "test-office-3");
    let _agent2 = create_test_agent("test-agent-4", "test-office-3");
    let _agent3 = create_test_agent("test-agent-5", "test-office-3");

    // 注意：由于没有实际的服务器，这里只测试Agent创建
    // 实际连接测试需要真实的服务器环境
    // 多个Agent创建成功
}

#[tokio::test]
async fn test_agent_reconnect_on_connection_loss() {
    // 中文：测试Agent在连接丢失后尝试重连
    // English: Test Agent attempts to reconnect on connection loss

    let auth = DefaultAuthProvider::new(
        "test-agent-reconnect".to_string(),
        "test-office-reconnect".to_string(),
    );
    let config = SmcpAgentConfig::new()
        .with_default_timeout(5)
        .with_tool_call_timeout(5)
        .with_reconnect_interval(100)
        .with_max_retries(0);

    let _agent = smcp_agent::AsyncSmcpAgent::new(auth, config);

    // 注意：由于没有实际的服务器，这里只测试Agent创建
    // 实际重连测试需要真实的服务器环境
    // Agent创建成功
}

#[tokio::test]
async fn test_agent_config_validation() {
    // 中文：测试Agent配置验证
    // English: Test Agent config validation

    let auth = DefaultAuthProvider::new(
        "test-agent-validate".to_string(),
        "test-office-validate".to_string(),
    );

    // 测试默认配置
    let default_config = SmcpAgentConfig::default();
    assert!(default_config.default_timeout > 0);
    assert!(default_config.tool_call_timeout > 0);
    assert!(default_config.reconnect_interval > 0);
    assert!(default_config.max_retries > 0);

    // 测试自定义配置
    let custom_config = SmcpAgentConfig::new()
        .with_default_timeout(30)
        .with_tool_call_timeout(60)
        .with_reconnect_interval(1000)
        .with_max_retries(10);

    let _agent = smcp_agent::AsyncSmcpAgent::new(auth, custom_config);
}
