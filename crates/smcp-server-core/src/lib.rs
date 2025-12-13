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

    #[test]
    fn test_library_imports() {
        // 确保所有主要类型都可以正确导入
        let _builder: SmcpServerBuilder = SmcpServerBuilder::new();
        let _manager: SessionManager = SessionManager::new();
    }
}
