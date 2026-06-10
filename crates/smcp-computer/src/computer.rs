/*!
* 文件名: computer
* 作者: JQQ
* 创建日期: 2025/12/16
* 最后修改日期: 2025/12/16
* 版权: 2023 JQQ. All rights reserved.
* 依赖: tokio, async-trait, serde, tracing
* 描述: Computer核心模块实现 / Core Computer module implementation
*/

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::RwLock as StdRwLock;
use tokio::sync::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

// INT-01 #68：SKILL / blob 子系统编排 / SKILL & blob subsystem orchestration。
use crate::blob::{
    decode_blob_handle, default_thresholds, encode_toolspool_handle, BlobHandleError, BlobResolver,
    BlobThresholds, BlobTooLargeError, DecodedHandle, ResolvedBlob, SkillBlobResolver,
    SkillRootLookup, ToolspoolBlobResolver, ToolspoolBlobStore,
};
use crate::skills::{
    resolve_skill_home, resolve_skill_view, stage_mcp_skills, stage_user_skills, user_dropin_root,
    workdir_skill_root, AsyncCallback, CallbackResult, OnChange, SkillEventDebouncer,
    SkillFileWatcher, SkillRegistry, SkillResourceView, SkillSandboxError, SOURCE_USER,
};
use smcp::utils::env_truthy;
use smcp::A2CSkillRef;

use crate::errors::{ComputerError, ComputerResult};
use crate::inputs::handler::InputHandler;
use crate::inputs::load_env_file;
use crate::inputs::model::InputValue;
use crate::inputs::utils::run_command;
use crate::mcp_clients::{
    manager::MCPServerManager,
    model::{
        content_as_text, is_call_tool_error, CallToolResult, CancellableCallOutcome, Content,
        MCPServerConfig, MCPServerInput, ReadResourceResult, Resource, Tool,
    },
    ConfigRender, RenderError,
};
use crate::socketio_client::{SmcpComputerClient, SmcpComputerClientBuilder};

/// 确认回调函数类型 / Confirmation callback function type
type ConfirmCallbackType = Arc<dyn Fn(&str, &str, &str, &serde_json::Value) -> bool + Send + Sync>;

/// 解析 "key:value,foo:bar" 格式的 headers 字符串为 HashMap
/// Parse "key:value,foo:bar" format headers string into HashMap
fn parse_headers_string(headers: &str) -> HashMap<String, String> {
    headers
        .split(',')
        .filter_map(|pair| {
            let mut parts = pair.splitn(2, ':');
            match (parts.next(), parts.next()) {
                (Some(k), Some(v)) if !k.trim().is_empty() => {
                    Some((k.trim().to_string(), v.trim().to_string()))
                }
                _ => None,
            }
        })
        .collect()
}

/// 将 InputValue 转换为 serde_json::Value / Convert InputValue to serde_json::Value
fn input_value_to_json(value: InputValue) -> serde_json::Value {
    match value {
        InputValue::String(s) => serde_json::Value::String(s),
        InputValue::Number(n) => serde_json::Value::Number(serde_json::Number::from(n)),
        InputValue::Float(f) => serde_json::Value::Number(
            serde_json::Number::from_f64(f).unwrap_or(serde_json::Number::from(0)),
        ),
        InputValue::Bool(b) => serde_json::Value::Bool(b),
    }
}

/// 将 serde_json::Value 转换为 InputValue / Convert serde_json::Value to InputValue
fn json_to_input_value(value: serde_json::Value) -> ComputerResult<InputValue> {
    match value {
        serde_json::Value::String(s) => Ok(InputValue::String(s)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(InputValue::Number(i))
            } else if let Some(u) = n.as_u64() {
                Ok(InputValue::Number(u as i64))
            } else if let Some(f) = n.as_f64() {
                Ok(InputValue::Float(f))
            } else {
                Err(ComputerError::ValidationError(
                    "Invalid number value".to_string(),
                ))
            }
        }
        serde_json::Value::Bool(b) => Ok(InputValue::Bool(b)),
        serde_json::Value::Null => Err(ComputerError::ValidationError(
            "Null value not supported".to_string(),
        )),
        _ => Err(ComputerError::ValidationError(
            "Unsupported value type".to_string(),
        )),
    }
}

/// 工具调用历史记录 / Tool call history record
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRecord {
    /// 时间戳 / Timestamp
    pub timestamp: DateTime<Utc>,
    /// 请求ID / Request ID
    pub req_id: String,
    /// 服务器名称 / Server name
    pub server: String,
    /// 工具名称 / Tool name
    pub tool: String,
    /// 参数 / Parameters
    pub parameters: serde_json::Value,
    /// 超时时间 / Timeout
    pub timeout: Option<f64>,
    /// 是否成功 / Success
    pub success: bool,
    /// 错误信息 / Error message
    pub error: Option<String>,
}

/// Session trait - 用于抽象不同的交互环境（CLI、GUI、Web）
/// Session trait - Abstract different interaction environments (CLI, GUI, Web)
#[async_trait]
pub trait Session: Send + Sync {
    /// 解析输入值 / Resolve input value
    async fn resolve_input(&self, input: &MCPServerInput) -> ComputerResult<serde_json::Value>;

    /// 获取会话ID / Get session ID
    fn session_id(&self) -> &str;
}

/// 默认的静默Session实现 / Default silent session implementation
///
/// `Clone`：socketio 接线（#72）的 [`Computer::clone_for_handlers`] 需克隆 Session 以构造
/// socketio-detached 句柄；`SilentSession` 仅持 `id`，克隆无副作用。自定义 Session 若要接 socketio
/// blob/skill/cancel handler，亦须可 `Clone`（handler 路径**不**触碰 session，克隆体仅占位）。
#[derive(Clone)]
pub struct SilentSession {
    id: String,
}

impl SilentSession {
    /// 创建新的静默Session / Create new silent session
    pub fn new(id: impl Into<String>) -> Self {
        Self { id: id.into() }
    }
}

#[async_trait]
impl Session for SilentSession {
    async fn resolve_input(&self, input: &MCPServerInput) -> ComputerResult<serde_json::Value> {
        // 静默Session只使用默认值 / Silent session only uses default values
        match input {
            MCPServerInput::PromptString(input) => Ok(serde_json::Value::String(
                input.default.clone().unwrap_or_default(),
            )),
            MCPServerInput::PickString(input) => Ok(serde_json::Value::String(
                input
                    .default
                    .clone()
                    .unwrap_or_else(|| input.options.first().cloned().unwrap_or_default()),
            )),
            MCPServerInput::Command(input) => {
                // 静默Session执行命令并返回输出 / Silent session executes command and returns output
                let args: Vec<String> = input
                    .args
                    .as_ref()
                    .map(|m| {
                        let mut sorted_pairs: Vec<_> = m.iter().collect();
                        sorted_pairs.sort_by_key(|(k, _)| *k);
                        sorted_pairs.into_iter().map(|(_, v)| v.clone()).collect()
                    })
                    .unwrap_or_default();
                match run_command(&input.command, &args).await {
                    Ok(output) => Ok(serde_json::Value::String(output)),
                    Err(e) => Err(ComputerError::RuntimeError(format!(
                        "Failed to execute command '{}': {}",
                        input.command, e
                    ))),
                }
            }
        }
    }

    fn session_id(&self) -> &str {
        &self.id
    }
}

/// Computer核心结构体 / Core Computer struct
pub struct Computer<S: Session> {
    /// 计算机名称 / Computer name
    name: String,
    /// MCP服务器管理器 / MCP server manager
    mcp_manager: Arc<RwLock<Option<MCPServerManager>>>,
    /// 输入定义映射 / Input definitions map (id -> input)
    /// 使用 Arc 以便与 Socket.IO 客户端共享
    /// Using Arc to share with Socket.IO client
    inputs: Arc<RwLock<HashMap<String, MCPServerInput>>>,
    /// MCP服务器配置映射 / MCP server configurations map (name -> config)
    mcp_servers: RwLock<HashMap<String, MCPServerConfig>>,
    /// 输入处理器 / Input handler
    input_handler: Arc<RwLock<InputHandler>>,
    /// 自动连接标志 / Auto connect flag
    auto_connect: bool,
    /// 自动重连标志 / Auto reconnect flag
    auto_reconnect: bool,
    /// 工具调用历史 / Tool call history
    tool_history: Arc<Mutex<Vec<ToolCallRecord>>>,
    /// Session实例 / Session instance
    session: S,
    /// Socket.IO客户端引用 / Socket.IO client reference
    /// 使用 Arc 而不是 Weak 以确保 client 生命周期
    /// Using Arc instead of Weak to ensure client lifetime
    socketio_client: Arc<RwLock<Option<Arc<SmcpComputerClient>>>>,
    /// 确认回调函数 / Confirmation callback function
    confirm_callback: Option<ConfirmCallbackType>,

    // ── INT-01 #68：SKILL 子系统 / SKILL subsystem ──────────────────────────
    /// SKILL 物化索引（name → A2CSkillRef）。`tokio::RwLock`：读路径（get_skills）取读锁、
    /// stage/reconcile 取写锁跨 await（async 守卫 `Send`，安全）/ materialized Registry。
    skill_registry: Arc<RwLock<SkillRegistry>>,
    /// SKILL Home 绝对根（boot_up 解析；`std::RwLock` 配置态、同步访问不跨 await）/ SKILL Home root。
    skill_home: Arc<StdRwLock<Option<PathBuf>>>,
    /// SKILL Home 覆盖（测试/部署注入）/ home override。
    skill_home_override: Option<PathBuf>,
    /// 已登记工作目录（能力发现并集：user DropIn 扫描 + watcher 跨此）/ registered workdirs。
    registered_workdirs: Arc<StdRwLock<Vec<PathBuf>>>,
    /// 多源 SKILL 变更去抖器（标脏 → 窗口合并 → invalidate 重扫 + 单次 emit）；`Arc` 供 watcher 线程共享。
    skill_debouncer: Arc<SkillEventDebouncer>,
    /// user 源 DropIn 文件 watcher（boot 启、shutdown 停）/ file watcher。
    skill_watcher: Arc<Mutex<Option<SkillFileWatcher>>>,
    /// 原生 Observer 不支持的 FS 切 polling 兜底 / polling fallback flag。
    skill_watch_polling: bool,

    // ── INT-01 #68：通用二进制传输 / generic blob transfer ───────────────────
    /// `.blobspool` 缓存根覆盖（缺省 `~/.a2c`）；boot 时建 store / blob cache root override。
    blob_cache_root_override: Option<PathBuf>,
    /// SKILL / blob 阈值（inline / too_large / chunk_max）/ thresholds。
    blob_thresholds: BlobThresholds,
    /// 内容寻址暂存（boot 时建；mint 时写入）/ toolspool store (built at boot)。
    toolspool_store: Arc<RwLock<Option<Arc<ToolspoolBlobStore>>>>,
    /// kind → resolver 派发表（boot 时装配 toolspool；skill 由 resolve_blob async 处理）/ resolver table。
    blob_resolvers: Arc<RwLock<HashMap<String, Arc<dyn BlobResolver>>>>,

    // ── INT-02 #70：tool_call 取消最后一公里 / tool_call cancellation last-mile ──────────
    /// 在途可取消工具调用注册表（`req_id` → [`CancellationToken`]），响应 `notify:tool_call_cancel`。
    /// `std::sync::Mutex`：临界区仅 HashMap 增删查、**不跨 await**（故 RAII 退场守卫可同步注销）；
    /// `Arc` 共享——clone 体与原 Computer 命中**同一**注册表，使任意 clone 上的 `acancel_tool` 生效。
    /// In-flight cancellable tool-call registry; `acancel_tool` fires the matching token.
    inflight_tool_tasks: Arc<StdMutex<HashMap<String, CancellationToken>>>,
}

/// 孤儿对账（按源谓词限定）：当前活跃、`source_pred(source)` 命中、但本轮 `present` 未出现的 SKILL →
/// `mark_orphan`（消失即从 `get_skills` 排除；恢复由 staging 的 `register_or_update` 命中孤儿自动完成）。
/// 自由函数——供去抖器 `invalidate` 回调（无 `self`）与 [`Computer`] 方法共用 / shared free fn (no `self`)。
fn reconcile_orphans_in(
    registry: &mut SkillRegistry,
    present: &HashSet<String>,
    source_pred: impl Fn(&str) -> bool,
) {
    let to_orphan: Vec<String> = registry
        .active_refs()
        .into_iter()
        .filter(|r| !r.name.is_empty() && !present.contains(&r.name) && source_pred(&r.source))
        .map(|r| r.name)
        .collect();
    for name in to_orphan {
        registry.mark_orphan(&name);
    }
}

