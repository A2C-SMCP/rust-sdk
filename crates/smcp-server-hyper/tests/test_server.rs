/**
* 文件名: test_server
* 作者: JQQ
* 创建日期: 2025/9/10
* 最后修改日期: 2025/9/10
* 版权: 2023 JQQ. All rights reserved.
* 依赖: None
* 描述: Test server implementation for integration tests
*/

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use http_body_util::Full;
use hyper::body::Bytes;
use hyper_util::rt::TokioIo;
use smcp_server_core::{
    auth::DefaultAuthenticationProvider,
    SmcpServerBuilder,
};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::time::sleep;
use tower::{Layer, Service};

/// 测试服务器配置 - 使用 SmcpTestServer 的实现
pub struct TestServer {
    pub addr: SocketAddr,
    shutdown_tx: Option<oneshot::Sender<()>>,
}

impl TestServer {
    pub async fn new() -> Self {
        // 使用与 smcp-server-core 相同的实现
        let port = find_available_port().await;
        let addr: SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();

        let layer = SmcpServerBuilder::new()
            .with_auth_provider(Arc::new(DefaultAuthenticationProvider::new(
                Some("test_secret".to_string()),
                None,
            )))
            .build_layer()
            .expect("failed to build SMCP server layer");

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let listener = TcpListener::bind(addr).await.unwrap();
        let actual_addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let mut shutdown_rx = shutdown_rx;
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        if let Ok((stream, _)) = result {
                            let io = TokioIo::new(stream);
                            let layer = layer.clone();

                            tokio::spawn(async move {
                                let svc = tower::service_fn(|req| {
                                    let layer = layer.clone();
                                    async move {
                                        let svc = tower::service_fn(|req: hyper::Request<hyper::body::Incoming>| async move {
                                            // 处理HTTP请求
                                            match (req.method(), req.uri().path()) {
                                                (&hyper::Method::GET, "/") => {
                                                    Ok::<_, std::convert::Infallible>(
                                                        hyper::Response::builder()
                                                            .status(hyper::StatusCode::OK)
                                                            .body(Full::new(hyper::body::Bytes::from("SMCP Server is running")))
                                                            .unwrap()
                                                    )
                                                }
                                                (&hyper::Method::GET, "/health") => {
                                                    Ok::<_, std::convert::Infallible>(
                                                        hyper::Response::builder()
                                                            .status(hyper::StatusCode::OK)
                                                            .header("content-type", "application/json")
                                                            .body(Full::new(hyper::body::Bytes::from("{\"status\":\"ok\"}")))
                                                            .unwrap()
                                                    )
                                                }
                                                _ => {
                                                    // 默认返回404
                                                    Ok::<_, std::convert::Infallible>(
                                                        hyper::Response::builder()
                                                            .status(hyper::StatusCode::NOT_FOUND)
                                                            .body(Full::new(hyper::body::Bytes::from("Not found")))
                                                            .unwrap()
                                                    )
                                                }
                                            }
                                        });
                                        let mut svc = layer.layer.layer(svc);
                                        svc.call(req).await
                                    }
                                });

                                let svc = hyper_util::service::TowerToHyperService::new(svc);
                                let _ = hyper::server::conn::http1::Builder::new()
                                    .serve_connection(io, svc)
                                    .with_upgrades()
                                    .await;
                            });
                        }
                    }
                    _ = &mut shutdown_rx => {
                        break;
                    }
                }
            }
        });

        // 等待服务器启动
        sleep(Duration::from_millis(100)).await;

        TestServer {
            addr: actual_addr,
            shutdown_tx: Some(shutdown_tx),
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

async fn find_available_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap().port()
}
