/*!
* 文件名: commands.rs
* 作者: JQQ
* 创建日期: 2025/12/16
* 最后修改日期: 2025/12/16
* 版权: 2023 JQQ. All rights reserved.
* 依赖: console, serde_json
* 描述: CLI命令处理器 / CLI command handlers
*/

use crate::computer::{Computer, ConnectOptions, SilentSession};
use crate::errors::ComputerError;
use crate::inventory::{McpOwnership, McpServerWithMetadata};
use crate::mcp_clients::model::{BundleId, MCPServerConfig, MCPServerInput};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use thiserror::Error;

/// CLI 运行时配置 / CLI runtime configuration
#[derive(Clone, Debug)]
pub struct CliConfig {
    pub url: Option<String>,
    pub namespace: String,
    pub auth: Option<String>,
    pub headers: Option<String>,
}

#[derive(Error, Debug)]
pub enum CommandError {
    #[error("Invalid command: {0}")]
    InvalidCommand(String),
    #[error("Parse error: {0}")]
    ParseError(String),
    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    JsonError(#[from] serde_json::Error),
    #[error("Computer error: {0}")]
    ComputerError(#[from] ComputerError),
}

pub struct CommandHandler {
    pub computer: Computer<SilentSession>,
    pub cli_config: CliConfig,
}

impl CommandHandler {
    pub fn new(computer: Computer<SilentSession>, cli_config: CliConfig) -> Self {
        Self {
            computer,
            cli_config,
        }
    }

    /// 显示帮助信息
    pub fn show_help(&self) {
        println!("可用命令 / Commands:");
        println!();
        println!("  status                    查看服务器状态 / show server status");
        println!("  tools                     列出可用工具 / list tools");
        println!("  mcp                       显示当前 MCP 配置 / show current MCP config");
        println!("  server add <json|@file>   添加或更新 MCP 配置 / add or update config");
        println!("  server rm <target>        移除 MCP 配置（target = 唯一 name 或 bundle_id）/ remove config");
        println!(
            "  start <target>|all        启动客户端（target = name 或 bundle_id）/ start client(s)"
        );
        println!(
            "  stop <target>|all         停止客户端（target = name 或 bundle_id）/ stop client(s)"
        );
        println!(
            "  restart <target>          重启客户端（target = name 或 bundle_id）/ restart client"
        );
        println!("  inputs load <@file>       从文件加载 inputs 定义 / load inputs");
        println!("  inputs add <json|@file>   添加 input 定义 / add input definition");
        println!("  inputs update <json|@file> 更新 input 定义 / update input definition");
        println!("  inputs rm <id>            移除 input 定义 / remove input definition");
        println!("  inputs get <id>           获取 input 定义 / get input definition");
        println!("  inputs list               查看当前inputs的定义 / show inputs");
        println!("  inputs value list         列出当前 inputs 的缓存值 / list current cached input values");
        println!("  inputs value get <id>     获取指定 id 的值 / get cached value by id");
        println!("  inputs value set <id>     设置指定 id 的值 / set cached value");
        println!("  inputs value rm <id>      删除指定 id 的值 / remove cached value");
        println!("  inputs value clear        清空全部缓存 / clear all cached values");
        println!("  tc <json|@file>           使用与 Socket.IO 一致的 JSON 结构调试工具");
        println!("  desktop [size] [uri]      获取当前桌面窗口组合 / get current desktop");
        println!("  history [n]               显示最近的工具调用历史 / show recent history");
        println!("  socket connect [url]      连接 Socket.IO / connect to Socket.IO");
        println!("  socket join <office> <name>  加入房间 / join office");
        println!("  socket leave              离开房间 / leave office");
        println!("  notify update             触发配置更新通知 / emit config updated");
        println!("  render <json|@file>       测试渲染（占位符解析）");
        println!("  quit | exit               退出 / quit");
    }

    /// 显示服务器状态
    pub async fn show_status(&self) -> Result<(), CommandError> {
        println!("服务器状态 / Server Status:");

        // 获取 Socket.IO 状态
        let socketio_client = self.computer.get_socketio_client();
        let socketio_ref = socketio_client.read().await;
        if let Some(ref client) = *socketio_ref {
            println!("  Socket.IO: 已连接 / Connected");
            println!("    URL: {}", client.get_url());
            println!("    Namespace: {}", client.get_namespace());
            if let Some(office_id) = client.get_office_id().await {
                println!("    Office ID: {}", office_id);
                println!("    Computer Name: {}", self.computer.name());
            } else {
                println!("    Office: 未加入 / Not joined");
            }
        } else {
            println!("  Socket.IO: 未连接 / Not connected");
        }

        // 获取 MCP Manager 状态
        if self.computer.is_mcp_manager_initialized().await {
            // 获取服务器状态列表
            let server_status = self.computer.get_server_status().await;
            let active_count = server_status
                .iter()
                .filter(|(_, _, active, _)| *active)
                .count();

            println!("  MCP Manager: 已初始化 / Initialized");
            println!("  Active Servers: {}", active_count);

            // #121 B：一并展示 bundle_id（软件唯一身份）——`server rm` 按 bundle_id 寻址。
            // #127：`bundle_id` 随 `get_server_status` 每行同源直出，**不再**按 name join 另一张映射——
            // 那张映射是 name-keyed 的，同名 server 会折叠，导致两行打印同一个 bundle_id、用户按提示
            // `server rm <bundle_id>` 删错对象且另一条从 CLI 完全无法寻址。
            for (bundle_id, name, active, state) in server_status {
                let status = if active {
                    "运行中 / Running"
                } else {
                    "已停止 / Stopped"
                };
                println!("    - {name} [bundle_id={bundle_id}]: {status} ({state})");
            }

            // 获取可用工具数量
            match self.computer.get_available_tools().await {
                Ok(tools) => println!("  Available Tools: {}", tools.len()),
                Err(_) => println!("  Available Tools: 获取失败 / Failed to get"),
            }
        } else {
            println!("  MCP Manager: 未初始化 / Not initialized");
            println!("  Active Servers: 0");
            println!("  Available Tools: 0");
        }

        // #165 Option B：pending bundled server（project-origin 激活、待批准）/ pending bundled approvals.
        let pending = self.computer.list_pending_bundled_approvals();
        if !pending.is_empty() {
            println!(
                "  Pending Bundled (project-origin, awaiting approval): {}",
                pending.len()
            );
            for rec in &pending {
                let bid = crate::mcp_clients::bundle_id::resolve_bundle_id(&rec.config);
                println!(
                    "    - {} [bundle_id={}] (plugin {}) — approve: settings.local.json enabledPlugins[\"{}\"]=true",
                    rec.config.name(),
                    bid.as_str(),
                    rec.plugin_id,
                    rec.plugin_id
                );
            }
        }

        Ok(())
    }

    /// 列出可用工具
    pub async fn list_tools(&self) -> Result<(), CommandError> {
        if !self.computer.is_mcp_manager_initialized().await {
            println!("MCP 管理器未初始化 / MCP manager not initialized");
            println!("请先添加并启动 MCP server，然后再执行 tools / Please add and start an MCP server before running 'tools'");
            println!();
            println!("示例 / Example:");
            println!("  server add @./config.json");
            println!("  start all");
            return Ok(());
        }

        match self.computer.get_available_tools().await {
            Ok(tools) => {
                println!("可用工具 / Available Tools:");
                for tool in tools {
                    println!("  - {}", tool.name);
                }
            }
            Err(e) => {
                return Err(CommandError::ComputerError(e));
            }
        }
        Ok(())
    }

    /// 显示 MCP 配置
    pub async fn show_mcp_config(&self) -> Result<(), CommandError> {
        // 获取服务器配置
        let servers = self.computer.list_mcp_servers().await;

        // 获取 inputs
        let inputs = self.computer.list_inputs().await?;

        let config = json!({
            "servers": servers,
            "inputs": inputs
        });

        println!("当前 MCP 配置 / Current MCP Config:");
        println!("{}", serde_json::to_string_pretty(&config)?);

        Ok(())
    }

    /// 添加或更新服务器配置
    pub async fn add_server(&mut self, config_str: &str) -> Result<(), CommandError> {
        let config: Value = if let Some(path) = config_str.strip_prefix('@') {
            let content = std::fs::read_to_string(path)?;
            serde_json::from_str(&content)?
        } else {
            serde_json::from_str(config_str)?
        };

        // 将 JSON 转换为 MCPServerConfig
        let server_config: MCPServerConfig = serde_json::from_value(config)?;

        // 添加或更新服务器配置
        self.computer.add_or_update_server(server_config).await?;

        println!("✅ 服务器配置已添加/更新 / Server config added/updated");

        Ok(())
    }

    #[cfg(test)]
    pub async fn add_server_debug(&mut self, config_str: &str) -> Result<(), CommandError> {
        let config: Value = if let Some(path) = config_str.strip_prefix('@') {
            let content = std::fs::read_to_string(path)?;
            serde_json::from_str(&content)?
        } else {
            serde_json::from_str(config_str)?
        };

        // 将 JSON 转换为 MCPServerConfig
        match serde_json::from_value::<MCPServerConfig>(config.clone()) {
            Ok(server_config) => {
                println!("JSON parsed successfully: {:?}", server_config);
                self.computer.add_or_update_server(server_config).await?;
                println!("✅ 服务器配置已添加/更新 / Server config added/updated");
            }
            Err(e) => {
                println!("JSON parse error: {:?}", e);
                println!("JSON was: {}", serde_json::to_string_pretty(&config)?);
                return Err(CommandError::JsonError(e));
            }
        }

        Ok(())
    }

    /// #141/R4 + #171/Candidate B：人机面 `token`（name 或 bundle_id）→ `BundleId` 解析（**只在人机面**，库层
    /// 永不 name 寻址）。
    ///
    /// **步骤序严格按协议 §5.1（`sdk-api-guidance.md` 行 127-145），与 python `cli/resolve.py` 逐行同构——
    /// 顺序有意义，勿重排**：
    ///
    /// 1. token 按 **display name** 反查，**唯一命中** → 其 bundle_id；
    /// 2. **多命中**，且 token 精确等于其中某候选的 bundle_id → 按该 bundle_id 执行（#171 Candidate B：
    ///    用户已显式表达身份意图，bundle_id 全局唯一，不构成真实二义性）；
    /// 3. **多命中**，且 token 不等于任何候选的 bundle_id → 报错并列出候选（bundle_id + name + 归属），
    ///    要求改用 bundle_id 重试（禁字典序最小：把不确定的错变成确定的错）；
    /// 4. **0 命中** ∧ token 是**合法且已注册**的 bundle_id → token 本身；
    /// 5. 其余 → 报错「未找到」。
    ///
    /// 🔴 订正记录（#141 复审发现，保留以警示未来维护者）：
    ///
    /// - **name 必须先于 bundle_id 查**。反过来会让 `server rm foo` 在「A(name=foo, id=foo_1) + B(name=bar,
    ///   id=foo)」时删掉 **B**——用户敲的是自己看得见的名字，回执里的 bundle_id 他分辨不出是别人的。
    /// - **语法合法 ≠ 存在**。放行未注册的合法 id 会让拼错的 token 一路走到底层幂等 no-op ⇒ 假成功复活
    ///   （协议步骤 5「未命中 MUST 报错，MUST NOT 静默成功」）。
    async fn resolve_target(&self, token: &str) -> Result<BundleId, CommandError> {
        let servers = self.candidates().await;
        let name_hits: Vec<&McpServerWithMetadata> =
            servers.iter().filter(|s| s.name == token).collect();

        // ① 唯一 name 命中 → 其 bundle_id。
        if let [one] = name_hits.as_slice() {
            return BundleId::try_from(one.bundle_id.clone()).map_err(|e| {
                CommandError::InvalidCommand(format!("invalid bundle_id {:?}: {e}", one.bundle_id))
            });
        }

        // ② 多命中：先精确 bundle_id 匹配（Candidate B / §5.1 步骤 2），再报错。
        if name_hits.len() > 1 {
            // token 精确等于某候选的 bundle_id → 执行用户显式表达的身份意图。
            if let Ok(id) = BundleId::try_from(token) {
                if name_hits.iter().any(|s| s.bundle_id == token) {
                    return Ok(id);
                }
            }
            // 仍不匹配 → 列候选报错（禁字典序最小：把不确定的错变成确定的错）。
            let candidates = name_hits
                .iter()
                .map(|s| {
                    let owner = match &s.managed_by {
                        McpOwnership::User => "user".to_string(),
                        McpOwnership::Plugin { plugin_id, .. } => format!("plugin:{plugin_id}"),
                    };
                    format!("   {} [bundle_id={}] ({owner})", s.name, s.bundle_id)
                })
                .collect::<Vec<_>>()
                .join("\n");
            return Err(CommandError::InvalidCommand(format!(
                "有 {} 个 server 叫 {token:?}:\n{candidates}\n请用 bundle_id 重试 / retry with bundle_id",
                name_hits.len()
            )));
        }

        // ③ 0 命中 ∧ 合法**且已注册**的 bundle_id → token 本身。
        if let Ok(id) = BundleId::try_from(token.to_string()) {
            if servers.iter().any(|s| s.bundle_id == token) {
                return Ok(id);
            }
        }

        // ④ 其余 → 未找到。
        Err(CommandError::InvalidCommand(format!(
            "未找到服务器 {token:?} / server not found（请经 status 核对 name 或 bundle_id）"
        )))
    }

    /// `resolve_target` 的查找空间：**运行期活跃集 ∪ 声明面**（python `collect_candidates`，#143 决策 1）。
    ///
    /// 🔴 只取运行期会漏掉「已落盘声明但未挂载」的 server（如待审批 pending 声明）：`Computer::remove_server`
    /// 读的是磁盘快照、**本来删得掉**，但 CLI 候选表看不见它 ⇒ 凡 display 名不是合法 bundle_id 字面量者
    /// （含 `.`、空格、中文…）**从 CLI 无路可删**。隔离复审已在真二进制上复现。
    async fn candidates(&self) -> Vec<McpServerWithMetadata> {
        // 运行期 + ledger 归属（携 `managed_by`，供歧义候选列表标注「谁的」）。
        let mut out = self.computer.list_mcp_servers_with_metadata().await;
        let known: std::collections::HashSet<String> =
            out.iter().map(|s| s.bundle_id.clone()).collect();
        // 声明面兜底：补上未挂载的已声明 server。归属按 origin 推导——`resolve` 的声明面结构性不含
        // origin==plugin（plugin bundled 走 transient 挂载、由上面的 ledger 源覆盖），故一律记 User。
        for cfg in self.computer.declared_mcp_servers() {
            let bundle_id = crate::mcp_clients::bundle_id::resolve_bundle_id(&cfg);
            if !known.contains(bundle_id.as_str()) {
                out.push(McpServerWithMetadata::new(
                    cfg.name(),
                    bundle_id.into_string(),
                    cfg.disabled(),
                    McpOwnership::User,
                ));
            }
        }
        out
    }

    /// 移除服务器配置（`bundle_id` 可经 `status` 查看；亦接受唯一 name）/ remove（#141：经 `resolve_target`）。
    ///
    /// **真实回执**：仅当确有声明被删或实例被停摘才报「已移除」；否则报「未做任何操作」。
    pub async fn remove_server(&mut self, target: &str) -> Result<(), CommandError> {
        let line = self.remove_server_line(target).await?;
        println!("{line}");
        Ok(())
    }

    /// [`remove_server`](Self::remove_server) 的**可断言内核**：返回回执行而非直接 `println!`。
    ///
    /// 🔴 存在的理由：`println!` 无法被单测检查，于是「回执是否诚实」——本 issue 的**交付物本身**——就没有
    /// 任何测试守得住。隔离复审实测：把回执硬编成成功态，920 条测试仍全绿。故把 sink 抽出来，让测试断言
    /// **真实调用链**的输出，而不是只断言纯函数映射。
    pub(crate) async fn remove_server_line(&self, target: &str) -> Result<String, CommandError> {
        let id = self.resolve_target(target).await?;
        let removed = self.computer.remove_server(&id).await?;
        Ok(remove_receipt(&id, removed))
    }

    /// 启动客户端（`<target>|all`，target = name 或 bundle_id）/ start（#141：经 `resolve_target`）。
    pub async fn start_client(&self, target: &str) -> Result<(), CommandError> {
        if target == "all" {
            return match self.computer.start_all_mcp_clients().await {
                Ok(()) => {
                    println!("✅ 所有服务器启动完成 / All servers started");
                    Ok(())
                }
                Err(e) => {
                    println!("❌ 启动服务器失败: {e}");
                    Ok(())
                }
            };
        }
        let id = self.resolve_target(target).await?;
        match self.computer.start_mcp_client(&id).await {
            Ok(()) => println!("✅ 服务器 [bundle_id={id}] 启动完成 / Server started"),
            Err(e) => println!("❌ 启动服务器失败: {e}"),
        }
        Ok(())
    }

    /// 停止客户端（`<target>|all`，target = name 或 bundle_id）/ stop（#141：根治假成功）。
    ///
    /// 旧实现：`stop_mcp_client(name)` 名未命中即静默 `Ok(())` → CLI 照打「✅ 停止完成」而 server 仍在跑。
    ///
    /// **两道防线**（缺一不可，隔离复审两轮各钉一道）：
    ///
    /// 1. `resolve_target` 按协议 §5.1 对**未注册**的 token 报「未找到」——拼错的
    ///    名字止步于此，根本不进 stop；
    /// 2. `stop_mcp_client` 回报 `bool`——**已注册但未挂载**这类真·no-op 打 ℹ️ 而非 ✅。这道防线还兼管
    ///    库层被非 CLI 宿主直调的路径（python `manager.py:245-249` 明文承认自己没堵这个洞）。
    pub async fn stop_client(&self, target: &str) -> Result<(), CommandError> {
        if target == "all" {
            return match self.computer.stop_all_mcp_clients().await {
                Ok(()) => {
                    println!("✅ 所有服务器停止完成 / All servers stopped");
                    Ok(())
                }
                Err(e) => {
                    println!("❌ 停止服务器失败: {e}");
                    Ok(())
                }
            };
        }
        match self.stop_client_line(target).await? {
            Ok(line) => println!("{line}"),
            Err(e) => println!("❌ 停止服务器失败: {e}"),
        }
        Ok(())
    }

    /// [`stop_client`](Self::stop_client) 的**可断言内核**（非 `all` 分支）：返回回执行而非直接 `println!`。
    ///
    /// 见 [`remove_server_line`](Self::remove_server_line) 里同一条理由。外层 `Result` = resolve 失败（未找到
    /// /歧义），内层 = 库层停止失败；二者呈现不同，故不压平。
    pub(crate) async fn stop_client_line(
        &self,
        target: &str,
    ) -> Result<Result<String, ComputerError>, CommandError> {
        let id = self.resolve_target(target).await?;
        Ok(self
            .computer
            .stop_mcp_client(&id)
            .await
            .map(|stopped| stop_receipt(&id, stopped)))
    }

    /// 重启客户端（`<target>`，target = name 或 bundle_id；不收 `all`）/ restart（#141）。
    ///
    /// 兑现 [`Computer::restart_mcp_client`] 的公开面——此前该 API 无任何调用点、doc 却已声称供 CLI 使用。
    pub async fn restart_client(&self, target: &str) -> Result<(), CommandError> {
        let id = self.resolve_target(target).await?;
        match self.computer.restart_mcp_client(&id).await {
            Ok(()) => println!("✅ 服务器 [bundle_id={id}] 重启完成 / Server restarted"),
            Err(e) => println!("❌ 重启服务器失败: {e}"),
        }
        Ok(())
    }

    /// 加载 inputs 配置
    pub async fn load_inputs(&mut self, path: &Path) -> Result<(), CommandError> {
        let content = std::fs::read_to_string(path)?;
        let inputs_value: Value = serde_json::from_str(&content)?;

        // 将 JSON 转换为 Vec<MCPServerInput>
        let inputs_array: Vec<Value> = serde_json::from_value(inputs_value)?;
        let mut inputs_map = HashMap::new();

        for input_value in inputs_array {
            let input: MCPServerInput = serde_json::from_value(input_value)?;
            inputs_map.insert(input.id().to_string(), input);
        }

        // 更新 inputs
        self.computer.update_inputs(inputs_map).await?;

        println!("✅ 已加载 Inputs 配置 / Inputs loaded");

        Ok(())
    }

    /// 列出 inputs 定义
    pub async fn list_inputs(&self) -> Result<(), CommandError> {
        let inputs = self.computer.list_inputs().await?;

        println!("当前 Inputs 定义 / Current Inputs:");
        for input in inputs {
            println!("  - {}", input.id());
        }

        Ok(())
    }

    /// 连接 SocketIO
    ///
    /// #86：`auth` 是连接面鉴权令牌，注入 Socket.IO CONNECT `auth` dict（`{"token": <auth>}`，
    /// 对齐 server 默认读 `token` 字段）；`headers` 仅作路由（非鉴权）。
    pub async fn connect_socketio(
        &mut self,
        url: &str,
        namespace: &str,
        auth: &Option<String>,
        headers: &Option<String>,
    ) -> Result<(), CommandError> {
        let auth_payload = auth
            .clone()
            .map(|token| serde_json::json!({ "token": token }));
        self.computer
            .connect_socketio(
                url,
                ConnectOptions {
                    auth_payload,
                    headers: headers.clone(),
                    namespace: namespace.to_string(),
                },
            )
            .await?;
        println!("✅ 已连接到 Socket.IO: {} / Connected to Socket.IO", url);
        Ok(())
    }

    /// 断开 SocketIO 连接 / Disconnect SocketIO
    pub async fn disconnect_socketio(&mut self) -> Result<(), CommandError> {
        self.computer.disconnect_socketio().await?;
        println!("✅ 已断开 Socket.IO 连接 / Disconnected from Socket.IO");
        Ok(())
    }

    /// 从文件批量导入 server/input 声明并**持久化**（经 `add_or_update_server` 写 local scope）/ bulk-import + persist。
    ///
    /// ⚠️ **无 CLI 入口**（#137 起）：旧 `run --config` 启动参数已退役为 `--mcp-config`——后者是 **flag scope
    /// 覆盖层**（次高、受信、**不落盘**，经 `run_mcp_approval` → `resolve_mcp_config`），与本方法**语义不同**
    /// （本方法落盘持久化）。且本方法读的是 **legacy 数组形态** `{"servers": [ … ]}`，
    /// 与 mcp.json 的**对象形态** `{"servers": {name: def}}`（协议 §9.1）**不一致**。仅作宿主/程序化的批量导入
    /// seam（当前唯 REPL 外调用方 = 测试）；勿把它误当作 mcp.json 加载器。
    pub async fn load_config(&mut self, path: &Path) -> Result<(), CommandError> {
        let content = std::fs::read_to_string(path)?;
        let config: Value = serde_json::from_str(&content)?;

        // 解析服务器配置数组
        if let Some(servers_array) = config.get("servers").and_then(|v| v.as_array()) {
            for server_value in servers_array {
                let server_config: MCPServerConfig = serde_json::from_value(server_value.clone())?;
                self.computer.add_or_update_server(server_config).await?;
            }
        }

        // 解析 inputs 配置
        if let Some(inputs_array) = config.get("inputs").and_then(|v| v.as_array()) {
            let mut inputs_map = HashMap::new();
            for input_value in inputs_array {
                let input: MCPServerInput = serde_json::from_value(input_value.clone())?;
                inputs_map.insert(input.id().to_string(), input);
            }
            self.computer.update_inputs(inputs_map).await?;
        }

        println!("✅ 已加载 Servers 配置 / Servers loaded");

        Ok(())
    }

    /// 获取桌面信息
    pub async fn get_desktop(
        &self,
        size: Option<u32>,
        uri: Option<&str>,
    ) -> Result<(), CommandError> {
        // TODO: 实现获取桌面信息 - 需要等待 desktop 模块实现
        let desktop = json!({
            "windows": [],
            "size": size,
            "uri": uri
        });

        println!("{}", serde_json::to_string_pretty(&desktop)?);
        Ok(())
    }

    /// 显示历史记录
    pub async fn show_history(&self, n: Option<usize>) -> Result<(), CommandError> {
        let history = self.computer.get_tool_history().await?;

        println!("最近工具调用历史 / Recent Tool Call History:");

        if history.is_empty() {
            println!("  (暂无记录 / No records yet)");
        } else {
            let limit = n.unwrap_or(10).min(history.len());
            let start_idx = history.len().saturating_sub(limit);

            for (i, record) in history.iter().skip(start_idx).enumerate() {
                println!(
                    "  {}. [{}] {}::{} - {}{}",
                    i + 1,
                    record.timestamp.format("%Y-%m-%d %H:%M:%S UTC"),
                    record.server,
                    record.tool,
                    if record.success {
                        "成功 / Success"
                    } else {
                        "失败 / Failed"
                    },
                    if let Some(ref error) = record.error {
                        format!(" - {}", error)
                    } else {
                        String::new()
                    }
                );
            }
        }

        Ok(())
    }

    /// 获取输入定义 / Get input definition
    pub async fn get_input_definition(
        &self,
        id: &str,
    ) -> Result<Option<MCPServerInput>, CommandError> {
        Ok(self.computer.get_input(id).await?)
    }

    /// 列出所有输入值 / List all input values
    pub async fn list_input_values(
        &self,
    ) -> Result<HashMap<String, serde_json::Value>, CommandError> {
        Ok(self.computer.list_input_values().await?)
    }

    /// 获取输入值 / Get input value
    pub async fn get_input_value(
        &self,
        id: &str,
    ) -> Result<Option<serde_json::Value>, CommandError> {
        Ok(self.computer.get_input_value(id).await?)
    }

    /// 设置输入值 / Set input value
    pub async fn set_input_value(
        &self,
        id: &str,
        value: &serde_json::Value,
    ) -> Result<bool, CommandError> {
        Ok(self.computer.set_input_value(id, value.clone()).await?)
    }

    /// 删除输入值 / Remove input value
    pub async fn remove_input_value(&self, id: &str) -> Result<bool, CommandError> {
        Ok(self.computer.remove_input_value(id).await?)
    }

    /// 测试渲染（占位符解析）
    pub async fn render_config(&self, config_str: &str) -> Result<(), CommandError> {
        use crate::mcp_clients::render::ConfigRender;

        // 解析配置
        let config: Value = if let Some(path) = config_str.strip_prefix('@') {
            let content = std::fs::read_to_string(path)?;
            serde_json::from_str(&content)?
        } else {
            serde_json::from_str(config_str)?
        };

        // 创建渲染器
        let render = ConfigRender::default();

        // 创建解析器函数
        let resolver = |id: String| async move {
            match self.computer.get_input_value(&id).await {
                Ok(Some(value)) => Ok(value),
                Ok(None) => Err(crate::mcp_clients::render::RenderError::InputNotFound(id)),
                Err(_e) => Err(crate::mcp_clients::render::RenderError::InputNotFound(id)),
            }
        };

        // 执行渲染
        match render.render(config, resolver).await {
            Ok(rendered) => {
                println!("渲染结果 / Rendered result:");
                println!("{}", serde_json::to_string_pretty(&rendered)?);
            }
            Err(e) => {
                eprintln!("渲染失败 / Render failed: {}", e);
            }
        }

        Ok(())
    }

    /// 工具调用调试 / Tool call debug
    pub async fn debug_tool_call(&self, tool_call_str: &str) -> Result<(), CommandError> {
        // 解析工具调用请求
        let tool_call: Value = if let Some(path) = tool_call_str.strip_prefix('@') {
            let content = std::fs::read_to_string(path)?;
            serde_json::from_str(&content)?
        } else {
            serde_json::from_str(tool_call_str)?
        };

        // 提取必需字段
        let req_id = tool_call
            .get("req_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                CommandError::InvalidCommand("缺少 req_id 字段 / Missing req_id field".to_string())
            })?;

        let tool_name = tool_call
            .get("tool_name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                CommandError::InvalidCommand(
                    "缺少 tool_name 字段 / Missing tool_name field".to_string(),
                )
            })?;

