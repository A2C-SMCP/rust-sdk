/*!
* 文件名: socketio_client_test
* 作者: JQQ
* 创建日期: 2025/12/16
* 最后修改日期: 2025/12/16
* 版权: 2023 JQQ. All rights reserved.
* 依赖: tokio, smcp-server-core
* 描述: SMCP Computer Socket.IO客户端的集成测试 / Integration tests for SMCP Computer Socket.IO client
*/

#[cfg(test)]
mod tests {
    use smcp_computer::errors::ComputerResult;
    use smcp_computer::mcp_clients::manager::MCPServerManager;
    use smcp_computer::socketio_client::SmcpComputerClient;
    use smcp_server_core::SmcpServerBuilder;
    use smcp_server_core::auth::{AuthenticationProvider, AuthError};
    use std::sync::Arc;
    use tokio::sync::Mutex;
    use tokio::time::{sleep, Duration};
    use tracing::info;
    use std::net::SocketAddr;
    use http_body_util::Full;
    use hyper::body::Bytes;
    use async_trait::async_trait;
    use hyper::HeaderMap;
    use tower::service_fn;
    use tower::Layer;
    use tower::Service;

    /// 测试用的无操作认证提供者 - 不进行任何认证检查
    /// No-op authentication provider for tests - performs no authentication checks
    #[derive(Debug)]
    struct NoOpAuthProvider;

    #[async_trait]
    impl AuthenticationProvider for NoOpAuthProvider {
        async fn authenticate(
            &self,
            _headers: &HeaderMap,
            _auth: Option<&serde_json::Value>,
        ) -> Result<(), AuthError> {
            Ok(())
        }
    }

    /// 启动测试服务器
    async fn start_test_server() -> String {
        // 构建SMCP服务器层 - 使用无操作认证提供者以避免API key检查
        // Build SMCP server layer - use no-op auth provider to avoid API key checks
        let layer = SmcpServerBuilder::new()
            .with_auth_provider(Arc::new(NoOpAuthProvider))
            .build_layer()
            .expect("Failed to build SMCP layer");
        
        // 使用随机端口
        // Use random port
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        info!("Starting test server on {}", addr);
        
        // 使用oneshot channel传递实际端口
        // Use oneshot channel to pass actual port
        let (tx, rx) = tokio::sync::oneshot::channel::<u16>();
        
        // 在后台运行服务器
        // Run server in background
        let layer_clone = layer.clone();
        tokio::spawn(async move {
            // 创建TCP监听器
            // Create TCP listener
            let listener = tokio::net::TcpListener::bind(addr).await.expect("Failed to bind");
            let local_addr = listener.local_addr().expect("Failed to get local addr");
            info!("Test server listening on {}", local_addr);
            
            // 发送实际端口
            // Send actual port
            let _ = tx.send(local_addr.port());
            
            // 构建服务栈 - 只创建一次
            // Build service stack - create only once
            let fallback_service = service_fn(|req: hyper::Request<hyper::body::Incoming>| async move {
                // 这个 fallback 服务只处理 socketioxide 不处理的请求
                // This fallback service only handles requests not handled by socketioxide
                match (req.method(), req.uri().path()) {
                    (&hyper::Method::GET, "/") => {
                        Ok::<_, std::convert::Infallible>(
                            hyper::Response::builder()
                                .status(hyper::StatusCode::OK)
                                .body(Full::<Bytes>::from("SMCP Test Server"))
                                .unwrap()
                        )
                    }
                    _ => {
                        // 默认返回404
                        // Default return 404
                        Ok::<_, std::convert::Infallible>(
                            hyper::Response::builder()
                                .status(hyper::StatusCode::NOT_FOUND)
                                .body(Full::<Bytes>::from("Not found"))
                                .unwrap()
                        )
                    }
                }
            });
            
            // 应用 layer 只一次
            // Apply layer only once
            let service = layer_clone.layer.layer(fallback_service);
            
            // 处理连接
            // Handle connections
            loop {
                let (stream, _) = listener.accept().await.expect("Failed to accept");
                let service = service.clone();
                tokio::spawn(async move {
                    let stream = hyper_util::rt::TokioIo::new(stream);
                    let svc = hyper_util::service::TowerToHyperService::new(service);
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(stream, svc)
                        .with_upgrades()
                        .await;
                });
            }
        });
        
        // 等待获取实际端口
        // Wait for actual port
        let port = rx.await.expect("Failed to receive port");
        
        // 等待服务器完全启动，避免竞态条件
        // Wait for server to be fully ready, avoiding race condition
        sleep(Duration::from_millis(100)).await;
        
        format!("http://127.0.0.1:{}", port)
    }

