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

/// #264 R3：一次性 `settings` 子命令也走同一 fail-closed 归一——植入的 `<cwd>/~alice/settings.json`
/// 不得作为 flag 层被读取；stderr 明确诊断。
#[test]
fn cli_settings_show_fails_closed_on_tilde_path() {
    let dir = tempdir().unwrap();
    let planted_dir = dir.path().join("~alice");
    fs::create_dir_all(&planted_dir).unwrap();
    fs::write(
        planted_dir.join("settings.json"),
        r#"{"trustedMarketplaces":["PLANTED"]}"#,
    )
    .unwrap();

    let output = run_args(
        dir.path(),
        &[
            "--settings",
            "~alice/settings.json",
            "settings",
            "show",
            "--scope",
            "flag",
            "--json",
        ],
    );
    let stdout = stdout_of(&output);
    let stderr = stderr_of(&output);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout={stdout}; stderr={stderr}"
    );
    assert!(
        stderr.contains("flag settings path") && stderr.contains("fail-closed"),
        "一次性 settings 子命令必须 fail-closed 诊断：stderr={stderr}"
    );
    assert!(
        !stdout.contains("PLANTED"),
        "植入的 `~alice/settings.json` 不得作为 flag 层被读取：stdout={stdout}"
    );
}

/// 以 piped stdin 跑 REPL（写入脚本后关闭 stdin → EOF 退出）。
fn run_repl_with_stdin(dir: &Path, args: &[&str], script: &str) -> Output {
    use std::io::Write;
    let mut child = Command::new(env!("CARGO_BIN_EXE_smcp-computer"))
        .args(args)
        .current_dir(dir)
        .env_clear()
        .envs(isolated_env(dir))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("smcp-computer binary runs");
    {
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(script.as_bytes()).unwrap();
        stdin.flush().unwrap();
    }
    child.wait_with_output().expect("smcp-computer exits")
}

/// #264 R3：REPL 内 `settings show` 只认 `run_repl` 传入的归一后路径（失败 = 无 flag 层），
/// 不得回读仓库植入的 `<cwd>/~alice/settings.json`。
#[test]
fn repl_settings_show_fails_closed_on_tilde_path() {
    let dir = tempdir().unwrap();
    let planted_dir = dir.path().join("~alice");
    fs::create_dir_all(&planted_dir).unwrap();
    fs::write(
        planted_dir.join("settings.json"),
        r#"{"trustedMarketplaces":["PLANTED"]}"#,
    )
    .unwrap();

    let output = run_repl_with_stdin(
        dir.path(),
        &["--settings", "~alice/settings.json", "run"],
        "settings show --scope flag --json\n",
    );
    let stdout = stdout_of(&output);
    let stderr = stderr_of(&output);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout={stdout}; stderr={stderr}"
    );
    assert!(
        stderr.contains("flag settings path") && stderr.contains("fail-closed"),
        "REPL 启动边界必须 fail-closed 诊断：stderr={stderr}"
    );
    assert!(
        !stdout.contains("PLANTED"),
        "REPL settings 不得回读植入文件：stdout={stdout}"
    );
}

/// #267：非法用户 scope marketplace 声明必须在 CLI 恢复输出可见（且凭据/控制字符已脱敏）。
#[test]
fn cli_run_reports_invalid_marketplace_declaration() {
    let dir = tempdir().unwrap();
    // `isolated_env` 将 XDG_CONFIG_HOME 指向 `<dir>/xdg-config`；user settings 落在 `<xdg>/a2c/`。
    let settings_dir = dir.path().join("xdg-config").join("a2c");
    fs::create_dir_all(&settings_dir).unwrap();
    fs::write(
        settings_dir.join("settings.json"),
        r#"{"extraKnownMarketplaces":{"https://user:PW_SECRET@example.com/repo.git":{"source":{"type":"git","url":"not-a-url"}}}}"#,
    )
    .unwrap();

    let output = run_args(dir.path(), &["run"]);
    let stdout = stdout_of(&output);
    let stderr = stderr_of(&output);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout={stdout}; stderr={stderr}"
    );
    assert!(
        stdout.contains("declaration ignored; existing registration unchanged"),
        "CLI 恢复输出必须显示非法声明警示：stdout={stdout}"
    );
    assert!(
        stdout.contains("extraKnownMarketplaces."),
        "警示须保留字段前缀：stdout={stdout}"
    );
    assert!(
        !stdout.contains("PW_SECRET") && !stderr.contains("PW_SECRET"),
        "不得回显凭据：stdout={stdout}; stderr={stderr}"
    );
}

/// #302 🟡：存在 `mcp.json` 时，批准流程会再次遍历 settings 错误——非法声明必须只打印一次
/// （治理重挂负责；REPL 批准入口抑制同类）。
#[test]
fn cli_run_prints_invalid_marketplace_declaration_once_with_mcp_json() {
    let dir = tempdir().unwrap();
    let settings_dir = dir.path().join("xdg-config").join("a2c");
    fs::create_dir_all(&settings_dir).unwrap();
    fs::write(
        settings_dir.join("settings.json"),
        r#"{"extraKnownMarketplaces":{"https://user:PW_SECRET@example.com/repo.git":{}}}"#,
    )
    .unwrap();
    // project mcp.json 使 `run_mcp_approval` 越过 `servers.is_empty()` early-return。
    let tfrobot = dir.path().join(".tfrobot");
    fs::create_dir_all(&tfrobot).unwrap();
    fs::write(
        tfrobot.join("mcp.json"),
        r#"{"servers":{"proj":{"type":"stdio","server_parameters":{"command":"/bin/true"}}}}"#,
    )
    .unwrap();

    let output = run_args(dir.path(), &["run"]);
    let stdout = stdout_of(&output);
    assert_eq!(output.status.code(), Some(0), "stdout={stdout}");
    // 关键：approval 的重复行走 `format_settings_errors`，**没有** report 的独有后缀；必须按字段路径
    // `extraKnownMarketplaces.` 计数（期望 1）。若把 `run_mcp_approval_for_repl` 换回全部呈现，这里变 2。
    let field_mentions = stdout.matches("extraKnownMarketplaces.").count();
    assert_eq!(
        field_mentions, 1,
        "同一非法声明只应呈现一次（remount 负责，approval 抑制）：stdout={stdout}"
    );
    let report_lines = stdout
        .matches("declaration ignored; existing registration unchanged")
        .count();
    assert_eq!(
        report_lines, 1,
        "report 警示行也须恰好一次：stdout={stdout}"
    );
    assert!(!stdout.contains("PW_SECRET"), "stdout={stdout}");
}
