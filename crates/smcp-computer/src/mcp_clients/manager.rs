/**
* 文件名: manager
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: tokio, async-trait, serde_json
* 描述: MCP服务器管理器，负责管理多个MCP服务器连接和工具调用路由
*/
use super::auth_error;
use super::bundle_id;
use super::http_client::HttpMCPClient;
use super::model::*;
use super::utils::client_factory;
use super::vrl_runtime::VrlRuntime;
use crate::errors::ComputerError;
use crate::inputs::SecretValueResolver;
use crate::oauth::{
    InMemoryOAuthCredentialStore, OAuthBeginRequest, OAuthCallback, OAuthCancellation,
    OAuthCredentialStore, OAuthError, OAuthFlow, OAuthFlowOutcome, OAuthLaunch, OAuthStatus,
};
use crate::skills::{McpResource, SkillResourceManager, SkillStagingError};
use crate::status::RuntimeStatus;
use crate::weak_registry::WeakRegistry;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc as StdArc;
use std::sync::Arc;
use tokio::sync::{mpsc, watch, Mutex, RwLock};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

/// `list_skill_resources` 单 server 翻页上限（防御非终止 cursor）/ per-server page cap for
/// `list_skill_resources` (guards a non-terminating cursor)。对标 Python `_MAX_SKILL_LIST_PAGES`。
const MAX_SKILL_LIST_PAGES: usize = 1000;

/// SKILL 资源 URI scheme 前缀 / SKILL resource URI scheme prefix。
const SKILL_URI_PREFIX: &str = "skill://";

/// 单条聚合工具的路由项 / A single aggregated-tool route entry（协议 0.3.0 `ExposedToolMapping`）。
///
/// `client:get_tools` 与 `client:tool_call` **共享同一份** `exposed_tool_name -> ExposedToolRoute` 表：前者
/// 产出 `exposed_tool_name`（键），后者按**整键查表**路由到 `bundle_id` + `original_tool_name` 调上游 MCP。
/// **禁**对 `exposed_tool_name` 做字符串 split 反解身份（`alias` 不可逆推 `original`，`bundle_id` 禁 `__` 保单射）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExposedToolRoute {
    /// 归属 MCP Server 唯一标识（路由到 `active_clients` 的键）/ owning server's BundleID（active_clients key）。
    pub bundle_id: BundleId,
    /// MCP Server 人类可读名（诊断/展示用，非路由键）/ human-readable server name（diagnostics only）。
    pub server_name: ServerName,
    /// MCP 上游注册的原始工具名（路由目标）/ upstream-registered original tool name（routing target）。
    pub original_tool_name: ToolName,
    /// 若配置了别名（仅替换工具名部分）/ configured alias if any（replaces the tool-name part only）。
    pub alias: Option<String>,
}

/// client factory 类型（#152 测试接缝）/ client factory type（test seam）。
///
/// [`MCPServerManager::start_client_by_id`] 默认用自由函数 [`client_factory`]
/// 创建真实 rmcp 客户端（stdio/sse/http）。经 [`MCPServerManager::set_client_factory`] 注入 override 后改用之——
/// **仅为 hermetic 测试**「mount→running」而设：真实 MCP 连接无法在单测复现，注入假 client（如 `MockSkillClient`）
/// 使 `connect()` 成功，方能断言 server 达 `active`。生产路径不注入（保持真实连接）。
pub type ClientFactory = StdArc<
    dyn Fn(MCPServerConfig, Option<ClientNotifyCtx>) -> StdArc<dyn MCPClientProtocol> + Send + Sync,
>;

/// MCP服务器管理器 / MCP server manager
///
/// **身份键 = `bundle_id`（协议 0.3.0，rust-sdk#117）**：`servers_config` / `active_clients` / `retry_counts`
/// 均以 [`BundleId`] 为键（no-double-open 去重、同名跨源 server 共存）。**#141/R4：公开生命周期方法
/// （`start_client_by_id` / `stop_client_by_id` / `remove_server_by_id` / `get_window_detail` 等）一律收
/// `&BundleId`**——管理器内**不再做名解析**（旧 `bundle_id_for_name` 的名未命中→静默 `Ok` 正是
/// `stop` 假回执的来源）。人类可读名→bundle_id 的启发式只存在于 CLI 的 `resolve_target`，库层不参与。
/// `bundle_id` 由 [`bundle_id`] 在管理器内**从所持 config 计算一次**——避免 raw/rendered 连接身份漂移
/// 致的 bundle_id 不一致。
///
/// 幂等方法（`stop_client_by_id` / `remove_server_by_id`）返回 `bool` 而非 `()`：缺席键不报错，但**必须
/// 如实回报「什么都没做」**，否则调用方（CLI）会把拼错的 target 打成 ✅。
///
/// **对外标识一律 `bundle_id`**：desktop `window://` 分组（#118）与 skill `skill://` 枚举/物化（#127）均以
/// `bundle_id` 标注——协议 §身份正交性规定 `name` 是纯 display、允许碰撞、永不做键。唯一仍与 `bundle_id`
/// **正交**的是 `window://` / `skill://` URI 里的 **host** 段：它由 MCP Server 自选，A2C 透传不解释。
pub struct MCPServerManager {
    /// 服务器配置映射（键 = `bundle_id`）/ Server configuration mapping keyed by bundle_id。
    servers_config: Arc<RwLock<HashMap<BundleId, MCPServerConfig>>>,
    /// 活动客户端映射（键 = `bundle_id`）/ Active client mapping keyed by bundle_id。
    active_clients: Arc<RwLock<HashMap<BundleId, StdArc<dyn MCPClientProtocol>>>>,
    /// 聚合工具路由表：`exposed_tool_name -> ExposedToolRoute`（get_tools 与 tool_call 共享，单射整键查表）。
    tool_routes: Arc<RwLock<HashMap<ExposedToolName, ExposedToolRoute>>>,
    /// 禁用工具集合（键 = `exposed_tool_name`）/ Disabled tools set keyed by exposed_tool_name。
    disabled_tools: Arc<RwLock<HashSet<ExposedToolName>>>,
    /// 自动重连标志 / Auto reconnect flag
    auto_reconnect: Arc<RwLock<bool>>,
    /// 自动连接标志 / Auto connect flag
    auto_connect: Arc<RwLock<bool>>,
    /// 状态变化通知器 / State change notifier
    state_notifier: watch::Sender<ManagerState>,
    /// 健康检查配置 / Health check configuration
    health_check_config: Arc<RwLock<HealthCheckConfig>>,
    /// 重连策略 / Reconnect policy
    reconnect_policy: Arc<RwLock<ReconnectPolicy>>,
    /// 健康监控任务句柄 / Health monitor task handle
    health_monitor_handle: Arc<RwLock<Option<tokio::task::JoinHandle<()>>>>,
    /// 重试计数器（`bundle_id` -> 重试次数）/ Retry counters (bundle_id -> retry count)
    retry_counts: Arc<RwLock<HashMap<BundleId, u32>>>,
    /// MCP 运行期变化通知发送端（#106）/ runtime MCP change-notification sender。
    ///
    /// 由 `Computer::boot_up` 在**客户端启动前**经 [`set_change_sender`](Self::set_change_sender) 注入；
    /// [`start_client`](Self::start_client) 据此为每个新客户端构造 [`ClientNotifyCtx`]，使 stdio/sse/http
    /// 三传输的服务器主动通知（tools/resources list_changed / resource updated）能上报给 Computer 消费者任务。
    /// 未注入（None）时客户端不转发通知，行为与历史一致。
    change_tx: Arc<RwLock<Option<mpsc::UnboundedSender<McpServerNotification>>>>,
    /// client factory override（#152 测试接缝）/ client factory override（test seam）。
    ///
    /// `None` → [`start_client_by_id`](Self::start_client_by_id) 用真实 [`client_factory`]；
    /// `Some` → 用注入的 factory（hermetic 测试注入假 client）。详见 [`ClientFactory`]。
    client_factory_override: Arc<RwLock<Option<ClientFactory>>>,
    /// HTTP OAuth clients are retained even when the initial MCP handshake requires authorization.
    oauth_clients: Arc<RwLock<HashMap<BundleId, Arc<HttpMCPClient>>>>,
    /// One host-injected keyed store shared by every OAuth MCP managed by this instance.
    oauth_credential_store: Arc<dyn OAuthCredentialStore>,
    /// Optional Computer-owned event sink for OAuth status transitions.
    oauth_events: Option<Arc<RuntimeStatus>>,
    secret_resolver: Arc<RwLock<Option<Arc<dyn SecretValueResolver>>>>,
    #[cfg(test)]
    test_http_root_certificates: Arc<RwLock<Vec<reqwest::Certificate>>>,
    /// Serialize start/stop/update for each server identity without blocking unrelated servers.
    lifecycle_locks: Arc<WeakRegistry<BundleId, Mutex<()>>>,
}

/// 管理器状态 / Manager state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagerState {
    /// 未初始化 / Uninitialized
    Uninitialized,
    /// 已初始化 / Initialized
    Initialized,
    /// 运行中 / Running
    Running,
    /// 错误状态 / Error
    Error,
}

impl MCPServerManager {
    fn lifecycle_lock(&self, bundle_id: &BundleId) -> Arc<Mutex<()>> {
        self.lifecycle_locks
            .get_or_insert_with(bundle_id.clone(), || Mutex::new(()))
    }

    #[cfg(test)]
    fn lifecycle_registry_len(&self) -> usize {
        self.lifecycle_locks.len()
    }

    /// 创建新的管理器 / Create new manager
    pub fn new() -> Self {
        let (state_tx, _) = watch::channel(ManagerState::Uninitialized);

        Self {
            servers_config: Arc::new(RwLock::new(HashMap::new())),
            active_clients: Arc::new(RwLock::new(HashMap::new())),
            tool_routes: Arc::new(RwLock::new(HashMap::new())),
            disabled_tools: Arc::new(RwLock::new(HashSet::new())),
            auto_reconnect: Arc::new(RwLock::new(true)),
            auto_connect: Arc::new(RwLock::new(false)),
            state_notifier: state_tx,
            health_check_config: Arc::new(RwLock::new(HealthCheckConfig::default())),
            reconnect_policy: Arc::new(RwLock::new(ReconnectPolicy::default())),
            health_monitor_handle: Arc::new(RwLock::new(None)),
            retry_counts: Arc::new(RwLock::new(HashMap::new())),
            change_tx: Arc::new(RwLock::new(None)),
            client_factory_override: Arc::new(RwLock::new(None)),
            oauth_clients: Arc::new(RwLock::new(HashMap::new())),
            oauth_credential_store: Arc::new(InMemoryOAuthCredentialStore::default()),
            oauth_events: None,
            secret_resolver: Arc::new(RwLock::new(None)),
            #[cfg(test)]
            test_http_root_certificates: Arc::new(RwLock::new(Vec::new())),
            lifecycle_locks: Arc::new(WeakRegistry::default()),
        }
    }

    /// 使用自定义配置创建管理器 / Create manager with custom configuration
    pub fn with_config(
        health_check_config: HealthCheckConfig,
        reconnect_policy: ReconnectPolicy,
    ) -> Self {
        let (state_tx, _) = watch::channel(ManagerState::Uninitialized);

        Self {
            servers_config: Arc::new(RwLock::new(HashMap::new())),
            active_clients: Arc::new(RwLock::new(HashMap::new())),
            tool_routes: Arc::new(RwLock::new(HashMap::new())),
            disabled_tools: Arc::new(RwLock::new(HashSet::new())),
            auto_reconnect: Arc::new(RwLock::new(reconnect_policy.enabled)),
            auto_connect: Arc::new(RwLock::new(false)),
            state_notifier: state_tx,
            health_check_config: Arc::new(RwLock::new(health_check_config)),
            reconnect_policy: Arc::new(RwLock::new(reconnect_policy)),
            health_monitor_handle: Arc::new(RwLock::new(None)),
            retry_counts: Arc::new(RwLock::new(HashMap::new())),
            change_tx: Arc::new(RwLock::new(None)),
            client_factory_override: Arc::new(RwLock::new(None)),
            oauth_clients: Arc::new(RwLock::new(HashMap::new())),
            oauth_credential_store: Arc::new(InMemoryOAuthCredentialStore::default()),
            oauth_events: None,
            secret_resolver: Arc::new(RwLock::new(None)),
            #[cfg(test)]
            test_http_root_certificates: Arc::new(RwLock::new(Vec::new())),
            lifecycle_locks: Arc::new(WeakRegistry::default()),
        }
    }

    /// Create a manager with a host-provided keyed OAuth credential store.
    ///
    /// The store is runtime state and is never serialized into MCP configuration.
    pub fn with_oauth_credential_store(store: Arc<dyn OAuthCredentialStore>) -> Self {
        Self {
            oauth_credential_store: store,
            ..Self::new()
        }
    }

    pub(crate) fn with_oauth_events(mut self, events: Arc<RuntimeStatus>) -> Self {
        self.oauth_events = Some(events);
        self
    }

    /// 获取状态通知器 / Get state notifier
    pub fn get_state_notifier(&self) -> watch::Receiver<ManagerState> {
        self.state_notifier.subscribe()
    }

    /// 注入 MCP 运行期变化通知发送端（#106）/ inject the runtime MCP change-notification sender。
    ///
    /// 必须在 [`start_client_by_id`](Self::start_client_by_id) / [`start_all`](Self::start_all) **之前**调用，才能让随后
    /// 创建的客户端携带 [`ClientNotifyCtx`]。已激活客户端不追溯注入（对齐"启动前接线"约定）。
    pub async fn set_change_sender(&self, tx: mpsc::UnboundedSender<McpServerNotification>) {
        *self.change_tx.write().await = Some(tx);
    }

    /// 注入 client factory override（#152 测试接缝）/ inject a client factory override（test seam）。
    ///
    /// `None`（默认）→ [`start_client_by_id`](Self::start_client_by_id) 用真实
    /// [`client_factory`]；`Some` → 改用注入 factory（hermetic 测试用）。须在
    /// `start_client_by_id` / `start_all` **之前**调用（已激活客户端不追溯）。详见 [`ClientFactory`]。
    pub async fn set_client_factory(&self, factory: Option<ClientFactory>) {
        *self.client_factory_override.write().await = factory;
    }

    pub async fn set_secret_resolver(&self, resolver: Option<Arc<dyn SecretValueResolver>>) {
        *self.secret_resolver.write().await = resolver;
    }

    #[cfg(test)]
    async fn set_test_http_root_certificates(&self, certificates: Vec<reqwest::Certificate>) {
        *self.test_http_root_certificates.write().await = certificates;
    }

    /// 按 config + 通知接缝创建客户端：override 优先，否则真实 [`client_factory`]（#152）/ make a client.
    fn make_client(
        &self,
        config: MCPServerConfig,
        notify: Option<ClientNotifyCtx>,
    ) -> StdArc<dyn MCPClientProtocol> {
        match self.client_factory_override.try_read() {
            Ok(guard) => match guard.as_ref() {
                Some(factory) => factory(config, notify),
                None => client_factory(config, notify),
            },
            // 仅 async 上下文写（set_client_factory）；start 路径无写竞争 → 立即重读真实 factory。
            Err(_) => client_factory(config, notify),
        }
    }

    /// 为指定 server 构造通知上报接缝（发送端已注入时）/ build a per-client notify seam if a sender is set。
    ///
    /// 以 `bundle_id`（server 唯一身份）打来源标签（#127）——消费侧的定向重挂据此寻址。
    async fn notify_ctx_for(&self, bundle_id: &BundleId) -> Option<ClientNotifyCtx> {
        self.change_tx
            .read()
            .await
            .as_ref()
            .map(|tx| ClientNotifyCtx {
                bundle_id: bundle_id.clone(),
                tx: tx.clone(),
            })
    }

    /// 更新管理器状态 / Update manager state
    async fn update_state(&self, state: ManagerState) {
        let _ = self.state_notifier.send(state);
    }

    /// 解析出恒有值的 `bundle_id`：显式配置值优先，否则缺省生成。**在管理器内从所持 config 计算**，避免
    /// raw/rendered 漂移。
    ///
    /// #130：**不再校验显式值**——[`BundleId`] 构造即校验（`mcp.json` 里的畸形 `bundleId` 在 serde 反序列化的
    /// **字段级**即判废，由 `settings::mcp_config::validate_server` 逐-server 降级；程序化构造亦拿不到非法
    /// `BundleId`）⇒ 本函数**无从**收到非法值，故由 `Result` 收敛为**不可失败**。
    fn resolve_key(config: &MCPServerConfig) -> BundleId {
        bundle_id::resolve_bundle_id(config)
    }

    // #141/R4：name→bundle_id 的桥 `bundle_id_for_name` / `active_client_key` **已删除**——库层公开 API 一律收
    // `bundle_id`（消歧的字典序最小 + 碰撞路由是同名歧义的根源）。人机面 name→身份的解析归 CLI `resolve_target`
    // （源 `list_mcp_servers_with_metadata`，多命中列候选、0 命中且合法 id 当 id、否则报错），不再进库层。

    /// 初始化管理器 / Initialize manager
    ///
    /// **no-double-open（加载期 first-wins）**：按传入顺序解析每个 server 的 `bundle_id`，重复 `bundle_id`
    /// （无论 connection config 是否相同）仅保留**第一个**，其余作 **Computer 本地配置诊断**（结构化 WARN，
    /// **非协议错误码**，协议 §config-diagnostics）。
    ///
    /// #130：「显式非法 `bundle_id` 作诊断跳过」的分支**已前移**——非法值在 `mcp.json` 反序列化的字段级即被
    /// [`BundleId`] 判废，由 `settings::mcp_config::validate_server` 逐-server drop + 记错（整份文件仍不 abort），
    /// 故到不了这里。「不硬失败整批 boot」的语义不变，只是判废点更早、更响亮。
    pub async fn initialize(&self, servers: Vec<MCPServerConfig>) -> Result<(), ComputerError> {
        // 停止所有现有客户端 / Stop all existing clients
        self.stop_all().await?;

        // 清空所有状态 / Clear all state
        self.clear_all().await?;

        // 添加新配置（按 bundle_id 去重，first-wins + 诊断）/ Add configs, deduped by bundle_id (first-wins)。
        {
            let mut configs = self.servers_config.write().await;
            for server in servers {
                let bid = Self::resolve_key(&server);
                match configs.entry(bid.clone()) {
                    std::collections::hash_map::Entry::Occupied(existing) => {
                        // no-double-open：重复 bundle_id，保留 config 顺序第一个（本地配置诊断，非协议错误码）。
                        warn!(
                            bundle_id = %bid,
                            kept = %existing.get().name(),
                            dropped = %server.name(),
                            "duplicate bundle_id at load; keeping first (no-double-open config diagnostic)"
                        );
                    }
                    std::collections::hash_map::Entry::Vacant(v) => {
                        v.insert(server);
                    }
                }
            }
        }

        // 刷新工具路由 / Refresh tool routes
        self.refresh_tool_routes().await?;

        // 更新状态 / Update state
        self.update_state(ManagerState::Initialized).await;

        info!("Manager initialized successfully");
        Ok(())
    }

    /// 添加或更新服务器配置 / Add or update server configuration
    ///
    /// **no-double-open（运行期 update-in-place）**：按解析出的 `bundle_id` **原地更新**（`name` 可变、
    /// `bundle_id` 稳定），传入已存在的 `bundle_id` 不算冲突。
    ///
    /// #130：显式非法 `bundle_id` 已无从抵达（[`BundleId`] 构造即校验），故此处不再有该拒绝分支。
    pub async fn add_or_update_server(&self, config: MCPServerConfig) -> Result<(), ComputerError> {
        let bundle_id = Self::resolve_key(&config);
        let lifecycle = self.lifecycle_lock(&bundle_id);
        let _lifecycle_guard = lifecycle.lock().await;

        // 检查是否已激活（按 bundle_id）/ Check if already active (by bundle_id)
        let is_active = {
            let clients = self.active_clients.read().await;
            clients.contains_key(&bundle_id)
        };

        if is_active {
            let auto_reconnect = *self.auto_reconnect.read().await;
            if !auto_reconnect {
                return Err(ComputerError::InvalidConfiguration(format!(
                    "Server {} (bundle_id={}) is active. Stop it before updating config",
                    config.name(),
                    bundle_id
                )));
            }
        }

        // Atomically retire the old OAuth capability before any fallible MCP stop. Callback
        // facades can no longer reacquire or recreate this client while replacement is in flight.
        let replaced_oauth = self.oauth_clients.write().await.remove(&bundle_id);
        if let Some(client) = replaced_oauth {
            client
                .cancel_and_drain_oauth_flow()
                .await
                .map_err(|error| {
                    ComputerError::ConnectionError(format!("failed to drain OAuth flow: {error}"))
                })?;
        }
        if is_active {
            self.stop_client_by_id_inner(&bundle_id).await?;
        }

        // 更新配置（原地更新：同 bundle_id 覆盖）/ Update configuration (update-in-place by bundle_id)
        {
            let mut configs = self.servers_config.write().await;
            configs.insert(bundle_id.clone(), config);
        }

        // 检查是否需要自动连接 / Check if need auto connect
        let auto_connect = *self.auto_connect.read().await;
        if is_active || auto_connect {
            self.start_client_by_id_inner(&bundle_id).await?;
        }

        // 刷新工具路由 / Refresh tool routes
        self.refresh_tool_routes().await?;

        Ok(())
    }

    /// 移除服务器配置（**bundle_id 寻址**，协议 §身份 MUST 用 bundle_id）；返回**是否真的移除** / remove by identity。
    ///
    /// 直接按身份键操作，**无** name→bundle_id 桥的歧义（对齐 Python `aremove_server(bundle_id)`）。
    ///
    /// #141/R4：形参由 `&str` 收敛为 `&BundleId`（库层公开 API 一律收身份），顺带消灭「非法串 → `refresh + Ok(())`
    /// 静默成功」这一与本 Issue 同形的假回执残留（非法值现在**构造不出** `BundleId`，在类型层即被拦）。
    /// 未注册的合法身份键 → 幂等 no-op 但如实返回 `false`（供调用方打真实回执）。
    pub async fn remove_server_by_id(&self, bundle_id: &BundleId) -> Result<bool, ComputerError> {
        let lifecycle = self.lifecycle_lock(bundle_id);
        let _lifecycle_guard = lifecycle.lock().await;
        let exists = { self.servers_config.read().await.contains_key(bundle_id) };
        if !exists {
            self.refresh_tool_routes().await?;
            return Ok(false);
        }

        // Retire OAuth before a fallible transport stop so removal always cancels a pending flow.
        let removed_oauth = self.oauth_clients.write().await.remove(bundle_id);
        if let Some(client) = removed_oauth {
            client
                .cancel_and_drain_oauth_flow()
                .await
                .map_err(|error| {
                    ComputerError::ConnectionError(format!("failed to drain OAuth flow: {error}"))
                })?;
        }

        // 停止客户端（按 bundle_id）/ Stop client (by bundle_id)
        self.stop_client_by_id_inner(bundle_id).await?;

        // 移除配置 / Remove configuration
        {
            let mut configs = self.servers_config.write().await;
            configs.remove(bundle_id);
        }
        // 刷新工具路由 / Refresh tool routes
        self.refresh_tool_routes().await?;

        Ok(true)
    }

