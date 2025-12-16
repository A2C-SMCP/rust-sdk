/*!
* 文件名: mod.rs
* 作者: JQQ
* 创建日期: 2025/12/16
* 最后修改日期: 2025/12/16
* 版权: 2023 JQQ. All rights reserved.
* 依赖: serde
* 描述: Desktop模块入口 / Desktop module entry point
*/

pub mod model;
pub mod organize;

pub use model::*;
pub use organize::*;

/// 桌面内容类型 / Desktop content type
pub type Desktop = String;
