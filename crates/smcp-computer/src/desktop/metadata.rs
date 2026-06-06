/*!
* 文件名: metadata.rs
* 作者: JQQ
* 创建日期: 2026/06/06
* 最后修改日期: 2026/06/06
* 版权: 2023 JQQ. All rights reserved.
* 依赖: rmcp, tracing
* 描述: Window/Desktop 资源元数据读取（v0.2 元数据下沉至 MCP `Resource.annotations`/`_meta`）
*       Window/Desktop resource metadata readers (v0.2: metadata sunk into MCP
*       `Resource.annotations` / `_meta`, replacing the old `window://` URI query).
*/

use crate::mcp_clients::model::Resource;

/// 从 `Resource.annotations.priority` 读取布局优先级（v0.2 协议指南 §6.2 / §6.4）。
/// Read layout priority from `Resource.annotations.priority` per protocol §6.2/§6.4.
///
/// 语义对标 Python `desktop/organize.py::_read_priority` 与 `base_client.list_windows` 的内联读取：
/// Semantics mirror Python `_read_priority` and the inline read in `base_client.list_windows`:
/// - `annotations` 缺失或 `priority` 缺失 → `0.0`（缺省）。
///   Missing `annotations` or missing `priority` → `0.0` (default).
/// - 越界（不在 `[0.0, 1.0]`，含 `NaN`）→ 记录 WARN 后回退 `0.0`。
///   Out-of-range (outside `[0.0, 1.0]`, incl. `NaN`) → WARN then fall back to `0.0`。
///
/// 注：rmcp `Annotations.priority` 已是强类型 `Option<f32>`，故无 Python 的"非数值类型"分支。
/// Note: rmcp `Annotations.priority` is already a typed `Option<f32>`, so there is no
/// "non-numeric" branch like Python's.
pub fn read_priority(resource: &Resource) -> f32 {
    match resource.priority() {
        None => 0.0,
        Some(p) if (0.0..=1.0).contains(&p) => p,
        Some(p) => {
            tracing::warn!(
                priority = p,
                "annotations.priority 越界 [0.0, 1.0]，按 0.0 处理 / out-of-range priority, treat as 0.0",
            );
            0.0
        }
    }
}

/// 从 `Resource._meta.fullscreen` 读取全屏标记（v0.2 协议指南 §6.2）。
/// Read the fullscreen flag from `Resource._meta.fullscreen` per protocol §6.2.
///
/// 语义对标 Python `desktop/organize.py::_read_fullscreen`：
/// Semantics mirror Python `_read_fullscreen`:
/// - `_meta` 缺失或缺 `fullscreen` 键 → `false`（缺省）。
///   Missing `_meta` or missing `fullscreen` key → `false` (default).
/// - 非布尔类型 → 记录 WARN 后回退 `false`。
///   Non-bool value → WARN then fall back to `false`。
pub fn read_fullscreen(resource: &Resource) -> bool {
    let Some(meta) = resource.meta.as_ref() else {
        return false;
    };
    match meta.0.get("fullscreen") {
        None => false,
        Some(serde_json::Value::Bool(b)) => *b,
        Some(other) => {
            tracing::warn!(
                value = %other,
                "_meta.fullscreen 非布尔类型，按 false 处理 / non-bool fullscreen, treat as false",
            );
            false
        }
    }
}

