/*!
* 文件名: blob_sideband.rs
* 作者: JQQ
* 创建日期: 2026/06/10
* 最后修改日期: 2026/06/10
* 版权: 2023 JQQ. All rights reserved.
* 依赖: smcp, serde_json, base64
* 描述: Agent tool_call 二进制旁路 extract/inject 纯函数 / Pure tool_call binary-sideband helpers
*/

//! `client:tool_call` 二进制旁路（`_meta.a2c_blob_handle`）消费的纯函数（AGT-04 #41）。
//! 对标 Python `a2c_smcp/agent/_blob_sideband.py`（`extract_sideband_handles` /
//! `inject_payload_into_content_item`）。
//!
//! 协议依据 / Protocol：`blob-transfer.md` §5——`client:tool_call` 返回原生 `CallToolResult`，超内联
//! 预算的二进制 content item 清空内联 `data`/`blob`、在其 `_meta` 写 `a2c_blob_handle`
//! （+`a2c_total_size`/`a2c_sha256`）。消费侧 MUST 遍历 content item 检测句柄并 drain 回填，否则大
//! 二进制静默变空（⚠️ 这是 AGT-04 的破坏性语义变化）。
//!
//! Rust 与 Python 的结构差异：Python 在一次迭代里 `yield (item_mut, meta_mut, handle)` 并就地改；Rust
//! 受借用规则约束（drain 是跨 `.await` 的异步 I/O，不能在迭代中持有 `&mut Value`），故拆成
//! **extract（只读收集索引+句柄）→ drain（无 Value 借用跨 await）→ inject（可变回填，无 await）**三段，
//! 由 [`crate::async_agent`] 编排，语义与 Python 等价。

use base64::Engine as _;
use serde_json::Value;

/// 从 `CallToolResult` 收集带二进制旁路句柄的 content item：返回 `(item 索引, blob_handle)` 列表。
///
/// 复用 smcp 核心的 [`smcp::read_content_blob_sideband`]（读 content item 子级 `_meta.a2c_blob_handle`，
/// 非空字符串才命中）。非对象 raw / 非数组 `content` / 无句柄的 item 一律跳过——**不**纳入回填集合，
/// 保证无旁路句柄的 item 路径完全不变（对齐 Python `extract_sideband_handles`）。
pub(crate) fn extract_sideband_handles(raw: &Value) -> Vec<(usize, String)> {
    let Some(content) = raw.get("content").and_then(Value::as_array) else {
        return Vec::new();
    };
    content
        .iter()
        .enumerate()
        .filter_map(|(idx, item)| {
            smcp::read_content_blob_sideband(item).map(|sb| (idx, sb.blob_handle))
        })
        .collect()
}

