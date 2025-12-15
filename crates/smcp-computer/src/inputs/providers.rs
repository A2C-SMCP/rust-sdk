/**
* 文件名: providers
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: tokio, async-trait
* 描述: 输入提供者实现，支持CLI、环境变量等多种输入方式
*/
use super::model::*;
use async_trait::async_trait;
use std::env;
use std::io::{self, Write};
use std::process::Command;
use std::time::Duration;
use tokio::time::timeout;
use tracing::{debug, warn};

/// 输入提供者trait / Input provider trait
#[async_trait]
pub trait InputProvider: Send + Sync {
    /// 获取输入 / Get input
    async fn get_input(&self, request: &InputRequest, context: &InputContext) -> InputResult<InputResponse>;
}

/// CLI输入提供者 / CLI input provider
pub struct CliInputProvider {
    /// 超时时间 / Timeout duration
    timeout: Duration,
}

impl CliInputProvider {
    /// 创建新的CLI输入提供者 / Create new CLI input provider
    pub fn new() -> Self {
        Self {
            timeout: Duration::from_secs(300), // 5分钟默认超时 / 5 minutes default timeout
        }
    }

    /// 设置超时时间 / Set timeout duration
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// 从标准输入读取字符串 / Read string from stdin
    async fn read_string(&self, prompt: &str, password: bool) -> InputResult<String> {
        let future = async {
            print!("{}", prompt);
            io::stdout().flush().map_err(InputError::IoError)?;
            
            if password {
                // 使用rpassword库读取密码 / Use rpassword library to read password
                // 这里简化处理，实际应该使用rpassword / Simplified here, should use rpassword in practice
                let mut input = String::new();
                io::stdin().read_line(&mut input).map_err(InputError::IoError)?;
                Ok(input.trim_end().to_string())
            } else {
                let mut input = String::new();
                io::stdin().read_line(&mut input).map_err(InputError::IoError)?;
                Ok(input.trim_end().to_string())
            }
        };
        
        timeout(self.timeout, future).await.map_err(|_| InputError::Timeout)?
    }

    /// 从标准输入读取选择 / Read pick from stdin
    async fn read_pick(&self, prompt: &str, options: &[String]) -> InputResult<String> {
        println!("{}", prompt);
        for (i, option) in options.iter().enumerate() {
            println!("  {}) {}", i + 1, option);
        }
        
        loop {
            let input = self.read_string("请输入选项编号 (Please enter option number): ", false).await?;
            
            match input.parse::<usize>() {
                Ok(n) if n >= 1 && n <= options.len() => {
                    return Ok(options[n - 1].clone());
                }
                _ => {
                    println!("无效选项，请重新输入 (Invalid option, please try again)");
                }
            }
        }
    }

    /// 从标准输入读取数字 / Read number from stdin
    async fn read_number(&self, prompt: &str) -> InputResult<i64> {
        loop {
            let input = self.read_string(prompt, false).await?;
            
            match input.parse::<i64>() {
                Ok(n) => return Ok(n),
                _ => {
                    println!("无效数字，请重新输入 (Invalid number, please try again)");
                }
            }
        }
    }

    /// 从标准输入读取布尔值 / Read boolean from stdin
    async fn read_bool(&self, prompt: &str, true_label: Option<&str>, false_label: Option<&str>) -> InputResult<bool> {
        let true_label = true_label.unwrap_or("是/yes");
        let false_label = false_label.unwrap_or("否/no");
        
        loop {
            let input = self.read_string(&format!("{} ({}/{}): ", prompt, true_label, false_label), false).await?;
            let input = input.to_lowercase();
            
            if input == "y" || input == "yes" || input == "是" {
                return Ok(true);
            } else if input == "n" || input == "no" || input == "否" {
                return Ok(false);
            } else {
                println!("无效选项，请重新输入 (Invalid option, please try again)");
            }
        }
    }

    /// 验证输入 / Validate input
    fn validate_input(&self, value: &str, validation: &Option<ValidationRule>) -> InputResult<()> {
        if let Some(rule) = validation {
            match rule {
                ValidationRule::Regex { pattern, message } => {
                    let regex = regex::Regex::new(pattern).map_err(|e| {
                        InputError::ValidationFailed(format!("Invalid regex pattern: {}", e))
                    })?;
                    
                    if !regex.is_match(value) {
                        let msg = message.as_deref().unwrap_or("输入格式不正确 (Input format is incorrect)");
                        return Err(InputError::ValidationFailed(msg.to_string()));
                    }
                }
                ValidationRule::Custom { .. } => {
                    // 自定义验证需要在更高层实现 / Custom validation needs to be implemented at higher level
                    warn!("Custom validation not implemented for CLI provider");
                }
            }
        }
        Ok(())
    }
}

