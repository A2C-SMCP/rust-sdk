/*!
* 文件名: interactive.rs
* 作者: JQQ
* 创建日期: 2025/12/16
* 最后修改日期: 2025/12/16
* 版权: 2023 JQQ. All rights reserved.
* 依赖: rustyline, console
* 描述: 交互式REPL循环 / Interactive REPL loop
*/

use crate::cli::commands::{CommandError, CommandHandler};
use crate::errors::ComputerError;
use rustyline::error::ReadlineError;
use rustyline::Editor;
use std::str::FromStr;

const PROMPT: &str = "a2c> ";

pub async fn run_interactive_loop(mut handler: CommandHandler) -> Result<(), CommandError> {
    let mut rl = Editor::<(), rustyline::history::DefaultHistory>::new()
        .map_err(|e| CommandError::ComputerError(ComputerError::TransportError(e.to_string())))?;

    println!("进入交互模式，输入 help 查看命令 / Enter interactive mode, type 'help' for commands");

    loop {
        match rl.readline(PROMPT) {
            Ok(line) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }

                rl.add_history_entry(line).ok();

                if let Err(e) = handle_command(&mut handler, line).await {
                    eprintln!("命令执行失败: {}", e);
                }
            }
            Err(ReadlineError::Interrupted) => {
                println!("使用 Ctrl-D 或输入 quit 退出 / Use Ctrl-D or type 'quit' to exit");
                continue;
            }
            Err(ReadlineError::Eof) => {
                println!("再见 / Bye");
                break;
            }
            Err(err) => {
                return Err(CommandError::ComputerError(ComputerError::TransportError(
                    err.to_string(),
                )));
            }
        }
    }

    Ok(())
}

