/*!
* 文件名: handle.rs
* 作者: JQQ
* 创建日期: 2026/06/04
* 最后修改日期: 2026/06/04
* 版权: 2023 JQQ. All rights reserved.
* 依赖: rmp-serde（msgpack）, base64, serde, serde_json
* 描述: blob_handle 单层无状态编解码（base64url(msgpack(...))，无 MAC）。
*       Single-layer stateless blob_handle codec.
*/

//! `blob_handle` 单层无状态编解码。
//!
//! 对标 Python 治理层资产 `a2c_smcp/computer/blob/handle.py`（0.2.1）。
//!
//! 形态 / Shape：`blob_handle = base64url( msgpack({ "v": 1, "kind": "skill"|"toolspool", ... }) )`。
//! base64url 去填充 `=`（解码兼容带 / 不带填充）；msgpack 用 **string-keyed map**（`rmp_serde::to_vec_named`），
//! 与 Python `msgpack(use_bin_type=True)` 字节兼容（**跨-SDK 互解**）。
//!
//! 为何无 MAC / Why no MAC：协议 `blob-transfer.md` §1/§5.4 把安全边界定在「Computer 解析时 **MUST** 重跑铸造
//! 通道边界校验，不信任句柄内容」。MAC 是冗余防御（伪造句柄仍要过 Registry + 沙箱），不增真安全却引入
//! per-process secret 管理 / 轮转 / 测试负担——故移除。业界对照：SFTP / OCI / gRPC / OpenAI Realtime 均不签
//! 内部传输 token。
//!
//! 不透明性 / Opacity：Agent **MUST NOT** 解析 / 拼接 / 伪造句柄（协议 §1，由 Agent SDK 担保）。本模块只服务
//! Computer 侧：minter 生成、resolver 解析。
//!
//! 错误归类 / Error taxonomy：本模块只判 codec 自身能识别的 `invalid_handle`（→ 协议 4018
//! `details.reason=invalid_handle`）；`forbidden` / `gone` / `range` 由 resolver 层（BLB-02 #64 / BLB-03 #66）在
//! 重施鉴权 / 回查物化 / 切片时各自抛出（[`BlobHandleError`] 已建模便于上层统一映射 flat `ErrorPayload`）。

use base64::Engine as _;
use serde::Serialize;
use serde_json::Value;

/// 当前句柄版本号 / current handle version。
pub const HANDLE_VERSION: u8 = 1;
/// `kind="skill"` / skill kind tag。
pub const HANDLE_KIND_SKILL: &str = "skill";
/// `kind="toolspool"` / toolspool kind tag。
pub const HANDLE_KIND_TOOLSPOOL: &str = "toolspool";

/// base64url（URL-safe，无填充）引擎 / the URL-safe, no-padding base64 engine。
const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// `kind="skill"` 句柄 payload / skill handle payload。
///
/// `rel_path` 由 Computer 在 `get_blob` 解析时**重跑沙箱**，绝不直接信任（协议 §5.4 + skill.md §9）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillHandlePayload {
    /// `A2CSkillRef.name`（含 source prefix 的合成全局唯一名）/ the synthetic global skill name。
    pub name: String,
    /// SKILL 包根 POSIX 相对路径（解码不校验）/ POSIX rel path under the package root (unchecked here)。
    pub rel_path: String,
}

/// `kind="toolspool"` 句柄 payload / toolspool handle payload。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolspoolHandlePayload {
    /// 内容寻址 id（`sha256` 十六进制）/ content-addressed id (sha256 hex)。
    pub cid: String,
    /// 内容 MIME（回显铸造时的 MIME）/ the minted MIME。
    pub mime: String,
}

/// 解码结果（已识别的句柄种类 + payload）/ a decoded handle (recognized kind + payload)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodedHandle {
    /// `kind="skill"`。
    Skill(SkillHandlePayload),
    /// `kind="toolspool"`。
    Toolspool(ToolspoolHandlePayload),
}

impl DecodedHandle {
    /// 句柄种类标签 / the kind tag。
    pub fn kind(&self) -> &'static str {
        match self {
            DecodedHandle::Skill(_) => HANDLE_KIND_SKILL,
            DecodedHandle::Toolspool(_) => HANDLE_KIND_TOOLSPOOL,
        }
    }
}

