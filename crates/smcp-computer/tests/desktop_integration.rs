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
use smcp_computer::mcp_clients::model::{ReadResourceResult, Resource, TextResourceContents};
use std::collections::HashMap;

/// 测试desktop模块与mcp_clients类型的集成 / Test integration between desktop and mcp_clients types
#[test]
fn test_desktop_with_mcp_clients_types() {
    // 使用mcp_clients中定义的类型创建窗口信息
    let windows = vec![
        WindowInfo {
            server_name: "test_server".to_string(),
            resource: Resource {
                uri: "window://test.mcp.com/window1?priority=10".to_string(),
                name: "Test Window 1".to_string(),
                description: Some("A test window".to_string()),
                mime_type: Some("text/plain".to_string()),
            },
            read_result: ReadResourceResult {
                contents: vec![TextResourceContents {
                    uri: "window://test.mcp.com/window1?priority=10".to_string(),
                    text: "Test content 1".to_string(),
                    mime_type: Some("text/plain".to_string()),
                }],
            },
        },
        WindowInfo {
            server_name: "test_server".to_string(),
            resource: Resource {
                uri: "window://test.mcp.com/window2?fullscreen=true".to_string(),
                name: "Test Window 2".to_string(),
                description: None,
                mime_type: None,
            },
            read_result: ReadResourceResult {
                contents: vec![TextResourceContents {
                    uri: "window://test.mcp.com/window2?fullscreen=true".to_string(),
                    text: "Fullscreen content".to_string(),
                    mime_type: None,
                }],
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
            resource: Resource {
                uri: "window://server_a.mcp.com/window1".to_string(),
                name: "Window A1".to_string(),
                description: None,
                mime_type: None,
            },
            read_result: ReadResourceResult {
                contents: vec![TextResourceContents {
                    uri: "window://server_a.mcp.com/window1".to_string(),
                    text: "Content A1".to_string(),
                    mime_type: None,
                }],
            },
        },
        WindowInfo {
            server_name: "server_b".to_string(),
            resource: Resource {
                uri: "window://server_b.mcp.com/window1".to_string(),
                name: "Window B1".to_string(),
                description: None,
                mime_type: None,
            },
            read_result: ReadResourceResult {
                contents: vec![TextResourceContents {
                    uri: "window://server_b.mcp.com/window1".to_string(),
                    text: "Content B1".to_string(),
                    mime_type: None,
                }],
            },
        },
        WindowInfo {
            server_name: "server_a".to_string(),
            resource: Resource {
                uri: "window://server_a.mcp.com/window2?priority=50".to_string(),
                name: "Window A2".to_string(),
                description: None,
                mime_type: None,
            },
            read_result: ReadResourceResult {
                contents: vec![TextResourceContents {
                    uri: "window://server_a.mcp.com/window2?priority=50".to_string(),
                    text: "Content A2".to_string(),
                    mime_type: None,
                }],
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
        resource: Resource {
            uri: "window://server.mcp.com/complex".to_string(),
            name: "Complex Window".to_string(),
            description: None,
            mime_type: None,
        },
        read_result: ReadResourceResult {
            contents: vec![
                TextResourceContents {
                    uri: "window://server.mcp.com/complex".to_string(),
                    text: "First paragraph\nwith multiple lines".to_string(),
                    mime_type: Some("text/plain".to_string()),
                },
                TextResourceContents {
                    uri: "window://server.mcp.com/complex".to_string(),
                    text: "Second paragraph".to_string(),
                    mime_type: Some("text/markdown".to_string()),
                },
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
            resource: Resource {
                uri: "window://server_a.mcp.com/window1".to_string(),
                name: "Window A1".to_string(),
                description: None,
                mime_type: None,
            },
            read_result: ReadResourceResult {
                contents: vec![TextResourceContents {
                    uri: "window://server_a.mcp.com/window1".to_string(),
                    text: "A1".to_string(),
                    mime_type: None,
                }],
            },
        },
        WindowInfo {
            server_name: "server_a".to_string(),
            resource: Resource {
                uri: "window://server_a.mcp.com/window2".to_string(),
                name: "Window A2".to_string(),
                description: None,
                mime_type: None,
            },
            read_result: ReadResourceResult {
                contents: vec![TextResourceContents {
                    uri: "window://server_a.mcp.com/window2".to_string(),
                    text: "A2".to_string(),
                    mime_type: None,
                }],
            },
        },
        WindowInfo {
            server_name: "server_b".to_string(),
            resource: Resource {
                uri: "window://server_b.mcp.com/window1".to_string(),
                name: "Window B1".to_string(),
                description: None,
                mime_type: None,
            },
            read_result: ReadResourceResult {
                contents: vec![TextResourceContents {
                    uri: "window://server_b.mcp.com/window1".to_string(),
                    text: "B1".to_string(),
                    mime_type: None,
                }],
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
            resource: Resource {
                uri: "window://server.mcp.com/valid".to_string(),
                name: "Valid Window".to_string(),
                description: None,
                mime_type: None,
            },
            read_result: ReadResourceResult {
                contents: vec![TextResourceContents {
                    uri: "window://server.mcp.com/valid".to_string(),
                    text: "Valid".to_string(),
                    mime_type: None,
                }],
            },
        },
        // 无效scheme的窗口
        WindowInfo {
            server_name: "server".to_string(),
            resource: Resource {
                uri: "http://server.mcp.com/invalid".to_string(),
                name: "Invalid Window".to_string(),
                description: None,
                mime_type: None,
            },
            read_result: ReadResourceResult {
                contents: vec![TextResourceContents {
                    uri: "http://server.mcp.com/invalid".to_string(),
                    text: "Invalid".to_string(),
                    mime_type: None,
                }],
            },
        },
        // 缺少host的窗口
        WindowInfo {
            server_name: "server".to_string(),
            resource: Resource {
                uri: "window:///nohost".to_string(),
                name: "No Host Window".to_string(),
                description: None,
                mime_type: None,
            },
            read_result: ReadResourceResult {
                contents: vec![TextResourceContents {
                    uri: "window:///nohost".to_string(),
                    text: "No Host".to_string(),
                    mime_type: None,
                }],
            },
        },
    ];

    let result = organize_desktop(windows, None, &[]);

    // 只有有效的窗口应该被保留
    assert_eq!(result.len(), 1);
    assert!(result[0].contains("window://server.mcp.com/valid"));
    assert!(result[0].contains("Valid"));
}
