/*!
* 文件名: sandbox.rs
* 作者: JQQ
* 创建日期: 2026/06/03
* 最后修改日期: 2026/06/03
* 版权: 2023 JQQ. All rights reserved.
* 依赖: smcp (utils::path::is_within, ErrorCode/ErrorPayload), thiserror
* 描述: SKILL 包根沙箱（对标 Python computer/skills/sandbox.py）/ SKILL package-root sandbox.
*/

//! SKILL 包根沙箱 / SKILL package-root sandbox。
//!
//! 协议依据 / Protocol: a2c-smcp-protocol `skill.md` §9 安全模型；`error-handling.md` §4017
//! （Skill Resource Not Accessible，`details.reason` 开放枚举 traversal/forbidden/not_found/too_large）。
//! 对标 Python `a2c_smcp/computer/skills/sandbox.py`。
//!
//! 职责 / Responsibility：
//! - **包根内安全解析**：`rel_path` 必须相对、无 `..`、解析符号链接后仍落在包根内；绝对 / `..` / symlink
//!   逃逸 → `traversal`。
//! - **敏感文件拒绝**：`.skillenv` 等任何 `rel_path` 下命中即拒（`forbidden`），且**不泄漏存在性**
//!   （存在与不存在同一 reason，命中判定纯词法、不 `stat`）。
//! - **name 寻址防越权**（§9.2 安全铁律）：本模块只收**包根绝对路径**（由 Registry 经 name 解析得到），
//!   **绝不**接收 SKILL name、**绝不**从 name/`rel_path` 推导 FS 路径——结构上杜绝越权寻址。
//!
//! 判定顺序保证「forbidden 不泄漏存在性」「触 FS 前先挡 traversal」：① 缺省 SKILL.md → ② 词法守卫
//! → ③ forbidden（纯词法，先于存在性）→ ④ realpath 容器校验（捕 symlink 逃逸）+ realpath 后 basename
//! 复检 forbidden → ⑤ 既存普通文件。

use std::path::{Path, PathBuf};

use smcp::utils::path::is_within;
use smcp::{ErrorCode, ErrorPayload};

/// `rel_path` 缺省 = 包根入口 SKILL.md / default rel_path = package-root entry SKILL.md。
pub const DEFAULT_SKILL_FILE: &str = "SKILL.md";

/// 敏感凭证文件默认黑集（basename，大小写不敏感比较）/ default sensitive-credential basenames。
///
/// 协议只明文命名 `.skillenv`；"及 Computer 判定的凭证类文件"留给调用方经 `forbidden_names` 注入扩展，
/// 避免在 SDK 内硬编码猜测性策略。
pub const FORBIDDEN_SKILL_FILES: &[&str] = &[".skillenv"];

/// 4017 `details.reason` 开放枚举 / open enum for protocol 4017 `details.reason`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillSandboxReason {
    /// 路径穿越（绝对 / `..` / symlink 逃逸）/ path traversal。
    Traversal,
    /// 敏感文件命中（`.skillenv` 等）/ forbidden sensitive file。
    Forbidden,
    /// 资源不存在或非普通文件 / not an existing regular file。
    NotFound,
    /// 超 size cap（铸造期决断）/ exceeds size cap。
    TooLarge,
}

impl SkillSandboxReason {
    /// 协议线格式字符串（直填 4017 `details.reason`）/ protocol wire string。
    pub fn as_str(&self) -> &'static str {
        match self {
            SkillSandboxReason::Traversal => "traversal",
            SkillSandboxReason::Forbidden => "forbidden",
            SkillSandboxReason::NotFound => "not_found",
            SkillSandboxReason::TooLarge => "too_large",
        }
    }
}

