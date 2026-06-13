/*!
* 文件名: mod.rs
* 作者: JQQ
* 创建日期: 2026/06/10
* 最后修改日期: 2026/06/10
* 版权: 2023 JQQ. All rights reserved.
* 依赖: clap, console, serde_json, async-trait
* 描述: CLI 命令核心（治理层子命令 marketplace/plugin/settings/skill 共用接缝）
*       CLI command core — shared seams for the governance subcommands.
*
* 对标 Python `a2c_smcp/computer/cli/commands/__init__.py`：把 marketplace / plugin / settings / skill
* 命令业务逻辑抽成显式资源（`registry` / `home` / `env` + flags）的 handler，便于隔离单测；REPL 经各模块
* 的 `repl_dispatch`（#54）把活跃 `Computer` 的 registry / home / session 绑定进去，Typer 非交互（#54）则构造
* 轻量上下文（不 boot Computer）。本模块只放跨命令共享的接缝：
*   - `CliMcpHooks`：从活跃 `Computer` 装配 installer / 卸载级联所需的 MCP 注入回调（对标 Python
*     `build_mcp_callbacks` / `McpCallbacks`；Rust 后端把四回调统一进 `McpInstallHooks` trait）。
*   - `Confirm`：异步 y/N 信任/卸载确认闸门（REPL 接 session prompt；Typer 非交互传 `None`）。
*   - `flag_value` / `resolved_settings`：REPL 行解析与六层合并视图。
*/

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use console::style;
use serde_json::{Map, Value};

use crate::computer::{Computer, Session};
use crate::inputs::load_plugin_inputs;
use crate::mcp_clients::model::MCPServerConfig;
use crate::settings::installer::{McpHookError, McpInstallHooks};
use crate::settings::scope::EnvMap;
use crate::settings::{
    resolve_policy_settings, resolve_settings, FileSkillGovernanceStore, ResolveSettingsArgs,
};
use crate::skills::manifest::{MCP_INPUTS_FILENAME, MCP_SERVERS_SUBDIR};

pub mod handler;
pub mod marketplace;
pub mod plugin;
pub mod settings;
pub mod skill;

// REPL 适配器复用既有 handler 的运行时类型 / re-export for the REPL entry & interactive loop。
pub use handler::{CliConfig, CommandError, CommandHandler};

// ── 退出码语义（§4.6）/ Exit code semantics ──────────────────────────────────
/// 成功 / success。
pub const EXIT_OK: i32 = 0;
/// 用户错（校验 / 未安装 / 冲突 / 中止 / 只读层）/ user error。
pub const EXIT_USER_ERROR: i32 = 1;
/// 网络错（clone/stage 失败）/ network error。
pub const EXIT_NETWORK_ERROR: i32 = 2;

// ── 输出辅助 / output helpers ────────────────────────────────────────────────
/// 结构化 JSON 输出（`--json`）/ pretty-print a JSON value。
pub(crate) fn print_json(value: &Value) {
    match serde_json::to_string_pretty(value) {
        Ok(text) => println!("{text}"),
        Err(_) => println!("{value}"),
    }
}

/// 绿色 ✓ 成功行 / green success line。
pub(crate) fn msg_ok(s: &str) {
    println!("{}", style(format!("✓ {s}")).green());
}

/// 红色 ✗ 错误行 / red error line。
pub(crate) fn msg_err(s: &str) {
    println!("{}", style(format!("✗ {s}")).red());
}

/// 黄色提示行（usage / 警告）/ yellow hint line。
pub(crate) fn msg_warn(s: &str) {
    println!("{}", style(s).yellow());
}

/// 暗色次要行 / dim secondary line。
pub(crate) fn msg_dim(s: &str) {
    println!("{}", style(s).dim());
}

/// 绿色 ✓ 成功行 + 返回 [`EXIT_OK`]（handler 收尾糖）/ success line returning EXIT_OK。
pub(crate) fn ok_msg(msg: &str) -> i32 {
    msg_ok(msg);
    EXIT_OK
}

/// 扁平错误输出 + 返回退出码（`--json` → `{"error": msg}`；否则红色 ✗）/ flat error output returning a code。
///
/// marketplace / settings / skill 共用（对标 Python 各模块 `_err(msg, json_output)`）；plugin 的
/// `{error, message}` 变体单列（额外 `error_code`）/ shared flat shape; plugin uses its own variant。
pub(crate) fn err_flat(msg: &str, json_output: bool, code: i32) -> i32 {
    if json_output {
        print_json(&serde_json::json!({ "error": msg }));
    } else {
        msg_err(msg);
    }
    code
}

