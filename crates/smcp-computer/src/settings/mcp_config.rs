/*!
* 文件名: mcp_config.rs
* 作者: JQQ
* 创建日期: 2026/06/05
* 最后修改日期: 2026/06/05
* 版权: 2023 JQQ. All rights reserved.
* 依赖: serde_json, indexmap, crate::mcp_clients::model, crate::settings::{schema,scope,store,policy}
* 描述: MCP server 定义层（mcp.json）多 scope 加载 + 批准门控（SET-06 #71）
*       MCP server definition layer (mcp.json) multi-scope load + approval gate.
*/

//! `.tfrobot/mcp.json` MCP server 定义层多 scope 加载 + 批准门控 / MCP definition load + approval gate。
//!
//! 协议依据 / Protocol: a2c-smcp-protocol §9.1（mcp.json 定义层、A2C 原生 schema）/ §9.2（批准门控）/
//! §5.6（字段级校验不阻断）/ §5.5（MCP 定义合并顺序）。对标 Python `computer/settings/mcp_config.py`。
//!
//! 本模块是 **MCP 定义/门控的纯逻辑层**（无 git / 无 MCP manager / 无网络）。职责三件：
//! 1. **多 scope 加载合并** `mcp.json`——顺序 `policy > local > project > user > flag`，**无能力层并集**
//!    （敏感面隔离，区别于 settings.json）；server 按 name **整体替换**（配置是原子单元、非深合并），记录最高
//!    定义 scope 为 `origin`。
//! 2. **批准门控判定**（[`mcp_server_status`]）——据 resolved settings（MCP 门控字段）+ plugin-bundled 账本
//!    （`installed_plugins.json`）算 `enabled/disabled/pending`。
//! 3. **批准写助手**——批准/拒绝写 **local scope**（`settings.local.json`，§9.2，个人决定不污染共享层），
//!    复用 store 持锁原子 RMW（无写保护头，同 installer `enabledPlugins`）。
//!
//! **显式划界 / Deferred boundaries**：
//! - **取值渲染**（`envFile` 加载 / `${env:}` / inputs 解析链 / keyring / 明文 state）归 inputs 层（#73）：
//!   本模块产出**带占位符**的定义，[`ResolvedMcpServer::ext`]（`envFile` 等 VS Code 扩展）+ 未渲染占位符是
//!   handoff。**绝不在此渲染**（§9.1 安全铁律：值不离 Computer）。
//! - **批准框 TTY 交互** / `--approve-all-mcp` / 非交互 pending→skip+WARN 接线归 CLI（#48/#69）：本模块只提供
//!   [`McpApprovalStatus`] 判定 + 三个写助手原语。
//! - `managed-mcp.json` 仅读 per-platform managed dir（remote/MDM stub），对齐 [`crate::settings::policy`]。
//!
//! **容错姿态**：`mcp.json` 是**人/团队编辑文件**，故**字段级容错**（单 server / input 畸形 → drop + 记
//! [`SettingsValidationError`]，**不 abort**）——刻意区别于 [`crate::skills::manifest`] 对 plugin-bundled
//! server 的**硬抛**（那是 install 原子前置）。照 §5.6。
//!
//! **与 Python 的差异 / Divergence**：Python `MCPServerConfig`（pydantic `extra="forbid"`）令非 `envFile` 的
//! 未知 server 键 drop+error；Rust 共享 [`MCPServerConfig`] 模型对未知键**宽容**（serde 默认忽略），故此类边角键
//! 被容受而非 drop。强制拒绝未知 server 键属模型层硬化（影响全 crate 反序列化），**不**在本模块引入。
//! 行为由 `unknown_server_key_is_leniently_accepted` 测试钉死——若未来给模型加 `deny_unknown_fields` 会令其失败、
//! 拦截无声语义漂移。**下游接缝须知（#73/#69）**：Python docstring 依赖的「畸形 server 被 drop → 进启动 WARN」语义
//! 对此类未知键在 Rust **不触发**（该 server 被容受、不入 `errors`），故 CLI 启动 WARN 接线**不应**假定与 Python
//! 逐条对等；真正畸形（缺必填 / `type` 非法 / name-key 冲突）仍 drop+error，与 Python 一致。

use std::collections::{BTreeMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde::Serialize;
use serde_json::{Map, Value};

use crate::mcp_clients::model::{MCPServerConfig, MCPServerInput};
use crate::settings::policy::{LINUX_MANAGED_DIR, MACOS_MANAGED_DIR, WINDOWS_MANAGED_DIR};
use crate::settings::schema::{
    SettingsScope, SettingsValidationError, FIELD_ALLOWED_MCP_SERVERS, FIELD_DENIED_MCP_SERVERS,
    FIELD_DISABLED_MCPJSON_SERVERS, FIELD_ENABLED_MCPJSON_SERVERS, FIELD_ENABLE_ALL_PROJECT_MCP,
};
use crate::settings::scope::{
    apply_write, load_settings_file, resolve_cwd, resolve_user_config_dir,
    workdir_local_settings_path, workdir_settings_dir, EnvMap, WriteValue,
};
use crate::settings::store::{self, load_installed_plugins, SettingsStoreError};

// ---------------------------------------------------------------------------
// 常量 / Constants
// ---------------------------------------------------------------------------
/// user / project scope 定义文件名（§9.1）/ definition filename for user/project scope。
pub const MCP_CONFIG_FILENAME: &str = "mcp.json";
/// local scope 文件名（`<cwd>/.tfrobot/`，不入 git）/ local-scope filename (not git-tracked)。
pub const MCP_LOCAL_CONFIG_FILENAME: &str = "mcp.local.json";
/// policy scope 文件名（企业下发）/ policy-scope filename (enterprise-managed)。
pub const MANAGED_MCP_FILENAME: &str = "managed-mcp.json";

/// server 定义里的 VS Code 风格扩展键：非 A2C `MCPServerConfig` 字段，校验前剥离、原样交渲染层 / ext keys。
const VSCODE_EXT_KEYS: &[&str] = &["envFile"];

/// 预信任 origin scope（user/flag/policy 自加 server，不弹批准框，§9.2）；project/local 受门控 / trusted origins。
const TRUSTED_ORIGINS: &[SettingsScope] = &[
    SettingsScope::User,
    SettingsScope::Flag,
    SettingsScope::Policy,
];

// ---------------------------------------------------------------------------
// 错误 / Errors
// ---------------------------------------------------------------------------
/// MCP 批准写助手的失败（持锁写 / I/O）/ Failure of an MCP approval-write helper (locked write / I/O)。
///
/// #98：`Contract`（批准写缺 active workdir）已随 workdir 概念瘦身移除——批准写锚定进程 cwd、无 fail-fast。
#[derive(Debug, thiserror::Error)]
pub enum McpConfigError {
    /// local settings 持锁写失败 / locked write failed。
    #[error(transparent)]
    Store(#[from] SettingsStoreError),
    /// local settings 写 I/O 失败 / write I/O failed。
    #[error("settings.local.json write io error: {0}")]
    Io(#[from] io::Error),
}

// ---------------------------------------------------------------------------
// 数据结构 / Data structures
// ---------------------------------------------------------------------------
/// 单个 MCP server 的批准门控状态（§9.2）/ Approval-gate status of one MCP server。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum McpApprovalStatus {
    /// 已批准 / 预信任 / bundled 免批准 → 可连接 / approved → connectable。
    Enabled,
    /// 显式拒绝 / 企业拒绝名单 / 不在白名单 → 不连接 / denied → not connected。
    Disabled,
    /// 工作区共享且未决 → 启动时弹批准框 / workspace-shared & undecided → prompt at startup。
    Pending,
}

impl McpApprovalStatus {
    /// 状态字符串（对齐 Python `StrEnum` 值）/ the wire string。
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            McpApprovalStatus::Enabled => "enabled",
            McpApprovalStatus::Disabled => "disabled",
            McpApprovalStatus::Pending => "pending",
        }
    }
}

