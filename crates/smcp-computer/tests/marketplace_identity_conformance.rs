//! Marketplace 身份唯一性 conformance（#272）。
//!
//! 协议依据：`a2c-smcp-protocol` `docs/specification/computer-management/conformance-tests.md` §5
//! 「Marketplace 身份唯一性」①–⑥ + `runtime-contract.md` §5.14。全部通过**公开** `add_marketplace` /
//! `refresh_marketplaces` / `remove_marketplace` 操作构造，不检查私有 registry。
//!
//! 夹具：本地 `file://` git 仓库（同 `tests/common/cli_binary.rs` 的 hermetic 手法），无真实远端。

use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::{json, Map, Value};
use tempfile::TempDir;

use smcp_computer::settings::scope::EnvMap;
use smcp_computer::settings::store::{
    load_known_marketplaces, update_installed_plugins_intent, update_known_marketplaces,
};
use smcp_computer::settings::{
    add_marketplace, refresh_marketplaces, remove_marketplace, AddMarketplaceParams,
    GovernanceError, KnownMarketplaceEntry, RefreshDiagnosticCode, RefreshStatus,
    RemoveMarketplaceParams,
};
use smcp_computer::skills::{stage_marketplace_skills, MarketplaceStageOptions, SkillRegistry};

// ---------------------------------------------------------------------------
// git 夹具 / git fixtures
// ---------------------------------------------------------------------------
fn git(args: &[&str], cwd: &Path) {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", cwd)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("git available");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn write_manifest(repo: &Path, marketplace_name: Option<&str>) {
    let mut manifest = json!({
        "plugins": [{"name": "alpha", "source": "./plugins/alpha"}],
    });
    if let Some(name) = marketplace_name {
        manifest["name"] = json!(name);
    }
    fs::create_dir_all(repo.join(".tfrobot-plugin")).unwrap();
    fs::write(
        repo.join(".tfrobot-plugin/marketplace.json"),
        serde_json::to_string(&manifest).unwrap(),
    )
    .unwrap();
}

fn write_skill(repo: &Path, description: &str) {
    let skill = repo.join("plugins/alpha/skills/code-review");
    fs::create_dir_all(&skill).unwrap();
    fs::write(
        skill.join("SKILL.md"),
        format!("---\nname: code-review\ndescription: {description}\n---\nbody"),
    )
    .unwrap();
}

/// 构造 marketplace 源仓库（`name` 为顶层清单名；`None` = 清单缺 name）。
fn build_repo(repo: &Path, marketplace_name: Option<&str>, description: &str) -> String {
    write_manifest(repo, marketplace_name);
    write_skill(repo, description);
    git(&["init", "-q"], repo);
    git(&["add", "-A"], repo);
    git(&["commit", "-qm", "init"], repo);
    format!("file://{}", repo.display())
}

/// 上游提交一次新内容（清单名 / 描述变化）/ commit an upstream change.
fn commit_update(repo: &Path, marketplace_name: Option<&str>, description: &str) {
    write_manifest(repo, marketplace_name);
    write_skill(repo, description);
    git(&["add", "-A"], repo);
    git(&["commit", "-qm", "update"], repo);
}

fn home_of(td: &TempDir) -> std::path::PathBuf {
    let home = td.path().join("home");
    fs::create_dir_all(&home).unwrap();
    home
}

/// 隔离的 user-config env（`enabledPlugins` 引用守卫不得读宿主 `~/.config`）。
fn env_of(td: &TempDir) -> EnvMap {
    std::iter::once((
        "XDG_CONFIG_HOME".to_string(),
        td.path().join("xdg").to_string_lossy().into_owned(),
    ))
    .collect()
}

fn source_of(url: &str) -> Value {
    json!({"type": "git", "url": url})
}

fn clone_dir(home: &Path, name: &str) -> std::path::PathBuf {
    home.join("marketplace").join(name)
}

fn skill_md(home: &Path, name: &str) -> String {
    fs::read_to_string(clone_dir(home, name).join("plugins/alpha/skills/code-review/SKILL.md"))
        .unwrap_or_default()
}

