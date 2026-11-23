//! conformance-tests.md §5「enable 失败回滚（scope 精确 + 写后失败）」①②③④/④b 的 rust 侧映射。
//!
//! 协议依据：runtime-contract §2.4（[Discussion #82](https://github.com/A2C-SMCP/a2c-smcp-protocol/discussions/82)
//! 裁决 C / protocol#83，PR#84 已合 develop `a5afb26`）：
//! - 预检失败零写：被写 scope 的 raw pre-state 保持（absent 不写 false / false 保持 / true 保持）；
//! - 有效 prior-true 失败保持 true + 既有 skills/server/能力不动 + 结构化诊断（`EnableFailedRetained`）；
//! - 写后失败：raw 精确恢复 pre-state、registry 回填 staging 前快照、只清理本次新挂 server；
//! - 场景③/④b：user=true / project absent 的跨 scope 失败 → project key 删除、合并视图 true、
//!   bundled server verdict ENABLED（`list_pending_bundled_approvals` 不含）。
//!
//! **构造方式**：公开 `Computer::add_marketplace` + `install_plugin` + `enable_plugin(scope=...)`，
//! 注入 `McpInstallHooks` 制造 register 失败；断言公开面（settings JSON / `get_skills` /
//! `list_pending_bundled_approvals`）与 hook 调用记录。
//!
//! **fake hooks 的边界**：`register_server` 不落真实 manager，故不能用 `list_mcp_servers()` 证明运行时挂载；
//! server 投影面由 hook 的 registered/removed 记录覆盖（生产 hook = `Computer::mount_server`/`unmount_server`）。
//!
//! 变异验证（已实测，逐条反向改生产后转红再还原）：
//! - 恢复预检强制 false（回滚写 false）→ 场景②/③ 转红；
//! - 把 attempt 清理扩回「本 plugin 全部声明依赖」→ installer 单测
//!   `enable_register_failure_cleans_only_attempt_servers_82` 的 alpha 保留断言转红；
//! - 去掉 register 失败路径的 `*registry = snapshot` → 场景④ 的 `get_skills` 回滚断言转红。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{json, Value};
use tempfile::TempDir;

use smcp_computer::computer::{Computer, SilentSession};
use smcp_computer::mcp_clients::model::{BundleId, ServerName};
use smcp_computer::mcp_clients::MCPServerConfig;
use smcp_computer::settings::scope::{user_settings_path, workdir_project_settings_path, EnvMap};
use smcp_computer::settings::{
    AddMarketplaceParams, EnableOptions, InstallOptions, McpHookError, McpInstallHooks,
    PluginInstallError,
};