/// 由 `home` / `env` 装配文件式治理存储（marketplace/plugin 命令的 prune/gc/recorder 共用）/ file store。
pub(crate) fn file_store(home: &Path, env: Option<&EnvMap>) -> FileSkillGovernanceStore {
    match env {
        Some(e) => FileSkillGovernanceStore::with_env(home, e.clone()),
        None => FileSkillGovernanceStore::new(home),
    }
}

// ── 异步确认闸门 / async confirm gate ────────────────────────────────────────
/// 信任 / 卸载 / gc 的异步 y/N 确认回调（对标 Python `ConfirmFn = Callable[[str], Awaitable[bool]]`）。
///
/// handler 只传**待确认目标**（marketplace add=url、remove=name、gc=孤儿列表串），由实现方（REPL session
/// prompt / 测试 mock）决定提示语与读取方式。Typer 非交互传 `None`（marketplace add 退回「须 --trust」）。
#[async_trait]
pub trait Confirm: Send + Sync {
    /// 展示 `target` 并询问是否继续 / show `target` and ask whether to proceed。
    async fn confirm(&self, target: &str) -> bool;
}

// ── REPL 行解析接缝 / shared REPL parse seam ──────────────────────────────────
/// 取 `--flag value` 形态的值（**不支持** `--flag=value`，REPL 简化）/ extract a `--flag value` pair。
///
/// 四个命令模块（marketplace / skill / plugin / settings）的 REPL dispatcher 共用，避免 4 处分叉。
pub fn flag_value(args: &[String], flag: &str) -> Option<String> {
    let idx = args.iter().position(|a| a == flag)?;
    let next = args.get(idx + 1)?;
    if next.starts_with("--") {
        None
    } else {
        Some(next.clone())
    }
}

/// 六层合并 settings（含 policy first-source-wins）/ six-layer merged settings incl. policy。
///
/// plugin（`enabledPlugins` / gc 声明视图）与 settings（merged show / get）共用。policy 层承载企业
/// allowed/deniedMcpServers（POLICY_ONLY 字段，批准门控须读到），故统一注入。
pub fn resolved_settings(
    registered_workdirs: &[PathBuf],
    active_workdir: Option<&Path>,
    env: Option<&EnvMap>,
    flag_path: Option<&Path>,
) -> Map<String, Value> {
    let policy = resolve_policy_settings(env, None, None);
    resolve_settings(ResolveSettingsArgs {
        registered_workdirs,
        active_workdir,
        env,
        flag_settings_path: flag_path,
        policy_settings: Some(&policy),
    })
    .settings
}

// ── MCP 注入回调装配（对标 build_mcp_callbacks / McpCallbacks）/ MCP hooks wiring ──
/// 从活跃 `Computer` 装配 installer / 卸载级联所需的 MCP 注入回调 / wire MCP hooks from a live Computer。
///
/// Python 把 `existing_server_names` / `register_server` / `remove_server` 三回调装进 `McpCallbacks`
/// dataclass，`inject_inputs` 单独传；Rust 后端把四者统一进 [`McpInstallHooks`] trait——故本结构一次实现
/// 四方法，既供 install/enable（existing+register+inject）也供 uninstall/disable/marketplace-remove 级联
/// （remove）。`plugin` / `marketplace` 上下文用于 `inject_inputs` 把 `<plugin_root>/mcp-servers/inputs.json`
/// 前缀化为 `<plugin>@<marketplace>/<id>` 注入 Computer 输入池（§9.3 D2）；卸载级联无需注入时传 `None`。
///
/// `existing_server_names` 须同步返回（trait 约束），故在构造期**快照**当前 server 名集合——installer 的冲突
/// 闸门在注册任何 bundled server 之前一次性读取，快照语义与之吻合。
pub struct CliMcpHooks<'a, S: Session> {
    comp: &'a Computer<S>,
    existing: HashSet<String>,
    plugin: Option<String>,
    marketplace: Option<String>,
}

