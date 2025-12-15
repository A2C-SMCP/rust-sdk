# A2C-SMCP Computer Module (Rust Implementation)

Rust版本的A2C-SMCP Computer模块实现，提供MCP服务器连接管理和工具调用功能。

## 功能特性 / Features

### 核心功能 / Core Features
- **多服务器管理**: 同时管理多个MCP服务器连接
- **连接类型支持**: 支持STDIO、HTTP、SSE三种连接方式
- **工具冲突处理**: 自动检测工具名冲突，支持别名机制
- **生命周期管理**: 严格的进程和内存安全管理
- **输入抽象**: 支持CLI、环境变量等多种输入方式

### 安全特性 / Security Features
- **进程安全**: 使用tokio::process管理子进程，防止进程泄漏
- **内存安全**: 使用Arc<RwLock>管理共享状态，避免数据竞争
- **超时控制**: 所有操作支持超时机制，防止任务挂起
- **资源清理**: 显式的shutdown机制，确保资源正确释放

## 架构设计 / Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                    MCPServerManager                         │
│  ┌─────────────────┐  ┌─────────────────┐  ┌──────────────┐ │
│  │  Server Configs │  │  Active Clients │  │ Tool Mapping │ │
│  └─────────────────┘  └─────────────────┘  └──────────────┘ │
└─────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────┐
│                   MCPClientProtocol                         │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────┐ │
│  │StdioClient  │  │HttpClient  │  │    SseClient        │ │
│  └─────────────┘  └─────────────┘  └─────────────────────┘ │
└─────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────┐
│                     InputHandler                            │
│  ┌─────────────────┐  ┌─────────────────┐  ┌──────────────┐ │
│  │  CLI Provider   │  │ Env Provider    │  │ Composite     │ │
│  └─────────────────┘  └─────────────────┘  └──────────────┘ │
└─────────────────────────────────────────────────────────────┘
```

## 快速开始 / Quick Start

### 基本用法 / Basic Usage

```rust
use smcp_computer::mcp_clients::*;
use std::collections::HashMap;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. 创建管理器 / Create manager
    let manager = MCPServerManager::new();
    
    // 2. 准备服务器配置 / Prepare server configurations
    let configs = vec![
        MCPServerConfig::Stdio(StdioServerConfig {
            name: "echo_server".to_string(),
            disabled: false,
            forbidden_tools: vec![],
            tool_meta: HashMap::new(),
            default_tool_meta: None,
            vrl: None,
            server_parameters: StdioServerParameters {
                command: "echo".to_string(),
                args: vec!["Hello MCP".to_string()],
                env: HashMap::new(),
                cwd: None,
            },
        }),
    ];
    
    // 3. 初始化并启动服务器 / Initialize and start servers
    manager.initialize(configs).await?;
    manager.start_all().await?;
    
    // 4. 调用工具 / Call tools
    let result = manager.execute_tool(
        "echo",
        serde_json::json!({"message": "test"}),
        Some(std::time::Duration::from_secs(5)),
    ).await?;
    
    println!("Tool result: {:?}", result);
    
    // 5. 清理资源 / Cleanup
    manager.close().await?;
    
    Ok(())
}
```

### 输入处理 / Input Handling

```rust
use smcp_computer::inputs::*;

// 创建输入处理器 / Create input handler
let input_handler = InputHandler::new();

// 创建输入请求 / Create input request
let request = InputRequest {
    id: "api_key".to_string(),
    input_type: InputType::String {
        password: Some(true),
        min_length: None,
        max_length: None,
    },
    title: "API Key".to_string(),
    description: "Enter your API key".to_string(),
    default: None,
    required: true,
    validation: None,
};

// 获取输入 / Get input
let context = InputContext::new()
    .with_server_name("my_server".to_string())
    .with_tool_name("authenticate".to_string());

