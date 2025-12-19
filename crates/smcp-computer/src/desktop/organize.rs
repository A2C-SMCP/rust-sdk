/**
* 文件名: organize.rs
* 作者: JQQ
* 创建日期: 2025/12/16
* 最后修改日期: 2025/12/16
* 版权: 2023 JQQ. All rights reserved.
* 依赖: None
* 描述: 桌面组织策略实现 / Desktop organizing strategy implementation
*/
use super::model::{ServerName, ToolCallRecord, WindowInfo};
use super::window_uri::WindowURI;
use super::Desktop;
use crate::mcp_clients::model::{ReadResourceResult, Resource, TextResourceContents};
use std::collections::{HashMap, HashSet};

/// 组织桌面内容 / Organize desktop content
///
/// 组织规则（来自 /desktop 工作流）：
/// Rules from /desktop workflow:
///   1) 若指定 window_uri，Manager 层已完成过滤；此处按一般规则组织即可。
///   2) 按最近工具调用历史对应的 MCP Server 倒序优先（最近使用的服务器优先）。
///   3) 同一 MCP Server 内，按 WindowURI.priority 降序推入（默认 0）。
///   4) 若遇到 fullscreen=True 的窗口，则该 MCP 仅推入这一个；若 size 仍有剩余，则进入下一个 MCP。
///   5) 全局按 size 截断（None 表示不限；size<=0 则返回空）。
///
/// # 参数 / Parameters
/// - windows: 窗口信息列表 / List of window information
/// - size: 期望返回的最大数量；None 表示全部 / Expected max number to return; None means all
/// - history: 最近的工具调用历史 / Recent tool call history
///
/// # 返回值 / Returns
/// 桌面内容列表 / List of desktop content
pub fn organize_desktop(
    windows: Vec<WindowInfo>,
    size: Option<usize>,
    history: &[ToolCallRecord],
) -> Vec<Desktop> {
    // 快速处理 size 边界 / Quick handling of size boundary
    if let Some(size) = size {
        if size == 0 {
            return Vec::new();
        }
    }

    // 1) 构建服务器 -> 窗口 列表映射，并解析 priority、fullscreen，保留原始序号以确定"第一个 fullscreen"
    //    同时过滤无内容的资源（contents 为空时跳过）。为后续渲染，保留 detail。
    let mut grouped: HashMap<ServerName, Vec<WindowItem>> = HashMap::new();

    for (idx, window) in windows.into_iter().enumerate() {
        // 过滤无内容的窗口 / Filter windows without content
        if window.read_result.contents.is_empty() {
            continue;
        }

        // 解析 WindowURI / Parse WindowURI
        let (priority, fullscreen) = match WindowURI::new(&window.resource.uri) {
            Ok(uri) => (uri.priority().unwrap_or(0), uri.fullscreen().unwrap_or(false)),
            Err(_) => {
                // 解析失败的资源，跳过 / Skip resources that failed to parse
                continue;
            }
        };

        let item = WindowItem {
            resource: window.resource,
            read_result: window.read_result,
            priority,
            fullscreen,
            original_index: idx,
        };

        grouped.entry(window.server_name).or_default().push(item);
    }

    // 2) 服务器优先级：根据最近工具调用历史，倒序去重
    let mut recent_servers: Vec<ServerName> = Vec::new();
    let mut seen: HashSet<ServerName> = HashSet::new();

    for rec in history.iter().rev() {
        if grouped.contains_key(&rec.server) && !seen.contains(&rec.server) {
            seen.insert(rec.server.clone());
            recent_servers.push(rec.server.clone());
        }
    }

    // 其余服务器（未在历史中出现）按名称稳定排序追加
    let mut remaining: Vec<ServerName> = grouped
        .keys()
        .filter(|s| !seen.contains(*s))
        .cloned()
        .collect();
    remaining.sort();

    let server_order: Vec<ServerName> = recent_servers.into_iter().chain(remaining).collect();

    // 3) 每个服务器内按 priority 降序排序
    for items in grouped.values_mut() {
        items.sort_by(|a, b| b.priority.cmp(&a.priority));
    }

    // 4) 组装按服务器顺序的窗口列表，处理 fullscreen 规则
    let mut result: Vec<Desktop> = Vec::new();
    let cap = size.unwrap_or(usize::MAX);

    for server in server_order {
        if result.len() >= cap {
            break;
        }

        if let Some(items) = grouped.get(&server) {
            // 若存在 fullscreen -> 选择"第一个出现的 fullscreen"（按原始 windows 序号最小）
            let fullscreen_items: Vec<&WindowItem> =
                items.iter().filter(|item| item.fullscreen).collect();

            if !fullscreen_items.is_empty() {
                // 找到原始序号最小的 fullscreen 窗口
                let fullscreen_item = fullscreen_items
                    .iter()
                    .min_by_key(|item| item.original_index)
                    .unwrap();

                result.push(render_desktop_item(
                    &fullscreen_item.resource,
                    &fullscreen_item.read_result,
                ));
                continue; // 该服务器仅加入这一个，转下一个服务器
            }

            // 否则按优先级加入多条直到 cap
            for item in items {
                if result.len() >= cap {
                    break;
                }
                result.push(render_desktop_item(&item.resource, &item.read_result));
            }
        }
    }

    result
}

