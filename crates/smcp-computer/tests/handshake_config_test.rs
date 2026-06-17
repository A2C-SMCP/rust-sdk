/*!
 * 文件名: handshake_config_test
 * 作者: JQQ
 * 创建日期: 2026-04-23
 * 版权: 2023 JQQ. All rights reserved.
 * 描述: 验证 smcp-computer Socket.IO 握手配置化 (TFRM-16) /
 *       Verify Socket.IO handshake configurability for smcp-computer (TFRM-16).
 *
 * 测试策略 / Test strategy:
 *   - namespace 相关测试：使用 socketioxide 直接搭建一个挂载到 `/custom_ns` 的 namespace；
 *     验证客户端能成功连上自定义 namespace 且 `get_namespace()` 返回真实配置值（而非旧 bug
 *     的字面量 `"/smcp"`）。
 *   - 连接面鉴权（#86 起统一走 Socket.IO auth dict）的 on-wire 验证见 `auth_dict_injection_test`；
 *     HTTP-header 鉴权已退役，相关 wire-header 测试随之移除。
 */

#[cfg(test)]
mod tests {
    use http_body_util::Full;
    use hyper::body::Bytes;
    use smcp_computer::errors::ComputerResult;
    use smcp_computer::mcp_clients::manager::MCPServerManager;
    use smcp_computer::mcp_clients::model::MCPServerInput;
    use smcp_computer::socketio_client::SmcpComputerClientBuilder;
    use socketioxide::extract::SocketRef;
    use socketioxide::SocketIo;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::net::TcpListener;
    use tokio::sync::RwLock;
    use tokio::time::{sleep, Duration};
    use tower::service_fn;
    use tower::Layer;

    type State = (
        Arc<RwLock<Option<MCPServerManager>>>,
        Arc<RwLock<HashMap<String, MCPServerInput>>>,
    );

    fn make_state() -> State {
        (
            Arc::new(RwLock::new(Some(MCPServerManager::new()))),
            Arc::new(RwLock::new(HashMap::new())),
        )
    }

    /// 启动一个挂载到指定 namespace 的最小 socketioxide 服务。
    /// Start a minimal socketioxide server mounted on the given namespace.
    async fn start_socketio_server(namespace: &'static str) -> String {
        let (svc_layer, io) = SocketIo::new_layer();
        io.ns(namespace, |_s: SocketRef| {
            // 仅接受连接，不做协议处理。
            // Accept connection only, no protocol handling.
        });

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind socketio listener");
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            let fallback = service_fn(|_req: hyper::Request<hyper::body::Incoming>| async {
                Ok::<_, std::convert::Infallible>(
                    hyper::Response::builder()
                        .status(hyper::StatusCode::NOT_FOUND)
                        .body(Full::<Bytes>::from(""))
                        .unwrap(),
                )
            });
            let service = svc_layer.layer(fallback);
            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(s) => s,
                    Err(_) => return,
                };
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

        // 等服务启动 / Wait for server to be ready
        sleep(Duration::from_millis(100)).await;
        format!("http://127.0.0.1:{}", port)
    }

    // 1) 自定义 namespace 必须真实参与 Socket.IO 握手 /
    //    Custom namespace must be honored during the Socket.IO handshake.
    #[tokio::test]
    async fn test_custom_namespace_propagates() -> ComputerResult<()> {
        let server_url = start_socketio_server("/custom_ns").await;
        let (manager, inputs) = make_state();

        let client =
            SmcpComputerClientBuilder::new(server_url, manager, "test".to_string(), inputs)
                .namespace("/custom_ns")
                .connect()
                .await?;

        // 若 namespace 未生效，连接将连不上自定义 ns，后续 disconnect 也异常。
        // If namespace wasn't honored, connect would fail or hang.
        client.disconnect().await?;
        Ok(())
    }

    // 2) get_namespace() 必须返回用户配置值，而非字面量 "/smcp" /
    //    get_namespace() must reflect the configured value, not the hardcoded literal.
    #[tokio::test]
    async fn test_get_namespace_reflects_configured_value() -> ComputerResult<()> {
        let server_url = start_socketio_server("/custom_ns").await;
        let (manager, inputs) = make_state();

        let client =
            SmcpComputerClientBuilder::new(server_url, manager, "test".to_string(), inputs)
                .namespace("/custom_ns")
                .connect()
                .await?;

        assert_eq!(
            client.get_namespace(),
            "/custom_ns",
            "get_namespace() must return the configured value (regression: old code returned literal '/smcp')"
        );

        client.disconnect().await?;
        Ok(())
    }
}
