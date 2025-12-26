// Test server implementation
//
// Provides a wrapper around SMCP server for testing purposes

use http_body_util::Full;
use hyper::body::Bytes;
use hyper::{Method, StatusCode};
use hyper_util::rt::TokioIo;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tower::{Layer, Service};
use tracing::info;

use smcp_server_core::SmcpServerBuilder;

/// Test server wrapper with proper lifecycle management
pub struct TestServer {
    pub addr: SocketAddr,
    pub server_url: String,
    _shutdown_tx: broadcast::Sender<()>,
}

impl TestServer {
    /// Create and start a new test server
    pub async fn start() -> Result<Self, Box<dyn std::error::Error>> {
        // Create TCP listener and bind to available port
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let server_url = format!("http://{}", addr);

        info!("Starting test server on {}", addr);

        // Build SMCP layer with default auth for testing
        // This layer already has all Socket.IO handlers registered
        let smcp_layer = SmcpServerBuilder::new()
            .with_default_auth(Some("test_secret".to_string()), None)
            .build_layer()?;

        // Create shutdown channel
        let (shutdown_tx, mut shutdown_rx) = broadcast::channel::<()>(1);

        // Spawn server in background
        tokio::spawn(async move {
            info!("Test server task started on {}", addr);

            // Accept connections with shutdown support
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        match result {
                            Ok((stream, _remote_addr)) => {
                                let layer = smcp_layer.clone();
                                let io = TokioIo::new(stream);

                                tokio::spawn(async move {
                                    // Create service that chains socket.io layer with fallback
                                    let svc = tower::service_fn(|req| {
                                        let layer = layer.clone();
                                        async move {
                                            // Use tower layer chain: socket.io layer -> fallback service
                                            let mut svc = layer.layer.layer(tower::service_fn(|req: hyper::Request<hyper::body::Incoming>| async move {
                                                // Fallback service for non-socket.io routes
                                                match (req.method(), req.uri().path()) {
                                                    (&Method::GET, "/") => {
                                                        Ok::<_, std::convert::Infallible>(
                                                            hyper::Response::builder()
                                                                .status(StatusCode::OK)
                                                                .body(Full::new(Bytes::from("SMCP Server is running")))
                                                                .unwrap()
                                                        )
                                                    }
                                                    (&Method::GET, "/health") => {
                                                        Ok::<_, std::convert::Infallible>(
                                                            hyper::Response::builder()
                                                                .status(StatusCode::OK)
                                                                .header("content-type", "application/json")
                                                                .body(Full::new(Bytes::from("{\"status\":\"ok\"}")))
                                                                .unwrap()
                                                        )
                                                    }
                                                    _ => {
                                                        Ok::<_, std::convert::Infallible>(
                                                            hyper::Response::builder()
                                                                .status(StatusCode::NOT_FOUND)
                                                                .body(Full::new(Bytes::from("Not found")))
                                                                .unwrap()
                                                        )
                                                    }
                                                }
                                            }));
                                            svc.call(req).await
                                        }
                                    });

                                    // Convert tower service to hyper service
                                    let svc = hyper_util::service::TowerToHyperService::new(svc);

                                    let _ = hyper::server::conn::http1::Builder::new()
                                        .serve_connection(io, svc)
                                        .with_upgrades()
                                        .await;
                                });
                            }
                            Err(e) => {
                                eprintln!("Server accept error: {}", e);
                                break;
                            }
                        }
                    }
                    _ = shutdown_rx.recv() => {
                        info!("Server shutdown signal received");
                        break;
                    }
                }
            }

            info!("Test server task finished");
        });

        // Wait a bit for server to actually start listening
        tokio::time::sleep(Duration::from_millis(100)).await;

        info!("Test server is ready on {}", addr);

        Ok(Self {
            addr,
            server_url,
            _shutdown_tx: shutdown_tx,
        })
    }

    /// Get the server address
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Get the server URL for client connections
    pub fn url(&self) -> &str {
        &self.server_url
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_test_server_startup() {
        let server = TestServer::start()
            .await
            .expect("Failed to start test server");

        assert!(server.addr.port() > 0);
        assert!(server.url().contains("http://127.0.0.1"));
    }
}
