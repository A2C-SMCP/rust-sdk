/**
* 文件名: mod
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: None
* 描述: Inputs抽象层模块，负责处理各种输入方式
*/
pub mod handler;
pub mod model;
pub mod providers;

// 重新导出核心类型 / Re-export core types
pub use handler::InputHandler;
pub use model::*;
pub use providers::{CliInputProvider, EnvironmentInputProvider, InputProvider};