fn head_sha(dir: &Path) -> String {
    let out = Command::new("git")
        .args(["-C", dir.to_str().unwrap(), "rev-parse", "HEAD"])
        .output()
        .expect("git available");
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn staging_is_empty(home: &Path) -> bool {
    home.join("marketplace/.staging")
        .read_dir()
        .map(|mut d| d.next().is_none())
        .unwrap_or(true)
}

fn entry(home: &Path, name: &str) -> Option<KnownMarketplaceEntry> {
    load_known_marketplaces(Some(home), None)
        .account
        .marketplaces
        .get(name)
        .cloned()
}

/// 播种一条 legacy 注册：clone 就位 + 账本无 `manifestNameBaseline`。
async fn seed_legacy_registration(home: &Path, registered_name: &str, url: &str) {
    let mut throwaway = SkillRegistry::new();
    stage_marketplace_skills(
        registered_name,
        &source_of(url),
        &mut throwaway,
        home,
        MarketplaceStageOptions::default(),
    )
    .await;
    let name = registered_name.to_string();
    let source = source_of(url);
    update_known_marketplaces(
        move |file| {
            file.account.marketplaces.insert(
                name,
                KnownMarketplaceEntry {
                    source,
                    manifest_name_baseline: None,
                    extra: Map::new(),
                },
            );
        },
        Some(home),
        None,
    )
    .unwrap();
}

// ===========================================================================
// conformance §5 ①–⑥
// ===========================================================================

/// ① 先后 add 两个不同 source 且注册名相同 → 第二次 MUST 以 validation 失败；既有注册 / 物化内容 /
/// 运行时能力保持不变。
#[tokio::test]
async fn conformance_1_same_name_different_source_rejected_first_unchanged() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    let repo_a = td.path().join("repo-a");
    let repo_b = td.path().join("repo-b");
    let url_a = build_repo(&repo_a, Some("team-tools"), "from-a");
    let url_b = build_repo(&repo_b, Some("team-tools"), "from-b");
    let mut reg = SkillRegistry::new();

    add_marketplace(
        &mut reg,
        &home,
        Some(&env),
        &url_a,
        AddMarketplaceParams::default(),
    )
    .await
    .unwrap();

    let err = add_marketplace(
        &mut reg,
        &home,
        Some(&env),
        &url_b,
        AddMarketplaceParams::default(),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&err, GovernanceError::DuplicateMarketplace(name) if name == "team-tools"),
        "{err:?}"
    );

    // 既有注册 / 内容 / 能力不变。
    let ledger = load_known_marketplaces(Some(&home), None).account;
    assert_eq!(ledger.marketplaces.len(), 1);
    assert_eq!(
        ledger.marketplaces["team-tools"].source["url"].as_str(),
        Some(url_a.as_str())
    );
    assert!(skill_md(&home, "team-tools").contains("from-a"));
    let reference = reg.resolve("alpha:code-review").unwrap();
    assert_eq!(reference.description, "from-a");
    assert_eq!(reference.source, "marketplace:team-tools");
    // 错误面不回显被拒 source。
    let rendered = format!("{err}\n{err:?}");
    assert!(!rendered.contains(&url_b), "{rendered}");
}

/// ② 同源重复 add MUST 幂等：不产生第二个注册、不替换内容。
#[tokio::test]
async fn conformance_2_same_source_repeat_add_is_idempotent() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    let repo = td.path().join("repo");
    let url = build_repo(&repo, Some("team-tools"), "from-a");
    let mut reg = SkillRegistry::new();

    let first = add_marketplace(
        &mut reg,
        &home,
        Some(&env),
        &url,
        AddMarketplaceParams::default(),
    )
    .await
    .unwrap();
    assert!(first.changed);
    let head = head_sha(&clone_dir(&home, "team-tools"));

    let again = add_marketplace(
        &mut reg,
        &home,
        Some(&env),
        &url,
        AddMarketplaceParams::default(),
    )
    .await
    .unwrap();
    assert!(!again.changed);
    assert!(again.skills.is_empty());
    assert_eq!(again.name, "team-tools");
    let ledger = load_known_marketplaces(Some(&home), None).account;
    assert_eq!(ledger.marketplaces.len(), 1);
    assert_eq!(head_sha(&clone_dir(&home, "team-tools")), head);
}

