//! #275 —— 跨 marketplace 同名 plugin 安装层唯一性（四序之存量双装 + 结构化诊断）。
//!
//! 协议依据：`conformance-tests.md:365`——已安装 `alpha@mp-a` 后 install `alpha@mp-b` MUST 在安装层拒绝；
//! `alpha@mp-a` 及其 SKILL 保持原 source、`installedPlugins` 不新增第二个。存量双装不自动迁移：
//! boot/reconcile fail-closed + 结构化诊断 + 指引 uninstall。
//!
//! 测试以外部 crate 身份链接 `smcp_computer`，仅经公开面驱动（治理 store 低层 seeding 与
//! `governance_snapshot.rs` 同一豁免边界）。

use std::path::Path;
use std::process::Command;

use serde_json::{json, Map};
use tempfile::TempDir;

use smcp_computer::computer::{Computer, SilentSession};
use smcp_computer::diagnostics::{DiagnosticCode, DiagnosticTarget};
use smcp_computer::settings::scope::{user_settings_path, EnvMap};
use smcp_computer::settings::store::{
    installed_plugins_intent_path, update_installed_plugins, update_installed_plugins_intent,
    update_known_marketplaces,
};
use smcp_computer::settings::{
    AddMarketplaceParams, EnableOptions, InstallOptions, InstalledPluginRecord,
    KnownMarketplaceEntry, PluginInstallError, PluginNameConflictKind, RemoveMarketplaceParams,
    UninstallOptions,
};

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

/// marketplace 源仓库：plugin `alpha` + skill `code-review`（合成 `alpha:code-review`）。
fn build_repo(repo: &Path, description: &str) -> String {
    std::fs::create_dir_all(repo.join(".tfrobot-plugin")).unwrap();
    std::fs::write(
        repo.join(".tfrobot-plugin/marketplace.json"),
        r#"{"plugins": [{"name": "alpha", "source": "./plugins/alpha"}]}"#,
    )
    .unwrap();
    let skill = repo.join("plugins/alpha/skills/code-review");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        format!("---\nname: code-review\ndescription: {description}\n---\nbody"),
    )
    .unwrap();
    git(&["init", "-q"], repo);
    git(&["add", "-A"], repo);
    git(&["commit", "-qm", "init"], repo);
    format!("file://{}", repo.display())
}

fn xdg_env(td: &TempDir) -> EnvMap {
    std::iter::once((
        "XDG_CONFIG_HOME".to_string(),
        td.path().join("xdg").to_string_lossy().into_owned(),
    ))
    .collect()
}