    #[tokio::test]
    async fn test_socketio_client_connection() -> ComputerResult<()> {
        // 初始化日志 - 只初始化一次
        // Initialize logging - only initialize once
        let _ = tracing_subscriber::fmt::try_init();
        
        // 启动测试服务器
        let server_url = start_test_server().await;
        
        // 创建MCP管理器
        let manager = Arc::new(Mutex::new(MCPServerManager::new()));
        
        // 创建Socket.IO客户端
        let client = SmcpComputerClient::new(
            &server_url,
            manager.clone(),
            "test_computer".to_string(),
        ).await?;
        
        // 等待一小段时间确保连接稳定
        tokio::time::sleep(Duration::from_millis(100)).await;
        
        // 测试连接状态
        let office_id = client.get_office_id().await;
        assert!(office_id.is_none(), "Office ID should be None initially");
        
        // 断开连接
        client.disconnect().await?;
        
        Ok(())
    }

    #[tokio::test]
    async fn test_join_and_leave_office() -> ComputerResult<()> {
        // 初始化日志 - 只初始化一次
        // Initialize logging - only initialize once
        let _ = tracing_subscriber::fmt::try_init();
        
        // 启动测试服务器
        let server_url = start_test_server().await;
        
        // 创建MCP管理器
        let manager = Arc::new(Mutex::new(MCPServerManager::new()));
        
        // 创建Socket.IO客户端
        let client = SmcpComputerClient::new(
            &server_url,
            manager.clone(),
            "test_computer".to_string(),
        ).await?;
        
        // 等待一小段时间确保连接稳定
        tokio::time::sleep(Duration::from_millis(100)).await;
        
        // 加入Office
        let office_id = "test_office_123";
        client.join_office(office_id).await?;
        
        // 验证已加入Office
        let current_office_id = client.get_office_id().await;
        assert_eq!(current_office_id, Some(office_id.to_string()));
        
        // 离开Office
        client.leave_office(office_id).await?;
        
        // 验证已离开Office
        let current_office_id = client.get_office_id().await;
        assert!(current_office_id.is_none());
        
        // 断开连接
        client.disconnect().await?;
        
        Ok(())
    }

    #[tokio::test]
    async fn test_emit_notifications() -> ComputerResult<()> {
        // 初始化日志 - 只初始化一次
        // Initialize logging - only initialize once
        let _ = tracing_subscriber::fmt::try_init();
        
        // 启动测试服务器
        let server_url = start_test_server().await;
        
        // 创建MCP管理器
        let manager = Arc::new(Mutex::new(MCPServerManager::new()));
        
        // 创建Socket.IO客户端
        let client = SmcpComputerClient::new(
            &server_url,
            manager.clone(),
            "test_computer".to_string(),
        ).await?;
        
        // 等待一小段时间确保连接稳定
        tokio::time::sleep(Duration::from_millis(100)).await;
        
        // 先加入Office
        let office_id = "test_office_456";
        client.join_office(office_id).await?;
        
        // 测试发送各种通知
        client.emit_update_config().await?;
        client.emit_update_tool_list().await?;
        client.emit_update_desktop().await?;
        
        // 离开Office
        client.leave_office(office_id).await?;
        
        // 断开连接
        client.disconnect().await?;
        
        Ok(())
    }

