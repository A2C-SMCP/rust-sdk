#![allow(unexpected_cfgs)]

/*!
* 文件名: factory.rs
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: async-trait
* 描述: MCP客户端工厂 / MCP client factory
*/

use crate::errors::McpClientError;
use crate::mcp_clients::base::McpClient;
use crate::mcp_clients::model::MCPServerConfig;
use std::boxed::Box;

/// 创建MCP客户端 / Create MCP client
pub fn client_factory(config: MCPServerConfig) -> Result<Box<dyn McpClient>, McpClientError> {
    match config {
        MCPServerConfig::Stdio(config) => {
            let client = super::stdio::StdioMcpClient::new(config.server_parameters);
            Ok(Box::new(client))
        }
        #[cfg(feature = "sse")]
        MCPServerConfig::Sse(config) => {
            let client = super::sse::SseMcpClient::new(config.server_parameters);
            Ok(Box::new(client))
        }
        #[cfg(feature = "http")]
        MCPServerConfig::StreamableHttp(config) => {
            let client = super::streamable_http::StreamableHttpMcpClient::new(config.server_parameters);
            Ok(Box::new(client))
        }
        #[cfg(not(feature = "sse"))]
        MCPServerConfig::Sse(_) => {
            Err(McpClientError::InvalidState("SSE support not enabled".to_string()))
        }
        #[cfg(not(feature = "http"))]
        MCPServerConfig::StreamableHttp(_) => {
            Err(McpClientError::InvalidState("HTTP support not enabled".to_string()))
        }
    }
}
