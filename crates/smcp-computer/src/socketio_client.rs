/*!
* 文件名: socketio_client
* 作者: JQQ
* 创建日期: 2025/12/16
* 最后修改日期: 2025/12/16
* 版权: 2023 JQQ. All rights reserved.
* 依赖: tf_rust_socketio, tokio, serde
* 描述: SMCP Computer的Socket.IO客户端实现 / Socket.IO client implementation for SMCP Computer
*/

use crate::blob::encode_skill_handle;
use crate::computer::{ComputerHandlerOps, SocketIoAuthProvider};
use crate::desktop::{organize_desktop, WindowInfo};
use crate::errors::{ComputerError, ComputerResult};
use crate::mcp_clients::manager::MCPServerManager;
use crate::mcp_clients::model::MCPServerInput;
use crate::skills::naming::parse_skill_name;
use crate::status::{LifecycleState, RuntimeStatus};
use base64::Engine as _;
use futures_util::FutureExt;
use serde_json::Value;
use smcp::{
    events::{
        CLIENT_GET_BLOB, CLIENT_GET_CONFIG, CLIENT_GET_DESKTOP, CLIENT_GET_RESOURCES,
        CLIENT_GET_SKILL, CLIENT_GET_SKILLS, CLIENT_GET_TOOLS, CLIENT_TOOL_CALL,
        NOTIFY_TOOL_CALL_CANCEL, SERVER_JOIN_OFFICE, SERVER_LEAVE_OFFICE, SERVER_UPDATE_CONFIG,
        SERVER_UPDATE_DESKTOP, SERVER_UPDATE_SKILLS, SERVER_UPDATE_TOOL_LIST,
    },
    set_content_blob_sideband, AgentCallData, ErrorCode, ErrorPayload, GetBlobReq, GetBlobRet,
    GetComputerConfigReq, GetComputerConfigRet, GetDesktopReq, GetDesktopRet, GetResourcesReq,
    GetResourcesRet, GetSkillReq, GetSkillRet, GetSkillsReq, GetSkillsRet, GetToolsReq,
    GetToolsRet, ToolCallReq, SMCP_NAMESPACE,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tf_rust_socketio::{
    asynchronous::{Client, ClientBuilder, ReconnectSettings},
    CloseReason, Event, Payload, TransportType,
};
use tokio::sync::{Mutex, Notify, RwLock};
use tracing::{debug, error, info, warn};

/// Default deadline for the initial Socket.IO namespace CONNECT handshake.
pub const DEFAULT_NAMESPACE_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

const NAMESPACE_CONNECT_CLEANUP_TIMEOUT: Duration = Duration::from_secs(1);

/// A client may publish Computer lifecycle changes only while it owns the current Socket.IO slot.
#[derive(Clone)]
struct RuntimeLifecycleLease {
    status: Arc<RuntimeStatus>,
    owner: Arc<StdMutex<u64>>,
    token: u64,
    active: bool,
}

impl RuntimeLifecycleLease {
    fn state(&self) -> LifecycleState {
        self.status.state()
    }

    fn transition(&self, next: LifecycleState) {
        if !self.active {
            return;
        }
        let owner = self.owner.lock().unwrap_or_else(|error| error.into_inner());
        if *owner == self.token {
            self.status.transition(next);
        }
    }

    fn activate(&self, next: LifecycleState) {
        let mut owner = self.owner.lock().unwrap_or_else(|error| error.into_inner());
        *owner = self.token;
        self.status.transition(next);
    }
}

/// Mutable lifecycle binding installed when a client becomes a Computer's active slot value.
/// Public standalone builders start unbound and can be attached later without rebuilding their
/// event callbacks.
#[derive(Clone, Default)]
struct RuntimeLifecycle {
    lease: Arc<StdMutex<Option<RuntimeLifecycleLease>>>,
}

impl RuntimeLifecycle {
    fn state(&self) -> Option<LifecycleState> {
        self.lease
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .map(RuntimeLifecycleLease::state)
    }

    fn transition(&self, next: LifecycleState) {
        let lease = self
            .lease
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        if let Some(lease) = lease {
            lease.transition(next);
        }
    }

    fn is_active_for(
        &self,
        status: &Arc<RuntimeStatus>,
        owner: &Arc<StdMutex<u64>>,
        retiring: &AtomicBool,
    ) -> bool {
        if retiring.load(Ordering::Acquire) {
            return false;
        }
        let lease = self
            .lease
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .filter(|lease| {
                lease.active
                    && Arc::ptr_eq(&lease.status, status)
                    && Arc::ptr_eq(&lease.owner, owner)
            })
            .cloned();
        let Some(lease) = lease else {
            return false;
        };
        let current_owner = lease
            .owner
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *current_owner == lease.token && !retiring.load(Ordering::Acquire)
    }

    /// Atomically reserve this client for exactly one Computer. The inactive reservation keeps
    /// callbacks from publishing to that Computer until replacement teardown has completed.
    fn try_claim(&self, lease: RuntimeLifecycleLease, retiring: &AtomicBool) -> bool {
        let mut current = self.lease.lock().unwrap_or_else(|error| error.into_inner());
        if retiring.load(Ordering::Acquire) || current.is_some() {
            return false;
        }
        *current = Some(lease);
        true
    }

    fn activate_claim(&self, token: u64, next: LifecycleState, retiring: &AtomicBool) -> bool {
        let lease = {
            let mut current = self.lease.lock().unwrap_or_else(|error| error.into_inner());
            if retiring.load(Ordering::Acquire) {
                return false;
            }
            let Some(lease) = current.as_mut().filter(|lease| lease.token == token) else {
                return false;
            };
            lease.active = true;
            lease.clone()
        };
        lease.activate(next);
        true
    }

    fn release_claim(&self, token: u64) {
        let mut current = self.lease.lock().unwrap_or_else(|error| error.into_inner());
        if current.as_ref().is_some_and(|lease| lease.token == token) {
            current.take();
        }
    }

    fn begin_retiring(&self, retiring: &AtomicBool) {
        // Serialize retirement with claim/activation so a client can never be published after a
        // concurrent direct disconnect has started, nor reclaimed after cleanup releases a lease.
        let _lease = self.lease.lock().unwrap_or_else(|error| error.into_inner());
        retiring.store(true, Ordering::Release);
    }

    fn deactivate(&self) {
        let lease = self
            .lease
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        let Some(lease) = lease else {
            return;
        };
        let mut owner = lease
            .owner
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if *owner == lease.token {
            *owner = 0;
        }
    }
}

/// Office membership is scoped to one Socket.IO namespace connection.
///
/// All fields that form the public membership invariant live behind one mutex so a transport
/// callback cannot interleave between generation validation and the confirmed/lifecycle commit.
#[derive(Default)]
struct OfficeMembership {
    desired: Option<String>,
    confirmed: Option<String>,
    generation: u64,
    connected: bool,
    rejoin_task: Option<tokio::task::AbortHandle>,
}

impl OfficeMembership {
    fn next_generation(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.generation
    }

    fn is_current(&self, generation: u64, desired: &str) -> bool {
        self.connected && self.generation == generation && self.desired.as_deref() == Some(desired)
    }

    fn invalidate_connection(&mut self, retain_desired: bool) -> Option<tokio::task::AbortHandle> {
        self.next_generation();
        self.connected = false;
        self.confirmed = None;
        if !retain_desired {
            self.desired = None;
        }
        self.rejoin_task.take()
    }
}

fn lock_office_membership(
    membership: &StdMutex<OfficeMembership>,
) -> std::sync::MutexGuard<'_, OfficeMembership> {
    membership.lock().unwrap_or_else(|error| error.into_inner())
}

/// SMCP Computer Socket.IO客户端 Builder /
/// Builder for the SMCP Computer Socket.IO client.
///
/// 通过 Builder 配置握手期的 namespace、Socket.IO auth dict 鉴权负载、自定义路由 HTTP headers 等。
/// Configure handshake-time namespace, Socket.IO auth-dict payload, custom routing headers, etc.
pub struct SmcpComputerClientBuilder {
    url: String,
    manager: Arc<RwLock<Option<MCPServerManager>>>,
    computer_name: String,
    inputs: Arc<RwLock<HashMap<String, MCPServerInput>>>,
    /// #85/#86：注入 Socket.IO CONNECT `auth` 字段的负载（如 `{"token":"<jwt>"}`）——连接面鉴权
    /// **唯一**信道（#86 起 HTTP header 鉴权已退役）。auth-agnostic：字段名由调用方决定。
    /// #85/#86: payload for the Socket.IO CONNECT `auth` field — the sole connection-auth channel
    /// (HTTP-header auth retired in #86). Auth-agnostic: the caller owns the field name.
    auth_payload: Option<Value>,
    /// #201：首连与每次自动重连前调用的动态 auth provider；配置时优先于 `auth_payload`。
    /// #201: dynamic auth provider called before the initial connection and every automatic
    /// reconnect; takes precedence over `auth_payload` when configured.
    auth_provider: Option<SocketIoAuthProvider>,
    namespace: Option<String>,
    namespace_connect_timeout: Duration,
    headers: Option<HashMap<String, String>>,
    /// INT-03 #72：Computer 操作句柄（socketio-detached），供 blob/skill/cancel/tool_call handler 调用。
    /// `Option`：兼容旧入口（`SmcpComputerClient::new` / 不接 Computer 的测试）——缺省时 blob/skill/cancel
    /// handler 回 InvalidState、tool_call 回退旧 `manager.execute_tool`（无取消/铸造）。
    computer_ops: Option<Arc<dyn ComputerHandlerOps>>,
}

impl SmcpComputerClientBuilder {
    /// 创建 Builder（必填项） / Create a new builder (required fields).
    pub fn new(
        url: impl Into<String>,
        manager: Arc<RwLock<Option<MCPServerManager>>>,
        computer_name: impl Into<String>,
        inputs: Arc<RwLock<HashMap<String, MCPServerInput>>>,
    ) -> Self {
        Self {
            url: url.into(),
            manager,
            computer_name: computer_name.into(),
            inputs,
            auth_payload: None,
            auth_provider: None,
            namespace: None,
            namespace_connect_timeout: DEFAULT_NAMESPACE_CONNECT_TIMEOUT,
            headers: None,
            computer_ops: None,
        }
    }

    /// 注入 Computer 操作句柄（INT-03 #72）/ inject the Computer ops handle for blob/skill/cancel handlers。
    ///
    /// 由 [`crate::computer::Computer::connect_socketio`] 以 socketio-detached 克隆注入；不设时
    /// blob/skill/cancel handler 不可用、tool_call 退回旧 manager 路径（无取消/铸造）。
    pub(crate) fn computer_ops(mut self, ops: Arc<dyn ComputerHandlerOps>) -> Self {
        self.computer_ops = Some(ops);
        self
    }

    /// #85/#86：注入 Socket.IO CONNECT `auth` 字段的负载（连接面鉴权唯一信道）。
    /// Inject the Socket.IO CONNECT `auth` payload (the sole connection-auth channel).
    ///
    /// 保持 auth-agnostic：**不硬编码字段名**，由调用方决定整个 JSON 负载（TuringFocus 生态传
    /// `{"token": "<jwt>"}`，server 默认读 `token` 字段）。负载同时透传到 4900→polling 重连，
    /// 保证重连不退化为无鉴权。
    /// Stays auth-agnostic — the caller supplies the whole JSON payload (no hardcoded field name).
    /// Also replayed on the 4900→polling reconnect.
    pub fn auth_payload(mut self, payload: impl Into<Value>) -> Self {
        self.auth_payload = Some(payload.into());
        self
    }

    /// #201：配置动态 Socket.IO auth provider。
    /// Configure a dynamic Socket.IO auth provider.
    ///
    /// 同一个 Provider 会在首次连接与每次底层自动重连的 CONNECT 前调用。Provider 返回完整 auth
    /// JSON，且配置时优先于静态 [`auth_payload`](Self::auth_payload)。Provider 调用由底层重连流程
    /// 串行驱动，本 Builder 不增加并行刷新任务、失败回退或额外重试策略。
    /// The same provider is called before the initial CONNECT and every automatic reconnect
    /// CONNECT. It returns the complete auth JSON and takes precedence over static
    /// [`auth_payload`](Self::auth_payload). Calls are serialized by the transport reconnect flow;
    /// this builder adds no parallel refresh task, failure fallback, or retry policy.
    pub fn auth_provider(mut self, provider: SocketIoAuthProvider) -> Self {
        self.auth_provider = Some(provider);
        self
    }

    /// 自定义 Socket.IO 应用层 namespace；未设置时默认 [`SMCP_NAMESPACE`] (`/smcp`)。
    /// Customize the Socket.IO application-layer namespace; defaults to
    /// [`SMCP_NAMESPACE`] (`/smcp`) when not set.
    pub fn namespace(mut self, ns: impl Into<String>) -> Self {
        self.namespace = Some(ns.into());
        self
    }

    /// Configure the total time [`connect`](Self::connect) allows for initial transport
    /// establishment and namespace readiness.
    ///
    /// Engine.IO transport establishment alone is not enough for application events, so connect
    /// returns only after the namespace callback fires. The default is 30 seconds.
    pub fn namespace_connect_timeout(mut self, timeout: Duration) -> Self {
        self.namespace_connect_timeout = timeout;
        self
    }

    /// 附加任意 HTTP upgrade header（路由用，如 TF 生态 `X-TF-RobotId`；**非鉴权信道**）。
    /// Attach arbitrary HTTP upgrade headers (routing, e.g. TF ecosystem headers; NOT for auth).
    pub fn headers(mut self, headers: HashMap<String, String>) -> Self {
        self.headers = Some(headers);
        self
    }

    /// 建立 Socket.IO 连接。 / Establish the Socket.IO connection.
    pub async fn connect(self) -> ComputerResult<SmcpComputerClient> {
        let namespace = self.namespace.unwrap_or_else(|| SMCP_NAMESPACE.to_string());
        SmcpComputerClient::connect_internal(
            self.url,
            self.manager,
            self.computer_name,
            self.inputs,
            self.auth_payload,
            self.auth_provider,
            namespace,
            self.namespace_connect_timeout,
            self.headers,
            self.computer_ops,
        )
        .await
    }
}

/// SMCP Computer Socket.IO客户端
/// SMCP Computer Socket.IO client
pub struct SmcpComputerClient {
    /// Socket.IO客户端实例 / Socket.IO client instance
    client: Client,
    /// Computer名称 / Computer name
    computer_name: String,
    /// Desired and server-confirmed Office plus the current namespace connection generation.
    office_membership: Arc<StdMutex<OfficeMembership>>,
    /// Serialize explicit join/leave and automatic rejoin wire operations.
    office_operation: Arc<Mutex<()>>,
    /// 输入定义映射 / Input definitions map
    #[allow(dead_code)]
    inputs: Arc<RwLock<HashMap<String, MCPServerInput>>>,
    /// 实际握手使用的 Socket.IO namespace / Socket.IO namespace used during handshake
    namespace: String,
    /// Shared Computer lifecycle binding; installed when attached to a Computer.
    runtime_status: RuntimeLifecycle,
    /// Once set, application handlers are inert and any late reconnect is closed immediately.
    retiring: Arc<AtomicBool>,
}

