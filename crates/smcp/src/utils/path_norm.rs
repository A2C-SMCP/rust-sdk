//! `canonical_path` 词法规范化（纯函数）/ Lexical `canonical_path` normalization (pure).
//!
//! 协议依据 / Protocol: `blob-transfer.md` §canonical_path 规范化 + 数据向量
//! `fixtures/canonical_path_conformance_vectors.json`（protocol#65）。对标 Python 参考实现
//! `a2c_smcp/utils/path_norm.py`。
//!
//! 设计约束 / Design constraints:
//! - 与宿主 OS 无关：`platform` 显式传入，Windows 形态在任意 CI 上可测；
//! - 不做 `~` 展开、百分号编解码、Unicode 归一，也不改变非盘符部分大小写；
//! - 词法部分由 [`normalize_path`] 负责；符号链接解析（非严格 realpath）由
//!   [`realpath_nonstrict`] 承担（宿主平台）。

use std::collections::VecDeque;
use std::ffi::OsString;
use std::fmt;
use std::path::{Component, Path, PathBuf};

/// 符号链接解析跳数上限（等价 POSIX `ELOOP` 常见阈值）/ Max symlink hops.
const MAX_SYMLINK_HOPS: usize = 40;

/// 路径平台形态 / Path platform form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathPlatform {
    /// POSIX（`/` 分隔，绝对路径以 `/` 开头）。
    Posix,
    /// Windows（canonical 形态 `/X:/...`，盘符大写、`/` 分隔）。
    Windows,
}

impl PathPlatform {
    /// 协议向量中的平台串 / Platform string used by the protocol vectors.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Posix => "posix",
            Self::Windows => "windows",
        }
    }
}

/// 宿主平台形态 / Host platform form.
pub const fn native_platform() -> PathPlatform {
    if cfg!(windows) {
        PathPlatform::Windows
    } else {
        PathPlatform::Posix
    }
}

/// 路径无法规范化 → 协议 `4020 invalid_path` / Path cannot be normalized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidPathError {
    message: String,
}

impl InvalidPathError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// 协议错误原因（恒为 `invalid_path`）/ Protocol error reason (always `invalid_path`).
    pub const fn reason(&self) -> &'static str {
        "invalid_path"
    }
}

impl fmt::Display for InvalidPathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for InvalidPathError {}

/// 把路径规范化为 `canonical_path` 形态 / Normalize a path to the `canonical_path` form.
///
/// `input` 为原始路径（POSIX 绝对路径；Windows 原生 `C:\...` 或 canonical `/C:/...`）。
/// 空串、含 U+0000、非绝对路径、Windows 非法分量 → [`InvalidPathError`]（协议 `invalid_path`）。
pub fn normalize_path(input: &str, platform: PathPlatform) -> Result<String, InvalidPathError> {
    if input.is_empty() || input.contains('\0') {
        return Err(InvalidPathError::new("path is empty or contains U+0000"));
    }
    match platform {
        PathPlatform::Posix => normalize_posix(input),
        PathPlatform::Windows => normalize_windows(input),
    }
}

/// `canonical_path` → 宿主原生路径（供 `lstat` / `open`）/ Canonical path → host-native path.
///
/// POSIX 原样返回；Windows `/C:/a/b` → `C:\a\b`（`/C:/` → `C:\`）。
pub fn to_native_path(canonical: &str, platform: PathPlatform) -> Result<String, InvalidPathError> {
    match platform {
        // POSIX canonical 形态即原生形态；不做额外校验（与 Python 同构）。
        PathPlatform::Posix => Ok(canonical.to_string()),
        PathPlatform::Windows => {
            if !is_canonical_windows_path(canonical) {
                return Err(InvalidPathError::new(format!(
                    "not a canonical Windows path: {canonical:?}"
                )));
            }
            Ok(canonical[1..].replace('/', "\\"))
        }
    }
}

