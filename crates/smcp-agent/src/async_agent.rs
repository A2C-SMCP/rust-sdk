/*!
* 文件名: async_agent
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: None
* 描述: SMCP异步Agent实现 / SMCP asynchronous Agent implementation
*/

use crate::{
    auth::AuthProvider,
    config::SmcpAgentConfig,
    error::{Result, SmcpAgentError},
    events::AsyncAgentEventHandler,
    office::{
        backoff_delay, classify_rejoin_error, retry_fits_budget, AttemptCommit, OfficeIntent,
        OfficeMembership, OfficeMembershipState, RejoinVerdict,
    },
    protocol_error::{parse_room_ack, raise_for_error_payload, SmcpProtocolError},
    request_builders::{
        build_get_blob_request, build_get_config_request, build_get_desktop_request,
        build_get_resources_request, build_get_skill_request, build_get_skills_request,
        build_get_tools_request, build_put_blob_request, build_tool_call_cancel,
        build_tool_call_request,
    },
    response::{
        ensure_req_id, parse_get_blob_response, parse_get_config_response,
        parse_get_resources_response,
    },
    skill_consume::{parse_get_skill_response, parse_get_skills_response},
    transport::{NotificationMessage, SocketIoTransport, TransportLifecycle},
};
use smcp::utils::blob::{
    pump_blob, BlobChunkFailure, PumpBlobOptions, PutBlobChunkRequest, PutBlobResult,
    DEFAULT_CHUNK_SIZE,
};
use smcp::{
    events::*, A2CSkillRef, AgentCallData, EnterOfficeReq, GetBlobRet, GetComputerConfigRet,
    GetResourcesRet, GetSkillRet, LeaveOfficeReq, ListRoomReq, PutBlobRet, ReqId, Role, SMCPTool,
    SessionInfo, SMCP_NAMESPACE,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tf_rust_socketio::CloseReason;
use tokio::sync::{mpsc, oneshot, Mutex, RwLock};
use tracing::{debug, error, info, warn};

/// 等待 namespace 连接建立（首个 `Connected` 生命周期事件）的上限。
///
/// [`AsyncSmcpAgent::connect`] 的**后置条件**是「返回时 namespace 已在册并落到成员状态」（与 Computer
/// 侧 `connect_socketio` 等待 Connect 回调同款）：否则调用方紧接着的 `join_office` 会用到尚未绑定的
/// 会话标识（合法 ack 被会话校验丢弃，或在 `connected == false` 时落账成员关系）。
///
/// 超时即视为连接不可用：`connect` **如实返回错误**并回滚本次装载的 transport 槽位与后台 task，绝不
/// 留下「可继续使用的半初始化状态」。传输层 HTTP 握手此时**已完成**（服务端可达且在讲 Socket.IO），
/// 故对「namespace CONNECT 帧」而言 10s 已极宽松——真超时说明对端没有建立 namespace 会话。
const NAMESPACE_READY_TIMEOUT: Duration = Duration::from_secs(10);

/// 锁定成员状态。毒化锁按内值恢复：状态字段全是纯数据，不存在「被毒化即不可信」的跨字段不变量
/// （与 Computer 侧 `lock_office_membership` 同款处理）。
fn lock_office(office: &StdMutex<OfficeMembership>) -> std::sync::MutexGuard<'_, OfficeMembership> {
    office.lock().unwrap_or_else(|error| error.into_inner())
}

/// 本地构造 `4106 Already In Room` 拒绝（#219：Agent 换房须先显式退房）。
///
/// 复用 [`smcp::build_room_rejection_error`] 作为「协议码 → canonical 文案 / `details` 键集」的单一
/// choke point：本地拒绝与服务端拒绝在**线上字节层面**一致，调用方无需按来源分流。`details.office_id`
/// 取会话**当前**所在房（协议 §错误码总表：`4106` 报当前房，**非**被拒的目标房）。
fn already_in_room_error(current_office_id: &str) -> SmcpAgentError {
    let payload = smcp::build_room_rejection_error(
        smcp::RoomRejectionCode::AlreadyInRoom,
        smcp::RoomRejectionContext {
            current_office_id: Some(current_office_id),
            ..Default::default()
        },
    );
    SmcpAgentError::Protocol(Box::new(SmcpProtocolError::from_error_payload(&payload)))
}

/// 已提交连接的后台任务归所有 Agent 克隆共同管理。
struct ConnectionTasks {
    notifications: tokio::task::AbortHandle,
    lifecycle: tokio::task::AbortHandle,
}

impl ConnectionTasks {
    fn abort(&self) {
        self.notifications.abort();
        self.lifecycle.abort();
    }
}

/// 异步SMCP Agent
pub struct AsyncSmcpAgent {
    /// #178：槽位内为 `Arc<SocketIoTransport>`——读锁内仅克隆 Arc（廉价），守卫绝不跨 await 持有；
    /// 所有 I/O（call/emit/drain）一律在无锁的 Arc 上进行。
    transport: Arc<RwLock<Option<Arc<SocketIoTransport>>>>,
    auth_provider: Arc<dyn AuthProvider>,
    event_handler: Option<Arc<dyn AsyncAgentEventHandler>>,
    config: SmcpAgentConfig,
    tools_cache: Arc<RwLock<HashMap<String, Vec<SMCPTool>>>>,
    connection_tasks: Arc<StdMutex<Option<ConnectionTasks>>>,
    /// 克隆体共享连接尝试门：单个 pending_attempt 槽位只能由一个 connect 持有。
    connect_operation: Arc<Mutex<()>>,
    /// #219：Office 成员关系（意图 + 服务端已确认 + 世代）。`std::sync::Mutex` 与 Computer 侧同款——
    /// 守卫只覆盖纯状态提交，**绝不跨 await 持有**。
    office: Arc<StdMutex<OfficeMembership>>,
    /// #219：Office 操作串行化门——显式 `join_office` / `leave_office` 与自动回房逐次尝试互斥，
    /// 使「显式退房」能作为回房循环的抢占点（而非与它竞争）。
    office_operation: Arc<Mutex<()>>,
    /// #219：连接序号发号器。**成员关系属于连接**——只有「已提交连接」的生命周期事件才允许改写成员
    /// 状态；序号是这一归属判据（各连接的 `session_epoch` 各自从 1 起算，不能单独标识连接）。
    office_connection_seq: Arc<AtomicU64>,
}

impl AsyncSmcpAgent {
    /// 创建新的Agent实例
    pub fn new(auth_provider: impl AuthProvider + 'static, config: SmcpAgentConfig) -> Self {
        Self {
            transport: Arc::new(RwLock::new(None)),
            auth_provider: Arc::new(auth_provider),
            event_handler: None,
            config,
            tools_cache: Arc::new(RwLock::new(HashMap::new())),
            connection_tasks: Arc::new(StdMutex::new(None)),
            connect_operation: Arc::new(Mutex::new(())),
            office: Arc::new(StdMutex::new(OfficeMembership::default())),
            office_operation: Arc::new(Mutex::new(())),
            office_connection_seq: Arc::new(AtomicU64::new(0)),
        }
    }

    /// 设置事件处理器
    pub fn with_event_handler(mut self, handler: impl AsyncAgentEventHandler + 'static) -> Self {
        self.event_handler = Some(Arc::new(handler));
        self
    }

    /// #178：解析当前 transport——读锁内仅克隆 `Arc`（廉价），守卫在函数返回前释放。
    /// 所有 I/O（call/emit/drain）一律在返回的无锁 Arc 上进行；锁只保护槽位替换，不保护在途调用。
    /// 由此消除旧实现的死锁环：`get_skill`/`tool_call` 外层读锁（跨响应 await）+ `drain_blob_bytes`
    /// 内层重入读 + `connect()` 排队写（tokio RwLock write-preferring 阻塞新读者）。
    async fn resolve_transport(&self) -> Result<Arc<SocketIoTransport>> {
        let guard = self.transport.read().await;
        guard
            .as_ref()
            .cloned()
            .ok_or_else(|| SmcpAgentError::connection("Not connected".to_string()))
    }

