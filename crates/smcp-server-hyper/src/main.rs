use tracing::info;
use tracing_subscriber::fmt;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let _ = fmt().with_env_filter("info").try_init();

    // Parse command line arguments
    let args: Vec<String> = std::env::args().collect();
    let addr = if args.len() > 1 {
        args[1].parse()?
    } else {
        "127.0.0.1:3000".parse()?
    };

    info!("Starting SMCP server on {}", addr);

    // Build and run the server
    smcp_server_hyper::run_server(addr).await
}
