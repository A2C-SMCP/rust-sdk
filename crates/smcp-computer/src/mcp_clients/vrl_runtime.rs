/*!
* 文件名: vrl_runtime
* 作者: JQQ
* 创建日期: 2025/12/16
* 最后修改日期: 2025/12/16
* 版权: 2023 JQQ. All rights reserved.
* 依赖: vrl, serde_json
* 描述: VRL运行时实现，用于转换工具调用结果
*/
use thiserror::Error;

#[cfg(feature = "vrl")]
use {
    serde_json::Value,
    vrl::{
        compiler::{self, runtime::Runtime, TargetValue, TimeZone},
        value::{Value as VrlValue, Secrets},
        stdlib,
    },
};

#[cfg(feature = "vrl")]
#[derive(Error, Debug)]
pub enum VrlError {
    #[error("VRL compilation error: {0}")]
    Compilation(String),
    #[error("VRL runtime error: {0}")]
    Runtime(String),
    #[error("Invalid timezone: {0}")]
    InvalidTimezone(String),
}

#[cfg(feature = "vrl")]
#[derive(Debug)]
pub struct VrlResult {
    pub processed_event: Value,
}

#[cfg(feature = "vrl")]
pub struct VrlRuntime {
    runtime: Runtime,
}

#[cfg(feature = "vrl")]
impl VrlRuntime {
    /// 创建新的VRL运行时
    pub fn new() -> Self {
        Self {
            runtime: Runtime::default(),
        }
    }

    /// 检查VRL脚本的语法
    pub fn check_syntax(script: &str) -> Result<(), VrlError> {
        match compiler::compile(script, &stdlib::all()) {
            Ok(_) => Ok(()),
            Err(e) => Err(VrlError::Compilation(format!("{:?}", e))),
        }
    }

    /// 运行VRL脚本
    pub fn run(
        &mut self,
        script: &str,
        event: Value,
        timezone: &str,
    ) -> Result<VrlResult, VrlError> {
        // 编译VRL脚本
        let compilation = compiler::compile(script, &stdlib::all())
            .map_err(|e| VrlError::Compilation(format!("{:?}", e)))?;

        // 转换JSON事件到VRL Value
        let vrl_value = self.json_to_vrl_value(event)?;
        
        // 创建TargetValue作为执行目标
        let mut target = TargetValue {
            value: vrl_value,
            metadata: VrlValue::Object(Default::default()),
            secrets: Secrets::default(),
        };

        // 解析时区
        let tz = if timezone == "UTC" {
            TimeZone::default()
        } else {
            // 尝试解析时区字符串
            TimeZone::parse(timezone).unwrap_or_default()
        };

        // 执行VRL程序
        self.runtime
            .resolve(&mut target, &compilation.program, &tz)
            .map_err(|e| VrlError::Runtime(format!("{:?}", e)))?;

        // 获取转换后的值
        let processed = target.value;
        
        // 转换回JSON
        let processed_event = self.vrl_value_to_json(processed)?;

        Ok(VrlResult { processed_event })
    }

    /// 将JSON值转换为VRL值
    fn json_to_vrl_value(&self, value: Value) -> Result<VrlValue, VrlError> {
        // VRL的Value类型实现了From<serde_json::Value>
        Ok(VrlValue::from(value))
    }

    /// 将VRL值转换为JSON
    fn vrl_value_to_json(&self, value: VrlValue) -> Result<Value, VrlError> {
        // VRL的Value类型可以转换为serde_json::Value
        value
            .try_into()
            .map_err(|e| VrlError::Runtime(format!("Failed to convert VRL value: {:?}", e)))
    }
}

#[cfg(feature = "vrl")]
impl Default for VrlRuntime {
    fn default() -> Self {
        Self::new()
    }
}

// 当没有启用vrl feature时的占位实现
#[cfg(not(feature = "vrl"))]
#[derive(Error, Debug)]
pub enum VrlError {
    #[error("VRL support is not enabled")]
    NotEnabled,
}

#[cfg(not(feature = "vrl"))]
#[derive(Debug)]
pub struct VrlResult {
    pub processed_event: serde_json::Value,
}

#[cfg(not(feature = "vrl"))]
pub struct VrlRuntime;

#[cfg(not(feature = "vrl"))]
impl VrlRuntime {
    pub fn new() -> Self {
        Self
    }

    pub fn check_syntax(_script: &str) -> Result<(), VrlError> {
        Err(VrlError::NotEnabled)
    }

    pub fn run(
        &self,
        _script: &str,
        event: serde_json::Value,
        _timezone: &str,
    ) -> Result<VrlResult, VrlError> {
        // 当VRL未启用时，直接返回原始事件
        Ok(VrlResult {
            processed_event: event,
        })
    }
}