let response = input_handler.get_input(request, context).await?;
println!("Input value: {}", response.value);
```

## 配置说明 / Configuration

### 服务器配置 / Server Configuration

#### STDIO服务器 / STDIO Server
```rust
MCPServerConfig::Stdio(StdioServerConfig {
    name: "server_name".to_string(),
    disabled: false,
    forbidden_tools: vec!["dangerous_tool".to_string()],
    tool_meta: {
        let mut meta = HashMap::new();
        meta.insert("my_tool".to_string(), ToolMeta {
            auto_apply: Some(true),
            alias: Some("custom_alias".to_string()),
            tags: Some(vec!["utility".to_string()]),
            ret_object_mapper: None,
        });
        meta
    },
    default_tool_meta: Some(ToolMeta::default()),
    vrl: Some("return . | filter(.status == \"success\")".to_string()),
    server_parameters: StdioServerParameters {
        command: "python".to_string(),
        args: vec!["server.py".to_string()],
        env: {
            let mut env = HashMap::new();
            env.insert("PYTHONPATH".to_string(), "/path/to/modules".to_string());
            env
        },
        cwd: Some("/workspace".to_string()),
    },
})
```

#### HTTP服务器 / HTTP Server
```rust
MCPServerConfig::Http(HttpServerConfig {
    name: "http_server".to_string(),
    disabled: false,
    forbidden_tools: vec![],
    tool_meta: HashMap::new(),
    default_tool_meta: None,
    vrl: None,
    server_parameters: HttpServerParameters {
        url: "http://localhost:8080/mcp".to_string(),
        headers: {
            let mut headers = HashMap::new();
            headers.insert("Authorization".to_string(), "Bearer token".to_string());
            headers
        },
    },
})
```

#### SSE服务器 / SSE Server
```rust
MCPServerConfig::Sse(SseServerConfig {
    name: "sse_server".to_string(),
    disabled: false,
    forbidden_tools: vec![],
    tool_meta: HashMap::new(),
    default_tool_meta: None,
    vrl: None,
    server_parameters: SseServerParameters {
        url: "http://localhost:8080/events".to_string(),
        headers: HashMap::new(),
    },
})
```

### 工具元数据 / Tool Metadata

```rust
ToolMeta {
    auto_apply: Some(true),           // 是否自动应用结果
    alias: Some("custom_name".to_string()), // 工具别名
    tags: Some(vec!["category".to_string()]), // 标签
    ret_object_mapper: Some({
        let mut mapper = HashMap::new();
        mapper.insert("result".to_string(), "output".to_string());
        mapper
    }), // 结果映射
}
```

## 高级用法 / Advanced Usage

### 自定义输入提供者 / Custom Input Provider

```rust
use async_trait::async_trait;

struct CustomInputProvider {
    default_value: String,
}

#[async_trait]
impl InputProvider for CustomInputProvider {
    async fn get_input(&self, request: &InputRequest, _context: &InputContext) -> InputResult<InputResponse> {
        Ok(InputResponse {
            id: request.id.clone(),
            value: InputValue::String(self.default_value.clone()),
            cancelled: false,
        })
    }
}

// 使用自定义提供者 / Use custom provider
let handler = InputHandler::with_provider(CustomInputProvider {
    default_value: "custom_value".to_string(),
});
```

### 工具冲突处理 / Tool Conflict Resolution

当多个服务器有同名工具时，必须使用别名解决冲突：

```rust
// 服务器1 / Server 1
MCPServerConfig::Stdio(StdioServerConfig {
    name: "server1".to_string(),
    tool_meta: {
        let mut meta = HashMap::new();
        meta.insert("common_tool".to_string(), ToolMeta {
            alias: Some("server1_tool".to_string()),
            ..Default::default()
        });
        meta
    },
    // ...
})

