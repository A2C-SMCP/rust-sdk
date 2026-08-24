//! 统一通用二进制拉取例程 / Unified generic binary-pull routine（`drain_blob`）。
//!
//! 供 Agent SDK（async / sync）在 `get_skill` 与 `tool_call` 二进制旁路两处共用，避免拉取循环、
//! 错误协调、并行重组三处重复。对标 Python 参考实现 / mirrors the Python reference:
//! `a2c_smcp/utils/blob.py`（`drain_blob` / `drain_blob_sync` / `BlobTransferError`）。
//!
//! 协议依据 / Protocol: `a2c-smcp-protocol` `docs/specification/blob-transfer.md`（句柄契约 /
//! 生产者-消费者模型 / 安全模型）；错误码 4018（`details.reason` ∈ invalid_handle / forbidden /
//! gone / range，开放枚举）。
//!
//! 并行安全 / Parallel safety：`client:get_blob` 协议 §3 明文——`chunk_offset` 为资源字节绝对偏移、
//! Computer 无服务端状态 →「天然幂等、可并行不同 offset」。`concurrency > 1` 时本例程启用并行红利
//! （async 走 [`futures::stream::StreamExt::buffer_unordered`]，sync 走 [`std::thread::scope`]）。
//!
//! 错误协调矩阵 / Error coordination matrix：
//!
//! | 情形 / case                              | 处置 / handling                              |
//! |------------------------------------------|----------------------------------------------|
//! | 4018 `invalid_handle` / `forbidden` / `gone` / 未知 reason | 取消在飞 + 直接 [`BlobTransferError`]（fatal） |
//! | 4018 `range`（并行态）                    | 取消 + 串行 fallback 从 0 重读               |
//! | 4018 `range`（串行态）                    | fatal（`NotAccessible{Range}`）              |
//! | `sha256` / `total_size` 跨块漂移          | 取消 + 串行 fallback；串行态触发从 0 重读     |
//! | 全量 `sha256` 校验失败                    | 串行从 0 重读（最多 `max_retries`）→ 失败抛出 |
//!
//! 错误优先级（async 与 sync 一致）/ error priority (async & sync agree)：并发态收集所有已完成
//! 结果后按「fatal > recoverable」分派——**永不隐藏 fatal**。recoverable（range / 漂移）**不**提前
//! 中止采集，确保并存的 fatal 必被发现；**仅** fatal 提前取消在飞任务（best-effort）。
//!
//! 注：此处刻意比 Python 参考更稳健——Python `_drain_parallel_sync` 用 `as_completed` + 首错即停，
//! range 先完成会掩盖并存 fatal（与其 `_drain_parallel_async` 不一致）；已出建议报告促 Python 对齐。
//! Deliberately more robust than the Python reference, whose sync path can mask a co-occurring fatal.
//!
//! # 上行写 / Upstream write（client:put_blob，v0.4.0）
//!
//! [`pump_blob`] / [`pump_blob_sync`] 是对称的上行落盘例程：ack-paced **顺序**单块发送（协议
//! in-order 强制，无并行红利）；首块声明 `total_size` / `sha256` / 可选 `name_hint` --> 末块
//! `eof` 取 `landing_path`。**无自动重试**：`busy` 等 4019 落到 [`BlobUploadError::WriteFailed`]，
//! 退避后重试 = 新 `upload_id` 从 0 重传（协议 §3 无跨尝试断点），由调用方决定。
//! 对标 / mirrors the Python reference: `a2c_smcp/utils/blob.py`（`pump_blob` /
//! `pump_blob_sync` / `BlobUploadError` / `BlobUploadUnsupportedError`）。
//!
//! 能力门控（协议 §3）：自身 `PROTOCOL_VERSION` minor ≥ 0.4 才可发起上行（版本握手 MINOR 严格
//! 匹配且同房间传递）；首块超时视为不支持（〔`BlobUploadError::UploadUnsupported`〕载荷随异常
//! 保留）——防御性兜底，**不是**正式回退路径。

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

use base64::Engine as _;
use futures::stream::{self, StreamExt as _};

use crate::utils::hash::sha256_hex;
use crate::{ErrorCode, ErrorPayload, GetBlobRet, PutBlobRet};

/// 默认单块上限 256 KiB / default chunk-size cap.
///
/// 与 Computer 端 `BlobThresholds.chunk_max_bytes` 默认一致；最终单块大小由 Computer clamp 决定。
pub const DEFAULT_CHUNK_SIZE: u64 = 256 * 1024;

/// 默认串行重读上限 / default serial-reread cap（应对源漂移 / 全量 sha256 不一致）。
pub const DEFAULT_MAX_RETRIES: usize = 3;

// ── 公开类型 / Public types ──────────────────────────────────────────────

/// 单块拉取请求参数 / single-chunk pull request params（传入调用方注入的 `call`）。
///
/// 对标 Python `AsyncBlobCall` / `SyncBlobCall` 的 `(computer, blob_handle, chunk_offset,
/// max_chunk_bytes)` 四元。调用方（Agent SDK）据此封装底层 `client:get_blob` ack 调用，注入
/// `namespace` / `agent` / `req_id` 等业务字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobChunkRequest {
    /// 目标 Computer 名（仅诊断；`call` 已具体路由）/ target Computer (diagnostic only)。
    pub computer: String,
    /// 来自某生产者通道的不透明句柄 / opaque handle from a producer channel。
    pub blob_handle: String,
    /// 资源字节绝对偏移 / absolute byte offset into the resource。
    pub chunk_offset: u64,
    /// 客户建议单块上限（字节）/ client-suggested per-chunk byte cap。
    pub max_chunk_bytes: u64,
}

/// `drain_blob` 调优选项 / tuning options。
#[derive(Debug, Clone, Copy)]
pub struct DrainBlobOptions {
    /// 并发度：`1` 串行（保守默认），`>1` 启用并行模式 / `1` = serial (default), `>1` = parallel。
    pub concurrency: usize,
    /// 客户建议单块上限；`0` 取 [`DEFAULT_CHUNK_SIZE`] / suggested chunk size (`0` → default)。
    pub chunk_size: u64,
    /// 串行重读上限 / serial-reread cap（应对源漂移 / 全量 sha256 不一致）。
    ///
    /// 入口处夹取至 `≥ 1`（至少尝试一次）/ clamped to `≥ 1` at entry (always at least one attempt)。
    pub max_retries: usize,
}

impl Default for DrainBlobOptions {
    fn default() -> Self {
        Self {
            concurrency: 1,
            chunk_size: DEFAULT_CHUNK_SIZE,
            max_retries: DEFAULT_MAX_RETRIES,
        }
    }
}

/// 4018 `details.reason` 开放枚举 / open enum for the 4018 reason。
///
/// 协议要求解析方 MUST 容忍未知值并兜底（默认「不重试 + 诊断」）→ 未知 reason 落 [`Self::Other`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlobErrorReason {
    /// 句柄无法解析 / handle cannot be parsed。
    InvalidHandle,
    /// 句柄合法但拒绝访问 / handle valid but access forbidden。
    Forbidden,
    /// 资源已不存在 / resource gone（上层应回生产者重铸）。
    Gone,
    /// 请求 range 不可满足 / requested range not satisfiable（并行态可恢复）。
    Range,
    /// 未知 reason（协议要求容忍）/ unknown reason (protocol mandates tolerance)。
    Other(String),
}

impl BlobErrorReason {
    /// 解析协议 `details.reason` 字符串 / parse the protocol `details.reason` string。
    pub fn parse(reason: &str) -> Self {
        match reason {
            "invalid_handle" => Self::InvalidHandle,
            "forbidden" => Self::Forbidden,
            "gone" => Self::Gone,
            "range" => Self::Range,
            other => Self::Other(other.to_string()),
        }
    }

    /// 线格式 reason 字符串 / the wire reason string。
    pub fn as_str(&self) -> &str {
        match self {
            Self::InvalidHandle => "invalid_handle",
            Self::Forbidden => "forbidden",
            Self::Gone => "gone",
            Self::Range => "range",
            Self::Other(s) => s,
        }
    }
}

impl std::fmt::Display for BlobErrorReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `drain_blob` 拉取阶段不可恢复错误 / unrecoverable error during a `drain_blob` pull。
///
/// 对标 Python `BlobTransferError`（其 `reason` 字段在此细分为类型安全的变体）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BlobTransferError {
    /// 4018：blob 不可达，按 `details.reason` 细分 / 4018 blob not accessible, keyed by reason。
    #[error("blob not accessible (reason: {reason}): {message}")]
    NotAccessible {
        /// 协议 4018 `details.reason` / the 4018 reason。
        reason: BlobErrorReason,
        /// 协议 `message` / the protocol message。
        message: String,
    },
    /// 跨块源漂移经 `max_retries` 仍未消解（`sha256` / `total_size` 不稳定）/ source drift unresolved。
    #[error("blob source drift unresolved after {retries} reread(s)")]
    MaxRetriesExceeded {
        /// 已尝试的串行重读次数 / serial rereads attempted。
        retries: usize,
    },
    /// 分块响应 base64 解码失败 / chunk base64 decode failure。
    #[error("blob chunk base64 decode failed: {0}")]
    Decode(String),
    /// 其它（非 4018）协议错误码原样透传 / other (non-4018) protocol error code surfaced verbatim。
    #[error("blob transfer protocol error (code {code}): {message}")]
    Protocol {
        /// 协议 `ErrorPayload.code` / the protocol error code。
        code: i64,
        /// 协议 `message` / the protocol message。
        message: String,
    },
}

// ── 公开 API / Public API ────────────────────────────────────────────────

