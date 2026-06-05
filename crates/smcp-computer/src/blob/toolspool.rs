/*!
* 文件名: toolspool.rs
* 作者: JQQ
* 创建日期: 2026/06/05
* 最后修改日期: 2026/06/05
* 版权: 2023 JQQ. All rights reserved.
* 依赖: sha2, handle, resolver, smcp::utils::{atomic_io, path}
* 描述: tool_call 二进制旁路的内容寻址暂存（.blobspool）+ 惰性切片解析器。
*       Content-addressed staging for tool_call binary sideband + lazy-slice resolver.
*/

//! `tool_call` 二进制旁路的内容寻址暂存 / Content-addressed staging for the `tool_call` sideband。
//!
//! 对标 Python 治理层资产 `a2c_smcp/computer/blob/toolspool.py`（0.2.1）。设计依据
//! `design-0.2.1-skill-computer-management.md` §4.3「kind=toolspool」+「跨 Computer 重启可解析」。
//!
//! 布局 / Layout：
//! ```text
//! <cache_root>/.blobspool/<cid>      # 字节内容（裸 bytes，无 envelope）
//! <cache_root>/.blobspool/<cid>.mime # MIME 元数据（UTF-8，单行）
//! ```
//!
//! `cid = sha256` 十六进制——内容寻址保证：相同字节复用同一 cid（幂等写入）；句柄**跨 Computer 重启**
//! 仍可解析（cid 在磁盘则可读、不在则 `4018 gone`）；不签名 / 不 MAC（协议 §1/§5.4 把安全边界定在
//! resolver 重施 cid 白名单 + 路径围栏）。

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};
use smcp::utils::atomic_io::{atomic_write_bytes, atomic_write_text};
use smcp::utils::path::is_within;

use crate::blob::handle::{BlobHandleError, DecodedHandle};
use crate::blob::resolver::{BlobResolver, BlobSlicer, ResolvedBlob};

/// 暂存根子目录名（带前导点，与 SKILL Home 内 source 子目录命名风格分开）/ Staging subdir name。
pub const BLOBSPOOL_DIRNAME: &str = ".blobspool";

/// cid 形态：64 位十六进制（sha256）/ cid form: 64 hex chars (sha256)。
const CID_LEN: usize = 64;

