/**
* 文件名: organize.rs
* 作者: JQQ
* 创建日期: 2025/12/16
* 最后修改日期: 2025/12/16
* 版权: 2023 JQQ. All rights reserved.
* 依赖: None
* 描述: 桌面组织策略实现 / Desktop organizing strategy implementation
*/
use super::metadata::{check_audience, cmp_priority_desc, read_fullscreen, read_priority};
use super::model::{ToolCallRecord, WindowInfo};
use super::window_uri::{is_window_uri, WindowURI};
use super::Desktop;
use crate::mcp_clients::model::{resource_contents_as_text, ReadResourceResult, Resource};
use std::collections::{HashMap, HashSet};

/// 组织桌面内容 / Organize desktop content
///
/// 组织规则（来自 /desktop 工作流）：
/// Rules from /desktop workflow:
///   1) 若指定 window_uri，Manager 层已完成过滤；此处按一般规则组织即可。
///   2) 按最近工具调用历史对应的 MCP Server 倒序优先（最近使用的服务器优先）。
///   3) 同一 MCP Server 内，按 `Resource.annotations.priority`（`f32[0,1]`，缺省 0.0）降序推入；
///      URI query 不再承载排序信息（v0.2 元数据下沉）。
///   4) 若 `Resource._meta.fullscreen=true` 的窗口，则该 MCP 仅推入这一个；若 size 仍有剩余，则进入下一个 MCP。
///   5) 全局按 size 截断（None 表示不限；size<=0 则返回空）。
///
/// # 参数 / Parameters
/// - windows: 窗口信息列表 / List of window information
/// - size: 期望返回的最大数量；None 表示全部 / Expected max number to return; None means all
/// - history: 最近的工具调用历史 / Recent tool call history
///
/// # 返回值 / Returns
/// 桌面内容列表 / List of desktop content
///
/// **recency 关联现状（#118 备案）**：第 2 步「按 `history` 的 `rec.bundle_id` 排最近使用优先」当前仅在**单测**中
/// 生效——**唯一生产调用点**（`socketio_client::handle_get_desktop_with_ack`）传入空 `history`（`&[]`）。且运行期真实
/// 工具调用历史用的是 [`crate::computer::ToolCallRecord`]（另一类型，无 `bundle_id` 字段、从不喂给本函数）。若未来要
/// 让 recency 在生产生效，须把真实历史桥接进来并按 `bundle_id` 关联——现状 `history` 空时该步为 no-op（不影响分组）。
pub fn organize_desktop(
    windows: Vec<WindowInfo>,
    size: Option<usize>,
    history: &[ToolCallRecord],
) -> Vec<Desktop> {
    // MCP-01：`window://` host **SHOULD** 跨 MCP Server 唯一。冲突仅记 lint-style WARN（不阻塞组织）。
    // desktop 组织按 **bundle_id** 分组、不依赖 host 唯一，故冲突不影响下方逻辑，仅作引导。
    // MCP-01: window:// host SHOULD be unique across MCP servers; collisions only WARN (non-blocking).
    for (host, servers) in detect_host_collisions(&windows) {
        tracing::warn!(
            host = %host,
            bundle_ids = ?servers,
            "window:// host 跨 MCP Server 冲突（SHOULD 唯一，仅 lint 引导，不阻塞）/ \
             window:// host collides across MCP servers (SHOULD be unique; lint-only, non-blocking)"
        );
    }

    // 快速处理 size 边界 / Quick handling of size boundary
    if let Some(size) = size {
        if size == 0 {
            return Vec::new();
        }
    }

    // 1) 构建 **bundle_id → 窗口列表** 映射（键为 server 身份键、非 display 名），并解析 priority、fullscreen，
    //    保留原始序号以确定"第一个 fullscreen"；同时过滤无内容的资源（contents 为空时跳过）。为后续渲染，保留 detail。
    let mut grouped: HashMap<String, Vec<WindowItem>> = HashMap::new();

    for (idx, window) in windows.into_iter().enumerate() {
        // 过滤无内容的窗口 / Filter windows without content
        if window.read_result.contents.is_empty() {
            continue;
        }

        // URI 仅作合法性守卫（必须是 window:// scheme）；元数据不再来自 query。
        // URI is only a scheme guard (must be window://); metadata no longer comes from query.
        if !is_window_uri(&window.resource.uri) {
            continue;
        }

        // v0.2 元数据下沉：priority ← Resource.annotations.priority（f32[0,1]，缺省 0.0）；
        // fullscreen ← Resource._meta.fullscreen（缺省 false）。audience 仅 WARN 校验，不过滤。
        check_audience(&window.resource);
        let priority = read_priority(&window.resource);
        let fullscreen = read_fullscreen(&window.resource);

        let item = WindowItem {
            resource: window.resource,
            read_result: window.read_result,
            priority,
            fullscreen,
            original_index: idx,
        };

        // 协议 0.3.0 §身份正交性（#18）：**按 bundle_id 分组**（server 唯一身份），避免同名 server 窗口误并。
        grouped.entry(window.bundle_id).or_default().push(item);
    }

    // 2) 服务器优先级：根据最近工具调用历史，倒序去重（按 bundle_id 关联，与分组键一致）
    let mut recent_servers: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for rec in history.iter().rev() {
        if grouped.contains_key(&rec.bundle_id) && !seen.contains(&rec.bundle_id) {
            seen.insert(rec.bundle_id.clone());
            recent_servers.push(rec.bundle_id.clone());
        }
    }

    // 其余服务器（未在历史中出现）按 bundle_id 字典序稳定排序追加
    let mut remaining: Vec<String> = grouped
        .keys()
        .filter(|s| !seen.contains(*s))
        .cloned()
        .collect();
    remaining.sort();

    let server_order: Vec<String> = recent_servers.into_iter().chain(remaining).collect();

    // 3) 每个服务器内按 priority 降序排序（共享比较器，单源化 f32 NaN→Equal 语义）。
    for items in grouped.values_mut() {
        items.sort_by(|a, b| cmp_priority_desc(a.priority, b.priority));
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

/// 检测跨 MCP Server 的 `window://` host 冲突（MCP-01）/ detect cross-server window:// host collisions。
///
/// host **SHOULD**（非 MUST）跨 MCP Server 唯一（协议 desktop §host）。本函数为**纯检测**：返回被
/// **≥2 个不同 MCP Server**（按 **`bundle_id`** 判「不同」）暴露的 host 及其 `bundle_id` 集合（host 升序、
/// `bundle_id` 升序，稳定输出），由调用方据此发 lint-style WARN——**不阻塞**注册/组织。desktop 组织按
/// `bundle_id` 分组、不依赖 host 唯一，故冲突不影响功能。Rust 侧从未构建 host 反向索引（无
/// `HostConflictError`/`find_server_by_host`），对齐 Python 参考实现「host 唯一性降级为 WARN、移除反向索引」。
///
/// **#127**：按 `bundle_id` 而非 display 名去重——两个 display 名相同、`bundle_id` 不同的合法共存 server
/// 暴露同一 host 时，按名去重会折成一条、`len() > 1` 不成立 → **漏报**该冲突。
///
/// Pure detector: returns hosts exposed by ≥2 distinct MCP servers (distinctness by `bundle_id`; sorted,
/// stable). Callers emit a non-blocking lint WARN; desktop organizing groups by `bundle_id` and does not
/// depend on host uniqueness. No reverse host index exists (mirrors the Python reference).
pub(crate) fn detect_host_collisions(windows: &[WindowInfo]) -> Vec<(String, Vec<String>)> {
    use std::collections::BTreeMap;

    let mut by_host: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for w in windows {
        // 仅 window:// 资源参与 host 唯一性检查；非法/非 window URI 跳过（与 organize 守卫一致）。
        if !is_window_uri(&w.resource.uri) {
            continue;
        }
        let Ok(uri) = WindowURI::new(&w.resource.uri) else {
            continue;
        };
        let servers = by_host.entry(uri.mcp_id().to_string()).or_default();
        if !servers.contains(&w.bundle_id) {
            servers.push(w.bundle_id.clone());
        }
    }

    by_host
        .into_iter()
        .filter(|(_, servers)| servers.len() > 1)
        .map(|(host, mut servers)| {
            servers.sort();
            (host, servers)
        })
        .collect()
}

/// 窗口项（内部使用） / Window item (internal use)
struct WindowItem {
    resource: Resource,
    read_result: ReadResourceResult,
    /// 布局优先级 / layout priority（`Resource.annotations.priority`，f32[0,1]，缺省 0.0）。
    priority: f32,
    fullscreen: bool,
    original_index: usize,
}

/// 渲染桌面项 / Render desktop item
fn render_desktop_item(resource: &Resource, read_result: &ReadResourceResult) -> Desktop {
    let mut parts: Vec<String> = Vec::new();

    for content in &read_result.contents {
        if let Some(text) = resource_contents_as_text(content) {
            if !text.is_empty() {
                parts.push(text.to_string());
            }
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
    use crate::mcp_clients::model::{make_resource, ResourceContents};

    /// 构造测试窗口 / Build a test window。
    ///
    /// v0.2 元数据下沉：`priority` / `fullscreen` 写入 `Resource.annotations.priority`（归一化为
    /// f32[0,1]）与 `_meta.fullscreen`，**不再编码进 URI query**。为保留既有用例的 0-100 直觉，
    /// 入参仍用 `i32`（0-100），内部除以 100 归一化。
    fn create_test_window(
        server: &str,
        uri: &str,
        content: &str,
        priority: i32,
        fullscreen: bool,
    ) -> WindowInfo {
        use crate::mcp_clients::model::Resource;
        use rmcp::model::{Annotations, Meta};

        let mut resource = Resource::new(uri.to_string(), format!("Window {}", uri));
        if fullscreen {
            let mut map = serde_json::Map::new();
            map.insert("fullscreen".to_string(), serde_json::Value::Bool(true));
            resource.meta = Some(Meta(map));
        }
        resource.annotations = Some(Annotations::default().with_priority(priority as f32 / 100.0));

        WindowInfo {
            // 测试中 server 串同时充当 bundle_id（分组键）与 server_name（展示名）。
            bundle_id: server.to_string(),
            server_name: server.to_string(),
            resource,
            read_result: ReadResourceResult::new(vec![ResourceContents::text(
                content,
                uri.to_string(),
            )]),
        }
    }

    /// #118 §身份正交性回归：两窗 `server_name` 相同、`bundle_id` 不同 → 按 **bundle_id** 分组**不合并**。
    /// 用 fullscreen 制造区分力：误按 name 分组 → 合并 1 组 → 仅取 1 个 fullscreen → 输出 1 条（错）；
    /// 按 bundle_id 分组 → 2 组 → 各取 1 fullscreen → 输出 2 条（对）。window://host 各自 MCP 自选、正交保留。
    #[test]
    fn desktop_groups_by_bundle_id_not_name_118() {
        let mut w1 = create_test_window("x", "window://a.mcp.com/w", "A", 0, true);
        w1.server_name = "shared".to_string();
        w1.bundle_id = "bundle_a".to_string();
        let mut w2 = create_test_window("x", "window://b.mcp.com/w", "B", 0, true);
        w2.server_name = "shared".to_string();
        w2.bundle_id = "bundle_b".to_string();

        let result = organize_desktop(vec![w1, w2], None, &[]);
        assert_eq!(
            result.len(),
            2,
            "同名不同 bundle_id 应各成一组、不合并: {result:?}"
        );
        let joined = result.join("\n");
        assert!(joined.contains("window://a.mcp.com/w") && joined.contains("window://b.mcp.com/w"));
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
            bundle_id: "server2".to_string(),
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
            bundle_id: "server1".to_string(),
            server_name: "server1".to_string(),
            resource: make_resource("window://server1.mcp.com/window1", "Window 1", None, None),
            read_result: ReadResourceResult::new(Vec::new()),
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
                bundle_id: "server1".to_string(),
                server_name: "server1".to_string(),
                resource: make_resource(":::this_is_not_a_uri", "Bad Window", None, None),
                read_result: ReadResourceResult::new(vec![ResourceContents::text(
                    "bad",
                    ":::this_is_not_a_uri",
                )]),
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
                bundle_id: "serverA".to_string(),
                server: "serverA".to_string(),
                tool: "test_tool".to_string(),
                timestamp: 1234567890,
                metadata: HashMap::new(),
            },
            ToolCallRecord {
                bundle_id: "serverC".to_string(),
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
            bundle_id: "serverA".to_string(),
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
            bundle_id: "serverA".to_string(),
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
    fn test_organize_desktop_default_priority_when_no_annotations() {
        use crate::mcp_clients::model::Resource;

        // 无 annotations 的窗口：priority 视为缺省 0.0，应排在高 priority 窗口之后。
        let bare = WindowInfo {
            bundle_id: "server1".to_string(),
            server_name: "server1".to_string(),
            resource: Resource::new("window://server1.mcp.com/no-ann", "no-ann"),
            read_result: ReadResourceResult::new(vec![ResourceContents::text(
                "bare",
                "window://server1.mcp.com/no-ann",
            )]),
        };
        let high = create_test_window(
            "server1",
            "window://server1.mcp.com/high",
            "high",
            90,
            false,
        );

        let result = organize_desktop(vec![bare, high], None, &[]);

        assert_eq!(result.len(), 2);
        // priority 0.9 在前，缺省 0.0 在后
        assert!(result[0].contains("window://server1.mcp.com/high"));
        assert!(result[1].contains("window://server1.mcp.com/no-ann"));
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
                bundle_id: "nonexistent".to_string(),
                server: "nonexistent".to_string(),
                tool: "test_tool".to_string(),
                timestamp: 1234567890,
                metadata: HashMap::new(),
            },
            ToolCallRecord {
                bundle_id: "serverB".to_string(),
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

    // ===== MCP-01: host 唯一性 lint（SHOULD WARN，纯检测，不阻塞）=====

    #[test]
    fn test_detect_host_collision_across_servers() {
        // 同一 host `shared.app` 被两个不同 MCP Server 暴露 → 视为冲突。
        let windows = vec![
            create_test_window("serverA", "window://shared.app/w1", "a", 0, false),
            create_test_window("serverB", "window://shared.app/w2", "b", 0, false),
        ];

        let collisions = detect_host_collisions(&windows);
        assert_eq!(collisions.len(), 1);
        assert_eq!(collisions[0].0, "shared.app");
        // server 名升序稳定输出
        assert_eq!(collisions[0].1, vec!["serverA", "serverB"]);
    }

    /// #127 扫漏：host 冲突检测按 **bundle_id** 判「不同 server」，非 display 名。
    ///
    /// 两个 display 名相同、`bundle_id` 不同的合法共存 server 暴露同一 host：旧实现按 display 名去重
    /// 折成一条 → `servers.len() > 1` 不成立 → **漏报**冲突。lint-only 路径，影响为漏诊断而非路由错。
    #[test]
    fn detect_host_collisions_distinguishes_by_bundle_id_127() {
        let mut w1 = create_test_window("x", "window://shared.app/w1", "a", 0, false);
        w1.server_name = "same-display-name".to_string();
        w1.bundle_id = "bundle_a".to_string();
        let mut w2 = create_test_window("x", "window://shared.app/w2", "b", 0, false);
        w2.server_name = "same-display-name".to_string();
        w2.bundle_id = "bundle_b".to_string();

        let collisions = detect_host_collisions(&[w1, w2]);
        assert_eq!(
            collisions.len(),
            1,
            "两个不同 bundle_id 的 server 暴露同一 host 应报冲突（旧实现按 display 名折叠 → 漏报）"
        );
        assert_eq!(collisions[0].0, "shared.app");
        assert_eq!(
            collisions[0].1,
            vec!["bundle_a", "bundle_b"],
            "应以身份键 bundle_id 报告涉事 server"
        );
    }

    #[test]
    fn test_no_collision_same_server_same_host() {
        // 同一 server 多个窗口共享 host —— 单 server 内 host 唯一由 MCP 自身保证，**不**算冲突。
        let windows = vec![
            create_test_window("serverA", "window://app.one/w1", "a", 0, false),
            create_test_window("serverA", "window://app.one/w2", "b", 0, false),
        ];

        assert!(detect_host_collisions(&windows).is_empty());
    }

    #[test]
    fn test_no_collision_distinct_hosts() {
        // 不同 server 各用不同 host —— 合规，无冲突。
        let windows = vec![
            create_test_window("serverA", "window://serverA.app/w", "a", 0, false),
            create_test_window("serverB", "window://serverB.app/w", "b", 0, false),
        ];

        assert!(detect_host_collisions(&windows).is_empty());
    }

    #[test]
    fn test_organize_still_succeeds_under_host_collision() {
        // 冲突仅 WARN（lint-style），**不阻塞**：两个 server 的窗口仍照常组织进桌面。
        let windows = vec![
            create_test_window("serverA", "window://shared.app/w1", "Content A", 0, false),
            create_test_window("serverB", "window://shared.app/w2", "Content B", 0, false),
        ];

        // 前置断言：的确存在 host 冲突（驱动 WARN 的检测结果非空）。
        assert_eq!(detect_host_collisions(&windows).len(), 1);

        let result = organize_desktop(windows, None, &[]);
        assert_eq!(result.len(), 2);
        assert!(result.iter().any(|d| d.contains("Content A")));
        assert!(result.iter().any(|d| d.contains("Content B")));
    }
}
