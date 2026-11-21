/*!
* 文件名: marketplace_migration.rs
* 作者: JQQ
* 创建日期: 2026/11/22
* 最后修改日期: 2026/11/22
* 版权: 2023 JQQ. All rights reserved.
* 依赖: serde, serde_json, sha2, chrono, smcp::utils::{atomic_io,path}
* 描述: Marketplace 存量注册名迁移（#289）——把 legacy 旧名迁移到清单顶层 `name`。
*       Marketplace registered-name migration (legacy name -> manifest top-level `name`).
*/

//! Marketplace 存量注册名迁移（#289）。
//!
//! 协议依据：`a2c-smcp-protocol` `skill.md §1.2` + `runtime-contract §5.14`——注册名唯一性 /
//! 禁止静默改名属协议管辖，**存量注册不追溯改名，由 SDK 迁移方案负责**（含冲突检测、幂等与回滚）。
//! rust-sdk#272 已落地方向 A（add 以清单名注册、refresh 改名拒绝、`manifestNameBaseline`），本模块
//! 提供显式迁移命令的库层实现：默认 dry-run、冲突整体 abort 零写入、journal/备份/回滚、幂等可续。
//!
//! 迁移面（权威 / 派生分层）：
//! - **权威**：`installed_plugins_intent.json` 的 `<plugin>@<mp>`；user settings 的 `enabledPlugins`
//!   键、`trustedMarketplaces` / `blockedMarketplaces` 成员；`extraKnownMarketplaces` 仅
//!   `--update-declarations` 时改 user scope；plugin-scoped keyring secret `<plugin>@<mp>/<id>`。
//!   project/local/flag/policy **只报告不写**。
//! - **派生**：`known_marketplaces.json`（key / `installLocation` / `manifestNameBaseline`）、
//!   `installed_plugins.json`（pid 键 / `installPath` 定向前缀改写）、clone 与 `.plugins` 外部树
//!   （文件系统 rename）。
//!
//! **离线前提**：迁移不 clone、不触网、不写 SKILL registry；建议先停所有 Computer / python
//! 实例再执行（迁移锁只互斥 migrate 进程，不阻断既有写入方）。

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io;
use std::path::{Path, PathBuf};

use indexmap::IndexSet;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use smcp::utils::atomic_io::{atomic_write_bytes, atomic_write_bytes_mode};
use smcp::utils::path::normalize_lexical;

use crate::inputs::plugin_pool::prefix_input_id;
use crate::inputs::resolver::env_var_name;
use crate::inputs::secret_store::SecretStore;
use crate::mcp_clients::model::MCPServerInput;
use crate::settings::raw_write;
use crate::settings::reconciler::InstalledPluginRecord;
use crate::settings::schema::{
    FIELD_BLOCKED_MARKETPLACES, FIELD_ENABLED_PLUGINS, FIELD_EXTRA_KNOWN_MARKETPLACES,
    FIELD_TRUSTED_MARKETPLACES,
};
use crate::settings::scope::{
    user_settings_path, workdir_local_settings_path, workdir_project_settings_path, EnvMap,
};
use crate::settings::store::{
    empty_installed_plugins, empty_installed_plugins_intent, empty_known_marketplaces,
    installed_plugins_intent_path, installed_plugins_path, known_marketplaces_path,
    load_installed_plugins_intent_strict, load_installed_plugins_strict,
    load_known_marketplaces_strict, try_update_installed_plugins_intent,
    update_installed_plugins_checked, update_known_marketplaces_checked, with_file_lock,
    SettingsStoreError, DEFAULT_LOCK_BACKOFF_BASE, DEFAULT_LOCK_BACKOFF_MAX,
};
use crate::skills::home::{marketplace_skill_dir, SOURCE_MARKETPLACE};
use crate::skills::manifest::{
    iter_plugin_entries, marketplace_manifest_name, plugin_root_base, read_marketplace_manifest,
    MCP_INPUTS_FILENAME, MCP_SERVERS_SUBDIR,
};
use crate::skills::sources::{
    compare_marketplace_sources, resolve_plugin_source, MarketplaceSourceRelation,
    ResolvedPluginSource,
};
use crate::skills::staging::{external_plugin_dir, EXTERNAL_PLUGINS_NS};

/// 迁移 journal 文件名（SKILL Home 根）/ migration journal filename。
pub const MIGRATION_JOURNAL_FILENAME: &str = "marketplace-migration-journal.json";
/// 迁移专用互斥锁文件名（`with_file_lock` 会追加 `.lock` 旁车）/ migration-only mutex lock stem。
pub const MIGRATION_LOCK_FILENAME: &str = "marketplace-migration";
/// 备份目录前缀 / backup directory prefix。
pub const MIGRATION_BACKUP_PREFIX: &str = "marketplace-migration-backup-";
/// journal / backup schema 版本 / schema version。
pub const MIGRATION_SCHEMA_VERSION: u32 = 1;

// ===========================================================================
// 公开类型 / Public types
// ===========================================================================
/// 迁移选项 / migration options。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MarketplaceMigrationOptions {
    /// 是否改写 user scope `extraKnownMarketplaces`（其余 scope 永远只报告）/ update user declarations。
    pub update_declarations: bool,
}

/// 单个 marketplace 的拟议动作 / planned action for one marketplace。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MigrationAction {
    /// 改名迁移（clone/外部树 rename + 全部引用改写）/ rename。
    Rename,
    /// 注册名已等于清单名，只补写 `manifestNameBaseline` / baseline repair only。
    BaselineOnly,
    /// 已完全就绪，无需动作 / no-op。
    NoOp,
    /// 离线 / 清单不可用等 grandfather，保留旧名 / unresolved (grandfathered)。
    Unresolved,
    /// 无法自动迁移（baseline 冲突 / secret 不可用等）/ blocked。
    Blocked,
}

/// 未迁移原因 / reason an item is unresolved or blocked。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrationReason {
    /// clone 目录缺失或不是目录 / catalog missing。
    CatalogMissing,
    /// 清单缺失 / 不可读 / manifest unreadable。
    ManifestUnreadable,
    /// 清单顶层 `name` 缺失或非法 / manifest name missing or invalid。
    ManifestNameInvalid,
    /// 既有 baseline 与当前清单名不一致（协议 refresh 会拒绝）/ baseline mismatch。
    BaselineMismatch,
    /// 旧注册名不适合作为路径段（穿越 / 保留名）/ unsafe old name。
    UnsafeOldName,
    /// clone 目录是符号链接（不猜解析目标）/ symlink catalog。
    SymlinkPath,
    /// keyring 后端不可用且存在 password input / keyring unavailable。
    KeyringUnavailable,
    /// plugin `mcp-servers/inputs.json` 不可读或畸形 / secret inputs unreadable。
    SecretInputsUnreadable,
    /// 目标 secret 键已存在且值不同 / secret target collision。
    SecretTargetConflict,
    /// 已装 plugin 根不可读 / plugin root unreadable。
    PluginRootUnreadable,
    /// 账本 `installPath` 不在预期旧根下（不部分改写）/ unexpected install path。
    InstallPathUnexpected,
    /// 状态文件损坏（严格只读预检）/ corrupt state file。
    StateCorrupt,
    /// 文件系统 rename 失败 / rename failed。
    RenameFailed,
    /// 迁移期间检测到并发写入 / concurrent change。
    ConcurrentChange,
    /// 前一项失败，本项未尝试 / not attempted after an earlier failure。
    NotAttempted,
}

/// 单个 marketplace 的计划/报告条目 / one planned or reported item。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrationItem {
    /// 旧注册名 / old registered name。
    pub old_name: String,
    /// 目标清单名（不可迁移时 `None`）/ target manifest name。
    pub new_name: Option<String>,
    /// 动作 / action。
    pub action: MigrationAction,
    /// 未迁移原因 / reason。
    pub reason: Option<MigrationReason>,
    /// 脱敏 source 展示值 / redacted source display。
    pub source: String,
    /// 受影响 plugin 名（去重排序）/ affected plugin names。
    pub plugins: Vec<String>,
    /// 计划重键的 secret 数量 / planned secret rekeys。
    pub secrets: usize,
    /// 既有 baseline / existing baseline。
    pub baseline: Option<String>,
    /// 因 scoped id 变化而需用户更新的环境变量名 / env var renames (host-side, report only)。
    pub env_renames: Vec<EnvRename>,
    #[serde(skip)]
    pub(crate) source_value: Value,
    #[serde(skip)]
    pub(crate) pids: Vec<String>,
    #[serde(skip)]
    pub(crate) secret_plans: Vec<SecretPlan>,
    #[serde(skip)]
    pub(crate) install_paths: Vec<(String, String)>,
}

/// 环境变量名 old→new 映射（迁移只报告，不改宿主环境）/ env var name rename (report only)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvRename {
    /// 旧变量名 / old env var。
    pub old: String,
    /// 新变量名 / new env var。
    pub new: String,
}

#[derive(Debug, Clone)]
pub(crate) struct SecretPlan {
    pub(crate) plugin: String,
    pub(crate) input_id: String,
    pub(crate) old_key: String,
    pub(crate) new_key: String,
}

/// 迁移冲突（任一存在 → 整体 abort、零写入）/ conflict (any -> abort with zero writes)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrationConflict {
    /// 冲突类别（snake_case）/ conflict kind。
    pub kind: String,
    /// 旧名 / old name。
    pub old_name: String,
    /// 目标名 / target name。
    pub new_name: Option<String>,
    /// 脱敏描述 / redacted detail。
    pub detail: String,
}

/// 只读手动引用（project/local 等不写盘）/ manual reference (report-only)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManualReference {
    /// 文件路径 / file path。
    pub file: String,
    /// JSON pointer / key path。
    pub pointer: String,
    /// 影响（`will-be-inactive` / `stale-declaration`）/ effect。
    pub effect: String,
    /// 建议替换值 / suggested replacement。
    pub suggested: String,
}

/// 迁移计划（dry-run 输出）/ migration plan。
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketplaceMigrationPlan {
    /// marketplace 条目（按旧名排序）/ items sorted by old name。
    pub items: Vec<MigrationItem>,
    /// 冲突（非空 → abort）/ conflicts。
    pub conflicts: Vec<MigrationConflict>,
    /// 只读手动引用 / manual references。
    #[serde(rename = "manualReferences")]
    pub manual: Vec<ManualReference>,
    /// 状态文件损坏等命令级阻塞原因（非空 → exit 3、零写入）/ command-level blocked reason。
    pub corrupt: Option<String>,
}

impl MarketplaceMigrationPlan {
    /// 是否存在将发生的写入 / whether any item would write。
    #[must_use]
    pub fn has_writes(&self) -> bool {
        self.items.iter().any(|i| {
            matches!(
                i.action,
                MigrationAction::Rename | MigrationAction::BaselineOnly
            )
        })
    }
}

/// 备份引用 / backup reference。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupRef {
    /// 时间戳后缀 / timestamp suffix。
    pub timestamp: String,
    /// 备份目录绝对路径 / absolute backup dir。
    pub path: String,
}

/// 迁移报告 / migration report。
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketplaceMigrationReport {
    /// 命令名 / command name。
    pub command: String,
    /// 是否 dry-run / dry run。
    pub dry_run: bool,
    /// 本次是否实际执行了写入（冲突/损坏/无可写项时即使 `--apply` 也为 false）。
    pub written: bool,
    /// 条目 / items。
    pub items: Vec<MigrationItem>,
    /// 冲突 / conflicts。
    pub conflicts: Vec<MigrationConflict>,
    /// 只读手动引用 / manual references。
    #[serde(rename = "manualReferences")]
    pub manual: Vec<ManualReference>,
    /// 备份（无写入时 `None`）/ backup when written。
    pub backup: Option<BackupRef>,
    /// 非阻塞告警（secret 删除失败等）/ warnings。
    pub warnings: Vec<String>,
    /// 状态文件损坏等命令级阻塞原因 / command-level blocked reason。
    pub corrupt: Option<String>,
    /// 汇总状态 / overall status。
    pub status: MigrationStatus,
}

/// 整体状态（映射 CLI 退出码）/ overall status mapping to CLI exit code。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MigrationStatus {
    /// 0：成功 / 无操作。
    #[default]
    Ok,
    /// 1：冲突（exit user error）。
    Conflicts,
    /// 3：部分完成（unresolved/blocked/keyring abort/失活诊断）。
    Partial,
}

impl MarketplaceMigrationReport {
    /// 计算整体状态 / compute overall status。
    #[must_use]
    pub fn status(&self) -> MigrationStatus {
        if !self.conflicts.is_empty() {
            return MigrationStatus::Conflicts;
        }
        let partial = self.corrupt.is_some()
            || !self.warnings.is_empty()
            || self.items.iter().any(|i| {
                !matches!(
                    i.action,
                    MigrationAction::Rename | MigrationAction::BaselineOnly | MigrationAction::NoOp
                )
            })
            || self.manual.iter().any(|m| m.effect == "will-be-inactive");
        if partial {
            MigrationStatus::Partial
        } else {
            MigrationStatus::Ok
        }
    }
}

/// 回滚报告 / rollback report。
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketplaceRollbackReport {
    /// 备份时间戳 / backup timestamp。
    pub timestamp: String,
    /// 已还原 marketplace 旧名 / restored old names。
    pub restored: Vec<String>,
    /// 信息性说明（不计入部分完成）/ informational notes。
    pub notes: Vec<String>,
    /// 告警（keyring 不可用等 → exit 3）/ warnings。
    pub warnings: Vec<String>,
}

/// 命令级错误（Result::Err，CLI exit 1）/ command-level errors。
#[derive(Debug, thiserror::Error)]
pub enum MarketplaceMigrationError {
    /// 迁移锁被占用 / migration lock busy。
    #[error("marketplace migration lock is busy at {path}")]
    LockBusy {
        /// 锁文件路径 / lock path。
        path: PathBuf,
    },
    /// I/O 失败 / IO failure。
    #[error("marketplace migration I/O error on {path}: {source}")]
    Io {
        /// 出错路径 / path。
        path: PathBuf,
        /// 底层错误 / underlying error。
        #[source]
        source: io::Error,
    },
    /// 存在未完成 journal（需 `--resume`）/ incomplete journal present。
    #[error(
        "an incomplete marketplace migration journal exists at {path}; rerun with --resume \
         or roll it back with --rollback <timestamp>"
    )]
    JournalInProgress {
        /// journal 路径 / journal path。
        path: PathBuf,
    },
    /// `--resume` 但没有 journal / resume without journal。
    #[error("no marketplace migration journal to resume")]
    NoJournal,
    /// journal 损坏 / corrupt journal。
    #[error("marketplace migration journal is corrupt: {reason}")]
    JournalCorrupt {
        /// 原因 / reason。
        reason: String,
    },
    /// 找不到备份 / backup not found。
    #[error("marketplace migration backup not found for timestamp {timestamp:?}")]
    BackupNotFound {
        /// 时间戳 / timestamp。
        timestamp: String,
    },
    /// 备份清单损坏 / invalid backup manifest。
    #[error("marketplace migration backup manifest is invalid: {reason}")]
    BackupManifestInvalid {
        /// 原因 / reason。
        reason: String,
    },
    /// 时间戳非法 / invalid timestamp。
    #[error("invalid marketplace migration timestamp {0:?}")]
    InvalidTimestamp(String),
    /// 备份属于另一个 home / backup belongs to another home。
    #[error("backup home {backup_home} does not match current home {current_home}")]
    HomeMismatch {
        /// 备份记录的 home / backup home。
        backup_home: PathBuf,
        /// 当前 home / current home。
        current_home: PathBuf,
    },
    /// 回滚前检测到状态漂移 / drift detected before rollback。
    #[error("refusing rollback: {detail}")]
    RollbackDrift {
        /// 描述 / detail。
        detail: String,
    },
    /// 执行期间检测到并发写入 / concurrent write detected。
    #[error("state changed during migration: {detail}")]
    ConcurrentChange {
        /// 描述 / detail。
        detail: String,
    },
    /// 状态文件损坏（apply/resume 命令级）/ corrupt state file。
    #[error("state file corrupt at {path}: {reason}")]
    StateCorrupt {
        /// 文件 / path。
        path: PathBuf,
        /// 原因 / reason。
        reason: String,
    },
}

