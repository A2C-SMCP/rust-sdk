/*!
 * 文件名: handshake_config_test
 * 作者: JQQ
 * 创建日期: 2026-04-23
 * 版权: 2023 JQQ. All rights reserved.
 * 描述: 验证 smcp-computer Socket.IO 握手配置化 (TFRM-16) /
 *       Verify Socket.IO handshake configurability for smcp-computer (TFRM-16).
 *
 * 测试策略 / Test strategy:
 *   - 鉴权 header 名相关测试 (1/2/5)：使用轻量 TCP 截包 (accept → 读到 \r\n\r\n → 503)，
 *     直接验证 wire-level HTTP upgrade headers，避免引入额外依赖。
 *   - namespace 相关测试 (3/4)：使用 socketioxide 直接搭建一个挂载到 `/custom_ns`
 *     的 namespace；验证客户端能成功连上自定义 namespace 且 `get_namespace()`
 *     返回真实配置值（而非旧 bug 的字面量 `"/smcp"`）。
 */

#[cfg(test)]
mod tests {
    use http_body_util::Full;
    use hyper::body::Bytes;
    use smcp_computer::errors::ComputerResult;
    use smcp_computer::mcp_clients::manager::MCPServerManager;
    use smcp_computer::mcp_clients::model::MCPServerInput;
    use smcp_computer::socketio_client::{SmcpComputerClient, SmcpComputerClientBuilder};
    use socketioxide::extract::SocketRef;
    use socketioxide::SocketIo;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex as StdMutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::RwLock;
    use tokio::time::{sleep, timeout, Duration};
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

    /// 启动一次性 TCP 服务：抓取首个连接的 HTTP headers 后立即返回 503。
    /// One-shot TCP server: capture the HTTP headers of the first connection and
    /// reply 503 to make the client abort handshake.
    async fn start_capture_server() -> (String, Arc<StdMutex<HashMap<String, String>>>) {
        let captured: Arc<StdMutex<HashMap<String, String>>> =
            Arc::new(StdMutex::new(HashMap::new()));
        let captured_clone = captured.clone();
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind capture listener");
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            // 持续接受连接，直到拿到一份完整 headers 即结束循环。
            // Keep accepting until we capture one complete HTTP headers block.
            let mut got_headers = false;
            while !got_headers {
                let (mut stream, _) = match listener.accept().await {
                    Ok(s) => s,
                    Err(_) => return,
                };
                let mut buf = vec![0u8; 4096];
                let mut acc: Vec<u8> = Vec::new();
                loop {
                    let n = match stream.read(&mut buf).await {
                        Ok(0) => break,
                        Ok(n) => n,
                        Err(_) => break,
                    };
                    acc.extend_from_slice(&buf[..n]);
                    if acc.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                    if acc.len() > 16384 {
                        break;
                    }
                }
                if let Ok(text) = std::str::from_utf8(&acc) {
                    let mut map: HashMap<String, String> = HashMap::new();
                    for line in text.split("\r\n").skip(1) {
                        if line.is_empty() {
                            break;
                        }
                        if let Some((k, v)) = line.split_once(':') {
                            map.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
                        }
                    }
                    if !map.is_empty() {
                        *captured_clone.lock().unwrap() = map;
                        got_headers = true;
                    }
                }
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await;
                let _ = stream.shutdown().await;
            }
        });

        (format!("http://127.0.0.1:{}", port), captured)
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

    // 1) 默认鉴权 header 应为 access_token /
    //    Default auth header key should be `access_token`.
    #[tokio::test]
    async fn test_default_auth_header_is_access_token() {
        let (url, captured) = start_capture_server().await;
        let (manager, inputs) = make_state();

        let _ = timeout(Duration::from_secs(3), async {
            SmcpComputerClientBuilder::new(url, manager, "test".to_string(), inputs)
                .auth_secret("supersecret")
                .connect()
                .await
        })
        .await;

        sleep(Duration::from_millis(100)).await;
        let map = captured.lock().unwrap().clone();
        assert!(
            map.contains_key("access_token"),
            "Default auth header should be 'access_token'. Captured: {:?}",
            map
        );
        assert_eq!(
            map.get("access_token").map(|s| s.as_str()),
            Some("supersecret")
        );
        assert!(
            !map.contains_key("x-api-key"),
            "Default should NOT use deprecated 'x-api-key'. Captured: {:?}",
            map
        );
    }

    // 2) 自定义鉴权 header 名应被实际写入 wire /
    //    Custom auth_header_name() must reach the wire.
    #[tokio::test]
    async fn test_custom_auth_header_name_override() {
        let (url, captured) = start_capture_server().await;
        let (manager, inputs) = make_state();

        let _ = timeout(Duration::from_secs(3), async {
            SmcpComputerClientBuilder::new(url, manager, "test".to_string(), inputs)
                .auth_secret("legacy_value")
                .auth_header_name("x-legacy-key")
                .connect()
                .await
        })
        .await;

        sleep(Duration::from_millis(100)).await;
        let map = captured.lock().unwrap().clone();
        assert_eq!(
            map.get("x-legacy-key").map(|s| s.as_str()),
            Some("legacy_value"),
            "Custom auth header should appear on the wire. Captured: {:?}",
            map
        );
        assert!(
            !map.contains_key("access_token"),
            "Default key should NOT be sent when overridden. Captured: {:?}",
            map
        );
    }

    // 3) 自定义 namespace 必须真实参与 Socket.IO 握手 /
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

    // 4) get_namespace() 必须返回用户配置值，而非字面量 "/smcp" /
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

    // 5) 兼容入口 SmcpComputerClient::new(..) 默认仍走新默认值 access_token /
    //    Backward-compat new() must use the new default `access_token`.
    #[tokio::test]
    async fn test_backward_compat_new_uses_access_token_default() {
        let (url, captured) = start_capture_server().await;
        let (manager, inputs) = make_state();

        let _ = timeout(Duration::from_secs(3), async {
            SmcpComputerClient::new(
                &url,
                manager,
                "test".to_string(),
                Some("compat_value".to_string()),
                inputs,
                None,
            )
            .await
        })
        .await;

        sleep(Duration::from_millis(100)).await;
        let map = captured.lock().unwrap().clone();
        assert_eq!(
            map.get("access_token").map(|s| s.as_str()),
            Some("compat_value"),
            "Backward-compat new() should default to 'access_token'. Captured: {:?}",
            map
        );
    }
}
