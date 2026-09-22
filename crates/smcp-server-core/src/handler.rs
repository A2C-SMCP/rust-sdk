//! SMCP 协议处理器 / SMCP protocol handler

use crate::auth::{AuthError, AuthenticationProvider};
use crate::session::{
    ClientRole, JoinDecision, LeaveCommit, SessionData, SessionError, SessionManager,
};
use futures_util::StreamExt;
use serde_json::Value;
use smcp::*;
use socketioxide::{
    extract::{AckSender, Data, SocketRef, TryData},
    SocketIo,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use thiserror::Error;
use tokio::sync::Notify;
use tracing::{error, info, warn};

/// 处理器错误类型
#[derive(Error, Debug)]
pub enum HandlerError {
    #[error("Authentication error: {0}")]
    Auth(#[from] AuthError),
    #[error("Session error: {0}")]
    Session(#[from] SessionError),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Timeout error: {0}")]
    Timeout(String),
    #[error("Invalid request: {0}")]
    InvalidRequest(String),
    /// 房间/角色隔离拒绝 / Room/role isolation rejection.
    ///
    /// 对标 Python `SMCPNamespaceError`（见 `server/namespace.py`）。属安全不变量——以**运行期**
    /// `Result` 表达（**非** `debug_assert!`），故 release build（无 `debug_assert!`）下隔离同样硬化。
    /// 投递语义：`client:*` 路由命中此变体时**不投递协议 ack**（镜像 Python `raise`，发起方侧自行超时，
    /// 不泄露 Computer 存在性、不造非协议错误码），见 `SmcpHandler::relay_client_call`。
    #[error("Isolation rejected: {0}")]
    Isolation(String),
}

impl HandlerError {
    /// 获取错误码 / Get error code
    pub fn error_code(&self) -> i32 {
        match self {
            HandlerError::Auth(_) => smcp::error_codes::UNAUTHORIZED,
            HandlerError::Session(e) => e.error_code(),
            HandlerError::Json(_) => smcp::error_codes::BAD_REQUEST,
            HandlerError::Timeout(_) => smcp::error_codes::TIMEOUT,
            HandlerError::InvalidRequest(_) => smcp::error_codes::BAD_REQUEST,
            // 隔离拒绝仅作内部诊断码，永不进 client:* ack（路由层直接丢弃，不投递）。
            HandlerError::Isolation(_) => smcp::error_codes::FORBIDDEN,
        }
    }

    /// 转换为 flat 错误负载 / Convert to a flat error payload
    ///
    /// 协议 0.2.2：错误负载统一为 flat [`smcp::ErrorPayload`]（顶层 `code`/`message`），禁止嵌套
    /// `{"error": {...}}` envelope；码空间是传输/管理层 [`smcp::error_codes`]（400/401/500/4101…），
    /// 不受协议谓词约束。
    ///
    /// **调用面**（#226 复审 🟡8 订正旧 doc）：三个房间事件（`server:join_office` /
    /// `server:leave_office` / `server:list_room`）已改走 [`RoomAckResult`]，其拒绝一律由
    /// [`smcp::build_room_rejection_error`] 产出 canonical 文案与 `details` 白名单；`client:*` 路由的
    /// 错误经 [`smcp::build_computer_not_found_error`]（404）或**不投递 ack**（隔离拒绝）表达——SRV-01
    /// (#47) 已收敛。故本方法在生产路径上**不再**是任何 ack 的构造入口，其唯一读者是
    /// `impl serde::Serialize for HandlerError`（框架收编处理器错误、需要落日志/诊断时用到）。
    /// 据此降为**私有**：外部无法把它当成「房间拒绝构造器」而绕过 canonical 文案不变量。
    fn to_error_payload(&self) -> smcp::ErrorPayload {
        let message = match self {
            // A session id is an internal Socket.IO identifier and MUST NOT cross an ack
            // boundary. Keep it in server logs via the source error, but expose only the
            // protocol-level category to the caller.
            HandlerError::Session(SessionError::NotFound(_)) => "Session not found".to_string(),
            _ => self.to_string(),
        };
        smcp::ErrorPayload::new(i64::from(self.error_code()), message)
    }
}

impl serde::Serialize for HandlerError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        // 使用 flat 错误负载格式 / Use the flat error payload shape
        self.to_error_payload().serialize(serializer)
    }
}

/// Ack handlers return boxed payloads so the error path stays small on the async stack while
/// preserving the same flat `ErrorPayload` wire shape.
type RoomAckResult<T> = Result<T, Box<smcp::ErrorPayload>>;

/// **单实参**载荷提取（`(T,)` 1-tuple）。
///
/// socketioxide 的解析器按 `T` 是否 tuple-like 分流（`socketioxide-parser-common::value::from_value`）：
/// 非 tuple-like 走 `FirstElement` seed——`[payload, "extra"]` 会被**静默读成** `payload`，多余实参凭空
/// 消失；tuple-like 则要求数组里**恰好**一个元素，多一个即反序列化失败。
///
/// 协议每个事件只定义**单个**载荷对象（`room-model.md` / `events.md`），Python 参考实现在
/// `test_valid_payload_plus_extra_argument_is_rejected` 里钉死「有效载荷 + 多余实参 ⇒ `400`」。
/// 本 SDK 用 1-tuple 把同一严格性前移到提取器层，故 `TryData<SingleArg<T>>` / `Data<SingleArg<T>>`
/// 的 `Err` 分支**同时**覆盖「载荷畸形」与「实参个数不为 1」两种情形（#226 复审 🟡9）。
///
/// Single-argument payload extractor: the 1-tuple makes the parser require *exactly* one argument,
/// matching the Python reference implementation's rejection of a valid payload plus extra arguments.
type SingleArg<T> = (T,);

/// 空 ack 的线载荷 / zero-argument Socket.IO ack payload.
///
/// 协议把 `server:join_office` / `server:leave_office` 的成功响应定义为**空 ack**；Python 参考实现
/// （python-socketio `_handle_event_internal`：handler 返回 `None` ⇒ `data = []`）在线上产出
/// **零参** ACK `[]`。
///
/// socketioxide 的 `AckSender::send` 只接受**单个**实参，`send(&())` 会被其 `to_value` 包成 1-tuple
/// ⇒ 线上成 `[null]`——凭空多出一个 `null` 实参。跨 SDK 逐字节不一致即源于此（#226 P1-6）：
/// 按「参数个数」判成功的对端（JS `emitWithAck` 回调、字节级 conformance fixture）会把成功读成畸形。
///
/// 本类型的 `Serialize` 直接 `serialize_tuple(0)`：socketioxide 的 `is_ser_tuple` 将其判为 tuple-like
/// ⇒ 其解析器**不再**包一层 ⇒ 序列化结果就是 `[]`，与 Python 逐字节一致。
///
/// 不用 `Vec::new()`：serde 把 `Vec` 归为 `seq`（非 tuple-like），同样会被包成 `[[]]`——一个「空数组」
/// 实参，仍是多一个参数。Emit a zero-argument ack (`[]` on the wire).
struct EmptyAck;

impl serde::Serialize for EmptyAck {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeTuple;
        serializer.serialize_tuple(0)?.end()
    }
}

/// 在途断连信号注册表 / In-flight disconnect signal registry.
///
/// 对标 Python `SMCPNamespace._inflight_disconnect_signals`（#100 Phase 1）。每个在途
/// `SmcpHandler::relay_client_call` 针对其**目标 Computer SID** 登记一个独立 [`Notify`]；目标
/// `on_disconnect` 时唤醒所有以该 SID 为目标的信号，使在途 relay 立即放弃等待、回 flat 404，而非
/// 空等满 timeout（socketioxide `emit_with_ack` 的等待原语不监听连接掉线）。
///
/// 语义：目标 Computer 中途消失 == 不存在，与「目标未命中」（#47 / `build_computer_not_found_error`）
/// 同一 flat `ErrorPayload(404)`，**不**泄露存在性、**不**新增错误码（协议 0.2.2：Server MAY 静默丢弃）。
#[derive(Default)]
pub struct InflightDisconnectRegistry {
    /// 目标 Computer SID -> 该 SID 当前在途的断连信号集合。
    signals: Mutex<HashMap<String, Vec<Arc<Notify>>>>,
}

impl std::fmt::Debug for InflightDisconnectRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 不打印 Notify（无 Debug）；仅暴露当前登记的目标数，便于诊断。
        let targets = self
            .signals
            .lock()
            .map(|m| m.len())
            .unwrap_or_else(|e| e.into_inner().len());
        f.debug_struct("InflightDisconnectRegistry")
            .field("targets", &targets)
            .finish()
    }
}

impl InflightDisconnectRegistry {
    /// 登记并返回一个针对 `computer_sid` 的在途断连信号。
    /// Register and return a fresh disconnect signal targeting `computer_sid`.
    fn register(&self, computer_sid: &str) -> Arc<Notify> {
        let notify = Arc::new(Notify::new());
        self.lock()
            .entry(computer_sid.to_string())
            .or_default()
            .push(notify.clone());
        notify
    }

    /// 注销指定信号（幂等）；桶空则回收。Discard a specific signal (idempotent); reap empty buckets.
    fn discard(&self, computer_sid: &str, notify: &Arc<Notify>) {
        let mut guard = self.lock();
        if let Some(bucket) = guard.get_mut(computer_sid) {
            bucket.retain(|n| !Arc::ptr_eq(n, notify));
            if bucket.is_empty() {
                guard.remove(computer_sid);
            }
        }
    }

