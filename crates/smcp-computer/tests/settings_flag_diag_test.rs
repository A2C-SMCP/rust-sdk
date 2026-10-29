// #248：flag scope `readRoots` / `landingRoot` 运行期不生效诊断（真实二进制）。
#![cfg(feature = "cli")]

/*!
* 文件名: settings_flag_diag_test.rs
* 描述: rust `Computer` 运行期不读取 flag settings，故 flag 层的 `readRoots` / `landingRoot`
*       不会生效（fail-closed）。GA 前不得静默：`settings show --scope flag|merged` 与
*       `settings get --scope merged` 必须在 stderr 明确提示「not applied at runtime」，
*       且 stdout 仍是干净 JSON。
*
* 锚定真实二进制与真实 flag 文件（`--settings`），路径取宿主平台原生绝对路径
* （Windows 用盘符形态，避免被校验器按 POSIX 丢弃造成假绿）。
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

fn run(dir: &Path, flag: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_smcp-computer"))
        .arg("--settings")
        .arg(flag)
        .args(args)
        .current_dir(dir)
        .env_clear()
        .envs(isolated_env(dir))
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

/// flag 层出现 `readRoots` / `landingRoot` → `--scope flag` 与默认 `merged` 均须**按字段**告警。
#[test]
fn flag_keys_are_diagnosed_on_flag_and_merged_views() {
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
        assert_stdout_json(&output);
        assert!(
            stderr
                .lines()
                .any(|l| l.contains("readRoots") && l.contains("not applied at runtime")),
            "scope={scope}：stderr 缺 readRoots 诊断；stderr={stderr}"
        );
        assert!(
            stderr
                .lines()
                .any(|l| l.contains("landingRoot") && l.contains("not applied at runtime")),
            "scope={scope}：stderr 缺 landingRoot 诊断；stderr={stderr}"
        );
    }
}

/// `settings get <flag key> --scope merged` 同样告警，且 stdout 是命中值的干净 JSON。
#[test]
fn settings_get_merged_reports_flag_diagnostic() {
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
        stderr
            .lines()
            .any(|l| l.contains("readRoots") && l.contains("not applied at runtime")),
        "stderr 缺诊断：{stderr}"
    );
}

/// flag 层无这两个键 → 不产生该诊断文案；exit 0 且 stdout 干净。
#[test]
fn flag_without_ineffective_keys_has_no_diagnostic() {
    let dir = tempdir().unwrap();
    let flag = dir.path().join("flag-settings.json");
    fs::write(&flag, r#"{"strictKnownMarketplaces":true}"#).unwrap();

    for scope in ["flag", "merged"] {
        let output = run(
            dir.path(),
            &flag,
            &["settings", "show", "--scope", scope, "--json"],
        );
        let stderr = stderr_lines(&output);
        assert_eq!(output.status.code(), Some(0), "scope={scope}");
        assert_stdout_json(&output);
        assert!(
            !stderr.contains("not applied at runtime"),
            "scope={scope} 不应告警；stderr={stderr}"
        );
    }
}