    /// 连接到服务器
    pub async fn connect(&mut self, url: &str) -> Result<()> {
        let _connect = self.connect_operation.lock().await;
        let auth = self.auth_provider.get_connection_auth();
        let headers = self.auth_provider.get_connection_headers();

        // #219：本次连接尝试的序号——成员状态只接受「已提交连接」的事件。
        let connection_id = self.office_connection_seq.fetch_add(1, Ordering::SeqCst) + 1;

        // #219：namespace 生命周期通道（连接建立 / 断开）——回房调度与「是否保留意图」的唯一信息源。
        let (lifecycle_tx, mut lifecycle_rx) = mpsc::unbounded_channel();

        // #219 **事务性连接**：在「新连接完全满足后置条件」之前，既有状态（transport 槽位、两个后台
        // task 句柄、成员状态）**一律不动**。故失败路径（握手失败 / namespace 未建立）只需丢弃本次新建的
        // 资源即可，既有连接原封不动——而不是先把旧连接拆掉、再发现新连接不可用。
        //
        // 认领本次尝试的槽位：必须早于起生命周期 task，否则「Closed 先于首个 Connected 到达」时会因槽位
        // 为空而丢掉该断开记录，随后被 Connected 写成「待提交」（评审 B3''① 残余路径）。
        lock_office(&self.office).begin_attempt(connection_id);

        // 创建transport并获取通知接收器（失败即返回，此时尚未触碰任何既有状态）。
        let (transport, mut notification_rx) =
            SocketIoTransport::connect_with_handlers_and_lifecycle(
                url,
                SMCP_NAMESPACE,
                auth,
                headers,
                Some(lifecycle_tx),
            )
            .await?;
        let transport = Arc::new(transport);

        // #219：生命周期消费 task **直接持有本连接的 transport 句柄**——不依赖共享槽位，故无需提前装载
        // 槽位（「提前装载」正是「回滚误伤既有连接」的成因），也让回房始终发在**发起它的那条连接**上。
        let (namespace_ready_tx, namespace_ready_rx) = oneshot::channel::<u64>();
        let office_lifecycle_task = {
            let agent_clone = self.clone();
            let transport = Arc::clone(&transport);
            tokio::spawn(async move {
                let mut namespace_ready_tx = Some(namespace_ready_tx);
                while let Some(event) = lifecycle_rx.recv().await {
                    match event {
                        TransportLifecycle::Connected { epoch } => {
                            // 本连接尚未被 `connect()` 提交 ⇒ 只**记录**待提交状态并把 epoch 交给
                            // `connect()`（不触碰既有连接的状态，事务性）；已提交连接的重连事件则走常规路径。
                            let already_committed = {
                                let mut membership = lock_office(&agent_clone.office);
                                if membership.committed_connection() == Some(connection_id) {
                                    true
                                } else {
                                    membership.record_attempt_connected(connection_id, epoch);
                                    false
                                }
                            };
                            if already_committed {
                                agent_clone
                                    .on_office_namespace_connected(&transport, connection_id, epoch)
                                    .await;
                            } else if let Some(tx) = namespace_ready_tx.take() {
                                let _ = tx.send(epoch);
                            }
                        }
                        TransportLifecycle::Closed { reason, epoch } => {
                            agent_clone
                                .on_office_namespace_closed(connection_id, reason, epoch)
                                .await;
                        }
                    }
                }
            })
        };

        // 启动通知处理任务
        let event_handler = self.event_handler.clone();
        let agent_clone = self.clone();
        let auto_fetch_tools = self.config.auto_fetch_tools;

        let notification_task = tokio::spawn(async move {
            info!("Notification processing task started");
            while let Some(notification) = notification_rx.recv().await {
                info!("Agent received notification in task: {:?}", notification);
                match notification {
                    NotificationMessage::EnterOffice(data) => {
                        info!("Processing EnterOffice event: {:?}", data);

                        // Python 的自动行为：收到 enter_office 后自动触发 get_tools
                        // Auto behavior: fetch tools when computer enters office
                        if auto_fetch_tools {
                            if let Some(ref computer) = data.computer {
                                debug!("Auto fetching tools for computer: {}", computer);
                                match agent_clone.get_tools(computer).await {
                                    Ok(tools) => {
                                        info!(
                                            "Auto fetched {} tools for computer: {}",
                                            tools.len(),
                                            computer
                                        );
                                        if let Some(ref handler) = event_handler {
                                            let _ = handler
                                                .on_tools_received(computer, tools, &agent_clone)
                                                .await;
                                        }
                                    }
                                    Err(e) => {
                                        warn!(
                                            "Failed to auto fetch tools for computer {}: {}",
                                            computer, e
                                        );
                                    }
                                }
                            }
                        }

                        info!(
                            "Checking event_handler: is_some = {}",
                            event_handler.is_some()
                        );
                        if let Some(ref handler) = event_handler {
                            info!("Calling on_computer_enter_office handler");
                            let _ = handler.on_computer_enter_office(data, &agent_clone).await;
                        } else {
                            info!("No event handler configured for on_computer_enter_office");
                        }
                    }
                    NotificationMessage::LeaveOffice(data) => {
                        if let Some(ref handler) = event_handler {
                            let _ = handler.on_computer_leave_office(data, &agent_clone).await;
                        }
                    }
                    NotificationMessage::UpdateConfig(data) => {
                        // Python 的自动行为：收到 update_config 后自动触发 get_tools
                        // Auto behavior: fetch tools when config is updated
                        if auto_fetch_tools {
                            match agent_clone.get_tools(&data.computer).await {
                                Ok(tools) => {
                                    debug!(
                                        "Auto fetched {} tools on config update for: {}",
                                        tools.len(),
                                        data.computer
                                    );
                                    if let Some(ref handler) = event_handler {
                                        let _ = handler
                                            .on_tools_received(&data.computer, tools, &agent_clone)
                                            .await;
                                    }
                                }
                                Err(e) => {
                                    warn!(
                                        "Failed to auto fetch tools on config update for {}: {}",
                                        data.computer, e
                                    );
                                }
                            }
                        }

                        if let Some(ref handler) = event_handler {
                            let _ = handler.on_computer_update_config(data, &agent_clone).await;
                        }
                    }
                    NotificationMessage::UpdateToolList(data) => {
                        // #106 三段式（预清 → 回拉 → 重加，对标 python#127）：先派发预清回调
                        // on_computer_update_tool_list，让加法式下游消费方清空该 computer 的旧工具视图，再自动
                        // 重拉 get_tools → on_tools_received 重加——使运行期**移除 / 同名换 schema**正确生效
                        // （否则纯回拉+加法式 merge 会残留旧定义）。预清失败独立捕获、不阻断后续回拉。
                        //
                        // 预清与回拉**成对**，故一并 gate 在 auto_fetch_tools 内：若消费方显式关闭 auto_fetch，
                        // 则既不预清也不回拉（由消费方自管），避免「清空却因不回拉而永不重填 → 工具凭空消失」。
                        if auto_fetch_tools {
                            if let Some(ref handler) = event_handler {
                                let _ = handler
                                    .on_computer_update_tool_list(data.clone(), &agent_clone)
                                    .await;
                            }
                            match agent_clone.get_tools(&data.computer).await {
                                Ok(tools) => {
                                    debug!(
                                        "Auto fetched {} tools on tool list update for: {}",
                                        tools.len(),
                                        data.computer
                                    );
                                    if let Some(ref handler) = event_handler {
                                        let _ = handler
                                            .on_tools_received(&data.computer, tools, &agent_clone)
                                            .await;
                                    }
                                }
                                Err(e) => {
                                    warn!(
                                        "Failed to auto fetch tools on tool list update for {}: {}",
                                        data.computer, e
                                    );
                                }
                            }
                        }
                    }
                    NotificationMessage::UpdateDesktop(computer) => {
                        // Python 的自动行为：收到 update_desktop 后自动触发 get_desktop
                        if let Ok(desktops) = agent_clone.get_desktop(&computer, None, None).await {
                            if let Some(ref handler) = event_handler {
                                let _ = handler
                                    .on_desktop_updated(&computer, desktops, &agent_clone)
                                    .await;
                            }
                        }
                    }
                    NotificationMessage::UpdateSkills(computer) => {
                        // v0.2.1 自动行为（对标 Python `_on_skills_updated`）：收到 notify:update_skills
                        // 后自动重拉 get_skills，再派发 on_skills_received hook。
                        // 错误隔离：重拉失败仅告警；hook 抛错独立捕获、不污染通知循环（对标 Python 双层 try）。
                        match agent_clone.get_skills(&computer).await {
                            Ok(skills) => {
                                info!(
                                    "Skills refreshed from computer {}: count={}",
                                    computer,
                                    skills.len()
                                );
                                if let Some(ref handler) = event_handler {
                                    if let Err(e) = handler
                                        .on_skills_received(&computer, skills, &agent_clone)
                                        .await
                                    {
                                        error!(
                                            "on_skills_received hook raised for computer {}: {}",
                                            computer, e
                                        );
                                    }
                                }
                            }
                            Err(e) => {
                                warn!("Failed to refresh skills for computer {}: {}", computer, e);
                            }
                        }
                    }
                }
            }
        });

        // 有界等待 namespace 会话建立（见 NAMESPACE_READY_TIMEOUT 的说明）。此刻生命周期 task 只把
        // epoch 交回（**不触碰**共享状态），故失败路径无需任何回滚：既有连接与成员状态自始至终未变。
        let Ok(Ok(epoch)) = tokio::time::timeout(NAMESPACE_READY_TIMEOUT, namespace_ready_rx).await
        else {
            // 后置条件未达成 ⇒ 丢弃**本次**新建的资源并如实失败。既有状态（槽位 / 两个 task 句柄 /
            // 成员状态）自始至终未被触碰，故不构成「破坏既有连接」。
            office_lifecycle_task.abort();
            notification_task.abort();
            Self::close_transport(&transport).await;
            warn!(
                "Namespace connect was not observed within {:?}; connection rolled back",
                NAMESPACE_READY_TIMEOUT
            );
            return Err(SmcpAgentError::connection(format!(
                "namespace connect was not observed within {:?}",
                NAMESPACE_READY_TIMEOUT
            )));
        };

        // 提交点：**一次加锁**完成「裁决（提交前是否已断开）+ 连接归属 + 会话绑定 + 世代推进」。这是与
        // 生命周期 task 唯一的裁决交点，故不存在「检查之后、提交之前」的窗口——已到达的 Close 要么在此
        // 被采信为「提交前已断开」（拒绝提交），要么发生在提交之后（按已提交连接的普通断开处理，状态机
        // 据此重连 / 回房）。详见 `OfficeMembership::commit_attempt`。
        // 发布与显式 join/leave 共用操作门。先取得所有异步锁，再无 await 地提交身份与槽位，
        // 房间请求因此不可能捕获新身份却发送到旧 transport。连接握手不占房间操作门。
        let operation = self.office_operation.clone().lock_owned().await;
        let mut transport_slot = self.transport.write().await;
        let commit = lock_office(&self.office).commit_attempt(connection_id);
        let replay = match commit {
            AttemptCommit::ClosedBeforeCommit => {
                // 提交前已断开（或状态不可信）⇒ 丢弃本次新建资源并如实失败，绝不把死连接报成「已连接」。
                office_lifecycle_task.abort();
                notification_task.abort();
                drop(transport_slot);
                drop(operation);
                Self::close_transport(&transport).await;
                warn!(
                    "Namespace closed before the connection was committed; connection rolled back"
                );
                return Err(SmcpAgentError::connection(
                    "namespace closed before the connection was committed".to_string(),
                ));
            }
            AttemptCommit::Committed {
                previous_rejoin,
                replay,
            } => {
                if let Some(task) = previous_rejoin {
                    task.abort();
                }
                replay
            }
        };

        let previous_transport = transport_slot.replace(Arc::clone(&transport));
        let previous_tasks = self
            .connection_tasks
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .replace(ConnectionTasks {
                notifications: notification_task.abort_handle(),
                lifecycle: office_lifecycle_task.abort_handle(),
            });
        drop(transport_slot);
        // 提交后的清理和恢复归独立任务负责：调用方被取消（包括在旧通知回调中连接替换，
        // 旧任务随之被 abort）也不能中断已提交连接的收尾。操作门随任务移交，直到旧连接关闭。
        let agent = self.clone();
        tokio::spawn(async move {
            if let Some(tasks) = previous_tasks {
                tasks.abort();
            }
            if let Some(previous) = previous_transport {
                Self::close_transport(&previous).await;
            }
            if let Some((generation, intent)) = replay {
                info!(
                    "Namespace connected (epoch {}); replaying office join for {}",
                    epoch, intent.office_id
                );
                agent.spawn_office_rejoin(transport, connection_id, generation, intent);
            }
            drop(operation);
        })
        .await
        .map_err(|error| {
            SmcpAgentError::internal(format!("connection finalization failed: {error}"))
        })?;

        info!("Connected to SMCP server at {}", url);
        Ok(())
    }