/// 异步拉取 blob 全量字节 / asynchronously pull all blob bytes（返回 `(payload_bytes, mime_type)`）。
///
/// `call` 为调用方注入的单块拉取函数：`Fn(BlobChunkRequest) -> Future<Output = Result<GetBlobRet,
/// ErrorPayload>>`——成功返回 [`GetBlobRet`]，协议级 ack 错误返回 [`ErrorPayload`]。
///
/// `concurrency > 1` 时利用协议 §3 并行红利并发拉取剩余块，按 offset 重组并校验全量 `sha256`；
/// 遇 `range` / 漂移自动回退串行从 0 重读。
///
/// # Errors
/// 返回 [`BlobTransferError`]：4018 `invalid_handle` / `forbidden` / `gone` / 未知 reason 不重试；
/// `range` 在串行态 fatal；`max_retries` 仍未通过 `sha256` 校验 → [`BlobTransferError::MaxRetriesExceeded`]。
pub async fn drain_blob<F, Fut>(
    call: F,
    computer: &str,
    blob_handle: &str,
    opts: DrainBlobOptions,
) -> Result<(Vec<u8>, String), BlobTransferError>
where
    F: Fn(BlobChunkRequest) -> Fut,
    Fut: Future<Output = Result<GetBlobRet, ErrorPayload>>,
{
    let chunk_size = effective_chunk(opts.chunk_size);
    // 夹取至 ≥1：至少尝试一次（`0` 会零次循环、不发 call 即 MaxRetriesExceeded，反直觉）。
    let max_retries = opts.max_retries.max(1);
    if opts.concurrency <= 1 {
        return drain_serial_async(&call, computer, blob_handle, chunk_size, max_retries).await;
    }
    match drain_parallel_async(&call, computer, blob_handle, chunk_size, opts.concurrency).await {
        Ok(v) => Ok(v),
        Err(ParallelErr::Fatal(e)) => Err(e),
        Err(ParallelErr::Fallback) => {
            drain_serial_async(&call, computer, blob_handle, chunk_size, max_retries).await
        }
    }
}

/// 同步拉取 blob 全量字节 / synchronous mirror of [`drain_blob`]。
///
/// `concurrency > 1` 用 [`std::thread::scope`] 并发拉取，错误协调矩阵与 async 版一致。
///
/// # Errors
/// 同 [`drain_blob`]。
pub fn drain_blob_sync<F>(
    call: F,
    computer: &str,
    blob_handle: &str,
    opts: DrainBlobOptions,
) -> Result<(Vec<u8>, String), BlobTransferError>
where
    F: Fn(BlobChunkRequest) -> Result<GetBlobRet, ErrorPayload> + Sync,
{
    let chunk_size = effective_chunk(opts.chunk_size);
    // 夹取至 ≥1：至少尝试一次（与 [`drain_blob`] 一致）。
    let max_retries = opts.max_retries.max(1);
    if opts.concurrency <= 1 {
        return drain_serial_sync(&call, computer, blob_handle, chunk_size, max_retries);
    }
    match drain_parallel_sync(&call, computer, blob_handle, chunk_size, opts.concurrency) {
        Ok(v) => Ok(v),
        Err(ParallelErr::Fatal(e)) => Err(e),
        Err(ParallelErr::Fallback) => {
            drain_serial_sync(&call, computer, blob_handle, chunk_size, max_retries)
        }
    }
}

// ── 上行（client:put_blob）公开类型 / Upstream (client:put_blob) public types ──

/// 首块声明 / first-chunk declaration（Agent 声明、Computer 校验）。
///
/// 对标 Python `create_put_blob_request` 的 `declaration` dict。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PutBlobDeclaration {
    /// 声明总字节（MUST ≥ 1）/ declared total bytes。
    pub total_size: u64,
    /// 声明全量 sha256（十六进制）/ declared full sha256 (hex)。
    pub sha256: String,
    /// 建议文件名（可选，Computer 消毒后采用或自定）/ preferred file name (optional)。
    pub name_hint: Option<String>,
}

/// 单块上行请求参数 / single-chunk upload params（传入调用方注入的 `call`）。
///
/// 对标 Python `AsyncPutCall` / `SyncPutCall` 的 `(upload_id, chunk_offset, eof, chunk_bytes,
/// declaration)` 元组：`upload_id` 为 `None` 即首块（携带 `declaration`）。调用方（Agent SDK）
/// 据此封装底层 `client:put_blob` ack 调用，注入 `agent` / `req_id`、base64 编码本块字节。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PutBlobChunkRequest {
    /// 目标 Computer 名（仅诊断；`call` 已具体路由）/ target Computer (diagnostic only)。
    pub computer: String,
    /// `None` ⟺ 首块（offset 0），Computer 分配并回传 / `None` ⟺ first chunk。
    pub upload_id: Option<String>,
    /// 本块起始字节偏移；MUST == Computer 已收字节（in-order）/ chunk start offset。
    pub chunk_offset: u64,
    /// 末块标志 / end-of-file marker。
    pub eof: bool,
    /// 本块原始字节（调用方负责 base64 编码进 wire）/ raw chunk bytes。
    pub chunk: Vec<u8>,
    /// 仅首块：声明 / first chunk only: declaration。
    pub declaration: Option<PutBlobDeclaration>,
}

/// `pump_blob` 调优选项 / tuning options。
#[derive(Debug, Clone)]
pub struct PumpBlobOptions {
    /// 建议文件名（仅首块送入声明）/ preferred file name (first chunk only)。
    pub name_hint: Option<String>,
    /// 单块原始字节上限；`0` 取 [`DEFAULT_CHUNK_SIZE`] / per-chunk raw-byte cap (`0` → default)。
    pub chunk_size: u64,
}

impl Default for PumpBlobOptions {
    fn default() -> Self {
        Self {
            name_hint: None,
            chunk_size: DEFAULT_CHUNK_SIZE,
        }
    }
}

/// 上行落盘成功结果 / final-chunk ack essentials。
///
/// 对标 Python `PutBlobResult`：`landing_path` 可直接嵌入后续 `client:tool_call` 参数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PutBlobResult {
    /// landing root 内绝对路径（Computer 生成安全名）/ absolute path in the landing root。
    pub landing_path: String,
    /// 实际落盘字节（== 声明值才成功）/ stored bytes。
    pub total_size: u64,
    /// Computer 重算全量 sha256（回显值，Agent SHOULD 比对声明）/ recomputed sha256 echo。
    pub sha256: String,
}

/// 4019 `details.reason` 开放枚举 / open enum for the 4019 reason。
///
/// 与 [`BlobErrorReason`]（4018 下行）分属两套 reason 集，MUST NOT 混用；未知值落 [`Self::Other`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlobWriteErrorReason {
    /// `upload_id` 无法识别 / 已过期 / unknown or expired session。
    InvalidUpload,
    /// 声明非法（字段齐备性 / 形状）/ invalid declaration。
    InvalidDeclaration,
    /// `chunk_offset` 与已收字节不符 / out-of-order offset。
    Range,
    /// 声明总字节超可配上限 / declared total exceeds the upload cap。
    TooLarge,
    /// 并发上传会话已达上限 / too many concurrent uploads。
    Busy,
    /// landing root 未配置 / 不可写 / denied by the landing sandbox。
    Forbidden,
    /// 重算 sha256 与声明不符（丢弃不落盘）/ integrity mismatch。
    Integrity,
    /// 落盘 IO 失败 / storage IO failure。
    IoError,
    /// 未知 reason（协议要求容忍）/ unknown reason。
    Other(String),
}

impl BlobWriteErrorReason {
    /// 解析协议 `details.reason` 字符串 / parse the protocol `details.reason` string。
    pub fn parse(reason: &str) -> Self {
        match reason {
            "invalid_upload" => Self::InvalidUpload,
            "invalid_declaration" => Self::InvalidDeclaration,
            "range" => Self::Range,
            "too_large" => Self::TooLarge,
            "busy" => Self::Busy,
            "forbidden" => Self::Forbidden,
            "integrity" => Self::Integrity,
            "io_error" => Self::IoError,
            other => Self::Other(other.to_string()),
        }
    }

    /// 线格式 reason 字符串 / the wire reason string。
    pub fn as_str(&self) -> &str {
        match self {
            Self::InvalidUpload => "invalid_upload",
            Self::InvalidDeclaration => "invalid_declaration",
            Self::Range => "range",
            Self::TooLarge => "too_large",
            Self::Busy => "busy",
            Self::Forbidden => "forbidden",
            Self::Integrity => "integrity",
            Self::IoError => "io_error",
            Self::Other(s) => s,
        }
    }
}

impl std::fmt::Display for BlobWriteErrorReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 单块调用失败信号 / per-chunk call failure signal（`call` 返回值）。
///
/// 对标 Python：单个 ack 回传的 flat `ErrorPayload` → [`Self::Protocol`]（对应
/// `_raise_for_put_blob_error` 的分类输入）；`socketio.TimeoutError` → [`Self::Timeout`]
/// （首块 → [`BlobUploadError::UploadUnsupported`]，后续块 → [`BlobUploadError::ChunkTransport`]）；
/// 其它传输异常 → [`Self::Transport`]。
///
/// 注：`ErrorPayload` 无 `Eq`（含 serde_json 宽字段）→ 本类型仅 `PartialEq`，与 `BlobTransferError` 同；
/// `Protocol` 持全 flat `ErrorPayload`（宽结构）→ 局部 allow `large_enum_variant`，勿装箱
/// （错误面保真优先，仓库先例）。
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum BlobChunkFailure {
    /// ack 回传 flat ErrorPayload（4019 / 其它协议码）/ ack carried a flat ErrorPayload。
    Protocol(ErrorPayload),
    /// ack 超时 / ack timeout。
    Timeout,
    /// 其它传输失败（断连等）/ other transport failure。
    Transport(String),
}

