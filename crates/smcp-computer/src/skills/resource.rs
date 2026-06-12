/*!
* 文件名: resource.rs
* 作者: JQQ
* 创建日期: 2026/06/03
* 最后修改日期: 2026/06/03
* 版权: 2023 JQQ. All rights reserved.
* 依赖: sha2, smcp::utils::mime（§6.4 单一权威）, crate::skills::{sandbox, frontmatter}
* 描述: SKILL 包内资源读取视图（对标 Python computer/skills/resource.py）/ In-package SKILL resource read view.
*/

//! SKILL 包内资源读取视图 / In-package SKILL resource read view。
//!
//! 协议依据 / Protocol: a2c-smcp-protocol `skill.md` §9（沙箱）、`blob-transfer.md` §3（完整性）。
//! 对标 Python `a2c_smcp/computer/skills/resource.py`。
//!
//! `client:get_skill` 铸造期（inline body vs blob_handle 决策）与 `client:get_blob` 解析期（重施沙箱后
//! 回源切片）**必须**对「Agent 最终消费字节」给出**一致**的 `total_size` / `sha256`。本模块把「沙箱解析
//! + frontmatter 剥离 + MIME + 流式 sha256 + 惰性切片」收敛到 [`resolve_skill_view`] 单一入口，两处共用。
//!
//! 消费字节基准 / Consumed-bytes baseline：SKILL.md 入口 = frontmatter 剥离后 body 的 UTF-8 字节；
//! 其它资源 = 原始字节。

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use smcp::utils::hash::to_hex;
use smcp::utils::mime::{guess_mime, is_text_mime};
use smcp::utils::slice::{plan_slice, SlicePlan};

use crate::skills::frontmatter::strip_skill_frontmatter;
use crate::skills::sandbox::{
    ensure_within_size_cap, resolve_skill_resource, SkillSandboxError, DEFAULT_SKILL_FILE,
};

/// SKILL.md 固定 MIME（frontmatter 剥离后仍是 markdown 正文）/ Fixed MIME for the SKILL.md entry doc。
pub const DEFAULT_SKILL_MIME: &str = "text/markdown";

/// 流式 sha256 / 惰性切片读块大小 / Read-block size for streaming sha256 & lazy slicing。
const HASH_BLOCK: usize = 1024 * 1024; // 1 MiB

/// 惰性切片来源 / Lazy slice source。
#[derive(Debug, Clone)]
enum Slicer {
    /// 内存字节（SKILL.md 剥离后 body）/ in-memory bytes (stripped SKILL.md body)。
    Mem(Vec<u8>),
    /// 磁盘文件（其它资源 seek+read）/ on-disk file (seek+read for other resources)。
    Disk(PathBuf),
}

impl Slicer {
    /// 读取已 bounds-clamp 的 `[offset, offset+length)` 字节 / Read pre-clamped bytes。
    fn read_slice(&self, offset: u64, length: u64) -> std::io::Result<Vec<u8>> {
        match self {
            Slicer::Mem(data) => {
                let o = offset as usize;
                let l = length as usize;
                Ok(data[o..o + l].to_vec())
            }
            Slicer::Disk(path) => {
                let mut file = File::open(path)?;
                file.seek(SeekFrom::Start(offset))?;
                let mut buf = vec![0u8; length as usize];
                file.read_exact(&mut buf)?;
                Ok(buf)
            }
        }
    }
}

/// 沙箱解析后的 SKILL 资源描述符 / Sandboxed SKILL resource descriptor。
///
/// `total_size` / `sha256` 均基于**消费字节**，[`slice`](Self::slice) / [`read_all`](Self::read_all)
/// 同样返回消费字节，故 get_skill 内联与 get_blob 切片字节完全一致。
#[derive(Debug, Clone)]
pub struct SkillResourceView {
    /// 回显用相对路径（缺省请求回显 `"SKILL.md"`）/ echoed relative path。
    pub rel_path: String,
    /// 沙箱校验后的真实绝对路径（realpath）/ realpath-resolved absolute path。
    pub abs_path: PathBuf,
    /// 资源 MIME / resource MIME。
    pub mime: String,
    /// 消费字节数 / consumed byte count。
    pub total_size: u64,
    /// 消费字节的 sha256 十六进制（流式）/ hex sha256 of consumed bytes。
    pub sha256: String,
    /// 是否服务 frontmatter 剥离后的 SKILL.md 入口 / serving the stripped SKILL.md entry。
    pub is_entry: bool,
    /// MIME 是否文本类（[`smcp::utils::mime::is_text_mime`]）/ whether MIME is textual。
    pub is_text: bool,
    slicer: Slicer,
}

