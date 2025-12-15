/*!
* 文件名: events.rs
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: None
* 描述: 事件定义 / Event definitions
*/

use serde::{Deserialize, Serialize};

/// Computer事件类型 / Computer event types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ComputerEvent {
    /// 工具列表变更 / Tool list changed
    ToolListChanged {
        server_name: String,
        tools: Vec<String>,
    },
    /// 桌面变更 / Desktop changed
    DesktopChanged {
        window_uris: Vec<String>,
    },
    /// 配置变更 / Configuration changed
    ConfigChanged {
        server_name: Option<String>,
    },
    /// 服务器状态变更 / Server status changed
    ServerStatusChanged {
        server_name: String,
        active: bool,
        state: String,
    },
}

/// 事件处理器 / Event handler
pub trait EventHandler: Send + Sync {
    /// 处理事件 / Handle event
    fn handle_event(&self, event: ComputerEvent);
}

/// 事件发射器 / Event emitter
pub trait EventEmitter: Send + Sync {
    /// 注册事件处理器 / Register event handler
    fn on_event(&mut self, handler: Box<dyn EventHandler>);
    
    /// 发射事件 / Emit event
    fn emit(&self, event: ComputerEvent);
}
