/*!
* 文件名: e2e_test_agent
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: None
* 描述: 端到端测试用的简单Agent / Simple Agent for end-to-end testing
*/

use std::env;
use std::time::Duration;
use tokio::time::timeout;
use tracing::{error, info};

use smcp_agent::{AsyncSmcpAgent, DefaultAuthProvider, SmcpAgentConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 初始化日志
    tracing_subscriber::fmt::init();

    // 从环境变量获取参数
    let server_url =
        env::var("SMCP_SERVER_URL").unwrap_or_else(|_| "http://127.0.0.1:8000".to_string());
    let agent_id = env::var("SMCP_AGENT_ID").unwrap_or_else(|_| "e2e-test-agent".to_string());
    let office_id = env::var("SMCP_OFFICE_ID").unwrap_or_else(|_| "e2e-test-office".to_string());
    let api_key = env::var("SMCP_API_KEY").ok();

    info!("Starting E2E Test Agent");
    info!("Server URL: {}", server_url);
    info!("Agent ID: {}", agent_id);
    info!("Office ID: {}", office_id);

    // 创建认证提供者
    let auth = DefaultAuthProvider::new(agent_id.clone(), office_id.clone());
    let auth = if let Some(key) = api_key {
        auth.with_api_key(key)
    } else {
        auth
    };

    // 创建Agent配置
    let config = SmcpAgentConfig::new()
        .with_default_timeout(10)
        .with_tool_call_timeout(10)
        .with_reconnect_interval(1000)
        .with_max_retries(3);

    // 创建Agent
    let mut agent = AsyncSmcpAgent::new(auth, config);

    // 连接到服务器
    info!("Connecting to server...");
    agent.connect(&server_url).await?;
    info!("Connected successfully");

    // 加入办公室
    info!("Joining office...");
    agent.join_office(&agent_id).await?;
    info!("Joined office successfully");

    // 等待一段时间以接收事件
    info!("Waiting for events...");
    tokio::time::sleep(Duration::from_secs(5)).await;

    // 如果指定了测试模式，执行一些测试操作
    if env::var("SMCP_TEST_MODE").unwrap_or_default() == "tool_call" {
        info!("Running tool call test...");

        // 尝试获取工具列表
        match timeout(Duration::from_secs(5), agent.get_tools("test-computer")).await {
            Ok(Ok(tools)) => {
                info!("Got {} tools", tools.len());
                for tool in tools {
                    info!("  - {}: {}", tool.name, tool.description);
                }
            }
            Ok(Err(e)) => error!("Failed to get tools: {}", e),
            Err(_) => error!("Timeout getting tools"),
        }
    }

    // 离开办公室
    info!("Leaving office...");
    agent.leave_office().await?;
    info!("Left office successfully");

    // 断开连接
    info!("Test completed");

    Ok(())
}
