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

/// 窗口URI信息 / Window URI information
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WindowURI {
    /// 原始URI字符串 / Original URI string
    pub uri: String,
    /// 优先级 / Priority
    pub priority: Option<i32>,
    /// 是否全屏 / Whether fullscreen
    pub fullscreen: Option<bool>,
    /// 额外参数 / Additional parameters
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub params: HashMap<String, String>,
}

impl WindowURI {
    /// 创建新的窗口URI / Create new window URI
    pub fn new(uri: String) -> Self {
        Self {
            uri,
            priority: None,
            fullscreen: None,
            params: HashMap::new(),
        }
    }

    /// 从字符串解析 / Parse from string
    pub fn parse(uri: &str) -> Result<Self, WindowURIError> {
        use std::collections::HashMap;

        // 解析URI
        let url = url::Url::parse(uri).map_err(|e| WindowURIError::InvalidFormat(e.to_string()))?;

        // 检查scheme
        if url.scheme() != "window" {
            return Err(WindowURIError::InvalidFormat(format!(
                "Invalid scheme: {}, expected 'window'",
                url.scheme()
            )));
        }

        // 检查host
        if url.host().is_none() {
            return Err(WindowURIError::InvalidFormat(
                "Missing host (MCP id)".to_string(),
            ));
        }

        // 解析查询参数
        let mut params = HashMap::new();
        for (key, value) in url.query_pairs() {
            params.insert(key.to_string(), value.to_string());
        }

        // 解析priority
        let priority = if let Some(val) = params.get("priority") {
            match val.parse::<i32>() {
                Ok(p) if (0..=100).contains(&p) => Some(p),
                _ => {
                    return Err(WindowURIError::ParseError(format!(
                        "Invalid priority value: {}",
                        val
                    )))
                }
            }
        } else {
            None
        };

        // 解析fullscreen
        let fullscreen = if let Some(val) = params.get("fullscreen") {
            match val.to_lowercase().as_str() {
                "1" | "true" | "yes" | "on" => Some(true),
                "0" | "false" | "no" | "off" => Some(false),
                _ => {
                    return Err(WindowURIError::ParseError(format!(
                        "Invalid fullscreen value: {}",
                        val
                    )))
                }
            }
        } else {
            None
        };

        Ok(Self {
            uri: uri.to_string(),
            priority,
            fullscreen,
            params,
        })
    }

    /// 设置优先级 / Set priority
    pub fn with_priority(mut self, priority: i32) -> Self {
        self.priority = Some(priority);
        self
    }

    /// 设置全屏 / Set fullscreen
    pub fn with_fullscreen(mut self, fullscreen: bool) -> Self {
        self.fullscreen = Some(fullscreen);
        self
    }
}

/// 窗口URI错误 / Window URI error
#[derive(Debug, thiserror::Error)]
pub enum WindowURIError {
    /// 无效的URI格式 / Invalid URI format
    #[error("Invalid URI format: {0}")]
    InvalidFormat(String),
    /// 解析错误 / Parse error
    #[error("Parse error: {0}")]
    ParseError(String),
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
    fn test_window_uri() {
        let uri = WindowURI::new("window://test.mcp.com/window1".to_string())
            .with_priority(10)
            .with_fullscreen(true);

        assert_eq!(uri.uri, "window://test.mcp.com/window1");
        assert_eq!(uri.priority, Some(10));
        assert_eq!(uri.fullscreen, Some(true));
    }

    #[test]
    fn test_window_uri_parse() {
        let uri =
            WindowURI::parse("window://test.mcp.com/window1?priority=50&fullscreen=true").unwrap();
        assert_eq!(
            uri.uri,
            "window://test.mcp.com/window1?priority=50&fullscreen=true"
        );
        assert_eq!(uri.priority, Some(50));
        assert_eq!(uri.fullscreen, Some(true));
    }

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
