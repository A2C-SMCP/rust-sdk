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
use rmcp::model::CallToolRequestParams;
use std::sync::Arc as StdArc;

/// Build rmcp tool-call parameters without changing the pre-rmcp-2 wire contract.
///
/// The public SDK accepts any JSON value for historical compatibility. Only JSON objects are
/// valid MCP `arguments`; other values were previously represented by an absent field and must
/// not be silently rewritten to an explicit empty object.
pub(crate) fn call_tool_request_params(
    tool_name: &str,
    params: serde_json::Value,
) -> CallToolRequestParams {
    let request = CallToolRequestParams::new(tool_name.to_string());
    match params.as_object().cloned() {
        Some(arguments) => request.with_arguments(arguments),
        None => request,
    }
}

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
///
/// `notify`（#106）：可选的运行期变化通知上报接缝，由 [`MCPServerManager`](super::manager::MCPServerManager)
/// 在启动客户端时按 server 名注入，透传给具体客户端构造函数，使 stdio/sse/http 三传输的服务器主动通知能
/// 上报给 Computer 消费者任务。为 `None` 时客户端不转发通知（行为与历史一致）。
pub fn client_factory(
    config: MCPServerConfig,
    notify: Option<ClientNotifyCtx>,
) -> StdArc<dyn MCPClientProtocol> {
    match config {
        MCPServerConfig::Stdio(config) => {
            StdArc::new(StdioMCPClient::new(config.server_parameters).with_notify(notify))
        }
        MCPServerConfig::Sse(config) => {
            StdArc::new(SseMCPClient::new(config.server_parameters).with_notify(notify))
        }
        MCPServerConfig::Http(config) => {
            StdArc::new(HttpMCPClient::new(config.server_parameters).with_notify(notify))
        }
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
        assert!(!server_declares_resources(&serde_json::json!(
            "not-an-object"
        )));
    }

    #[test]
    fn call_tool_arguments_preserve_legacy_absent_vs_object_contract() {
        let object = serde_json::to_value(call_tool_request_params(
            "tool",
            serde_json::json!({"a": 1}),
        ))
        .unwrap();
        assert_eq!(object["arguments"], serde_json::json!({"a": 1}));

        for non_object in [
            serde_json::Value::Null,
            serde_json::json!([]),
            serde_json::json!("value"),
            serde_json::json!(1),
        ] {
            let encoded =
                serde_json::to_value(call_tool_request_params("tool", non_object)).unwrap();
            assert!(
                encoded.get("arguments").is_none(),
                "non-object arguments must remain absent: {encoded}"
            );
        }
    }

    #[tokio::test]
    async fn test_client_factory_stdio() {
        let config = MCPServerConfig::Stdio(StdioServerConfig {
            env_file: None,
            name: "test_stdio".to_string(),
            bundle_id: None,
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

        let client = client_factory(config, None);
        assert_eq!(client.state(), ClientState::Initialized);
    }

    #[tokio::test]
    async fn test_client_factory_http() {
        let config = MCPServerConfig::Http(HttpServerConfig {
            env_file: None,
            name: "test_http".to_string(),
            bundle_id: None,
            disabled: false,
            forbidden_tools: vec![],
            tool_meta: HashMap::new(),
            default_tool_meta: None,
            vrl: None,
            oauth: None,
            auth_policy: None,
            server_parameters: HttpServerParameters {
                url: "http://localhost:8080".to_string(),
                headers: HashMap::new(),
            },
        });

        let client = client_factory(config, None);
        assert_eq!(client.state(), ClientState::Initialized);
    }

    #[tokio::test]
    async fn test_client_factory_sse() {
        let config = MCPServerConfig::Sse(SseServerConfig {
            env_file: None,
            name: "test_sse".to_string(),
            bundle_id: None,
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

        let client = client_factory(config, None);
        assert_eq!(client.state(), ClientState::Initialized);
    }
}
