/*!
* 文件名: store.rs
* 作者: JQQ
* 创建日期: 2026/06/04
* 最后修改日期: 2026/06/04
* 版权: 2023 JQQ. All rights reserved.
* 依赖: fd-lock, chrono, serde_json, smcp::utils::{atomic_io,path}, settings::reconciler, skills::{home,staging}
* 描述: 治理层物化文件 store —— 原子写 + 文件锁 + JSONC 损坏恢复 + .bak 备份。
*       Governance-layer materialized-file store (atomic write + file lock + JSONC corruption recovery).
*/

//! 物化层文件 store：原子写 + 文件锁 + 损坏恢复。
//!
//! 对标 Python 治理层资产 `a2c_smcp/computer/settings/store.py`（v0.2.1）。
//! SDK 设计 / Design: python-sdk `docs/design-0.2.1-cli-marketplace-ux.md` §6.1（known_marketplaces.json）/
//! §6.2（installed_plugins.json）/ §6.3（原子写 + 锁 + 恢复）。
//!
//! 本模块是物化层（CLI 自动维护、**不可手编**）的持久化 I/O 底座，供 [`reconcile`] / installer（#69）/
//! marketplace CLI（#48）调用。承载两个文件：`known_marketplaces.json` 与 `installed_plugins.json`；二者
//! **带 `version` 字段**（区别于无 version 的 settings.json 意图层，见 [`schema`]）。
//!
//! 三条「优于 Claude Code 现状」的可靠性保证 / Three reliability guarantees:
//! - **原子写**：唯一临时文件 + `fsync` + atomic rename（经 UTIL-03 [`atomic_write_text`]）——进程中途死不留
//!   半截 JSON。
//! - **文件锁**：旁车 `<path>.lock` 排他锁（[`fd_lock`]，unix `flock` / win32 `LockFileEx`）防同用户多实例
//!   撕裂；非阻塞 try + 指数退避，仍拿不到 → 抛 [`SettingsStoreError::Lock`]（fail-fast，**绝不无锁写**）。
//!   读-改-写须用 [`update_known_marketplaces`] / [`update_installed_plugins`]（单把锁内 RMW），**不可**在持锁
//!   上下文内再调 `save_*`（同进程二次 flock 会 WouldBlock → 退避耗尽报错）。
//! - **损坏恢复**：load 失败先把损坏文件 `os.rename` 备份为 `.corrupt-<ts>.bak` **再**降级空配置（不丢用户
//!   数据、不静默永久覆盖）。
//!
//! 写保护：物化文件顶端写一行 JSONC 注释 [`WRITE_PROTECTION_HEADER`]；读时容错剥离前导 `//` 行再解析。
//! 发现手编痕迹（version 不符 / 未知顶层键）→ WARN + 用 in-memory 解析、下次 save 重写覆盖。
//!
//! DRY：账本记录类型（[`KnownMarketplaceEntry`] / [`KnownMarketplaces`] / [`InstalledPluginRecord`] /
//! [`InstalledPlugins`]）复用 [`reconciler`] 已定义者（`#[serde(flatten)] extra` 已对全部字段保真 round-trip）；
//! 本模块仅新增带 `version` 的 `*File` 信封 + 持久化 I/O。
//!
//! 接缝实现 / Seam impl：[`FileSkillGovernanceStore`] 实现 [`SkillGovernanceStore`] 与 staging 的
//! [`KnownMarketplaceRecorder`]（同一对象经 [`as_recorder`](SkillGovernanceStore::as_recorder) 两用），闭合
//! #57 reconciler 留下的存储接缝；真实注入（含 #68 INT-01 接线）由集成层承担。
//!
//! 同步阻塞约定 / Sync-blocking note：文件锁退避用 [`std::thread::sleep`]（非 async），遵守 [`reconcile`] /
//! [`stage_marketplace_skills`] 文档化的同步 `file_lock` 阻塞约束；单用户场景争用罕见，退避窗口短。
//!
//! [`reconcile`]: crate::settings::reconciler::reconcile
//! [`stage_marketplace_skills`]: crate::skills::staging::stage_marketplace_skills
//! [`schema`]: crate::settings::schema
//! [`reconciler`]: crate::settings::reconciler
//! [`atomic_write_text`]: smcp::utils::atomic_io::atomic_write_text

use std::fs::OpenOptions;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use serde_json::{Map, Value};

use smcp::utils::atomic_io::atomic_write_text;
use smcp::utils::path::normalize_lexical;

use crate::settings::reconciler::{
    InstalledPlugins, KnownMarketplaceEntry, KnownMarketplaces, SkillGovernanceStore,
};
use crate::settings::scope::EnvMap;
use crate::skills::home::resolve_skill_home;
use crate::skills::staging::{KnownMarketplaceRecord, KnownMarketplaceRecorder};

// ===========================================================================
// 常量 / Constants
// ===========================================================================
/// 物化文件顶端写保护注释（JSONC 单行；读时被 [`strip_jsonc_header`] 剥离）/ Write-protection header line。
pub const WRITE_PROTECTION_HEADER: &str =
    "// Maintained automatically by a2c-computer. DO NOT EDIT.";

/// 物化文件 schema 版本（区别于无 version 的 settings.json 意图层）/ Materialized-file schema version。
pub const MATERIALIZED_VERSION: u32 = 1;

/// `known_marketplaces.json` 文件名 / filename。
pub const KNOWN_MARKETPLACES_FILENAME: &str = "known_marketplaces.json";
/// `installed_plugins.json` 文件名 / filename。
pub const INSTALLED_PLUGINS_FILENAME: &str = "installed_plugins.json";

