//! SMCP 服务器核心库 / SMCP server core library
//! 
//! 提供基于 socketioxide + Tokio 的 SMCP 协议服务器实现
//! Provides SMCP protocol server implementation based on socketioxide + Tokio

pub mod auth;
pub mod handler;
pub mod session;
pub mod server;

// 重新导出主要类型
pub use auth::{AuthenticationProvider, DefaultAuthenticationProvider, AuthError};
pub use handler::{HandlerError, SmcpHandler, ServerState};
pub use session::{SessionManager, SessionData, SessionError, ClientRole, SessionStats};
pub use server::{SmcpServerBuilder, SmcpServerLayer};

/// SMCP 服务器预lude
/// SMCP server prelude
pub mod prelude {
    pub use crate::auth::*;
    pub use crate::handler::*;
    pub use crate::session::*;
    pub use crate::server::*;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json;

    #[test]
    fn test_library_imports() {
        // 确保所有主要类型都可以正确导入
        let _builder: SmcpServerBuilder = SmcpServerBuilder::new();
        let _manager: SessionManager = SessionManager::new();
    }

    #[test]
    fn test_smcp_crate_api_is_executed() {
        let req_id = smcp::ReqId::new();
        assert!(!req_id.as_str().is_empty());

        let role_json = serde_json::to_string(&smcp::Role::Agent).unwrap();
        assert_eq!(role_json, "\"agent\"");

        let n = smcp::Notification::EnterOffice(smcp::EnterOfficeNotification {
            office_id: "office1".to_string(),
            computer: Some("c1".to_string()),
            agent: None,
        });
        let json = serde_json::to_string(&n).unwrap();
        let de: smcp::Notification = serde_json::from_str(&json).unwrap();
        match de {
            smcp::Notification::EnterOffice(p) => {
                assert_eq!(p.office_id, "office1");
                assert_eq!(p.computer.as_deref(), Some("c1"));
                assert!(p.agent.is_none());
            }
            _ => panic!("unexpected notification"),
        }
    }
}
