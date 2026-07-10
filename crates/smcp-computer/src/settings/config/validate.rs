/*!
* 文件名: validate.rs
* 作者: JQQ
* 创建日期: 2026/07/10
* 最后修改日期: 2026/07/10
* 版权: 2023 JQQ. All rights reserved.
* 依赖: serde_json, settings::{schema, mcp_config, config::crud}
* 描述: #107 S4（#111）—— config `validate` / `migrate`（schema-only；无环境探测）。
*       Config `validate` / `migrate`: schema-only, never probes the environment.
*
* 边界（design-107 §8 = 协议 §4.1）/ boundary:
*   - **validate 只做 schema**：version 受支持 / section 结构 / ID 唯一合法 / enum 合法 / 引用**语法**合法。
*     **不**探测：secret 可解析、文件存在、marketplace 可达、plugin 可下载、MCP 可启动（那些归 runtime preflight）。
*   - **意图层 versionless**（`schema.rs`：人编文件不背版本负担 + 未知键 passthrough 前向兼容）→ **不发明**
*     version 字段：`migrate` = 幂等**形态规范化**（把 legacy 别名/非规范形经现有校验器 canonicalize 到规范形），
*     而非 N→N+1 版本迁移。唯一带 `version` 的 ambient home 物化文件已有自迁移、且在 config-CRUD 边界外。
*
* 复用既有校验器（避免与运行期漂移）/ reuses the runtime validators (no drift):
*   settings → [`schema::validate_settings`]；mcp server/input → [`mcp_config::validate_server`/`validate_input`]。
*   validate 产出的错误集**基本上**就是运行期加载会逐条过滤掉的那些——故不误报可运行配置。**唯一刻意更严**的一处：
*   同文件内**重复 input id**（loader 静默去重、不报错，但 §8 要求「ID 唯一」）→ validate 逐条检出。此额外严格性
*   是 schema 层面的合法诊断（重复 id 是编辑意图不清的信号），非环境探测。
*/

use std::collections::HashSet;

use serde_json::{Map, Value};

use super::super::mcp_config::{validate_input, validate_server};
use super::super::schema::{validate_settings, SettingsScope, SettingsValidationError};
use super::crud::{load_project_config_doc, save_config, ConfigCrudError, ProjectConfigDoc};
use std::path::Path;

// 逻辑文件名（`SettingsValidationError.source_path` 溯源用；validate 对内存 doc 作业、无真实路径）。
const SETTINGS_FILE: &str = "settings.json";
const SETTINGS_LOCAL_FILE: &str = "settings.local.json";
const MCP_FILE: &str = "mcp.json";
const MCP_LOCAL_FILE: &str = "mcp.local.json";

// ===========================================================================
// validate
// ===========================================================================

/// config schema 校验报告（非阻断、供诊断/CLI 呈现）/ config schema validation report。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ValidationReport {
    /// 逐条 schema 错误（`source_path` = 逻辑文件名）/ per-entry schema errors。
    pub errors: Vec<SettingsValidationError>,
}

impl ValidationReport {
    /// 无 schema 错误即通过 / valid iff no errors。
    pub fn is_valid(&self) -> bool {
        self.errors.is_empty()
    }
}

/// 对 project 锚点文档做 **schema-only** 校验（design §8 / 协议 §4.1）/ schema-only validation of a project doc。
///
/// 逐文件复用运行期校验器；不触碰环境、不解析 secret、不探测可达性。四个文件各按其 scope 归类校验。
pub fn validate_config(doc: &ProjectConfigDoc) -> ValidationReport {
    let mut errors = Vec::new();
    if let Some(m) = &doc.settings {
        errors.extend(validate_settings_map(
            m,
            SettingsScope::Project,
            SETTINGS_FILE,
        ));
    }
    if let Some(m) = &doc.settings_local {
        errors.extend(validate_settings_map(
            m,
            SettingsScope::Local,
            SETTINGS_LOCAL_FILE,
        ));
    }
    if let Some(m) = &doc.mcp {
        errors.extend(validate_mcp_map(m, SettingsScope::Project, MCP_FILE));
    }
    if let Some(m) = &doc.mcp_local {
        errors.extend(validate_mcp_map(m, SettingsScope::Local, MCP_LOCAL_FILE));
    }
    ValidationReport { errors }
}