/// 窗口项（内部使用） / Window item (internal use)
struct WindowItem {
    resource: Resource,
    read_result: ReadResourceResult,
    priority: i32,
    fullscreen: bool,
    original_index: usize,
}

/// 渲染桌面项 / Render desktop item
fn render_desktop_item(resource: &Resource, read_result: &ReadResourceResult) -> Desktop {
    let mut parts: Vec<String> = Vec::new();

    for content in &read_result.contents {
        let TextResourceContents { text, .. } = content;
        if !text.is_empty() {
            parts.push(text.clone());
        }
    }

    let body = parts.join("\n\n").trim().to_string();

    if body.is_empty() {
        resource.uri.clone()
    } else {
        format!("{}\n\n{}", resource.uri, body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_window(
        server: &str,
        uri: &str,
        content: &str,
        priority: i32,
        fullscreen: bool,
    ) -> WindowInfo {
        let mut final_uri = uri.to_string();
        let mut query: Vec<String> = Vec::new();
        if priority != 0 {
            query.push(format!("priority={}", priority));
        }
        if fullscreen {
            query.push("fullscreen=true".to_string());
        }
        if !query.is_empty() {
            final_uri.push('?');
            final_uri.push_str(&query.join("&"));
        }
        WindowInfo {
            server_name: server.to_string(),
            resource: Resource {
                uri: final_uri.clone(),
                name: format!("Window {}", final_uri),
                description: None,
                mime_type: None,
            },
            read_result: ReadResourceResult {
                contents: vec![TextResourceContents {
                    uri: final_uri.clone(),
                    text: content.to_string(),
                    mime_type: None,
                }],
            },
        }
    }

    #[test]
    fn test_organize_desktop_basic() {
        let windows = vec![
            create_test_window(
                "server1",
                "window://server1.mcp.com/window1",
                "Content 1",
                0,
                false,
            ),
            create_test_window(
                "server2",
                "window://server2.mcp.com/window1",
                "Content 2",
                0,
                false,
            ),
        ];

        let result = organize_desktop(windows, None, &[]);

        assert_eq!(result.len(), 2);
        assert!(result[0].contains("window://server1.mcp.com/window1"));
        assert!(result[0].contains("Content 1"));
        assert!(result[1].contains("window://server2.mcp.com/window1"));
        assert!(result[1].contains("Content 2"));
    }

    #[test]
    fn test_organize_desktop_with_size() {
        let windows = vec![
            create_test_window(
                "server1",
                "window://server1.mcp.com/window1",
                "Content 1",
                0,
                false,
            ),
            create_test_window(
                "server2",
                "window://server2.mcp.com/window1",
                "Content 2",
                0,
                false,
            ),
            create_test_window(
                "server3",
                "window://server3.mcp.com/window1",
                "Content 3",
                0,
                false,
            ),
        ];

        let result = organize_desktop(windows, Some(2), &[]);

        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_organize_desktop_with_priority() {
        let windows = vec![
            create_test_window(
                "server1",
                "window://server1.mcp.com/window1",
                "Content 1",
                1,
                false,
            ),
            create_test_window(
                "server1",
                "window://server1.mcp.com/window2",
                "Content 2",
                3,
                false,
            ),
            create_test_window(
                "server1",
                "window://server1.mcp.com/window3",
                "Content 3",
                2,
                false,
            ),
        ];

        let result = organize_desktop(windows, None, &[]);

        // 同一服务器内按 priority 降序
        assert!(result[0].contains("window://server1.mcp.com/window2")); // priority 3
        assert!(result[1].contains("window://server1.mcp.com/window3")); // priority 2
        assert!(result[2].contains("window://server1.mcp.com/window1")); // priority 1
    }

    #[test]
    fn test_organize_desktop_with_history() {
        let windows = vec![
            create_test_window(
                "server1",
                "window://server1.mcp.com/window1",
                "Content 1",
                0,
                false,
            ),
            create_test_window(
                "server2",
                "window://server2.mcp.com/window1",
                "Content 2",
                0,
                false,
            ),
        ];

        let history = vec![ToolCallRecord {
            server: "server2".to_string(),
            tool: "test_tool".to_string(),
            timestamp: 1234567890,
            metadata: HashMap::new(),
        }];

        let result = organize_desktop(windows, None, &history);

        // server2 在历史中，所以优先
        assert!(result[0].contains("window://server2.mcp.com/window1"));
        assert!(result[1].contains("window://server1.mcp.com/window1"));
    }

    #[test]
    fn test_organize_desktop_with_fullscreen() {
        let windows = vec![
            create_test_window(
                "server1",
                "window://server1.mcp.com/window1",
                "Content 1",
                0,
                false,
            ),
            create_test_window(
                "server1",
                "window://server1.mcp.com/window2",
                "Content 2",
                0,
                true,
            ),
            create_test_window(
                "server1",
                "window://server1.mcp.com/window3",
                "Content 3",
                0,
                false,
            ),
        ];

        let result = organize_desktop(windows, None, &[]);

        // 有 fullscreen 时，只返回一个 fullscreen 窗口
        assert_eq!(result.len(), 1);
        assert!(result[0].contains("window://server1.mcp.com/window2"));
    }

    #[test]
    fn test_organize_desktop_empty_content() {
        let windows = vec![WindowInfo {
            server_name: "server1".to_string(),
            resource: Resource {
                uri: "window://server1.mcp.com/window1".to_string(),
                name: "Window 1".to_string(),
                description: None,
                mime_type: None,
            },
            read_result: ReadResourceResult {
                contents: Vec::new(),
            },
        }];

        let result = organize_desktop(windows, None, &[]);

        // 无内容的窗口应该被过滤
        assert_eq!(result.len(), 0);
    }

    #[test]
    fn test_organize_desktop_size_zero_returns_empty() {
        let windows = vec![create_test_window(
            "server1",
            "window://server1.mcp.com/window1",
            "Content 1",
            0,
            false,
        )];

        let result = organize_desktop(windows, Some(0), &[]);

        // size=0 应该返回空
        assert_eq!(result.len(), 0);
    }

    #[test]
    fn test_organize_desktop_invalid_uri_is_skipped() {
        let windows = vec![
            WindowInfo {
                server_name: "server1".to_string(),
                resource: Resource {
                    uri: ":::this_is_not_a_uri".to_string(),
                    name: "Bad Window".to_string(),
                    description: None,
                    mime_type: None,
                },
                read_result: ReadResourceResult {
                    contents: vec![TextResourceContents {
                        uri: ":::this_is_not_a_uri".to_string(),
                        text: "bad".to_string(),
                        mime_type: None,
                    }],
                },
            },
            create_test_window("server1", "window://server1.mcp.com/good", "good", 0, false),
        ];

        let result = organize_desktop(windows, None, &[]);

        // 仅包含合法 URI
        assert_eq!(result.len(), 1);
        assert!(result[0].contains("window://server1.mcp.com/good"));
        assert!(result[0].contains("good"));
    }

    #[test]
    fn test_organize_desktop_multiple_fullscreen_picks_first() {
        let windows = vec![
            create_test_window(
                "server1",
                "window://server1.mcp.com/window1",
                "Content 1",
                0,
                true,
            ),
            create_test_window(
                "server1",
                "window://server1.mcp.com/window2",
                "Content 2",
                0,
                true,
            ),
            create_test_window(
                "server1",
                "window://server1.mcp.com/window3",
                "Content 3",
                0,
                false,
            ),
        ];

        let result = organize_desktop(windows, None, &[]);

        // 多个fullscreen时，只返回第一个（按原始顺序）
        assert_eq!(result.len(), 1);
        assert!(result[0].contains("window://server1.mcp.com/window1"));
    }

    #[test]
    fn test_organize_desktop_server_order_by_recent_history() {
        let windows = vec![
            create_test_window(
                "serverA",
                "window://serverA.mcp.com/window1",
                "Content A",
                1,
                false,
            ),
            create_test_window(
                "serverB",
                "window://serverB.mcp.com/window1",
                "Content B",
                1,
                false,
            ),
            create_test_window(
                "serverC",
                "window://serverC.mcp.com/window1",
                "Content C",
                1,
                false,
            ),
        ];

        // 最近使用顺序：C -> A（B 未使用）
        let history = vec![
            ToolCallRecord {
                server: "serverA".to_string(),
                tool: "test_tool".to_string(),
                timestamp: 1234567890,
                metadata: HashMap::new(),
            },
            ToolCallRecord {
                server: "serverC".to_string(),
                tool: "test_tool".to_string(),
                timestamp: 1234567891,
                metadata: HashMap::new(),
            },
        ];

        let result = organize_desktop(windows, None, &history);

        // C 在 A 前，剩余 B 按名称排序追加
        assert!(result[0].contains("window://serverC.mcp.com/window1"));
        assert!(result[1].contains("window://serverA.mcp.com/window1"));
        assert!(result[2].contains("window://serverB.mcp.com/window1"));
    }

    #[test]
    fn test_organize_desktop_fullscreen_one_per_server_then_next() {
        let windows = vec![
            create_test_window("serverA", "window://serverA.mcp.com/a1", "a1", 50, false),
            create_test_window(
                "serverA",
                "window://serverA.mcp.com/a2",
                "a2-full",
                10,
                true,
            ),
            create_test_window("serverA", "window://serverA.mcp.com/a3", "a3", 90, false),
            create_test_window("serverB", "window://serverB.mcp.com/b1", "b1", 5, false),
        ];

        // history 让 A 在前
        let history = vec![ToolCallRecord {
            server: "serverA".to_string(),
            tool: "test_tool".to_string(),
            timestamp: 1234567890,
            metadata: HashMap::new(),
        }];

        let result = organize_desktop(windows, None, &history);

        // A 只应输出 fullscreen 的 a2，然后进入 B
        assert!(result[0].contains("window://serverA.mcp.com/a2"));
        assert!(result[0].contains("a2-full"));
        assert!(result[1].contains("window://serverB.mcp.com/b1"));
        assert!(result[1].contains("b1"));
    }

    #[test]
    fn test_organize_desktop_server_level_cap_breaks_iteration() {
        let windows = vec![
            create_test_window("serverA", "window://serverA.mcp.com/a", "a", 0, false),
            create_test_window("serverB", "window://serverB.mcp.com/b", "b", 0, false),
        ];

        // 使用 history 让 A 在前，size=1 使得在进入 B 时触发服务器层级的 break
        let history = vec![ToolCallRecord {
            server: "serverA".to_string(),
            tool: "test_tool".to_string(),
            timestamp: 1234567890,
            metadata: HashMap::new(),
        }];

        let result = organize_desktop(windows, Some(1), &history);

        // 只应该有 A 的内容，B 不应该被处理
        assert_eq!(result.len(), 1);
        assert!(result[0].contains("window://serverA.mcp.com/a"));
    }

    #[test]
    fn test_organize_desktop_priority_boundary_values() {
        let windows = vec![
            create_test_window(
                "server1",
                "window://server1.mcp.com/window1",
                "Min",
                0,
                false,
            ),
            create_test_window(
                "server1",
                "window://server1.mcp.com/window2",
                "Max",
                100,
                false,
            ),
            create_test_window(
                "server1",
                "window://server1.mcp.com/window3",
                "Mid",
                50,
                false,
            ),
        ];

        let result = organize_desktop(windows, None, &[]);

        // 应该按 priority 降序：100 -> 50 -> 0
        assert!(result[0].contains("window://server1.mcp.com/window2"));
        assert!(result[0].contains("Max"));
        assert!(result[1].contains("window://server1.mcp.com/window3"));
        assert!(result[1].contains("Mid"));
        assert!(result[2].contains("window://server1.mcp.com/window1"));
        assert!(result[2].contains("Min"));
    }

    #[test]
    fn test_organize_desktop_history_with_nonexistent_server() {
        let windows = vec![
            create_test_window("serverA", "window://serverA.mcp.com/a", "a", 0, false),
            create_test_window("serverB", "window://serverB.mcp.com/b", "b", 0, false),
        ];

        // history 包含不存在的 server
        let history = vec![
            ToolCallRecord {
                server: "nonexistent".to_string(),
                tool: "test_tool".to_string(),
                timestamp: 1234567890,
                metadata: HashMap::new(),
            },
            ToolCallRecord {
                server: "serverB".to_string(),
                tool: "test_tool".to_string(),
                timestamp: 1234567891,
                metadata: HashMap::new(),
            },
        ];

        let result = organize_desktop(windows, None, &history);

        // serverB 应该优先（因为最近），serverA 次之
        assert!(result[0].contains("window://serverB.mcp.com/b"));
        assert!(result[1].contains("window://serverA.mcp.com/a"));
    }
}
