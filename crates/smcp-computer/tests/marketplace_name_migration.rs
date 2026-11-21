//! #289 D3 存量 marketplace 注册名迁移——库级验收测试。
//!
//! 覆盖：dry-run 零写、全量 apply、幂等、冲突整体 abort、离线 grandfather、baseline 冲突、
//! secret 重键 / keyring 不可用、installPath 定向改写、project/local 只报告、extraKnown 开关、
//! 损坏状态零写入、回滚。
#![allow(clippy::too_many_arguments)]

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{json, Map, Value};
use tempfile::TempDir;

use smcp_computer::inputs::secret_store::{KeyringBackend, SecretStore, SERVICE_NAME};
use smcp_computer::settings::marketplace_migration::{
    apply_marketplace_name_migration, plan_marketplace_name_migration,
    rollback_marketplace_name_migration, MarketplaceMigrationOptions, MigrationAction,
    MigrationReason, MigrationStatus,
};
use smcp_computer::settings::reconciler::{InstalledPluginRecord, KnownMarketplaceEntry};
use smcp_computer::settings::scope::{user_settings_path, workdir_project_settings_path, EnvMap};
use smcp_computer::settings::store::{
    installed_plugins_intent_path, installed_plugins_path, known_marketplaces_path,
    update_installed_plugins, update_installed_plugins_intent, update_known_marketplaces,
};

// ---------------------------------------------------------------------------
// 夹具 / Fixtures
// ---------------------------------------------------------------------------
fn home_of(td: &TempDir) -> PathBuf {
    let home = td.path().join("home");
    fs::create_dir_all(&home).unwrap();
    home
}

fn env_of(td: &TempDir) -> EnvMap {
    let mut env = HashMap::new();
    env.insert(
        "XDG_CONFIG_HOME".to_string(),
        td.path().join("xdg").to_string_lossy().into_owned(),
    );
    env
}

fn source(url: &str) -> Value {
    json!({"type": "git", "url": url})
}

/// 播种一个 legacy marketplace：clone 就位（清单名 `manifest_name`），账本旧名 `old`。
fn seed_marketplace(home: &Path, old: &str, manifest_name: &str, plugins: &[(&str, Option<&str>)]) {
    let catalog = home.join("marketplace").join(old);
    fs::create_dir_all(catalog.join(".tfrobot-plugin")).unwrap();
    let entries: Vec<Value> = plugins
        .iter()
        .map(|(name, _)| json!({"name": name, "source": format!("./plugins/{name}")}))
        .collect();
    fs::write(
        catalog.join(".tfrobot-plugin/marketplace.json"),
        serde_json::to_string(&json!({"name": manifest_name, "plugins": entries})).unwrap(),
    )
    .unwrap();
    for (name, password_input) in plugins {
        let root = catalog.join("plugins").join(name);
        fs::create_dir_all(&root).unwrap();
        if let Some(input_id) = password_input {
            let dir = root.join("mcp-servers");
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                dir.join("inputs.json"),
                serde_json::to_string(&json!({
                    "inputs": [{
                        "type": "PromptString",
                        "id": input_id,
                        "description": "token",
                        "password": true
                    }]
                }))
                .unwrap(),
            )
            .unwrap();
        }
    }
    let url = format!("https://example.com/org/{old}.git");
    update_known_marketplaces(
        move |file| {
            let mut extra = Map::new();
            extra.insert(
                "installLocation".to_string(),
                Value::String(catalog.to_string_lossy().into_owned()),
            );
            extra.insert("autoUpdate".to_string(), Value::Bool(true));
            extra.insert(
                "customKey".to_string(),
                Value::String("keep-me".to_string()),
            );
            file.account.marketplaces.insert(
                old.to_string(),
                KnownMarketplaceEntry {
                    source: source(&url),
                    manifest_name_baseline: None,
                    extra,
                },
            );
        },
        Some(home),
        None,
    )
    .unwrap();
}

fn seed_intent(home: &Path, pids: &[&str]) {
    let pids: Vec<String> = pids.iter().map(|s| s.to_string()).collect();
    update_installed_plugins_intent(
        move |file| {
            for pid in &pids {
                file.account.installed_plugins.insert(pid.clone());
            }
        },
        Some(home),
        None,
    )
    .unwrap();
}

fn seed_ledger(
    home: &Path,
    pid: &str,
    install_path: &str,
    scope: &str,
    project_path: Option<&str>,
) {
    update_installed_plugins(
        move |file| {
            let mut extra = Map::new();
            extra.insert("scope".to_string(), Value::String(scope.to_string()));
            extra.insert("version".to_string(), Value::String("1.2.3".to_string()));
            if let Some(pp) = project_path {
                extra.insert("projectPath".to_string(), Value::String(pp.to_string()));
            }
            file.account.plugins.insert(
                pid.to_string(),
                vec![InstalledPluginRecord {
                    install_path: Some(install_path.to_string()),
                    mcp_servers: Vec::new(),
                    extra,
                }],
            );
        },
        Some(home),
        None,
    )
    .unwrap();
}

fn write_user_settings(env: &EnvMap, value: &Value) {
    let path = user_settings_path(Some(env));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, serde_json::to_string_pretty(value).unwrap()).unwrap();
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

fn read_bytes(path: &Path) -> Vec<u8> {
    fs::read(path).unwrap_or_default()
}

fn known_entry(home: &Path, name: &str) -> Option<KnownMarketplaceEntry> {
    let v = read_json(&known_marketplaces_path(Some(home), None));
    let entry = v.get("marketplaces")?.get(name)?.clone();
    serde_json::from_value(entry).ok()
}

// ---------------------------------------------------------------------------
// Fake keyring / 假 keyring
// ---------------------------------------------------------------------------
#[derive(Clone, Default)]
struct FakeKeyring {
    inner: Arc<Mutex<HashMap<String, String>>>,
    ops: Arc<Mutex<Vec<String>>>,
    fail_set: Arc<Mutex<Option<String>>>,
    available: bool,
}

