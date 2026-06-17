/*!
 * #86 连接面鉴权（auth-dict-only）端到端接线测试 / End-to-end wiring test for connection auth.
 *
 * 锁死 #86 的核心修复：server `on_connect` 经 socketioxide `TryData<Value>` 提取器拿 Socket.IO
 * CONNECT `auth` dict 并转发给 `AuthenticationProvider::authenticate`（此前从 `extensions` 取、无人
 * 写入 → 恒 None，鉴权被旁路）。用一个**记录型** provider 断言 authenticate 实际收到了客户端发的
 * auth dict；并验证错误 token 被**拒绝**（#86 起 on_connect 失败会主动 disconnect，而非半开旁路）。
 */

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use http::HeaderMap;
use http_body_util::Full;
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use tf_rust_socketio::asynchronous::ClientBuilder;
use tf_rust_socketio::TransportType;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::time::sleep;
use tower::Layer;

use smcp_server_core::{
    AuthError, AuthenticationProvider, DefaultAuthenticationProvider, SmcpServerBuilder,
};

/// 记录 `authenticate` 实际收到的 auth dict，并按 `accept` 决定放行/拒绝。
#[derive(Debug)]
struct RecordingAuthProvider {
    captured: Arc<Mutex<Option<Value>>>,
    accept: bool,
}

#[async_trait]
impl AuthenticationProvider for RecordingAuthProvider {
    async fn authenticate(
        &self,
        _headers: &HeaderMap,
        auth: Option<&Value>,
    ) -> Result<(), AuthError> {
        *self.captured.lock().unwrap() = auth.cloned();
        if self.accept {
            Ok(())
        } else {
            Err(AuthError::InvalidApiKey)
        }
    }
}

/// 启动带指定 auth provider 的最小 SMCP server（layer 只叠一次 clone-per-connection）。
async fn start_server_with(
    provider: Arc<dyn AuthenticationProvider>,
) -> (String, oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();

    let layer = SmcpServerBuilder::new()
        .with_auth_provider(provider)
        .build_layer()
        .expect("build SMCP layer");

    let fallback = tower::service_fn(|_req: hyper::Request<hyper::body::Incoming>| async move {
        Ok::<_, std::convert::Infallible>(hyper::Response::new(
            Full::new(hyper::body::Bytes::new()),
        ))
    });
    let service = layer.layer.layer(fallback);

    let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    if let Ok((stream, _)) = accepted {
                        let io = TokioIo::new(stream);
                        let svc = hyper_util::service::TowerToHyperService::new(service.clone());
                        tokio::spawn(async move {
                            let _ = hyper::server::conn::http1::Builder::new()
                                .serve_connection(io, svc)
                                .with_upgrades()
                                .await;
                        });
                    }
                }
                _ = &mut shutdown_rx => break,
            }
        }
    });

    sleep(Duration::from_millis(120)).await;
    (format!("http://{addr}"), shutdown_tx)
}

/// #86 验收：server `on_connect` 经 `TryData<Value>` 把客户端 CONNECT `auth` dict 原样交给
/// `authenticate`——记录型 provider 断言收到的就是客户端发的 `{"token": ...}`（锁死「不再丢弃 auth dict」）。
#[tokio::test]
async fn server_reads_token_from_connect_auth_dict() {
    let captured = Arc::new(Mutex::new(None));
    let provider = Arc::new(RecordingAuthProvider {
        captured: captured.clone(),
        accept: true,
    });
    let (url, shutdown) = start_server_with(provider).await;

    let client = ClientBuilder::new(url)
        .transport_type(TransportType::Websocket)
        .namespace("/smcp")
        .auth(json!({ "token": "wired-secret" }))
        .connect()
        .await
        .expect("connect should succeed when provider accepts");

    // 轮询等 on_connect → authenticate 跑完。
    for _ in 0..100 {
        if captured.lock().unwrap().is_some() {
            break;
        }
        sleep(Duration::from_millis(20)).await;
    }

    assert_eq!(
        *captured.lock().unwrap(),
        Some(json!({ "token": "wired-secret" })),
        "server must hand the client's CONNECT auth dict to authenticate (not drop it)"
    );

    let _ = client.disconnect().await;
    let _ = shutdown.send(());
}

/// #86：错误 token 必须被拒绝。验证 (a) authenticate 确实收到了（错误的）auth dict——证明 reject 路径
/// 同样经过真实接线（而非客户端连不上才"过"）；(b) 真实 [`DefaultAuthenticationProvider`] 下错误 token
/// 不放行（与 auth.rs 单测的拒绝逻辑端到端一致）。
#[tokio::test]
async fn server_rejects_wrong_token() {
    // (a) 记录型 provider（reject）：错误 auth dict 仍被原样交给 authenticate。
    let captured = Arc::new(Mutex::new(None));
    let provider = Arc::new(RecordingAuthProvider {
        captured: captured.clone(),
        accept: false,
    });
    let (url, shutdown) = start_server_with(provider).await;

    // on_connect 失败 → server 主动 disconnect。裸客户端的 connect() 可能返回 Err（被拒），
    // 也可能短暂 Ok 后被断开——两者都可接受；本测试的硬断言落在「authenticate 收到了 auth dict」。
    let _ = ClientBuilder::new(url)
        .transport_type(TransportType::Websocket)
        .namespace("/smcp")
        .auth(json!({ "token": "wrong-secret" }))
        .connect()
        .await;

    for _ in 0..100 {
        if captured.lock().unwrap().is_some() {
            break;
        }
        sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        *captured.lock().unwrap(),
        Some(json!({ "token": "wrong-secret" })),
        "authenticate must receive the (wrong) auth dict on the reject path too"
    );
    let _ = shutdown.send(());

    // (b) 真实 DefaultAuthenticationProvider：错误 token 不放行（孤立判定，端到端字段名 `token` 对齐）。
    let provider = Arc::new(DefaultAuthenticationProvider::new(
        Some("test_secret".to_string()),
        None,
    ));
    assert!(
        provider
            .authenticate(&HeaderMap::new(), Some(&json!({ "token": "wrong-secret" })))
            .await
            .is_err(),
        "DefaultAuthenticationProvider must reject a wrong token in the `token` field"
    );
    assert!(
        provider
            .authenticate(&HeaderMap::new(), Some(&json!({ "token": "test_secret" })))
            .await
            .is_ok(),
        "DefaultAuthenticationProvider must accept the correct token"
    );
}
