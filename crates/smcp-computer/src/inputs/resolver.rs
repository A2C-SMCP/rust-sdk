/*!
* 文件名: resolver.rs
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: async-trait, tokio
* 描述: 输入解析器 / Input resolver
*/

use crate::inputs::model::InputValue;
use crate::mcp_clients::model::MCPServerInput;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// 输入解析器错误 / Input resolver error
#[derive(Debug, thiserror::Error)]
pub enum InputResolverError {
    #[error("Input not found: {input_id}")]
    /// 输入未找到 / Input not found
    InputNotFound { input_id: String },
    
    #[error("Invalid input type: {input_type}")]
    /// 无效输入类型 / Invalid input type
    InvalidInputType { input_type: String },
    
    #[error("Command execution failed: {0}")]
    /// 命令执行失败 / Command execution failed
    CommandError(String),
    
    #[error("IO error: {0}")]
    /// IO错误 / IO error
    IoError(#[from] std::io::Error),
}

/// 输入解析器trait / Input resolver trait
#[async_trait]
pub trait InputResolver: Send + Sync {
    /// 解析输入值 / Resolve input value
    async fn resolve(&self, input_id: &str) -> Result<InputValue, InputResolverError>;
    
    /// 设置缓存值 / Set cached value
    async fn set_cached_value(&self, input_id: &str, value: InputValue) -> bool;
    
    /// 获取缓存值 / Get cached value
    async fn get_cached_value(&self, input_id: &str) -> Option<InputValue>;
    
    /// 删除缓存值 / Delete cached value
    async fn delete_cached_value(&self, input_id: &str) -> bool;
    
    /// 清空缓存 / Clear cache
    async fn clear_cache(&self, input_id: Option<&str>);
}

/// 基础输入解析器 / Base input resolver
pub struct BaseInputResolver {
    /// 输入定义 / Input definitions
    inputs: HashMap<String, MCPServerInput>,
    /// 值缓存 / Value cache
    cache: Arc<RwLock<HashMap<String, InputValue>>>,
}

impl BaseInputResolver {
    /// 创建新的解析器 / Create new resolver
    pub fn new(inputs: Vec<MCPServerInput>) -> Self {
        let mut input_map = HashMap::new();
        for input in inputs {
            input_map.insert(input.id().to_string(), input);
        }
        
        Self {
            inputs: input_map,
            cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }
    
    /// 添加输入定义 / Add input definition
    pub async fn add_input(&mut self, input: MCPServerInput) {
        self.inputs.insert(input.id().to_string(), input);
    }
    
    /// 移除输入定义 / Remove input definition
    pub async fn remove_input(&mut self, input_id: &str) -> Option<MCPServerInput> {
        self.inputs.remove(input_id)
    }
    
    /// 执行命令 / Execute command
    async fn execute_command(&self, command: &str, args: Option<&HashMap<String, String>>) -> Result<InputValue, InputResolverError> {
        use tokio::process::Command;
        
        let mut cmd = Command::new(command);
        
        if let Some(args) = args {
            for (key, value) in args {
                cmd.arg(format!("{}={}", key, value));
            }
        }
        
        let output = cmd.output().await.map_err(InputResolverError::IoError)?;
        
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            Ok(InputValue::String(stdout.trim().to_string()))
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(InputResolverError::CommandError(stderr.to_string()))
        }
    }
}

#[async_trait]
impl InputResolver for BaseInputResolver {
    async fn resolve(&self, input_id: &str) -> Result<InputValue, InputResolverError> {
        // 先检查缓存
        if let Some(value) = self.get_cached_value(input_id).await {
            return Ok(value);
        }
        
        // 获取输入定义
        let input = self.inputs.get(input_id)
            .ok_or_else(|| InputResolverError::InputNotFound { 
                input_id: input_id.to_string() 
            })?;
        
        // 根据类型解析
        let value = match input {
            MCPServerInput::PromptString(input) => {
                // 基础实现返回默认值或空字符串
                // 实际实现应该在CLI解析器中处理用户输入
                if let Some(default) = &input.default {
                    InputValue::String(default.clone())
                } else {
                    InputValue::String(String::new())
                }
            }
            MCPServerInput::PickString(input) => {
                // 基础实现返回默认值或第一个选项
                if let Some(default) = &input.default {
                    InputValue::String(default.clone())
                } else if !input.options.is_empty() {
                    InputValue::String(input.options[0].clone())
                } else {
                    InputValue::String(String::new())
                }
            }
            MCPServerInput::Command(input) => {
                self.execute_command(&input.command, input.args.as_ref()).await?
            }
        };
        
        // 缓存结果
        self.set_cached_value(input_id, value.clone()).await;
        
        Ok(value)
    }
    
