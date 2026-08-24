/*!
* 文件名: schema.rs
* 作者: JQQ
* 创建日期: 2026/06/01
* 最后修改日期: 2026/06/01
* 版权: 2023 JQQ. All rights reserved.
* 依赖: serde, serde_json, regex
* 描述: 意图层 settings.json 结构与字段级容错校验 / Intent-layer settings.json schema & validation
*/

//! 意图层 settings.json 结构与字段级容错校验。
//! Intent-layer settings.json schema & field-level tolerant validation.
//!
//! 对标 Python 参考实现 `a2c_smcp/computer/settings/schema.py`（约 388 行）。
//!
//! `settings.json` 是 Computer 的**意图 / 治理层**（plugin 启停、marketplace 声明、MCP 门控、
//! trust policy），**不**承载 MCP server 定义 / inputs（那些在 `.tfrobot/mcp.json`）。
//!
//! 前向兼容模型（复刻 Claude Code）/ Forward-compat model (mirrors Claude Code):
//! - **全字段可选 + 顶层 passthrough**：未知顶层键**静默保留**、不报错、不剥离（升降级都不丢）。
//! - **无 version 字段**：人编文件不背版本负担；`version` 只在 CLI 维护的物化文件。
//! - **字段级容错**：单个已知键 / map 项 / 数组元素校验失败 → **逐条过滤**（回退默认）、其余保留、
//!   错误收进 [`SettingsValidationError`] 列表（错误不阻断启动、不整文件作废）。
//! - **scope 越权**（两类，均为「过滤 + 记错」）：
//!   1. [`POLICY_ONLY_FIELDS`]：policy-only；出现在非 policy scope → 过滤 + 记错（杜绝用户态自我提权）。
//!   2. [`TRUSTED_SCOPE_ONLY_FIELDS`]：审批门 **enable 方向**判据；出现在 **project** scope → 过滤 + 记错
//!      （杜绝**不受信 scope 自我批准**，#143 / 协议 `mcp-approval-gate-alignment.md` §2.1）。

use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// settings 来源 scope（低 → 高，high 覆盖 low）/ Settings source scope (low → high).
///
/// 与 Claude Code 对齐（user/project/local/flag/policy）。`Embed` 为 A2C 特有的**宿主构造挂载层**
/// （`Computer::new(mcp_servers=…)`），**仅存在于 mcp.json 轴**——settings.json 无此来源（#147/S14）。
/// #98：能力发现层 `Capability` 已随 workdir 概念瘦身移除（对齐 protocol#10 / python-sdk#116）——
/// project/local 现锚定进程 cwd，`enabledPlugins` / `extraKnownMarketplaces` 经常规 project/local 层进入，
/// 无需专门的能力并集层。
///
/// **顺序权威在 [`ProvenanceScope::priority`](super::config::ProvenanceScope::priority)**（协议
/// `runtime-contract.md` §2.5 第3条 `plugin < user < project < local < embed < flag < policy`），
/// **不在本枚举的成员声明序**——成员序恰好一致，勿依赖。
///
/// 序列化为小写字符串，与 Python `StrEnum` 字面一致（`user` / `project` / `local` / `embed` / `flag` /
/// `policy`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SettingsScope {
    User,
    Project,
    Local,
    /// 宿主构造挂载（`Computer::new(mcp_servers=…)`）；仅 mcp.json 轴（#147）/ embedded-constructor, mcp.json axis only。
    Embed,
    Flag,
    /// 企业策略层（最高）/ enterprise policy (highest)。
    Policy,
}

impl SettingsScope {
    /// 返回 scope 的小写字面量（与序列化一致）/ Lowercase literal (matches serialization).
    pub fn as_str(self) -> &'static str {
        match self {
            SettingsScope::User => "user",
            SettingsScope::Project => "project",
            SettingsScope::Local => "local",
            SettingsScope::Embed => "embed",
            SettingsScope::Flag => "flag",
            SettingsScope::Policy => "policy",
        }
    }
}

// ---------------------------------------------------------------------------
// 已知字段名（camelCase，对齐 CC / JSON schema）/ Known field names (camelCase)
// ---------------------------------------------------------------------------
/// `$schema`（编辑器补全用，CLI 不消费）/ editor-only, not consumed by the CLI。
pub const FIELD_SCHEMA: &str = "$schema";
pub const FIELD_EXTRA_KNOWN_MARKETPLACES: &str = "extraKnownMarketplaces";
pub const FIELD_ENABLED_PLUGINS: &str = "enabledPlugins";
pub const FIELD_STRICT_KNOWN_MARKETPLACES: &str = "strictKnownMarketplaces";
pub const FIELD_TRUSTED_MARKETPLACES: &str = "trustedMarketplaces";
pub const FIELD_BLOCKED_MARKETPLACES: &str = "blockedMarketplaces";
pub const FIELD_ENABLE_ALL_PROJECT_MCP: &str = "enableAllProjectMcpServers";
pub const FIELD_ENABLED_MCPJSON_SERVERS: &str = "enabledMcpjsonServers";
pub const FIELD_DISABLED_MCPJSON_SERVERS: &str = "disabledMcpjsonServers";
pub const FIELD_ALLOWED_MCP_SERVERS: &str = "allowedMcpServers";
pub const FIELD_DENIED_MCP_SERVERS: &str = "deniedMcpServers";
pub const FIELD_PERMISSIONS: &str = "permissions";
/// `client:put_blob` 落盘根（v0.4.0，协议 blob-transfer.md §7 顶层键，camelCase）/ landing root.
pub const FIELD_LANDING_ROOT: &str = "landingRoot";

/// policy-only 字段：出现在非 policy scope → 过滤 + 记错（杜绝用户态自我提权）。
/// Policy-only fields: filtered + recorded if seen outside the policy scope.
pub const POLICY_ONLY_FIELDS: &[&str] = &[FIELD_ALLOWED_MCP_SERVERS, FIELD_DENIED_MCP_SERVERS];