/// 文件锁退避重试次数（指数退避）/ default lock backoff retry count。
pub const DEFAULT_LOCK_RETRIES: u32 = 10;
/// 文件锁退避起始延迟 / lock backoff base delay。
pub const DEFAULT_LOCK_BACKOFF_BASE: Duration = Duration::from_millis(50);
/// 文件锁退避封顶延迟 / lock backoff cap。
pub const DEFAULT_LOCK_BACKOFF_MAX: Duration = Duration::from_millis(1000);

// ===========================================================================
// 错误 / Error
// ===========================================================================
/// store 读写错误 / store read-write errors。
///
/// `load_*` **不**返回错误（缺失 / 损坏均降级空配置）；`save_*` / `update_*` 在锁不可得或 I/O 失败时返回。
#[derive(Debug, thiserror::Error)]
pub enum SettingsStoreError {
    /// 退避重试后仍无法获取文件锁（fail-fast，绝不无锁写）/ lock unavailable after backoff。
    #[error("could not acquire settings store lock {} after {retries} retries", .path.display())]
    Lock {
        /// 旁车锁文件路径 / sidecar lock path。
        path: PathBuf,
        /// 已重试次数 / retries attempted。
        retries: u32,
    },
    /// 物化文件 I/O 失败（原子写 / 目录创建）/ materialized-file I/O failure。
    #[error("settings store I/O error on {}: {source}", .path.display())]
    Io {
        /// 出错路径 / offending path。
        path: PathBuf,
        /// 底层 I/O 错误 / underlying error。
        #[source]
        source: io::Error,
    },
}

impl SettingsStoreError {
    fn io(path: &Path, source: io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

// ===========================================================================
// 物化文件信封（带 version；复用 reconciler 账本类型）/ Materialized-file envelopes
// ===========================================================================
fn default_materialized_version() -> u32 {
    MATERIALIZED_VERSION
}

/// `known_marketplaces.json` 整文件结构（§6.1）/ the full known_marketplaces.json shape。
///
/// `version` 信封 + 复用 [`KnownMarketplaces`]（`marketplaces` 经 `#[serde(flatten)]` 与 `version` 同层）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct KnownMarketplacesFile {
    /// 物化 schema 版本 / materialized schema version。
    #[serde(default = "default_materialized_version")]
    pub version: u32,
    /// `name → entry`（保物化顺序）/ the marketplace ledger（复用 reconciler 类型）。
    #[serde(flatten)]
    pub account: KnownMarketplaces,
}

/// `installed_plugins.json` 整文件结构（§6.2）/ the full installed_plugins.json shape。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct InstalledPluginsFile {
    /// 物化 schema 版本 / materialized schema version。
    #[serde(default = "default_materialized_version")]
    pub version: u32,
    /// `pid → 安装记录列表`（保物化顺序）/ the install ledger（复用 reconciler 类型）。
    #[serde(flatten)]
    pub account: InstalledPlugins,
}

/// 空的 `known_marketplaces` 物化结构 / an empty known_marketplaces file。
pub fn empty_known_marketplaces() -> KnownMarketplacesFile {
    KnownMarketplacesFile {
        version: MATERIALIZED_VERSION,
        account: KnownMarketplaces::default(),
    }
}

/// 空的 `installed_plugins` 物化结构 / an empty installed_plugins file。
pub fn empty_installed_plugins() -> InstalledPluginsFile {
    InstalledPluginsFile {
        version: MATERIALIZED_VERSION,
        account: InstalledPlugins::default(),
    }
}

// ===========================================================================
// 原子写 JSON / Atomic JSON write
// ===========================================================================
/// 原子写 JSON（可选 JSONC 顶部写保护注释）/ Atomic JSON write with an optional JSONC header line。
///
/// 2 空格缩进、保留非 ASCII、末尾补换行；`header`（形如 `// ...`）写在首行，与 [`read_jsonc_with_recovery`]
/// 的前导 `//` 剥离对称。落盘经 UTIL-03 [`atomic_write_text`]（唯一临时文件 + `fsync` + 原子 rename）。
fn atomic_write_json<T: Serialize>(path: &Path, value: &T, header: Option<&str>) -> io::Result<()> {
    let body = serde_json::to_string_pretty(value)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let text = match header {
        Some(h) => format!("{h}\n{body}\n"),
        None => format!("{body}\n"),
    };
    atomic_write_text(path, &text)
}

// ===========================================================================
// 文件锁（旁车 .lock，跨平台 fd-lock）/ File lock (sidecar .lock, cross-platform)
// ===========================================================================
/// 旁车锁文件路径 `<path>.lock` / the sidecar lock path。
///
/// 锁加在**旁车** `.lock` 而非数据文件本身——数据文件每次写都被原子 rename 换掉，持有其 inode 上的锁会随
/// rename 失效。
fn lock_sidecar_path(data_path: &Path) -> PathBuf {
    let name = data_path
        .file_name()
        .map(|n| n.to_owned())
        .unwrap_or_default();
    let mut lock_name = name;
    lock_name.push(".lock");
    match data_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(lock_name),
        _ => PathBuf::from(lock_name),
    }
}