impl FakeKeyring {
    fn available() -> (Self, SecretStore) {
        let fake = Self {
            inner: Arc::default(),
            ops: Arc::default(),
            fail_set: Arc::default(),
            available: true,
        };
        let store = SecretStore::with_backend(SERVICE_NAME, Box::new(fake.clone()));
        (fake, store)
    }

    fn unavailable() -> (Self, SecretStore) {
        let fake = Self {
            inner: Arc::default(),
            ops: Arc::default(),
            fail_set: Arc::default(),
            available: false,
        };
        let store = SecretStore::with_backend(SERVICE_NAME, Box::new(fake.clone()));
        (fake, store)
    }

    fn get_value(&self, key: &str) -> Option<String> {
        self.inner.lock().unwrap().get(key).cloned()
    }

    fn set_value(&self, key: &str, value: &str) {
        self.inner
            .lock()
            .unwrap()
            .insert(key.to_string(), value.to_string());
    }

    fn fail_set_for(&self, key: &str) {
        *self.fail_set.lock().unwrap() = Some(key.to_string());
    }

    fn ops(&self) -> Vec<String> {
        self.ops.lock().unwrap().clone()
    }
}

impl KeyringBackend for FakeKeyring {
    fn available(&self) -> bool {
        self.available
    }

    fn get(&self, _service: &str, id: &str) -> Option<String> {
        self.inner.lock().unwrap().get(id).cloned()
    }

    fn set(&self, _service: &str, id: &str, value: &str) -> bool {
        if !self.available {
            return false;
        }
        if self.fail_set.lock().unwrap().as_deref() == Some(id) {
            return false;
        }
        self.ops.lock().unwrap().push(format!("set:{id}"));
        self.inner
            .lock()
            .unwrap()
            .insert(id.to_string(), value.to_string());
        true
    }

    fn delete(&self, _service: &str, id: &str) -> bool {
        self.ops.lock().unwrap().push(format!("delete:{id}"));
        self.inner.lock().unwrap().remove(id).is_some()
    }
}

// ---------------------------------------------------------------------------
// 测试 / Tests
// ---------------------------------------------------------------------------
#[test]
fn dry_run_reports_rename_and_writes_nothing() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    seed_marketplace(&home, "old-name", "team-tools", &[("alpha", None)]);
    seed_intent(&home, &["alpha@old-name"]);

    let before_known = read_bytes(&known_marketplaces_path(Some(&home), Some(&env)));
    let before_intent = read_bytes(&installed_plugins_intent_path(Some(&home), Some(&env)));

    let (_, secrets) = FakeKeyring::available();
    let plan = plan_marketplace_name_migration(
        &home,
        Some(&env),
        None,
        MarketplaceMigrationOptions::default(),
        &secrets,
    )
    .unwrap();

    let item = plan
        .items
        .iter()
        .find(|i| i.old_name == "old-name")
        .expect("item");
    assert_eq!(item.action, MigrationAction::Rename);
    assert_eq!(item.new_name.as_deref(), Some("team-tools"));
    assert_eq!(item.source, "https://example.com/org/old-name.git");
    assert!(plan.conflicts.is_empty());

    assert_eq!(
        before_known,
        read_bytes(&known_marketplaces_path(Some(&home), Some(&env)))
    );
    assert_eq!(
        before_intent,
        read_bytes(&installed_plugins_intent_path(Some(&home), Some(&env)))
    );
    assert!(!home.join("marketplace-migration-journal.json").exists());
}

#[test]
fn apply_migrates_authority_and_derived_surfaces() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    let project = td.path().join("proj");
    fs::create_dir_all(&project).unwrap();
    seed_marketplace(&home, "old-name", "team-tools", &[("alpha", None)]);
    seed_intent(&home, &["alpha@old-name"]);
    let old_install = home
        .join("marketplace/old-name/plugins/alpha")
        .to_string_lossy()
        .into_owned();
    seed_ledger(
        &home,
        "alpha@old-name",
        &old_install,
        "project",
        Some(project.to_str().unwrap()),
    );
    write_user_settings(
        &env,
        &json!({
            "enabledPlugins": {"alpha@old-name": false},
            "trustedMarketplaces": ["old-name"],
            "blockedMarketplaces": ["old-name"],
            "extraKnownMarketplaces": {
                "old-name": {"source": {"type": "git", "url": "https://example.com/org/old-name.git"}}
            }
        }),
    );
    let project_settings = workdir_project_settings_path(&project);
    fs::create_dir_all(project_settings.parent().unwrap()).unwrap();
    fs::write(
        &project_settings,
        serde_json::to_string_pretty(&json!({"enabledPlugins": {"alpha@old-name": true}})).unwrap(),
    )
    .unwrap();
    let project_before = read_bytes(&project_settings);

    let (_, secrets) = FakeKeyring::available();
    let report = apply_marketplace_name_migration(
        &home,
        Some(&env),
        Some(&project),
        MarketplaceMigrationOptions::default(),
        &secrets,
    )
    .unwrap();

    assert_eq!(report.status(), MigrationStatus::Partial); // project will-be-inactive
    assert!(report.written);
    assert!(report.backup.is_some());

    // 派生：clone 改名。
    assert!(!home.join("marketplace/old-name").exists());
    assert!(home.join("marketplace/team-tools").is_dir());

    // known：旧键消失、新键带 baseline / installLocation / 保留 extra。
    assert!(known_entry(&home, "old-name").is_none());
    let entry = known_entry(&home, "team-tools").expect("new known entry");
    assert_eq!(entry.manifest_name_baseline.as_deref(), Some("team-tools"));
    assert_eq!(
        entry.extra.get("customKey").and_then(Value::as_str),
        Some("keep-me")
    );
    assert!(entry
        .extra
        .get("installLocation")
        .and_then(Value::as_str)
        .unwrap()
        .ends_with("marketplace/team-tools"));

    // intent pid 改名。
    let intent = read_json(&installed_plugins_intent_path(Some(&home), Some(&env)));
    assert_eq!(intent["installedPlugins"], json!(["alpha@team-tools"]));

    // 账本：pid + installPath 改写；scope/projectPath/version 保留。
    let ledger = read_json(&installed_plugins_path(Some(&home), Some(&env)));
    let recs = ledger["plugins"]["alpha@team-tools"].as_array().unwrap();
    assert_eq!(recs.len(), 1);
    let rec = &recs[0];
    assert_eq!(
        rec["installPath"].as_str().unwrap(),
        home.join("marketplace/team-tools/plugins/alpha")
            .to_string_lossy()
    );
    assert_eq!(rec["scope"], "project");
    assert_eq!(rec["projectPath"], project.to_str().unwrap());
    assert_eq!(rec["version"], "1.2.3");

    // user settings：enabled 显式 false 保留；trusted/blocked 改名；extraKnown 默认不动。
    let user = read_json(&user_settings_path(Some(&env)));
    assert_eq!(user["enabledPlugins"]["alpha@team-tools"], false);
    assert!(user["enabledPlugins"].get("alpha@old-name").is_none());
    assert_eq!(user["trustedMarketplaces"], json!(["team-tools"]));
    assert_eq!(user["blockedMarketplaces"], json!(["team-tools"]));
    assert!(user["extraKnownMarketplaces"].get("old-name").is_some());

    // project/local 不写盘；报告显著提示失活。
    assert_eq!(project_before, read_bytes(&project_settings));
    assert!(report
        .manual
        .iter()
        .any(|m| m.effect == "will-be-inactive" && m.file == project_settings.to_string_lossy()));
}

