//! #162 —— 结构化 Runtime diagnostics 集成守护（SDK 视角，外部 crate 编译）。
//!
//! 逐条映射 Issue #162 验收：
//! - ① boot / MCP / governance 异常 → 结构化诊断（非裸字符串）
//! - ② 恢复成功 / 移除后，旧诊断不再出现在当前 snapshot
//! - ③ 诊断变化带单调 revision + 事件（`DiagnosticsChanged`），事件丢失可经 snapshot 重建
//! - ⑤ 消息构造期强制脱敏（凭据 URL 不落 message）
//! - ⑦ DTO 字段面最小（无 UI 文案 / Robot / connection authority / 客户端策略）
//!
//! 测试以外部 crate 身份链接 `smcp_computer`，仅经公开面驱动（含治理 store 低层 seeding，
//! 与 `governance_snapshot.rs` 同一豁免边界）。

use std::collections::HashMap;
use std::path::Path;

use serde_json::{json, Map};
use tempfile::TempDir;

use smcp_computer::computer::{Computer, SilentSession};
use smcp_computer::diagnostics::{
    DiagnosticCode, DiagnosticOperation, DiagnosticSeverity, DiagnosticSource, DiagnosticTarget,
    RuntimeDiagnostic,
};
use smcp_computer::mcp_clients::bundle_id::resolve_bundle_id;
use smcp_computer::mcp_clients::model::MCPServerConfig;
use smcp_computer::settings::scope::{workdir_local_settings_path, EnvMap};
use smcp_computer::settings::store::{
    update_installed_plugins, update_installed_plugins_intent, update_known_marketplaces,
};
use smcp_computer::settings::InstalledPluginRecord;
use smcp_computer::{ComputerEvent, LifecycleState};

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
fn make_computer(
    td: &TempDir,
    declared: HashMap<String, MCPServerConfig>,
) -> Computer<SilentSession> {
    let env: EnvMap = xdg_env(td);
    Computer::new(
        "c",
        SilentSession::new("s"),
        None,
        Some(declared),
        false,
        false,
    )
    .with_skill_home(td.path().join("home"))
    .with_blob_cache_root(td.path().join("blob"))
    .with_config_dir(td.path().join("config"))
    .with_config_env(env)
}

/// 一个 command 必然 spawn 失败的 stdio server（进程不存在 → start/restart 真实运行期失败）。
fn missing_binary_server(name: &str) -> MCPServerConfig {
    serde_json::from_value(json!({
        "type": "stdio",
        "name": name,
        "server_parameters": {"command": "definitely-missing-binary-xyz-162"}
    }))
    .unwrap()
}

/// 播种「enabled plugin 挂在不可达 marketplace」fixture：boot → Degraded + MarketplaceSyncFailed。
fn seed_unreachable_marketplace(td: &TempDir) {
    let home = td.path().join("home");
    let config = td.path().join("config");
    let bad = format!("file://{}/nonexistent-repo", td.path().display());
    update_known_marketplaces(
        move |file| {
            file.account.marketplaces.insert(
                "acme".to_string(),
                smcp_computer::settings::KnownMarketplaceEntry {
                    source: json!({"type": "git", "url": bad}),
                    extra: Map::new(),
                },
            );
        },
        Some(&home),
        None,
    )
    .unwrap();
    update_installed_plugins(
        |file| {
            file.account.plugins.insert(
                "audit@acme".to_string(),
                vec![InstalledPluginRecord::default()],
            );
        },
        Some(&home),
        None,
    )
    .unwrap();
    update_installed_plugins_intent(
        |file| {
            file.account
                .installed_plugins
                .insert("audit@acme".to_string());
        },
        Some(&home),
        None,
    )
    .unwrap();
    write(
        &workdir_local_settings_path(&config),
        r#"{"enabledPlugins": {"audit@acme": true}}"#,
    );
}

// ---------------------------------------------------------------------------
// ⑦ DTO 字段面最小 + ⑤ 构造期脱敏
// ---------------------------------------------------------------------------