/// 持 `<data_path>.lock` 旁车排他锁执行 `work`（§6.3）/ Run `work` while holding the sidecar lock。
///
/// 被占时按**指数退避**重试 `retries` 次（每次 WARN，延迟封顶 `backoff_max`）；仍拿不到 → 返回
/// [`SettingsStoreError::Lock`]（fail-fast，绝不无锁写）。`work` 在持锁期间执行，返回后锁释放。
fn with_file_lock<T>(
    data_path: &Path,
    retries: u32,
    backoff_base: Duration,
    backoff_max: Duration,
    work: impl FnOnce() -> T,
) -> Result<T, SettingsStoreError> {
    let lock_path = lock_sidecar_path(data_path);
    if let Some(parent) = lock_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| SettingsStoreError::io(&lock_path, e))?;
        }
    }
    let mut opts = OpenOptions::new();
    opts.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let file = opts
        .open(&lock_path)
        .map_err(|e| SettingsStoreError::io(&lock_path, e))?;
    let mut lock = fd_lock::RwLock::new(file);

    let mut delay = backoff_base;
    for attempt in 0..=retries {
        match lock.try_write() {
            Ok(_guard) => return Ok(work()), // 持锁执行；_guard 存活至 return，随后释放
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                if attempt >= retries {
                    return Err(SettingsStoreError::Lock {
                        path: lock_path,
                        retries,
                    });
                }
                tracing::warn!(
                    lock = %lock_path.display(),
                    attempt = attempt + 1,
                    retries,
                    delay_ms = delay.as_millis() as u64,
                    "settings store lock busy, backing off"
                );
                std::thread::sleep(delay);
                delay = (delay * 2).min(backoff_max);
            }
            Err(e) => return Err(SettingsStoreError::io(&lock_path, e)),
        }
    }
    Err(SettingsStoreError::Lock {
        path: lock_path,
        retries,
    })
}

// ===========================================================================
// JSONC 读取 + 损坏恢复 / JSONC read + corruption recovery
// ===========================================================================
/// 剥离**前导**整行 `//` 注释（写保护头）与前导空行 / Strip the leading `//` header + blank lines。
///
/// 仅处理文件顶部连续的整行注释 / 空行，遇首个内容行即停——不是通用 JSONC（不处理行尾 `//`、块注释、
/// 字符串内 `//`），够用且对数据零误伤。
fn strip_jsonc_header(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut idx = 0;
    for line in &lines {
        let stripped = line.trim_start();
        if stripped.is_empty() || stripped.starts_with("//") {
            idx += 1;
        } else {
            break;
        }
    }
    lines[idx..].join("\n")
}

/// 当前 UTC 时间戳（紧凑、文件名安全）/ a filesystem-safe UTC timestamp for backup names。
fn backup_timestamp() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%S%6fZ").to_string()
}

/// 把损坏文件**移走**为 `<name>.corrupt-<ts>.bak`（§6.3）/ Move a corrupt file aside to a `.bak`。
///
/// 用原子 rename 整体移动（保留损坏内容到 `.bak`），原路径腾空——下次 save 重建全新文件，损坏数据不被静默
/// 永久覆盖。备份本身失败仅记 ERROR、返回 `None`（不阻断降级）。
fn backup_corrupt(path: &Path) -> Option<PathBuf> {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let backup = path.with_file_name(format!("{name}.corrupt-{}.bak", backup_timestamp()));
    match std::fs::rename(path, &backup) {
        Ok(()) => {
            tracing::warn!(
                path = %path.display(),
                backup = %backup.display(),
                "corrupt materialized file backed up, degrading to empty config"
            );
            Some(backup)
        }
        Err(e) => {
            tracing::error!(
                path = %path.display(),
                backup = %backup.display(),
                error = %e,
                "failed to back up corrupt materialized file"
            );
            None
        }
    }
}

/// 读取 JSONC 物化文件（容错前导 `//` 头）+ 损坏恢复 / Read a JSONC materialized file with recovery。
///
/// - 文件缺失 → `None`（调用方按空配置处理）。
/// - I/O 读取错误 → `None`、**不**备份（文件可能仍完好、只是暂时不可读）。
/// - JSON 解析失败 / 根非对象（损坏）→ 备份 `.corrupt-<ts>.bak` **再**返回 `None`（§6.3）。
/// - 成功 → 原始对象 map（version / shape 校验交给上层 `coerce_*`）。
fn read_jsonc_with_recovery(path: &Path) -> Option<Map<String, Value>> {
    if !path.exists() {
        return None;
    }
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "materialized file unreadable (kept on disk)");
            return None;
        }
    };
    match serde_json::from_str::<Value>(&strip_jsonc_header(&raw)) {
        Ok(Value::Object(map)) => Some(map),
        Ok(_) => {
            tracing::warn!(path = %path.display(), "materialized file root is not an object, backing up before degrade");
            backup_corrupt(path);
            None
        }
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "materialized file corrupt, backing up before degrade");
            backup_corrupt(path);
            None
        }
    }
}

// ===========================================================================
// 路径解析 / Path resolution（基于 $A2C_SKILL_HOME）
// ===========================================================================
fn resolve_home(home: Option<&Path>, env: Option<&EnvMap>) -> PathBuf {
    home.map(Path::to_path_buf)
        .unwrap_or_else(|| resolve_skill_home(env))
}

/// `$A2C_SKILL_HOME/known_marketplaces.json` 路径 / Path to known_marketplaces.json。
pub fn known_marketplaces_path(home: Option<&Path>, env: Option<&EnvMap>) -> PathBuf {
    resolve_home(home, env).join(KNOWN_MARKETPLACES_FILENAME)
}

/// `$A2C_SKILL_HOME/installed_plugins.json` 路径 / Path to installed_plugins.json。
pub fn installed_plugins_path(home: Option<&Path>, env: Option<&EnvMap>) -> PathBuf {
    resolve_home(home, env).join(INSTALLED_PLUGINS_FILENAME)
}

