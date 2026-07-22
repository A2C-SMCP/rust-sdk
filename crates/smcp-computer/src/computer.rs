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
use std::sync::Weak;
use tokio::sync::{broadcast, mpsc, Mutex, RwLock};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

// INT-01 #68：SKILL / blob 子系统编排 / SKILL & blob subsystem orchestration。
use crate::blob::{
    decode_blob_handle, default_thresholds, encode_toolspool_handle, BlobHandleError, BlobResolver,
    BlobThresholds, BlobTooLargeError, DecodedHandle, ResolvedBlob, SkillBlobResolver,
    SkillRootLookup, ToolspoolBlobResolver, ToolspoolBlobStore,
};
// 治理生命周期：只导入类型；自由函数全限定调用以免与同名 Computer 方法混淆 / types only; call free fns FQ.
use crate::governance::{
    resolve_governance_snapshot, GovernanceArgs, GovernanceQueryError, GovernanceRuntimeOverlay,
    GovernanceSnapshot, ListPluginsOptions, MarketplaceSnapshot, PluginSnapshot, PluginStatus,
};
use crate::inventory::{McpOwnership, McpServerWithMetadata};
use crate::settings::config::{
    import_mcp_servers as import_mcp_servers_cfg, load_config, preflight_mcp_import, update_config,
    ConfigContext, ConfigEdit, ConfigEntity, EditIntent, PlannedServer, PreflightReport,
    ProvenanceScope, WriteScope, WriteTargetError, WriteTargetOptions,
};
use crate::settings::installer::{
    DisableOptions, EnableOptions, InstallOptions, McpInstallHooks, PluginInstallError,
    UninstallOptions,
};
use crate::settings::lifecycle::{
    AddMarketplaceParams, GovernanceError, MarketplaceAddOutcome, MarketplaceRefreshRow,
    MarketplaceRemoveOutcome, RemoveMarketplaceParams,
};
use crate::settings::mcp_config::canonicalize_persist_body;
use crate::settings::policy::resolve_policy_settings;
use crate::settings::reconciler::InstalledPluginRecord;
use crate::settings::recovery::{BundledServerRecord, GovernanceRecoveryReport};
use crate::settings::scope::{resolve_settings, EnvMap, ResolveSettingsArgs};
use crate::skills::{
    resolve_skill_home, resolve_skill_view, stage_mcp_skills, stage_user_skills, user_dropin_root,
    AsyncCallback, CallbackResult, OnChange, SkillEventDebouncer, SkillFileWatcher, SkillRegistry,
    SkillResourceView, SkillSandboxError, SOURCE_USER,
};
use smcp::utils::env_truthy;
use smcp::A2CSkillRef;

use crate::errors::{ComputerError, ComputerResult};
use crate::inputs::handler::InputHandler;
use crate::inputs::load_env_file;
use crate::inputs::model::InputValue;
use crate::inputs::runtime_resolver::{
    InputKind, InputResolutionError, InputValueResolver, SecretValueResolver,
};
use crate::inputs::{env_var_name, utils::run_command};
use crate::mcp_clients::{
    manager::MCPServerManager,
    model::{
        content_as_text, is_call_tool_error, BundleId, CallToolResult, CancellableCallOutcome,
        Content, MCPServerConfig, MCPServerInput, McpChangeKind, McpServerNotification,
        ReadResourceResult, Resource, ServerName, Tool,
    },
    ConfigRender, RenderError,
};
use crate::socketio_client::{SmcpComputerClient, SmcpComputerClientBuilder};
use crate::status::{ComputerEvent, ComputerStatusSnapshot, LifecycleState, RuntimeStatus};

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

/// [`Computer::connect_socketio`] 的连接可选项 / Options for [`Computer::connect_socketio`].
///
/// 用具名字段替代位置参。#86 起连接面鉴权**唯一**走 Socket.IO auth dict
/// （[`auth_payload`](Self::auth_payload)，如 `{"token":"<jwt>"}`，server 默认读 `token` 字段）；
/// HTTP header 仅用于路由（[`headers`](Self::headers)，如 `X-TF-*`，**非鉴权**）。A2C-SMCP auth-agnostic。
///
/// Named-field options. Since #86 connection auth lives **only** in the Socket.IO auth dict
/// (`auth_payload`); HTTP headers are routing-only.
#[derive(Debug, Clone)]
pub struct ConnectOptions {
    /// Socket.IO CONNECT `auth` 字段负载（连接面鉴权唯一信道）；auth-agnostic，整个 JSON 由调用方决定。
    /// Socket.IO CONNECT `auth` payload (the sole connection-auth channel; caller owns the JSON).
    pub auth_payload: Option<serde_json::Value>,
    /// 路由 HTTP upgrade headers，`"k:v,foo:bar"` 串（沿用 `parse_headers_string`；**非鉴权**）。
    /// Routing HTTP upgrade headers as a `"k:v,foo:bar"` string (NOT for auth).
    pub headers: Option<String>,
    /// 应用层 namespace；[`Default`] 为 [`smcp::SMCP_NAMESPACE`] (`/smcp`)。
    /// Application-layer namespace; defaults to `/smcp`.
    pub namespace: String,
}

impl Default for ConnectOptions {
    fn default() -> Self {
        Self {
            auth_payload: None,
            headers: None,
            namespace: smcp::SMCP_NAMESPACE.to_string(),
        }
    }
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
/// `Clone`：socketio 接线（#72）的 `Computer::clone_for_handlers` 需克隆 Session 以构造
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
    /// MCP 服务器配置映射（键 = `bundle_id`，server 唯一身份）/ MCP server configs keyed by bundle_id。
    ///
    /// #127：键从 display `name` 改为 `bundle_id`——协议 §身份正交性规定 `name` 允许碰撞、永不做键。
    /// name-keyed 时两个同名 + 显式不同 `bundle_id` 的合法共存 server 会在此**折叠**（后写覆盖先写），
    /// 使 `list_mcp_servers` / inventory 归属 / CLI `status` 少一条身份、并令 `server rm <bundle_id>`
    /// 删错对象。存 **raw** config（保留 `${input:*}` 引用，与落盘一致）；键取 `render_server_config`
    /// 已 stamp 的值（从 raw 派生，#117：不在此重派生，避免 raw/rendered 连接身份漂移）。
    mcp_servers: RwLock<HashMap<BundleId, MCPServerConfig>>,
    /// #147/S14：宿主构造入参 `Computer::new(mcp_servers=…)` 的 **frozen 声明快照**（embed 层）。
    ///
    /// 与 `mcp_servers`（运行期可变物化集，随 mount/unmount/add 变动）**分离**——本字段构造后**不可变**、
    /// 每次 resolve 作 embed 层重投影、**不落盘**（协议 §2.5-5「origin 可从当次 boot 输入重建、MUST NOT
    /// 落盘为快照」）。**消费方 = 回收判据(#139) + remove 守卫**：二者经 `resolve_snapshot(embed_servers=…)`
    /// 读 `origin=embed`（#139 过滤 `origin != Plugin` 永不连坐 embed；remove 见只读 embed 声明 →
    /// `ReadOnlyOrigin`）。注：`managedBy`(F1) 走 `self.mcp_servers`+账本、**不经**本层（embed 现归
    /// `managedBy=User`）。CLI 空集构造下恒空（协议 §2.5-4）。
    embed_servers: Vec<MCPServerConfig>,
    /// #139：`--mcp-config <file>` 的 **flag 层** mcp.json 路径（当次 boot 的声明式输入，与 `embed_servers` 同族）。
    ///
    /// 协议 §2.5-5 要求 origin 集「可从当次 boot 的声明式输入重建」——Computer 即该 boot 对象，故 flag 与 embed
    /// 同住于此，经 [`non_plugin_declared_bundle_ids`](Self::non_plugin_declared_bundle_ids) 一并投影。
    /// **回收判据 MUST 含本层**：漏传会让经 `--mcp-config` 声明的用户 server 退回「非用户声明」而被连坐停摘
    /// （#153 遗留缺口形状）。镜像 python `Computer(mcp_flag_config=)`。
    mcp_flag_config: Option<PathBuf>,
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

    // ── #106：MCP 运行期变化检测 / runtime MCP change detection ──────────────────
    /// 上次已知的 `window://` URI 集合（desktop 集合去抖缓存）。`ResourceListChanged` 到达时与新集合比较，
    /// 仅集合变化才 emit `server:update_desktop`（对齐 desktop.md §变化检测）；`Arc` 供消费者任务共享。
    /// Last-known window:// URI set for desktop set-diff debouncing.
    desktop_window_uris: Arc<RwLock<HashSet<String>>>,
    /// MCP 变化通知单消费者任务句柄（boot 起、disconnect 停）/ MCP change-notification consumer task handle。
    mcp_notify_task: Arc<Mutex<Option<JoinHandle<()>>>>,

    // ── #112 S5：D1 运行期 input/secret 注入契约 / D1 runtime input/secret injection ──────────
    /// client 注入的 input 值 resolver（= `RuntimeOptions.input_resolver`；缺省 None）。D1：SDK 不落盘明文值，
    /// server-start 渲染 `${input:*}` 时经此向 client 取值 / client-provided input resolver。
    input_resolver: Option<Arc<dyn InputValueResolver>>,
    /// client 注入的 secret resolver（= `RuntimeOptions.secret_resolver`；缺省 None）。仅 `password:true` input 走此，
    /// SDK 不落盘 secret 明文 / client-provided secret resolver。
    secret_resolver: Option<Arc<dyn SecretValueResolver>>,

    // ── #114 S7：runtime status / observability / runtime status surface ──────────
    /// runtime 生命周期状态 + 分离单调 revision（config ⊥ capability）+ 公开诊断 + 事件广播。`Arc` 跨 clone 共享，
    /// 使 handler-detached 克隆与本体观测同一视图。见 [`crate::status`] / status snapshot + monotonic revisions。
    status: Arc<RuntimeStatus>,

    // ── #113 S6：config 落盘锚点 / config persistence anchor ──────────────────
    /// SDK-owned config CRUD 的 project 锚点目录（缺省进程 cwd，#98；测试/部署可经 [`with_config_dir`] 注入）。
    /// runtime mutate（`add_or_update_server`/`remove_server`）落盘经此锚点的 project/local scope，**只碰
    /// project 含 local、不碰 home**（D1/§2.3 四根边界）。构造期 seam，跨 clone 保留（与 `skill_home_override` 同）。
    config_dir: Option<PathBuf>,

    // ── #121：per-Computer User-config 环境上下文 / per-instance User-config env context ──────────
    /// SDK-owned config 解析的 **User-scope 环境映射**（HOME / XDG_CONFIG_HOME）。缺省 `None` → 走进程环境
    /// （CLI / ambient，与今日一致）。嵌入式多实例 client 经 [`with_config_env`] 注入，使 runtime add/update/remove
    /// 的**来源解析 + 写目标 + 重载**、以及 ownership inventory / boot 恢复，全部锚定**本实例**的 User config，
    /// **绝不**读/改/删宿主或其他实例的 `~/.config/a2c/mcp.json`（补 #113 只锚 project 的 env-context 洞）。
    /// 构造期 seam，跨 clone 保留（与 `config_dir` / `skill_home_override` 同）。
    config_env: Option<EnvMap>,
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

/// 递归扫描配置 JSON，收集所有 `${input:<id>}` 引用的 input id（#112 S5）/ collect referenced input ids。
///
/// 与 [`ConfigRender`](crate::mcp_clients::ConfigRender) 同一占位符文法（`\$\{input:<id>}`）。供
/// [`render_server_config`](Computer::render_server_config) **只解析被引用的 input**——未被引用者不 resolve，从而不触发
/// 其 resolver / keyring / command 副作用，也天然容忍其缺失。占位符替换不递归到替换值内，故单次扫描原始配置即完整。
fn collect_referenced_input_ids(config: &serde_json::Value) -> HashSet<String> {
    // 与 `mcp_clients::render::ConfigRender` 同一占位符文法；hoist 为 static 避免每次渲染重新编译。
    static INPUT_PLACEHOLDER_RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| {
            regex::Regex::new(r"\$\{input:([^}]+)}").expect("static input-placeholder regex")
        });
    fn walk(v: &serde_json::Value, re: &regex::Regex, out: &mut HashSet<String>) {
        match v {
            serde_json::Value::String(s) => {
                for cap in re.captures_iter(s) {
                    out.insert(cap[1].to_string());
                }
            }
            serde_json::Value::Array(a) => a.iter().for_each(|x| walk(x, re, out)),
            serde_json::Value::Object(m) => m.values().for_each(|x| walk(x, re, out)),
            _ => {}
        }
    }
    let mut out = HashSet::new();
    walk(config, &INPUT_PLACEHOLDER_RE, &mut out);
    out
}

