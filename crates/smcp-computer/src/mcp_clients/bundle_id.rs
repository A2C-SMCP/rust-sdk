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

use smcp::utils::bundle_id::is_bundle_id_char;
/// BundleID 的**类型 / 判据 / 错误分类**——**单一权威**在协议 crate（[`smcp::utils::bundle_id`]），本模块只
/// re-export（#130）。**本模块的职责仅剩「缺省生成算法」**。
///
/// 不在此另写等价谓词或另一个 newtype：SKILL 的 mcp `<server>` 段**就是** `bundle_id`（skill.md §1.3），其
/// 判据由 `smcp::skill_name` 消费；两处若各写一份，一旦漂移即令合法 `bundle_id` 的 SKILL 对 Agent 隐身
/// （rust-sdk#127 / python-sdk#142 要消灭的失效模式）。
pub use smcp::utils::bundle_id::{validate_bundle_id, BundleId, BundleIdError};

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
///
/// # 产出恒合法（不变量）
///
/// 两条分支的产出都**可证**满足 [`smcp::utils::bundle_id::is_valid_bundle_id`]：
/// - `normalize_name` 已把非字符集码点映射为 `_`、折叠连续 `_`（故无 `__`）、裁首尾——非空分支即合法；
/// - fallback `bundle_` + 16 位小写 hex：字符集内、单个 `_`、非空——亦合法。
///
/// 故此处 `expect` **不可达**；由 `derive_bundle_id_always_yields_valid_id_130` 覆盖（含 CJK / 全符号 /
/// 空名等会走 fallback 的输入）。
#[must_use]
pub fn derive_bundle_id(config: &MCPServerConfig) -> BundleId {
    let normalized = normalize_name(config.name());
    let raw = if normalized.is_empty() {
        // fallback：确定性摘要（禁随机 UUID / 内建 hash / base32·64）。sha256_hex 为 64 位小写 hex，取前 16 = 前 8 字节。
        let digest = sha256_hex(&connection_identity_bytes(config));
        format!("bundle_{}", &digest[..16])
    } else {
        normalized
    };
    BundleId::try_from(raw).expect("derive_bundle_id 产出恒合法（见函数文档的不变量论证）")
}

/// 解析出**恒有值**的 `bundle_id`：显式配置值优先，否则 [`derive_bundle_id`] 缺省生成。
///
/// #130：显式值的合法性**由构造保证**（[`BundleId`] 存在即合法——`mcp.json` 里的畸形 `bundleId` 在 serde
/// 反序列化的**字段级**即判废，由 `settings::mcp_config::validate_server` 逐-server 降级），故本函数与注册
/// 边界都**无需**再校验一遍。
#[must_use]
pub fn resolve_bundle_id(config: &MCPServerConfig) -> BundleId {
    match config.bundle_id() {
        Some(explicit) => explicit.clone(),
        None => derive_bundle_id(config),
    }
}