impl SkillResourceView {
    /// 读取消费字节 `[offset, offset+length)`，截断到 `total_size` / Read consumed bytes, clamped to `total_size`。
    ///
    /// # Errors
    /// `offset > total_size` → [`std::io::ErrorKind::InvalidInput`]；磁盘读失败透传 IO 错误。
    pub fn slice(&self, offset: u64, length: u64) -> std::io::Result<Vec<u8>> {
        match plan_slice(offset, length, self.total_size) {
            SlicePlan::OutOfRange => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("offset {offset} > total_size {}", self.total_size),
            )),
            SlicePlan::Empty => Ok(Vec::new()),
            SlicePlan::Read { offset, length } => self.slicer.read_slice(offset, length),
        }
    }

    /// 读取全部消费字节（供 get_skill 内联 `body`）/ Read all consumed bytes。
    ///
    /// # Errors
    /// 同 [`slice`](Self::slice)。
    pub fn read_all(&self) -> std::io::Result<Vec<u8>> {
        self.slice(0, self.total_size)
    }
}

/// 流式计算文件 sha256（O([`HASH_BLOCK`]) 内存）/ Streaming file sha256 (bounded memory)。
fn stream_sha256(path: &Path) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; HASH_BLOCK];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(to_hex(&hasher.finalize()))
}

/// 沙箱解析 SKILL 包内资源 → [`SkillResourceView`]（mint / serve 共用唯一入口）/ Resolve to a resource view。
///
/// 流程：① [`resolve_skill_resource`] 包根内沙箱 → ② DoS 守卫（仅当 `too_large_cap` 给定，即 get_skill
/// 铸造期）先 stat 原始字节，超限 → `too_large`、**不读文件** → ③ 消费字节基准（SKILL.md 入口读全文 →
/// [`strip_skill_frontmatter`] → UTF-8 body；其它 → 原始磁盘字节）→ ④ `total_size`/`sha256` 基于消费字节。
///
/// §9.2 name 寻址防越权：**只**接受 `root`（Registry 经 name 解析的包根），绝不从 name/rel_path 推导路径。
///
/// # Errors
/// 沙箱拒绝（traversal/forbidden/not_found）或 too_large（仅 mint 期）→ [`SkillSandboxError`]。
pub fn resolve_skill_view(
    root: &Path,
    rel_path: Option<&str>,
    too_large_cap: Option<u64>,
) -> Result<SkillResourceView, SkillSandboxError> {
    let target = resolve_skill_resource(root, rel_path)?;
    let rel_echo = match rel_path {
        Some(p) if !p.is_empty() => p.to_string(),
        _ => DEFAULT_SKILL_FILE.to_string(),
    };
    let is_entry = target.file_name().and_then(|n| n.to_str()) == Some(DEFAULT_SKILL_FILE);

    if is_entry {
        // SKILL.md 入口：先 stat 守卫（避免读超大文件），再读全文剥 frontmatter。
        if let Some(cap) = too_large_cap {
            ensure_within_size_cap(&target, cap, &rel_echo)?;
        }
        let raw_bytes =
            std::fs::read(&target).map_err(|_| SkillSandboxError::not_found(rel_echo.clone()))?;
        // errors="replace" 等价：非法 UTF-8 → U+FFFD。
        let raw_text = String::from_utf8_lossy(&raw_bytes);
        let body_bytes = strip_skill_frontmatter(&raw_text).into_bytes();
        let total_size = body_bytes.len() as u64;
        // 守卫也覆盖剥离后 body（罕见：frontmatter 极小而 body 因 U+FFFD 膨胀）。
        if let Some(cap) = too_large_cap {
            if total_size > cap {
                return Err(SkillSandboxError::too_large(rel_echo, total_size));
            }
        }
        let sha256 = to_hex(&Sha256::digest(&body_bytes));
        return Ok(SkillResourceView {
            rel_path: rel_echo,
            abs_path: target,
            mime: DEFAULT_SKILL_MIME.to_string(),
            total_size,
            sha256,
            is_entry: true,
            is_text: true,
            slicer: Slicer::Mem(body_bytes),
        });
    }

    // 其它包内资源：原始字节 + 惰性 disk 切片 + 流式 sha256。size-cap 收敛到单一校验点。
    let raw_size = match too_large_cap {
        Some(cap) => ensure_within_size_cap(&target, cap, &rel_echo)?,
        None => file_size(&target, &rel_echo)?,
    };
    // §6.4(1)/(3)：确定性扩展名→MIME，绝不依赖宿主库（mime_guess 内置表对 .yaml/.toml/.rst 等
    // 与协议基线不一致、跨 SDK 漂移）；对标 Python `guess_mime(target.name)`。
    let file_name = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(rel_echo.as_str());
    let mime = guess_mime(file_name).to_string();
    let sha256 =
        stream_sha256(&target).map_err(|_| SkillSandboxError::not_found(rel_echo.clone()))?;
    // §6.4(2)：文本性判据三分支单一权威，与 Agent drain 门同源同判。
    let is_text = is_text_mime(&mime);
    Ok(SkillResourceView {
        rel_path: rel_echo,
        abs_path: target.clone(),
        mime,
        total_size: raw_size,
        sha256,
        is_entry: false,
        is_text,
        slicer: Slicer::Disk(target),
    })
}

