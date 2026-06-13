/*!
* 文件名: resolver.rs
* 作者: JQQ
* 创建日期: 2026/06/05
* 最后修改日期: 2026/06/05
* 版权: 2023 JQQ. All rights reserved.
* 依赖: handle, skills::{registry, resource, sandbox}
* 描述: blob_handle 解析器抽象接口 + SKILL 解析器 + 惰性切片 ResolvedBlob。
*       blob_handle resolver trait + SKILL resolver + lazy-slice ResolvedBlob.
*/

//! `blob_handle` 解析器接口 + 内置 SKILL 解析器。
//!
//! 对标 Python 治理层资产 `a2c_smcp/computer/blob/resolver.py`（0.2.1）。设计依据
//! `design-0.2.1-skill-computer-management.md` §4.3「无状态可重解析」+ 协议 `blob-transfer.md` §5.4
//! 「不信任句柄内容」（解析时**重跑铸造通道边界校验**）。
//!
//! 核心契约：每次 `client:get_blob` 独立确定性回源、幂等、可并行（不同 chunk_offset）；
//! `kind=skill` → 仅以 `name` 经 [`SkillRegistry`] O(1) 解析包根，再对 `rel_path` **重跑 §6 沙箱**，
//! **忽略句柄内任何路径推导**（§9.2 防越权）。`kind=toolspool` 解析器（`ToolspoolBlobResolver`）见
//! [`crate::blob::toolspool`]（BLB-03 #66）。
//!
//! 惰性切片（v0.2.1 #51）：[`ResolvedBlob::resolve`](BlobResolver::resolve) 期间 O(1) 内存——
//! `total_size` / `sha256` / `mime` 即时确定，字节经 [`ResolvedBlob::slice`] 按需回读。

use std::path::PathBuf;

use smcp::utils::slice::{plan_slice, SlicePlan};

use crate::blob::handle::{BlobHandleError, DecodedHandle, SkillHandlePayload};
use crate::skills::registry::SkillRegistry;
use crate::skills::resource::resolve_skill_view;
use crate::skills::sandbox::{SkillSandboxError, SkillSandboxReason};

/// 惰性切片闭包：`(offset, length) -> bytes`，由 resolver 在 `resolve()` 构造并注入 [`ResolvedBlob`]。
///
/// handler 切片时按需回读（每次独立 seek+read，O(1) 内存/次）。读源失败 → [`BlobHandleError::Gone`]
/// （源在铸造后消失）。Resolver-supplied lazy-read closure; source read failure → `Gone`。
pub type BlobSlicer = Box<dyn Fn(u64, u64) -> Result<Vec<u8>, BlobHandleError> + Send + Sync>;

/// `blob_handle` 解析后的资源描述符（惰性切片视图）/ Resolved blob descriptor (lazy slice view)。
///
/// `mime` 为 handle-authoritative（resolver 从句柄载荷透传）；`total_size` / `sha256` 基于**消费字节**，
/// 与铸造期严格一致。对标 Python `ResolvedBlob`。
pub struct ResolvedBlob {
    /// 内容 MIME（handle-authoritative）/ MIME from handle payload (authoritative)。
    pub mime: String,
    /// 资源总字节数 / total resource size in bytes。
    pub total_size: u64,
    /// 全量 `sha256` 十六进制（`kind=toolspool` 即 cid）/ full sha256 hex。
    pub sha256: String,
    /// 惰性读闭包（resolver 提供，永不为空）/ resolver-supplied lazy-read closure。
    slicer: BlobSlicer,
}

impl std::fmt::Debug for ResolvedBlob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 闭包字段不可 Debug，跳过 / closure field skipped。
        f.debug_struct("ResolvedBlob")
            .field("mime", &self.mime)
            .field("total_size", &self.total_size)
            .field("sha256", &self.sha256)
            .finish_non_exhaustive()
    }
}

