/*!
* 文件名: manifest.rs
* 作者: JQQ
* 创建日期: 2026/06/03
* 最后修改日期: 2026/06/03
* 版权: 2023 JQQ. All rights reserved.
* 依赖: serde_json, smcp (utils::path), crate::mcp_clients::model, crate::skills::sources
* 描述: Plugin manifest 文件式解析（marketplace.json + plugin.json + mcp-servers/<n>.json）
*       对标 Python computer/skills/manifest.py / Plugin manifest file parsing.
*/

//! Plugin manifest 文件式解析 / Plugin manifest file parsing。
//!
//! 协议依据 / Protocol: tfrobot-marketplace `docs/marketplace/protocol-v1.md` §3（marketplace.json）/
//! §4（plugin entry / version 优先级 / strict mode）/ §5（plugin source）；mcp-servers 协议 §1/§2
//! （文件式、文件名=name）。对标 Python `a2c_smcp/computer/skills/manifest.py`。
//!
//! 本模块是 **manifest 纯解析器唯一权威层**（无 git / 无 registry / 无 MCP manager 副作用），供 plugin
//! installer 与 [`crate::skills::staging`]（marketplace git staging）**共同消费**——单向 `staging → manifest`。
//! **刻意保持 leaf**：**绝不** import `staging`（否则构成模块环）。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use smcp::utils::path::{is_within, normalize_lexical};

use crate::mcp_clients::model::MCPServerConfig;
use crate::skills::sources::DEFAULT_PLUGIN_ROOT;

// manifest 布局常量（marketplace-v1 §2.1/§3.1 + mcp-servers §1）/ manifest layout constants。
/// marketplace.json / plugin.json 所在目录（镜像 CC `.claude-plugin`）/ manifest dir。
pub const MARKETPLACE_MANIFEST_DIR: &str = ".tfrobot-plugin";
/// 仓库级 manifest 文件名 / repo-level manifest filename。
pub const MARKETPLACE_MANIFEST: &str = "marketplace.json";
/// plugin 级 manifest 文件名（仅元数据）/ plugin-level manifest filename。
pub const PLUGIN_MANIFEST: &str = "plugin.json";
/// plugin 携带 MCP server 的文件式子树 / bundled MCP server subdir。
pub const MCP_SERVERS_SUBDIR: &str = "mcp-servers";
/// plugin 范围 inputs 池定义（枚举 server 时排除）/ plugin inputs pool file (excluded from server enum)。
pub const MCP_INPUTS_FILENAME: &str = "inputs.json";

/// strict=false 下 plugin.json 不得再声明的组件字段全集（loading-behavior §6.2）/ component fields。
///
/// 本 SDK 仅消费 `skills` + `mcp-servers`，但冲突检测按规范判**全六字段**（前向兼容 CC 私有组件）。
pub const COMPONENT_FIELDS: &[&str] = &[
    "commands",
    "agents",
    "hooks",
    "skills",
    "mcpServers",
    "lspServers",
];

/// manifest / mcp-servers 文件解析或校验失败（致命，§3.3：不降级）/ Manifest parse/validate failure (fatal)。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct PluginManifestError(pub String);

impl PluginManifestError {
    fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

/// JSON 值是否真值（对齐 Python truthy：空串/空数组/空对象/0/false/null 皆假）/ JSON truthiness。
fn json_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// 读 JSON 文件并要求根为对象（失败 → [`PluginManifestError`]）/ Read a JSON file requiring an object root。
fn read_json_object(path: &Path, what: &str) -> Result<Map<String, Value>, PluginManifestError> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        PluginManifestError::new(format!(
            "{what} unreadable/invalid at {}: {e}",
            path.display()
        ))
    })?;
    let data: Value = serde_json::from_str(&text).map_err(|e| {
        PluginManifestError::new(format!(
            "{what} unreadable/invalid at {}: {e}",
            path.display()
        ))
    })?;
    match data {
        Value::Object(m) => Ok(m),
        _ => Err(PluginManifestError::new(format!(
            "{what} root is not an object: {}",
            path.display()
        ))),
    }
}

