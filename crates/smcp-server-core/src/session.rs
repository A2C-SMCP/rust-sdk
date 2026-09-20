//! 会话管理模块 / Session management module

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use thiserror::Error;

// 类型别名
pub type OfficeId = String;
pub type SessionId = String;

/// 会话错误类型
#[derive(Error, Debug, serde::Serialize)]
pub enum SessionError {
    #[error("Session not found: {0}")]
    NotFound(String),
    #[error("Name already registered: {0}")]
    NameAlreadyRegistered(String),
    #[error("Agent already in room: {0}")]
    AgentAlreadyInRoom(OfficeId),
    #[error("Agent already exists in room")]
    AgentAlreadyExists,
    #[error("Computer with name '{0}' already exists in room '{1}'")]
    ComputerAlreadyExists(String, OfficeId),
    #[error("Invalid session state: {0}")]
    InvalidState(String),
}

impl SessionError {
    /// 获取错误码 / Get error code
    pub fn error_code(&self) -> i32 {
        match self {
            SessionError::NotFound(_) => smcp::error_codes::NOT_FOUND,
            SessionError::NameAlreadyRegistered(_) => smcp::error_codes::FORBIDDEN,
            SessionError::AgentAlreadyInRoom(_) => smcp::error_codes::FORBIDDEN,
            SessionError::AgentAlreadyExists => smcp::error_codes::ROOM_FULL,
            SessionError::ComputerAlreadyExists(_, _) => smcp::error_codes::FORBIDDEN,
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

/// 会话管理器
#[derive(Debug)]
pub struct SessionManager {
    /// sid -> session_data 映射
    sessions: Arc<DashMap<SessionId, SessionData>>,
    /// name -> sid 映射（用于通过 name 查找 session）
    name_to_sid: Arc<DashMap<String, SessionId>>,
}

impl SessionManager {
    fn name_key(role: &ClientRole, office_id: Option<&OfficeId>, name: &str) -> Option<String> {
        let office_id = office_id?;
        match role {
            // Prefix the office length so `office_id` and `name` remain an
            // unambiguous tuple even when either value contains `:`.
            ClientRole::Agent => Some(format!("agent:{}:{}:{}", office_id.len(), office_id, name)),
            ClientRole::Computer => Some(format!(
                "computer:{}:{}:{}",
                office_id.len(),
                office_id,
                name
            )),
        }
    }

    /// 创建新的会话管理器
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(DashMap::new()),
            name_to_sid: Arc::new(DashMap::new()),
        }
    }

    /// 注册新会话
    pub fn register_session(&self, session: SessionData) -> Result<(), SessionError> {
        if let Some(key) = Self::name_key(&session.role, session.office_id.as_ref(), &session.name)
        {
            // Name uniqueness is room-scoped; pre-room sessions have no key.
            match self.name_to_sid.entry(key) {
                dashmap::mapref::entry::Entry::Occupied(entry) => {
                    if *entry.get() != session.sid {
                        return Err(SessionError::NameAlreadyRegistered(session.name));
                    }
                    tracing::debug!("Name '{}' re-registered by same sid", session.name);
                }
                dashmap::mapref::entry::Entry::Vacant(entry) => {
                    entry.insert(session.sid.clone());
                }
            }
        }

        // Always retain the sid -> session record, including pre-room sessions.
        self.sessions.insert(session.sid.clone(), session.clone());

        tracing::debug!("Registered session: {} -> {}", session.name, session.sid);
        Ok(())
    }

    /// 注销会话
    pub fn unregister_session(&self, sid: &SessionId) -> Option<SessionData> {
        let session = self.sessions.remove(sid)?;

        // 清理 name 映射
        if let Some(key) = Self::name_key(
            &session.1.role,
            session.1.office_id.as_ref(),
            &session.1.name,
        ) {
            self.name_to_sid.remove(&key);
        }

        tracing::debug!("Unregistered session: {} -> {}", session.1.name, sid);
        Some(session.1)
    }

    /// 获取会话数据
    pub fn get_session(&self, sid: &SessionId) -> Option<SessionData> {
        self.sessions.get(sid).map(|s| s.clone())
    }

    /// 通过名称获取会话 ID
    pub fn get_sid_by_name(&self, name: &str) -> Option<SessionId> {
        // Legacy compatibility lookup: without an office this is only safe
        // when exactly one Agent with that name exists.  Ambiguous names are
        // intentionally not resolved across rooms.
        let mut matches = self
            .sessions
            .iter()
            .filter(|s| s.role == ClientRole::Agent && s.name == name)
            .map(|s| s.sid.clone());
        let sid = matches.next()?;
        if matches.next().is_none() {
            Some(sid)
        } else {
            None
        }
    }

    /// 更新会话的办公室 ID
    pub fn update_office_id(
        &self,
        sid: &SessionId,
        office_id: Option<OfficeId>,
    ) -> Result<(), SessionError> {
        let mut session = self
            .sessions
            .get_mut(sid)
            .ok_or_else(|| SessionError::NotFound(sid.clone()))?;

        let role = session.role.clone();
        let name = session.name.clone();
        let old_office_id = session.office_id.clone();

        let old_key = Self::name_key(&role, old_office_id.as_ref(), &name);
        let new_key = Self::name_key(&role, office_id.as_ref(), &name);

        if old_key != new_key {
            let remove_old_key = if let Some(new_key) = new_key {
                // DashMap's entry API keeps the conflict check and claim in
                // one shard lock, preventing two joins from claiming the
                // same room/name concurrently.
                match self.name_to_sid.entry(new_key) {
                    dashmap::mapref::entry::Entry::Occupied(entry) => {
                        if *entry.get() != *sid {
                            return Err(SessionError::NameAlreadyRegistered(name));
                        }
                        true
                    }
                    dashmap::mapref::entry::Entry::Vacant(entry) => {
                        entry.insert(sid.clone());
                        true
                    }
                }
            } else {
                true
            };

            // Drop the entry guard before touching another key in the same
            // DashMap; otherwise a same-shard old key can deadlock on write.
            if remove_old_key {
                if let Some(old_key) = old_key {
                    self.name_to_sid.remove(&old_key);
                }
            }
        }

        session.office_id = office_id;
        Ok(())
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
        self.sessions
            .iter()
            .any(|s| s.office_id.as_ref() == Some(office_id) && s.role == ClientRole::Agent)
    }

    /// 检查房间内是否有指定名称的 Computer
    pub fn has_computer_in_office(&self, office_id: &OfficeId, name: &str) -> bool {
        self.sessions.iter().any(|s| {
            s.office_id.as_ref() == Some(office_id)
                && s.role == ClientRole::Computer
                && s.name == name
        })
    }

    /// Check for a same-name Computer in a room, excluding one session.
    pub fn has_computer_in_office_except(
        &self,
        office_id: &OfficeId,
        name: &str,
        excluded_sid: &SessionId,
    ) -> bool {
        self.sessions.iter().any(|s| {
            s.sid != *excluded_sid
                && s.office_id.as_ref() == Some(office_id)
                && s.role == ClientRole::Computer
                && s.name == name
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

        // A single unambiguous legacy lookup remains available.
        assert_eq!(manager.get_sid_by_name("test_agent"), Some(sid));
    }

    #[test]
    fn test_duplicate_name_registration() {
        let manager = SessionManager::new();
        let sid1 = Uuid::new_v4().to_string();
        let sid2 = Uuid::new_v4().to_string();
        let sid3 = Uuid::new_v4().to_string();
        let sid4 = Uuid::new_v4().to_string();

        let session1 = SessionData::new(
            sid1.clone(),
            "duplicate_name".to_string(),
            ClientRole::Agent,
        )
        .with_office_id("office1".to_string());
        let session2 = SessionData::new(
            sid2.clone(),
            "duplicate_name".to_string(),
            ClientRole::Agent,
        );

        let session3 = SessionData::new(
            sid3.clone(),
            "duplicate_name".to_string(),
            ClientRole::Computer,
        )
        .with_office_id("office1".to_string());

        let session4 = SessionData::new(
            sid4.clone(),
            "duplicate_name".to_string(),
            ClientRole::Computer,
        )
        .with_office_id("office2".to_string());

        // 第一个注册成功
        assert!(manager.register_session(session1).is_ok());

        // Agent names are unique only within an office.
        let session2 = session2.with_office_id("office2".to_string());
        assert!(manager.register_session(session2).is_ok());

        // Computer 名称按 office 唯一：同 office 冲突
        assert!(manager.register_session(session3.clone()).is_ok());
        assert!(manager.register_session(session3).is_ok());

        let dup_same_office = SessionData::new(
            Uuid::new_v4().to_string(),
            "duplicate_name".to_string(),
            ClientRole::Computer,
        )
        .with_office_id("office1".to_string());
        assert!(manager.register_session(dup_same_office).is_err());

        // 不同 office 允许同名
        assert!(manager.register_session(session4).is_ok());
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

    #[test]
    fn test_concurrent_pre_room_same_name_is_not_a_conflict() {
        let manager = Arc::new(SessionManager::new());
        let handles = (1..=2)
            .map(|n| {
                let manager = Arc::clone(&manager);
                std::thread::spawn(move || {
                    manager.register_session(SessionData::new(
                        format!("sid{n}"),
                        "same_name".to_string(),
                        ClientRole::Agent,
                    ))
                })
            })
            .collect::<Vec<_>>();

        for handle in handles {
            assert!(handle.join().unwrap().is_ok());
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

        // 检查是否有指定 Computer
        assert!(manager.has_computer_in_office(&office_id, "test_computer"));
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
        assert!(manager.get_sid_by_name("test_agent").is_none());
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
    fn test_update_office_id_ok_and_not_found() {
        let manager = SessionManager::new();
        let sid = Uuid::new_v4().to_string();
        let session = SessionData::new(sid.clone(), "test".to_string(), ClientRole::Agent);
        manager.register_session(session).unwrap();

        assert!(manager
            .update_office_id(&sid, Some("office_x".to_string()))
            .is_ok());
        assert_eq!(
            manager.get_session(&sid).unwrap().office_id,
            Some("office_x".to_string())
        );

        let missing_sid = Uuid::new_v4().to_string();
        let err = manager
            .update_office_id(&missing_sid, Some("office_y".to_string()))
            .unwrap_err();
        assert!(matches!(err, SessionError::NotFound(s) if s == missing_sid));
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
}