#[test]
fn apply_update_declarations_rewrites_only_user() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    let project = td.path().join("proj");
    fs::create_dir_all(&project).unwrap();
    seed_marketplace(&home, "old-name", "team-tools", &[]);
    write_user_settings(
        &env,
        &json!({
            "extraKnownMarketplaces": {
                "old-name": {"source": {"type": "git", "url": "https://example.com/org/old-name.git"}}
            }
        }),
    );
    let project_settings = workdir_project_settings_path(&project);
    fs::create_dir_all(project_settings.parent().unwrap()).unwrap();
    let project_json = json!({
        "extraKnownMarketplaces": {
            "old-name": {"source": {"type": "git", "url": "https://example.com/org/old-name.git"}}
        }
    });
    fs::write(
        &project_settings,
        serde_json::to_string_pretty(&project_json).unwrap(),
    )
    .unwrap();
    let before_project = read_bytes(&project_settings);

    let (_, secrets) = FakeKeyring::available();
    let report = apply_marketplace_name_migration(
        &home,
        Some(&env),
        Some(&project),
        MarketplaceMigrationOptions {
            update_declarations: true,
        },
        &secrets,
    )
    .unwrap();
    let user = read_json(&user_settings_path(Some(&env)));
    assert!(user["extraKnownMarketplaces"].get("old-name").is_none());
    assert!(user["extraKnownMarketplaces"].get("team-tools").is_some());
    // project/local 声明只报告，不写盘。
    assert_eq!(before_project, read_bytes(&project_settings));
    assert!(report
        .manual
        .iter()
        .any(|m| m.file == project_settings.to_string_lossy()
            && m.pointer.contains("extraKnownMarketplaces")));
}

#[test]
fn url_derived_and_explicit_alias_old_names_both_migrate() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    // 旧名来源不同（URL 末段 / 显式 --name 别名）对迁移不可区分，两者都应迁移。
    seed_marketplace(&home, "repo-plugins", "team-tools", &[]);
    seed_marketplace(&home, "my-alias", "data-tools", &[]);
    let (_, secrets) = FakeKeyring::available();
    let report = apply_marketplace_name_migration(
        &home,
        Some(&env),
        None,
        MarketplaceMigrationOptions::default(),
        &secrets,
    )
    .unwrap();
    assert_eq!(report.status(), MigrationStatus::Ok);
    assert!(home.join("marketplace/team-tools").is_dir());
    assert!(home.join("marketplace/data-tools").is_dir());
    assert!(!home.join("marketplace/repo-plugins").exists());
    assert!(!home.join("marketplace/my-alias").exists());
}

#[test]
fn second_apply_is_noop_without_backup() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    seed_marketplace(&home, "old-name", "team-tools", &[]);
    let (_, secrets) = FakeKeyring::available();
    apply_marketplace_name_migration(
        &home,
        Some(&env),
        None,
        MarketplaceMigrationOptions::default(),
        &secrets,
    )
    .unwrap();
    let report = apply_marketplace_name_migration(
        &home,
        Some(&env),
        None,
        MarketplaceMigrationOptions::default(),
        &secrets,
    )
    .unwrap();
    assert!(report.backup.is_none());
    assert_eq!(report.status(), MigrationStatus::Ok);
}

#[test]
fn conflict_duplicate_target_aborts_zero_write() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    seed_marketplace(&home, "old-a", "team-tools", &[]);
    seed_marketplace(&home, "old-b", "team-tools", &[]);
    let before = read_bytes(&known_marketplaces_path(Some(&home), Some(&env)));
    let (_, secrets) = FakeKeyring::available();
    let report = apply_marketplace_name_migration(
        &home,
        Some(&env),
        None,
        MarketplaceMigrationOptions::default(),
        &secrets,
    )
    .unwrap();
    assert_eq!(report.status(), MigrationStatus::Conflicts);
    assert!(report
        .conflicts
        .iter()
        .any(|c| c.kind == "duplicate_target"));
    assert!(report.backup.is_none());
    assert_eq!(
        before,
        read_bytes(&known_marketplaces_path(Some(&home), Some(&env)))
    );
    assert!(home.join("marketplace/old-a").is_dir());
    assert!(home.join("marketplace/old-b").is_dir());
}

