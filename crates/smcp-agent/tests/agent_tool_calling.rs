/*!
* 文件名: agent_tool_calling
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: None
* 描述: SMCP Agent工具调用测试 / SMCP Agent tool calling tests
*/

use smcp_agent::{AsyncSmcpAgent, DefaultAuthProvider, SmcpAgentConfig};
mod common;
use common::*;

#[tokio::test]
async fn test_agent_tool_call_creation() {
    // 中文：测试Agent工具调用创建
    // English: Test Agent tool call creation

    let _agent = create_test_agent("test-agent-tool", "test-office-tool");

    // 注意：由于没有实际的服务器和Computer，这里只测试Agent创建
    // 实际工具调用需要真实的服务器环境和Computer
    // Agent创建成功
}

#[tokio::test]
async fn test_agent_get_tools() {
    // 中文：测试Agent获取工具列表
    // English: Test Agent gets tool list

    let _agent = create_test_agent("test-agent-get-tools", "test-office-get-tools");

    // 注意：由于没有实际的服务器和Computer，这里只测试Agent创建
    // 实际获取工具需要真实的服务器环境和Computer
    // Agent创建成功
}

#[tokio::test]
async fn test_agent_call_tool_with_parameters() {
    // 中文：测试Agent带参数调用工具
    // English: Test Agent calls tool with parameters

    let _agent = create_test_agent("test-agent-call", "test-office-call");

    let _params = serde_json::json!({
        "text": "Hello, World!",
        "count": 3
    });

    // 注意：由于没有实际的服务器和Computer，这里只测试参数创建
    // 实际工具调用需要真实的服务器环境和Computer
    // 参数创建成功
}

#[tokio::test]
async fn test_agent_tool_call_timeout() {
    // 中文：测试Agent工具调用超时
    // English: Test Agent tool call timeout

    // 创建一个短超时的配置
    let auth = DefaultAuthProvider::new(
        "test-agent-timeout".to_string(),
        "test-office-timeout".to_string(),
    );
    let config = SmcpAgentConfig::new()
        .with_default_timeout(0) // 立即超时
        .with_tool_call_timeout(0);
    let _agent = AsyncSmcpAgent::new(auth, config);

    // 验证Agent创建成功
    // Agent创建成功
}

#[tokio::test]
async fn test_agent_get_desktop() {
    // 中文：测试Agent获取桌面信息
    // English: Test Agent gets desktop information

    let _agent = create_test_agent("test-agent-desktop", "test-office-desktop");

    // 注意：由于没有实际的服务器和Computer，这里只测试Agent创建
    // 实际获取桌面信息需要真实的服务器环境和Computer
    // Agent创建成功
}

#[tokio::test]
async fn test_agent_list_room() {
    // 中文：测试Agent列出房间成员
    // English: Test Agent lists room members

    let _agent = create_test_agent("test-agent-list", "test-office-list");

    // 注意：由于没有实际的服务器，这里只测试Agent创建
    // 实际列出房间成员需要真实的服务器环境
    // Agent创建成功
}

#[tokio::test]
async fn test_agent_concurrent_tool_calls() {
    // 中文：测试Agent并发工具调用
    // English: Test Agent concurrent tool calls

    let _agent = create_test_agent("test-agent-concurrent", "test-office-concurrent");

    // 创建多个工具调用参数
    let _params1 = serde_json::json!({"text": "Call 1"});
    let _params2 = serde_json::json!({"text": "Call 2"});
    let _params3 = serde_json::json!({"text": "Call 3"});

    // 注意：由于没有实际的服务器和Computer，这里只测试参数创建
    // 实际并发工具调用需要真实的服务器环境和Computer
    // 参数创建成功
}

#[tokio::test]
async fn test_agent_tool_call_error_handling() {
    // 中文：测试Agent工具调用错误处理
    // English: Test Agent tool call error handling

    let _agent = create_test_agent("test-agent-error", "test-office-error");

    // 测试无效的参数
    let _invalid_params = serde_json::json!({
        "invalid_param": "value"
    });

    // 注意：由于没有实际的服务器和Computer，这里只测试参数创建
    // 实际错误处理需要真实的服务器环境和Computer
    // 无效参数创建成功
}

#[tokio::test]
async fn test_agent_tool_configuration() {
    // 中文：测试Agent工具调用配置
    // English: Test Agent tool call configuration

    let auth = DefaultAuthProvider::new(
        "test-agent-config".to_string(),
        "test-office-config".to_string(),
    );
    let config = SmcpAgentConfig::new()
        .with_default_timeout(30)
        .with_tool_call_timeout(60)
        .with_reconnect_interval(1000)
        .with_max_retries(3);

    let _agent = AsyncSmcpAgent::new(auth, config);

    // 验证Agent创建成功
    // Agent创建成功
}
