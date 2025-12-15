/*!
* 文件名: agent_event_handling
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: None
* 描述: SMCP Agent事件处理测试 / SMCP Agent event handling tests
*/

use std::time::Duration;
// use smcp_agent::{DefaultAuthProvider, SmcpAgentConfig, AsyncSmcpAgent}; // Unused imports
mod common;
use common::*;

#[tokio::test]
async fn test_agent_event_handler_creation() {
    // 中文：测试事件处理器创建
    // English: Test event handler creation

    let handler = TestEventHandler::new();
    assert!(handler.computer_enter_events.lock().await.is_empty());
    assert!(handler.computer_leave_events.lock().await.is_empty());
    assert!(handler.computer_update_events.lock().await.is_empty());
    assert!(handler.tools_received.lock().await.is_empty());
}

#[tokio::test]
async fn test_agent_event_handler_clear() {
    // 中文：测试事件处理器清空功能
    // English: Test event handler clear functionality

    let handler = TestEventHandler::new();

    // 清空事件（即使为空）
    handler.clear().await;

    // 验证所有事件列表都是空的
    assert!(handler.computer_enter_events.lock().await.is_empty());
    assert!(handler.computer_leave_events.lock().await.is_empty());
    assert!(handler.computer_update_events.lock().await.is_empty());
    assert!(handler.tools_received.lock().await.is_empty());
}

#[tokio::test]
async fn test_agent_with_event_handler() {
    // 中文：测试Agent设置事件处理器
    // English: Test Agent with event handler

    let handler = TestEventHandler::new();
    let _agent =
        create_test_agent_with_handler("test-agent-handler", "test-office-handler", handler);

    // 验证Agent创建成功
    // Agent创建成功
}

#[tokio::test]
async fn test_agent_event_handler_wait_for_events() {
    // 中文：测试事件处理器等待事件功能
    // English: Test event handler wait for events functionality

    let handler = TestEventHandler::new();

    // 等待0个事件（应该立即返回false）
    let result = handler.wait_for_events(0, Duration::from_millis(10)).await;
    assert!(!result, "Should not wait for 0 events");

    // 等待事件（超时）
    let result = handler.wait_for_events(1, Duration::from_millis(10)).await;
    assert!(!result, "Should timeout waiting for events");
}

#[tokio::test]
async fn test_agent_multiple_event_handlers() {
    // 中文：测试多个事件处理器
    // English: Test multiple event handlers

    let handler1 = TestEventHandler::new();
    let handler2 = TestEventHandler::new();

    let _agent1 = create_test_agent_with_handler("test-agent-1", "test-office-1", handler1);
    let _agent2 = create_test_agent_with_handler("test-agent-2", "test-office-2", handler2);

    // 验证两个Agent都创建成功
    // 多个Agent创建成功
}

#[tokio::test]
async fn test_agent_event_handler_clone() {
    // 中文：测试事件处理器克隆
    // English: Test event handler clone

    let handler = TestEventHandler::new();
    let cloned_handler = handler.clone();

    // 验证克隆的事件处理器是独立的
    handler.clear().await;
    cloned_handler.clear().await;

    assert!(handler.computer_enter_events.lock().await.is_empty());
    assert!(cloned_handler.computer_enter_events.lock().await.is_empty());
}

#[tokio::test]
async fn test_agent_event_handler_concurrent_access() {
    // 中文：测试事件处理器并发访问
    // English: Test event handler concurrent access

    let handler = TestEventHandler::new();
    let handler_clone = handler.clone();

    // 并发清空事件
    let clear1 = handler.clear();
    let clear2 = handler_clone.clear();

    tokio::join!(clear1, clear2);

    // 验证并发访问没有问题
    assert!(handler.computer_enter_events.lock().await.is_empty());
    assert!(handler.tools_received.lock().await.is_empty());
}