    /// 清理连接时保留原始连接结果，关闭失败单独记录；不持有状态锁或 transport 槽位锁。
    async fn close_transport(transport: &SocketIoTransport) {
        if let Err(error) = transport.close().await {
            warn!("Failed to close retired Socket.IO connection: {error}");
        }
    }

    /// 加入办公室（**等 ack**，取得服务端裁决）
    ///
    /// 协议 v0.5.0：`server:join_office` 成功回空 ack、失败回 flat `ErrorPayload`
    /// （`400` / `403` / `4101` / `4105` / `4106`）。历史实现是无 ack 的 `emit` + 直接记
    /// `"Joined office"`——被 `4101` 拒绝时日志写「入房成功」并返回 `Ok(())`，此后所有 `client:*`
    /// 调用都从一个**从未进入**的房发起，拿回调用方无法解释的 404（#226 P0-1，本 SDK 主要消费者的
    /// 契约缺席）。
    ///
    /// 故此处改为 `call`：拿到 ack 后由 [`parse_room_ack`] 裁决——空 ack ⇒ `Ok(())`；
    /// 含 `code` 的 flat ErrorPayload ⇒ [`SmcpAgentError::Protocol`]（带 code / message / details，
    /// 调用方可按码分流：`4101` / `4105` 是传输层重连后的瞬态冲突，`4106` / `400` 永久不可重试）。
    /// 形状不认识同样判失败（宁严勿宽），**绝不**把未获裁决读成成功。
    ///
    /// # 兼作意图声明点（#219）
    ///
    /// 成功入房后记住 `(office_id, agent_name)`——Socket.IO 房间成员关系属于**会话**，传输层自动重连
    /// 后新 SID 不在房里；SDK 必须据此在重连后重放 `server:join_office`，否则会「看起来还在房间、
    /// 实际收不到任何房间流量」。注意 `agent_name` 只在本次调用实参里（`auth_provider` 只给
    /// `office_id`），故必须随意图一起记住。
    ///
    /// # 换房须先显式退房（协议 room-model §Agent 加入规则 1）
    ///
    /// Agent 已在**其它**房又请求入新房时，本方法**本地**即以 canonical `4106 Already In Room`
    /// 快速失败（`details.office_id` = **当前**所在房），不发请求：Computer 的自动换房规则**不适用**
    /// 于 Agent，先显式 [`Self::leave_office`] 再入新房才是协议规定的两步语义。本地判定同时避免在
    /// 服务端裁决前覆写本地意图。
    ///
    /// Join the office, **await the server's verdict**, and record the join intent so a later
    /// transport reconnect can replay it.
    pub async fn join_office(&self, agent_name: &str) -> Result<()> {
        let office_id = self.auth_provider.get_agent_config().office_id.clone();
        let intent = OfficeIntent {
            office_id: office_id.clone(),
            agent_name: agent_name.to_string(),
        };

        // 与自动回房逐次尝试互斥：保证「意图校验 → 入房 → 落账」是一个不可交错的单元。
        let _operation = self.office_operation.lock().await;

        if let Some(current) = lock_office(&self.office).conflicting_office(&office_id) {
            return Err(already_in_room_error(&current));
        }

        // 发起前捕获会话标识：ack 只在**同一会话**仍在册时才算数（见 `OfficeSessionToken`）。
        let session = lock_office(&self.office).session_token();

        let req = EnterOfficeReq {
            role: Role::Agent,
            name: agent_name.to_string(),
            office_id,
        };
        self.call_room_event(SERVER_JOIN_OFFICE, &req, self.config.default_timeout)
            .await?;

        let (committed, pending) = lock_office(&self.office).commit_join(session, intent);
        if let Some(task) = pending {
            task.abort();
        }
        if !committed {
            // ACK 属**已死会话**：传输层回调与 ack 分发是各自独立的 task（无顺序契约），故等待 ack 期间
            // 可能夹进 Close / Connect / 显式退房。此时「入房成功」陈述的是已失效会话的事实，落账会
            // 造成「namespace 已断开却报 JoinedOffice」（违反「不静默假装在线」）。既不落账已确认，
            // 也不落账意图（否则会把期间被显式退房清掉的意图复活），如实报错、由调用方自行重试。
            warn!(
                "Discarding office join verdict for {}: the namespace session was replaced while awaiting the ack",
                req.office_id
            );
            return Err(SmcpAgentError::connection(
                "office join ack arrived after the namespace session was replaced; verdict discarded"
                    .to_string(),
            ));
        }

        info!("Joined office: {}", req.office_id);
        Ok(())
    }

    /// 离开办公室（**等 ack**，取得服务端裁决）
    ///
    /// 协议 v0.5.0：`server:leave_office` 在有无房时均为**幂等成功**（空 ack）；失败只可能是
    /// 载荷畸形（`400`）。等 ack 的意义不在错误码，而在**时序**：退房是「先退旧房再入新房」的显式
    /// 两步语义，客户端必须确知旧房已提交（否则紧接着的 join 会撞上 `4106`）。协议自身也要求
    /// 客户端**不**抢跑——只投递意图、以 ack 为终态。
    ///
    /// Await the server's verdict so a follow-up join cannot race the leave's commit.
    pub async fn leave_office(&self) -> Result<()> {
        let office_id = self.auth_provider.get_agent_config().office_id.clone();

        // ⚠️ 清意图**必须在操作门之内**：若在取门前清（旧写法），并发的 `join_office` 可抢先取门并成功
        // 落账，随后本方法再把 leave 发到服务端 ⇒ 服务端已退房而本地仍报 `JoinedOffice`（且下次重连
        // 还会自动重回）——本地状态与服务端事实相反，正是「不静默假装在线」要杜绝的形态。置于门内后，
        // leave 与 join 全序化：谁后进临界区谁的最后一次服务端操作与本地状态一致。
        let _operation = self.office_operation.lock().await;

        // 显式退房是调用方对入房意图的**权威撤销**：清意图 + 作废在途回房，再发请求。清在请求之前
        // 不可颠倒——若请求在传输层失败而意图仍在，下一次自动重连会把用户刚主动退出的房**重新加入**
        // （与调用方刚表达的意图相反）。清空后即使请求失败，SDK 也只是「未宣称在房」（如实报错）。
        let pending = lock_office(&self.office).clear_intent();
        if let Some(task) = pending {
            task.abort();
        }

        let req = LeaveOfficeReq {
            office_id: office_id.clone(),
        };
        self.call_room_event(SERVER_LEAVE_OFFICE, &req, self.config.default_timeout)
            .await?;

        info!("Left office: {}", office_id);
        Ok(())
    }

