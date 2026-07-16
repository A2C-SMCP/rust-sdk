/*!
* 文件名: approval.rs
* 作者: JQQ
* 创建日期: 2026/06/10
* 最后修改日期: 2026/06/10
* 版权: 2023 JQQ. All rights reserved.
* 依赖: serde_json, console
* 描述: 启动期 MCP 批准框 / boot-time MCP approval box.
*
* 对标 Python `a2c_smcp/computer/cli/commands/plugin.py::run_mcp_approval`：启动期解析 `.tfrobot/mcp.json`
* 定义层 + 套门控 + 挂载 ENABLED server。
* - user-flag-policy origin → 门控判 ENABLED → 直挂（免批准框）；
* - DISABLED（企业拒绝/不在白名单/显式 disabled）→ 跳过；
* - PENDING（工作区共享未决）→ TTY 弹 y/a/n 写 local scope；非 TTY → skip+WARN，`--approve-all-mcp` 全批（仅本次不落盘）。
*
* #131（P0 授权门绕过）：本路径**不再**读 bundled 名集。此前 `bundled_mcp_server_names` 的账本名集喂门控档④，
* 令任何 project/local 声明只要**显示名**撞上账本里任一插件的 bundled 名即免批准框直挂——而真 bundled server
* 走 enable→mount、**从不进** `resolve_mcp_config` ⇒ 该档唯一可达路径 100% 是借名绕过。plugin 声明依赖的
* server MUST 不进入本门迭代（协议 `runtime-contract.md` §5 item 10 + `guides/mcp-approval-gate-alignment.md` §2）。
*
* flag 层 schema 区分（fix-review #1）：`flag_config` 是 **settings.json** flag 层（喂 `resolved_settings` 的
* `flag_path`），**不是 mcp.json**，故不喂 `resolve_mcp_config(flag_config_path=)`。
*/

use std::io::Write;
use std::path::Path;

use serde_json::Value;

use super::commands::{
    format_settings_errors, msg_dim, msg_err, msg_ok, msg_warn, resolved_settings_with_errors,
};
use crate::computer::{Computer, Session};
use crate::mcp_clients::model::MCPServerConfig;
use crate::settings::mcp_config::{
    approve_all_project_mcp, approve_mcp_server, deny_mcp_server, gate_mcp_servers,
    resolve_mcp_config, McpApprovalStatus, ResolveMcpConfigArgs, ResolvedMcpServer,
};

/// 合并 resolved server 的 `config`（含占位符）+ `ext`（剥离的 envFile 等）为挂载用 config。
///
/// ★ 必须合回 `ext`，否则 spawn 时看不到 `envFile` → 静默丢失（对标 Python `_mount_dict`，#69 Group B 风险 2）。
fn merge_mount_config(srv: &ResolvedMcpServer) -> MCPServerConfig {
    let mut value = serde_json::to_value(&srv.config).unwrap_or(Value::Null);
    if let Value::Object(map) = &mut value {
        for (key, val) in &srv.ext {
            map.insert(key.clone(), val.clone());
        }
    }
    match serde_json::from_value(value) {
        Ok(config) => config,
        // 回退到剥离了 ext 的 config——若有 ext（envFile 等）告警，别让其无声丢失（对标 _mount_dict 永不丢 ext）。
        Err(e) => {
            if !srv.ext.is_empty() {
                msg_warn(&format!(
                    "⚠ MCP server {:?}: ext merge failed ({e}); envFile/ext fields not applied",
                    srv.name
                ));
            }
            srv.config.clone()
        }
    }
}

/// 同步读一行（启动期批准框；REPL 主循环尚未起）/ blocking single-line prompt at boot。
fn prompt_line(prompt: &str) -> String {
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
    line.trim().to_lowercase()
}

async fn mount<S: Session>(comp: &Computer<S>, srv: &ResolvedMcpServer) {
    let name = srv.name.clone();
    // #113 S6：boot 批准挂载读的是**已在盘**的 `.tfrobot/mcp.json` 定义 → 走**运行期挂载**（不回写落盘，
    // 否则重复写用户已声明的 server、可能 scope 漂移）。用户新增走 `Computer::add_or_update_server`。
    match comp.mount_server(merge_mount_config(srv)).await {
        Ok(()) => msg_ok(&format!("mounted MCP server {name:?}")),
        // 单个 server 挂载失败不阻断其余。
        Err(e) => msg_err(&format!("failed to mount MCP server {name:?}: {e}")),
    }
}