#[test]
fn conflict_target_name_diff_source() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    seed_marketplace(&home, "old-name", "team-tools", &[]);
    update_known_marketplaces(
        |file| {
            file.account.marketplaces.insert(
                "team-tools".to_string(),
                KnownMarketplaceEntry {
                    source: source("https://example.com/other/team-tools.git"),
                    manifest_name_baseline: Some("team-tools".to_string()),
                    extra: Map::new(),
                },
            );
        },
        Some(&home),
        None,
    )
    .unwrap();
    let (_, secrets) = FakeKeyring::available();
    let report = apply_marketplace_name_migration(
        &home,
        Some(&env),
        None,
        MarketplaceMigrationOptions::default(),
        &secrets,
    )
    .unwrap();
    assert_eq!(report.status(), MigrationStatus::Conflicts);
    assert!(report
        .conflicts
        .iter()
        .any(|c| c.kind == "target_name_conflict"));
}

#[test]
fn conflict_bare_name_clash() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    seed_marketplace(&home, "old-name", "team-tools", &[("alpha", None)]);
    seed_intent(&home, &["alpha@old-name", "alpha@other"]);
    let (_, secrets) = FakeKeyring::available();
    let report = apply_marketplace_name_migration(
        &home,
        Some(&env),
        None,
        MarketplaceMigrationOptions::default(),
        &secrets,
    )
    .unwrap();
    assert_eq!(report.status(), MigrationStatus::Conflicts);
    assert!(report.conflicts.iter().any(|c| c.kind == "bare_name_clash"));
}

#[test]
fn unresolved_catalog_missing_grandfathers() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    seed_marketplace(&home, "old-name", "team-tools", &[]);
    fs::remove_dir_all(home.join("marketplace/old-name")).unwrap();
    let before = read_bytes(&known_marketplaces_path(Some(&home), Some(&env)));
    let (_, secrets) = FakeKeyring::available();
    let report = apply_marketplace_name_migration(
        &home,
        Some(&env),
        None,
        MarketplaceMigrationOptions::default(),
        &secrets,
    )
    .unwrap();
    let item = report
        .items
        .iter()
        .find(|i| i.old_name == "old-name")
        .unwrap();
    assert_eq!(item.action, MigrationAction::Unresolved);
    assert_eq!(item.reason, Some(MigrationReason::CatalogMissing));
    assert_eq!(report.status(), MigrationStatus::Partial);
    assert!(report.backup.is_none());
    assert_eq!(
        before,
        read_bytes(&known_marketplaces_path(Some(&home), Some(&env)))
    );
}

#[test]
fn blocked_baseline_mismatch_keeps_old_registration() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    seed_marketplace(&home, "old-name", "team-tools", &[]);
    update_known_marketplaces(
        |file| {
            if let Some(entry) = file.account.marketplaces.get_mut("old-name") {
                entry.manifest_name_baseline = Some("previous-name".to_string());
            }
        },
        Some(&home),
        None,
    )
    .unwrap();
    let (_, secrets) = FakeKeyring::available();
    let report = apply_marketplace_name_migration(
        &home,
        Some(&env),
        None,
        MarketplaceMigrationOptions::default(),
        &secrets,
    )
    .unwrap();
    let item = report
        .items
        .iter()
        .find(|i| i.old_name == "old-name")
        .unwrap();
    assert_eq!(item.action, MigrationAction::Blocked);
    assert_eq!(item.reason, Some(MigrationReason::BaselineMismatch));
    assert!(report.backup.is_none());
    assert!(known_entry(&home, "old-name").is_some());
}

#[test]
fn secret_rekey_sets_new_before_deleting_old() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    seed_marketplace(&home, "old-name", "team-tools", &[("alpha", Some("token"))]);
    seed_intent(&home, &["alpha@old-name"]);
    let (fake, secrets) = FakeKeyring::available();
    fake.set_value("alpha@old-name/token", "s3cret");

    let report = apply_marketplace_name_migration(
        &home,
        Some(&env),
        None,
        MarketplaceMigrationOptions::default(),
        &secrets,
    )
    .unwrap();
    assert_eq!(report.status(), MigrationStatus::Ok, "report={report:?}");
    assert_eq!(
        fake.get_value("alpha@team-tools/token").as_deref(),
        Some("s3cret")
    );
    assert!(fake.get_value("alpha@old-name/token").is_none());
    // env 变量名变化被报告。
    let item = report
        .items
        .iter()
        .find(|i| i.old_name == "old-name")
        .unwrap();
    assert!(item.env_renames.iter().any(|e| {
        e.old == "A2C_SMCP_alpha_old_name_token" && e.new == "A2C_SMCP_alpha_team_tools_token"
    }));
}

#[test]
fn secret_keyring_unavailable_aborts_zero_write() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    seed_marketplace(&home, "old-name", "team-tools", &[("alpha", Some("token"))]);
    let (fake, secrets) = FakeKeyring::unavailable();
    fake.set_value("alpha@old-name/token", "s3cret");
    let before = read_bytes(&known_marketplaces_path(Some(&home), Some(&env)));
    let report = apply_marketplace_name_migration(
        &home,
        Some(&env),
        None,
        MarketplaceMigrationOptions::default(),
        &secrets,
    )
    .unwrap();
    let item = report
        .items
        .iter()
        .find(|i| i.old_name == "old-name")
        .unwrap();
    assert_eq!(item.action, MigrationAction::Blocked);
    assert_eq!(item.reason, Some(MigrationReason::KeyringUnavailable));
    assert!(report.backup.is_none());
    assert_eq!(
        before,
        read_bytes(&known_marketplaces_path(Some(&home), Some(&env)))
    );
    assert!(home.join("marketplace/old-name").is_dir());
}