impl From<SettingsStoreError> for MarketplaceMigrationError {
    fn from(e: SettingsStoreError) -> Self {
        match e {
            SettingsStoreError::Lock { path, .. } => Self::LockBusy { path },
            SettingsStoreError::Io { path, source } => {
                if source.kind() == io::ErrorKind::InvalidData {
                    Self::StateCorrupt {
                        path,
                        reason: source.to_string(),
                    }
                } else {
                    Self::Io { path, source }
                }
            }
        }
    }
}

// ===========================================================================
// 只读检测 / Read-only planning
// ===========================================================================
/// 生成迁移计划（dry-run；零写入）/ Build a migration plan without writing anything.
///
/// # Errors
/// 命令级 I/O / 锁 / 状态错误。损坏状态与不可迁移项通过 `plan.corrupt` / item action 表达。
pub fn plan_marketplace_name_migration(
    home: &Path,
    env: Option<&EnvMap>,
    cwd: Option<&Path>,
    options: MarketplaceMigrationOptions,
    secrets: &SecretStore,
) -> Result<MarketplaceMigrationPlan, MarketplaceMigrationError> {
    let home = normalize_lexical(home);
    if !home.is_dir() {
        return Err(MarketplaceMigrationError::Io {
            path: home.clone(),
            source: io::Error::new(io::ErrorKind::NotFound, "skill home is not a directory"),
        });
    }
    let ctx = load_context(&home, env, cwd, options, secrets)?;
    Ok(ctx.plan)
}

struct PlanContext {
    home: PathBuf,
    plan: MarketplaceMigrationPlan,
    user_path: PathBuf,
    update_declarations: bool,
    /// `installed_plugins_intent.json` 在本轮开始时缺失（pre-v0.3.0 home）。
    intent_missing: bool,
    /// intent 缺失时用于 seed 的账本/启用 pid 全集（保序去重）。
    seed_intent_pids: Vec<String>,
    /// 预检完成时的权威文件哈希（apply 前复核，检测预检后的并发写）。
    pre_hashes: BTreeMap<String, String>,
    /// intent 中的 local-only id（no-`@`），写回 intent 时保留（tolerant coerce 会丢弃）。
    preserved_intent_ids: Vec<String>,
}

fn load_context(
    home: &Path,
    env: Option<&EnvMap>,
    cwd: Option<&Path>,
    options: MarketplaceMigrationOptions,
    secrets: &SecretStore,
) -> Result<PlanContext, MarketplaceMigrationError> {
    let user_path = user_settings_path(env);
    // 严格只读：损坏 → 命令级 blocked，不触发 load_* 的 .corrupt 改名备份。
    let mut corrupt: Option<String> = None;
    let known = match load_known_marketplaces_strict(Some(home), env) {
        Ok(v) => v,
        Err(e) => {
            corrupt = Some(format!("known_marketplaces.json: {e}"));
            empty_known_marketplaces()
        }
    };
    let ledger = match load_installed_plugins_strict(Some(home), env) {
        Ok(v) => v,
        Err(e) => {
            corrupt.get_or_insert_with(|| format!("installed_plugins.json: {e}"));
            empty_installed_plugins()
        }
    };
    let intent = match load_installed_plugins_intent_strict(Some(home), env) {
        Ok(v) => v,
        Err(e) => {
            corrupt.get_or_insert_with(|| format!("installed_plugins_intent.json: {e}"));
            empty_installed_plugins_intent()
        }
    };
    let user_map = match raw_write::read_strict_map(&user_path) {
        Ok(v) => v,
        Err(e) => {
            corrupt.get_or_insert_with(|| format!("{}: {e}", user_path.display()));
            Map::new()
        }
    };
    // 迁移会触碰的 user 容器必须是预期类型；否则 fail-closed（不部分写）。
    for (field, want_object) in [
        (FIELD_ENABLED_PLUGINS, true),
        (FIELD_TRUSTED_MARKETPLACES, false),
        (FIELD_BLOCKED_MARKETPLACES, false),
        (FIELD_EXTRA_KNOWN_MARKETPLACES, true),
    ] {
        if let Some(value) = user_map.get(field) {
            let ok = if want_object {
                value.is_object()
            } else {
                value.is_array()
            };
            if !ok {
                corrupt.get_or_insert_with(|| {
                    format!(
                        "{}: field {field:?} has an unexpected type",
                        user_path.display()
                    )
                });
            }
        }
    }
    if let Some(reason) = corrupt {
        return Ok(PlanContext {
            home: home.to_path_buf(),
            plan: MarketplaceMigrationPlan {
                corrupt: Some(reason),
                ..Default::default()
            },
            user_path,
            update_declarations: options.update_declarations,
            intent_missing: false,
            seed_intent_pids: Vec::new(),
            pre_hashes: BTreeMap::new(),
            preserved_intent_ids: Vec::new(),
        });
    }

    let mut plan = MarketplaceMigrationPlan::default();
    let old_names: Vec<String> = known.account.marketplaces.keys().cloned().collect();
    let mut sorted = old_names;
    sorted.sort();

    // 已装集合（intent ∪ ledger pid keys ∪ user enabledPlugins keys）——裸名冲突与目标 pid 判定共用。
    let installed_pids = installed_pid_set(&intent, &ledger, &user_map);

    for old in sorted {
        let entry = match known.account.marketplaces.get(&old) {
            Some(e) => e.clone(),
            None => continue,
        };
        let mut item = MigrationItem {
            old_name: old.clone(),
            new_name: None,
            action: MigrationAction::Unresolved,
            reason: None,
            source: display_source(&entry.source),
            plugins: Vec::new(),
            secrets: 0,
            baseline: entry.manifest_name_baseline.clone(),
            env_renames: Vec::new(),
            source_value: entry.source.clone(),
            pids: Vec::new(),
            secret_plans: Vec::new(),
            install_paths: Vec::new(),
        };

        if !is_safe_name_segment(&old) || !crate::settings::schema::is_valid_marketplace_name(&old)
        {
            // 非严格 kebab 旧名无法在 intent/known 键空间中安全迁移（coerce 会丢弃非法 pid）。
            item.reason = Some(MigrationReason::UnsafeOldName);
            plan.items.push(item);
            continue;
        }

        let catalog = marketplace_skill_dir(home, &old, &[]);
        match std::fs::symlink_metadata(&catalog) {
            Err(_) => {
                item.reason = Some(MigrationReason::CatalogMissing);
                plan.items.push(item);
                continue;
            }
            Ok(meta) if meta.file_type().is_symlink() => {
                item.reason = Some(MigrationReason::SymlinkPath);
                plan.items.push(item);
                continue;
            }
            Ok(meta) if !meta.is_dir() => {
                item.reason = Some(MigrationReason::CatalogMissing);
                plan.items.push(item);
                continue;
            }
            Ok(_) => {}
        }

        let manifest = match read_marketplace_manifest(&catalog) {
            Ok(m) => m,
            Err(_) => {
                item.reason = Some(MigrationReason::ManifestUnreadable);
                plan.items.push(item);
                continue;
            }
        };
        let manifest_name = match marketplace_manifest_name(&manifest) {
            Ok(n) => n,
            Err(_) => {
                item.reason = Some(MigrationReason::ManifestNameInvalid);
                plan.items.push(item);
                continue;
            }
        };

        let baseline = entry.manifest_name_baseline.clone();
        if manifest_name == old {
            match baseline {
                None => {
                    item.action = MigrationAction::BaselineOnly;
                }
                Some(b) if b == old => {
                    item.action = MigrationAction::NoOp;
                }
                Some(_) => {
                    item.action = MigrationAction::Blocked;
                    item.reason = Some(MigrationReason::BaselineMismatch);
                }
            }
            // 收集引用（NoOp 也报告，便于用户理解影响面）。
            collect_pids_and_paths(home, &mut item, &intent, &ledger, &user_map, &old);
            plan.items.push(item);
            continue;
        }
        if let Some(b) = baseline.as_deref() {
            if b != manifest_name {
                item.action = MigrationAction::Blocked;
                item.reason = Some(MigrationReason::BaselineMismatch);
                plan.items.push(item);
                continue;
            }
        }

        item.action = MigrationAction::Rename;
        item.new_name = Some(manifest_name.clone());
        collect_pids_and_paths(home, &mut item, &intent, &ledger, &user_map, &old);
        // 账本 installPath 预检：任一非空路径不在预期旧根下 → 整体不可迁移（不部分改写）。
        if item
            .install_paths
            .iter()
            .any(|(_, p)| rewrite_install_path(p, home, &old, &manifest_name).is_err())
        {
            item.action = MigrationAction::Unresolved;
            item.reason = Some(MigrationReason::InstallPathUnexpected);
            plan.items.push(item);
            continue;
        }
        // secret / env 预检。
        match plan_secrets(
            home,
            &manifest,
            &catalog,
            &old,
            &manifest_name,
            &item.pids,
            secrets,
        ) {
            Ok((secrets_plan, env_renames, secret_count)) => {
                item.secret_plans = secrets_plan;
                item.secrets = secret_count;
                item.env_renames = env_renames;
            }
            Err(reason) => {
                item.action = if matches!(
                    reason,
                    MigrationReason::KeyringUnavailable
                        | MigrationReason::SecretTargetConflict
                        | MigrationReason::SecretInputsUnreadable
                        | MigrationReason::PluginRootUnreadable
                ) {
                    MigrationAction::Blocked
                } else {
                    MigrationAction::Unresolved
                };
                item.reason = Some(reason);
            }
        }
        plan.items.push(item);
    }

    // 只读手动引用（project/local + user 未改写的 extraKnown）。
    plan.manual = collect_manual_references(
        env,
        cwd,
        &plan.items,
        &ledger,
        &user_map,
        options.update_declarations,
    );

    // 冲突预检（任一 → 整体 abort）。
    plan.conflicts = detect_conflicts(
        home,
        &plan.items,
        &known,
        &ledger,
        &intent,
        &user_map,
        &installed_pids,
        options,
    );
    let intent_missing = !installed_plugins_intent_path(Some(home), env).exists();
    let mut seed_intent_pids: Vec<String> = ledger.account.plugins.keys().cloned().collect();
    seed_intent_pids.sort();
    seed_intent_pids.dedup();
    let pre_hashes = current_hashes(home, env, &user_path)?;
    // local-only id（no-`@`）在 intent 与 ledger 中都可能合法；写回 intent 时全部保留。
    let mut preserved_intent_ids: Vec<String> = intent
        .account
        .installed_plugins
        .iter()
        .filter(|pid| !pid.contains('@'))
        .cloned()
        .collect();
    if intent_missing {
        // pre-v0.3.0：账本是唯一来源，local-only id 也要随 seed 进 intent 并保留。
        preserved_intent_ids.extend(
            ledger
                .account
                .plugins
                .keys()
                .filter(|pid| !pid.contains('@'))
                .cloned(),
        );
    }
    preserved_intent_ids.sort();
    preserved_intent_ids.dedup();
    Ok(PlanContext {
        home: home.to_path_buf(),
        plan,
        user_path,
        update_declarations: options.update_declarations,
        intent_missing,
        seed_intent_pids,
        pre_hashes,
        preserved_intent_ids,
    })
}

fn installed_pid_set(
    intent: &crate::settings::store::InstalledPluginsIntentFile,
    ledger: &crate::settings::store::InstalledPluginsFile,
    user_map: &Map<String, Value>,
) -> HashSet<String> {
    let mut set: HashSet<String> = intent.account.installed_plugins.iter().cloned().collect();
    set.extend(ledger.account.plugins.keys().cloned());
    if let Some(Value::Object(enabled)) = user_map.get(FIELD_ENABLED_PLUGINS) {
        set.extend(enabled.keys().cloned());
    }
    set
}

fn collect_pids_and_paths(
    _home: &Path,
    item: &mut MigrationItem,
    intent: &crate::settings::store::InstalledPluginsIntentFile,
    ledger: &crate::settings::store::InstalledPluginsFile,
    user_map: &Map<String, Value>,
    old: &str,
) {
    let suffix = format!("@{old}");
    let mut pids: BTreeSet<String> = BTreeSet::new();
    for pid in &intent.account.installed_plugins {
        if pid.ends_with(&suffix) {
            pids.insert(pid.clone());
        }
    }
    for pid in ledger.account.plugins.keys() {
        if pid.ends_with(&suffix) {
            pids.insert(pid.clone());
        }
    }
    if let Some(Value::Object(enabled)) = user_map.get(FIELD_ENABLED_PLUGINS) {
        for pid in enabled.keys() {
            if pid.ends_with(&suffix) {
                pids.insert(pid.clone());
            }
        }
    }
    let mut plugins: BTreeSet<String> = BTreeSet::new();
    for pid in &pids {
        if let Some(plugin) = pid.strip_suffix(&suffix) {
            if !plugin.is_empty() {
                plugins.insert(plugin.to_string());
            }
        }
        if let Some(records) = ledger.account.plugins.get(pid) {
            for rec in records {
                if let Some(p) = rec.install_path.as_deref().filter(|p| !p.is_empty()) {
                    item.install_paths.push((pid.clone(), p.to_string()));
                }
            }
        }
    }
    item.pids = pids.into_iter().collect();
    item.plugins = plugins.into_iter().collect();
}

fn plan_secrets(
    home: &Path,
    manifest: &Map<String, Value>,
    catalog: &Path,
    old: &str,
    new: &str,
    installed_pids: &[String],
    secrets: &SecretStore,
) -> Result<(Vec<SecretPlan>, Vec<EnvRename>, usize), MigrationReason> {
    let installed_plugins: HashSet<&str> = installed_pids
        .iter()
        .filter_map(|p| p.strip_suffix(&format!("@{old}")))
        .collect();
    let mut plans: Vec<SecretPlan> = Vec::new();
    let mut envs: Vec<EnvRename> = Vec::new();

    for entry in iter_plugin_entries(manifest) {
        let Some(plugin) = entry.get("name").and_then(Value::as_str) else {
            continue;
        };
        if plugin.is_empty() {
            continue;
        }
        let Some(raw_source) = entry.get("source") else {
            if installed_plugins.contains(plugin) {
                return Err(MigrationReason::PluginRootUnreadable);
            }
            continue;
        };
        let root = match resolve_plugin_source(raw_source, &plugin_root_base(manifest)) {
            Ok(ResolvedPluginSource::Local(local)) => {
                let root = normalize_lexical(&catalog.join(&local.rel_path));
                if !root.starts_with(catalog) || !root.is_dir() {
                    if installed_plugins.contains(plugin) {
                        return Err(MigrationReason::PluginRootUnreadable);
                    }
                    continue;
                }
                root
            }
            Ok(ResolvedPluginSource::Git(spec)) => {
                let root = match &spec.subdir {
                    Some(subdir) => external_plugin_dir(home, old, plugin).join(subdir),
                    None => external_plugin_dir(home, old, plugin),
                };
                if !root.is_dir()
                    || std::fs::symlink_metadata(&root)
                        .map(|m| m.file_type().is_symlink())
                        .unwrap_or(false)
                {
                    if installed_plugins.contains(plugin) {
                        return Err(MigrationReason::PluginRootUnreadable);
                    }
                    continue;
                }
                root
            }
            Err(_) => {
                if installed_plugins.contains(plugin) {
                    return Err(MigrationReason::PluginRootUnreadable);
                }
                continue;
            }
        };
        let inputs_path = root.join(MCP_SERVERS_SUBDIR).join(MCP_INPUTS_FILENAME);
        let Some(defs) = read_input_defs_strict(&inputs_path)? else {
            continue;
        };
        for def in defs {
            let input: MCPServerInput =
                serde_json::from_value(def).map_err(|_| MigrationReason::SecretInputsUnreadable)?;
            let input_id = input.id().to_string();
            let old_key = prefix_input_id(plugin, old, &input_id);
            let new_key = prefix_input_id(plugin, new, &input_id);
            envs.push(EnvRename {
                old: env_var_name(&old_key),
                new: env_var_name(&new_key),
            });
            let is_password =
                matches!(&input, MCPServerInput::PromptString(p) if p.password.unwrap_or(false));
            if is_password {
                plans.push(SecretPlan {
                    plugin: plugin.to_string(),
                    input_id,
                    old_key,
                    new_key,
                });
            }
        }
    }

    if plans.is_empty() {
        envs.sort_by(|a, b| a.old.cmp(&b.old));
        envs.dedup_by(|a, b| a.old == b.old && a.new == b.new);
        return Ok((Vec::new(), envs, 0));
    }
    if !secrets.available() {
        // keyring 不可用：无法证明旧 secret 不存在 → 零写入 abort 该 marketplace。
        return Err(MigrationReason::KeyringUnavailable);
    }
    // 目标键冲突：已存在且不同于旧值 → 预检 abort（不覆盖）。
    for plan in &plans {
        let old_val = secrets.get(&plan.old_key);
        let new_val = secrets.get(&plan.new_key);
        if let Some(new_val) = new_val {
            if old_val.as_deref() != Some(new_val.as_str()) {
                return Err(MigrationReason::SecretTargetConflict);
            }
        }
    }
    // 只为确有旧值的条目计划 set（无旧值 = 无迁移面），计数与计划保持一致。
    plans.retain(|p| secrets.get(&p.old_key).is_some());
    let count = plans.len();
    envs.sort_by(|a, b| a.old.cmp(&b.old));
    envs.dedup_by(|a, b| a.old == b.old && a.new == b.new);
    Ok((plans, envs, count))
}

