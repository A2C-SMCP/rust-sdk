//! #245 PR3：`client:resolve_path` / `readRoots` 的 **conformance §4.5 真实 FS** 行为测试。
//!
//! 覆盖协议 `computer-management/conformance-tests.md` §4.5（v0.5.0 / protocol#65 + #75/#76）：
//! 符号链接 root 换指向、悬空链接、resolve 后删除 → 4018 gone、移除 root → 4018 forbidden、
//! 目录 → 4020 not_a_file、`canonical_path` 回灌 resolve → get_blob 往返、readRoots 空/越界、
//! 固定拒绝集（`.skillenv` / landing `.part` / 大小写不敏感宿主的 `.SKILLENV`）、非 ASCII 路径。
//!
//! 用公开 [`Computer`] API（真实 settings 文件 + 真实 FS），不 mock 管线内部。

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::json;
use tempfile::TempDir;

use smcp_computer::blob::{decode_blob_handle, BlobHandleError, DecodedHandle, ResolveReason};
use smcp_computer::computer::{Computer, SilentSession};

/// 测试上下文：隔离 HOME/XDG + readRoots/landingRoot 可写的 user settings。
struct Ctx {
    tmp: TempDir,
    share: PathBuf,
    home: PathBuf,
    xdg: PathBuf,
    config_dir: PathBuf,
    env: HashMap<String, String>,
}

impl Ctx {
    fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let share = tmp.path().join("share");
        let home = tmp.path().join("home");
        let xdg = tmp.path().join("xdg");
        let config_dir = tmp.path().join("proj");
        for dir in [&share, &home, &config_dir] {
            fs::create_dir_all(dir).unwrap();
        }
        fs::create_dir_all(xdg.join("a2c")).unwrap();
        let mut env = HashMap::new();
        env.insert("HOME".to_string(), home.to_string_lossy().into_owned());
        env.insert(
            "XDG_CONFIG_HOME".to_string(),
            xdg.to_string_lossy().into_owned(),
        );
        env.insert(
            "XDG_STATE_HOME".to_string(),
            tmp.path().join("state").to_string_lossy().into_owned(),
        );
        Self {
            tmp,
            share,
            home,
            xdg,
            config_dir,
            env,
        }
    }

    fn settings_path(&self) -> PathBuf {
        self.xdg.join("a2c").join("settings.json")
    }

    fn write_settings(&self, value: serde_json::Value) {
        fs::write(self.settings_path(), value.to_string()).unwrap();
    }

    fn set_read_roots(&self, roots: &[&Path]) {
        self.write_settings(json!({
            "readRoots": roots.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>()
        }));
    }

    fn set_read_roots_and_landing(&self, roots: &[&Path], landing: &Path) {
        self.write_settings(json!({
            "readRoots": roots.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>(),
            "landingRoot": landing.to_string_lossy(),
        }));
    }

    fn computer(&self) -> Computer<SilentSession> {
        Computer::new("c", SilentSession::new("s"), None, None, false, false)
            .with_config_dir(&self.config_dir)
            .with_config_env(self.env.clone())
            .with_skill_home(&self.home)
            .with_blob_cache_root(self.tmp.path().join("blob"))
    }

    fn file(&self, name: &str, data: &[u8]) -> PathBuf {
        let path = self.share.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, data).unwrap();
        path
    }
}

fn reason(error: &smcp_computer::blob::PathResolveError) -> &'static str {
    error.reason.as_str()
}

fn pull_reason(error: &BlobHandleError) -> &'static str {
    error.reason()
}

