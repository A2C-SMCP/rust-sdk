//! Common test utilities for cross-crate integration tests

#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::sync::Notify;
use tokio::time::timeout;

/// Find an available port on localhost
pub async fn find_available_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap().port()
}

/// Test timeout duration
#[allow(dead_code)]
pub const TEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Wait for a condition with timeout
#[allow(dead_code)]
pub async fn wait_with_timeout<F, Fut>(
    duration: Duration,
    future: F,
) -> Result<Fut::Output, &'static str>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    match timeout(duration, future()).await {
        Ok(true) => Ok(true),
        Ok(false) => Err("Condition not met"),
        Err(_) => Err("Timeout"),
    }
}

/// A flag that can be used to signal test completion
#[derive(Clone)]
#[allow(dead_code)]
pub struct TestFlag(Arc<AtomicBool>);

impl TestFlag {
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    pub fn set(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_set(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    pub async fn wait(&self) {
        while !self.is_set() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

/// A notify wrapper for easier testing
#[derive(Clone)]
#[allow(dead_code)]
pub struct TestNotify(Arc<Notify>);

impl TestNotify {
    pub fn new() -> Self {
        Self(Arc::new(Notify::new()))
    }

    pub fn notify(&self) {
        self.0.notify_one();
    }

    pub async fn wait(&self) {
        self.0.notified().await;
    }

    pub async fn wait_with_timeout(&self, duration: Duration) -> bool {
        timeout(duration, self.0.notified()).await.is_ok()
    }
}

/// Macro to skip tests if required features are not enabled
#[macro_export]
macro_rules! skip_if_no_feature {
    ($feature:literal) => {
        if !cfg!(feature = $feature) {
            eprintln!("Skipping test: feature '{}' not enabled", $feature);
            return;
        }
    };
}
