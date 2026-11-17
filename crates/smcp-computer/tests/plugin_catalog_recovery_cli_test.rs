//! #278：0.5.0 旧卸载删 catalog 子树后的升级迁移——真实二进制端到端回归。
//!
//! 覆盖：① install 失败错误携带恢复指引且不回显 marketplace 名/URL（占位符，零凭据泄漏）；
//! ② 按迁移文档执行 `marketplace remove`（默认级联 + 撤销 trust）→ `marketplace add --trust`
//! （重 clone）→ 重装成功。文档：`docs/computer/marketplace-0.5.1-migration.md`。
#![cfg(feature = "cli")]

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::tempdir;

fn git(args: &[&str], cwd: &Path) {
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

/// marketplace 源仓库：plugin `alpha` 用**相对 source**（0.5.0 旧卸载的受害者形态）。
fn build_repo(repo: &Path) -> String {
    fs::create_dir_all(repo.join(".tfrobot-plugin")).unwrap();
    fs::write(
        repo.join(".tfrobot-plugin/marketplace.json"),
        r#"{"plugins": [{"name": "alpha", "source": "./plugins/alpha"}]}"#,
    )
    .unwrap();
    let skill = repo.join("plugins/alpha/skills/code-review");
    fs::create_dir_all(&skill).unwrap();
    fs::write(
        skill.join("SKILL.md"),
        "---\nname: code-review\ndescription: recovery fixture\n---\nbody",
    )
    .unwrap();
    git(&["init", "-q"], repo);
    git(&["add", "-A"], repo);
    git(&["commit", "-qm", "init"], repo);
    format!("file://{}", repo.display())
}

fn run(home: &Path, skill_home: &Path, args: &[&str]) -> Output {
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

fn json_stdout(output: &Output) -> Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout 应为 JSON: {e}; stdout={stdout}"))
}

fn assert_ok(output: &Output, what: &str) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{what} 应成功; stdout={stdout} stderr={stderr}"
    );
}

#[test]
fn catalog_subtree_deleted_recovery_via_remove_add_reinstall_278() {
    let dir = tempdir().unwrap();
    let home = dir.path();
    let skill_home = home.join("skill-home");
    fs::create_dir_all(&skill_home).unwrap();
    let url = build_repo(&home.join("mp-src"));
    // 名字取 token 形态：错误提示必须用占位符，绝不回显它（#279/#285 同源泄漏面）。
    let mp = "acme-ghp-fake-token";
    let pid = format!("alpha@{mp}");
    let catalog_skill = skill_home
        .join("marketplace")
        .join(mp)
        .join("plugins/alpha/skills/code-review/SKILL.md");

    // ① add（非交互必须 --trust）→ ② install
    let out = run(
        home,
        &skill_home,
        &[
            "marketplace",
            "add",
            &url,
            "--name",
            mp,
            "--trust",
            "--json",
        ],
    );
    assert_ok(&out, "marketplace add");
    let out = run(home, &skill_home, &["plugin", "install", &pid, "--json"]);
    assert_ok(&out, "首次 install");
    assert_eq!(json_stdout(&out)["installed"], pid.as_str());
    assert!(catalog_skill.exists(), "catalog 内相对源 SKILL.md 应落盘");

    // ③ 模拟 0.5.0 旧卸载后果：删除共享 catalog 内的相对源子树。
    fs::remove_dir_all(
        skill_home
            .join("marketplace")
            .join(mp)
            .join("plugins/alpha"),
    )
    .unwrap();

    // ④ 直接重装失败，错误给出恢复指引；不得回显 marketplace 名（含 token）或源 URL。
    let out = run(home, &skill_home, &["plugin", "install", &pid, "--json"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(1),
        "损坏态 install 应失败; stdout={stdout}"
    );
    let msg = json_stdout(&out)["message"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        msg.contains("plugin root not found after source resolution"),
        "{msg}"
    );
    assert!(
        msg.contains("marketplace remove <name>"),
        "须给级联移除命令: {msg}"
    );
    assert!(
        msg.contains("marketplace add <git-url> --trust"),
        "须给重加命令: {msg}"
    );
    assert!(
        msg.contains("docs/computer/marketplace-0.5.1-migration.md"),
        "须指向迁移文档: {msg}"
    );
    assert!(!msg.contains(mp), "不得回显 marketplace 名: {msg}");
    assert!(!msg.contains(&url), "不得回显源 URL: {msg}");

    // ⑤ 按文档：marketplace remove 默认级联卸载（无 --keep-plugins）。
    let out = run(home, &skill_home, &["marketplace", "remove", mp, "--json"]);
    assert_ok(&out, "marketplace remove");
    let v = json_stdout(&out);
    assert_eq!(v["removed"], mp, "{v}");
    assert_eq!(v["keptPlugins"], false, "默认必须级联而非留孤儿: {v}");
    assert!(
        v["uninstalledPlugins"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p == pid.as_str()),
        "级联卸载应包含 {pid}: {v}"
    );
    assert!(
        !skill_home.join("marketplace").join(mp).exists(),
        "remove 后 catalog clone 应被 prune"
    );
    // 撤销 trust（迁移文档明示的副作用）：settings.json 不再含该名。
    let settings = home.join("xdg-config/a2c/settings.json");
    if settings.exists() {
        let text = fs::read_to_string(&settings).unwrap();
        assert!(
            !text.contains(mp),
            "remove 后 trustedMarketplaces 不应保留: {text}"
        );
    }

    // ⑥ 无 --trust 重新 add 应被拒（证明确实撤销了 trust）。
    let out = run(
        home,
        &skill_home,
        &["marketplace", "add", &url, "--name", mp, "--json"],
    );
    assert_eq!(
        out.status.code(),
        Some(1),
        "trust 撤销后非交互 add 应被拒; stdout={}",
        String::from_utf8_lossy(&out.stdout)
    );

    // ⑦ 按文档 --trust 重新 add（重 clone）→ ⑧ 重装成功。
    let out = run(
        home,
        &skill_home,
        &[
            "marketplace",
            "add",
            &url,
            "--name",
            mp,
            "--trust",
            "--json",
        ],
    );
    assert_ok(&out, "重新 add --trust");
    let out = run(home, &skill_home, &["plugin", "install", &pid, "--json"]);
    assert_ok(&out, "恢复后重装");
    assert_eq!(json_stdout(&out)["installed"], pid.as_str());
    assert!(
        catalog_skill.exists(),
        "恢复后 catalog 内相对源 SKILL.md 应回来"
    );
}
