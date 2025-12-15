#![allow(unexpected_cfgs)]

/*!
* 文件名: mod.rs
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: None
* 描述: MCP客户端模块 / MCP client module
*/

pub mod base;
pub mod factory;
pub mod model;
pub mod stdio;
#[cfg(feature = "sse")]
pub mod sse;
#[cfg(feature = "http")]
pub mod streamable_http;

pub use base::*;
pub use factory::client_factory;
pub use model::*;

#[cfg(feature = "sse")]
pub use sse::SseMcpClient;
#[cfg(feature = "http")]
pub use streamable_http::StreamableHttpMcpClient;
