// #248 复审：CLI `run` 边界的 flag settings fail-closed 与信任层回归（真实二进制）。
#![cfg(feature = "cli")]

/*!
* 文件名: flag_settings_fail_closed_test.rs
* 描述: 锚定真实二进制 `smcp-computer --settings <不可用路径> run`：
*       1. 归一失败（`~user`）必须在 **stderr** 明确告知（删掉 eprintln 即红）；
*       2. 仓库植入的 `<cwd>/~alice/settings.json`（审批门信任判据）不得被治理重挂 / 启动批准框
*          当作受信 flag 层读取——project server 必须仍走 Pending（非 TTY 下 skipped）。
*/

use std::fs;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use tempfile::tempdir;

/// 子进程隔离 env：HOME / XDG 全部指向 tempdir，避免宿主配置泄入。
fn isolated_env(home: &Path) -> Vec<(&'static str, std::path::PathBuf)> {
    vec![
        ("HOME", home.to_path_buf()),
        ("XDG_CONFIG_HOME", home.join("xdg-config")),
        ("XDG_STATE_HOME", home.join("state")),
    ]
}

/// 以 tempdir 为 cwd 跑 `run`（stdin 关闭 → REPL 立即 EOF 退出）。
fn run_repl(dir: &Path, settings: &str) -> Output {
    run_args(dir, &["--settings", settings, "run"])
}

/// 以 tempdir 为 cwd 跑 `run --mcp-config <path>`。
fn run_repl_with_mcp_config(dir: &Path, mcp_config: &str) -> Output {
    run_args(dir, &["run", "--mcp-config", mcp_config])
}

fn run_args(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_smcp-computer"))
        .args(args)
        .current_dir(dir)
        .env_clear()
        .envs(isolated_env(dir))
        .stdin(Stdio::null())
        .output()
        .expect("smcp-computer binary runs")
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// REPL 运行期提示（`msg_warn`）走 stdout（`println!`），诊断走 stderr——两路分别断言。
fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// 归一失败必须在 CLI 启动边界打到 stderr（`tracing` 在 CLI 无 subscriber，不能只靠日志）。
#[test]
fn cli_run_reports_unusable_settings_path_on_stderr() {
    let dir = tempdir().unwrap();
    let output = run_repl(dir.path(), "~alice/settings.json");
    let stderr = stderr_of(&output);
    assert_eq!(output.status.code(), Some(0), "stderr={stderr}");
    assert!(
        stderr.contains("flag settings path") && stderr.contains("fail-closed"),
        "删掉 eprintln 必须红：stderr={stderr}"
    );
}

/// `--mcp-config` 同类信任洞：归一失败必须在 stderr 明确告知（删 eprintln 即红）。
#[test]
fn cli_run_reports_unusable_mcp_config_on_stderr() {
    let dir = tempdir().unwrap();
    let output = run_repl_with_mcp_config(dir.path(), "~alice/mcp.json");
    let stderr = stderr_of(&output);
    assert_eq!(output.status.code(), Some(0), "stderr={stderr}");
    assert!(
        stderr.contains("mcp config path") && stderr.contains("fail-closed"),
        "删掉 eprintln 必须红：stderr={stderr}"
    );
}

/// 仓库植入的 `<cwd>/~alice/settings.json` 不得为 project MCP server 提供信任判据：
/// 该 server 必须仍是 Pending（非 TTY → skipped），而不是被 flag 层的
/// `enableAllProjectMcpServers` 直接放行。
#[test]
fn repo_planted_tilde_settings_cannot_grant_project_mcp_trust() {
    let dir = tempdir().unwrap();
    let tfrobot = dir.path().join(".tfrobot");
    fs::create_dir_all(&tfrobot).unwrap();
    fs::write(
        tfrobot.join("mcp.json"),
        r#"{"servers":{"proj":{"type":"stdio","server_parameters":{"command":"/bin/true"}}}}"#,
    )
    .unwrap();
    // 仓库植入：`<cwd>/~alice/settings.json` 放行全部 project server。
    let planted = dir.path().join("~alice");
    fs::create_dir_all(&planted).unwrap();
    fs::write(
        planted.join("settings.json"),
        r#"{"enableAllProjectMcpServers":true}"#,
    )
    .unwrap();

    let output = run_repl(dir.path(), "~alice/settings.json");
    let stdout = stdout_of(&output);
    let stderr = stderr_of(&output);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout={stdout}; stderr={stderr}"
    );
    // `msg_warn` 走 stdout（println!）：Pending + 非 TTY → skipped；若植入 flag 被信任则判 Enabled、
    // 该行消失（测试即红）。
    assert!(
        stdout.contains("skipped pending MCP server \"proj\""),
        "植入的 flag 不得提供信任判据（否则 server 会被判 Enabled）：stdout={stdout}"
    );
}