impl std::fmt::Display for SkillSandboxReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// SKILL 包内资源不可访问 / SKILL in-package resource not accessible。
///
/// 映射协议 `4017`；`reason` 开放枚举，`rel_path` 回显请求，`total_size` 仅 `too_large` 携带——三者直填
/// 4017 `details`。用 [`SkillSandboxError::to_error_payload`] 构造 flat [`ErrorPayload`]。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("skill resource not accessible (reason={reason}, rel_path={rel_path:?})")]
pub struct SkillSandboxError {
    /// 失败原因（4017 `details.reason`）/ failure reason。
    pub reason: SkillSandboxReason,
    /// 回显的请求相对路径 / echoed request path。
    pub rel_path: String,
    /// 仅 `too_large` 携带的字节数 / byte size, set only for `too_large`。
    pub total_size: Option<u64>,
}

impl SkillSandboxError {
    fn new(reason: SkillSandboxReason, rel_path: impl Into<String>) -> Self {
        Self {
            reason,
            rel_path: rel_path.into(),
            total_size: None,
        }
    }

    /// 构造 `traversal` 错误 / Build a `traversal` error。
    pub fn traversal(rel_path: impl Into<String>) -> Self {
        Self::new(SkillSandboxReason::Traversal, rel_path)
    }

    /// 构造 `forbidden` 错误 / Build a `forbidden` error。
    pub fn forbidden(rel_path: impl Into<String>) -> Self {
        Self::new(SkillSandboxReason::Forbidden, rel_path)
    }

    /// 构造 `not_found` 错误 / Build a `not_found` error。
    pub fn not_found(rel_path: impl Into<String>) -> Self {
        Self::new(SkillSandboxReason::NotFound, rel_path)
    }

    /// 构造 `too_large` 错误（携带字节数）/ Build a `too_large` error with the byte size。
    pub fn too_large(rel_path: impl Into<String>, total_size: u64) -> Self {
        Self {
            reason: SkillSandboxReason::TooLarge,
            rel_path: rel_path.into(),
            total_size: Some(total_size),
        }
    }

    /// 映射的协议错误码（恒 `4017`）/ Mapped protocol error code。
    pub fn error_code(&self) -> ErrorCode {
        ErrorCode::SkillResourceNotAccessible
    }

    /// 构造对应 flat [`ErrorPayload`]（`code=4017`，`details.{reason,rel_path,total_size?}`）。
    pub fn to_error_payload(&self) -> ErrorPayload {
        let mut payload = ErrorPayload::new(i64::from(self.error_code().code()), self.to_string())
            .with_detail("reason", self.reason.as_str())
            .with_detail("rel_path", self.rel_path.clone());
        if let Some(size) = self.total_size {
            payload = payload.with_detail("total_size", size);
        }
        payload
    }
}

/// 触 FS 前的纯词法 traversal 守卫 / Pre-FS lexical traversal guard。
///
/// 拒绝：反斜杠（非 POSIX 分隔符，防 Windows 路径走私）/ 绝对路径 / 含 `..` 段 / 段内含 `:`（盘符 /
/// scheme 走私）。返回清洗后的相对路径（`.` 与空段已丢弃，对齐 `PurePosixPath`）。
fn guard_lexical(rel: &str) -> Result<PathBuf, SkillSandboxError> {
    if rel.contains('\\') {
        return Err(SkillSandboxError::traversal(rel));
    }
    if rel.starts_with('/') {
        return Err(SkillSandboxError::traversal(rel));
    }
    let mut out = PathBuf::new();
    for seg in rel.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." || seg.contains(':') {
            return Err(SkillSandboxError::traversal(rel));
        }
        out.push(seg);
    }
    Ok(out)
}

/// 资源 basename（小写）是否命中 forbidden 集 / Whether the basename hits the forbidden set。
fn is_forbidden(path: &Path, forbidden_lower: &[String]) -> bool {
    match path.file_name().and_then(|n| n.to_str()) {
        Some(name) => forbidden_lower.iter().any(|f| *f == name.to_lowercase()),
        None => false,
    }
}

/// 在包根 `root` 内安全解析 `rel_path`，返回已规范化（解析符号链接）的资源绝对路径（默认 forbidden 集）。
///
/// # Errors
/// `traversal` / `forbidden` / `not_found`（见模块文档判定顺序）。
pub fn resolve_skill_resource(
    root: &Path,
    rel_path: Option<&str>,
) -> Result<PathBuf, SkillSandboxError> {
    resolve_skill_resource_with(root, rel_path, FORBIDDEN_SKILL_FILES)
}

