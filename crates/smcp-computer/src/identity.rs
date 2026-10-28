/*!
* 文件名: identity.rs
* 描述: Computer 稳定身份 `computer_id` 的生成与持久化 / Stable Computer identity persistence
*/

//! protocol#64（v0.5.0）：Computer 的稳定、与展示名 `name` 解耦的身份 `computer_id`。
//!
//! 语义（见协议 `docs/specification/room-model.md` §Computer 身份）：
//! - 随机 UUID v4（不得由硬件指纹派生，不得为 nil / max），小写规范形态（`8-4-4-4-12`）；
//! - 物化时机：首次 `connect_socketio`；此后每次 `server:join_office` 携带同一值；
//! - 持久化于 `$XDG_CONFIG_HOME/a2c/computer-id`（XDG 须为绝对路径，否则按规范忽略），
//!   回退 `~/.config/a2c/computer-id`；纯文本 + 换行，跨重启复用；
//! - 首建用 `O_CREAT|O_EXCL`（并发首启先创建者赢）；**新建**目录 0700（既存目录不改权限，
//!   与 settings 共用 `a2c` 目录时可能已是别的 mode）、文件 0600；
//! - 文件损坏 / 空 / 半写 ⇒ 短重试（≤1s）后返回 [`ComputerError::IdentityStore`]：
//!   **不静默重生成、不自动覆写**（重置身份是显式入口，见后续 PR）。
//!
//! 本模块只做「读取或首建」，不含重置/删除。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::errors::{ComputerError, ComputerResult};
use crate::settings::{resolve_user_config_dir, EnvMap, A2C_CONFIG_SUBDIR};
use crate::skills::home::create_private_dir;

/// 身份文件名（位于 user-config 目录 `<config>/a2c/` 下）。
pub const COMPUTER_ID_FILENAME: &str = "computer-id";

/// 规范 UUID 中的 nil / max：协议禁止 Computer 发出。
const NIL_UUID: &str = "00000000-0000-0000-0000-000000000000";
const MAX_UUID: &str = "ffffffff-ffff-ffff-ffff-ffffffffffff";

/// 损坏 / 空 / 半写时的短重试窗口（创建者可能刚 `O_CREAT|O_EXCL` 尚未写完）。
const READ_RETRY_WINDOW: Duration = Duration::from_secs(1);
const READ_RETRY_INTERVAL: Duration = Duration::from_millis(50);

/// 身份文件路径 / Path to the Computer identity file.
///
/// 与 user-scope settings 同锚点（`resolve_user_config_dir`）：注入 `env` 时按其解析，便于
/// hermetic 测试与嵌入式多实例；`None` 时读进程环境（CLI / ambient）。
pub fn computer_id_path(env: Option<&EnvMap>) -> PathBuf {
    resolve_identity_config_dir(env).join(COMPUTER_ID_FILENAME)
}

/// 解析身份所在 user-config 目录（`<base>/a2c`）。
///
/// - **注入 `env`**：必须自带 `HOME` 或绝对 `XDG_CONFIG_HOME`；否则返回相对路径哨兵，由
///   [`require_absolute`] 转成明确错误——**绝不**静默回退宿主进程 `$HOME`（否则嵌入式实例会把
///   身份写进真实用户目录，违背 `with_config_env` 的隔离承诺）。
/// - **ambient（`env = None`）**：无 `HOME` 时（Windows 原生 / systemd 守护进程）底层解析会给出
///   字面 `~` 相对路径，这里改用 `dirs::home_dir()`（USERPROFILE / getpwuid）兜底。
fn resolve_identity_config_dir(env: Option<&EnvMap>) -> PathBuf {
    if let Some(map) = env {
        // 环境变量名与 settings 模块保持一致（`HOME` / `XDG_CONFIG_HOME`）。
        let has_home = map.contains_key("HOME");
        let has_absolute_xdg = map
            .get(crate::settings::XDG_CONFIG_HOME_ENV)
            .map(|value| Path::new(value).is_absolute())
            .unwrap_or(false);
        if !has_home && !has_absolute_xdg {
            return PathBuf::from("~").join(".config").join(A2C_CONFIG_SUBDIR);
        }
        return resolve_user_config_dir(Some(map));
    }
    let resolved = resolve_user_config_dir(None);
    if resolved.is_absolute() {
        return resolved;
    }
    dirs::home_dir()
        .map(|home| home.join(".config").join(A2C_CONFIG_SUBDIR))
        .unwrap_or(resolved)
}