/// 校验单个 settings map（复用 [`schema::validate_settings`]，只取错误）/ validate one settings map。
fn validate_settings_map(
    m: &Map<String, Value>,
    scope: SettingsScope,
    label: &str,
) -> Vec<SettingsValidationError> {
    let (_cleaned, errors) = validate_settings(&Value::Object(m.clone()), scope, Some(label));
    errors
}

/// 校验单个 mcp map（top-level shape + per-server/input + input-id 唯一）/ validate one mcp map。
fn validate_mcp_map(
    m: &Map<String, Value>,
    scope: SettingsScope,
    label: &str,
) -> Vec<SettingsValidationError> {
    let mut errors = Vec::new();

    // servers：缺失/null 合法（空）；非对象 → 记错。object key 即身份，天然唯一（JSON 解析去重）。
    match m.get("servers") {
        None | Some(Value::Null) => {}
        Some(Value::Object(servers)) => {
            for (name, sdef) in servers {
                let (_resolved, errs) = validate_server(name, sdef, scope, Some(label));
                errors.extend(errs);
            }
        }
        Some(_) => errors.push(mcp_err(
            scope,
            "servers",
            "'servers' must be an object",
            label,
        )),
    }

    // inputs：缺失/null 合法（空）；非数组 → 记错。数组可含重复 id → §8「ID 唯一」逐条检出。
    match m.get("inputs") {
        None | Some(Value::Null) => {}
        Some(Value::Array(arr)) => {
            let mut seen: HashSet<&str> = HashSet::new();
            for idef in arr {
                let (_resolved, errs) = validate_input(idef, scope, Some(label));
                errors.extend(errs);
                if let Some(id) = idef.get("id").and_then(Value::as_str) {
                    if !seen.insert(id) {
                        errors.push(mcp_err(
                            scope,
                            &format!("inputs.{id}"),
                            "duplicate input id (schema requires unique ids per file)",
                            label,
                        ));
                    }
                }
            }
        }
        Some(_) => errors.push(mcp_err(scope, "inputs", "'inputs' must be an array", label)),
    }

    errors
}

/// 构造一条 mcp 层 schema 错误（`source_path` = 逻辑文件名）/ build one mcp-layer schema error。
fn mcp_err(
    scope: SettingsScope,
    field: &str,
    reason: &str,
    label: &str,
) -> SettingsValidationError {
    SettingsValidationError {
        scope,
        field: field.to_string(),
        reason: reason.to_string(),
        source_path: Some(label.to_string()),
    }
}

// ===========================================================================
// migrate（幂等形态规范化；不发明 version 字段）
// ===========================================================================

/// **幂等**规范化 project 锚点在盘配置：把 legacy/非规范形 canonicalize 到规范形，只写**内容真变**的文件。
/// Idempotently normalize the on-disk project config; writes only files whose content actually changed.
///
/// 返回 `true` = 至少一个文件被改写；`false` = 已是规范形（无写、幂等）。重复调用第二次必返 `false`。
/// **非破坏前向兼容**：未知顶层键 passthrough 保留；mcp body 逐字保留（仅 top-level shape 层面无改写需求）。
///
/// ⚠️ **settings 层是有损规范化**：settings 采 loader 本就采用的 cleaned 形——**类型错/policy-only 错位/畸形**
/// 的条目会被**从盘上移除**（这些条目运行期本就被 loader 忽略，故对运行行为无损；但对「保留原始编辑」而言是删）。
/// migrate 只返回 `bool`、**不逐条回报**被规范化掉的内容——调用方若需预览「将被清理什么」，请先跑
/// [`validate_config`]（其错误集即 settings 层将被丢弃的条目 + mcp 层 schema 问题）。
pub fn migrate_config(config_dir: &Path) -> Result<bool, ConfigCrudError> {
    let doc = load_project_config_doc(config_dir)?;
    let normalized = normalize_doc(&doc);

    // 只落「内容真变」的 slot：normalized_slot == original_slot → None（save_config 跳过，不触 mtime）。
    let write_doc = ProjectConfigDoc {
        settings: changed_slot(&doc.settings, normalized.settings),
        settings_local: changed_slot(&doc.settings_local, normalized.settings_local),
        mcp: changed_slot(&doc.mcp, normalized.mcp),
        mcp_local: changed_slot(&doc.mcp_local, normalized.mcp_local),
    };
    if write_doc == ProjectConfigDoc::default() {
        return Ok(false); // 已规范 → 无写（幂等）。
    }
    save_config(config_dir, &write_doc)?;
    Ok(true)
}

