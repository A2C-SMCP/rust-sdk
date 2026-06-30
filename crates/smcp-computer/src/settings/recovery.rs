/*!
* 文件名: recovery.rs
* 作者: JQQ
* 创建日期: 2026/06/30
* 最后修改日期: 2026/06/30
* 版权: 2023 JQQ. All rights reserved.
* 依赖: serde_json, crate::settings::store, crate::skills::{staging,manifest,registry,home}
* 描述: Computer 治理状态启动恢复（从持久化 ledger 重建边界内派生态）非 CLI 收口（#95）
*       Non-CLI governance boot recovery (rebuild within-boundary derived state from ledgers).
*/

//! 治理启动恢复核心（**非 CLI**、ledger 驱动、additive、离线优先、enabled 门控）/ governance boot recovery。
//!
//! 协议依据 / Protocol: a2c-smcp-protocol §9.x（plugin 生命周期）/ §10.x（marketplace 物化）；父 epic #93
//! 三层划分（配置根=构造期固定不可变；治理账本 + 派生注册表=边界内可变）。子任务 #95：补齐一条**目前不
//! 存在、且失跟踪**的能力——冷启动从持久化 ledger 重建治理状态。
//!
//! ## 为什么 ledger 驱动（而非 [`reconcile`](crate::settings::reconciler::reconcile)）
//!
//! 体系内有两条并行模型：
//! - **声明式**（settings.json 的 `extraKnownMarketplaces` + `enabledPlugins=true`）→ 由
//!   [`reconcile`](crate::settings::reconciler::reconcile) additive 对账消费。**用户手编**意图层，Rust runtime
//!   目前无写入方。
//! - **命令式**（[`install_plugin`](crate::settings::installer::install_plugin) 写 `installed_plugins.json`、
//!   [`add_marketplace`](crate::settings::lifecycle::add_marketplace) 写 `known_marketplaces.json`）→ #94
//!   Computer 级 lifecycle API 与 CLI 实际走的路径。
//!
//! 关键事实：`install_plugin` **不**写 `enabledPlugins=true`（装即活跃，无显式 enable 旗）。故
//! [`reconcile`](crate::settings::reconciler::reconcile) 的 `enabled_plugin_names_for`（要求 `== true`）**无法**
//! 恢复命令式安装的 plugin。#95 恢复的是**命令式 ledger** 态，故本模块直接读两份 ledger，复用 reconcile 的
//! **后端原语**（[`stage_marketplace_skills`] / [`load_bundled_servers`] / store），**不**重写后端、**不**经
//! `reconcile()` 入口（对齐 issue「复用 reconcile/prune/gc + FileSkillGovernanceStore，不重写后端」）。
//!
//! ## 恢复语义（#95 范围）
//!
//! 1. **enabled 门控**：plugin 为「boot-active」⟺ 在 `installed_plugins.json` 且
//!    `enabledPlugins[pid] != false`（缺省 / `true` 皆视为启用——匹配 install 装即活跃语义）。显式 `false`
//!    （disable / enable-rollback 落定）→ **不**重挂 skills、**不**重挂 server。这条门控正是「boot 恢复**不得**
//!    复活 Sub-A(#94) 中 hook 失败已回滚的半装 plugin」的实现机制：回滚落定 `enabledPlugins=false` → 不复活。
//!    （#94 enable-rollback 的**窄残窗**：回写 false 这步也失败 → 账本残留 `true` → 本恢复据账本视其为启用并
//!    重挂——与持久化态一致，属可容忍降级，见 [`installer`](crate::settings::installer) enable 回滚注释。）
//! 2. **additive-only**：只增不删（不 prune/gc 孤儿——那是 §7.3 显式入口）。重复调用幂等。
//! 3. **离线优先**：`refresh = false` → 已存在 clone 树**复用、不触网**；clone 树缺失才尝试 clone。
//! 4. **降级铁律**（§7.2）：marketplace 源不可达 / clone 树缺失且 clone 失败 → 该 marketplace 入
//!    `failed_marketplaces` + WARN，**不**阻断恢复其余、**不**抛。
//!
//! ## 两阶段（锁纪律）/ Two phases (lock discipline)
//!
//! 本模块只提供两个**独立**入口，由 [`Computer`](crate::computer::Computer) 编排以**断开**潜在 ABBA：
//! - [`recover_marketplace_skills`]（持 `skill_registry` 写锁）：重挂 marketplace skills。
//! - [`collect_enabled_bundled_servers`]（**不**持任何锁，纯读 ledger + 解析）：产出待重挂的 bundled server
//!   配置；调用方在**释放 skill 写锁后**经 [`McpInstallHooks`](crate::settings::installer::McpInstallHooks)
//!   逐个 `register_server`（client 拥有「如何物化」，SDK 决定「哪些」）。两阶段分离避免「持 skill 写锁 →
//!   经 hooks 取 mcp_manager 锁」的相反序死锁（见 computer.rs `restage_mcp_skills` 锁序注释）。