/// 读取已有身份；缺失则原子首建 / Read the persisted identity, creating it atomically if absent.
///
/// 错误一律为 [`ComputerError::IdentityStore`]，且**不修改**既有文件（损坏值原样保留）。
/// 文件 I/O（含 fsync）在阻塞线程池执行，不占用 Tokio worker。
pub async fn load_or_create_computer_id(env: Option<&EnvMap>) -> ComputerResult<String> {
    let env = env.cloned();
    tokio::task::spawn_blocking(move || {
        load_or_create_computer_id_blocking(env.as_ref(), READ_RETRY_WINDOW)
    })
    .await
    .map_err(|error| {
        ComputerError::RuntimeError(format!("computer identity task failed: {error}"))
    })?
}

/// 可注入重试窗口的同步实现（单元测试用毫秒级窗口，避免每条损坏向量都白等满生产窗口）。
///
/// 只对**可能瞬态**的失败重试：内容暂不完整（并发写者尚未写完）与 `AlreadyExists`
/// （另一进程刚抢到创建权）。权限拒绝、路径是目录、只读文件系统等确定性失败立即 fail-fast。
fn load_or_create_computer_id_blocking(
    env: Option<&EnvMap>,
    retry_window: Duration,
) -> ComputerResult<String> {
    let path = computer_id_path(env);
    // HOME / XDG 皆不可用时会解析出相对路径（字面 `~/.config/...`）：此时落盘位置取决于 CWD，
    // 跨重启会静默换身份——必须 fail-fast 而不是在 CWD 里建文件。
    require_absolute(&path)?;
    let deadline = Instant::now() + retry_window;
    loop {
        let (reason, retryable) = match std::fs::read_to_string(&path) {
            Ok(content) => match parse_computer_id(&content) {
                Some(id) => return Ok(id),
                None => (describe_invalid(&content), true),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match create_computer_id(&path) {
                    Ok(id) => return Ok(id),
                    // 并发首启：另一进程刚创建，回到读取分支等它写完。
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (
                        "identity file appeared concurrently; waiting for its content".to_string(),
                        true,
                    ),
                    Err(error) => (format!("cannot create identity file: {error}"), false),
                }
            }
            Err(error) => (format!("cannot read identity file: {error}"), false),
        };

        if !retryable || Instant::now() >= deadline {
            return Err(identity_error(&path, reason));
        }
        std::thread::sleep(READ_RETRY_INTERVAL);
    }
}

/// 身份路径必须绝对：相对路径依赖 CWD，跨重启会静默换身份（协议 MUST NOT 的退化）。
fn require_absolute(path: &Path) -> ComputerResult<()> {
    if path.is_absolute() {
        return Ok(());
    }
    Err(identity_error(
        path,
        "resolved identity path is not absolute (HOME/XDG_CONFIG_HOME missing or relative)",
    ))
}

fn identity_error(path: &Path, reason: impl Into<String>) -> ComputerError {
    ComputerError::IdentityStore {
        path: path.display().to_string(),
        reason: reason.into(),
    }
}

/// 解析文件内容：仅接受「小写规范 UUID + 可选换行」。
fn parse_computer_id(content: &str) -> Option<String> {
    let body = content.strip_suffix('\n').unwrap_or(content);
    let body = body.strip_suffix('\r').unwrap_or(body);
    if !is_canonical_lowercase_uuid(body) || body == NIL_UUID || body == MAX_UUID {
        return None;
    }
    Some(body.to_string())
}

fn describe_invalid(content: &str) -> String {
    format!(
        "invalid identity content (expected lowercase canonical UUID, non-nil/max; got {} bytes)",
        content.len()
    )
}

/// 严格形态校验：36 字符、连字符位置固定、其余为小写十六进制。
fn is_canonical_lowercase_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    bytes.iter().enumerate().all(|(index, byte)| match index {
        8 | 13 | 18 | 23 => *byte == b'-',
        _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(byte),
    })
}

