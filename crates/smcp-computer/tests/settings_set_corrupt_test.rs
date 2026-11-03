// `settings set` strict raw 保真：corrupt 拒绝、原文件逐字节不变、正常写保留被过滤字段（#253）。
#![cfg(feature = "cli")]

/*!
* 文件名: settings_set_corrupt_test.rs
* 描述: 真实二进制验证 #253——`settings set` 遇 corrupt settings 文件必须非零退出、给出可操作错误、
*       且原文件逐字节不变；正常写入必须 raw 保真（A+：project `readRoots` 保留 + WARN，
*       其余字段原值保留）。
*
* CLI 错误经 `msg_err` / `err_flat` 输出到 **stdout**（不是 stderr），断言按 stdout。
*/

use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use std::time::SystemTime;

use tempfile::tempdir;

/// 子进程隔离 env：HOME + XDG_CONFIG_HOME 指向 tempdir。
fn isolated_env(home: &Path) -> Vec<(&'static str, std::path::PathBuf)> {
    vec![
        ("HOME", home.to_path_buf()),
        ("XDG_CONFIG_HOME", home.join("xdg-config")),
    ]
}

fn run_set(dir: &Path, key: &str, value: &str, scope: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_smcp-computer"))
        .args(["settings", "set", key, value, "--scope", scope, "--json"])
        .current_dir(dir)
        .env_clear()
        .envs(isolated_env(dir))
        .output()
        .expect("smcp-computer binary runs")
}

fn mtime(path: &Path) -> SystemTime {
    fs::metadata(path).unwrap().modified().unwrap()
}

fn project_settings(dir: &Path) -> std::path::PathBuf {
    let tfrobot = dir.join(".tfrobot");
    fs::create_dir_all(&tfrobot).unwrap();
    tfrobot.join("settings.json")
}

#[test]
fn set_refuses_corrupt_project_settings_and_keeps_bytes() {
    let dir = tempdir().unwrap();
    let path = project_settings(dir.path());
    fs::write(&path, r#"{"keep":1,"broken":"#).unwrap();
    let before = fs::read(&path).unwrap();

    let output = run_set(dir.path(), "newKey", "42", "project");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(1), "stdout={stdout}");
    assert!(stdout.contains("corrupt"), "stdout={stdout}");
    assert!(stdout.contains("settings.json"), "stdout={stdout}");
    assert_eq!(
        fs::read(&path).unwrap(),
        before,
        "corrupt 文件必须逐字节不变"
    );
}

#[test]
fn set_refuses_non_object_root_and_keeps_bytes() {
    let dir = tempdir().unwrap();
    let path = project_settings(dir.path());
    fs::write(&path, "[1,2]").unwrap();
    let before = fs::read(&path).unwrap();

    let output = run_set(dir.path(), "newKey", "42", "project");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(1), "stdout={stdout}");
    assert!(
        stdout.contains("not a JSON object"),
        "stdout should explain the non-object root; got: {stdout}"
    );
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn set_read_roots_project_json_warns_on_stderr_and_keeps_file_253() {
    // A+：project scope readRoots 不写入、不删旧键；--json 契约含 ignored，且 WARN 经 stderr 可见。
    let dir = tempdir().unwrap();
    let path = project_settings(dir.path());
    fs::write(&path, r#"{"readRoots":["/old"],"keep":1}"#).unwrap();
    let before = fs::read(&path).unwrap();

    let output = run_set(dir.path(), "readRoots", "[\"/x\"]", "project");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout={stdout} stderr={stderr}"
    );
    assert!(stdout.contains("\"ignored\""), "stdout={stdout}");
    assert!(
        stderr.contains("readRoots") && stderr.contains("preserved"),
        "stderr must warn that readRoots is preserved/ignored; stderr={stderr}"
    );
    assert_eq!(
        fs::read(&path).unwrap(),
        before,
        "A+：project readRoots 不得写盘、不得删旧键"
    );
}

#[test]
fn set_same_value_is_noop_and_does_not_touch_file() {
    let dir = tempdir().unwrap();
    let path = project_settings(dir.path());
    fs::write(&path, r#"{"maxConcurrency":7,"keepUnknown":{"k":1}}"#).unwrap();
    let before = fs::read(&path).unwrap();
    let before_mtime = mtime(&path);

    let output = run_set(dir.path(), "maxConcurrency", "7", "project");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(0), "stdout={stdout}");
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(mtime(&path), before_mtime, "同值重设不得写盘");
}

#[test]
fn set_preserves_project_filtered_fields_and_read_roots_with_warning_253() {
    // A+：project scope readRoots 保留 + WARN（stderr），其余被过滤字段原样保留。
    let dir = tempdir().unwrap();
    let path = project_settings(dir.path());
    fs::write(
        &path,
        r#"{
            "readRoots": ["/data"],
            "landingRoot": "/landing",
            "enabledMcpjsonServers": ["srv"],
            "permissions": "invalid-not-object",
            "keepUnknown": {"k": 1}
        }"#,
    )
    .unwrap();

    let output = run_set(dir.path(), "maxConcurrency", "7", "project");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "stdout={stdout}");
    assert!(
        stderr.contains("readRoots") && stderr.contains("preserved"),
        "stderr must warn that readRoots is preserved/ignored; stderr={stderr}"
    );

    let saved: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        saved["readRoots"],
        serde_json::json!(["/data"]),
        "A+：readRoots 必须保留"
    );
    assert_eq!(saved["landingRoot"], serde_json::json!("/landing"));
    assert_eq!(saved["enabledMcpjsonServers"], serde_json::json!(["srv"]));
    assert_eq!(
        saved["permissions"],
        serde_json::json!("invalid-not-object")
    );
    assert_eq!(saved["keepUnknown"], serde_json::json!({"k": 1}));
    assert_eq!(saved["maxConcurrency"], serde_json::json!(7));
}
