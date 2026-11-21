//! #289 marketplace migrate CLI 级验收：dry-run / apply / 退出码 / --resume / --rollback / help。
#![cfg(feature = "cli")]

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};
use tempfile::TempDir;

use smcp_computer::settings::reconciler::KnownMarketplaceEntry;
use smcp_computer::settings::scope::{user_settings_path, EnvMap};
use smcp_computer::settings::store::{
    known_marketplaces_path, update_installed_plugins_intent, update_known_marketplaces,
};

#[path = "common/cli_binary.rs"]
mod cli_binary;

fn env_for(home: &Path, skill_home: &Path) -> EnvMap {
    let mut env = HashMap::new();
    env.insert("HOME".to_string(), home.to_string_lossy().into_owned());
    env.insert(
        "XDG_CONFIG_HOME".to_string(),
        home.join("xdg-config").to_string_lossy().into_owned(),
    );
    env.insert(
        "A2C_SKILL_HOME".to_string(),
        skill_home.to_string_lossy().into_owned(),
    );
    env
}

fn seed_legacy(skill_home: &Path, env: &EnvMap, old: &str, manifest_name: &str) {
    let catalog = skill_home.join("marketplace").join(old);
    fs::create_dir_all(catalog.join(".tfrobot-plugin")).unwrap();
    fs::write(
        catalog.join(".tfrobot-plugin/marketplace.json"),
        serde_json::to_string(&json!({"name": manifest_name, "plugins": []})).unwrap(),
    )
    .unwrap();
    update_known_marketplaces(
        |file| {
            file.account.marketplaces.insert(
                old.to_string(),
                KnownMarketplaceEntry {
                    source: json!({"type": "git", "url": "https://example.com/org/repo.git"}),
                    manifest_name_baseline: None,
                    extra: Map::new(),
                },
            );
        },
        Some(skill_home),
        Some(env),
    )
    .unwrap();
    update_installed_plugins_intent(|_| {}, Some(skill_home), Some(env)).unwrap();
}

fn read_json(path: &Path) -> Value {
    let text = fs::read_to_string(path).unwrap();
    let body: String = text
        .lines()
        .skip_while(|l| l.trim().is_empty() || l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    serde_json::from_str(&body).unwrap()
}

fn setup(td: &TempDir) -> (PathBuf, PathBuf, EnvMap) {
    let home = td.path().join("home");
    let skill_home = td.path().join("skill-home");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&skill_home).unwrap();
    let env = env_for(&home, &skill_home);
    (home, skill_home, env)
}