/// 合并 VS Code 风格 `envFile` 的 `KEY=VALUE` 进 stdio `server_parameters.env`（显式 env 胜，§9.1）/
/// merge a VS Code-style envFile's KEY=VALUE into a stdio server's env (explicit env wins)。
///
/// 在 [`render_server_config`](Computer::render_server_config) 渲染后、反序列化前作用于 JSON 值：envFile
/// 路径此时已展开（`${workspaceFolder}` 等）。仅对 stdio（`server_parameters` 含 `env`/`command`）生效；
/// 置于 sse/http 上记 WARN + 原样返回。envFile 缺失 / 空 / 文件为空 → 原样返回。对标 Python `_apply_env_file`。
fn apply_env_file(mut rendered: serde_json::Value) -> serde_json::Value {
    let Some(env_file) = rendered
        .get("envFile")
        .or_else(|| rendered.get("env_file"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
    else {
        return rendered;
    };

    // stdio 判定：server_parameters 为对象且含 `env` 或 `command`（sse/http 无此二者）。
    let is_stdio = rendered
        .get("server_parameters")
        .and_then(|p| p.as_object())
        .is_some_and(|p| p.contains_key("env") || p.contains_key("command"));
    if !is_stdio {
        let name = rendered
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let srv_type = rendered
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        warn!("envFile 在非 stdio（{srv_type}）server 上不适用，已忽略: {name} / envFile ignored on non-stdio");
        return rendered;
    }

    let file_env = load_env_file(std::path::Path::new(&env_file));
    if file_env.is_empty() {
        return rendered;
    }

    if let Some(params) = rendered
        .get_mut("server_parameters")
        .and_then(|p| p.as_object_mut())
    {
        // 显式 env 同名项胜：先铺 envFile，再以显式 env 覆盖 / explicit env wins.
        let explicit = params
            .get("env")
            .and_then(|e| e.as_object())
            .cloned()
            .unwrap_or_default();
        let mut merged = serde_json::Map::new();
        for (k, v) in file_env {
            merged.insert(k, serde_json::Value::String(v));
        }
        for (k, v) in explicit {
            merged.insert(k, v);
        }
        params.insert("env".to_string(), serde_json::Value::Object(merged));
    }
    rendered
}

/// 构造 SKILL 去抖器（回调捕获共享 `Arc` 句柄克隆，非 `Computer` 本体——杜绝强引用环）/ Build the debouncer。
///
/// `on_emit` → 读 Socket.IO 客户端发 `server:update_skills`；`invalidate` → 重扫 user 源 DropIn + 对账孤儿。
/// `new()` 与 `Clone` 共用（`SkillEventDebouncer` 非 `Clone`，clone 时按相同共享句柄重建）。
fn build_skill_debouncer(
    skill_registry: &Arc<RwLock<SkillRegistry>>,
    skill_home: &Arc<StdRwLock<Option<PathBuf>>>,
    registered_workdirs: &Arc<StdRwLock<Vec<PathBuf>>>,
    socketio_client: &Arc<RwLock<Option<Arc<SmcpComputerClient>>>>,
) -> SkillEventDebouncer {
    let emit_client = socketio_client.clone();
    let on_emit: AsyncCallback = Arc::new(
        move || -> Pin<Box<dyn Future<Output = CallbackResult> + Send>> {
            let client_ref = emit_client.clone();
            Box::pin(async move {
                let guard = client_ref.read().await;
                if let Some(client) = guard.as_ref() {
                    if let Err(e) = client.emit_update_skills().await {
                        debug!(error = %e, "emit_update_skills failed, skipped");
                    }
                }
                Ok(())
            })
        },
    );

    let inv_registry = skill_registry.clone();
    let inv_home = skill_home.clone();
    let inv_workdirs = registered_workdirs.clone();
    let invalidate: AsyncCallback = Arc::new(
        move || -> Pin<Box<dyn Future<Output = CallbackResult> + Send>> {
            let registry = inv_registry.clone();
            let home_cell = inv_home.clone();
            let workdirs_cell = inv_workdirs.clone();
            Box::pin(async move {
                let home = home_cell.read().expect("skill_home poisoned").clone();
                let Some(home) = home else {
                    return Ok(());
                };
                let workdirs = workdirs_cell.read().expect("workdirs poisoned").clone();
                let mut reg = registry.write().await;
                let discovered: HashSet<String> = stage_user_skills(&mut reg, &home, &workdirs)
                    .into_iter()
                    .collect();
                reconcile_orphans_in(&mut reg, &discovered, |s| s == SOURCE_USER);
                Ok(())
            })
        },
    );

    SkillEventDebouncer::builder(on_emit)
        .invalidate(invalidate)
        .build()
}

/// `mint_toolspool_handle` 失败 / toolspool mint failure。
#[derive(Debug, thiserror::Error)]
pub enum BlobMintError {
    /// 超 `too_large_cap` —— 拒绝铸造、**不写盘**（DoS 防御，blob-transfer §3）/ too large, no write。
    #[error(transparent)]
    TooLarge(#[from] BlobTooLargeError),
    /// blob 子系统未初始化（须先 `boot_up`）/ subsystem not initialized。
    #[error("blob subsystem not initialized (call boot_up first)")]
    NotBooted,
    /// toolspool 写盘失败 / store write failed。
    #[error("toolspool store error: {0}")]
    Store(String),
}

/// 默认 blob 缓存根 `~/.a2c`（`.blobspool/` 挂其下）/ default blob cache root (`~/.a2c`)。
fn default_blob_cache_root() -> PathBuf {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".a2c")
}

/// 一次性根 lookup（把 sync [`SkillRootLookup`] 适配到 async 锁的 registry：[`Computer::resolve_blob`] 先
/// async 解析 skill 根再注入此 lookup）/ A one-shot root lookup adapting the sync trait to the locked registry。
struct PreresolvedRoot(Option<PathBuf>);

impl SkillRootLookup for PreresolvedRoot {
    fn lookup_root(&self, _name: &str) -> Option<PathBuf> {
        self.0.clone()
    }
}

/// 在 rmcp [`CallToolResult`] 的**结果级** `meta` 写取消标记（SMCP-07 键，data-structures.md §结果级 meta）。
///
/// computer.rs 的结果流是 rmcp `CallToolResult`（其 `meta` 为 `rmcp::model::Meta`，**非** smcp
/// `ToolCallRet`），故不能直接调用 [`smcp::ToolCallRet::mark_cancelled`]；本 helper 用**同名键**就地写入，
/// 在线（wire）形态与之等价（`meta.a2c_cancelled=true` + `a2c_cancel_reason`）。
fn mark_result_cancelled(result: &mut CallToolResult, reason: &str) {
    let meta = result.meta.get_or_insert_with(rmcp::model::Meta::new);
    meta.insert(
        smcp::tool_meta::A2C_CANCELLED_KEY.to_string(),
        serde_json::Value::Bool(true),
    );
    meta.insert(
        smcp::tool_meta::A2C_CANCEL_REASON_KEY.to_string(),
        serde_json::Value::String(reason.to_string()),
    );
}

/// 在 rmcp [`CallToolResult`] 的结果级 `meta` 写超时标记 `a2c_timeout=true`（SHOULD）/ mark timeout。
fn mark_result_timeout(result: &mut CallToolResult) {
    result
        .meta
        .get_or_insert_with(rmcp::model::Meta::new)
        .insert(
            smcp::tool_meta::A2C_TIMEOUT_KEY.to_string(),
            serde_json::Value::Bool(true),
        );
}

/// RAII 守卫：[`Computer::execute_tool_cancellable`] 从**任意**路径退出（正常完成 / `?` 早返回 / 本 future
/// 被 drop 的「外层断连/teardown」）时注销其在途取消令牌，使注册表不泄露。Drop 即便在 future 被 drop 时
/// 也会运行——这正是 tokio 下「外层取消」的判别：本 future 消失 ⇒ 无取消态结果被产生（不伪装），与 Python
/// 用 `current_task().cancelling()` 区分外层取消等效。
struct InflightCancelGuard {
    registry: Arc<StdMutex<HashMap<String, CancellationToken>>>,
    req_id: String,
}

impl Drop for InflightCancelGuard {
    fn drop(&mut self) {
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.req_id);
    }
}

impl<S: Session> Computer<S> {
    /// 创建新的Computer实例 / Create new Computer instance
    pub fn new(
        name: impl Into<String>,
        session: S,
        inputs: Option<HashMap<String, MCPServerInput>>,
        mcp_servers: Option<HashMap<String, MCPServerConfig>>,
        auto_connect: bool,
        auto_reconnect: bool,
    ) -> Self {
        let name = name.into();
        let inputs = inputs.unwrap_or_default();
        let mcp_servers = mcp_servers.unwrap_or_default();

        // 共享 Arc 句柄先建，供去抖器回调捕获（避免 Computer ↔ debouncer 强引用环）。
        // Pre-create shared handles so debouncer callbacks capture clones, not Computer itself.
        let skill_registry: Arc<RwLock<SkillRegistry>> =
            Arc::new(RwLock::new(SkillRegistry::new()));
        let skill_home: Arc<StdRwLock<Option<PathBuf>>> = Arc::new(StdRwLock::new(None));
        let registered_workdirs: Arc<StdRwLock<Vec<PathBuf>>> =
            Arc::new(StdRwLock::new(Vec::new()));
        let socketio_client: Arc<RwLock<Option<Arc<SmcpComputerClient>>>> =
            Arc::new(RwLock::new(None));

        let skill_debouncer = Arc::new(build_skill_debouncer(
            &skill_registry,
            &skill_home,
            &registered_workdirs,
            &socketio_client,
        ));

        Self {
            name,
            mcp_manager: Arc::new(RwLock::new(None)),
            inputs: Arc::new(RwLock::new(inputs)),
            mcp_servers: RwLock::new(mcp_servers),
            input_handler: Arc::new(RwLock::new(InputHandler::new())),
            auto_connect,
            auto_reconnect,
            tool_history: Arc::new(Mutex::new(Vec::new())),
            session,
            socketio_client,
            confirm_callback: None,
            skill_registry,
            skill_home,
            skill_home_override: None,
            registered_workdirs,
            skill_debouncer,
            skill_watcher: Arc::new(Mutex::new(None)),
            skill_watch_polling: env_truthy("A2C_SKILL_WATCH_POLLING"),
            blob_cache_root_override: None,
            blob_thresholds: default_thresholds(),
            toolspool_store: Arc::new(RwLock::new(None)),
            blob_resolvers: Arc::new(RwLock::new(HashMap::new())),
            inflight_tool_tasks: Arc::new(StdMutex::new(HashMap::new())),
        }
    }

    /// 注入 SKILL Home 覆盖（测试/部署）/ Inject a SKILL Home override。
    #[must_use]
    pub fn with_skill_home(mut self, home: impl Into<PathBuf>) -> Self {
        self.skill_home_override = Some(home.into());
        self
    }

    /// 注入已登记工作目录（能力发现并集 + watcher 监控根）/ Inject registered workdirs。
    #[must_use]
    pub fn with_registered_workdirs(self, workdirs: impl IntoIterator<Item = PathBuf>) -> Self {
        *self.registered_workdirs.write().expect("workdirs poisoned") =
            workdirs.into_iter().collect();
        self
    }

    /// 注入 blob 缓存根覆盖（缺省 `~/.a2c`）/ Inject a blob cache-root override。
    #[must_use]
    pub fn with_blob_cache_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.blob_cache_root_override = Some(root.into());
        self
    }

    /// 注入 blob 阈值（inline / too_large / chunk_max）/ Inject blob thresholds。
    #[must_use]
    pub fn with_blob_thresholds(mut self, thresholds: BlobThresholds) -> Self {
        self.blob_thresholds = thresholds;
        self
    }

    // ── INT-01 #68：SKILL Home 解析 / SKILL Home resolution ──────────────────
    /// 解析并缓存 SKILL Home（override > env 链）/ Resolve & cache the SKILL Home root。
    fn ensure_skill_home(&self) -> PathBuf {
        {
            let guard = self.skill_home.read().expect("skill_home poisoned");
            if let Some(home) = guard.as_ref() {
                return home.clone();
            }
        }
        let resolved = self
            .skill_home_override
            .clone()
            .unwrap_or_else(|| resolve_skill_home(None));
        *self.skill_home.write().expect("skill_home poisoned") = Some(resolved.clone());
        resolved
    }

    // ── SKILL 通道委托 / SKILL channel delegation（#68，skill.md §9）────────────
    /// 当前活跃 SKILL（排除孤儿；不排序/不去重）—— `client:get_skills` 数据源 / active skills。
    pub async fn get_skills(&self) -> Vec<A2CSkillRef> {
        self.skill_registry.read().await.active_refs()
    }

    /// O(1) 活跃精确解析 `name` → [`A2CSkillRef`]（孤儿/未注册 → `None`；handler 据此回 4014）/ resolve one。
    pub async fn get_skill_ref(&self, name: &str) -> Option<A2CSkillRef> {
        self.skill_registry.read().await.resolve(name)
    }

    /// 沙箱解析 SKILL 包内资源 → 消费字节视图（§9.2 包根**仅**取自 `ref.path`；带 too_large 守卫）/ read resource。
    ///
    /// # Errors
    /// 沙箱拒绝（traversal/forbidden/not_found）或 too_large → [`SkillSandboxError`]（handler 映射 4017）。
    pub fn read_skill_resource(
        &self,
        skill_ref: &A2CSkillRef,
        rel_path: Option<&str>,
    ) -> Result<SkillResourceView, SkillSandboxError> {
        let root = std::path::Path::new(&skill_ref.path);
        resolve_skill_view(root, rel_path, Some(self.blob_thresholds.too_large_cap))
    }

    /// 标记 SKILL 集合变更 → 去抖器窗口合并触发单次 `emit_update_skills`（CLI marketplace 变更后调）/ mark dirty。
    pub fn mark_skills_dirty(&self) {
        self.skill_debouncer.mark_dirty();
    }

    /// SKILL Home 绝对根（CLI marketplace/skill 命令经此取物化根）/ the SKILL Home root。
    pub fn skill_home(&self) -> PathBuf {
        self.ensure_skill_home()
    }

    /// 绑定当前任务的 active-workdir 单根（首个登记 workdir；空闲 = `None`）/ the active workdir。
    pub fn active_workdir(&self) -> Option<PathBuf> {
        self.registered_workdirs
            .read()
            .expect("workdirs poisoned")
            .first()
            .cloned()
    }

    /// 单页透传指定 MCP Server 的 `resources/list`（v0.2 `client:get_resources`）/ single-page passthrough。
    ///
    /// Computer 仅作透传层：无 scheme/元数据过滤、无跨 Server 聚合，翻页由调用方经 `cursor` 控制
    /// （首页传 `None`）。
    ///
    /// # Errors
    /// - [`ComputerError::InvalidState`]：MCP Manager 未初始化。
    /// - [`ComputerError::McpServerNotFound`]：`mcp_server` 未注册（handler 映射 4014）。
    /// - [`ComputerError::McpCapabilityNotSupported`]：未声明 `resources` 能力（handler 映射 4015）。
    pub async fn get_resources(
        &self,
        mcp_server: &str,
        cursor: Option<String>,
    ) -> ComputerResult<(Vec<Resource>, Option<String>)> {
        let guard = self.mcp_manager.read().await;
        let Some(manager) = guard.as_ref() else {
            return Err(ComputerError::InvalidState(
                "MCP Manager is not initialized".to_string(),
            ));
        };
        manager.list_resources(mcp_server, cursor).await
    }

    // ── 通用二进制传输 / generic blob transfer（#68，blob-transfer.md §3/§5）─────
    /// blob 阈值（inline / too_large / chunk_max）/ the threshold bundle。
    #[must_use]
    pub fn blob_thresholds(&self) -> BlobThresholds {
        self.blob_thresholds
    }

    /// 铸造 `kind=toolspool` 不透明句柄并写盘 / Mint an opaque toolspool handle。
    ///
    /// `tool_call` 超内联预算二进制经此写入 `.blobspool`，返回句柄走 `_meta.a2c_blob_handle` 旁路。
    /// `len > too_large_cap` → 拒绝、**不写盘**（铸造期 DoS 防御，§3）。
    ///
    /// # Errors
    /// [`BlobMintError`]：too_large / 未 boot / 写盘失败。
    pub async fn mint_toolspool_handle(
        &self,
        payload: &[u8],
        mime: &str,
    ) -> Result<String, BlobMintError> {
        let size = payload.len() as u64;
        let cap = self.blob_thresholds.too_large_cap;
        if size > cap {
            return Err(BlobTooLargeError { size, cap }.into());
        }
        let store = self
            .toolspool_store
            .read()
            .await
            .clone()
            .ok_or(BlobMintError::NotBooted)?;
        let cid = store
            .put(payload, mime)
            .map_err(|e| BlobMintError::Store(e.to_string()))?;
        Ok(encode_toolspool_handle(&cid, mime))
    }

    /// 解码 blob 句柄 → 按 kind 路由解析 / decode → route by kind → resolve。
    ///
    /// `toolspool` → 注册表内 resolver（同步）；`skill` → async 读 registry 取根再跑沙箱（适配 BLB-02 的
    /// **同步** [`SkillRootLookup`] 到 async 锁的 registry）。
    ///
    /// # Errors
    /// 句柄非法 / 不可访问 → [`BlobHandleError`]（handler 映射 flat 4018）。
    pub async fn resolve_blob(&self, handle: &str) -> Result<ResolvedBlob, BlobHandleError> {
        let decoded = decode_blob_handle(handle)?;
        match &decoded {
            DecodedHandle::Toolspool(_) => {
                let resolvers = self.blob_resolvers.read().await;
                let resolver = resolvers.get("toolspool").ok_or_else(|| {
                    BlobHandleError::Gone(
                        "toolspool resolver not initialized (call boot_up first)".into(),
                    )
                })?;
                resolver.resolve(&decoded)
            }
            DecodedHandle::Skill(payload) => {
                let root = self
                    .skill_registry
                    .read()
                    .await
                    .resolve(&payload.name)
                    .map(|r| PathBuf::from(r.path));
                SkillBlobResolver::new(PreresolvedRoot(root)).resolve(&decoded)
            }
        }
    }

    // ── 编排链：去抖结算 + user 源 watcher（#68，设计 §8）─────────────────────
    /// 全量重扫/重物化后的孤儿对账（限定单一源谓词）/ reconcile orphans after a full rescan。
    pub async fn reconcile_orphans(
        &self,
        present_names: HashSet<String>,
        source_pred: impl Fn(&str) -> bool,
    ) {
        let mut reg = self.skill_registry.write().await;
        reconcile_orphans_in(&mut reg, &present_names, source_pred);
    }

    /// 缓存失效：就地重扫 user 源 DropIn + 对账孤儿（SKILL Home 未就绪 → no-op）/ rescan user-source。
    pub async fn invalidate_user_skills(&self) {
        let Some(home) = self.skill_home.read().expect("skill_home poisoned").clone() else {
            return;
        };
        let workdirs = self
            .registered_workdirs
            .read()
            .expect("workdirs poisoned")
            .clone();
        let mut reg = self.skill_registry.write().await;
        let discovered: HashSet<String> = stage_user_skills(&mut reg, &home, &workdirs)
            .into_iter()
            .collect();
        reconcile_orphans_in(&mut reg, &discovered, |s| s == SOURCE_USER);
    }

    /// 物化 mcp 源 `skill://` → 注册进 Registry（全量 → `mcp:` 源孤儿对账）/ materialize & register mcp-source skills。
    ///
    /// `server_name` 给定则仅重物化该 server（单 server 重枚举，不对账）；否则全部活跃 server + 孤儿对账
    /// （本轮未出现的 `mcp:` 源 SKILL → 标孤儿，保留以便 source 回归时恢复）。SKILL Home 未就绪 / 无
    /// manager → 空列表；staging 失败 → 记 ERROR + 空列表（失败隔离，对标 Python `_restage_mcp_skills`）。
    /// 由 boot_up 与 MCP `ResourceListChanged`/`ResourceUpdated` 通知处理器（INT-03 #72）触发。
    ///
    /// **持锁语义（注意）**：`skill_registry` 写锁全程持有跨 `stage_mcp_skills` await——后者 `archive`
    /// 模式会发起归档**网络下载**、`resources` 模式会做 MCP `read_resource`。期间所有 `get_skills` /
    /// `get_skill_ref`（读锁）被阻塞，慢/卡 fetch 会拖住全部 SKILL 读（Python 单事件循环天然串行掩盖此点）。
    /// 锁序安全（唯一同时持 `mcp_manager.read` + `skill_registry.write` 处，无反向获取路径，不构成死锁）。
    /// 两阶段化（先 materialize 全部、仅 register 阶段短持写锁）的硬化见 follow-up issue（触及封板 staging.rs #49）。
    pub async fn restage_mcp_skills(&self, server_name: Option<&str>) -> Vec<String> {
        let Some(home) = self.skill_home.read().expect("skill_home poisoned").clone() else {
            return Vec::new();
        };
        let manager_guard = self.mcp_manager.read().await;
        let Some(manager) = manager_guard.as_ref() else {
            return Vec::new();
        };
        let mut reg = self.skill_registry.write().await;
        let registered = match stage_mcp_skills(manager, &mut reg, &home, server_name, None).await {
            Ok(names) => names,
            Err(e) => {
                error!(error = %e, "restage_mcp_skills failed (non-blocking)");
                return Vec::new();
            }
        };
        // 仅全量重物化做孤儿对账（限定 `mcp:` 源，不误标 user/marketplace）。
        if server_name.is_none() {
            let present: HashSet<String> = registered.iter().cloned().collect();
            reconcile_orphans_in(&mut reg, &present, |s| s.starts_with("mcp:"));
        }
        registered
    }

    /// 去抖器结算末端：推送 `server:update_skills`（无 client / 未入房间 → no-op）/ emit now。
    pub async fn emit_update_skills_now(&self) {
        let guard = self.socketio_client.read().await;
        if let Some(client) = guard.as_ref() {
            if let Err(e) = client.emit_update_skills().await {
                debug!(error = %e, "emit_update_skills failed, skipped");
            }
        }
    }

    /// 给 SKILL 文件 watcher 打内部写标记（避免自写触发重载循环）/ mark an internal write。
    pub async fn mark_skill_internal_write(&self, path: impl AsRef<std::path::Path>) {
        let guard = self.skill_watcher.lock().await;
        if let Some(watcher) = guard.as_ref() {
            watcher.mark_internal_write(path);
        }
    }

    /// 启动 user 源 DropIn 文件 watcher（监控 `<home>/user/` + 各登记 `<workdir>/.tfrobot/skills/`）/ start watcher。
    ///
    /// watcher 回调在独立线程触发 → 直接调去抖器 `mark_dirty`（`Send + Sync`、内部锁，线程安全）。已有 watcher
    /// → 先停。SKILL Home 未就绪 → no-op。
    async fn start_skill_watcher(&self) {
        let Some(home) = self.skill_home.read().expect("skill_home poisoned").clone() else {
            return;
        };
        let workdirs = self
            .registered_workdirs
            .read()
            .expect("workdirs poisoned")
            .clone();
        let debouncer = Arc::clone(&self.skill_debouncer);
        let on_change: OnChange = Arc::new(move || debouncer.mark_dirty());
        let mut watcher = SkillFileWatcher::builder(on_change)
            .use_polling(self.skill_watch_polling)
            .build();
        let mut roots: Vec<PathBuf> = vec![user_dropin_root(&home)];
        roots.extend(workdirs.iter().map(|wd| workdir_skill_root(wd)));
        if let Err(e) = watcher.watch(roots) {
            warn!(error = %e, "SKILL file watcher start failed, skipped");
            return;
        }
        let mut guard = self.skill_watcher.lock().await;
        if let Some(old) = guard.as_mut() {
            old.stop();
        }
        *guard = Some(watcher);
    }

    /// 设置确认回调函数 / Set confirmation callback function
    pub fn with_confirm_callback<F>(mut self, callback: F) -> Self
    where
        F: Fn(&str, &str, &str, &serde_json::Value) -> bool + Send + Sync + 'static,
    {
        self.confirm_callback = Some(Arc::new(callback));
        self
    }

    /// 获取计算机名称 / Get computer name
    pub fn name(&self) -> &str {
        &self.name
    }

    /// 获取 Socket.IO 客户端引用 / Get Socket.IO client reference
    /// 返回 Arc 包装的客户端，确保其生命周期
    /// Returns Arc-wrapped client, ensuring its lifetime
    pub fn get_socketio_client(&self) -> Arc<RwLock<Option<Arc<SmcpComputerClient>>>> {
        self.socketio_client.clone()
    }

    /// 启动Computer / Boot up the computer
    pub async fn boot_up(&self) -> ComputerResult<()> {
        info!("Starting Computer: {}", self.name);

        // 创建MCP服务器管理器 / Create MCP server manager
        let manager = MCPServerManager::new();

        // 渲染并验证服务器配置 / Render and validate server configurations
        let servers = self.mcp_servers.read().await;
        let mut validated_servers = Vec::new();

        for (_name, server_config) in servers.iter() {
            match self.render_server_config(server_config).await {
                Ok(validated) => validated_servers.push(validated),
                Err(e) => {
                    error!(
                        "Failed to render server config {}: {}",
                        server_config.name(),
                        e
                    );
                    // 保留原配置作为回退 / Keep original config as fallback
                    validated_servers.push(server_config.clone());
                }
            }
        }

        // 初始化管理器 / Initialize manager
        manager.initialize(validated_servers).await?;

        // 设置管理器到实例 / Set manager to instance
        *self.mcp_manager.write().await = Some(manager);

        // ── INT-01 #68：SKILL / blob 子系统装配 / SKILL & blob subsystem wiring ──
        // blob：建内容寻址暂存 + 装配 resolver 表（toolspool 完整；skill 由 resolve_blob async 处理）。
        // **失败隔离**（对标 Python computer.py「SKILL boot init failed (non-blocking)」+ skill.md §1.5）：
        // 建目录/canonicalize 失败 → 记 ERROR、blob 本会话禁用（mint→NotBooted、resolve_blob toolspool→Gone），
        // **不**阻断 Computer 启动（与同 block `start_skill_watcher` 的 warn!+skip 隔离自洽）。
        let cache_root = self
            .blob_cache_root_override
            .clone()
            .unwrap_or_else(default_blob_cache_root);
        match ToolspoolBlobStore::new(&cache_root) {
            Ok(store) => {
                let store = Arc::new(store);
                self.blob_resolvers.write().await.insert(
                    "toolspool".to_string(),
                    Arc::new(ToolspoolBlobResolver::new(Arc::clone(&store)))
                        as Arc<dyn BlobResolver>,
                );
                *self.toolspool_store.write().await = Some(store);
            }
            Err(e) => {
                error!(error = %e, cache_root = %cache_root.display(),
                    "toolspool blob store init failed (non-blocking); blob disabled this session");
            }
        }

        // SKILL：解析 Home + 物化 mcp 源（_restage_mcp_skills，boot 时多为空——server 后续接入）+ 初次全量
        // 发现 user 源（= invalidate_user_skills）+ 启 watcher。三者各自失败隔离；marketplace 源 reconcile 归
        // #60/#61（非 boot）。对标 Python boot_up SKILL 子系统初始化。
        self.ensure_skill_home();
        self.restage_mcp_skills(None).await;
        self.invalidate_user_skills().await;
        self.start_skill_watcher().await;

        info!("Computer {} started successfully", self.name);
        Ok(())
    }

    /// 渲染服务器配置 / Render server configuration
    /// 解析配置中的 ${input:xxx} 占位符，通过 Session 获取输入值
    /// Parse ${input:xxx} placeholders in config, get input values through Session
    async fn render_server_config(
        &self,
        config: &MCPServerConfig,
    ) -> ComputerResult<MCPServerConfig> {
        // 将配置序列化为 JSON / Serialize config to JSON
        let config_json = serde_json::to_value(config)?;

        // 创建渲染器 / Create renderer
        let renderer = ConfigRender::default();

        // 获取 inputs 的引用以便在闭包中使用 / Get inputs reference for use in closure
        let inputs = self.inputs.read().await;
        let inputs_clone: std::collections::HashMap<String, MCPServerInput> = inputs.clone();
        drop(inputs); // 释放读锁 / Release read lock

        // 首先预解析所有输入值 / Pre-resolve all input values first
        // 这样可以在闭包中使用解析后的值
        // This allows using resolved values in the closure
        let mut resolved_values: std::collections::HashMap<String, serde_json::Value> =
            std::collections::HashMap::new();
        for (input_id, input) in inputs_clone.iter() {
            match self.session.resolve_input(input).await {
                Ok(value) => {
                    resolved_values.insert(input_id.clone(), value);
                }
                Err(e) => {
                    debug!(
                        "Failed to resolve input '{}': {}, will use default",
                        input_id, e
                    );
                    // 使用默认值作为回退 / Use default value as fallback
                    if let Some(default) = input.default() {
                        resolved_values.insert(input_id.clone(), default);
                    }
                }
            }
        }

        // 创建输入解析闭包 / Create input resolver closure
        let resolver = |input_id: String| {
            let values = resolved_values.clone();
            async move {
                if let Some(value) = values.get(&input_id) {
                    Ok(value.clone())
                } else {
                    Err(RenderError::InputNotFound(input_id))
                }
            }
        };

        // 渲染配置 / Render config
        let rendered_json = renderer.render(config_json, resolver).await?;

        // #68：envFile 合并——把渲染后 envFile 的 `KEY=VALUE` 并入 stdio `server_parameters.env`（显式胜）。
        // envFile merge: fold envFile's KEY=VALUE into stdio env (explicit env wins).
        let rendered_json = apply_env_file(rendered_json);

        // 反序列化回配置类型 / Deserialize back to config type
        let rendered_config: MCPServerConfig = serde_json::from_value(rendered_json)?;

        Ok(rendered_config)
    }

    /// 动态添加或更新服务器配置 / Add or update server configuration dynamically
    pub async fn add_or_update_server(&self, server: MCPServerConfig) -> ComputerResult<()> {
        // 确保管理器已初始化 / Ensure manager is initialized
        {
            let mut manager_guard = self.mcp_manager.write().await;
            if manager_guard.is_none() {
                *manager_guard = Some(MCPServerManager::new());
            }
        }

        // 渲染并验证配置 / Render and validate configuration
        let validated = self.render_server_config(&server).await?;

        // 添加到管理器 / Add to manager
        let manager = self.mcp_manager.read().await;
        if let Some(ref manager) = *manager {
            manager.add_or_update_server(validated).await?;
        }

        // 更新本地配置映射 / Update local configuration map
        {
            let mut servers = self.mcp_servers.write().await;
            servers.insert(server.name().to_string(), server);
        }

        // 如果 Socket.IO 已连接，自动发送配置更新通知 / Auto emit update config if Socket.IO connected
        let _ = self.emit_update_config().await;

        Ok(())
    }

    /// 移除服务器配置 / Remove server configuration
    pub async fn remove_server(&self, server_name: &str) -> ComputerResult<()> {
        let manager = self.mcp_manager.read().await;
        if let Some(ref manager) = *manager {
            manager.remove_server(server_name).await?;
        }

        // 从本地配置映射移除 / Remove from local configuration map
        {
            let mut servers = self.mcp_servers.write().await;
            servers.remove(server_name);
        }

        // 如果 Socket.IO 已连接，自动发送配置更新通知 / Auto emit update config if Socket.IO connected
        let _ = self.emit_update_config().await;

        Ok(())
    }

    /// 更新inputs定义 / Update inputs definition
    pub async fn update_inputs(
        &self,
        inputs: HashMap<String, MCPServerInput>,
    ) -> ComputerResult<()> {
        *self.inputs.write().await = inputs;

        // 重新创建输入处理器 / Recreate input handler
        {
            let mut input_handler = self.input_handler.write().await;
            *input_handler = InputHandler::new();
        }

        // 如果 Socket.IO 已连接，自动发送配置更新通知 / Auto emit update config if Socket.IO connected
        let _ = self.emit_update_config().await;

        Ok(())
    }

    /// 添加或更新单个input / Add or update single input
    pub async fn add_or_update_input(&self, input: MCPServerInput) -> ComputerResult<()> {
        let input_id = input.id().to_string();
        {
            let mut inputs = self.inputs.write().await;
            inputs.insert(input_id.clone(), input);
        }

        // 清除相关缓存 / Clear related cache
        self.clear_input_values(Some(&input_id)).await?;

        // 如果 Socket.IO 已连接，自动发送配置更新通知 / Auto emit update config if Socket.IO connected
        let _ = self.emit_update_config().await;

        Ok(())
    }

    /// 移除input / Remove input
    pub async fn remove_input(&self, input_id: &str) -> ComputerResult<bool> {
        let removed = {
            let mut inputs = self.inputs.write().await;
            inputs.remove(input_id).is_some()
        };

        if removed {
            // 清除缓存 / Clear cache
            self.clear_input_values(Some(input_id)).await?;

            // 如果 Socket.IO 已连接，自动发送配置更新通知 / Auto emit update config if Socket.IO connected
            let _ = self.emit_update_config().await;
        }

        Ok(removed)
    }

    /// 获取input定义 / Get input definition
    pub async fn get_input(&self, input_id: &str) -> ComputerResult<Option<MCPServerInput>> {
        let inputs = self.inputs.read().await;
        Ok(inputs.get(input_id).cloned())
    }

    /// 列出所有inputs / List all inputs
    pub async fn list_inputs(&self) -> ComputerResult<Vec<MCPServerInput>> {
        let inputs = self.inputs.read().await;
        Ok(inputs.values().cloned().collect())
    }

    /// 获取输入值 / Get input value
    pub async fn get_input_value(
        &self,
        input_id: &str,
    ) -> ComputerResult<Option<serde_json::Value>> {
        // 从 InputHandler 获取缓存值 / Get cached value from InputHandler
        let handler = self.input_handler.read().await;
        let cached_values = handler.get_all_cached_values().await;

        // 查找匹配的缓存项 / Find matching cached item
        for (key, value) in cached_values {
            // 缓存键格式: input_id[:server:tool[:metadata...]]
            // Cache key format: input_id[:server:tool[:metadata...]]
            if key.starts_with(input_id) {
                // 提取 input_id 部分 / Extract input_id part
                let parts: Vec<&str> = key.split(':').collect();
                if !parts.is_empty() && parts[0] == input_id {
                    return Ok(Some(input_value_to_json(value)));
                }
            }
        }

        Ok(None)
    }

    /// 设置输入值 / Set input value
    pub async fn set_input_value(
        &self,
        input_id: &str,
        value: serde_json::Value,
    ) -> ComputerResult<bool> {
        // 检查input是否存在 / Check if input exists
        {
            let inputs = self.inputs.read().await;
            if !inputs.contains_key(input_id) {
                return Ok(false);
            }
        }

        // 设置缓存值 / Set cached value
        let handler = self.input_handler.read().await;
        let input_value = json_to_input_value(value)?;
        handler
            .set_cached_value(input_id.to_string(), input_value)
            .await;

        Ok(true)
    }

    /// 移除输入值 / Remove input value
    pub async fn remove_input_value(&self, input_id: &str) -> ComputerResult<bool> {
        let handler = self.input_handler.read().await;
        let removed = handler.remove_cached_value(input_id).await.is_some();
        Ok(removed)
    }

    /// 列出所有输入值 / List all input values
    pub async fn list_input_values(&self) -> ComputerResult<HashMap<String, serde_json::Value>> {
        let handler = self.input_handler.read().await;
        let cached_values = handler.get_all_cached_values().await;

        let mut result = HashMap::new();
        for (key, value) in cached_values {
            // 只返回简单的 input_id，不包含上下文信息
            // Only return simple input_id, without context info
            let parts: Vec<&str> = key.split(':').collect();
            if !parts.is_empty() {
                result.insert(parts[0].to_string(), input_value_to_json(value));
            }
        }

        Ok(result)
    }

    /// 清空输入值缓存 / Clear input value cache
    pub async fn clear_input_values(&self, input_id: Option<&str>) -> ComputerResult<()> {
        let handler = self.input_handler.read().await;

        if let Some(id) = input_id {
            // 清除特定输入的所有缓存 / Clear all cache for specific input
            let cached_values = handler.get_all_cached_values().await;
            let keys_to_remove: Vec<String> = cached_values
                .keys()
                .filter(|key| key.starts_with(id))
                .cloned()
                .collect();

            for key in keys_to_remove {
                handler.remove_cached_value(&key).await;
            }
        } else {
            // 清空所有缓存 / Clear all cache
            handler.clear_all_cache().await;
        }

        Ok(())
    }

    /// 获取可用工具列表 / Get available tools list
    pub async fn get_available_tools(&self) -> ComputerResult<Vec<Tool>> {
        let manager = self.mcp_manager.read().await;
        if let Some(ref manager) = *manager {
            let tools: Vec<Tool> = manager.list_available_tools().await;
            // TODO: 转换为SMCPTool格式 / TODO: Convert to SMCPTool format
            // 这里需要实现工具格式转换
            // This needs to implement tool format conversion
            Ok(tools)
        } else {
            Err(ComputerError::InvalidState(
                "Computer not initialized".to_string(),
            ))
        }
    }

    /// 列出所有窗口资源 / List all window resources
    pub async fn list_all_windows(
        &self,
        window_uri: Option<&str>,
    ) -> ComputerResult<Vec<(String, Resource)>> {
        let manager = self.mcp_manager.read().await;
        if let Some(ref manager) = *manager {
            Ok(manager.list_all_windows(window_uri).await)
        } else {
            Err(ComputerError::InvalidState(
                "Computer not initialized".to_string(),
            ))
        }
    }

    /// 获取所有窗口资源的详情 / Get details of all window resources
    pub async fn get_windows_details(
        &self,
        window_uri: Option<&str>,
    ) -> ComputerResult<Vec<(String, Resource, ReadResourceResult)>> {
        let manager = self.mcp_manager.read().await;
        if let Some(ref manager) = *manager {
            Ok(manager.get_windows_details(window_uri).await)
        } else {
            Err(ComputerError::InvalidState(
                "Computer not initialized".to_string(),
            ))
        }
    }

    /// 获取单个窗口资源的详情 / Get detail of a single window resource
    pub async fn get_window_detail(
        &self,
        server_name: &str,
        resource: Resource,
    ) -> ComputerResult<ReadResourceResult> {
        let manager = self.mcp_manager.read().await;
        if let Some(ref manager) = *manager {
            manager.get_window_detail(server_name, resource).await
        } else {
            Err(ComputerError::InvalidState(
                "Computer not initialized".to_string(),
            ))
        }
    }

    /// 执行工具调用 / Execute tool call
    pub async fn execute_tool(
        &self,
        req_id: &str,
        tool_name: &str,
        parameters: serde_json::Value,
        timeout: Option<f64>,
    ) -> ComputerResult<CallToolResult> {
        let manager = self.mcp_manager.read().await;
        if let Some(ref manager) = *manager {
            // 验证工具调用 / Validate tool call
            let (server_name, tool_name) =
                manager.validate_tool_call(tool_name, &parameters).await?;
            let server_name = server_name.to_string();
            let tool_name = tool_name.to_string();

            let timestamp = Utc::now();
            let mut success = false;
            let mut error_msg = None;
            let result: CallToolResult;

            // 检查是否需要确认 / Check if confirmation is needed
            // TODO: 需要实现获取工具元数据的方法
            let need_confirm = true; // 暂时默认需要确认

            // 准备参数，只在实际调用时clone / Prepare parameters, only clone when actually calling
            let parameters_for_call = parameters.clone();

            if need_confirm {
                if let Some(ref callback) = self.confirm_callback {
                    let confirmed = callback(req_id, &server_name, &tool_name, &parameters);
                    if confirmed {
                        let timeout_duration = timeout.map(std::time::Duration::from_secs_f64);
                        result = manager
                            .call_tool(
                                &server_name,
                                &tool_name,
                                parameters_for_call,
                                timeout_duration,
                            )
                            .await?;
                        success = !is_call_tool_error(&result);
                    } else {
                        result = CallToolResult::success(vec![Content::text(
                            "工具调用二次确认被拒绝，请稍后再试",
                        )]);
                    }
                } else {
                    result = CallToolResult::error(vec![Content::text(
                        "当前工具需要调用前进行二次确认，但客户端目前没有实现二次确认回调方法",
                    )]);
                    error_msg = Some("No confirmation callback".to_string());
                }
            } else {
                let timeout_duration = timeout.map(std::time::Duration::from_secs_f64);
                result = manager
                    .call_tool(
                        &server_name,
                        &tool_name,
                        parameters_for_call,
                        timeout_duration,
                    )
                    .await?;
                success = !is_call_tool_error(&result);
            }

            if is_call_tool_error(&result) {
                error_msg = result
                    .content
                    .iter()
                    .find_map(|c| content_as_text(c).map(|t| t.to_string()));
            }

            // 记录历史 / Record history
            let record = ToolCallRecord {
                timestamp,
                req_id: req_id.to_string(),
                server: server_name,
                tool: tool_name,
                parameters,
                timeout,
                success,
                error: error_msg,
            };

            {
                let mut history = self.tool_history.lock().await;
                history.push(record);
                // 保持最近10条记录 / Keep last 10 records
                if history.len() > 10 {
                    history.remove(0);
                }
            }

            Ok(result)
        } else {
            Err(ComputerError::InvalidState(
                "Computer not initialized".to_string(),
            ))
        }
    }

    /// 执行**可取消**工具调用（INT-02 #70 取消最后一公里）/ Execute a cancellable tool call.
    ///
    /// 与 [`Self::execute_tool`] 的差异：登记 `req_id` 的取消令牌，使 [`Self::acancel_tool`]
    /// （← `notify:tool_call_cancel`）能就地中断在途调用，并把原 `client:tool_call` 的 ack 回填为
    /// **取消态** `CallToolResult(isError=true, meta.a2c_cancelled=true, a2c_cancel_reason="agent_requested")`；
    /// 超时回填 `meta.a2c_timeout=true`（协议 0.2.2 结果级标记，SMCP-07 键）。
    ///
    /// 三态判别（对齐 #70 验收）：
    /// - **显式取消**（`acancel_tool` fire 令牌）→ [`CancellableCallOutcome::Cancelled`] → 取消态结果；
    /// - **超时**（manager 级 timeout）→ [`ComputerError::TimeoutError`] → 超时态结果；
    /// - **外层断连/teardown**（本 future 被 drop）→ 不产生任何结果（future 消失，无 ack 可投），
    ///   RAII [`InflightCancelGuard`] 注销注册表，**绝不**被误判为取消态（tokio drop 语义天然满足）。
    ///
    /// 注：跳过了 [`Self::execute_tool`] 的二次确认回调——取消语义聚焦在途中断；二次确认在 socketio 接线
    /// （#72）汇入时按需补接。当前 auto_apply 路径直达可取消调用。
    pub async fn execute_tool_cancellable(
        &self,
        req_id: &str,
        tool_name: &str,
        parameters: serde_json::Value,
        timeout: Option<f64>,
    ) -> ComputerResult<CallToolResult> {
        let manager = self.mcp_manager.read().await;
        let Some(ref manager) = *manager else {
            return Err(ComputerError::InvalidState(
                "Computer not initialized".to_string(),
            ));
        };

        // 校验并解析真实 server/tool（含别名解析）/ validate + resolve real server/tool (alias-aware).
        let (server_name, resolved_tool) =
            manager.validate_tool_call(tool_name, &parameters).await?;
        let server_name = server_name.to_string();
        let resolved_tool = resolved_tool.to_string();

        // 登记取消令牌；RAII 守卫覆盖所有返回路径（含本 future 被 drop 的外层取消）注销注册表。
        let token = CancellationToken::new();
        self.inflight_tool_tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(req_id.to_string(), token.clone());
        let _guard = InflightCancelGuard {
            registry: Arc::clone(&self.inflight_tool_tasks),
            req_id: req_id.to_string(),
        };

        let timestamp = Utc::now();
        let timeout_duration = timeout.map(std::time::Duration::from_secs_f64);

        let outcome = manager
            .call_tool_cancellable(
                &server_name,
                &resolved_tool,
                parameters.clone(),
                timeout_duration,
                token,
            )
            .await;

        let mut success = false;
        let mut error_msg: Option<String> = None;
        let result: CallToolResult = match outcome {
            Ok(CancellableCallOutcome::Completed(r)) => {
                success = !is_call_tool_error(&r);
                if !success {
                    error_msg = r
                        .content
                        .iter()
                        .find_map(|c| content_as_text(c).map(|t| t.to_string()));
                }
                r
            }
            Ok(CancellableCallOutcome::Cancelled) => {
                // 显式取消：写协议 0.2.2 取消态结果级 meta（区别于普通失败/超时）。取消唯一性由
                // CancellableCallOutcome::Cancelled 自身保证（token 每调用独享、仅 acancel_tool 调 .cancel()），
                // 无需额外标记集合。
                error_msg = Some("cancelled".to_string());
                let mut r = CallToolResult::error(vec![Content::text(
                    "工具调用已被取消 / Tool call cancelled",
                )]);
                mark_result_cancelled(&mut r, smcp::tool_meta::A2C_DEFAULT_CANCEL_REASON);
                r
            }
            Err(ComputerError::TimeoutError(_)) => {
                // 超时：写 meta.a2c_timeout=true（SHOULD），区别于取消。
                error_msg = Some("timeout".to_string());
                let mut r = CallToolResult::error(vec![Content::text(
                    "工具调用超时 / Tool call timed out",
                )]);
                mark_result_timeout(&mut r);
                r
            }
            // 其它错误维持上抛（与 execute_tool 一致：非取消/超时的真实失败不伪装成结果）。
            Err(e) => return Err(e),
        };

        // 记录历史（与 execute_tool 同结构）/ Record history.
        let record = ToolCallRecord {
            timestamp,
            req_id: req_id.to_string(),
            server: server_name,
            tool: resolved_tool,
            parameters,
            timeout,
            success,
            error: error_msg,
        };
        {
            let mut history = self.tool_history.lock().await;
            history.push(record);
            // 保持最近10条记录 / Keep last 10 records
            if history.len() > 10 {
                history.remove(0);
            }
        }

        Ok(result)
    }

    /// 取消一个在途工具调用（响应 `notify:tool_call_cancel`，INT-02 #70）/ Cancel an in-flight tool call.
    ///
    /// fire 该 `req_id` 的 [`CancellationToken`]，触发 [`Self::execute_tool_cancellable`] 的取消分支
    /// （就地中断 + rmcp 传输 best-effort 补发 MCP `notifications/cancelled`）。
    ///
    /// 返回 `true`：已对一个在途任务请求取消；`false`：`req_id` 未知或已完成（**幂等 no-op**，对齐
    /// Python `acancel_tool`——完成即由 [`InflightCancelGuard`] 注销注册表，再次取消落空回 `false`）。
    /// MCP 取消为**协作式**——远端是否真正停止**不保证**。
    pub async fn acancel_tool(&self, req_id: &str) -> bool {
        let token = {
            let inflight = self
                .inflight_tool_tasks
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            inflight.get(req_id).cloned()
        };
        match token {
            Some(token) => {
                token.cancel();
                true
            }
            None => false,
        }
    }

    /// 获取工具调用历史 / Get tool call history
    pub async fn get_tool_history(&self) -> ComputerResult<Vec<ToolCallRecord>> {
        let history = self.tool_history.lock().await;
        Ok(history.clone())
    }

    /// 获取服务器状态列表 / Get server status list
    pub async fn get_server_status(&self) -> Vec<(String, bool, String)> {
        let manager_guard = self.mcp_manager.read().await;
        if let Some(ref manager) = *manager_guard {
            manager.get_server_status().await
        } else {
            Vec::new()
        }
    }

    /// 列出 MCP 服务器配置 / List MCP server configurations
    pub async fn list_mcp_servers(&self) -> Vec<MCPServerConfig> {
        let servers = self.mcp_servers.read().await;
        servers.values().cloned().collect()
    }

    /// 启动 MCP 客户端 / Start MCP client
    pub async fn start_mcp_client(&self, server_name: &str) -> ComputerResult<()> {
        let manager_guard = self.mcp_manager.read().await;
        if let Some(ref manager) = *manager_guard {
            if server_name == "all" {
                manager.start_all().await
            } else {
                manager.start_client(server_name).await
            }
        } else {
            Err(ComputerError::InvalidState(
                "MCP Manager not initialized".to_string(),
            ))
        }
    }

    /// 停止 MCP 客户端 / Stop MCP client
    pub async fn stop_mcp_client(&self, server_name: &str) -> ComputerResult<()> {
        let manager_guard = self.mcp_manager.read().await;
        if let Some(ref manager) = *manager_guard {
            if server_name == "all" {
                manager.stop_all().await
            } else {
                manager.stop_client(server_name).await
            }
        } else {
            Err(ComputerError::InvalidState(
                "MCP Manager not initialized".to_string(),
            ))
        }
    }

    /// 检查 MCP Manager 是否已初始化 / Check if MCP Manager is initialized
    pub async fn is_mcp_manager_initialized(&self) -> bool {
        let manager_guard = self.mcp_manager.read().await;
        manager_guard.is_some()
    }

    /// 设置Socket.IO客户端 / Set Socket.IO client
    /// 此方法会替换现有的 client（如果有）并保持强引用
    /// This method replaces existing client (if any) and keeps strong reference
    pub async fn set_socketio_client(&self, client: Arc<SmcpComputerClient>) {
        let mut socketio_ref = self.socketio_client.write().await;
        // 替换旧的 client（如果有），旧的会被自动 drop
        // Replace old client (if any), old one will be dropped automatically
        *socketio_ref = Some(client);
    }

    /// 连接Socket.IO服务器 / Connect to Socket.IO server
    /// socketio-detached 克隆，供 [`SmcpComputerClient`] handler 持有（INT-03 #72）/ socketio-detached clone for handlers.
    ///
    /// 与 [`Clone`] 实现一致地**共享**全部运行态 Arc（manager / tool_history / skill registry /
    /// toolspool / resolvers / inflight 注册表），使 handler 命中与原 Computer **同一**状态——唯独
    /// `socketio_client` 置空（fresh `None`），且去抖器按此 detached 句柄重建，从而**断开** client →
    /// ops → socketio_client → client 的 Arc 环。handler 路径（resolve_blob / get_skill* /
    /// mint / execute_tool_cancellable / acancel_tool）均不经 socketio_client 发包，故置空无副作用。
    ///
    /// `session` 被克隆但 handler 路径**不触碰**它（仅占位满足 struct 完整性）。
    fn clone_for_handlers(&self) -> Self
    where
        S: Clone,
    {
        // detached socketio：handler 持有的 ops 不得反向强引用真正的 client（否则成环）。
        let detached_socketio: Arc<RwLock<Option<Arc<SmcpComputerClient>>>> =
            Arc::new(RwLock::new(None));
        Self {
            name: self.name.clone(),
            mcp_manager: Arc::clone(&self.mcp_manager),
            inputs: Arc::new(RwLock::new(HashMap::new())), // 运行时态不复制（同 Clone 语义）。
            mcp_servers: RwLock::new(HashMap::new()),
            input_handler: Arc::clone(&self.input_handler),
            auto_connect: self.auto_connect,
            auto_reconnect: self.auto_reconnect,
            tool_history: Arc::clone(&self.tool_history),
            session: self.session.clone(),
            socketio_client: detached_socketio.clone(),
            confirm_callback: self.confirm_callback.clone(),
            skill_registry: Arc::clone(&self.skill_registry),
            skill_home: Arc::clone(&self.skill_home),
            skill_home_override: self.skill_home_override.clone(),
            registered_workdirs: Arc::clone(&self.registered_workdirs),
            // 去抖器按 detached socketio 重建：避免 handler 持有的 ops 经去抖器反向引用真 client。
            skill_debouncer: Arc::new(build_skill_debouncer(
                &self.skill_registry,
                &self.skill_home,
                &self.registered_workdirs,
                &detached_socketio,
            )),
            skill_watcher: Arc::new(Mutex::new(None)),
            skill_watch_polling: self.skill_watch_polling,
            blob_cache_root_override: self.blob_cache_root_override.clone(),
            blob_thresholds: self.blob_thresholds,
            toolspool_store: Arc::clone(&self.toolspool_store),
            blob_resolvers: Arc::clone(&self.blob_resolvers),
            inflight_tool_tasks: Arc::clone(&self.inflight_tool_tasks),
        }
    }

    pub async fn connect_socketio(
        &self,
        url: &str,
        namespace: &str,
        auth: &Option<String>,
        headers: &Option<String>,
    ) -> ComputerResult<()>
    where
        S: Clone + 'static,
    {
        // 确保管理器已初始化 / Ensure manager is initialized
        let _manager_check = {
            let manager_guard = self.mcp_manager.read().await;
            match manager_guard.as_ref() {
                Some(_m) => {
                    // Manager 已初始化
                    // Manager is initialized
                    true
                }
                None => {
                    return Err(ComputerError::InvalidState(
                        "MCP Manager not initialized. Please add and start servers first."
                            .to_string(),
                    ));
                }
            }
        };

        // 解析 headers 字符串为 HashMap / Parse headers string into HashMap
        let parsed_headers = headers.as_deref().map(parse_headers_string);

        // INT-03 #72：共享**真实** manager（修复历史 throwaway `MCPServerManager::new()` bug——旧码给
        // socket client 传空 manager，使 on_tool_call / on_get_resources 命中空注册表），并经
        // `computer_ops` 注入 socketio-detached 的 [`ComputerHandlerOps`]，让 blob/skill/cancel handler
        // 能调 resolve_blob / get_skill* / mint / execute_tool_cancellable / acancel_tool。
        // Create Socket.IO client via Builder (threading namespace through).
        let ops: Arc<dyn ComputerHandlerOps> = Arc::new(self.clone_for_handlers());
        let mut builder = SmcpComputerClientBuilder::new(
            url,
            self.mcp_manager.clone(),
            self.name.clone(),
            self.inputs.clone(),
        )
        .namespace(namespace)
        .computer_ops(ops);
        if let Some(secret) = auth.clone() {
            builder = builder.auth_secret(secret);
        }
        if let Some(h) = parsed_headers {
            builder = builder.headers(h);
        }
        let client = builder.connect().await?;

        // 设置客户端到Computer / Set client to Computer
        let client_arc = Arc::new(client);
        self.set_socketio_client(client_arc.clone()).await;

        info!(
            "Connected to SMCP server at {} with computer name: {}",
            url, self.name
        );

        Ok(())
    }

    /// 断开Socket.IO连接 / Disconnect Socket.IO
    pub async fn disconnect_socketio(&self) -> ComputerResult<()> {
        let mut socketio_ref = self.socketio_client.write().await;
        *socketio_ref = None;
        info!("Disconnected from server");
        Ok(())
    }

    /// 加入办公室 / Join office
    pub async fn join_office(&self, office_id: &str, _computer_name: &str) -> ComputerResult<()> {
        let socketio_ref = self.socketio_client.read().await;
        if let Some(ref client) = *socketio_ref {
            // 直接使用 Arc<SmcpComputerClient>，不需要 upgrade
            // Use Arc<SmcpComputerClient> directly, no need to upgrade
            client.join_office(office_id).await?;
            return Ok(());
        }
        Err(ComputerError::InvalidState(
            "Socket.IO client not connected".to_string(),
        ))
    }

    /// 离开办公室 / Leave office
    pub async fn leave_office(&self) -> ComputerResult<()> {
        let socketio_ref = self.socketio_client.read().await;
        if let Some(ref client) = *socketio_ref {
            // 直接使用 Arc<SmcpComputerClient>，不需要 upgrade
            // Use Arc<SmcpComputerClient> directly, no need to upgrade
            let current_office_id = client.get_current_office_id().await?;
            client.leave_office(&current_office_id).await?;
            return Ok(());
        }
        Err(ComputerError::InvalidState(
            "Socket.IO client not connected".to_string(),
        ))
    }

    /// 发送配置更新通知 / Emit config update notification
    pub async fn emit_update_config(&self) -> ComputerResult<()> {
        let socketio_ref = self.socketio_client.read().await;
        if let Some(ref client) = *socketio_ref {
            // 直接使用 Arc<SmcpComputerClient>，不需要 upgrade
            // Use Arc<SmcpComputerClient> directly, no need to upgrade
            client.emit_update_config().await?;
            return Ok(());
        }
        Err(ComputerError::InvalidState(
            "Socket.IO client not connected".to_string(),
        ))
    }

    /// 关闭Computer / Shutdown computer
    pub async fn shutdown(&self) -> ComputerResult<()> {
        info!("Shutting down Computer: {}", self.name);

        // INT-01 #68：停 SKILL watcher + 关去抖器（防停机竞态遗留任务）/ stop watcher + close debouncer。
        {
            let mut guard = self.skill_watcher.lock().await;
            if let Some(mut watcher) = guard.take() {
                watcher.stop();
            }
        }
        self.skill_debouncer.aclose().await;

        let mut manager_guard = self.mcp_manager.write().await;
        if let Some(manager) = manager_guard.take() {
            manager.stop_all().await?;
        }

        // 清除Socket.IO客户端引用 / Clear Socket.IO client reference
        {
            let mut socketio_ref = self.socketio_client.write().await;
            *socketio_ref = None;
        }

        info!("Computer {} shutdown successfully", self.name);
        Ok(())
    }
}

