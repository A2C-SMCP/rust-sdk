//! #267：`marketplace add <url> --no-clone --json` 的真实 CLI stdout 契约。
//!
//! 此前只有处理器函数级 `add_outcome_json_keys_and_no_clone_flags`（不经过二进制/输出层）；
//! 本用例锚定真实进程的退出码与 stdout 纯 JSON 载荷（spinner / 日志不得混入），并证明
//! `--no-clone` 不触碰既有 catalog（上游新提交也不得被 pull 进来）。
#![cfg(feature = "cli")]

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::json;
use tempfile::tempdir;

#[path = "common/cli_binary.rs"]
mod cli_binary;
use cli_binary::{build_relative_source_repo, git, json_stdout, run_binary};

fn assert_ok(output: &Output, what: &str) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{what} 应成功; stdout={stdout} stderr={stderr}"
    );
}

/// 读取 git 仓库当前 HEAD（clone 未动则不变）。
fn git_head(repo: &Path) -> String {
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", repo)
        .output()
        .expect("git available");
    assert!(
        out.status.success(),
        "git rev-parse: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

#[test]
fn marketplace_add_no_clone_json_stdout_contract_267() {
    let dir = tempdir().unwrap();
    let home = dir.path().join("home");
    let skill_home = dir.path().join("skill-home");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&skill_home).unwrap();
    let repo = dir.path().join("repo");
    let url = build_relative_source_repo(&repo, "team-tools", "from test");

    // 首次正常 add（clone）→ 建立 catalog + 账本；这是 --no-clone 的前置条件。
    let first = run_binary(
        &home,
        &skill_home,
        &["marketplace", "add", &url, "--trust", "--json"],
    );
    assert_ok(&first, "first add");
    let first_json = json_stdout(&first);
    assert_eq!(first_json["added"], "team-tools");
    assert_eq!(first_json["noClone"], false);
    assert_eq!(first_json["alreadyRegistered"], false);
    assert_eq!(first_json["skills"], 1);

    let catalog = skill_home.join("marketplace/team-tools");
    let sentinel = catalog.join("NO_CLONE_SENTINEL");
    fs::write(&sentinel, b"keep").unwrap();
    let head_before = git_head(&catalog);

    // 上游新增一次提交：若 --no-clone 误做 pull/fetch 后 ff，catalog HEAD 会变化（哨兵文件抓不住）。
    fs::write(repo.join("UPSTREAM_UPDATE"), b"new").unwrap();
    git(&["add", "-A"], &repo);
    git(&["commit", "-qm", "update"], &repo);

    let second = run_binary(
        &home,
        &skill_home,
        &[
            "marketplace",
            "add",
            &url,
            "--name",
            "team-tools",
            "--trust",
            "--no-clone",
            "--json",
        ],
    );
    assert_ok(&second, "no-clone add");
    let stdout = String::from_utf8_lossy(&second.stdout);
    assert!(
        stdout.trim_start().starts_with('{'),
        "stdout 必须是纯 JSON（不含 spinner/日志前缀）：{stdout}"
    );
    let second_json = json_stdout(&second);
    assert_eq!(
        second_json,
        json!({
            "added": "team-tools",
            "url": url,
            "skills": 0,
            "noClone": true,
            "alreadyRegistered": true,
            "flagsUpdated": false,
        }),
        "no-clone JSON 载荷契约（键名/类型/取值）"
    );
    assert!(sentinel.exists(), "--no-clone 不得触碰既有 catalog");
    assert_eq!(
        git_head(&catalog),
        head_before,
        "--no-clone 不得 pull/fetch 既有 catalog（上游新提交不得进入）"
    );
}