// ── marketplace.json（仓库级）/ marketplace-level manifest ────────────────────
/// 读 `<catalog>/.tfrobot-plugin/marketplace.json`（缺失/损坏 → 抛）/ Read the marketplace manifest。
///
/// # Errors
/// 文件缺失或非对象根 / JSON 损坏 → [`PluginManifestError`]。
pub fn read_marketplace_manifest(
    catalog_dir: &Path,
) -> Result<Map<String, Value>, PluginManifestError> {
    let path = catalog_dir
        .join(MARKETPLACE_MANIFEST_DIR)
        .join(MARKETPLACE_MANIFEST);
    if !path.is_file() {
        return Err(PluginManifestError::new(format!(
            "marketplace manifest not found: {}",
            path.display()
        )));
    }
    read_json_object(&path, "marketplace manifest")
}

/// 取 `manifest.plugins` 中的对象条目（非数组 → 空；非对象条目跳过）/ Object entries under `manifest.plugins`。
pub fn iter_plugin_entries(manifest: &Map<String, Value>) -> Vec<&Map<String, Value>> {
    match manifest.get("plugins") {
        Some(Value::Array(arr)) => arr.iter().filter_map(Value::as_object).collect(),
        _ => Vec::new(),
    }
}

/// 按 `entry.name == plugin_name` 定位 plugin 条目（marketplace-v1 §4.1）/ Locate a plugin entry by name。
pub fn find_plugin_entry<'a>(
    manifest: &'a Map<String, Value>,
    plugin_name: &str,
) -> Option<&'a Map<String, Value>> {
    iter_plugin_entries(manifest)
        .into_iter()
        .find(|e| e.get("name").and_then(Value::as_str).map(str::trim) == Some(plugin_name))
}

/// 取 `metadata.pluginRoot`（缺省 [`DEFAULT_PLUGIN_ROOT`]）/ Resolve the pluginRoot base prefix。
pub fn plugin_root_base(manifest: &Map<String, Value>) -> String {
    manifest
        .get("metadata")
        .and_then(Value::as_object)
        .and_then(|md| md.get("pluginRoot"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map_or_else(|| DEFAULT_PLUGIN_ROOT.to_string(), String::from)
}

// ── plugin.json（plugin 级，仅元数据）/ plugin-level metadata ──────────────────
/// 读 `<plugin>/.tfrobot-plugin/plugin.json`（best-effort，缺失/损坏 → 空 map）/ Read plugin.json best-effort。
pub fn read_plugin_metadata(plugin_root: &Path) -> Map<String, Value> {
    let path = plugin_root
        .join(MARKETPLACE_MANIFEST_DIR)
        .join(PLUGIN_MANIFEST);
    if !path.is_file() {
        return Map::new();
    }
    match read_json_object(&path, "plugin manifest") {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %e, "plugin manifest ignored");
            Map::new()
        }
    }
}

/// version 优先级：entry.version > plugin.json.version > git commit SHA / Resolve plugin version。
pub fn resolve_plugin_version(
    entry: &Map<String, Value>,
    plugin_metadata: &Map<String, Value>,
    fallback_sha: Option<&str>,
) -> Option<String> {
    for src in [entry, plugin_metadata] {
        if let Some(v) = src.get("version").and_then(Value::as_str) {
            let trimmed = v.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    fallback_sha.map(String::from)
}

// ── strict mode + 组件路径覆写（marketplace-v1 §4.3/§4.4）─────────────────────
/// marketplace 条目 `strict` 语义（默认 `true`；缺省/非 bool → `true`）/ Whether the entry is strict。
pub fn entry_is_strict(entry: &Map<String, Value>) -> bool {
    match entry.get("strict") {
        Some(Value::Bool(b)) => *b,
        _ => true,
    }
}

/// plugin.json 中**真值**组件字段名列表（空/缺/falsy 不算声明）/ Truthy component fields declared。
pub fn manifest_declared_components(plugin_manifest: &Map<String, Value>) -> Vec<&'static str> {
    COMPONENT_FIELDS
        .iter()
        .filter(|f| plugin_manifest.get(**f).is_some_and(json_truthy))
        .copied()
        .collect()
}

/// strict=false 且 plugin.json 声明组件 → 抛 conflicting manifests（§4.4）/ Hard-fail on conflict。
///
/// strict=true（默认）恒不冲突。
///
/// # Errors
/// strict=false 且 plugin.json 声明任一组件 → [`PluginManifestError`]。
pub fn check_strict_conflict(
    entry: &Map<String, Value>,
    plugin_manifest: &Map<String, Value>,
) -> Result<(), PluginManifestError> {
    if entry_is_strict(entry) {
        return Ok(());
    }
    let declared = manifest_declared_components(plugin_manifest);
    if !declared.is_empty() {
        return Err(PluginManifestError::new(format!(
            "conflicting manifests: strict=false but plugin.json declares components {declared:?} \
             (marketplace-v1 §4.4: the marketplace entry is the authoritative component set)"
        )));
    }
    Ok(())
}

/// 归一 `string | array<string>` → `Vec<String>`（非 str 项静默跳过）/ Normalize string|array<string>。
fn as_str_path_list(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(Value::as_str)
            .map(String::from)
            .collect(),
        _ => Vec::new(),
    }
}

