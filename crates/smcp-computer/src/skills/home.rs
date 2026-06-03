/*!
* 文件名: home.rs
* 作者: JQQ
* 创建日期: 2026/06/03
* 最后修改日期: 2026/06/03
* 版权: 2023 JQQ. All rights reserved.
* 依赖: smcp::utils::path (resolve_xdg_first / normalize_lexical)
* 描述: SKILL Home 路径解析 + 安装目录布局（对标 Python computer/skills/home.py）
*       SKILL Home resolution + install-dir layout.
*/

//! SKILL Home 路径解析 + 安装目录布局 / SKILL Home resolution + install-dir layout。
//!
//! 协议依据 / Protocol: a2c-smcp-protocol `skill.md` §4（本地安装位置判给 SDK）、§9.2（staging 隔离）。
//! 对标 Python 参考实现 / Mirrors the Python reference: `a2c_smcp/computer/skills/home.py`。
//!
//! 解析优先级（镜像 Claude Code「默认用户 home + env 覆盖」范式）/ Resolution priority:
//! 1. 环境变量 `A2C_SKILL_HOME`（显式覆盖；容器 / 多实例旋钮）/ env override.
//! 2. `$XDG_DATA_HOME/a2c/skills`（`XDG_DATA_HOME` 须为**绝对路径**，否则按 XDG 规范忽略）/ XDG-first.
//! 3. `~/.a2c/skills`（回退）/ fallback.
//!
//! 布局（skill.md §4 三级分组）/ Layout `<home>/<source>/.../<skill>/`:
//! - mcp:         `<home>/mcp/<server>/<skill>/`
//! - marketplace: `<home>/marketplace/<repo>/.../<skill>/`
//! - user:        `<home>/user/<skill>/`
//!
//! 跨用户隔离 / Cross-user isolation（对齐 Claude Code）：默认落到**每用户私有**路径（home / XDG），
//! 创建时以 `0o700` 防御性写，把「不跨用户共享」交给 **OS 文件权限**；**不**做 path 前缀 deny-list
//! （黑名单拦不全且 CC 亦不采用）。协议 §9.2 当前措辞为「MUST NOT 跨用户共享」，本实现按 CC 对齐落实
//! 「每用户默认 + `0o700` + 用户自负 env 覆盖」，**待**协议把该 MUST 校准为 SHOULD / 部署层指引。
//!
//! XDG-first 解析复用 [`smcp::utils::path::resolve_xdg_first`]（与 settings user config dir 同范式，
//! 见 [`crate::settings::scope`]）。

use std::path::{Path, PathBuf};

use smcp::utils::path::{normalize_lexical, resolve_xdg_first};

use crate::settings::EnvMap;

// ---------------------------------------------------------------------------
// 常量 / Constants
// ---------------------------------------------------------------------------
/// env 覆盖键（镜像 CC `CLAUDE_CODE_PLUGIN_CACHE_DIR` 范式）/ Env override key。
pub const SKILL_HOME_ENV: &str = "A2C_SKILL_HOME";
/// XDG data home 环境变量名 / XDG data-home env var。
pub const XDG_DATA_HOME_ENV: &str = "XDG_DATA_HOME";
/// `$HOME` 环境变量名（fallback 基目录）/ `$HOME` env var (fallback base)。
const HOME_ENV: &str = "HOME";

/// XDG 分支默认相对布局 `a2c/skills` / Default XDG-branch layout fragments。
const A2C_DATA_SUBPATH: [&str; 2] = ["a2c", "skills"];
/// fallback 分支相对布局 `.a2c/skills` / Fallback-branch layout fragments。
const DOTDIR_SUBPATH: [&str; 2] = [".a2c", "skills"];
/// workspace 登记工作目录内的 user 源 DropIn 子路径 `.tfrobot/skills` / Per-workdir DropIn subpath。
const WORKDIR_SKILL_SUBPATH: [&str; 2] = [".tfrobot", "skills"];

/// 每用户私有目录权限 / Per-user-private dir mode（防御性写，隔离交给 OS）。
#[cfg(unix)]
pub const SKILL_HOME_MODE: u32 = 0o700;

/// mcp source namespace 目录名 / mcp source-namespace dir name（skill.md §4）。
pub const SOURCE_MCP: &str = "mcp";
/// marketplace source namespace 目录名 / marketplace source-namespace dir name。
pub const SOURCE_MARKETPLACE: &str = "marketplace";
/// user source namespace 目录名 / user source-namespace dir name。
pub const SOURCE_USER: &str = "user";