/// `pump_blob` 上行阶段错误 / upload-stage error。
///
/// 对标 Python `BlobUploadError` + `BlobUploadUnsupportedError`（`UnsupportedBySdk` /
/// `UploadUnsupported` 对应后者；`WriteFailed` 对应 4019 reason 细分；`Protocol` 对应非 4019
/// 协议码原样透传）。
///
/// 大变体 `UploadUnsupported` 持完整载荷（首块超时兜底需字节留上下文——协议 §3「字节留上下文
/// 不落盘」）。盒装会改变载荷语义（python 直接持 bytes），按仓库先例局部 allow 即可。
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BlobUploadError {
    /// 载荷为空（协议 `total_size >= 1`）/ empty payload。
    #[error("put_blob requires at least one byte (protocol total_size >= 1)")]
    EmptyPayload,
    /// `chunk_size < 1`（负 / 零切片会恒空 → offset 永不前进）/ bad chunk size。
    #[error("chunk_size must be >= 1, got {chunk_size}")]
    BadChunkSize {
        /// 给定的非法值 / the offending value。
        chunk_size: u64,
    },
    /// 自身 `PROTOCOL_VERSION` minor < 0.4（能力门控 fail-fast）/ SDK does not support uploads。
    #[error("client:put_blob requires protocol minor >= 0.4; this SDK speaks {0}")]
    UnsupportedBySdk(String),
    /// 首块超时（目标疑似不支持 put_blob；载荷随异常保留）/ first-chunk timeout。
    #[error("put_blob first chunk timed out; target likely predates protocol 0.4.0")]
    UploadUnsupported {
        /// 完整载荷（字节留上下文，不落盘）/ the full payload.
        data: Vec<u8>,
        /// 声明总字节 / declared total bytes。
        total_size: u64,
        /// 声明 sha256 / declared sha256。
        sha256: String,
        /// 建议文件名（如果有）/ preferred file name (if any)。
        name_hint: Option<String>,
    },
    /// 4019 写入失败，按 `details.reason` 细分 / 4019 write failed, keyed by reason。
    #[error("blob write failed (reason: {reason}): {message}")]
    WriteFailed {
        /// 协议 4019 `details.reason` / the 4019 reason。
        reason: BlobWriteErrorReason,
        /// 协议 `message` / the protocol message。
        message: String,
    },
    /// 末块 ack 缺失 / 空 `landing_path` / final ack missing landing_path。
    #[error("final chunk ack missing landing_path")]
    IncompleteAck,
    /// 末块回显 sha256 与声明不符（落盘内容与声明不符的损坏信号）/ echo sha mismatch。
    #[error("Computer echo sha256 {echo} != declared {declared}")]
    EchoMismatch {
        /// 回显值 / the echoed value。
        echo: String,
        /// 声明值 / the declared value。
        declared: String,
    },
    /// 其它（非 4019）协议错误码原样透传 / other (non-4019) protocol code surfaced verbatim。
    #[error("blob upload protocol error (code {code}): {message}")]
    Protocol {
        /// 协议 `ErrorPayload.code` / the protocol error code。
        code: i64,
        /// 协议 `message` / the protocol message。
        message: String,
    },
    /// 非首块超时 / 其它传输失败（传输故障，未归一为协议错误）/ chunk transport failure。
    #[error("put_blob chunk transport failure: {0}")]
    ChunkTransport(String),
}

// ── 上行公开 API / Upstream public API ────────────────────────────────────

/// 异步上行落盘 / upload bytes to the Computer landing root。
///
/// ack-paced **顺序**单块发送（协议 in-order 强制，无并行）：首块声明 `total_size` / `sha256` /
/// 可选 `name_hint` → 逐块 base64 → 末块 `eof` 取 `landing_path`。**无自动重试**：4019 `busy` /
/// `too_large` 等 → [`BlobUploadError::WriteFailed`]，调用方可按 reason 决定退避——任何失败重试 =
/// 新 `upload_id` 从 0 重传（协议 §3 无跨尝试断点）。
///
/// `call` 为调用方注入的单块上行函数：`Fn(PutBlobChunkRequest) -> Future<Output = Result<
/// PutBlobRet, BlobChunkFailure>>`——成功返回 [`PutBlobRet`]，失败返回 [`BlobChunkFailure`]。
///
/// # Errors
/// 入口自检（能力门控 minor ≥ 0.4 / 空载荷 / `chunk_size < 1`）后进入循环；首块超时 →
/// [`BlobUploadError::UploadUnsupported`]（载荷随异常保留）；4019 → [`WriteFailed`]；
/// 末块 `landing_path` 缺失 → [`BlobUploadError::IncompleteAck`]；回显 sha 不符 →
/// [`BlobUploadError::EchoMismatch`]。
///
/// [`WriteFailed`]: BlobUploadError::WriteFailed
pub async fn pump_blob<F, Fut>(
    call: F,
    computer: &str,
    data: &[u8],
    opts: PumpBlobOptions,
) -> Result<PutBlobResult, BlobUploadError>
where
    F: Fn(PutBlobChunkRequest) -> Fut,
    Fut: Future<Output = Result<PutBlobRet, BlobChunkFailure>>,
{
    ensure_upload_supported(crate::PROTOCOL_VERSION)?;
    if data.is_empty() {
        return Err(BlobUploadError::EmptyPayload);
    }
    // 显式 `chunk_size=0` → bad_chunk_size（镜像 python `_prepare_declaration`：0/负 显拒；
    // rust 无负 u64，调用方缺省经 `None→DEFAULT_CHUNK_SIZE` 表达）。
    if opts.chunk_size == 0 {
        return Err(BlobUploadError::BadChunkSize { chunk_size: 0 });
    }
    let chunk_size = opts.chunk_size;
    let total_size = data.len() as u64;
    let sha256 = sha256_hex(data);
    let mut upload_id: Option<String> = None;
    let mut offset: usize = 0;
    loop {
        // saturating_add：巨大 chunk_size 不得溢出（debug panic / release wrap 都会致切片 panic）。
        let end = offset.saturating_add(chunk_size as usize).min(data.len());
        let eof = end == data.len();
        let first = upload_id.is_none();
        let req = PutBlobChunkRequest {
            computer: computer.to_owned(),
            upload_id: upload_id.clone(),
            chunk_offset: offset as u64,
            eof,
            chunk: data[offset..end].to_vec(),
            declaration: first.then(|| PutBlobDeclaration {
                total_size,
                sha256: sha256.clone(),
                name_hint: opts.name_hint.clone(),
            }),
        };
        let ret = match call(req).await {
            Ok(ret) => ret,
            Err(BlobChunkFailure::Timeout) => {
                // 协议 §3 防御性兜底：首块超时视为不支持（字节留上下文，不落盘）。
                if first {
                    return Err(BlobUploadError::UploadUnsupported {
                        data: data.to_vec(),
                        total_size,
                        sha256,
                        name_hint: opts.name_hint.clone(),
                    });
                }
                return Err(BlobUploadError::ChunkTransport(
                    "put_blob chunk ack timed out".to_string(),
                ));
            }
            Err(BlobChunkFailure::Transport(e)) => {
                return Err(BlobUploadError::ChunkTransport(e));
            }
            Err(BlobChunkFailure::Protocol(e)) => return Err(classify_upload_failure(&e)),
        };
        upload_id = Some(ret.upload_id.clone());
        if eof {
            return finalize_upload(ret, &sha256, total_size);
        }
        offset = end;
    }
}

/// 同步上行落盘 / synchronous mirror of [`pump_blob`]。
///
/// # Errors
/// 同 [`pump_blob`]。
pub fn pump_blob_sync<F>(
    call: F,
    computer: &str,
    data: &[u8],
    opts: PumpBlobOptions,
) -> Result<PutBlobResult, BlobUploadError>
where
    F: Fn(PutBlobChunkRequest) -> Result<PutBlobRet, BlobChunkFailure>,
{
    ensure_upload_supported(crate::PROTOCOL_VERSION)?;
    if data.is_empty() {
        return Err(BlobUploadError::EmptyPayload);
    }
    // 显式 `chunk_size=0` → bad_chunk_size（镜像 python `_prepare_declaration`：0/负 显拒；
    // rust 无负 u64，调用方缺省经 `None→DEFAULT_CHUNK_SIZE` 表达）。
    if opts.chunk_size == 0 {
        return Err(BlobUploadError::BadChunkSize { chunk_size: 0 });
    }
    let chunk_size = opts.chunk_size;
    let total_size = data.len() as u64;
    let sha256 = sha256_hex(data);
    let mut upload_id: Option<String> = None;
    let mut offset: usize = 0;
    loop {
        // saturating_add：巨大 chunk_size 不得溢出（debug panic / release wrap 都会致切片 panic）。
        let end = offset.saturating_add(chunk_size as usize).min(data.len());
        let eof = end == data.len();
        let first = upload_id.is_none();
        let req = PutBlobChunkRequest {
            computer: computer.to_owned(),
            upload_id: upload_id.clone(),
            chunk_offset: offset as u64,
            eof,
            chunk: data[offset..end].to_vec(),
            declaration: first.then(|| PutBlobDeclaration {
                total_size,
                sha256: sha256.clone(),
                name_hint: opts.name_hint.clone(),
            }),
        };
        let ret = match call(req) {
            Ok(ret) => ret,
            Err(BlobChunkFailure::Timeout) => {
                if first {
                    return Err(BlobUploadError::UploadUnsupported {
                        data: data.to_vec(),
                        total_size,
                        sha256,
                        name_hint: opts.name_hint.clone(),
                    });
                }
                return Err(BlobUploadError::ChunkTransport(
                    "put_blob chunk ack timed out".to_string(),
                ));
            }
            Err(BlobChunkFailure::Transport(e)) => {
                return Err(BlobUploadError::ChunkTransport(e));
            }
            Err(BlobChunkFailure::Protocol(e)) => return Err(classify_upload_failure(&e)),
        };
        upload_id = Some(ret.upload_id.clone());
        if eof {
            return finalize_upload(ret, &sha256, total_size);
        }
        offset = end;
    }
}

// ── 内部控制流信号 / Internal control-flow signals ────────────────────────

/// 串行 `do_serial_drain` 的失败信号 / failure signal of one serial pass。
enum SerialErr {
    /// 跨块漂移 / 全量校验失败 → 从 0 重读 / drift → reread from 0。
    Drift,
    /// 不可恢复错误 → 直接抛出 / unrecoverable → propagate。
    Fatal(BlobTransferError),
}

/// 并行实现的失败信号 / failure signal of the parallel implementation。
enum ParallelErr {
    /// 不可恢复错误 → 直接抛出 / unrecoverable → propagate。
    Fatal(BlobTransferError),
    /// 可恢复（range / 漂移）→ 串行 fallback / recoverable → serial fallback。
    Fallback,
}

/// 单块拉取的协议级错误分类（并行态）/ per-chunk protocol-error classification (parallel mode)。
enum ChunkErr {
    /// 不可恢复 / unrecoverable。
    Fatal(BlobTransferError),
    /// `range` → 触发 fallback / triggers fallback。
    Range,
}

// ── 串行实现 / Serial implementations ────────────────────────────────────

async fn drain_serial_async<F, Fut>(
    call: &F,
    computer: &str,
    blob_handle: &str,
    chunk_size: u64,
    max_retries: usize,
) -> Result<(Vec<u8>, String), BlobTransferError>
where
    F: Fn(BlobChunkRequest) -> Fut,
    Fut: Future<Output = Result<GetBlobRet, ErrorPayload>>,
{
    for _ in 0..max_retries {
        match do_serial_drain_async(call, computer, blob_handle, chunk_size).await {
            Ok(v) => return Ok(v),
            Err(SerialErr::Drift) => continue,
            Err(SerialErr::Fatal(e)) => return Err(e),
        }
    }
    Err(BlobTransferError::MaxRetriesExceeded {
        retries: max_retries,
    })
}