/// 合并解析后的单个 MCP server 定义 / A merged-and-resolved single MCP server definition。
///
/// `config` 是校验后的 A2C [`MCPServerConfig`]（**含占位符、未渲染**）；`ext` 是剥离出的 VS Code 扩展字段
/// （如 `envFile`，交渲染层消费）；`origin` 为最高定义 scope；`trusted_origin` 决定是否免批准门控。
#[derive(Debug, Clone)]
pub struct ResolvedMcpServer {
    /// server 身份（= map key）/ server identity (= map key)。
    pub name: String,
    /// 校验后的 A2C server 配置（未渲染占位符）/ validated config (placeholders unrendered)。
    pub config: MCPServerConfig,
    /// 剥离出的 VS Code 扩展字段（`envFile` 等），交渲染层只读消费 / stripped VS Code ext fields。
    pub ext: Map<String, Value>,
    /// 最高定义 scope / highest-defining scope。
    pub origin: SettingsScope,
    /// origin ∈ {user, flag, policy} → 免门控 / pre-trusted (no approval gate)。
    pub trusted_origin: bool,
}

/// 多 scope 合并后的 MCP 定义视图 / The multi-scope merged MCP definition view。
#[derive(Debug, Clone, Default)]
pub struct ResolvedMcpConfig {
    /// 按 name 索引的 server（保物化顺序）/ servers by name (insertion-ordered)。
    pub servers: IndexMap<String, ResolvedMcpServer>,
    /// 去重后的 input 定义（取值/渲染归 inputs 层）/ deduped input definitions。
    pub inputs: Vec<MCPServerInput>,
    /// 字段级校验错误汇总（不阻断、供诊断）/ field-level validation errors (non-blocking)。
    pub errors: Vec<SettingsValidationError>,
}

/// 单个 `mcp.json` 文件的原始（未校验）内容 / Raw (unvalidated) contents of one mcp.json file。
#[derive(Debug, Clone, Default)]
pub struct RawMcpConfigFile {
    /// `name → server 原始定义`（含 VS Code 扩展键，未剥离）/ raw server defs by name。
    pub servers: Map<String, Value>,
    /// input 原始定义列表 / raw input defs。
    pub inputs: Vec<Value>,
}

/// [`resolve_mcp_config`] 入参 / arguments to [`resolve_mcp_config`]。
#[derive(Default)]
pub struct ResolveMcpConfigArgs<'a> {
    /// project/local 锚定的工作目录；`None` → 进程 cwd / project/local anchor, `None` → process cwd。
    /// #98：`Computer` 不再持有 workspace，project/local 锚定进程 cwd。测试注入接缝（镜像 `env`）；生产传 `None`。
    pub cwd: Option<&'a Path>,
    /// 环境映射（解析 user config dir），`None` → 进程环境 / env map。
    pub env: Option<&'a EnvMap>,
    /// `--config @file` 老接口文件（最低优先级，§5.5）/ legacy flag config path。
    pub flag_config_path: Option<&'a Path>,
    /// policy scope `managed-mcp.json` 覆盖路径（缺省按平台推导）/ managed path override。
    pub managed_mcp_path: Option<&'a Path>,
    /// 平台标识（缺省 `std::env::consts::OS`；接受 `darwin`/`win32`/`linux` 或 `macos`/`windows`）/ platform。
    pub platform: Option<&'a str>,
}

// ---------------------------------------------------------------------------
// 路径解析（复用 scope 的路径根）/ Path resolution (reuses scope roots)
// ---------------------------------------------------------------------------
/// user scope `$XDG_CONFIG_HOME/a2c/mcp.json` 路径 / Path to the user-scope mcp.json。
#[must_use]
pub fn user_mcp_config_path(env: Option<&EnvMap>) -> PathBuf {
    resolve_user_config_dir(env).join(MCP_CONFIG_FILENAME)
}

/// project scope `<workdir>/.tfrobot/mcp.json` 路径（入 git、团队共享）/ project-scope mcp.json path。
#[must_use]
pub fn workdir_mcp_config_path(workdir: &Path) -> PathBuf {
    workdir_settings_dir(workdir).join(MCP_CONFIG_FILENAME)
}

