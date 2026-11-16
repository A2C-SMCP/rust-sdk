/*!
* 文件名: skill.rs
* 作者: JQQ
* 创建日期: 2026/06/10
* 最后修改日期: 2026/06/10
* 版权: 2023 JQQ. All rights reserved.
* 依赖: serde_json
* 描述: `skill` 命令 handler（跨源扁平视图：list / info 两个只读动词）
*       Skill command handlers (read-only list/info).
*
* 对标 Python `a2c_smcp/computer/cli/commands/skill.py`：skill **没有** install/uninstall/enable/disable
* （经 plugin 层 / server add-rm / 文件操作完成）。CLI 只暴露 list / info（§4.4）。list 跨三源（marketplace/
* mcp/user）扁平展示 name / source / enabled / orphan，`--source mp|mcp|user` 过滤。REPL 直读 `Computer`
* 活跃 registry（#54）；Typer 非交互在新进程经 [`rebuild_registry`] 离线重建（marketplace clone + user DropIn；
* mcp 源需 live 服务器 → 非交互不列）。
*/

use std::path::Path;

use serde_json::{json, Map, Value};

use super::{err_flat as err, msg_dim, msg_warn, print_json, EXIT_OK, EXIT_USER_ERROR};
use crate::settings::scope::EnvMap;
use crate::settings::store::{installed_marketplace_order, load_known_marketplaces};
use crate::skills::{
    stage_marketplace_skills, stage_user_skills, MarketplaceStageOptions, SkillRegistry,
    SOURCE_MARKETPLACE, SOURCE_MCP, SOURCE_USER,
};

/// 取 `A2CSkillRef.source` 的来源段（`marketplace:<mp>` → `marketplace`）/ source-kind segment。
fn source_segment(src: &str) -> &str {
    src.split(':').next().unwrap_or(src)
}

/// CLI `--source` 短名 → `A2CSkillRef.source` 前缀段 / CLI short name → source prefix segment。
fn source_alias(short: &str) -> Option<&'static str> {
    match short {
        "mp" => Some(SOURCE_MARKETPLACE),
        "mcp" => Some(SOURCE_MCP),
        "user" => Some(SOURCE_USER),
        _ => None,
    }
}

/// 跨源列出当前可见 SKILL（name / source / enabled / orphan），`source` ∈ {mp,mcp,user} 过滤 / list skills。
pub fn skill_list(registry: &SkillRegistry, source: Option<&str>, json_output: bool) -> i32 {
    let want: Option<&str> = match source {
        None => None,
        Some(short) => match source_alias(short) {
            Some(seg) => Some(seg),
            None => {
                msg_warn(&format!(
                    "unknown --source {short:?} (expected mp|mcp|user)"
                ));
                return EXIT_USER_ERROR;
            }
        },
    };

    let mut rows: Vec<Value> = Vec::new();
    for (skill_ref, orphaned) in registry.all_refs() {
        if let Some(want) = want {
            if source_segment(&skill_ref.source) != want {
                continue;
            }
        }
        rows.push(json!({
            "name": skill_ref.name,
            "source": skill_ref.source,
            "enabled": !orphaned,
            "orphan": orphaned,
        }));
    }
    rows.sort_by(|a, b| {
        a["name"]
            .as_str()
            .unwrap_or("")
            .cmp(b["name"].as_str().unwrap_or(""))
    });

    if json_output {
        print_json(&json!(rows));
        return EXIT_OK;
    }
    if rows.is_empty() {
        msg_dim("No skills visible.");
        return EXIT_OK;
    }
    println!("Skills:");
    for r in &rows {
        println!(
            "  {}  ·  {}  ·  enabled={}  ·  orphan={}",
            r["name"].as_str().unwrap_or(""),
            r["source"].as_str().unwrap_or(""),
            if r["enabled"].as_bool().unwrap_or(false) {
                "✓"
            } else {
                "—"
            },
            if r["orphan"].as_bool().unwrap_or(false) {
                "⚠"
            } else {
                "—"
            },
        );
    }
    EXIT_OK
}

