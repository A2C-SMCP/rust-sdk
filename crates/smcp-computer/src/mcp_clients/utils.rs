use super::http_client::HttpMCPClient;
/**
* 文件名: utils
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: async-trait
* 描述: MCP客户端工具函数
*/
use super::model::*;
use super::sse_client::SseMCPClient;
use super::stdio_client::StdioMCPClient;
use std::sync::Arc as StdArc;

/// 根据配置创建客户端 / Create client based on configuration
pub fn client_factory(config: MCPServerConfig) -> StdArc<dyn MCPClientProtocol> {
    match config {
        MCPServerConfig::Stdio(config) => {
            StdArc::new(StdioMCPClient::new(config.server_parameters))
        }
        MCPServerConfig::Sse(config) => StdArc::new(SseMCPClient::new(config.server_parameters)),
        MCPServerConfig::Http(config) => StdArc::new(HttpMCPClient::new(config.server_parameters)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[tokio::test]
    async fn test_client_factory_stdio() {
        let config = MCPServerConfig::Stdio(StdioServerConfig {
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

        let client = client_factory(config);
        assert_eq!(client.state(), ClientState::Initialized);
    }

    #[tokio::test]
    async fn test_client_factory_http() {
        let config = MCPServerConfig::Http(HttpServerConfig {
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

        let client = client_factory(config);
        assert_eq!(client.state(), ClientState::Initialized);
    }

    #[tokio::test]
    async fn test_client_factory_sse() {
        let config = MCPServerConfig::Sse(SseServerConfig {
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

        let client = client_factory(config);
        assert_eq!(client.state(), ClientState::Initialized);
    }
}
