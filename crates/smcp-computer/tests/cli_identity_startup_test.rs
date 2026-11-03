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
        stderr.contains("identity reset"),
        "内容损坏应给出 `identity reset` 重建指引，实际:\n{stderr}"
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

/// protocol#64：一次性 `identity reset --yes` 原子替换身份；空文件（崩溃窗口残留）可重建。
#[test]
fn identity_reset_yes_replaces_identity_and_recovers_empty_file() {
    let home = TempDir::new().unwrap();
    let config_dir = home.path().join("config").join("a2c");
    std::fs::create_dir_all(&config_dir).unwrap();
    // O_EXCL 创建后未写完即 SIGKILL 的典型残留。
    std::fs::write(config_dir.join("computer-id"), "").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_smcp-computer"))
        .args(["--no-color", "identity", "reset", "--yes"])
        .env("A2C_SKILL_HOME", home.path().join("skills"))
        .env("XDG_CONFIG_HOME", home.path().join("config"))
        .env("XDG_DATA_HOME", home.path().join("data"))
        .env("XDG_CACHE_HOME", home.path().join("cache"))
        .env("HOME", home.path())
        .current_dir(home.path())
        .stdin(Stdio::null())
        .output()
        .expect("spawn smcp-computer identity reset");

    assert_eq!(
        output.status.code(),
        Some(0),
        "identity reset --yes 必须成功，stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let persisted = std::fs::read_to_string(config_dir.join("computer-id")).unwrap();
    assert!(
        smcp::is_canonical_computer_id(persisted.trim_end()),
        "重置后应为规范 UUID：{persisted:?}"
    );
    assert!(persisted.ends_with('\n'), "文件应以单换行结尾");
}

/// protocol#64：非交互环境未带 `--yes` ⇒ 拒绝（非零退出）且**不修改**身份文件。
#[test]
fn identity_reset_requires_yes_when_non_interactive() {
    let home = TempDir::new().unwrap();
    let config_dir = home.path().join("config").join("a2c");
    std::fs::create_dir_all(&config_dir).unwrap();
    let original = "7c9e6679-7425-40de-944b-e07fc1f90ae7\n";
    std::fs::write(config_dir.join("computer-id"), original).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_smcp-computer"))
        .args(["--no-color", "identity", "reset"])
        .env("A2C_SKILL_HOME", home.path().join("skills"))
        .env("XDG_CONFIG_HOME", home.path().join("config"))
        .env("XDG_DATA_HOME", home.path().join("data"))
        .env("XDG_CACHE_HOME", home.path().join("cache"))
        .env("HOME", home.path())
        .current_dir(home.path())
        .stdin(Stdio::null())
        .output()
        .expect("spawn smcp-computer identity reset");

    assert_eq!(
        output.status.code(),
        Some(smcp_computer::cli::commands::EXIT_USER_ERROR),
        "非交互未确认必须非零退出，stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(config_dir.join("computer-id")).unwrap(),
        original,
        "被拒绝时不得改动身份文件"
    );
}