/// 验收⑦：单条诊断的序列化字段面**恰好**是机器可读 9 键——不含 UI 文案、Robot、
/// connection authority 或 tfrobot-client 操作策略字段。
#[test]
fn dto_field_surface_is_minimal() {
    let diag = RuntimeDiagnostic::new(
        DiagnosticCode::McpStartFailed,
        DiagnosticSeverity::Degraded,
        DiagnosticSource::Mcp,
        DiagnosticOperation::StartClient,
        DiagnosticTarget::Runtime,
        "server failed to start",
    );
    let v = serde_json::to_value(&diag).unwrap();
    let obj = v.as_object().unwrap();
    let expected: Vec<&str> = vec![
        "code",
        "severity",
        "source",
        "operation",
        "target",
        "message",
        "occurred_at",
        "retryable",
        "transient",
    ];
    let mut got: Vec<&str> = obj.keys().map(String::as_str).collect();
    got.sort_unstable();
    let mut want = expected;
    want.sort_unstable();
    assert_eq!(got, want, "诊断 DTO 字段面必须恰好为机器可读 9 键");
    // 词汇 serde 形态（snake_case，稳定契约）。
    assert_eq!(obj["code"], json!("mcp_start_failed"));
    assert_eq!(obj["severity"], json!("degraded"));
    assert_eq!(obj["source"], json!("mcp"));
    assert_eq!(obj["operation"], json!("start_client"));
    assert_eq!(obj["retryable"], json!(false));
    assert_eq!(obj["transient"], json!(false));
}

/// 验收⑤：message 经构造点强制脱敏——嵌入凭据的 Git URL 不落 message（fail-closed）。
#[test]
fn diagnostic_message_redacts_embedded_credentials() {
    let diag = RuntimeDiagnostic::new(
        DiagnosticCode::MarketplaceSyncFailed,
        DiagnosticSeverity::Degraded,
        DiagnosticSource::Governance,
        DiagnosticOperation::MarketplaceSync,
        DiagnosticTarget::Marketplace("acme".into()),
        "clone https://cnb:hunter2@example.com/mp.git failed",
    );
    assert!(!diag.message.contains("hunter2"), "凭据不得落 message");
    assert!(
        !diag.message.contains("cnb:"),
        "userinfo 整段不得落 message"
    );
    assert!(
        diag.message.contains("example.com"),
        "脱敏保留 host 供定位：{}",
        diag.message
    );
}

/// 构造点补充 retryable/transient（机器可读恢复属性）。
#[test]
fn diagnostic_with_recovery_flags() {
    let base = RuntimeDiagnostic::new(
        DiagnosticCode::McpStartFailed,
        DiagnosticSeverity::Degraded,
        DiagnosticSource::Mcp,
        DiagnosticOperation::StartClient,
        DiagnosticTarget::Runtime,
        "connection refused",
    );
    assert!(!base.retryable);
    let flagged = base.with_recovery(true, true);
    assert!(flagged.retryable);
    assert!(flagged.transient);
}

// ---------------------------------------------------------------------------
// ①②③④ MCP 运行期失败：记录 / 事件 / 替代 / 移除清除（现状缺口的红灯）
// ---------------------------------------------------------------------------