/// ③ 已注册 source refresh 后 name 变化 → structured validation（Rejected，MUST NOT 报 unchanged）；
/// 旧注册 / 旧内容 / 旧能力保留，引用不被静默重写。
#[tokio::test]
async fn conformance_3_refresh_rename_rejected_and_old_state_preserved() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    let repo = td.path().join("repo");
    let url = build_repo(&repo, Some("team-tools"), "from-a");
    let mut reg = SkillRegistry::new();
    add_marketplace(
        &mut reg,
        &home,
        Some(&env),
        &url,
        AddMarketplaceParams::default(),
    )
    .await
    .unwrap();
    let head = head_sha(&clone_dir(&home, "team-tools"));

    commit_update(&repo, Some("team-tools-v2"), "from-a-v2");
    let rows = refresh_marketplaces(&mut reg, &home, Some(&env), "all").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, RefreshStatus::Rejected);
    let diagnostic = rows[0]
        .diagnostic
        .as_ref()
        .expect("rejected row diagnostic");
    assert_eq!(diagnostic.code, RefreshDiagnosticCode::ManifestNameChanged);
    assert!(
        diagnostic.message.contains("team-tools-v2"),
        "{diagnostic:?}"
    );

    // 旧注册 / 旧内容 / 旧能力保留；未发生 pull（HEAD 不变）。
    let rec = entry(&home, "team-tools").expect("registration kept");
    assert_eq!(rec.manifest_name_baseline.as_deref(), Some("team-tools"));
    assert_eq!(head_sha(&clone_dir(&home, "team-tools")), head);
    assert!(skill_md(&home, "team-tools").contains("from-a"));
    assert_eq!(
        reg.resolve("alpha:code-review").unwrap().description,
        "from-a"
    );
}

/// ④ remove 后以同名不同源 re-add，且仍存在 `<plugin>@<name>` 意图引用 → MUST 拒绝；
/// 清除引用后 re-add 才是新注册。
#[tokio::test]
async fn conformance_4_readd_with_dangling_references_rejected() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    let repo_a = td.path().join("repo-a");
    let repo_b = td.path().join("repo-b");
    let url_a = build_repo(&repo_a, Some("team-tools"), "from-a");
    let url_b = build_repo(&repo_b, Some("team-tools"), "from-b");
    let mut reg = SkillRegistry::new();
    add_marketplace(
        &mut reg,
        &home,
        Some(&env),
        &url_a,
        AddMarketplaceParams::default(),
    )
    .await
    .unwrap();
    update_installed_plugins_intent(
        |intent| {
            intent
                .account
                .installed_plugins
                .insert("alpha@team-tools".to_string());
        },
        Some(&home),
        None,
    )
    .unwrap();
    remove_marketplace(
        &mut reg,
        &home,
        Some(&env),
        "team-tools",
        RemoveMarketplaceParams {
            keep_plugins: true,
            hooks: None,
        },
        &HashSet::new(),
    )
    .await
    .unwrap();

    let err = add_marketplace(
        &mut reg,
        &home,
        Some(&env),
        &url_b,
        AddMarketplaceParams::default(),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            &err,
            GovernanceError::MarketplaceNameReferenced { name, plugin_ids }
                if name == "team-tools" && plugin_ids.iter().any(|p| p == "alpha@team-tools")
        ),
        "{err:?}"
    );
    assert!(entry(&home, "team-tools").is_none());

    // 显式移除引用后，re-add 是新注册。
    update_installed_plugins_intent(
        |intent| {
            intent
                .account
                .installed_plugins
                .shift_remove("alpha@team-tools");
        },
        Some(&home),
        None,
    )
    .unwrap();
    add_marketplace(
        &mut reg,
        &home,
        Some(&env),
        &url_b,
        AddMarketplaceParams::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        entry(&home, "team-tools").unwrap().source["url"].as_str(),
        Some(url_b.as_str())
    );
}