/// 审批门 **enable 方向**判据：出现在 **project** scope → 过滤 + 记错（#143）。
/// Approval-gate ENABLE-direction inputs: filtered + recorded if supplied by the **project** scope.
///
/// 协议依据：[审批门对齐指南][guide] §2.1 通则 ——
/// > 审批门的输入 MUST 来自比被判定 server 更高信任的来源；任何 scope 都不得为「自身是否受信」提供判据。
///
/// `.tfrobot/settings.json`（project scope）与 `mcp.json` 一样**入 git、随仓库分发**。若门接受它供给
/// 档⑤/⑥，被 clone 的仓库携一份 `{"enableAllProjectMcpServers": true}` 即可让其 `mcp.json` 里的任意
/// server 启动期免批准框直挂 —— 与 #131 删掉的档④ **同构且更易达成**（无需装任何插件、无需猜任何名字）。
///
/// # 为何**只拒 `Project`**（而非复用 mcp.json 声明面的预信任集 `is_trusted_origin`）
///
/// 受信供给方 = `user` / `local` / `flag` / `policy`（协议 §2.1 表）——**含 `Local`**。这与 mcp.json
/// **声明面**的预信任集（`ProvenanceScope::is_trusted_origin` = `{user, embed, flag, policy}`，文件 scope
/// 落到即 `{user, flag, policy}`，**不含 Local**）**有意不同**，勿混用：
/// 三个批准写助手（`approve_mcp_server` / `deny_mcp_server` / `approve_all_project_mcp`）**只写 local
/// scope**（个人决定不污染共享层）。若把 `Local` 也判为不受信，**每次批准都会在读回时被自己过滤掉、
/// 批准永远不生效**。读面与写面 MUST 对称。
///
/// # 为何**不含** `disabledMcpjsonServers`
///
/// 那是 **DENY** 方向（协议 §2.1 表第 3 行）：**任意 scope（含 project）可供给** —— fail-safe，仓库禁自己的
/// server 无安全影响，更严格永远安全。把它收进本类目属**过度矫正**，由
/// `disabled_mcpjson_from_project_scope_is_honored_143` 守护。
///
/// # 为何**含** `landingRoot`（v0.4.0，协议 blob-transfer.md §7）
///
/// 那是 `client:put_blob` 的**写目标**：project settings 与 `mcp.json` 一样入 git、随仓库分发，clone 的
/// 仓库不得把写目标重定向到任意路径（如 `~/.ssh`）——否则掏空「写入沙箱由写入原语强制」的
/// 不变量。协议 §7 明文：`landingRoot` 仅受信 scope（`user`/`local`/`flag`/`policy`/`embed`）可设；
/// **`project` scope 提供该键 MUST 被拒绝**（本列表 + 下方 project 过滤即达成）。
///
/// [guide]: https://github.com/A2C-SMCP/a2c-smcp-protocol/blob/develop/docs/guides/mcp-approval-gate-alignment.md
pub const TRUSTED_SCOPE_ONLY_FIELDS: &[&str] = &[
    FIELD_ENABLED_MCPJSON_SERVERS,
    FIELD_ENABLE_ALL_PROJECT_MCP,
    FIELD_LANDING_ROOT,
];

/// 字符串数组字段（读合并：拼接去重；写回：整体替换）/ String-array fields.
pub const STRING_ARRAY_FIELDS: &[&str] = &[
    FIELD_TRUSTED_MARKETPLACES,
    FIELD_BLOCKED_MARKETPLACES,
    FIELD_ENABLED_MCPJSON_SERVERS,
    FIELD_DISABLED_MCPJSON_SERVERS,
    FIELD_ALLOWED_MCP_SERVERS,
    FIELD_DENIED_MCP_SERVERS,
];

/// 标量布尔字段（读合并：取最高 scope）/ Scalar bool fields (highest scope wins).
pub const BOOL_FIELDS: &[&str] = &[
    FIELD_STRICT_KNOWN_MARKETPLACES,
    FIELD_ENABLE_ALL_PROJECT_MCP,
];

// ---------------------------------------------------------------------------
// 形态正则 / Shape regexes（与 Python `re` 模式逐字一致）
// ---------------------------------------------------------------------------
/// marketplace / plugin 名：严格 kebab。
static KEBAB_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z0-9]+(?:-[a-z0-9]+)*$").unwrap());
/// `enabledPlugins` key 形态 `<plugin>@<marketplace>`。
static PLUGIN_AT_MP_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z0-9]+(?:-[a-z0-9]+)*@[a-z0-9]+(?:-[a-z0-9]+)*$").unwrap());
/// git url：scp-like `user@host:path`。
static GIT_SCP_LIKE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[\w.+-]+@[\w.-]+:.+$").unwrap());
/// git url：URL scheme（ssh/git/http/https/file）。
static GIT_URL_SCHEME_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:ssh|git|https?|file)://.+$").unwrap());

/// marketplace / plugin / enabledPlugins 段名最大长度 / max name length.
pub const MAX_NAME_LEN: usize = 64;

/// marketplace / plugin 名是否合规（严格 kebab，1–64）/ Whether a marketplace/plugin name is valid.
pub fn is_valid_marketplace_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAX_NAME_LEN && KEBAB_RE.is_match(name)
}

/// `enabledPlugins` key 是否形如 `<plugin>@<marketplace>` / Whether the key matches `<plugin>@<mp>`.
pub fn is_valid_enabled_plugin_key(key: &str) -> bool {
    if !PLUGIN_AT_MP_RE.is_match(key) {
        return false;
    }
    match key.split_once('@') {
        Some((plugin, marketplace)) => {
            plugin.len() <= MAX_NAME_LEN && marketplace.len() <= MAX_NAME_LEN
        }
        None => false,
    }
}