/// 首建：目录 0700、文件 0600、`O_CREAT|O_EXCL`（先创建者赢）。
///
/// 写/同步失败时**清理自己刚创建的文件**并上抛：该文件由本进程以 `O_EXCL` 新建，绝不可能
/// 属于既有持久值，删掉是安全的；若留着会让后续每次启动都读到半写内容而报 IdentityStore。
/// 崩溃窗口里目录项可能尚未落盘，因此成功后对父目录 fsync（best-effort）。
fn create_computer_id(path: &Path) -> std::io::Result<String> {
    let computer_id = uuid::Uuid::new_v4().to_string();
    if let Some(dir) = path.parent() {
        create_private_dir(dir)?;
    }
    let mut file = open_private_new_file(path)?;
    let written = write_identity(&mut file, &computer_id);
    drop(file);
    if let Err(error) = written {
        let _ = std::fs::remove_file(path);
        return Err(error);
    }
    sync_parent_dir(path);
    Ok(computer_id)
}

fn write_identity(file: &mut std::fs::File, computer_id: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    file.write_all(computer_id.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()
}

/// 首次创建后同步父目录，保证目录项在崩溃后仍可见（POSIX：文件 fsync 不保证目录项持久）。
fn sync_parent_dir(path: &Path) {
    #[cfg(unix)]
    if let Some(dir) = path.parent() {
        if let Ok(handle) = std::fs::File::open(dir) {
            let _ = handle.sync_all();
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

fn open_private_new_file(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_map(home: &Path, xdg: Option<&Path>) -> EnvMap {
        let mut env = HashMap::new();
        env.insert("HOME".to_string(), home.display().to_string());
        if let Some(xdg) = xdg {
            env.insert("XDG_CONFIG_HOME".to_string(), xdg.display().to_string());
        }
        env
    }

    fn xdg_env(dir: &Path) -> EnvMap {
        // HOME 指向同一临时树，避免任何回退路径落到真实用户目录。
        env_map(dir, Some(dir))
    }

    #[test]
    fn test_path_prefers_xdg_then_home_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let xdg = tmp.path().join("xdg");
        let home = tmp.path().join("home");
        let with_xdg = computer_id_path(Some(&xdg_env(&xdg)));
        assert_eq!(with_xdg, xdg.join("a2c").join(COMPUTER_ID_FILENAME));

        let without_xdg = computer_id_path(Some(&env_map(&home, None)));
        assert_eq!(
            without_xdg,
            home.join(".config").join("a2c").join(COMPUTER_ID_FILENAME)
        );
    }

    #[test]
    fn test_create_then_reuse_across_restarts() {
        let tmp = tempfile::tempdir().unwrap();
        let env = xdg_env(tmp.path());
        let first = load_or_create_computer_id_blocking(Some(&env), READ_RETRY_WINDOW).unwrap();
        assert!(is_canonical_lowercase_uuid(&first), "{first}");
        assert_ne!(first, NIL_UUID);
        assert_ne!(first, MAX_UUID);

        let path = computer_id_path(Some(&env));
        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(raw, format!("{first}\n"), "文件应为 UUID + 换行");

        // 「重启」：同一持久化文件再次读取 ⇒ 同一身份，不重新生成。
        let second = load_or_create_computer_id_blocking(Some(&env), READ_RETRY_WINDOW).unwrap();
        assert_eq!(first, second);
    }

    #[cfg(unix)]
    #[test]
    fn test_create_uses_private_permissions() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let env = xdg_env(tmp.path());
        load_or_create_computer_id_blocking(Some(&env), READ_RETRY_WINDOW).unwrap();

        let path = computer_id_path(Some(&env));
        let file_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600, "身份文件应为 0600");
        let dir_mode = std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700, "身份目录应为 0700");
    }

    #[test]
    fn test_corrupt_file_fails_fast_without_overwrite() {
        for corrupt in [
            "",
            "\n",
            "not-a-uuid",
            "7c9e6679-7425-40de-944b-e07fc1f90ae7", // 合法 UUID 无换行也可接受
            "7C9E6679-7425-40DE-944B-E07FC1F90AE7\n", // 大写非法
            "7c9e6679742540de944be07fc1f90ae7\n",   // 无连字符非法
            "7c9e6679-7425-40de-944b\n",            // 半写（截断）非法
            "zz9e6679-7425-40de-944b-e07fc1f90ae7\n", // 非十六进制非法
            "00000000-0000-0000-0000-000000000000\n", // nil 非法
            "ffffffff-ffff-ffff-ffff-ffffffffffff\n", // max 非法
            "7c9e6679-7425-40de-944b-e07fc1f90ae7\nextra\n", // 尾随内容非法
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let env = xdg_env(tmp.path());
            let path = computer_id_path(Some(&env));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, corrupt).unwrap();

            // 用毫秒级重试窗口：生产 1s 窗口只是「等并发写者」的上界，与本用例语义无关。
            let result = load_or_create_computer_id_blocking(Some(&env), Duration::from_millis(40));
            let still_valid = corrupt == "7c9e6679-7425-40de-944b-e07fc1f90ae7";
            if still_valid {
                assert_eq!(result.unwrap(), corrupt, "无换行的规范 UUID 应被接受");
                continue;
            }
            match result {
                Err(ComputerError::IdentityStore { path: reported, .. }) => {
                    assert_eq!(reported, path.display().to_string());
                }
                other => panic!("损坏内容 {corrupt:?} 应返回 IdentityStore，实得 {other:?}"),
            }
            // 不自动覆写：文件原样保留。
            assert_eq!(std::fs::read_to_string(&path).unwrap(), corrupt);
        }
    }

    /// 注入 env 缺 HOME / 绝对 XDG ⇒ 拒绝（绝不静默回退进程 HOME，把身份写进真实用户目录）。
    #[test]
    fn test_injected_env_without_home_or_xdg_is_rejected() {
        let env = HashMap::new();
        let path = computer_id_path(Some(&env));
        assert!(
            require_absolute(&path).is_err(),
            "注入 env 缺 HOME/XDG 时必须拒绝，实得 {}",
            path.display()
        );
        if let Ok(process_home) = std::env::var("HOME") {
            assert!(
                !path.starts_with(&process_home),
                "注入 env 不得回退进程 HOME：{}",
                path.display()
            );
        }
    }

    /// 相对身份路径必须 fail-fast（不得在 CWD 建字面 `~` 目录、更不得静默换身份）。
    #[test]
    fn test_relative_path_is_rejected() {
        match require_absolute(Path::new("~/.config/a2c/computer-id")) {
            Err(ComputerError::IdentityStore { reason, .. }) => {
                assert!(reason.contains("not absolute"), "{reason}");
            }
            other => panic!("相对路径必须 fail-fast，实得 {other:?}"),
        }
        assert!(require_absolute(Path::new("/tmp/abs/computer-id")).is_ok());
    }

    /// 确定性读错误（身份路径是目录）不得白等重试窗口。
    #[test]
    fn test_deterministic_error_is_not_retried() {
        let tmp = tempfile::tempdir().unwrap();
        let env = xdg_env(tmp.path());
        let path = computer_id_path(Some(&env));
        std::fs::create_dir_all(&path).unwrap(); // 路径是目录 ⇒ 读取失败（非瞬态）
        let started = Instant::now();
        let result = load_or_create_computer_id_blocking(Some(&env), READ_RETRY_WINDOW);
        assert!(matches!(result, Err(ComputerError::IdentityStore { .. })));
        assert!(
            started.elapsed() < READ_RETRY_WINDOW / 2,
            "确定性错误不得等满重试窗口（实际 {:?}）",
            started.elapsed()
        );
    }

    /// 并发首启：8 个真实线程 + 起跑屏障，制造 `O_CREAT|O_EXCL` 竞态。
    #[test]
    fn test_concurrent_first_start_single_winner() {
        let tmp = tempfile::tempdir().unwrap();
        let env = xdg_env(tmp.path());
        let start = std::sync::Arc::new(std::sync::Barrier::new(8));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let env = env.clone();
            let start = std::sync::Arc::clone(&start);
            handles.push(std::thread::spawn(move || {
                start.wait();
                load_or_create_computer_id_blocking(Some(&env), READ_RETRY_WINDOW)
            }));
        }
        let mut ids = std::collections::BTreeSet::new();
        for handle in handles {
            ids.insert(handle.join().unwrap().unwrap());
        }
        assert_eq!(ids.len(), 1, "并发首启必须收敛到同一身份，实得 {ids:?}");
        let persisted = std::fs::read_to_string(computer_id_path(Some(&env))).unwrap();
        assert_eq!(persisted.trim_end(), ids.iter().next().unwrap());
    }
}
