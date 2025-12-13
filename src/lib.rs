//! # A2C-SMCP Rust SDK
//!
//! A Rust implementation of the A2C-SMCP protocol, providing Agent, Computer, and Server
//! components for building intelligent agent systems with tool execution capabilities.
//!
//! ## Features
//!
//! - **agent** - Agent client implementation for connecting to SMCP servers
//! - **computer** - Computer client implementation for managing MCP servers and desktop resources
//! - **server** - Server implementation with Socket.IO support
//! - **full** - Enables all features (default when using `--all-features`)
//!
//! ## Quick Start
//!
//! Add to your `Cargo.toml`:
//!
//! ```toml
//! [dependencies]
//! a2c-smcp = { version = "0.1.0", features = ["agent", "computer"] }
//! ```
//!
//! ## Example
//!
//! ```rust,no_run,ignore
//! // Add features to your Cargo.toml:
//! // a2c-smcp = { version = "0.1.0", features = ["agent", "computer"] }
//!
//! #[cfg(feature = "agent")]
//! use a2c_smcp::agent::SmcpAgent;
//!
//! #[cfg(feature = "computer")]
//! use a2c_smcp::computer::SmcpComputer;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Your SMCP application code here
//!     Ok(())
//! }
//! ```

// Re-export core protocol types (always available)
pub use smcp::*;

// Re-export optional components based on features
#[cfg(feature = "agent")]
pub use smcp_agent;

#[cfg(feature = "computer")]
pub use smcp_computer;

#[cfg(feature = "server")]
pub use smcp_server_core;

#[cfg(feature = "server")]
pub use smcp_server_hyper;

// Re-export commonly used dependencies for convenience
pub use serde;
pub use serde_json;
pub use thiserror;
pub use tokio;
pub use tracing;

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        let result = 2 + 2;
        assert_eq!(result, 4);
    }
}