impl SmcpComputerClient {
    /// Whether this client still owns the active lifecycle lease for the supplied Computer.
    pub(crate) fn has_active_runtime_lifecycle(
        &self,
        status: &Arc<RuntimeStatus>,
        owner: &Arc<StdMutex<u64>>,
    ) -> bool {
        self.runtime_status
            .is_active_for(status, owner, &self.retiring)
    }

    /// Atomically reserve this client for one Computer before touching its current slot value.
    pub(crate) fn claim_runtime_lifecycle(
        &self,
        status: Arc<RuntimeStatus>,
        owner: Arc<StdMutex<u64>>,
        token: u64,
    ) -> bool {
        self.runtime_status.try_claim(
            RuntimeLifecycleLease {
                status,
                owner,
                token,
                active: false,
            },
            &self.retiring,
        )
    }

    /// Activate a previously acquired ownership reservation after replacement teardown succeeds.
    pub(crate) fn activate_runtime_lifecycle(&self, token: u64) -> bool {
        let membership = self
            .office_membership
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let state = if membership.confirmed.is_some() {
            LifecycleState::JoinedOffice
        } else if membership.connected {
            LifecycleState::Connected
        } else {
            LifecycleState::Started
        };
        self.runtime_status
            .activate_claim(token, state, &self.retiring)
    }

    /// Roll back an installation reservation that was never published.
    pub(crate) fn release_runtime_lifecycle_claim(&self, token: u64) {
        self.runtime_status.release_claim(token);
    }

    /// Revoke this client's lifecycle lease after its transport has been closed and removed from
    /// the owning Computer. The token check prevents a stale client from revoking a replacement.
    pub(crate) fn deactivate_runtime_lifecycle(&self) {
        self.runtime_status.deactivate();
    }

    /// 创建新的Socket.IO客户端（便捷入口） /
    /// Create a new Socket.IO client (convenience entry point).
    ///
    /// 内部委托给 [`SmcpComputerClientBuilder`]（默认 `namespace = "/smcp"`）。
    /// `auth_payload`：Socket.IO CONNECT `auth` 负载（连接面鉴权唯一信道，#86）；
    /// `headers`：路由 HTTP headers（非鉴权）。
    /// Delegates to [`SmcpComputerClientBuilder`]; `auth_payload` is the Socket.IO CONNECT
    /// `auth` payload (sole connection-auth channel), `headers` are routing-only.
    pub async fn new(
        url: &str,
        manager: Arc<RwLock<Option<MCPServerManager>>>,
        computer_name: String,
        auth_payload: Option<Value>,
        inputs: Arc<RwLock<HashMap<String, MCPServerInput>>>,
        headers: Option<HashMap<String, String>>,
    ) -> ComputerResult<Self> {
        let mut b = SmcpComputerClientBuilder::new(url, manager, computer_name, inputs);
        if let Some(payload) = auth_payload {
            b = b.auth_payload(payload);
        }
        if let Some(h) = headers {
            b = b.headers(h);
        }
        b.connect().await
    }

    /// 真正的连接实现 / Actual connection implementation.
    ///
    /// 私有 helper，参数已由 [`SmcpComputerClientBuilder::connect`] 解析过默认值。
    /// Private helper; defaults already resolved by
    /// [`SmcpComputerClientBuilder::connect`].
    #[allow(clippy::too_many_arguments)]
    async fn connect_internal(
        url: String,
        manager: Arc<RwLock<Option<MCPServerManager>>>,
        computer_name: String,
        inputs: Arc<RwLock<HashMap<String, MCPServerInput>>>,
        auth_payload: Option<Value>,
        auth_provider: Option<SocketIoAuthProvider>,
        namespace: String,
        namespace_connect_timeout: Duration,
        headers: Option<HashMap<String, String>>,
        computer_ops: Option<Arc<dyn ComputerHandlerOps>>,
    ) -> ComputerResult<Self> {
        let office_membership = Arc::new(StdMutex::new(OfficeMembership::default()));
        let office_operation = Arc::new(Mutex::new(()));
        let runtime_status = RuntimeLifecycle::default();
        let retiring = Arc::new(AtomicBool::new(false));
        let namespace_ready = Arc::new(Notify::new());
        let manager_clone = manager.clone();
        let computer_name_clone = computer_name.clone();
        let inputs_clone = inputs.clone();
        // INT-03 #72：Computer ops 句柄供 blob/skill/cancel/tool_call handler 闭包按需克隆。
        let computer_ops_clone = computer_ops.clone();

        // HS-02 #22: 在连接 URL 注入权威 a2c_version（丢弃调用方自带值，防版本漂移），
        // 使服务端 HTTP 握手中间件能在 Socket.IO 业务层之前完成版本协商。
        // HS-02 #22: inject the authoritative a2c_version into the connection URL so the server's
        // HTTP handshake middleware can negotiate the version before the Socket.IO layer.
        let handshake_url =
            smcp::utils::handshake::build_handshake_url(&url, smcp::PROTOCOL_VERSION).map_err(
                |e| ComputerError::ConnectionError(format!("Invalid handshake URL: {e}")),
            )?;

        // 汇总路由 HTTP headers（仅自定义路由头，如 X-TF-*）；#86 起鉴权移到 Socket.IO auth dict，
        // header 不再承载鉴权。用于首连与 4900 改 polling 重连。
        // Collect routing HTTP headers (custom routing only, e.g. X-TF-*); auth lives in the
        // Socket.IO auth dict since #86. Reused for the primary connect and the 4900 polling retry.
        let mut handshake_headers: HashMap<String, String> = HashMap::new();
        if let Some(custom_headers) = headers {
            for (key, value) in custom_headers {
                handshake_headers.insert(key, value);
            }
        }

        // #201：Provider 在首连前解析一次；未配置时复用静态 payload。Provider 优先级在此统一收口，
        // 使 Builder 直用与 Computer::connect_socketio 行为一致。
        let initial_auth_payload = match auth_provider.as_ref() {
            Some(provider) => Some(provider().await),
            None => auth_payload,
        };

        // 使用ClientBuilder注册事件处理器
        // Use ClientBuilder to register event handlers
        let mut builder = ClientBuilder::new(&handshake_url).namespace(namespace.clone());

        // HS-02 #22: polling-first（先 HTTP polling 握手，可被服务端 400+4008 body 拦截，
        // 失败时再升级 WebSocket）。⚠️ 不可用 WS-only（TransportType::Websocket）——会绕过服务端
        // HTTP 版本握手中间件，使版本不兼容无法被感知。
        // HS-02 #22: polling-first (HTTP polling handshake can be intercepted by the server's
        // 400 + 4008 body, then upgrades to WebSocket). MUST NOT use WS-only — it bypasses the
        // server's HTTP version handshake gate.
        builder = builder.transport_type(TransportType::Any);

        // 添加握手 HTTP headers / Add handshake HTTP headers.
        // Safety: opening_header 底层使用 http::HeaderValue::from_bytes() 做 RFC 7230 校验，
        // 会拒绝包含 \r\n 等控制字符的恶意输入，无需额外防御 header injection。
        // Safety: opening_header internally uses http::HeaderValue::from_bytes() for RFC 7230
        // validation, rejecting \r\n and other control characters. No extra injection defense needed.
        for (key, value) in &handshake_headers {
            builder = builder.opening_header(key.as_str(), value.as_str());
        }

        // AUTH-DICT #85：注入 Socket.IO CONNECT `auth` 字段（如 `{"token":"<jwt>"}`）。clone 保留原值，
        // 供下方 `connect_and_classify` 在 4900→polling 重连时重放（否则重连退化为无 auth dict）。
        // AUTH-DICT #85: inject the Socket.IO CONNECT `auth` field on the primary builder; the original
        // is kept (clone here) and replayed by `connect_and_classify` on the 4900→polling reconnect.
        if let Some(payload) = &initial_auth_payload {
            builder = builder.auth(payload.clone());
        }

        // #201：底层在每次自动重连尝试前串行调用 on_reconnect。这里只更新 auth，不改变地址、
        // headers、退避或最大重试策略，也不在 Provider 失败时回放旧凭证。
        if let Some(provider) = auth_provider {
            builder = builder.on_reconnect(move || {
                let provider = Arc::clone(&provider);
                async move {
                    let mut settings = ReconnectSettings::new();
                    settings.auth(provider().await);
                    settings
                }
                .boxed()
            });
        }

        // #204：Office room membership belongs to a Socket.IO namespace connection and therefore
        // never survives a transport reconnect. Close/Connect update one membership state machine:
        // unexpected transport close retains only the desired Office, while clean server disconnect
        // clears it. Manual disconnect is invalidated explicitly in `disconnect`, because
        // tf-rust-socketio does not emit Event::Close for that path.
        let close_membership = Arc::clone(&office_membership);
        let close_status = runtime_status.clone();
        let close_retiring = Arc::clone(&retiring);
        builder = builder.on(Event::Close, move |payload, _client| {
            let membership = Arc::clone(&close_membership);
            let status = close_status.clone();
            let retiring = Arc::clone(&close_retiring);
            async move {
                let transport_close = !retiring.load(Ordering::Acquire)
                    && Self::payload_contains_text(&payload, CloseReason::TransportClose.as_str());
                let rejoin_task = {
                    let mut membership = lock_office_membership(&membership);
                    let rejoin_task = membership.invalidate_connection(transport_close);
                    let next = if transport_close {
                        LifecycleState::Connecting
                    } else {
                        LifecycleState::Started
                    };
                    if status.state() != Some(next) {
                        status.transition(next);
                    }
                    rejoin_task
                };
                if let Some(task) = rejoin_task {
                    task.abort();
                }
            }
            .boxed()
        });

        let connect_membership = Arc::clone(&office_membership);
        let connect_operation = Arc::clone(&office_operation);
        let connect_name = computer_name.clone();
        let connect_status = runtime_status.clone();
        let connect_retiring = Arc::clone(&retiring);
        let connect_ready = Arc::clone(&namespace_ready);
        builder = builder.on(Event::Connect, move |_payload, client| {
            let membership = Arc::clone(&connect_membership);
            let operation = Arc::clone(&connect_operation);
            let computer_name = connect_name.clone();
            let status = connect_status.clone();
            let retiring = Arc::clone(&connect_retiring);
            let ready = Arc::clone(&connect_ready);
            async move {
                if retiring.load(Ordering::Acquire) {
                    // 0.8.1 does not re-check Manual once its reconnect loop is in backoff. Close
                    // a late namespace inline before the reader can process following app events.
                    let _ = client.disconnect().await;
                    return;
                }
                // Capture the generation inside the reader callback, before the task can queue
                // behind an explicit Office operation. A later Close/Connect invalidates it.
                let (generation, desired, previous_task) = {
                    let mut membership = lock_office_membership(&membership);
                    let previous_task = membership.rejoin_task.take();
                    let generation = membership.next_generation();
                    membership.connected = true;
                    membership.confirmed = None;
                    let desired = membership.desired.clone();
                    if desired.is_none() {
                        status.transition(LifecycleState::Connected);
                    }
                    (generation, desired, previous_task)
                };
                ready.notify_one();
                if let Some(task) = previous_task {
                    task.abort();
                }

                let Some(desired) = desired else {
                    return;
                };

                // The reader awaits event callbacks, so waiting for an ACK inline would self-block.
                // The detached task is generation-bound and aborted by Close/manual disconnect.
                let task_membership = Arc::clone(&membership);
                let task_status = status.clone();
                let task_desired = desired.clone();
                let task = tokio::spawn(async move {
                    let _operation = operation.lock().await;
                    {
                        let membership = lock_office_membership(&task_membership);
                        if !membership.is_current(generation, &task_desired) {
                            return;
                        }
                    }

                    let result =
                        Self::join_office_with_client(&client, &computer_name, &task_desired).await;
                    let mut membership = lock_office_membership(&task_membership);
                    if !membership.is_current(generation, &task_desired) {
                        return;
                    }
                    membership.rejoin_task = None;

                    match result {
                        Ok(()) => {
                            membership.confirmed = Some(task_desired.clone());
                            task_status.transition(LifecycleState::JoinedOffice);
                            info!("Automatically rejoined office: {}", task_desired);
                        }
                        Err(error) => {
                            membership.desired = None;
                            membership.confirmed = None;
                            task_status.transition(LifecycleState::Connected);
                            error!(
                                "Failed to automatically rejoin office {}: {}",
                                task_desired, error
                            );
                        }
                    }
                });
                let abort_handle = task.abort_handle();
                let mut current = lock_office_membership(&membership);
                if current.is_current(generation, &desired) {
                    current.rejoin_task = Some(abort_handle);
                } else {
                    task.abort();
                }
            }
            .boxed()
        });

        // 注册事件处理器（on_any 消费并返回 builder）/ Register handlers (on_any consumes & returns builder)。
        let event_retiring = Arc::clone(&retiring);
        builder = builder.on_any(move |event, payload, client| {
            if event_retiring.load(Ordering::Acquire) {
                return async {}.boxed();
            }
            // 只处理自定义事件
            // Only handle custom events
            let event_str = match event {
                Event::Custom(s) => s,
                _ => return async {}.boxed(),
            };

            match event_str.as_str() {
                CLIENT_TOOL_CALL => {
                    let manager = manager_clone.clone();
                    let computer_name = computer_name_clone.clone();
                    let ops = computer_ops_clone.clone();
                    let client_clone = client.clone();
                    let payload_clone = payload.clone();

                    async move {
                        match Self::handle_tool_call_with_ack(
                            payload,
                            manager,
                            computer_name,
                            ops,
                            client_clone,
                        )
                        .await
                        {
                            Ok((ack_id, response)) => {
                                if let Some(id) = ack_id {
                                    if let Err(e) = client.ack_with_id(id, response).await {
                                        error!("Failed to send ack: {}", e);
                                    }
                                }
                            }
                            Err(e) => {
                                error!("Error handling tool call: {}", e);
                                // 尝试返回错误响应 / Try to return error response
                                if let Ok((Some(id), _)) = Self::extract_ack_id(payload_clone) {
                                    let error_response = serde_json::json!({
                                        "isError": true,
                                        "content": [],
                                        "structuredContent": {
                                            "error": e.to_string(),
                                            "error_type": "ComputerError"
                                        }
                                    });
                                    let _ = client.ack_with_id(id, error_response).await;
                                }
                            }
                        }
                    }
                    .boxed()
                }
                CLIENT_GET_TOOLS => {
                    let manager = manager_clone.clone();
                    let computer_name = computer_name_clone.clone();
                    let client_clone = client.clone();

                    async move {
                        match Self::handle_get_tools_with_ack(
                            payload,
                            manager,
                            computer_name,
                            client_clone,
                        )
                        .await
                        {
                            Ok((ack_id, response)) => {
                                if let Some(id) = ack_id {
                                    if let Err(e) = client.ack_with_id(id, response).await {
                                        error!("Failed to send ack: {}", e);
                                    }
                                }
                            }
                            Err(e) => {
                                error!("Error handling get tools: {}", e);
                            }
                        }
                    }
                    .boxed()
                }
                CLIENT_GET_CONFIG => {
                    let manager = manager_clone.clone();
                    let computer_name = computer_name_clone.clone();
                    let ops = computer_ops_clone.clone();
                    let client_clone = client.clone();
                    let inputs = inputs_clone.clone();

                    async move {
                        match Self::handle_get_config_with_ack(
                            payload,
                            manager,
                            computer_name,
                            ops,
                            client_clone,
                            inputs,
                        )
                        .await
                        {
                            Ok((ack_id, response)) => {
                                if let Some(id) = ack_id {
                                    if let Err(e) = client.ack_with_id(id, response).await {
                                        error!("Failed to send ack: {}", e);
                                    }
                                }
                            }
                            Err(e) => {
                                error!("Error handling get config: {}", e);
                            }
                        }
                    }
                    .boxed()
                }
                CLIENT_GET_DESKTOP => {
                    let manager = manager_clone.clone();
                    let computer_name = computer_name_clone.clone();
                    let client_clone = client.clone();

                    async move {
                        match Self::handle_get_desktop_with_ack(
                            payload,
                            manager,
                            computer_name,
                            Some(client_clone),
                        )
                        .await
                        {
                            Ok((ack_id, response)) => {
                                if let Some(id) = ack_id {
                                    if let Err(e) = client.ack_with_id(id, response).await {
                                        error!("Failed to send ack: {}", e);
                                    }
                                }
                            }
                            Err(e) => {
                                error!("Error handling get desktop: {}", e);
                            }
                        }
                    }
                    .boxed()
                }
                CLIENT_GET_RESOURCES => {
                    let manager = manager_clone.clone();
                    let computer_name = computer_name_clone.clone();

                    async move {
                        match Self::handle_get_resources_with_ack(payload, manager, computer_name)
                            .await
                        {
                            Ok((ack_id, response)) => {
                                if let Some(id) = ack_id {
                                    if let Err(e) = client.ack_with_id(id, response).await {
                                        error!("Failed to send ack: {}", e);
                                    }
                                }
                            }
                            Err(e) => {
                                error!("Error handling get resources: {}", e);
                            }
                        }
                    }
                    .boxed()
                }
                // INT-03 #72：通用二进制拉取 client:get_blob（带 ACK）。
                CLIENT_GET_BLOB => {
                    let ops = computer_ops_clone.clone();
                    let computer_name = computer_name_clone.clone();
                    async move {
                        match Self::handle_get_blob_with_ack(payload, ops, computer_name).await {
                            Ok((ack_id, response)) => {
                                if let Some(id) = ack_id {
                                    if let Err(e) = client.ack_with_id(id, response).await {
                                        error!("Failed to send ack: {}", e);
                                    }
                                }
                            }
                            Err(e) => error!("Error handling get_blob: {}", e),
                        }
                    }
                    .boxed()
                }
                // INT-03 #72：SKILL 清单 client:get_skills（带 ACK）。
                CLIENT_GET_SKILLS => {
                    let ops = computer_ops_clone.clone();
                    let computer_name = computer_name_clone.clone();
                    async move {
                        match Self::handle_get_skills_with_ack(payload, ops, computer_name).await {
                            Ok((ack_id, response)) => {
                                if let Some(id) = ack_id {
                                    if let Err(e) = client.ack_with_id(id, response).await {
                                        error!("Failed to send ack: {}", e);
                                    }
                                }
                            }
                            Err(e) => error!("Error handling get_skills: {}", e),
                        }
                    }
                    .boxed()
                }
                // INT-03 #72：SKILL 单资源 client:get_skill（带 ACK；body XOR blob_handle）。
                CLIENT_GET_SKILL => {
                    let ops = computer_ops_clone.clone();
                    let computer_name = computer_name_clone.clone();
                    async move {
                        match Self::handle_get_skill_with_ack(payload, ops, computer_name).await {
                            Ok((ack_id, response)) => {
                                if let Some(id) = ack_id {
                                    if let Err(e) = client.ack_with_id(id, response).await {
                                        error!("Failed to send ack: {}", e);
                                    }
                                }
                            }
                            Err(e) => error!("Error handling get_skill: {}", e),
                        }
                    }
                    .boxed()
                }
                // INT-03 #72 + 取消纵切：notify:tool_call_cancel → acancel_tool（**无 ACK**，fire-and-forget）。
                NOTIFY_TOOL_CALL_CANCEL => {
                    let ops = computer_ops_clone.clone();
                    async move {
                        Self::handle_tool_call_cancel(payload, ops).await;
                    }
                    .boxed()
                }
                _ => {
                    debug!("Unhandled event: {}", event_str);
                    async {}.boxed()
                }
            }
        });

        // 连接服务器（polling-first 已设；分类版本握手错误，4900 时改 polling 取 4008）
        // Connect (polling-first already set; classify version-handshake errors; on 4900 re-fetch
        // the authoritative 4008 over polling).
        // Bound both the tf builder's namespace CONNECT send and our callback readiness wait. With
        // polling transport, a server-side namespace middleware can hold the CONNECT request itself,
        // so timing only the Notify wait would still let Builder::connect hang forever.
        let pending_cleanup = Arc::new(Mutex::new(None::<Client>));
        let connection_cleanup = Arc::clone(&pending_cleanup);
        let connection = async {
            let client = match smcp_client_transport::connect_and_classify(
                builder,
                &handshake_url,
                &namespace,
                initial_auth_payload,
                handshake_headers,
            )
            .await
            {
                Ok(client) => client,
                Err(smcp_client_transport::ConnectError::ProtocolVersion(pve)) => {
                    return Err(ComputerError::ProtocolVersionMismatch(pve));
                }
                Err(smcp_client_transport::ConnectError::Connection(msg)) => {
                    return Err(ComputerError::SocketIoError(msg));
                }
            };
            *connection_cleanup.lock().await = Some(client.clone());

            // A successful return is immediately join-ready; a fixed sleep leaves a deterministic
            // race between Engine.IO establishment and the namespace CONNECT callback.
            loop {
                let notified = namespace_ready.notified();
                if lock_office_membership(&office_membership).connected {
                    break;
                }
                notified.await;
            }
            Ok(client)
        };
        let client = match tokio::time::timeout(namespace_connect_timeout, connection).await {
            Ok(result) => {
                let client = result?;
                pending_cleanup.lock().await.take();
                client
            }
            Err(_) => {
                // The tf reader retains an internal clone after connect_and_classify returns. Arm
                // callbacks first, then give teardown a separate bounded budget; if the builder was
                // still sending CONNECT, dropping its future owns the only client and closes it.
                retiring.store(true, Ordering::Release);
                if let Some(client) = pending_cleanup.lock().await.take() {
                    let _ = tokio::time::timeout(
                        NAMESPACE_CONNECT_CLEANUP_TIMEOUT,
                        client.disconnect(),
                    )
                    .await;
                }
                return Err(ComputerError::TimeoutError(
                    "Socket.IO namespace connect timed out".to_string(),
                ));
            }
        };

        info!(
            "Connected to SMCP server at {} with computer name: {}",
            url, computer_name
        );

        Ok(Self {
            client,
            computer_name,
            office_membership,
            office_operation,
            inputs,
            namespace,
            runtime_status,
            retiring,
        })
    }