use std::collections::HashSet;
use std::path::Path;

use indexmap::IndexMap;
use serde_json::{Map, Value};

use crate::mcp_clients::model::MCPServerConfig;
use crate::settings::scope::EnvMap;
use crate::settings::store::{load_installed_plugins, load_known_marketplaces};
use crate::skills::home::marketplace_skill_dir;
use crate::skills::manifest::load_bundled_servers;
use crate::skills::registry::SkillRegistry;
use crate::skills::staging::{stage_marketplace_skills, MarketplaceStageOptions};

// ===========================================================================
// 恢复报告 / Recovery report
// ===========================================================================
/// 一次 [`recover_marketplace_skills`] + MCP 重挂的结果（观测 + 测试信号）/ Boot recovery report。
///
/// `remounted_servers` 由编排方（[`Computer`](crate::computer::Computer)）在第二阶段填充——本模块的
/// [`recover_marketplace_skills`] 仅产出 skills 维度，`remounted_servers` 留空。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GovernanceRecoveryReport {
    /// 成功重挂 skills 的启用 plugin id（`<plugin>@<mp>`）/ enabled plugin ids whose skills were restaged。
    pub restored_plugins: Vec<String>,
    /// 本次重新注册的全部 marketplace SKILL 名 / all marketplace SKILL names re-registered。
    pub restored_skills: Vec<String>,
    /// 经 hooks 成功重挂的 bundled MCP server 名（编排方第二阶段填充）/ bundled servers remounted via hooks。
    pub remounted_servers: Vec<String>,
    /// 源不可达 / clone 树缺失且 clone 失败的 marketplace（降级、未恢复其 skills）/ degraded marketplaces。
    pub failed_marketplaces: Vec<String>,
    /// 显式 `enabledPlugins=false` 故**刻意跳过**的已装 plugin id（不复活）/ deliberately-skipped disabled。
    pub skipped_disabled: Vec<String>,
}

// ===========================================================================
// enabled 门控 / Enabled gating
// ===========================================================================
/// plugin 是否「boot-active」：在 ledger 且 `enabledPlugins[pid] != false` / whether a plugin is boot-active。
///
/// 缺省（无 key）与 `true` 皆视为启用——匹配 [`install_plugin`](crate::settings::installer::install_plugin)
/// 「装即活跃、无显式 enable 旗」语义；仅显式 `Bool(false)`（disable / enable-rollback 落定）视为禁用。
///
/// ⚠️ `declared` 的**完整性由调用方负责**：仅当禁用旗所在 scope 已并入 `declared` 才生效。写在**未登记
/// workdir 的 project/local scope** 的 `enabledPlugins=false` 可能不在 `declared` 内 → 此处误判为启用。跨重启
/// 可靠禁用应写 user scope（见 [`Computer::reconcile_governance`](crate::computer::Computer::reconcile_governance)
/// 调用方须知）。
#[must_use]
fn plugin_enabled(declared: &Map<String, Value>, pid: &str) -> bool {
    !matches!(
        declared
            .get("enabledPlugins")
            .and_then(|plugins| plugins.get(pid)),
        Some(Value::Bool(false))
    )
}

