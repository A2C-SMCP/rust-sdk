/*!
* 文件名: mod.rs
* 作者: JQQ
* 创建日期: 2025/12/16
* 最后修改日期: 2026/06/10
* 版权: 2023 JQQ. All rights reserved.
* 依赖: clap, tokio, console, rustyline
* 描述: CLI 模块入口 + 治理层子命令树 + 根级全局 flag 透传 / CLI entry + governance subcommand tree.
*
* 对标 Python `a2c_smcp/computer/cli/main.py`：根回调采集全局 flag（`--settings` / `--add-dir` / `--approve-all-mcp`）
* 经 [`RootState`] 透传给**所有**子命令（含 `settings`）——修 #97「flag scope 恒空」bug（此前仅 `run` 路径拿到）。
* 非交互子命令（marketplace/plugin/settings/skill）不 boot Computer，离线经 `rebuild_registry` 构上下文；
* `run`（默认）进 REPL：建 Computer、banner、启动期 MCP 批准框、交互循环。
*/

use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

use crate::computer::{Computer, SilentSession};

pub mod approval;
pub mod banner;
pub mod commands;
pub mod completer;
pub mod help;
mod interactive;
pub mod progress;
pub mod repl;
mod utils;

use commands::marketplace::{
    marketplace_add, marketplace_info, marketplace_list, marketplace_refresh, marketplace_remove,
    marketplace_set, MarketplaceAddOptions, MarketplaceRemoveOptions,
};
use commands::plugin::{
    plugin_disable, plugin_enable, plugin_gc, plugin_info, plugin_install, plugin_list,
    plugin_uninstall, PluginInstallOptions,
};
use commands::settings::{settings_edit, settings_get, settings_set, settings_show};
use commands::skill::{rebuild_registry, skill_info, skill_list};
use commands::CommandHandler;
use progress::clone_spinner;

/// A2C-SMCP Computer CLI - 计算机客户端命令行工具
#[derive(Parser, Debug)]
#[command(name = "smcp-computer")]
#[command(about = "A2C-SMCP Computer CLI - 计算机客户端命令行工具", long_about = None)]
#[command(version = env!("CARGO_PKG_VERSION"))]
pub struct Args {
    /// 是否自动连接 / Auto connect
    #[arg(long, default_value = "true")]
    pub auto_connect: bool,

    /// 是否自动重连 / Auto reconnect
    #[arg(long, default_value = "true")]
    pub auto_reconnect: bool,

    /// Socket.IO 服务器URL，例如 https://host:port
    #[arg(long)]
    pub url: Option<String>,

    /// Socket.IO 命名空间（默认: /smcp）
    #[arg(long, default_value = "/smcp")]
    pub namespace: String,

    /// 认证参数，形如 key:value,foo:bar
    #[arg(long)]
    pub auth: Option<String>,

    /// 请求头参数，形如 key:value,foo:bar
    #[arg(long)]
    pub headers: Option<String>,

    /// 关闭彩色输出
    #[arg(long)]
    pub no_color: bool,

    /// flag scope settings.json 路径（最低优先级，§5.5）/ flag-scope settings.json file
    #[arg(long)]
    pub settings: Option<PathBuf>,

    /// 注册工作目录（可重复；首个作 active-workdir）/ register a workdir (repeatable; first = active)
    #[arg(long = "add-dir")]
    pub add_dir: Vec<PathBuf>,

    /// 启动期一次性批准全部待决 MCP server（不落盘）/ approve all pending MCP servers this run (no persist)
    #[arg(long)]
    pub approve_all_mcp: bool,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// 启动计算机并进入持续运行模式（REPL）
    Run {
        /// 在启动时从文件加载 MCP Servers 配置
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// 在启动时从文件加载 Inputs 定义
        #[arg(short, long)]
        inputs: Option<PathBuf>,
    },
    /// SKILL marketplace 管理（git 源）/ SKILL marketplaces
    Marketplace {
        #[command(subcommand)]
        action: MarketplaceCmd,
    },
    /// Plugin（skill+mcp bundle）安装/启停 / plugins
    Plugin {
        #[command(subcommand)]
        action: PluginCmd,
    },
    /// settings.json 意图层读写 / settings intent layer
    Settings {
        #[command(subcommand)]
        action: SettingsCmd,
    },
    /// 跨源 SKILL 查询 / cross-source skill query
    Skill {
        #[command(subcommand)]
        action: SkillCmd,
    },
}