    /// 启动所有启用的服务器 / Start all enabled servers
    pub async fn start_all(&self) -> Result<(), ComputerError> {
        // 直接取身份键（bundle_id）迭代，避免逐个 name→bundle_id 解析。
        let bundle_ids: Vec<BundleId> = {
            let configs = self.servers_config.read().await;
            configs
                .iter()
                .filter(|(_, config)| !config.disabled())
                .map(|(bid, _)| bid.clone())
                .collect()
        };

        for bundle_id in bundle_ids {
            self.start_client_by_id(&bundle_id).await?;
        }

        // 更新状态 / Update state
        self.update_state(ManagerState::Running).await;

        info!("All servers started successfully");
        Ok(())
    }

    /// 启动单个客户端（**身份键寻址**）/ Start single client (bundle_id-addressed)。
    ///
    /// #141/R4：删除 name 寻址的 `start_client`——库层公开 API 一律收 `bundle_id`；name→身份的解析归 CLI
    /// `resolve_target`（未知即报错，不再 `Unknown server` 混淆同名歧义）。
    pub async fn start_client_by_id(&self, bundle_id: &BundleId) -> Result<(), ComputerError> {
        let lifecycle = self.lifecycle_lock(bundle_id);
        let _lifecycle_guard = lifecycle.lock().await;
        self.start_client_by_id_inner(bundle_id).await
    }

    async fn start_client_by_id_inner(&self, bundle_id: &BundleId) -> Result<(), ComputerError> {
        // 获取配置 / Get configuration
        let config = {
            let configs = self.servers_config.read().await;
            configs.get(bundle_id).cloned()
        };

        let config = config.ok_or_else(|| {
            ComputerError::InvalidConfiguration(format!("Unknown server bundle_id: {}", bundle_id))
        })?;

        let server_name = config.name().to_string();

        if config.disabled() {
            return Err(ComputerError::InvalidConfiguration(format!(
                "Cannot start disabled server: {}",
                server_name
            )));
        }

        // 检查是否已启动 / Check if already started
        {
            let clients = self.active_clients.read().await;
            if clients.contains_key(bundle_id) {
                return Ok(()); // 已经启动 / Already started
            }
        }

        // 创建客户端（注入通知上报接缝，用 **bundle_id** 打来源标签——消费侧定向重挂按身份寻址，#106/#127）。
        let notify = self.notify_ctx_for(bundle_id).await;
        let client: StdArc<dyn MCPClientProtocol> = match config.clone() {
            MCPServerConfig::Http(http) => {
                let existing = { self.oauth_clients.read().await.get(bundle_id).cloned() };
                let concrete = if let Some(existing) = existing {
                    existing.set_notify(notify);
                    existing
                } else {
                    let resolver = self.secret_resolver.read().await.clone();
                    let candidate = HttpMCPClient::new(http.server_parameters)
                        .with_oauth_context(
                            bundle_id.clone(),
                            Arc::clone(&self.oauth_credential_store),
                            self.oauth_events.clone(),
                        )
                        .with_auth_policy(http.auth_policy, http.oauth, resolver)
                        .map_err(|error| ComputerError::InvalidConfiguration(error.to_string()))?
                        .with_notify(notify.clone());
                    #[cfg(test)]
                    let candidate = candidate.with_test_root_certificates(
                        self.test_http_root_certificates.read().await.clone(),
                    );
                    let candidate = Arc::new(candidate);
                    let mut oauth_clients = self.oauth_clients.write().await;
                    if let Some(existing) = oauth_clients.get(bundle_id).cloned() {
                        existing.set_notify(notify);
                        existing
                    } else {
                        oauth_clients.insert(bundle_id.clone(), candidate.clone());
                        candidate
                    }
                };
                concrete
            }
            _ => self.make_client(config, notify),
        };

        // 连接服务器 / Connect to server
        client.connect().await.map_err(|error| match error {
            MCPClientError::HttpAuthentication(error) => ComputerError::HttpAuthentication(error),
            error => ComputerError::ConnectionError(format!(
                "Failed to connect to {}: {}",
                server_name, error
            )),
        })?;

        // 添加到活动客户端（按 bundle_id）/ Add to active clients (by bundle_id)
        {
            let mut clients = self.active_clients.write().await;
            clients.insert(bundle_id.clone(), client);
        }

        // 刷新工具路由 / Refresh tool routes
        self.refresh_tool_routes().await?;

        info!(
            "Client {} (bundle_id={}) started successfully",
            server_name, bundle_id
        );
        Ok(())
    }

    /// 停止单个客户端（**身份键寻址**）；返回**是否真的停了**/ Stop by bundle_id; returns whether it actually stopped。
    ///
    /// #141/R4：删除 name 寻址的 `stop_client`（它在 name 未命中时 `refresh + Ok(())` 谎报成功）。
    ///
    /// 🔴 **返回 `bool` 是假回执的根治点**：库层对缺席键保持**幂等**（无可停、非错误），但把「有没有停到东西」
    /// **如实上报**给调用方——否则 CLI 无从分辨「真停了」与「压根没这个活跃客户端」，只能一律打 ✅，那正是
    /// 本 Issue 要消灭的谎报（拼错的 server 名恰好是合法 bundle_id 字面量时尤甚）。
    /// `Ok(true)` = 确有活跃客户端被摘除并断连；`Ok(false)` = 该身份键无活跃客户端，未做任何事。
    pub async fn stop_client_by_id(&self, bundle_id: &BundleId) -> Result<bool, ComputerError> {
        let lifecycle = self.lifecycle_lock(bundle_id);
        let _lifecycle_guard = lifecycle.lock().await;
        self.stop_client_by_id_inner(bundle_id).await
    }

    async fn stop_client_by_id_inner(&self, bundle_id: &BundleId) -> Result<bool, ComputerError> {
        // 移除客户端 / Remove client
        let mut client = {
            let mut clients = self.active_clients.write().await;
            clients.remove(bundle_id)
        };
        let was_active = client.is_some();

        // 断开连接 / Disconnect
        if let Some(ref mut c) = client {
            c.disconnect().await.map_err(|e| {
                ComputerError::ConnectionError(format!(
                    "Failed to disconnect from {}: {}",
                    bundle_id, e
                ))
            })?;
        }

        // 刷新工具路由（幂等，无论是否停到都保持路由一致）/ Refresh tool routes
        self.refresh_tool_routes().await?;

        if was_active {
            info!("Client (bundle_id={}) stopped successfully", bundle_id);
        } else {
            debug!(
                "No active client for bundle_id={} — nothing to stop (idempotent)",
                bundle_id
            );
        }
        Ok(was_active)
    }

    /// 重启服务器（**身份键寻址**）/ Restart server (bundle_id-addressed)。#141：公开供 CLI `restart`。
    pub async fn restart_client_by_id(&self, bundle_id: &BundleId) -> Result<(), ComputerError> {
        let lifecycle = self.lifecycle_lock(bundle_id);
        let _lifecycle_guard = lifecycle.lock().await;
        // #141：**先查声明再动手**。此前用 `unwrap_or(false)` 兜底，未知 bundle_id 走成
        // 「stop 幂等 no-op + enabled=false 不 start」⇒ 静默 `Ok(())` ——与被根治的 `stop` 假回执同形。
        // restart 语义蕴含「事后应在跑」，故对不存在的声明必须报错（与 `start_client_by_id` 一致）；
        // 声明存在但 `disabled` 则停而不起，仍是 `Ok`（尊重停用意图，非假成功）。
        let enabled = {
            let configs = self.servers_config.read().await;
            let config = configs.get(bundle_id).ok_or_else(|| {
                ComputerError::InvalidConfiguration(format!(
                    "Unknown server bundle_id: {bundle_id}"
                ))
            })?;
            !config.disabled()
        };

        self.stop_client_by_id_inner(bundle_id).await?;

        if enabled {
            self.start_client_by_id_inner(bundle_id).await?;
        }

        Ok(())
    }

    /// 停止所有客户端 / Stop all clients
    pub async fn stop_all(&self) -> Result<(), ComputerError> {
        let bundle_ids: Vec<BundleId> = {
            let clients = self.active_clients.read().await;
            clients.keys().cloned().collect()
        };

        for bundle_id in bundle_ids {
            self.stop_client_by_id(&bundle_id).await?;
        }

        // 更新状态 / Update state
        self.update_state(ManagerState::Initialized).await;

        info!("All servers stopped successfully");
        Ok(())
    }

    /// 清空所有状态 / Clear all state
    async fn clear_all(&self) -> Result<(), ComputerError> {
        self.servers_config.write().await.clear();
        self.active_clients.write().await.clear();
        self.tool_routes.write().await.clear();
        self.disabled_tools.write().await.clear();
        let oauth_clients = {
            let mut clients = self.oauth_clients.write().await;
            clients
                .drain()
                .map(|(_, client)| client)
                .collect::<Vec<_>>()
        };
        let mut first_error = None;
        for client in oauth_clients {
            if let Err(error) = client.cancel_and_drain_oauth_flow().await {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), |error| {
            Err(ComputerError::ConnectionError(format!(
                "failed to drain OAuth flow: {error}"
            )))
        })
    }

    /// 已注册（routed）工具数——读**已缓存**的 `tool_routes`，**不发** `tools/list` RPC（#114 S7 status 用）。
    /// Registered/routed tool count from the cached routes; issues no MCP RPC (cheap for status snapshots)。
    ///
    /// 与 [`list_available_tools`](Self::list_available_tools) 的区别：后者为产出完整 `Tool` 会对每个活跃 server 拉
    /// 一次 `tools/list`（网络/子进程往返）；本方法只取路由表长度。路由由 start/stop/refresh 维护，反映 desired 已加载
    /// 工具集，适合廉价、非阻塞的 status 计数。
    pub async fn tool_count(&self) -> usize {
        self.tool_routes.read().await.len()
    }

    /// 关闭管理器 / Close manager
    pub async fn close(&self) -> Result<(), ComputerError> {
        let stop_result = self.stop_all().await;
        // OAuth cleanup is unconditional: a transport disconnect failure must not leave browser
        // authorization running until the provider timeout.
        let clear_result = self.clear_all().await;
        self.update_state(ManagerState::Uninitialized).await;
        stop_result?;
        clear_result?;
        info!("Manager closed successfully");
        Ok(())
    }

    /// 兼容别名：重建工具路由（旧名 `refresh_tool_mapping`）/ compat alias for [`Self::refresh_tool_routes`]。
    ///
    /// #106 消费者任务经此在 `emit_update_tool_list` **之前**重建路由。保留旧名以免破坏既有调用点。
    pub async fn refresh_tool_mapping(&self) -> Result<(), ComputerError> {
        self.refresh_tool_routes().await
    }

    /// 重建聚合工具路由表 `tool_routes`（`exposed_tool_name -> ExposedToolRoute`，协议 0.3.0 BundleID 模型）。
    ///
    /// exposed 名 = `{bundle_id}__{alias ?? 原始工具名}`（[`bundle_id::exposed_tool_name`]），跨 Server / bundle
    /// 天然唯一（`bundle_id` 禁 `__` 保单射）。**不再有**跨 server 同名硬错误（前缀化后重名消失，即 #116 收益）；
    /// **同一 bundle_id 内** alias 撞出相同 exposed 名 = **Computer 本地配置诊断**（WARN，first-wins，非协议错误码）。
    /// `forbidden_tools` 按 `original_tool_name` 或 `exposed_tool_name` 命中 → 不进路由、记入 `disabled_tools`。
    ///
    /// #106：运行期 `tools/list_changed` 到达时须在 `emit_update_tool_list` **之前**调用，否则新增工具不进路由
    /// → `list_available_tools` 漏掉 → Agent 回拉看不到（"坑 1"）。消费者任务在 event-loop 外调用，无重入风险。
    pub async fn refresh_tool_routes(&self) -> Result<(), ComputerError> {
        let mut routes: HashMap<ExposedToolName, ExposedToolRoute> = HashMap::new();
        let mut disabled: HashSet<ExposedToolName> = HashSet::new();

        // 收集所有活动服务器的工具 / Collect tools from all active servers
        let clients = self.active_clients.read().await;
        let configs = self.servers_config.read().await;

        for (bundle_id, client) in clients.iter() {
            let config = match configs.get(bundle_id) {
                Some(c) => c,
                None => continue,
            };
            let server_name = config.name().to_string();

            // #134：`default_tool_meta.alias` 天生 per-tool、放 default 位无合理用例（无法把 N 个工具改成
            // 同一名）。merged_tool_meta 已不再继承它；此处每 server 每次 refresh 打一次配置诊断，把「静默
            // 忽略」变响亮（不静默丢 + 配置诊断姿态）。空串按未设处理，与 python 真值判定逐行同构。
            if config
                .default_tool_meta()
                .and_then(|d| d.alias.as_deref())
                .is_some_and(|a| !a.is_empty())
            {
                warn!(
                    bundle_id = %bundle_id,
                    server_name = %server_name,
                    "default_tool_meta.alias is ignored (aliases are per-tool; a default alias would \
                     collapse all tools of this server into one exposed name). Rename via per-tool \
                     tool_meta.<tool>.alias instead (config diagnostic)"
                );
            }

            // 获取工具列表 / Get tool list
            match client.list_tools().await {
                Ok(tools) => {
                    for tool in tools {
                        let original_tool_name = tool.name.to_string();

                        // 合并工具元数据取 alias（仅替换工具名部分）/ merged alias (replaces the tool-name part)。
                        let tool_meta = self.merged_tool_meta(config, &original_tool_name);
                        let alias = tool_meta.and_then(|meta| meta.alias);

                        // exposed 名 = {bundle_id}__{alias ?? original}，恒带前缀、跨 bundle 唯一。
                        let exposed = bundle_id::exposed_tool_name(
                            bundle_id,
                            alias.as_deref(),
                            &original_tool_name,
                        );

                        // forbidden_tools 按 original 或 exposed 命中 → 不暴露、不路由（记入 disabled 供 4001 前
                        // 更明确的 PermissionError）。The forbidden check precedes routing so a disabled tool
                        // is neither exposed nor routed.
                        let forbidden_tools = config.forbidden_tools();
                        if forbidden_tools
                            .iter()
                            .any(|f| f == &original_tool_name || f == &exposed)
                        {
                            disabled.insert(exposed);
                            continue;
                        }

                        // 同一 bundle_id 内 alias 撞出相同 exposed 名：本地配置诊断，first-wins（跨 bundle 不会撞）。
                        if let Some(existing) = routes.get(&exposed) {
                            warn!(
                                bundle_id = %bundle_id,
                                exposed = %exposed,
                                kept = %existing.original_tool_name,
                                dropped = %original_tool_name,
                                "duplicate exposed_tool_name within one bundle_id (alias collision); \
                                 keeping first (config diagnostic)"
                            );
                            continue;
                        }

                        routes.insert(
                            exposed,
                            ExposedToolRoute {
                                bundle_id: bundle_id.clone(),
                                server_name: server_name.clone(),
                                original_tool_name,
                                alias,
                            },
                        );
                    }
                }
                Err(e) => {
                    error!(
                        "Error listing tools for {} (bundle_id={}): {}",
                        server_name, bundle_id, e
                    );
                }
            }
        }

        drop(clients);
        drop(configs);

        // 原子换出（读侧只见旧或新整表）/ atomically swap in the freshly built tables。
        *self.tool_routes.write().await = routes;
        *self.disabled_tools.write().await = disabled;

        debug!("Tool routes refreshed successfully");
        Ok(())
    }

    /// 验证工具调用并路由 / Validate a tool call and route it（协议 0.3.0 BundleID 模型）。
    ///
    /// 入参 `exposed_tool_name` = `client:tool_call` 的 `tool_name`（`{bundle_id}__{alias ?? 原始名}`）。经**共享**
    /// `tool_routes` **整键查表**（**不** split 反解）路由，返回 `(bundle_id, server_name,
    /// original_tool_name)`：`bundle_id` 是 `active_clients` 的键（供 [`call_tool`](Self::call_tool)），`server_name`
    /// 为诊断/历史用人类可读名，`original_tool_name` 是调上游 MCP 的原始名。映射未命中 →
    /// [`ComputerError::InvalidConfiguration`]（Computer 层映射协议 `4001 Tool Not Found`）。被 `forbidden_tools`
    /// 禁用 → [`ComputerError::PermissionError`]（比 4001 更明确的本地拒绝）。
    pub async fn validate_tool_call(
        &self,
        exposed_tool_name: &str,
        _parameters: &serde_json::Value,
    ) -> Result<(BundleId, ServerName, ToolName), ComputerError> {
        // 被 forbidden_tools 禁用 / disabled by forbidden_tools。
        if self.disabled_tools.read().await.contains(exposed_tool_name) {
            return Err(ComputerError::PermissionError(format!(
                "Tool '{}' is disabled by configuration",
                exposed_tool_name
            )));
        }

        // 整键查共享路由表 / whole-key lookup in the shared route table。
        let routes = self.tool_routes.read().await;
        let route = routes.get(exposed_tool_name).ok_or_else(|| {
            ComputerError::InvalidConfiguration(format!(
                "Tool '{}' not found in any active server",
                exposed_tool_name
            ))
        })?;

        Ok((
            route.bundle_id.clone(),
            route.server_name.clone(),
            route.original_tool_name.clone(),
        ))
    }

    /// 把 pub API 收到的 `&str` 身份键转为 [`BundleId`]（#130 过渡垫片）/ parse a pub-API id param。
    ///
    /// 形参改收 `BundleId` 属 **#141**（库层 API 一律收 bundle_id）；在此之前，非法串**必然**不是活动键
    /// ⇒ 归入既有的「未激活」错误，**无新增失败模式、无行为变更**。
    fn active_key(bundle_id: &str, tool_name: &str) -> Result<BundleId, ComputerError> {
        BundleId::try_from(bundle_id).map_err(|_| {
            ComputerError::InvalidConfiguration(format!(
                "Server '{}' for tool '{}' is not active",
                bundle_id, tool_name
            ))
        })
    }

    /// 调用工具 / Call tool
    pub async fn call_tool(
        &self,
        bundle_id: &str,
        tool_name: &str,
        parameters: serde_json::Value,
        timeout: Option<std::time::Duration>,
    ) -> Result<CallToolResult, ComputerError> {
        let key = Self::active_key(bundle_id, tool_name)?;
        // 获取客户端引用 / Get client reference
        let client = {
            let clients = self.active_clients.read().await;
            clients
                .get(&key)
                .ok_or_else(|| {
                    ComputerError::InvalidConfiguration(format!(
                        "Server '{}' for tool '{}' is not active",
                        bundle_id, tool_name
                    ))
                })?
                .clone()
        };
        // 执行工具调用 / Execute tool call
        let result = if let Some(timeout) = timeout {
            tokio::time::timeout(timeout, client.call_tool(tool_name, parameters))
                .await
                .map_err(|_| ComputerError::TimeoutError("Tool execution timed out".to_string()))?
        } else {
            client.call_tool(tool_name, parameters).await
        };

        self.finalize_tool_result(&key, tool_name, result).await
    }

    /// 可取消工具调用（INT-02 #70 取消最后一公里）/ Cancellable tool call.
    ///
    /// 与 [`Self::call_tool`] 同：套用 manager 级 `timeout`，并对**完成**结果跑相同收尾
    /// （授权分流 / `tool_meta` / VRL，见 `Self::finalize_tool_result`）。差异仅在改调
    /// [`MCPClientProtocol::call_tool_cancellable`] 并透传 `cancel`：
    /// - [`CancellableCallOutcome::Cancelled`]（取消胜出）→ 原样上抛，由 `Computer` 写结果级 `meta.a2c_cancelled`；
    /// - 完成 / 上游错误 → 经 `finalize_tool_result` 收尾后包回 [`CancellableCallOutcome::Completed`]；
    /// - 超时 → [`ComputerError::TimeoutError`]（`Computer` 写 `meta.a2c_timeout`）。
    pub async fn call_tool_cancellable(
        &self,
        bundle_id: &str,
        tool_name: &str,
        parameters: serde_json::Value,
        timeout: Option<std::time::Duration>,
        cancel: CancellationToken,
    ) -> Result<CancellableCallOutcome, ComputerError> {
        let key = Self::active_key(bundle_id, tool_name)?;
        // 获取客户端引用 / Get client reference
        let client = {
            let clients = self.active_clients.read().await;
            clients
                .get(&key)
                .ok_or_else(|| {
                    ComputerError::InvalidConfiguration(format!(
                        "Server '{}' for tool '{}' is not active",
                        bundle_id, tool_name
                    ))
                })?
                .clone()
        };

        // 执行可取消调用（manager 级 timeout 包裹；token 透传至客户端就地中断 + best-effort 远端补发）。
        // ⚠️ 超时分支语义：manager 级 timeout 触发时直接 **drop** client future、**不**经取消 token 的
        // select! 分支，故超时**不**向远端补发 MCP notifications/cancelled（区别于 Agent 显式取消）。这是
        // best-effort 协作式取消的有意取舍（timeout ≠ cancel）：超时的 stdio 工具子进程可能仍在跑，但 Agent
        // 已据 meta.a2c_timeout 拿到超时态响应。如需超时也补发，须改成 token 路径取消而非 drop（后续评估）。
        let outcome = if let Some(timeout) = timeout {
            tokio::time::timeout(
                timeout,
                client.call_tool_cancellable(tool_name, parameters, cancel),
            )
            .await
            .map_err(|_| ComputerError::TimeoutError("Tool execution timed out".to_string()))?
        } else {
            client
                .call_tool_cancellable(tool_name, parameters, cancel)
                .await
        };

        match outcome {
            // 取消胜出：在途调用已就地中断（rmcp 传输已 best-effort 补发 notifications/cancelled）。上抛由
            // Computer 写取消态结果，不在此构造（保持「控制流结果 vs 协议态结果」分层）。
            Ok(CancellableCallOutcome::Cancelled) => Ok(CancellableCallOutcome::Cancelled),
            // 完成：跑与 call_tool 一致的收尾（授权分流可能把 4006/4007 转成协议形状授权结果）。
            Ok(CancellableCallOutcome::Completed(r)) => self
                .finalize_tool_result(&key, tool_name, Ok(r))
                .await
                .map(CancellableCallOutcome::Completed),
            // 上游错误：交由 finalize 的授权分流（Err 路径）；非授权类仍上抛 ProtocolError。
            Err(e) => self
                .finalize_tool_result(&key, tool_name, Err(e))
                .await
                .map(CancellableCallOutcome::Completed),
        }
    }

