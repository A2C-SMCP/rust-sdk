/*!
* 文件名: thresholds.rs
* 作者: JQQ
* 创建日期: 2026/06/05
* 最后修改日期: 2026/06/05
* 版权: 2023 JQQ. All rights reserved.
* 依赖: std::env
* 描述: SKILL / blob 通道阈值（inline 预算 / too_large 上限 / 单块上限），SDK 可配 + env 覆盖。
*       SKILL / blob channel thresholds (inline budget / too_large cap / chunk ceiling).
*/

//! SKILL / blob 通道阈值集合。
//!
//! 对标 Python 治理层资产 `a2c_smcp/computer/blob/thresholds.py`（0.2.1）。设计依据
//! `design-0.2.1-skill-computer-management.md` §4.4「保证单条 ack 不超 Server buffer」。
//!
//! - [`BlobThresholds::inline_budget`]：文本资源 `total_size` ≤ 此 → 直接 body，否则铸 `blob_handle`；
//!   二进制 MIME 一律铸句柄（与此无关）。
//! - [`BlobThresholds::too_large_cap`]：`total_size` 超此 → `4017 too_large`，**不铸句柄、零字节传输**（DoS 防御）。
//! - [`BlobThresholds::chunk_max_bytes`]：单块原始字节上限，`clamp` 后恒保证 base64(+33%)+envelope ≤ Server
//!   `maxHttpBufferSize`（默认 1 MiB）。
//! - [`BlobThresholds::upload_idle_timeout_seconds`] / [`BlobThresholds::upload_max_concurrent`] /
//!   [`BlobThresholds::upload_max_bytes`]：`client:put_blob` 上行**有界会话**（协议 blob-transfer.md §3
//!   MUST）——闲置超时 / 并发上限 / 首块声明上限（超 → `4019 too_large`，零字节落盘）。阈值 SDK
//!   自治、独立于下行三键。

use std::env;

/// 文本资源内联预算默认值（32 KiB）/ Default inline text budget。
pub const DEFAULT_INLINE_BUDGET: u64 = 32 * 1024;
/// 资源绝对上限默认值（100 MiB）/ Default hard size cap。
pub const DEFAULT_TOO_LARGE_CAP: u64 = 100 * 1024 * 1024;
/// 单块原始字节上限默认值（256 KiB）/ Default per-chunk raw-byte ceiling。
pub const DEFAULT_CHUNK_MAX_BYTES: u64 = 256 * 1024;
/// 上传会话闲置超时默认值（120 s）/ Default upload-session idle timeout。
pub const DEFAULT_UPLOAD_IDLE_TIMEOUT_SECONDS: u64 = 120;
/// 并发上传会话上限默认值（4）/ Default concurrent-upload cap。
pub const DEFAULT_UPLOAD_MAX_CONCURRENT: u64 = 4;
/// 首块声明总字节上限默认值（100 MiB）/ Default declared-upload size cap。
pub const DEFAULT_UPLOAD_MAX_BYTES: u64 = 100 * 1024 * 1024;

/// env 覆盖键：inline 预算 / Env override key for the inline budget。
pub const ENV_INLINE_BUDGET: &str = "A2C_SKILL_INLINE_BUDGET";
/// env 覆盖键：too_large 上限 / Env override key for the hard cap。
pub const ENV_TOO_LARGE_CAP: &str = "A2C_SKILL_MAX_SIZE";
/// env 覆盖键：单块上限 / Env override key for the chunk ceiling。
pub const ENV_CHUNK_MAX_BYTES: &str = "A2C_BLOB_CHUNK_BYTES";
/// env 覆盖键：上传会话闲置超时 / Env override key for the upload idle timeout。
pub const ENV_UPLOAD_IDLE_TIMEOUT: &str = "A2C_UPLOAD_IDLE_TIMEOUT";
/// env 覆盖键：并发上传会话上限 / Env override key for the concurrent-upload cap。
pub const ENV_UPLOAD_MAX_CONCURRENT: &str = "A2C_UPLOAD_MAX_CONCURRENT";
/// env 覆盖键：首块声明总字节上限 / Env override key for the declared-upload size cap。
pub const ENV_UPLOAD_MAX_BYTES: &str = "A2C_UPLOAD_MAX_BYTES";

