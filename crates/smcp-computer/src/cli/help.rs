/*!
* 文件名: help.rs
* 作者: JQQ
* 创建日期: 2026/06/10
* 最后修改日期: 2026/06/10
* 版权: 2023 JQQ. All rights reserved.
* 依赖: console
* 描述: 分组 help 渲染 + 命令分类法（REPL help / completer 共用单一事实源）/ grouped help + command taxonomy.
*
* 对标 Python `a2c_smcp/computer/cli/help.py`：`render_help` 默认列 namespace（折叠），`help <ns>` 列该
* namespace 命令。分类法（[`NAMESPACES`] / [`namespace_commands`] / [`subcommands`] / [`flags_for`] / [`ROOT_WORDS`]）
* 是 REPL help 与 [`super::completer`] 的单一事实源。
*/

use console::style;

/// namespace → 一行描述（折叠 help 用，顺序即展示序）/ namespace → one-line description (ordered)。
pub const NAMESPACES: &[(&str, &str)] = &[
    ("server", "MCP server lifecycle (add / rm / start / stop)"),
    ("inputs", "Input definitions and values"),
    ("marketplace", "SKILL marketplaces (git sources)"),
    (
        "plugin",
        "Plugins — skill+mcp bundles (install / enable / list ...)",
    ),
    ("skill", "Skills cross-source query (list / info)"),
    ("socket", "Socket.IO connection control"),
    ("notify", "Send notifications to Agent"),
    (
        "settings",
        "settings.json intent layer (show / get / set / edit)",
    ),
    (
        "utility",
        "status / tools / mcp / desktop / render / tc / history",
    ),
];

/// 行首可补全词：namespace + 顶层命令 / root completions: namespaces + top-level commands。
pub const ROOT_WORDS: &[&str] = &[
    "server",
    "inputs",
    "marketplace",
    "plugin",
    "skill",
    "socket",
    "notify",
    "settings",
    "start",
    "stop",
    "status",
    "tools",
    "mcp",
    "desktop",
    "render",
    "tc",
    "history",
    "help",
    "quit",
    "exit",
];

/// 已知 settings.json 顶层字段（与 schema FIELD_* 对齐；completer 静态名集）/ known settings keys for completion。
pub const SETTINGS_KEYS: &[&str] = &[
    "extraKnownMarketplaces",
    "enabledPlugins",
    "strictKnownMarketplaces",
    "trustedMarketplaces",
    "blockedMarketplaces",
    "enableAllProjectMcpServers",
    "enabledMcpjsonServers",
    "disabledMcpjsonServers",
    "allowedMcpServers",
    "deniedMcpServers",
    "permissions",
];

/// namespace → 子命令词（completer 用）/ subcommand words per namespace。
pub fn subcommands(ns: &str) -> &'static [&'static str] {
    match ns {
        "server" => &["add", "rm", "remove"],
        "inputs" => &[
            "load", "add", "update", "rm", "remove", "get", "list", "value",
        ],
        "marketplace" => &["add", "list", "info", "remove", "refresh", "set"],
        "plugin" => &[
            "install",
            "uninstall",
            "enable",
            "disable",
            "list",
            "info",
            "gc",
        ],
        "skill" => &["list", "info"],
        "socket" => &["connect", "join", "leave"],
        "notify" => &["update"],
        "settings" => &["show", "edit", "get", "set"],
        _ => &[],
    }
}

/// (namespace, subcommand) → flag 集（completer 用）/ flags per command。
pub fn flags_for(ns: &str, sub: &str) -> &'static [&'static str] {
    match (ns, sub) {
        ("marketplace", "add") => &["--name", "--trust", "--auto-update", "--no-clone", "--json"],
        ("marketplace", "list" | "info" | "refresh" | "set") => &["--json"],
        ("marketplace", "remove") => &["--keep-plugins", "--json"],
        ("skill", "list") => &["--source", "--json"],
        ("skill", "info") => &["--json"],
        ("plugin", "install") => &["--version", "--scope", "--json"],
        ("plugin", "uninstall") => &["--keep-servers", "--json"],
        ("plugin", "enable" | "disable" | "info" | "gc") => &["--json"],
        ("plugin", "list") => &["--available", "--json"],
        ("settings", "show" | "get" | "set" | "edit") => &["--scope", "--json"],
        _ => &[],
    }
}

