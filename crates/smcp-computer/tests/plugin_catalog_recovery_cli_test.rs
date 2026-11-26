//! #278：旧版本卸载删 catalog 子树后的升级迁移——真实二进制端到端回归。
//!
//! 覆盖：① 0.5.0 破坏态（旧卸载已删账本/安装意图 + catalog 子树）下，`plugin info` 对受损插件
//! 报 not installed；install 失败错误携带恢复指引且不回显 marketplace 名/URL（占位符，零凭据泄漏）。
//! ② 按迁移文档执行 `marketplace remove`（默认级联 + 撤销 trust）→ `marketplace add --trust
//! --name`（重 clone）→ 按默认值重装成功。文档：`docs/computer/marketplace-0.5.1-migration.md`。
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
use cli_binary::{build_relative_source_repo_with_plugins, json_stdout, run_binary};

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
    // 两个插件：alpha 为受损插件，beta 留在该 marketplace 下验证 remove 的级联卸载。
    // 名字取 token 形态：错误提示必须用占位符，绝不回显它（#279/#285 同源泄漏面）。
    let mp = "acme-ghp-fake-token";
    let url = build_relative_source_repo_with_plugins(
        &home.join("mp-src"),
        mp,
        &[("alpha", "recovery fixture"), ("beta", "healthy fixture")],
    );
    let pid = format!("alpha@{mp}");
    let beta_pid = format!("beta@{mp}");
    let catalog = skill_home.join("marketplace").join(mp);
    let alpha_skill = catalog.join("plugins/alpha/skills/code-review/SKILL.md");
    let beta_skill = catalog.join("plugins/beta/skills/code-review/SKILL.md");

    // ① add（非交互必须 --trust）→ ② 装两个插件
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
    for p in [&pid, &beta_pid] {
        let out = run_binary(home, &skill_home, &["plugin", "install", p, "--json"]);
        assert_ok(&out, "首次 install");
        assert_eq!(json_stdout(&out)["installed"], p.as_str());
    }
    assert!(alpha_skill.exists() && beta_skill.exists());

    // ③ 精确复现 0.5.0 破坏态：旧卸载删账本/安装意图（0.5.1 的 uninstall 已不再删共享子树，
    //    故先 uninstall 再手删子树），受损插件此后不在 `plugin list` / `plugin info` 中。
    let out = run_binary(home, &skill_home, &["plugin", "uninstall", &pid, "--json"]);
    assert_ok(&out, "uninstall alpha（模拟旧版卸载）");
    assert!(
        alpha_skill.exists(),
        "#271 之后 uninstall 不得再删共享 catalog 子树（本测试再手删以复现旧态）"
    );
    fs::remove_dir_all(catalog.join("plugins/alpha")).unwrap();
    let out = run_binary(home, &skill_home, &["plugin", "info", &pid, "--json"]);
    assert_eq!(out.status.code(), Some(1), "受损插件应已不在安装意图");
    assert!(
        json_stdout(&out)["message"]
            .as_str()
            .unwrap_or_default()
            .contains("not installed"),
        "受损插件应从账本/意图消失（迁移文档 caveat 依据）"
    );

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
        msg.contains("may no longer list them"),
        "受损插件已不在账本，提示须说明查不到: {msg}"
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
        msg.contains("does not pin content"),
        "提示须说明 --version 仅恢复账本字段: {msg}"
    );
    assert!(
        msg.contains(
            "https://github.com/A2C-SMCP/rust-sdk/blob/v0.5.1/docs/computer/marketplace-0.5.1-migration.md"
        ),
        "须给出发布物用户可打开的文档 URL: {msg}"
    );
    assert!(!msg.contains(mp), "不得回显 marketplace 名: {msg}");
    assert!(!msg.contains(&url), "不得回显源 URL: {msg}");

    // ⑤ 按文档：marketplace remove 默认级联卸载仍安装的 beta（无 --keep-plugins）。
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
            .any(|p| p == beta_pid.as_str()),
        "级联卸载应包含仍安装的 {beta_pid}: {v}"
    );
    assert!(!catalog.exists(), "remove 后 catalog clone 应被 prune");
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

    // ⑥ 按文档 --trust 重新 add（重 clone）→ ⑦ 逐一重装（默认 user scope）成功。
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
    for p in [&pid, &beta_pid] {
        let out = run_binary(home, &skill_home, &["plugin", "install", p, "--json"]);
        assert_ok(&out, "恢复后重装");
        assert_eq!(json_stdout(&out)["installed"], p.as_str());
    }
    assert!(
        alpha_skill.exists() && beta_skill.exists(),
        "恢复后 catalog 内相对源 SKILL.md 应回来"
    );
    assert_ok(
        &run_binary(home, &skill_home, &["plugin", "info", &pid, "--json"]),
        "恢复后 plugin info 可查",
    );
}