async fn do_serial_drain_async<F, Fut>(
    call: &F,
    computer: &str,
    blob_handle: &str,
    chunk_size: u64,
) -> Result<(Vec<u8>, String), SerialErr>
where
    F: Fn(BlobChunkRequest) -> Fut,
    Fut: Future<Output = Result<GetBlobRet, ErrorPayload>>,
{
    let mut state = SerialState::new();
    loop {
        let req = chunk_request(computer, blob_handle, state.offset, chunk_size);
        let ret = call(req)
            .await
            .map_err(|e| SerialErr::Fatal(classify_fatal(&e)))?;
        if state.absorb(ret)? {
            break;
        }
    }
    state.finish()
}

fn drain_serial_sync<F>(
    call: &F,
    computer: &str,
    blob_handle: &str,
    chunk_size: u64,
    max_retries: usize,
) -> Result<(Vec<u8>, String), BlobTransferError>
where
    F: Fn(BlobChunkRequest) -> Result<GetBlobRet, ErrorPayload>,
{
    for _ in 0..max_retries {
        match do_serial_drain_sync(call, computer, blob_handle, chunk_size) {
            Ok(v) => return Ok(v),
            Err(SerialErr::Drift) => continue,
            Err(SerialErr::Fatal(e)) => return Err(e),
        }
    }
    Err(BlobTransferError::MaxRetriesExceeded {
        retries: max_retries,
    })
}

fn do_serial_drain_sync<F>(
    call: &F,
    computer: &str,
    blob_handle: &str,
    chunk_size: u64,
) -> Result<(Vec<u8>, String), SerialErr>
where
    F: Fn(BlobChunkRequest) -> Result<GetBlobRet, ErrorPayload>,
{
    let mut state = SerialState::new();
    loop {
        let req = chunk_request(computer, blob_handle, state.offset, chunk_size);
        let ret = call(req).map_err(|e| SerialErr::Fatal(classify_fatal(&e)))?;
        if state.absorb(ret)? {
            break;
        }
    }
    state.finish()
}

/// 串行累积器（async / sync 共用，消除拉取循环重复）/ serial accumulator shared by async & sync。
struct SerialState {
    offset: u64,
    acc: Vec<u8>,
    first_sha: Option<String>,
    first_size: u64,
    mime: String,
}

impl SerialState {
    fn new() -> Self {
        Self {
            offset: 0,
            acc: Vec::new(),
            first_sha: None,
            first_size: 0,
            mime: String::new(),
        }
    }

    /// 吸收一块；返回 `true` 表示 `eof`（应停止）/ absorb a chunk; `Ok(true)` means stop at eof。
    fn absorb(&mut self, ret: GetBlobRet) -> Result<bool, SerialErr> {
        match &self.first_sha {
            None => {
                self.first_sha = Some(ret.sha256.clone());
                self.first_size = ret.total_size;
                self.mime = ret.mime_type.clone().unwrap_or_default();
            }
            Some(first) => {
                if &ret.sha256 != first || ret.total_size != self.first_size {
                    return Err(SerialErr::Drift);
                }
            }
        }
        let decoded =
            b64_decode(&ret.blob).map_err(|e| SerialErr::Fatal(BlobTransferError::Decode(e)))?;
        self.offset = ret.chunk_offset + decoded.len() as u64;
        self.acc.extend_from_slice(&decoded);
        Ok(ret.eof)
    }

    /// 全量 sha256 自证后返回 / verify full sha256, then return。
    fn finish(self) -> Result<(Vec<u8>, String), SerialErr> {
        // 不变式：循环至少跑一次（首块即设 first_sha）/ invariant: loop ran at least once.
        let expected = self.first_sha.ok_or(SerialErr::Drift)?;
        if sha256_hex(&self.acc) != expected {
            return Err(SerialErr::Drift);
        }
        Ok((self.acc, self.mime))
    }
}

// ── 并行实现 / Parallel implementations ──────────────────────────────────

async fn drain_parallel_async<F, Fut>(
    call: &F,
    computer: &str,
    blob_handle: &str,
    chunk_size: u64,
    concurrency: usize,
) -> Result<(Vec<u8>, String), ParallelErr>
where
    F: Fn(BlobChunkRequest) -> Fut,
    Fut: Future<Output = Result<GetBlobRet, ErrorPayload>>,
{
    // 步骤 1 / Step 1：首块串行获知 total_size / sha256 / mime。
    let first_req = chunk_request(computer, blob_handle, 0, chunk_size);
    let first = call(first_req)
        .await
        .map_err(|e| ParallelErr::Fatal(classify_fatal(&e)))?;
    let head = ParallelHead::from_first(first)?;
    if let Some(done) = head.single_chunk_result() {
        return done;
    }

    // 步骤 2 / Step 2：计算剩余 offset 集合。
    let offsets = head.remaining_offsets(chunk_size);

    // 步骤 3 / Step 3：bounded-concurrency 拉取；首个 fatal 即 break（drop stream → 取消在飞）。
    let mut chunks: HashMap<u64, Vec<u8>> = HashMap::new();
    let mut fatal: Option<BlobTransferError> = None;
    let mut recoverable = false;
    {
        let mut stream = stream::iter(offsets.into_iter().map(|off| {
            let req = chunk_request(computer, blob_handle, off, chunk_size);
            let fut = call(req);
            async move { (off, fut.await) }
        }))
        .buffer_unordered(concurrency);

        while let Some((off, res)) = stream.next().await {
            match head.absorb_parallel(off, res) {
                Ok(Some((off, bytes))) => {
                    chunks.insert(off, bytes);
                }
                Ok(None) => recoverable = true,
                Err(ChunkErr::Range) => recoverable = true,
                Err(ChunkErr::Fatal(e)) => {
                    fatal = Some(e);
                    break;
                }
            }
        }
    }

    // 步骤 4 / Step 4：优先级分派——fatal > recoverable（fatal 必须先于可恢复信号）。
    if let Some(e) = fatal {
        return Err(ParallelErr::Fatal(e));
    }
    if recoverable {
        return Err(ParallelErr::Fallback);
    }

    // 步骤 5 / Step 5：按 offset 重组 + 全量 sha256 自证。
    head.reassemble(chunks)
}

fn drain_parallel_sync<F>(
    call: &F,
    computer: &str,
    blob_handle: &str,
    chunk_size: u64,
    concurrency: usize,
) -> Result<(Vec<u8>, String), ParallelErr>
where
    F: Fn(BlobChunkRequest) -> Result<GetBlobRet, ErrorPayload> + Sync,
{
    // 步骤 1
    let first_req = chunk_request(computer, blob_handle, 0, chunk_size);
    let first = call(first_req).map_err(|e| ParallelErr::Fatal(classify_fatal(&e)))?;
    let head = ParallelHead::from_first(first)?;
    if let Some(done) = head.single_chunk_result() {
        return done;
    }

    let offsets = head.remaining_offsets(chunk_size);

    // 步骤 2-3：ThreadPoolExecutor 等价——固定 worker 池 + 原子游标领取 offset；首个错误置 stop。
    let next = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let recoverable = AtomicBool::new(false);
    let results: Mutex<HashMap<u64, Vec<u8>>> = Mutex::new(HashMap::new());
    let fatal: Mutex<Option<BlobTransferError>> = Mutex::new(None);
    let workers = concurrency.min(offsets.len()).max(1);

    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let idx = next.fetch_add(1, Ordering::Relaxed);
                let Some(&off) = offsets.get(idx) else { break };
                let req = chunk_request(computer, blob_handle, off, chunk_size);
                match head.absorb_parallel(off, call(req)) {
                    Ok(Some((off, bytes))) => {
                        results.lock().unwrap().insert(off, bytes);
                    }
                    // recoverable（漂移 / range）**不**早停：继续领取剩余 offset，确保并存的
                    // fatal 必被发现（永不隐藏 fatal）——与 async 路径一致。
                    // Recoverable does NOT stop early: keep draining so a co-occurring fatal is
                    // always discovered (never hide a fatal) — matching the async path.
                    Ok(None) => {
                        recoverable.store(true, Ordering::Relaxed);
                    }
                    Err(ChunkErr::Range) => {
                        recoverable.store(true, Ordering::Relaxed);
                    }
                    // 仅 fatal 提前取消在飞任务（best-effort：已运行的 call 无法中断）。
                    // Only a fatal cancels in-flight work (best-effort; a running call can't be killed).
                    Err(ChunkErr::Fatal(e)) => {
                        let mut guard = fatal.lock().unwrap();
                        if guard.is_none() {
                            *guard = Some(e);
                        }
                        stop.store(true, Ordering::Relaxed);
                        break;
                    }
                }
            });
        }
    });

    // 优先级分派：fatal 永远优先（即便有 recoverable 并存）。
    if let Some(e) = fatal.into_inner().unwrap() {
        return Err(ParallelErr::Fatal(e));
    }
    if recoverable.load(Ordering::Relaxed) {
        return Err(ParallelErr::Fallback);
    }
    head.reassemble(results.into_inner().unwrap())
}

/// 并行模式的首块快照 + 校验/重组逻辑 / first-chunk snapshot + validation/reassembly for parallel mode。
struct ParallelHead {
    total_size: u64,
    expected_sha: String,
    mime: String,
    first_bytes: Vec<u8>,
    first_len: u64,
}

impl ParallelHead {
    /// 从首块响应构造（解码 + 记录 total/sha/mime）/ build from the first chunk response。
    fn from_first(first: GetBlobRet) -> Result<Self, ParallelErr> {
        let first_bytes = b64_decode(&first.blob)
            .map_err(|e| ParallelErr::Fatal(BlobTransferError::Decode(e)))?;
        let first_len = first_bytes.len() as u64;
        Ok(Self {
            total_size: first.total_size,
            expected_sha: first.sha256,
            mime: first.mime_type.unwrap_or_default(),
            first_bytes,
            first_len,
            // `first.eof` 经 single_chunk_result 间接消费（first_len/total_size 即可判定单块）。
        })
    }

