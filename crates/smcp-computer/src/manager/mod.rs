/*!
* 文件名: mod.rs
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: None
* 描述: MCP服务器管理器模块 / MCP server manager module
*/

#![allow(clippy::module_inception)]

pub mod errors;
pub mod manager;

pub use errors::*;
pub use manager::McpServerManager;
