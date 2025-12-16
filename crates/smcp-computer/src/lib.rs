/*!
* 文件名: lib.rs
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: None
* 描述: A2C-SMCP Computer模块的Rust实现 / Rust implementation of A2C-SMCP Computer module
*/

pub mod desktop;
pub mod errors;
pub mod inputs;
pub mod mcp_clients;
pub mod socketio_client;

#[cfg(test)]
pub use errors::{ComputerError, ComputerResult};

/// Computer模块的版本号 / Version of the Computer module
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
