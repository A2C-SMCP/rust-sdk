//! SKILL 资源 MIME 推断与「文本 MIME」判据（单一权威）/ Deterministic MIME inference for SKILL resources.
//!
//! 协议依据 / Protocol: a2c-smcp-protocol `skill.md` §6.4「mime_type 确定性与「文本 MIME」判据」
//! （refs A2C-SMCP/a2c-smcp-protocol#6，覆盖 v0.2.2）。对标 Python `a2c_smcp/utils/mime.py`。
//!
//! 存在意义 / Why this module：
//!   §6.4(1) 要求 SKILL 子资源 `mime_type` 推断 **MUST** 确定、**MUST NOT** 依赖随宿主环境而变的
//!   OS MIME 注册表 / 系统库（Python `mimetypes.guess_type`、Rust `mime_guess` 回退宿主库的部分）。
//!   本模块以实现内置、与宿主无关的「扩展名 → MIME」映射（§6.4(3)）+ 引用既有标准的「文本 MIME」
//!   判据（§6.4(2)，RFC 2046 / RFC 6839 / RFC 9512 + WHATWG mimesniff），为 Computer（铸造期
//!   `is_text` 路由 + wire `mime_type`）与 Agent（`get_skill` 自动 drain 门）提供**唯一**权威，杜绝
//!   跨 OS / 跨 SDK / 跨决策点的判定漂移。
//!
//!   Single source of truth for textuality so the Computer mint path and the Agent drain gate cannot
//!   disagree, and so the same resource yields the same `mime_type` on any OS / any SDK.

/// 未登记 / 无扩展名的回退 MIME / Fallback for unknown or missing extension。
pub const FALLBACK_MIME: &str = "application/octet-stream";

/// §6.4(3) 最小确定性扩展名映射基线（规范性 SHOULD 表，MUST 至少内置）+ 常见二进制扩展名（准确性增强：
/// 避免 wire `mime_type` 退化为 octet-stream；§6.4(1) 的确定性约束对二进制同样适用）。键为小写扩展名
/// （不含前导点）。未登记扩展名一律回退 [`FALLBACK_MIME`]（仍确定）。逐项对标 Python `EXT_TO_MIME`。
///
/// Baseline text exts (spec §6.4(3), MUST) + common binary exts (keep wire mime accurate, no host dep).
const EXT_TO_MIME: &[(&str, &str)] = &[
    // —— 文本基线（§6.4(3)，MUST）/ Text baseline (spec §6.4(3)) ——
    ("md", "text/markdown"),       // RFC 7763
    ("markdown", "text/markdown"), // RFC 7763
    ("txt", "text/plain"),         // RFC 2046
    ("json", "application/json"),  // RFC 8259
    ("xml", "application/xml"),    // RFC 7303
    ("yaml", "application/yaml"),  // RFC 9512
    ("yml", "application/yaml"),   // RFC 9512
    ("toml", "application/toml"),  // IANA, 2024-10-21
    ("rst", "text/x-rst"),         // freedesktop 事实标准（无正式 IANA 注册）
    // —— 常见二进制（准确性增强；未列出者回退 octet-stream，仍确定）/ Common binary (accuracy) ——
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("bmp", "image/bmp"),
    ("ico", "image/vnd.microsoft.icon"),
    ("svg", "image/svg+xml"), // 注：+xml 后缀 → is_text_mime 判为文本（SVG 本即文本 XML）
    ("pdf", "application/pdf"),
    ("zip", "application/zip"),
    ("gz", "application/gzip"),
    ("tar", "application/x-tar"),
    ("wasm", "application/wasm"),
    ("woff", "font/woff"),
    ("woff2", "font/woff2"),
    ("ttf", "font/ttf"),
    ("otf", "font/otf"),
    ("mp3", "audio/mpeg"),
    ("wav", "audio/wav"),
    ("mp4", "video/mp4"),
    ("webm", "video/webm"),
];

