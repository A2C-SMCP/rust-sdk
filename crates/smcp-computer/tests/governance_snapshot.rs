//! #124 —— 高层 governance snapshot/inventory 集成守护（SDK 视角，外部 crate 编译）。
//!
//! 逐条映射验收：四类 plugin 状态、intent 权威、env/home 隔离、revision 稳定 + 事件可观察、
//! per-item 降级不清空、富字段回归。测试以 **外部 crate** 身份链接 `smcp_computer`，仅经公开面驱动。
//! 低层 seeding（store）在 SDK 集成测试中允许——**consumer 边界**由 `governance_consumer_boundary.rs` 单独守护。

use std::path::Path;
use std::time::Duration;

use serde_json::{json, Map, Value};
use tempfile::TempDir;

use smcp_computer::computer::{Computer, SilentSession};
use smcp_computer::settings::scope::{workdir_local_settings_path, EnvMap};
use smcp_computer::settings::store::{
    update_installed_plugins, update_installed_plugins_intent, update_known_marketplaces,
};
use smcp_computer::settings::{InstalledPluginRecord, KnownMarketplaceEntry};
use smcp_computer::{
    ComputerEvent, GovernanceDecision, ListPluginsOptions, MarketplaceStatus, PluginStatus,
};

// ---------------------------------------------------------------------------
// 脚手架 / harness
// ---------------------------------------------------------------------------

