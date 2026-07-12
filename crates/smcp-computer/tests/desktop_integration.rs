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

use rmcp::model::{Annotations, Meta};
use smcp_computer::desktop::{organize_desktop, ToolCallRecord, WindowInfo};
use smcp_computer::mcp_clients::model::{
    make_resource, Annotated, RawResource, ReadResourceResult, ResourceContents,
};
use std::collections::HashMap;

/// 构造带 v0.2 下沉元数据的窗口资源 / Build a window resource carrying v0.2 sunk metadata。
///
/// 对齐 WIN-01 #16 / WIN-02 #18：`window://` URI 是**纯标识符**，不再承载 query；布局元数据下沉到
/// MCP `Resource.annotations.priority`（f32[0,1]，`None` = 不附带 annotations）与 `_meta.fullscreen`。
/// 与 `desktop::organize` 模块内单测的 `create_test_window` 同构。
/// Mirrors WIN-01/02: bare-identifier URI; priority → `annotations.priority`, fullscreen → `_meta.fullscreen`.
fn window_with_meta(
    server: &str,
    uri: &str,
    content: &str,
    priority: Option<f32>,
    fullscreen: bool,
) -> WindowInfo {
    let mut raw = RawResource::new(uri.to_string(), format!("Window {uri}"));
    if fullscreen {
        let mut map = serde_json::Map::new();
        map.insert("fullscreen".to_string(), serde_json::Value::Bool(true));
        raw.meta = Some(Meta(map));
    }
    let annotations = priority.map(|p| Annotations {
        audience: None,
        priority: Some(p),
        last_modified: None,
    });
    WindowInfo {
        bundle_id: server.to_string(),
        server_name: server.to_string(),
        resource: Annotated::new(raw, annotations),
        read_result: ReadResourceResult {
            contents: vec![ResourceContents::text(content, uri.to_string())],
        },
    }
}

/// 测试desktop模块与mcp_clients类型的集成 / Test integration between desktop and mcp_clients types
#[test]
fn test_desktop_with_mcp_clients_types() {
    // 使用 mcp_clients 类型创建窗口信息；priority/fullscreen 经 v0.2 下沉元数据承载（非 URI query）。
    let windows = vec![
        window_with_meta(
            "test_server",
            "window://test.mcp.com/window1",
            "Test content 1",
            Some(0.1),
            false,
        ),
        window_with_meta(
            "test_server",
            "window://test.mcp.com/window2",
            "Fullscreen content",
            None,
            true,
        ),
    ];

    let result = organize_desktop(windows, None, &[]);

    // 同一 server 内存在 _meta.fullscreen → 仅返回该 fullscreen 窗口（一条），URI 为纯标识符。
    assert_eq!(result.len(), 1);
    assert!(result[0].contains("window://test.mcp.com/window2"));
    assert!(result[0].contains("Fullscreen content"));
}

/// 测试多个服务器的窗口组织 / Test organizing windows from multiple servers
#[test]
fn test_multi_server_organization() {
    let windows = vec![
        window_with_meta(
            "server_a",
            "window://server_a.mcp.com/window1",
            "Content A1",
            None,
            false,
        ),
        window_with_meta(
            "server_b",
            "window://server_b.mcp.com/window1",
            "Content B1",
            None,
            false,
        ),
        // window2 经 annotations.priority=0.5 在 server_a 内排到 window1（缺省 0.0）之前。
        window_with_meta(
            "server_a",
            "window://server_a.mcp.com/window2",
            "Content A2",
            Some(0.5),
            false,
        ),
    ];

    // 设置历史记录让server_b优先
    let history = vec![ToolCallRecord {
        bundle_id: "server_b".to_string(),
        server: "server_b".to_string(),
        tool: "test_tool".to_string(),
        timestamp: 1234567890,
        metadata: HashMap::new(),
    }];

    let result = organize_desktop(windows, None, &history);

    // server_b 最近使用 → 优先；server_a 内按 annotations.priority 降序（window2=0.5 在 window1=0.0 前）。
    assert_eq!(result.len(), 3);
    assert!(result[0].contains("window://server_b.mcp.com/window1"));
    assert!(result[1].contains("window://server_a.mcp.com/window2"));
    assert!(result[2].contains("window://server_a.mcp.com/window1"));
}

/// 测试复杂的内容渲染场景 / Test complex content rendering scenarios
#[test]
fn test_complex_content_rendering() {
    let windows = vec![WindowInfo {
        bundle_id: "server".to_string(),
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
            bundle_id: "server_a".to_string(),
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
            bundle_id: "server_a".to_string(),
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
            bundle_id: "server_b".to_string(),
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
        bundle_id: "server_a".to_string(),
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
            bundle_id: "server".to_string(),
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
            bundle_id: "server".to_string(),
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
            bundle_id: "server".to_string(),
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