#[test]
fn install_path_outside_roots_blocks_without_partial_rewrite() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    seed_marketplace(&home, "old-name", "team-tools", &[("alpha", None)]);
    seed_intent(&home, &["alpha@old-name"]);
    seed_ledger(&home, "alpha@old-name", "/elsewhere/alpha", "user", None);
    let (_, secrets) = FakeKeyring::available();
    let report = apply_marketplace_name_migration(
        &home,
        Some(&env),
        None,
        MarketplaceMigrationOptions::default(),
        &secrets,
    )
    .unwrap();
    let item = report
        .items
        .iter()
        .find(|i| i.old_name == "old-name")
        .unwrap();
    assert_eq!(item.action, MigrationAction::Unresolved);
    assert_eq!(item.reason, Some(MigrationReason::InstallPathUnexpected));
    assert!(report.backup.is_none());
    assert!(home.join("marketplace/old-name").is_dir());
}

#[test]
fn corrupt_state_is_blocked_without_renaming_backup() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    let intent_path = installed_plugins_intent_path(Some(&home), Some(&env));
    fs::write(&intent_path, "{ this is not json").unwrap();
    let before = read_bytes(&intent_path);
    let (_, secrets) = FakeKeyring::available();
    let report = apply_marketplace_name_migration(
        &home,
        Some(&env),
        None,
        MarketplaceMigrationOptions::default(),
        &secrets,
    )
    .unwrap();
    assert_eq!(report.status(), MigrationStatus::Partial);
    assert!(report.corrupt.is_some());
    assert!(report.backup.is_none());
    assert_eq!(before, read_bytes(&intent_path));
    let names: Vec<String> = fs::read_dir(&home)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        !names.iter().any(|n| n.contains(".corrupt-")),
        "must not rename corrupt state during preflight: {names:?}"
    );
}

#[test]
fn rollback_restores_files_and_directories() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    seed_marketplace(&home, "old-name", "team-tools", &[("alpha", None)]);
    seed_intent(&home, &["alpha@old-name"]);
    let known_path = known_marketplaces_path(Some(&home), Some(&env));
    let intent_path = installed_plugins_intent_path(Some(&home), Some(&env));
    let before_known = read_bytes(&known_path);
    let before_intent = read_bytes(&intent_path);

    let (_, secrets) = FakeKeyring::available();
    let report = apply_marketplace_name_migration(
        &home,
        Some(&env),
        None,
        MarketplaceMigrationOptions::default(),
        &secrets,
    )
    .unwrap();
    let timestamp = report.backup.as_ref().unwrap().timestamp.clone();
    rollback_marketplace_name_migration(&home, Some(&env), &timestamp, &secrets).unwrap();

    assert!(home.join("marketplace/old-name").is_dir());
    assert!(!home.join("marketplace/team-tools").exists());
    assert_eq!(before_known, read_bytes(&known_path));
    assert_eq!(before_intent, read_bytes(&intent_path));
}

#[test]
fn external_git_plugin_tree_is_renamed_and_install_path_rewritten() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    let catalog = home.join("marketplace/old-name");
    fs::create_dir_all(catalog.join(".tfrobot-plugin")).unwrap();
    fs::write(
        catalog.join(".tfrobot-plugin/marketplace.json"),
        serde_json::to_string(&json!({
            "name": "team-tools",
            "plugins": [{"name": "alpha", "source": {"source": "github", "repo": "org/alpha"}}]
        }))
        .unwrap(),
    )
    .unwrap();
    let ext = home.join("marketplace/.plugins/old-name/alpha");
    fs::create_dir_all(&ext).unwrap();
    update_known_marketplaces(
        |file| {
            file.account.marketplaces.insert(
                "old-name".to_string(),
                KnownMarketplaceEntry {
                    source: source("https://example.com/org/repo.git"),
                    manifest_name_baseline: None,
                    extra: Map::new(),
                },
            );
        },
        Some(&home),
        None,
    )
    .unwrap();
    seed_intent(&home, &["alpha@old-name"]);
    seed_ledger(
        &home,
        "alpha@old-name",
        &ext.to_string_lossy(),
        "user",
        None,
    );
    let (_, secrets) = FakeKeyring::available();
    apply_marketplace_name_migration(
        &home,
        Some(&env),
        None,
        MarketplaceMigrationOptions::default(),
        &secrets,
    )
    .unwrap();
    assert!(!home.join("marketplace/.plugins/old-name").exists());
    assert!(home.join("marketplace/.plugins/team-tools/alpha").is_dir());
    let ledger = read_json(&installed_plugins_path(Some(&home), Some(&env)));
    assert_eq!(
        ledger["plugins"]["alpha@team-tools"][0]["installPath"]
            .as_str()
            .unwrap(),
        home.join("marketplace/.plugins/team-tools/alpha")
            .to_string_lossy()
    );
}

#[test]
fn multi_scope_and_unknown_ledger_fields_preserved() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    let project = td.path().join("proj");
    fs::create_dir_all(&project).unwrap();
    seed_marketplace(&home, "old-name", "team-tools", &[("alpha", None)]);
    seed_intent(&home, &["alpha@old-name"]);
    let path = home
        .join("marketplace/old-name/plugins/alpha")
        .to_string_lossy()
        .into_owned();
    update_installed_plugins(
        |file| {
            let mut user_extra = Map::new();
            user_extra.insert("scope".into(), Value::String("user".into()));
            user_extra.insert("version".into(), Value::String("1.0.0".into()));
            user_extra.insert("customLedgerKey".into(), Value::String("keep".into()));
            let mut proj_extra = Map::new();
            proj_extra.insert("scope".into(), Value::String("project".into()));
            proj_extra.insert(
                "projectPath".into(),
                Value::String(project.to_string_lossy().into_owned()),
            );
            file.account.plugins.insert(
                "alpha@old-name".into(),
                vec![
                    InstalledPluginRecord {
                        install_path: Some(path.clone()),
                        mcp_servers: Vec::new(),
                        extra: user_extra,
                    },
                    InstalledPluginRecord {
                        install_path: Some(path.clone()),
                        mcp_servers: Vec::new(),
                        extra: proj_extra,
                    },
                ],
            );
        },
        Some(&home),
        None,
    )
    .unwrap();
    let (_, secrets) = FakeKeyring::available();
    apply_marketplace_name_migration(
        &home,
        Some(&env),
        Some(&project),
        MarketplaceMigrationOptions::default(),
        &secrets,
    )
    .unwrap();
    let ledger = read_json(&installed_plugins_path(Some(&home), Some(&env)));
    let recs = ledger["plugins"]["alpha@team-tools"].as_array().unwrap();
    assert_eq!(recs.len(), 2);
    assert_eq!(recs[0]["scope"], "user");
    assert_eq!(recs[0]["customLedgerKey"], "keep");
    assert_eq!(recs[1]["scope"], "project");
    assert_eq!(recs[1]["projectPath"], project.to_str().unwrap());
}

