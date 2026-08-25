/*!
* 文件名: upload.rs
* 作者: JQQ
* 创建日期: 2026/08/24
* 版权: 2023 JQQ. All rights reserved.
* 依赖: smcp
* 描述: `client:put_blob` 上行写入的有界上传会话管理（v0.4.0 #195）。
*       Bounded upload-session management for the `client:put_blob` write channel (v0.4.0 #195).
*/

//! `client:put_blob` 上行写入的有界上传会话管理（v0.4.0 #195）。
//!
//! 协议依据 / Protocol: ``a2c-smcp-protocol`` ``docs/specification/blob-transfer.md`` §3（事件 +
//! 上传会话生命周期）/ §7（landing 沙箱）；``error-handling.md`` §4019（reason 开放枚举）。
//! 镜像 Python 参考实现 / mirrors: ``a2c_smcp/computer/blob/upload.py``（#196）。
//!
//! 核心不变量 / Core invariants:
//!   - **写入沙箱由写入原语强制**（§7）：一切落盘（``.part`` 与最终产物）构造上严格落于 landing root
//!     内（``upload_id`` 派生安全名 + 消毒后 ``name_hint``）；Agent 拿不到写任意路径的能力。
//!   - **有界会话 MUST**（§3）：闲置超时 + 并发上限 + 孤儿 ``.part`` GC（阈值经 [`BlobThresholds`]
//!     注入，SDK 自治、不进协议常量）；无跨尝试断点（失败重试 = 新 ``upload_id`` 从 0 重传）。
//!   - **声明-校验镜像**：Agent 首块声明 ``total_size`` / ``sha256``，Computer 增量计算、末块比对；
//!     不符 → ``4019 integrity``（丢弃 ``.part``，不返回 path）。
//!   - **in-order 强制**：``chunk_offset`` == 已收字节（无稀疏缓冲）；末块另需
//!     ``chunk_offset + 本块字节数 == total_size``。
//!   - **fail-closed**：landing root 未配置 / 不可写 → ``4019 forbidden``（零字节落盘）。
//!
//! 落盘布局 / On-disk layout:
//!   - in-flight:  ``<landingRoot>/.a2c-upload/<upload_id>.part``
//!   - finalized: ``<landingRoot>/<upload_id>[_<sanitized name_hint>]``（``rename`` 原子定稿）
//!   - GC 严格限于 ``.a2c-upload`` 目录成员（不越授权边界，computer-management §7 不变量 #5）。
//!
//! 与 Python 的有意分叉 / Deliberate divergences from Python:
//! - base64 解码用 crate 默认 padding 容错（Python ``validate=True`` 严格）；合法向量逐字节一致。
//! - 畸形类型字段（如 ``total_size: true``）在 wire serde 层失败 → 无 ack（``get_blob`` 先例），
//!   Python 会回 ``invalid_declaration`` ack。
//! - 围栏断言（安全名构造被破坏的「不可能」态）以 ``4019 io_error`` fail-closed 呈现，Python 抛
//!   ``RuntimeError`` —— 本项目避免 panic 打断 socketio 处理循环。
//!
//! 4019 flat `ErrorPayload` 为协议级错误载荷（宽 flat 结构）——`Result<_, ErrorPayload>` 是
//! 本模块签名**有意如此**（与 Python 的 `PutBlobRet | ErrorPayload` 同构），按仓库先例模块级
//! allow `result_large_err`，不做装箱（保真优先）。
#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{Instant, SystemTime};

use base64::Engine as _;
use regex::Regex;
use sha2::{Digest, Sha256};
use smcp::utils::hash::to_hex;
use smcp::{ErrorCode, ErrorPayload, PutBlobReq, PutBlobRet};

use super::thresholds::BlobThresholds;

/// in-flight ``.part`` 子目录名（landing root 内，与最终产物分离，GC 只扫这里）。
/// In-flight ``.part`` subdirectory inside the landing root.
pub const PART_DIR_NAME: &str = ".a2c-upload";

/// ``name_hint`` 消毒白名单：字母 / 数字 / ``.`` / ``-`` / ``_``；其余字符折叠为 ``_``。
/// Sanitization whitelist: alnum plus ``.-_``; everything else folds to ``_``.
static SAFE_NAME_CHARS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[^A-Za-z0-9._-]+").unwrap());

/// sha256 十六进制形态判定（64 位小写 hex）/ 64-char lowercase-hex sha256 shape.
static SHA256_HEX_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9a-f]{64}$").unwrap());

const MAX_NAME_HINT_LEN: usize = 64;

/// 构造 ``4019 Blob Write Failed`` flat ErrorPayload（``reason`` 等经 ``details`` 下沉）。
fn blob_write_error(reason: &str, message: impl Into<String>) -> ErrorPayload {
    ErrorPayload::from_error_code(ErrorCode::BlobWriteFailed, message.into())
        .with_detail("reason", reason)
}

/// 消毒 ``name_hint`` 为安全文件名片段 / Sanitize ``name_hint`` into a safe filename fragment.
///
/// 规则 / Rules（协议 §7「Computer 生成安全名」的 rust-sdk 实现，镜像 python「SDK 自治」）:
///   - ``None`` / 空 → 空串（最终名 = 纯 ``upload_id``）
///   - 白名单外字符折叠 ``_``、剥前后 ``._-``、长度夹取 64；消毒后为空（如 ``"../.."``）→ 空串
///
/// 返回值恒可安全嵌入 ``f"{upload_id}_{fragment}"``（``upload_id`` 为 hex32 前缀，构造上杜绝穿越）。
pub fn sanitize_name_hint(name_hint: Option<&str>) -> String {
    let Some(raw) = name_hint else {
        return String::new();
    };
    if raw.is_empty() {
        return String::new();
    }
    let fragment = SAFE_NAME_CHARS_RE.replace_all(raw.trim(), "_");
    let fragment: String = fragment.chars().take(MAX_NAME_HINT_LEN).collect();
    fragment
        .trim_matches(|c| c == '.' || c == '_' || c == '-')
        .to_string()
}