    /// 唤醒以 `computer_sid` 为目标的全部在途断连信号（幂等）。
    /// Wake all in-flight signals targeting `computer_sid` (idempotent).
    ///
    /// 用 [`Notify::notify_one`]（每个信号恰一个等待者）：即便在 relay 进入 `notified().await` 之前
    /// fire，permit 也会被暂存，等待者随后立即就绪——闭合「解析 SID 与登记信号之间目标已断连」的竞速窗。
    fn fire(&self, computer_sid: &str) {
        // 先拷出桶再 notify，避免在持锁期间触发任何回调路径。
        let bucket = self.lock().get(computer_sid).cloned();
        if let Some(bucket) = bucket {
            for notify in bucket {
                notify.notify_one();
            }
        }
    }

    /// 取锁并自动从 poison 中恢复——隔离守卫 MUST NOT crash（临界区无 panic 路径，恢复仅为兜底）。
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Vec<Arc<Notify>>>> {
        self.signals.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// RAII 守卫：relay 从任意路径退出（含 `?`/早返回/panic 展开）时自动注销其在途断连信号。
/// RAII guard: deregister a relay's in-flight signal on any exit path.
struct InflightSignalGuard<'a> {
    registry: &'a InflightDisconnectRegistry,
    computer_sid: &'a str,
    notify: Arc<Notify>,
}

impl Drop for InflightSignalGuard<'_> {
    fn drop(&mut self) {
        self.registry.discard(self.computer_sid, &self.notify);
    }
}

/// 服务器状态
#[derive(Clone, Debug)]
pub struct ServerState {
    /// 会话管理器
    pub session_manager: Arc<SessionManager>,
    /// 认证提供者
    pub auth_provider: Arc<dyn AuthenticationProvider>,
    /// SocketIo 实例引用，用于跨 socket 通信
    pub io: Arc<SocketIo>,
    /// 在途断连信号注册表（#56 SRV-04）：目标 Computer 飞行中断连时唤醒在途 relay。
    /// In-flight disconnect signals: wake relays whose target Computer dies mid-flight.
    pub inflight_disconnect: Arc<InflightDisconnectRegistry>,
}

/// SMCP 事件处理器
pub struct SmcpHandler;

impl SmcpHandler {
    /// Return the Socket.IO room used for an SMCP office.
    ///
    /// Office rooms live in their own `office:` namespace so an attacker-controlled
    /// `office_id` can never collide with a connection-level room name.
    ///
    /// ⚠️ 不要把这件事说成「Socket.IO 为每个连接保留 SID 私房」：**socketioxide 不会**把连接自动加入
    /// 以其 SID 命名的房（唯一房间写入口是 `Socket::join`，全仓无 `socket.join(sid)`），而
    /// python-socketio **会**。故本前缀的意义是**跨实现**的确定性：无论宿主栈是否保留 SID 私房，
    /// 房间名空间都不与它相交。#226 P0-1 记录过一处基于「本栈有 SID 私房」的错误注释与随之而来的
    /// 死守卫（`room != sid`）。
    ///
    /// Do not claim this stacks auto-joins a per-connection SID room: socketioxide does not (only
    /// python-socketio does). The prefix is what makes the guarantee hold across implementations.
    fn office_room(office_id: &str) -> String {
        format!("office:{office_id}")
    }

    /// Ack a malformed payload without reflecting parser details or input data.
    ///
    /// 载荷 schema 校验失败 **MUST** 回 `400`（协议 error-handling.md：具备 ack 通道的事件不得静默
    /// 不 ack——「挂到客户端自身超时」与「立即收到结构化错误」是两种客户端可感行为）。文案与 Python
    /// `build_bad_request_error` 对齐。
    fn ack_bad_request(ack: AckSender) {
        let _ = ack.send(&smcp::build_bad_request_error());
    }

    /// 注册所有事件处理器
    pub fn register_handlers(io: &SocketIo, state: ServerState) {
        // 注册命名空间和连接处理器
        // #86：用 `TryData<Value>` 提取器拿 Socket.IO CONNECT `auth` dict（连接面鉴权唯一来源）。
        io.ns(
            SMCP_NAMESPACE,
            move |socket: SocketRef, TryData(auth): TryData<Value>| {
                let state = state.clone();
                async move {
                    if let Err(e) = Self::on_connect(socket.clone(), auth.ok(), &state).await {
                        // #86：鉴权/握手失败 → **主动断开** socket，真正拒绝连接，而非仅 log+return
                        // 留下「半开、无业务 handler」的 socket（后者会让无效鉴权静默旁路）。
                        // Auth/handshake failure → disconnect the socket (real rejection), instead of
                        // leaving a half-open handler-less socket that silently bypasses auth.
                        warn!(
                            "on_connect rejected ({}); disconnecting socket {}",
                            e, socket.id
                        );
                        let _ = socket.disconnect();
                        return;
                    }

                    // 连接时注册所有事件处理器
                    Self::handle_connection(socket, state)
                }
            },
        );
    }