    /// 加入Office（Socket.IO Room）
    /// Join an Office (Socket.IO Room)
    pub async fn join_office(&self, office_id: &str) -> ComputerResult<()> {
        debug!("Joining office: {}", office_id);
        let _operation = self.office_operation.lock().await;
        let (generation, previous_desired, previous_confirmed) = {
            let mut membership = lock_office_membership(&self.office_membership);
            if !membership.connected {
                return Err(ComputerError::InvalidState(
                    "Socket.IO client not connected".to_string(),
                ));
            }
            let generation = membership.next_generation();
            let previous_desired = membership.desired.clone();
            let previous_confirmed = membership.confirmed.clone();
            membership.desired = Some(office_id.to_string());
            (generation, previous_desired, previous_confirmed)
        };

        let result =
            Self::join_office_with_client(&self.client, &self.computer_name, office_id).await;
        let mut membership = lock_office_membership(&self.office_membership);
        if !membership.is_current(generation, office_id) {
            return Err(ComputerError::SocketIoError(
                "Socket.IO connection changed while joining office".to_string(),
            ));
        }

        match result {
            Ok(()) => {
                membership.confirmed = Some(office_id.to_string());
                self.runtime_status.transition(LifecycleState::JoinedOffice);
                info!("Successfully joined office: {}", office_id);
                Ok(())
            }
            Err(error) => {
                membership.desired = previous_desired;
                membership.confirmed = previous_confirmed;
                self.runtime_status
                    .transition(if membership.confirmed.is_some() {
                        LifecycleState::JoinedOffice
                    } else {
                        LifecycleState::Connected
                    });
                Err(error)
            }
        }
    }

    /// Send `server:join_office` over a specific namespace connection and validate its ACK.
    async fn join_office_with_client(
        client: &Client,
        computer_name: &str,
        office_id: &str,
    ) -> ComputerResult<()> {
        let req_data = serde_json::json!({
            "office_id": office_id,
            "role": "computer",
            "name": computer_name
        });
        let response =
            Self::call_with_client(client, SERVER_JOIN_OFFICE, req_data, Some(10)).await?;
        debug!("Join office response: {:?}", response);

        // The ACK may arrive as `[true, null]` or as one nested tuple argument `[[true, null]]`.
        let actual_response = if response.len() == 1 {
            response
                .first()
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or(response)
        } else {
            response
        };

        match actual_response.first().and_then(Value::as_bool) {
            Some(true) => Ok(()),
            Some(false) => {
                let error_msg = actual_response
                    .get(1)
                    .and_then(Value::as_str)
                    .unwrap_or("Unknown error");
                Err(ComputerError::SocketIoError(format!(
                    "Failed to join office: {error_msg}"
                )))
            }
            None if actual_response.is_empty() => Err(ComputerError::SocketIoError(
                "Empty response from server".to_string(),
            )),
            None => Err(ComputerError::SocketIoError(format!(
                "Invalid response format from server: {actual_response:?}"
            ))),
        }
    }

    /// 获取当前Office ID / Get current Office ID
    pub async fn get_current_office_id(&self) -> ComputerResult<String> {
        let membership = lock_office_membership(&self.office_membership);
        match membership.confirmed.as_ref() {
            Some(id) => Ok(id.clone()),
            None => Err(ComputerError::InvalidState(
                "Not currently in any office".to_string(),
            )),
        }
    }

    /// 离开Office
    /// Leave an Office
    pub async fn leave_office(&self, office_id: &str) -> ComputerResult<()> {
        debug!("Leaving office: {}", office_id);
        let _operation = self.office_operation.lock().await;
        let (generation, previous_desired, previous_confirmed) = {
            let mut membership = lock_office_membership(&self.office_membership);
            if !membership.connected {
                return Err(ComputerError::InvalidState(
                    "Socket.IO client not connected".to_string(),
                ));
            }
            let generation = membership.next_generation();
            let previous_desired = membership.desired.take();
            let previous_confirmed = membership.confirmed.clone();
            (generation, previous_desired, previous_confirmed)
        };

        let req_data = serde_json::json!({
            "office_id": office_id
        });

        let result = self.emit(SERVER_LEAVE_OFFICE, req_data).await;
        let mut membership = lock_office_membership(&self.office_membership);
        if membership.generation != generation || !membership.connected {
            return Err(ComputerError::SocketIoError(
                "Socket.IO connection changed while leaving office".to_string(),
            ));
        }
        if let Err(error) = result {
            membership.desired = previous_desired;
            membership.confirmed = previous_confirmed;
            self.runtime_status
                .transition(if membership.confirmed.is_some() {
                    LifecycleState::JoinedOffice
                } else {
                    LifecycleState::Connected
                });
            return Err(error);
        }
        membership.confirmed = None;
        self.runtime_status.transition(LifecycleState::Connected);

        info!("Left office: {}", office_id);
        Ok(())
    }

    async fn has_confirmed_office(&self) -> bool {
        lock_office_membership(&self.office_membership)
            .confirmed
            .is_some()
    }

    /// 发送配置更新通知
    /// Emit config update notification
    pub async fn emit_update_config(&self) -> ComputerResult<()> {
        if self.has_confirmed_office().await {
            let req_data = serde_json::json!({
                "computer": self.computer_name
            });
            self.emit(SERVER_UPDATE_CONFIG, req_data).await?;
            info!("Emitted config update notification");
        }
        Ok(())
    }

    /// 发送工具列表更新通知
    /// Emit tool list update notification
    pub async fn emit_update_tool_list(&self) -> ComputerResult<()> {
        if self.has_confirmed_office().await {
            let req_data = serde_json::json!({
                "computer": self.computer_name
            });
            self.emit(SERVER_UPDATE_TOOL_LIST, req_data).await?;
            info!("Emitted tool list update notification");
        }
        Ok(())
    }

    /// 发送 SKILL 集合更新通知（`server:update_skills` → Server 广播 `notify:update_skills`，SRV-02 #50）
    /// Emit SKILL-set update notification (INT-01 #68; handler/broadcast refined by SRV-02/#72)
    pub async fn emit_update_skills(&self) -> ComputerResult<()> {
        if self.has_confirmed_office().await {
            let req_data = serde_json::json!({
                "computer": self.computer_name
            });
            self.emit(SERVER_UPDATE_SKILLS, req_data).await?;
            info!("Emitted SKILL update notification");
        }
        Ok(())
    }

    /// 发送桌面更新通知
    /// Emit desktop update notification
    pub async fn emit_update_desktop(&self) -> ComputerResult<()> {
        if self.has_confirmed_office().await {
            let req_data = serde_json::json!({
                "computer": self.computer_name
            });
            self.emit(SERVER_UPDATE_DESKTOP, req_data).await?;
            info!("Emitted desktop update notification");
        }
        Ok(())
    }

    /// 发送事件（不等待响应）
    /// Emit event without waiting for response
    async fn emit(&self, event: &str, data: Value) -> ComputerResult<()> {
        // 检查事件名 policy / Check event name policy
        if event.starts_with("notify:") || event.starts_with("client:") {
            return Err(ComputerError::InvalidState(
                format!(
                    "Computer 不允许发送 notify:* 或 client:* 事件 / Computer cannot send notify:* or client:* events: {}",
                    event
                )
            ));
        }

        debug!("Emitting event: {}", event);

        self.client
            .emit(event, Payload::Text(vec![data], None))
            .await
            .map_err(|e| ComputerError::SocketIoError(format!("Failed to emit {}: {}", event, e)))
    }

