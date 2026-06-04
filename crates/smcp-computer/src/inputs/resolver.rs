/*!
* 文件名: resolver.rs
* 作者: JQQ
* 创建日期: 2026/06/04
* 最后修改日期: 2026/06/04
* 版权: 2023 JQQ. All rights reserved.
* 依赖: serde_json, mcp_clients::model, inputs::{secret_store,value_store,plugin_pool}
* 描述: inputs 解析链（env→keyring→value store→prompt）+ 按类持久化。
*       Inputs resolution chain (env→keyring→value store→prompt) + per-type persistence.
*/

//! inputs 解析器：按 id 惰性解析 + 解析链取值 + 按类持久化（§9.3，对标 VS Code SecretStorage）。
//!
//! 对标 Python 治理层资产 `a2c_smcp/computer/inputs/resolver.py`（v0.2.1 #65）。
//!
//! 解析链（命中即返回、并按解析后池 id 进程内缓存）：
//! 1. **定位定义**：裸 id 未命中且给了 plugin 上下文 → 回退查带前缀池条目（`<plugin>@<mp>/<id>`，§9.3 D2）。
//! 2. **进程内 cache**（按解析后池 id，避免不同 plugin 同裸 id 串味）。
//! 3. **环境变量** `A2C_INPUT_<ID>`（编排层注入，命中**不**落盘）。
//! 4. **OS keyring**（仅 `password:true`）。
//! 5. **明文 value store**（仅非密钥 promptString / pickString）。
//! 6. **交互 prompt**——password 在 headless（无 env + 无 keyring + 无 TTY）下**硬错误**，绝不落明文。
//!
//! 持久化（仅**交互 prompt 得值**时）：password → keyring（不可用 → 仅会话缓存、绝不明文）；非密钥 → 明文
//! value store。env 命中 / command / headless 默认回退**不**持久化。
//!
//! 同步设计：交互 prompt 经 [`Prompter`] seam（blocking）；真实 CLI prompter（rustyline / rpassword + 命令执行）
//! 接线归 CLI-02（#51），本模块提供 trait + headless 默认实现 + 测试替身，令解析链可确定性单测。

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::Value;

use crate::mcp_clients::model::MCPServerInput;
use crate::settings::scope::EnvMap;

use super::plugin_pool::prefix_input_id;
use super::secret_store::SecretStore;
use super::value_store::ValueStore;

/// 把 input id 映射为环境变量名 `A2C_INPUT_<ID_UPPER>`（非 `[A-Z0-9]` → `_`）/ map id to env var name。
///
/// 含前缀 plugin id（`<plugin>@<mp>/<id>`）一并归一，如 `frontend@team/figma_token` →
/// `A2C_INPUT_FRONTEND_TEAM_FIGMA_TOKEN`。
pub fn env_var_name(input_id: &str) -> String {
    let mapped: String = input_id
        .to_uppercase()
        .chars()
        .map(|c| {
            if c.is_ascii_uppercase() || c.is_ascii_digit() {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("A2C_INPUT_{mapped}")
}

/// inputs 解析错误 / input resolution errors。
#[derive(Debug, thiserror::Error)]
pub enum InputResolveError {
    /// 未找到该 id 的 input 定义 / no input definition for the id。
    #[error("input id not found: {0}")]
    NotFound(String),
    /// `password:true` input 在 headless 下无法解析（**硬错误**，绝不落明文，§9.3）/ headless secret。
    #[error("cannot resolve secret '{0}' headless; set {1} or retry in a TTY")]
    Secret(String, String),
    /// 交互 prompt 失败 / interactive prompt failed。
    #[error("failed to prompt for '{0}': {1}")]
    Prompt(String, String),
}

/// 交互 prompt 接缝（headless 默认 / 真实 CLI / 测试替身）/ interactive prompt seam。
///
/// 真实 CLI 实现（rustyline / rpassword + 命令执行）由 CLI-02（#51）接线；本模块提供 trait 与 headless 默认。
pub trait Prompter: Send + Sync {
    /// 是否处于可交互环境（有 TTY / 注入会话）/ whether interactive (has TTY)。
    fn is_interactive(&self) -> bool;
    /// promptString 取值 / prompt for a string。
    fn prompt_string(
        &self,
        message: &str,
        password: bool,
        default: Option<&str>,
    ) -> std::io::Result<String>;
    /// pickString 取值 / pick from options。
    fn pick_string(
        &self,
        message: &str,
        options: &[String],
        default_index: Option<usize>,
    ) -> std::io::Result<String>;
    /// command 取值（执行命令取 stdout）/ run a command and capture stdout。
    fn run_command(&self, command: &str) -> std::io::Result<String>;
}

/// headless 默认 prompter：非交互、所有 prompt 报错（绝不静默）/ a headless, non-interactive prompter。
#[derive(Debug, Default, Clone, Copy)]
pub struct NonInteractivePrompter;

impl Prompter for NonInteractivePrompter {
    fn is_interactive(&self) -> bool {
        false
    }
    fn prompt_string(&self, _m: &str, _p: bool, _d: Option<&str>) -> std::io::Result<String> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "no interactive TTY",
        ))
    }
    fn pick_string(&self, _m: &str, _o: &[String], _d: Option<usize>) -> std::io::Result<String> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "no interactive TTY",
        ))
    }
    fn run_command(&self, _c: &str) -> std::io::Result<String> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "no interactive TTY",
        ))
    }
}

