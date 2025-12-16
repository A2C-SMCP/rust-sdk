/*!
* 文件名: utils
* 作者: JQQ
* 创建日期: 2025/12/16
* 最后修改日期: 2025/12/16
* 版权: 2023 JQQ. All rights reserved.
* 依赖: std::process::Command
* 描述: 输入处理相关的工具函数
*/

use crate::errors::ComputerError;
use std::process::Command;

/// 执行 shell 命令并返回输出 / Execute shell command and return output
///
/// # Arguments
///
/// * `command` - 要执行的命令 / Command to execute
/// * `args` - 命令参数 / Command arguments
///
/// # Returns
///
/// 返回命令的标准输出（去除首尾空白） / Returns command stdout (trimmed)
///
/// # Examples
///
/// ```
/// use smcp_computer::inputs::utils::run_command;
///
/// #[tokio::main]
/// async fn main() -> Result<(), Box<dyn std::error::Error>> {
///     let output = run_command("echo", &["hello".to_string()]).await?;
///     assert_eq!(output, "hello");
///     Ok(())
/// }
/// ```
pub async fn run_command(command: &str, args: &[String]) -> Result<String, ComputerError> {
    let output = if cfg!(target_os = "windows") {
        // Windows: Use cmd /C for shell mode
        let mut cmd = Command::new("cmd");
        cmd.arg("/C");
        cmd.arg(command);
        for arg in args {
            cmd.arg(arg);
        }
        cmd.output()
    } else {
        // Unix: Use sh -c for shell mode
        let mut cmd = Command::new("sh");
        cmd.arg("-c");
        // Combine command and args into a single shell string
        let shell_cmd = if args.is_empty() {
            command.to_string()
        } else {
            format!("{} {}", command, args.join(" "))
        };
        cmd.arg(&shell_cmd);
        cmd.output()
    }
    .map_err(|e| ComputerError::RuntimeError(format!("Command execution failed: {}", e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ComputerError::RuntimeError(format!(
            "Command failed with exit code {}: {}",
            output.status.code().unwrap_or(-1),
            stderr
        )));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_run_command_simple() {
        let output = run_command("echo", &["hello".to_string()]).await.unwrap();
        assert_eq!(output, "hello");
    }

    #[tokio::test]
    async fn test_run_command_no_args() {
        let output = run_command("pwd", &[]).await.unwrap();
        // pwd should return something (current directory)
        assert!(!output.is_empty());
    }

    #[tokio::test]
    async fn test_run_command_failure() {
        let result = run_command("nonexistent_command", &[]).await;
        assert!(result.is_err());
    }
}
