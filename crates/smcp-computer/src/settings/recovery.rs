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
//! 关键事实（v0.3.0，协议 §2.4）：install 与 enable 分离——install 写 `installedPlugins` **全局安装意图** +
//! 物化，**不**写 `enabledPlugins`、**不**激活；enable 才写 `enabledPlugins=true` 并激活。故 boot 活跃集 =
//! 「已安装（`installedPlugins` 意图）」∧「`enabledPlugins` 合并为 `true`」，与
//! [`reconcile`](crate::settings::reconciler::reconcile) 侧 `enabled_plugin_names_for`（`== true`）语义一致。
//! 本模块 install-set 取自 `installedPlugins` 意图（账本 `installed_plugins.json` 仅供 `installPath` 等
//! materialization 细节），复用 reconcile 的**后端原语**（[`stage_marketplace_skills`] /
//! [`load_bundled_servers`] / store），**不**重写后端、**不**经 `reconcile()` 入口。
//!
//! ## 恢复语义（#95 范围）
//!
//! 1. **enabled 门控（v0.3.0 翻转）**：plugin 为「boot-active」⟺ 在 `installedPlugins` 意图 且
//!    `enabledPlugins[pid] == true`（absent / `false` 均**不**激活——install 不再装即活跃）。故仅 install 未
//!    enable 的 plugin 处于惰性 `installed_disabled`，boot **不**重挂其 skills / server。这条门控也承接「boot
//!    **不得**复活 Sub-A(#94) 中 hook 失败已回滚的半装 plugin」：enable-rollback 落定 `enabledPlugins=false`
//!    → 不复活。（#94 enable-rollback **窄残窗**：回写 false 也失败 → 账本残留 `true` → 据账本视其为启用重挂，
//!    与持久化态一致，属可容忍降级，见 [`installer`](crate::settings::installer) enable 回滚注释。）
//! 2. **additive-only**：只增不删（不 prune/gc 孤儿——那是 §7.3 显式入口）。重复调用幂等。
//! 3. **离线优先**：`refresh = false` → 已存在 clone 树**复用、不触网**；clone 树缺失才尝试 clone。
//! 4. **降级铁律**（§7.2）：marketplace 源不可达 / clone 树缺失且 clone 失败 → 该 marketplace 入
//!    `failed_marketplaces` + WARN，**不**阻断恢复其余、**不**抛。
//!
//! ## 阶段划分（锁纪律）/ Phases (lock discipline)
//!
//! 本模块提供**独立**入口，由 [`Computer`](crate::computer::Computer) 编排以**断开**潜在 ABBA：
//! - 阶段一 [`recover_marketplace_skills`]（持 `skill_registry` 写锁）：重挂 marketplace skills。
//! - 阶段一·五 [`rematerialize_missing_ledger_records`]（**不**持任何锁，纯 FS/git）：账本被外部删除后从
//!   `installedPlugins` 意图重建缺失的账本派生缓存（`installPath`），使阶段二得以解析 bundled server（§63）。
//! - 阶段二 [`collect_enabled_bundled_servers`]（**不**持任何锁，纯读 ledger + 解析）：产出待重挂的 bundled
//!   server 配置；调用方在**释放 skill 写锁后**经 [`McpInstallHooks`](crate::settings::installer::McpInstallHooks)
//!   逐个 `register_server`（client 拥有「如何物化」，SDK 决定「哪些」）。阶段分离避免「持 skill 写锁 →
//!   经 hooks 取 mcp_manager 锁」的相反序死锁（见 computer.rs `restage_mcp_skills` 锁序注释）。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde_json::{Map, Value};

use crate::mcp_clients::bundle_id::resolve_bundle_id;
use crate::mcp_clients::model::{BundleId, MCPServerConfig};
use crate::settings::installer::materialize_plugin_record;
use crate::settings::scope::EnvMap;
use crate::settings::store::{
    load_installed_plugins, load_installed_plugins_intent, load_known_marketplaces,
};
use crate::skills::home::marketplace_skill_dir;
use crate::skills::manifest::load_bundled_servers;
use crate::skills::registry::SkillRegistry;
use crate::skills::staging::{
    stage_marketplace_skills, MarketplaceStageOptions, DEFAULT_GIT_TIMEOUT,
};