impl<'a, S: Session> CliMcpHooks<'a, S> {
    /// 快照当前 server 名 + 绑定 plugin/marketplace 上下文 / snapshot server names + bind context。
    pub async fn new(
        comp: &'a Computer<S>,
        plugin: Option<String>,
        marketplace: Option<String>,
    ) -> CliMcpHooks<'a, S> {
        let existing = comp
            .list_mcp_servers()
            .await
            .iter()
            .map(|cfg| cfg.name().to_string())
            .collect();
        CliMcpHooks {
            comp,
            existing,
            plugin,
            marketplace,
        }
    }
}

#[async_trait]
impl<S: Session> McpInstallHooks for CliMcpHooks<'_, S> {
    fn existing_server_names(&self) -> HashSet<String> {
        self.existing.clone()
    }

    async fn register_server(&self, cfg: MCPServerConfig) -> Result<(), McpHookError> {
        // TODO(#74 INT-04): 透传 plugin/marketplace 上下文给 add_or_update_server，使 bundled server 的裸
        // `${input:id}` 经 resolver D2 前缀回退命中带前缀池条目（Python `_plugin_register_cb` 已传上下文；
        // 当前 Computer::add_or_update_server 尚不接受该上下文——故 inject_inputs 前缀注入、register 不透传，
        // 暂以「显式引用带前缀 id」可用，裸 id 回退待 #74）。
        self.comp
            .add_or_update_server(cfg)
            .await
            .map_err(|e| McpHookError(e.to_string()))
    }

    async fn remove_server(&self, name: &str) -> Result<(), McpHookError> {
        self.comp
            .remove_server(name)
            .await
            .map_err(|e| McpHookError(e.to_string()))
    }

    async fn inject_inputs(&self, plugin_root: &Path) -> Result<(), McpHookError> {
        // 仅 plugin install/enable 上下文（plugin+marketplace 齐备）注入；卸载级联无上下文 → no-op。
        let (Some(plugin), Some(marketplace)) = (&self.plugin, &self.marketplace) else {
            return Ok(());
        };
        let inputs_json = plugin_root
            .join(MCP_SERVERS_SUBDIR)
            .join(MCP_INPUTS_FILENAME);
        for inp in load_plugin_inputs(&inputs_json, plugin, marketplace) {
            self.comp
                .add_or_update_input(inp)
                .await
                .map_err(|e| McpHookError(e.to_string()))?;
        }
        Ok(())
    }
}

/// hermetic 测试环境映射：user settings 落 `home/.config`（XDG_CONFIG_HOME）、HOME 兜底，避免污染真实用户配置。
/// 供 marketplace / plugin / settings / skill 各命令模块的测试共用 / shared hermetic env fixture for command tests。
#[cfg(test)]
pub(crate) fn test_env(home: &Path) -> std::collections::HashMap<String, String> {
    let mut env = std::collections::HashMap::new();
    env.insert(
        "XDG_CONFIG_HOME".to_string(),
        home.join("xdg-config").to_string_lossy().into_owned(),
    );
    env.insert("HOME".to_string(), home.to_string_lossy().into_owned());
    env
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computer::{Computer, SilentSession};

    fn cli_computer() -> Computer<SilentSession> {
        Computer::new(
            "test-friday",
            SilentSession::new("cli-test"),
            None,
            None,
            false,
            false,
        )
    }

    #[test]
    fn flag_value_extracts_pair_and_rejects_flag_like_value() {
        let args: Vec<String> = ["--name", "acme", "--trust", "--scope"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(flag_value(&args, "--name"), Some("acme".to_string()));
        // --trust 后跟 --scope（flag-like）→ None；末尾 flag 无值 → None。
        assert_eq!(flag_value(&args, "--trust"), None);
        assert_eq!(flag_value(&args, "--scope"), None);
        assert_eq!(flag_value(&args, "--missing"), None);
    }

    #[tokio::test]
    async fn cli_mcp_hooks_snapshots_existing_server_names() {
        // 回调装配：空 Computer → 快照为空；占位上下文不触发注入。
        let comp = cli_computer();
        let hooks = CliMcpHooks::new(&comp, Some("figma".into()), Some("acme".into())).await;
        assert!(hooks.existing_server_names().is_empty());
        // inject 无 inputs.json → no-op（不 panic）。
        let tmp = std::env::temp_dir();
        assert!(hooks.inject_inputs(&tmp).await.is_ok());
    }
}