/// 启动期解析 `.tfrobot/mcp.json` 定义层 + 批准门控 + 挂载 ENABLED server / boot-time MCP approval + mount。
pub async fn run_mcp_approval<S: Session>(
    comp: &Computer<S>,
    approve_all: bool,
    flag_config: Option<&Path>,
) {
    // #98：project/local scope 锚定进程 cwd（`Computer` 不再持有 workspace）。
    let cwd = std::env::current_dir().ok();
    // flag_config 是 settings.json（见模块文档）→ resolve_mcp_config 不收（避免当 mcp.json 误读）。
    let resolved = resolve_mcp_config(ResolveMcpConfigArgs {
        cwd: cwd.as_deref(),
        env: None,
        flag_config_path: None,
        managed_mcp_path: None,
        platform: None,
    });

    // 被 drop 的畸形 server/input 必须呈现（mcp_config 容错不静默）。
    for err in &resolved.errors {
        msg_warn(&format!("⚠ mcp.json: {err:?}"));
    }
    if resolved.servers.is_empty() {
        return;
    }

    // #143：settings 的校验错误必须**呈现**——scope 越权（policy-only / 审批门 enable 方向判据）会**静默
    // 丢弃字段**，若连错误也吞掉，用户只会看到「我的 settings 莫名不生效」。协议指南 §2.1/§3：响亮失败。
    let resolved_st = resolved_settings_with_errors(cwd.as_deref(), None, flag_config);
    for line in format_settings_errors(&resolved_st.errors) {
        msg_warn(&line);
    }
    let settings = resolved_st.settings;
    let statuses = gate_mcp_servers(&resolved, &settings);

    // mcp.json 定义的 input 入池（无前缀），供 server config 的裸 `${input:}` 解析。
    for inp in resolved.inputs.iter().cloned() {
        let _ = comp.add_or_update_input(inp).await;
    }

    let interactive = console::user_attended();
    let mut approved_all_session = approve_all;

    for (name, status) in &statuses {
        let Some(srv) = resolved.servers.get(name) else {
            continue;
        };
        match status {
            McpApprovalStatus::Enabled => mount(comp, srv).await,
            McpApprovalStatus::Disabled => {
                msg_dim(&format!(
                    "· MCP server {name:?} disabled (policy/denied), skipped"
                ));
            }
            McpApprovalStatus::Pending => {
                if approved_all_session {
                    mount(comp, srv).await;
                    continue;
                }
                if !interactive {
                    msg_warn(&format!(
                        "⚠ skipped pending MCP server {name:?} (no TTY); approve in REPL or pass --approve-all-mcp"
                    ));
                    continue;
                }
                let ans = prompt_line(&format!(
                    "⚠ Unapproved workspace MCP server '{name}' (origin={:?})\n  [y]es this server · [a]ll project servers · [n]o (deny): ",
                    srv.origin
                ));
                match ans.as_str() {
                    "a" | "all" => {
                        let _ = approve_all_project_mcp(cwd.as_deref());
                        approved_all_session = true;
                        mount(comp, srv).await;
                    }
                    "y" | "yes" => {
                        let _ = approve_mcp_server(name, cwd.as_deref());
                        mount(comp, srv).await;
                    }
                    _ => {
                        let _ = deny_mcp_server(name, cwd.as_deref());
                        msg_dim(&format!(
                            "· denied MCP server {name:?} (written to local scope)"
                        ));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::SettingsScope;
    use serde_json::json;

    #[test]
    fn merge_mount_config_reattaches_envfile() {
        // resolver 把 envFile 剥进 ext；挂载前须经 merge 回挂到 typed config（对标 Python _mount_dict）。
        let config: MCPServerConfig = serde_json::from_value(json!({
            "type": "stdio",
            "name": "demo",
            "server_parameters": {"command": "echo"}
        }))
        .unwrap();
        assert_eq!(config.env_file(), None);

        let srv = ResolvedMcpServer {
            name: "demo".to_string(),
            config,
            ext: serde_json::Map::from_iter([("envFile".to_string(), json!(".env"))]),
            origin: SettingsScope::User,
            trusted_origin: true,
        };
        // merge 后 config.env_file() 回挂为 ".env"（否则 spawn 静默丢失）。
        assert_eq!(merge_mount_config(&srv).env_file(), Some(".env"));
    }
}