/// ⑤ 同一仓库的等价写法在 canonical 无法判定为同源时 MUST fail-closed 拒绝（不要求归一）。
#[tokio::test]
async fn conformance_5_unverifiable_equivalent_sources_fail_closed() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    let repo = td.path().join("repo");
    let url_plain = build_repo(&repo, Some("team-tools"), "from-a");
    let url_localhost = format!("file://localhost{}", repo.display());
    let mut reg = SkillRegistry::new();
    add_marketplace(
        &mut reg,
        &home,
        Some(&env),
        &url_plain,
        AddMarketplaceParams::default(),
    )
    .await
    .unwrap();

    let err = add_marketplace(
        &mut reg,
        &home,
        Some(&env),
        &url_localhost,
        AddMarketplaceParams::default(),
    )
    .await
    .unwrap_err();
    // 不同写法无法证明同源 → fail-closed 拒绝（不要求归一为同源）。
    assert!(
        matches!(&err, GovernanceError::DuplicateMarketplace(name) if name == "team-tools"),
        "{err:?}"
    );
    // 既有注册不变。
    assert_eq!(
        entry(&home, "team-tools").unwrap().source["url"].as_str(),
        Some(url_plain.as_str())
    );
}

/// ⑥ 无本地清单且无可用缓存的 `no_clone` / 离线注册 SHOULD 以 validation 拒绝。
#[tokio::test]
async fn conformance_6_no_clone_without_catalog_is_validation() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    let repo = td.path().join("repo");
    let url = build_repo(&repo, Some("team-tools"), "from-a");
    let mut reg = SkillRegistry::new();

    let err = add_marketplace(
        &mut reg,
        &home,
        Some(&env),
        &url,
        AddMarketplaceParams {
            name: Some("team-tools"),
            no_clone: true,
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            &err,
            GovernanceError::Validation(
                smcp_computer::skills::MarketplaceStageError::ManifestUnreadable
            )
        ),
        "{err:?}"
    );
    assert!(entry(&home, "team-tools").is_none());
    assert!(!clone_dir(&home, "team-tools").exists());
    assert!(staging_is_empty(&home));
}

// ===========================================================================
// #272 验收补充
// ===========================================================================

/// ④ 补充：`enabledPlugins`（user settings）引用同样触发守卫（不止 install intent）。
#[tokio::test]
async fn readd_blocked_by_enabled_plugins_reference() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    let repo_a = td.path().join("repo-a");
    let repo_b = td.path().join("repo-b");
    let url_a = build_repo(&repo_a, Some("team-tools"), "from-a");
    let url_b = build_repo(&repo_b, Some("team-tools"), "from-b");

    seed_legacy_registration(&home, "team-tools", &url_a).await;
    // user settings 中残留 enabledPlugins 引用（未安装也可存在）。
    let settings_path = td.path().join("xdg/a2c/settings.json");
    fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
    fs::write(
        &settings_path,
        r#"{"enabledPlugins": {"alpha@team-tools": true}}"#,
    )
    .unwrap();
    let mut reg = SkillRegistry::new();
    remove_marketplace(
        &mut reg,
        &home,
        Some(&env),
        "team-tools",
        RemoveMarketplaceParams {
            keep_plugins: true,
            hooks: None,
        },
        &HashSet::new(),
    )
    .await
    .unwrap();

    let err = add_marketplace(
        &mut reg,
        &home,
        Some(&env),
        &url_b,
        AddMarketplaceParams::default(),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            &err,
            GovernanceError::MarketplaceNameReferenced { name, plugin_ids }
                if name == "team-tools" && plugin_ids.iter().any(|p| p == "alpha@team-tools")
        ),
        "{err:?}"
    );
}

/// 仓库名 ≠ 清单名 → 注册名、clone 目录、skill source 全用清单名。
#[tokio::test]
async fn add_uses_manifest_name_not_repo_slug() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    let repo = td.path().join("weird-repo");
    let url = build_repo(&repo, Some("team-tools"), "from-a");
    let mut reg = SkillRegistry::new();

    let outcome = add_marketplace(
        &mut reg,
        &home,
        Some(&env),
        &url,
        AddMarketplaceParams::default(),
    )
    .await
    .unwrap();
    assert_eq!(outcome.name, "team-tools");
    assert!(!clone_dir(&home, "weird-repo").exists());
    assert!(clone_dir(&home, "team-tools").is_dir());
    let reference = reg.resolve("alpha:code-review").unwrap();
    assert_eq!(reference.source, "marketplace:team-tools");
    assert_eq!(
        entry(&home, "team-tools")
            .unwrap()
            .manifest_name_baseline
            .as_deref(),
        Some("team-tools")
    );
}

