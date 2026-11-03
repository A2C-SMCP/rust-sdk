// `settings edit` 的 A+ 行为：编辑器写入 project readRoots 后保留 + stderr WARN（#253 复审跟进）。
#![cfg(feature = "cli")]
#![cfg(unix)]

/*!
* 文件名: settings_edit_read_roots_warn_test.rs
* 描述: A+（2026-11-03 用户拍板）要求 project scope `readRoots` 保留 + WARN、不主动删除。
*       `settings edit` 是外部编辑器 carve-out：编辑器写入 `readRoots` 后不得删除，且必须对用户
*       可见地告警（CLI 无 tracing subscriber → eprintln 到 stderr）。
*/

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use tempfile::tempdir;

fn isolated_env(home: &Path) -> Vec<(&'static str, std::path::PathBuf)> {
    vec![
        ("HOME", home.to_path_buf()),
        ("XDG_CONFIG_HOME", home.join("xdg-config")),
    ]
}

#[test]
fn settings_edit_preserves_project_read_roots_and_warns() {
    let dir = tempdir().unwrap();
    let tfrobot = dir.path().join(".tfrobot");
    fs::create_dir_all(&tfrobot).unwrap();
    let settings = tfrobot.join("settings.json");
    fs::write(&settings, "{}").unwrap();

    let editor = dir.path().join("editor.sh");
    fs::write(
        &editor,
        "#!/bin/sh\nprintf '%s' '{\"readRoots\":[\"/data\"],\"keep\":1}' > \"$1\"\n",
    )
    .unwrap();
    fs::set_permissions(&editor, fs::Permissions::from_mode(0o755)).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_smcp-computer"))
        .args(["settings", "edit", "--scope", "project", "--json"])
        .current_dir(dir.path())
        .env_clear()
        .envs(isolated_env(dir.path()))
        .env("EDITOR", &editor)
        .output()
        .expect("smcp-computer binary runs");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout={stdout} stderr={stderr}"
    );
    assert!(
        stderr.contains("readRoots") && stderr.contains("preserved"),
        "stderr must warn that readRoots is preserved/ignored; stderr={stderr}"
    );

    let saved: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
    assert_eq!(
        saved["readRoots"],
        serde_json::json!(["/data"]),
        "A+：edit 不得主动删除 readRoots"
    );
    assert_eq!(saved["keep"], serde_json::json!(1));
}