    /// 工具调用结果收尾——[`Self::call_tool`] 与 [`Self::call_tool_cancellable`] 共用 / shared finalize。
    ///
    /// 上游错误分流（AUTH-01 #23）：授权类（4006/4007）→ 协议形状授权 `CallToolResult` 透传；其余 ProtocolError。
    /// 成功结果：注入合并后的 `tool_meta` + 可选 VRL 转换。
    async fn finalize_tool_result(
        &self,
        bundle_id: &BundleId,
        tool_name: &str,
        result: Result<CallToolResult, MCPClientError>,
    ) -> Result<CallToolResult, ComputerError> {
        // 上游错误分流（AUTH-01 #23）：授权类（4006/4007）→ 以协议形状的授权 CallToolResult 透传
        // （error-handling.md §403，内嵌结果级 meta，**非** flat ErrorPayload）；其余维持通用 ProtocolError。
        // Branch upstream errors: authorization (4006/4007) surfaces a protocol-shaped auth
        // CallToolResult; everything else stays a generic ProtocolError.
        let mut result = match result {
            Ok(r) => r,
            Err(e) => match auth_error::classify_auth_error(&e) {
                Some(code) => {
                    let hint = auth_error::build_default_auth_hint(code);
                    // 协议 0.3.0 §身份正交性（#18）：授权错误 `meta.mcp_server` = **bundle_id**（形参 bundle_id
                    // 即 call_tool 传入的 bundle_id 身份键），与 `get_config` 归属一致，Agent 可 correlate 到具体 server。
                    return Ok(auth_error::build_auth_error_result(
                        bundle_id.as_str(),
                        code,
                        hint,
                    ));
                }
                None => {
                    return Err(ComputerError::ProtocolError(format!(
                        "Tool execution failed: {}",
                        e
                    )))
                }
            },
        };

        // 添加工具元数据到结果 / Add tool metadata to result
        let config = {
            let configs = self.servers_config.read().await;
            configs.get(bundle_id).cloned()
        };

        if let Some(config) = config {
            if let Some(tool_meta) = self.merged_tool_meta(&config, tool_name) {
                if result.meta.is_none() {
                    result.meta = Some(rmcp::model::Meta::new());
                }
                if let Some(ref mut meta) = result.meta {
                    meta.insert(
                        A2C_TOOL_META.to_string(),
                        serde_json::to_value(tool_meta).unwrap(),
                    );
                }
            }

            // VRL转换 / VRL transformation
            if let Some(vrl_script) = config.vrl() {
                // 获取原始参数用于VRL处理
                // Note: 这里需要从调用栈获取原始参数，暂时使用空对象
                let parameters = serde_json::json!({});

                // 创建VRL事件，包含工具调用结果和元数据
                let mut event = serde_json::to_value(&result).unwrap_or_default();
                if let Value::Object(ref mut map) = event {
                    map.insert(
                        "tool_name".to_string(),
                        Value::String(tool_name.to_string()),
                    );
                    map.insert("parameters".to_string(), parameters);
                }

                // 执行VRL转换
                let mut runtime = VrlRuntime::new();
                match runtime.run(vrl_script, event, "UTC") {
                    Ok(vrl_result) => {
                        // 将转换后的结果存储到meta中
                        if result.meta.is_none() {
                            result.meta = Some(rmcp::model::Meta::new());
                        }
                        if let Some(ref mut meta) = result.meta {
                            // 将转换后的结果序列化为JSON字符串
                            if let Ok(transformed_json) =
                                serde_json::to_string(&vrl_result.processed_event)
                            {
                                meta.insert(
                                    A2C_VRL_TRANSFORMED.to_string(),
                                    Value::String(transformed_json),
                                );
                            }
                        }
                        debug!(
                            "VRL转换成功 / VRL transformation succeeded for tool '{}'",
                            tool_name
                        );
                    }
                    Err(e) => {
                        warn!(
                            "VRL转换失败 / VRL transformation failed for tool '{}': {}. 原始结果将正常返回 / Original result will be returned normally.",
                            tool_name, e
                        );
                    }
                }
            }
        }

        Ok(result)
    }

    /// 执行工具（支持别名） / Execute tool (supports alias)
    pub async fn execute_tool(
        &self,
        tool_name: &str,
        parameters: serde_json::Value,
        timeout: Option<std::time::Duration>,
    ) -> Result<CallToolResult, ComputerError> {
        let (bundle_id, _server_name, original_tool_name) =
            self.validate_tool_call(tool_name, &parameters).await?;
        self.call_tool(bundle_id.as_str(), &original_tool_name, parameters, timeout)
            .await
    }

    /// 获取服务器状态列表 `(bundle_id, name, is_active, state)` / Get server status list。
    ///
    /// **每行自带身份键（#127）**：`.0` = `bundle_id`（唯一身份、寻址键），`.1` = display 名（人类可读、
    /// 可碰撞、非身份）。此前只出 name，调用方（CLI `status`）须再按 name 去 join 一张 name-keyed 的
    /// bundle_id 映射——同名 server 在那张映射里折叠，导致两行打印**同一个** `bundle_id`、用户按
    /// `server rm <bundle_id>` 删错对象。同源直出即消除该 join。
    pub async fn get_server_status(&self) -> Vec<(BundleId, ServerName, bool, String)> {
        let configs = self.servers_config.read().await;
        let clients = self.active_clients.read().await;

        configs
            .iter()
            .map(|(bundle_id, config)| {
                let is_active = clients.contains_key(bundle_id);
                let state = if is_active {
                    clients
                        .get(bundle_id)
                        .map(|c| c.state().to_string())
                        .unwrap_or_else(|| "unknown".to_string())
                } else {
                    "pending".to_string()
                };
                (
                    bundle_id.clone(),
                    config.name().to_string(),
                    is_active,
                    state,
                )
            })
            .collect()
    }

    /// 获取所有服务器配置（用于 GetComputerConfigRet）
    /// Get all server configurations (for GetComputerConfigRet)
    ///
    /// **协议 0.3.0 §身份正交性（#18）**：返回字典 **key = `bundle_id`**（server 唯一身份，非 `name`）；每个 value
    /// 额外带 `name`（纯 display，key 不再人类可读，故须显式暴露展示名）。
    /// Returns format: `{ bundle_id: { name, type, status, disabled, ... } }`。
    pub async fn get_server_configs(&self) -> serde_json::Value {
        let configs = self.servers_config.read().await;
        let clients = self.active_clients.read().await;

        let mut result = serde_json::Map::new();

        for (bundle_id, config) in configs.iter() {
            let is_active = clients.contains_key(bundle_id);
            let state = if is_active {
                clients
                    .get(bundle_id)
                    .map(|c| c.state().to_string())
                    .unwrap_or_else(|| "unknown".to_string())
            } else {
                "pending".to_string()
            };

            // 构建服务器配置信息 / Build server config info
            let mut server_info = serde_json::Map::new();

            // 展示名（key 现为 bundle_id，须显式带 name 供 display / tool→server 归属）/ display name。
            server_info.insert(
                "name".to_string(),
                serde_json::Value::String(config.name().to_string()),
            );

            // 添加类型信息 / Add type info
            let server_type = match config {
                MCPServerConfig::Stdio(_) => "stdio",
                MCPServerConfig::Sse(_) => "sse",
                MCPServerConfig::Http(_) => "http",
            };
            server_info.insert(
                "type".to_string(),
                serde_json::Value::String(server_type.to_string()),
            );

            // 添加状态信息 / Add status info
            server_info.insert("status".to_string(), serde_json::Value::String(state));
            server_info.insert("is_active".to_string(), serde_json::Value::Bool(is_active));
            server_info.insert(
                "disabled".to_string(),
                serde_json::Value::Bool(config.disabled()),
            );

            // 添加禁用工具列表 / Add forbidden tools list
            let forbidden_tools: Vec<serde_json::Value> = config
                .forbidden_tools()
                .iter()
                .map(|t| serde_json::Value::String(t.clone()))
                .collect();
            server_info.insert(
                "forbidden_tools".to_string(),
                serde_json::Value::Array(forbidden_tools),
            );

            // 添加工具元数据 / Add tool metadata
            if let Ok(tool_meta_json) = serde_json::to_value(config.tool_meta()) {
                server_info.insert("tool_meta".to_string(), tool_meta_json);
            }

            // 添加默认工具元数据 / Add default tool metadata
            if let Some(default_meta) = config.default_tool_meta() {
                if let Ok(default_meta_json) = serde_json::to_value(default_meta) {
                    server_info.insert("default_tool_meta".to_string(), default_meta_json);
                }
            }

            // 添加 VRL 脚本（如果有）/ Add VRL script if present
            if let Some(vrl) = config.vrl() {
                server_info.insert(
                    "vrl".to_string(),
                    serde_json::Value::String(vrl.to_string()),
                );
            }

            // 添加服务器参数（根据类型）/ Add server parameters based on type
            match config {
                MCPServerConfig::Stdio(stdio_config) => {
                    if let Ok(params_json) = serde_json::to_value(&stdio_config.server_parameters) {
                        server_info.insert("server_parameters".to_string(), params_json);
                    }
                }
                MCPServerConfig::Sse(sse_config) => {
                    if let Ok(params_json) = serde_json::to_value(&sse_config.server_parameters) {
                        server_info.insert("server_parameters".to_string(), params_json);
                    }
                }
                MCPServerConfig::Http(http_config) => {
                    if let Ok(params_json) = serde_json::to_value(&http_config.server_parameters) {
                        server_info.insert("server_parameters".to_string(), params_json);
                    }
                }
            }

            // key = bundle_id（server 唯一身份，协议 0.3.0 #18）；name 已作为 value 字段暴露。
            result.insert(
                bundle_id.to_string(),
                serde_json::Value::Object(server_info),
            );
        }

        serde_json::Value::Object(result)
    }

    /// 获取可用工具列表 / Get available tools list
    ///
    /// 与 [`validate_tool_call`](Self::validate_tool_call) **共享同一份** `tool_routes`：每项
    /// 产出 `SMCPTool.name = exposed_tool_name`（`{bundle_id}__{alias ?? 原始名}`）。原始工具名只从 `route`
    /// 读取（永不从 exposed 名 split 反解，协议 0.3.0 单射性）。
    ///
    /// 本方法丢弃每项的 `bundle_id`；wire 产出需要 [`SMCPTool`](smcp::SMCPTool) 的 `bundle_id`（协议 0.3.0
    /// D1 / #136）时改用 `list_available_tools_with_bundle_id`（本 crate 内部）。
    pub async fn list_available_tools(&self) -> Vec<Tool> {
        self.list_available_tools_with_bundle_id()
            .await
            .into_iter()
            .map(|(_, tool)| tool)
            .collect()
    }

    /// 同 [`list_available_tools`](Self::list_available_tools)，但每项额外携带其**解析后** `bundle_id`
    /// （= `route.bundle_id`，恒非空）。
    ///
    /// 供 wire 产出 [`SMCPTool.bundle_id`](smcp::SMCPTool) 用（协议 0.3.0 D1 / #136）：Agent 据此把工具
    /// 归属回具体 server，**MUST NOT** 切分 exposed name 的 `__` 前缀反推——故 bundle_id 只从 `route` 取，
    /// 与 `name`（exposed 名）在同一循环内配对，无二次解析。
    pub(crate) async fn list_available_tools_with_bundle_id(&self) -> Vec<(BundleId, Tool)> {
        let mut tools = Vec::new();
        let routes = self.tool_routes.read().await;

        // 每 bundle_id 仅拉一次 tools/list，跨该 server 的多个 routed tool 复用（对齐 Python `available_tools`
        // 的 `servers_cached_tools`；修 #91：此前每 tool 都调 list_tools → N 工具 = N 次冗余往返）。「每 server
        // 一次」仅在 list_tools **成功**时成立：持续报错则不写缓存、后续 routed tool 会重试（吞错、跳过、不 panic）。
        let mut server_tools_cache: HashMap<BundleId, Vec<Tool>> = HashMap::new();

        for (exposed_name, route) in routes.iter() {
            let bundle_id = &route.bundle_id;
            let client = {
                let clients = self.active_clients.read().await;
                clients.get(bundle_id).cloned()
            };
            let Some(client) = client else { continue };

            // 该 server 首见 → 拉一次并缓存；拉取失败 → 跳过（保留吞错、跳过语义）。
            if !server_tools_cache.contains_key(bundle_id) {
                match client.list_tools().await {
                    Ok(list) => {
                        server_tools_cache.insert(bundle_id.clone(), list);
                    }
                    Err(_) => continue,
                }
            }
            let tool_list = &server_tools_cache[bundle_id];

            // 原始工具名只从 route 取（不从 exposed 名解析）/ original tool name comes from the route only。
            let original_name = &route.original_tool_name;

            // 从缓存查匹配项；命中则产出**改名副本**（exposed 名，缓存被复用勿原地 mutate；与 Python 一致）。
            if let Some(tool) = tool_list
                .iter()
                .find(|t| t.name.as_ref() == original_name.as_str())
            {
                let mut display_tool = tool.clone();
                display_tool.name = exposed_name.clone().into();

                // 合并工具元数据 / Merge tool metadata
                let config = {
                    let configs = self.servers_config.read().await;
                    configs.get(bundle_id).cloned()
                };
                if let Some(config) = config {
                    if let Some(tool_meta) = self.merged_tool_meta(&config, original_name) {
                        if display_tool.meta.is_none() {
                            display_tool.meta = Some(rmcp::model::Meta::new());
                        }
                        if let Some(ref mut meta) = display_tool.meta {
                            meta.insert(
                                A2C_TOOL_META.to_string(),
                                serde_json::to_value(tool_meta).unwrap(),
                            );
                        }
                    }
                }

                tools.push((bundle_id.clone(), display_tool));
            }
        }

        tools
    }

    /// 活动客户端快照，附各 server **人类可读名**（`active_clients` 键为 `bundle_id`）/ snapshot tagged with names。
    ///
    /// 仅供**需要展示名**的场合（如 `get_windows_details` 的 `.1`、诊断日志）——身份/分组/寻址一律用
    /// `bundle_id`，**勿**用本 helper 的名做键（协议 §身份正交性：`name` 允许碰撞）。
    /// 极端情况下（config 已移除但 client 尚存）回退用 `bundle_id` 作名。
    async fn active_clients_by_name(&self) -> Vec<(ServerName, StdArc<dyn MCPClientProtocol>)> {
        let clients = self.active_clients.read().await;
        let configs = self.servers_config.read().await;
        clients
            .iter()
            .map(|(bundle_id, client)| {
                let name = configs
                    .get(bundle_id)
                    .map(|cfg| cfg.name().to_string())
                    .unwrap_or_else(|| bundle_id.to_string());
                (name, client.clone())
            })
            .collect()
    }

    /// 活动客户端快照，附 `bundle_id`（身份键）+ 展示名。供需要身份的窗口枚举路径共用。
    ///
    /// 与 [`active_clients_by_name`](Self::active_clients_by_name) 的区别：保留 `bundle_id`（server 唯一身份），
    /// 仅把展示名作为 `.1` 附带——身份/分组/寻址一律用 `bundle_id`（协议 §身份正交性：`name` 允许碰撞）。
    /// 极端情况下（config 已移除但 client 尚存）展示名回退用 `bundle_id` 作名。
    async fn active_clients_with_identity(
        &self,
    ) -> Vec<(BundleId, ServerName, StdArc<dyn MCPClientProtocol>)> {
        let clients = self.active_clients.read().await;
        let configs = self.servers_config.read().await;
        clients
            .iter()
            .map(|(bundle_id, client)| {
                let name = configs
                    .get(bundle_id)
                    .map(|c| c.name().to_string())
                    .unwrap_or_else(|| bundle_id.to_string());
                (bundle_id.clone(), name, client.clone())
            })
            .collect()
    }

    /// 列出所有窗口资源 / List all window resources
    /// 聚合所有活动客户端的 window:// 资源，可选按 URI 完全匹配过滤
    /// Aggregates window:// resources from all active clients, optionally filtered by exact URI match
    pub async fn list_all_windows(&self, window_uri: Option<&str>) -> Vec<(ServerName, Resource)> {
        let clients = self.active_clients_by_name().await;

        let mut results = Vec::new();
        for (server_name, client) in clients {
            match client.list_windows().await {
                Ok(windows) => {
                    for resource in windows {
                        if let Some(uri_filter) = window_uri {
                            if resource.uri.as_str() != uri_filter {
                                continue;
                            }
                        }
                        results.push((server_name.clone(), resource));
                    }
                }
                Err(e) => {
                    warn!(
                        "Failed to list windows from server '{}': {}",
                        server_name, e
                    );
                }
            }
        }
        results
    }

    /// 枚举所有窗口资源，携带**稳定身份 `bundle_id`** + 展示名，**不读取窗口内容**。
    ///
    /// Enumerate all window resources tagged with **stable identity `bundle_id`** + display name,
    /// **without reading window contents**.
    ///
    /// 介于 [`list_all_windows`](Self::list_all_windows)（仅展示名、丢身份）与
    /// [`get_windows_details`](Self::get_windows_details)（携带身份但 eager-read 且读取失败丢窗）之间：
    /// 只调用 [`list_windows`](MCPClientProtocol::list_windows)（resources/list），**从不调用**
    /// [`get_window_detail`](MCPClientProtocol::get_window_detail)（resources/read）。因此：
    /// - 两个展示名相同、`bundle_id` 不同的 server，其窗口均无歧义返回（协议 §身份正交性）。
    /// - 单个窗口 `resources/read` 失败**不影响**该窗口或其余窗口的枚举结果（本方法压根不读取）。
    ///
    /// `window://` host 属 MCP 自选、透传不解释（正交）。`window_uri` 为 `Some` 时按 URI 完全匹配过滤。
    /// 需读取单个窗口详情请用 [`get_window_detail`](Self::get_window_detail)。
    pub async fn list_windows_with_identity(
        &self,
        window_uri: Option<&str>,
    ) -> Vec<(BundleId, ServerName, Resource)> {
        let entries = self.active_clients_with_identity().await;
        let mut results = Vec::new();
        for (bundle_id, server_name, client) in entries {
            let windows = match client.list_windows().await {
                Ok(w) => w,
                Err(e) => {
                    warn!(
                        "Failed to list windows from server '{}': {}",
                        server_name, e
                    );
                    continue;
                }
            };
            for resource in windows {
                if let Some(uri_filter) = window_uri {
                    if resource.uri.as_str() != uri_filter {
                        continue;
                    }
                }
                results.push((bundle_id.clone(), server_name.clone(), resource));
            }
        }
        results
    }

    /// 枚举活跃 MCP Server 的 `skill://` 资源（附 server 归属），**完整消费 cursor 翻页直至末尾**。
    /// Enumerate `skill://` resources from active MCP servers (with owning server), exhausting cursor pages。
    ///
    /// 与 [`list_resources_page`](MCPClientProtocol::list_resources_page)（单页、Agent 控制翻页）不同：
    /// SKILL 物化由 Computer 主导，须拿到**全量** `skill://` 集合，故在此完整消费翻页（协议 skill.md §12）。
    /// 未声明 `resources` 能力或枚举出错的 server **跳过**（记 ERROR、不中断其余），对齐「SKILL 通道不使用
    /// 4015——无 resources 能力的 server 在物化阶段即被排除」（skill.md §1.5）。
    /// Unlike `list_resources_page` (single-page, Agent-driven): Computer-driven SKILL materialization needs
    /// the full `skill://` set, so pages are exhausted here. Servers lacking `resources` or erroring are skipped.
    ///
    /// `bundle_id` 给定则仅枚举该 server（用于 ResourceListChanged 单 server 重枚举）。
    ///
    /// **协议 0.3.0 §身份正交性（#127）**：标注与过滤均用 `bundle_id`（`active_clients` 键，server 唯一
    /// 身份），**不经 name 解析**——mcp 形态 SKILL 的 name / `source` / 磁盘落点全部由它构成（skill.md
    /// §1.3），退回 display 名会让两个同名 server 撞名、令其一的 SKILL 隐身。对齐 [`list_resources`] 的
    /// bundle_id 直查先例。
    ///
    /// [`list_resources`]: Self::list_resources
    pub async fn list_skill_resources(&self, bundle_id: Option<&str>) -> Vec<(BundleId, Resource)> {
        // bundle_id 直查/过滤 active_clients（身份键 == 键，不经 name 解析）。
        let clients: Vec<(BundleId, StdArc<dyn MCPClientProtocol>)> = self
            .active_clients
            .read()
            .await
            .iter()
            .filter(|(bid, _)| bundle_id.is_none() || bundle_id == Some(bid.as_str()))
            .map(|(bid, client)| (bid.clone(), client.clone()))
            .collect();

        let mut results = Vec::new();
        for (sname, client) in clients {
            let mut cursor: Option<String> = None;
            let mut pages = 0usize;
            loop {
                match client.list_resources_page(cursor.clone()).await {
                    Ok((page, next)) => {
                        for resource in page {
                            if resource.uri.starts_with(SKILL_URI_PREFIX) {
                                results.push((sname.clone(), resource));
                            }
                        }
                        pages += 1;
                        match next {
                            Some(c) => cursor = Some(c),
                            None => break,
                        }
                        if pages >= MAX_SKILL_LIST_PAGES {
                            error!(
                                "list_skill_resources: server '{}' exceeded {} pages \
                                 (non-terminating cursor?); aborting enumeration for this server",
                                sname, MAX_SKILL_LIST_PAGES
                            );
                            break;
                        }
                    }
                    Err(e) => {
                        // 未声明 resources 能力 / 连接异常 / 翻页失败 → 跳过该 server，不阻断其余。
                        error!("Error listing skill resources for '{}': {}", sname, e);
                        break;
                    }
                }
            }
        }
        results
    }

    /// 单页透传指定 MCP Server 的 `resources/list`（`client:get_resources` 路由层）。
    /// Single-page passthrough of a server's `resources/list` (the `client:get_resources` router)。
    ///
    /// **协议 0.3.0 §身份正交性（#18）**：入参 `bundle_id` = `get_resources.mcp_server`（server 唯一身份，**非
    /// name**），**直查** `active_clients`——不经 name 解析（wire 契约已定 bundle_id）。仅作透传：定位 client →
    /// 调 [`list_resources_page`](MCPClientProtocol::list_resources_page)，不做 scheme/元数据过滤、不聚合，翻页由
    /// 调用方经 cursor 控制。未命中 bundle_id → [`ComputerError::McpServerNotFound`]（携 bundle_id，映射 4014）；
    /// 无 `resources` 能力 → [`ComputerError::McpCapabilityNotSupported`]（映射 4015）。
    pub async fn list_resources(
        &self,
        bundle_id: &str,
        cursor: Option<String>,
    ) -> Result<(Vec<Resource>, Option<String>), ComputerError> {
        // bundle_id 寻址：直查 active_clients（身份键 == 键，不经 name 解析）。未命中 → 4014 携 bundle_id。
        // #141 裁决：形参**保留** `&str`，不改 `&BundleId`。本方法是 **wire 入口**（`mcp_server` 由
        // `client:*` 载荷直送，见 `socketio_client.rs`），正确形状是「边界处解析、失败即协议错」——这里
        // 已经 `try_from` + 非法串归入既有的「未命中 → 4014」。改收 `&BundleId` 只会把解析与 4014 映射
        // 推给每个调用方（含 socketio handler），反而分散。R4 约束的是**做 name 启发式**的治理/生命周期
        // API，本方法零启发式、直接按身份键查表 ⇒ 不在其射程内。
        //
        // 同理适用：`list_skill_resources`、`SkillResourceManager` trait 边界。
        let client = match BundleId::try_from(bundle_id) {
            Ok(key) => self.active_clients.read().await.get(&key).cloned(),
            Err(_) => None,
        };
        let client =
            client.ok_or_else(|| ComputerError::McpServerNotFound(bundle_id.to_string()))?;

        match client.list_resources_page(cursor).await {
            Ok(pair) => Ok(pair),
            Err(MCPClientError::CapabilityNotSupported(cap)) => {
                Err(ComputerError::McpCapabilityNotSupported {
                    bundle_id: bundle_id.to_string(),
                    capability: cap,
                })
            }
            Err(e) => Err(ComputerError::ProtocolError(format!(
                "list_resources '{bundle_id}': {e}"
            ))),
        }
    }