/// [`resolve_skill_resource`] 的可注入 forbidden 集版本 / Variant with an injectable forbidden set。
///
/// # Errors
/// `traversal` / `forbidden` / `not_found`。
pub fn resolve_skill_resource_with(
    root: &Path,
    rel_path: Option<&str>,
    forbidden_names: &[&str],
) -> Result<PathBuf, SkillSandboxError> {
    let rel = match rel_path {
        Some(p) if !p.is_empty() => p,
        _ => DEFAULT_SKILL_FILE,
    };

    // ② 词法守卫（触 FS 前）。
    let pure = guard_lexical(rel)?;

    // 大小写不敏感比较（堵 macOS/APFS、Windows 大小写折叠绕过）。
    let forbidden_lower: Vec<String> = forbidden_names.iter().map(|n| n.to_lowercase()).collect();

    // ③ forbidden 先于任何 FS 访问（纯词法，不泄漏存在性）。
    if is_forbidden(&pure, &forbidden_lower) {
        return Err(SkillSandboxError::forbidden(rel));
    }

    // ④ realpath 容器校验（捕 symlink 逃逸）。root / target 解析符号链接；不存在 → not_found。
    let root_real = root
        .canonicalize()
        .map_err(|_| SkillSandboxError::not_found(rel))?;
    let candidate = root_real.join(&pure);
    let target_real = match candidate.canonicalize() {
        Ok(t) => t,
        Err(_) => return Err(SkillSandboxError::not_found(rel)),
    };
    if !is_within(&target_real, &root_real) {
        return Err(SkillSandboxError::traversal(rel));
    }
    // symlink 指向 `.skillenv` 等：realpath 后 basename 复检。
    if is_forbidden(&target_real, &forbidden_lower) {
        return Err(SkillSandboxError::forbidden(rel));
    }

    // ⑤ 既存普通文件（不存在 / 目录 → not_found）。
    if !target_real.is_file() {
        return Err(SkillSandboxError::not_found(rel));
    }

    Ok(target_real)
}