fn write(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

/// user-scope（XDG）锚到 `<tmp>/xdg`，隔离宿主 `~/.config`。
fn xdg_env(td: &TempDir) -> EnvMap {
    std::iter::once((
        "XDG_CONFIG_HOME".to_string(),
        td.path().join("xdg").to_string_lossy().into_owned(),
    ))
    .collect()
}

/// 一台注入全上下文的 Computer（skill_home=ledger/marketplaces 根、config_dir=project 锚、config_env=XDG）。
fn make_computer(td: &TempDir) -> Computer<SilentSession> {
    let env: EnvMap = xdg_env(td);
    Computer::new("c", SilentSession::new("s"), None, None, false, false)
        .with_skill_home(td.path().join("home"))
        .with_blob_cache_root(td.path().join("blob"))
        .with_config_dir(td.path().join("config"))
        .with_config_env(env)
}

fn home_of(td: &TempDir) -> std::path::PathBuf {
    td.path().join("home")
}
fn config_of(td: &TempDir) -> std::path::PathBuf {
    td.path().join("config")
}

/// 写一个 marketplace catalog（`<dir>/.tfrobot-plugin/marketplace.json`，列出 plugin 名）。
fn seed_catalog(dir: &Path, plugin_names: &[&str]) {
    let plugins: Vec<Value> = plugin_names.iter().map(|n| json!({ "name": n })).collect();
    write(
        &dir.join(".tfrobot-plugin").join("marketplace.json"),
        &serde_json::to_string(&json!({ "plugins": plugins })).unwrap(),
    );
}

fn ledger_record(
    install_path: Option<&Path>,
    version: &str,
    bundled: &[&str],
) -> InstalledPluginRecord {
    let mut extra = Map::new();
    extra.insert("version".to_string(), json!(version));
    extra.insert("scope".to_string(), json!("user"));
    InstalledPluginRecord {
        install_path: install_path.map(|p| p.to_string_lossy().into_owned()),
        bundled_mcp_servers: bundled.iter().map(|s| s.to_string()).collect(),
        extra,
    }
}

/// 播种「四类 plugin」fixture：mp catalog=[a,b,c,d]；intent={a,b,c}；enabled{a:true,b:false}；
/// a/b install_path 真实存在，c install_path 缺失 → Degraded；d 仅在 catalog → Available。
fn seed_four_classes(td: &TempDir) {
    let home = home_of(td);
    let config = config_of(td);
    let catalog = td.path().join("catalog");
    seed_catalog(&catalog, &["a", "b", "c", "d"]);
    let install_a = td.path().join("plugins").join("a");
    let install_b = td.path().join("plugins").join("b");
    std::fs::create_dir_all(&install_a).unwrap();
    std::fs::create_dir_all(&install_b).unwrap();

    update_known_marketplaces(
        |f| {
            let mut extra = Map::new();
            extra.insert(
                "installLocation".to_string(),
                json!(catalog.to_string_lossy()),
            );
            extra.insert("commitSha".to_string(), json!("deadbeef01"));
            extra.insert("autoUpdate".to_string(), json!(true));
            extra.insert("lastUpdated".to_string(), json!("2026-07-13T00:00:00Z"));
            f.account.marketplaces.insert(
                "mp".to_string(),
                KnownMarketplaceEntry {
                    source: json!({"type": "git", "url": "https://example.com/mp.git"}),
                    extra,
                },
            );
        },
        Some(&home),
        None,
    )
    .unwrap();

    update_installed_plugins_intent(
        |f| {
            f.account.installed_plugins.insert("a@mp".to_string());
            f.account.installed_plugins.insert("b@mp".to_string());
            f.account.installed_plugins.insert("c@mp".to_string());
        },
        Some(&home),
        None,
    )
    .unwrap();

    update_installed_plugins(
        |f| {
            f.account.plugins.insert(
                "a@mp".to_string(),
                vec![ledger_record(Some(&install_a), "1.0.0", &["srv-a"])],
            );
            f.account.plugins.insert(
                "b@mp".to_string(),
                vec![ledger_record(Some(&install_b), "2.0.0", &["srv-b"])],
            );
            // c: install_path 指向不存在目录 → per-item degraded。
            f.account.plugins.insert(
                "c@mp".to_string(),
                vec![ledger_record(Some(&td.path().join("gone")), "3.0.0", &[])],
            );
        },
        Some(&home),
        None,
    )
    .unwrap();

    // b@mp 故意**不**列入 enabledPlugins（absent → 默认 disabled）——留给 revision 测试的 enable(b@mp) 生效，
    // 避免 local `b@mp:false` 覆盖 enable 写入的 user-scope true 而使 revision 不变（scope 合并语义）。
    write(
        &workdir_local_settings_path(&config),
        r#"{"enabledPlugins": {"a@mp": true}}"#,
    );
}

// ---------------------------------------------------------------------------
// 1. 四类 plugin 状态齐全（同一 fixture）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn governance_snapshot_covers_four_plugin_classes() {
    let td = TempDir::new().unwrap();
    seed_four_classes(&td);
    let computer = make_computer(&td);

    let snap = computer.governance_snapshot().await.unwrap();

    let by_id = |id: &str| snap.plugins.iter().find(|p| p.id == id).cloned();
    assert_eq!(
        by_id("a@mp").unwrap().status,
        PluginStatus::InstalledEnabled
    );
    assert_eq!(
        by_id("b@mp").unwrap().status,
        PluginStatus::InstalledDisabled
    );
    let c = by_id("c@mp").unwrap();
    assert_eq!(c.status, PluginStatus::Degraded, "install_path 缺失应降级");
    assert!(
        !c.diagnostics.is_empty(),
        "降级项须带结构化 diagnostic，不得静默"
    );
    let d = by_id("d@mp").unwrap();
    assert_eq!(
        d.status,
        PluginStatus::Available,
        "catalog 有但未装 → available"
    );
    assert!(!d.installed);

    // 富字段回归：version / install_path / bundled_mcp。
    let a = by_id("a@mp").unwrap();
    assert_eq!(a.version.as_deref(), Some("1.0.0"));
    assert!(a.install_path.is_some());
    assert_eq!(a.bundled_mcp_servers, vec!["srv-a".to_string()]);
    assert!(a.installed && a.enabled);

    // marketplace 富字段。
    let mp = computer.get_marketplace("mp").await.unwrap().unwrap();
    assert_eq!(
        mp.status,
        MarketplaceStatus::Available,
        "已克隆 + catalog 可读"
    );
    assert_eq!(mp.commit_sha.as_deref(), Some("deadbeef01"));
    assert!(mp.auto_update);
    assert_eq!(mp.source_url.as_deref(), Some("https://example.com/mp.git"));
    assert_eq!(mp.decision, GovernanceDecision::Allowed);
    let mut avail = mp.available_plugin_ids.clone();
    avail.sort();
    assert_eq!(avail, vec!["a@mp", "b@mp", "c@mp", "d@mp"]);
    let mut owned = mp.plugin_ids.clone();
    owned.sort();
    assert_eq!(
        owned,
        vec!["a@mp", "b@mp", "c@mp"],
        "plugin_ids = intent 已装集"
    );

    // 查询整体非空（单损坏项未拖垮全表）。
    assert!(snap.plugins.len() >= 4);
    assert_eq!(snap.marketplaces.len(), 1);
}

