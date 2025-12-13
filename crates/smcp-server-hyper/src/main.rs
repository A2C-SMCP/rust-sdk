#[tokio::main]
async fn main() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .try_init();

    let _server = smcp_server_core::SmcpServerBuilder::new()
        .build_layer()
        .expect("failed to build SMCP server layer");

    tracing::info!("smcp-server-hyper built successfully");
}
