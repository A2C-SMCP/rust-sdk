/*!
* 文件名: staging.rs
* 作者: JQQ
* 创建日期: 2026/06/03
* 最后修改日期: 2026/06/03
* 版权: 2023 JQQ. All rights reserved.
* 依赖: tar, flate2, zip, reqwest, tokio, async-trait, regex, sha2, smcp, crate::skills::*
* 描述: SKILL staging —— 三源物化（user 就地 / marketplace git / mcp）+ 安全解包 + git 子进程
*       对标 Python computer/skills/staging.py / Three-source SKILL staging.
*/

//! SKILL staging：三源物化 / Three-source SKILL staging。
//!
//! 协议依据 / Protocol: a2c-smcp-protocol `skill.md` §3（MCP source 模式 mounted/archive/resources）、
//! §4（包根目录名 = frontmatter.name）、§9.2（staging 跨用户隔离）。对标 Python
//! `a2c_smcp/computer/skills/staging.py`（最大模块）。
//!
//! 三源 / Three sources：
//! - **user**：就地发现 `<home>/user/` + `<workdir>/.tfrobot/skills/`（**不复制**），name = 目录 basename。
//! - **marketplace**：`git clone --depth 1` catalog → 读 `marketplace.json` → 解析 plugin source（5 类）
//!   → 扫 `<plugin>/skills/<skill>/SKILL.md` → 注册（name = `<plugin>:<skill>`）。
//! - **mcp**：经 [`SkillResourceManager`] 枚举 `skill://` 资源，按 mounted/archive/resources 物化到
//!   `<home>/mcp/<bundle_id>/<skill>/`，name = `mcp:<bundle_id>:<frontmatter.name>`（#127：`<server>`
//!   段 = server 唯一身份 `bundle_id` 原样，**非** display 名——见 [`smcp::skill_name`] 模块文档）。
//!
//! 安全 / Security：tar.gz/zip 解包防 zip-slip（[`is_within`]）、拒符号/硬链接、解压累计 / 成员数 cap；
//! git 经 `tokio::process` **显式 argv**（非 shell，杜绝注入）+ `GIT_TERMINAL_PROMPT=0` 非交互。
//!
//! 失败降级铁律 / Failure isolation：任一 SKILL 物化/解析失败 → 记 ERROR、清理半成品、跳过该 SKILL，
//! **不**阻断其余、**不**向上层 panic（§1.5）。
//!
//! 集成接缝 / Integration seams（#67 store / #74 mcp manager 仍 OPEN）：MCP manager 经
//! [`SkillResourceManager`] trait 抽象、`known_marketplaces.json` 持久化经 [`KnownMarketplaceRecorder`]
//! trait 抽象、归档拉取经 [`ArchiveFetcher`] trait（默认 [`DefaultArchiveFetcher`] 用 reqwest）——三者由
//! 各自完成方注入，本模块不耦合其实现。

use std::collections::HashSet;
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

use flate2::read::GzDecoder;
use regex::Regex;
use serde_json::{Map, Value};
use tokio::sync::RwLock;

use smcp::utils::path::{is_within, normalize_lexical};
use smcp::A2CSkillRef;

use crate::settings::redaction::{
    git_url_for_display, redact_git_urls_in_text, untrusted_name_for_display,
};
use crate::settings::{is_valid_marketplace_name, EnvMap};
use crate::skills::frontmatter::parse_skill_frontmatter;
use crate::skills::home::{
    marketplace_skill_dir, mcp_skill_dir, user_dropin_root, SOURCE_MARKETPLACE, SOURCE_USER,
};
use crate::skills::manifest::{
    check_strict_conflict, entry_is_strict, plugin_root_base as resolve_plugin_root_base,
    read_marketplace_manifest, read_plugin_metadata, resolve_plugin_version,
    resolve_skill_override_dirs, PluginManifestError,
};
use crate::skills::naming::{
    synthesize_marketplace_name, synthesize_mcp_name, synthesize_user_name,
};
use crate::skills::registry::SkillRegistry;
use crate::skills::sources::{
    marketplace_clone_url, resolve_plugin_source, GitCloneSpec, ResolvedPluginSource,
    SkillSourceError,
};

/// 包根入口文件名 / package-root entry filename。
pub const SKILL_MD: &str = "SKILL.md";
/// MCP SKILL 资源 URI 前缀 / MCP SKILL resource URI scheme。
pub const SKILL_URI_PREFIX: &str = "skill://";
/// plugin 内 SKILL 子树约定目录（SKILL 协议 §2）/ plugin SKILL convention subdir。
pub const SKILLS_SUBDIR: &str = "skills";
/// 独立 clone plugin 的命名空间目录（以 `.` 起首，与 catalog 物理隔离）/ external-plugin clone namespace。
pub const EXTERNAL_PLUGINS_NS: &str = ".plugins";
/// git clone/pull 默认超时 / Default git timeout（design §2.2：120s）。
pub const DEFAULT_GIT_TIMEOUT: Duration = Duration::from_secs(120);
/// 归档下载超时（reqwest 默认无超时，慢速源会令 staging 无限挂起）/ Archive fetch timeout。
pub const ARCHIVE_FETCH_TIMEOUT: Duration = Duration::from_secs(120);

/// 压缩态下载上限（64 MiB）/ compressed download cap。
pub const MAX_ARCHIVE_DOWNLOAD_BYTES: u64 = 64 * 1024 * 1024;
/// 解压累计字节上限（256 MiB）/ cumulative uncompressed cap。
pub const MAX_EXTRACTED_BYTES: u64 = 256 * 1024 * 1024;
/// 归档成员数上限 / member-count cap。
pub const MAX_ARCHIVE_MEMBERS: usize = 10_000;

/// 合法 MCP source 物化模式 / valid MCP source materialization modes。
const MCP_SOURCE_MODES: &[&str] = &["mounted", "archive", "resources"];

// ssh:// 与 scp-like → https 回退解析（design §2.2）/ ssh→https rewrite patterns。
static SSH_SCHEME_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^ssh://(?:[^@/]+@)?([^/:]+)(?::\d+)?/(.+)$").unwrap());
static SSH_SCP_LIKE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[\w.+-]+@([\w.-]+):(.+)$").unwrap());

/// staging 物化失败（穿越 / 校验 / 格式 / git 等）/ Materialization failure。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct SkillStagingError(pub String);

impl SkillStagingError {
    fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

// ===========================================================================
// 集成接缝 trait / integration-seam traits
// ===========================================================================
/// 归档拉取替身（默认 [`DefaultArchiveFetcher`]）/ Archive fetcher (mockable)。
#[async_trait::async_trait]
pub trait ArchiveFetcher: Send + Sync {
    /// 拉取 `url` 的归档字节 / Fetch the archive bytes for `url`。
    async fn fetch(&self, url: &str) -> Result<Vec<u8>, SkillStagingError>;
}

/// 默认归档拉取（reqwest GET + 下载上限后检）/ Default archive fetch via reqwest。
pub struct DefaultArchiveFetcher;

#[async_trait::async_trait]
impl ArchiveFetcher for DefaultArchiveFetcher {
    async fn fetch(&self, url: &str) -> Result<Vec<u8>, SkillStagingError> {
        let client = reqwest::Client::builder()
            .timeout(ARCHIVE_FETCH_TIMEOUT)
            .build()
            .map_err(|e| SkillStagingError::new(format!("archive client build failed: {e}")))?;
        let mut resp = client
            .get(url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| SkillStagingError::new(format!("archive fetch failed: {e}")))?;
        // 流式累计：超 cap **立即**中止，峰值内存钳在 ~MAX_ARCHIVE_DOWNLOAD_BYTES（对齐 Python
        // iter_chunked）——防半受信 archive_uri 推送数 GB body 在事后 len 检查前耗尽内存。
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| SkillStagingError::new(format!("archive read failed: {e}")))?
        {
            if buf.len() as u64 + chunk.len() as u64 > MAX_ARCHIVE_DOWNLOAD_BYTES {
                return Err(SkillStagingError::new(format!(
                    "archive exceeds download size limit ({MAX_ARCHIVE_DOWNLOAD_BYTES} bytes)"
                )));
            }
            buf.extend_from_slice(&chunk);
        }
        Ok(buf)
    }
}

/// 一个 MCP `skill://` 资源（manager 抽象，解耦 rmcp 具体类型）/ An MCP `skill://` resource。
#[derive(Debug, Clone)]
pub struct McpResource {
    /// 资源 URI（`skill://host/path`）/ resource URI。
    pub uri: String,
    /// 资源 `_meta`（`source` / `mount_dir` / `archive_*` 等）/ resource `_meta`。
    pub meta: Map<String, Value>,
}

/// MCP manager 接缝（#74 注入真实实现）/ MCP manager seam (real impl injected by #74)。
///
/// **身份寻址（#127）**：本接缝以 server 唯一身份 `bundle_id` 标注与寻址，**不使用** display `name`
/// （协议 §身份正交性：`name` 允许碰撞、永不做键）。mcp 形态 SKILL 的 name / `source` / 磁盘落点均由
/// 此 `bundle_id` 构成，故此处若退回 display 名，两个同名 server 会撞名并令其一的 SKILL 隐身。
#[async_trait::async_trait]
pub trait SkillResourceManager: Send + Sync {
    /// 完整消费 cursor 枚举 `(bundle_id, skill 资源)` / Enumerate `(bundle_id, skill resource)`。
    ///
    /// `bundle_id` 给定则仅枚举该 server（**按身份寻址**，非 display 名）。
    async fn list_skill_resources(
        &self,
        bundle_id: Option<&str>,
    ) -> Result<Vec<(String, McpResource)>, SkillStagingError>;

    /// 读取单个资源的字节（content 块已拼接；**按 `bundle_id` 寻址**）/ Read a resource's bytes by bundle_id。
    async fn read_resource(&self, bundle_id: &str, uri: &str)
        -> Result<Vec<u8>, SkillStagingError>;
}

/// `known_marketplaces.json` 物化记录入参 / Inputs for recording a known marketplace。
pub struct KnownMarketplaceRecord<'a> {
    /// marketplace 名 / marketplace name。
    pub name: &'a str,
    /// marketplace git 源 `{type:"git", url}` / the marketplace git source。
    pub source: &'a Value,
    /// clone 落点绝对路径 / clone install location。
    pub install_location: &'a Path,
    /// catalog HEAD commit SHA / catalog HEAD sha。
    pub commit_sha: Option<&'a str>,
    /// 物化记录的 autoUpdate 旗 / autoUpdate flag。
    pub auto_update: bool,
    /// 本次是否实际 clone/pull（决定 lastUpdated 是否刷新）/ whether an actual clone/pull happened。
    pub changed: bool,
}