/// `cid` 是否为合法形态（长度 64 + 小写 hex）/ Whether `cid` is well-formed (len 64, lowercase hex)。
///
/// 拒绝大写 / 非 hex / 含 `..` 或 `/` 的伪造 cid——在触 FS 前挡掉借 cid 路径越界。
fn is_cid_form(s: &str) -> bool {
    s.len() == CID_LEN
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `.blobspool` 条目元数据（**不读字节**）/ Entry metadata; never reads payload bytes。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobStat {
    /// 字节数 / size in bytes。
    pub size: u64,
    /// `.mime` sidecar 内容（已 trim）/ MIME from sidecar (trimmed)。
    pub mime: String,
}

/// `put` 失败 / store mint-time error。
#[derive(Debug, thiserror::Error)]
pub enum ToolspoolStoreError {
    /// `mime` 为空 / 非 ASCII（防误传非文本 MIME 走入暂存元数据）/ empty or non-ASCII mime。
    #[error("mime must be a non-empty ASCII string, got {0:?}")]
    InvalidMime(String),
    /// 落盘 IO 失败 / atomic write IO failure。
    #[error("toolspool store io error: {0}")]
    Io(#[from] std::io::Error),
}

/// 内容寻址 `.blobspool` 暂存（线程/协程安全：文件原子重命名 + 内容寻址幂等）。
///
/// Content-addressed `.blobspool` store; concurrency-safe via atomic rename + idempotency。
#[derive(Debug, Clone)]
pub struct ToolspoolBlobStore {
    /// 暂存根目录**已 canonicalize** 绝对路径（`<cache_root>/.blobspool`）/ canonicalized root。
    root: PathBuf,
}

impl ToolspoolBlobStore {
    /// 初始化暂存：`cache_root/.blobspool` `create_dir_all` 后 canonicalize / Initialize the store。
    ///
    /// # Errors
    /// 目录创建 / canonicalize 失败 → [`std::io::Error`]。
    pub fn new(cache_root: &Path) -> std::io::Result<Self> {
        let root_raw = cache_root.join(BLOBSPOOL_DIRNAME);
        std::fs::create_dir_all(&root_raw)?;
        // canonicalize 让后续 realpath 围栏（解析后仍在 root 内）有一致基准、并消解 symlink。
        let root = std::fs::canonicalize(&root_raw)?;
        Ok(Self { root })
    }

    /// 暂存根目录（已 canonicalize）/ The canonicalized staging root。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 写入字节内容；返回 `cid`（`sha256` hex）/ Stage bytes, return `cid`。
    ///
    /// 幂等：内容已存在则直接复用（不重写 blob），仍刷新 `.mime` 兜底首次写盘失败的脏状态。
    ///
    /// # Errors
    /// `mime` 为空 / 非 ASCII → [`ToolspoolStoreError::InvalidMime`]；落盘失败 → [`ToolspoolStoreError::Io`]。
    pub fn put(&self, payload: &[u8], mime: &str) -> Result<String, ToolspoolStoreError> {
        if mime.is_empty() || !mime.is_ascii() {
            return Err(ToolspoolStoreError::InvalidMime(mime.to_string()));
        }
        let cid = sha256_hex(payload);
        let blob_path = self.blob_path(&cid);
        let mime_path = self.mime_path(&cid);
        if blob_path.exists() {
            // 幂等：内容存在则复用；仍刷新 mime 以恢复极端的「blob 在、mime 缺/旧」脏状态。
            atomic_write_text(&mime_path, mime)?;
            return Ok(cid);
        }
        atomic_write_bytes(&blob_path, payload)?;
        atomic_write_text(&mime_path, mime)?;
        Ok(cid)
    }

    /// 按 `cid` 回查（一次性全量读）/ Look up by `cid` (eager full read)。
    ///
    /// 保留为 round-trip 便利接口；新代码 SHOULD 用 [`stat`](Self::stat) + [`read_chunk`](Self::read_chunk)
    /// 惰性读，避免大 blob 内存放大。[`ToolspoolBlobResolver`] 已走惰性接口。
    ///
    /// # Errors
    /// `cid` 非法 → [`BlobHandleError::Invalid`]；缺失 → [`BlobHandleError::Gone`]。
    pub fn get(&self, cid: &str) -> Result<(Vec<u8>, String), BlobHandleError> {
        validate_cid(cid)?;
        let blob_real = self.fenced_realpath(&self.blob_path(cid), cid)?;
        let mime_real = self.fenced_realpath(&self.mime_path(cid), cid)?;
        let payload = std::fs::read(&blob_real).map_err(|_| gone(cid))?;
        let mime = std::fs::read_to_string(&mime_real)
            .map_err(|_| gone(cid))?
            .trim()
            .to_string();
        Ok((payload, mime))
    }

    /// 读取条目元数据：尺寸 + MIME；**不读 payload 字节** / Read metadata (size + MIME), no payload read。
    ///
    /// # Errors
    /// `cid` 非法 → [`BlobHandleError::Invalid`]；缺失 → [`BlobHandleError::Gone`]。
    pub fn stat(&self, cid: &str) -> Result<BlobStat, BlobHandleError> {
        validate_cid(cid)?;
        let blob_real = self.fenced_realpath(&self.blob_path(cid), cid)?;
        let mime_real = self.fenced_realpath(&self.mime_path(cid), cid)?;
        let size = std::fs::metadata(&blob_real).map_err(|_| gone(cid))?.len();
        let mime = std::fs::read_to_string(&mime_real)
            .map_err(|_| gone(cid))?
            .trim()
            .to_string();
        Ok(BlobStat { size, mime })
    }

    /// 从 `cid` 字节流 `[offset, offset+length)` 区间读取；越界自然短读 / 空 / Lazy slice primitive。
    ///
    /// 每次独立 `open` + `seek` + `read`：`length == 0` → 空（无 I/O）；`offset >= size` → 空；
    /// `length` 超剩余 → 自然短读（OS 截断）。
    ///
    /// # Errors
    /// `cid` 非法 → [`BlobHandleError::Invalid`]；缺失 → [`BlobHandleError::Gone`]。
    pub fn read_chunk(
        &self,
        cid: &str,
        offset: u64,
        length: u64,
    ) -> Result<Vec<u8>, BlobHandleError> {
        if length == 0 {
            return Ok(Vec::new());
        }
        validate_cid(cid)?;
        let blob_real = self.fenced_realpath(&self.blob_path(cid), cid)?;
        let mut f = std::fs::File::open(&blob_real).map_err(|_| gone(cid))?;
        f.seek(SeekFrom::Start(offset)).map_err(|_| gone(cid))?;
        // take(length) + read_to_end：自然处理 EOF 短读（offset>=size → 0 字节）。
        let mut buf = Vec::new();
        f.take(length)
            .read_to_end(&mut buf)
            .map_err(|_| gone(cid))?;
        Ok(buf)
    }

    /// 轻量存在性探测，不读字节；`cid` 非法静默 `false` / Lightweight existence probe。
    pub fn exists(&self, cid: &str) -> bool {
        is_cid_form(cid) && self.blob_path(cid).is_file()
    }

    /// 枚举当前所有 cid（仅诊断 / GC 用，不保证排序）/ Enumerate all cids (diagnostics only)。
    ///
    /// 过滤掉 `.mime` sidecar 与任何不符合 cid 形态的条目（防御读盘脏数据）。
    pub fn iter_cids(&self) -> Vec<String> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return out;
        };
        for entry in entries.flatten() {
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name.ends_with(".mime") {
                continue;
            }
            if is_cid_form(name) {
                out.push(name.to_string());
            }
        }
        out
    }

    // ── 内部 / Internal ────────────────────────────────────────────────

    fn blob_path(&self, cid: &str) -> PathBuf {
        self.root.join(cid)
    }

    fn mime_path(&self, cid: &str) -> PathBuf {
        self.root.join(format!("{cid}.mime"))
    }

    /// canonicalize + realpath 围栏（解析后仍在 root 内，防 symlink 逃逸）/ realpath fence。
    ///
    /// 缺失 → `Gone`；解析后逃逸 root → `Invalid`（理论不可达——cid 已过白名单；出现即视为伪造句柄）。
    fn fenced_realpath(&self, path: &Path, cid: &str) -> Result<PathBuf, BlobHandleError> {
        let real = std::fs::canonicalize(path).map_err(|_| gone(cid))?;
        if !is_within(&real, &self.root) {
            return Err(BlobHandleError::Invalid(format!(
                "resolved path escapes blobspool root: cid={cid}"
            )));
        }
        Ok(real)
    }
}