// ===========================================================================
// 恢复报告 / Recovery report
// ===========================================================================
/// 一次 [`recover_marketplace_skills`] + MCP 重挂的结果（观测 + 测试信号）/ Boot recovery report。
///
/// `remounted_servers` 由编排方（[`Computer`](crate::computer::Computer)）在第二阶段填充——本模块的
/// [`recover_marketplace_skills`] 仅产出 skills 维度，`remounted_servers` 留空。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GovernanceRecoveryReport {
    /// **marketplace clone 树存在（可达）故已尝试重挂**的启用 plugin id（`<plugin>@<mp>`）/ attempted plugins。
    ///
    /// ⚠️ 语义为「**clone 可达 + 已尝试 stage**」，**非**「保证注册了 skill」：成功判定以 clone 树存在为代理
    /// （[`stage_marketplace_skills`] 失败降级吞错）。clone 存在但 manifest 损坏 / 条目非法时，pid 仍计入此处、
    /// 但 [`restored_skills`](Self::restored_skills) 可能不含其 skill。**`restored_skills` 才是已注册 skill 的
    /// 权威清单**（亦含合法但无 SKILL.md、仅带 bundled server 的 plugin → 不在 restored_skills 但在此）。
    pub restored_plugins: Vec<String>,
    /// 本次重新注册的全部 marketplace SKILL 名（**已注册 skill 的权威清单**）/ all SKILL names re-registered。
    pub restored_skills: Vec<String>,
    /// 经 hooks 成功重挂的 bundled MCP server 名（编排方第二阶段填充）/ bundled servers remounted via hooks。
    pub remounted_servers: Vec<String>,
    /// 源不可达 / clone 树缺失且 clone 失败的 marketplace（降级、未恢复其 skills）/ degraded marketplaces。
    pub failed_marketplaces: Vec<String>,
    /// 显式 `enabledPlugins=false` 故**刻意跳过**的已装 plugin id（不复活）/ deliberately-skipped disabled。
    pub skipped_disabled: Vec<String>,
    /// 账本缺记录、从 `installedPlugins` 意图**重物化派生缓存成功**的 enabled plugin id（§63）/ rebuilt from intent。
    pub rematerialized_plugins: Vec<String>,
    /// 账本缺记录但**重物化失败**（源不可达等）的 enabled plugin id（降级、未阻断其余）/ degraded rematerialize。
    pub failed_rematerialize: Vec<String>,
}