// 服务器2 / Server 2  
MCPServerConfig::Stdio(StdioServerConfig {
    name: "server2".to_string(),
    tool_meta: {
        let mut meta = HashMap::new();
        meta.insert("common_tool".to_string(), ToolMeta {
            alias: Some("server2_tool".to_string()),
            ..Default::default()
        });
        meta
    },
    // ...
})

// 使用别名调用工具 / Call tools using aliases
manager.execute_tool("server1_tool", params, timeout).await?;
manager.execute_tool("server2_tool", params, timeout).await?;
```

### 错误处理 / Error Handling

```rust
match manager.execute_tool("tool_name", params, timeout).await {
    Ok(result) => {
        println!("Success: {:?}", result);
    }
    Err(ComputerError::ConfigurationError(msg)) => {
        eprintln!("Configuration error: {}", msg);
    }
    Err(ComputerError::ConnectionError(msg)) => {
        eprintln!("Connection error: {}", msg);
    }
    Err(ComputerError::ProtocolError(msg)) => {
        eprintln!("Protocol error: {}", msg);
    }
    Err(ComputerError::TimeoutError(msg)) => {
        eprintln!("Timeout error: {}", msg);
    }
    Err(e) => {
        eprintln!("Other error: {}", e);
    }
}
```

## 性能优化 / Performance Optimization

### 并发控制 / Concurrency Control

```rust
// 启用自动连接和重连 / Enable auto connect and reconnect
manager.enable_auto_connect().await;
manager.enable_auto_reconnect().await;

// 并发启动多个服务器 / Start multiple servers concurrently
let server_configs = vec![/* ... */];
manager.initialize(server_configs).await?;
tokio::try_join!(
    manager.start_client("server1"),
    manager.start_client("server2"),
    manager.start_client("server3"),
)?;
```

### 缓存优化 / Cache Optimization

```rust
// 启用输入缓存 / Enable input caching
let handler = InputHandler::new().with_cache(true);

// 批量获取输入 / Batch get inputs
let requests = vec![/* ... */];
let responses = handler.get_inputs(requests, context).await?;

// 清理缓存 / Clear cache
handler.clear_cache().await;
```

## 测试 / Testing

运行测试套件：

```bash
# 运行所有测试 / Run all tests
cargo test

# 运行特定测试 / Run specific tests
cargo test mcp_client_tests
cargo test manager_tests  
cargo test input_tests
cargo test integration_tests

# 运行测试并显示输出 / Run tests with output
cargo test -- --nocapture
```

## 故障排除 / Troubleshooting

### 常见问题 / Common Issues

1. **进程泄漏 / Process Leaks**
   - 确保调用`manager.close().await`清理资源
   - 使用显式的disconnect而不是依赖Drop trait

2. **连接超时 / Connection Timeouts**
   - 检查网络连接和防火墙设置
   - 适当调整超时时间

3. **工具冲突 / Tool Conflicts**
   - 使用alias机制解决同名工具冲突
   - 检查forbidden_tools配置

4. **内存使用 / Memory Usage**
   - 定期清理输入缓存
   - 监控活动客户端数量

### 调试技巧 / Debugging Tips

```rust
// 启用详细日志 / Enable verbose logging
use tracing_subscriber;
tracing_subscriber::fmt::init();

// 监控服务器状态 / Monitor server status
let status = manager.get_server_status().await;
for (name, active, state) in status {
    println!("Server {}: active={}, state={}", name, active, state);
}

// 检查工具映射 / Check tool mapping
let tools = manager.list_available_tools().await;
for tool in tools {
    println!("Available tool: {}", tool.name);
}
```

## 贡献 / Contributing

欢迎提交Issue和Pull Request来改进这个项目。

## 许可证 / License

本项目采用MIT许可证。详见LICENSE文件。

## 更新日志 / Changelog

### v0.1.0 (2025-12-15)
- 初始版本发布
- 实现基本的MCP客户端管理功能
- 支持STDIO、HTTP、SSE三种连接方式
- 实现输入抽象层
- 完整的测试覆盖