/// 非严格 realpath：解析符号链接（含悬空链接），缺失分量原样保留。
/// Non-strict realpath: resolve symlinks (including dangling ones), keep missing components.
///
/// `canonical` 为 `canonical_path` 形态；经 [`to_native_path`] 转宿主原生形态后解析。等价
/// Python `os.path.realpath(..., strict=False)`；**不**以跟随链接的 `stat` / `exists()` 判定
/// 存在性。返回宿主原生路径，调用方再经 [`normalize_path`] 归一。
pub fn realpath_nonstrict(
    canonical: &str,
    platform: PathPlatform,
) -> Result<String, InvalidPathError> {
    let native = to_native_path(canonical, platform)?;
    let path = Path::new(&native);
    if !path.is_absolute() {
        return Err(InvalidPathError::new(format!(
            "realpath requires an absolute path: {native:?}"
        )));
    }

    let mut comps = path.components().peekable();
    let mut resolved = PathBuf::new();
    if let Some(Component::Prefix(prefix)) = comps.peek() {
        resolved.push(prefix.as_os_str());
        comps.next();
    }
    if matches!(comps.peek(), Some(Component::RootDir)) {
        resolved.push(Component::RootDir.as_os_str());
        comps.next();
    }

    let mut pending: VecDeque<OsString> = comps.map(|c| c.as_os_str().to_os_string()).collect();
    let mut hops = 0usize;
    while let Some(component) = pending.pop_front() {
        if component.as_os_str() == "." {
            continue;
        }
        if component.as_os_str() == ".." {
            resolved.pop();
            continue;
        }
        let candidate = resolved.join(&component);
        match std::fs::symlink_metadata(&candidate) {
            Ok(meta) if meta.file_type().is_symlink() => {
                hops += 1;
                if hops > MAX_SYMLINK_HOPS {
                    return Err(InvalidPathError::new(format!(
                        "too many levels of symbolic links: {native:?}"
                    )));
                }
                let target = std::fs::read_link(&candidate).map_err(|e| {
                    InvalidPathError::new(format!(
                        "cannot read symbolic link {}: {e}",
                        candidate.display()
                    ))
                })?;
                if target.is_absolute() {
                    let mut target_comps = target.components().peekable();
                    resolved = PathBuf::new();
                    if let Some(Component::Prefix(prefix)) = target_comps.peek() {
                        resolved.push(prefix.as_os_str());
                        target_comps.next();
                    }
                    if matches!(target_comps.peek(), Some(Component::RootDir)) {
                        resolved.push(Component::RootDir.as_os_str());
                        target_comps.next();
                    }
                    for c in target_comps.rev() {
                        pending.push_front(c.as_os_str().to_os_string());
                    }
                } else {
                    for c in target.components().rev() {
                        pending.push_front(c.as_os_str().to_os_string());
                    }
                }
            }
            // 普通文件 / 目录（或其它已存在类型）：按分量推进。
            Ok(_) => resolved.push(&component),
            // 第一个不存在的分量起，其余原样拼接（不再访问文件系统）。
            Err(_) => {
                resolved.push(&component);
                for c in pending.drain(..) {
                    resolved.push(&c);
                }
            }
        }
    }

    let resolved = crate::utils::path::normalize_lexical(&resolved);
    resolved
        .to_str()
        .map(str::to_string)
        .ok_or_else(|| InvalidPathError::new(format!("path is not valid Unicode: {native:?}")))
}

// ── 内部实现 / Internal implementation ───────────────────────────────────

fn normalize_posix(path: &str) -> Result<String, InvalidPathError> {
    if !path.starts_with('/') {
        return Err(InvalidPathError::new(format!(
            "POSIX path must be absolute: {path:?}"
        )));
    }
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                // 根处钳制：已在根时忽略 / Clamp at root: ignore when already at root.
                segments.pop();
            }
            _ => segments.push(segment),
        }
    }
    Ok(format!("/{}", segments.join("/")))
}

fn normalize_windows(path: &str) -> Result<String, InvalidPathError> {
    // ① 设备前缀不支持（先于 `\\?\` 剥离判定）。
    if path.starts_with("\\\\.\\") {
        return Err(InvalidPathError::new(format!(
            "Windows device prefix is not supported: {path:?}"
        )));
    }
    let mut normalized = path.to_string();
    // ② 剥 `\\?\`；其后 `UNC\` ⇒ 不支持。
    if let Some(rest) = normalized.strip_prefix("\\\\?\\") {
        normalized = rest.to_string();
        if normalized
            .get(..4)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("UNC\\"))
        {
            return Err(InvalidPathError::new(format!(
                "UNC path is not supported: {path:?}"
            )));
        }
    }
    // ③ `\` → `/`。
    normalized = normalized.replace('\\', "/");
    // ④ canonical `/X:` 形态剥前导 `/`；恰为 `X:` 时补 `/`。
    if is_canonical_windows_drive(&normalized) {
        normalized = normalized[1..].to_string();
        if normalized.len() == 2 {
            normalized.push('/');
        }
    }
    // ⑤ MUST `X:/` 开头。
    if !is_windows_drive_path(&normalized) {
        return Err(InvalidPathError::new(format!(
            "Windows path must be drive-absolute: {path:?}"
        )));
    }
    let drive = char::from(normalized.as_bytes()[0].to_ascii_uppercase());
    let mut segments: Vec<&str> = Vec::new();
    for segment in normalized[3..].split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        check_windows_component(segment)?;
        if segment == ".." {
            segments.pop();
            continue;
        }
        segments.push(segment);
    }
    Ok(format!("/{drive}:/{}", segments.join("/")))
}

