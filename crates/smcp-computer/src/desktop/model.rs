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
pub use crate::mcp_clients::model::{ReadResourceResult, Resource, ResourceContents};

/// 服务器名称类型 / Server name type
pub type ServerName = String;

/// 工具调用记录 / Tool call record
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCallRecord {
    /// 服务器身份键（`bundle_id`）——desktop 分组/最近排序按此关联（协议 0.3.0 §身份正交性 #18）。
    /// Server identity key (bundle_id); desktop grouping/recency correlates on this。
    pub bundle_id: String,
    /// 服务器展示名（display，非身份）/ Server display name (not identity)。
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
    /// 服务器身份键（`bundle_id`）——desktop **分组键**（协议 0.3.0 §身份正交性 #18：避免同名 server 窗口误并）。
    /// Server identity key (bundle_id); the desktop **grouping key**。
    pub bundle_id: String,
    /// 服务器展示名（display，非身份、非分组键）/ Server display name (not identity/grouping key)。
    pub server_name: ServerName,
    /// 资源 / Resource
    pub resource: Resource,
    /// 读取结果 / Read result
    pub read_result: ReadResourceResult,
}

impl WindowInfo {
    /// 创建新的窗口信息 / Create new window info
    pub fn new(
        bundle_id: String,
        server_name: ServerName,
        resource: Resource,
        read_result: ReadResourceResult,
    ) -> Self {
        Self {
            bundle_id,
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
        let resource = crate::mcp_clients::model::make_resource(
            "window://test.mcp.com/window1",
            "Test Window",
            Some("Test window".to_string()),
            None,
        );

        let read_result = ReadResourceResult {
            contents: vec![ResourceContents::text(
                "Test content",
                "window://test.mcp.com/window1",
            )],
        };

        let window_info = WindowInfo::new(
            "test_server".to_string(),
            "test_server".to_string(),
            resource,
            read_result,
        );

        assert_eq!(window_info.server_name, "test_server");
        assert_eq!(window_info.resource.name, "Test Window");
        assert_eq!(window_info.read_result.contents.len(), 1);
    }

    #[test]
    fn test_tool_call_record() {
        let record = ToolCallRecord {
            bundle_id: "test_server".to_string(),
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
