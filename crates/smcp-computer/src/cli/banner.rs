/*!
* 文件名: banner.rs
* 作者: JQQ
* 创建日期: 2026/06/10
* 最后修改日期: 2026/06/10
* 版权: 2023 JQQ. All rights reserved.
* 依赖: console
* 描述: REPL 启动 banner（zero-state 引导 + 版本/协议/home）/ REPL startup banner.
*
* 对标 Python `a2c_smcp/computer/cli/banner.py`：触发条件 `installed plugins == 0 AND servers == 0`（marketplace
* 数量不计入）→ 引导框；否则一行摘要。Rust 版按 issue #54 在摘要/引导中并展示版本、协议版本、SKILL home 路径。
*/

use std::path::Path;

use console::style;

use crate::settings::scope::EnvMap;
use crate::settings::store::load_installed_plugins;

/// installed plugin 数（读失败不打断启动 → 0）/ installed-plugin count (never break boot)。
fn installed_plugin_count(home: &Path, env: Option<&EnvMap>) -> usize {
    load_installed_plugins(Some(home), env)
        .account
        .plugins
        .len()
}

/// zero-state（0 plugins AND 0 servers）→ 引导框；否则一行摘要 / zero-state box or one-line summary。
pub fn render_banner(
    server_count: usize,
    home: &Path,
    env: Option<&EnvMap>,
    version: &str,
    protocol_version: &str,
) {
    let plugins = installed_plugin_count(home, env);
    let coords = format!(
        "v{version} · proto {protocol_version} · home {}",
        home.display()
    );

    if plugins == 0 && server_count == 0 {
        println!(
            "{}",
            style("A2C Computer — ready (0 plugins, 0 servers)").bold()
        );
        println!("{}", style(&coords).dim());
        println!();
        println!("{}", style("Next steps:").bold());
        println!(
            "  • {}   add a SKILL marketplace",
            style("marketplace add <git-url>").cyan()
        );
        println!(
            "  • {}          add an MCP server",
            style("server add @<file>").cyan()
        );
        println!(
            "  • {}                        full command list",
            style("help").cyan()
        );
        return;
    }
    println!(
        "{}  ·  {plugins} plugins  ·  {server_count} servers  ·  {}",
        style("A2C Computer").bold(),
        style(&coords).dim()
    );
}
