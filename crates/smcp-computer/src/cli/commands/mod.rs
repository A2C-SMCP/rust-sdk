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

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use console::style;
use serde_json::{Map, Value};

use crate::computer::{Computer, Session};
use crate::inputs::load_plugin_inputs;
use crate::mcp_clients::bundle_id::resolve_bundle_id;
use crate::mcp_clients::model::{BundleId, MCPServerConfig, ServerName};
use crate::settings::installer::{McpHookError, McpInstallHooks};
use crate::settings::scope::EnvMap;
use crate::settings::{
    resolve_policy_settings, resolve_settings, FileSkillGovernanceStore, ResolveSettingsArgs,
    ResolvedSettings,
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

/// 五层合并 settings（含 policy first-source-wins）/ five-layer merged settings incl. policy。
///
/// plugin（`enabledPlugins` / gc 声明视图）与 settings（merged show / get）共用。policy 层承载企业
/// allowed/deniedMcpServers（POLICY_ONLY 字段，批准门控须读到），故统一注入。#98：project/local 锚定
/// `cwd`（注入接缝，`None` → 进程 cwd）。
pub fn resolved_settings(
    cwd: Option<&Path>,
    env: Option<&EnvMap>,
    flag_path: Option<&Path>,
) -> Map<String, Value> {
    resolved_settings_with_errors(cwd, env, flag_path).settings
}

/// 同 [`resolved_settings`]，但**保留校验错误**（scope 越权 / 字段级判废）/ same, but keeps errors。
///
/// #143：scope 越权过滤（[`POLICY_ONLY_FIELDS`](crate::settings::POLICY_ONLY_FIELDS) /
/// [`TRUSTED_SCOPE_ONLY_FIELDS`](crate::settings::TRUSTED_SCOPE_ONLY_FIELDS)）**静默丢弃字段**——若调用方
/// 连错误也丢，用户就只能看到「我的 settings 莫名不生效」。协议指南 §2.1/§3 要求**响亮失败、不静默忽略**，
/// 故需要本变体把 `errors` 交给能呈现的调用方（如 boot 批准流程）。
///
/// [`resolved_settings`] 保留为薄封装：多数调用方（`settings show` / plugin 视图等）只关心合并值。
pub fn resolved_settings_with_errors(
    cwd: Option<&Path>,
    env: Option<&EnvMap>,
    flag_path: Option<&Path>,
) -> ResolvedSettings {
    let policy = resolve_policy_settings(env, None, None);
    resolve_settings(ResolveSettingsArgs {
        cwd,
        env,
        flag_settings_path: flag_path,
        policy_settings: Some(&policy),
    })
}

/// 把 settings 校验错误格式化为人读警示行（**纯函数**，供 boot 批准流程与 `settings show` 共用）/ format.
///
/// #143：scope 越权过滤（policy-only / 审批门 enable 方向判据）**静默丢弃字段** —— 若连错误也不呈现，用户
/// 只会看到「我的 settings 莫名不生效」。抽为纯函数以便**单测文案与 scope/field 拼装**，杜绝未来重构把「呈现」
/// 半程静默回退成吞错误（呈现行为无法在 `run_mcp_approval` 这类 `Session`-泛型异步副作用函数里直接断言）。
#[must_use]
pub fn format_settings_errors(errors: &[crate::settings::SettingsValidationError]) -> Vec<String> {
    errors
        .iter()
        .map(|e| {
            format!(
                "⚠ settings.json[{}]: {} — {}",
                e.scope.as_str(),
                e.field,
                e.reason
            )
        })
        .collect()
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
///
/// **重挂场景**（[`new_remount`](Self::new_remount)，#100 治理恢复）：单次 `reconcile_governance` 会跨**多个**
/// plugin 逐 server 回调，但本 hooks 只能持一份上下文。故 `roots`（`install_path → (plugin, marketplace)`）在
/// 构造期从 `collect_enabled_bundled_servers` 建索引，[`inject_inputs`](McpInstallHooks::inject_inputs) 据
/// `plugin_root` **按记录归属**前缀化注入（对齐 Python `_inject(record)` 从 record 取归属）；install/enable 单
/// plugin 场景 `roots` 为空、回退到构造期绑定的 `plugin`/`marketplace`。
pub struct CliMcpHooks<'a, S: Session> {
    comp: &'a Computer<S>,
    /// 构造期 server 快照：`bundle_id → display 名`（依赖预检输入；#139 由 name 集改 bundle_id 键）。
    existing: HashMap<BundleId, ServerName>,
    plugin: Option<String>,
    marketplace: Option<String>,
    /// 重挂归属索引：`install_path → (plugin, marketplace)`（单 plugin 场景为空）/ remount ownership index。
    roots: HashMap<PathBuf, (String, String)>,
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
            .map(|cfg| (resolve_bundle_id(cfg), cfg.name().to_string()))
            .collect();
        CliMcpHooks {
            comp,
            existing,
            plugin,
            marketplace,
            roots: HashMap::new(),
        }
    }

    /// 治理重挂用 hooks（#100）：从 ledger 派生 `install_path → 归属` 索引 + 快照当前 server 名 / remount hooks。
    ///
    /// `roots` 用**调用方传入的同一份 `declared`** 建索引——[`run_governance_remount`] 把该 `declared` 同时喂给
    /// [`Computer::reconcile_governance`](crate::computer::Computer::reconcile_governance)，故 `roots` 键
    /// （`install_path`）与 `reconcile_governance` 传给 `inject_inputs` 的路径**逐字对齐**（含 flag scope 一致）。
    pub async fn new_remount(
        comp: &'a Computer<S>,
        declared: &Map<String, Value>,
    ) -> CliMcpHooks<'a, S> {
        let home = comp.skill_home();
        let mut roots: HashMap<PathBuf, (String, String)> = HashMap::new();
        for rec in crate::settings::recovery::collect_enabled_bundled_servers(&home, None, declared)
        {
            roots
                .entry(rec.install_path)
                .or_insert((rec.plugin, rec.marketplace));
        }
        let existing = comp
            .list_mcp_servers()
            .await
            .iter()
            .map(|cfg| (resolve_bundle_id(cfg), cfg.name().to_string()))
            .collect();
        CliMcpHooks {
            comp,
            existing,
            plugin: None,
            marketplace: None,
            roots,
        }
    }
}

