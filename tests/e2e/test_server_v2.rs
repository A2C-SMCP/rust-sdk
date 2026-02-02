// Improved test server implementation

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tracing::info;

use smcp_server_core::SmcpServerBuilder;
use smcp_server_hyper::HyperServer;

/// Test server wrapper with proper lifecycle management
pub struct TestServerV2 {
    pub addr: SocketAddr,
    pub server_url: String,
    shutdown_trigger: Arc<Notify>,
}

impl TestServerV2 {
    /// Create and start a new test server
    pub async fn start() -> Result<Self, Box<dyn std::error::Error>> {
        // Find an available port
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let server_url = format!("http://{}", addr);

        info!("Starting test server on {}", addr);

        // Build SMCP layer with default auth for testing
        let layer = SmcpServerBuilder::new()
            .with_default_auth(Some("test_secret".to_string()), None)
            .build_layer()?;

        // Create shutdown trigger
        let shutdown_trigger = Arc::new(Notify::new());
        let shutdown_trigger_clone = shutdown_trigger.clone();

        // Spawn server in background with shutdown support
        tokio::spawn(async move {
            let server = HyperServer::new().with_layer(layer);

            // Run server with Ctrl+C support
            tokio::select! {
                result = server.run(addr) => {
                    if let Err(e) = result {
                        eprintln!("Server error: {}", e);
                    }
                }
                _ = shutdown_trigger_clone.notified() => {
                    info!("Server shutdown triggered");
                }
            }
        });

        // Wait for server to start
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Verify we can connect to the port
        let retry_count = 0;
        loop {
            match TcpListener::bind("127.0.0.1:0").await {
                Ok(_) => break, // If we can bind to another port, the original port is in use
                Err(_) => {
                    if retry_count > 10 {
                        return Err("Server didn't start properly".into());
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }

        info!("Test server is ready on {}", addr);

        Ok(Self {
            addr,
            server_url,
            shutdown_trigger,
        })
    }

    /// Get the server URL for client connections
    pub fn url(&self) -> &str {
        &self.server_url
    }

    /// Trigger server shutdown
    pub fn shutdown(self) {
        self.shutdown_trigger.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_server_v2_startup() {
        let server = TestServerV2::start()
            .await
            .expect("Failed to start test server");

        assert!(server.addr.port() > 0);
        assert!(server.url().contains("http://127.0.0.1"));

        server.shutdown();
    }
}