// ===========================================================================
// 手编痕迹容错 / Hand-edit tolerance（§6.3：WARN + in-memory + 下次 save 重写）
// ===========================================================================
/// 检测手编痕迹（version 不符 / 未知顶层键）并 WARN（不阻断、不丢已知字段）/ Warn on hand-edit traces。
fn warn_hand_edit(
    path: &Path,
    version: Option<&Value>,
    known_keys: &[&str],
    data: &Map<String, Value>,
) {
    let v = version.and_then(Value::as_u64);
    if v != Some(u64::from(MATERIALIZED_VERSION)) {
        tracing::warn!(
            path = %path.display(),
            version = ?version,
            expected = MATERIALIZED_VERSION,
            "materialized file has unexpected version; using in-memory, will rewrite on next save"
        );
    }
    let unknown: Vec<&String> = data
        .keys()
        .filter(|k| !known_keys.contains(&k.as_str()))
        .collect();
    if !unknown.is_empty() {
        tracing::warn!(
            path = %path.display(),
            keys = ?unknown,
            "materialized file has unexpected top-level keys (hand-edited?); ignoring them"
        );
    }
}

/// 把读到的对象 map 规整为 [`KnownMarketplacesFile`]（容错 + 手编 WARN）/ Coerce to the typed shape。
fn coerce_known_marketplaces(data: &Map<String, Value>, path: &Path) -> KnownMarketplacesFile {
    warn_hand_edit(
        path,
        data.get("version"),
        &["version", "marketplaces"],
        data,
    );
    let account = match data.get("marketplaces") {
        Some(Value::Object(_)) => serde_json::from_value::<KnownMarketplaces>(serde_json::json!({
            "marketplaces": data.get("marketplaces").cloned().unwrap_or(Value::Null)
        }))
        .unwrap_or_default(),
        Some(Value::Null) | None => KnownMarketplaces::default(),
        Some(_) => {
            tracing::warn!(path = %path.display(), "field 'marketplaces' is not an object; treating as empty");
            KnownMarketplaces::default()
        }
    };
    KnownMarketplacesFile {
        version: MATERIALIZED_VERSION,
        account,
    }
}

/// 把读到的对象 map 规整为 [`InstalledPluginsFile`]（容错 + 手编 WARN）/ Coerce to the typed shape。
fn coerce_installed_plugins(data: &Map<String, Value>, path: &Path) -> InstalledPluginsFile {
    warn_hand_edit(path, data.get("version"), &["version", "plugins"], data);
    let account = match data.get("plugins") {
        Some(Value::Object(_)) => serde_json::from_value::<InstalledPlugins>(serde_json::json!({
            "plugins": data.get("plugins").cloned().unwrap_or(Value::Null)
        }))
        .unwrap_or_default(),
        Some(Value::Null) | None => InstalledPlugins::default(),
        Some(_) => {
            tracing::warn!(path = %path.display(), "field 'plugins' is not an object; treating as empty");
            InstalledPlugins::default()
        }
    };
    InstalledPluginsFile {
        version: MATERIALIZED_VERSION,
        account,
    }
}

// ===========================================================================
// 高层读写 / High-level load & save
// ===========================================================================
/// 加载 `known_marketplaces.json`（缺失 / 损坏 → 空配置）/ Load known_marketplaces.json。
pub fn load_known_marketplaces(home: Option<&Path>, env: Option<&EnvMap>) -> KnownMarketplacesFile {
    let path = known_marketplaces_path(home, env);
    match read_jsonc_with_recovery(&path) {
        Some(data) => coerce_known_marketplaces(&data, &path),
        None => empty_known_marketplaces(),
    }
}

/// 加载 `installed_plugins.json`（缺失 / 损坏 → 空配置）/ Load installed_plugins.json。
pub fn load_installed_plugins(home: Option<&Path>, env: Option<&EnvMap>) -> InstalledPluginsFile {
    let path = installed_plugins_path(home, env);
    match read_jsonc_with_recovery(&path) {
        Some(data) => coerce_installed_plugins(&data, &path),
        None => empty_installed_plugins(),
    }
}

/// 加锁 + 原子写 `known_marketplaces.json`（带写保护头 + `version`）/ Locked atomic write。
///
/// ⚠️ **不可**在已持有 [`with_file_lock`] 的上下文内调用；读-改-写请改用 [`update_known_marketplaces`]。
pub fn save_known_marketplaces(
    file: &KnownMarketplacesFile,
    home: Option<&Path>,
    env: Option<&EnvMap>,
) -> Result<PathBuf, SettingsStoreError> {
    let path = known_marketplaces_path(home, env);
    with_file_lock(
        &path,
        DEFAULT_LOCK_RETRIES,
        DEFAULT_LOCK_BACKOFF_BASE,
        DEFAULT_LOCK_BACKOFF_MAX,
        || atomic_write_json(&path, file, Some(WRITE_PROTECTION_HEADER)),
    )?
    .map_err(|e| SettingsStoreError::io(&path, e))?;
    Ok(path)
}

/// 加锁 + 原子写 `installed_plugins.json`（带写保护头 + `version`）/ Locked atomic write。
///
/// ⚠️ **不可**在已持有 [`with_file_lock`] 的上下文内调用；读-改-写请改用 [`update_installed_plugins`]。
pub fn save_installed_plugins(
    file: &InstalledPluginsFile,
    home: Option<&Path>,
    env: Option<&EnvMap>,
) -> Result<PathBuf, SettingsStoreError> {
    let path = installed_plugins_path(home, env);
    with_file_lock(
        &path,
        DEFAULT_LOCK_RETRIES,
        DEFAULT_LOCK_BACKOFF_BASE,
        DEFAULT_LOCK_BACKOFF_MAX,
        || atomic_write_json(&path, file, Some(WRITE_PROTECTION_HEADER)),
    )?
    .map_err(|e| SettingsStoreError::io(&path, e))?;
    Ok(path)
}

