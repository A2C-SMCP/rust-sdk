/*!
* 文件名: office
* 作者: JQQ
* 描述: Agent 侧 Office 成员关系状态机与重连回房策略 / Agent-side office membership state machine
*       and reconnect-rejoin policy
*
* 背景（#219，镜像 python-sdk#203）：Socket.IO 房间成员关系属于**会话**——传输层自动重连后
* namespace 换新 SID，服务端已随旧会话销毁成员关系，客户端若只保留本地状态就会「看起来还在房间、
* 实际收不到任何房间流量」。故客户端必须自己记住**入房意图**，并在 namespace 重连后重放
* `server:join_office` 取得服务端裁决。
*
* 本模块只承载两件事：
*
* 1. `OfficeMembership`（crate 内部）——意图 / 已确认状态 / 世代（generation）状态机，与
*    `smcp-computer` 的 `OfficeMembership`（#204/#211）同构；
* 2. **纯函数**形式的回房策略（拒绝码分类、退避曲线、预算判据）——传输层为具体 struct 无 mock 接缝，
*    把可判定逻辑抽成纯函数是本仓既有做法（见 `crates/smcp-agent/src/protocol_error.rs`）。
*/

use std::time::Duration;

use crate::error::SmcpAgentError;

/// 恢复路径上**可退避重试**的协议码：`4101 Room Full` / `4105 Name Conflict`。
///
/// 这两个码的**瞬态**成因只可能出现在「传输层重连后的恢复路径」上——静默断线使服务端仍持有本客户端
/// 的旧会话，新会话必然撞上「一房一 Agent」/「同名唯一」检查（协议 error-handling.md §4101/§4105、
/// room-model.md §静默断线与会话回收）。首次入房不存在这一窗口（此前本客户端无会话）。
///
/// `4106 Already In Room` **不在**此列：重连产生的是新会话（服务端侧 `office_id` 为空），不可能
/// 「已在其它房」；它只由客户端自身状态错误产生，须先显式退房再入新房。
pub(crate) const TRANSIENT_ROOM_CONFLICT_CODES: [i64; 2] = [4101, 4105];

/// 回房重试的**初始退避间隔**（1s）。
pub(crate) const OFFICE_REJOIN_BACKOFF_INITIAL: Duration = Duration::from_secs(1);

/// 回房重试的**退避封顶**（8s）。
pub(crate) const OFFICE_REJOIN_BACKOFF_MAX: Duration = Duration::from_secs(8);

/// Agent 的入房意图：`(office_id, agent_name)`。
///
/// `agent_name` 必须随意图一起记住——Agent 侧它只出现在 `join_office` 的**调用实参**里
/// （`auth_provider` 只提供 `office_id`），不记住就无法在重连后原样重放。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfficeIntent {
    /// 目标房间号 / target office id.
    pub office_id: String,
    /// 入房时自述的名称 / the name declared at join time.
    pub agent_name: String,
}

/// 在途回房 task 的 abort 句柄 / abort handle of an in-flight rejoin task.
pub(crate) type RejoinTaskHandle = tokio::task::AbortHandle;

/// [`OfficeMembership::bind_connected`] 的返回值：`(待作废的在途回房句柄, 待重放的 (世代, 意图))`。
pub(crate) type ConnectedBinding = (Option<RejoinTaskHandle>, Option<(u64, OfficeIntent)>);

/// [`OfficeMembership::bind_closed`] 的返回值：`(是否生效, 待作废的在途回房句柄)`。
pub(crate) type ClosedBinding = (bool, Option<RejoinTaskHandle>);

/// 未提交连接（正在进行 `connect()` 尝试）的状态。
///
/// 由 `connect()` 与生命周期 task 经**同一把锁**读写——这正是「提交」与「已到达的断开」之间不存在
/// 检查-使用窗口（TOCTOU）的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttemptState {
    /// 尝试已登记（`connect()` 已认领该槽位），但尚未收到 namespace 建立。
    Pending,
    /// 该连接已报告 namespace 建立（待提交）。
    Connected {
        /// 本次会话的内核 epoch / the session epoch reported by this attempt.
        epoch: u64,
    },
    /// 该连接在**提交之前**已断开。
    Closed,
}

/// [`OfficeMembership::commit_attempt`] 的裁决 / the verdict of committing a connect attempt.
pub(crate) enum AttemptCommit {
    /// 提交成功：连接归属与本会话绑定已在**同一临界区**内完成。
    Committed {
        /// 上一连接遗留的在途回房句柄（调用方须 abort）。
        previous_rejoin: Option<RejoinTaskHandle>,
        /// 待重放的 `(世代, 意图)`（意图仍在时）。
        replay: Option<(u64, OfficeIntent)>,
    },
    /// 该连接在提交前已断开（或状态不可信）⇒ **不得提交**。
    ClosedBeforeCommit,
}

/// Agent 侧 Office 成员关系（对外可观测状态）。
///
/// `Connected` 与 `JoinedOffice` **必须可区分**——回房失败后回退到
/// [`Connected`](Self::Connected) 而非继续宣称在房，是本特性的承重语义（「不静默假装在线」）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OfficeMembershipState {
    /// namespace 未在册（尚未连接，或连接已断开）。
    Disconnected,
    /// namespace 已连接，但**不在任何房**——含回房失败后的回退态。
    Connected,
    /// 服务端已确认的成员关系 / membership confirmed by the server.
    JoinedOffice {
        /// 已确认的房间号 / the confirmed office id.
        office_id: String,
    },
}

/// Office 成员关系状态机（意图 + 已确认 + 世代 + 连接在册）。
///
/// 所有构成公开不变量的字段同处一把 `std::sync::Mutex` 之下（持有者为
/// [`crate::async_agent::AsyncSmcpAgent`]），使传输层回调不可能在「校验世代」与「提交状态」之间插入；
/// 守卫**绝不跨 await 持有**（与 Computer 侧同款纪律）。
#[derive(Default)]
pub(crate) struct OfficeMembership {
    /// **当前已提交连接**的序号（`AsyncSmcpAgent::connect` 每次尝试分配一个）。成员关系属于连接：
    /// 只有该连接的生命周期事件才允许改写状态——未提交（或已被替换）的连接的事件一律忽略，使
    /// `connect()` 在满足后置条件前**不触碰**既有连接的状态（事务性），也让「旧连接的迟到事件」
    /// 无法clobber 新连接。注意各连接的 `session_epoch` **各自从 1 起算**，故 epoch 不能单独标识连接。
    connection: Option<u64>,
    /// 客户端表达的**入房意图**：`join_office` 成功后落账；回房失败 / 显式退房 / 服务端踢出即清除。
    desired: Option<OfficeIntent>,
    /// 服务端**已确认**的成员关系；断连或回房失败即清空（≠ 意图）。
    confirmed: Option<OfficeIntent>,
    /// 世代号：Connect / Close / 显式 join / 显式 leave 均使其前进，作废在途回房结果。
    generation: u64,
    /// 当前连接已观察到的最大 namespace epoch，包括先于 Connect 到达的 Close。
    /// 同 epoch 的关闭是终态；只有更大的 epoch 才能重新置为 connected。
    transport_epoch: u64,
    /// namespace 是否在册（Connect 按 epoch 置真、同 epoch 的 Close 置假）。
    connected: bool,
    /// 在途回房 task 的句柄：Close / 显式 join / 显式 leave 一律 abort（不持续抢房）。
    rejoin_task: Option<tokio::task::AbortHandle>,
    /// 正在进行中的 `connect()` 尝试（未提交）：`(连接序号, 状态)`。见 [`AttemptState`]。
    pending_attempt: Option<(u64, AttemptState)>,
}