// 实现Clone以供内部使用 / Implement Clone for internal use
impl<S: Session + Clone> Clone for Computer<S> {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            mcp_manager: Arc::clone(&self.mcp_manager),
            inputs: Arc::new(RwLock::new(HashMap::new())), // Note: 不复制运行时状态 / Don't copy runtime state
            mcp_servers: RwLock::new(HashMap::new()),
            input_handler: Arc::clone(&self.input_handler),
            auto_connect: self.auto_connect,
            auto_reconnect: self.auto_reconnect,
            tool_history: Arc::clone(&self.tool_history),
            session: self.session.clone(),
            socketio_client: Arc::clone(&self.socketio_client),
            confirm_callback: self.confirm_callback.clone(),
            // SKILL/blob 子系统：共享同一组 Arc 句柄；去抖器按相同句柄重建（非 Clone）。
            skill_registry: Arc::clone(&self.skill_registry),
            skill_home: Arc::clone(&self.skill_home),
            skill_home_override: self.skill_home_override.clone(),
            registered_workdirs: Arc::clone(&self.registered_workdirs),
            skill_debouncer: Arc::new(build_skill_debouncer(
                &self.skill_registry,
                &self.skill_home,
                &self.registered_workdirs,
                &self.socketio_client,
            )),
            // watcher 属**运行时态**（非共享句柄）：克隆体重启时自建（同 inputs/mcp_servers 重置语义）。
            // 否则共享 watcher 的 on_change 仍驱动**原** debouncer、克隆体 shutdown 会误 stop 共享 watcher。
            skill_watcher: Arc::new(Mutex::new(None)),
            skill_watch_polling: self.skill_watch_polling,
            blob_cache_root_override: self.blob_cache_root_override.clone(),
            blob_thresholds: self.blob_thresholds,
            toolspool_store: Arc::clone(&self.toolspool_store),
            blob_resolvers: Arc::clone(&self.blob_resolvers),
            // 取消注册表属**共享态**：clone 体与原 Computer 须命中同一表，否则跨 clone 的 acancel_tool 失效。
            inflight_tool_tasks: Arc::clone(&self.inflight_tool_tasks),
        }
    }
}

