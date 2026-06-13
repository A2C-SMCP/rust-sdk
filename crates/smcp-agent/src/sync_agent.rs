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
use smcp::{A2CSkillRef, GetBlobRet, GetResourcesRet, GetSkillRet, SMCPTool, SessionInfo};
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

    /// 获取指定 Computer 上某 MCP Server 的资源列表（v0.2.0，同步）/ Get a MCP Server's resources (sync)。
    ///
    /// 阻塞包装 [`AsyncSmcpAgent::get_resources`]：透明转发 MCP `resources/list`，`cursor` 调用方控制
    /// （首次 `None`），flat ErrorPayload（`4014`/`4015`）透传为协议错误。
    pub fn get_resources(
        &self,
        computer: &str,
        mcp_server: &str,
        cursor: Option<String>,
    ) -> Result<GetResourcesRet> {
        self.runtime
            .block_on(self.async_agent.get_resources(computer, mcp_server, cursor))
    }

    /// 获取指定 Computer 的 SKILL 清单（v0.2.1，同步）/ Get a Computer's SKILL inventory (sync)。
    pub fn get_skills(&self, computer: &str) -> Result<Vec<A2CSkillRef>> {
        self.runtime.block_on(self.async_agent.get_skills(computer))
    }

    /// 获取 SKILL 包内单个资源（v0.2.1，同步）/ Get a single in-package SKILL resource (sync)。
    ///
    /// 语义同 [`AsyncSmcpAgent::get_skill`]：`body` / `blob_handle` 恰一存在。文本 MIME 的 `blob_handle`
    /// 已由 AGT-03 #38 自动 drain 回填为 `body`（对调用方透明）；二进制句柄原样返回，按需经
    /// [`Self::get_blob`] 自取字节。
    pub fn get_skill(
        &self,
        computer: &str,
        name: &str,
        rel_path: Option<&str>,
    ) -> Result<GetSkillRet> {
        self.runtime
            .block_on(self.async_agent.get_skill(computer, name, rel_path))
    }

    /// 通用二进制单块拉取（v0.2.1，同步，AGT-03 #38）/ Pull one binary chunk via `client:get_blob` (sync)。
    ///
    /// 阻塞包装 [`AsyncSmcpAgent::get_blob`]：按 `chunk_offset`/`max_chunk_bytes` 取一块返回 [`GetBlobRet`]；
    /// flat ErrorPayload（`4018`）透传为协议错误。多块重组由上层用 [`SyncSmcpAgent::tool_call`] /
    /// [`SyncSmcpAgent::get_skill`] 的自动旁路覆盖；裸 drain 暂不公开同步入口（按需再加）。
    pub fn get_blob(
        &self,
        computer: &str,
        blob_handle: &str,
        chunk_offset: Option<u64>,
        max_chunk_bytes: Option<u64>,
    ) -> Result<GetBlobRet> {
        self.runtime.block_on(self.async_agent.get_blob(
            computer,
            blob_handle,
            chunk_offset,
            max_chunk_bytes,
        ))
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

    /// 取消一次在途工具调用（fire-and-forget，无 ack，AGT-05 #44）。
    /// `req_id` MUST==被取消的原 tool_call req_id。详见 [`AsyncSmcpAgent::tool_call_cancel`]。
    pub fn tool_call_cancel(&self, req_id: &str) -> Result<()> {
        self.runtime
            .block_on(self.async_agent.tool_call_cancel(req_id))
    }

    /// 列出房间内的所有会话
    pub fn list_room(&self, office_id: &str) -> Result<Vec<SessionInfo>> {
        self.runtime.block_on(self.async_agent.list_room(office_id))
    }
}
