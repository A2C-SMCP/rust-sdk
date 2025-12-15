/**
* 文件名: mod
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: tokio, serde_json
* 描述: 测试公共工具模块
*/
/// 带超时的测试辅助宏 / Test helper macro with timeout
#[macro_export]
macro_rules! with_timeout {
    ($future:expr) => {
        timeout(std::time::Duration::from_secs(30), $future)
            .await
            .expect("Test timed out")
    };
}
