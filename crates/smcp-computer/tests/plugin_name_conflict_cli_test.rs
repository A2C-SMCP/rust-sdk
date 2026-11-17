//! #275：CLI 跨 marketplace 同名 plugin 的真实二进制回归。
//!
//! 覆盖错误面（install/enable → `plugin_name_conflict` + 退出码 1）与「已安装来源胜过仅 add 的
//! marketplace」的关键交互（CLI 每次命令先 `rebuild_registry` 全量 stage 已知 marketplace）。
#![cfg(feature = "cli")]

use std::fs;
use std::path::Path;

use serde_json::{json, Map};
use tempfile::tempdir;

use smcp_computer::settings::store::{
    update_installed_plugins, update_installed_plugins_intent, update_known_marketplaces,
};
use smcp_computer::settings::{InstalledPluginRecord, KnownMarketplaceEntry};

#[path = "common/cli_binary.rs"]
mod cli_binary;
use cli_binary::{build_relative_source_repo, json_stdout, run_binary};

fn seed_intent(skill_home: &Path, pids: &[&str]) {
    let pids: Vec<String> = pids.iter().map(|s| s.to_string()).collect();
    update_installed_plugins_intent(
        move |f| {
            for pid in pids {
                f.account.installed_plugins.insert(pid);
            }
        },
        Some(skill_home),
        None,
    )
    .unwrap();
}

fn seed_ledger(skill_home: &Path, pid: &str, install_path: &Path) {
    let pid = pid.to_string();
    let install_path = install_path.to_string_lossy().into_owned();
    update_installed_plugins(
        move |f| {
            f.account.plugins.insert(
                pid,
                vec![InstalledPluginRecord {
                    install_path: Some(install_path),
                    mcp_servers: Vec::new(),
                    extra: Map::from_iter([("scope".to_string(), json!("user"))]),
                }],
            );
        },
        Some(skill_home),
        None,
    )
    .unwrap();
}

#[test]
fn plugin_install_duplicate_bare_plugin_reports_plugin_name_conflict() {
    let dir = tempdir().unwrap();
    let skill_home = dir.path().join("skill-home");
    fs::create_dir_all(&skill_home).unwrap();
    seed_intent(&skill_home, &["alpha@mp-a"]);

    let output = run_binary(
        dir.path(),
        &skill_home,
        &["plugin", "install", "alpha@mp-b", "--json"],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(1),
        "plugin_name_conflict 应为用户错误退出码 1; stdout={stdout}"
    );
    let v = json_stdout(&output);
    assert_eq!(v["error"], "plugin_name_conflict", "stdout={stdout}");
    assert!(
        v["message"]
            .as_str()
            .unwrap_or_default()
            .contains("alpha@mp-a"),
        "消息须给出已安装 pid; stdout={stdout}"
    );
}

#[test]
fn plugin_enable_duplicate_installed_plugin_reports_plugin_name_conflict() {
    let dir = tempdir().unwrap();
    let skill_home = dir.path().join("skill-home");
    fs::create_dir_all(&skill_home).unwrap();
    seed_intent(&skill_home, &["alpha@mp-a", "alpha@mp-b"]);
    let install = dir.path().join("installed-alpha-b");
    fs::create_dir_all(&install).unwrap();
    seed_ledger(&skill_home, "alpha@mp-b", &install);

    let output = run_binary(
        dir.path(),
        &skill_home,
        &["plugin", "enable", "alpha@mp-b", "--json"],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(1), "stdout={stdout}");
    let v = json_stdout(&output);
    assert_eq!(v["error"], "plugin_name_conflict", "stdout={stdout}");
    assert!(
        v["message"]
            .as_str()
            .unwrap_or_default()
            .contains("alpha@mp-a"),
        "消息须给出已安装 pid; stdout={stdout}"
    );
}

/// 🔴1 回归：CLI 每次命令先 `rebuild_registry` 全量 stage 已知 marketplace。仅 add、从未安装的
/// mp-a 不得凭载入序成为 owner，挡住已安装 `alpha@mp-b` 的 enable。
#[test]
fn plugin_enable_installed_plugin_wins_over_add_only_marketplace() {
    let dir = tempdir().unwrap();
    let skill_home = dir.path().join("skill-home");
    fs::create_dir_all(&skill_home).unwrap();
    let url_a = build_relative_source_repo(&dir.path().join("mp-a"), "from mp-a");
    let url_b = build_relative_source_repo(&dir.path().join("mp-b"), "from mp-b");
    update_known_marketplaces(
        |f| {
            f.account.marketplaces.insert(
                "mp-a".to_string(),
                KnownMarketplaceEntry {
                    source: json!({"type": "git", "url": url_a}),
                    extra: Map::new(),
                },
            );
            f.account.marketplaces.insert(
                "mp-b".to_string(),
                KnownMarketplaceEntry {
                    source: json!({"type": "git", "url": url_b}),
                    extra: Map::new(),
                },
            );
        },
        Some(&skill_home),
        None,
    )
    .unwrap();
    seed_intent(&skill_home, &["alpha@mp-b"]);
    seed_ledger(
        &skill_home,
        "alpha@mp-b",
        &skill_home.join("marketplace/mp-b/plugins/alpha"),
    );

    let output = run_binary(
        dir.path(),
        &skill_home,
        &["plugin", "enable", "alpha@mp-b", "--json"],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(0),
        "仅 add 的 mp-a 不得挡住已安装 alpha@mp-b 的 enable; stdout={stdout} stderr={stderr}"
    );
    let v = json_stdout(&output);
    assert_eq!(v["enabled"], "alpha@mp-b", "stdout={stdout}");
}

/// R4：真二进制「install mp-a → uninstall mp-a → install mp-b」换源闭环（CI 锁定）。
#[test]
fn plugin_uninstall_then_install_other_marketplace_succeeds() {
    let dir = tempdir().unwrap();
    let skill_home = dir.path().join("skill-home");
    fs::create_dir_all(&skill_home).unwrap();
    let url_a = build_relative_source_repo(&dir.path().join("mp-a"), "from mp-a");
    let url_b = build_relative_source_repo(&dir.path().join("mp-b"), "from mp-b");
    update_known_marketplaces(
        |f| {
            for (name, url) in [("mp-a", &url_a), ("mp-b", &url_b)] {
                f.account.marketplaces.insert(
                    name.to_string(),
                    KnownMarketplaceEntry {
                        source: json!({"type": "git", "url": url}),
                        extra: Map::new(),
                    },
                );
            }
        },
        Some(&skill_home),
        None,
    )
    .unwrap();

    let out = run_binary(
        dir.path(),
        &skill_home,
        &["plugin", "install", "alpha@mp-a", "--json"],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(0),
        "install mp-a failed: stdout={stdout}"
    );
    assert_eq!(json_stdout(&out)["installed"], "alpha@mp-a");

    let out = run_binary(
        dir.path(),
        &skill_home,
        &["plugin", "uninstall", "alpha@mp-a", "--json"],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(0),
        "uninstall mp-a failed: stdout={stdout}"
    );

    let out = run_binary(
        dir.path(),
        &skill_home,
        &["plugin", "install", "alpha@mp-b", "--json"],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(0),
        "uninstall 后换源 install 应成功: stdout={stdout} stderr={stderr}"
    );
    assert_eq!(json_stdout(&out)["installed"], "alpha@mp-b");
}