    /// 获取所有窗口资源的详情 / Get details of all window resources
    /// 复用 list_all_windows 聚合窗口列表，再逐个获取内容
    /// Reuses list_all_windows to aggregate window list, then fetches each detail
    /// 返回每个 window 详情，附 **`bundle_id`（分组键）** + `server_name`（展示名）（协议 0.3.0 §身份正交性 #18：
    /// desktop 按 bundle_id 分组避免同名 server 误并）。`window://` host 仍属 MCP 自选、透传不解释、正交。
    pub async fn get_windows_details(
        &self,
        window_uri: Option<&str>,
    ) -> Vec<(BundleId, ServerName, Resource, ReadResourceResult)> {
        // (bundle_id, name, client) 快照与 `list_windows_with_identity` 共用：bundle_id = active_clients 键
        // （分组），name 从 servers_config 取（展示）。
        let entries = self.active_clients_with_identity().await;
        let mut results = Vec::new();
        for (bundle_id, server_name, client) in entries {
            let windows = match client.list_windows().await {
                Ok(w) => w,
                Err(e) => {
                    warn!(
                        "Failed to list windows from server '{}': {}",
                        server_name, e
                    );
                    continue;
                }
            };
            for resource in windows {
                if let Some(uri_filter) = window_uri {
                    if resource.uri.as_str() != uri_filter {
                        continue;
                    }
                }
                match client.get_window_detail(resource.clone()).await {
                    Ok(detail) => {
                        results.push((bundle_id.clone(), server_name.clone(), resource, detail));
                    }
                    Err(e) => {
                        warn!(
                            "Failed to get window detail for '{}' from server '{}': {}",
                            resource.uri, server_name, e
                        );
                    }
                }
            }
        }
        results
    }

    /// 读取指定 server 的单个资源（**`bundle_id` 直查**，通用 `resources/read`）/ read a resource by bundle_id。
    ///
    /// 身份键直查 `active_clients`、**不经 name 解析**——[`get_window_detail`](Self::get_window_detail)
    /// （name 寻址公开面）与 [`SkillResourceManager::read_resource`]（#127 起 bundle_id 寻址）共用本实现，
    /// 避免两条读路径分叉。
    async fn read_resource_by_id(
        &self,
        bundle_id: &BundleId,
        resource: Resource,
    ) -> Result<ReadResourceResult, ComputerError> {
        let client = {
            let clients = self.active_clients.read().await;
            clients.get(bundle_id).cloned().ok_or_else(|| {
                ComputerError::InvalidState(format!("Server '{}' not connected", bundle_id))
            })?
        };
        client
            .get_window_detail(resource)
            .await
            .map_err(|e| ComputerError::ProtocolError(format!("Get window detail error: {}", e)))
    }

    /// 获取单个窗口资源的详情 / Get detail of a single window resource
    ///
    /// **名称寻址**公开面（`window://` 通道沿用 display 名寻址，内部解析为身份键后委托
    /// `read_resource_by_id`）。
    pub async fn get_window_detail(
        &self,
        bundle_id: &BundleId,
        resource: Resource,
    ) -> Result<ReadResourceResult, ComputerError> {
        // #141/R4：按身份寻址，删 `active_client_key` name 桥（同名歧义 + 文档化的碰撞路由）。
        {
            let clients = self.active_clients.read().await;
            if !clients.contains_key(bundle_id) {
                return Err(ComputerError::InvalidState(format!(
                    "Server (bundle_id={bundle_id}) not connected"
                )));
            }
        }
        self.read_resource_by_id(bundle_id, resource).await
    }

    /// 合并工具元数据 / Merge tool metadata
    ///
    /// #134：`alias` 天生 per-tool（把某个工具改名），故**只取自具体 `tool_meta[tool]`，绝不从
    /// `default_tool_meta` 继承**——否则同 server 多工具会全塌成同一个 `{bundle_id}__{alias}`、first-wins
    /// 静默丢弃其余。default 的其余字段（`auto_apply`/`tags`/`ret_object_mapper`）仍作默认值照常回落。
    /// 被忽略的 `default_tool_meta.alias` 由 [`refresh_tool_routes`](Self::refresh_tool_routes) 每 server 打一次
    /// 配置诊断 WARN（与 python-sdk #151 同方案）。
    fn merged_tool_meta(&self, config: &MCPServerConfig, tool_name: &str) -> Option<ToolMeta> {
        let specific = config.tool_meta().get(tool_name);
        let default = config.default_tool_meta();

        match (specific, default) {
            (None, None) => None,
            (Some(s), None) => Some(s.clone()),
            (None, Some(d)) => {
                // default 位的 alias 绝不回落到任何工具。
                let mut merged = d.clone();
                merged.alias = None;
                Some(merged)
            }
            (Some(s), Some(d)) => {
                // 浅合并，specific优先 / Shallow merge, specific takes priority
                let mut merged = d.clone();
                if s.auto_apply.is_some() {
                    merged.auto_apply = s.auto_apply;
                }
                // alias 仅源自 specific（含 None 语义）——绝不继承 default。
                merged.alias = s.alias.clone();
                if s.tags.is_some() {
                    merged.tags = s.tags.clone();
                }
                if s.ret_object_mapper.is_some() {
                    merged.ret_object_mapper = s.ret_object_mapper.clone();
                }
                Some(merged)
            }
        }
    }

    /// 启用自动连接 / Enable auto connect
    pub async fn enable_auto_connect(&self) {
        *self.auto_connect.write().await = true;
    }

    /// 禁用自动连接 / Disable auto connect
    pub async fn disable_auto_connect(&self) {
        *self.auto_connect.write().await = false;
    }

    /// 启用自动重连 / Enable auto reconnect
    pub async fn enable_auto_reconnect(&self) {
        *self.auto_reconnect.write().await = true;
    }

    /// 禁用自动重连 / Disable auto reconnect
    pub async fn disable_auto_reconnect(&self) {
        *self.auto_reconnect.write().await = false;
    }

    /// 设置健康检查配置 / Set health check configuration
    pub async fn set_health_check_config(&self, config: HealthCheckConfig) {
        *self.health_check_config.write().await = config;
    }

    /// 获取健康检查配置 / Get health check configuration
    pub async fn get_health_check_config(&self) -> HealthCheckConfig {
        self.health_check_config.read().await.clone()
    }

    /// 设置重连策略 / Set reconnect policy
    pub async fn set_reconnect_policy(&self, policy: ReconnectPolicy) {
        *self.reconnect_policy.write().await = policy;
    }

    /// 获取重连策略 / Get reconnect policy
    pub async fn get_reconnect_policy(&self) -> ReconnectPolicy {
        self.reconnect_policy.read().await.clone()
    }

    /// 启动健康监控 / Start health monitoring
    /// 定期检查所有活动客户端的健康状态，并在检测到故障时自动重连
    /// Periodically checks health of all active clients and auto-reconnects on failure
    pub async fn start_health_monitor(&self) {
        // 先停止现有的监控任务 / Stop existing monitor task first
        self.stop_health_monitor().await;

        let health_config = self.health_check_config.clone();
        let reconnect_policy = self.reconnect_policy.clone();
        let active_clients = self.active_clients.clone();
        let _servers_config = self.servers_config.clone();
        let retry_counts = self.retry_counts.clone();
        let auto_reconnect = self.auto_reconnect.clone();
        let lifecycle_locks = self.lifecycle_locks.clone();

        let handle = tokio::spawn(async move {
            loop {
                let config = health_config.read().await.clone();
                if !config.enabled {
                    // 健康检查禁用，等待一段时间后重新检查配置
                    // Health check disabled, wait and re-check config
                    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                    continue;
                }

                // 获取所有活动客户端 / Get all active clients
                // 键 = bundle_id（`active_clients` 的身份键、非 display 名）——下方循环变量即以 `bundle_id` 命名。
                let clients: Vec<(BundleId, StdArc<dyn MCPClientProtocol>)> = {
                    let clients_guard = active_clients.read().await;
                    clients_guard
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect()
                };

                // 对每个客户端执行健康检查 / Perform health check on each client
                // 注：`clients` 键（`bundle_id`）源自 `active_clients`（`HashMap<BundleId,_>`），即 server 身份键、非 display 名。
                for (bundle_id, client) in clients {
                    let check_result = tokio::time::timeout(
                        std::time::Duration::from_secs(config.timeout_secs),
                        client.health_check(),
                    )
                    .await;

                    let is_healthy = match check_result {
                        Ok(result) => result.is_healthy,
                        Err(_) => {
                            warn!(bundle_id = %bundle_id, "MCP server health check timed out");
                            false
                        }
                    };

                    if !is_healthy {
                        warn!(bundle_id = %bundle_id, "MCP server is unhealthy");

                        // 检查是否启用自动重连 / Check if auto-reconnect is enabled
                        let should_reconnect = *auto_reconnect.read().await;
                        if !should_reconnect {
                            continue;
                        }

                        let policy = reconnect_policy.read().await.clone();
                        let retry_count = *retry_counts.read().await.get(&bundle_id).unwrap_or(&0);

                        if policy.should_retry(retry_count) {
                            let delay = policy.calculate_delay(retry_count);
                            let max_retries_label = if policy.max_retries == 0 {
                                "∞".to_string()
                            } else {
                                policy.max_retries.to_string()
                            };
                            info!(
                                bundle_id = %bundle_id,
                                retry = retry_count + 1,
                                max_retries = %max_retries_label,
                                ?delay,
                                "Attempting to reconnect MCP server"
                            );

                            tokio::time::sleep(delay).await;

                            // Health reconnect mutates the same server lifecycle as explicit
                            // start/stop/update. Reject a stale snapshot after waiting.
                            let lifecycle = lifecycle_locks
                                .get_or_insert_with(bundle_id.clone(), || Mutex::new(()));
                            let _lifecycle_guard = lifecycle.lock().await;
                            let is_current = active_clients
                                .read()
                                .await
                                .get(&bundle_id)
                                .is_some_and(|current| StdArc::ptr_eq(current, &client));
                            if !is_current {
                                retry_counts.write().await.remove(&bundle_id);
                                continue;
                            }

                            // 尝试断开并重新连接 / Try disconnect and reconnect
                            if let Err(e) = client.disconnect().await {
                                warn!(bundle_id = %bundle_id, error = %e, "Failed to disconnect MCP server");
                            }

                            match client.connect().await {
                                Ok(_) => {
                                    info!(bundle_id = %bundle_id, "Successfully reconnected to MCP server");
                                    // 重置重试计数 / Reset retry count
                                    retry_counts.write().await.remove(&bundle_id);
                                }
                                Err(e) => {
                                    error!(bundle_id = %bundle_id, error = %e, "Failed to reconnect to MCP server");
                                    retry_counts
                                        .write()
                                        .await
                                        .insert(bundle_id.clone(), retry_count + 1);
                                }
                            }
                        } else {
                            error!(
                                bundle_id = %bundle_id,
                                max_retries = policy.max_retries,
                                "Max retries reached for MCP server, giving up"
                            );
                            // 可以考虑从活动客户端中移除 / Consider removing from active clients
                        }
                    } else {
                        // 健康检查通过，重置重试计数 / Health check passed, reset retry count
                        let mut retries = retry_counts.write().await;
                        retries.remove(&bundle_id);
                        debug!(bundle_id = %bundle_id, "MCP server is healthy");
                    }
                }

                // 等待下一次健康检查 / Wait for next health check
                tokio::time::sleep(std::time::Duration::from_secs(config.interval_secs)).await;
            }
        });

        *self.health_monitor_handle.write().await = Some(handle);
        info!("Health monitor started");
    }

    /// 停止健康监控 / Stop health monitoring
    pub async fn stop_health_monitor(&self) {
        if let Some(handle) = self.health_monitor_handle.write().await.take() {
            handle.abort();
            info!("Health monitor stopped");
        }
    }

    /// 检查单个服务器的健康状态（**bundle_id 寻址**）/ Check health of a single server by bundle_id（#141/R4）。
    pub async fn check_server_health(&self, bundle_id: &BundleId) -> Option<HealthCheckResult> {
        let clients = self.active_clients.read().await;
        if let Some(client) = clients.get(bundle_id) {
            let config = self.health_check_config.read().await;
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(config.timeout_secs),
                client.health_check(),
            )
            .await;

            match result {
                Ok(health_result) => Some(health_result),
                Err(_) => Some(HealthCheckResult {
                    is_healthy: false,
                    checked_at: std::time::Instant::now(),
                    error: Some("Health check timed out".to_string()),
                    response_time_ms: None,
                }),
            }
        } else {
            None
        }
    }

    /// 检查所有服务器的健康状态 / Check health of all servers
    ///
    /// 返回 map 以**身份键 `bundle_id`** 为键（#127；此前为 display 名——同名 server 会在 map 里折叠、
    /// **丢一条**健康结果，令不健康者被同名健康者静默掩盖）。
    pub async fn check_all_health(&self) -> HashMap<BundleId, HealthCheckResult> {
        let mut results = HashMap::new();
        // 以 (bundle_id, client) 快照迭代——键即身份键。
        let clients: Vec<(BundleId, StdArc<dyn MCPClientProtocol>)> = self
            .active_clients
            .read()
            .await
            .iter()
            .map(|(bid, client)| (bid.clone(), client.clone()))
            .collect();

        let config = self.health_check_config.read().await.clone();

        for (bundle_id, client) in clients {
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(config.timeout_secs),
                client.health_check(),
            )
            .await;

            let health_result = match result {
                Ok(hr) => hr,
                Err(_) => HealthCheckResult {
                    is_healthy: false,
                    checked_at: std::time::Instant::now(),
                    error: Some("Health check timed out".to_string()),
                    response_time_ms: None,
                },
            };

            results.insert(bundle_id, health_result);
        }

        results
    }

    /// 获取重试计数 / Get retry counts
    ///
    /// 键 = **`bundle_id`**（身份键），与 [`check_all_health`](Self::check_all_health) 一致（#127 起两者同键；
    /// 此前本函数已是 bundle_id 键却无文档说明，与紧邻的 name 键 `check_all_health` 同为 `HashMap<String, _>`、
    /// 类型上无从分辨）。
    pub async fn get_retry_counts(&self) -> HashMap<BundleId, u32> {
        self.retry_counts.read().await.clone()
    }

    /// 重置特定服务器的重试计数（**bundle_id 寻址**）/ Reset retry count by bundle_id（#141/R4）。
    pub async fn reset_retry_count(&self, bundle_id: &BundleId) {
        self.retry_counts.write().await.remove(bundle_id);
    }

    /// 重置所有重试计数 / Reset all retry counts
    pub async fn reset_all_retry_counts(&self) {
        self.retry_counts.write().await.clear();
    }
}

/// SKILL staging 接缝实现（#74 INT-04）：把 manager 的 rmcp-typed 枚举/读取适配成 staging 层
/// 解耦类型 [`McpResource`] + 字节，供 [`stage_mcp_skills`](crate::skills::stage_mcp_skills) 物化消费。
/// The SKILL staging seam: adapts the manager's rmcp-typed enumeration/read into staging's decoupled
/// [`McpResource`] + bytes, consumed by `stage_mcp_skills`。
#[async_trait::async_trait]
impl SkillResourceManager for MCPServerManager {
    async fn list_skill_resources(
        &self,
        bundle_id: Option<&str>,
    ) -> Result<Vec<(String, McpResource)>, SkillStagingError> {
        let pairs = MCPServerManager::list_skill_resources(self, bundle_id).await;
        let mut out = Vec::with_capacity(pairs.len());
        for (bid, resource) in pairs {
            // rmcp `Resource._meta`（`Option<Meta(JsonObject)>`）→ staging 的 `Map<String, Value>`。
            let meta = resource.meta.clone().map(|m| m.0).unwrap_or_default();
            out.push((
                // trait 边界收 `String`（`SkillResourceManager` 定义在 skills 模块）。#141 裁决：**保留**
                // ——见 `list_resources` 处的同一理由（值已是身份键、零 name 启发式，改型只搬运不消错）。
                bid.into_string(),
                McpResource {
                    uri: resource.uri.clone(),
                    meta,
                },
            ));
        }
        Ok(out)
    }

    async fn read_resource(
        &self,
        bundle_id: &str,
        uri: &str,
    ) -> Result<Vec<u8>, SkillStagingError> {
        // 复用 manager 的 bundle_id 直查通用 read（#127：身份寻址，不经 name 解析——`active_client_key`
        // 的 name-first 回退会在「某 server 的 display 名恰等于另一 server 的 bundle_id」时路由到错的 server）。
        // Reuse the manager's bundle_id-direct generic read (no name resolution).
        let resource = make_resource(uri, uri, None, None);
        // trait 边界仍收 `&str`（→ #132/#141）；非法 bundle_id 串必然不是活动键 → 与「未连接」同义。
        let key = BundleId::try_from(bundle_id).map_err(|e| {
            SkillStagingError(format!("read_resource '{uri}' from '{bundle_id}': {e}"))
        })?;
        let result = self
            .read_resource_by_id(&key, resource)
            .await
            .map_err(|e| {
                SkillStagingError(format!("read_resource '{uri}' from '{bundle_id}': {e}"))
            })?;

        // 拼接 content blocks → 字节：文本按 UTF-8，二进制按 base64（MCP 标准编码）解码。
        // Concatenate content blocks → bytes: text as UTF-8, blob as standard-base64.
        let mut bytes = Vec::new();
        for content in result.contents {
            match content {
                ResourceContents::TextResourceContents { text, .. } => {
                    bytes.extend_from_slice(text.as_bytes());
                }
                ResourceContents::BlobResourceContents { blob, .. } => {
                    use base64::Engine as _;
                    let decoded = base64::engine::general_purpose::STANDARD
                        .decode(blob.as_bytes())
                        .map_err(|e| SkillStagingError(format!("base64 decode '{uri}': {e}")))?;
                    bytes.extend_from_slice(&decoded);
                }
                _ => {
                    return Err(SkillStagingError(format!(
                        "unsupported resource content from '{bundle_id}' at '{uri}'"
                    )));
                }
            }
        }
        Ok(bytes)
    }
}

impl MCPServerManager {
    /// Return the authorization state of a protected HTTP MCP server.
    pub async fn oauth_status(&self, bundle_id: &BundleId) -> Result<OAuthStatus, OAuthError> {
        let lifecycle = self.lifecycle_lock(bundle_id);
        let _lifecycle_guard = lifecycle.lock().await;
        let client = self.oauth_client_for(bundle_id).await?;
        client.oauth_status().await
    }

    /// Start Authorization Code + PKCE. The caller opens the returned URL.
    pub async fn create_oauth_flow(
        &self,
        bundle_id: &BundleId,
        request: OAuthBeginRequest,
    ) -> Result<OAuthFlow, OAuthError> {
        let lifecycle = self.lifecycle_lock(bundle_id);
        let _lifecycle_guard = lifecycle.lock().await;
        let client = self.oauth_client_for(bundle_id).await?;
        client.create_oauth_flow(request).await
    }

    /// Compatibility facade that waits for [`OAuthFlow::launch`].
    pub async fn begin_oauth(
        &self,
        bundle_id: &BundleId,
        request: OAuthBeginRequest,
    ) -> Result<OAuthLaunch, OAuthError> {
        self.create_oauth_flow(bundle_id, request)
            .await?
            .launch()
            .await
    }

    /// Complete an authorization callback and return its structured outcome.
    pub async fn complete_oauth(
        &self,
        bundle_id: &BundleId,
        callback: OAuthCallback,
    ) -> Result<OAuthFlowOutcome, OAuthError> {
        self.oauth_flow_for_callback(bundle_id)
            .await?
            .complete(callback)
            .await
    }

    /// Terminate a pending browser flow, delete its PKCE/CSRF state, and return the resulting status.
    pub async fn cancel_oauth(
        &self,
        bundle_id: &BundleId,
        cancellation: OAuthCancellation,
    ) -> Result<OAuthFlowOutcome, OAuthError> {
        self.oauth_flow_for_callback(bundle_id)
            .await?
            .cancel_compat(cancellation)
            .await
    }

    pub(crate) async fn oauth_flow_for_callback(
        &self,
        bundle_id: &BundleId,
    ) -> Result<OAuthFlow, OAuthError> {
        let client = {
            let lifecycle = self.lifecycle_lock(bundle_id);
            let _lifecycle_guard = lifecycle.lock().await;
            self.oauth_client_for_callback(bundle_id).await?
        };
        client.active_oauth_flow()
    }

    /// Remove stored tokens and pending authorization state.
    pub async fn clear_oauth(&self, bundle_id: &BundleId) -> Result<(), OAuthError> {
        let lifecycle = self.lifecycle_lock(bundle_id);
        let _lifecycle_guard = lifecycle.lock().await;
        let client = self.oauth_client_for(bundle_id).await?;
        client.clear_oauth().await
    }

    async fn oauth_client_for(
        &self,
        bundle_id: &BundleId,
    ) -> Result<Arc<HttpMCPClient>, OAuthError> {
        if let Some(client) = self.oauth_clients.read().await.get(bundle_id).cloned() {
            return Ok(client);
        }
        let config = self
            .servers_config
            .read()
            .await
            .get(bundle_id)
            .cloned()
            .ok_or(OAuthError::NotConfigured)?;
        let MCPServerConfig::Http(http) = config else {
            return Err(OAuthError::UnsupportedTransport);
        };
        if http.auth_policy == Some(HttpAuthPolicy::Disabled) {
            return Err(OAuthError::NotConfigured);
        }
        let client = Arc::new(
            HttpMCPClient::new(http.server_parameters)
                .with_oauth_context(
                    bundle_id.clone(),
                    Arc::clone(&self.oauth_credential_store),
                    self.oauth_events.clone(),
                )
                .with_auth_policy(
                    http.auth_policy,
                    http.oauth,
                    self.secret_resolver.read().await.clone(),
                )?,
        );
        let mut oauth_clients = self.oauth_clients.write().await;
        Ok(oauth_clients
            .entry(bundle_id.clone())
            .or_insert(client)
            .clone())
    }

    async fn oauth_client_for_callback(
        &self,
        bundle_id: &BundleId,
    ) -> Result<Arc<HttpMCPClient>, OAuthError> {
        if let Some(client) = self.oauth_clients.read().await.get(bundle_id).cloned() {
            if client.oauth_callback_configured() {
                return Ok(client);
            }
        }

        // Preserve the facade's distinction between an unknown/non-OAuth server and a callback
        // for which no live flow exists, without recreating a retired client from stale config.
        let config = self
            .servers_config
            .read()
            .await
            .get(bundle_id)
            .cloned()
            .ok_or(OAuthError::NotConfigured)?;
        let MCPServerConfig::Http(http) = config else {
            return Err(OAuthError::UnsupportedTransport);
        };
        if http.auth_policy == Some(HttpAuthPolicy::Disabled)
            || (http.auth_policy.is_none() && http.oauth.is_none())
            || http.auth_policy == Some(HttpAuthPolicy::Auto)
        {
            return Err(OAuthError::NotConfigured);
        }
        Err(OAuthError::StateMismatch)
    }
}

impl Default for MCPServerManager {
    fn default() -> Self {
        Self::new()
    }
}

