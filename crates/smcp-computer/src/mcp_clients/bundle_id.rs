/*
 * 文件名: bundle_id
 * 作者: JQQ
 * 创建日期: 2026/07/11
 * 版权: 2023 JQQ. All rights reserved.
 * 依赖: smcp::utils::hash
 */
//! # BundleID 模型（BundleID model）
//!
//! **source of truth**：`a2c-smcp-protocol@95b8553`
//! `docs/specification/data-structures.md` §「MCP Tool 命名与路由（BundleID 模型）」。
//!
//! 本模块是 `bundle_id` **缺省生成算法**的**单一权威**（对标 python-sdk `a2c_smcp/utils/bundle_id.py`）。
//! 缺省生成**逐字节确定性**，跨 SDK（Python / Rust）**MUST** 产出同一结果——由协议仓一致性测试向量强制
//! （rust 首版提供参考实现 + 向量，python 对拍锁定）。任何改动都是**跨 SDK 契约变更**，须同步协议仓向量。
//!
//! ## 缺省生成（[`derive_bundle_id`]）
//! 1. **规范化 name**（[`normalize_name`]，按 Unicode 码点迭代）：非 `[A-Za-z0-9_-]` → `_`（显式 ASCII 类，
//!    禁 `\w`）；折叠连续 `_`（含原文 `__`）为单个、**不折叠** `-`；裁首尾 `[_-]`；**不做**大小写折叠。
//! 2. 规范化结果**非空** → 即 `bundle_id`。
//! 3. 结果**为空**（name 全为符号 / CJK / 空串）→ `bundle_` + `sha256([connection-identity TLV])[:8]` 小写 hex。
//!
//! ## connection-identity TLV 字节帧（[`connection_identity_bytes`]，rust 首版参考实现）
//!
//! **input_state = raw**（协议 §connection-identity，a2c-smcp-protocol#17）：摘要输入取 **raw / 未注入**配置——
//! `${input:*}` / `${env:*}` / secret 占位**按字面**参与，**MUST NOT** 先渲染。本模块的函数 render-agnostic（只序列化
//! 所给 config 的 env/headers 当前值），**raw 契约由调用方保证**：`Computer::render_server_config` 从 **raw config**
//! 派生 bundle_id 并 stamp 到渲染后配置（见其实现），使无名 server 的引用 input/secret 轮换**不**漂移 bundle_id。
//!
//! 为避免 JSON 跨语言序列化漂移，缺省生成 fallback 的摘要输入用**长度前缀（TLV）字节帧**、**非 JSON**：
//! - **字符串字段**：`u32-BE 字节长度 ‖ UTF-8 字节`。
//! - **字符串列表**：`u32-BE 元素数 ‖ 元素(字符串字段)*`（保序）。
//! - **字符串映射**：按 key 升序（Rust `str` 的字节序 == UTF-8 码点序）`u32-BE 条目数 ‖ (key字段 ‖ value字段)*`。
//! - **帧**：`stdio` = `TYPE("stdio") ‖ command ‖ args-list ‖ env-map`；
//!   `streamable`/`sse` = `TYPE("streamable"|"sse") ‖ url ‖ headers-map`。
//! - `type` 判别符用协议 §9.1 规范小写（`stdio` / `streamable` / `sse`；`Http` 变体记 `streamable`）。
//! - 长度前缀自定界，无需分隔符；空 `args`/`env`/`headers` → 计数 0。
//! - **排除**：`disabled` / `tool_meta` / `forbidden_tools` / `vrl` / `env_file` / `cwd` / `encoding` /
//!   `timeout`（非连接身份，或跨语言类型不一致）——仅纳入连接建立字段。

use super::model::MCPServerConfig;
use smcp::utils::hash::sha256_hex;
use std::collections::HashMap;

