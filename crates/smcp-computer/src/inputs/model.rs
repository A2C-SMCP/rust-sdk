/**
* 文件名: model
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: serde, async-trait
* 描述: Inputs相关的数据模型定义
*/
use serde::{Deserialize, Serialize};
use std::fmt;
use thiserror::Error;

/// 输入值 / Input value
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum InputValue {
    /// 字符串值 / String value
    String(String),
    /// 布尔值 / Boolean value
    Bool(bool),
    /// 数字值 / Number value
    Number(i64),
    /// 浮点数值 / Float value
    Float(f64),
}

impl fmt::Display for InputValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InputValue::String(s) => write!(f, "{}", s),
            InputValue::Bool(b) => write!(f, "{}", b),
            InputValue::Number(n) => write!(f, "{}", n),
            InputValue::Float(fl) => write!(f, "{}", fl),
        }
    }
}

impl From<String> for InputValue {
    fn from(s: String) -> Self {
        InputValue::String(s)
    }
}

impl From<&str> for InputValue {
    fn from(s: &str) -> Self {
        InputValue::String(s.to_string())
    }
}

impl From<bool> for InputValue {
    fn from(b: bool) -> Self {
        InputValue::Bool(b)
    }
}

impl From<i64> for InputValue {
    fn from(n: i64) -> Self {
        InputValue::Number(n)
    }
}

impl From<f64> for InputValue {
    fn from(f: f64) -> Self {
        InputValue::Float(f)
    }
}

/// 输入请求 / Input request
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InputRequest {
    /// 输入ID / Input ID
    pub id: String,
    /// 输入类型 / Input type
    pub input_type: InputType,
    /// 标题 / Title
    pub title: String,
    /// 描述 / Description
    pub description: String,
    /// 默认值 / Default value
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<InputValue>,
    /// 是否必填 / Required
    #[serde(default)]
    pub required: bool,
    /// 验证规则 / Validation rules
    #[serde(skip_serializing_if = "Option::is_none")]
    pub validation: Option<ValidationRule>,
}

/// 输入类型 / Input type
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum InputType {
    /// 字符串输入 / String input
    String {
        /// 是否为密码 / Whether it's a password
        #[serde(skip_serializing_if = "Option::is_none")]
        password: Option<bool>,
        /// 最小长度 / Minimum length
        #[serde(skip_serializing_if = "Option::is_none")]
        min_length: Option<usize>,
        /// 最大长度 / Maximum length
        #[serde(skip_serializing_if = "Option::is_none")]
        max_length: Option<usize>,
    },
    /// 选择输入 / Pick string input
    PickString {
        /// 选项列表 / Options list
        options: Vec<String>,
        /// 是否允许多选 / Allow multiple selection
        #[serde(default)]
        multiple: bool,
    },
    /// 数字输入 / Number input
    Number {
        /// 最小值 / Minimum value
        #[serde(skip_serializing_if = "Option::is_none")]
        min: Option<i64>,
        /// 最大值 / Maximum value
        #[serde(skip_serializing_if = "Option::is_none")]
        max: Option<i64>,
    },
    /// 布尔输入 / Boolean input
    Bool {
        /// 真值标签 / True label
        #[serde(skip_serializing_if = "Option::is_none")]
        true_label: Option<String>,
        /// 假值标签 / False label
        #[serde(skip_serializing_if = "Option::is_none")]
        false_label: Option<String>,
    },
    /// 文件路径输入 / File path input
    FilePath {
        /// 是否必须存在 / Must exist
        #[serde(default)]
        must_exist: bool,
        /// 文件类型过滤器 / File type filter
        #[serde(skip_serializing_if = "Option::is_none")]
        filter: Option<String>,
    },
    /// 命令输入 / Command input
    Command {
        /// 命令 / Command
        command: String,
        /// 参数 / Arguments
        #[serde(default)]
        args: Vec<String>,
    },
}

