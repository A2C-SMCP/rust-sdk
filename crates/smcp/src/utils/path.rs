//! 路径工具 / Path utilities。
//!
//! 跨子系统复用的纯路径助手（无外部依赖，仅 std）。
//! Cross-subsystem pure path helpers (std only).
//!
//! 对标 Python 参考实现 / Mirrors the Python reference: `a2c_smcp/utils/path.py`。

use std::path::{Component, Path, PathBuf};

/// 词法规范化路径 / Lexically normalize a path（消解 `.` / `..`，**不**触碰文件系统、**不**解析符号链接）。
///
/// 与 Python `Path.resolve(strict=False)` 的**词法部分**等价：逐组件折叠 `.`（丢弃）与 `..`
/// （回退上一个普通组件；越过根则吸收；相对路径前导 `..` 保留）。不访问 FS，故对**不存在**的目标
/// 同样可用——这是治理层（SKILL home / settings）在目录创建前解析路径所必需的。
///
/// Lexical equivalent of Python `Path.resolve(strict=False)`: folds `.` and `..` without touching
/// the filesystem (no symlink resolution), so it works on non-existent targets.
pub fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::Prefix(_) | Component::RootDir => out.push(comp.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                // 回退上一个普通组件；若顶部是根 / 前导 `..`，则按词法规则保留 `..`。
                match out.components().next_back() {
                    Some(Component::Normal(_)) => {
                        out.pop();
                    }
                    Some(Component::RootDir) | Some(Component::Prefix(_)) => {
                        // 已在根，`..` 无意义，丢弃（与 POSIX `/.. == /` 一致）。
                    }
                    _ => out.push(".."),
                }
            }
            Component::Normal(seg) => out.push(seg),
        }
    }
    out
}

/// 展开前导 `~` / Expand a leading `~`（使用 `$HOME`；无 `$HOME` 或非前导 `~` 时原样返回）。
fn expanduser(path: &Path) -> PathBuf {
    let s = match path.to_str() {
        Some(s) => s,
        None => return path.to_path_buf(),
    };
    if s == "~" {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home);
        }
        return path.to_path_buf();
    }
    if let Some(rest) = s.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return Path::new(&home).join(rest);
        }
    }
    path.to_path_buf()
}

/// XDG-first 路径解析 / XDG-first path resolution（镜像 Claude Code「XDG 优先 + 回退」范式）。
///
/// 若 `xdg_value` 为**绝对路径** → 取 `$<xdg_value>/<subpath...>`；否则（未设置 / 空白 / 相对值——按
/// XDG 规范相对值视为未设置）→ 取 `fallback`。结果统一 `expanduser` + 词法规范化为**绝对路径**；**不**创建目录。
///
/// If `xdg_value` is an **absolute** path, returns `$<xdg_value>/<subpath...>`; otherwise (unset /
/// blank / relative — treated as unset per the XDG spec) returns `fallback`. The result is
/// `expanduser`-ed and lexically normalized to an absolute path; no directory is created.
///
/// 供 SKILL Home（`XDG_DATA_HOME`）与 user 配置目录（`XDG_CONFIG_HOME`）等复用——两者仅 env 变量、
/// 子路径、fallback 不同。更高优先级旋钮（如 `A2C_SKILL_HOME`）由调用方在调用前自行处理。
///
/// 与 Python 的差异 / Difference from Python: 本实现做**词法**规范化（不解析符号链接），以支持
/// 解析尚未创建的目标目录；Python `resolve()` 会解析符号链接但对不存在路径同样不报错。
pub fn resolve_xdg_first(xdg_value: Option<&str>, subpath: &[&str], fallback: &Path) -> PathBuf {
    let trimmed = xdg_value.map(str::trim).filter(|s| !s.is_empty());
    let base: PathBuf = match trimmed {
        Some(raw) if Path::new(raw).is_absolute() => {
            let mut p = PathBuf::from(raw);
            for seg in subpath {
                p.push(seg);
            }
            p
        }
        // XDG 规范：变量必须为绝对路径，未设置 / 空白 / 相对值均按未设置处理 → fallback。
        _ => fallback.to_path_buf(),
    };
    normalize_lexical(&expanduser(&base))
}

/// `path` 是否等于或位于 `parent` 之下 / Whether `path` is at or nested under `parent`。
///
/// **先对两侧做** [`normalize_lexical`]（消解 `.` / `..`）再做前缀判定，因此对 `..` 穿越与绝对路径逃逸
/// 返回 `false`——这是防目录穿越的核心防线。不解析符号链接；若需防符号链接逃逸，调用方应先传入
/// 已 canonicalize 的路径。
///
/// Both sides are [`normalize_lexical`]-ed (folding `.` / `..`) before the prefix check, so `..`
/// traversal and absolute-path escapes return `false`. Symlinks are not resolved.
pub fn is_within(path: &Path, parent: &Path) -> bool {
    let path = normalize_lexical(path);
    let parent = normalize_lexical(parent);
    path.strip_prefix(&parent).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_lexical_folds_dot_dotdot() {
        assert_eq!(
            normalize_lexical(Path::new("/a/b/../c")),
            PathBuf::from("/a/c")
        );
        assert_eq!(
            normalize_lexical(Path::new("/a/./b/")),
            PathBuf::from("/a/b")
        );
        assert_eq!(
            normalize_lexical(Path::new("/a/../../b")),
            PathBuf::from("/b")
        ); // 越根吸收
        assert_eq!(
            normalize_lexical(Path::new("a/b/../c")),
            PathBuf::from("a/c")
        );
    }

    #[test]
    fn test_is_within_basic() {
        assert!(is_within(Path::new("/base/sub/file"), Path::new("/base")));
        assert!(is_within(Path::new("/base"), Path::new("/base"))); // 等于 parent 视为 within
        assert!(is_within(Path::new("/base/./sub"), Path::new("/base")));
    }

    #[test]
    fn test_is_within_rejects_traversal_and_escape() {
        // `..` 穿越：规范化后逃出 base → false
        assert!(!is_within(Path::new("/base/../secret"), Path::new("/base")));
        assert!(!is_within(
            Path::new("/base/sub/../../etc"),
            Path::new("/base")
        ));
        // 绝对路径逃逸
        assert!(!is_within(Path::new("/etc/passwd"), Path::new("/base")));
        // 同前缀但非子树（/base2 不在 /base 下）
        assert!(!is_within(Path::new("/base2/x"), Path::new("/base")));
    }

    #[test]
    fn test_resolve_xdg_first_prefers_absolute_xdg() {
        let fallback = Path::new("/home/u/.local/share/a2c");
        // 绝对 XDG → 取 $XDG/<subpath>
        let r = resolve_xdg_first(Some("/xdg/data"), &["a2c", "skills"], fallback);
        assert_eq!(r, PathBuf::from("/xdg/data/a2c/skills"));
    }

    #[test]
    fn test_resolve_xdg_first_falls_back() {
        let fallback = Path::new("/home/u/.local/share/a2c");
        // 未设置 / 空白 / 相对值 → fallback（相对值按 XDG 规范视为未设置）
        assert_eq!(resolve_xdg_first(None, &["x"], fallback), fallback);
        assert_eq!(resolve_xdg_first(Some("   "), &["x"], fallback), fallback);
        assert_eq!(
            resolve_xdg_first(Some("relative/dir"), &["x"], fallback),
            fallback
        );
    }
}
