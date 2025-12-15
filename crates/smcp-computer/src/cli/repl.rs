/*!
* 文件名: repl.rs
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: None
* 描述: REPL实现 / REPL implementation
*/

use crate::core::ComputerCore;
use crate::errors::ComputerResult;

/// REPL / REPL
#[allow(dead_code)]
pub struct Repl {
    /// Computer核心 / Computer core
    computer: ComputerCore,
}

impl Repl {
    /// 创建新的REPL / Create new REPL
    pub fn new(computer: ComputerCore) -> Self {
        Self { computer }
    }
    
    /// 运行REPL / Run REPL
    pub async fn run(&self) -> ComputerResult<()> {
        // TODO: 实现REPL逻辑
        println!("SMCP Computer REPL");
        Ok(())
    }
}