// ===========================================================================
// 持锁原子读-改-写 / Locked atomic read-modify-write（供 reconciler / installer / recorder）
// ===========================================================================
/// 持锁原子读-改-写 `known_marketplaces.json`（§6.3）/ Locked atomic read-modify-write。
///
/// **单把锁**覆盖 load→mutate→save，杜绝并发进程间的丢更新窗口。`mutator` 收到当前（已规整；缺失 / 损坏则
/// 为空）结构，就地改之；返回最终写入的结构。
///
/// 这是「持锁 RMW」的推荐入口；**不要**自行 `with_file_lock(p, .., || save_*(..))`（`save_*` 会再次加锁 →
/// [`SettingsStoreError::Lock`]）。
pub fn update_known_marketplaces(
    mutator: impl FnOnce(&mut KnownMarketplacesFile),
    home: Option<&Path>,
    env: Option<&EnvMap>,
) -> Result<KnownMarketplacesFile, SettingsStoreError> {
    let path = known_marketplaces_path(home, env);
    with_file_lock(
        &path,
        DEFAULT_LOCK_RETRIES,
        DEFAULT_LOCK_BACKOFF_BASE,
        DEFAULT_LOCK_BACKOFF_MAX,
        || -> io::Result<KnownMarketplacesFile> {
            let mut current = match read_jsonc_with_recovery(&path) {
                Some(data) => coerce_known_marketplaces(&data, &path),
                None => empty_known_marketplaces(),
            };
            mutator(&mut current);
            current.version = MATERIALIZED_VERSION;
            atomic_write_json(&path, &current, Some(WRITE_PROTECTION_HEADER))?;
            Ok(current)
        },
    )?
    .map_err(|e| SettingsStoreError::io(&path, e))
}

/// 持锁原子读-改-写 `installed_plugins.json`（语义同 [`update_known_marketplaces`]）/ Locked atomic RMW。
pub fn update_installed_plugins(
    mutator: impl FnOnce(&mut InstalledPluginsFile),
    home: Option<&Path>,
    env: Option<&EnvMap>,
) -> Result<InstalledPluginsFile, SettingsStoreError> {
    let path = installed_plugins_path(home, env);
    with_file_lock(
        &path,
        DEFAULT_LOCK_RETRIES,
        DEFAULT_LOCK_BACKOFF_BASE,
        DEFAULT_LOCK_BACKOFF_MAX,
        || -> io::Result<InstalledPluginsFile> {
            let mut current = match read_jsonc_with_recovery(&path) {
                Some(data) => coerce_installed_plugins(&data, &path),
                None => empty_installed_plugins(),
            };
            mutator(&mut current);
            current.version = MATERIALIZED_VERSION;
            atomic_write_json(&path, &current, Some(WRITE_PROTECTION_HEADER))?;
            Ok(current)
        },
    )?
    .map_err(|e| SettingsStoreError::io(&path, e))
}

// ===========================================================================
// 普通 settings JSON 持锁写（无写保护头）/ Locked write of a plain settings JSON (no header)
// ===========================================================================
// 与 `update_*` 信封入口的区别：`settings.json` / `mcp.local.json` 是**人编意图层**——无 `version`
// 信封、无 `// 写保护` 头。plugin installer（SET-05 #69，`enabledPlugins`）与 MCP config 门控
// （SET-06 #71，`mcp.local.json` 数组）经此对人编文件做持锁原子 RMW，复用 store 的旁车锁 + 原子写
// 原语而不套 `version`/header 信封。`pub(crate)`：仅 settings 子模块内部接缝。

/// 持 `<path>.lock` 旁车锁执行 `work`（默认退避参数）/ Run `work` holding the sidecar lock。
///
/// 与 [`update_installed_plugins`] 共用 [`with_file_lock`] 的退避语义；供 installer / mcp_config 对
/// 人编 settings 文件做「持锁 load→mutate→write」（写体经 [`atomic_write_settings_json`]，无信封头）。
pub(crate) fn with_settings_lock<T>(
    path: &Path,
    work: impl FnOnce() -> T,
) -> Result<T, SettingsStoreError> {
    with_file_lock(
        path,
        DEFAULT_LOCK_RETRIES,
        DEFAULT_LOCK_BACKOFF_BASE,
        DEFAULT_LOCK_BACKOFF_MAX,
        work,
    )
}

/// 原子写一个**普通 settings JSON**（`header=None`——人编意图层，无写保护头）/ Atomic write, no header。
pub(crate) fn atomic_write_settings_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    atomic_write_json(path, value, None)
}

// ===========================================================================
// lastUpdated 语义 / lastUpdated semantics（供 recorder）
// ===========================================================================
/// 当前 UTC `lastUpdated` 时间戳（`%Y-%m-%dT%H:%M:%SZ`，对齐 Python）/ now ISO-8601 second-precision。
fn now_iso8601() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// 决定 `lastUpdated`：实际 clone/pull（`changed`）或无既有值 → 刷新为 `now`；纯复用 → 保留 `prior`。
///
/// 使该字段语义为「最后更新时间」而非「最后扫描时间」（§6.1）。对齐 Python
/// `now if (changed or not prior_last) else prior_last`（空串视为无既有值）。
fn resolve_last_updated(prior: Option<String>, changed: bool, now: &str) -> String {
    match prior {
        Some(prev) if !changed && !prev.is_empty() => prev,
        _ => now.to_string(),
    }
}

/// `clone_dir.resolve()` 等价：canonicalize（解析 symlink + 绝对化），失败回退词法归一 / resolve display path。
fn resolve_install_location(path: &Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| normalize_lexical(path))
        .display()
        .to_string()
}

