/*!
* 文件名: events
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: None
* 描述: SMCP Agent事件处理器定义 / SMCP Agent event handler definitions
*/

use async_trait::async_trait;
use smcp::{
    A2CSkillRef, EnterOfficeNotification, LeaveOfficeNotification, SMCPTool,
    UpdateMCPConfigNotification, UpdateToolListNotification,
};

/// 异步事件处理器trait
#[async_trait]
pub trait AsyncAgentEventHandler: Send + Sync {
    /// 当Computer进入办公室时触发
    async fn on_computer_enter_office(
        &self,
        data: EnterOfficeNotification,
        _agent: &AsyncSmcpAgent,
    ) -> Result<(), crate::error::SmcpAgentError> {
        tracing::info!("Computer entered office: {:?}", data);
        Ok(())
    }

    /// 当Computer离开办公室时触发
    async fn on_computer_leave_office(
        &self,
        data: LeaveOfficeNotification,
        _agent: &AsyncSmcpAgent,
    ) -> Result<(), crate::error::SmcpAgentError> {
        tracing::info!("Computer left office: {:?}", data);
        Ok(())
    }

    /// 当Computer更新配置时触发
    async fn on_computer_update_config(
        &self,
        data: UpdateMCPConfigNotification,
        _agent: &AsyncSmcpAgent,
    ) -> Result<(), crate::error::SmcpAgentError> {
        tracing::info!("Computer updated config: {:?}", data);
        Ok(())
    }

    /// 收到 `notify:update_tool_list` 时的**预清回调**（#106，对标 python-sdk #127）。
    ///
    /// 触发时机 / Trigger：在 Agent 自动重拉 `get_tools` → [`on_tools_received`](Self::on_tools_received)
    /// **之前**派发，语义对齐 [`on_computer_update_config`](Self::on_computer_update_config)。
    ///
    /// **为何需要 / Why**：`on_tools_received` 交付的是 Computer 当前**全量**工具集，但**加法式**下游消费方
    /// （只 add 不 remove，如 TFRobotServer）无法据此感知**移除 / 同名换 schema**——旧定义会残留。消费方可在此
    /// 预清回调里先清空该 computer 的既有工具视图，形成「预清 → 回拉 → 重加」三段式，使移除/换 schema 正确生效。
    ///
    /// 向后兼容 / Backward-compat：默认实现仅记录日志（no-op），旧处理器无需改动——以默认实现取代 Python 的
    /// `hasattr` 运行时探测。
    async fn on_computer_update_tool_list(
        &self,
        data: UpdateToolListNotification,
        _agent: &AsyncSmcpAgent,
    ) -> Result<(), crate::error::SmcpAgentError> {
        tracing::info!("Computer tool list updated (pre-clear hook): {:?}", data);
        Ok(())
    }

    /// 当工具列表更新时触发
    async fn on_tools_received(
        &self,
        computer: &str,
        tools: Vec<SMCPTool>,
        _agent: &AsyncSmcpAgent,
    ) -> Result<(), crate::error::SmcpAgentError> {
        tracing::info!("Received {} tools from computer: {}", tools.len(), computer);
        Ok(())
    }

    /// 当桌面更新时触发
    async fn on_desktop_updated(
        &self,
        computer: &str,
        desktops: Vec<String>,
        _agent: &AsyncSmcpAgent,
    ) -> Result<(), crate::error::SmcpAgentError> {
        tracing::info!(
            "Desktop updated for computer: {}, windows: {}",
            computer,
            desktops.len()
        );
        Ok(())
    }

    /// 当 SKILL 清单更新时触发（v0.2.1）/ Triggered when the SKILL inventory updates。
    ///
    /// 触发时机 / Trigger：收到 `notify:update_skills` 后 Agent 自动重拉 `client:get_skills` 成功时。
    /// 携带轻量 [`A2CSkillRef`] 列表（无 SKILL.md body；body 经 `get_skill` 按需拉取）。
    ///
    /// 向后兼容 / Backward-compat：本方法提供默认实现（仅记录日志），旧处理器无需改动即编译通过、
    /// 行为为 no-op——对标 Python 端的 `hasattr` 守卫（Rust 以默认实现取代运行时探测）。
    async fn on_skills_received(
        &self,
        computer: &str,
        skills: Vec<A2CSkillRef>,
        _agent: &AsyncSmcpAgent,
    ) -> Result<(), crate::error::SmcpAgentError> {
        tracing::info!(
            "Received {} skills from computer: {}",
            skills.len(),
            computer
        );
        Ok(())
    }
}

/// 同步事件处理器trait
pub trait AgentEventHandler: Send + Sync {
    /// 当Computer进入办公室时触发
    fn on_computer_enter_office(
        &self,
        data: EnterOfficeNotification,
        _agent: &SyncSmcpAgent,
    ) -> Result<(), crate::error::SmcpAgentError> {
        tracing::info!("Computer entered office: {:?}", data);
        Ok(())
    }

    /// 当Computer离开办公室时触发
    fn on_computer_leave_office(
        &self,
        data: LeaveOfficeNotification,
        _agent: &SyncSmcpAgent,
    ) -> Result<(), crate::error::SmcpAgentError> {
        tracing::info!("Computer left office: {:?}", data);
        Ok(())
    }

    /// 当Computer更新配置时触发
    fn on_computer_update_config(
        &self,
        data: UpdateMCPConfigNotification,
        _agent: &SyncSmcpAgent,
    ) -> Result<(), crate::error::SmcpAgentError> {
        tracing::info!("Computer updated config: {:?}", data);
        Ok(())
    }

    /// 收到 `notify:update_tool_list` 时的**预清回调**（#106，同步版；语义同
    /// [`AsyncAgentEventHandler::on_computer_update_tool_list`]）。默认 no-op，向后兼容。
    fn on_computer_update_tool_list(
        &self,
        data: UpdateToolListNotification,
        _agent: &SyncSmcpAgent,
    ) -> Result<(), crate::error::SmcpAgentError> {
        tracing::info!("Computer tool list updated (pre-clear hook): {:?}", data);
        Ok(())
    }

    /// 当工具列表更新时触发
    fn on_tools_received(
        &self,
        computer: &str,
        tools: Vec<SMCPTool>,
        _agent: &SyncSmcpAgent,
    ) -> Result<(), crate::error::SmcpAgentError> {
        tracing::info!("Received {} tools from computer: {}", tools.len(), computer);
        Ok(())
    }

    /// 当桌面更新时触发
    fn on_desktop_updated(
        &self,
        computer: &str,
        desktops: Vec<String>,
        _agent: &SyncSmcpAgent,
    ) -> Result<(), crate::error::SmcpAgentError> {
        tracing::info!(
            "Desktop updated for computer: {}, windows: {}",
            computer,
            desktops.len()
        );
        Ok(())
    }

    /// 当 SKILL 清单更新时触发（v0.2.1，同步）/ Triggered when the SKILL inventory updates (sync)。
    ///
    /// 语义同 [`AsyncAgentEventHandler::on_skills_received`]：`notify:update_skills` 自动重拉成功后派发；
    /// 默认实现仅记录日志，保证旧处理器向后兼容（对标 Python `hasattr` 守卫）。
    fn on_skills_received(
        &self,
        computer: &str,
        skills: Vec<A2CSkillRef>,
        _agent: &SyncSmcpAgent,
    ) -> Result<(), crate::error::SmcpAgentError> {
        tracing::info!(
            "Received {} skills from computer: {}",
            skills.len(),
            computer
        );
        Ok(())
    }
}

// 前向声明，避免循环依赖
use crate::{AsyncSmcpAgent, SyncSmcpAgent};