    /// Send an event with ACK over the supplied namespace connection.
    async fn call_with_client(
        client: &Client,
        event: &str,
        data: Value,
        timeout_secs: Option<u64>,
    ) -> ComputerResult<Vec<Value>> {
        // 检查事件名 policy / Check event name policy
        if event.starts_with("notify:") || event.starts_with("client:") {
            return Err(ComputerError::InvalidState(
                format!(
                    "Computer 不允许发送 notify:* 或 client:* 事件 / Computer cannot send notify:* or client:* events: {}",
                    event
                )
            ));
        }

        let timeout = std::time::Duration::from_secs(timeout_secs.unwrap_or(30));
        debug!("Calling event: {} with timeout {:?}", event, timeout);

        let (tx, rx) = tokio::sync::oneshot::channel();
        let tx = Arc::new(std::sync::Mutex::new(Some(tx)));

        let callback = move |payload: Payload, _client: Client| {
            if let Some(tx_opt) = tx.try_lock().ok().and_then(|mut m| m.take()) {
                let _ = tx_opt.send(payload);
            }
            async {}.boxed()
        };

        client
            .emit_with_ack(event, Payload::Text(vec![data], None), timeout, callback)
            .await
            .map_err(|e| {
                ComputerError::SocketIoError(format!("Failed to call {}: {}", event, e))
            })?;

        // 使用 tokio::time::timeout 来确保 rx.await 不会无限期等待
        // Use tokio::time::timeout to ensure rx.await doesn't wait forever
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(response)) => {
                // 从响应中提取JSON数据 / Extract JSON data from response
                match response {
                    Payload::Text(values, _) => {
                        debug!("Received response: {:?}", values);
                        Ok(values)
                    }
                    #[allow(deprecated)]
                    Payload::String(s, _) => {
                        // 尝试解析字符串为JSON数组
                        // Try to parse string as JSON array
                        let parsed: Vec<Value> = serde_json::from_str(&s).map_err(|e| {
                            ComputerError::SocketIoError(format!("Failed to parse response: {}", e))
                        })?;
                        debug!("Received parsed response: {:?}", parsed);
                        Ok(parsed)
                    }
                    Payload::Binary(_, _) => Err(ComputerError::SocketIoError(
                        "Binary response not supported".to_string(),
                    )),
                }
            }
            Ok(Err(_)) => {
                error!("Channel closed while calling event: {}", event);
                Err(ComputerError::SocketIoError(
                    "Channel closed while waiting for response".to_string(),
                ))
            }
            Err(_) => {
                error!("Timeout while calling event: {}", event);
                Err(ComputerError::SocketIoError(
                    "Timeout while waiting for response".to_string(),
                ))
            }
        }
    }

    /// 处理工具调用事件（带ACK响应）
    /// Handle tool call event (with ACK response)
    async fn handle_tool_call_with_ack(
        payload: Payload,
        manager: Arc<RwLock<Option<MCPServerManager>>>,
        computer_name: String,
        ops: Option<Arc<dyn ComputerHandlerOps>>,
        _client: Client,
    ) -> ComputerResult<(Option<i32>, Value)> {
        let (ack_id, req) = Self::extract_ack_and_parse::<ToolCallReq>(payload)?;

        // 验证 computer_name（Server 路由已保证请求来自同一 office，无需验证 agent 字段）
        // Validate computer_name (Server routing ensures request is from same office, no need to validate agent field)
        if computer_name != req.computer {
            return Err(ComputerError::ValidationError(format!(
                "Computer name mismatch: expected {}, got {}",
                computer_name, req.computer
            )));
        }

        // timeout=0 → 无超时（None）；否则秒。两条执行路径（ops / 旧 manager 兜底）从同一规则派生，
        // 语义统一（fix-review #4：避免 0 在兜底路径被当 0s-立即超时）。/ unify timeout 0 = no timeout.
        let timeout = if req.timeout > 0 {
            Some(f64::from(req.timeout))
        } else {
            None
        };
        let timeout_duration = timeout.map(std::time::Duration::from_secs_f64);

        let result_value = match ops {
            // INT-03 #72：经 Computer::execute_tool_cancellable（支持 notify:tool_call_cancel 协作式取消
            // + 取消/超时结果级 meta），随后对超内联预算的二进制 content item 铸造 toolspool 旁路句柄。
            Some(ops) => {
                let result = ops
                    .execute_tool_cancellable(
                        req.base.req_id.as_str(),
                        &req.tool_name,
                        req.params,
                        timeout,
                    )
                    .await?;
                let mut value =
                    serde_json::to_value(result).map_err(ComputerError::SerializationError)?;
                // #92：顶层结果级 `_meta`（rmcp rename）→ 协议规范的 `meta`（producer MUST，跨 SDK 互通）。
                // 先提升顶层、再铸造 content 旁路（二者操作不相交键：顶层 meta vs content[*]._meta）。
                promote_result_meta_to_meta(&mut value);
                // 铸造旁路（>内联预算的二进制 → toolspool 句柄 + 内联清空；超 too_large_cap 拒绝 + WARN）。
                // mint 失败/小尺寸不致命：保留原内联，不阻断 tool_call 应答。
                mint_oversize_binary_content(ops.as_ref(), &mut value).await;
                value
            }
            // 兼容旧入口（无 Computer ops）：退回 manager.execute_tool（无取消/铸造）。
            None => {
                // #178：克隆 manager 出读锁（守卫语句结束即释放），rmcp await 无锁进行。
                let Some(mgr) = manager.read().await.as_ref().cloned() else {
                    return Err(ComputerError::InvalidState(
                        "MCP Manager not initialized".to_string(),
                    ));
                };
                let result = mgr
                    .execute_tool(&req.tool_name, req.params, timeout_duration)
                    .await?;
                let mut value =
                    serde_json::to_value(result).map_err(ComputerError::SerializationError)?;
                // #92：同上——旧 manager 兜底路径亦须把顶层 `_meta` 提升为 `meta`（无标记则 no-op）。
                promote_result_meta_to_meta(&mut value);
                value
            }
        };

        info!("Tool call executed successfully: {}", req.tool_name);
        Ok((ack_id, result_value))
    }

    /// 处理获取工具列表事件（带ACK响应）
    /// Handle get tools event (with ACK response)
    async fn handle_get_tools_with_ack(
        payload: Payload,
        manager: Arc<RwLock<Option<MCPServerManager>>>,
        computer_name: String,
        _client: Client,
    ) -> ComputerResult<(Option<i32>, Value)> {
        let (ack_id, req) = Self::extract_ack_and_parse::<GetToolsReq>(payload)?;

        // 验证 computer_name（Server 路由已保证请求来自同一 office，无需验证 agent 字段）
        // Validate computer_name (Server routing ensures request is from same office, no need to validate agent field)
        if computer_name != req.computer {
            return Err(ComputerError::ValidationError(format!(
                "Computer name mismatch: expected {}, got {}",
                computer_name, req.computer
            )));
        }

        // 获取工具列表 / Get tools list
        // #178：克隆 manager 出读锁（守卫语句结束即释放），rmcp await 无锁进行。
        let Some(mgr) = manager.read().await.as_ref().cloned() else {
            return Err(ComputerError::InvalidState(
                "MCP Manager not initialized".to_string(),
            ));
        };
        let tools: Vec<smcp::SMCPTool> = {
            // 转换Tool为SMCPTool
            // Convert Tool to SMCPTool
            // 携 bundle_id 拉取（协议 0.3.0 D1 / #136）：每个 SMCPTool 必带其所属 server 的
            // 解析后 bundle_id，供 Agent 归属工具（禁切 exposed `__` 前缀反推）。
            let tool_list = mgr.list_available_tools_with_bundle_id().await;
            tool_list
                .into_iter()
                .map(|(bundle_id, tool)| convert_tool_to_smcp_tool(tool, bundle_id.as_str()))
                .collect()
        };

        let response = GetToolsRet {
            tools: tools.clone(),
            req_id: req.base.req_id,
        };

        info!(
            "Returned {} tools for agent {}",
            tools.len(),
            req.base.agent
        );
        Ok((ack_id, serde_json::to_value(response)?))
    }

    /// 处理获取配置事件（带ACK响应）
    /// Handle get config event (with ACK response)
    async fn handle_get_config_with_ack(
        payload: Payload,
        manager: Arc<RwLock<Option<MCPServerManager>>>,
        computer_name: String,
        ops: Option<Arc<dyn ComputerHandlerOps>>,
        _client: Client,
        inputs: Arc<RwLock<HashMap<String, MCPServerInput>>>,
    ) -> ComputerResult<(Option<i32>, Value)> {
        let (ack_id, req) = Self::extract_ack_and_parse::<GetComputerConfigReq>(payload)?;

        // 验证 computer_name（Server 路由已保证请求来自同一 office，无需验证 agent 字段）
        // Validate computer_name (Server routing ensures request is from same office, no need to validate agent field)
        if computer_name != req.computer {
            return Err(ComputerError::ValidationError(format!(
                "Computer name mismatch: expected {}, got {}",
                computer_name, req.computer
            )));
        }

        // 获取配置 / Get config
        let servers = if let Some(ops) = ops {
            ops.get_computer_config_servers().await?
        } else {
            // Standalone client compatibility: without Computer operations, the Manager owns the
            // full configurations supplied by its caller.
            // #178：克隆 manager 出读锁（守卫语句结束即释放），rmcp await 无锁进行。
            let Some(mgr) = manager.read().await.as_ref().cloned() else {
                return Err(ComputerError::InvalidState(
                    "MCP Manager not initialized".to_string(),
                ));
            };
            mgr.get_server_configs().await
        };

        // 获取输入定义 / Get input definitions
        // 将 HashMap<String, MCPServerInput> 转换为 Vec<serde_json::Value>
        // Convert HashMap<String, MCPServerInput> to Vec<serde_json::Value>
        let inputs_data = {
            let inputs_guard = inputs.read().await;
            if inputs_guard.is_empty() {
                None
            } else {
                let inputs_vec: Vec<serde_json::Value> = inputs_guard
                    .values()
                    .filter_map(|input| serde_json::to_value(input).ok())
                    .collect();
                if inputs_vec.is_empty() {
                    None
                } else {
                    Some(inputs_vec)
                }
            }
        };

        let response = GetComputerConfigRet {
            servers,
            inputs: inputs_data,
        };

        info!("Returned config for agent {}", req.base.agent);
        Ok((ack_id, serde_json::to_value(response)?))
    }

    /// 处理获取桌面事件（带ACK响应）
    /// Handle get desktop event (with ACK response)
    async fn handle_get_desktop_with_ack(
        payload: Payload,
        manager: Arc<RwLock<Option<MCPServerManager>>>,
        computer_name: String,
        _client: Option<Client>,
    ) -> ComputerResult<(Option<i32>, Value)> {
        let (ack_id, req) = Self::extract_ack_and_parse::<GetDesktopReq>(payload)?;

        // 验证 computer_name（Server 路由已保证请求来自同一 office，无需验证 agent 字段）
        // Validate computer_name (Server routing ensures request is from same office, no need to validate agent field)
        if computer_name != req.computer {
            return Err(ComputerError::ValidationError(format!(
                "Computer name mismatch: expected {}, got {}",
                computer_name, req.computer
            )));
        }

        // 获取桌面窗口信息 / Get desktop window info
        // #178：克隆 manager 出读锁，rmcp await 无锁进行。注意**不能**用 `if let Some(mgr) =
        // manager.read().await.as_ref().cloned()` 的 scrutinee 内联形状——if-let 的临时守卫存活至
        // 整个语句（含 body）结束，读锁仍会跨 `get_windows_details` await 持有（Defect 2 冻结链）。
        // 先 let 绑定（语句结束即释放守卫），再 if-let 消费 owned 克隆。
        let manager_snapshot = manager.read().await.as_ref().cloned();
        let desktops = {
            if let Some(mgr) = manager_snapshot {
                let raw_windows = mgr.get_windows_details(req.window.as_deref()).await;
                let windows: Vec<WindowInfo> = raw_windows
                    .into_iter()
                    .map(|(bundle_id, server_name, resource, read_result)| {
                        // `WindowInfo.bundle_id` 仍标 `ServerName`（desktop/model.rs 的既有误标）——
                        // newtype 一上来就照出了它；改型归 #132 卫生批次，此处先按边界转换。
                        WindowInfo::new(bundle_id.into_string(), server_name, resource, read_result)
                    })
                    .collect();
                organize_desktop(windows, req.desktop_size.map(|s| s as usize), &[])
            } else {
                Vec::new()
            }
        };

        let response = GetDesktopRet {
            desktops: Some(desktops),
            req_id: req.base.req_id,
        };

        info!("Returned desktop for agent {}", req.base.agent);
        Ok((ack_id, serde_json::to_value(response)?))
    }

    /// 处理资源发现事件（带 ACK 响应）/ Handle get_resources event (with ACK).
    ///
    /// 透明转发指定 MCP Server 的 `resources/list`：单页透传（cursor 入参/`next_cursor` 出参原样转发），
    /// **不**聚合、**不**做 scheme/元数据过滤、**不**返回 `resourceTemplates`。错误经 ACK 第一参回传
    /// **flat ErrorPayload**（禁止嵌套 envelope）：未知 `mcp_server` → 4014（顶层平铺 `mcp_server`）；
    /// 目标 server 未声明 `resources` 能力 → 4015（顶层平铺 `mcp_server` + `capability`）。对齐
    /// Python `on_get_resources`（RES-01 #30，协议 0.2.0 `client:get_resources`）。
    async fn handle_get_resources_with_ack(
        payload: Payload,
        manager: Arc<RwLock<Option<MCPServerManager>>>,
        computer_name: String,
    ) -> ComputerResult<(Option<i32>, Value)> {
        let (ack_id, req) = Self::extract_ack_and_parse::<GetResourcesReq>(payload)?;

        // 验证 computer_name（Server 路由已保证请求来自同一 office，无需验证 agent 字段）
        if computer_name != req.computer {
            return Err(ComputerError::ValidationError(format!(
                "Computer name mismatch: expected {}, got {}",
                computer_name, req.computer
            )));
        }

        // 单页透传 MCP `resources/list` / single-page passthrough。
        // #178：克隆 manager 出读锁（守卫语句结束即释放），rmcp await 无锁进行。
        let Some(mgr) = manager.read().await.as_ref().cloned() else {
            return Err(ComputerError::InvalidState(
                "MCP Manager not initialized".to_string(),
            ));
        };
        let result = mgr
            .list_resources(&req.mcp_server, req.cursor.clone())
            .await;

        match result {
            Ok((resources, next_cursor)) => {
                let response = GetResourcesRet {
                    resources: resources.iter().map(to_a2c_resource).collect(),
                    next_cursor,
                    req_id: Some(req.base.req_id),
                };
                info!(
                    "Returned {} resource(s) from '{}' for agent {}",
                    response.resources.len(),
                    req.mcp_server,
                    req.base.agent
                );
                Ok((ack_id, serde_json::to_value(response)?))
            }
            // 未知 mcp_server → 4014 flat ErrorPayload（ACK 第一参回传）。
            Err(ComputerError::McpServerNotFound(server)) => {
                warn!(
                    "client:get_resources references unregistered MCP server '{}'",
                    server
                );
                let payload = smcp::ErrorPayload::from_error_code(
                    smcp::ErrorCode::McpServerNotFound,
                    "MCP Server not registered",
                )
                .with_mcp_server(server);
                Ok((ack_id, serde_json::to_value(payload)?))
            }
            // capability 不支持 → 4015 flat ErrorPayload。
            Err(ComputerError::McpCapabilityNotSupported {
                bundle_id,
                capability,
            }) => {
                warn!(
                    "client:get_resources MCP server '{}' does not support '{}' capability",
                    bundle_id, capability
                );
                let payload = smcp::ErrorPayload::from_error_code(
                    smcp::ErrorCode::McpCapabilityNotSupported,
                    "MCP Server does not support the requested capability",
                )
                .with_mcp_server(bundle_id)
                .with_capability(capability);
                Ok((ack_id, serde_json::to_value(payload)?))
            }
            Err(other) => Err(other),
        }
    }

    /// 处理 SKILL 清单事件 `client:get_skills`（带 ACK，INT-03 #72）/ Handle get_skills (with ACK)。
    ///
    /// 透传 [`ComputerHandlerOps::get_skills`]（仅活跃 SKILL，排除孤儿）。对齐 Python `on_get_skills`。
    async fn handle_get_skills_with_ack(
        payload: Payload,
        ops: Option<Arc<dyn ComputerHandlerOps>>,
        computer_name: String,
    ) -> ComputerResult<(Option<i32>, Value)> {
        let (ack_id, req) = Self::extract_ack_and_parse::<GetSkillsReq>(payload)?;
        if computer_name != req.computer {
            return Err(ComputerError::ValidationError(format!(
                "Computer name mismatch: expected {}, got {}",
                computer_name, req.computer
            )));
        }
        let ops = ops.ok_or_else(|| {
            ComputerError::InvalidState("Computer ops not available for get_skills".to_string())
        })?;
        let skills = ops.get_skills().await;
        let ret = GetSkillsRet {
            skills,
            req_id: Some(req.base.req_id),
        };
        Ok((ack_id, serde_json::to_value(ret)?))
    }

    /// 处理 SKILL 单资源事件 `client:get_skill`（带 ACK，INT-03 #72）/ Handle get_skill (with ACK)。
    ///
    /// 错误码（flat ErrorPayload，ACK 第一参）：`name` 格式非法 → `4016`；格式合法但未注册/孤儿 →
    /// `4014`；`rel_path` 沙箱不可达（traversal/forbidden/not_found/too_large）→ `4017`。成功：`body`
    /// 与 `blob_handle` **恰一**——文本且 ≤ 内联预算且 UTF-8 → `body`；否则铸 skill `blob_handle`。
    /// 对齐 Python `on_get_skill`。
    async fn handle_get_skill_with_ack(
        payload: Payload,
        ops: Option<Arc<dyn ComputerHandlerOps>>,
        computer_name: String,
    ) -> ComputerResult<(Option<i32>, Value)> {
        let (ack_id, req) = Self::extract_ack_and_parse::<GetSkillReq>(payload)?;
        if computer_name != req.computer {
            return Err(ComputerError::ValidationError(format!(
                "Computer name mismatch: expected {}, got {}",
                computer_name, req.computer
            )));
        }
        let ops = ops.ok_or_else(|| {
            ComputerError::InvalidState("Computer ops not available for get_skill".to_string())
        })?;

        // 1) name 格式校验 → 4016（格式硬错，先于 registry）。
        if let Err(e) = parse_skill_name(&req.name) {
            warn!("client:get_skill invalid skill name {:?}: {}", req.name, e);
            let payload =
                ErrorPayload::from_error_code(ErrorCode::SkillNameInvalid, "Invalid SKILL name")
                    .with_detail("name", req.name.clone());
            return Ok((ack_id, serde_json::to_value(payload)?));
        }

        // 2) registry 查找 → 4014（格式合法但未注册/孤儿）。
        let Some(skill_ref) = ops.get_skill_ref(&req.name).await else {
            warn!(
                "client:get_skill name not in registry (unregistered or orphaned): {:?}",
                req.name
            );
            let payload =
                ErrorPayload::from_error_code(ErrorCode::McpServerNotFound, "SKILL not found")
                    .with_detail("name", req.name.clone());
            return Ok((ack_id, serde_json::to_value(payload)?));
        };

        // 3) 沙箱解析资源 → 4017（traversal/forbidden/not_found/too_large）。
        let view = match ops.read_skill_resource(&skill_ref, req.rel_path.as_deref()) {
            Ok(v) => v,
            Err(e) => {
                warn!(
                    "client:get_skill resource not accessible: name={:?} reason={} rel={:?}",
                    req.name, e.reason, e.rel_path
                );
                return Ok((ack_id, serde_json::to_value(e.to_error_payload())?));
            }
        };

        // 4) body XOR blob_handle：文本 + ≤ 内联预算 + UTF-8 → body；否则铸 skill 句柄。
        let budget = ops.blob_thresholds().inline_budget;
        let mut body: Option<String> = None;
        if view.is_text && view.total_size <= budget {
            if let Ok(bytes) = view.read_all() {
                match String::from_utf8(bytes) {
                    Ok(text) => body = Some(text),
                    // 文本 MIME 但非 UTF-8 → 回退 blob_handle（保守，对齐 Python）。
                    Err(_) => debug!(
                        "client:get_skill {:?} rel={:?} textual mime but not UTF-8; routing to blob_handle",
                        req.name, view.rel_path
                    ),
                }
            }
        }
        // too_large 已在 read_skill_resource 拦截，此处铸造安全。
        let blob_handle = if body.is_none() {
            Some(encode_skill_handle(&req.name, &view.rel_path))
        } else {
            None
        };

        let ret = GetSkillRet {
            name: Some(req.name),
            rel_path: Some(view.rel_path),
            mime_type: Some(view.mime),
            total_size: Some(view.total_size),
            sha256: Some(view.sha256),
            body,
            blob_handle,
            req_id: Some(req.base.req_id),
        };
        Ok((ack_id, serde_json::to_value(ret)?))
    }

    /// 处理通用二进制拉取事件 `client:get_blob`（带 ACK，INT-03 #72）/ Handle get_blob (with ACK)。
    ///
    /// 解码句柄 → 路由 resolver（重施铸造通道鉴权）→ 切片（`max_chunk_bytes` clamp）→ base64。范围守卫
    /// **严格 `>`**：`offset == total_size` 是 EOF probe（返回空块 + `eof=true`），仅 `offset > total_size`
    /// 才 `4018 range`。句柄失效/越权/消失 → `4018`（reason ∈ invalid_handle/forbidden/gone/range）。
    /// 对齐 Python `on_get_blob`。
    async fn handle_get_blob_with_ack(
        payload: Payload,
        ops: Option<Arc<dyn ComputerHandlerOps>>,
        computer_name: String,
    ) -> ComputerResult<(Option<i32>, Value)> {
        let (ack_id, req) = Self::extract_ack_and_parse::<GetBlobReq>(payload)?;
        if computer_name != req.computer {
            return Err(ComputerError::ValidationError(format!(
                "Computer name mismatch: expected {}, got {}",
                computer_name, req.computer
            )));
        }
        let ops = ops.ok_or_else(|| {
            ComputerError::InvalidState("Computer ops not available for get_blob".to_string())
        })?;

        let thresholds = ops.blob_thresholds();
        let max_chunk = thresholds.clamp_chunk(req.max_chunk_bytes.map(|v| v as i64));
        let chunk_offset = req.chunk_offset.unwrap_or(0);

        // 解码 + 路由 + 重施鉴权 → 4018（按 BlobHandleError::reason 映射）。
        let resolved = match ops.resolve_blob(&req.blob_handle).await {
            Ok(r) => r,
            Err(e) => {
                warn!(
                    "client:get_blob handle rejected: reason={}, err={}",
                    e.reason(),
                    e
                );
                return Ok((
                    ack_id,
                    serde_json::to_value(blob_error_payload(e.reason()))?,
                ));
            }
        };

        // 范围守卫：严格 `>`（offset==total_size 为 EOF probe，返回空块 + eof）。
        if chunk_offset > resolved.total_size {
            warn!(
                "client:get_blob range out of bounds: offset={}, total_size={}",
                chunk_offset, resolved.total_size
            );
            return Ok((ack_id, serde_json::to_value(blob_error_payload("range"))?));
        }

        let remaining = resolved.total_size - chunk_offset;
        let slice_len = max_chunk.min(remaining);
        let chunk = match resolved.slice(chunk_offset, slice_len) {
            Ok(c) => c,
            Err(e) => {
                return Ok((
                    ack_id,
                    serde_json::to_value(blob_error_payload(e.reason()))?,
                ));
            }
        };
        // eof：保留 len(chunk) 形式（对短读稳健）。
        let eof = chunk_offset + chunk.len() as u64 == resolved.total_size;

        let ret = GetBlobRet {
            blob_handle: req.blob_handle,
            mime_type: Some(resolved.mime),
            total_size: resolved.total_size,
            sha256: resolved.sha256,
            chunk_offset,
            eof,
            blob: base64::engine::general_purpose::STANDARD.encode(&chunk),
            req_id: Some(req.base.req_id),
        };
        Ok((ack_id, serde_json::to_value(ret)?))
    }

    /// 处理取消通知 `notify:tool_call_cancel`（**无 ACK**，fire-and-forget，INT-03 #72 + 取消纵切）。
    ///
    /// 解析广播载体 `{agent, req_id}`（[`AgentCallData`]），fire 对应在途调用的取消令牌
    /// （[`ComputerHandlerOps::acancel_tool`]）。取消为协作式——远端是否真正停止不保证；未知 req_id
    /// 幂等 no-op。无应答（与 Server fire-and-forget 广播语义一致）。
    async fn handle_tool_call_cancel(payload: Payload, ops: Option<Arc<dyn ComputerHandlerOps>>) {
        let Some(ops) = ops else {
            warn!("notify:tool_call_cancel received but Computer ops not available");
            return;
        };
        match Self::extract_ack_and_parse::<AgentCallData>(payload) {
            Ok((_ack, data)) => {
                let cancelled = ops.acancel_tool(data.req_id.as_str()).await;
                debug!(
                    "notify:tool_call_cancel req_id={} → cancelled={}",
                    data.req_id.as_str(),
                    cancelled
                );
            }
            Err(e) => warn!("notify:tool_call_cancel parse failed: {}", e),
        }
    }

    /// Whether a callback payload contains the given textual close reason.
    fn payload_contains_text(payload: &Payload, expected: &str) -> bool {
        match payload {
            Payload::Text(values, _) => values.iter().any(|value| value.as_str() == Some(expected)),
            #[allow(deprecated)]
            Payload::String(value, _) => value == expected,
            Payload::Binary(_, _) => false,
        }
    }

    /// 从payload中提取ack_id并解析数据
    /// Extract ack_id from payload and parse data
    fn extract_ack_and_parse<T: serde::de::DeserializeOwned>(
        payload: Payload,
    ) -> ComputerResult<(Option<i32>, T)> {
        match payload {
            Payload::Text(mut values, ack_id) => {
                if let Some(value) = values.pop() {
                    let req =
                        serde_json::from_value(value).map_err(ComputerError::SerializationError)?;
                    Ok((ack_id, req))
                } else {
                    Err(ComputerError::ProtocolError("Empty payload".to_string()))
                }
            }
            #[allow(deprecated)]
            Payload::String(s, ack_id) => {
                let req = serde_json::from_str(&s).map_err(ComputerError::SerializationError)?;
                Ok((ack_id, req))
            }
            Payload::Binary(_, _) => Err(ComputerError::SocketIoError(
                "Binary payload not supported".to_string(),
            )),
        }
    }

    /// 仅提取ack_id（用于错误处理）
    /// Extract ack_id only (for error handling)
    fn extract_ack_id(payload: Payload) -> ComputerResult<(Option<i32>, ())> {
        match payload {
            Payload::Text(_, ack_id) => Ok((ack_id, ())),
            #[allow(deprecated)]
            Payload::String(_, ack_id) => Ok((ack_id, ())),
            Payload::Binary(_, _) => Ok((None, ())),
        }
    }

    /// 断开连接（发 Socket.IO DISCONNECT 包并关 transport）。
    ///
    /// #148：取 `&self`（底层 [`tf_rust_socketio::asynchronous::Client::disconnect`] 本就 `&self`），
    /// 解除「`disconnect(self)` 按值取参被 `Arc` 挡住」的约束——使 `Computer::disconnect_socketio` /
    /// `Computer::shutdown`（见 `computer` 模块）能直接对槽内 `Arc<SmcpComputerClient>` 调用，无需
    /// 调用方 `try_unwrap` 取出内部句柄（这正是 tfrobot-client TFRC-65 要消除的封装边界缺口）。
    ///
    /// Disconnect from server (sends the Socket.IO DISCONNECT packet and closes the transport).
    pub async fn disconnect(&self) -> ComputerResult<()> {
        debug!("Disconnecting from server");
        // This synchronous gate also covers a tf-rust-socketio reconnect loop that has already
        // entered backoff: any later Connect is closed inline and all application events are inert.
        self.runtime_status.begin_retiring(&self.retiring);
        // tf-rust-socketio marks manual teardown internally and does not emit Event::Close. Invalidate
        // before awaiting transport shutdown so an in-flight ACK can never resurrect Office state.
        let rejoin_task = {
            let mut membership = lock_office_membership(&self.office_membership);
            let rejoin_task = membership.invalidate_connection(false);
            // Publish the membership/lifecycle pair before the first cancellation point. If this
            // future is dropped while the transport is closing, callers must not observe the old
            // JoinedOffice state after desired/confirmed membership has already been cleared.
            self.runtime_status.transition(LifecycleState::Started);
            rejoin_task
        };
        if let Some(task) = rejoin_task {
            task.abort();
        }

        let result = match self.client.disconnect().await {
            Ok(()) | Err(tf_rust_socketio::Error::IllegalActionBeforeOpen()) => Ok(()),
            Err(error) => Err(ComputerError::SocketIoError(format!(
                "Failed to disconnect: {error}"
            ))),
        };
        result.map(|()| info!("Disconnected from server"))
    }

    /// 获取当前office ID
    /// Get current office ID
    pub async fn get_office_id(&self) -> Option<String> {
        lock_office_membership(&self.office_membership)
            .confirmed
            .clone()
    }

    /// 获取连接的 URL
    /// Get connected URL
    pub fn get_url(&self) -> String {
        // 由于 tf_rust_socketio 的 Client 没有 uri() 方法，返回默认值
        // Since tf_rust_socketio Client doesn't have uri() method, return default
        "unknown".to_string()
    }

    /// 获取握手时使用的 Socket.IO namespace（实际配置值，非字面量）。
    /// Get the Socket.IO namespace used at handshake time (the actual
    /// configured value, not a hardcoded literal).
    pub fn get_namespace(&self) -> String {
        self.namespace.clone()
    }
}