/// 存量双装：intent 两个同名 pid 并存、两 flags=true、两源可 clone。reconcile 后：
/// - registry 中 `alpha:code-review` 唯一且 source=mp-a（先到者胜）；
/// - RuntimeDiagnostics 同时报告 `PluginNameConflict` 与 `SkillSourceConflict`（target=mp-b）；
/// - enable 任一 pid fail-closed；intent/settings 字节不变（不自动迁移）。
#[tokio::test]
async fn legacy_double_install_reconcile_reports_and_enable_fails_closed() {
    let td = TempDir::new().unwrap();
    let home = td.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let env = xdg_env(&td);
    let url_a = build_repo(&td.path().join("mp-a"), "from mp-a");
    let url_b = build_repo(&td.path().join("mp-b"), "from mp-b");

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
        Some(&home),
        Some(&env),
    )
    .unwrap();

    update_installed_plugins_intent(
        |f| {
            f.account.installed_plugins.insert("alpha@mp-a".to_string());
            f.account.installed_plugins.insert("alpha@mp-b".to_string());
        },
        Some(&home),
        Some(&env),
    )
    .unwrap();

    update_installed_plugins(
        |f| {
            for (pid, marketplace) in [("alpha@mp-a", "mp-a"), ("alpha@mp-b", "mp-b")] {
                f.account.plugins.insert(
                    pid.to_string(),
                    vec![InstalledPluginRecord {
                        install_path: Some(
                            home.join("marketplace")
                                .join(marketplace)
                                .join("plugins/alpha")
                                .to_string_lossy()
                                .into_owned(),
                        ),
                        mcp_servers: Vec::new(),
                        extra: Map::new(),
                    }],
                );
            }
        },
        Some(&home),
        Some(&env),
    )
    .unwrap();

    let settings_path = user_settings_path(Some(&env));
    std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
    std::fs::write(
        &settings_path,
        r#"{"enabledPlugins":{"alpha@mp-a":true,"alpha@mp-b":true}}"#,
    )
    .unwrap();

    let intent_path = installed_plugins_intent_path(Some(&home), Some(&env));
    let intent_before = std::fs::read(&intent_path).unwrap();
    let settings_before = std::fs::read(&settings_path).unwrap();

    let computer = Computer::new("c", SilentSession::new("s"), None, None, false, false)
        .with_skill_home(home.clone())
        .with_blob_cache_root(td.path().join("blob"))
        .with_config_dir(td.path().join("config"))
        .with_config_env(env.clone());

    computer.reconcile_governance(None, None).await;

    // 先到者胜：唯一一条 alpha:code-review，source=mp-a。
    let skills = computer.get_skills().await;
    let alpha: Vec<_> = skills
        .iter()
        .filter(|s| s.name == "alpha:code-review")
        .collect();
    assert_eq!(alpha.len(), 1, "同名 SKILL 必须唯一：{skills:?}");
    assert_eq!(alpha[0].source, "marketplace:mp-a");

    // 存量双装的结构化诊断（按被拒 pid 聚合）。
    let snap = computer.status().await;
    assert!(
        snap.diagnostics.iter().any(|d| {
            d.code == DiagnosticCode::PluginNameConflict
                && d.target == DiagnosticTarget::Plugin("alpha@mp-b".into())
        }),
        "缺少 PluginNameConflict 诊断：{:?}",
        snap.diagnostics
    );
    assert!(
        snap.diagnostics.iter().any(|d| {
            d.code == DiagnosticCode::SkillSourceConflict
                && d.target == DiagnosticTarget::Plugin("alpha@mp-b".into())
        }),
        "缺少 SkillSourceConflict 诊断：{:?}",
        snap.diagnostics
    );

    // enable 任一 pid 都 fail-closed（不自动挑选赢家）。
    let err = computer
        .enable_plugin("alpha@mp-b", EnableOptions::default(), None)
        .await
        .unwrap_err();
    match err {
        PluginInstallError::NameConflict(e) => {
            assert!(matches!(e.kind, PluginNameConflictKind::Installed { .. }))
        }
        other => panic!("expected NameConflict, got {other:?}"),
    }
    // 赢家（intent 首个 pid）同样拒绝：另一个同名已装 pid 仍存在。
    let err = computer
        .enable_plugin("alpha@mp-a", EnableOptions::default(), None)
        .await
        .unwrap_err();
    match err {
        PluginInstallError::NameConflict(e) => assert!(matches!(
            e.kind,
            PluginNameConflictKind::Installed { installed_pid } if installed_pid == "alpha@mp-b"
        )),
        other => panic!("expected NameConflict for winner, got {other:?}"),
    }

    // 不自动迁移：intent / enabledPlugins 字节不变。
    assert_eq!(std::fs::read(&intent_path).unwrap(), intent_before);
    assert_eq!(std::fs::read(&settings_path).unwrap(), settings_before);

    // 诊断代际清除：卸载重复项 → 第二轮 reconcile 收敛为空。
    assert!(computer
        .uninstall_plugin(
            "alpha@mp-b",
            UninstallOptions {
                env: Some(&env),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap());
    // R3：uninstall 后 stale 冲突记录应从 registry 清除（不再被 add/refresh 重发）。
    let reg_arc = computer.skill_registry_arc();
    let conflicts_len = { reg_arc.read().await.source_conflicts().len() };
    assert_eq!(conflicts_len, 0, "uninstall 后 stale 冲突记录应清除");
    computer.reconcile_governance(None, None).await;
    let snap = computer.status().await;
    assert!(
        !snap.diagnostics.iter().any(|d| matches!(
            d.code,
            DiagnosticCode::PluginNameConflict | DiagnosticCode::SkillSourceConflict
        )),
        "卸载重复项后诊断应收敛：{:?}",
        snap.diagnostics
    );

    // 卸载重复项后，剩余 plugin 可正常启用（不再被同名占位阻塞）。
    computer
        .enable_plugin("alpha@mp-a", EnableOptions::default(), None)
        .await
        .expect("卸载重复项后赢家应可启用");
    let skills = computer.get_skills().await;
    let alpha: Vec<_> = skills
        .iter()
        .filter(|s| s.name == "alpha:code-review")
        .collect();
    assert_eq!(alpha.len(), 1);
    assert_eq!(alpha[0].source, "marketplace:mp-a");

    // 同进程换源闭环（#271 修复后相对源 catalog 树在 uninstall 时保留）：全部卸载 → 从 mp-b
    // 重装并启用，source 变为 mp-b。
    assert!(computer
        .uninstall_plugin(
            "alpha@mp-a",
            UninstallOptions {
                env: Some(&env),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap());
    computer
        .install_plugin(
            "alpha@mp-b",
            InstallOptions {
                env: Some(&env),
                ..Default::default()
            },
            None,
        )
        .await
        .expect("uninstall 后换源 install 不得再被占名阻塞");
    computer
        .enable_plugin("alpha@mp-b", EnableOptions::default(), None)
        .await
        .expect("换源后 enable 应成功");
    let skills = computer.get_skills().await;
    let alpha: Vec<_> = skills
        .iter()
        .filter(|s| s.name == "alpha:code-review")
        .collect();
    assert_eq!(alpha.len(), 1);
    assert_eq!(alpha[0].source, "marketplace:mp-b");
}

/// 🟡3 回归：add/refresh 阶段被跨源守卫拒绝的 SKILL 必须即时可见，且 reconcile 只丢弃可证明已不存在的记录。
#[tokio::test]
async fn add_conflict_surfaces_diagnostic_and_survives_reconcile_until_source_removed() {
    let td = TempDir::new().unwrap();
    let home = td.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let env = xdg_env(&td);
    let url_a = build_repo(&td.path().join("mp-a"), "from mp-a");
    let url_b = build_repo(&td.path().join("mp-b"), "from mp-b");
    let computer = Computer::new("c", SilentSession::new("s"), None, None, false, false)
        .with_skill_home(home.clone())
        .with_blob_cache_root(td.path().join("blob"))
        .with_config_dir(td.path().join("config"))
        .with_config_env(env.clone());

    computer
        .add_marketplace(
            &url_a,
            AddMarketplaceParams {
                name: Some("mp-a"),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    computer
        .install_plugin(
            "alpha@mp-a",
            InstallOptions {
                env: Some(&env),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    computer
        .enable_plugin("alpha@mp-a", EnableOptions::default(), None)
        .await
        .unwrap();

    // add mp-b（含同名 alpha）→ 冲突即时诊断（不静默）。
    computer
        .add_marketplace(
            &url_b,
            AddMarketplaceParams {
                name: Some("mp-b"),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let snap = computer.status().await;
    assert!(
        snap.diagnostics.iter().any(|d| {
            d.code == DiagnosticCode::SkillSourceConflict
                && d.target == DiagnosticTarget::Plugin("alpha@mp-b".into())
        }),
        "add 冲突必须即时可见：{:?}",
        snap.diagnostics
    );
    assert_eq!(
        computer
            .get_skills()
            .await
            .iter()
            .find(|s| s.name == "alpha:code-review")
            .unwrap()
            .source,
        "marketplace:mp-a"
    );

    // reconcile 不得静默丢弃仍成立的冲突（kept source 仍是 owner + mp-b 仍 known）。
    computer.reconcile_governance(None, None).await;
    let snap = computer.status().await;
    assert!(
        snap.diagnostics.iter().any(|d| {
            d.code == DiagnosticCode::SkillSourceConflict
                && d.target == DiagnosticTarget::Plugin("alpha@mp-b".into())
        }),
        "reconcile 后冲突诊断应保留：{:?}",
        snap.diagnostics
    );

    // R2：再 reconcile（无变更）→ 校验通过的 prior 必须回写 registry，诊断仍在。
    computer.reconcile_governance(None, None).await;
    let snap = computer.status().await;
    assert!(
        snap.diagnostics.iter().any(|d| {
            d.code == DiagnosticCode::SkillSourceConflict
                && d.target == DiagnosticTarget::Plugin("alpha@mp-b".into())
        }),
        "二次 reconcile 不得静默清除仍成立的冲突：{:?}",
        snap.diagnostics
    );

    // 移除冲突 marketplace → 下一轮 reconcile 清除。
    computer
        .remove_marketplace("mp-b", RemoveMarketplaceParams::default())
        .await
        .unwrap();
    computer.reconcile_governance(None, None).await;
    let snap = computer.status().await;
    assert!(
        !snap
            .diagnostics
            .iter()
            .any(|d| d.code == DiagnosticCode::SkillSourceConflict),
        "源移除后诊断应收敛：{:?}",
        snap.diagnostics
    );
}
