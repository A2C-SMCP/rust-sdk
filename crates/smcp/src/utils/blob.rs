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
//! async 与 sync 差异（与 Python 一致）/ async-vs-sync nuance (matches Python)：async 并行收集所有
//! 已发生异常后按「fatal > recoverable」优先级分派；sync 并行遇首个错误即停（fatal 仍优先于
//! recoverable，永不隐藏 fatal）。

use std::collections::HashMap;
use std::fmt::Write as _;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

use base64::Engine as _;
use futures::stream::{self, StreamExt as _};
use sha2::{Digest, Sha256};

use crate::{ErrorCode, ErrorPayload, GetBlobRet};

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
    if opts.concurrency <= 1 {
        return drain_serial_async(&call, computer, blob_handle, chunk_size, opts.max_retries)
            .await;
    }
    match drain_parallel_async(&call, computer, blob_handle, chunk_size, opts.concurrency).await {
        Ok(v) => Ok(v),
        Err(ParallelErr::Fatal(e)) => Err(e),
        Err(ParallelErr::Fallback) => {
            drain_serial_async(&call, computer, blob_handle, chunk_size, opts.max_retries).await
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
    if opts.concurrency <= 1 {
        return drain_serial_sync(&call, computer, blob_handle, chunk_size, opts.max_retries);
    }
    match drain_parallel_sync(&call, computer, blob_handle, chunk_size, opts.concurrency) {
        Ok(v) => Ok(v),
        Err(ParallelErr::Fatal(e)) => Err(e),
        Err(ParallelErr::Fallback) => {
            drain_serial_sync(&call, computer, blob_handle, chunk_size, opts.max_retries)
        }
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
                    Ok(None) => {
                        recoverable.store(true, Ordering::Relaxed);
                        stop.store(true, Ordering::Relaxed);
                        break;
                    }
                    Err(ChunkErr::Range) => {
                        recoverable.store(true, Ordering::Relaxed);
                        stop.store(true, Ordering::Relaxed);
                        break;
                    }
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

/// base64 标准编码解码 / standard base64 decode。
fn b64_decode(s: &str) -> Result<Vec<u8>, String> {
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .map_err(|e| e.to_string())
}

/// 计算字节的 sha256 十六进制 / sha256 hex of the bytes。
fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
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
}
