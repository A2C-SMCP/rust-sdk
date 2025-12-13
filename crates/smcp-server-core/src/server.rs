//! SMCP 服务器构建器 / SMCP server builder

use crate::auth::{AuthenticationProvider, DefaultAuthenticationProvider};
use crate::handler::{ServerState, SmcpHandler};
use crate::session::SessionManager;
use socketioxide::{SocketIo};
use socketioxide::layer::SocketIoLayer;
use std::sync::Arc;
use tracing::info;

/// SMCP 服务器构建器
/// SMCP server builder
#[derive(Clone)]
pub struct SmcpServerBuilder {
    /// 认证提供者
    auth_provider: Option<Arc<dyn AuthenticationProvider>>,
    /// 会话管理器
    session_manager: Option<Arc<SessionManager>>,
}

impl Default for SmcpServerBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl SmcpServerBuilder {
    /// 创建新的服务器构建器
    /// Create new server builder
    pub fn new() -> Self {
        Self {
            auth_provider: None,
            session_manager: None,
        }
    }

    /// 设置认证提供者
    /// Set authentication provider
    pub fn with_auth_provider(mut self, provider: Arc<dyn AuthenticationProvider>) -> Self {
        self.auth_provider = Some(provider);
        self
    }

    /// 设置默认认证提供者（基于 API Key）
    /// Set default authentication provider (API Key based)
    pub fn with_default_auth(mut self, admin_secret: Option<String>, api_key_name: Option<String>) -> Self {
        let provider = Arc::new(DefaultAuthenticationProvider::new(admin_secret, api_key_name));
        self.auth_provider = Some(provider);
        self
    }

    /// 设置会话管理器
    /// Set session manager
    pub fn with_session_manager(mut self, manager: Arc<SessionManager>) -> Self {
        self.session_manager = Some(manager);
        self
    }

    /// 构建 Socket.IO Layer
    /// Build Socket.IO layer
    pub fn build_layer(self) -> Result<SmcpServerLayer, crate::handler::HandlerError> {
        // 使用默认值
        let auth_provider = self.auth_provider
            .unwrap_or_else(|| Arc::new(DefaultAuthenticationProvider::new(None, None)));
        let session_manager = self.session_manager
            .unwrap_or_else(|| Arc::new(SessionManager::new()));
        
        // 创建服务器状态
        let state = ServerState {
            session_manager,
            auth_provider,
        };

        // 创建 Socket.IO
        let (layer, io) = SocketIo::builder().with_state(state.clone()).build_layer();

        // 注册处理器
        SmcpHandler::register_handlers(&io);

        info!("SMCP Server layer built successfully");

        Ok(SmcpServerLayer { io, layer, state })
    }
}

/// SMCP 服务器 Layer
/// SMCP server layer
#[derive(Clone)]
pub struct SmcpServerLayer {
    /// Socket.IO 实例
    pub io: SocketIo,
    /// Tower Layer
    pub layer: SocketIoLayer,
    /// 服务器状态
    pub state: ServerState,
}

impl SmcpServerLayer {
    pub fn socket_io(&self) -> &SocketIo {
        &self.io
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn test_server_builder() {
        let builder = SmcpServerBuilder::new()
            .with_default_auth(Some("test".to_string()), None);

        assert!(builder.build_layer().is_ok());
    }

    #[test]
    fn test_server_builder_with_custom_auth() {
        let auth = Arc::new(DefaultAuthenticationProvider::new(Some("test".to_string()), None));

        let builder = SmcpServerBuilder::new().with_auth_provider(auth);
        assert!(builder.build_layer().is_ok());
    }

    #[test]
    fn test_server_builder_default_ok() {
        let layer = SmcpServerBuilder::default().build_layer().unwrap();
        let _io_ref = layer.socket_io();
    }

    #[test]
    fn test_server_builder_with_session_manager_injection() {
        let manager = Arc::new(SessionManager::new());
        let layer = SmcpServerBuilder::new()
            .with_session_manager(manager.clone())
            .build_layer()
            .unwrap();

        assert!(Arc::ptr_eq(&layer.state.session_manager, &manager));
    }

    #[test]
    fn test_socket_io_accessor_returns_inner() {
        let layer = SmcpServerBuilder::new().build_layer().unwrap();
        let io_ref = layer.socket_io();
        let inner_ptr: *const SocketIo = &layer.io;
        let accessor_ptr: *const SocketIo = io_ref;
        assert_eq!(inner_ptr, accessor_ptr);
    }
}