    /// 处理连接并注册事件处理器
    fn handle_connection(socket: SocketRef, state: ServerState) {
        // 注册各种事件处理器
        socket.on_disconnect({
            let state = state.clone();
            move |socket: SocketRef| {
                let state = state.clone();
                async move { Self::on_disconnect(socket, state).await }
            }
        });

        let state_join = state.clone();
        socket.on(
            smcp::events::SERVER_JOIN_OFFICE,
            move |socket: SocketRef, TryData::<SingleArg<Value>>(data), ack: AckSender| async move {
                match data {
                    Ok((value,)) => match serde_json::from_value::<EnterOfficeReq>(value) {
                        Ok(data) => {
                            match Self::on_server_join_office(socket, data, state_join.clone())
                                .await
                            {
                                Ok(()) => {
                                    let _ = ack.send(&EmptyAck);
                                }
                                Err(error) => {
                                    let _ = ack.send(&error);
                                }
                            }
                        }
                        Err(_) => Self::ack_bad_request(ack),
                    },
                    Err(_) => Self::ack_bad_request(ack),
                }
            },
        );

        let state_leave = state.clone();
        socket.on(
            smcp::events::SERVER_LEAVE_OFFICE,
            move |socket: SocketRef, TryData::<SingleArg<Value>>(data), ack: AckSender| async move {
                match data {
                    Ok((value,)) => match serde_json::from_value::<LeaveOfficeReq>(value) {
                        Ok(data) => {
                            let result =
                                Self::on_server_leave_office(socket, data, state_leave.clone())
                                    .await;
                            match result {
                                Ok(()) => {
                                    let _ = ack.send(&EmptyAck);
                                }
                                Err(error) => {
                                    let _ = ack.send(&error);
                                }
                            }
                        }
                        Err(_) => Self::ack_bad_request(ack),
                    },
                    Err(_) => Self::ack_bad_request(ack),
                }
            },
        );

        let state_tool_call_cancel = state.clone();
        socket.on(
            smcp::events::SERVER_TOOL_CALL_CANCEL,
            move |socket: SocketRef, Data::<SingleArg<AgentCallData>>((data,))| async move {
                Self::on_server_tool_call_cancel(socket, data, state_tool_call_cancel.clone()).await
            },
        );

        let state_update_config = state.clone();
        socket.on(
            smcp::events::SERVER_UPDATE_CONFIG,
            move |socket: SocketRef, Data::<SingleArg<UpdateComputerConfigReq>>((data,))| async move {
                Self::on_server_update_config(socket, data, state_update_config.clone()).await
            },
        );

        let state_update_tool_list = state.clone();
        socket.on(
            smcp::events::SERVER_UPDATE_TOOL_LIST,
            move |socket: SocketRef, Data::<SingleArg<UpdateComputerConfigReq>>((data,))| async move {
                Self::on_server_update_tool_list(socket, data, state_update_tool_list.clone()).await
            },
        );

        let state_tool_call = state.clone();
        socket.on(
            smcp::events::CLIENT_TOOL_CALL,
            move |socket: SocketRef,
                  ack: AckSender,
                  TryData::<SingleArg<ToolCallReq>>(data)| async move {
                match data {
                    Ok((data,)) => {
                        match Self::on_client_tool_call(socket, data, state_tool_call.clone()).await
                        {
                            Ok(payload) => {
                                let _ = ack.send(&payload);
                            }
                            // 隔离拒绝（发起方非 Agent / 会话已断连）：镜像 Python，不投递协议 ack（发起方侧自行超时）
                            Err(e) => warn!("client:tool_call relay rejected, no ack: {e}"),
                        }
                    }
                    Err(_) => Self::ack_bad_request(ack),
                }
            },
        );

        let state_get_tools = state.clone();
        socket.on(
            smcp::events::CLIENT_GET_TOOLS,
            move |socket: SocketRef,
                  ack: AckSender,
                  TryData::<SingleArg<GetToolsReq>>(data)| async move {
                match data {
                    Ok((data,)) => {
                        match Self::on_client_get_tools(socket, data, state_get_tools.clone()).await
                        {
                            Ok(payload) => {
                                let _ = ack.send(&payload);
                            }
                            Err(e) => warn!("client:get_tools relay rejected, no ack: {e}"),
                        }
                    }
                    Err(_) => Self::ack_bad_request(ack),
                }
            },
        );

        let state_get_desktop = state.clone();
        socket.on(
            smcp::events::CLIENT_GET_DESKTOP,
            move |socket: SocketRef,
                  ack: AckSender,
                  TryData::<SingleArg<GetDesktopReq>>(data)| async move {
                match data {
                    Ok((data,)) => {
                        match Self::on_client_get_desktop(socket, data, state_get_desktop.clone())
                            .await
                        {
                            Ok(payload) => {
                                let _ = ack.send(&payload);
                            }
                            Err(e) => warn!("client:get_desktop relay rejected, no ack: {e}"),
                        }
                    }
                    Err(_) => Self::ack_bad_request(ack),
                }
            },
        );

        let state_get_config = state.clone();
        socket.on(
            smcp::events::CLIENT_GET_CONFIG,
            move |socket: SocketRef,
                  ack: AckSender,
                  TryData::<SingleArg<GetComputerConfigReq>>(data)| async move {
                match data {
                    Ok((data,)) => {
                        match Self::on_client_get_config(socket, data, state_get_config.clone())
                            .await
                        {
                            Ok(payload) => {
                                let _ = ack.send(&payload);
                            }
                            Err(e) => warn!("client:get_config relay rejected, no ack: {e}"),
                        }
                    }
                    Err(_) => Self::ack_bad_request(ack),
                }
            },
        );

        let state_update_desktop = state.clone();
        socket.on(
            smcp::events::SERVER_UPDATE_DESKTOP,
            move |socket: SocketRef, Data::<SingleArg<UpdateComputerConfigReq>>((data,))| async move {
                Self::on_server_update_desktop(socket, data, state_update_desktop.clone()).await
            },
        );

        let state_list_room = state.clone();
        socket.on(
            smcp::events::SERVER_LIST_ROOM,
            move |socket: SocketRef,
                  ack: AckSender,
                  TryData::<SingleArg<ListRoomReq>>(data)| async move {
                match data {
                    Ok((data,)) => {
                        let result =
                            Self::on_server_list_room(socket, data, state_list_room.clone()).await;
                        match result {
                            Ok(payload) => {
                                let _ = ack.send(&payload);
                            }
                            Err(payload) => {
                                let _ = ack.send(&payload);
                            }
                        }
                    }
                    Err(_) => Self::ack_bad_request(ack),
                }
            },
        );

        // ── SKILL 通道（SRV-02 #50）/ SKILL channel ──────────────────────────
        let state_get_skills = state.clone();
        socket.on(
            smcp::events::CLIENT_GET_SKILLS,
            move |socket: SocketRef,
                  ack: AckSender,
                  TryData::<SingleArg<GetSkillsReq>>(data)| async move {
                match data {
                    Ok((data,)) => {
                        match Self::on_client_get_skills(socket, data, state_get_skills.clone())
                            .await
                        {
                            Ok(payload) => {
                                let _ = ack.send(&payload);
                            }
                            Err(e) => warn!("client:get_skills relay rejected, no ack: {e}"),
                        }
                    }
                    Err(_) => Self::ack_bad_request(ack),
                }
            },
        );

        let state_get_skill = state.clone();
        socket.on(
            smcp::events::CLIENT_GET_SKILL,
            move |socket: SocketRef,
                  ack: AckSender,
                  TryData::<SingleArg<GetSkillReq>>(data)| async move {
                match data {
                    Ok((data,)) => {
                        match Self::on_client_get_skill(socket, data, state_get_skill.clone()).await
                        {
                            Ok(payload) => {
                                let _ = ack.send(&payload);
                            }
                            Err(e) => warn!("client:get_skill relay rejected, no ack: {e}"),
                        }
                    }
                    Err(_) => Self::ack_bad_request(ack),
                }
            },
        );

        let state_get_blob = state.clone();
        socket.on(
            smcp::events::CLIENT_GET_BLOB,
            move |socket: SocketRef,
                  ack: AckSender,
                  TryData::<SingleArg<GetBlobReq>>(data)| async move {
                match data {
                    Ok((data,)) => {
                        match Self::on_client_get_blob(socket, data, state_get_blob.clone()).await {
                            Ok(payload) => {
                                let _ = ack.send(&payload);
                            }
                            Err(e) => warn!("client:get_blob relay rejected, no ack: {e}"),
                        }
                    }
                    Err(_) => Self::ack_bad_request(ack),
                }
            },
        );

        // #195：client:put_blob 上行写入（与 get_blob 同构：按 computer 透传，逐 ack 回 4019 原样）。
        let state_put_blob = state.clone();
        socket.on(
            smcp::events::CLIENT_PUT_BLOB,
            move |socket: SocketRef,
                  ack: AckSender,
                  TryData::<SingleArg<PutBlobReq>>(data)| async move {
                match data {
                    Ok((data,)) => {
                        match Self::on_client_put_blob(socket, data, state_put_blob.clone()).await {
                            Ok(payload) => {
                                let _ = ack.send(&payload);
                            }
                            Err(e) => warn!("client:put_blob relay rejected, no ack: {e}"),
                        }
                    }
                    Err(_) => Self::ack_bad_request(ack),
                }
            },
        );

        let state_get_resources = state.clone();
        socket.on(
            smcp::events::CLIENT_GET_RESOURCES,
            move |socket: SocketRef,
                  ack: AckSender,
                  TryData::<SingleArg<GetResourcesReq>>(data)| async move {
                match data {
                    Ok((data,)) => match Self::on_client_get_resources(
                        socket,
                        data,
                        state_get_resources.clone(),
                    )
                    .await
                    {
                        Ok(payload) => {
                            let _ = ack.send(&payload);
                        }
                        Err(e) => warn!("client:get_resources relay rejected, no ack: {e}"),
                    },
                    Err(_) => Self::ack_bad_request(ack),
                }
            },
        );

        let state_update_skills = state.clone();
        socket.on(
            smcp::events::SERVER_UPDATE_SKILLS,
            move |socket: SocketRef, Data::<SingleArg<UpdateComputerConfigReq>>((data,))| async move {
                Self::on_server_update_skills(socket, data, state_update_skills.clone()).await
            },
        );
    }

    /// 处理连接事件
    ///
    /// `auth`：Socket.IO CONNECT `auth` dict（#86 连接面鉴权唯一来源，由 `.ns()` 的
    /// `TryData<Value>` 提取器解码得到；HTTP header 不再参与鉴权）。
    async fn on_connect(
        socket: SocketRef,
        auth: Option<Value>,
        state: &ServerState,
    ) -> Result<(), HandlerError> {
        info!(
            "SocketIO Client {} connecting to {}...",
            socket.id, SMCP_NAMESPACE
        );

        // #86：用 CONNECT auth dict 鉴权（headers 仅供需要路由头的自定义 provider 参考）。
        let headers = socket.req_parts().headers.clone();

        // 认证
        state
            .auth_provider
            .authenticate(&headers, auth.as_ref())
            .await?;

        info!(
            "SocketIO Client {} connected successfully to {}",
            socket.id, SMCP_NAMESPACE
        );
        Ok(())
    }

    /// 处理断开连接事件
    async fn on_disconnect(socket: SocketRef, state: ServerState) {
        info!(
            "SocketIO Client {} disconnecting from {}...",
            socket.id, SMCP_NAMESPACE
        );

        // 清理会话
        let sid = socket.id.to_string();
        if let Some(session) = state.session_manager.unregister_session(&sid) {
            // 如果在房间内，广播离开消息
            if let Some(office_id) = session.office_id {
                let notification = if session.role == ClientRole::Computer {
                    LeaveOfficeNotification {
                        office_id: office_id.clone(),
                        computer: Some(session.name),
                        agent: None,
                    }
                } else {
                    LeaveOfficeNotification {
                        office_id: office_id.clone(),
                        computer: None,
                        agent: Some(session.name),
                    }
                };

                let _ = socket
                    .within(Self::office_room(&office_id))
                    .emit(smcp::events::NOTIFY_LEAVE_OFFICE, &notification)
                    .await;
            }
        }

        // 在途断连守卫（#56 SRV-04）：先做完上面的会话/name 清理（关闭 TOCTOU 微窗，对齐 Python
        // 「先 super().on_disconnect 再 fire」），再唤醒所有以本 sid 为目标的在途 relay——它们随即
        // 放弃等待、回 flat 404，而非空等满 timeout。Wake every in-flight relay targeting this sid.
        state.inflight_disconnect.fire(&sid);

        info!(
            "SocketIO Client {} disconnected from {}",
            socket.id, SMCP_NAMESPACE
        );
    }