    /// 当前 Office 成员关系（只读快照）。
    ///
    /// 返回值区分 `Connected`（已连接但**不在任何房**，含回房失败后的回退态）与
    /// `JoinedOffice`——SDK 绝不静默假装仍在房（#219）。
    pub fn office_membership(&self) -> OfficeMembershipState {
        lock_office(&self.office).state()
    }

    /// 服务端**已确认**的房间号；不在房（含回房失败后的回退态）时为 `None`。
    ///
    /// 注意与 `auth_provider` 配置里的 `office_id` 区分：后者是**意图**，前者是服务端裁决后的**事实**。
    pub fn confirmed_office_id(&self) -> Option<String> {
        lock_office(&self.office).confirmed_office_id()
    }

    /// 发送房间事件（`server:join_office` / `server:leave_office`）并等 ack，按协议三分歧裁决。
    ///
    /// 空 ack ⇒ `Ok(())`；含 `code` 的 flat ErrorPayload ⇒ [`SmcpAgentError::Protocol`]；形状不认识
    /// ⇒ 同样判失败（宁严勿宽）。见 [`parse_room_ack`]。
    ///
    /// 走**当前槽位**的连接（显式 `join_office` / `leave_office` 即此语义）。
    async fn call_room_event<T: serde::Serialize>(
        &self,
        event: &str,
        payload: &T,
        timeout_secs: u64,
    ) -> Result<()> {
        let transport = self.resolve_transport().await?;
        Self::call_room_event_on(&transport, event, payload, timeout_secs).await
    }

    /// 在**指定**连接上发送房间事件并等 ack。
    ///
    /// 自动回房走本入口并显式传入「派生该回房的连接」：不依赖共享槽位，既保证回房发在正确的连接上，
    /// 也让 [`Self::connect`] 无需提前装载槽位即可被做成事务（见其注释）。
    ///
    /// ⚠️ 外层**自带**真超时：`tf-rust-socketio` 的 ack 超时只在「ack 到达」时才被判过期（其
    /// `outstanding_acks` 仅在收到对应 ack 时才移除条目），故对**根本不 ack** 的对端，
    /// [`SocketIoTransport::call`] 的 `timeout_secs` 形同虚设、会一直挂住。房间事件是回房循环的
    /// 承重路径——「有界等待」是本特性的硬要求（#219 裁决），故此处补一层真 deadline。
    async fn call_room_event_on<T: serde::Serialize>(
        transport: &SocketIoTransport,
        event: &str,
        payload: &T,
        timeout_secs: u64,
    ) -> Result<()> {
        let data = serde_json::to_value(payload)?;
        let response = tokio::time::timeout(
            Duration::from_secs(timeout_secs),
            transport.call(event, data, timeout_secs),
        )
        .await
        .map_err(|_| {
            warn!(
                "Room event {} received no ack within {}s",
                event, timeout_secs
            );
            SmcpAgentError::Timeout
        })??;
        parse_room_ack(&response)?;
        Ok(())
    }

    /// namespace 连接建立（含自动重连后的新会话）：绑定会话，并按待重放的入房意图派生回房 task。
    ///
    /// 只采信**已提交连接**（`connection_id` 与成员状态登记的连接一致）的事件：未提交连接的 Connected
    /// 由 `connect()` 在提交点亲自绑定，旧连接的迟到事件则被 `None` 丢弃。
    async fn on_office_namespace_connected(
        &self,
        transport: &Arc<SocketIoTransport>,
        connection_id: u64,
        epoch: u64,
    ) {
        let Some((previous, replay)) =
            lock_office(&self.office).bind_connected(connection_id, epoch)
        else {
            debug!(
                "Ignoring namespace connect of uncommitted/superseded connection {}",
                connection_id
            );
            return;
        };
        if let Some(task) = previous {
            task.abort();
        }

        let Some((generation, intent)) = replay else {
            debug!(
                "Namespace connected (epoch {}); no office intent to restore",
                epoch
            );
            return;
        };
        info!(
            "Namespace connected (epoch {}); replaying office join for {}",
            epoch, intent.office_id
        );
        self.spawn_office_rejoin(Arc::clone(transport), connection_id, generation, intent);
    }

    /// namespace 断开：按**关闭原因**决定意图去留，并作废在途回房。
    async fn on_office_namespace_closed(
        &self,
        connection_id: u64,
        reason: CloseReason,
        epoch: u64,
    ) {
        // 只有「传输层断线且底层会自动重连」（`transport close`）才保留意图；服务端踢出 / 手工断开
        // 一律清空——否则会把用户主动退出、或被服务端踢掉的房在下次重连时**自动加回去**。
        let retain_intent = matches!(reason, CloseReason::TransportClose);
        // **一次加锁**完成两件事，二者不可分割：① 记录「该尝试在提交前已断开」（供提交点裁决）；
        // ② 若该连接**已提交**，按常规断开处理。分开做会留下「检查-使用」窗口。
        let applied = {
            let mut membership = lock_office(&self.office);
            membership.record_attempt_closed(connection_id);
            membership.bind_closed(connection_id, epoch, retain_intent)
        };
        let Some((applied, pending)) = applied else {
            debug!(
                "Ignoring namespace close of uncommitted/superseded connection {}",
                connection_id
            );
            return;
        };
        if let Some(task) = pending {
            task.abort();
        }

        match (applied, retain_intent) {
            (false, _) => debug!(
                "Ignoring namespace close of superseded session (epoch {})",
                epoch
            ),
            (true, true) => info!(
                "Namespace transport closed (epoch {}); office intent retained for rejoin",
                epoch
            ),
            (true, false) => info!(
                "Namespace closed ({:?}); office intent cleared (no automatic rejoin)",
                reason
            ),
        }
    }

    /// 派生 generation-bound 回房 task 并登记其 abort 句柄；登记失败（世代/意图已不新鲜）即刻 abort。
    fn spawn_office_rejoin(
        &self,
        transport: Arc<SocketIoTransport>,
        connection_id: u64,
        generation: u64,
        intent: OfficeIntent,
    ) {
        let agent = self.clone();
        let task_intent = intent.clone();
        let task = tokio::spawn(async move {
            agent
                .recover_office(&transport, connection_id, generation, task_intent)
                .await;
        });
        let handle = task.abort_handle();
        if !lock_office(&self.office).set_rejoin_task(connection_id, generation, &intent, handle) {
            task.abort();
        }
    }

    /// 重连恢复路径：在**有界预算**内重放 `server:join_office`。
    ///
    /// 与显式 [`Self::join_office`] 的关键差异是**允许对瞬态冲突退避重试**：静默断线后服务端仍可能
    /// 持有本客户端的旧会话（回收时刻由传输层心跳决定，socket.io 默认最长 45s），此时重放会撞上
    /// `4101` / `4105` 被拒。协议 error-handling.md §建议的重试策略要求客户端在**恢复路径**上做有界
    /// 退避重试（单次尝试为下限、预算可配）。
    ///
    /// **为什么不是事件驱动**：协议明确禁止服务端收编 / 驱逐旧会话（room-model.md §静默断线与
    /// 会话回收——服务端仅凭 `(role, name)` 无法区分僵尸会话与真实同名客户端），故不存在
    /// 「旧会话已回收」事件可供等待；唯一机器可判的信号就是这两个拒绝码本身。
    ///
    /// 结果只在**世代仍新鲜**时落账；失败一律清空成员状态（回退到 `Connected`）并报错，绝不静默假装
    /// 在线。
    async fn recover_office(
        &self,
        transport: &Arc<SocketIoTransport>,
        connection_id: u64,
        generation: u64,
        intent: OfficeIntent,
    ) {
        let budget = Duration::from_secs(self.config.office_rejoin_budget_secs);
        let started = tokio::time::Instant::now();
        let mut attempt: u32 = 0;

        loop {
            attempt += 1;
            // 操作门按**单次尝试**粒度持有：使显式 join / leave 能在两次尝试之间抢占回房循环，
            // 而不与之竞争（若整段循环都持门，用户退房会被最长一个预算挡住）。
            let verdict = {
                let _operation = self.office_operation.lock().await;
                if !lock_office(&self.office).is_current(connection_id, generation, &intent) {
                    return;
                }
                self.replay_join(transport, &intent).await
            };

            // 每个 await 之后复核新鲜度（Close / 显式操作可能已接管）；落账侧还有同一守卫的二次检查，
            // 覆盖「复核之后、落账之前」的窗口。
            if !lock_office(&self.office).is_current(connection_id, generation, &intent) {
                return;
            }

            match verdict {
                Ok(()) => {
                    if lock_office(&self.office).commit_rejoin(connection_id, generation, &intent) {
                        info!(
                            "Automatically rejoined office {} after reconnect (attempt {})",
                            intent.office_id, attempt
                        );
                    }
                    return;
                }
                Err(error) => match classify_rejoin_error(&error) {
                    RejoinVerdict::TransientConflict => {
                        let delay = backoff_delay(attempt);
                        if !retry_fits_budget(started.elapsed(), delay, budget) {
                            self.abandon_office_rejoin(
                                connection_id,
                                generation,
                                &intent,
                                format!(
                                    "still rejected by transient conflict after {attempt} attempt(s) \
                                     within the {}s rejoin budget: {error}",
                                    self.config.office_rejoin_budget_secs
                                ),
                            )
                            .await;
                            return;
                        }
                        warn!(
                            "Office rejoin hit a transient conflict (attempt {attempt}): {error}; \
                             retrying in {delay:?}"
                        );
                        tokio::time::sleep(delay).await;
                    }
                    RejoinVerdict::Permanent => {
                        self.abandon_office_rejoin(
                            connection_id,
                            generation,
                            &intent,
                            format!("office rejoin rejected: {error}"),
                        )
                        .await;
                        return;
                    }
                },
            }
        }
    }

