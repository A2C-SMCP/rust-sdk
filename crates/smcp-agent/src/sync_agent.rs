/*!
* 文件名: sync_agent
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: tokio
* 描述: SMCP同步Agent实现 / SMCP synchronous Agent implementation
*/

use crate::{
    auth::AuthProvider,
    config::SmcpAgentConfig,
    error::{Result, SmcpAgentError},
    AsyncSmcpAgent,
};
use smcp::{SMCPTool, SessionInfo};
use tokio::runtime::Runtime;

/// 同步SMCP Agent
pub struct SyncSmcpAgent {
    runtime: Runtime,
    async_agent: AsyncSmcpAgent,
}

impl SyncSmcpAgent {
    /// 创建新的同步Agent实例
    pub fn new(
        auth_provider: impl AuthProvider + 'static,
        config: SmcpAgentConfig,
    ) -> Result<Self> {
        let runtime = Runtime::new()
            .map_err(|e| SmcpAgentError::internal(format!("Failed to create runtime: {}", e)))?;

        let async_agent = AsyncSmcpAgent::new(auth_provider, config);

        Ok(Self {
            runtime,
            async_agent,
        })
    }

    /// 连接到服务器
    pub fn connect(&mut self, url: &str) -> Result<()> {
        self.runtime.block_on(self.async_agent.connect(url))
    }

    /// 加入办公室
    pub fn join_office(&self, agent_name: &str) -> Result<()> {
        self.runtime
            .block_on(self.async_agent.join_office(agent_name))
    }

    /// 离开办公室
    pub fn leave_office(&self) -> Result<()> {
        self.runtime.block_on(self.async_agent.leave_office())
    }

    /// 获取指定Computer的工具列表
    pub fn get_tools(&self, computer: &str) -> Result<Vec<SMCPTool>> {
        self.runtime.block_on(self.async_agent.get_tools(computer))
    }

    /// 获取指定Computer的桌面信息
    pub fn get_desktop(
        &self,
        computer: &str,
        size: Option<i32>,
        window: Option<String>,
    ) -> Result<Vec<String>> {
        self.runtime
            .block_on(self.async_agent.get_desktop(computer, size, window))
    }

    /// 调用工具
    pub fn tool_call(
        &self,
        computer: &str,
        tool_name: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        self.runtime
            .block_on(self.async_agent.tool_call(computer, tool_name, params))
    }

    /// 列出房间内的所有会话
    pub fn list_room(&self, office_id: &str) -> Result<Vec<SessionInfo>> {
        self.runtime.block_on(self.async_agent.list_room(office_id))
    }
}