/// 一次异步房间操作的**会话标识**（发起前捕获，ACK 回来时校验）。
///
/// 传输层回调与 ack 分发在内核里是**各自独立的 task，无执行顺序契约**（`tf-rust-socketio` 的
/// #12 语义），故「请求已发出」与「收到了成功 ack」之间可能夹进一次 Close / Connect。此时 ack 陈述的
/// 是**已死会话**的事实，不得落成成员关系（否则会出现「namespace 已断开却报 `JoinedOffice`」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OfficeSessionToken {
    /// 发起时**已提交**的连接序号 / the committed connection captured before the request.
    connection: Option<u64>,
    /// 发起时的世代号 / generation captured before the request was sent.
    generation: u64,
    /// 发起时的会话 epoch / session epoch captured before the request was sent.
    epoch: u64,
}

impl OfficeMembership {
    /// 推进世代并返回新值 / advance the generation and return it.
    fn next_generation(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.generation
    }

    /// 本次回房结果是否仍然「新鲜」：连接仍在册 ∧ 世代未前进 ∧ 意图未被改写。
    ///
    /// 四个条件缺一不可——Close 会置 `connected = false` 并推进世代，显式 join/leave 会改写意图，
    /// 而**连接被替换**（`bind_connection` 指向新连接）使旧连接的在途回房结果作废：旧 task 的 abort 是
    /// 异步的、不能当同步屏障，故连接归属必须落在状态检查里。
    pub(crate) fn is_current(
        &self,
        connection_id: u64,
        generation: u64,
        intent: &OfficeIntent,
    ) -> bool {
        self.connection == Some(connection_id)
            && self.connected
            && self.generation == generation
            && self.desired.as_ref() == Some(intent)
    }

    /// 对外可观测状态快照 / observable state snapshot.
    ///
    /// `connected == false` 时**一律**报 `Disconnected`——即使 `confirmed` 尚有余留：成员关系属于
    /// **会话**，会话不在册就不存在「在房」这一事实（「不静默假装在线」的兜底防线）。
    pub(crate) fn state(&self) -> OfficeMembershipState {
        match (self.connected, &self.confirmed) {
            (true, Some(intent)) => OfficeMembershipState::JoinedOffice {
                office_id: intent.office_id.clone(),
            },
            (true, None) => OfficeMembershipState::Connected,
            (false, _) => OfficeMembershipState::Disconnected,
        }
    }

    /// 捕获当前会话标识，供异步操作的 ACK 回来后校验（见 [`OfficeSessionToken`]）。
    pub(crate) fn session_token(&self) -> OfficeSessionToken {
        OfficeSessionToken {
            connection: self.connection,
            generation: self.generation,
            epoch: self.transport_epoch,
        }
    }

    /// 登记的**已提交连接**（供生命周期 task 判断事件归属）。
    pub(crate) fn committed_connection(&self) -> Option<u64> {
        self.connection
    }

    /// **认领本次尝试的槽位**（`connect()` 在起生命周期 task 之前调用）。
    ///
    /// 归属先行是必要的：否则「`Closed` 先于首个 `Connected` 到达」时槽位为空，断开记录无处安放而被丢弃，
    /// 随后的 `Connected` 又把它写成「待提交」，于是一条已断开的连接会被提交（评审 B3''① 残余路径）。
    /// 认领是无条件覆盖——它只由**当前**尝试在自己的事件被处理之前调用。
    pub(crate) fn begin_attempt(&mut self, connection_id: u64) {
        self.pending_attempt = Some((connection_id, AttemptState::Pending));
    }

    /// 只撤销属于本次尝试的记录，取消清理不得触碰后继尝试。
    pub(crate) fn cancel_attempt(&mut self, connection_id: u64) {
        if self
            .pending_attempt
            .as_ref()
            .is_some_and(|(id, _)| *id == connection_id)
        {
            self.pending_attempt = None;
        }
    }

    /// 记录「未提交连接报告了 namespace 建立」（生命周期 task 调用）。
    ///
    /// 两条**单调性**纪律（否则提交点会漏判已到达的断开）：
    ///
    /// 1. **同一连接已记为 `Closed` ⇒ 不得复活为 `Connected`**。内核的 Connect / Close 回调是各自独立的
    ///    task，投递顺序无契约，故「先到的 Closed 被后到的 Connected 覆盖」在原理上不可排除；而「宁严勿宽」
    ///    在这里的代价只是让 `connect()` 如实失败（调用方可重试），误判的代价则是把死连接报成「已连接」。
    /// 2. **已有其它连接的在途记录 ⇒ 忽略**。上一个尝试（如已超时、task 虽 abort 但回调仍在途）的迟到
    ///    `Connected` 不得覆盖当前尝试的记录，否则当前 `connect()` 会被误判为「提交前已断开」。
    pub(crate) fn record_attempt_connected(&mut self, connection_id: u64, epoch: u64) {
        match self.pending_attempt {
            // 同一连接已见断开（单调，不复活） / 别的连接的在途记录（不得覆盖）
            Some((pending_id, AttemptState::Closed)) if pending_id == connection_id => {}
            Some((pending_id, _)) if pending_id != connection_id => {}
            Some((pending_id, _)) if pending_id == connection_id => {
                self.pending_attempt = Some((connection_id, AttemptState::Connected { epoch }));
            }
            // 槽位不属于任何尝试（理论不可达：`connect()` 认领在先）⇒ 宁严勿宽，记为「提交前已断开」，
            // 使提交点拒绝对未知状态放行。
            _ => self.pending_attempt = Some((connection_id, AttemptState::Closed)),
        }
    }

    /// 记录「未提交连接已断开」（生命周期 task 调用）：提交点据此拒绝把死连接登记为当前连接。
    pub(crate) fn record_attempt_closed(&mut self, connection_id: u64) {
        match self.pending_attempt {
            // 别的连接的在途记录 ⇒ 迟到的旧连接事件一律忽略（不得覆盖当前尝试）
            Some((pending_id, _)) if pending_id != connection_id => {}
            // 本连接（无论此前是 Pending 还是 Connected）⇒ 一律记为「提交前已断开」（单调）
            Some(_) => self.pending_attempt = Some((connection_id, AttemptState::Closed)),
            // 无在途记录 ⇒ 与 [`Self::record_attempt_connected`] 同款兜底：宁严勿宽。
            None => self.pending_attempt = Some((connection_id, AttemptState::Closed)),
        }
    }

