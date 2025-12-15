/**
* 文件名: test_stdio_server_bin
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: rmcp, tokio, serde_json
* 描述: 测试用的STDIO MCP服务器二进制 / Test STDIO MCP server binary
*/

use rmcp::{
    ErrorData as McpError,
    handler::server::{
        ServerHandler,
        tool::{ToolCallContext, ToolRouter},
    },
    model::{CallToolRequestParam, CallToolResult, JsonObject, ListToolsResult, PaginatedRequestParam},
    service::{RequestContext, RoleServer, ServiceExt},
    tool,
    tool_router,
    transport::io::stdio,
};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing_subscriber::EnvFilter;

/// 简单的计算器服务器实现 / Simple calculator server implementation
#[derive(Debug, Clone)]
pub struct CalculatorServer {
    state: Arc<RwLock<f64>>,
    tool_router: ToolRouter<Self>,
}

impl CalculatorServer {
    pub fn new() -> Self {
        Self {
            state: Arc::new(RwLock::new(0.0)),
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl CalculatorServer {
    #[tool(name = "add", description = "Add two numbers")]
    async fn add(&self, args: JsonObject) -> Result<CallToolResult, McpError> {
        let a = args
            .get("a")
            .and_then(Value::as_f64)
            .ok_or_else(|| McpError::invalid_params("Missing or invalid 'a'", None))?;
        let b = args
            .get("b")
            .and_then(Value::as_f64)
            .ok_or_else(|| McpError::invalid_params("Missing or invalid 'b'", None))?;
        let result = a + b;
        *self.state.write().await = result;
        Ok(CallToolResult::success(vec![rmcp::model::Content::text(
            serde_json::to_string(&json!({ "result": result })).unwrap(),
        )]))
    }

    #[tool(name = "subtract", description = "Subtract two numbers")]
    async fn subtract(&self, args: JsonObject) -> Result<CallToolResult, McpError> {
        let a = args
            .get("a")
            .and_then(Value::as_f64)
            .ok_or_else(|| McpError::invalid_params("Missing or invalid 'a'", None))?;
        let b = args
            .get("b")
            .and_then(Value::as_f64)
            .ok_or_else(|| McpError::invalid_params("Missing or invalid 'b'", None))?;
        let result = a - b;
        *self.state.write().await = result;
        Ok(CallToolResult::success(vec![rmcp::model::Content::text(
            serde_json::to_string(&json!({ "result": result })).unwrap(),
        )]))
    }

    #[tool(name = "get_value", description = "Get the current stored value")]
    async fn get_value(&self) -> Result<CallToolResult, McpError> {
        let value = *self.state.read().await;
        Ok(CallToolResult::success(vec![rmcp::model::Content::text(
            serde_json::to_string(&json!({ "value": value })).unwrap(),
        )]))
    }
}

impl ServerHandler for CalculatorServer {
    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParam>,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListToolsResult, McpError>> + Send + '_ {
        async move {
            Ok(ListToolsResult {
                meta: None,
                tools: self.tool_router.list_all(),
                next_cursor: None,
            })
        }
    }

    fn call_tool(
        &self,
        request: CallToolRequestParam,
        context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<CallToolResult, McpError>> + Send + '_ {
        async move {
            let tool_context = ToolCallContext::new(self, request, context);
            self.tool_router.call(tool_context).await
        }
    }

    fn get_info(&self) -> rmcp::model::ServerInfo {
        rmcp::model::ServerInfo::default()
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 初始化日志 / Initialize logging
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let server = CalculatorServer::new();
    
    // 运行 stdio 服务器 / Run stdio server
    let service = server.serve(stdio()).await.inspect_err(|e| {
        eprintln!("Error starting server: {}", e);
    })?;
    
    service.waiting().await?;
    
    Ok(())
}
