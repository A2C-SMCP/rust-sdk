/*!
* 文件名: config
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: None
* 描述: SMCP Agent配置 / SMCP Agent configuration
*/

/// SMCP Agent配置
#[derive(Debug, Clone)]
pub struct SmcpAgentConfig {
    /// 默认超时时间（秒）
    pub default_timeout: u64,
    /// 工具调用超时时间（秒）
    pub tool_call_timeout: u64,
    /// 是否在收到桌面更新通知时自动拉取桌面
    pub auto_fetch_desktop: bool,
    /// 连接重试次数
    pub max_retries: u32,
    /// 重连间隔（毫秒）
    pub reconnect_interval: u64,
}

impl Default for SmcpAgentConfig {
    fn default() -> Self {
        Self {
            default_timeout: 20,
            tool_call_timeout: 60,
            auto_fetch_desktop: true,
            max_retries: 3,
            reconnect_interval: 1000,
        }
    }
}

impl SmcpAgentConfig {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_default_timeout(mut self, timeout: u64) -> Self {
        self.default_timeout = timeout;
        self
    }

    pub fn with_tool_call_timeout(mut self, timeout: u64) -> Self {
        self.tool_call_timeout = timeout;
        self
    }

    pub fn with_auto_fetch_desktop(mut self, auto_fetch: bool) -> Self {
        self.auto_fetch_desktop = auto_fetch;
        self
    }

    pub fn with_max_retries(mut self, max_retries: u32) -> Self {
        self.max_retries = max_retries;
        self
    }

    pub fn with_reconnect_interval(mut self, interval: u64) -> Self {
        self.reconnect_interval = interval;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_agent_config() {
        let config = SmcpAgentConfig::new()
            .with_default_timeout(10)
            .with_tool_call_timeout(30)
            .with_auto_fetch_desktop(false)
            .with_max_retries(5)
            .with_reconnect_interval(2000);

        assert_eq!(config.default_timeout, 10);
        assert_eq!(config.tool_call_timeout, 30);
        assert!(!config.auto_fetch_desktop);
        assert_eq!(config.max_retries, 5);
        assert_eq!(config.reconnect_interval, 2000);
    }
}
