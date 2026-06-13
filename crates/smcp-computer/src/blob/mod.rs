/*!
* 文件名: mod.rs
* 作者: JQQ
* 创建日期: 2026/06/04
* 最后修改日期: 2026/06/04
* 版权: 2023 JQQ. All rights reserved.
* 依赖: handle
* 描述: Computer 侧通用二进制传输（blob）模块。
*       Computer-side generic binary transfer (blob) module.
*/

//! Computer 侧通用二进制传输（blob）。
//!
//! 对标 Python 治理层资产 `a2c_smcp/computer/blob/`（0.2.1）。含 [`handle`]（无状态句柄编解码，BLB-01
//! #62）、[`resolver`]（解析器接口 + SKILL 解析器 + 惰性切片，BLB-02 #64）、[`thresholds`]（SKILL/blob
//! 通道阈值，BLB-02 #64）、[`toolspool`]（tool_call 二进制内容寻址暂存 + 惰性切片解析器，BLB-03 #66）。

pub mod handle;
pub mod resolver;
pub mod thresholds;
pub mod toolspool;

pub use handle::{
    decode_blob_handle, encode_skill_handle, encode_toolspool_handle, BlobHandleError,
    BlobTooLargeError, DecodedHandle, SkillHandlePayload, ToolspoolHandlePayload,
    HANDLE_KIND_SKILL, HANDLE_KIND_TOOLSPOOL, HANDLE_VERSION,
};
pub use resolver::{BlobResolver, BlobSlicer, ResolvedBlob, SkillBlobResolver, SkillRootLookup};
pub use thresholds::{
    default_thresholds, BlobThresholds, DEFAULT_CHUNK_MAX_BYTES, DEFAULT_INLINE_BUDGET,
    DEFAULT_TOO_LARGE_CAP, ENV_CHUNK_MAX_BYTES, ENV_INLINE_BUDGET, ENV_TOO_LARGE_CAP,
};
pub use toolspool::{
    BlobStat, ToolspoolBlobResolver, ToolspoolBlobStore, ToolspoolStoreError, BLOBSPOOL_DIRNAME,
};