    /// 重放一次 `server:join_office`（等 ack，用
    /// [`SmcpAgentConfig::office_rejoin_timeout`](crate::config::SmcpAgentConfig::office_rejoin_timeout)
    /// 作单次有界等待）。
    async fn replay_join(
        &self,
        transport: &Arc<SocketIoTransport>,
        intent: &OfficeIntent,
    ) -> Result<()> {
        let req = EnterOfficeReq {
            role: Role::Agent,
            name: intent.agent_name.clone(),
            office_id: intent.office_id.clone(),
        };
        Self::call_room_event_on(
            transport,
            SERVER_JOIN_OFFICE,
            &req,
            self.config.office_rejoin_timeout,
        )
        .await
    }

    /// 回房放弃：清空成员状态（如实回退到 `Connected`）+ error 日志 + 派发
    /// [`AsyncAgentEventHandler::on_office_membership_lost`]（默认 no-op）。
    async fn abandon_office_rejoin(
        &self,
        connection_id: u64,
        generation: u64,
        intent: &OfficeIntent,
        reason: String,
    ) {
        if !lock_office(&self.office).abandon_rejoin(connection_id, generation, intent) {
            // 已被更新的操作 / 断连接管：既不改状态也不再派发（避免误报「失去」）。
            return;
        }
        error!(
            "Office membership lost for {} (state falls back to Connected): {}",
            intent.office_id, reason
        );
        if let Some(handler) = &self.event_handler {
            if let Err(hook_error) = handler
                .on_office_membership_lost(&intent.office_id, &reason, self)
                .await
            {
                error!(
                    "on_office_membership_lost hook raised for office {}: {}",
                    intent.office_id, hook_error
                );
            }
        }
    }

    /// 获取指定Computer的工具列表
    pub async fn get_tools(&self, computer: &str) -> Result<Vec<SMCPTool>> {
        let agent_config = self.auth_provider.get_agent_config();
        let req = build_get_tools_request(&agent_config.agent, computer);
        let req_id = req.base.req_id.clone();

        debug!("Getting tools from computer: {}", computer);

        let transport = self.resolve_transport().await?;
        let data = serde_json::to_value(&req)?;
        let response = transport
            .call(CLIENT_GET_TOOLS, data, self.config.get_timeout)
            .await?;

        // flat ErrorPayload → 协议错误（#47↔#34：对端未命中/能力错误经 ack 回 flat error）
        raise_for_error_payload(&response)?;

        // 验证 req_id（全 crate 单点收敛，见 response::ensure_req_id）
        ensure_req_id(&response, req_id.as_str())?;

        let tools: Vec<SMCPTool> =
            serde_json::from_value(response.get("tools").cloned().unwrap_or_default())?;

        // 更新缓存
        self.tools_cache
            .write()
            .await
            .insert(computer.to_string(), tools.clone());

        info!("Received {} tools from computer: {}", tools.len(), computer);
        Ok(tools)
    }

    /// 获取指定 Computer 的配置（`client:get_config`，#136 / D#23 B-1）/ Get a Computer's config。
    ///
    /// 返回 [`GetComputerConfigRet`]：`servers` = Computer 的**运行期活跃** MCP Server 集（字典
    /// key = **`bundle_id`**，server 唯一身份；F2/PROTO-2：读运行期权威配置集、非构造期快照），`inputs`
    /// = input 定义列表。**唯一途径**取纯资源型 server（无工具、只出 `window://`）的 `bundle_id`，供
    /// 后续 [`get_resources`](Self::get_resources)（其 `mcp_server` 参即此处 `servers` 的 key）。
    ///
    /// flat ErrorPayload 经 ack 透传为协议错误、不吞错。`GetComputerConfigRet` 无 `req_id` 字段 ⇒
    /// 不做回显校验（见 `response::parse_get_config_response`）。对标 Python
    /// `a2c_smcp/agent/client.py::get_config`（SDK 方法名各按语言惯例，D#23 R5：无需跨端对拍）。
    pub async fn get_computer_config(&self, computer: &str) -> Result<GetComputerConfigRet> {
        let agent_config = self.auth_provider.get_agent_config();
        let req = build_get_config_request(&agent_config.agent, computer);

        debug!("Getting config from computer: {}", computer);

        let transport = self.resolve_transport().await?;
        let data = serde_json::to_value(&req)?;
        let response = transport
            .call(CLIENT_GET_CONFIG, data, self.config.get_timeout)
            .await?;

        // flat ErrorPayload 透传 + 整包解析单点收敛于纯函数（无 req_id 回显校验，见 parse_get_config_response）。
        let ret = parse_get_config_response(&response)?;
        info!(
            "Received config from computer {} ({} servers)",
            computer,
            ret.servers.as_object().map(|m| m.len()).unwrap_or(0)
        );
        Ok(ret)
    }

    /// 获取指定Computer的桌面信息
    pub async fn get_desktop(
        &self,
        computer: &str,
        size: Option<i32>,
        window: Option<String>,
    ) -> Result<Vec<String>> {
        let agent_config = self.auth_provider.get_agent_config();
        let req = build_get_desktop_request(&agent_config.agent, computer, size, window.as_deref());
        let req_id = req.base.req_id.clone();

        debug!("Getting desktop from computer: {}", computer);

        let transport = self.resolve_transport().await?;
        let data = serde_json::to_value(&req)?;
        let response = transport
            .call(CLIENT_GET_DESKTOP, data, self.config.get_timeout)
            .await?;

        // flat ErrorPayload → 协议错误（#47↔#34）
        raise_for_error_payload(&response)?;

        // 验证 req_id（全 crate 单点收敛，见 response::ensure_req_id）
        ensure_req_id(&response, req_id.as_str())?;

        let desktops: Vec<String> =
            serde_json::from_value(response.get("desktops").cloned().unwrap_or_default())?;

        info!(
            "Received {} desktops from computer: {}",
            desktops.len(),
            computer
        );
        Ok(desktops)
    }

    /// 获取指定 Computer 上某 MCP Server 的资源列表（v0.2.0）/ Get a MCP Server's resource list。
    ///
    /// `mcp_server`：目标 MCP Server 的 **bundle_id**（**非** display 名；协议 0.3.0 §身份正交性 #18）。
    /// Computer 按 bundle_id 直查、不经 name 解析 ⇒ 传 display 名必得 `4014`。bundle_id 取自
    /// `client:get_config` 响应的 `servers` map 键（typed 消费方法由 #136 补齐）。
    ///
    /// 透明转发 MCP `resources/list`：SDK **不**自动翻页——`cursor` 由调用方控制，首次传 `None`，
    /// 响应含 `next_cursor` 时由调用方决定是否带该 cursor 继续请求（协议指南 §5.3 #3）。flat
    /// ErrorPayload（`4014` MCP Server 未注册 / `4015` 未声明 `resources` 能力）经 ack 透传为协议
    /// 错误，不吞错。对标 Python `a2c_smcp/agent/client.py::get_resources`。
    pub async fn get_resources(
        &self,
        computer: &str,
        mcp_server: &str,
        cursor: Option<String>,
    ) -> Result<GetResourcesRet> {
        let agent_config = self.auth_provider.get_agent_config();
        let req = build_get_resources_request(
            &agent_config.agent,
            computer,
            mcp_server,
            cursor.as_deref(),
        );
        let req_id = req.base.req_id.clone();

        debug!(
            "Getting resources from computer {}, mcp_server={}, cursor={:?}",
            computer, mcp_server, cursor
        );

        let transport = self.resolve_transport().await?;
        let data = serde_json::to_value(&req)?;
        let response = transport
            .call(CLIENT_GET_RESOURCES, data, self.config.get_timeout)
            .await?;

        // 响应编排（flat ErrorPayload 4014/4015 透传 + req_id 校验 + 整页解析）单点收敛于纯函数，
        // 与 get_skills 同构、可独立单测（见 response::parse_get_resources_response）。
        let ret = parse_get_resources_response(&response, req_id.as_str())?;
        info!(
            "Received {} resources from computer: {} (next_cursor={:?})",
            ret.resources.len(),
            computer,
            ret.next_cursor
        );
        Ok(ret)
    }

    /// 获取指定 Computer 的 SKILL 清单（v0.2.1）/ Get a Computer's SKILL inventory。
    ///
    /// 发起 `client:get_skills`，返回轻量 [`A2CSkillRef`] 列表（**不含** SKILL.md body；body 经
    /// [`Self::get_skill`] 按需拉取）。flat ErrorPayload（如 `4014`）经 ack 透传为协议错误。
    /// 对标 Python `a2c_smcp/agent/client.py::get_skills`。
    pub async fn get_skills(&self, computer: &str) -> Result<Vec<A2CSkillRef>> {
        let agent_config = self.auth_provider.get_agent_config();
        let req = build_get_skills_request(&agent_config.agent, computer);
        let req_id = req.base.req_id.clone();

        debug!("Getting skills from computer: {}", computer);

        let transport = self.resolve_transport().await?;
        let data = serde_json::to_value(&req)?;
        let response = transport
            .call(CLIENT_GET_SKILLS, data, self.config.get_timeout)
            .await?;

        let skills = parse_get_skills_response(&response, req_id.as_str())?;
        info!(
            "Received {} skills from computer: {}",
            skills.len(),
            computer
        );
        Ok(skills)
    }