#[test]
fn same_source_target_merge_is_not_conflict() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    seed_marketplace(&home, "old-name", "team-tools", &[]);
    update_known_marketplaces(
        |file| {
            let mut extra = Map::new();
            extra.insert("autoUpdate".to_string(), Value::Bool(false));
            extra.insert("targetOnly".to_string(), Value::String("x".to_string()));
            file.account.marketplaces.insert(
                "team-tools".to_string(),
                KnownMarketplaceEntry {
                    source: source("https://example.com/org/old-name.git"),
                    manifest_name_baseline: Some("team-tools".to_string()),
                    extra,
                },
            );
        },
        Some(&home),
        None,
    )
    .unwrap();
    let (_, secrets) = FakeKeyring::available();
    let report = apply_marketplace_name_migration(
        &home,
        Some(&env),
        None,
        MarketplaceMigrationOptions::default(),
        &secrets,
    )
    .unwrap();
    assert!(report.conflicts.is_empty(), "{:?}", report.conflicts);
    let entry = known_entry(&home, "team-tools").unwrap();
    assert_eq!(entry.extra.get("autoUpdate"), Some(&Value::Bool(true)));
    assert_eq!(
        entry.extra.get("customKey").and_then(Value::as_str),
        Some("keep-me")
    );
    assert_eq!(
        entry.extra.get("targetOnly").and_then(Value::as_str),
        Some("x")
    );
}

#[test]
fn keyring_unavailable_without_secrets_still_migrates() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    seed_marketplace(&home, "old-name", "team-tools", &[("alpha", None)]);
    let (_, secrets) = FakeKeyring::unavailable();
    let report = apply_marketplace_name_migration(
        &home,
        Some(&env),
        None,
        MarketplaceMigrationOptions::default(),
        &secrets,
    )
    .unwrap();
    assert_eq!(report.status(), MigrationStatus::Ok);
    assert!(home.join("marketplace/team-tools").is_dir());
}

#[test]
fn update_declarations_mismatched_old_source_conflicts() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    seed_marketplace(&home, "old-name", "team-tools", &[]);
    write_user_settings(
        &env,
        &json!({
            "extraKnownMarketplaces": {
                "old-name": {"source": {"type": "git", "url": "https://example.com/other/source.git"}}
            }
        }),
    );
    let before = read_bytes(&known_marketplaces_path(Some(&home), Some(&env)));
    let (_, secrets) = FakeKeyring::available();
    let report = apply_marketplace_name_migration(
        &home,
        Some(&env),
        None,
        MarketplaceMigrationOptions {
            update_declarations: true,
        },
        &secrets,
    )
    .unwrap();
    assert_eq!(report.status(), MigrationStatus::Conflicts);
    assert!(report
        .conflicts
        .iter()
        .any(|c| c.kind == "extra_known_old_conflict"));
    assert_eq!(
        before,
        read_bytes(&known_marketplaces_path(Some(&home), Some(&env)))
    );
}

#[test]
fn missing_intent_is_seeded_from_ledger_instead_of_becoming_empty() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    seed_marketplace(&home, "old-name", "team-tools", &[("alpha", None)]);
    // pre-v0.3.0 home：只有账本，没有 installed_plugins_intent.json。
    let intent_path = installed_plugins_intent_path(Some(&home), Some(&env));
    assert!(!intent_path.exists());
    let install = home
        .join("marketplace/old-name/plugins/alpha")
        .to_string_lossy()
        .into_owned();
    seed_ledger(&home, "alpha@old-name", &install, "user", None);
    let (_, secrets) = FakeKeyring::available();
    apply_marketplace_name_migration(
        &home,
        Some(&env),
        None,
        MarketplaceMigrationOptions::default(),
        &secrets,
    )
    .unwrap();
    let intent = read_json(&intent_path);
    assert_eq!(intent["installedPlugins"], json!(["alpha@team-tools"]));
}

#[test]
fn pre_v03_multi_marketplace_seed_migrates_all_without_resurrecting_old_pids() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    seed_marketplace(&home, "old-a", "team-a", &[("alpha", None)]);
    seed_marketplace(&home, "old-b", "team-b", &[("beta", None)]);
    let path_a = home
        .join("marketplace/old-a/plugins/alpha")
        .to_string_lossy()
        .into_owned();
    let path_b = home
        .join("marketplace/old-b/plugins/beta")
        .to_string_lossy()
        .into_owned();
    // pre-v0.3.0：只有 ledger，没有 intent。
    seed_ledger(&home, "alpha@old-a", &path_a, "user", None);
    seed_ledger(&home, "beta@old-b", &path_b, "user", None);
    let intent_path = installed_plugins_intent_path(Some(&home), Some(&env));
    assert!(!intent_path.exists());

    let (_, secrets) = FakeKeyring::available();
    let report = apply_marketplace_name_migration(
        &home,
        Some(&env),
        None,
        MarketplaceMigrationOptions::default(),
        &secrets,
    )
    .unwrap();
    assert_eq!(report.status(), MigrationStatus::Ok, "{report:?}");
    let intent = read_json(&intent_path);
    let ids = intent["installedPlugins"].as_array().unwrap();
    assert!(ids.iter().any(|v| v == "alpha@team-a"));
    assert!(ids.iter().any(|v| v == "beta@team-b"));
    assert!(!ids.iter().any(|v| v == "alpha@old-a"));
    assert!(!ids.iter().any(|v| v == "beta@old-b"));
}

