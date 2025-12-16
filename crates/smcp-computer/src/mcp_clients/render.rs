/**
* 文件名: render
* 作者: JQQ
* 创建日期: 2025/12/16
* 最后修改日期: 2025/12/16
* 版权: 2023 JQQ. All rights reserved.
* 依赖: serde_json, regex, async-trait
* 描述: 配置渲染器，支持 ${input:xxx} 占位符解析
*/
use async_recursion::async_recursion;
use regex::Regex;
use serde_json::Value;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum RenderError {
    #[error("Input not found: {0}")]
    InputNotFound(String),
    #[error("Render depth exceeded")]
    DepthExceeded,
    #[error("Invalid placeholder format")]
    InvalidPlaceholder,
}

/// 配置渲染器，用于处理 ${input:xxx} 占位符
pub struct ConfigRender {
    placeholder_regex: Regex,
    max_depth: usize,
}

impl ConfigRender {
    /// 创建新的配置渲染器
    pub fn new(max_depth: usize) -> Self {
        Self {
            placeholder_regex: Regex::new(r"\$\{input:([^}]+)}").unwrap(),
            max_depth,
        }
    }

    /// 渲染配置值
    pub async fn render<F, Fut>(&self, data: Value, resolver: F) -> Result<Value, RenderError>
    where
        F: Fn(String) -> Fut + Copy + Send + Sync,
        Fut: std::future::Future<Output = Result<Value, RenderError>> + Send,
    {
        self.render_with_depth(data, resolver, 0).await
    }

    #[async_recursion]
    async fn render_with_depth<F, Fut>(
        &self,
        data: Value,
        resolver: F,
        depth: usize,
    ) -> Result<Value, RenderError>
    where
        F: Fn(String) -> Fut + Copy + Send + Sync,
        Fut: std::future::Future<Output = Result<Value, RenderError>> + Send,
    {
        if depth > self.max_depth {
            return Err(RenderError::DepthExceeded);
        }

        match data {
            Value::String(s) => self.render_string(s, resolver, depth).await,
            Value::Object(mut map) => {
                for (k, v) in map.clone() {
                    map.insert(k, self.render_with_depth(v, resolver, depth + 1).await?);
                }
                Ok(Value::Object(map))
            }
            Value::Array(arr) => {
                let mut new_arr = Vec::with_capacity(arr.len());
                for item in arr {
                    new_arr.push(self.render_with_depth(item, resolver, depth + 1).await?);
                }
                Ok(Value::Array(new_arr))
            }
            _ => Ok(data),
        }
    }

    async fn render_string<F, Fut>(
        &self,
        s: String,
        resolver: F,
        _depth: usize,
    ) -> Result<Value, RenderError>
    where
        F: Fn(String) -> Fut + Copy + Send + Sync,
        Fut: std::future::Future<Output = Result<Value, RenderError>> + Send,
    {
        let matches: Vec<_> = self.placeholder_regex.find_iter(&s).collect();

        if matches.is_empty() {
            return Ok(Value::String(s));
        }

        // 如果字符串是单个占位符，直接返回解析后的值（可能不是字符串）
        if matches.len() == 1 && matches[0].start() == 0 && matches[0].end() == s.len() {
            let input_id = matches[0]
                .as_str()
                .strip_prefix("${input:")
                .unwrap()
                .strip_suffix('}')
                .unwrap();
            return match resolver(input_id.to_string()).await {
                Ok(value) => Ok(value),
                Err(RenderError::InputNotFound(_)) => {
                    // 未找到输入，返回原字符串
                    Ok(Value::String(s))
                }
                Err(e) => Err(e),
            };
        }

        // 处理字符串中的多个占位符
        let mut result = s.clone();
        let mut offset: isize = 0;

        for m in matches {
            let input_id = m
                .as_str()
                .strip_prefix("${input:")
                .unwrap()
                .strip_suffix('}')
                .unwrap();

            let replacement = match resolver(input_id.to_string()).await {
                Ok(value) => match value {
                    Value::String(s) => s,
                    other => other.to_string(),
                },
                Err(RenderError::InputNotFound(_)) => {
                    // 未找到输入，保留原占位符
                    m.as_str().to_string()
                }
                Err(e) => return Err(e),
            };

            let start = (m.start() as isize + offset) as usize;
            let end = (m.end() as isize + offset) as usize;
            result.replace_range(start..end, &replacement);
            offset += replacement.len() as isize - (m.end() - m.start()) as isize;
        }

        Ok(Value::String(result))
    }
}

impl Default for ConfigRender {
    fn default() -> Self {
        Self::new(10)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn mock_resolver(id: String) -> Result<Value, RenderError> {
        match id.as_str() {
            "test" => Ok(Value::String("resolved".to_string())),
            "number" => Ok(Value::Number(serde_json::Number::from(42))),
            "missing" => Err(RenderError::InputNotFound(id)),
            _ => Ok(Value::String(format!("resolved_{}", id))),
        }
    }

    #[tokio::test]
    async fn test_simple_placeholder() {
        let render = ConfigRender::default();
        let input = Value::String("${input:test}".to_string());
        let result = render.render(input, mock_resolver).await.unwrap();
        assert_eq!(result, Value::String("resolved".to_string()));
    }

    #[tokio::test]
    async fn test_multiple_placeholders() {
        let render = ConfigRender::default();
        let input = Value::String("Hello ${input:test} and ${input:world}".to_string());
        let result = render.render(input, mock_resolver).await.unwrap();
        assert_eq!(
            result,
            Value::String("Hello resolved and resolved_world".to_string())
        );
    }

    #[tokio::test]
    async fn test_missing_input() {
        let render = ConfigRender::default();
        let input = Value::String("${input:missing}".to_string());
        let result = render.render(input, mock_resolver).await.unwrap();
        assert_eq!(result, Value::String("${input:missing}".to_string()));
    }

    #[tokio::test]
    async fn test_object_render() {
        let render = ConfigRender::default();
        let mut obj = serde_json::Map::new();
        obj.insert(
            "key".to_string(),
            Value::String("${input:test}".to_string()),
        );
        obj.insert("nested".to_string(), Value::String("value".to_string()));
        let input = Value::Object(obj);
        let result = render.render(input, mock_resolver).await.unwrap();

        if let Value::Object(map) = result {
            assert_eq!(
                map.get("key").unwrap(),
                &Value::String("resolved".to_string())
            );
            assert_eq!(
                map.get("nested").unwrap(),
                &Value::String("value".to_string())
            );
        } else {
            panic!("Expected object");
        }
    }
}