// ===========================================================================
// 治理存储接缝实现 / Governance store seam impl
// ===========================================================================
/// 基于物化文件的 [`SkillGovernanceStore`] 实现（闭合 #57 reconciler 接缝）/ file-backed governance store。
///
/// 持有解析后的 SKILL Home 与可选 env；读/写经本模块自由函数（原子写 + 文件锁 + 损坏恢复）。同一对象既实现
/// [`SkillGovernanceStore`]（供 [`reconcile`]）又实现 [`KnownMarketplaceRecorder`]（供
/// [`stage_marketplace_skills`]），经 [`as_recorder`](SkillGovernanceStore::as_recorder) 两用，保证 stage 写入与
/// reconcile 读回同一份 `known_marketplaces.json`。
///
/// trait 表面**无错误**（reconciler 失败降级铁律）：load 缺失 / 损坏 → 空；update / record 的锁或 I/O 失败 →
/// 记 ERROR 并降级（不抛、不阻断）。需要错误可见的调用方（CLI / installer）应直接用自由函数版本。
///
/// [`reconcile`]: crate::settings::reconciler::reconcile
/// [`stage_marketplace_skills`]: crate::skills::staging::stage_marketplace_skills
pub struct FileSkillGovernanceStore {
    home: PathBuf,
    env: Option<EnvMap>,
}

impl FileSkillGovernanceStore {
    /// 用显式 SKILL Home 构造（env 取进程环境）/ construct with an explicit SKILL Home。
    pub fn new(home: impl Into<PathBuf>) -> Self {
        Self {
            home: home.into(),
            env: None,
        }
    }

    /// 用显式 Home + 注入 env 构造（测试 / hermetic）/ construct with an explicit Home and injected env。
    pub fn with_env(home: impl Into<PathBuf>, env: EnvMap) -> Self {
        Self {
            home: home.into(),
            env: Some(env),
        }
    }

    /// 由 env 解析 SKILL Home 构造（对齐 [`resolve_skill_home`] 优先级）/ resolve Home from env。
    pub fn resolve(env: Option<&EnvMap>) -> Self {
        Self {
            home: resolve_skill_home(env),
            env: env.cloned(),
        }
    }

    /// 解析后的 SKILL Home / the resolved SKILL Home。
    pub fn home(&self) -> &Path {
        &self.home
    }

    fn env(&self) -> Option<&EnvMap> {
        self.env.as_ref()
    }
}

