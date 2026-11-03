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
//!   **不静默重生成、不自动覆写**；重置身份只能走显式入口 [`reset_computer_id`]。
//! - 重置：生成新 UUID 并**原子替换**持久化文件（同目录临时文件 + rename；Unix 原子，
//!   Unix/Windows 均为替换语义）。rename 后父目录 fsync 失败仅告警（身份已替换）。
//!   空文件 / 半写 / 非 UTF-8 等崩溃窗口残留可经重置直接重建。
//!
//! 本模块只做「读取或首建」与「显式重置」，其余删除语义不在此处。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::errors::{ComputerError, ComputerResult};
use crate::settings::{resolve_user_config_dir, EnvMap, A2C_CONFIG_SUBDIR};
use crate::skills::home::create_private_dir;

/// 身份文件名（位于 user-config 目录 `<config>/a2c/` 下）。
pub const COMPUTER_ID_FILENAME: &str = "computer-id";

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
/// - **注入 `env`**：必须自带**绝对** `HOME` 或**绝对** `XDG_CONFIG_HOME`；否则返回相对路径哨兵，
///   由 [`require_absolute`] 转成明确错误——**绝不**静默回退宿主进程 `$HOME`。仅检查键存在是不够的：
///   `HOME="~"` / `"~/x"` 会经 `expanduser` 用**宿主进程** HOME 展开，仍会把身份写进真实用户目录。
/// - **ambient（`env = None`）**：无 `HOME` 时（Windows 原生 / systemd 守护进程）底层解析会给出
///   字面 `~` 相对路径，这里改用 `dirs::home_dir()`（USERPROFILE / getpwuid）兜底。
fn resolve_identity_config_dir(env: Option<&EnvMap>) -> PathBuf {
    if let Some(map) = env {
        // 环境变量名与 settings 模块保持一致（`HOME` / `XDG_CONFIG_HOME`）。两者都必须是绝对路径：
        // `~` 前缀由 `expanduser` 展开，而它读的是**进程**环境，不是注入映射。
        let has_absolute_home = map
            .get("HOME")
            .map(|value| Path::new(value).is_absolute())
            .unwrap_or(false);
        let has_absolute_xdg = map
            .get(crate::settings::XDG_CONFIG_HOME_ENV)
            .map(|value| Path::new(value).is_absolute())
            .unwrap_or(false);
        if !has_absolute_home && !has_absolute_xdg {
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
    let path = computer_id_path(env);
    let env = env.cloned();
    // 阻塞池 join 失败（任务 panic / 运行时关闭）同样是**身份不可用**：统一映射为
    // `IdentityStore`，让 CLI 的启动致命分类与 `error_code() == 500` 口径保持一致。
    tokio::task::spawn_blocking(move || {
        load_or_create_computer_id_blocking(env.as_ref(), READ_RETRY_WINDOW)
    })
    .await
    .map_err(|error| identity_error(&path, format!("identity task failed: {error}")))?
}

/// 重置身份：生成新 UUID 并**原子替换**持久化文件 / Reset the identity atomically.
///
/// - 同目录临时文件（0600）+ `fsync` 后 `rename` 覆盖目标（Unix/Windows 均为替换语义）；
///   替换后父目录 fsync 失败只告警并继续（身份已落盘）；
/// - 目标文件缺失 / 空 / 半写 / 非 UTF-8 损坏 ⇒ 直接以新值重建（崩溃窗口恢复路径）；
/// - 目录不存在 ⇒ 0700 创建；失败一律 [`ComputerError::IdentityStore`]，临时文件会被清理；
/// - **连接中不可调用**（新身份只能经新连接生效）：由 [`crate::computer::Computer::reset_computer_id`]
///   与 CLI `identity reset` 在上层拒绝/编排。
pub async fn reset_computer_id(env: Option<&EnvMap>) -> ComputerResult<String> {
    let path = computer_id_path(env);
    let blocking_path = path.clone();
    tokio::task::spawn_blocking(move || reset_computer_id_blocking(&blocking_path))
        .await
        .map_err(|error| identity_error(&path, format!("identity reset task failed: {error}")))?
}

fn reset_computer_id_blocking(path: &Path) -> ComputerResult<String> {
    require_absolute(path)?;
    let computer_id = uuid::Uuid::new_v4().to_string();
    if let Some(dir) = path.parent() {
        create_private_dir(dir).map_err(|error| {
            identity_error(path, format!("cannot create identity directory: {error}"))
        })?;
    }

    // 同目录临时文件（隐藏名 + UUID 去重）：写到一半失败时清理，不污染目标文件。
    let temp = path.with_file_name(format!(".{COMPUTER_ID_FILENAME}.tmp-{computer_id}"));
    let write_result = (|| -> std::io::Result<()> {
        let mut file = open_private_new_file(&temp)?;
        write_identity(&mut file, &computer_id)
    })();
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&temp);
        return Err(identity_error(
            path,
            format!("cannot write identity file: {error}"),
        ));
    }

    // `fs::rename` 在 Unix 与 Windows 上均为**替换语义**（Windows 走 MoveFileExW /
    // SetFileInformationByHandle）——不做「先删后换」：预删会制造「目标消失→并发进程重建」
    // 的非原子窗口。
    if let Err(error) = std::fs::rename(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(identity_error(
            path,
            format!("cannot replace identity file: {error}"),
        ));
    }
    // rename 已成功 ⇒ 磁盘上**已是新身份**。父目录 fsync 失败只影响「崩溃后目录项是否持久」，
    // 此时返回错误会让调用方在断开后中止（用户离线，却已换身份）。记录告警并按已替换处理，
    // 让上层继续用新身份重连回房。
    if let Err(error) = sync_parent_dir(path) {
        tracing::warn!(
            path = %path.display(),
            %error,
            "identity file replaced but parent directory sync failed; continuing with the new identity"
        );
    }
    Ok(computer_id)
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
            // 非 UTF-8 字节（典型：崩溃窗口留下的半写/替换残片）按**内容损坏**处理，
            // 给出与非法内容一致的处置指引（重置或删除重建），而不是晦涩的 IO 文案。
            Err(error) if error.kind() == std::io::ErrorKind::InvalidData => (
                "invalid identity content (file is not valid UTF-8) — run \
                 `smcp-computer identity reset` to regenerate the identity"
                    .to_string(),
                false,
            ),
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
        "resolved identity path is not absolute — check HOME / XDG_CONFIG_HOME \
         (both must be absolute)",
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
    if !smcp::is_canonical_computer_id(body) {
        return None;
    }
    Some(body.to_string())
}