// ---------------------------------------------------------------------------
// env 解析助手（镜像 settings::scope 范式，便于 hermetic 测试）/ env helpers
// ---------------------------------------------------------------------------
/// 从注入映射或进程环境取变量 / Look up a var from an injected map or the process env。
///
/// 与 [`crate::settings::scope`] 的私有 `env_lookup` 同语义：`None` → 读进程环境（生产）；`Some(map)`
/// → 读注入映射（测试隔离）。
fn env_lookup(env: Option<&EnvMap>, key: &str) -> Option<String> {
    match env {
        Some(m) => m.get(key).cloned(),
        None => std::env::var(key).ok(),
    }
}

/// 展开前导 `~` / Expand a leading `~`（用注入 env 的 `HOME`，便于 hermetic 测试；无 `HOME` 时原样保留）。
///
/// 仅前导 `~` / `~/...` 展开；中段 `~` 不动。区别于 [`smcp::utils::path`] 的私有 `expanduser`（读进程
/// `HOME`）——此处尊重注入 `env` 以保证 `resolve_skill_home` 测试可完全隔离进程环境。
fn expand_home(raw: &str, env: Option<&EnvMap>) -> PathBuf {
    if raw == "~" {
        if let Some(home) = env_lookup(env, HOME_ENV) {
            return PathBuf::from(home);
        }
    } else if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = env_lookup(env, HOME_ENV) {
            return Path::new(&home).join(rest);
        }
    }
    PathBuf::from(raw)
}

// ---------------------------------------------------------------------------
// 解析 / Resolution
// ---------------------------------------------------------------------------
/// 解析 SKILL Home 绝对路径 / Resolve the absolute SKILL Home path（**不**创建目录）。
///
/// 优先级 `A2C_SKILL_HOME` > `$XDG_DATA_HOME/a2c/skills` > `~/.a2c/skills`（见模块文档）。结果统一
/// 词法规范化为绝对路径；落盘由 staging 负责（见 [`ensure_skill_home`]）。不做 path deny-list 校验
/// ——跨用户隔离按 CC 对齐交给 OS 权限。
///
/// 相对 `A2C_SKILL_HOME`（非 `~` 前缀的相对值）在**解析时锚定到 CWD**（对齐 Python `Path.resolve()`
/// 的词法等价物），故返回值恒为绝对路径、且落盘位置在解析时即定格（不随后续 CWD 漂移）。
///
/// `env` 为 `None` 时读进程环境（生产）；测试注入隔离映射（含 `HOME` / `XDG_DATA_HOME` /
/// `A2C_SKILL_HOME`）保持 hermetic。
pub fn resolve_skill_home(env: Option<&EnvMap>) -> PathBuf {
    // 1. env 覆盖：最高优先级旋钮，绕过 XDG / fallback（空白视为未设置）。
    if let Some(raw) = env_lookup(env, SKILL_HOME_ENV) {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            let expanded = expand_home(trimmed, env);
            // 相对 override 锚定到 CWD（对齐 Python `Path.resolve()` 的词法等价物）：保证返回绝对路径，
            // 并在解析时即**定格**落盘位置（避免相对值下 ensure_skill_home 随 CWD 漂移）。`~` 展开后与
            // 绝对 override 已绝对，原样透传。
            let anchored = if expanded.is_absolute() {
                expanded
            } else if let Ok(cwd) = std::env::current_dir() {
                cwd.join(expanded)
            } else {
                expanded
            };
            return normalize_lexical(&anchored);
        }
    }
    // 2. `$XDG_DATA_HOME/a2c/skills`（须绝对，否则忽略）→ 3. `~/.a2c/skills` 回退。
    let xdg = env_lookup(env, XDG_DATA_HOME_ENV);
    // fallback 基目录：注入 / 进程 `$HOME`；皆缺时留 `~` 交由 `resolve_xdg_first` 的 expanduser 兜底。
    let home = env_lookup(env, HOME_ENV).unwrap_or_else(|| "~".to_string());
    let fallback = PathBuf::from(home)
        .join(DOTDIR_SUBPATH[0])
        .join(DOTDIR_SUBPATH[1]);
    resolve_xdg_first(xdg.as_deref(), &A2C_DATA_SUBPATH, &fallback)
}

