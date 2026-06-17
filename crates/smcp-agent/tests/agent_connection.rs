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
async fn test_agent_auth_provider_auth_dict() {
    // #86：认证提供者把 api_key 放入 Socket.IO CONNECT auth dict（默认字段 `token`），不再用 HTTP header。
    // #86: the provider puts api_key into the Socket.IO CONNECT auth dict (default field `token`).

    let agent_id = "test-agent-headers";
    let office_id = "test-office-headers";
    let api_key = "test-api-key-123";

    let auth = DefaultAuthProvider::new(agent_id.to_string(), office_id.to_string())
        .with_api_key(api_key.to_string());

    // 验证连接面鉴权走 auth dict（header 仅路由、默认空）
    let auth_dict = auth.get_connection_auth().unwrap();
    assert_eq!(auth_dict, serde_json::json!({ "token": api_key }));
    assert!(auth.get_connection_headers().is_empty());

    // 验证Agent配置
    let config = auth.get_agent_config();
    assert_eq!(config.agent, agent_id.to_string());
    assert_eq!(config.office_id, office_id.to_string());
}

#[tokio::test]
async fn test_agent_auth_provider_custom_field() {
    // #86：自定义 auth dict 密钥字段名（with_auth_field_name）必须真实生效。
    // #86: the custom auth-dict field name (with_auth_field_name) must take effect.

    let agent_id = "test-agent-custom";
    let office_id = "test-office-custom";
    let api_key = "custom-api-key";

    let auth = DefaultAuthProvider::new(agent_id.to_string(), office_id.to_string())
        .with_api_key(api_key.to_string())
        .with_auth_field_name("x-legacy");

    let auth_dict = auth.get_connection_auth().unwrap();
    assert_eq!(auth_dict, serde_json::json!({ "x-legacy": api_key }));
    assert!(auth.get_connection_headers().is_empty());
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