/// `known_marketplaces.json` 持久化接缝（#67 SET-04 注入真实实现）/ Store seam (real impl by #67)。
pub trait KnownMarketplaceRecorder: Send + Sync {
    /// 记录一次 marketplace 物化（失败应自行降级、不抛）/ Record a marketplace materialization。
    fn record(&self, record: KnownMarketplaceRecord<'_>);
}

// ===========================================================================
// frontmatter → A2CSkillRef 合成 / ref construction
// ===========================================================================
/// YAML 标量 → 字符串（字符串原样；其它走 JSON 表示）/ Scalar to string。
fn scalar_to_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// `string | array` → `Vec<String>`（allowed-tools 归一）/ Normalize to a string list。
fn to_str_list(value: &Value) -> Vec<String> {
    match value {
        Value::Array(arr) => arr.iter().map(scalar_to_string).collect(),
        other => vec![scalar_to_string(other)],
    }
}

/// 把 frontmatter 派生的**可选**字段写入 ref（两源共用）/ Apply frontmatter-derived optional fields。
fn apply_frontmatter_optional_fields(
    skill_ref: &mut A2CSkillRef,
    frontmatter: &Map<String, Value>,
) {
    if let Some(v) = frontmatter.get("license").filter(|v| !v.is_null()) {
        skill_ref.license = Some(scalar_to_string(v));
    }
    if let Some(v) = frontmatter.get("compatibility").filter(|v| !v.is_null()) {
        skill_ref.compatibility = Some(scalar_to_string(v));
    }
    if let Some(v) = frontmatter
        .get("allowed-tools")
        .or_else(|| frontmatter.get("allowed_tools"))
        .filter(|v| !v.is_null())
    {
        skill_ref.allowed_tools = Some(to_str_list(v));
    }
    if let Some(Value::Object(m)) = frontmatter.get("metadata") {
        skill_ref.skill_metadata = Some(Value::Object(m.clone()));
    }
}

/// 取 frontmatter 必选 `description`（缺失 → `None`）/ Required `description`。
fn frontmatter_description(frontmatter: &Map<String, Value>) -> Option<String> {
    frontmatter
        .get("description")
        .filter(|v| !v.is_null())
        .map(scalar_to_string)
        .filter(|s| !s.is_empty())
}

/// 组装基础 ref（必选 4 字段 + 可选）/ Assemble a base ref with required + optional fields。
fn build_ref(
    name: String,
    source: String,
    uri: Option<String>,
    path: &Path,
    description: String,
    frontmatter: &Map<String, Value>,
    version: Option<String>,
) -> A2CSkillRef {
    let mut skill_ref = A2CSkillRef {
        name,
        source,
        uri,
        path: path.to_string_lossy().into_owned(),
        description,
        license: None,
        compatibility: None,
        allowed_tools: None,
        version,
        skill_metadata: None,
    };
    apply_frontmatter_optional_fields(&mut skill_ref, frontmatter);
    skill_ref
}

// ===========================================================================
// 安全解包 / safe extraction
// ===========================================================================
/// 归一化并校验归档成员落点仍在 `dest` 内（防 `..` / 绝对路径穿越）/ Anti-traversal member target。
fn resolved_member_target(dest: &Path, member_name: &str) -> Result<PathBuf, SkillStagingError> {
    let target = normalize_lexical(&dest.join(member_name));
    if !is_within(&target, &normalize_lexical(dest)) {
        return Err(SkillStagingError::new(format!(
            "archive member escapes staging dir: {member_name:?}"
        )));
    }
    Ok(target)
}

/// 分块复制并累计校验解压上限（防 bomb）/ Chunked copy enforcing the cumulative extraction cap。
fn copy_capped<R: Read, W: Write>(
    src: &mut R,
    out: &mut W,
    already: u64,
) -> Result<u64, SkillStagingError> {
    let mut total = already;
    let mut buf = [0u8; 1 << 16];
    loop {
        let n = src
            .read(&mut buf)
            .map_err(|e| SkillStagingError::new(format!("archive read error: {e}")))?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > MAX_EXTRACTED_BYTES {
            return Err(SkillStagingError::new(format!(
                "archive exceeds extracted size limit ({MAX_EXTRACTED_BYTES} bytes)"
            )));
        }
        out.write_all(&buf[..n])
            .map_err(|e| SkillStagingError::new(format!("archive write error: {e}")))?;
    }
    Ok(total)
}

/// 清空并新建目标目录（重物化幂等）/ Clear and recreate the dest dir。
fn reset_dir(dest: &Path) -> Result<(), SkillStagingError> {
    if dest.exists() || dest.is_symlink() {
        let _ = std::fs::remove_dir_all(dest);
    }
    std::fs::create_dir_all(dest).map_err(|e| {
        SkillStagingError::new(format!("reset_dir failed for {}: {e}", dest.display()))
    })
}

fn io_err(e: std::io::Error) -> SkillStagingError {
    SkillStagingError::new(format!("io error: {e}"))
}

/// 安全解 `tar.gz`：拒符号/硬链接、成员数上限、解压累计上限、逐成员校验落点 / Safe tar.gz extraction。
fn extract_tar_gz(data: &[u8], dest: &Path) -> Result<(), SkillStagingError> {
    let mut archive = tar::Archive::new(GzDecoder::new(Cursor::new(data)));
    let mut extracted: u64 = 0;
    let mut count = 0usize;
    for entry in archive.entries().map_err(io_err)? {
        let mut entry = entry.map_err(io_err)?;
        count += 1;
        if count > MAX_ARCHIVE_MEMBERS {
            return Err(SkillStagingError::new(format!(
                "archive has too many members (> {MAX_ARCHIVE_MEMBERS})"
            )));
        }
        let et = entry.header().entry_type();
        if et.is_symlink() || et.is_hard_link() {
            let name = entry
                .path()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            return Err(SkillStagingError::new(format!(
                "archive contains link member (rejected): {name:?}"
            )));
        }
        let name = entry.path().map_err(io_err)?.to_string_lossy().into_owned();
        let target = resolved_member_target(dest, &name)?;
        if et.is_dir() {
            std::fs::create_dir_all(&target).map_err(io_err)?;
        } else if et.is_file() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(io_err)?;
            }
            let mut out = std::fs::File::create(&target).map_err(io_err)?;
            extracted = copy_capped(&mut entry, &mut out, extracted)?;
        }
        // 其它类型（设备/FIFO 等）忽略。
    }
    Ok(())
}

/// 安全解 `zip`：成员数上限、解压累计上限、逐成员校验落点 / Safe zip extraction。
///
/// `zip` crate 不还原 in-zip 符号链接（写为普通文件），故无需 tar 那样的链接拒绝分支；落点校验已防穿越。
fn extract_zip(data: &[u8], dest: &Path) -> Result<(), SkillStagingError> {
    let mut archive = zip::ZipArchive::new(Cursor::new(data))
        .map_err(|e| SkillStagingError::new(format!("zip open failed: {e}")))?;
    if archive.len() > MAX_ARCHIVE_MEMBERS {
        return Err(SkillStagingError::new(format!(
            "archive has too many members ({} > {MAX_ARCHIVE_MEMBERS})",
            archive.len()
        )));
    }
    let mut extracted: u64 = 0;
    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| SkillStagingError::new(format!("zip entry read failed: {e}")))?;
        let name = file.name().to_string();
        let target = resolved_member_target(dest, &name)?;
        if name.ends_with('/') {
            std::fs::create_dir_all(&target).map_err(io_err)?;
        } else {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(io_err)?;
            }
            let mut out = std::fs::File::create(&target).map_err(io_err)?;
            extracted = copy_capped(&mut file, &mut out, extracted)?;
        }
    }
    Ok(())
}

// ===========================================================================
// user 源 DropIn（就地发现，不复制）/ user-source in-place DropIn
// ===========================================================================
/// 枚举发现根下一级、含直接 `SKILL.md` 的 SKILL 目录（排序）/ One-level skill dirs with a direct SKILL.md。
fn iter_user_skill_dirs(root: &Path) -> Vec<PathBuf> {
    if !root.is_dir() {
        return Vec::new();
    }
    let mut children: Vec<PathBuf> = match std::fs::read_dir(root) {
        Ok(rd) => rd.filter_map(Result::ok).map(|e| e.path()).collect(),
        Err(_) => return Vec::new(),
    };
    children.sort();
    children
        .into_iter()
        .filter(|child| child.is_dir() && child.join(SKILL_MD).is_file())
        .collect()
}

/// 读就地 `SKILL.md` frontmatter 组装 user 源 ref（缺 description / 不可读 → `None`）/ Build a user-source ref。
fn build_user_ref(name: &str, skill_dir: &Path) -> Option<A2CSkillRef> {
    let skill_md = skill_dir.join(SKILL_MD);
    let text = match std::fs::read_to_string(&skill_md) {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(name, path = %skill_md.display(), error = %e,
                "user SKILL SKILL.md unreadable, skipped");
            return None;
        }
    };
    let frontmatter = parse_skill_frontmatter(&text);
    let Some(description) = frontmatter_description(&frontmatter) else {
        tracing::error!(name, path = %skill_md.display(),
            "user SKILL frontmatter missing required 'description', skipped");
        return None;
    };
    if let Some(fm_name) = frontmatter.get("name").and_then(Value::as_str) {
        if fm_name != name {
            tracing::debug!(
                basename = name,
                frontmatter_name = fm_name,
                "user SKILL dir basename != frontmatter.name; basename is authoritative"
            );
        }
    }
    Some(build_ref(
        name.to_string(),
        SOURCE_USER.to_string(),
        None,
        &normalize_lexical(skill_dir),
        description,
        &frontmatter,
        None, // user 源无 manifest → 无 version
    ))
}

