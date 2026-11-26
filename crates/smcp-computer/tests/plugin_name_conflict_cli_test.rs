//! #275：CLI 跨 marketplace 同名 plugin 的真实二进制回归。
//!
//! 覆盖错误面（install/enable → `plugin_name_conflict` + 退出码 1）与「已安装来源胜过仅 add 的
//! marketplace」的关键交互（CLI 每次命令先 `rebuild_registry` 全量 stage 已知 marketplace）。
#![cfg(feature = "cli")]

use std::fs;
use std::path::Path;

use serde_json::{json, Map};
use tempfile::tempdir;

use smcp_computer::mcp_clients::model::BundleId;
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

/// project-scope 账本记录（显式 `projectPath`；用于诊断锚点断言）。
fn seed_project_ledger(skill_home: &Path, pid: &str, install_path: &Path, project_path: &Path) {
    let pid = pid.to_string();
    let install_path = install_path.to_string_lossy().into_owned();
    let project_path = project_path.to_string_lossy().into_owned();
    update_installed_plugins(
        move |f| {
            f.account.plugins.insert(
                pid,
                vec![InstalledPluginRecord {
                    install_path: Some(install_path),
                    mcp_servers: vec![BundleId::try_from("figma-mcp".to_string()).unwrap()],
                    extra: Map::from_iter([
                        ("scope".to_string(), json!("project")),
                        ("projectPath".to_string(), json!(project_path)),
                    ]),
                }],
            );
        },
        Some(skill_home),
        None,
    )
    .unwrap();
}

/// 写 user-scope settings（`run_binary` 的 XDG_CONFIG_HOME = `<dir>/xdg-config`）。
fn write_user_enabled(dir: &Path, pid: &str) {
    let p = dir.join("xdg-config/a2c/settings.json");
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(&p, json!({"enabledPlugins": {pid: true}}).to_string()).unwrap();
}