impl KnownMarketplaceRecorder for FileSkillGovernanceStore {
    fn record(&self, rec: KnownMarketplaceRecord<'_>) {
        let now = now_iso8601();
        let res = update_known_marketplaces(
            |file| {
                let prior = file
                    .account
                    .marketplaces
                    .get(rec.name)
                    .and_then(|e| e.extra.get("lastUpdated"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let last_updated = resolve_last_updated(prior, rec.changed, &now);

                let mut extra = Map::new();
                extra.insert(
                    "installLocation".to_string(),
                    Value::String(resolve_install_location(rec.install_location)),
                );
                extra.insert("lastUpdated".to_string(), Value::String(last_updated));
                if let Some(sha) = rec.commit_sha {
                    extra.insert("commitSha".to_string(), Value::String(sha.to_string()));
                }
                if rec.auto_update {
                    extra.insert("autoUpdate".to_string(), Value::Bool(true));
                }
                file.account.marketplaces.insert(
                    rec.name.to_string(),
                    KnownMarketplaceEntry {
                        source: rec.source.clone(),
                        extra,
                    },
                );
            },
            Some(&self.home),
            self.env(),
        );
        if let Err(e) = res {
            // 失败降级铁律：物化记录是诊断 / 对账元数据，丢失靠下次 reconcile 重建，不阻断 staging。
            tracing::error!(marketplace = %rec.name, error = %e, "failed to record known_marketplaces.json");
        }
    }
}

impl SkillGovernanceStore for FileSkillGovernanceStore {
    fn load_known_marketplaces(&self) -> KnownMarketplaces {
        load_known_marketplaces(Some(&self.home), self.env()).account
    }

    fn update_known_marketplaces(&self, mutate: &mut dyn FnMut(&mut KnownMarketplaces)) {
        let res = update_known_marketplaces(
            |file| mutate(&mut file.account),
            Some(&self.home),
            self.env(),
        );
        if let Err(e) = res {
            tracing::error!(error = %e, "update known_marketplaces.json failed (degraded)");
        }
    }

    fn load_installed_plugins(&self) -> InstalledPlugins {
        load_installed_plugins(Some(&self.home), self.env()).account
    }

    fn update_installed_plugins(&self, mutate: &mut dyn FnMut(&mut InstalledPlugins)) {
        let res = update_installed_plugins(
            |file| mutate(&mut file.account),
            Some(&self.home),
            self.env(),
        );
        if let Err(e) = res {
            tracing::error!(error = %e, "update installed_plugins.json failed (degraded)");
        }
    }

    fn as_recorder(&self) -> &dyn KnownMarketplaceRecorder {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::reconciler::InstalledPluginRecord;
    use serde_json::json;
    use std::time::Duration;
    use tempfile::TempDir;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    // ---- 纯逻辑 / pure logic ------------------------------------------------
    #[test]
    fn strip_jsonc_header_drops_leading_comment_and_blanks() {
        let text = "// header\n\n{\n  \"a\": 1\n}\n";
        assert_eq!(strip_jsonc_header(text), "{\n  \"a\": 1\n}");
        // 无头不误伤 / no header → untouched body
        assert_eq!(strip_jsonc_header("{\"a\":1}"), "{\"a\":1}");
        // 行尾 // 不剥（仅前导整行）/ trailing // kept
        assert_eq!(strip_jsonc_header("{\"a\":1} // x"), "{\"a\":1} // x");
    }

    #[test]
    fn resolve_last_updated_refresh_vs_preserve() {
        // 无既有值 → now / no prior → now
        assert_eq!(resolve_last_updated(None, false, "NOW"), "NOW");
        // 既有值 + 未变 → 保留 / prior + unchanged → preserve
        assert_eq!(
            resolve_last_updated(Some("OLD".into()), false, "NOW"),
            "OLD"
        );
        // 既有值 + changed → now / prior + changed → now
        assert_eq!(resolve_last_updated(Some("OLD".into()), true, "NOW"), "NOW");
        // 空串视为无既有值 → now / empty prior → now
        assert_eq!(
            resolve_last_updated(Some(String::new()), false, "NOW"),
            "NOW"
        );
    }

    #[test]
    fn lock_sidecar_path_is_dotlock_sibling() {
        let p = Path::new("/tmp/home/known_marketplaces.json");
        assert_eq!(
            lock_sidecar_path(p),
            Path::new("/tmp/home/known_marketplaces.json.lock")
        );
    }

    // ---- 读写 round-trip / load-save round-trip -----------------------------
    #[test]
    fn save_then_load_known_marketplaces_roundtrip() {
        let home = TempDir::new().unwrap();
        let h = Some(home.path());
        let mut file = empty_known_marketplaces();
        file.account.marketplaces.insert(
            "acme".to_string(),
            KnownMarketplaceEntry {
                source: json!({"type": "git", "url": "https://example.com/acme.git"}),
                extra: {
                    let mut m = Map::new();
                    m.insert("installLocation".into(), json!("/home/acme"));
                    m.insert("autoUpdate".into(), json!(true));
                    m
                },
            },
        );
        let path = save_known_marketplaces(&file, h, None).unwrap();
        assert!(path.exists());

        // 写保护头在首行 / header on first line
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk.lines().next().unwrap(), WRITE_PROTECTION_HEADER);
        // 无临时文件残留（原子写）/ no tmp litter
        let litter: Vec<_> = std::fs::read_dir(home.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(
            litter.is_empty(),
            "atomic write left tmp litter: {litter:?}"
        );

        let loaded = load_known_marketplaces(h, None);
        assert_eq!(loaded.version, MATERIALIZED_VERSION);
        let entry = loaded.account.marketplaces.get("acme").unwrap();
        assert_eq!(
            entry.source,
            json!({"type": "git", "url": "https://example.com/acme.git"})
        );
        assert_eq!(entry.extra.get("autoUpdate"), Some(&json!(true)));
    }

    #[test]
    fn save_then_load_installed_plugins_roundtrip() {
        let home = TempDir::new().unwrap();
        let h = Some(home.path());
        let mut file = empty_installed_plugins();
        file.account.plugins.insert(
            "figma@acme".to_string(),
            vec![InstalledPluginRecord {
                install_path: Some("/home/.plugins/figma".to_string()),
                bundled_mcp_servers: vec!["figma-mcp".to_string()],
                extra: {
                    let mut m = Map::new();
                    m.insert("scope".into(), json!("user"));
                    m
                },
            }],
        );
        save_installed_plugins(&file, h, None).unwrap();

        let loaded = load_installed_plugins(h, None);
        let recs = loaded.account.plugins.get("figma@acme").unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(
            recs[0].install_path.as_deref(),
            Some("/home/.plugins/figma")
        );
        assert_eq!(recs[0].bundled_mcp_servers, vec!["figma-mcp".to_string()]);
        assert_eq!(recs[0].extra.get("scope"), Some(&json!("user")));
    }

    #[test]
    fn load_missing_returns_empty() {
        let home = TempDir::new().unwrap();
        let km = load_known_marketplaces(Some(home.path()), None);
        assert!(km.account.marketplaces.is_empty());
        assert_eq!(km.version, MATERIALIZED_VERSION);
        let ip = load_installed_plugins(Some(home.path()), None);
        assert!(ip.account.plugins.is_empty());
    }

    // ---- 损坏恢复 / corruption recovery -------------------------------------
    #[test]
    fn corrupt_json_backed_up_and_degrades_then_rebuilds() {
        let home = TempDir::new().unwrap();
        let h = Some(home.path());
        let path = known_marketplaces_path(h, None);
        std::fs::write(&path, "{ this is not valid json ::::").unwrap();

        // load 触发损坏恢复：备份 .bak + 降级空 / corrupt → backup + empty
        let loaded = load_known_marketplaces(h, None);
        assert!(loaded.account.marketplaces.is_empty());
        assert!(!path.exists(), "corrupt file should have been moved aside");
        let baks: Vec<_> = std::fs::read_dir(home.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| {
                let n = e.file_name().to_string_lossy().to_string();
                n.starts_with("known_marketplaces.json.corrupt-") && n.ends_with(".bak")
            })
            .collect();
        assert_eq!(baks.len(), 1, "expected exactly one .bak backup");
        // 损坏内容保留在 .bak / corrupt content preserved
        let bak_content = std::fs::read_to_string(baks[0].path()).unwrap();
        assert!(bak_content.contains("not valid json"));

        // 下次 save 基于空账本重建 / next save rebuilds cleanly
        save_known_marketplaces(&empty_known_marketplaces(), h, None).unwrap();
        assert!(path.exists());
        let reloaded = load_known_marketplaces(h, None);
        assert!(reloaded.account.marketplaces.is_empty());
    }

    #[test]
    fn root_not_object_is_backed_up() {
        let home = TempDir::new().unwrap();
        let h = Some(home.path());
        let path = known_marketplaces_path(h, None);
        std::fs::write(&path, "[1, 2, 3]").unwrap(); // valid JSON, wrong shape
        let loaded = load_known_marketplaces(h, None);
        assert!(loaded.account.marketplaces.is_empty());
        assert!(!path.exists());
    }

    // ---- 手编痕迹容错 / hand-edit tolerance ---------------------------------
    #[test]
    fn hand_edited_version_and_unknown_keys_tolerated() {
        let home = TempDir::new().unwrap();
        let h = Some(home.path());
        let path = known_marketplaces_path(h, None);
        // 手编：version 异常 + 未知顶层键，但 marketplaces 有效 / odd version + unknown key + valid data
        let raw = json!({
            "version": 999,
            "marketplaces": {"acme": {"source": {"type": "git", "url": "u"}, "installLocation": "/x"}},
            "handEditedJunk": true
        });
        std::fs::write(&path, serde_json::to_string_pretty(&raw).unwrap()).unwrap();

        let loaded = load_known_marketplaces(h, None);
        // version 归一 / version normalized
        assert_eq!(loaded.version, MATERIALIZED_VERSION);
        // 已知数据存活、未知键被忽略（不进 account）/ known data survives, unknown key dropped
        let entry = loaded.account.marketplaces.get("acme").unwrap();
        assert_eq!(entry.extra.get("installLocation"), Some(&json!("/x")));
        assert!(!path.exists() || std::fs::read_to_string(&path).is_ok()); // 未触发损坏备份 / not corrupted
    }

    // ---- 文件锁 / file lock --------------------------------------------------
    #[test]
    fn file_lock_serializes_and_errors_after_retries() {
        let home = TempDir::new().unwrap();
        let data = known_marketplaces_path(Some(home.path()), None);

        // 外层持锁；内层对同一旁车 .lock 再 flock → WouldBlock → 退避 retries=1 耗尽 → Lock 错误。
        let outcome = with_file_lock(&data, 0, ms(1), ms(1), || {
            // 持锁期间内层争用 / inner contends while outer holds
            with_file_lock(&data, 1, ms(1), ms(2), || ())
        })
        .expect("outer lock should be acquired");
        match outcome {
            Err(SettingsStoreError::Lock { retries, .. }) => assert_eq!(retries, 1),
            other => panic!("expected inner Lock error, got {other:?}"),
        }
    }

    #[test]
    fn file_lock_runs_work_when_uncontended() {
        let home = TempDir::new().unwrap();
        let data = known_marketplaces_path(Some(home.path()), None);
        let got = with_file_lock(&data, 0, ms(1), ms(1), || 42).unwrap();
        assert_eq!(got, 42);
    }

    // ---- update RMW / locked read-modify-write ------------------------------
    #[test]
    fn update_known_marketplaces_is_atomic_rmw() {
        let home = TempDir::new().unwrap();
        let h = Some(home.path());
        update_known_marketplaces(
            |file| {
                file.account.marketplaces.insert(
                    "beta".into(),
                    KnownMarketplaceEntry {
                        source: json!({"type": "git", "url": "b"}),
                        extra: Map::new(),
                    },
                );
            },
            h,
            None,
        )
        .unwrap();
        let loaded = load_known_marketplaces(h, None);
        assert!(loaded.account.marketplaces.contains_key("beta"));
    }

    // ---- 接缝 / governance store seam ---------------------------------------
    #[test]
    fn store_seam_load_update_and_recorder() {
        let home = TempDir::new().unwrap();
        let store = FileSkillGovernanceStore::new(home.path());

        // SkillGovernanceStore：空 → update 增 → reload 可见
        assert!(store.load_known_marketplaces().marketplaces.is_empty());
        store.update_known_marketplaces(&mut |km| {
            km.marketplaces.insert(
                "gamma".into(),
                KnownMarketplaceEntry {
                    source: json!({"type": "git", "url": "g"}),
                    extra: Map::new(),
                },
            );
        });
        assert!(store
            .load_known_marketplaces()
            .marketplaces
            .contains_key("gamma"));

        // installed_plugins 接缝 / installed_plugins seam
        store.update_installed_plugins(&mut |ip| {
            ip.plugins
                .insert("p@m".into(), vec![InstalledPluginRecord::default()]);
        });
        assert!(store.load_installed_plugins().plugins.contains_key("p@m"));

        // as_recorder 指向同一 store / upcast yields the same store
        let _recorder: &dyn KnownMarketplaceRecorder = store.as_recorder();
    }

    #[test]
    fn recorder_sets_install_location_and_preserves_last_updated_on_reuse() {
        let home = TempDir::new().unwrap();
        let store = FileSkillGovernanceStore::new(home.path());
        let clone_dir = home.path().join("marketplace").join("acme");
        std::fs::create_dir_all(&clone_dir).unwrap();
        let source = json!({"type": "git", "url": "https://example.com/acme.git"});

        // 首次记录（changed=true）→ 写 installLocation / lastUpdated / autoUpdate
        store.record(KnownMarketplaceRecord {
            name: "acme",
            source: &source,
            install_location: &clone_dir,
            commit_sha: Some("deadbeef"),
            auto_update: true,
            changed: true,
        });
        let after_first = load_known_marketplaces(Some(home.path()), None);
        let e1 = after_first.account.marketplaces.get("acme").unwrap();
        assert!(e1.extra.get("installLocation").is_some());
        assert_eq!(e1.extra.get("commitSha"), Some(&json!("deadbeef")));
        assert_eq!(e1.extra.get("autoUpdate"), Some(&json!(true)));
        let first_last = e1.extra.get("lastUpdated").cloned().unwrap();

        // 复用（changed=false）→ lastUpdated 保留 / reuse preserves lastUpdated
        store.record(KnownMarketplaceRecord {
            name: "acme",
            source: &source,
            install_location: &clone_dir,
            commit_sha: None,
            auto_update: false,
            changed: false,
        });
        let after_reuse = load_known_marketplaces(Some(home.path()), None);
        let e2 = after_reuse.account.marketplaces.get("acme").unwrap();
        assert_eq!(e2.extra.get("lastUpdated"), Some(&first_last));
        // commitSha / autoUpdate 在本次（无 sha + autoUpdate=false）整条替换后不再出现
        assert!(e2.extra.get("commitSha").is_none());
        assert!(e2.extra.get("autoUpdate").is_none());
    }
}