/// 验收①③：MCP start 真实失败 → 结构化诊断（code/severity/source/target/message）+
/// 单调 diagnostics revision + `DiagnosticsChanged` 事件；投影 `degraded_reason` 非 None、
/// `last_error` 保持 None（无 Error 级条目）。验收②：unmount 移除后诊断不再出现。
#[tokio::test]
async fn mcp_start_failure_records_diagnostic_event_and_removal_clears_it() {
    let td = TempDir::new().unwrap();
    let cfg = missing_binary_server("broken-srv");
    let bid = resolve_bundle_id(&cfg);
    let mut declared = HashMap::new();
    declared.insert("broken-srv".to_string(), cfg);
    let computer = make_computer(&td, declared);

    computer.boot_up().await.unwrap();
    let rev0 = computer.diagnostics_revision();
    let mut rx = computer.subscribe_events();

    // start 真实失败（spawn 不存在的 command）。
    let result = computer.start_mcp_client(&bid).await;
    assert!(result.is_err(), "缺失二进制的 start 必须失败");

    // 验收①：结构化诊断落在当前 snapshot（现状：完全不记录 → 红灯点）。
    let snap = computer.status().await;
    let found = snap
        .diagnostics
        .iter()
        .find(|d| {
            d.code == DiagnosticCode::McpStartFailed
                && d.target == DiagnosticTarget::Bundle(bid.clone())
        })
        .expect("MCP start 失败必须落结构化诊断");
    assert_eq!(found.severity, DiagnosticSeverity::Degraded);
    assert_eq!(found.source, DiagnosticSource::Mcp);
    assert_eq!(found.operation, DiagnosticOperation::StartClient);
    assert!(!found.message.is_empty(), "message 非空（secret-free）");
    // 投影：Degraded 级 → degraded_reason；无 Error 级 → last_error None。
    assert!(
        snap.degraded_reason.is_some(),
        "Degraded 级诊断必须投影进 degraded_reason"
    );
    assert!(
        snap.last_error.is_none(),
        "无 Error 级条目 → last_error 保持 None"
    );

    // 验收③：revision 单调 + 事件广播。
    assert!(
        computer.diagnostics_revision() > rev0,
        "诊断变化必须 bump 单调 revision"
    );
    let mut saw_diag_event = false;
    while let Ok(ev) = rx.try_recv() {
        if let ComputerEvent::DiagnosticsChanged { revision } = ev {
            assert!(revision > rev0);
            saw_diag_event = true;
        }
    }
    assert!(saw_diag_event, "诊断变化必须广播 DiagnosticsChanged");

    // 验收④：同 bundle 同问题单条（同键不堆叠）。
    let count_before = snap
        .diagnostics
        .iter()
        .filter(|d| d.target == DiagnosticTarget::Bundle(bid.clone()))
        .count();
    assert_eq!(count_before, 1, "同 bundle 同问题单条（不堆叠）");

    // restart 真实失败 → 记 McpRestartFailed，与 McpStartFailed **异码异键并存**（验收④：
    // 不同问题互不覆盖——同键替代只发生在同 code+target 重复出现时）。
    assert!(computer.restart_mcp_client(&bid).await.is_err());
    let snap_restart = computer.status().await;
    let codes: Vec<DiagnosticCode> = snap_restart
        .diagnostics
        .iter()
        .filter(|d| d.target == DiagnosticTarget::Bundle(bid.clone()))
        .map(|d| d.code)
        .collect();
    assert!(
        codes.contains(&DiagnosticCode::McpStartFailed)
            && codes.contains(&DiagnosticCode::McpRestartFailed),
        "restart 失败与 start 失败异码并存：{codes:?}"
    );
    assert_eq!(
        codes.len(),
        2,
        "该 bundle 恰 2 条（异码并存，不堆叠不互覆）"
    );

    // stop 真停到（Ok(true)：failed start 仍在 manager 登记 error 态客户端）→ **清该 bundle 全部
    // 诊断**（「有意停 = 消亡」，失败记录被操作意图替代——验收②的替代语义）。
    let stopped = computer.stop_mcp_client(&bid).await.unwrap();
    assert!(stopped, "failed start 后 manager 仍登记客户端 → 真停到");
    let snap_stop = computer.status().await;
    assert!(
        snap_stop
            .diagnostics
            .iter()
            .all(|d| d.target != DiagnosticTarget::Bundle(bid.clone())),
        "stop Ok(true) 清该 bundle 全部诊断（消亡即替代）"
    );
    // stop 幂等 no-op（Ok(false)：已无活跃客户端）→ 空集保持、不虚假 bump（窄域清除分支的
    // Computer 级可见面；谓词精确性由 status.rs 单测 `clear_where_removes_only_matching` 锚定）。
    let rev_after_stop = computer.diagnostics_revision();
    let stopped_again = computer.stop_mcp_client(&bid).await.unwrap();
    assert!(!stopped_again, "无活跃客户端 → Ok(false)");
    assert_eq!(
        computer.diagnostics_revision(),
        rev_after_stop,
        "空清除不 bump"
    );
    assert!(computer
        .status()
        .await
        .diagnostics
        .iter()
        .all(|d| d.target != DiagnosticTarget::Bundle(bid.clone())));

    // 验收②：unmount 移除声明 → 该 bundle 诊断不再出现在当前 snapshot。
    computer.unmount_server(&bid).await.unwrap();
    let snap2 = computer.status().await;
    assert!(
        snap2
            .diagnostics
            .iter()
            .all(|d| d.target != DiagnosticTarget::Bundle(bid.clone())),
        "移除的 server 诊断必须清除（不得永久滞留）"
    );
    assert!(snap2.degraded_reason.is_none());
}

// ---------------------------------------------------------------------------
// ①②③ governance：boot 降级结构化 + 运行期恢复清除（现状缺口的红灯）
// ---------------------------------------------------------------------------