    /// 处理加入办公室事件
    async fn on_server_join_office(
        socket: SocketRef,
        data: EnterOfficeReq,
        state: ServerState,
    ) -> RoomAckResult<()> {
        info!("on_server_join_office called with data: {:?}", data);

        let sid = socket.id.to_string();
        let requested_role = ClientRole::from(data.role.clone());
        let requested_name = data.name.clone();

        // 从握手 URL query 提取协商到的协议版本（仅记录用于诊断/展示，兼容性已由 HTTP 握手中间件
        // 保证）。提取逻辑对齐 Python `_extract_a2c_version`。
        //
        // 时机说明：Python 在 on_connect 即记录 a2c_version；Rust 会话在 join 时才懒建，故在此
        // （会话创建处）记录。二者 list_room 结果等价——`req_parts().uri.query()` 反映的是该连接的
        // 握手请求 URI，连接存续期间稳定（已由集成测试 `test_list_room_reports_a2c_version` 固化）。
        let a2c_version = smcp::utils::handshake::extract_a2c_version(
            socket.req_parts().uri.query().unwrap_or(""),
        );

        // 取或建会话：**原子**且**绝不覆盖**既有记录。历史实现是「先 `get_session` 再
        // `register_session`」两步式，两个并发 join 会双双看到「查无」并互相覆盖——被覆盖者占下的
        // name 预留从此无人释放，该 name 在该房内被**永久**占死（#226 P1-4）。
        let session = state.session_manager.get_or_register_session(
            sid.clone(),
            requested_name.clone(),
            requested_role.clone(),
            a2c_version,
        );

        // 身份声明一致性（协议 events.md §server:join_office）：同一 sid 声明了与既有会话不同的
        // `role` **或** `name` ⇒ 403（**非**房间语义，故走 [§通用错误码] 而不走 4101–4106）。
        // 身份在一次连接内不可变更：改身份须新建连接（faq.md 给出的补救即「重连或换 sid」）。
        if session.role != requested_role || session.name != requested_name {
            warn!(
                sid = %sid,
                session_role = %session.role,
                session_name = %session.name,
                request_role = %requested_role,
                request_name = %requested_name,
                "server:join_office identity claim mismatch with existing session"
            );
            // 403 无 code-specific 字段（协议 §各错误码标准字段总表），故三个来源参数均为 `None`。
            return Err(Box::new(smcp::build_room_rejection_error(
                smcp::RoomRejectionCode::Forbidden,
                smcp::RoomRejectionContext::default(),
            )));
        }

        // 入房事务：**校验 → 预留 → 提交**在单临界区内完成（`reserve_join`）。任何拒绝都在
        // **零成员关系副作用**时返回，故被拒客户端绝不留在目标房——否则它会持续收到该房的
        // `notify:*`，而 `list_room` 说它不在任何房（#226 P0-2 / P1-4 的原形成因）。
        let reservation = match state.session_manager.reserve_join(&sid, &data.office_id) {
            Ok(reservation) => reservation,
            Err(error) => {
                error!("server:join_office rejected for sid={}: {}", sid, error);
                return Err(Box::new(Self::room_rejection(
                    &error,
                    &session.role,
                    &data.office_id,
                )));
            }
        };

        // 闸门全部通过后才动 Socket.IO 成员关系（含 Computer 的退旧房）。
        Self::apply_join_room(
            socket.clone(),
            &session,
            &data.office_id,
            reservation.decision,
        )
        .await;

        // 构建通知数据
        let session_name = session.name.clone();
        let notification_data = if session.role == ClientRole::Computer {
            EnterOfficeNotification {
                office_id: data.office_id.clone(),
                computer: Some(session_name.clone()),
                agent: None,
            }
        } else {
            EnterOfficeNotification {
                office_id: data.office_id.clone(),
                computer: None,
                agent: Some(session_name.clone()),
            }
        };

        let result = socket
            .to(Self::office_room(&data.office_id))
            .emit(smcp::events::NOTIFY_ENTER_OFFICE, &notification_data)
            .await;

        if let Err(e) = result {
            warn!("Failed to broadcast NOTIFY_ENTER_OFFICE: {}", e);
        }

        Ok(())
    }

    /// 处理离开办公室事件
    async fn on_server_leave_office(
        socket: SocketRef,
        data: LeaveOfficeReq,
        state: ServerState,
    ) -> RoomAckResult<()> {
        let sid = socket.id.to_string();

        // 会话懒建于 join：无会话的连接与「无房会话」同义——退房在无房时**幂等成功**（回空 ack），
        // 且**不得**采信载荷里的 office_id 作为广播目标（协议 §房间广播类事件的目标来源）。
        let session = state.session_manager.get_session(&sid);
        let broadcast_office = session.as_ref().and_then(|s| s.office_id.clone());

        if let (Some(session), Some(current_office)) = (session.as_ref(), broadcast_office.as_ref())
        {
            if current_office != &data.office_id {
                warn!(
                    sid = %sid,
                    session_office = %current_office,
                    payload_office = %data.office_id,
                    "Ignoring leave_office payload office_id; using session office"
                );
            }

            let notification = if session.role == ClientRole::Computer {
                LeaveOfficeNotification {
                    office_id: current_office.clone(),
                    computer: Some(session.name.clone()),
                    agent: None,
                }
            } else {
                LeaveOfficeNotification {
                    office_id: current_office.clone(),
                    computer: None,
                    agent: Some(session.name.clone()),
                }
            };

            let _ = socket
                .within(Self::office_room(current_office))
                .emit(smcp::events::NOTIFY_LEAVE_OFFICE, &notification)
                .await;
        }

        // 提交点重读（#226 P0-3）：只在会话**仍**位于我刚刚广播的那个房时才清空。若该 `await` 期间
        // 并发 `join` 已把会话迁到别的房，则本调用**不**改动会话——否则会把新状态倒着覆盖成「无房」，
        // 而 socket 仍留在新房，成为继续收 `notify:*`、`list_room` 却查不到的幽灵成员。
        match state
            .session_manager
            .commit_leave(&sid, broadcast_office.as_deref())
        {
            Ok(LeaveCommit::Superseded { current }) => warn!(
                sid = %sid,
                current_office = %current,
                "leave_office superseded by a concurrent room change; keeping the newer state"
            ),
            Ok(_) => {}
            Err(SessionError::NotFound(_)) => {}
            Err(e) => warn!(sid = %sid, "leave_office commit failed: {}", e),
        }

        // 收敛真实成员关系（协议 events.md §server:leave_office：无房时 SHOULD 按真实成员关系收敛一次，
        // 使漂移态可自愈、操作可重试）。目标取**重读后**的会话状态，故并发的换房会被如实保留。
        let office_after = state
            .session_manager
            .get_session(&sid)
            .and_then(|s| s.office_id);
        Self::converge_socket_rooms(&socket, office_after.as_deref());

        Ok(())
    }

    /// 把 socket 的房间成员关系收敛到权威会话状态：加入该在的房，退出其余全部房间。
    ///
    /// 这是「会话说它不在任何房，socket 却还在房里」这类**漂移态**的唯一自愈路径：只增不减或只减不增
    /// 都只能收敛一半。收敛目标恒为会话状态（服务端权威），**不**取自客户端载荷。
    ///
    /// Converge Socket.IO membership onto the authoritative session state (both directions).
    fn converge_socket_rooms(socket: &SocketRef, office_id: Option<&str>) {
        let expected = office_id.map(Self::office_room);
        for room in socket.rooms() {
            if expected.as_deref() != Some(room.as_ref()) {
                socket.leave(room.into_owned());
            }
        }
        if let Some(expected) = expected {
            socket.join(expected);
        }
    }

    /// 房间管理拒绝的唯一构造入口：`SessionError` → 协议 canonical flat `ErrorPayload`。
    ///
    /// 历史实现把 `SessionError` 的内部 `Display`（外层还套 `HandlerError` 的 `"Session error: "` 前缀）
    /// 直接序列化上 wire，既与协议 / python-sdk 的 canonical 文案逐字不符，也把内部错误类名泄给对端
    /// （#226 P1-5）。本函数是「协议码 → 文案 / `details` 白名单」在服务端的唯一出口，调用方无从绕过。
    ///
    /// `declared_role` 取自**发起者自己的会话**（4105 的 `details.role` 报的是发起者声明的 role，
    /// 不是冲突方的），`target_office_id` 取自请求载荷（发起者自己声明的目标房）。
    fn room_rejection(
        error: &SessionError,
        declared_role: &ClientRole,
        target_office_id: &str,
    ) -> smcp::ErrorPayload {
        use smcp::RoomRejectionCode as Code;
        match error {
            // 4106：details 报会话**当前**所在房（非被拒的目标房）——该值由 `reserve_join` 在
            // 临界区内从权威会话读出，故并发下也不会报过期房号。
            SessionError::AgentAlreadyInRoom(current) => smcp::build_room_rejection_error(
                Code::AlreadyInRoom,
                smcp::RoomRejectionContext {
                    current_office_id: Some(current),
                    ..Default::default()
                },
            ),
            SessionError::AgentAlreadyExists => smcp::build_room_rejection_error(
                Code::RoomFull,
                smcp::RoomRejectionContext {
                    target_office_id: Some(target_office_id),
                    ..Default::default()
                },
            ),
            SessionError::NameAlreadyRegistered(_) => smcp::build_room_rejection_error(
                Code::NameConflict,
                smcp::RoomRejectionContext {
                    target_office_id: Some(target_office_id),
                    declared_role: Some(&declared_role.to_string()),
                    ..Default::default()
                },
            ),
            // 会话不存在 / 状态非法属**内部**事故，不属房间语义：回笼统 500（协议空档内以通用码承载），
            // 原文只进日志。绝不把内部错误文本当房间拒绝文案上 wire。
            SessionError::NotFound(_) | SessionError::InvalidState(_) => {
                smcp::ErrorPayload::from_error_code(
                    smcp::ErrorCode::InternalError,
                    "Internal error",
                )
            }
        }
    }

    /// 处理工具调用取消事件
    async fn on_server_tool_call_cancel(
        socket: SocketRef,
        data: AgentCallData,
        state: ServerState,
    ) {
        let sid = socket.id.to_string();
        let session = match state.session_manager.get_session(&sid) {
            Some(s) => s,
            None => {
                warn!("SERVER_TOOL_CALL_CANCEL from unknown session sid={}", sid);
                return;
            }
        };

        // Python 侧语义：向 office(room) 广播并跳过自己
        // 这里沿用 socketioxide 的 to(room) 语义：从当前 socket 触发时，会自动排除自身。

        // 角色断言：取消工具调用通常由 Agent 发起
        if session.role != ClientRole::Agent {
            warn!(
                "SERVER_TOOL_CALL_CANCEL role mismatch: expected Agent, got {:?}, sid={}",
                session.role, sid
            );
            return;
        }

        let office_id = match session.office_id {
            Some(ref office_id) => office_id.clone(),
            None => {
                warn!(
                    "SERVER_TOOL_CALL_CANCEL but session not in office, sid={}",
                    sid
                );
                return;
            }
        };

        if let Err(e) = socket
            .to(Self::office_room(&office_id))
            .emit(smcp::events::NOTIFY_TOOL_CALL_CANCEL, &data)
            .await
        {
            warn!("Failed to broadcast NOTIFY_TOOL_CALL_CANCEL: {}", e);
        }
    }