    /// **原子提交点**：在**同一临界区**内裁决「该尝试是否已断开」并完成连接归属 + 会话绑定。
    ///
    /// 这是 `connect()` 与生命周期 task 之间唯一的裁决点——断开要么在提交前已被记录（⇒ 拒绝提交），
    /// 要么发生在提交之后（⇒ 按已提交连接的普通断开处理，状态机据此重连 / 回房）。故不存在「检查之后、
    /// 提交之前」的窗口：这正是「已到达的 Close 不得丢失」的落地方式。
    pub(crate) fn commit_attempt(&mut self, connection_id: u64) -> AttemptCommit {
        let Some((pending_id, state)) = self.pending_attempt.take() else {
            // 没有尝试记录却来提交：状态不可信 ⇒ 一律拒绝（宁严勿宽，绝不放行未知状态）。
            return AttemptCommit::ClosedBeforeCommit;
        };
        if pending_id != connection_id {
            return AttemptCommit::ClosedBeforeCommit;
        }
        let AttemptState::Connected { epoch } = state else {
            // `Pending`（尚未收到 namespace 建立）或 `Closed`（提交前已断开）一律拒绝。
            return AttemptCommit::ClosedBeforeCommit;
        };

        // 连接归属 + 会话绑定 + 世代推进在同一临界区内完成（不再存在「已登记未绑定」的中间态）。
        self.connection = Some(connection_id);
        let previous_rejoin = self.rejoin_task.take();
        let generation = self.next_generation();
        self.transport_epoch = epoch;
        self.connected = true;
        // 新会话的成员关系未经服务端确认：重放取得空 ack 之前不得宣称在房。
        self.confirmed = None;
        let replay = self.desired.clone().map(|intent| (generation, intent));
        AttemptCommit::Committed {
            previous_rejoin,
            replay,
        }
    }

    /// 服务端已确认的房间号 / the server-confirmed office id.
    pub(crate) fn confirmed_office_id(&self) -> Option<String> {
        self.confirmed
            .as_ref()
            .map(|intent| intent.office_id.clone())
    }

    /// 换房判据：已有成员关系（意图或已确认）指向的房与 `target` 不同 ⇒ 返回**当前**房号。
    ///
    /// 协议 room-model §Agent 加入规则 1：Agent 已在其它房又请求入新房 MUST 先显式
    /// `server:leave_office`——Computer 的自动换房规则**不适用**于 Agent。
    pub(crate) fn conflicting_office(&self, target: &str) -> Option<String> {
        self.desired
            .as_ref()
            .or(self.confirmed.as_ref())
            .map(|intent| intent.office_id.as_str())
            .filter(|current| *current != target)
            .map(str::to_string)
    }

    /// 显式入房成功：**仅当 ACK 所属会话仍是当前且仍在册的会话**时落账意图 + 已确认。
    ///
    /// 三个前提缺一不可：`connected`（会话在册）∧ 世代未前进 ∧ epoch 未变。要求 `connected` 是
    /// **不变量的下界**——`confirmed` 一经落账即代表「会话在册且服务端已确认」，故
    /// [`Self::confirmed_office_id`] 与 [`Self::state`] 对外的语义不会自相矛盾（不会出现
    /// 「`confirmed` 有值却报 `Disconnected`」的组合）。
    ///
    /// 会话已更替（期间发生 Close / 新 Connect / 显式退房）⇒ 返回 `None`，**完全不改状态**——不仅不
    /// 落账 `confirmed`，也**不**落账 `desired`：否则会把期间刚被「显式退房」清掉的意图复活成下一次
    /// 重连的自动回房目标（与调用方刚表达的意图相反）。
    ///
    /// 落账成功时一并作废在途回房（返回其句柄供调用方 abort）。返回
    /// `(是否落账, 待 abort 的在途回房句柄)`。
    pub(crate) fn commit_join(
        &mut self,
        session: OfficeSessionToken,
        intent: OfficeIntent,
    ) -> (bool, Option<tokio::task::AbortHandle>) {
        if !self.connected
            || self.generation != session.generation
            || self.transport_epoch != session.epoch
            || self.connection != session.connection
        {
            return (false, None);
        }
        self.next_generation();
        self.desired = Some(intent.clone());
        self.confirmed = Some(intent);
        (true, self.rejoin_task.take())
    }

    /// 显式退房 / 服务端踢出：清空意图与已确认，作废在途回房。
    ///
    /// 与 [`Self::commit_join`] 一样推进世代，使在途回房的结果无法再落账。
    pub(crate) fn clear_intent(&mut self) -> Option<tokio::task::AbortHandle> {
        self.next_generation();
        self.desired = None;
        self.confirmed = None;
        self.rejoin_task.take()
    }

    /// namespace 连接建立（含自动重连后的新会话）：绑定 epoch、置在册、作废上一世代，
    /// 并按「意图是否仍在」交出待重放的 `(generation, intent)`。
    /// 返回 `None` 表示连接归属不符或 epoch 不比已观察到的更新（陈旧、重复或已关闭）。
    pub(crate) fn bind_connected(
        &mut self,
        connection_id: u64,
        epoch: u64,
    ) -> Option<ConnectedBinding> {
        if self.connection != Some(connection_id) || epoch <= self.transport_epoch {
            return None;
        }
        let previous = self.rejoin_task.take();
        let generation = self.next_generation();
        self.transport_epoch = epoch;
        self.connected = true;
        // 新会话的成员关系未经服务端确认：重放取得空 ack 之前不得宣称在房。
        self.confirmed = None;
        let replay = self.desired.clone().map(|intent| (generation, intent));
        Some((previous, replay))
    }

    /// namespace 断开：接受当前或更新 epoch 的 Close（它可能先于 Connect 到达）。
    /// 陈旧及重复 Close 丢弃；记录关闭 epoch，阻止迟到 Connect 复活该会话。
    /// `retain_intent` 为真时保留意图供下一次 Connect 重放。
    ///
    /// 返回 `None` 表示该事件**不属于已提交的连接** ⇒ 调用方须原样忽略（未提交连接的断开不得动
    /// 既有连接的状态）；`Some((是否生效, 在途回房句柄))` 见上。
    pub(crate) fn bind_closed(
        &mut self,
        connection_id: u64,
        epoch: u64,
        retain_intent: bool,
    ) -> Option<ClosedBinding> {
        if self.connection != Some(connection_id) {
            return None;
        }
        if epoch < self.transport_epoch || (epoch == self.transport_epoch && !self.connected) {
            return Some((false, None));
        }
        self.next_generation();
        self.transport_epoch = epoch;
        self.connected = false;
        self.confirmed = None;
        if !retain_intent {
            self.desired = None;
        }
        Some((true, self.rejoin_task.take()))
    }

    /// 回房成功：仅在世代仍新鲜时落账已确认成员关系。返回是否落账。
    pub(crate) fn commit_rejoin(
        &mut self,
        connection_id: u64,
        generation: u64,
        intent: &OfficeIntent,
    ) -> bool {
        if !self.is_current(connection_id, generation, intent) {
            return false;
        }
        self.confirmed = Some(intent.clone());
        true
    }

    /// 回房**放弃**（预算耗尽 / 永久拒绝）：仅在世代仍新鲜时清空意图与已确认，使状态如实回退到
    /// [`OfficeMembershipState::Connected`] 且不再重试。返回是否清空。
    pub(crate) fn abandon_rejoin(
        &mut self,
        connection_id: u64,
        generation: u64,
        intent: &OfficeIntent,
    ) -> bool {
        if !self.is_current(connection_id, generation, intent) {
            return false;
        }
        self.next_generation();
        self.desired = None;
        self.confirmed = None;
        self.rejoin_task = None;
        true
    }

