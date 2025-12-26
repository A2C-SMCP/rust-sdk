// Helper functions and utilities for E2E testing

use std::time::Duration;
use tokio::time::timeout;

// Simple UUID generator using timestamp and random
fn generate_uuid_segment() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{:x}", timestamp)
}

/// Default timeout for E2E test operations
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Timeout for tool call operations
pub const TOOL_CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Timeout for joining office
pub const JOIN_OFFICE_TIMEOUT: Duration = Duration::from_secs(5);

/// Wait for a condition to become true with timeout
pub async fn wait_for_condition<F, Fut>(
    condition: F,
    timeout_duration: Duration,
    check_interval: Duration,
) -> bool
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let start = std::time::Instant::now();

    while start.elapsed() < timeout_duration {
        if condition().await {
            return true;
        }
        tokio::time::sleep(check_interval).await;
    }

    false
}

/// Generate a random office ID for testing
pub fn generate_office_id() -> String {
    format!("test_office_{}", generate_uuid_segment())
}

/// Generate a random agent name for testing
pub fn generate_agent_name() -> String {
    format!("test_agent_{}", generate_uuid_segment())
}

/// Generate a random computer name for testing
pub fn generate_computer_name() -> String {
    format!("test_computer_{}", generate_uuid_segment())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_wait_for_condition() {
        // Test condition that becomes true immediately
        let result = wait_for_condition(
            || async { true },
            Duration::from_secs(1),
            Duration::from_millis(10),
        )
        .await;
        assert!(result);

        // Test condition that becomes true after delay
        let start = std::time::Instant::now();
        let result = wait_for_condition(
            || async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                start.elapsed() >= Duration::from_millis(100)
            },
            Duration::from_secs(1),
            Duration::from_millis(10),
        )
        .await;
        assert!(result);

        // Test condition that never becomes true (timeout)
        let result = wait_for_condition(
            || async { false },
            Duration::from_millis(100),
            Duration::from_millis(10),
        )
        .await;
        assert!(!result);
    }

    #[test]
    fn test_generate_ids() {
        let office_id = generate_office_id();
        assert!(office_id.starts_with("test_office_"));

        let agent_name = generate_agent_name();
        assert!(agent_name.starts_with("test_agent_"));

        let computer_name = generate_computer_name();
        assert!(computer_name.starts_with("test_computer_"));
    }
}