/// 枚举 user 源 DropIn 并注册进 Registry（就地发现、不复制）/ Discover user-source DropIn skills in place。
///
/// #98：仅扫 home 级全局 DropIn 根 `<home>/user/`（`Computer` 不再持有 workspace；workdir 范围 SKILL 改由
/// MCP `mcp` 源 + `skill://` 承载，对齐 protocol#10 / python-sdk#116）。name = 目录 basename（单段裸名）。
/// 单根下 basename 唯一；同名冲突（不同 basename 归一为同 name）记 WARN、后者胜。返回本次发现并成功注册/刷新
/// 的 name 列表（供 reconciler/watcher diff 孤儿）。
pub fn stage_user_skills(registry: &mut SkillRegistry, home: &Path) -> Vec<String> {
    // name → (ref, 发现目录)；后者覆盖前者。保插入序便于确定性。
    let mut winners: indexmap::IndexMap<String, (A2CSkillRef, PathBuf)> = indexmap::IndexMap::new();
    let root = normalize_lexical(&user_dropin_root(home));
    for skill_dir in iter_user_skill_dirs(&root) {
        let basename = skill_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        let name = match synthesize_user_name(basename) {
            Ok(n) => n,
            Err(e) => {
                tracing::error!(path = %skill_dir.display(), reason = %e.reason,
                    "user DropIn skill dir name invalid, skipped");
                continue;
            }
        };
        let Some(skill_ref) = build_user_ref(&name, &skill_dir) else {
            continue;
        };
        if let Some((_, prev)) = winners.get(&name) {
            tracing::warn!(name = %name, at = %skill_dir.display(), shadows = %prev.display(),
                "user SKILL name collides with earlier DropIn (later entry wins)");
        }
        winners.insert(name, (skill_ref, skill_dir));
    }

    let mut registered = Vec::new();
    for (name, (skill_ref, _)) in winners {
        if registry.register_or_update(skill_ref) {
            registered.push(name);
        }
    }
    registered
}

// ===========================================================================
// git 操作（tokio 子进程，显式 argv）/ git operations
// ===========================================================================
/// 应用非交互 git 环境（`GIT_TERMINAL_PROMPT=0` 等）/ Apply a non-interactive git environment。
fn apply_git_env(cmd: &mut tokio::process::Command, env: Option<&EnvMap>) {
    if let Some(map) = env {
        cmd.env_clear();
        cmd.envs(map);
    }
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    cmd.env("GIT_ASKPASS", "");
    let has_ssh = match env {
        Some(m) => m.contains_key("GIT_SSH_COMMAND"),
        None => std::env::var_os("GIT_SSH_COMMAND").is_some(),
    };
    if !has_ssh {
        cmd.env("GIT_SSH_COMMAND", "ssh -oBatchMode=yes");
    }
}

/// Git stderr 可能回显 clone URL；按本次命令中已知 URL 参数逐一替换为安全展示值。
fn sanitize_git_stderr(args: &[&str], stderr: &str) -> String {
    let known_args_redacted = args
        .iter()
        .filter(|arg| arg.contains("://") || SSH_SCP_LIKE_RE.is_match(arg))
        .fold(stderr.to_string(), |sanitized, raw_url| {
            let display = git_url_for_display(raw_url);
            let mut sanitized = sanitized.replace(*raw_url, &display);
            if SSH_SCP_LIKE_RE.is_match(raw_url) {
                if let Some((endpoint, _)) = raw_url.split_once(':') {
                    sanitized = sanitized.replace(endpoint, &display);
                }
            }
            sanitized
        });
    redact_git_urls_in_text(&known_args_redacted)
}