/// 显式 `--name` = 期望清单名校验；不匹配 → validation 且零残留。
#[tokio::test]
async fn explicit_expected_name_mismatch_fails_with_zero_residue() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    let repo = td.path().join("repo");
    let url = build_repo(&repo, Some("team-tools"), "from-a");
    let mut reg = SkillRegistry::new();

    let err = add_marketplace(
        &mut reg,
        &home,
        Some(&env),
        &url,
        AddMarketplaceParams {
            name: Some("other-name"),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            &err,
            GovernanceError::Validation(
                smcp_computer::skills::MarketplaceStageError::ExpectedNameMismatch {
                    expected,
                    actual
                }
            ) if expected == "other-name" && actual == "team-tools"
        ),
        "{err:?}"
    );
    assert!(load_known_marketplaces(Some(&home), None)
        .account
        .marketplaces
        .is_empty());
    assert!(!clone_dir(&home, "team-tools").exists());
    assert!(staging_is_empty(&home));
}

/// 清单 name 缺失 / 非法 → validation（不回退 URL 派生名），零残留。
#[tokio::test]
async fn missing_or_invalid_manifest_name_is_validation() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    let long = "a".repeat(65);
    for (idx, manifest_name) in [None, Some("Bad_Name"), Some("a--b"), Some(long.as_str())]
        .into_iter()
        .enumerate()
    {
        let repo = td.path().join(format!("repo-{idx}"));
        let url = build_repo(&repo, manifest_name, "from-a");
        let mut reg = SkillRegistry::new();
        let err = add_marketplace(
            &mut reg,
            &home,
            Some(&env),
            &url,
            AddMarketplaceParams::default(),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(
                &err,
                GovernanceError::Validation(
                    smcp_computer::skills::MarketplaceStageError::ManifestNameMissing
                        | smcp_computer::skills::MarketplaceStageError::ManifestNameInvalid
                )
            ),
            "case {idx} ({manifest_name:?}): {err:?}"
        );
        assert!(load_known_marketplaces(Some(&home), None)
            .account
            .marketplaces
            .is_empty());
        assert!(staging_is_empty(&home));
    }
}

/// legacy 注册（无基线）首次成功 refresh：记录基线、保留旧名 + advisory；之后再改名 → Rejected。
#[tokio::test]
async fn legacy_registration_records_baseline_then_rejects_rename() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    let repo = td.path().join("repo");
    let url = build_repo(&repo, Some("team-tools"), "from-a");
    seed_legacy_registration(&home, "plugins", &url).await;

    let mut reg = SkillRegistry::new();
    let rows = refresh_marketplaces(&mut reg, &home, Some(&env), "all").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, RefreshStatus::Unchanged);
    let diagnostic = rows[0].diagnostic.as_ref().expect("legacy advisory");
    assert_eq!(
        diagnostic.code,
        RefreshDiagnosticCode::LegacyManifestNameMismatch
    );
    let rec = entry(&home, "plugins").expect("legacy name kept");
    assert_eq!(rec.manifest_name_baseline.as_deref(), Some("team-tools"));

    // 基线建立后再改名 → Rejected，旧名保留。
    commit_update(&repo, Some("team-tools-v2"), "from-a-v2");
    let rows = refresh_marketplaces(&mut reg, &home, Some(&env), "all").await;
    assert_eq!(rows[0].status, RefreshStatus::Rejected);
    assert_eq!(
        rows[0].diagnostic.as_ref().unwrap().code,
        RefreshDiagnosticCode::ManifestNameChanged
    );
    assert!(entry(&home, "plugins").is_some());
    assert!(entry(&home, "team-tools-v2").is_none());
}