impl ResolvedBlob {
    /// 构造 [`ResolvedBlob`]（resolver 实现侧用）/ Construct (resolver-side use)。
    pub fn new(mime: String, total_size: u64, sha256: String, slicer: BlobSlicer) -> Self {
        Self {
            mime,
            total_size,
            sha256,
            slicer,
        }
    }

    /// 读取 `[offset, offset+length)` 字节区间，截断到 `total_size`。
    ///
    /// 边界语义（对齐 Python `ResolvedBlob.slice` + `on_get_blob` 严格 `>` 守卫）：
    /// - `offset == total_size` → 空（EOF probe，**非** range error）；
    /// - `offset > total_size` → [`BlobHandleError::Range`]（→ 协议 4018 `range`）；
    /// - `length == 0` → 空（无 I/O 触发）；
    /// - `length` 超剩余 → 自动截断为 `total_size - offset`（不报错）。
    ///
    /// EOF probe 语义（`offset == total_size` 返回空、`>` 才报错）与 `client:get_blob` 严格 `>` 范围守卫
    /// 一致，避免破坏 HTTP-Range 风格的「探测末尾」客户端行为。
    pub fn slice(&self, offset: u64, length: u64) -> Result<Vec<u8>, BlobHandleError> {
        match plan_slice(offset, length, self.total_size) {
            SlicePlan::OutOfRange => Err(BlobHandleError::Range(format!(
                "offset {offset} > total_size {}",
                self.total_size
            ))),
            SlicePlan::Empty => Ok(Vec::new()),
            SlicePlan::Read { offset, length } => (self.slicer)(offset, length),
        }
    }
}

/// `blob_handle` 解析器抽象接口 / Resolver trait。
///
/// 实现者职责：解析 [`DecodedHandle`]（kind-specific payload）→ [`ResolvedBlob`]；**重施铸造通道边界
/// 校验**；越权 / 不可达 / 范围错误经 [`BlobHandleError`] 子类传达（`invalid_handle` / `forbidden` /
/// `gone` / `range`）；实现 **MUST** 无副作用、确定性、幂等——同一句柄可被任意并发 `get_blob` 解析。
pub trait BlobResolver: Send + Sync {
    /// 解析句柄 → [`ResolvedBlob`]；越界 / 失败抛 [`BlobHandleError`]。
    fn resolve(&self, handle: &DecodedHandle) -> Result<ResolvedBlob, BlobHandleError>;
}

/// `name → SKILL 包根` 解析接缝（由 [`SkillRegistry`] 实现；#68 集成注入共享句柄）。
///
/// 抽象出最小读能力，让 [`SkillBlobResolver`] 与 Registry 的并发包装（#68 决定 `Arc<RwLock<..>>` 等）
/// 解耦——本 issue 直接为真实 [`SkillRegistry`] 提供 blanket impl（DRY），集成期可再为共享句柄实现。
pub trait SkillRootLookup: Send + Sync {
    /// 解析**活跃** SKILL 的包根绝对路径；未命中 / 孤儿 → `None`。
    fn lookup_root(&self, name: &str) -> Option<PathBuf>;
}

impl SkillRootLookup for SkillRegistry {
    fn lookup_root(&self, name: &str) -> Option<PathBuf> {
        // resolve 仅返回活跃条目克隆；孤儿 / 未注册 → None（§9.2 name 唯一寻址）。
        self.resolve(name).map(|r| PathBuf::from(r.path))
    }
}

/// `kind="skill"` 解析器：name→Registry 解析包根 → `rel_path` 重跑 §6 沙箱 → 惰性切片回源。
///
/// 无状态、确定性、幂等（设计 §4.3）：**忽略句柄内任何路径推导**（协议 §5.4），仅以 `name` 经
/// [`SkillRootLookup`]（默认 [`SkillRegistry`]）解析包根，再对 `rel_path` 经 [`resolve_skill_view`]
/// **重跑沙箱**。`total_size` / `sha256` 基于消费字节（SKILL.md→frontmatter 剥离后 body；其它→原始字节），
/// 与 `client:get_skill` 铸造期严格一致。
///
/// 错误映射（→ 协议 4018 `details.reason`）：
/// - Registry 未命中 / 孤儿 → [`BlobHandleError::Gone`]（`gone`：SKILL 已卸载 / 不可用）；
/// - 沙箱 `not_found`（源文件铸造后消失）→ `Gone`；
/// - 沙箱 `traversal` / `forbidden` → [`BlobHandleError::Forbidden`]（`forbidden`）；
/// - `too_large` 不会出现（get_blob 不传 `too_large_cap`，协议 4018 无 too_large 语义）。
pub struct SkillBlobResolver<L: SkillRootLookup = SkillRegistry> {
    lookup: L,
}

