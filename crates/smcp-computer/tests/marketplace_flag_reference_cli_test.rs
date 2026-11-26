//! #267：一次性 CLI `marketplace add --settings` 的引用守卫接线（真实二进制）。
//!
//! 覆盖：① flag 层含 `<plugin>@<name>` 引用 → `MarketplaceNameReferenced`（exit 1），零注册/零 clone；
//! ② 同一操作去掉 `--settings` → 成功（证明拒绝来自 flag 层）；③ `--settings` 为不可归一形态
//! （`~user/...`）→ fail-closed 为无 flag 层（stderr 诊断）+ add 成功。
#![cfg(feature = "cli")]

use std::fs;
use std::process::Output;

use tempfile::tempdir;

#[path = "common/cli_binary.rs"]
mod cli_binary;
use cli_binary::{build_relative_source_repo, run_binary};

fn code(out: &Output) -> i32 {
    out.status.code().unwrap_or(-1)
}

fn combined(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[test]
fn one_shot_marketplace_add_honors_flag_layer_reference_267() {
    let dir = tempdir().unwrap();
    let home = dir.path();
    let skill_home = home.join("skill-home");
    fs::create_dir_all(&skill_home).unwrap();

    let mp = format!("cli-flag-guard-267-{}", std::process::id());
    let url = build_relative_source_repo(&home.join("mp-src"), &mp, "cli flag guard");

    let flag = home.join("flag-settings.json");
    let mut enabled = serde_json::Map::new();
    enabled.insert(format!("alpha@{mp}"), serde_json::Value::Bool(true));
    let mut flag_settings = serde_json::Map::new();
    flag_settings.insert(
        "enabledPlugins".to_string(),
        serde_json::Value::Object(enabled),
    );
    fs::write(&flag, serde_json::Value::Object(flag_settings).to_string()).unwrap();

    // ① flag 引用 → 拒绝（exit 1），零注册 / 零 clone。
    let out = run_binary(
        home,
        &skill_home,
        &[
            "--settings",
            flag.to_str().unwrap(),
            "marketplace",
            "add",
            &url,
            "--trust",
        ],
    );
    let text = combined(&out);
    assert_eq!(code(&out), 1, "flag 层引用应拒绝: {text}");
    assert!(text.contains("still referenced"), "{text}");
    assert!(
        !skill_home.join("marketplace").join(&mp).exists(),
        "拒绝不得留下 clone: {text}"
    );

    // ② 同一操作无 --settings → 成功（证明拒绝来自 flag 层）。
    let out = run_binary(home, &skill_home, &["marketplace", "add", &url, "--trust"]);
    assert_eq!(code(&out), 0, "控制组应成功: {}", combined(&out));

    // ③ 不可归一的 --settings（~user 形态）→ fail-closed 为无 flag 层，add 成功 + stderr 诊断。
    let mp2 = format!("cli-flag-guard2-267-{}", std::process::id());
    let url2 = build_relative_source_repo(&home.join("mp-src-2"), &mp2, "cli flag guard 2");
    let out = run_binary(
        home,
        &skill_home,
        &[
            "--settings",
            "~alice/settings.json",
            "marketplace",
            "add",
            &url2,
            "--trust",
        ],
    );
    assert_eq!(
        code(&out),
        0,
        "无效 --settings 应 fail-closed 后继续: {}",
        combined(&out)
    );
    assert!(
        combined(&out).contains("unusable"),
        "须给出 flag 路径不可用诊断: {}",
        combined(&out)
    );
    assert!(skill_home.join("marketplace").join(&mp2).exists());
}