// ---------------------------------------------------------------------------
// 2. intent 权威：陈旧 ledger 记录不得报为 installed
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stale_ledger_record_not_reported_installed() {
    let td = TempDir::new().unwrap();
    let home = home_of(td_ref(&td));
    // ledger 里有 z@mp，但 intent 空 → 不得 installed。
    update_installed_plugins(
        |f| {
            f.account.plugins.insert(
                "z@mp".to_string(),
                vec![ledger_record(Some(td.path()), "9.9.9", &[])],
            );
        },
        Some(&home),
        None,
    )
    .unwrap();
    let computer = make_computer(&td);

    let z = computer.get_plugin("z@mp").await.unwrap();
    if let Some(p) = z {
        assert_ne!(p.status, PluginStatus::InstalledEnabled);
        assert_ne!(p.status, PluginStatus::InstalledDisabled);
        assert!(!p.installed, "intent 未含 → 绝不 installed");
    }
    // 且不出现在 installed 列表里。
    let installed = computer
        .list_plugins(ListPluginsOptions::default())
        .await
        .unwrap();
    assert!(installed.iter().all(|p| p.id != "z@mp" || !p.installed));
}

fn td_ref(td: &TempDir) -> &TempDir {
    td
}

// ---------------------------------------------------------------------------
// 3. 两台 Computer 的 env/home 完全隔离
// ---------------------------------------------------------------------------

#[tokio::test]
async fn two_computers_are_context_isolated() {
    let td1 = TempDir::new().unwrap();
    let td2 = TempDir::new().unwrap();
    seed_four_classes(&td1); // 只有 td1 播种 mp
    let c1 = make_computer(&td1);
    let c2 = make_computer(&td2);

    let m1 = c1.list_marketplaces().await.unwrap();
    let m2 = c2.list_marketplaces().await.unwrap();
    assert_eq!(m1.len(), 1, "c1 看到自己的 mp");
    assert!(m2.is_empty(), "c2 不得看到 c1 的治理状态（无宿主回退）");
}

// ---------------------------------------------------------------------------
// 4. revision 稳定 + 生命周期变更后 revision/event 可观察
// ---------------------------------------------------------------------------

#[tokio::test]
async fn revision_stable_and_enable_is_observable() {
    let td = TempDir::new().unwrap();
    seed_four_classes(&td);
    let computer = make_computer(&td);
    computer.boot_up().await.unwrap();

    let r1 = computer.governance_snapshot().await.unwrap().revision;
    let r2 = computer.governance_snapshot().await.unwrap().revision;
    assert_eq!(r1, r2, "无变化 → revision 稳定");

    let mut events = computer.subscribe_events();
    // 启用 b@mp（此前 disabled）→ 声明式治理内容变。
    computer
        .enable_plugin("b@mp", Default::default(), None)
        .await
        .unwrap();

    let r3 = computer.governance_snapshot().await.unwrap().revision;
    assert_ne!(r1, r3, "enable 后治理内容变 → revision 变");

    // subscribe_events 收到 ConfigRevisionBumped。
    let got = wait_for_config_bump(&mut events).await;
    assert!(
        got,
        "enable_plugin 应经 subscribe_events 发 ConfigRevisionBumped"
    );

    computer.shutdown().await.unwrap();
}

async fn wait_for_config_bump(
    events: &mut tokio::sync::broadcast::Receiver<ComputerEvent>,
) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Ok(ComputerEvent::ConfigRevisionBumped { .. })) => return true,
            Ok(Ok(_)) => continue,
            _ => return false,
        }
    }
}

// ---------------------------------------------------------------------------
// 5. remove_marketplace（本次新增事件发射路径）亦可观察
// ---------------------------------------------------------------------------

#[tokio::test]
async fn remove_marketplace_is_observable() {
    let td = TempDir::new().unwrap();
    seed_four_classes(&td);
    let computer = make_computer(&td);
    computer.boot_up().await.unwrap();

    let mut events = computer.subscribe_events();
    let _ = computer
        .remove_marketplace(
            "mp",
            smcp_computer::settings::RemoveMarketplaceParams {
                keep_plugins: true,
                hooks: None,
            },
        )
        .await;
    let got = wait_for_config_bump(&mut events).await;
    assert!(
        got,
        "remove_marketplace 应发 ConfigRevisionBumped（#124 新增）"
    );

    computer.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// 6. per-item marketplace 降级不清空整表
// ---------------------------------------------------------------------------

#[tokio::test]
async fn corrupt_marketplace_degrades_not_empties() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    // mp1 catalog 正常；mp2 installLocation 指向无 catalog 的目录 → Degraded。
    let good = td.path().join("good");
    seed_catalog(&good, &["p1"]);
    let bad = td.path().join("bad"); // 存在但无 .tfrobot-plugin/marketplace.json
    std::fs::create_dir_all(&bad).unwrap();
    update_known_marketplaces(
        |f| {
            let mk = |loc: &Path| {
                let mut e = Map::new();
                e.insert("installLocation".to_string(), json!(loc.to_string_lossy()));
                KnownMarketplaceEntry {
                    source: json!({"type": "git", "url": "https://example.com/x.git"}),
                    extra: e,
                }
            };
            f.account.marketplaces.insert("mp1".to_string(), mk(&good));
            f.account.marketplaces.insert("mp2".to_string(), mk(&bad));
        },
        Some(&home),
        None,
    )
    .unwrap();
    let computer = make_computer(&td);

    let mps = computer.list_marketplaces().await.unwrap();
    assert_eq!(mps.len(), 2, "损坏项不得使查询整表变空");
    let mp2 = mps.iter().find(|m| m.name == "mp2").unwrap();
    assert_eq!(mp2.status, MarketplaceStatus::Degraded);
    assert!(!mp2.diagnostics.is_empty());
    let mp1 = mps.iter().find(|m| m.name == "mp1").unwrap();
    assert_eq!(mp1.status, MarketplaceStatus::Available);
}

