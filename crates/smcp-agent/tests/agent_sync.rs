/*!
* 文件名: agent_sync
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: None
* 描述: SMCP SyncAgent集成测试 / SMCP SyncAgent integration tests
*/

use smcp_agent::{DefaultAuthProvider, SmcpAgentConfig, SyncSmcpAgent};
mod common;
use common::*;

#[test]
fn test_sync_agent_creation() {
    // 中文：测试同步Agent创建
    // English: Test synchronous Agent creation

    let auth = DefaultAuthProvider::new(
        "test-sync-agent".to_string(),
        "test-sync-office".to_string(),
    );
    let config = SmcpAgentConfig::new();
    let _agent = SyncSmcpAgent::new(auth, config).expect("Failed to create sync agent");

    // 验证Agent创建成功
    // 同步Agent创建成功
}

#[test]
fn test_sync_agent_with_custom_config() {
    // 中文：测试同步Agent使用自定义配置
    // English: Test synchronous Agent with custom config

    let auth = DefaultAuthProvider::new(
        "test-sync-agent-config".to_string(),
        "test-sync-office-config".to_string(),
    );
    let config = SmcpAgentConfig::new()
        .with_default_timeout(10)
        .with_tool_call_timeout(10)
        .with_reconnect_interval(200)
        .with_max_retries(5);

    let _agent = SyncSmcpAgent::new(auth, config).expect("Failed to create sync agent");

    // 验证Agent创建成功
    // 同步Agent创建成功
}

#[test]
fn test_sync_agent_multiple_instances() {
    // 中文：测试多个同步Agent实例
    // English: Test multiple synchronous Agent instances

    let _agent1 = create_sync_agent("test-sync-1", "test-sync-office-1");
    let _agent2 = create_sync_agent("test-sync-2", "test-sync-office-2");
    let _agent3 = create_sync_agent("test-sync-3", "test-sync-office-3");

    // 验证多个Agent创建成功
    // 多个同步Agent创建成功
}

#[test]
fn test_sync_agent_special_characters() {
    // 中文：测试同步Agent支持特殊字符
    // English: Test synchronous Agent with special characters

    let _agent1 = create_sync_agent("test-sync-中文", "test-sync-office-中文");
    let _agent2 = create_sync_agent("test-sync-😀", "test-sync-office-😀");

    // 验证支持特殊字符的Agent创建成功
    // 同步Agent创建成功
}

#[test]
fn test_sync_agent_long_names() {
    // 中文：测试长名称的同步Agent
    // English: Test synchronous Agent with long names

    let long_agent_id = "a".repeat(100);
    let long_office_id = "b".repeat(100);

    let _agent = create_sync_agent(&long_agent_id, &long_office_id);

    // 验证长名称的Agent创建成功
    // 同步Agent创建成功
}

#[test]
fn test_sync_agent_error_handling() {
    // 中文：测试同步Agent错误处理
    // English: Test synchronous Agent error handling

    // 测试无效配置
    let auth = DefaultAuthProvider::new("".to_string(), "".to_string()); // 空字符串
    let config = SmcpAgentConfig::new();

    // Agent应该仍然能够创建（验证在连接时才进行验证）
    let agent = SyncSmcpAgent::new(auth, config);
    assert!(agent.is_ok(), "Sync agent created with empty IDs");
}

#[test]
fn test_sync_agent_config_validation() {
    // 中文：测试同步Agent配置验证
    // English: Test synchronous Agent configuration validation

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

    let auth = DefaultAuthProvider::new(
        "test-sync-validate".to_string(),
        "test-sync-office-validate".to_string(),
    );
    let _agent = SyncSmcpAgent::new(auth, custom_config).expect("Failed to create sync agent");
}

/// 创建测试用的同步Agent实例
pub fn create_sync_agent(agent_id: &str, office_id: &str) -> SyncSmcpAgent {
    let auth = DefaultAuthProvider::new(agent_id.to_string(), office_id.to_string());
    let config = create_test_agent_config();
    SyncSmcpAgent::new(auth, config).expect("Failed to create sync agent")
}
