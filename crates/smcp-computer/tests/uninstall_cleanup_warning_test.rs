// best-effort 清理失败告警必须对 CLI 用户可见（#253 审查跟进）。
#![cfg(feature = "cli")]

/*!
* 文件名: uninstall_cleanup_warning_test.rs
* 描述: `plugin uninstall` 的 best-effort `enabledPlugins` 清理走 tracing::warn!，而 CLI 未安装
*       tracing subscriber——#253 在 `#[cfg(feature = "cli")]` 下补 eprintln! 让告警落到 stderr。
*       本测试用真实二进制锁定：corrupt user settings 时卸载仍成功、stderr 含清理失败告警、
*       原 settings 文件逐字节不变。
*/

use std::fs;
use std::process::Command;

use tempfile::tempdir;

#[test]
fn plugin_uninstall_corrupt_settings_warns_on_stderr_and_keeps_file() {
    let dir = tempdir().unwrap();
    let skill_home = dir.path().join("skill-home");
    fs::create_dir_all(&skill_home).unwrap();
    // 悬挂 intent：账本无记录、意图仍含 pid → uninstall 走 best-effort 清 enabledPlugins 分支。
    fs::write(
        skill_home.join("installed_plugins_intent.json"),
        r#"{"version":1,"installedPlugins":["a@mp"]}"#,
    )
    .unwrap();
    // corrupt user settings → clear_enabled_plugin 返回 Corrupt，告警须经 stderr 可见。
    let user_settings = dir
        .path()
        .join("xdg-config")
        .join("a2c")
        .join("settings.json");
    fs::create_dir_all(user_settings.parent().unwrap()).unwrap();
    fs::write(&user_settings, "{not json").unwrap();
    let before = fs::read(&user_settings).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_smcp-computer"))
        .args(["plugin", "uninstall", "a@mp", "--json"])
        .current_dir(dir.path())
        .env_clear()
        .env("HOME", dir.path())
        .env("XDG_CONFIG_HOME", dir.path().join("xdg-config"))
        .env("A2C_SKILL_HOME", &skill_home)
        .output()
        .expect("smcp-computer binary runs");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(0),
        "best-effort 清理失败不得让卸载失败; stdout={stdout} stderr={stderr}"
    );
    assert!(stdout.contains("uninstalled"), "stdout={stdout}");
    assert!(
        stderr.contains("failed to clear enabledPlugins"),
        "stderr must surface the best-effort cleanup failure; stderr={stderr}"
    );
    assert_eq!(
        fs::read(&user_settings).unwrap(),
        before,
        "corrupt settings 文件必须逐字节不变"
    );
}