/// 执行 `git <args>` 并返回 stdout（非零退出 / 超时 → 错误）/ Run `git`, capture stdout。
async fn run_git(
    args: &[&str],
    timeout: Duration,
    env: Option<&EnvMap>,
) -> Result<String, SkillStagingError> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    apply_git_env(&mut cmd, env);
    let child = cmd
        .spawn()
        .map_err(|e| SkillStagingError::new(format!("git spawn failed: {e}")))?;
    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err(SkillStagingError::new(format!("git io error: {e}"))),
        Err(_) => {
            return Err(SkillStagingError::new(format!(
                "git {} timed out after {timeout:?}",
                args.first().copied().unwrap_or("")
            )))
        }
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let err = sanitize_git_stderr(args, stderr.trim());
        return Err(SkillStagingError::new(format!(
            "git {} failed (rc={:?}): {err}",
            args.first().copied().unwrap_or("operation"),
            output.status.code()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// SSH/scp-like URL → `https://` 回退候选（非 ssh 形态 → `None`）/ Rewrite an ssh URL to https。
fn ssh_to_https(url: &str) -> Option<String> {
    let u = url.trim();
    if let Some(c) = SSH_SCHEME_RE.captures(u) {
        return Some(format!("https://{}/{}", &c[1], &c[2]));
    }
    if let Some(c) = SSH_SCP_LIKE_RE.captures(u) {
        return Some(format!("https://{}/{}", &c[1], &c[2]));
    }
    None
}

/// 转 `Vec<String>` → `Vec<&str>`（run_git 入参）/ borrow helper。
fn as_str_args(args: &[String]) -> Vec<&str> {
    args.iter().map(String::as_str).collect()
}

/// `git clone` 到 `dest`（失败 SSH→HTTPS 回退）/ Clone with SSH→HTTPS fallback。
async fn git_clone_with_fallback(
    clone_args: &[String],
    url: &str,
    dest: &Path,
    timeout: Duration,
    env: Option<&EnvMap>,
) -> Result<(), SkillStagingError> {
    let _ = std::fs::remove_dir_all(dest);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(io_err)?;
    }
    let dest_s = dest.to_string_lossy().into_owned();
    let mut full: Vec<String> = vec!["clone".to_string()];
    full.extend(clone_args.iter().cloned());
    full.push("--".to_string());
    full.push(url.to_string());
    full.push(dest_s.clone());
    match run_git(&as_str_args(&full), timeout, env).await {
        Ok(_) => Ok(()),
        Err(first) => {
            let https = ssh_to_https(url);
            match https {
                Some(h) if h != url => {
                    tracing::warn!(
                        url = %git_url_for_display(url),
                        error = %first,
                        https = %git_url_for_display(&h),
                        "git clone failed; retrying via HTTPS"
                    );
                    let _ = std::fs::remove_dir_all(dest);
                    let mut retry: Vec<String> = vec!["clone".to_string()];
                    retry.extend(clone_args.iter().cloned());
                    retry.push("--".to_string());
                    retry.push(h);
                    retry.push(dest_s);
                    run_git(&as_str_args(&retry), timeout, env)
                        .await
                        .map(|_| ())
                }
                _ => Err(first),
            }
        }
    }
}

/// marketplace catalog `git clone --depth 1` / shallow-clone the catalog。
async fn git_clone_marketplace(
    url: &str,
    dest: &Path,
    timeout: Duration,
    env: Option<&EnvMap>,
) -> Result<(), SkillStagingError> {
    git_clone_with_fallback(
        &["--depth".to_string(), "1".to_string()],
        url,
        dest,
        timeout,
        env,
    )
    .await
}

/// 原地 `git pull --ff-only`；失败 → 全量重 clone / In-place pull, full re-clone on failure。
async fn git_refresh_marketplace(
    dest: &Path,
    url: &str,
    timeout: Duration,
    env: Option<&EnvMap>,
) -> Result<(), SkillStagingError> {
    let dest_s = dest.to_string_lossy().into_owned();
    if run_git(&["-C", &dest_s, "pull", "--ff-only"], timeout, env)
        .await
        .is_ok()
    {
        return Ok(());
    }
    tracing::warn!(dest = %dest.display(), "git pull failed; full re-clone");
    git_clone_marketplace(url, dest, timeout, env).await
}

/// 克隆独立 plugin 源（url/github/cnb/git-subdir）→ 返回 plugin 根 / Clone a standalone plugin source。
async fn git_clone_plugin(
    spec: &GitCloneSpec,
    dest: &Path,
    timeout: Duration,
    env: Option<&EnvMap>,
) -> Result<PathBuf, SkillStagingError> {
    let clone_args: Vec<String> = if spec.sha.is_some() {
        vec![
            "--filter=blob:none".to_string(),
            "--no-checkout".to_string(),
        ]
    } else if spec.subdir.is_some() {
        let mut a = vec![
            "--filter=tree:0".to_string(),
            "--sparse".to_string(),
            "--depth".to_string(),
            "1".to_string(),
        ];
        if let Some(r) = &spec.git_ref {
            a.push("--branch".to_string());
            a.push(r.clone());
        }
        a
    } else {
        let mut a = vec!["--depth".to_string(), "1".to_string()];
        if let Some(r) = &spec.git_ref {
            a.push("--branch".to_string());
            a.push(r.clone());
        }
        a
    };

    git_clone_with_fallback(&clone_args, &spec.url, dest, timeout, env).await?;

    let dest_s = dest.to_string_lossy().into_owned();
    if let Some(sha) = &spec.sha {
        run_git(&["-C", &dest_s, "checkout", sha], timeout, env).await?;
    } else if let Some(subdir) = &spec.subdir {
        run_git(
            &["-C", &dest_s, "sparse-checkout", "set", subdir],
            timeout,
            env,
        )
        .await?;
    }

    Ok(match &spec.subdir {
        Some(subdir) => dest.join(subdir),
        None => dest.to_path_buf(),
    })
}

/// `git rev-parse HEAD`（失败 → `None`）/ HEAD sha, `None` on failure。
async fn git_head_sha(dest: &Path, timeout: Duration, env: Option<&EnvMap>) -> Option<String> {
    let dest_s = dest.to_string_lossy().into_owned();
    match run_git(&["-C", &dest_s, "rev-parse", "HEAD"], timeout, env).await {
        Ok(out) => {
            let s = out.trim().to_string();
            (!s.is_empty()).then_some(s)
        }
        Err(e) => {
            tracing::warn!(dest = %dest.display(), error = %e, "git rev-parse HEAD failed");
            None
        }
    }
}

// ===========================================================================
// marketplace 源 git staging / marketplace-source git staging
// ===========================================================================
/// plugin 条目 ID = entry.name / The plugin entry name。
fn entry_plugin_name(entry: &Map<String, Value>) -> Result<String, SkillStagingError> {
    match entry.get("name").and_then(Value::as_str) {
        Some(n) if is_valid_marketplace_name(n.trim()) => Ok(n.trim().to_string()),
        Some(_) => Err(SkillStagingError::new(
            "plugin entry 'name' must be lowercase kebab-case (1-64 characters)",
        )),
        _ => Err(SkillStagingError::new(
            "plugin entry missing required 'name'",
        )),
    }
}

/// 独立 clone plugin 的落点 `<home>/marketplace/.plugins/<mp>/<plugin>/` / External plugin clone dir。
fn external_plugin_dir(home: &Path, marketplace: &str, plugin: &str) -> PathBuf {
    home.join(SOURCE_MARKETPLACE)
        .join(EXTERNAL_PLUGINS_NS)
        .join(marketplace)
        .join(plugin)
}

/// 扫给定**容器目录**下 `<skill>/SKILL.md` 并注册 / Scan SKILL container dirs and register each。
fn scan_and_register_plugin_skills(
    marketplace: &str,
    plugin_name: &str,
    skill_dirs: &[PathBuf],
    version: Option<&str>,
    registry: &mut SkillRegistry,
    seen: &mut HashSet<String>,
) -> Vec<String> {
    // 容器去重（解析后路径），保序。
    let mut unique_dirs: Vec<PathBuf> = Vec::new();
    let mut seen_dirs: HashSet<PathBuf> = HashSet::new();
    for d in skill_dirs {
        let rp = normalize_lexical(d);
        if seen_dirs.insert(rp) {
            unique_dirs.push(d.clone());
        }
    }
    let existing: Vec<&PathBuf> = unique_dirs.iter().filter(|d| d.is_dir()).collect();
    if existing.is_empty() {
        tracing::warn!(
            marketplace,
            plugin = plugin_name,
            "plugin has no SKILL container dir; no SKILLs registered"
        );
        return Vec::new();
    }

    let mut registered = Vec::new();
    for skills_dir in existing {
        let mut entries: Vec<PathBuf> = match std::fs::read_dir(skills_dir) {
            Ok(rd) => rd
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect(),
            Err(_) => continue,
        };
        entries.sort();
        for skill_dir in entries {
            let skill_md = skill_dir.join(SKILL_MD);
            if !skill_md.is_file() {
                continue;
            }
            let skill_basename = skill_dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            let text = match std::fs::read_to_string(&skill_md) {
                Ok(t) => t,
                Err(e) => {
                    tracing::error!(marketplace, plugin = plugin_name, skill = skill_basename,
                        error = %e, "plugin skill SKILL.md unreadable, skipped");
                    continue;
                }
            };
            let frontmatter = parse_skill_frontmatter(&text);
            let Some(description) = frontmatter_description(&frontmatter) else {
                tracing::error!(
                    marketplace,
                    plugin = plugin_name,
                    skill = skill_basename,
                    "missing frontmatter 'description', skipped"
                );
                continue;
            };
            let name = match synthesize_marketplace_name(plugin_name, skill_basename) {
                Ok(n) => n,
                Err(e) => {
                    tracing::error!(marketplace, plugin = plugin_name, skill = skill_basename,
                        reason = %e.reason, "skill name synthesis failed, skipped");
                    continue;
                }
            };
            if seen.contains(&name) {
                tracing::error!(name = %name, "duplicate marketplace SKILL name within run, keeping first");
                continue;
            }
            seen.insert(name.clone());
            let skill_ref = build_ref(
                name.clone(),
                format!("{SOURCE_MARKETPLACE}:{marketplace}"),
                None,
                &normalize_lexical(&skill_dir),
                description,
                &frontmatter,
                version.map(String::from),
            );
            if registry.register_or_update(skill_ref) {
                registered.push(name);
            }
        }
    }
    registered
}

/// 解析 plugin source 并 clone/定位其根，返回 `(plugin_root, version_fallback_sha)` / Resolve & locate a plugin root。
///
/// # Errors
/// source 解析 / clone 失败 → [`SkillSourceError`] / [`SkillStagingError`]（调用方据失败降级）。
// `pub(crate)`：plugin installer（SET-05 #69）独立定位 plugin 根（在 stage 前读 bundled servers /
// 冲突预检 / 记录 installPath）；与 [`stage_one_plugin`] 复用同一定位逻辑、不重复 clone（缓存命中）。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn locate_plugin_root(
    marketplace: &str,
    plugin_name: &str,
    entry: &Map<String, Value>,
    catalog_dir: &Path,
    plugin_root_base: &str,
    home: &Path,
    catalog_sha: Option<&str>,
    refresh: bool,
    timeout: Duration,
    env: Option<&EnvMap>,
) -> Result<(PathBuf, Option<String>), SkillStagingError> {
    let Some(raw_source) = entry.get("source") else {
        return Err(SkillStagingError::new(
            "plugin entry missing required 'source'",
        ));
    };
    let resolved = resolve_plugin_source(raw_source, plugin_root_base)
        .map_err(|e: SkillSourceError| SkillStagingError::new(e.to_string()))?;

    let (plugin_root, version_fallback) = match resolved {
        ResolvedPluginSource::Local(local) => {
            let plugin_root = normalize_lexical(&catalog_dir.join(&local.rel_path));
            if !is_within(&plugin_root, &normalize_lexical(catalog_dir)) {
                return Err(SkillStagingError::new(format!(
                    "relative plugin source escapes marketplace clone: {:?}",
                    local.rel_path
                )));
            }
            (plugin_root, catalog_sha.map(String::from))
        }
        ResolvedPluginSource::Git(spec) => {
            let ext_dir = external_plugin_dir(home, marketplace, plugin_name);
            let mut head = if ext_dir.exists() {
                git_head_sha(&ext_dir, timeout, env).await
            } else {
                None
            };
            // 复用条件：sha 锁版本既有 HEAD==pin 复用、pin 变更重 clone；非 sha 仅 refresh=false 复用。
            let reuse = ext_dir.exists()
                && match &spec.sha {
                    Some(sha) => head.as_deref() == Some(sha.as_str()),
                    None => !refresh,
                };
            if !reuse {
                let _root = git_clone_plugin(&spec, &ext_dir, timeout, env).await?;
                head = git_head_sha(&ext_dir, timeout, env).await;
            }
            let plugin_root = match &spec.subdir {
                Some(subdir) => ext_dir.join(subdir),
                None => ext_dir,
            };
            (plugin_root, head)
        }
    };

    if !plugin_root.is_dir() {
        return Err(SkillStagingError::new(
            "plugin root not found after source resolution",
        ));
    }
    Ok((plugin_root, version_fallback))
}

/// 解析 plugin source → 定位根 → 扫描注册其 SKILL / Resolve source, locate root, scan & register。
#[allow(clippy::too_many_arguments)]
async fn stage_one_plugin(
    marketplace: &str,
    plugin_name: &str,
    entry: &Map<String, Value>,
    catalog_dir: &Path,
    plugin_root_base: &str,
    home: &Path,
    registry: &mut SkillRegistry,
    seen: &mut HashSet<String>,
    catalog_sha: Option<&str>,
    refresh: bool,
    timeout: Duration,
    env: Option<&EnvMap>,
) -> Result<Vec<String>, SkillStagingError> {
    let (plugin_root, version_fallback) = locate_plugin_root(
        marketplace,
        plugin_name,
        entry,
        catalog_dir,
        plugin_root_base,
        home,
        catalog_sha,
        refresh,
        timeout,
        env,
    )
    .await?;
    let plugin_manifest = read_plugin_metadata(&plugin_root);
    // strict mode 冲突检测（§4.4）：strict=false + plugin.json 声明组件 → 错误（外层降级）。
    check_strict_conflict(entry, &plugin_manifest)
        .map_err(|e: PluginManifestError| SkillStagingError::new(e.to_string()))?;
    let version = resolve_plugin_version(entry, &plugin_manifest, version_fallback.as_deref());
    let override_dirs = resolve_skill_override_dirs(
        entry,
        &plugin_manifest,
        &plugin_root,
        entry_is_strict(entry),
    );
    let mut skill_dirs = vec![plugin_root.join(SKILLS_SUBDIR)];
    skill_dirs.extend(override_dirs);
    Ok(scan_and_register_plugin_skills(
        marketplace,
        plugin_name,
        &skill_dirs,
        version.as_deref(),
        registry,
        seen,
    ))
}

/// marketplace 源 staging 选项 / options for [`stage_marketplace_skills`]。
#[derive(Default)]
pub struct MarketplaceStageOptions<'a> {
    /// 仅物化这些 plugin（`None` = 全部）；#62 注入 enabledPlugins ∩ installed / plugin allowlist。
    pub plugin_filter: Option<&'a HashSet<String>>,
    /// 物化记录的 autoUpdate 旗 / autoUpdate flag。
    pub auto_update: bool,
    /// `true` → 已存在 catalog 走 pull、独立 plugin 重 clone（sha-pin 例外）/ refresh mode。
    pub refresh: bool,
    /// 单次 git 操作超时（`None` → [`DEFAULT_GIT_TIMEOUT`]）/ per-op timeout。
    pub timeout: Option<Duration>,
    /// git 子进程环境（`None` → 进程环境）/ git env。
    pub env: Option<&'a EnvMap>,
    /// `known_marketplaces.json` 记录器（`None` → 跳过记录，#67 注入）/ store recorder。
    pub recorder: Option<&'a dyn KnownMarketplaceRecorder>,
}

