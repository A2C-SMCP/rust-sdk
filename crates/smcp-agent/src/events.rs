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
    EnterOfficeNotification, LeaveOfficeNotification, SMCPTool, UpdateMCPConfigNotification,
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
}

// 前向声明，避免循环依赖
use crate::{AsyncSmcpAgent, SyncSmcpAgent};