/// git url 形态是否合规（scp-like 或 ssh/git/http(s)/file scheme）/ Whether a git url is well-formed.
pub fn is_valid_git_url(url: &str) -> bool {
    if url.trim().is_empty() {
        return false;
    }
    GIT_URL_SCHEME_RE.is_match(url) || GIT_SCP_LIKE_RE.is_match(url)
}

/// 绝对路径判定：POSIX `/...` 或 Windows 盘符 `X:\` / `X:/` / Absolute path (POSIX or Windows drive).
fn is_absolute_path(p: &str) -> bool {
    if p.starts_with('/') {
        return true;
    }
    let b = p.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')
}

/// JSON 值的类型名（用于诊断 reason 文案）/ Type name of a JSON value (for diagnostics).
fn json_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

// ---------------------------------------------------------------------------
// 字段级容错校验 / Field-level tolerant validation
// ---------------------------------------------------------------------------
/// 单条 settings 校验错误（不阻断启动，经 `settings show` / 诊断命令呈现）。
/// A single non-fatal settings validation error.
///
/// `field` 为点路径（如 `extraKnownMarketplaces.my-mp.source.url`）；`scope` 标明来源层；
/// `source_path` 为产生该错误的文件（可空，便于诊断定位）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SettingsValidationError {
    pub scope: SettingsScope,
    pub field: String,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
}

fn err(
    scope: SettingsScope,
    field: impl Into<String>,
    reason: impl Into<String>,
) -> SettingsValidationError {
    SettingsValidationError {
        scope,
        field: field.into(),
        reason: reason.into(),
        source_path: None,
    }
}

/// 字段校验器返回：`(Some(cleaned) | None=DROP, errors)`。`None` 表示整字段判废、回退默认。
type ValidateResult = (Option<Value>, Vec<SettingsValidationError>);

fn validate_bool(key: &str, value: &Value, scope: SettingsScope) -> ValidateResult {
    if value.is_boolean() {
        (Some(value.clone()), vec![])
    } else {
        (
            None,
            vec![err(
                scope,
                key,
                format!("expected bool, got {}", json_type_name(value)),
            )],
        )
    }
}

fn validate_string_array(key: &str, value: &Value, scope: SettingsScope) -> ValidateResult {
    let Some(arr) = value.as_array() else {
        return (
            None,
            vec![err(
                scope,
                key,
                format!("expected array, got {}", json_type_name(value)),
            )],
        );
    };
    let mut cleaned: Vec<Value> = Vec::new();
    let mut errors: Vec<SettingsValidationError> = Vec::new();
    for (idx, item) in arr.iter().enumerate() {
        if let Some(s) = item.as_str() {
            cleaned.push(Value::String(s.to_string()));
        } else {
            errors.push(err(
                scope,
                format!("{key}[{idx}]"),
                format!("expected string, got {}", json_type_name(item)),
            ));
        }
    }
    (Some(Value::Array(cleaned)), errors)
}

/// `landingRoot`：绝对路径字符串（协议 blob-transfer.md §7；镜像 python `_validate_landing_root`）。
fn validate_landing_root(key: &str, value: &Value, scope: SettingsScope) -> ValidateResult {
    match value.as_str() {
        Some(s) if !s.trim().is_empty() && is_absolute_path(s.trim()) => {
            (Some(Value::String(s.trim().to_string())), vec![])
        }
        Some(s) => (
            None,
            vec![err(
                scope,
                key,
                format!(
                    "landingRoot must be an absolute path string, got relative path {s:?}"
                ),
            )],
        ),
        None => (
            None,
            vec![err(
                scope,
                key,
                format!("expected absolute path string, got {}", json_type_name(value)),
            )],
        ),
    }
}

fn validate_enabled_plugins(key: &str, value: &Value, scope: SettingsScope) -> ValidateResult {
    let Some(obj) = value.as_object() else {
        return (
            None,
            vec![err(
                scope,
                key,
                format!("expected object, got {}", json_type_name(value)),
            )],
        );
    };
    let mut cleaned = serde_json::Map::new();
    let mut errors: Vec<SettingsValidationError> = Vec::new();
    for (plugin_key, enabled) in obj {
        if !is_valid_enabled_plugin_key(plugin_key) {
            errors.push(err(
                scope,
                format!("{key}.{plugin_key}"),
                "key must be '<plugin>@<marketplace>' (strict kebab)",
            ));
            continue;
        }
        if !enabled.is_boolean() {
            errors.push(err(
                scope,
                format!("{key}.{plugin_key}"),
                format!("value must be bool, got {}", json_type_name(enabled)),
            ));
            continue;
        }
        cleaned.insert(plugin_key.clone(), enabled.clone());
    }
    (Some(Value::Object(cleaned)), errors)
}