/// 单个在途上传会话的受限状态 / The bounded state of one in-flight upload.
struct UploadSession {
    upload_id: String,
    part_path: PathBuf,
    final_path: PathBuf,
    file: fs::File,
    received: u64,
    hasher: Sha256,
    last_active: Instant,
    total_size: u64,
    declared_sha256: String,
}

impl UploadSession {
    /// 关闭句柄并删除残留 ``.part`` 文件（幂等）/ Close + unlink the ``.part`` (idempotent).
    fn close(&mut self) {
        if let Err(e) = fs::remove_file(&self.part_path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(
                    "put_blob: cannot unlink stale .part {:?}: {e}",
                    self.part_path
                );
            }
        }
    }
}

/// ``client:put_blob`` 上传会话表（线程安全，有界 MUST）。
///
/// The upload-session table for ``client:put_blob`` (thread-safe; bounded per protocol MUST).
///
/// landing root 由调用方每次传入（config-first：Computer 从 settings resolve 的 ``landingRoot``
/// 取值，进程生命周期内缓存——运行期变更需重启 Computer，部署决策）。会话状态（``.part`` 句柄 /
/// 已收字节 / 增量 hasher）**只在内存**；进程重启即全部作废，遗留 ``.part`` 由孤儿 GC 回收。
pub struct BlobUploadStore {
    landing_root: Option<PathBuf>,
    thresholds: BlobThresholds,
    sessions: Mutex<HashMap<String, UploadSession>>,
}

impl BlobUploadStore {
    pub fn new(landing_root: Option<PathBuf>, thresholds: BlobThresholds) -> Self {
        Self {
            landing_root,
            thresholds,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// 本 store 绑定的落盘根（``None`` = 未配置，fail-closed 拒绝一切上传）。
    pub fn landing_root(&self) -> Option<&Path> {
        self.landing_root.as_deref()
    }

    /// 处理单个 ``client:put_blob`` 块（首块 / 后续块 / 末块统一入口）。
    ///
    /// Handle one ``client:put_blob`` chunk (first / middle / final in one entry).
    ///
    /// 线程安全：全路径持锁（块级 ``.part`` 追加 ≤ chunk_max_bytes 量级，锁内 IO 可忽略——
    /// python 同款粒度，见 upload.py docstring）。
    pub fn handle_chunk(&self, req: &PutBlobReq) -> Result<PutBlobRet, ErrorPayload> {
        let mut sessions = self.sessions.lock().unwrap();
        self.expire_stale_sessions(&mut sessions);
        match req.upload_id.as_deref() {
            // 缺省即首块（offset 0）；``Some("")`` 走后续路径 → invalid_upload（python 保真）。
            None => self.handle_first_chunk(req, &mut sessions),
            Some(upload_id) => self.handle_subsequent_chunk(req, upload_id, &mut sessions),
        }
    }

    /// 作废全部在途会话（测试 / 关停清理）/ Drop all in-flight sessions (tests / shutdown).
    pub fn discard_all(&self) {
        let mut sessions = self.sessions.lock().unwrap();
        for (_, mut session) in sessions.drain() {
            session.close();
        }
    }

    // ── 首块 / First chunk ────────────────────────────────────────────────

    fn handle_first_chunk(
        &self,
        req: &PutBlobReq,
        sessions: &mut HashMap<String, UploadSession>,
    ) -> Result<PutBlobRet, ErrorPayload> {
        // 1) fail-closed：landing root 未配置 → forbidden（零字节落盘，§7）。
        let Some(root) = &self.landing_root else {
            tracing::warn!("client:put_blob rejected: landingRoot not configured (fail-closed)");
            return Err(blob_write_error("forbidden", "landing root not configured"));
        };

        // 2) 声明校验（字段齐备、total_size ≥ 1、sha256 为 64 位 hex）→ invalid_declaration。
        let total_size = match req.total_size {
            Some(v) if v >= 1 => v,
            _ => {
                tracing::warn!(
                    "client:put_blob invalid declaration: total_size={:?}",
                    req.total_size
                );
                return Err(blob_write_error(
                    "invalid_declaration",
                    "total_size must be an int >= 1",
                ));
            }
        };
        let declared_sha256 = match req.sha256.as_deref() {
            Some(v) if SHA256_HEX_RE.is_match(v) => v.to_ascii_lowercase(),
            other => {
                tracing::warn!("client:put_blob invalid declaration: sha256={other:?}");
                return Err(blob_write_error(
                    "invalid_declaration",
                    "sha256 must be a 64-char hex string",
                ));
            }
        };

        // 3) 绝对上限（首块决断，零字节落盘）→ too_large（拒绝路径不建任何目录/文件）。
        if total_size > self.thresholds.upload_max_bytes {
            tracing::warn!(
                "client:put_blob too_large: declared={total_size} cap={}",
                self.thresholds.upload_max_bytes
            );
            return Err(blob_write_error(
                "too_large",
                "declared total_size exceeds the upload cap",
            )
            .with_detail("total_size", total_size));
        }

        // 4) 并发上限 → busy（Agent SHOULD 退避后从 0 重传）。
        if (sessions.len() as u64) >= self.thresholds.upload_max_concurrent {
            tracing::warn!(
                "client:put_blob busy: {}/{} sessions in flight",
                sessions.len(),
                self.thresholds.upload_max_concurrent
            );
            return Err(blob_write_error("busy", "too many concurrent uploads"));
        }

        // 5) 接纳会话：建 landing 暂存目录（不可建 → forbidden「沙箱不可写」）+ 开 ``.part`` 句柄。
        let part_dir = root.join(PART_DIR_NAME);
        if let Err(e) = fs::create_dir_all(&part_dir) {
            tracing::warn!(
                "client:put_blob landing root not writable: {} ({e})",
                root.display()
            );
            return Err(blob_write_error("forbidden", "landing root not writable"));
        }
        let upload_id = uuid::Uuid::new_v4().simple().to_string();
        let fragment = sanitize_name_hint(req.name_hint.as_deref());
        let final_name = if fragment.is_empty() {
            upload_id.clone()
        } else {
            format!("{upload_id}_{fragment}")
        };
        let part_path = part_dir.join(format!("{upload_id}.part"));
        let final_path = root.join(&final_name);
        let file = match fs::File::create(&part_path) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(
                    "client:put_blob cannot create .part {}: {e}",
                    part_path.display()
                );
                return Err(blob_write_error("forbidden", "landing root not writable"));
            }
        };
        let session = UploadSession {
            upload_id: upload_id.clone(),
            part_path,
            final_path,
            file,
            received: 0,
            hasher: Sha256::new(),
            last_active: Instant::now(),
            total_size,
            declared_sha256,
        };
        // 孤儿 GC 挂在首块建立点（上传频率低，扫描代价可忽略）。覆盖「进程重启后表空、
        // 无 stale 可触发」的盲区——崩溃遗留 .part 在下一次上传时回收。
        self.collect_orphan_parts(sessions);
        // 首块即 eof（单块上传）为合法退化：fallthrough 到通用块路径一次定稿。
        self.append_chunk(req, session, sessions)
    }