/// `kind="toolspool"` 解析器：按 `cid` 回查 `.blobspool` 暂存（惰性切片）。
///
/// 跨进程重启可解析：cid 在磁盘 → 成功；不在 → [`BlobHandleError::Gone`]（`4018 gone`）。
/// `resolve()` 仅读 metadata（[`ToolspoolBlobStore::stat`]），不读 payload 字节；
/// [`ResolvedBlob::slice`] 经 [`ToolspoolBlobStore::read_chunk`] 按需 seek+read。
/// mime 以**句柄入参**为准（铸造期权威），磁盘 sidecar 仅诊断；cid 即 sha256 无需重算。
pub struct ToolspoolBlobResolver {
    store: Arc<ToolspoolBlobStore>,
}

impl ToolspoolBlobResolver {
    /// 用一个共享 store 构造 / Construct with a shared store。
    pub fn new(store: Arc<ToolspoolBlobStore>) -> Self {
        Self { store }
    }
}

impl BlobResolver for ToolspoolBlobResolver {
    fn resolve(&self, handle: &DecodedHandle) -> Result<ResolvedBlob, BlobHandleError> {
        let payload = match handle {
            DecodedHandle::Toolspool(p) => p,
            other => {
                return Err(BlobHandleError::Invalid(format!(
                    "ToolspoolBlobResolver received non-toolspool handle: kind={}",
                    other.kind()
                )));
            }
        };
        // stat 仅读元数据（size + sidecar mime）+ cid 白名单 + realpath 围栏 + Gone/Invalid 抛出。
        let info = self.store.stat(&payload.cid)?;
        // mime 以铸造时入参为准；磁盘旁路只作诊断，不参与协议返回。
        let _ = info.mime;
        // 闭包捕获 store（Arc）+ cid，不捕获 self：每次 slice 独立 seek+read（O(1) 内存/次）。
        let store = Arc::clone(&self.store);
        let cid = payload.cid.clone();
        let slicer: BlobSlicer =
            Box::new(move |offset, length| store.read_chunk(&cid, offset, length));
        // cid 即 sha256，无需重算（stat 已 fail-fast 路径校验）。
        Ok(ResolvedBlob::new(
            payload.mime.clone(),
            info.size,
            payload.cid.clone(),
            slicer,
        ))
    }
}

// ── 自由函数 / Free helpers ──────────────────────────────────────────────

