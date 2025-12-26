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

pub use helpers::*;
pub use mock_agent::*;
pub use mock_minimal::*;
pub use test_server::*;
