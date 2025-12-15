/*!
* 文件名: model.rs
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: serde
* 描述: 输入模型定义 / Input model definitions
*/

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// 输入值类型 / Input value type
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum InputValue {
    /// 字符串值 / String value
    String(String),
    /// 数字值 / Number value
    Number(serde_json::Number),
    /// 布尔值 / Boolean value
    Boolean(bool),
    /// 对象值 / Object value
    Object(HashMap<String, serde_json::Value>),
    /// 数组值 / Array value
    Vec(Vec<serde_json::Value>),
    /// 空值 / Null value
    Null,
}

impl From<serde_json::Value> for InputValue {
    fn from(value: serde_json::Value) -> Self {
        match value {
            serde_json::Value::String(s) => InputValue::String(s),
            serde_json::Value::Number(n) => InputValue::Number(n),
            serde_json::Value::Bool(b) => InputValue::Boolean(b),
            serde_json::Value::Object(o) => InputValue::Object(o.into_iter().collect()),
            serde_json::Value::Array(a) => InputValue::Vec(a),
            serde_json::Value::Null => InputValue::Null,
        }
    }
}

impl From<InputValue> for serde_json::Value {
    fn from(value: InputValue) -> Self {
        match value {
            InputValue::String(s) => serde_json::Value::String(s),
            InputValue::Number(n) => serde_json::Value::Number(n),
            InputValue::Boolean(b) => serde_json::Value::Bool(b),
            InputValue::Object(o) => serde_json::Value::Object(o.into_iter().collect()),
            InputValue::Vec(a) => serde_json::Value::Array(a),
            InputValue::Null => serde_json::Value::Null,
        }
    }
}

/// 输入缓存项 / Input cache item
#[derive(Debug, Clone)]
pub struct InputCacheItem {
    /// 值 / Value
    pub value: InputValue,
    /// 解析时间 / Resolved time
    pub resolved_at: chrono::DateTime<chrono::Utc>,
}

impl InputCacheItem {
    /// 创建新的缓存项 / Create new cache item
    pub fn new(value: InputValue) -> Self {
        Self {
            value,
            resolved_at: chrono::Utc::now(),
        }
    }
}