/// 将 MCP `Resource` 转换为 A2C 协议层 [`smcp::A2CResource`]（snake_case
/// mirror）/ Convert an MCP `Resource` to the A2C protocol-level `A2CResource`。
///
/// 元数据分工 / metadata partition：MCP 标准 annotations（`priority`/`audience`/`last_modified`）→
/// `annotations`；rmcp `_meta`（A2C 扩展，如 `fullscreen`）原样搬运到 `_meta`。`audience` 的
/// `Role::{User,Assistant}` 映射到 [`smcp::ResourceAudience`]；`last_modified` 的 `DateTime<Utc>`
/// 序列化为 RFC3339（ISO 8601）字符串。对齐 Python `_to_a2c_resource`（RES-01 #30）。
pub(crate) fn to_a2c_resource(resource: &crate::mcp_clients::model::Resource) -> smcp::A2CResource {
    let annotations = resource
        .annotations
        .as_ref()
        .map(|ann| smcp::ResourceAnnotations {
            audience: ann.audience.as_ref().map(|roles| {
                roles
                    .iter()
                    .map(|role| match role {
                        rmcp::model::Role::User => smcp::ResourceAudience::User,
                        rmcp::model::Role::Assistant => smcp::ResourceAudience::Assistant,
                    })
                    .collect()
            }),
            priority: ann.priority,
            last_modified: ann.last_modified.map(|dt| dt.to_rfc3339()),
        })
        // 与 Python `_to_a2c_resource` 的 `if ann:` 守卫对齐：三字段全 None 时折叠为 None，避免线格式
        // 产出空 `"annotations": {}`（rmcp 把入参 `"annotations": {}` 解析为 `Some(全 None)`）造成
        // 跨-SDK 字节分歧 / fold to None when all sub-fields are None, mirroring Python's truthiness guard.
        .filter(|a| a.audience.is_some() || a.priority.is_some() || a.last_modified.is_some());

    // rmcp `_meta`（`Meta(JsonObject)`）→ `serde_json::Value::Object`，原样搬运 A2C 扩展字段。
    let meta = resource
        .meta
        .as_ref()
        .map(|m| serde_json::Value::Object(m.0.clone()));

    smcp::A2CResource {
        uri: Some(resource.uri.clone()),
        name: Some(resource.name.clone()),
        description: resource.description.clone(),
        mime_type: resource.mime_type.clone(),
        size: resource.size,
        annotations,
        meta,
    }
}

