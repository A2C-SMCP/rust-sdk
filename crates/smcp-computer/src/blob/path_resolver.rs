//! 按路径读取的只读根基础：`readRoots` 三形态编译（配置 / realpath / 磁盘 canonical）+ 围栏。
//! Read-only root foundation for path-based reads: dual-form `readRoots` compilation + fences.
//!
//! 协议依据 / Protocol: `blob-transfer.md` §readRoots（config-first）+ §Computer 处理流程
//! 第 3 / 5 步（protocol#65）。对标 Python `a2c_smcp/computer/blob/path_resolver.py` 的
//! `ReadRoot` / `build_read_roots` / 围栏部分；9 步 resolve 管线、固定拒绝集、句柄铸造与
//! Computer handler 由 PR2b 在本模块扩展。
//!
//! 安全语义 / Security semantics:
//! - **词法围栏**（第 3 步）：配置形态或 realpath 形态任一命中即通过，仅用于免于访问授权外
//!   的文件系统；
//! - **realpath 围栏**（第 5 步）：只认 realpath 形态——安全性只由后者保证；
//! - 围栏按**路径分量**判定，不用裸字符串前缀（`/data` 不包含 `/database`）。

use std::collections::HashSet;
use std::fs::{File, Metadata, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};

use smcp::utils::hash::to_hex;
use smcp::utils::mime::guess_mime;
use smcp::utils::path_norm::{
    native_platform, normalize_path, realpath_nonstrict, to_native_path, PathPlatform,
    RealpathError,
};
use smcp::utils::slice::{plan_slice, SlicePlan};
use tracing::warn;

use crate::blob::handle::{encode_path_handle, BlobHandleError, PathHandlePayload};
use crate::blob::resolver::{BlobSlicer, ResolvedBlob};
use crate::settings::mcp_config::{managed_mcp_config_path, user_mcp_config_path};
use crate::settings::policy::{
    resolve_policy_settings, LINUX_MANAGED_DIR, MACOS_MANAGED_DIR, MACOS_PLIST_PATH,
    MANAGED_SETTINGS_FILENAME, WINDOWS_MANAGED_DIR,
};
use crate::settings::schema::{FIELD_LANDING_ROOT, FIELD_READ_ROOTS};
use crate::settings::scope::{resolve_settings, EnvMap, ResolveSettingsArgs};
use crate::settings::MCP_LOCAL_CONFIG_FILENAME;

/// 一个授权读取根的三形态（配置 / realpath / 磁盘 canonical）/ One read root in three forms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadRoot {
    /// `normalize_path(配置值)`（配置形态；词法围栏命中即可）。
    pub configured: String,
    /// `normalize_path(realpath(配置值))`（realpath 形态；realpath 围栏唯一依据）。
    ///
    /// realpath 失败或结果无法归一 / 非 Unicode → `None`（只保留配置形态并记诊断）。
    pub real: Option<String>,
    /// `normalize_path(canonicalize(配置值))`（**磁盘真实形态**；step-8 最终路径码点比较基准）。
    ///
    /// 与 `real` 分离：`realpath` 保留配置串大小写，`canonicalize` 返回磁盘大小写/链接目标。
    /// 在**构建 roots 时**固化（policy 生成点），早于任何目标路径访问，消除 step4→快照的竞态窗口。
    /// 失败（root 不可解析）→ `None`，该 root 不参与 step-8 匹配。
    pub disk: Option<String>,
}

/// 把 settings `readRoots` 原始列表编译为三形态根列表（宿主平台）。
/// Compile raw `readRoots` items into dual-form roots for the host platform.
///
/// 非法项（非绝对 / `normalize_path` 失败）丢弃；按**配置形态规范化后**去重（首次出现胜出）；
/// realpath 失败只保留配置形态。空输入 = 空列表 = 不可读（fail-closed）。
pub fn build_read_roots(raw: &[String]) -> Vec<ReadRoot> {
    let platform = native_platform();
    let mut roots: Vec<ReadRoot> = Vec::new();
    for item in raw {
        let configured = match normalize_path(item, platform) {
            Ok(path) => path,
            Err(err) => {
                warn!(root = %item, error = %err, "readRoots: 丢弃非法路径 / dropping invalid root");
                continue;
            }
        };
        // 去重前先规范化：`/x` 与 `/x/./` 视为同一根（授权只增不减，首次出现胜出）。
        if roots.iter().any(|root| root.configured == configured) {
            continue;
        }
        let real = match realpath_nonstrict(&configured, platform)
            .and_then(|realpath| normalize_path(&realpath, platform).map_err(RealpathError::from))
        {
            Ok(real) => Some(real),
            Err(err) => {
                warn!(
                    root = %configured,
                    error = %err,
                    "readRoots: realpath 失败，仅保留配置形态 / realpath failed, keeping configured form"
                );
                None
            }
        };
        // 磁盘真实形态：与 real 在同一次构建中固化（早于任何目标访问）。
        let disk = match to_native_path(&configured, platform) {
            Ok(native) => match std::fs::canonicalize(&native) {
                Ok(canonical) => match canonical.to_str() {
                    Some(text) => match normalize_path(text, platform) {
                        Ok(disk) => Some(disk),
                        Err(err) => {
                            warn!(
                                root = %configured, error = %err,
                                "readRoots: 磁盘 canonical 归一失败，该 root 不参与 step-8 / disk form normalization failed"
                            );
                            None
                        }
                    },
                    None => {
                        warn!(
                            root = %configured,
                            "readRoots: 磁盘 canonical 非 Unicode，该 root 不参与 step-8 / disk form is not Unicode"
                        );
                        None
                    }
                },
                Err(err) => {
                    warn!(
                        root = %configured, error = %err,
                        "readRoots: 磁盘 canonical 失败，该 root 不参与 step-8 / disk canonicalization failed"
                    );
                    None
                }
            },
            Err(err) => {
                warn!(
                    root = %configured, error = %err,
                    "readRoots: 无法转宿主原生形态 / to_native_path failed"
                );
                None
            }
        };
        roots.push(ReadRoot {
            configured,
            real,
            disk,
        });
    }
    roots
}

/// `path` 是否等于或位于 `root` 内（按**路径分量**前缀判定）。
/// Whether `path` equals or is nested under `root` (component-prefix check).
///
/// 两侧均应为 `normalize_path` 产物（canonical 形态）。**不**用裸字符串前缀：
/// `/data` 不包含 `/database`。
pub fn path_within_root(path: &str, root: &str) -> bool {
    if path == root {
        return true;
    }
    if root.ends_with('/') {
        // 根 `/` 与 Windows 盘符根 `/C:/`：直接前缀即可。
        path.starts_with(root)
    } else {
        path.starts_with(root) && path.as_bytes().get(root.len()) == Some(&b'/')
    }
}

/// 词法围栏（处理流程第 3 步）：配置形态 **或** realpath 形态任一命中即通过。
/// Lexical fence (step 3): passes if either the configured or the realpath form contains `path`.
pub fn lexical_fence(path: &str, roots: &[ReadRoot]) -> bool {
    roots.iter().any(|root| {
        path_within_root(path, &root.configured)
            || root
                .real
                .as_deref()
                .is_some_and(|real| path_within_root(path, real))
    })
}

/// realpath 围栏（处理流程第 5 步）：**只认** realpath 形态。
/// Realpath fence (step 5): only the realpath form counts.
pub fn realpath_fence(canonical: &str, roots: &[ReadRoot]) -> bool {
    roots.iter().any(|root| {
        root.real
            .as_deref()
            .is_some_and(|real| path_within_root(canonical, real))
    })
}

/// 从五层合并 settings 取 `readRoots` 并编译（每次现读，不缓存）。
/// Read the merged `readRoots` from settings and compile it (no caching).
///
/// # flag scope（**有意分叉**，跟进 rust-sdk#248）
///
/// `flag_settings_path` 恒 `None`：rust `Computer` 是库组件，运行态不持有 CLI `--settings <file>` 路径
/// （与 `Computer::landing_root` 同款）。因此 flag 层声明的 `readRoots` 在 CLI `settings show` 可见、
/// 运行期不生效——这是 SDK 级差异而非静默忽略（见 rust-sdk#248）。
///
/// settings 可能被原子重写，故调用方每次请求应重新调用。
pub fn resolve_read_roots(config_dir: &Path, env: Option<&EnvMap>) -> Vec<ReadRoot> {
    resolve_read_roots_and_landing_root(config_dir, env).0
}

/// 一次 settings 解析同时得到 `readRoots` 与 `landingRoot`（供 Computer 每请求策略装配）。
///
/// `landingRoot` 的 `~` 展开与 `Computer::landing_root` 同源（`expand_home`）；空值 / 非字符串 →
/// `None`。合并解析避免每个 resolve / 每个 get_blob 分块重复跑两遍声明层与 policy 探测。
pub fn resolve_read_roots_and_landing_root(
    config_dir: &Path,
    env: Option<&EnvMap>,
) -> (Vec<ReadRoot>, Option<PathBuf>) {
    let policy = resolve_policy_settings(None, None, None);
    let resolved = resolve_settings(ResolveSettingsArgs {
        cwd: Some(config_dir),
        env,
        flag_settings_path: None,
        policy_settings: Some(&policy),
    });
    let raw: Vec<String> = resolved
        .settings
        .get(FIELD_READ_ROOTS)
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let landing_root = resolved
        .settings
        .get(FIELD_LANDING_ROOT)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
        .map(|raw| crate::skills::home::expand_home(raw, env));
    (build_read_roots(&raw), landing_root)
}

/// `sha256` 流式读取块大小 / Hash read chunk size。
const HASH_CHUNK_BYTES: usize = 1024 * 1024;

/// 铸造期失败原因（协议 4020 `details.reason`，开放枚举）/ Mint-time 4020 reasons。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveReason {
    /// `normalize_path` 失败 / path cannot be normalized。
    InvalidPath,
    /// 越出 `readRoots` / 固定拒绝集 / 打开期权限类失败。
    Forbidden,
    /// 不存在 / does not exist。
    NotFound,
    /// 非普通文件（目录 / 设备 / FIFO / socket）/ not a regular file。
    NotAFile,
}

impl ResolveReason {
    /// 协议 4020 `details.reason` 字符串 / the protocol reason string。
    pub fn as_str(self) -> &'static str {
        match self {
            ResolveReason::InvalidPath => "invalid_path",
            ResolveReason::Forbidden => "forbidden",
            ResolveReason::NotFound => "not_found",
            ResolveReason::NotAFile => "not_a_file",
        }
    }
}

/// `client:resolve_path` 铸造期失败 → 协议 4020 / Mint-time 4020 failure。
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct PathResolveError {
    /// 4020 reason / the reason。
    pub reason: ResolveReason,
    /// 诊断文案（只进日志，不上 wire）/ diagnostic message (logs only)。
    pub message: String,
    /// 内部拒绝阶段（测试判别力用，不上 wire）/ internal rejection stage (tests only).
    pub(crate) stage: &'static str,
}

impl PathResolveError {
    /// 构造 / construct。
    pub fn new(reason: ResolveReason, message: impl Into<String>) -> Self {
        Self {
            reason,
            message: message.into(),
            stage: "unspecified",
        }
    }

    /// 标注拒绝阶段（内部诊断；用于证明端到端用例命中的是预期步骤）/ tag the rejection stage.
    pub(crate) fn at(mut self, stage: &'static str) -> Self {
        self.stage = stage;
        self
    }

    /// 协议 4020 `details.reason` 字符串 / the protocol reason string。
    pub fn reason_str(&self) -> &'static str {
        self.reason.as_str()
    }
}

/// 固定拒绝集（即便在 `readRoots` 内也拒绝）/ Fixed rejection set.
#[derive(Debug, Clone, Default)]
pub struct ProtectedPaths {
    /// 已归一化的绝对路径；按**路径分量前缀**判定 / normalized absolute paths。
    pub paths: Vec<String>,
    /// 上行写入根；其下末级分量以 `.part` 结尾的文件拒绝 / landing root for `.part` rule。
    pub landing_root: Option<String>,
    /// `(dev, ino)` 快照，用于硬链接 SHOULD 防御 / file identities for hard-link defense。
    pub file_ids: HashSet<(u64, u64)>,
}

/// 一次请求的授权快照（读根 + 固定拒绝集）/ Per-request authorization snapshot.
#[derive(Debug, Clone, Default)]
pub struct PathAccessPolicy {
    /// 读取授权根 / read roots。
    pub roots: Vec<ReadRoot>,
    /// 固定拒绝集 / rejection set。
    pub protected: ProtectedPaths,
}

/// `client:resolve_path` 成功结果 / Successful resolve result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPath {
    /// 规范路径（realpath 后经 `normalize_path`，可直接回用作 `path`）。
    pub canonical_path: String,
    /// 现铸的不透明 path 句柄（转 `client:get_blob` 拉字节）。
    pub blob_handle: String,
    /// 确定性 MIME（复用 `smcp::utils::mime`）。
    pub mime_type: String,
    /// 文件字节数。
    pub total_size: u64,
    /// 全量 `sha256` 十六进制（小写）。
    pub sha256: String,
    /// 最后修改时间（Unix epoch 毫秒，向下取整）。
    pub mtime: i64,
}

/// `client:get_blob` 拉取期复核结果；持有同一打开句柄 / Pull-time revalidation result.
pub struct PathPullResult {
    /// 句柄内 MIME。
    pub mime_type: String,
    /// 句柄内总字节数（与打开句柄 stat 键相符）。
    pub total_size: u64,
    /// 句柄内摘要（拉取期**不**重算）。
    pub sha256: String,
    /// 同一打开句柄（`read_at` 无状态，`Arc` 允许切片闭包共享）。
    file: Arc<File>,
}

impl std::fmt::Debug for PathPullResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PathPullResult")
            .field("mime_type", &self.mime_type)
            .field("total_size", &self.total_size)
            .field("sha256", &self.sha256)
            .finish_non_exhaustive()
    }
}

impl PathPullResult {
    /// 读取 `[offset, offset+length)`；边界语义与 [`ResolvedBlob::slice`] 一致（共享 [`plan_slice`]）。
    pub fn slice(&self, offset: u64, length: u64) -> Result<Vec<u8>, BlobHandleError> {
        match plan_slice(offset, length, self.total_size) {
            SlicePlan::OutOfRange => Err(BlobHandleError::Range(format!(
                "offset {offset} > total_size {}",
                self.total_size
            ))),
            SlicePlan::Empty => Ok(Vec::new()),
            SlicePlan::Read { offset, length } => {
                let length = usize::try_from(length).map_err(|_| {
                    BlobHandleError::Range(format!("slice length {length} does not fit usize"))
                })?;
                read_exact_at(&self.file, offset, length)
                    .map_err(|e| BlobHandleError::Gone(format!("path slice read failed: {e}")))
            }
        }
    }