    #[tokio::test]
    async fn test_error_handling() -> ComputerResult<()> {
        // 初始化日志 - 只初始化一次
        // Initialize logging - only initialize once
        let _ = tracing_subscriber::fmt::try_init();
        
        // 启动测试服务器
        let server_url = start_test_server().await;
        
        // 创建MCP管理器
        let manager = Arc::new(Mutex::new(MCPServerManager::new()));
        
        // 创建Socket.IO客户端
        let client = SmcpComputerClient::new(
            &server_url,
            manager.clone(),
            "test_computer".to_string(),
        ).await?;
        
        // 等待一小段时间确保连接稳定
        tokio::time::sleep(Duration::from_millis(100)).await;
        
        // 尝试加入空的Office（当前实现允许空字符串）
        let result = client.join_office("").await;
        // 注意：服务器当前允许空的office_id，所以这里会成功
        // Note: Server currently allows empty office_id, so this will succeed
        assert!(result.is_ok(), "Empty office_id is currently allowed");
        
        // 验证已加入空office
        let current_office_id = client.get_office_id().await;
        assert_eq!(current_office_id, Some("".to_string()));
        
        // 尝试离开未加入的Office（应该成功但不报错）
        client.leave_office("nonexistent").await?;
        
        // 断开连接
        client.disconnect().await?;
        
        Ok(())
    }

    #[tokio::test]
    async fn test_multiple_clients() -> ComputerResult<()> {
        // 初始化日志 - 只初始化一次
        // Initialize logging - only initialize once
        let _ = tracing_subscriber::fmt::try_init();
        
        // 启动测试服务器
        let server_url = start_test_server().await;
        
        // 创建多个客户端
        let mut clients = Vec::new();
        
        for i in 0..3 {
            let manager = Arc::new(Mutex::new(MCPServerManager::new()));
            let client = SmcpComputerClient::new(
                &server_url,
                manager,
                format!("test_computer_{}", i),
            ).await?;
            
            // 等待一小段时间确保连接稳定
            tokio::time::sleep(Duration::from_millis(100)).await;
            
            clients.push(client);
        }
        
        // 每个客户端加入不同的Office
        for (i, client) in clients.iter().enumerate() {
            let office_id = format!("test_office_{}", i);
            client.join_office(&office_id).await?;
            
            let current_office_id = client.get_office_id().await;
            assert_eq!(current_office_id, Some(office_id));
        }
        
        // 清理：断开所有客户端
        for client in clients {
            client.disconnect().await?;
        }
        
        Ok(())
    }

    #[tokio::test]
    async fn test_reconnect_after_disconnect() -> ComputerResult<()> {
        // 初始化日志 - 只初始化一次
        // Initialize logging - only initialize once
        let _ = tracing_subscriber::fmt::try_init();
        
        // 启动测试服务器 - 只启动一次
        let server_url = start_test_server().await;
        
        // 创建第一个MCP管理器
        let manager1 = Arc::new(Mutex::new(MCPServerManager::new()));
        
        // 创建第一个客户端连接
        let client1 = SmcpComputerClient::new(
            &server_url,
            manager1.clone(),
            "test_computer".to_string(),
        ).await?;
        
        // 等待一小段时间确保连接稳定
        tokio::time::sleep(Duration::from_millis(100)).await;
        
        // 加入Office
        let office_id = "test_office_reconnect";
        client1.join_office(office_id).await?;
        
        // 断开第一个客户端
        client1.disconnect().await?;
        
        // 等待一小段时间确保服务器已处理断开连接
        tokio::time::sleep(Duration::from_millis(100)).await;
        
        // 创建新的MCP管理器和客户端重新连接（使用同一个服务器）
        let manager2 = Arc::new(Mutex::new(MCPServerManager::new()));
        let client2 = SmcpComputerClient::new(
            &server_url,
            manager2.clone(),
            "test_computer_reconnected".to_string(),
        ).await?;
        
        // 等待一小段时间确保连接稳定
        tokio::time::sleep(Duration::from_millis(100)).await;
        
        // 重新加入Office
        client2.join_office(office_id).await?;
        
        // 验证连接状态
        let current_office_id = client2.get_office_id().await;
        assert_eq!(current_office_id, Some(office_id.to_string()));
        
        // 断开连接
        client2.disconnect().await?;
        
        Ok(())
    }
}