    /// 处理配置更新事件
    async fn on_server_update_config(
        socket: SocketRef,
        data: UpdateComputerConfigReq,
        state: ServerState,
    ) {
        let sid = socket.id.to_string();
        let session = match state.session_manager.get_session(&sid) {
            Some(s) => s,
            None => {
                warn!("SERVER_UPDATE_CONFIG from unknown session sid={}", sid);
                return;
            }
        };

        // 角色断言：配置更新通常由 Computer 发起
        if session.role != ClientRole::Computer {
            warn!(
                "SERVER_UPDATE_CONFIG role mismatch: expected Computer, got {:?}, sid={}",
                session.role, sid
            );
            return;
        }

        let office_id = match session.office_id {
            Some(ref office_id) => office_id.clone(),
            None => {
                warn!(
                    "SERVER_UPDATE_CONFIG but session not in office, sid={}",
                    sid
                );
                return;
            }
        };

        // 广播配置更新通知（向 office 广播并跳过自己）
        let notification = UpdateMCPConfigNotification {
            computer: data.computer.clone(),
        };

        let office_id_clone = office_id.clone();
        let computer_clone = data.computer.clone();
        info!(
            "Broadcasting NOTIFY_UPDATE_CONFIG to room '{}' from computer '{}' (sid: {})",
            office_id_clone, computer_clone, sid
        );

        if let Err(e) = socket
            .to(Self::office_room(&office_id))
            .emit(smcp::events::NOTIFY_UPDATE_CONFIG, &notification)
            .await
        {
            warn!("Failed to broadcast NOTIFY_UPDATE_CONFIG: {}", e);
        } else {
            info!(
                "Successfully broadcasted NOTIFY_UPDATE_CONFIG to room '{}'",
                office_id
            );
        }
    }

    /// 处理工具列表更新事件
    async fn on_server_update_tool_list(
        socket: SocketRef,
        data: UpdateComputerConfigReq,
        state: ServerState,
    ) {
        let sid = socket.id.to_string();
        let session = match state.session_manager.get_session(&sid) {
            Some(s) => s,
            None => {
                warn!("SERVER_UPDATE_TOOL_LIST from unknown session sid={}", sid);
                return;
            }
        };

        // 角色断言：工具列表更新通常由 Computer 发起
        if session.role != ClientRole::Computer {
            warn!(
                "SERVER_UPDATE_TOOL_LIST role mismatch: expected Computer, got {:?}, sid={}",
                session.role, sid
            );
            return;
        }

        let office_id = match session.office_id {
            Some(ref office_id) => office_id.clone(),
            None => {
                warn!(
                    "SERVER_UPDATE_TOOL_LIST but session not in office, sid={}",
                    sid
                );
                return;
            }
        };

        // 广播工具列表更新通知（向 office 广播并跳过自己）
        let notification = UpdateToolListNotification {
            computer: data.computer,
        };

        if let Err(e) = socket
            .to(Self::office_room(&office_id))
            .emit(smcp::events::NOTIFY_UPDATE_TOOL_LIST, &notification)
            .await
        {
            warn!("Failed to broadcast NOTIFY_UPDATE_TOOL_LIST: {}", e);
        }
    }

    /// 处理 SKILL 集合更新事件（SRV-02 #50）/ broadcast `server:update_skills` → `notify:update_skills`.
    ///
    /// Computer 在 SKILL 集合变化（增/删/物化更新）时触发；Server **仅**在来源为 Computer 时向其所在 office
    /// 广播 `notify:update_skills`（跳过自己），Agent 据此自动重拉 `client:get_skills`（仿既有 `notify:update_*`
    /// 自动刷新模式）。非法来源（非 Computer / 无 office）被拒绝、不广播。对标 Python `on_server_update_skills`；
    /// 载荷复用 `UpdateComputerConfigReq`（仅 `computer` 字段，三事件族共用）。
    async fn on_server_update_skills(
        socket: SocketRef,
        data: UpdateComputerConfigReq,
        state: ServerState,
    ) {
        let sid = socket.id.to_string();
        let session = match state.session_manager.get_session(&sid) {
            Some(s) => s,
            None => {
                warn!("SERVER_UPDATE_SKILLS from unknown session sid={}", sid);
                return;
            }
        };

        // 仅 Computer 可上报 SKILL 变更（非法来源拒绝）。
        if session.role != ClientRole::Computer {
            warn!(
                "SERVER_UPDATE_SKILLS role mismatch: expected Computer, got {:?}, sid={}",
                session.role, sid
            );
            return;
        }

        let office_id = match session.office_id {
            Some(ref office_id) => office_id.clone(),
            None => {
                warn!(
                    "SERVER_UPDATE_SKILLS but session not in office, sid={}",
                    sid
                );
                return;
            }
        };

        // 广播 SKILL 更新通知（向 office 广播并跳过自己）；载荷复用 `{computer}` 形态。
        let notification = UpdateMCPConfigNotification {
            computer: data.computer,
        };
        if let Err(e) = socket
            .to(Self::office_room(&office_id))
            .emit(smcp::events::NOTIFY_UPDATE_SKILLS, &notification)
            .await
        {
            warn!("Failed to broadcast NOTIFY_UPDATE_SKILLS: {}", e);
        }
    }

    /// 构造 bare flat `ErrorPayload(404)`（Computer 未命中 / 中途消失）的 ack 负载。
    /// 复用 [`smcp::build_computer_not_found_error`]（与 #47/#92 同 payload，不泄露存在性、不新增错误码）。
    fn computer_not_found_value(computer_name: &str) -> Result<Value, HandlerError> {
        Ok(serde_json::to_value(build_computer_not_found_error(
            computer_name,
        ))?)
    }

    /// 构造 bare flat `ErrorPayload(4103)`（发起方无房 ⇒ 无从定位任何 Computer）的 ack 负载。
    ///
    /// 协议依据 / Protocol: `error-handling.md` §Not In Room（4103）——**触发时机**明列
    /// 「会话尚无 `office_id`（未成功加入任何房间）时，发起**需要房间上下文**的操作：`client:*`
    /// 路由请求、`server:list_room` 等」。故无房来源的 `client:*` MUST 回 4103，而**不是**
    /// 「目标 Computer 不存在」的 404——后者会把「你不在任何房间」这个调用方可自纠的状态
    /// （先入房再重试）伪装成「这个 Computer 不存在」（换个目标重试也永远不会成功）。
    ///
    /// 与 `server:list_room` 共用 [`smcp::build_room_rejection_error`] 这一唯一 choke point，
    /// 故文案（`Not in any room`）与「无 `details`」两条线上形态天然一致。
    fn not_in_room_value() -> Result<Value, HandlerError> {
        Ok(serde_json::to_value(smcp::build_room_rejection_error(
            smcp::RoomRejectionCode::NotInRoom,
            smcp::RoomRejectionContext::default(),
        ))?)
    }

