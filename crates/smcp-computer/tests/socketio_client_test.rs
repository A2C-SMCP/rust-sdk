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
    use std::sync::Arc;
    use tokio::sync::Mutex;
    use tokio::time::{sleep, Duration};
    use tracing::info;
    use std::net::SocketAddr;
    use http_body_util::Full;
    use hyper::body::Bytes;

    /// 启动测试服务器
    async fn start_test_server() -> String {
        // 构建SMCP服务器层
        // Build SMCP server layer
        let layer = SmcpServerBuilder::new()
            .with_default_auth(None, None)
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
            
            // 构建服务栈
            // Build service stack
            let service = tower::ServiceBuilder::new()
                .layer(layer_clone.layer)
                .service(hyper::service::service_fn(move |_req| {
                    let _io = layer_clone.io.clone();
                    async move { Ok::<_, hyper::Error>(hyper::Response::new(Full::<Bytes>::from(""))) }
                }));
            
            // 处理连接
            // Handle connections
            loop {
                let (stream, _) = listener.accept().await.expect("Failed to accept");
                let service = service.clone();
                tokio::spawn(async move {
                    let stream = hyper_util::rt::TokioIo::new(stream);
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(stream, service)
                        .await;
                });
            }
        });
        
        // 等待获取实际端口
        // Wait for actual port
        let port = rx.await.expect("Failed to receive port");
        
        format!("http://127.0.0.1:{}", port)
    }

    #[tokio::test]
    async fn test_socketio_client_connection() -> ComputerResult<()> {
        // 初始化日志
        tracing_subscriber::fmt::init();
        
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
        
        // 测试连接状态
        let office_id = client.get_office_id().await;
        assert!(office_id.is_none(), "Office ID should be None initially");
        
        // 断开连接
        client.disconnect().await?;
        
        Ok(())
    }

    #[tokio::test]
    async fn test_join_and_leave_office() -> ComputerResult<()> {
        // 初始化日志
        tracing_subscriber::fmt::init();
        
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
        // 初始化日志
        tracing_subscriber::fmt::init();
        
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
        // 初始化日志
        tracing_subscriber::fmt::init();
        
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
        
        // 尝试加入不存在的Office（应该失败）
        let result = client.join_office("").await;
        assert!(result.is_err(), "Should fail to join empty office");
        
        // 尝试离开未加入的Office（应该成功但不报错）
        client.leave_office("nonexistent").await?;
        
        // 断开连接
        client.disconnect().await?;
        
        Ok(())
    }

    #[tokio::test]
    async fn test_multiple_clients() -> ComputerResult<()> {
        // 初始化日志
        tracing_subscriber::fmt::init();
        
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
        // 初始化日志
        tracing_subscriber::fmt::init();
        
        // 启动测试服务器
        let server_url = start_test_server().await;
        
        // 创建MCP管理器
        let manager = Arc::new(Mutex::new(MCPServerManager::new()));
        
        // 创建第一个客户端连接
        let client1 = SmcpComputerClient::new(
            &server_url,
            manager.clone(),
            "test_computer".to_string(),
        ).await?;
        
        // 加入Office
        let office_id = "test_office_reconnect";
        client1.join_office(office_id).await?;
        
        // 断开第一个客户端
        client1.disconnect().await?;
        
        // 创建新客户端重新连接
        let client2 = SmcpComputerClient::new(
            &server_url,
            manager.clone(),
            "test_computer".to_string(),
        ).await?;
        
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