#[test]
fn cli_default_is_dry_run_with_zero_writes() {
    let td = TempDir::new().unwrap();
    let (home, skill_home, env) = setup(&td);
    seed_legacy(&skill_home, &env, "old-name", "team-tools");
    let before = fs::read(known_marketplaces_path(Some(&skill_home), Some(&env))).unwrap();

    let out = cli_binary::run_binary(&home, &skill_home, &["marketplace", "migrate", "--json"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = cli_binary::json_stdout(&out);
    assert_eq!(v["dryRun"], true);
    assert_eq!(v["items"][0]["action"], "rename");
    assert_eq!(v["items"][0]["oldName"], "old-name");
    assert_eq!(v["items"][0]["newName"], "team-tools");

    assert_eq!(
        before,
        fs::read(known_marketplaces_path(Some(&skill_home), Some(&env))).unwrap()
    );
    assert!(skill_home.join("marketplace/old-name").is_dir());
    assert!(!skill_home
        .join("marketplace-migration-journal.json")
        .exists());
}

#[test]
fn cli_apply_migrates_and_reports_backup() {
    let td = TempDir::new().unwrap();
    let (home, skill_home, env) = setup(&td);
    seed_legacy(&skill_home, &env, "old-name", "team-tools");

    let out = cli_binary::run_binary(
        &home,
        &skill_home,
        &["marketplace", "migrate", "--apply", "--json"],
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = cli_binary::json_stdout(&out);
    assert_eq!(v["dryRun"], false);
    assert!(v["backup"]["timestamp"].is_string());
    assert!(skill_home.join("marketplace/team-tools").is_dir());
    let known = read_json(&known_marketplaces_path(Some(&skill_home), Some(&env)));
    assert!(known["marketplaces"].get("team-tools").is_some());
    assert!(known["marketplaces"].get("old-name").is_none());
}

#[test]
fn cli_conflict_exits_1_with_zero_writes() {
    let td = TempDir::new().unwrap();
    let (home, skill_home, env) = setup(&td);
    seed_legacy(&skill_home, &env, "old-a", "team-tools");
    seed_legacy(&skill_home, &env, "old-b", "team-tools");

    let out = cli_binary::run_binary(
        &home,
        &skill_home,
        &["marketplace", "migrate", "--apply", "--json"],
    );
    assert_eq!(
        out.status.code(),
        Some(1),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = cli_binary::json_stdout(&out);
    assert!(v["conflicts"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["kind"] == "duplicate_target"));
    assert!(skill_home.join("marketplace/old-a").is_dir());
    assert!(skill_home.join("marketplace/old-b").is_dir());
}

#[test]
fn cli_unresolved_exits_3() {
    let td = TempDir::new().unwrap();
    let (home, skill_home, env) = setup(&td);
    seed_legacy(&skill_home, &env, "old-name", "team-tools");
    fs::remove_dir_all(skill_home.join("marketplace/old-name")).unwrap();

    let out = cli_binary::run_binary(
        &home,
        &skill_home,
        &["marketplace", "migrate", "--apply", "--json"],
    );
    assert_eq!(
        out.status.code(),
        Some(3),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = cli_binary::json_stdout(&out);
    assert_eq!(v["items"][0]["action"], "unresolved");
}

#[test]
fn cli_resume_without_journal_exits_1() {
    let td = TempDir::new().unwrap();
    let (home, skill_home, _env) = setup(&td);
    let out = cli_binary::run_binary(
        &home,
        &skill_home,
        &["marketplace", "migrate", "--resume", "--json"],
    );
    assert_eq!(
        out.status.code(),
        Some(1),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn cli_rollback_after_apply_restores() {
    let td = TempDir::new().unwrap();
    let (home, skill_home, env) = setup(&td);
    seed_legacy(&skill_home, &env, "old-name", "team-tools");
    let before = fs::read(known_marketplaces_path(Some(&skill_home), Some(&env))).unwrap();

    let out = cli_binary::run_binary(
        &home,
        &skill_home,
        &["marketplace", "migrate", "--apply", "--json"],
    );
    assert_eq!(out.status.code(), Some(0));
    let v = cli_binary::json_stdout(&out);
    let ts = v["backup"]["timestamp"].as_str().unwrap().to_string();

    let out = cli_binary::run_binary(
        &home,
        &skill_home,
        &["marketplace", "migrate", "--rollback", &ts, "--json"],
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(skill_home.join("marketplace/old-name").is_dir());
    assert!(!skill_home.join("marketplace/team-tools").exists());
    assert_eq!(
        before,
        fs::read(known_marketplaces_path(Some(&skill_home), Some(&env))).unwrap()
    );
}

#[test]
fn cli_help_lists_migrate_flags() {
    let td = TempDir::new().unwrap();
    let (home, skill_home, _env) = setup(&td);
    let out = cli_binary::run_binary(&home, &skill_home, &["marketplace", "migrate", "--help"]);
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    for flag in [
        "--apply",
        "--update-declarations",
        "--resume",
        "--rollback",
        "--json",
    ] {
        assert!(text.contains(flag), "missing {flag} in help: {text}");
    }
}

#[test]
fn cli_user_settings_path_is_isolated() {
    // 防止测试误写宿主 ~/.config：run_binary 固定 XDG_CONFIG_HOME。
    let td = TempDir::new().unwrap();
    let (home, skill_home, env) = setup(&td);
    let user_path = user_settings_path(Some(&env));
    assert!(user_path.starts_with(&home));
    let out = cli_binary::run_binary(&home, &skill_home, &["marketplace", "migrate", "--json"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(!user_path.exists(), "dry-run must not create user settings");
}

#[test]
fn cli_scans_current_project_settings_and_exits_3() {
    let td = TempDir::new().unwrap();
    let (home, skill_home, env) = setup(&td);
    seed_legacy(&skill_home, &env, "old-name", "team-tools");
    // run_binary 的 current_dir 是 home，故项目 settings 放在 home/.tfrobot/。
    let project_settings = home.join(".tfrobot/settings.json");
    fs::create_dir_all(project_settings.parent().unwrap()).unwrap();
    fs::write(
        &project_settings,
        serde_json::to_string(&json!({"enabledPlugins": {"alpha@old-name": true}})).unwrap(),
    )
    .unwrap();
    let out = cli_binary::run_binary(&home, &skill_home, &["marketplace", "migrate", "--json"]);
    assert_eq!(
        out.status.code(),
        Some(3),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = cli_binary::json_stdout(&out);
    let manual = v["manualReferences"].as_array().unwrap();
    assert!(manual.iter().any(|m| m["effect"] == "will-be-inactive"
        && m["file"]
            .as_str()
            .is_some_and(|f| f.ends_with(".tfrobot/settings.json"))));
}