    /// 处理客户端工具调用事件
    /// 统一 `client:*` 事件路由 / Generic `client:*` event router.
    ///
    /// 收敛 office/role 隔离校验、Computer SID 解析、flat `ErrorPayload` 透传，让每个 `client:*`
    /// handler 缩到一行。对标 Python `server/namespace.py::_relay_client_call`。
    ///
    /// 投递到 ack 的负载为 **bare** 形态（成功 Ret 原样 或 flat [`smcp::ErrorPayload`]），**无**
    /// `{"Ok"/"Err"}` / `{"error":{...}}` 嵌套 envelope（协议 0.2.2：`client:*` ack 协议级错误
    /// MUST 为 flat ErrorPayload）。
    ///
    /// - 目标 Computer 在**发起方 office 内**未命中（含跨 office：office-scoped 查找天然不可达，
    ///   不泄露存在性）/ 发起方无 office / 目标 SID 已断连 → 返回 flat `ErrorPayload(404)`（经 ack 投递）。
    /// - 目标 ack 为协议级 flat ErrorPayload（[`smcp::is_protocol_error_payload`]）→ 原样透传；
    ///   成功响应同样 **bare 透传**，**不**剥 `"result"`（旧实现的 `map.remove("result")` 是 bug：
    ///   Computer 发送 bare `CallToolResult`，剥取把成功结果误抹为 `Null`）。
    /// - 发起方非 Agent → [`HandlerError::Isolation`]（隔离拒绝）；发起方会话已断连 →
    ///   [`HandlerError::Session`]：两者调用点均**不投递协议 ack**（镜像 Python `raise` 语义，
    ///   发起方侧自行超时；不泄露 Computer 存在性、不造非协议错误码）。
    ///
    /// ## 在途断连容错（#56 SRV-04）/ In-flight disconnect tolerance
    ///
    /// - **目标 Computer 飞行中断连**：socketioxide `emit_with_ack` 的等待原语不监听掉线，目标在途
    ///   断连会空等满 `timeout`。故把 ack 流与 [`InflightDisconnectRegistry`] 的目标断连信号竞速
    ///   （对标 Python `_relay_client_call` #100）——断连先到 → 立即回 flat `ErrorPayload(404)`
    ///   （目标中途消失 == 不存在，与 #47/#92 同一 payload）。真超时仍原样回 [`HandlerError::Timeout`]，
    ///   **绝不**转 404（守住「超时 ≠ 不存在」语义区分）。
    /// - **原发起方（Agent）飞行中断连**：relay 完成后复查发起方会话——已断连 → 静默丢弃
    ///   （返回 [`HandlerError::Session`]，与前置检查同变体；调用点不投 ack；协议 0.2.2：Server MAY
    ///   不 ack、不投 ErrorPayload）。session 不可达全程经受控 `Result` 收编，**MUST NOT** panic。
    async fn relay_client_call<T: serde::Serialize + ?Sized>(
        socket: &SocketRef,
        computer_name: &str,
        data: &T,
        event: &str,
        state: &ServerState,
        timeout: tokio::time::Duration,
    ) -> Result<Value, HandlerError> {
        // 发起方（Agent）会话 / Originator (Agent) session
        let sid = socket.id.to_string();
        // 无会话记录：连接从未 join 过，或会话已随断连注销（飞行中消失）。
        //
        // 这两类**不**回 4103，保持「不投递 ack」的历史语义（协议 0.2.2：Server MAY 不 ack、
        // 不投 ErrorPayload；镜像 Python `_relay_client_call` 的 raise）：服务端此时连「是谁在问」
        // 都无从确认，回一个「你不在任何房间」的房间语义拒绝反而是假装知道对方身份。
        // **有会话、无房**（join 被拒 / 已退房）才是协议 §4103 的触发态，见下方分支。
        let session = state
            .session_manager
            .get_session(&sid)
            .ok_or_else(|| HandlerError::Session(SessionError::NotFound(sid.clone())))?;

        // 角色隔离：仅 Agent 可发起 client:* 调用 / Role isolation: only Agents issue client:* calls.
        // 运行期 `Result`（**非** `debug_assert!`）→ release build 下隔离不变量同样硬化。
        if session.role != ClientRole::Agent {
            return Err(HandlerError::Isolation(
                "only agents may issue client:* calls".to_string(),
            ));
        }

        // 发起方无 office → 无从在任何 office 内定位目标 → flat **4103** `Not in any room`。
        //
        // 协议依据：`error-handling.md` §Not In Room 把 `client:*` 路由请求明列为触发场景
        // （#226 复审 建议项 3 / 本轮按协议接线）。此前回的是「目标 Computer 找不到」的 404，
        // 把「先入房再重试即可」的可自纠状态误导成「换个目标才有用」。
        let Some(office_id) = session.office_id else {
            return Self::not_in_room_value();
        };

        // office-scoped 解析目标 Computer SID：跨 office 目标天然不可达 → 404（不泄露存在性）
        let Some(computer_sid) = state
            .session_manager
            .get_computer_sid_in_office(&office_id, computer_name)
        else {
            return Self::computer_not_found_value(computer_name);
        };

        // 目标 socket：SID 已解析但 socket 不在 → 目标已断连，按「不存在」回 404
        let target_socket = match computer_sid.parse() {
            Ok(parsed) => state
                .io
                .of(SMCP_NAMESPACE)
                .and_then(|op| op.get_socket(parsed)),
            Err(_) => None,
        };
        let Some(target_socket) = target_socket else {
            return Self::computer_not_found_value(computer_name);
        };

        // ── 在途断连守卫（#56 SRV-04）登记 ──────────────────────────────────────────
        // 针对目标 Computer SID 登记一个断连信号；目标飞行中断连时 `on_disconnect` 会 fire 它。
        // Drop guard 兜底注销，覆盖所有 return 路径（panic-safe）。
        let disconnect = state.inflight_disconnect.register(&computer_sid);
        let _signal_guard = InflightSignalGuard {
            registry: &state.inflight_disconnect,
            computer_sid: &computer_sid,
            notify: disconnect.clone(),
        };

        // TOCTOU 复查：解析目标 SID 与登记信号之间目标可能已断连（其信号 fire 早于本次 register →
        // 永不会唤醒我们）。`on_disconnect` 内 `unregister_session` 早于 `fire`，且 register/fire 经
        // 注册表 Mutex 串行化——故复查**我方 session_manager**（权威同步态）即可气密闭合竞速窗，
        // **不**依赖 socketioxide 摘除 socket 的内部时序。对齐 Python `get_sid_by_name is None` 复查。
        if state.session_manager.get_session(&computer_sid).is_none() {
            return Self::computer_not_found_value(computer_name);
        }

        // 转发并把「首个 ack」与「目标断连信号」竞速 / Relay, racing the first ack vs disconnect.
        let ack_result = target_socket.emit_with_ack(event, data);
        let ack_fut = async move {
            match ack_result {
                Ok(stream) => {
                    let mut pinned = Box::pin(stream);
                    match pinned.next().await {
                        Some((_, response)) => response,
                        None => Ok(Value::Null),
                    }
                }
                Err(_) => Ok(Value::Null),
            }
        };
        let raced = tokio::time::timeout(timeout, async {
            tokio::select! {
                // biased：就绪的响应优先于断连信号（目标响应后又断连时仍取回有效结果，
                // 对齐 Python `if call_task in done` 先判定）。
                biased;
                resp = ack_fut => Some(resp),
                _ = disconnect.notified() => None,
            }
        })
        .await;

        let response = match raced {
            // 真超时：原样回 Timeout，**绝不**转 404（守住「超时 ≠ 不存在」语义区分，对齐 Python）。
            Err(_) => {
                return Err(HandlerError::Timeout(format!(
                    "{event} timed out after {}s",
                    timeout.as_secs()
                )))
            }
            // 目标飞行中断连先到：目标中途消失 == 不存在，回 flat 404（与 #47/#92 同一 payload；
            // 协议 0.2.2：Server MAY 静默丢弃、不新增错误码）。
            Ok(None) => return Self::computer_not_found_value(computer_name),
            Ok(Some(Ok(response))) => response,
            Ok(Some(Err(e))) => {
                return Err(HandlerError::Timeout(format!(
                    "Failed to get response from computer: {e}"
                )))
            }
        };

        // 原发起方（Agent）在途断连容错：relay 等待期间发起方可能已断连。此时静默丢弃——不向已消失的
        // AckSender 投递（协议 0.2.2：Server MAY 不 ack、不投 ErrorPayload）。显式受控 `Result` 让调用点
        // 走 no-ack 分支，而非依赖 ack.send 的隐式 no-op；session 不可达不 panic。与前置发起方检查
        // （`get_session().ok_or_else(NotFound)`）同变体，语义自洽（会话存活性 ≠ 角色授权拒绝）。
        if state.session_manager.get_session(&sid).is_none() {
            return Err(HandlerError::Session(SessionError::NotFound(sid.clone())));
        }

        // bare 透传：成功 Ret 或 flat ErrorPayload 原样返回（无嵌套 envelope、无 "result" 剥取）。
        Ok(response)
    }

    /// 处理工具调用事件（经统一 [`Self::relay_client_call`] 转发）。
    async fn on_client_tool_call(
        socket: SocketRef,
        data: ToolCallReq,
        state: ServerState,
    ) -> Result<Value, HandlerError> {
        Self::relay_client_call(
            &socket,
            &data.computer,
            &data,
            smcp::events::CLIENT_TOOL_CALL,
            &state,
            tokio::time::Duration::from_secs(30),
        )
        .await
    }

    /// 处理获取工具列表事件（经统一 [`Self::relay_client_call`] 转发）。
    async fn on_client_get_tools(
        socket: SocketRef,
        data: GetToolsReq,
        state: ServerState,
    ) -> Result<Value, HandlerError> {
        Self::relay_client_call(
            &socket,
            &data.computer,
            &data,
            smcp::events::CLIENT_GET_TOOLS,
            &state,
            tokio::time::Duration::from_secs(30),
        )
        .await
    }

    /// 处理获取桌面信息事件（经统一 [`Self::relay_client_call`] 转发）。
    async fn on_client_get_desktop(
        socket: SocketRef,
        data: GetDesktopReq,
        state: ServerState,
    ) -> Result<Value, HandlerError> {
        Self::relay_client_call(
            &socket,
            &data.computer,
            &data,
            smcp::events::CLIENT_GET_DESKTOP,
            &state,
            tokio::time::Duration::from_secs(30),
        )
        .await
    }

    /// 处理获取计算机配置事件（经统一 [`Self::relay_client_call`] 转发）。
    async fn on_client_get_config(
        socket: SocketRef,
        data: GetComputerConfigReq,
        state: ServerState,
    ) -> Result<Value, HandlerError> {
        Self::relay_client_call(
            &socket,
            &data.computer,
            &data,
            smcp::events::CLIENT_GET_CONFIG,
            &state,
            tokio::time::Duration::from_secs(30),
        )
        .await
    }

    /// 透明转发 `client:get_skills` 至目标 Computer（SKILL 轻量元数据清单）/ relay `client:get_skills`.
    async fn on_client_get_skills(
        socket: SocketRef,
        data: GetSkillsReq,
        state: ServerState,
    ) -> Result<Value, HandlerError> {
        Self::relay_client_call(
            &socket,
            &data.computer,
            &data,
            smcp::events::CLIENT_GET_SKILLS,
            &state,
            tokio::time::Duration::from_secs(30),
        )
        .await
    }