/// clone/refresh **单个** marketplace 并物化其 plugin SKILL → 注册进 Registry / Stage one marketplace's SKILLs。
///
/// 失败降级铁律：clone/pull/解析/物化失败 → 记 ERROR、该源 / 该 plugin 不入 Registry、**不** panic、**不**阻断其余。
/// 返回本次成功注册 / 刷新的 SKILL name 列表。
pub async fn stage_marketplace_skills(
    name: &str,
    source: &Value,
    registry: &mut SkillRegistry,
    home: &Path,
    options: MarketplaceStageOptions<'_>,
) -> Vec<String> {
    let timeout = options.timeout.unwrap_or(DEFAULT_GIT_TIMEOUT);
    let env = options.env;
    let mut registered: Vec<String> = Vec::new();

    // 防御纵深：name 直接作路径段，仅接受严格 kebab marketplace 名（拒 `..` / `/` 穿越）。
    if !is_valid_marketplace_name(name) {
        tracing::error!(
            marketplace = %untrusted_name_for_display(name),
            "marketplace name invalid (strict-kebab 1-64), skipped"
        );
        return registered;
    }

    let url = match marketplace_clone_url(source) {
        Ok(u) => u,
        Err(_) => {
            tracing::error!(marketplace = name, "invalid marketplace source, skipped");
            return registered;
        }
    };

    let clone_dir = marketplace_skill_dir(home, name, &[]);
    let mut changed = false;
    let clone_result = if clone_dir.exists() {
        if options.refresh {
            changed = true;
            git_refresh_marketplace(&clone_dir, &url, timeout, env).await
        } else {
            Ok(())
        }
    } else {
        changed = true;
        git_clone_marketplace(&url, &clone_dir, timeout, env).await
    };
    if let Err(e) = clone_result {
        tracing::error!(marketplace = name, error = %e, "clone/refresh failed, skipped");
        return registered;
    }

    let commit_sha = git_head_sha(&clone_dir, timeout, env).await;
    if let Some(recorder) = options.recorder {
        recorder.record(KnownMarketplaceRecord {
            name,
            source,
            install_location: &normalize_lexical(&clone_dir),
            commit_sha: commit_sha.as_deref(),
            auto_update: options.auto_update,
            changed,
        });
    }

    let manifest = match read_marketplace_manifest(&clone_dir) {
        Ok(m) => m,
        Err(e) => {
            tracing::error!(marketplace = name, error = %e, "manifest invalid, skipped (clone kept)");
            return registered;
        }
    };

    let plugins = match manifest.get("plugins") {
        Some(Value::Array(arr)) => arr.clone(),
        _ => {
            tracing::error!(
                marketplace = name,
                "manifest 'plugins' is not an array, skipped"
            );
            return registered;
        }
    };

    let plugin_root_base = resolve_plugin_root_base(&manifest);
    let mut seen: HashSet<String> = HashSet::new();
    for entry_val in &plugins {
        let Some(entry) = entry_val.as_object() else {
            tracing::error!(marketplace = name, "non-object plugin entry, skipped");
            continue;
        };
        let plugin_name = match entry_plugin_name(entry) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(marketplace = name, error = %e, "plugin entry invalid, skipped");
                continue;
            }
        };
        if let Some(filter) = options.plugin_filter {
            if !filter.contains(&plugin_name) {
                continue;
            }
        }
        match stage_one_plugin(
            name,
            &plugin_name,
            entry,
            &clone_dir,
            &plugin_root_base,
            home,
            registry,
            &mut seen,
            commit_sha.as_deref(),
            options.refresh,
            timeout,
            env,
        )
        .await
        {
            Ok(names) => registered.extend(names),
            Err(e) => {
                tracing::error!(marketplace = name, plugin = %plugin_name, error = %e,
                    "plugin staging failed, skipped");
            }
        }
    }
    registered
}

// ===========================================================================
// mcp 源物化 / mcp-source materialization
// ===========================================================================
/// `skill://host/a/b` → `a/b`（host 之后路径，无前导 `/`）/ URI path after host。
fn uri_path(uri: &str) -> &str {
    let rest = uri.strip_prefix(SKILL_URI_PREFIX).unwrap_or(uri);
    match rest.split_once('/') {
        Some((_host, path)) => path,
        None => "",
    }
}

/// SKILL 根 URI 的首路径段（provisional staging 目录名）/ first path segment of a SKILL root URI。
fn uri_leaf(uri: &str) -> &str {
    let path = uri_path(uri);
    path.split('/').next().unwrap_or("")
}

/// mounted：复制 `_meta.mount_dir` 本地目录树进 staging（自包含，不留符号链接）/ mounted materialization。
fn materialize_mounted(meta: &Map<String, Value>, dest: &Path) -> Result<(), SkillStagingError> {
    let mount_dir = meta
        .get("mount_dir")
        .and_then(Value::as_str)
        .ok_or_else(|| SkillStagingError::new("mounted source missing 'mount_dir'"))?;
    let src = Path::new(mount_dir);
    if !src.is_dir() {
        return Err(SkillStagingError::new(format!(
            "mounted source dir not found: {mount_dir:?}"
        )));
    }
    reset_dir(dest)?;
    copy_tree_no_symlinks(src, dest)
}

/// 递归复制目录树，跳过符号链接（自包含，沙箱 realpath 不外逃）/ Recursive copy skipping symlinks。
fn copy_tree_no_symlinks(src: &Path, dest: &Path) -> Result<(), SkillStagingError> {
    std::fs::create_dir_all(dest).map_err(io_err)?;
    for entry in std::fs::read_dir(src).map_err(io_err)? {
        let entry = entry.map_err(io_err)?;
        let file_type = entry.file_type().map_err(io_err)?;
        if file_type.is_symlink() {
            continue; // 跳过符号链接
        }
        let target = dest.join(entry.file_name());
        if file_type.is_dir() {
            copy_tree_no_symlinks(&entry.path(), &target)?;
        } else if file_type.is_file() {
            std::fs::copy(entry.path(), &target).map_err(io_err)?;
        }
    }
    Ok(())
}

/// archive：拉取 → 校验 sha256（若声明）→ 按格式安全解包 / archive materialization。
async fn materialize_archive(
    meta: &Map<String, Value>,
    dest: &Path,
    fetcher: &dyn ArchiveFetcher,
) -> Result<(), SkillStagingError> {
    let uri = meta
        .get("archive_uri")
        .and_then(Value::as_str)
        .ok_or_else(|| SkillStagingError::new("archive source missing 'archive_uri'"))?;
    let fmt = meta
        .get("archive_format")
        .and_then(Value::as_str)
        .unwrap_or("");
    if fmt != "tar.gz" && fmt != "zip" {
        return Err(SkillStagingError::new(format!(
            "unsupported archive_format: {fmt:?} (expect 'tar.gz' | 'zip')"
        )));
    }
    let data = fetcher.fetch(uri).await?;
    if data.len() as u64 > MAX_ARCHIVE_DOWNLOAD_BYTES {
        return Err(SkillStagingError::new(format!(
            "archive exceeds download size limit ({} > {MAX_ARCHIVE_DOWNLOAD_BYTES} bytes)",
            data.len()
        )));
    }
    if let Some(expected) = meta.get("archive_sha256").and_then(Value::as_str) {
        let actual = smcp::utils::hash::sha256_hex(&data);
        if actual != expected.to_lowercase() {
            return Err(SkillStagingError::new(format!(
                "archive sha256 mismatch: expected {expected}, got {actual}"
            )));
        }
    }
    reset_dir(dest)?;
    if fmt == "tar.gz" {
        extract_tar_gz(&data, dest)
    } else {
        extract_zip(&data, dest)
    }
}

/// resources：逐个读子资源，按相对路径安全写入 staging / resources materialization。
async fn materialize_resources(
    manager: &dyn SkillResourceManager,
    bundle_id: &str,
    root_uri: &str,
    sub_resources: &[McpResource],
    dest: &Path,
) -> Result<(), SkillStagingError> {
    reset_dir(dest)?;
    let root_path = uri_path(root_uri);
    let mut wrote_any = false;
    for res in sub_resources {
        let sub_path = uri_path(&res.uri);
        // 去掉 "<root_path>/" 前缀。
        let rel = sub_path
            .strip_prefix(root_path)
            .and_then(|r| r.strip_prefix('/'))
            .unwrap_or("");
        if rel.is_empty() {
            continue;
        }
        let target = resolved_member_target(dest, rel)?;
        let bytes = manager.read_resource(bundle_id, &res.uri).await?;
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(io_err)?;
        }
        std::fs::write(&target, &bytes).map_err(io_err)?;
        wrote_any = true;
    }
    if !wrote_any {
        return Err(SkillStagingError::new(format!(
            "resources-mode SKILL has no sub-resources under {root_uri:?}"
        )));
    }
    Ok(())
}

/// 读 frontmatter → 真冲突拒绝 → 校正包根目录名 → register/update（失败 → `None`）/ Finalize one mcp SKILL。
///
/// `bundle_id` 同时构成合成 name 的 `<server>` 段、`source` 与磁盘落点的分组目录（#127：三者同源于
/// server 唯一身份，此前是 raw display 名 + 其规范化结果**两个**标识并行）。
fn finalize_and_register(
    bundle_id: &str,
    meta: &Map<String, Value>,
    staged: &Path,
    root_uri: &str,
    home: &Path,
    registry: &mut SkillRegistry,
    seen_this_run: &mut HashSet<String>,
) -> Option<String> {
    let skill_md = staged.join(SKILL_MD);
    if !skill_md.is_file() {
        tracing::error!(uri = root_uri, "staged SKILL missing SKILL.md, skipped");
        let _ = std::fs::remove_dir_all(staged);
        return None;
    }
    let text = std::fs::read_to_string(&skill_md).ok()?;
    let frontmatter = parse_skill_frontmatter(&text);
    let fm_name = frontmatter
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let description = frontmatter_description(&frontmatter);
    let (Some(fm_name), Some(description)) = (fm_name, description) else {
        tracing::error!(
            uri = root_uri,
            "frontmatter missing required 'name'/'description', skipped"
        );
        let _ = std::fs::remove_dir_all(staged);
        return None;
    };
    let name = match synthesize_mcp_name(bundle_id, fm_name) {
        Ok(n) => n,
        Err(e) => {
            tracing::error!(uri = root_uri, reason = %e.reason, "skill name synthesis failed, skipped");
            let _ = std::fs::remove_dir_all(staged);
            return None;
        }
    };
    // 真冲突（本 run 内不同 SKILL 合成同 name）须在 rename 落盘前拒绝（否则覆盖先到者磁盘文件）。
    if seen_this_run.contains(&name) {
        tracing::error!(name = %name, uri = root_uri,
            "duplicate synthesized SKILL name within staging run, keeping first");
        let _ = std::fs::remove_dir_all(staged);
        return None;
    }
    seen_this_run.insert(name.clone());

    // 包根目录名校正为 frontmatter.name（§4）。
    // 已知边界（与 Python staging.py 逐行一致，跨 SDK 共享）：同一 server 下若 SKILL A 的
    // frontmatter.name 恰等于 SKILL B 的 URI leaf，处理 B 时 materialize 的 reset_dir(staged) 会抹掉
    // A 已 rename 落盘的内容（A 仍在 registry 但磁盘被毁）。seen_this_run 仅对合成 name 去重、不覆盖
    // staging↔final 路径。加固（如 rename 前探测 final_dir 是否为本 run 内其它落点）须协议/Python 先行，
    // 以维持跨 SDK 一致——不在 Rust 单侧修改。
    let final_dir = mcp_skill_dir(home, bundle_id, fm_name);
    if final_dir != staged {
        if final_dir.exists() {
            let _ = std::fs::remove_dir_all(&final_dir);
        }
        if let Some(parent) = final_dir.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::rename(staged, &final_dir) {
            tracing::error!(uri = root_uri, error = %e, "rename to frontmatter.name failed, skipped");
            let _ = std::fs::remove_dir_all(staged);
            return None;
        }
    }

    let version = meta
        .get("version")
        .filter(|v| !v.is_null())
        .map(scalar_to_string);
    let skill_ref = build_ref(
        name.clone(),
        format!("mcp:{bundle_id}"),
        Some(root_uri.to_string()),
        &final_dir,
        description,
        &frontmatter,
        version,
    );
    registry.register_or_update(skill_ref).then_some(name)
}

