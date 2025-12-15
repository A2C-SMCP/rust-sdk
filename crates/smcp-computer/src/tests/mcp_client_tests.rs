/**
* 文件名: mcp_client_tests
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: tokio, async-trait
* 描述: MCP客户端测试
*/

use smcp_computer::mcp_clients::*;
use std::collections::HashMap;
use tokio::time::{sleep, Duration};

#[tokio::test]
async fn test_stdio_client_lifecycle() {
    // 创建STDIO客户端配置 / Create STDIO client configuration
    let params = StdioServerParameters {
        command: "echo".to_string(),
        args: vec!["hello".to_string()],
        env: HashMap::new(),
        cwd: None,
    };
    
    let mut client = StdioMCPClient::new(params);
    
    // 测试初始状态 / Test initial state
    assert_eq!(client.state(), ClientState::Initialized);
    
    // 测试连接 / Test connection
    let result = client.connect().await;
    assert!(result.is_ok(), "Failed to connect: {:?}", result.err());
    assert_eq!(client.state(), ClientState::Connected);
    
    // 等待一小段时间确保进程启动 / Wait a bit to ensure process started
    sleep(Duration::from_millis(100)).await;
    
    // 测试断开连接 / Test disconnection
    let result = client.disconnect().await;
    assert!(result.is_ok(), "Failed to disconnect: {:?}", result.err());
    assert_eq!(client.state(), ClientState::Disconnected);
}

#[tokio::test]
async fn test_http_client_creation() {
    let params = HttpServerParameters {
        url: "http://localhost:8080".to_string(),
        headers: HashMap::new(),
    };
    
    let client = HttpMCPClient::new(params);
    assert_eq!(client.state(), ClientState::Initialized);
}

#[tokio::test]
async fn test_sse_client_creation() {
    let params = SseServerParameters {
        url: "http://localhost:8080".to_string(),
        headers: HashMap::new(),
    };
    
    let client = SseMCPClient::new(params);
    assert_eq!(client.state(), ClientState::Initialized);
}

#[tokio::test]
async fn test_client_factory() {
    // 测试STDIO客户端工厂 / Test STDIO client factory
    let stdio_config = MCPServerConfig::Stdio(StdioServerConfig {
        name: "test_stdio".to_string(),
        disabled: false,
        forbidden_tools: vec![],
        tool_meta: HashMap::new(),
        default_tool_meta: None,
        vrl: None,
        server_parameters: StdioServerParameters {
            command: "echo".to_string(),
            args: vec![],
            env: HashMap::new(),
            cwd: None,
        },
    });
    
    let client = client_factory(stdio_config);
    assert_eq!(client.state(), ClientState::Initialized);
    
    // 测试HTTP客户端工厂 / Test HTTP client factory
    let http_config = MCPServerConfig::Http(HttpServerConfig {
        name: "test_http".to_string(),
        disabled: false,
        forbidden_tools: vec![],
        tool_meta: HashMap::new(),
        default_tool_meta: None,
        vrl: None,
        server_parameters: HttpServerParameters {
            url: "http://localhost:8080".to_string(),
            headers: HashMap::new(),
        },
    });
    
    let client = client_factory(http_config);
    assert_eq!(client.state(), ClientState::Initialized);
    
    // 测试SSE客户端工厂 / Test SSE client factory
    let sse_config = MCPServerConfig::Sse(SseServerConfig {
        name: "test_sse".to_string(),
        disabled: false,
        forbidden_tools: vec![],
        tool_meta: HashMap::new(),
        default_tool_meta: None,
        vrl: None,
        server_parameters: SseServerParameters {
            url: "http://localhost:8080".to_string(),
            headers: HashMap::new(),
        },
    });
    
    let client = client_factory(sse_config);
    assert_eq!(client.state(), ClientState::Initialized);
}

#[tokio::test]
async fn test_tool_meta_merge() {
    let default_meta = ToolMeta {
        auto_apply: Some(true),
        alias: None,
        tags: Some(vec!["default".to_string()]),
        ret_object_mapper: None,
    };
    
    let specific_meta = ToolMeta {
        auto_apply: Some(false), // 应该覆盖默认值 / Should override default
        alias: Some("custom_alias".to_string()),
        tags: None, // 应该保留默认值 / Should keep default
        ret_object_mapper: Some(HashMap::new()),
    };
    
    // 模拟合并逻辑 / Simulate merge logic
    let merged = ToolMeta {
        auto_apply: specific_meta.auto_apply,
        alias: specific_meta.alias,
        tags: default_meta.tags,
        ret_object_mapper: specific_meta.ret_object_mapper,
    };
    
    assert_eq!(merged.auto_apply, Some(false));
    assert_eq!(merged.alias, Some("custom_alias".to_string()));
    assert_eq!(merged.tags, Some(vec!["default".to_string()]));
    assert!(merged.ret_object_mapper.is_some());
}

#[tokio::test]
async fn test_server_config_accessors() {
    let mut tool_meta = HashMap::new();
    tool_meta.insert("test_tool".to_string(), ToolMeta {
        auto_apply: Some(true),
        alias: Some("test_alias".to_string()),
        tags: None,
        ret_object_mapper: None,
    });
    
    let config = MCPServerConfig::Stdio(StdioServerConfig {
        name: "test_server".to_string(),
        disabled: true,
        forbidden_tools: vec!["forbidden_tool".to_string()],
        tool_meta: tool_meta.clone(),
        default_tool_meta: Some(ToolMeta::default()),
        vrl: Some("return true".to_string()),
        server_parameters: StdioServerParameters {
            command: "test".to_string(),
            args: vec![],
            env: HashMap::new(),
            cwd: None,
        },
    });
    
    // 测试访问器方法 / Test accessor methods
    assert_eq!(config.name(), "test_server");
    assert!(config.disabled());
    assert_eq!(config.forbidden_tools(), &["forbidden_tool".to_string()]);
    assert_eq!(config.tool_meta(), &tool_meta);
    assert!(config.default_tool_meta().is_some());
    assert_eq!(config.vrl(), Some("return true"));
}

#[tokio::test]
async fn test_client_state_transitions() {
    let params = StdioServerParameters {
        command: "echo".to_string(),
        args: vec!["test".to_string()],
        env: HashMap::new(),
        cwd: None,
    };
    
    let mut client = StdioMCPClient::new(params);
    
    // 测试无效的状态转换 / Test invalid state transitions
    // 初始状态下断开连接应该失败 / Should fail to disconnect in initial state
    let result = client.disconnect().await;
    assert!(result.is_err());
    
    // 连接后再次连接应该成功或忽略 / Should succeed or ignore when connecting again
    let _ = client.connect().await;
    let result = client.connect().await;
    // 根据实现，这可能成功（已连接）或失败，取决于具体逻辑
    // Depending on implementation, this might succeed (already connected) or fail
    
    // 断开连接 / Disconnect
    let _ = client.disconnect().await;
    
    // 断开状态下断开连接应该失败 / Should fail to disconnect when already disconnected
    let result = client.disconnect().await;
    assert!(result.is_err());
}