/// `bundle_id` 校验错误（仅用于**显式**配置值；缺省生成结果恒合法）/ explicit bundle_id validation error。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BundleIdError {
    /// 显式 `bundle_id` 为空 / explicit bundle_id is empty。
    #[error("bundle_id must not be empty")]
    Empty,
    /// 含保留分隔符 `__`（BundleID 与工具名的分隔符，禁出现于 BundleID 内）/ contains reserved `__`。
    #[error("bundle_id '{0}' must not contain the reserved separator '__'")]
    ReservedSeparator(String),
    /// 含字符集 `[A-Za-z0-9_-]` 之外的字符（含 `.`）/ contains a char outside `[A-Za-z0-9_-]`。
    #[error(
        "bundle_id '{value}' contains illegal character '{ch}' (allowed charset: [A-Za-z0-9_-])"
    )]
    IllegalChar {
        /// 违规的完整值 / the offending value。
        value: String,
        /// 首个违规字符 / first offending character。
        ch: char,
    },
}

/// 判定字符是否属 BundleID 字符集 `[A-Za-z0-9_-]`（**显式 ASCII 类**，非 Unicode-aware `\w`）。
#[inline]
fn is_bundle_id_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// Step 1：规范化 `name` → 候选 `bundle_id`（可能为空，空时走 [`derive_bundle_id`] 的 fallback）。
///
/// 按 **Unicode 码点**迭代（Rust `.chars()`，非 UTF-8 字节 / 非 grapheme）：非 `[A-Za-z0-9_-]` → `_`；
/// 折叠连续 `_`（**含**原文 `__`）为单个、**不折叠** `-`；裁首尾 `[_-]`；**不做**大小写折叠。
pub fn normalize_name(name: &str) -> String {
    // 1) 非字符集码点 → '_'（按码点迭代；任何非 ASCII 一律命中）。
    let mapped: String = name
        .chars()
        .map(|c| if is_bundle_id_char(c) { c } else { '_' })
        .collect();

    // 2) 折叠连续 '_' 为单个（不折 '-'）。
    let mut folded = String::with_capacity(mapped.len());
    let mut prev_underscore = false;
    for c in mapped.chars() {
        if c == '_' {
            if !prev_underscore {
                folded.push('_');
            }
            prev_underscore = true;
        } else {
            folded.push(c);
            prev_underscore = false;
        }
    }

    // 3) 裁首尾 [_-]。
    folded.trim_matches(|c| c == '_' || c == '-').to_string()
}

/// 追加一个字符串字段：`u32-BE 字节长度 ‖ UTF-8 字节`。
fn push_field(buf: &mut Vec<u8>, s: &str) {
    // 字节长度上限 u32：MCP 连接字段远不及 4GiB，`as u32` 无实际截断风险。
    buf.extend_from_slice(&(s.len() as u32).to_be_bytes());
    buf.extend_from_slice(s.as_bytes());
}

/// 追加一个字符串列表（保序）：`u32-BE 元素数 ‖ 元素(字符串字段)*`。
fn push_list(buf: &mut Vec<u8>, items: &[String]) {
    buf.extend_from_slice(&(items.len() as u32).to_be_bytes());
    for it in items {
        push_field(buf, it);
    }
}

/// 追加一个字符串映射（**按 key 升序**）：`u32-BE 条目数 ‖ (key字段 ‖ value字段)*`。
///
/// Rust `str` 的 `Ord` 是 UTF-8 字节字典序；对合法 UTF-8 而言与**码点序**等价（UTF-8 保序），
/// 与协议「按 key 码点序排序」及 Python `sorted(dict)` 逐字节一致。
fn push_map(buf: &mut Vec<u8>, map: &HashMap<String, String>) {
    let mut keys: Vec<&String> = map.keys().collect();
    keys.sort();
    buf.extend_from_slice(&(keys.len() as u32).to_be_bytes());
    for k in keys {
        push_field(buf, k);
        push_field(buf, &map[k]);
    }
}

