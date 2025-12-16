/**
* 文件名: handler
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: tokio, async-trait
* 描述: 输入处理器，负责协调各种输入提供者
*/
use super::model::*;
use super::providers::{
    CliInputProvider, CompositeInputProvider, EnvironmentInputProvider, InputProvider,
};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, error, info};

/// 输入处理器 / Input handler
pub struct InputHandler {
    /// 输入提供者 / Input provider
    provider: Arc<dyn InputProvider>,
    /// 缓存的输入值 / Cached input values
    cache: Arc<RwLock<HashMap<String, InputValue>>>,
    /// 是否启用缓存 / Whether to enable cache
    enable_cache: bool,
}

impl InputHandler {
    /// 创建新的输入处理器 / Create new input handler
    pub fn new() -> Self {
        // 默认使用组合提供者：先尝试环境变量，再尝试CLI
        // Use composite provider by default: try environment variable first, then CLI
        let provider: Box<dyn InputProvider> = Box::new(
            CompositeInputProvider::new()
                .add_provider(Box::new(EnvironmentInputProvider::new()))
                .add_provider(Box::new(CliInputProvider::new())),
        );

        Self {
            provider: Arc::from(provider),
            cache: Arc::new(RwLock::new(HashMap::new())),
            enable_cache: true,
        }
    }

    /// 使用自定义提供者创建输入处理器 / Create input handler with custom provider
    pub fn with_provider<P>(provider: P) -> Self
    where
        P: InputProvider + 'static,
    {
        Self {
            provider: Arc::new(provider),
            cache: Arc::new(RwLock::new(HashMap::new())),
            enable_cache: true,
        }
    }

    /// 设置是否启用缓存 / Set whether to enable cache
    pub fn with_cache(mut self, enable: bool) -> Self {
        self.enable_cache = enable;
        self
    }

    /// 获取单个输入 / Get single input
    pub async fn get_input(
        &self,
        request: InputRequest,
        context: InputContext,
    ) -> InputResult<InputResponse> {
        debug!("Getting input for: {} (context: {:?})", request.id, context);

        // 检查缓存 / Check cache
        if self.enable_cache {
            let cache_key = self.build_cache_key(&request.id, &context);
            if let Some(value) = self.get_cached_value(&cache_key).await {
                debug!("Using cached value for: {}", request.id);
                return Ok(InputResponse {
                    id: request.id,
                    value,
                    cancelled: false,
                });
            }
        }

        // 从提供者获取输入 / Get input from provider
        let mut response = self.provider.get_input(&request, &context).await;

        // 如果获取失败且有默认值，返回默认值
        // If failed and has default value, return default value
        if response.is_err() && request.default.is_some() && !request.required {
            info!("Using default value for: {}", request.id);
            response = Ok(InputResponse {
                id: request.id.clone(),
                value: request.default.unwrap().clone(),
                cancelled: false,
            });
        }

        // 缓存结果 / Cache result
        if self.enable_cache {
            if let Ok(ref resp) = response {
                if !resp.cancelled {
                    let cache_key = self.build_cache_key(&request.id, &context);
                    self.cache_value(cache_key, resp.value.clone()).await;
                }
            }
        }

        response
    }

    /// 批量获取输入 / Get multiple inputs
    pub async fn get_inputs(
        &self,
        requests: Vec<InputRequest>,
        context: InputContext,
    ) -> InputResult<Vec<InputResponse>> {
        let mut responses = Vec::new();

        for request in requests {
            match self.get_input(request, context.clone()).await {
                Ok(response) => responses.push(response),
                Err(e) => {
                    error!("Failed to get input: {}", e);
                    return Err(e);
                }
            }
        }

        Ok(responses)
    }

    /// 清除缓存 / Clear cache
    pub async fn clear_cache(&self) {
        self.cache.write().await.clear();
        debug!("Input cache cleared");
    }

    /// 清除特定缓存 / Clear specific cache
    pub async fn clear_cache_for(&self, id: &str, context: &InputContext) {
        let cache_key = self.build_cache_key(id, context);
        let mut cache = self.cache.write().await;
        cache.remove(&cache_key);
        debug!("Cleared cache for: {}", id);
    }

    /// 构建缓存键 / Build cache key
    fn build_cache_key(&self, id: &str, context: &InputContext) -> String {
        let mut key = id.to_string();

        if let Some(server) = &context.server_name {
            key = format!("{}:{}", key, server);
        }

        if let Some(tool) = &context.tool_name {
            key = format!("{}:{}", key, tool);
        }

        // 添加其他元数据 / Add other metadata
        if !context.metadata.is_empty() {
            let mut metadata_pairs: Vec<_> = context.metadata.iter().collect();
            metadata_pairs.sort_by(|(k1, _), (k2, _)| k1.cmp(k2)); // 确保顺序一致 / Ensure consistent order

            for (k, v) in metadata_pairs {
                key = format!("{}:{}={}", key, k, v);
            }
        }

        key
    }

