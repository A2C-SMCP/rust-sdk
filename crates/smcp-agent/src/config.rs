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
    /// 「重连后回房」单次 `server:join_office` 重放的 **ack 等待上限（秒）**。
    ///
    /// 默认 `10`——与 python 参考实现 `OFFICE_REJOIN_TIMEOUT` 及本仓 Computer 侧重放所用的 `10`
    /// 同值（跨端时序可比）。回房用**等 ack**（`call`）而非 emit，故本值即单次尝试的耗时上界。
    ///
    /// Per-attempt ack timeout (seconds) for the automatic office rejoin.
    pub office_rejoin_timeout: u64,
    /// 「重连后回房」有界退避重试的**总预算（秒）**。
    ///
    /// 默认 `60`——须覆盖部署方的会话回收窗口：socket.io 默认 `ping_interval(25) + ping_timeout(20)`
    /// ⇒ 最长 45s（协议 room-model §静默断线与会话回收 的 SHOULD 级部署约束），故默认预算须显著大于
    /// 该窗口。预算约束**下一次尝试的实际起始时刻**，包括退避及操作锁等待。
    /// 无并发显式操作时总耗时上界 ≈ 预算 + [`Self::office_rejoin_timeout`]；首次尝试至少一次。
    /// 失败状态提交和通知须等在途显式入房完成，可能晚于网络重试预算。
    ///
    /// `0` ⇒ 仅单次尝试（协议规定的下限：首次入房撞上冲突视为永久冲突，不重试）。
    ///
    /// Total budget (seconds) for the bounded backoff retries of the automatic office rejoin.
    pub office_rejoin_budget_secs: u64,
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
            office_rejoin_timeout: 10,
            office_rejoin_budget_secs: 60,
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

    /// 设置回房单次重放的 ack 等待上限（秒）。默认 10s。
    pub fn with_office_rejoin_timeout(mut self, timeout_secs: u64) -> Self {
        self.office_rejoin_timeout = timeout_secs;
        self
    }

    /// 设置回房有界退避重试的总预算（秒）。默认 60s（覆盖 socket.io 默认 45s 回收窗口）；
    /// 传 `0` ⇒ 仅单次尝试。
    pub fn with_office_rejoin_budget_secs(mut self, budget_secs: u64) -> Self {
        self.office_rejoin_budget_secs = budget_secs;
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
            .with_reconnect_interval(2000)
            .with_office_rejoin_timeout(3)
            .with_office_rejoin_budget_secs(20);

        assert_eq!(config.default_timeout, 10);
        assert_eq!(config.tool_call_timeout, 30);
        assert!(!config.auto_fetch_desktop);
        assert!(!config.auto_fetch_tools);
        assert_eq!(config.max_retries, 5);
        assert_eq!(config.reconnect_interval, 2000);
        assert_eq!(config.office_rejoin_timeout, 3);
        assert_eq!(config.office_rejoin_budget_secs, 20);
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
        // 回房默认：单次 ack 等待 10s（对齐 python `OFFICE_REJOIN_TIMEOUT` 与本仓 Computer 侧），
        // 总预算 60s ⇒ 覆盖 socket.io 默认最长回收窗口 45s（#219 裁决）。
        assert_eq!(config.office_rejoin_timeout, 10);
        assert_eq!(
            config.office_rejoin_budget_secs, 60,
            "默认回房预算 MUST 覆盖 socket.io 默认最长回收窗口 45s"
        );
        assert!(config.office_rejoin_budget_secs > 45);
    }
}