async fn handle_command(handler: &mut CommandHandler, line: &str) -> Result<(), CommandError> {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.is_empty() {
        return Ok(());
    }

    let cmd = parts[0].to_lowercase();

    match cmd.as_str() {
        "help" | "?" => {
            handler.show_help();
        }
        "status" => {
            handler.show_status().await?;
        }
        "tools" => {
            handler.list_tools().await?;
        }
        "mcp" => {
            handler.show_mcp_config().await?;
        }
        "server" => {
            if parts.len() < 2 {
                return Err(CommandError::InvalidCommand(
                    "server 子命令缺失".to_string(),
                ));
            }
            match parts[1] {
                "add" => {
                    if parts.len() < 3 {
                        return Err(CommandError::InvalidCommand("缺少配置参数".to_string()));
                    }
                    let config = line.splitn(3, ' ').nth(2).unwrap();
                    handler.add_server(config).await?;
                }
                "rm" | "remove" => {
                    if parts.len() < 3 {
                        return Err(CommandError::InvalidCommand("缺少服务器名称".to_string()));
                    }
                    handler.remove_server(parts[2]).await?;
                }
                _ => {
                    return Err(CommandError::InvalidCommand(format!(
                        "未知的 server 子命令: {}",
                        parts[1]
                    )));
                }
            }
        }
        "start" => {
            if parts.len() < 2 {
                return Err(CommandError::InvalidCommand("缺少目标名称".to_string()));
            }
            handler.start_client(parts[1]).await?;
        }
        "stop" => {
            if parts.len() < 2 {
                return Err(CommandError::InvalidCommand("缺少目标名称".to_string()));
            }
            handler.stop_client(parts[1]).await?;
        }
        "inputs" => {
            if parts.len() < 2 {
                return Err(CommandError::InvalidCommand(
                    "inputs 子命令缺失".to_string(),
                ));
            }
            match parts[1] {
                "load" => {
                    if parts.len() < 3 {
                        return Err(CommandError::InvalidCommand(
                            "缺少文件路径 / Missing file path".to_string(),
                        ));
                    }
                    let path = std::path::Path::new(&parts[2][1..]);
                    handler.load_inputs(path).await?;
                }
                "add" => {
                    if parts.len() < 3 {
                        return Err(CommandError::InvalidCommand("用法: inputs add <json|@file.json> / Usage: inputs add <json|@file.json>".to_string()));
                    }
                    let input_str = line.splitn(3, ' ').nth(2).unwrap();
                    handler.add_input(input_str).await?;
                }
                "update" => {
                    if parts.len() < 3 {
                        return Err(CommandError::InvalidCommand("用法: inputs update <json|@file.json> / Usage: inputs update <json|@file.json>".to_string()));
                    }
                    let input_str = line.splitn(3, ' ').nth(2).unwrap();
                    handler.update_input(input_str).await?;
                }
                "rm" | "remove" => {
                    if parts.len() < 3 {
                        return Err(CommandError::InvalidCommand(
                            "用法: inputs rm <id> / Usage: inputs rm <id>".to_string(),
                        ));
                    }
                    handler.remove_input_def(parts[2]).await?;
                }
                "get" => {
                    if parts.len() < 3 {
                        return Err(CommandError::InvalidCommand(
                            "用法: inputs get <id> / Usage: inputs get <id>".to_string(),
                        ));
                    }
                    handler.get_input_def(parts[2]).await?;
                }
                "list" => {
                    handler.list_inputs().await?;
                }
                "value" => {
                    handle_inputs_value(handler, &parts, line).await?;
                }
                _ => {
                    return Err(CommandError::InvalidCommand(format!(
                        "未知的 inputs 子命令: {} / Unknown inputs subcommand: {}",
                        parts[1], parts[1]
                    )));
                }
            }
        }
        "desktop" => {
            let mut size: Option<u32> = None;
            let mut uri: Option<&str> = None;

            for arg in parts.iter().skip(1) {
                if size.is_none() {
                    if let Ok(s) = u32::from_str(arg) {
                        size = Some(s);
                        continue;
                    }
                }
                if uri.is_none() {
                    uri = Some(arg);
                }
            }

            handler.get_desktop(size, uri).await?;
        }
        "history" => {
            let n = if parts.len() > 1 {
                Some(usize::from_str(parts[1]).unwrap_or(10))
            } else {
                Some(10)
            };
            handler.show_history(n).await?;
        }
        "socket" => {
            if parts.len() < 2 {
                return Err(CommandError::InvalidCommand(
                    "socket 子命令缺失".to_string(),
                ));
            }
            match parts[1] {
                "connect" => {
                    let url = if parts.len() > 2 {
                        parts[2]
                    } else {
                        "http://localhost:3000"
                    };
                    handler.connect_socketio(url, "/smcp", &None, &None).await?;
                }
                "join" => {
                    if parts.len() < 4 {
                        return Err(CommandError::InvalidCommand(
                            "用法: socket join <office_id> <computer_name> / Usage: socket join <office_id> <computer_name>".to_string(),
                        ));
                    }
                    handler.join_socket_room(parts[2], parts[3]).await?;
                }
                "leave" => {
                    handler.leave_socket_room().await?;
                }
                _ => {
                    return Err(CommandError::InvalidCommand(format!(
                        "未知的 socket 子命令: {}",
                        parts[1]
                    )));
                }
            }
        }
        "notify" => {
            if parts.len() < 2 {
                return Err(CommandError::InvalidCommand(
                    "notify 子命令缺失".to_string(),
                ));
            }
            if parts[1] == "update" {
                handler.notify_config_update().await?;
            }
        }
        "render" => {
            if parts.len() < 2 {
                return Err(CommandError::InvalidCommand(
                    "缺少渲染参数 / Missing render parameter".to_string(),
                ));
            }
            let config_str = line.splitn(3, ' ').nth(2).unwrap();
            handler.render_config(config_str).await?;
        }
        "tc" => {
            if parts.len() < 2 {
                return Err(CommandError::InvalidCommand(
                    "缺少调试参数 / Missing debug parameter".to_string(),
                ));
            }
            let tool_call_str = line.splitn(3, ' ').nth(2).unwrap();
            handler.debug_tool_call(tool_call_str).await?;
        }
        "quit" | "exit" => {
            std::process::exit(0);
        }
        _ => {
            return Err(CommandError::InvalidCommand(format!("未知命令: {}", cmd)));
        }
    }

    Ok(())
}