    /// 获取缓存值 / Get cached value
    async fn get_cached_value(&self, key: &str) -> Option<InputValue> {
        let cache = self.cache.read().await;
        cache.get(key).cloned()
    }

    /// 缓存值 / Cache value
    async fn cache_value(&self, key: String, value: InputValue) {
        let mut cache = self.cache.write().await;
        cache.insert(key, value);
    }

    /// 从MCP服务器输入配置创建请求 / Create request from MCP server input configuration
    pub fn create_request_from_mcp_input(
        &self,
        mcp_input: &crate::mcp_clients::model::MCPServerInput,
        default: Option<InputValue>,
    ) -> InputRequest {
        match mcp_input {
            crate::mcp_clients::model::MCPServerInput::PromptString(input) => InputRequest {
                id: input.id.clone(),
                input_type: InputType::String {
                    password: input.password,
                    min_length: None,
                    max_length: None,
                },
                title: input.description.clone(),
                description: input.description.clone(),
                default,
                required: true,
                validation: None,
            },
            crate::mcp_clients::model::MCPServerInput::PickString(input) => InputRequest {
                id: input.id.clone(),
                input_type: InputType::PickString {
                    options: input.options.clone(),
                    multiple: false,
                },
                title: input.description.clone(),
                description: input.description.clone(),
                default,
                required: true,
                validation: None,
            },
            crate::mcp_clients::model::MCPServerInput::Command(input) => InputRequest {
                id: input.id.clone(),
                input_type: InputType::Command {
                    command: input.command.clone(),
                    args: input
                        .args
                        .as_ref()
                        .map(|m| m.values().cloned().collect())
                        .unwrap_or_default(),
                },
                title: input.description.clone(),
                description: input.description.clone(),
                default,
                required: true,
                validation: None,
            },
        }
    }

    /// 处理MCP服务器输入 / Handle MCP server inputs
    pub async fn handle_mcp_inputs(
        &self,
        inputs: &[crate::mcp_clients::model::MCPServerInput],
        context: InputContext,
    ) -> InputResult<HashMap<String, InputValue>> {
        let mut results = HashMap::new();
        let mut requests = Vec::new();

        // 创建请求 / Create requests
        for input in inputs {
            let request = self.create_request_from_mcp_input(input, None);
            requests.push(request);
        }

        // 获取输入 / Get inputs
        let responses = self.get_inputs(requests, context).await?;

        // 收集结果 / Collect results
        for response in responses {
            results.insert(response.id, response.value);
        }

        Ok(results)
    }
}

impl Default for InputHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_clients::model::*;

    #[tokio::test]
    async fn test_input_handler_creation() {
        let handler = InputHandler::new();
        assert!(handler.enable_cache);
    }

    #[tokio::test]
    async fn test_cache_key_generation() {
        let handler = InputHandler::new();
        let context = InputContext::new()
            .with_server_name("test_server".to_string())
            .with_tool_name("test_tool".to_string());

        let key = handler.build_cache_key("test_input", &context);
        assert_eq!(key, "test_input:test_server:test_tool");
    }

    #[tokio::test]
    async fn test_cache_operations() {
        let handler = InputHandler::new();
        let _context = InputContext::new();

        // 测试缓存设置和获取 / Test cache set and get
        let key = "test_key";
        let value = InputValue::String("test_value".to_string());

        handler.cache_value(key.to_string(), value.clone()).await;
        let cached = handler.get_cached_value(key).await;

        assert_eq!(cached, Some(value));
    }

    #[tokio::test]
    async fn test_create_request_from_mcp_input() {
        let handler = InputHandler::new();

        let mcp_input = MCPServerInput::PromptString(PromptStringInput {
            id: "test_input".to_string(),
            description: "Test input".to_string(),
            default: Some("default".to_string()),
            password: Some(false),
        });

        let request = handler.create_request_from_mcp_input(&mcp_input, None);

        assert_eq!(request.id, "test_input");
        assert_eq!(request.title, "Test input");
        assert_eq!(request.description, "Test input");
        assert!(request.required);

        match request.input_type {
            InputType::String { password, .. } => {
                assert_eq!(password, Some(false));
            }
            _ => panic!("Expected string input type"),
        }
    }
}