/// `blob_handle` 处理错误（子类对齐协议 4018 `details.reason` 开放枚举）/ blob-handle errors。
#[derive(Debug, thiserror::Error)]
pub enum BlobHandleError {
    /// 格式非法 / 非本 Computer 铸造 / 无法识别 → `4018 invalid_handle`。
    #[error("invalid blob handle: {0}")]
    Invalid(String),
    /// 重施铸造通道鉴权失败 → `4018 forbidden`（由 resolver 抛）/ authorization revoked。
    #[error("blob handle forbidden: {0}")]
    Forbidden(String),
    /// 源已不可达 → `4018 gone`（由 resolver 抛）/ source gone。
    #[error("blob handle gone: {0}")]
    Gone(String),
}

impl BlobHandleError {
    /// 协议 4018 `details.reason` 值 / the protocol 4018 reason string。
    pub fn reason(&self) -> &'static str {
        match self {
            BlobHandleError::Invalid(_) => "invalid_handle",
            BlobHandleError::Forbidden(_) => "forbidden",
            BlobHandleError::Gone(_) => "gone",
        }
    }
}

/// 铸造期 payload 字节超 `too_large_cap` → 拒绝铸造（SDK 自决，无协议错误码）/ mint-time too-large reject。
///
/// 协议依据：`blob-transfer.md` §3 + `skill.md` §9「铸造期决断 too_large，不铸句柄」（DoS 防御）。本错误由
/// toolspool minter（BLB-03 #66）在铸造前抛出；codec 本身不产生。
#[derive(Debug, thiserror::Error)]
#[error("payload size {size} exceeds too_large_cap {cap}")]
pub struct BlobTooLargeError {
    /// 实际字节数 / actual byte size。
    pub size: u64,
    /// 上限 / the cap。
    pub cap: u64,
}

// ── 公开 API / Public API ────────────────────────────────────────────────

/// 铸造 `kind=skill` 不透明句柄 / Mint an opaque `kind="skill"` handle。
///
/// `name` = `A2CSkillRef.name`；`rel_path` = SKILL 包根 POSIX 相对路径（Computer 解析时**重跑沙箱**，本编码不
/// 校验）。返回 base64url 字符串（无填充 `=`，URL 安全）。
pub fn encode_skill_handle(name: &str, rel_path: &str) -> String {
    #[derive(Serialize)]
    struct SkillWire<'a> {
        v: u8,
        kind: &'a str,
        name: &'a str,
        rel_path: &'a str,
    }
    encode_named(&SkillWire {
        v: HANDLE_VERSION,
        kind: HANDLE_KIND_SKILL,
        name,
        rel_path,
    })
}

/// 铸造 `kind=toolspool` 不透明句柄 / Mint an opaque `kind="toolspool"` handle。
///
/// `cid` = 内容寻址 `sha256` hex；`mime` = 内容 MIME。返回 base64url 字符串。
pub fn encode_toolspool_handle(cid: &str, mime: &str) -> String {
    #[derive(Serialize)]
    struct ToolspoolWire<'a> {
        v: u8,
        kind: &'a str,
        cid: &'a str,
        mime: &'a str,
    }
    encode_named(&ToolspoolWire {
        v: HANDLE_VERSION,
        kind: HANDLE_KIND_TOOLSPOOL,
        cid,
        mime,
    })
}