    /// 单块即完成（`eof` 或 `total_size == 0`）时返回 `Some(result)`，否则 `None` 继续并行。
    fn single_chunk_result(&self) -> Option<Result<(Vec<u8>, String), ParallelErr>> {
        if self.total_size == 0 || self.first_len >= self.total_size {
            if sha256_hex(&self.first_bytes) != self.expected_sha {
                return Some(Err(ParallelErr::Fallback));
            }
            return Some(Ok((self.first_bytes.clone(), self.mime.clone())));
        }
        None
    }

    /// 剩余块的 offset 集合（从 first_len 起步进 chunk_size）/ remaining chunk offsets。
    fn remaining_offsets(&self, chunk_size: u64) -> Vec<u64> {
        (self.first_len..self.total_size)
            .step_by(chunk_size as usize)
            .collect()
    }

    /// 校验并解码一块并行结果 / validate + decode one parallel chunk outcome。
    ///
    /// 返回 `Ok(Some((off, bytes)))` 成功；`Ok(None)` 表示漂移（recoverable）；`Err` 分类协议错误。
    fn absorb_parallel(
        &self,
        off: u64,
        res: Result<GetBlobRet, ErrorPayload>,
    ) -> Result<Option<(u64, Vec<u8>)>, ChunkErr> {
        match res {
            Err(e) => Err(classify_chunk(&e)),
            Ok(ret) => {
                if ret.sha256 != self.expected_sha || ret.total_size != self.total_size {
                    return Ok(None); // 漂移 / drift → recoverable
                }
                let bytes = b64_decode(&ret.blob)
                    .map_err(|e| ChunkErr::Fatal(BlobTransferError::Decode(e)))?;
                Ok(Some((off, bytes)))
            }
        }
    }

    /// 合并首块 + 并行块，按 offset 排序重组并校验全量 sha256 / reassemble & verify。
    fn reassemble(
        &self,
        mut chunks: HashMap<u64, Vec<u8>>,
    ) -> Result<(Vec<u8>, String), ParallelErr> {
        chunks.insert(0, self.first_bytes.clone());
        let mut keys: Vec<u64> = chunks.keys().copied().collect();
        keys.sort_unstable();
        let mut full = Vec::with_capacity(self.total_size as usize);
        for k in keys {
            full.extend_from_slice(&chunks[&k]);
        }
        if sha256_hex(&full) != self.expected_sha {
            return Err(ParallelErr::Fallback);
        }
        Ok((full, self.mime.clone()))
    }
}

// ── 通用辅助 / Common helpers ────────────────────────────────────────────

/// `0` → [`DEFAULT_CHUNK_SIZE`]（对标 Python `chunk_size or DEFAULT_CHUNK_SIZE`）。
fn effective_chunk(chunk_size: u64) -> u64 {
    if chunk_size == 0 {
        DEFAULT_CHUNK_SIZE
    } else {
        chunk_size
    }
}

fn chunk_request(computer: &str, blob_handle: &str, offset: u64, max: u64) -> BlobChunkRequest {
    BlobChunkRequest {
        computer: computer.to_owned(),
        blob_handle: blob_handle.to_owned(),
        chunk_offset: offset,
        max_chunk_bytes: max,
    }
}

/// 分类协议错误为 fatal（串行路径：`range` 也是 fatal）/ classify as fatal (serial: range is fatal too)。
fn classify_fatal(err: &ErrorPayload) -> BlobTransferError {
    if err.code == i64::from(ErrorCode::BlobNotAccessible.code()) {
        BlobTransferError::NotAccessible {
            reason: extract_reason(err),
            message: err.message.clone(),
        }
    } else {
        BlobTransferError::Protocol {
            code: err.code,
            message: err.message.clone(),
        }
    }
}

/// 分类协议错误（并行路径：`range` 单列为可恢复）/ classify (parallel: range singled out as recoverable)。
fn classify_chunk(err: &ErrorPayload) -> ChunkErr {
    if err.code == i64::from(ErrorCode::BlobNotAccessible.code()) {
        let reason = extract_reason(err);
        if matches!(reason, BlobErrorReason::Range) {
            return ChunkErr::Range;
        }
        ChunkErr::Fatal(BlobTransferError::NotAccessible {
            reason,
            message: err.message.clone(),
        })
    } else {
        ChunkErr::Fatal(BlobTransferError::Protocol {
            code: err.code,
            message: err.message.clone(),
        })
    }
}

/// 从 4018 ErrorPayload 提取 `details.reason`（缺省 `invalid_handle`）/ extract 4018 reason。
fn extract_reason(err: &ErrorPayload) -> BlobErrorReason {
    let raw = err
        .details
        .as_ref()
        .and_then(|d| d.get("reason"))
        .and_then(|v| v.as_str())
        .unwrap_or("invalid_handle");
    BlobErrorReason::parse(raw)
}

/// 能力门控（协议 §3）：自身 `PROTOCOL_VERSION` minor ≥ 0.4 才可发起上行 / capability gate。
///
/// 版本握手 MINOR 严格匹配 + 同房间传递 ⇒ 连上即保证房间内 Computer 同 minor。本检查是编译期
/// 常量的运行时断言（常量回退时 fail-fast）；首块超时兜底见 [`BlobUploadError::UploadUnsupported`]。
fn ensure_upload_supported(version: &str) -> Result<(), BlobUploadError> {
    let minor = crate::ProtocolVersion::parse(version).map(|v| v.minor).unwrap_or(0);
    if minor >= 4 {
        Ok(())
    } else {
        Err(BlobUploadError::UnsupportedBySdk(version.to_string()))
    }
}

/// 分类单块失败为 [`BlobUploadError`]：4019 → [`BlobUploadError::WriteFailed`]，其它协议码透传。
fn classify_upload_failure(err: &ErrorPayload) -> BlobUploadError {
    if err.code == i64::from(ErrorCode::BlobWriteFailed.code()) {
        BlobUploadError::WriteFailed {
            reason: extract_write_reason(err),
            message: err.message.clone(),
        }
    } else {
        BlobUploadError::Protocol {
            code: err.code,
            message: err.message.clone(),
        }
    }
}

/// 从 4019 ErrorPayload 提取 `details.reason`（缺省 `invalid_upload`）/ extract 4019 reason。
fn extract_write_reason(err: &ErrorPayload) -> BlobWriteErrorReason {
    let raw = err
        .details
        .as_ref()
        .and_then(|d| d.get("reason"))
        .and_then(|v| v.as_str())
        .unwrap_or("invalid_upload");
    BlobWriteErrorReason::parse(raw)
}

/// 末块 ack 收口：取 `landing_path`，回显 sha256 与声明比对（协议 SHOULD）。
///
/// 对标 Python `_finalize_result`：`landing_path` 缺失/空 → [`BlobUploadError::IncompleteAck`]；
/// 回显 sha 非空且 != 声明 → [`BlobUploadError::EchoMismatch`]；`total_size` 缺省回退声明值。
fn finalize_upload(
    ret: PutBlobRet,
    declared_sha: &str,
    declared_total: u64,
) -> Result<PutBlobResult, BlobUploadError> {
    let landing_path = ret.landing_path.as_deref().filter(|s| !s.is_empty()).ok_or(
        BlobUploadError::IncompleteAck,
    )?;
    let echo_sha = ret.sha256.as_deref().unwrap_or("");
    if !echo_sha.is_empty() && echo_sha != declared_sha {
        return Err(BlobUploadError::EchoMismatch {
            echo: echo_sha.to_string(),
            declared: declared_sha.to_string(),
        });
    }
    Ok(PutBlobResult {
        landing_path: landing_path.to_string(),
        total_size: ret.total_size.unwrap_or(declared_total),
        sha256: if echo_sha.is_empty() {
            declared_sha.to_string()
        } else {
            echo_sha.to_string()
        },
    })
}