/// SKILL / blob 阈值集合（不可变，便于注入与共享）/ Immutable threshold bundle for injection。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlobThresholds {
    /// 文本资源内联预算（字节）/ inline text budget in bytes。
    pub inline_budget: u64,
    /// 资源绝对上限（字节）/ hard size cap in bytes。
    pub too_large_cap: u64,
    /// 单块原始字节上限 / per-chunk raw-byte ceiling。
    pub chunk_max_bytes: u64,
    /// 上传会话闲置超时（秒）；作废后该 `upload_id` → `4019 invalid_upload` / idle timeout。
    pub upload_idle_timeout_seconds: u64,
    /// 并发上传会话上限；打满 → `4019 busy` / concurrent-upload cap。
    pub upload_max_concurrent: u64,
    /// 首块声明总字节上限；超 → `4019 too_large`（零字节落盘）/ declared-upload size cap。
    pub upload_max_bytes: u64,
}

impl Default for BlobThresholds {
    fn default() -> Self {
        Self {
            inline_budget: DEFAULT_INLINE_BUDGET,
            too_large_cap: DEFAULT_TOO_LARGE_CAP,
            chunk_max_bytes: DEFAULT_CHUNK_MAX_BYTES,
            upload_idle_timeout_seconds: DEFAULT_UPLOAD_IDLE_TIMEOUT_SECONDS,
            upload_max_concurrent: DEFAULT_UPLOAD_MAX_CONCURRENT,
            upload_max_bytes: DEFAULT_UPLOAD_MAX_BYTES,
        }
    }
}

impl BlobThresholds {
    /// `clamp` 客户建议的单块大小到 `[1, chunk_max_bytes]`；`None` / 非正 → `chunk_max_bytes`。
    ///
    /// 对标 Python `clamp_chunk`：缺省 / ≤0 取 max，超 max 取 max，合法取原值。入参取 `i64` 以忠实表达
    /// 「客户建议可能为负 / 0」并统一回退（协议线 `chunk_size` 来自 JSON number，恶意值需防御）。
    pub fn clamp_chunk(&self, requested: Option<i64>) -> u64 {
        match requested {
            Some(n) if n > 0 => (n as u64).min(self.chunk_max_bytes),
            _ => self.chunk_max_bytes,
        }
    }
}

/// 构造一份**当前进程**默认阈值，按环境变量覆盖；非整数 / 非正 env → 回退默认值。
///
/// 对标 Python `default_thresholds`。
pub fn default_thresholds() -> BlobThresholds {
    BlobThresholds {
        inline_budget: read_positive_int_env(ENV_INLINE_BUDGET, DEFAULT_INLINE_BUDGET),
        too_large_cap: read_positive_int_env(ENV_TOO_LARGE_CAP, DEFAULT_TOO_LARGE_CAP),
        chunk_max_bytes: read_positive_int_env(ENV_CHUNK_MAX_BYTES, DEFAULT_CHUNK_MAX_BYTES),
        upload_idle_timeout_seconds: read_positive_int_env(
            ENV_UPLOAD_IDLE_TIMEOUT,
            DEFAULT_UPLOAD_IDLE_TIMEOUT_SECONDS,
        ),
        upload_max_concurrent: read_positive_int_env(
            ENV_UPLOAD_MAX_CONCURRENT,
            DEFAULT_UPLOAD_MAX_CONCURRENT,
        ),
        upload_max_bytes: read_positive_int_env(ENV_UPLOAD_MAX_BYTES, DEFAULT_UPLOAD_MAX_BYTES),
    }
}

