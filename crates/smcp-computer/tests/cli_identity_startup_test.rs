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

fn run_with_corrupt_identity(home: &Path) -> std::process::Output {
    let config_dir = home.join("config").join("a2c");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(config_dir.join("computer-id"), "not-a-uuid\n").unwrap();

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
    let output = run_with_corrupt_identity(home.path());

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

    // 不得静默重生成或覆写损坏值。
    let persisted = std::fs::read_to_string(home.path().join("config/a2c/computer-id")).unwrap();
    assert_eq!(persisted, "not-a-uuid\n");
}
