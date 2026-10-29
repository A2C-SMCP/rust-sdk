//! 按路径读取的只读根基础（PR2a）：`readRoots` 双形态编译 + 词法 / realpath 围栏。
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

use std::path::Path;

use smcp::utils::path_norm::{native_platform, normalize_path, realpath_nonstrict, RealpathError};
use tracing::warn;

use crate::settings::policy::resolve_policy_settings;
use crate::settings::schema::FIELD_READ_ROOTS;
use crate::settings::scope::{resolve_settings, EnvMap, ResolveSettingsArgs};

/// 一个授权读取根的双形态 / One authorized read root in two forms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadRoot {
    /// `normalize_path(配置值)`（配置形态；词法围栏命中即可）。
    pub configured: String,
    /// `normalize_path(realpath(配置值))`（realpath 形态；realpath 围栏唯一依据）。
    ///
    /// realpath 失败或结果无法归一 / 非 Unicode → `None`（只保留配置形态并记诊断）。
    pub real: Option<String>,
}

/// 把 settings `readRoots` 原始列表编译为双形态根列表（宿主平台）。
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
        roots.push(ReadRoot { configured, real });
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
    let policy = resolve_policy_settings(None, None, None);
    let resolved = resolve_settings(ResolveSettingsArgs {
        cwd: Some(config_dir),
        env,
        flag_settings_path: None,
        policy_settings: Some(&policy),
    });
    let Some(items) = resolved
        .settings
        .get(FIELD_READ_ROOTS)
        .and_then(|value| value.as_array())
    else {
        return Vec::new();
    };
    let raw: Vec<String> = items
        .iter()
        .filter_map(|item| item.as_str().map(str::to_string))
        .collect();
    build_read_roots(&raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

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
}