    /// 获取 SKILL 包内单个资源（v0.2.1）/ Get a single in-package SKILL resource。
    ///
    /// 发起 `client:get_skill`；`rel_path` 缺省由 Computer 解析为包根 `SKILL.md` 入口，否则 MUST
    /// 相对、无 `..`、无绝对路径（沙箱在 Computer 端强制）。返回的 [`GetSkillRet`] 中 `body` 与
    /// `blob_handle` **恰一存在**（经 [`GetSkillRet::resource`] 校验访问）：
    /// - 文本且可内联 → `body` 直接可读（[`smcp::SkillResource::Inline`]）；
    /// - 二进制或过大文本 → `blob_handle` **原样返回**，由调用方经 `client:get_blob` 自取字节。
    ///
    /// 文本 MIME 的 `blob_handle` 自动经 `drain_blob` 拉回并 UTF-8 解码回填 `body`，对调用方透明；
    /// 文本性判定经单一权威 `mime_is_textual`（协议 §6.4(2)）。对标 Python
    /// `a2c_smcp/agent/client.py::get_skill`。
    pub async fn get_skill(
        &self,
        computer: &str,
        name: &str,
        rel_path: Option<&str>,
    ) -> Result<GetSkillRet> {
        let agent_config = self.auth_provider.get_agent_config();
        let req = build_get_skill_request(&agent_config.agent, computer, name, rel_path);
        let req_id = req.base.req_id.clone();

        debug!(
            "Getting skill {:?} rel_path={:?} from computer: {}",
            name, rel_path, computer
        );

        let transport = self.resolve_transport().await?;
        let data = serde_json::to_value(&req)?;
        let response = transport
            .call(CLIENT_GET_SKILL, data, self.config.get_timeout)
            .await?;

        let mut ret = parse_get_skill_response(&response, req_id.as_str())?;

        // AGT-03 #38：文本 MIME 的 `blob_handle` 自动 drain 回填 `body`（Python parity）。
        // Computer 对「文本但超内联预算」走句柄；Agent 拉回并 UTF-8 解码即得 body，对调用方透明。
        // 二进制句柄（image/png 等）**保持原样**，由调用方按二进制经 get_blob/drain 自取。
        if ret.body.is_none() {
            if let Some(handle) = ret.blob_handle.clone() {
                if mime_is_textual(ret.mime_type.as_deref()) {
                    match self.drain_blob_bytes(computer, &handle).await {
                        Ok((bytes, _mime)) => match String::from_utf8(bytes) {
                            Ok(text) => {
                                ret.body = Some(text);
                                ret.blob_handle = None;
                            }
                            // 非 UTF-8：保留 blob_handle 让调用方按二进制处理（保守回退，对齐 Python）。
                            Err(_) => debug!(
                                "get_skill {:?} textual mime but body not UTF-8; keeping blob_handle",
                                name
                            ),
                        },
                        Err(e) => warn!(
                            "get_skill blob backfill failed for handle={}: {}; keeping blob_handle",
                            handle, e
                        ),
                    }
                }
            }
        }

        info!("Received skill {:?} from computer: {}", name, computer);
        Ok(ret)
    }

    /// 通用二进制单块拉取（v0.2.1，AGT-03 #38）/ Pull one binary chunk via `client:get_blob`。
    ///
    /// 直接对应协议 `client:get_blob`：按 `chunk_offset`（资源字节绝对偏移，缺省 0）+ `max_chunk_bytes`
    /// （客户建议单块上限，Computer clamp 至 `BlobThresholds.chunk_max_bytes`）取一块，返回 [`GetBlobRet`]
    /// （含 `total_size`/`sha256`/`eof`/base64 `blob`）。flat ErrorPayload（`4018` invalid_handle/
    /// forbidden/gone/range）经 ack 透传为协议错误。多块重组/校验/重试请用 `Self::drain_blob_bytes`。
    /// 对标 Python `a2c_smcp/agent/client.py::get_blob`。
    pub async fn get_blob(
        &self,
        computer: &str,
        blob_handle: &str,
        chunk_offset: Option<u64>,
        max_chunk_bytes: Option<u64>,
    ) -> Result<GetBlobRet> {
        let agent_config = self.auth_provider.get_agent_config();
        let req = build_get_blob_request(
            &agent_config.agent,
            computer,
            blob_handle,
            chunk_offset,
            max_chunk_bytes,
        );
        let req_id = req.base.req_id.clone();

        debug!(
            "Getting blob chunk from computer {}, handle={}, offset={:?}",
            computer, blob_handle, chunk_offset
        );

        let transport = self.resolve_transport().await?;
        let data = serde_json::to_value(&req)?;
        let response = transport
            .call(CLIENT_GET_BLOB, data, self.config.get_timeout)
            .await?;

        let ret = parse_get_blob_response(&response, req_id.as_str())?;
        Ok(ret)
    }

    /// 经 `client:get_blob` 拉取并重组某句柄的全量字节（AGT-03 #38）/ drain & reassemble all bytes。
    ///
    /// 复用 UTIL-02 [`smcp::utils::blob::drain_blob`]（串行多块拉取 + 跨块源漂移重读 + 全量 `sha256`
    /// 自证 + 4018 处置），注入「以本 Agent transport 发 `client:get_blob`」的单块 `call` 闭包。
    /// 传输层错误（超时/断连/序列化）经 out-of-band 暂存**保真**上抛（[`SmcpAgentError::Timeout`] 等，
    /// 不被压成协议码）；拉取期协议错误（4018/漂移/解码）经 [`SmcpAgentError::Blob`] 传播。返回
    /// `(bytes, mime_type)`。供 [`Self::get_skill`] 文本回填与 tool_call 二进制旁路（AGT-04 #41）共用。
    /// 对标 Python `client.py` 的 `_make_blob_call` + `drain_blob`。
    pub(crate) async fn drain_blob_bytes(
        &self,
        computer: &str,
        blob_handle: &str,
    ) -> Result<(Vec<u8>, String)> {
        use smcp::utils::blob::{drain_blob, BlobChunkRequest, DrainBlobOptions};

        // #178：槽位解析一次（读锁内仅克隆 Arc，守卫即释放），drain 全程在无锁 Arc 上进行——
        // 不再在分块闭包内重入 `transport.read()`（旧实现与调用方外层读锁构成 write-preferring
        // RwLock 死锁环）。
        let transport = self.resolve_transport().await?;
        let agent = self.auth_provider.get_agent_config().agent.clone();
        let get_timeout = self.config.get_timeout;
        // drain 的 `call` 仅能回 ErrorPayload，无法表达传输层错误；用 out-of-band 暂存保真
        // SmcpAgentError（超时/断连/序列化），drain 失败后优先返回它，回退才映射 BlobTransferError。
        let stash: Arc<std::sync::Mutex<Option<SmcpAgentError>>> =
            Arc::new(std::sync::Mutex::new(None));

        let call = |chunk: BlobChunkRequest| {
            let agent = agent.clone();
            let stash = stash.clone();
            let transport = Arc::clone(&transport);
            async move {
                let req = build_get_blob_request(
                    &agent,
                    &chunk.computer,
                    &chunk.blob_handle,
                    Some(chunk.chunk_offset),
                    Some(chunk.max_chunk_bytes),
                );
                let data = match serde_json::to_value(&req) {
                    Ok(v) => v,
                    Err(e) => {
                        *stash.lock().unwrap() = Some(SmcpAgentError::from(e));
                        return Err(smcp::ErrorPayload::new(
                            -1,
                            "serialize get_blob request failed",
                        ));
                    }
                };
                match transport.call(CLIENT_GET_BLOB, data, get_timeout).await {
                    // flat ErrorPayload（4018 等）→ 交 drain 分类（4018→NotAccessible，余→Protocol）。
                    Ok(resp) if smcp::is_protocol_error_payload(&resp) => {
                        Err(error_payload_from_value(&resp))
                    }
                    Ok(resp) => match serde_json::from_value::<GetBlobRet>(resp) {
                        Ok(ret) => Ok(ret),
                        Err(e) => {
                            *stash.lock().unwrap() = Some(SmcpAgentError::from(e));
                            Err(smcp::ErrorPayload::new(-1, "malformed GetBlobRet"))
                        }
                    },
                    Err(e) => {
                        *stash.lock().unwrap() = Some(e);
                        Err(smcp::ErrorPayload::new(
                            -1,
                            "transport error during get_blob",
                        ))
                    }
                }
            }
        };

        match drain_blob(call, computer, blob_handle, DrainBlobOptions::default()).await {
            Ok(pair) => Ok(pair),
            Err(blob_err) => {
                // 传输层错误优先保真返回；否则映射拉取期协议错误（4018/漂移/解码）。
                if let Some(te) = stash.lock().unwrap().take() {
                    return Err(te);
                }
                Err(SmcpAgentError::from(blob_err))
            }
        }
    }