// ===========================================================================
// enabled 门控 / Enabled gating
// ===========================================================================
/// plugin 启用维度门控：`enabledPlugins[pid] == true`（v0.3.0 翻转）/ enabled gate。
///
/// v0.3.0（协议 §2.4）：install 与 enable 分离后 install **不**写 `enabledPlugins`，故 **absent = 未启用**
/// （惰性 `installed_disabled`），`false` 亦不启用，仅显式 `true` 才激活——与 reconcile 侧
/// [`enabled_plugin_names_for`](crate::settings::reconciler) 的 `== true` 一致。boot 活跃集 =「已安装
/// （`installedPlugins` 意图）」∧ 本 gate。存量 v0.2.x 账本（无 flag）经 boot 一次性迁移回填
/// `enabledPlugins=true`，不受本翻转影响（见 [`Computer::boot_up`](crate::computer::Computer)）。
///
/// ⚠️ `declared` 的**完整性由调用方负责**：仅当启用旗所在 scope 已并入 `declared` 才生效。#98 后 project/local
/// 层来自**进程 cwd**——写在**非进程-cwd 的 project/local scope** 的 `enabledPlugins=true` 可能不在 `declared`
/// 内 → 此处误判为未启用。跨重启可靠启用应写 user scope（见
/// [`Computer::reconcile_governance`](crate::computer::Computer::reconcile_governance) 调用方须知）。
#[must_use]
fn plugin_enabled(declared: &Map<String, Value>, pid: &str) -> bool {
    matches!(
        declared
            .get("enabledPlugins")
            .and_then(|plugins| plugins.get(pid)),
        Some(Value::Bool(true))
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
    // install-set 取自 `installedPlugins` 意图（v0.3.0 权威）；账本仅供 materialization 细节，不再当 install-set。
    let intent = load_installed_plugins_intent(Some(home), env).account;
    let known = load_known_marketplaces(Some(home), env).account;

    let mut report = GovernanceRecoveryReport::default();
    // 按 marketplace 分组启用 plugin（保意图首见顺序）：mp → (启用 plugin 名集, 启用 pid 列表)。
    let mut by_marketplace: IndexMap<String, (HashSet<String>, Vec<String>)> = IndexMap::new();

    for pid in &intent.installed_plugins {
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
// 阶段二输入：待重挂 bundled server 记录采集 / Phase 2 input: bundled servers to remount
// ===========================================================================
/// 一条**已启用 plugin 派生的 bundled MCP server** 恢复记录（携完整归属）/ one enabled bundled MCP server。
///
/// 对标 Python `a2c_smcp/computer/settings/recovery.py::BundledServerRecord`（严格同构：字段
/// `plugin_id` / `plugin` / `marketplace` / `install_path` / `config`）。既是 boot 第二阶段经 hooks 重挂的
/// 输入，也是 [`Computer::list_mcp_servers_with_metadata`](crate::computer::Computer::list_mcp_servers_with_metadata)
/// 归属推导的**唯一**来源：归属为 ledger + manifest 的纯函数输出（protocol v0.2.3 §4.8.3——意图 + resolved
/// location + manifest 重推导，每次 boot 可复现），**不**依赖任何调用方持有的内存 ownership map。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundledServerRecord {
    /// plugin id：`<plugin>@<marketplace>` / plugin id。
    pub plugin_id: String,
    /// plugin 名（pid `@` 前段；本地-only 记录 = 整个 pid）/ plugin name。
    pub plugin: String,
    /// marketplace 名（pid `@` 后段；本地-only 记录留空）/ marketplace name。
    pub marketplace: String,
    /// plugin 物化落点（bundled server 从此解析）/ install path。
    pub install_path: PathBuf,
    /// bundled MCP server 配置 / bundled MCP server config。
    pub config: MCPServerConfig,
}

/// 采集**已装且启用** plugin 的 bundled MCP server 记录（供调用方经 hooks 重挂 / 归属查询）/ Collect bundled servers。
///
/// 从每条 [`InstalledPluginRecord`](crate::settings::reconciler::InstalledPluginRecord) 的 `installPath`
/// 重解析 [`load_bundled_servers`]（与 [`enable_plugin`](crate::settings::installer::enable_plugin) 同源、
/// 不重 clone）。enabled 门控同 [`recover_marketplace_skills`]：`enabledPlugins=false` 的 plugin **不**采集
/// （其 server 保持下线，含已回滚的半装 plugin）。跨 plugin / scope 按 server 名去重（首见保留）。每条结果携
/// 完整归属（[`BundledServerRecord`]），供 `list_mcp_servers_with_metadata` 直接映射 `managedBy=plugin`。
///
/// **无锁、纯读**：不持任何 Registry / manager 锁；调用方在释放 skill 写锁后逐个 `register_server`，避免
/// 「skill 写锁 → mcp_manager 锁」相反序死锁。解析失败 → WARN 跳过该 plugin（降级、不阻断）。
#[must_use]
pub fn collect_enabled_bundled_servers(
    home: &Path,
    env: Option<&EnvMap>,
    declared: &Map<String, Value>,
) -> Vec<BundledServerRecord> {
    // 权威 install-set = `installedPlugins` 意图；账本仅供 installPath / bundled 细节。
    let intent = load_installed_plugins_intent(Some(home), env).account;
    let installed = load_installed_plugins(Some(home), env).account;
    let mut seen: HashSet<BundleId> = HashSet::new();
    let mut out: Vec<BundledServerRecord> = Vec::new();

    for (pid, records) in &installed.plugins {
        // 不在安装意图的账本记录 = 陈旧派生缓存（已 uninstall / 待 gc），忽略。
        if !intent.installed_plugins.contains(pid) {
            continue;
        }
        if !plugin_enabled(declared, pid) {
            continue;
        }
        // pid = `<plugin>@<marketplace>`；本地-only（无 '@'）记录**跳过**——与同文件 recover_marketplace_skills
        // (`:146`) 及 Python `collect_enabled_bundled_servers`（`_split_pid` → continue）一致：非 marketplace
        // plugin，归属恢复不涉及（严格同构；避免以退化 `marketplace:""` 归属误入 inventory）。
        let Some((plugin, marketplace)) = pid.split_once('@') else {
            continue;
        };
        for rec in records {
            let Some(install_path) = rec.install_path.as_deref().filter(|s| !s.is_empty()) else {
                continue;
            };
            match load_bundled_servers(Path::new(install_path)) {
                Ok(servers) => {
                    for cfg in servers {
                        // no-double-open：按 `bundle_id` first-wins 去重（协议 0.3.0，rust-sdk#117）。bundled
                        // config 未 render，无名 server 的 fallback 摘要基于未注入 inputs 的连接身份——对 collect
                        // 自身的去重内部自洽即可（manager 注册期会以 rendered config 再次按 bundle_id 去重兜底）。
                        if seen.insert(resolve_bundle_id(&cfg)) {
                            out.push(BundledServerRecord {
                                plugin_id: pid.clone(),
                                plugin: plugin.to_string(),
                                marketplace: marketplace.to_string(),
                                install_path: PathBuf::from(install_path),
                                config: cfg,
                            });
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

// ===========================================================================
// 阶段一·五：账本派生缓存补全（§63 账本删除无损）/ Phase 1.5: rebuild ledger from intent
// ===========================================================================
/// 账本删除/损坏后从 `installedPlugins` 意图**重物化缺失的账本派生缓存**（conformance §63）/ Rebuild missing ledger records。
///
/// v0.3.0 权威模型：`installedPlugins` 意图是「已安装」唯一权威，账本 `installed_plugins.json` 是可从意图重建的
/// 派生缓存。skills 恢复（[`recover_marketplace_skills`]）已不依赖账本，但 bundled MCP server 恢复
/// （[`collect_enabled_bundled_servers`]）仍需账本记录的 `installPath`。故账本被外部删除后，「意图有 enabled pid
/// 但账本无记录」的 plugin 其 bundled server 会静默丢失。本函数补齐这一步：遍历意图，为 enabled 且账本缺记录的
/// marketplace plugin 调 [`materialize_plugin_record`] 重建，之后 `collect` / 归属查询即可重现。
///
/// **门控**（与 phase 1 / phase 2 同构）：仅 `<plugin>@<marketplace>`（本地-only 无 bundled 归属恢复）且
/// `enabledPlugins[pid] == true` 才重建；账本已有**非空 `installPath`** 记录则跳过（幂等 + 自愈）。
///
/// **降级铁律**（§7.2）：单个 plugin 重物化失败（源不可达 / manifest 畸形）→ 记入
/// [`failed_rematerialize`](GovernanceRecoveryReport::failed_rematerialize) + WARN，**不 panic、不阻断其余**。
///
/// **无锁、离线优先**：不持任何 Registry / manager 锁；`materialize_plugin_record` 内 `refresh=false` 复用既有
/// clone（catalog 通常已由 phase 1 clone）。编排方（[`Computer::reconcile_governance`](crate::computer::Computer::reconcile_governance)）
/// 在 phase 1 释放 skill 写锁后、phase 2 之前调用（断开 ABBA）。
pub async fn rematerialize_missing_ledger_records(
    home: &Path,
    env: Option<&EnvMap>,
    // #139：**不再** gate on `enabledPlugins`（保留形参仅为签名稳定，故 `_`）——见下方循环内注释。
    _declared: &Map<String, Value>,
    report: &mut GovernanceRecoveryReport,
) {
    // 权威 install-set = `installedPlugins` 意图；单次账本快照供「已有记录」判定（逐 pid 只写自身、pid 唯一 →
    // 快照跨迭代无 staleness）。
    let intent = load_installed_plugins_intent(Some(home), env).account;
    let installed = load_installed_plugins(Some(home), env).account;

    for pid in &intent.installed_plugins {
        // 仅 marketplace plugin（本地-only 无 '@' → 无 bundled 归属恢复，与 collect/recover 同构）。
        if pid.split_once('@').is_none() {
            continue;
        }
        // #139：**不** gate on enabled——`installed_disabled` 也重建账本**派生缓存**（物化 ≠ 激活：
        // `materialize_plugin_record` 只 clone + 读 manifest + 写账本，**不** stage skills、**不** mount server；
        // 激活仍由后续 enabled 门控的 skills/mount 路径负责）。对齐 python `recover_marketplace_skills`
        // （`needs_materialize` 按账本完好度、非 enabled 判定）+ 协议 §4.9「账本删除无损」（对 disabled 亦无损）。
        // ⚠️ 旧 gate 在 #139 whole-record-drop 迁移下会让「disabled + 旧格式 + bundled」插件账本永失、
        // `plugin enable` 谎报「未安装」不可复原（隔离复审逮到的回归）。
        // 账本已有非空 installPath 记录 → 派生缓存健在，无需重建（幂等）。
        let has_usable_record = installed.plugins.get(pid).is_some_and(|recs| {
            recs.iter()
                .any(|r| r.install_path.as_deref().is_some_and(|s| !s.is_empty()))
        });
        if has_usable_record {
            continue;
        }
        // 从意图重物化派生缓存（离线优先；失败降级、不阻断其余）。
        match materialize_plugin_record(pid, home, env, DEFAULT_GIT_TIMEOUT).await {
            Ok(_) => {
                tracing::info!(plugin = %pid, "recover: rebuilt ledger record from installedPlugins intent (§63)");
                report.rematerialized_plugins.push(pid.clone());
            }
            Err(e) => {
                tracing::warn!(plugin = %pid, error = %e, "recover: rematerialize ledger record failed (degraded, non-blocking)");
                report.failed_rematerialize.push(pid.clone());
            }
        }
    }
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

    // ---- 冷启动恢复 happy path（installed ∧ enabledPlugins==true）--------------------
    #[tokio::test]
    async fn recover_restages_enabled_installed_plugin() {
        let tmp = TempDir::new().unwrap();
        let (home, _src) = setup_installed(&tmp).await;

        // 模拟重启：全新空 registry + declared 显式启用 audit@acme（v0.3.0：absent 不再默认启用）。
        let mut fresh = SkillRegistry::new();
        let declared = declared_enabled(&["audit@acme"]);
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

        let declared = declared_enabled(&["audit@acme"]);
        let servers = collect_enabled_bundled_servers(&home, None, &declared);
        let names: Vec<&str> = servers.iter().map(|r| r.config.name()).collect();
        assert_eq!(names, vec!["audit-mcp"]);
        // 归属为 ledger 纯函数输出（§4.8.3）：pid `audit@acme` → plugin/marketplace 分段。
        let rec = &servers[0];
        assert_eq!(rec.plugin_id, "audit@acme");
        assert_eq!(rec.plugin, "audit");
        assert_eq!(rec.marketplace, "acme");
    }

    // ---- §63：账本删除后从 installedPlugins 意图重建 bundled server（#104）----------
    #[tokio::test]
    async fn rematerialize_rebuilds_bundled_server_after_ledger_deleted() {
        let tmp = TempDir::new().unwrap();
        let (home, _src) = setup_installed(&tmp).await;
        let declared = declared_enabled(&["audit@acme"]);

        // 前置：账本在 → collect 有 audit-mcp。
        assert_eq!(
            collect_enabled_bundled_servers(&home, None, &declared).len(),
            1,
            "前置：账本在时应有 1 个 bundled server"
        );

        // 删账本（模拟外部删除/损坏）——`installedPlugins` 意图仍在。
        fs::remove_file(crate::settings::store::installed_plugins_path(
            Some(&home),
            None,
        ))
        .unwrap();
        // 缺口现状：collect 从（已删）账本取不到 installPath → 空。
        assert!(
            collect_enabled_bundled_servers(&home, None, &declared).is_empty(),
            "缺口现状：删账本后 collect 为空"
        );

        // §63 重建：从意图重物化账本派生缓存。
        let mut report = GovernanceRecoveryReport::default();
        rematerialize_missing_ledger_records(&home, None, &declared, &mut report).await;
        assert_eq!(
            report.rematerialized_plugins,
            vec!["audit@acme".to_string()],
            "enabled plugin 的账本记录应被重建"
        );
        assert!(report.failed_rematerialize.is_empty());

        // 重建后 collect 重现 bundled server + 归属（恢复不受影响）。这三段归属字段正是
        // `Computer::list_mcp_servers_with_metadata` 的 `plugin_ownership` 纯映射输入（§4.8.3：
        // `managedBy=Plugin{marketplace,plugin,plugin_id}`），故归属重现由此确定性传递（inventory 层归属映射
        // 因 Computer 无 settings 注入 seam 无法 hermetic 断言，按项目惯例在 collect 层覆盖）。
        let servers = collect_enabled_bundled_servers(&home, None, &declared);
        let names: Vec<&str> = servers.iter().map(|r| r.config.name()).collect();
        assert_eq!(names, vec!["audit-mcp"], "重建后 bundled server 应重现");
        assert_eq!(servers[0].plugin_id, "audit@acme");
        assert_eq!(servers[0].plugin, "audit");
        assert_eq!(servers[0].marketplace, "acme");
    }

    // ---- §63 降级：catalog 从未 clone（前置早退）→ failed_rematerialize、不 panic、不阻断（#104）----
    #[tokio::test]
    async fn rematerialize_degrades_when_catalog_missing() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();

        // 意图有 enabled pid，但无账本、无 catalog clone、known_marketplaces 指向不存在 repo。
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
        seed_intent(&home, &["audit@acme"]);

        let declared = declared_enabled(&["audit@acme"]);
        let mut report = GovernanceRecoveryReport::default();
        rematerialize_missing_ledger_records(&home, None, &declared, &mut report).await;

        assert_eq!(
            report.failed_rematerialize,
            vec!["audit@acme".to_string()],
            "不可达 → 降级记入 failed_rematerialize"
        );
        assert!(report.rematerialized_plugins.is_empty());
        // 未重建 → collect 仍空（不 panic、不阻断）。
        assert!(collect_enabled_bundled_servers(&home, None, &declared).is_empty());
    }

    // ---- §63 降级：catalog 在但 plugin 根缺失（locate 期失败，非前置早退）→ failed_rematerialize（#104）----
    #[tokio::test]
    async fn rematerialize_degrades_when_plugin_root_missing() {
        let tmp = TempDir::new().unwrap();
        let (home, _src) = setup_installed(&tmp).await;
        let declared = declared_enabled(&["audit@acme"]);

        // 删账本 + 删 catalog 内 plugin 源子树：catalog/known_marketplaces 仍在 → 过 catalog 前置 + manifest +
        // find_entry，但 `locate_plugin_root` 解析出的 plugin 根非目录 → 真正的 locate 期 Err（区别于 catalog 未 clone）。
        fs::remove_file(crate::settings::store::installed_plugins_path(
            Some(&home),
            None,
        ))
        .unwrap();
        let plugin_root = marketplace_skill_dir(&home, "acme", &[]).join("plugins/audit");
        fs::remove_dir_all(&plugin_root).unwrap();

        let mut report = GovernanceRecoveryReport::default();
        rematerialize_missing_ledger_records(&home, None, &declared, &mut report).await;
        assert_eq!(
            report.failed_rematerialize,
            vec!["audit@acme".to_string()],
            "plugin 根缺失 → locate 期降级"
        );
        assert!(report.rematerialized_plugins.is_empty());
        assert!(collect_enabled_bundled_servers(&home, None, &declared).is_empty());
    }

    // ---- §63/#139：disabled plugin 账本删后**也重建派生缓存**（物化 ≠ 激活，对齐 python）--------------
    // 推翻旧 `rematerialize_skips_disabled_plugin`（#104 惰性 gate 与 python 分叉；#139 whole-record-drop
    // 迁移下该 gate 会让 disabled 旧格式插件账本永失、`plugin enable` 谎报「未安装」）。
    #[tokio::test]
    async fn rematerialize_rebuilds_disabled_plugin_ledger_but_not_active_139() {
        let tmp = TempDir::new().unwrap();
        let (home, _src) = setup_installed(&tmp).await;
        fs::remove_file(crate::settings::store::installed_plugins_path(
            Some(&home),
            None,
        ))
        .unwrap();

        // declared 未启用 audit@acme（absent = 未启用）。
        let declared = Map::new();
        let mut report = GovernanceRecoveryReport::default();
        rematerialize_missing_ledger_records(&home, None, &declared, &mut report).await;

        // 账本派生缓存**重建**（installed_disabled 也重建，§4.9 删除无损）——记录回来、可查询、可 enable。
        assert_eq!(
            report.rematerialized_plugins,
            vec!["audit@acme".to_string()],
            "disabled plugin 的账本记录 MUST 重建（物化 ≠ 激活）"
        );
        assert!(report.failed_rematerialize.is_empty());
        let installed = load_installed_plugins(Some(&home), None).account;
        assert!(
            installed.plugins.contains_key("audit@acme"),
            "重建后账本含该 pid（可查询/可 enable）"
        );
        // 但**未激活**：collect_enabled_bundled_servers 仍空（激活由 enabled 门控，rematerialize 不 stage/mount）。
        assert!(
            collect_enabled_bundled_servers(&home, None, &declared).is_empty(),
            "物化 ≠ 激活：disabled plugin 的 bundled server 不进活跃集"
        );
    }

    // ---- §63：重建幂等（已有非空 installPath 记录则跳过，#104）------------------------
    #[tokio::test]
    async fn rematerialize_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let (home, _src) = setup_installed(&tmp).await;
        let declared = declared_enabled(&["audit@acme"]);
        fs::remove_file(crate::settings::store::installed_plugins_path(
            Some(&home),
            None,
        ))
        .unwrap();

        // 首次重建。
        let mut r1 = GovernanceRecoveryReport::default();
        rematerialize_missing_ledger_records(&home, None, &declared, &mut r1).await;
        assert_eq!(r1.rematerialized_plugins, vec!["audit@acme".to_string()]);

        // 再次调用：账本已有非空 installPath 记录 → 跳过、不重复重建。
        let mut r2 = GovernanceRecoveryReport::default();
        rematerialize_missing_ledger_records(&home, None, &declared, &mut r2).await;
        assert!(r2.rematerialized_plugins.is_empty(), "已有记录 → 幂等跳过");
        assert!(r2.failed_rematerialize.is_empty());
        // collect 仍正常。
        assert_eq!(
            collect_enabled_bundled_servers(&home, None, &declared).len(),
            1
        );
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

        seed_intent(&home, &["audit@acme"]);
        let mut fresh = SkillRegistry::new();
        let declared = declared_enabled(&["audit@acme"]);
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

        seed_intent(&home, &["audit@ghost"]);
        let mut fresh = SkillRegistry::new();
        let declared = declared_enabled(&["audit@ghost"]);
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

    /// 建启用意图 declared map：`{"enabledPlugins": {pid: true, ...}}`（v0.3.0：absent 不再默认启用）/ enabled declared。
    fn declared_enabled(pids: &[&str]) -> Map<String, Value> {
        let mut inner = Map::new();
        for pid in pids {
            inner.insert((*pid).to_string(), Value::Bool(true));
        }
        let mut m = Map::new();
        m.insert("enabledPlugins".to_string(), Value::Object(inner));
        m
    }

    /// 写 `installedPlugins` 全局安装意图（v0.3.0：boot install-set 权威来源）/ seed the install intent。
    fn seed_intent(home: &Path, pids: &[&str]) {
        let owned: Vec<String> = pids.iter().map(|s| (*s).to_string()).collect();
        crate::settings::store::update_installed_plugins_intent(
            move |file| {
                for pid in owned {
                    file.account.installed_plugins.insert(pid);
                }
            },
            Some(home),
            None,
        )
        .unwrap();
    }

    // ---- 直接写 installed_plugins 记录（绕过 install）/ seed an install record -------
    fn seed_install_record(home: &Path, pid: &str, install_path: Option<&Path>) {
        seed_intent(home, &[pid]); // v0.3.0：同时登记安装意图，否则 recover/collect 的 install-set 取不到。
        let pid_owned = pid.to_string();
        let ip = install_path.map(|p| p.to_string_lossy().into_owned());
        crate::settings::store::update_installed_plugins(
            move |file| {
                file.account.plugins.insert(
                    pid_owned,
                    vec![crate::settings::reconciler::InstalledPluginRecord {
                        install_path: ip,
                        mcp_servers: Vec::new(),
                        extra: Map::new(),
                    }],
                );
            },
            Some(home),
            None,
        )
        .unwrap();
    }

    /// 造一个含单个 bundled server 文件的 plugin 根 / a plugin root with one bundled server file。
    fn plugin_root_with_server(root: &Path, server_name: &str) -> PathBuf {
        plugin_root_with_server_bid(root, server_name, None)
    }

    /// 同上，但可注入**显式** `bundle_id`（#142 / R5②）——令 display 名与身份**可控地分叉**。
    ///
    /// 缺省（`None`）走 `derive_bundle_id` ⇒ `bundle_id == normalize_name(name)`：同 display 名必同 id，
    /// 于是「按 name 去重」与「按 bundle_id 去重」两种实现在该夹具下**双双通过**（零鉴别力）。要让去重键
    /// 的语义真正可被断言分辨，必须至少一方**显式声明** `bundle_id`（conformance §2.0-2）。
    fn plugin_root_with_server_bid(
        root: &Path,
        server_name: &str,
        bundle_id: Option<&str>,
    ) -> PathBuf {
        let sd = root.join("mcp-servers");
        fs::create_dir_all(&sd).unwrap();
        let bid_field = bundle_id.map_or_else(String::new, |b| format!(r#","bundle_id":"{b}""#));
        fs::write(
            sd.join(format!("{server_name}.json")),
            format!(
                r#"{{"type":"stdio","name":"{server_name}"{bid_field},"server_parameters":{{"command":"node"}}}}"#
            ),
        )
        .unwrap();
        root.to_path_buf()
    }

    // ---- 🟡7：clone 存在但 manifest 损坏 → pid 计入 restored_plugins 但 restored_skills 空（钉记文档语义）----
    #[tokio::test]
    async fn recover_present_clone_broken_manifest_overcounts_plugins() {
        let tmp = TempDir::new().unwrap();
        let (home, _src) = setup_installed(&tmp).await;
        // 损坏 clone 的 marketplace.json（保留 clone 树存在）。
        let manifest = crate::skills::home::marketplace_skill_dir(&home, "acme", &[])
            .join(".tfrobot-plugin/marketplace.json");
        fs::write(&manifest, b"{ not valid json").unwrap();

        let mut fresh = SkillRegistry::new();
        let report =
            recover_marketplace_skills(&mut fresh, &home, None, &declared_enabled(&["audit@acme"]))
                .await;
        // clone 树存在 → 不算 failed；pid 计入 restored_plugins；但无 skill 注册。
        assert_eq!(report.restored_plugins, vec!["audit@acme".to_string()]);
        assert!(report.failed_marketplaces.is_empty(), "clone 存在不算降级");
        assert!(
            report.restored_skills.is_empty(),
            "manifest 损坏 → 无 skill 注册（restored_skills 才是权威）"
        );
    }

    // ---- 🟡8a：显式 enabledPlugins=true → 恢复（三态之 true）----------------------
    #[tokio::test]
    async fn recover_respects_explicit_enabled_true() {
        let tmp = TempDir::new().unwrap();
        let (home, _src) = setup_installed(&tmp).await;
        let declared = json!({"enabledPlugins": {"audit@acme": true}})
            .as_object()
            .unwrap()
            .clone();
        let mut fresh = SkillRegistry::new();
        let report = recover_marketplace_skills(&mut fresh, &home, None, &declared).await;
        assert_eq!(report.restored_plugins, vec!["audit@acme".to_string()]);
        assert!(fresh.resolve("audit:code-review").is_some());
    }

    // ---- 🟡8b：无 '@' 的本地-only pid → 跳过（非 marketplace plugin，不 restored 不 skipped）----
    #[tokio::test]
    async fn recover_skips_local_only_pid_without_at() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        seed_install_record(&home, "local-only", None);
        let mut fresh = SkillRegistry::new();
        let report = recover_marketplace_skills(&mut fresh, &home, None, &Map::new()).await;
        assert!(report.restored_plugins.is_empty());
        assert!(
            report.skipped_disabled.is_empty(),
            "无 '@' 非禁用、不计 skipped"
        );
        assert!(report.failed_marketplaces.is_empty());
    }

    // ---- 🟡8c/8d：collect 跳过损坏 bundled JSON + install_path None ----------------
    #[tokio::test]
    async fn collect_skips_broken_json_and_none_install_path() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();

        let good = plugin_root_with_server(&tmp.path().join("good"), "good-mcp");
        // 损坏 bundled JSON：mcp-servers/bad.json 非法。
        let bad = tmp.path().join("bad");
        fs::create_dir_all(bad.join("mcp-servers")).unwrap();
        fs::write(bad.join("mcp-servers/bad.json"), b"{ not json").unwrap();

        seed_install_record(&home, "good@acme", Some(&good));
        seed_install_record(&home, "bad@acme", Some(&bad));
        seed_install_record(&home, "nopath@acme", None);

        let servers = collect_enabled_bundled_servers(
            &home,
            None,
            &declared_enabled(&["good@acme", "bad@acme", "nopath@acme"]),
        );
        let names: Vec<&str> = servers.iter().map(|r| r.config.name()).collect();
        assert_eq!(
            names,
            vec!["good-mcp"],
            "损坏 JSON + 无 install_path 均跳过"
        );
    }

    // ---- #97：collect 跳过无 '@' 的本地-only pid（与 recover_marketplace_skills / Python 一致）--------
    #[tokio::test]
    async fn collect_skips_local_only_pid_without_at() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        // 本地-only pid（无 '@'）虽带 bundled server，亦不采集——非 marketplace plugin，归属不涉及。
        let local = plugin_root_with_server(&tmp.path().join("local"), "local-mcp");
        seed_install_record(&home, "local-only", Some(&local));
        // 对照：正常 marketplace pid 仍采集。
        let good = plugin_root_with_server(&tmp.path().join("good"), "good-mcp");
        seed_install_record(&home, "good@acme", Some(&good));

        let servers =
            collect_enabled_bundled_servers(&home, None, &declared_enabled(&["good@acme"]));
        let names: Vec<&str> = servers.iter().map(|r| r.config.name()).collect();
        assert_eq!(
            names,
            vec!["good-mcp"],
            "无 '@' 本地-only pid 的 bundled server 不采集（归属恢复不涉及）"
        );
    }

    // ---- 🟡8e：跨 plugin **同 bundle_id** 的 bundled server 去重（首见保留）------------------------
    /// 两 plugin 声明同 display 名且**均走缺省派生** ⇒ 同 `bundle_id` ⇒ no-double-open 去重为一条。
    ///
    /// **#142 命名订正**：原名 `collect_dedups_same_named_servers_across_plugins` 描述的是 **name-keyed**
    /// 语义，与生产实现（`seen: HashSet<BundleId>` + `resolve_bundle_id`）正好相反。此夹具下 name 与
    /// bundle_id 恰好重合，两种键法都过——它守的其实是「同 **id** ⇒ 去重」，故按 id 更名。真正能分辨去重键
    /// 的用例是下面的 `collect_keeps_same_name_distinct_bundle_id`。
    #[tokio::test]
    async fn collect_dedups_same_bundle_id_servers_across_plugins() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let a = plugin_root_with_server(&tmp.path().join("a"), "shared-mcp");
        let b = plugin_root_with_server(&tmp.path().join("b"), "shared-mcp");
        seed_install_record(&home, "a@acme", Some(&a));
        seed_install_record(&home, "b@acme", Some(&b));

        let servers =
            collect_enabled_bundled_servers(&home, None, &declared_enabled(&["a@acme", "b@acme"]));
        let names: Vec<&str> = servers.iter().map(|r| r.config.name()).collect();
        assert_eq!(
            names,
            vec!["shared-mcp"],
            "同 bundle_id server 跨 plugin 去重"
        );
    }

    // ---- #142 / R5②：跨 plugin 同 display 名 + **显式异 bundle_id** → 两条各自保留 --------------
    /// 去重键是 **bundle_id（身份）而非 display name**：同 display 名但显式异 `bundle_id` 是两个**不同身份**
    /// 的 server，MUST 各自保留（协议 data-structures.md §BundleID no-double-open 同键）。
    ///
    /// English: same display name but explicitly distinct bundle_id ⇒ both kept (dedup key is bundle_id, not name).
    ///
    /// **本用例是「假绿」修复的核心**：上面那条同名用例走缺省派生 ⇒ 同名必同 id ⇒ 无论按 name 还是按
    /// bundle_id 去重都通过，对真实契约零覆盖。此处显式注入异 `bundle_id` 令两识别空间分叉，去重键一旦
    /// 退回 `config.name()` 本用例即红（变异验证已实测）。
    ///
    /// 双端对拍：python-sdk `test_collect_keeps_same_name_distinct_bundle_id`（`2fc8428`）逐条同构。
    /// 注：python 侧此用例曾是**真红灯**（其 `collect_enabled_bundled_servers` 当时按 `config.name` 去重，
    /// 随该提交一并根治）；rust 侧生产实现自 #117 起即按 `resolve_bundle_id` 去重，故本用例是**回归栅栏**。
    #[tokio::test]
    async fn collect_keeps_same_name_distinct_bundle_id() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        // 同 display 名 "shared"，两 plugin 根不同 ⇒ 文件名不碰撞；显式异 bundle_id ⇒ 两个不同身份的 server。
        let a = plugin_root_with_server_bid(&tmp.path().join("a"), "shared", Some("shared-a"));
        let b = plugin_root_with_server_bid(&tmp.path().join("b"), "shared", Some("shared-b"));
        seed_install_record(&home, "a@acme", Some(&a));
        seed_install_record(&home, "b@acme", Some(&b));

        let servers =
            collect_enabled_bundled_servers(&home, None, &declared_enabled(&["a@acme", "b@acme"]));

        assert_eq!(
            servers.len(),
            2,
            "同 display 名 + 异 bundle_id MUST 各自保留（去重键 = 身份，非 name）"
        );
        let bids: HashSet<String> = servers
            .iter()
            .map(|r| resolve_bundle_id(&r.config).into_string())
            .collect();
        assert_eq!(
            bids,
            ["shared-a".to_string(), "shared-b".to_string()]
                .into_iter()
                .collect::<HashSet<String>>()
        );
        // display 名碰撞合法、非身份——两条的 name 本就该相同。
        assert!(servers.iter().all(|r| r.config.name() == "shared"));
    }

    // ---- 🟡8f：单 marketplace 下多 plugin 分组恢复 --------------------------------
    #[tokio::test]
    async fn recover_groups_multiple_plugins_one_marketplace() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();

        // 两 plugin（audit/lint）的源仓 → stage catalog + 写 known_marketplaces。
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join(".tfrobot-plugin")).unwrap();
        fs::write(
            repo.join(".tfrobot-plugin/marketplace.json"),
            r#"{"plugins":[{"name":"audit","source":"./plugins/audit"},{"name":"lint","source":"./plugins/lint"}]}"#,
        )
        .unwrap();
        for p in ["audit", "lint"] {
            let skill = repo.join(format!("plugins/{p}/skills/{p}-skill"));
            fs::create_dir_all(&skill).unwrap();
            fs::write(
                skill.join("SKILL.md"),
                format!("---\nname: {p}-skill\ndescription: d\n---\nbody"),
            )
            .unwrap();
        }
        git(&["init", "-q"], &repo);
        git(&["add", "-A"], &repo);
        git(&["commit", "-qm", "init"], &repo);
        let source = json!({"type":"git","url":format!("file://{}",repo.display())});
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
        seed_install_record(&home, "audit@acme", None);
        seed_install_record(&home, "lint@acme", None);

        let mut fresh = SkillRegistry::new();
        let report = recover_marketplace_skills(
            &mut fresh,
            &home,
            None,
            &declared_enabled(&["audit@acme", "lint@acme"]),
        )
        .await;
        assert_eq!(
            report.restored_plugins,
            vec!["audit@acme".to_string(), "lint@acme".to_string()]
        );
        assert!(fresh.resolve("audit:audit-skill").is_some());
        assert!(fresh.resolve("lint:lint-skill").is_some());
    }

    // ---- 🟡9：v0.3.0 翻转——enable 旗不在 declared（absent）→ 不激活（installed_disabled 惰性）----
    #[tokio::test]
    async fn recover_skips_when_enable_flag_absent_from_declared() {
        // v0.3.0（协议 §2.4）：absent enabledPlugins = 未启用。仅 install 未 enable、或 enable 旗写在**未并入
        // declared** 的 scope（如非进程-cwd 的 project/local）→ plugin 处于惰性 installed_disabled，boot **不**
        // 复活其 skills。跨重启可靠启用须写 user scope（与旧 v0.2.x「absent=启用默认复活」相反）。
        let tmp = TempDir::new().unwrap();
        let (home, _src) = setup_installed(&tmp).await;
        let declared = Map::new(); // 无 enabledPlugins → 未启用
        let mut fresh = SkillRegistry::new();
        let report = recover_marketplace_skills(&mut fresh, &home, None, &declared).await;
        assert!(
            report.restored_plugins.is_empty(),
            "absent enable 旗 → 不激活（installed_disabled 惰性）"
        );
        assert_eq!(report.skipped_disabled, vec!["audit@acme".to_string()]);
        assert!(fresh.resolve("audit:code-review").is_none());
    }
}
