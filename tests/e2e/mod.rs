// End-to-end tests for SMCP protocol
//
// This module contains full lifecycle tests that verify:
// - Server startup and management
// - Agent connection and interaction
// - Computer client lifecycle
// - Tool call flow
// - Notification system
// - Error handling

pub mod helpers;
pub mod mock_agent;
pub mod mock_minimal;
pub mod test_server;

// Re-export for test convenience
// These are used by test files, not in this module
#[allow(unused_imports)]
pub use helpers::{generate_agent_name, generate_computer_name, generate_office_id};
#[allow(unused_imports)]
pub use mock_agent::MockEventHandler;
pub use test_server::*;
