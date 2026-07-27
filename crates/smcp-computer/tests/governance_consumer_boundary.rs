//! #124 —— 外部 consumer **边界** compile/run 守护。
//!
//! 本文件以**外部 crate** 身份链接 `smcp_computer`，刻意**仅** import `Computer` 与治理 DTO（`smcp_computer::`
//! 顶层 re-export），**绝不** import `settings::store` / `settings::scope` / `skills::manifest`，也不扫描 SDK
//! 文件布局——即验收「外部 consumer 仅导入 `Computer` 和治理 DTO 即可查询」。若高层 API 的签名/返回型泄漏了低层
//! 内部类型（迫使 consumer 补 import 才能编译），本文件将**编译失败**——这就是边界守护本身。
//!
//! ⚠️ 维护约束：**不得**在本文件新增 `smcp_computer::settings::*` / `smcp_computer::skills::*` 的 import，
//! 否则边界守护失效。所有治理查询只经 `Computer` 方法 + 顶层治理 DTO 完成。

use std::collections::HashMap;

use smcp_computer::computer::{Computer, SilentSession};
use smcp_computer::{
    DeclaredCapabilities, GovernanceDecision, GovernanceDiagnostic, GovernanceRevision,
    GovernanceSnapshot, ListPluginsOptions, MarketplaceSnapshot, MarketplaceStatus, PluginSnapshot,
    PluginStatus, ProvenanceScope,
};

/// 仅用高层类型即可写出完整消费函数——签名不依赖任何低层 SDK 类型。
fn classify(snapshot: &GovernanceSnapshot) -> (usize, usize, usize, usize) {
    let mut available = 0;
    let mut disabled = 0;
    let mut enabled = 0;
    let mut degraded = 0;
    for p in &snapshot.plugins {
        // `PluginStatus` 是 `#[non_exhaustive]`（output 枚举，前向兼容）：外部 consumer 须带 wildcard 臂——
        // 本 match 即验证「未来新增 status 变体不破坏 consumer 编译」的正确消费姿势。
        match p.status {
            PluginStatus::Available => available += 1,
            PluginStatus::InstalledDisabled => disabled += 1,
            PluginStatus::InstalledEnabled => enabled += 1,
            PluginStatus::Degraded => degraded += 1,
            _ => {}
        }
    }
    (available, disabled, enabled, degraded)
}

/// consumer 视角读取每个治理 DTO 字段（编译即证字段公开可达、类型自足）。
fn read_all_fields(mp: &MarketplaceSnapshot, p: &PluginSnapshot) {
    // marketplace 面。
    let _: &str = &mp.name;
    let _: Option<&String> = mp.source_url.as_ref();
    let _: bool = mp.trusted && mp.blocked && mp.strict && mp.auto_update;
    let _: GovernanceDecision = mp.decision;
    let _: MarketplaceStatus = mp.status;
    let _: Option<&String> = mp.install_location.as_ref();
    let _: Option<&String> = mp.commit_sha.as_ref();
    let _: Option<&String> = mp.last_updated.as_ref();
    let _: &Vec<String> = &mp.plugin_ids;
    let _: &Vec<String> = &mp.available_plugin_ids;
    let _diag: &Vec<GovernanceDiagnostic> = &mp.diagnostics;

    // plugin 面。
    let _: &str = &p.id;
    let _: &str = &p.plugin;
    let _: &str = &p.marketplace;
    let _: Option<&String> = p.name.as_ref();
    let _: Option<&String> = p.version.as_ref();
    let _: PluginStatus = p.status;
    let _: bool = p.installed && p.enabled;
    let _: Option<ProvenanceScope> = p.enablement_scope;
    let _: Option<&String> = p.install_scope.as_ref();
    let _: Option<&String> = p.install_path.as_ref();
    let _: GovernanceDecision = p.decision;
    let _: &Vec<String> = &p.bundled_mcp_servers;
    let _: &Vec<String> = &p.bundled_skills;
    let _: &Vec<String> = &p.materialized_mcp_servers;
    // #125：目录声明能力（安装前预览）——`Option` 区分「未知」与「确实无」，字段类型自足、不 import 低层。
    let _declared: Option<&DeclaredCapabilities> = p.declared.as_ref();
    if let Some(caps) = p.declared.as_ref() {
        let _: Option<&String> = caps.version.as_ref();
        let _: Option<&String> = caps.description.as_ref();
        let _: &Vec<String> = &caps.mcp_servers;
        let _: &Vec<String> = &caps.skills;
    }
    let _: &Vec<GovernanceDiagnostic> = &p.diagnostics;
}

#[tokio::test]
async fn external_consumer_queries_via_high_level_api_only() {
    let td = tempfile::TempDir::new().unwrap();
    let env: HashMap<String, String> = std::iter::once((
        "XDG_CONFIG_HOME".to_string(),
        td.path().join("xdg").to_string_lossy().into_owned(),
    ))
    .collect();

    // 空实例——无需 seed、无需 boot、无需 cli feature：证「仅高层 API 即可查询」端到端可达。
    let computer = Computer::new("c", SilentSession::new("s"), None, None, false, false)
        .with_skill_home(td.path().join("home"))
        .with_blob_cache_root(td.path().join("blob"))
        .with_config_dir(td.path().join("config"))
        .with_config_env(env);

    // 统一快照。
    let snapshot: GovernanceSnapshot = computer.governance_snapshot().await.unwrap();
    let _rev: &GovernanceRevision = &snapshot.revision;
    let _ = classify(&snapshot);

    // list/get 派生一致（同一状态语义、同 revision）。
    let marketplaces: Vec<MarketplaceSnapshot> = computer.list_marketplaces().await.unwrap();
    assert_eq!(marketplaces.len(), snapshot.marketplaces.len());
    let plugins: Vec<PluginSnapshot> = computer
        .list_plugins(ListPluginsOptions {
            include_available: true,
            marketplace: None,
        })
        .await
        .unwrap();

    // 空实例：无 marketplace / plugin，但查询成功（非静默错误）。
    assert!(marketplaces.is_empty());
    assert!(plugins.is_empty());

    // 单项查询返回 Option，未知 → None。
    let none_mp: Option<MarketplaceSnapshot> = computer.get_marketplace("nope").await.unwrap();
    let none_pl: Option<PluginSnapshot> = computer.get_plugin("nope@nope").await.unwrap();
    assert!(none_mp.is_none() && none_pl.is_none());

    // 字段可达性守护（不实际运行分支，仅需类型自足）。
    if let (Some(mp), Some(p)) = (marketplaces.first(), plugins.first()) {
        read_all_fields(mp, p);
    }
}