#[derive(Subcommand, Debug)]
pub enum MarketplaceCmd {
    Add {
        git_url: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        trust: bool,
        #[arg(long)]
        auto_update: bool,
        #[arg(long)]
        no_clone: bool,
        #[arg(long)]
        json: bool,
    },
    List {
        #[arg(long)]
        json: bool,
    },
    Info {
        name: String,
        #[arg(long)]
        json: bool,
    },
    Remove {
        name: String,
        #[arg(long)]
        keep_plugins: bool,
        #[arg(long)]
        json: bool,
    },
    Refresh {
        target: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// marketplace set <name> auto-update=<bool>
    Set {
        name: String,
        assignment: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum PluginCmd {
    Install {
        id: String,
        #[arg(long, default_value = "user")]
        scope: String,
        #[arg(long)]
        version: Option<String>,
        #[arg(long)]
        json: bool,
    },
    Uninstall {
        id: String,
        #[arg(long)]
        keep_servers: bool,
        #[arg(long)]
        json: bool,
    },
    Enable {
        id: String,
        #[arg(long)]
        json: bool,
    },
    Disable {
        id: String,
        #[arg(long)]
        json: bool,
    },
    List {
        #[arg(long)]
        available: bool,
        #[arg(long)]
        json: bool,
    },
    Info {
        id: String,
        #[arg(long)]
        json: bool,
    },
    Gc {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum SettingsCmd {
    Show {
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        json: bool,
    },
    Get {
        key: String,
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        json: bool,
    },
    Set {
        key: String,
        value: String,
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        json: bool,
    },
    Edit {
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum SkillCmd {
    List {
        #[arg(long)]
        source: Option<String>,
        #[arg(long)]
        json: bool,
    },
    Info {
        name: String,
        #[arg(long)]
        json: bool,
    },
}

/// 根回调采集、透传给所有子命令的根级上下文（修 #97 flag scope 恒空）/ root context propagated to all subcommands。
#[derive(Debug, Clone, Default)]
pub struct RootState {
    pub flag_path: Option<PathBuf>,
    pub registered_workdirs: Vec<PathBuf>,
    pub active_workdir: Option<PathBuf>,
    pub approve_all_mcp: bool,
}

/// `--add-dir` → `(registered_workdirs, active_workdir)`：绝对化、去重保序；active 取首个 / resolve workdirs。
fn resolve_workdirs(add_dir: &[PathBuf]) -> (Vec<PathBuf>, Option<PathBuf>) {
    let mut registered: Vec<PathBuf> = Vec::new();
    for dir in add_dir {
        let abs = absolutize(dir);
        if !registered.contains(&abs) {
            registered.push(abs);
        }
    }
    let active = registered.first().cloned();
    (registered, active)
}

fn absolutize(path: &Path) -> PathBuf {
    // `~` 展开。
    let expanded = if let Ok(rest) = path.strip_prefix("~") {
        match dirs::home_dir() {
            Some(home) => home.join(rest),
            None => path.to_path_buf(),
        }
    } else {
        path.to_path_buf()
    };
    // 词法绝对化（不要求存在，对齐 Python `Path.resolve(strict=False)`）。
    std::path::absolute(&expanded).unwrap_or(expanded)
}

fn skill_home() -> PathBuf {
    use crate::skills::{ensure_skill_home, resolve_skill_home};
    ensure_skill_home(None).unwrap_or_else(|_| resolve_skill_home(None))
}

pub fn main() {
    let args = Args::parse();
    if args.no_color {
        console::set_colors_enabled(false);
    }
    let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
    let code = rt.block_on(dispatch(args));
    std::process::exit(code);
}

async fn dispatch(mut args: Args) -> i32 {
    let (registered_workdirs, active_workdir) = resolve_workdirs(&args.add_dir);
    let root = RootState {
        flag_path: args.settings.clone(),
        registered_workdirs,
        active_workdir,
        approve_all_mcp: args.approve_all_mcp,
    };

    // 取出 subcommand（避免部分移动后再借 `&args` 给 run_repl）/ take command out so `&args` stays valid。
    let command = args.command.take();
    match command {
        Some(Commands::Marketplace { action }) => dispatch_marketplace(action).await,
        Some(Commands::Plugin { action }) => dispatch_plugin(action, &root).await,
        Some(Commands::Settings { action }) => dispatch_settings(action, &root),
        Some(Commands::Skill { action }) => dispatch_skill(action).await,
        Some(Commands::Run { config, inputs }) => {
            run_repl(&args, &root, config, inputs).await;
            0
        }
        None => {
            run_repl(&args, &root, None, None).await;
            0
        }
    }
}

// ── 非交互子命令分发（不 boot Computer；离线 registry）/ non-interactive dispatch ──
async fn dispatch_marketplace(action: MarketplaceCmd) -> i32 {
    let home = skill_home();
    let mut registry = rebuild_registry(&home, None).await;
    match action {
        MarketplaceCmd::Add {
            git_url,
            name,
            trust,
            auto_update,
            no_clone,
            json,
        } => {
            let _spinner = clone_spinner("Cloning marketplace...", !json && !no_clone);
            marketplace_add(
                &mut registry,
                &home,
                None,
                &git_url,
                MarketplaceAddOptions {
                    name: name.as_deref(),
                    trust,
                    auto_update,
                    no_clone,
                    confirm: None, // 非交互无 confirm → add 须 --trust。
                    json_output: json,
                },
            )
            .await
        }
        MarketplaceCmd::List { json } => marketplace_list(&home, None, json),
        MarketplaceCmd::Info { name, json } => marketplace_info(&home, None, &name, json),
        MarketplaceCmd::Remove {
            name,
            keep_plugins,
            json,
        } => {
            marketplace_remove(
                &mut registry,
                &home,
                None,
                &name,
                MarketplaceRemoveOptions {
                    keep_plugins,
                    confirm: None,
                    hooks: None, // ledger-only：MCP 摘除延到下次 REPL boot。
                    json_output: json,
                },
            )
            .await
        }
        MarketplaceCmd::Refresh { target, json } => {
            let _spinner = clone_spinner("Refreshing marketplaces...", !json);
            marketplace_refresh(
                &mut registry,
                &home,
                None,
                target.as_deref().unwrap_or("all"),
                json,
            )
            .await
        }
        MarketplaceCmd::Set {
            name,
            assignment,
            json,
        } => {
            let (key, value) = assignment
                .split_once('=')
                .unwrap_or((assignment.as_str(), ""));
            marketplace_set(&home, None, &name, key, value, json)
        }
    }
}

async fn dispatch_plugin(action: PluginCmd, root: &RootState) -> i32 {
    let home = skill_home();
    let mut registry = rebuild_registry(&home, None).await;
    let active = root.active_workdir.as_deref();
    match action {
        PluginCmd::Install {
            id,
            scope,
            version,
            json,
        } => {
            let project_path = if scope == "project" || scope == "local" {
                active.and_then(Path::to_str)
            } else {
                None
            };
            let _spinner = clone_spinner("Installing plugin...", !json);
            plugin_install(
                &mut registry,
                &home,
                None,
                &id,
                PluginInstallOptions {
                    scope: &scope,
                    project_path,
                    version: version.as_deref(),
                    hooks: None, // ledger-only（非交互无 live Computer）。
                    json_output: json,
                },
            )
            .await
        }
        PluginCmd::Uninstall {
            id,
            keep_servers,
            json,
        } => plugin_uninstall(&mut registry, &home, None, &id, keep_servers, None, json).await,
        PluginCmd::Enable { id, json } => {
            plugin_enable(&mut registry, &home, None, &id, None, json).await
        }
        PluginCmd::Disable { id, json } => {
            plugin_disable(&mut registry, &home, None, &id, None, json).await
        }
        PluginCmd::List { available, json } => plugin_list(
            &home,
            None,
            &root.registered_workdirs,
            active,
            available,
            json,
        ),
        PluginCmd::Info { id, json } => {
            plugin_info(&home, None, &id, &root.registered_workdirs, active, json)
        }
        PluginCmd::Gc { json } => {
            plugin_gc(
                &mut registry,
                &home,
                None,
                &root.registered_workdirs,
                active,
                None,
                None,
                json,
            )
            .await
        }
    }
}

fn dispatch_settings(action: SettingsCmd, root: &RootState) -> i32 {
    let active = root.active_workdir.as_deref();
    let flag = root.flag_path.as_deref();
    match action {
        SettingsCmd::Show { scope, json } => settings_show(
            None,
            scope.as_deref().unwrap_or("merged"),
            &root.registered_workdirs,
            active,
            flag,
            json,
        ),
        SettingsCmd::Get { key, scope, json } => settings_get(
            None,
            &key,
            scope.as_deref().unwrap_or("merged"),
            &root.registered_workdirs,
            active,
            flag,
            json,
        ),
        SettingsCmd::Set {
            key,
            value,
            scope,
            json,
        } => settings_set(
            None,
            &key,
            &value,
            scope.as_deref().unwrap_or("user"),
            active,
            json,
        ),
        SettingsCmd::Edit { scope, json } => {
            settings_edit(None, scope.as_deref().unwrap_or("user"), active, None, json)
        }
    }
}

async fn dispatch_skill(action: SkillCmd) -> i32 {
    let home = skill_home();
    let registry = rebuild_registry(&home, None).await;
    match action {
        SkillCmd::List { source, json } => skill_list(&registry, source.as_deref(), json),
        SkillCmd::Info { name, json } => skill_info(&registry, &name, json),
    }
}

// ── REPL 运行模式 / REPL run mode ─────────────────────────────────────────────
async fn run_repl(args: &Args, root: &RootState, config: Option<PathBuf>, inputs: Option<PathBuf>) {
    let session = SilentSession::new("cli-session");
    let mut computer = Computer::new(
        "friday_hands",
        session,
        None,
        None,
        args.auto_connect,
        args.auto_reconnect,
    );
    if !root.registered_workdirs.is_empty() {
        computer = computer.with_registered_workdirs(root.registered_workdirs.clone());
    }

    let cli_config = commands::CliConfig {
        url: args.url.clone(),
        namespace: args.namespace.clone(),
        auth: args.auth.clone(),
        headers: args.headers.clone(),
    };

    // 启动前缀（inputs/config 加载 → boot_up 子系统装配 → socket.io 连接）抽出为可测 helper。
    let handler = prepare_handler(computer, cli_config, config, inputs).await;

    // 启动 banner（版本 / 协议版本 / home）。
    let home = handler.computer.skill_home();
    let server_count = handler.computer.list_mcp_servers().await.len();
    banner::render_banner(
        server_count,
        &home,
        None,
        env!("CARGO_PKG_VERSION"),
        smcp::PROTOCOL_VERSION,
    );

    // 启动期 MCP 批准框（--approve-all-mcp / 逐项 y-a-n）。
    approval::run_mcp_approval(
        &handler.computer,
        root.approve_all_mcp,
        root.flag_path.as_deref(),
    )
    .await;

    // 进入交互模式。
    if let Err(e) = interactive::run_interactive_loop(handler, root.flag_path.clone()).await {
        eprintln!("交互模式错误: {e}");
    }
}

/// 装配 REPL handler：加载 inputs/config → `boot_up`（SKILL/blob/watcher 子系统）→ 连接 Socket.IO。
///
/// 从 [`run_repl`] 抽出的启动前缀，使「`run` 模式必须 boot SKILL/blob 子系统」可被回归测试覆盖
/// （`run_repl` 末尾进交互循环、不可直接测）。各步失败均**非阻塞**告警，与 Python `a2c-computer run`
/// 的非阻塞隔离策略一致（修 #83：此前 `run` 全程不调 `boot_up`，致 `get_skills` 恒空 / `get_skill`
/// 恒 4014 / blob mint `NotBooted`）。
async fn prepare_handler(
    computer: Computer<SilentSession>,
    cli_config: commands::CliConfig,
    config: Option<PathBuf>,
    inputs: Option<PathBuf>,
) -> CommandHandler {
    let url = cli_config.url.clone();
    let namespace = cli_config.namespace.clone();
    let auth = cli_config.auth.clone();
    let headers = cli_config.headers.clone();
    let mut handler = CommandHandler::new(computer, cli_config);

    if let Some(inputs_path) = inputs {
        if let Err(e) = handler.load_inputs(&inputs_path).await {
            eprintln!("加载 inputs 失败: {e}");
        }
    }
    if let Some(config_path) = config {
        if let Err(e) = handler.load_config(&config_path).await {
            eprintln!("加载 config 失败: {e}");
        }
    }

    // #83 修复：装配 SKILL/blob/watcher 子系统（对齐 Python `a2c-computer run`）。
    // 必须在 connect_socketio 之前完成——Computer 一旦在线，Agent 可立即 get_skills/get_skill/mint blob。
    if let Err(e) = handler.computer.boot_up().await {
        eprintln!("初始化 SKILL/blob 子系统失败: {e}");
    }

    if let Some(url) = &url {
        if let Err(e) = handler
            .connect_socketio(url, &namespace, &auth, &headers)
            .await
        {
            eprintln!("连接 SocketIO 失败: {e}");
        }
    }

    handler
}

#[cfg(test)]
mod tests {
    use super::*;
    use commands::{EXIT_OK, EXIT_USER_ERROR};
    use tempfile::tempdir;

    /// #83 回归：`run` 模式的启动前缀（`prepare_handler`）必须 `boot_up` SKILL 子系统。
    /// 修复前 `run_repl` 全程不调 `boot_up`，user 源 SKILL 永不被发现（`get_skills` 恒空）。
    #[tokio::test]
    async fn prepare_handler_boots_skill_subsystem() {
        let tmp = tempdir().unwrap();
        let home = tmp.path().join("home");
        // seed 一个 user 源 SKILL（home/user/<pkg>/SKILL.md，带 frontmatter）。
        let pkg = home.join("user").join("my-helper");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("SKILL.md"), "---\ndescription: helps\n---\nbody").unwrap();

        let computer = Computer::new("c", SilentSession::new("s"), None, None, false, false)
            .with_skill_home(home.clone())
            .with_blob_cache_root(tmp.path().join("blob"));
        // 前置：未 boot 时 SKILL 子系统未装配，get_skills 为空。
        assert_eq!(computer.get_skills().await.len(), 0);

        let cli_config = commands::CliConfig {
            url: None, // 不连接 Socket.IO，仅验证 boot 装配。
            namespace: "/smcp".to_string(),
            auth: None,
            headers: None,
        };
        let handler = prepare_handler(computer, cli_config, None, None).await;

        // 修复后：prepare_handler 内 boot_up 已发现 user 源 SKILL。
        assert_eq!(handler.computer.get_skills().await.len(), 1);
        handler.computer.shutdown().await.unwrap();
    }

    #[test]
    fn resolve_workdirs_absolutize_dedup_order() {
        // 去重保序 + active=first。
        let (registered, active) = resolve_workdirs(&[
            PathBuf::from("/a/b"),
            PathBuf::from("/a/b"),
            PathBuf::from("/c"),
        ]);
        assert_eq!(registered, vec![PathBuf::from("/a/b"), PathBuf::from("/c")]);
        assert_eq!(active, Some(PathBuf::from("/a/b")));
        // 相对路径 → 绝对化（不要求存在）。
        let (rel, rel_active) = resolve_workdirs(&[PathBuf::from("relative/dir")]);
        assert!(rel[0].is_absolute());
        assert_eq!(rel_active, Some(rel[0].clone()));
        // 空 → 无 active。
        let (empty, none) = resolve_workdirs(&[]);
        assert!(empty.is_empty());
        assert!(none.is_none());
    }

    #[test]
    fn settings_flag_scope_propagates_through_root_state() {
        // #97 回归：RootState.flag_path 经 dispatch_settings 透传给 settings handler。
        let tmp = tempdir().unwrap();
        let flag = tmp.path().join("flag.json");
        std::fs::write(&flag, r#"{"trustedMarketplaces":["from-flag"]}"#).unwrap();

        // 有 flag_path → get flag-scope key 命中 → EXIT_OK（证明透传生效）。
        let root = RootState {
            flag_path: Some(flag.clone()),
            ..Default::default()
        };
        let code = dispatch_settings(
            SettingsCmd::Get {
                key: "trustedMarketplaces".to_string(),
                scope: Some("flag".to_string()),
                json: true,
            },
            &root,
        );
        assert_eq!(code, EXIT_OK);

        // 无 flag_path → 同 key 在空 flag scope 缺失 → EXIT_USER_ERROR（修复前此分支恒走）。
        let code = dispatch_settings(
            SettingsCmd::Get {
                key: "trustedMarketplaces".to_string(),
                scope: Some("flag".to_string()),
                json: true,
            },
            &RootState::default(),
        );
        assert_eq!(code, EXIT_USER_ERROR);
    }

    #[test]
    fn add_dir_propagates_active_workdir_for_project_scope() {
        // #97 回归：--add-dir 解析的 active_workdir 经 RootState 透传 → project scope 可写。
        let tmp = tempdir().unwrap();
        let root = RootState {
            registered_workdirs: vec![tmp.path().to_path_buf()],
            active_workdir: Some(tmp.path().to_path_buf()),
            ..Default::default()
        };
        let code = dispatch_settings(
            SettingsCmd::Set {
                key: "foo".to_string(),
                value: "1".to_string(),
                scope: Some("project".to_string()),
                json: true,
            },
            &root,
        );
        assert_eq!(code, EXIT_OK);

        // 无 active_workdir → project scope 缺 active workdir → EXIT_USER_ERROR。
        let code = dispatch_settings(
            SettingsCmd::Set {
                key: "foo".to_string(),
                value: "1".to_string(),
                scope: Some("project".to_string()),
                json: true,
            },
            &RootState::default(),
        );
        assert_eq!(code, EXIT_USER_ERROR);
    }
}