/// 输入解析器：基于 id 的惰性解析、解析链取值与按类持久化 / lazy per-id resolver with chain + persistence。
pub struct InputResolver {
    inputs: HashMap<String, MCPServerInput>,
    cache: Mutex<HashMap<String, Value>>,
    env: EnvMap,
    value_store: ValueStore,
    secret_store: SecretStore,
    prompter: Box<dyn Prompter>,
}

impl InputResolver {
    /// 构造解析器 / construct a resolver。
    ///
    /// - `env` = `None` → 进程环境；`value_store` / `secret_store` = `None` → 默认（env 解析路径 / OS keyring）。
    /// - `prompter` 注入交互 seam（headless 用 [`NonInteractivePrompter`]）。
    pub fn new(
        inputs: impl IntoIterator<Item = MCPServerInput>,
        prompter: Box<dyn Prompter>,
        env: Option<EnvMap>,
        value_store: Option<ValueStore>,
        secret_store: Option<SecretStore>,
    ) -> Self {
        let env = env.unwrap_or_else(|| std::env::vars().collect());
        let value_store = value_store.unwrap_or_else(|| ValueStore::new(Some(&env)));
        let secret_store = secret_store.unwrap_or_default();
        let inputs = inputs
            .into_iter()
            .map(|i| (i.id().to_string(), i))
            .collect();
        Self {
            inputs,
            cache: Mutex::new(HashMap::new()),
            env,
            value_store,
            secret_store,
            prompter,
        }
    }