#[test]
fn project_scope_ledger_does_not_write_project_settings() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    let project = td.path().join("proj");
    fs::create_dir_all(&project).unwrap();
    seed_marketplace(&home, "old-name", "team-tools", &[("alpha", None)]);
    let path = home
        .join("marketplace/old-name/plugins/alpha")
        .to_string_lossy()
        .into_owned();
    seed_ledger(
        &home,
        "alpha@old-name",
        &path,
        "project",
        Some(project.to_str().unwrap()),
    );
    let project_settings = workdir_project_settings_path(&project);
    let (_, secrets) = FakeKeyring::available();
    apply_marketplace_name_migration(
        &home,
        Some(&env),
        Some(&project),
        MarketplaceMigrationOptions::default(),
        &secrets,
    )
    .unwrap();
    // 官方 v0.3.0 迁移会顺手写 project enabledPlugins；本迁移必须不写 project/local。
    assert!(
        !project_settings.exists(),
        "project settings must not be created by migration: {}",
        project_settings.display()
    );
}

#[test]
fn user_only_references_migrate_cleanly_exit_ok() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    seed_marketplace(&home, "old-name", "team-tools", &[]);
    write_user_settings(
        &env,
        &json!({
            "enabledPlugins": {"alpha@old-name": true},
            "trustedMarketplaces": ["old-name"],
            "blockedMarketplaces": ["old-name"]
        }),
    );
    let (_, secrets) = FakeKeyring::available();
    let report = apply_marketplace_name_migration(
        &home,
        Some(&env),
        None,
        MarketplaceMigrationOptions::default(),
        &secrets,
    )
    .unwrap();
    assert_eq!(report.status(), MigrationStatus::Ok, "{report:?}");
    assert!(
        report.manual.is_empty(),
        "user-scope refs are rewritten, not manual"
    );
}

#[test]
fn cwd_project_reference_reports_will_be_inactive() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    let project = td.path().join("proj");
    fs::create_dir_all(&project).unwrap();
    seed_marketplace(&home, "old-name", "team-tools", &[]);
    let settings = workdir_project_settings_path(&project);
    fs::create_dir_all(settings.parent().unwrap()).unwrap();
    fs::write(
        &settings,
        serde_json::to_string(&json!({"enabledPlugins": {"alpha@old-name": true}})).unwrap(),
    )
    .unwrap();
    let before = read_bytes(&settings);
    let (_, secrets) = FakeKeyring::available();
    let report = apply_marketplace_name_migration(
        &home,
        Some(&env),
        Some(&project),
        MarketplaceMigrationOptions::default(),
        &secrets,
    )
    .unwrap();
    assert_eq!(report.status(), MigrationStatus::Partial);
    assert!(report
        .manual
        .iter()
        .any(|m| m.effect == "will-be-inactive" && m.file == settings.to_string_lossy()));
    assert_eq!(before, read_bytes(&settings));
}

#[test]
fn non_kebab_old_name_is_blocked_zero_write() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    seed_marketplace(&home, "old_name", "team-tools", &[]);
    let before = read_bytes(&known_marketplaces_path(Some(&home), Some(&env)));
    let (_, secrets) = FakeKeyring::available();
    let report = apply_marketplace_name_migration(
        &home,
        Some(&env),
        None,
        MarketplaceMigrationOptions::default(),
        &secrets,
    )
    .unwrap();
    let item = report
        .items
        .iter()
        .find(|i| i.old_name == "old_name")
        .unwrap();
    assert_eq!(item.action, MigrationAction::Unresolved);
    assert_eq!(item.reason, Some(MigrationReason::UnsafeOldName));
    assert!(report.backup.is_none());
    assert_eq!(
        before,
        read_bytes(&known_marketplaces_path(Some(&home), Some(&env)))
    );
}

#[test]
fn invalid_intent_pid_is_blocked_without_rename_backup() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    seed_marketplace(&home, "old-name", "team-tools", &[]);
    let intent_path = installed_plugins_intent_path(Some(&home), Some(&env));
    fs::write(
        &intent_path,
        serde_json::to_string(&json!({"installedPlugins": ["alpha@old_name"]})).unwrap(),
    )
    .unwrap();
    let before = read_bytes(&intent_path);
    let (_, secrets) = FakeKeyring::available();
    let report = apply_marketplace_name_migration(
        &home,
        Some(&env),
        None,
        MarketplaceMigrationOptions::default(),
        &secrets,
    )
    .unwrap();
    assert_eq!(report.status(), MigrationStatus::Partial);
    assert!(report.corrupt.is_some());
    assert!(report.backup.is_none());
    assert_eq!(before, read_bytes(&intent_path));
    assert!(home.join("marketplace/old-name").is_dir());
}

#[test]
fn secret_ops_set_before_delete() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    seed_marketplace(&home, "old-name", "team-tools", &[("alpha", Some("token"))]);
    seed_intent(&home, &["alpha@old-name"]);
    let (fake, secrets) = FakeKeyring::available();
    fake.set_value("alpha@old-name/token", "s3cret");
    apply_marketplace_name_migration(
        &home,
        Some(&env),
        None,
        MarketplaceMigrationOptions::default(),
        &secrets,
    )
    .unwrap();
    let ops = fake.ops();
    let last_set = ops.iter().rposition(|op| op.starts_with("set:")).unwrap();
    let first_delete = ops.iter().position(|op| op.starts_with("delete:")).unwrap();
    assert!(
        last_set < first_delete,
        "all new secrets must be set before any old delete: {ops:?}"
    );
}

