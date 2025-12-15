/*!
* 文件名: lib
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: None
* 描述: SMCP Agent库 / SMCP Agent library
*/

pub mod async_agent;
pub mod auth;
pub mod config;
pub mod error;
pub mod events;
pub mod sync_agent;
pub mod transport;

// 重新导出主要类型
pub use async_agent::AsyncSmcpAgent;
pub use auth::{AuthProvider, DefaultAuthProvider};
pub use config::SmcpAgentConfig;
pub use error::{Result, SmcpAgentError};
pub use events::{AgentEventHandler, AsyncAgentEventHandler};
pub use sync_agent::SyncSmcpAgent;
