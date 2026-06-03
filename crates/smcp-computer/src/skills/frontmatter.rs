/*!
* 文件名: frontmatter.rs
* 作者: JQQ
* 创建日期: 2026/06/03
* 最后修改日期: 2026/06/03
* 版权: 2023 JQQ. All rights reserved.
* 依赖: serde_json, serde_yaml_ng
* 描述: SKILL.md YAML frontmatter 解析与剥离（共享）（对标 Python staging.py parse/strip_skill_frontmatter）
*       Shared SKILL.md YAML frontmatter parse & strip.
*/

//! SKILL.md frontmatter 解析与剥离（共享）/ Shared SKILL.md frontmatter parse & strip。
//!
//! 协议依据 / Protocol: a2c-smcp-protocol `skill.md` §6（A2CSkillRef 字段源）、`blob-transfer.md` §3
//! （消费字节一致性）。对标 Python 参考实现 / Mirrors `a2c_smcp/computer/skills/staging.py` 的
//! `parse_skill_frontmatter` / `strip_skill_frontmatter`。
//!
//! **单一事实源**：由 [`crate::skills::resource`]（SKL-05 / `client:get_skill` 读路径）与
//! [`crate::skills::staging`]（SKL-04 三源落盘）**共用**，杜绝两处实现漂移——`get_skill` 铸造期与
//! `get_blob` 解析期对 SKILL.md「消费字节」基准必须一致。
//!
//! 安全 / Security：frontmatter YAML 可能来自不可信 marketplace，故解析前对 block 字节数设上限
//! （[`MAX_FRONTMATTER_BYTES`]），超限按"无有效 frontmatter"降级（返回空 map）——defense-in-depth，
//! 不改变合法 SKILL.md（frontmatter 通常 < 1 KiB）的语义。

use serde_json::{Map, Value};

/// frontmatter 块解析字节上限（防不可信源超大 YAML DoS）/ frontmatter block parse cap。
pub const MAX_FRONTMATTER_BYTES: usize = 1024 * 1024; // 1 MiB

/// 解析 SKILL.md 头部 YAML frontmatter → JSON 对象映射 / Parse the leading YAML frontmatter to a JSON object map。
///
/// 行级定位首尾 `---` fence（首行须恰为 `---`）。无 frontmatter / 无闭合 fence / YAML 解析失败 /
/// 根非映射 / 超 [`MAX_FRONTMATTER_BYTES`] → 返回空 map（调用方据缺字段判废）。
pub fn parse_skill_frontmatter(text: &str) -> Map<String, Value> {
    let Some(block) = frontmatter_block(text) else {
        return Map::new();
    };
    if block.len() > MAX_FRONTMATTER_BYTES {
        return Map::new();
    }
    // serde_yaml_ng 直接反序列化为 serde_json::Value（与 settings 层统一用 serde_json 动态值）。
    match serde_yaml_ng::from_str::<Value>(&block) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

/// 剥离 SKILL.md 头部 frontmatter，返回正文 body（Agent 最终消费字节）/ Strip frontmatter, return the body。
///
/// 与 [`parse_skill_frontmatter`] 同源行级定位；无 frontmatter / 无闭合 fence → **原样返回**
/// （保留原行尾、不额外 trim），保证 `get_skill` 铸造与 `get_blob` 解析对 SKILL.md 消费字节一致。
pub fn strip_skill_frontmatter(text: &str) -> String {
    body_after_frontmatter(text).unwrap_or(text).to_string()
}

// ---------------------------------------------------------------------------
// 内部：行级 fence 定位 / internal: line-level fence detection
// ---------------------------------------------------------------------------
/// 按 `\n` 切行，返回 (行内容\[不含 `\n`，可能含尾随 `\r`\], 该行 `\n` 之后的字节偏移)。
///
/// 对齐 Python `splitlines` 在 `\n` / `\r\n` 上的常见行为；尾随 `\r` 由比较侧 `trim()` / 构造侧
/// `trim_end_matches('\r')` 消解（与 Python `splitlines()` 去 `\r` 等价）。孤立 `\r`（旧 Mac）不单独切行
/// （SKILL.md 实务为 `\n`/`\r\n`，此简化对消费字节无影响）。
fn split_lines(text: &str) -> Vec<(&str, usize)> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut start = 0usize;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' {
            out.push((&text[start..i], i + 1));
            start = i + 1;
        }
    }
    if start < bytes.len() {
        out.push((&text[start..], bytes.len()));
    }
    out
}

