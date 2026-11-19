//! 真实 CLI 二进制 / git 夹具共享 helper（#278 抽取；消除 CLI 测试逐字复制）。
//!
//! 用法：`#[path = "common/cli_binary.rs"] mod cli_binary;`。文件本身**不**做 cli feature
//! gate——`git` / `build_relative_source_repo` / `json_stdout` 在无 cli 的测试里也可用；
//! `run_binary` 依赖 `CARGO_BIN_EXE_smcp-computer`，仅在 cli feature 下编译。
#![allow(dead_code)]

use std::fs;
use std::path::Path;
use std::process::Command;

/// 运行 git（GIT_* 环境隔离，保证 hermetic；HOME 指向 cwd 防读用户 gitconfig）。
pub fn git(args: &[&str], cwd: &Path) {
    let out = Command::new("git")
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

/// marketplace 源仓库：plugin `alpha`（相对 source `./plugins/alpha`）+ skill `code-review`。
///
/// `marketplace_name` 写入清单顶层 `name`（#272 起注册名以清单为准）；返回 `file://…` URL。
pub fn build_relative_source_repo(
    repo: &Path,
    marketplace_name: &str,
    description: &str,
) -> String {
    build_relative_source_repo_with_plugins(repo, marketplace_name, &[("alpha", description)])
}

/// 多插件 marketplace 源仓库：每个插件一个相对 source `./plugins/<name>` + `skills/code-review`。
///
/// `marketplace_name` 写入清单顶层 `name`（#272 起注册名以清单为准）；返回 `file://…` URL。
pub fn build_relative_source_repo_with_plugins(
    repo: &Path,
    marketplace_name: &str,
    plugins: &[(&str, &str)],
) -> String {
    fs::create_dir_all(repo.join(".tfrobot-plugin")).unwrap();
    let entries: Vec<serde_json::Value> = plugins
        .iter()
        .map(|(name, _)| serde_json::json!({"name": name, "source": format!("./plugins/{name}")}))
        .collect();
    fs::write(
        repo.join(".tfrobot-plugin/marketplace.json"),
        serde_json::to_string(&serde_json::json!({ "name": marketplace_name, "plugins": entries }))
            .unwrap(),
    )
    .unwrap();
    for (name, description) in plugins {
        let skill = repo.join("plugins").join(name).join("skills/code-review");
        fs::create_dir_all(&skill).unwrap();
        fs::write(
            skill.join("SKILL.md"),
            format!("---\nname: code-review\ndescription: {description}\n---\nbody"),
        )
        .unwrap();
    }
    git(&["init", "-q"], repo);
    git(&["add", "-A"], repo);
    git(&["commit", "-qm", "init"], repo);
    format!("file://{}", repo.display())
}

/// 解析 CLI `--json` stdout。
pub fn json_stdout(output: &std::process::Output) -> serde_json::Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout 应为 JSON: {e}; stdout={stdout}"))
}

/// 用隔离 HOME / XDG_CONFIG_HOME / A2C_SKILL_HOME 运行 smcp-computer 二进制。
#[cfg(feature = "cli")]
pub fn run_binary(home: &Path, skill_home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_smcp-computer"))
        .args(args)
        .current_dir(home)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("xdg-config"))
        .env("A2C_SKILL_HOME", skill_home)
        .output()
        .expect("smcp-computer binary runs")
}
