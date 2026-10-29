// Enable the cli feature for tests
#![cfg(feature = "cli")]
#![allow(dead_code)]

/// protocol#64：隔离的 user-config 环境（XDG-first），测试绝不读写真实 `$HOME`。
/// 后续统一抽共享 helper（本批未含）；`#[allow(dead_code)]` 兼容部分 feature 组合下用例被 cfg 掉的情况。
#[allow(dead_code)]
fn isolated_config_env(
    base: impl AsRef<std::path::Path>,
) -> std::collections::HashMap<String, String> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    // 每次调用分配独立子目录：同一测试二进制内的多个 Computer、以及共享 temp_dir 的 e2e
    // 用例都不会复用同一身份（PR3 记录/回显身份后“两台电脑”前提仍然成立）。
    let base = base.as_ref().join(format!(
        "smcp-identity-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let mut env = std::collections::HashMap::new();
    env.insert("HOME".to_string(), base.display().to_string());
    env.insert(
        "XDG_CONFIG_HOME".to_string(),
        base.join("xdg-config").display().to_string(),
    );
    env
}

use serde_json::json;
/**
* 文件名: mod.rs
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/16
* 版权: 2023 JQQ. All rights reserved.
* 依赖: tokio, serde_json, tempfile
* 描述: 集成测试公共工具 / Integration test common utilities
*/
use std::collections::HashMap;
use std::io::Write;
use tempfile::{NamedTempFile, TempDir};

use smcp_computer::cli::commands::{CliConfig, CommandHandler};
use smcp_computer::computer::{Computer, SilentSession};
use smcp_computer::mcp_clients::model::{
    MCPServerConfig, MCPServerInput, PromptStringInput, StdioServerConfig, StdioServerParameters,
};

/// 带超时的测试辅助宏 / Test helper macro with timeout
#[macro_export]
macro_rules! with_timeout {
    ($future:expr) => {
        timeout(std::time::Duration::from_secs(30), $future)
            .await
            .expect("Test timed out")
    };
}

/// 创建测试用的临时目录 / Create test temporary directory
pub async fn create_test_temp_dir() -> TempDir {
    TempDir::new().expect("Failed to create temp dir")
}

/// 创建测试用的 Computer 实例 / Create test Computer instance
pub async fn create_test_computer_with_servers() -> Computer<SilentSession> {
    let mut servers = HashMap::new();

    // 添加测试服务器配置 / Add test server config
    servers.insert(
        "test_server".to_string(),
        MCPServerConfig::Stdio(StdioServerConfig::new(
            "test_server",
            StdioServerParameters {
                command: "echo".to_string(),
                args: vec!["hello".to_string()],
                env: HashMap::new(),
                cwd: None,
            },
        )),
    );

    let mut inputs = HashMap::new();

    // 添加测试 input / Add test input
    inputs.insert(
        "test_input".to_string(),
        MCPServerInput::PromptString(PromptStringInput {
            id: "test_input".to_string(),
            description: "Test input".to_string(),
            default: Some("default_value".to_string()),
            password: Some(false),
        }),
    );

    // #113 S6：add/remove_server 落盘 → 定向隔离临时目录（forget 保留至进程退出），避免污染仓库工作树。
    let tmp = TempDir::new().unwrap();
    let config_dir = tmp.path().to_path_buf();
    std::mem::forget(tmp);
    Computer::new(
        "test_computer",
        SilentSession::new("test_session"),
        Some(inputs),
        Some(servers),
        false,
        false,
    )
    .with_config_dir(&config_dir)
    .with_config_env(isolated_config_env(&config_dir))
}

/// 创建测试服务器配置文件 / Create test server config file
pub fn create_test_server_config_file() -> NamedTempFile {
    let mut file = NamedTempFile::new().expect("Failed to create temp file");

    let config = json!({
        "type": "stdio",
        "name": "file_test_server",
        "disabled": false,
        "forbidden_tools": [],
        "tool_meta": {},
        "default_tool_meta": null,
        "vrl": null,
        "server_parameters": {
            "command": "echo",
            "args": ["from_file"],
            "env": {},
            "cwd": null
        }
    });

    writeln!(file, "{}", config).expect("Failed to write to temp file");
    file
}

/// 创建测试 inputs 配置文件 / Create test inputs config file
pub fn create_test_inputs_config_file() -> NamedTempFile {
    let mut file = NamedTempFile::new().expect("Failed to create temp file");

    let inputs = json!([
        {
            "type": "prompt_string",
            "id": "file_test_input",
            "description": "Test input from file",
            "default": "file_default",
            "password": false
        }
    ]);

    writeln!(file, "{}", inputs).expect("Failed to write to temp file");
    file
}

/// 创建完整测试配置文件 / Create complete test config file
pub fn create_test_complete_config_file() -> NamedTempFile {
    let mut file = NamedTempFile::new().expect("Failed to create temp file");

    let config = json!({
        "servers": [
            {
                "type": "stdio",
                "name": "complete_test_server",
                "disabled": false,
                "forbidden_tools": [],
                "tool_meta": {},
                "default_tool_meta": null,
                "vrl": null,
                "server_parameters": {
                    "command": "echo",
                    "args": ["complete"],
                    "env": {},
                    "cwd": null
                }
            }
        ],
        "inputs": [
            {
                "type": "prompt_string",
                "id": "complete_test_input",
                "description": "Complete test input",
                "default": "complete_default",
                "password": false
            }
        ]
    });

    writeln!(file, "{}", config).expect("Failed to write to temp file");
    file
}

/// 创建 CommandHandler 实例 / Create CommandHandler instance
pub async fn create_command_handler() -> CommandHandler {
    let computer = create_test_computer_with_servers().await;
    let cli_config = CliConfig {
        url: None,
        namespace: "test_namespace".to_string(),
        auth: None,
        headers: None,
        office_rejoin_budget_secs: None,
    };
    CommandHandler::new(computer, cli_config)
}

/// 创建未初始化的 CommandHandler 实例 / Create uninitialized CommandHandler instance
pub async fn create_uninitialized_command_handler() -> CommandHandler {
    // #113 S6：remove_server 落盘 → 定向隔离临时目录（forget 保留至进程退出），避免污染仓库工作树。
    let tmp = TempDir::new().unwrap();
    let config_dir = tmp.path().to_path_buf();
    std::mem::forget(tmp);
    let computer = Computer::new(
        "uninitialized_computer",
        SilentSession::new("uninitialized_session"),
        None,
        None,
        false,
        false,
    )
    .with_config_dir(&config_dir)
    .with_config_env(isolated_config_env(&config_dir));
    let cli_config = CliConfig {
        url: None,
        namespace: "test_namespace".to_string(),
        auth: None,
        headers: None,
        office_rejoin_budget_secs: None,
    };
    CommandHandler::new(computer, cli_config)
}
