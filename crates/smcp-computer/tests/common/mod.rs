// Enable the cli feature for tests
#![cfg(feature = "cli")]
#![allow(dead_code)]

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

use smcp_computer::cli::commands::{CommandHandler, CliConfig};
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
        MCPServerConfig::Stdio(StdioServerConfig {
            name: "test_server".to_string(),
            disabled: false,
            forbidden_tools: vec![],
            tool_meta: HashMap::new(),
            default_tool_meta: None,
            vrl: None,
            server_parameters: StdioServerParameters {
                command: "echo".to_string(),
                args: vec!["hello".to_string()],
                env: HashMap::new(),
                cwd: None,
            },
        }),
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

    Computer::new(
        "test_computer",
        SilentSession::new("test_session"),
        Some(inputs),
        Some(servers),
        false,
        false,
    )
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
    };
    CommandHandler::new(computer, cli_config)
}

/// 创建未初始化的 CommandHandler 实例 / Create uninitialized CommandHandler instance
pub async fn create_uninitialized_command_handler() -> CommandHandler {
    let computer = Computer::new(
        "uninitialized_computer",
        SilentSession::new("uninitialized_session"),
        None,
        None,
        false,
        false,
    );
    let cli_config = CliConfig {
        url: None,
        namespace: "test_namespace".to_string(),
        auth: None,
        headers: None,
    };
    CommandHandler::new(computer, cli_config)
}