/// 将内部 `Tool`（rmcp 模型）转成协议类型 `SMCPTool` / Convert internal Tool to protocol SMCPTool。
///
/// `bundle_id`：该工具所属 MCP Server 的**解析后** `bundle_id`（协议 0.3.0 D1 / #136），由调用方从
/// [`list_available_tools_with_bundle_id`](crate::mcp_clients::manager::MCPServerManager::list_available_tools_with_bundle_id)
/// 与 `tool` 配对传入——**绝不**从 `tool.name`（exposed 名）切 `__` 前缀反推。
pub(crate) fn convert_tool_to_smcp_tool(
    tool: crate::mcp_clients::model::Tool,
    bundle_id: &str,
) -> smcp::SMCPTool {
    let mut meta_map = serde_json::Map::new();

    // 传递 tool.meta 中的所有键值（如 a2c_tool_meta）
    // 值需要序列化为 JSON 字符串，与 Python SDK 对齐
    if let Some(existing_meta) = &tool.meta {
        for (k, v) in existing_meta.iter() {
            let str_val = if v.is_string() {
                v.as_str().unwrap().to_string()
            } else {
                serde_json::to_string(v).unwrap_or_default()
            };
            meta_map.insert(k.clone(), serde_json::Value::String(str_val));
        }
    }

    // 添加 MCP_TOOL_ANNOTATION
    if let Some(annotations) = &tool.annotations {
        if let Ok(json_str) = serde_json::to_string(annotations) {
            meta_map.insert(
                "MCP_TOOL_ANNOTATION".to_string(),
                serde_json::Value::String(json_str),
            );
        }
    }

    let meta = if meta_map.is_empty() {
        None
    } else {
        Some(serde_json::Value::Object(meta_map))
    };

    let description = tool.description.as_deref().unwrap_or("").to_string();
    let params_schema = tool.schema_as_json_value();
    smcp::SMCPTool {
        name: tool.name.to_string(),
        bundle_id: bundle_id.to_string(),
        description,
        params_schema,
        return_schema: None,
        meta,
    }
}

/// 构造 `4018 Blob Not Accessible` flat ErrorPayload（`details.reason` ∈ invalid_handle/forbidden/
/// gone/range）/ build a flat 4018 ErrorPayload。供 `client:get_blob` handler 回传（ACK 第一参）。
fn blob_error_payload(reason: &str) -> ErrorPayload {
    ErrorPayload::from_error_code(ErrorCode::BlobNotAccessible, "Blob not accessible")
        .with_detail("reason", reason)
}

/// 对 `CallToolResult`（已序列化为 `Value`）的超内联预算二进制 content item 铸造 toolspool 旁路句柄
/// （INT-03 #72）/ mint oversize binary content items into toolspool sideband handles。
///
/// 遍历 `content[*]`，检出二进制载体（顶层 `data`+`mimeType`，或 `EmbeddedResource` 的
/// `resource.blob`+`resource.mimeType`），base64 解码后按**三档阈值**：
/// - `size ≤ inline_budget` → 保留内联（跳过）；
/// - `size > too_large_cap` → 拒绝铸造 + WARN（DoS 防御，**不**写句柄、**不**清内联）；
/// - 介于之间 → `mint_toolspool_handle` 铸 cid 句柄，**清空**内联 `data`/`blob`，并经
///   [`set_content_blob_sideband`] 写 `_meta.a2c_blob_handle`(+`a2c_total_size`/`a2c_sha256`)
///   （该 helper 规范化非 dict `_meta`，避免孤儿 cid）。
///
/// 容错：解码失败 / mint 失败均仅跳过该 item（保留内联），**不**阻断 tool_call 应答。对齐 Python
/// `_mint_oversize_binary_content`。
async fn mint_oversize_binary_content(ops: &dyn ComputerHandlerOps, raw: &mut Value) {
    let thresholds = ops.blob_thresholds();
    let budget = thresholds.inline_budget;
    let too_large = thresholds.too_large_cap;

    let Some(content) = raw.get_mut("content").and_then(Value::as_array_mut) else {
        return;
    };

    for item in content.iter_mut() {
        // 1) 只读提取候选载体（不跨 await 持有借用）：(base64, mime, is_resource, meta_is_bad)。
        // `meta_is_bad`：`_meta` 存在且既非 null 又非 object（如字符串/数字/数组）——畸形 MCP 输入，
        // 铸造时须跳过（见 fix-review #2，对齐 Python `_mint_oversize_binary_content`）。
        let candidate: Option<(String, String, bool, bool)> = {
            let Some(obj) = item.as_object() else {
                continue;
            };
            let meta_is_bad = obj
                .get("_meta")
                .is_some_and(|m| !m.is_null() && !m.is_object());
            if let Some(d) = obj.get("data").and_then(Value::as_str) {
                let mime = obj
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                Some((d.to_string(), mime, false, meta_is_bad))
            } else if let Some(res) = obj.get("resource").and_then(Value::as_object) {
                res.get("blob").and_then(Value::as_str).map(|b| {
                    let mime = res
                        .get("mimeType")
                        .and_then(Value::as_str)
                        .or_else(|| obj.get("mimeType").and_then(Value::as_str))
                        .unwrap_or_default()
                        .to_string();
                    (b.to_string(), mime, true, meta_is_bad)
                })
            } else {
                None
            }
        };
        let Some((b64, mime, is_resource, meta_is_bad)) = candidate else {
            continue;
        };

        // 2) 解码 + 三档阈值判定。
        let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64.as_bytes()) else {
            continue; // 非法 base64 → 保留内联，不铸造。
        };
        let size = bytes.len() as u64;
        if size <= budget {
            continue; // 内联预算内 → 保留。
        }
        if size > too_large {
            warn!(
                "on_tool_call binary item size {} exceeds too_large_cap {}; skipping mint",
                size, too_large
            );
            continue; // DoS 防御：不铸造、保留内联。
        }
        // 畸形 `_meta`（非 null 非 object）→ 跳过铸造、保留内联（对齐 Python：避免覆写畸形 _meta 后
        // 落盘孤儿 cid）。null/缺省/object 由 set_content_blob_sideband 正确规范化，照常铸造。
        if meta_is_bad {
            warn!("on_tool_call skipping mint: item['_meta'] is not a dict; keeping inline");
            continue;
        }

        // 3) 铸造（await，无 item 借用）。
        let mime_for_mint = if mime.is_empty() {
            "application/octet-stream"
        } else {
            mime.as_str()
        };
        let handle = match ops.mint_toolspool_handle(&bytes, mime_for_mint).await {
            Ok(h) => h,
            Err(e) => {
                warn!(
                    "on_tool_call mint failed for item size={}: {}; keeping inline",
                    size, e
                );
                continue;
            }
        };
        let sha = smcp::utils::sha256_hex(&bytes);

        // 4) 清空内联载体 + 写 _meta 旁路（set_content_blob_sideband 规范化非 dict _meta）。
        if let Some(obj) = item.as_object_mut() {
            if is_resource {
                if let Some(res) = obj.get_mut("resource").and_then(Value::as_object_mut) {
                    res.insert("blob".to_string(), Value::String(String::new()));
                }
            } else {
                obj.insert("data".to_string(), Value::String(String::new()));
            }
        }
        set_content_blob_sideband(item, &handle, Some(size), Some(&sha));
    }
}