/// base64 标准编码解码 / standard base64 decode。
fn b64_decode(s: &str) -> Result<Vec<u8>, String> {
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
// 测试 `call` 闭包返回协议原生 ack 类型 `Result<GetBlobRet, ErrorPayload>`；`ErrorPayload` 是
// 宽 flat 协议结构（~160B），large_err 在此为可接受取舍（保真优先于装箱）。
#[allow(clippy::result_large_err)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn b64_encode(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    /// 构造一块成功响应（按 [offset, offset+max) 切片）/ build one successful chunk response。
    fn serve(data: &[u8], sha: &str, offset: u64, max: u64) -> GetBlobRet {
        let start = (offset as usize).min(data.len());
        let end = (start + max as usize).min(data.len());
        let chunk = &data[start..end];
        GetBlobRet {
            blob_handle: "h".to_string(),
            mime_type: Some("application/octet-stream".to_string()),
            total_size: data.len() as u64,
            sha256: sha.to_string(),
            chunk_offset: offset,
            eof: end >= data.len(),
            blob: b64_encode(chunk),
            req_id: None,
        }
    }

    fn err_4018(reason: &str) -> ErrorPayload {
        ErrorPayload::new(
            i64::from(ErrorCode::BlobNotAccessible.code()),
            "blob not accessible",
        )
        .with_detail("reason", reason)
    }

    fn sample(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    // ── 串行 async / serial async ──────────────────────────────────────

    #[tokio::test]
    async fn serial_async_multi_chunk_reassembles() {
        let data = Arc::new(sample(1000));
        let sha = sha256_hex(&data);
        let call = {
            let data = data.clone();
            let sha = sha.clone();
            move |req: BlobChunkRequest| {
                let data = data.clone();
                let sha = sha.clone();
                async move {
                    Ok::<_, ErrorPayload>(serve(&data, &sha, req.chunk_offset, req.max_chunk_bytes))
                }
            }
        };
        let opts = DrainBlobOptions {
            concurrency: 1,
            chunk_size: 128,
            max_retries: 3,
        };
        let (bytes, mime) = drain_blob(call, "c", "h", opts).await.unwrap();
        assert_eq!(bytes, *data);
        assert_eq!(mime, "application/octet-stream");
    }

    #[tokio::test]
    async fn serial_async_empty_blob() {
        let data: Vec<u8> = Vec::new();
        let sha = sha256_hex(&data);
        let call = move |req: BlobChunkRequest| {
            let sha = sha.clone();
            async move { Ok::<_, ErrorPayload>(serve(&[], &sha, req.chunk_offset, req.max_chunk_bytes)) }
        };
        let (bytes, _mime) = drain_blob(call, "c", "h", DrainBlobOptions::default())
            .await
            .unwrap();
        assert!(bytes.is_empty());
    }

    #[tokio::test]
    async fn serial_async_sha_mismatch_exhausts_retries() {
        let data = Arc::new(sample(300));
        // 始终返回错误 sha → 每趟全量校验失败 → Drift → 重读 → 耗尽 max_retries。
        let bogus = "0".repeat(64);
        let call = {
            let data = data.clone();
            move |req: BlobChunkRequest| {
                let data = data.clone();
                let bogus = bogus.clone();
                async move {
                    Ok::<_, ErrorPayload>(serve(
                        &data,
                        &bogus,
                        req.chunk_offset,
                        req.max_chunk_bytes,
                    ))
                }
            }
        };
        let opts = DrainBlobOptions {
            concurrency: 1,
            chunk_size: 128,
            max_retries: 2,
        };
        let err = drain_blob(call, "c", "h", opts).await.unwrap_err();
        assert_eq!(err, BlobTransferError::MaxRetriesExceeded { retries: 2 });
    }

    #[tokio::test]
    async fn serial_async_total_size_drift_then_recovers() {
        // 首趟：前两块用旧 sha，第三块起源被改写（新 sha/size）→ 漂移 → 重读；
        // 重读趟：源已稳定（新内容）→ 成功。
        let old = Arc::new(sample(400));
        let new = Arc::new(sample(640));
        let old_sha = sha256_hex(&old);
        let new_sha = sha256_hex(&new);
        let started_reread = Arc::new(AtomicBool::new(false));
        let seen_offsets = Arc::new(Mutex::new(Vec::<u64>::new()));
        let call = {
            let old = old.clone();
            let new = new.clone();
            let old_sha = old_sha.clone();
            let new_sha = new_sha.clone();
            let started_reread = started_reread.clone();
            let seen_offsets = seen_offsets.clone();
            move |req: BlobChunkRequest| {
                let old = old.clone();
                let new = new.clone();
                let old_sha = old_sha.clone();
                let new_sha = new_sha.clone();
                let started_reread = started_reread.clone();
                let seen_offsets = seen_offsets.clone();
                async move {
                    // 第二次回到 offset 0 = 进入重读趟。
                    if req.chunk_offset == 0 && seen_offsets.lock().unwrap().contains(&0) {
                        started_reread.store(true, Ordering::SeqCst);
                    }
                    seen_offsets.lock().unwrap().push(req.chunk_offset);
                    if started_reread.load(Ordering::SeqCst) {
                        Ok::<_, ErrorPayload>(serve(
                            &new,
                            &new_sha,
                            req.chunk_offset,
                            req.max_chunk_bytes,
                        ))
                    } else if req.chunk_offset < 256 {
                        Ok(serve(&old, &old_sha, req.chunk_offset, req.max_chunk_bytes))
                    } else {
                        // 源在传输中被改写：返回新 sha/size → 触发漂移。
                        Ok(serve(&new, &new_sha, req.chunk_offset, req.max_chunk_bytes))
                    }
                }
            }
        };
        let opts = DrainBlobOptions {
            concurrency: 1,
            chunk_size: 128,
            max_retries: 3,
        };
        let (bytes, _mime) = drain_blob(call, "c", "h", opts).await.unwrap();
        assert!(started_reread.load(Ordering::SeqCst), "应触发从 0 重读");
        assert_eq!(bytes, *new);
    }

    #[tokio::test]
    async fn serial_async_4018_invalid_handle_is_fatal() {
        let call =
            |_req: BlobChunkRequest| async { Err::<GetBlobRet, _>(err_4018("invalid_handle")) };
        let err = drain_blob(call, "c", "h", DrainBlobOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            BlobTransferError::NotAccessible {
                reason: BlobErrorReason::InvalidHandle,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn serial_async_4018_range_is_fatal_in_serial() {
        let call = |_req: BlobChunkRequest| async { Err::<GetBlobRet, _>(err_4018("range")) };
        let err = drain_blob(call, "c", "h", DrainBlobOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            BlobTransferError::NotAccessible {
                reason: BlobErrorReason::Range,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn serial_async_other_protocol_code_surfaces() {
        let call = |_req: BlobChunkRequest| async {
            Err::<GetBlobRet, _>(ErrorPayload::new(4014, "nope"))
        };
        let err = drain_blob(call, "c", "h", DrainBlobOptions::default())
            .await
            .unwrap_err();
        assert_eq!(
            err,
            BlobTransferError::Protocol {
                code: 4014,
                message: "nope".to_string()
            }
        );
    }

    // ── 并行 async / parallel async ────────────────────────────────────

    #[tokio::test]
    async fn parallel_async_multi_chunk_reassembles() {
        let data = Arc::new(sample(5000));
        let sha = sha256_hex(&data);
        let call = {
            let data = data.clone();
            let sha = sha.clone();
            move |req: BlobChunkRequest| {
                let data = data.clone();
                let sha = sha.clone();
                async move {
                    Ok::<_, ErrorPayload>(serve(&data, &sha, req.chunk_offset, req.max_chunk_bytes))
                }
            }
        };
        let opts = DrainBlobOptions {
            concurrency: 4,
            chunk_size: 256,
            max_retries: 3,
        };
        let (bytes, _mime) = drain_blob(call, "c", "h", opts).await.unwrap();
        assert_eq!(bytes, *data);
    }

    #[tokio::test]
    async fn parallel_async_range_falls_back_to_serial() {
        // 并行态：非零 offset 首次遇 range → 回退串行；串行从 0 顺序拉取（offset 单调）→ 成功。
        let data = Arc::new(sample(2000));
        let sha = sha256_hex(&data);
        let parallel_seen = Arc::new(AtomicBool::new(false));
        let call = {
            let data = data.clone();
            let sha = sha.clone();
            let parallel_seen = parallel_seen.clone();
            move |req: BlobChunkRequest| {
                let data = data.clone();
                let sha = sha.clone();
                let parallel_seen = parallel_seen.clone();
                async move {
                    // 串行从 0 单调推进；并行会乱序/跳跃请求非零 offset。
                    // 第一次出现「offset>0 且尚未串行化」→ 注入一次 range。
                    if req.chunk_offset > 0 && !parallel_seen.load(Ordering::SeqCst) {
                        parallel_seen.store(true, Ordering::SeqCst);
                        return Err::<GetBlobRet, _>(err_4018("range"));
                    }
                    Ok(serve(&data, &sha, req.chunk_offset, req.max_chunk_bytes))
                }
            }
        };
        let opts = DrainBlobOptions {
            concurrency: 4,
            chunk_size: 256,
            max_retries: 3,
        };
        let (bytes, _mime) = drain_blob(call, "c", "h", opts).await.unwrap();
        assert!(parallel_seen.load(Ordering::SeqCst), "应曾触发并行 range");
        assert_eq!(bytes, *data);
    }

    #[tokio::test]
    async fn parallel_async_invalid_handle_is_fatal_no_fallback() {
        let data = Arc::new(sample(2000));
        let sha = sha256_hex(&data);
        let call = {
            let data = data.clone();
            let sha = sha.clone();
            move |req: BlobChunkRequest| {
                let data = data.clone();
                let sha = sha.clone();
                async move {
                    if req.chunk_offset > 0 {
                        return Err::<GetBlobRet, _>(err_4018("forbidden"));
                    }
                    Ok(serve(&data, &sha, req.chunk_offset, req.max_chunk_bytes))
                }
            }
        };
        let opts = DrainBlobOptions {
            concurrency: 4,
            chunk_size: 256,
            max_retries: 3,
        };
        let err = drain_blob(call, "c", "h", opts).await.unwrap_err();
        assert!(matches!(
            err,
            BlobTransferError::NotAccessible {
                reason: BlobErrorReason::Forbidden,
                ..
            }
        ));
    }

    // ── 同步 / sync ────────────────────────────────────────────────────

    #[test]
    fn serial_sync_multi_chunk_reassembles() {
        let data = sample(1000);
        let sha = sha256_hex(&data);
        let call = |req: BlobChunkRequest| {
            Ok::<_, ErrorPayload>(serve(&data, &sha, req.chunk_offset, req.max_chunk_bytes))
        };
        let opts = DrainBlobOptions {
            concurrency: 1,
            chunk_size: 128,
            max_retries: 3,
        };
        let (bytes, _mime) = drain_blob_sync(call, "c", "h", opts).unwrap();
        assert_eq!(bytes, data);
    }

    #[test]
    fn parallel_sync_multi_chunk_reassembles() {
        let data = sample(5000);
        let sha = sha256_hex(&data);
        let call = |req: BlobChunkRequest| {
            Ok::<_, ErrorPayload>(serve(&data, &sha, req.chunk_offset, req.max_chunk_bytes))
        };
        let opts = DrainBlobOptions {
            concurrency: 4,
            chunk_size: 256,
            max_retries: 3,
        };
        let (bytes, _mime) = drain_blob_sync(call, "c", "h", opts).unwrap();
        assert_eq!(bytes, data);
    }

    #[test]
    fn parallel_sync_range_falls_back_to_serial() {
        let data = sample(2000);
        let sha = sha256_hex(&data);
        let parallel_seen = AtomicBool::new(false);
        let call = |req: BlobChunkRequest| {
            if req.chunk_offset > 0 && !parallel_seen.load(Ordering::SeqCst) {
                parallel_seen.store(true, Ordering::SeqCst);
                return Err::<GetBlobRet, _>(err_4018("range"));
            }
            Ok(serve(&data, &sha, req.chunk_offset, req.max_chunk_bytes))
        };
        let opts = DrainBlobOptions {
            concurrency: 4,
            chunk_size: 256,
            max_retries: 3,
        };
        let (bytes, _mime) = drain_blob_sync(call, "c", "h", opts).unwrap();
        assert!(parallel_seen.load(Ordering::SeqCst));
        assert_eq!(bytes, data);
    }

    #[test]
    fn sync_4018_gone_is_fatal() {
        let call = |_req: BlobChunkRequest| Err::<GetBlobRet, _>(err_4018("gone"));
        let err = drain_blob_sync(call, "c", "h", DrainBlobOptions::default()).unwrap_err();
        assert!(matches!(
            err,
            BlobTransferError::NotAccessible {
                reason: BlobErrorReason::Gone,
                ..
            }
        ));
    }

    #[test]
    fn reason_parse_round_trip() {
        for r in ["invalid_handle", "forbidden", "gone", "range"] {
            assert_eq!(BlobErrorReason::parse(r).as_str(), r);
        }
        assert_eq!(
            BlobErrorReason::parse("future_reason"),
            BlobErrorReason::Other("future_reason".to_string())
        );
    }

    // ── fix-review 跟进：并发关键分支 + 边界 ───────────────────────────

    /// 并行态：某非零 offset 块返回**漂移**的 sha（成功响应但 sha≠expected）→ absorb_parallel
    /// `Ok(None)` → recoverable → 回退串行（源已稳定）→ 成功。覆盖 finding 1a。
    #[tokio::test]
    async fn parallel_async_drift_falls_back_to_serial() {
        let data = Arc::new(sample(2000));
        let sha = sha256_hex(&data);
        let wrong = "0".repeat(64);
        let injected = Arc::new(AtomicBool::new(false));
        let call = {
            let data = data.clone();
            let sha = sha.clone();
            let wrong = wrong.clone();
            let injected = injected.clone();
            move |req: BlobChunkRequest| {
                let data = data.clone();
                let sha = sha.clone();
                let wrong = wrong.clone();
                let injected = injected.clone();
                async move {
                    // 首个非零 offset 注入一次漂移（合法字节 + 错误 sha）；之后（含串行回退）正常。
                    let use_sha = if req.chunk_offset > 0 && !injected.swap(true, Ordering::SeqCst)
                    {
                        &wrong
                    } else {
                        &sha
                    };
                    Ok::<_, ErrorPayload>(serve(
                        &data,
                        use_sha,
                        req.chunk_offset,
                        req.max_chunk_bytes,
                    ))
                }
            }
        };
        let opts = DrainBlobOptions {
            concurrency: 4,
            chunk_size: 256,
            max_retries: 3,
        };
        let (bytes, _mime) = drain_blob(call, "c", "h", opts).await.unwrap();
        assert!(injected.load(Ordering::SeqCst), "应曾注入并行漂移");
        assert_eq!(bytes, *data);
    }

    /// 并行态：range（低 offset，recoverable）与 forbidden（高 offset，fatal）并存 → **fatal 胜出**，
    /// 返回 NotAccessible{Forbidden} 而非回退串行（回退会得到 serial 态 range fatal）。覆盖 finding 1d (async)。
    #[tokio::test]
    async fn parallel_async_fatal_beats_recoverable() {
        let data = Arc::new(sample(2000));
        let sha = sha256_hex(&data);
        let call = {
            let data = data.clone();
            let sha = sha.clone();
            move |req: BlobChunkRequest| {
                let data = data.clone();
                let sha = sha.clone();
                async move {
                    match req.chunk_offset {
                        256 => Err::<GetBlobRet, _>(err_4018("range")),
                        512 => Err(err_4018("forbidden")),
                        off => Ok(serve(&data, &sha, off, req.max_chunk_bytes)),
                    }
                }
            }
        };
        let opts = DrainBlobOptions {
            concurrency: 4,
            chunk_size: 256,
            max_retries: 3,
        };
        let err = drain_blob(call, "c", "h", opts).await.unwrap_err();
        assert!(
            matches!(
                err,
                BlobTransferError::NotAccessible {
                    reason: BlobErrorReason::Forbidden,
                    ..
                }
            ),
            "fatal 必须胜出，得到的却是 {err:?}"
        );
    }

    /// sync 版 fatal>recoverable——改动 1（recoverable 不早停）后确定性成立。覆盖 finding 1d (sync)。
    #[test]
    fn parallel_sync_fatal_beats_recoverable() {
        let data = sample(2000);
        let sha = sha256_hex(&data);
        let call = |req: BlobChunkRequest| match req.chunk_offset {
            256 => Err::<GetBlobRet, _>(err_4018("range")),
            512 => Err(err_4018("forbidden")),
            off => Ok(serve(&data, &sha, off, req.max_chunk_bytes)),
        };
        let opts = DrainBlobOptions {
            concurrency: 4,
            chunk_size: 256,
            max_retries: 3,
        };
        let err = drain_blob_sync(call, "c", "h", opts).unwrap_err();
        assert!(
            matches!(
                err,
                BlobTransferError::NotAccessible {
                    reason: BlobErrorReason::Forbidden,
                    ..
                }
            ),
            "sync fatal 必须胜出，得到的却是 {err:?}"
        );
    }

    /// sync 版并行漂移回退串行成功（absorb_parallel `Ok(None)` 对称覆盖）。
    #[test]
    fn parallel_sync_drift_falls_back_to_serial() {
        let data = sample(2000);
        let sha = sha256_hex(&data);
        let wrong = "0".repeat(64);
        let injected = AtomicBool::new(false);
        let call = |req: BlobChunkRequest| {
            let use_sha = if req.chunk_offset > 0 && !injected.swap(true, Ordering::SeqCst) {
                &wrong
            } else {
                &sha
            };
            Ok::<_, ErrorPayload>(serve(&data, use_sha, req.chunk_offset, req.max_chunk_bytes))
        };
        let opts = DrainBlobOptions {
            concurrency: 4,
            chunk_size: 256,
            max_retries: 3,
        };
        let (bytes, _mime) = drain_blob_sync(call, "c", "h", opts).unwrap();
        assert!(injected.load(Ordering::SeqCst));
        assert_eq!(bytes, data);
    }

    /// 并行单块（data < chunk_size）首块 sha 不符 → `single_chunk_result` Fallback → 串行成功。覆盖 finding 1c。
    #[tokio::test]
    async fn parallel_async_single_chunk_sha_mismatch_falls_back() {
        let data = Arc::new(sample(100)); // < chunk_size → 单块路径
        let sha = sha256_hex(&data);
        let wrong = "0".repeat(64);
        let count = Arc::new(AtomicUsize::new(0));
        let call = {
            let data = data.clone();
            let sha = sha.clone();
            let wrong = wrong.clone();
            let count = count.clone();
            move |req: BlobChunkRequest| {
                let data = data.clone();
                let sha = sha.clone();
                let wrong = wrong.clone();
                let count = count.clone();
                async move {
                    // 首次（并行首块）报错 sha，触发 single_chunk_result Fallback；串行重读正常。
                    let use_sha = if count.fetch_add(1, Ordering::SeqCst) == 0 {
                        &wrong
                    } else {
                        &sha
                    };
                    Ok::<_, ErrorPayload>(serve(
                        &data,
                        use_sha,
                        req.chunk_offset,
                        req.max_chunk_bytes,
                    ))
                }
            }
        };
        let opts = DrainBlobOptions {
            concurrency: 4,
            chunk_size: 256,
            max_retries: 3,
        };
        let (bytes, _mime) = drain_blob(call, "c", "h", opts).await.unwrap();
        assert_eq!(bytes, *data);
    }

    /// 并行：全块一致报 bogus sha（每块 sha 自洽 → absorb 通过）但重组字节哈希不符 →
    /// `reassemble` Fallback → 串行 Drift → 耗尽 → `MaxRetriesExceeded`。覆盖 finding 1b。
    #[tokio::test]
    async fn parallel_async_reassemble_sha_mismatch_exhausts() {
        let data = Arc::new(sample(2000));
        let bogus = "0".repeat(64);
        let call = {
            let data = data.clone();
            let bogus = bogus.clone();
            move |req: BlobChunkRequest| {
                let data = data.clone();
                let bogus = bogus.clone();
                async move {
                    Ok::<_, ErrorPayload>(serve(
                        &data,
                        &bogus,
                        req.chunk_offset,
                        req.max_chunk_bytes,
                    ))
                }
            }
        };
        let opts = DrainBlobOptions {
            concurrency: 4,
            chunk_size: 256,
            max_retries: 2,
        };
        let err = drain_blob(call, "c", "h", opts).await.unwrap_err();
        assert_eq!(err, BlobTransferError::MaxRetriesExceeded { retries: 2 });
    }

    /// 分块响应 base64 非法 → `BlobTransferError::Decode`（fatal）。覆盖 finding 2。
    #[tokio::test]
    async fn serial_async_decode_error() {
        let call = |_req: BlobChunkRequest| async {
            Ok::<_, ErrorPayload>(GetBlobRet {
                blob_handle: "h".to_string(),
                mime_type: Some("application/octet-stream".to_string()),
                total_size: 8,
                sha256: "deadbeef".to_string(),
                chunk_offset: 0,
                eof: true,
                blob: "@@@not-base64@@@".to_string(),
                req_id: None,
            })
        };
        let err = drain_blob(call, "c", "h", DrainBlobOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(err, BlobTransferError::Decode(_)), "got {err:?}");
    }

    /// sync 串行重读耗尽 → `MaxRetriesExceeded`（对称 async 既有用例）。覆盖 finding 3。
    #[test]
    fn serial_sync_sha_mismatch_exhausts_retries() {
        let data = sample(300);
        let bogus = "0".repeat(64);
        let call = |req: BlobChunkRequest| {
            Ok::<_, ErrorPayload>(serve(&data, &bogus, req.chunk_offset, req.max_chunk_bytes))
        };
        let opts = DrainBlobOptions {
            concurrency: 1,
            chunk_size: 128,
            max_retries: 2,
        };
        let err = drain_blob_sync(call, "c", "h", opts).unwrap_err();
        assert_eq!(err, BlobTransferError::MaxRetriesExceeded { retries: 2 });
    }

    /// `max_retries == 0` 经入口 clamp 仍尝试一次 → 正常成功（证明 ≥1 加固）。覆盖 finding 4。
    #[tokio::test]
    async fn serial_async_zero_retries_still_attempts_once() {
        let data = Arc::new(sample(300));
        let sha = sha256_hex(&data);
        let call = {
            let data = data.clone();
            let sha = sha.clone();
            move |req: BlobChunkRequest| {
                let data = data.clone();
                let sha = sha.clone();
                async move {
                    Ok::<_, ErrorPayload>(serve(&data, &sha, req.chunk_offset, req.max_chunk_bytes))
                }
            }
        };
        let opts = DrainBlobOptions {
            concurrency: 1,
            chunk_size: 128,
            max_retries: 0,
        };
        let (bytes, _mime) = drain_blob(call, "c", "h", opts).await.unwrap();
        assert_eq!(bytes, *data);
    }

    // ── 上行 pump / upstream pump ──────────────────────────────────────

    /// 模拟 Computer ack：首块回 `u1`；其余回显；末块回 landing 字段（total/sha 回显缺省 →
    /// pump 回退声明值——对拍 python `_finalize_result` 的 fallback 分支）。
    fn upload_ack(req: &PutBlobChunkRequest) -> PutBlobRet {
        PutBlobRet {
            upload_id: req.upload_id.clone().unwrap_or_else(|| "u1".to_string()),
            chunk_offset: req.chunk_offset,
            landing_path: req.eof.then(|| "/landing/u1.txt".to_string()),
            total_size: None,
            sha256: None,
            req_id: None,
        }
    }

    #[tokio::test]
    async fn pump_multi_chunk_calls_sequence() {
        let data = sample(1000);
        let sha = sha256_hex(&data);
        let seen: Mutex<Vec<PutBlobChunkRequest>> = Mutex::new(Vec::new());
        let call = |req: PutBlobChunkRequest| {
            let seen = &seen;
            async move {
                seen.lock().unwrap().push(req.clone());
                Ok::<_, BlobChunkFailure>(upload_ack(&req))
            }
        };
        let opts = PumpBlobOptions {
            name_hint: Some("big.bin".to_string()),
            chunk_size: 256,
        };
        let ret = pump_blob(call, "c", &data, opts).await.unwrap();
        assert_eq!(ret.landing_path, "/landing/u1.txt");
        assert_eq!(ret.total_size, 1000);
        assert_eq!(ret.sha256, sha);

        let reqs = seen.into_inner().unwrap();
        assert_eq!(reqs.len(), 4, "1000 / 256 → 4 块");
        // eof 仅末块；chunk_offset 顺序推进；upload_id 贯穿（首块 None）；声明仅首块。
        for (i, r) in reqs.iter().enumerate() {
            assert_eq!(r.chunk_offset, (i as u64) * 256);
            assert_eq!(r.eof, i == 3);
            assert_eq!(r.upload_id, if i == 0 { None } else { Some("u1".into()) });
            assert_eq!(r.declaration.is_some(), i == 0);
            if i == 0 {
                let d = r.declaration.as_ref().unwrap();
                assert_eq!(d.name_hint.as_deref(), Some("big.bin"));
                assert_eq!(d.total_size, 1000);
                assert_eq!(d.sha256, sha);
            }
            assert_eq!(r.computer, "c");
        }
    }

    #[tokio::test]
    async fn pump_single_chunk_degenerate() {
        let data = sample(1);
        let sha = sha256_hex(&data);
        let call = |req: PutBlobChunkRequest| async move {
            assert!(req.eof, "单块即 eof");
            assert_eq!(req.declaration.as_ref().map(|d| d.total_size), Some(1));
            Ok::<_, BlobChunkFailure>(upload_ack(&req))
        };
        let ret = pump_blob(call, "c", &data, PumpBlobOptions::default())
            .await
            .unwrap();
        assert_eq!(ret.total_size, 1);
        assert_eq!(ret.sha256, sha);
    }

    #[tokio::test]
    async fn pump_empty_payload_rejected() {
        let call = |_req: PutBlobChunkRequest| async { Err::<PutBlobRet, _>(BlobChunkFailure::Timeout) };
        let err = pump_blob(call, "c", &[], PumpBlobOptions::default())
            .await
            .unwrap_err();
        assert_eq!(err, BlobUploadError::EmptyPayload);
    }

    #[tokio::test]
    async fn pump_zero_chunk_size_rejected() {
        // 显式 0 → bad_chunk_size（镜像 python `_prepare_declaration`：`chunk_size < 1` 显拒）。
        let call = |_req: PutBlobChunkRequest| async { Err::<PutBlobRet, _>(BlobChunkFailure::Timeout) };
        let err = pump_blob(
            call,
            "c",
            b"hello",
            PumpBlobOptions { chunk_size: 0, ..Default::default() },
        )
        .await
        .unwrap_err();
        assert_eq!(err, BlobUploadError::BadChunkSize { chunk_size: 0 });
        // sync 同构。
        let call_sync = |_req: PutBlobChunkRequest| Err::<PutBlobRet, _>(BlobChunkFailure::Timeout);
        let err_sync = pump_blob_sync(
            call_sync,
            "c",
            b"hello",
            PumpBlobOptions { chunk_size: 0, ..Default::default() },
        )
        .unwrap_err();
        assert_eq!(err_sync, BlobUploadError::BadChunkSize { chunk_size: 0 });
    }

    #[tokio::test]
    async fn pump_4019_busy_maps_write_failed() {
        let call = |_req: PutBlobChunkRequest| async {
            Err::<PutBlobRet, _>(BlobChunkFailure::Protocol(
                ErrorPayload::new(4019, "too many concurrent uploads").with_detail("reason", "busy"),
            ))
        };
        let err = pump_blob(call, "c", b"hello", PumpBlobOptions::default())
            .await
            .unwrap_err();
        assert_eq!(
            err,
            BlobUploadError::WriteFailed {
                reason: BlobWriteErrorReason::Busy,
                message: "too many concurrent uploads".to_string(),
            }
        );
    }

    #[tokio::test]
    async fn pump_4019_without_reason_defaults_invalid_upload() {
        let call = |_req: PutBlobChunkRequest| async {
            Err::<PutBlobRet, _>(BlobChunkFailure::Protocol(ErrorPayload::new(
                4019,
                "unknown session",
            )))
        };
        let err = pump_blob(call, "c", b"hello", PumpBlobOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            BlobUploadError::WriteFailed {
                reason: BlobWriteErrorReason::InvalidUpload,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn pump_other_protocol_code_surfaces() {
        let call = |_req: PutBlobChunkRequest| async {
            Err::<PutBlobRet, _>(BlobChunkFailure::Protocol(ErrorPayload::new(404, "nope")))
        };
        let err = pump_blob(call, "c", b"hello", PumpBlobOptions::default())
            .await
            .unwrap_err();
        assert_eq!(
            err,
            BlobUploadError::Protocol {
                code: 404,
                message: "nope".to_string()
            }
        );
    }

    #[tokio::test]
    async fn pump_missing_landing_path_is_incomplete_ack() {
        let call = |req: PutBlobChunkRequest| async move {
            // eof 但缺 landing_path。
            Ok::<_, BlobChunkFailure>(PutBlobRet {
                upload_id: "u1".into(),
                chunk_offset: req.chunk_offset,
                landing_path: None,
                total_size: None,
                sha256: None,
                req_id: None,
            })
        };
        let err = pump_blob(call, "c", b"hello", PumpBlobOptions::default())
            .await
            .unwrap_err();
        assert_eq!(err, BlobUploadError::IncompleteAck);
    }

    #[tokio::test]
    async fn pump_echo_sha_mismatch() {
        let call = |req: PutBlobChunkRequest| async move {
            let decl = req.declaration.clone().unwrap();
            Ok::<_, BlobChunkFailure>(PutBlobRet {
                upload_id: "u1".into(),
                chunk_offset: req.chunk_offset,
                landing_path: Some("/landing/u1.txt".into()),
                total_size: Some(decl.total_size),
                sha256: Some("0".repeat(64)),
                req_id: None,
            })
        };
        let err = pump_blob(call, "c", b"hello", PumpBlobOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(err, BlobUploadError::EchoMismatch { .. }));
    }

    #[tokio::test]
    async fn pump_first_chunk_timeout_is_unsupported_and_carries_data() {
        let call = |_req: PutBlobChunkRequest| async { Err::<PutBlobRet, _>(BlobChunkFailure::Timeout) };
        let err = pump_blob(call, "c", b"hello", PumpBlobOptions::default())
            .await
            .unwrap_err();
        match err {
            BlobUploadError::UploadUnsupported { data, total_size, .. } => {
                assert_eq!(data, b"hello");
                assert_eq!(total_size, 5);
            }
            other => panic!("expected UploadUnsupported, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pump_second_chunk_timeout_is_chunk_transport() {
        // 首块成功，后续块超时 → 传输故障（非能力缺失）。
        let first = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let call = move |req: PutBlobChunkRequest| {
            let first = first.clone();
            async move {
                if req.upload_id.is_none() {
                    first.store(true, Ordering::SeqCst);
                    Ok::<_, BlobChunkFailure>(upload_ack(&req))
                } else {
                    Err(BlobChunkFailure::Timeout)
                }
            }
        };
        let err = pump_blob(call, "c", b"hello", PumpBlobOptions { chunk_size: 2, ..Default::default() })
            .await
            .unwrap_err();
        assert!(matches!(err, BlobUploadError::ChunkTransport(_)), "got {err:?}");
    }

    #[test]
    fn pump_sync_mirror_multi_chunk() {
        let data = sample(1000);
        let sha = sha256_hex(&data);
        let seen = Arc::new(Mutex::new(Vec::<PutBlobChunkRequest>::new()));
        let call = {
            let seen = seen.clone();
            move |req: PutBlobChunkRequest| {
                seen.lock().unwrap().push(req.clone());
                Ok::<_, BlobChunkFailure>(upload_ack(&req))
            }
        };
        let opts = PumpBlobOptions {
            name_hint: Some("s.bin".into()),
            chunk_size: 256,
        };
        let ret = pump_blob_sync(call, "c", &data, opts).unwrap();
        assert_eq!(ret.landing_path, "/landing/u1.txt");
        assert_eq!(ret.total_size, 1000);
        assert_eq!(ret.sha256, sha);
        assert_eq!(seen.lock().unwrap().len(), 4);
    }

    #[test]
    fn ensure_upload_supported_gates_on_minor() {
        assert!(ensure_upload_supported("0.4.0").is_ok());
        assert!(ensure_upload_supported("0.4.5").is_ok());
        let err = ensure_upload_supported("0.3.9").unwrap_err();
        assert!(matches!(err, BlobUploadError::UnsupportedBySdk(_)));
    }

    #[test]
    fn write_reason_parse_round_trip() {
        for r in [
            "invalid_upload", "invalid_declaration", "range", "too_large", "busy",
            "forbidden", "integrity", "io_error",
        ] {
            assert_eq!(BlobWriteErrorReason::parse(r).as_str(), r);
        }
        assert_eq!(
            BlobWriteErrorReason::parse("future_write_reason"),
            BlobWriteErrorReason::Other("future_write_reason".to_string())
        );
    }
}