/// Socket.IO handler 所需的 Computer 操作（非泛型 trait 对象，INT-03 #72）/ Computer ops for socketio handlers.
///
/// `socketio_client.rs` 的 `SmcpComputerClient` 是**非泛型** struct（其在 `Computer<S>` 中以
/// `Arc<SmcpComputerClient>` 存储），故不能直接持 `Computer<S>`。本 trait 把 handler 所需的 Computer
/// 操作**类型擦除**为 `Arc<dyn ComputerHandlerOps>`，由 [`Computer::clone_for_handlers`] 的
/// socketio-detached 克隆充当——克隆共享全部运行态 Arc（manager / toolspool / resolvers / skill
/// registry / inflight 注册表），故 handler 与原 Computer 命中同一状态；唯独 `socketio_client` 被置空以
/// **断开 Arc 环**（client → ops → socketio_client → client）。handler 路径均不经 socketio_client 发包。
///
/// 各方法为对 [`Computer`] 同名 inherent 方法的纯委托，无逻辑重复。
#[async_trait]
pub(crate) trait ComputerHandlerOps: Send + Sync {
    /// 解析 blob 句柄（toolspool / skill）→ 可切片描述符 / resolve a blob handle。
    async fn resolve_blob(&self, handle: &str) -> Result<ResolvedBlob, BlobHandleError>;
    /// 活跃 SKILL 列表（排除孤儿）/ active SKILL refs。
    async fn get_skills(&self) -> Vec<A2CSkillRef>;
    /// 按 name 查活跃 SKILL / lookup an active SKILL by name。
    async fn get_skill_ref(&self, name: &str) -> Option<A2CSkillRef>;
    /// 沙箱解析 SKILL 包内资源 → 字节视图 / sandbox-resolve a SKILL resource。
    fn read_skill_resource(
        &self,
        skill_ref: &A2CSkillRef,
        rel_path: Option<&str>,
    ) -> Result<SkillResourceView, SkillSandboxError>;
    /// 铸造 toolspool blob 句柄（写 `.blobspool`）/ mint a toolspool blob handle。
    async fn mint_toolspool_handle(
        &self,
        payload: &[u8],
        mime: &str,
    ) -> Result<String, BlobMintError>;
    /// blob 阈值（inline / too_large / chunk_max）/ blob thresholds。
    fn blob_thresholds(&self) -> BlobThresholds;
    /// 可取消执行工具调用（取消/超时写结果级 meta）/ cancellable tool-call execution。
    async fn execute_tool_cancellable(
        &self,
        req_id: &str,
        tool_name: &str,
        parameters: serde_json::Value,
        timeout: Option<f64>,
    ) -> ComputerResult<CallToolResult>;
    /// fire 在途调用的取消令牌（响应 `notify:tool_call_cancel`）/ fire an in-flight cancel token。
    async fn acancel_tool(&self, req_id: &str) -> bool;
}