/// 合并 VS Code 风格 `envFile` 的 `KEY=VALUE` 进 stdio `server_parameters.env`（显式 env 胜，§9.1）/
/// merge a VS Code-style envFile's KEY=VALUE into a stdio server's env (explicit env wins)。
///
/// 在 [`render_server_config`](Computer::render_server_config) 渲染后、反序列化前作用于 JSON 值：envFile
/// 路径此时已展开（`${input:...}` / `${userHome}` 等）。仅对 stdio（`server_parameters` 含 `env`/`command`）生效；
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
    let invalidate: AsyncCallback = Arc::new(
        move || -> Pin<Box<dyn Future<Output = CallbackResult> + Send>> {
            let registry = inv_registry.clone();
            let home_cell = inv_home.clone();
            Box::pin(async move {
                let home = home_cell.read().expect("skill_home poisoned").clone();
                let Some(home) = home else {
                    return Ok(());
                };
                let mut reg = registry.write().await;
                let discovered: HashSet<String> =
                    stage_user_skills(&mut reg, &home).into_iter().collect();
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
/// #140 迁移诊断：若已废止的旧 env 名 `A2C_INPUT_<UPPER>` 在环境中存在而新名不存在 → WARN 教改名。
///
/// **仅检测存在性（`var_os`），绝不用旧值解析**——F5 硬切、无双读、无过渡期，旧名恒被忽略。旧名派生
/// 复刻 0.3.0 前的 `A2C_INPUT_` + `to_uppercase()` + 非 `[A-Z0-9]`→`_`（仅供匹配盘上残留，非新契约）。
fn warn_if_legacy_env_var_present(input_id: &str, current_var: &str) {
    let legacy: String = input_id
        .to_uppercase()
        .chars()
        .map(|c| {
            if c.is_ascii_uppercase() || c.is_ascii_digit() {
                c
            } else {
                '_'
            }
        })
        .collect();
    let legacy = format!("A2C_INPUT_{legacy}");
    if std::env::var_os(&legacy).is_some() && std::env::var_os(current_var).is_none() {
        warn!(
            legacy = %legacy, current = %current_var,
            "#140: legacy input env var is set but retired (A2C_INPUT_ → A2C_SMCP_, case-preserved); \
             it is IGNORED — set {current_var:?} instead (no dual-read, F5)"
        );
    }
}

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
/// `ToolCallRet`），故不能直接调用 [`smcp::ToolCallRet::mark_cancelled`]；本 helper 用**同名键**就地写入。
///
/// ⚠️ wire 形态注意（#92）：rmcp `CallToolResult.meta` 为 `#[serde(rename = "_meta")]`（**无条件**），
/// 故本 helper 写入的标记**直接序列化为 `_meta.a2c_*`**，**并非**协议规范的 `meta`。协议合规的出线
/// `meta.a2c_*` 由 tool_call ack 边界的 `promote_result_meta_to_meta`（socketio_client.rs）把顶层
/// `_meta` 提升为 `meta` 而产生（对齐 Python `result.meta=` + `model_dump`，data-structures.md §234）。
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
    ///
    /// **`mcp_servers` 入参键被忽略（#127）**：内部投影按 `bundle_id`（server 唯一身份）建键，键由每条
    /// config **自身**经 `resolve_bundle_id` 派生，**不采信调用方给的键**。历史契约是 `name -> config`，
    /// 而调用方通常直接从 name-keyed 的 `mcp.json`（协议 §9.1 合法 name-keyed）播种；若原样搬入，键 ≠
    /// 真身份时（`name` 含 `.`/CJK、或配置显式设了 `bundle_id`）会污染整个投影——`BundleId = String` 是类型
    /// 别名，编译期无信号。派生而非采信亦与 `boot_up` 一致（它一贯忽略键、只读 config）。
    ///
    /// 同一 `bundle_id` 的多条 config 会互相覆盖（入参 `HashMap` 本就无序，无稳定 first-wins 可言）；
    /// 需要确定性 no-double-open 语义请改走 [`mount_server`](Self::mount_server) 或 boot 后的
    /// `MCPServerManager::initialize`（按配置顺序 first-wins + 诊断）。
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
        let raw_mcp_servers = mcp_servers.unwrap_or_default();
        // #147/S14：frozen embed 声明快照 = 构造入参**原样**（存储层不折叠，保留同 display 名异显式 bundle_id
        // 的多条）。注意 resolve 的 embed 层是 **name-keyed** 投影（`raw_servers.insert(cfg.name())`，last-wins，
        // 与 python `_embed_layer` dict 及所有文件层同构）⇒ 同名多条在**声明面**按 name 归一。构造后不可变，
        // 与下方运行期可变物化集分离。
        let embed_servers: Vec<MCPServerConfig> = raw_mcp_servers.values().cloned().collect();
        // 按 bundle_id 重建键（丢弃调用方键，见上方 rustdoc）。
        let mcp_servers: HashMap<BundleId, MCPServerConfig> = raw_mcp_servers
            .into_values()
            .map(|cfg| (crate::mcp_clients::bundle_id::resolve_bundle_id(&cfg), cfg))
            .collect();

        // 共享 Arc 句柄先建，供去抖器回调捕获（避免 Computer ↔ debouncer 强引用环）。
        // Pre-create shared handles so debouncer callbacks capture clones, not Computer itself.
        let skill_registry: Arc<RwLock<SkillRegistry>> =
            Arc::new(RwLock::new(SkillRegistry::new()));
        let skill_home: Arc<StdRwLock<Option<PathBuf>>> = Arc::new(StdRwLock::new(None));
        let socketio_client: Arc<RwLock<Option<Arc<SmcpComputerClient>>>> =
            Arc::new(RwLock::new(None));

        let skill_debouncer = Arc::new(build_skill_debouncer(
            &skill_registry,
            &skill_home,
            &socketio_client,
        ));

        Self {
            name,
            mcp_manager: Arc::new(RwLock::new(None)),
            inputs: Arc::new(RwLock::new(inputs)),
            mcp_servers: RwLock::new(mcp_servers),
            embed_servers,
            mcp_flag_config: None,
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
            skill_debouncer,
            skill_watcher: Arc::new(Mutex::new(None)),
            skill_watch_polling: env_truthy("A2C_SKILL_WATCH_POLLING"),
            blob_cache_root_override: None,
            blob_thresholds: default_thresholds(),
            toolspool_store: Arc::new(RwLock::new(None)),
            blob_resolvers: Arc::new(RwLock::new(HashMap::new())),
            inflight_tool_tasks: Arc::new(StdMutex::new(HashMap::new())),
            desktop_window_uris: Arc::new(RwLock::new(HashSet::new())),
            mcp_notify_task: Arc::new(Mutex::new(None)),
            input_resolver: None,
            secret_resolver: None,
            config_dir: None,
            config_env: None,
            // #114 S7：初始生命周期 = Created（尚未 boot 初始化本地资源）/ status starts at Created。
            status: Arc::new(RuntimeStatus::new()),
        }
    }

    /// 注入 SKILL Home 覆盖（测试/部署）/ Inject a SKILL Home override。
    #[must_use]
    pub fn with_skill_home(mut self, home: impl Into<PathBuf>) -> Self {
        self.skill_home_override = Some(home.into());
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

    /// 注入 config 落盘锚点（缺省进程 cwd）/ Inject the config-persistence anchor (default: process cwd)。
    ///
    /// #113 S6：`add_or_update_server` / `remove_server` 落盘经此目录的 project/local scope（#98 project 锚点）。
    /// 测试/部署据此定向落盘目录，避免污染真实进程 cwd。**只碰 project 含 local、不碰 home**。
    #[must_use]
    pub fn with_config_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.config_dir = Some(dir.into());
        self
    }

    /// 注入 User-config 环境上下文（HOME / XDG_CONFIG_HOME）/ Inject the User-config env context。
    ///
    /// #121：嵌入式多实例 client 为**单个** `Computer` 提供固定的 User-scope 解析环境。注入后，runtime
    /// `add_or_update_server` / `remove_server` 的来源解析与写目标、`list_mcp_servers_with_metadata` 的归属门控、
    /// 以及 boot 的 `reconcile_governance` 恢复，全部锚定**本实例** User config，**不**回退宿主进程 `$HOME`/`$XDG`。
    /// 缺省（未注入）→ `None` → 走进程环境（CLI / ambient，行为与今日一致，守 #121 DoD「未注入保持 ambient」）。
    #[must_use]
    pub fn with_config_env(mut self, env: impl Into<EnvMap>) -> Self {
        self.config_env = Some(env.into());
        self
    }

    // ── #113 S6：config 落盘锚点解析 / config anchor resolution ──────────────────
    /// 解析 config 落盘锚点：override > 进程 cwd（#98：`Computer` 不再持有 workspace，project/local 锚进程 cwd）。
    fn config_dir(&self) -> PathBuf {
        self.config_dir
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
    }

    // ── #121：User-config 环境上下文解析 / User-config env resolution ──────────────────
    /// 本实例的 User-config 环境映射（`None` → 进程环境，与今日一致）/ this instance's User-config env (None → process env)。
    fn config_env(&self) -> Option<&EnvMap> {
        self.config_env.as_ref()
    }

    /// #121：以本实例上下文（project 锚点 + User env + Skill Home）组装 [`ConfigContext`]，供 runtime config CRUD。
    ///
    /// 补 #113 只锚 project 的洞：`env` 注入使 User-scope（`~/.config/a2c/mcp.json`）解析锚定**本实例**、不回退宿主
    /// 进程 `$HOME`/`$XDG`；`home` 注入使 home DropIn / 账本源亦锚定本实例。`config_dir` / `home` 借用须与返回的
    /// context 同域存活（调用方持有 owned `PathBuf` 局部）。
    ///
    /// #123（协议#19 加固）：`upsert_new_scope` 决定**新** server 声明的落盘 scope——公开 CRUD 默认 `Local`
    /// （`mcp.local.json`，不入 git；仍 boot 读取→重启存活），避免 API/UI 加的 server 静默污染团队共享层；
    /// `remove` 不 upsert，此参数对其无影响。
    /// 注入 `--mcp-config <file>` 的 flag 层路径（#139）/ inject the `--mcp-config` flag-layer path。
    ///
    /// 与 `Computer::new(mcp_servers=…)`（embed 层）同属**当次 boot 的声明式输入**（§2.5-5）：二者共同构成
    /// 回收判据的「非用户声明」数据源。CLI `--mcp-config` 与嵌入宿主均应注入，否则该层声明的用户 server 会在
    /// plugin uninstall/disable/gc 时被**连坐**停摘。镜像 python `Computer(mcp_flag_config=)`。
    #[must_use]
    pub fn with_mcp_flag_config(mut self, path: impl Into<PathBuf>) -> Self {
        self.mcp_flag_config = Some(path.into());
        self
    }

    /// 宿主构造入参的 frozen embed 声明快照（#147）——供 CLI boot 审批的 resolve embed 层投影。
    /// CLI 空集构造下恒空；SDK 嵌入宿主传 `Computer::new(mcp_servers=…)` 时非空。非 CLI 路径直接读
    /// `self.embed_servers`（如 #139 回收判据），故本访问器随其唯一用户 `cli` feature 门控。
    #[cfg(feature = "cli")]
    pub(crate) fn embed_servers(&self) -> &[MCPServerConfig] {
        &self.embed_servers
    }

    fn instance_config_context<'a>(
        &'a self,
        config_dir: &'a std::path::Path,
        home: &'a std::path::Path,
        upsert_new_scope: WriteScope,
    ) -> ConfigContext<'a> {
        ConfigContext {
            env: self.config_env(),
            home: Some(home),
            // #147：remove 守卫按 origin 判定须能看见 embed 声明（否则宿主构造 server 被判「无声明的纯运行期
            // 投影」误拒/误判）。frozen 快照，与 mcp.json/durable 同入声明面。
            embed_servers: &self.embed_servers,
            opts: WriteTargetOptions {
                upsert_new_scope,
                ..WriteTargetOptions::default()
            },
            ..ConfigContext::new(config_dir)
        }
    }

    // ── INT-01 #68：SKILL Home 解析 / SKILL Home resolution ──────────────────
    /// 读**已解析**的 SKILL Home，不触发解析/建目录 / read the cached SKILL Home without side effects。
    ///
    /// 与 [`ensure_skill_home`](Self::ensure_skill_home) 的区别是**无副作用**：后者会解析并落缓存（且下游
    /// `skill_home()` 会建目录）。只读路径（如 #141 的 CLI 候选表）用本方法——boot 后恒已解析，boot 前
    /// 返回 `None` 而不是凭空造出一个 home。对齐 python `collect_candidates` 读裸 `_skill_home` 的约定。
    fn skill_home_opt(&self) -> Option<PathBuf> {
        self.skill_home
            .read()
            .expect("skill_home poisoned")
            .clone()
            .or_else(|| self.skill_home_override.clone())
    }

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

    /// 是否有挂起未结算的 SKILL 去抖窗口（[`mark_skills_dirty`](Self::mark_skills_dirty) 已触发、尚未 emit）/
    /// whether a SKILL settlement window is pending。
    ///
    /// 内省 / 测试用：治理封装（marketplace/plugin lifecycle、[`reconcile_governance`](Self::reconcile_governance)）
    /// 成功后应标脏、空恢复不应标脏，本访问器使该接线可被断言（默认窗口 300ms，调用后同步可见）。
    #[must_use]
    pub fn skill_settlement_pending(&self) -> bool {
        self.skill_debouncer.has_pending()
    }

    /// SKILL Home 绝对根（CLI marketplace/skill 命令经此取物化根）/ the SKILL Home root。
    pub fn skill_home(&self) -> PathBuf {
        self.ensure_skill_home()
    }

    /// SKILL registry 的共享句柄 / shared SKILL registry handle。
    ///
    /// CLI REPL 的 governance 命令（marketplace/plugin add/install/...）经此取写锁拿到 `&mut SkillRegistry`
    /// 调 handler（installer/staging 直收 `&mut SkillRegistry`，不再回锁 registry；与 [`add_or_update_server`]
    /// 等只触 `mcp_servers`/`mcp_manager` 的方法无锁序冲突）。
    ///
    /// [`add_or_update_server`]: Self::add_or_update_server
    pub fn skill_registry_arc(&self) -> Arc<RwLock<SkillRegistry>> {
        Arc::clone(&self.skill_registry)
    }

    // ── Computer 级 marketplace / plugin 生命周期 API（#94）/ governance lifecycle API ──────
    //
    // 把 marketplace/plugin 生命周期编排从 `cli` feature 抬到 `Computer` 级（#93 北极星）：GUI/Tauri 产品
    // client 无需启用 `cli` feature、无需直接触碰 `SkillRegistry`，即可在**构造期固定的 `skill_home` 边界内**
    // 完成治理。统一锁纪律：取 `skill_registry` 写锁 → 调非 CLI 编排核心（[`crate::settings::lifecycle`] /
    // [`crate::settings::installer`]）→ 成功后 `mark_skills_dirty()`。`home` 恒取自 `skill_home()`（运行期
    // 只读，**无** setter），`env = None`（进程环境）。`McpInstallHooks` 保持可注入，让产品 client 把 plugin
    // bundled MCP server 物化到自己的 MCP 配置模型。**不**含 boot 恢复 / `reconcile_governance()`（归 Sub-B）。

    /// #124：治理声明式内容变更后统一发信号——bump **config** revision（经 [`subscribe_events`](Self::subscribe_events)
    /// 发 [`ComputerEvent::ConfigRevisionBumped`]，使 GUI 无需轮询内部文件即可观察治理生命周期变更）+ fire-and-forget
    /// socketio `update_config` 通知（未连接 → `InvalidState` 静默，绝不 panic）。
    ///
    /// **调用契约（无虚假 bump）**：仅在 mutator **成功且确有内容变更**的路径调用——`add_marketplace` / `remove_marketplace`
    /// 对重复/未知目标返 `Err`（[`GovernanceError::DuplicateMarketplace`] / `UnknownMarketplace`）、`install_plugin`
    /// 成功即真实（重）物化账本、`uninstall_plugin` 仅在 `Ok(true)`（确有移除）调用，故成功即真变，无需再门控。
    /// 这与 `enable/disable_plugin` **刻意**用 `changed` 门控（#115 R1：re-enable 已启用者是返 `Ok` 的**真 no-op**、
    /// 须避免虚假 bump）的差异**是有意的**——install/uninstall/add/remove 的成功语义本身已排除 no-op。§12 R2：
    /// config ⊥ capability——bundled server 重挂的能力变化由 MCP start/stop 各自 bump capability，与此正交。
    async fn emit_governance_config_change(&self) {
        self.bump_config_revision();
        let _ = self.emit_update_config().await;
    }

    /// 添加 marketplace（归一 URL + 派生名 + clone/stage 或仅注册意图）/ add a marketplace。
    ///
    /// 信任门（user-scope `trustedMarketplaces`）由产品 client 在调用前自理——**不**属 `skill_home` 治理边界。
    ///
    /// # Errors
    /// 见 [`GovernanceError`]（非法 URL/名、重名、clone 失败、账本写失败）。
    pub async fn add_marketplace(
        &self,
        git_url: &str,
        params: AddMarketplaceParams<'_>,
    ) -> Result<MarketplaceAddOutcome, GovernanceError> {
        let home = self.skill_home();
        let res = {
            let mut reg = self.skill_registry.write().await;
            crate::settings::lifecycle::add_marketplace(&mut reg, &home, None, git_url, params)
                .await
        };
        if res.is_ok() {
            self.mark_skills_dirty();
            self.emit_governance_config_change().await;
        }
        res
    }

    /// 刷新 marketplace（`git pull` 失败则全量重 clone；逐 marketplace 对账分类）/ refresh marketplaces。
    ///
    /// `target == "all"` → 全部已知 marketplace；否则单个目标。未知目标 → `missing` 行（不整体报错）。
    pub async fn refresh_marketplace(&self, target: &str) -> Vec<MarketplaceRefreshRow> {
        let home = self.skill_home();
        let rows = {
            let mut reg = self.skill_registry.write().await;
            crate::settings::lifecycle::refresh_marketplaces(&mut reg, &home, None, target).await
        };
        // refresh 即便全 unchanged 也可能翻活孤儿 / 重挂；统一标脏交去抖器判定。
        self.mark_skills_dirty();
        rows
    }

    /// 移除 marketplace（默认级联卸载其下 installed plugin + prune clone；`keep_plugins` 仅 prune）/ remove。
    ///
    /// trust 撤销（user-scope）由产品 client 自理。`hooks` 提供 `remove_server` 供级联卸载摘除 bundled server。
    ///
    /// # Errors
    /// 未知 marketplace → [`GovernanceError::UnknownMarketplace`]。
    pub async fn remove_marketplace(
        &self,
        name: &str,
        params: RemoveMarketplaceParams<'_>,
    ) -> Result<MarketplaceRemoveOutcome, GovernanceError> {
        let home = self.skill_home();
        // #139：全集回收判据数据源（durable + flag + embed + 实例 config_dir/config_env）——marketplace 级联
        // 卸载 MUST 用全集，否则连坐用户/宿主自有 server（同 uninstall/disable）。
        let non_plugin = self.non_plugin_declared_bundle_ids(&home, self.config_env());
        let res = {
            let mut reg = self.skill_registry.write().await;
            crate::settings::lifecycle::remove_marketplace(
                &mut reg,
                &home,
                None,
                name,
                params,
                &non_plugin,
            )
            .await
        };
        if res.is_ok() {
            self.mark_skills_dirty();
            self.emit_governance_config_change().await;
        }
        res
    }

    /// 安装单个 plugin（外来 MCP 同名硬抛、原子失败，§10.6）/ install a plugin。
    ///
    /// `options.env`/scope 由调用方按上下文给定；`home` 恒取 `skill_home()`。`hooks=None` ⇒ ledger-only。
    ///
    /// # Errors
    /// 见 [`PluginInstallError`]（冲突 / 前置 / manifest / 定位 / 注入 / 账本）。
    pub async fn install_plugin(
        &self,
        plugin_id: &str,
        options: InstallOptions<'_>,
        hooks: Option<&dyn McpInstallHooks>,
    ) -> Result<InstalledPluginRecord, PluginInstallError> {
        let home = self.skill_home();
        let res = {
            let mut reg = self.skill_registry.write().await;
            crate::settings::installer::install_plugin(plugin_id, &mut reg, &home, options, hooks)
                .await
        };
        if res.is_ok() {
            self.mark_skills_dirty();
            self.emit_governance_config_change().await;
        }
        res
    }

    /// 从 ledger 安装记录解析 plugin 的 enable/disable 落盘 scope（#113 S6，DoD item2）/ resolve enable scope from record。
    ///
    /// `enabledPlugins` 写入 scope **须与安装 scope 一致**（§5.1）。[`installer`](crate::settings::installer) 层刻意
    /// **不**回查（账本可含多 scope 记录、回查有歧义，保 Python 行为），把消解托付给 SDK 接线层——本方法即该层：
    /// **确定性**取该 plugin **首条**安装记录的 `scope`（= 最早安装 / 主 scope）。无安装记录 → `None`（installer
    /// 回退默认 `user`；enable 本就要求已安装，故正常路径必有记录）。**只读**账本，不改任何状态。
    fn resolve_plugin_install_scope(
        &self,
        plugin_id: &str,
        home: &std::path::Path,
        env: Option<&crate::settings::scope::EnvMap>,
    ) -> Option<String> {
        let installed = crate::settings::store::load_installed_plugins(Some(home), env);
        let records = installed.account.plugins.get(plugin_id)?;
        records.iter().find_map(|r| {
            r.extra
                .get("scope")
                .and_then(|v| v.as_str())
                .map(String::from)
        })
    }

    /// 启用单个 plugin（廉价复原：复活 skills + 重挂 server；hook 失败原子回滚）/ enable a plugin。
    ///
    /// **scope（#113 S6）**：`options.scope` 缺省时从 ledger 安装记录**消解**（`resolve_plugin_install_scope`，
    /// **非恒定 user**，守「与安装 scope 一致」契约）；显式传入则原样尊重。**仅当 `enabledPlugins` 内容真变**时
    /// bump **config** revision + `emit_update_config`（#115 R1：installer 据实际写盘返回 `changed`，幂等 re-enable
    /// 不虚假 bump / 不惊动 robot）；bundled server 若翻活经 hooks 走 [`Self::mount_server`] 另 bump capability（§12 R2 正交）。
    ///
    /// # Errors
    /// 见 [`PluginInstallError`]（未安装 / 冲突 / manifest / settings 写 / 注入）。
    pub async fn enable_plugin(
        &self,
        plugin_id: &str,
        options: EnableOptions<'_>,
        hooks: Option<&dyn McpInstallHooks>,
    ) -> Result<(), PluginInstallError> {
        let home = self.skill_home();
        // #121：`options.env` 缺省时回退**本实例** `config_env`（与 runtime CRUD / boot 迁移同源），避免注入实例
        // 上下文的嵌入式 client 忘传 env 时「enable 写宿主、inventory/reconcile 读实例」的 split-brain；未注入 →
        // None → ambient（CLI 不变）。显式传入 `options.env` 仍优先（override）。
        let env = options.env.or_else(|| self.config_env());
        // scope 缺省 → 按安装记录消解（非恒定 user）；resolved 为本地 String，effective 借其，二者同域存活。
        let resolved_scope = if options.scope.is_none() {
            self.resolve_plugin_install_scope(plugin_id, &home, env)
        } else {
            None
        };
        let effective = EnableOptions {
            scope: options.scope.or(resolved_scope.as_deref()),
            project_path: options.project_path,
            timeout: options.timeout,
            env,
        };
        let res = {
            let mut reg = self.skill_registry.write().await;
            crate::settings::installer::enable_plugin(plugin_id, &mut reg, &home, effective, hooks)
                .await
        };
        match res {
            Ok(changed) => {
                // skills 可能因孤儿复活而变（installer 无条件 re-stage），故 mark_skills_dirty 不受 changed 门控。
                self.mark_skills_dirty();
                // #115 R1（方案 A）：只在 `enabledPlugins` **内容真变**时 bump config revision + 通知 robot——
                // installer 据实际写盘结果返回 `changed`；幂等 re-enable（已启用）→ false → 不虚假 bump、不惊动
                // robot（对齐 add/remove「真变才 bump」，false-negative 安全：写了即真变）。§12 R2：config ⊥
                // capability——server 重挂的能力变化由 MCP start/stop 各自 bump capability，与此正交。
                if changed {
                    self.bump_config_revision();
                    let _ = self.emit_update_config().await;
                }
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// #139/§4.9.1-2 回收判据「X 非用户声明」项的数据源：**带 origin 的运行期权威配置集中 `origin != plugin`
    /// 的 bundle_id 集**（durable scopes + flag `--mcp-config` + embed 构造入参）。每次重算、零持久态（§2.5-5）。
    ///
    /// 传给 disable/uninstall/gc 的回收判据（`reclaimable_mcp_deps`）——**MUST 传全集**：退回只读 mcp.json
    /// 声明面会让经 flag/embed 挂载的用户/宿主 server 被误判「非用户声明」而连坐停摘（#153 缺口形状）。
    /// 经 [`resolve_snapshot`] 消费 [#147](Self::embed_servers) 投影的 `origin=embed`。
    pub(crate) fn non_plugin_declared_bundle_ids(
        &self,
        home: &std::path::Path,
        env: Option<&EnvMap>,
    ) -> std::collections::HashSet<BundleId> {
        use crate::settings::config::snapshot::{resolve_snapshot, SnapshotArgs};
        use crate::settings::config::ProvenanceScope;
        // project/local 锚点 MUST 用**实例** `config_dir`（与 `governance_snapshot` 同源）——漏传则退回进程 cwd，
        // 嵌入宿主 `with_config_dir(/proj)` 下解析不到 `/proj/.tfrobot/mcp[.local].json` 声明的用户 server，
        // 其 durable(project/local) 声明会被误判「非用户声明」而连坐停摘（#139「永不连坐」覆盖 durable scope）。
        let config_dir = self.config_dir();
        let snap = resolve_snapshot(SnapshotArgs {
            cwd: Some(&config_dir),
            env,
            home: Some(home),
            // 当次 boot 的两条声明式输入 MUST 全传（§2.5-5）：漏 flag ⇒ 经 `--mcp-config` 声明的用户 server
            // 被误判「非用户声明」而连坐；漏 embed ⇒ 宿主构造 server 同理（#147）。
            flag_mcp_config_path: self.mcp_flag_config.as_deref(),
            embed_servers: &self.embed_servers,
            ..Default::default()
        });
        snap.mcp
            .servers
            .iter()
            .filter(|s| s.origin != ProvenanceScope::Plugin)
            .map(|s| crate::mcp_clients::bundle_id::resolve_bundle_id(&s.config))
            .collect()
    }

    /// **声明面**：当次 boot 权威配置集里的 server 声明（durable scopes + flag + embed），供 CLI 寻址补全。
    ///
    /// #141：`list_mcp_servers_with_metadata` 的查找空间是「运行期投影 ∪ ledger」，**不含**已落盘但未挂载的
    /// 声明（如卡在审批门外的 pending server）。[`remove_server`](Self::remove_server) 读磁盘快照、本来删得掉
    /// 它们，若 CLI 候选表看不见就会误报「未找到」⇒ display 名不是合法 bundle_id 字面量者从 CLI 无路可删。
    /// 故 CLI 的候选表取二者并集（python `collect_candidates` 决策 1：查找空间 = 运行期活跃集 ∪ 声明面）。
    ///
    /// 与 [`non_plugin_declared_bundle_ids`](Self::non_plugin_declared_bundle_ids) 同源同参（同一次
    /// `resolve_snapshot`、同样必须传全 flag/embed 输入），差别只在此处返回 config、那处返回 bundle_id 集。
    pub(crate) fn declared_mcp_servers(&self) -> Vec<crate::mcp_clients::model::MCPServerConfig> {
        use crate::settings::config::snapshot::{resolve_snapshot, SnapshotArgs};
        let config_dir = self.config_dir();
        let snap = resolve_snapshot(SnapshotArgs {
            cwd: Some(&config_dir),
            home: self.skill_home_opt().as_deref(),
            flag_mcp_config_path: self.mcp_flag_config.as_deref(),
            embed_servers: &self.embed_servers,
            ..Default::default()
        });
        snap.mcp.servers.iter().map(|s| s.config.clone()).collect()
    }

    /// 禁用单个 plugin = 整 plugin 下线（停摘 bundled server + 隐藏 skills；可经 [`Self::enable_plugin`] 复原）/ disable。
    ///
    /// **scope（#113 S6）**：同 [`enable_plugin`](Self::enable_plugin)——缺省时按安装记录消解；**仅当 `enabledPlugins`
    /// 内容真变**时 bump config revision + `emit_update_config`（#115 R1：重复 disable 不虚假 bump）。
    ///
    /// # Errors
    /// 见 [`PluginInstallError`]（id 非法 / settings 写 / `remove_server` 失败）。
    pub async fn disable_plugin(
        &self,
        plugin_id: &str,
        options: DisableOptions<'_>,
        hooks: Option<&dyn McpInstallHooks>,
    ) -> Result<(), PluginInstallError> {
        let home = self.skill_home();
        // #121：同 [`enable_plugin`](Self::enable_plugin)——`options.env` 缺省时回退**本实例** `config_env`，避免
        // 「disable 写宿主、inventory 读实例」的 split-brain；未注入 → None → ambient；显式传入仍优先。
        let env = options.env.or_else(|| self.config_env());
        let resolved_scope = if options.scope.is_none() {
            self.resolve_plugin_install_scope(plugin_id, &home, env)
        } else {
            None
        };
        let effective = DisableOptions {
            scope: options.scope.or(resolved_scope.as_deref()),
            project_path: options.project_path,
            env,
        };
        let non_plugin = self.non_plugin_declared_bundle_ids(&home, env);
        let res = {
            let mut reg = self.skill_registry.write().await;
            crate::settings::installer::disable_plugin(
                plugin_id,
                &mut reg,
                &home,
                effective,
                &non_plugin,
                hooks,
            )
            .await
        };
        match res {
            Ok(changed) => {
                // skills orphan / server 停摘幂等，projection 可能变 → mark_skills_dirty 不受 changed 门控。
                self.mark_skills_dirty();
                // #115 R1（方案 A）：只在 `enabledPlugins` **内容真变**时 bump config revision + 通知 robot——
                // installer 据实际写盘结果返回 `changed`；已禁用再禁用 → false → 不虚假 bump。§12 R2：config ⊥
                // capability——server 停摘的能力变化由 MCP stop 各自 bump capability，与此正交。
                if changed {
                    self.bump_config_revision();
                    let _ = self.emit_update_config().await;
                }
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// 卸载单个 plugin（删 installPath 树 + 注销 skills + 级联停摘 bundled server + 删账本）/ uninstall。
    ///
    /// 未安装 / 无匹配 scope → `Ok(false)`（no-op）。
    ///
    /// # Errors
    /// 见 [`PluginInstallError`]（id 非法 / `remove_server` 失败 / 账本写失败）。
    pub async fn uninstall_plugin(
        &self,
        plugin_id: &str,
        options: UninstallOptions<'_>,
        hooks: Option<&dyn McpInstallHooks>,
    ) -> Result<bool, PluginInstallError> {
        let home = self.skill_home();
        let non_plugin =
            self.non_plugin_declared_bundle_ids(&home, options.env.or_else(|| self.config_env()));
        let res = {
            let mut reg = self.skill_registry.write().await;
            crate::settings::installer::uninstall_plugin(
                plugin_id,
                &mut reg,
                &home,
                options,
                &non_plugin,
                hooks,
            )
            .await
        };
        if matches!(res, Ok(true)) {
            self.mark_skills_dirty();
            self.emit_governance_config_change().await;
        }
        res
    }

    /// 治理状态启动恢复（从 `skill_home` 持久化 ledger 重建边界内派生态）/ governance boot recovery（#95）。
    ///
    /// 冷启动 / 进程重启后，从 `installed_plugins_intent.json`（安装意图，v0.3.0 权威）+ `known_marketplaces.json`
    /// 重挂**已装且启用**（intent ∧ `enabledPlugins==true`）的 marketplace plugin skills；给定 `hooks` 时再经
    /// [`McpInstallHooks`] 重挂其 bundled MCP server（SDK 决定
    /// 「哪些」= 已装且启用 plugin 的 bundled server，client 经 hooks 决定「如何物化」）。由
    /// [`boot_up`](Self::boot_up)（`hooks = None`）自动调用，亦允许 client 显式调用驱动 MCP 重挂。
    ///
    /// - **幂等**：重复调用（boot 自动 + client 显式）结果一致，不重复注册 / 重复 staging。
    /// - **enabled 门控（v0.3.0 翻转）**：仅 `enabledPlugins[pid] == true` 的已装 plugin 恢复——`absent`/`false`
    ///   均**不**激活（install 不再装即活跃；含 disable / #94 enable-rollback 落定 `false` 的半装 plugin）。install-set
    ///   取自 `installedPlugins` 意图（账本 `installed_plugins.json` 仅供 installPath 等 materialization 细节）。
    /// - **降级铁律**：marketplace 源不可达 / clone 树缺失 → WARN 降级、**不**阻断；`register_server` 失败 →
    ///   WARN、**不**阻断（best-effort 重挂）。
    /// - **边界**（#93 Non-Goal）：只在**构造期固定的 `skill_home`** 内重建派生态，**不**改 `skill_home` /
    ///   配置根 / skill 源根。`home = skill_home()`、`env = None`（进程环境）。
    /// - **锁纪律**：阶段一持 `skill_registry` 写锁重挂 skills；**释放写锁后**再经 hooks 重挂 server，避免
    ///   「skill 写锁 → mcp_manager 锁」相反序死锁（见 [`restage_mcp_skills`](Self::restage_mcp_skills) 锁序）。
    ///
    /// ## 调用方须知 / Caller notes
    /// - **boot 仅恢复 skills 派生态**：[`boot_up`](Self::boot_up) 以 `hooks = None` 调用 → 阶段二跳过，**boot
    ///   不重挂任何 bundled MCP server**（boot 期 SDK 无 hooks 对象）。bundled MCP 恢复由 **client** 承担：要么
    ///   client 在重启时把 bundled server 物化进**自己的 MCP 配置模型**（boot 的 `manager.initialize` 自然重挂），
    ///   要么 client 重启后**显式**调 `reconcile_governance(Some(hooks))` 让 SDK 决定「哪些」、client hooks 物化
    ///   「如何」。此为 #93 point 4「client owns MCP config」边界的直接后果，非疏漏。
    /// - **运行期显式调用会阻塞 skill 读**：阶段一对**全部** marketplace 串行 stage 且写锁跨 stage await。常态
    ///   （clone 树已存在、`refresh = false`）仅本地 FS、无网络，开销小；但 clone 树缺失需 clone 时单源最坏
    ///   `DEFAULT_GIT_TIMEOUT`，期间 `get_skills` 等 skill 读阻塞。宜**低频 / 受控**触发（恢复语义本就一次性）。
    /// - **跨重启 enable 语义以 user scope 为准（v0.3.0）**：enabled 门控读合并 `declared`，其 project/local 层来自
    ///   **进程 cwd**（#98：`Computer` 不再持有 workspace）。写在**非进程-cwd 的 project/local scope**
    ///   的 `enabledPlugins=true` 在恢复时可能不可见 → 该 plugin **不**激活。**跨重启可靠启用应写 user scope**；
    ///   project/local-scoped plugin 的 enable 天然 cwd 相关（含存量 v0.2.x 迁移到 project scope 的记录，属 v0.3.0
    ///   per-scope 语义，非疏漏——迁移「不熄灯」保证对 user scope 成立、对 project/local 以进程 cwd 为准）。
    ///
    /// ## 参数 / Params
    /// - `hooks`：`None` = skills-only（boot 默认）；`Some` = client 经 hooks 重挂 bundled MCP server。
    /// - `declared`：`enabledPlugins` 合并声明视图覆盖（对齐 Python `reconcile_governance(declared=...)`）。
    ///   `None` → 内部解析（进程 cwd 的 user/project/local/policy，**无** `--settings` flag scope）；CLI 参考
    ///   接线传 **flag-aware** 视图使 `--settings`-scope 的 `enabledPlugins=false` 在重挂阶段生效。
    ///
    /// 返回 [`GovernanceRecoveryReport`]（恢复 / 跳过 / 降级明细，供观测与测试）。
    pub async fn reconcile_governance(
        &self,
        hooks: Option<&dyn McpInstallHooks>,
        declared: Option<&serde_json::Map<String, serde_json::Value>>,
    ) -> GovernanceRecoveryReport {
        let home = self.skill_home();
        // #121 A：以**本实例** User env 解析 settings / 账本，绝不混入宿主 ambient（守 boot 重启的归属一致）。
        let env = self.config_env();
        // `declared` 覆盖：CLI 参考接线传 **flag-aware** 合并视图（`--settings` scope 生效，对齐 Python
        // `reconcile_governance(declared=...)` kwarg）；`None` → 内部解析（user + 进程 cwd 的 project/local +
        // policy，**无** `--settings` flag scope；跨重启可靠 disable 请写 user scope）。cwd=None（进程态）。
        let resolved_declared;
        let declared: &serde_json::Map<String, serde_json::Value> = match declared {
            Some(d) => d,
            None => {
                let policy = resolve_policy_settings(None, None, None);
                resolved_declared = resolve_settings(ResolveSettingsArgs {
                    cwd: None,
                    env,
                    flag_settings_path: None,
                    policy_settings: Some(&policy),
                })
                .settings;
                &resolved_declared
            }
        };

        // 阶段一：重挂 marketplace skills（持 `skill_registry` 写锁；stage 含 git await，boot 期无并发故安全）。
        let mut report = {
            let mut reg = self.skill_registry.write().await;
            crate::settings::recovery::recover_marketplace_skills(&mut reg, &home, env, declared)
                .await
        };

        // 阶段一·五：账本派生缓存补全（§63 账本删除无损，#104）。**已释放 skill 写锁**、无锁纯 FS/git，故置于
        // phase 1 与 phase 2 之间：意图有 enabled pid 但账本缺记录 → 从意图重物化账本 `installPath`，使随后 phase 2
        // 的 `collect_enabled_bundled_servers` 与 `list_mcp_servers_with_metadata` 归属查询得以重现。**无条件执行**
        // （不看 hooks）：boot 走 `reconcile_governance(None, None)`，须先补回账本供其后查询/重挂读到。
        crate::settings::recovery::rematerialize_missing_ledger_records(
            &home,
            env,
            declared,
            &mut report,
        )
        .await;

        // 阶段二：重挂 bundled MCP server（**已释放 skill 写锁**）。best-effort、逐个降级。
        // 严格镜像 Python `computer.py::reconcile_governance` remount 臂（PR #119 / #100 设计 Y）：
        // ① 同名冲突 → skip + WARN（additive-only，既有 / 用户配置胜，**不覆盖**）；
        // ② 每 plugin 根仅 `inject_inputs` 一次（bundled server 的 `${input:}` 经 D2 前缀回退前置，与
        //    install/enable 流一致）；注入失败 → **隔离该 server**（不 register、不阻断其余）；
        // ③ 成功后把名字并入 `existing`，使同名 bundled server（跨 plugin）后见者亦被跳过（首见胜）。
        if let Some(h) = hooks {
            // #139：去重按 **bundle_id**（身份键）——name 允许碰撞、非身份。同名不同 bundle_id 的合法 server
            // 不再互相隐身；同 bundle_id 已存在 → 既有/用户配置胜（首见胜，additive-only）。
            let mut existing: HashSet<BundleId> = h.existing_servers().into_keys().collect();
            let mut injected_roots: HashSet<PathBuf> = HashSet::new();
            for rec in
                crate::settings::recovery::collect_enabled_bundled_servers(&home, env, declared)
            {
                let name = rec.config.name().to_string();
                let bid = crate::mcp_clients::bundle_id::resolve_bundle_id(&rec.config);
                if existing.contains(&bid) {
                    warn!(server = %name, bundle_id = %bid.as_str(), plugin = %rec.plugin_id,
                        "reconcile_governance: remount skipped (bundle_id conflicts with an existing server, existing wins)");
                    continue;
                }
                // 每 plugin 根注入一次 inputs；注入失败 → 隔离该 server（roots 不入集，同根后续 server 会重试）。
                if !injected_roots.contains(&rec.install_path) {
                    if let Err(e) = h.inject_inputs(&rec.install_path).await {
                        warn!(root = %rec.install_path.display(), plugin = %rec.plugin_id, error = %e,
                            "reconcile_governance: remount inject_inputs failed (non-blocking)");
                        continue;
                    }
                    injected_roots.insert(rec.install_path.clone());
                }
                match h.register_server(rec.config).await {
                    Ok(()) => {
                        existing.insert(bid);
                        report.remounted_servers.push(name);
                    }
                    Err(e) => {
                        warn!(server = %name, bundle_id = %bid.as_str(), plugin = %rec.plugin_id, error = %e,
                            "reconcile_governance: remount register_server failed (non-blocking)");
                    }
                }
            }
        }

        // 派生注册表已变更才标脏（交去抖器 emit `server:update_skills`；boot 期 socketio 未连 → no-op）。
        if !report.restored_skills.is_empty() {
            self.mark_skills_dirty();
        }
        report
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
        let mut reg = self.skill_registry.write().await;
        let discovered: HashSet<String> = stage_user_skills(&mut reg, &home).into_iter().collect();
        reconcile_orphans_in(&mut reg, &discovered, |s| s == SOURCE_USER);
    }

    /// 物化 mcp 源 `skill://` → 注册进 Registry（全量 → `mcp:` 源孤儿对账）/ materialize & register mcp-source skills。
    ///
    /// `bundle_id`（server 唯一身份，**非** display 名——#127）给定则仅重物化该 server（单 server 重枚举，
    /// 不对账）；否则全部活跃 server + 孤儿对账（本轮未出现的 `mcp:` 源 SKILL → 标孤儿，保留以便 source
    /// 回归时恢复）。SKILL Home 未就绪 / 无 manager → 空列表；staging 失败 → 记 ERROR + 空列表（失败隔离，
    /// 对标 Python `_restage_mcp_skills`）。
    /// 由 boot_up 与 MCP `ResourceListChanged`/`ResourceUpdated` 通知处理器（INT-03 #72）触发。
    ///
    /// **持锁语义（#77 两阶段化后）**：`skill_registry` 写锁**不再**跨 `stage_mcp_skills` 的网络 await 持有——
    /// `stage_mcp_skills` 内部按 SKILL 仅在 `finalize`（FS rename + 内存注册，同步无 await）**短持写锁**，
    /// `archive` 网络下载 / `resources` MCP `read_resource` 期间**不持任何 Registry 锁**。慢/卡 fetch 不再阻塞
    /// `get_skills` / `get_skill_ref` 读（修复 Python 单事件循环掩盖、Rust 暴露的尾延迟竞争）。孤儿对账亦短持写锁。
    /// **锁序（#106 并发接线后）**：本路径（含 `McpChangeReactor` 消费者，运行期由 MCP
    /// `ResourceListChanged`/`ResourceUpdated` 通知驱动，**已可达**）取 `mcp_manager.read` → `skill_registry.write`；
    /// CLI REPL 的 governance 路径（`cli::repl`）取 `skill_registry.write` → 经 `CliMcpHooks` 调
    /// `add_or_update_server`/`remove_server` 取 `mcp_manager` 锁。为消除相反序 ABBA，已把
    /// [`add_or_update_server`](Self::add_or_update_server) 的惰性初始化改为「先 read 探测、仅 None 才 write」，
    /// 使 post-boot 的 governance 路径退化为 `skill_registry.write` → `mcp_manager.read`——与本路径的 `mcp_manager.read`
    /// **读读相容**，不再循环等待（`remove_server` 本就只取 `mcp_manager.read`）。#77 后写锁窗口已收窄到 per-SKILL
    /// finalize。回归见 `tests/mcp_change_notifications.rs` 的并发死锁守卫用例。
    pub async fn restage_mcp_skills(&self, bundle_id: Option<&str>) -> Vec<String> {
        let Some(home) = self.skill_home.read().expect("skill_home poisoned").clone() else {
            return Vec::new();
        };
        let manager_guard = self.mcp_manager.read().await;
        let Some(manager) = manager_guard.as_ref() else {
            return Vec::new();
        };
        // #77：写锁不再跨 materialize 网络持有——stage_mcp_skills 内部按 SKILL 在 finalize 阶段短持写锁。
        // #106：物化 + `mcp:` 源孤儿对账抽为共享自由函数，与 [`McpChangeReactor`] 复用（见 restage_mcp_skills_into）。
        restage_mcp_skills_into(manager, &self.skill_registry, &home, bundle_id).await
    }

    /// 直接处理一条 MCP 运行期变化通知（#106）：刷新工具映射 / desktop 集合去抖 / MCP 源 skill 重挂，并触发
    /// 对应 `server:update_*` emit。供**测试直调**与**消费者任务**共用（消费者持 `McpChangeReactor`，
    /// 此方法即时构建等价 reactor）。无 socketio / 未入房间 → emit 均为 no-op。
    pub async fn handle_mcp_notification(&self, notif: McpServerNotification) {
        self.mcp_change_reactor().handle(notif).await;
    }

    /// 从 `self` 的共享状态构建一个 [`McpChangeReactor`]（manager 取 `Weak` 以断开 sender 自持环）。
    fn mcp_change_reactor(&self) -> McpChangeReactor {
        McpChangeReactor {
            manager: Arc::downgrade(&self.mcp_manager),
            socketio_client: Arc::clone(&self.socketio_client),
            skill_registry: Arc::clone(&self.skill_registry),
            skill_home: Arc::clone(&self.skill_home),
            skill_debouncer: Arc::clone(&self.skill_debouncer),
            desktop_window_uris: Arc::clone(&self.desktop_window_uris),
        }
    }

    /// 停止 MCP 变化通知消费者任务（由 [`shutdown`](Self::shutdown) 调用；`disconnect_socketio` **不**调用——
    /// 断开 socket 后重连仍应继续检测 MCP 变化，故消费者只在整机关停时停）/ stop the consumer (called by shutdown)。
    pub async fn stop_mcp_notify_consumer(&self) {
        if let Some(handle) = self.mcp_notify_task.lock().await.take() {
            handle.abort();
            debug!("MCP change-notification consumer aborted");
        }
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

    /// 启动 user 源 DropIn 文件 watcher（监控 `<home>/user/`）/ start watcher。
    ///
    /// #98：`Computer` 不再持有 workspace，故仅监控 home 级全局 DropIn 根 `<home>/user/`；workdir 范围
    /// SKILL 改由 MCP 服务经 `mcp` 源 + `skill://` 承载（`ResourceListChanged` 自动重挂）。
    ///
    /// watcher 回调在 notify/Poll 观察者的**独立 OS 线程**触发——该线程**无 Tokio 运行时上下文**。
    /// 去抖器契约（见 [`skills::debouncer`](crate::skills) 线程模型）要求跨线程触发**先 marshal 回运行时**
    /// 再 `mark_dirty`（其内部 `tokio::spawn` 须有运行时上下文）；否则在观察者线程上 panic → 毒化去抖器
    /// 状态锁 + 静默断 SKILL 热重载。故经 `Handle::spawn` 把 `mark_dirty` 调度回运行时。已有 watcher → 先停。
    async fn start_skill_watcher(&self) {
        let Some(home) = self.skill_home.read().expect("skill_home poisoned").clone() else {
            return;
        };
        let debouncer = Arc::clone(&self.skill_debouncer);
        // `start_skill_watcher` 是 async → 必在 Tokio 运行时内，`Handle::current()` 恒有效。
        let rt = tokio::runtime::Handle::current();
        let on_change: OnChange = Arc::new(move || {
            let debouncer = Arc::clone(&debouncer);
            // marshal：从观察者线程把 mark_dirty 调度回运行时线程执行（fire-and-forget）。
            rt.spawn(async move { debouncer.mark_dirty() });
        });
        let mut watcher = SkillFileWatcher::builder(on_change)
            .use_polling(self.skill_watch_polling)
            .build();
        let roots: Vec<PathBuf> = vec![user_dropin_root(&home)];
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

    /// 注入 D1 运行期 input resolver（#112 S5）/ Inject the client input resolver。
    ///
    /// server-start 渲染 `${input:<id>}` 时，非密钥 input 优先经此向 client 取值；缺省不注入则回退
    /// env / session / 定义默认值，仍缺 → 结构化 [`ComputerError::InputResolution`]。SDK 不落盘明文值。
    #[must_use]
    pub fn with_input_resolver(mut self, resolver: Arc<dyn InputValueResolver>) -> Self {
        self.input_resolver = Some(resolver);
        self
    }

    /// 注入 D1 运行期 secret resolver（#112 S5）/ Inject the client secret resolver。
    ///
    /// 仅 `password:true` input 走此（如 [`KeyringSecretResolver`](crate::inputs::KeyringSecretResolver)）；SDK 不落盘
    /// secret 明文。缺省不注入则该 secret 走 env / session / 默认值，仍缺 → 结构化 [`ComputerError::InputResolution`]。
    #[must_use]
    pub fn with_secret_resolver(mut self, resolver: Arc<dyn SecretValueResolver>) -> Self {
        self.secret_resolver = Some(resolver);
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
    ///
    /// # Errors
    /// 渲染阶段对已定义但 resolver/env/default 均无法提供的 input/secret 上抛
    /// [`ComputerError::InputResolution`]（#144，对齐 [`Self::mount_server`]，**非仅日志**）；client 据此驱动补录
    /// UI 并在保存后重试。失败落 `Error` 状态、不残留 manager/task/transport，可安全重试。管理器初始化失败（manager 错）。
    pub async fn boot_up(&self) -> ComputerResult<()> {
        info!("Starting Computer: {}", self.name);
        // #140：注册期 env 名坍缩 fail-fast——两 input id 经 ENV_SEGMENT 归一同一完整 env 名（如 `a-b`/`a_b`）
        // 会静默串味（后写的赢，含 `password:true` 密钥）。live 解析只用裸 id ⇒ 检 `self.inputs` 键集即全部活跃
        // env 名（对齐 python `raise_on_env_name_collisions`，接线 server/tool 段时须扩形）。
        {
            let inputs = self.inputs.read().await;
            // 迭代 `values().id()`（value 的权威 id）而非 map key——live resolve 亦用 `input.id()`，二者对齐；
            // 畸形调用方令 key ≠ value.id() 时不致检查落到错误 keyspace。
            smcp::utils::env_segment::raise_on_env_name_collisions(inputs.values().map(|v| v.id()))
                .map_err(|e| ComputerError::InvalidConfiguration(e.to_string()))?;
        }
        // #114 S7：进入 Starting（加载 config / 解析本地状态 / 启动 MCP 资源，契约 §3）。
        self.status.transition(LifecycleState::Starting);

        // 创建MCP服务器管理器 / Create MCP server manager
        let manager = MCPServerManager::new();

        // #106：建 MCP 运行期变化通知 channel，并在**客户端启动前**把 sender 注入 manager（start_client 据此
        // 为每个新客户端携带 ClientNotifyCtx）。stdio/sse/http 三传输的服务器主动通知经此 channel 汇聚。
        let (change_tx, change_rx) = mpsc::unbounded_channel::<McpServerNotification>();
        manager.set_change_sender(change_tx).await;

        // 渲染并验证服务器配置 / Render and validate server configurations
        let servers = self.mcp_servers.read().await;
        let mut validated_servers = Vec::new();

        for (_name, server_config) in servers.iter() {
            match self.render_server_config(server_config).await {
                Ok(validated) => validated_servers.push(validated),
                Err(e) => {
                    // #144：D1 结构化 input 解析错误（Missing/ResolverFailed）须上抛供 client 驱动补录（非仅日志，
                    // 对齐 mount_server）。此处位于 commit/spawn/watcher 之前 ⇒ 无残留 manager/task/transport，boot 可
                    // 安全重试。落 `Error` + last_error（同下方 initialize 失败路径）使观测面反映失败、不卡 `Starting`；
                    // 诊断仅用 error_code + 名（不含渲染细节/secret，契约 §3）。其余渲染错误维持「保留原配置」容错。
                    if matches!(e, ComputerError::InputResolution(_)) {
                        self.status.set_last_error(Some(format!(
                            "boot failed to render server config '{}' (code {})",
                            server_config.name(),
                            e.error_code()
                        )));
                        self.status.transition(LifecycleState::Error);
                        return Err(e);
                    }
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
        // #114 S7：boot 的硬失败点之一（另一为上方 #144 render 阶段的 InputResolution）。失败 → 落 `Error` 状态 +
        // 公开诊断（不含 secret，仅错误类别串），使观测面反映「boot 失败」而非卡在 `Starting`（契约 §3 `error` 语义）。
        // 诊断用 `error_code` + 简述，避免透传可能含渲染细节的 Display 全文。
        if let Err(e) = manager.initialize(validated_servers).await {
            self.status.set_last_error(Some(format!(
                "boot failed to initialize MCP manager (code {})",
                e.error_code()
            )));
            self.status.transition(LifecycleState::Error);
            return Err(e);
        }

        // 设置管理器到实例 / Set manager to instance
        *self.mcp_manager.write().await = Some(manager);

        // #106：起 MCP 变化通知单消费者任务。reactor 持 Weak manager（断 sender 自持环——sender 存于 manager，
        // 若强持 manager 则 rx 永不关闭）+ 强持 socketio/skill/desktop 缓存；逐条 recv → 反应（刷新工具映射 /
        // desktop 集合去抖 / skill 重挂 → 对应 emit）。
        //
        // **停止契约**：`shutdown()` 显式 abort 本任务（`stop_mcp_notify_consumer`），是**确定性**的停止路径。
        // rx 关闭需所有 sender 克隆 drop：stdio/http 客户端的 sender 随 rmcp `RunningService`（客户端 drop）一并
        // 释放；**SSE 客户端的常驻流任务是 detached spawn、持 sender 克隆**，仅在其 SSE 连接结束或 `disconnect`
        // 时才释放。故「drop 而不 shutdown」时本任务可能滞留到 SSE 流结束——推荐经 `shutdown()` 收尾。
        {
            let reactor = self.mcp_change_reactor();
            let mut change_rx = change_rx;
            let handle = tokio::spawn(async move {
                while let Some(notif) = change_rx.recv().await {
                    reactor.handle(notif).await;
                }
                debug!("MCP change-notification consumer exited");
            });
            *self.mcp_notify_task.lock().await = Some(handle);
        }

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

        // SKILL：解析 Home + **治理启动恢复**（#95：从 ledger 重挂已装/启用 marketplace plugin skills，
        // boot 不带 hooks → 仅恢复 skills 派生态；bundled MCP 重挂由 client 显式 reconcile_governance(hooks)
        // 或其自身 MCP 配置物化承担，对齐 #93 client-owns-MCP-config 边界）+ 物化 mcp 源（_restage_mcp_skills，
        // boot 时多为空——server 后续接入）+ 初次全量发现 user 源（= invalidate_user_skills）+ 启 watcher。
        // 各自失败隔离。对标 Python boot_up SKILL 子系统初始化。
        self.ensure_skill_home();
        // v0.3.0 一次性迁移（reconcile 之前）：把存量 v0.2.x「装即活跃」账本迁到 installedPlugins 意图 +
        // enabledPlugins=true，避免升级后「absent = 未启用」使既有 plugin 熄灯。幂等靠意图文件存在性；失败仅 WARN
        // （降级：迁移失败不阻断 boot，下次 boot 重试）。#121：settings 写目标经 **本实例** `config_env` 解析
        // （与紧随其后的 `reconcile_governance` 读**同源**）——注入实例上下文时，enabledPlugins 迁移写**本实例**
        // User settings，**绝不**误写宿主 `~/.config/.../settings.json`（守 #121 不变量：未注入 → None → ambient）。
        if let Err(e) = crate::settings::installer::migrate_ledger_to_intent_once(
            &self.skill_home(),
            self.config_env(),
        ) {
            warn!(error = %e, "governance boot: v0.3.0 ledger→intent migration failed (non-blocking)");
        }
        // #139：丢弃旧格式 `bundledMcpServers`（display-name 数组）——每条发 WARN、不做 name→id 映射；依赖集
        // 由紧随其后的 `reconcile_governance` 从 installedPlugins 意图重建。非幂等风险：无（无旧键 → 0 且不写盘）。
        let discarded = crate::settings::installer::discard_legacy_bundled_mcp_servers(
            &self.skill_home(),
            self.config_env(),
        );
        if discarded > 0 {
            info!(
                records = discarded,
                "governance boot: discarded legacy 'bundledMcpServers' ledger fields (#139); rebuilding from intent"
            );
        }
        let recovery = self.reconcile_governance(None, None).await;
        if !recovery.restored_plugins.is_empty() || !recovery.failed_marketplaces.is_empty() {
            info!(
                restored_plugins = recovery.restored_plugins.len(),
                restored_skills = recovery.restored_skills.len(),
                failed_marketplaces = recovery.failed_marketplaces.len(),
                "governance boot recovery complete"
            );
        }
        self.restage_mcp_skills(None).await;
        self.invalidate_user_skills().await;
        self.start_skill_watcher().await;

        // #114 S7：本地 runtime 已初始化 → 能力投影首次就绪，bump capability revision（能力变化计数，§12 R2）。
        // marketplace 源部分失败 → Degraded + 公开诊断（契约 §3/§5.2「其它 sources 可继续」），否则 Started。
        // boot 成功完成 → 清除上一轮可能残留的 boot `last_error`（重启语义）。
        self.status.bump_capability();
        self.status.set_last_error(None);
        if recovery.failed_marketplaces.is_empty() {
            self.status.set_degraded_reason(None);
            self.status.transition(LifecycleState::Started);
        } else {
            self.status.set_degraded_reason(Some(format!(
                "{} marketplace source(s) failed to sync: {}",
                recovery.failed_marketplaces.len(),
                recovery.failed_marketplaces.join(", ")
            )));
            self.status.transition(LifecycleState::Degraded);
        }

        info!("Computer {} started successfully", self.name);
        Ok(())
    }

    /// D1（#112 S5）运行期解析单个 input：SDK 不落盘明文值/secret，缺失且无默认值 → 结构化错误（**非仅日志**）。
    ///
    /// 解析序：**client resolver**（`secret_resolver` / `input_resolver`，D1 权威源）→ **env** `A2C_SMCP_<ENV_SEGMENT(id)>`
    /// （编排注入）→ **session**（自定义交互 Session 给真值 / `SilentSession` 给 default-or-empty；Command 经此执行）
    /// → **定义默认值** → [`InputResolutionError::Missing`]。仅当既无 resolver/env/session 命中**且**无默认值时硬错
    /// （有默认值仍回退默认，保后向兼容）。value store 明文已硬退役——本路径不落盘任何明文。
    async fn resolve_one_input(&self, input: &MCPServerInput) -> ComputerResult<serde_json::Value> {
        // Command：非交互 subprocess，经 session 执行（无默认值；失败即 Err，不静默）。
        if let MCPServerInput::Command(_) = input {
            return self.session.resolve_input(input).await;
        }

        let is_secret =
            matches!(input, MCPServerInput::PromptString(p) if p.password.unwrap_or(false));
        let kind = if is_secret {
            InputKind::Secret
        } else {
            InputKind::Value
        };

        // 1. client resolver（D1 权威源；keyring 亦作为一种 secret resolver 由 client opt-in 注入）。
        if is_secret {
            if let Some(resolver) = &self.secret_resolver {
                if let Some(secret) = resolver.resolve_secret(input).await? {
                    return Ok(serde_json::Value::String(secret));
                }
            }
        } else if let Some(resolver) = &self.input_resolver {
            if let Some(value) = resolver.resolve_input(input).await? {
                return Ok(value);
            }
        }

        // 2. 环境变量 A2C_SMCP_<ENV_SEGMENT(id)>（编排层注入）。
        let var = env_var_name(input.id());
        if let Ok(env_val) = std::env::var(&var) {
            return Ok(serde_json::Value::String(env_val));
        }
        // #140 迁移诊断（UX-gate）：新名未命中但旧 `A2C_INPUT_<UPPER>` 仍在环境 ⇒ WARN 教改名。
        // **仅检测存在性、绝不读旧值**（F5 无双读、无过渡期，旧名恒被忽略）。
        warn_if_legacy_env_var_present(input.id(), &var);

        // 3. session（自定义交互 Session 可给真值；SilentSession 给 default-or-empty）。
        //    - Ok(空串) 仅在「有默认值」时算有意义（显式空默认），否则视作未命中继续回退——区分「无默认值缺失」与「解析到空」。
        //    - Err（自定义 Session 硬失败，如 GUI 关闭 / IPC 断）：有默认值则回退默认（后向兼容），否则**上抛真实错误**
        //      优于误导性 Missing（SilentSession 永不 Err，故仅影响自定义 Session）。
        match self.session.resolve_input(input).await {
            Ok(value) => {
                let is_empty_string =
                    matches!(&value, serde_json::Value::String(s) if s.is_empty());
                if !is_empty_string || input.default().is_some() {
                    return Ok(value);
                }
            }
            Err(e) => {
                if input.default().is_none() {
                    return Err(e);
                }
            }
        }

        // 4. 定义默认值（后向兼容：有默认值绝不硬错）。
        if let Some(default) = input.default() {
            return Ok(default);
        }

        // 5. 无 resolver / env / session / 默认值 → 结构化缺失错误（非仅日志，绝不静默用空串）。
        Err(ComputerError::InputResolution(
            InputResolutionError::missing(input.id(), kind),
        ))
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

        // 预解析 input 值（D1 #112 S5：client resolver → env → session → 默认值 → 结构化缺失，见
        // [`resolve_one_input`](Self::resolve_one_input)）。**只解析本 server 配置真正引用（`${input:<id>}`）的
        // input**——未被引用的 input 根本不 resolve，从而既不触发其 resolver / keyring / command 副作用，也天然容忍
        // 其缺失（不误伤本次渲染）。resolve 失败先不上抛，仅在渲染真正取用时才 surface 结构化错误。
        // Resolve only inputs actually referenced by this config: no side effects & natural tolerance for the rest.
        let referenced = collect_referenced_input_ids(&config_json);
        let mut resolved_values: std::collections::HashMap<String, serde_json::Value> =
            std::collections::HashMap::new();
        let mut deferred_errors: std::collections::HashMap<String, ComputerError> =
            std::collections::HashMap::new();
        for input_id in &referenced {
            // 未定义的引用（不在 inputs 池）→ 跳过，闭包将回退 `InputNotFound` → 保留占位符原样（VS Code parity）。
            let Some(input) = inputs_clone.get(input_id) else {
                continue;
            };
            match self.resolve_one_input(input).await {
                Ok(value) => {
                    resolved_values.insert(input_id.clone(), value);
                }
                Err(e) => {
                    // 未解析（无默认值的结构化缺失 / resolver 硬失败 / command 执行失败 / session 硬失败）→ 暂存；引用取用时上抛。
                    deferred_errors.insert(input_id.clone(), e);
                }
            }
        }

        // 渲染配置。输入解析闭包：命中 → 值；已定义但未解析 → `InputUnresolved`（向上传播）；未定义 →
        // `InputNotFound`（保留原样）。闭包内联（非具名 local）——其对 `resolved_values`/`deferred_errors` 的不可变
        // 借用随本语句 `;` 结束，故下方 match 可安全 `remove(&id)`。`render_result` 自身不带借用。
        let render_result = renderer
            .render(config_json, |input_id: String| {
                let value = resolved_values.get(&input_id).cloned();
                let is_deferred = deferred_errors.contains_key(&input_id);
                async move {
                    match value {
                        Some(v) => Ok(v),
                        None if is_deferred => Err(RenderError::InputUnresolved(input_id)),
                        None => Err(RenderError::InputNotFound(input_id)),
                    }
                }
            })
            .await;

        // 引用到「已定义但无法解析」的 input → 上抛暂存的结构化错误（非静默空串）；未定义占位符 → 已被 renderer 保留原样。
        // A referenced-but-unresolvable input surfaces its structured error instead of silently defaulting.
        let rendered_json = match render_result {
            Ok(v) => v,
            Err(RenderError::InputUnresolved(id)) => {
                return Err(deferred_errors.remove(&id).unwrap_or_else(|| {
                    ComputerError::InputResolution(InputResolutionError::missing(
                        id,
                        InputKind::Value,
                    ))
                }));
            }
            Err(e) => return Err(e.into()),
        };

        // #68：envFile 合并——把渲染后 envFile 的 `KEY=VALUE` 并入 stdio `server_parameters.env`（显式胜）。
        // envFile merge: fold envFile's KEY=VALUE into stdio env (explicit env wins).
        let rendered_json = apply_env_file(rendered_json);

        // 反序列化回配置类型 / Deserialize back to config type
        let mut rendered_config: MCPServerConfig = serde_json::from_value(rendered_json)?;

        // 协议 0.3.0 §connection-identity = **raw**（a2c-smcp-protocol#17）：bundle_id 缺省生成 MUST 用**未渲染**
        // 连接身份（`${input:*}` 占位按字面）。故从 **raw `config`**（占位仍在）派生并 stamp 到渲染后配置，使
        // manager 不从渲染后连接身份派生——否则无名 server 的引用 input/secret 轮换会漂移 bundle_id / exposed 名。
        // 具名 server 无差（bundle_id = 规范化 name，与 render 无关）；仅无名 fallback 受影响。显式 bundle_id 则保留。
        if rendered_config.bundle_id().is_none() {
            rendered_config.set_bundle_id(Some(crate::mcp_clients::bundle_id::derive_bundle_id(
                config,
            )));
        }

        Ok(rendered_config)
    }

    /// **运行期挂载** MCP server（render + manager + 内存投影 + capability bump + emit），**不落盘** /
    /// mount an MCP server at runtime only (no persistence)。
    ///
    /// #122：这是供 **plugin / 治理 Hook**（[`McpInstallHooks`] 实现，含 **SDK 外部** client，如 tfrobot-client）
    /// 使用的 runtime-only 生命周期通道——把归属 plugin ledger 意图的 bundled server 挂进当前 Computer runtime，
    /// 而**不**写入用户 `mcp.json`。其 durability 由 ledger（`installedPlugins` / `enabledPlugins`）承载、每次 boot 由
    /// `reconcile_governance` 从意图重新派生重挂；经本方法挂载的 server **绝不能**落进 project `mcp.json`（否则形成
    /// 双事实来源：卸载后孤儿化、每次 boot remount 重写用户配置、disable 后跨重启复活并漂移为 `User` ownership）。
    ///
    /// **何时用哪条**（与 [`add_or_update_server`](Self::add_or_update_server) 的区别）：
    /// - **本方法（transient-mount）**：server 的真相在别处（plugin ledger）、只需运行期投影 → 挂进 runtime、**不落盘**、
    ///   只 bump **capability** revision。
    /// - [`add_or_update_server`](Self::add_or_update_server)（**declare-durable**）：**用户显式声明**、真相就是这份声明、
    ///   须重启存活 → **落盘**（#123：新 server 默认落非 git 共享的 local `mcp.local.json`；`add_or_update_server_in_scope`
    ///   可显式选 project/user；bump **config** revision）后再运行期物化。
    ///
    /// - `#106` ABBA：manager 惰性初始化**先 read 探测、仅 None 才升写锁**（governance 路径持 `skill_registry`
    ///   写锁 → hooks → 此方法；post-boot manager 恒 `Some` 只取读锁，与 `McpChangeReactor` 的读锁相容）。
    /// - `§12 R2`：工具投影变化 → bump **capability** revision（**不** bump config——运行期物化不改持久 config）。
    ///
    /// # Preconditions
    /// 本方法**不执行** §10.6 冲突门（install/enable 流程经 [`McpInstallHooks::existing_servers`] 依赖预检属其职责）。
    /// **绕过**标准安装/启用路径**直接**驱动本方法者，须自行确保不与已声明 server 冲突——#127 起运行期投影按
    /// **`bundle_id`** 建键，故**同名不再互相覆盖**（display 名可合法碰撞），但**同 `bundle_id`** 仍会覆盖既有条目
    /// （仅内存、不落盘、重启即复原，非持久边界击穿）；同 `bundle_id` = 同一软件，manager 侧另有 no-double-open
    /// 约束。经 [`McpInstallHooks`] 标准路径挂载（installer 已做冲突门）无此顾虑。
    ///
    /// # Errors
    /// render 校验失败（[`ComputerError::RenderError`] / [`ComputerError::InputResolution`]）；运行期物化失败（manager 错）。
    pub async fn mount_server(&self, server: MCPServerConfig) -> ComputerResult<()> {
        // 渲染并验证配置（**唯一一次** render——resolver 可能有副作用如 keyring/交互取值，禁重复调用）。
        let validated = self.render_server_config(&server).await?;
        self.mount_rendered(server, validated).await
    }

    /// 运行期物化**核心**：入 manager + 内存投影 + capability bump + emit，**不 render、不落盘** / mount core。
    ///
    /// #113 S6：抽出以复用于 [`mount_server`]（治理物化，render 后调）与 [`add_or_update_server`]（用户声明，
    /// 落盘前已 render 一次后调）——二者共用同一次 `render` 结果，**避免重复触发 input/secret resolver 副作用**。
    /// `raw` 存内存投影（保留 `${input:*}` 引用，与落盘一致）、`validated` 入 manager（渲染后运行期用）。
    async fn mount_rendered(
        &self,
        raw: MCPServerConfig,
        validated: MCPServerConfig,
    ) -> ComputerResult<()> {
        // 确保管理器已初始化（read-first 探测，仅 boot 前冷启动升写锁，彼时消费者尚未 spawn，无并发）。
        if self.mcp_manager.read().await.is_none() {
            let mut manager_guard = self.mcp_manager.write().await;
            if manager_guard.is_none() {
                *manager_guard = Some(MCPServerManager::new());
            }
        }

        // 投影键取 `validated` 的身份——`render_server_config` 已从 **raw** 派生并 stamp（#117），故它与
        // manager 的 `resolve_key` 结果**按构造相同**；不在此重派生，避免 raw/rendered 连接身份漂移。
        let bundle_id = crate::mcp_clients::bundle_id::resolve_bundle_id(&validated);

        // 添加到管理器 / Add to manager
        {
            let manager = self.mcp_manager.read().await;
            if let Some(ref manager) = *manager {
                manager.add_or_update_server(validated).await?;
            }
        }

        // 更新本地配置映射（键 = bundle_id，值存原始引用与落盘一致）/ Update local projection, keyed by bundle_id
        {
            let mut servers = self.mcp_servers.write().await;
            servers.insert(bundle_id, raw);
        }

        // 工具投影变化 → capability revision +1（§12 R2）。
        self.status.bump_capability();

        // 如果 Socket.IO 已连接，自动发送配置更新通知 / Auto emit update config if Socket.IO connected
        let _ = self.emit_update_config().await;

        Ok(())
    }

    /// **运行期停摘** MCP server（**bundle_id 寻址**；manager + 内存投影 + capability bump + emit），**不落盘** /
    /// unmount an MCP server at runtime only, by bundle_id (no persistence)。
    ///
    /// #122：[`mount_server`](Self::mount_server) 的对侧——供 **plugin / 治理 Hook**（[`McpInstallHooks`] 的
    /// `remove_server` 实现，含 **SDK 外部** client）按身份停摘运行期实例，**不删** project `mcp.json` 声明
    /// （bundled server 本不在用户 config 层，其增减由 plugin enablement 意图驱动）。**用户删除**声明走
    /// [`remove_server`](Self::remove_server)（落盘删声明后经本运行期臂停摘）。
    ///
    /// - `§12 R2`：工具投影变化 → bump **capability** revision（**不** bump config——运行期停摘不改持久 config）。
    ///
    /// #141/R4：由「name 寻址 `unmount_server` + `pub(crate)` `unmount_server_by_id`」**合并**为单一
    /// bundle_id-addressed 公开 API。旧实现按 name 解析、同名多 server 取**字典序最小**（确定但任意，会误摘）
    /// 且 boot 前回退到本地投影按 name 解析——**两者均已删除**。同名两 server（显式异 bundle_id）从此**精确**停摘。
    ///
    /// 返回**是否真的摘到东西**（`true` = manager 或本地投影确有该身份键并已移除）——未挂载的 bundle_id 是
    /// 幂等 no-op，但如实上报 `false`，供调用方打真实回执（勿一律报成功，见 [`stop_mcp_client`](Self::stop_mcp_client)）。
    ///
    /// # Errors
    /// 运行期停摘失败（manager 错）。
    pub async fn unmount_server(&self, id: &BundleId) -> ComputerResult<bool> {
        let mut removed = false;
        {
            let manager = self.mcp_manager.read().await;
            if let Some(ref manager) = *manager {
                removed |= manager.remove_server_by_id(id).await?;
            }
        }

        // 从本地投影移除（键即 bundle_id，直删）。未挂载键 → 无匹配、no-op。
        {
            let mut servers = self.mcp_servers.write().await;
            removed |= servers.remove(id).is_some();
        }

        // 仅**真摘到**才是工具投影变化 → 才 bump capability + 通知（§12 R2；no-op 不是能力变化）。
        if removed {
            self.status.bump_capability();
            let _ = self.emit_update_config().await;
        }

        Ok(removed)
    }

    /// 动态添加或更新服务器配置（**落盘 + 运行期物化**）/ Add or update a server config (persist + mount)。
    ///
    /// #113 S6（补 #96 洞）：用户经此声明的 server 现**落盘**（重启不丢），再运行期物化。
    /// - **新 server 默认落 `local` scope**（`mcp.local.json`，**不入 git**；#123 / 协议#19 加固）——避免 API/UI
    ///   加的 server 静默污染团队共享的 `mcp.json`；local 仍 boot 读取→**重启存活**（不损失 #113 收益）。想入 git
    ///   团队共享 → 用 [`add_or_update_server_in_scope`](Self::add_or_update_server_in_scope) 显式选 `Project`/`User`。
    /// - **D1 安全**：落盘的是**原始** `server`（保留 `${input:*}`/`${env:*}` 引用），**绝不**落渲染后的明文值/secret。
    /// - **改已有 server** → 恒落其 **origin scope**（`upsert_new_scope` 只作用于新声明）。
    /// - **#131 F3(a)：撞 plugin 基线不拒写**。用户声明与某启用中插件的 bundled server 同 `bundle_id` → **照常
    ///   写入**并覆盖之。协议 `runtime-contract.md` §2.5 定 `plugin 声明 < user < project < local < flag < policy`，
    ///   plugin 声明是**最低基线层**、被任何用户侧 scope 覆盖（用户主权）；`guides/mcp-approval-gate-alignment.md`
    ///   §5 明定 upsert **MUST NOT** 因「同 bundle_id 已由 plugin 提供」拒写。此前 #126 的 `Synthesized` 拒写门控
    ///   据此**移除**。（`remove_server` 侧门控仍在——有意非对称，其 origin 判据改造属 F3(b) / #138。）
    /// - **§12 R2**：落盘成功后 bump **config** revision；随后运行期物化 bump **capability**。
    /// - 治理物化（bundled 重挂）**不**走此路径（走 [`Self::mount_server`]），避免 ledger 意图重复写入 mcp.json。
    ///
    /// # Errors
    /// render 校验失败（[`ComputerError::RenderError`] / [`ComputerError::InputResolution`]）；落盘失败
    /// （[`ComputerError::ConfigPersist`]，含只读 origin / I/O）；运行期物化失败（manager 错）。
    pub async fn add_or_update_server(&self, server: MCPServerConfig) -> ComputerResult<()> {
        // #123（协议#19 加固）：默认 `Local`（不入 git、机器本地、重启存活）。
        self.add_or_update_server_in_scope(server, WriteScope::Local)
            .await
    }

    /// 同 [`Self::add_or_update_server`]，但**显式指定新 server 的落盘 scope**（opt-in 团队共享 `Project` / 用户全局 `User`）。
    ///
    /// #123（协议#19 加固）：`upsert_new_scope` **只作用于新声明**——更新已有 server 恒落其 origin scope，与本参数无关。
    /// `Local` = `<cwd>/.tfrobot/mcp.local.json`（不入 git）；`Project` = `<cwd>/.tfrobot/mcp.json`（入 git、团队共享）；
    /// `User` = `~/.config/a2c/mcp.json`（用户全局）。
    ///
    /// # Errors
    /// 同 [`Self::add_or_update_server`]。
    pub async fn add_or_update_server_in_scope(
        &self,
        server: MCPServerConfig,
        upsert_new_scope: WriteScope,
    ) -> ComputerResult<()> {
        // #121：以本实例上下文（含 User env + Skill Home）解析写目标，绝不误写宿主 User config。
        let config_dir = self.config_dir();
        let home = self.skill_home();
        let name = server.name().to_string();
        let ctx = self.instance_config_context(&config_dir, &home, upsert_new_scope);
        let snapshot = load_config(&ctx);

        // #131 F3(a)：**不设**「同 bundle_id 已由 plugin 提供 → 拒写」门控。协议
        // `guides/mcp-approval-gate-alignment.md` §5 + `runtime-contract.md` §2.5：用户在自己的 scope 声明同
        // `bundle_id` **正是来源优先序赋予的覆盖权**（`plugin 声明 < user < project < local < flag < policy`，
        // plugin 声明是**最低基线层**）——upsert **MUST NOT** 因此拒写。#126 引入的 `Synthesized` 拒写短路据此
        // 移除（提示「你的声明将覆盖 plugin 基线」为 MAY，本轮不做）。
        //
        // 注：`remove_server` 的归属门控仍在（有意非对称）——删除面的 origin 判据改造属 F3(b)，见 #138。

        // 先 render 校验：非法 config / 无法解析的 input 早失败，**不落盘**（**唯一一次** render，下方物化复用其结果，
        // 避免重复触发 resolver 副作用）/ validate before persist; single render reused by mount_rendered below。
        let validated = self.render_server_config(&server).await?;

        // 落盘（原始引用；D1 不落 secret）→ 经 S2 消解器定 scope + S3 执行器两阶段写。
        let body = canonicalize_persist_body(serde_json::to_value(&server)?);
        // 内容摘要 revision（S1）：仅当真落盘（内容变）才 bump config，避免 no-op/幂等 mutate 虚假 bump（§12 R2）。
        let before_rev = snapshot.revision;
        let edit = ConfigEdit::new(ConfigEntity::McpServer(name), EditIntent::Upsert(body));
        let after = update_config(&ctx, std::slice::from_ref(&edit))
            .map_err(|e| ComputerError::ConfigPersist(e.to_string()))?;

        // 落盘且内容真变 → config revision +1（§12 R2；capability 于 mount_rendered bump）。
        if after.revision != before_rev {
            self.bump_config_revision();
        }

        // 运行期物化（复用上面已 render 的 validated，不重复 render）+ 内存投影 + capability bump + emit。
        self.mount_rendered(server, validated).await
    }

    /// #151 Part 2：typed MCP **零写 preflight**——对一批 server 做确定性 + 引用语法可达校验，预测落盘清单。
    ///
    /// 不取真实值、不写盘（守 #107：不接管 client-owned inputs/profile/secrets）。供下游在事务提交前暴露只读来源
    /// / 损坏目标 / schema / `${input:}` 不可达等确定性错误。新声明默认落 `Local`（与 [`Self::add_or_update_server`] 一致）。
    pub fn preflight_mcp_servers(
        &self,
        servers: &[MCPServerConfig],
    ) -> ComputerResult<PreflightReport> {
        let config_dir = self.config_dir();
        let home = self.skill_home();
        let ctx = self.instance_config_context(&config_dir, &home, WriteScope::Local);
        Ok(preflight_mcp_import(&ctx, servers))
    }

    /// #151 Part 2：typed MCP **import（全有或全无）**——preflight 干净后两阶段原子落盘。
    ///
    /// SDK 决定序列化（canonicalize）/ provenance / write-target；任一实体确定性失败 → 整批零写
    /// （`ImportError::Preflight`）。**不 mount / 不 render 取值**（运行期物化归 [`Self::mount_server`] /
    /// [`Self::add_or_update_server`]）。新声明默认落 `Local`。
    pub fn import_mcp_servers(
        &self,
        servers: &[MCPServerConfig],
    ) -> ComputerResult<Vec<PlannedServer>> {
        let config_dir = self.config_dir();
        let home = self.skill_home();
        let ctx = self.instance_config_context(&config_dir, &home, WriteScope::Local);
        import_mcp_servers_cfg(&ctx, servers)
            .map_err(|e| ComputerError::ConfigPersist(e.to_string()))
    }

    /// 移除服务器配置（**bundle_id 寻址**；落盘删声明 + 运行期停摘）/ Remove a server config by bundle_id (persist + unmount)。
    ///
    /// #121（B）：**按 `bundle_id`（软件唯一身份）寻址**，对齐协议 §身份「MUST 用 bundle_id、MUST NOT 用 name」
    /// 与 Python `aremove_server(bundle_id)`。此前按 `name` 寻址经 manager 的 `bundle_id_for_name` 桥，在同名 +
    /// 不同显式 `bundle_id` 时**跨运行非确定**（协议告警场景）；改直接身份寻址后消除该歧义。
    ///
    /// - **落盘删声明**：从**本实例** config 快照（#121 A：`config_env` 锚定，不误读宿主）解析 `bundle_id` → 声明
    ///   `name`（`resolve_bundle_id` 与 manager 同键，含显式 `bundle_id`），再按 name 删**所有可写 scope**（mcp.json
    ///   是 name-keyed；S2 R1 真删干净）。匹配多个声明名（no-double-open 冲突）→ 全删。
    /// - 不在任何 config scope 且**非**启用插件占用（纯运行期实例）→ 落盘 no-op；随后仍按 `bundle_id` 停摘运行期实例。
    /// - **#126 归属门控**：无匹配用户声明、但该 `bundle_id` 属**启用中**插件的 bundled server → 拒
    ///   （[`WriteTargetError::Synthesized`]，用户应停用/卸载 plugin）。用户**自己声明的**同名 server 可正常删除
    ///   （归属 enabled-gated，与 managedBy 查询同源；停用该 plugin 后该名亦可自由删除）。
    ///
    /// # Errors
    /// 落盘失败（[`ComputerError::ConfigPersist`]，含只读 origin / synthesized / I/O）；运行期停摘失败（manager 错）。
    pub async fn remove_server(&self, bundle_id: &BundleId) -> ComputerResult<bool> {
        // #121 A：以本实例上下文（含 User env + Skill Home）解析 config，绝不误读/误删宿主 User config。
        // remove 不 upsert（删所有可写 scope，S2 R1），`upsert_new_scope` 对其无影响，占位传 `Local`。
        let config_dir = self.config_dir();
        let home = self.skill_home();
        let ctx = self.instance_config_context(&config_dir, &home, WriteScope::Local);

        // bundle_id → **用户声明名**（去重）。快照 `McpServerView.config` 为 raw，`resolve_bundle_id` 与 manager
        // 注册期同键。**F3(b) origin 判据**（#138，与 A4 plugin 投影耦合）：`origin == Plugin` 的条目是 plugin
        // 基线的**读侧投影**、非用户声明（runtime-only、不落 mcp.json、不可 Remove）——MUST 排除，否则「用户无自有
        // 声明、bundle_id 属启用插件」会误判为「有声明」而绕过归属门（回归 #131/#126）。
        let snap = load_config(&ctx);
        let mut seen = HashSet::new();
        let names: Vec<String> = snap
            .mcp
            .servers
            .iter()
            .filter(|v| v.origin != ProvenanceScope::Plugin)
            .filter(|v| crate::mcp_clients::bundle_id::resolve_bundle_id(&v.config) == *bundle_id)
            .map(|v| v.name.clone())
            .filter(|n| seen.insert(n.clone()))
            .collect();
        // names 已 collect（拥有所有权），此后移出 revision 不影响；snap 余部随作用域自然析构。
        let before_rev = snap.revision;

        // #126 归属门控：无匹配用户声明、但该 `bundle_id` 属**启用中**插件的 bundled server → 拒绝直接删除
        // （用户应停用/卸载 plugin，不经 config 删 plugin server）。有用户声明（names 非空）则删其声明、放行。
        // 归属集与 `list_mcp_servers_with_metadata`（managedBy）**同源**（#126 验收#3）；停用插件后不占名（D3）。
        if names.is_empty() {
            if let Some(rec) = self.enabled_bundled_ownership().into_iter().find(|rec| {
                crate::mcp_clients::bundle_id::resolve_bundle_id(&rec.config) == *bundle_id
            }) {
                return Err(ComputerError::ConfigPersist(
                    WriteTargetError::Synthesized {
                        entity: format!("mcp:{}", rec.config.name()),
                    }
                    .to_string(),
                ));
            }
        }

        // 落盘删声明（每个匹配名一条 Remove）。无匹配（纯运行期实例、非 plugin-owned）→ 空计划、不落盘。
        //
        // 🔴 回执信号取 **revision 真变**、而非 `!names.is_empty()`：`names` 来自合并快照（只滤掉
        // `origin==Plugin`），`origin==Embed`（#147 宿主构造入参投影）**不落盘**⇒ names 非空但 `update_config`
        // 无事可做，据 names 打「已移除」会谎报，而该声明下次 boot 原样回来。
        let mut removed_declaration = false;
        if !names.is_empty() {
            let edits: Vec<ConfigEdit> = names
                .into_iter()
                .map(|n| ConfigEdit::new(ConfigEntity::McpServer(n), EditIntent::Remove))
                .collect();
            let after = update_config(&ctx, &edits)
                .map_err(|e| ComputerError::ConfigPersist(e.to_string()))?;
            removed_declaration = after.revision != before_rev;
            // 落盘且内容真变 → config revision +1（§12 R2）。
            if removed_declaration {
                self.bump_config_revision();
            }
        }

        // 运行期停摘（按 bundle_id；manager + 内存投影 + capability bump + emit）。
        let unmounted = self.unmount_server(bundle_id).await?;
        // #141：如实上报「有没有删到东西」——既无声明也无活跃实例时返回 `false`，供 CLI 打真实回执
        // （否则拼错的 target 恰是合法 bundle_id 字面量时会谎报「已移除」）。
        Ok(removed_declaration || unmounted)
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
    ///
    /// 元组 = `(bundle_id, server_name, resource, read_result)`：`bundle_id` = desktop 分组键（协议 0.3.0 #18），
    /// `server_name` = 展示名。
    pub async fn get_windows_details(
        &self,
        window_uri: Option<&str>,
    ) -> ComputerResult<Vec<(BundleId, ServerName, Resource, ReadResourceResult)>> {
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
        id: &BundleId,
        resource: Resource,
    ) -> ComputerResult<ReadResourceResult> {
        let manager = self.mcp_manager.read().await;
        if let Some(ref manager) = *manager {
            manager.get_window_detail(id, resource).await
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
            // 验证工具调用（协议 0.3.0：入参为 exposed_tool_name，返回 bundle_id + 展示名 + 原始工具名）。
            let (bundle_id, server_name, tool_name) =
                manager.validate_tool_call(tool_name, &parameters).await?;
            let server_name = server_name.to_string(); // 人类可读名，供确认回调/历史记录
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
                                bundle_id.as_str(),
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
                        bundle_id.as_str(),
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
    ///   RAII `InflightCancelGuard` 注销注册表，**绝不**被误判为取消态（tokio drop 语义天然满足）。
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

        // 校验并解析真实 bundle_id/server/tool（协议 0.3.0：入参为 exposed_tool_name）。
        let (bundle_id, server_name, resolved_tool) =
            manager.validate_tool_call(tool_name, &parameters).await?;
        let server_name = server_name.to_string(); // 人类可读名，供历史记录
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
                bundle_id.as_str(),
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
    /// Python `acancel_tool`——完成即由 `InflightCancelGuard` 注销注册表，再次取消落空回 `false`）。
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

    /// 获取服务器状态列表 `(bundle_id, name, is_active, state)` / Get server status list。
    ///
    /// 每行**自带身份键** `bundle_id`（#127）——`remove_server` 按 bundle_id 寻址（协议 §身份：`name` 非
    /// 身份键），故 client / CLI 直接从本列表拿到寻址键，**无需**再按 name 去 join 另一张映射（旧的
    /// `materialized_server_bundle_ids` 即为此 join 而设，其 name-keyed map 在同名场景下会折叠身份，
    /// 令 CLI 给两行打印同一个 bundle_id → `server rm` 删错对象；该 API 随本次修复移除）。
    pub async fn get_server_status(&self) -> Vec<(BundleId, ServerName, bool, String)> {
        let manager_guard = self.mcp_manager.read().await;
        if let Some(ref manager) = *manager_guard {
            manager.get_server_status().await
        } else {
            Vec::new()
        }
    }

    // ── #114 S7：runtime status / observability 公开面 / runtime status surface ──────────

    /// runtime 状态快照（#114 S7）：生命周期 + 分离单调 revision + 能力汇总 + 公开诊断 / runtime status snapshot。
    ///
    /// **cheap、非阻塞**：状态 / revision / 诊断取自 [`RuntimeStatus`]（原子无锁），汇总计数为当次对内存态的只读
    /// 投影（MCP 声明集 / 活跃集 / **已注册工具映射** / 活跃 SKILL 集）——**不做 ledger / 磁盘 IO / MCP RPC**。
    /// 工具数取 [`MCPServerManager::tool_count`] 的**已缓存映射长度**（非 `list_available_tools` 的逐 server
    /// `tools/list` 往返），避免观测端点自身阻塞于不健康的 MCP server。plugin / marketplace 明细留给专用 inventory
    /// API（[`list_mcp_servers_with_metadata`](Self::list_mcp_servers_with_metadata)）。满足契约 §3「暴露生命周期状态
    /// 或等价公开诊断」，且反映**已加载的 desired state**（未 boot 时 manager=None → 活跃/工具计数为 0，`mcp_servers`
    /// 仍反映已声明集）。
    ///
    /// 锁纪律：三把读锁**逐次取、至多同时持一把**（`mcp_servers` → `mcp_manager` → `skill_registry`，各在语句/块
    /// 结束即释放），故不参与 #106 的 mcp/skill ABBA 环。
    pub async fn status(&self) -> ComputerStatusSnapshot {
        let mcp_servers = self.mcp_servers.read().await.len();
        let (active_mcp_servers, tools) = {
            let manager_guard = self.mcp_manager.read().await;
            if let Some(ref manager) = *manager_guard {
                let active = manager
                    .get_server_status()
                    .await
                    .into_iter()
                    .filter(|(_, _, active, _)| *active)
                    .count();
                // 廉价：读已缓存 tool_mapping 长度，不发 tools/list RPC（🟡 修复：status 不因 MCP server 挂起而阻塞）。
                let tools = manager.tool_count().await;
                (active, tools)
            } else {
                (0, 0)
            }
        };
        let skills = self.skill_registry.read().await.active_refs().len();
        self.status
            .snapshot(mcp_servers, active_mcp_servers, tools, skills)
    }

    /// 订阅 runtime 观测事件流（#114 S7）/ subscribe to runtime observability events。
    ///
    /// 返回 [`tokio::sync::broadcast::Receiver`]：生命周期迁移 / revision 增长逐条广播。**shutdown 后**（契约
    /// §4.7）除进入 shutdown 时的终态 [`ComputerEvent::LifecycleChanged`]`(shutdown)` 外不再收到新事件。滞后订阅者
    /// 会收到 `Lagged`——可经 [`status`](Self::status) 重新拉取全量快照对齐。
    pub fn subscribe_events(&self) -> broadcast::Receiver<ComputerEvent> {
        self.status.subscribe()
    }

    /// 当前 config revision（声明式配置内容单调计数；S6 mutate 落盘时 bump）/ current config revision。
    #[must_use]
    pub fn config_revision(&self) -> u64 {
        self.status.config_revision()
    }

    /// 当前 capability revision（Agent-facing 能力投影单调计数）/ current capability revision。
    #[must_use]
    pub fn capability_revision(&self) -> u64 {
        self.status.capability_revision()
    }

    /// 当前生命周期状态 / current lifecycle state。
    #[must_use]
    pub fn lifecycle_state(&self) -> LifecycleState {
        self.status.state()
    }

    /// bump config revision（#113 S6 mutate 落盘接线入口；单调 +1 并广播）/ bump config revision (S6 entry)。
    ///
    /// config revision 的 mutate-bump 由 S6 在写目标**落盘成功且内容真变**后调用（config ⊥ capability 分离，
    /// 设计 §12 R2）。生产调用点：[`add_or_update_server`](Self::add_or_update_server) /
    /// [`remove_server`](Self::remove_server) / [`enable_plugin`](Self::enable_plugin) /
    /// [`disable_plugin`](Self::disable_plugin)。
    pub(crate) fn bump_config_revision(&self) -> u64 {
        self.status.bump_config()
    }

    /// 列出 MCP 服务器配置 / List MCP server configurations
    pub async fn list_mcp_servers(&self) -> Vec<MCPServerConfig> {
        let servers = self.mcp_servers.read().await;
        servers.values().cloned().collect()
    }

    /// 本实例 enabled-bundled 归属集（intent ∧ `enabledPlugins==true` 门控）/ enabled bundled ownership set。
    ///
    /// [`list_mcp_servers_with_metadata`](Self::list_mcp_servers_with_metadata) 的 `managedBy=plugin` 与 runtime
    /// CRUD 归属门控（#126：[`add_or_update_server`](Self::add_or_update_server) /
    /// [`remove_server`](Self::remove_server)）的**唯一同源**——保证「查询归属」与「增删可否」一致（#126 验收#3）。
    /// #121 A：以**本实例** env（`config_env`）解析账本 + `enabledPlugins`，绝不混入宿主 ambient settings。**停用的
    /// plugin 不在结果内**（#126 D3：停用插件后同名 server 可自由增删——issue 记录的规避路径）。
    ///
    /// **join key = `bundle_id`（#127 起对称）**：`add_or_update_server` / `remove_server` /
    /// `list_mcp_servers_with_metadata` 三者**一律**按软件唯一身份 `bundle_id` 判占用。此前 add 按 name、
    /// remove 按 bundle_id（#126 记为「有意的非对称」），在「用户声明与某启用插件 bundled server 同名但显式设了
    /// 不同 `bundle_id`」时自相矛盾：查询标 `managedBy=plugin`（只读）、`remove_server` 却放行。协议 §身份正交性
    /// 判定该场景下二者是**不同软件**（`name` 允许碰撞、非身份），故统一按 `bundle_id` 判——查询与增删同时正确。
    ///
    /// ## 已知限制：`declared_origin` 命中即跳过门控（非本次引入）
    ///
    /// `add_or_update_server` 的门控前置条件是「用户在 config 中**无同名声明**」（`declared_origin` 按 **name** 查
    /// ——那是 config-file 域，`mcp.json` 按协议 §9.1 合法 name-keyed）。故用户**已拥有**名为 `N` 的声明时门控整体
    /// 跳过，此时可把该声明的 `bundle_id` 改写成某启用插件 bundled server 的身份。该缝隙**旧实现同样放行**（非本次
    /// 回归），且 bundled 配置 runtime-only、不落 `mcp.json`，冲突止于 manager 注册期的 no-double-open first-wins +
    /// 本地配置诊断（协议 §config-diagnostics），不击穿持久边界。彻底封堵需让门控同时看「声明的身份变更」，属独立议题。
    fn enabled_bundled_ownership(&self) -> Vec<BundledServerRecord> {
        let home = self.skill_home();
        let env = self.config_env();
        let policy = resolve_policy_settings(None, None, None);
        let declared = resolve_settings(ResolveSettingsArgs {
            cwd: None,
            env,
            flag_settings_path: None,
            policy_settings: Some(&policy),
        })
        .settings;
        crate::settings::recovery::collect_enabled_bundled_servers(&home, env, &declared)
    }

    /// 列出 MCP 服务器 + 归属 / 生命周期元数据（活跃 inventory）/ List MCP servers with ownership metadata.
    ///
    /// 面向 client（如 `tfrobot-client`）Skill / MCP tab：一次拿到「当前 Computer 有哪些 MCP server + 每条归
    /// 谁（user vs plugin，含 marketplace / plugin / pluginId）+ 能否从普通 MCP tab 编辑 / 启停」，**无需**读
    /// SDK ledger、**无需**解析 plugin manifest、**无需**持内存 ownership map。协议依据 a2c-smcp-protocol
    /// v0.2.3 §4.8（归属 = boot 纯函数、每次可复现；enabled bundled server 进程未拉起也须可查询）。元数据类型
    /// 见 [`crate::inventory`]，**SDK-facing、不进** Agent-facing `client:*` wire。
    ///
    /// 合并两个来源（去重按 **`bundle_id`**，运行期条目优先）：
    /// 1. 运行期已物化集 `self.mcp_servers`——用户配置 server，或 client 经 `reconcile_governance(hooks)` 物化
    ///    的 plugin bundled server；`bundle_id` 命中 ledger 派生 bundled 集 → `managedBy=plugin`，否则 `user`。
    /// 2. ledger 派生的**已启用但尚未物化**的 plugin bundled server（boot `hooks=None` 后即此态）——补入
    ///    inventory 并标 `managedBy=plugin`，满足 §4.8「进程未拉起也可观测」（客户端据此物化或引导 Marketplace）。
    ///
    /// 结果按 `(name, bundle_id)` 双键排序（`self.mcp_servers` 为 `HashMap` 且 `name` 现可合法碰撞，故须以身份键
    /// 兜底 tiebreak 才有稳定可测输出）。**不**含运行期「进程是否
    /// 已启动」状态——那由 [`get_server_status`](Self::get_server_status) 单独提供。
    ///
    /// ## 归属 join key = `bundle_id`（#127）/ ownership join key = bundle_id
    ///
    /// 归属以**软件唯一身份 `bundle_id`** 为 join key（协议 0.3.0 §身份正交性：`name` 是纯 display、允许碰撞、
    /// **永不做键/寻址**）。故两个 display 名相同、`bundle_id` 不同的 server **各自保留身份**：用户声明的那条标
    /// `user`（可从 MCP tab 编辑），插件的那条标 `plugin`（只读）——此前的 name-join 会把用户自己的声明误标
    /// `plugin`（假阳性、只读），且两个 plugin 的同名 bundled server 经首见去重后者身份不出现。
    ///
    /// 与 [`add_or_update_server`](Self::add_or_update_server) / [`remove_server`](Self::remove_server) 的门控
    /// **同源同键**（#126 验收#3）。**可靠的同名冲突拦截仍是安装期职责**（[`install_plugin`](Self::install_plugin)
    /// 经 hooks `existing_server_names` 的冲突门）——#96 pt5「同名返回明确错误」属安装期契约、不在本只读投影范围；
    /// 调用方若需强冲突保证，应经带 hooks 的安装路径拦截，而非依赖本查询。
    pub async fn list_mcp_servers_with_metadata(&self) -> Vec<McpServerWithMetadata> {
        // 由 [`BundledServerRecord`] 纯函数派生 `managedBy=plugin`（§4.8.3）。
        let plugin_ownership = |rec: &BundledServerRecord| McpOwnership::Plugin {
            marketplace: rec.marketplace.clone(),
            plugin: rec.plugin.clone(),
            plugin_id: rec.plugin_id.clone(),
        };

        // ledger 派生的已启用 bundled server（归属纯函数，与 reconcile_governance 同解析视图）。
        // #127：按 **bundle_id** 建索引——归属 join 的键即身份键，与 add/remove 门控同源同键。
        let bundled: HashMap<BundleId, BundledServerRecord> = self
            .enabled_bundled_ownership()
            .into_iter()
            .map(|rec| {
                (
                    crate::mcp_clients::bundle_id::resolve_bundle_id(&rec.config),
                    rec,
                )
            })
            .collect();

        let mut out: Vec<McpServerWithMetadata> = Vec::new();
        let mut materialized: HashSet<BundleId> = HashSet::new();

        // 来源一：运行期已物化 server。`bundle_id` 命中 ledger bundled 集 → plugin，否则 user。
        {
            let servers = self.mcp_servers.read().await;
            for (bundle_id, cfg) in servers.iter() {
                materialized.insert(bundle_id.clone());
                let managed_by = match bundled.get(bundle_id) {
                    Some(rec) => plugin_ownership(rec),
                    None => McpOwnership::User,
                };
                out.push(McpServerWithMetadata::new(
                    cfg.name().to_string(),
                    bundle_id.clone(),
                    cfg.disabled(),
                    managed_by,
                ));
            }
        }

        // 来源二：已启用但尚未物化的 bundled server（不在运行期集 → 补入，标 plugin；§4.8 可观测）。
        for (bundle_id, rec) in &bundled {
            if !materialized.contains(bundle_id) {
                out.push(McpServerWithMetadata::new(
                    rec.config.name().to_string(),
                    bundle_id.clone(),
                    rec.config.disabled(),
                    plugin_ownership(rec),
                ));
            }
        }

        // 按 (name, bundle_id) 排序：`HashMap` 迭代序不定，且 `name` 现可合法碰撞（#127）——须以身份键
        // 兜底 tiebreak，否则同名两条的相对次序不确定、输出不可测。
        out.sort_by(|a, b| {
            a.name
                .cmp(&b.name)
                .then_with(|| a.bundle_id.cmp(&b.bundle_id))
        });
        out
    }

    // ── #124：高层 governance snapshot/inventory（SDK-facing，只读）─────────────────
    /// 采集轻量 live 叠加（bundled skills / 已物化 server），供治理快照富化——**不入 revision**。
    ///
    /// bundled skills 从活跃 SKILL registry 按 plugin_id 分组（`source == "marketplace:<mp>"` +
    /// name `"<plugin>:<skill>"` → `"<plugin>@<mp>"`）；materialized 取当前运行期已物化 server 名集。
    /// 均为已缓存读、非阻塞（不发 MCP RPC）。
    async fn governance_overlay(&self) -> GovernanceRuntimeOverlay {
        let mut bundled_skills_by_plugin: std::collections::BTreeMap<String, Vec<String>> =
            std::collections::BTreeMap::new();
        for r in self.skill_registry.read().await.active_refs() {
            if let Some(mp) = r.source.strip_prefix("marketplace:") {
                if let Some((plugin, _)) = r.name.split_once(':') {
                    bundled_skills_by_plugin
                        .entry(format!("{plugin}@{mp}"))
                        .or_default()
                        .push(r.name.clone());
                }
            }
        }
        for v in bundled_skills_by_plugin.values_mut() {
            v.sort();
        }
        // #139：取 **bundle_id**（身份键）——ledger 已迁 bundle_id 数组（`mcpServers`），本集合唯一消费者是
        // 与之求交集（`governance::resolve_governance_snapshot`），两侧须同 bundle_id 域。`self.mcp_servers`
        // 已按 bundle_id 建键，直接取 keys。仅治理展示、不做寻址。
        let materialized_mcp_servers: std::collections::BTreeSet<String> = self
            .mcp_servers
            .read()
            .await
            .keys()
            .map(|b| b.as_str().to_string())
            .collect();
        GovernanceRuntimeOverlay {
            bundled_skills_by_plugin,
            materialized_mcp_servers,
        }
    }

    /// 统一治理快照（Marketplace/plugin 完整列表 + 详情）/ unified governance snapshot（#124）。
    ///
    /// 面向集成 client（GUI/Tauri）：**只经本 `Computer` + [`crate::governance`] DTO** 即可查询治理状态。
    /// 以本实例注入的 `skill_home` / `config_env` / config directory 解析，**绝不回退宿主 env/home**。只读、
    /// 非阻塞、不隐式 clone/refresh。`installedPlugins` 意图为安装权威；单项损坏 → `Degraded` + diagnostic，
    /// 不吞成空。`list_*` / `get_*` 均由本快照派生，故共享同一状态语义与 `revision`。
    pub async fn governance_snapshot(&self) -> Result<GovernanceSnapshot, GovernanceQueryError> {
        let home = self.skill_home();
        let config_dir = self.config_dir();
        let overlay = self.governance_overlay().await;
        Ok(resolve_governance_snapshot(
            GovernanceArgs {
                cwd: Some(&config_dir),
                env: self.config_env(),
                home: Some(&home),
                ..Default::default()
            },
            &overlay,
        ))
    }

    /// 列出全部已知 marketplace（含详情）/ list marketplaces（#124）。
    pub async fn list_marketplaces(
        &self,
    ) -> Result<Vec<MarketplaceSnapshot>, GovernanceQueryError> {
        Ok(self.governance_snapshot().await?.marketplaces)
    }

    /// 按名取单个 marketplace（未知 → `None`）/ get one marketplace（#124）。
    pub async fn get_marketplace(
        &self,
        name: &str,
    ) -> Result<Option<MarketplaceSnapshot>, GovernanceQueryError> {
        Ok(self
            .governance_snapshot()
            .await?
            .marketplaces
            .into_iter()
            .find(|m| m.name == name))
    }

    /// 列出 plugin（按 [`ListPluginsOptions`] 过滤）/ list plugins（#124）。
    ///
    /// 默认仅返回已安装项；`include_available=true` 追加 catalog 可用（未安装）项；`marketplace` 限定归属。
    pub async fn list_plugins(
        &self,
        options: ListPluginsOptions,
    ) -> Result<Vec<PluginSnapshot>, GovernanceQueryError> {
        let plugins = self.governance_snapshot().await?.plugins;
        Ok(plugins
            .into_iter()
            .filter(|p| options.include_available || p.status != PluginStatus::Available)
            .filter(|p| {
                options
                    .marketplace
                    .as_deref()
                    .is_none_or(|m| p.marketplace == m)
            })
            .collect())
    }

    /// 按 id（`<plugin>@<marketplace>`）取单个 plugin（未知 → `None`）/ get one plugin（#124）。
    pub async fn get_plugin(
        &self,
        plugin_id: &str,
    ) -> Result<Option<PluginSnapshot>, GovernanceQueryError> {
        Ok(self
            .governance_snapshot()
            .await?
            .plugins
            .into_iter()
            .find(|p| p.id == plugin_id))
    }

    /// 启动 MCP 客户端 / Start MCP client
    pub async fn start_mcp_client(&self, id: &BundleId) -> ComputerResult<()> {
        let result = {
            let mgr = self.mcp_manager.read().await;
            match mgr.as_ref() {
                Some(m) => m.start_client_by_id(id).await,
                None => Err(Self::manager_uninit()),
            }
        };
        // #148：成功后 bump capability + joined 时自动广播 server:update_tool_list
        // （manager 层 start_client_by_id 已 refresh_tool_routes，本地 tool mapping 已最新）。
        //
        // 注：start 类（含 restart/all）在**任何** Ok 都广播——与 stop 的 `Ok(true)` gate 不对称：manager 层
        // `start_client_by_id` 对「已启动再 start」早返 Ok(())（manager.rs 无 bool 返回），故「无实际变更」的
        // 幂等 start 也会触发一次广播。这与既有 capability_revision 在同路径的 spurious bump 同形；Agent 侧
        // 回拉幂等（一次额外 tools/list，结果不变），无正确性影响。精确化（manager 返回 bool）为后续 follow-up。
        if result.is_ok() {
            self.on_capability_changed().await;
        }
        result
    }

    /// 停止单个 MCP 客户端（**bundle_id 寻址**）；返回**是否真的停了** / Stop by bundle_id; returns whether it stopped。
    ///
    /// #141/R4：由 name 寻址改 `&BundleId`——同名两 server 精确起停。
    ///
    /// 🔴 **根治 `stop` 假回执**：旧 `manager.stop_client(name)` 名解析未命中即 `refresh + Ok(())` 谎报成功。
    /// 仅把寻址改成身份键**不足以**根治——CLI 的 `resolve_target` 按 R4 必须放行「0 命中但语法合法的 bundle_id」
    /// （拼错的 server 名几乎总是合法字面量），那条 token 会一路走到这里、对缺席键幂等返回。故本方法**如实上报**
    /// `Ok(true)=确有活跃客户端被停` / `Ok(false)=无活跃客户端、未做任何事`，由调用方（CLI）据此打真实回执。
    pub async fn stop_mcp_client(&self, id: &BundleId) -> ComputerResult<bool> {
        let result = {
            let mgr = self.mcp_manager.read().await;
            match mgr.as_ref() {
                Some(m) => m.stop_client_by_id(id).await,
                None => Err(Self::manager_uninit()),
            }
        };
        // 只有**真停了**才改变工具投影 → 仅此时同步能力（未停到不是能力变化，不广播）。
        if matches!(result, Ok(true)) {
            self.on_capability_changed().await;
        }
        result
    }

    /// 重启单个 MCP 客户端（**bundle_id 寻址**）/ Restart one MCP client by bundle_id（#141 新增公开 restart）。
    pub async fn restart_mcp_client(&self, id: &BundleId) -> ComputerResult<()> {
        let result = {
            let mgr = self.mcp_manager.read().await;
            match mgr.as_ref() {
                Some(m) => m.restart_client_by_id(id).await,
                None => Err(Self::manager_uninit()),
            }
        };
        if result.is_ok() {
            self.on_capability_changed().await;
        }
        result
    }

    /// 启动全部 MCP 客户端（CLI `start all`）/ Start all MCP clients。
    pub async fn start_all_mcp_clients(&self) -> ComputerResult<()> {
        let result = {
            let mgr = self.mcp_manager.read().await;
            match mgr.as_ref() {
                Some(m) => m.start_all().await,
                None => Err(Self::manager_uninit()),
            }
        };
        if result.is_ok() {
            self.on_capability_changed().await;
        }
        result
    }

    /// 停止全部 MCP 客户端（CLI `stop all`）/ Stop all MCP clients。
    pub async fn stop_all_mcp_clients(&self) -> ComputerResult<()> {
        let result = {
            let mgr = self.mcp_manager.read().await;
            match mgr.as_ref() {
                Some(m) => m.stop_all().await,
                None => Err(Self::manager_uninit()),
            }
        };
        if result.is_ok() {
            self.on_capability_changed().await;
        }
        result
    }

    fn manager_uninit() -> ComputerError {
        ComputerError::InvalidState("MCP Manager not initialized".to_string())
    }

    /// #148：MCP 起停**真有变更**后：bump capability revision（§12 R2：改变 Agent-facing 工具投影）
    /// 并（joined 时）向 Office 广播 `server:update_tool_list`（events.md §server:update_tool_list）。
    /// 与 [`McpChangeReactor`] 的 `tools/list_changed` 路径合流——二者皆由 [`broadcast_tool_list_update`] 收口。
    async fn on_capability_changed(&self) {
        self.status.bump_capability();
        broadcast_tool_list_update(&self.socketio_client).await;
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
            // #147：embed 声明快照是**不可变构造入参**（非运行期状态），MUST 随 clone 保留——否则克隆出的
            // Computer 上跑 #139 回收判据时 embed 面静默消失 → 重开「误回收 embed」缺口。同理 flag 层路径。
            embed_servers: self.embed_servers.clone(),
            mcp_flag_config: self.mcp_flag_config.clone(),
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
            // 去抖器按 detached socketio 重建：避免 handler 持有的 ops 经去抖器反向引用真 client。
            skill_debouncer: Arc::new(build_skill_debouncer(
                &self.skill_registry,
                &self.skill_home,
                &detached_socketio,
            )),
            skill_watcher: Arc::new(Mutex::new(None)),
            skill_watch_polling: self.skill_watch_polling,
            blob_cache_root_override: self.blob_cache_root_override.clone(),
            blob_thresholds: self.blob_thresholds,
            toolspool_store: Arc::clone(&self.toolspool_store),
            blob_resolvers: Arc::clone(&self.blob_resolvers),
            inflight_tool_tasks: Arc::clone(&self.inflight_tool_tasks),
            // desktop 缓存共享（clone 与本体同一 window:// 集合视图）；通知任务句柄不复制（仅本体持有消费者）。
            desktop_window_uris: Arc::clone(&self.desktop_window_uris),
            mcp_notify_task: Arc::new(Mutex::new(None)),
            // resolver 注入随 clone 保留（handler 路径不渲染 server config，仅占位满足 struct 完整性）。
            input_resolver: self.input_resolver.clone(),
            secret_resolver: self.secret_resolver.clone(),
            // #114 S7：detached 克隆共享同一 RuntimeStatus（观测视图对本体一致）。
            status: Arc::clone(&self.status),
            // #113 S6：config 锚点是构造期 seam，随 clone 保留（handler 路径不 mutate config，占位满足完整性）。
            config_dir: self.config_dir.clone(),
            // #121：User-config env 上下文亦是构造期 seam，随 clone 保留（实例上下文对 detached 克隆一致）。
            config_env: self.config_env.clone(),
        }
    }

    /// 建立 Socket.IO 连接 / Establish the Socket.IO connection.
    ///
    /// #86：连接面鉴权唯一走 `options.auth_payload`（Socket.IO auth dict）；`options.headers` 仅路由。
    pub async fn connect_socketio(&self, url: &str, options: ConnectOptions) -> ComputerResult<()>
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
        let parsed_headers = options.headers.as_deref().map(parse_headers_string);

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
        .namespace(options.namespace)
        .computer_ops(ops);
        // #86：auth dict 负载接到 Builder（Builder 再透传到 CONNECT auth + 4900 重连）——唯一鉴权信道。
        if let Some(payload) = options.auth_payload {
            builder = builder.auth_payload(payload);
        }
        if let Some(h) = parsed_headers {
            builder = builder.headers(h);
        }
        let client = builder.connect().await?;

        // 设置客户端到Computer / Set client to Computer
        let client_arc = Arc::new(client);
        self.set_socketio_client(client_arc.clone()).await;

        // #114 S7：Socket.IO 已连接（Office join 可能未完成，契约 §3）/ connected。
        self.status.transition(LifecycleState::Connected);

        info!(
            "Connected to SMCP server at {} with computer name: {}",
            url, self.name
        );

        Ok(())
    }

    /// #148：底层 transport 断开 + 清空 `socketio_client` 槽（`disconnect_socketio` / `shutdown` 共用，
    /// 同根 bug 统一收口）。
    ///
    /// 取写锁；若槽内有 client，先调底层 transport disconnect（发 Socket.IO DISCONNECT 包并关 transport；
    /// 持写锁跨 await，与 `join_office`/`leave_office` 持读锁跨 await 同构——`tf-rust-socketio` 的 `Client`
    /// 背后 reader 后台任务持克隆，仅 Drop 用户句柄**不会**关 transport，必须显式 `disconnect()`），
    /// 成功后再置 `None`。幂等：槽已空 → no-op。
    ///
    /// 失败上抛 Err、**不**清槽、**不**迁移 lifecycle——槽内 client 保留可重试（契约：`Ok` ⟹ 旧 transport
    /// 已结束，而非仅表示 SDK 丢弃本地引用）。
    async fn close_socketio_transport(&self) -> ComputerResult<()> {
        let mut socketio_ref = self.socketio_client.write().await;
        if let Some(client) = socketio_ref.as_ref() {
            client.disconnect().await?;
        }
        *socketio_ref = None;
        Ok(())
    }

    /// 断开Socket.IO连接 / Disconnect Socket.IO
    pub async fn disconnect_socketio(&self) -> ComputerResult<()> {
        // #148：自身完成底层 transport disconnect（不再仅置 None）。失败上抛 Err、不迁移 lifecycle。
        self.close_socketio_transport().await?;
        // #114 S7：断开 Socket.IO 后本地 runtime 仍存活 → 回 Started（契约 §4.5：断开后不再向旧 Office 发
        // `server:update_*`——由 client=None 天然保证；本地管理操作可继续）。已 shutdown 则 transition 为 no-op。
        self.status.transition(LifecycleState::Started);
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
            // #114 S7：已加入 Office，可接收路由来的 `client:*`（契约 §3）/ joined office。
            self.status.transition(LifecycleState::JoinedOffice);
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
            // #114 S7：离开 Office 但连接仍在 → 回 Connected（契约 §3）/ back to connected。
            self.status.transition(LifecycleState::Connected);
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

        // #114 S7：**在拆除资源之前**即进入 Shutdown 终态并闸断观测事件流（契约 §4.7「shutdown 开始即阻断 stale
        // callbacks / emissions」）。放在开头而非结尾——否则若下方 `stop_all().await?` 失败提前 return，闸门将永不
        // engage、状态卡在非终态。`enter_shutdown` 幂等：发唯一终态 `LifecycleChanged(Shutdown)` 后所有后续
        // transition/emit/bump 均 no-op。
        self.status.enter_shutdown();

        // #106：先停 MCP 变化通知消费者，避免其在 manager stop 期间仍反应残留通知。
        self.stop_mcp_notify_consumer().await;

        // INT-01 #68：停 SKILL watcher + 关去抖器（防停机竞态遗留任务）/ stop watcher + close debouncer。
        {
            let mut guard = self.skill_watcher.lock().await;
            if let Some(mut watcher) = guard.take() {
                watcher.stop();
            }
        }
        self.skill_debouncer.aclose().await;

        // #148：先关底层 transport（与 disconnect_socketio 同根 bug 收口），再停 MCP——二者独立、锁不冲突
        // （socketio_client vs mcp_manager；close_socketio_transport 返回前已释放其 W 锁）。**先**关 transport
        // 保证下方 `manager.stop_all().await?` 失败早返时 transport 不泄漏到进程退出（原序置 None 在 stop_all
        // 之后，失败即漏）。notify 消费者已在上方停掉，期间无 reactor emit。
        if let Err(e) = self.close_socketio_transport().await {
            warn!(error = %e, "transport disconnect during shutdown failed, skipped");
        }

        let mut manager_guard = self.mcp_manager.write().await;
        if let Some(manager) = manager_guard.take() {
            manager.stop_all().await?;
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
            // #147：embed 声明快照是**不可变构造入参**（非运行期状态），MUST 随 clone 保留——否则克隆出的
            // Computer 上跑 #139 回收判据时 embed 面静默消失 → 重开「误回收 embed」缺口。同理 flag 层路径。
            embed_servers: self.embed_servers.clone(),
            mcp_flag_config: self.mcp_flag_config.clone(),
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
            skill_debouncer: Arc::new(build_skill_debouncer(
                &self.skill_registry,
                &self.skill_home,
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
            // desktop 缓存共享（clone 与本体同一 window:// 集合视图）；通知任务句柄不复制（仅本体持有消费者）。
            desktop_window_uris: Arc::clone(&self.desktop_window_uris),
            mcp_notify_task: Arc::new(Mutex::new(None)),
            // resolver 注入随 clone 保留（handler 路径不渲染 server config，仅占位满足 struct 完整性）。
            input_resolver: self.input_resolver.clone(),
            secret_resolver: self.secret_resolver.clone(),
            // #114 S7：status 共享（clone 与本体同一观测视图）。
            status: Arc::clone(&self.status),
            // #113 S6：config 锚点构造期 seam，随 clone 保留。
            config_dir: self.config_dir.clone(),
            // #121：User-config env 上下文构造期 seam，随 clone 保留。
            config_env: self.config_env.clone(),
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

/// MCP 源 SKILL 重物化的共享自由函数（#106）：`Computer::restage_mcp_skills` 与 [`McpChangeReactor`] 共用，
/// 避免消费者任务依赖 `&Computer<S>`（Session 泛型）。语义与原 `restage_mcp_skills` 一致：
/// `stage_mcp_skills` 物化 + 全量重物化时按 `mcp:` 源做孤儿对账。staging 失败 → 记 ERROR + 空列表（失败隔离）。
async fn restage_mcp_skills_into(
    manager: &MCPServerManager,
    skill_registry: &Arc<RwLock<SkillRegistry>>,
    home: &std::path::Path,
    bundle_id: Option<&str>,
) -> Vec<String> {
    let registered = match stage_mcp_skills(manager, skill_registry, home, bundle_id, None).await {
        Ok(names) => names,
        Err(e) => {
            error!(error = %e, "restage_mcp_skills failed (non-blocking)");
            return Vec::new();
        }
    };
    if bundle_id.is_none() {
        let present: HashSet<String> = registered.iter().cloned().collect();
        let mut reg = skill_registry.write().await;
        reconcile_orphans_in(&mut reg, &present, |s| s.starts_with("mcp:"));
    }
    registered
}

/// #148：joined 时向 Office 广播 `server:update_tool_list`（events.md §server:update_tool_list）。
///
/// Computer 的显式 MCP start/stop/restart/all（经 [`Computer::on_capability_changed`]）与
/// [`McpChangeReactor`] 的 `tools/list_changed` 路径合流于此——单一广播出口，消除重复。`slot` 为 Computer
/// 与 reactor 共享的 socketio 客户端槽。未连接/未加入 → [`SmcpComputerClient::emit_update_tool_list`]
/// 内部 `office_id` guard 静默 no-op（不发旧 Office 消息）；emit 失败仅 `debug` 日志、不上抛（本地能力变更
/// 已成功，协议重试归 SDK 观测层，业务 client 不承担）。
async fn broadcast_tool_list_update(slot: &Arc<RwLock<Option<Arc<SmcpComputerClient>>>>) {
    if let Some(client) = slot.read().await.clone() {
        if let Err(e) = client.emit_update_tool_list().await {
            debug!(error = %e, "emit_update_tool_list failed, skipped");
        }
    }
}

/// MCP 运行期变化的单一反应器（#106）：把一条 [`McpServerNotification`] 转成对应的刷新/emit 动作。
///
/// **持锁/断环设计**：持 `Weak` 管理器 cell —— 变化通知的 sender 存于 `MCPServerManager.change_tx`，消费任务
/// 持 `rx`；若 reactor 强持 manager cell，则 sender 永不 drop → `rx` 永不关闭 → 消费任务泄漏。用 `Weak` 让
/// Computer drop 时 manager cell（连同所有 sender）随之释放，`rx` 关闭、消费任务自然退出。socketio / skill /
/// desktop 缓存不含 sender，强持无环。由 `boot_up` 构建并移入消费任务；`Computer::handle_mcp_notification`
/// 也即时构建等价 reactor（供测试/直调）。
struct McpChangeReactor {
    /// `Weak` 管理器 cell（断开 sender 自持环，见类型注释）。
    manager: Weak<RwLock<Option<MCPServerManager>>>,
    socketio_client: Arc<RwLock<Option<Arc<SmcpComputerClient>>>>,
    skill_registry: Arc<RwLock<SkillRegistry>>,
    skill_home: Arc<StdRwLock<Option<PathBuf>>>,
    skill_debouncer: Arc<SkillEventDebouncer>,
    desktop_window_uris: Arc<RwLock<HashSet<String>>>,
}

impl McpChangeReactor {
    /// 反应一条 MCP 变化通知。协议映射（events.md §server:update_* / desktop.md / skill.md §8）：
    /// - `tools/list_changed` → 刷新 tool_mapping 后 emit `server:update_tool_list`；
    /// - `resources/list_changed` → desktop 集合去抖 emit + MCP 源 skill 重挂（去抖 emit_update_skills）；
    /// - `resources/updated{uri}` → `window://` 直接刷桌面 / `skill://` 重挂该源 / 其它忽略。
    async fn handle(&self, notif: McpServerNotification) {
        match notif.kind {
            McpChangeKind::ToolListChanged => self.on_tool_list_changed().await,
            McpChangeKind::ResourceListChanged => {
                // 一条 resources/list_changed 同时驱动 desktop 与 skill 两条链（skill.md §8.1 / desktop.md）。
                self.on_desktop_maybe_changed().await;
                // 集合级变化**可能含移除**：走**全量** restage（server=None）以触发 `mcp:` 源孤儿对账，
                // 使运行期消失的 skill:// 从 registry 剔除（scoped restage 只注册"当前存在"、不清理已移除，
                // 会残留陈旧项——与工具侧"预清全清重建"理念对齐）。语义同 boot_up 的全量重挂。
                self.on_skills_changed(None).await;
            }
            McpChangeKind::ResourceUpdated { uri } => {
                self.on_resource_updated(notif.server.as_str(), &uri).await
            }
        }
    }

    async fn on_tool_list_changed(&self) {
        // 先刷新 tool_mapping 再 emit：保证 Agent 回拉 get_tools 时 mapping 已含运行期新增工具（修"坑 1"），
        // 并顺带修 execute_tool 路由校验对新工具的可见性。消费者任务不在 rmcp event loop 内，调用安全无死锁。
        if let Some(mgr_cell) = self.manager.upgrade() {
            let guard = mgr_cell.read().await;
            if let Some(mgr) = guard.as_ref() {
                if let Err(e) = mgr.refresh_tool_mapping().await {
                    warn!(error = %e, "refresh_tool_mapping on tools/list_changed failed");
                }
            }
        }
        self.emit_tool_list().await;
    }

    async fn on_desktop_maybe_changed(&self) {
        let Some(windows) = self.collect_window_uris().await else {
            return;
        };
        // 集合去抖：仅 window:// URI 集合变化才 emit（desktop.md §变化检测）。
        let changed = {
            let mut cache = self.desktop_window_uris.write().await;
            if *cache != windows {
                *cache = windows;
                true
            } else {
                false
            }
        };
        if changed {
            self.emit_desktop().await;
        } else {
            debug!("window:// set unchanged, skip server:update_desktop");
        }
    }

    /// `bundle_id`（server 唯一身份，非 display 名——#127）给定则仅重挂该 server；None 则全量 + 孤儿对账。
    async fn on_skills_changed(&self, bundle_id: Option<&str>) {
        let Some(home) = self.skill_home.read().expect("skill_home poisoned").clone() else {
            return;
        };
        if let Some(mgr_cell) = self.manager.upgrade() {
            let guard = mgr_cell.read().await;
            if let Some(mgr) = guard.as_ref() {
                let _ = restage_mcp_skills_into(mgr, &self.skill_registry, &home, bundle_id).await;
            }
        }
        // 去抖 emit_update_skills（与本地 watcher 路径合流；标脏 → 窗口合并 → 单次 emit）。
        self.skill_debouncer.mark_dirty();
    }

    /// `bundle_id` = 通知来源 server 的唯一身份（[`McpServerNotification::server`]，#127）。
    async fn on_resource_updated(&self, bundle_id: &str, uri: &str) {
        if uri.starts_with("window://") {
            // 内容级更新：直接刷桌面（不比集合，降延迟）；同步刷新集合缓存避免后续集合比较误判。
            if let Some(windows) = self.collect_window_uris().await {
                *self.desktop_window_uris.write().await = windows;
            }
            self.emit_desktop().await;
        } else if uri.starts_with("skill://") {
            self.on_skills_changed(Some(bundle_id)).await;
        } else {
            debug!(
                uri,
                "resources/updated for non-window/non-skill URI, ignored"
            );
        }
    }

    /// 汇总所有活跃 server 的 `window://` URI 集合（manager 不可达 → None，调用方跳过）。
    async fn collect_window_uris(&self) -> Option<HashSet<String>> {
        let mgr_cell = self.manager.upgrade()?;
        let guard = mgr_cell.read().await;
        let mgr = guard.as_ref()?;
        Some(
            mgr.list_all_windows(None)
                .await
                .into_iter()
                .map(|(_, r)| r.uri.to_string())
                .collect(),
        )
    }

    async fn emit_tool_list(&self) {
        // #148：与 Computer 显式 start/stop 路径合流于同一广播函数（DRY）。
        broadcast_tool_list_update(&self.socketio_client).await;
    }

    async fn emit_desktop(&self) {
        if let Some(client) = self.socketio_client.read().await.clone() {
            if let Err(e) = client.emit_update_desktop().await {
                debug!(error = %e, "emit_update_desktop failed, skipped");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_clients::manager::test_support::bid;
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

    // ── #114 S7：runtime status / revision / events ──────────────────────────────

    #[tokio::test]
    async fn status_reflects_loaded_desired_state() {
        // 未 boot：manager=None → 活跃/工具计数 0；但 status 仍反映**已声明** desired MCP 集（构造注入）。
        let mut declared = HashMap::new();
        declared.insert("srv-a".to_string(), user_stdio_server97("srv-a"));
        declared.insert("srv-b".to_string(), user_stdio_server97("srv-b"));
        let computer = Computer::new(
            "c",
            SilentSession::new("s"),
            None,
            Some(declared),
            false,
            false,
        );

        let snap = computer.status().await;
        assert_eq!(snap.lifecycle, LifecycleState::Created);
        assert_eq!(snap.mcp_servers, 2, "已声明 desired MCP 集");
        assert_eq!(snap.active_mcp_servers, 0, "未 boot → 无活跃进程");
        assert_eq!(snap.tools, 0);
        assert_eq!(snap.skills, 0);
        assert_eq!(snap.config_revision, 0);
        assert_eq!(snap.capability_revision, 0);
        assert!(snap.last_error.is_none());
        assert!(snap.degraded_reason.is_none());
    }

    /// #147/#139：`Clone` / `clone_for_handlers` MUST 保留 frozen embed 声明快照——否则克隆出的 Computer 上
    /// 跑回收判据时 embed 面静默消失、重开「误回收 embed」缺口（隔离复审 🟡1）。embed 是不可变构造入参、
    /// 非运行期状态，随 clone 存续（同 `name`）。
    #[test]
    fn clone_preserves_frozen_embed_declaration_147() {
        let cfg: MCPServerConfig = serde_json::from_value(serde_json::json!({
            "type": "stdio", "name": "host-srv", "server_parameters": {"command": "e"}
        }))
        .unwrap();
        let mut servers = HashMap::new();
        servers.insert("host-srv".to_string(), cfg);
        let comp = Computer::new(
            "c",
            SilentSession::new("s"),
            None,
            Some(servers),
            false,
            false,
        );
        assert_eq!(comp.embed_servers.len(), 1, "构造入参入 frozen embed 快照");
        let cloned = comp.clone();
        assert_eq!(
            cloned.embed_servers.len(),
            1,
            "clone MUST 保留 embed 声明面（#147 连坐防线随 clone 存续）"
        );
    }

    /// #139 回归（隔离复审 🔴#2）：`non_plugin_declared_bundle_ids` MUST 以**实例 config_dir** 锚定 project/local
    /// （非进程 cwd）——否则 `with_config_dir` 嵌入宿主在 `<config_dir>/.tfrobot/mcp.json` 声明的用户 server 看不见，
    /// 其 durable(project) 声明会在 plugin uninstall/disable 时被误判「非用户声明」而连坐停摘（「永不连坐」）。
    #[test]
    fn non_plugin_declared_anchors_project_at_config_dir_139() {
        let tmp = tempfile::TempDir::new().unwrap();
        let proj = tmp.path().join("proj");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        // 在 config_dir 的 project mcp.json 声明用户 server。
        let mcp = crate::settings::mcp_config::workdir_mcp_config_path(&proj);
        std::fs::create_dir_all(mcp.parent().unwrap()).unwrap();
        std::fs::write(
            &mcp,
            r#"{"servers":{"user-owned":{"type":"stdio","server_parameters":{"command":"u"}}}}"#,
        )
        .unwrap();

        let comp = Computer::new("c", SilentSession::new("s"), None, None, false, false)
            .with_config_dir(&proj);
        // 隔离 user scope（XDG → 空临时目录，防读到真实 ~/.config）。
        let mut env = EnvMap::new();
        env.insert(
            "XDG_CONFIG_HOME".to_string(),
            tmp.path().join("xdg").to_string_lossy().into_owned(),
        );
        let set = comp.non_plugin_declared_bundle_ids(&home, Some(&env));
        let want = crate::mcp_clients::bundle_id::resolve_bundle_id(
            &serde_json::from_value(serde_json::json!({
                "type":"stdio","name":"user-owned","server_parameters":{"command":"u"}
            }))
            .unwrap(),
        );
        assert!(
            set.contains(&want),
            "config_dir 的 project 声明用户 server MUST 入非-plugin 集（永不连坐）；实际: {set:?}"
        );
    }

    #[tokio::test]
    async fn config_revision_bumps_and_is_observable() {
        // S6（#113）落盘接线入口：bump_config_revision 单调 +1、广播事件、快照可见；capability 独立（§12 R2）。
        let computer = Computer::new("c", SilentSession::new("s"), None, None, false, false);
        let mut rx = computer.subscribe_events();
        assert_eq!(computer.config_revision(), 0);

        assert_eq!(computer.bump_config_revision(), 1);
        assert_eq!(computer.config_revision(), 1);
        assert_eq!(computer.status().await.config_revision, 1);
        // config bump 不动 capability（分离单调）。
        assert_eq!(computer.capability_revision(), 0);

        assert_eq!(
            rx.recv().await.unwrap(),
            ComputerEvent::ConfigRevisionBumped { revision: 1 }
        );
    }

    // ── #113 S6：runtime mutate 落盘接线 / persist-on-mutate wiring ─────────────────

    /// add_or_update_server_in_scope(Project) 显式落 project scope（团队共享，opt-in）+ config revision +1（§12 R2）。
    /// #123（协议#19 加固）：默认已改 local，团队共享须**显式** opt-in project——本测试守护该 opt-in 路径。
    #[tokio::test]
    async fn add_or_update_server_in_scope_project_persists_and_survives_reload() {
        use crate::settings::config::{load_config, ConfigContext, WriteScope};
        use crate::settings::mcp_config::{workdir_mcp_config_path, workdir_mcp_local_config_path};
        let tmp = tempfile::TempDir::new().unwrap();
        let computer = Computer::new("c", SilentSession::new("s"), None, None, false, false)
            .with_config_dir(tmp.path());

        computer
            .add_or_update_server_in_scope(user_stdio_server97("persisted"), WriteScope::Project)
            .await
            .unwrap();

        // 落盘成功 → config revision 前进；capability 亦 +1（mount_server，工具投影变化）。
        assert_eq!(computer.config_revision(), 1);
        assert_eq!(computer.capability_revision(), 1);

        // 显式 opt-in → 落 project mcp.json（**非** local）。
        assert!(
            workdir_mcp_config_path(tmp.path()).exists(),
            "显式 Project 应落 project mcp.json"
        );
        assert!(
            !workdir_mcp_local_config_path(tmp.path()).exists(),
            "显式 Project 不应落 local"
        );

        // 「重启」= 以同一 config_dir 重投影快照 → server 仍在（不丢）。
        let snap = load_config(&ConfigContext::new(tmp.path()));
        assert!(
            snap.mcp.servers.iter().any(|s| s.name == "persisted"),
            "add_or_update_server_in_scope 应落盘、重投影可读"
        );
    }

    /// #123（协议#19 加固）：`add_or_update_server`（默认）新 server 落 **local**（不入 git）、**不**碰 project mcp.json；
    /// local 仍重投影可读（重启存活）。
    #[tokio::test]
    async fn add_or_update_server_defaults_to_local_scope_not_git_shared() {
        use crate::settings::config::{load_config, ConfigContext};
        use crate::settings::mcp_config::{workdir_mcp_config_path, workdir_mcp_local_config_path};
        let tmp = tempfile::TempDir::new().unwrap();
        let computer = Computer::new("c", SilentSession::new("s"), None, None, false, false)
            .with_config_dir(tmp.path());

        computer
            .add_or_update_server(user_stdio_server97("declared"))
            .await
            .unwrap();

        // 落 local（不入 git），**不**落 project（team-shared）。
        assert!(
            workdir_mcp_local_config_path(tmp.path()).exists(),
            "默认应落 local mcp.local.json"
        );
        assert!(
            !workdir_mcp_config_path(tmp.path()).exists(),
            "默认**不得**静默污染 git 共享的 project mcp.json（#123 / 协议#19 加固）"
        );

        // local 仍重投影可读 → 重启存活（不损失 #113 收益）。
        let snap = load_config(&ConfigContext::new(tmp.path()));
        assert!(
            snap.mcp.servers.iter().any(|s| s.name == "declared"),
            "local scope 声明须重投影可读（重启存活）"
        );
    }

    /// mount_server（治理物化路径）**不落盘**、只 bump capability 不 bump config——bundled server 归属 ledger
    /// 意图，不得重复写入 project mcp.json（否则卸载后孤儿化、每次 boot remount 重写用户配置）。
    #[tokio::test]
    async fn mount_server_does_not_persist_and_bumps_only_capability() {
        use crate::settings::config::{load_config, ConfigContext};
        use crate::settings::mcp_config::workdir_mcp_config_path;
        let tmp = tempfile::TempDir::new().unwrap();
        let computer = Computer::new("c", SilentSession::new("s"), None, None, false, false)
            .with_config_dir(tmp.path());

        computer
            .mount_server(user_stdio_server97("bundled-like"))
            .await
            .unwrap();

        // 运行期物化：capability +1，config 不动（分离单调，§12 R2）。
        assert_eq!(computer.capability_revision(), 1);
        assert_eq!(computer.config_revision(), 0);
        // 未落盘：project mcp.json 不存在、快照不含该 server。
        assert!(
            !workdir_mcp_config_path(tmp.path()).exists(),
            "mount_server 不得落盘"
        );
        let snap = load_config(&ConfigContext::new(tmp.path()));
        assert!(!snap.mcp.servers.iter().any(|s| s.name == "bundled-like"));
    }

    /// remove_server 落盘删声明（S2 R1：删所有可写 scope）+ config revision +1；重投影不再见该 server。
    #[tokio::test]
    async fn remove_server_persists_removal_and_bumps_config() {
        use crate::settings::config::{load_config, ConfigContext};
        let tmp = tempfile::TempDir::new().unwrap();
        let computer = Computer::new("c", SilentSession::new("s"), None, None, false, false)
            .with_config_dir(tmp.path());

        computer
            .add_or_update_server(user_stdio_server97("gone"))
            .await
            .unwrap();
        assert_eq!(computer.config_revision(), 1);
        assert!(load_config(&ConfigContext::new(tmp.path()))
            .mcp
            .servers
            .iter()
            .any(|s| s.name == "gone"));

        let removed = computer
            .remove_server(&BundleId::try_from("gone".to_string()).unwrap())
            .await
            .unwrap();
        assert!(removed, "确有声明被删 ⇒ 回执 MUST 为 true（#141）");
        assert_eq!(
            computer.config_revision(),
            2,
            "删声明落盘 → config revision 再 +1"
        );
        assert!(
            !load_config(&ConfigContext::new(tmp.path()))
                .mcp
                .servers
                .iter()
                .any(|s| s.name == "gone"),
            "remove_server 应落盘删声明"
        );
    }

    /// #121 🔴（隔离复审发现）：boot 期 v0.2.x→v0.3.0 迁移的 `enabledPlugins` 写目标须经**本实例** `config_env`
    /// 解析——绝不误写宿主 User settings。此前 boot 硬传 `env=None` → 迁移写 ambient 宿主 `~/.config/.../settings.json`
    /// （污染宿主）、且对本实例无效（reconcile 读实例、迁移写宿主，本实例存量 plugin 不点亮）。
    #[tokio::test]
    async fn boot_migration_writes_enabled_plugins_to_injected_instance_settings() {
        use crate::settings::scope::user_settings_path;
        use crate::settings::store::installed_plugins_path;
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        // 本实例 User-config 环境（XDG → 实例目录，与宿主进程环境隔离）。
        let inst_cfg = tmp.path().join("instance-xdg");
        let mut env = EnvMap::new();
        env.insert(
            "XDG_CONFIG_HOME".to_string(),
            inst_cfg.to_string_lossy().into_owned(),
        );

        // v0.2.x 态：账本一条 enabled 记录（scope=user）、无意图文件、无 enabledPlugins → boot 触发迁移。
        std::fs::write(
            installed_plugins_path(Some(&home), None),
            r#"{"plugins": {"p@m": [{"installPath": "x", "scope": "user"}]}}"#,
        )
        .unwrap();

        let computer = Computer::new("c", SilentSession::new("s"), None, None, false, false)
            .with_skill_home(&home)
            .with_config_env(env.clone())
            .with_config_dir(tmp.path().join("proj"))
            .with_blob_cache_root(tmp.path().join("blob"));
        computer.boot_up().await.unwrap();

        // 迁移把 enabledPlugins=true 写到**注入的实例** User settings（config_env 锚定），而非宿主 ambient。
        // 修复前（env=None）此文件不会被创建/写入 → 断言失败（RED，且宿主被污染）。
        let inst_settings = user_settings_path(Some(&env));
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&inst_settings).unwrap()).unwrap();
        assert_eq!(
            v["enabledPlugins"]["p@m"],
            serde_json::json!(true),
            "boot 迁移须把 enabledPlugins=true 写到注入的实例 User settings（#121：config_env 锚定，不误写宿主）"
        );
    }

    /// DoD item2：enable/disable 落盘 scope 由**安装记录**消解（非恒定 user）——installer 层刻意不回查，SDK
    /// 接线层从 ledger `record.scope` 确定性取值。缺省时读账本、显式传入时尊重原值。
    #[tokio::test]
    async fn resolve_plugin_install_scope_reads_record_scope_not_constant_user() {
        use crate::settings::store::installed_plugins_path;
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().to_path_buf();
        // 播种账本：plugin `p@m` 装在 **project** scope（非 user）。
        std::fs::write(
            installed_plugins_path(Some(&home), None),
            r#"{"plugins": {"p@m": [{"installPath": "x", "scope": "project"}]}}"#,
        )
        .unwrap();

        let computer = Computer::new("c", SilentSession::new("s"), None, None, false, false)
            .with_skill_home(&home);

        // 消解到安装记录的 scope（project），而非硬编码 user。
        assert_eq!(
            computer.resolve_plugin_install_scope("p@m", &home, None),
            Some("project".to_string())
        );
        // 无记录 → None（installer 回退默认 user；enable 本要求已安装）。
        assert_eq!(
            computer.resolve_plugin_install_scope("absent@m", &home, None),
            None
        );
    }

    /// 归一化纯函数：剥内嵌 name + `type` 判别符归协议 §9.1 规范小写（Stdio→stdio / Sse→sse / Http→streamable）。
    #[test]
    fn canonicalize_persist_body_strips_name_and_lowercases_type() {
        use serde_json::json;
        let out = canonicalize_persist_body(
            json!({"type": "Stdio", "name": "s", "server_parameters": {"command": "x"}}),
        );
        assert_eq!(
            out["type"],
            json!("stdio"),
            "Rust 变体名 Stdio → 规范小写 stdio"
        );
        assert!(out.get("name").is_none(), "map key 即身份，剥内嵌 name");
        assert_eq!(
            out["server_parameters"]["command"],
            json!("x"),
            "其余字段保真"
        );
        assert_eq!(
            canonicalize_persist_body(json!({"type": "Sse"}))["type"],
            json!("sse")
        );
        // Http → streamable（对齐 Python StreamableHttpServerConfig 的 Literal["streamable"]）。
        assert_eq!(
            canonicalize_persist_body(json!({"type": "Http"}))["type"],
            json!("streamable")
        );
        // 已规范则原样（防御）。
        assert_eq!(
            canonicalize_persist_body(json!({"type": "stdio"}))["type"],
            json!("stdio")
        );
    }

    /// 🔴 回归守卫：落盘的 `mcp.json` 用协议规范小写判别符（跨 SDK/Python 可读），**非** Rust 变体名 "Stdio"；
    /// 且经 Rust 读端（alias）往返无损。
    #[tokio::test]
    async fn persisted_mcp_json_uses_protocol_canonical_type_token_and_roundtrips() {
        use crate::settings::config::{load_config, ConfigContext};
        use crate::settings::mcp_config::workdir_mcp_local_config_path;
        let tmp = tempfile::TempDir::new().unwrap();
        let computer = Computer::new("c", SilentSession::new("s"), None, None, false, false)
            .with_config_dir(tmp.path());
        computer
            .add_or_update_server(user_stdio_server97("x"))
            .await
            .unwrap();

        // 落盘原始字节：规范小写 `stdio`（非 `Stdio`）、无内嵌 name。#123：默认落 local（不入 git）。
        let raw: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(workdir_mcp_local_config_path(tmp.path())).unwrap(),
        )
        .unwrap();
        assert_eq!(
            raw["servers"]["x"]["type"],
            serde_json::json!("stdio"),
            "跨 SDK 可读需协议规范小写判别符"
        );
        assert!(raw["servers"]["x"].get("name").is_none(), "map key 即身份");

        // Rust 读端往返无损（重投影仍解析出该 server）。
        let snap = load_config(&ConfigContext::new(tmp.path()));
        assert!(snap.mcp.servers.iter().any(|s| s.name == "x"));
    }

    /// 🟡 §12 R2：幂等 re-add / no-op remove **不**虚假 bump config revision（内容真变才 bump）。
    #[tokio::test]
    async fn idempotent_readd_and_noop_remove_do_not_bump_config() {
        let tmp = tempfile::TempDir::new().unwrap();
        let computer = Computer::new("c", SilentSession::new("s"), None, None, false, false)
            .with_config_dir(tmp.path());

        // 首次 add：内容变 → config +1。
        computer
            .add_or_update_server(user_stdio_server97("x"))
            .await
            .unwrap();
        assert_eq!(computer.config_revision(), 1);

        // 同内容幂等 re-add：零落盘 → config **不**再 bump。
        computer
            .add_or_update_server(user_stdio_server97("x"))
            .await
            .unwrap();
        assert_eq!(
            computer.config_revision(),
            1,
            "幂等 re-add 不虚假 bump config"
        );

        // 删不存在的 server：空计划零落盘 → config **不** bump。
        // #141：回执亦须如实为 `false`——CLI 据此打 ℹ️ 而非 ✅（拼错的 target 往往是合法 bundle_id 字面量）。
        let removed = computer
            .remove_server(&BundleId::try_from("never-existed".to_string()).unwrap())
            .await
            .unwrap();
        assert!(
            !removed,
            "既无声明也无实例 ⇒ 回执 MUST 为 false（禁假成功）"
        );
        assert_eq!(
            computer.config_revision(),
            1,
            "no-op remove 不虚假 bump config"
        );

        // 真删已存在 → config +1，且回执为 `true`。
        let removed = computer
            .remove_server(&BundleId::try_from("x".to_string()).unwrap())
            .await
            .unwrap();
        assert!(removed, "确有声明被删 ⇒ 回执 MUST 为 true");
        assert_eq!(computer.config_revision(), 2);
    }

    #[tokio::test]
    async fn shutdown_enters_terminal_state_and_silences_events() {
        // 契约 §4.7：shutdown 后除终态事件外不再发；revision bump 降 no-op。无需 boot（manager/watcher 均 None-safe）。
        let computer = Computer::new("c", SilentSession::new("s"), None, None, false, false);
        let mut rx = computer.subscribe_events();

        computer.shutdown().await.unwrap();
        assert_eq!(computer.lifecycle_state(), LifecycleState::Shutdown);
        assert_eq!(
            rx.recv().await.unwrap(),
            ComputerEvent::LifecycleChanged {
                state: LifecycleState::Shutdown
            }
        );

        // shutdown 后 bump 为 no-op（不发事件、不推进 revision）。
        assert_eq!(computer.bump_config_revision(), 0);
        assert_eq!(computer.config_revision(), 0);
        // 通道再无新事件。
        assert!(matches!(
            rx.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn clone_shares_runtime_status() {
        // 守卫「三处构造点共享同一 Arc<RuntimeStatus>」不变量：任一 clone 上的 bump/状态迁移对本体可见。
        // 若某构造点误写成 `Arc::new(RuntimeStatus::new())`（观测视图割裂），本测试即失败。
        let computer = Computer::new("c", SilentSession::new("s"), None, None, false, false);
        let clone = computer.clone();
        assert_eq!(clone.bump_config_revision(), 1);
        assert_eq!(computer.config_revision(), 1, "clone 的 bump 须对本体可见");
        // 反向：本体 bump → clone 可见。
        assert_eq!(computer.bump_config_revision(), 2);
        assert_eq!(clone.config_revision(), 2);
        // 第三个构造点 clone_for_handlers（socketio-detached handler 克隆）亦须共享同一 status Arc——
        // 否则 handler 路径触发的观测变化对本体割裂不可见。
        let handler_clone = computer.clone_for_handlers();
        assert_eq!(
            handler_clone.config_revision(),
            2,
            "handler 克隆须共享 status"
        );
        assert_eq!(handler_clone.bump_config_revision(), 3);
        assert_eq!(
            computer.config_revision(),
            3,
            "handler 克隆的 bump 须对本体可见"
        );
    }

    #[tokio::test]
    async fn status_transitions_and_capability_bumps_across_boot_and_mcp_lifecycle() {
        // 验收 1 的**实义分支**（已加载 = 已 boot）+ boot/start/stop 的生命周期接线回归守卫。
        let tmp = tempfile::TempDir::new().unwrap();
        let computer = Computer::new("c", SilentSession::new("s"), None, None, false, false)
            .with_skill_home(tmp.path().join("home"))
            .with_blob_cache_root(tmp.path().join("blob"));

        // boot 前：Created、revision 皆 0。
        assert_eq!(computer.lifecycle_state(), LifecycleState::Created);
        assert_eq!(computer.capability_revision(), 0);

        // boot：无 marketplace 失败 → Started；能力投影就绪 → capability bump。
        computer.boot_up().await.unwrap();
        let snap = computer.status().await;
        assert_eq!(snap.lifecycle, LifecycleState::Started);
        assert!(
            snap.capability_revision >= 1,
            "boot 应 bump capability revision"
        );
        assert!(snap.degraded_reason.is_none());
        assert!(snap.last_error.is_none());

        // start/stop MCP（空配置 → Ok）改变工具投影 → 各 bump 一次 capability（单调）。
        let cap_after_boot = computer.capability_revision();
        computer.start_all_mcp_clients().await.unwrap();
        let cap_after_start = computer.capability_revision();
        assert!(cap_after_start > cap_after_boot, "start 应 bump capability");
        computer.stop_all_mcp_clients().await.unwrap();
        assert!(
            computer.capability_revision() > cap_after_start,
            "stop 应 bump capability"
        );

        // shutdown → 终态 + 闸断。
        computer.shutdown().await.unwrap();
        assert_eq!(computer.lifecycle_state(), LifecycleState::Shutdown);
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

    // ── #94：Computer 级 marketplace / plugin 生命周期 API ───────────────────────
    #[tokio::test]
    async fn computer_marketplace_add_and_remove_no_clone() {
        use crate::settings::lifecycle::marketplace_name_taken;
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        // 不 boot_up：add/remove 只需 skill_registry 写锁 + skill_home（运行期只读、override 即足）。
        let computer = Computer::new("c", SilentSession::new("s"), None, None, false, false)
            .with_skill_home(home.clone())
            .with_blob_cache_root(tmp.path().join("blob"));

        // 产品 client 经 Computer 方法添加 marketplace——**不**直接触碰 SkillRegistry，得结构化结果。
        let outcome = computer
            .add_marketplace(
                "acme/skills",
                AddMarketplaceParams {
                    no_clone: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(outcome.name, "skills");
        assert!(outcome.no_clone);
        // 账本落在构造期固定的 skill_home 边界内。
        assert!(marketplace_name_taken(
            &computer.skill_home(),
            None,
            "skills"
        ));

        // 移除（保留 plugin 记录）→ prune 账本条目。
        let removed = computer
            .remove_marketplace(
                "skills",
                RemoveMarketplaceParams {
                    keep_plugins: true,
                    hooks: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(removed.name, "skills");
        assert!(removed.kept_plugins);
        assert!(!marketplace_name_taken(
            &computer.skill_home(),
            None,
            "skills"
        ));

        // 未知 marketplace → 结构化错误。
        assert!(matches!(
            computer
                .remove_marketplace("ghost", RemoveMarketplaceParams::default())
                .await,
            Err(GovernanceError::UnknownMarketplace(n)) if n == "ghost"
        ));
    }

    #[tokio::test]
    async fn computer_plugin_wrappers_plumb_errors_and_noop() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let computer = Computer::new("c", SilentSession::new("s"), None, None, false, false)
            .with_skill_home(home)
            .with_blob_cache_root(tmp.path().join("blob"));

        // 非法 id → 错误经薄封装如实上抛（Precondition）。
        assert!(matches!(
            computer
                .install_plugin("no-at-sign", InstallOptions::default(), None)
                .await,
            Err(PluginInstallError::Precondition(_))
        ));
        // 未安装 plugin uninstall → Ok(false) no-op（不标脏）。
        assert!(!computer
            .uninstall_plugin("ghost@acme", UninstallOptions::default(), None)
            .await
            .unwrap());
    }

    // ── #95：Computer::reconcile_governance + boot 启动恢复 ───────────────────────
    /// 真实 git 子进程（与 staging / installer 测试同款假设：本机有 git）/ git subprocess helper。
    fn git95(args: &[&str], cwd: &std::path::Path) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", cwd)
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

    /// 构造 marketplace 源仓库（audit plugin：1 skill + 1 bundled MCP server）→ file:// url。
    fn build_marketplace_repo95(repo: &std::path::Path) -> String {
        std::fs::create_dir_all(repo.join(".tfrobot-plugin")).unwrap();
        std::fs::write(
            repo.join(".tfrobot-plugin/marketplace.json"),
            r#"{"plugins": [{"name": "audit", "source": "./plugins/audit"}]}"#,
        )
        .unwrap();
        let skill = repo.join("plugins/audit/skills/code-review");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: code-review\ndescription: review code\n---\nbody",
        )
        .unwrap();
        // bundled MCP server（供 reconcile_governance 阶段二经 hooks 重挂）。
        let servers = repo.join("plugins/audit/mcp-servers");
        std::fs::create_dir_all(&servers).unwrap();
        std::fs::write(
            servers.join("audit-mcp.json"),
            r#"{"type":"stdio","name":"audit-mcp","server_parameters":{"command":"node"}}"#,
        )
        .unwrap();
        git95(&["init", "-q"], repo);
        git95(&["add", "-A"], repo);
        git95(&["commit", "-qm", "init"], repo);
        format!("file://{}", repo.display())
    }

    /// 记录式 `McpInstallHooks` 替身（重挂阶段二测试用；可注入注定失败的 `register_server` /
    /// 预置 `existing` 名集测同名跳过 / 记录 `inject_inputs` 的 plugin 根）/ recording hooks。
    struct RecordingRemountHooks {
        registered: std::sync::Mutex<Vec<String>>,
        injected_roots: std::sync::Mutex<Vec<std::path::PathBuf>>,
        existing: std::collections::HashSet<String>,
        fail_register: Option<String>,
    }
    impl RecordingRemountHooks {
        fn new() -> Self {
            Self {
                registered: std::sync::Mutex::new(Vec::new()),
                injected_roots: std::sync::Mutex::new(Vec::new()),
                existing: std::collections::HashSet::new(),
                fail_register: None,
            }
        }
        fn failing(name: &str) -> Self {
            Self {
                fail_register: Some(name.to_string()),
                ..Self::new()
            }
        }
        /// 预置「既有 server 名」集（模拟用户配置已占名）→ 触发同名 skip / seed existing names。
        fn with_existing(names: &[&str]) -> Self {
            Self {
                existing: names.iter().map(|n| (*n).to_string()).collect(),
                ..Self::new()
            }
        }
    }
    #[async_trait::async_trait]
    impl McpInstallHooks for RecordingRemountHooks {
        fn existing_servers(&self) -> std::collections::HashMap<BundleId, ServerName> {
            self.existing
                .iter()
                .filter_map(|n| BundleId::try_from(n.clone()).ok().map(|b| (b, n.clone())))
                .collect()
        }
        async fn register_server(
            &self,
            cfg: MCPServerConfig,
        ) -> Result<(), crate::settings::installer::McpHookError> {
            if self.fail_register.as_deref() == Some(cfg.name()) {
                return Err(crate::settings::installer::McpHookError(format!(
                    "boom on {}",
                    cfg.name()
                )));
            }
            self.registered.lock().unwrap().push(cfg.name().to_string());
            Ok(())
        }
        async fn remove_server(
            &self,
            _id: &BundleId,
        ) -> Result<(), crate::settings::installer::McpHookError> {
            Ok(())
        }
        async fn inject_inputs(
            &self,
            plugin_root: &std::path::Path,
        ) -> Result<(), crate::settings::installer::McpHookError> {
            self.injected_roots
                .lock()
                .unwrap()
                .push(plugin_root.to_path_buf());
            Ok(())
        }
    }

    /// 重启前装配：Computer A add marketplace（真实 clone）+ install audit@acme（写 ledger）→ 返回 home。
    async fn cold_start_setup95(tmp: &tempfile::TempDir) -> std::path::PathBuf {
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let url = build_marketplace_repo95(&tmp.path().join("repo"));
        let comp_a = Computer::new("a", SilentSession::new("s"), None, None, false, false)
            .with_skill_home(home.clone())
            .with_blob_cache_root(tmp.path().join("blob-a"));
        comp_a
            .add_marketplace(
                &url,
                AddMarketplaceParams {
                    name: Some("acme"),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        comp_a
            .install_plugin("audit@acme", InstallOptions::default(), None)
            .await
            .unwrap();
        home
    }

    /// declared 视图：显式启用 `audit@acme`（v0.3.0：install 落 `installed_disabled`、不写 enabledPlugins；
    /// Computer 层无 settings 注入 seam，故经 `reconcile_governance(declared=...)` 传入启用意图做 hermetic 恢复）。
    fn declared_audit_enabled() -> serde_json::Map<String, serde_json::Value> {
        serde_json::json!({ "enabledPlugins": { "audit@acme": true } })
            .as_object()
            .unwrap()
            .clone()
    }

    /// 冷启动恢复：Computer A 经 SDK API add+install（写 ledger），新 Computer B 同 skill_home
    /// 经 `reconcile_governance` 从 ledger 重挂 marketplace skill（registry 重启即空 → 恢复）+ 幂等。
    #[tokio::test]
    async fn reconcile_governance_cold_start_restores_installed_plugin() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let url = build_marketplace_repo95(&tmp.path().join("repo"));

        // ── 重启前：Computer A 添加 marketplace（真实 clone）+ 安装 plugin（写 installed_plugins.json）。
        let comp_a = Computer::new("a", SilentSession::new("s"), None, None, false, false)
            .with_skill_home(home.clone())
            .with_blob_cache_root(tmp.path().join("blob-a"));
        comp_a
            .add_marketplace(
                &url,
                AddMarketplaceParams {
                    name: Some("acme"),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        comp_a
            .install_plugin("audit@acme", InstallOptions::default(), None)
            .await
            .unwrap();
        // v0.3.0：install = installed_disabled，comp_a registry 里 skill 为 orphan（不活跃）。
        assert!(comp_a
            .skill_registry_arc()
            .read()
            .await
            .resolve("audit:code-review")
            .is_none());
        drop(comp_a);

        // ── 重启后：Computer B 同 skill_home、registry 为空 → reconcile_governance 从 ledger 恢复。
        let comp_b = Computer::new("b", SilentSession::new("s"), None, None, false, false)
            .with_skill_home(home.clone())
            .with_blob_cache_root(tmp.path().join("blob-b"));
        assert!(
            comp_b
                .skill_registry_arc()
                .read()
                .await
                .resolve("audit:code-review")
                .is_none(),
            "新进程 registry 恢复前应为空"
        );

        let declared = declared_audit_enabled();
        let report = comp_b.reconcile_governance(None, Some(&declared)).await;
        assert_eq!(report.restored_plugins, vec!["audit@acme".to_string()]);
        assert_eq!(
            report.restored_skills,
            vec!["audit:code-review".to_string()]
        );
        assert!(report.failed_marketplaces.is_empty());
        assert!(
            comp_b
                .skill_registry_arc()
                .read()
                .await
                .resolve("audit:code-review")
                .is_some(),
            "冷启动应从 ledger 恢复 marketplace skill（enabled）"
        );

        // 幂等：再调一次仍恢复同一 skill、不重复 / 不 panic。
        let report2 = comp_b.reconcile_governance(None, Some(&declared)).await;
        assert_eq!(
            report2.restored_skills,
            vec!["audit:code-review".to_string()]
        );
        assert!(comp_b
            .skill_registry_arc()
            .read()
            .await
            .resolve("audit:code-review")
            .is_some());
    }

    /// 阶段二：给定 hooks → 重挂 bundled MCP server；report.remounted_servers 命中 + hook 收到注册 + 幂等。
    #[tokio::test]
    async fn reconcile_governance_remounts_bundled_servers_via_hooks() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = cold_start_setup95(&tmp).await;

        let comp_b = Computer::new("b", SilentSession::new("s"), None, None, false, false)
            .with_skill_home(home)
            .with_blob_cache_root(tmp.path().join("blob-b"));
        let hooks = RecordingRemountHooks::new();
        let report = comp_b
            .reconcile_governance(Some(&hooks), Some(&declared_audit_enabled()))
            .await;

        // skills 恢复 + bundled server 经 hooks 重挂。
        assert_eq!(
            report.restored_skills,
            vec!["audit:code-review".to_string()]
        );
        assert_eq!(report.remounted_servers, vec!["audit-mcp".to_string()]);
        assert_eq!(
            *hooks.registered.lock().unwrap(),
            vec!["audit-mcp".to_string()]
        );
        // #100 item3：重挂前按 plugin 根注入一次 inputs（`${input:}` D2 前缀回退前置）。
        assert_eq!(
            hooks.injected_roots.lock().unwrap().len(),
            1,
            "每 plugin 根仅注入一次 inputs"
        );

        // 幂等：二次调用仍重挂同一 server（register-or-update，by name 幂等）、不 panic。
        let report2 = comp_b
            .reconcile_governance(Some(&hooks), Some(&declared_audit_enabled()))
            .await;
        assert_eq!(report2.remounted_servers, vec!["audit-mcp".to_string()]);
    }

    /// §63（#104）：账本被外部删除后，reconcile_governance 从 `installedPlugins` 意图重建账本派生缓存
    /// （phase 1.5），enabled plugin 的 bundled server 经 hooks 重挂重现——恢复不受账本删除影响。
    #[tokio::test]
    async fn reconcile_governance_rebuilds_ledger_and_remounts_after_deletion() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = cold_start_setup95(&tmp).await; // install audit@acme（写 ledger + 意图）。

        // 外部删除账本（`installedPlugins` 意图仍在）。
        let ledger = crate::settings::store::installed_plugins_path(Some(&home), None);
        std::fs::remove_file(&ledger).unwrap();
        assert!(!ledger.exists(), "前置：账本已删");
        assert!(
            crate::settings::store::load_installed_plugins_intent(Some(&home), None)
                .account
                .installed_plugins
                .contains("audit@acme"),
            "前置：installedPlugins 意图仍含该 pid"
        );

        // 新进程 Computer B：reconcile_governance 应先重建账本（phase 1.5）再经 hooks 重挂（phase 2）。
        let comp_b = Computer::new("b", SilentSession::new("s"), None, None, false, false)
            .with_skill_home(home.clone())
            .with_blob_cache_root(tmp.path().join("blob-b"));
        let hooks = RecordingRemountHooks::new();
        let report = comp_b
            .reconcile_governance(Some(&hooks), Some(&declared_audit_enabled()))
            .await;

        // §63：账本从意图重建。
        assert_eq!(
            report.rematerialized_plugins,
            vec!["audit@acme".to_string()],
            "账本删除后应从 installedPlugins 意图重建"
        );
        assert!(report.failed_rematerialize.is_empty());
        // 重建后账本文件重现、记录含非空 installPath + bundled server 名。
        assert!(ledger.exists(), "重建后账本文件应重现");
        let recs = crate::settings::store::load_installed_plugins(Some(&home), None).account;
        let rebuilt = recs.plugins.get("audit@acme").expect("重建账本含该 pid");
        assert!(
            rebuilt
                .iter()
                .any(|r| r.install_path.as_deref().is_some_and(|s| !s.is_empty())
                    && r.mcp_servers.iter().any(|n| n.as_str() == "audit-mcp")),
            "重建记录含非空 installPath + bundled server 名"
        );

        // bundled server 经 hooks 重挂重现（恢复不受账本删除影响）。
        assert_eq!(report.remounted_servers, vec!["audit-mcp".to_string()]);
        assert_eq!(
            *hooks.registered.lock().unwrap(),
            vec!["audit-mcp".to_string()]
        );
    }

    /// #100 item3：既有同名 server → 重挂**跳过**（additive-only，用户配置胜）；不 register 覆盖、不注入。
    #[tokio::test]
    async fn reconcile_governance_remount_skips_name_conflicting_with_existing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = cold_start_setup95(&tmp).await; // 装 audit@acme（bundled server "audit-mcp"）。

        let comp_b = Computer::new("b", SilentSession::new("s"), None, None, false, false)
            .with_skill_home(home)
            .with_blob_cache_root(tmp.path().join("blob-b"));
        // hooks 报告 "audit-mcp" 已被既有 server 占名（模拟用户配置先挂先占）。
        let hooks = RecordingRemountHooks::with_existing(&["audit-mcp"]);
        let report = comp_b
            .reconcile_governance(Some(&hooks), Some(&declared_audit_enabled()))
            .await;

        // 同名冲突 → 跳过：不入 report.remounted、register 未被调用、跳过项不注入 inputs。
        assert!(
            report.remounted_servers.is_empty(),
            "同名冲突 → skip，不重挂"
        );
        assert!(
            hooks.registered.lock().unwrap().is_empty(),
            "同名冲突 → 不 register 覆盖用户配置"
        );
        assert!(
            hooks.injected_roots.lock().unwrap().is_empty(),
            "跳过项不注入 inputs"
        );
        // 阶段一 skills 恢复不受同名跳过影响。
        assert_eq!(
            report.restored_skills,
            vec!["audit:code-review".to_string()]
        );
    }

    /// 失败隔离铁律：`register_server` 注定失败 → 不 panic、不阻断、skills 仍恢复、failed server 不入 report。
    #[tokio::test]
    async fn reconcile_governance_register_failure_is_non_blocking() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = cold_start_setup95(&tmp).await;

        let comp_b = Computer::new("b", SilentSession::new("s"), None, None, false, false)
            .with_skill_home(home)
            .with_blob_cache_root(tmp.path().join("blob-b"));
        let hooks = RecordingRemountHooks::failing("audit-mcp");
        // 不 panic、正常返回。
        let report = comp_b
            .reconcile_governance(Some(&hooks), Some(&declared_audit_enabled()))
            .await;

        // skills 阶段不受 MCP 重挂失败影响，仍恢复。
        assert_eq!(
            report.restored_skills,
            vec!["audit:code-review".to_string()]
        );
        assert!(
            comp_b
                .skill_registry_arc()
                .read()
                .await
                .resolve("audit:code-review")
                .is_some(),
            "register_server 失败不应阻断 skill 恢复"
        );
        // 失败的 server 未计入 remounted、也未被 hook 记录为成功。
        assert!(report.remounted_servers.is_empty());
        assert!(hooks.registered.lock().unwrap().is_empty());
    }

    /// v0.3.0：`boot_up()` 对仅 install（未 enable）的 plugin **不**激活——installed_disabled 惰性、skill 不投影。
    /// （新 install 已写 `installedPlugins` 意图 → boot 迁移跳过；enabled plugin 的 boot 恢复由
    /// `reconcile_governance(declared=enabled)` 覆盖——Computer 层无 settings 注入 seam，boot_up 无法 hermetic 传
    /// 启用意图，见本模块 enabledPlugins 测试约定。）
    #[tokio::test]
    async fn boot_up_leaves_installed_disabled_plugin_lazy() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = cold_start_setup95(&tmp).await; // 仅 install（installed_disabled）。

        let comp_b = Computer::new("b", SilentSession::new("s"), None, None, false, false)
            .with_skill_home(home)
            .with_blob_cache_root(tmp.path().join("blob-b"));
        comp_b.boot_up().await.unwrap();

        let skills = comp_b.get_skills().await;
        assert!(
            !skills.iter().any(|s| s.name == "audit:code-review"),
            "install 未 enable → boot_up 不激活（installed_disabled 惰性）"
        );
        comp_b.shutdown().await.unwrap();
    }

    // ── #97：list_mcp_servers_with_metadata 归属 + 活跃 inventory ─────────────────────
    /// 构造一条禁用的用户 stdio server（配置态即可，disabled 免 boot 拉起进程）/ a disabled user server。
    fn user_stdio_server97(name: &str) -> MCPServerConfig {
        MCPServerConfig::Stdio(StdioServerConfig {
            env_file: None,
            name: name.to_string(),
            bundle_id: None,
            disabled: true,
            forbidden_tools: vec![],
            tool_meta: std::collections::HashMap::new(),
            default_tool_meta: None,
            vrl: None,
            server_parameters: StdioServerParameters {
                command: "node".to_string(),
                args: vec![],
                env: std::collections::HashMap::new(),
                cwd: None,
            },
        })
    }

    /// v0.3.0：装（**未 enable**）plugin、以同一 `skill_home` 重建 Computer、boot 后——inventory 返回用户 server
    /// （`managedBy=user`，可从 MCP tab 全权管），但 **不** 返回 installed_disabled plugin 的 bundled server
    /// （§2.4 未启用不投影）。enabled plugin 的 `managedBy=plugin` 归属映射由 recovery/remount 测试覆盖。
    #[tokio::test]
    async fn list_mcp_servers_with_metadata_boot_user_owned_hides_disabled_plugin() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = cold_start_setup95(&tmp).await;

        let mut user_servers = std::collections::HashMap::new();
        user_servers.insert("user-fs".to_string(), user_stdio_server97("user-fs"));

        let comp_b = Computer::new(
            "b",
            SilentSession::new("s"),
            None,
            Some(user_servers),
            false,
            false,
        )
        .with_skill_home(home)
        .with_blob_cache_root(tmp.path().join("blob-b"));
        comp_b.boot_up().await.unwrap();

        let inv = comp_b.list_mcp_servers_with_metadata().await;

        // 用户 server：managedBy=user，可从 MCP tab 全权管理（入口 mcp）。
        let user = inv
            .iter()
            .find(|e| e.name == "user-fs")
            .expect("用户 server 应在 active inventory");
        assert_eq!(user.managed_by, McpOwnership::User);
        assert!(user.disabled, "禁用旗应透传");
        assert!(user.lifecycle.can_edit_from_mcp_tab);
        assert!(user.lifecycle.can_start_from_mcp_tab);
        assert_eq!(user.lifecycle.manage_from, "mcp");

        // v0.3.0：plugin 仅 install（installed_disabled、未 enable）→ 其 bundled server **不**进 active inventory
        // （§2.4 未启用不投影；§4.8「可查询」仅约束 enabled bundled server）。enabled plugin 的 inventory 归属由
        // recovery 层 `collect_returns_enabled_bundled_servers` + Computer 层 remount 测试覆盖。
        assert!(
            !inv.iter().any(|e| e.name == "audit-mcp"),
            "installed_disabled plugin 的 bundled server 不进 inventory"
        );

        comp_b.shutdown().await.unwrap();
    }

    // ── #126：同名用户 MCP vs 插件 bundled server 的归属门控 ────────────────────────────

    /// 本实例 User-config 环境（XDG → tmp/inst-xdg，与宿主进程环境隔离）。
    fn instance_xdg_env_126(tmp: &tempfile::TempDir) -> EnvMap {
        let mut env = EnvMap::new();
        env.insert(
            "XDG_CONFIG_HOME".to_string(),
            tmp.path().join("inst-xdg").to_string_lossy().into_owned(),
        );
        env
    }

    /// 写本实例 User settings（`enabledPlugins` 门控源——与 `enabled_bundled_ownership` 的 `resolve_settings` 同源）。
    fn seed_user_settings_126(env: &EnvMap, settings: serde_json::Value) {
        let path = crate::settings::scope::user_settings_path(Some(env));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_string(&settings).unwrap()).unwrap();
    }

    /// #126 验收#1（复现 issue）：用户在插件**停用**期添加了与插件 bundled server 同名的 server（真实用户声明）；
    /// 随后**启用**插件（用户 server 仍是自己的声明）→ 更新 / 删除该声明**应成功**。此前 bug：`write_target` 按 ledger
    /// bundled 名（未门控）误判 `Synthesized` 拒绝持久化 CRUD。
    #[tokio::test]
    async fn same_name_user_server_editable_and_removable_after_plugin_enabled() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = cold_start_setup95(&tmp).await; // 装 audit@acme（bundling audit-mcp），installed_disabled。
        let env = instance_xdg_env_126(&tmp);
        let proj = tmp.path().join("proj");
        let comp = Computer::new("c", SilentSession::new("s"), None, None, false, false)
            .with_skill_home(home)
            .with_config_env(env.clone())
            .with_config_dir(&proj);

        // 步骤 2：插件停用（未 seed enabledPlugins）→ 归属集空 → 用户添加同名 audit-mcp 成功落盘。
        comp.add_or_update_server(user_stdio_server97("audit-mcp"))
            .await
            .expect("停用插件期同名 server 应可添加");
        assert_eq!(comp.config_revision(), 1);

        // 步骤 3：启用 audit@acme（enabledPlugins 落本实例 User settings）→ audit-mcp 变 enabled-plugin-owned。
        seed_user_settings_126(
            &env,
            serde_json::json!({ "enabledPlugins": { "audit@acme": true } }),
        );

        // 步骤 4：更新用户**自己**的同名声明（改 command）→ 应成功（有 writable 声明，归属门控放行、write_target
        // 编辑其 origin scope），落盘 bump。
        let mut updated = user_stdio_server97("audit-mcp");
        if let MCPServerConfig::Stdio(ref mut s) = updated {
            s.server_parameters.command = "deno".to_string();
        }
        comp.add_or_update_server(updated.clone())
            .await
            .expect("用户自己声明的同名 server 应可更新（不被 bundled 名冲突拦截）");
        assert_eq!(comp.config_revision(), 2, "更新同名用户声明应落盘 bump");

        // 删除用户声明（按 bundle_id）→ 应成功，落盘 bump。
        let bundle_id = crate::mcp_clients::bundle_id::resolve_bundle_id(&updated);
        comp.remove_server(&bundle_id)
            .await
            .expect("用户自己声明的同名 server 应可删除");
        assert_eq!(comp.config_revision(), 3, "删除同名用户声明应落盘 bump");
    }

    /// #131 F3(a)：撞 plugin 基线的用户声明**不再拒写**（推翻 #126 写侧）；`remove` 侧门控**仍在**（F3(b) → #138）。
    ///
    /// 协议 `runtime-contract.md` §2.5 定来源优先序 `plugin 声明 < user < project < local < flag < policy`——plugin
    /// 声明是**最低基线层**，被任何用户侧 scope 覆盖（用户主权）；`guides/mcp-approval-gate-alignment.md` §5 明定
    /// upsert **MUST NOT** 因「同 bundle_id 已由 plugin 提供」拒写。
    ///
    /// 本测同时守护**有意的非对称**：`remove` 在「用户无自有声明 ∧ 该 bundle_id 属启用中插件」时仍拒（导向
    /// `plugin uninstall`），其 origin 判据改造归 #138 —— 故 remove 断言须**先于** add 执行（add 之后用户就有声明了，
    /// remove 会合法放行、测不到该门）。
    #[tokio::test]
    async fn plugin_baseline_add_allowed_remove_still_gated_131() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = cold_start_setup95(&tmp).await;
        let env = instance_xdg_env_126(&tmp);
        let proj = tmp.path().join("proj");
        let comp = Computer::new("c", SilentSession::new("s"), None, None, false, false)
            .with_skill_home(home)
            .with_config_env(env.clone())
            .with_config_dir(&proj);

        // 启用 audit@acme → audit-mcp = enabled-plugin-owned；用户无同名声明。
        seed_user_settings_126(
            &env,
            serde_json::json!({ "enabledPlugins": { "audit@acme": true } }),
        );

        // remove 侧（F3(b) 未改造，仍拒）——必须在 add 之前测，理由见 doc。
        let inv = comp.list_mcp_servers_with_metadata().await;
        let plugin_srv = inv
            .iter()
            .find(|e| e.name == "audit-mcp")
            .expect("enabled bundled server 应可查询（§4.8）");
        let err = comp
            .remove_server(&BundleId::try_from(plugin_srv.bundle_id.clone()).unwrap())
            .await
            .expect_err("用户无自有声明时，删 plugin bundled bundle_id 仍应拒（F3(b) → #138）");
        assert!(matches!(
            err,
            crate::errors::ComputerError::ConfigPersist(_)
        ));
        assert_eq!(comp.config_revision(), 0, "拒绝应零落盘");

        // F3(a)：同 bundle_id 撞 plugin 基线 → **放行**（此前 #126 拒 `Synthesized`），落盘 bump。
        comp.add_or_update_server(user_stdio_server97("audit-mcp"))
            .await
            .expect("撞 plugin 基线的用户声明 MUST NOT 被拒写（#131 F3(a)·指南 §5）");
        assert_eq!(comp.config_revision(), 1, "放行后应落盘 bump");

        // 用户声明既已存在 → 覆盖 plugin 基线；此时 remove 删的是用户自己那条，放行。
        comp.remove_server(&BundleId::try_from(plugin_srv.bundle_id.clone()).unwrap())
            .await
            .expect("用户自有声明存在 → remove 删自己那条，放行");
    }

    /// #127 扫漏：归属 join key = **bundle_id**，非 display 名（订正 #126 自陈的「有意非对称」）。
    ///
    /// 用户声明一个与某启用插件 bundled server **同名、但显式 `bundle_id` 不同**的 server：二者是**不同软件**
    /// （协议 §身份正交性：`name` 允许碰撞、`bundle_id` 才是身份），且 bundled 配置 runtime-only、不落
    /// `mcp.json`（#122）→ 无 name-key 冲突，可合法共存。
    ///
    /// 旧的 name-join 把用户自己的声明误标 `managedBy=plugin`（只读）并拒其 `add_or_update_server`
    /// （`Synthesized`）——**假阳性**；而 `remove_server` 自 #121 起已按 bundle_id 门控 ⇒ 同一场景下
    /// 「查询说只读、删除却放行」自相矛盾。本测锁定 **inventory 归属仍按 bundle_id 分辨**（#127 的核心不变量）。
    ///
    /// #131 F3(a)：原「同一 bundle_id 仍应拒 add」前置守卫**已移除**——协议指南 §5 定 upsert MUST NOT 拒写，
    /// 无论 bundle_id 是否撞 plugin 基线（用户主权覆盖）。写侧「同一身份仍拒」的守护由
    /// [`plugin_baseline_add_allowed_remove_still_gated_131`] 在 **remove 侧**接管。
    #[tokio::test]
    async fn ownership_gate_joins_by_bundle_id_127() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = cold_start_setup95(&tmp).await;
        let env = instance_xdg_env_126(&tmp);
        let proj = tmp.path().join("proj");
        let comp = Computer::new("c", SilentSession::new("s"), None, None, false, false)
            .with_skill_home(home)
            .with_config_env(env.clone())
            .with_config_dir(&proj);

        seed_user_settings_126(
            &env,
            serde_json::json!({ "enabledPlugins": { "audit@acme": true } }),
        );

        // 用户声明：display 名与插件 bundled server 相同，但显式 bundle_id 不同 → 不同软件 → 应放行。
        let mut own = user_stdio_server97("audit-mcp");
        if let MCPServerConfig::Stdio(ref mut c) = own {
            c.bundle_id = Some(bid("user-own-id"));
        }
        comp.add_or_update_server(own).await.expect(
            "同名但 bundle_id 不同 = 不同软件，用户应可声明（旧的 name-join 误拒为 Synthesized）",
        );

        // 查询侧与门控同源：用户自己的声明标 user（可编辑），插件的仍标 plugin（只读）。
        let inv = comp.list_mcp_servers_with_metadata().await;
        let mine = inv
            .iter()
            .find(|e| e.bundle_id == "user-own-id")
            .expect("用户自己的声明应在 inventory 中");
        assert!(
            matches!(mine.managed_by, McpOwnership::User),
            "同名但不同 bundle_id 的用户声明应 user-owned，实得 {:?}",
            mine.managed_by
        );
        let theirs = inv
            .iter()
            .find(|e| e.bundle_id == "audit-mcp")
            .expect("插件 bundled server 仍应可查询（§4.8）");
        assert!(
            matches!(theirs.managed_by, McpOwnership::Plugin { .. }),
            "插件 bundled server 仍应 plugin-owned"
        );
    }

    /// v0.3.0：uninstall 从 `installedPlugins` 全局意图移除该 pid（改 `home` 内文件，hermetic，非 `~/.config`），
    /// 卸载后 inventory 不再出现其 bundled server。（installed_disabled server 本就不在 inventory；本测聚焦
    /// uninstall 对**安装意图**的移除——v0.3.0 权威 install-set。卸载从未 enable 的 plugin 不触碰 `~/.config`，
    /// 见 installer `clear_enabled_plugin` 的存在性守卫。）
    #[tokio::test]
    async fn uninstall_removes_plugin_from_intent_and_inventory() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = cold_start_setup95(&tmp).await; // install → 写 installedPlugins 意图（installed_disabled）。

        // 装后：intent 含 audit@acme。
        assert!(
            crate::settings::store::load_installed_plugins_intent(Some(&home), None)
                .account
                .installed_plugins
                .contains("audit@acme"),
            "install 后 installedPlugins 意图含该 pid"
        );

        let comp_a = Computer::new("a", SilentSession::new("s"), None, None, false, false)
            .with_skill_home(home.clone())
            .with_blob_cache_root(tmp.path().join("blob-a"));
        comp_a
            .uninstall_plugin(
                "audit@acme",
                crate::settings::installer::UninstallOptions::default(),
                None,
            )
            .await
            .unwrap();

        // 卸载后：intent 移除该 pid。
        assert!(
            !crate::settings::store::load_installed_plugins_intent(Some(&home), None)
                .account
                .installed_plugins
                .contains("audit@acme"),
            "uninstall 从 installedPlugins 意图移除该 pid"
        );

        // 以同一 home 重建 Computer B：inventory 不再出现其 bundled server。
        let comp_b = Computer::new("b", SilentSession::new("s"), None, None, false, false)
            .with_skill_home(home)
            .with_blob_cache_root(tmp.path().join("blob-b"));
        let inv = comp_b.list_mcp_servers_with_metadata().await;
        assert!(
            !inv.iter().any(|e| e.name == "audit-mcp"),
            "uninstall 后 plugin bundled server 不应出现在 inventory"
        );
    }

    // ── #94/#95 follow-up：Computer 薄封装的接线（skill_home / 写锁 / mark_skills_dirty）冒烟 ──────
    //
    // ⚠️ disable_plugin / enable_plugin(happy) 不在此做 Computer 级测试：Computer 封装恒用 `env = None`
    // （运行期无注入 seam），二者会把 `enabledPlugins` 写到**真实** user settings（~/.config），污染开发机。
    // 其完整启停语义在 installer 层经注入 env hermetic 覆盖（disable_then_enable_toggles_flag_and_skills 等）。
    // 此处覆盖**不写 settings** 的封装路径：refresh（标脏接线）、enable 错误前置（不写不脏）、空恢复（不脏）。

    /// 🟡4/🟡5：refresh_marketplace 薄封装冒烟——结构化行 + 成功标脏（skill_settlement_pending 探针）。
    #[tokio::test]
    async fn computer_refresh_marketplace_smoke_marks_dirty() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = cold_start_setup95(&tmp).await;
        let comp = Computer::new("b", SilentSession::new("s"), None, None, false, false)
            .with_skill_home(home)
            .with_blob_cache_root(tmp.path().join("blob-b"));
        assert!(!comp.skill_settlement_pending(), "初始无挂起去抖窗口");

        let rows = comp.refresh_marketplace("all").await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "acme");
        // 源未变 → Unchanged（真实 file:// pull，无新提交）。
        assert_eq!(rows[0].status, crate::settings::RefreshStatus::Unchanged);
        // refresh 封装恒标脏 → 去抖窗口挂起（300ms 内同步可见）。
        assert!(
            comp.skill_settlement_pending(),
            "refresh_marketplace 成功应触发 mark_skills_dirty"
        );
    }

    /// 🟡4/🟡5：enable_plugin 薄封装错误前置——未安装 → Precondition，且**不写不脏**（Err 不标脏）。
    #[tokio::test]
    async fn computer_enable_plugin_error_path_does_not_mark_dirty() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let comp = Computer::new("b", SilentSession::new("s"), None, None, false, false)
            .with_skill_home(home)
            .with_blob_cache_root(tmp.path().join("blob"));
        // 未安装 → enable 在写 enabledPlugins 之前即 Precondition 失败（不触真实 settings）。
        let err = comp
            .enable_plugin("ghost@acme", EnableOptions::default(), None)
            .await
            .unwrap_err();
        assert!(matches!(err, PluginInstallError::Precondition(_)));
        assert!(
            !comp.skill_settlement_pending(),
            "失败封装不应 mark_skills_dirty"
        );
    }

    /// 🟡6：空 home reconcile_governance → 0 skills → 不标脏（假分支的 Computer 级断言）。
    #[tokio::test]
    async fn reconcile_governance_empty_home_does_not_mark_dirty() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let comp = Computer::new("b", SilentSession::new("s"), None, None, false, false)
            .with_skill_home(home)
            .with_blob_cache_root(tmp.path().join("blob"));
        let report = comp.reconcile_governance(None, None).await;
        assert!(report.restored_skills.is_empty());
        assert!(
            !comp.skill_settlement_pending(),
            "恢复 0 skills 不应 mark_skills_dirty"
        );
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
    async fn build_without_workdir_and_mark_dirty_no_panic() {
        // #98：Computer 不再持有 workspace（无 with_registered_workdirs / active_workdir）；
        // 构建 + 标脏仍正常。
        let tmp = tempfile::TempDir::new().unwrap();
        let computer = Computer::new("c", SilentSession::new("s"), None, None, false, false)
            .with_skill_home(tmp.path().join("home"))
            .with_blob_cache_root(tmp.path().join("blob"));
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
                bundle_id: None,
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
        // #113 S6：add/remove_server 现落盘 → 注入隔离 config_dir，避免污染进程 cwd / inject isolated config anchor。
        let tmp = tempfile::TempDir::new().unwrap();
        let computer = Computer::new("test_computer", session, None, None, true, true)
            .with_config_dir(tmp.path());

        // 添加服务器配置 / Add server configuration
        let server_config = MCPServerConfig::Stdio(StdioServerConfig {
            env_file: None,
            name: "test_server".to_string(),
            bundle_id: None,
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
            bundle_id: None,
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
        computer
            .remove_server(&BundleId::try_from("test_server".to_string()).unwrap())
            .await
            .unwrap();
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
            bundle_id: None,
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
            bundle_id: None,
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

    // ── #112 S5：D1 运行期 resolver 契约 + 结构化缺失 / D1 runtime resolver contract + structured missing ──

    /// 测试用 map-backed input resolver / test double。
    struct MapInputResolver(HashMap<String, serde_json::Value>);
    #[async_trait]
    impl crate::inputs::runtime_resolver::InputValueResolver for MapInputResolver {
        async fn resolve_input(
            &self,
            def: &MCPServerInput,
        ) -> Result<Option<serde_json::Value>, InputResolutionError> {
            Ok(self.0.get(def.id()).cloned())
        }
    }

    /// 测试用 map-backed secret resolver / test double。
    struct MapSecretResolver(HashMap<String, String>);
    #[async_trait]
    impl crate::inputs::runtime_resolver::SecretValueResolver for MapSecretResolver {
        async fn resolve_secret(
            &self,
            def: &MCPServerInput,
        ) -> Result<Option<String>, InputResolutionError> {
            Ok(self.0.get(def.id()).cloned())
        }
    }

    fn prompt_def(id: &str, default: Option<&str>, password: bool) -> MCPServerInput {
        MCPServerInput::PromptString(PromptStringInput {
            id: id.to_string(),
            description: String::new(),
            default: default.map(|s| s.to_string()),
            password: Some(password),
        })
    }

    fn stdio_with_arg(arg: &str) -> MCPServerConfig {
        MCPServerConfig::Stdio(StdioServerConfig {
            env_file: None,
            name: "s".to_string(),
            bundle_id: None,
            disabled: false,
            forbidden_tools: vec![],
            tool_meta: std::collections::HashMap::new(),
            default_tool_meta: None,
            vrl: None,
            server_parameters: StdioServerParameters {
                command: "echo".to_string(),
                args: vec![arg.to_string()],
                env: std::collections::HashMap::new(),
                cwd: None,
            },
        })
    }

    fn rendered_arg0(cfg: MCPServerConfig) -> String {
        match cfg {
            MCPServerConfig::Stdio(c) => c.server_parameters.args[0].clone(),
            _ => panic!("Expected Stdio config"),
        }
    }

    #[tokio::test]
    async fn render_errors_structured_when_no_default_no_resolver_referenced() {
        // D1 验收：引用到「已定义、无默认值、无 resolver/env」的 input → 结构化 InputResolution（非静默空串）。
        let mut inputs = HashMap::new();
        inputs.insert("s5_tok".to_string(), prompt_def("s5_tok", None, false));
        let computer = Computer::new("c", SilentSession::new("t"), Some(inputs), None, true, true);
        let err = computer
            .render_server_config(&stdio_with_arg("${input:s5_tok}"))
            .await
            .unwrap_err();
        match &err {
            ComputerError::InputResolution(InputResolutionError::Missing { id, .. }) => {
                assert_eq!(id, "s5_tok");
            }
            other => panic!("expected structured InputResolution::Missing, got {other:?}"),
        }
        assert_eq!(err.error_code(), 400);
    }

    #[tokio::test]
    async fn render_stamps_raw_derived_bundle_id_for_nameless_server() {
        // 协议 0.3.0 §connection-identity=raw（a2c-smcp-protocol#17）：无名 server 的 bundle_id 缺省生成 MUST 用
        // **未渲染**连接身份（`${input:*}` 占位字面）。render_server_config 须 stamp 从 raw 派生的 bundle_id，
        // **即便 input 已成功解析**。期望值 = 一致性向量「无名 + stdio env 含 ${input:*}」的 raw 值。
        let mut inputs = HashMap::new();
        inputs.insert("api_key".to_string(), prompt_def("api_key", None, false));
        let mut m = HashMap::new();
        m.insert(
            "api_key".to_string(),
            serde_json::Value::String("secret-xyz".to_string()),
        );
        let computer = Computer::new("c", SilentSession::new("t"), Some(inputs), None, true, true)
            .with_input_resolver(Arc::new(MapInputResolver(m)));

        // 无名 stdio（name=""），env 含 ${input:api_key} 占位——与协议向量同构。
        let mut env = HashMap::new();
        env.insert("API_KEY".to_string(), "${input:api_key}".to_string());
        let raw = MCPServerConfig::Stdio(StdioServerConfig::new(
            "",
            StdioServerParameters {
                command: "node".to_string(),
                args: vec!["server.js".to_string()],
                env,
                cwd: None,
            },
        ));

        let validated = computer.render_server_config(&raw).await.unwrap();
        // stamped bundle_id 来自 raw 占位字面（raw 决策），而非渲染后值。
        assert_eq!(
            validated.bundle_id().map(BundleId::as_str),
            Some("bundle_68ae00fea9122c01"),
            "无名 server 应 stamp raw 派生 bundle_id（占位字面），非渲染后值"
        );
        // 佐证渲染确实替换了占位（证明 raw≠rendered，测试有区分力）。
        match &validated {
            MCPServerConfig::Stdio(vc) => assert_eq!(
                vc.server_parameters.env.get("API_KEY").map(String::as_str),
                Some("secret-xyz")
            ),
            _ => panic!("expected stdio"),
        }
    }

    #[tokio::test]
    async fn render_resolves_no_default_input_via_injected_resolver() {
        // D1：无默认值 input 经 client input_resolver 取值（SDK 不落盘明文）。
        let mut inputs = HashMap::new();
        inputs.insert("s5_url".to_string(), prompt_def("s5_url", None, false));
        let mut m = HashMap::new();
        m.insert(
            "s5_url".to_string(),
            serde_json::Value::String("https://injected".to_string()),
        );
        let computer = Computer::new("c", SilentSession::new("t"), Some(inputs), None, true, true)
            .with_input_resolver(Arc::new(MapInputResolver(m)));
        let rendered = computer
            .render_server_config(&stdio_with_arg("${input:s5_url}"))
            .await
            .unwrap();
        assert_eq!(rendered_arg0(rendered), "https://injected");
    }

    #[tokio::test]
    async fn render_resolves_secret_via_injected_secret_resolver() {
        // D1：password:true input 经 client secret_resolver 取值（SDK 不落盘 secret 明文）。
        let mut inputs = HashMap::new();
        inputs.insert("s5_key".to_string(), prompt_def("s5_key", None, true));
        let mut m = HashMap::new();
        m.insert("s5_key".to_string(), "sk-injected".to_string());
        let computer = Computer::new("c", SilentSession::new("t"), Some(inputs), None, true, true)
            .with_secret_resolver(Arc::new(MapSecretResolver(m)));
        let rendered = computer
            .render_server_config(&stdio_with_arg("${input:s5_key}"))
            .await
            .unwrap();
        assert_eq!(rendered_arg0(rendered), "sk-injected");
    }

    #[tokio::test]
    async fn render_tolerates_unreferenced_unresolvable_input() {
        // 容忍：全局池里有个无法解析的 input，但本 server 不引用它 → 渲染照常成功（不误伤）。
        let mut inputs = HashMap::new();
        inputs.insert(
            "s5_orphan".to_string(),
            prompt_def("s5_orphan", None, false),
        );
        inputs.insert(
            "s5_used".to_string(),
            prompt_def("s5_used", Some("U"), false),
        );
        let computer = Computer::new("c", SilentSession::new("t"), Some(inputs), None, true, true);
        // 仅引用 s5_used（有默认值）；s5_orphan 无法解析但未被引用。
        let rendered = computer
            .render_server_config(&stdio_with_arg("${input:s5_used}"))
            .await
            .unwrap();
        assert_eq!(rendered_arg0(rendered), "U");
    }

    /// 恒硬失败的 input resolver / always-failing test double。
    struct FailingInputResolver;
    #[async_trait]
    impl crate::inputs::runtime_resolver::InputValueResolver for FailingInputResolver {
        async fn resolve_input(
            &self,
            def: &MCPServerInput,
        ) -> Result<Option<serde_json::Value>, InputResolutionError> {
            Err(InputResolutionError::resolver_failed(def.id(), "boom"))
        }
    }

    #[tokio::test]
    async fn render_propagates_resolver_hard_failure() {
        // resolver 侧硬失败（Err）→ 引用取用时 propagate（区别于「未提供」的 Ok(None) 回退）。
        let mut inputs = HashMap::new();
        inputs.insert("s5_fail".to_string(), prompt_def("s5_fail", None, false));
        let computer = Computer::new("c", SilentSession::new("t"), Some(inputs), None, true, true)
            .with_input_resolver(Arc::new(FailingInputResolver));
        let err = computer
            .render_server_config(&stdio_with_arg("${input:s5_fail}"))
            .await
            .unwrap_err();
        match &err {
            ComputerError::InputResolution(InputResolutionError::ResolverFailed { id, .. }) => {
                assert_eq!(id, "s5_fail");
            }
            other => panic!("expected ResolverFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn render_secret_input_not_resolved_by_input_resolver() {
        // 安全隔离：仅注入 input_resolver（含同 id 值），input 为 password:true → secret 绝不走 input_resolver，
        // 落到结构化 Missing{kind:Secret}（而非泄漏 input_resolver 里的值）。
        let mut inputs = HashMap::new();
        inputs.insert(
            "s5_sec_iso".to_string(),
            prompt_def("s5_sec_iso", None, true),
        );
        let mut m = HashMap::new();
        m.insert(
            "s5_sec_iso".to_string(),
            serde_json::Value::String("LEAKED".to_string()),
        );
        let computer = Computer::new("c", SilentSession::new("t"), Some(inputs), None, true, true)
            .with_input_resolver(Arc::new(MapInputResolver(m)));
        let err = computer
            .render_server_config(&stdio_with_arg("${input:s5_sec_iso}"))
            .await
            .unwrap_err();
        match &err {
            ComputerError::InputResolution(InputResolutionError::Missing { id, kind, .. }) => {
                assert_eq!(id, "s5_sec_iso");
                assert_eq!(*kind, InputKind::Secret);
            }
            other => panic!("expected Missing(Secret), got {other:?}"),
        }
        assert!(
            !format!("{err}").contains("LEAKED"),
            "input_resolver 值绝不得泄漏到 secret 解析"
        );
    }

    /// #140：注册期 env 名坍缩 fail-fast——`a-b` 与 `a_b` 经 ENV_SEGMENT 归一到同一 `A2C_SMCP_a_b`
    /// ⇒ boot 硬错（否则静默串味、后写的赢，含密钥）。检查在 boot_up 首步、早于任何 FS 副作用。
    #[tokio::test]
    async fn boot_fails_on_env_name_collision_140() {
        let mut inputs = HashMap::new();
        inputs.insert("a-b".to_string(), prompt_def("a-b", None, false));
        inputs.insert("a_b".to_string(), prompt_def("a_b", None, false));
        let computer = Computer::new(
            "c",
            SilentSession::new("t"),
            Some(inputs),
            None,
            false,
            false,
        );
        let res = computer.boot_up().await;
        let err = res.expect_err("两 input id 坍缩同一 env 名 MUST boot fail-fast");
        let msg = format!("{err}");
        assert!(
            msg.contains("A2C_SMCP_a_b") && msg.to_lowercase().contains("collide"),
            "错误须指明坍缩到同一 env 名：{msg}"
        );
    }

    // ── #144：boot_up 须把 D1 结构化 InputResolution 上抛（非仅日志），对齐 mount_server ──────────

    /// 构造单个引用 `${input:<id>}` 的 stdio server 的 mcp_servers 映射（boot_up 读 self.mcp_servers 渲染）。
    fn one_server_referencing(arg: &str) -> HashMap<String, MCPServerConfig> {
        HashMap::from([("s".to_string(), stdio_with_arg(arg))])
    }

    #[tokio::test]
    async fn boot_up_propagates_missing_value_input() {
        // #144：已定义、无默认值、无 resolver/env 的 value input → boot_up 上抛 InputResolution::Missing（非 Ok+日志）。
        let mut inputs = HashMap::new();
        inputs.insert("b144_val".to_string(), prompt_def("b144_val", None, false));
        let computer = Computer::new(
            "c",
            SilentSession::new("t"),
            Some(inputs),
            Some(one_server_referencing("${input:b144_val}")),
            false,
            false,
        );
        let err = computer
            .boot_up()
            .await
            .expect_err("boot_up MUST propagate missing-value InputResolution");
        match &err {
            ComputerError::InputResolution(InputResolutionError::Missing { id, kind, .. }) => {
                assert_eq!(id, "b144_val");
                assert_eq!(*kind, InputKind::Value);
            }
            other => panic!("expected InputResolution::Missing, got {other:?}"),
        }
        assert_eq!(err.error_code(), 400);
    }

    #[tokio::test]
    async fn boot_up_propagates_missing_secret_input() {
        // #144：password:true secret 缺失 → Missing{kind:Secret}。Missing 结构体无值字段 ⇒ 安全验收：错误天然不含明文。
        let mut inputs = HashMap::new();
        inputs.insert("b144_sec".to_string(), prompt_def("b144_sec", None, true));
        let computer = Computer::new(
            "c",
            SilentSession::new("t"),
            Some(inputs),
            Some(one_server_referencing("${input:b144_sec}")),
            false,
            false,
        );
        let err = computer
            .boot_up()
            .await
            .expect_err("boot_up MUST propagate missing-secret InputResolution");
        match &err {
            ComputerError::InputResolution(InputResolutionError::Missing {
                id,
                kind,
                env_hint,
            }) => {
                assert_eq!(id, "b144_sec");
                assert_eq!(*kind, InputKind::Secret);
                // env_hint 为补录变量名（非 secret 明文）。
                assert_eq!(env_hint, &env_var_name("b144_sec"));
            }
            other => panic!("expected Missing(Secret), got {other:?}"),
        }
        // 错误文案不得含任何疑似明文（此处 secret 本就缺失，确保无泄漏路径）。
        assert!(!format!("{err}").contains("password"));
        assert_eq!(err.error_code(), 400);
    }

    #[tokio::test]
    async fn boot_up_propagates_resolver_hard_failure() {
        // #144：client resolver 硬失败（Err）→ boot_up 上抛 ResolverFailed（区别于 Ok(None) 的 Missing 回退）。
        let mut inputs = HashMap::new();
        inputs.insert(
            "b144_fail".to_string(),
            prompt_def("b144_fail", None, false),
        );
        let computer = Computer::new(
            "c",
            SilentSession::new("t"),
            Some(inputs),
            Some(one_server_referencing("${input:b144_fail}")),
            false,
            false,
        )
        .with_input_resolver(Arc::new(FailingInputResolver));
        let err = computer
            .boot_up()
            .await
            .expect_err("boot_up MUST propagate resolver hard-failure");
        match &err {
            ComputerError::InputResolution(InputResolutionError::ResolverFailed { id, reason }) => {
                assert_eq!(id, "b144_fail");
                assert!(reason.contains("boom"), "reason 须透传：{reason}");
            }
            other => panic!("expected ResolverFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn boot_up_input_resolution_sets_error_status() {
        // #144：失败须落 Error 状态 + last_error（对齐 manager.initialize 失败路径）——boot 可安全重试、观测面反映失败，
        // 而非卡在 Starting。失败发生在 render 循环（commit/spawn/watcher 之前）⇒ 无残留 manager/task/transport。
        let mut inputs = HashMap::new();
        inputs.insert(
            "b144_state".to_string(),
            prompt_def("b144_state", None, false),
        );
        let computer = Computer::new(
            "c",
            SilentSession::new("t"),
            Some(inputs),
            Some(one_server_referencing("${input:b144_state}")),
            false,
            false,
        );
        assert!(
            computer.boot_up().await.is_err(),
            "boot MUST fail on missing input"
        );
        let snap = computer.status().await;
        assert_eq!(
            snap.lifecycle,
            LifecycleState::Error,
            "失败后状态须为 Error（非卡 Starting）"
        );
        assert!(
            snap.last_error.is_some(),
            "last_error 须落诊断（仅错误类别串、不含 secret）"
        );
    }

    #[tokio::test]
    async fn boot_up_retry_succeeds_after_resolver_provides_value() {
        // #144：boot 失败后生命周期可安全重试——同一 Computer 在值可得后再次 boot 成功（无残留状态阻塞）。
        // Error→Starting 迁移被允许（仅 Shutdown 终态拒绝），故首轮失败后可直挂重试。
        let id = "b144_retry";
        let var = env_var_name(id); // A2C_SMCP_b144_retry（#140 保留大小写）
        std::env::remove_var(&var); // 确保首轮缺失
        let mut inputs = HashMap::new();
        inputs.insert(id.to_string(), prompt_def(id, None, false));
        let tmp = tempfile::TempDir::new().unwrap();
        let computer = Computer::new(
            "c",
            SilentSession::new("t"),
            Some(inputs),
            Some(one_server_referencing(&format!("${{input:{id}}}"))),
            false,
            false,
        )
        .with_skill_home(tmp.path().join("skills"))
        .with_blob_cache_root(tmp.path().join("blob"))
        .with_config_dir(tmp.path().join("config"));

        // 首轮：值缺失 → boot 失败（InputResolution），状态落 Error。
        assert!(
            computer.boot_up().await.is_err(),
            "首轮 boot 须因 missing input 失败"
        );
        assert_eq!(computer.status().await.lifecycle, LifecycleState::Error);

        // 提供值（env 回退路径）→ 同一 Computer 重试成功（证明无残留 manager/task/transport 阻塞重试）。
        std::env::set_var(&var, "provided");
        let second = computer.boot_up().await;
        std::env::remove_var(&var);
        second.expect("重试 boot 须在值可得后成功（无残留状态阻塞）");
    }

    #[tokio::test]
    async fn boot_up_tolerates_undefined_placeholder() {
        // #144：未定义占位符（不在 inputs 池）≠ 已定义但解析失败。前者保留字面、不上抛（VS Code parity），
        // 仅后者（InputResolution）上抛。本测试守护「不连坐误伤」：无 input 定义时 boot 不应失败。
        let tmp = tempfile::TempDir::new().unwrap();
        let computer = Computer::new(
            "c",
            SilentSession::new("t"),
            None, // 无 inputs 定义 → b144_undef 为未定义占位符
            Some(one_server_referencing("${input:b144_undef}")),
            false,
            false,
        )
        .with_skill_home(tmp.path().join("skills"))
        .with_blob_cache_root(tmp.path().join("blob"))
        .with_config_dir(tmp.path().join("config"));
        computer
            .boot_up()
            .await
            .expect("undefined placeholder MUST NOT fail boot（字面保留）");
    }

    #[tokio::test]
    async fn render_resolves_input_via_env_fallback() {
        // env `A2C_SMCP_<ENV_SEGMENT(id)>` 回退（= 豁免无损迁移后用户重新提供值的迁移路径）。用唯一 id 避免跨测污染。
        let id = "s5_env_hit_uid";
        let var = env_var_name(id); // A2C_SMCP_s5_env_hit_uid（#140：保留大小写）
        std::env::set_var(&var, "from-env-fallback");
        let mut inputs = HashMap::new();
        inputs.insert(id.to_string(), prompt_def(id, None, false));
        let computer = Computer::new("c", SilentSession::new("t"), Some(inputs), None, true, true);
        let rendered = computer
            .render_server_config(&stdio_with_arg(&format!("${{input:{id}}}")))
            .await;
        std::env::remove_var(&var);
        assert_eq!(rendered_arg0(rendered.unwrap()), "from-env-fallback");
    }

    #[tokio::test]
    async fn render_resolves_command_input_via_session() {
        // Command input：经 session subprocess 执行、渲染其输出（resolve_one_input 的 Command 分支）。
        let mut inputs = HashMap::new();
        inputs.insert(
            "s5_cmd".to_string(),
            MCPServerInput::Command(CommandInput {
                id: "s5_cmd".to_string(),
                description: String::new(),
                command: "echo cmd-out".to_string(),
                args: None,
            }),
        );
        let computer = Computer::new("c", SilentSession::new("t"), Some(inputs), None, true, true);
        let rendered = computer
            .render_server_config(&stdio_with_arg("${input:s5_cmd}"))
            .await
            .unwrap();
        assert_eq!(rendered_arg0(rendered), "cmd-out");
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

    /// #127 扫漏（根因）：`Computer` 的运行期投影 `mcp_servers` 按 **bundle_id** 键，非 display 名。
    ///
    /// 两个 display 名相同、`bundle_id` 不同的合法共存 server（协议：`name` 允许碰撞）：旧的 name-keyed
    /// 投影会**折叠**（后写覆盖先写）→ `list_mcp_servers` 少一条；且 `get_server_status`（Vec，两行都在）
    /// 与 `materialized_server_bundle_ids`（name-keyed map，折叠）**按名 join** → CLI `status` 给两行打
    /// 同一个 bundle_id，用户照 `server rm <bundle_id>` 提示操作会**删错 server**，另一个从 CLI 无从寻址。
    #[tokio::test]
    async fn runtime_projection_keys_by_bundle_id_127() {
        use crate::mcp_clients::manager::test_support::stdio_cfg_with_bundle;

        let computer = Computer::new("c", SilentSession::new("s"), None, None, false, false);
        *computer.mcp_manager.write().await = Some(MCPServerManager::new());
        computer
            .mount_server(stdio_cfg_with_bundle("same-display-name", Some("id-a")))
            .await
            .unwrap();
        computer
            .mount_server(stdio_cfg_with_bundle("same-display-name", Some("id-b")))
            .await
            .unwrap();

        assert_eq!(
            computer.list_mcp_servers().await.len(),
            2,
            "两个同名不同 bundle_id 的 server 均须在投影中（旧实现折叠成 1 条）"
        );

        let mut ids: Vec<BundleId> = computer
            .list_mcp_servers()
            .await
            .iter()
            .map(crate::mcp_clients::bundle_id::resolve_bundle_id)
            .collect();
        ids.sort();
        assert_eq!(ids, vec![bid("id-a"), bid("id-b")], "两条身份须各自可辨");

        // status 每行自带身份键 —— CLI 无需再按 name join（那个 join 正是误删的根源）。
        let mut status = computer.get_server_status().await;
        status.sort_by(|a, b| a.0.cmp(&b.0));
        let status_ids: Vec<&str> = status.iter().map(|(bid, ..)| bid.as_str()).collect();
        assert_eq!(
            status_ids,
            vec!["id-a", "id-b"],
            "status 两行须各自带可寻址的 bundle_id（旧实现两行同一个 id → server rm 删错人）"
        );
        for (_, name, ..) in &status {
            assert_eq!(name, "same-display-name", "展示名保留（display 非身份）");
        }
    }

    /// #127 隔离审查 🔴：`Computer::new` 的 `mcp_servers` 入参键**不被采信**，投影键由 config 自身派生。
    ///
    /// 外部 embedder 一贯从 name-keyed 的 `mcp.json`（协议 §9.1）播种。若原样搬入 bundle_id-keyed 投影，
    /// 键 ≠ 真身份时（此处 display 名 `my.api` → 真 bundle_id `my_api`）会全面污染下游：inventory 出线
    /// **错的** `bundle_id` → 用户拿它 `remove_server` 永不命中、删不掉；归属 join 亦 miss → plugin-owned
    /// 被误标 `user`。`BundleId = String` 是类型别名 ⇒ 编译期零信号，只能靠本测试守。
    #[tokio::test]
    async fn new_rekeys_seeded_projection_by_bundle_id_127() {
        use crate::mcp_clients::manager::test_support::stdio_cfg_with_bundle;

        // 调用方按旧契约用 display 名作键；`my.api` 的真身份是 `my_api`（缺省生成规范化 `.`→`_`）。
        let mut seeded = HashMap::new();
        seeded.insert("my.api".to_string(), stdio_cfg_with_bundle("my.api", None));
        // 另一条显式 bundle_id ≠ 其 display 名。
        seeded.insert(
            "display-name".to_string(),
            stdio_cfg_with_bundle("display-name", Some("explicit-id")),
        );

        let computer = Computer::new(
            "c",
            SilentSession::new("s"),
            None,
            Some(seeded),
            false,
            false,
        );

        let inv = computer.list_mcp_servers_with_metadata().await;
        let mut ids: Vec<&str> = inv.iter().map(|e| e.bundle_id.as_str()).collect();
        ids.sort();
        assert_eq!(
            ids,
            vec!["explicit-id", "my_api"],
            "投影键须由 config 派生（`my.api`→`my_api`），而非采信调用方给的 display 名键"
        );
        // 出线的 bundle_id 必须真能用于寻址（`remove_server` 按 bundle_id 比对 resolve_bundle_id）。
        for e in &inv {
            assert!(
                computer.list_mcp_servers().await.iter().any(|c| {
                    crate::mcp_clients::bundle_id::resolve_bundle_id(c).as_str()
                        == e.bundle_id.as_str()
                }),
                "inventory 出线的 bundle_id {} 须可寻址到实际 config",
                e.bundle_id
            );
        }
    }

    /// #127 隔离审查 🟡1：**boot 前** `unmount_server` 仍须真正停摘（manager 未建 → 回退按投影解析身份）。
    ///
    /// 与 `mount_server` 的 boot 前可挂载对称。若此路径静默 no-op，`boot_up` 会照样把已被显式停摘的
    /// server 拉起来。
    #[tokio::test]
    async fn unmount_server_works_before_boot_127() {
        use crate::mcp_clients::manager::test_support::stdio_cfg_with_bundle;

        let cfg = stdio_cfg_with_bundle("my.api", None);
        // display 名含 `.` ⇒ 不是合法 bundle_id 字面量；身份键须由同一份 config 派生（#141）。
        let cfg_bundle_id = crate::mcp_clients::bundle_id::resolve_bundle_id(&cfg);
        let mut seeded = HashMap::new();
        seeded.insert("my.api".to_string(), cfg);
        let computer = Computer::new(
            "c",
            SilentSession::new("s"),
            None,
            Some(seeded),
            false,
            false,
        );
        // 前置：manager 尚未建（未 boot / 未挂载）。
        assert!(!computer.is_mcp_manager_initialized().await);
        assert_eq!(computer.list_mcp_servers().await.len(), 1);

        // 按 bundle_id 停摘（#122 plugin-hook 契约的公开面；#141 由 display 名改身份键）→ 投影须真的少一条。
        computer.unmount_server(&cfg_bundle_id).await.unwrap();
        assert!(
            computer.list_mcp_servers().await.is_empty(),
            "boot 前 unmount_server 须真正停摘（否则 boot_up 会把它重新拉起）"
        );

        // 未注册的名字 → 幂等 no-op，不 panic、不报错。
        computer
            .unmount_server(&BundleId::try_from("never-declared".to_string()).unwrap())
            .await
            .unwrap();
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
            &bid("srv"),
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
            &bid("tfrobot-tools"),
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

    /// #127 验收：mcp SKILL 的 `name` / `source` / 磁盘落点的 `<server>` 段 = **`bundle_id` 原样**。
    ///
    /// 三个 server 覆盖协议 skill.md §1.6 的关键分支，且前两者**故意共用 display 名**（协议：`name`
    /// 允许碰撞、永不做键）：显式 `bundle_id=acme-editor` / 显式 `bundle_id=id-b` / CJK display 名
    /// （缺省生成走 `bundle_<16hex>` hash fallback）。三者暴露的 SKILL frontmatter `name` **全为
    /// `real-name`**——故合成 name 是否碰撞**完全取决于 `<server>` 段取值**，这正是本测试要锁的。
    ///
    /// 旧实现取规范化 display 名：前两者撞出同一个 `mcp:same-display-name:real-name`，后到者被
    /// `seen_this_run` 丢弃 → 一个**合法 SKILL 对 Agent 隐身**；CJK 名则退化成 `mcp:___:real-name`。
    #[tokio::test]
    async fn restage_mcp_skills_uses_bundle_id_segment_127() {
        use crate::mcp_clients::bundle_id::derive_bundle_id;
        use crate::mcp_clients::manager::test_support::{
            inject, inject_config, skill_resource_mounted, stdio_cfg_with_bundle, MockSkillClient,
        };

        let tmp = tempfile::TempDir::new().unwrap();
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
        computer.boot_up().await.unwrap();

        // CJK display 名 → normalize_name 结果为空 → 缺省生成走 sha256 fallback。
        let cjk_cfg = stdio_cfg_with_bundle("服务器", None);
        let cjk_bundle = derive_bundle_id(&cjk_cfg);
        assert!(
            cjk_bundle.as_str().starts_with("bundle_"),
            "CJK display 名应触发 hash fallback，实得 {cjk_bundle}"
        );

        let mgr = MCPServerManager::new();
        for (bundle_id, cfg) in [
            (
                bid("acme-editor"),
                stdio_cfg_with_bundle("same-display-name", Some("acme-editor")),
            ),
            (
                bid("id-b"),
                stdio_cfg_with_bundle("same-display-name", Some("id-b")),
            ),
            (cjk_bundle.clone(), cjk_cfg),
        ] {
            inject_config(&mgr, &bundle_id, cfg).await;
            inject(
                &mgr,
                &bundle_id,
                MockSkillClient {
                    pages: vec![vec![skill_resource_mounted(
                        &format!("skill://h.example.com/{bundle_id}"),
                        Some("mounted"),
                        mount.to_str().unwrap(),
                    )]],
                    fail: false,
                    cap_fail: false,
                    read_text: String::new(),
                },
            )
            .await;
        }
        *computer.mcp_manager.write().await = Some(mgr);

        let mut registered = computer.restage_mcp_skills(None).await;
        registered.sort();
        assert_eq!(
            registered,
            vec![
                "mcp:acme-editor:real-name".to_string(),
                format!("mcp:{cjk_bundle}:real-name"),
                "mcp:id-b:real-name".to_string(),
            ],
            "三个 server 的 SKILL 均须可见：`<server>` 段取 bundle_id 后构造上不碰撞"
        );

        // source 与磁盘落点同取 bundle_id：两个同名 server 的 staged 目录不再互相覆盖。
        let skills = computer.get_skills().await;
        for bundle_id in ["acme-editor", "id-b", cjk_bundle.as_str()] {
            let name = format!("mcp:{bundle_id}:real-name");
            let r = skills
                .iter()
                .find(|s| s.name == name)
                .unwrap_or_else(|| panic!("{name} 应对 Agent 可见"));
            assert_eq!(r.source, format!("mcp:{bundle_id}"));
            assert!(
                r.path.ends_with(&format!("mcp/{bundle_id}/real-name")),
                "磁盘落点应按 bundle_id 分组，实得 {}",
                r.path
            );
        }

        // 定向重挂按 bundle_id 寻址；display 名不再是寻址键。
        assert_eq!(
            computer.restage_mcp_skills(Some("acme-editor")).await,
            vec!["mcp:acme-editor:real-name".to_string()]
        );
        assert!(
            computer
                .restage_mcp_skills(Some("same-display-name"))
                .await
                .is_empty(),
            "display 名非寻址键，不应命中任何 server"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_no_abba_deadlock_governance_vs_reactor_skill_restage() {
        // #106 回归守卫：反应器 skill 重挂（`mcp_manager.read` → `skill_registry.write`）与 governance
        // `add_or_update_server`（`skill_registry.write` → `mcp_manager` 锁）曾构成 ABBA 死锁。修复把
        // `add_or_update_server` 惰性初始化改为「先 read 探测、仅 None 才 write」，post-boot 只取
        // `mcp_manager.read`，与反应器读锁相容。守卫：多轮并发下两侧均须在超时内完成（未修复则死锁 → 超时 panic）。
        use crate::mcp_clients::manager::test_support::{
            inject, skill_resource_mounted, MockSkillClient,
        };

        let tmp = tempfile::TempDir::new().unwrap();
        let mount = tmp.path().join("mount");
        std::fs::create_dir_all(&mount).unwrap();
        std::fs::write(
            mount.join("SKILL.md"),
            "---\nname: real-name\ndescription: d\n---\nbody",
        )
        .unwrap();

        let computer = Computer::new("c", SilentSession::new("s"), None, None, false, false)
            .with_skill_home(tmp.path().join("home"))
            .with_blob_cache_root(tmp.path().join("blob"));
        computer.boot_up().await.unwrap();

        let mgr = MCPServerManager::new();
        inject(
            &mgr,
            &bid("tfrobot-tools"),
            MockSkillClient {
                pages: vec![vec![skill_resource_mounted(
                    "skill://tfrobot-tools/leaf",
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

        let computer = Arc::new(computer);
        let cfg = MCPServerConfig::Stdio(crate::mcp_clients::model::StdioServerConfig {
            env_file: None,
            name: "gov".to_string(),
            bundle_id: None,
            disabled: false,
            forbidden_tools: vec![],
            tool_meta: HashMap::new(),
            default_tool_meta: None,
            vrl: None,
            server_parameters: crate::mcp_clients::model::StdioServerParameters {
                command: "echo".to_string(),
                args: vec![],
                env: HashMap::new(),
                cwd: None,
            },
        });

        for _ in 0..25 {
            let ca = Arc::clone(&computer);
            let cfg_a = cfg.clone();
            let a = tokio::spawn(async move {
                // governance 侧：持 `skill_registry` 写锁跨对 `mcp_manager` 的访问。#113 S6：治理重挂现经
                // **运行期物化** `mount_server`（hooks.register_server 的落点；不落盘），与真实 governance 路径一致。
                let reg = ca.skill_registry_arc();
                let g = reg.write().await;
                tokio::task::yield_now().await; // 给 B 抢 mcp_manager.read 的窗口，最大化命中旧 ABBA
                let _ = ca.mount_server(cfg_a).await;
                drop(g);
            });
            let cb = Arc::clone(&computer);
            let b = tokio::spawn(async move {
                // reactor 侧：`mcp_manager.read` → `skill_registry.write`（经 on_skills_changed(None)）。
                cb.handle_mcp_notification(McpServerNotification {
                    server: bid("tfrobot-tools"),
                    kind: McpChangeKind::ResourceListChanged,
                })
                .await;
            });
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                let _ = a.await;
                let _ = b.await;
            })
            .await
            .expect("ABBA 死锁：governance 与 reactor skill 重挂未在 5s 内完成");
        }
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
        // 本测试断言的是 **rmcp 层**直接序列化形态：rmcp CallToolResult.meta 为 `#[serde(rename="_meta")]`
        // （无条件），故此处必为 `_meta`。协议规范的 wire `meta` 由 ack 边界 `promote_result_meta_to_meta`
        // 重映射产生（#92），其出线断言见 socketio_client.rs `test_promote_result_meta_*`。
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
        // 同上：此为 rmcp 层 `_meta`；wire `meta` 由 promote_result_meta_to_meta 重映射（#92）。
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
        // 假 client 睡 10s + 50ms timeout → manager 级超时 → 回填超时态结果（rmcp 层 _meta.a2c_timeout）+ 历史。
        let computer =
            computer_with_cancel_mock(CancelBehavior::Sleep(Duration::from_secs(10))).await;

        let result = computer
            .execute_tool_cancellable("rid-timeout", "t", serde_json::json!({}), Some(0.05))
            .await
            .expect("超时应回填结果而非上抛");

        // execute_tool_cancellable 返回 rmcp CallToolResult，直接序列化为 `_meta`（rename）。
        // 协议规范的 wire `meta` 由 ack 边界 promote_result_meta_to_meta 重映射（#92，出线断言见
        // socketio_client.rs `test_promote_result_meta_*`）。
        let v = serde_json::to_value(&result).unwrap();
        assert_eq!(
            v["_meta"]["a2c_timeout"], true,
            "rmcp 层超时态须写 _meta.a2c_timeout（wire `meta` 由 promote 重映射）"
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