/// 枚举并物化 mcp 源 SKILL，注册进 Registry / Enumerate, materialize and register mcp-source SKILLs。
///
/// `bundle_id` 给定则仅物化该 server（**按身份寻址**，非 display 名）。`fetcher` 为归档拉取替身
/// （`None` → [`DefaultArchiveFetcher`]）。返回成功注册（或刷新）的 SKILL name 列表。
///
/// **磁盘落点按 `bundle_id` 分组（#127）**：`<home>/mcp/<bundle_id>/<skill>/`。升级前若某 server 的
/// display 名规范化结果 ≠ 其 `bundle_id`（含 CJK / 连续 `__` / 首尾 `_-` / 显式指定 `bundle_id` 等边角
/// 情形），旧 `<home>/mcp/<旧段>/` 目录会**留在盘上**——全量重挂时其 name 经 registry 孤儿对账标 orphan、
/// 对 Agent 不可见（`get_skills` / `get_skill` 不返回），仅占磁盘，可由用户手动清理。常见情形（display 名
/// 本就是 ASCII kebab）缺省生成结果与旧规范化结果一致，落点不变、无残留。
///
/// **两阶段持锁（#77 INT 硬化）**：写锁 **不再** 跨 `materialize_*` 的网络/FS await 持有——
/// `registry` 改为 `&RwLock<SkillRegistry>`，按 SKILL 仅在 `finalize_and_register`（FS rename + 内存注册，
/// 同步无 await）**短持写锁**；`materialize_archive`（归档网络下载）/ `materialize_resources`（MCP
/// `read_resource`）**不持任何 Registry 锁**。期间 `client:get_skills` / `get_skill_ref` 读不再被慢/卡
/// fetch 阻塞（修复 Python 单事件循环掩盖、Rust 暴露的尾延迟竞争）。
///
/// **刻意保持** per-SKILL 的 `materialize → finalize` **交错次序**（而非"全部 materialize 后再批量
/// register"）：finalize 内含 `staged → final_dir` 的 rename，其相对 materialize 的次序决定了
/// `finalize_and_register` 注释所述「A.frontmatter.name == B.uri-leaf」rename 冲突边界的可观测行为
/// （与 Python `staging.py` 逐行一致的跨 SDK 边界，**不在 Rust 单侧改变**）。批量化会重排该次序、
/// 改变此边界，故采用「per-SKILL 短持写锁」取得相同的"锁不跨网络"收益而不动语义。
/// 失败隔离不变：单 SKILL materialize 失败 → skip + 清理 staged。
pub async fn stage_mcp_skills(
    manager: &dyn SkillResourceManager,
    registry: &RwLock<SkillRegistry>,
    home: &Path,
    bundle_id: Option<&str>,
    fetcher: Option<&dyn ArchiveFetcher>,
) -> Result<Vec<String>, SkillStagingError> {
    let default_fetcher = DefaultArchiveFetcher;
    let fetch: &dyn ArchiveFetcher = fetcher.unwrap_or(&default_fetcher);

    let pairs = manager.list_skill_resources(bundle_id).await?;
    let mut by_server: indexmap::IndexMap<String, Vec<McpResource>> = indexmap::IndexMap::new();
    for (bid, res) in pairs {
        by_server.entry(bid).or_default().push(res);
    }

    let mut registered = Vec::new();
    let mut seen_this_run: HashSet<String> = HashSet::new();
    for (bid, resources) in &by_server {
        for res in resources {
            let mode = res.meta.get("source").and_then(Value::as_str).unwrap_or("");
            if !MCP_SOURCE_MODES.contains(&mode) {
                continue; // 非 SKILL 根
            }
            let root_uri = res.uri.as_str();
            let leaf = uri_leaf(root_uri);
            if leaf.is_empty() {
                tracing::error!(
                    uri = root_uri,
                    "skill root URI has no path segment, skipped"
                );
                continue;
            }
            let staged = mcp_skill_dir(home, bid, leaf);
            // phase 1（不持 Registry 锁）：物化到 staged——archive 网络下载 / resources MCP read_resource。
            let materialize = match mode {
                "mounted" => materialize_mounted(&res.meta, &staged),
                "archive" => materialize_archive(&res.meta, &staged, fetch).await,
                _ => {
                    let subs: Vec<McpResource> = resources
                        .iter()
                        .filter(|r| r.uri.starts_with(&format!("{root_uri}/")))
                        .cloned()
                        .collect();
                    materialize_resources(manager, bid, root_uri, &subs, &staged).await
                }
            };
            if let Err(e) = materialize {
                tracing::error!(uri = root_uri, mode, error = %e, "materialize failed, skipped");
                let _ = std::fs::remove_dir_all(&staged);
                continue;
            }
            // phase 2（短持写锁）：finalize（FS rename + 内存注册，同步无 await）。写锁不跨 materialize
            // 网络，并严格保持 per-SKILL materialize→finalize 交错（维持 rename 冲突的跨 SDK 边界）。
            let mut reg = registry.write().await;
            if let Some(name) = finalize_and_register(
                bid,
                &res.meta,
                &staged,
                root_uri,
                home,
                &mut reg,
                &mut seen_this_run,
            ) {
                registered.push(name);
            }
            drop(reg);
        }
    }
    Ok(registered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::fs;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;
    use tracing::instrument::WithSubscriber;

    #[derive(Clone, Default)]
    struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

    impl CapturedLogs {
        fn contents(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    impl std::io::Write for CapturedLogs {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLogs {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    // ---- 安全解包 / safe extraction ----
    #[test]
    fn test_resolved_member_target_rejects_traversal() {
        let tmp = TempDir::new().unwrap();
        assert!(resolved_member_target(tmp.path(), "a/b.txt").is_ok());
        assert!(resolved_member_target(tmp.path(), "../escape").is_err());
        assert!(resolved_member_target(tmp.path(), "a/../../escape").is_err());
        assert!(resolved_member_target(tmp.path(), "/abs/path").is_err());
    }

    fn make_tar_gz(entries: &[(&str, &[u8])]) -> Vec<u8> {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
        for (name, data) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, name, &data[..]).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn test_extract_tar_gz_ok() {
        // 注：tar::Builder 在写入时即拒绝 `..` 成员（无法经安全 API 构造恶意 tar），故 zip-slip 防线由
        // resolved_member_target 的单测（test_resolved_member_target_rejects_traversal）覆盖；此处验证良性解包。
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("out");
        let archive = make_tar_gz(&[
            ("SKILL.md", b"---\nname: x\n---\nbody"),
            ("refs/a.txt", b"hi"),
        ]);
        extract_tar_gz(&archive, &dest).unwrap();
        assert!(dest.join("SKILL.md").is_file());
        assert_eq!(fs::read(dest.join("refs/a.txt")).unwrap(), b"hi");
    }

    #[test]
    fn test_extract_zip_ok_and_slip() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("z");
        let mut buf = Vec::new();
        {
            let mut zw = zip::ZipWriter::new(Cursor::new(&mut buf));
            let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default();
            zw.start_file("SKILL.md", opts).unwrap();
            zw.write_all(b"---\nname: x\n---\nb").unwrap();
            zw.start_file("sub/note.txt", opts).unwrap();
            zw.write_all(b"hi").unwrap();
            zw.finish().unwrap();
        }
        extract_zip(&buf, &dest).unwrap();
        assert!(dest.join("SKILL.md").is_file());
        assert_eq!(fs::read(dest.join("sub/note.txt")).unwrap(), b"hi");
        // zip-slip 成员。
        let mut evil = Vec::new();
        {
            let mut zw = zip::ZipWriter::new(Cursor::new(&mut evil));
            let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default();
            zw.start_file("../escape.txt", opts).unwrap();
            zw.write_all(b"x").unwrap();
            zw.finish().unwrap();
        }
        assert!(extract_zip(&evil, &tmp.path().join("z2")).is_err());
    }

    // ---- ssh→https ----
    #[test]
    fn test_ssh_to_https() {
        assert_eq!(
            ssh_to_https("ssh://git@github.com:22/owner/repo.git").as_deref(),
            Some("https://github.com/owner/repo.git")
        );
        assert_eq!(
            ssh_to_https("git@github.com:owner/repo.git").as_deref(),
            Some("https://github.com/owner/repo.git")
        );
        assert_eq!(ssh_to_https("https://github.com/owner/repo.git"), None);
        assert_eq!(ssh_to_https("file:///tmp/repo"), None);
    }

    #[tokio::test]
    async fn test_git_failure_diagnostics_redact_url_credentials() {
        const SECRET_URL: &str = "https://cnb:FAKE_TOKEN@127.0.0.1:1/repo.git?token=QUERY#FRAGMENT";
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("clone");
        let dest_s = dest.to_string_lossy().into_owned();
        let err = run_git(
            &["clone", "--", SECRET_URL, &dest_s],
            Duration::from_secs(10),
            None,
        )
        .await
        .unwrap_err();
        let public_forms = format!("{err}\n{err:?}");
        assert!(public_forms.contains("git clone failed"));
        for secret in ["cnb", "FAKE_TOKEN", "QUERY", "FRAGMENT", SECRET_URL] {
            assert!(
                !public_forms.contains(secret),
                "git diagnostic leaked {secret:?}: {public_forms}"
            );
        }
    }

    #[test]
    fn test_git_stderr_redacts_abbreviated_scp_endpoint() {
        for (source, stderr, secrets) in [
            (
                "alice@my_host:org/repo.git",
                "alice@my_host: Permission denied (publickey).",
                ["alice", "unused"],
            ),
            (
                "用户@例子.公司:org/repo.git",
                "用户@例子.公司: Permission denied (publickey).",
                ["用户", "例子.公司"],
            ),
            (
                "alice@my_host:org/repo.git",
                "alice@my_host's password:",
                ["alice", "my_host"],
            ),
        ] {
            let rendered = sanitize_git_stderr(&["clone", "--", source], stderr);
            assert!(
                rendered.contains(crate::settings::redaction::REDACTED_GIT_SOURCE),
                "{rendered}"
            );
            for secret in secrets {
                assert!(!rendered.contains(secret), "{rendered}");
            }
        }
    }

    // ---- ref 合成 / ref construction ----
    #[test]
    fn test_apply_frontmatter_optional_fields() {
        let fm = serde_json::json!({
            "license": "MIT",
            "compatibility": "1.0",
            "allowed-tools": ["a", "b"],
            "metadata": {"k": "v"}
        });
        let mut r = build_ref(
            "s".into(),
            "user".into(),
            None,
            Path::new("/abs/s"),
            "d".into(),
            fm.as_object().unwrap(),
            None,
        );
        assert_eq!(r.license.as_deref(), Some("MIT"));
        assert_eq!(r.compatibility.as_deref(), Some("1.0"));
        assert_eq!(
            r.allowed_tools.as_deref(),
            Some(&["a".to_string(), "b".to_string()][..])
        );
        assert!(r.skill_metadata.is_some());
        // 标量 allowed-tools 归一为单元素列表。
        let fm2 = serde_json::json!({"allowed_tools": "solo"});
        apply_frontmatter_optional_fields(&mut r, fm2.as_object().unwrap());
        assert_eq!(r.allowed_tools.as_deref(), Some(&["solo".to_string()][..]));
    }

    // ---- user 源 staging（就地，仅 home 级）/ user-source staging (home-only) ----
    #[test]
    fn test_stage_user_skills_home_only_no_workdir_dimension() {
        // #98：仅扫 <home>/user/；workdir 的 .tfrobot/skills/ 不再是发现维度（对齐 python-sdk#116）。
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        // <home>/user/my-helper/SKILL.md
        let user_skill = home.join("user/my-helper");
        fs::create_dir_all(&user_skill).unwrap();
        fs::write(
            user_skill.join("SKILL.md"),
            "---\nname: my-helper\ndescription: from home\n---\nbody",
        )
        .unwrap();
        // <home>/user/other-skill/SKILL.md
        let other = home.join("user/other-skill");
        fs::create_dir_all(&other).unwrap();
        fs::write(other.join("SKILL.md"), "---\ndescription: other\n---\nb").unwrap();
        // 非 kebab basename → 跳过。
        let bad = home.join("user/Bad_Name");
        fs::create_dir_all(&bad).unwrap();
        fs::write(bad.join("SKILL.md"), "---\ndescription: bad\n---\nb").unwrap();
        // workdir/.tfrobot/skills/proj-a/SKILL.md —— #98 后**不**应被发现。
        let wd_skill = tmp.path().join("wd/.tfrobot/skills/proj-a");
        fs::create_dir_all(&wd_skill).unwrap();
        fs::write(wd_skill.join("SKILL.md"), "---\ndescription: proj\n---\nb").unwrap();

        let mut reg = SkillRegistry::new();
        let names = stage_user_skills(&mut reg, &home);
        assert!(names.contains(&"my-helper".to_string()));
        assert!(names.contains(&"other-skill".to_string()));
        assert!(!names.contains(&"Bad_Name".to_string()));
        assert!(!names.contains(&"proj-a".to_string())); // workdir 维度已移除
        assert!(reg.resolve("proj-a").is_none());
        // home DropIn 就地注册。
        let r = reg.resolve("my-helper").unwrap();
        assert_eq!(r.description, "from home");
        assert_eq!(r.source, "user");
        assert!(r.uri.is_none()); // user 源无 uri
    }

    // ---- marketplace 源 git staging（真实 git，离线 file://）/ real-git integration ----
    fn git(args: &[&str], cwd: &Path) {
        let out = std::process::Command::new("git")
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

    #[tokio::test]
    async fn test_stage_marketplace_skills_relative_plugin() {
        let tmp = TempDir::new().unwrap();
        // 构造源仓库：.tfrobot-plugin/marketplace.json + plugins/audit/skills/code-review/SKILL.md
        let repo = tmp.path().join("repo");
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
        git(&["init", "-q"], &repo);
        git(&["add", "-A"], &repo);
        git(&["commit", "-qm", "init"], &repo);

        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let url = format!("file://{}", repo.display());
        let source = serde_json::json!({"type": "git", "url": url});
        let mut reg = SkillRegistry::new();
        let names = stage_marketplace_skills(
            "acme",
            &source,
            &mut reg,
            &home,
            MarketplaceStageOptions::default(),
        )
        .await;
        assert_eq!(names, vec!["audit:code-review".to_string()]);
        let r = reg.resolve("audit:code-review").unwrap();
        assert_eq!(r.source, "marketplace:acme");
        assert_eq!(r.description, "review code");
        assert!(r.uri.is_none());
    }

    #[tokio::test]
    async fn test_plugin_source_error_log_redacts_url_credentials() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join(".tfrobot-plugin")).unwrap();
        fs::write(
            repo.join(".tfrobot-plugin/marketplace.json"),
            r#"{"plugins":[
                {"name":"audit","source":{"source":"url","url":"ftp://cnb:FAKE_TOKEN@example.com/org/repo.git?token=QUERY#FRAGMENT"}},
                {"name":"prefix","source":"key=https://user:PW_PREFIX@example.com/org/repo.git"},
                {"name":"scp","source":"key=git@example.com:org/repo.git"},
                {"name":"x=https://alice:PW_NAME@example.com/repo.git","source":{"source":"url","url":"bogus"}},
                {"name":"local-leak","source":"./x=https://alice:PW_LOCAL@example.com/repo.git"}
            ]}"#,
        )
        .unwrap();
        git(&["init", "-q"], &repo);
        git(&["add", "-A"], &repo);
        git(&["commit", "-qm", "init"], &repo);

        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let source =
            serde_json::json!({"type": "git", "url": format!("file://{}", repo.display())});
        let logs = CapturedLogs::default();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_target(false)
            .with_writer(logs.clone())
            .finish();
        let mut registry = SkillRegistry::new();
        let names = stage_marketplace_skills(
            "acme",
            &source,
            &mut registry,
            &home,
            MarketplaceStageOptions::default(),
        )
        .with_subscriber(subscriber)
        .await;

        assert!(names.is_empty());
        let rendered = logs.contents();
        assert!(
            rendered.contains("plugin staging failed, skipped"),
            "{rendered}"
        );
        for secret in [
            "cnb",
            "FAKE_TOKEN",
            "QUERY",
            "FRAGMENT",
            "user",
            "PW_PREFIX",
            "PW_NAME",
            "PW_LOCAL",
            "git@example.com",
        ] {
            assert!(!rendered.contains(secret), "{rendered}");
        }
    }

    #[tokio::test]
    async fn test_invalid_marketplace_name_log_redacts_embedded_credentials() {
        let tmp = TempDir::new().unwrap();
        let logs = CapturedLogs::default();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_target(false)
            .with_writer(logs.clone())
            .finish();
        let mut registry = SkillRegistry::new();
        let names = stage_marketplace_skills(
            "x=https://public.example/a=https://user2:PW_TWO@secret.example/repo.git",
            &serde_json::json!({"type": "git", "url": "https://example.com/repo.git"}),
            &mut registry,
            tmp.path(),
            MarketplaceStageOptions::default(),
        )
        .with_subscriber(subscriber)
        .await;

        assert!(names.is_empty());
        let rendered = logs.contents();
        assert!(rendered.contains("marketplace name invalid"), "{rendered}");
        for secret in ["user2", "PW_TWO"] {
            assert!(!rendered.contains(secret), "{rendered}");
        }
    }

    // ---- mcp 源物化（fake manager，mounted 模式）/ mcp materialization with a fake manager ----
    struct FakeManager {
        mount_src: PathBuf,
    }

    #[async_trait::async_trait]
    impl SkillResourceManager for FakeManager {
        async fn list_skill_resources(
            &self,
            _server: Option<&str>,
        ) -> Result<Vec<(String, McpResource)>, SkillStagingError> {
            let mut meta = Map::new();
            meta.insert("source".into(), Value::String("mounted".into()));
            meta.insert(
                "mount_dir".into(),
                Value::String(self.mount_src.to_string_lossy().into_owned()),
            );
            meta.insert("version".into(), Value::String("2.1".into()));
            Ok(vec![(
                "tfrobot-tools".to_string(),
                McpResource {
                    uri: "skill://tfrobot-tools/raw-leaf".to_string(),
                    meta,
                },
            )])
        }

        async fn read_resource(
            &self,
            _server: &str,
            _uri: &str,
        ) -> Result<Vec<u8>, SkillStagingError> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn test_stage_mcp_mounted_canonicalizes_dir_name() {
        let tmp = TempDir::new().unwrap();
        // 被挂载的源目录：SKILL.md frontmatter.name = real-name（包根名应被校正）。
        let mount = tmp.path().join("mount");
        fs::create_dir_all(&mount).unwrap();
        fs::write(
            mount.join("SKILL.md"),
            "---\nname: real-name\ndescription: mounted skill\n---\nbody",
        )
        .unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let manager = FakeManager { mount_src: mount };
        // #77：stage_mcp_skills 现取 `&RwLock<SkillRegistry>`（内部 per-skill 短持写锁）。
        let reg = RwLock::new(SkillRegistry::new());
        let names = stage_mcp_skills(&manager, &reg, &home, None, None)
            .await
            .unwrap();
        // name = mcp:<server>:<frontmatter.name>，包根目录名校正为 real-name。
        assert_eq!(names, vec!["mcp:tfrobot-tools:real-name".to_string()]);
        let reg = reg.into_inner();
        let r = reg.resolve("mcp:tfrobot-tools:real-name").unwrap();
        assert_eq!(r.source, "mcp:tfrobot-tools");
        assert_eq!(r.version.as_deref(), Some("2.1"));
        assert!(r.path.ends_with("mcp/tfrobot-tools/real-name"));
        assert!(Path::new(&r.path).join("SKILL.md").is_file());
    }

    // ---- #188：URI 前缀归属优先于 _meta.source（镜像 python-sdk#188）----
    /// 可配置 stub：全量回放 `(bundle_id, resource)` 清单 + `uri → bytes` 读取映射。
    struct StubManager {
        resources: Vec<(String, McpResource)>,
        reads: HashMap<String, Vec<u8>>,
    }

    #[async_trait::async_trait]
    impl SkillResourceManager for StubManager {
        async fn list_skill_resources(
            &self,
            bundle_id: Option<&str>,
        ) -> Result<Vec<(String, McpResource)>, SkillStagingError> {
            Ok(self
                .resources
                .iter()
                .filter(|(bid, _)| bundle_id.is_none_or(|f| f == bid))
                .cloned()
                .collect())
        }

        async fn read_resource(
            &self,
            _bundle_id: &str,
            uri: &str,
        ) -> Result<Vec<u8>, SkillStagingError> {
            self.reads
                .get(uri)
                .cloned()
                .ok_or_else(|| SkillStagingError::new(format!("no read for {uri}")))
        }
    }

    fn mcp_res(uri: &str, meta: Map<String, Value>) -> McpResource {
        McpResource {
            uri: uri.to_string(),
            meta,
        }
    }

    fn source_meta(source: &str) -> Map<String, Value> {
        let mut meta = Map::new();
        meta.insert("source".into(), Value::String(source.into()));
        meta
    }

    /// 日志断言用 subscriber：捕获全部级别（含 DEBUG），无时间戳/目标噪声。
    fn capture_subscriber(logs: CapturedLogs) -> impl tracing::Subscriber {
        tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_target(false)
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(logs)
            .finish()
    }

    /// 建一个 mounted SKILL 源目录（含 SKILL.md），返回其 `_meta` 与目录路径。
    fn make_mount(tmp: &Path, name: &str) -> (Map<String, Value>, PathBuf) {
        let dir = tmp.join(format!("mount_{name}"));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: mounted {name}\n---\nbody"),
        )
        .unwrap();
        let mut meta = source_meta("mounted");
        meta.insert(
            "mount_dir".into(),
            Value::String(dir.to_string_lossy().into_owned()),
        );
        (meta, dir)
    }

    /// #188 规则 1+2：provider 给根+子资源全部打 `_meta.source` → 被根前缀覆盖的子资源
    /// 不再当独立根物化（无 ERROR 刷屏、无 staging 目录抖动）——归属判断优先于 meta 判断。
    /// pre-fix 症状：被覆盖子资源 materialize 时 `reset_dir` 抹掉真根已落盘内容 + ERROR 刷屏。
    #[tokio::test]
    async fn test_stage_mcp_resources_covered_child_with_source_meta_not_materialized_as_root() {
        let root_uri = "skill://h/my-skill";
        let sub1_uri = "skill://h/my-skill/SKILL.md";
        let sub2_uri = "skill://h/my-skill/assets/logo.bin";
        let root = mcp_res(root_uri, source_meta("resources"));
        let sub1 = mcp_res(sub1_uri, source_meta("resources")); // 不合规：子资源携带 source
        let sub2 = mcp_res(sub2_uri, source_meta("resources"));
        let mut reads = HashMap::new();
        reads.insert(
            sub1_uri.to_string(),
            b"---\nname: my-skill\ndescription: covered skill\n---\nbody".to_vec(),
        );
        reads.insert(sub2_uri.to_string(), b"\x00\x01\x02".to_vec());
        let manager = StubManager {
            resources: vec![
                ("srv".to_string(), root),
                ("srv".to_string(), sub1),
                ("srv".to_string(), sub2),
            ],
            reads,
        };
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let logs = CapturedLogs::default();
        let subscriber = capture_subscriber(logs.clone());
        let reg = RwLock::new(SkillRegistry::new());
        let names = stage_mcp_skills(&manager, &reg, &home, None, None)
            .with_subscriber(subscriber)
            .await
            .unwrap();

        assert_eq!(names, vec!["mcp:srv:my-skill".to_string()]); // 仅真根注册一次
        assert_eq!(reg.read().await.len(), 1);
        let r = reg.read().await.resolve("mcp:srv:my-skill").unwrap();
        let path = Path::new(&r.path);
        assert!(path.join("SKILL.md").is_file());
        assert_eq!(fs::read(path.join("assets/logo.bin")).unwrap(), b"\x00\x01\x02");
        // 被覆盖资源：每 bundle 一条汇总 WARNING + 逐条 DEBUG；不得出现任何 ERROR（pre-fix 是 ERROR 刷屏）
        let rendered = logs.contents();
        assert!(!rendered.contains("ERROR"), "{rendered}");
        assert_eq!(
            rendered.matches("covered by other roots").count(),
            1,
            "{rendered}"
        );
        assert!(rendered.contains("(immediate parent)"), "{rendered}");
        assert!(
            rendered.contains(&format!("uri={sub1_uri} coverer={root_uri}")),
            "{rendered}"
        );
        assert!(rendered.contains(sub2_uri), "{rendered}");
    }

    /// #188 嵌套链 A>B>C（均 mounted）：B、C 均被排除（覆盖判定不看覆盖者自身是否被覆盖），
    /// 仅 A 注册；DEBUG 记立即父（B→A、C→B 最长前缀覆盖者）。mounted 无子资源枚举，链式覆盖可干净表达。
    #[tokio::test]
    async fn test_stage_mcp_nested_covered_roots_mounted_only_a_registered() {
        let tmp = TempDir::new().unwrap();
        let (meta_a, _) = make_mount(tmp.path(), "a");
        let (meta_b, _) = make_mount(tmp.path(), "b");
        let (meta_c, _) = make_mount(tmp.path(), "c");
        let manager = StubManager {
            resources: vec![
                ("srv".to_string(), mcp_res("skill://h/a", meta_a)),
                ("srv".to_string(), mcp_res("skill://h/a/b", meta_b)),
                ("srv".to_string(), mcp_res("skill://h/a/b/c", meta_c)),
            ],
            reads: HashMap::new(),
        };
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let logs = CapturedLogs::default();
        let subscriber = capture_subscriber(logs.clone());
        let reg = RwLock::new(SkillRegistry::new());
        let names = stage_mcp_skills(&manager, &reg, &home, None, None)
            .with_subscriber(subscriber)
            .await
            .unwrap();

        assert_eq!(names, vec!["mcp:srv:a".to_string()]); // 仅最外层根注册
        assert_eq!(reg.read().await.len(), 1);
        let rendered = logs.contents();
        assert!(!rendered.contains("ERROR"), "{rendered}");
        assert_eq!(
            rendered.matches("covered by other roots").count(),
            1,
            "{rendered}"
        );
        // 立即父语义：B→A、C→B（最长前缀覆盖者）
        assert!(
            rendered.contains("uri=skill://h/a/b coverer=skill://h/a\n"),
            "{rendered}"
        );
        assert!(
            rendered.contains("uri=skill://h/a/b/c coverer=skill://h/a/b\n"),
            "{rendered}"
        );
    }

    /// 无 `_meta.source` 的 skill:// 资源（子资源 / 未声明）→ 非 SKILL 根 → 跳过。
    /// 守护循环头重构：skip 逻辑从 `continue` 移入根归属切分后语义不变。
    #[tokio::test]
    async fn test_stage_mcp_resource_without_source_meta_skipped() {
        let plain = McpResource {
            uri: "skill://h/not-a-root".to_string(),
            meta: Map::new(),
        };
        let manager = StubManager {
            resources: vec![("srv".to_string(), plain)],
            reads: HashMap::new(),
        };
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let reg = RwLock::new(SkillRegistry::new());
        let names = stage_mcp_skills(&manager, &reg, &home, None, None)
            .await
            .unwrap();
        assert!(names.is_empty());
        assert_eq!(reg.read().await.len(), 0);
    }

    // ---- #77：写锁不跨 materialize 网络 fetch（archive 模式回归守卫）----
    /// 列出单个 `archive` 源 SKILL（字节由 fetcher 决定）/ lists one archive-source SKILL。
    struct ArchiveManager;

    #[async_trait::async_trait]
    impl SkillResourceManager for ArchiveManager {
        async fn list_skill_resources(
            &self,
            _server: Option<&str>,
        ) -> Result<Vec<(String, McpResource)>, SkillStagingError> {
            let mut meta = Map::new();
            meta.insert("source".into(), Value::String("archive".into()));
            meta.insert(
                "archive_uri".into(),
                Value::String("https://example.test/skill.tar.gz".into()),
            );
            meta.insert("archive_format".into(), Value::String("tar.gz".into()));
            Ok(vec![(
                "tfrobot-tools".to_string(),
                McpResource {
                    uri: "skill://tfrobot-tools/pkg".to_string(),
                    meta,
                },
            )])
        }

        async fn read_resource(
            &self,
            _server: &str,
            _uri: &str,
        ) -> Result<Vec<u8>, SkillStagingError> {
            Ok(Vec::new())
        }
    }

    /// fetch（模拟网络下载）期间断言 Registry **未被写锁占用**——直接证明 #77：写锁不跨 materialize 网络。
    /// 旧实现（调用方跨整个 stage 持写锁）会令此 `try_read` 返回 `Err`，从而捕获回归。
    struct AssertUnlockedFetcher {
        registry: std::sync::Arc<RwLock<SkillRegistry>>,
        body: Vec<u8>,
    }

    #[async_trait::async_trait]
    impl ArchiveFetcher for AssertUnlockedFetcher {
        async fn fetch(&self, _url: &str) -> Result<Vec<u8>, SkillStagingError> {
            assert!(
                self.registry.try_read().is_ok(),
                "#77 regression: skill_registry MUST NOT be write-locked during archive fetch"
            );
            Ok(self.body.clone())
        }
    }

    #[tokio::test]
    async fn test_stage_mcp_archive_does_not_hold_write_lock_during_fetch() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let body = make_tar_gz(&[(
            "SKILL.md",
            b"---\nname: pkg\ndescription: archived skill\n---\nbody",
        )]);
        let registry = std::sync::Arc::new(RwLock::new(SkillRegistry::new()));
        let fetcher = AssertUnlockedFetcher {
            registry: registry.clone(),
            body,
        };
        let names = stage_mcp_skills(&ArchiveManager, &registry, &home, None, Some(&fetcher))
            .await
            .unwrap();
        // fetch 期间的 try_read 断言已通过（写锁未跨网络），且 finalize 阶段完成注册。
        assert_eq!(names, vec!["mcp:tfrobot-tools:pkg".to_string()]);
        assert!(registry
            .read()
            .await
            .resolve("mcp:tfrobot-tools:pkg")
            .is_some());
    }
}