#[async_trait]
impl<S: Session + 'static> ComputerHandlerOps for Computer<S> {
    async fn resolve_blob(&self, handle: &str) -> Result<ResolvedBlob, BlobHandleError> {
        Computer::resolve_blob(self, handle).await
    }
    async fn get_skills(&self) -> Vec<A2CSkillRef> {
        Computer::get_skills(self).await
    }
    async fn get_skill_ref(&self, name: &str) -> Option<A2CSkillRef> {
        Computer::get_skill_ref(self, name).await
    }
    fn read_skill_resource(
        &self,
        skill_ref: &A2CSkillRef,
        rel_path: Option<&str>,
    ) -> Result<SkillResourceView, SkillSandboxError> {
        Computer::read_skill_resource(self, skill_ref, rel_path)
    }
    async fn mint_toolspool_handle(
        &self,
        payload: &[u8],
        mime: &str,
    ) -> Result<String, BlobMintError> {
        Computer::mint_toolspool_handle(self, payload, mime).await
    }
    fn blob_thresholds(&self) -> BlobThresholds {
        Computer::blob_thresholds(self)
    }
    async fn execute_tool_cancellable(
        &self,
        req_id: &str,
        tool_name: &str,
        parameters: serde_json::Value,
        timeout: Option<f64>,
    ) -> ComputerResult<CallToolResult> {
        Computer::execute_tool_cancellable(self, req_id, tool_name, parameters, timeout).await
    }
    async fn acancel_tool(&self, req_id: &str) -> bool {
        Computer::acancel_tool(self, req_id).await
    }
}