    /// 登记在途回房 task 句柄；世代/意图已不新鲜则**不入册**（返回 `false`，调用方须立即 abort）。
    pub(crate) fn set_rejoin_task(
        &mut self,
        connection_id: u64,
        generation: u64,
        intent: &OfficeIntent,
        handle: tokio::task::AbortHandle,
    ) -> bool {
        if !self.is_current(connection_id, generation, intent) {
            return false;
        }
        self.rejoin_task = Some(handle);
        true
    }
}

/// 回房单次重放的裁决分类 / verdict classification of a single rejoin replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RejoinVerdict {
    /// 瞬态冲突（`4101` / `4105`）：服务端可能尚未回收旧会话，可在**预算内**退避重试。
    TransientConflict,
    /// 永久失败：其它协议码、未获裁决（形状不认识）或传输层错误——重试不改变结果。
    Permanent,
}

/// 判定单次回房失败是否值得重试。
///
/// 只有 [`TRANSIENT_ROOM_CONFLICT_CODES`] 可重试；其余一律永久失败——包括**未获裁决**
/// （`SmcpProtocolError::indeterminate`，`code = -1`）与传输层错误。「宁严勿宽」：把不确定读成
/// 「可重试」会把一次真拒绝放大成整段预算的无效重放，而读成「永久」最多让用户手工重入一次。
pub(crate) fn classify_rejoin_error(error: &SmcpAgentError) -> RejoinVerdict {
    match error {
        SmcpAgentError::Protocol(protocol)
            if TRANSIENT_ROOM_CONFLICT_CODES.contains(&protocol.code) =>
        {
            RejoinVerdict::TransientConflict
        }
        _ => RejoinVerdict::Permanent,
    }
}

/// 第 `attempt` 次尝试失败后的退避延迟（`attempt` 自 1 起）：`1s → 2s → 4s → 8s`，此后封顶 8s。
///
/// 曲线归 SDK 自治（协议 error-handling.md §建议的重试策略）；对外可配的是**总预算**
/// （[`crate::config::SmcpAgentConfig::office_rejoin_budget_secs`]）。
pub(crate) fn backoff_delay(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(3);
    let delay = OFFICE_REJOIN_BACKOFF_INITIAL.saturating_mul(1u32 << shift);
    delay.min(OFFICE_REJOIN_BACKOFF_MAX)
}