/// 测试支撑：可控分页/失败的假 MCP client + 资源/注入助手，`pub(crate)` 供 manager 与 computer 两处
/// 集成测试共用（单一 mock 真源）/ test-support mock + helpers shared by manager and computer tests。
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// 测试夹具：构造合法 [`BundleId`]（非法字面量在此 panic —— 夹具写错须立刻暴露，而非静默成 `String`）。
    ///
    /// #130：夹具此前直写 `String`；改型后身份键须经构造校验，这也顺带守护「夹具取值必须是合法 bundle_id」。
    pub(crate) fn bid(s: &str) -> BundleId {
        BundleId::try_from(s).expect("测试夹具的 bundle_id 字面量必须合法")
    }

    /// 受控分页的假 MCP client：按 cursor 顺序返回各页，可注入翻页失败 / 能力缺失 / read 文本。
    /// Controllable fake MCP client paginating canned pages, with injectable failure/cap-fail/read text.
    pub(crate) struct MockSkillClient {
        pub(crate) pages: Vec<Vec<Resource>>,
        pub(crate) fail: bool,
        /// `list_resources_page` 返回 `CapabilityNotSupported`（模拟无 `resources` 能力）。
        pub(crate) cap_fail: bool,
        pub(crate) read_text: String,
    }

    #[async_trait::async_trait]
    impl MCPClientProtocol for MockSkillClient {
        fn state(&self) -> ClientState {
            ClientState::Connected
        }
        async fn connect(&self) -> Result<(), MCPClientError> {
            Ok(())
        }
        async fn disconnect(&self) -> Result<(), MCPClientError> {
            Ok(())
        }
        async fn list_tools(&self) -> Result<Vec<Tool>, MCPClientError> {
            Ok(vec![])
        }
        async fn call_tool(
            &self,
            _tool: &str,
            _params: Value,
        ) -> Result<CallToolResult, MCPClientError> {
            Err(MCPClientError::ProtocolError("n/a".into()))
        }
        async fn list_windows(&self) -> Result<Vec<Resource>, MCPClientError> {
            Ok(vec![])
        }
        async fn list_resources_page(
            &self,
            cursor: Option<String>,
        ) -> Result<(Vec<Resource>, Option<String>), MCPClientError> {
            if self.cap_fail {
                return Err(MCPClientError::CapabilityNotSupported("resources".into()));
            }
            if self.fail {
                return Err(MCPClientError::ProtocolError("boom".into()));
            }
            let idx: usize = cursor.as_deref().and_then(|c| c.parse().ok()).unwrap_or(0);
            match self.pages.get(idx) {
                Some(page) => {
                    let next = if idx + 1 < self.pages.len() {
                        Some((idx + 1).to_string())
                    } else {
                        None
                    };
                    Ok((page.clone(), next))
                }
                None => Ok((vec![], None)),
            }
        }
        async fn get_window_detail(
            &self,
            _resource: Resource,
        ) -> Result<ReadResourceResult, MCPClientError> {
            Ok(ReadResourceResult::new(vec![ResourceContents::text(
                self.read_text.clone(),
                "skill://x",
            )]))
        }
        async fn subscribe_window(&self, _resource: Resource) -> Result<(), MCPClientError> {
            Ok(())
        }
        async fn unsubscribe_window(&self, _resource: Resource) -> Result<(), MCPClientError> {
            Ok(())
        }
    }

    /// 构造带 `_meta.source` 的 `skill://` 资源（mount_dir 固定占位）/ a `skill://` resource with `_meta.source`。
    pub(crate) fn skill_resource(uri: &str, source: Option<&str>) -> Resource {
        skill_resource_mounted(uri, source, "/tmp/mount")
    }

    /// 同上但 mount_dir 取真实路径（供 mounted 物化 happy-path 测试）/ with a real mount_dir for materialization。
    pub(crate) fn skill_resource_mounted(
        uri: &str,
        source: Option<&str>,
        mount_dir: &str,
    ) -> Resource {
        use rmcp::model::Meta;
        let mut resource = Resource::new(uri, "skill");
        if let Some(src) = source {
            let mut m = serde_json::Map::new();
            m.insert("source".into(), Value::String(src.to_string()));
            m.insert("mount_dir".into(), Value::String(mount_dir.to_string()));
            resource.meta = Some(Meta(m));
        }
        resource
    }

    /// 把假 client 注入 manager 的 `active_clients`（键 = `bundle_id`）/ inject a fake client。
    pub(crate) async fn inject(
        manager: &MCPServerManager,
        bundle_id: &BundleId,
        client: MockSkillClient,
    ) {
        manager
            .active_clients
            .write()
            .await
            .insert(bundle_id.clone(), StdArc::new(client));
    }

    /// 构造带**显式** bundle_id 的 stdio 配置（其余占位）/ a stdio config with an explicit bundle_id。
    pub(crate) fn stdio_cfg_with_bundle(name: &str, bundle_id: Option<&str>) -> MCPServerConfig {
        let mut c = StdioServerConfig::new(
            name,
            StdioServerParameters {
                command: "echo".to_string(),
                args: vec![],
                env: HashMap::new(),
                cwd: None,
            },
        );
        c.bundle_id = bundle_id.map(bid);
        MCPServerConfig::Stdio(c)
    }

    /// 把一条 config 注入 `servers_config`（键 = `bundle_id`）/ inject a server config keyed by bundle_id。
    ///
    /// 供需要 **display 名 ≠ bundle_id**（含两个 server 共用 display 名）的测试构造身份/展示分离的场景——
    /// 仅 [`inject`] 时 `servers_config` 为空，展示名会回退成 bundle_id，测不出两者分歧。
    pub(crate) async fn inject_config(
        manager: &MCPServerManager,
        bundle_id: &BundleId,
        config: MCPServerConfig,
    ) {
        manager
            .servers_config
            .write()
            .await
            .insert(bundle_id.clone(), config);
    }

    // ── INT-02 #70：可取消调用的共享假 client（manager 三态 + computer 端到端共用）──────────

    /// 可配置行为的假 MCP client，覆盖可取消调用三态（用默认 trait 实现的 select-drop 竞速）。经
    /// [`inject_callable`] 注入，供 manager（`call_tool_cancellable` 三态）与 computer
    /// （`execute_tool_cancellable` / `acancel_tool` 端到端）测试共享。字段私有——构造走 `inject_callable`。
    pub(crate) struct CancelMockClient {
        behavior: CancelBehavior,
    }

    /// [`CancelMockClient`] 的注入行为 / injected behavior for the cancellable mock。
    pub(crate) enum CancelBehavior {
        /// 立即返回成功结果 / return Ok immediately.
        CompleteOk,
        /// 永不返回（模拟在途阻塞——由取消令牌就地中断）/ never resolves (interrupted by cancel token).
        BlockForever,
        /// 睡眠后返回（配合短 timeout 触发 manager 级超时）/ sleep then Ok.
        Sleep(std::time::Duration),
    }

    #[async_trait::async_trait]
    impl MCPClientProtocol for CancelMockClient {
        fn state(&self) -> ClientState {
            ClientState::Connected
        }
        async fn connect(&self) -> Result<(), MCPClientError> {
            Ok(())
        }
        async fn disconnect(&self) -> Result<(), MCPClientError> {
            Ok(())
        }
        async fn list_tools(&self) -> Result<Vec<Tool>, MCPClientError> {
            Ok(vec![])
        }
        async fn call_tool(
            &self,
            _tool: &str,
            _params: Value,
        ) -> Result<CallToolResult, MCPClientError> {
            match &self.behavior {
                CancelBehavior::CompleteOk => {
                    Ok(CallToolResult::success(vec![Content::text("done")]))
                }
                CancelBehavior::BlockForever => std::future::pending().await,
                CancelBehavior::Sleep(d) => {
                    tokio::time::sleep(*d).await;
                    Ok(CallToolResult::success(vec![Content::text("late")]))
                }
            }
        }
        async fn list_windows(&self) -> Result<Vec<Resource>, MCPClientError> {
            Ok(vec![])
        }
        async fn list_resources_page(
            &self,
            _cursor: Option<String>,
        ) -> Result<(Vec<Resource>, Option<String>), MCPClientError> {
            Ok((vec![], None))
        }
        async fn get_window_detail(
            &self,
            _resource: Resource,
        ) -> Result<ReadResourceResult, MCPClientError> {
            Err(MCPClientError::ProtocolError("n/a".into()))
        }
        async fn subscribe_window(&self, _r: Resource) -> Result<(), MCPClientError> {
            Ok(())
        }
        async fn unsubscribe_window(&self, _r: Resource) -> Result<(), MCPClientError> {
            Ok(())
        }
    }

    /// 注入可取消假 client + exposed→route 路由，使 `validate_tool_call` 可解析（供 computer 端到端测试）。
    /// 测试中 `server` 同时充当 `bundle_id` 与展示名；`tool` 直接作 `exposed_tool_name` 路由键。
    /// Inject a cancellable fake client + an exposed→route entry so `validate_tool_call` resolves.
    pub(crate) async fn inject_callable(
        manager: &MCPServerManager,
        server: &str,
        tool: &str,
        behavior: CancelBehavior,
    ) {
        manager
            .active_clients
            .write()
            .await
            .insert(bid(server), StdArc::new(CancelMockClient { behavior }));
        manager.tool_routes.write().await.insert(
            tool.to_string(),
            ExposedToolRoute {
                bundle_id: bid(server),
                server_name: server.to_string(),
                original_tool_name: tool.to_string(),
                alias: None,
            },
        );
    }

    /// 返回固定工具列表的假 client：仅 `list_tools` 有意义，供 `refresh_tool_mapping` /
    /// `list_available_tools` 的 forbidden/alias 回归测试构造工具来源（对标 Python python-sdk
    /// #106/#107）。A fake client returning a fixed tool list; only `list_tools` is meaningful.
    pub(crate) struct MockToolsClient {
        pub(crate) tools: Vec<Tool>,
    }

    #[async_trait::async_trait]
    impl MCPClientProtocol for MockToolsClient {
        fn state(&self) -> ClientState {
            ClientState::Connected
        }
        async fn connect(&self) -> Result<(), MCPClientError> {
            Ok(())
        }
        async fn disconnect(&self) -> Result<(), MCPClientError> {
            Ok(())
        }
        async fn list_tools(&self) -> Result<Vec<Tool>, MCPClientError> {
            Ok(self.tools.clone())
        }
        async fn call_tool(
            &self,
            _tool: &str,
            _params: Value,
        ) -> Result<CallToolResult, MCPClientError> {
            Err(MCPClientError::ProtocolError("n/a".into()))
        }
        async fn list_windows(&self) -> Result<Vec<Resource>, MCPClientError> {
            Ok(vec![])
        }
        async fn list_resources_page(
            &self,
            _cursor: Option<String>,
        ) -> Result<(Vec<Resource>, Option<String>), MCPClientError> {
            Ok((vec![], None))
        }
        async fn get_window_detail(
            &self,
            _resource: Resource,
        ) -> Result<ReadResourceResult, MCPClientError> {
            Err(MCPClientError::ProtocolError("n/a".into()))
        }
        async fn subscribe_window(&self, _resource: Resource) -> Result<(), MCPClientError> {
            Ok(())
        }
        async fn unsubscribe_window(&self, _resource: Resource) -> Result<(), MCPClientError> {
            Ok(())
        }
    }

    /// 返回一个固定 `window://` 资源 + 空 detail 的假 client，供 `get_windows_details` / `list_windows_with_identity`
    /// 的身份投影与「读取失败不丢窗」测试（#118 / #153）。`fail_detail = true` 让 `get_window_detail` 报错
    /// （`list_windows` 仍成功），用于演练单窗口 `resources/read` 失败。`detail_calls` 计 `get_window_detail`
    /// 调用次数，供 #153 锁死「`list_windows_with_identity` 从不读取」契约（仿 [`CountingToolsClient`]）。
    pub(crate) struct WindowMockClient {
        pub(crate) uri: String,
        pub(crate) fail_detail: bool,
        pub(crate) detail_calls: StdArc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl MCPClientProtocol for WindowMockClient {
        fn state(&self) -> ClientState {
            ClientState::Connected
        }
        async fn connect(&self) -> Result<(), MCPClientError> {
            Ok(())
        }
        async fn disconnect(&self) -> Result<(), MCPClientError> {
            Ok(())
        }
        async fn list_tools(&self) -> Result<Vec<Tool>, MCPClientError> {
            Ok(vec![])
        }
        async fn call_tool(
            &self,
            _tool: &str,
            _params: Value,
        ) -> Result<CallToolResult, MCPClientError> {
            Err(MCPClientError::ProtocolError("n/a".into()))
        }
        async fn list_windows(&self) -> Result<Vec<Resource>, MCPClientError> {
            Ok(vec![make_resource(&self.uri, "w", None, None)])
        }
        async fn list_resources_page(
            &self,
            _cursor: Option<String>,
        ) -> Result<(Vec<Resource>, Option<String>), MCPClientError> {
            Ok((vec![], None))
        }
        async fn get_window_detail(
            &self,
            _resource: Resource,
        ) -> Result<ReadResourceResult, MCPClientError> {
            self.detail_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.fail_detail {
                Err(MCPClientError::ProtocolError("forced read failure".into()))
            } else {
                Ok(ReadResourceResult::new(vec![]))
            }
        }
        async fn subscribe_window(&self, _resource: Resource) -> Result<(), MCPClientError> {
            Ok(())
        }
        async fn unsubscribe_window(&self, _resource: Resource) -> Result<(), MCPClientError> {
            Ok(())
        }
    }

    /// 用给定工具集构造 [`MockToolsClient`] 并注入 `active_clients`；配套 `ServerConfig` 由调用方写入
    /// `servers_config`（`refresh_tool_mapping` 同时读两者）。Inject a `MockToolsClient` into `active_clients`.
    pub(crate) async fn inject_tools(
        manager: &MCPServerManager,
        bundle_id: &BundleId,
        tools: Vec<Tool>,
    ) {
        manager
            .active_clients
            .write()
            .await
            .insert(bundle_id.clone(), StdArc::new(MockToolsClient { tools }));
    }

    /// 计数版 [`MockToolsClient`]：`list_tools` 每次调用 `calls += 1`，供 #91「每 server 仅拉一次
    /// `tools/list`」回归验证。A counting fake whose `list_tools` bumps a shared call counter.
    pub(crate) struct CountingToolsClient {
        pub(crate) tools: Vec<Tool>,
        pub(crate) calls: StdArc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl MCPClientProtocol for CountingToolsClient {
        fn state(&self) -> ClientState {
            ClientState::Connected
        }
        async fn connect(&self) -> Result<(), MCPClientError> {
            Ok(())
        }
        async fn disconnect(&self) -> Result<(), MCPClientError> {
            Ok(())
        }
        async fn list_tools(&self) -> Result<Vec<Tool>, MCPClientError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(self.tools.clone())
        }
        async fn call_tool(
            &self,
            _tool: &str,
            _params: Value,
        ) -> Result<CallToolResult, MCPClientError> {
            Err(MCPClientError::ProtocolError("n/a".into()))
        }
        async fn list_windows(&self) -> Result<Vec<Resource>, MCPClientError> {
            Ok(vec![])
        }
        async fn list_resources_page(
            &self,
            _cursor: Option<String>,
        ) -> Result<(Vec<Resource>, Option<String>), MCPClientError> {
            Ok((vec![], None))
        }
        async fn get_window_detail(
            &self,
            _resource: Resource,
        ) -> Result<ReadResourceResult, MCPClientError> {
            Err(MCPClientError::ProtocolError("n/a".into()))
        }
        async fn subscribe_window(&self, _resource: Resource) -> Result<(), MCPClientError> {
            Ok(())
        }
        async fn unsubscribe_window(&self, _resource: Resource) -> Result<(), MCPClientError> {
            Ok(())
        }
    }

    /// `list_tools` 恒返错误的假 client（供 #91 `list_available_tools` 的 `Err=>continue` 分支回归）/
    /// a fake whose `list_tools` always errors.
    pub(crate) struct ErrToolsClient;

    #[async_trait::async_trait]
    impl MCPClientProtocol for ErrToolsClient {
        fn state(&self) -> ClientState {
            ClientState::Connected
        }
        async fn connect(&self) -> Result<(), MCPClientError> {
            Ok(())
        }
        async fn disconnect(&self) -> Result<(), MCPClientError> {
            Ok(())
        }
        async fn list_tools(&self) -> Result<Vec<Tool>, MCPClientError> {
            Err(MCPClientError::ProtocolError("list_tools boom".into()))
        }
        async fn call_tool(
            &self,
            _tool: &str,
            _params: Value,
        ) -> Result<CallToolResult, MCPClientError> {
            Err(MCPClientError::ProtocolError("n/a".into()))
        }
        async fn list_windows(&self) -> Result<Vec<Resource>, MCPClientError> {
            Ok(vec![])
        }
        async fn list_resources_page(
            &self,
            _cursor: Option<String>,
        ) -> Result<(Vec<Resource>, Option<String>), MCPClientError> {
            Ok((vec![], None))
        }
        async fn get_window_detail(
            &self,
            _resource: Resource,
        ) -> Result<ReadResourceResult, MCPClientError> {
            Err(MCPClientError::ProtocolError("n/a".into()))
        }
        async fn subscribe_window(&self, _resource: Resource) -> Result<(), MCPClientError> {
            Ok(())
        }
        async fn unsubscribe_window(&self, _resource: Resource) -> Result<(), MCPClientError> {
            Ok(())
        }
    }

    /// 注入 [`CountingToolsClient`] 到 `active_clients`，返回 `list_tools` 调用计数句柄 / inject + return counter.
    pub(crate) async fn inject_counting_tools(
        manager: &MCPServerManager,
        name: &str,
        tools: Vec<Tool>,
    ) -> StdArc<std::sync::atomic::AtomicUsize> {
        let calls = StdArc::new(std::sync::atomic::AtomicUsize::new(0));
        manager.active_clients.write().await.insert(
            bid(name),
            StdArc::new(CountingToolsClient {
                tools,
                calls: calls.clone(),
            }),
        );
        calls
    }
}

#[cfg(test)]
#[path = "manager_auto_oauth_private_key_jwt_tests.rs"]
mod manager_auto_oauth_private_key_jwt_tests;

#[cfg(test)]
mod tests {
    use super::test_support::{bid, stdio_cfg_with_bundle};
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::time::{sleep, Duration};

    /// 最小假 client：`call_tool` 返回注入的 transport 错误，用于覆盖 `call_tool` 授权分流（AUTH-01 #23）。
    /// 余方法 trivial。Minimal fake whose `call_tool` returns an injected transport error.
    struct AuthErrClient {
        msg: String,
    }

    #[async_trait::async_trait]
    impl MCPClientProtocol for AuthErrClient {
        fn state(&self) -> ClientState {
            ClientState::Connected
        }
        async fn connect(&self) -> Result<(), MCPClientError> {
            Ok(())
        }
        async fn disconnect(&self) -> Result<(), MCPClientError> {
            Ok(())
        }
        async fn list_tools(&self) -> Result<Vec<Tool>, MCPClientError> {
            Ok(vec![])
        }
        async fn call_tool(
            &self,
            _tool: &str,
            _params: Value,
        ) -> Result<CallToolResult, MCPClientError> {
            Err(MCPClientError::ConnectionError(self.msg.clone()))
        }
        async fn list_windows(&self) -> Result<Vec<Resource>, MCPClientError> {
            Ok(vec![])
        }
        async fn list_resources_page(
            &self,
            _cursor: Option<String>,
        ) -> Result<(Vec<Resource>, Option<String>), MCPClientError> {
            Ok((vec![], None))
        }
        async fn get_window_detail(
            &self,
            _resource: Resource,
        ) -> Result<ReadResourceResult, MCPClientError> {
            Ok(ReadResourceResult::new(vec![]))
        }
        async fn subscribe_window(&self, _resource: Resource) -> Result<(), MCPClientError> {
            Ok(())
        }
        async fn unsubscribe_window(&self, _resource: Resource) -> Result<(), MCPClientError> {
            Ok(())
        }
    }

    struct HealthRaceClient {
        health_started: tokio::sync::Notify,
        release_health: tokio::sync::Notify,
        connect_calls: AtomicUsize,
        disconnect_calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl MCPClientProtocol for HealthRaceClient {
        fn state(&self) -> ClientState {
            ClientState::Connected
        }
        async fn connect(&self) -> Result<(), MCPClientError> {
            self.connect_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn disconnect(&self) -> Result<(), MCPClientError> {
            self.disconnect_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn list_tools(&self) -> Result<Vec<Tool>, MCPClientError> {
            Ok(vec![])
        }
        async fn call_tool(
            &self,
            _tool: &str,
            _params: Value,
        ) -> Result<CallToolResult, MCPClientError> {
            Err(MCPClientError::ProtocolError("n/a".into()))
        }
        async fn list_windows(&self) -> Result<Vec<Resource>, MCPClientError> {
            Ok(vec![])
        }
        async fn list_resources_page(
            &self,
            _cursor: Option<String>,
        ) -> Result<(Vec<Resource>, Option<String>), MCPClientError> {
            Ok((vec![], None))
        }
        async fn get_window_detail(
            &self,
            _resource: Resource,
        ) -> Result<ReadResourceResult, MCPClientError> {
            Ok(ReadResourceResult::new(vec![]))
        }
        async fn subscribe_window(&self, _resource: Resource) -> Result<(), MCPClientError> {
            Ok(())
        }
        async fn unsubscribe_window(&self, _resource: Resource) -> Result<(), MCPClientError> {
            Ok(())
        }
        async fn health_check(&self) -> HealthCheckResult {
            self.health_started.notify_one();
            self.release_health.notified().await;
            HealthCheckResult {
                is_healthy: false,
                checked_at: std::time::Instant::now(),
                error: Some("forced unhealthy".into()),
                response_time_ms: None,
            }
        }
    }

    /// 注入 `AuthErrClient` 到 `active_clients`（auth 分支在 `servers_config` 读取前早返回，仅需此）。
    async fn inject_auth_err(manager: &MCPServerManager, name: &str, msg: &str) {
        manager.active_clients.write().await.insert(
            bid(name),
            StdArc::new(AuthErrClient {
                msg: msg.to_string(),
            }),
        );
    }

    #[tokio::test]
    async fn test_call_tool_upstream_401_yields_auth_result_4006() {
        // 上游 401 → call_tool 不再 Err，而是返回协议形状的授权 CallToolResult（_meta.error_code=4006）。
        let manager = MCPServerManager::new();
        inject_auth_err(&manager, "srv", "HTTP error: 401 Unauthorized").await;

        let r = manager
            .call_tool("srv", "t", serde_json::json!({}), None)
            .await
            .expect("auth error should surface as Ok(CallToolResult), not Err");

        assert_eq!(r.is_error, Some(true));
        let meta = r.meta.as_ref().expect("meta present");
        assert_eq!(meta.get("error_code").and_then(|v| v.as_i64()), Some(4006));
        assert_eq!(meta.get("mcp_server").and_then(|v| v.as_str()), Some("srv"));
        assert!(meta.get("auth_hint").is_some());
    }

    #[tokio::test]
    async fn test_call_tool_non_auth_error_stays_protocol_error() {
        // 非授权上游错误 → 维持通用 ProtocolError（覆盖 classify 的 None 臂）。
        let manager = MCPServerManager::new();
        inject_auth_err(&manager, "srv", "boom: something broke").await;

        let err = manager
            .call_tool("srv", "t", serde_json::json!({}), None)
            .await
            .unwrap_err();
        assert!(
            matches!(err, ComputerError::ProtocolError(_)),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn test_manager_creation() {
        let manager = MCPServerManager::new();
        let status = manager.get_server_status().await;
        assert!(status.is_empty());
    }

    #[tokio::test]
    async fn close_drains_oauth_even_when_active_client_disconnect_fails() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepted = Arc::new(tokio::sync::Notify::new());
        let accepted_by_server = Arc::clone(&accepted);
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            accepted_by_server.notify_one();
            let _stream = stream;
            std::future::pending::<()>().await;
        });

        let manager = MCPServerManager::new();
        let bundle_id = bid("oauth-close-error");
        let client = Arc::new(
            HttpMCPClient::new(HttpServerParameters {
                url: format!("http://127.0.0.1:{port}/mcp"),
                headers: HashMap::new(),
            })
            .with_oauth_context(
                bundle_id.clone(),
                Arc::clone(&manager.oauth_credential_store),
                None,
            )
            .with_oauth(
                crate::oauth::OAuthOptions {
                    resource: None,
                    scopes: vec!["tools.read".to_string()],
                    client_name: Some("A2C Computer".to_string()),
                    mode: crate::oauth::OAuthClientMode::AuthorizationCode {
                        registration: crate::oauth::OAuthClientRegistration::Preregistered {
                            client_id: "oauth-client".to_string(),
                            client_secret_input: None,
                        },
                    },
                },
                None,
            ),
        );
        manager
            .oauth_clients
            .write()
            .await
            .insert(bundle_id.clone(), Arc::clone(&client));
        let active: StdArc<dyn MCPClientProtocol> = client.clone();
        manager
            .active_clients
            .write()
            .await
            .insert(bundle_id, active);
        let flow = client
            .create_oauth_flow(OAuthBeginRequest {
                redirect_uri: "http://127.0.0.1:9876/callback".to_string(),
                required_scope: None,
            })
            .await
            .unwrap();
        accepted.notified().await;

        assert!(manager.close().await.is_err());
        let terminal = tokio::time::timeout(
            Duration::from_secs(1),
            flow.cancel(crate::oauth::OAuthCancellationReason::Cancelled),
        )
        .await
        .expect("close must drain OAuth despite the disconnect error")
        .unwrap();
        assert!(matches!(terminal, OAuthFlowOutcome::Terminated { .. }));
    }

    #[tokio::test]
    async fn test_manager_initialization() {
        let manager = MCPServerManager::new();

        // 创建服务器配置 / Create server configurations
        let configs = vec![
            // STDIO服务器配置 / STDIO server configuration
            MCPServerConfig::Stdio(StdioServerConfig {
                env_file: None,
                bundle_id: None,
                name: "test_stdio".to_string(),
                disabled: false,
                forbidden_tools: vec![],
                tool_meta: HashMap::new(),
                default_tool_meta: None,
                vrl: None,
                server_parameters: StdioServerParameters {
                    command: "echo".to_string(),
                    args: vec!["hello".to_string()],
                    env: HashMap::new(),
                    cwd: None,
                },
            }),
            // HTTP服务器配置 / HTTP server configuration
            MCPServerConfig::Http(HttpServerConfig {
                env_file: None,
                bundle_id: None,
                name: "test_http".to_string(),
                disabled: true, // 禁用此服务器 / Disable this server
                forbidden_tools: vec![],
                tool_meta: HashMap::new(),
                default_tool_meta: None,
                vrl: None,
                oauth: None,
                auth_policy: None,
                server_parameters: HttpServerParameters {
                    url: "http://localhost:8080".to_string(),
                    headers: HashMap::new(),
                },
            }),
        ];

        // 初始化管理器 / Initialize manager
        let result = manager.initialize(configs).await;
        assert!(result.is_ok());

        // 检查状态 / Check status
        let status = manager.get_server_status().await;
        assert_eq!(status.len(), 2);

        // 验证状态 / Verify status（#127：行形态 = (bundle_id, name, is_active, state)）
        let stdio_status = status
            .iter()
            .find(|(_, name, _, _)| name == "test_stdio")
            .unwrap();
        assert!(!stdio_status.2); // 未激活 / Not active

        let http_status = status
            .iter()
            .find(|(_, name, _, _)| name == "test_http")
            .unwrap();
        assert!(!http_status.2); // 未激活 / Not active
    }

    #[tokio::test]
    async fn test_add_server() {
        let manager = MCPServerManager::new();

        // 添加服务器配置 / Add server configuration
        let config = MCPServerConfig::Stdio(StdioServerConfig {
            env_file: None,
            bundle_id: None,
            name: "test_server".to_string(),
            disabled: false,
            forbidden_tools: vec![],
            tool_meta: HashMap::new(),
            default_tool_meta: None,
            vrl: None,
            server_parameters: StdioServerParameters {
                command: "echo".to_string(),
                args: vec![],
                env: HashMap::new(),
                cwd: None,
            },
        });

        let result = manager.add_or_update_server(config).await;
        assert!(result.is_ok());

        // 检查状态 / Check status（`.0` = bundle_id（此处缺省生成 == name）、`.1` = 展示名）
        let status = manager.get_server_status().await;
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].0.as_str(), "test_server");
        assert_eq!(status[0].1, "test_server");
    }

