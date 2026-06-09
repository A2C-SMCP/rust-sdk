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
    /// `client:get_*` 请求 ack 超时（秒）。协议 0.2.2 建议 **≥30s**（AGT-05 #44）——get_skills/get_skill
    /// 等可能拉取较大 SKILL/blob 元数据，过短阈值会误判为超时。缺省 30s。
    /// `client:get_*` ack timeout; protocol 0.2.2 recommends ≥30s (large skill/blob payloads). Default 30s.
    pub get_timeout: u64,
    /// 是否在收到桌面更新通知时自动拉取桌面
    pub auto_fetch_desktop: bool,
    /// 是否在 Computer 进入办公室时自动获取工具列表
    /// When true, agent will automatically fetch tools when a computer enters the office
    pub auto_fetch_tools: bool,
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
            get_timeout: 30,
            auto_fetch_desktop: true,
            auto_fetch_tools: true,
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

    /// 设置 `client:get_*` ack 超时（秒）。协议 0.2.2 建议 ≥30s（AGT-05 #44）。
    pub fn with_get_timeout(mut self, timeout: u64) -> Self {
        self.get_timeout = timeout;
        self
    }

    pub fn with_auto_fetch_desktop(mut self, auto_fetch: bool) -> Self {
        self.auto_fetch_desktop = auto_fetch;
        self
    }

    pub fn with_auto_fetch_tools(mut self, auto_fetch: bool) -> Self {
        self.auto_fetch_tools = auto_fetch;
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
            .with_auto_fetch_tools(false)
            .with_max_retries(5)
            .with_reconnect_interval(2000);

        assert_eq!(config.default_timeout, 10);
        assert_eq!(config.tool_call_timeout, 30);
        assert!(!config.auto_fetch_desktop);
        assert!(!config.auto_fetch_tools);
        assert_eq!(config.max_retries, 5);
        assert_eq!(config.reconnect_interval, 2000);
    }

    #[test]
    fn test_agent_config_defaults() {
        let config = SmcpAgentConfig::default();

        assert_eq!(config.default_timeout, 20);
        assert_eq!(config.tool_call_timeout, 60);
        // 协议 0.2.2：client:get_* ack 超时缺省 ≥30s（AGT-05 #44）。
        assert_eq!(config.get_timeout, 30);
        assert!(config.get_timeout >= 30, "client:get_* 超时 MUST ≥30s");
        assert!(config.auto_fetch_desktop);
        assert!(config.auto_fetch_tools); // 默认开启 / Default enabled
        assert_eq!(config.max_retries, 3);
        assert_eq!(config.reconnect_interval, 1000);
    }
}
