/**
* 文件名: basic_agent
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: None
* 描述: 基本的SMCP Agent使用示例 / Basic SMCP Agent usage example
*/

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 初始化日志
    // tracing_subscriber::fmt::init(); // 需要添加 tracing_subscriber 依赖

    // 创建认证提供者
    let auth_provider =
        smcp_agent::DefaultAuthProvider::new("my-agent".to_string(), "test-office".to_string())
            .with_api_key("your-api-key-here".to_string());

    // 创建Agent配置
    let config = smcp_agent::SmcpAgentConfig::new()
        .with_default_timeout(30)
        .with_tool_call_timeout(120)
        .with_auto_fetch_desktop(true);

    // 创建Agent实例
    let mut agent = smcp_agent::AsyncSmcpAgent::new(auth_provider, config);

    // 连接到服务器
    agent.connect("http://localhost:3000").await?;

    // 加入办公室
    agent.join_office("My Rust Agent").await?;

    // 获取房间内的Computer列表
    let sessions = agent.list_room("test-office").await?;
    println!("Found {} sessions", sessions.len());

    // 如果有Computer，获取其工具列表
    if let Some(computer_session) = sessions
        .iter()
        .find(|s| matches!(s.role, smcp::Role::Computer))
    {
        let tools = agent.get_tools(&computer_session.name).await?;
        println!(
            "Computer {} has {} tools",
            computer_session.name,
            tools.len()
        );

        // 调用第一个工具
        if let Some(tool) = tools.first() {
            let result = agent
                .tool_call(&computer_session.name, &tool.name, serde_json::json!({}))
                .await?;
            println!(
                "Tool call result: {}",
                serde_json::to_string_pretty(&result)?
            );
        }
    }

    // 保持运行以接收通知
    tokio::signal::ctrl_c().await?;

    // 离开办公室
    agent.leave_office().await?;

    Ok(())
}