/// 解码 `blob_handle` → [`DecodedHandle`]。
///
/// 解码失败的所有路径（非 base64url / 非 msgpack / 非 map / 缺字段 / `v` 未知 / kind 未知）一律
/// [`BlobHandleError::Invalid`]（由调用方映射 `4018 invalid_handle`）。协议 §5.4「不信任句柄内容」。
pub fn decode_blob_handle(handle: &str) -> Result<DecodedHandle, BlobHandleError> {
    let raw = decode_bytes(handle)?;
    // msgpack 自描述：解成 serde_json::Value 再按键校验（对齐 Python「解成 dict 再查键」）。
    let value: Value = rmp_serde::from_slice(&raw)
        .map_err(|e| BlobHandleError::Invalid(format!("msgpack decode failed: {e}")))?;
    let obj = value
        .as_object()
        .ok_or_else(|| BlobHandleError::Invalid("handle payload must be a map".to_string()))?;

    match obj.get("v").and_then(Value::as_u64) {
        Some(v) if v == u64::from(HANDLE_VERSION) => {}
        _ => {
            return Err(BlobHandleError::Invalid(format!(
                "unsupported handle version: {:?}",
                obj.get("v")
            )));
        }
    }

    match obj.get("kind").and_then(Value::as_str) {
        Some(HANDLE_KIND_SKILL) => {
            match (
                obj.get("name").and_then(Value::as_str),
                obj.get("rel_path").and_then(Value::as_str),
            ) {
                (Some(name), Some(rel_path)) => Ok(DecodedHandle::Skill(SkillHandlePayload {
                    name: name.to_string(),
                    rel_path: rel_path.to_string(),
                })),
                _ => Err(BlobHandleError::Invalid(
                    "skill handle missing name / rel_path".to_string(),
                )),
            }
        }
        Some(HANDLE_KIND_TOOLSPOOL) => {
            match (
                obj.get("cid").and_then(Value::as_str),
                obj.get("mime").and_then(Value::as_str),
            ) {
                (Some(cid), Some(mime)) => Ok(DecodedHandle::Toolspool(ToolspoolHandlePayload {
                    cid: cid.to_string(),
                    mime: mime.to_string(),
                })),
                _ => Err(BlobHandleError::Invalid(
                    "toolspool handle missing cid / mime".to_string(),
                )),
            }
        }
        other => Err(BlobHandleError::Invalid(format!(
            "unknown handle kind: {other:?}"
        ))),
    }
}

// ── 内部辅助 / Internal helpers ──────────────────────────────────────────

/// msgpack（string-keyed map）+ base64url 无填充编码 / msgpack named-map + base64url no-pad。
fn encode_named<T: Serialize>(wire: &T) -> String {
    // 固定结构（仅 u8 + &str 字段）的 msgpack 编码不会失败；理论错误退化为空串前缀不影响安全边界。
    let packed =
        rmp_serde::to_vec_named(wire).expect("msgpack encode of fixed handle struct is infallible");
    B64.encode(packed)
}

