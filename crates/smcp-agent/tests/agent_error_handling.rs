/*!
* 文件名: agent_error_handling
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: None
* 描述: SMCP Agent错误处理和重连测试 / SMCP Agent error handling and reconnection tests
*/

use smcp_agent::{AsyncSmcpAgent, DefaultAuthProvider, SmcpAgentConfig, SmcpAgentError};
mod common;
use common::*;

#[tokio::test]
async fn test_agent_handles_invalid_url() {
    // 中文：测试Agent处理无效URL
    // English: Test Agent handles invalid URL

    let mut agent = create_test_agent("test-agent-invalid", "test-office-invalid");

    // 尝试连接到无效URL
    let result = agent.connect("invalid-url").await;
    assert!(result.is_err(), "Should fail to connect to invalid URL");

    match result.err().unwrap() {
        SmcpAgentError::Connection(_) => {} // 期望的错误类型
        other => panic!("Expected Connection error, got: {:?}", other),
    }
}

#[tokio::test]
async fn test_agent_handles_connection_timeout() {
    // 中文：测试Agent处理连接超时
    // English: Test Agent handles connection timeout

    // 创建一个短超时的配置
    let auth = DefaultAuthProvider::new(
        "test-agent-timeout".to_string(),
        "test-office-timeout".to_string(),
    );
    let config = SmcpAgentConfig::new()
        .with_default_timeout(0) // 立即超时
        .with_tool_call_timeout(0)
        .with_reconnect_interval(100)
        .with_max_retries(0);
    let mut agent = AsyncSmcpAgent::new(auth, config);

    // 尝试连接到不存在的地址
    let result = agent.connect("ws://127.0.0.1:9999").await;

    // 应该连接失败
    assert!(
        result.is_err(),
        "Should fail to connect to non-existent address"
    );
}

#[tokio::test]
async fn test_agent_handles_disconnected_operations() {
    // 中文：测试Agent在未连接状态下的操作
    // English: Test Agent handles operations while disconnected

    let agent = create_test_agent("test-agent-disconnected", "test-office-disconnected");

    // 在未连接状态下尝试操作
    let result = agent.join_office("TestOffice").await;
    assert!(
        result.is_err(),
        "Should fail to join office when disconnected"
    );

    let result = agent.get_tools("test-computer").await;
    assert!(
        result.is_err(),
        "Should fail to get tools when disconnected"
    );

    let result = agent
        .tool_call("test-computer", "echo", serde_json::json!({"text": "test"}))
        .await;
    assert!(
        result.is_err(),
        "Should fail to call tool when disconnected"
    );
}

#[tokio::test]
async fn test_agent_error_recovery() {
    // 中文：测试Agent错误恢复
    // English: Test Agent error recovery

    let mut agent = create_test_agent("test-agent-recovery", "test-office-recovery");

    // 第一次连接失败
    let result1 = agent.connect("ws://127.0.0.1:9999").await;
    assert!(result1.is_err(), "First connection should fail");

    // Agent应该仍然可用
    let result2 = agent.join_office("TestOffice").await;
    assert!(result2.is_err(), "Should still fail to join office");

    // 验证Agent状态
    // Agent应该仍然可用
}

#[tokio::test]
async fn test_agent_concurrent_error_handling() {
    // 中文：测试Agent并发错误处理
    // English: Test Agent concurrent error handling

    let agent = create_test_agent("test-agent-concurrent", "test-office-concurrent");

    // 并发执行多个失败的操作
    let join1 = agent.join_office("Office1");
    let join2 = agent.join_office("Office2");
    let join3 = agent.join_office("Office3");

    let (result1, result2, result3): (
        Result<(), smcp_agent::SmcpAgentError>,
        Result<(), smcp_agent::SmcpAgentError>,
        Result<(), smcp_agent::SmcpAgentError>,
    ) = tokio::join!(join1, join2, join3);

    // 所有操作都应该失败
    assert!(result1.is_err());
    assert!(result2.is_err());
    assert!(result3.is_err());

    // Agent应该仍然可用
    // Agent应该仍然可用
}
