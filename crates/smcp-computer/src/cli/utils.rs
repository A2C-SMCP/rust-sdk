/*!
* 文件名: utils.rs
* 作者: JQQ
* 创建日期: 2025/12/16
* 最后修改日期: 2025/12/16
* 版权: 2023 JQQ. All rights reserved.
* 依赖: serde_json
* 描述: CLI工具函数 / CLI utility functions
*/

use serde_json::Value;
use std::collections::HashMap;

/// 解析键值对字符串，格式如 "k1:v1,k2:v2"
pub fn parse_kv_pairs(text: &str) -> Result<HashMap<String, String>, String> {
    let mut map = HashMap::new();
    
    if text.is_empty() {
        return Ok(map);
    }
    
    // 尝试解析为 JSON
    if let Ok(json_value) = serde_json::from_str::<Value>(text) {
        if let Value::Object(obj) = json_value {
            for (k, v) in obj {
                if let Value::String(s) = v {
                    map.insert(k, s);
                } else {
                    map.insert(k, v.to_string());
                }
            }
            return Ok(map);
        }
    }
    
    // 解析为键值对格式
    for pair in text.split(',') {
        let pair = pair.trim();
        if let Some((key, value)) = pair.split_once(':') {
            map.insert(key.trim().to_string(), value.trim().to_string());
        } else {
            return Err(format!("Invalid key-value pair: {}", pair));
        }
    }
    
    Ok(map)
}

/// 解析导入目标字符串
pub fn resolve_import_target(target: &str) -> Result<String, String> {
    // 简化版本，返回目标字符串
    // TODO: 实现动态导入功能
    Ok(target.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_kv_pairs() {
        // 测试键值对格式
        let result = parse_kv_pairs("key1:value1,key2:value2").unwrap();
        assert_eq!(result.get("key1"), Some(&"value1".to_string()));
        assert_eq!(result.get("key2"), Some(&"value2".to_string()));
        
        // 测试 JSON 格式
        let result = parse_kv_pairs(r#"{"key1":"value1","key2":"value2"}"#).unwrap();
        assert_eq!(result.get("key1"), Some(&"value1".to_string()));
        assert_eq!(result.get("key2"), Some(&"value2".to_string()));
        
        // 测试空字符串
        let result = parse_kv_pairs("").unwrap();
        assert!(result.is_empty());
    }
}