// ---------------------------------------------------------------------------
// 7. add_marketplace / uninstall_plugin（本次新增的其余事件发射点）亦可观察
// ---------------------------------------------------------------------------

#[tokio::test]
async fn add_marketplace_is_observable() {
    let td = TempDir::new().unwrap();
    let computer = make_computer(&td);
    computer.boot_up().await.unwrap();

    let mut events = computer.subscribe_events();
    // no_clone：仅注册意图，离线可成功（不触网络）。
    let res = computer
        .add_marketplace(
            "https://example.com/acme.git",
            smcp_computer::settings::AddMarketplaceParams {
                name: Some("acme"),
                no_clone: true,
                ..Default::default()
            },
        )
        .await;
    assert!(res.is_ok(), "no_clone add_marketplace 应离线成功: {res:?}");
    assert!(
        wait_for_config_bump(&mut events).await,
        "add_marketplace 应发 ConfigRevisionBumped（#124 新增）"
    );

    computer.shutdown().await.unwrap();
}

#[tokio::test]
async fn uninstall_plugin_is_observable() {
    let td = TempDir::new().unwrap();
    seed_four_classes(&td);
    let computer = make_computer(&td);
    computer.boot_up().await.unwrap();

    let mut events = computer.subscribe_events();
    // a@mp 已安装 → 卸载确有移除 → Ok(true) → 发信号。
    let removed = computer
        .uninstall_plugin("a@mp", Default::default(), None)
        .await;
    assert!(
        matches!(removed, Ok(true)),
        "已装 plugin 卸载应 Ok(true): {removed:?}"
    );
    assert!(
        wait_for_config_bump(&mut events).await,
        "uninstall_plugin(Ok(true)) 应发 ConfigRevisionBumped（#124 新增）"
    );

    computer.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// 8. 已安装 plugin 的 plugin.json 损坏 → 不吞错（挂 diagnostic，不必翻 Degraded）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn corrupt_plugin_manifest_surfaces_diagnostic_without_over_degrading() {
    let td = TempDir::new().unwrap();
    let home = home_of(&td);
    let install = td.path().join("plugins").join("x");
    // install_path 存在，但 plugin.json 写坏（非法 JSON）。
    let manifest = install.join(".tfrobot-plugin").join("plugin.json");
    write(&manifest, "{ this is not json ");

    update_installed_plugins_intent(
        |f| {
            f.account.installed_plugins.insert("x@mp".to_string());
        },
        Some(&home),
        None,
    )
    .unwrap();
    update_installed_plugins(
        |f| {
            f.account.plugins.insert(
                "x@mp".to_string(),
                vec![ledger_record(Some(&install), "1.0.0", &[])],
            );
        },
        Some(&home),
        None,
    )
    .unwrap();
    let computer = make_computer(&td);

    let x = computer.get_plugin("x@mp").await.unwrap().unwrap();
    // 不吞错：挂结构化 diagnostic。
    assert!(
        x.diagnostics
            .iter()
            .any(|d| d.code == "plugin_manifest_unreadable"),
        "损坏 plugin.json 须挂 plugin_manifest_unreadable diagnostic，不得静默"
    );
    // 但不因可选元数据损坏而翻 Degraded（install_path 在、skills/mcp 各自目录不受影响）。
    assert_ne!(
        x.status,
        PluginStatus::Degraded,
        "plugin.json 属可选元数据，损坏不应过度降级"
    );
    assert!(x.installed);
}