    /// 转成 get_blob 统一描述符（切片仍从同一打开句柄读取）。
    pub fn into_resolved_blob(self) -> ResolvedBlob {
        let file = self.file;
        let slicer: BlobSlicer = Box::new(move |offset, length| {
            let length = usize::try_from(length).map_err(|_| {
                BlobHandleError::Range(format!("slice length {length} does not fit usize"))
            })?;
            read_exact_at(&file, offset, length)
                .map_err(|e| BlobHandleError::Gone(format!("path slice read failed: {e}")))
        });
        ResolvedBlob::new(self.mime_type, self.total_size, self.sha256, slicer)
    }
}

/// `$XDG_STATE_HOME`（绝对）或 `~/.local/state` / state home resolution.
fn state_home_dir(env: Option<&EnvMap>) -> PathBuf {
    let lookup = |key: &str| {
        env.and_then(|map| map.get(key).cloned())
            .or_else(|| std::env::var(key).ok())
    };
    if let Some(xdg) = lookup("XDG_STATE_HOME") {
        let path = PathBuf::from(&xdg);
        if path.is_absolute() {
            return path;
        }
    }
    let home = lookup("HOME").unwrap_or_else(|| "~".to_string());
    PathBuf::from(home).join(".local").join("state")
}

/// 构造固定拒绝集：SDK 配置 / 状态 / 凭据目录 + 接线层注入的额外路径 + landing root。
///
/// 默认集覆盖 user-config 目录（settings / `computer-id`）；`extra_paths` 由 Computer 注入
/// （SKILL Home、blob 缓存根、`.tfrobot` 治理目录与具体文件、flag settings 等）。每次请求
/// **现算**（settings 与文件身份必须反映当前状态）。
pub fn build_protected_paths(
    extra_paths: &[PathBuf],
    landing_root: Option<&Path>,
    env: Option<&EnvMap>,
    platform: PathPlatform,
) -> ProtectedPaths {
    let mut candidates: Vec<PathBuf> = Vec::new();
    let config_dir = crate::settings::resolve_user_config_dir(env);
    candidates.push(config_dir.clone());
    candidates.push(crate::settings::user_settings_path(env));
    candidates.push(crate::identity::computer_id_path(env));
    // user-scope MCP 声明（可含 server 字面量 env / 凭据）：目录前缀 + 具体文件身份双保险。
    let user_mcp = user_mcp_config_path(env);
    candidates.push(user_mcp.clone());
    candidates.push(user_mcp.with_file_name(MCP_LOCAL_CONFIG_FILENAME));
    // policy scope 企业下发位置（可含 env / 凭据）：按宿主平台取有效形态（与 Python 同源）。
    if platform == PathPlatform::Windows {
        candidates.push(PathBuf::from(WINDOWS_MANAGED_DIR));
        candidates.push(PathBuf::from(WINDOWS_MANAGED_DIR).join(MANAGED_SETTINGS_FILENAME));
    } else {
        candidates.push(PathBuf::from(MACOS_MANAGED_DIR));
        candidates.push(PathBuf::from(MACOS_MANAGED_DIR).join(MANAGED_SETTINGS_FILENAME));
        candidates.push(PathBuf::from(MACOS_PLIST_PATH));
        candidates.push(PathBuf::from(LINUX_MANAGED_DIR));
        candidates.push(PathBuf::from(LINUX_MANAGED_DIR).join(MANAGED_SETTINGS_FILENAME));
    }
    candidates.push(managed_mcp_config_path(None));
    // 旧版（≤v0.4）非密钥 input 明文缓存（`$XDG_STATE_HOME/a2c/input-values.json`）；升级残留同样拒绝。
    let state_dir = state_home_dir(env).join("a2c");
    candidates.push(state_dir.clone());
    candidates.push(state_dir.join("input-values.json"));
    candidates.extend(extra_paths.iter().cloned());

    let mut paths: HashSet<String> = HashSet::new();
    let mut file_ids: HashSet<(u64, u64)> = HashSet::new();
    for candidate in candidates {
        // 相对注入项（如 `--mcp-config mcp.json`、相对 with_* 覆盖）按 cwd 绝对化后再归一，
        // 避免被静默丢弃而绕过固定拒绝集。
        let candidate = std::path::absolute(&candidate).unwrap_or(candidate);
        let Some(candidate) = candidate.to_str() else {
            continue;
        };
        let Ok(configured) = normalize_path(candidate, platform) else {
            warn!(
                path = %candidate,
                "rejection set: dropping path that cannot be normalized"
            );
            continue;
        };
        paths.insert(configured.clone());
        if let Ok(real_native) = realpath_nonstrict(&configured, platform) {
            if let Ok(real) = normalize_path(&real_native, platform) {
                paths.insert(real);
            }
        }
        if let Ok(native) = to_native_path(&configured, platform) {
            // 磁盘真实形态（大小写/链接目标）：step-8 最终路径为磁盘形态，按码点前缀匹配。
            if let Ok(canonical) = std::fs::canonicalize(&native) {
                if let Some(text) = canonical.to_str() {
                    if let Ok(disk) = normalize_path(text, platform) {
                        paths.insert(disk);
                    }
                }
            }
            // 硬链接 SHOULD：记录实际文件的 (dev, ino)；目录不展开（前缀判定已覆盖）。
            if let Ok(metadata) = std::fs::metadata(&native) {
                if metadata.is_file() {
                    if let Some(identity) = file_identity(&metadata) {
                        file_ids.insert(identity);
                    }
                }
            }
        }
    }

    let landing_root = landing_root.and_then(|root| {
        let root = std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf());
        let text = root.to_str()?;
        let configured = normalize_path(text, platform).ok()?;
        // 磁盘真实形态优先：step-8 最终路径为磁盘形态，`.part` 规则须能码点匹配。
        let disk = to_native_path(&configured, platform)
            .ok()
            .and_then(|native| std::fs::canonicalize(native).ok())
            .and_then(|canonical| canonical.to_str().map(str::to_string))
            .and_then(|text| normalize_path(&text, platform).ok());
        let real = realpath_nonstrict(&configured, platform)
            .ok()
            .and_then(|resolved| normalize_path(&resolved, platform).ok());
        Some(disk.or(real).unwrap_or(configured))
    });

    ProtectedPaths {
        paths: paths.into_iter().collect(),
        landing_root,
        file_ids,
    }
}

/// 执行协议九步 resolve 管线并铸造 path 句柄。
///
/// 顺序 MUST：`normalize_path`（`..` 词法先消解）→ `readRoots` 空 / 词法围栏（不访问文件系统）→
/// 非严格 realpath 围栏 → 固定拒绝集 → `lstat` 类型预检 → 非阻塞打开 + `fstat` →
/// 同一打开句柄 stat 键 + `sha256` → 铸造句柄。
pub fn resolve_path(
    path: &str,
    roots: &[ReadRoot],
    protected: &ProtectedPaths,
    platform: PathPlatform,
) -> Result<ResolvedPath, PathResolveError> {
    let normalized = normalize_path(path, platform).map_err(|e| {
        PathResolveError::new(ResolveReason::InvalidPath, format!("invalid path: {e}"))
    })?;

    let (canonical, file) = run_pipeline(&normalized, roots, protected, platform)?;

    // 第 9 步：同一打开句柄取 stat 键 + sha256；计算后 SHOULD 再 fstat，变化则重算一次。
    let mut key = stat_key(&file.metadata().map_err(|e| {
        PathResolveError::new(ResolveReason::Forbidden, format!("fstat failed: {e}"))
    })?);
    let mut digest = hash_file(&file).map_err(|e| {
        PathResolveError::new(ResolveReason::Forbidden, format!("sha256 failed: {e}"))
    })?;
    let key_after = stat_key(&file.metadata().map_err(|e| {
        PathResolveError::new(ResolveReason::Forbidden, format!("fstat failed: {e}"))
    })?);
    if key_after != key {
        key = key_after;
        digest = hash_file(&file).map_err(|e| {
            PathResolveError::new(ResolveReason::Forbidden, format!("sha256 failed: {e}"))
        })?;
    }

    let mime = guess_mime(&canonical).to_string();
    let handle = encode_path_handle(
        &canonical,
        key.size,
        key.mtime_ms,
        &digest,
        &mime,
        key.identity,
    );
    Ok(ResolvedPath {
        canonical_path: canonical,
        blob_handle: handle,
        mime_type: mime,
        total_size: key.size,
        sha256: digest,
        mtime: key.mtime_ms,
    })
}

/// `client:get_blob` 解析 path 句柄时按**当前** `readRoots` 与文件系统状态重跑第 3–8 步。
///
/// 失败映射（4018 枚举不变）：越界 / 拒绝集 / 其余打开错误 → `forbidden`；不存在 / 非普通文件 /
/// stat 键不符（已改写）→ `gone`；句柄结构非法 → `invalid_handle`。相符则回填句柄内
/// `sha256` / `total_size`，**不**重算摘要。
pub fn resolve_for_pull(
    payload: &PathHandlePayload,
    roots: &[ReadRoot],
    protected: &ProtectedPaths,
    platform: PathPlatform,
) -> Result<PathPullResult, BlobHandleError> {
    validate_path_payload(payload, platform)?;

    let (_, file) = run_pipeline(&payload.canonical_path, roots, protected, platform)
        .map_err(map_pull_error)?;

    let key = stat_key(&file.metadata().map_err(|e| {
        BlobHandleError::Gone(format!("path handle fstat failed after resolve: {e}"))
    })?);
    if key.size != payload.total_size || key.mtime_ms != payload.mtime {
        return Err(BlobHandleError::Gone(format!(
            "path handle stat key mismatch: size {}/{} mtime {}/{}",
            key.size, payload.total_size, key.mtime_ms, payload.mtime
        )));
    }
    if let (Some(dev), Some(ino)) = (payload.dev, payload.ino) {
        if key.identity != Some((dev, ino)) {
            return Err(BlobHandleError::Gone(format!(
                "path handle file identity mismatch: {:?} vs ({dev}, {ino})",
                key.identity
            )));
        }
    }

    Ok(PathPullResult {
        mime_type: payload.mime.clone(),
        total_size: payload.total_size,
        sha256: payload.sha256.clone(),
        file: Arc::new(file),
    })
}

// ── 管线内部 / Pipeline internals ────────────────────────────────────────

/// 执行第 2–8 步；成功返回 `(canonical_path, opened file)`，失败抛 [`PathResolveError`]。
fn run_pipeline(
    normalized: &str,
    roots: &[ReadRoot],
    protected: &ProtectedPaths,
    platform: PathPlatform,
) -> Result<(String, File), PathResolveError> {
    run_pipeline_with(normalized, roots, protected, platform, &final_path)
}

/// [`run_pipeline`] 的可注入最终路径源版本（测试用；生产固定走 [`final_path`]）。
fn run_pipeline_with<F>(
    normalized: &str,
    roots: &[ReadRoot],
    protected: &ProtectedPaths,
    platform: PathPlatform,
    final_path_fn: &F,
) -> Result<(String, File), PathResolveError>
where
    F: Fn(&File) -> Result<String, PathResolveError>,
{
    // 2) readRoots 空 → forbidden。
    if roots.is_empty() {
        return Err(PathResolveError::new(
            ResolveReason::Forbidden,
            "readRoots is empty",
        ));
    }

    // 3) 词法围栏（不访问文件系统）。
    if !lexical_fence(normalized, roots) {
        return Err(PathResolveError::new(
            ResolveReason::Forbidden,
            format!("path is outside readRoots: {normalized:?}"),
        )
        .at("lexical_fence"));
    }

    // 4) 非严格 realpath（含悬空链接）；本步之前任何 FS 错误一律 forbidden。
    let real = realpath_nonstrict(normalized, platform).map_err(|e| {
        PathResolveError::new(ResolveReason::Forbidden, format!("realpath failed: {e}"))
            .at("realpath")
    })?;
    let canonical = normalize_path(&real, platform).map_err(|e| {
        PathResolveError::new(
            ResolveReason::Forbidden,
            format!("canonical path cannot be normalized: {e}"),
        )
        .at("realpath")
    })?;

    // 5) realpath 围栏（只认 real 形态）。
    if !realpath_fence(&canonical, roots) {
        return Err(PathResolveError::new(
            ResolveReason::Forbidden,
            format!("canonical path is outside readRoots: {canonical:?}"),
        )
        .at("realpath_fence"));
    }

    // 6) 固定拒绝集。
    if is_protected(&canonical, protected, platform) {
        return Err(PathResolveError::new(
            ResolveReason::Forbidden,
            format!("path is in the fixed rejection set: {canonical:?}"),
        )
        .at("protected_set"));
    }

    let native = to_native_path(&canonical, platform).map_err(|e| {
        PathResolveError::new(
            ResolveReason::Forbidden,
            format!("canonical path has no native form: {e}"),
        )
    })?;

    // 7) 类型预检（打开之前）。
    let lstat = std::fs::symlink_metadata(&native).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory => PathResolveError::new(
            ResolveReason::NotFound,
            format!("path does not exist: {canonical:?}"),
        ),
        _ => PathResolveError::new(ResolveReason::Forbidden, format!("lstat failed: {e}")),
    })?;
    if !lstat.is_file() {
        return Err(PathResolveError::new(
            ResolveReason::NotAFile,
            format!("not a regular file: {canonical:?}"),
        ));
    }

    // 8) 非阻塞打开（SHOULD O_NOFOLLOW）+ fstat + 最终路径复核。
    let file = open_nonblocking(&native).map_err(|e| open_error(&e, &native, platform))?;
    let fstat = file.metadata().map_err(|e| {
        PathResolveError::new(ResolveReason::Forbidden, format!("fstat failed: {e}"))
    })?;
    if !fstat.is_file() {
        return Err(PathResolveError::new(
            ResolveReason::NotAFile,
            format!("opened handle is not a regular file: {canonical:?}"),
        ));
    }
    if file_identity(&fstat) != file_identity(&lstat) {
        return Err(PathResolveError::new(
            ResolveReason::Forbidden,
            "file identity changed between lstat and open",
        ));
    }
    if matches_protected_identity(&fstat, protected, platform) {
        return Err(PathResolveError::new(
            ResolveReason::Forbidden,
            "file identity is in the fixed rejection set (hard link)",
        ));
    }
    // TOCTOU 闭环（MUST）：从已打开句柄取最终路径，重跑第 5–6 步；取不到/不符 → forbidden。
    // 最终路径**仅用于复核**；返回/铸造的 canonical_path 保持第 4 步形态（保留输入与配置大小写），
    // 以保证「canonical_path 可直接回用作 path」——Windows 上若改铸磁盘大小写，step 3/5 的码点
    // 比较会让回灌与 get_blob 全部 forbidden。
    recheck_final_path_with(&file, roots, protected, platform, final_path_fn)?;

    Ok((canonical, file))
}