/// legacy refresh 遇清单 name 缺失 → 保留旧名 + 诊断，刷新照常提交。
#[tokio::test]
async fn legacy_refresh_with_missing_name_keeps_old_name_with_diagnostic() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    let repo = td.path().join("repo");
    let url = build_repo(&repo, Some("team-tools"), "from-a");
    seed_legacy_registration(&home, "plugins", &url).await;

    commit_update(&repo, None, "from-a-v2");
    let mut reg = SkillRegistry::new();
    let rows = refresh_marketplaces(&mut reg, &home, Some(&env), "all").await;
    assert_eq!(rows[0].status, RefreshStatus::Updated);
    let diagnostic = rows[0].diagnostic.as_ref().expect("missing-name advisory");
    assert_eq!(
        diagnostic.code,
        RefreshDiagnosticCode::LegacyManifestNameMissing
    );
    assert!(diagnostic.message.contains("missing"), "{diagnostic:?}");
    let rec = entry(&home, "plugins").expect("legacy name kept");
    assert!(rec.manifest_name_baseline.is_none());
    assert!(skill_md(&home, "plugins").contains("from-a-v2"));
}

/// 同源以旧名 add → 拒绝（一源不多名）。
#[tokio::test]
async fn same_source_under_old_name_add_is_rejected() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    let repo = td.path().join("repo");
    let url = build_repo(&repo, Some("team-tools"), "from-a");
    seed_legacy_registration(&home, "plugins", &url).await;

    let mut reg = SkillRegistry::new();
    let err = add_marketplace(
        &mut reg,
        &home,
        Some(&env),
        &url,
        AddMarketplaceParams::default(),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&err, GovernanceError::SourceAlreadyRegistered { existing } if existing == "plugins"),
        "{err:?}"
    );
    assert!(entry(&home, "team-tools").is_none());
}

/// 同源内容更新（注册名不变）→ Updated 且内容替换到新 commit。
#[tokio::test]
async fn refresh_same_name_updates_content() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    let repo = td.path().join("repo");
    let url = build_repo(&repo, Some("team-tools"), "from-a");
    let mut reg = SkillRegistry::new();
    add_marketplace(
        &mut reg,
        &home,
        Some(&env),
        &url,
        AddMarketplaceParams::default(),
    )
    .await
    .unwrap();

    commit_update(&repo, Some("team-tools"), "from-a-v2");
    let rows = refresh_marketplaces(&mut reg, &home, Some(&env), "all").await;
    assert_eq!(rows[0].status, RefreshStatus::Updated);
    assert!(rows[0].diagnostic.is_none());
    assert!(skill_md(&home, "team-tools").contains("from-a-v2"));
    assert_eq!(
        reg.resolve("alpha:code-review").unwrap().description,
        "from-a-v2"
    );
}

/// fetch 失败回退 staged clone 且远端未变 → `unchanged`（不是 `updated`）。
#[tokio::test]
async fn refresh_staged_fallback_same_commit_is_unchanged() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    let repo = td.path().join("repo");
    let url = build_repo(&repo, Some("team-tools"), "from-a");
    let mut reg = SkillRegistry::new();
    add_marketplace(
        &mut reg,
        &home,
        Some(&env),
        &url,
        AddMarketplaceParams::default(),
    )
    .await
    .unwrap();
    // 破坏 fetch（移除 origin）→ 走 staged clone 回退；远端未变 → 同 HEAD → unchanged。
    let clone = clone_dir(&home, "team-tools");
    git(
        &["-C", clone.to_str().unwrap(), "remote", "remove", "origin"],
        td.path(),
    );
    let rows = refresh_marketplaces(&mut reg, &home, Some(&env), "all").await;
    assert_eq!(rows[0].status, RefreshStatus::Unchanged);
}

