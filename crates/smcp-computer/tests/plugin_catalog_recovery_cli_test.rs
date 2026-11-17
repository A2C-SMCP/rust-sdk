//! #278：0.5.0 旧卸载删 catalog 子树后的升级迁移——真实二进制端到端回归。
//!
//! 覆盖：① install 失败错误携带恢复指引且不回显 marketplace 名/URL（占位符，零凭据泄漏）；
//! ② 按迁移文档执行 `marketplace remove`（默认级联 + 撤销 trust）→ `marketplace add --trust`
//! （重 clone）→ 重装成功。文档：`docs/computer/marketplace-0.5.1-migration.md`。
//!
//! git / 仓库夹具 / 二进制运行 helper 与 `plugin_name_conflict_cli_test.rs` 共用
//! [`common/cli_binary.rs`](common/cli_binary.rs)。
#![cfg(feature = "cli")]

use std::fs;
use std::process::Output;

use serde_json::Value;
use tempfile::tempdir;

#[path = "common/cli_binary.rs"]
mod cli_binary;
use cli_binary::{build_relative_source_repo, json_stdout, run_binary};

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
    let url = build_relative_source_repo(&home.join("mp-src"), "recovery fixture");
    // 名字取 token 形态：错误提示必须用占位符，绝不回显它（#279/#285 同源泄漏面）。
    let mp = "acme-ghp-fake-token";
    let pid = format!("alpha@{mp}");
    let catalog_skill = skill_home
        .join("marketplace")
        .join(mp)
        .join("plugins/alpha/skills/code-review/SKILL.md");

    // ① add（非交互必须 --trust）→ ② install
    let out = run_binary(
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
    let out = run_binary(home, &skill_home, &["plugin", "install", &pid, "--json"]);
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
    let out = run_binary(home, &skill_home, &["plugin", "install", &pid, "--json"]);
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
        msg.contains(
            "https://github.com/A2C-SMCP/rust-sdk/blob/v0.5.1/docs/computer/marketplace-0.5.1-migration.md"
        ),
        "须给出发布物用户可打开的文档 URL: {msg}"
    );
    assert!(!msg.contains(mp), "不得回显 marketplace 名: {msg}");
    assert!(!msg.contains(&url), "不得回显源 URL: {msg}");

    // ⑤ 按文档：marketplace remove 默认级联卸载（无 --keep-plugins）。
    let out = run_binary(home, &skill_home, &["marketplace", "remove", mp, "--json"]);
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
    // 撤销 trust（迁移文档明示的副作用）：settings.json 的 trustedMarketplaces 不再含该名。
    // 注意：非交互 `marketplace add` 无 `--trust` 一律拒绝（陈旧 trust 不豁免），故「不带 --trust
    // 被拒」不能作为撤销凭据；这里直接解析 settings（add --trust 必然创建该文件）。
    let settings = home.join("xdg-config/a2c/settings.json");
    let text = fs::read_to_string(&settings).expect("add --trust 后 settings.json 应存在");
    let sv: Value = serde_json::from_str(&text).expect("settings.json 应为合法 JSON");
    let still_trusted = sv["trustedMarketplaces"]
        .as_array()
        .map(|arr| arr.iter().any(|n| n == mp))
        .unwrap_or(false);
    assert!(
        !still_trusted,
        "remove 后 trustedMarketplaces 不应保留 {mp}: {sv}"
    );

    // ⑥ 按文档 --trust 重新 add（重 clone）→ ⑦ 重装成功。
    let out = run_binary(
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
    let out = run_binary(home, &skill_home, &["plugin", "install", &pid, "--json"]);
    assert_ok(&out, "恢复后重装");
    assert_eq!(json_stdout(&out)["installed"], pid.as_str());
    assert!(
        catalog_skill.exists(),
        "恢复后 catalog 内相对源 SKILL.md 应回来"
    );
}