/// 校验 Window 资源的 `annotations.audience`（协议 §4.1，v0.2 仅 WARN 不过滤）。
/// Validate `annotations.audience` of a window resource (§4.1; v0.2 WARN-only, no filtering)。
///
/// 对标 Python `desktop/organize.py::_check_audience`：若声明了 `audience` 但不含 `assistant`，
/// 记录 WARN，资源仍纳入聚合（v0.3+ 可能硬过滤）。
/// Mirrors Python `_check_audience`: if `audience` is declared without `assistant`, log WARN but
/// still include the resource (v0.3+ may hard-filter)。
pub fn check_audience(resource: &Resource) {
    let Some(annotations) = resource.annotations.as_ref() else {
        return;
    };
    let Some(audience) = annotations.audience.as_ref() else {
        return;
    };
    if !audience
        .iter()
        .any(|r| matches!(r, rmcp::model::Role::Assistant))
    {
        tracing::warn!(
            audience = ?audience,
            "Window 资源 annotations.audience 不含 'assistant'（v0.2 仅 WARN，v0.3+ 可能硬过滤）/ \
             window resource missing 'assistant' in audience",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_clients::model::{Annotated, RawResource};
    use rmcp::model::{Annotations, Meta, Role};

    /// 构造带 `annotations.priority` 的窗口资源 / Build a window resource carrying
    /// `annotations.priority`。`None` 表示不附带 annotations。
    fn res_with_priority(priority: Option<f32>) -> Resource {
        let raw = RawResource::new("window://host/path", "win");
        let annotations = priority.map(|p| Annotations {
            audience: None,
            priority: Some(p),
            last_modified: None,
        });
        Annotated::new(raw, annotations)
    }

    /// 构造带 `_meta.fullscreen` 的窗口资源 / Build a window resource carrying
    /// `_meta.fullscreen`。`meta_value` 为写入 `_meta["fullscreen"]` 的原始 JSON（`None` = 无 `_meta`）。
    fn res_with_fullscreen_value(meta_value: Option<serde_json::Value>) -> Resource {
        let mut raw = RawResource::new("window://host/path", "win");
        if let Some(v) = meta_value {
            let mut map = serde_json::Map::new();
            map.insert("fullscreen".to_string(), v);
            raw.meta = Some(Meta(map));
        }
        Annotated::new(raw, None)
    }

    #[test]
    fn test_read_priority_absent_annotations_is_zero() {
        assert_eq!(read_priority(&res_with_priority(None)), 0.0);
    }

    #[test]
    fn test_read_priority_in_range() {
        assert_eq!(read_priority(&res_with_priority(Some(0.5))), 0.5);
    }

    #[test]
    fn test_read_priority_boundaries_ok() {
        assert_eq!(read_priority(&res_with_priority(Some(0.0))), 0.0);
        assert_eq!(read_priority(&res_with_priority(Some(1.0))), 1.0);
    }

    #[test]
    fn test_read_priority_out_of_range_falls_back_to_zero() {
        // 越界（>1.0 / <0.0）按 0.0 处理 / out-of-range clamps to 0.0
        assert_eq!(read_priority(&res_with_priority(Some(1.5))), 0.0);
        assert_eq!(read_priority(&res_with_priority(Some(-0.1))), 0.0);
    }

    #[test]
    fn test_read_priority_nan_falls_back_to_zero() {
        assert_eq!(read_priority(&res_with_priority(Some(f32::NAN))), 0.0);
    }

    #[test]
    fn test_read_fullscreen_absent_meta_is_false() {
        assert!(!read_fullscreen(&res_with_fullscreen_value(None)));
    }

    #[test]
    fn test_read_fullscreen_true_and_false() {
        assert!(read_fullscreen(&res_with_fullscreen_value(Some(
            serde_json::Value::Bool(true)
        ))));
        assert!(!read_fullscreen(&res_with_fullscreen_value(Some(
            serde_json::Value::Bool(false)
        ))));
    }

    #[test]
    fn test_read_fullscreen_non_bool_falls_back_to_false() {
        // 非布尔（字符串/数字）→ WARN + false
        assert!(!read_fullscreen(&res_with_fullscreen_value(Some(
            serde_json::Value::String("true".to_string())
        ))));
        assert!(!read_fullscreen(&res_with_fullscreen_value(Some(
            serde_json::json!(1)
        ))));
    }

    #[test]
    fn test_read_fullscreen_missing_key_is_false() {
        // _meta 存在但无 fullscreen 键 → false
        let raw = RawResource::new("window://host/path", "win");
        let mut map = serde_json::Map::new();
        map.insert("other".to_string(), serde_json::Value::Bool(true));
        let mut raw = raw;
        raw.meta = Some(Meta(map));
        let res = Annotated::new(raw, None);
        assert!(!read_fullscreen(&res));
    }

    #[test]
    fn test_check_audience_does_not_panic() {
        // 无 annotations / 含 assistant / 仅 user —— 均不应 panic（WARN 为副作用）。
        check_audience(&res_with_priority(None));

        let raw = RawResource::new("window://host/path", "win");
        let with_assistant = Annotated::new(
            raw.clone(),
            Some(Annotations {
                audience: Some(vec![Role::Assistant]),
                priority: None,
                last_modified: None,
            }),
        );
        check_audience(&with_assistant);

        let user_only = Annotated::new(
            raw,
            Some(Annotations {
                audience: Some(vec![Role::User]),
                priority: None,
                last_modified: None,
            }),
        );
        check_audience(&user_only);
    }
}