/// 来料清单存在但损坏 → 旧内容保留，行状态 `unchanged` 但必须带 `manifest_unreadable` 诊断。
#[tokio::test]
async fn refresh_corrupt_incoming_manifest_surfaces_diagnostic() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    let repo = td.path().join("repo");
    let url = build_repo(&repo, Some("team-tools"), "from-a");
    let mut reg = SkillRegistry::new();
    add_marketplace(
        &mut reg,
        &home,
        Some(&env),
        &url,
        AddMarketplaceParams::default(),
    )
    .await
    .unwrap();
    fs::write(repo.join(".tfrobot-plugin/marketplace.json"), "{ not json").unwrap();
    git(&["add", "-A"], &repo);
    git(&["commit", "-qm", "corrupt"], &repo);
    let rows = refresh_marketplaces(&mut reg, &home, Some(&env), "all").await;
    assert_eq!(rows[0].status, RefreshStatus::Unchanged);
    assert_eq!(
        rows[0].diagnostic.as_ref().expect("diagnostic").code,
        RefreshDiagnosticCode::ManifestUnreadable
    );
    assert!(skill_md(&home, "team-tools").contains("from-a"));
}

/// `no_clone` 同源证明接受 SSH→HTTPS 回退等价（clone origin 为 https、请求为 ssh）。
#[tokio::test]
async fn no_clone_accepts_ssh_to_https_fallback_origin() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    let repo = td.path().join("repo");
    let url = build_repo(&repo, Some("team-tools"), "from-a");
    let mut reg = SkillRegistry::new();
    add_marketplace(
        &mut reg,
        &home,
        Some(&env),
        &url,
        AddMarketplaceParams::default(),
    )
    .await
    .unwrap();
    // 模拟「账本丢失但 catalog 仍在」+ clone 时 SSH 失败回退 HTTPS 的 origin。
    let ledger = smcp_computer::settings::store::known_marketplaces_path(Some(&home), Some(&env));
    fs::remove_file(&ledger).unwrap();
    let clone = clone_dir(&home, "team-tools");
    git(
        &[
            "-C",
            clone.to_str().unwrap(),
            "remote",
            "set-url",
            "origin",
            "https://example.com/org/repo.git",
        ],
        td.path(),
    );
    let outcome = add_marketplace(
        &mut reg,
        &home,
        Some(&env),
        "git@example.com:org/repo.git",
        AddMarketplaceParams {
            name: Some("team-tools"),
            no_clone: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(outcome.no_clone && outcome.created && outcome.changed);
}

/// 同源幂等 add 也应用 `--auto-update`（`flags_updated`），不静默丢弃用户意图。
#[tokio::test]
async fn idempotent_add_applies_auto_update_flag() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    let repo = td.path().join("repo");
    let url = build_repo(&repo, Some("team-tools"), "from-a");
    let mut reg = SkillRegistry::new();
    add_marketplace(
        &mut reg,
        &home,
        Some(&env),
        &url,
        AddMarketplaceParams::default(),
    )
    .await
    .unwrap();
    let again = add_marketplace(
        &mut reg,
        &home,
        Some(&env),
        &url,
        AddMarketplaceParams {
            auto_update: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(!again.changed);
    assert!(again.flags_updated);
    let rec = entry(&home, "team-tools").unwrap();
    assert_eq!(rec.extra.get("autoUpdate"), Some(&serde_json::json!(true)));
}

/// 🔴 回归：legacy + 上游清单损坏 → `unchanged` + `manifest_unreadable`，旧内容/旧能力保留。
#[tokio::test]
async fn legacy_refresh_corrupt_manifest_preserves_old_content() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    let repo = td.path().join("repo");
    let url = build_repo(&repo, Some("team-tools"), "from-a");
    seed_legacy_registration(&home, "plugins", &url).await;
    let clone = clone_dir(&home, "plugins");
    let head_before = head_sha(&clone);

    // 上游推送损坏清单。
    fs::write(repo.join(".tfrobot-plugin/marketplace.json"), "{ not json").unwrap();
    git(&["add", "-A"], &repo);
    git(&["commit", "-qm", "corrupt"], &repo);

    let mut reg = SkillRegistry::new();
    let rows = refresh_marketplaces(&mut reg, &home, Some(&env), "all").await;
    assert_eq!(rows[0].status, RefreshStatus::Unchanged);
    assert_eq!(
        rows[0].diagnostic.as_ref().expect("diagnostic").code,
        RefreshDiagnosticCode::ManifestUnreadable
    );
    // 旧 clone / 旧 SKILL / 旧注册原样保留。
    assert_eq!(head_sha(&clone), head_before);
    assert!(skill_md(&home, "plugins").contains("from-a"));
    let rec = entry(&home, "plugins").expect("legacy registration kept");
    assert!(rec.manifest_name_baseline.is_none());
}

/// legacy + 清单 name 非法 → 保留旧名 + `legacy_manifest_name_invalid` advisory，内容照常刷新。
#[tokio::test]
async fn legacy_refresh_invalid_name_advisory_kind() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    let repo = td.path().join("repo");
    let url = build_repo(&repo, Some("team-tools"), "from-a");
    seed_legacy_registration(&home, "plugins", &url).await;

    commit_update(&repo, Some("Bad_Name"), "from-a-v2");
    let mut reg = SkillRegistry::new();
    let rows = refresh_marketplaces(&mut reg, &home, Some(&env), "all").await;
    assert_eq!(rows[0].status, RefreshStatus::Updated);
    assert_eq!(
        rows[0].diagnostic.as_ref().unwrap().code,
        RefreshDiagnosticCode::LegacyManifestNameInvalid
    );
    assert!(skill_md(&home, "plugins").contains("from-a-v2"));
}

