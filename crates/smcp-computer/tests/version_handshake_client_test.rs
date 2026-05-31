/*!
 * HS-02 #22 Computer 客户端版本握手集成测试 / Computer client version-handshake integration test。
 *
 * 与 HS-01（smcp-server-hyper 版本握手中间件）联调：启动一台**伪装成 0.3.0** 的 HyperServer，
 * Computer 客户端（`SmcpComputerClient`）发送 [`smcp::PROTOCOL_VERSION`]（`0.2.0`）。v0.x MINOR
 * 严格匹配判定不兼容 → 服务端在 polling 握手返回 HTTP 400 + 4008 body → Computer 经共享
 * `smcp_client_transport::connect_and_classify` 映射为强类型
 * [`ComputerError::ProtocolVersionMismatch`]（非 panic），诊断字段逐字段携带，error_code == 4008。
 *
 * 驱动真实 Computer 连接路径（`SmcpComputerClient::new` → `connect_internal` →
 * `connect_and_classify`），覆盖 #22 验收的「4008 → 受控错误 → ComputerError 映射」。
 * 版本不匹配发生在 HTTP 握手阶段（早于 namespace/manager 业务逻辑），故用一个最小的空 manager 即可。
 */

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use smcp::version::ProtocolVersion;
use smcp_computer::errors::ComputerError;
use smcp_computer::mcp_clients::manager::MCPServerManager;
use smcp_computer::socketio_client::SmcpComputerClient;

use smcp_server_core::{DefaultAuthenticationProvider, SmcpServerBuilder};
use smcp_server_hyper::{HyperServerBuilder, VersionHandshakeConfig};

use tokio::sync::RwLock;
use tokio::time::sleep;

/// 服务端伪装 0.3.0，Computer 发 0.2.0 → 必须收到 ProtocolVersionMismatch 强类型错误（error_code 4008）。
#[tokio::test]
async fn test_computer_maps_incompatible_version_to_typed_error() {
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

    // 服务端伪装成 0.3.0（HS-01 版本握手中间件；其余配置取默认：启用 + 严格 a2c_version）。
    let server = HyperServerBuilder::new()
        .with_layer(layer)
        .with_addr(server_addr)
        .with_version_handshake(VersionHandshakeConfig {
            server_version: ProtocolVersion::new(0, 3, 0),
            ..Default::default()
        })
        .build();

    let server_handle = tokio::spawn(async move { server.run(server_addr).await });
    sleep(Duration::from_millis(150)).await;

    // Computer 走真实连接路径：connect_internal 注入 0.2.0 + polling-first + connect_and_classify。
    // 版本不匹配发生在 HTTP 握手阶段，故用最小空 manager（None）即可。
    let url = format!("http://127.0.0.1:{}/", port);
    let manager: Arc<RwLock<Option<MCPServerManager>>> = Arc::new(RwLock::new(None));
    let inputs = Arc::new(RwLock::new(HashMap::new()));
    let mut headers = HashMap::new();
    headers.insert("access_token".to_string(), "test_secret".to_string());

    let result = SmcpComputerClient::new(
        &url,
        manager,
        "test-computer".to_string(),
        Some("test_secret".to_string()),
        inputs,
        Some(headers),
    )
    .await;

    match result {
        Err(ComputerError::ProtocolVersionMismatch(e)) => {
            // 诊断字段逐字段携带（HS-01 _mismatch_body 派生：server 0.3.0 → min/max 0.3.x）。
            assert_eq!(
                e.server_version.as_deref(),
                Some("0.3.0"),
                "server_version 应为 0.3.0"
            );
            assert_eq!(
                e.client_version.as_deref(),
                Some(smcp::PROTOCOL_VERSION),
                "client_version 应为 SDK PROTOCOL_VERSION (0.2.0)"
            );
            // error_code 必须是 4008（ProtocolVersionMismatch）。
            let err = ComputerError::ProtocolVersionMismatch(e);
            assert_eq!(err.error_code(), 4008, "error_code 应为 4008");
        }
        Err(other) => panic!("expected ProtocolVersionMismatch, got: {other:?}"),
        Ok(_) => panic!("expected connection to be rejected on version mismatch, but it succeeded"),
    }

    server_handle.abort();
}