/// 严格读 `mcp-servers/inputs.json`：缺失 → `None`；畸形 → Err（不静默跳过）。
fn read_input_defs_strict(path: &Path) -> Result<Option<Vec<Value>>, MigrationReason> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(MigrationReason::SecretInputsUnreadable),
    };
    let parsed: Value =
        serde_json::from_str(&text).map_err(|_| MigrationReason::SecretInputsUnreadable)?;
    match parsed {
        Value::Array(items) => Ok(Some(items)),
        Value::Object(mut map) => match map.remove("inputs") {
            Some(Value::Array(items)) => Ok(Some(items)),
            Some(Value::Null) | None => Ok(Some(Vec::new())),
            Some(_) => Err(MigrationReason::SecretInputsUnreadable),
        },
        _ => Err(MigrationReason::SecretInputsUnreadable),
    }
}

// ===========================================================================
// 冲突检测 / Conflict detection
// ===========================================================================
#[allow(clippy::too_many_arguments)]
fn detect_conflicts(
    home: &Path,
    items: &[MigrationItem],
    known: &crate::settings::store::KnownMarketplacesFile,
    ledger: &crate::settings::store::InstalledPluginsFile,
    intent: &crate::settings::store::InstalledPluginsIntentFile,
    user_map: &Map<String, Value>,
    installed_pids: &HashSet<String>,
    options: MarketplaceMigrationOptions,
) -> Vec<MigrationConflict> {
    let mut conflicts: Vec<MigrationConflict> = Vec::new();
    let candidates: Vec<&MigrationItem> = items
        .iter()
        .filter(|i| i.action == MigrationAction::Rename)
        .collect();

    // 两个旧名映射同一新名。
    let mut by_target: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    for item in &candidates {
        if let Some(new) = item.new_name.as_deref() {
            by_target
                .entry(new.to_string())
                .or_default()
                .push(item.old_name.as_str());
        }
    }
    for (new, olds) in &by_target {
        if olds.len() > 1 {
            conflicts.push(MigrationConflict {
                kind: "duplicate_target".to_string(),
                old_name: olds.join(","),
                new_name: Some(new.clone()),
                detail: format!("multiple legacy names map to the same manifest name {new:?}"),
            });
        }
    }

    for item in &candidates {
        let Some(new) = item.new_name.as_deref() else {
            continue;
        };
        let old = item.old_name.as_str();
        // 旧名必须是 strict kebab（plan 已拦截），manifest 名也是 strict kebab；两者的
        // 大小写差异不可达，故不单列 case-only 冲突。
        // 目标名已注册。
        if let Some(target) = known.account.marketplaces.get(new) {
            let same_source = compare_marketplace_sources(&item.source_value, &target.source)
                == MarketplaceSourceRelation::Same;
            if !same_source {
                conflicts.push(MigrationConflict {
                    kind: "target_name_conflict".to_string(),
                    old_name: old.to_string(),
                    new_name: Some(new.to_string()),
                    detail: format!(
                        "target name {new:?} is already registered from a different source"
                    ),
                });
            } else if target
                .manifest_name_baseline
                .as_deref()
                .is_some_and(|b| b != new)
            {
                conflicts.push(MigrationConflict {
                    kind: "target_baseline_conflict".to_string(),
                    old_name: old.to_string(),
                    new_name: Some(new.to_string()),
                    detail: format!("target name {new:?} has an inconsistent manifestNameBaseline"),
                });
            }
        }
        // 目标目录 / 外部树已存在。
        let target_dir = marketplace_skill_dir(home, new, &[]);
        if old != new && target_dir.exists() {
            conflicts.push(MigrationConflict {
                kind: "target_dir_exists".to_string(),
                old_name: old.to_string(),
                new_name: Some(new.to_string()),
                detail: format!("target catalog directory already exists for {new:?}"),
            });
        }
        let target_ext = home
            .join(SOURCE_MARKETPLACE)
            .join(EXTERNAL_PLUGINS_NS)
            .join(new);
        if old != new && target_ext.exists() {
            conflicts.push(MigrationConflict {
                kind: "target_external_tree_exists".to_string(),
                old_name: old.to_string(),
                new_name: Some(new.to_string()),
                detail: format!("target external plugin tree already exists for {new:?}"),
            });
        }
        // 目标 pid 已存在（排除本次迁移的源 pid）。
        let suffix = format!("@{new}");
        let mut target_pids: Vec<String> = Vec::new();
        target_pids.extend(intent.account.installed_plugins.iter().cloned());
        target_pids.extend(ledger.account.plugins.keys().cloned());
        if let Some(Value::Object(enabled)) = user_map.get(FIELD_ENABLED_PLUGINS) {
            target_pids.extend(enabled.keys().cloned());
        }
        for pid in target_pids {
            if pid.ends_with(&suffix) && !item.pids.contains(&pid) {
                conflicts.push(MigrationConflict {
                    kind: "target_pid_exists".to_string(),
                    old_name: old.to_string(),
                    new_name: Some(new.to_string()),
                    detail: format!("target plugin id {pid:?} already exists"),
                });
            }
        }
        // 裸名跨 marketplace 冲突：该 plugin 已被别的 marketplace 的已装 pid 占用。
        for plugin in &item.plugins {
            let other_prefix = format!("{plugin}@");
            for pid in installed_pids {
                if pid.starts_with(&other_prefix)
                    && !pid.ends_with(&format!("@{old}"))
                    && !item.pids.contains(pid)
                {
                    // 同一 plugin 名在本次迁移源集合内是预期变化，不算冲突。
                    conflicts.push(MigrationConflict {
                        kind: "bare_name_clash".to_string(),
                        old_name: old.to_string(),
                        new_name: Some(new.to_string()),
                        detail: format!(
                            "plugin bare name {plugin:?} is already installed from another marketplace"
                        ),
                    });
                }
            }
        }
        // extraKnownMarketplaces 目标键（仅 --update-declarations 时会写）。
        if options.update_declarations {
            if let Some(Value::Object(extra)) = user_map.get(FIELD_EXTRA_KNOWN_MARKETPLACES) {
                // 旧声明若存在，其 source 必须与注册 source 可证同源，否则移动会让声明指向错误来源。
                if let Some(old_decl) = extra.get(old) {
                    let decl_source = old_decl.get("source").cloned().unwrap_or(Value::Null);
                    if compare_marketplace_sources(&item.source_value, &decl_source)
                        != MarketplaceSourceRelation::Same
                    {
                        conflicts.push(MigrationConflict {
                            kind: "extra_known_old_conflict".to_string(),
                            old_name: old.to_string(),
                            new_name: Some(new.to_string()),
                            detail: format!(
                                "extraKnownMarketplaces[{old:?}] does not match the registered source"
                            ),
                        });
                    }
                }
                if let Some(target_decl) = extra.get(new) {
                    let decl_source = target_decl.get("source").cloned().unwrap_or(Value::Null);
                    if compare_marketplace_sources(&item.source_value, &decl_source)
                        != MarketplaceSourceRelation::Same
                    {
                        conflicts.push(MigrationConflict {
                            kind: "extra_known_target_conflict".to_string(),
                            old_name: old.to_string(),
                            new_name: Some(new.to_string()),
                            detail: format!(
                                "extraKnownMarketplaces[{new:?}] already exists with a different source"
                            ),
                        });
                    }
                }
            }
        }
    }
    conflicts
}

// ===========================================================================
// 手动引用（只读）/ Manual references (read-only)
// ===========================================================================
fn collect_manual_references(
    env: Option<&EnvMap>,
    cwd: Option<&Path>,
    items: &[MigrationItem],
    ledger: &crate::settings::store::InstalledPluginsFile,
    user_map: &Map<String, Value>,
    update_declarations: bool,
) -> Vec<ManualReference> {
    let renames: BTreeMap<&str, &str> = items
        .iter()
        .filter_map(|i| match (i.action, i.new_name.as_deref()) {
            (MigrationAction::Rename, Some(new)) => Some((i.old_name.as_str(), new)),
            _ => None,
        })
        .collect();
    if renames.is_empty() {
        return Vec::new();
    }
    let mut refs: Vec<ManualReference> = Vec::new();
    // user scope：enabled/trusted/blocked 会被改写；extraKnown 默认只报告。
    if !update_declarations {
        // user_scope=true：只报 user `extraKnownMarketplaces`（enabled/trusted/blocked 会被自动改写）。
        scan_settings_map(
            &user_settings_path(env),
            user_map,
            &renames,
            true,
            &mut refs,
        );
    }
    // project/local：cwd + 账本记录 projectPath（去重）。
    let mut dirs: BTreeSet<PathBuf> = BTreeSet::new();
    if let Some(cwd) = cwd {
        dirs.insert(cwd.to_path_buf());
    }
    for recs in ledger.account.plugins.values() {
        for rec in recs {
            if let Some(pp) = rec.extra.get("projectPath").and_then(Value::as_str) {
                if !pp.is_empty() {
                    dirs.insert(PathBuf::from(pp));
                }
            }
        }
    }
    for dir in dirs {
        for path in [
            workdir_project_settings_path(&dir),
            workdir_local_settings_path(&dir),
        ] {
            if let Ok(Some(map)) = crate::settings::read_roots::read_raw_map(&path) {
                scan_settings_map(&path, &map, &renames, false, &mut refs);
            }
        }
    }
    refs.sort_by(|a, b| (&a.file, &a.pointer).cmp(&(&b.file, &b.pointer)));
    refs.dedup_by(|a, b| a.file == b.file && a.pointer == b.pointer);
    refs
}

fn scan_settings_map(
    path: &Path,
    map: &Map<String, Value>,
    renames: &BTreeMap<&str, &str>,
    user_scope: bool,
    out: &mut Vec<ManualReference>,
) {
    let file = path.display().to_string();
    if user_scope {
        // user 的 enabled/trusted/blocked 会被迁移改写；只有 extraKnown 在缺省 flag 下需手动处理。
        if let Some(Value::Object(extra)) = map.get(FIELD_EXTRA_KNOWN_MARKETPLACES) {
            for old in renames.keys() {
                if extra.contains_key(*old) {
                    out.push(ManualReference {
                        file: file.clone(),
                        pointer: format!("/{FIELD_EXTRA_KNOWN_MARKETPLACES}/{old}"),
                        effect: "stale-declaration".to_string(),
                        suggested: (*renames.get(old).copied().unwrap_or(old)).to_string(),
                    });
                }
            }
        }
        return;
    }
    if let Some(Value::Object(enabled)) = map.get(FIELD_ENABLED_PLUGINS) {
        for (old, new) in renames {
            let from = format!("@{old}");
            let to = format!("@{new}");
            for key in enabled.keys() {
                if key.ends_with(&from) {
                    let plugin = key.trim_end_matches(&from);
                    out.push(ManualReference {
                        file: file.clone(),
                        pointer: format!("/enabledPlugins/{key}"),
                        effect: "will-be-inactive".to_string(),
                        suggested: format!("{plugin}{to}"),
                    });
                }
            }
        }
    }
    for field in [FIELD_TRUSTED_MARKETPLACES, FIELD_BLOCKED_MARKETPLACES] {
        if let Some(Value::Array(arr)) = map.get(field) {
            for (idx, value) in arr.iter().enumerate() {
                if let Some(name) = value.as_str() {
                    if let Some(new) = renames.get(name) {
                        out.push(ManualReference {
                            file: file.clone(),
                            pointer: format!("/{field}/{idx}"),
                            effect: "stale-declaration".to_string(),
                            suggested: (*new).to_string(),
                        });
                    }
                }
            }
        }
    }
    if let Some(Value::Object(extra)) = map.get(FIELD_EXTRA_KNOWN_MARKETPLACES) {
        for old in renames.keys() {
            if extra.contains_key(*old) {
                out.push(ManualReference {
                    file: file.clone(),
                    pointer: format!("/{FIELD_EXTRA_KNOWN_MARKETPLACES}/{old}"),
                    effect: "stale-declaration".to_string(),
                    suggested: (*renames.get(old).copied().unwrap_or(old)).to_string(),
                });
            }
        }
    }
}

// ===========================================================================
// 应用 / Apply
// ===========================================================================
/// 应用迁移（默认 CLI 只在 `--apply` 时调用）。冲突 → 零写入返回报告；行动项写入 journal/备份。
///
/// # Errors
/// 命令级锁 / I/O / journal / 状态错误。
pub fn apply_marketplace_name_migration(
    home: &Path,
    env: Option<&EnvMap>,
    cwd: Option<&Path>,
    options: MarketplaceMigrationOptions,
    secrets: &SecretStore,
) -> Result<MarketplaceMigrationReport, MarketplaceMigrationError> {
    let home = normalize_lexical(home);
    with_migration_lock(&home, || {
        let journal_path = home.join(MIGRATION_JOURNAL_FILENAME);
        if journal_path.exists() {
            return Err(MarketplaceMigrationError::JournalInProgress { path: journal_path });
        }
        let ctx = load_context(&home, env, cwd, options, secrets)?;
        if ctx.plan.corrupt.is_some() || !ctx.plan.conflicts.is_empty() || !ctx.plan.has_writes() {
            return Ok(report_from_plan(&ctx.plan, false, None));
        }
        apply_fresh(&ctx, env, secrets, None)
    })
}

/// 续做未完成 journal。`--resume` 专用。
pub fn resume_marketplace_name_migration(
    home: &Path,
    env: Option<&EnvMap>,
    cwd: Option<&Path>,
    secrets: &SecretStore,
) -> Result<MarketplaceMigrationReport, MarketplaceMigrationError> {
    let home = normalize_lexical(home);
    with_migration_lock(&home, || {
        let journal_path = home.join(MIGRATION_JOURNAL_FILENAME);
        let meta = read_journal(&journal_path)?;
        if Path::new(&meta.home) != home.as_path() {
            return Err(MarketplaceMigrationError::HomeMismatch {
                backup_home: PathBuf::from(&meta.home),
                current_home: home.clone(),
            });
        }
        if meta.version != MIGRATION_SCHEMA_VERSION || meta.kind != "marketplace-name-migration" {
            return Err(MarketplaceMigrationError::JournalCorrupt {
                reason: "unsupported journal schema".to_string(),
            });
        }
        if meta.phase == "done" {
            let _ = std::fs::remove_file(&journal_path);
            return Ok(MarketplaceMigrationReport {
                command: "marketplace migrate --resume".to_string(),
                dry_run: false,
                ..Default::default()
            });
        }
        let backup_dir = PathBuf::from(&meta.backup_dir);
        if !backup_dir.is_dir() {
            return Err(MarketplaceMigrationError::BackupNotFound {
                timestamp: meta.timestamp.clone(),
            });
        }
        resume_from_journal(&home, env, cwd, secrets, meta, &backup_dir)
    })
}