/// 解析 `skills` 组件路径覆写为**容器目录**列表（§4.3，追加进约定 `skills/` 之外）/ Resolve skills override dirs。
///
/// 源 = `entry.skills`（恒取）+ `plugin_manifest.skills`（仅 strict=true）。每项相对 `plugin_root` 解析后
/// `is_within` 防穿越（越界记 ERROR 跳过、**不**抛），非目录记 WARNING 跳过；按解析后路径去重保序。
pub fn resolve_skill_override_dirs(
    entry: &Map<String, Value>,
    plugin_manifest: &Map<String, Value>,
    plugin_root: &Path,
    strict: bool,
) -> Vec<PathBuf> {
    let mut raw = as_str_path_list(entry.get("skills"));
    if strict {
        raw.extend(as_str_path_list(plugin_manifest.get("skills")));
    }

    let plugin_root_norm = normalize_lexical(plugin_root);
    let mut out: Vec<PathBuf> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    for rel in raw {
        let cand = normalize_lexical(&plugin_root.join(&rel));
        if !is_within(&cand, &plugin_root_norm) {
            tracing::error!(rel = %rel, plugin_root = %plugin_root.display(),
                "skills override path escapes plugin root, skipped");
            continue;
        }
        if seen.contains(&cand) {
            continue;
        }
        if !cand.is_dir() {
            tracing::warn!(rel = %rel, path = %cand.display(),
                "skills override path is not a directory, skipped");
            continue;
        }
        seen.insert(cand.clone());
        out.push(cand);
    }
    out
}

