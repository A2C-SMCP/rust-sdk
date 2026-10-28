/*!
 * protocol#64：CLI 启动期的身份存储 fail-fast 回归。
 *
 * 身份文件损坏时 `smcp-computer run` **必须非零退出**（不得静默回落临时身份继续启动），
 * 且损坏值原样保留（不得自动覆写）。走真实二进制子进程断言——`prepare_handler` 吞错回归
 * （#145 教训：stderr 行为须真子进程验证）在此被固定。
 */
#![cfg(feature = "cli")]

use std::path::Path;
use std::process::{Command, Stdio};

use tempfile::TempDir;

/// 预置身份文件内容（`None` = 不预置，由 CLI 首建），跑一次 `smcp-computer run`。
fn run_cli(home: &Path, identity: Option<&str>) -> std::process::Output {
    let config_dir = home.join("config").join("a2c");
    std::fs::create_dir_all(&config_dir).unwrap();
    if let Some(identity) = identity {
        std::fs::write(config_dir.join("computer-id"), identity).unwrap();
    }

    Command::new(env!("CARGO_BIN_EXE_smcp-computer"))
        .args(["--no-color", "--url", "http://127.0.0.1:9", "run"])
        .env("A2C_SKILL_HOME", home.join("skills"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env("HOME", home)
        .current_dir(home)
        .stdin(Stdio::null())
        .output()
        .expect("spawn smcp-computer run")
}

#[test]
fn run_exits_nonzero_on_corrupt_computer_identity_without_overwriting() {
    let home = TempDir::new().unwrap();
    let output = run_cli(home.path(), Some("not-a-uuid\n"));

    assert_eq!(
        output.status.code(),
        Some(smcp_computer::cli::commands::EXIT_USER_ERROR),
        "损坏的 computer-id 必须以用户错误码退出，实得 status={:?}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Computer identity store error") && stderr.contains("computer-id"),
        "stderr 应明确指出身份存储错误与文件路径，实际:\n{stderr}"
    );
    assert!(
        stderr.contains("delete this file to regenerate the identity"),
        "内容损坏应给出删除重建指引，实际:\n{stderr}"
    );

    // 不得静默重生成或覆写损坏值。
    let persisted = std::fs::read_to_string(home.path().join("config/a2c/computer-id")).unwrap();
    assert_eq!(persisted, "not-a-uuid\n");
}

/// 身份正常、仅连接失败 ⇒ **非致命**（既有非阻塞语义）：进 REPL 后 EOF 退出，退出码 0。
#[test]
fn run_with_valid_identity_and_unreachable_url_stays_nonfatal() {
    let home = TempDir::new().unwrap();
    let identity = "7c9e6679-7425-40de-944b-e07fc1f90ae7\n";
    let output = run_cli(home.path(), Some(identity));

    assert_eq!(
        output.status.code(),
        Some(0),
        "普通连接失败不得使启动致命，实得 status={:?}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    // 身份文件保持不变（连接失败不得触发重新生成）。
    assert_eq!(
        std::fs::read_to_string(home.path().join("config/a2c/computer-id")).unwrap(),
        identity
    );
}