/// `exposed_tool_name = bundle_id + "__" + (alias ?? original_tool_name)`。
///
/// `alias` 仅替换**工具名部分**，仍带 `{bundle_id}__` 前缀。因 `bundle_id` 禁 `__`，第一个 `__` 之前恒为
/// `bundle_id` → 对 `(bundle_id, tool)` **单射**；路由 MUST 查 [`ExposedToolMapping`] 整键、**不** split 反解。
///
/// #130：前缀形参收 [`BundleId`]（而非 `&str`）——「第一个 `__` 之前恒为 `bundle_id`」这条单射性前提，
/// 由**类型**保证（`BundleId` 存在即合法 ⇒ 恒不含 `__`），而非靠调用方自觉传对东西。
///
/// [`ExposedToolMapping`]: super::manager::ExposedToolRoute
#[must_use]
pub fn exposed_tool_name(
    bundle_id: &BundleId,
    alias: Option<&str>,
    original_tool_name: &str,
) -> String {
    format!("{bundle_id}__{}", alias.unwrap_or(original_tool_name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_clients::model::{
        HttpServerConfig, HttpServerParameters, MCPServerConfig, SseServerConfig,
        SseServerParameters, StdioServerConfig, StdioServerParameters,
    };

    /// 测试夹具：构造合法 [`BundleId`]（非法字面量在此 panic —— 夹具写错须立刻暴露）。
    fn bid(s: &str) -> BundleId {
        BundleId::try_from(s).expect("测试夹具的 bundle_id 字面量必须合法")
    }

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

    /// #127 隔离审查 🟡：`validate_bundle_id`（结构化）与权威 `is_valid_bundle_id`（bool）**判决恒等**。
    ///
    /// 前者已委托后者判决、只做分类，故本测试是**防回退网**：若日后有人把规则集重新在此手写一份，
    /// 语料一旦分歧即红。分歧的真实后果是注册边界放行、SKILL 层判废 → 合法 `bundle_id` 的 SKILL 隐身。
    #[test]
    fn validate_bundle_id_verdict_matches_authority_127() {
        let long = "a".repeat(256);
        let corpus = [
            // 合法
            "tfrobot-tools",
            "my_api",
            "MyServer",
            "bundle_a1b2c3d4e5f60718",
            "a",
            "_leading-ok",
            "trailing-ok_",
            &long, // 无长度上限
            // 非法
            "",
            "my.api",
            "a__b",
            "__lead",
            "trail__",
            "服务器",
            "a b",
        ];
        for v in corpus {
            assert_eq!(
                validate_bundle_id(v).is_ok(),
                smcp::utils::bundle_id::is_valid_bundle_id(v),
                "判决须与权威一致: {v:?}"
            );
        }
    }

    // ---- derive_bundle_id（缺省生成）----

    #[test]
    fn derive_uses_normalized_name_when_nonempty() {
        let c = stdio("My Server", "npx", &["everything"], &[]);
        assert_eq!(derive_bundle_id(&c).as_str(), "My_Server");
    }

    #[test]
    fn derive_falls_back_to_digest_for_empty_name() {
        let c = stdio("你好", "npx", &["-y", "everything"], &[]);
        let id = derive_bundle_id(&c);
        let id = id.as_str();
        assert!(id.starts_with("bundle_"), "got {id}");
        assert_eq!(id.len(), "bundle_".len() + 16); // bundle_ + 16 hex
        assert!(id["bundle_".len()..]
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
    }

    /// #130 不变量：[`derive_bundle_id`] 的产出**恒合法**（故其内部 `expect` 不可达）。
    ///
    /// 覆盖两条分支：normalize 非空（ASCII / 混合符号 / 连续下划线源）与 fallback（CJK / 全符号 / 空名）。
    #[test]
    fn derive_bundle_id_always_yields_valid_id_130() {
        static LONG_NAME: &str = concat!(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        );
        for name in [
            "My Server",
            "a__b",         // 源含保留分隔符 → 折叠后须合法
            "___",          // 全下划线 → 裁空 → 走 fallback
            "你好",         // CJK → 走 fallback
            "!!!",          // 全符号 → 走 fallback
            "",             // 空名 → 走 fallback
            "-lead-trail-", // 首尾连字符 → 裁剪
            "a.b.c",        // 含点 → 映射为 _
            "UPPER_lower-9",
            // 长名：不变量与权威**耦合**——若协议未来给 `is_valid_bundle_id` 加长度上限，此条会让
            // `derive_bundle_id` 的 `expect` 从"不可达"变成线上 panic，本例把该耦合显式钉住。
            LONG_NAME,
        ] {
            let c = stdio(name, "npx", &["x"], &[]);
            // 不 panic 即证 expect 不可达；再正面断言权威判据。
            let id = derive_bundle_id(&c);
            assert!(
                smcp::utils::bundle_id::is_valid_bundle_id(id.as_str()),
                "derive_bundle_id({name:?}) 产出 {id:?} 不合法——不变量被破坏"
            );
        }
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
        let b = bid("b");
        assert_eq!(exposed_tool_name(&b, None, "foo"), "b__foo");
        assert_eq!(exposed_tool_name(&b, Some("bar"), "foo"), "b__bar");
        // 原始工具名内含 __ 无害：单射，第一个 __ 前恒为 bundle_id。
        assert_eq!(exposed_tool_name(&b, None, "foo__bar"), "b__foo__bar");
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
        c.bundle_id = Some(bid("custom_id"));
        let cfg = MCPServerConfig::Stdio(c);
        assert_eq!(resolve_bundle_id(&cfg).as_str(), "custom_id");
    }
}
