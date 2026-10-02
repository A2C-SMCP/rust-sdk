//! 会话管理模块 / Session management module

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use thiserror::Error;

// 类型别名
pub type OfficeId = String;
pub type SessionId = String;

/// 会话错误类型
#[derive(Error, Debug, serde::Serialize)]
pub enum SessionError {
    #[error("Session not found: {0}")]
    NotFound(String),
    #[error("Agent already in room: {0}")]
    AgentAlreadyInRoom(OfficeId),
    /// 目标房**该 role 的席位**已被其它会话占据（协议 `4101 Room Full`）。
    ///
    /// 泛化自旧的 `AgentAlreadyExists`（protocol#66「每 role 一席」）：Agent / Computer 同码同形，
    /// 由 `details.role`（取自本变体）标明被占席位。[`SessionError::error_code`] 映射为 `4101`。
    #[error("Seat already taken in room for role {0:?}")]
    SeatTaken(ClientRole),
    #[error("Invalid session state: {0}")]
    InvalidState(String),
}

impl SessionError {
    /// 获取错误码 / Get error code
    pub fn error_code(&self) -> i32 {
        match self {
            SessionError::NotFound(_) => smcp::error_codes::NOT_FOUND,
            SessionError::AgentAlreadyInRoom(_) => smcp::error_codes::ALREADY_IN_ROOM,
            SessionError::SeatTaken(_) => smcp::error_codes::ROOM_FULL,
            SessionError::InvalidState(_) => smcp::error_codes::BAD_REQUEST,
        }
    }
}

/// 客户端角色
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ClientRole {
    Agent,
    Computer,
}

impl From<smcp::Role> for ClientRole {
    fn from(role: smcp::Role) -> Self {
        match role {
            smcp::Role::Agent => ClientRole::Agent,
            smcp::Role::Computer => ClientRole::Computer,
        }
    }
}

impl From<ClientRole> for smcp::Role {
    fn from(role: ClientRole) -> Self {
        match role {
            ClientRole::Agent => smcp::Role::Agent,
            ClientRole::Computer => smcp::Role::Computer,
        }
    }
}

impl std::fmt::Display for ClientRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientRole::Agent => write!(f, "agent"),
            ClientRole::Computer => write!(f, "computer"),
        }
    }
}

/// 会话数据
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionData {
    /// 会话 ID
    pub sid: SessionId,
    /// 客户端名称
    pub name: String,
    /// 客户端角色
    pub role: ClientRole,
    /// 当前所在的办公室 ID
    pub office_id: Option<OfficeId>,
    /// 握手协商到的协议版本号（来自连接 URL query `a2c_version`）。
    /// 仅用于 `server:list_room` 展示与诊断；缺省（旧连接未协商）时为 `None`。
    /// 对齐 Python `session["a2c_version"]`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub a2c_version: Option<String>,
    /// 其他扩展数据
    pub extra: serde_json::Value,
}

impl SessionData {
    /// 创建新的会话数据
    pub fn new(sid: SessionId, name: String, role: ClientRole) -> Self {
        Self {
            sid,
            name,
            role,
            office_id: None,
            a2c_version: None,
            extra: serde_json::Value::Object(Default::default()),
        }
    }

    /// 设置办公室 ID
    pub fn with_office_id(mut self, office_id: OfficeId) -> Self {
        self.office_id = Some(office_id);
        self
    }

    /// 设置握手协商到的协议版本号 / Set the negotiated protocol version (from handshake)
    pub fn with_a2c_version(mut self, a2c_version: Option<String>) -> Self {
        self.a2c_version = a2c_version;
        self
    }

    /// 设置扩展数据
    pub fn with_extra(mut self, extra: serde_json::Value) -> Self {
        self.extra = extra;
        self
    }
}

/// 入房事务对 Socket.IO 成员关系的裁决 / What the join transaction decided for room membership.
///
/// 由 [`SessionManager::reserve_join`] 产出：会话状态（含**席位**提交）已在此刻落定，剩下的
/// `socket.join` / `socket.leave` / 广播属**表层生效**，按本裁决执行即可。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoinDecision {
    /// 已在目标房（同一会话重复入房）：幂等，无成员关系变更。
    Noop,
    /// 加入目标房。
    Join,
    /// Computer 换房：先退旧房（并按协议向旧房广播 `notify:leave_office`），再入目标房。
    ///
    /// 该动作排在**所有**可能失败的闸门之后（协议 room-model.md 注记 4「校验必须先于副作用」）：
    /// 若先退旧房再查目标房，一旦目标房席位被占，该 Computer 已离开原房且对端已收到离开通知，
    /// 落成「无房」中间态却无补救语义。
    LeaveAndJoin {
        /// 需要退出的旧房 / the room being left。
        leave_office: OfficeId,
    },
}