#[async_trait]
impl InputProvider for CliInputProvider {
    async fn get_input(&self, request: &InputRequest, _context: &InputContext) -> InputResult<InputResponse> {
        let prompt = format!("{}: {}", request.title, request.description);
        
        let value = match &request.input_type {
            InputType::String { password, min_length, max_length } => {
                let input = self.read_string(&prompt, password.unwrap_or(false)).await?;
                
                // 验证长度 / Validate length
                if let Some(min) = min_length {
                    if input.len() < *min {
                        return Err(InputError::ValidationFailed(
                            format!("输入长度不能少于{}个字符 (Minimum length is {})", min, min)
                        ));
                    }
                }
                if let Some(max) = max_length {
                    if input.len() > *max {
                        return Err(InputError::ValidationFailed(
                            format!("输入长度不能超过{}个字符 (Maximum length is {})", max, max)
                        ));
                    }
                }
                
                // 验证格式 / Validate format
                self.validate_input(&input, &request.validation)?;
                
                InputValue::String(input)
            }
            InputType::PickString { options, .. } => {
                let selected = self.read_pick(&prompt, options).await?;
                InputValue::String(selected)
            }
            InputType::Number { min, max } => {
                let num = self.read_number(&prompt).await?;
                
                // 验证范围 / Validate range
                if let Some(min_val) = min {
                    if num < *min_val {
                        return Err(InputError::ValidationFailed(
                            format!("数值不能小于{} (Minimum value is {})", min_val, min_val)
                        ));
                    }
                }
                if let Some(max_val) = max {
                    if num > *max_val {
                        return Err(InputError::ValidationFailed(
                            format!("数值不能大于{} (Maximum value is {})", max_val, max_val)
                        ));
                    }
                }
                
                InputValue::Number(num)
            }
            InputType::Bool { true_label, false_label } => {
                let bool_val = self.read_bool(&prompt, true_label.as_deref(), false_label.as_deref()).await?;
                InputValue::Bool(bool_val)
            }
            InputType::FilePath { must_exist, filter } => {
                let path = self.read_string(&prompt, false).await?;
                
                // 检查文件是否存在 / Check if file exists
                if *must_exist && !std::path::Path::new(&path).exists() {
                    return Err(InputError::ValidationFailed(
                        "文件不存在 (File does not exist)".to_string()
                    ));
                }
                
                // 检查文件类型 / Check file type
                if let Some(filter) = filter {
                    if !path.ends_with(filter) {
                        return Err(InputError::ValidationFailed(
                            format!("文件类型不匹配，期望: {} (File type mismatch, expected: {})", filter, filter)
                        ));
                    }
                }
                
                InputValue::String(path)
            }
            InputType::Command { command, args } => {
                debug!("Executing command: {} {:?}", command, args);
                let output = Command::new(command)
                    .args(args)
                    .output()
                    .map_err(|e| InputError::Other(format!("Command execution failed: {}", e)))?;
                
                if !output.status.success() {
                    return Err(InputError::Other(format!("Command failed: {:?}", output.stderr)));
                }
                
                let result = String::from_utf8_lossy(&output.stdout).trim().to_string();
                InputValue::String(result)
            }
        };
        
        Ok(InputResponse {
            id: request.id.clone(),
            value,
            cancelled: false,
        })
    }
}

impl Default for CliInputProvider {
    fn default() -> Self {
        Self::new()
    }
}

/// 环境变量输入提供者 / Environment variable input provider
pub struct EnvironmentInputProvider {
    /// 前缀 / Prefix
    prefix: String,
}

impl EnvironmentInputProvider {
    /// 创建新的环境变量输入提供者 / Create new environment input provider
    pub fn new() -> Self {
        Self {
            prefix: "A2C_SMCP_".to_string(),
        }
    }

    /// 设置前缀 / Set prefix
    pub fn with_prefix(mut self, prefix: String) -> Self {
        self.prefix = prefix;
        self
    }

