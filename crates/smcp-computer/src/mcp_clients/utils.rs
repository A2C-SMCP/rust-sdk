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

/// 判定 MCP Server 的 initialize `result` 是否声明 `resources` 能力（v0.2 `get_resources` 4015 预检）。
/// Whether a server's initialize `result` declares the `resources` capability (for the 4015 pre-check).
///
/// 对齐 stdio 的 `peer_info().capabilities.resources.is_some()` 与 Python base_client 的统一预检：
/// `result.capabilities.resources` **存在**（含空对象 `{}`）即视为支持；缺 `capabilities`、缺
/// `resources`、或 `result` 非对象 → 不支持（默认拒绝）。sse/http `initialize_session` 据此缓存布尔，
/// 使 `list_resources_page` 的 4015 语义与 stdio 一致（INT-04 #78）。
/// Mirrors stdio's `capabilities.resources.is_some()` and Python's shared pre-check: presence of
/// `result.capabilities.resources` (even an empty `{}`) ⇒ supported; otherwise default-deny.
pub(crate) fn server_declares_resources(init_result: &serde_json::Value) -> bool {
    init_result
        .get("capabilities")
        .and_then(|c| c.get("resources"))
        .is_some()
}

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

    #[test]
    fn test_server_declares_resources_present_empty_object() {
        // capabilities.resources = {} （MCP 常见声明形式）→ 支持。
        let result = serde_json::json!({ "capabilities": { "resources": {} } });
        assert!(server_declares_resources(&result));
    }

    #[test]
    fn test_server_declares_resources_present_with_subkeys() {
        let result = serde_json::json!({
            "capabilities": { "resources": { "subscribe": true, "listChanged": false } }
        });
        assert!(server_declares_resources(&result));
    }

    #[test]
    fn test_server_declares_resources_missing_resources_key() {
        // 声明了 capabilities 但无 resources → 默认拒绝（4015）。
        let result = serde_json::json!({ "capabilities": { "tools": {} } });
        assert!(!server_declares_resources(&result));
    }

    #[test]
    fn test_server_declares_resources_missing_capabilities() {
        let result = serde_json::json!({ "serverInfo": { "name": "x" } });
        assert!(!server_declares_resources(&result));
    }

    #[test]
    fn test_server_declares_resources_non_object_result() {
        assert!(!server_declares_resources(&serde_json::Value::Null));
        assert!(!server_declares_resources(&serde_json::json!("not-an-object")));
    }

    #[tokio::test]
    async fn test_client_factory_stdio() {
        let config = MCPServerConfig::Stdio(StdioServerConfig {
            env_file: None,
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
            env_file: None,
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
            env_file: None,
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