/// base64url 解码（容忍带 / 不带填充；空串 / 非法 → Invalid）/ base64url decode (padding-tolerant)。
fn decode_bytes(handle: &str) -> Result<Vec<u8>, BlobHandleError> {
    if handle.is_empty() {
        return Err(BlobHandleError::Invalid("empty handle".to_string()));
    }
    // 去尾部 `=` 再按无填充解码——兼容带 / 不带填充两种铸造产物。
    let trimmed = handle.trim_end_matches('=');
    B64.decode(trimmed)
        .map_err(|e| BlobHandleError::Invalid(format!("base64url decode failed: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 把任意 serde_json::Value 编为 blob_handle（构造非法句柄用）/ encode an arbitrary value as a handle。
    fn encode_value(value: &Value) -> String {
        B64.encode(rmp_serde::to_vec_named(value).unwrap())
    }

    // ── round-trip ──────────────────────────────────────────────────────
    #[test]
    fn skill_handle_roundtrip() {
        let h = encode_skill_handle("mcp::figma::token", "skills/token/SKILL.md");
        assert_eq!(
            decode_blob_handle(&h).unwrap(),
            DecodedHandle::Skill(SkillHandlePayload {
                name: "mcp::figma::token".to_string(),
                rel_path: "skills/token/SKILL.md".to_string(),
            })
        );
    }

    #[test]
    fn toolspool_handle_roundtrip() {
        let h = encode_toolspool_handle("deadbeef", "image/png");
        let decoded = decode_blob_handle(&h).unwrap();
        assert_eq!(decoded.kind(), HANDLE_KIND_TOOLSPOOL);
        assert_eq!(
            decoded,
            DecodedHandle::Toolspool(ToolspoolHandlePayload {
                cid: "deadbeef".to_string(),
                mime: "image/png".to_string(),
            })
        );
    }

    // ── URL-safe / 无填充 / 兼容填充 ───────────────────────────────────
    #[test]
    fn handle_is_url_safe_without_padding() {
        let h = encode_skill_handle("n", "p");
        assert!(!h.contains('='), "handle must be unpadded");
        assert!(
            !h.contains('+') && !h.contains('/'),
            "handle must be URL-safe alphabet"
        );
        assert!(h
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn decode_tolerates_optional_padding() {
        let h = encode_toolspool_handle("c", "text/plain");
        // 人为补齐到 4 的倍数（部分编码器会带 `=`）/ re-pad to a multiple of 4
        let mut padded = h.clone();
        while !padded.len().is_multiple_of(4) {
            padded.push('=');
        }
        assert_eq!(
            decode_blob_handle(&padded).unwrap(),
            decode_blob_handle(&h).unwrap()
        );
    }

    // ── invalid 路径 ───────────────────────────────────────────────────
    #[test]
    fn empty_handle_invalid() {
        let e = decode_blob_handle("").unwrap_err();
        assert_eq!(e.reason(), "invalid_handle");
    }

    #[test]
    fn tampered_or_truncated_invalid() {
        let h = encode_skill_handle("name", "rel");
        // 截断 / truncate
        assert!(decode_blob_handle(&h[..h.len() - 3]).is_err());
        // 篡改字节 / flip a char to garble the msgpack
        let mut bytes = h.into_bytes();
        bytes[0] = if bytes[0] == b'A' { b'B' } else { b'A' };
        let tampered = String::from_utf8(bytes).unwrap();
        // 要么 base64/msgpack 解码失败、要么字段缺失——总之 Invalid（不 panic）
        if let Err(e) = decode_blob_handle(&tampered) {
            assert_eq!(e.reason(), "invalid_handle");
        }
    }

    #[test]
    fn non_base64url_invalid() {
        // 标准 base64 字符 '+' '/' 不在 URL-safe 字母表 → Invalid
        assert!(decode_blob_handle("++//").is_err());
        // 非 ASCII → Invalid
        assert!(decode_blob_handle("汉字").is_err());
    }

    #[test]
    fn non_msgpack_invalid() {
        // 0xc1 是 msgpack「永不使用」标记 → 解码失败
        let h = B64.encode([0xc1_u8]);
        let e = decode_blob_handle(&h).unwrap_err();
        assert_eq!(e.reason(), "invalid_handle");
    }

    #[test]
    fn payload_not_map_invalid() {
        let h = encode_value(&json!([1, 2, 3])); // msgpack array, not map
        assert!(matches!(
            decode_blob_handle(&h),
            Err(BlobHandleError::Invalid(_))
        ));
    }

    #[test]
    fn unknown_version_invalid() {
        let h = encode_value(&json!({"v": 2, "kind": "skill", "name": "n", "rel_path": "r"}));
        assert!(decode_blob_handle(&h).is_err());
    }

    #[test]
    fn unknown_kind_invalid() {
        let h = encode_value(&json!({"v": 1, "kind": "bogus", "x": 1}));
        assert!(decode_blob_handle(&h).is_err());
    }

    #[test]
    fn missing_fields_invalid() {
        // skill 缺 rel_path / toolspool 缺 mime
        let s = encode_value(&json!({"v": 1, "kind": "skill", "name": "n"}));
        assert!(decode_blob_handle(&s).is_err());
        let t = encode_value(&json!({"v": 1, "kind": "toolspool", "cid": "c"}));
        assert!(decode_blob_handle(&t).is_err());
    }

    // ── reason 枚举 + too_large ─────────────────────────────────────────
    #[test]
    fn reason_strings_match_protocol_enum() {
        assert_eq!(
            BlobHandleError::Invalid(String::new()).reason(),
            "invalid_handle"
        );
        assert_eq!(
            BlobHandleError::Forbidden(String::new()).reason(),
            "forbidden"
        );
        assert_eq!(BlobHandleError::Gone(String::new()).reason(), "gone");
    }

    #[test]
    fn too_large_error_carries_size_cap() {
        let e = BlobTooLargeError {
            size: 9_000,
            cap: 4_096,
        };
        assert_eq!(e.size, 9_000);
        assert_eq!(e.cap, 4_096);
        assert!(e.to_string().contains("9000") && e.to_string().contains("4096"));
    }

    // ── 跨-SDK 字节兼容：固定输入 → 稳定字节 ────────────────────────────
    #[test]
    fn encoding_is_deterministic_named_map() {
        // 同输入两次编码字节一致（string-keyed map，无随机性）→ 跨-SDK 互解的前提
        assert_eq!(encode_skill_handle("a", "b"), encode_skill_handle("a", "b"));
        // 解码我们自己的 named-map 与手工构造的等价 map 一致 → 与 Python msgpack(map) 同构
        let ours = encode_skill_handle("a", "b");
        let manual = encode_value(&json!({"v": 1, "kind": "skill", "name": "a", "rel_path": "b"}));
        assert_eq!(
            decode_blob_handle(&ours).unwrap(),
            decode_blob_handle(&manual).unwrap()
        );
    }
}