/// connection-identity TLV 字节帧（[`derive_bundle_id`] fallback 的摘要输入）。见模块文档「TLV 字节帧」。
pub fn connection_identity_bytes(config: &MCPServerConfig) -> Vec<u8> {
    let mut buf = Vec::new();
    match config {
        MCPServerConfig::Stdio(c) => {
            push_field(&mut buf, "stdio");
            push_field(&mut buf, &c.server_parameters.command);
            push_list(&mut buf, &c.server_parameters.args);
            push_map(&mut buf, &c.server_parameters.env);
        }
        MCPServerConfig::Http(c) => {
            // 协议 §9.1 规范判别符：Http 变体记 `streamable`（非历史别名 `http`）。
            push_field(&mut buf, "streamable");
            push_field(&mut buf, &c.server_parameters.url);
            push_map(&mut buf, &c.server_parameters.headers);
        }
        MCPServerConfig::Sse(c) => {
            push_field(&mut buf, "sse");
            push_field(&mut buf, &c.server_parameters.url);
            push_map(&mut buf, &c.server_parameters.headers);
        }
    }
    buf
}

/// **缺省生成**：从 `name` 派生 `bundle_id`（忽略配置里可能存在的显式 `bundle_id`）。
///
/// `normalize_name(name)` 非空 → 即为结果；为空 → `bundle_` + `sha256(TLV)[:8]` 小写 hex（16 hex 字符）。
pub fn derive_bundle_id(config: &MCPServerConfig) -> String {
    let normalized = normalize_name(config.name());
    if !normalized.is_empty() {
        return normalized;
    }
    // fallback：确定性摘要（禁随机 UUID / 内建 hash / base32·64）。sha256_hex 为 64 位小写 hex，取前 16 = 前 8 字节。
    let digest = sha256_hex(&connection_identity_bytes(config));
    format!("bundle_{}", &digest[..16])
}

/// 解析出**恒有值**的 `bundle_id`：显式配置值优先，否则 [`derive_bundle_id`] 缺省生成。
///
/// 显式值的合法性由注册边界经 [`validate_bundle_id`] 校验（非法值在此之前即报错）；本函数只做「取显式或派生」。
pub fn resolve_bundle_id(config: &MCPServerConfig) -> String {
    match config.bundle_id() {
        Some(explicit) => explicit.to_string(),
        None => derive_bundle_id(config),
    }
}

/// 校验**显式** `bundle_id`：非空、无 `__`、字符集 `[A-Za-z0-9_-]`（含 `.` 判为 [`BundleIdError::IllegalChar`]）。
pub fn validate_bundle_id(value: &str) -> Result<(), BundleIdError> {
    if value.is_empty() {
        return Err(BundleIdError::Empty);
    }
    for c in value.chars() {
        if !is_bundle_id_char(c) {
            return Err(BundleIdError::IllegalChar {
                value: value.to_string(),
                ch: c,
            });
        }
    }
    if value.contains("__") {
        return Err(BundleIdError::ReservedSeparator(value.to_string()));
    }
    Ok(())
}