    /// 构建环境变量名 / Build environment variable name
    fn build_env_name(&self, id: &str, context: &InputContext) -> String {
        let mut name = format!("{}{}", self.prefix, id.to_uppercase());
        
        if let Some(server) = &context.server_name {
            name = format!("{}_{}", name, server.to_uppercase());
        }
        
        if let Some(tool) = &context.tool_name {
            name = format!("{}_{}", name, tool.to_uppercase());
        }
        
        name
    }
}

#[async_trait]
impl InputProvider for EnvironmentInputProvider {
    async fn get_input(&self, request: &InputRequest, context: &InputContext) -> InputResult<InputResponse> {
        let env_name = self.build_env_name(&request.id, context);
        
        debug!("Looking for environment variable: {}", env_name);
        
        match env::var(&env_name) {
            Ok(value) => {
                // 根据输入类型转换值 / Convert value based on input type
                let converted_value = match &request.input_type {
                    InputType::String { .. } => InputValue::String(value),
                    InputType::PickString { .. } => InputValue::String(value),
                    InputType::FilePath { .. } => InputValue::String(value),
                    InputType::Command { .. } => InputValue::String(value),
                    InputType::Number { .. } => {
                        value.parse::<i64>()
                            .map(InputValue::Number)
                            .map_err(|_| InputError::ValidationFailed(
                                format!("Invalid number in environment variable: {}", env_name)
                            ))?
                    }
                    InputType::Bool { .. } => {
                        let lower = value.to_lowercase();
                        if lower == "true" || lower == "1" || lower == "yes" || lower == "是" {
                            InputValue::Bool(true)
                        } else if lower == "false" || lower == "0" || lower == "no" || lower == "否" {
                            InputValue::Bool(false)
                        } else {
                            return Err(InputError::ValidationFailed(
                                format!("Invalid boolean value in environment variable: {}", env_name)
                            ));
                        }
                    }
                };
                
                Ok(InputResponse {
                    id: request.id.clone(),
                    value: converted_value,
                    cancelled: false,
                })
            }
            Err(env::VarError::NotPresent) => {
                // 如果环境变量不存在，返回默认值或错误
                // If environment variable doesn't exist, return default value or error
                if let Some(default) = &request.default {
                    Ok(InputResponse {
                        id: request.id.clone(),
                        value: default.clone(),
                        cancelled: false,
                    })
                } else if request.required {
                    Err(InputError::ValidationFailed(
                        format!("Required environment variable not found: {}", env_name)
                    ))
                } else {
                    Err(InputError::Cancelled)
                }
            }
            Err(e) => {
                Err(InputError::Other(format!("Environment variable error: {}", e)))
            }
        }
    }
}

impl Default for EnvironmentInputProvider {
    fn default() -> Self {
        Self::new()
    }
}

/// 组合输入提供者 / Composite input provider
pub struct CompositeInputProvider {
    /// 提供者列表 / Provider list
    providers: Vec<Box<dyn InputProvider>>,
}

impl CompositeInputProvider {
    /// 创建新的组合输入提供者 / Create new composite input provider
    pub fn new() -> Self {
        Self {
            providers: Vec::new(),
        }
    }

    /// 添加提供者 / Add provider
    pub fn add_provider(mut self, provider: Box<dyn InputProvider>) -> Self {
        self.providers.push(provider);
        self
    }
}

#[async_trait]
impl InputProvider for CompositeInputProvider {
    async fn get_input(&self, request: &InputRequest, context: &InputContext) -> InputResult<InputResponse> {
        // 依次尝试每个提供者 / Try each provider in order
        for provider in &self.providers {
            match provider.get_input(request, context).await {
                Ok(response) => return Ok(response),
                Err(InputError::Cancelled) => {
                    // 取消错误继续尝试下一个提供者
                    // Continue trying next provider for cancelled error
                    continue;
                }
                Err(e) => {
                    // 其他错误直接返回 / Return other errors directly
                    return Err(e);
                }
            }
        }
        
        // 所有提供者都失败 / All providers failed
        Err(InputError::Cancelled)
    }
}

impl Default for CompositeInputProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_cli_provider_creation() {
        let provider = CliInputProvider::new();
        assert_eq!(provider.timeout.as_secs(), 300);
    }

    #[tokio::test]
    async fn test_environment_provider_creation() {
        let provider = EnvironmentInputProvider::new();
        assert_eq!(provider.prefix, "A2C_SMCP_");
    }

    #[tokio::test]
    async fn test_environment_provider_custom_prefix() {
        let provider = EnvironmentInputProvider::new().with_prefix("CUSTOM_".to_string());
        assert_eq!(provider.prefix, "CUSTOM_");
    }
}