    /// 透明转发 `client:get_skill` 至目标 Computer（单 SKILL 详情/资源；4016/4017 flat 透传）/ relay `client:get_skill`.
    async fn on_client_get_skill(
        socket: SocketRef,
        data: GetSkillReq,
        state: ServerState,
    ) -> Result<Value, HandlerError> {
        Self::relay_client_call(
            &socket,
            &data.computer,
            &data,
            smcp::events::CLIENT_GET_SKILL,
            &state,
            tokio::time::Duration::from_secs(30),
        )
        .await
    }

    /// 透明转发 `client:get_blob` 至目标 Computer（Server 不重组，逐 ack 透传分块）/ relay `client:get_blob`.
    async fn on_client_get_blob(
        socket: SocketRef,
        data: GetBlobReq,
        state: ServerState,
    ) -> Result<Value, HandlerError> {
        Self::relay_client_call(
            &socket,
            &data.computer,
            &data,
            smcp::events::CLIENT_GET_BLOB,
            &state,
            tokio::time::Duration::from_secs(30),
        )
        .await
    }

    /// 透明转发 `client:put_blob` 至目标 Computer（Server 不重组；Computer 的 4019 flat ErrorPayload
    /// 原样回传，与 `get_blob` 同构）/ relay `client:put_blob` (v0.4.0 #195).
    async fn on_client_put_blob(
        socket: SocketRef,
        data: PutBlobReq,
        state: ServerState,
    ) -> Result<Value, HandlerError> {
        Self::relay_client_call(
            &socket,
            &data.computer,
            &data,
            smcp::events::CLIENT_PUT_BLOB,
            &state,
            tokio::time::Duration::from_secs(30),
        )
        .await
    }

    /// 透明转发 `client:get_resources` 至目标 Computer（含 cursor 翻页；4014/4015 flat 透传）/ relay `client:get_resources`.
    async fn on_client_get_resources(
        socket: SocketRef,
        data: GetResourcesReq,
        state: ServerState,
    ) -> Result<Value, HandlerError> {
        Self::relay_client_call(
            &socket,
            &data.computer,
            &data,
            smcp::events::CLIENT_GET_RESOURCES,
            &state,
            tokio::time::Duration::from_secs(30),
        )
        .await
    }

    /// 处理桌面更新事件
    async fn on_server_update_desktop(
        socket: SocketRef,
        data: UpdateComputerConfigReq,
        state: ServerState,
    ) {
        let sid = socket.id.to_string();
        let session = match state.session_manager.get_session(&sid) {
            Some(s) => s,
            None => {
                warn!("SERVER_UPDATE_DESKTOP from unknown session sid={}", sid);
                return;
            }
        };

        // 角色断言：桌面更新通常由 Computer 发起
        if session.role != ClientRole::Computer {
            warn!(
                "SERVER_UPDATE_DESKTOP role mismatch: expected Computer, got {:?}, sid={}",
                session.role, sid
            );
            return;
        }

        let office_id = match session.office_id {
            Some(ref office_id) => office_id.clone(),
            None => {
                warn!(
                    "SERVER_UPDATE_DESKTOP but session not in office, sid={}",
                    sid
                );
                return;
            }
        };

        // 广播桌面更新通知（向 office 广播并跳过自己）
        let notification = UpdateMCPConfigNotification {
            computer: data.computer,
        };

        if let Err(e) = socket
            .to(Self::office_room(&office_id))
            .emit(smcp::events::NOTIFY_UPDATE_DESKTOP, &notification)
            .await
        {
            warn!("Failed to broadcast NOTIFY_UPDATE_DESKTOP: {}", e);
        }
    }

    /// 处理列出房间事件
    async fn on_server_list_room(
        socket: SocketRef,
        data: ListRoomReq,
        state: ServerState,
    ) -> RoomAckResult<ListRoomRet> {
        // 获取发起者会话信息
        let sid = socket.id.to_string();
        let session = match state.session_manager.get_session(&sid) {
            Some(s) => s,
            None => {
                warn!("List room from unknown session sid={}", sid);
                // 无会话 == 无房（会话懒建于 join）：协议把「无房却发起需要房间上下文的操作」
                // 定为 4103 `Not in any room`。canonical 文案由 builder 单点产出。
                return Err(Box::new(smcp::build_room_rejection_error(
                    smcp::RoomRejectionCode::NotInRoom,
                    smcp::RoomRejectionContext::default(),
                )));
            }
        };

        if session.office_id.is_none() {
            warn!("List room from session outside an office sid={}", sid);
            return Err(Box::new(smcp::build_room_rejection_error(
                smcp::RoomRejectionCode::NotInRoom,
                smcp::RoomRejectionContext::default(),
            )));
        }

        // 房间隔离：仅可查询自己所在的 office。判定下沉到纯函数 [`Self::list_room_authorized`]
        // （运行期 `Result`/分支，**非** `debug_assert!`）→ release build 下隔离同样硬化，对标
        // Python `-O` 回归 `test_isolation_hardening`：断言被剥离时跨房间访问仍 MUST 拒绝、不泄露
        // 另一房间会话。无 office（None）同样不授权。
        if !Self::list_room_authorized(session.office_id.as_deref(), &data.office_id) {
            warn!(
                "list_room isolation rejected: session {} (office {:?}) requested room {}",
                sid, session.office_id, data.office_id
            );
            return Err(Box::new(smcp::build_room_rejection_error(
                smcp::RoomRejectionCode::CrossRoomAccess,
                smcp::RoomRejectionContext {
                    target_office_id: Some(&data.office_id),
                    ..Default::default()
                },
            )));
        }

        // 获取指定办公室的所有会话
        let sessions = state
            .session_manager
            .get_sessions_in_office(&data.office_id);

        // 转换为 SessionInfo 列表
        let session_infos: Vec<SessionInfo> = sessions
            .into_iter()
            .map(|s| SessionInfo {
                sid: s.sid,
                name: s.name,
                role: s.role.into(),
                office_id: s.office_id.unwrap_or_default(),
                a2c_version: s.a2c_version,
            })
            .collect();

        Ok(ListRoomRet {
            sessions: session_infos,
            req_id: data.base.req_id,
        })
    }

    /// 处理加入房间的逻辑
    ///
    /// 三个分支都以**权威会话状态**为收敛目标（`Noop` 亦收敛，见下），故 `join` 与 `leave` 两条路径
    /// 对「会话说在房、socket 不在房」这类漂移态的对策**对称**：只增不减或只减不增都只能收敛一半。
    ///
    /// ⚠️ **残余窗口（如实标注，不宣称消除）**：`apply_join_room` 操作的是 socketioxide 的成员表，
    /// 而权威状态在 [`SessionManager`] 里——两者是**双存储**，写入之间没有共同锁。故在
    /// 「handler 读会话」与「socket.join/leave 生效」之间仍存在一个极窄的残余窗；本 PR 消除的是
    /// **可观测**的幽灵成员（`leave` 用提交点重读、`join` 用临界区内一次完成校验+提交），把该窗收窄到
    /// 单次 `await` 的调度粒度，而**不是**把双存储变成单存储。
    async fn apply_join_room(
        socket: SocketRef,
        session: &SessionData,
        office_id: &str,
        decision: JoinDecision,
    ) {
        info!(
            "handle_join_room called: sid={}, office_id={}, role={:?}",
            socket.id, office_id, session.role
        );

        match decision {
            JoinDecision::Noop => {
                // 重复入同一房不改变权威状态，但**仍**收敛一次 socket 成员关系：`leave` 会收敛而 `join`
                // 不收敛会形成不对称——漂移态（如 leave 的「读会话 → 收敛」之间的窄窗，或历史版本
                // 残留的脏成员关系）只会在一次 `leave` 后才自愈。收敛是幂等的（已在目标房 ⇒
                // `join` 无副作用；不在 ⇒ 补上），开销可忽略，故没有理由不做。
                info!(
                    "Noop decision for sid={}; converging socket rooms",
                    socket.id
                );
                Self::converge_socket_rooms(&socket, Some(office_id));
            }
            JoinDecision::Join => {
                info!("Joining room '{}' for sid={}", office_id, socket.id);
                socket.join(Self::office_room(office_id));
            }
            JoinDecision::LeaveAndJoin { leave_office } => {
                info!(
                    "Leaving room '{}' and joining '{}' for sid={}",
                    leave_office, office_id, socket.id
                );

                // 构建离开通知（Python语义：切换房间前需要通知旧房间）
                let leave_notification = if session.role == ClientRole::Computer {
                    LeaveOfficeNotification {
                        office_id: leave_office.clone(),
                        computer: Some(session.name.clone()),
                        agent: None,
                    }
                } else {
                    LeaveOfficeNotification {
                        office_id: leave_office.clone(),
                        computer: None,
                        agent: Some(session.name.clone()),
                    }
                };

                // 向旧房间广播离开消息
                let _ = socket
                    .within(Self::office_room(&leave_office))
                    .emit(smcp::events::NOTIFY_LEAVE_OFFICE, &leave_notification)
                    .await;

                socket.leave(Self::office_room(&leave_office));
                socket.join(Self::office_room(office_id));
            }
        }
    }