/// SKILL 详情：source / version / description / path / license / compatibility / enabled / orphan（含孤儿）。
pub fn skill_info(registry: &SkillRegistry, name: &str, json_output: bool) -> i32 {
    // all_refs 含孤儿（resolve 排除孤儿）——info 须能展示孤儿状态，故用 all_refs 查找。
    let Some((skill_ref, orphaned)) = registry
        .all_refs()
        .into_iter()
        .find(|(r, _)| r.name == name)
    else {
        return err(
            &format!("unknown skill: {name:?}"),
            json_output,
            EXIT_USER_ERROR,
        );
    };

    let mut obj = match serde_json::to_value(&skill_ref) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    };
    obj.insert("orphan".to_string(), json!(orphaned));
    obj.insert("enabled".to_string(), json!(!orphaned));

    if json_output {
        print_json(&Value::Object(obj));
        return EXIT_OK;
    }
    println!("Skill · {name}");
    for key in [
        "source",
        "version",
        "description",
        "path",
        "license",
        "compatibility",
        "enabled",
        "orphan",
    ] {
        if let Some(v) = obj.get(key) {
            println!("  {key}: {v}");
        }
    }
    EXIT_OK
}

/// 非交互重建 registry：从 `known_marketplaces.json` 离线重扫 marketplace clone + user DropIn / offline rebuild。
///
/// 新进程 registry 为空；据已物化 marketplace clone（`refresh=false` 复用本地树、不触网）+ user 源就地发现
/// 重新注册。**mcp 源不在此**（需运行中 MCP 服务器，仅 REPL 可见）。
pub async fn rebuild_registry(home: &Path, env: Option<&EnvMap>) -> SkillRegistry {
    let mut registry = SkillRegistry::new();
    let known = load_known_marketplaces(Some(home), env).account;
    // #275：已装 plugin 的 marketplace 优先 stage（installed intent 顺序），避免「仅添加」的源凭
    // known_marketplaces 载入序抢占同名 SKILL 所有权（reload 经 serde_json Map 不保证文件原序）。
    let priority = installed_marketplace_order(Some(home), env);
    let mut ordered = Vec::new();
    for mp in &priority {
        if let Some(rec) = known.marketplaces.get(mp) {
            ordered.push((mp, rec));
        }
    }
    for (name, rec) in &known.marketplaces {
        if !priority.contains(name) {
            ordered.push((name, rec));
        }
    }
    for (name, rec) in ordered {
        let auto_update = rec
            .extra
            .get("autoUpdate")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        stage_marketplace_skills(
            name,
            &rec.source,
            &mut registry,
            home,
            MarketplaceStageOptions {
                plugin_filter: None,
                auto_update,
                refresh: false,
                timeout: None,
                env,
                recorder: None,
            },
        )
        .await;
    }
    stage_user_skills(&mut registry, home);
    registry
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::store::{update_installed_plugins_intent, update_known_marketplaces};
    use smcp::A2CSkillRef;

    fn skill_ref(name: &str, source: &str) -> A2CSkillRef {
        A2CSkillRef {
            name: name.to_string(),
            source: source.to_string(),
            uri: None,
            path: format!("/tmp/skills/{name}"),
            description: "a test skill".to_string(),
            license: Some("MIT".to_string()),
            compatibility: None,
            allowed_tools: None,
            tags: None,
            version: Some("1.0.0".to_string()),
            skill_metadata: None,
        }
    }

    #[test]
    fn source_segment_extracts_kind() {
        assert_eq!(source_segment("marketplace:acme"), "marketplace");
        assert_eq!(source_segment("mcp:tfrobot"), "mcp");
        assert_eq!(source_segment("user"), "user");
    }

    #[test]
    fn list_empty_registry_is_ok() {
        let registry = SkillRegistry::new();
        assert_eq!(skill_list(&registry, None, true), EXIT_OK);
    }

    #[test]
    fn list_unknown_source_is_user_error() {
        let registry = SkillRegistry::new();
        assert_eq!(skill_list(&registry, Some("bogus"), true), EXIT_USER_ERROR);
    }

    #[test]
    fn list_filters_by_source_and_flags_orphan() {
        let mut registry = SkillRegistry::new();
        registry.register(skill_ref("alpha", "marketplace:acme"));
        registry.register(skill_ref("beta", "user"));
        registry.register(skill_ref("gamma", "mcp:tfrobot"));
        // 标记 beta 为孤儿 → enabled=false。
        registry.mark_orphan("beta");

        // 全量列出 3 条 → 0。
        assert_eq!(skill_list(&registry, None, true), EXIT_OK);
        // --source mp 过滤命中 marketplace 段 → 0。
        assert_eq!(skill_list(&registry, Some("mp"), true), EXIT_OK);
        assert_eq!(skill_list(&registry, Some("user"), true), EXIT_OK);

        // all_refs 含孤儿，且孤儿 enabled=false。
        let refs = registry.all_refs();
        let beta = refs.iter().find(|(r, _)| r.name == "beta").unwrap();
        assert!(beta.1, "beta should be orphaned");
    }

    #[test]
    fn info_unknown_skill_is_user_error() {
        let registry = SkillRegistry::new();
        assert_eq!(skill_info(&registry, "ghost", true), EXIT_USER_ERROR);
    }

    #[test]
    fn info_existing_skill_ok_including_orphan() {
        let mut registry = SkillRegistry::new();
        registry.register(skill_ref("alpha", "marketplace:acme"));
        assert_eq!(skill_info(&registry, "alpha", true), EXIT_OK);
        // 孤儿仍可 info（all_refs 查找）。
        registry.mark_orphan("alpha");
        assert_eq!(skill_info(&registry, "alpha", true), EXIT_OK);
    }

    #[tokio::test]
    async fn rebuild_registry_empty_home_is_offline_empty() {
        // 空 home：无 known_marketplaces（不触网）+ 无 user DropIn → 空 registry，不 panic。
        let dir = tempfile::tempdir().unwrap();
        let registry = rebuild_registry(dir.path(), None).await;
        assert!(registry.is_empty());
    }

    #[tokio::test]
    async fn rebuild_registry_picks_up_user_dropin() {
        // user DropIn（home/user/<skill>/SKILL.md）离线重建被注册；mcp 源不在离线重建内。
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let skill_dir = home.join("user").join("mytool");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: mytool\ndescription: a tool\n---\nbody",
        )
        .unwrap();
        let registry = rebuild_registry(home, None).await;
        assert!(registry
            .all_refs()
            .iter()
            .any(|(r, _)| source_segment(&r.source) == "user"));
    }

    fn git(args: &[&str], cwd: &std::path::Path) {
        let out = std::process::Command::new("git")
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

    fn build_marketplace_repo(repo: &std::path::Path, description: &str) -> String {
        std::fs::create_dir_all(repo.join(".tfrobot-plugin")).unwrap();
        std::fs::write(
            repo.join(".tfrobot-plugin/marketplace.json"),
            r#"{"plugins": [{"name": "alpha", "source": "./plugins/alpha"}]}"#,
        )
        .unwrap();
        let skill = repo.join("plugins/alpha/skills/code-review");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            format!("---\nname: code-review\ndescription: {description}\n---\nbody"),
        )
        .unwrap();
        git(&["init", "-q"], repo);
        git(&["add", "-A"], repo);
        git(&["commit", "-qm", "init"], repo);
        format!("file://{}", repo.display())
    }

    /// #275：CLI 离线重建须让已装 plugin 的 marketplace 先 stage，避免仅 add 的源凭载入序占名。
    #[tokio::test]
    async fn rebuild_registry_prioritizes_installed_marketplace_over_add_only() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let url_a = build_marketplace_repo(&dir.path().join("repo-a"), "from mp-a");
        let url_b = build_marketplace_repo(&dir.path().join("repo-b"), "from mp-b");
        let mut throwaway = SkillRegistry::new();
        for (name, url) in [("mp-a", &url_a), ("mp-b", &url_b)] {
            stage_marketplace_skills(
                name,
                &json!({"type": "git", "url": url}),
                &mut throwaway,
                home,
                MarketplaceStageOptions {
                    refresh: true,
                    ..Default::default()
                },
            )
            .await;
        }
        update_known_marketplaces(
            |f| {
                for (name, url) in [("mp-a", &url_a), ("mp-b", &url_b)] {
                    f.account.marketplaces.insert(
                        name.to_string(),
                        crate::settings::reconciler::KnownMarketplaceEntry {
                            source: json!({"type": "git", "url": url}),
                            extra: Map::new(),
                        },
                    );
                }
            },
            Some(home),
            None,
        )
        .unwrap();
        update_installed_plugins_intent(
            |f| {
                f.account.installed_plugins.insert("alpha@mp-b".to_string());
            },
            Some(home),
            None,
        )
        .unwrap();

        let registry = rebuild_registry(home, None).await;
        let (kept, _) = registry
            .all_refs()
            .into_iter()
            .find(|(r, _)| r.name == "alpha:code-review")
            .unwrap();
        assert_eq!(
            kept.source, "marketplace:mp-b",
            "已装来源必须先于仅 add 的源注册"
        );
    }
}