/// local scope `<workdir>/.tfrobot/mcp.local.json` 路径（不入 git）/ local-scope mcp.local.json path。
#[must_use]
pub fn workdir_mcp_local_config_path(workdir: &Path) -> PathBuf {
    workdir_settings_dir(workdir).join(MCP_LOCAL_CONFIG_FILENAME)
}

/// 按平台选 managed 目录（接受 Python `sys.platform` 与 Rust `OS` 两族 token）/ Per-platform managed dir。
fn default_managed_dir(platform: &str) -> PathBuf {
    match platform {
        "darwin" | "macos" => PathBuf::from(MACOS_MANAGED_DIR),
        "win32" | "windows" => PathBuf::from(WINDOWS_MANAGED_DIR),
        _ => PathBuf::from(LINUX_MANAGED_DIR),
    }
}

/// policy scope `<managed-dir>/managed-mcp.json` 路径 / Path to the policy-scope managed-mcp.json。
#[must_use]
pub fn managed_mcp_config_path(platform: Option<&str>) -> PathBuf {
    let platform = platform
        .map(String::from)
        .unwrap_or_else(|| std::env::consts::OS.to_string());
    default_managed_dir(&platform).join(MANAGED_MCP_FILENAME)
}

// ---------------------------------------------------------------------------
// 单文件加载（容错）/ Single-file load (tolerant)
// ---------------------------------------------------------------------------
/// 构造一条字段级校验错误 / Build one field-level validation error。
fn err(
    scope: SettingsScope,
    field: &str,
    reason: &str,
    source: Option<&str>,
) -> SettingsValidationError {
    SettingsValidationError {
        scope,
        field: field.to_string(),
        reason: reason.to_string(),
        source_path: source.map(String::from),
    }
}

/// 读取并容错规整单个 `mcp.json` 为 `{servers, inputs}` / Load + tolerantly coerce one mcp.json。
///
/// 缺失 → 空；JSON 损坏 / 根非对象 → 空 + 一条错误（**不**备份、**不**清盘，§5.6 人编文件姿态）；
/// `servers` 非对象 / `inputs` 非数组（且非 null）→ 该字段判空 + 记错（其余仍用）。
#[must_use]
pub fn load_mcp_config_file(
    path: &Path,
    scope: SettingsScope,
) -> (RawMcpConfigFile, Vec<SettingsValidationError>) {
    let src = path.to_string_lossy().into_owned();
    if !path.exists() {
        return (RawMcpConfigFile::default(), Vec::new());
    }
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            return (
                RawMcpConfigFile::default(),
                vec![err(
                    scope,
                    "<file>",
                    &format!("unreadable or corrupt JSON: {e}"),
                    Some(&src),
                )],
            )
        }
    };
    let raw: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            return (
                RawMcpConfigFile::default(),
                vec![err(
                    scope,
                    "<file>",
                    &format!("unreadable or corrupt JSON: {e}"),
                    Some(&src),
                )],
            )
        }
    };
    let Value::Object(obj) = raw else {
        return (
            RawMcpConfigFile::default(),
            vec![err(
                scope,
                "<root>",
                "mcp config root must be an object",
                Some(&src),
            )],
        );
    };

    let mut errors: Vec<SettingsValidationError> = Vec::new();
    let servers = match obj.get("servers") {
        Some(Value::Object(m)) => m.clone(),
        None | Some(Value::Null) => Map::new(),
        Some(_) => {
            errors.push(err(
                scope,
                "servers",
                "'servers' must be an object",
                Some(&src),
            ));
            Map::new()
        }
    };
    let inputs = match obj.get("inputs") {
        Some(Value::Array(a)) => a.clone(),
        None | Some(Value::Null) => Vec::new(),
        Some(_) => {
            errors.push(err(
                scope,
                "inputs",
                "'inputs' must be an array",
                Some(&src),
            ));
            Vec::new()
        }
    };
    (RawMcpConfigFile { servers, inputs }, errors)
}

// ---------------------------------------------------------------------------
// 校验单元（字段级容错）/ Validation units (field-level tolerant)
// ---------------------------------------------------------------------------
/// 校验单个 server 定义 → [`ResolvedMcpServer`]（畸形 → `None` + 错误，**不抛**）/ Validate one server。
///
/// map **key 即 server 身份**：注入 `name=<key>`；若 `sdef` 内显式 `name` 与 key 冲突 → 判废。剥离
/// [`VSCODE_EXT_KEYS`] 入 `ext`，其余校验为 [`MCPServerConfig`]。
fn validate_server(
    name: &str,
    sdef: &Value,
    scope: SettingsScope,
    source: Option<&str>,
) -> (Option<ResolvedMcpServer>, Vec<SettingsValidationError>) {
    let fld = format!("servers.{name}");
    let Value::Object(obj) = sdef else {
        return (
            None,
            vec![err(
                scope,
                &fld,
                "server definition must be an object",
                source,
            )],
        );
    };
    let mut ext: Map<String, Value> = Map::new();
    let mut body: Map<String, Value> = Map::new();
    for (k, v) in obj {
        if VSCODE_EXT_KEYS.contains(&k.as_str()) {
            ext.insert(k.clone(), v.clone());
        } else {
            body.insert(k.clone(), v.clone());
        }
    }
    // key 即身份：显式 name 与 key 冲突（含非字符串）→ 判废。
    if let Some(n) = body.get("name") {
        if n.as_str() != Some(name) {
            let reason = format!(
                "server 'name' field {n} != map key {name:?} (the map key is the canonical identity)"
            );
            return (None, vec![err(scope, &fld, &reason, source)]);
        }
    }
    body.insert("name".to_string(), Value::String(name.to_string()));
    let cfg: MCPServerConfig = match serde_json::from_value(Value::Object(body)) {
        Ok(c) => c,
        Err(e) => {
            return (
                None,
                vec![err(
                    scope,
                    &fld,
                    &format!("invalid MCP server config: {e}"),
                    source,
                )],
            )
        }
    };
    let server = ResolvedMcpServer {
        name: name.to_string(),
        config: cfg,
        ext,
        origin: scope,
        trusted_origin: TRUSTED_ORIGINS.contains(&scope),
    };
    (Some(server), Vec::new())
}