    /// 按 id 惰性解析（解析链 + 缓存 + 按类持久化）/ resolve by id (chain + cache + persistence)。
    pub fn resolve_by_id(
        &self,
        input_id: &str,
        plugin: Option<&str>,
        marketplace: Option<&str>,
    ) -> Result<Value, InputResolveError> {
        // 1. 定位定义：裸 id 未命中 + plugin 上下文 → 回退带前缀池条目（§9.3 D2）
        let (cfg, resolved_id) = match self.inputs.get(input_id) {
            Some(c) => (c.clone(), input_id.to_string()),
            None => match (plugin, marketplace) {
                (Some(p), Some(m)) => {
                    let prefixed = prefix_input_id(p, m, input_id);
                    match self.inputs.get(&prefixed) {
                        Some(c) => (c.clone(), prefixed),
                        None => return Err(InputResolveError::NotFound(input_id.to_string())),
                    }
                }
                _ => return Err(InputResolveError::NotFound(input_id.to_string())),
            },
        };

        // 2. 进程内 cache（按解析后池 id）
        if let Some(v) = self.cache.lock().unwrap().get(&resolved_id) {
            return Ok(v.clone());
        }

        let is_password =
            matches!(&cfg, MCPServerInput::PromptString(p) if p.password.unwrap_or(false));
        let is_plain_persistable = matches!(
            &cfg,
            MCPServerInput::PromptString(_) | MCPServerInput::PickString(_)
        ) && !is_password;

        // 3. 环境变量 A2C_INPUT_<ID>（命中不落盘）
        if let Some(env_val) = self.env.get(&env_var_name(&resolved_id)) {
            let v = Value::String(env_val.clone());
            self.cache.lock().unwrap().insert(resolved_id, v.clone());
            return Ok(v);
        }

        // 4. OS keyring（仅 password:true）
        if is_password {
            if let Some(secret) = self.secret_store.get(&resolved_id) {
                let v = Value::String(secret);
                self.cache.lock().unwrap().insert(resolved_id, v.clone());
                return Ok(v);
            }
        }

        // 5. 明文 value store（仅非密钥）
        if is_plain_persistable {
            if let Some(stored) = self.value_store.get(&resolved_id) {
                self.cache
                    .lock()
                    .unwrap()
                    .insert(resolved_id.clone(), stored.clone());
                return Ok(stored);
            }
        }

        // 6. 交互 prompt——password 在 headless 下硬错误，绝不落明文。
        let has_tty = self.prompter.is_interactive();
        if is_password && !has_tty {
            return Err(InputResolveError::Secret(
                resolved_id.clone(),
                env_var_name(&resolved_id),
            ));
        }

        let value = self.prompt_value(&cfg, &resolved_id)?;

        // 解析后持久化（仅交互 prompt 得值时）：password → keyring；非密钥 → 明文 value store。
        if has_tty {
            if is_password {
                if !self.secret_store.set(&resolved_id, &value_as_str(&value)) {
                    tracing::debug!(id = %resolved_id, "keyring unavailable, secret cached only (not plaintext)");
                }
            } else if is_plain_persistable {
                if let Err(e) = self.value_store.set(&resolved_id, value.clone()) {
                    tracing::warn!(id = %resolved_id, error = %e, "failed to persist non-secret value");
                }
            }
        }

        self.cache
            .lock()
            .unwrap()
            .insert(resolved_id, value.clone());
        Ok(value)
    }

    fn prompt_value(
        &self,
        cfg: &MCPServerInput,
        resolved_id: &str,
    ) -> Result<Value, InputResolveError> {
        let map_err =
            |e: std::io::Error| InputResolveError::Prompt(resolved_id.to_string(), e.to_string());
        match cfg {
            MCPServerInput::PromptString(p) => {
                let msg = prompt_message(&p.description, "Please input", &p.id);
                let pwd = p.password.unwrap_or(false);
                let s = self
                    .prompter
                    .prompt_string(&msg, pwd, p.default.as_deref())
                    .map_err(map_err)?;
                Ok(Value::String(s))
            }
            MCPServerInput::PickString(p) => {
                let msg = prompt_message(&p.description, "Please pick", &p.id);
                let default_index = p
                    .default
                    .as_ref()
                    .and_then(|d| p.options.iter().position(|o| o == d));
                let picked = self
                    .prompter
                    .pick_string(&msg, &p.options, default_index)
                    .map_err(map_err)?;
                let value = if picked.is_empty() {
                    p.default.clone().unwrap_or_default()
                } else {
                    picked
                };
                Ok(Value::String(value))
            }
            MCPServerInput::Command(c) => {
                let out = self.prompter.run_command(&c.command).map_err(map_err)?;
                Ok(Value::String(out))
            }
        }
    }
}

fn prompt_message(description: &str, verb: &str, id: &str) -> String {
    if description.is_empty() {
        format!("{verb} {id}")
    } else {
        description.to_string()
    }
}