/// 解析并创建 SKILL Home 目录 / Resolve and create the SKILL Home directory。
///
/// 解析同 [`resolve_skill_home`]，随后递归创建并以 `0o700`（POSIX）防御性写——保证**新建**的 home 为
/// 每用户私有。既存目录权限**不改**（与 CC / Python 一致，不对用户已有目录 chmod）；env 覆盖到不可写
/// 位置时由 OS 的 `mkdir` 自然 fail-fast。Windows 上 mode 被忽略，隔离由 per-user AppData ACL 承担。
///
/// 供 staging（SKL-04）启动时建根使用 / Used by staging to materialize the root at startup.
///
/// # Errors
/// 目标不可写 / 父路径冲突等 OS 错误透传 [`std::io::Error`]。
pub fn ensure_skill_home(env: Option<&EnvMap>) -> std::io::Result<PathBuf> {
    let home = resolve_skill_home(env);
    create_private_dir(&home)?;
    Ok(home)
}

/// 递归创建每用户私有目录（POSIX `0o700`）/ Recursively create a per-user-private dir。
#[cfg(unix)]
fn create_private_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    // recursive(true)：既存目录视为成功且**不**改其权限（仅新建目录套用 mode）。
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(SKILL_HOME_MODE)
        .create(path)
}

/// 递归创建目录（非 POSIX：隔离由平台 ACL 承担，忽略 mode）/ Recursive create (non-POSIX).
#[cfg(not(unix))]
fn create_private_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)
}

// ---------------------------------------------------------------------------
// 布局助手 / Layout helpers
// ---------------------------------------------------------------------------
/// mcp 源安装目录 / mcp-source install dir：`<home>/mcp/<server>/<skill>/`。
pub fn mcp_skill_dir(home: &Path, server: &str, skill: &str) -> PathBuf {
    home.join(SOURCE_MCP).join(server).join(skill)
}

/// marketplace 源安装目录 / marketplace-source install dir：`<home>/marketplace/<repo>/<...inner>/`。
///
/// `inner` 为仓库内命名空间路径段（通常以 `<plugin>/<skill>` 收尾），由 staging 按物化树传入。
pub fn marketplace_skill_dir(home: &Path, repo: &str, inner: &[&str]) -> PathBuf {
    let mut path = home.join(SOURCE_MARKETPLACE).join(repo);
    for seg in inner {
        path.push(seg);
    }
    path
}

/// user 源安装目录 / user-source install dir：`<home>/user/<skill>/`。
pub fn user_skill_dir(home: &Path, skill: &str) -> PathBuf {
    home.join(SOURCE_USER).join(skill)
}

/// SKILL Home 内的 user 源 DropIn 发现根 / Global user-source DropIn root：`<home>/user/`。
pub fn user_dropin_root(home: &Path) -> PathBuf {
    home.join(SOURCE_USER)
}