/// 校验单个 input 定义 → [`MCPServerInput`]（畸形 → `None` + 错误，**不抛**）/ Validate one input def。
fn validate_input(
    idef: &Value,
    scope: SettingsScope,
    source: Option<&str>,
) -> (Option<MCPServerInput>, Vec<SettingsValidationError>) {
    let raw_id = idef.get("id").and_then(Value::as_str);
    let fld = match raw_id {
        Some(id) => format!("inputs.{id}"),
        None => "inputs.<unknown>".to_string(),
    };
    match serde_json::from_value::<MCPServerInput>(idef.clone()) {
        Ok(inp) => (Some(inp), Vec::new()),
        Err(e) => (
            None,
            vec![err(
                scope,
                &fld,
                &format!("invalid input definition: {e}"),
                source,
            )],
        ),
    }
}

// ---------------------------------------------------------------------------
// 多 scope 解析 / Multi-scope resolution
// ---------------------------------------------------------------------------
/// 多 scope 加载合并 `.tfrobot/mcp.json` + 字段级校验 / Multi-scope load + merge + validate mcp.json。
///
/// 合并顺序 low → high = `[flag, user, project, local, policy]`（优先级
/// `policy > local > project > user > flag`，§9.1/§5.5）；**无能力层并集**。#98：project/local **无条件**锚定
/// 进程 cwd（`cwd` 注入接缝，`None` → `std::env::current_dir()`；cwd 不可读则该两层缺省）。server 按 name
/// **整体替换**（`origin` = 最高定义 scope）；inputs 按 `id` 去重高 scope 胜（缺 `id` 的条目各自保留以逐条报错）。
/// 单 server / input 畸形 → drop + 错误，**不 abort**（§5.6）。
#[must_use]
pub fn resolve_mcp_config(args: ResolveMcpConfigArgs<'_>) -> ResolvedMcpConfig {
    let managed_path = args
        .managed_mcp_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| managed_mcp_config_path(args.platform));

    // (scope, path) 层，低优先级在前 / layers, lowest priority first。
    let mut layers: Vec<(SettingsScope, PathBuf)> = Vec::new();
    if let Some(fc) = args.flag_config_path {
        layers.push((SettingsScope::Flag, fc.to_path_buf()));
    }
    layers.push((SettingsScope::User, user_mcp_config_path(args.env)));
    // project/local：无条件锚定进程 cwd（cwd 不可读 → 跳过该两层）。
    if let Some(base) = resolve_cwd(args.cwd) {
        layers.push((SettingsScope::Project, workdir_mcp_config_path(&base)));
        layers.push((SettingsScope::Local, workdir_mcp_local_config_path(&base)));
    }
    layers.push((SettingsScope::Policy, managed_path));

    let mut errors: Vec<SettingsValidationError> = Vec::new();
    // 累积原始定义（低→高，后者覆盖前者）；IndexMap 保插入序、同 key 覆盖值不挪位（对齐 Python dict）。
    let mut raw_servers: IndexMap<String, (Value, SettingsScope, String)> = IndexMap::new();
    let mut raw_inputs: IndexMap<String, (Value, SettingsScope, String)> = IndexMap::new();
    let mut noid: usize = 0;

    for (scope, path) in &layers {
        let (file, errs) = load_mcp_config_file(path, *scope);
        errors.extend(errs);
        let src = path.to_string_lossy().into_owned();
        for (srv_name, sdef) in file.servers {
            raw_servers.insert(srv_name, (sdef, *scope, src.clone()));
        }
        for idef in file.inputs {
            let iid = idef.get("id").and_then(Value::as_str).map(String::from);
            let key = match &iid {
                Some(s) => s.clone(),
                None => {
                    let k = format!("<noid-{noid}>");
                    noid += 1;
                    k
                }
            };
            raw_inputs.insert(key, (idef, *scope, src.clone()));
        }
    }

    let mut servers: IndexMap<String, ResolvedMcpServer> = IndexMap::new();
    for (srv_name, (sdef, scope, src)) in raw_servers {
        let (resolved, errs) = validate_server(&srv_name, &sdef, scope, Some(&src));
        errors.extend(errs);
        if let Some(s) = resolved {
            servers.insert(srv_name, s);
        }
    }

    let mut inputs: Vec<MCPServerInput> = Vec::new();
    for (_key, (idef, scope, src)) in raw_inputs {
        let (resolved, errs) = validate_input(&idef, scope, Some(&src));
        errors.extend(errs);
        if let Some(i) = resolved {
            inputs.push(i);
        }
    }

    ResolvedMcpConfig {
        servers,
        inputs,
        errors,
    }
}

// ---------------------------------------------------------------------------
// 批准门控判定 / Approval-gate decision（§9.2）
// ---------------------------------------------------------------------------
/// 从 resolved settings 取字符串数组字段（非 list → `[]`）/ Read a string-array field (non-list → [])。
fn str_list<'a>(settings: &'a Map<String, Value>, key: &str) -> Vec<&'a str> {
    settings
        .get(key)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

/// 判定单个 MCP server 的批准状态（§9.2，**顺序即优先级**）/ Decide one server's approval status。
///
/// 优先级（先到先决）：① `deniedMcpServers` → Disabled；② `allowedMcpServers` 非空且不在其中 → Disabled；
/// ③ `disabledMcpjsonServers` → Disabled（disabled 优先 over enabled）；④ plugin-bundled（`bundled`）→ Enabled；
/// ⑤ `trusted_origin`（user/flag/policy）→ Enabled；⑥ `enabledMcpjsonServers` → Enabled；
/// ⑦ `enableAllProjectMcpServers == true` → Enabled；⑧ 否则 → Pending。
#[must_use]
pub fn mcp_server_status(
    name: &str,
    settings: &Map<String, Value>,
    bundled: &HashSet<String>,
    trusted_origin: bool,
) -> McpApprovalStatus {
    if str_list(settings, FIELD_DENIED_MCP_SERVERS).contains(&name) {
        return McpApprovalStatus::Disabled;
    }
    let allowed = str_list(settings, FIELD_ALLOWED_MCP_SERVERS);
    if !allowed.is_empty() && !allowed.contains(&name) {
        return McpApprovalStatus::Disabled;
    }
    if str_list(settings, FIELD_DISABLED_MCPJSON_SERVERS).contains(&name) {
        return McpApprovalStatus::Disabled;
    }
    if bundled.contains(name) {
        return McpApprovalStatus::Enabled;
    }
    if trusted_origin {
        return McpApprovalStatus::Enabled;
    }
    if str_list(settings, FIELD_ENABLED_MCPJSON_SERVERS).contains(&name) {
        return McpApprovalStatus::Enabled;
    }
    if settings.get(FIELD_ENABLE_ALL_PROJECT_MCP) == Some(&Value::Bool(true)) {
        return McpApprovalStatus::Enabled;
    }
    McpApprovalStatus::Pending
}