        let parameters = tool_call
            .get("params")
            .unwrap_or(&Value::Object(serde_json::Map::new()))
            .clone();

        let timeout = tool_call.get("timeout").and_then(|v| v.as_f64());

        // 检查 MCP Manager 是否已初始化
        if !self.computer.is_mcp_manager_initialized().await {
            println!("警告 / Warning: MCP 管理器未初始化。请先添加并启动服务器 (server add/start) / MCP manager not initialized. Add and start a server first.");
            return Ok(());
        }

        // 执行工具调用
        match self
            .computer
            .execute_tool(req_id, tool_name, parameters, timeout)
            .await
        {
            Ok(result) => {
                println!("工具调用成功 / Tool call succeeded:");
                println!("{}", serde_json::to_string_pretty(&result)?);
            }
            Err(e) => {
                eprintln!("工具调用失败 / Tool call failed: {}", e);
            }
        }

        Ok(())
    }

    /// 加入 Socket.IO 房间 / Join Socket.IO room
    pub async fn join_socket_room(
        &self,
        office_id: &str,
        computer_name: &str,
    ) -> Result<(), CommandError> {
        self.computer.join_office(office_id, computer_name).await?;
        println!("✅ 已加入房间 / Joined office: {}", office_id);
        Ok(())
    }

    /// 离开 Socket.IO 房间 / Leave Socket.IO room
    pub async fn leave_socket_room(&self) -> Result<(), CommandError> {
        self.computer.leave_office().await?;
        println!("✅ 已离开房间 / Left office");
        Ok(())
    }

    /// 发送配置更新通知 / Send config update notification
    pub async fn notify_config_update(&self) -> Result<(), CommandError> {
        self.computer.emit_update_config().await?;
        println!("✅ 配置更新通知已发送 / Config update notification sent");
        Ok(())
    }

    /// 添加或更新输入 / Add or update input
    pub async fn add_input(&mut self, input_str: &str) -> Result<(), CommandError> {
        // 解析输入
        let input_value: Value = if let Some(path) = input_str.strip_prefix('@') {
            let content = std::fs::read_to_string(path)?;
            serde_json::from_str(&content)?
        } else {
            serde_json::from_str(input_str)?
        };

        // 支持单个或数组
        if let Some(array) = input_value.as_array() {
            for item in array {
                let input: MCPServerInput = serde_json::from_value(item.clone())?;
                self.computer.add_or_update_input(input).await?;
            }
        } else {
            let input: MCPServerInput = serde_json::from_value(input_value)?;
            self.computer.add_or_update_input(input).await?;
        }

        println!("Input(s) 已添加/更新 / Added/Updated");
        Ok(())
    }

    /// 更新输入 / Update input
    pub async fn update_input(&mut self, input_str: &str) -> Result<(), CommandError> {
        // 解析输入
        let input_value: Value = if let Some(path) = input_str.strip_prefix('@') {
            let content = std::fs::read_to_string(path)?;
            serde_json::from_str(&content)?
        } else {
            serde_json::from_str(input_str)?
        };

        // 支持单个或数组
        if let Some(array) = input_value.as_array() {
            for item in array {
                let input: MCPServerInput = serde_json::from_value(item.clone())?;
                self.computer.add_or_update_input(input).await?;
            }
        } else {
            let input: MCPServerInput = serde_json::from_value(input_value)?;
            self.computer.add_or_update_input(input).await?;
        }

        println!("Input(s) 已添加/更新 / Added/Updated");
        Ok(())
    }

    /// 移除输入定义 / Remove input definition
    pub async fn remove_input_def(&mut self, id: &str) -> Result<bool, CommandError> {
        let removed = self.computer.remove_input(id).await?;
        if removed {
            println!("已移除 / Removed");
        } else {
            println!("不存在的 id / Not found");
        }
        Ok(removed)
    }

    /// 获取输入定义 / Get input definition
    pub async fn get_input_def(&self, id: &str) -> Result<(), CommandError> {
        match self.computer.get_input(id).await? {
            Some(input) => {
                println!("Input '{}':", id);
                println!("{}", serde_json::to_string_pretty(&input)?);
            }
            None => {
                println!("不存在的 id / Not found: {}", id);
            }
        }
        Ok(())
    }
}