    /// 上行落盘到 Computer landing root（v0.4.0 #195）/ Upload bytes to the Computer landing root.
    ///
    /// 分块推送 `data` 至 `client:put_blob`：首块声明 `total_size`/`sha256`（+可选 `name_hint`）→
    /// ack-paced 顺序发送 → 末块取绝对 `landing_path`，可直接嵌入后续 `client:tool_call` 参数。
    /// 复用 UTIL-02 [`smcp::utils::blob::pump_blob`]，注入「以本 Agent transport 发 `client:put_blob`」
    /// 的单块 `call` 闭包。能力门控 = 自身 `PROTOCOL_VERSION` minor ≥ 0.4（pump 内强制）。
    ///
    /// 失败语义（协议 §3，`client:put_blob` 写入期一切失败 = 4019）：4019 reason（busy / too_large /
    /// forbidden / integrity ……）→ [`SmcpAgentError::Upload`]（调用方可按 reason 决定退避；重试 =
    /// 新 `upload_id` 从 0 重传）；首块超时 → `UploadUnsupported`（目标疑似不支持 put_blob，字节留
    /// 上下文，防御性兜底）；非首块传输错误 → `ChunkTransport`。
    /// 对标 Python `a2c_smcp/agent/client.py::put_blob` + `utils/blob.py::pump_blob`。
    pub async fn put_blob(
        &self,
        computer: &str,
        data: &[u8],
        name_hint: Option<&str>,
        chunk_size: Option<u64>,
    ) -> Result<PutBlobResult> {
        let transport = self.resolve_transport().await?;
        let agent = self.auth_provider.get_agent_config().agent.clone();
        let get_timeout = self.config.get_timeout;

        let call = move |chunk: PutBlobChunkRequest| {
            let agent = agent.clone();
            let transport = Arc::clone(&transport);
            async move {
                let req = build_put_blob_request(
                    &agent,
                    &chunk.computer,
                    chunk.upload_id.as_deref(),
                    chunk.chunk_offset,
                    chunk.eof,
                    &chunk.chunk,
                    chunk.declaration,
                );
                let data = match serde_json::to_value(&req) {
                    Ok(v) => v,
                    Err(e) => {
                        return Err(BlobChunkFailure::Transport(format!(
                            "serialize put_blob request failed: {e}"
                        )));
                    }
                };
                match transport.call(CLIENT_PUT_BLOB, data, get_timeout).await {
                    // flat ErrorPayload（4019 等）→ 交 pump 分类（4019→WriteFailed，余→Protocol）。
                    Ok(resp) if smcp::is_protocol_error_payload(&resp) => {
                        Err(BlobChunkFailure::Protocol(error_payload_from_value(&resp)))
                    }
                    Ok(resp) => match serde_json::from_value::<PutBlobRet>(resp) {
                        Ok(ret) => Ok(ret),
                        Err(e) => Err(BlobChunkFailure::Transport(format!(
                            "malformed PutBlobRet: {e}"
                        ))),
                    },
                    Err(SmcpAgentError::Timeout) => Err(BlobChunkFailure::Timeout),
                    Err(e) => Err(BlobChunkFailure::Transport(format!("{e}"))),
                }
            }
        };

        pump_blob(
            call,
            computer,
            data,
            PumpBlobOptions {
                name_hint: name_hint.map(str::to_string),
                // `None` → 默认 256 KiB；显式 0 由 pump 拒（bad_chunk_size，镜像 python）。
                chunk_size: chunk_size.unwrap_or(DEFAULT_CHUNK_SIZE),
            },
        )
        .await
        .map_err(|e| SmcpAgentError::Upload(Box::new(e)))
    }