async fn handle_inputs_value(
    handler: &mut CommandHandler,
    parts: &[&str],
    line: &str,
) -> Result<(), CommandError> {
    if parts.len() < 3 {
        return Err(CommandError::InvalidCommand(
            "inputs value 子命令缺失".to_string(),
        ));
    }

    match parts[2] {
        "list" => {
            // 列出当前 inputs 的缓存值 / List current cached input values
            match handler.list_input_values().await {
                Ok(values) => {
                    if values.is_empty() {
                        println!("(暂无缓存值 / No cached values)");
                    } else {
                        println!("当前 inputs 缓存值 / Current input values:");
                        println!("{}", serde_json::to_string_pretty(&values)?);
                    }
                }
                Err(e) => {
                    eprintln!("获取缓存值失败: {} / Failed to get cached values: {}", e, e);
                }
            }
        }
        "get" => {
            if parts.len() < 4 {
                return Err(CommandError::InvalidCommand(
                    "缺少 id 参数 / Missing id parameter".to_string(),
                ));
            }
            // 获取指定 id 的值 / Get value by id
            match handler.get_input_value(parts[3]).await {
                Ok(Some(value)) => {
                    println!("Input '{}' 的值 / Value of '{}':", parts[3], parts[3]);
                    println!("{}", serde_json::to_string_pretty(&value)?);
                }
                Ok(None) => {
                    println!(
                        "未找到或尚未解析: {} / Not found or not resolved yet: {}",
                        parts[3], parts[3]
                    );
                }
                Err(e) => {
                    eprintln!("获取值失败: {} / Failed to get value: {}", e, e);
                }
            }
        }
        "set" => {
            if parts.len() < 4 {
                return Err(CommandError::InvalidCommand(
                    "缺少 id 参数 / Missing id parameter".to_string(),
                ));
            }
            // 设置指定 id 的值 / Set value by id
            let input_id = parts[3];

            // 如果只提供了 id，尝试使用 default 值 / If only id provided, try to use default value
            let value = if parts.len() == 4 {
                // 获取 input 定义以获取 default 值 / Get input definition to get default value
                match handler.get_input_definition(input_id).await {
                    Ok(Some(input)) => {
                        if let Some(default_val) = input.default() {
                            println!("使用 default 值 / Using default value: {}", default_val);
                            default_val.clone()
                        } else {
                            return Err(CommandError::InvalidCommand(format!(
                                "Input '{}' 没有 default 值 / has no default value",
                                input_id
                            )));
                        }
                    }
                    Ok(None) => {
                        return Err(CommandError::InvalidCommand(format!(
                            "不存在的 id / Not found: {}",
                            input_id
                        )));
                    }
                    Err(e) => {
                        return Err(e);
                    }
                }
            } else {
                // 解析提供的值 / Parse provided value
                let value_str = line.splitn(5, ' ').nth(4).unwrap();
                match serde_json::from_str(value_str) {
                    Ok(v) => v,
                    Err(_) => {
                        // 如果不是 JSON，当作字符串处理 / If not JSON, treat as string
                        serde_json::Value::String(value_str.to_string())
                    }
                }
            };

            match handler.set_input_value(input_id, &value).await {
                Ok(_) => {
                    println!("已设置 / Set successfully");
                }
                Err(e) => {
                    eprintln!("设置值失败: {} / Failed to set value: {}", e, e);
                }
            }
        }
        "rm" => {
            if parts.len() < 4 {
                return Err(CommandError::InvalidCommand(
                    "缺少 id 参数 / Missing id parameter".to_string(),
                ));
            }
            // 删除指定 id 的值 / Remove value by id
            match handler.remove_input_value(parts[3]).await {
                Ok(true) => {
                    println!("已删除 / Removed successfully");
                }
                Ok(false) => {
                    println!("无此缓存 / No such cached value");
                }
                Err(e) => {
                    eprintln!("删除值失败: {} / Failed to remove value: {}", e, e);
                }
            }
        }
        "clear" => {
            // 清空全部或指定 id 的缓存 / Clear all or specific cached value
            let target_id = if parts.len() >= 4 {
                Some(parts[3])
            } else {
                None
            };
            match handler.computer.clear_input_values(target_id).await {
                Ok(_) => {
                    if let Some(id) = target_id {
                        println!("已清空 '{}' 的缓存 / Cleared cache for '{}'", id, id);
                    } else {
                        println!("已清空所有缓存 / Cleared all cached values");
                    }
                }
                Err(e) => {
                    eprintln!("清空缓存失败: {} / Failed to clear cache: {}", e, e);
                }
            }
        }
        _ => {
            return Err(CommandError::InvalidCommand(format!(
                "未知的 inputs value 子命令: {}",
                parts[2]
            )));
        }
    }

    Ok(())
}