/// `stop <target>` 的用户可见回执 / user-facing receipt for `stop`。
///
/// 🔴 **假回执的第二道防线**（#141）。`stop_client_by_id` 对缺席键幂等返回 `Ok`，据 `Ok` 打 ✅ 就是谎报
/// 「已停止」而 server 仍在跑。故回执**只认 `stopped` 布尔**，不认 `Ok`。
///
/// 第一道防线是 [`resolve_target`](CommandHandler::resolve_target)（未注册 token → 报「未找到」，拼错的名字
/// 止步于此）。二者分工：走到这里的 `false` 只可能是**已注册但未挂载**——故文案说「尚未挂载」而**不**再提
/// 「是否拼写正确」（那会对拼写完全正确、status 里看得见的 server 发出误导提示）。
///
/// 抽成纯函数是为了**可断言**：`println!` 无法被单测检查，而回执文案正是本 issue 的交付物本身。
fn stop_receipt(id: &BundleId, stopped: bool) -> String {
    if stopped {
        format!("✅ 服务器 [bundle_id={id}] 停止完成 / Server stopped")
    } else {
        format!("ℹ️ 服务器 [bundle_id={id}] 尚未挂载，无需停止 / not mounted, nothing to stop")
    }
}

/// `server rm <target>` 的用户可见回执 / user-facing receipt for `server rm`。
///
/// 同 [`stop_receipt`]：`removed=false` 表示**既无声明落盘可删、也无运行期实例可停摘**——必须如实说
/// 「未做任何操作」，否则用户以为删掉了、而它下次 boot 原样回来。
fn remove_receipt(id: &BundleId, removed: bool) -> String {
    if removed {
        format!("已移除服务器配置 (bundle_id={id}) / Removed server config")
    } else {
        format!(
            "ℹ️ 服务器 [bundle_id={id}] 无可删声明、亦无活跃实例，未做任何操作 / nothing to remove"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computer::SilentSession;
    use std::io::Write;
    use tempfile::NamedTempFile;

    /// 创建测试用的 Computer 实例 / Create test Computer instance
    async fn create_test_computer() -> Computer<SilentSession> {
        // #113 S6：add/remove_server 现落盘 → 定向到隔离临时目录，避免污染进程 cwd。TempDir 唯一，`forget`
        // 保留至进程退出（测试期定向落盘、不清理；助手不返回 TempDir 句柄故显式 leak）/ isolated config anchor。
        let tmp = tempfile::TempDir::new().unwrap();
        let config_dir = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        Computer::new(
            "test_computer",
            SilentSession::new("test_session"),
            None,
            None,
            false,
            false,
        )
        .with_config_dir(config_dir)
    }

    /// 创建测试用的 CommandHandler 实例 / Create test CommandHandler instance
    fn create_test_handler(computer: Computer<SilentSession>) -> CommandHandler {
        let cli_config = CliConfig {
            url: None,
            namespace: "/smcp".to_string(),
            auth: None,
            headers: None,
        };
        CommandHandler::new(computer, cli_config)
    }

    /// 挂一个 stdio server（display 名与显式 bundle_id 分离，用于钉「按名还是按 id」）。
    async fn mount(computer: &Computer<SilentSession>, name: &str, bundle_id: Option<&str>) {
        let mut v = serde_json::json!({
            "type": "stdio",
            "name": name,
            "server_parameters": {"command": "echo"},
        });
        if let Some(b) = bundle_id {
            v["bundle_id"] = serde_json::json!(b);
        }
        computer
            .mount_server(serde_json::from_value(v).unwrap())
            .await
            .unwrap();
    }

    /// #141 🔴1（隔离复审在**真二进制**上复现的破坏性误删）：**display name 必须先于 bundle_id 查**。
    ///
    /// 复现原文（修复前）：`A(name=foo, id=foo_1)` + `B(name=bar, id=foo)` 共存时
    /// ```text
    /// a2c> server rm foo
    /// 已移除服务器配置 (bundle_id=foo) / Removed server config   ← 删掉的是 bar！
    /// ```
    /// 用户敲的是自己**看得见的名字**，回执里的 bundle_id 他分辨不出是别人的。这比原先的「假成功」更危险。
    ///
    /// 权威：协议 §5.1（`sdk-api-guidance.md:127-145`）步骤序 **1=display name → 2=bundle_id**；
    /// python `cli/resolve.py:183-192` 逐行同构并注明「顺序有意义，勿重排」。
    #[tokio::test]
    async fn resolve_target_checks_name_before_bundle_id_141() {
        let computer = create_test_computer().await;
        mount(&computer, "foo", Some("foo_1")).await;
        // B 的 **bundle_id** 恰好等于 A 的 **display name**——这正是步骤序会露馅的配置。
        mount(&computer, "bar", Some("foo")).await;
        let handler = create_test_handler(computer);

        assert_eq!(
            handler.resolve_target("foo").await.unwrap().as_str(),
            "foo_1",
            "MUST 命中 display 名为 foo 的那条（A），而非 bundle_id 恰为 foo 的 B"
        );
        // 反向：按 B 自己的名字查仍得 B。
        assert_eq!(handler.resolve_target("bar").await.unwrap().as_str(), "foo");
    }

    /// #141 🔴2：**语法合法 ≠ 存在**——未注册的合法 bundle_id MUST 报「未找到」，不得放行到底层幂等 no-op。
    ///
    /// 协议 §5.1 步骤 2 明文「仍无 → 报错「未找到」」、步骤 5「未命中 MUST 报错，MUST NOT 静默成功」；
    /// python `resolve.py:165-166` 同构并写明理由「否则 `stop <合法但不存在的 id>` 会一路走到底层的静默
    /// no-op ⇒ 假成功复活」。
    ///
    /// 此前本仓注释宣称「R4 **必须**放行 0 命中但语法合法的 bundle_id」——**协议、issue 正文、python 三处
    /// 均无此说，系凭空杜撰**，且正是那条假回执得以复活的入口。
    #[tokio::test]
    async fn resolve_target_rejects_unregistered_valid_bundle_id_141() {
        let computer = create_test_computer().await;
        mount(&computer, "everything", None).await;
        let handler = create_test_handler(computer);

        // `everthing`（拼错一个字母）是**合法** bundle_id 字面量——字符集 `[A-Za-z0-9_-]` 决定了绝大多数
        // 拼写错误都合法。正因如此，「语法合法就放行」等于对一切拼写错误假成功。
        assert!(BundleId::try_from("everthing".to_string()).is_ok());
        for unknown in ["everthing", "no-such-server", "a__b", "my.api", ""] {
            let err = handler
                .resolve_target(unknown)
                .await
                .expect_err("未注册的 target MUST 报「未找到」（禁静默成功）");
            let msg = format!("{err}");
            assert!(
                msg.contains("未找到") || msg.contains("not found"),
                "错误须自解释「未找到」，实际: {msg}"
            );
        }
        // 已注册的 bundle_id 仍照常命中（别把收紧做成「一律拒绝」）。
        assert_eq!(
            handler.resolve_target("everything").await.unwrap().as_str(),
            "everything"
        );
    }

    /// #141 🔴3：候选空间 = **运行期活跃集 ∪ 声明面**——已落盘但未挂载的声明按 display 名必须可寻址。
    ///
    /// 复现（修复前）：`my.api` 落盘到 project mcp.json 但卡在审批门外未挂载 ⇒ `server rm my.api` 报
    /// 「未知目标」，而 `Computer::remove_server` 读磁盘快照**本来删得掉**它。凡 display 名不是合法
    /// bundle_id 字面量者（含 `.`、空格、中文…），用户从 CLI **无路可删**。
    ///
    /// python `collect_candidates`（`resolve.py:81-156` 决策 1）逐字命中该缺陷：「若只取运行期，『手改
    /// mcp.json 但未重载』的声明会被本解析器判为『未找到』，而 `aremove_server` 本可删掉它 ⇒ 回归」。
    #[tokio::test]
    async fn resolve_target_covers_declared_but_unmounted_141() {
        let computer = create_test_computer().await;
        // 只落盘声明、**不**挂载（`add_or_update_server` 写 project mcp.json + 内存投影；这里靠 CLI 候选表
        // 的声明面分支来寻址它）。名字含 `.` ⇒ 不是合法 bundle_id 字面量，只能按 name 找到。
        computer
            .add_or_update_server(
                serde_json::from_value(serde_json::json!({
                    "type": "stdio",
                    "name": "my.api",
                    "server_parameters": {"command": "echo"},
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        let handler = create_test_handler(computer);

        let id = handler
            .resolve_target("my.api")
            .await
            .expect("已声明的 server 按 display 名 MUST 可寻址（即便未挂载）");
        assert_eq!(id.as_str(), "my_api", "解析到其派生 bundle_id");
    }

    /// #141 🔴4：**回执的真实调用链**必须被测到——不是只测纯函数映射。
    ///
    /// 隔离复审的判别性检验：把回执硬编成成功态，920 条测试仍全绿（因为没有一条覆盖 `stop_client` /
    /// `remove_server` 的非 `all` 分支）。本测走 `*_line` 内核，断言用户真正看到的那行字。
    #[tokio::test]
    async fn cli_receipts_are_honest_through_real_call_chain_141() {
        let computer = create_test_computer().await;
        mount(&computer, "everything", None).await;
        let handler = create_test_handler(computer);

        // ① 拼错的 target：止步于 resolve，**根本打不出回执**（第一道防线）。
        let err = handler
            .stop_client_line("everthing")
            .await
            .expect_err("拼错的 target MUST 在 resolve 阶段报错");
        assert!(format!("{err}").contains("未找到"));

        // ② 已注册但**未挂载**：走完真实调用链，回执 MUST 无 ✅（第二道防线）。
        let line = handler
            .stop_client_line("everything")
            .await
            .expect("已注册 ⇒ resolve 通过")
            .expect("库层不报错");
        assert!(
            !line.contains('✅') && !line.contains("停止完成"),
            "未挂载却打了成功回执: {line}"
        );
        assert!(line.contains("尚未挂载"), "回执须自解释: {line}");

        // ③ 别把根治做成「一律不报成功」：`true` 分支仍是 ✅。（真活跃客户端的 `true` 由
        // `tests/integration_tests.rs` 的 manager 级用例覆盖，此处钉回执映射。）
        let id = handler.resolve_target("everything").await.unwrap();
        assert!(stop_receipt(&id, true).contains('✅'));

        // ④ `server rm` 同链路：真删到 → 「已移除」。
        let line = handler.remove_server_line("everything").await.unwrap();
        assert!(
            line.contains("已移除"),
            "确有实例可停摘 ⇒ 应报已移除: {line}"
        );
        // 再删一次：此时既无声明也无实例 ⇒ MUST 报「未做任何操作」，且该 target 已不可寻址。
        assert!(handler.remove_server_line("everything").await.is_err());
    }

    /// #141/R4：同名多 server → **列候选（bundle_id + name + 归属）报错，禁字典序最小**。
    #[tokio::test]
    async fn resolve_target_ambiguous_lists_candidates_141() {
        let computer = create_test_computer().await;
        // 两条**同 display 名、显式异 bundle_id** 的合法共存 server（协议：name 允许碰撞、非身份）。
        let mk = |bid: &str| -> MCPServerConfig {
            serde_json::from_value(serde_json::json!({
                "type": "stdio",
                "name": "dup",
                "bundle_id": bid,
                "server_parameters": {"command": "echo"},
            }))
            .unwrap()
        };
        computer.mount_server(mk("dup-a")).await.unwrap();
        computer.mount_server(mk("dup-b")).await.unwrap();
        let handler = create_test_handler(computer);

        let err = handler
            .resolve_target("dup")
            .await
            .expect_err("同名多命中 MUST 报歧义、禁字典序最小");
        let msg = format!("{err}");
        assert!(
            msg.contains("dup-a") && msg.contains("dup-b"),
            "须列全部候选 bundle_id: {msg}"
        );
        assert!(msg.contains("bundle_id="), "候选须带 bundle_id 标注: {msg}");
        // 精确按 bundle_id 寻址 → 各自命中（同名不再互相隐身）。
        assert_eq!(
            handler.resolve_target("dup-a").await.unwrap().as_str(),
            "dup-a"
        );
        assert_eq!(
            handler.resolve_target("dup-b").await.unwrap().as_str(),
            "dup-b"
        );
    }

    /// #171 Candidate B：bundle_id 精确等于冲突名时 MUST 命中，不应报「请用 bundle_id 重试」。
    ///
    /// 死锁场景：A(name="foo", bundle_id="foo") + B(name="foo", bundle_id="bundle_x") 同时存在时，
    /// 步骤②多命中直接报错，而步骤③ bundle_id 匹配被「0 name hits」门阻断 ⇒ A 永远不可寻址。
    /// Candidate B 在步骤②多命中分支内先做精确 bundle_id 匹配，匹配到则直接返回。
    #[tokio::test]
    async fn resolve_target_deadlock_bundle_id_equals_collision_name_171() {
        let computer = create_test_computer().await;
        // A: name="foo", bundle_id="foo"（缺省派生——与冲突名完全重合的死锁场景）
        mount(&computer, "foo", None).await;
        // B: name="foo", bundle_id="bundle_x"（plugin 贡献的同名 server）
        mount(&computer, "foo", Some("bundle_x")).await;
        let handler = create_test_handler(computer);

        // 核心断言：token 精确等于 A 的 bundle_id → MUST 命中 A
        assert_eq!(
            handler.resolve_target("foo").await.unwrap().as_str(),
            "foo",
            "bundle_id 精确等于冲突名时，MUST 命中该 server 而非报错"
        );
        // B 仍通过自己的 bundle_id 可达（步骤③ 0 name hit 路径）
        assert_eq!(
            handler.resolve_target("bundle_x").await.unwrap().as_str(),
            "bundle_x"
        );
        // 即使再加第三个同名 server（C: name="foo", bundle_id="foo_2"），
        // token "foo" 仍精确命中 A 的 bundle_id → 不报歧义。
        mount(&handler.computer, "foo", Some("foo_2")).await;
        assert_eq!(
            handler.resolve_target("foo").await.unwrap().as_str(),
            "foo",
            "新增同名 server 后，精确 bundle_id 匹配仍优先"
        );
        // 同名无精确匹配 → 仍报歧义（现有 resolve_target_ambiguous_lists_candidates_141 已覆盖：
        // dup-a/dup-b 同时存在，token="dup" 不匹配任何 bundle_id → 报错列候选）。
    }

    #[tokio::test]
    async fn test_show_help() {
        let computer = create_test_computer().await;
        let handler = create_test_handler(computer);

        // 测试帮助信息不会崩溃 / Test help doesn't crash
        handler.show_help();
    }

    #[tokio::test]
    async fn test_show_status_uninitialized() {
        let computer = create_test_computer().await;
        let handler = create_test_handler(computer);

        // 测试未初始化状态 / Test uninitialized state
        let result = handler.show_status().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_add_server_with_json() {
        let computer = create_test_computer().await;
        let mut handler = create_test_handler(computer);

        // 测试添加服务器配置 / Test adding server config
        let json_config = r#"
{
    "type": "Stdio",
    "name": "test_stdio",
    "disabled": false,
    "forbidden_tools": [],
    "tool_meta": {},
    "default_tool_meta": null,
    "vrl": null,
    "server_parameters": {
        "command": "echo",
        "args": ["hello"],
        "env": {},
        "cwd": null
    }
}
"#;

        let result = handler.add_server_debug(json_config).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_add_server_invalid_json() {
        let computer = create_test_computer().await;
        let mut handler = create_test_handler(computer);

        // 测试无效 JSON / Test invalid JSON
        let invalid_json = "{ invalid json }";

        let result = handler.add_server(invalid_json).await;
        assert!(result.is_err());
        matches!(result.unwrap_err(), CommandError::JsonError(_));
    }

    #[tokio::test]
    async fn test_add_server_from_file() -> Result<(), std::io::Error> {
        let computer = create_test_computer().await;
        let mut handler = create_test_handler(computer);

        // 创建临时配置文件 / Create temp config file
        let mut temp_file = NamedTempFile::new()?;
        writeln!(
            temp_file,
            r#"
{{
    "type": "Stdio",
    "name": "test_from_file",
    "disabled": false,
    "forbidden_tools": [],
    "tool_meta": {{}},
    "default_tool_meta": null,
    "vrl": null,
    "server_parameters": {{
        "command": "echo",
        "args": ["hello"],
        "env": {{}},
        "cwd": null
    }}
}}
        "#
        )?;

        let config_path = format!("@{}", temp_file.path().display());
        let result = handler.add_server(&config_path).await;
        assert!(result.is_ok());

        Ok(())
    }

    /// #141：删不存在的 target → **报「未找到」**。
    ///
    /// 旧断言是 `assert!(result.is_ok())`，注释写「即使不存在也应该成功」——那正是本 issue 要根治的假成功，
    /// 且 `remove_server` 当时任何情况下都返回 `Ok` ⇒ 该断言恒真、零判别力（隔离复审 🔴4）。
    #[tokio::test]
    async fn test_remove_server() {
        let computer = create_test_computer().await;
        let mut handler = create_test_handler(computer);

        let err = handler
            .remove_server("non_existent")
            .await
            .expect_err("不存在的 target MUST 报错（协议 §5.1-5：MUST NOT 静默成功）");
        assert!(format!("{err}").contains("未找到"));
    }

    /// #141：未初始化（无任何 server）时 `start <未知>` → 报「未找到」；`stop all` 仍是无害 no-op。
    #[tokio::test]
    async fn test_start_stop_client_uninitialized() {
        let computer = create_test_computer().await;
        let handler = create_test_handler(computer);

        let err = handler
            .start_client("test")
            .await
            .expect_err("未注册的 target MUST 报「未找到」");
        assert!(format!("{err}").contains("未找到"));

        // `all` 是 CLI 侧哨兵、不过 resolve——空集停全部是合法 no-op。
        handler.stop_client("all").await.expect("stop all 应无害");
    }

    #[tokio::test]
    async fn test_load_inputs() -> Result<(), std::io::Error> {
        let computer = create_test_computer().await;
        let mut handler = create_test_handler(computer);

        // 创建临时 inputs 文件 / Create temp inputs file
        let mut temp_file = NamedTempFile::new()?;
        writeln!(
            temp_file,
            r#"
[
    {{
        "type": "PromptString",
        "id": "test_input",
        "description": "Test input",
        "default": "default_value",
        "password": false
    }}
]
        "#
        )?;

        let result = handler.load_inputs(temp_file.path()).await;
        assert!(result.is_ok());

        Ok(())
    }

    #[tokio::test]
    async fn test_list_inputs_empty() {
        let computer = create_test_computer().await;
        let handler = create_test_handler(computer);

        // 测试列出空的 inputs / Test listing empty inputs
        let result = handler.list_inputs().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_show_history_empty() {
        let computer = create_test_computer().await;
        let handler = create_test_handler(computer);

        // 测试显示空历史 / Test showing empty history
        let result = handler.show_history(Some(5)).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_get_desktop() {
        let computer = create_test_computer().await;
        let handler = create_test_handler(computer);

        // 测试获取桌面信息 / Test getting desktop info
        let result = handler.get_desktop(Some(10), Some("test://uri")).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_load_config() -> Result<(), std::io::Error> {
        let computer = create_test_computer().await;
        let mut handler = create_test_handler(computer);

        // 创建完整配置文件 / Create complete config file
        let mut temp_file = NamedTempFile::new()?;
        writeln!(
            temp_file,
            r#"
{{
    "servers": [
        {{
            "type": "Stdio",
            "name": "test_server",
            "disabled": false,
            "forbidden_tools": [],
            "tool_meta": {{}},
            "default_tool_meta": null,
            "vrl": null,
            "server_parameters": {{
                "command": "echo",
                "args": ["test"],
                "env": {{}},
                "cwd": null
            }}
        }}
    ],
    "inputs": [
        {{
            "type": "PromptString",
            "id": "test_input",
            "description": "Test input",
            "default": "default",
            "password": false
        }}
    ]
}}
        "#
        )?;

        let result = handler.load_config(temp_file.path()).await;
        assert!(result.is_ok());

        Ok(())
    }

    // 表驱动测试示例 / Table-driven test example
    #[tokio::test]
    async fn test_add_server_validation() {
        let computer = create_test_computer().await;
        let mut handler = create_test_handler(computer);

        let test_cases = vec![
            // (json, should_succeed, description)
            (
                r#"{"type": "Stdio", "name": "test"}"#,
                false,
                "Missing required fields",
            ),
            (
                r#"{"type": "Invalid", "name": "test"}"#,
                false,
                "Invalid server type",
            ),
            (r#""not a json""#, false, "Not a JSON object"),
            (
                r#"{"type": "Stdio", "name": "", "server_parameters": {}}"#,
                false,
                "Empty name",
            ),
        ];

        for (json, should_succeed, description) in test_cases {
            let result = handler.add_server(json).await;
            if should_succeed {
                assert!(result.is_ok(), "Should succeed: {}", description);
            } else {
                assert!(result.is_err(), "Should fail: {}", description);
            }
        }
    }
}