fn write_server(root: &Path, command: &str) {
    let sd = root.join("mcp-servers");
    fs::create_dir_all(&sd).unwrap();
    fs::write(
        sd.join("figma-mcp.json"),
        json!({
            "type": "stdio",
            "name": "figma-mcp",
            "server_parameters": {"command": command}
        })
        .to_string(),
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
    // 有效 prior-true → 失败包装 `EnableFailedRetained`；CLI 剥壳后 error_code 不变。
    write_user_enabled(dir.path(), "alpha@mp-b");

    let output = run_binary(
        dir.path(),
        &skill_home,
        &["plugin", "enable", "alpha@mp-b", "--json"],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(1), "stdout={stdout}");
    let v = json_stdout(&output);
    assert_eq!(v["error"], "plugin_name_conflict", "stdout={stdout}");
    let message = v["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("alpha@mp-a"),
        "消息须给出已安装 pid; stdout={stdout}"
    );
    assert!(
        message.contains("existing enabled state retained"),
        "prior-true 包装须在 CLI 文本明示保留; stdout={stdout}"
    );
}

/// prior-true + ContentUnavailable（预检失败）：CLI 输出 retained 文本，退出码 1；内层无专属 error_code。
#[test]
fn plugin_enable_prior_true_content_unavailable_reports_retained() {
    let dir = tempdir().unwrap();
    let skill_home = dir.path().join("skill-home");
    fs::create_dir_all(&skill_home).unwrap();
    seed_intent(&skill_home, &["gamma@mp"]);
    let install = dir.path().join("installed-gamma");
    fs::create_dir_all(&install).unwrap();
    // 账本声明 figma-mcp，但安装根内没有该 server 文件 → 预检 ContentUnavailable。
    update_installed_plugins(
        |f| {
            f.account.plugins.insert(
                "gamma@mp".to_string(),
                vec![InstalledPluginRecord {
                    install_path: Some(install.to_string_lossy().into_owned()),
                    mcp_servers: vec![BundleId::try_from("figma-mcp".to_string()).unwrap()],
                    extra: Map::from_iter([("scope".to_string(), json!("user"))]),
                }],
            );
        },
        Some(&skill_home),
        None,
    )
    .unwrap();
    write_user_enabled(dir.path(), "gamma@mp");

    let output = run_binary(
        dir.path(),
        &skill_home,
        &["plugin", "enable", "gamma@mp", "--json"],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(1), "stdout={stdout}");
    let v = json_stdout(&output);
    assert_eq!(v["error"], "error", "stdout={stdout}");
    assert!(
        v["message"]
            .as_str()
            .unwrap_or_default()
            .contains("existing enabled state retained"),
        "stdout={stdout}"
    );
}

/// 诊断锚点（plain CLI）：user=true 但 cwd 的 project=false → 合并视图 disabled，
/// 预检失败 MUST NOT 报 retained（否则与 `plugin list`/协议合并视图矛盾）。
#[test]
fn plugin_enable_prior_true_cwd_project_false_does_not_report_retained() {
    let dir = tempdir().unwrap();
    let skill_home = dir.path().join("skill-home");
    fs::create_dir_all(&skill_home).unwrap();
    seed_intent(&skill_home, &["gamma@mp"]);
    let install = dir.path().join("installed-gamma");
    fs::create_dir_all(&install).unwrap();
    update_installed_plugins(
        |f| {
            f.account.plugins.insert(
                "gamma@mp".to_string(),
                vec![InstalledPluginRecord {
                    install_path: Some(install.to_string_lossy().into_owned()),
                    mcp_servers: vec![BundleId::try_from("figma-mcp".to_string()).unwrap()],
                    extra: Map::from_iter([("scope".to_string(), json!("user"))]),
                }],
            );
        },
        Some(&skill_home),
        None,
    )
    .unwrap();
    write_user_enabled(dir.path(), "gamma@mp");
    // cwd 的 project 层显式 false 覆盖 user true。
    let project = dir.path().join(".tfrobot").join("settings.json");
    fs::create_dir_all(project.parent().unwrap()).unwrap();
    fs::write(
        &project,
        json!({"enabledPlugins": {"gamma@mp": false}}).to_string(),
    )
    .unwrap();

    let output = run_binary(
        dir.path(),
        &skill_home,
        &["plugin", "enable", "gamma@mp", "--json"],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(1), "stdout={stdout}");
    let v = json_stdout(&output);
    assert!(
        !v["message"]
            .as_str()
            .unwrap_or_default()
            .contains("existing enabled state retained"),
        "合并视图 disabled 时不得报 retained; stdout={stdout}"
    );
}

/// 诊断锚点（逐记录）：账本 `projectPath=A`、cwd=B。A project=true → retained 可见；A project=false +
/// user=true → 有效视图 false，不得报 retained。证明诊断不随 cwd 漂移。
#[test]
fn plugin_enable_diagnostic_anchor_uses_record_project_path() {
    let dir = tempdir().unwrap();
    let skill_home = dir.path().join("skill-home");
    fs::create_dir_all(&skill_home).unwrap();
    let pa = dir.path().join("proj-a");
    let pb = dir.path().join("proj-b");
    fs::create_dir_all(&pa).unwrap();
    fs::create_dir_all(&pb).unwrap();
    seed_intent(&skill_home, &["gamma@mp"]);
    let install = dir.path().join("installed-gamma");
    fs::create_dir_all(&install).unwrap();
    seed_project_ledger(&skill_home, "gamma@mp", &install, &pa);
    let a_settings = pa.join(".tfrobot/settings.json");
    fs::create_dir_all(a_settings.parent().unwrap()).unwrap();
    fs::write(
        &a_settings,
        json!({"enabledPlugins": {"gamma@mp": true}}).to_string(),
    )
    .unwrap();

    // 从 B（cwd）运行，操作锚 = 记录 projectPath A（enabled）→ retained 可见。
    let output = run_binary(
        &pb,
        &skill_home,
        &["plugin", "enable", "gamma@mp", "--json"],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(1), "stdout={stdout}");
    let v = json_stdout(&output);
    assert!(
        v["message"]
            .as_str()
            .unwrap_or_default()
            .contains("existing enabled state retained"),
        "诊断必须锚定 record.projectPath=A（enabled），不得用 cwd=B; stdout={stdout}"
    );

    // 反向：A project=false + B（user=true）→ 操作锚 A 的有效视图 false，不得报 retained。
    fs::write(
        &a_settings,
        json!({"enabledPlugins": {"gamma@mp": false}}).to_string(),
    )
    .unwrap();
    write_user_enabled(&pb, "gamma@mp");
    let output = run_binary(
        &pb,
        &skill_home,
        &["plugin", "enable", "gamma@mp", "--json"],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(1), "stdout={stdout}");
    let v = json_stdout(&output);
    assert!(
        !v["message"]
            .as_str()
            .unwrap_or_default()
            .contains("existing enabled state retained"),
        "操作锚 A project=false 覆盖 user=true → 不得报 retained; stdout={stdout}"
    );
}

/// 一次性 CLI（`--settings` flag 层）：flag-only `enabledPlugins=true` 也是有效 prior-true，
/// 预检失败 MUST 输出 retained（flag-aware provider 接线）。
#[test]
fn plugin_enable_prior_true_flag_only_reports_retained() {
    let dir = tempdir().unwrap();
    let skill_home = dir.path().join("skill-home");
    fs::create_dir_all(&skill_home).unwrap();
    seed_intent(&skill_home, &["gamma@mp"]);
    let install = dir.path().join("installed-gamma");
    fs::create_dir_all(&install).unwrap();
    update_installed_plugins(
        |f| {
            f.account.plugins.insert(
                "gamma@mp".to_string(),
                vec![InstalledPluginRecord {
                    install_path: Some(install.to_string_lossy().into_owned()),
                    mcp_servers: vec![BundleId::try_from("figma-mcp".to_string()).unwrap()],
                    extra: Map::from_iter([("scope".to_string(), json!("user"))]),
                }],
            );
        },
        Some(&skill_home),
        None,
    )
    .unwrap();
    let flag = dir.path().join("flag-settings.json");
    fs::write(
        &flag,
        json!({"enabledPlugins": {"gamma@mp": true}}).to_string(),
    )
    .unwrap();

    let output = run_binary(
        dir.path(),
        &skill_home,
        &[
            "--settings",
            flag.to_str().unwrap(),
            "plugin",
            "enable",
            "gamma@mp",
            "--json",
        ],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(1), "stdout={stdout}");
    let v = json_stdout(&output);
    assert!(
        v["message"]
            .as_str()
            .unwrap_or_default()
            .contains("existing enabled state retained"),
        "flag-only prior-true 必须报 retained; stdout={stdout}"
    );
}

/// prior-true + MCP 同名身份坍缩（预检 Conflict）：CLI 剥壳后仍输出 `mcp_server_name_conflict` + retained。
#[test]
fn plugin_enable_prior_true_mcp_conflict_reports_retained() {
    let dir = tempdir().unwrap();
    let skill_home = dir.path().join("skill-home");
    fs::create_dir_all(&skill_home).unwrap();
    seed_intent(&skill_home, &["gamma@mp"]);
    let root_a = dir.path().join("gamma-a");
    let root_b = dir.path().join("gamma-b");
    write_server(&root_a, "node");
    write_server(&root_b, "npx");
    update_installed_plugins(
        |f| {
            f.account.plugins.insert(
                "gamma@mp".to_string(),
                vec![
                    InstalledPluginRecord {
                        install_path: Some(root_a.to_string_lossy().into_owned()),
                        mcp_servers: vec![BundleId::try_from("figma-mcp".to_string()).unwrap()],
                        extra: Map::from_iter([("scope".to_string(), json!("user"))]),
                    },
                    InstalledPluginRecord {
                        install_path: Some(root_b.to_string_lossy().into_owned()),
                        mcp_servers: vec![BundleId::try_from("figma-mcp".to_string()).unwrap()],
                        extra: Map::from_iter([("scope".to_string(), json!("user"))]),
                    },
                ],
            );
        },
        Some(&skill_home),
        None,
    )
    .unwrap();
    write_user_enabled(dir.path(), "gamma@mp");

    let output = run_binary(
        dir.path(),
        &skill_home,
        &["plugin", "enable", "gamma@mp", "--json"],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(1), "stdout={stdout}");
    let v = json_stdout(&output);
    assert_eq!(v["error"], "mcp_server_name_conflict", "stdout={stdout}");
    assert!(
        v["message"]
            .as_str()
            .unwrap_or_default()
            .contains("existing enabled state retained"),
        "stdout={stdout}"
    );
}

/// 🔴1 回归：CLI 每次命令先 `rebuild_registry` 全量 stage 已知 marketplace。仅 add、从未安装的
/// mp-a 不得凭载入序成为 owner，挡住已安装 `alpha@mp-b` 的 enable。
#[test]
fn plugin_enable_installed_plugin_wins_over_add_only_marketplace() {
    let dir = tempdir().unwrap();
    let skill_home = dir.path().join("skill-home");
    fs::create_dir_all(&skill_home).unwrap();
    let url_a = build_relative_source_repo(&dir.path().join("mp-a"), "mp-a", "from mp-a");
    let url_b = build_relative_source_repo(&dir.path().join("mp-b"), "mp-b", "from mp-b");
    update_known_marketplaces(
        |f| {
            f.account.marketplaces.insert(
                "mp-a".to_string(),
                KnownMarketplaceEntry {
                    manifest_name_baseline: None,
                    source: json!({"type": "git", "url": url_a}),
                    extra: Map::new(),
                },
            );
            f.account.marketplaces.insert(
                "mp-b".to_string(),
                KnownMarketplaceEntry {
                    manifest_name_baseline: None,
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
    let url_a = build_relative_source_repo(&dir.path().join("mp-a"), "mp-a", "from mp-a");
    let url_b = build_relative_source_repo(&dir.path().join("mp-b"), "mp-b", "from mp-b");
    update_known_marketplaces(
        |f| {
            for (name, url) in [("mp-a", &url_a), ("mp-b", &url_b)] {
                f.account.marketplaces.insert(
                    name.to_string(),
                    KnownMarketplaceEntry {
                        manifest_name_baseline: None,
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
