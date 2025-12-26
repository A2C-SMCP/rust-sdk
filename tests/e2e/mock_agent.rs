// Mock Agent event handler for testing
//
// Provides a mock implementation of AsyncAgentEventHandler
// that captures and records events for verification

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use smcp::{
    events::*, EnterOfficeNotification, LeaveOfficeNotification, ReqId, Role, SMCPTool,
    UpdateMCPConfigNotification,
};
use smcp_agent::{AsyncAgentEventHandler, Result as AgentResult};

/// Mock event handler that records all events
#[derive(Clone)]
pub struct MockEventHandler {
    /// Recorded enter_office events
    pub enter_office_events: Arc<RwLock<Vec<EnterOfficeNotification>>>,
    /// Recorded leave_office events
    pub leave_office_events: Arc<RwLock<Vec<LeaveOfficeNotification>>>,
    /// Recorded update_config events
    pub update_config_events: Arc<RwLock<Vec<UpdateMCPConfigNotification>>>,
    /// Recorded tools received events: (computer, tools)
    pub tools_received: Arc<RwLock<Vec<(String, Vec<SMCPTool>)>>>,
    /// Recorded update_desktop events
    pub update_desktop_events: Arc<RwLock<Vec<String>>>,
    /// Recorded desktop updated events: (computer, desktops)
    pub desktop_updated: Arc<RwLock<Vec<(String, Vec<String>)>>>,
    /// Recorded tool call results: (computer, tool, result)
    pub tool_call_results: Arc<RwLock<Vec<(String, String, serde_json::Value)>>>,
}

impl MockEventHandler {
    /// Create a new mock event handler
    pub fn new() -> Self {
        Self {
            enter_office_events: Arc::new(RwLock::new(Vec::new())),
            leave_office_events: Arc::new(RwLock::new(Vec::new())),
            update_config_events: Arc::new(RwLock::new(Vec::new())),
            tools_received: Arc::new(RwLock::new(Vec::new())),
            update_desktop_events: Arc::new(RwLock::new(Vec::new())),
            desktop_updated: Arc::new(RwLock::new(Vec::new())),
            tool_call_results: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Clear all recorded events
    pub async fn clear(&self) {
        self.enter_office_events.write().await.clear();
        self.leave_office_events.write().await.clear();
        self.update_config_events.write().await.clear();
        self.tools_received.write().await.clear();
        self.update_desktop_events.write().await.clear();
        self.desktop_updated.write().await.clear();
        self.tool_call_results.write().await.clear();
    }

    /// Get count of enter_office events
    pub async fn enter_office_count(&self) -> usize {
        self.enter_office_events.read().await.len()
    }

    /// Get count of leave_office events
    pub async fn leave_office_count(&self) -> usize {
        self.leave_office_events.read().await.len()
    }

    /// Get count of tools received events
    pub async fn tools_received_count(&self) -> usize {
        self.tools_received.read().await.len()
    }

    /// Check if a specific computer entered office
    pub async fn has_computer(&self, computer_name: &str) -> bool {
        let events = self.enter_office_events.read().await;
        events
            .iter()
            .any(|e| e.computer.as_deref() == Some(computer_name))
    }

    /// Get tools for a specific computer
    pub async fn get_tools_for_computer(&self, computer_name: &str) -> Option<Vec<SMCPTool>> {
        let tools = self.tools_received.read().await;
        tools
            .iter()
            .find(|(computer, _)| computer == computer_name)
            .map(|(_, tools)| tools.clone())
    }

    /// Wait for a computer to enter office
    pub async fn wait_for_computer(&self, computer_name: &str, timeout_secs: u64) -> bool {
        let start = std::time::Instant::now();
        let timeout_duration = std::time::Duration::from_secs(timeout_secs);

        while start.elapsed() < timeout_duration {
            if self.has_computer(computer_name).await {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        false
    }

    /// Wait for tools from a specific computer
    pub async fn wait_for_tools(&self, computer_name: &str, timeout_secs: u64) -> bool {
        let start = std::time::Instant::now();
        let timeout_duration = std::time::Duration::from_secs(timeout_secs);

        while start.elapsed() < timeout_duration {
            if self.get_tools_for_computer(computer_name).await.is_some() {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        false
    }
}

impl Default for MockEventHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl AsyncAgentEventHandler for MockEventHandler {
    /// Handle computer enter office event
    async fn on_computer_enter_office(
        &self,
        data: EnterOfficeNotification,
        _agent: &smcp_agent::AsyncSmcpAgent,
    ) -> AgentResult<()> {
        println!("MockEventHandler: on_computer_enter_office called: {:?}", data);
        self.enter_office_events.write().await.push(data);
        Ok(())
    }

    /// Handle computer leave office event
    async fn on_computer_leave_office(
        &self,
        data: LeaveOfficeNotification,
        _agent: &smcp_agent::AsyncSmcpAgent,
    ) -> AgentResult<()> {
        self.leave_office_events.write().await.push(data);
        Ok(())
    }

    /// Handle computer update config event
    async fn on_computer_update_config(
        &self,
        data: UpdateMCPConfigNotification,
        _agent: &smcp_agent::AsyncSmcpAgent,
    ) -> AgentResult<()> {
        self.update_config_events.write().await.push(data);
        Ok(())
    }

    /// Handle tools received event
    async fn on_tools_received(
        &self,
        computer: &str,
        tools: Vec<SMCPTool>,
        _agent: &smcp_agent::AsyncSmcpAgent,
    ) -> AgentResult<()> {
        self.tools_received
            .write()
            .await
            .push((computer.to_string(), tools));
        Ok(())
    }

    /// Handle desktop updated event
    async fn on_desktop_updated(
        &self,
        computer: &str,
        desktops: Vec<String>,
        _agent: &smcp_agent::AsyncSmcpAgent,
    ) -> AgentResult<()> {
        self.desktop_updated
            .write()
            .await
            .push((computer.to_string(), desktops));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_mock_event_handler() {
        let handler = MockEventHandler::new();

        assert_eq!(handler.enter_office_count().await, 0);
        assert!(!handler.has_computer("test_computer").await);

        // Simulate an event
        handler
            .on_computer_enter_office(
                EnterOfficeNotification {
                    office_id: "test_office".to_string(),
                    computer: Some("test_computer".to_string()),
                    agent: None,
                },
                &smcp_agent::AsyncSmcpAgent::new(
                    smcp_agent::DefaultAuthProvider::new("agent".to_string(), "office".to_string()),
                    smcp_agent::SmcpAgentConfig::default(),
                ),
            )
            .await
            .unwrap();

        assert_eq!(handler.enter_office_count().await, 1);
        assert!(handler.has_computer("test_computer").await);
    }
}