/// 对全部已解析 server 套 [`mcp_server_status`] / Apply the gate to all resolved servers。
#[must_use]
pub fn gate_mcp_servers(
    resolved: &ResolvedMcpConfig,
    settings: &Map<String, Value>,
    bundled: &HashSet<String>,
) -> IndexMap<String, McpApprovalStatus> {
    resolved
        .servers
        .iter()
        .map(|(name, srv)| {
            (
                name.clone(),
                mcp_server_status(name, settings, bundled, srv.trusted_origin),
            )
        })
        .collect()
}

/// 已安装 plugin 携带的 bundled MCP server name 并集（账本接缝）/ Union of installed plugins' bundled servers。
///
/// 读 `installed_plugins.json` 全记录的 `bundledMcpServers`——这些 server 因用户**显式 install plugin** 而
/// trusted、门控免批准（§9.2 第 4 档）。
#[must_use]
pub fn bundled_mcp_server_names(home: Option<&Path>, env: Option<&EnvMap>) -> HashSet<String> {
    let mut out: HashSet<String> = HashSet::new();
    for records in load_installed_plugins(home, env).account.plugins.values() {
        for record in records {
            out.extend(record.bundled_mcp_servers.iter().cloned());
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 批准写助手（写 local scope = settings.local.json）/ Approval write helpers（§9.2）
// ---------------------------------------------------------------------------
/// 批准写落 local scope = `<cwd>/.tfrobot/settings.local.json`（cwd 注入接缝，`None` → 进程 cwd）。
///
/// #98：不再需要 active workdir——批准写锚定进程 cwd（无 fail-fast）。进程 cwd 不可读（罕见）→ `Io` 错误。
fn local_settings_write_path(cwd: Option<&Path>) -> Result<PathBuf, McpConfigError> {
    let base = resolve_cwd(cwd).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "process cwd unavailable for local write",
        )
    })?;
    Ok(workdir_local_settings_path(&base))
}

/// 把 `name` 追加进 local `settings.local.json` 的某 MCP 数组字段（持锁原子 RMW + dedup）/ Append to a local array。
///
/// 复用 store 旁车锁 + 原子写 + scope 的 [`load_settings_file`] / [`apply_write`]（数组整体替换，§5.4）；
/// settings.local.json 人编意图层 → 无写保护头。锁内读-改-写杜绝并发丢更新。
fn append_local_mcp_array(
    cwd: Option<&Path>,
    field_name: &str,
    name: &str,
) -> Result<(), McpConfigError> {
    let path = local_settings_write_path(cwd)?;
    store::with_settings_lock(&path, || -> io::Result<()> {
        let (existing, _errors) = load_settings_file(&path, SettingsScope::Local);
        let mut current: Vec<String> = existing
            .get(field_name)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        if !current.iter().any(|v| v == name) {
            current.push(name.to_string());
        }
        let mut updates: BTreeMap<String, WriteValue> = BTreeMap::new();
        updates.insert(
            field_name.to_string(),
            WriteValue::Set(Value::Array(
                current.into_iter().map(Value::String).collect(),
            )),
        );
        let updated = apply_write(&existing, &updates);
        store::atomic_write_settings_json(&path, &Value::Object(updated))
    })??;
    Ok(())
}

/// 批准框 `[y]es`：追加 `enabledMcpjsonServers` 到 local scope（§9.2）/ Approve → append to enabled list (local)。
///
/// #98：写锚定进程 cwd（`cwd` 注入接缝，`None` → 进程 cwd）。
///
/// # Errors
/// 进程 cwd 不可读 / 写失败 → [`McpConfigError`]。
pub fn approve_mcp_server(name: &str, cwd: Option<&Path>) -> Result<(), McpConfigError> {
    append_local_mcp_array(cwd, FIELD_ENABLED_MCPJSON_SERVERS, name)
}

/// 批准框 `[n]o`：追加 `disabledMcpjsonServers` 到 local scope（§9.2）/ Deny → append to disabled list (local)。
///
/// #98：写锚定进程 cwd（`cwd` 注入接缝，`None` → 进程 cwd）。
///
/// # Errors
/// 进程 cwd 不可读 / 写失败 → [`McpConfigError`]。
pub fn deny_mcp_server(name: &str, cwd: Option<&Path>) -> Result<(), McpConfigError> {
    append_local_mcp_array(cwd, FIELD_DISABLED_MCPJSON_SERVERS, name)
}