    /// 调用工具，可选地响应外部取消信号。
    pub async fn tool_call(
        &self,
        computer: &str,
        tool_name: &str,
        params: serde_json::Value,
        cancel: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<serde_json::Value> {
        let agent_config = self.auth_provider.get_agent_config();
        let req = build_tool_call_request(
            &agent_config.agent,
            computer,
            tool_name,
            params,
            self.config.tool_call_timeout as i32,
        );
        let req_id_for_cancel = req.base.req_id.clone();

        debug!("Calling tool {} on computer: {}", tool_name, computer);

        let transport = self.resolve_transport().await?;
        let data = serde_json::to_value(&req)?;

        let call = transport.call(CLIENT_TOOL_CALL, data, self.config.tool_call_timeout);
        let mut external_cancel_sent = false;
        let call_result = if let Some(cancel) = cancel {
            tokio::pin!(call);
            tokio::select! {
                result = &mut call => result,
                _ = cancel.cancelled() => {
                    warn!("Tool call cancellation requested: {} on {}", tool_name, computer);
                    let cancel_data =
                        build_tool_call_cancel(&agent_config.agent, req_id_for_cancel.as_str());
                    let cancel_value = serde_json::to_value(cancel_data)?;
                    if let Err(e) = transport.emit(SERVER_TOOL_CALL_CANCEL, cancel_value).await {
                        error!("Failed to send cancel request: {}", e);
                    }
                    external_cancel_sent = true;
                    call.await
                }
            }
        } else {
            call.await
        };

        match call_result {
            Ok(response) => {
                // flat ErrorPayload → 协议错误（如 4006/4007 授权、404 未命中）；正常 CallToolResult
                // （含 isError 的工具执行失败）无顶层协议 code，原样透传。
                raise_for_error_payload(&response)?;
                // AGT-04 #41（⚠️ BREAKING）：消费 CallToolResult 二进制旁路——遍历 content item 检测
                // `_meta.a2c_blob_handle` 并 drain 回填，否则超内联预算的大二进制会静默变空。无句柄路径不变。
                let response = self
                    .resolve_tool_call_binary_sideband(computer, response)
                    .await;
                info!("Tool call successful: {} on {}", tool_name, computer);
                Ok(response)
            }
            Err(SmcpAgentError::Timeout) => {
                warn!(
                    "Tool call timeout, cancelling: {} on {}",
                    tool_name, computer
                );
                // 发送取消请求（复用统一取消载体 builder：req_id==原 tool_call req_id）
                if !external_cancel_sent {
                    let cancel_data =
                        build_tool_call_cancel(&agent_config.agent, req_id_for_cancel.as_str());
                    let cancel_value = serde_json::to_value(cancel_data)?;
                    if let Err(e) = transport.emit(SERVER_TOOL_CALL_CANCEL, cancel_value).await {
                        error!("Failed to send cancel request: {}", e);
                    }
                }

                // 返回超时错误（结果级 meta.a2c_timeout=true → Agent 三态分类归 TimedOut，#92 P1）
                Ok(local_timeout_call_result(req_id_for_cancel.as_str()))
            }
            Err(e) => {
                error!(
                    "Tool call failed: {} on {}, error: {}",
                    tool_name, computer, e
                );
                Err(e)
            }
        }
    }

    /// 解析并回填 `client:tool_call` 结果的二进制旁路（AGT-04 #41，⚠️ BREAKING）/ resolve tool_call binary sideband。
    ///
    /// 三段式（见 [`crate::blob_sideband`] 模块文档）：
    /// 1. **extract**——遍历 `CallToolResult.content` 收集带 `_meta.a2c_blob_handle` 的 item 索引+句柄；
    /// 2. **drain**——逐句柄经 [`Self::drain_blob_bytes`] 拉回全量字节（多块 + sha256 自证 + 4018 处置）；
    /// 3. **inject**——把字节回填到对应 content item 内联载体并清理 `_meta` 旁路键（消费后与小尺寸内联无异）。
    ///
    /// 无句柄的 content item 路径**完全不变**（无句柄时直接早返回原值）。容错（对齐 Python
    /// `_resolve_tool_call_binary_sideband`）：任一 item 的 drain 失败仅 `warn` + **保留**其
    /// `_meta.a2c_blob_handle` 不回填，继续处理其余 item（不整体失败、不早退）——调用方仍可据残留句柄
    /// 自行 [`Self::get_blob`] 兜底。
    async fn resolve_tool_call_binary_sideband(
        &self,
        computer: &str,
        mut response: serde_json::Value,
    ) -> serde_json::Value {
        let handles = crate::blob_sideband::extract_sideband_handles(&response);
        if handles.is_empty() {
            return response; // 无旁路句柄：路径不变（含全部非二进制 tool_call）。
        }
        for (idx, handle) in handles {
            match self.drain_blob_bytes(computer, &handle).await {
                Ok((bytes, _mime)) => {
                    if let Some(item) = response
                        .get_mut("content")
                        .and_then(serde_json::Value::as_array_mut)
                        .and_then(|arr| arr.get_mut(idx))
                    {
                        crate::blob_sideband::inject_payload_into_content_item(item, &bytes);
                    }
                }
                Err(e) => warn!(
                    "tool_call binary sideband drain failed for handle={}: {}; keeping _meta.a2c_blob_handle intact",
                    handle, e
                ),
            }
        }
        response
    }

    /// 列出房间内的所有会话
    pub async fn list_room(&self, office_id: &str) -> Result<Vec<SessionInfo>> {
        let agent_config = self.auth_provider.get_agent_config();
        let req_id = ReqId::new();
        let req = ListRoomReq {
            base: AgentCallData {
                agent: agent_config.agent.clone(),
                req_id: req_id.clone(),
            },
            office_id: office_id.to_string(),
        };

        debug!("Listing sessions in office: {}", office_id);

        let transport = self.resolve_transport().await?;
        let data = serde_json::to_value(req)?;
        let response = transport
            .call(SERVER_LIST_ROOM, data, self.config.default_timeout)
            .await?;

        // flat ErrorPayload → 协议错误（`400` 载荷畸形 / `4103` 无房 / `4104` 跨房）。
        // **必须先行**于 `req_id` 校验：ErrorPayload 不带 `req_id`，先查 req_id 会把结构化拒绝误报成
        // 「响应 req_id 不匹配」的内部错误——调用方看到一个无法解释的协议违约，而非「我不在任何房」。
        // 对标 Python `client.py::get_computers_in_office` 的同款顺序约束（其注释点名了该坑）。
        raise_for_error_payload(&response)?;

        // 验证 req_id（全 crate 单点收敛，见 response::ensure_req_id）
        ensure_req_id(&response, req_id.as_str())?;

        let sessions: Vec<SessionInfo> =
            serde_json::from_value(response.get("sessions").cloned().unwrap_or_default())?;

        info!(
            "Listed {} sessions in office: {}",
            sessions.len(),
            office_id
        );
        Ok(sessions)
    }
}

/// MIME 是否「文本类」（决定 `get_skill` 句柄是否自动 drain 回填 `body`）/ is this MIME textual?
///
/// 经单一权威 [`smcp::utils::mime::is_text_mime`] 实施协议 §6.4(2) 三分支（`text/*` ∨ `+json/+xml/+yaml`
/// 后缀 ∨ essence 白名单），与 Computer 铸造期 `is_text` 路由及当前 Python `a2c_smcp/agent/client.py`
/// 的 `is_text_mime` **同源同判**（跨 SDK 一致：同一 Computer 响应在两 SDK 给调用方相同结果）。
/// 旧实现仅 `starts_with("text/")`，会漏判 `application/json|yaml|toml` 等超内联预算的文本（python-sdk
/// #105 盲区）；现已收敛。非文本（真二进制）一律保持 `blob_handle`，由调用方按需经 `get_blob` 自取。
fn mime_is_textual(mime: Option<&str>) -> bool {
    mime.map(smcp::utils::mime::is_text_mime).unwrap_or(false)
}

/// 从已判定为 flat 协议错误的 `Value` 宽松构造 [`smcp::ErrorPayload`]，交 `drain_blob` 分类。
/// 保留 `code`/`message`/`details`（`classify_fatal` 仅据 `code` + `details.reason` 分流），
/// 缺字段宽松回退（不 panic），等价于直接 `from_value` 但容忍缺 `message`。
fn error_payload_from_value(v: &serde_json::Value) -> smcp::ErrorPayload {
    let code = v
        .get("code")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(-1);
    let message = v
        .get("message")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    let mut payload = smcp::ErrorPayload::new(code, message);
    payload.details = v.get("details").cloned();
    payload
}

// 实现Clone以便在事件处理器中使用
impl Clone for AsyncSmcpAgent {
    fn clone(&self) -> Self {
        Self {
            transport: self.transport.clone(),
            auth_provider: self.auth_provider.clone(),
            event_handler: self.event_handler.clone(),
            config: self.config.clone(),
            tools_cache: self.tools_cache.clone(),
            connection_tasks: self.connection_tasks.clone(),
            connect_operation: self.connect_operation.clone(),
            // 状态、操作门与连接资源归属在所有克隆体间共享。
            office: self.office.clone(),
            office_operation: self.office_operation.clone(),
            office_connection_seq: self.office_connection_seq.clone(),
        }
    }
}

/// 构造 Agent **本地**超时兜底的 `CallToolResult`-shape 响应（P1 #92）。
///
/// 写结果级 `meta.a2c_timeout=true`（协议规范 wire key `meta`，键用单一权威常量
/// [`smcp::tool_meta::A2C_TIMEOUT_KEY`]），使 [`crate::response::classify_tool_call_outcome`] 把该响应归类为
/// `TimedOut`（而非 `Failed`），与 Computer 侧超时态语义对齐。纯函数，便于单测（本仓 transport
/// 无 mock 接缝，约定抽纯 helper 单测）。
fn local_timeout_call_result(req_id: &str) -> serde_json::Value {
    let mut v = serde_json::json!({
        "content": [{
            "type": "text",
            "text": format!("工具调用超时 / Tool call timeout, req_id={}", req_id)
        }],
        "isError": true,
    });
    let mut meta = serde_json::Map::new();
    meta.insert(
        smcp::tool_meta::A2C_TIMEOUT_KEY.to_string(),
        serde_json::Value::Bool(true),
    );
    v["meta"] = serde_json::Value::Object(meta);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #81：`get_skill` 文本句柄自动 drain 的 MIME 判定遵循协议 §6.4(2) 三分支，经单一权威
    /// `smcp::utils::mime::is_text_mime`（与当前 Python `is_text_mime` parity）/ §6.4(2) textual gate。
    #[test]
    fn test_mime_is_textual_follows_spec_64_2() {
        // 分支 1：text/*（含 charset 参数）。
        assert!(mime_is_textual(Some("text/plain")));
        assert!(mime_is_textual(Some("text/markdown")));
        assert!(mime_is_textual(Some("text/plain; charset=utf-8")));
        // 分支 2：+json / +xml / +yaml 后缀（旧 starts_with("text/") 漏掉）。
        assert!(mime_is_textual(Some("image/svg+xml")));
        assert!(mime_is_textual(Some("application/vnd.api+json")));
        // 分支 3：application/* 文本 essence 白名单（旧 starts_with("text/") 漏掉 → python-sdk #105 盲区）。
        assert!(mime_is_textual(Some("application/json")));
        assert!(mime_is_textual(Some("application/json; charset=utf-8")));
        assert!(mime_is_textual(Some("application/xml")));
        assert!(mime_is_textual(Some("application/yaml")));
        assert!(mime_is_textual(Some("application/toml")));
        // 真二进制 / 缺省一律非文本（保持 blob_handle）。
        assert!(!mime_is_textual(Some("application/octet-stream")));
        assert!(!mime_is_textual(Some("image/png")));
        assert!(!mime_is_textual(Some("")));
        assert!(!mime_is_textual(None));
    }

    /// #92 P1：本地超时兜底结果写结果级 `meta.a2c_timeout=true`，使三态分类归 `TimedOut`（非 `Failed`）。
    #[test]
    fn test_local_timeout_result_classifies_as_timed_out() {
        use crate::response::{classify_tool_call_outcome, ToolCallOutcome};
        let v = local_timeout_call_result("rid-42");
        // 结果级标记落协议规范的 wire key `meta`。
        assert_eq!(
            v["meta"][smcp::tool_meta::A2C_TIMEOUT_KEY],
            serde_json::json!(true)
        );
        assert_eq!(v["isError"], serde_json::json!(true));
        assert!(
            v["content"][0]["text"].as_str().unwrap().contains("rid-42"),
            "超时文案应含 req_id"
        );
        // 关键：被三态分类器归为 TimedOut（之前无 meta 时归 Failed）。
        assert_eq!(classify_tool_call_outcome(&v), ToolCallOutcome::TimedOut);
    }

    /// #178 Defect 1 回归守卫：`resolve_transport` 的读锁内**不跨任何 await**（仅克隆 Arc）。
    /// 并发读者 + 排队写者（模拟 `connect()` 槽位替换）必须在超时内全部完成——旧实现的
    /// 「外层读锁跨响应 + `drain_blob_bytes` 内层重入读」在 tokio write-preferring RwLock 下
    /// 构成永久死锁环（队列中的写者阻塞新读者）。本测试按仓库「transport 无 mock 接缝，
    /// 约定抽纯 helper 单测」的约定，直接对锁模式做并发验证。
    #[tokio::test]
    async fn test_resolve_transport_not_deadlocked_by_queued_connect_write() {
        use crate::auth::DefaultAuthProvider;
        use std::time::Duration;

        let agent = Arc::new(AsyncSmcpAgent::new(
            DefaultAuthProvider::new("agent".into(), "office".into()),
            SmcpAgentConfig::default(),
        ));

        // 16 个并发读者（模拟 in-flight 的 get_skill/tool_call 解析槽位）+ 1 个排队写者
        // （模拟 connect() 的 `*self.transport.write().await = Some(..)`）。
        let mut tasks = Vec::new();
        for _ in 0..16 {
            let a = Arc::clone(&agent);
            tasks.push(tokio::spawn(async move {
                // transport 槽位为 None → 统一得 connection 错误；关键在于不卡死。
                let _ = a.resolve_transport().await;
            }));
        }
        let w = Arc::clone(&agent);
        tasks.push(tokio::spawn(async move {
            *w.transport.write().await = None;
        }));

        tokio::time::timeout(Duration::from_secs(2), async {
            for t in tasks {
                let _ = t.await;
            }
        })
        .await
        .expect("resolve_transport 读锁与排队写者构成死锁（#178）");
    }
}