#[test]
fn secret_midway_failure_cleans_new_keys_and_keeps_old() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    // 两个 password input，第二个 set 失败。
    let catalog = home.join("marketplace/old-name");
    fs::create_dir_all(catalog.join(".tfrobot-plugin")).unwrap();
    fs::write(
        catalog.join(".tfrobot-plugin/marketplace.json"),
        serde_json::to_string(&json!({
            "name": "team-tools",
            "plugins": [{"name": "alpha", "source": "./plugins/alpha"}]
        }))
        .unwrap(),
    )
    .unwrap();
    let inputs = catalog.join("plugins/alpha/mcp-servers");
    fs::create_dir_all(&inputs).unwrap();
    fs::write(
        inputs.join("inputs.json"),
        serde_json::to_string(&json!({
            "inputs": [
                {"type": "PromptString", "id": "token_a", "description": "a", "password": true},
                {"type": "PromptString", "id": "token_b", "description": "b", "password": true}
            ]
        }))
        .unwrap(),
    )
    .unwrap();
    update_known_marketplaces(
        |file| {
            file.account.marketplaces.insert(
                "old-name".to_string(),
                KnownMarketplaceEntry {
                    source: source("https://example.com/org/repo.git"),
                    manifest_name_baseline: None,
                    extra: Map::new(),
                },
            );
        },
        Some(&home),
        None,
    )
    .unwrap();
    seed_intent(&home, &["alpha@old-name"]);
    let (fake, secrets) = FakeKeyring::available();
    fake.set_value("alpha@old-name/token_a", "value-a");
    fake.set_value("alpha@old-name/token_b", "value-b");
    fake.fail_set_for("alpha@team-tools/token_b");

    let report = apply_marketplace_name_migration(
        &home,
        Some(&env),
        None,
        MarketplaceMigrationOptions::default(),
        &secrets,
    )
    .unwrap();
    let item = report
        .items
        .iter()
        .find(|i| i.old_name == "old-name")
        .unwrap();
    assert_eq!(item.action, MigrationAction::Blocked);
    assert_eq!(item.reason, Some(MigrationReason::KeyringUnavailable));
    // 旧键保留；本次已 set 的新键被清理；目录/账本未迁移。
    assert_eq!(
        fake.get_value("alpha@old-name/token_a").as_deref(),
        Some("value-a")
    );
    assert_eq!(
        fake.get_value("alpha@old-name/token_b").as_deref(),
        Some("value-b")
    );
    assert!(fake.get_value("alpha@team-tools/token_a").is_none());
    assert!(fake.get_value("alpha@team-tools/token_b").is_none());
    assert!(home.join("marketplace/old-name").is_dir());
    assert!(!home.join("marketplace/team-tools").exists());
}

#[test]
fn secret_count_excludes_absent_values() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    seed_marketplace(&home, "old-name", "team-tools", &[("alpha", Some("token"))]);
    let (_, secrets) = FakeKeyring::available(); // 无旧 secret
    let plan = plan_marketplace_name_migration(
        &home,
        Some(&env),
        None,
        MarketplaceMigrationOptions::default(),
        &secrets,
    )
    .unwrap();
    let item = plan
        .items
        .iter()
        .find(|i| i.old_name == "old-name")
        .unwrap();
    assert_eq!(item.secrets, 0, "absent old values are not planned rekeys");
}

#[test]
fn structurally_corrupt_known_is_blocked_zero_write() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    let known_path = known_marketplaces_path(Some(&home), Some(&env));
    fs::write(
        &known_path,
        serde_json::to_string(&json!({"marketplaces": {"a": 1}})).unwrap(),
    )
    .unwrap();
    let before = read_bytes(&known_path);
    let (_, secrets) = FakeKeyring::available();
    let report = apply_marketplace_name_migration(
        &home,
        Some(&env),
        None,
        MarketplaceMigrationOptions::default(),
        &secrets,
    )
    .unwrap();
    assert_eq!(report.status(), MigrationStatus::Partial);
    assert!(report.corrupt.is_some());
    assert!(!report.written);
    assert_eq!(before, read_bytes(&known_path));
}

#[test]
fn local_only_intent_ids_are_preserved() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    seed_marketplace(&home, "old-name", "team-tools", &[]);
    // local-only 可能只存在于 ledger（boot 迁移产物）。
    seed_ledger(&home, "local-only", "/tmp/local-only", "user", None);
    let intent_path = installed_plugins_intent_path(Some(&home), Some(&env));
    fs::write(
        &intent_path,
        serde_json::to_string(&json!({
            "installedPlugins": ["local-only", "alpha@old-name"]
        }))
        .unwrap(),
    )
    .unwrap();
    let (_, secrets) = FakeKeyring::available();
    let report = apply_marketplace_name_migration(
        &home,
        Some(&env),
        None,
        MarketplaceMigrationOptions::default(),
        &secrets,
    )
    .unwrap();
    assert_eq!(report.status(), MigrationStatus::Ok, "{report:?}");
    let intent = read_json(&intent_path);
    let ids = intent["installedPlugins"].as_array().unwrap();
    assert!(
        ids.iter().any(|v| v == "local-only"),
        "local-only id lost: {ids:?}"
    );
    assert!(ids.iter().any(|v| v == "alpha@team-tools"));
    assert!(!ids.iter().any(|v| v == "alpha@old-name"));
}

#[test]
fn pre_v03_seed_backfills_user_enabled_plugins() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    seed_marketplace(&home, "old-name", "team-tools", &[("alpha", None)]);
    let path = home
        .join("marketplace/old-name/plugins/alpha")
        .to_string_lossy()
        .into_owned();
    seed_ledger(&home, "alpha@old-name", &path, "user", None);
    // pre-v0.3.0：无 intent、无 enabledPlugins。
    assert!(!installed_plugins_intent_path(Some(&home), Some(&env)).exists());
    let (_, secrets) = FakeKeyring::available();
    apply_marketplace_name_migration(
        &home,
        Some(&env),
        None,
        MarketplaceMigrationOptions::default(),
        &secrets,
    )
    .unwrap();
    let user = read_json(&user_settings_path(Some(&env)));
    // seed 回填 user-scope true（否则 boot 因 intent 已存在而跳过回填 → 静默熄灯）。
    assert_eq!(user["enabledPlugins"]["alpha@team-tools"], true);
    assert!(user["enabledPlugins"].get("alpha@old-name").is_none());
}
