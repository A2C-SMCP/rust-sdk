//! Hyper adapter for SMCP Server
//!
//! This crate provides a Hyper-based HTTP server implementation for the SMCP protocol.
//! It exposes both programmatic API and a standalone binary.

use std::net::SocketAddr;

/// A Hyper-based SMCP server
pub struct HyperServer {
    _addr: SocketAddr,
}

impl HyperServer {
    /// Create a new HyperServer
    pub fn new() -> Self {
        Self {
            _addr: "127.0.0.1:0".parse().unwrap(),
        }
    }

    /// Run the server on the given address
    pub async fn run(
        self,
        addr: SocketAddr,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("Starting SMCP server on {} (placeholder)", addr);

        // TODO: Implement actual server when smcp-server-core is ready
        // For now, just wait for a signal to exit
        tokio::signal::ctrl_c().await?;
        println!("Server stopped");

        Ok(())
    }
}

/// Builder for creating and configuring a HyperServer
pub struct HyperServerBuilder {
    _config: (),
}

impl HyperServerBuilder {
    /// Create a new builder
    pub fn new() -> Self {
        Self { _config: () }
    }

    /// Build the HyperServer
    pub fn build(self) -> HyperServer {
        HyperServer::new()
    }
}

impl Default for HyperServerBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Convenience function to quickly run a server with default configuration
pub async fn run_server(addr: SocketAddr) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let server = HyperServerBuilder::new().build();
    server.run(addr).await
}
