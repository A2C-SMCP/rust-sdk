/*!
* 文件名: render.rs
* 作者: JQQ
* 创建日期: 2026/06/04
* 最后修改日期: 2026/06/04
* 版权: 2023 JQQ. All rights reserved.
* 依赖: regex, serde_json
* 描述: MCP 配置按需渲染（${input:}/${env:}/预定义变量）+ envFile 加载。
*       On-demand MCP config rendering (placeholders) + envFile loading.
*/

//! MCP 配置按需渲染器 + envFile 加载（§9.1，对标 VS Code）。
//!
//! 对标 Python 治理层资产 `a2c_smcp/computer/inputs/render.py`（v0.2.1 #65）。
//!
//! 识别 `${...}` 占位符并替换，支持递归容器与深度限制：
//! - `${input:<id>}` → 经 `resolve_input` 回调走解析链（[`InputResolver`](super::resolver::InputResolver)）；
//!   未找到 → 保持原样。
//! - `${env:<VAR>}` → 进程 / 注入环境变量，缺失 → 空串 + WARN（VS Code parity）。
//! - 预定义变量 `${workspaceFolder}` / `${userHome}` / `${pathSeparator}` → `variables` 取值，缺失 → 空串。
//! - 未知占位符 → 保持原样（向后兼容）。
//!
//! 同步设计：解析链（含交互 prompt）在 Rust 走同步 blocking（见 [`resolver`](super::resolver)），故渲染器同步；
//! 整串恰为单一占位符时允许返回非字符串值（如 `${input:x}` → 数组 / 对象）。

use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;
use serde_json::Value;

use crate::settings::scope::EnvMap;

/// 预定义变量名 / predefined variable names。
pub const PREDEFINED_VARS: &[&str] = &["workspaceFolder", "userHome", "pathSeparator"];

/// `${...}` 占位符正则（惰性编译）/ the `${...}` placeholder regex (lazily compiled)。
fn placeholder_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\$\{([^}]+)\}").expect("valid placeholder regex"))
}

/// 加载 VS Code 风格 `envFile` 的 `KEY=VALUE` 行（§9.1）/ Load a VS Code-style envFile。
///
/// 跳过空行与 `#` 注释；容忍前缀 `export `；去除值两端**成对**引号；无 `=` 的行跳过 + WARN。文件缺失 / 读失败
/// → `{}` + WARN（容错，不抛）。变量展开**不**在此发生（envFile 值按字面取）。
pub fn load_env_file(path: &Path) -> HashMap<String, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "envFile unreadable, skip");
            return HashMap::new();
        }
    };
    let mut result = HashMap::new();
    for raw_line in text.lines() {
        let mut line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("export ") {
            line = rest.trim();
        }
        let Some((key, value)) = line.split_once('=') else {
            tracing::warn!(line = %raw_line, "envFile line without '=', skip");
            continue;
        };
        let key = key.trim();
        let mut value = value.trim();
        // 去除成对引号 / strip matching surrounding quotes
        let bytes = value.as_bytes();
        if bytes.len() >= 2
            && (bytes[0] == b'"' || bytes[0] == b'\'')
            && bytes[bytes.len() - 1] == bytes[0]
        {
            value = &value[1..value.len() - 1];
        }
        if !key.is_empty() {
            result.insert(key.to_string(), value.to_string());
        }
    }
    result
}

/// 占位符解析结果 / placeholder resolution outcome。
enum Resolved {
    /// 已匹配并取得值（可为非字符串）/ matched, value (may be non-string)。
    Value(Value),
    /// 未匹配（未知占位符 / input 未找到）→ 保持原样 / unmatched → leave as-is。
    Unmatched,
}

/// 配置渲染器，按需解析字符串中的占位符，并递归处理对象与数组 / on-demand config renderer。
pub struct ConfigRender {
    max_depth: usize,
}

impl Default for ConfigRender {
    fn default() -> Self {
        Self { max_depth: 10 }
    }
}

impl ConfigRender {
    /// 默认深度上限 10 / default max recursion depth 10。
    pub fn new() -> Self {
        Self::default()
    }