// ── mcp-servers/<n>.json（plugin 携带 MCP server，文件式）/ bundled MCP server files ──────────
/// 列 `<plugin>/mcp-servers/*.json`（一级，排除 `inputs.json`，排序）/ List bundled server files。
///
/// 无 `mcp-servers/` 目录 → 空（plugin 不携带 MCP server，合法）。
pub fn enumerate_bundled_server_files(plugin_root: &Path) -> Vec<PathBuf> {
    let dir = plugin_root.join(MCP_SERVERS_SUBDIR);
    if !dir.is_dir() {
        return Vec::new();
    }
    let mut files: Vec<PathBuf> = match std::fs::read_dir(&dir) {
        Ok(rd) => rd
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.is_file()
                    && p.extension().and_then(|x| x.to_str()) == Some("json")
                    && p.file_name().and_then(|n| n.to_str()) != Some(MCP_INPUTS_FILENAME)
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    files.sort();
    files
}

/// 解析单个 `mcp-servers/<name>.json` → 校验后的 [`MCPServerConfig`] / Parse one bundled server file。
///
/// 强制**文件名（去 `.json`）== 配置内 `name`**（mcp-servers 协议 §1）。
///
/// # ⚠️ #130 起：畸形 `bundle_id` 会在此判废，爆炸半径 = **整个 plugin**（非单个 server）
///
/// [`BundleId`](crate::mcp_clients::model::BundleId) 构造即校验后，`bundle_id` 成为 **parse 级**判据——
/// 此前它**不是**（`Option<String>` 恒解析成功、到 manager 注册期才逐条诊断跳过）。因 [`load_bundled_servers`]
/// 是 `collect::<Result<Vec<_>, _>>()` **原子**语义，一个畸形 `bundle_id` ⇒ 该 plugin 的**全部** bundled server
/// 一起废：install / enable **硬失败**（合理——install 原子前置，§2.4 禁半态），但 **boot recovery** 路径
/// （`settings::recovery`）会 WARN 跳过该 plugin 的全部 server。
///
/// **升级路径注意**：旧版下已安装、携畸形 `bundleId` 的 plugin，升级后其 bundled server 会整包消失（仅 WARN）。
/// recovery 路径是否应改为**逐-server** 降级（install 保持原子）是待评估项。对比：`mcp.json` 路径是**逐-server**
/// 降级（`settings::mcp_config::validate_server`，整份文件不 abort）——两条路径的粒度**有意不同**。
///
/// # Errors
/// 解析/校验失败（含畸形 `bundle_id`）或文件名 stem 与 `name` 不一致 → [`PluginManifestError`]。
pub fn parse_bundled_server(path: &Path) -> Result<MCPServerConfig, PluginManifestError> {
    let data = read_json_object(path, "bundled MCP server")?;
    let cfg: MCPServerConfig = serde_json::from_value(Value::Object(data)).map_err(|e| {
        PluginManifestError::new(format!(
            "invalid MCP server config at {}: {e}",
            path.display()
        ))
    })?;
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    if cfg.name() != stem {
        return Err(PluginManifestError::new(format!(
            "bundled MCP server filename stem {:?} != config name {:?} at {} \
             (mcp-servers protocol §1: filename = server identity)",
            stem,
            cfg.name(),
            path.display()
        )));
    }
    Ok(cfg)
}

/// 枚举并解析一个 plugin 的全部 bundled MCP server 配置（任一失败即抛）/ Parse all bundled servers (atomic)。
///
/// # Errors
/// 任一文件解析/校验失败 → [`PluginManifestError`]（install 不留半装）。无 server → 空。
pub fn load_bundled_servers(
    plugin_root: &Path,
) -> Result<Vec<MCPServerConfig>, PluginManifestError> {
    enumerate_bundled_server_files(plugin_root)
        .iter()
        .map(|p| parse_bundled_server(p))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use tempfile::TempDir;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    fn write(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[test]
    fn test_read_marketplace_manifest() {
        let tmp = TempDir::new().unwrap();
        let mf = tmp.path().join(".tfrobot-plugin/marketplace.json");
        write(&mf, r#"{"plugins": [{"name": "audit"}]}"#);
        let m = read_marketplace_manifest(tmp.path()).unwrap();
        assert_eq!(iter_plugin_entries(&m).len(), 1);
        // 缺失 → 抛。
        assert!(read_marketplace_manifest(TempDir::new().unwrap().path()).is_err());
        // 非对象根 → 抛。
        let tmp2 = TempDir::new().unwrap();
        write(
            &tmp2.path().join(".tfrobot-plugin/marketplace.json"),
            "[1,2]",
        );
        assert!(read_marketplace_manifest(tmp2.path()).is_err());
    }

    #[test]
    fn test_plugin_root_base_and_entries() {
        assert_eq!(plugin_root_base(&Map::new()), "./plugins");
        assert_eq!(
            plugin_root_base(&obj(json!({"metadata": {"pluginRoot": " ./pkgs "}}))),
            "./pkgs"
        );
        // plugins 非数组 → 空。
        assert!(iter_plugin_entries(&obj(json!({"plugins": "x"}))).is_empty());
        // find_plugin_entry。
        let m = obj(json!({"plugins": [{"name": "a"}, {"name": "b"}]}));
        assert!(find_plugin_entry(&m, "b").is_some());
        assert!(find_plugin_entry(&m, "z").is_none());
    }

    #[test]
    fn test_plugin_metadata_and_version() {
        let tmp = TempDir::new().unwrap();
        // best-effort：缺失 → {}。
        assert!(read_plugin_metadata(tmp.path()).is_empty());
        // 损坏 → {}（不抛）。
        write(&tmp.path().join(".tfrobot-plugin/plugin.json"), "{bad json");
        assert!(read_plugin_metadata(tmp.path()).is_empty());
        // version 优先级 entry > plugin > sha。
        assert_eq!(
            resolve_plugin_version(
                &obj(json!({"version": "1.0"})),
                &obj(json!({"version": "2.0"})),
                Some("sha")
            ),
            Some("1.0".to_string())
        );
        assert_eq!(
            resolve_plugin_version(&Map::new(), &obj(json!({"version": "2.0"})), Some("sha")),
            Some("2.0".to_string())
        );
        assert_eq!(
            resolve_plugin_version(&Map::new(), &Map::new(), Some("sha")),
            Some("sha".to_string())
        );
        assert_eq!(resolve_plugin_version(&Map::new(), &Map::new(), None), None);
    }

    #[test]
    fn test_strict_mode() {
        // entry_is_strict 默认 true；显式 false；非 bool → true。
        assert!(entry_is_strict(&Map::new()));
        assert!(!entry_is_strict(&obj(json!({"strict": false}))));
        assert!(entry_is_strict(&obj(json!({"strict": "false"}))));
        // declared components（真值过滤）。
        let pm = obj(json!({"skills": ["x"], "agents": [], "hooks": null, "commands": "c"}));
        let declared = manifest_declared_components(&pm);
        assert!(declared.contains(&"skills") && declared.contains(&"commands"));
        assert!(!declared.contains(&"agents") && !declared.contains(&"hooks"));
        // strict=true 恒不冲突。
        assert!(check_strict_conflict(&obj(json!({"strict": true})), &pm).is_ok());
        // strict=false + 声明组件 → 冲突。
        assert!(check_strict_conflict(&obj(json!({"strict": false})), &pm).is_err());
        // strict=false 但仅 metadata（无组件）→ ok。
        assert!(check_strict_conflict(
            &obj(json!({"strict": false})),
            &obj(json!({"version": "1"}))
        )
        .is_ok());
    }

    #[test]
    fn test_resolve_skill_override_dirs() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("extra-skills")).unwrap();
        fs::create_dir_all(root.join("pj-skills")).unwrap();
        // entry.skills + plugin.json.skills（strict=true 合并）。
        let entry = obj(json!({"skills": "extra-skills"}));
        let pm = obj(json!({"skills": ["pj-skills", "extra-skills"]})); // 含重复 + 与 entry 同径
        let dirs = resolve_skill_override_dirs(&entry, &pm, root, true);
        // 去重保序：extra-skills, pj-skills。
        assert_eq!(dirs.len(), 2);
        assert!(dirs[0].ends_with("extra-skills"));
        assert!(dirs[1].ends_with("pj-skills"));
        // 越界 / 非目录跳过。
        let bad = obj(json!({"skills": ["../escape", "nonexistent"]}));
        assert!(resolve_skill_override_dirs(&bad, &Map::new(), root, false).is_empty());
        // strict=false 不并入 plugin.json.skills。
        assert!(resolve_skill_override_dirs(&Map::new(), &pm, root, false).is_empty());
    }

    #[test]
    fn test_bundled_servers() {
        let tmp = TempDir::new().unwrap();
        let pr = tmp.path();
        // 无 mcp-servers/ → 空。
        assert!(load_bundled_servers(pr).unwrap().is_empty());
        let sd = pr.join("mcp-servers");
        fs::create_dir_all(&sd).unwrap();
        write(
            &sd.join("figma.json"),
            r#"{"type":"stdio","name":"figma","server_parameters":{"command":"node"}}"#,
        );
        write(
            &sd.join("alpha.json"),
            r#"{"type":"stdio","name":"alpha","server_parameters":{"command":"go"}}"#,
        );
        write(&sd.join("inputs.json"), r#"{"x":1}"#); // 排除
        write(&sd.join("notes.txt"), "x"); // 排除（非 .json）
                                           // 枚举排序、排除 inputs/非 json。
        let files = enumerate_bundled_server_files(pr);
        assert_eq!(files.len(), 2);
        assert!(files[0].ends_with("alpha.json")); // sorted
        let servers = load_bundled_servers(pr).unwrap();
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].name(), "alpha");
        // stem != name → 抛。
        write(
            &sd.join("wrong.json"),
            r#"{"type":"stdio","name":"figma","server_parameters":{"command":"node"}}"#,
        );
        assert!(parse_bundled_server(&sd.join("wrong.json")).is_err());
        // 任一畸形 → load 原子失败。
        write(&sd.join("bad.json"), "{not json");
        assert!(load_bundled_servers(pr).is_err());
    }
}