    #[tokio::test]
    async fn concurrent_start_uses_one_client_lifecycle() {
        let manager = MCPServerManager::new();
        let created = Arc::new(AtomicUsize::new(0));
        let factory_created = Arc::clone(&created);
        manager
            .set_client_factory(Some(Arc::new(move |_, _| {
                factory_created.fetch_add(1, Ordering::SeqCst);
                Arc::new(super::test_support::MockSkillClient {
                    pages: vec![],
                    fail: false,
                    cap_fail: false,
                    read_text: String::new(),
                })
            })))
            .await;
        manager
            .add_or_update_server(stdio_cfg_with_bundle("concurrent", Some("concurrent")))
            .await
            .unwrap();
        let bundle_id = bid("concurrent");

        let (first, second) = tokio::join!(
            manager.start_client_by_id(&bundle_id),
            manager.start_client_by_id(&bundle_id)
        );

        assert!(first.is_ok());
        assert!(second.is_ok());
        assert_eq!(created.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn lifecycle_registry_reclaims_inactive_server_slots() {
        let manager = MCPServerManager::new();

        for index in 0..32 {
            let bundle_id = bid(&format!("inactive-lifecycle-{index}"));
            drop(manager.lifecycle_lock(&bundle_id));
        }

        assert!(
            manager.lifecycle_registry_len() <= 1,
            "inactive server lifecycle slots must be reclaimed"
        );
    }

    /// #141：`restart_client_by_id` 对**未声明**的 bundle_id 必须报错，不得静默 `Ok`。
    ///
    /// 修复前的形状与被根治的 `stop` 假回执同源：stop 幂等 no-op → `configs.get(id)` 落空 →
    /// `unwrap_or(false)` 判为「未启用」→ 不 start → `Ok(())`。调用方据此以为「已重启」，实则什么都没发生。
    #[tokio::test]
    async fn restart_unknown_bundle_id_is_error_not_silent_ok_141() {
        let manager = MCPServerManager::new();
        let err = manager
            .restart_client_by_id(&BundleId::try_from("never-declared").unwrap())
            .await
            .expect_err("未声明的 bundle_id MUST NOT 静默 Ok");
        assert!(
            format!("{err}").contains("never-declared"),
            "错误须点名该 bundle_id，实际: {err}"
        );
    }

    /// #141：声明存在但 `disabled` → 停而不起，仍是 `Ok`（尊重停用意图，与「未知 id」区分开）。
    #[tokio::test]
    async fn restart_disabled_but_declared_is_ok_141() {
        let manager = MCPServerManager::new();
        let config = MCPServerConfig::Stdio(StdioServerConfig {
            env_file: None,
            bundle_id: None,
            name: "disabled_server".to_string(),
            disabled: true,
            forbidden_tools: vec![],
            tool_meta: HashMap::new(),
            default_tool_meta: None,
            vrl: None,
            server_parameters: StdioServerParameters {
                command: "echo".to_string(),
                args: vec![],
                env: HashMap::new(),
                cwd: None,
            },
        });
        manager.add_or_update_server(config).await.unwrap();

        manager
            .restart_client_by_id(&BundleId::try_from("disabled_server").unwrap())
            .await
            .expect("已声明但停用 → 停而不起，非错误");
    }

    #[tokio::test]
    async fn test_remove_server() {
        let manager = MCPServerManager::new();

        // 添加服务器 / Add server
        let config = MCPServerConfig::Stdio(StdioServerConfig {
            env_file: None,
            bundle_id: None,
            name: "test_server".to_string(),
            disabled: false,
            forbidden_tools: vec![],
            tool_meta: HashMap::new(),
            default_tool_meta: None,
            vrl: None,
            server_parameters: StdioServerParameters {
                command: "echo".to_string(),
                args: vec![],
                env: HashMap::new(),
                cwd: None,
            },
        });

        manager.add_or_update_server(config).await.unwrap();

        // 移除服务器 / Remove server
        let removed = manager
            .remove_server_by_id(&BundleId::try_from("test_server").unwrap())
            .await
            .unwrap();
        assert!(removed, "确有该声明 ⇒ 回执 MUST 为 true");

        // 检查状态 / Check status
        let status = manager.get_server_status().await;
        assert!(status.is_empty());
    }

    /// no-double-open（加载期）：两个 config 解析出相同 bundle_id（同名缺省生成）→ 仅保留**配置顺序第一个**。
    #[tokio::test]
    async fn no_double_open_initialize_first_wins() {
        let manager = MCPServerManager::new();
        // 同名 "dup" 两条（缺省生成 bundle_id 均为 "dup"）；args 不同以区分「留了哪条」。
        let mut first = stdio_cfg_with_bundle("dup", None);
        if let MCPServerConfig::Stdio(ref mut c) = first {
            c.server_parameters.args = vec!["first".to_string()];
        }
        let mut second = stdio_cfg_with_bundle("dup", None);
        if let MCPServerConfig::Stdio(ref mut c) = second {
            c.server_parameters.args = vec!["second".to_string()];
        }
        manager.initialize(vec![first, second]).await.unwrap();

        // 仅一条存活，且是第一条（args=first）。
        let status = manager.get_server_status().await;
        assert_eq!(status.len(), 1, "重复 bundle_id 应只留一条: {status:?}");
        let configs = manager.servers_config.read().await;
        let kept = configs.get(&bid("dup")).expect("bundle_id=dup 应存在");
        match kept {
            MCPServerConfig::Stdio(c) => {
                assert_eq!(
                    c.server_parameters.args,
                    vec!["first".to_string()],
                    "应保留配置顺序第一个"
                );
            }
            _ => panic!("wrong variant"),
        }
    }

    /// 显式不同 bundle_id 使**同名** server 共存（#116 目标）：两条都在，键为各自 bundle_id。
    #[tokio::test]
    async fn distinct_explicit_bundle_ids_coexist_same_name() {
        let manager = MCPServerManager::new();
        let a = stdio_cfg_with_bundle("playwright", Some("playwright"));
        let b = stdio_cfg_with_bundle("playwright", Some("playwright_isolated"));
        manager.initialize(vec![a, b]).await.unwrap();

        let configs = manager.servers_config.read().await;
        assert_eq!(configs.len(), 2, "不同 bundle_id 的同名 server 应共存");
        assert!(configs.contains_key(&bid("playwright")));
        assert!(configs.contains_key(&bid("playwright_isolated")));
        // 两者 name 相同、身份不同。
        assert_eq!(
            configs.get(&bid("playwright")).unwrap().name(),
            "playwright"
        );
        assert_eq!(
            configs.get(&bid("playwright_isolated")).unwrap().name(),
            "playwright"
        );
    }

    /// no-double-open（运行期）：add_or_update 传已存在的 bundle_id → **原地更新**（不新增条目）。
    #[tokio::test]
    async fn no_double_open_add_updates_in_place() {
        let manager = MCPServerManager::new();
        let mut v1 = stdio_cfg_with_bundle("srv", Some("fixed_id"));
        if let MCPServerConfig::Stdio(ref mut c) = v1 {
            c.server_parameters.args = vec!["v1".to_string()];
        }
        manager.add_or_update_server(v1).await.unwrap();

        // name 变、bundle_id 不变 → 原地替换。
        let mut v2 = stdio_cfg_with_bundle("srv-renamed", Some("fixed_id"));
        if let MCPServerConfig::Stdio(ref mut c) = v2 {
            c.server_parameters.args = vec!["v2".to_string()];
        }
        manager.add_or_update_server(v2).await.unwrap();

        let configs = manager.servers_config.read().await;
        assert_eq!(configs.len(), 1, "同 bundle_id 应原地更新，不新增");
        let updated = configs.get(&bid("fixed_id")).expect("bundle_id 稳定");
        assert_eq!(updated.name(), "srv-renamed", "name 可变");
        match updated {
            MCPServerConfig::Stdio(c) => {
                assert_eq!(c.server_parameters.args, vec!["v2".to_string()])
            }
            _ => panic!(),
        }
    }

    /// #130：「显式非法 bundle_id 抵达 manager」这一失效模式已被**类型**消灭。
    ///
    /// 原测断言「运行期 add → Err；加载期 initialize → 跳过该条」。[`BundleId`] 构造即校验后，非法值**根本
    /// 构造不出来** ⇒ manager 无从收到它，那两个分支不再可达（已随 `resolve_key` 的 `Result` 一并移除）。
    ///
    /// **保证没有消失，只是前移且更响亮**，由两处接管——本测钉死①，②见
    /// `settings::mcp_config::tests::malformed_bundle_id_degrades_per_server_130`：
    /// 1. **构造边界**：非法值 → `Err`，编译期就不存在"带着非法 id 的 config"这种值；
    /// 2. **parse 边界**：`mcp.json` 的畸形 `bundleId` → 该 server **单条**判废 + 记错，**整份文件照常解析**
    ///    （即原「不硬失败整批 boot」语义的新家）。
    #[test]
    fn invalid_explicit_bundle_id_is_unconstructible_130() {
        // 含 `__` / `.` / 空 —— 原测覆盖的三类非法值，现在连值都造不出来。
        for bad in ["a__b", "has__sep", "a.b", ""] {
            assert!(
                BundleId::try_from(bad).is_err(),
                "{bad:?} 是非法 bundle_id，MUST 构造失败"
            );
        }
        // 不过度矫正：合法值仍可构造。
        assert!(BundleId::try_from("good_id").is_ok());
    }

    /// 空名 stdio → fallback 缺省生成（`bundle_` + 16 hex），仍可作身份键注册。
    #[tokio::test]
    async fn nameless_server_uses_fallback_bundle_id() {
        let manager = MCPServerManager::new();
        // 名字全符号 → 规范化为空 → fallback。
        manager
            .add_or_update_server(stdio_cfg_with_bundle("***", None))
            .await
            .unwrap();
        let configs = manager.servers_config.read().await;
        assert_eq!(configs.len(), 1);
        let key = configs.keys().next().unwrap();
        assert!(
            key.as_str().starts_with("bundle_"),
            "空规范化应触发 fallback: {key}"
        );
        assert_eq!(key.as_str().len(), "bundle_".len() + 16);
    }

    /// #141/R4：`reset_retry_count` 按 **bundle_id 寻址**（推翻旧 name→bundle_id 解析——name 桥
    /// `bundle_id_for_name` 已删，库层不再 name 寻址）。
    #[tokio::test]
    async fn reset_retry_count_by_bundle_id_141() {
        let manager = MCPServerManager::new();
        manager.retry_counts.write().await.insert(bid("bid"), 3);
        manager.reset_retry_count(&bid("bid")).await;
        assert!(
            !manager.get_retry_counts().await.contains_key(&bid("bid")),
            "按 bundle_id 清除重试计数"
        );
    }

    /// #118 P1：`get_config` 输出 `servers` 字典 **key = bundle_id**（非 name），value 带 `name` display 字段。
    #[tokio::test]
    async fn get_config_keyed_by_bundle_id_with_name_field_118() {
        let manager = MCPServerManager::new();
        // 展示名 "display-name" ≠ 显式 bundle_id "id_x"，以区分 key/name。
        manager.servers_config.write().await.insert(
            bid("id_x"),
            stdio_cfg_with_bundle("display-name", Some("id_x")),
        );

        let cfg = manager.get_server_configs().await;
        let obj = cfg.as_object().expect("object");
        assert!(obj.contains_key("id_x"), "key 应为 bundle_id: {obj:?}");
        assert!(!obj.contains_key("display-name"), "key 不应为 name");
        assert_eq!(
            obj["id_x"]["name"],
            serde_json::json!("display-name"),
            "value 应带 name display 字段（key 不再人类可读）"
        );
    }

    /// #118 P2/P3：`get_resources` 按 bundle_id 未命中 → `McpServerNotFound`（4014），载荷携 **bundle_id**。
    #[tokio::test]
    async fn get_resources_miss_returns_4014_with_bundle_id_118() {
        let manager = MCPServerManager::new();
        let err = manager
            .list_resources("no_such_bundle", None)
            .await
            .unwrap_err();
        assert_eq!(err.error_code(), 4014);
        match err {
            ComputerError::McpServerNotFound(id) => {
                assert_eq!(id, "no_such_bundle", "4014 载荷应回显未命中的 bundle_id")
            }
            other => panic!("expected McpServerNotFound, got {other:?}"),
        }
    }

    /// #118 P5：`get_windows_details` 投影——`.0` = **bundle_id**（active_clients 键），`.1` = 展示名
    /// （servers_config），window://host 属 MCP 自选、原样保留（正交）。用 bundle_id ≠ name 端到端区分该链。
    #[tokio::test]
    async fn get_windows_details_projects_bundle_id_and_name_118() {
        use super::test_support::WindowMockClient;
        let manager = MCPServerManager::new();
        // bundle_id "id_x" ≠ 展示名 "display-name"。
        manager.servers_config.write().await.insert(
            bid("id_x"),
            stdio_cfg_with_bundle("display-name", Some("id_x")),
        );
        manager.active_clients.write().await.insert(
            bid("id_x"),
            StdArc::new(WindowMockClient {
                uri: "window://a.mcp.com/w".to_string(),
                fail_detail: false,
                detail_calls: StdArc::new(std::sync::atomic::AtomicUsize::new(0)),
            }),
        );

        let details = manager.get_windows_details(None).await;
        assert_eq!(details.len(), 1, "应返回一个 window 详情");
        let (bundle_id, name, resource, _detail) = &details[0];
        assert_eq!(
            bundle_id.as_str(),
            "id_x",
            ".0 应为 bundle_id（active_clients 键）"
        );
        assert_eq!(name, "display-name", ".1 应为展示名（servers_config）");
        assert_eq!(
            resource.uri.as_str(),
            "window://a.mcp.com/w",
            "window://host 属 MCP 自选、原样保留（正交，不受 #118 改动）"
        );
    }

    /// #153：`list_windows_with_identity` 的 `.0` = **bundle_id**（`active_clients` 键），`.1` = 展示名。
    ///
    /// 两个 display 名相同、`bundle_id` 不同的合法共存 server：旧 `list_all_windows` 路径经
    /// `active_clients_by_name()` 把两者都标成同一展示名，下游无法区分；新方法按 bundle_id 标注，两者无歧义。
    /// 且本方法只调 `list_windows`（resources/list），不读取窗口内容。
    #[tokio::test]
    async fn list_windows_with_identity_keys_by_bundle_id_153() {
        use super::test_support::{stdio_cfg_with_bundle, WindowMockClient};
        let manager = MCPServerManager::new();
        // 两个 server 共用 display 名 "same-display-name"，bundle_id 各异（协议：name 允许碰撞）。
        for (id, uri) in [
            ("id_a", "window://a.example.com/w"),
            ("id_b", "window://b.example.com/w"),
        ] {
            manager.servers_config.write().await.insert(
                bid(id),
                stdio_cfg_with_bundle("same-display-name", Some(id)),
            );
            manager.active_clients.write().await.insert(
                bid(id),
                StdArc::new(WindowMockClient {
                    uri: uri.to_string(),
                    fail_detail: false,
                    detail_calls: StdArc::new(std::sync::atomic::AtomicUsize::new(0)),
                }),
            );
        }

        let mut entries = manager.list_windows_with_identity(None).await;
        entries.sort_by_key(|(bid, _, _)| bid.clone());
        assert_eq!(entries.len(), 2, "两个同名 server 的窗口均应返回");

        let (bid_a, name_a, res_a) = &entries[0];
        let (bid_b, name_b, res_b) = &entries[1];
        assert_eq!(
            bid_a.as_str(),
            "id_a",
            ".0 应为 bundle_id（active_clients 键）"
        );
        assert_eq!(bid_b.as_str(), "id_b");
        assert_eq!(
            name_a, "same-display-name",
            ".1 应为展示名（servers_config）"
        );
        assert_eq!(
            name_b, "same-display-name",
            "两 server 共用展示名但仍可由 bundle_id 区分"
        );
        assert_eq!(res_a.uri.as_str(), "window://a.example.com/w");
        assert_eq!(res_b.uri.as_str(), "window://b.example.com/w");
    }

    /// #153：`list_windows_with_identity` 只调 `list_windows`（resources/list）、**从不调**
    /// `get_window_detail`（resources/read），故单窗口读取失败不会令窗口从结果中消失。
    ///
    /// 契约 #1/#3 的**确定性**证明：用共享计数器断言新方法后 `detail_calls == 0`（压根没读取），
    /// 再调 `get_windows_details` 断言计数 `> 0`——既证明计数器接线正常（免「恒 0」假绿），又演示两路径差异
    /// （此处只看是否触发 `resources/read`，不依赖 `get_windows_details` 的丢窗行为，免未来耦合误红）。
    #[tokio::test]
    async fn list_windows_with_identity_survives_read_failure_153() {
        use super::test_support::{stdio_cfg_with_bundle, WindowMockClient};
        let manager = MCPServerManager::new();
        manager.servers_config.write().await.insert(
            bid("id_x"),
            stdio_cfg_with_bundle("display-name", Some("id_x")),
        );
        // list_windows 成功、get_window_detail 失败；detail_calls 共享计数以锁死「从不读取」。
        let detail_calls = StdArc::new(std::sync::atomic::AtomicUsize::new(0));
        manager.active_clients.write().await.insert(
            bid("id_x"),
            StdArc::new(WindowMockClient {
                uri: "window://a.mcp.com/w".to_string(),
                fail_detail: true,
                detail_calls: detail_calls.clone(),
            }),
        );

        let enumerated = manager.list_windows_with_identity(None).await;
        assert_eq!(
            enumerated.len(),
            1,
            "读取失败不应令窗口从枚举结果消失（本方法不读取）"
        );
        let (bid, name, resource) = &enumerated[0];
        assert_eq!(bid.as_str(), "id_x");
        assert_eq!(name, "display-name");
        assert_eq!(resource.uri.as_str(), "window://a.mcp.com/w");
        // 确定性契约证明：新方法不应触发任何 resources/read。
        assert_eq!(
            detail_calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "list_windows_with_identity 不应调用 get_window_detail（resources/read）"
        );

        // 计数器接线自检 + 行为对照：get_windows_details 会 eager-read（计数 >0）。
        // 证明计数非恒 0（防 mock 忘增计数致假绿），且新方法确未读取——不依赖丢窗行为。
        let _details = manager.get_windows_details(None).await;
        assert!(
            detail_calls.load(std::sync::atomic::Ordering::SeqCst) > 0,
            "对照：get_windows_details 应触发 resources/read（证明计数器接线 + 新方法确未读取）"
        );
    }

    /// #127：`list_skill_resources` 的 `.0` = **bundle_id**（`active_clients` 键），非 display 名。
    ///
    /// SKILL 通道是最后一个仍以 display 名标注/寻址的通道（协议 skill.md §1.3 已废止该例外）。
    /// 两个 display 名相同、`bundle_id` 不同的**合法共存** server：旧实现经 `active_clients_by_name()`
    /// 把两者都标成同一个 display 名 → staging 合成同一个 `mcp:<name>:<skill>` → 后者被去重丢弃、
    /// 其 SKILL 对 Agent **隐身**。
    #[tokio::test]
    async fn list_skill_resources_keys_by_bundle_id_127() {
        use super::test_support::{inject, inject_config, skill_resource, MockSkillClient};
        let manager = MCPServerManager::new();
        // 两个 server 共用 display 名 "same-display-name"，bundle_id 各异（协议：name 允许碰撞）。
        for id in ["id_a", "id_b"] {
            inject_config(
                &manager,
                &bid(id),
                stdio_cfg_with_bundle("same-display-name", Some(id)),
            )
            .await;
            inject(
                &manager,
                &bid(id),
                MockSkillClient {
                    pages: vec![vec![skill_resource(
                        &format!("skill://h.example.com/{id}"),
                        Some("mounted"),
                    )]],
                    fail: false,
                    cap_fail: false,
                    read_text: String::new(),
                },
            )
            .await;
        }

        let mut keys: Vec<BundleId> = manager
            .list_skill_resources(None)
            .await
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            vec![bid("id_a"), bid("id_b")],
            ".0 应为 bundle_id：两个同名 server 须各自可辨（旧实现两条都标 'same-display-name'）"
        );

        // 定向枚举按 bundle_id 寻址；display 名不再是寻址键。
        let scoped = manager.list_skill_resources(Some("id_a")).await;
        assert_eq!(scoped.len(), 1, "定向枚举应只命中 id_a");
        assert_eq!(scoped[0].0.as_str(), "id_a");
        assert!(
            manager
                .list_skill_resources(Some("same-display-name"))
                .await
                .is_empty(),
            "display 名非寻址键，不应命中任何 server"
        );
    }

    /// #127 扫漏：`check_all_health` 的键 = **bundle_id**，非 display 名。
    ///
    /// 旧实现按 display 名建 map：两个同名 server 只剩**一条**健康结果——不健康的那个会被同名健康者
    /// 静默掩盖（后写覆盖先写），使观测端点对「哪个软件挂了」给出错误答案。
    #[tokio::test]
    async fn check_all_health_keys_by_bundle_id_127() {
        use super::test_support::{inject, inject_config, MockSkillClient};
        let manager = MCPServerManager::new();
        for id in ["id_a", "id_b"] {
            inject_config(
                &manager,
                &bid(id),
                stdio_cfg_with_bundle("same-display-name", Some(id)),
            )
            .await;
            inject(
                &manager,
                &bid(id),
                MockSkillClient {
                    pages: vec![],
                    fail: false,
                    cap_fail: false,
                    read_text: String::new(),
                },
            )
            .await;
        }

        let health = manager.check_all_health().await;
        let mut keys: Vec<BundleId> = health.keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            vec![bid("id_a"), bid("id_b")],
            "键应为 bundle_id：两个同名 server 各须有独立健康结果（旧实现丢一条）"
        );
    }