fn write(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

fn git(args: &[&str], dir: &Path) {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", dir)
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

/// 本地 git marketplace：`audit-mp` 下 `audit` plugin（1 skill + 1 bundled server）。
fn build_catalog(repo: &Path) -> String {
    write(
        &repo.join(".tfrobot-plugin/marketplace.json"),
        &serde_json::to_string(&json!({
            "name": "audit-mp",
            "plugins": [{"name": "audit", "source": "./plugins/audit"}]
        }))
        .unwrap(),
    );
    write(
        &repo.join("plugins/audit/skills/code-review/SKILL.md"),
        "---\nname: code-review\ndescription: review code\n---\nbody",
    );
    write(
        &repo.join("plugins/audit/mcp-servers/audit-mcp.json"),
        r#"{"type":"stdio","name":"audit-mcp","server_parameters":{"command":"node"}}"#,
    );
    git(&["init", "-q"], repo);
    git(&["add", "-A"], repo);
    git(&["commit", "-qm", "init"], repo);
    format!("file://{}", repo.display())
}

fn xdg_env(td: &TempDir) -> EnvMap {
    std::iter::once((
        "XDG_CONFIG_HOME".to_string(),
        td.path().join("xdg").to_string_lossy().into_owned(),
    ))
    .collect()
}

fn base_computer(td: &TempDir, env: &EnvMap) -> Computer<SilentSession> {
    base_computer_with_config(td, env, &td.path().join("config"))
}

/// 指定实例 `config_dir`（project/local 解析锚）。scene ③/④b 的 project settings 写在 `proj`，
/// 必须让实例锚点 = `proj`，否则 `list_pending_bundled_approvals` 看不到 project 层（恒真空断言）。
fn base_computer_with_config(
    td: &TempDir,
    env: &EnvMap,
    config_dir: &Path,
) -> Computer<SilentSession> {
    Computer::new("c", SilentSession::new("s"), None, None, false, false)
        .with_skill_home(td.path().join("home"))
        .with_blob_cache_root(td.path().join("blob"))
        .with_config_dir(config_dir.to_path_buf())
        .with_config_env(env.clone())
}

/// add marketplace + install（user scope），返回安装记录。
async fn add_and_install(
    comp: &Computer<SilentSession>,
    env: &EnvMap,
    td: &TempDir,
) -> smcp_computer::settings::InstalledPluginRecord {
    let url = build_catalog(&td.path().join("repo"));
    comp.add_marketplace(
        &url,
        AddMarketplaceParams {
            name: Some("audit-mp"),
            ..Default::default()
        },
    )
    .await
    .expect("add_marketplace");
    comp.install_plugin(
        "audit@audit-mp",
        InstallOptions {
            scope: Some("user"),
            env: Some(env),
            ..Default::default()
        },
        None,
    )
    .await
    .expect("install_plugin")
}

/// 读指定 settings 文件的 raw `enabledPlugins[pid]`（None = key 不存在；保留 false/true 原值）。
fn raw_enabled(path: &Path, pid: &str) -> Option<Value> {
    let txt = std::fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(&txt).ok()?;
    v.get("enabledPlugins")?.get(pid).cloned()
}

fn user_enabled(env: &EnvMap, pid: &str) -> Option<Value> {
    raw_enabled(&user_settings_path(Some(env)), pid)
}

fn project_enabled(proj: &Path, pid: &str) -> Option<Value> {
    raw_enabled(&workdir_project_settings_path(proj), pid)
}

fn skill_names(skills: &[smcp::A2CSkillRef]) -> Vec<String> {
    skills.iter().map(|s| s.name.clone()).collect()
}

/// 记录/注入失败的 hook（register 失败、remove 记录）。
struct RecordingHooks {
    existing: Mutex<HashSet<String>>,
    registered: Mutex<Vec<String>>,
    removed: Mutex<Vec<String>>,
    fail_register: Option<String>,
}

impl RecordingHooks {
    fn new() -> Self {
        Self {
            existing: Mutex::new(HashSet::new()),
            registered: Mutex::new(Vec::new()),
            removed: Mutex::new(Vec::new()),
            fail_register: None,
        }
    }

    fn failing_register(mut self, name: &str) -> Self {
        self.fail_register = Some(name.to_string());
        self
    }

    fn removed(&self) -> Vec<String> {
        self.removed.lock().unwrap().clone()
    }

    fn registered(&self) -> Vec<String> {
        self.registered.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl McpInstallHooks for RecordingHooks {
    fn existing_servers(&self) -> HashMap<BundleId, ServerName> {
        self.existing
            .lock()
            .unwrap()
            .iter()
            .filter_map(|n| BundleId::try_from(n.clone()).ok().map(|b| (b, n.clone())))
            .collect()
    }

    fn mounted_servers(&self) -> HashSet<BundleId> {
        self.existing
            .lock()
            .unwrap()
            .iter()
            .filter_map(|n| BundleId::try_from(n.clone()).ok())
            .collect()
    }

    async fn register_server(&self, cfg: MCPServerConfig) -> Result<(), McpHookError> {
        if self.fail_register.as_deref() == Some(cfg.name()) {
            return Err(McpHookError(format!("boom on {}", cfg.name())));
        }
        self.registered.lock().unwrap().push(cfg.name().to_string());
        Ok(())
    }

    async fn remove_server(&self, id: &BundleId) -> Result<(), McpHookError> {
        self.removed.lock().unwrap().push(id.as_str().to_string());
        Ok(())
    }
}

fn installed_root(record: &smcp_computer::settings::InstalledPluginRecord) -> PathBuf {
    PathBuf::from(record.install_path.clone().expect("installPath"))
}

/// 预检失败夹具（保持 bundled server 文件完好，供 pending verdict 断言）：
/// 写 `plugin.json` 声明 skills + 删除 skills 目录 → `no_projectable_content`。
fn break_skills_only(root: &Path) {
    write(
        &root.join(".tfrobot-plugin/plugin.json"),
        r#"{"skills":["./skills"]}"#,
    );
    std::fs::remove_dir_all(root.join("skills")).unwrap();
}

/// 写后失败夹具（生产可达）：保留 SKILL.md 文件（预检 has_projectable_skill 仍 true），但去掉
/// frontmatter description → staging 零注册 → `skills_not_staged`。
fn break_skill_staging(root: &Path) {
    write(
        &root.join("skills/code-review/SKILL.md"),
        "---\nname: code-review\n---\nbody",
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// ① 预检失败：prior-absent / prior-false
// ═══════════════════════════════════════════════════════════════════════════════

/// §5 ①a：pre-state `absent` → 失败后 key **不存在**（不得写 false），投影不出现、无半态。
#[tokio::test]
async fn conformance_preflight_prior_absent_leaves_key_absent_82() {
    let td = TempDir::new().unwrap();
    let env = xdg_env(&td);
    let comp = base_computer(&td, &env);
    let record = add_and_install(&comp, &env, &td).await;
    assert!(user_enabled(&env, "audit@audit-mp").is_none());
    std::fs::remove_dir_all(installed_root(&record)).unwrap();

    let hooks = RecordingHooks::new();
    let err = comp
        .enable_plugin("audit@audit-mp", EnableOptions::default(), Some(&hooks))
        .await
        .unwrap_err();
    assert!(matches!(err, PluginInstallError::ContentUnavailable { .. }));
    assert!(
        !matches!(err, PluginInstallError::EnableFailedRetained { .. }),
        "prior-absent 失败不得包装 retained"
    );
    assert!(
        user_enabled(&env, "audit@audit-mp").is_none(),
        "pre-state absent → key 必须保持不存在（不得写 false）"
    );
    assert!(comp.get_skills().await.is_empty());
    assert!(hooks.registered().is_empty());
    assert!(hooks.removed().is_empty(), "预检失败零写零回收");
}

/// §5 ①b：pre-state `false` → 精确恢复 `false`。
#[tokio::test]
async fn conformance_preflight_prior_false_keeps_false_82() {
    let td = TempDir::new().unwrap();
    let env = xdg_env(&td);
    let comp = base_computer(&td, &env);
    let record = add_and_install(&comp, &env, &td).await;
    write(
        &user_settings_path(Some(&env)),
        r#"{"enabledPlugins":{"audit@audit-mp":false}}"#,
    );
    std::fs::remove_dir_all(installed_root(&record)).unwrap();

    let hooks = RecordingHooks::new();
    let err = comp
        .enable_plugin("audit@audit-mp", EnableOptions::default(), Some(&hooks))
        .await
        .unwrap_err();
    assert!(matches!(err, PluginInstallError::ContentUnavailable { .. }));
    assert!(!matches!(
        err,
        PluginInstallError::EnableFailedRetained { .. }
    ));
    assert_eq!(
        user_enabled(&env, "audit@audit-mp"),
        Some(json!(false)),
        "pre-state false → 精确保持 false"
    );
    assert!(hooks.removed().is_empty());
}

// ═══════════════════════════════════════════════════════════════════════════════
// ② 预检失败：同 scope prior-true
// ═══════════════════════════════════════════════════════════════════════════════

/// §5 ②：同 scope 原为 `true` 的重复 enable 预检失败 → key 保持 true；`get_skills()` 与操作前一致；
/// 结构化诊断（`EnableFailedRetained`）可见、原错误（ContentUnavailable）保留。
#[tokio::test]
async fn conformance_preflight_prior_true_keeps_state_82() {
    let td = TempDir::new().unwrap();
    let env = xdg_env(&td);
    let comp = base_computer(&td, &env);
    let record = add_and_install(&comp, &env, &td).await;

    let first = RecordingHooks::new();
    comp.enable_plugin("audit@audit-mp", EnableOptions::default(), Some(&first))
        .await
        .expect("first enable");
    let before = skill_names(&comp.get_skills().await);
    assert!(!before.is_empty());
    assert_eq!(user_enabled(&env, "audit@audit-mp"), Some(json!(true)));

    break_skills_only(&installed_root(&record));
    let hooks = RecordingHooks::new();
    let err = comp
        .enable_plugin("audit@audit-mp", EnableOptions::default(), Some(&hooks))
        .await
        .unwrap_err();
    assert!(
        matches!(err, PluginInstallError::EnableFailedRetained { .. }),
        "prior-true 失败须包装 retained: {err}"
    );
    assert!(matches!(
        err.retained_source_deep(),
        PluginInstallError::ContentUnavailable { .. }
    ));
    assert!(err.to_string().contains("existing enabled state retained"));
    assert_eq!(
        user_enabled(&env, "audit@audit-mp"),
        Some(json!(true)),
        "同 scope prior-true 预检失败 MUST 保持 true"
    );
    assert_eq!(
        skill_names(&comp.get_skills().await),
        before,
        "投影与操作前一致"
    );
    assert!(hooks.removed().is_empty(), "既有 server 不得被摘");
}

// ═══════════════════════════════════════════════════════════════════════════════
// ③ 预检失败：跨 scope（user=true / project absent）
// ═══════════════════════════════════════════════════════════════════════════════

/// §5 ③：user=true、project absent，在 project enable 预检失败 → project key 仍 absent、合并视图 true、
/// bundled server verdict ENABLED（`list_pending_bundled_approvals` 不含，非 PENDING）。
#[tokio::test]
async fn conformance_preflight_cross_scope_user_true_project_absent_82() {
    let td = TempDir::new().unwrap();
    let env = xdg_env(&td);
    let proj = td.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    // 实例锚点 = proj：`list_pending_bundled_approvals` 才会解析 project 层。
    let comp = base_computer_with_config(&td, &env, &proj);
    let record = add_and_install(&comp, &env, &td).await;

    let first = RecordingHooks::new();
    comp.enable_plugin("audit@audit-mp", EnableOptions::default(), Some(&first))
        .await
        .expect("user enable");
    let before = skill_names(&comp.get_skills().await);
    assert_eq!(user_enabled(&env, "audit@audit-mp"), Some(json!(true)));
    assert!(project_enabled(&proj, "audit@audit-mp").is_none());

    break_skills_only(&installed_root(&record));
    let hooks = RecordingHooks::new();
    let err = comp
        .enable_plugin(
            "audit@audit-mp",
            EnableOptions {
                scope: Some("project"),
                project_path: proj.to_str(),
                ..Default::default()
            },
            Some(&hooks),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        PluginInstallError::EnableFailedRetained { .. }
    ));
    assert!(
        project_enabled(&proj, "audit@audit-mp").is_none(),
        "project 预检失败 MUST 保持 absent（不得写 false/true）"
    );
    assert_eq!(
        user_enabled(&env, "audit@audit-mp"),
        Some(json!(true)),
        "user=true 不得被更高 scope 的失败触碰"
    );
    assert_eq!(skill_names(&comp.get_skills().await), before);
    assert!(
        comp.list_pending_bundled_approvals().is_empty(),
        "origin 仍为 user → bundled server verdict ENABLED（不得落 PENDING）"
    );
}

/// 诊断锚点（嵌入场景）：实例 `config_dir=A`、操作在 `project_path=B`，A 下 project=false、
/// B 下 absent、user=true。失败诊断 MUST 以 **B** 解析合并视图（保留 retained），不得固定读 A
/// （否则误判 disabled、丢失 retained）。
#[tokio::test]
async fn conformance_preflight_diagnostic_anchor_uses_operation_path_82() {
    let td = TempDir::new().unwrap();
    let env = xdg_env(&td);
    let config_a = td.path().join("config-a");
    let proj_b = td.path().join("proj-b");
    std::fs::create_dir_all(&config_a).unwrap();
    std::fs::create_dir_all(&proj_b).unwrap();
    let comp = base_computer_with_config(&td, &env, &config_a);
    let record = add_and_install(&comp, &env, &td).await;
    // A 下 project=false（若诊断错误地锚 A，会得到 disabled）。add 之后写：add 的引用守卫会把
    // 任何 scope 的 enabledPlugins 键（含 false）视为意图引用并拒绝。
    write(
        &workdir_project_settings_path(&config_a),
        r#"{"enabledPlugins":{"audit@audit-mp":false}}"#,
    );

    comp.enable_plugin("audit@audit-mp", EnableOptions::default(), None)
        .await
        .expect("user enable");
    let before = skill_names(&comp.get_skills().await);
    break_skills_only(&installed_root(&record));

    let err = comp
        .enable_plugin(
            "audit@audit-mp",
            EnableOptions {
                scope: Some("project"),
                project_path: proj_b.to_str(),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, PluginInstallError::EnableFailedRetained { .. }),
        "诊断必须锚定操作 path（B：user=true），不得固定读 config_dir=A（project=false）：{err}"
    );
    assert_eq!(
        project_enabled(&config_a, "audit@audit-mp"),
        Some(json!(false)),
        "A 的 project=false 不得被触碰"
    );
    assert!(
        project_enabled(&proj_b, "audit@audit-mp").is_none(),
        "B 原 absent，失败后仍 absent"
    );
    assert_eq!(skill_names(&comp.get_skills().await), before);
}

// ═══════════════════════════════════════════════════════════════════════════════
// ④ 写后失败：prior-absent
// ═══════════════════════════════════════════════════════════════════════════════

/// §5 ④：写入 `enabledPlugins=true` / stage skill 后 register 失败 → key 精确恢复 absent（删键）、
/// 本次 stage 的 skill 撤销（registry 快照）、本次新挂（失败者）清理；原错误不包装（有效 prior-false）。
#[tokio::test]
async fn conformance_write_after_prior_absent_restores_snapshot_82() {
    let td = TempDir::new().unwrap();
    let env = xdg_env(&td);
    let comp = base_computer(&td, &env);
    add_and_install(&comp, &env, &td).await;
    assert!(user_enabled(&env, "audit@audit-mp").is_none());
    assert!(comp.get_skills().await.is_empty());

    let hooks = RecordingHooks::new().failing_register("audit-mcp");
    let err = comp
        .enable_plugin("audit@audit-mp", EnableOptions::default(), Some(&hooks))
        .await
        .unwrap_err();
    assert!(
        matches!(err, PluginInstallError::Hook(_)),
        "原错误不吞: {err}"
    );
    assert!(!matches!(
        err,
        PluginInstallError::EnableFailedRetained { .. }
    ));
    assert!(
        user_enabled(&env, "audit@audit-mp").is_none(),
        "写后失败 prior-absent → key 删除（不得留 true/false）"
    );
    assert!(
        user_settings_path(Some(&env)).is_file(),
        "写后失败用例必须真正经过步骤②（settings 文件已创建；纯预检失败不会创建）"
    );
    assert!(
        comp.get_skills().await.is_empty(),
        "本次 stage 的 skill 必须回滚（registry 快照）"
    );
    assert_eq!(
        hooks.removed(),
        vec!["audit-mcp".to_string()],
        "失败者纳入本次清理集合"
    );
}

/// §5 ④：pre-state `false` → 写后失败精确恢复 `false`（不得删键/改 true），投影不出现。
#[tokio::test]
async fn conformance_write_after_prior_false_keeps_false_82() {
    let td = TempDir::new().unwrap();
    let env = xdg_env(&td);
    let comp = base_computer(&td, &env);
    add_and_install(&comp, &env, &td).await;
    write(
        &user_settings_path(Some(&env)),
        r#"{"enabledPlugins":{"audit@audit-mp":false}}"#,
    );

    let hooks = RecordingHooks::new().failing_register("audit-mcp");
    let err = comp
        .enable_plugin("audit@audit-mp", EnableOptions::default(), Some(&hooks))
        .await
        .unwrap_err();
    assert!(matches!(err, PluginInstallError::Hook(_)));
    assert_eq!(
        user_enabled(&env, "audit@audit-mp"),
        Some(json!(false)),
        "写后失败 prior-false → 精确恢复 false"
    );
    assert!(comp.get_skills().await.is_empty());
    assert_eq!(hooks.removed(), vec!["audit-mcp".to_string()]);
}

/// §5 ④：pre-state `true` 的同 scope 重复 enable 写后失败 → 保持 `true`、投影不变、
/// 结构化诊断包装 retained（生产可达的 staging 零注册注入）。
#[tokio::test]
async fn conformance_write_after_prior_true_same_scope_keeps_state_82() {
    let td = TempDir::new().unwrap();
    let env = xdg_env(&td);
    let comp = base_computer(&td, &env);
    let record = add_and_install(&comp, &env, &td).await;
    comp.enable_plugin("audit@audit-mp", EnableOptions::default(), None)
        .await
        .expect("user enable");
    let before = skill_names(&comp.get_skills().await);
    assert!(!before.is_empty());

    break_skill_staging(&installed_root(&record));
    let err = comp
        .enable_plugin("audit@audit-mp", EnableOptions::default(), None)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        PluginInstallError::EnableFailedRetained { .. }
    ));
    assert!(matches!(
        err.retained_source_deep(),
        PluginInstallError::ContentUnavailable { .. }
    ));
    assert!(
        err.to_string().contains("skills_not_staged"),
        "用例必须命中写后 staging 失败（而非预检）：{err}"
    );
    assert_eq!(
        user_enabled(&env, "audit@audit-mp"),
        Some(json!(true)),
        "prior-true 写后失败 MUST 保持 true"
    );
    assert_eq!(skill_names(&comp.get_skills().await), before);
}

// ═══════════════════════════════════════════════════════════════════════════════
// ④b 写后失败：跨 scope（user=true / project absent）
// ═══════════════════════════════════════════════════════════════════════════════

/// §5 ④b：user=true、project absent，project enable 写入 true 后 apply 失败 → project key 删除、
/// 合并视图 true、`get_skills()` 不变、bundled server verdict 回 ENABLED（非 PENDING）。
#[tokio::test]
async fn conformance_write_after_cross_scope_verdict_enabled_82() {
    let td = TempDir::new().unwrap();
    let env = xdg_env(&td);
    let proj = td.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    let comp = base_computer_with_config(&td, &env, &proj);
    let record = add_and_install(&comp, &env, &td).await;

    let first = RecordingHooks::new();
    comp.enable_plugin("audit@audit-mp", EnableOptions::default(), Some(&first))
        .await
        .expect("user enable");
    let before = skill_names(&comp.get_skills().await);
    assert!(!before.is_empty());

    // 写后失败（staging 零注册，生产可达）：预检仍通过（SKILL.md 文件在），write 已 true 后失败。
    break_skill_staging(&installed_root(&record));
    let err = comp
        .enable_plugin(
            "audit@audit-mp",
            EnableOptions {
                scope: Some("project"),
                project_path: proj.to_str(),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        PluginInstallError::EnableFailedRetained { .. }
    ));
    assert!(matches!(
        err.retained_source_deep(),
        PluginInstallError::ContentUnavailable { .. }
    ));
    assert!(
        err.to_string().contains("skills_not_staged"),
        "用例必须命中写后 staging 失败（而非预检）：{err}"
    );
    assert!(
        workdir_project_settings_path(&proj).is_file(),
        "写后失败用例必须真正经过步骤②（project settings 文件已创建）"
    );
    assert!(
        project_enabled(&proj, "audit@audit-mp").is_none(),
        "写后失败 project key MUST 删除（恢复 absent）"
    );
    assert_eq!(user_enabled(&env, "audit@audit-mp"), Some(json!(true)));
    assert_eq!(
        skill_names(&comp.get_skills().await),
        before,
        "合并视图保持 true"
    );
    assert!(
        comp.list_pending_bundled_approvals().is_empty(),
        "origin 回 user → verdict ENABLED（非 PENDING）"
    );
}

/// 正对照：project-only enable（origin=Project）的 bundled server MUST 出现在待批准列表——
/// 证明 `list_pending_bundled_approvals()` 在本夹具下**可以非空**，scene ③/④b 的空断言非恒真。
#[tokio::test]
async fn conformance_pending_verdict_positive_control_82() {
    let td = TempDir::new().unwrap();
    let env = xdg_env(&td);
    let proj = td.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    let comp = base_computer_with_config(&td, &env, &proj);
    add_and_install(&comp, &env, &td).await;

    comp.enable_plugin(
        "audit@audit-mp",
        EnableOptions {
            scope: Some("project"),
            project_path: proj.to_str(),
            ..Default::default()
        },
        None,
    )
    .await
    .expect("project-only enable");
    assert_eq!(project_enabled(&proj, "audit@audit-mp"), Some(json!(true)));
    let pending = comp.list_pending_bundled_approvals();
    assert!(
        pending.iter().any(|r| r.config.name() == "audit-mcp"),
        "project-origin bundled server MUST 进入待批准列表（正对照）：{pending:?}"
    );
}