/// 回滚一次已完成的迁移（或 rebase 前的 aborted 状态）。
pub fn rollback_marketplace_name_migration(
    home: &Path,
    env: Option<&EnvMap>,
    timestamp: &str,
    secrets: &SecretStore,
) -> Result<MarketplaceRollbackReport, MarketplaceMigrationError> {
    let home = normalize_lexical(home);
    if !is_safe_timestamp(timestamp) {
        return Err(MarketplaceMigrationError::InvalidTimestamp(
            timestamp.to_string(),
        ));
    }
    with_migration_lock(&home, || {
        let backup_dir = home.join(format!("{MIGRATION_BACKUP_PREFIX}{timestamp}"));
        let manifest_path = backup_dir.join("backup.json");
        if !manifest_path.is_file() {
            return Err(MarketplaceMigrationError::BackupNotFound {
                timestamp: timestamp.to_string(),
            });
        }
        let meta = read_meta_file(&manifest_path).map_err(|e| {
            MarketplaceMigrationError::BackupManifestInvalid {
                reason: e.to_string(),
            }
        })?;
        if Path::new(&meta.home) != home {
            return Err(MarketplaceMigrationError::HomeMismatch {
                backup_home: PathBuf::from(&meta.home),
                current_home: home.clone(),
            });
        }
        if meta.version != MIGRATION_SCHEMA_VERSION || meta.kind != "marketplace-name-migration" {
            return Err(MarketplaceMigrationError::BackupManifestInvalid {
                reason: "unsupported backup schema".to_string(),
            });
        }
        let journal_path = home.join(MIGRATION_JOURNAL_FILENAME);
        // prepared/applying 也可回滚：恢复预迁移备份并反向 rename（中断态可能只有部分文件被写）。
        let journal = if journal_path.exists() {
            let journal = read_journal(&journal_path)?;
            if Path::new(&journal.home) != home {
                return Err(MarketplaceMigrationError::HomeMismatch {
                    backup_home: PathBuf::from(&journal.home),
                    current_home: home.clone(),
                });
            }
            if journal.version != MIGRATION_SCHEMA_VERSION
                || journal.kind != "marketplace-name-migration"
            {
                return Err(MarketplaceMigrationError::JournalCorrupt {
                    reason: "unsupported journal schema".to_string(),
                });
            }
            Some(journal)
        } else {
            None
        };
        let in_progress = journal
            .as_ref()
            .is_some_and(|j| matches!(j.phase.as_str(), "prepared" | "applying"));
        // 中断 journal 必须属于本次请求的 backup；否则可能拿旧备份覆盖在途迁移。
        if in_progress && journal.as_ref().is_some_and(|j| j.timestamp != timestamp) {
            return Err(MarketplaceMigrationError::JournalInProgress { path: journal_path });
        }
        rollback_from_meta(
            &home,
            env,
            timestamp,
            &meta,
            &backup_dir,
            &journal_path,
            secrets,
            in_progress,
        )
    })
}

// ---------------------------------------------------------------------------
// 应用主流程 / Apply pipeline
// ---------------------------------------------------------------------------
fn apply_fresh(
    ctx: &PlanContext,
    env: Option<&EnvMap>,
    secrets: &SecretStore,
    fail_after: Option<&str>,
) -> Result<MarketplaceMigrationReport, MarketplaceMigrationError> {
    let home = ctx.home.as_path();
    // 预检后的并发写检测：当前权威文件必须与 plan 时哈希一致（不一致 → 零写入 abort）。
    verify_ctx_hashes(home, env, ctx)?;
    let timestamp = unique_backup_timestamp(home);
    let backup_dir = home.join(format!("{MIGRATION_BACKUP_PREFIX}{timestamp}"));
    std::fs::create_dir_all(&backup_dir).map_err(|e| io_err(&backup_dir, e))?;
    set_private_dir(&backup_dir);

    let files = snapshot_authority_files(home, env, &backup_dir)?;
    let mut meta = MigrationMeta {
        version: MIGRATION_SCHEMA_VERSION,
        kind: "marketplace-name-migration".to_string(),
        home: home.display().to_string(),
        timestamp: timestamp.clone(),
        backup_dir: backup_dir.display().to_string(),
        update_declarations: ctx.update_declarations,
        seed_intent: ctx.intent_missing,
        seed_pids: ctx.seed_intent_pids.clone(),
        phase: "prepared".to_string(),
        files,
        items: ctx
            .plan
            .items
            .iter()
            .filter(|i| {
                matches!(
                    i.action,
                    MigrationAction::Rename | MigrationAction::BaselineOnly
                )
            })
            .map(meta_item_from_plan)
            .collect(),
        post_hashes: BTreeMap::new(),
        rolled_back: false,
    };
    write_meta(&backup_dir.join("backup.json"), &meta)?;
    write_journal(home, &meta)?;
    if fail_after == Some("mutate-after-backup") {
        // 测试用：模拟备份后、执行前的外部写入（应被 pre-hash 复核拦截并清理 journal）。
        let path = known_marketplaces_path(Some(home), env);
        std::fs::write(&path, "{}").map_err(|e| io_err(&path, e))?;
    }
    if fail_after == Some("create-settings-after-backup") {
        // 测试用：模拟备份后新出现的 authority 文件（存在性变化也应拦截）。
        let path = user_settings_path(env);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| io_err(parent, e))?;
        }
        std::fs::write(&path, "{}").map_err(|e| io_err(&path, e))?;
    }

    // 执行前哈希复核（与 journal 的 pre-hash 一致；不一致 → 并发写，abort 并清掉毒 journal）。
    if let Err(e) = verify_pre_hashes(home, env, &meta) {
        let _ = std::fs::remove_file(home.join(MIGRATION_JOURNAL_FILENAME));
        return Err(e);
    }
    // pre-v0.3.0 home：事务内（备份 + journal 之后）按账本 seed intent **一次**；
    // 不调用 migrate_ledger_to_intent_once（它会写 project/local enabledPlugins，越界）。
    seed_intent_if_missing(home, env, ctx, fail_after)?;
    if fail_after == Some("after-seed") {
        // 测试用：模拟 seed 已落盘但 journal 尚未更新 post-hash 的硬崩窗口。
        return Err(MarketplaceMigrationError::ConcurrentChange {
            detail: "simulated crash after seed".to_string(),
        });
    }
    meta.post_hashes = current_hashes(home, env, &ctx.user_path)?;
    meta.phase = "applying".to_string();
    write_journal(home, &meta)?;

    let mut report_items: Vec<MigrationItem> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut failed = false;

    for plan_item in ctx.plan.items.iter() {
        if !matches!(
            plan_item.action,
            MigrationAction::Rename | MigrationAction::BaselineOnly
        ) {
            report_items.push(plan_item.clone());
            continue;
        }
        if failed {
            // 前一项失败：后续项不执行，保持原样并明确标记未尝试。
            let mut skipped = plan_item.clone();
            skipped.action = MigrationAction::Blocked;
            skipped.reason = Some(MigrationReason::NotAttempted);
            report_items.push(skipped);
            continue;
        }
        // 逐项 before-image（用于当前项失败回滚）。
        let item_dir = backup_dir
            .join("items")
            .join(safe_dir_name(&plan_item.old_name));
        snapshot_item_files(home, env, &item_dir)?;
        match apply_one_item(
            home,
            env,
            ctx,
            plan_item,
            secrets,
            &mut warnings,
            fail_after,
        ) {
            Ok(()) => {
                report_items.push(plan_item.clone());
                update_meta_item(&mut meta, &plan_item.old_name, "done");
                meta.post_hashes = current_hashes(home, env, &ctx.user_path)?;
                write_journal(home, &meta)?;
            }
            Err(reason) => {
                failed = true;
                // 当前项回滚：反向 rename + 还原 before-image；已 set 的 secret 由 apply_one_item 清理。
                let _ = rollback_item_on_failure(
                    home,
                    env,
                    &ctx.user_path,
                    plan_item,
                    &item_dir,
                    &backup_dir,
                );
                let mut aborted = plan_item.clone();
                aborted.action = MigrationAction::Blocked;
                aborted.reason = Some(reason);
                report_items.push(aborted);
                update_meta_item(&mut meta, &plan_item.old_name, "aborted");
                meta.phase = "aborted".to_string();
                meta.post_hashes = current_hashes(home, env, &ctx.user_path).unwrap_or_default();
                let _ = write_journal(home, &meta);
            }
        }
    }

    if !failed {
        if let Err(e) = verify_all(home, env, &report_items) {
            meta.phase = "aborted".to_string();
            meta.post_hashes = current_hashes(home, env, &ctx.user_path).unwrap_or_default();
            let _ = write_meta(&backup_dir.join("backup.json"), &meta);
            let _ = write_journal(home, &meta);
            return Err(e);
        }
        meta.post_hashes = current_hashes(home, env, &ctx.user_path)?;
        meta.phase = "done".to_string();
        write_meta(&backup_dir.join("backup.json"), &meta)?;
        let _ = std::fs::remove_file(home.join(MIGRATION_JOURNAL_FILENAME));
    } else {
        let _ = write_meta(&backup_dir.join("backup.json"), &meta);
    }

    let mut report = MarketplaceMigrationReport {
        command: "marketplace migrate".to_string(),
        dry_run: false,
        written: true,
        items: report_items,
        conflicts: Vec::new(),
        manual: ctx.plan.manual.clone(),
        backup: Some(BackupRef {
            timestamp,
            path: backup_dir.display().to_string(),
        }),
        warnings,
        corrupt: None,
        status: MigrationStatus::Ok,
    };
    report.items.sort_by(|a, b| a.old_name.cmp(&b.old_name));
    let report = report.finalized();
    Ok(report)
}