/// workspace 登记工作目录的 user 源 DropIn 发现根 / Per-workdir user-source DropIn root。
///
/// `<workdir>/.tfrobot/skills/`——**就地发现、不 staging 进 SKILL Home**（与 marketplace clone 树相反）。
pub fn workdir_skill_root(workdir: &Path) -> PathBuf {
    workdir
        .join(WORKDIR_SKILL_SUBPATH[0])
        .join(WORKDIR_SKILL_SUBPATH[1])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tempfile::TempDir;

    /// 构造仅含 `HOME` 的隔离 env（XDG / override 未设置）。
    fn home_only_env(home: &Path) -> EnvMap {
        let mut env = HashMap::new();
        env.insert(HOME_ENV.to_string(), home.to_string_lossy().into_owned());
        env
    }

    // ---- 解析优先级 / resolution priority ----
    #[test]
    fn test_override_absolute_wins() {
        let mut env = home_only_env(Path::new("/home/u"));
        env.insert(SKILL_HOME_ENV.to_string(), "/custom/skills".to_string());
        // 即便 XDG 也设置，override 仍最高优先级。
        env.insert(XDG_DATA_HOME_ENV.to_string(), "/xdg/data".to_string());
        assert_eq!(
            resolve_skill_home(Some(&env)),
            PathBuf::from("/custom/skills")
        );
    }

    #[test]
    fn test_override_tilde_expands_with_injected_home() {
        let mut env = home_only_env(Path::new("/home/alice"));
        env.insert(SKILL_HOME_ENV.to_string(), "~/my-skills".to_string());
        assert_eq!(
            resolve_skill_home(Some(&env)),
            PathBuf::from("/home/alice/my-skills")
        );
    }

    #[test]
    fn test_relative_override_anchored_to_cwd() {
        // 相对（非 `~`）A2C_SKILL_HOME 在解析时锚定到 CWD → 绝对路径（对齐 Python .resolve() 定格语义）。
        let mut env = home_only_env(Path::new("/home/u"));
        env.insert(SKILL_HOME_ENV.to_string(), "rel-skills".to_string());
        let cwd = std::env::current_dir().unwrap();
        let resolved = resolve_skill_home(Some(&env));
        assert!(resolved.is_absolute());
        assert_eq!(resolved, normalize_lexical(&cwd.join("rel-skills")));
    }

    #[test]
    fn test_blank_override_falls_through_to_xdg() {
        let mut env = home_only_env(Path::new("/home/u"));
        env.insert(SKILL_HOME_ENV.to_string(), "   ".to_string()); // 空白视为未设置
        env.insert(XDG_DATA_HOME_ENV.to_string(), "/xdg/data".to_string());
        assert_eq!(
            resolve_skill_home(Some(&env)),
            PathBuf::from("/xdg/data/a2c/skills")
        );
    }

    #[test]
    fn test_xdg_absolute_used() {
        let mut env = home_only_env(Path::new("/home/u"));
        env.insert(XDG_DATA_HOME_ENV.to_string(), "/xdg/data".to_string());
        assert_eq!(
            resolve_skill_home(Some(&env)),
            PathBuf::from("/xdg/data/a2c/skills")
        );
    }

    #[test]
    fn test_xdg_relative_ignored_falls_back_home() {
        let mut env = home_only_env(Path::new("/home/u"));
        // 相对 XDG_DATA_HOME 按规范忽略 → 回退 <HOME>/.a2c/skills。
        env.insert(XDG_DATA_HOME_ENV.to_string(), "relative/data".to_string());
        assert_eq!(
            resolve_skill_home(Some(&env)),
            PathBuf::from("/home/u/.a2c/skills")
        );
    }

    #[test]
    fn test_no_xdg_falls_back_home_dotdir() {
        let env = home_only_env(Path::new("/home/u"));
        assert_eq!(
            resolve_skill_home(Some(&env)),
            PathBuf::from("/home/u/.a2c/skills")
        );
    }

    // ---- 子目录布局互不重叠 / layout helpers disjoint ----
    #[test]
    fn test_layout_helpers() {
        let home = Path::new("/home/u/.a2c/skills");
        assert_eq!(
            mcp_skill_dir(home, "tfrobot-tools", "code-review"),
            PathBuf::from("/home/u/.a2c/skills/mcp/tfrobot-tools/code-review")
        );
        assert_eq!(
            user_skill_dir(home, "my-helper"),
            PathBuf::from("/home/u/.a2c/skills/user/my-helper")
        );
        assert_eq!(
            marketplace_skill_dir(home, "acme-skills", &["acme-audit", "audit"]),
            PathBuf::from("/home/u/.a2c/skills/marketplace/acme-skills/acme-audit/audit")
        );
        assert_eq!(
            user_dropin_root(home),
            PathBuf::from("/home/u/.a2c/skills/user")
        );
        assert_eq!(
            workdir_skill_root(Path::new("/work/proj")),
            PathBuf::from("/work/proj/.tfrobot/skills")
        );
        // user / marketplace / mcp 三类根互不重叠。
        assert_ne!(home.join(SOURCE_USER), home.join(SOURCE_MARKETPLACE));
        assert_ne!(home.join(SOURCE_MARKETPLACE), home.join(SOURCE_MCP));
    }

    // ---- ensure 创建语义 / ensure creates ----
    #[test]
    fn test_ensure_creates_dir_idempotent() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("xdg-data");
        let mut env = home_only_env(tmp.path());
        env.insert(
            XDG_DATA_HOME_ENV.to_string(),
            target.to_string_lossy().into_owned(),
        );
        let home = ensure_skill_home(Some(&env)).unwrap();
        assert!(home.is_dir());
        assert_eq!(home, target.join("a2c").join("skills"));
        // 幂等：再次 ensure 不报错。
        let again = ensure_skill_home(Some(&env)).unwrap();
        assert_eq!(again, home);
    }

    #[test]
    #[cfg(unix)]
    fn test_ensure_sets_private_mode_on_new_dir() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let mut env = home_only_env(tmp.path());
        env.insert(
            XDG_DATA_HOME_ENV.to_string(),
            tmp.path().join("xdg").to_string_lossy().into_owned(),
        );
        let home = ensure_skill_home(Some(&env)).unwrap();
        let mode = std::fs::metadata(&home).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, SKILL_HOME_MODE);
    }
}