    #[tokio::test]
    async fn test_tool_conflict_detection() {
        let manager = MCPServerManager::new();

        // 创建两个服务器，有同名工具 / Create two servers with same tool name
        let configs = vec![
            // 第一个服务器 / First server
            MCPServerConfig::Stdio(StdioServerConfig {
                env_file: None,
                bundle_id: None,
                name: "server1".to_string(),
                disabled: false,
                forbidden_tools: vec![],
                tool_meta: HashMap::new(),
                default_tool_meta: None,
                vrl: None,
                server_parameters: StdioServerParameters {
                    command: "echo".to_string(),
                    args: vec!["server1".to_string()],
                    env: HashMap::new(),
                    cwd: None,
                },
            }),
            // 第二个服务器 / Second server
            MCPServerConfig::Stdio(StdioServerConfig {
                env_file: None,
                bundle_id: None,
                name: "server2".to_string(),
                disabled: false,
                forbidden_tools: vec![],
                tool_meta: HashMap::new(),
                default_tool_meta: None,
                vrl: None,
                server_parameters: StdioServerParameters {
                    command: "echo".to_string(),
                    args: vec!["server2".to_string()],
                    env: HashMap::new(),
                    cwd: None,
                },
            }),
        ];

        // 初始化应该成功 / Initialization should succeed
        let result = manager.initialize(configs).await;
        assert!(result.is_ok());

        // 启动所有服务器 / Start all servers（协议 0.3.0：同名工具经 {bundle_id}__ 前缀天然不冲突，不再报错；
        // 此处仅验证真实 echo 进程启动路径不 panic——连接可能失败，属预期）。
        let _result = manager.start_all().await;

        // 等待连接建立 / Wait for connections to establish
        sleep(Duration::from_millis(200)).await;
    }

    #[tokio::test]
    async fn test_health_check_config() {
        let manager = MCPServerManager::new();

        // 验证默认配置 / Verify default config
        let config = manager.get_health_check_config().await;
        assert_eq!(config.interval_secs, 30);
        assert_eq!(config.timeout_secs, 5);
        assert!(config.enabled);

        // 更新配置 / Update config
        let new_config = HealthCheckConfig {
            interval_secs: 60,
            timeout_secs: 10,
            enabled: false,
        };
        manager.set_health_check_config(new_config.clone()).await;

        let updated = manager.get_health_check_config().await;
        assert_eq!(updated.interval_secs, 60);
        assert_eq!(updated.timeout_secs, 10);
        assert!(!updated.enabled);
    }

    #[tokio::test]
    async fn health_monitor_does_not_reconnect_client_removed_during_health_check() {
        let manager = MCPServerManager::new();
        let bundle_id = bid("health-race");
        manager.servers_config.write().await.insert(
            bundle_id.clone(),
            stdio_cfg_with_bundle("health-race", Some("health-race")),
        );
        let client = Arc::new(HealthRaceClient {
            health_started: tokio::sync::Notify::new(),
            release_health: tokio::sync::Notify::new(),
            connect_calls: AtomicUsize::new(0),
            disconnect_calls: AtomicUsize::new(0),
        });
        manager
            .active_clients
            .write()
            .await
            .insert(bundle_id.clone(), client.clone());
        manager
            .set_health_check_config(HealthCheckConfig {
                interval_secs: 3600,
                timeout_secs: 5,
                enabled: true,
            })
            .await;
        manager
            .set_reconnect_policy(ReconnectPolicy {
                enabled: true,
                max_retries: 1,
                initial_delay_ms: 0,
                max_delay_ms: 0,
                backoff_factor: 1.0,
            })
            .await;

        let health_started = client.health_started.notified();
        manager.start_health_monitor().await;
        tokio::time::timeout(Duration::from_secs(2), health_started)
            .await
            .expect("health monitor did not inspect the client");
        assert!(manager.stop_client_by_id(&bundle_id).await.unwrap());
        client.release_health.notify_one();
        sleep(Duration::from_millis(100)).await;
        manager.stop_health_monitor().await;

        assert_eq!(client.disconnect_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            client.connect_calls.load(Ordering::SeqCst),
            0,
            "a stale health snapshot must not reconnect a removed client"
        );
        assert!(!manager.active_clients.read().await.contains_key(&bundle_id));
    }

    #[tokio::test]
    async fn test_reconnect_policy() {
        let manager = MCPServerManager::new();

        // 验证默认策略 / Verify default policy
        let policy = manager.get_reconnect_policy().await;
        assert!(policy.enabled);
        assert_eq!(policy.max_retries, 5);
        assert_eq!(policy.initial_delay_ms, 1000);
        assert_eq!(policy.max_delay_ms, 30000);
        assert_eq!(policy.backoff_factor, 2.0);

        // 测试延迟计算 / Test delay calculation
        assert_eq!(policy.calculate_delay(0).as_millis(), 1000);
        assert_eq!(policy.calculate_delay(1).as_millis(), 2000);
        assert_eq!(policy.calculate_delay(2).as_millis(), 4000);
        assert_eq!(policy.calculate_delay(3).as_millis(), 8000);

        // 测试 should_retry / Test should_retry
        assert!(policy.should_retry(0));
        assert!(policy.should_retry(4));
        assert!(!policy.should_retry(5)); // max is 5

        // 测试无限重试 / Test infinite retry
        let infinite_policy = ReconnectPolicy {
            enabled: true,
            max_retries: 0,
            ..Default::default()
        };
        assert!(infinite_policy.should_retry(100));
    }

    #[tokio::test]
    async fn test_retry_counts() {
        let manager = MCPServerManager::new();

        // 初始应该为空 / Should be empty initially
        let counts = manager.get_retry_counts().await;
        assert!(counts.is_empty());

        // 通过内部操作添加重试计数 / Add retry counts through internal operation
        {
            manager.retry_counts.write().await.insert(bid("server1"), 3);
            manager.retry_counts.write().await.insert(bid("server2"), 5);
        }

        let counts = manager.get_retry_counts().await;
        assert_eq!(counts.get(&bid("server1")), Some(&3));
        assert_eq!(counts.get(&bid("server2")), Some(&5));

        // 重置单个服务器 / Reset single server
        manager.reset_retry_count(&bid("server1")).await;
        let counts = manager.get_retry_counts().await;
        assert!(!counts.contains_key(&bid("server1")));
        assert_eq!(counts.get(&bid("server2")), Some(&5));

        // 重置所有 / Reset all
        manager.reset_all_retry_counts().await;
        let counts = manager.get_retry_counts().await;
        assert!(counts.is_empty());
    }

    #[tokio::test]
    async fn test_manager_with_custom_config() {
        let health_config = HealthCheckConfig {
            interval_secs: 15,
            timeout_secs: 3,
            enabled: true,
        };
        let reconnect_policy = ReconnectPolicy {
            enabled: true,
            max_retries: 10,
            initial_delay_ms: 500,
            max_delay_ms: 60000,
            backoff_factor: 1.5,
        };

        let manager =
            MCPServerManager::with_config(health_config.clone(), reconnect_policy.clone());

        let got_health = manager.get_health_check_config().await;
        assert_eq!(got_health.interval_secs, 15);
        assert_eq!(got_health.timeout_secs, 3);

        let got_reconnect = manager.get_reconnect_policy().await;
        assert_eq!(got_reconnect.max_retries, 10);
        assert_eq!(got_reconnect.initial_delay_ms, 500);
    }

    #[tokio::test]
    async fn test_merged_tool_meta() {
        let manager = MCPServerManager::new();

        // Case 1: specific only
        let config = MCPServerConfig::Stdio(StdioServerConfig {
            env_file: None,
            bundle_id: None,
            name: "s".to_string(),
            disabled: false,
            forbidden_tools: vec![],
            tool_meta: HashMap::from([(
                "tool_a".to_string(),
                ToolMeta {
                    auto_apply: Some(true),
                    alias: None,
                    tags: Some(vec!["tag1".to_string()]),
                    ret_object_mapper: None,
                },
            )]),
            default_tool_meta: None,
            vrl: None,
            server_parameters: StdioServerParameters {
                command: "echo".to_string(),
                args: vec![],
                env: HashMap::new(),
                cwd: None,
            },
        });
        let meta = manager.merged_tool_meta(&config, "tool_a").unwrap();
        assert_eq!(meta.auto_apply, Some(true));
        assert_eq!(meta.tags, Some(vec!["tag1".to_string()]));

        // Case 2: default only
        let config = MCPServerConfig::Stdio(StdioServerConfig {
            env_file: None,
            bundle_id: None,
            name: "s".to_string(),
            disabled: false,
            forbidden_tools: vec![],
            tool_meta: HashMap::new(),
            default_tool_meta: Some(ToolMeta {
                auto_apply: Some(false),
                alias: Some("ignored_default_alias".to_string()),
                tags: Some(vec!["default_tag".to_string()]),
                ret_object_mapper: None,
            }),
            vrl: None,
            server_parameters: StdioServerParameters {
                command: "echo".to_string(),
                args: vec![],
                env: HashMap::new(),
                cwd: None,
            },
        });
        let meta = manager.merged_tool_meta(&config, "any_tool").unwrap();
        assert_eq!(meta.auto_apply, Some(false));
        assert_eq!(meta.tags, Some(vec!["default_tag".to_string()]));
        // #134：default 位的 alias 绝不回落到任何工具（其余字段照常作默认值）。
        assert_eq!(meta.alias, None);

        // Case 3: specific + default merge (specific wins)
        let config = MCPServerConfig::Stdio(StdioServerConfig {
            env_file: None,
            bundle_id: None,
            name: "s".to_string(),
            disabled: false,
            forbidden_tools: vec![],
            tool_meta: HashMap::from([(
                "tool_a".to_string(),
                ToolMeta {
                    auto_apply: Some(true),
                    alias: None,
                    tags: None,
                    ret_object_mapper: None,
                },
            )]),
            default_tool_meta: Some(ToolMeta {
                auto_apply: Some(false),
                alias: Some("default_alias".to_string()),
                tags: Some(vec!["default_tag".to_string()]),
                ret_object_mapper: None,
            }),
            vrl: None,
            server_parameters: StdioServerParameters {
                command: "echo".to_string(),
                args: vec![],
                env: HashMap::new(),
                cwd: None,
            },
        });
        let meta = manager.merged_tool_meta(&config, "tool_a").unwrap();
        assert_eq!(meta.auto_apply, Some(true)); // specific wins
                                                 // #134：alias 天生 per-tool，绝不从 default 继承——specific 无 alias ⇒ 结果为 None（旧行为曾误取 default_alias）。
        assert_eq!(meta.alias, None);
        assert_eq!(meta.tags, Some(vec!["default_tag".to_string()])); // 其余字段仍从 default 回落

        // Case 4: no config
        let config = MCPServerConfig::Stdio(StdioServerConfig {
            env_file: None,
            bundle_id: None,
            name: "s".to_string(),
            disabled: false,
            forbidden_tools: vec![],
            tool_meta: HashMap::new(),
            default_tool_meta: None,
            vrl: None,
            server_parameters: StdioServerParameters {
                command: "echo".to_string(),
                args: vec![],
                env: HashMap::new(),
                cwd: None,
            },
        });
        assert!(manager.merged_tool_meta(&config, "tool_a").is_none());
    }

    #[tokio::test]
    async fn test_list_all_windows_empty_manager() {
        let manager = MCPServerManager::new();
        let windows = manager.list_all_windows(None).await;
        assert!(windows.is_empty());
    }

    #[tokio::test]
    async fn test_get_window_detail_server_not_connected() {
        use super::make_resource;
        let manager = MCPServerManager::new();
        let resource = make_resource(
            "window://test/status",
            "Test",
            None,
            Some("text/plain".into()),
        );
        let result = manager
            .get_window_detail(&bid("unknown_server"), resource)
            .await;
        assert!(result.is_err());
        match result {
            Err(ComputerError::InvalidState(msg)) => {
                assert!(msg.contains("not connected"));
            }
            other => panic!("Expected InvalidState, got {:?}", other),
        }
    }

    // ---- #74 INT-04：list_skill_resources + SkillResourceManager 接缝 ----
    // Mock / helpers 提升至 `super::test_support`（`pub(crate)`），供 computer.rs 集成测试复用。
    use super::test_support::{inject, skill_resource, MockSkillClient};

    #[tokio::test]
    async fn test_list_skill_resources_filters_and_exhausts_pages() {
        let manager = MCPServerManager::new();
        let pages = vec![
            vec![
                skill_resource("skill://srv/a", Some("mounted")),
                make_resource("window://w", "w", None, None),
            ],
            vec![skill_resource("skill://srv/b", Some("mounted"))],
        ];
        inject(
            &manager,
            &bid("srv"),
            MockSkillClient {
                pages,
                fail: false,
                cap_fail: false,
                read_text: "x".into(),
            },
        )
        .await;

        let got = manager.list_skill_resources(None).await;
        let uris: Vec<&str> = got.iter().map(|(_, r)| r.uri.as_str()).collect();
        // window:// 被过滤；两页都被消费 / window:// filtered out; both pages consumed.
        assert_eq!(uris, vec!["skill://srv/a", "skill://srv/b"]);
        assert!(got.iter().all(|(s, _)| s.as_str() == "srv"));
    }

    #[tokio::test]
    async fn test_skill_resource_manager_trait_meta_and_read_bytes() {
        let manager = MCPServerManager::new();
        inject(
            &manager,
            &bid("srv"),
            MockSkillClient {
                pages: vec![vec![skill_resource("skill://srv/a", Some("mounted"))]],
                fail: false,
                cap_fail: false,
                read_text: "hello-bytes".into(),
            },
        )
        .await;

        // 经 SkillResourceManager trait：Resource → McpResource（提取 `_meta`）。
        let pairs = SkillResourceManager::list_skill_resources(&manager, None)
            .await
            .unwrap();
        assert_eq!(pairs.len(), 1);
        let (sname, mcp_res) = &pairs[0];
        assert_eq!(sname, "srv");
        assert_eq!(mcp_res.uri, "skill://srv/a");
        assert_eq!(
            mcp_res.meta.get("source").and_then(|v| v.as_str()),
            Some("mounted")
        );

        // read_resource → 字节（文本 content 拼接为 UTF-8 字节）。
        let bytes = SkillResourceManager::read_resource(&manager, "srv", "skill://srv/a")
            .await
            .unwrap();
        assert_eq!(bytes, b"hello-bytes");
    }

    #[tokio::test]
    async fn test_list_skill_resources_per_server_isolation_and_filter() {
        let manager = MCPServerManager::new();
        inject(
            &manager,
            &bid("bad"),
            MockSkillClient {
                pages: vec![],
                fail: true,
                cap_fail: false,
                read_text: String::new(),
            },
        )
        .await;
        inject(
            &manager,
            &bid("good"),
            MockSkillClient {
                pages: vec![vec![skill_resource("skill://good/a", Some("mounted"))]],
                fail: false,
                cap_fail: false,
                read_text: String::new(),
            },
        )
        .await;

        // 出错 server 跳过，good 的结果仍在 / erroring server skipped, good's result remains.
        let got = manager.list_skill_resources(None).await;
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0.as_str(), "good");
        assert_eq!(got[0].1.uri, "skill://good/a");