/// `exposed_tool_name = bundle_id + "__" + (alias ?? original_tool_name)`。
///
/// `alias` 仅替换**工具名部分**，仍带 `{bundle_id}__` 前缀。因 `bundle_id` 禁 `__`，第一个 `__` 之前恒为
/// `bundle_id` → 对 `(bundle_id, tool)` **单射**；路由 MUST 查 [`ExposedToolMapping`] 整键、**不** split 反解。
///
/// [`ExposedToolMapping`]: super::manager::ExposedToolRoute
pub fn exposed_tool_name(bundle_id: &str, alias: Option<&str>, original_tool_name: &str) -> String {
    format!("{bundle_id}__{}", alias.unwrap_or(original_tool_name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_clients::model::{
        HttpServerConfig, HttpServerParameters, MCPServerConfig, SseServerConfig,
        SseServerParameters, StdioServerConfig, StdioServerParameters,
    };

    fn stdio(name: &str, command: &str, args: &[&str], env: &[(&str, &str)]) -> MCPServerConfig {
        let mut c = StdioServerConfig::new(
            name,
            StdioServerParameters {
                command: command.to_string(),
                args: args.iter().map(|s| s.to_string()).collect(),
                env: env
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
                cwd: None,
            },
        );
        c.disabled = false;
        MCPServerConfig::Stdio(c)
    }

    fn http(name: &str, url: &str, headers: &[(&str, &str)]) -> MCPServerConfig {
        MCPServerConfig::Http(HttpServerConfig::new(
            name,
            HttpServerParameters {
                url: url.to_string(),
                headers: headers
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            },
        ))
    }

    fn sse(name: &str, url: &str) -> MCPServerConfig {
        MCPServerConfig::Sse(SseServerConfig::new(
            name,
            SseServerParameters {
                url: url.to_string(),
                headers: HashMap::new(),
            },
        ))
    }

    // ---- normalize_name（规范化，Step 1）----

    #[test]
    fn normalize_collapses_and_trims() {
        // 空格/符号 → '_'，折叠连续 '_'（含原文 __），不折 '-'，裁首尾。
        assert_eq!(normalize_name("my server"), "my_server");
        assert_eq!(normalize_name("my-server"), "my-server");
        assert_eq!(normalize_name("my_server"), "my_server");
        assert_eq!(normalize_name("my__server"), "my_server"); // 原文 __ 折叠
        assert_eq!(normalize_name("my..server"), "my_server"); // .. → __ → _
        assert_eq!(normalize_name("__lead_trail__"), "lead_trail"); // 裁首尾
        assert_eq!(normalize_name("--dash--"), "dash"); // 裁首尾 '-'
        assert_eq!(normalize_name("a--b"), "a--b"); // 不折 '-'
    }

    #[test]
    fn normalize_no_case_fold() {
        assert_eq!(normalize_name("MyServer"), "MyServer");
        assert_ne!(normalize_name("MyServer"), normalize_name("myserver"));
    }

    #[test]
    fn normalize_non_injective_examples() {
        // 规范化非单射：三种写法归一到同一结果（缺省生成后可能撞 → no-double-open 诊断）。
        assert_eq!(
            normalize_name("my server"),
            normalize_name("my-server").replace('-', "_")
        );
        assert_eq!(normalize_name("everything"), "everything");
    }

    #[test]
    fn normalize_cjk_and_symbols_empty() {
        // 全 CJK / 全符号 / 空 → 规范化为空（触发 fallback）。
        assert_eq!(normalize_name("你好世界"), "");
        assert_eq!(normalize_name("***"), "");
        assert_eq!(normalize_name(""), "");
        assert_eq!(normalize_name("   "), "");
    }

    // ---- derive_bundle_id（缺省生成）----

    #[test]
    fn derive_uses_normalized_name_when_nonempty() {
        let c = stdio("My Server", "npx", &["everything"], &[]);
        assert_eq!(derive_bundle_id(&c), "My_Server");
    }

    #[test]
    fn derive_falls_back_to_digest_for_empty_name() {
        let c = stdio("你好", "npx", &["-y", "everything"], &[]);
        let id = derive_bundle_id(&c);
        assert!(id.starts_with("bundle_"), "got {id}");
        assert_eq!(id.len(), "bundle_".len() + 16); // bundle_ + 16 hex
        assert!(id["bundle_".len()..]
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
    }

    #[test]
    fn derive_fallback_is_deterministic_and_connection_sensitive() {
        // 同连接身份 → 同 id；不同 command → 不同 id（fallback 靠 connection-identity 区分无名 server）。
        let a = stdio("符号!!!", "npx", &["everything"], &[]);
        let a2 = stdio("符号!!!", "npx", &["everything"], &[]);
        let b = stdio("其他", "uvx", &["everything"], &[]);
        assert_eq!(derive_bundle_id(&a), derive_bundle_id(&a2));
        assert_ne!(derive_bundle_id(&a), derive_bundle_id(&b));
    }

    #[test]
    fn derive_fallback_http_vs_sse_differ_by_type() {
        // 同 url、不同传输类型 → type 判别符不同 → 不同 id。
        let h = http("汉字", "https://x.example/mcp", &[]);
        let s = sse("汉字", "https://x.example/mcp");
        assert_ne!(derive_bundle_id(&h), derive_bundle_id(&s));
    }

    // ---- connection_identity_bytes（TLV 帧，精确字节锁定）----

    #[test]
    fn tlv_frame_exact_bytes_stdio() {
        // 锁定 TLV 精确字节：type="stdio"(5) ‖ command="cmd"(3) ‖ args=["a"] ‖ env={}。
        // 供 python 对拍帧本身（非仅最终 bundle_id）。
        let c = stdio("x", "cmd", &["a"], &[]);
        let bytes = connection_identity_bytes(&c);
        let mut want = Vec::new();
        // "stdio"
        want.extend_from_slice(&5u32.to_be_bytes());
        want.extend_from_slice(b"stdio");
        // command "cmd"
        want.extend_from_slice(&3u32.to_be_bytes());
        want.extend_from_slice(b"cmd");
        // args ["a"]
        want.extend_from_slice(&1u32.to_be_bytes()); // 元素数
        want.extend_from_slice(&1u32.to_be_bytes()); // "a" 长度
        want.extend_from_slice(b"a");
        // env {}
        want.extend_from_slice(&0u32.to_be_bytes()); // 条目数
        assert_eq!(bytes, want);
    }

    #[test]
    fn tlv_map_key_sorted() {
        // env/headers 按 key 升序（码点序）编码：{b,a} 与 {a,b} 产出同帧。
        let ab = stdio("x", "c", &[], &[("a", "1"), ("b", "2")]);
        let ba = stdio("x", "c", &[], &[("b", "2"), ("a", "1")]);
        assert_eq!(
            connection_identity_bytes(&ab),
            connection_identity_bytes(&ba)
        );
    }

    // ---- validate_bundle_id（显式值校验）----

    #[test]
    fn validate_accepts_legal() {
        assert!(validate_bundle_id("playwright").is_ok());
        assert!(validate_bundle_id("playwright_isolated").is_ok());
        assert!(validate_bundle_id("tf-mkt_toolkit").is_ok());
        assert!(validate_bundle_id("A9_-z").is_ok());
    }

    #[test]
    fn validate_rejects_dot_double_underscore_and_empty() {
        assert_eq!(validate_bundle_id(""), Err(BundleIdError::Empty));
        assert_eq!(
            validate_bundle_id("a__b"),
            Err(BundleIdError::ReservedSeparator("a__b".to_string()))
        );
        assert!(matches!(
            validate_bundle_id("a.b"),
            Err(BundleIdError::IllegalChar { ch: '.', .. })
        ));
        assert!(matches!(
            validate_bundle_id("汉字"),
            Err(BundleIdError::IllegalChar { .. })
        ));
    }

    // ---- exposed_tool_name（单射性）----

    #[test]
    fn exposed_prefixes_bundle_id() {
        assert_eq!(exposed_tool_name("b", None, "foo"), "b__foo");
        assert_eq!(exposed_tool_name("b", Some("bar"), "foo"), "b__bar");
        // 原始工具名内含 __ 无害：单射，第一个 __ 前恒为 bundle_id。
        assert_eq!(exposed_tool_name("b", None, "foo__bar"), "b__foo__bar");
    }

    #[test]
    fn resolve_prefers_explicit_bundle_id() {
        let mut c = StdioServerConfig::new(
            "whatever",
            StdioServerParameters {
                command: "npx".to_string(),
                args: vec![],
                env: HashMap::new(),
                cwd: None,
            },
        );
        c.bundle_id = Some("custom_id".to_string());
        let cfg = MCPServerConfig::Stdio(c);
        assert_eq!(resolve_bundle_id(&cfg), "custom_id");
    }
}
