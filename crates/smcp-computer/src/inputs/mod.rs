/*!
* 文件名: mod
* 作者: JQQ
* 创建日期: 2025/12/15
* 最后修改日期: 2025/12/15
* 版权: 2023 JQQ. All rights reserved.
* 依赖: None
* 描述: Inputs抽象层模块，负责处理各种输入方式
*/
pub mod handler;
pub mod model;
pub mod providers;
pub mod utils;

// 治理层 inputs 解析链（INP-01 #73）/ governance inputs resolution chain
pub mod plugin_pool;
pub mod render;
pub mod resolver;
pub mod secret_store;
pub mod value_store;

// 重新导出核心类型 / Re-export core types
pub use handler::InputHandler;
pub use model::*;
pub use providers::{CliInputProvider, EnvironmentInputProvider, InputProvider};
pub use utils::run_command;

// INP-01 解析链 re-export / resolution-chain re-export
pub use plugin_pool::{load_plugin_inputs, prefix_input_id};
pub use render::{load_env_file, ConfigRender, PREDEFINED_VARS};
pub use resolver::{
    env_var_name, InputResolveError, InputResolver, NonInteractivePrompter, Prompter,
};
pub use secret_store::{keyring_available, KeyringBackend, OsKeyring, SecretStore, SERVICE_NAME};
pub use value_store::{
    resolve_value_store_path, ValueStore, VALUE_STORE_FILENAME, XDG_STATE_HOME_ENV,
};
