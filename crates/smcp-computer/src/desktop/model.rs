/**
* 文件名: model.rs
* 作者: JQQ
* 创建日期: 2025/12/16
* 最后修改日期: 2025/12/16
* 版权: 2023 JQQ. All rights reserved.
* 依赖: serde, async-trait
* 描述: Desktop相关的数据模型定义 / Desktop-related data model definitions
*/
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// 重新导出mcp_clients中的类型 / Re-export types from mcp_clients
pub use crate::mcp_clients::model::{ReadResourceResult, Resource, TextResourceContents};

/// 服务器名称类型 / Server name type
pub type ServerName = String;

/// 工具调用记录 / Tool call record
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCallRecord {
    /// 服务器名称 / Server name
    pub server: ServerName,
    /// 工具名称 / Tool name
    pub tool: String,
    /// 调用时间戳 / Call timestamp
    pub timestamp: i64,
    /// 额外元数据 / Additional metadata
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub metadata: HashMap<String, serde_json::Value>,
}

/// 窗口信息 / Window information
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WindowInfo {
    /// 服务器名称 / Server name
    pub server_name: ServerName,
    /// 资源 / Resource
    pub resource: Resource,
    /// 读取结果 / Read result
    pub read_result: ReadResourceResult,
}

impl WindowInfo {
    /// 创建新的窗口信息 / Create new window info
    pub fn new(
        server_name: ServerName,
        resource: Resource,
        read_result: ReadResourceResult,
    ) -> Self {
        Self {
            server_name,
            resource,
            read_result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_window_info() {
        let resource = Resource {
            uri: "window://test.mcp.com/window1".to_string(),
            name: "Test Window".to_string(),
            description: Some("Test window".to_string()),
            mime_type: None,
        };

        let read_result = ReadResourceResult {
            contents: vec![TextResourceContents {
                uri: "window://test.mcp.com/window1".to_string(),
                text: "Test content".to_string(),
                mime_type: None,
            }],
        };

        let window_info = WindowInfo::new("test_server".to_string(), resource, read_result);

        assert_eq!(window_info.server_name, "test_server");
        assert_eq!(window_info.resource.name, "Test Window");
        assert_eq!(window_info.read_result.contents.len(), 1);
    }

    #[test]
    fn test_tool_call_record() {
        let record = ToolCallRecord {
            server: "test_server".to_string(),
            tool: "test_tool".to_string(),
            timestamp: 1234567890,
            metadata: HashMap::new(),
        };

        assert_eq!(record.server, "test_server");
        assert_eq!(record.tool, "test_tool");
        assert_eq!(record.timestamp, 1234567890);
    }
}