fn apply_one_item(
    home: &Path,
    env: Option<&EnvMap>,
    ctx: &PlanContext,
    item: &MigrationItem,
    secrets: &SecretStore,
    warnings: &mut Vec<String>,
    fail_after: Option<&str>,
) -> Result<(), MigrationReason> {
    // 本项已 set 的新 secret 键：任一步失败统一清理（崩溃场景由 resume/rollback 清理）。
    let mut newly_set: Vec<SecretPlan> = Vec::new();
    let result = apply_one_item_inner(
        home,
        env,
        ctx,
        item,
        secrets,
        warnings,
        fail_after,
        &mut newly_set,
    );
    if result.is_err() {
        for done in &newly_set {
            let _ = secrets.delete(&done.new_key);
        }
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn apply_one_item_inner(
    home: &Path,
    env: Option<&EnvMap>,
    ctx: &PlanContext,
    item: &MigrationItem,
    secrets: &SecretStore,
    warnings: &mut Vec<String>,
    fail_after: Option<&str>,
    newly_set: &mut Vec<SecretPlan>,
) -> Result<(), MigrationReason> {
    let old = item.old_name.as_str();
    let new = item.new_name.as_deref().unwrap_or(old);

    if let Some(f) = fail_after {
        if f == "after-entry" {
            return Err(MigrationReason::StateCorrupt);
        }
    }

    if item.action == MigrationAction::Rename {
        // 1) secret set（先写后删；中途失败清理已 set 的新键）。
        if !item.secret_plans.is_empty() {
            if !secrets.available() {
                return Err(MigrationReason::KeyringUnavailable);
            }
            for plan in &item.secret_plans {
                let Some(old_val) = secrets.get(&plan.old_key) else {
                    continue;
                };
                if let Some(existing) = secrets.get(&plan.new_key) {
                    if existing != old_val {
                        return Err(MigrationReason::SecretTargetConflict);
                    }
                    continue;
                }
                if !secrets.set(&plan.new_key, &old_val)
                    || secrets.get(&plan.new_key).as_deref() != Some(old_val.as_str())
                {
                    return Err(MigrationReason::KeyringUnavailable);
                }
                newly_set.push(plan.clone());
            }
        }
        if fail_after == Some("after-secrets") {
            return Err(MigrationReason::StateCorrupt);
        }

        // 2) 文件系统 rename（catalog 再外部树）。
        rename_dir_if_needed(
            &marketplace_skill_dir(home, old, &[]),
            &marketplace_skill_dir(home, new, &[]),
            true,
        )?;
        let old_ext = home
            .join(SOURCE_MARKETPLACE)
            .join(EXTERNAL_PLUGINS_NS)
            .join(old);
        let new_ext = home
            .join(SOURCE_MARKETPLACE)
            .join(EXTERNAL_PLUGINS_NS)
            .join(new);
        // 外部 plugin 树可以不存在（无 Git 源插件）：neither → no-op。
        rename_dir_if_needed(&old_ext, &new_ext, false)?;
        if fail_after == Some("after-rename") {
            return Err(MigrationReason::StateCorrupt);
        }
    }

    // 3) known_marketplaces RMW。
    write_known_item(home, env, item)?;
    if fail_after == Some("after-known") {
        return Err(MigrationReason::StateCorrupt);
    }

    if item.action == MigrationAction::Rename {
        // 4) intent RMW（seed 已在事务开头一次性完成）。
        write_intent_item(home, env, item, &ctx.preserved_intent_ids)?;
        // 5) ledger RMW。
        write_ledger_item(home, env, item)?;
        if fail_after == Some("after-ledger") {
            return Err(MigrationReason::StateCorrupt);
        }
        // 6) user settings RMW。
        write_user_settings_item(home, env, item, ctx.update_declarations)?;
        // 7) 验证。
        verify_item(home, env, item)?;
        // 8) 删除旧 secret（失败仅告警）。
        for plan in &item.secret_plans {
            if secrets.get(&plan.old_key).is_some() && !secrets.delete(&plan.old_key) {
                warnings.push(format!(
                    "failed to delete old secret key {}; remove it manually",
                    plan.old_key
                ));
            }
        }
    }
    Ok(())
}

fn rename_dir_if_needed(
    old_dir: &Path,
    new_dir: &Path,
    required: bool,
) -> Result<(), MigrationReason> {
    let old_exists = old_dir.exists();
    let new_exists = new_dir.exists();
    match (old_exists, new_exists) {
        (true, false) => {
            std::fs::rename(old_dir, new_dir).map_err(|_| MigrationReason::RenameFailed)
        }
        (false, true) => Ok(()), // 崩溃续做：rename 已完成。
        (false, false) if required => Err(MigrationReason::CatalogMissing),
        (false, false) => Ok(()), // 可选树不存在 = 无该来源 plugin。
        (true, true) => Err(MigrationReason::ConcurrentChange),
    }
}

fn write_known_item(
    home: &Path,
    env: Option<&EnvMap>,
    item: &MigrationItem,
) -> Result<(), MigrationReason> {
    let old = item.old_name.clone();
    let new = item.new_name.clone().unwrap_or_else(|| old.clone());
    let source = item.source_value.clone();
    let new_dir = marketplace_skill_dir(home, &new, &[]);
    let mut err: Option<MigrationReason> = None;
    update_known_marketplaces_checked(Some(home), env, |file| {
        let Some(mut entry) = file.account.marketplaces.shift_remove(&old) else {
            // 幂等：已知新键已就位。
            if file.account.marketplaces.contains_key(&new) {
                return ((), false);
            }
            err = Some(MigrationReason::BaselineMismatch);
            return ((), false);
        };
        if let Some(target) = file.account.marketplaces.get(&new) {
            if compare_marketplace_sources(&source, &target.source)
                != MarketplaceSourceRelation::Same
            {
                err = Some(MigrationReason::ConcurrentChange);
                return ((), false);
            }
            // 同源合并：目标值优先，autoUpdate 取 OR，补旧 entry 独有键。
            let mut merged = target.extra.clone();
            for (k, v) in &entry.extra {
                merged.entry(k.clone()).or_insert_with(|| v.clone());
            }
            let old_auto = entry
                .extra
                .get("autoUpdate")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let new_auto = merged
                .get("autoUpdate")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if old_auto || new_auto {
                merged.insert("autoUpdate".to_string(), Value::Bool(true));
            }
            entry.extra = merged;
        }
        if let Ok(dir) = std::fs::canonicalize(&new_dir) {
            entry.extra.insert(
                "installLocation".to_string(),
                Value::String(dir.display().to_string()),
            );
        } else {
            entry.extra.insert(
                "installLocation".to_string(),
                Value::String(normalize_lexical(&new_dir).display().to_string()),
            );
        }
        entry.manifest_name_baseline = Some(new.clone());
        file.account.marketplaces.insert(new.clone(), entry);
        ((), true)
    })
    .map_err(|_| MigrationReason::StateCorrupt)?;
    if let Some(e) = err {
        return Err(e);
    }
    Ok(())
}

fn write_intent_item(
    home: &Path,
    env: Option<&EnvMap>,
    item: &MigrationItem,
    preserved_ids: &[String],
) -> Result<(), MigrationReason> {
    let old = item.old_name.clone();
    let new = item.new_name.clone().unwrap_or_else(|| old.clone());
    let source_pids: HashSet<String> = item.pids.iter().cloned().collect();
    let owned_new: HashSet<String> = item
        .pids
        .iter()
        .filter_map(|pid| {
            pid.strip_suffix(&format!("@{old}"))
                .map(|plugin| format!("{plugin}@{new}"))
        })
        .collect();
    let mut err: Option<MigrationReason> = None;
    let res = try_update_installed_plugins_intent(
        |file| {
            // tolerant coerce 会丢弃 no-`@` local-only id；写回时显式保留。
            for pid in preserved_ids {
                file.account.installed_plugins.insert(pid.clone());
            }
            let suffix = format!("@{new}");
            for pid in &file.account.installed_plugins {
                if pid.ends_with(&suffix) && !source_pids.contains(pid) && !owned_new.contains(pid)
                {
                    err = Some(MigrationReason::ConcurrentChange);
                    return false;
                }
            }
            let old_set: HashSet<String> = source_pids.clone();
            let mut rebuilt = IndexSet::new();
            for pid in &file.account.installed_plugins {
                if let Some(plugin) = pid.strip_suffix(&format!("@{old}")) {
                    if old_set.contains(pid) {
                        rebuilt.insert(format!("{plugin}@{new}"));
                        continue;
                    }
                }
                rebuilt.insert(pid.clone());
            }
            file.account.installed_plugins = rebuilt;
            true
        },
        Some(home),
        env,
    )
    .map_err(|_| MigrationReason::StateCorrupt)?;
    if res.is_none() {
        return Err(err.unwrap_or(MigrationReason::ConcurrentChange));
    }
    Ok(())
}

fn write_ledger_item(
    home: &Path,
    env: Option<&EnvMap>,
    item: &MigrationItem,
) -> Result<(), MigrationReason> {
    let old = item.old_name.clone();
    let new = item.new_name.clone().unwrap_or_else(|| old.clone());
    let source_pids: HashSet<String> = item.pids.iter().cloned().collect();
    let owned_new: HashSet<String> = item
        .pids
        .iter()
        .filter_map(|pid| {
            pid.strip_suffix(&format!("@{old}"))
                .map(|plugin| format!("{plugin}@{new}"))
        })
        .collect();
    let mut err: Option<MigrationReason> = None;
    update_installed_plugins_checked(Some(home), env, |file| {
        let suffix = format!("@{new}");
        for pid in file.account.plugins.keys() {
            if pid.ends_with(&suffix) && !source_pids.contains(pid) && !owned_new.contains(pid) {
                err = Some(MigrationReason::ConcurrentChange);
                return ((), false);
            }
        }
        let mut rebuilt: indexmap::IndexMap<String, Vec<InstalledPluginRecord>> =
            indexmap::IndexMap::new();
        for (pid, records) in file.account.plugins.iter() {
            if source_pids.contains(pid) {
                let mut recs = records.clone();
                for rec in &mut recs {
                    if let Some(path) = rec.install_path.as_deref().filter(|p| !p.is_empty()) {
                        match rewrite_install_path(path, home, &old, &new) {
                            Ok(rewritten) => rec.install_path = Some(rewritten),
                            Err(()) => {
                                err = Some(MigrationReason::InstallPathUnexpected);
                                return ((), false);
                            }
                        }
                    }
                }
                let plugin = pid.strip_suffix(&format!("@{old}")).unwrap_or(pid.as_str());
                rebuilt.insert(format!("{plugin}@{new}"), recs);
            } else {
                rebuilt.insert(pid.clone(), records.clone());
            }
        }
        file.account.plugins = rebuilt;
        ((), true)
    })
    .map_err(|_| MigrationReason::StateCorrupt)?;
    if let Some(e) = err {
        return Err(e);
    }
    Ok(())
}

fn write_user_settings_item(
    _home: &Path,
    env: Option<&EnvMap>,
    item: &MigrationItem,
    update_declarations: bool,
) -> Result<(), MigrationReason> {
    let old = item.old_name.clone();
    let new = item.new_name.clone().unwrap_or_else(|| old.clone());
    let path = user_settings_path(env);
    let Ok(map) = raw_write::read_strict_map(&path) else {
        return Err(MigrationReason::StateCorrupt);
    };
    let mut changed = false;
    // 预判是否有 user 侧变更（无变更不建文件/锁）。
    let old_suffix = format!("@{old}");
    if let Some(Value::Object(enabled)) = map.get(FIELD_ENABLED_PLUGINS) {
        changed |= enabled.keys().any(|k| k.ends_with(&old_suffix));
    }
    for field in [FIELD_TRUSTED_MARKETPLACES, FIELD_BLOCKED_MARKETPLACES] {
        if let Some(Value::Array(arr)) = map.get(field) {
            changed |= arr.iter().any(|v| v.as_str() == Some(old.as_str()));
        }
    }
    if update_declarations {
        if let Some(Value::Object(extra)) = map.get(FIELD_EXTRA_KNOWN_MARKETPLACES) {
            changed |= extra.contains_key(&old);
        }
    }
    if !changed {
        return Ok(());
    }
    let mut err: Option<MigrationReason> = None;
    let path_for_closure = path.clone();
    raw_write::update_raw_settings(&path, false, |map| {
        // 容器类型错误 → 立即 fail-closed（不部分写）；所有已知字段先校验后改。
        let enabled = match raw_write::object_field_opt_mut(
            map,
            FIELD_ENABLED_PLUGINS,
            &path_for_closure,
        ) {
            Ok(v) => v,
            Err(e) => {
                err = Some(MigrationReason::StateCorrupt);
                return Err(e);
            }
        };
        if let Some(enabled) = enabled {
            let keys: Vec<String> = enabled
                .keys()
                .filter(|k| k.ends_with(&old_suffix))
                .cloned()
                .collect();
            for key in keys {
                let value = enabled.get(&key).cloned().unwrap_or(Value::Null);
                let plugin = key.trim_end_matches(&old_suffix).to_string();
                let new_key = format!("{plugin}@{new}");
                if let Some(existing) = enabled.get(&new_key) {
                    if existing != &value {
                        err = Some(MigrationReason::ConcurrentChange);
                        return Err(raw_write::RawWriteError::Io {
                            path: path_for_closure.clone(),
                            source: io::Error::new(
                                io::ErrorKind::InvalidData,
                                "enabledPlugins target key already exists with a different value",
                            ),
                        });
                    }
                }
                enabled.insert(new_key, value);
                enabled.remove(&key);
            }
        }
        // trusted / blocked：加新名一次、删旧名；保留非字符串兄弟值。
        for field in [FIELD_TRUSTED_MARKETPLACES, FIELD_BLOCKED_MARKETPLACES] {
            let arr = match raw_write::array_field_opt_mut(map, field, &path_for_closure) {
                Ok(v) => v,
                Err(e) => {
                    err = Some(MigrationReason::StateCorrupt);
                    return Err(e);
                }
            };
            if let Some(arr) = arr {
                if arr.iter().any(|v| v.as_str() == Some(old.as_str())) {
                    if !arr.iter().any(|v| v.as_str() == Some(new.as_str())) {
                        arr.push(Value::String(new.clone()));
                    }
                    arr.retain(|v| v.as_str() != Some(old.as_str()));
                }
            }
        }
        // extraKnownMarketplaces（仅 --update-declarations）：同源移动/合并。
        if update_declarations {
            let extra = match raw_write::object_field_opt_mut(
                map,
                FIELD_EXTRA_KNOWN_MARKETPLACES,
                &path_for_closure,
            ) {
                Ok(v) => v,
                Err(e) => {
                    err = Some(MigrationReason::StateCorrupt);
                    return Err(e);
                }
            };
            if let Some(extra) = extra {
                if let Some(mut decl) = extra.remove(&old) {
                    if let Some(existing) = extra.get_mut(&new) {
                        let existing_source =
                            existing.get("source").cloned().unwrap_or(Value::Null);
                        let decl_source = decl.get("source").cloned().unwrap_or(Value::Null);
                        if compare_marketplace_sources(&decl_source, &existing_source)
                            != MarketplaceSourceRelation::Same
                        {
                            err = Some(MigrationReason::ConcurrentChange);
                            return Err(raw_write::RawWriteError::Io {
                                path: path_for_closure.clone(),
                                source: io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "extraKnownMarketplaces target conflicts with existing declaration",
                                ),
                            });
                        }
                        // 同源合并：目标为基底，旧声明独有字段补齐，autoUpdate 取 OR。
                        if let (Some(old_obj), Some(new_obj)) =
                            (decl.as_object(), existing.as_object_mut())
                        {
                            for (k, v) in old_obj {
                                if k == "source" {
                                    continue;
                                }
                                if k == "autoUpdate" {
                                    let old_flag = v.as_bool().unwrap_or(false);
                                    let new_flag = new_obj
                                        .get("autoUpdate")
                                        .and_then(Value::as_bool)
                                        .unwrap_or(false);
                                    new_obj.insert(
                                        "autoUpdate".to_string(),
                                        Value::Bool(old_flag || new_flag),
                                    );
                                } else {
                                    new_obj.entry(k.clone()).or_insert_with(|| v.clone());
                                }
                            }
                        }
                    } else {
                        extra.insert(new.clone(), decl.take());
                    }
                }
            }
        }
        Ok(())
    })
    .map_err(|_| MigrationReason::StateCorrupt)?;
    if let Some(e) = err {
        return Err(e);
    }
    Ok(())
}

fn verify_item(
    home: &Path,
    env: Option<&EnvMap>,
    item: &MigrationItem,
) -> Result<(), MigrationReason> {
    let old = item.old_name.as_str();
    let new = item.new_name.as_deref().unwrap_or(old);
    let known = load_known_marketplaces_strict(Some(home), env)
        .map_err(|_| MigrationReason::StateCorrupt)?;
    if item.action == MigrationAction::Rename
        && (known.account.marketplaces.contains_key(old)
            || !marketplace_skill_dir(home, new, &[]).is_dir())
    {
        return Err(MigrationReason::StateCorrupt);
    }
    let entry = known
        .account
        .marketplaces
        .get(new)
        .ok_or(MigrationReason::StateCorrupt)?;
    if entry.manifest_name_baseline.as_deref() != Some(new) {
        return Err(MigrationReason::StateCorrupt);
    }
    if item.action == MigrationAction::Rename {
        let intent = load_installed_plugins_intent_strict(Some(home), env)
            .map_err(|_| MigrationReason::StateCorrupt)?;
        for pid in &item.pids {
            if intent.account.installed_plugins.contains(pid) {
                return Err(MigrationReason::StateCorrupt);
            }
        }
        let ledger = load_installed_plugins_strict(Some(home), env)
            .map_err(|_| MigrationReason::StateCorrupt)?;
        if item
            .pids
            .iter()
            .any(|pid| ledger.account.plugins.contains_key(pid))
        {
            return Err(MigrationReason::StateCorrupt);
        }
        if old != new {
            for (old_dir, new_dir) in [
                (
                    marketplace_skill_dir(home, old, &[]),
                    marketplace_skill_dir(home, new, &[]),
                ),
                (
                    home.join(SOURCE_MARKETPLACE)
                        .join(EXTERNAL_PLUGINS_NS)
                        .join(old),
                    home.join(SOURCE_MARKETPLACE)
                        .join(EXTERNAL_PLUGINS_NS)
                        .join(new),
                ),
            ] {
                if old_dir.exists() && !new_dir.exists() {
                    return Err(MigrationReason::StateCorrupt);
                }
            }
        }
    }
    Ok(())
}

fn verify_all(
    home: &Path,
    env: Option<&EnvMap>,
    items: &[MigrationItem],
) -> Result<(), MarketplaceMigrationError> {
    for item in items {
        if matches!(
            item.action,
            MigrationAction::Rename | MigrationAction::BaselineOnly
        ) {
            verify_item(home, env, item).map_err(|_| {
                MarketplaceMigrationError::ConcurrentChange {
                    detail: format!("post-migration verification failed for {:?}", item.old_name),
                }
            })?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 逐项回滚 / Per-item rollback
// ---------------------------------------------------------------------------
fn rollback_item_on_failure(
    home: &Path,
    env: Option<&EnvMap>,
    user_path: &Path,
    item: &MigrationItem,
    item_backup_dir: &Path,
    global_backup_dir: &Path,
) -> Result<(), MarketplaceMigrationError> {
    let old = item.old_name.as_str();
    let new = item.new_name.as_deref().unwrap_or(old);
    if item.action == MigrationAction::Rename {
        // 反向 rename（外部树先、clone 后），仅当 new 在且 old 不在。
        let pairs = [
            (
                home.join(SOURCE_MARKETPLACE)
                    .join(EXTERNAL_PLUGINS_NS)
                    .join(new),
                home.join(SOURCE_MARKETPLACE)
                    .join(EXTERNAL_PLUGINS_NS)
                    .join(old),
            ),
            (
                marketplace_skill_dir(home, new, &[]),
                marketplace_skill_dir(home, old, &[]),
            ),
        ];
        for (from, to) in pairs {
            if from.exists() && !to.exists() {
                let _ = std::fs::rename(&from, &to);
            }
        }
    }
    // 还原逐项 before-image：snapshot 存在则还原；不存在 = 该项开始时文件不存在 → 删除本项创建的文件。
    let _ = global_backup_dir;
    for (key, path) in authority_paths(home, env) {
        let from = item_backup_dir.join(key);
        if let Ok(bytes) = std::fs::read(&from) {
            let _ = atomic_write_bytes(&path, &bytes);
        } else if path.is_file() {
            let _ = std::fs::remove_file(&path);
        }
    }
    let _ = user_path;
    Ok(())
}

// ---------------------------------------------------------------------------
// journal / 备份 / 哈希 / Journal, backup, hashes
// ---------------------------------------------------------------------------
#[derive(Debug, Clone, Serialize, Deserialize)]
struct MigrationMeta {
    version: u32,
    kind: String,
    home: String,
    timestamp: String,
    backup_dir: String,
    update_declarations: bool,
    /// 该事务是否需要在 items 循环前 seed intent（pre-v0.3.0 home）。
    #[serde(default)]
    seed_intent: bool,
    /// seed 使用的账本 pid 全集（用于 resume 时校验 seed 窗口内容）。
    #[serde(default)]
    seed_pids: Vec<String>,
    phase: String,
    files: Vec<BackupFile>,
    items: Vec<MetaItem>,
    post_hashes: BTreeMap<String, String>,
    rolled_back: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BackupFile {
    key: String,
    path: String,
    existed: bool,
    snapshot: Option<String>,
    sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MetaItem {
    old: String,
    new: String,
    action: String,
    pids: Vec<String>,
    secret_keys: Vec<MetaSecret>,
    status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MetaSecret {
    plugin: String,
    input: String,
    old_key: String,
    new_key: String,
}

fn meta_item_from_plan(item: &MigrationItem) -> MetaItem {
    MetaItem {
        old: item.old_name.clone(),
        new: item
            .new_name
            .clone()
            .unwrap_or_else(|| item.old_name.clone()),
        action: format!("{:?}", item.action).to_lowercase(),
        pids: item.pids.clone(),
        secret_keys: item
            .secret_plans
            .iter()
            .map(|s| MetaSecret {
                plugin: s.plugin.clone(),
                input: s.input_id.clone(),
                old_key: s.old_key.clone(),
                new_key: s.new_key.clone(),
            })
            .collect(),
        status: "pending".to_string(),
    }
}

fn update_meta_item(meta: &mut MigrationMeta, old: &str, status: &str) {
    if let Some(item) = meta.items.iter_mut().find(|i| i.old == old) {
        item.status = status.to_string();
    }
}

fn authority_paths(home: &Path, env: Option<&EnvMap>) -> Vec<(&'static str, PathBuf)> {
    vec![
        (
            "known_marketplaces.json",
            known_marketplaces_path(Some(home), env),
        ),
        (
            "installed_plugins.json",
            installed_plugins_path(Some(home), env),
        ),
        (
            "installed_plugins_intent.json",
            installed_plugins_intent_path(Some(home), env),
        ),
        ("settings.json", user_settings_path(env)),
    ]
}

fn snapshot_authority_files(
    home: &Path,
    env: Option<&EnvMap>,
    backup_dir: &Path,
) -> Result<Vec<BackupFile>, MarketplaceMigrationError> {
    let files_dir = backup_dir.join("files");
    std::fs::create_dir_all(&files_dir).map_err(|e| io_err(&files_dir, e))?;
    let mut out = Vec::new();
    for (key, path) in authority_paths(home, env) {
        let existed = path.is_file();
        let mut snapshot = None;
        let mut sha = None;
        if existed {
            let bytes = std::fs::read(&path).map_err(|e| io_err(&path, e))?;
            sha = Some(hex(&bytes));
            let dest = files_dir.join(key);
            #[cfg(unix)]
            atomic_write_bytes_mode(&dest, &bytes, Some(0o600)).map_err(|e| io_err(&dest, e))?;
            #[cfg(not(unix))]
            atomic_write_bytes(&dest, &bytes).map_err(|e| io_err(&dest, e))?;
            snapshot = Some(format!("files/{key}"));
        }
        out.push(BackupFile {
            key: key.to_string(),
            path: path.display().to_string(),
            existed,
            snapshot,
            sha256: sha,
        });
    }
    Ok(out)
}

fn snapshot_item_files(
    home: &Path,
    env: Option<&EnvMap>,
    item_dir: &Path,
) -> Result<(), MarketplaceMigrationError> {
    std::fs::create_dir_all(item_dir).map_err(|e| io_err(item_dir, e))?;
    for (key, path) in authority_paths(home, env) {
        if path.is_file() {
            let dest = item_dir.join(key);
            let bytes = std::fs::read(&path).map_err(|e| io_err(&path, e))?;
            #[cfg(unix)]
            atomic_write_bytes_mode(&dest, &bytes, Some(0o600)).map_err(|e| io_err(&dest, e))?;
            #[cfg(not(unix))]
            atomic_write_bytes(&dest, &bytes).map_err(|e| io_err(&dest, e))?;
        }
    }
    Ok(())
}

fn current_hashes(
    home: &Path,
    env: Option<&EnvMap>,
    _user_path: &Path,
) -> Result<BTreeMap<String, String>, MarketplaceMigrationError> {
    let mut out = BTreeMap::new();
    for (key, path) in authority_paths(home, env) {
        if path.is_file() {
            let bytes = std::fs::read(&path).map_err(|e| io_err(&path, e))?;
            out.insert(key.to_string(), hex(&bytes));
        }
    }
    Ok(out)
}

/// 把 user-scope `enabledPlugins[pid] = true`（仅 absent；不写 project/local）。
fn backfill_enabled_plugins(
    home: &Path,
    env: Option<&EnvMap>,
    pids: &[String],
) -> Result<(), MarketplaceMigrationError> {
    let _ = home;
    let user_path = user_settings_path(env);
    let marketplace_pids: Vec<&String> = pids.iter().filter(|p| p.contains('@')).collect();
    if marketplace_pids.is_empty() {
        return Ok(());
    }
    raw_write::update_raw_settings(&user_path, false, |map| {
        let enabled = raw_write::object_field_mut(map, FIELD_ENABLED_PLUGINS, &user_path)?;
        for pid in &marketplace_pids {
            enabled.entry((*pid).clone()).or_insert(Value::Bool(true));
        }
        Ok(())
    })
    .map_err(raw_write_error_to_migration)?;
    Ok(())
}

fn raw_write_error_to_migration(error: raw_write::RawWriteError) -> MarketplaceMigrationError {
    match error {
        raw_write::RawWriteError::Store(e) => e.into(),
        raw_write::RawWriteError::Io { path, source } => {
            MarketplaceMigrationError::Io { path, source }
        }
        raw_write::RawWriteError::Corrupt { path, reason } => {
            MarketplaceMigrationError::StateCorrupt { path, reason }
        }
        raw_write::RawWriteError::MalformedTarget {
            path,
            key,
            expected,
        } => MarketplaceMigrationError::StateCorrupt {
            path,
            reason: format!("field {key:?} has unexpected type (expected {expected})"),
        },
    }
}

/// 从当前 intent 回填 user-scope enabledPlugins（resume 半 seed 窗口幂等修复）。
fn backfill_enabled_from_intent(
    home: &Path,
    env: Option<&EnvMap>,
) -> Result<(), MarketplaceMigrationError> {
    let intent = crate::settings::store::load_installed_plugins_intent_strict(Some(home), env)?;
    let pids: Vec<String> = intent.account.installed_plugins.iter().cloned().collect();
    backfill_enabled_plugins(home, env, &pids)
}

/// pre-v0.3.0：intent 缺失时在事务内按账本 pid 一次性 seed（不写 project/local）。
///
/// 写序：先 settings 回填、后 intent——若在两次写之间崩溃，intent 仍缺失，resume 会重跑整个 seed，
/// 不会出现「intent 已在、enabled 回填丢失」的熄灯窗口。
fn seed_intent_if_missing(
    home: &Path,
    env: Option<&EnvMap>,
    ctx: &PlanContext,
    fail_after: Option<&str>,
) -> Result<(), MarketplaceMigrationError> {
    if !ctx.intent_missing {
        return Ok(());
    }
    let seed = ctx.seed_intent_pids.clone();
    backfill_enabled_plugins(home, env, &seed)?;
    if fail_after == Some("after-seed-settings") {
        return Err(MarketplaceMigrationError::ConcurrentChange {
            detail: "simulated crash between seed writes".to_string(),
        });
    }
    crate::settings::store::update_installed_plugins_intent(
        |file| {
            for pid in &seed {
                file.account.installed_plugins.insert(pid.clone());
            }
        },
        Some(home),
        env,
    )?;
    Ok(())
}

fn verify_ctx_hashes(
    home: &Path,
    env: Option<&EnvMap>,
    ctx: &PlanContext,
) -> Result<(), MarketplaceMigrationError> {
    let current = current_hashes(home, env, &ctx.user_path)?;
    for (key, path) in authority_paths(home, env) {
        let now = current.get(key);
        let planned = ctx.pre_hashes.get(key);
        if now != planned {
            return Err(MarketplaceMigrationError::ConcurrentChange {
                detail: format!(
                    "{} changed after preflight; rerun `marketplace migrate`",
                    path.display()
                ),
            });
        }
    }
    Ok(())
}

fn verify_pre_hashes(
    _home: &Path,
    _env: Option<&EnvMap>,
    meta: &MigrationMeta,
) -> Result<(), MarketplaceMigrationError> {
    for file in &meta.files {
        let path = PathBuf::from(&file.path);
        let exists = path.is_file();
        if exists != file.existed {
            return Err(MarketplaceMigrationError::ConcurrentChange {
                detail: format!(
                    "{} existence changed after preflight; rerun `marketplace migrate`",
                    path.display()
                ),
            });
        }
        if file.existed {
            let bytes = std::fs::read(&path).map_err(|e| io_err(&path, e))?;
            if Some(hex(&bytes)) != file.sha256 {
                return Err(MarketplaceMigrationError::ConcurrentChange {
                    detail: format!(
                        "{} changed after preflight; rerun `marketplace migrate`",
                        path.display()
                    ),
                });
            }
        }
    }
    Ok(())
}

fn write_journal(home: &Path, meta: &MigrationMeta) -> Result<(), MarketplaceMigrationError> {
    let path = home.join(MIGRATION_JOURNAL_FILENAME);
    write_meta(&path, meta)
}

fn write_meta(path: &Path, meta: &MigrationMeta) -> Result<(), MarketplaceMigrationError> {
    let text = serde_json::to_string_pretty(meta)
        .map_err(|e| io_err(path, io::Error::new(io::ErrorKind::InvalidData, e)))?;
    let bytes = format!("{text}\n").into_bytes();
    #[cfg(unix)]
    atomic_write_bytes_mode(path, &bytes, Some(0o600)).map_err(|e| io_err(path, e))?;
    #[cfg(not(unix))]
    atomic_write_bytes(path, &bytes).map_err(|e| io_err(path, e))?;
    Ok(())
}

fn read_journal(path: &Path) -> Result<MigrationMeta, MarketplaceMigrationError> {
    if !path.exists() {
        return Err(MarketplaceMigrationError::NoJournal);
    }
    read_meta_file(path)
}

fn read_meta_file(path: &Path) -> Result<MigrationMeta, MarketplaceMigrationError> {
    let text = std::fs::read_to_string(path).map_err(|e| io_err(path, e))?;
    serde_json::from_str(&text).map_err(|e| MarketplaceMigrationError::JournalCorrupt {
        reason: format!("{path}: {e}", path = path.display()),
    })
}

// ---------------------------------------------------------------------------
// resume / rollback / Resume & rollback
// ---------------------------------------------------------------------------
fn resume_from_journal(
    home: &Path,
    env: Option<&EnvMap>,
    cwd: Option<&Path>,
    secrets: &SecretStore,
    mut meta: MigrationMeta,
    backup_dir: &Path,
) -> Result<MarketplaceMigrationReport, MarketplaceMigrationError> {
    // 重新加载当前状态并构造 items（以 journal 的 old/new 映射为准）。
    let ctx = load_context(
        home,
        env,
        cwd,
        MarketplaceMigrationOptions {
            update_declarations: meta.update_declarations,
        },
        secrets,
    )?;
    verify_resume_hashes(home, env, &meta)?;
    // pre-v0.3.0：无论 intent 是否已存在，都幂等重跑 seed + enabled 回填（修复半 seed 崩溃窗口）。
    if meta.seed_intent {
        seed_intent_if_missing(home, env, &ctx, None)?;
        backfill_enabled_from_intent(home, env)?;
        meta.post_hashes = current_hashes(home, env, &ctx.user_path)?;
        let _ = write_journal(home, &meta);
    }
    let manual = if ctx.plan.manual.is_empty() {
        manual_for_journal(home, env, cwd, &meta)
    } else {
        ctx.plan.manual.clone()
    };
    let mut report_items: Vec<MigrationItem> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut failed = false;
    let done_items: Vec<MetaItem> = meta
        .items
        .iter()
        .filter(|i| i.status == "done")
        .cloned()
        .collect();
    for m in &done_items {
        let mut item = synthesized_item(m, &ctx);
        if m.action == "baselineonly" {
            item.action = MigrationAction::BaselineOnly;
            item.new_name = Some(m.old.clone());
        }
        report_items.push(item);
    }
    let pending: Vec<MetaItem> = meta
        .items
        .iter()
        .filter(|i| i.status != "done")
        .cloned()
        .collect();
    for meta_item in pending.iter() {
        if failed {
            break;
        }
        // 以 journal 的 old/new 映射驱动（崩溃后旧目录可能已不在，不能依赖重新检测）。
        let mut item = synthesized_item(meta_item, &ctx);
        if meta_item.action == "baselineonly" {
            item.action = MigrationAction::BaselineOnly;
            item.new_name = Some(meta_item.old.clone());
        }
        let item_dir = backup_dir
            .join("items")
            .join(format!("{}-resume", safe_dir_name(&item.old_name)));
        snapshot_item_files(home, env, &item_dir)?;
        match apply_one_item(home, env, &ctx, &item, secrets, &mut warnings, None) {
            Ok(()) => {
                report_items.push(item);
                update_meta_item(&mut meta, &meta_item.old, "done");
                meta.post_hashes = current_hashes(home, env, &ctx.user_path)?;
                let _ = write_journal(home, &meta);
            }
            Err(reason) => {
                failed = true;
                let _ = rollback_item_on_failure(
                    home,
                    env,
                    &ctx.user_path,
                    &item,
                    &item_dir,
                    backup_dir,
                );
                meta.phase = "aborted".to_string();
                meta.post_hashes = current_hashes(home, env, &ctx.user_path).unwrap_or_default();
                let mut aborted = item;
                aborted.action = MigrationAction::Blocked;
                aborted.reason = Some(reason);
                report_items.push(aborted);
            }
        }
    }
    if !failed {
        // 续做完成前对全部 done 项复跑后置条件（含本轮与历史项），防止「静默洗白」不一致状态。
        for m in &done_items {
            let mut item = synthesized_item(m, &ctx);
            if m.action == "baselineonly" {
                item.action = MigrationAction::BaselineOnly;
                item.new_name = Some(m.old.clone());
            }
            if let Err(reason) = verify_item(home, env, &item) {
                failed = true;
                meta.phase = "aborted".to_string();
                let mut aborted = item;
                aborted.action = MigrationAction::Blocked;
                aborted.reason = Some(reason);
                report_items.push(aborted);
                break;
            }
        }
    }
    if !failed {
        meta.phase = "done".to_string();
        meta.post_hashes = current_hashes(home, env, &ctx.user_path)?;
    }
    let _ = write_meta(&backup_dir.join("backup.json"), &meta);
    if !failed {
        let _ = std::fs::remove_file(home.join(MIGRATION_JOURNAL_FILENAME));
    } else {
        let _ = write_journal(home, &meta);
    }
    Ok(MarketplaceMigrationReport {
        command: "marketplace migrate --resume".to_string(),
        dry_run: false,
        written: true,
        items: report_items,
        conflicts: Vec::new(),
        manual,
        backup: Some(BackupRef {
            timestamp: meta.timestamp.clone(),
            path: backup_dir.display().to_string(),
        }),
        warnings,
        corrupt: None,
        status: MigrationStatus::Ok,
    }
    .finalized())
}

/// resume 时若 fresh plan 已看不到旧名（known 已被改写），按 journal 的 old/new 重建手动引用报告。
fn manual_for_journal(
    home: &Path,
    env: Option<&EnvMap>,
    cwd: Option<&Path>,
    meta: &MigrationMeta,
) -> Vec<ManualReference> {
    let ledger = crate::settings::store::load_installed_plugins(Some(home), env);
    let user_map = raw_write::read_strict_map(&user_settings_path(env)).unwrap_or_default();
    let items: Vec<MigrationItem> = meta
        .items
        .iter()
        .map(|m| MigrationItem {
            old_name: m.old.clone(),
            new_name: Some(m.new.clone()),
            action: MigrationAction::Rename,
            reason: None,
            source: String::new(),
            plugins: m
                .pids
                .iter()
                .filter_map(|p| p.strip_suffix(&format!("@{}", m.old)).map(str::to_string))
                .collect(),
            secrets: m.secret_keys.len(),
            baseline: None,
            env_renames: Vec::new(),
            source_value: Value::Null,
            pids: m.pids.clone(),
            secret_plans: Vec::new(),
            install_paths: Vec::new(),
        })
        .collect();
    collect_manual_references(
        env,
        cwd,
        &items,
        &ledger,
        &user_map,
        meta.update_declarations,
    )
}

fn synthesized_item(meta_item: &MetaItem, ctx: &PlanContext) -> MigrationItem {
    // journal 不存 source（可能含凭据）：从当前 known 账本（旧名或新名）取回。
    let known = crate::settings::store::load_known_marketplaces(Some(&ctx.home), None).account;
    let source_value = known
        .marketplaces
        .get(&meta_item.old)
        .or_else(|| known.marketplaces.get(&meta_item.new))
        .map(|e| e.source.clone())
        .unwrap_or(Value::Null);
    MigrationItem {
        old_name: meta_item.old.clone(),
        new_name: Some(meta_item.new.clone()),
        action: MigrationAction::Rename,
        reason: None,
        source: display_source(&source_value),
        plugins: meta_item
            .pids
            .iter()
            .filter_map(|p| {
                p.strip_suffix(&format!("@{}", meta_item.old))
                    .map(str::to_string)
            })
            .collect(),
        secrets: meta_item.secret_keys.len(),
        baseline: None,
        env_renames: Vec::new(),
        source_value,
        pids: meta_item.pids.clone(),
        secret_plans: meta_item
            .secret_keys
            .iter()
            .map(|s| SecretPlan {
                plugin: s.plugin.clone(),
                input_id: s.input.clone(),
                old_key: s.old_key.clone(),
                new_key: s.new_key.clone(),
            })
            .collect(),
        install_paths: Vec::new(),
    }
}

/// resume 前哈希校验：当前文件必须等于 journal 的 pre-hash（尚未写）或最近 post-hash（已写），
/// 否则视为并发修改，拒绝续做。
fn verify_resume_hashes(
    home: &Path,
    env: Option<&EnvMap>,
    meta: &MigrationMeta,
) -> Result<(), MarketplaceMigrationError> {
    let paths: BTreeMap<&str, PathBuf> = authority_paths(home, env).into_iter().collect();
    for file in &meta.files {
        let path = paths
            .get(file.key.as_str())
            .cloned()
            .unwrap_or_else(|| PathBuf::from(&file.path));
        let current = if path.is_file() {
            Some(hex(&std::fs::read(&path).map_err(|e| io_err(&path, e))?))
        } else {
            None
        };
        let pre = file.sha256.clone();
        let post = meta.post_hashes.get(&file.key).cloned();
        // seed 窗口：phase=prepared 且本事务声明 seed；intent/settings 由 seed 写入但 journal
        // 尚未记录 post-hash（进程被 kill）。允许 resume 继续（seed 幂等）。
        let mut seed_window = meta.seed_intent
            && meta.phase == "prepared"
            && matches!(
                file.key.as_str(),
                "installed_plugins_intent.json" | "settings.json"
            )
            && current.is_some();
        if seed_window && file.key == "installed_plugins_intent.json" {
            // seed 窗口只接受「恰好等于 seed 集合」的 intent；外部写入的 dangling pid 必须拒绝。
            let intent =
                crate::settings::store::load_installed_plugins_intent_strict(Some(home), env)?;
            let current_set: HashSet<&String> = intent.account.installed_plugins.iter().collect();
            let expected: HashSet<&String> = meta.seed_pids.iter().collect();
            seed_window = current_set == expected;
        }
        if current != pre && current != post && !seed_window {
            return Err(MarketplaceMigrationError::ConcurrentChange {
                detail: format!(
                    "{} changed after the interrupted migration; roll back or retry manually",
                    path.display()
                ),
            });
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn rollback_from_meta(
    home: &Path,
    env: Option<&EnvMap>,
    timestamp: &str,
    meta: &MigrationMeta,
    backup_dir: &Path,
    journal_path: &Path,
    secrets: &SecretStore,
    in_progress: bool,
) -> Result<MarketplaceRollbackReport, MarketplaceMigrationError> {
    if meta.rolled_back {
        // 幂等：已回滚过 → 直接成功返回（不再次改动状态）。
        return Ok(MarketplaceRollbackReport {
            timestamp: timestamp.to_string(),
            ..Default::default()
        });
    }
    #[allow(unused_mut)]
    let mut report = MarketplaceRollbackReport {
        timestamp: timestamp.to_string(),
        ..Default::default()
    };
    let paths: BTreeMap<&str, PathBuf> = authority_paths(home, env).into_iter().collect();
    if in_progress {
        report.notes.push(
            "rolling back an interrupted migration: restoring the pre-migration backup (partial migration state will be discarded)"
                .to_string(),
        );
    }
    // 漂移检测（仅 done/aborted）：当前文件必须等于迁移后的 post-hash；未被迁移写过的文件必须保持
    // pre-hash（含「原本不存在、现在被创建」）。中断态跳过（可能是迁移自身的半写）。
    for file in meta.files.iter().filter(|_| !in_progress) {
        let path = paths
            .get(file.key.as_str())
            .cloned()
            .unwrap_or_else(|| PathBuf::from(&file.path));
        let current = if path.is_file() {
            Some(hex(&std::fs::read(&path).map_err(|e| io_err(&path, e))?))
        } else {
            None
        };
        let expected = match meta.post_hashes.get(&file.key) {
            Some(post) => Some(post.clone()),
            None => file.sha256.clone(),
        };
        if current != expected {
            return Err(MarketplaceMigrationError::RollbackDrift {
                detail: format!(
                    "{} changed after migration; refusing to overwrite",
                    path.display()
                ),
            });
        }
    }
    // 反向 rename（外部树先、clone 后）。不依赖 journal status：硬崩溃可能留下 pending 但目录已改名。
    for item in &meta.items {
        if item.action == "baselineonly" {
            continue;
        }
        let pairs = [
            (
                home.join(SOURCE_MARKETPLACE)
                    .join(EXTERNAL_PLUGINS_NS)
                    .join(&item.new),
                home.join(SOURCE_MARKETPLACE)
                    .join(EXTERNAL_PLUGINS_NS)
                    .join(&item.old),
            ),
            (
                marketplace_skill_dir(home, &item.new, &[]),
                marketplace_skill_dir(home, &item.old, &[]),
            ),
        ];
        let mut renamed = false;
        for (from, to) in pairs {
            if from.exists() && !to.exists() {
                std::fs::rename(&from, &to).map_err(|e| io_err(&from, e))?;
                renamed = true;
            }
        }
        if renamed {
            report.restored.push(item.old.clone());
        }
    }
    // 还原四份权威文件（按 key 映射到当前 env 解析出的路径，避免恢复到他处）。
    for file in &meta.files {
        let path = paths
            .get(file.key.as_str())
            .cloned()
            .unwrap_or_else(|| PathBuf::from(&file.path));
        if file.existed {
            let snapshot = backup_dir.join(file.snapshot.as_deref().unwrap_or_default());
            let bytes = std::fs::read(&snapshot).map_err(|e| io_err(&snapshot, e))?;
            atomic_write_bytes(&path, &bytes).map_err(|e| io_err(&path, e))?;
        } else if path.is_file() {
            std::fs::remove_file(&path).map_err(|e| io_err(&path, e))?;
        }
    }
    // 反写 secret（new→old）；keyring 不可用 → 警告。
    for item in &meta.items {
        for secret in &item.secret_keys {
            if !secrets.available() {
                report
                    .warnings
                    .push("keyring unavailable; secrets were not reversed".to_string());
                break;
            }
            if let Some(value) = secrets.get(&secret.new_key) {
                if secrets.set(&secret.old_key, &value) {
                    let _ = secrets.delete(&secret.new_key);
                } else {
                    report
                        .warnings
                        .push(format!("failed to restore secret for {}", secret.new_key));
                }
            }
        }
    }
    let _ = std::fs::remove_file(journal_path);
    // 标记 backup 已回滚。
    let mut updated = meta.clone();
    updated.rolled_back = true;
    let _ = write_meta(&backup_dir.join("backup.json"), &updated);
    Ok(report)
}

// ---------------------------------------------------------------------------
// 报告辅助 / Report helpers
// ---------------------------------------------------------------------------
fn report_from_plan(
    plan: &MarketplaceMigrationPlan,
    dry_run: bool,
    backup: Option<BackupRef>,
) -> MarketplaceMigrationReport {
    MarketplaceMigrationReport {
        command: "marketplace migrate".to_string(),
        dry_run,
        written: false,
        items: plan.items.clone(),
        conflicts: plan.conflicts.clone(),
        manual: plan.manual.clone(),
        backup,
        warnings: Vec::new(),
        corrupt: plan.corrupt.clone(),
        status: MigrationStatus::Ok,
    }
    .finalized()
}

impl MarketplaceMigrationReport {
    fn finalized(mut self) -> Self {
        self.status = self.status();
        self
    }
}

fn display_source(source: &Value) -> String {
    source
        .get("url")
        .and_then(Value::as_str)
        .map(crate::settings::redaction::known_git_source_for_display)
        .unwrap_or_else(|| "<unknown>".to_string())
}

// ===========================================================================
// 通用辅助 / Generic helpers
// ===========================================================================
fn with_migration_lock<T>(
    home: &Path,
    work: impl FnOnce() -> Result<T, MarketplaceMigrationError>,
) -> Result<T, MarketplaceMigrationError> {
    let lock_path = home.join(MIGRATION_LOCK_FILENAME);
    match with_file_lock(
        &lock_path,
        0,
        DEFAULT_LOCK_BACKOFF_BASE,
        DEFAULT_LOCK_BACKOFF_MAX,
        work,
    ) {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(e),
        Err(SettingsStoreError::Lock { path, .. }) => {
            Err(MarketplaceMigrationError::LockBusy { path })
        }
        Err(SettingsStoreError::Io { path, source }) => {
            Err(MarketplaceMigrationError::Io { path, source })
        }
    }
}

fn io_err(path: &Path, source: io::Error) -> MarketplaceMigrationError {
    MarketplaceMigrationError::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn is_safe_name_segment(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.starts_with('.')
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
}

fn is_safe_timestamp(ts: &str) -> bool {
    !ts.is_empty() && ts.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn unique_backup_timestamp(home: &Path) -> String {
    let base = chrono::Utc::now().format("%Y%m%dT%H%M%S%6fZ").to_string();
    let mut candidate = base.clone();
    let mut n = 0u32;
    while home
        .join(format!("{MIGRATION_BACKUP_PREFIX}{candidate}"))
        .exists()
    {
        n += 1;
        candidate = format!("{base}-{n}");
    }
    candidate
}

fn safe_dir_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push_str("item");
    }
    out
}

fn set_private_dir(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
    }
    let _ = path;
}

/// 把 `installPath` 从旧 marketplace 根改写为新根（组件边界匹配）。
///
/// 仅接受两条已知根：`<home>/marketplace/<old>` 与 `<home>/marketplace/.plugins/<old>`；
/// 其余路径返回 `Err(())`（调用方按「不部分改写」整体 unresolved）。
pub(crate) fn rewrite_install_path(
    path: &str,
    home: &Path,
    old: &str,
    new: &str,
) -> Result<String, ()> {
    let p = Path::new(path);
    let catalog_old = marketplace_skill_dir(home, old, &[]);
    let catalog_new = marketplace_skill_dir(home, new, &[]);
    if let Ok(rest) = p.strip_prefix(&catalog_old) {
        return Ok(catalog_new.join(rest).to_string_lossy().into_owned());
    }
    let ext_old = home
        .join(SOURCE_MARKETPLACE)
        .join(EXTERNAL_PLUGINS_NS)
        .join(old);
    let ext_new = home
        .join(SOURCE_MARKETPLACE)
        .join(EXTERNAL_PLUGINS_NS)
        .join(new);
    if let Ok(rest) = p.strip_prefix(&ext_old) {
        return Ok(ext_new.join(rest).to_string_lossy().into_owned());
    }
    Err(())
}

// ===========================================================================
// 单元测试 / Unit tests
// ===========================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::reconciler::InstalledPluginRecord;
    use crate::settings::reconciler::KnownMarketplaceEntry;
    use crate::settings::store::{update_installed_plugins, update_known_marketplaces};
    use serde_json::json;
    use tempfile::TempDir;

    fn unit_env(td: &TempDir) -> EnvMap {
        std::iter::once((
            "XDG_CONFIG_HOME".to_string(),
            td.path().join("xdg").to_string_lossy().into_owned(),
        ))
        .collect()
    }

    fn seed_unit(home: &Path, old: &str, manifest_name: &str) {
        let catalog = marketplace_skill_dir(home, old, &[]);
        std::fs::create_dir_all(catalog.join(".tfrobot-plugin")).unwrap();
        std::fs::write(
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
            Some(home),
            None,
        )
        .unwrap();
    }

    #[test]
    fn baseline_only_writes_baseline_without_rename() {
        let td = TempDir::new().unwrap();
        let home = td.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        seed_unit(&home, "same-name", "same-name");
        let env = unit_env(&td);
        let secrets = SecretStore::new();
        let ctx = load_context(
            &home,
            Some(&env),
            None,
            MarketplaceMigrationOptions::default(),
            &secrets,
        )
        .unwrap();
        assert_eq!(ctx.plan.items[0].action, MigrationAction::BaselineOnly);
        apply_fresh(&ctx, Some(&env), &secrets, None).unwrap();
        let known = load_known_marketplaces_strict(Some(&home), Some(&env)).unwrap();
        assert_eq!(
            known
                .account
                .marketplaces
                .get("same-name")
                .unwrap()
                .manifest_name_baseline
                .as_deref(),
            Some("same-name")
        );
        assert!(marketplace_skill_dir(&home, "same-name", &[]).is_dir());
    }

    #[test]
    fn journal_present_blocks_fresh_apply() {
        let td = TempDir::new().unwrap();
        let home = td.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        seed_unit(&home, "old-name", "team-tools");
        std::fs::write(home.join(MIGRATION_JOURNAL_FILENAME), "{}").unwrap();
        let env = unit_env(&td);
        let secrets = SecretStore::new();
        let err = apply_marketplace_name_migration(
            &home,
            Some(&env),
            None,
            MarketplaceMigrationOptions::default(),
            &secrets,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            MarketplaceMigrationError::JournalInProgress { .. }
        ));
    }

    #[test]
    fn aborted_run_can_resume_to_completion() {
        let td = TempDir::new().unwrap();
        let home = td.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        seed_unit(&home, "old-name", "team-tools");
        let env = unit_env(&td);
        let secrets = SecretStore::new();
        let ctx = load_context(
            &home,
            Some(&env),
            None,
            MarketplaceMigrationOptions::default(),
            &secrets,
        )
        .unwrap();
        // 模拟在 rename 后失败：当前项按 before-image 回滚，journal 留在 aborted。
        let report = apply_fresh(&ctx, Some(&env), &secrets, Some("after-rename")).unwrap();
        assert_eq!(report.status(), MigrationStatus::Partial);
        assert!(home.join(MIGRATION_JOURNAL_FILENAME).exists());
        assert!(marketplace_skill_dir(&home, "old-name", &[]).is_dir());

        let resumed = resume_marketplace_name_migration(&home, Some(&env), None, &secrets).unwrap();
        assert_eq!(resumed.status(), MigrationStatus::Ok);
        assert!(!home.join(MIGRATION_JOURNAL_FILENAME).exists());
        assert!(marketplace_skill_dir(&home, "team-tools", &[]).is_dir());
        let known = load_known_marketplaces_strict(Some(&home), Some(&env)).unwrap();
        assert!(known.account.marketplaces.contains_key("team-tools"));
        assert!(!known.account.marketplaces.contains_key("old-name"));
    }

    #[test]
    fn interrupted_apply_can_roll_back() {
        let td = TempDir::new().unwrap();
        let home = td.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        seed_unit(&home, "old-name", "team-tools");
        let env = unit_env(&td);
        let secrets = SecretStore::new();
        let ctx = load_context(
            &home,
            Some(&env),
            None,
            MarketplaceMigrationOptions::default(),
            &secrets,
        )
        .unwrap();
        let item = ctx.plan.items[0].clone();
        // 先走一次可回滚的失败，拿到备份时间戳；再把状态改造成「硬崩溃中途」。
        let report = apply_fresh(&ctx, Some(&env), &secrets, Some("after-known")).unwrap();
        let timestamp = report.backup.as_ref().unwrap().timestamp.clone();
        // 模拟崩溃窗口：catalog 已改名、known 已写，journal 仍是 applying。
        std::fs::rename(
            marketplace_skill_dir(&home, "old-name", &[]),
            marketplace_skill_dir(&home, "team-tools", &[]),
        )
        .unwrap();
        write_known_item(&home, Some(&env), &item).unwrap();
        let journal_path = home.join(MIGRATION_JOURNAL_FILENAME);
        let mut meta = read_journal(&journal_path).unwrap();
        meta.phase = "applying".to_string();
        write_journal(&home, &meta).unwrap();

        // 中断态 rollback 不应再被 journal 相位门控拒绝。
        let rb =
            rollback_marketplace_name_migration(&home, Some(&env), &timestamp, &secrets).unwrap();
        assert!(rb.notes.iter().any(|w| w.contains("interrupted")));
        assert!(
            rb.warnings.is_empty(),
            "informational note must not be a warning"
        );
        assert!(marketplace_skill_dir(&home, "old-name", &[]).is_dir());
        assert!(!marketplace_skill_dir(&home, "team-tools", &[]).exists());
        let known = load_known_marketplaces_strict(Some(&home), Some(&env)).unwrap();
        assert!(known.account.marketplaces.contains_key("old-name"));
        assert!(!known.account.marketplaces.contains_key("team-tools"));
        assert!(!journal_path.exists());
    }

    #[test]
    fn seed_crash_window_can_resume() {
        let td = TempDir::new().unwrap();
        let home = td.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        seed_unit(&home, "old-name", "team-tools");
        let env = unit_env(&td);
        let secrets = SecretStore::new();
        let ctx = load_context(
            &home,
            Some(&env),
            None,
            MarketplaceMigrationOptions::default(),
            &secrets,
        )
        .unwrap();
        // seed 已写、journal 仍是 prepared（模拟 kill 在 post-hash 记录前）。
        let err = apply_fresh(&ctx, Some(&env), &secrets, Some("after-seed")).unwrap_err();
        assert!(matches!(
            err,
            MarketplaceMigrationError::ConcurrentChange { .. }
        ));
        assert!(home.join(MIGRATION_JOURNAL_FILENAME).exists());
        let resumed = resume_marketplace_name_migration(&home, Some(&env), None, &secrets).unwrap();
        assert_eq!(resumed.status(), MigrationStatus::Ok);
        assert!(!home.join(MIGRATION_JOURNAL_FILENAME).exists());
        assert!(marketplace_skill_dir(&home, "team-tools", &[]).is_dir());
    }

    #[test]
    fn poison_journal_is_cleaned_when_pre_hash_mismatch() {
        let td = TempDir::new().unwrap();
        let home = td.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        seed_unit(&home, "old-name", "team-tools");
        let env = unit_env(&td);
        let secrets = SecretStore::new();
        let ctx = load_context(
            &home,
            Some(&env),
            None,
            MarketplaceMigrationOptions::default(),
            &secrets,
        )
        .unwrap();
        let err = apply_fresh(&ctx, Some(&env), &secrets, Some("mutate-after-backup")).unwrap_err();
        assert!(matches!(
            err,
            MarketplaceMigrationError::ConcurrentChange { .. }
        ));
        // 毒 journal 被清掉，后续可重新 apply（备份保留）。
        assert!(!home.join(MIGRATION_JOURNAL_FILENAME).exists());
    }

    #[test]
    fn rollback_of_old_backup_is_rejected_while_another_journal_is_in_progress() {
        let td = TempDir::new().unwrap();
        let home = td.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        seed_unit(&home, "old-a", "team-a");
        let env = unit_env(&td);
        let secrets = SecretStore::new();
        let first = apply_fresh(
            &load_context(
                &home,
                Some(&env),
                None,
                MarketplaceMigrationOptions::default(),
                &secrets,
            )
            .unwrap(),
            Some(&env),
            &secrets,
            None,
        )
        .unwrap();
        let first_ts = first.backup.unwrap().timestamp;
        // 第二个迁移中途失败，留下 applying journal + 自己的 backup。
        seed_unit(&home, "old-b", "team-b");
        let ctx2 = load_context(
            &home,
            Some(&env),
            None,
            MarketplaceMigrationOptions::default(),
            &secrets,
        )
        .unwrap();
        let second = apply_fresh(&ctx2, Some(&env), &secrets, Some("after-known")).unwrap();
        let second_ts = second.backup.unwrap().timestamp;
        assert_ne!(first_ts, second_ts);
        let journal_path = home.join(MIGRATION_JOURNAL_FILENAME);
        let mut meta = read_journal(&journal_path).unwrap();
        meta.phase = "applying".to_string();
        write_journal(&home, &meta).unwrap();
        // 用旧 backup 回滚必须被拒绝（否则会覆盖在途迁移并删 journal）。
        let err = rollback_marketplace_name_migration(&home, Some(&env), &first_ts, &secrets)
            .unwrap_err();
        assert!(matches!(
            err,
            MarketplaceMigrationError::JournalInProgress { .. }
        ));
        assert!(journal_path.exists());
    }

    #[test]
    fn poison_journal_is_cleaned_when_authority_file_appears() {
        let td = TempDir::new().unwrap();
        let home = td.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        seed_unit(&home, "old-name", "team-tools");
        let env = unit_env(&td);
        let secrets = SecretStore::new();
        let ctx = load_context(
            &home,
            Some(&env),
            None,
            MarketplaceMigrationOptions::default(),
            &secrets,
        )
        .unwrap();
        let err = apply_fresh(
            &ctx,
            Some(&env),
            &secrets,
            Some("create-settings-after-backup"),
        )
        .unwrap_err();
        assert!(
            matches!(err, MarketplaceMigrationError::ConcurrentChange { .. }),
            "unexpected error: {err:?}"
        );
        assert!(!home.join(MIGRATION_JOURNAL_FILENAME).exists());
    }

    #[test]
    fn resume_rejects_tampered_done_item() {
        let td = TempDir::new().unwrap();
        let home = td.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        seed_unit(&home, "old-name", "team-tools");
        let env = unit_env(&td);
        let secrets = SecretStore::new();
        let report = apply_marketplace_name_migration(
            &home,
            Some(&env),
            None,
            MarketplaceMigrationOptions::default(),
            &secrets,
        )
        .unwrap();
        let backup_dir = PathBuf::from(&report.backup.unwrap().path);
        // 篡改 done 项（删除新 catalog 目录）后重建 journal；post-hash 同步，确保 hash 门放行，
        // 只剩「最终 verify done 项」能拦截。
        std::fs::remove_dir_all(marketplace_skill_dir(&home, "team-tools", &[])).unwrap();
        let mut meta: MigrationMeta =
            serde_json::from_str(&std::fs::read_to_string(backup_dir.join("backup.json")).unwrap())
                .unwrap();
        meta.phase = "aborted".to_string();
        meta.items[0].status = "done".to_string();
        meta.post_hashes =
            current_hashes(&home, Some(&env), &user_settings_path(Some(&env))).unwrap();
        write_journal(&home, &meta).unwrap();
        let resumed = resume_marketplace_name_migration(&home, Some(&env), None, &secrets).unwrap();
        assert_eq!(resumed.status(), MigrationStatus::Partial);
        assert!(resumed
            .items
            .iter()
            .any(|i| i.action == MigrationAction::Blocked));
        assert!(home.join(MIGRATION_JOURNAL_FILENAME).exists());
    }

    fn seed_unit_with_ledger(home: &Path, old: &str, manifest: &str, plugin: &str) {
        seed_unit(home, old, manifest);
        let pid = format!("{plugin}@{old}");
        let install = marketplace_skill_dir(home, old, &[])
            .join("plugins")
            .join(plugin);
        update_installed_plugins(
            |file| {
                let mut extra = Map::new();
                extra.insert("scope".to_string(), Value::String("user".to_string()));
                file.account.plugins.insert(
                    pid,
                    vec![InstalledPluginRecord {
                        install_path: Some(install.to_string_lossy().into_owned()),
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

    #[test]
    fn half_seed_crash_resumes_and_backfills_enabled() {
        let td = TempDir::new().unwrap();
        let home = td.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        seed_unit_with_ledger(&home, "old-name", "team-tools", "alpha");
        let env = unit_env(&td);
        let secrets = SecretStore::new();
        let ctx = load_context(
            &home,
            Some(&env),
            None,
            MarketplaceMigrationOptions::default(),
            &secrets,
        )
        .unwrap();
        // settings 已回填、intent 尚未写（模拟两次 seed 写之间被 kill）。
        let err = apply_fresh(&ctx, Some(&env), &secrets, Some("after-seed-settings")).unwrap_err();
        assert!(matches!(
            err,
            MarketplaceMigrationError::ConcurrentChange { .. }
        ));
        assert!(home.join(MIGRATION_JOURNAL_FILENAME).exists());
        assert!(!installed_plugins_intent_path(Some(&home), Some(&env)).exists());
        let resumed = resume_marketplace_name_migration(&home, Some(&env), None, &secrets).unwrap();
        assert_eq!(resumed.status(), MigrationStatus::Ok, "{resumed:?}");
        let user = raw_write::read_strict_map(&user_settings_path(Some(&env))).unwrap();
        assert_eq!(user["enabledPlugins"]["alpha@team-tools"], true);
    }

    #[test]
    fn seed_window_rejects_foreign_intent() {
        let td = TempDir::new().unwrap();
        let home = td.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        seed_unit_with_ledger(&home, "old-name", "team-tools", "alpha");
        let env = unit_env(&td);
        let secrets = SecretStore::new();
        let ctx = load_context(
            &home,
            Some(&env),
            None,
            MarketplaceMigrationOptions::default(),
            &secrets,
        )
        .unwrap();
        let _ = apply_fresh(&ctx, Some(&env), &secrets, Some("after-seed-settings")).unwrap_err();
        // 外部把 dangling pid 写进 intent，不应被 seed 窗口放行。
        crate::settings::store::update_installed_plugins_intent(
            |file| {
                file.account
                    .installed_plugins
                    .insert("alpha@old-name".to_string());
                file.account
                    .installed_plugins
                    .insert("beta@old-name".to_string());
            },
            Some(&home),
            Some(&env),
        )
        .unwrap();
        let err = resume_marketplace_name_migration(&home, Some(&env), None, &secrets).unwrap_err();
        assert!(matches!(
            err,
            MarketplaceMigrationError::ConcurrentChange { .. }
        ));
        assert!(home.join(MIGRATION_JOURNAL_FILENAME).exists());
    }

    #[test]
    fn resume_backfills_enabled_when_settings_write_was_lost() {
        let td = TempDir::new().unwrap();
        let home = td.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        seed_unit_with_ledger(&home, "old-name", "team-tools", "alpha");
        let env = unit_env(&td);
        let secrets = SecretStore::new();
        let report = apply_marketplace_name_migration(
            &home,
            Some(&env),
            None,
            MarketplaceMigrationOptions::default(),
            &secrets,
        )
        .unwrap();
        let backup_dir = PathBuf::from(&report.backup.unwrap().path);
        // 模拟「intent 已写、settings 回填丢失」：删掉 enabled 键后重建 journal。
        let user_path = user_settings_path(Some(&env));
        raw_write::update_raw_settings(&user_path, false, |map| {
            let enabled = raw_write::object_field_mut(map, FIELD_ENABLED_PLUGINS, &user_path)?;
            enabled.remove("alpha@team-tools");
            Ok(())
        })
        .unwrap();
        let mut meta: MigrationMeta =
            serde_json::from_str(&std::fs::read_to_string(backup_dir.join("backup.json")).unwrap())
                .unwrap();
        meta.phase = "aborted".to_string();
        meta.post_hashes = current_hashes(&home, Some(&env), &user_path).unwrap();
        write_journal(&home, &meta).unwrap();
        let resumed = resume_marketplace_name_migration(&home, Some(&env), None, &secrets).unwrap();
        assert_eq!(resumed.status(), MigrationStatus::Ok, "{resumed:?}");
        let user = raw_write::read_strict_map(&user_path).unwrap();
        assert_eq!(user["enabledPlugins"]["alpha@team-tools"], true);
    }

    #[test]
    fn pre_v03_ledger_local_only_id_survives_first_intent_write() {
        let td = TempDir::new().unwrap();
        let home = td.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        seed_unit(&home, "old-name", "team-tools");
        update_installed_plugins(
            |file| {
                file.account.plugins.insert(
                    "local-only".to_string(),
                    vec![InstalledPluginRecord::default()],
                );
            },
            Some(&home),
            None,
        )
        .unwrap();
        let env = unit_env(&td);
        let secrets = SecretStore::new();
        apply_marketplace_name_migration(
            &home,
            Some(&env),
            None,
            MarketplaceMigrationOptions::default(),
            &secrets,
        )
        .unwrap();
        let intent = load_installed_plugins_intent_strict(Some(&home), Some(&env)).unwrap();
        assert!(
            intent.account.installed_plugins.contains("local-only"),
            "pre-v0.3 ledger local-only id lost: {:?}",
            intent.account.installed_plugins
        );
    }

    #[test]
    fn rollback_rejects_drifted_state() {
        let td = TempDir::new().unwrap();
        let home = td.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        seed_unit(&home, "old-name", "team-tools");
        let env = unit_env(&td);
        let secrets = SecretStore::new();
        let report = apply_marketplace_name_migration(
            &home,
            Some(&env),
            None,
            MarketplaceMigrationOptions::default(),
            &secrets,
        )
        .unwrap();
        let ts = report.backup.unwrap().timestamp;
        // 迁移后用户又改了 settings.json → 回滚必须拒绝覆盖。
        let settings = user_settings_path(Some(&env));
        std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
        std::fs::write(&settings, "{\"userEdit\":true}").unwrap();
        let err =
            rollback_marketplace_name_migration(&home, Some(&env), &ts, &secrets).unwrap_err();
        assert!(matches!(
            err,
            MarketplaceMigrationError::RollbackDrift { .. }
        ));
    }

    #[test]
    fn safe_name_segment_rejects_traversal_and_reserved() {
        assert!(is_safe_name_segment("team-tools"));
        assert!(!is_safe_name_segment(""));
        assert!(!is_safe_name_segment("."));
        assert!(!is_safe_name_segment(".."));
        assert!(!is_safe_name_segment(".plugins"));
        assert!(!is_safe_name_segment("a/b"));
        assert!(!is_safe_name_segment("a\\b"));
    }

    #[test]
    fn rewrite_install_path_uses_component_boundaries() {
        let home = Path::new("/h");
        assert_eq!(
            rewrite_install_path("/h/marketplace/foo/plugins/a", home, "foo", "bar").unwrap(),
            "/h/marketplace/bar/plugins/a"
        );
        assert_eq!(
            rewrite_install_path("/h/marketplace/.plugins/foo/a/sub", home, "foo", "bar").unwrap(),
            "/h/marketplace/.plugins/bar/a/sub"
        );
        // `foobar` 不得匹配 `foo` 根。
        assert!(rewrite_install_path("/h/marketplace/foobar/a", home, "foo", "bar").is_err());
        assert!(rewrite_install_path("/elsewhere/a", home, "foo", "bar").is_err());
    }

    #[test]
    fn safe_timestamp_rejects_traversal() {
        assert!(is_safe_timestamp("20261122T000000123456Z"));
        assert!(!is_safe_timestamp("../x"));
        assert!(!is_safe_timestamp("a/b"));
    }
}