fn describe_invalid(content: &str) -> String {
    format!(
        "invalid identity content (expected lowercase canonical UUID, non-nil/max; got {} bytes) \
         — run `smcp-computer identity reset` to regenerate the identity",
        content.len()
    )
}

/// 首建：目录 0700、文件 0600、`O_CREAT|O_EXCL`（先创建者赢）。
///
/// 写/同步失败时**清理自己刚创建的文件**并上抛：该文件由本进程以 `O_EXCL` 新建，绝不可能
/// 属于既有持久值，删掉是安全的；若留着会让后续每次启动都读到半写内容而报 IdentityStore。
/// 崩溃窗口里目录项可能尚未落盘，因此成功后必须对父目录 fsync；该步失败同样上抛（协议要求
/// 持久化失败必须终止启动），但**不删除**已写入的文件——文件内容有效，下次启动会复用它，
/// 删除反而会造成静默换身份。
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
    sync_parent_dir(path)?;
    Ok(computer_id)
}

fn write_identity(file: &mut std::fs::File, computer_id: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    file.write_all(computer_id.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()
}

/// 首次创建后同步父目录，保证目录项在崩溃后仍可见（POSIX：文件 fsync 不保证目录项持久）。
/// 失败向上传播——静默吞掉会让「身份已持久化」成为未兑现的承诺。
fn sync_parent_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    if let Some(dir) = path.parent() {
        std::fs::File::open(dir)?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
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
        assert!(smcp::is_canonical_computer_id(&first), "{first}");
        assert_ne!(first, smcp::NIL_UUID);
        assert_ne!(first, smcp::MAX_UUID);

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
                Err(ComputerError::IdentityStore {
                    path: reported,
                    reason,
                }) => {
                    assert_eq!(reported, path.display().to_string());
                    // 仅「内容无效」分支给删除重建的处置指引。
                    assert!(reason.contains("identity reset"), "{reason}");
                }
                other => panic!("损坏内容 {corrupt:?} 应返回 IdentityStore，实得 {other:?}"),
            }
            // 不自动覆写：文件原样保留。
            assert_eq!(std::fs::read_to_string(&path).unwrap(), corrupt);
        }
    }

    /// 注入 env 的 OR 语义与端到端拒绝：
    ///
    /// (a) `HOME` 缺省 + **绝对** `XDG_CONFIG_HOME` ⇒ 接受；
    /// (b) `HOME` 缺省/相对/空 + `XDG` 缺省/相对/空 ⇒ 拒绝（绝不复用进程 HOME）；
    /// (c) 端到端：无效 env 返回 `IdentityStore` 且**不产生任何落盘**（CWD 哨兵目录与进程
    ///     HOME 身份文件均不变）——锁住「先 require_absolute 再碰文件系统」的顺序。
    ///
    /// 关键回归：`HOME="~"` 键**存在**但非绝对，旧实现只看键存在性，会经 `expanduser` 用宿主
    /// 进程 HOME 展开，把身份写进真实 `~/.config/a2c`。
    #[test]
    fn test_injected_env_home_xdg_or_semantics() {
        let tmp = tempfile::tempdir().unwrap();
        let abs_xdg = tmp.path().join("xdg");

        // (a) 绝对 XDG 单独即可（HOME 缺省）。
        let mut env = HashMap::new();
        env.insert("XDG_CONFIG_HOME".to_string(), abs_xdg.display().to_string());
        let path = computer_id_path(Some(&env));
        assert!(require_absolute(&path).is_ok(), "{}", path.display());
        assert!(path.starts_with(&abs_xdg));

        // (b) 两端都不满足 ⇒ 拒绝。
        for (home, xdg) in [
            (None, None),
            (None, Some("")),
            (None, Some("relative/xdg")),
            (Some(""), None),
            (Some("~"), None),
            (Some("~/x"), Some("")),
            (Some("relative/home"), Some("relative/xdg")),
        ] {
            let mut env = HashMap::new();
            if let Some(home) = home {
                env.insert("HOME".to_string(), home.to_string());
            }
            if let Some(xdg) = xdg {
                env.insert(
                    crate::settings::XDG_CONFIG_HOME_ENV.to_string(),
                    xdg.to_string(),
                );
            }
            let path = computer_id_path(Some(&env));
            assert!(
                require_absolute(&path).is_err(),
                "HOME={home:?} XDG={xdg:?} 必须拒绝，实得 {}",
                path.display()
            );
            if let Ok(process_home) = std::env::var("HOME") {
                assert!(
                    !path.starts_with(&process_home),
                    "HOME={home:?} XDG={xdg:?} 不得解析到进程 HOME：{}",
                    path.display()
                );
            }
        }

        // (c) 端到端：拒绝且不落盘。哨兵路径由 `computer_id_path` 派生（而非手工拼），
        // 并额外检查 CWD 下字面 `~` 目录本身不存在（防止实现往 CWD 建树）。
        let mut invalid = HashMap::new();
        invalid.insert("HOME".to_string(), "~".to_string());
        let sentinel = computer_id_path(Some(&invalid));
        assert!(
            !sentinel.is_absolute(),
            "哨兵必须保持相对：{}",
            sentinel.display()
        );
        let sentinel_existed = sentinel.exists();
        let cwd_tilde_existed = Path::new("~").exists();
        let process_identity = std::env::var("HOME").ok().map(|home| {
            Path::new(&home)
                .join(".config")
                .join("a2c")
                .join(COMPUTER_ID_FILENAME)
        });
        let before = process_identity
            .as_ref()
            .and_then(|path| std::fs::read(path).ok());

        match load_or_create_computer_id_blocking(Some(&invalid), Duration::from_millis(20)) {
            Err(ComputerError::IdentityStore { reason, .. }) => {
                assert!(reason.contains("not absolute"), "{reason}");
            }
            other => panic!("无效注入 env 必须返回 IdentityStore，实得 {other:?}"),
        }

        assert_eq!(
            sentinel.exists(),
            sentinel_existed,
            "不得在原地创建哨兵身份文件"
        );
        assert_eq!(
            Path::new("~").exists(),
            cwd_tilde_existed,
            "不得在 CWD 创建字面 `~` 目录"
        );
        let after = process_identity
            .as_ref()
            .and_then(|path| std::fs::read(path).ok());
        assert_eq!(before, after, "不得改写进程 HOME 的身份文件");
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
        // 平台无关的绝对路径（Windows 上 `/tmp/...` 非绝对）。
        let absolute = std::env::temp_dir()
            .join("smcp-abs-check")
            .join("computer-id");
        assert!(require_absolute(&absolute).is_ok());
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
        match &result {
            Err(error @ ComputerError::IdentityStore { .. }) => {
                // 确定性错误不属于「内容无效」，不得给出删除/重置的误导性指引（按 Display 全文断言）。
                let displayed = error.to_string();
                assert!(
                    !displayed.contains("delete this file")
                        && !displayed.contains("identity reset"),
                    "确定性错误不得给出处置指引：{displayed}"
                );
            }
            other => panic!("确定性错误必须返回 IdentityStore，实得 {other:?}"),
        }
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

    /// protocol#64：重置生成新 UUID 并**原子替换**文件（新值 ≠ 旧值、单换行、0600、无临时残留）。
    #[cfg(unix)]
    #[test]
    fn test_reset_replaces_identity_atomically() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let env = xdg_env(tmp.path());
        let old = load_or_create_computer_id_blocking(Some(&env), READ_RETRY_WINDOW).unwrap();

        let new = reset_computer_id_blocking(&computer_id_path(Some(&env))).unwrap();
        assert!(smcp::is_canonical_computer_id(&new), "{new}");
        assert_ne!(old, new, "重置必须生成新身份");

        let path = computer_id_path(Some(&env));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), format!("{new}\n"));
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "重置后的文件仍须 0600");

        // 无临时文件残留。
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".tmp-"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "原子替换不得留下临时文件：{leftovers:?}"
        );
    }

    /// 崩溃窗口恢复：空文件 / 非 UTF-8 残片都可经**重置**直接重建；普通加载则给出重置指引。
    #[test]
    fn test_reset_recovers_crash_window_artifacts() {
        let tmp = tempfile::tempdir().unwrap();
        let env = xdg_env(tmp.path());
        let path = computer_id_path(Some(&env));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();

        // 空文件（O_EXCL 后未写完即 SIGKILL 的典型残留）。
        std::fs::write(&path, "").unwrap();
        let empty_reason =
            match load_or_create_computer_id_blocking(Some(&env), Duration::from_millis(20)) {
                Err(ComputerError::IdentityStore { reason, .. }) => reason,
                other => panic!("空文件应 IdentityStore，实得 {other:?}"),
            };
        assert!(empty_reason.contains("identity reset"), "{empty_reason}");
        let recovered = reset_computer_id_blocking(&path).unwrap();
        assert!(smcp::is_canonical_computer_id(&recovered));

        // 非 UTF-8 残片（读到即 InvalidData）：同样给重置指引，且重置可恢复。
        std::fs::write(&path, [0xff, 0xfe, 0x00]).unwrap();
        let non_utf8_reason =
            match load_or_create_computer_id_blocking(Some(&env), Duration::from_millis(20)) {
                Err(ComputerError::IdentityStore { reason, .. }) => reason,
                other => panic!("非 UTF-8 应 IdentityStore，实得 {other:?}"),
            };
        assert!(
            non_utf8_reason.contains("not valid UTF-8")
                && non_utf8_reason.contains("identity reset"),
            "{non_utf8_reason}"
        );
        let recovered_again = reset_computer_id_blocking(&path).unwrap();
        assert!(smcp::is_canonical_computer_id(&recovered_again));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("{recovered_again}\n")
        );
    }

    /// 重置同样要求绝对路径（无效注入 env ⇒ IdentityStore，且不落盘）。
    #[test]
    fn test_reset_rejects_relative_path() {
        let mut env = HashMap::new();
        env.insert("HOME".to_string(), "~".to_string());
        let path = computer_id_path(Some(&env));
        assert!(!path.is_absolute());
        match reset_computer_id_blocking(&path) {
            Err(ComputerError::IdentityStore { reason, .. }) => {
                assert!(reason.contains("not absolute"), "{reason}");
            }
            other => panic!("相对路径重置必须 IdentityStore，实得 {other:?}"),
        }
    }

    /// 失败注入：目标路径是目录 ⇒ `rename` 失败，临时文件必须清理。
    #[test]
    fn test_reset_rename_failure_cleans_temp_file() {
        let tmp = tempfile::tempdir().unwrap();
        let env = xdg_env(tmp.path());
        let path = computer_id_path(Some(&env));
        std::fs::create_dir_all(&path).unwrap(); // 目标位置是目录 ⇒ rename 失败
        let leftovers = |dir: &Path| -> Vec<String> {
            std::fs::read_dir(dir)
                .unwrap()
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| name.contains(".tmp-"))
                .collect()
        };
        assert!(leftovers(path.parent().unwrap()).is_empty());

        let result = reset_computer_id_blocking(&path);
        assert!(
            matches!(result, Err(ComputerError::IdentityStore { .. })),
            "{result:?}"
        );
        let after = leftovers(path.parent().unwrap());
        assert!(after.is_empty(), "rename 失败必须清理临时文件：{after:?}");
    }

    /// 失败注入：身份目录无法创建（父路径是普通文件）⇒ `IdentityStore`，不产生临时文件。
    #[test]
    fn test_reset_directory_creation_failure_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let blocker = tmp.path().join("xdg-is-a-file");
        std::fs::write(&blocker, b"x").unwrap();
        let mut env = HashMap::new();
        env.insert("HOME".to_string(), tmp.path().display().to_string());
        env.insert("XDG_CONFIG_HOME".to_string(), blocker.display().to_string());
        let path = computer_id_path(Some(&env));

        match reset_computer_id_blocking(&path) {
            Err(ComputerError::IdentityStore { reason, .. }) => {
                assert!(reason.contains("directory"), "{reason}");
            }
            other => panic!("目录创建失败必须 IdentityStore，实得 {other:?}"),
        }
    }

    /// 失败注入（unix）：目录不可写 ⇒ `IdentityStore` 诊断。
    ///
    /// 失败发生在**临时文件创建之前**，不可能产生残留——本用例只锁诊断路径；临时文件清理
    /// 由 [`test_reset_rename_failure_cleans_temp_file`] 覆盖。
    #[cfg(unix)]
    #[test]
    fn test_reset_unwritable_directory_reports_identity_store() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let env = xdg_env(tmp.path());
        let path = computer_id_path(Some(&env));
        let dir = path.parent().unwrap().to_path_buf();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();

        let result = reset_computer_id_blocking(&path);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();

        // 失败发生在**临时文件创建之前**（目录不可写），不可能产生残留；本用例只锁
        // 「不可写目录 ⇒ IdentityStore」诊断路径。残留清理由 rename-失败用例覆盖。
        match result {
            Err(ComputerError::IdentityStore { reason, .. }) => {
                assert!(reason.contains("write identity file"), "{reason}");
            }
            other => panic!("写失败必须 IdentityStore，实得 {other:?}"),
        }
    }
}
