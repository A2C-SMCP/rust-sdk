/*!
* 文件名: render.rs
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: regex, async-trait
* 描述: 配置渲染器 / Configuration renderer
*/

use regex::Regex;
use serde_json::Value;
use std::collections::HashMap;
use crate::inputs::resolver::InputResolver;

lazy_static::lazy_static! {
    /// 占位符模式正则 / Placeholder pattern regex
    static ref PLACEHOLDER_PATTERN: Regex = Regex::new(r"\$\{input:([^}]+)}").unwrap();
}

/// 配置渲染器 / Configuration renderer
pub struct ConfigRender {
    /// 最大递归深度 / Maximum recursion depth
    max_depth: usize,
}

impl ConfigRender {
    /// 创建新的渲染器 / Create new renderer
    pub fn new() -> Self {
        Self { max_depth: 10 }
    }

    /// 设置最大递归深度 / Set maximum recursion depth
    pub fn with_max_depth(mut self, max_depth: usize) -> Self {
        self.max_depth = max_depth;
        self
    }

    /// 渲染配置数据 / Render configuration data
    pub async fn render<R>(
        &self,
        data: Value,
        resolver: &R,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>>
    where
        R: InputResolver + ?Sized,
    {
        self.render_internal(data, resolver, 0).await
    }

    /// 内部渲染实现 / Internal render implementation
    async fn render_internal<R>(
        &self,
        data: Value,
        resolver: &R,
        depth: usize,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>>
    where
        R: InputResolver + ?Sized,
    {
        if depth > self.max_depth {
            return Err("渲染深度超过限制 / Rendering depth exceeded".into());
        }

        match data {
            Value::Object(map) => {
                let mut result = HashMap::new();
                for (k, v) in map {
                    let rendered = Box::pin(self.render_internal(v, resolver, depth + 1)).await?;
                    result.insert(k, rendered);
                }
                Ok(Value::Object(result.into_iter().collect()))
            }
            Value::Array(arr) => {
                let mut result = Vec::with_capacity(arr.len());
                for v in arr {
                    let rendered = Box::pin(self.render_internal(v, resolver, depth + 1)).await?;
                    result.push(rendered);
                }
                Ok(Value::Array(result))
            }
            Value::String(s) => self.render_string(&s, resolver).await,
            _ => Ok(data),
        }
    }

    /// 渲染字符串 / Render string
    async fn render_string<R>(
        &self,
        s: &str,
        resolver: &R,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>>
    where
        R: InputResolver + ?Sized,
    {
        let matches: Vec<_> = PLACEHOLDER_PATTERN.find_iter(s).collect();
        if matches.is_empty() {
            return Ok(Value::String(s.to_string()));
        }

        // 如果字符串只包含一个占位符且没有其他字符
        // If the string contains only one placeholder and no other characters
        if matches.len() == 1 && matches[0].start() == 0 && matches[0].end() == s.len() {
            let input_id = matches[0].as_str().trim_start_matches("${input:").trim_end_matches('}');
            match resolver.resolve(input_id).await {
                Ok(value) => return Ok(value.into()),
                Err(_) => {
                    eprintln!("未找到输入项: {} / Input id not found: {}", input_id, input_id);
                    return Ok(Value::String(s.to_string()));
                }
            }
        }

        // 否则逐个替换占位符
        // Otherwise replace placeholders one by one
        let mut result = s.to_string();
        let mut offset = 0;

        for m in matches {
            let (start, end) = (m.start(), m.end());
            let input_id = m.as_str().trim_start_matches("${input:").trim_end_matches('}');
            
            match resolver.resolve(input_id).await {
                Ok(value) => {
                    let repl = match value.into() {
                        Value::String(s) => s,
                        v => v.to_string(),
                    };
                    
                    result.replace_range(start + offset..end + offset, &repl);
                    offset += repl.len() - (end - start);
                }
                Err(_) => {
                    eprintln!("解析输入失败: {}, 错误: 未找到", input_id);
                    // 保持原值
                }
            }
        }

        Ok(Value::String(result))
    }
}

impl Default for ConfigRender {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use crate::inputs::model::InputValue;
    use crate::inputs::resolver::InputResolverError;
    use async_trait::async_trait;

    struct MockResolver {
        values: HashMap<String, Value>,
        cache: HashMap<String, InputValue>,
    }

    #[async_trait]
    impl InputResolver for MockResolver {
        async fn resolve(&self, input_id: &str) -> Result<InputValue, InputResolverError> {
            match self.values.get(input_id) {
                Some(v) => Ok(v.clone().into()),
                None => Err(InputResolverError::InputNotFound { input_id: input_id.to_string() }),
            }
        }

        async fn set_cached_value(&self, _input_id: &str, _value: InputValue) -> bool {
            // For test, just return true
            true
        }

        async fn get_cached_value(&self, input_id: &str) -> Option<InputValue> {
            self.cache.get(input_id).cloned()
        }

        async fn delete_cached_value(&self, _input_id: &str) -> bool {
            // For test, just return true
            true
        }

        async fn clear_cache(&self, _input_id: Option<&str>) {
            // For test, do nothing
        }
    }

    #[tokio::test]
    async fn test_render_string() {
        let mut values = HashMap::new();
        values.insert("name".to_string(), Value::String("world".to_string()));
        let resolver = MockResolver { values, cache: HashMap::new() };
        let renderer = ConfigRender::new();

        let result = renderer
            .render(Value::String("Hello ${input:name}!".to_string()), &resolver)
            .await
            .unwrap();

        assert_eq!(result, Value::String("Hello world!".to_string()));
    }

    #[tokio::test]
    async fn test_render_single_placeholder() {
        let mut values = HashMap::new();
        values.insert("number".to_string(), Value::Number(42.into()));
        let resolver = MockResolver { values, cache: HashMap::new() };
        let renderer = ConfigRender::new();

        let result = renderer
            .render(Value::String("${input:number}".to_string()), &resolver)
            .await
            .unwrap();

        assert_eq!(result, Value::Number(42.into()));
    }
}