/// 批准框 `[a]ll`：`enableAllProjectMcpServers=true` 写 local scope（§9.2）/ Approve-all → set the bool (local)。
///
/// #98：写锚定进程 cwd（`cwd` 注入接缝，`None` → 进程 cwd）。
///
/// # Errors
/// 进程 cwd 不可读 / 写失败 → [`McpConfigError`]。
pub fn approve_all_project_mcp(cwd: Option<&Path>) -> Result<(), McpConfigError> {
    let path = local_settings_write_path(cwd)?;
    store::with_settings_lock(&path, || -> io::Result<()> {
        let (existing, _errors) = load_settings_file(&path, SettingsScope::Local);
        let mut updates: BTreeMap<String, WriteValue> = BTreeMap::new();
        updates.insert(
            FIELD_ENABLE_ALL_PROJECT_MCP.to_string(),
            WriteValue::Set(Value::Bool(true)),
        );
        let updated = apply_write(&existing, &updates);
        store::atomic_write_settings_json(&path, &Value::Object(updated))
    })??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use tempfile::TempDir;

    fn write(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn settings_with(arrays: Value) -> Map<String, Value> {
        arrays.as_object().cloned().unwrap()
    }

    // ---- load_mcp_config_file ----------------------------------------------
    #[test]
    fn load_missing_corrupt_and_wrong_typed() {
        let tmp = TempDir::new().unwrap();
        // 缺失 → 空、无错。
        let (f, e) = load_mcp_config_file(&tmp.path().join("absent.json"), SettingsScope::User);
        assert!(f.servers.is_empty() && f.inputs.is_empty() && e.is_empty());
        // 损坏 → 空 + 一错。
        let p = tmp.path().join("bad.json");
        write(&p, "{not json");
        let (_f, e) = load_mcp_config_file(&p, SettingsScope::User);
        assert_eq!(e.len(), 1);
        // 根非对象 → 错。
        let p2 = tmp.path().join("arr.json");
        write(&p2, "[1,2]");
        let (_f, e) = load_mcp_config_file(&p2, SettingsScope::User);
        assert_eq!(e[0].field, "<root>");
        // servers 非对象 / inputs 非数组 → 该字段判空 + 记错。
        let p3 = tmp.path().join("wt.json");
        write(&p3, r#"{"servers": [1], "inputs": {"x":1}}"#);
        let (f, e) = load_mcp_config_file(&p3, SettingsScope::User);
        assert!(f.servers.is_empty() && f.inputs.is_empty());
        assert_eq!(e.len(), 2);
        // servers/inputs = null → 空、无错。
        let p4 = tmp.path().join("nul.json");
        write(&p4, r#"{"servers": null, "inputs": null}"#);
        let (_f, e) = load_mcp_config_file(&p4, SettingsScope::User);
        assert!(e.is_empty());
    }

    // ---- validate_server ----------------------------------------------------
    #[test]
    fn validate_server_strips_ext_and_checks_identity() {
        // envFile 剥离入 ext；name 注入。
        let sdef = json!({
            "type": "stdio",
            "server_parameters": {"command": "node"},
            "envFile": ".env"
        });
        let (srv, errs) = validate_server("figma", &sdef, SettingsScope::Project, None);
        let srv = srv.unwrap();
        assert!(errs.is_empty());
        assert_eq!(srv.name, "figma");
        assert_eq!(srv.config.name(), "figma");
        assert_eq!(srv.ext.get("envFile"), Some(&json!(".env")));
        assert!(!srv.trusted_origin); // project 受门控
                                      // user scope → trusted。
        let (srv2, _) = validate_server("figma", &sdef, SettingsScope::User, None);
        assert!(srv2.unwrap().trusted_origin);
        // 显式 name 与 key 冲突 → 判废。
        let bad = json!({"type": "stdio", "name": "other", "server_parameters": {"command": "x"}});
        let (none, errs) = validate_server("figma", &bad, SettingsScope::User, None);
        assert!(none.is_none() && errs.len() == 1);
        // 非对象 → 判废。
        let (none, errs) = validate_server("x", &json!("scalar"), SettingsScope::User, None);
        assert!(none.is_none() && errs.len() == 1);
        // 缺必填（command）→ 判废。
        let invalid = json!({"type": "stdio", "server_parameters": {}});
        let (none, errs) = validate_server("x", &invalid, SettingsScope::User, None);
        assert!(none.is_none() && errs.len() == 1);
    }

    // ---- resolve_mcp_config 多 scope 合并 ------------------------------------
    #[test]
    fn resolve_merges_scopes_high_wins_origin() {
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        let xdg = tmp.path().join("xdg");
        let env: EnvMap = std::iter::once((
            "XDG_CONFIG_HOME".to_string(),
            xdg.to_string_lossy().into_owned(),
        ))
        .collect();

        // user mcp.json：srv-a（user）。
        write(
            &user_mcp_config_path(Some(&env)),
            r#"{"servers": {"srv-a": {"type":"stdio","server_parameters":{"command":"u"}}},
                "inputs": [{"type":"PromptString","id":"tok","description":"d"}]}"#,
        );
        // project mcp.json：srv-a（project 覆盖 user 整体替换）+ srv-b。
        write(
            &workdir_mcp_config_path(&wd),
            r#"{"servers": {
                "srv-a": {"type":"stdio","server_parameters":{"command":"p"}},
                "srv-b": {"type":"stdio","server_parameters":{"command":"b"}}
            }}"#,
        );

        let resolved = resolve_mcp_config(ResolveMcpConfigArgs {
            cwd: Some(&wd),
            env: Some(&env),
            // 用不存在的 managed 路径，避免读到真实系统 managed-mcp.json。
            managed_mcp_path: Some(&tmp.path().join("no-managed.json")),
            ..Default::default()
        });
        assert_eq!(resolved.servers.len(), 2);
        // srv-a：project 整体替换 user → origin=Project、非 trusted、command=p。
        let a = &resolved.servers["srv-a"];
        assert_eq!(a.origin, SettingsScope::Project);
        assert!(!a.trusted_origin);
        assert_eq!(a.config.name(), "srv-a");
        // srv-b project。
        assert_eq!(resolved.servers["srv-b"].origin, SettingsScope::Project);
        // inputs（来自 user）。
        assert_eq!(resolved.inputs.len(), 1);
        assert_eq!(resolved.inputs[0].id(), "tok");
        assert!(resolved.errors.is_empty());
    }

    // ---- mcp_server_status 8 档优先级 ---------------------------------------
    #[test]
    fn status_priority_matrix() {
        let empty = Map::new();
        let no_bundled = HashSet::new();
        // ⑧ 未决 → Pending（非 trusted、无名单）。
        assert_eq!(
            mcp_server_status("s", &empty, &no_bundled, false),
            McpApprovalStatus::Pending
        );
        // ⑤ trusted → Enabled。
        assert_eq!(
            mcp_server_status("s", &empty, &no_bundled, true),
            McpApprovalStatus::Enabled
        );
        // ④ bundled → Enabled（即便非 trusted）。
        let bundled: HashSet<String> = std::iter::once("s".to_string()).collect();
        assert_eq!(
            mcp_server_status("s", &empty, &bundled, false),
            McpApprovalStatus::Enabled
        );
        // ① denied 最高优先 → Disabled（即便 trusted + bundled）。
        let denied = settings_with(json!({"deniedMcpServers": ["s"]}));
        assert_eq!(
            mcp_server_status("s", &denied, &bundled, true),
            McpApprovalStatus::Disabled
        );
        // ② allowed 非空且不在其中 → Disabled。
        let allowed = settings_with(json!({"allowedMcpServers": ["other"]}));
        assert_eq!(
            mcp_server_status("s", &allowed, &no_bundled, true),
            McpApprovalStatus::Disabled
        );
        // ③ disabled 优先 over enabled → Disabled。
        let both = settings_with(json!({
            "disabledMcpjsonServers": ["s"], "enabledMcpjsonServers": ["s"]
        }));
        assert_eq!(
            mcp_server_status("s", &both, &no_bundled, false),
            McpApprovalStatus::Disabled
        );
        // ⑥ enabledMcpjsonServers → Enabled。
        let en = settings_with(json!({"enabledMcpjsonServers": ["s"]}));
        assert_eq!(
            mcp_server_status("s", &en, &no_bundled, false),
            McpApprovalStatus::Enabled
        );
        // ⑦ enableAllProjectMcpServers → Enabled。
        let all = settings_with(json!({"enableAllProjectMcpServers": true}));
        assert_eq!(
            mcp_server_status("s", &all, &no_bundled, false),
            McpApprovalStatus::Enabled
        );
    }

    #[test]
    fn gate_applies_to_all_servers() {
        let srv = validate_server(
            "s",
            &json!({"type":"stdio","server_parameters":{"command":"x"}}),
            SettingsScope::Project,
            None,
        )
        .0
        .unwrap();
        let mut servers = IndexMap::new();
        servers.insert("s".to_string(), srv);
        let resolved = ResolvedMcpConfig {
            servers,
            ..Default::default()
        };
        let gated = gate_mcp_servers(&resolved, &Map::new(), &HashSet::new());
        assert_eq!(gated["s"], McpApprovalStatus::Pending);
    }

    // ---- bundled_mcp_server_names ------------------------------------------
    #[test]
    fn bundled_names_from_ledger() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let env = EnvMap::new();
        // 空账本 → 空。
        assert!(bundled_mcp_server_names(Some(home), Some(&env)).is_empty());
        // 写一条记录。
        crate::settings::store::update_installed_plugins(
            |file| {
                file.account.plugins.insert(
                    "audit@acme".to_string(),
                    vec![crate::settings::reconciler::InstalledPluginRecord {
                        install_path: Some("/x".to_string()),
                        bundled_mcp_servers: vec!["audit-mcp".to_string()],
                        extra: Map::new(),
                    }],
                );
            },
            Some(home),
            Some(&env),
        )
        .unwrap();
        let names = bundled_mcp_server_names(Some(home), Some(&env));
        assert!(names.contains("audit-mcp"));
    }

    // ---- 批准写助手（#98：锚定 cwd，无 fail-fast）----------------------------
    #[test]
    fn approve_deny_and_all_write_local_settings_with_dedup() {
        // #98：批准写锚定注入 cwd（`Some(&wd)`），不再要求 active workdir、无 fail-fast。
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");

        approve_mcp_server("s", Some(&wd)).unwrap();
        approve_mcp_server("s", Some(&wd)).unwrap(); // dedup
        deny_mcp_server("bad", Some(&wd)).unwrap();
        approve_all_project_mcp(Some(&wd)).unwrap();

        let (settings, _e) =
            load_settings_file(&workdir_local_settings_path(&wd), SettingsScope::Local);
        assert_eq!(
            str_list(&settings, FIELD_ENABLED_MCPJSON_SERVERS),
            vec!["s"] // dedup：仅一项
        );
        assert_eq!(
            str_list(&settings, FIELD_DISABLED_MCPJSON_SERVERS),
            vec!["bad"]
        );
        assert_eq!(
            settings.get(FIELD_ENABLE_ALL_PROJECT_MCP),
            Some(&Value::Bool(true))
        );

        // 写入后 resolve_settings 视角下 status 一致：approved server → enabled。
        let bundled = HashSet::new();
        assert_eq!(
            mcp_server_status("s", &settings, &bundled, false),
            McpApprovalStatus::Enabled
        );
        assert_eq!(
            mcp_server_status("bad", &settings, &bundled, false),
            McpApprovalStatus::Disabled
        );
    }

    #[test]
    fn managed_path_per_platform() {
        assert!(managed_mcp_config_path(Some("darwin"))
            .to_string_lossy()
            .contains("A2CComputer"));
        assert!(managed_mcp_config_path(Some("linux"))
            .to_string_lossy()
            .ends_with("managed-mcp.json"));
    }

    // ---- 🟡2：wire 字符串单源钉死（serde rename_all 与手写 as_str 不得漂移）-----
    #[test]
    fn approval_status_wire_strings_pinned() {
        for (status, wire) in [
            (McpApprovalStatus::Enabled, "enabled"),
            (McpApprovalStatus::Disabled, "disabled"),
            (McpApprovalStatus::Pending, "pending"),
        ] {
            assert_eq!(status.as_str(), wire);
            assert_eq!(serde_json::to_value(status).unwrap(), json!(wire));
        }
    }

    // ---- 🟡3：未知 server 键宽容（钉死 Rust 行为，拦 deny_unknown_fields 无声漂移）---
    #[test]
    fn unknown_server_key_is_leniently_accepted() {
        // 非 envFile 的未知键（typo / VS Code 扩展）：Rust 共享模型 serde 默认忽略 → 容受、不 drop。
        let sdef = json!({
            "type": "stdio",
            "server_parameters": {"command": "x"},
            "unknownExtKey": "ignored"
        });
        let (srv, errs) = validate_server("s", &sdef, SettingsScope::User, None);
        assert!(
            srv.is_some(),
            "unknown key tolerated (no deny_unknown_fields)"
        );
        assert!(errs.is_empty());
        // 对照：真正畸形（缺必填 command）仍 drop+error（与 Python 一致）。
        let bad = json!({"type": "stdio", "server_parameters": {}});
        assert!(validate_server("s", &bad, SettingsScope::User, None)
            .0
            .is_none());
    }

    // ---- 🟡1：resolve 层补测（对标 Python test_mcp_config 集成层）----------------
    /// 用不存在的 managed 路径，隔离真实系统 managed-mcp.json。
    fn no_managed(tmp: &TempDir) -> PathBuf {
        tmp.path().join("no-managed.json")
    }
    fn xdg_env(tmp: &TempDir) -> EnvMap {
        std::iter::once((
            "XDG_CONFIG_HOME".to_string(),
            tmp.path().join("xdg").to_string_lossy().into_owned(),
        ))
        .collect()
    }

    #[test]
    fn resolve_mcp_config_anchors_project_at_cwd() {
        // #98：project/local 锚定注入 cwd。cwd=Some(wd) 时读 <wd>/.tfrobot/mcp.json：
        // srv-p → origin=Project、trusted_origin=false（project/local 非受信、须门控）。
        let tmp = TempDir::new().unwrap();
        let env = xdg_env(&tmp);
        let wd = tmp.path().join("wd");
        write(
            &user_mcp_config_path(Some(&env)),
            r#"{"servers": {"srv-u": {"type":"stdio","server_parameters":{"command":"u"}}}}"#,
        );
        write(
            &workdir_mcp_config_path(&wd),
            r#"{"servers": {"srv-p": {"type":"stdio","server_parameters":{"command":"p"}}}}"#,
        );
        let resolved = resolve_mcp_config(ResolveMcpConfigArgs {
            cwd: Some(&wd),
            env: Some(&env),
            managed_mcp_path: Some(&no_managed(&tmp)),
            ..Default::default()
        });
        assert!(resolved.servers.contains_key("srv-u"));
        let p = &resolved.servers["srv-p"];
        assert_eq!(p.origin, SettingsScope::Project);
        assert!(!p.trusted_origin); // project 层 → 非受信、须门控
        assert_eq!(resolved.servers["srv-u"].origin, SettingsScope::User);
    }

    #[test]
    fn resolve_inputs_dedup_high_wins_and_idless_dropped() {
        let tmp = TempDir::new().unwrap();
        let env = xdg_env(&tmp);
        let wd = tmp.path().join("wd");
        // user：input tok(desc=u) + 一条无 id（→ <noid-N> → 校验失败 drop）。
        write(
            &user_mcp_config_path(Some(&env)),
            r#"{"inputs": [
                {"type":"PromptString","id":"tok","description":"u"},
                {"type":"PromptString","description":"idless"}
            ]}"#,
        );
        // project：input tok(desc=p) → 同 id 高 scope 胜。
        write(
            &workdir_mcp_config_path(&wd),
            r#"{"inputs": [{"type":"PromptString","id":"tok","description":"p"}]}"#,
        );
        let resolved = resolve_mcp_config(ResolveMcpConfigArgs {
            cwd: Some(&wd),
            env: Some(&env),
            managed_mcp_path: Some(&no_managed(&tmp)),
            ..Default::default()
        });
        // 去重 → 仅 tok；高 scope（project）胜 → desc=p。
        assert_eq!(resolved.inputs.len(), 1);
        assert_eq!(resolved.inputs[0].id(), "tok");
        assert_eq!(resolved.inputs[0].description(), "p");
        // 无 id 项被 drop 且记错（noid 路径 + §5.6）。
        assert!(resolved
            .errors
            .iter()
            .any(|e| e.field == "inputs.<unknown>"));
    }

    #[test]
    fn resolve_malformed_server_dropped_sibling_survives() {
        let tmp = TempDir::new().unwrap();
        let env = xdg_env(&tmp);
        let wd = tmp.path().join("wd");
        // ok 合法 + bad 缺必填 command → bad drop、ok 存活、不 abort（§5.6）。
        write(
            &workdir_mcp_config_path(&wd),
            r#"{"servers": {
                "ok":  {"type":"stdio","server_parameters":{"command":"x"}},
                "bad": {"type":"stdio","server_parameters":{}}
            }}"#,
        );
        let resolved = resolve_mcp_config(ResolveMcpConfigArgs {
            cwd: Some(&wd),
            env: Some(&env),
            managed_mcp_path: Some(&no_managed(&tmp)),
            ..Default::default()
        });
        assert!(resolved.servers.contains_key("ok"));
        assert!(!resolved.servers.contains_key("bad"));
        assert!(resolved.errors.iter().any(|e| e.field == "servers.bad"));
    }

    #[test]
    fn resolve_local_overrides_project_and_policy_is_trusted() {
        let tmp = TempDir::new().unwrap();
        let env = xdg_env(&tmp);
        let wd = tmp.path().join("wd");
        // project srv-x(command=p) ← local srv-x(command=l) 整体替换、origin=Local、非 trusted。
        write(
            &workdir_mcp_config_path(&wd),
            r#"{"servers": {"srv-x": {"type":"stdio","server_parameters":{"command":"p"}}}}"#,
        );
        write(
            &workdir_mcp_local_config_path(&wd),
            r#"{"servers": {"srv-x": {"type":"stdio","server_parameters":{"command":"l"}}}}"#,
        );
        // policy srv-y → origin=Policy、trusted_origin。
        let policy = tmp.path().join("managed-mcp.json");
        write(
            &policy,
            r#"{"servers": {"srv-y": {"type":"stdio","server_parameters":{"command":"pol"}}}}"#,
        );
        let resolved = resolve_mcp_config(ResolveMcpConfigArgs {
            cwd: Some(&wd),
            env: Some(&env),
            managed_mcp_path: Some(&policy),
            ..Default::default()
        });
        let x = &resolved.servers["srv-x"];
        assert_eq!(x.origin, SettingsScope::Local);
        assert!(!x.trusted_origin); // local 受门控
        let y = &resolved.servers["srv-y"];
        assert_eq!(y.origin, SettingsScope::Policy);
        assert!(y.trusted_origin); // policy 预信任
    }
}
