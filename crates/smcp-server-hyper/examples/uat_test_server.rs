//! UAT 专用「放行鉴权」SMCP server（**测试 artifact，非生产二进制**）。
//!
//! 用途：让 UAT 完整链路编排（Server + Computer + Agent 三真实进程）能在不配置共享
//! 密钥的情况下真实端到端建立连接。等价于 python-sdk 的
//! `tests/integration_tests/server/_local_sync_server.py` 里的 `_PassSyncAuth`
//! （`authenticate → True`）+ `LocalSyncSMCPNamespace`。
//!
//! 设计约束：**不改任何业务逻辑**——仅用 `smcp-server-core` / `smcp-server-hyper`
//! 已公开的 API（`AuthenticationProvider` trait、`SmcpServerBuilder::with_auth_provider`、
//! `HyperServerBuilder`）组装。版本握手中间件沿用 `HyperServer::run` 的默认配置（启用），
//! 故 4008 版本闸门行为与生产 `smcp-server-hyper` 二进制一致。
//!
//! 用法（与生产二进制相同的位置参数）：
//! ```bash
//! cargo build -p smcp-server-hyper --example uat_test_server
//! ./target/debug/examples/uat_test_server 127.0.0.1:0   # 端口 0 → 随机端口，日志打印实际端口
//! ```

use std::sync::Arc;

use async_trait::async_trait;
use http::HeaderMap;
use smcp_server_core::{AuthError, AuthenticationProvider, SmcpServerBuilder};
use smcp_server_hyper::HyperServerBuilder;
use tracing::info;
use tracing_subscriber::fmt;

/// 放行一切连接的测试鉴权 provider（mirrors python `_PassSyncAuth`）。
///
/// A2C-SMCP 协议本身 auth-agnostic；UAT 关注的是协议流（tool_call / get_resources /
/// skill 渐进披露 / 错误码），鉴权由独立的版本握手与 crate 级单测覆盖。故此处放行，
/// 让完整链路无需密钥即可建立——与 python UAT 的方法学一致。
#[derive(Debug)]
struct PermissiveAuth;

#[async_trait]
impl AuthenticationProvider for PermissiveAuth {
    async fn authenticate(
        &self,
        _headers: &HeaderMap,
        _auth: Option<&serde_json::Value>,
    ) -> Result<(), AuthError> {
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // 默认 info；允许 RUST_LOG 覆盖（UAT 调试时可开 socketioxide=trace 抓 ack 帧）。
    let _ = fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    // 位置参数 1 = 监听地址（与生产二进制一致）；缺省随机端口。
    let args: Vec<String> = std::env::args().collect();
    let addr: std::net::SocketAddr = args
        .get(1)
        .map(String::as_str)
        .unwrap_or("127.0.0.1:0")
        .parse()?;

    info!(
        "Starting UAT permissive SMCP server on {} (auth: permissive)",
        addr
    );

    let layer = SmcpServerBuilder::new()
        .with_auth_provider(Arc::new(PermissiveAuth))
        .build_layer()
        .map_err(|e| format!("Failed to build SMCP layer: {}", e))?;

    let server = HyperServerBuilder::new()
        .with_layer(layer)
        .with_addr(addr)
        .build();

    server.run(addr).await
}