impl<L: SkillRootLookup> SkillBlobResolver<L> {
    /// 用一个 `name → root` 解析器构造 / Construct with a name→root lookup。
    pub fn new(lookup: L) -> Self {
        Self { lookup }
    }

    fn resolve_skill(&self, payload: &SkillHandlePayload) -> Result<ResolvedBlob, BlobHandleError> {
        // name 经 Registry O(1) 解析包根；未命中 / 孤儿 → 铸造通道授权已撤销 → gone。
        let root = self.lookup.lookup_root(&payload.name).ok_or_else(|| {
            BlobHandleError::Gone(format!(
                "skill not resolvable: name={:?} (unregistered or orphaned)",
                payload.name
            ))
        })?;
        // rel_path 重跑沙箱；get_blob 不传 too_large_cap（超大资源由分块传输自然承载）。
        let view = resolve_skill_view(&root, Some(&payload.rel_path), None)
            .map_err(|e| map_sandbox_error(&e, &payload.name, &payload.rel_path))?;
        let mime = view.mime.clone();
        let total_size = view.total_size;
        let sha256 = view.sha256.clone();
        // 闭包仅捕获 view（**不**捕获 self）：每次 slice 独立 seek+read；读源失败 → gone。
        let slicer: BlobSlicer = Box::new(move |offset, length| {
            view.slice(offset, length)
                .map_err(|e| BlobHandleError::Gone(format!("skill resource read failed: {e}")))
        });
        Ok(ResolvedBlob::new(mime, total_size, sha256, slicer))
    }
}

impl<L: SkillRootLookup> BlobResolver for SkillBlobResolver<L> {
    fn resolve(&self, handle: &DecodedHandle) -> Result<ResolvedBlob, BlobHandleError> {
        match handle {
            DecodedHandle::Skill(payload) => self.resolve_skill(payload),
            other => Err(BlobHandleError::Invalid(format!(
                "SkillBlobResolver received non-skill handle: kind={}",
                other.kind()
            ))),
        }
    }
}