    /// 指定深度上限 / with an explicit max depth。
    pub fn with_max_depth(max_depth: usize) -> Self {
        Self { max_depth }
    }

    /// 递归渲染任意结构 / recursively render arbitrary structured data。
    ///
    /// - `resolve_input(id) -> Option<Value>`：`${input:id}` 取值；`None` = 未找到 → 保持原样。
    /// - `variables`：预定义变量（workspaceFolder / userHome / pathSeparator）。
    /// - `env`：`${env:VAR}` 取值来源（`None` → 进程环境）。
    pub fn render<F>(
        &self,
        data: &Value,
        resolve_input: &mut F,
        variables: &HashMap<String, String>,
        env: Option<&EnvMap>,
    ) -> Value
    where
        F: FnMut(&str) -> Option<Value>,
    {
        self.render_at(data, resolve_input, variables, env, 0)
    }

    fn render_at<F>(
        &self,
        data: &Value,
        resolve_input: &mut F,
        variables: &HashMap<String, String>,
        env: Option<&EnvMap>,
        depth: usize,
    ) -> Value
    where
        F: FnMut(&str) -> Option<Value>,
    {
        if depth > self.max_depth {
            tracing::error!("rendering depth exceeded, stop recursion");
            return data.clone();
        }
        match data {
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(k, v)| {
                        (
                            k.clone(),
                            self.render_at(v, resolve_input, variables, env, depth + 1),
                        )
                    })
                    .collect(),
            ),
            Value::Array(arr) => Value::Array(
                arr.iter()
                    .map(|x| self.render_at(x, resolve_input, variables, env, depth + 1))
                    .collect(),
            ),
            Value::String(s) => render_str(s, resolve_input, variables, env),
            other => other.clone(),
        }
    }
}

/// 解析单个占位符 token / resolve a single placeholder token。
fn resolve_token<F>(
    token: &str,
    resolve_input: &mut F,
    variables: &HashMap<String, String>,
    env: Option<&EnvMap>,
) -> Resolved
where
    F: FnMut(&str) -> Option<Value>,
{
    if let Some(id) = token.strip_prefix("input:") {
        match resolve_input(id) {
            Some(v) => Resolved::Value(v),
            None => {
                tracing::warn!(id = %id, "input id not resolved, leaving placeholder as-is");
                Resolved::Unmatched
            }
        }
    } else if let Some(var) = token.strip_prefix("env:") {
        let val = match env {
            Some(e) => e.get(var).cloned(),
            None => std::env::var(var).ok(),
        };
        match val {
            Some(v) => Resolved::Value(Value::String(v)),
            None => {
                tracing::warn!(var = %var, "env var undefined, substitute empty (VS Code parity)");
                Resolved::Value(Value::String(String::new()))
            }
        }
    } else if PREDEFINED_VARS.contains(&token) {
        match variables.get(token) {
            Some(v) => Resolved::Value(Value::String(v.clone())),
            None => {
                tracing::warn!(var = %token, "predefined var missing, substitute empty");
                Resolved::Value(Value::String(String::new()))
            }
        }
    } else {
        Resolved::Unmatched // 未知占位符 → 保持原样
    }
}

