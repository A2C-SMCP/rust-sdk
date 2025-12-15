/*!
* 文件名: types.rs
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: serde, chrono
* 描述: 核心类型定义 / Core type definitions
*/

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// 工具调用历史记录 / Tool call history record
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRecord {
    /// 时间戳 / Timestamp
    pub timestamp: DateTime<Utc>,
    /// 请求ID / Request ID
    pub req_id: String,
    /// 服务器名称 / Server name
    pub server: String,
    /// 工具名称 / Tool name
    pub tool: String,
    /// 参数 / Parameters
    pub parameters: serde_json::Value,
    /// 超时时间 / Timeout
    pub timeout: Option<f64>,
    /// 是否成功 / Success
    pub success: bool,
    /// 错误信息 / Error message
    pub error: Option<String>,
}

/// 工具调用历史管理器 / Tool call history manager
#[derive(Debug)]
pub struct ToolCallHistory {
    /// 历史记录队列 / History queue
    records: VecDeque<ToolCallRecord>,
    /// 最大记录数 / Maximum records
    max_size: usize,
}

impl ToolCallHistory {
    /// 创建新的历史记录管理器 / Create new history manager
    pub fn new(max_size: usize) -> Self {
        Self {
            records: VecDeque::with_capacity(max_size),
            max_size,
        }
    }

    /// 添加记录 / Add record
    pub fn push(&mut self, record: ToolCallRecord) {
        if self.records.len() >= self.max_size {
            self.records.pop_front();
        }
        self.records.push_back(record);
    }

    /// 获取所有记录 / Get all records
    pub fn get_all(&self) -> Vec<ToolCallRecord> {
        self.records.iter().cloned().collect()
    }

    /// 清空记录 / Clear records
    pub fn clear(&mut self) {
        self.records.clear();
    }
}

impl Default for ToolCallHistory {
    fn default() -> Self {
        Self::new(10)
    }
}