    // ── 后续块 / Subsequent chunks ────────────────────────────────────────

    fn handle_subsequent_chunk(
        &self,
        req: &PutBlobReq,
        upload_id: &str,
        sessions: &mut HashMap<String, UploadSession>,
    ) -> Result<PutBlobRet, ErrorPayload> {
        let session = sessions.remove(upload_id).ok_or_else(|| {
            tracing::warn!("client:put_blob unknown/expired upload_id: {upload_id:?}");
            blob_write_error("invalid_upload", "unknown or expired upload session")
                .with_detail("upload_id", upload_id.to_string())
        })?;
        // 声明字段仅首块携带，后续块 MUST NOT（违反 → invalid_declaration，§3 流程 2）。
        // 会话**保留**（python 同构：仅返回错误不 drop——约束由合规 pump 保证，违规 Agent 由闲置 GC 收）。
        if req.total_size.is_some() || req.sha256.is_some() || req.name_hint.is_some() {
            tracing::warn!(
                "client:put_blob declaration fields re-sent on a subsequent chunk: {upload_id:?}"
            );
            sessions.insert(upload_id.to_string(), session);
            return Err(blob_write_error(
                "invalid_declaration",
                "declaration fields must only appear on the first chunk",
            ));
        }
        self.append_chunk(req, session, sessions)
    }

    // ── 通用块路径（首块 / 后续块共享）/ Common chunk path ─────────────────

