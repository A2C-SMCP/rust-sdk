/*!
 * HS-02 #22 客户端版本握手集成测试 / Client-side version handshake integration test。
 *
 * 与 HS-01（smcp-server-hyper 版本握手中间件）三态联调：启动一台**伪装成一个必然不兼容的版本（MAJOR+1）** 的 HyperServer，
 * 客户端（smcp-agent `SocketIoTransport`）发送 [`smcp::PROTOCOL_VERSION`]。v0.x MINOR
 * 严格匹配判定不兼容 → 服务端在 polling 握手返回 HTTP 400 + 4008 body → 客户端映射为强类型
 * [`smcp_agent::SmcpAgentError::ProtocolVersionMismatch`]（非 panic），且诊断字段逐字段携带。
 *
 * Drives the real smcp-agent client code path (URL a2c_version 注入 + polling-first +
 * connect-error 分类)，覆盖 issue #22 验收的「4008 → 受控错误」与「与 HS-01 三态联调」。
 */

use std::collections::HashMap;
use std::time::Duration;

use smcp::version::ProtocolVersion;
use smcp_agent::{SmcpAgentError, SocketIoTransport};
use smcp_server_core::{DefaultAuthenticationProvider, SmcpServerBuilder};
use smcp_server_hyper::{HyperServerBuilder, VersionHandshakeConfig};
use tokio::time::sleep;

/// 服务端伪装一个必然不兼容的版本（MAJOR+1），客户端发 PROTOCOL_VERSION → 客户端必须收到 ProtocolVersionMismatch 强类型错误。
#[tokio::test]
async fn test_client_maps_incompatible_version_to_typed_error() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();

    let layer = SmcpServerBuilder::new()
        .with_auth_provider(std::sync::Arc::new(DefaultAuthenticationProvider::new(
            Some("test_secret".to_string()),
            None,
        )))
        .build_layer()
        .expect("failed to build SMCP server layer");

    // 选取可用端口 / Pick a free port.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let server_addr = format!("127.0.0.1:{}", port).parse().unwrap();

    // 从 PROTOCOL_VERSION 派生一个**必然不兼容**的服务端版本（MAJOR+1 → v0.x/v1+ 判定均不兼容），使测试
    // 与协议版本具体值解耦：未来 bump PROTOCOL_VERSION 时本测试无需改。
    let client_v = ProtocolVersion::parse(smcp::PROTOCOL_VERSION).expect("PROTOCOL_VERSION 合法");
    let server_v = ProtocolVersion::new(client_v.major + 1, client_v.minor, client_v.patch);
    let server = HyperServerBuilder::new()
        .with_layer(layer)
        .with_addr(server_addr)
        .with_version_handshake(VersionHandshakeConfig {
            server_version: server_v,
            ..Default::default()
        })
        .build();

    let server_handle = tokio::spawn(async move { server.run(server_addr).await });
    sleep(Duration::from_millis(150)).await;

    // 客户端走真实 smcp-agent 代码路径：build_handshake_url 注入 PROTOCOL_VERSION + polling-first + 分类。
    let url = format!("http://127.0.0.1:{}/", port);
    // #86：鉴权走 Socket.IO CONNECT auth dict（字段 `token`，对齐 server 默认）；headers 仅路由。
    let result = SocketIoTransport::connect(
        &url,
        "/smcp",
        Some(serde_json::json!({ "token": "test_secret" })),
        HashMap::new(),
    )
    .await;

    match result {
        Err(SmcpAgentError::ProtocolVersionMismatch(e)) => {
            // 诊断字段全部从 server_v 派生（HS-01 _mismatch_body：min/max = server 的 MAJOR.MINOR.{0,999}），
            // 不与具体版本字面量耦合。
            let server_str = server_v.to_string();
            let min = format!("{}.{}.0", server_v.major, server_v.minor);
            let max = format!("{}.{}.999", server_v.major, server_v.minor);
            assert_eq!(e.server_version.as_deref(), Some(server_str.as_str()));
            assert_eq!(
                e.client_version.as_deref(),
                Some(smcp::PROTOCOL_VERSION),
                "client_version 应回显 SDK PROTOCOL_VERSION"
            );
            assert_eq!(e.min_supported.as_deref(), Some(min.as_str()));
            assert_eq!(e.max_supported.as_deref(), Some(max.as_str()));
        }
        Err(other) => panic!("expected ProtocolVersionMismatch, got: {other:?}"),
        Ok(_) => panic!("expected connection to be rejected on version mismatch, but it succeeded"),
    }

    server_handle.abort();
}

/// 反向对照：服务端默认 PROTOCOL_VERSION，客户端发 PROTOCOL_VERSION → 连接成功（transport=Any/polling-first 不破坏兼容路径）。
#[tokio::test]
async fn test_client_compatible_version_connects() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();

    let layer = SmcpServerBuilder::new()
        .with_auth_provider(std::sync::Arc::new(DefaultAuthenticationProvider::new(
            Some("test_secret".to_string()),
            None,
        )))
        .build_layer()
        .expect("failed to build SMCP server layer");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let server_addr = format!("127.0.0.1:{}", port).parse().unwrap();

    // 默认版本握手配置 = server_version 取 PROTOCOL_VERSION，与客户端一致。
    let server = HyperServerBuilder::new()
        .with_layer(layer)
        .with_addr(server_addr)
        .build();

    let server_handle = tokio::spawn(async move { server.run(server_addr).await });
    sleep(Duration::from_millis(150)).await;

    let url = format!("http://127.0.0.1:{}/", port);
    // #86：鉴权走 Socket.IO CONNECT auth dict（字段 `token`，对齐 server 默认）；headers 仅路由。
    let result = SocketIoTransport::connect(
        &url,
        "/smcp",
        Some(serde_json::json!({ "token": "test_secret" })),
        HashMap::new(),
    )
    .await;
    assert!(
        result.is_ok(),
        "compatible client (== PROTOCOL_VERSION) should connect via polling-first; got: {:?}",
        result.err()
    );

    if let Ok((transport, _rx)) = result {
        let _ = transport.disconnect().await;
    }
    server_handle.abort();
}