/// 把已序列化 tool_call ack 的**顶层**结果级 `_meta` 重映射为协议规范的 `meta`
/// （data-structures.md §234：producer MUST 写结果级 `meta`）。
///
/// 背景：rmcp `CallToolResult.meta` 为 `#[serde(rename = "_meta")]`（**无条件** rename），故
/// `serde_json::to_value` 后 A2C 结果级标记出线为 `_meta.a2c_*`；而 Python 参考实现用 `result.meta=`
/// 配合 `model_dump(mode="json")`（按字段名 dump）出线为 `meta`。为跨 SDK 互通，须在 wire 边界把顶层
/// `_meta` 整体提升为 `meta`，覆盖所有结果级标记：`a2c_cancelled`、`a2c_cancel_reason`、`a2c_timeout`
/// 及 AUTH-01 授权失败键（`error_code`、`mcp_server`、`auth_hint` 等，同根因顺带覆盖）。
///
/// 边界：**仅**动顶层；**不**触碰 `content[*]._meta`（blob 句柄子级，data-structures.md §683 规定
/// MUST 保持 `_meta`）。非 object 的顶层 `_meta`（畸形）原样保留。已有顶层 `meta` 时按键合并、不覆盖
/// 既有键（幂等、防丢键）。
fn promote_result_meta_to_meta(raw: &mut Value) {
    let Some(obj) = raw.as_object_mut() else {
        return;
    };
    // 仅当顶层 `_meta` 为 object 时提升；非 object（畸形）原样保留、不报错。
    if !obj.get("_meta").is_some_and(Value::is_object) {
        return;
    }
    let Some(Value::Object(underscored)) = obj.remove("_meta") else {
        return; // 不可达（上面已确认是 object），防御性返回。
    };
    // 并入顶层 `meta`：缺省新建；已存在则按键合并、不覆盖既有键（幂等、防丢键）。
    let meta = obj
        .entry("meta".to_string())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    match meta.as_object_mut() {
        Some(meta_map) => {
            for (k, v) in underscored {
                meta_map.entry(k).or_insert(v);
            }
        }
        // 顶层 `meta` 已存在但非 object（畸形，rmcp 不会产生）：不破坏既有值，整体放回 `_meta`。
        None => {
            obj.insert("_meta".to_string(), Value::Object(underscored));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_clients::model::{Tool, ToolAnnotations};
    use serde_json::json;

    fn make_tool(
        meta: Option<serde_json::Map<String, serde_json::Value>>,
        annotations: Option<ToolAnnotations>,
    ) -> Tool {
        use std::sync::Arc;
        let input_schema: serde_json::Map<String, serde_json::Value> =
            serde_json::from_value(json!({"type": "object"})).unwrap();
        let mut tool = Tool::new("test_tool", "A test tool", Arc::new(input_schema));
        tool.annotations = annotations;
        tool.meta = meta.map(rmcp::model::Meta);
        tool
    }

    #[test]
    fn test_tool_to_smcp_tool_with_meta_and_annotations() {
        let mut meta = serde_json::Map::new();
        meta.insert(
            "a2c_tool_meta".to_string(),
            json!({"tags": ["browser"], "priority": 1}),
        );
        let annotations = ToolAnnotations::from_raw(
            Some("Test".to_string()),
            Some(false),
            Some(false),
            None,
            Some(false),
        );
        let smcp_tool =
            convert_tool_to_smcp_tool(make_tool(Some(meta), Some(annotations)), "fs-bundle");

        // 协议 0.3.0 D1 / #136：bundle_id 由调用方传入，原样填 wire 字段（不受 meta/annotations 影响）。
        assert_eq!(smcp_tool.bundle_id, "fs-bundle");
        let meta_obj = smcp_tool.meta.unwrap();
        let meta_map = meta_obj.as_object().unwrap();
        assert!(meta_map.contains_key("a2c_tool_meta"));
        assert!(meta_map.contains_key("MCP_TOOL_ANNOTATION"));
        // Values should be JSON strings
        assert!(meta_map["a2c_tool_meta"].is_string());
        assert!(meta_map["MCP_TOOL_ANNOTATION"].is_string());
    }

    #[test]
    fn test_tool_to_smcp_tool_only_meta() {
        let mut meta = serde_json::Map::new();
        meta.insert("a2c_tool_meta".to_string(), json!({"tags": ["fs"]}));
        let smcp_tool = convert_tool_to_smcp_tool(make_tool(Some(meta), None), "test_bundle");

        let meta_obj = smcp_tool.meta.unwrap();
        let meta_map = meta_obj.as_object().unwrap();
        assert_eq!(meta_map.len(), 1);
        assert!(meta_map.contains_key("a2c_tool_meta"));
    }

    #[test]
    fn test_tool_to_smcp_tool_only_annotations() {
        let annotations = ToolAnnotations::from_raw(
            Some("My Tool".to_string()),
            Some(true),
            Some(false),
            None,
            Some(false),
        );
        let smcp_tool =
            convert_tool_to_smcp_tool(make_tool(None, Some(annotations)), "test_bundle");

        let meta_obj = smcp_tool.meta.unwrap();
        let meta_map = meta_obj.as_object().unwrap();
        assert_eq!(meta_map.len(), 1);
        assert!(meta_map.contains_key("MCP_TOOL_ANNOTATION"));
    }

    #[test]
    fn test_tool_to_smcp_tool_no_meta_no_annotations() {
        let smcp_tool = convert_tool_to_smcp_tool(make_tool(None, None), "test_bundle");
        assert_eq!(smcp_tool.bundle_id, "test_bundle");
        assert!(smcp_tool.meta.is_none());
    }

    #[test]
    fn test_tool_to_smcp_tool_a2c_tool_meta_canonical_string() {
        // #200：wire 线格式 —— `a2c_tool_meta` 终值为 JSON **字符串**（非嵌套对象），解析后含 ToolMeta
        // **全字段**（未设置=null，与 python `model_dump(mode="json")` 同形）；原生 `_meta` key 原样保留。
        let mut meta = serde_json::Map::new();
        meta.insert("custom_key".to_string(), serde_json::json!("v"));
        meta.insert(
            "a2c_tool_meta".to_string(),
            serde_json::json!({
                "auto_apply": null, "alias": null, "tags": ["read"], "ret_object_mapper": null
            }),
        );
        let smcp_tool = convert_tool_to_smcp_tool(make_tool(Some(meta), None), "test_bundle");

        let meta_map = smcp_tool.meta.unwrap().as_object().unwrap().clone();
        assert_eq!(
            meta_map["custom_key"],
            serde_json::json!("v"),
            "原生 _meta key 原样保留"
        );
        let wire = meta_map["a2c_tool_meta"]
            .as_str()
            .expect("a2c_tool_meta MUST 是 JSON 字符串");
        let parsed: serde_json::Value = serde_json::from_str(wire).unwrap();
        assert_eq!(
            parsed,
            serde_json::json!({
                "auto_apply": null, "alias": null, "tags": ["read"], "ret_object_mapper": null
            }),
            "wire 字符串解析后应为全字段 canonical（双 SDK 同形）"
        );
    }

    #[test]
    fn test_tool_to_smcp_tool_string_value_not_double_serialized() {
        let mut meta = serde_json::Map::new();
        meta.insert(
            "simple_key".to_string(),
            serde_json::Value::String("already_a_string".to_string()),
        );
        let smcp_tool = convert_tool_to_smcp_tool(make_tool(Some(meta), None), "test_bundle");

        let meta_obj = smcp_tool.meta.unwrap();
        let meta_map = meta_obj.as_object().unwrap();
        // Should be the raw string, not "\"already_a_string\""
        assert_eq!(meta_map["simple_key"].as_str().unwrap(), "already_a_string");
    }

    // ===== RES-01 #30: to_a2c_resource 转换器 =====

    #[test]
    fn test_to_a2c_resource_full() {
        use crate::mcp_clients::model::Resource;
        use rmcp::model::{Annotations, Meta, Role};

        let mut resource = Resource::new("window://app/w1", "Win");
        resource.description = Some("desc".into());
        resource.mime_type = Some("text/plain".into());
        resource.size = Some(42);
        let mut m = serde_json::Map::new();
        m.insert("fullscreen".into(), serde_json::Value::Bool(true));
        resource.meta = Some(Meta(m));
        // last_modified 设为定值，覆盖 DateTime<Utc> → RFC3339 映射分支。
        let dt: chrono::DateTime<chrono::Utc> = "2025-01-02T03:04:05Z".parse().unwrap();
        resource.annotations = Some(
            Annotations::default()
                .with_audience(vec![Role::Assistant])
                .with_priority(0.7)
                .with_timestamp(dt),
        );
        let a2c = to_a2c_resource(&resource);

        assert_eq!(a2c.uri.as_deref(), Some("window://app/w1"));
        assert_eq!(a2c.name.as_deref(), Some("Win"));
        assert_eq!(a2c.description.as_deref(), Some("desc"));
        assert_eq!(a2c.mime_type.as_deref(), Some("text/plain"));
        assert_eq!(a2c.size, Some(42u64));
        let out_ann = a2c.annotations.expect("annotations preserved");
        assert_eq!(out_ann.priority, Some(0.7));
        assert_eq!(
            out_ann.audience,
            Some(vec![smcp::ResourceAudience::Assistant])
        );
        // RFC3339（ISO 8601）字符串，UTC 以 `+00:00` 结尾。
        assert_eq!(
            out_ann.last_modified.as_deref(),
            Some("2025-01-02T03:04:05+00:00")
        );
        // _meta（A2C 扩展，如 fullscreen）原样搬运。
        assert_eq!(
            a2c.meta.unwrap()["fullscreen"],
            serde_json::Value::Bool(true)
        );
    }

    #[test]
    fn test_to_a2c_resource_empty_annotations_folded() {
        use crate::mcp_clients::model::Resource;
        use rmcp::model::Annotations;

        // MCP server 主动发回 `"annotations": {}` → rmcp 解析为 Some(全 None)。转换器须折叠为 None，
        // 与 Python `if ann:` 守卫字节对齐（否则线格式残留空 `"annotations": {}`）。
        let mut resource = Resource::new("custom://a/b", "R");
        resource.annotations = Some(Annotations::default());
        let a2c = to_a2c_resource(&resource);
        assert!(
            a2c.annotations.is_none(),
            "all-None annotations must fold to None"
        );
    }

    #[test]
    fn test_to_a2c_resource_minimal() {
        // 裸资源（无 annotations / _meta）：仅 uri / name，annotations/meta 缺省。
        let res = crate::mcp_clients::model::make_resource("custom://x/y", "Y", None, None);
        let a2c = to_a2c_resource(&res);
        assert_eq!(a2c.uri.as_deref(), Some("custom://x/y"));
        assert_eq!(a2c.name.as_deref(), Some("Y"));
        assert!(a2c.annotations.is_none());
        assert!(a2c.meta.is_none());
        assert!(a2c.description.is_none());
    }

    // ===== RES-01 #30: on_get_resources 透明转发 handler =====

    /// 构造 `client:get_resources` 的 wire 形态 payload（flatten 后 agent/req_id 顶层）。
    fn get_resources_payload(
        computer: &str,
        mcp_server: &str,
        cursor: Option<&str>,
        ack: i32,
    ) -> Payload {
        let mut obj = json!({
            "agent": "agent-1",
            "req_id": "req-1",
            "computer": computer,
            "mcp_server": mcp_server,
        });
        if let Some(c) = cursor {
            obj.as_object_mut()
                .unwrap()
                .insert("cursor".into(), json!(c));
        }
        Payload::Text(vec![obj], Some(ack))
    }

    fn mock(
        pages: Vec<Vec<crate::mcp_clients::model::Resource>>,
        cap_fail: bool,
    ) -> crate::mcp_clients::manager::test_support::MockSkillClient {
        crate::mcp_clients::manager::test_support::MockSkillClient {
            pages,
            fail: false,
            cap_fail,
            read_text: String::new(),
        }
    }

    #[tokio::test]
    async fn test_get_resources_single_page_passthrough() {
        use crate::mcp_clients::manager::{
            test_support::{bid, inject},
            MCPServerManager,
        };
        use crate::mcp_clients::model::make_resource;

        let manager = MCPServerManager::new();
        inject(
            &manager,
            &bid("srv-1"),
            mock(
                vec![vec![
                    make_resource("window://app/w1", "W1", None, None),
                    make_resource("custom://x/y", "Y", None, None),
                ]],
                false,
            ),
        )
        .await;
        let manager = Arc::new(RwLock::new(Some(manager)));

        let (ack, value) = SmcpComputerClient::handle_get_resources_with_ack(
            get_resources_payload("comp-1", "srv-1", None, 7),
            manager,
            "comp-1".to_string(),
        )
        .await
        .unwrap();

        assert_eq!(ack, Some(7));
        let ret: smcp::GetResourcesRet = serde_json::from_value(value).unwrap();
        // 单页透传：2 个资源，无下一页；不含 resourceTemplates（仅 resources）。
        assert_eq!(ret.resources.len(), 2);
        assert_eq!(ret.next_cursor, None);
        assert_eq!(ret.resources[0].uri.as_deref(), Some("window://app/w1"));
        assert_eq!(ret.req_id.unwrap().0, "req-1");
    }

    #[tokio::test]
    async fn test_get_resources_cursor_passthrough() {
        use crate::mcp_clients::manager::{
            test_support::{bid, inject},
            MCPServerManager,
        };
        use crate::mcp_clients::model::make_resource;

        let manager = MCPServerManager::new();
        inject(
            &manager,
            &bid("srv-1"),
            mock(
                vec![
                    vec![make_resource("res://0", "r0", None, None)],
                    vec![make_resource("res://1", "r1", None, None)],
                ],
                false,
            ),
        )
        .await;
        let manager = Arc::new(RwLock::new(Some(manager)));

        // 首页：cursor=None → next_cursor 透传为 "1"。
        let (_, v0) = SmcpComputerClient::handle_get_resources_with_ack(
            get_resources_payload("comp-1", "srv-1", None, 1),
            manager.clone(),
            "comp-1".to_string(),
        )
        .await
        .unwrap();
        let p0: smcp::GetResourcesRet = serde_json::from_value(v0).unwrap();
        assert_eq!(p0.next_cursor.as_deref(), Some("1"));
        assert_eq!(p0.resources[0].uri.as_deref(), Some("res://0"));

        // 次页：cursor="1" 入参透传 → 末页 next_cursor=None。
        let (_, v1) = SmcpComputerClient::handle_get_resources_with_ack(
            get_resources_payload("comp-1", "srv-1", Some("1"), 2),
            manager,
            "comp-1".to_string(),
        )
        .await
        .unwrap();
        let p1: smcp::GetResourcesRet = serde_json::from_value(v1).unwrap();
        assert_eq!(p1.next_cursor, None);
        assert_eq!(p1.resources[0].uri.as_deref(), Some("res://1"));
    }

    #[tokio::test]
    async fn test_get_resources_unknown_server_4014() {
        use crate::mcp_clients::manager::MCPServerManager;

        let manager = Arc::new(RwLock::new(Some(MCPServerManager::new())));
        let (ack, value) = SmcpComputerClient::handle_get_resources_with_ack(
            get_resources_payload("comp-1", "missing", None, 3),
            manager,
            "comp-1".to_string(),
        )
        .await
        .unwrap();

        // flat ErrorPayload 4014（经 ACK 第一参回传），顶层平铺 mcp_server。
        assert_eq!(ack, Some(3));
        let err: smcp::ErrorPayload = serde_json::from_value(value).unwrap();
        assert_eq!(err.code, 4014);
        assert_eq!(err.mcp_server.as_deref(), Some("missing"));
        // 无嵌套 envelope：顶层即 code。
        assert!(err.capability.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_ack_handler_does_not_hold_manager_guard_across_hanging_call() {
        // #178 回归守卫：ack handler 在挂起的 MCP 调用（`list_resources_page` 永不返回）期间
        // 不得持有 `mcp_manager` 读锁——否则排队中的写锁（shutdown 的 take、挂载写）被永久阻塞。
        // 修复前：handler 持读锁跨 `mgr.list_resources(...).await` → 写锁排队 → 1s 超时 panic。
        use crate::mcp_clients::manager::test_support::inject_hanging;
        use crate::mcp_clients::manager::MCPServerManager;
        use std::time::Duration;

        let manager = MCPServerManager::new();
        inject_hanging(&manager, "srv-1", "t").await;
        let lock = Arc::new(RwLock::new(Some(manager)));

        // 挂起的 get_resources 调用（handler 内持读锁等待 list_resources_page 永不返回）。
        let h_lock = lock.clone();
        // detached：挂起 handler 设计为永不返回，绝不 await（否则测试自身卡死）。
        let _handler = tokio::spawn(async move {
            SmcpComputerClient::handle_get_resources_with_ack(
                get_resources_payload("comp-1", "srv-1", None, 7),
                h_lock,
                "comp-1".to_string(),
            )
            .await
        });

        // 确定性窗口：等 handler 进入挂起（已持有读锁）后，写锁（模拟 shutdown/mount 的
        // manager 替换）必须仍能在超时内完成。修复前：handler 持读锁跨挂起调用 → 写锁排队
        // 永久阻塞 → 1s 超时 panic（红灯）。
        tokio::time::sleep(Duration::from_millis(50)).await;

        let w_lock = lock.clone();
        tokio::time::timeout(Duration::from_secs(1), async move {
            *w_lock.write().await = None;
        })
        .await
        .expect("ack handler 持读锁跨挂起调用，写锁被永久阻塞（#178）");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_get_desktop_ack_handler_does_not_hold_manager_guard_across_hanging_call() {
        // #178 回归守卫：`handle_get_desktop_with_ack` 的 manager 读锁不得跨 `get_windows_details`
        // await 持有。专防 **if-let scrutinee 临时守卫陷阱**——`if let Some(mgr) =
        // manager.read().await.as_ref().cloned()` 形状下守卫存活至语句（含 body）结束，修复前的
        // 内联形状会持读锁跨挂起的 `list_windows` → 排队写锁 1s 超时 panic（红灯）。
        use crate::mcp_clients::manager::test_support::inject_hanging;
        use crate::mcp_clients::manager::MCPServerManager;
        use std::time::Duration;

        let manager = MCPServerManager::new();
        inject_hanging(&manager, "srv-1", "t").await;
        let lock = Arc::new(RwLock::new(Some(manager)));

        let mut obj = json!({
            "agent": "agent-1",
            "req_id": "req-1",
            "computer": "comp-1",
        });
        obj.as_object_mut()
            .unwrap()
            .insert("window".into(), json!("window://app/w1"));
        let payload = Payload::Text(vec![obj], Some(9));

        // detached：挂起 handler 设计为永不返回，绝不 await。
        let h_lock = lock.clone();
        let _handler = tokio::spawn(async move {
            SmcpComputerClient::handle_get_desktop_with_ack(
                payload,
                h_lock,
                "comp-1".to_string(),
                None,
            )
            .await
        });

        // 确定性窗口：等 handler 进入挂起（已持有读锁——修复前形状）后，写锁必须仍能完成。
        tokio::time::sleep(Duration::from_millis(50)).await;

        let w_lock = lock.clone();
        tokio::time::timeout(Duration::from_secs(1), async move {
            *w_lock.write().await = None;
        })
        .await
        .expect("get_desktop ack handler 持读锁跨挂起调用，写锁被永久阻塞（#178）");
    }

    #[tokio::test]
    async fn test_get_resources_capability_not_supported_4015() {
        use crate::mcp_clients::manager::{
            test_support::{bid, inject},
            MCPServerManager,
        };

        let manager = MCPServerManager::new();
        inject(&manager, &bid("srv-1"), mock(vec![], true)).await;
        let manager = Arc::new(RwLock::new(Some(manager)));

        let (_, value) = SmcpComputerClient::handle_get_resources_with_ack(
            get_resources_payload("comp-1", "srv-1", None, 9),
            manager,
            "comp-1".to_string(),
        )
        .await
        .unwrap();

        // flat ErrorPayload 4015，顶层平铺 mcp_server + capability。
        let err: smcp::ErrorPayload = serde_json::from_value(value).unwrap();
        assert_eq!(err.code, 4015);
        assert_eq!(err.mcp_server.as_deref(), Some("srv-1"));
        assert_eq!(err.capability.as_deref(), Some("resources"));
    }

    #[tokio::test]
    async fn test_get_resources_computer_name_mismatch() {
        use crate::mcp_clients::manager::MCPServerManager;

        let manager = Arc::new(RwLock::new(Some(MCPServerManager::new())));
        // computer_name ≠ req.computer → ValidationError（不进入转发）。
        let result = SmcpComputerClient::handle_get_resources_with_ack(
            get_resources_payload("other-comp", "srv-1", None, 5),
            manager,
            "comp-1".to_string(),
        )
        .await;
        assert!(matches!(result, Err(ComputerError::ValidationError(_))));
    }

    // ── INT-03 #72：blob mint / get_blob / get_skill[s] / cancel handler ─────────────────

    /// 启一个带 blob 子系统的 Computer，返回 socketio handler 用的 ops（直接 `Arc<Computer>`——测试无
    /// socketio client，故无需 detached 克隆、无环）。`TempDir` 一并返回避免被 drop。
    async fn boot_blob_ops(
        inline_budget: u64,
        too_large_cap: u64,
    ) -> (Arc<dyn ComputerHandlerOps>, tempfile::TempDir) {
        use crate::blob::BlobThresholds;
        use crate::computer::{Computer, SilentSession};
        let tmp = tempfile::TempDir::new().unwrap();
        let computer = Computer::new("c", SilentSession::new("s"), None, None, false, false)
            .with_skill_home(tmp.path().join("home"))
            .with_blob_cache_root(tmp.path().join("blob"))
            .with_blob_thresholds(BlobThresholds {
                inline_budget,
                too_large_cap,
                chunk_max_bytes: 256 * 1024,
            });
        computer.boot_up().await.unwrap();
        let ops: Arc<dyn ComputerHandlerOps> = Arc::new(computer);
        (ops, tmp)
    }

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    #[tokio::test]
    async fn test_mint_oversize_image_item_minted_text_untouched() {
        let (ops, _tmp) = boot_blob_ops(8, 1024).await;
        let bytes = vec![0xABu8; 20]; // 20 > inline_budget(8) 且 < too_large_cap(1024) → 铸造
        let mut result = json!({
            "content": [
                { "type": "text", "text": "hello" },
                { "type": "image", "mimeType": "image/png", "data": b64(&bytes) }
            ]
        });
        mint_oversize_binary_content(ops.as_ref(), &mut result).await;
        // text item 路径不变。
        assert_eq!(result["content"][0]["text"], json!("hello"));
        // image item：内联 data 清空 + _meta 旁路写入（handle / total_size / sha256）。
        let item = &result["content"][1];
        assert_eq!(item["data"], json!(""));
        assert!(!item["_meta"]["a2c_blob_handle"]
            .as_str()
            .unwrap()
            .is_empty());
        assert_eq!(item["_meta"]["a2c_total_size"], json!(20));
        assert_eq!(
            item["_meta"]["a2c_sha256"].as_str().unwrap(),
            smcp::utils::sha256_hex(&bytes)
        );
    }

    #[tokio::test]
    async fn test_mint_small_inline_and_too_large_rejected() {
        let (ops, _tmp) = boot_blob_ops(8, 16).await; // budget=8, cap=16
        let small = b64(&[1u8; 4]); // 4 ≤ 8 → 保留内联
        let huge = b64(&[2u8; 20]); // 20 > 16(cap) → 拒绝铸造，保留内联
        let mut result = json!({
            "content": [
                { "type": "image", "mimeType": "image/png", "data": small.clone() },
                { "type": "image", "mimeType": "image/png", "data": huge.clone() }
            ]
        });
        mint_oversize_binary_content(ops.as_ref(), &mut result).await;
        // 小尺寸：data 原样、无 _meta。
        assert_eq!(result["content"][0]["data"], json!(small));
        assert!(result["content"][0].get("_meta").is_none());
        // 超 too_large_cap：data 原样（保留内联避免丢字节）、无 _meta。
        assert_eq!(result["content"][1]["data"], json!(huge));
        assert!(result["content"][1].get("_meta").is_none());
    }

    #[tokio::test]
    async fn test_mint_embedded_resource_blob_and_non_dict_meta() {
        let (ops, _tmp) = boot_blob_ops(8, 1024).await;
        let bytes = vec![0xCDu8; 20];
        let mut result = json!({
            "content": [{
                "type": "resource",
                "resource": { "mimeType": "application/pdf", "blob": b64(&bytes) },
                "_meta": null  // 非 dict _meta：铸造时须规范化，避免孤儿 cid。
            }]
        });
        mint_oversize_binary_content(ops.as_ref(), &mut result).await;
        let item = &result["content"][0];
        // EmbeddedResource：写回 resource.blob 清空，顶层不误写 data。
        assert_eq!(item["resource"]["blob"], json!(""));
        assert!(item.get("data").is_none());
        // null _meta 被规范化为 object 并写入旁路（null/缺省/object 均照常铸造）。
        assert!(item["_meta"].is_object());
        assert!(item["_meta"]["a2c_blob_handle"].is_string());
        assert_eq!(item["_meta"]["a2c_total_size"], json!(20));
    }

    #[tokio::test]
    async fn test_mint_skips_non_dict_meta() {
        // fix-review #2：`_meta` 非 null 非 object（字符串 / 数组）→ 跳过铸造、保留内联（对齐 Python）。
        let (ops, _tmp) = boot_blob_ops(8, 1024).await;
        let payload = b64(&[0xEEu8; 20]); // 20 > budget(8)，本应铸造——但畸形 _meta 阻止
        let mut result = json!({
            "content": [
                { "type": "image", "mimeType": "image/png", "data": payload.clone(), "_meta": "i-am-a-string" },
                { "type": "image", "mimeType": "image/png", "data": payload.clone(), "_meta": [1, 2, 3] }
            ]
        });
        mint_oversize_binary_content(ops.as_ref(), &mut result).await;
        // 字符串 _meta：data 原样、_meta 不被改写为 object、无旁路句柄。
        assert_eq!(result["content"][0]["data"], json!(payload));
        assert_eq!(result["content"][0]["_meta"], json!("i-am-a-string"));
        // 数组 _meta：同样跳过。
        assert_eq!(result["content"][1]["data"], json!(payload));
        assert_eq!(result["content"][1]["_meta"], json!([1, 2, 3]));
    }

    #[tokio::test]
    async fn test_get_blob_roundtrip_eof_probe_and_range() {
        let (ops, _tmp) = boot_blob_ops(8, 1 << 20).await;
        let handle = ops
            .mint_toolspool_handle(b"0123456789", "text/plain")
            .await
            .unwrap(); // 10 bytes
        let mk = |off: u64, max: u64, ack: i32| {
            Payload::Text(
                vec![json!({
                    "agent": "a", "req_id": "r1", "computer": "c",
                    "blob_handle": handle.clone(), "chunk_offset": off, "max_chunk_bytes": max
                })],
                Some(ack),
            )
        };

        // 整块拉取：total_size/sha256/eof/chunk_offset + base64 还原。
        let (ack, resp) = SmcpComputerClient::handle_get_blob_with_ack(
            mk(0, 1000, 3),
            Some(ops.clone()),
            "c".to_string(),
        )
        .await
        .unwrap();
        assert_eq!(ack, Some(3));
        assert_eq!(resp["total_size"], json!(10));
        assert_eq!(resp["eof"], json!(true));
        assert_eq!(resp["chunk_offset"], json!(0));
        assert_eq!(resp["mime_type"], json!("text/plain"));
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(resp["blob"].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, b"0123456789");

        // EOF probe：offset==total_size → 空块 + eof=true（**非** range error）。
        let (_, probe) = SmcpComputerClient::handle_get_blob_with_ack(
            mk(10, 1000, 4),
            Some(ops.clone()),
            "c".to_string(),
        )
        .await
        .unwrap();
        assert_eq!(probe["eof"], json!(true));
        assert_eq!(probe["blob"], json!(""));
        assert!(probe.get("code").is_none());

        // 越界：offset > total_size → 4018 range。
        let (_, rng) = SmcpComputerClient::handle_get_blob_with_ack(
            mk(11, 1000, 5),
            Some(ops.clone()),
            "c".to_string(),
        )
        .await
        .unwrap();
        assert_eq!(rng["code"], json!(4018));
        assert_eq!(rng["details"]["reason"], json!("range"));
    }

    #[tokio::test]
    async fn test_get_blob_invalid_handle_4018() {
        let (ops, _tmp) = boot_blob_ops(8, 1 << 20).await;
        let payload = Payload::Text(
            vec![json!({
                "agent": "a", "req_id": "r1", "computer": "c", "blob_handle": "garbage-not-a-handle"
            })],
            Some(9),
        );
        let (_, resp) =
            SmcpComputerClient::handle_get_blob_with_ack(payload, Some(ops), "c".to_string())
                .await
                .unwrap();
        assert_eq!(resp["code"], json!(4018));
        assert_eq!(resp["details"]["reason"], json!("invalid_handle"));
    }

    #[tokio::test]
    async fn test_get_skills_empty_and_get_skill_not_found_4014() {
        let (ops, _tmp) = boot_blob_ops(1024, 1 << 20).await;

        // get_skills：空 registry → 空列表（透传 ComputerHandlerOps::get_skills）。
        let p = Payload::Text(
            vec![json!({ "agent": "a", "req_id": "r1", "computer": "c" })],
            Some(1),
        );
        let (ack, resp) =
            SmcpComputerClient::handle_get_skills_with_ack(p, Some(ops.clone()), "c".to_string())
                .await
                .unwrap();
        assert_eq!(ack, Some(1));
        assert_eq!(resp["skills"], json!([]));

        // get_skill：合法 kebab name 但未注册 → 4014（格式合法但不存在）。
        let p2 = Payload::Text(
            vec![json!({ "agent": "a", "req_id": "r1", "computer": "c", "name": "my-skill" })],
            Some(2),
        );
        let (_, resp2) =
            SmcpComputerClient::handle_get_skill_with_ack(p2, Some(ops), "c".to_string())
                .await
                .unwrap();
        assert_eq!(resp2["code"], json!(4014));
    }

    #[tokio::test]
    async fn test_tool_call_cancel_unknown_reqid_is_noop() {
        let (ops, _tmp) = boot_blob_ops(1024, 1 << 20).await;
        // notify:tool_call_cancel 对未知 req_id 幂等 no-op（不 panic、无 ACK）。
        let p = Payload::Text(vec![json!({ "agent": "a", "req_id": "nonexistent" })], None);
        SmcpComputerClient::handle_tool_call_cancel(p, Some(ops)).await;
    }

    #[tokio::test]
    async fn test_handlers_require_ops_when_absent() {
        // ops 缺省（旧入口）：blob/skill handler 回 InvalidState（不 panic）。
        let p = Payload::Text(
            vec![json!({ "agent": "a", "req_id": "r1", "computer": "c", "blob_handle": "x" })],
            Some(1),
        );
        let r = SmcpComputerClient::handle_get_blob_with_ack(p, None, "c".to_string()).await;
        assert!(matches!(r, Err(ComputerError::InvalidState(_))));
    }

    // ── #92：tool_call ack 顶层结果级 `_meta`→`meta` 重映射（协议 §234 producer MUST=meta）──────

    #[test]
    fn test_promote_result_meta_top_level_cancel_timeout_to_meta() {
        // rmcp CallToolResult 出线形态：顶层结果级标记落 `_meta`（rename）。重映射后须为协议规范的 `meta`。
        let mut v = json!({
            "content": [{ "type": "text", "text": "x" }],
            "isError": true,
            "_meta": { "a2c_cancelled": true, "a2c_cancel_reason": "agent_requested" }
        });
        promote_result_meta_to_meta(&mut v);
        // 顶层标记出线为 `meta.*`，且顶层不再残留 `_meta`。
        assert_eq!(v["meta"]["a2c_cancelled"], json!(true));
        assert_eq!(v["meta"]["a2c_cancel_reason"], json!("agent_requested"));
        assert!(
            v.get("_meta").is_none(),
            "顶层 _meta 应被提升为 meta 后移除"
        );

        // 超时态同理。
        let mut t = json!({ "isError": true, "_meta": { "a2c_timeout": true } });
        promote_result_meta_to_meta(&mut t);
        assert_eq!(t["meta"]["a2c_timeout"], json!(true));
        assert!(t.get("_meta").is_none());
    }

    #[test]
    fn test_promote_result_meta_preserves_content_item_meta() {
        // 子级 content[*]._meta（blob 句柄）MUST 保持 `_meta` 不变（data-structures.md §683）。
        let mut v = json!({
            "content": [
                { "type": "text", "text": "x" },
                { "type": "image", "data": "", "_meta": { "a2c_blob_handle": "ts:img", "a2c_total_size": 1024 } }
            ],
            "_meta": { "a2c_cancelled": true }
        });
        promote_result_meta_to_meta(&mut v);
        // 顶层提升为 meta。
        assert_eq!(v["meta"]["a2c_cancelled"], json!(true));
        assert!(v.get("_meta").is_none());
        // content item 的 `_meta` 原封不动（绝不被提升/移除）。
        assert_eq!(v["content"][1]["_meta"]["a2c_blob_handle"], json!("ts:img"));
        assert_eq!(v["content"][1]["_meta"]["a2c_total_size"], json!(1024));
        assert!(
            v["content"][1].get("meta").is_none(),
            "content item 不应长出 meta"
        );
    }

    #[test]
    fn test_promote_result_meta_covers_auth_error_keys() {
        // 同根因：AUTH-01 的 error_code / mcp_server 亦经 rmcp `_meta` 出线，顺带覆盖。
        let mut v = json!({
            "content": [{ "type": "text", "text": "denied" }],
            "isError": true,
            "_meta": { "error_code": 4006, "mcp_server": "srv-a" }
        });
        promote_result_meta_to_meta(&mut v);
        assert_eq!(v["meta"]["error_code"], json!(4006));
        assert_eq!(v["meta"]["mcp_server"], json!("srv-a"));
        assert!(v.get("_meta").is_none());
    }

    #[test]
    fn test_promote_result_meta_noop_and_merge_and_malformed() {
        // 1) 无顶层 `_meta` → no-op（含完全无 meta 的成功结果）。
        let mut ok = json!({ "content": [{ "type": "text", "text": "ok" }] });
        let before = ok.clone();
        promote_result_meta_to_meta(&mut ok);
        assert_eq!(ok, before, "无 _meta 应原样返回");

        // 2) 顶层已有 `meta` 时按键合并、不覆盖既有键，并清掉 `_meta`。
        let mut both = json!({
            "meta": { "a2c_cancelled": true, "keep": "orig" },
            "_meta": { "a2c_timeout": true, "keep": "shadow" }
        });
        promote_result_meta_to_meta(&mut both);
        assert_eq!(both["meta"]["a2c_cancelled"], json!(true));
        assert_eq!(both["meta"]["a2c_timeout"], json!(true), "新键并入");
        assert_eq!(
            both["meta"]["keep"],
            json!("orig"),
            "既有 meta 键不被 _meta 覆盖"
        );
        assert!(both.get("_meta").is_none());

        // 3) 畸形非-object 顶层 `_meta` → 原样保留（不提升、不报错）。
        let mut bad = json!({ "_meta": "i-am-a-string" });
        promote_result_meta_to_meta(&mut bad);
        assert_eq!(bad["_meta"], json!("i-am-a-string"));
        assert!(bad.get("meta").is_none());
    }
}