#[tokio::test]
async fn resolve_pull_roundtrip_and_canonical_reuse() {
    let ctx = Ctx::new();
    let file = ctx.file("hello.txt", b"hello path");
    ctx.set_read_roots(&[&ctx.share]);
    let computer = ctx.computer();

    let resolved = computer
        .resolve_read_path(file.to_str().unwrap())
        .await
        .unwrap();
    assert_eq!(resolved.total_size, 10);
    assert_eq!(resolved.mime_type, "text/plain");
    assert!(resolved.mtime != 0);
    assert_eq!(
        resolved.sha256,
        smcp::utils::hash::sha256_hex(b"hello path")
    );

    // canonical_path 可直接回用作 path。
    let again = computer
        .resolve_read_path(&resolved.canonical_path)
        .await
        .unwrap();
    assert_eq!(again.canonical_path, resolved.canonical_path);
    assert_eq!(again.sha256, resolved.sha256);

    // get_blob 链路（经 resolve_blob 路由 + 分块切片）。
    let payload = match decode_blob_handle(&resolved.blob_handle).unwrap() {
        DecodedHandle::Path(payload) => payload,
        other => panic!("unexpected kind {}", other.kind()),
    };
    let pulled = computer.pull_path_blob(&payload).await.unwrap();
    assert_eq!(pulled.total_size, 10);
    assert_eq!(pulled.sha256, resolved.sha256);
    assert_eq!(pulled.slice(0, 4).unwrap(), b"hell".to_vec());
    assert_eq!(pulled.slice(4, 100).unwrap(), b"o path".to_vec());
    // 同一套 get_blob 路由入口。
    let routed = computer.resolve_blob(&resolved.blob_handle).await.unwrap();
    assert_eq!(routed.slice(0, 10).unwrap(), b"hello path".to_vec());
}

