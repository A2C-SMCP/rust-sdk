/*!
* 文件名: lib.rs
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: None
* 描述: A2C-SMCP Computer模块的Rust实现 / Rust implementation of A2C-SMCP Computer module
*/

pub mod blob;
pub mod computer;
pub mod desktop;
pub mod errors;
pub mod governance;
pub mod inputs;
pub mod inventory;
pub mod mcp_clients;
pub mod settings;
pub mod skills;
pub mod socketio_client;
pub mod status;

#[cfg(feature = "cli")]
pub mod cli;

#[cfg(test)]
pub use errors::{ComputerError, ComputerResult};

/// #107 S7（#114）：runtime status / 事件公开面 re-export / runtime status surface re-export。
pub use status::{ComputerEvent, ComputerStatusSnapshot, LifecycleState, RuntimeStatus};

/// #124：高层 governance snapshot/inventory 公开面 re-export（SDK-facing，consumer 仅经此 + `Computer`）。
pub use governance::{
    DeclaredCapabilities, GovernanceDecision, GovernanceDiagnostic, GovernanceQueryError,
    GovernanceRevision, GovernanceSnapshot, ListPluginsOptions, MarketplaceSnapshot,
    MarketplaceStatus, PluginSnapshot, PluginStatus, ProvenanceScope,
};

/// Computer模块的版本号 / Version of the Computer module
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
