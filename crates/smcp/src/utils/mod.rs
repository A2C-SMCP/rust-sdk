//! 基础设施工具 / Infrastructure utilities（治理层 SET/SKL 共用）。
//!
//! 纯 std（+ `uuid`）实现，无协议依赖：原子写、XDG 优先路径解析、路径包含判定、环境变量真值判定。
//! 对标 Python 参考实现 / Mirrors the Python reference: `a2c_smcp/utils/{atomic_io,env,path}.py`。
//!
//! 协议依据 / Protocol: 无直接协议条款（基础设施层），服务于 0.2.2 治理资产
//! （SKILL staging 私有目录、settings 原子持久化）。

pub mod atomic_io;
pub mod env;
pub mod handshake;
pub mod path;

pub use atomic_io::{atomic_write_bytes, atomic_write_text, unique_tmp_path};
pub use env::{env_truthy, env_truthy_in, env_truthy_or, is_truthy};
pub use handshake::{
    apply_polling_first_guard, build_handshake_url, build_protocol_version_error,
    enforce_polling_first, extract_4008_payload, A2C_VERSION_QUERY_KEY,
    DEFAULT_HANDSHAKE_TRANSPORTS,
};
pub use path::{is_within, normalize_lexical, resolve_xdg_first};
