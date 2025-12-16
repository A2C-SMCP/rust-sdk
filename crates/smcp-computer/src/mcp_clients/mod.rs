/**
* 文件名: mod
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: None
* 描述: MCP客户端模块，负责管理与MCP服务器的连接和交互
*/
// 模块声明 / Module declarations
pub mod base_client;
pub mod http_client;
pub mod manager;
pub mod model;
pub mod sse_client;
pub mod stdio_client;
pub mod utils;

// 重新导出核心类型 / Re-export core types
pub use base_client::BaseMCPClient;
pub use manager::{MCPServerManager, ToolNameDuplicatedError};
pub use model::*;
pub use utils::client_factory;