/// 验收①②③：不可达 marketplace → boot 落 `Degraded` + `MarketplaceSyncFailed` 结构化诊断 +
/// 事件；**运行期**修复后 `reconcile_governance` 清除诊断并窄域恢复 `Started`
/// （现状：运行期无清除路径、降级后须重启 → 红灯点）。
#[tokio::test]
async fn governance_boot_degrades_structured_and_runtime_recovery_clears() {
    let td = TempDir::new().unwrap();
    seed_unreachable_marketplace(&td);
    let computer = make_computer(&td, HashMap::new());
    let mut rx = computer.subscribe_events();

    computer.boot_up().await.unwrap();

    // 验收①：结构化 Degraded 诊断（现状为裸汇总字符串 → 红灯点）。
    let snap = computer.status().await;
    assert_eq!(
        snap.lifecycle,
        LifecycleState::Degraded,
        "boot 降级语义保持"
    );
    let found = snap
        .diagnostics
        .iter()
        .find(|d| {
            d.code == DiagnosticCode::MarketplaceSyncFailed
                && d.target == DiagnosticTarget::Marketplace("acme".into())
        })
        .expect("marketplace 同步失败必须落 MarketplaceSyncFailed 诊断");
    assert_eq!(found.severity, DiagnosticSeverity::Degraded);
    assert_eq!(found.source, DiagnosticSource::Governance);
    assert_eq!(found.operation, DiagnosticOperation::MarketplaceSync);
    assert!(snap.degraded_reason.is_some());
    assert!(snap.last_error.is_none(), "降级非 Error 级");

    // 验收③：事件可观测。
    let mut saw_diag_event = false;
    while let Ok(ev) = rx.try_recv() {
        if matches!(ev, ComputerEvent::DiagnosticsChanged { .. }) {
            saw_diag_event = true;
        }
    }
    assert!(saw_diag_event, "诊断出现必须广播 DiagnosticsChanged");

    // 运行期恢复：①建回 marketplace clone 树（MarketplaceSyncFailed 的降级判据 = clone 树缺失；
    // SOURCE_MARKETPLACE = "marketplace"）；②账本补可用 install_path（令 rematerialize 幂等跳过，
    // LedgerRematerializeFailed 消除）。
    let mp_dir = td.path().join("home").join("marketplace").join("acme");
    std::fs::create_dir_all(&mp_dir).unwrap();
    let plugin_root = td.path().join("plugins").join("audit");
    std::fs::create_dir_all(&plugin_root).unwrap();
    update_installed_plugins(
        |file| {
            file.account.plugins.insert(
                "audit@acme".to_string(),
                vec![InstalledPluginRecord {
                    install_path: Some(plugin_root.to_string_lossy().into_owned()),
                    ..InstalledPluginRecord::default()
                }],
            );
        },
        Some(&td.path().join("home")),
        None,
    )
    .unwrap();

    let report = computer.reconcile_governance(None, None).await;
    assert!(
        report.failed_marketplaces.is_empty(),
        "clone 树恢复后不再失败：{:?}",
        report.failed_marketplaces
    );

    // 验收②：旧诊断被清除（现状：运行期无清除路径 → 红灯点）。
    let snap2 = computer.status().await;
    assert!(
        snap2.diagnostics.is_empty(),
        "恢复后当前 snapshot 不得残留过期诊断：{:?}",
        snap2.diagnostics
    );
    assert!(snap2.degraded_reason.is_none());
    assert_eq!(
        snap2.lifecycle,
        LifecycleState::Started,
        "窄域恢复迁移 Degraded → Started"
    );
}

// ---------------------------------------------------------------------------
// 事件丢失后 snapshot 重建（验收⑥的事件丢失半边；Computer 级）
// ---------------------------------------------------------------------------

/// 验收⑥：订阅方 Lagged（事件丢失）后，经 `status()` 全量快照 + revision 重建当前诊断集。
#[tokio::test]
async fn snapshot_rebuilds_diagnostics_after_event_loss() {
    let td = TempDir::new().unwrap();
    let cfg = missing_binary_server("lagged-srv");
    let bid = resolve_bundle_id(&cfg);
    let mut declared = HashMap::new();
    declared.insert("lagged-srv".to_string(), cfg);
    let computer = make_computer(&td, declared);
    computer.boot_up().await.unwrap();
    let mut rx = computer.subscribe_events();

    // 触发一次诊断。
    assert!(computer.start_mcp_client(&bid).await.is_err());
    // 故意排干/丢弃事件（模拟消费方事件丢失——含 Lagged 丢帧）。
    while rx.try_recv().is_ok() {}

    // 消费方不依赖事件重放：拉全量快照重建（诊断 + revision）。
    let snap = computer.status().await;
    assert!(snap.diagnostics_revision > 0);
    assert!(
        snap.diagnostics
            .iter()
            .any(|d| d.code == DiagnosticCode::McpStartFailed),
        "事件丢失后 snapshot 仍可重建诊断集"
    );
}