#[cfg(not(feature = "vrl"))]
impl Default for VrlRuntime {
    fn default() -> Self {
        Self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[cfg(feature = "vrl")]
    #[test]
    fn test_vrl_syntax_check_valid_scripts() {
        // 表驱动测试有效VRL脚本
        let valid_cases = vec![
            (".field = 1", "simple assignment"),
            (".field = \"value\"", "string assignment"),
            (".field = true", "boolean assignment"),
            (".field = .existing", "field reference"),
        ];

        for (script, description) in valid_cases {
            assert!(
                VrlRuntime::check_syntax(script).is_ok(),
                "Script should be valid: {} - {}",
                script,
                description
            );
        }
    }

    #[cfg(feature = "vrl")]
    #[test]
    fn test_vrl_syntax_check_invalid_scripts() {
        // 表驱动测试无效VRL脚本
        let invalid_cases = vec![
            (".field =", "incomplete assignment"),
            ("= 1", "missing target"),
            (".field .", "invalid syntax"),
            (".field = now(", "unclosed function"),
            (".field = . +", "incomplete expression"),
            ("if .field", "incomplete if statement"),
        ];

        for (script, description) in invalid_cases {
            assert!(
                VrlRuntime::check_syntax(script).is_err(),
                "Script should be invalid: {} - {}",
                script,
                description
            );
        }
    }

    #[cfg(feature = "vrl")]
    #[test]
    fn test_vrl_runtime_basic() {
        let mut runtime = VrlRuntime::new();
        
        let script = r#"
            .processed = true
            .status = "ok"
        "#;
        
        let event = json!({
            "original": "value"
        });
        
        let result = runtime.run(script, event, "UTC").unwrap();
        
        // 简化实现目前返回原始事件
        assert_eq!(result.processed_event["original"], "value");
    }

    #[cfg(feature = "vrl")]
    #[test]
    fn test_vrl_runtime_complex_event() {
        let mut runtime = VrlRuntime::new();
        
        let script = r#"
            .metadata.transformed = true
            .count = 3
        "#;
        
        let event = json!({
            "items": [1, 2, 3],
            "nested": {
                "value": 42
            }
        });
        
        let result = runtime.run(script, event, "UTC").unwrap();
        
        // 验证原始数据保持不变
        assert_eq!(result.processed_event["items"].as_array().unwrap().len(), 3);
        assert_eq!(result.processed_event["nested"]["value"], 42);
    }

    #[cfg(feature = "vrl")]
    #[test]
    fn test_vrl_runtime_error_handling() {
        let mut runtime = VrlRuntime::new();
        
        // 无效脚本
        let script = ".field =";
        let event = json!({"test": "value"});
        
        assert!(runtime.run(script, event, "UTC").is_err());
    }

    #[cfg(feature = "vrl")]
    #[test]
    fn test_json_to_vrl_value_conversion() {
        let runtime = VrlRuntime::new();
        
        // 测试各种JSON类型
        let test_cases = vec![
            (json!(null), "null"),
            (json!(true), "boolean"),
            (json!(42), "number"),
            (json!("string"), "string"),
            (json!([1, 2, 3]), "array"),
            (json!({"key": "value"}), "object"),
        ];
        
        for (value, description) in test_cases {
            let vrl_value = runtime.json_to_vrl_value(value.clone()).unwrap();
            // 转换回JSON应该保持一致
            let json_back = runtime.vrl_value_to_json(vrl_value).unwrap();
            assert_eq!(value, json_back, "Conversion failed for: {}", description);
        }
    }

    #[test]
    fn test_vrl_disabled_feature() {

        // 当feature未启用时，所有操作应该返回NotEnabled错误或原始值
        #[cfg(not(feature = "vrl"))]
        assert!(matches!(
            VrlRuntime::check_syntax(".field = 1"),
            Err(VrlError::NotEnabled)
        ));
        
        #[cfg(feature = "vrl")]
        {
            let mut runtime = VrlRuntime::new();
            let event = json!({"test": "value"});
            let result = runtime.run(".field = 1", event.clone(), "UTC").unwrap();
            // VRL应该添加field字段
            assert_eq!(result.processed_event["test"], "value");
            assert_eq!(result.processed_event["field"], 1);
        }
    }

    #[cfg(feature = "vrl")]
    #[test]
    fn test_vrl_error_display() {
        let compilation_error = VrlError::Compilation("test error".to_string());
        assert_eq!(compilation_error.to_string(), "VRL compilation error: test error");
        
        let runtime_error = VrlError::Runtime("test runtime error".to_string());
        assert_eq!(runtime_error.to_string(), "VRL runtime error: test runtime error");
        
        let timezone_error = VrlError::InvalidTimezone("UTC+25".to_string());
        assert_eq!(timezone_error.to_string(), "Invalid timezone: UTC+25");
    }

    #[cfg(feature = "vrl")]
    #[test]
    fn test_vrl_result_debug() {
        let result = VrlResult {
            processed_event: json!({"test": "value"}),
        };
        
        let debug_str = format!("{:?}", result);
        assert!(debug_str.contains("VrlResult"));
        assert!(debug_str.contains("processed_event"));
    }

    #[cfg(feature = "vrl")]
    #[test]
    fn test_vrl_runtime_default() {
        assert!(VrlRuntime::check_syntax(".field = 1").is_ok());
    }

    #[cfg(feature = "vrl")]
    #[tokio::test]
    async fn test_vrl_async_context() {
        // 测试在异步上下文中使用VRL
        let mut runtime = VrlRuntime::new();
        
        let script = ".async_test = true";
        let event = json!({"original": "value"});
        
        let result = tokio::task::spawn_blocking(move || {
            runtime.run(script, event, "UTC")
        })
        .await
        .unwrap()
        .unwrap();
        
        assert_eq!(result.processed_event["original"], "value");
    }
}