    async fn set_cached_value(&self, input_id: &str, value: InputValue) -> bool {
        let mut cache = self.cache.write().await;
        cache.insert(input_id.to_string(), value);
        true
    }
    
    async fn get_cached_value(&self, input_id: &str) -> Option<InputValue> {
        let cache = self.cache.read().await;
        cache.get(input_id).cloned()
    }
    
    async fn delete_cached_value(&self, input_id: &str) -> bool {
        let mut cache = self.cache.write().await;
        cache.remove(input_id).is_some()
    }
    
    async fn clear_cache(&self, input_id: Option<&str>) {
        let mut cache = self.cache.write().await;
        if let Some(input_id) = input_id {
            cache.remove(input_id);
        } else {
            cache.clear();
        }
    }
}

/// 环境变量解析器 / Environment variable resolver
pub struct EnvInputResolver {
    cache: Arc<RwLock<HashMap<String, InputValue>>>,
}

impl EnvInputResolver {
    /// 创建新的环境变量解析器 / Create new env resolver
    pub fn new() -> Self {
        Self {
            cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl Default for EnvInputResolver {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl InputResolver for EnvInputResolver {
    async fn resolve(&self, input_id: &str) -> Result<InputValue, InputResolverError> {
        // 先检查缓存
        if let Some(value) = self.get_cached_value(input_id).await {
            return Ok(value);
        }
        
        // 从环境变量读取
        match std::env::var(input_id) {
            Ok(value) => {
                let input_value = InputValue::String(value);
                self.set_cached_value(input_id, input_value.clone()).await;
                Ok(input_value)
            }
            Err(_) => Err(InputResolverError::InputNotFound { 
                input_id: input_id.to_string() 
            }),
        }
    }
    
    async fn set_cached_value(&self, input_id: &str, value: InputValue) -> bool {
        let mut cache = self.cache.write().await;
        cache.insert(input_id.to_string(), value);
        true
    }
    
    async fn get_cached_value(&self, input_id: &str) -> Option<InputValue> {
        let cache = self.cache.read().await;
        cache.get(input_id).cloned()
    }
    
    async fn delete_cached_value(&self, input_id: &str) -> bool {
        let mut cache = self.cache.write().await;
        cache.remove(input_id).is_some()
    }
    
    async fn clear_cache(&self, input_id: Option<&str>) {
        let mut cache = self.cache.write().await;
        if let Some(input_id) = input_id {
            cache.remove(input_id);
        } else {
            cache.clear();
        }
    }
}

/// 组合解析器 / Composite resolver
pub struct CompositeResolver {
    resolvers: Vec<Box<dyn InputResolver>>,
}

impl CompositeResolver {
    /// 创建新的组合解析器 / Create new composite resolver
    pub fn new() -> Self {
        Self {
            resolvers: Vec::new(),
        }
    }
    
    /// 添加解析器 / Add resolver
    pub fn add_resolver(mut self, resolver: Box<dyn InputResolver>) -> Self {
        self.resolvers.push(resolver);
        self
    }
}

impl Default for CompositeResolver {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl InputResolver for CompositeResolver {
    async fn resolve(&self, input_id: &str) -> Result<InputValue, InputResolverError> {
        for resolver in &self.resolvers {
            match resolver.resolve(input_id).await {
                Ok(value) => return Ok(value),
                Err(InputResolverError::InputNotFound { .. }) => continue,
                Err(e) => return Err(e),
            }
        }
        
        Err(InputResolverError::InputNotFound { 
            input_id: input_id.to_string() 
        })
    }
    
    async fn set_cached_value(&self, input_id: &str, value: InputValue) -> bool {
        // 只在第一个解析器中设置缓存
        if let Some(resolver) = self.resolvers.first() {
            resolver.set_cached_value(input_id, value).await
        } else {
            false
        }
    }
    
    async fn get_cached_value(&self, input_id: &str) -> Option<InputValue> {
        // 从第一个解析器获取缓存
        if let Some(resolver) = self.resolvers.first() {
            resolver.get_cached_value(input_id).await
        } else {
            None
        }
    }
    
    async fn delete_cached_value(&self, input_id: &str) -> bool {
        // 从第一个解析器删除缓存
        if let Some(resolver) = self.resolvers.first() {
            resolver.delete_cached_value(input_id).await
        } else {
            false
        }
    }
    
    async fn clear_cache(&self, input_id: Option<&str>) {
        // 清空所有解析器的缓存
        for resolver in &self.resolvers {
            resolver.clear_cache(input_id).await;
        }
    }
}
