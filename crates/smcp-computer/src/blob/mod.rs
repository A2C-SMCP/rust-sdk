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
//! 对标 Python 治理层资产 `a2c_smcp/computer/blob/`（0.2.1）。当前含 [`handle`]（无状态句柄编解码，BLB-01
//! #62）；resolver（BLB-02 #64）/ toolspool（BLB-03 #66）后续接入。

pub mod handle;

pub use handle::{
    decode_blob_handle, encode_skill_handle, encode_toolspool_handle, BlobHandleError,
    BlobTooLargeError, DecodedHandle, SkillHandlePayload, ToolspoolHandlePayload,
    HANDLE_KIND_SKILL, HANDLE_KIND_TOOLSPOOL, HANDLE_VERSION,
};