    /// 追加 + 落定一块（`session` 按值进出：成功非 eof 放回表；终结失败不会重放回）。
    ///
    /// 会话保留语义（python 保真）：in-order 违反 / 坏 b64 / 末块尺寸不符 → `range` /
    /// `invalid_declaration` **不 drop**（表送回，闲置 GC 清）；过流违约 / IO 失败 / integrity
    /// 不符 → **drop**（删 ``.part``，Agent 重试 = 新 upload_id 从 0 重传）。
    fn append_chunk(
        &self,
        req: &PutBlobReq,
        mut session: UploadSession,
        sessions: &mut HashMap<String, UploadSession>,
    ) -> Result<PutBlobRet, ErrorPayload> {
        let upload_id = session.upload_id.clone();

        // in-order：chunk_offset == 已收字节（无稀疏缓冲）→ range（会话保留）。
        if req.chunk_offset != session.received {
            tracing::warn!(
                "client:put_blob out-of-order chunk: offset={} received={} upload_id={upload_id}",
                req.chunk_offset,
                session.received
            );
            sessions.insert(upload_id.clone(), session);
            return Err(
                blob_write_error("range", "chunk_offset does not match received bytes")
                    .with_detail("upload_id", upload_id),
            );
        }

        // base64 解码（wire 保证 str；padding 容错 vs python validate=True——见模块头分叉）。
        let chunk = match base64::engine::general_purpose::STANDARD.decode(&req.blob) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("client:put_blob base64 decode failed ({upload_id}): {e}");
                sessions.insert(upload_id.clone(), session);
                return Err(
                    blob_write_error("invalid_declaration", "blob is not valid base64")
                        .with_detail("upload_id", upload_id),
                );
            }
        };
        let chunk_len = chunk.len() as u64;

        // 过流防御（DoS）：任何块写后不得超首块声明的 total_size（声明即契约，§6）。超界即不可
        // 恢复违约 → 作废会话（Agent 重试 = 新 upload_id 从 0 重传）。
        if req.chunk_offset + chunk_len > session.total_size {
            tracing::warn!(
                "client:put_blob chunk overruns declared total_size: end={} declared={} upload_id={upload_id}",
                req.chunk_offset + chunk_len,
                session.total_size
            );
            session.close();
            return Err(
                blob_write_error("range", "received bytes exceed the declared total_size")
                    .with_detail("upload_id", upload_id),
            );
        }

        // 末块总量一致性：chunk_offset + 本块字节数 == total_size，否则 range（§3 流程 4）。
        if req.eof && req.chunk_offset + chunk_len != session.total_size {
            tracing::warn!(
                "client:put_blob final chunk size mismatch: end={} declared={} upload_id={upload_id}",
                req.chunk_offset + chunk_len,
                session.total_size
            );
            sessions.insert(upload_id.clone(), session);
            return Err(blob_write_error(
                "range",
                "final chunk does not complete the declared total_size",
            )
            .with_detail("upload_id", upload_id));
        }

        // 追加 + 增量 hash；IO 失败 → io_error（作废会话，删 .part）。
        if let Err(e) = session.file.write_all(&chunk) {
            tracing::warn!("client:put_blob .part write failed ({upload_id}): {e}");
            session.close();
            return Err(blob_write_error("io_error", "write to landing root failed")
                .with_detail("upload_id", upload_id));
        }
        session.hasher.update(&chunk);
        session.received += chunk_len;
        session.last_active = Instant::now();

        if !req.eof {
            let ret = PutBlobRet {
                upload_id: upload_id.clone(),
                chunk_offset: req.chunk_offset,
                landing_path: None,
                total_size: None,
                sha256: None,
                req_id: Some(req.base.req_id.clone()),
            };
            sessions.insert(upload_id, session);
            return Ok(ret);
        }
        self.finalize(req, session)
    }

    /// 末块定稿：fsync → 完整性比对 → 原子 rename 进 landing root（§3 流程 4）。
    fn finalize(
        &self,
        req: &PutBlobReq,
        mut session: UploadSession,
    ) -> Result<PutBlobRet, ErrorPayload> {
        let upload_id = session.upload_id.clone();
        if let Err(e) = session.file.sync_all() {
            tracing::warn!("client:put_blob fsync failed ({upload_id}): {e}");
            session.close();
            return Err(blob_write_error("io_error", "flushing the upload failed")
                .with_detail("upload_id", upload_id));
        }
        // 增量 hasher 的累积 digest 即 Computer 重算的全量 sha256（数学等价「末块重算」）。
        let recomputed = to_hex(&session.hasher.clone().finalize());
        if recomputed != session.declared_sha256 {
            tracing::warn!(
                "client:put_blob integrity mismatch: upload_id={upload_id} declared={} recomputed={recomputed}",
                session.declared_sha256
            );
            session.close();
            return Err(
                blob_write_error("integrity", "sha256 mismatch; upload discarded")
                    .with_detail("upload_id", upload_id),
            );
        }
        // 原子 rename 定稿（`.part` → 安全名产物）；IO 失败 → io_error。
        if let Err(e) = fs::rename(&session.part_path, &session.final_path) {
            tracing::warn!("client:put_blob finalize rename failed ({upload_id}): {e}");
            session.close();
            return Err(blob_write_error("io_error", "finalizing the upload failed")
                .with_detail("upload_id", upload_id));
        }
        let landing_path_str = session.final_path.to_string_lossy().to_string();
        // 围栏断言（防御纵深）：产物 resolve 后必须仍在 landing root 内（§7 不变量 #5）。
        self.assert_within_root(&session.final_path, &upload_id)?;
        Ok(PutBlobRet {
            upload_id,
            chunk_offset: req.chunk_offset,
            landing_path: Some(landing_path_str),
            total_size: Some(session.received),
            sha256: Some(recomputed),
            req_id: Some(req.base.req_id.clone()),
        })
    }

    // ── 有界会话（GC）/ Bounded sessions (GC) ─────────────────────────────

    /// 作废闲置超时会话（须持锁调用）/ Drop sessions idle beyond the timeout (caller holds lock).
    ///
    /// 协议 MUST（§3 生命周期表）：超时后该 ``upload_id`` → ``4019 invalid_upload``。
    fn expire_stale_sessions(&self, sessions: &mut HashMap<String, UploadSession>) {
        let now = Instant::now();
        let idle_timeout =
            std::time::Duration::from_secs(self.thresholds.upload_idle_timeout_seconds);
        let mut stale = Vec::new();
        for (id, s) in sessions.iter_mut() {
            if now.duration_since(s.last_active) > idle_timeout {
                stale.push(id.clone());
                s.close();
            }
        }
        for id in &stale {
            sessions.remove(id);
            tracing::info!("client:put_blob session expired (idle): {id}");
        }
        if !stale.is_empty() {
            self.collect_orphan_parts(sessions);
        }
    }

    /// 孤儿 ``.part`` GC（须持锁）：删 ``.a2c-upload`` 内不属于任何活跃会话的**超龄**文件。
    ///
    /// Orphan ``.part`` GC (caller holds lock): remove files in the staging dir that belong
    /// to no live session **and are older than the idle timeout** (mtime 宽限)。协议 MUST（§3）
    /// + GC 严格限于 landing root（§7 不变量 #5）——只扫 [`PART_DIR_NAME`] 目录成员，绝不越界。
    ///
    /// 为何按龄宽限（表外 ≠ 立即孤儿）：同机多 Computer 进程共享同一 user-scope ``landingRoot``
    /// 时，另一进程在途会话的 ``.part`` 也在本表之外——其 mtime 随每次写块刷新，永不满龄。
    fn collect_orphan_parts(&self, sessions: &mut HashMap<String, UploadSession>) {
        let Some(root) = &self.landing_root else {
            return;
        };
        let part_dir = root.join(PART_DIR_NAME);
        let live: std::collections::HashSet<String> = sessions
            .values()
            .map(|s| {
                s.part_path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string()
            })
            .collect();
        let idle_timeout =
            std::time::Duration::from_secs(self.thresholds.upload_idle_timeout_seconds);
        let Ok(entries) = fs::read_dir(&part_dir) else {
            return; // 目录不存在（尚无上传）/ absent until the first upload.
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                continue; // 只收文件；目录非本 store 产物，勿递归（围栏纪律）。
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if live.contains(&name) {
                continue;
            }
            // 未超龄：可能是共享 root 的兄弟进程在途会话（见 fn 文档）。
            let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else {
                continue;
            };
            let Ok(age) = SystemTime::now().duration_since(modified) else {
                continue;
            };
            if age <= idle_timeout {
                continue;
            }
            if let Err(e) = fs::remove_file(entry.path()) {
                tracing::debug!("client:put_blob orphan .part unlink failed: {e}");
            } else {
                tracing::debug!("client:put_blob orphan .part collected: {name}");
            }
        }
    }

    /// 防御纵深围栏：``path`` resolve 后必须落在 landing root 内（违规说明安全名构造被破坏）。
    /// 防御纵深围栏：``path`` resolve 后必须落在 landing root 内（违规说明安全名构造被破坏）。
    ///
    /// Python 以 ``RuntimeError`` 显式抛；本实现返 `io_error`（fail-closed 且不中断 socketio 循环）。
    /// **canonicalize 失败亦 fail-closed**（审查 #195 🟡6a：断言解析不了 ≠ 安全，宁可报 io_error）。
    fn assert_within_root(&self, path: &Path, upload_id: &str) -> Result<(), ErrorPayload> {
        let Some(root) = &self.landing_root else {
            return Ok(());
        };
        let root_resolved = fs::canonicalize(root).map_err(|e| {
            tracing::error!(
                "put_blob fence: landing root unresolvable {root:?} ({e}); fail-closed (upload_id={upload_id})"
            );
            blob_write_error(
                "io_error",
                format!("put_blob sandbox fence unresolvable for upload_id={upload_id}"),
            )
        })?;
        let resolved = fs::canonicalize(path).map_err(|e| {
            tracing::error!(
                "put_blob fence: finalized path unresolvable {path:?} ({e}); fail-closed (upload_id={upload_id})"
            );
            blob_write_error(
                "io_error",
                format!("put_blob sandbox fence unresolvable for upload_id={upload_id}"),
            )
        })?;
        if smcp::utils::path::is_within(&resolved, &root_resolved) {
            Ok(())
        } else {
            tracing::error!(
                "put_blob sandbox fence violated: {resolved:?} escaped landing root {root_resolved:?} (upload_id={upload_id})"
            );
            Err(blob_write_error(
                "io_error",
                format!("put_blob sandbox fence violated for upload_id={upload_id}"),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smcp::AgentCallData;
    use smcp::ReqId;

    fn sha_hex(data: &[u8]) -> String {
        to_hex(&Sha256::digest(data))
    }

    fn base(data: &[u8], offset: u64, eof: bool, total_size: u64, sha: &str) -> PutBlobReq {
        PutBlobReq {
            base: AgentCallData {
                agent: "a".to_string(),
                req_id: ReqId::from_string("r1".to_string()),
            },
            computer: "c".to_string(),
            upload_id: None,
            chunk_offset: offset,
            eof,
            total_size: Some(total_size),
            sha256: Some(sha.to_string()),
            name_hint: None,
            blob: base64::engine::general_purpose::STANDARD.encode(data),
        }
    }

    fn first_req(data: &[u8], eof: bool, sha: &str) -> PutBlobReq {
        base(data, 0, eof, data.len() as u64, sha)
    }

    fn chunk_req(upload_id: &str, data: &[u8], offset: u64, eof: bool) -> PutBlobReq {
        PutBlobReq {
            base: AgentCallData {
                agent: "a".to_string(),
                req_id: ReqId::from_string("r2".to_string()),
            },
            computer: "c".to_string(),
            upload_id: Some(upload_id.to_string()),
            chunk_offset: offset,
            eof,
            total_size: None,
            sha256: None,
            name_hint: None,
            blob: base64::engine::general_purpose::STANDARD.encode(data),
        }
    }

    /// 带 root 的 store + 一次完整首块 ack（返回 upload_id）。
    fn seeded_store(tmp: &tempfile::TempDir) -> (BlobUploadStore, String) {
        let store = BlobUploadStore::new(Some(tmp.path().to_path_buf()), BlobThresholds::default());
        let ack = store
            .handle_chunk(&first_req(&[0u8; 10], false, &"a".repeat(64)))
            .unwrap();
        (store, ack.upload_id)
    }

    fn reason_of(err: &ErrorPayload) -> String {
        err.details
            .as_ref()
            .and_then(|d| d.get("reason"))
            .and_then(|r| r.as_str())
            .unwrap_or_default()
            .to_string()
    }

    #[test]
    fn first_ack_returns_upload_id_without_landing_fields() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = BlobUploadStore::new(Some(tmp.path().to_path_buf()), BlobThresholds::default());
        let ret = store
            .handle_chunk(&first_req(b"hello", false, &"a".repeat(64)))
            .unwrap();
        assert_eq!(ret.upload_id.len(), 32);
        assert_eq!(ret.chunk_offset, 0);
        assert!(ret.landing_path.is_none());
        assert!(ret.total_size.is_none());
        assert!(ret.sha256.is_none());
    }

    #[test]
    fn multi_chunk_roundtrip_reassembles_on_disk() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = BlobUploadStore::new(Some(tmp.path().to_path_buf()), BlobThresholds::default());
        let data: Vec<u8> = (0..10240).map(|i| (i % 251) as u8).collect();
        let sha = sha_hex(&data);
        let chunk: usize = 256;
        let ack1 = store
            .handle_chunk(&base(&data[0..chunk], 0, false, data.len() as u64, &sha))
            .unwrap();
        let upload_id = ack1.upload_id.clone();
        let mut offset = chunk;
        while offset + chunk < data.len() {
            let req = chunk_req(
                &upload_id,
                &data[offset..offset + chunk],
                offset as u64,
                false,
            );
            let ack = store.handle_chunk(&req).unwrap();
            assert_eq!(ack.upload_id, upload_id);
            offset += chunk;
        }
        let req = chunk_req(&upload_id, &data[offset..], offset as u64, true);
        let final_ack = store.handle_chunk(&req).unwrap();
        assert_eq!(final_ack.total_size, Some(data.len() as u64));
        assert_eq!(final_ack.sha256.as_deref(), Some(sha.as_str()));
        let lp = final_ack.landing_path.clone().unwrap();
        assert_eq!(std::fs::read(&lp).unwrap(), data);
        // finalize 后 .part 消失。
        let part_dir = tmp.path().join(PART_DIR_NAME);
        assert_eq!(std::fs::read_dir(&part_dir).unwrap().count(), 0);
        // 最终产物在 landing root 内（严格成员）。
        let parent = std::path::Path::new(&lp).parent().unwrap();
        assert_eq!(parent, tmp.path());
    }

    #[test]
    fn single_chunk_eof_degenerate_finalizes() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = BlobUploadStore::new(Some(tmp.path().to_path_buf()), BlobThresholds::default());
        let data = b"one chunk";
        let ret = store
            .handle_chunk(&first_req(data, true, &sha_hex(data)))
            .unwrap();
        assert!(ret.landing_path.is_some());
        assert_eq!(ret.total_size, Some(data.len() as u64));
        assert_eq!(
            std::fs::read(ret.landing_path.as_ref().unwrap()).unwrap(),
            data
        );
    }

    #[test]
    fn unset_root_is_forbidden() {
        let store = BlobUploadStore::new(None, BlobThresholds::default());
        let err = store
            .handle_chunk(&first_req(b"hello", true, &"a".repeat(64)))
            .unwrap_err();
        assert_eq!(err.code, 4019);
        assert_eq!(err.details.unwrap()["reason"], "forbidden");
    }

    #[test]
    fn declaration_invalid_cases() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = BlobUploadStore::new(Some(tmp.path().to_path_buf()), BlobThresholds::default());
        // total_size 缺失（=0 语义由 u64 不可达）→ invalid_declaration。
        let mut req = first_req(b"x", true, &"a".repeat(64));
        req.total_size = None;
        let err = store.handle_chunk(&req).unwrap_err();
        assert_eq!(reason_of(&err), "invalid_declaration");
        // sha256 非 64-hex（大写 / 短 / 非 hex / 缺失）。
        for bad in ["A".repeat(64), String::from("abc"), "g".repeat(64)] {
            let err = store
                .handle_chunk(&first_req(b"x", true, &bad))
                .unwrap_err();
            assert_eq!(reason_of(&err), "invalid_declaration", "{bad}");
        }
        let mut req = first_req(b"x", true, &"a".repeat(64));
        req.sha256 = None;
        assert_eq!(
            reason_of(&store.handle_chunk(&req).unwrap_err()),
            "invalid_declaration"
        );
    }

    #[test]
    fn too_large_zero_bytes() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = BlobUploadStore::new(
            Some(tmp.path().to_path_buf()),
            BlobThresholds {
                upload_max_bytes: 8,
                ..BlobThresholds::default()
            },
        );
        let err = store
            .handle_chunk(&first_req(&[0u8; 9], false, &"a".repeat(64)))
            .unwrap_err();
        assert_eq!(reason_of(&err), "too_large");
        // 拒绝路径不建任何目录/文件。
        assert!(!tmp.path().join(PART_DIR_NAME).exists());
    }

    #[test]
    fn busy_at_concurrent_cap() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = BlobUploadStore::new(
            Some(tmp.path().to_path_buf()),
            BlobThresholds {
                upload_max_concurrent: 1,
                ..BlobThresholds::default()
            },
        );
        store
            .handle_chunk(&base(&[0u8; 10], 0, false, 10, &"a".repeat(64)))
            .unwrap();
        let err = store
            .handle_chunk(&first_req(b"x", true, &"b".repeat(64)))
            .unwrap_err();
        assert_eq!(reason_of(&err), "busy");
    }

    #[test]
    fn unknown_upload_id_is_invalid_upload() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = BlobUploadStore::new(Some(tmp.path().to_path_buf()), BlobThresholds::default());
        let err = store
            .handle_chunk(&chunk_req("nonexistent", b"x", 0, false))
            .unwrap_err();
        assert_eq!(reason_of(&err), "invalid_upload");
    }

    #[test]
    fn empty_string_upload_id_is_subsequent_chunk() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = BlobUploadStore::new(Some(tmp.path().to_path_buf()), BlobThresholds::default());
        let err = store
            .handle_chunk(&chunk_req("", b"x", 0, false))
            .unwrap_err();
        assert_eq!(reason_of(&err), "invalid_upload");
    }

    #[test]
    fn declaration_fields_resent_on_subsequent_chunk() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (store, id) = seeded_store(&tmp);
        let mut req = chunk_req(&id, b"x", 0, false);
        req.total_size = Some(11); // 欺瞒：再携声明字段。
        assert_eq!(
            reason_of(&store.handle_chunk(&req).unwrap_err()),
            "invalid_declaration"
        );
        // 会话保留（python 同构）：合法推进（offset 10 空块 eof）→ 静默走到定稿（此处 sha 不符 →
        // integrity）而非 invalid_upload —— 证明声明重发不 drop 会话。
        let err2 = store
            .handle_chunk(&chunk_req(&id, b"", 10, true))
            .unwrap_err();
        assert_eq!(reason_of(&err2), "integrity");
    }

    #[test]
    fn invalid_base64_is_invalid_declaration() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = BlobUploadStore::new(Some(tmp.path().to_path_buf()), BlobThresholds::default());
        // 首块（offset 0 == received 0，in-order 先过）但 blob 非合法 base64 → invalid_declaration。
        let mut req = first_req(b"x", true, &"a".repeat(64));
        req.blob = "@@@".to_string();
        assert_eq!(
            reason_of(&store.handle_chunk(&req).unwrap_err()),
            "invalid_declaration"
        );
    }

    #[test]
    fn out_of_order_is_range_and_session_kept() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = BlobUploadStore::new(Some(tmp.path().to_path_buf()), BlobThresholds::default());
        let ack = store
            .handle_chunk(&first_req(b"0123456789", false, &sha_hex(b"0123456789")))
            .unwrap();
        // offset 5（已收 10）→ range（会话保留）。
        let err = store
            .handle_chunk(&chunk_req(&ack.upload_id, b"x", 5, false))
            .unwrap_err();
        assert_eq!(reason_of(&err), "range");
        // 改回合法推进：offset 10 eof 空块 → 定稿成功（证明会话仍在表内而非 invalid_upload）。
        let final_ack = store
            .handle_chunk(&chunk_req(&ack.upload_id, b"", 10, true))
            .unwrap();
        assert!(final_ack.landing_path.is_some());
    }

    #[test]
    fn first_chunk_nonzero_offset_is_range() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = BlobUploadStore::new(Some(tmp.path().to_path_buf()), BlobThresholds::default());
        // 首块 offset != 0 → range（无稀疏缓冲；§3 首块含 upload_id 缺席 + offset 0）。
        let err = store
            .handle_chunk(&base(&[0u8; 3], 7, false, 10, &"a".repeat(64)))
            .unwrap_err();
        assert_eq!(reason_of(&err), "range");
    }

    #[test]
    fn final_chunk_undercompletes_declared_is_range() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = BlobUploadStore::new(Some(tmp.path().to_path_buf()), BlobThresholds::default());
        let ack = store
            .handle_chunk(&base(&[0u8; 4], 0, false, 10, &"a".repeat(64)))
            .unwrap();
        let err = store
            .handle_chunk(&chunk_req(&ack.upload_id, &[0u8; 2], 4, true))
            .unwrap_err();
        assert_eq!(reason_of(&err), "range");
    }

    #[test]
    fn overrun_guard_drops_session() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = BlobUploadStore::new(Some(tmp.path().to_path_buf()), BlobThresholds::default());
        let ack = store
            .handle_chunk(&base(&[0u8; 4], 0, false, 10, &"a".repeat(64)))
            .unwrap();
        // 非 eof 块但已超声明 → 违约 → 作废 + 删 .part。
        let err = store
            .handle_chunk(&chunk_req(&ack.upload_id, &[0u8; 9], 4, false))
            .unwrap_err();
        assert_eq!(reason_of(&err), "range");
        // 会话已删：后续块 → invalid_upload。
        let err2 = store
            .handle_chunk(&chunk_req(&ack.upload_id, b"x", 4, false))
            .unwrap_err();
        assert_eq!(reason_of(&err2), "invalid_upload");
    }

    #[test]
    fn integrity_mismatch_drops_and_no_artifact() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = BlobUploadStore::new(Some(tmp.path().to_path_buf()), BlobThresholds::default());
        // 声明 sha 与真实内容不符：单块 eof → integrity，丢弃不落盘。
        let err = store
            .handle_chunk(&first_req(b"hello", true, &"a".repeat(64)))
            .unwrap_err();
        assert_eq!(reason_of(&err), "integrity");
        let part_dir = tmp.path().join(PART_DIR_NAME);
        assert_eq!(std::fs::read_dir(&part_dir).unwrap().count(), 0);
        // 无最终产物。
        assert_eq!(
            std::fs::read_dir(tmp.path()).unwrap().count(),
            1,
            "仅 .a2c-upload"
        );
    }

    #[test]
    fn idle_timeout_expiry() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = BlobUploadStore::new(
            Some(tmp.path().to_path_buf()),
            BlobThresholds {
                upload_idle_timeout_seconds: 1,
                ..BlobThresholds::default()
            },
        );
        let ack = store
            .handle_chunk(&base(&[0u8; 10], 0, false, 10, &"a".repeat(64)))
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let err = store
            .handle_chunk(&chunk_req(&ack.upload_id, b"x", 0, false))
            .unwrap_err();
        assert_eq!(reason_of(&err), "invalid_upload");
        // 过期会话的 .part 已被删。
        assert_eq!(
            std::fs::read_dir(tmp.path().join(PART_DIR_NAME))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn orphan_gc_collects_crash_leftover() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = BlobUploadStore::new(
            Some(tmp.path().to_path_buf()),
            BlobThresholds {
                upload_idle_timeout_seconds: 1,
                ..BlobThresholds::default()
            },
        );
        let part_dir = tmp.path().join(PART_DIR_NAME);
        std::fs::create_dir_all(&part_dir).unwrap();
        let stale = part_dir.join("deadbeef.part");
        std::fs::write(&stale, b"x").unwrap();
        // 手动回拨 mtime 到超龄（避免测试内 sleep）。
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(10);
        fs::File::options()
            .write(true)
            .open(&stale)
            .unwrap()
            .set_modified(old)
            .unwrap();
        // 下一次首块上传触发孤儿 GC。
        store
            .handle_chunk(&base(&[0u8; 10], 0, false, 10, &"a".repeat(64)))
            .unwrap();
        assert!(!stale.exists(), "超龄孤儿应被 GC");
    }

    #[test]
    fn orphan_gc_age_grace_spares_fresh_sibling() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = BlobUploadStore::new(
            Some(tmp.path().to_path_buf()),
            BlobThresholds {
                upload_idle_timeout_seconds: 3600,
                ..BlobThresholds::default()
            },
        );
        let part_dir = tmp.path().join(PART_DIR_NAME);
        std::fs::create_dir_all(&part_dir).unwrap();
        let sibling = part_dir.join("sibling.part");
        std::fs::write(&sibling, b"y").unwrap();
        store
            .handle_chunk(&base(&[0u8; 10], 0, false, 10, &"a".repeat(64)))
            .unwrap();
        assert!(sibling.exists(), "未超龄兄弟文件必须保留");
    }

    #[test]
    fn discard_all_closes_sessions() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = BlobUploadStore::new(Some(tmp.path().to_path_buf()), BlobThresholds::default());
        store
            .handle_chunk(&base(&[0u8; 10], 0, false, 10, &"a".repeat(64)))
            .unwrap();
        store.discard_all();
        assert_eq!(
            std::fs::read_dir(tmp.path().join(PART_DIR_NAME))
                .unwrap()
                .count(),
            0
        );
    }

    /// 对拍 python（#196 实测向量）：4019 flat payload 逐字段一致（code 裸整数 / details.reason）。
    #[test]
    fn python_byte_compat_write_error_payload() {
        let err = blob_write_error("busy", "too many concurrent uploads");
        let v = serde_json::to_value(&err).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "code": 4019,
                "message": "too many concurrent uploads",
                "details": {"reason": "busy"}
            })
        );
    }

    /// 对拍 python `handle_chunk` 首块/末块 ack 键集（键缺席语义：中间 ack 无 landing 字段；
    /// python 实测首块 = {chunk_offset, req_id, upload_id}，末块 = {…, landing_path, sha256, total_size}）。
    #[test]
    fn python_byte_compat_ack_shapes() {
        use std::collections::BTreeSet;
        let tmp = tempfile::TempDir::new().unwrap();
        let store = BlobUploadStore::new(Some(tmp.path().to_path_buf()), BlobThresholds::default());
        let data = b"0123456789";
        let sha = sha_hex(data);
        let ack1 = store.handle_chunk(&first_req(data, false, &sha)).unwrap();
        let v1 = serde_json::to_value(&ack1).unwrap();
        let keys1: BTreeSet<&str> = v1.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys1,
            ["chunk_offset", "req_id", "upload_id"]
                .into_iter()
                .collect()
        );
        let ack2 = store
            .handle_chunk(&chunk_req(&ack1.upload_id, b"", 10, true))
            .unwrap();
        let v2 = serde_json::to_value(&ack2).unwrap();
        let keys2: BTreeSet<&str> = v2.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys2,
            [
                "chunk_offset",
                "landing_path",
                "req_id",
                "sha256",
                "total_size",
                "upload_id"
            ]
            .into_iter()
            .collect()
        );
        assert_eq!(v2["sha256"], serde_json::json!(sha));
        assert_eq!(v2["total_size"], serde_json::json!(10));
    }

    #[test]
    fn sanitize_name_hint_vectors() {
        assert_eq!(sanitize_name_hint(None), "");
        assert_eq!(sanitize_name_hint(Some("")), "");
        assert_eq!(sanitize_name_hint(Some("my.txt")), "my.txt");
        assert_eq!(sanitize_name_hint(Some("../../etc/passwd")), "etc_passwd");
        assert_eq!(sanitize_name_hint(Some("../../..")), "");
        assert_eq!(sanitize_name_hint(Some("  spaced name  ")), "spaced_name");
        assert_eq!(sanitize_name_hint(Some("..x.y-..")), "x.y");
        let long = "a".repeat(100);
        assert_eq!(sanitize_name_hint(Some(&long)).len(), 64);
    }
}