fn value_as_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inputs::secret_store::KeyringBackend;
    use crate::mcp_clients::model::{CommandInput, PickStringInput, PromptStringInput};
    use serde_json::json;
    use std::sync::Mutex as StdMutex;
    use tempfile::TempDir;

    // ---- 测试替身 / test doubles -------------------------------------------
    struct FakeKeyring {
        store: StdMutex<HashMap<String, String>>,
        available: bool,
    }
    impl KeyringBackend for FakeKeyring {
        fn available(&self) -> bool {
            self.available
        }
        fn get(&self, _: &str, id: &str) -> Option<String> {
            self.store.lock().unwrap().get(id).cloned()
        }
        fn set(&self, _: &str, id: &str, v: &str) -> bool {
            self.store
                .lock()
                .unwrap()
                .insert(id.to_string(), v.to_string());
            true
        }
        fn delete(&self, _: &str, id: &str) -> bool {
            self.store.lock().unwrap().remove(id).is_some()
        }
    }

    /// 记录式 prompter：交互，返回预设值并记录被 prompt 的 id / interactive recording prompter。
    struct RecordingPrompter {
        answer: String,
        prompted: StdMutex<Vec<String>>,
    }
    impl Prompter for RecordingPrompter {
        fn is_interactive(&self) -> bool {
            true
        }
        fn prompt_string(
            &self,
            message: &str,
            _p: bool,
            _d: Option<&str>,
        ) -> std::io::Result<String> {
            self.prompted.lock().unwrap().push(message.to_string());
            Ok(self.answer.clone())
        }
        fn pick_string(
            &self,
            _m: &str,
            _o: &[String],
            _d: Option<usize>,
        ) -> std::io::Result<String> {
            Ok(self.answer.clone())
        }
        fn run_command(&self, _c: &str) -> std::io::Result<String> {
            Ok(self.answer.clone())
        }
    }

    fn secret_store(available: bool) -> SecretStore {
        SecretStore::with_backend(
            "a2c-computer",
            Box::new(FakeKeyring {
                store: StdMutex::new(HashMap::new()),
                available,
            }),
        )
    }

    fn prompt_input(id: &str, password: bool) -> MCPServerInput {
        MCPServerInput::PromptString(PromptStringInput {
            id: id.to_string(),
            description: String::new(),
            default: None,
            password: Some(password),
        })
    }

    fn value_store(dir: &TempDir) -> ValueStore {
        ValueStore::with_path(dir.path().join("input-values.json"))
    }

    // ---- env_var_name -------------------------------------------------------
    #[test]
    fn env_var_name_normalizes() {
        assert_eq!(env_var_name("figma_token"), "A2C_INPUT_FIGMA_TOKEN");
        assert_eq!(
            env_var_name("frontend@team/figma_token"),
            "A2C_INPUT_FRONTEND_TEAM_FIGMA_TOKEN"
        );
    }

    // ---- 解析链顺序 / chain ordering ---------------------------------------
    #[test]
    fn env_beats_keyring_and_value_store() {
        let dir = TempDir::new().unwrap();
        let mut env = EnvMap::new();
        env.insert("A2C_INPUT_TOK".to_string(), "from-env".to_string());
        let ks = secret_store(true);
        ks.set("tok", "from-keyring"); // keyring 也有，但 env 优先
        let resolver = InputResolver::new(
            [prompt_input("tok", true)],
            Box::new(NonInteractivePrompter),
            Some(env),
            Some(value_store(&dir)),
            Some(ks),
        );
        assert_eq!(
            resolver.resolve_by_id("tok", None, None).unwrap(),
            json!("from-env")
        );
    }

    #[test]
    fn keyring_used_for_password_secret() {
        let dir = TempDir::new().unwrap();
        let ks = secret_store(true);
        ks.set("tok", "kr-secret");
        let resolver = InputResolver::new(
            [prompt_input("tok", true)],
            Box::new(NonInteractivePrompter),
            Some(EnvMap::new()),
            Some(value_store(&dir)),
            Some(ks),
        );
        assert_eq!(
            resolver.resolve_by_id("tok", None, None).unwrap(),
            json!("kr-secret")
        );
    }

    #[test]
    fn value_store_used_for_non_secret() {
        let dir = TempDir::new().unwrap();
        let vs = value_store(&dir);
        vs.set("region", json!("us-east-1")).unwrap();
        let resolver = InputResolver::new(
            [prompt_input("region", false)],
            Box::new(NonInteractivePrompter),
            Some(EnvMap::new()),
            Some(vs),
            Some(secret_store(true)),
        );
        assert_eq!(
            resolver.resolve_by_id("region", None, None).unwrap(),
            json!("us-east-1")
        );
    }

    #[test]
    fn password_headless_hard_errors_never_plaintext() {
        let dir = TempDir::new().unwrap();
        let resolver = InputResolver::new(
            [prompt_input("secret", true)],
            Box::new(NonInteractivePrompter), // 无 TTY
            Some(EnvMap::new()),
            Some(value_store(&dir)),
            Some(secret_store(false)), // keyring 不可用
        );
        match resolver.resolve_by_id("secret", None, None) {
            Err(InputResolveError::Secret(id, var)) => {
                assert_eq!(id, "secret");
                assert_eq!(var, "A2C_INPUT_SECRET");
            }
            other => panic!("expected Secret error, got {other:?}"),
        }
    }

    #[test]
    fn interactive_prompt_persists_secret_to_keyring_and_caches() {
        let dir = TempDir::new().unwrap();
        let ks = secret_store(true);
        let resolver = InputResolver::new(
            [prompt_input("apikey", true)],
            Box::new(RecordingPrompter {
                answer: "typed".into(),
                prompted: StdMutex::new(vec![]),
            }),
            Some(EnvMap::new()),
            Some(value_store(&dir)),
            Some(ks),
        );
        // 首次：prompt → 持久化 keyring
        assert_eq!(
            resolver.resolve_by_id("apikey", None, None).unwrap(),
            json!("typed")
        );
        // 二次：命中 cache（不再 prompt），值一致
        assert_eq!(
            resolver.resolve_by_id("apikey", None, None).unwrap(),
            json!("typed")
        );
    }

    #[test]
    fn non_secret_prompt_persists_to_value_store() {
        let dir = TempDir::new().unwrap();
        let vs = value_store(&dir);
        let resolver = InputResolver::new(
            [prompt_input("region", false)],
            Box::new(RecordingPrompter {
                answer: "eu".into(),
                prompted: StdMutex::new(vec![]),
            }),
            Some(EnvMap::new()),
            Some(ValueStore::with_path(vs.path().to_path_buf())),
            Some(secret_store(true)),
        );
        assert_eq!(
            resolver.resolve_by_id("region", None, None).unwrap(),
            json!("eu")
        );
        // 落了明文 value store / persisted to plaintext store
        assert_eq!(vs.get("region"), Some(json!("eu")));
    }

    // ---- 池前缀回退 / prefixed-pool fallback --------------------------------
    #[test]
    fn bare_id_falls_back_to_prefixed_pool_entry() {
        let dir = TempDir::new().unwrap();
        // 池里只有带前缀的 figma@acme/token；plugin 上下文用裸 token 引用
        let prefixed = MCPServerInput::PromptString(PromptStringInput {
            id: "figma@acme/token".to_string(),
            description: String::new(),
            default: None,
            password: Some(false),
        });
        let mut env = EnvMap::new();
        env.insert("A2C_INPUT_FIGMA_ACME_TOKEN".to_string(), "v".to_string());
        let resolver = InputResolver::new(
            [prefixed],
            Box::new(NonInteractivePrompter),
            Some(env),
            Some(value_store(&dir)),
            Some(secret_store(true)),
        );
        assert_eq!(
            resolver
                .resolve_by_id("token", Some("figma"), Some("acme"))
                .unwrap(),
            json!("v")
        );
        // 无 plugin 上下文 → NotFound
        assert!(matches!(
            resolver.resolve_by_id("token", None, None),
            Err(InputResolveError::NotFound(_))
        ));
    }

    // ---- pick / command -----------------------------------------------------
    #[test]
    fn pick_and_command_resolve_via_prompter() {
        let dir = TempDir::new().unwrap();
        let pick = MCPServerInput::PickString(PickStringInput {
            id: "env".to_string(),
            description: String::new(),
            options: vec!["dev".into(), "prod".into()],
            default: Some("dev".into()),
        });
        let cmd = MCPServerInput::Command(CommandInput {
            id: "host".to_string(),
            description: String::new(),
            command: "echo h".to_string(),
            args: None,
        });
        let resolver = InputResolver::new(
            [pick, cmd],
            Box::new(RecordingPrompter {
                answer: "prod".into(),
                prompted: StdMutex::new(vec![]),
            }),
            Some(EnvMap::new()),
            Some(value_store(&dir)),
            Some(secret_store(true)),
        );
        assert_eq!(
            resolver.resolve_by_id("env", None, None).unwrap(),
            json!("prod")
        );
        assert_eq!(
            resolver.resolve_by_id("host", None, None).unwrap(),
            json!("prod")
        );
    }
}