#[async_trait]
impl<S: Session> McpInstallHooks for CliMcpHooks<'_, S> {
    fn existing_servers(&self) -> HashMap<BundleId, ServerName> {
        self.existing.clone()
    }

    async fn register_server(&self, cfg: MCPServerConfig) -> Result<(), McpHookError> {
        // 无 scope 路径（trait 必需方法 + 外部直接调用）：走公开 [`Computer::mount_server`]（裸 input 仅全局解析）。
        // plugin-bound 注册走 [`Self::register_server_with_input_scope`]。
        // #113 S6：治理物化走**运行期挂载**（不落盘）——bundled server 归属 ledger 意图，不得写入 project mcp.json。
        self.comp
            .mount_server(cfg)
            .await
            .map_err(|e| McpHookError(e.to_string()))
    }

    async fn register_server_with_input_scope(
        &self,
        cfg: MCPServerConfig,
        plugin_id: Option<&str>,
    ) -> Result<(), McpHookError> {
        // §5.11（a2c-smcp-protocol v0.3.1）：把既有 plugin_id（`<plugin>@<marketplace>`，enable/reconcile 透传）
        // 派生为 [`PluginScope`]，透传进 scope-aware [`Computer::mount_server_with_scope`]，使 plugin-bound server
        // 裸 `${input:<id>}` 按 `<P>@<M>/<id>` → 全局 `<id>` 序解析。分叉二裁决：客户端用标准生命周期即无感，
        // scope 从自有 ledger 派生、**不**经客户端可见 API。plugin_id 非法/None → 退化为无 scope（mount_server）。
        let scope = plugin_id.and_then(crate::inputs::plugin_pool::PluginScope::from_plugin_id);
        self.comp
            .mount_server_with_scope(cfg, scope)
            .await
            .map_err(|e| McpHookError(e.to_string()))
    }

    async fn remove_server(&self, id: &BundleId) -> Result<(), McpHookError> {
        // #113 S6：治理级联停摘走**运行期卸载**（不删 config 声明）——bundled server 本不在用户 config 层。
        // #139/#141：按 bundle_id 精确停摘，经合并后的 `unmount_server(&BundleId)`（R4：库层收 bundle_id）。
        // 治理级联本就幂等（账本条目可能早已不活跃）——`false`（本无实例）不是错误，忽略回执即可。
        self.comp
            .unmount_server(id)
            .await
            .map(|_| ())
            .map_err(|e| McpHookError(e.to_string()))
    }

    async fn inject_inputs(&self, plugin_root: &Path) -> Result<(), McpHookError> {
        // 归属上下文：优先按 `install_path` 查重挂归属索引（一 hooks 跨多 plugin，#100）；回退构造期绑定的单
        // plugin 上下文（install/enable）。二者皆无（卸载级联）→ no-op。
        let (plugin, marketplace) = if let Some((p, m)) = self.roots.get(plugin_root) {
            (p.as_str(), m.as_str())
        } else if let (Some(p), Some(m)) = (&self.plugin, &self.marketplace) {
            (p.as_str(), m.as_str())
        } else {
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

/// 启动期治理重挂（#100 / python-sdk#117 设计 Y 的 client 接线**参考实现**）/ boot-time governance remount。
///
/// `boot_up` 已恢复 bundled SKILL（skills-only，§4.8 #93 边界：SDK 不擅自拉 MCP 进程）；此处 CLI 作为**参考
/// client** 经公共 API [`Computer::reconcile_governance`] 显式重挂 enabled bundled MCP server（外部 client /
/// 未来 GUI 照抄本函数）。对齐 Python `cli/commands/plugin.py::run_governance_remount`：
/// - `existing_server_names` 取自活跃 `Computer`（[`CliMcpHooks::new_remount`] 构造期快照）→ 同名冲突由
///   `reconcile_governance` 内部 **skip+WARN**（用户配置胜）；bundled **免批准**（§5.10 不走 project 信任门）；
/// - inputs 注入先于 register（bundled server `${input:}` 经 D2 前缀回退解析，与 install/enable 流一致）；
/// - `declared` 为 **flag-aware** 合并视图（`resolved_settings(_, _, flag_path)`，对齐 Python
///   `run_governance_remount(flag_config=...)`）：`--settings`-scope 的 `enabledPlugins=false` 在重挂阶段生效；
///   同一份 `declared` 既建归属索引又驱动重挂 → `roots` 键与迭代集对齐；
/// - 单 server 失败不阻断；marketplace 降级仅 WARN；整体**非阻塞**（与 `a2c-computer run` 启动隔离策略一致）。
///
/// [`Computer::reconcile_governance`]: crate::computer::Computer::reconcile_governance
pub async fn run_governance_remount<S: Session>(comp: &Computer<S>, flag_path: Option<&Path>) {
    let declared = resolved_settings(None, None, flag_path);
    let hooks = CliMcpHooks::new_remount(comp, &declared).await;
    let report = comp
        .reconcile_governance(Some(&hooks), Some(&declared))
        .await;
    if !report.restored_skills.is_empty() {
        msg_ok(&format!(
            "governance recovery: {} skill(s) restored",
            report.restored_skills.len()
        ));
    }
    for name in &report.remounted_servers {
        msg_ok(&format!("restored bundled MCP server {name:?}"));
    }
    for marketplace in &report.failed_marketplaces {
        msg_warn(&format!(
            "⚠ marketplace {marketplace:?} degraded during governance recovery (skills/servers not restored)"
        ));
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
    use crate::settings::{SettingsScope, SettingsValidationError};

    /// #143：`format_settings_errors` 的文案与 scope/field 拼装（呈现半程的可断言守护——`run_mcp_approval`
    /// 是 `Session`-泛型异步副作用函数，无法直接断言其打印；抽纯函数即为此）。
    #[test]
    fn format_settings_errors_pins_scope_field_reason() {
        let errors = vec![
            SettingsValidationError {
                scope: SettingsScope::Project,
                field: "enableAllProjectMcpServers".to_string(),
                reason: "approval-gate field not allowed in the project scope (filtered)"
                    .to_string(),
                source_path: None,
            },
            SettingsValidationError {
                scope: SettingsScope::User,
                field: "allowedMcpServers".to_string(),
                reason: "policy-only field not allowed outside the policy scope (filtered)"
                    .to_string(),
                source_path: None,
            },
        ];
        let lines = format_settings_errors(&errors);
        assert_eq!(lines.len(), 2);
        assert!(
            lines[0].contains("[project]"),
            "须含 scope，实得 {}",
            lines[0]
        );
        assert!(
            lines[0].contains("enableAllProjectMcpServers"),
            "须含 field"
        );
        assert!(lines[0].contains("project scope"), "须含 reason");
        assert!(lines[1].contains("[user]") && lines[1].contains("allowedMcpServers"));
        // 空输入 → 空输出（无噪音）。
        assert!(format_settings_errors(&[]).is_empty());
    }

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
        assert!(hooks.existing_servers().is_empty());
        // inject 无 inputs.json → no-op（不 panic）。
        let tmp = std::env::temp_dir();
        assert!(hooks.inject_inputs(&tmp).await.is_ok());
    }

    #[tokio::test]
    async fn cli_hooks_register_with_input_scope_threads_scope_into_render() {
        // §5.11 wiring（#155）：register_server_with_input_scope(Some(plugin_id)) 经 CliMcpHooks override →
        // mount_server_with_scope → render 按 plugin scope 解析裸 `${input:}`。池里仅 scoped def（无值/默认/resolver）
        // → Missing(scoped) 上抛。证明 scope 透传：若 wiring 断了（退化为无 scope 的 register_server），裸 "token"
        // 查不到 scoped 池条目 → 占位符字面保留 → Ok，本测试会失败。
        use crate::mcp_clients::model::{
            MCPServerConfig, MCPServerInput, PromptStringInput, StdioServerConfig,
            StdioServerParameters,
        };
        use std::collections::HashMap;

        let mut inputs = HashMap::new();
        inputs.insert(
            "figma@acme/token".to_string(),
            MCPServerInput::PromptString(PromptStringInput {
                id: "figma@acme/token".to_string(),
                description: String::new(),
                default: None,
                password: Some(false),
            }),
        );
        let comp = Computer::new(
            "wiring",
            SilentSession::new("t"),
            Some(inputs),
            None,
            false,
            false,
        );
        let hooks = CliMcpHooks::new(&comp, Some("figma".into()), Some("acme".into())).await;
        let cfg = MCPServerConfig::Stdio(StdioServerConfig {
            env_file: None,
            name: "s".to_string(),
            bundle_id: None,
            disabled: false,
            forbidden_tools: vec![],
            tool_meta: HashMap::new(),
            default_tool_meta: None,
            vrl: None,
            server_parameters: StdioServerParameters {
                command: "echo".to_string(),
                args: vec!["${input:token}".to_string()],
                env: HashMap::new(),
                cwd: None,
            },
        });
        let err = hooks
            .register_server_with_input_scope(cfg, Some("figma@acme"))
            .await
            .unwrap_err();
        let McpHookError(msg) = err;
        assert!(
            msg.contains("figma@acme/token"),
            "scoped id should surface in error, got: {msg}"
        );
    }

    /// #100 item1：`new_remount` 从 ledger 建 `install_path → 归属` 索引；`inject_inputs` **按记录归属**
    /// 前缀化注入（plugin/marketplace 绑定为 `None`，仅靠 `roots` 才能命中——证明多-plugin 重挂正确前缀）。
    #[tokio::test]
    async fn new_remount_indexes_ownership_and_inject_uses_per_root_context() {
        use crate::settings::store::update_installed_plugins;
        use crate::settings::InstalledPluginRecord;
        use serde_json::json;

        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();

        // 造 plugin 安装树：<home>/plug/mcp-servers/{a-mcp.json, inputs.json}。
        let install_path = home.join("plug");
        let servers = install_path.join("mcp-servers");
        std::fs::create_dir_all(&servers).unwrap();
        std::fs::write(
            servers.join("a-mcp.json"),
            r#"{"type":"stdio","name":"a-mcp","server_parameters":{"command":"node","args":["${input:tok}"]}}"#,
        )
        .unwrap();
        std::fs::write(
            servers.join("inputs.json"),
            r#"[{"id":"tok","type":"PromptString","description":"t"}]"#,
        )
        .unwrap();

        // seed ledger（env=None → 与 new_remount / collect 的读取路径一致）。
        update_installed_plugins(
            |file| {
                file.account.plugins.insert(
                    "remounttest@acme".to_string(),
                    vec![InstalledPluginRecord {
                        install_path: Some(install_path.to_string_lossy().into_owned()),
                        mcp_servers: vec![BundleId::try_from("a-mcp".to_string()).unwrap()],
                        extra: Map::from_iter([("scope".to_string(), json!("user"))]),
                    }],
                );
            },
            Some(home),
            None,
        )
        .unwrap();

        let comp = cli_computer().with_skill_home(home.to_path_buf());

        // v0.3.0：seed 安装意图 + 显式启用（absent 不再默认启用），hermetic 不读真实用户配置。
        crate::settings::store::update_installed_plugins_intent(
            |f| {
                f.account
                    .installed_plugins
                    .insert("remounttest@acme".to_string());
            },
            Some(home),
            None,
        )
        .unwrap();
        let declared = json!({ "enabledPlugins": { "remounttest@acme": true } })
            .as_object()
            .unwrap()
            .clone();
        // new_remount 建归属索引：install_path → (plugin, marketplace)。
        let hooks = CliMcpHooks::new_remount(&comp, &declared).await;
        assert_eq!(
            hooks.roots.get(&install_path),
            Some(&("remounttest".to_string(), "acme".to_string())),
            "roots 应按 install_path 索引记录归属"
        );

        // inject_inputs 靠 roots 归属前缀化入池（绑定 plugin/marketplace=None，若不查 roots 则会 no-op）。
        hooks.inject_inputs(&install_path).await.unwrap();
        assert!(
            comp.get_input("remounttest@acme/tok")
                .await
                .unwrap()
                .is_some(),
            "应按记录归属前缀化注入 <plugin>@<marketplace>/<id>"
        );
    }

    /// #100 item1/3：**多 plugin 同批重挂**——各 `install_path` 的 inputs 按**各自**归属前缀化，前缀不串。
    #[tokio::test]
    async fn new_remount_multi_plugin_prefixes_do_not_cross() {
        use crate::settings::store::update_installed_plugins;
        use crate::settings::InstalledPluginRecord;
        use serde_json::json;

        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();

        // 两个 plugin 安装树，各含**同 id** `tok` 的 input，但归属不同 → 前缀须区分。
        let mk = |name: &str| -> std::path::PathBuf {
            let root = home.join(name);
            let servers = root.join("mcp-servers");
            std::fs::create_dir_all(&servers).unwrap();
            std::fs::write(
                servers.join(format!("{name}-mcp.json")),
                format!(r#"{{"type":"stdio","name":"{name}-mcp","server_parameters":{{"command":"node"}}}}"#),
            )
            .unwrap();
            std::fs::write(
                servers.join("inputs.json"),
                r#"[{"id":"tok","type":"PromptString","description":"t"}]"#,
            )
            .unwrap();
            root
        };
        let alpha_root = mk("alpha");
        let beta_root = mk("beta");

        update_installed_plugins(
            |file| {
                file.account.plugins.insert(
                    "alpha@m1".to_string(),
                    vec![InstalledPluginRecord {
                        install_path: Some(alpha_root.to_string_lossy().into_owned()),
                        mcp_servers: vec![BundleId::try_from("alpha-mcp".to_string()).unwrap()],
                        extra: Map::from_iter([("scope".to_string(), json!("user"))]),
                    }],
                );
                file.account.plugins.insert(
                    "beta@m2".to_string(),
                    vec![InstalledPluginRecord {
                        install_path: Some(beta_root.to_string_lossy().into_owned()),
                        mcp_servers: vec![BundleId::try_from("beta-mcp".to_string()).unwrap()],
                        extra: Map::from_iter([("scope".to_string(), json!("user"))]),
                    }],
                );
            },
            Some(home),
            None,
        )
        .unwrap();

        let comp = cli_computer().with_skill_home(home.to_path_buf());
        // v0.3.0：seed 安装意图 + 两 plugin 显式启用。
        crate::settings::store::update_installed_plugins_intent(
            |f| {
                f.account.installed_plugins.insert("alpha@m1".to_string());
                f.account.installed_plugins.insert("beta@m2".to_string());
            },
            Some(home),
            None,
        )
        .unwrap();
        let declared = json!({ "enabledPlugins": { "alpha@m1": true, "beta@m2": true } })
            .as_object()
            .unwrap()
            .clone();
        let hooks = CliMcpHooks::new_remount(&comp, &declared).await;

        // 两根各自归属正确（不混）。
        assert_eq!(
            hooks.roots.get(&alpha_root),
            Some(&("alpha".to_string(), "m1".to_string()))
        );
        assert_eq!(
            hooks.roots.get(&beta_root),
            Some(&("beta".to_string(), "m2".to_string()))
        );

        // 各根注入 → 同 id `tok` 落到**各自**前缀键，不串。
        hooks.inject_inputs(&alpha_root).await.unwrap();
        hooks.inject_inputs(&beta_root).await.unwrap();
        assert!(comp.get_input("alpha@m1/tok").await.unwrap().is_some());
        assert!(comp.get_input("beta@m2/tok").await.unwrap().is_some());
    }
}