/// 取文件字节数（stat 失败 → not_found）/ File byte size (stat failure → not_found)。
fn file_size(path: &Path, rel_path: &str) -> Result<u64, SkillSandboxError> {
    std::fs::metadata(path)
        .map(|m| m.len())
        .map_err(|_| SkillSandboxError::not_found(rel_path.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::sandbox::SkillSandboxReason;
    use std::fs;
    use tempfile::TempDir;

    fn pkg_with(skill_md: &str) -> TempDir {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("SKILL.md"), skill_md).unwrap();
        tmp
    }

    /// #81：铸造期 wire `mime_type` + `is_text` 走单一权威，遵循协议 §6.4（确定、跨 SDK 一致）。
    /// 重点覆盖 mime_guess 旧实现会漂移的扩展名（.yaml/.toml/.rst/.xml/.svg），值与 Python 逐项一致。
    #[test]
    fn test_subresource_mime_follows_spec_64() {
        let pkg = pkg_with("---\nname: d\n---\nb");
        // (相对路径, 期望 mime, 期望 is_text)：mime 值即 §6.4(3) 基线，与 Python guess_mime 同值。
        let cases: &[(&str, &str, bool)] = &[
            ("conf.yaml", "application/yaml", true), // 旧 mime_guess → text/x-yaml（漂移）
            ("conf.yml", "application/yaml", true),
            ("pyproject.toml", "application/toml", true), // 旧 mime_guess → text/x-toml（漂移）
            ("readme.rst", "text/x-rst", true),           // 旧 mime_guess → 缺失 → octet-stream
            ("data.xml", "application/xml", true),        // 旧 mime_guess → text/xml（漂移）
            ("logo.svg", "image/svg+xml", true), // +xml 后缀 → §6.4(2) 判文本（旧 looks_textual 漏判）
            ("icon.png", "image/png", false),    // 真二进制
            ("blob.bin", "application/octet-stream", false), // 未登记 → 确定回退
        ];
        for (rel, want_mime, want_text) in cases {
            fs::write(pkg.path().join(rel), "x").unwrap();
            let v = resolve_skill_view(pkg.path(), Some(rel), None).unwrap();
            assert_eq!(v.mime, *want_mime, "mime for {rel}");
            assert_eq!(v.is_text, *want_text, "is_text for {rel}");
        }
    }

    #[test]
    fn test_skill_md_strips_frontmatter_consistent() {
        let pkg = pkg_with("---\nname: demo\nversion: 1.0.0\n---\n# Title\n\nhello body\n");
        let body = "# Title\n\nhello body\n";
        let a = resolve_skill_view(pkg.path(), None, None).unwrap();
        assert!(a.is_entry && a.is_text);
        assert_eq!(a.mime, "text/markdown");
        assert_eq!(a.total_size, body.len() as u64);
        assert_eq!(a.read_all().unwrap(), body.as_bytes());
        assert_eq!(a.sha256, to_hex(&Sha256::digest(body.as_bytes())));
        // 显式 SKILL.md 与缺省一致。
        let b = resolve_skill_view(pkg.path(), Some("SKILL.md"), None).unwrap();
        assert_eq!(a.sha256, b.sha256);
        assert_eq!(a.total_size, b.total_size);
        // total_size < 原始（含 frontmatter）。
        let raw = fs::metadata(pkg.path().join("SKILL.md")).unwrap().len();
        assert!(a.total_size < raw);
    }

    #[test]
    fn test_subresource_raw_bytes_and_mime() {
        let pkg = pkg_with("---\nname: d\n---\nb");
        fs::write(pkg.path().join("note.txt"), "hello").unwrap();
        fs::write(pkg.path().join("data.json"), "{}").unwrap();
        let v = resolve_skill_view(pkg.path(), Some("note.txt"), None).unwrap();
        assert!(!v.is_entry && v.is_text);
        assert_eq!(v.total_size, 5);
        assert_eq!(v.read_all().unwrap(), b"hello");
        assert!(v.mime.starts_with("text/"));
        // json 属文本允许集。
        assert!(
            resolve_skill_view(pkg.path(), Some("data.json"), None)
                .unwrap()
                .is_text
        );
    }

    #[test]
    fn test_slice_mid_eof_and_oob() {
        let pkg = pkg_with("---\nname: d\n---\nb");
        fs::write(pkg.path().join("note.txt"), "0123456789").unwrap();
        let v = resolve_skill_view(pkg.path(), Some("note.txt"), None).unwrap();
        assert_eq!(v.slice(2, 3).unwrap(), b"234");
        assert_eq!(v.slice(v.total_size, 10).unwrap(), b""); // EOF
        assert_eq!(v.slice(8, 100).unwrap(), b"89"); // 截断到 total_size
        assert!(v.slice(v.total_size + 1, 1).is_err()); // 越界
    }

    #[test]
    fn test_too_large_guards() {
        let pkg = pkg_with("---\nname: d\n---\nbody-content");
        fs::write(pkg.path().join("big.bin"), vec![0u8; 100]).unwrap();
        // 子资源超 cap。
        let err = resolve_skill_view(pkg.path(), Some("big.bin"), Some(10)).unwrap_err();
        assert_eq!(err.reason, SkillSandboxReason::TooLarge);
        assert_eq!(err.total_size, Some(100));
        // 无 cap（get_blob 解析期）→ 不触发。
        assert!(resolve_skill_view(pkg.path(), Some("big.bin"), None).is_ok());
        // SKILL.md cap 触发。
        assert_eq!(
            resolve_skill_view(pkg.path(), None, Some(1))
                .unwrap_err()
                .reason,
            SkillSandboxReason::TooLarge
        );
    }

    #[test]
    fn test_sandbox_errors_propagate() {
        let pkg = pkg_with("---\nname: d\n---\nb");
        assert_eq!(
            resolve_skill_view(pkg.path(), Some("../x"), None)
                .unwrap_err()
                .reason,
            SkillSandboxReason::Traversal
        );
        assert_eq!(
            resolve_skill_view(pkg.path(), Some(".skillenv"), None)
                .unwrap_err()
                .reason,
            SkillSandboxReason::Forbidden
        );
        assert_eq!(
            resolve_skill_view(pkg.path(), Some("nope.txt"), None)
                .unwrap_err()
                .reason,
            SkillSandboxReason::NotFound
        );
    }
}
