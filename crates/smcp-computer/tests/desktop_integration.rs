/**
* 文件名: desktop_integration.rs
* 作者: JQQ
* 创建日期: 2025/12/16
* 最后修改日期: 2025/12/16
* 版权: 2023 JQQ. All rights reserved.
* 依赖: None
* 描述: Desktop模块集成测试 / Desktop module integration tests
*/
mod common;

use smcp_computer::desktop::{organize_desktop, ToolCallRecord, WindowInfo};
use smcp_computer::mcp_clients::model::{make_resource, ReadResourceResult, ResourceContents};
use std::collections::HashMap;

/// 测试desktop模块与mcp_clients类型的集成 / Test integration between desktop and mcp_clients types
#[test]
fn test_desktop_with_mcp_clients_types() {
    // 使用mcp_clients中定义的类型创建窗口信息
    let windows = vec![
        WindowInfo {
            server_name: "test_server".to_string(),
            resource: make_resource(
                "window://test.mcp.com/window1?priority=10",
                "Test Window 1",
                Some("A test window".to_string()),
                Some("text/plain".to_string()),
            ),
            read_result: ReadResourceResult {
                contents: vec![ResourceContents::text(
                    "Test content 1",
                    "window://test.mcp.com/window1?priority=10",
                )],
            },
        },
        WindowInfo {
            server_name: "test_server".to_string(),
            resource: make_resource(
                "window://test.mcp.com/window2?fullscreen=true",
                "Test Window 2",
                None,
                None,
            ),
            read_result: ReadResourceResult {
                contents: vec![ResourceContents::text(
                    "Fullscreen content",
                    "window://test.mcp.com/window2?fullscreen=true",
                )],
            },
        },
    ];

    let result = organize_desktop(windows, None, &[]);

    // 验证结果符合预期：有fullscreen时只返回一个窗口
    assert_eq!(result.len(), 1);
    assert!(result[0].contains("window://test.mcp.com/window2?fullscreen=true"));
    assert!(result[0].contains("Fullscreen content"));
}

/// 测试多个服务器的窗口组织 / Test organizing windows from multiple servers
#[test]
fn test_multi_server_organization() {
    let windows = vec![
        WindowInfo {
            server_name: "server_a".to_string(),
            resource: make_resource("window://server_a.mcp.com/window1", "Window A1", None, None),
            read_result: ReadResourceResult {
                contents: vec![ResourceContents::text(
                    "Content A1",
                    "window://server_a.mcp.com/window1",
                )],
            },
        },
        WindowInfo {
            server_name: "server_b".to_string(),
            resource: make_resource("window://server_b.mcp.com/window1", "Window B1", None, None),
            read_result: ReadResourceResult {
                contents: vec![ResourceContents::text(
                    "Content B1",
                    "window://server_b.mcp.com/window1",
                )],
            },
        },
        WindowInfo {
            server_name: "server_a".to_string(),
            resource: make_resource(
                "window://server_a.mcp.com/window2?priority=50",
                "Window A2",
                None,
                None,
            ),
            read_result: ReadResourceResult {
                contents: vec![ResourceContents::text(
                    "Content A2",
                    "window://server_a.mcp.com/window2?priority=50",
                )],
            },
        },
    ];

    // 设置历史记录让server_b优先
    let history = vec![ToolCallRecord {
        server: "server_b".to_string(),
        tool: "test_tool".to_string(),
        timestamp: 1234567890,
        metadata: HashMap::new(),
    }];

    let result = organize_desktop(windows, None, &history);

    // server_b应该优先，然后是server_a（按priority排序）
    assert_eq!(result.len(), 3);
    assert!(result[0].contains("window://server_b.mcp.com/window1"));
    assert!(result[1].contains("window://server_a.mcp.com/window2?priority=50"));
    assert!(result[2].contains("window://server_a.mcp.com/window1"));
}