/// 读正整数 env：缺失 / 非整数 / 非正 → `fallback` / Read a positive-int env or fall back。
fn read_positive_int_env(key: &str, fallback: u64) -> u64 {
    match env::var(key) {
        // `parse::<u64>` 天然拒绝负数 / 非数字；再加 `> 0` 拒绝 `0`，与 Python `value > 0 else fallback` 一致。
        Ok(raw) => match raw.trim().parse::<u64>() {
            Ok(v) if v > 0 => v,
            _ => fallback,
        },
        Err(_) => fallback,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// env 是进程全局——串行化所有读写 `A2C_SKILL_*` / `A2C_BLOB_*` 的测试，避免并行污染。
    static ENV_GUARD: Mutex<()> = Mutex::new(());

    fn clear_env() {
        for key in [
            ENV_INLINE_BUDGET,
            ENV_TOO_LARGE_CAP,
            ENV_CHUNK_MAX_BYTES,
            ENV_UPLOAD_IDLE_TIMEOUT,
            ENV_UPLOAD_MAX_CONCURRENT,
            ENV_UPLOAD_MAX_BYTES,
        ] {
            env::remove_var(key);
        }
    }

    #[test]
    fn defaults_match_design_4_4() {
        let t = BlobThresholds::default();
        assert_eq!(t.inline_budget, 32 * 1024);
        assert_eq!(t.too_large_cap, 100 * 1024 * 1024);
        assert_eq!(t.chunk_max_bytes, 256 * 1024);
        assert_eq!(t.upload_idle_timeout_seconds, 120);
        assert_eq!(t.upload_max_concurrent, 4);
        assert_eq!(t.upload_max_bytes, 100 * 1024 * 1024);
    }

    #[test]
    fn default_thresholds_with_no_env_uses_defaults() {
        let _g = ENV_GUARD.lock().unwrap();
        clear_env();
        assert_eq!(default_thresholds(), BlobThresholds::default());
    }

    #[test]
    fn env_overrides_are_honored() {
        let _g = ENV_GUARD.lock().unwrap();
        clear_env();
        env::set_var(ENV_INLINE_BUDGET, "1024");
        env::set_var(ENV_TOO_LARGE_CAP, "2048");
        env::set_var(ENV_CHUNK_MAX_BYTES, "4096");
        env::set_var(ENV_UPLOAD_IDLE_TIMEOUT, "10");
        env::set_var(ENV_UPLOAD_MAX_CONCURRENT, "2");
        env::set_var(ENV_UPLOAD_MAX_BYTES, "8192");
        let t = default_thresholds();
        assert_eq!(t.inline_budget, 1024);
        assert_eq!(t.too_large_cap, 2048);
        assert_eq!(t.chunk_max_bytes, 4096);
        assert_eq!(t.upload_idle_timeout_seconds, 10);
        assert_eq!(t.upload_max_concurrent, 2);
        assert_eq!(t.upload_max_bytes, 8192);
        clear_env();
    }

    #[test]
    fn env_non_integer_or_non_positive_falls_back() {
        let _g = ENV_GUARD.lock().unwrap();
        clear_env();
        // 非整数 / 负数 / 0 / 浮点 → 一律回退默认。
        env::set_var(ENV_INLINE_BUDGET, "not-a-number");
        env::set_var(ENV_TOO_LARGE_CAP, "-5");
        env::set_var(ENV_CHUNK_MAX_BYTES, "0");
        let t = default_thresholds();
        assert_eq!(t.inline_budget, DEFAULT_INLINE_BUDGET);
        assert_eq!(t.too_large_cap, DEFAULT_TOO_LARGE_CAP);
        assert_eq!(t.chunk_max_bytes, DEFAULT_CHUNK_MAX_BYTES);
        // 浮点也回退。
        env::set_var(ENV_INLINE_BUDGET, "1024.0");
        assert_eq!(default_thresholds().inline_budget, DEFAULT_INLINE_BUDGET);
        clear_env();
    }

    #[test]
    fn clamp_chunk_boundaries() {
        let t = BlobThresholds::default();
        let max = t.chunk_max_bytes;
        // None / 0 / 负 → max
        assert_eq!(t.clamp_chunk(None), max);
        assert_eq!(t.clamp_chunk(Some(0)), max);
        assert_eq!(t.clamp_chunk(Some(-1)), max);
        // 超 max → max
        assert_eq!(t.clamp_chunk(Some((max + 1) as i64)), max);
        // 合法 → 原值
        assert_eq!(t.clamp_chunk(Some(1)), 1);
        assert_eq!(t.clamp_chunk(Some(1024)), 1024);
        assert_eq!(t.clamp_chunk(Some(max as i64)), max);
    }
}