fn validate_extra_marketplaces(key: &str, value: &Value, scope: SettingsScope) -> ValidateResult {
    let Some(obj) = value.as_object() else {
        return (
            None,
            vec![err(
                scope,
                key,
                format!("expected object, got {}", json_type_name(value)),
            )],
        );
    };
    let mut cleaned = serde_json::Map::new();
    let mut errors: Vec<SettingsValidationError> = Vec::new();
    for (name, entry) in obj {
        let path = format!("{key}.{name}");
        if !is_valid_marketplace_name(name) {
            errors.push(err(
                scope,
                path,
                "marketplace name must be strict kebab (1-64)",
            ));
            continue;
        }
        let Some(entry_obj) = entry.as_object() else {
            errors.push(err(
                scope,
                path,
                format!("entry must be object, got {}", json_type_name(entry)),
            ));
            continue;
        };
        let source_obj = entry_obj.get("source").and_then(Value::as_object);
        let Some(source_obj) = source_obj else {
            errors.push(err(
                scope,
                format!("{path}.source"),
                "missing or non-object 'source'",
            ));
            continue;
        };
        if source_obj.get("type").and_then(Value::as_str) != Some("git") {
            errors.push(err(
                scope,
                format!("{path}.source.type"),
                "only source.type == 'git' is supported",
            ));
            continue;
        }
        let url = source_obj.get("url").and_then(Value::as_str).unwrap_or("");
        if !is_valid_git_url(url) {
            errors.push(err(
                scope,
                format!("{path}.source.url"),
                "malformed git url",
            ));
            continue;
        }
        let auto_update = entry_obj.get("autoUpdate");
        if let Some(au) = auto_update {
            if !au.is_boolean() {
                errors.push(err(
                    scope,
                    format!("{path}.autoUpdate"),
                    format!("must be bool, got {}", json_type_name(au)),
                ));
                continue;
            }
        }
        // 仅保留规范化后的已知子字段（source.{type,url} / autoUpdate）；其余子键静默丢（爆炸半径 0）。
        let mut src = serde_json::Map::new();
        src.insert("type".to_string(), Value::String("git".to_string()));
        src.insert("url".to_string(), Value::String(url.to_string()));
        let mut normalized = serde_json::Map::new();
        normalized.insert("source".to_string(), Value::Object(src));
        if let Some(au) = auto_update {
            normalized.insert("autoUpdate".to_string(), au.clone());
        }
        cleaned.insert(name.clone(), Value::Object(normalized));
    }
    (Some(Value::Object(cleaned)), errors)
}

fn validate_permissions(key: &str, value: &Value, scope: SettingsScope) -> ValidateResult {
    let Some(obj) = value.as_object() else {
        return (
            None,
            vec![err(
                scope,
                key,
                format!("expected object, got {}", json_type_name(value)),
            )],
        );
    };
    let mut cleaned = serde_json::Map::new();
    let mut errors: Vec<SettingsValidationError> = Vec::new();
    for (sub_key, sub_value) in obj {
        if sub_key == "additionalDirectories" {
            let Some(arr) = sub_value.as_array() else {
                errors.push(err(
                    scope,
                    format!("{key}.additionalDirectories"),
                    "expected array",
                ));
                continue;
            };
            let mut dirs: Vec<Value> = Vec::new();
            for (idx, item) in arr.iter().enumerate() {
                match item.as_str() {
                    None => errors.push(err(
                        scope,
                        format!("{key}.additionalDirectories[{idx}]"),
                        "expected string",
                    )),
                    Some(s) if is_absolute_path(s) => dirs.push(Value::String(s.to_string())),
                    Some(_) => errors.push(err(
                        scope,
                        format!("{key}.additionalDirectories[{idx}]"),
                        "must be an absolute path",
                    )),
                }
            }
            cleaned.insert("additionalDirectories".to_string(), Value::Array(dirs));
        } else {
            // permissions 下未知子键 passthrough（与顶层 passthrough 同纪律）。
            cleaned.insert(sub_key.clone(), sub_value.clone());
        }
    }
    (Some(Value::Object(cleaned)), errors)
}

/// 按字段名分派校验器；`None` = 未知字段（passthrough）/ dispatch validator; `None` = unknown field.
fn dispatch_validator(key: &str, value: &Value, scope: SettingsScope) -> Option<ValidateResult> {
    let validator: fn(&str, &Value, SettingsScope) -> ValidateResult = match key {
        FIELD_EXTRA_KNOWN_MARKETPLACES => validate_extra_marketplaces,
        FIELD_ENABLED_PLUGINS => validate_enabled_plugins,
        FIELD_STRICT_KNOWN_MARKETPLACES | FIELD_ENABLE_ALL_PROJECT_MCP => validate_bool,
        FIELD_TRUSTED_MARKETPLACES
        | FIELD_BLOCKED_MARKETPLACES
        | FIELD_ENABLED_MCPJSON_SERVERS
        | FIELD_DISABLED_MCPJSON_SERVERS
        | FIELD_ALLOWED_MCP_SERVERS
        | FIELD_DENIED_MCP_SERVERS => validate_string_array,
        FIELD_PERMISSIONS => validate_permissions,
        FIELD_LANDING_ROOT => validate_landing_root,
        _ => return None,
    };
    Some(validator(key, value, scope))
}

/// 对单个 scope 的原始 settings 值做**字段级容错**校验 / Field-level tolerant validation for one scope.
///
/// 语义（照 Claude Code，错误不阻断启动）/ Semantics (Claude Code; never blocks startup):
/// - 非 object 根 → 返回 `({}, [error("<root>")])`（整层判废，回退空）。
/// - `$schema`：原样保留、CLI 不消费。
/// - **policy-only 字段在非 policy scope** → 过滤 + 记错。
/// - 已知字段：经对应校验器**逐条过滤**非法 map 项 / 数组元素；整字段类型错 → 回退默认（剔除）。
/// - 未知字段：**静默保留**（passthrough）。
///
/// 返回 `(cleaned, errors)`——`cleaned` 供跨 scope 合并（SET-02）；磁盘文件**不**被本函数改写。
/// 镜像 Python 的 `dict` 返回（保留 `$schema` + passthrough 未知键），故返回 [`serde_json::Map`]
/// 而非类型化的 [`ComputerSettings`]（后者是独立的 typed round-trip 视图）。
///
/// - `raw`：单个 settings 文件 parse 后的值。
/// - `scope`：该文件所属 scope。
/// - `source_path`：文件路径（用于错误溯源，可空）。
pub fn validate_settings(
    raw: &Value,
    scope: SettingsScope,
    source_path: Option<&str>,
) -> (serde_json::Map<String, Value>, Vec<SettingsValidationError>) {
    let Some(obj) = raw.as_object() else {
        return (
            serde_json::Map::new(),
            vec![SettingsValidationError {
                scope,
                field: "<root>".to_string(),
                reason: "settings root must be an object".to_string(),
                source_path: source_path.map(str::to_string),
            }],
        );
    };

    let mut cleaned = serde_json::Map::new();
    let mut errors: Vec<SettingsValidationError> = Vec::new();

    for (key, value) in obj {
        if key == FIELD_SCHEMA {
            cleaned.insert(key.clone(), value.clone()); // 编辑器补全用，CLI 不消费。
            continue;
        }
        if POLICY_ONLY_FIELDS.contains(&key.as_str()) && scope != SettingsScope::Policy {
            errors.push(err(
                scope,
                key.as_str(),
                "policy-only field not allowed outside the policy scope (filtered)",
            ));
            continue;
        }
        // #143：审批门 enable 方向判据 MUST NOT 由 project scope（入 git、随仓库分发）供给——
        // 否则被 clone 的仓库可自我批准（与档④ 同构）。协议指南 §2.1。DENY 方向不在本类目（fail-safe）。
        if TRUSTED_SCOPE_ONLY_FIELDS.contains(&key.as_str()) && scope == SettingsScope::Project {
            errors.push(err(
                scope,
                key.as_str(),
                "trusted-scope-only field not allowed in the project scope (filtered): approval-gate \
                 inputs are personal decisions and landingRoot is a write target — both are unfit for \
                 the git-tracked project settings.json; move it to settings.local.json (not \
                 git-tracked) or the user scope",
            ));
            continue;
        }
        match dispatch_validator(key, value, scope) {
            None => {
                cleaned.insert(key.clone(), value.clone()); // passthrough 未知字段。
            }
            Some((result, field_errors)) => {
                errors.extend(field_errors);
                if let Some(v) = result {
                    cleaned.insert(key.clone(), v);
                }
            }
        }
    }

    // 把 source_path 回填进本批错误（校验器内部不知道文件路径）。
    if let Some(sp) = source_path {
        for e in errors.iter_mut() {
            if e.source_path.is_none() {
                e.source_path = Some(sp.to_string());
            }
        }
    }

    (cleaned, errors)
}