/// cid 白名单校验：长度 64 + 小写 hex；否则 [`BlobHandleError::Invalid`]。
fn validate_cid(cid: &str) -> Result<(), BlobHandleError> {
    if is_cid_form(cid) {
        Ok(())
    } else {
        Err(BlobHandleError::Invalid(format!(
            "invalid cid form (must be sha256 hex): {cid:?}"
        )))
    }
}

/// 构造「条目缺失」Gone 错误 / Build a "missing entry" Gone error。
fn gone(cid: &str) -> BlobHandleError {
    BlobHandleError::Gone(format!("toolspool entry missing: cid={cid}"))
}

/// 计算字节的 sha256 十六进制 / sha256 hex of bytes。
fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blob::handle::{SkillHandlePayload, ToolspoolHandlePayload};
    use tempfile::TempDir;

    fn store_in(tmp: &TempDir) -> ToolspoolBlobStore {
        ToolspoolBlobStore::new(tmp.path()).unwrap()
    }

    fn toolspool_handle(cid: &str, mime: &str) -> DecodedHandle {
        DecodedHandle::Toolspool(ToolspoolHandlePayload {
            cid: cid.to_string(),
            mime: mime.to_string(),
        })
    }

    // ── put / 幂等 / get ─────────────────────────────────────────────────
    #[test]
    fn put_mints_cid_and_mime_sidecar() {
        let tmp = TempDir::new().unwrap();
        let store = store_in(&tmp);
        let cid = store.put(b"binary-payload", "image/png").unwrap();
        // cid = sha256 hex 形态。
        assert!(is_cid_form(&cid));
        assert_eq!(cid, sha256_hex(b"binary-payload"));
        // blob + .mime 两个文件落盘。
        assert!(store.root().join(&cid).is_file());
        assert!(store.root().join(format!("{cid}.mime")).is_file());
        // get 回读一致。
        let (payload, mime) = store.get(&cid).unwrap();
        assert_eq!(payload, b"binary-payload");
        assert_eq!(mime, "image/png");
    }

    #[test]
    fn put_is_idempotent_for_same_bytes() {
        let tmp = TempDir::new().unwrap();
        let store = store_in(&tmp);
        let a = store.put(b"same", "text/plain").unwrap();
        let b = store.put(b"same", "text/plain").unwrap();
        assert_eq!(a, b, "相同字节复用同一 cid");
        assert_eq!(store.iter_cids().len(), 1, "无重复副本");
    }

    #[test]
    fn put_rejects_empty_or_non_ascii_mime() {
        let tmp = TempDir::new().unwrap();
        let store = store_in(&tmp);
        assert!(matches!(
            store.put(b"x", ""),
            Err(ToolspoolStoreError::InvalidMime(_))
        ));
        assert!(matches!(
            store.put(b"x", "图像/png"),
            Err(ToolspoolStoreError::InvalidMime(_))
        ));
    }

    // ── stat / read_chunk lazy + 边界 ────────────────────────────────────
    #[test]
    fn stat_reads_metadata_only() {
        let tmp = TempDir::new().unwrap();
        let store = store_in(&tmp);
        let cid = store
            .put(b"0123456789", "application/octet-stream")
            .unwrap();
        let info = store.stat(&cid).unwrap();
        assert_eq!(info.size, 10);
        assert_eq!(info.mime, "application/octet-stream");
    }

    #[test]
    fn read_chunk_boundaries() {
        let tmp = TempDir::new().unwrap();
        let store = store_in(&tmp);
        let cid = store.put(b"0123456789", "text/plain").unwrap();
        // 正常切片。
        assert_eq!(store.read_chunk(&cid, 0, 4).unwrap(), b"0123");
        assert_eq!(store.read_chunk(&cid, 3, 3).unwrap(), b"345");
        // length == 0 → 空（无 I/O）。
        assert_eq!(store.read_chunk(&cid, 2, 0).unwrap(), b"");
        // offset >= size → 空。
        assert_eq!(store.read_chunk(&cid, 10, 5).unwrap(), b"");
        assert_eq!(store.read_chunk(&cid, 99, 5).unwrap(), b"");
        // length 超剩余 → 自然短读。
        assert_eq!(store.read_chunk(&cid, 8, 100).unwrap(), b"89");
    }

    // ── cid 白名单 / 缺失 / 围栏 ─────────────────────────────────────────
    #[test]
    fn invalid_cid_forms_are_invalid() {
        let tmp = TempDir::new().unwrap();
        let store = store_in(&tmp);
        for bad in ["abc", &"g".repeat(64), &"A".repeat(64), "../../etc/passwd"] {
            assert_eq!(
                store.stat(bad).unwrap_err().reason(),
                "invalid_handle",
                "cid={bad}"
            );
            assert_eq!(
                store.read_chunk(bad, 0, 1).unwrap_err().reason(),
                "invalid_handle",
                "cid={bad}"
            );
            assert!(!store.exists(bad));
        }
    }

    #[test]
    fn missing_cid_is_gone() {
        let tmp = TempDir::new().unwrap();
        let store = store_in(&tmp);
        let absent = "a".repeat(64);
        assert_eq!(store.stat(&absent).unwrap_err().reason(), "gone");
        assert_eq!(
            store.read_chunk(&absent, 0, 1).unwrap_err().reason(),
            "gone"
        );
        assert!(!store.exists(&absent));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_rejected() {
        use std::os::unix::fs::symlink;
        let tmp = TempDir::new().unwrap();
        let store = store_in(&tmp);
        // root 外的真实文件。
        let outside = TempDir::new().unwrap();
        let secret = outside.path().join("secret");
        std::fs::write(&secret, b"top-secret").unwrap();
        // 在 root 内用一个**合法形态** cid 名建 symlink → 指向 root 外。
        let cid = "b".repeat(64);
        symlink(&secret, store.root().join(&cid)).unwrap();
        // canonicalize 解析 symlink → 逃逸 root → Invalid（不泄漏内容）。
        assert_eq!(
            store.read_chunk(&cid, 0, 1).unwrap_err().reason(),
            "invalid_handle"
        );
    }

    // ── 跨重启可解析 + iter_cids 过滤 ───────────────────────────────────
    #[test]
    fn cid_survives_store_restart() {
        let tmp = TempDir::new().unwrap();
        let cid = {
            let store = store_in(&tmp);
            store.put(b"persisted", "text/plain").unwrap()
        };
        // 模拟重启：同 cache_root 重 new store。
        let store2 = store_in(&tmp);
        assert_eq!(store2.stat(&cid).unwrap().size, 9);
        assert_eq!(store2.read_chunk(&cid, 0, 9).unwrap(), b"persisted");
    }

    #[test]
    fn iter_cids_skips_sidecars_and_dirty_entries() {
        let tmp = TempDir::new().unwrap();
        let store = store_in(&tmp);
        let cid = store.put(b"real", "text/plain").unwrap();
        // 脏条目：非 cid 形态文件 + 短 hex。
        std::fs::write(store.root().join("not-a-cid.txt"), b"x").unwrap();
        std::fs::write(store.root().join("deadbeef"), b"x").unwrap();
        let cids = store.iter_cids();
        assert_eq!(cids, vec![cid], "只枚举合法 cid，跳过 .mime 与脏条目");
    }

    // ── ToolspoolBlobResolver ────────────────────────────────────────────
    #[test]
    fn resolver_resolves_lazy_slice() {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(store_in(&tmp));
        let cid = store.put(b"0123456789", "image/png").unwrap();
        let resolver = ToolspoolBlobResolver::new(Arc::clone(&store));

        // mime 以句柄入参为准（这里故意与磁盘 sidecar 一致，亦测 sha256==cid）。
        let resolved = resolver
            .resolve(&toolspool_handle(&cid, "image/jpeg"))
            .unwrap();
        assert_eq!(
            resolved.mime, "image/jpeg",
            "mime 取句柄入参，非磁盘 sidecar"
        );
        assert_eq!(resolved.sha256, cid);
        assert_eq!(resolved.total_size, 10);
        // 惰性切片正确：分块拼回。
        assert_eq!(resolved.slice(0, 4).unwrap(), b"0123");
        assert_eq!(resolved.slice(4, 6).unwrap(), b"456789");
        // EOF probe 空。
        assert_eq!(resolved.slice(10, 5).unwrap(), b"");
    }

    #[test]
    fn resolver_missing_cid_is_gone() {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(store_in(&tmp));
        let resolver = ToolspoolBlobResolver::new(store);
        let e = resolver
            .resolve(&toolspool_handle(&"c".repeat(64), "image/png"))
            .unwrap_err();
        assert_eq!(e.reason(), "gone");
    }

    #[test]
    fn resolver_wrong_kind_is_invalid() {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(store_in(&tmp));
        let resolver = ToolspoolBlobResolver::new(store);
        let skill = DecodedHandle::Skill(SkillHandlePayload {
            name: "n".to_string(),
            rel_path: "r".to_string(),
        });
        assert_eq!(
            resolver.resolve(&skill).unwrap_err().reason(),
            "invalid_handle"
        );
    }
}