// ===========================================================================
// 阶段一：marketplace skills 重挂 / Phase 1: restage marketplace skills
// ===========================================================================
/// 从 ledger 重挂**已装且启用**的 marketplace plugin skills（additive、离线优先、降级）/ Restage skills。
///
/// 读 `installed_plugins.json`（哪些 plugin 已装）+ `known_marketplaces.json`（marketplace git 源）；按
/// marketplace 分组启用 plugin，逐 marketplace 经 [`stage_marketplace_skills`]（`refresh = false`、
/// `plugin_filter = 启用 plugin 名`、`recorder = None` 不改账本）重挂。enabled 门控见模块文档。
///
/// **持锁契约**：调用方持 `skill_registry` 写锁传入 `&mut SkillRegistry`；本函数内 stage 含 git await，写锁
/// 跨 await（与 install/enable/refresh 同构，boot 期无并发故安全）。MCP server 重挂**不**在此（见
/// [`collect_enabled_bundled_servers`]），以便调用方释放写锁后再经 hooks 重挂、断开 ABBA。
///
/// **幂等**：`stage_marketplace_skills` 以 register-or-update 重挂同名 skill；重复调用结果一致。
pub async fn recover_marketplace_skills(
    registry: &mut SkillRegistry,
    home: &Path,
    env: Option<&EnvMap>,
    declared: &Map<String, Value>,
) -> GovernanceRecoveryReport {
    let installed = load_installed_plugins(Some(home), env).account;
    let known = load_known_marketplaces(Some(home), env).account;

    let mut report = GovernanceRecoveryReport::default();
    // 按 marketplace 分组启用 plugin（保 ledger 首见顺序）：mp → (启用 plugin 名集, 启用 pid 列表)。
    let mut by_marketplace: IndexMap<String, (HashSet<String>, Vec<String>)> = IndexMap::new();

    for pid in installed.plugins.keys() {
        let Some((plugin, marketplace)) = pid.split_once('@') else {
            // 无 '@'（本地-only 记录）→ 非 marketplace plugin，恢复不涉及。
            continue;
        };
        if !plugin_enabled(declared, pid) {
            report.skipped_disabled.push(pid.clone());
            continue;
        }
        let bucket = by_marketplace.entry(marketplace.to_string()).or_default();
        bucket.0.insert(plugin.to_string());
        bucket.1.push(pid.clone());
    }

    for (marketplace, (plugin_names, pids)) in &by_marketplace {
        let Some(record) = known.marketplaces.get(marketplace) else {
            // 已装 plugin 但 marketplace 不在 known_marketplaces（账本失配 / 已 prune）→ 降级、不恢复。
            tracing::warn!(
                marketplace = %marketplace,
                "recover: installed plugin's marketplace missing from known_marketplaces, skipped"
            );
            report.failed_marketplaces.push(marketplace.clone());
            continue;
        };
        let auto_update = record
            .extra
            .get("autoUpdate")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let names = stage_marketplace_skills(
            marketplace,
            &record.source,
            registry,
            home,
            MarketplaceStageOptions {
                plugin_filter: Some(plugin_names),
                auto_update,
                refresh: false,
                timeout: None,
                env,
                recorder: None,
            },
        )
        .await;

        // 降级判定（对齐 reconcile）：stage 吞错降级 → clone 树仍缺失即视为失败（源不可达 / 缺失且 clone 失败）。
        if !marketplace_skill_dir(home, marketplace, &[]).exists() {
            tracing::warn!(
                marketplace = %marketplace,
                "recover: marketplace clone unreachable/missing, degraded (boot not blocked)"
            );
            report.failed_marketplaces.push(marketplace.clone());
            continue;
        }
        report.restored_plugins.extend(pids.iter().cloned());
        report.restored_skills.extend(names);
    }

    report
}