/// 沙箱错误 → 4018 reason 映射 / Map a sandbox error to a 4018-reason [`BlobHandleError`]。
///
/// `not_found` → `gone`（源铸造后消失）；`traversal` / `forbidden` / `too_large` → `forbidden`
/// （`too_large` 理论不达——get_blob 不传 cap；保守归 forbidden，不泄漏越权细节）。
fn map_sandbox_error(err: &SkillSandboxError, name: &str, rel_path: &str) -> BlobHandleError {
    match err.reason {
        SkillSandboxReason::NotFound => BlobHandleError::Gone(format!(
            "skill resource gone: name={name:?} rel_path={rel_path:?}"
        )),
        _ => BlobHandleError::Forbidden(format!(
            "skill resource forbidden: name={name:?} rel_path={rel_path:?} reason={}",
            err.reason.as_str()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blob::handle::{SkillHandlePayload, ToolspoolHandlePayload};
    use smcp::A2CSkillRef;
    use std::fs;
    use tempfile::TempDir;

    // ── ResolvedBlob.slice 边界矩阵（内存 slicer）─────────────────────────
    fn mem_blob(data: &[u8]) -> ResolvedBlob {
        let owned = data.to_vec();
        ResolvedBlob::new(
            "application/octet-stream".to_string(),
            owned.len() as u64,
            "sha".to_string(),
            Box::new(move |offset, length| {
                let start = offset as usize;
                let end = start + length as usize;
                Ok(owned[start..end].to_vec())
            }),
        )
    }

    #[test]
    fn slice_reads_in_range() {
        let b = mem_blob(b"0123456789");
        assert_eq!(b.slice(0, 4).unwrap(), b"0123");
        assert_eq!(b.slice(3, 3).unwrap(), b"345");
    }

    #[test]
    fn slice_over_length_truncates_to_total() {
        let b = mem_blob(b"0123456789");
        // length 超剩余 → 截断为 total - offset，不报错。
        assert_eq!(b.slice(8, 100).unwrap(), b"89");
        assert_eq!(b.slice(0, 100).unwrap(), b"0123456789");
    }

    #[test]
    fn slice_eof_probe_and_zero_length_are_empty() {
        let b = mem_blob(b"0123456789");
        // offset == total_size → 空（EOF probe，非 range error）。
        assert_eq!(b.slice(10, 5).unwrap(), b"");
        // length == 0 → 空。
        assert_eq!(b.slice(2, 0).unwrap(), b"");
    }

    #[test]
    fn slice_offset_past_total_is_range_error() {
        let b = mem_blob(b"0123456789");
        let e = b.slice(11, 1).unwrap_err();
        assert_eq!(e.reason(), "range");
    }

    // ── SkillBlobResolver（真实 Registry + 真实沙箱）──────────────────────
    const SKILL_MD: &str = "---\nname: my-helper\ndescription: d\n---\n# Body\nhello skill body\n";

    fn skill_ref(name: &str, path: &str) -> A2CSkillRef {
        A2CSkillRef {
            name: name.to_string(),
            source: "user".to_string(),
            uri: None,
            path: path.to_string(),
            description: "d".to_string(),
            license: None,
            compatibility: None,
            allowed_tools: None,
            version: None,
            skill_metadata: None,
        }
    }

    /// 建一个 SKILL 包（SKILL.md + sub.txt + .skillenv）/ Build a SKILL package。
    fn make_pkg() -> TempDir {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("SKILL.md"), SKILL_MD).unwrap();
        fs::write(tmp.path().join("sub.txt"), b"hello").unwrap();
        fs::write(tmp.path().join(".skillenv"), b"SECRET=1").unwrap();
        tmp
    }

    fn registry_with(name: &str, root: &std::path::Path) -> SkillRegistry {
        let mut reg = SkillRegistry::new();
        assert!(reg.register(skill_ref(name, root.to_str().unwrap())));
        reg
    }

    fn skill_handle(name: &str, rel_path: &str) -> DecodedHandle {
        DecodedHandle::Skill(SkillHandlePayload {
            name: name.to_string(),
            rel_path: rel_path.to_string(),
        })
    }

    #[test]
    fn resolves_skill_md_entry_with_frontmatter_stripped() {
        let pkg = make_pkg();
        let reg = registry_with("my-helper", pkg.path());
        let resolver = SkillBlobResolver::new(reg);

        let resolved = resolver
            .resolve(&skill_handle("my-helper", "SKILL.md"))
            .unwrap();
        assert_eq!(resolved.mime, "text/markdown");
        // 消费字节 = frontmatter 剥离后的 body。
        let body = resolved.slice(0, resolved.total_size).unwrap();
        let text = String::from_utf8(body).unwrap();
        assert!(text.contains("hello skill body"), "got: {text:?}");
        assert!(
            !text.contains("name: my-helper"),
            "frontmatter must be stripped"
        );
    }

    #[test]
    fn resolves_subresource_raw_bytes() {
        let pkg = make_pkg();
        let reg = registry_with("my-helper", pkg.path());
        let resolver = SkillBlobResolver::new(reg);

        let resolved = resolver
            .resolve(&skill_handle("my-helper", "sub.txt"))
            .unwrap();
        assert_eq!(resolved.total_size, 5);
        assert_eq!(resolved.slice(0, 5).unwrap(), b"hello");
        // 惰性切片：分两次回读拼回整体。
        assert_eq!(resolved.slice(0, 3).unwrap(), b"hel");
        assert_eq!(resolved.slice(3, 2).unwrap(), b"lo");
    }

    #[test]
    fn unregistered_name_is_gone() {
        let pkg = make_pkg();
        let reg = registry_with("my-helper", pkg.path());
        let resolver = SkillBlobResolver::new(reg);
        let e = resolver
            .resolve(&skill_handle("absent-skill", "SKILL.md"))
            .unwrap_err();
        assert_eq!(e.reason(), "gone");
    }

    #[test]
    fn orphaned_name_is_gone() {
        let pkg = make_pkg();
        let mut reg = registry_with("my-helper", pkg.path());
        assert!(reg.mark_orphan("my-helper"));
        let resolver = SkillBlobResolver::new(reg);
        let e = resolver
            .resolve(&skill_handle("my-helper", "SKILL.md"))
            .unwrap_err();
        assert_eq!(e.reason(), "gone");
    }

    #[test]
    fn missing_resource_is_gone() {
        let pkg = make_pkg();
        let reg = registry_with("my-helper", pkg.path());
        let resolver = SkillBlobResolver::new(reg);
        let e = resolver
            .resolve(&skill_handle("my-helper", "nope.txt"))
            .unwrap_err();
        assert_eq!(e.reason(), "gone");
    }

    #[test]
    fn forbidden_sensitive_file_is_forbidden() {
        let pkg = make_pkg();
        let reg = registry_with("my-helper", pkg.path());
        let resolver = SkillBlobResolver::new(reg);
        let e = resolver
            .resolve(&skill_handle("my-helper", ".skillenv"))
            .unwrap_err();
        assert_eq!(e.reason(), "forbidden");
    }

    #[test]
    fn traversal_rel_path_is_forbidden() {
        let pkg = make_pkg();
        let reg = registry_with("my-helper", pkg.path());
        let resolver = SkillBlobResolver::new(reg);
        // ".." 逃逸 → traversal → 映射 forbidden。
        let e = resolver
            .resolve(&skill_handle("my-helper", "../outside.txt"))
            .unwrap_err();
        assert_eq!(e.reason(), "forbidden");
        // 绝对路径 rel_path 同样被沙箱拒（句柄内路径推导被忽略，仅 Registry name 寻址）。
        let e2 = resolver
            .resolve(&skill_handle("my-helper", "/etc/passwd"))
            .unwrap_err();
        assert_eq!(e2.reason(), "forbidden");
    }

    #[test]
    fn root_comes_from_registry_not_handle() {
        // 在 pkg_a 注册 name，但 pkg_b 也有同名 SKILL.md（不同内容）。
        // 解析 root **只**来自 Registry（pkg_a），证明句柄不携带 / 不影响包根。
        let pkg_a = TempDir::new().unwrap();
        fs::write(
            pkg_a.path().join("SKILL.md"),
            "---\nname: my-helper\n---\nAAA from a\n",
        )
        .unwrap();
        let _pkg_b = TempDir::new().unwrap();
        let reg = registry_with("my-helper", pkg_a.path());
        let resolver = SkillBlobResolver::new(reg);
        let resolved = resolver
            .resolve(&skill_handle("my-helper", "SKILL.md"))
            .unwrap();
        let text = String::from_utf8(resolved.slice(0, resolved.total_size).unwrap()).unwrap();
        assert!(text.contains("AAA from a"));
    }

    #[test]
    fn wrong_kind_handle_is_invalid() {
        let pkg = make_pkg();
        let reg = registry_with("my-helper", pkg.path());
        let resolver = SkillBlobResolver::new(reg);
        let toolspool = DecodedHandle::Toolspool(ToolspoolHandlePayload {
            cid: "c".to_string(),
            mime: "image/png".to_string(),
        });
        let e = resolver.resolve(&toolspool).unwrap_err();
        assert_eq!(e.reason(), "invalid_handle");
    }
}