fn validate_path_payload(
    payload: &PathHandlePayload,
    platform: PathPlatform,
) -> Result<(), BlobHandleError> {
    let normalized = normalize_path(&payload.canonical_path, platform).map_err(|e| {
        BlobHandleError::Invalid(format!("path handle invalid canonical_path: {e}"))
    })?;
    if normalized != payload.canonical_path {
        return Err(BlobHandleError::Invalid(format!(
            "path handle canonical_path is not normalized: {:?}",
            payload.canonical_path
        )));
    }
    if payload.sha256.len() != 64
        || !payload
            .sha256
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(BlobHandleError::Invalid(
            "path handle invalid sha256".to_string(),
        ));
    }
    if payload.mime.is_empty() {
        return Err(BlobHandleError::Invalid(
            "path handle missing mime".to_string(),
        ));
    }
    if payload.dev.is_some() != payload.ino.is_some() {
        return Err(BlobHandleError::Invalid(
            "path handle must carry both dev and ino, or neither".to_string(),
        ));
    }
    Ok(())
}

fn map_pull_error(error: PathResolveError) -> BlobHandleError {
    match error.reason {
        ResolveReason::InvalidPath => BlobHandleError::Invalid(error.message),
        ResolveReason::Forbidden => BlobHandleError::Forbidden(error.message),
        ResolveReason::NotFound | ResolveReason::NotAFile => BlobHandleError::Gone(error.message),
    }
}

/// 仅服务**固定拒绝集**的目录前缀判定（protocol#76）。
///
/// Windows + macOS：大小写不敏感（`to_lowercase`，与既有名称谓词 / `skills::sandbox` 一致）；
/// 其余平台保持码点。**不**用于 readRoots 围栏——后者 MUST 保持码点（[`path_within_root`]）。
fn protected_path_within(path: &str, root: &str, fold: bool) -> bool {
    if !fold {
        return path_within_root(path, root);
    }
    path_within_root(&path.to_lowercase(), &root.to_lowercase())
}

/// 固定拒绝集判定（含 `.skillenv` / landing `.part`）。
///
/// protocol#76：拒绝集的**名称判定与目录判定**在 **Windows + macOS** 均大小写不敏感
/// （`to_lowercase`；与 Python `casefold` 的常见路径一致）；Linux（大小写敏感）保持码点，
/// 避免把普通 `.SKILLENV` / `.TFROBOT` 误判为受保护。step 6 目录变体因此直接命中
/// `protected_set`，不再依赖 step-8 磁盘形态兜底。
///
/// readRoots 围栏**不**折叠（协议要求码点比较，见 [`path_within_root`]）。
fn is_protected(canonical_path: &str, protected: &ProtectedPaths, platform: PathPlatform) -> bool {
    let fold = platform == PathPlatform::Windows || cfg!(target_os = "macos");
    if protected
        .paths
        .iter()
        .any(|entry| protected_path_within(canonical_path, entry, fold))
    {
        return true;
    }
    let raw_name = canonical_path.rsplit('/').next().unwrap_or(canonical_path);
    let name = if fold {
        raw_name.to_lowercase()
    } else {
        raw_name.to_string()
    };
    if name == ".skillenv" {
        return true;
    }
    if let Some(landing) = protected.landing_root.as_deref() {
        if protected_path_within(canonical_path, landing, fold) && name.ends_with(".part") {
            return true;
        }
    }
    false
}

fn matches_protected_identity(
    metadata: &Metadata,
    protected: &ProtectedPaths,
    platform: PathPlatform,
) -> bool {
    let Some(identity) = file_identity(metadata) else {
        return false;
    };
    if protected.file_ids.contains(&identity) {
        return true;
    }
    // 构建期快照可能过期（原子重写改变 inode）；对受保护文件中当前为 regular file 的项 re-stat。
    protected.paths.iter().any(|entry| {
        to_native_path(entry, platform)
            .ok()
            .and_then(|native| std::fs::metadata(native).ok())
            .is_some_and(|metadata| {
                metadata.is_file() && file_identity(&metadata) == Some(identity)
            })
    })
}

/// 打开句柄的 stat 键（句柄内编码 + 拉取期比对）/ stat key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StatKey {
    size: u64,
    mtime_ms: i64,
    identity: Option<(u64, u64)>,
}

fn stat_key(metadata: &Metadata) -> StatKey {
    StatKey {
        size: metadata.len(),
        mtime_ms: mtime_ms(metadata),
        identity: file_identity(metadata),
    }
}

fn hash_file(file: &File) -> io::Result<String> {
    let mut hasher = Sha256::new();
    let mut offset = 0u64;
    let mut buffer = vec![0u8; HASH_CHUNK_BYTES];
    loop {
        let read = read_at(file, offset, &mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        offset += read as u64;
    }
    Ok(to_hex(&hasher.finalize()))
}

fn open_nonblocking(native: &str) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    options.open(native)
}

fn open_error(error: &io::Error, native: &str, platform: PathPlatform) -> PathResolveError {
    #[cfg(unix)]
    if let Some(code) = error.raw_os_error() {
        if code == libc::ENOENT || code == libc::ENOTDIR {
            return PathResolveError::new(ResolveReason::NotFound, format!("open failed: {error}"));
        }
        if code == libc::EISDIR {
            return PathResolveError::new(ResolveReason::NotAFile, format!("open failed: {error}"));
        }
    }
    if platform == PathPlatform::Windows {
        // Win32：FILE_NOT_FOUND(2) / PATH_NOT_FOUND(3) → not_found；
        // 打开目录的 EACCES(5)/共享冲突(32) 由下方目录判定归 not_a_file。
        match error.raw_os_error() {
            Some(2) | Some(3) => {
                return PathResolveError::new(
                    ResolveReason::NotFound,
                    format!("open failed: {error}"),
                )
            }
            _ => {}
        }
    }
    // 打开目录常报权限类错误；用 metadata 区分目录 / 权限。
    if let Ok(metadata) = std::fs::metadata(native) {
        if metadata.is_dir() {
            return PathResolveError::new(
                ResolveReason::NotAFile,
                format!("open failed (directory): {error}"),
            );
        }
    }
    PathResolveError::new(ResolveReason::Forbidden, format!("open failed: {error}"))
}

fn file_identity(metadata: &Metadata) -> Option<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some((metadata.dev(), metadata.ino()))
    }
    #[cfg(windows)]
    {
        // std 的 `volume_serial_number` / `file_index` 仍不稳定（`windows_by_handle`，stable 不可用）；
        // 协议文件身份为**可选**，Windows 由最终路径复核 + size/mtime stat 键兜底。
        let _ = metadata;
        None
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = metadata;
        None
    }
}

fn mtime_ms(metadata: &Metadata) -> i64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let nanos_since_epoch =
            i128::from(metadata.mtime()) * 1_000_000_000 + i128::from(metadata.mtime_nsec());
        nanos_since_epoch.div_euclid(1_000_000) as i64
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // FILETIME：100ns since 1601-01-01；Unix epoch 偏移 11644473600s。
        const EPOCH_OFFSET: i128 = 116_444_736_000_000_000;
        let hundred_ns = i128::from(metadata.last_write_time()) - EPOCH_OFFSET;
        hundred_ns.div_euclid(10_000) as i64
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = metadata;
        0
    }
}

#[cfg(unix)]
fn read_at(file: &File, offset: u64, buffer: &mut [u8]) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.read_at(buffer, offset)
}

#[cfg(windows)]
fn read_at(file: &File, offset: u64, buffer: &mut [u8]) -> io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_read(buffer, offset)
}

#[cfg(not(any(unix, windows)))]
fn read_at(_file: &File, _offset: u64, _buffer: &mut [u8]) -> io::Result<usize> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "positional reads are not supported on this platform",
    ))
}

fn read_exact_at(file: &File, offset: u64, length: usize) -> io::Result<Vec<u8>> {
    let mut buffer = vec![0u8; length];
    let mut filled = 0usize;
    while filled < length {
        let read = read_at(file, offset + filled as u64, &mut buffer[filled..])?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    buffer.truncate(filled);
    Ok(buffer)
}

/// 第 8 步 TOCTOU 闭环：对已打开句柄取最终路径，与 open 前固化的 root 磁盘快照做码点比较，
/// 并重跑固定拒绝集。**取不到最终路径即 fail-closed（forbidden）**，不静默跳过。
#[cfg(test)]
fn recheck_final_path(
    file: &File,
    roots: &[ReadRoot],
    protected: &ProtectedPaths,
    platform: PathPlatform,
) -> Result<String, PathResolveError> {
    recheck_final_path_with(file, roots, protected, platform, &final_path)
}

/// [`recheck_final_path`] 的可注入最终路径源版本（测试用）。
fn recheck_final_path_with<F>(
    file: &File,
    roots: &[ReadRoot],
    protected: &ProtectedPaths,
    platform: PathPlatform,
    final_path_fn: &F,
) -> Result<String, PathResolveError>
where
    F: Fn(&File) -> Result<String, PathResolveError>,
{
    let final_path = final_path_fn(file).map_err(|error| error.at("final_path_recheck"))?;
    // 与 **roots 构建时固化的磁盘形态** 做码点比较（不做大小写折叠）：
    // 磁盘形态在 open 前后不一致 ⇒ 竞态/替换 ⇒ 拒绝。
    let within_root = roots
        .iter()
        .filter_map(|root| root.disk.as_deref())
        .any(|snapshot| path_within_root(&final_path, snapshot));
    if !within_root {
        return Err(PathResolveError::new(
            ResolveReason::Forbidden,
            format!("opened file final path is outside readRoots: {final_path:?}"),
        )
        .at("final_path_recheck"));
    }
    if is_protected(&final_path, protected, platform) {
        return Err(PathResolveError::new(
            ResolveReason::Forbidden,
            format!("opened file final path is in the rejection set: {final_path:?}"),
        )
        .at("final_path_recheck"));
    }
    Ok(final_path)
}

/// 已打开句柄的最终路径（MUST / fail-closed）。
///
/// Linux `/proc/self/fd` → macOS `F_GETPATH` → Windows `GetFinalPathNameByHandleW`；
/// 取不到 / 无法归一 / 已删除 → `forbidden`（不返回 `None` 跳过）。
fn final_path(file: &File) -> Result<String, PathResolveError> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;
        let link = format!("/proc/self/fd/{}", file.as_raw_fd());
        let target = std::fs::read_link(&link).map_err(|e| {
            PathResolveError::new(
                ResolveReason::Forbidden,
                format!("final path unavailable for opened handle: {e}"),
            )
        })?;
        let target = target.to_str().ok_or_else(|| {
            PathResolveError::new(
                ResolveReason::Forbidden,
                "final path is not representable as UTF-8",
            )
        })?;
        if target.ends_with(" (deleted)") {
            return Err(PathResolveError::new(
                ResolveReason::Forbidden,
                "file was deleted after open",
            ));
        }
        normalize_path(target, native_platform()).map_err(|e| {
            PathResolveError::new(
                ResolveReason::Forbidden,
                format!("final path cannot be normalized: {e}"),
            )
        })
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::io::AsRawFd;
        let mut buffer = [0u8; 1024];
        let result = unsafe {
            libc::fcntl(
                file.as_raw_fd(),
                libc::F_GETPATH,
                buffer.as_mut_ptr() as *mut libc::c_char,
            )
        };
        if result == -1 {
            return Err(PathResolveError::new(
                ResolveReason::Forbidden,
                "F_GETPATH failed for opened handle",
            ));
        }
        let length = buffer.iter().position(|b| *b == 0).unwrap_or(buffer.len());
        let target = std::str::from_utf8(&buffer[..length]).map_err(|_| {
            PathResolveError::new(ResolveReason::Forbidden, "final path is not valid UTF-8")
        })?;
        normalize_path(target, native_platform()).map_err(|e| {
            PathResolveError::new(
                ResolveReason::Forbidden,
                format!("final path cannot be normalized: {e}"),
            )
        })
    }
    #[cfg(windows)]
    {
        windows_final_path(file)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = file;
        Err(PathResolveError::new(
            ResolveReason::Forbidden,
            "final path recheck is unavailable on this platform",
        ))
    }
}