/// namespace → 完整 help 行 `(命令样式, 描述)` / full help rows per namespace。
fn namespace_commands(ns: &str) -> &'static [(&'static str, &'static str)] {
    match ns {
        "server" => &[
            (
                "server add <json|@file>",
                "添加或更新 MCP 配置 / add or update config",
            ),
            (
                "server rm <bundle_id>",
                "移除 MCP 配置（按 bundle_id，可经 status 查看）/ remove config (by bundle_id)",
            ),
            ("start <name>|all", "启动客户端 / start client(s)"),
            ("stop <name>|all", "停止客户端 / stop client(s)"),
        ],
        "inputs" => &[
            (
                "inputs load <@file>",
                "从文件加载 inputs 定义 / load inputs",
            ),
            ("inputs list", "查看当前 inputs 定义 / show inputs"),
            (
                "inputs value list|get|set|rm|clear",
                "inputs 缓存值增删改查 / CRUD cached input values",
            ),
        ],
        "marketplace" => &[
            (
                "marketplace add <git-url> [--name N] [--trust] [--auto-update] [--no-clone]",
                "添加 marketplace（首次 y/N trust）/ add",
            ),
            ("marketplace list [--json]", "列出已知 marketplace / list"),
            (
                "marketplace info <name> [--json]",
                "marketplace 详情 / detail",
            ),
            (
                "marketplace remove <name> [--keep-plugins]",
                "移除（默认级联卸载 plugin）/ remove (cascade)",
            ),
            (
                "marketplace refresh [<name>|all]",
                "git pull / 重 clone + 对账 / refresh",
            ),
            (
                "marketplace set <name> auto-update=<bool>",
                "设置 per-source auto-update / set flag",
            ),
        ],
        "plugin" => &[
            (
                "plugin install <plugin>@<mp> [--version V] [--scope S]",
                "安装 plugin（外来 MCP 同名硬抛）/ install (name conflict aborts)",
            ),
            (
                "plugin uninstall <plugin>@<mp> [--keep-servers]",
                "卸载 plugin / uninstall",
            ),
            (
                "plugin enable|disable <plugin>@<mp>",
                "启用 / 禁用（整 plugin 上/下线）/ enable / disable",
            ),
            (
                "plugin list [--available] [--json]",
                "列出 installed plugin / list",
            ),
            ("plugin info <plugin>@<mp> [--json]", "plugin 详情 / detail"),
            ("plugin gc", "清理孤儿 plugin / gc orphan plugins"),
        ],
        "skill" => &[
            (
                "skill list [--source mp|mcp|user] [--json]",
                "跨源列出可见 SKILL / list skills",
            ),
            ("skill info <name> [--json]", "SKILL 详情 / skill detail"),
        ],
        "socket" => &[
            ("socket connect [<url>]", "连接 Socket.IO / connect"),
            (
                "socket join <office_id> <computer_name>",
                "加入房间 / join office",
            ),
            ("socket leave", "离开房间 / leave office"),
        ],
        "notify" => &[(
            "notify update",
            "触发配置更新通知 / emit config updated notification",
        )],
        "settings" => &[
            (
                "settings show [--scope user|project|local|flag|policy|merged]",
                "展示某 scope（默认 merged）/ show a scope",
            ),
            (
                "settings get <key> [--scope ...]",
                "读取单字段 / read a field",
            ),
            (
                "settings set <key> <value> [--scope user|project|local]",
                "写单字段（flag/policy 只读）/ set a field",
            ),
            (
                "settings edit [--scope user|project|local]",
                "$EDITOR 编辑 + 保存后 reconcile / edit then reconcile",
            ),
        ],
        "utility" => &[
            ("status", "查看服务器状态 / show server status"),
            ("tools", "列出可用工具 / list tools"),
            ("mcp", "显示当前 MCP 配置 / show current MCP config"),
            (
                "desktop [size] [window_uri]",
                "获取当前桌面窗口组合 / get desktop",
            ),
            (
                "render <json|@file>",
                "测试渲染（占位符解析）/ test rendering",
            ),
            ("tc <json|@file>", "调试工具调用 / debug tool call"),
            ("history [n]", "最近工具调用历史 / recent tool call history"),
            ("help [<namespace>]", "查看命令 / show commands"),
            ("quit | exit", "退出 / quit"),
        ],
        _ => &[],
    }
}

/// 折叠 namespace 列表（`namespace=None`）或某 namespace 命令详情 / render grouped help。
pub fn render_help(namespace: Option<&str>) {
    let Some(ns) = namespace else {
        println!(
            "{}",
            style("Namespaces (type 'help <name>' for details):").bold()
        );
        for (name, desc) in NAMESPACES {
            println!("  {:<14}{desc}", style(name).cyan());
        }
        println!(
            "{}",
            style("提示: help <namespace> 查看该组命令；? 重看本表。").dim()
        );
        return;
    };
    let ns = ns.to_lowercase();
    let rows = namespace_commands(&ns);
    if rows.is_empty() {
        println!(
            "{}",
            style(format!("unknown namespace: {ns:?} (try 'help')")).yellow()
        );
        return;
    }
    println!("{}", style(format!("{ns} commands:")).bold());
    for (cmd, desc) in rows {
        println!("  {}", style(cmd).cyan());
        println!("      {desc}");
    }
}
