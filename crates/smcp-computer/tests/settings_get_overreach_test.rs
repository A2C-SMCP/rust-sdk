// `settings get` 越权字段诊断：真实二进制复现 #145（对拍 python-sdk#157 capsys 守卫）。
#![cfg(feature = "cli")]

/*!
* 文件名: settings_get_overreach_test.rs
* 描述: `settings get <overreach-key> --scope project` 在字段被 #143 的 `TRUSTED_SCOPE_ONLY_FIELDS`
*       过滤后，MUST 在 stderr 解释「该字段因 project scope 越权被过滤」，而非只在 stdout 答
*       「not set in scope」主动误导。stdout JSON 契约不变（命中→{key:value}+0；未命中→{"error":..}+1）。
*
* 锚定**真实过滤路径**：写 project `.tfrobot/settings.json` 携越权字段 → `TRUSTED_SCOPE_ONLY_FIELDS`
* → 真 `SettingsValidationError`（非 hand-stuffed 字面量）。
*/

use std::fs;
use std::path::Path;
use std::process::Command;

use tempfile::tempdir;

/// 子进程隔离 env：HOME + XDG_CONFIG_HOME 指向 tempdir，避免宿主 `~/.tfrobot` 泄入。
fn isolated_env(home: &Path) -> Vec<(&'static str, std::path::PathBuf)> {
    vec![
        ("HOME", home.to_path_buf()),
        ("XDG_CONFIG_HOME", home.join("xdg-config")),
    ]
}

/// #145：越权字段被过滤后，`settings get` 必在 stderr 解释（修复前 stderr 全空 → 主动误导）。
#[test]
fn get_overreach_field_explains_instead_of_bare_not_set() {
    let dir = tempdir().unwrap();
    let tfrobot = dir.path().join(".tfrobot");
    fs::create_dir_all(&tfrobot).unwrap();
    // project scope 越权字段（#143 TRUSTED_SCOPE_ONLY_FIELDS）：写了但会被过滤 + 记错。
    fs::write(
        tfrobot.join("settings.json"),
        r#"{"enableAllProjectMcpServers": true}"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_smcp-computer"))
        .args([
            "settings",
            "get",
            "enableAllProjectMcpServers",
            "--scope",
            "project",
            "--json",
        ])
        .current_dir(dir.path())
        .env_clear()
        .envs(isolated_env(dir.path()))
        .output()
        .expect("smcp-computer binary runs");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // stdout 契约：未命中 → {"error": ... not set in scope ...} + exit 1。
    assert_eq!(output.status.code(), Some(1), "exit code; stdout={stdout}");
    assert!(
        stdout.contains("not set in scope"),
        "stdout should carry the not-set JSON; got: {stdout}"
    );

    // 🔴 红绿判别器：stderr 必解释越权过滤（修复前 stderr 全空）。
    assert!(
        stderr.contains("enableAllProjectMcpServers"),
        "stderr must explain the overreach filter instead of bare not-set; got stderr: {stderr}"
    );
    assert!(
        stderr.contains("project"),
        "stderr diagnostic should name the project scope; got stderr: {stderr}"
    );
}