/// 绝对上限校验：`path` 字节数 > `cap` → `4017 too_large`（不铸句柄）/ Absolute size-cap check。
///
/// 协议依据 skill.md §9（铸造期决断 too_large，防 DoS）。返回 cap 内的文件字节数。
///
/// # Errors
/// `too_large`（携带 `total_size`）；路径不可 stat → `not_found`（防御性，调用前应已沙箱通过）。
pub fn ensure_within_size_cap(
    path: &Path,
    cap: u64,
    rel_path: &str,
) -> Result<u64, SkillSandboxError> {
    let size = std::fs::metadata(path)
        .map_err(|_| SkillSandboxError::not_found(rel_path))?
        .len();
    if size > cap {
        return Err(SkillSandboxError::too_large(rel_path, size));
    }
    Ok(size)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// 建一个含 SKILL.md + references/note.txt 的包根。
    fn make_pkg() -> TempDir {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("SKILL.md"), "---\nname: x\n---\nbody").unwrap();
        fs::create_dir_all(tmp.path().join("references")).unwrap();
        fs::write(tmp.path().join("references/note.txt"), "hello").unwrap();
        tmp
    }

    #[test]
    fn test_default_and_explicit_skill_md() {
        let pkg = make_pkg();
        let a = resolve_skill_resource(pkg.path(), None).unwrap();
        let b = resolve_skill_resource(pkg.path(), Some("SKILL.md")).unwrap();
        assert_eq!(a, b);
        assert!(a.ends_with("SKILL.md"));
        // 空 rel_path 亦取默认。
        assert_eq!(resolve_skill_resource(pkg.path(), Some("")).unwrap(), a);
    }

    #[test]
    fn test_subresource_and_dot_segments() {
        let pkg = make_pkg();
        let r = resolve_skill_resource(pkg.path(), Some("references/note.txt")).unwrap();
        assert!(r.ends_with("note.txt"));
        // `.` 段被规整。
        let r2 = resolve_skill_resource(pkg.path(), Some("./references/./note.txt")).unwrap();
        assert_eq!(r, r2);
    }

    #[test]
    fn test_traversal_rejected() {
        let pkg = make_pkg();
        for bad in [
            "../../etc/passwd",
            "/etc/passwd",
            "..\\windows",
            "references/../../escape",
            "C:/Windows",
        ] {
            let err = resolve_skill_resource(pkg.path(), Some(bad)).unwrap_err();
            assert_eq!(err.reason, SkillSandboxReason::Traversal, "{bad}");
            assert_eq!(err.to_error_payload().code, 4017);
        }
    }

    #[test]
    fn test_forbidden_skillenv_no_existence_leak() {
        let pkg = make_pkg();
        // 不存在的 .skillenv 也按 forbidden（不泄漏存在性）。
        let err = resolve_skill_resource(pkg.path(), Some(".skillenv")).unwrap_err();
        assert_eq!(err.reason, SkillSandboxReason::Forbidden);
        // 存在时同 reason。
        fs::write(pkg.path().join(".skillenv"), "secret").unwrap();
        let err = resolve_skill_resource(pkg.path(), Some(".skillenv")).unwrap_err();
        assert_eq!(err.reason, SkillSandboxReason::Forbidden);
        // 嵌套 + 大小写变体亦命中（basename 比较，大小写不敏感）。
        assert_eq!(
            resolve_skill_resource(pkg.path(), Some("references/.SKILLENV"))
                .unwrap_err()
                .reason,
            SkillSandboxReason::Forbidden
        );
    }

    #[test]
    fn test_not_found_missing_and_directory() {
        let pkg = make_pkg();
        assert_eq!(
            resolve_skill_resource(pkg.path(), Some("missing.md"))
                .unwrap_err()
                .reason,
            SkillSandboxReason::NotFound
        );
        // 目录而非文件。
        assert_eq!(
            resolve_skill_resource(pkg.path(), Some("references"))
                .unwrap_err()
                .reason,
            SkillSandboxReason::NotFound
        );
    }

    #[test]
    #[cfg(unix)]
    fn test_symlink_escape_is_traversal() {
        use std::os::unix::fs::symlink;
        let pkg = make_pkg();
        let outside = TempDir::new().unwrap();
        fs::write(outside.path().join("secret.txt"), "s").unwrap();
        symlink(
            outside.path().join("secret.txt"),
            pkg.path().join("link.txt"),
        )
        .unwrap();
        let err = resolve_skill_resource(pkg.path(), Some("link.txt")).unwrap_err();
        assert_eq!(err.reason, SkillSandboxReason::Traversal);
    }

    #[test]
    fn test_size_cap() {
        let pkg = make_pkg();
        let p = resolve_skill_resource(pkg.path(), Some("references/note.txt")).unwrap();
        assert_eq!(
            ensure_within_size_cap(&p, 1024, "references/note.txt").unwrap(),
            5
        );
        let err = ensure_within_size_cap(&p, 0, "references/note.txt").unwrap_err();
        assert_eq!(err.reason, SkillSandboxReason::TooLarge);
        assert_eq!(err.total_size, Some(5));
        assert_eq!(err.to_error_payload().details.unwrap()["total_size"], 5);
    }

    #[test]
    fn test_custom_forbidden_names() {
        let pkg = make_pkg();
        fs::write(pkg.path().join("creds.json"), "{}").unwrap();
        // 默认集不拦 creds.json。
        assert!(resolve_skill_resource(pkg.path(), Some("creds.json")).is_ok());
        // 注入扩展后命中 forbidden。
        let err = resolve_skill_resource_with(
            pkg.path(),
            Some("creds.json"),
            &[".skillenv", "creds.json"],
        )
        .unwrap_err();
        assert_eq!(err.reason, SkillSandboxReason::Forbidden);
    }
}