/// 字符串占位符替换；整串恰为单一占位符且解析为非字符串值 → 返回该值 / replace placeholders in a string。
fn render_str<F>(
    s: &str,
    resolve_input: &mut F,
    variables: &HashMap<String, String>,
    env: Option<&EnvMap>,
) -> Value
where
    F: FnMut(&str) -> Option<Value>,
{
    let re = placeholder_re();
    let matches: Vec<_> = re.captures_iter(s).collect();
    if matches.is_empty() {
        return Value::String(s.to_string());
    }

    // 整串恰为单一占位符 → 允许返回非字符串类型（如 ${input:x} → dict/list）
    if matches.len() == 1 {
        let m = matches[0].get(0).unwrap();
        if m.start() == 0 && m.end() == s.len() {
            let token = matches[0].get(1).unwrap().as_str();
            return match resolve_token(token, resolve_input, variables, env) {
                Resolved::Value(v) => v,
                Resolved::Unmatched => Value::String(s.to_string()),
            };
        }
    }

    // 否则逐个替换为其字符串表现（未知占位符保持原样）
    let mut result = String::new();
    let mut last = 0;
    for caps in &matches {
        let whole = caps.get(0).unwrap();
        let token = caps.get(1).unwrap().as_str();
        result.push_str(&s[last..whole.start()]);
        match resolve_token(token, resolve_input, variables, env) {
            Resolved::Value(v) => result.push_str(&value_to_string(&v)),
            Resolved::Unmatched => result.push_str(whole.as_str()), // 保持原样
        }
        last = whole.end();
    }
    result.push_str(&s[last..]);
    Value::String(result)
}

/// 占位符多值拼接时的字符串表现：字符串原样，其它走 JSON 表示 / string repr for interpolation。
fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn no_inputs(_: &str) -> Option<Value> {
        None
    }

    #[test]
    fn load_env_file_parses_kv_export_quotes_comments() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".env");
        std::fs::write(
            &path,
            "# comment\n\nexport FOO=bar\nQUOTED=\"hello world\"\nSINGLE='x y'\nNOEQ line\nKEY = spaced \n",
        )
        .unwrap();
        let env = load_env_file(&path);
        assert_eq!(env.get("FOO").map(String::as_str), Some("bar"));
        assert_eq!(env.get("QUOTED").map(String::as_str), Some("hello world"));
        assert_eq!(env.get("SINGLE").map(String::as_str), Some("x y"));
        assert_eq!(env.get("KEY").map(String::as_str), Some("spaced"));
        assert!(!env.contains_key("NOEQ")); // 无 '=' 行跳过
    }

    #[test]
    fn load_env_file_missing_is_empty() {
        let dir = TempDir::new().unwrap();
        assert!(load_env_file(&dir.path().join("nope.env")).is_empty());
    }

    #[test]
    fn render_env_and_predefined_and_unknown() {
        let render = ConfigRender::new();
        let mut env = EnvMap::new();
        env.insert("TOKEN".to_string(), "abc".to_string());
        let mut vars = HashMap::new();
        vars.insert("workspaceFolder".to_string(), "/ws".to_string());

        let data = json!({
            "url": "https://${env:TOKEN}@host/${workspaceFolder}/x",
            "missing": "${env:NOPE}-tail",
            "unknown": "${weird:thing}",
        });
        let out = render.render(&data, &mut no_inputs, &vars, Some(&env));
        assert_eq!(out["url"], json!("https://abc@host//ws/x"));
        assert_eq!(out["missing"], json!("-tail")); // 缺失 env → 空串
        assert_eq!(out["unknown"], json!("${weird:thing}")); // 未知 → 原样
    }

    #[test]
    fn render_single_input_placeholder_returns_non_string() {
        let render = ConfigRender::new();
        let vars = HashMap::new();
        // 整串恰为单一 ${input:cfg} → 返回非字符串（数组）
        let mut resolve = |id: &str| -> Option<Value> {
            if id == "cfg" {
                Some(json!(["a", "b"]))
            } else {
                None
            }
        };
        let data = json!({ "list": "${input:cfg}", "leftover": "${input:absent}" });
        let out = render.render(&data, &mut resolve, &vars, None);
        assert_eq!(out["list"], json!(["a", "b"]));
        assert_eq!(out["leftover"], json!("${input:absent}")); // 未找到 → 原样
    }

    #[test]
    fn render_recurses_objects_and_arrays() {
        let render = ConfigRender::new();
        let mut env = EnvMap::new();
        env.insert("V".to_string(), "1".to_string());
        let vars = HashMap::new();
        let data = json!({ "a": ["${env:V}", {"b": "x${env:V}y"}] });
        let out = render.render(&data, &mut no_inputs, &vars, Some(&env));
        assert_eq!(out, json!({ "a": ["1", {"b": "x1y"}] }));
    }
}