/// §6.4(2) 已 IANA 注册的文本类 `application/*` essence 白名单（非 `text/*` 但属文本）。
/// `application/x-yaml` 为规范名 `application/yaml`（RFC 9512）出现前的事实别名，保留兼容。
const TEXT_MIME_ESSENCES: &[&str] = &[
    "application/json",
    "application/xml",
    "application/yaml",
    "application/x-yaml",
    "application/toml",
    "application/javascript",
];

/// §6.4(2) 结构化语法后缀（可解析为对应文本格式 → 文本）/ Structured syntax suffixes implying text。
/// RFC 6839（`+json` / `+xml`）、RFC 9512（`+yaml`）。
const TEXT_STRUCTURED_SUFFIXES: &[&str] = &["+json", "+xml", "+yaml"];

/// 取文件名末段扩展名的小写形式（不含点）/ Lowercase trailing extension (no dot)。
///
/// 仅看最后一个 `.` 之后的片段，且 `.` 必须在末段路径分隔符之后（与 Python `PurePosixPath.suffix`
/// 一致：`".bashrc"` / `"noext"` 无扩展名）。跨平台按 `/` 与 `\` 同时切分末段。
fn ext_of(filename: &str) -> Option<String> {
    let last_seg = filename.rsplit(['/', '\\']).next().unwrap_or(filename);
    let dot = last_seg.rfind('.')?;
    // 前导点（隐藏文件如 ".bashrc"）不算扩展名；末尾点（"name."）扩展名为空，回退。
    if dot == 0 || dot + 1 >= last_seg.len() {
        return None;
    }
    Some(last_seg[dot + 1..].to_ascii_lowercase())
}

/// 按文件名扩展名推断 MIME（§6.4(1) 确定性：**绝不**调用宿主系统库）/ Deterministic MIME by extension。
///
/// 纯查表：命中 `EXT_TO_MIME` → 规范 MIME；未命中（含无扩展名）→ [`FALLBACK_MIME`]。结果只取决于扩展名
/// （大小写不敏感），跨 OS / 跨 SDK 一致——这正是 python-sdk #105 的根因修复点（`mimetypes.guess_type` /
/// `mime_guess` 内置表对 `.yaml` / `.toml` / `.rst` 等返回与协议基线不一致的值）。对标 Python `guess_mime`。
///
/// # Arguments
/// * `filename` - 文件名或包内相对路径（仅取末段扩展名）。
pub fn guess_mime(filename: &str) -> &'static str {
    match ext_of(filename) {
        Some(ext) => EXT_TO_MIME
            .iter()
            .find(|(k, _)| *k == ext)
            .map(|(_, v)| *v)
            .unwrap_or(FALLBACK_MIME),
        None => FALLBACK_MIME,
    }
}