/// 提取首尾 `---` fence 之间的 YAML 块（行去尾随 `\r` 后以 `\n` 连接）；无有效 frontmatter → `None`。
fn frontmatter_block(text: &str) -> Option<String> {
    if !text.starts_with("---") {
        return None;
    }
    let lines = split_lines(text);
    if lines.first()?.0.trim() != "---" {
        return None;
    }
    let close = lines
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, (content, _))| content.trim() == "---")?
        .0;
    let block: Vec<&str> = lines[1..close]
        .iter()
        .map(|(content, _)| content.trim_end_matches('\r'))
        .collect();
    Some(block.join("\n"))
}

/// 返回闭合 `---` fence 行之后的原始 body 子串；无有效 frontmatter → `None`。
fn body_after_frontmatter(text: &str) -> Option<&str> {
    if !text.starts_with("---") {
        return None;
    }
    let lines = split_lines(text);
    if lines.first()?.0.trim() != "---" {
        return None;
    }
    lines
        .iter()
        .skip(1)
        .find(|(content, _)| content.trim() == "---")
        .map(|(_, after)| &text[*after..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_basic_frontmatter() {
        let md = "---\nname: demo\nversion: 1.0.0\ndescription: hello\n---\n# Title\n\nbody\n";
        let fm = parse_skill_frontmatter(md);
        assert_eq!(fm["name"], serde_json::json!("demo"));
        assert_eq!(fm["description"], serde_json::json!("hello"));
        assert_eq!(fm["version"], serde_json::json!("1.0.0"));
    }

    #[test]
    fn test_strip_returns_body_after_fence() {
        let md = "---\nname: demo\nversion: 1.0.0\n---\n# Title\n\nhello body\n";
        assert_eq!(strip_skill_frontmatter(md), "# Title\n\nhello body\n");
    }

    #[test]
    fn test_no_frontmatter_returns_empty_and_unchanged() {
        let md = "# No frontmatter\n\njust body\n";
        assert!(parse_skill_frontmatter(md).is_empty());
        assert_eq!(strip_skill_frontmatter(md), md);
    }

    #[test]
    fn test_unclosed_fence_unchanged() {
        // 无闭合 fence → parse 空、strip 原样（与 Python 一致）。
        let md = "---\nname: demo\nno closing fence\n";
        assert!(parse_skill_frontmatter(md).is_empty());
        assert_eq!(strip_skill_frontmatter(md), md);
    }

    #[test]
    fn test_crlf_line_endings_preserved_in_body() {
        // \r\n：block 去 \r 可解析；body 保留原始 \r\n 字节（消费字节一致性）。
        let md = "---\r\nname: demo\r\ndescription: d\r\n---\r\nbody line\r\n";
        let fm = parse_skill_frontmatter(md);
        assert_eq!(fm["name"], serde_json::json!("demo"));
        assert_eq!(strip_skill_frontmatter(md), "body line\r\n");
    }

    #[test]
    fn test_four_dashes_not_a_fence() {
        // startswith("---") 但首行 strip != "---" → 非 frontmatter。
        let md = "----\nname: x\n----\nbody\n";
        assert!(parse_skill_frontmatter(md).is_empty());
        assert_eq!(strip_skill_frontmatter(md), md);
    }

    #[test]
    fn test_non_mapping_root_yields_empty() {
        // frontmatter 为标量/列表（非映射）→ 空 map。
        let md = "---\n- a\n- b\n---\nbody\n";
        assert!(parse_skill_frontmatter(md).is_empty());
    }
}