fn is_canonical_windows_drive(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 3
        && bytes[0] == b'/'
        && bytes[1].is_ascii_alphabetic()
        && bytes[2] == b':'
        && (bytes.len() == 3 || bytes[3] == b'/')
}

fn is_canonical_windows_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 4
        && bytes[0] == b'/'
        && bytes[1].is_ascii_alphabetic()
        && bytes[2] == b':'
        && bytes[3] == b'/'
}

fn is_windows_drive_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'/'
}

fn check_windows_component(segment: &str) -> Result<(), InvalidPathError> {
    // `.` / `..` 本身豁免（`.` 已在上层丢弃；`..` 走弹出分支）。
    if segment == "." || segment == ".." {
        return Ok(());
    }
    for ch in segment.chars() {
        if matches!(ch, '<' | '>' | ':' | '"' | '|' | '?' | '*') {
            return Err(InvalidPathError::new(format!(
                "Windows path component contains a forbidden character: {segment:?}"
            )));
        }
        if ('\u{0001}'..='\u{001f}').contains(&ch) {
            return Err(InvalidPathError::new(format!(
                "Windows path component contains a control character: {segment:?}"
            )));
        }
    }
    if segment.ends_with('.') || segment.ends_with(' ') {
        return Err(InvalidPathError::new(format!(
            "Windows path component ends with '.' or space: {segment:?}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn posix_normalization() {
        let cases = [
            ("/home/u/a.txt", "/home/u/a.txt"),
            ("//home///u//a.txt", "/home/u/a.txt"),
            ("/home/./u/a.txt", "/home/u/a.txt"),
            ("/home/u/../v/a.txt", "/home/v/a.txt"),
            ("/../../a.txt", "/a.txt"),
            ("/", "/"),
            ("/a/", "/a"),
        ];
        for (input, expected) in cases {
            assert_eq!(
                normalize_path(input, PathPlatform::Posix).unwrap(),
                expected,
                "input={input:?}"
            );
        }
    }

    #[test]
    fn posix_rejects_invalid() {
        for input in ["", "a/b", "./a", "~ /a", "/a\u{0}b"] {
            assert!(
                normalize_path(input, PathPlatform::Posix).is_err(),
                "input={input:?}"
            );
        }
    }

    #[test]
    fn windows_normalization() {
        let cases = [
            (r"C:\a\b", "/C:/a/b"),
            (r"c:\a\..\b", "/C:/b"),
            ("/C:/a/b", "/C:/a/b"),
            ("/c:/a/./b/", "/C:/a/b"),
            (r"\\?\C:\a\b", "/C:/a/b"),
            (r"C:\", "/C:/"),
            ("/C:", "/C:/"),
            ("/C:/", "/C:/"),
        ];
        for (input, expected) in cases {
            assert_eq!(
                normalize_path(input, PathPlatform::Windows).unwrap(),
                expected,
                "input={input:?}"
            );
        }
    }

    #[test]
    fn windows_rejects_invalid() {
        let cases = [
            "",
            "a/b",
            r"C:foo",
            "C:",
            "/x",
            r"\\server\share",
            r"\\?\UNC\server\share",
            r"\\.\PIPE\name",
            r"C:\a<b",
            r"C:\a:b",
            r"C:\a.",
            r"C:\a ",
            "C:\\a\u{1}",
        ];
        for input in cases {
            assert!(
                normalize_path(input, PathPlatform::Windows).is_err(),
                "input={input:?}"
            );
        }
    }

    #[test]
    fn to_native_round_trip() {
        assert_eq!(
            to_native_path("/C:/a/b", PathPlatform::Windows).unwrap(),
            r"C:\a\b"
        );
        assert_eq!(
            to_native_path("/C:/", PathPlatform::Windows).unwrap(),
            r"C:\"
        );
        assert_eq!(to_native_path("/a/b", PathPlatform::Posix).unwrap(), "/a/b");
        assert!(to_native_path("C:/a", PathPlatform::Windows).is_err());
    }

    #[test]
    fn invalid_path_error_reason_is_fixed() {
        let err = normalize_path("relative", PathPlatform::Posix).unwrap_err();
        assert_eq!(err.reason(), "invalid_path");
    }
}