/// 预算判据：退避后再发起一次尝试是否仍落在预算内。
///
/// 预算约束的是**尝试起始时刻**（`elapsed + delay ≤ budget`），故单次尝试自身的耗时不计入预算——
/// 总耗时上界 ≈ 预算 + 单次 `server:join_office` 的 ack 等待上限。这样默认预算 60s 在覆盖 socket.io
/// 默认最长回收窗口 45s（`ping_interval(25) + ping_timeout(20)`）之后仍留有一次尝试的余量。
pub(crate) fn retry_fits_budget(elapsed: Duration, delay: Duration, budget: Duration) -> bool {
    elapsed.saturating_add(delay) <= budget
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol_error::SmcpProtocolError;

    fn intent(office: &str, name: &str) -> OfficeIntent {
        OfficeIntent {
            office_id: office.to_string(),
            agent_name: name.to_string(),
        }
    }

    /// 构造任意协议码的拒绝错误（不限已建模闭集，便于钉「未知码也判永久」）。
    fn protocol_error(code: i64) -> SmcpAgentError {
        let payload = smcp::ErrorPayload::new(code, "rejected");
        SmcpAgentError::Protocol(Box::new(SmcpProtocolError::from_error_payload(&payload)))
    }

    /// 测试助手：在「同一会话」前提下完成一次显式入房（等价于 `join_office` 的成功路径）。
    fn join_here(membership: &mut OfficeMembership, office: &str, name: &str) -> bool {
        let session = membership.session_token();
        membership.commit_join(session, intent(office, name)).0
    }

    /// 测试用**已提交连接**序号：测试只关心「同一连接」（重连 = 同连接、新 epoch）。
    const CONN: u64 = 7;

    /// 测试助手：走一次**原子提交**（等价于 `connect()` 的成功提交：记录尝试 → 提交）。
    ///
    /// 对**同一连接**的再次调用即等价于「重连后重新绑定新会话」（新 epoch、世代前进）。
    fn bind_connected(membership: &mut OfficeMembership, epoch: u64) -> ConnectedBinding {
        membership.begin_attempt(CONN);
        membership.record_attempt_connected(CONN, epoch);
        match membership.commit_attempt(CONN) {
            AttemptCommit::Committed {
                previous_rejoin,
                replay,
            } => (previous_rejoin, replay),
            AttemptCommit::ClosedBeforeCommit => panic!("未断开的尝试必须提交成功"),
        }
    }

    /// 测试助手：投递一次断开事件并解包 `Option`。
    fn bind_closed(
        membership: &mut OfficeMembership,
        epoch: u64,
        retain_intent: bool,
    ) -> ClosedBinding {
        membership
            .bind_closed(CONN, epoch, retain_intent)
            .expect("已提交连接的事件必须被采信")
    }

    /// 初始态：未连接、无意图、无已确认。
    #[test]
    fn initial_state_is_disconnected_without_membership() {
        let membership = OfficeMembership::default();
        assert_eq!(membership.state(), OfficeMembershipState::Disconnected);
        assert_eq!(membership.confirmed_office_id(), None);
        assert_eq!(membership.conflicting_office("office-a"), None);
    }

    #[test]
    fn cancelling_an_attempt_does_not_remove_its_successor() {
        let mut membership = OfficeMembership::default();
        membership.begin_attempt(1);
        membership.begin_attempt(2);
        membership.cancel_attempt(1);
        membership.record_attempt_connected(2, 1);
        assert!(matches!(
            membership.commit_attempt(2),
            AttemptCommit::Committed { .. }
        ));
        membership.begin_attempt(3);
        membership.cancel_attempt(3);
        assert!(membership.pending_attempt.is_none());
        assert_eq!(membership.committed_connection(), Some(2));
    }

    /// Connect 把状态推进到 `Connected`；无意图时不产生重放计划。
    #[test]
    fn bind_connected_without_intent_reports_connected_only() {
        let mut membership = OfficeMembership::default();
        let (previous, replay) = bind_connected(&mut membership, 1);
        assert!(previous.is_none());
        assert!(replay.is_none(), "无意图时 MUST NOT 产生回房计划");
        assert_eq!(membership.state(), OfficeMembershipState::Connected);
    }

    /// 入房成功后状态是 `JoinedOffice`；Connect（新会话）先回退到 `Connected` 并交出重放计划。
    #[test]
    fn join_then_reconnect_yields_replay_plan_and_clears_confirmation() {
        let mut membership = OfficeMembership::default();
        let _ = bind_connected(&mut membership, 1);
        assert!(join_here(&mut membership, "office-a", "agent-1"));
        assert_eq!(
            membership.state(),
            OfficeMembershipState::JoinedOffice {
                office_id: "office-a".to_string()
            }
        );
        assert_eq!(
            membership.confirmed_office_id().as_deref(),
            Some("office-a")
        );

        // 自动重连：新会话 epoch=2 ⇒ 成员关系未经确认，须重放。
        let (previous, replay) = bind_connected(&mut membership, 2);
        assert!(previous.is_none());
        let (generation, planned) = replay.expect("意图仍在 ⇒ 必须交出重放计划");
        assert_eq!(planned, intent("office-a", "agent-1"));
        assert_eq!(
            membership.state(),
            OfficeMembershipState::Connected,
            "重放取得空 ack 之前 MUST NOT 宣称在房"
        );

        // 重放成功 ⇒ 落账；状态回到 JoinedOffice。
        assert!(membership.commit_rejoin(CONN, generation, &planned));
        assert_eq!(
            membership.state(),
            OfficeMembershipState::JoinedOffice {
                office_id: "office-a".to_string()
            }
        );
    }

    /// 回房放弃（预算耗尽 / 永久拒绝）⇒ 清空意图与已确认，状态回退到 `Connected` 且不再重试。
    #[test]
    fn abandon_rejoin_clears_intent_and_falls_back_to_connected() {
        let mut membership = OfficeMembership::default();
        let _ = bind_connected(&mut membership, 1);
        let _ = join_here(&mut membership, "office-a", "agent-1");
        let (_, replay) = bind_connected(&mut membership, 2);
        let (generation, planned) = replay.expect("replay plan");

        assert!(membership.abandon_rejoin(CONN, generation, &planned));
        assert_eq!(
            membership.state(),
            OfficeMembershipState::Connected,
            "回房失败 MUST 回退到 Connected（≠ JoinedOffice），不静默假装在线"
        );
        // 意图已清空 ⇒ 后续 Connect 不再重放（用户须手工重入）。
        let (_, replay_again) = bind_connected(&mut membership, 3);
        assert!(replay_again.is_none());
    }

    /// 陈旧结果一律不落账：世代前进（显式退房接管）或已断连（Close 生效）后，
    /// 在途回房的成功都 MUST 被丢弃。
    #[test]
    fn stale_rejoin_results_are_dropped() {
        let mut membership = OfficeMembership::default();
        let _ = bind_connected(&mut membership, 1);
        let _ = join_here(&mut membership, "office-a", "agent-1");
        let (_, replay) = bind_connected(&mut membership, 2);
        let (generation, planned) = replay.expect("replay plan");

        // 显式退房：世代前进 ⇒ 在途回房的成功不得再落账。
        let _ = membership.clear_intent();
        assert!(!membership.commit_rejoin(CONN, generation, &planned));
        assert_eq!(membership.state(), OfficeMembershipState::Connected);

        // 重新回到「重放中」的状态，再由 Close 打断 ⇒ 成功同样不得落账。
        let _ = join_here(&mut membership, "office-a", "agent-1");
        let (_, replay) = bind_connected(&mut membership, 3);
        let (generation, planned) = replay.expect("replay plan");
        let (applied, _) = bind_closed(&mut membership, 3, true);
        assert!(applied, "同 epoch 的 Close 必须生效");
        assert!(!membership.commit_rejoin(CONN, generation, &planned));
        assert_eq!(
            membership.state(),
            OfficeMembershipState::Disconnected,
            "断连后不得宣称在房"
        );
    }

    /// 陈旧 Close（epoch 不匹配）MUST 被丢弃，不得动新会话的状态或意图（#211）。
    #[test]
    fn stale_close_is_ignored() {
        let mut membership = OfficeMembership::default();
        let _ = bind_connected(&mut membership, 1);
        let _ = join_here(&mut membership, "office-a", "agent-1");
        let (_, replay) = bind_connected(&mut membership, 2);
        let (generation, planned) = replay.expect("replay plan");

        // 旧会话（epoch=1）的迟到 Close：不得清意图、不得推进世代。
        let (applied, task) = bind_closed(&mut membership, 1, false);
        assert!(!applied, "陈旧 Close 必须被丢弃");
        assert!(task.is_none());
        assert!(
            membership.commit_rejoin(CONN, generation, &planned),
            "陈旧 Close 不得使在途回房失效"
        );
    }

    /// 断连保留意图（传输层断线会自动重连）与清空意图（服务端踢出 / 手工断开）两条分支。
    #[test]
    fn close_retains_intent_only_for_transport_close() {
        let mut membership = OfficeMembership::default();
        let _ = bind_connected(&mut membership, 1);
        let _ = join_here(&mut membership, "office-a", "agent-1");

        // 传输层断线：保留意图 ⇒ 重连后重放。
        let (applied, _) = bind_closed(&mut membership, 1, true);
        assert!(applied);
        let (_, replay) = bind_connected(&mut membership, 2);
        assert!(replay.is_some(), "传输层断线 MUST 保留意图供重放");

        // 服务端踢出：清空意图 ⇒ 重连后不再重放。
        let (applied, _) = bind_closed(&mut membership, 2, false);
        assert!(applied);
        let (_, replay) = bind_connected(&mut membership, 3);
        assert!(replay.is_none(), "服务端踢出后 MUST NOT 自动重回该房");
    }

    /// 自动重连的 Close 先于 Connect 到达：关闭 epoch 是终态，不能被迟到事件复活。
    #[test]
    fn reconnect_close_before_connect_is_terminal_for_that_epoch() {
        for retain_intent in [true, false] {
            let mut membership = OfficeMembership::default();
            bind_connected(&mut membership, 1);
            assert!(join_here(&mut membership, "office-a", "agent-1"));
            assert!(membership.bind_closed(CONN, 2, retain_intent).unwrap().0);
            assert!(membership.bind_connected(CONN, 2).is_none());
            assert!(membership.bind_connected(CONN, 1).is_none());
            assert_eq!(membership.state(), OfficeMembershipState::Disconnected);
            assert_eq!(membership.confirmed_office_id(), None);

            // 后续真正的新会话仍可建立；服务端踢出则不得恢复旧意图。
            let (_, replay) = membership.bind_connected(CONN, 3).unwrap();
            assert_eq!(replay.is_some(), retain_intent);
            assert_eq!(membership.state(), OfficeMembershipState::Connected);
            assert!(!membership.bind_closed(CONN, 2, false).unwrap().0);
            assert_eq!(membership.state(), OfficeMembershipState::Connected);
        }
    }

    #[test]
    fn duplicate_connect_does_not_clear_confirmed_membership() {
        let mut membership = OfficeMembership::default();
        bind_connected(&mut membership, 1);
        assert!(join_here(&mut membership, "office-a", "agent-1"));
        assert!(membership.bind_connected(CONN, 1).is_none());
        assert_eq!(
            membership.confirmed_office_id().as_deref(),
            Some("office-a")
        );
        assert!(membership.bind_closed(CONN, 1, true).unwrap().0);
        assert!(membership.bind_connected(CONN, 1).is_none());
        assert_eq!(membership.state(), OfficeMembershipState::Disconnected);
    }

    /// 换房判据：目标房与既有成员关系不一致 ⇒ 报**当前**房号（Agent 须先显式退房）。
    #[test]
    fn conflicting_office_detects_room_switch() {
        let mut membership = OfficeMembership::default();
        let _ = bind_connected(&mut membership, 1);
        let _ = join_here(&mut membership, "office-a", "agent-1");

        assert_eq!(membership.conflicting_office("office-a"), None);
        assert_eq!(
            membership.conflicting_office("office-b").as_deref(),
            Some("office-a"),
            "换房 MUST 报当前所在房，而非被拒的目标房"
        );

        // 显式退房后不再有冲突（可在任意房重新入房）。
        let _ = membership.clear_intent();
        assert_eq!(membership.conflicting_office("office-b"), None);
    }

    /// 退避曲线：1s → 2s → 4s → 8s，此后封顶 8s（不随尝试次数继续放大）。
    #[test]
    fn backoff_schedule_doubles_then_caps() {
        let schedule: Vec<u64> = (1..=6).map(|a| backoff_delay(a).as_secs()).collect();
        assert_eq!(schedule, vec![1, 2, 4, 8, 8, 8]);
        assert_eq!(
            backoff_delay(0),
            OFFICE_REJOIN_BACKOFF_INITIAL,
            "attempt 下界防御"
        );
        assert_eq!(backoff_delay(u32::MAX), OFFICE_REJOIN_BACKOFF_MAX);
    }

    /// 预算判据边界：`elapsed + delay == budget` 仍**允许**重试（含端点），超出即停止。
    #[test]
    fn retry_budget_boundary_includes_endpoint() {
        let budget = Duration::from_secs(60);
        assert!(retry_fits_budget(
            Duration::ZERO,
            Duration::from_secs(1),
            budget
        ));
        assert!(retry_fits_budget(
            Duration::from_secs(59),
            Duration::from_secs(1),
            budget
        ));
        assert!(!retry_fits_budget(
            Duration::from_secs(60),
            Duration::from_secs(1),
            budget
        ));
        // 默认曲线下的实际取值：45s 时刻再退避 8s 仍在预算内（覆盖 socket.io 默认回收窗口后仍有尝试）。
        assert!(retry_fits_budget(
            Duration::from_secs(45),
            Duration::from_secs(8),
            budget
        ));
        assert!(!retry_fits_budget(
            Duration::from_secs(53),
            Duration::from_secs(8),
            budget
        ));
        // `0` 预算 ⇒ 单次尝试为下限（任何重试都不落在预算内）。
        assert!(!retry_fits_budget(
            Duration::ZERO,
            Duration::from_secs(1),
            Duration::ZERO
        ));
    }

    /// 拒绝码分类：仅 `4101` / `4105` 可重试；`4106` / `400` / `403` / 未知码 / 未获裁决 /
    /// 传输层错误一律永久失败。
    #[test]
    fn only_transient_room_conflicts_are_retryable() {
        for code in TRANSIENT_ROOM_CONFLICT_CODES {
            assert_eq!(
                classify_rejoin_error(&protocol_error(code)),
                RejoinVerdict::TransientConflict,
                "code {code} 是重连恢复路径上的瞬态冲突，必须可重试"
            );
        }
        for code in [400, 403, 4103, 4104, 4106, 4299, -1] {
            assert_eq!(
                classify_rejoin_error(&protocol_error(code)),
                RejoinVerdict::Permanent,
                "code {code} 重试不改变结果，必须判定为永久失败"
            );
        }
        // 传输层错误（超时 / 断连）同样不重试：调用方须如实报错，由下一次 Connect 钩子接管。
        assert_eq!(
            classify_rejoin_error(&SmcpAgentError::Timeout),
            RejoinVerdict::Permanent
        );
        assert_eq!(
            classify_rejoin_error(&SmcpAgentError::connection("lost")),
            RejoinVerdict::Permanent
        );
    }

    /// 在途回房句柄的登记受 `is_current` 守卫：世代前进后登记失败（调用方须立即 abort）。
    #[tokio::test]
    async fn rejoin_task_registration_is_generation_bound() {
        let mut membership = OfficeMembership::default();
        let _ = bind_connected(&mut membership, 1);
        let _ = join_here(&mut membership, "office-a", "agent-1");
        let (_, replay) = bind_connected(&mut membership, 2);
        let (generation, planned) = replay.expect("replay plan");

        let task = tokio::spawn(async { std::future::pending::<()>().await });
        let handle = task.abort_handle();
        assert!(
            membership.set_rejoin_task(CONN, generation, &planned, handle.clone()),
            "新鲜世代必须登记成功"
        );
        // 显式退房 ⇒ 世代前进且意图改写 ⇒ 旧世代的句柄不得再入册。
        let _ = membership.clear_intent();
        assert!(!membership.set_rejoin_task(CONN, generation, &planned, handle));
    }

    /// ❌B1 回归：显式入房的 ack 只在**发起时的会话仍是当前会话**时才落账。
    ///
    /// 传输层回调与 ack 分发无执行顺序契约（内核各自成 task），等待 ack 期间可能夹进 Close；此时
    /// 「入房成功」陈述的是**已死会话**的事实，落账会造成「namespace 已断开却报 `JoinedOffice`」
    /// （违反「不静默假装在线」）。
    #[test]
    fn commit_join_is_discarded_when_the_session_was_replaced() {
        let mut membership = OfficeMembership::default();
        let _ = bind_connected(&mut membership, 1);
        // 显式入房发起前的会话标识。
        let session = membership.session_token();

        // 等待 ack 期间传输层断开（Close 生效：作废世代、清已确认）。
        let (applied, _) = bind_closed(&mut membership, 1, true);
        assert!(applied);

        let (committed, pending) = membership.commit_join(session, intent("office-a", "agent-1"));
        assert!(!committed, "已死会话的入房 ack MUST 被丢弃");
        assert!(pending.is_none());
        assert_eq!(
            membership.state(),
            OfficeMembershipState::Disconnected,
            "丢弃后状态必须是 Disconnected，绝非 JoinedOffice"
        );
        assert_eq!(
            membership.confirmed_office_id(),
            None,
            "MUST NOT 落账已确认成员关系"
        );
    }

    /// ❌B1 同族：陈旧 ack 也不得**复活**期间被显式退房清掉的意图。
    ///
    /// 若只把陈旧 ack 判成「不落 `confirmed`、但落 `desired`」，则在「join 在途 + 并发显式退房」下会把
    /// 调用方刚撤销的意图写回，使下一次重连自动重回他主动退出的房——与调用方意图相反。
    #[test]
    fn stale_join_ack_does_not_resurrect_a_cleared_intent() {
        let mut membership = OfficeMembership::default();
        let _ = bind_connected(&mut membership, 1);
        let session = membership.session_token();

        // 期间调用方显式退房（清空意图、作废世代）。
        let _ = membership.clear_intent();

        let (committed, _) = membership.commit_join(session, intent("office-a", "agent-1"));
        assert!(!committed);
        // 意图未被复活 ⇒ 重连不产生任何回放。
        let (_, replay) = bind_connected(&mut membership, 2);
        assert!(
            replay.is_none(),
            "陈旧入房 ack MUST NOT 复活被显式撤销的意图"
        );
    }

    /// ❌B1 边界：新 Connect（新会话）同样使在途显式入房的 ack 失效。
    #[test]
    fn commit_join_is_discarded_after_a_new_session_binds() {
        let mut membership = OfficeMembership::default();
        let _ = bind_connected(&mut membership, 1);
        let session = membership.session_token();

        // 重连产生新会话：epoch 与世代一并前进。
        let (_, _replay) = bind_connected(&mut membership, 2);

        let (committed, _) = membership.commit_join(session, intent("office-a", "agent-1"));
        assert!(!committed, "会话已更替，旧会话的 ack MUST 被丢弃");
    }

    /// ❌B2 回归：**不在册**时不得落账成员关系。
    ///
    /// `connect()` 在首个 `Connected` 状态绑定完成后才返回（配合的守卫在 `async_agent`），故本断言是
    /// 不变量下界：`confirmed` 一经落账即代表「会话在册且服务端已确认」，`confirmed_office_id()` 与
    /// `state()` 的对外语义因此不会自相矛盾（不会出现「有 confirmed 却报 Disconnected」）。
    #[test]
    fn commit_join_requires_a_connected_session() {
        let mut membership = OfficeMembership::default();
        // 尚未处理首个 Connected：connected == false。
        let session = membership.session_token();
        let (committed, _) = membership.commit_join(session, intent("office-a", "agent-1"));
        assert!(
            !committed,
            "未在册（connected == false）时 MUST NOT 落账成员关系"
        );
        assert_eq!(membership.confirmed_office_id(), None);
        assert_eq!(membership.state(), OfficeMembershipState::Disconnected);
    }

    /// ❌W3 回归：**旧世代**的「回房放弃」不得清掉新意图（不误报「失去成员关系」）。
    #[test]
    fn stale_abandon_rejoin_keeps_the_new_intent() {
        let mut membership = OfficeMembership::default();
        let _ = bind_connected(&mut membership, 1);
        let _ = join_here(&mut membership, "office-a", "agent-1");
        let (_, replay) = bind_connected(&mut membership, 2);
        let (generation, planned) = replay.expect("replay plan");

        // 期间由显式操作接管：世代前进、意图改写为 office-b。
        let _ = membership.clear_intent();
        let _ = join_here(&mut membership, "office-b", "agent-1");

        // 旧世代回房放弃（例如预算耗尽）⇒ 必须被丢弃，不得清掉新意图、不得回退状态。
        assert!(
            !membership.abandon_rejoin(CONN, generation, &planned),
            "陈旧世代的放弃 MUST 不生效"
        );
        assert_eq!(
            membership.confirmed_office_id().as_deref(),
            Some("office-b"),
            "新意图/已确认不得被旧世代的放弃清掉"
        );
        assert_eq!(
            membership.state(),
            OfficeMembershipState::JoinedOffice {
                office_id: "office-b".to_string()
            }
        );
    }

    /// ❌W4 回归：显式入房成功时**取出并交出**在途回房句柄（供调用方 abort），且登记槽清空。
    #[tokio::test]
    async fn commit_join_takes_over_the_pending_rejoin_handle() {
        let mut membership = OfficeMembership::default();
        let _ = bind_connected(&mut membership, 1);
        let _ = join_here(&mut membership, "office-a", "agent-1");
        // 制造一个在途回房（世代 2 的重放计划）并登记其句柄。
        let (_, replay) = bind_connected(&mut membership, 2);
        let (generation, planned) = replay.expect("replay plan");
        let task = tokio::spawn(async { std::future::pending::<()>().await });
        let handle = task.abort_handle();
        assert!(membership.set_rejoin_task(CONN, generation, &planned, handle.clone()));

        // 显式入房接管：必须交回在途句柄（调用方据此 abort），否则旧回房会继续抢房。
        let session = membership.session_token();
        let (committed, pending) = membership.commit_join(session, intent("office-a", "agent-1"));
        assert!(committed);
        let pending = pending.expect("显式入房 MUST 交出在途回房句柄");
        pending.abort();
        task.await.expect_err("被接管的回房 task 必须已 abort");

        // 登记槽已清空 ⇒ 后续不再持有陈旧句柄。
        let (_, replay) = bind_connected(&mut membership, 3);
        assert!(replay.is_some(), "意图仍在 ⇒ 新世代照常产生重放计划");
    }

    /// 「不静默假装在线」的兜底防线：`connected == false` 时无论 `confirmed` 是否有余留，状态一律报
    /// `Disconnected`——成员关系属于会话，会话不在册就不存在「在房」这一事实。
    #[test]
    fn state_never_reports_joined_office_while_disconnected() {
        let mut membership = OfficeMembership::default();
        let _ = bind_connected(&mut membership, 1);
        assert!(join_here(&mut membership, "office-a", "agent-1"));
        assert_eq!(
            membership.state(),
            OfficeMembershipState::JoinedOffice {
                office_id: "office-a".to_string()
            }
        );

        // 服务端踢出（不保留意图）⇒ 已确认被清、连接不在册。
        let (applied, _) = bind_closed(&mut membership, 1, false);
        assert!(applied);
        assert_eq!(membership.confirmed_office_id(), None);
        assert_eq!(membership.state(), OfficeMembershipState::Disconnected);
    }

    /// ❌B3 回归：**未提交连接**的事件不得改写既有连接的成员状态（`connect()` 事务性的状态面）。
    ///
    /// 生命周期回调与 `connect()` 提交点是并发执行的：新连接可能先报 `Connected` / `Closed`。未提交
    /// 连接的事件必须被原样忽略，否则一次**失败**的重复 `connect()` 会清掉既有连接的已确认状态。
    #[test]
    fn events_of_an_uncommitted_connection_are_ignored() {
        const OTHER: u64 = 99;
        let mut membership = OfficeMembership::default();
        let _ = bind_connected(&mut membership, 1);
        assert!(join_here(&mut membership, "office-a", "agent-1"));

        // 未提交连接（OTHER）的 Connected / Closed 事件：一律忽略，状态与已确认不变。
        assert!(
            membership.bind_connected(OTHER, 1).is_none(),
            "未提交连接的 Connected MUST 被忽略"
        );
        assert!(
            membership.bind_closed(OTHER, 1, false).is_none(),
            "未提交连接的 Closed MUST 被忽略（不得清意图 / 不得改状态）"
        );
        assert_eq!(
            membership.confirmed_office_id().as_deref(),
            Some("office-a"),
            "既有连接的成员关系必须原封不动"
        );
        assert_eq!(
            membership.state(),
            OfficeMembershipState::JoinedOffice {
                office_id: "office-a".to_string()
            }
        );
    }

    /// ❌B3 同族：连接被替换后，**旧连接**的迟到事件同样不得改写新连接的状态。
    ///
    /// 各连接的 `session_epoch` 各自从 1 起算，故 epoch 相同的旧事件若只按 epoch 过滤会被误采信——
    /// 归属判据必须是连接序号。
    #[test]
    fn events_of_a_superseded_connection_are_ignored() {
        const NEXT: u64 = 8;
        let mut membership = OfficeMembership::default();
        let _ = bind_connected(&mut membership, 1);
        assert!(join_here(&mut membership, "office-a", "agent-1"));

        // 换连接：新连接的**首个会话 epoch 同样是 1**（内核按连接各自计数）。
        membership.begin_attempt(NEXT);
        membership.record_attempt_connected(NEXT, 1);
        let (_, replay) = match membership.commit_attempt(NEXT) {
            AttemptCommit::Committed {
                previous_rejoin,
                replay,
            } => (previous_rejoin, replay),
            AttemptCommit::ClosedBeforeCommit => panic!("新连接应提交成功"),
        };
        let (generation, planned) = replay.expect("意图仍在 ⇒ 有重放计划");
        assert!(
            membership.commit_rejoin(NEXT, generation, &planned),
            "新连接的回房结果必须落账"
        );

        // 旧连接（CONN）的迟到事件：epoch 同为 1，但连接已不是当前 ⇒ 必须忽略。
        assert!(
            membership.bind_closed(CONN, 1, false).is_none(),
            "旧连接的迟到 Closed MUST 被忽略"
        );
        assert_eq!(
            membership.confirmed_office_id().as_deref(),
            Some("office-a"),
            "新连接的已确认成员关系不得被旧连接的事件清掉"
        );
    }

    /// ❌B3''：旧连接的**在途回房**结果不得落进新连接。
    ///
    /// task 的 `abort()` 是异步的、不能当同步屏障，故回房结果的落账判据必须含**连接归属**。本用例特意
    /// 用**同一个世代**给旧连接伪造一份落账：世代判据挡不住它，只有连接归属判据能挡住。
    #[tokio::test]
    async fn rejoin_results_are_bound_to_the_connection() {
        const NEXT: u64 = 8;
        let mut membership = OfficeMembership::default();
        let _ = bind_connected(&mut membership, 1);
        assert!(join_here(&mut membership, "office-a", "agent-1"));

        // 换连接（原子提交）：新连接的**首个会话 epoch 同样是 1**。
        membership.begin_attempt(NEXT);
        membership.record_attempt_connected(NEXT, 1);
        let (_, replay) = match membership.commit_attempt(NEXT) {
            AttemptCommit::Committed {
                previous_rejoin,
                replay,
            } => (previous_rejoin, replay),
            AttemptCommit::ClosedBeforeCommit => panic!("新连接应提交成功"),
        };
        let (generation, planned) = replay.expect("意图仍在 ⇒ 有重放计划");

        // 用**当前世代**伪造旧连接（CONN）的落账：世代判据通过，只有连接归属判据能挡住。
        assert!(
            !membership.commit_rejoin(CONN, generation, &planned),
            "同世代但连接已替换 ⇒ 旧连接的回房结果 MUST NOT 落账"
        );
        assert!(
            !membership.abandon_rejoin(CONN, generation, &planned),
            "同世代但连接已替换 ⇒ 旧连接的回房放弃 MUST NOT 清掉状态"
        );
        assert!(
            !membership.set_rejoin_task(
                CONN,
                generation,
                &planned,
                tokio::spawn(async {}).abort_handle()
            ),
            "同世代但连接已替换 ⇒ 旧连接不得再登记在途回房句柄"
        );

        // 正对照：新连接的同类操作照常生效。
        assert!(
            membership.commit_rejoin(NEXT, generation, &planned),
            "新连接的回房结果必须落账"
        );
    }

    /// ❌B3''①：`Connected → Closed → 提交` ⇒ 提交点必须**拒绝**（已到达的断开不得被丢失）。
    #[test]
    fn commit_attempt_rejects_a_connection_closed_before_commit() {
        let mut membership = OfficeMembership::default();
        membership.begin_attempt(CONN);
        membership.record_attempt_connected(CONN, 1);
        membership.record_attempt_closed(CONN);
        assert!(
            matches!(
                membership.commit_attempt(CONN),
                AttemptCommit::ClosedBeforeCommit
            ),
            "提交前已断开 ⇒ MUST NOT 提交（否则把死连接报成「已连接」）"
        );
        assert_eq!(membership.state(), OfficeMembershipState::Disconnected);
        assert_eq!(membership.committed_connection(), None);
    }

    /// ❌B3''① 回归：同一尝试内**已观察到的断开是单调的**——`Connected → Closed → Connected` 仍须拒绝提交。
    ///
    /// 内核 Connect / Close 回调是各自独立 task、投递顺序无契约，故「后到的 Connected 覆盖先到的 Closed」
    /// 无法从原理上排除。此时按「宁严勿宽」拒绝提交（调用方可重试），而不是冒险把可能已死的连接报成
    /// 「已连接」。
    #[test]
    fn observed_close_is_monotonic_within_an_attempt() {
        let mut membership = OfficeMembership::default();
        membership.begin_attempt(CONN);
        membership.record_attempt_connected(CONN, 1);
        membership.record_attempt_closed(CONN);
        // 同一连接的「再次 Connected」（自动重连出的新会话，或乱序投递）不得复活记录。
        membership.record_attempt_connected(CONN, 2);
        assert!(
            matches!(
                membership.commit_attempt(CONN),
                AttemptCommit::ClosedBeforeCommit
            ),
            "已观察到断开的尝试 MUST NOT 被后续 Connected 复活"
        );
        assert_eq!(membership.committed_connection(), None);
    }

    /// ❌B3''① 回归：**上一次尝试的迟到事件**不得覆盖当前尝试的在途记录。
    ///
    /// 上一个 `connect()` 尝试（如已超时）虽已 abort，其回调仍可能在途；若它覆盖当前记录，当前的
    /// `connect()` 会被误判为「提交前已断开」而失败。
    #[test]
    fn late_connected_of_a_previous_attempt_does_not_clobber_the_current_one() {
        const PREVIOUS: u64 = 6;
        let mut membership = OfficeMembership::default();
        // 当前尝试先记录，随后上一次尝试的迟到 Connected 到达。
        membership.begin_attempt(CONN);
        membership.record_attempt_connected(CONN, 1);
        membership.record_attempt_connected(PREVIOUS, 1);
        assert!(
            matches!(
                membership.commit_attempt(CONN),
                AttemptCommit::Committed { .. }
            ),
            "当前尝试的记录 MUST NOT 被上一次尝试的迟到事件覆盖"
        );
        assert_eq!(membership.committed_connection(), Some(CONN));
        assert_eq!(membership.state(), OfficeMembershipState::Connected);
    }

    /// ❌B3''① 同族：无尝试记录 / 归属不符一律拒绝（宁严勿宽，绝不放行未知状态）。
    #[test]
    fn commit_attempt_rejects_unknown_or_mismatched_attempts() {
        const NEXT: u64 = 8;
        let mut membership = OfficeMembership::default();
        assert!(matches!(
            membership.commit_attempt(CONN),
            AttemptCommit::ClosedBeforeCommit
        ));

        membership.begin_attempt(NEXT);
        membership.record_attempt_connected(NEXT, 1);
        assert!(
            matches!(
                membership.commit_attempt(CONN),
                AttemptCommit::ClosedBeforeCommit
            ),
            "尝试归属不符 ⇒ MUST NOT 提交"
        );
        assert_eq!(membership.committed_connection(), None);
    }

    /// ❌B3''① 残余路径回归：`Closed` **先于**首个 `Connected` 到达时不得被丢弃。
    ///
    /// 归属先行（`begin_attempt`）使断开记录有处安放；若仍按「有 pending 才记录」的老写法，这条 Close 会
    /// 被丢掉，随后 `Connected` 把槽位写成「待提交」，一条已断开的连接就会被提交。
    #[test]
    fn close_before_the_first_connected_is_not_dropped() {
        let mut membership = OfficeMembership::default();
        membership.begin_attempt(CONN);
        // 顺序：Closed → Connected（内核 Connect/Close 回调顺序无契约）。
        membership.record_attempt_closed(CONN);
        membership.record_attempt_connected(CONN, 1);
        assert!(
            matches!(
                membership.commit_attempt(CONN),
                AttemptCommit::ClosedBeforeCommit
            ),
            "先到的 Closed 不得被后到的 Connected 抹掉"
        );
        assert_eq!(membership.committed_connection(), None);
        assert_eq!(membership.state(), OfficeMembershipState::Disconnected);
    }
}