/// [`SessionManager::reserve_join`] 的提交结果 / Outcome of a committed join reservation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinReservation {
    /// 表层需要执行的成员关系动作 / membership action to apply。
    pub decision: JoinDecision,
    /// 转换**之前**会话所在的房：`4106` 的 `details.office_id` 与旧房广播都取自它。
    /// The session's room before the transition.
    pub previous_office: Option<OfficeId>,
}

/// [`SessionManager::commit_leave`] 的提交结果 / Outcome of a leave commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaveCommit {
    /// 会话本就不在任何房：幂等成功。
    AlreadyIdle,
    /// 本次调用释放了该房（会话 `office_id` 已置空）。
    Released(OfficeId),
    /// 会话在广播期间被并发转换接管，现处于 `current` 房：本次调用**不**改动会话，
    /// 调用方须按 `current` 收敛 Socket.IO 成员关系。
    Superseded {
        /// 会话当前的房 / the session's current room。
        current: OfficeId,
    },
}

/// 会话管理器
///
/// 为什么需要临界区：房间归属转换是「检查-提交」事务——「目标房该 role 的席位是否已被其它会话占据」
/// 的判据是**扫描 `sessions`**（席位即 `(office_id, role)`）。
///
/// 单表扫描自身不构成「检查-提交」原子性：两个不同名 Agent / 两台 Computer 并发加入同一空房时
/// 双方都扫描不到对方，于是双双成功（#226 P0-2）。故：**读**走 DashMap 无锁路径（`client:*` 路由与
/// `list_room` 每帧查表，热点在读）；**写**（注册 / 入房 / 退房 / 注销）一律经
/// `SessionManager::transition()` 临界区串行化。临界区**只覆盖同步段**（内含零 `.await`），转换是
/// 短操作，故用 `std::sync::Mutex`。
///
/// **名字注册表已随 protocol#66 删除**：`(office_id, role, name)` 唯一性由「每 role 一席」蕴含
/// （房内同 role 至多一个会话），独立的 name → sid 表失去全部生产性消费者，且其自带的
/// 「预留无人释放 / 残留键永久占死」缺陷类（#226 P1-4）随之整体消失。房内按名解析路由
/// （[`Self::get_computer_sid_in_office`]）一直走 `sessions` 扫描，不依赖该表。
///
#[derive(Debug)]
pub struct SessionManager {
    /// sid -> session_data 映射
    sessions: Arc<DashMap<SessionId, SessionData>>,
    /// 房间归属转换的事务锁（见结构体文档；**勿**跨 `.await` 持有）。
    transition: Arc<Mutex<()>>,
}