/// 是否「文本 MIME」（§6.4(2) 判据，三条任一成立即文本）/ Whether a MIME is textual per §6.4(2)。
///
/// - top-level type 为 `text`（RFC 2046 §4.1）；或
/// - structured syntax suffix ∈ {`+json`, `+xml`, `+yaml`}（RFC 6839 / RFC 9512）；或
/// - essence ∈ `TEXT_MIME_ESSENCES`（`application/json|xml|yaml|toml|javascript`）。
///
/// 与 WHATWG MIME Sniffing 对「JSON / XML MIME type」的定义同构（后缀 ∨ essence 白名单）。不满足任一
/// → 二进制（铸 `blob_handle`）。供 Computer inline 路由与 Agent `get_skill` 自动 drain 门**共用**，
/// 保证「同一 MIME 在两端判定一致」。大小写不敏感；忽略 `;` 后参数（如 `text/markdown; charset=utf-8`）。
/// 对标 Python `is_text_mime`。
pub fn is_text_mime(mime: &str) -> bool {
    let essence = mime
        .split(';')
        .next()
        .unwrap_or(mime)
        .trim()
        .to_ascii_lowercase();
    if essence.starts_with("text/") {
        return true;
    }
    if TEXT_STRUCTURED_SUFFIXES
        .iter()
        .any(|suf| essence.ends_with(suf))
    {
        return true;
    }
    TEXT_MIME_ESSENCES.contains(&essence.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guess_mime_text_baseline_matches_spec_64_3() {
        // §6.4(3) MUST 基线：与 Python guess_mime / 协议表逐项一致（杜绝 mime_guess 的 text/x-yaml 漂移）。
        assert_eq!(guess_mime("a.md"), "text/markdown");
        assert_eq!(guess_mime("a.markdown"), "text/markdown");
        assert_eq!(guess_mime("a.txt"), "text/plain");
        assert_eq!(guess_mime("a.json"), "application/json");
        assert_eq!(guess_mime("a.xml"), "application/xml");
        assert_eq!(guess_mime("a.yaml"), "application/yaml");
        assert_eq!(guess_mime("a.yml"), "application/yaml");
        assert_eq!(guess_mime("a.toml"), "application/toml");
        assert_eq!(guess_mime("a.rst"), "text/x-rst");
    }

    #[test]
    fn guess_mime_is_case_insensitive_and_path_aware() {
        assert_eq!(guess_mime("README.MD"), "text/markdown");
        assert_eq!(guess_mime("pkg/sub/config.YAML"), "application/yaml");
        assert_eq!(guess_mime("a\\b\\data.JSON"), "application/json");
    }

    #[test]
    fn guess_mime_unknown_and_extless_fall_back() {
        assert_eq!(guess_mime("noext"), FALLBACK_MIME);
        assert_eq!(guess_mime(".bashrc"), FALLBACK_MIME); // 隐藏文件无扩展名
        assert_eq!(guess_mime("trailingdot."), FALLBACK_MIME);
        assert_eq!(guess_mime("a.unknownext"), FALLBACK_MIME);
    }

    #[test]
    fn guess_mime_common_binary() {
        assert_eq!(guess_mime("a.png"), "image/png");
        assert_eq!(guess_mime("a.svg"), "image/svg+xml");
        assert_eq!(guess_mime("a.pdf"), "application/pdf");
    }

    #[test]
    fn is_text_mime_branch1_text_star() {
        assert!(is_text_mime("text/plain"));
        assert!(is_text_mime("text/markdown"));
        assert!(is_text_mime("text/x-rst"));
        // 忽略 ; 后参数（charset）。
        assert!(is_text_mime("text/plain; charset=utf-8"));
        assert!(is_text_mime("TEXT/PLAIN")); // 大小写不敏感
    }

    #[test]
    fn is_text_mime_branch2_structured_suffix() {
        // §6.4(2) 第 2 分支：+json / +xml / +yaml（旧实现漏掉，正是缺陷 B）。
        assert!(is_text_mime("image/svg+xml"));
        assert!(is_text_mime("application/vnd.api+json"));
        assert!(is_text_mime("application/ld+json"));
        assert!(is_text_mime("application/foo+yaml"));
    }

    #[test]
    fn is_text_mime_branch3_essence_whitelist() {
        // §6.4(2) 第 3 分支：application/* 文本 essence（旧 Agent 门漏掉，正是缺陷 C）。
        assert!(is_text_mime("application/json"));
        assert!(is_text_mime("application/json; charset=utf-8"));
        assert!(is_text_mime("application/xml"));
        assert!(is_text_mime("application/yaml"));
        assert!(is_text_mime("application/x-yaml"));
        assert!(is_text_mime("application/toml"));
        assert!(is_text_mime("application/javascript"));
    }

    #[test]
    fn is_text_mime_binary_is_not_text() {
        assert!(!is_text_mime("application/octet-stream"));
        assert!(!is_text_mime("image/png"));
        assert!(!is_text_mime("application/pdf"));
        assert!(!is_text_mime("application/zip"));
        assert!(!is_text_mime(""));
    }
}
