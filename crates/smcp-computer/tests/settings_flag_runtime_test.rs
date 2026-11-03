// #248：flag 层 readRoots / landingRoot 已由运行期消费——`settings show/get` 不得再报
// 「not applied at runtime」（#251 诊断随 seam 落地撤除）。
#![cfg(feature = "cli")]

/*!
* 文件名: settings_flag_runtime_test.rs
* 描述: rust `Computer` 运行期已读取 flag settings（`--settings`）的 `readRoots` / `landingRoot`
*       （rust-sdk#248）。此前 #251 的诊断在 `settings show/get --scope flag|merged` 上打
*       「not applied at runtime」；本测试锚定**反转**：stdout 仍是干净 JSON、stderr 不再出现该
*       诊断文案，且普通 flag 层校验错误仍照常呈现（不静默）。
*/

mod common;

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use tempfile::tempdir;

fn run(dir: &Path, flag: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_smcp-computer"))
        .arg("--settings")
        .arg(flag)
        .args(args)
        .current_dir(dir)
        .env_clear()
        .envs(common::isolated_binary_env(dir))
        .output()
        .expect("smcp-computer binary runs")
}

fn assert_stdout_json(output: &Output) -> serde_json::Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout 必须是干净 JSON：{e}; stdout={stdout}"))
}

fn stderr_lines(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// `settings show --scope flag|merged`：值可见、exit 0、stderr **无**「not applied at runtime」。
#[test]
fn settings_show_no_longer_reports_flag_scope_as_ineffective() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("data");
    fs::create_dir_all(&root).unwrap();
    let landing = dir.path().join("landing");
    let flag = dir.path().join("flag-settings.json");
    let body = serde_json::json!({
        "readRoots": [root.to_str().unwrap()],
        "landingRoot": landing.to_str().unwrap(),
    });
    fs::write(&flag, serde_json::to_vec(&body).unwrap()).unwrap();

    for scope in ["flag", "merged"] {
        let output = run(
            dir.path(),
            &flag,
            &["settings", "show", "--scope", scope, "--json"],
        );
        let stderr = stderr_lines(&output);
        assert_eq!(
            output.status.code(),
            Some(0),
            "scope={scope}; stderr={stderr}"
        );
        let value = assert_stdout_json(&output);
        assert!(
            value.get("readRoots").is_some(),
            "scope={scope}：flag 值应仍可见：{value}"
        );
        assert!(
            !stderr.contains("not applied at runtime"),
            "scope={scope}：诊断已撤除，不得再报运行期不生效；stderr={stderr}"
        );
    }
}

/// 撤除的仅是「运行期不生效」诊断；普通 flag 层校验错误（scope 越权 / 字段判废）仍须照常呈现
/// （决策保留项，stderr 呈现且不污染 stdout JSON）。
#[test]
fn settings_show_still_reports_flag_validation_errors() {
    let dir = tempdir().unwrap();
    let flag = dir.path().join("flag-settings.json");
    // `allowedMcpServers` 是 policy-only：出现于 flag scope → 过滤 + 记错（#143 既有契约）。
    fs::write(&flag, r#"{"allowedMcpServers":["x"]}"#).unwrap();

    let output = run(
        dir.path(),
        &flag,
        &["settings", "show", "--scope", "flag", "--json"],
    );
    let stderr = stderr_lines(&output);
    assert_eq!(output.status.code(), Some(0), "stderr={stderr}");
    let value = assert_stdout_json(&output);
    assert!(
        value.get("allowedMcpServers").is_none(),
        "policy-only 字段须被过滤：{value}"
    );
    assert!(
        stderr.lines().any(|l| l.contains("allowedMcpServers")),
        "普通 flag 层校验错误须照常呈现：stderr={stderr}"
    );
    assert!(
        !stderr.contains("not applied at runtime"),
        "撤除的诊断不得复活：{stderr}"
    );
}

/// 撤诊断反转覆盖**畸形值**：`readRoots` / `landingRoot` 类型错误照常呈现（stderr 含字段名），
/// 且不得出现「not applied at runtime」（show / get 同类）。
#[test]
fn malformed_flag_values_are_reported_without_ineffective_diagnostic() {
    let dir = tempdir().unwrap();
    let flag = dir.path().join("flag-settings.json");
    fs::write(&flag, r#"{"readRoots":123,"landingRoot":123}"#).unwrap();

    for scope in ["flag", "merged"] {
        let output = run(
            dir.path(),
            &flag,
            &["settings", "show", "--scope", scope, "--json"],
        );
        let stderr = stderr_lines(&output);
        assert_eq!(
            output.status.code(),
            Some(0),
            "scope={scope}; stderr={stderr}"
        );
        assert_stdout_json(&output);
        for field in ["readRoots", "landingRoot"] {
            assert!(
                stderr.lines().any(|line| line.contains(field)),
                "scope={scope}：畸形值校验错误须含字段名 {field}；stderr={stderr}"
            );
        }
        assert!(
            !stderr.contains("not applied at runtime"),
            "scope={scope}：撤除的诊断不得复活；stderr={stderr}"
        );
    }

    for key in ["readRoots", "landingRoot"] {
        let output = run(
            dir.path(),
            &flag,
            &["settings", "get", key, "--scope", "merged", "--json"],
        );
        let stderr = stderr_lines(&output);
        assert!(
            stderr.lines().any(|line| line.contains(key)),
            "get {key}：畸形值校验错误须含字段名；stderr={stderr}"
        );
        assert!(
            !stderr.contains("not applied at runtime"),
            "get {key}：撤除的诊断不得复活；stderr={stderr}"
        );
    }
}

/// `settings get <flag key> --scope merged` 同样不告警，且 stdout 是命中值的干净 JSON。
#[test]
fn settings_get_merged_has_no_ineffective_diagnostic() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("data");
    fs::create_dir_all(&root).unwrap();
    let flag = dir.path().join("flag-settings.json");
    let body = serde_json::json!({"readRoots": [root.to_str().unwrap()]});
    fs::write(&flag, serde_json::to_vec(&body).unwrap()).unwrap();

    let output = run(
        dir.path(),
        &flag,
        &[
            "settings",
            "get",
            "readRoots",
            "--scope",
            "merged",
            "--json",
        ],
    );
    let stderr = stderr_lines(&output);
    assert_eq!(output.status.code(), Some(0), "stderr={stderr}");
    let value = assert_stdout_json(&output);
    assert!(
        value.get("readRoots").is_some(),
        "merged get 应命中 flag 层值：{value}"
    );
    assert!(
        !stderr.contains("not applied at runtime"),
        "stderr 不得再报运行期不生效：{stderr}"
    );
}