/// 把 drain 回的字节回填到一个 content item 的内联载体，并清理其 `_meta` 的 `a2c_*` 旁路键。
///
/// 载体选择（对齐 Python `inject_payload_into_content_item`）：
/// - `ImageContent` / `AudioContent`（含 `data` 键，或 `type` ∈ {image, audio}）→ 顶层 `data`；
/// - `EmbeddedResource`（`resource.blob` 存在）→ `resource.blob`；
/// - 未知形态兜底 → 顶层 `data`（避免静默丢字节）。
///
/// 回填后从 `_meta` 移除所有 `a2c_` 前缀键；若 `_meta` 因此清空则整键移除——消费完成后该 item 与
/// 「小尺寸直接内联」的结果**无从区分**（透明性不变量）。非对象 item 安全 no-op。
pub(crate) fn inject_payload_into_content_item(item: &mut Value, payload: &[u8]) {
    let Some(obj) = item.as_object_mut() else {
        return;
    };
    let b64 = base64::engine::general_purpose::STANDARD.encode(payload);

    let is_image_audio = obj.contains_key("data")
        || matches!(
            obj.get("type").and_then(Value::as_str),
            Some("image") | Some("audio")
        );
    let has_embedded_blob = obj.get("resource").and_then(|r| r.get("blob")).is_some();

    if is_image_audio {
        obj.insert("data".to_string(), Value::String(b64));
    } else if has_embedded_blob {
        if let Some(res) = obj.get_mut("resource").and_then(Value::as_object_mut) {
            res.insert("blob".to_string(), Value::String(b64));
        }
    } else {
        // 兜底：未知形态写顶层 `data`，绝不静默丢字节（对齐 Python fallback）。
        obj.insert("data".to_string(), Value::String(b64));
    }

    // 清理旁路元数据：移除 `_meta` 中所有 `a2c_*` 键；空 `_meta` 整键移除（透明性）。
    if let Some(meta) = obj.get_mut("_meta").and_then(Value::as_object_mut) {
        meta.retain(|k, _| !k.starts_with("a2c_"));
        if meta.is_empty() {
            obj.remove("_meta");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn b64(s: &str) -> String {
        base64::engine::general_purpose::STANDARD.encode(s.as_bytes())
    }

    // ── extract_sideband_handles ─────────────────────────────────────────────

    #[test]
    fn test_extract_collects_only_items_with_handles() {
        let raw = json!({
            "content": [
                { "type": "text", "text": "no handle here" },
                {
                    "type": "image",
                    "mimeType": "image/png",
                    "data": "",
                    "_meta": { "a2c_blob_handle": "ts:img", "a2c_total_size": 1024 }
                },
                {
                    "type": "resource",
                    "resource": { "mimeType": "application/pdf", "blob": "" },
                    "_meta": { "a2c_blob_handle": "ts:pdf" }
                }
            ]
        });
        let handles = extract_sideband_handles(&raw);
        assert_eq!(
            handles,
            vec![(1, "ts:img".to_string()), (2, "ts:pdf".to_string())]
        );
    }

    #[test]
    fn test_extract_empty_when_no_content_or_no_handles() {
        assert!(extract_sideband_handles(&json!({})).is_empty());
        assert!(extract_sideband_handles(&json!({ "content": "notarray" })).is_empty());
        let no_handles = json!({ "content": [{ "type": "text", "text": "x" }] });
        assert!(extract_sideband_handles(&no_handles).is_empty());
        // _meta 存在但无 a2c_blob_handle → 跳过
        let meta_no_handle = json!({ "content": [{ "data": "z", "_meta": { "k": "v" } }] });
        assert!(extract_sideband_handles(&meta_no_handle).is_empty());
    }

    // ── inject_payload_into_content_item ─────────────────────────────────────

    #[test]
    fn test_inject_image_writes_top_level_data_and_strips_a2c_meta() {
        let mut item = json!({
            "type": "image",
            "mimeType": "image/png",
            "data": "",
            "_meta": { "a2c_blob_handle": "ts:img", "a2c_total_size": 5, "a2c_sha256": "deadbeef" }
        });
        inject_payload_into_content_item(&mut item, b"hello");
        assert_eq!(item["data"], json!(b64("hello")));
        // _meta 仅含 a2c_* → 清空后整键移除（透明）
        assert!(item.get("_meta").is_none(), "全 a2c_* 的 _meta 应被移除");
    }

    #[test]
    fn test_inject_embedded_resource_writes_resource_blob() {
        let mut item = json!({
            "type": "resource",
            "resource": { "mimeType": "application/pdf", "blob": "" },
            "_meta": { "a2c_blob_handle": "ts:pdf", "other": "keep" }
        });
        inject_payload_into_content_item(&mut item, b"PDFDATA");
        assert_eq!(item["resource"]["blob"], json!(b64("PDFDATA")));
        // 顶层不应误写 data
        assert!(item.get("data").is_none());
        // 非 a2c_ 键保留，_meta 不整键移除
        assert_eq!(item["_meta"]["other"], json!("keep"));
        assert!(item["_meta"].get("a2c_blob_handle").is_none());
    }

    #[test]
    fn test_inject_unknown_shape_falls_back_to_data() {
        // 无 data/type/resource.blob 的未知形态 → 兜底写顶层 data（不丢字节）
        let mut item = json!({ "_meta": { "a2c_blob_handle": "ts:x" } });
        inject_payload_into_content_item(&mut item, b"bytes");
        assert_eq!(item["data"], json!(b64("bytes")));
    }

    #[test]
    fn test_inject_non_object_is_noop() {
        let mut item = json!("a plain string");
        inject_payload_into_content_item(&mut item, b"bytes");
        assert_eq!(item, json!("a plain string"));
    }
}