// ===========================================================================
// 阶段二输入：待重挂 bundled server 配置采集 / Phase 2 input: bundled servers to remount
// ===========================================================================
/// 采集**已装且启用** plugin 的 bundled MCP server 配置（供调用方经 hooks 重挂）/ Collect bundled servers。
///
/// 从每条 [`InstalledPluginRecord`](crate::settings::reconciler::InstalledPluginRecord) 的 `installPath`
/// 重解析 [`load_bundled_servers`]（与 [`enable_plugin`](crate::settings::installer::enable_plugin) 同源、
/// 不重 clone）。enabled 门控同 [`recover_marketplace_skills`]：`enabledPlugins=false` 的 plugin **不**采集
/// （其 server 保持下线，含已回滚的半装 plugin）。跨 plugin / scope 按 server 名去重（首见保留）。
///
/// **无锁、纯读**：不持任何 Registry / manager 锁；调用方在释放 skill 写锁后逐个 `register_server`，避免
/// 「skill 写锁 → mcp_manager 锁」相反序死锁。解析失败 → WARN 跳过该 plugin（降级、不阻断）。
#[must_use]
pub fn collect_enabled_bundled_servers(
    home: &Path,
    env: Option<&EnvMap>,
    declared: &Map<String, Value>,
) -> Vec<MCPServerConfig> {
    let installed = load_installed_plugins(Some(home), env).account;
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<MCPServerConfig> = Vec::new();

    for (pid, records) in &installed.plugins {
        if !plugin_enabled(declared, pid) {
            continue;
        }
        for rec in records {
            let Some(install_path) = rec.install_path.as_deref().filter(|s| !s.is_empty()) else {
                continue;
            };
            match load_bundled_servers(Path::new(install_path)) {
                Ok(servers) => {
                    for cfg in servers {
                        if seen.insert(cfg.name().to_string()) {
                            out.push(cfg);
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        plugin = %pid,
                        error = %e,
                        "recover: failed to parse bundled servers, skipped"
                    );
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    // 注意（#94 教训）：本模块是**非 CLI** 层，测试**不得**依赖 cli-gated 的 `crate::cli::commands::test_env`，
    // 否则 `cargo test-ws`（默认特性、无 cli）将无法编译，反噬「GUI 无需 cli feature」目标。ledger 落 `home`
    // 内、`enabledPlugins` 经显式构造的 `declared` map 注入，故全 hermetic（`env = None`）。
    use super::*;
    use crate::settings::installer::{install_plugin, InstallOptions};
    use crate::settings::store::update_known_marketplaces;
    use crate::skills::staging::stage_marketplace_skills as stage;
    use serde_json::json;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use tempfile::TempDir;

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

    /// 构造 marketplace 源 git 仓库（audit plugin：1 skill + 1 bundled server）→ 返回 git source。
    fn build_source_repo(repo: &Path) -> Value {
        fs::create_dir_all(repo.join(".tfrobot-plugin")).unwrap();
        fs::write(
            repo.join(".tfrobot-plugin/marketplace.json"),
            r#"{"plugins": [{"name": "audit", "source": "./plugins/audit"}]}"#,
        )
        .unwrap();
        let skill = repo.join("plugins/audit/skills/code-review");
        fs::create_dir_all(&skill).unwrap();
        fs::write(
            skill.join("SKILL.md"),
            "---\nname: code-review\ndescription: review code\n---\nbody",
        )
        .unwrap();
        let sd = repo.join("plugins/audit/mcp-servers");
        fs::create_dir_all(&sd).unwrap();
        fs::write(
            sd.join("audit-mcp.json"),
            r#"{"type":"stdio","name":"audit-mcp","server_parameters":{"command":"node"}}"#,
        )
        .unwrap();
        git(&["init", "-q"], repo);
        git(&["add", "-A"], repo);
        git(&["commit", "-qm", "init"], repo);
        json!({"type": "git", "url": format!("file://{}", repo.display())})
    }

    /// 预 clone catalog + 写 known_marketplaces + install audit@acme（命令式 ledger 落盘）→ 返回 (home, source)。
    /// 模拟「重启前」治理态：ledger 已写，但**新进程 registry 为空**。
    async fn setup_installed(tmp: &TempDir) -> (PathBuf, Value) {
        let repo = tmp.path().join("repo");
        let source = build_source_repo(&repo);
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();

        // 预 clone catalog（throwaway registry）：install 要求 catalog 已存在。
        let mut throwaway = SkillRegistry::new();
        stage(
            "acme",
            &source,
            &mut throwaway,
            &home,
            MarketplaceStageOptions {
                refresh: true,
                ..Default::default()
            },
        )
        .await;
        let src_clone = source.clone();
        update_known_marketplaces(
            move |file| {
                file.account.marketplaces.insert(
                    "acme".to_string(),
                    crate::settings::reconciler::KnownMarketplaceEntry {
                        source: src_clone,
                        extra: Map::new(),
                    },
                );
            },
            Some(&home),
            None,
        )
        .unwrap();

        // install audit@acme（ledger-only，hooks=None）：写 installed_plugins.json。
        let mut reg = SkillRegistry::new();
        install_plugin(
            "audit@acme",
            &mut reg,
            &home,
            InstallOptions::default(),
            None,
        )
        .await
        .unwrap();
        (home, source)
    }

    // ---- 冷启动恢复 happy path（enabledPlugins 缺省 = 启用）-----------------------
    #[tokio::test]
    async fn recover_restages_enabled_installed_plugin() {
        let tmp = TempDir::new().unwrap();
        let (home, _src) = setup_installed(&tmp).await;

        // 模拟重启：全新空 registry + 空 declared（无 enabledPlugins → 缺省启用）。
        let mut fresh = SkillRegistry::new();
        let declared = Map::new();
        let report = recover_marketplace_skills(&mut fresh, &home, None, &declared).await;

        assert_eq!(report.restored_plugins, vec!["audit@acme".to_string()]);
        assert_eq!(
            report.restored_skills,
            vec!["audit:code-review".to_string()]
        );
        assert!(report.failed_marketplaces.is_empty());
        assert!(report.skipped_disabled.is_empty());
        assert!(fresh.resolve("audit:code-review").is_some(), "skill 应恢复");

        // 幂等：再调一次结果一致、不重复。
        let report2 = recover_marketplace_skills(&mut fresh, &home, None, &declared).await;
        assert_eq!(report2.restored_plugins, vec!["audit@acme".to_string()]);
        assert_eq!(
            report2.restored_skills,
            vec!["audit:code-review".to_string()]
        );
        assert!(fresh.resolve("audit:code-review").is_some());
    }

    // ---- enabledPlugins=false → 不复活（含已回滚半装 plugin 的核心门控）-------------
    #[tokio::test]
    async fn recover_skips_disabled_plugin_does_not_revive() {
        let tmp = TempDir::new().unwrap();
        let (home, _src) = setup_installed(&tmp).await;

        let mut fresh = SkillRegistry::new();
        let declared = json!({"enabledPlugins": {"audit@acme": false}})
            .as_object()
            .unwrap()
            .clone();
        let report = recover_marketplace_skills(&mut fresh, &home, None, &declared).await;

        assert!(report.restored_plugins.is_empty(), "禁用 plugin 不恢复");
        assert!(report.restored_skills.is_empty());
        assert_eq!(report.skipped_disabled, vec!["audit@acme".to_string()]);
        assert!(
            fresh.resolve("audit:code-review").is_none(),
            "禁用 plugin 的 skill 不应被复活"
        );

        // bundled server 采集亦跳过禁用 plugin。
        let servers = collect_enabled_bundled_servers(&home, None, &declared);
        assert!(
            servers.is_empty(),
            "禁用 plugin 的 bundled server 不应被采集"
        );
    }

    // ---- bundled server 采集（启用）----------------------------------------------
    #[tokio::test]
    async fn collect_returns_enabled_bundled_servers() {
        let tmp = TempDir::new().unwrap();
        let (home, _src) = setup_installed(&tmp).await;

        let declared = Map::new(); // 缺省启用
        let servers = collect_enabled_bundled_servers(&home, None, &declared);
        let names: Vec<&str> = servers.iter().map(MCPServerConfig::name).collect();
        assert_eq!(names, vec!["audit-mcp"]);
    }

    // ---- 降级铁律：marketplace 源不可达 / clone 树缺失 → failed、不 panic、不阻断 -----
    #[tokio::test]
    async fn recover_degrades_when_marketplace_unreachable() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();

        // 写一个指向不存在 repo 的 known_marketplaces + installed_plugins（无 clone 树）。
        let bad = format!("file://{}/nonexistent-repo", tmp.path().display());
        update_known_marketplaces(
            move |file| {
                file.account.marketplaces.insert(
                    "acme".to_string(),
                    crate::settings::reconciler::KnownMarketplaceEntry {
                        source: json!({"type": "git", "url": bad}),
                        extra: Map::new(),
                    },
                );
            },
            Some(&home),
            None,
        )
        .unwrap();
        crate::settings::store::update_installed_plugins(
            move |file| {
                file.account.plugins.insert(
                    "audit@acme".to_string(),
                    vec![crate::settings::reconciler::InstalledPluginRecord::default()],
                );
            },
            Some(&home),
            None,
        )
        .unwrap();

        let mut fresh = SkillRegistry::new();
        let declared = Map::new();
        let report = recover_marketplace_skills(&mut fresh, &home, None, &declared).await;

        assert_eq!(report.failed_marketplaces, vec!["acme".to_string()]);
        assert!(report.restored_plugins.is_empty());
        assert!(report.restored_skills.is_empty());
        assert!(fresh.is_empty(), "不可达源不入 registry");
    }

    // ---- 已装 plugin 的 marketplace 不在 known_marketplaces → 降级 ----------------
    #[tokio::test]
    async fn recover_degrades_when_marketplace_record_absent() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        // 仅写 installed_plugins，known_marketplaces 空（账本失配）。
        crate::settings::store::update_installed_plugins(
            move |file| {
                file.account.plugins.insert(
                    "audit@ghost".to_string(),
                    vec![crate::settings::reconciler::InstalledPluginRecord::default()],
                );
            },
            Some(&home),
            None,
        )
        .unwrap();

        let mut fresh = SkillRegistry::new();
        let declared = Map::new();
        let report = recover_marketplace_skills(&mut fresh, &home, None, &declared).await;
        assert_eq!(report.failed_marketplaces, vec!["ghost".to_string()]);
        assert!(report.restored_plugins.is_empty());
    }

    // ---- 空 home（无 ledger）→ 空报告、不 panic ----------------------------------
    #[tokio::test]
    async fn recover_empty_home_is_noop() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let mut fresh = SkillRegistry::new();
        let declared = Map::new();
        let report = recover_marketplace_skills(&mut fresh, &home, None, &declared).await;
        assert_eq!(report, GovernanceRecoveryReport::default());
        assert!(collect_enabled_bundled_servers(&home, None, &declared).is_empty());
    }
}
