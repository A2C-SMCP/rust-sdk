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

    /// 自动回房**最终失败**时触发（#219）/ Triggered when automatic office rejoin gives up。
    ///
    /// 触发时机 / Trigger：传输层重连后重放 `server:join_office` 未能恢复成员关系，且 SDK 已按策略
    /// 停止重试（重试预算耗尽，或被 `4106` / `400` / `403` 等永久性拒绝）。此刻本地成员关系**已被
    /// 清空**——[`crate::AsyncSmcpAgent::office_membership`] 回退到
    /// [`crate::office::OfficeMembershipState::Connected`]，绝不静默假装仍在房。消费方据此清理依赖该
    /// 房间的本地视图（工具 / 桌面 / SKILL 缓存等）。
    ///
    /// 不触发的情形 / Not triggered：
    ///
    /// - 传输层断线但会自动重连、且回房仍在预算内——SDK 正在自愈，尚不构成「失去」；
    /// - 服务端踢出与调用方显式退房——那是**调用方自己表达的意图**（或协议明确的终态），不属意外失去；
    /// - 传输层彻底放弃重连（`tf-rust-socketio` 未提供「重连耗尽」事件，见 `docs/agent/office-rejoin.md`）。
    ///
    /// 向后兼容 / Backward-compat：默认实现仅记录 error 日志，旧处理器无需改动即编译通过。
    async fn on_office_membership_lost(
        &self,
        office_id: &str,
        reason: &str,
        _agent: &AsyncSmcpAgent,
    ) -> Result<(), crate::error::SmcpAgentError> {
        tracing::error!("Office membership lost for {}: {}", office_id, reason);
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