/// Windows 最终路径（`GetFinalPathNameByHandleW`；返回 `\\?\` 形态由 `normalize_path` 剥离）。
#[cfg(windows)]
fn windows_final_path(file: &File) -> Result<String, PathResolveError> {
    use std::os::windows::io::AsRawHandle;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetFinalPathNameByHandleW(
            handle: *mut core::ffi::c_void,
            buffer: *mut u16,
            buffer_len: u32,
            flags: u32,
        ) -> u32;
    }

    let handle = file.as_raw_handle();
    let mut buffer = vec![0u16; 1024];
    loop {
        let len = unsafe {
            GetFinalPathNameByHandleW(handle, buffer.as_mut_ptr(), buffer.len() as u32, 0)
        };
        if len == 0 {
            return Err(PathResolveError::new(
                ResolveReason::Forbidden,
                "GetFinalPathNameByHandleW failed for opened handle",
            ));
        }
        if (len as usize) < buffer.len() {
            let target = String::from_utf16(&buffer[..len as usize]).map_err(|_| {
                PathResolveError::new(ResolveReason::Forbidden, "final path is not valid UTF-16")
            })?;
            return normalize_path(&target, native_platform()).map_err(|e| {
                PathResolveError::new(
                    ResolveReason::Forbidden,
                    format!("final path cannot be normalized: {e}"),
                )
            });
        }
        // 缓冲区不足：按返回的所需长度扩容重试。
        buffer.resize(len as usize + 1, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blob::handle::{decode_blob_handle, DecodedHandle};
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn path_within_root_uses_component_prefix() {
        assert!(path_within_root("/data", "/data"));
        assert!(path_within_root("/data/a", "/data"));
        assert!(!path_within_root("/database", "/data")); // 裸字符串前缀陷阱
        assert!(!path_within_root("/data2/a", "/data"));
        assert!(path_within_root("/a/b", "/"));
        assert!(path_within_root("/C:/a", "/C:/"));
        assert!(path_within_root("/C:/ab", "/C:/")); // 盘符根包含其下一切路径
        assert!(!path_within_root("/data", "/data/sub"));
    }

    #[test]
    fn empty_roots_fail_closed() {
        let roots = build_read_roots(&[]);
        assert!(roots.is_empty());
        assert!(!lexical_fence("/any/file", &roots));
        assert!(!realpath_fence("/any/file", &roots));
    }

    #[test]
    fn invalid_items_are_dropped_and_normalized_duplicates_collapse() {
        // `build_read_roots` 用宿主平台：测试路径随宿主取形（Windows 须盘符绝对）。
        let (root, duplicate, expected) = if cfg!(windows) {
            ("C:/data/share/", "C:/data/./share", "/C:/data/share")
        } else {
            ("/data/share/", "/data/./share", "/data/share")
        };
        let raw = vec![
            root.to_string(),
            duplicate.to_string(),
            "relative/path".to_string(),
            String::new(),
        ];
        let roots = build_read_roots(&raw);
        assert_eq!(roots.len(), 1, "roots={roots:?}");
        assert_eq!(roots[0].configured, expected);
    }

    #[cfg(unix)]
    #[test]
    fn dual_forms_and_symlink_fences() {
        let temp = tempfile::tempdir().unwrap();
        let outside = temp.path().join("outside");
        let inside = temp.path().join("inside");
        fs::create_dir_all(&outside).unwrap();
        fs::create_dir_all(&inside).unwrap();
        let file = outside.join("secret.txt");
        fs::write(&file, b"x").unwrap();
        let link = inside.join("link");
        std::os::unix::fs::symlink(&file, &link).unwrap();

        let root = inside.to_str().unwrap().to_string();
        let roots = build_read_roots(&[root]);
        assert_eq!(roots.len(), 1);
        let real_root = fs::canonicalize(&inside).unwrap();
        assert_eq!(
            roots[0].real.as_deref(),
            Some(real_root.to_str().unwrap()),
            "root realpath form"
        );

        // 词法围栏：配置形态命中（链接本身位于 root 内）→ 通过，不访问 FS 即可判定。
        let lexical_link = inside.join("link").to_str().unwrap().to_string();
        assert!(lexical_fence(&lexical_link, &roots));
        // realpath 围栏：只认 realpath 形态 → 链接目标越界 → 拒绝。
        let real_link = fs::canonicalize(&link).unwrap();
        assert!(!realpath_fence(real_link.to_str().unwrap(), &roots));
        // 目标也在 root 内时 realpath 围栏通过。
        let inside_file = inside.join("ok.txt");
        fs::write(&inside_file, b"ok").unwrap();
        let real_inside = fs::canonicalize(&inside_file).unwrap();
        assert!(realpath_fence(real_inside.to_str().unwrap(), &roots));
    }

    #[cfg(unix)]
    #[test]
    fn dangling_symlink_is_resolved_non_strictly() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("dir");
        fs::create_dir_all(&dir).unwrap();
        let dangling = dir.join("dangling");
        let missing = dir.join("missing.txt");
        std::os::unix::fs::symlink(&missing, &dangling).unwrap();

        let canonical = normalize_path(dangling.to_str().unwrap(), native_platform()).unwrap();
        let resolved = realpath_nonstrict(&canonical, native_platform()).unwrap();
        let expected = fs::canonicalize(&dir).unwrap().join("missing.txt");
        assert_eq!(resolved, expected.to_str().unwrap());
    }

    #[test]
    fn resolve_read_roots_reads_user_scope_settings() {
        let temp = tempfile::tempdir().unwrap();
        let user_dir = temp.path().join("a2c");
        fs::create_dir_all(&user_dir).unwrap();
        let share = temp.path().join("share");
        fs::create_dir_all(&share).unwrap();
        let settings = serde_json::json!({
            "readRoots": [share.to_str().unwrap(), "relative/ignored"]
        });
        fs::write(
            user_dir.join("settings.json"),
            serde_json::to_vec_pretty(&settings).unwrap(),
        )
        .unwrap();

        let mut env: EnvMap = std::collections::HashMap::new();
        env.insert(
            "XDG_CONFIG_HOME".to_string(),
            temp.path().to_str().unwrap().to_string(),
        );
        let roots = resolve_read_roots(temp.path(), Some(&env));
        // `configured` 是 canonical 形态（Windows 宿主为 `/C:/...`），不能与原生串直接比。
        let expected = normalize_path(share.to_str().unwrap(), native_platform()).unwrap();
        assert!(
            roots.iter().any(|root| root.configured == expected),
            "roots={roots:?}"
        );
        assert!(
            !roots
                .iter()
                .any(|root| root.configured == "relative/ignored"),
            "非法项应在配置期丢弃：{roots:?}"
        );
    }

    #[test]
    fn lexical_fence_accepts_either_form_realpath_fence_only_real() {
        let roots = vec![ReadRoot {
            configured: "/cfg".to_string(),
            real: Some("/real".to_string()),
            disk: None,
        }];
        assert!(lexical_fence("/cfg/x", &roots));
        assert!(lexical_fence("/real/x", &roots));
        assert!(realpath_fence("/real/x", &roots));
        assert!(
            !realpath_fence("/cfg/x", &roots),
            "realpath 围栏只认 real 形态"
        );

        let no_real = vec![ReadRoot {
            configured: "/cfg".to_string(),
            real: None,
            disk: None,
        }];
        assert!(lexical_fence("/cfg/x", &no_real));
        assert!(
            !realpath_fence("/cfg/x", &no_real),
            "real=None 时 realpath 围栏不命中"
        );
    }

    #[cfg(unix)]
    #[test]
    fn realpath_relative_target_with_dotdot_and_midpath_symlink() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let file = outside.join("f.txt");
        fs::write(&file, b"x").unwrap();
        let expected = fs::canonicalize(&file).unwrap();

        // 相对符号链接目标 + `..`：root/link -> ../outside/f.txt
        let link = root.join("link");
        std::os::unix::fs::symlink("../outside/f.txt", &link).unwrap();
        let canonical = normalize_path(link.to_str().unwrap(), native_platform()).unwrap();
        assert_eq!(
            realpath_nonstrict(&canonical, native_platform()).unwrap(),
            expected.to_str().unwrap()
        );

        // 中间分量符号链接：root/mid -> ../outside（目录），再拼 f.txt
        let mid = root.join("mid");
        std::os::unix::fs::symlink("../outside", &mid).unwrap();
        let via_mid = root.join("mid/f.txt");
        let canonical = normalize_path(via_mid.to_str().unwrap(), native_platform()).unwrap();
        assert_eq!(
            realpath_nonstrict(&canonical, native_platform()).unwrap(),
            expected.to_str().unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn realpath_symlink_loop_errors() {
        let temp = tempfile::tempdir().unwrap();
        let a = temp.path().join("a");
        let b = temp.path().join("b");
        std::os::unix::fs::symlink("b", &a).unwrap();
        std::os::unix::fs::symlink("a", &b).unwrap();
        let canonical = normalize_path(a.to_str().unwrap(), native_platform()).unwrap();
        let err = realpath_nonstrict(&canonical, native_platform())
            .expect_err("符号链接环须报错（跳数上限）");
        assert_eq!(
            err.reason(),
            "forbidden",
            "realpath 错误不得映射成 invalid_path"
        );
    }

    /// 非 UTF-8 符号链接目标：结果无法以 Unicode 表示 → `RealpathError::NotUnicode`（reason=forbidden）。
    #[cfg(unix)]
    #[test]
    fn realpath_non_utf8_link_target_is_forbidden() {
        use std::os::unix::ffi::OsStrExt;
        let temp = tempfile::tempdir().unwrap();
        let link = temp.path().join("link");
        let target = std::ffi::OsStr::from_bytes(b"bad\xFFname");
        std::os::unix::fs::symlink(std::path::Path::new(target), &link).unwrap();
        let canonical = normalize_path(link.to_str().unwrap(), native_platform()).unwrap();
        let err =
            realpath_nonstrict(&canonical, native_platform()).expect_err("非 UTF-8 链接目标须报错");
        assert!(
            matches!(&err, RealpathError::NotUnicode { .. }),
            "须命中 NotUnicode 变体（reason 恒 forbidden 无判别力）：{err:?}"
        );
        assert_eq!(err.reason(), "forbidden");
    }

    /// `ENOTDIR`（`file/child`）按"缺失尾"处理：追加分量并继续，不报错（协议处理流程第 4 步）。
    #[cfg(unix)]
    #[test]
    fn realpath_enotdir_is_treated_as_missing_tail() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("file");
        fs::write(&file, b"x").unwrap();
        let child = file.join("child");
        let canonical = normalize_path(child.to_str().unwrap(), native_platform()).unwrap();
        let resolved =
            realpath_nonstrict(&canonical, native_platform()).expect("ENOTDIR 须按缺失尾继续");
        let expected = fs::canonicalize(temp.path()).unwrap().join("file/child");
        assert_eq!(resolved, expected.to_str().unwrap());
    }

    #[test]
    fn resolve_read_roots_merges_trusted_scopes_and_drops_project() {
        let temp = tempfile::tempdir().unwrap();
        let user_dir = temp.path().join("a2c");
        fs::create_dir_all(&user_dir).unwrap();
        let wd = temp.path().join("wd");
        fs::create_dir_all(wd.join(".tfrobot")).unwrap();
        let a = temp.path().join("a");
        let b = temp.path().join("b");
        let evil = temp.path().join("evil");
        for dir in [&a, &b, &evil] {
            fs::create_dir_all(dir).unwrap();
        }
        let (a_s, b_s, evil_s) = (
            a.to_str().unwrap(),
            b.to_str().unwrap(),
            evil.to_str().unwrap(),
        );
        fs::write(
            user_dir.join("settings.json"),
            serde_json::to_vec(&serde_json::json!({"readRoots": [a_s]})).unwrap(),
        )
        .unwrap();
        fs::write(
            wd.join(".tfrobot").join("settings.local.json"),
            serde_json::to_vec(&serde_json::json!({"readRoots": [b_s, a_s]})).unwrap(),
        )
        .unwrap();
        // project scope：协议 MUST 不生效（读侧过滤 + 不入并集）。
        fs::write(
            wd.join(".tfrobot").join("settings.json"),
            serde_json::to_vec(&serde_json::json!({"readRoots": [evil_s]})).unwrap(),
        )
        .unwrap();

        let mut env: EnvMap = std::collections::HashMap::new();
        env.insert(
            "XDG_CONFIG_HOME".to_string(),
            temp.path().to_str().unwrap().to_string(),
        );
        let roots = resolve_read_roots(&wd, Some(&env));
        let configured: Vec<String> = roots.iter().map(|root| root.configured.clone()).collect();
        let a_norm = normalize_path(a_s, native_platform()).unwrap();
        let b_norm = normalize_path(b_s, native_platform()).unwrap();
        let evil_norm = normalize_path(evil_s, native_platform()).unwrap();
        assert!(
            configured.contains(&a_norm),
            "user scope root 须保留：{configured:?}"
        );
        assert!(
            configured.contains(&b_norm),
            "local scope root 须保留：{configured:?}"
        );
        assert!(
            !configured.contains(&evil_norm),
            "project scope root MUST 不生效：{configured:?}"
        );
        assert_eq!(
            configured.iter().filter(|root| **root == a_norm).count(),
            1,
            "跨 scope 去重：{configured:?}"
        );
    }

    /// 🔴 回归（审查发现）：缺失分量**之后**的符号链接必须继续解析。
    ///
    /// `link -> missing/../sub/evil/id`（`missing` 不存在）、`sub/evil -> /outside`。若在首个 `lstat`
    /// 失败处停止并只做词法折叠，会得到 `/repo/sub/evil/id`（围栏通过），而 open 会跟随 `sub/evil`
    /// 越界读到 `/outside/id`。修复后必须解析到 `/outside/id`，realpath 围栏拒绝。
    #[cfg(unix)]
    #[test]
    fn realpath_resolves_symlink_after_missing_component_and_dotdot() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let outside = temp.path().join("outside");
        fs::create_dir_all(repo.join("sub")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let outside_file = outside.join("id");
        fs::write(&outside_file, b"secret").unwrap();
        std::os::unix::fs::symlink(&outside, repo.join("sub").join("evil")).unwrap();
        let link = repo.join("link");
        std::os::unix::fs::symlink("missing/../sub/evil/id", &link).unwrap();

        let canonical = normalize_path(link.to_str().unwrap(), native_platform()).unwrap();
        let resolved = realpath_nonstrict(&canonical, native_platform()).unwrap();
        let expected = fs::canonicalize(&outside_file).unwrap();
        assert_eq!(
            resolved,
            expected.to_str().unwrap(),
            "缺失分量后的符号链接必须继续解析"
        );

        // 词法形态在 root 内（修复前的假通过点），realpath 形态越界必须拒绝。
        let roots = build_read_roots(&[repo.to_str().unwrap().to_string()]);
        let lexical_target = normalize_path(
            repo.join("sub/evil/id").to_str().unwrap(),
            native_platform(),
        )
        .unwrap();
        assert!(lexical_fence(&lexical_target, &roots));
        assert!(
            !realpath_fence(&resolved, &roots),
            "realpath 围栏须拒绝越界目标"
        );
    }

    /// 根自身 realpath 失败（符号链接环）→ 只保留配置形态（`real=None`），不丢根。
    #[cfg(unix)]
    #[test]
    fn build_read_roots_keeps_configured_when_realpath_fails() {
        let temp = tempfile::tempdir().unwrap();
        let a = temp.path().join("a");
        let b = temp.path().join("b");
        std::os::unix::fs::symlink("b", &a).unwrap();
        std::os::unix::fs::symlink("a", &b).unwrap();
        let roots = build_read_roots(&[a.to_str().unwrap().to_string()]);
        assert_eq!(roots.len(), 1, "roots={roots:?}");
        assert_eq!(roots[0].real, None, "环 → realpath 失败，仅配置形态");
        let configured = normalize_path(a.to_str().unwrap(), native_platform()).unwrap();
        assert_eq!(roots[0].configured, configured);
        assert!(lexical_fence(&configured, &roots));
        assert!(
            !realpath_fence(&configured, &roots),
            "real=None 不得通过 realpath 围栏"
        );
    }

    /// 根内悬空链接指向根外：词法围栏通过、realpath 围栏拒绝。
    #[cfg(unix)]
    #[test]
    fn outside_dangling_link_lexical_passes_realpath_rejects() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let missing_target = outside.join("missing.txt");
        let link = root.join("link");
        std::os::unix::fs::symlink(&missing_target, &link).unwrap();

        let roots = build_read_roots(&[root.to_str().unwrap().to_string()]);
        let canonical = normalize_path(link.to_str().unwrap(), native_platform()).unwrap();
        assert!(lexical_fence(&canonical, &roots), "词法形态在根内");
        let resolved = realpath_nonstrict(&canonical, native_platform()).unwrap();
        assert!(
            !realpath_fence(&resolved, &roots),
            "realpath 形态越界须拒绝：{resolved}"
        );
    }

    fn roots_for(root: &Path) -> Vec<ReadRoot> {
        build_read_roots(&[root.to_str().unwrap().to_string()])
    }

    fn write_file(dir: &Path, name: &str, data: &[u8]) -> PathBuf {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, data).unwrap();
        path
    }

    /// 翻转 ASCII 字母大小写（构造大小写变体拼写）。
    fn flip_ascii_case(name: &str) -> String {
        name.chars()
            .map(|ch| {
                if ch.is_ascii_lowercase() {
                    ch.to_ascii_uppercase()
                } else if ch.is_ascii_uppercase() {
                    ch.to_ascii_lowercase()
                } else {
                    ch
                }
            })
            .collect()
    }

    /// 隔离 EnvMap（XDG / HOME 指向测试临时目录，避免读宿主真实配置）。
    fn isolated_env(tmp: &Path) -> EnvMap {
        let xdg = tmp.join("xdg");
        let state = tmp.join("state");
        let home = tmp.join("home");
        fs::create_dir_all(&xdg).unwrap();
        fs::create_dir_all(&state).unwrap();
        fs::create_dir_all(&home).unwrap();
        let mut env = EnvMap::new();
        env.insert(
            "XDG_CONFIG_HOME".to_string(),
            xdg.to_string_lossy().into_owned(),
        );
        env.insert(
            "XDG_STATE_HOME".to_string(),
            state.to_string_lossy().into_owned(),
        );
        env.insert("HOME".to_string(), home.to_string_lossy().into_owned());
        env
    }

    #[test]
    fn resolve_mints_path_handle_with_metadata() {
        let tmp = TempDir::new().unwrap();
        let file = write_file(tmp.path(), "hello.txt", b"hello world");
        let roots = roots_for(tmp.path());
        let resolved = resolve_path(
            file.to_str().unwrap(),
            &roots,
            &ProtectedPaths::default(),
            native_platform(),
        )
        .unwrap();

        assert_eq!(resolved.total_size, 11);
        assert_eq!(resolved.mime_type, "text/plain");
        assert_eq!(
            resolved.sha256,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
        assert!(resolved.canonical_path.ends_with("hello.txt"));

        let decoded = decode_blob_handle(&resolved.blob_handle).unwrap();
        let DecodedHandle::Path(payload) = decoded else {
            panic!("expected path handle");
        };
        assert_eq!(payload.canonical_path, resolved.canonical_path);
        assert_eq!(payload.total_size, 11);
        assert_eq!(payload.sha256, resolved.sha256);
        assert_eq!(payload.mtime, resolved.mtime);
    }

    #[test]
    fn relative_path_is_invalid_path() {
        let tmp = TempDir::new().unwrap();
        let roots = roots_for(tmp.path());
        let error = resolve_path(
            "relative/file.txt",
            &roots,
            &ProtectedPaths::default(),
            native_platform(),
        )
        .unwrap_err();
        assert_eq!(error.reason, ResolveReason::InvalidPath);
        assert_eq!(error.reason_str(), "invalid_path");
    }

    #[cfg(unix)]
    #[test]
    fn mtime_is_floored_before_epoch() {
        let tmp = TempDir::new().unwrap();
        let file = write_file(tmp.path(), "old.txt", b"x");
        // 1970 前 1.5005s → 毫秒 -1500.5 → 向负无穷取整 -1501（截断会给 -1500）。
        let pre_epoch = std::time::UNIX_EPOCH - std::time::Duration::from_nanos(1_500_500_000);
        let handle = fs::File::options().write(true).open(&file).unwrap();
        handle
            .set_times(fs::FileTimes::new().set_modified(pre_epoch))
            .unwrap();
        drop(handle);

        let metadata = fs::metadata(&file).unwrap();
        use std::os::unix::fs::MetadataExt;
        if metadata.mtime_nsec() != 500_000_000 {
            // 文件系统精度不足（秒级），无法验证亚毫秒 floor。
            return;
        }
        let roots = roots_for(tmp.path());
        let resolved = resolve_path(
            file.to_str().unwrap(),
            &roots,
            &ProtectedPaths::default(),
            native_platform(),
        )
        .unwrap();
        assert_eq!(resolved.mtime, -1501);
    }

    #[test]
    fn multi_chunk_sha256_matches_full_hash() {
        let tmp = TempDir::new().unwrap();
        // 2 个 hash 块 + 17 字节：覆盖流式读取的分块边界。
        let size = HASH_CHUNK_BYTES * 2 + 17;
        let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        let file = write_file(tmp.path(), "big.bin", &data);
        let roots = roots_for(tmp.path());
        let resolved = resolve_path(
            file.to_str().unwrap(),
            &roots,
            &ProtectedPaths::default(),
            native_platform(),
        )
        .unwrap();
        assert_eq!(resolved.total_size, size as u64);
        assert_eq!(resolved.sha256, smcp::utils::hash::sha256_hex(&data));
    }

    #[test]
    fn empty_roots_are_forbidden() {
        let tmp = TempDir::new().unwrap();
        let file = write_file(tmp.path(), "a.txt", b"x");
        let error = resolve_path(
            file.to_str().unwrap(),
            &[],
            &ProtectedPaths::default(),
            native_platform(),
        )
        .unwrap_err();
        assert_eq!(error.reason, ResolveReason::Forbidden);
    }

    #[test]
    fn outside_root_is_forbidden_before_existence_probe() {
        let root = TempDir::new().unwrap();
        let other = TempDir::new().unwrap();
        let missing = other.path().join("does-not-exist.txt");
        let roots = roots_for(root.path());
        let error = resolve_path(
            missing.to_str().unwrap(),
            &roots,
            &ProtectedPaths::default(),
            native_platform(),
        )
        .unwrap_err();
        // 越界判定先于存在性探测 → forbidden（而非 not_found），不泄露授权外文件存在性。
        assert_eq!(error.reason, ResolveReason::Forbidden);
    }

    #[test]
    fn missing_file_inside_root_is_not_found() {
        let tmp = TempDir::new().unwrap();
        let roots = roots_for(tmp.path());
        let missing = tmp.path().join("nope.txt");
        let error = resolve_path(
            missing.to_str().unwrap(),
            &roots,
            &ProtectedPaths::default(),
            native_platform(),
        )
        .unwrap_err();
        assert_eq!(error.reason, ResolveReason::NotFound);
    }

    #[test]
    fn directory_is_not_a_file() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("sub");
        fs::create_dir(&dir).unwrap();
        let roots = roots_for(tmp.path());
        let error = resolve_path(
            dir.to_str().unwrap(),
            &roots,
            &ProtectedPaths::default(),
            native_platform(),
        )
        .unwrap_err();
        assert_eq!(error.reason, ResolveReason::NotAFile);
    }

    #[test]
    fn skillenv_is_forbidden_even_inside_root() {
        let tmp = TempDir::new().unwrap();
        let file = write_file(tmp.path(), ".skillenv", b"SECRET=1");
        let roots = roots_for(tmp.path());
        let error = resolve_path(
            file.to_str().unwrap(),
            &roots,
            &ProtectedPaths::default(),
            native_platform(),
        )
        .unwrap_err();
        assert_eq!(error.reason, ResolveReason::Forbidden);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_forbidden() {
        use std::os::unix::fs::symlink;
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let target = write_file(outside.path(), "secret.txt", b"s");
        let link = root.path().join("escape.txt");
        symlink(&target, &link).unwrap();
        let roots = roots_for(root.path());
        let error = resolve_path(
            link.to_str().unwrap(),
            &roots,
            &ProtectedPaths::default(),
            native_platform(),
        )
        .unwrap_err();
        assert_eq!(error.reason, ResolveReason::Forbidden);
    }

    /// 回归（协议订正）：缺失分量之后的符号链接仍须展开，否则围栏通过后 open 可越界读。
    /// 复现：`link -> missing/../sub/evil/id`、`sub/evil -> /outside`、`missing` 不存在。
    #[cfg(unix)]
    #[test]
    fn symlink_after_missing_component_escape_is_forbidden() {
        use std::os::unix::fs::symlink;
        let outside = TempDir::new().unwrap();
        write_file(outside.path(), "id", b"secret");
        let root = TempDir::new().unwrap();
        let sub = root.path().join("sub");
        fs::create_dir(&sub).unwrap();
        symlink(outside.path(), sub.join("evil")).unwrap();
        let link = root.path().join("link");
        symlink("missing/../sub/evil/id", &link).unwrap();

        let roots = roots_for(root.path());
        let link_path = link.to_str().unwrap();
        let error = resolve_path(
            link_path,
            &roots,
            &ProtectedPaths::default(),
            native_platform(),
        )
        .unwrap_err();
        assert_eq!(error.reason, ResolveReason::Forbidden);
        // 判别力：必须由**第 5 步 realpath 围栏**拒绝，而不是第 8 步最终路径复核兜住。
        // 若把 realpath 换回「缺失分量原样拼接」旧算法，canonical 会停在 root/sub/evil/id，
        // 第 5 步通过、由第 8 步（stage=final_path_recheck）拒绝，本断言即失败。
        assert_eq!(error.stage, "realpath_fence");

        // 正对照：把 outside 也纳入 readRoots 后同一路径必须成功，证明 canonical 解析确实到达
        // outside/id（第 5 步拒绝只因授权不含 outside，而非路径解析失败）。
        let roots_with_outside = build_read_roots(&[
            root.path().to_string_lossy().to_string(),
            outside.path().to_string_lossy().to_string(),
        ]);
        let resolved = resolve_path(
            link_path,
            &roots_with_outside,
            &ProtectedPaths::default(),
            native_platform(),
        )
        .unwrap();
        let expected = std::fs::canonicalize(outside.path().join("id")).unwrap();
        assert_eq!(resolved.canonical_path, expected.to_str().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn dangling_symlink_pointing_outside_is_forbidden() {
        use std::os::unix::fs::symlink;
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let link = root.path().join("escape.txt");
        symlink(outside.path().join("missing.txt"), &link).unwrap();
        let roots = roots_for(root.path());
        let error = resolve_path(
            link.to_str().unwrap(),
            &roots,
            &ProtectedPaths::default(),
            native_platform(),
        )
        .unwrap_err();
        // 悬空链接照样解析到授权外 → realpath 围栏拒绝（非 not_found）。
        assert_eq!(error.reason, ResolveReason::Forbidden);
    }

    #[cfg(unix)]
    #[test]
    fn dangling_symlink_inside_root_is_not_found() {
        use std::os::unix::fs::symlink;
        let tmp = TempDir::new().unwrap();
        let link = tmp.path().join("dangling.txt");
        symlink(tmp.path().join("missing.txt"), &link).unwrap();
        let roots = roots_for(tmp.path());
        let error = resolve_path(
            link.to_str().unwrap(),
            &roots,
            &ProtectedPaths::default(),
            native_platform(),
        )
        .unwrap_err();
        // 悬空链接照样解析（第 4 步），目标仍在 root 内 → 第 7 步 not_found。
        assert_eq!(error.reason, ResolveReason::NotFound);
    }

    #[test]
    fn pull_returns_handle_sha_and_bytes() {
        let tmp = TempDir::new().unwrap();
        let file = write_file(tmp.path(), "data.bin", b"0123456789");
        let roots = roots_for(tmp.path());
        let resolved = resolve_path(
            file.to_str().unwrap(),
            &roots,
            &ProtectedPaths::default(),
            native_platform(),
        )
        .unwrap();
        let pulled = resolve_for_pull(
            &match decode_blob_handle(&resolved.blob_handle).unwrap() {
                DecodedHandle::Path(payload) => payload,
                other => panic!("unexpected kind {}", other.kind()),
            },
            &roots,
            &ProtectedPaths::default(),
            native_platform(),
        )
        .unwrap();
        // 拉取期不重算：直接回填句柄内摘要。
        assert_eq!(pulled.sha256, resolved.sha256);
        assert_eq!(pulled.total_size, 10);
        assert_eq!(pulled.slice(0, 4).unwrap(), b"0123");
        assert_eq!(pulled.slice(8, 100).unwrap(), b"89");
    }

    #[test]
    fn pull_after_delete_is_gone() {
        let tmp = TempDir::new().unwrap();
        let file = write_file(tmp.path(), "gone.txt", b"bye");
        let roots = roots_for(tmp.path());
        let resolved = resolve_path(
            file.to_str().unwrap(),
            &roots,
            &ProtectedPaths::default(),
            native_platform(),
        )
        .unwrap();
        let payload = match decode_blob_handle(&resolved.blob_handle).unwrap() {
            DecodedHandle::Path(payload) => payload,
            other => panic!("unexpected kind {}", other.kind()),
        };
        fs::remove_file(&file).unwrap();
        let error = resolve_for_pull(
            &payload,
            &roots,
            &ProtectedPaths::default(),
            native_platform(),
        )
        .unwrap_err();
        assert_eq!(error.reason(), "gone");
    }

    #[test]
    fn pull_after_rewrite_is_gone() {
        let tmp = TempDir::new().unwrap();
        let file = write_file(tmp.path(), "change.txt", b"before");
        let roots = roots_for(tmp.path());
        let resolved = resolve_path(
            file.to_str().unwrap(),
            &roots,
            &ProtectedPaths::default(),
            native_platform(),
        )
        .unwrap();
        let payload = match decode_blob_handle(&resolved.blob_handle).unwrap() {
            DecodedHandle::Path(payload) => payload,
            other => panic!("unexpected kind {}", other.kind()),
        };
        // 原子替换（新 inode / 新 mtime）确保 stat 键不符。
        let replacement = tmp.path().join("replacement.txt");
        fs::write(&replacement, b"after!").unwrap();
        fs::rename(&replacement, &file).unwrap();
        let error = resolve_for_pull(
            &payload,
            &roots,
            &ProtectedPaths::default(),
            native_platform(),
        )
        .unwrap_err();
        assert_eq!(error.reason(), "gone");
    }

    #[test]
    fn pull_rechecks_roots_revocation() {
        let tmp = TempDir::new().unwrap();
        let file = write_file(tmp.path(), "revoked.txt", b"x");
        let roots = roots_for(tmp.path());
        let resolved = resolve_path(
            file.to_str().unwrap(),
            &roots,
            &ProtectedPaths::default(),
            native_platform(),
        )
        .unwrap();
        let payload = match decode_blob_handle(&resolved.blob_handle).unwrap() {
            DecodedHandle::Path(payload) => payload,
            other => panic!("unexpected kind {}", other.kind()),
        };
        let error = resolve_for_pull(&payload, &[], &ProtectedPaths::default(), native_platform())
            .unwrap_err();
        assert_eq!(error.reason(), "forbidden");
    }

    #[test]
    fn pull_rejects_tampered_stat_key_and_sha() {
        let tmp = TempDir::new().unwrap();
        let file = write_file(tmp.path(), "tamper.txt", b"payload");
        let roots = roots_for(tmp.path());
        let resolved = resolve_path(
            file.to_str().unwrap(),
            &roots,
            &ProtectedPaths::default(),
            native_platform(),
        )
        .unwrap();
        let mut payload = match decode_blob_handle(&resolved.blob_handle).unwrap() {
            DecodedHandle::Path(payload) => payload,
            other => panic!("unexpected kind {}", other.kind()),
        };
        payload.total_size += 1;
        let error = resolve_for_pull(
            &payload,
            &roots,
            &ProtectedPaths::default(),
            native_platform(),
        )
        .unwrap_err();
        assert_eq!(error.reason(), "gone");

        let mut invalid = payload;
        invalid.sha256 = "not-a-sha".to_string();
        let error = resolve_for_pull(
            &invalid,
            &roots,
            &ProtectedPaths::default(),
            native_platform(),
        )
        .unwrap_err();
        assert_eq!(error.reason(), "invalid_handle");
    }

    #[test]
    fn build_protected_paths_includes_sdk_config_and_identity() {
        let tmp = TempDir::new().unwrap();
        let xdg = tmp.path().join("xdg");
        let config_dir = xdg.join("a2c");
        fs::create_dir_all(&config_dir).unwrap();
        let settings = config_dir.join("settings.json");
        fs::write(&settings, "{}").unwrap();
        let identity = config_dir.join("computer-id");
        fs::write(&identity, "8-4-4-4-12").unwrap();

        let mut env = EnvMap::new();
        env.insert(
            "XDG_CONFIG_HOME".to_string(),
            xdg.to_string_lossy().into_owned(),
        );
        let protected = build_protected_paths(&[], None, Some(&env), native_platform());

        assert!(
            protected.paths.iter().any(|p| p.ends_with("/a2c")),
            "paths={:?}",
            protected.paths
        );
        assert!(
            protected
                .paths
                .iter()
                .any(|p| p.ends_with("/a2c/settings.json")),
            "paths={:?}",
            protected.paths
        );
        assert!(
            protected
                .paths
                .iter()
                .any(|p| p.ends_with("/a2c/computer-id")),
            "paths={:?}",
            protected.paths
        );
        // 文件身份在 Windows 上不可用（std API 未稳定）→ 仅 Unix 断言。
        #[cfg(unix)]
        assert!(
            !protected.file_ids.is_empty(),
            "regular protected files must contribute file ids"
        );
        // user-scope MCP 声明（可含 env 凭据）。
        assert!(
            protected.paths.iter().any(|p| p.ends_with("/a2c/mcp.json")),
            "paths={:?}",
            protected.paths
        );
        assert!(
            protected
                .paths
                .iter()
                .any(|p| p.ends_with("/a2c/mcp.local.json")),
            "paths={:?}",
            protected.paths
        );
        // policy-scope 企业下发位置（宿主平台形态；路径集合为 canonical 形态）。
        #[cfg(windows)]
        let host_managed = normalize_path(WINDOWS_MANAGED_DIR, PathPlatform::Windows).unwrap();
        #[cfg(not(windows))]
        let host_managed = LINUX_MANAGED_DIR.to_string();
        assert!(
            protected.paths.iter().any(|p| p == &host_managed),
            "managed dir missing: paths={:?}",
            protected.paths
        );
        assert!(
            protected
                .paths
                .iter()
                .any(|p| p.ends_with("/managed-settings.json")),
            "paths={:?}",
            protected.paths
        );
        assert!(
            protected
                .paths
                .iter()
                .any(|p| p.ends_with("/managed-mcp.json")),
            "paths={:?}",
            protected.paths
        );
    }

    #[test]
    fn pull_time_protected_set_is_forbidden() {
        let tmp = TempDir::new().unwrap();
        let file = write_file(tmp.path(), "revoked-by-protection.txt", b"x");
        let roots = roots_for(tmp.path());
        let resolved = resolve_path(
            file.to_str().unwrap(),
            &roots,
            &ProtectedPaths::default(),
            native_platform(),
        )
        .unwrap();
        let payload = match decode_blob_handle(&resolved.blob_handle).unwrap() {
            DecodedHandle::Path(payload) => payload,
            other => panic!("unexpected kind {}", other.kind()),
        };
        // resolve 时未被保护；拉取期拒绝集覆盖其目录 → 4018 forbidden（防御纵深）。
        let env = isolated_env(tmp.path());
        let protected = build_protected_paths(
            &[tmp.path().to_path_buf()],
            None,
            Some(&env),
            native_platform(),
        );
        let error = resolve_for_pull(&payload, &roots, &protected, native_platform()).unwrap_err();
        assert_eq!(error.reason(), "forbidden");
    }

    #[cfg(unix)]
    #[test]
    fn pull_identity_mismatch_is_gone_with_same_size_and_mtime() {
        let tmp = TempDir::new().unwrap();
        let file = write_file(tmp.path(), "identity.txt", b"ABCDE");
        let roots = roots_for(tmp.path());
        let resolved = resolve_path(
            file.to_str().unwrap(),
            &roots,
            &ProtectedPaths::default(),
            native_platform(),
        )
        .unwrap();
        let payload = match decode_blob_handle(&resolved.blob_handle).unwrap() {
            DecodedHandle::Path(payload) => payload,
            other => panic!("unexpected kind {}", other.kind()),
        };

        // 同尺寸 + 复刻 mtime，仅 inode 不同 → 只能由文件身份比对拦下。
        let original_mtime = fs::metadata(&file).unwrap().modified().unwrap();
        let replacement = tmp.path().join("replacement.txt");
        fs::write(&replacement, b"abcde").unwrap();
        let replacement_handle = fs::File::options().write(true).open(&replacement).unwrap();
        replacement_handle
            .set_times(fs::FileTimes::new().set_modified(original_mtime))
            .unwrap();
        drop(replacement_handle);
        fs::rename(&replacement, &file).unwrap();

        let error = resolve_for_pull(
            &payload,
            &roots,
            &ProtectedPaths::default(),
            native_platform(),
        )
        .unwrap_err();
        assert_eq!(error.reason(), "gone");
    }

    /// TOCTOU 闭环：对已打开句柄的最终路径重跑围栏；直接打开授权外文件模拟
    /// `realpath`→`open` 之间被换掉的链接（覆盖最终路径复核分支）。
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn final_path_recheck_rejects_handle_opened_outside_root() {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let outside_file = write_file(outside.path(), "secret.txt", b"x");
        let roots = roots_for(root.path());
        let file = fs::File::open(&outside_file).unwrap();
        let error =
            recheck_final_path(&file, &roots, &ProtectedPaths::default(), native_platform())
                .unwrap_err();
        assert_eq!(error.reason, ResolveReason::Forbidden);
        assert_eq!(error.stage, "final_path_recheck");
    }

    /// 锁定调用点：删除 `run_pipeline` 中的最终路径复核后本测必须失败。
    #[test]
    fn pipeline_invokes_final_path_recheck() {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let outside_file = write_file(outside.path(), "secret.txt", b"x");
        let inside_file = write_file(root.path(), "inside.txt", b"x");
        let roots = roots_for(root.path());
        let normalized = normalize_path(inside_file.to_str().unwrap(), native_platform()).unwrap();
        let fake_outside =
            normalize_path(outside_file.to_str().unwrap(), native_platform()).unwrap();
        let error = run_pipeline_with(
            &normalized,
            &roots,
            &ProtectedPaths::default(),
            native_platform(),
            &|_file| Ok(fake_outside.clone()),
        )
        .unwrap_err();
        assert_eq!(error.reason, ResolveReason::Forbidden);
        assert_eq!(error.stage, "final_path_recheck");
    }

    /// 快照必须是 `canonicalize` 后的磁盘形态，而不是配置拼写：用 symlink root 钉住。
    #[cfg(unix)]
    #[test]
    fn root_disk_snapshot_is_canonicalized_not_configured() {
        use std::os::unix::fs::symlink;
        let tmp = TempDir::new().unwrap();
        let real = tmp.path().join("real");
        fs::create_dir(&real).unwrap();
        let link = tmp.path().join("link");
        symlink(&real, &link).unwrap();
        let roots = build_read_roots(&[link.to_string_lossy().to_string()]);
        let disk = roots[0].disk.clone().expect("disk form");
        let expected = normalize_path(
            std::fs::canonicalize(&link).unwrap().to_str().unwrap(),
            native_platform(),
        )
        .unwrap();
        assert_eq!(
            disk, expected,
            "snapshot must be the canonicalized disk form"
        );
        assert_ne!(
            disk, roots[0].configured,
            "snapshot must not be the configured spelling"
        );
        assert!(path_within_root(&format!("{disk}/file.txt"), &disk));
    }

    /// 快照在 roots 构建时固化：构建后换 root 指向，快照不随之改变（早于目标访问）。
    #[cfg(unix)]
    #[test]
    fn root_disk_snapshot_is_frozen_at_build_time() {
        use std::os::unix::fs::symlink;
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        fs::create_dir(&a).unwrap();
        fs::create_dir(&b).unwrap();
        let link = tmp.path().join("link");
        symlink(&a, &link).unwrap();
        let roots = build_read_roots(&[link.to_string_lossy().to_string()]);
        let frozen = roots[0].disk.clone().expect("disk form");

        // 构建后换指向：roots 中的快照保持旧目标。
        fs::remove_file(&link).unwrap();
        symlink(&b, &link).unwrap();
        assert_eq!(roots[0].disk.as_deref(), Some(frozen.as_str()));
        let b_disk = normalize_path(
            std::fs::canonicalize(&b).unwrap().to_str().unwrap(),
            native_platform(),
        )
        .unwrap();
        assert_ne!(frozen, b_disk, "snapshot must not follow later retargets");
    }

    /// macOS 大小写不敏感卷：配置拼写与磁盘拼写不同（`diskdir` vs `DiskDir`），
    /// 快照取磁盘形态；管线对磁盘最终路径成功（若改用 root.real 则码点不匹配 → 红）。
    #[cfg(target_os = "macos")]
    #[test]
    fn root_disk_snapshot_uses_disk_case() {
        let tmp = TempDir::new().unwrap();
        let disk_dir = tmp.path().join("DiskDir");
        fs::create_dir(&disk_dir).unwrap();
        let variant = tmp.path().join("diskdir");
        if fs::metadata(&variant).is_err() {
            return; // 大小写敏感卷
        }
        let roots = build_read_roots(&[variant.to_string_lossy().to_string()]);
        let snapshot = roots[0].disk.clone().expect("disk form");
        let real = roots[0].real.clone().expect("real form");
        assert_ne!(
            snapshot, real,
            "disk form must differ from realpath spelling"
        );
        assert!(snapshot.ends_with("DiskDir"), "snapshot={snapshot}");
        write_file(&disk_dir, "f.txt", b"x");
        // 用配置拼写（diskdir）发起请求：词法围栏按配置形态命中；最终路径为磁盘形态。
        let input = variant.join("f.txt");
        let resolved = resolve_path(
            input.to_str().unwrap(),
            &roots,
            &ProtectedPaths::default(),
            native_platform(),
        )
        .unwrap();
        // 第 4 步保留请求/配置拼写；管线成功即证明 step-8 用的是磁盘快照。
        assert!(resolved.canonical_path.ends_with("diskdir/f.txt"));
    }

    /// 竞态形状：open 后 root 被换成大小写变体兄弟链接 → 最终路径与快照不匹配 → forbidden。
    #[test]
    fn pipeline_final_path_case_variant_sibling_is_forbidden() {
        let root = TempDir::new().unwrap();
        let inside = write_file(root.path(), "inside.txt", b"x");
        let roots = roots_for(root.path());
        let normalized = normalize_path(inside.to_str().unwrap(), native_platform()).unwrap();
        let snapshot = roots[0].disk.clone().expect("disk form");
        let (parent, name) = snapshot.rsplit_once('/').unwrap();
        let flipped = flip_ascii_case(name);
        if flipped == name {
            return;
        }
        let fake_final = format!("{parent}/{flipped}/inside.txt");
        let error = run_pipeline_with(
            &normalized,
            &roots,
            &ProtectedPaths::default(),
            native_platform(),
            &|_| Ok(fake_final.clone()),
        )
        .unwrap_err();
        assert_eq!(error.reason, ResolveReason::Forbidden);
        assert_eq!(error.stage, "final_path_recheck");
    }

    /// 非 ASCII 路径不受折叠限制：resolve + get_blob 正常。
    #[test]
    fn non_ascii_paths_resolve_and_pull() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("共享");
        fs::create_dir(&dir).unwrap();
        let file = write_file(&dir, "文件.txt", b"content");
        let roots = roots_for(&dir);
        let resolved = resolve_path(
            file.to_str().unwrap(),
            &roots,
            &ProtectedPaths::default(),
            native_platform(),
        )
        .unwrap();
        assert!(resolved.canonical_path.ends_with("文件.txt"));
        let payload = match decode_blob_handle(&resolved.blob_handle).unwrap() {
            DecodedHandle::Path(payload) => payload,
            other => panic!("unexpected kind {}", other.kind()),
        };
        let pulled = resolve_for_pull(
            &payload,
            &roots,
            &ProtectedPaths::default(),
            native_platform(),
        )
        .unwrap();
        assert_eq!(pulled.sha256, resolved.sha256);
    }

    /// step-8 固定拒绝集分支：注入「在 root 内但命中拒绝集」的最终路径 → final_path_recheck 拒绝。
    #[test]
    fn pipeline_final_path_recheck_rejects_protected_path() {
        let root = TempDir::new().unwrap();
        write_file(root.path(), ".skillenv", b"SECRET=1");
        let inside = write_file(root.path(), "inside.txt", b"x");
        let roots = roots_for(root.path());
        let normalized = normalize_path(inside.to_str().unwrap(), native_platform()).unwrap();
        // 用磁盘快照拼接 final path：确保通过根围栏，命中的必须是拒绝集分支。
        let protected_final = format!("{}/.skillenv", roots[0].disk.as_deref().unwrap());
        let error = run_pipeline_with(
            &normalized,
            &roots,
            &ProtectedPaths::default(),
            native_platform(),
            &|_| Ok(protected_final.clone()),
        )
        .unwrap_err();
        assert_eq!(error.reason, ResolveReason::Forbidden);
        assert_eq!(error.stage, "final_path_recheck");
        assert!(
            error.message.contains("rejection set"),
            "must be the protected-set branch: {error}"
        );
    }

    /// 铸造返回的 `canonical_path` 必须是**第 4 步**形态，而不是最终路径的磁盘拼写
    /// （Windows 上最终路径为磁盘大小写；回灌/拉取按码点围栏）。
    #[test]
    fn pipeline_returns_step4_canonical_not_final_path_spelling() {
        let root = TempDir::new().unwrap();
        write_file(root.path(), "Canonical.txt", b"x");
        let roots = roots_for(root.path());
        // 用大小写变体发起请求（大小写敏感卷跳过）：第 4 步保留请求拼写，磁盘 canonical 不同。
        let variant_input = root.path().join("canonical.txt");
        if fs::metadata(&variant_input).is_err() {
            return;
        }
        let normalized =
            normalize_path(variant_input.to_str().unwrap(), native_platform()).unwrap();
        let baseline = resolve_path(
            &normalized,
            &roots,
            &ProtectedPaths::default(),
            native_platform(),
        )
        .unwrap();
        let canonical_native = to_native_path(&normalized, native_platform()).unwrap();
        let disk = std::fs::canonicalize(&canonical_native).unwrap();
        let disk = normalize_path(disk.to_str().unwrap(), native_platform()).unwrap();
        assert_ne!(disk, baseline.canonical_path);

        let (canonical, _file) = run_pipeline_with(
            &normalized,
            &roots,
            &ProtectedPaths::default(),
            native_platform(),
            &|_| Ok(disk.clone()),
        )
        .unwrap();
        assert_eq!(canonical, baseline.canonical_path);
        assert_ne!(canonical, disk);
    }

    /// Windows open errno 映射与目录回退（跨宿主可验证部分）。
    #[test]
    fn open_error_maps_windows_codes_and_directories() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("dir");
        fs::create_dir(&dir).unwrap();
        let native = dir.to_str().unwrap();
        // raw 3 = Win32 PATH_NOT_FOUND；仅当 platform=Windows 时走该分支。
        let not_found = open_error(
            &std::io::Error::from_raw_os_error(3),
            native,
            PathPlatform::Windows,
        );
        assert_eq!(not_found.reason, ResolveReason::NotFound);
        // 权限/共享冲突 + 真实目录 → not_a_file（跨平台目录回退）。
        let dir_error = open_error(
            &std::io::Error::from_raw_os_error(5),
            native,
            native_platform(),
        );
        assert_eq!(dir_error.reason, ResolveReason::NotAFile);
    }

    /// `canonical_path` 可直接回用作 `path`（含大小写变体），且 path 句柄拉取期复核通过。
    #[cfg(unix)]
    #[test]
    fn canonical_path_roundtrip_and_pull_with_case_variants() {
        let tmp = TempDir::new().unwrap();
        let file = write_file(tmp.path(), "CaseFile.txt", b"case");
        let roots = roots_for(tmp.path());
        let first = resolve_path(
            file.to_str().unwrap(),
            &roots,
            &ProtectedPaths::default(),
            native_platform(),
        )
        .unwrap();
        let second = resolve_path(
            &first.canonical_path,
            &roots,
            &ProtectedPaths::default(),
            native_platform(),
        )
        .unwrap();
        assert_eq!(second.canonical_path, first.canonical_path);

        // 大小写变体（macOS 默认不敏感）：大小写敏感 FS 跳过。
        let variant = tmp.path().join("casefile.txt");
        if fs::metadata(&variant).is_err() {
            return;
        }
        let third = resolve_path(
            variant.to_str().unwrap(),
            &roots,
            &ProtectedPaths::default(),
            native_platform(),
        )
        .unwrap();
        let again = resolve_path(
            &third.canonical_path,
            &roots,
            &ProtectedPaths::default(),
            native_platform(),
        )
        .unwrap();
        assert_eq!(again.canonical_path, third.canonical_path);

        let payload = match decode_blob_handle(&third.blob_handle).unwrap() {
            DecodedHandle::Path(payload) => payload,
            other => panic!("unexpected kind {}", other.kind()),
        };
        let pulled = resolve_for_pull(
            &payload,
            &roots,
            &ProtectedPaths::default(),
            native_platform(),
        )
        .unwrap();
        assert_eq!(pulled.sha256, third.sha256);
    }

    /// macOS 默认大小写不敏感：`.CONFIG` 目录变体在 step 6 即按**折叠前缀**命中
    /// `protected_set`（protocol#76），不再依赖 step-8 磁盘形态兜底。
    #[cfg(target_os = "macos")]
    #[test]
    fn protected_prefix_case_variant_forbidden_at_protected_set() {
        let tmp = TempDir::new().unwrap();
        let secret_dir = tmp.path().join("config");
        fs::create_dir(&secret_dir).unwrap();
        write_file(&secret_dir, "settings.json", b"{}");
        let variant = tmp.path().join("CONFIG/settings.json");
        if fs::metadata(&variant).is_err() {
            // 大小写敏感文件系统：本测不适用。
            return;
        }
        let roots = roots_for(tmp.path());
        let env = isolated_env(tmp.path());
        let protected = build_protected_paths(
            std::slice::from_ref(&secret_dir),
            None,
            Some(&env),
            native_platform(),
        );
        let error = resolve_path(
            variant.to_str().unwrap(),
            &roots,
            &protected,
            native_platform(),
        )
        .unwrap_err();
        assert_eq!(error.reason, ResolveReason::Forbidden);
        assert_eq!(error.stage, "protected_set", "{error:?}");
    }

    /// macOS 真实 FS：`.TFROBOT` 目录变体命中拒绝集（protocol#76；与 python#245 对拍形态）。
    #[cfg(target_os = "macos")]
    #[test]
    fn tfrobot_directory_case_variant_is_forbidden_at_protected_set() {
        let tmp = TempDir::new().unwrap();
        let config_dir = tmp.path().join(".tfrobot");
        fs::create_dir(&config_dir).unwrap();
        write_file(&config_dir, "settings.json", b"{}");
        let variant = tmp.path().join(".TFROBOT/settings.json");
        if fs::metadata(&variant).is_err() {
            return; // 大小写敏感文件系统：本测不适用。
        }
        let roots = roots_for(tmp.path());
        let env = isolated_env(tmp.path());
        let protected = build_protected_paths(
            std::slice::from_ref(&config_dir),
            None,
            Some(&env),
            native_platform(),
        );
        let error = resolve_path(
            variant.to_str().unwrap(),
            &roots,
            &protected,
            native_platform(),
        )
        .unwrap_err();
        assert_eq!(error.reason, ResolveReason::Forbidden);
        assert_eq!(error.stage, "protected_set", "{error:?}");
    }

    /// macOS 真实 FS：landing 目录大小写变体 + `.PART` → `protected_set`。
    #[cfg(target_os = "macos")]
    #[test]
    fn landing_case_variant_part_is_forbidden_at_protected_set() {
        let tmp = TempDir::new().unwrap();
        let landing = tmp.path().join("landing");
        fs::create_dir(&landing).unwrap();
        write_file(&landing, "upload.part", b"partial");
        let variant = tmp.path().join("LANDING/upload.part");
        if fs::metadata(&variant).is_err() {
            return; // 大小写敏感文件系统：本测不适用。
        }
        let roots = roots_for(tmp.path());
        let env = isolated_env(tmp.path());
        let protected = build_protected_paths(&[], Some(&landing), Some(&env), native_platform());
        let error = resolve_path(
            variant.to_str().unwrap(),
            &roots,
            &protected,
            native_platform(),
        )
        .unwrap_err();
        assert_eq!(error.reason, ResolveReason::Forbidden);
        assert_eq!(error.stage, "protected_set", "{error:?}");
    }

    #[test]
    fn path_pull_slice_boundaries_match_resolved_blob() {
        let tmp = TempDir::new().unwrap();
        let file = write_file(tmp.path(), "slice.txt", b"01234");
        let roots = roots_for(tmp.path());
        let resolved = resolve_path(
            file.to_str().unwrap(),
            &roots,
            &ProtectedPaths::default(),
            native_platform(),
        )
        .unwrap();
        let payload = match decode_blob_handle(&resolved.blob_handle).unwrap() {
            DecodedHandle::Path(payload) => payload,
            other => panic!("unexpected kind {}", other.kind()),
        };
        let pulled = resolve_for_pull(
            &payload,
            &roots,
            &ProtectedPaths::default(),
            native_platform(),
        )
        .unwrap();
        // EOF probe → 空；越界 → range；超长 → clamp。
        assert_eq!(pulled.slice(5, 1).unwrap(), b"".to_vec());
        assert_eq!(pulled.slice(6, 1).unwrap_err().reason(), "range");
        assert_eq!(pulled.slice(0, 100).unwrap(), b"01234".to_vec());
    }

    /// macOS 默认大小写不敏感：名称谓词在 macOS 折叠（Windows 同），`.SKILLENV` 在
    /// step 6 即拦截（与 python#245 已合入实现一致：macOS 宿主折叠）。
    #[cfg(target_os = "macos")]
    #[test]
    fn skillenv_uppercase_is_forbidden_on_macos() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), ".skillenv", b"SECRET=1");
        let upper = tmp.path().join(".SKILLENV");
        if fs::metadata(&upper).is_err() {
            // 大小写敏感文件系统（非默认 APFS 配置）：本测不适用。
            return;
        }
        let roots = roots_for(tmp.path());
        let error = resolve_path(
            upper.to_str().unwrap(),
            &roots,
            &ProtectedPaths::default(),
            native_platform(),
        )
        .unwrap_err();
        assert_eq!(error.reason, ResolveReason::Forbidden);
        assert_eq!(error.stage, "protected_set", "{error:?}");
    }

    #[cfg(unix)]
    #[test]
    fn fifo_is_not_a_file_without_blocking() {
        let tmp = TempDir::new().unwrap();
        let fifo = tmp.path().join("pipe");
        let c_path = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        // 说明：`O_NONBLOCK` 下打开 FIFO 本身不会阻塞，因此本测断言的是「FIFO 归类为
        // not_a_file」这一可观察结果（第 7 步预检与第 8 步 fstat 兜底行为等价）；预检时序
        // 由实现结构保证，行为上不可单独区分。
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        let roots = roots_for(tmp.path());
        let error = resolve_path(
            fifo.to_str().unwrap(),
            &roots,
            &ProtectedPaths::default(),
            native_platform(),
        )
        .unwrap_err();
        assert_eq!(error.reason, ResolveReason::NotAFile);
    }

    #[cfg(unix)]
    #[test]
    fn dotdot_is_lexical_before_symlink_resolution() {
        use std::os::unix::fs::symlink;
        let tmp = TempDir::new().unwrap();
        let deep = tmp.path().join("sub/deep");
        fs::create_dir_all(&deep).unwrap();
        let link = tmp.path().join("link");
        symlink(&deep, &link).unwrap();
        let target = write_file(tmp.path(), "a.txt", b"a");

        let roots = roots_for(tmp.path());
        // `link/..` 按词法消解为 root（而非物理 `sub`）。
        let input = link.join("../a.txt");
        let resolved = resolve_path(
            input.to_str().unwrap(),
            &roots,
            &ProtectedPaths::default(),
            native_platform(),
        )
        .unwrap();
        let expected = normalize_path(
            realpath_nonstrict(target.to_str().unwrap(), native_platform())
                .unwrap()
                .as_str(),
            native_platform(),
        )
        .unwrap();
        assert_eq!(resolved.canonical_path, expected);
    }

    #[cfg(unix)]
    #[test]
    fn root_configured_via_symlink_is_readable_by_configured_path() {
        use std::os::unix::fs::symlink;
        let tmp = TempDir::new().unwrap();
        let real_root = tmp.path().join("share");
        fs::create_dir_all(&real_root).unwrap();
        write_file(&real_root, "f.txt", b"via-link");
        let link_root = tmp.path().join("share-link");
        symlink(&real_root, &link_root).unwrap();

        let roots = build_read_roots(&[link_root.to_string_lossy().to_string()]);
        let input = link_root.join("f.txt");
        let resolved = resolve_path(
            input.to_str().unwrap(),
            &roots,
            &ProtectedPaths::default(),
            native_platform(),
        )
        .unwrap();
        assert!(resolved.canonical_path.ends_with("share/f.txt"));
        let payload = match decode_blob_handle(&resolved.blob_handle).unwrap() {
            DecodedHandle::Path(payload) => payload,
            other => panic!("unexpected kind {}", other.kind()),
        };
        let pulled = resolve_for_pull(
            &payload,
            &roots,
            &ProtectedPaths::default(),
            native_platform(),
        )
        .unwrap();
        assert_eq!(pulled.slice(0, resolved.total_size).unwrap(), b"via-link");
    }

    #[cfg(unix)]
    #[test]
    fn canonical_path_roundtrip_resolves_to_same_value() {
        let tmp = TempDir::new().unwrap();
        let file = write_file(tmp.path(), "round.txt", b"x");
        let roots = roots_for(tmp.path());
        let first = resolve_path(
            file.to_str().unwrap(),
            &roots,
            &ProtectedPaths::default(),
            native_platform(),
        )
        .unwrap();
        let second = resolve_path(
            &first.canonical_path,
            &roots,
            &ProtectedPaths::default(),
            native_platform(),
        )
        .unwrap();
        assert_eq!(first.canonical_path, second.canonical_path);
        assert_eq!(first.sha256, second.sha256);
    }

    #[cfg(unix)]
    #[test]
    fn unsearchable_parent_is_forbidden_not_not_found() {
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc::geteuid() } == 0 {
            // root 绕过权限位，测试无意义。
            return;
        }
        let tmp = TempDir::new().unwrap();
        let locked = tmp.path().join("locked");
        fs::create_dir(&locked).unwrap();
        let file = write_file(&locked, "f.txt", b"x");
        let roots = roots_for(tmp.path());
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let result = resolve_path(
            file.to_str().unwrap(),
            &roots,
            &ProtectedPaths::default(),
            native_platform(),
        );
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        let error = result.unwrap_err();
        assert_eq!(error.reason, ResolveReason::Forbidden);
    }

    #[cfg(unix)]
    #[test]
    fn hard_link_to_protected_file_is_forbidden() {
        let tmp = TempDir::new().unwrap();
        let config_dir = tmp.path().join("config");
        fs::create_dir(&config_dir).unwrap();
        let protected_file = config_dir.join("settings.json");
        fs::write(&protected_file, b"{\"secret\":true}").unwrap();
        let env = isolated_env(tmp.path());
        let protected = build_protected_paths(
            std::slice::from_ref(&protected_file),
            None,
            Some(&env),
            native_platform(),
        );
        assert!(
            !protected.file_ids.is_empty(),
            "protected file must contribute an identity"
        );

        let share = tmp.path().join("share");
        fs::create_dir(&share).unwrap();
        let hard_link = share.join("innocent.json");
        fs::hard_link(&protected_file, &hard_link).unwrap();
        let roots = roots_for(&share);
        let error = resolve_path(
            hard_link.to_str().unwrap(),
            &roots,
            &protected,
            native_platform(),
        )
        .unwrap_err();
        assert_eq!(error.reason, ResolveReason::Forbidden);
    }

    #[test]
    fn relative_protected_path_is_absolutized() {
        let tmp = TempDir::new().unwrap();
        let env = isolated_env(tmp.path());
        let relative = PathBuf::from("relative-mcp-config.json");
        let protected = build_protected_paths(
            std::slice::from_ref(&relative),
            None,
            Some(&env),
            native_platform(),
        );
        let expected = std::path::absolute(&relative).unwrap();
        let expected = normalize_path(expected.to_str().unwrap(), native_platform()).unwrap();
        assert!(
            protected.paths.contains(&expected),
            "relative protected path must be absolutized: paths={:?}",
            protected.paths
        );
    }

    #[test]
    fn protected_paths_include_legacy_state_dir() {
        let tmp = TempDir::new().unwrap();
        let env = isolated_env(tmp.path());
        let state = tmp.path().join("state");
        let protected = build_protected_paths(&[], None, Some(&env), native_platform());
        let state_dir =
            normalize_path(state.join("a2c").to_str().unwrap(), native_platform()).unwrap();
        let legacy_file = normalize_path(
            state.join("a2c/input-values.json").to_str().unwrap(),
            native_platform(),
        )
        .unwrap();
        assert!(
            protected.paths.contains(&state_dir),
            "state dir missing: {}",
            state_dir
        );
        assert!(
            protected.paths.contains(&legacy_file),
            "legacy input-values.json missing: {}",
            legacy_file
        );
    }

    /// `.skillenv` / `.part` 名称谓词：Windows + macOS 折叠，Linux（大小写敏感）码点。
    #[test]
    fn is_protected_name_folding_is_platform_gated() {
        let protected = ProtectedPaths::default();
        // Windows 恒折叠（任意宿主可测）。
        assert!(is_protected(
            "/x/.SKILLENV",
            &protected,
            PathPlatform::Windows
        ));
        assert!(is_protected(
            "/x/.SkillEnv",
            &protected,
            PathPlatform::Windows
        ));
        // 显式小写恒命中（各平台）。
        assert!(is_protected(
            "/x/.skillenv",
            &protected,
            PathPlatform::Posix
        ));
        assert!(!is_protected(
            "/x/normal.txt",
            &protected,
            PathPlatform::Posix
        ));
        // 宿主为 macOS 时 Posix 也折叠；Linux 下保持码点（不过度拒绝）。
        #[cfg(target_os = "macos")]
        assert!(is_protected(
            "/x/.SKILLENV",
            &protected,
            PathPlatform::Posix
        ));
        #[cfg(not(target_os = "macos"))]
        assert!(!is_protected(
            "/x/.SKILLENV",
            &protected,
            PathPlatform::Posix
        ));

        let landing = ProtectedPaths {
            landing_root: Some("/landing".to_string()),
            ..Default::default()
        };
        assert!(is_protected(
            "/landing/a.PART",
            &landing,
            PathPlatform::Windows
        ));
        assert!(is_protected(
            "/landing/a.part",
            &landing,
            PathPlatform::Posix
        ));
        // macOS 宿主（大小写不敏感）上 Posix 分支也折叠 `.PART`（protocol#76）。
        #[cfg(target_os = "macos")]
        assert!(is_protected(
            "/landing/a.PART",
            &landing,
            PathPlatform::Posix
        ));
        #[cfg(not(target_os = "macos"))]
        assert!(!is_protected(
            "/landing/a.PART",
            &landing,
            PathPlatform::Posix
        ));
        assert!(!is_protected(
            "/other/a.PART",
            &landing,
            PathPlatform::Windows
        ));
    }

    /// protocol#76：拒绝集的**目录前缀**在 Windows + macOS 折叠、Linux 码点；
    /// 组件前缀语义不变（`/repo/conf` 不包含 `/repo/config`）。
    #[test]
    fn protected_prefix_folding_is_platform_gated() {
        let protected = ProtectedPaths {
            paths: vec!["/repo/config".to_string()],
            ..Default::default()
        };
        assert!(is_protected(
            "/REPO/CONFIG/x",
            &protected,
            PathPlatform::Windows
        ));
        assert!(is_protected(
            "/repo/config/x",
            &protected,
            PathPlatform::Windows
        ));
        assert!(!is_protected(
            "/repo/conf/x",
            &protected,
            PathPlatform::Windows
        ));
        #[cfg(not(target_os = "macos"))]
        assert!(!is_protected(
            "/REPO/CONFIG/x",
            &protected,
            PathPlatform::Posix
        ));
        #[cfg(target_os = "macos")]
        assert!(is_protected(
            "/REPO/CONFIG/x",
            &protected,
            PathPlatform::Posix
        ));

        let landing = ProtectedPaths {
            landing_root: Some("/landing".to_string()),
            ..Default::default()
        };
        assert!(is_protected(
            "/LANDING/a.PART",
            &landing,
            PathPlatform::Windows
        ));
        assert!(is_protected(
            "/landing/a.part",
            &landing,
            PathPlatform::Windows
        ));
        assert!(!is_protected(
            "/other/a.PART",
            &landing,
            PathPlatform::Windows
        ));
        #[cfg(not(target_os = "macos"))]
        assert!(!is_protected(
            "/LANDING/a.PART",
            &landing,
            PathPlatform::Posix
        ));
        #[cfg(target_os = "macos")]
        assert!(is_protected(
            "/LANDING/a.PART",
            &landing,
            PathPlatform::Posix
        ));
    }

    /// 反向：readRoots 围栏 MUST 保持码点（不得因拒绝集折叠而全局折叠）。
    #[test]
    fn read_roots_fence_remains_code_point() {
        assert!(!path_within_root("/Repo/x", "/repo"));
        let roots = vec![ReadRoot {
            configured: "/repo".to_string(),
            real: Some("/repo".to_string()),
            disk: None,
        }];
        assert!(!lexical_fence("/Repo/x", &roots));
        assert!(!realpath_fence("/Repo/x", &roots));
    }

    /// macOS 大小写不敏感卷：protected/landing 的**磁盘 canonical 形态**必须入集，
    /// 否则拼写与磁盘不同的受保护目录可被绕过（删掉磁盘插入本测必红）。
    #[cfg(target_os = "macos")]
    #[test]
    fn protected_and_landing_disk_forms_are_added() {
        let tmp = TempDir::new().unwrap();
        let env = isolated_env(tmp.path());
        let disk_cfg = tmp.path().join("Config");
        fs::create_dir(&disk_cfg).unwrap();
        write_file(&disk_cfg, "settings.json", b"{}");
        let variant_cfg = tmp.path().join("config");
        if fs::metadata(&variant_cfg).is_err() {
            return; // 大小写敏感卷
        }
        let protected = build_protected_paths(
            std::slice::from_ref(&variant_cfg),
            None,
            Some(&env),
            native_platform(),
        );
        let disk_form = normalize_path(
            std::fs::canonicalize(&disk_cfg).unwrap().to_str().unwrap(),
            native_platform(),
        )
        .unwrap();
        assert!(
            protected.paths.contains(&disk_form),
            "disk form must be in the rejection set: {disk_form}"
        );
        assert!(is_protected(
            &format!("{disk_form}/settings.json"),
            &protected,
            native_platform()
        ));

        let disk_landing = tmp.path().join("Landing");
        fs::create_dir(&disk_landing).unwrap();
        let variant_landing = tmp.path().join("landing");
        if fs::metadata(&variant_landing).is_err() {
            return;
        }
        let protected =
            build_protected_paths(&[], Some(&variant_landing), Some(&env), native_platform());
        let disk_landing_form = normalize_path(
            std::fs::canonicalize(&disk_landing)
                .unwrap()
                .to_str()
                .unwrap(),
            native_platform(),
        )
        .unwrap();
        assert_eq!(
            protected.landing_root.as_deref(),
            Some(disk_landing_form.as_str()),
            "landing root must prefer the disk form"
        );
        assert!(is_protected(
            &format!("{disk_landing_form}/x.part"),
            &protected,
            native_platform()
        ));
    }

    #[test]
    fn protected_prefix_is_component_wise() {
        // `/data` 不包含 `/database`。
        assert!(!path_within_root("/database/x", "/data"));
        assert!(path_within_root("/data/x", "/data"));
        assert!(path_within_root("/data", "/data"));
    }

    #[test]
    fn protected_directory_is_forbidden_even_inside_root() {
        let tmp = TempDir::new().unwrap();
        let secret_dir = tmp.path().join("config");
        fs::create_dir(&secret_dir).unwrap();
        let file = write_file(&secret_dir, "settings.json", b"{}");
        let roots = roots_for(tmp.path());
        let env = isolated_env(tmp.path());
        // 走真实 builder：配置形态 + realpath 形态双写（macOS `/var` → `/private/var`）。
        let protected = build_protected_paths(
            std::slice::from_ref(&secret_dir),
            None,
            Some(&env),
            native_platform(),
        );
        let error = resolve_path(
            file.to_str().unwrap(),
            &roots,
            &protected,
            native_platform(),
        )
        .unwrap_err();
        assert_eq!(error.reason, ResolveReason::Forbidden);
    }

    #[test]
    fn landing_part_is_forbidden() {
        let tmp = TempDir::new().unwrap();
        let landing = tmp.path().join("landing");
        fs::create_dir(&landing).unwrap();
        let part = write_file(&landing, "upload.part", b"partial");
        let roots = roots_for(tmp.path());
        let env = isolated_env(tmp.path());
        let protected = build_protected_paths(&[], Some(&landing), Some(&env), native_platform());
        let error = resolve_path(
            part.to_str().unwrap(),
            &roots,
            &protected,
            native_platform(),
        )
        .unwrap_err();
        assert_eq!(error.reason, ResolveReason::Forbidden);
    }

    #[cfg(unix)]
    #[test]
    fn pull_after_root_symlink_retarget_is_forbidden() {
        use std::os::unix::fs::symlink;
        let tmp = TempDir::new().unwrap();
        let real_a = tmp.path().join("real-a");
        let real_b = tmp.path().join("real-b");
        fs::create_dir(&real_a).unwrap();
        fs::create_dir(&real_b).unwrap();
        let root_link = tmp.path().join("root");
        symlink(&real_a, &root_link).unwrap();
        write_file(&real_a, "f.txt", b"from-a");

        let roots_a = build_read_roots(&[root_link.to_string_lossy().to_string()]);
        // Agent 以授权根下的路径（经 root 符号链接）发起 resolve。
        let input = root_link.join("f.txt");
        let resolved = resolve_path(
            input.to_str().unwrap(),
            &roots_a,
            &ProtectedPaths::default(),
            native_platform(),
        )
        .unwrap();
        assert!(resolved.canonical_path.ends_with("real-a/f.txt"));
        let payload = match decode_blob_handle(&resolved.blob_handle).unwrap() {
            DecodedHandle::Path(payload) => payload,
            other => panic!("unexpected kind {}", other.kind()),
        };

        // root 符号链接换指向 → 拉取期按当前 realpath 围栏重跑 → forbidden。
        fs::remove_file(&root_link).unwrap();
        symlink(&real_b, &root_link).unwrap();
        let roots_b = build_read_roots(&[root_link.to_string_lossy().to_string()]);
        let error = resolve_for_pull(
            &payload,
            &roots_b,
            &ProtectedPaths::default(),
            native_platform(),
        )
        .unwrap_err();
        assert_eq!(error.reason(), "forbidden");
    }

    #[cfg(unix)]
    #[test]
    fn pull_after_final_symlink_swap_is_forbidden() {
        use std::os::unix::fs::symlink;
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let file = write_file(root.path(), "f.txt", b"inside");
        let roots = roots_for(root.path());
        let resolved = resolve_path(
            file.to_str().unwrap(),
            &roots,
            &ProtectedPaths::default(),
            native_platform(),
        )
        .unwrap();
        let payload = match decode_blob_handle(&resolved.blob_handle).unwrap() {
            DecodedHandle::Path(payload) => payload,
            other => panic!("unexpected kind {}", other.kind()),
        };

        // resolve 后把 canonical_path 换成指向授权外的符号链接 → 重跑第 4–5 步 → forbidden。
        let outside_file = write_file(outside.path(), "secret.txt", b"secret");
        fs::remove_file(&file).unwrap();
        symlink(&outside_file, &file).unwrap();
        let error = resolve_for_pull(
            &payload,
            &roots,
            &ProtectedPaths::default(),
            native_platform(),
        )
        .unwrap_err();
        assert_eq!(error.reason(), "forbidden");
    }
}
