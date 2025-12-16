/*!
* 文件名: main.rs
* 作者: JQQ
* 创建日期: 2025/12/16
* 最后修改日期: 2025/12/16
* 版权: 2023 JQQ. All rights reserved.
* 依赖: clap, tokio, console, rustyline
* 描述: A2C-SMCP Computer CLI的Rust实现入口 / Rust implementation entry for A2C-SMCP Computer CLI
*/

#[cfg(feature = "cli")]
fn main() {
    smcp_computer::cli::main();
}

#[cfg(not(feature = "cli"))]
fn main() {
    eprintln!("Error: CLI feature is not enabled. Please compile with --features cli");
    std::process::exit(1);
}