#[tokio::test]
async fn read_roots_empty_and_outside_are_forbidden() {
    let ctx = Ctx::new();
    let file = ctx.file("visible.txt", b"x");
    // 未配置 readRoots：默认空 = 不可读（且不泄露存在性）。
    let computer = ctx.computer();
    let error = computer
        .resolve_read_path(file.to_str().unwrap())
        .await
        .unwrap_err();
    assert_eq!(reason(&error), "forbidden");

    // 配置 root 后，root 外文件（存在与否）一律 forbidden。
    ctx.set_read_roots(&[&ctx.share]);
    let computer = ctx.computer();
    let outside = ctx.tmp.path().join("outside.txt");
    fs::write(&outside, b"x").unwrap();
    let error = computer
        .resolve_read_path(outside.to_str().unwrap())
        .await
        .unwrap_err();
    assert_eq!(reason(&error), "forbidden");
    let missing_outside = ctx.tmp.path().join("does-not-exist.txt");
    let error = computer
        .resolve_read_path(missing_outside.to_str().unwrap())
        .await
        .unwrap_err();
    assert_eq!(reason(&error), "forbidden");

    // root 内不存在 → not_found；目录 → not_a_file；相对路径 → invalid_path。
    let error = computer
        .resolve_read_path(ctx.share.join("missing.txt").to_str().unwrap())
        .await
        .unwrap_err();
    assert_eq!(reason(&error), "not_found");
    let error = computer
        .resolve_read_path(ctx.share.to_str().unwrap())
        .await
        .unwrap_err();
    assert_eq!(reason(&error), "not_a_file");
    let error = computer
        .resolve_read_path("relative/path")
        .await
        .unwrap_err();
    assert_eq!(reason(&error), "invalid_path");
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_escape_dangling_and_root_retarget() {
    use std::os::unix::fs::symlink;

    let ctx = Ctx::new();
    let outside = ctx.tmp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    let outside_file = outside.join("secret.txt");
    fs::write(&outside_file, b"secret").unwrap();

    // 符号链接越界 → forbidden。
    let escape = ctx.share.join("escape.txt");
    symlink(&outside_file, &escape).unwrap();
    // 悬空链接（root 内）→ not_found；悬空链接指向 root 外 → forbidden。
    let dangling_inside = ctx.share.join("dangling.txt");
    symlink(ctx.share.join("missing.txt"), &dangling_inside).unwrap();
    let dangling_outside = ctx.share.join("dangling-out.txt");
    symlink(outside.join("missing.txt"), &dangling_outside).unwrap();

    ctx.set_read_roots(&[&ctx.share]);
    let computer = ctx.computer();
    for (path, expected) in [
        (&escape, "forbidden"),
        (&dangling_inside, "not_found"),
        (&dangling_outside, "forbidden"),
    ] {
        let error = computer
            .resolve_read_path(path.to_str().unwrap())
            .await
            .unwrap_err();
        assert_eq!(reason(&error), expected, "path={path:?}");
    }

    // 经符号链接配置的 root：resolve 后换指向 → 拉取期 forbidden。
    let real_root = ctx.tmp.path().join("real-root");
    let other_root = ctx.tmp.path().join("other-root");
    fs::create_dir_all(&real_root).unwrap();
    fs::create_dir_all(&other_root).unwrap();
    fs::write(real_root.join("f.txt"), b"from-real").unwrap();
    fs::write(other_root.join("f.txt"), b"from-other").unwrap();
    let link_root = ctx.tmp.path().join("share-link");
    symlink(&real_root, &link_root).unwrap();

    ctx.set_read_roots(&[&link_root]);
    let computer = ctx.computer();
    let resolved = computer
        .resolve_read_path(link_root.join("f.txt").to_str().unwrap())
        .await
        .unwrap();
    let payload = match decode_blob_handle(&resolved.blob_handle).unwrap() {
        DecodedHandle::Path(payload) => payload,
        other => panic!("unexpected kind {}", other.kind()),
    };

    fs::remove_file(&link_root).unwrap();
    symlink(&other_root, &link_root).unwrap();
    let error = computer.pull_path_blob(&payload).await.unwrap_err();
    assert_eq!(pull_reason(&error), "forbidden");

    // 换向后新 resolve 成功并命中新目标（对齐 python root symlink retarget）。
    let new_resolved = computer
        .resolve_read_path(link_root.join("f.txt").to_str().unwrap())
        .await
        .unwrap();
    assert_eq!(
        new_resolved.sha256,
        smcp::utils::hash::sha256_hex(b"from-other")
    );
    assert!(
        new_resolved.canonical_path.contains("other-root"),
        "canonical={}",
        new_resolved.canonical_path
    );
}

#[tokio::test]
async fn resolve_then_delete_is_gone_and_revoke_root_is_forbidden() {
    let ctx = Ctx::new();
    let file = ctx.file("gone.txt", b"bye");
    ctx.set_read_roots(&[&ctx.share]);
    let computer = ctx.computer();
    let resolved = computer
        .resolve_read_path(file.to_str().unwrap())
        .await
        .unwrap();
    let payload = match decode_blob_handle(&resolved.blob_handle).unwrap() {
        DecodedHandle::Path(payload) => payload,
        other => panic!("unexpected kind {}", other.kind()),
    };

    // 删除 → 4018 gone。
    fs::remove_file(&file).unwrap();
    let error = computer.pull_path_blob(&payload).await.unwrap_err();
    assert_eq!(pull_reason(&error), "gone");

    // 撤权（settings 去掉 readRoots）→ 4018 forbidden。
    let file2 = ctx.file("revoked.txt", b"x");
    let resolved = computer
        .resolve_read_path(file2.to_str().unwrap())
        .await
        .unwrap();
    let payload = match decode_blob_handle(&resolved.blob_handle).unwrap() {
        DecodedHandle::Path(payload) => payload,
        other => panic!("unexpected kind {}", other.kind()),
    };
    // 根列表**非空**但移除目标 root（避免空根短路，真正走围栏拒绝）。
    let other_root = ctx.tmp.path().join("other-root");
    fs::create_dir_all(&other_root).unwrap();
    ctx.write_settings(json!({"readRoots": [other_root.to_string_lossy()]}));
    let error = computer.pull_path_blob(&payload).await.unwrap_err();
    assert_eq!(pull_reason(&error), "forbidden");
    // 撤权后**新 resolve** 同样 4020 forbidden（external settings rewrite 无需刷新）。
    let error = computer
        .resolve_read_path(file2.to_str().unwrap())
        .await
        .unwrap_err();
    assert_eq!(reason(&error), "forbidden");
}

#[tokio::test]
async fn rewrite_after_resolve_is_gone() {
    let ctx = Ctx::new();
    let file = ctx.file("rewrite.txt", b"before");
    ctx.set_read_roots(&[&ctx.share]);
    let computer = ctx.computer();
    let resolved = computer
        .resolve_read_path(file.to_str().unwrap())
        .await
        .unwrap();
    let payload = match decode_blob_handle(&resolved.blob_handle).unwrap() {
        DecodedHandle::Path(payload) => payload,
        other => panic!("unexpected kind {}", other.kind()),
    };
    // 改写（尺寸变化）→ stat 键不符 → gone。
    fs::write(&file, b"after-longer").unwrap();
    let error = computer.pull_path_blob(&payload).await.unwrap_err();
    assert_eq!(pull_reason(&error), "gone");
}

/// 对照 python `test_project_scope_read_roots_is_ignored`：project scope（入 git）提供
/// `readRoots` → 整字段不生效（PR2a schema 门控），仍不可读。
#[tokio::test]
async fn project_scope_read_roots_is_ignored() {
    let ctx = Ctx::new();
    let file = ctx.file("a.txt", b"x");
    ctx.set_read_roots(&[]); // user scope 未授权
    let governance = ctx.config_dir.join(".tfrobot");
    fs::create_dir_all(&governance).unwrap();
    fs::write(
        governance.join("settings.json"),
        json!({"readRoots": [ctx.share.to_string_lossy()]}).to_string(),
    )
    .unwrap();
    let computer = ctx.computer();
    let error = computer
        .resolve_read_path(file.to_str().unwrap())
        .await
        .unwrap_err();
    assert_eq!(reason(&error), "forbidden");
}

/// 同长度覆写（同 inode、新 mtime）→ stat 键不符 → 4018 gone（钉住 mtime 分支）。
#[tokio::test]
async fn same_length_rewrite_is_gone() {
    let ctx = Ctx::new();
    let file = ctx.file("same.txt", b"ABCDEF");
    ctx.set_read_roots(&[&ctx.share]);
    let computer = ctx.computer();
    let resolved = computer
        .resolve_read_path(file.to_str().unwrap())
        .await
        .unwrap();
    let payload = match decode_blob_handle(&resolved.blob_handle).unwrap() {
        DecodedHandle::Path(payload) => payload,
        other => panic!("unexpected kind {}", other.kind()),
    };
    // 同长度覆写：截断重写保持 inode；显式把 mtime 推后 1s 确保毫秒键变化。
    fs::write(&file, b"abcdef").unwrap();
    let handle = fs::File::options().write(true).open(&file).unwrap();
    let new_mtime =
        fs::metadata(&file).unwrap().modified().unwrap() + std::time::Duration::from_secs(1);
    handle
        .set_times(fs::FileTimes::new().set_modified(new_mtime))
        .unwrap();
    drop(handle);
    assert_eq!(fs::metadata(&file).unwrap().len(), 6);

    let error = computer.pull_path_blob(&payload).await.unwrap_err();
    assert_eq!(pull_reason(&error), "gone");
}

#[tokio::test]
async fn fixed_rejection_set_skillenv_and_landing_part() {
    let ctx = Ctx::new();
    let skillenv = ctx.file(".skillenv", b"SECRET=1");
    let landing = ctx.tmp.path().join("landing");
    fs::create_dir_all(&landing).unwrap();
    let part = landing.join("upload.part");
    fs::write(&part, b"partial").unwrap();

    ctx.set_read_roots_and_landing(&[&ctx.tmp.path().to_path_buf()], &landing);
    let computer = ctx.computer();

    let error = computer
        .resolve_read_path(skillenv.to_str().unwrap())
        .await
        .unwrap_err();
    assert_eq!(reason(&error), "forbidden");

    let error = computer
        .resolve_read_path(part.to_str().unwrap())
        .await
        .unwrap_err();
    assert_eq!(reason(&error), "forbidden");

    // 正对照：同 root 普通文件、landing root 下非 `.part` 文件必须可读（防整根误拒）。
    let normal = ctx.file("normal.txt", b"ok");
    let resolved = computer
        .resolve_read_path(normal.to_str().unwrap())
        .await
        .unwrap();
    assert_eq!(resolved.total_size, 2);
    let landing_ok = landing.join("upload.txt");
    fs::write(&landing_ok, b"landed").unwrap();
    let resolved = computer
        .resolve_read_path(landing_ok.to_str().unwrap())
        .await
        .unwrap();
    assert_eq!(resolved.total_size, 6);
}

/// protocol#76：macOS/Windows 上 `root/.SKILLENV` → 4020 forbidden（名称谓词折叠）。
#[cfg(any(target_os = "macos", windows))]
#[tokio::test]
async fn skillenv_uppercase_is_forbidden_on_case_insensitive_hosts() {
    let ctx = Ctx::new();
    ctx.file(".skillenv", b"SECRET=1");
    let upper = ctx.share.join(".SKILLENV");
    // 默认 APFS/NTFS 大小写不敏感；大小写敏感卷上该路径是不同文件，本用例不适用。
    if fs::metadata(&upper).is_err() {
        return;
    }
    ctx.set_read_roots(&[&ctx.share]);
    let computer = ctx.computer();
    let error = computer
        .resolve_read_path(upper.to_str().unwrap())
        .await
        .unwrap_err();
    assert_eq!(reason(&error), "forbidden");

    // landing root 上 `UPLOAD.PART`（磁盘拼写）同样 forbidden（protocol#76 `.part` 折叠）。
    let landing = ctx.tmp.path().join("Landing");
    fs::create_dir_all(&landing).unwrap();
    fs::write(landing.join("UPLOAD.PART"), b"partial").unwrap();
    let tmp_root = ctx.tmp.path().to_path_buf();
    ctx.set_read_roots_and_landing(&[&tmp_root], &landing);
    let computer = ctx.computer();
    let error = computer
        .resolve_read_path(landing.join("UPLOAD.PART").to_str().unwrap())
        .await
        .unwrap_err();
    assert_eq!(reason(&error), "forbidden");
}

#[tokio::test]
async fn non_ascii_root_and_file_roundtrip() {
    let ctx = Ctx::new();
    let dir = ctx.tmp.path().join("共享");
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join("文件.txt");
    fs::write(&file, b"unicode-content").unwrap();
    ctx.set_read_roots(&[&dir]);
    let computer = ctx.computer();

    let resolved = computer
        .resolve_read_path(file.to_str().unwrap())
        .await
        .unwrap();
    assert!(resolved.canonical_path.ends_with("文件.txt"));
    let payload = match decode_blob_handle(&resolved.blob_handle).unwrap() {
        DecodedHandle::Path(payload) => payload,
        other => panic!("unexpected kind {}", other.kind()),
    };
    let pulled = computer.pull_path_blob(&payload).await.unwrap();
    assert_eq!(pulled.sha256, resolved.sha256);
    assert_eq!(
        pulled.slice(0, pulled.total_size).unwrap(),
        b"unicode-content".to_vec()
    );
}

#[tokio::test]
async fn header_mapping_uses_resolve_reason_enum() {
    // 4020 reason 的枚举字符串与 wire 口径固定（供 handler/对拍引用）。
    assert_eq!(ResolveReason::InvalidPath.as_str(), "invalid_path");
    assert_eq!(ResolveReason::Forbidden.as_str(), "forbidden");
    assert_eq!(ResolveReason::NotFound.as_str(), "not_found");
    assert_eq!(ResolveReason::NotAFile.as_str(), "not_a_file");
}
