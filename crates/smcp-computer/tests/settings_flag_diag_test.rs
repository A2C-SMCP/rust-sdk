// #248：flag scope `readRoots` / `landingRoot` 运行期不生效诊断（真实二进制）。
#![cfg(feature = "cli")]

/*!
* 文件名: settings_flag_diag_test.rs
* 描述: rust `Computer` 运行期不读取 flag settings，故 flag 层的 `readRoots` / `landingRoot`
*       不会生效（fail-closed）。GA 前不得静默：`settings show --scope flag` 与默认 merged 视图
*       必须在 stderr 明确提示「not applied at runtime」，且 stdout 仍是干净 JSON。
*
* 锚定真实二进制与真实 flag 文件（`--settings`），不 hand-stuff 诊断字符串。
*/

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use tempfile::tempdir;

/// 子进程隔离 env：HOME + XDG_CONFIG_HOME 指向 tempdir，避免宿主配置泄入。
fn isolated_env(home: &Path) -> Vec<(&'static str, std::path::PathBuf)> {
    vec![
        ("HOME", home.to_path_buf()),
        ("XDG_CONFIG_HOME", home.join("xdg-config")),
    ]
}

fn run_show(dir: &Path, flag: &Path, scope: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_smcp-computer"))
        .args([
            "--settings",
            flag.to_str().unwrap(),
            "settings",
            "show",
            "--scope",
            scope,
            "--json",
        ])
        .current_dir(dir)
        .env_clear()
        .envs(isolated_env(dir))
        .output()
        .expect("smcp-computer binary runs")
}

/// flag 层出现 `readRoots` / `landingRoot` → `--scope flag` 与默认 `merged` 均须告警；stdout 干净。
#[test]
fn flag_keys_are_diagnosed_on_flag_and_merged_views() {
    let dir = tempdir().unwrap();
    let flag = dir.path().join("flag-settings.json");
    fs::write(&flag, r#"{"readRoots":["/data"],"landingRoot":"/landing"}"#).unwrap();

    for scope in ["flag", "merged"] {
        let output = run_show(dir.path(), &flag, scope);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.code(),
            Some(0),
            "scope={scope}; stderr={stderr}"
        );
        serde_json::from_str::<serde_json::Value>(&stdout).unwrap_or_else(|e| {
            panic!("stdout 必须是干净 JSON（scope={scope}）：{e}; stdout={stdout}")
        });
        for field in ["readRoots", "landingRoot"] {
            assert!(
                stderr.contains(field),
                "scope={scope}：stderr 缺字段名 {field}；stderr={stderr}"
            );
        }
        assert!(
            stderr.contains("not applied at runtime"),
            "scope={scope}：stderr 缺不生效文案；stderr={stderr}"
        );
    }
}

/// flag 层无这两个键 → 不产生该诊断文案。
#[test]
fn flag_without_ineffective_keys_has_no_diagnostic() {
    let dir = tempdir().unwrap();
    let flag = dir.path().join("flag-settings.json");
    fs::write(&flag, r#"{"strictKnownMarketplaces":true}"#).unwrap();

    for scope in ["flag", "merged"] {
        let output = run_show(dir.path(), &flag, scope);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !stderr.contains("not applied at runtime"),
            "scope={scope} 不应告警；stderr={stderr}"
        );
    }
}
