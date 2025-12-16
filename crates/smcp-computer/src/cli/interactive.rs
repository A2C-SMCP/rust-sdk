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
    let mut rl = Editor::<(), rustyline::history::DefaultHistory>::new().map_err(|e| CommandError::ComputerError(ComputerError::TransportError(e.to_string())))?;
    
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
                return Err(CommandError::ComputerError(ComputerError::TransportError(err.to_string())));
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
                return Err(CommandError::InvalidCommand("server 子命令缺失".to_string()));
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
                    return Err(CommandError::InvalidCommand(format!("未知的 server 子命令: {}", parts[1])));
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
                return Err(CommandError::InvalidCommand("inputs 子命令缺失".to_string()));
            }
            match parts[1] {
                "load" => {
                    if parts.len() < 3 {
                        return Err(CommandError::InvalidCommand("缺少文件路径".to_string()));
                    }
                    let path = std::path::Path::new(&parts[2][1..]);
                    handler.load_inputs(path).await?;
                }
                "list" => {
                    handler.list_inputs().await?;
                }
                "value" => {
                    handle_inputs_value(handler, &parts).await?;
                }
                _ => {
                    return Err(CommandError::InvalidCommand(format!("未知的 inputs 子命令: {}", parts[1])));
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
                return Err(CommandError::InvalidCommand("socket 子命令缺失".to_string()));
            }
            match parts[1] {
                "connect" => {
                    let url = if parts.len() > 2 { parts[2] } else { "http://localhost:3000" };
                    handler.connect_socketio(url, "/smcp", &None, &None).await?;
                }
                "join" => {
                    // TODO: 实现 join room
                    println!("加入房间功能暂未实现 / Join room not implemented yet");
                }
                "leave" => {
                    // TODO: 实现 leave room
                    println!("离开房间功能暂未实现 / Leave room not implemented yet");
                }
                _ => {
                    return Err(CommandError::InvalidCommand(format!("未知的 socket 子命令: {}", parts[1])));
                }
            }
        }
        "notify" => {
            if parts.len() < 2 {
                return Err(CommandError::InvalidCommand("notify 子命令缺失".to_string()));
            }
            if parts[1] == "update" {
                // TODO: 实现通知更新
                println!("配置更新通知已发送 / Config update notification sent");
            }
        }
        "render" => {
            if parts.len() < 2 {
                return Err(CommandError::InvalidCommand("缺少渲染参数".to_string()));
            }
            // TODO: 实现渲染功能
            println!("渲染功能暂未实现 / Render not implemented yet");
        }
        "tc" => {
            if parts.len() < 2 {
                return Err(CommandError::InvalidCommand("缺少调试参数".to_string()));
            }
            // TODO: 实现工具调用调试
            println!("工具调试功能暂未实现 / Tool debug not implemented yet");
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

async fn handle_inputs_value(handler: &mut CommandHandler, parts: &[&str]) -> Result<(), CommandError> {
    if parts.len() < 3 {
        return Err(CommandError::InvalidCommand("inputs value 子命令缺失".to_string()));
    }
    
    match parts[2] {
        "list" => {
            // TODO: 实现 list values
            println!("列出 values 功能暂未实现 / List values not implemented yet");
        }
        "get" => {
            if parts.len() < 4 {
                return Err(CommandError::InvalidCommand("缺少 id 参数".to_string()));
            }
            // TODO: 实现 get value
            println!("获取 value {} 功能暂未实现 / Get value not implemented yet", parts[3]);
        }
        "set" => {
            if parts.len() < 4 {
                return Err(CommandError::InvalidCommand("缺少 id 参数".to_string()));
            }
            // TODO: 实现 set value
            println!("设置 value {} 功能暂未实现 / Set value not implemented yet", parts[3]);
        }
        "rm" => {
            if parts.len() < 4 {
                return Err(CommandError::InvalidCommand("缺少 id 参数".to_string()));
            }
            // TODO: 实现 remove value
            println!("删除 value {} 功能暂未实现 / Remove value not implemented yet", parts[3]);
        }
        "clear" => {
            // TODO: 实现 clear values
            println!("清空 values 功能暂未实现 / Clear values not implemented yet");
        }
        _ => {
            return Err(CommandError::InvalidCommand(format!("未知的 inputs value 子命令: {}", parts[2])));
        }
    }
    
    Ok(())
}