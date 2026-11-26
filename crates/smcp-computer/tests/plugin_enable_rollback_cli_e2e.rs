//! #286 follow-up：真实 `Computer` + 生产 `CliMcpHooks` 的写后失败 e2e（架构师复审 🟡3）。
//!
//! 目的：durable 断言不能只在假 hook 下恒真。本测试用生产 `CliMcpHooks`（`remove_server` =
//! 运行期 `unmount_server`）真实挂载 `alpha-mcp`，再让 `zeta-mcp` 注册失败；断言：
//! - `alpha-mcp` 被**真实 unmount**（运行期投影消失）；
//! - 用户 `mcp.json` 逐字节不变；
//! - 另一 plugin 的账本依赖记录逐字节不变。
//!
//! `#![cfg(feature = "cli")]`：`CliMcpHooks` 属 `cli` feature。
#![cfg(feature = "cli")]

use std::collections::{HashMap, HashSet};
use std::path::Path;

use async_trait::async_trait;
use serde_json::Map;
use tempfile::TempDir;

use smcp_computer::cli::commands::CliMcpHooks;
use smcp_computer::computer::{Computer, SilentSession};
use smcp_computer::mcp_clients::model::{BundleId, ServerName};
use smcp_computer::mcp_clients::MCPServerConfig;
use smcp_computer::settings::scope::EnvMap;
use smcp_computer::settings::store::{load_installed_plugins, update_installed_plugins};
use smcp_computer::settings::{
    AddMarketplaceParams, EnableOptions, InstallOptions, InstalledPluginRecord, McpHookError,
    McpInstallHooks, PluginInstallError,
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

/// marketplace `audit-mp` / plugin `audit`：skill + 两个 server（alpha 先挂、zeta 后失败）。
fn build_catalog(repo: &Path) -> String {
    write(
        &repo.join(".tfrobot-plugin/marketplace.json"),
        r#"{"name":"audit-mp","plugins":[{"name":"audit","source":"./plugins/audit"}]}"#,
    );
    write(
        &repo.join("plugins/audit/skills/code-review/SKILL.md"),
        "---\nname: code-review\ndescription: review code\n---\nbody",
    );
    for name in ["alpha-mcp", "zeta-mcp"] {
        write(
            &repo.join(format!("plugins/audit/mcp-servers/{name}.json")),
            &format!(
                r#"{{"type":"stdio","name":"{name}","server_parameters":{{"command":"node"}}}}"#
            ),
        );
    }
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

/// 生产 hook 的薄包装：alpha 走真实 `CliMcpHooks`（真实挂载），zeta 注册失败。
struct FailZeta<'a, 'b> {
    inner: &'b CliMcpHooks<'a, SilentSession>,
}

#[async_trait]
impl McpInstallHooks for FailZeta<'_, '_> {
    fn existing_servers(&self) -> HashMap<BundleId, ServerName> {
        self.inner.existing_servers()
    }

    fn mounted_servers(&self) -> HashSet<BundleId> {
        self.inner.mounted_servers()
    }

    async fn register_server(&self, cfg: MCPServerConfig) -> Result<(), McpHookError> {
        self.inner.register_server(cfg).await
    }

    async fn register_server_with_input_scope(
        &self,
        cfg: MCPServerConfig,
        plugin_id: Option<&str>,
    ) -> Result<(), McpHookError> {
        if cfg.name() == "zeta-mcp" {
            return Err(McpHookError("boom on zeta-mcp".to_string()));
        }
        self.inner
            .register_server_with_input_scope(cfg, plugin_id)
            .await
    }

    async fn remove_server(&self, id: &BundleId) -> Result<(), McpHookError> {
        self.inner.remove_server(id).await
    }

    async fn inject_inputs(&self, plugin_root: &Path) -> Result<(), McpHookError> {
        self.inner.inject_inputs(plugin_root).await
    }
}

#[tokio::test]
async fn write_after_failure_with_real_cli_hooks_keeps_durable_82() {
    let td = TempDir::new().unwrap();
    let env = xdg_env(&td);
    let home = td.path().join("home");
    let comp = Computer::new("c", SilentSession::new("s"), None, None, false, false)
        .with_skill_home(home.clone())
        .with_blob_cache_root(td.path().join("blob"))
        .with_config_dir(td.path().join("config"))
        .with_config_env(env.clone());

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
            env: Some(&env),
            ..Default::default()
        },
        None,
    )
    .await
    .expect("install_plugin");

    // 用户 durable 声明 alpha-mcp（真实 user mcp.json 路径）。
    let mcp_path = smcp_computer::settings::user_mcp_config_path(Some(&env));
    let mcp_bytes =
        br#"{"servers":{"alpha-mcp":{"type":"stdio","server_parameters":{"command":"user-cmd"}}}}"#;
    write(&mcp_path, std::str::from_utf8(mcp_bytes).unwrap());
    // 另一 plugin 依赖 zeta-mcp（账本派生记录）。
    update_installed_plugins(
        |f| {
            f.account.plugins.insert(
                "other@audit-mp".to_string(),
                vec![InstalledPluginRecord {
                    install_path: None,
                    mcp_servers: vec![BundleId::try_from("zeta-mcp".to_string()).unwrap()],
                    extra: Map::new(),
                }],
            );
        },
        Some(&home),
        Some(&env),
    )
    .unwrap();

    let inner = CliMcpHooks::new(
        &comp,
        Some("audit".to_string()),
        Some("audit-mp".to_string()),
    )
    .await;
    let hooks = FailZeta { inner: &inner };
    let err = comp
        .enable_plugin("audit@audit-mp", EnableOptions::default(), Some(&hooks))
        .await
        .unwrap_err();
    assert!(
        matches!(err, PluginInstallError::Hook(_)),
        "原错误不吞: {err}"
    );

    let servers = comp.list_mcp_servers().await;
    assert!(
        !servers.iter().any(|c| c.name() == "alpha-mcp"),
        "生产 hook 必须真实 unmount alpha-mcp（运行期投影消失）：{servers:?}"
    );
    assert_eq!(
        std::fs::read(&mcp_path).unwrap(),
        mcp_bytes,
        "用户 mcp.json 不得被回滚删除/改写"
    );
    let other = load_installed_plugins(Some(&home), Some(&env)).account;
    assert_eq!(
        other.plugins["other@audit-mp"][0].mcp_servers,
        vec![BundleId::try_from("zeta-mcp".to_string()).unwrap()],
        "他 plugin 账本依赖记录不得被回滚删除/改写"
    );
}