        // server_name 过滤：只枚举指定 server / server_name filter narrows enumeration.
        let only_good = manager.list_skill_resources(Some("good")).await;
        assert_eq!(only_good.len(), 1);
        let none = manager.list_skill_resources(Some("missing")).await;
        assert!(none.is_empty());
    }

    // ---- #68：list_resources（get_resources 路由 + 4014/4015 映射）----

    #[tokio::test]
    async fn test_list_resources_unknown_server_4014() {
        let manager = MCPServerManager::new();
        let err = manager.list_resources("nope", None).await.unwrap_err();
        assert_eq!(err.error_code(), 4014);
        assert!(matches!(err, ComputerError::McpServerNotFound(s) if s == "nope"));
    }

    #[tokio::test]
    async fn test_list_resources_capability_not_supported_4015() {
        let manager = MCPServerManager::new();
        inject(
            &manager,
            &bid("srv"),
            MockSkillClient {
                pages: vec![],
                fail: false,
                cap_fail: true,
                read_text: String::new(),
            },
        )
        .await;
        let err = manager.list_resources("srv", None).await.unwrap_err();
        assert_eq!(err.error_code(), 4015);
        assert!(matches!(
            err,
            ComputerError::McpCapabilityNotSupported { bundle_id, capability }
            if bundle_id == "srv" && capability == "resources"
        ));
    }

    #[tokio::test]
    async fn test_list_resources_single_page_passthrough_cursor() {
        let manager = MCPServerManager::new();
        inject(
            &manager,
            &bid("srv"),
            MockSkillClient {
                pages: vec![
                    vec![make_resource("res://a", "a", None, None)],
                    vec![make_resource("res://b", "b", None, None)],
                ],
                fail: false,
                cap_fail: false,
                read_text: String::new(),
            },
        )
        .await;

        // 首页（cursor=None）：返回第 1 页 + next cursor（透传，不聚合第 2 页）。
        let (page1, next1) = manager.list_resources("srv", None).await.unwrap();
        assert_eq!(page1.len(), 1);
        assert_eq!(page1[0].uri, "res://a");
        assert_eq!(next1.as_deref(), Some("1"));

        // 第 2 页（透传 cursor）：返回第 2 页 + 末页（next=None）。
        let (page2, next2) = manager.list_resources("srv", next1).await.unwrap();
        assert_eq!(page2[0].uri, "res://b");
        assert!(next2.is_none());
    }

    // ── INT-02 #70：call_tool_cancellable 三态（completed / cancelled / timeout）─────────
    // CancelMockClient / CancelBehavior / inject_callable 已上移至 test_support（pub(crate)），
    // 供 manager（本节）与 computer（execute_tool_cancellable 端到端）测试共享。
    use super::test_support::{inject_callable, CancelBehavior};

    async fn manager_with_cancel_mock(behavior: CancelBehavior) -> MCPServerManager {
        let manager = MCPServerManager::new();
        inject_callable(&manager, "srv", "t", behavior).await;
        manager
    }

    #[tokio::test]
    async fn test_call_tool_cancellable_completed() {
        // 正常完成 → Completed(result)，无取消/超时（令牌未 fire）。
        let manager = manager_with_cancel_mock(CancelBehavior::CompleteOk).await;
        let outcome = manager
            .call_tool_cancellable(
                "srv",
                "t",
                serde_json::json!({}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        match outcome {
            CancellableCallOutcome::Completed(r) => assert_ne!(r.is_error, Some(true)),
            CancellableCallOutcome::Cancelled => panic!("未 fire 令牌不应取消"),
        }
    }

    #[tokio::test]
    async fn test_call_tool_cancellable_cancelled() {
        // 在途阻塞 + 令牌已 fire → 就地中断回 Cancelled（biased select 先轮询阻塞的 call_tool=pending，
        // 再命中已就绪的取消分支）。
        let manager = manager_with_cancel_mock(CancelBehavior::BlockForever).await;
        let token = CancellationToken::new();
        token.cancel();
        let outcome = manager
            .call_tool_cancellable("srv", "t", serde_json::json!({}), None, token)
            .await
            .unwrap();
        assert!(
            matches!(outcome, CancellableCallOutcome::Cancelled),
            "在途阻塞 + 取消令牌应回 Cancelled"
        );
    }

    #[tokio::test]
    async fn test_call_tool_cancellable_timeout() {
        // 睡眠 10s + 50ms timeout → manager 级超时 → Err(TimeoutError)（Computer 据此写 meta.a2c_timeout）。
        let manager =
            manager_with_cancel_mock(CancelBehavior::Sleep(Duration::from_secs(10))).await;
        let err = manager
            .call_tool_cancellable(
                "srv",
                "t",
                serde_json::json!({}),
                Some(Duration::from_millis(50)),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ComputerError::TimeoutError(_)),
            "超时应回 TimeoutError，实得: {err:?}"
        );
    }

    // ── forbidden_tools / alias 暴露面回归（对标 Python python-sdk #106/#107）─────────────────
    use super::test_support::{inject_counting_tools, inject_tools, ErrToolsClient};

    /// 最小 Stdio 工具：仅 name + 空 object inputSchema（参照 `socketio_client::tests::make_tool`）。
    fn tool_named(name: &str) -> Tool {
        let input_schema: serde_json::Map<String, serde_json::Value> =
            serde_json::from_value(serde_json::json!({"type": "object"})).unwrap();
        Tool::new(name.to_string(), "t", StdArc::new(input_schema))
    }

    /// Stdio ServerConfig 构造器（forbidden_tools / tool_meta 可定制），server_parameters 占位。
    fn stdio_cfg(
        name: &str,
        forbidden: Vec<String>,
        tool_meta: HashMap<String, ToolMeta>,
    ) -> MCPServerConfig {
        let mut c = StdioServerConfig::new(
            name,
            StdioServerParameters {
                command: "echo".to_string(),
                args: vec![],
                env: HashMap::new(),
                cwd: None,
            },
        );
        c.forbidden_tools = forbidden;
        c.tool_meta = tool_meta;
        MCPServerConfig::Stdio(c)
    }

    /// exposed_tool_name 便捷构造（`{bundle_id}__{tool}`）；测试中 server 名简单即等于其 bundle_id。
    fn exposed(bundle: &str, tool: &str) -> String {
        format!("{bundle}__{tool}")
    }

    /// 注入 (client, config) 并重建路由；键统一用 `resolve_bundle_id(&cfg)`（= 简单名本身），与生产语义一致。
    /// `refresh_tool_routes` 同时读 active_clients + servers_config（均按 bundle_id 键）。
    async fn setup_and_refresh(
        manager: &MCPServerManager,
        servers: Vec<(&str, Vec<Tool>, MCPServerConfig)>,
    ) -> Result<(), ComputerError> {
        for (_name, tools, cfg) in servers {
            let bid = super::bundle_id::resolve_bundle_id(&cfg);
            inject_tools(manager, &bid, tools).await;
            manager.servers_config.write().await.insert(bid, cfg);
        }
        manager.refresh_tool_routes().await
    }

    fn meta_with_alias(original: &str, alias: &str) -> HashMap<String, ToolMeta> {
        let mut tm = HashMap::new();
        tm.insert(
            original.to_string(),
            ToolMeta {
                alias: Some(alias.to_string()),
                ..ToolMeta::new()
            },
        );
        tm
    }

    /// alias 反映到对外暴露的 Tool.name，但**仍带 `{bundle_id}__` 前缀**（协议 0.3.0：alias 只替换工具名部分）。
    /// 原始名、裸 alias 均不出现。
    #[tokio::test]
    async fn test_available_tools_exposes_alias_as_name() {
        let manager = MCPServerManager::new();
        setup_and_refresh(
            &manager,
            vec![(
                "srv",
                vec![tool_named("tool5")],
                stdio_cfg("srv", vec![], meta_with_alias("tool5", "aliased_tool")),
            )],
        )
        .await
        .expect("refresh ok");

        let names: Vec<String> = manager
            .list_available_tools()
            .await
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        assert!(
            names.contains(&exposed("srv", "aliased_tool")),
            "暴露名应为 {{bundle_id}}__alias: {names:?}"
        );
        assert!(
            !names.contains(&exposed("srv", "tool5"))
                && !names.contains(&"aliased_tool".to_string()),
            "原始名与裸 alias 均不应出现: {names:?}"
        );
    }

    /// #91：一次 `list_available_tools` 对同一 server 只应发**一次** `tools/list`（此前每 mapped tool
    /// 各发一次 → N 工具 = N 次冗余往返 + N 行重复 `Found N tools` 日志）。对齐 Python `available_tools`
    /// 的 `servers_cached_tools` per-server 缓存。
    #[tokio::test]
    async fn list_available_tools_calls_list_tools_once_per_server() {
        let manager = MCPServerManager::new();
        // srv 暴露 3 个工具 → tool_mapping 三条都指向 srv。
        let calls = inject_counting_tools(
            &manager,
            "srv",
            vec![tool_named("a"), tool_named("b"), tool_named("c")],
        )
        .await;
        manager
            .servers_config
            .write()
            .await
            .insert(bid("srv"), stdio_cfg("srv", vec![], HashMap::new()));
        manager.refresh_tool_mapping().await.expect("refresh ok");

        // 归零，隔离 refresh_tool_mapping 自身那次 list_tools —— 仅测 list_available_tools。
        calls.store(0, std::sync::atomic::Ordering::SeqCst);
        let out = manager.list_available_tools().await;

        assert_eq!(out.len(), 3, "3 个工具都应暴露");
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "list_available_tools 应每 server 仅调用 list_tools 一次（#91）"
        );
    }

    /// #136 D1（**防假绿**·Epic #129 关键坑）：`list_available_tools_with_bundle_id` 每项携带的
    /// bundle_id 是**解析后** bundle_id（= `servers_config`/`active_clients` 键 = `route.bundle_id`），
    /// **非**从 display 名派生。用**显式 bundle_id ≠ `normalize_name(display 名)`** 令任何名派生实现显红。
    ///
    /// 缺省路径下 `bundle_id == normalize_name(name)` 会盖住裂缝（四轮扫漏之源），故此处显式令二者分叉。
    #[tokio::test]
    async fn list_available_tools_carries_resolved_bundle_id_not_name_derived() {
        let manager = MCPServerManager::new();
        // 展示名 "Display Name"（normalize → "display_name"）≠ 显式 bundle_id "id_x"，令名派生实现暴露。
        inject_counting_tools(&manager, "id_x", vec![tool_named("real_tool")]).await;
        manager.servers_config.write().await.insert(
            bid("id_x"),
            stdio_cfg_with_bundle("Display Name", Some("id_x")),
        );
        manager.refresh_tool_mapping().await.expect("refresh ok");

        let out = manager.list_available_tools_with_bundle_id().await;
        assert_eq!(out.len(), 1, "应暴露一个工具");
        let (bundle_id, tool) = &out[0];
        assert_eq!(
            bundle_id.as_str(),
            "id_x",
            "bundle_id 应为解析后值（servers_config 键 / route.bundle_id），\
             非从 display 名 'Display Name' 派生的 'display_name'"
        );
        assert_eq!(
            tool.name.as_ref(),
            exposed("id_x", "real_tool"),
            "exposed 名 = {{bundle_id}}__{{tool}}"
        );
    }

    /// #136 F2：`get_server_configs`（`get_config.servers` 数据源）读**运行期活跃集**——经**运行期
    /// 公开入口** `add_or_update_server` 注入一个 server 后即以其 bundle_id 为 key 反映（非空、非构造期
    /// 死快照，对齐 python#149 的修复方向）。走公开入口而非直写 `servers_config`，同时守护「运行期变更
    /// 入口 ↔ get_config 所读 map」之间的接线（`auto_connect` 默认关，不触发真实连接）。
    #[tokio::test]
    async fn get_server_configs_reflects_runtime_active_set() {
        let manager = MCPServerManager::new();
        // 构造后初始为空（无死快照残留）。
        assert!(
            manager
                .get_server_configs()
                .await
                .as_object()
                .expect("object")
                .is_empty(),
            "初始应为空"
        );

        // 经运行期公开入口注入 → 立即以 bundle_id 键反映。
        manager
            .add_or_update_server(stdio_cfg_with_bundle("Display Name", Some("id_x")))
            .await
            .expect("add_or_update_server ok");
        let cfg = manager.get_server_configs().await;
        let obj = cfg.as_object().expect("object");
        assert!(
            obj.contains_key("id_x"),
            "运行期新增 server 应即以 bundle_id 为 key 出现（活跃集，非死快照）: {obj:?}"
        );
        assert_eq!(
            obj["id_x"]["name"],
            serde_json::json!("Display Name"),
            "value 带 display 名（key = bundle_id 非 name）"
        );
    }

    /// #91：缓存**按 server 键隔离** —— 多 server 各自仅一次 `list_tools`、工具不串（防「全局单缓存」退化，
    /// 该退化在单 server 用例下无法暴露）。
    #[tokio::test]
    async fn list_available_tools_caches_per_server_not_globally() {
        let manager = MCPServerManager::new();
        let calls_a =
            inject_counting_tools(&manager, "srvA", vec![tool_named("a1"), tool_named("a2")]).await;
        let calls_b =
            inject_counting_tools(&manager, "srvB", vec![tool_named("b1"), tool_named("b2")]).await;
        {
            let mut cfgs = manager.servers_config.write().await;
            cfgs.insert(bid("srvA"), stdio_cfg("srvA", vec![], HashMap::new()));
            cfgs.insert(bid("srvB"), stdio_cfg("srvB", vec![], HashMap::new()));
        }
        manager.refresh_tool_mapping().await.expect("refresh ok");

        calls_a.store(0, std::sync::atomic::Ordering::SeqCst);
        calls_b.store(0, std::sync::atomic::Ordering::SeqCst);
        let out = manager.list_available_tools().await;
        let names: Vec<String> = out.iter().map(|t| t.name.to_string()).collect();

        assert_eq!(
            calls_a.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "srvA 仅一次"
        );
        assert_eq!(
            calls_b.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "srvB 仅一次"
        );
        assert_eq!(out.len(), 4, "两 server 4 个工具全暴露");
        for (bundle, tool) in [
            ("srvA", "a1"),
            ("srvA", "a2"),
            ("srvB", "b1"),
            ("srvB", "b2"),
        ] {
            assert!(
                names.contains(&exposed(bundle, tool)),
                "{bundle}__{tool} 应暴露且不串: {names:?}"
            );
        }
    }

    /// #91：`list_tools` 持续报错的 server 被**跳过**（不暴露其工具、不 panic），保留原吞错语义。
    #[tokio::test]
    async fn list_available_tools_skips_server_when_list_tools_errs() {
        let manager = MCPServerManager::new();
        // 直接注入常错 client + 手填一条路由（模拟此前已路由，但此刻 list_tools 失败），精确命中 Err 分支。
        manager
            .active_clients
            .write()
            .await
            .insert(bid("srv"), StdArc::new(ErrToolsClient));
        manager
            .servers_config
            .write()
            .await
            .insert(bid("srv"), stdio_cfg("srv", vec![], HashMap::new()));
        manager.tool_routes.write().await.insert(
            exposed("srv", "t"),
            ExposedToolRoute {
                bundle_id: bid("srv"),
                server_name: "srv".to_string(),
                original_tool_name: "t".to_string(),
                alias: None,
            },
        );

        let out = manager.list_available_tools().await;
        assert!(out.is_empty(), "list_tools 失败的 server 不应暴露工具");
    }

    /// 两 server 同名 tool1：协议 0.3.0 前缀化后跨 server **天然不冲突**（`server1__tool1` ⊥ `server2__tool1`），
    /// #116 收益。forbid server1 侧仅禁用 `server1__tool1`，server2 侧独立暴露、寻址无误伤（无跨 server 对账）。
    #[tokio::test]
    async fn test_same_tool_name_across_servers_no_conflict() {
        let manager = MCPServerManager::new();
        let res = setup_and_refresh(
            &manager,
            vec![
                (
                    "server1",
                    vec![tool_named("tool1")],
                    stdio_cfg("server1", vec!["tool1".to_string()], HashMap::new()),
                ),
                (
                    "server2",
                    vec![tool_named("tool1")],
                    stdio_cfg("server2", vec![], HashMap::new()),
                ),
            ],
        )
        .await;

        assert!(res.is_ok(), "前缀化后跨 server 同名不应报错: {res:?}");
        // server2 侧独立路由；server1 侧被 forbid（各自独立的 exposed 名，无对账）。
        let routes = manager.tool_routes.read().await;
        let route = routes
            .get(&exposed("server2", "tool1"))
            .expect("server2 路由存在");
        assert_eq!(route.bundle_id.as_str(), "server2");
        assert_eq!(route.original_tool_name, "tool1");
        assert!(
            !routes.contains_key(&exposed("server1", "tool1")),
            "server1 侧被 forbid，不路由"
        );
        drop(routes);
        assert!(
            manager
                .disabled_tools
                .read()
                .await
                .contains(&exposed("server1", "tool1")),
            "server1 侧应禁用"
        );
        let (bid, sname, orig) = manager
            .validate_tool_call(&exposed("server2", "tool1"), &serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(
            (bid.as_str(), sname.as_str(), orig.as_str()),
            ("server2", "server2", "tool1")
        );
        // 暴露面：仅 server2__tool1 一条（server1 侧被禁）。
        let names: Vec<String> = manager
            .list_available_tools()
            .await
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        assert_eq!(
            names.iter().filter(|n| n.ends_with("__tool1")).count(),
            1,
            "暴露面 *__tool1 应恰一次: {names:?}"
        );
        assert!(names.contains(&exposed("server2", "tool1")));
    }

    /// 对原始名 forbid 时，即便配了 alias，alias 也被抑制（forbid 优先于 alias）。
    #[tokio::test]
    async fn test_forbidden_original_name_suppresses_alias() {
        let manager = MCPServerManager::new();
        setup_and_refresh(
            &manager,
            vec![(
                "srv",
                vec![tool_named("tool5")],
                stdio_cfg(
                    "srv",
                    vec!["tool5".to_string()],
                    meta_with_alias("tool5", "aliased_tool"),
                ),
            )],
        )
        .await
        .expect("refresh ok");

        let names: Vec<String> = manager
            .list_available_tools()
            .await
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        assert!(
            !names.contains(&exposed("srv", "aliased_tool"))
                && !names.contains(&exposed("srv", "tool5")),
            "forbid 原始名应同时抑制 alias: {names:?}"
        );
        let routes = manager.tool_routes.read().await;
        assert!(
            !routes.contains_key(&exposed("srv", "aliased_tool"))
                && !routes.contains_key(&exposed("srv", "tool5"))
        );
    }

    /// 单 server 工具被 forbid（按原始名）：不路由、禁用集含 exposed 名、调用 PermissionError。
    #[tokio::test]
    async fn test_forbidden_tool_without_provider_stays_disabled() {
        let manager = MCPServerManager::new();
        setup_and_refresh(
            &manager,
            vec![(
                "server1",
                vec![tool_named("tool2")],
                stdio_cfg("server1", vec!["tool2".to_string()], HashMap::new()),
            )],
        )
        .await
        .expect("refresh ok");

        assert!(manager
            .disabled_tools
            .read()
            .await
            .contains(&exposed("server1", "tool2")));
        assert!(!manager
            .tool_routes
            .read()
            .await
            .contains_key(&exposed("server1", "tool2")));
        let err = manager
            .validate_tool_call(&exposed("server1", "tool2"), &serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(
            matches!(err, ComputerError::PermissionError(_)),
            "禁用工具调用应 PermissionError，实得: {err:?}"
        );
    }

    /// 禁用工具不出现在 `list_available_tools` 暴露面（不可见且不可调用）；同 server 未禁用工具正常暴露。
    #[tokio::test]
    async fn test_forbidden_tool_not_in_available_list() {
        let manager = MCPServerManager::new();
        setup_and_refresh(
            &manager,
            vec![(
                "server1",
                vec![tool_named("tool2"), tool_named("safe")],
                stdio_cfg("server1", vec!["tool2".to_string()], HashMap::new()),
            )],
        )
        .await
        .expect("refresh ok");

        let names: Vec<String> = manager
            .list_available_tools()
            .await
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        assert!(
            !names.contains(&exposed("server1", "tool2")),
            "禁用工具不应出现在暴露面: {names:?}"
        );
        assert!(
            names.contains(&exposed("server1", "safe")),
            "未禁用工具应正常暴露: {names:?}"
        );
    }

    /// 按 **exposed_tool_name** forbid（协议 0.3.0：forbidden_tools 匹配 original 或 exposed）：
    /// 该 aliased 工具被禁用、不暴露、不路由、调用 PermissionError。
    #[tokio::test]
    async fn test_forbidden_by_exposed_name_disables_aliased_tool() {
        let manager = MCPServerManager::new();
        setup_and_refresh(
            &manager,
            vec![(
                "srv",
                vec![tool_named("tool5")],
                stdio_cfg(
                    "srv",
                    vec![exposed("srv", "aliased_tool")],
                    meta_with_alias("tool5", "aliased_tool"),
                ),
            )],
        )
        .await
        .expect("refresh ok");

        let names: Vec<String> = manager
            .list_available_tools()
            .await
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        assert!(
            !names.contains(&exposed("srv", "aliased_tool"))
                && !names.contains(&exposed("srv", "tool5")),
            "按 exposed 名 forbid 应禁用该工具且原始名也不出现: {names:?}"
        );
        assert!(manager
            .disabled_tools
            .read()
            .await
            .contains(&exposed("srv", "aliased_tool")));
        assert!(!manager
            .tool_routes
            .read()
            .await
            .contains_key(&exposed("srv", "aliased_tool")));
        let err = manager
            .validate_tool_call(&exposed("srv", "aliased_tool"), &serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, ComputerError::PermissionError(_)));
    }

    /// 便捷：构造带 `default_tool_meta`（可含 per-tool `tool_meta`）的 Stdio 配置。
    fn stdio_cfg_with_default_meta(
        name: &str,
        tool_meta: HashMap<String, ToolMeta>,
        default_tool_meta: Option<ToolMeta>,
    ) -> MCPServerConfig {
        let mut c = StdioServerConfig::new(
            name,
            StdioServerParameters {
                command: "echo".to_string(),
                args: vec![],
                env: HashMap::new(),
                cwd: None,
            },
        );
        c.tool_meta = tool_meta;
        c.default_tool_meta = default_tool_meta;
        MCPServerConfig::Stdio(c)
    }

    /// #134 复现（回归守护）：一 server 暴露 ≥2 工具 + `default_tool_meta.alias` → alias 天生 per-tool，
    /// 绝不从 default 继承，故各工具按**原始名**暴露、无一塌名丢弃。旧行为下三者塌成同一个 `srv__custom`、
    /// first-wins 静默丢 2/3（该测试在旧码红、修后绿）。
    #[tokio::test]
    async fn test_default_tool_meta_alias_no_collapse_all_tools_exposed() {
        let manager = MCPServerManager::new();
        let cfg = stdio_cfg_with_default_meta(
            "srv",
            HashMap::new(),
            Some(ToolMeta {
                alias: Some("custom".to_string()),
                ..ToolMeta::new()
            }),
        );
        setup_and_refresh(
            &manager,
            vec![(
                "srv",
                vec![tool_named("alpha"), tool_named("beta"), tool_named("gamma")],
                cfg,
            )],
        )
        .await
        .expect("refresh ok");

        let names: Vec<String> = manager
            .list_available_tools()
            .await
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();

        // 三工具均按原始名暴露，无一塌名到 srv__custom。
        for tool in ["alpha", "beta", "gamma"] {
            assert!(
                names.contains(&exposed("srv", tool)),
                "工具 {tool} 应按原始名暴露: {names:?}"
            );
        }
        assert!(
            !names.contains(&exposed("srv", "custom")),
            "default alias 不应产生 srv__custom: {names:?}"
        );
        assert_eq!(names.len(), 3, "恰好三个工具、无丢弃: {names:?}");

        // 每个 exposed 名都能路由回各自 original（真·可调用，非仅出现在列表）。
        for tool in ["alpha", "beta", "gamma"] {
            let (b, _s, orig) = manager
                .validate_tool_call(&exposed("srv", tool), &serde_json::json!({}))
                .await
                .unwrap();
            assert_eq!((b.as_str(), orig.as_str()), ("srv", tool));
        }
    }

    /// #134 边界：per-tool `tool_meta[alpha].alias` 仍优先生效，且不受「default alias 被忽略」牵连——
    /// `alpha` 暴露 `srv__renamed`、`beta` 暴露原始名 `srv__beta`，无塌名、default alias 不出线。
    /// 镜像 python `test_per_tool_alias_wins_over_default_alias_no_collapse`。
    #[tokio::test]
    async fn test_per_tool_alias_wins_over_ignored_default_alias() {
        let manager = MCPServerManager::new();
        let cfg = stdio_cfg_with_default_meta(
            "srv",
            meta_with_alias("alpha", "renamed"),
            Some(ToolMeta {
                alias: Some("custom".to_string()),
                ..ToolMeta::new()
            }),
        );
        setup_and_refresh(
            &manager,
            vec![("srv", vec![tool_named("alpha"), tool_named("beta")], cfg)],
        )
        .await
        .expect("refresh ok");

        let names: Vec<String> = manager
            .list_available_tools()
            .await
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        assert!(
            names.contains(&exposed("srv", "renamed")),
            "alpha 应用 per-tool alias: {names:?}"
        );
        assert!(
            names.contains(&exposed("srv", "beta")),
            "beta 应按原始名暴露: {names:?}"
        );
        assert!(
            !names.contains(&exposed("srv", "custom")),
            "default alias 应被忽略: {names:?}"
        );
        assert!(
            !names.contains(&exposed("srv", "alpha")),
            "alpha 原始名已被 per-tool alias 取代: {names:?}"
        );
        assert_eq!(names.len(), 2, "{names:?}");
    }

    /// #134：`default_tool_meta.alias` 被忽略（alias 天生 per-tool），工具按原始名暴露；forbid 原始名仍抑制。
    /// （本测试原名 `..._flows_through_exposure_and_forbid` + 原断言恰是旧 bug 行为的活文档，随 #134 重写。）
    #[tokio::test]
    async fn test_default_tool_meta_alias_ignored_original_exposed_and_forbid() {
        let cfg_default = |forbidden: Vec<String>| {
            let mut c = StdioServerConfig::new(
                "srv",
                StdioServerParameters {
                    command: "echo".to_string(),
                    args: vec![],
                    env: HashMap::new(),
                    cwd: None,
                },
            );
            c.forbidden_tools = forbidden;
            c.default_tool_meta = Some(ToolMeta {
                alias: Some("def_alias".to_string()),
                ..ToolMeta::new()
            });
            MCPServerConfig::Stdio(c)
        };

        // (a) default alias 被忽略 → 工具按原始名暴露（带 bundle_id 前缀），def_alias 不出线。
        let mgr_a = MCPServerManager::new();
        inject_tools(&mgr_a, &bid("srv"), vec![tool_named("t")]).await;
        mgr_a
            .servers_config
            .write()
            .await
            .insert(bid("srv"), cfg_default(vec![]));
        mgr_a.refresh_tool_routes().await.expect("refresh ok");
        let names: Vec<String> = mgr_a
            .list_available_tools()
            .await
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        assert!(
            names.contains(&exposed("srv", "t")) && !names.contains(&exposed("srv", "def_alias")),
            "default alias 应被忽略、工具按原始名暴露: {names:?}"
        );

        // (b) 对原始名 forbid 时该工具被抑制（default alias 既已忽略、暴露名即原始名）/ forbidding original suppresses it
        let mgr_b = MCPServerManager::new();
        inject_tools(&mgr_b, &bid("srv"), vec![tool_named("t")]).await;
        mgr_b
            .servers_config
            .write()
            .await
            .insert(bid("srv"), cfg_default(vec!["t".to_string()]));
        mgr_b.refresh_tool_routes().await.expect("refresh ok");
        let names2: Vec<String> = mgr_b
            .list_available_tools()
            .await
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        assert!(
            !names2.contains(&exposed("srv", "def_alias"))
                && !names2.contains(&exposed("srv", "t")),
            "forbid 原始名应抑制该工具（def_alias 本就忽略、不出线）: {names2:?}"
        );
    }

    /// 协议 0.3.0 前缀化消除了旧的「跨 server alias/原始名撞名」难题：A 的 alias `shared` → `serverA__shared`，
    /// B 的原始名 `shared` → `serverB__shared`，两者天然不同键、各自独立路由/禁用，无需对账（对比 #116 前需消歧）。
    #[tokio::test]
    async fn test_alias_and_other_server_original_never_collide() {
        let manager = MCPServerManager::new();
        let res = setup_and_refresh(
            &manager,
            vec![
                // server A：原始名 x，alias 为 shared → serverA__shared
                (
                    "serverA",
                    vec![tool_named("x")],
                    stdio_cfg("serverA", vec![], meta_with_alias("x", "shared")),
                ),
                // server B：原始名就叫 shared，被 B forbid → serverB__shared 禁用
                (
                    "serverB",
                    vec![tool_named("shared")],
                    stdio_cfg("serverB", vec!["shared".to_string()], HashMap::new()),
                ),
            ],
        )
        .await;

        assert!(res.is_ok(), "前缀化后不应冲突: {res:?}");
        // A 的 alias 独立路由到 serverA__shared → 解析回 A 的原始名 x。
        let (bid, sname, orig) = manager
            .validate_tool_call(&exposed("serverA", "shared"), &serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(
            (bid.as_str(), sname.as_str(), orig.as_str()),
            ("serverA", "serverA", "x")
        );
        // B 的原始名 shared 被 forbid → serverB__shared 禁用、不路由（与 A 侧互不影响）。
        assert!(manager
            .disabled_tools
            .read()
            .await
            .contains(&exposed("serverB", "shared")));
        assert!(!manager
            .tool_routes
            .read()
            .await
            .contains_key(&exposed("serverB", "shared")));
    }
}