/// `Some(new)` 当且仅当 `new != old`，否则 `None`（不写该 slot）/ keep new slot only when it differs。
fn changed_slot(
    old: &Option<Map<String, Value>>,
    new: Option<Map<String, Value>>,
) -> Option<Map<String, Value>> {
    match (&new, old) {
        (Some(n), Some(o)) if n == o => None,
        (None, _) => None,
        _ => new,
    }
}

/// 纯规范化整个文档（各 slot 结构 Some/None 保持不变）/ purely normalize the whole doc。
fn normalize_doc(doc: &ProjectConfigDoc) -> ProjectConfigDoc {
    ProjectConfigDoc {
        settings: doc
            .settings
            .as_ref()
            .map(|m| normalize_settings_map(m, SettingsScope::Project)),
        settings_local: doc
            .settings_local
            .as_ref()
            .map(|m| normalize_settings_map(m, SettingsScope::Local)),
        // mcp body 逐字保留（前向兼容；typed round-trip 会丢未知 server 子字段，故不做）。
        mcp: doc.mcp.clone(),
        mcp_local: doc.mcp_local.clone(),
    }
}

/// 规范化单个 settings map = loader 本就采用的 cleaned 形（幂等：`validate(cleaned) == cleaned`）。
fn normalize_settings_map(m: &Map<String, Value>, scope: SettingsScope) -> Map<String, Value> {
    validate_settings(&Value::Object(m.clone()), scope, None).0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::config::crud::init_config;
    use crate::settings::mcp_config::{workdir_mcp_config_path, workdir_mcp_local_config_path};
    use crate::settings::scope::{workdir_local_settings_path, workdir_project_settings_path};
    use serde_json::json;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    fn read_json(path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    // ---- validate：schema-only ----

    #[test]
    fn validate_clean_doc_has_no_errors() {
        let doc = ProjectConfigDoc {
            settings: Some(obj(json!({"strictKnownMarketplaces": true}))),
            mcp: Some(obj(json!({
                "servers": {"srv": {"type": "stdio", "server_parameters": {"command": "x"}}},
                "inputs": [{"type": "PromptString", "id": "tok", "description": "d"}]
            }))),
            ..Default::default()
        };
        assert!(validate_config(&doc).is_valid());
    }

    #[test]
    fn validate_flags_bad_server_type_enum() {
        // enum 非法：type 不是 stdio/sse/http → per-server serde 校验报错。
        let doc = ProjectConfigDoc {
            mcp: Some(obj(json!({"servers": {"srv": {"type": "carrier-pigeon"}}}))),
            ..Default::default()
        };
        let report = validate_config(&doc);
        assert!(!report.is_valid());
        assert!(report.errors.iter().any(|e| e.field == "servers.srv"));
        assert_eq!(report.errors[0].source_path.as_deref(), Some(MCP_FILE));
    }

    #[test]
    fn validate_flags_duplicate_input_id() {
        // inputs 是数组 → 可含重复 id → §8「ID 唯一」检出。
        let doc = ProjectConfigDoc {
            mcp: Some(obj(json!({
                "inputs": [
                    {"type": "PromptString", "id": "dup", "description": "a"},
                    {"type": "PromptString", "id": "dup", "description": "b"}
                ]
            }))),
            ..Default::default()
        };
        let report = validate_config(&doc);
        assert!(report
            .errors
            .iter()
            .any(|e| e.field == "inputs.dup" && e.reason.contains("unique")));
    }

    #[test]
    fn validate_flags_non_object_servers_and_non_array_inputs() {
        let doc = ProjectConfigDoc {
            mcp: Some(obj(
                json!({"servers": ["nope"], "inputs": {"not": "array"}}),
            )),
            ..Default::default()
        };
        let report = validate_config(&doc);
        let fields: HashSet<&str> = report.errors.iter().map(|e| e.field.as_str()).collect();
        assert!(fields.contains("servers"));
        assert!(fields.contains("inputs"));
    }

    #[test]
    fn validate_settings_scope_tagging_local_vs_project() {
        // 同一 policy-only 字段：project 层报错、local 层报错，各带对应 scope + 文件名。
        let doc = ProjectConfigDoc {
            settings: Some(obj(json!({"allowedMcpServers": ["a"]}))),
            settings_local: Some(obj(json!({"deniedMcpServers": ["b"]}))),
            ..Default::default()
        };
        let report = validate_config(&doc);
        assert!(report
            .errors
            .iter()
            .any(|e| e.scope == SettingsScope::Project
                && e.source_path.as_deref() == Some(SETTINGS_FILE)));
        assert!(report.errors.iter().any(|e| e.scope == SettingsScope::Local
            && e.source_path.as_deref() == Some(SETTINGS_LOCAL_FILE)));
    }

    #[test]
    fn validate_empty_doc_is_valid() {
        assert!(validate_config(&ProjectConfigDoc::default()).is_valid());
    }

    #[test]
    fn validate_does_not_probe_environment() {
        // 引用未定义 input / 指向不存在文件 —— schema-only 不报错（不探测可解析/文件存在）。
        let doc = ProjectConfigDoc {
            mcp: Some(obj(json!({
                "servers": {"srv": {"type": "stdio", "server_parameters": {
                    "command": "run", "env": {"TOK": "${input:never-defined}"}
                }}}
            }))),
            ..Default::default()
        };
        assert!(
            validate_config(&doc).is_valid(),
            "未定义引用是运行期问题，schema 不报"
        );
    }

    // ---- migrate：幂等 ----

    struct Fx {
        _tmp: TempDir,
        wd: PathBuf,
    }
    impl Fx {
        fn new() -> Self {
            let tmp = TempDir::new().unwrap();
            let wd = tmp.path().join("wd");
            Self { _tmp: tmp, wd }
        }
    }

    #[test]
    fn migrate_is_idempotent() {
        let fx = Fx::new();
        // 非规范：marketplace entry 带多余子键 + autoUpdate；migrate 应 canonicalize。
        write(
            &workdir_project_settings_path(&fx.wd),
            r#"{"extraKnownMarketplaces": {"mp": {"source": {"type": "git", "url": "git@h:p", "junk": 1}, "autoUpdate": true, "extra": 9}}}"#,
        );
        // 第一次：有改写。
        assert!(migrate_config(&fx.wd).unwrap(), "非规范 → 第一次改写");
        // 规范化后：source 只留 {type,url}，顶层 entry 只留 {source, autoUpdate}。
        assert_eq!(
            read_json(&workdir_project_settings_path(&fx.wd))["extraKnownMarketplaces"]["mp"],
            json!({"source": {"type": "git", "url": "git@h:p"}, "autoUpdate": true})
        );
        // 第二次：已规范 → 幂等，无改写。
        assert!(!migrate_config(&fx.wd).unwrap(), "已规范 → 第二次幂等无写");
    }

    #[test]
    fn migrate_canonical_config_is_noop() {
        let fx = Fx::new();
        init_config(&fx.wd).unwrap();
        // init 产出的骨架已是规范形 → migrate 无改写。
        assert!(!migrate_config(&fx.wd).unwrap());
    }

    #[test]
    fn migrate_preserves_unknown_passthrough_keys() {
        let fx = Fx::new();
        write(
            &workdir_project_settings_path(&fx.wd),
            r#"{"futureTopLevelKey": {"keep": "me"}, "trustedMarketplaces": ["a", "a"]}"#,
        );
        migrate_config(&fx.wd).unwrap();
        let after = read_json(&workdir_project_settings_path(&fx.wd));
        // 未知键 passthrough 保留。
        assert_eq!(after["futureTopLevelKey"], json!({"keep": "me"}));
    }

    #[test]
    fn migrate_only_rewrites_changed_files() {
        let fx = Fx::new();
        // settings 非规范（需改写）；mcp 已规范（不应被改写）。
        write(
            &workdir_project_settings_path(&fx.wd),
            r#"{"strictKnownMarketplaces": "not-a-bool"}"#,
        );
        write(
            &workdir_mcp_config_path(&fx.wd),
            r#"{"servers":{"srv":{"type":"stdio","server_parameters":{"command":"x"}}}}"#,
        );
        let mcp_before = std::fs::read_to_string(workdir_mcp_config_path(&fx.wd)).unwrap();
        assert!(migrate_config(&fx.wd).unwrap());
        // 非法 bool 被丢 → settings 规范化为空对象。
        assert_eq!(read_json(&workdir_project_settings_path(&fx.wd)), json!({}));
        // mcp 未变 → 逐字节保持（含原始格式），未被改写。
        let mcp_after = std::fs::read_to_string(workdir_mcp_config_path(&fx.wd)).unwrap();
        assert_eq!(
            mcp_before, mcp_after,
            "已规范的 mcp.json 不应被 migrate 改写"
        );
        // local 文件从未存在 → 不被凭空创建。
        assert!(!workdir_local_settings_path(&fx.wd).exists());
        assert!(!workdir_mcp_local_config_path(&fx.wd).exists());
    }
}
