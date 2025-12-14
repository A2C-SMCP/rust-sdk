//! Hyper adapter for SMCP Server
//!
//! This crate provides a Hyper-based HTTP server implementation for the SMCP protocol.
//! It exposes both programmatic API and a standalone binary.

use std::convert::Infallible;
use std::net::SocketAddr;

use http_body_util::Full;
use hyper::body::Bytes;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use socketioxide::SocketIo;
use tokio::net::TcpListener;
use tower::ServiceBuilder;
use tracing::{error, info};

use smcp_server_core::SmcpServerLayer;

/// A Hyper-based SMCP server
pub struct HyperServer {
    pub layer: Option<SmcpServerLayer>,
    pub addr: SocketAddr,
}

impl HyperServer {
    /// Create a new HyperServer
    pub fn new() -> Self {
        Self {
            layer: None,
            addr: "127.0.0.1:0".parse().unwrap(),
        }
    }

    /// Set the SMCP server layer
    pub fn with_layer(mut self, layer: SmcpServerLayer) -> Self {
        self.layer = Some(layer);
        self
    }

    /// Run the server on the given address
    pub async fn run(
        self,
        addr: SocketAddr,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let layer = self.layer.ok_or("SMCP layer not configured")?;

        info!("Starting SMCP server on {}", addr);

        // Create a TCP listener
        let listener = TcpListener::bind(addr).await?;
        let local_addr = listener.local_addr()?;
        info!("Server listening on {}", local_addr);

        // Build the service stack
        let service = ServiceBuilder::new()
            .layer(layer.layer)
            .service(service_fn(move |req| {
                let io = layer.io.clone();
                async move { handle_request(req, &io).await }
            }));

        // Serve connections
        loop {
            let (stream, remote_addr) = listener.accept().await?;
            info!("New connection from: {}", remote_addr);

            let service = service.clone();
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                if let Err(err) = hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, service)
                    .await
                {
                    error!("Failed to serve connection: {}", err);
                }
            });
        }
    }
}

impl Default for HyperServer {
    fn default() -> Self {
        Self::new()
    }
}

/// Handle HTTP requests
pub async fn handle_request(
    req: Request<hyper::body::Incoming>,
    _io: &SocketIo,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let response = match (req.method(), req.uri().path()) {
        (&Method::GET, "/") => Response::builder()
            .status(StatusCode::OK)
            .body(Full::new(Bytes::from("SMCP Server is running")))
            .unwrap(),
        (&Method::GET, "/health") => Response::builder()
            .status(StatusCode::OK)
            .body(Full::new(Bytes::from("{\"status\":\"ok\"}")))
            .unwrap(),
        (&Method::GET, "/socket.io/") => {
            // Socket.IO will handle these requests through the layer
            Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Full::new(Bytes::from("Not found")))
                .unwrap()
        }
        _ => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Full::new(Bytes::from("Not found")))
            .unwrap(),
    };

    Ok(response)
}

/// Builder for creating and configuring a HyperServer
pub struct HyperServerBuilder {
    layer: Option<SmcpServerLayer>,
    addr: Option<SocketAddr>,
}

impl HyperServerBuilder {
    /// Create a new builder
    pub fn new() -> Self {
        Self {
            layer: None,
            addr: None,
        }
    }

    /// Set the SMCP server layer
    pub fn with_layer(mut self, layer: SmcpServerLayer) -> Self {
        self.layer = Some(layer);
        self
    }

    /// Set the server address
    pub fn with_addr(mut self, addr: SocketAddr) -> Self {
        self.addr = Some(addr);
        self
    }

    /// Build the HyperServer
    pub fn build(self) -> HyperServer {
        let mut server = HyperServer::new();
        if let Some(layer) = self.layer {
            server = server.with_layer(layer);
        }
        if let Some(addr) = self.addr {
            server.addr = addr;
        }
        server
    }
}

impl Default for HyperServerBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Convenience function to quickly run a server with default configuration
pub async fn run_server(addr: SocketAddr) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Build SMCP layer with default configuration
    let layer = smcp_server_core::SmcpServerBuilder::new()
        .build_layer()
        .map_err(|e| format!("Failed to build SMCP layer: {}", e))?;

    let server = HyperServerBuilder::new()
        .with_layer(layer)
        .with_addr(addr)
        .build();

    server.run(addr).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hyper_server_creation() {
        let server = HyperServer::new();
        assert_eq!(server.addr, "127.0.0.1:0".parse().unwrap());
    }

    #[test]
    fn test_hyper_server_builder() {
        let builder = HyperServerBuilder::new();
        assert!(builder.layer.is_none());
        assert!(builder.addr.is_none());
    }
}