/// 测试复杂的内容渲染场景 / Test complex content rendering scenarios
#[test]
fn test_complex_content_rendering() {
    let windows = vec![WindowInfo {
        server_name: "server".to_string(),
        resource: make_resource(
            "window://server.mcp.com/complex",
            "Complex Window",
            None,
            None,
        ),
        read_result: ReadResourceResult {
            contents: vec![
                ResourceContents::text(
                    "First paragraph\nwith multiple lines",
                    "window://server.mcp.com/complex",
                ),
                ResourceContents::text("Second paragraph", "window://server.mcp.com/complex"),
            ],
        },
    }];

    let result = organize_desktop(windows, None, &[]);

    // 验证多个内容块被正确合并
    assert_eq!(result.len(), 1);
    assert!(result[0].contains("window://server.mcp.com/complex"));
    assert!(result[0].contains("First paragraph"));
    assert!(result[0].contains("with multiple lines"));
    assert!(result[0].contains("Second paragraph"));
}

/// 测试size限制在不同服务器间的行为 / Test size limit behavior across servers
#[test]
fn test_size_limit_across_servers() {
    let windows = vec![
        WindowInfo {
            server_name: "server_a".to_string(),
            resource: make_resource("window://server_a.mcp.com/window1", "Window A1", None, None),
            read_result: ReadResourceResult {
                contents: vec![ResourceContents::text(
                    "A1",
                    "window://server_a.mcp.com/window1",
                )],
            },
        },
        WindowInfo {
            server_name: "server_a".to_string(),
            resource: make_resource("window://server_a.mcp.com/window2", "Window A2", None, None),
            read_result: ReadResourceResult {
                contents: vec![ResourceContents::text(
                    "A2",
                    "window://server_a.mcp.com/window2",
                )],
            },
        },
        WindowInfo {
            server_name: "server_b".to_string(),
            resource: make_resource("window://server_b.mcp.com/window1", "Window B1", None, None),
            read_result: ReadResourceResult {
                contents: vec![ResourceContents::text(
                    "B1",
                    "window://server_b.mcp.com/window1",
                )],
            },
        },
    ];

    // 设置size=1，且server_a优先
    let history = vec![ToolCallRecord {
        server: "server_a".to_string(),
        tool: "test_tool".to_string(),
        timestamp: 1234567890,
        metadata: HashMap::new(),
    }];

    let result = organize_desktop(windows, Some(1), &history);

    // 应该只返回server_a的第一个窗口
    assert_eq!(result.len(), 1);
    assert!(result[0].contains("window://server_a.mcp.com/window1"));
}

/// 测试WindowURI解析错误处理 / Test WindowURI parsing error handling
#[test]
fn test_window_uri_parsing_errors() {
    let windows = vec![
        // 有效的窗口
        WindowInfo {
            server_name: "server".to_string(),
            resource: make_resource("window://server.mcp.com/valid", "Valid Window", None, None),
            read_result: ReadResourceResult {
                contents: vec![ResourceContents::text(
                    "Valid",
                    "window://server.mcp.com/valid",
                )],
            },
        },
        // 无效scheme的窗口
        WindowInfo {
            server_name: "server".to_string(),
            resource: make_resource(
                "http://server.mcp.com/invalid",
                "Invalid Window",
                None,
                None,
            ),
            read_result: ReadResourceResult {
                contents: vec![ResourceContents::text(
                    "Invalid",
                    "http://server.mcp.com/invalid",
                )],
            },
        },
        // 缺少host的窗口
        WindowInfo {
            server_name: "server".to_string(),
            resource: make_resource("window:///nohost", "No Host Window", None, None),
            read_result: ReadResourceResult {
                contents: vec![ResourceContents::text("No Host", "window:///nohost")],
            },
        },
    ];

    let result = organize_desktop(windows, None, &[]);

    // 只有有效的窗口应该被保留
    assert_eq!(result.len(), 1);
    assert!(result[0].contains("window://server.mcp.com/valid"));
    assert!(result[0].contains("Valid"));
}