/// 用于管理器变更通知的trait / Trait for manager change notification
#[async_trait]
pub trait ManagerChangeHandler: Send + Sync {
    /// 处理管理器变更 / Handle manager change
    async fn on_change(&self, message: ManagerChangeMessage) -> ComputerResult<()>;
}

/// 管理器变更消息 / Manager change message
#[derive(Debug, Clone)]
pub enum ManagerChangeMessage {
    /// 工具列表变更 / Tool list changed
    ToolListChanged,
    /// 资源列表变更 / Resource list changed,
    ResourceListChanged { windows: Vec<String> },
    /// 资源更新 / Resource updated
    ResourceUpdated { uri: String },
}

#[async_trait]
impl<S: Session> ManagerChangeHandler for Computer<S> {
    async fn on_change(&self, message: ManagerChangeMessage) -> ComputerResult<()> {
        match message {
            ManagerChangeMessage::ToolListChanged => {
                debug!("Tool list changed, notifying Socket.IO client");
                let socketio_ref = self.socketio_client.read().await;
                if let Some(ref client) = *socketio_ref {
                    // 直接使用 Arc<SmcpComputerClient>，不需要 upgrade
                    // Use Arc<SmcpComputerClient> directly, no need to upgrade
                    client.emit_update_tool_list().await?;
                }
            }
            ManagerChangeMessage::ResourceListChanged { windows: _ } => {
                debug!("Resource list changed, checking for window updates");
                // TODO: 实现窗口变更检测逻辑 / TODO: Implement window change detection logic
            }
            ManagerChangeMessage::ResourceUpdated { uri } => {
                debug!("Resource updated: {}", uri);
                // TODO: 检查是否为window://资源 / TODO: Check if it's a window:// resource
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_clients::model::{
        CommandInput, MCPServerConfig, MCPServerInput, PickStringInput, PromptStringInput,
        StdioServerConfig, StdioServerParameters,
    };

    #[tokio::test]
    async fn test_computer_creation() {
        let session = SilentSession::new("test");
        let computer = Computer::new("test_computer", session, None, None, true, true);

        assert_eq!(computer.name, "test_computer");
        assert!(computer.auto_connect);
        assert!(computer.auto_reconnect);
    }

    // ── INT-01 #68：SKILL / blob 编排集成测试 ────────────────────────────────
    /// 建一个带 user 源 skill 的 Computer（隔离 skill_home + blob 缓存）/ build a Computer with one user skill。
    fn write_user_skill(home: &std::path::Path, name: &str, description: &str) {
        let dir = home.join("user").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\ndescription: {description}\n---\nbody"),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn skill_api_after_boot_user_source() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        write_user_skill(&home, "my-helper", "helps");

        let computer = Computer::new("c", SilentSession::new("s"), None, None, false, false)
            .with_skill_home(home.clone())
            .with_blob_cache_root(tmp.path().join("blob"));
        computer.boot_up().await.unwrap();

        // get_skills / get_skill_ref。
        let skills = computer.get_skills().await;
        assert_eq!(skills.len(), 1);
        let name = skills[0].name.clone();
        let r = computer.get_skill_ref(&name).await.unwrap();
        assert_eq!(r.description, "helps");
        assert!(computer.get_skill_ref("does-not-exist").await.is_none());
        // read_skill_resource：SKILL.md frontmatter 剥离 → "body"。
        let view = computer.read_skill_resource(&r, None).unwrap();
        let bytes = view.slice(0, view.total_size).unwrap();
        assert_eq!(String::from_utf8_lossy(&bytes), "body");
        // skill_home 访问器。
        assert_eq!(computer.skill_home(), home);
        computer.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn mint_and_resolve_toolspool_roundtrip() {
        let tmp = tempfile::TempDir::new().unwrap();
        let computer = Computer::new("c", SilentSession::new("s"), None, None, false, false)
            .with_skill_home(tmp.path().join("home"))
            .with_blob_cache_root(tmp.path().join("blob"));
        computer.boot_up().await.unwrap();

        let payload = b"hello blob payload";
        let handle = computer
            .mint_toolspool_handle(payload, "text/plain")
            .await
            .unwrap();
        let resolved = computer.resolve_blob(&handle).await.unwrap();
        assert_eq!(resolved.total_size, payload.len() as u64);
        let bytes = resolved.slice(0, resolved.total_size).unwrap();
        assert_eq!(bytes, payload);
        computer.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn mint_too_large_rejected_no_write() {
        let tmp = tempfile::TempDir::new().unwrap();
        let computer = Computer::new("c", SilentSession::new("s"), None, None, false, false)
            .with_skill_home(tmp.path().join("home"))
            .with_blob_cache_root(tmp.path().join("blob"))
            .with_blob_thresholds(BlobThresholds {
                inline_budget: 8,
                too_large_cap: 4,
                chunk_max_bytes: 8,
            });
        computer.boot_up().await.unwrap();
        let err = computer
            .mint_toolspool_handle(b"way too large", "text/plain")
            .await
            .unwrap_err();
        assert!(matches!(err, BlobMintError::TooLarge(_)));
        computer.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn invalidate_reconciles_user_orphans() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        write_user_skill(&home, "temp-skill", "tmp");
        let computer = Computer::new("c", SilentSession::new("s"), None, None, false, false)
            .with_skill_home(home.clone())
            .with_blob_cache_root(tmp.path().join("blob"));
        computer.boot_up().await.unwrap();
        assert_eq!(computer.get_skills().await.len(), 1);

        // 删 skill 目录 → invalidate 重扫 → 孤儿排除（从 get_skills 消失）。
        std::fs::remove_dir_all(home.join("user").join("temp-skill")).unwrap();
        computer.invalidate_user_skills().await;
        assert_eq!(computer.get_skills().await.len(), 0);
        computer.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn active_workdir_and_mark_dirty_no_panic() {
        let tmp = tempfile::TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        let computer = Computer::new("c", SilentSession::new("s"), None, None, false, false)
            .with_skill_home(tmp.path().join("home"))
            .with_blob_cache_root(tmp.path().join("blob"))
            .with_registered_workdirs([wd.clone()]);
        assert_eq!(computer.active_workdir(), Some(wd));
        // 去抖器存在 → mark_skills_dirty 不 panic（无 client → 结算 no-op）。
        computer.mark_skills_dirty();
    }

    #[tokio::test]
    async fn test_computer_with_initial_inputs_and_servers() {
        let session = SilentSession::new("test");
        let mut inputs = HashMap::new();
        inputs.insert(
            "input1".to_string(),
            MCPServerInput::PromptString(PromptStringInput {
                id: "input1".to_string(),
                description: "Test input".to_string(),
                default: Some("default".to_string()),
                password: Some(false),
            }),
        );

        let mut servers = HashMap::new();
        servers.insert(
            "server1".to_string(),
            MCPServerConfig::Stdio(StdioServerConfig {
                env_file: None,
                name: "server1".to_string(),
                disabled: false,
                forbidden_tools: vec![],
                tool_meta: std::collections::HashMap::new(),
                default_tool_meta: None,
                vrl: None,
                server_parameters: StdioServerParameters {
                    command: "echo".to_string(),
                    args: vec![],
                    env: std::collections::HashMap::new(),
                    cwd: None,
                },
            }),
        );

        let computer = Computer::new(
            "test_computer",
            session,
            Some(inputs),
            Some(servers),
            false,
            false,
        );

        // 验证初始inputs / Verify initial inputs
        let inputs = computer.list_inputs().await.unwrap();
        assert_eq!(inputs.len(), 1);
        match &inputs[0] {
            MCPServerInput::PromptString(input) => {
                assert_eq!(input.id, "input1");
                assert_eq!(input.description, "Test input");
            }
            _ => panic!("Expected PromptString input"),
        }
    }

    #[tokio::test]
    async fn test_input_management() {
        let session = SilentSession::new("test");
        let computer = Computer::new("test_computer", session, None, None, true, true);

        // 测试添加input / Test adding input
        let input = MCPServerInput::PromptString(PromptStringInput {
            id: "test_input".to_string(),
            description: "Test input".to_string(),
            default: Some("default".to_string()),
            password: Some(false),
        });

        computer.add_or_update_input(input.clone()).await.unwrap();

        // 验证input已添加 / Verify input is added
        let retrieved = computer.get_input("test_input").await.unwrap();
        assert!(retrieved.is_some());

        // 测试列出所有inputs / Test listing all inputs
        let inputs = computer.list_inputs().await.unwrap();
        assert_eq!(inputs.len(), 1);

        // 测试更新input / Test updating input
        let updated_input = MCPServerInput::PromptString(PromptStringInput {
            id: "test_input".to_string(),
            description: "Updated description".to_string(),
            default: Some("new_default".to_string()),
            password: Some(true),
        });
        computer.add_or_update_input(updated_input).await.unwrap();

        let retrieved = computer.get_input("test_input").await.unwrap().unwrap();
        match retrieved {
            MCPServerInput::PromptString(input) => {
                assert_eq!(input.description, "Updated description");
                assert_eq!(input.default, Some("new_default".to_string()));
                assert_eq!(input.password, Some(true));
            }
            _ => panic!("Expected PromptString input"),
        }

        // 测试移除input / Test removing input
        let removed = computer.remove_input("test_input").await.unwrap();
        assert!(removed);

        let retrieved = computer.get_input("test_input").await.unwrap();
        assert!(retrieved.is_none());

        // 测试移除不存在的input / Test removing non-existent input
        let removed = computer.remove_input("non_existent").await.unwrap();
        assert!(!removed);
    }

    #[tokio::test]
    async fn test_multiple_input_types() {
        let session = SilentSession::new("test");
        let computer = Computer::new("test_computer", session, None, None, true, true);

        // 添加不同类型的inputs / Add different types of inputs
        let prompt_input = MCPServerInput::PromptString(PromptStringInput {
            id: "prompt".to_string(),
            description: "Prompt input".to_string(),
            default: None,
            password: Some(false),
        });

        let pick_input = MCPServerInput::PickString(PickStringInput {
            id: "pick".to_string(),
            description: "Pick input".to_string(),
            options: vec!["option1".to_string(), "option2".to_string()],
            default: Some("option1".to_string()),
        });

        let command_input = MCPServerInput::Command(CommandInput {
            id: "command".to_string(),
            description: "Command input".to_string(),
            command: "ls".to_string(),
            args: None,
        });

        computer.add_or_update_input(prompt_input).await.unwrap();
        computer.add_or_update_input(pick_input).await.unwrap();
        computer.add_or_update_input(command_input).await.unwrap();

        let inputs = computer.list_inputs().await.unwrap();
        assert_eq!(inputs.len(), 3);

        // 验证每个input类型 / Verify each input type
        let input_types: std::collections::HashSet<_> = inputs
            .iter()
            .map(|input| match input {
                MCPServerInput::PromptString(_) => "prompt",
                MCPServerInput::PickString(_) => "pick",
                MCPServerInput::Command(_) => "command",
            })
            .collect();

        assert!(input_types.contains("prompt"));
        assert!(input_types.contains("pick"));
        assert!(input_types.contains("command"));
    }

    #[tokio::test]
    async fn test_server_management() {
        let session = SilentSession::new("test");
        let computer = Computer::new("test_computer", session, None, None, true, true);

        // 添加服务器配置 / Add server configuration
        let server_config = MCPServerConfig::Stdio(StdioServerConfig {
            env_file: None,
            name: "test_server".to_string(),
            disabled: false,
            forbidden_tools: vec![],
            tool_meta: std::collections::HashMap::new(),
            default_tool_meta: None,
            vrl: None,
            server_parameters: StdioServerParameters {
                command: "echo".to_string(),
                args: vec!["hello".to_string()],
                env: std::collections::HashMap::new(),
                cwd: None,
            },
        });

        computer
            .add_or_update_server(server_config.clone())
            .await
            .unwrap();

        // 注意：由于MCPServerManager是私有的，我们通过添加重复的服务器来测试更新
        // Note: Since MCPServerManager is private, we test updates by adding duplicate servers
        let updated_config = MCPServerConfig::Stdio(StdioServerConfig {
            env_file: None,
            name: "test_server".to_string(),
            disabled: true, // 更新为禁用状态 / Update to disabled state
            forbidden_tools: vec!["tool1".to_string()],
            tool_meta: std::collections::HashMap::new(),
            default_tool_meta: None,
            vrl: None,
            server_parameters: StdioServerParameters {
                command: "echo".to_string(),
                args: vec!["updated".to_string()],
                env: std::collections::HashMap::new(),
                cwd: None,
            },
        });

        computer.add_or_update_server(updated_config).await.unwrap();

        // 移除服务器 / Remove server
        computer.remove_server("test_server").await.unwrap();
    }

    #[tokio::test]
    async fn test_session_trait() {
        // 测试SilentSession的行为 / Test SilentSession behavior
        let session = SilentSession::new("test_session");
        assert_eq!(session.session_id(), "test_session");

        // 测试PromptString输入解析 / Test PromptString input resolution
        let prompt_input = MCPServerInput::PromptString(PromptStringInput {
            id: "test".to_string(),
            description: "Test".to_string(),
            default: Some("default_value".to_string()),
            password: Some(false),
        });

        let result = session.resolve_input(&prompt_input).await.unwrap();
        assert_eq!(
            result,
            serde_json::Value::String("default_value".to_string())
        );

        // 测试无默认值的PromptString / Test PromptString without default
        let no_default_input = MCPServerInput::PromptString(PromptStringInput {
            id: "test2".to_string(),
            description: "Test2".to_string(),
            default: None,
            password: Some(false),
        });

        let result = session.resolve_input(&no_default_input).await.unwrap();
        assert_eq!(result, serde_json::Value::String("".to_string()));

        // 测试PickString输入解析 / Test PickString input resolution
        let pick_input = MCPServerInput::PickString(PickStringInput {
            id: "pick".to_string(),
            description: "Pick".to_string(),
            options: vec!["opt1".to_string(), "opt2".to_string()],
            default: Some("opt2".to_string()),
        });

        let result = session.resolve_input(&pick_input).await.unwrap();
        assert_eq!(result, serde_json::Value::String("opt2".to_string()));

        // 测试Command输入解析 / Test Command input resolution
        let command_input = MCPServerInput::Command(CommandInput {
            id: "cmd".to_string(),
            description: "Command".to_string(),
            command: "echo hello world".to_string(),
            args: None,
        });

        let result = session.resolve_input(&command_input).await.unwrap();
        assert_eq!(result, serde_json::Value::String("hello world".to_string()));
    }

    #[tokio::test]
    async fn test_cache_operations() {
        let session = SilentSession::new("test");
        let computer = Computer::new("test_computer", session, None, None, true, true);

        // 添加一个 input / Add an input
        let input = MCPServerInput::PromptString(PromptStringInput {
            id: "test_input".to_string(),
            description: "Test input".to_string(),
            default: Some("default".to_string()),
            password: Some(false),
        });
        computer.add_or_update_input(input).await.unwrap();

        // 测试设置和获取缓存值 / Test setting and getting cache value
        let test_value = serde_json::Value::String("cached_value".to_string());
        let set_result = computer
            .set_input_value("test_input", test_value.clone())
            .await
            .unwrap();
        assert!(set_result);

        let retrieved = computer.get_input_value("test_input").await.unwrap();
        assert_eq!(retrieved, Some(test_value));

        // 测试设置不存在的 input / Test setting non-existent input
        let invalid_result = computer
            .set_input_value(
                "nonexistent",
                serde_json::Value::String("value".to_string()),
            )
            .await
            .unwrap();
        assert!(!invalid_result);

        // 测试获取不存在的缓存 / Test getting non-existent cache
        let not_found = computer.get_input_value("nonexistent").await.unwrap();
        assert!(not_found.is_none());
    }

    #[tokio::test]
    async fn test_cache_remove_and_clear() {
        let session = SilentSession::new("test");
        let computer = Computer::new("test_computer", session, None, None, true, true);

        // 添加 inputs / Add inputs
        let input1 = MCPServerInput::PromptString(PromptStringInput {
            id: "input1".to_string(),
            description: "Input 1".to_string(),
            default: None,
            password: Some(false),
        });
        let input2 = MCPServerInput::PromptString(PromptStringInput {
            id: "input2".to_string(),
            description: "Input 2".to_string(),
            default: None,
            password: Some(false),
        });
        computer.add_or_update_input(input1).await.unwrap();
        computer.add_or_update_input(input2).await.unwrap();

        // 设置缓存值 / Set cache values
        computer
            .set_input_value("input1", serde_json::Value::String("value1".to_string()))
            .await
            .unwrap();
        computer
            .set_input_value("input2", serde_json::Value::String("value2".to_string()))
            .await
            .unwrap();

        // 测试删除特定缓存 / Test removing specific cache
        let removed = computer.remove_input_value("input1").await.unwrap();
        assert!(removed);

        let retrieved = computer.get_input_value("input1").await.unwrap();
        assert!(retrieved.is_none());

        let still_exists = computer.get_input_value("input2").await.unwrap();
        assert!(still_exists.is_some());

        // 测试清空所有缓存 / Test clearing all cache
        computer.clear_input_values(None).await.unwrap();
        let cleared1 = computer.get_input_value("input1").await.unwrap();
        let cleared2 = computer.get_input_value("input2").await.unwrap();
        assert!(cleared1.is_none());
        assert!(cleared2.is_none());
    }

    #[tokio::test]
    async fn test_cache_list_values() {
        let session = SilentSession::new("test");
        let computer = Computer::new("test_computer", session, None, None, true, true);

        // 添加 inputs / Add inputs
        let input1 = MCPServerInput::PromptString(PromptStringInput {
            id: "input1".to_string(),
            description: "Input 1".to_string(),
            default: None,
            password: Some(false),
        });
        let input2 = MCPServerInput::PromptString(PromptStringInput {
            id: "input2".to_string(),
            description: "Input 2".to_string(),
            default: None,
            password: Some(false),
        });
        computer.add_or_update_input(input1).await.unwrap();
        computer.add_or_update_input(input2).await.unwrap();

        // 设置不同类型的值 / Set different types of values
        computer
            .set_input_value(
                "input1",
                serde_json::Value::String("string_value".to_string()),
            )
            .await
            .unwrap();
        computer
            .set_input_value(
                "input2",
                serde_json::Value::Number(serde_json::Number::from(42)),
            )
            .await
            .unwrap();

        // 列出所有值 / List all values
        let values = computer.list_input_values().await.unwrap();
        assert_eq!(values.len(), 2);
        assert_eq!(
            values.get("input1"),
            Some(&serde_json::Value::String("string_value".to_string()))
        );
        assert_eq!(
            values.get("input2"),
            Some(&serde_json::Value::Number(serde_json::Number::from(42)))
        );
    }

    #[tokio::test]
    async fn test_cache_clear_on_input_update() {
        let session = SilentSession::new("test");
        let computer = Computer::new("test_computer", session, None, None, true, true);

        // 添加 input / Add input
        let input = MCPServerInput::PromptString(PromptStringInput {
            id: "test_input".to_string(),
            description: "Test input".to_string(),
            default: None,
            password: Some(false),
        });
        computer.add_or_update_input(input).await.unwrap();

        // 设置缓存 / Set cache
        computer
            .set_input_value(
                "test_input",
                serde_json::Value::String("cached".to_string()),
            )
            .await
            .unwrap();
        assert!(computer
            .get_input_value("test_input")
            .await
            .unwrap()
            .is_some());

        // 更新 input（应该清除缓存）/ Update input (should clear cache)
        let updated_input = MCPServerInput::PromptString(PromptStringInput {
            id: "test_input".to_string(),
            description: "Updated input".to_string(),
            default: Some("new_default".to_string()),
            password: Some(true),
        });
        computer.add_or_update_input(updated_input).await.unwrap();

        // 缓存应该被清除 / Cache should be cleared
        assert!(computer
            .get_input_value("test_input")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn test_cache_clear_on_input_remove() {
        let session = SilentSession::new("test");
        let computer = Computer::new("test_computer", session, None, None, true, true);

        // 添加 input / Add input
        let input = MCPServerInput::PromptString(PromptStringInput {
            id: "test_input".to_string(),
            description: "Test input".to_string(),
            default: None,
            password: Some(false),
        });
        computer.add_or_update_input(input).await.unwrap();

        // 设置缓存 / Set cache
        computer
            .set_input_value(
                "test_input",
                serde_json::Value::String("cached".to_string()),
            )
            .await
            .unwrap();
        assert!(computer
            .get_input_value("test_input")
            .await
            .unwrap()
            .is_some());

        // 移除 input（应该清除缓存）/ Remove input (should clear cache)
        let removed = computer.remove_input("test_input").await.unwrap();
        assert!(removed);

        // 缓存应该被清除 / Cache should be cleared
        assert!(computer
            .get_input_value("test_input")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn test_tool_call_history() {
        let session = SilentSession::new("test");
        let computer = Computer::new("test_computer", session, None, None, true, true);

        // 初始历史应该为空 / Initial history should be empty
        let history = computer.get_tool_history().await.unwrap();
        assert!(history.is_empty());

        // 注意：实际的工具调用需要MCP服务器，这里只测试历史记录的结构
        // Note: Actual tool calls need MCP server, here we only test history structure
    }

    #[tokio::test]
    async fn test_confirmation_callback() {
        let session = SilentSession::new("test");
        let computer = Computer::new("test_computer", session, None, None, true, true);

        // 设置确认回调 / Set confirmation callback
        let callback_called = Arc::new(Mutex::new(false));
        let callback_called_clone = callback_called.clone();

        let _computer = computer.with_confirm_callback(move |_req_id, _server, _tool, _params| {
            // 使用tokio::block_on在同步回调中执行异步操作
            // Use tokio::block_in_async to execute async operations in sync callback
            let rt = tokio::runtime::Handle::current();
            rt.block_on(async {
                let mut called = callback_called_clone.lock().await;
                *called = true;
            });
            true // 确认 / Confirm
        });

        // 回调已设置，但实际测试需要MCP服务器
        // Callback is set, but actual testing needs MCP server
    }

    #[tokio::test]
    async fn test_computer_shutdown() {
        // INT-01 #68：boot_up 起 FS 副作用 → 隔离到 TempDir（不污染真实 ~/.a2c / skill home）。
        let td = tempfile::TempDir::new().unwrap();
        let session = SilentSession::new("test");
        let computer = Computer::new("test_computer", session, None, None, true, true)
            .with_skill_home(td.path().join("skills"))
            .with_blob_cache_root(td.path().join("blob"));

        // 测试关闭未初始化的Computer / Test shutting down uninitialized computer
        computer.shutdown().await.unwrap();

        // 测试关闭已初始化的Computer / Test shutting down initialized computer
        computer.boot_up().await.unwrap();
        computer.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn test_config_render() {
        let session = SilentSession::new("test");

        // 创建带有输入定义的 inputs / Create inputs with input definitions
        let mut inputs = HashMap::new();
        inputs.insert(
            "api_key".to_string(),
            MCPServerInput::PromptString(PromptStringInput {
                id: "api_key".to_string(),
                description: "API Key".to_string(),
                default: Some("test-api-key-12345".to_string()),
                password: Some(true),
            }),
        );
        inputs.insert(
            "server_url".to_string(),
            MCPServerInput::PromptString(PromptStringInput {
                id: "server_url".to_string(),
                description: "Server URL".to_string(),
                default: Some("https://api.example.com".to_string()),
                password: Some(false),
            }),
        );

        let computer = Computer::new("test_computer", session, Some(inputs), None, true, true);

        // 创建带有占位符的服务器配置 / Create server config with placeholders
        let server_config = MCPServerConfig::Stdio(StdioServerConfig {
            env_file: None,
            name: "test_server".to_string(),
            disabled: false,
            forbidden_tools: vec![],
            tool_meta: std::collections::HashMap::new(),
            default_tool_meta: None,
            vrl: None,
            server_parameters: StdioServerParameters {
                command: "echo".to_string(),
                args: vec!["${input:api_key}".to_string()],
                env: {
                    let mut env = std::collections::HashMap::new();
                    env.insert("API_URL".to_string(), "${input:server_url}".to_string());
                    env
                },
                cwd: None,
            },
        });

        // 渲染配置 / Render config
        let rendered = computer.render_server_config(&server_config).await.unwrap();

        // 验证占位符已被替换 / Verify placeholders are replaced
        match rendered {
            MCPServerConfig::Stdio(config) => {
                assert_eq!(config.server_parameters.args[0], "test-api-key-12345");
                assert_eq!(
                    config.server_parameters.env.get("API_URL"),
                    Some(&"https://api.example.com".to_string())
                );
            }
            _ => panic!("Expected Stdio config"),
        }
    }

    #[tokio::test]
    async fn test_config_render_missing_input() {
        let session = SilentSession::new("test");
        let computer = Computer::new("test_computer", session, None, None, true, true);

        // 创建带有不存在输入的配置 / Create config with non-existent input
        let server_config = MCPServerConfig::Stdio(StdioServerConfig {
            env_file: None,
            name: "test_server".to_string(),
            disabled: false,
            forbidden_tools: vec![],
            tool_meta: std::collections::HashMap::new(),
            default_tool_meta: None,
            vrl: None,
            server_parameters: StdioServerParameters {
                command: "echo".to_string(),
                args: vec!["${input:missing_input}".to_string()],
                env: std::collections::HashMap::new(),
                cwd: None,
            },
        });

        // 渲染配置应该保留原占位符 / Render should preserve original placeholder
        let rendered = computer.render_server_config(&server_config).await.unwrap();

        match rendered {
            MCPServerConfig::Stdio(config) => {
                // 未找到的输入应该保留原占位符 / Missing input should preserve placeholder
                assert_eq!(config.server_parameters.args[0], "${input:missing_input}");
            }
            _ => panic!("Expected Stdio config"),
        }
    }

    #[test]
    fn test_parse_headers_string_normal() {
        let result = parse_headers_string("x-tenant-id:abc123,x-custom:value");
        assert_eq!(result.len(), 2);
        assert_eq!(result["x-tenant-id"], "abc123");
        assert_eq!(result["x-custom"], "value");
    }

    #[test]
    fn test_parse_headers_string_with_spaces() {
        let result = parse_headers_string(" x-tenant-id : abc123 , x-custom : value ");
        assert_eq!(result.len(), 2);
        assert_eq!(result["x-tenant-id"], "abc123");
        assert_eq!(result["x-custom"], "value");
    }

    #[test]
    fn test_parse_headers_string_empty() {
        let result = parse_headers_string("");
        assert!(result.is_empty());
    }

    #[test]
    fn test_parse_headers_string_missing_value() {
        let result = parse_headers_string("key-only,x-valid:ok");
        assert_eq!(result.len(), 1);
        assert_eq!(result["x-valid"], "ok");
    }

    #[test]
    fn test_parse_headers_string_value_with_colon() {
        let result = parse_headers_string("Authorization:Bearer:token123");
        assert_eq!(result.len(), 1);
        assert_eq!(result["Authorization"], "Bearer:token123");
    }

    // ── #68 收尾：envFile 合并 / get_resources / restage 编排 ────────────────────

    #[test]
    fn test_apply_env_file_stdio_merge_explicit_wins() {
        let tmp = tempfile::TempDir::new().unwrap();
        let env_path = tmp.path().join(".env");
        std::fs::write(&env_path, "FROM_FILE=file_val\nSHARED=file_shared\n").unwrap();

        let rendered = serde_json::json!({
            "type": "stdio",
            "name": "srv",
            "envFile": env_path.to_str().unwrap(),
            "server_parameters": {
                "command": "echo",
                "args": [],
                "env": { "SHARED": "explicit_shared", "EXPLICIT": "explicit_val" }
            }
        });
        let out = apply_env_file(rendered);
        let env = &out["server_parameters"]["env"];
        assert_eq!(env["FROM_FILE"], "file_val"); // envFile-only 键并入
        assert_eq!(env["EXPLICIT"], "explicit_val"); // 显式-only 键保留
        assert_eq!(env["SHARED"], "explicit_shared"); // 同名：显式胜
    }

    #[test]
    fn test_apply_env_file_non_stdio_ignored() {
        let rendered = serde_json::json!({
            "type": "sse",
            "name": "srv",
            "envFile": "/nonexistent/.env",
            "server_parameters": { "url": "http://x", "headers": {} }
        });
        // sse 无 env/command → envFile 忽略，原样返回（未注入 env）。
        let out = apply_env_file(rendered.clone());
        assert_eq!(out, rendered);
        assert!(out["server_parameters"].get("env").is_none());
    }

    #[test]
    fn test_apply_env_file_missing_and_empty_file() {
        // 无 envFile → 原样。
        let no_ef = serde_json::json!({
            "type": "stdio", "name": "s",
            "server_parameters": { "command": "echo", "env": { "A": "1" } }
        });
        assert_eq!(apply_env_file(no_ef.clone()), no_ef);

        // envFile 指向「仅注释」文件 → file_env 空 → env 不变。
        let tmp = tempfile::TempDir::new().unwrap();
        let empty = tmp.path().join("empty.env");
        std::fs::write(&empty, "# only a comment\n\n").unwrap();
        let with_empty = serde_json::json!({
            "type": "stdio", "name": "s",
            "envFile": empty.to_str().unwrap(),
            "server_parameters": { "command": "echo", "env": { "A": "1" } }
        });
        let out = apply_env_file(with_empty);
        assert_eq!(out["server_parameters"]["env"]["A"], "1");
    }

    #[tokio::test]
    async fn test_get_resources_no_manager_invalid_state() {
        // 未 boot（mcp_manager 为 None）→ InvalidState。
        let computer = Computer::new("c", SilentSession::new("s"), None, None, false, false);
        let err = computer.get_resources("srv", None).await.unwrap_err();
        assert!(matches!(err, ComputerError::InvalidState(_)));
    }

    #[tokio::test]
    async fn test_restage_mcp_skills_no_home_empty() {
        // 未 boot（skill_home None / 无 manager）→ 空列表，不 panic。
        let computer = Computer::new("c", SilentSession::new("s"), None, None, false, false);
        assert!(computer.restage_mcp_skills(None).await.is_empty());
    }

    #[tokio::test]
    async fn test_get_resources_delegates_to_manager() {
        use crate::mcp_clients::manager::test_support::{inject, MockSkillClient};
        use crate::mcp_clients::model::make_resource;

        let computer = Computer::new("c", SilentSession::new("s"), None, None, false, false);
        let mgr = MCPServerManager::new();
        inject(
            &mgr,
            "srv",
            MockSkillClient {
                pages: vec![vec![make_resource("res://a", "a", None, None)]],
                fail: false,
                cap_fail: false,
                read_text: String::new(),
            },
        )
        .await;
        *computer.mcp_manager.write().await = Some(mgr);

        // 成功委托：单页透传 manager.list_resources，cursor 出（末页 None）。
        let (page, next) = computer.get_resources("srv", None).await.unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].uri, "res://a");
        assert!(next.is_none());

        // 未注册 server → McpServerNotFound（4014）。
        let err = computer.get_resources("missing", None).await.unwrap_err();
        assert_eq!(err.error_code(), 4014);
    }

    #[tokio::test]
    async fn test_restage_mcp_skills_happy_registers_mounted() {
        use crate::mcp_clients::manager::test_support::{
            inject, skill_resource_mounted, MockSkillClient,
        };

        let tmp = tempfile::TempDir::new().unwrap();
        // 真实挂载源：含 SKILL.md frontmatter.name=real-name（包根名应被校正）。
        let mount = tmp.path().join("mount");
        std::fs::create_dir_all(&mount).unwrap();
        std::fs::write(
            mount.join("SKILL.md"),
            "---\nname: real-name\ndescription: mounted skill\n---\nbody",
        )
        .unwrap();

        let computer = Computer::new("c", SilentSession::new("s"), None, None, false, false)
            .with_skill_home(tmp.path().join("home"))
            .with_blob_cache_root(tmp.path().join("blob"));
        // boot 解析 skill_home（此时 manager 仍 None → boot 内 restage 为空，符合预期）。
        computer.boot_up().await.unwrap();

        // boot 后注入带 mounted skill:// 的 manager，再全量重物化。
        let mgr = MCPServerManager::new();
        inject(
            &mgr,
            "tfrobot-tools",
            MockSkillClient {
                pages: vec![vec![skill_resource_mounted(
                    "skill://tfrobot-tools/raw-leaf",
                    Some("mounted"),
                    mount.to_str().unwrap(),
                )]],
                fail: false,
                cap_fail: false,
                read_text: String::new(),
            },
        )
        .await;
        *computer.mcp_manager.write().await = Some(mgr);

        // happy path：物化 + 注册 mcp 源（name = mcp:<server>:<frontmatter.name>）。
        let registered = computer.restage_mcp_skills(None).await;
        assert_eq!(registered, vec!["mcp:tfrobot-tools:real-name".to_string()]);

        // get_skills 反映新注册的 mcp 源 skill。
        let skills = computer.get_skills().await;
        assert!(skills
            .iter()
            .any(|s| s.name == "mcp:tfrobot-tools:real-name"));

        // 单 server 重物化（server_name=Some）：不做孤儿对账，仍返回该名。
        let again = computer.restage_mcp_skills(Some("tfrobot-tools")).await;
        assert_eq!(again, vec!["mcp:tfrobot-tools:real-name".to_string()]);
    }

    // ── INT-02 #70：取消最后一公里（acancel_tool 幂等 / 守卫退场 / 结果级 meta 标记）──────────

    #[tokio::test]
    async fn test_acancel_tool_unknown_req_id_is_idempotent_false() {
        // 未知 / 无在途的 req_id → 幂等 no-op 回 false（对齐 Python acancel_tool；已完成同样落空）。
        let session = SilentSession::new("test");
        let computer = Computer::new("test_computer", session, None, None, true, true);
        assert!(!computer.acancel_tool("never-registered").await);
    }

    #[test]
    fn test_inflight_cancel_guard_deregisters_on_drop() {
        // 「外层断连/teardown」语义：execute_tool_cancellable 的 future 被 drop 时守卫注销注册表，
        // 不残留在途条目（故后续 acancel_tool 落空回 false，绝不被误判为取消态结果）。
        let registry: Arc<StdMutex<HashMap<String, CancellationToken>>> =
            Arc::new(StdMutex::new(HashMap::new()));
        registry
            .lock()
            .unwrap()
            .insert("rid".to_string(), CancellationToken::new());
        {
            let _guard = InflightCancelGuard {
                registry: Arc::clone(&registry),
                req_id: "rid".to_string(),
            };
            assert!(registry.lock().unwrap().contains_key("rid"));
        } // 守卫在此 drop（含外层 future 被取消 drop 的场景）
        assert!(
            !registry.lock().unwrap().contains_key("rid"),
            "守卫 drop 后注册表条目应被注销"
        );
    }

    #[test]
    fn test_mark_result_cancelled_writes_protocol_meta() {
        // 取消态结果级 meta：a2c_cancelled=true + a2c_cancel_reason（协议 0.2.2 MUST/SHOULD）。
        // 注意：rmcp CallToolResult.meta 在线 key 为 `_meta`（#[serde(rename="_meta")]，MCP 约定）——
        // 协议允许（producer 写结果级 meta，consumer 宽松读 meta/_meta，data-structures.md §234）。
        let mut r = CallToolResult::error(vec![Content::text("x")]);
        mark_result_cancelled(&mut r, smcp::tool_meta::A2C_DEFAULT_CANCEL_REASON);
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["_meta"]["a2c_cancelled"], true);
        assert_eq!(v["_meta"]["a2c_cancel_reason"], "agent_requested");
        assert!(
            v["_meta"].get("a2c_timeout").is_none(),
            "取消 ≠ 超时：不应同时写 a2c_timeout"
        );
    }

    #[test]
    fn test_mark_result_timeout_writes_protocol_meta() {
        // 超时态结果级 meta：a2c_timeout=true（SHOULD），且不写取消标记。
        let mut r = CallToolResult::error(vec![Content::text("x")]);
        mark_result_timeout(&mut r);
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["_meta"]["a2c_timeout"], true);
        assert!(
            v["_meta"].get("a2c_cancelled").is_none(),
            "超时 ≠ 取消：不应同时写 a2c_cancelled"
        );
    }

    // ── INT-02 #70：execute_tool_cancellable / acancel_tool 端到端（注入 mock manager）──────
    // 用 manager test_support 的可取消假 client + tool_mapping 把这条编排路径拉成单测（不必等 #72 e2e）。

    /// 建一个注入了可取消假 client（server="srv", tool="t"）的 Computer / build a Computer with a
    /// cancellable fake MCP client wired in。
    async fn computer_with_cancel_mock(
        behavior: crate::mcp_clients::manager::test_support::CancelBehavior,
    ) -> Computer<SilentSession> {
        let computer = Computer::new(
            "test_computer",
            SilentSession::new("t"),
            None,
            None,
            true,
            true,
        );
        let manager = MCPServerManager::new();
        crate::mcp_clients::manager::test_support::inject_callable(&manager, "srv", "t", behavior)
            .await;
        *computer.mcp_manager.write().await = Some(manager);
        computer
    }

    #[tokio::test]
    async fn test_execute_tool_cancellable_timeout_marks_meta_and_history() {
        use crate::mcp_clients::manager::test_support::CancelBehavior;
        use std::time::Duration;
        // 假 client 睡 10s + 50ms timeout → manager 级超时 → 回填超时态结果（_meta.a2c_timeout）+ 历史。
        let computer =
            computer_with_cancel_mock(CancelBehavior::Sleep(Duration::from_secs(10))).await;

        let result = computer
            .execute_tool_cancellable("rid-timeout", "t", serde_json::json!({}), Some(0.05))
            .await
            .expect("超时应回填结果而非上抛");

        let v = serde_json::to_value(&result).unwrap();
        assert_eq!(
            v["_meta"]["a2c_timeout"], true,
            "超时态须写 _meta.a2c_timeout"
        );
        assert_eq!(result.is_error, Some(true));

        let history = computer.get_tool_history().await.unwrap();
        let rec = history
            .iter()
            .find(|r| r.req_id == "rid-timeout")
            .expect("历史应落一条记录");
        assert!(!rec.success);
        assert_eq!(rec.error.as_deref(), Some("timeout"));
    }

    #[tokio::test]
    async fn test_execute_tool_cancellable_cancel_marks_meta_and_acancel_true() {
        use crate::mcp_clients::manager::test_support::CancelBehavior;
        use std::time::Duration;
        // 假 client 永不返回（在途阻塞）；另一上下文 acancel → 就地中断 → 回填取消态结果。
        // Arc<Computer> 两持有共享 inflight 注册表（与 socketio 接线 #72 同款：取消由独立上下文触发）。
        let computer = Arc::new(computer_with_cancel_mock(CancelBehavior::BlockForever).await);

        let exec = computer.clone();
        let handle = tokio::spawn(async move {
            exec.execute_tool_cancellable("rid-cancel", "t", serde_json::json!({}), None)
                .await
        });

        // bounded-poll：等 execute 注册令牌后 acancel 命中在途调用回 true（确定性，避免裸 sleep 竞速）。
        let mut cancelled = false;
        for _ in 0..200 {
            if computer.acancel_tool("rid-cancel").await {
                cancelled = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(cancelled, "acancel_tool 应对在途调用返回 true");

        let result = handle.await.expect("join").expect("取消应回填结果而非上抛");

        let v = serde_json::to_value(&result).unwrap();
        assert_eq!(
            v["_meta"]["a2c_cancelled"], true,
            "取消态须写 _meta.a2c_cancelled"
        );
        assert_eq!(v["_meta"]["a2c_cancel_reason"], "agent_requested");
        assert_eq!(result.is_error, Some(true));

        let history = computer.get_tool_history().await.unwrap();
        let rec = history
            .iter()
            .find(|r| r.req_id == "rid-cancel")
            .expect("历史应落一条记录");
        assert!(!rec.success);
        assert_eq!(rec.error.as_deref(), Some("cancelled"));

        // 已完成：注册表已注销，再次 acancel 落空回 false（幂等）。
        assert!(
            !computer.acancel_tool("rid-cancel").await,
            "已完成的 req_id 再次取消应回 false"
        );
    }
}