impl SessionManager {
    /// 创建新的会话管理器
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(DashMap::new()),
            transition: Arc::new(Mutex::new(())),
        }
    }

    /// 进入房间归属转换临界区 / Enter the room-transition critical section.
    ///
    /// 毒化锁按「临界区只做内存表操作、最坏是本次转换未完成」处理，取回内部 guard 继续；
    /// 毒化标记本身不携带可用信息。Poisoning carries no actionable state here.
    fn transition(&self) -> std::sync::MutexGuard<'_, ()> {
        self.transition
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 注册会话（幂等；**绝不覆盖**既有记录）。
    ///
    /// 同一 sid 重复注册返回 `Ok(())` 且保留原记录：身份（role / name）在一次连接内**不可变更**，
    /// 声明与既有会话不符必须由 handler 判 `403`（协议 events.md §server:join_office），
    /// 而不是在这里把旧记录悄悄换掉。
    ///
    /// Register a session; re-registering the same sid keeps the original record.
    ///
    /// ⚠️ 本方法是底层「置入」原语，**不是**入房闸门——席位（每 role 一席）与房间归属**只能**经
    /// [`Self::reserve_join`] 获得；既有测试用它直接布置房间态。
    pub fn register_session(&self, session: SessionData) -> Result<(), SessionError> {
        let _guard = self.transition();
        if let Some(existing) = self.sessions.get(&session.sid) {
            tracing::debug!(
                "Session {} already registered as {}/{}; keeping the existing identity",
                session.sid,
                existing.role,
                existing.name
            );
            return Ok(());
        }

        self.sessions.insert(session.sid.clone(), session.clone());

        tracing::debug!("Registered session: {} -> {}", session.name, session.sid);
        Ok(())
    }

    /// 注销会话（席位随会话记录一并释放）
    pub fn unregister_session(&self, sid: &SessionId) -> Option<SessionData> {
        let _guard = self.transition();
        let session = self.sessions.remove(sid)?;

        tracing::debug!("Unregistered session: {} -> {}", session.1.name, sid);
        Some(session.1)
    }

    /// 获取会话数据
    pub fn get_session(&self, sid: &SessionId) -> Option<SessionData> {
        self.sessions.get(sid).map(|s| s.clone())
    }

    /// 取或建会话（原子）：`sid` 已存在则**原样返回**既有记录，否则按入参建一条**无房**会话。
    ///
    /// 与「先 `get_session` 再 `register_session`」的**非原子**两步式的区别：两个并发 `join` 拿不到
    /// 各自的「查无」结论再互相覆盖——查与建在同一临界区内完成，返回**权威**记录，调用方据其做身份
    /// 一致性判定（不符 ⇒ `403`）。
    ///
    /// 新会话必然无 `office_id`：房间归属**只能**经 [`Self::reserve_join`] 获得。故本方法不触碰
    /// 席位（无房会话不占任何 `(office_id, role)`），也就没有「占了却无人释放」的失败模式。
    ///
    /// Atomically get-or-create the session for `sid`; the returned record is authoritative, and a
    /// freshly created session is always room-less because room ownership is granted only by
    /// [`Self::reserve_join`].
    pub fn get_or_register_session(
        &self,
        sid: SessionId,
        name: String,
        role: ClientRole,
        a2c_version: Option<String>,
    ) -> SessionData {
        let _guard = self.transition();
        if let Some(existing) = self.sessions.get(&sid) {
            return existing.clone();
        }
        let created = SessionData::new(sid, name, role).with_a2c_version(a2c_version);
        self.sessions.insert(created.sid.clone(), created.clone());
        tracing::debug!("Registered session: {} -> {}", created.name, created.sid);
        created
    }

    /// 入房事务：**校验 → 提交**全部在一次临界区内完成。
    ///
    /// 取代原先「扫描既有会话 + `update_office_id` 写入」的两步式——两步式在两次调用之间给了并发
    /// 窗口，使「每 role 一席」这类跨键不变量可被绕过（#226 P0-2）。本方法把闸门与提交放进同一
    /// 临界区，故检查-提交是原子的。
    ///
    /// 闸门按协议 room-model.md §加入房间 / events.md §server:join_office 的规则与顺序（protocol#66
    /// 「每 role 一席」）：
    ///
    /// | 角色 | 既有房 | 闸门 | 结果 |
    /// |---|---|---|---|
    /// | Agent | 其它房 | Agent 换房须显式两步 | [`SessionError::AgentAlreadyInRoom`]（`4106`）|
    /// | 两者 | 本房 | 同一会话重复入房幂等 | [`JoinDecision::Noop`] |
    /// | Agent | 无 | 目标房已有 Agent（非本会话）| [`SessionError::SeatTaken`]（`4101`）|
    /// | Computer | 其它房 | 目标房已有 Computer（非本会话）| [`SessionError::SeatTaken`]（`4101`），否则 [`JoinDecision::LeaveAndJoin`] |
    /// | Computer | 无 | 目标房已有 Computer（非本会话）| [`SessionError::SeatTaken`]（`4101`），否则 [`JoinDecision::Join`] |
    ///
    /// **席位检查先于一切副作用**（协议 room-model.md 注记 4「校验必须先于副作用」）：Computer 的
    /// 席位检查排在「自动离开旧房」**之前**——否则一旦席位被占，该 Computer 已离开原房且原房成员已收到
    /// `notify:leave_office`，落成「无房」的中间态却无补救语义（场景 #4 / #6）。
    ///
    /// **席位检查 + 占席对同一 `office_id` 原子**（注记 3）：二者同处本次 `transition()` 临界区，故并发
    /// 加入空房仅一者通过（恰一个 `4101`，场景 #8）。失败路径在任何写入之前返回，绝不留下半提交状态。
    ///
    /// Validate-then-commit as one atomic step; the per-role seat check runs before any write and,
    /// for Computers, before the auto-leave side effect.
    pub fn reserve_join(
        &self,
        sid: &SessionId,
        office_id: &str,
    ) -> Result<JoinReservation, SessionError> {
        let _guard = self.transition();
        let session = self
            .sessions
            .get(sid)
            .map(|s| s.clone())
            .ok_or_else(|| SessionError::NotFound(sid.clone()))?;
        let previous_office = session.office_id.clone();

        // 席位闸门：目标房该 role 的席位已被**其它会话**占据 ⇒ 4101（自会话由 Noop 分支先行短路）。
        // 判据是「每 role 一席」本身，与 name 无关（protocol#66；`(office_id, role, name)` 唯一性由
        // 本规则蕴含）——同 role 的第二个会话同名与否都撞此处。
        let seat_taken = |role: &ClientRole| -> Result<(), SessionError> {
            if self.role_in_office(office_id, role, Some(sid)) {
                Err(SessionError::SeatTaken(role.clone()))
            } else {
                Ok(())
            }
        };

        let decision = match session.role {
            ClientRole::Agent => match previous_office.as_deref() {
                Some(current) if current != office_id => {
                    // Agent 换房 MUST 是显式两步：静默迁房会让它在未预期时刻离开原房，
                    // 与原房 Computer 的在途交互随之中断且无显式终态（协议 §4106）。
                    return Err(SessionError::AgentAlreadyInRoom(current.to_string()));
                }
                Some(_) => JoinDecision::Noop,
                None => {
                    seat_taken(&ClientRole::Agent)?;
                    JoinDecision::Join
                }
            },
            ClientRole::Computer => match previous_office.as_deref() {
                Some(current) if current == office_id => JoinDecision::Noop,
                Some(current) => {
                    seat_taken(&ClientRole::Computer)?;
                    JoinDecision::LeaveAndJoin {
                        leave_office: current.to_string(),
                    }
                }
                None => {
                    seat_taken(&ClientRole::Computer)?;
                    JoinDecision::Join
                }
            },
        };

        if decision != JoinDecision::Noop {
            self.commit_office(&session, office_id);
        }
        Ok(JoinReservation {
            decision,
            previous_office,
        })
    }

    /// 提交房间归属（仅在 [`Self::reserve_join`] 的闸门全部通过后调用）。
    ///
    /// 席位由 `office_id` 字段蕴含（`(office_id, role)` 即席位），故提交 = 写权威 `office_id`；
    /// 席位释放 = 该字段置空（[`Self::commit_leave`]）。**无失败路径**：闸门已把全部拒绝条件
    /// 判完才允许到达这里。
    fn commit_office(&self, session: &SessionData, office_id: &str) {
        let target_office = office_id.to_string();
        if let Some(mut record) = self.sessions.get_mut(&session.sid) {
            record.office_id = Some(target_office);
        }
    }

    /// 退房事务（幂等；对标协议 events.md §server:leave_office）。
    ///
    /// - 会话无房 ⇒ [`LeaveCommit::AlreadyIdle`]：**幂等成功**，不产生错误码；
    /// - 会话仍在 `broadcast_office` ⇒ [`LeaveCommit::Released`]：释放该房**席位**（清空 `office_id`）；
    /// - 会话已被并发转换改到**别的房** ⇒ [`LeaveCommit::Superseded`]：**不改动会话**，把新状态如实
    ///   回报给调用方去收敛 Socket.IO 成员关系。
    ///
    /// 第三分支是 #226 P0-3 的根治点：旧实现无条件 `update_office_id(None)`，于是在它 `await` 广播期间
    /// 完成的并发 `join` 会被**倒着覆盖**——会话被清成无房，socket 却留在新房，成为继续收 `notify:*`
    /// 的幽灵成员。**提交点**语义（「只在状态仍是我观测到的那份时才提交」）使这种覆盖不可表达。
    ///
    /// Commit-point re-read: the leave only commits while the session still sits in the room the
    /// caller broadcast to; anything newer wins and is merely reported back for convergence.
    pub fn commit_leave(
        &self,
        sid: &SessionId,
        broadcast_office: Option<&str>,
    ) -> Result<LeaveCommit, SessionError> {
        let _guard = self.transition();
        let session = self
            .sessions
            .get(sid)
            .map(|s| s.clone())
            .ok_or_else(|| SessionError::NotFound(sid.clone()))?;

        let Some(current) = session.office_id.clone() else {
            return Ok(LeaveCommit::AlreadyIdle);
        };
        if broadcast_office != Some(current.as_str()) {
            return Ok(LeaveCommit::Superseded { current });
        }

        // 释放席位 = 清空 `office_id`（席位由 `(office_id, role)` 蕴含，无独立名表可清）。
        if let Some(mut record) = self.sessions.get_mut(sid) {
            record.office_id = None;
        }
        Ok(LeaveCommit::Released(current))
    }

    /// 获取指定办公室内的所有会话
    pub fn get_sessions_in_office(&self, office_id: &OfficeId) -> Vec<SessionData> {
        self.sessions
            .iter()
            .filter(|s| s.office_id.as_ref() == Some(office_id))
            .map(|s| s.clone())
            .collect()
    }

    /// 检查房间内是否已有 Agent
    pub fn has_agent_in_office(&self, office_id: &OfficeId) -> bool {
        self.role_in_office(office_id, &ClientRole::Agent, None)
    }

    /// 检查房间内是否已有指定 role 的会话，**排除**指定 sid（同一会话重复入房不算占用）。
    ///
    /// 席位 = `(office_id, role)`：本扫描即「每 role 一席」的判据本身。只读扫描，**并发安全**依赖
    /// 调用点：需要「检查-提交」原子性的场景必须在 [`Self::reserve_join`] 的临界区内调用，单独调用
    /// 本方法不构成闸门（#226 P0-2 的原形成因）。
    fn role_in_office(
        &self,
        office_id: &str,
        role: &ClientRole,
        excluded_sid: Option<&SessionId>,
    ) -> bool {
        self.sessions.iter().any(|s| {
            excluded_sid.map(|sid| sid.as_str()) != Some(s.sid.as_str())
                && s.office_id.as_deref() == Some(office_id)
                && &s.role == role
        })
    }

    /// 获取房间内指定 Computer 的 sid
    pub fn get_computer_sid_in_office(
        &self,
        office_id: &OfficeId,
        name: &str,
    ) -> Option<SessionId> {
        self.sessions.iter().find_map(|s| {
            if s.office_id.as_ref() == Some(office_id)
                && s.role == ClientRole::Computer
                && s.name == name
            {
                Some(s.sid.clone())
            } else {
                None
            }
        })
    }

    /// 获取所有会话
    pub fn get_all_sessions(&self) -> Vec<SessionData> {
        self.sessions.iter().map(|s| s.clone()).collect()
    }

    /// 获取会话统计信息
    pub fn get_stats(&self) -> SessionStats {
        let total = self.sessions.len();
        let agents = self
            .sessions
            .iter()
            .filter(|s| s.role == ClientRole::Agent)
            .count();
        let computers = self
            .sessions
            .iter()
            .filter(|s| s.role == ClientRole::Computer)
            .count();

        SessionStats {
            total,
            agents,
            computers,
        }
    }
}