// ---------------------------------------------------------------------------
// TypedDict 结构（typed round-trip 视图；运行时容错校验走 validate_settings）/ Typed shapes
// ---------------------------------------------------------------------------
/// marketplace git 源 / marketplace git source（`extraKnownMarketplaces[].source`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitSource {
    /// 固定 `"git"` / fixed `"git"`。
    #[serde(rename = "type")]
    pub source_type: String,
    pub url: String,
}

/// `extraKnownMarketplaces` 单条声明 / a single marketplace declaration。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketplaceEntry {
    pub source: GitSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_update: Option<bool>,
}

/// `permissions` 块 / the `permissions` block。
///
/// 不派生 `Eq`：`extra` 持 [`serde_json::Value`]（含 `f64`，非 `Eq`）。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionsBlock {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub additional_directories: Option<Vec<String>>,
    /// permissions 下未知子键 passthrough / unknown sub-keys passthrough。
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

/// settings.json 完整结构（全可选；未知顶层键经 `extra` flatten passthrough）。
/// Full settings.json shape (all optional; unknown top-level keys captured by `extra`).
///
/// 本结构是 typed round-trip 视图；运行时容错校验见 [`validate_settings`]（其返回的是 cleaned
/// [`serde_json::Map`]，而非本结构）。
///
/// 不派生 `Eq`：`extra` 持 [`serde_json::Value`]（含 `f64`，非 `Eq`）。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComputerSettings {
    #[serde(rename = "$schema", skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra_known_marketplaces: Option<std::collections::BTreeMap<String, MarketplaceEntry>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled_plugins: Option<std::collections::BTreeMap<String, bool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strict_known_marketplaces: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trusted_marketplaces: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked_marketplaces: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enable_all_project_mcp_servers: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled_mcpjson_servers: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled_mcpjson_servers: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_mcp_servers: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub denied_mcp_servers: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permissions: Option<PermissionsBlock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub landing_root: Option<String>,
    /// 未知顶层字段 passthrough（前向兼容）/ unknown top-level fields passthrough.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---- 形态校验器 / shape validators ----
    #[test]
    fn test_is_valid_git_url() {
        let cases = [
            ("git@github.com:team/skills.git", true),
            ("https://github.com/team/skills.git", true),
            ("http://example.com/x.git", true),
            ("ssh://git@host:22/team/skills.git", true),
            ("git://host/path", true),
            ("file:///abs/path/repo", true),
            ("not a url", false),
            ("ftp://host/x", false),
            ("", false),
            ("   ", false),
        ];
        for (url, ok) in cases {
            assert_eq!(is_valid_git_url(url), ok, "git url: {url:?}");
        }
    }

    #[test]
    fn test_is_valid_enabled_plugin_key() {
        let cases = [
            ("frontend-design@my-team-skills", true),
            ("a@b", true),
            ("foo", false), // 缺 @
            ("foo@", false),
            ("@mp", false),
            ("Foo@mp", false),    // 大写非 kebab
            ("foo@my_mp", false), // 下划线非 kebab
            ("-foo@mp", false),   // 首字符 -
        ];
        for (key, ok) in cases {
            assert_eq!(is_valid_enabled_plugin_key(key), ok, "plugin key: {key:?}");
        }
    }

    #[test]
    fn test_is_valid_marketplace_name() {
        let cases = [
            ("my-team-skills", true),
            ("a", true),
            ("Foo", false),
            ("a--b", false), // 连续 --
            ("a_b", false),
            ("", false),
        ];
        for (name, ok) in cases {
            assert_eq!(is_valid_marketplace_name(name), ok, "mp name: {name:?}");
        }
        // 超长 (>64) 被拒
        assert!(!is_valid_marketplace_name(&"a".repeat(65)));
        assert!(is_valid_marketplace_name(&"a".repeat(64)));
    }

    // ---- passthrough / $schema ----
    #[test]
    fn test_unknown_top_level_field_passthrough() {
        let raw = json!({"futureUnknownField": {"x": 1}, "anotherOne": [1, 2, 3]});
        let (cleaned, errors) = validate_settings(&raw, SettingsScope::User, None);
        assert_eq!(Value::Object(cleaned), raw); // 未知键原样保留
        assert!(errors.is_empty());
    }

    #[test]
    fn test_schema_field_preserved_not_consumed() {
        let raw = json!({"$schema": "https://a2c-smcp.dev/schemas/computer-settings-0.2.1.json"});
        let (cleaned, errors) = validate_settings(&raw, SettingsScope::User, None);
        assert_eq!(Value::Object(cleaned), raw);
        assert!(errors.is_empty());
    }

    #[test]
    fn test_non_dict_root_falls_back_empty() {
        let raw = json!(["not", "an", "object"]);
        let (cleaned, errors) = validate_settings(&raw, SettingsScope::User, None);
        assert!(cleaned.is_empty());
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].field, "<root>");
    }

    // ---- policy-only 越权 / policy-only overreach ----
    #[test]
    fn test_policy_only_fields_filtered_outside_policy() {
        for scope in [
            SettingsScope::User,
            SettingsScope::Project,
            SettingsScope::Local,
        ] {
            let raw = json!({
                "allowedMcpServers": ["a"],
                "deniedMcpServers": ["b"],
                "trustedMarketplaces": ["mp"]
            });
            let (cleaned, errors) = validate_settings(&raw, scope, None);
            assert!(!cleaned.contains_key("allowedMcpServers"));
            assert!(!cleaned.contains_key("deniedMcpServers"));
            assert_eq!(cleaned["trustedMarketplaces"], json!(["mp"])); // 非 policy-only 字段保留
            let bad: std::collections::BTreeSet<&str> =
                errors.iter().map(|e| e.field.as_str()).collect();
            assert_eq!(
                bad,
                ["allowedMcpServers", "deniedMcpServers"]
                    .into_iter()
                    .collect()
            );
        }
    }

    #[test]
    fn test_policy_only_fields_kept_in_policy_scope() {
        let raw = json!({"allowedMcpServers": ["a"], "deniedMcpServers": ["b"]});
        let (cleaned, errors) = validate_settings(&raw, SettingsScope::Policy, None);
        assert_eq!(Value::Object(cleaned), raw);
        assert!(errors.is_empty());
    }

    // ---- #143：审批门 enable 方向判据的 scope 越权 / trusted-scope-only overreach ----

    /// project scope（**入 git**）供给 enable 方向判据 → 过滤 + 记错（自我批准闭环，与档④ 同构）。
    #[test]
    fn test_trusted_scope_only_fields_filtered_in_project() {
        let raw = json!({
            "enabledMcpjsonServers": ["evil"],
            "enableAllProjectMcpServers": true,
            "disabledMcpjsonServers": ["x"],   // DENY 方向 → 不受限，须保留
            "trustedMarketplaces": ["mp"]      // 无关字段 → 须保留
        });
        let (cleaned, errors) = validate_settings(&raw, SettingsScope::Project, None);
        assert!(!cleaned.contains_key("enabledMcpjsonServers"));
        assert!(!cleaned.contains_key("enableAllProjectMcpServers"));
        assert_eq!(cleaned["disabledMcpjsonServers"], json!(["x"]));
        assert_eq!(cleaned["trustedMarketplaces"], json!(["mp"]));
        let bad: std::collections::BTreeSet<&str> =
            errors.iter().map(|e| e.field.as_str()).collect();
        assert_eq!(
            bad,
            ["enableAllProjectMcpServers", "enabledMcpjsonServers"]
                .into_iter()
                .collect(),
            "只该拒 enable 方向两个字段"
        );
        // 文案须给出可操作去向（团队升级后能自助）。
        assert!(
            errors
                .iter()
                .all(|e| e.reason.contains("settings.local.json")),
            "错误文案须含迁移指引，实得 {errors:?}"
        );
    }

    /// **含 `Local`**——这条是 mcp.json 声明面预信任集（`is_trusted_origin`，不含 Local）的反面守护：三个
    /// 批准写助手只写 local scope，若 local 被判不受信，则「批准 → 写 local → 读回」链断裂、批准永不生效。
    #[test]
    fn test_trusted_scope_only_fields_kept_in_user_local_flag_policy() {
        for scope in [
            SettingsScope::User,
            SettingsScope::Local,
            SettingsScope::Flag,
            SettingsScope::Policy,
        ] {
            let raw = json!({"enabledMcpjsonServers": ["s"], "enableAllProjectMcpServers": true});
            let (cleaned, errors) = validate_settings(&raw, scope, None);
            assert_eq!(
                cleaned["enabledMcpjsonServers"],
                json!(["s"]),
                "{scope:?} 是受信供给方，MUST 保留"
            );
            assert_eq!(
                cleaned["enableAllProjectMcpServers"],
                json!(true),
                "{scope:?}"
            );
            assert!(errors.is_empty(), "{scope:?} 不应记错，实得 {errors:?}");
        }
    }

    // ---- #195 landingRoot（协议 blob-transfer.md §7：project MUST 拒绝）----

    /// project scope（入 git）供给 `landingRoot` → 过滤 + 记错（写目标不放仓库随分发的路径重定向）。
    #[test]
    fn test_landing_root_filtered_in_project_scope() {
        let raw = json!({"landingRoot": "/tmp/landing"});
        let (cleaned, errors) = validate_settings(&raw, SettingsScope::Project, None);
        assert!(
            !cleaned.contains_key("landingRoot"),
            "project scope 提供 landingRoot MUST 被拒绝"
        );
        assert_eq!(errors.len(), 1, "须记错，实得 {errors:?}");
        assert!(errors[0].reason.contains("trusted-scope-only"));
    }

    /// 受信 scope（此处以 user 为代表；Local/Flag/Policy/Embed 走既有门户，不必逐层重复）→ 保留。
    #[test]
    fn test_landing_root_kept_in_user_scope() {
        let raw = json!({"landingRoot": "/tmp/landing"});
        let (cleaned, errors) = validate_settings(&raw, SettingsScope::User, None);
        assert_eq!(cleaned["landingRoot"], json!("/tmp/landing"));
        assert!(errors.is_empty());
    }

    /// 形态校验：相对路径 / 非字符串 → 整字段判废 + 记错；绝对路径（POSIX 与 Windows 盘符）→ 放行。
    #[test]
    fn test_landing_root_validation() {
        let (cleaned, errors) = validate_settings(
            &json!({"landingRoot": "relative/path"}),
            SettingsScope::User,
            None,
        );
        assert!(!cleaned.contains_key("landingRoot"));
        assert!(!errors.is_empty());

        let (cleaned, errors) = validate_settings(
            &json!({"landingRoot": 123}),
            SettingsScope::User,
            None,
        );
        assert!(!cleaned.contains_key("landingRoot"));
        assert!(!errors.is_empty());

        for ok in ["/abs/landing", "/Users/me/.a2c", "C:\\data", "D:/data"] {
            let (cleaned, errors) =
                validate_settings(&json!({"landingRoot": ok}), SettingsScope::User, None);
            assert_eq!(cleaned["landingRoot"], json!(ok), "{ok:?} 应为合法绝对路径");
            assert!(errors.is_empty(), "{ok:?} 不应记错，实得 {errors:?}");
        }
    }

    /// typed round-trip：`landingRoot` 以 camelCase 键进出 [`ComputerSettings`]。
    #[test]
    fn test_landing_root_typed_roundtrip() {
        let parsed: ComputerSettings =
            serde_json::from_str(r#"{"landingRoot": "/tmp/landing"}"#).unwrap();
        assert_eq!(parsed.landing_root.as_deref(), Some("/tmp/landing"));
        let v = serde_json::to_value(&parsed).unwrap();
        assert_eq!(v["landingRoot"], json!("/tmp/landing"));
    }

    /// ★**防过度矫正**：`disabledMcpjsonServers` 是 DENY 方向，协议 §2.1 表第 3 行明定**任意 scope 可供给**
    /// （fail-safe）。后人若顺手把它收进 [`TRUSTED_SCOPE_ONLY_FIELDS`]，此测立刻红。
    #[test]
    fn test_disabled_mcpjson_allowed_from_any_scope() {
        assert!(
            !TRUSTED_SCOPE_ONLY_FIELDS.contains(&FIELD_DISABLED_MCPJSON_SERVERS),
            "DENY 方向 MUST NOT 进 enable-方向类目（§2.1 表第 3 行 fail-safe）"
        );
        for scope in [
            SettingsScope::User,
            SettingsScope::Project,
            SettingsScope::Local,
            SettingsScope::Flag,
            SettingsScope::Policy,
        ] {
            let raw = json!({"disabledMcpjsonServers": ["srv"]});
            let (cleaned, errors) = validate_settings(&raw, scope, None);
            assert_eq!(
                cleaned["disabledMcpjsonServers"],
                json!(["srv"]),
                "{scope:?}：DENY 方向任意 scope 可供给（更严格永远安全）"
            );
            assert!(errors.is_empty(), "{scope:?} 不应记错");
        }
    }

    // ---- enabledPlugins 逐条过滤 ----
    #[test]
    fn test_enabled_plugins_per_entry_filtering() {
        let raw = json!({
            "enabledPlugins": {
                "good@mp": true,
                "also-good@mp": false,
                "badshape": true,    // 缺 @ → 过滤
                "nonbool@mp": "yes", // 值非 bool → 过滤
            }
        });
        let (cleaned, errors) = validate_settings(&raw, SettingsScope::User, None);
        assert_eq!(
            cleaned["enabledPlugins"],
            json!({"good@mp": true, "also-good@mp": false})
        );
        let bad: std::collections::BTreeSet<&str> =
            errors.iter().map(|e| e.field.as_str()).collect();
        assert_eq!(
            bad,
            ["enabledPlugins.badshape", "enabledPlugins.nonbool@mp"]
                .into_iter()
                .collect()
        );
    }

    #[test]
    fn test_enabled_plugins_non_object_dropped() {
        let (cleaned, errors) = validate_settings(
            &json!({"enabledPlugins": ["nope"]}),
            SettingsScope::User,
            None,
        );
        assert!(!cleaned.contains_key("enabledPlugins"));
        assert_eq!(errors.len(), 1);
    }

    // ---- extraKnownMarketplaces 逐条过滤 + 规范化 ----
    #[test]
    fn test_extra_marketplaces_valid_entry_normalized() {
        let raw = json!({
            "extraKnownMarketplaces": {
                "my-team-skills": {
                    "source": {"type": "git", "url": "git@github.com:team/skills.git", "extra": "dropped"},
                    "autoUpdate": true,
                    "junk": 1
                }
            }
        });
        let (cleaned, errors) = validate_settings(&raw, SettingsScope::User, None);
        assert!(errors.is_empty());
        assert_eq!(
            cleaned["extraKnownMarketplaces"]["my-team-skills"],
            json!({"source": {"type": "git", "url": "git@github.com:team/skills.git"}, "autoUpdate": true})
        );
    }

    #[test]
    fn test_extra_marketplaces_bad_entry_filtered() {
        let cases: [(&str, Value, &str); 5] = [
            (
                "Bad_Name",
                json!({"source": {"type": "git", "url": "git@h:p"}}),
                "extraKnownMarketplaces.Bad_Name",
            ),
            (
                "ok-mp",
                json!({"source": {"type": "svn", "url": "x"}}),
                "extraKnownMarketplaces.ok-mp.source.type",
            ),
            (
                "ok-mp",
                json!({"source": {"type": "git", "url": "garbage"}}),
                "extraKnownMarketplaces.ok-mp.source.url",
            ),
            (
                "ok-mp",
                json!({"noSource": 1}),
                "extraKnownMarketplaces.ok-mp.source",
            ),
            (
                "ok-mp",
                json!({"source": {"type": "git", "url": "git@h:p"}, "autoUpdate": "yes"}),
                "extraKnownMarketplaces.ok-mp.autoUpdate",
            ),
        ];
        for (name, entry, bad_field) in cases {
            let raw = json!({"extraKnownMarketplaces": {name: entry}});
            let (cleaned, errors) = validate_settings(&raw, SettingsScope::User, None);
            // 唯一项被剔除 → 空对象
            assert_eq!(
                cleaned["extraKnownMarketplaces"],
                json!({}),
                "case {bad_field}"
            );
            assert!(
                errors.iter().any(|e| e.field == bad_field),
                "missing error {bad_field}"
            );
        }
    }

    // ---- 标量 / 数组类型校验 ----
    #[test]
    fn test_bool_field_type_error_dropped() {
        let (cleaned, errors) = validate_settings(
            &json!({"strictKnownMarketplaces": "true"}),
            SettingsScope::User,
            None,
        );
        assert!(!cleaned.contains_key("strictKnownMarketplaces"));
        assert_eq!(errors.len(), 1);
    }

    #[test]
    fn test_string_array_element_filtering() {
        let (cleaned, errors) = validate_settings(
            &json!({"trustedMarketplaces": ["a", 1, "b", null]}),
            SettingsScope::User,
            None,
        );
        assert_eq!(cleaned["trustedMarketplaces"], json!(["a", "b"]));
        assert_eq!(errors.len(), 2);
    }

    #[test]
    fn test_string_array_non_list_dropped() {
        let (cleaned, errors) = validate_settings(
            &json!({"trustedMarketplaces": "a,b"}),
            SettingsScope::User,
            None,
        );
        assert!(!cleaned.contains_key("trustedMarketplaces"));
        assert_eq!(errors.len(), 1);
    }

    // ---- permissions.additionalDirectories ----
    #[test]
    fn test_permissions_additional_directories_absolute_only() {
        let raw = json!({"permissions": {"additionalDirectories": ["/abs/ok", "relative/no", 42], "unknownSub": "kept"}});
        let (cleaned, errors) = validate_settings(&raw, SettingsScope::User, None);
        assert_eq!(
            cleaned["permissions"]["additionalDirectories"],
            json!(["/abs/ok"])
        );
        assert_eq!(cleaned["permissions"]["unknownSub"], json!("kept")); // 未知子键 passthrough
        assert_eq!(errors.len(), 2); // relative + 非 string
    }

    #[test]
    fn test_source_path_backfilled_into_errors() {
        let (_, errors) = validate_settings(
            &json!({"strictKnownMarketplaces": 1}),
            SettingsScope::User,
            Some("/x/settings.json"),
        );
        assert_eq!(errors[0].source_path.as_deref(), Some("/x/settings.json"));
    }

    // ---- SettingsScope 序列化与 Python StrEnum 字面一致 ----
    #[test]
    fn test_settings_scope_serializes_lowercase() {
        let pairs = [
            (SettingsScope::User, "user"),
            (SettingsScope::Project, "project"),
            (SettingsScope::Local, "local"),
            (SettingsScope::Flag, "flag"),
            (SettingsScope::Policy, "policy"),
        ];
        for (scope, lit) in pairs {
            assert_eq!(serde_json::to_value(scope).unwrap(), json!(lit));
            assert_eq!(scope.as_str(), lit);
            assert_eq!(
                serde_json::from_value::<SettingsScope>(json!(lit)).unwrap(),
                scope
            );
        }
    }

    // ---- 字段集合内容精确 ----
    #[test]
    fn test_field_sets_exact() {
        assert_eq!(
            POLICY_ONLY_FIELDS,
            &["allowedMcpServers", "deniedMcpServers"]
        );
        // #143：enable 方向判据（拒 project）。**不含** disabledMcpjsonServers —— DENY 方向 fail-safe。
        // #195：landingRoot（写目标）同属受信 scope only（协议 blob-transfer.md §7）。
        assert_eq!(
            TRUSTED_SCOPE_ONLY_FIELDS,
            &[
                "enabledMcpjsonServers",
                "enableAllProjectMcpServers",
                "landingRoot"
            ]
        );
        assert_eq!(
            BOOL_FIELDS,
            &["strictKnownMarketplaces", "enableAllProjectMcpServers"]
        );
        assert_eq!(STRING_ARRAY_FIELDS.len(), 6);
    }

    // ---- ComputerSettings 全字段可选 serde round-trip ----
    #[test]
    fn test_computer_settings_empty_serializes_to_empty_object() {
        let s = ComputerSettings::default();
        assert_eq!(serde_json::to_value(&s).unwrap(), json!({})); // 缺省不产出空键
    }

    #[test]
    fn test_computer_settings_round_trip() {
        let raw = json!({
            "$schema": "https://x/schema.json",
            "enabledPlugins": {"frontend-design@my-team-skills": true},
            "extraKnownMarketplaces": {
                "my-team-skills": {"source": {"type": "git", "url": "git@github.com:team/skills.git"}, "autoUpdate": true}
            },
            "strictKnownMarketplaces": true,
            "trustedMarketplaces": ["mp"],
            "permissions": {"additionalDirectories": ["/abs"], "futureSub": 1},
            "futureTopLevel": {"keep": "me"}
        });
        let parsed: ComputerSettings = serde_json::from_value(raw.clone()).unwrap();
        // 已知字段 typed 落位
        assert_eq!(parsed.schema.as_deref(), Some("https://x/schema.json"));
        assert_eq!(parsed.strict_known_marketplaces, Some(true));
        // 未知顶层字段进 extra
        assert!(parsed.extra.contains_key("futureTopLevel"));
        // round-trip 逐字段保留
        assert_eq!(serde_json::to_value(&parsed).unwrap(), raw);
    }
}
