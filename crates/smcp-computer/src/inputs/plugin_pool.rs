/*!
* 文件名: plugin_pool.rs
* 作者: JQQ
* 创建日期: 2026/06/04
* 最后修改日期: 2026/06/04
* 版权: 2023 JQQ. All rights reserved.
* 依赖: serde_json, mcp_clients::model::MCPServerInput
* 描述: plugin inputs 入池消歧（<plugin>@<marketplace>/<id> 前缀）。
*       Plugin inputs pool disambiguation (prefixed pool ids).
*/

//! plugin `mcp-servers/inputs.json` 入池消歧（§9.3 D2）。
//!
//! 对标 Python 治理层资产 `a2c_smcp/computer/inputs/plugin_pool.py`（v0.2.1 #65）。
//!
//! plugin 的 inputs 是 plugin-scoped（协议 v1 不跨 plugin 共享）；合进 Computer 全局 inputs 池时，为避免两个
//! plugin 同 `id` 撞（如都用 `api_token`），**池内存前缀 id** `<plugin>@<marketplace>/<id>`（方案 a：自动前缀
//! 消歧，对用户透明）。plugin 内 `${input:<id>}` 引用用**裸 id**，由 resolver 在该 plugin 上下文里回退到带前缀
//! 池条目。user / CLI 直接定义的 inputs（非 plugin 来源）无前缀，裸 id 入池。
//!
//! 本模块只负责**读取 + 校验 + 前缀改写**；实时挂载（enumerate installed plugins → 填池 → spawn）属 #69。

use std::path::Path;

use serde_json::Value;

use crate::mcp_clients::model::MCPServerInput;

/// 构造带前缀的池内 id `<plugin>@<marketplace>/<id>` / Build the prefixed pool id（§9.3 D2）。
///
/// 与 installer 的 plugin_id `<plugin>@<marketplace>` 命名空间一致。
pub fn prefix_input_id(plugin: &str, marketplace: &str, input_id: &str) -> String {
    format!("{plugin}@{marketplace}/{input_id}")
}

/// 重建 [`MCPServerInput`]，仅替换其 `id`（frozen 语义：复制后改 id）/ rebuild input with a new id。
fn with_id(input: MCPServerInput, new_id: String) -> MCPServerInput {
    match input {
        MCPServerInput::PromptString(mut p) => {
            p.id = new_id;
            MCPServerInput::PromptString(p)
        }
        MCPServerInput::PickString(mut p) => {
            p.id = new_id;
            MCPServerInput::PickString(p)
        }
        MCPServerInput::Command(mut c) => {
            c.id = new_id;
            MCPServerInput::Command(c)
        }
    }
}

/// 读 inputs.json → 原始 input 定义列表；兼容 `{"inputs": [...]}` 与裸 `[...]`；畸形 → `[]`。
fn read_input_defs(inputs_json: &Path) -> Vec<Value> {
    let raw = match std::fs::read_to_string(inputs_json) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            tracing::warn!(path = %inputs_json.display(), error = %e, "failed to read plugin inputs.json");
            return Vec::new();
        }
    };
    let data: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => {
            tracing::warn!(path = %inputs_json.display(), "malformed plugin inputs.json, skip");
            return Vec::new();
        }
    };
    match data {
        Value::Object(map) => match map.get("inputs") {
            Some(Value::Array(arr)) => arr.clone(),
            _ => Vec::new(),
        },
        Value::Array(arr) => arr,
        _ => Vec::new(),
    }
}

/// 读取并前缀化 plugin 的 inputs 定义 / Load and prefix a plugin's input definitions。
///
/// 逐项校验为 [`MCPServerInput`]（field-tolerant：畸形条目记 WARN 跳过、**不抛**），再把 `id` 改写为
/// [`prefix_input_id`]。文件缺失 / 畸形 → `[]`。
pub fn load_plugin_inputs(
    inputs_json: &Path,
    plugin: &str,
    marketplace: &str,
) -> Vec<MCPServerInput> {
    let mut result = Vec::new();
    for idef in read_input_defs(inputs_json) {
        match serde_json::from_value::<MCPServerInput>(idef.clone()) {
            Ok(inp) => {
                let prefixed = prefix_input_id(plugin, marketplace, inp.id());
                result.push(with_id(inp, prefixed));
            }
            Err(exc) => {
                let raw_id = idef
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("<unknown>");
                tracing::warn!(id = %raw_id, error = %exc, "invalid plugin input def, skip");
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_clients::model::PromptStringInput;
    use serde_json::json;
    use tempfile::TempDir;

    fn prompt(id: &str) -> MCPServerInput {
        MCPServerInput::PromptString(PromptStringInput {
            id: id.to_string(),
            description: format!("desc {id}"),
            default: None,
            password: None,
        })
    }

    #[test]
    fn prefix_input_id_namespace() {
        assert_eq!(
            prefix_input_id("frontend", "team", "figma_token"),
            "frontend@team/figma_token"
        );
    }

    #[test]
    fn load_prefixes_ids_and_tolerates_bad_entries() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("inputs.json");
        // 一个合法 promptString + 一个畸形条目（缺 type）；兼容 {"inputs": [...]} 外壳
        let canonical = serde_json::to_value(prompt("api_token")).unwrap();
        let file = json!({ "inputs": [canonical, {"id": "broken", "nonsense": true}] });
        std::fs::write(&path, serde_json::to_string(&file).unwrap()).unwrap();

        let loaded = load_plugin_inputs(&path, "figma", "acme");
        assert_eq!(loaded.len(), 1, "malformed entry should be skipped");
        assert_eq!(loaded[0].id(), "figma@acme/api_token");
    }

    #[test]
    fn load_accepts_bare_array_form() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("inputs.json");
        let arr = json!([serde_json::to_value(prompt("k")).unwrap()]);
        std::fs::write(&path, serde_json::to_string(&arr).unwrap()).unwrap();
        let loaded = load_plugin_inputs(&path, "p", "m");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id(), "p@m/k");
    }

    #[test]
    fn load_missing_or_malformed_is_empty() {
        let dir = TempDir::new().unwrap();
        assert!(load_plugin_inputs(&dir.path().join("nope.json"), "p", "m").is_empty());
        let bad = dir.path().join("bad.json");
        std::fs::write(&bad, "{ not json").unwrap();
        assert!(load_plugin_inputs(&bad, "p", "m").is_empty());
    }
}
