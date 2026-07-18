//! 基础设施工具 / Infrastructure utilities（治理层 SET/SKL 共用）。
//!
//! `{atomic_io,env,path}` 为纯 std（+ `uuid`）实现，无协议依赖：原子写、XDG 优先路径解析、
//! 路径包含判定、环境变量真值判定。`bundle_id` 为纯静态判据（无依赖）：BundleID 字符集/合法性
//! **单一权威**（§BundleID，`smcp-computer` 注册边界与本 crate SKILL 命名共用）。
//! `mime` 为纯静态表（无依赖）：确定性扩展名→MIME + 文本性判据
//! （§6.4，Computer 铸造 / Agent drain 共用单一权威）。`handshake` 复用协议类型；`blob` 引入
//! `base64` / `sha2` / `futures`（分块拉取 + 完整性自证 + 并发）。对标 Python 参考实现 /
//! mirrors the Python reference: `a2c_smcp/utils/{atomic_io,env,path,mime,blob}.py`。
//!
//! 协议依据 / Protocol：`{atomic_io,env,path}` 无直接协议条款（基础设施层，服务于 0.2.2 治理资产）；
//! `blob` 对标 blob-transfer.md（`client:get_blob` 句柄契约 + 4018）。

pub mod atomic_io;
pub mod blob;
pub mod bundle_id;
pub mod env;
pub mod env_segment;
pub mod handshake;
pub mod hash;
pub mod mime;
pub mod path;
pub mod slice;

pub use atomic_io::{atomic_write_bytes, atomic_write_text, unique_tmp_path};
pub use blob::{
    drain_blob, drain_blob_sync, BlobChunkRequest, BlobErrorReason, BlobTransferError,
    DrainBlobOptions, DEFAULT_CHUNK_SIZE, DEFAULT_MAX_RETRIES,
};
pub use bundle_id::{
    is_bundle_id_char, is_valid_bundle_id, validate_bundle_id, BundleId, BundleIdError,
};
pub use env::{env_truthy, env_truthy_in, env_truthy_or, is_truthy};
pub use env_segment::{
    detect_env_name_collisions, env_segment, env_var_name, raise_on_env_name_collisions,
    EnvNameCollisionError, A2C_ENV_PREFIX,
};
pub use handshake::{
    apply_polling_first_guard, build_handshake_url, build_protocol_version_error,
    enforce_polling_first, extract_4008_payload, A2C_VERSION_QUERY_KEY,
    DEFAULT_HANDSHAKE_TRANSPORTS,
};
pub use hash::{sha256_hex, to_hex};
pub use mime::{guess_mime, is_text_mime, FALLBACK_MIME};
pub use path::{is_within, normalize_lexical, resolve_xdg_first};
pub use slice::{plan_slice, SlicePlan};