/// 验证规则 / Validation rule
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum ValidationRule {
    /// 正则表达式 / Regular expression
    Regex {
        /// 模式 / Pattern
        pattern: String,
        /// 错误消息 / Error message
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    /// 自定义验证函数 / Custom validation function
    Custom {
        /// 函数名 / Function name
        function: String,
        /// 参数 / Parameters
        #[serde(default)]
        params: std::collections::HashMap<String, serde_json::Value>,
    },
}

/// 输入响应 / Input response
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InputResponse {
    /// 输入ID / Input ID
    pub id: String,
    /// 输入值 / Input value
    pub value: InputValue,
    /// 是否取消 / Cancelled
    #[serde(default)]
    pub cancelled: bool,
}

/// 输入错误 / Input error
#[derive(Debug, Error)]
pub enum InputError {
    /// 无效的输入类型 / Invalid input type
    #[error("Invalid input type: {0}")]
    InvalidType(String),
    /// 验证失败 / Validation failed
    #[error("Validation failed: {0}")]
    ValidationFailed(String),
    /// 取消输入 / Input cancelled
    #[error("Input cancelled")]
    Cancelled,
    /// IO错误 / IO error
    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),
    /// 超时错误 / Timeout error
    #[error("Input timeout")]
    Timeout,
    /// 其他错误 / Other error
    #[error("Other error: {0}")]
    Other(String),
}

/// 输入结果 / Input result
pub type InputResult<T> = Result<T, InputError>;

/// 输入上下文 / Input context
#[derive(Debug, Clone)]
pub struct InputContext {
    /// 服务器名称 / Server name
    pub server_name: Option<String>,
    /// 工具名称 / Tool name
    pub tool_name: Option<String>,
    /// 额外元数据 / Additional metadata
    pub metadata: std::collections::HashMap<String, String>,
}

impl InputContext {
    /// 创建新的输入上下文 / Create new input context
    pub fn new() -> Self {
        Self {
            server_name: None,
            tool_name: None,
            metadata: std::collections::HashMap::new(),
        }
    }

    /// 设置服务器名称 / Set server name
    pub fn with_server_name(mut self, name: String) -> Self {
        self.server_name = Some(name);
        self
    }

    /// 设置工具名称 / Set tool name
    pub fn with_tool_name(mut self, name: String) -> Self {
        self.tool_name = Some(name);
        self
    }

    /// 添加元数据 / Add metadata
    pub fn with_metadata(mut self, key: String, value: String) -> Self {
        self.metadata.insert(key, value);
        self
    }
}

impl Default for InputContext {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn test_input_value_display() {
        assert_eq!(InputValue::String("test".to_string()).to_string(), "test");
        assert_eq!(InputValue::Bool(true).to_string(), "true");
        assert_eq!(InputValue::Number(42).to_string(), "42");
        assert_eq!(InputValue::Float(3.15).to_string(), "3.15");
    }

    #[test]
    fn test_input_value_conversions() {
        // 测试各种类型的转换 / Test conversions of various types
        let string_val: InputValue = "test".into();
        assert_eq!(string_val, InputValue::String("test".to_string()));
        
        let bool_val: InputValue = true.into();
        assert_eq!(bool_val, InputValue::Bool(true));
        
        let number_val: InputValue = 42i64.into();
        assert_eq!(number_val, InputValue::Number(42));
        
        let float_val: InputValue = 3.14f64.into();
        assert_eq!(float_val, InputValue::Float(3.14));
    }

    #[test]
    fn test_input_context() {
        let ctx = InputContext::new()
            .with_server_name("test_server".to_string())
            .with_tool_name("test_tool".to_string())
            .with_metadata("key1".to_string(), "value1".to_string())
            .with_metadata("key2".to_string(), "value2".to_string());
        
        assert_eq!(ctx.server_name, Some("test_server".to_string()));
        assert_eq!(ctx.tool_name, Some("test_tool".to_string()));
        assert_eq!(ctx.metadata.len(), 2);
        assert_eq!(ctx.metadata.get("key1"), Some(&"value1".to_string()));
        assert_eq!(ctx.metadata.get("key2"), Some(&"value2".to_string()));
    }
}