/// 自愈：既有注册、clone 目录丢失 → 同源 repeat add 重新物化（created=false, changed=true）。
#[tokio::test]
async fn same_source_repeat_add_restores_missing_clone() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    let repo = td.path().join("repo");
    let url = build_repo(&repo, Some("team-tools"), "from-a");
    let mut reg = SkillRegistry::new();
    add_marketplace(
        &mut reg,
        &home,
        Some(&env),
        &url,
        AddMarketplaceParams::default(),
    )
    .await
    .unwrap();
    fs::remove_dir_all(clone_dir(&home, "team-tools")).unwrap();

    let outcome = add_marketplace(
        &mut reg,
        &home,
        Some(&env),
        &url,
        AddMarketplaceParams::default(),
    )
    .await
    .unwrap();
    assert!(!outcome.created);
    assert!(outcome.changed);
    assert!(!outcome.skills.is_empty());
    assert!(clone_dir(&home, "team-tools").is_dir());
    assert!(skill_md(&home, "team-tools").contains("from-a"));
}

/// clone 失败（不可达源）→ `.staging` 无残留（失败零残留）。
#[tokio::test]
async fn clone_failure_leaves_no_staging_residue() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    let mut reg = SkillRegistry::new();
    let bad = format!("file://{}/nonexistent-repo.git", td.path().display());
    let err = add_marketplace(
        &mut reg,
        &home,
        Some(&env),
        &bad,
        AddMarketplaceParams::default(),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&err, GovernanceError::CloneFailed(name) if name.is_empty()),
        "{err:?}"
    );
    assert!(staging_is_empty(&home));
}

/// no_clone 同源回填不受 dangling 引用误杀（同源恢复既有绑定，不是换源）。
#[tokio::test]
async fn no_clone_same_source_refill_succeeds_with_dangling_refs() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let env = env_of(&td);
    let repo = td.path().join("repo");
    let url = build_repo(&repo, Some("team-tools"), "from-a");
    // 既有 catalog + 注册，再模拟「账本丢失但 catalog 与引用仍在」。
    seed_legacy_registration(&home, "team-tools", &url).await;
    update_installed_plugins_intent(
        |intent| {
            intent
                .account
                .installed_plugins
                .insert("alpha@team-tools".to_string());
        },
        Some(&home),
        Some(&env),
    )
    .unwrap();
    update_known_marketplaces(
        |file| {
            file.account.marketplaces.shift_remove("team-tools");
        },
        Some(&home),
        Some(&env),
    )
    .unwrap();

    let mut reg = SkillRegistry::new();
    let outcome = add_marketplace(
        &mut reg,
        &home,
        Some(&env),
        &url,
        AddMarketplaceParams {
            name: Some("team-tools"),
            no_clone: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(outcome.no_clone && outcome.created && outcome.changed);
    assert!(entry(&home, "team-tools").is_some());
}
