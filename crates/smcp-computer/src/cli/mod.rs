/*!
* 文件名: mod.rs
* 作者: JQQ
* 创建日期: 2025/12/16
* 最后修改日期: 2025/12/16
* 版权: 2023 JQQ. All rights reserved.
* 依赖: clap, tokio, console, rustyline
* 描述: CLI模块的入口 / CLI module entry point
*/

use crate::computer::{Computer, SilentSession};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

pub mod commands;
mod interactive;
mod utils;

use commands::CommandHandler;

/// A2C-SMCP Computer CLI - 计算机客户端命令行工具
#[derive(Parser, Debug)]
#[command(name = "smcp-computer")]
#[command(about = "A2C-SMCP Computer CLI - 计算机客户端命令行工具", long_about = None)]
#[command(version = env!("CARGO_PKG_VERSION"))]
pub struct Args {
    /// 是否自动连接 / Auto connect
    #[arg(long, default_value = "true")]
    pub auto_connect: bool,

    /// 是否自动重连 / Auto reconnect
    #[arg(long, default_value = "true")]
    pub auto_reconnect: bool,

    /// Socket.IO 服务器URL，例如 https://host:port
    #[arg(long)]
    pub url: Option<String>,

    /// Socket.IO 命名空间（默认: /smcp）
    #[arg(long, default_value = "/smcp")]
    pub namespace: String,

    /// 认证参数，形如 key:value,foo:bar
    #[arg(long)]
    pub auth: Option<String>,

    /// 请求头参数，形如 key:value,foo:bar
    #[arg(long)]
    pub headers: Option<String>,

    /// 关闭彩色输出
    #[arg(long)]
    pub no_color: bool,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// 启动计算机并进入持续运行模式
    Run {
        /// 在启动时从文件加载 MCP Servers 配置
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// 在启动时从文件加载 Inputs 定义
        #[arg(short, long)]
        inputs: Option<PathBuf>,
    },
}

pub fn main() {
    let args = Args::parse();

    // 设置颜色控制
    if args.no_color {
        console::set_colors_enabled(false);
    }

    // 创建 tokio runtime
    let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");

    rt.block_on(async {
        // Extract needed fields before the match to avoid borrow issues
        let auto_connect = args.auto_connect;
        let auto_reconnect = args.auto_reconnect;
        let url = args.url.clone();
        let namespace = args.namespace.clone();
        let auth = args.auth.clone();
        let headers = args.headers.clone();

        if let Some(ref command) = args.command {
            match command {
                Commands::Run { config, inputs } => {
                    let cli_config = CliConfig {
                        auto_connect,
                        auto_reconnect,
                        url,
                        namespace,
                        auth,
                        headers,
                        config: config.clone(),
                        inputs: inputs.clone(),
                    };
                    run_command(cli_config).await;
                }
            }
        } else {
            // 默认执行 run 命令
            let cli_config = CliConfig {
                auto_connect,
                auto_reconnect,
                url,
                namespace,
                auth,
                headers,
                config: None,
                inputs: None,
            };
            run_command(cli_config).await;
        }
    });
}

/// CLI 运行时配置 / CLI runtime configuration
struct CliConfig {
    auto_connect: bool,
    auto_reconnect: bool,
    url: Option<String>,
    namespace: String,
    auth: Option<String>,
    headers: Option<String>,
    config: Option<PathBuf>,
    inputs: Option<PathBuf>,
}

async fn run_command(config: CliConfig) {
    // 创建 Computer 实例
    let session = SilentSession::new("cli-session");
    let computer = Computer::new(
        "friday_hands",
        session,
        None, // inputs
        None, // mcp_servers
        config.auto_connect,
        config.auto_reconnect,
    );

    // 创建命令处理器
    let cli_config_for_handler = commands::CliConfig {
        url: config.url.clone(),
        namespace: config.namespace.clone(),
        auth: config.auth.clone(),
        headers: config.headers.clone(),
    };
    let mut handler = CommandHandler::new(computer, cli_config_for_handler);

    // 加载配置
    if let Some(inputs_path) = config.inputs {
        if let Err(e) = handler.load_inputs(&inputs_path).await {
            eprintln!("加载 inputs 失败: {}", e);
        }
    }

    if let Some(config_path) = config.config {
        if let Err(e) = handler.load_config(&config_path).await {
            eprintln!("加载 config 失败: {}", e);
        }
    }

    // 连接 SocketIO（如果提供了 URL）
    if let Some(url) = &config.url {
        if let Err(e) = handler
            .connect_socketio(url, &config.namespace, &config.auth, &config.headers)
            .await
        {
            eprintln!("连接 SocketIO 失败: {}", e);
        }
    }

    // 进入交互模式
    if let Err(e) = interactive::run_interactive_loop(handler).await {
        eprintln!("交互模式错误: {}", e);
    }
}
