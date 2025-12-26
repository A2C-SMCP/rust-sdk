// Minimal mock event handler for testing

use std::sync::Arc;
use tokio::sync::RwLock;

use smcp::EnterOfficeNotification;

/// Simple mock event handler that tracks computer entry
#[derive(Clone)]
pub struct SimpleMockHandler {
    pub computers: Arc<RwLock<Vec<String>>>,
}

impl SimpleMockHandler {
    pub fn new() -> Self {
        Self {
            computers: Arc::new(RwLock::new(Vec::new())),
        }
    }

    pub async fn add_computer(&self, name: String) {
        self.computers.write().await.push(name);
    }

    pub async fn has_computer(&self, name: &str) -> bool {
        self.computers.read().await.contains(&name.to_string())
    }

    pub async fn count(&self) -> usize {
        self.computers.read().await.len()
    }
}

impl Default for SimpleMockHandler {
    fn default() -> Self {
        Self::new()
    }
}