/// 会话统计信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStats {
    /// 总会话数
    pub total: usize,
    /// Agent 数量
    pub agents: usize,
    /// Computer 数量
    pub computers: usize,
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use uuid::Uuid;

    #[test]
    fn test_session_registration() {
        let manager = SessionManager::new();
        let sid = Uuid::new_v4().to_string();
        let session = SessionData::new(sid.clone(), "test_agent".to_string(), ClientRole::Agent);

        // 注册会话
        assert!(manager.register_session(session).is_ok());

        // 获取会话
        let retrieved = manager.get_session(&sid);
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().name, "test_agent");

        // 房内解析（`get_computer_sid_in_office`）以外的全局按名查找已删除（#226 复审 🟡1）：
        // 名字解析 MUST 限定在会话所在房内，故这里只能断言记录本身。
        assert_eq!(manager.get_all_sessions().len(), 1);
    }

    /// protocol#66（场景 #2 的单元版）：**同 role 同名**的第二个会话入房撞的是**席位规则**
    /// （`4101 SeatTaken`），不是名字冲突——`(office_id, role, name)` 唯一性由「每 role 一席」蕴含，
    /// 独立的同名判定（旧 `4105`）已随名字注册表一并删除。
    ///
    /// 「跨 role 同名允许」同场覆盖：先入房的 Computer 与后入房的 Agent 同名不冲突（路由地址由 role
    /// 字段区分），佐证席位键是 `(office_id, role)` 而非名字。
    #[test]
    fn test_same_name_second_session_hits_seat_rule_not_name_conflict() {
        let manager = SessionManager::new();

        // 第一台 Computer 入房。
        manager
            .register_session(
                SessionData::new(
                    "sid-c1".to_string(),
                    "duplicate_name".to_string(),
                    ClientRole::Computer,
                )
                .with_office_id("office1".to_string()),
            )
            .unwrap();

        // 第二台 **同名** Computer 入房：席位被占 ⇒ 4101（SeatTaken(Computer)），不得是 4105。
        manager
            .register_session(SessionData::new(
                "sid-c2".to_string(),
                "duplicate_name".to_string(),
                ClientRole::Computer,
            ))
            .unwrap();
        assert!(
            matches!(
                manager.reserve_join(&"sid-c2".to_string(), "office1"),
                Err(SessionError::SeatTaken(ClientRole::Computer))
            ),
            "同 role 第二个会话（同名与否）都必须被席位规则以 4101 拒绝"
        );

        // 跨 role 同名允许：同名 Agent 入同一房成功（席位键是 (office_id, role)，不是名字）。
        manager
            .register_session(SessionData::new(
                "sid-a1".to_string(),
                "duplicate_name".to_string(),
                ClientRole::Agent,
            ))
            .unwrap();
        assert!(
            manager
                .reserve_join(&"sid-a1".to_string(), "office1")
                .is_ok(),
            "同房内跨 role 同名必须允许（一个 Computer 与一个 Agent 可以同名）"
        );

        // 跨房同名也允许：另一台同名 Computer 入另一空房成功。
        manager
            .register_session(SessionData::new(
                "sid-c3".to_string(),
                "duplicate_name".to_string(),
                ClientRole::Computer,
            ))
            .unwrap();
        assert!(
            manager
                .reserve_join(&"sid-c3".to_string(), "office2")
                .is_ok(),
            "跨房同名必须允许——名字唯一性是房内的，SDK MUST NOT 施加全局名字空间"
        );
    }

    #[test]
    fn test_pre_room_same_name_is_not_a_conflict() {
        let manager = SessionManager::new();
        let first = SessionData::new(
            "sid1".to_string(),
            "same_name".to_string(),
            ClientRole::Agent,
        );
        let second = SessionData::new(
            "sid2".to_string(),
            "same_name".to_string(),
            ClientRole::Agent,
        );

        assert!(manager.register_session(first).is_ok());
        assert!(manager.register_session(second).is_ok());
    }

    /// #226 P0-2 / protocol#66 场景 #8 的 Agent 版：**两个不同名 Agent 并发加入同一空房 ⇒ 恰一个成功**。
    ///
    /// 这是「每 role 一席」真正的判别性测试。被替换掉的旧版
    /// `test_concurrent_pre_room_same_name_is_not_a_conflict` 让两条线程注册**无房**同名会话——
    /// 无房会话根本不触碰房占用扫描，故把闸门回退成 get-then-insert 照样绿：它测的是「注册」而不是
    /// 「入房」（零判别力）。
    ///
    /// 本测试让两个线程同时调 [`SessionManager::reserve_join`] 抢同一空房，断言**恰一个**
    /// `SeatTaken`（4101）而另一个成功。回归（去掉临界区）会实测出 2/2 成功 ⇒ 红。
    #[test]
    fn test_concurrent_distinct_agents_cannot_share_one_room() {
        const ROUNDS: usize = 200;
        for round in 0..ROUNDS {
            let manager = Arc::new(SessionManager::new());
            let office = format!("office-{round}");
            let sids = [format!("sid-a-{round}"), format!("sid-b-{round}")];
            for (index, sid) in sids.iter().enumerate() {
                manager
                    .register_session(SessionData::new(
                        sid.clone(),
                        format!("agent-{index}"),
                        ClientRole::Agent,
                    ))
                    .unwrap();
            }

            let barrier = Arc::new(std::sync::Barrier::new(sids.len()));
            let handles = sids
                .iter()
                .map(|sid| {
                    let manager = Arc::clone(&manager);
                    let office = office.clone();
                    let sid = sid.clone();
                    let barrier = Arc::clone(&barrier);
                    std::thread::spawn(move || {
                        barrier.wait();
                        manager.reserve_join(&sid, &office)
                    })
                })
                .collect::<Vec<_>>();
            let outcomes = handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>();

            let accepted = outcomes.iter().filter(|result| result.is_ok()).count();
            let rejected = outcomes
                .iter()
                .filter(|result| matches!(result, Err(SessionError::SeatTaken(ClientRole::Agent))))
                .count();
            assert_eq!(
                (accepted, rejected),
                (1, 1),
                "round {round}: 一房一 Agent 必须原子生效（实得 {outcomes:?}）"
            );
            assert_eq!(manager.get_sessions_in_office(&office).len(), 1);
        }
    }

    /// #226 P0-2 / protocol#66 场景 #8+#2 的 Computer 版：**两台（同名）Computer 并发加入同一空房
    /// ⇒ 恰一个 `4101 SeatTaken(Computer)`**。
    ///
    /// 「同名」不再是独立判定维度（`4105` 已转预留）：场景 #2 要求同名第二台也回 `4101`，本测试把
    /// 「同名 + 并发」压到极值——两条线程同名抢房，仍恰一个席位码。
    #[test]
    fn test_concurrent_same_name_computers_collide_exactly_once() {
        const ROUNDS: usize = 200;
        for round in 0..ROUNDS {
            let manager = Arc::new(SessionManager::new());
            let office = format!("office-{round}");
            let sids = [format!("sid-a-{round}"), format!("sid-b-{round}")];
            for sid in &sids {
                manager
                    .register_session(SessionData::new(
                        sid.clone(),
                        "shared".to_string(),
                        ClientRole::Computer,
                    ))
                    .unwrap();
            }

            let barrier = Arc::new(std::sync::Barrier::new(sids.len()));
            let handles = sids
                .iter()
                .map(|sid| {
                    let manager = Arc::clone(&manager);
                    let office = office.clone();
                    let sid = sid.clone();
                    let barrier = Arc::clone(&barrier);
                    std::thread::spawn(move || {
                        barrier.wait();
                        manager.reserve_join(&sid, &office)
                    })
                })
                .collect::<Vec<_>>();
            let outcomes = handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>();

            let rejected = outcomes
                .iter()
                .filter(|result| {
                    matches!(result, Err(SessionError::SeatTaken(ClientRole::Computer)))
                })
                .count();
            assert_eq!(
                (outcomes.iter().filter(|r| r.is_ok()).count(), rejected),
                (1, 1),
                "round {round}: 每 role 一席必须原子生效，同名第二台也回 4101（实得 {outcomes:?}）"
            );
        }
    }

    #[test]
    fn test_office_management() {
        let manager = SessionManager::new();
        let office_id = "office_123".to_string();
        let sid = Uuid::new_v4().to_string();
        let session = SessionData::new(
            sid.clone(),
            "test_computer".to_string(),
            ClientRole::Computer,
        )
        .with_office_id(office_id.clone());

        manager.register_session(session).unwrap();

        // 检查房间内的会话
        let sessions = manager.get_sessions_in_office(&office_id);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].sid, sid);

        // 检查是否有 Agent
        assert!(!manager.has_agent_in_office(&office_id));

        // 检查是否有指定 Computer（房内解析，替换已删除的全局按名查找）
        assert_eq!(
            manager
                .get_computer_sid_in_office(&office_id, "test_computer")
                .as_deref(),
            Some(sid.as_str())
        );
    }

    #[test]
    fn test_session_unregistration() {
        let manager = SessionManager::new();
        let sid = Uuid::new_v4().to_string();
        let session = SessionData::new(sid.clone(), "test_agent".to_string(), ClientRole::Agent);

        manager.register_session(session).unwrap();

        // 注销会话
        let removed = manager.unregister_session(&sid);
        assert!(removed.is_some());

        // 验证会话已删除
        assert!(manager.get_session(&sid).is_none());
        assert!(manager.get_all_sessions().is_empty());
    }

    #[test]
    fn test_stats() {
        let manager = SessionManager::new();

        // 添加一些会话
        let agent_session = SessionData::new(
            Uuid::new_v4().to_string(),
            "agent1".to_string(),
            ClientRole::Agent,
        );
        let computer_session1 = SessionData::new(
            Uuid::new_v4().to_string(),
            "computer1".to_string(),
            ClientRole::Computer,
        );
        let computer_session2 = SessionData::new(
            Uuid::new_v4().to_string(),
            "computer2".to_string(),
            ClientRole::Computer,
        );

        manager.register_session(agent_session).unwrap();
        manager.register_session(computer_session1).unwrap();
        manager.register_session(computer_session2).unwrap();

        let stats = manager.get_stats();
        assert_eq!(stats.total, 3);
        assert_eq!(stats.agents, 1);
        assert_eq!(stats.computers, 2);
    }

    #[test]
    fn test_register_session_idempotent_same_sid() {
        let manager = SessionManager::new();
        let sid = Uuid::new_v4().to_string();
        let session1 = SessionData::new(sid.clone(), "same_name".to_string(), ClientRole::Agent);
        let session2 = SessionData::new(sid.clone(), "same_name".to_string(), ClientRole::Agent);

        assert!(manager.register_session(session1).is_ok());
        assert!(manager.register_session(session2).is_ok());

        let retrieved = manager.get_session(&sid).unwrap();
        assert_eq!(retrieved.name, "same_name");
    }

    #[test]
    fn test_get_all_sessions() {
        let manager = SessionManager::new();
        let s1 = SessionData::new(
            Uuid::new_v4().to_string(),
            "a1".to_string(),
            ClientRole::Agent,
        );
        let s2 = SessionData::new(
            Uuid::new_v4().to_string(),
            "c1".to_string(),
            ClientRole::Computer,
        );
        manager.register_session(s1).unwrap();
        manager.register_session(s2).unwrap();

        let all = manager.get_all_sessions();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn test_client_role_convert_and_display() {
        let agent: ClientRole = smcp::Role::Agent.into();
        let computer: ClientRole = smcp::Role::Computer.into();
        assert_eq!(agent.to_string(), "agent");
        assert_eq!(computer.to_string(), "computer");

        let back_agent: smcp::Role = agent.into();
        let back_computer: smcp::Role = computer.into();
        assert!(matches!(back_agent, smcp::Role::Agent));
        assert!(matches!(back_computer, smcp::Role::Computer));
    }

    #[test]
    fn test_session_data_with_extra() {
        let sid = Uuid::new_v4().to_string();
        let extra = json!({"k": "v", "n": 1});
        let session =
            SessionData::new(sid, "n".to_string(), ClientRole::Computer).with_extra(extra.clone());
        assert_eq!(session.extra, extra);
    }

    /// 席位随会话生命周期释放：**断连（注销）前旧会话占住席位、断连后同房同名可复用**。
    ///
    /// 俯视语义：席位 = `(office_id, role)`，由会话记录的 `office_id` 蕴含。静默断线期间旧会话仍在，
    /// 新会话（新 sid，同名与否）撞 `4101`——这正是协议 §静默断线与会话回收描述的瞬态冲突窗口
    /// （客户端补偿=有界退避重试）；传输层回收旧会话（`unregister_session`）后席位即释放。
    ///
    /// 另覆盖 `register_session` 的身份不可变性：既有会话不可被重复注册路径悄悄换掉身份
    /// （身份变更须新连接 + 403）——旧实现的无条件覆盖曾造成 name 预留泄漏（#226 P1-4，
    /// 该缺陷类随名字注册表删除整体消失，此处钉住防回归的注册语义）。
    #[test]
    fn seat_is_held_until_disconnect_then_reusable() {
        let manager = SessionManager::new();
        let sid = "sid-owner".to_string();

        // 1) 原会话以 Agent/agent-1 入房 ⇒ 占下 (office-A, agent) 席位。
        let owner = manager.get_or_register_session(
            sid.clone(),
            "agent-1".to_string(),
            ClientRole::Agent,
            None,
        );
        assert_eq!(owner.name, "agent-1");
        manager
            .reserve_join(&sid, "office-A")
            .expect("first join must succeed");
        assert_eq!(
            manager.get_session(&sid).unwrap().office_id.as_deref(),
            Some("office-A")
        );

        // 2) 同一 sid 以**另一身份**重复注册：保留原记录（身份不可被注册路径替换）。
        manager
            .register_session(SessionData::new(
                sid.clone(),
                "computer-2".to_string(),
                ClientRole::Computer,
            ))
            .unwrap();
        assert_eq!(
            manager.get_session(&sid).unwrap().name,
            "agent-1",
            "既有会话的身份不可被注册路径悄悄替换（身份变更须新连接 + 403）"
        );

        // 3) 旧会话未回收期间：另一个（**同名**）Agent 入同一房 ⇒ 撞席位（4101，瞬态冲突窗口）。
        manager
            .register_session(SessionData::new(
                "sid-next".to_string(),
                "agent-1".to_string(),
                ClientRole::Agent,
            ))
            .unwrap();
        assert!(
            matches!(
                manager.reserve_join(&"sid-next".to_string(), "office-A"),
                Err(SessionError::SeatTaken(ClientRole::Agent))
            ),
            "旧会话未回收时席位仍被占据（同名与否一律 4101）——瞬态冲突窗口"
        );

        // 4) 断连（传输层回收）⇒ 席位释放；同房同名复用 MUST 成功。
        assert!(manager.unregister_session(&sid).is_some());
        assert!(
            manager
                .reserve_join(&"sid-next".to_string(), "office-A")
                .is_ok(),
            "断连后席位必须已释放，同房同名方可复用"
        );
    }

    /// #226 P0-3：`commit_leave` 的**提交点**语义——广播期间被并发换房接管时不倒着覆盖。
    ///
    /// 旧实现是「广播 `await` 之后无条件 `update_office_id(None)`」：若该 `await` 期间另一个 handler 已把
    /// 会话迁到新房，则清空会把新状态倒着抹成「无房」，而 socket 仍留在新房，成为继续收 `notify:*`
    /// 而 `list_room` 查不到的**幽灵成员**。本测试固定制造该交错（先广播、再并发入房、才提交退房），
    /// 断言会话仍在新房且 `Superseded` 如实回报。
    #[test]
    fn leave_commit_does_not_overwrite_a_concurrent_room_change() {
        let manager = SessionManager::new();
        let sid = "sid-ghost".to_string();
        manager
            .register_session(SessionData::new(
                sid.clone(),
                "c-1".to_string(),
                ClientRole::Computer,
            ))
            .unwrap();
        manager.reserve_join(&sid, "office-old").unwrap();

        // 退房 handler 读到「我该向 office-old 广播」（模拟其在 `await` 之前取到的快照）。
        let broadcast_office = manager
            .get_session(&sid)
            .and_then(|s| s.office_id)
            .expect("session must be in office-old");

        // 广播 `await` 期间：并发 join 把会话迁到 office-new 并提交。
        let joined = manager
            .reserve_join(&sid, "office-new")
            .expect("computer may switch rooms");
        assert_eq!(
            joined.decision,
            JoinDecision::LeaveAndJoin {
                leave_office: "office-old".to_string()
            }
        );

        // 退房 handler 恢复执行并提交：必须识别出「已被接管」，不得清空 office-new。
        let commit = manager
            .commit_leave(&sid, Some(&broadcast_office))
            .expect("commit must not fail");
        assert_eq!(
            commit,
            LeaveCommit::Superseded {
                current: "office-new".to_string()
            }
        );
        assert_eq!(
            manager.get_session(&sid).unwrap().office_id.as_deref(),
            Some("office-new"),
            "并发换房后的权威状态不得被过期的退房倒着覆盖（幽灵成员根因）"
        );

        // 反向：会话仍在我广播的那个房时，提交必须真的释放它（幂等退房/正常退房路径）。
        assert_eq!(
            manager.commit_leave(&sid, Some("office-new")).unwrap(),
            LeaveCommit::Released("office-new".to_string())
        );
        assert_eq!(manager.get_session(&sid).unwrap().office_id, None);
        assert_eq!(
            manager.commit_leave(&sid, None).unwrap(),
            LeaveCommit::AlreadyIdle,
            "无房退房必须幂等成功"
        );
    }

    #[test]
    fn test_room_error_codes_follow_protocol_contract() {
        // protocol#66：4101 泛化为「本 role 席位已占」，Agent / Computer 同码。
        assert_eq!(
            SessionError::SeatTaken(ClientRole::Agent).error_code(),
            smcp::error_codes::ROOM_FULL
        );
        assert_eq!(
            SessionError::SeatTaken(ClientRole::Computer).error_code(),
            smcp::error_codes::ROOM_FULL
        );
        assert_eq!(
            SessionError::AgentAlreadyInRoom("office".to_string()).error_code(),
            smcp::error_codes::ALREADY_IN_ROOM
        );
        assert_ne!(
            SessionError::AgentAlreadyInRoom("office".to_string()).error_code(),
            smcp::error_codes::ROOM_NOT_FOUND
        );
        // 4105 为预留码：会话层不再存在可映射到它的错误变体（类型层面已排除产出）。
        assert_ne!(
            SessionError::SeatTaken(ClientRole::Computer).error_code(),
            smcp::error_codes::NAME_CONFLICT
        );
    }
}