    /// `server:list_room` 房间隔离判定（纯函数）。/ Room-isolation predicate for `server:list_room`.
    ///
    /// 返回发起方（其 office 为 `session_office`）是否获准查询 `requested_office`——仅限本房间。
    /// 无 office（`None`）一律不授权。**运行期**判定（**非** `debug_assert!`/`cfg(debug_assertions)`），
    /// 故 release build 下隔离不变量同样硬化；对标 Python `-O` 隔离回归 `test_isolation_hardening`。
    fn list_room_authorized(session_office: Option<&str>, requested_office: &str) -> bool {
        session_office == Some(requested_office)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::DefaultAuthenticationProvider;
    use serde_json;

    #[tokio::test]
    async fn test_agent_join_office() {
        let (_layer, io) = SocketIo::builder().build_layer();
        let state = ServerState {
            session_manager: Arc::new(SessionManager::new()),
            auth_provider: Arc::new(DefaultAuthenticationProvider::new(
                Some("test_secret".to_string()),
                None,
            )),
            io: Arc::new(io.clone()),
            inflight_disconnect: Arc::new(InflightDisconnectRegistry::default()),
        };

        // 注册处理器
        SmcpHandler::register_handlers(&io, state.clone());

        // 测试逻辑需要实际的 Socket.IO 客户端连接
        // 这里只做基本的单元测试
        assert_eq!(state.session_manager.get_stats().total, 0);
    }

    #[test]
    fn test_handler_error_serialize() {
        // 协议 0.2.2：HandlerError 序列化为 flat ErrorPayload（顶层 code/message），
        // 禁止嵌套 {"error":{...}} envelope。断言 flat 形态，使回退到嵌套时测试失败。
        let err = HandlerError::InvalidRequest("bad".to_string());
        let v: serde_json::Value = serde_json::to_value(&err).unwrap();

        assert_eq!(v["code"], smcp::error_codes::BAD_REQUEST); // InvalidRequest → 400
        let message = v["message"].as_str().expect("message 应为顶层字符串");
        assert!(message.contains("Invalid request"));
        assert!(message.contains("bad"));
        assert!(v.get("error").is_none(), "禁止嵌套 envelope"); // 回退到 {"error":{...}} 即失败
    }

    #[test]
    fn test_session_identifier_is_not_exposed_in_error_payload() {
        let err = HandlerError::Session(SessionError::NotFound("private-sid".to_string()));
        let payload = err.to_error_payload();
        assert_eq!(payload.code, i64::from(smcp::error_codes::NOT_FOUND));
        assert_eq!(payload.message, "Session not found");
        assert!(!payload.message.contains("private-sid"));
    }

    // ── #56 SRV-04：在途断连信号注册表 ──────────────────────────────────────────────

    #[tokio::test]
    async fn test_inflight_signal_fire_wakes_waiter() {
        // 目标断连 fire 后，在途 relay 的 notified() MUST 立即就绪（即便先 fire 后 await——
        // notify_one 暂存 permit，闭合解析 SID 与登记之间的竞速窗）。
        let reg = InflightDisconnectRegistry::default();
        let notify = reg.register("computer-sid");
        reg.fire("computer-sid");
        tokio::time::timeout(std::time::Duration::from_millis(200), notify.notified())
            .await
            .expect("fire 后 notified 应立即就绪");
    }

    #[tokio::test]
    async fn test_inflight_signal_fire_wakes_all_for_same_target() {
        // 同一目标的多个在途 relay：fire 一次 MUST 全部唤醒。
        let reg = InflightDisconnectRegistry::default();
        let a = reg.register("computer-sid");
        let b = reg.register("computer-sid");
        reg.fire("computer-sid");
        for n in [a, b] {
            tokio::time::timeout(std::time::Duration::from_millis(200), n.notified())
                .await
                .expect("同目标全部在途信号均应被唤醒");
        }
    }

    #[tokio::test]
    async fn test_inflight_signal_discard_then_fire_is_noop() {
        // discard 后该信号已脱离注册表：fire 不应唤醒它（无 permit），且对空桶 fire 不 panic。
        let reg = InflightDisconnectRegistry::default();
        let notify = reg.register("computer-sid");
        reg.discard("computer-sid", &notify);
        reg.fire("computer-sid"); // 桶已回收：幂等、不 panic
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), notify.notified())
                .await
                .is_err(),
            "discard 后的信号不应被 fire 唤醒"
        );
    }

    #[test]
    fn test_inflight_signal_fire_unknown_target_is_noop() {
        // 无任何在途请求时对未知/已断连目标 fire：幂等、不 panic（on_disconnect 对每个掉线 sid 都会 fire）。
        let reg = InflightDisconnectRegistry::default();
        reg.fire("never-registered");
    }

    #[test]
    fn test_isolation_error_maps_to_forbidden_and_is_flat() {
        // 隔离拒绝错误码归入 FORBIDDEN（仅内部诊断，永不进 client:* ack）；序列化仍为 flat 形态。
        let err = HandlerError::Isolation("only agents".to_string());
        assert_eq!(err.error_code(), smcp::error_codes::FORBIDDEN);
        let v: serde_json::Value = serde_json::to_value(&err).unwrap();
        assert_eq!(v["code"], smcp::error_codes::FORBIDDEN);
        assert!(v.get("error").is_none(), "禁止嵌套 envelope");
    }

    /// #226 复审 🟡7：500 降级路径（`room_rejection` 的 `NotFound | InvalidState → InternalError`）
    /// 此前零覆盖。Python 有三条对应用例（`test_unknown_exception_is_500_without_echoing_detail` /
    /// `test_unmapped_rejection_code_degrades_to_500` / `test_non_int_rejection_code_degrades_to_500`）。
    ///
    /// 这两类错误属**内部**事故，不属房间语义：必须回笼统 `500 Internal error`，**不得**把内部
    /// 错误文本（含 `sid` 等标识）当房间拒绝文案上 wire——那既与协议 canonical 文案不符，也是信息泄露。
    #[test]
    fn test_room_rejection_internal_failures_degrade_to_generic_500() {
        for error in [
            SessionError::NotFound("private-sid".to_string()),
            SessionError::InvalidState("private internal state".to_string()),
        ] {
            let payload = SmcpHandler::room_rejection(&error, &ClientRole::Agent, "office-a");
            assert_eq!(payload.code, i64::from(smcp::error_codes::INTERNAL_ERROR));
            assert_eq!(payload.message, "Internal error");
            assert!(
                payload.details.is_none(),
                "内部降级不得携带 details: {payload:?}"
            );
            let serialized = serde_json::to_string(&payload).unwrap();
            assert!(
                !serialized.contains("private-"),
                "内部错误文本 MUST NOT 上 wire: {serialized}"
            );
        }
    }

    #[test]
    fn test_list_room_authorized_is_runtime_isolation_invariant() {
        // 对标 Python `-O` 回归 test_isolation_hardening：判定是运行期纯函数（**非** debug_assert!），
        // 故 release build 下同样硬化。跨房间 / 无 office MUST 拒绝；仅同房间放行。
        assert!(
            !SmcpHandler::list_room_authorized(Some("office_A"), "office_B"),
            "跨房间 MUST 拒绝（不得泄露另一房间会话）"
        );
        assert!(
            !SmcpHandler::list_room_authorized(None, "office_B"),
            "无 office MUST 拒绝"
        );
        assert!(
            SmcpHandler::list_room_authorized(Some("office1"), "office1"),
            "同房间应放行"
        );
    }

    #[test]
    fn test_office_room_is_disjoint_from_socket_sid_namespace() {
        let sid = "abc123";
        assert_eq!(SmcpHandler::office_room(sid), "office:abc123");
        assert_ne!(SmcpHandler::office_room(sid), sid);
    }

    #[test]
    fn test_enter_office_notification_computer() {
        let computer_name = "computer1".to_string();
        let office_id = "office1".to_string();

        let notification = EnterOfficeNotification {
            office_id: office_id.clone(),
            computer: Some(computer_name.clone()),
            agent: None,
        };

        assert_eq!(notification.office_id, office_id);
        assert_eq!(notification.computer, Some(computer_name));
        assert_eq!(notification.agent, None);
    }

    #[test]
    fn test_enter_office_notification_agent() {
        let agent_name = "agent1".to_string();
        let office_id = "office1".to_string();

        let notification = EnterOfficeNotification {
            office_id: office_id.clone(),
            computer: None,
            agent: Some(agent_name.clone()),
        };

        assert_eq!(notification.office_id, office_id);
        assert_eq!(notification.computer, None);
        assert_eq!(notification.agent, Some(agent_name));
    }

    #[test]
    fn test_leave_office_notification_computer() {
        let computer_name = "computer1".to_string();
        let office_id = "office1".to_string();

        let notification = LeaveOfficeNotification {
            office_id: office_id.clone(),
            computer: Some(computer_name.clone()),
            agent: None,
        };

        assert_eq!(notification.office_id, office_id);
        assert_eq!(notification.computer, Some(computer_name));
        assert_eq!(notification.agent, None);
    }

    #[test]
    fn test_update_tool_list_notification() {
        let computer_name = "computer1".to_string();

        let notification = UpdateToolListNotification {
            computer: computer_name.clone(),
        };

        assert_eq!(notification.computer, computer_name);
    }

    #[test]
    fn test_update_mcp_config_notification() {
        let computer_name = "computer1".to_string();

        let notification = UpdateMCPConfigNotification {
            computer: computer_name.clone(),
        };

        assert_eq!(notification.computer, computer_name);
    }

    #[test]
    fn test_notification_serialization() {
        // 验证通知类型序列化正确性
        let tool_list_notification = UpdateToolListNotification {
            computer: "computer1".to_string(),
        };

        let json = serde_json::to_string(&tool_list_notification).unwrap();
        assert!(json.contains("\"computer\":\"computer1\""));

        let mcp_config_notification = UpdateMCPConfigNotification {
            computer: "computer1".to_string(),
        };

        let json = serde_json::to_string(&mcp_config_notification).unwrap();
        assert!(json.contains("\"computer\":\"computer1\""));
    }
}
