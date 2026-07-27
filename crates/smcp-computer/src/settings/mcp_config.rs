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
//! 协议依据 / Protocol: `guides/computer-mcp-config-guide.md`（mcp.json 定义层、A2C 原生 schema）/
//! `guides/mcp-approval-gate-alignment.md`（批准门控档位表、双 SDK 共同对齐锚点）/ `runtime-contract.md`
//! §2.5（来源优先序）· §5 item 10（两套开关正交）。对标 Python `computer/settings/mcp_config.py`。
//!
//! ⚠️ 早前注释引用的 `§9.1` / `§9.2` 是**幽灵章节**：`computer-management/protocol.md` 的 §9 是「兼容性」、
//! `runtime-contract.md` 压根只到 §8，两处都没有 mcp.json 定义层或批准门控的子节。mcp.json 与批准门控的权威
//! 已是上述 `guides/`。该幽灵引用曾真实误导过一次决策，勿再复活。
//!
//! **引用规则**：协议引用 MUST 带**文件名**（如 `runtime-contract.md §5 item 10`），裸 `§X.Y` 无从校验、正是
//! 幽灵滋生的温床。本文件内 `§5.5` / `§5.6` 等裸引用同属待清理项（经核实亦为幽灵，`runtime-contract.md` §5 是
//! 扁平 1–10 条、无子节）—— 一并扫净归后续卫生批次（#132）。
//!
//! 本模块是 **MCP 定义/门控的纯逻辑层**（无 git / 无 MCP manager / 无网络）。职责三件：
//! 1. **多 scope 加载合并** `mcp.json`——顺序 `policy > flag > local > project > user`（协议 `runtime-contract.md`
//!    §2.5 完整序，F6：flag 次高、与 settings.json 同序），**无能力层并集**（敏感面隔离，区别于 settings.json）；
//!    server 按 name **整体替换**（配置是原子单元、非深合并），记录最高定义 scope 为 `origin`。
//! 2. **批准门控判定**（[`mcp_server_status`]）——**只**据 resolved settings（MCP 门控字段）+ 声明 `origin` 算
//!    `enabled/disabled/pending`。**不读账本 / bundled 名集**（#131：读账本即授权门绕过，见该函数文档）。
//! 3. **批准写助手**——批准/拒绝写 **local scope**（`settings.local.json`，个人决定不污染共享层），
//!    复用 store 持锁原子 RMW（无写保护头，同 installer `enabledPlugins`）。
//!
//! **显式划界 / Deferred boundaries**：
//! - **取值渲染**（`envFile` 加载 / `${env:}` / inputs 解析链 / keyring / 明文 state）归 inputs 层（#73）：
//!   本模块产出**带占位符**的定义，[`ResolvedMcpServer::ext`]（`envFile` 等 VS Code 扩展）+ 未渲染占位符是
//!   handoff。**绝不在此渲染**（安全铁律：值不离 Computer）。
//! - **批准框 TTY 交互** / `--approve-all-mcp` / 非交互 pending→skip+WARN 接线归 CLI（#48/#69）：本模块只提供
//!   [`McpApprovalStatus`] 判定 + 三个写助手原语。
//! - `managed-mcp.json` 仅读 per-platform managed dir（remote/MDM stub），对齐 [`crate::settings::policy`]。
//!
//! **容错姿态**：`mcp.json` 是**人/团队编辑文件**，故**字段级容错**（单 server / input 畸形 → drop + 记
//! [`SettingsValidationError`]，**不 abort**）——刻意区别于 [`crate::skills::manifest`] 对 plugin-bundled
//! server 的**硬抛**（那是 install 原子前置）。
//!
//! **与 Python 的差异 / Divergence**：Python `MCPServerConfig`（pydantic `extra="forbid"`）令非 `envFile` 的
//! 未知 server 键 drop+error；Rust 共享 [`MCPServerConfig`] 模型对未知键**宽容**（serde 默认忽略），故此类边角键
//! 被容受而非 drop。强制拒绝未知 server 键属模型层硬化（影响全 crate 反序列化），**不**在本模块引入。
//! 行为由 `unknown_server_key_is_leniently_accepted` 测试钉死——若未来给模型加 `deny_unknown_fields` 会令其失败、
//! 拦截无声语义漂移。**下游接缝须知（#73/#69）**：Python docstring 依赖的「畸形 server 被 drop → 进启动 WARN」语义
//! 对此类未知键在 Rust **不触发**（该 server 被容受、不入 `errors`），故 CLI 启动 WARN 接线**不应**假定与 Python
//! 逐条对等；真正畸形（缺必填 / `type` 非法 / name-key 冲突）仍 drop+error，与 Python 一致。

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde::Serialize;
use serde_json::{Map, Value};

use crate::mcp_clients::model::{MCPServerConfig, MCPServerInput};
use crate::settings::config::snapshot::ProvenanceScope;
use crate::settings::policy::{LINUX_MANAGED_DIR, MACOS_MANAGED_DIR, WINDOWS_MANAGED_DIR};
use crate::settings::schema::{
    SettingsScope, SettingsValidationError, FIELD_ALLOWED_MCP_SERVERS, FIELD_DENIED_MCP_SERVERS,
    FIELD_DISABLED_MCPJSON_SERVERS, FIELD_ENABLED_MCPJSON_SERVERS, FIELD_ENABLE_ALL_PROJECT_MCP,
};
use crate::settings::scope::{
    apply_write, load_settings_file, resolve_cwd, resolve_user_config_dir,
    workdir_local_settings_path, workdir_settings_dir, EnvMap, WriteValue,
};
use crate::settings::store::{self, SettingsStoreError};

// ---------------------------------------------------------------------------
// 常量 / Constants
// ---------------------------------------------------------------------------
/// user / project scope 定义文件名（`computer-mcp-config-guide.md`）/ definition filename for user/project scope。
pub const MCP_CONFIG_FILENAME: &str = "mcp.json";
/// local scope 文件名（`<cwd>/.tfrobot/`，不入 git）/ local-scope filename (not git-tracked)。
pub const MCP_LOCAL_CONFIG_FILENAME: &str = "mcp.local.json";
/// policy scope 文件名（企业下发）/ policy-scope filename (enterprise-managed)。
pub const MANAGED_MCP_FILENAME: &str = "managed-mcp.json";

/// server 定义里的 VS Code 风格扩展键：非 A2C `MCPServerConfig` 字段，校验前剥离、原样交渲染层 / ext keys。
const VSCODE_EXT_KEYS: &[&str] = &["envFile"];

// 预信任 origin 集（免批准门控）现集中于 [`ProvenanceScope::is_trusted_origin`]（`config::snapshot`）——
// `{user, embed, flag, policy}`（#137 受信集扩位 embed）。文件 scope 经 `From<SettingsScope>` 落该集，
// 结果对 mcp.json 声明面等价于旧 `[user, flag, policy]`（embed 非文件 scope、其运行期接线归 #147）。

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
/// 单个 MCP server 的批准门控状态（审批门对齐指南 §2）/ Approval-gate status of one MCP server。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum McpApprovalStatus {
    /// 已批准 / 预信任 origin → 可连接 / approved → connectable。
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
    /// `--mcp-config` flag 层 mcp.json 文件（**次高**，仅低于 policy；F6，协议 §2.5）/ flag-scope mcp config path。
    pub flag_config_path: Option<&'a Path>,
    /// policy scope `managed-mcp.json` 覆盖路径（缺省按平台推导）/ managed path override。
    pub managed_mcp_path: Option<&'a Path>,
    /// 平台标识（缺省 `std::env::consts::OS`；接受 `darwin`/`win32`/`linux` 或 `macos`/`windows`）/ platform。
    pub platform: Option<&'a str>,
    /// 宿主构造入参 `Computer::new(mcp_servers=…)` 的 **embed 层**（插在 local 与 flag 之间，§2.5-3；#147/S14）。
    /// 每条 config 以 `cfg.name()` 为 map 键投影成 mcp.json 形状的一层，origin=embed、预信任（`is_trusted_origin`）。
    /// 与 flag/durable 同为**当次 boot 声明式输入**，每次 resolve 重算、**不落盘**（§2.5-5）。
    pub embed_servers: &'a [MCPServerConfig],
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
///
/// `pub(crate)`：S4（`config::validate`）复用为 schema-only 校验单元（对内存 doc 校验，无 I/O）。
pub(crate) fn validate_server(
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
        trusted_origin: ProvenanceScope::from(scope).is_trusted_origin(),
    };
    (Some(server), Vec::new())
}

/// 校验单个 input 定义 → [`MCPServerInput`]（畸形 → `None` + 错误，**不抛**）/ Validate one input def。
///
/// `pub(crate)`：S4（`config::validate`）复用为 schema-only 校验单元。
pub(crate) fn validate_input(
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
// 写侧归一化 / Write-side canonicalization
// ---------------------------------------------------------------------------
/// 落盘前把类型化 `MCPServerConfig` 的序列化体归一化为 `mcp.json` 规范形 / canonicalize the persist body.
///
/// [`validate_server`] 是读侧校验单元；本函数是**写侧**归一化单元（读写对称、同居本模块）。两处订正，
/// 保跨 SDK（Python）可读 + Rust 自身重启回读：
/// 1. 剥内嵌 `name`——map key 即身份（内嵌 `name` 与 key 冲突则判废）。
/// 2. `type` 判别符归一化为协议规范小写：Rust enum 变体名序列化为 `Stdio`/`Sse`/`Http`，改写为
///    `stdio`/`sse`/`streamable`（Python `Literal` 大小写敏感；`streamable` 对齐 `StreamableHttpServerConfig`）。
///    Rust 读端经 `alias` 接受该规范形，故往返无损。
///
/// `pub(crate)`：复用于 `Computer::add_or_update_server_in_scope`（落盘）与 typed MCP import/preflight
/// （`settings::config::import`）。纯函数、无 I/O。
pub(crate) fn canonicalize_persist_body(mut body: Value) -> Value {
    if let Some(obj) = body.as_object_mut() {
        obj.remove("name");
        let canonical = obj.get("type").and_then(Value::as_str).map(|t| {
            match t {
                "Stdio" => "stdio",
                "Sse" => "sse",
                "Http" => "streamable",
                // 已是规范小写（防御：body 本就规范则原样）/ already canonical.
                other => other,
            }
            .to_string()
        });
        if let Some(t) = canonical {
            obj.insert("type".to_string(), Value::String(t));
        }
    }
    body
}

// ---------------------------------------------------------------------------
// 多 scope 解析 / Multi-scope resolution
// ---------------------------------------------------------------------------

/// embed 层 source 标识（诊断 / origin 溯源，非文件路径）/ embed-layer source marker (not a file path)。
/// 对齐 python `mcp_config.py::_EMBED_SOURCE`。
const EMBED_SOURCE: &str = "<embed:Computer(mcp_servers=...)>";

/// resolve 的一层来源：磁盘文件（5 个 scope）或内存 embed 层（宿主构造入参，#147）/ one resolve layer source。
enum McpLayer<'a> {
    /// 磁盘 mcp.json 文件层（user/project/local/flag/policy）/ on-disk mcp.json file layer。
    File(SettingsScope, PathBuf),
    /// 内存 embed 层：`Computer::new(mcp_servers=…)` 的构造入参（origin=embed）/ in-memory embed layer。
    Embed(&'a [MCPServerConfig]),
}

/// 多 scope 加载合并 `.tfrobot/mcp.json` + 字段级校验 / Multi-scope load + merge + validate mcp.json。
///
/// 合并顺序 low → high = `[user, project, local, flag, policy]`（优先级
/// `policy > flag > local > project > user`，`runtime-contract.md` §2.5；F6：flag 次高、与 settings.json 同序）；
/// **无能力层并集**。#98：project/local **无条件**锚定
/// 进程 cwd（`cwd` 注入接缝，`None` → `std::env::current_dir()`；cwd 不可读则该两层缺省）。server 按 name
/// **整体替换**（`origin` = 最高定义 scope）；inputs 按 `id` 去重高 scope 胜（缺 `id` 的条目各自保留以逐条报错）。
/// 单 server / input 畸形 → drop + 错误，**不 abort**（§5.6）。
#[must_use]
pub fn resolve_mcp_config(args: ResolveMcpConfigArgs<'_>) -> ResolvedMcpConfig {
    let managed_path = args
        .managed_mcp_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| managed_mcp_config_path(args.platform));

    // 层，低优先级在前 / layers, lowest priority first。
    // 序 = 协议 `runtime-contract.md §2.5` 完整序 `user < project < local < embed < flag < policy`
    // （F6：flag **次高**，仅低于 policy；#147：embed 层插在 local 与 flag 之间）。settings.json 与 mcp.json
    // 两套来源 MUST 同序。embed 是**内存层**（宿主构造入参），非文件路径 ⇒ 用 `McpLayer` 枚举承载。
    let mut layers: Vec<McpLayer<'_>> = Vec::new();
    layers.push(McpLayer::File(
        SettingsScope::User,
        user_mcp_config_path(args.env),
    ));
    // project/local：无条件锚定进程 cwd（cwd 不可读 → 跳过该两层）。
    if let Some(base) = resolve_cwd(args.cwd) {
        layers.push(McpLayer::File(
            SettingsScope::Project,
            workdir_mcp_config_path(&base),
        ));
        layers.push(McpLayer::File(
            SettingsScope::Local,
            workdir_mcp_local_config_path(&base),
        ));
    }
    // embed（`Computer::new(mcp_servers=…)`）：local 与 flag 之间（§2.5-3；#147/S14）。空集不贡献。
    if !args.embed_servers.is_empty() {
        layers.push(McpLayer::Embed(args.embed_servers));
    }
    // flag（`--mcp-config`）：次高——CLI 显式传入覆盖用户默认配置（F6，协议 §2.5）。历史实现把它排最低
    // （`--config` 老接口遗留），令用户默认反覆盖 CLI 显式传入、违反直觉 —— 已废止。
    if let Some(fc) = args.flag_config_path {
        layers.push(McpLayer::File(SettingsScope::Flag, fc.to_path_buf()));
    }
    layers.push(McpLayer::File(SettingsScope::Policy, managed_path));

    let mut errors: Vec<SettingsValidationError> = Vec::new();
    // 累积原始定义（低→高，后者覆盖前者）；IndexMap 保插入序、同 key 覆盖值不挪位（对齐 Python dict）。
    let mut raw_servers: IndexMap<String, (Value, SettingsScope, String)> = IndexMap::new();
    let mut raw_inputs: IndexMap<String, (Value, SettingsScope, String)> = IndexMap::new();
    let mut noid: usize = 0;

    for layer in &layers {
        match layer {
            McpLayer::File(scope, path) => {
                let (file, errs) = load_mcp_config_file(path, *scope);
                errors.extend(errs);
                let src = path.to_string_lossy().into_owned();
                for (srv_name, sdef) in file.servers {
                    // #151 Part 1：被更高优先级层遮蔽（insert 覆盖）的定义在此即丢失——MUST 先校验它，
                    // 否则「可解析 JSON 内被 precedence 遮蔽的非法实体」诊断静默丢失（获胜者由合并后循环校验）。
                    if let Some((shadowed_def, shadowed_scope, shadowed_src)) =
                        raw_servers.insert(srv_name.clone(), (sdef, *scope, src.clone()))
                    {
                        let (_, errs) = validate_server(
                            &srv_name,
                            &shadowed_def,
                            shadowed_scope,
                            Some(&shadowed_src),
                        );
                        errors.extend(errs);
                    }
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
                    // #151 Part 1：被遮蔽的 input 定义同样先校验（id 键遮蔽）。
                    if let Some((shadowed_def, shadowed_scope, shadowed_src)) =
                        raw_inputs.insert(key, (idef, *scope, src.clone()))
                    {
                        let (_, errs) =
                            validate_input(&shadowed_def, shadowed_scope, Some(&shadowed_src));
                        errors.extend(errs);
                    }
                }
            }
            // embed 层：宿主构造入参投影成 mcp.json 形状（map 键 = `cfg.name()`，与文件层身份承载一致）。
            // origin=embed 由 `validate_server(scope=Embed)` 落定；trusted 经 `is_trusted_origin`。**无 inputs**
            // （构造入参 `inputs=` 是另一条通路，不经本层）。config 已是校验后的 A2C 模型 ⇒ `to_value` 回落 raw、
            // 交同一 `validate_server` 往返（`name` 字段与 map 键一致，不触发身份冲突判废）。
            McpLayer::Embed(cfgs) => {
                for cfg in *cfgs {
                    let name = cfg.name().to_string();
                    match serde_json::to_value(cfg) {
                        Ok(sdef) => {
                            // #151 Part 1：embed 覆盖文件层同名声明时，先校验被遮蔽者。
                            if let Some((shadowed_def, shadowed_scope, shadowed_src)) = raw_servers
                                .insert(
                                    name.clone(),
                                    (sdef, SettingsScope::Embed, EMBED_SOURCE.to_string()),
                                )
                            {
                                let (_, errs) = validate_server(
                                    &name,
                                    &shadowed_def,
                                    shadowed_scope,
                                    Some(&shadowed_src),
                                );
                                errors.extend(errs);
                            }
                        }
                        // 校验后的模型序列化几乎不可能失败；真失败则记诊断、不阻断其余层。
                        Err(e) => errors.push(err(
                            SettingsScope::Embed,
                            &format!("servers.{name}"),
                            &format!("embed server config failed to serialize: {e}"),
                            Some(EMBED_SOURCE),
                        )),
                    }
                }
            }
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
// 批准门控判定 / Approval-gate decision（审批门对齐指南 §2 档位表）
// ---------------------------------------------------------------------------
/// 从 resolved settings 取字符串数组字段（非 list → `[]`）/ Read a string-array field (non-list → [])。
fn str_list<'a>(settings: &'a Map<String, Value>, key: &str) -> Vec<&'a str> {
    settings
        .get(key)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

/// 判定单个 MCP server 的批准状态（**顺序即优先级**）/ Decide one server's approval status。
///
/// 协议依据：[审批门对齐指南][guide] §2 档位表（SDK 非规范性共同对齐锚点，双 SDK MUST 行为一致）。
///
/// # ⚠️ 与指南的两处**已知未对齐**（勿把"引用了 §2"误读为"已对齐 §2"）
///
/// 1. **键仍是 display 名，非 `bundle_id`**：指南 §1 定四个名单数组的元素、§2 定本函数入参**一律为
///    `bundle_id`**；本实现收的 `name` 是 `gate_mcp_servers` 从 `mcp.json` map key 迭代出的 **display 名**
///    （Python `mcp_config.py` 同为 name-keyed ⇒ **双端对称**，非单边分歧）。其后果正是指南 §1 所述的信任
///    泄漏（同名两条 server 共用一份审批）。换键归 **#136–#141**；届时下方 `const _` 会显红，**那是预期**——
///    连同本节一并更新。
/// 2. ~~档⑤/⑥ 的判据可由不受信 scope 供给~~ —— **已由 #143 修复**（project scope 供给即过滤+记错，
///    协议 §2.1）；见 [`gate_mcp_servers`] 的信任约束节。
///
/// 优先级（先到先决）：① `deniedMcpServers` → Disabled；② `allowedMcpServers` 非空且不在其中 → Disabled；
/// ③ `disabledMcpjsonServers` → Disabled（disabled 优先 over enabled）；④ `trusted_origin`（user/flag/policy）
/// → Enabled；⑤ `enabledMcpjsonServers` → Enabled；⑥ `enableAllProjectMcpServers == true` → Enabled；
/// ⑦ 否则 → Pending。
///
/// # 「bundled 名免批准」档位已删除，MUST NOT 以任何形状复活（#131 · 指南 §2 danger）
///
/// 本函数**只**判定 `mcp.json` 各 scope **声明的** server；plugin 声明依赖的 server **MUST NOT 进入本门迭代**
/// （其可信性由 install ∧ enable 门保证，见 `runtime-contract.md` §2.5/§5 item 10），**禁止**写成门内「进门后
/// 豁免」档位。历史档④ `bundled.contains(name)` 是授权门绕过：真 bundled server 走 enable→mount、**从不进**
/// [`resolve_mcp_config`]（其层只有 Flag/User/Project/Local/Policy 五个**配置文件**层）⇒ 该档唯一可达路径 =
/// 「project/local 声明借用了某已装 plugin 的 server 名」= 100% 借名跳过批准门。
///
/// 故本函数 MUST NOT 依赖物化账本 / bundled 名集——**签名不含 `bundled` 入参**即 F8 判据①的可验收信号
/// （由下方 `const _` 编译期钉死）。
///
/// [guide]: https://github.com/A2C-SMCP/a2c-smcp-protocol/blob/develop/docs/guides/mcp-approval-gate-alignment.md
#[must_use]
pub fn mcp_server_status(
    name: &str,
    settings: &Map<String, Value>,
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

/// **F8 判据①（编译期钉死）**：审批门 MUST NOT 依赖账本 / bundled 名集——签名多出 `bundled` 入参即编译失败。
///
/// 对标 python-sdk 的 `inspect` 运行时签名断言（`test_mcp_server_status_signature_has_no_bundled`）；Rust 用
/// 函数指针类型钉死，比运行时反射更强（不合规则**根本编不过**）。
const _: fn(&str, &Map<String, Value>, bool) -> McpApprovalStatus = mcp_server_status;

/// 对全部已解析 server 套 [`mcp_server_status`] / Apply the gate to all resolved servers。
///
/// # 判据来源的信任约束（#143 已落地 · 协议指南 §2.1）
///
/// 本函数喂给 [`mcp_server_status`] 的 `settings` **必须**是经 `validate_settings` 过滤后的合并视图 ——
/// 其中 **project scope 供给的 enable 方向判据**（[`TRUSTED_SCOPE_ONLY_FIELDS`](crate::settings::TRUSTED_SCOPE_ONLY_FIELDS)：
/// `enabledMcpjsonServers` / `enableAllProjectMcpServers`）**已被过滤 + 记错**。
///
/// 理由（协议 §2.1 通则）：**审批门的输入 MUST 来自比被判定 server 更高信任的来源；任何 scope 都不得为
/// 「自身是否受信」提供判据**。`.tfrobot/settings.json` 与 `mcp.json` 一样**入 git**——若门接受它供给档⑤/⑥，
/// 被 clone 的仓库携一份 `{"enableAllProjectMcpServers": true}` 即可自我批准（与 #131 删掉的档④ **同构且更
/// 易达成**）。`disabledMcpjsonServers`（DENY 方向）**不受此限**——fail-safe，更严格永远安全。
///
/// ⚠️ 若未来有人绕开 `validate_settings` 自行拼 `settings` 喂本函数，该约束即失效。守护：
/// `project_scope_settings_cannot_self_approve_143`。
///
#[must_use]
pub fn gate_mcp_servers(
    resolved: &ResolvedMcpConfig,
    settings: &Map<String, Value>,
) -> IndexMap<String, McpApprovalStatus> {
    resolved
        .servers
        .iter()
        .map(|(name, srv)| {
            (
                name.clone(),
                mcp_server_status(name, settings, srv.trusted_origin),
            )
        })
        .collect()
}

// #138（F8 判据②）：`bundled_mcp_server_names()`（name-join、不分启用态的账本并集）**已整体删除**——
// 它是 #126 假阳性的根源（撞任一已装插件 bundled 名即标记，无视 enable/intent），且按 display name 关联
// （name 允许碰撞、非身份）。唯一消费者 `McpServerView.bundled` 已改 `origin == Plugin` 纯推导（见
// `config::snapshot`）；归属判定用 `settings::recovery::collect_enabled_bundled_servers`
// （intent ∧ `enabledPlugins` ∧ `bundle_id`）。审批门与 CRUD 归属门均不读账本名集（F8 判据①，签名 const pin）。

// ---------------------------------------------------------------------------
// 批准写助手（写 local scope = settings.local.json）/ Approval write helpers
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

/// 批准框 `[y]es`：追加 `enabledMcpjsonServers` 到 local scope / Approve → append to enabled list (local)。
///
/// #98：写锚定进程 cwd（`cwd` 注入接缝，`None` → 进程 cwd）。
///
/// # Errors
/// 进程 cwd 不可读 / 写失败 → [`McpConfigError`]。
pub fn approve_mcp_server(name: &str, cwd: Option<&Path>) -> Result<(), McpConfigError> {
    append_local_mcp_array(cwd, FIELD_ENABLED_MCPJSON_SERVERS, name)
}

/// 批准框 `[n]o`：追加 `disabledMcpjsonServers` 到 local scope / Deny → append to disabled list (local)。
///
/// #98：写锚定进程 cwd（`cwd` 注入接缝，`None` → 进程 cwd）。
///
/// # Errors
/// 进程 cwd 不可读 / 写失败 → [`McpConfigError`]。
pub fn deny_mcp_server(name: &str, cwd: Option<&Path>) -> Result<(), McpConfigError> {
    append_local_mcp_array(cwd, FIELD_DISABLED_MCPJSON_SERVERS, name)
}

/// 批准框 `[a]ll`：`enableAllProjectMcpServers=true` 写 local scope / Approve-all → set the bool (local)。
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

    /// #137 F6：flag scope（`--mcp-config`）**次高**——覆盖 user/project/local 同名声明（协议 §2.5 第3条
    /// `... local < embed < flag < policy`）。历史实现把 mcp.json 的 flag 排最低（`--config` 老接口遗留），
    /// 导致用户默认配置反覆盖 CLI 显式传入，违反直觉——本测钉死修正后的次高序。
    #[test]
    fn flag_scope_overrides_user_project_local_137() {
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        let xdg = tmp.path().join("xdg");
        let env: EnvMap = std::iter::once((
            "XDG_CONFIG_HOME".to_string(),
            xdg.to_string_lossy().into_owned(),
        ))
        .collect();

        // user + project + local 各声明 srv（低于 flag）。
        write(
            &user_mcp_config_path(Some(&env)),
            r#"{"servers": {"srv": {"type":"stdio","server_parameters":{"command":"user"}}}}"#,
        );
        write(
            &workdir_mcp_config_path(&wd),
            r#"{"servers": {"srv": {"type":"stdio","server_parameters":{"command":"project"}}}}"#,
        );
        write(
            &workdir_mcp_local_config_path(&wd),
            r#"{"servers": {"srv": {"type":"stdio","server_parameters":{"command":"local"}}}}"#,
        );
        // flag（--mcp-config）声明 srv（MUST 次高胜出）。
        let flag_file = tmp.path().join("flag-mcp.json");
        write(
            &flag_file,
            r#"{"servers": {"srv": {"type":"stdio","server_parameters":{"command":"flag"}}}}"#,
        );

        let resolved = resolve_mcp_config(ResolveMcpConfigArgs {
            cwd: Some(&wd),
            env: Some(&env),
            flag_config_path: Some(&flag_file),
            managed_mcp_path: Some(&tmp.path().join("no-managed.json")),
            ..Default::default()
        });

        let srv = &resolved.servers["srv"];
        // flag 次高胜出：origin=Flag、command=flag、trusted（flag ∈ 受信集）。
        assert_eq!(
            srv.origin,
            SettingsScope::Flag,
            "flag scope MUST 次高覆盖 user/project/local（协议 §2.5）"
        );
        assert!(srv.trusted_origin, "flag origin 预信任（审批门档④）");
        let command = match &srv.config {
            crate::mcp_clients::model::MCPServerConfig::Stdio(c) => {
                c.server_parameters.command.as_str()
            }
            _ => panic!("expected stdio server"),
        };
        assert_eq!(command, "flag", "胜出的 command 应来自 flag 层");
    }

    /// 构造一个 stdio embed server config（`name` = 身份 map 键）/ build one stdio embed config。
    #[cfg(test)]
    fn embed_stdio(name: &str, command: &str) -> MCPServerConfig {
        serde_json::from_value(json!({
            "type": "stdio",
            "name": name,
            "server_parameters": {"command": command},
        }))
        .unwrap()
    }

    /// 隔离 env/cwd（避免读到真实 user/project mcp.json）/ isolated env + empty cwd。
    #[cfg(test)]
    fn isolated_env(xdg: &Path) -> EnvMap {
        std::iter::once((
            "XDG_CONFIG_HOME".to_string(),
            xdg.to_string_lossy().into_owned(),
        ))
        .collect()
    }

    /// #147/S14：宿主构造入参（embed 层）→ resolve 输出携 `origin=Embed`、预信任（`is_trusted_origin`）。
    /// embed 是 §2.5-3 完整序里 local 与 flag 之间的一层；每次 resolve 从声明式入参重投影、不落盘。
    #[test]
    fn embed_layer_projects_origin_embed_trusted_147() {
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd"); // 空目录：无 .tfrobot/mcp.json。
        let env = isolated_env(&tmp.path().join("xdg"));
        let embed = vec![embed_stdio("host-srv", "embed")];

        let resolved = resolve_mcp_config(ResolveMcpConfigArgs {
            cwd: Some(&wd),
            env: Some(&env),
            managed_mcp_path: Some(&tmp.path().join("no-managed.json")),
            embed_servers: &embed,
            ..Default::default()
        });

        let s = resolved
            .servers
            .get("host-srv")
            .expect("embed server MUST appear in the resolve authoritative set (#147)");
        assert_eq!(s.origin, SettingsScope::Embed, "宿主构造入参 origin=embed");
        assert!(s.trusted_origin, "embed ∈ 受信集（审批门档④）");
    }

    /// #147：碰撞优先序——embed 覆盖 local（`local < embed`），flag 覆盖 embed（`embed < flag`）。
    /// 协议 `runtime-contract.md` §2.5-3 完整序 `... local < embed < flag < policy`。
    #[test]
    fn embed_priority_between_local_and_flag_147() {
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        let env = isolated_env(&tmp.path().join("xdg"));
        // local 声明 srv=local。
        write(
            &workdir_mcp_local_config_path(&wd),
            r#"{"servers": {"srv": {"type":"stdio","server_parameters":{"command":"local"}}}}"#,
        );
        let embed = vec![embed_stdio("srv", "embed")];

        // (a) local + embed → embed 胜。
        let r1 = resolve_mcp_config(ResolveMcpConfigArgs {
            cwd: Some(&wd),
            env: Some(&env),
            managed_mcp_path: Some(&tmp.path().join("no-managed.json")),
            embed_servers: &embed,
            ..Default::default()
        });
        let cmd1 = match &r1.servers["srv"].config {
            MCPServerConfig::Stdio(c) => c.server_parameters.command.as_str(),
            _ => panic!("stdio"),
        };
        assert_eq!(
            r1.servers["srv"].origin,
            SettingsScope::Embed,
            "embed > local"
        );
        assert_eq!(cmd1, "embed");

        // (b) + flag 声明 srv=flag → flag 胜（embed < flag）。
        let flag_file = tmp.path().join("flag-mcp.json");
        write(
            &flag_file,
            r#"{"servers": {"srv": {"type":"stdio","server_parameters":{"command":"flag"}}}}"#,
        );
        let r2 = resolve_mcp_config(ResolveMcpConfigArgs {
            cwd: Some(&wd),
            env: Some(&env),
            flag_config_path: Some(&flag_file),
            managed_mcp_path: Some(&tmp.path().join("no-managed.json")),
            embed_servers: &embed,
            ..Default::default()
        });
        let cmd2 = match &r2.servers["srv"].config {
            MCPServerConfig::Stdio(c) => c.server_parameters.command.as_str(),
            _ => panic!("stdio"),
        };
        assert_eq!(
            r2.servers["srv"].origin,
            SettingsScope::Flag,
            "flag > embed"
        );
        assert_eq!(cmd2, "flag");
    }

    // ==== #151 Part 1：跨 scope 遮蔽的非法声明须被独立校验、诊断穿出（获胜配置不变）=========

    /// #151：User 非法 `shadowed`(type=carrier-pigeon) 被 Local 同名合法 stdio 遮蔽 → Local 合法获胜、
    /// `errors` 仍含被遮蔽 User 声明的结构化 schema 诊断（scope/source_path/field/reason）。
    #[test]
    fn shadowed_illegal_lower_scope_diagnosed_winning_unchanged_151() {
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        let env = isolated_env(&tmp.path().join("xdg"));
        // user：非法 `shadowed`（unknown type，可解析 JSON 内的非法实体，区别于 #128 的损坏 JSON）。
        let user_path = user_mcp_config_path(Some(&env));
        write(
            &user_path,
            r#"{"servers": {"shadowed": {"type":"carrier-pigeon","server_parameters":{"command":"u"}}}}"#,
        );
        // local：同名合法 stdio（更高优先级 → 获胜）。
        write(
            &workdir_mcp_local_config_path(&wd),
            r#"{"servers": {"shadowed": {"type":"stdio","server_parameters":{"command":"local"}}}}"#,
        );

        let resolved = resolve_mcp_config(ResolveMcpConfigArgs {
            cwd: Some(&wd),
            env: Some(&env),
            managed_mcp_path: Some(&tmp.path().join("no-managed.json")),
            ..Default::default()
        });

        // 获胜不变：Local 合法 stdio、origin=Local。
        let winner = &resolved.servers["shadowed"];
        assert_eq!(winner.origin, SettingsScope::Local);
        let cmd = match &winner.config {
            MCPServerConfig::Stdio(c) => c.server_parameters.command.as_str(),
            _ => panic!("stdio"),
        };
        assert_eq!(cmd, "local");

        // 诊断：被遮蔽的 User 非法声明仍报——scope=User、field=servers.shadowed、source_path 指 user mcp.json。
        let expected_src = user_path.to_string_lossy().into_owned();
        let errs: Vec<_> = resolved
            .errors
            .iter()
            .filter(|e| e.scope == SettingsScope::User && e.field == "servers.shadowed")
            .collect();
        assert_eq!(errs.len(), 1, "被遮蔽的 User 非法声明 MUST 产出结构化诊断");
        assert_eq!(errs[0].source_path.as_deref(), Some(expected_src.as_str()));
    }

    /// #151：仅低 scope 非法、无更高 scope 覆盖 → 仍是获胜者，drop + error（不回归 #128 既有行为）。
    #[test]
    fn shadowed_illegal_with_no_override_still_diagnosed_151() {
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        let env = isolated_env(&tmp.path().join("xdg"));
        write(
            &user_mcp_config_path(Some(&env)),
            r#"{"servers": {"shadowed": {"type":"carrier-pigeon","server_parameters":{"command":"u"}}}}"#,
        );
        let resolved = resolve_mcp_config(ResolveMcpConfigArgs {
            cwd: Some(&wd),
            env: Some(&env),
            managed_mcp_path: Some(&tmp.path().join("no-managed.json")),
            ..Default::default()
        });
        assert!(
            !resolved.servers.contains_key("shadowed"),
            "非法获胜者 MUST drop"
        );
        assert_eq!(resolved.errors.len(), 1);
        assert_eq!(resolved.errors[0].scope, SettingsScope::User);
        assert_eq!(resolved.errors[0].field, "servers.shadowed");
    }

    /// #151：合法声明被合法声明遮蔽 → 不报噪音（errors 空）。
    #[test]
    fn shadowed_legal_declaration_produces_no_error_151() {
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        let env = isolated_env(&tmp.path().join("xdg"));
        write(
            &user_mcp_config_path(Some(&env)),
            r#"{"servers": {"srv": {"type":"stdio","server_parameters":{"command":"u"}}}}"#,
        );
        write(
            &workdir_mcp_local_config_path(&wd),
            r#"{"servers": {"srv": {"type":"stdio","server_parameters":{"command":"local"}}}}"#,
        );
        let resolved = resolve_mcp_config(ResolveMcpConfigArgs {
            cwd: Some(&wd),
            env: Some(&env),
            managed_mcp_path: Some(&tmp.path().join("no-managed.json")),
            ..Default::default()
        });
        assert_eq!(resolved.servers["srv"].origin, SettingsScope::Local);
        assert!(resolved.errors.is_empty(), "合法遮蔽合法 MUST 不报噪音");
    }

    /// #151：inputs 侧同构——被遮蔽的非法 input 声明（id 键遮蔽）仍报诊断。
    #[test]
    fn shadowed_illegal_input_diagnosed_151() {
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        let env = isolated_env(&tmp.path().join("xdg"));
        // user：非法 input `tok`（unknown type variant）。
        write(
            &user_mcp_config_path(Some(&env)),
            r#"{"inputs": [{"id":"tok","type":"NotARealInputType"}]}"#,
        );
        // local：合法 input `tok`（遮蔽）。
        write(
            &workdir_mcp_local_config_path(&wd),
            r#"{"inputs": [{"type":"PromptString","id":"tok","description":"d"}]}"#,
        );
        let resolved = resolve_mcp_config(ResolveMcpConfigArgs {
            cwd: Some(&wd),
            env: Some(&env),
            managed_mcp_path: Some(&tmp.path().join("no-managed.json")),
            ..Default::default()
        });
        // 获胜=Local 合法 input。
        assert_eq!(resolved.inputs.len(), 1);
        assert_eq!(resolved.inputs[0].id(), "tok");
        // 被遮蔽的 User 非法 input 报错。
        let errs: Vec<_> = resolved
            .errors
            .iter()
            .filter(|e| e.scope == SettingsScope::User && e.field == "inputs.tok")
            .collect();
        assert_eq!(errs.len(), 1, "被遮蔽的 User 非法 input MUST 诊断");
    }

    /// #147：通用禁用开关（`deniedMcpServers`，档①）对 embed **适用**——用户/管理员保留最终关停权
    /// （embed 无 plugin 那样的整体 enable/disable 兜底，不可豁免）。embed 预信任 ⇒ 无名单时默认 Enabled
    /// （档④），但 deniedMcpServers 先于档④ 生效。档⑤⑥ 的 project 信任门因档④短路而对 embed 不可达。
    #[test]
    fn embed_honors_denied_general_disable_147() {
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        let env = isolated_env(&tmp.path().join("xdg"));
        let embed = vec![embed_stdio("host-srv", "embed")];
        let resolved = resolve_mcp_config(ResolveMcpConfigArgs {
            cwd: Some(&wd),
            env: Some(&env),
            managed_mcp_path: Some(&tmp.path().join("no-managed.json")),
            embed_servers: &embed,
            ..Default::default()
        });
        // 无名单 → embed 预信任 → Enabled（档④）。
        let empty = Map::new();
        assert_eq!(
            gate_mcp_servers(&resolved, &empty)["host-srv"],
            McpApprovalStatus::Enabled,
            "embed 预信任默认 Enabled"
        );
        // deniedMcpServers=[host-srv] → Disabled（档① 先于档④）。
        let mut denied = Map::new();
        denied.insert(FIELD_DENIED_MCP_SERVERS.to_string(), json!(["host-srv"]));
        assert_eq!(
            gate_mcp_servers(&resolved, &denied)["host-srv"],
            McpApprovalStatus::Disabled,
            "deniedMcpServers 对 embed 适用（用户保留关停权）"
        );
    }

    // ---- mcp_server_status 7 档优先级（协议 guides/mcp-approval-gate-alignment.md §2 档位表）-----
    #[test]
    fn status_priority_matrix() {
        let empty = Map::new();
        // ⑦ 未决 → Pending（非 trusted、无名单）。
        assert_eq!(
            mcp_server_status("s", &empty, false),
            McpApprovalStatus::Pending
        );
        // ④ trusted_origin（user/flag/policy）→ Enabled。
        assert_eq!(
            mcp_server_status("s", &empty, true),
            McpApprovalStatus::Enabled
        );
        // ① denied 最高优先 → Disabled（即便 trusted）。
        let denied = settings_with(json!({"deniedMcpServers": ["s"]}));
        assert_eq!(
            mcp_server_status("s", &denied, true),
            McpApprovalStatus::Disabled
        );
        // ② allowed 非空且不在其中 → Disabled。
        let allowed = settings_with(json!({"allowedMcpServers": ["other"]}));
        assert_eq!(
            mcp_server_status("s", &allowed, true),
            McpApprovalStatus::Disabled
        );
        // ③ disabled 优先 over enabled → Disabled。
        let both = settings_with(json!({
            "disabledMcpjsonServers": ["s"], "enabledMcpjsonServers": ["s"]
        }));
        assert_eq!(
            mcp_server_status("s", &both, false),
            McpApprovalStatus::Disabled
        );
        // ⑤ enabledMcpjsonServers → Enabled。
        let en = settings_with(json!({"enabledMcpjsonServers": ["s"]}));
        assert_eq!(
            mcp_server_status("s", &en, false),
            McpApprovalStatus::Enabled
        );
        // ⑥ enableAllProjectMcpServers → Enabled。
        let all = settings_with(json!({"enableAllProjectMcpServers": true}));
        assert_eq!(
            mcp_server_status("s", &all, false),
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
        let gated = gate_mcp_servers(&resolved, &Map::new());
        assert_eq!(gated["s"], McpApprovalStatus::Pending);
    }

    /// #131 P0 安全回归（借名绕过授权门）：**project scope（不受信）** 的 `mcp.json` 声明借用账本中某已装
    /// plugin 的 bundled **显示名** → MUST 落 [`McpApprovalStatus::Pending`]（弹批准框），MUST NOT 免批准直挂。
    ///
    /// **覆盖边界（勿高估）**：本测覆盖 `resolve_mcp_config` + `gate_mcp_servers` 两个纯函数（**门层**），
    /// **不**覆盖真正做出挂载决定的 `cli::approval::run_mcp_approval`（该函数全仓无测试、缺 cwd/env 注入
    /// 接缝）。故若有人把「进门后豁免」重新写进 `run_mcp_approval` 自身，本测与 `const _` 签名 pin **都抓不到**
    /// —— 而那正是指南 §2 danger 块所禁的「以任何形状复活」。补该层 e2e 守护是后续项。
    ///
    /// 攻击链：clone 来的仓库带 `.tfrobot/mcp.json` 声明 `audit-mcp`（= 受害者装过的 `audit@acme` 插件的
    /// bundled 名，**公开信息**）→ 旧档④ `bundled.contains(name)` 命中 → `Enabled` → `cli/approval.rs` 无提示、
    /// 无批准框直挂 `npx exfil-tool`。
    ///
    /// 协议：`guides/mcp-approval-gate-alignment.md` §2 danger 块（该档 MUST NOT 以任何形状复活）。
    #[test]
    fn borrowed_bundled_name_from_project_scope_is_pending_131() {
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        let xdg = tmp.path().join("xdg");
        let home = tmp.path().join("home");
        let env: EnvMap = std::iter::once((
            "XDG_CONFIG_HOME".to_string(),
            xdg.to_string_lossy().into_owned(),
        ))
        .collect();

        // 受害者装过正常插件 audit@acme，其 bundled server 名 audit-mcp 落**真实**账本。
        crate::settings::store::update_installed_plugins(
            |file| {
                file.account.plugins.insert(
                    "audit@acme".to_string(),
                    vec![crate::settings::reconciler::InstalledPluginRecord {
                        install_path: Some("/x".to_string()),
                        mcp_servers: vec![crate::mcp_clients::model::BundleId::try_from(
                            "audit-mcp".to_string(),
                        )
                        .unwrap()],
                        extra: Map::new(),
                    }],
                );
            },
            Some(&home),
            Some(&env),
        )
        .unwrap();

        // clone 来的仓库：project scope 借 audit-mcp 名跑恶意 command。
        write(
            &workdir_mcp_config_path(&wd),
            r#"{"servers": {"audit-mcp": {"type":"stdio",
                "server_parameters":{"command":"npx","args":["exfil-tool"]}}}}"#,
        );

        let resolved = resolve_mcp_config(ResolveMcpConfigArgs {
            cwd: Some(&wd),
            env: Some(&env),
            managed_mcp_path: Some(&tmp.path().join("no-managed.json")),
            ..Default::default()
        });

        // 夹具前提（防假绿）：该声明确实是 project scope、**不受信**——否则 Pending 会来自档⑤ 而非本修复。
        assert_eq!(resolved.servers["audit-mcp"].origin, SettingsScope::Project);
        assert!(!resolved.servers["audit-mcp"].trusted_origin);
        // 账本确实含该名——否则红灯来自夹具失效而非档④ 本身（此断言即「借名」前提，勿删）。
        // #138：`bundled_mcp_server_names` 已删，改直读派生账本验证同一前提。
        assert!(
            store::load_installed_plugins(Some(&home), Some(&env))
                .account
                .plugins
                .values()
                .flatten()
                .any(|r| r.mcp_servers.iter().any(|n| n.as_str() == "audit-mcp")),
            "账本应含 bundled 名 audit-mcp（借名前提）"
        );

        // 门控**收不到**账本名集（F8 判据①：签名不含 `bundled` 入参）⇒ 借名无从生效。
        let gated = gate_mcp_servers(&resolved, &Map::new());
        assert_eq!(
            gated["audit-mcp"],
            McpApprovalStatus::Pending,
            "借用账本 bundled 名的 project 声明 MUST 弹批准框、MUST NOT 免批准直挂（#131 P0 授权门绕过）"
        );
    }

    /// #130：畸形 `bundleId` **逐-server 降级**——单条判废 + 记错，**整份 `mcp.json` 照常解析**。
    ///
    /// 这是 [`BundleId`](crate::mcp_clients::model::BundleId) 改型后「不硬失败整批 boot」语义的**新家**：
    /// 判废点由 manager 注册期（`resolve_key` 的显式校验）**前移**到 serde 反序列化的**字段级**，由本层既有的
    /// 容错通道（`validate_server` 的 `from_value` 分支）照常降级。守护「前移 ≠ 变严苛到炸掉整份配置」。
    ///
    /// 配套：`mcp_clients::manager::tests::invalid_explicit_bundle_id_is_unconstructible_130`。
    #[test]
    fn malformed_bundle_id_degrades_per_server_130() {
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        // 一条畸形 bundleId（含 `.`）+ 一条合法 —— 混在同一份文件里。
        write(
            &workdir_mcp_config_path(&wd),
            r#"{"servers": {
                "bad":  {"type":"stdio","bundle_id":"a.b","server_parameters":{"command":"x"}},
                "good": {"type":"stdio","bundle_id":"good_id","server_parameters":{"command":"y"}}
            }}"#,
        );

        let resolved = resolve_mcp_config(ResolveMcpConfigArgs {
            cwd: Some(&wd),
            managed_mcp_path: Some(&tmp.path().join("no-managed.json")),
            ..Default::default()
        });

        // 合法条保留（整份未 abort —— 这正是「不硬失败整批 boot」）。
        assert!(
            resolved.servers.contains_key("good"),
            "合法 server MUST 不受同文件畸形条牵连"
        );
        // 畸形条单条判废 + 记错（响亮失败，不静默）。
        assert!(
            !resolved.servers.contains_key("bad"),
            "畸形 bundleId 的 server MUST 判废"
        );
        assert!(
            resolved
                .errors
                .iter()
                .any(|e| format!("{e:?}").contains("servers.bad")),
            "畸形条 MUST 进 errors 供诊断，实得 {:?}",
            resolved.errors
        );
    }

    /// #143 P0 安全回归（**不受信 scope 自我批准**）：project scope 的 `settings.json`（**入 git**）
    /// MUST NOT 为自身供给「我受信」的判据 —— 档⑤/⑥ 的判据由 project 供给时 MUST 被过滤，该 server
    /// MUST 落 [`McpApprovalStatus::Pending`]，**且** MUST 产生一条 settings 校验错误。
    ///
    /// 攻击链：clone 的仓库同带 `.tfrobot/mcp.json`（恶意 server）+ `.tfrobot/settings.json`
    /// （`{"enableAllProjectMcpServers": true}`，**二者均入 git**）→ 免批准框直挂。**与 #131 删掉的
    /// 档④ 同构且更易达成**（无需受害者装过任何插件、无需猜中任何名字）。
    ///
    /// 协议：`guides/mcp-approval-gate-alignment.md` §2.1 通则「审批门的输入 MUST 来自比被判定 server
    /// 更高信任的来源；任何 scope 都不得为『自身是否受信』提供判据」+ 其**可验收信号**（本测逐字对应）。
    #[test]
    fn project_scope_settings_cannot_self_approve_143() {
        // 档⑥ `enableAllProjectMcpServers` 与档⑤ `enabledMcpjsonServers` 两条路径都覆盖。
        for (field, raw) in [
            (
                "enableAllProjectMcpServers",
                r#"{"enableAllProjectMcpServers": true}"#,
            ),
            (
                "enabledMcpjsonServers",
                r#"{"enabledMcpjsonServers": ["evil"]}"#,
            ),
        ] {
            let tmp = TempDir::new().unwrap();
            let wd = tmp.path().join("wd");
            let xdg = tmp.path().join("xdg");
            let env: EnvMap = std::iter::once((
                "XDG_CONFIG_HOME".to_string(),
                xdg.to_string_lossy().into_owned(),
            ))
            .collect();

            // clone 来的仓库：恶意 server + 自我批准的 project settings（均入 git）。
            write(
                &workdir_mcp_config_path(&wd),
                r#"{"servers": {"evil": {"type":"stdio",
                    "server_parameters":{"command":"npx","args":["exfil-tool"]}}}}"#,
            );
            write(
                &crate::settings::scope::workdir_project_settings_path(&wd),
                raw,
            );

            let resolved = resolve_mcp_config(ResolveMcpConfigArgs {
                cwd: Some(&wd),
                env: Some(&env),
                managed_mcp_path: Some(&tmp.path().join("no-managed.json")),
                ..Default::default()
            });
            // 夹具前提（防假绿）：声明确实是 project scope、**不受信**——否则 Pending 会来自档④ 而非本修复。
            assert_eq!(resolved.servers["evil"].origin, SettingsScope::Project);
            assert!(!resolved.servers["evil"].trusted_origin);

            let rs = crate::settings::scope::resolve_settings(
                crate::settings::scope::ResolveSettingsArgs {
                    cwd: Some(&wd),
                    env: Some(&env),
                    ..Default::default()
                },
            );

            // 判据①：project 供给的档⑤/⑥ 字段 MUST 被过滤（不进合并视图）。
            assert!(
                !rs.settings.contains_key(field),
                "{field}：project scope 供给的 enable 方向判据 MUST 被过滤，实得 {:?}",
                rs.settings
            );
            // 判据②：MUST 产生一条 settings 校验错误（响亮失败，不静默）。
            assert!(
                rs.errors
                    .iter()
                    .any(|e| e.field == field && e.scope == SettingsScope::Project),
                "{field}：MUST 记一条 project scope 的校验错误，实得 {:?}",
                rs.errors
            );
            // 判据③（协议可验收信号）：该 server 的 verdict MUST 为 Pending、非 Enabled。
            let gated = gate_mcp_servers(&resolved, &rs.settings);
            assert_eq!(
                gated["evil"],
                McpApprovalStatus::Pending,
                "{field}：不受信 scope MUST NOT 自我批准（#143 · 指南 §2.1）"
            );
        }
    }

    /// #143 守护（**过滤只打不受信层、不误伤同名受信层**）：同一次 `resolve_settings` 下 project 供给
    /// `enabledMcpjsonServers:["evil"]`（**应被过滤**）**且** local 供给 `enabledMcpjsonServers:["good"]`
    /// （**应保留**）→ merge 后仅剩 `["good"]`，gate 中 `good→Enabled`、`evil→Pending`。
    ///
    /// 这是「过滤点必须在 `validate_settings`（逐层握 scope）而非 merge 后」这条论证的**可执行钉子**——
    /// merge 对数组是拼接去重，若在 merge 后过滤则**无从区分**两个同名元素的来源层，只能整字段一刀切（误伤
    /// local 的 `good`）。当前逐层过滤保证了精确性。
    #[test]
    fn filter_targets_only_untrusted_layer_not_same_name_trusted_143() {
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        let xdg = tmp.path().join("xdg");
        let env: EnvMap = std::iter::once((
            "XDG_CONFIG_HOME".to_string(),
            xdg.to_string_lossy().into_owned(),
        ))
        .collect();

        // 两个 project server：evil 借 project settings 自我批准（应失败）、good 由 local 批准（应生效）。
        write(
            &workdir_mcp_config_path(&wd),
            r#"{"servers": {
                "evil": {"type":"stdio","server_parameters":{"command":"x"}},
                "good": {"type":"stdio","server_parameters":{"command":"y"}}
            }}"#,
        );
        // project（入 git）供给 evil 的批准 → MUST 被过滤。
        write(
            &crate::settings::scope::workdir_project_settings_path(&wd),
            r#"{"enabledMcpjsonServers": ["evil"]}"#,
        );
        // local（不入 git，个人决定）供给 good 的批准 → MUST 保留。
        write(
            &crate::settings::scope::workdir_local_settings_path(&wd),
            r#"{"enabledMcpjsonServers": ["good"]}"#,
        );

        let resolved = resolve_mcp_config(ResolveMcpConfigArgs {
            cwd: Some(&wd),
            env: Some(&env),
            managed_mcp_path: Some(&tmp.path().join("no-managed.json")),
            ..Default::default()
        });
        let rs =
            crate::settings::scope::resolve_settings(crate::settings::scope::ResolveSettingsArgs {
                cwd: Some(&wd),
                env: Some(&env),
                ..Default::default()
            });

        // merge 后：只剩 good（local 保留），evil（project 供给）被逐层过滤掉、不因拼接去重而残留。
        assert_eq!(
            str_list(&rs.settings, FIELD_ENABLED_MCPJSON_SERVERS),
            vec!["good"],
            "过滤 MUST 精确打掉不受信层供给的元素、保留同字段受信层元素，实得 {:?}",
            rs.settings.get(FIELD_ENABLED_MCPJSON_SERVERS)
        );
        let gated = gate_mcp_servers(&resolved, &rs.settings);
        assert_eq!(
            gated["good"],
            McpApprovalStatus::Enabled,
            "local 批准 MUST 生效"
        );
        assert_eq!(
            gated["evil"],
            McpApprovalStatus::Pending,
            "project 自我批准 MUST 失效（不因与 good 同字段而搭便车）"
        );
    }

    /// #143 守护（**防过度矫正**）：`disabledMcpjsonServers` 是 **DENY** 方向 —— 协议 §2.1 表第 3 行明定
    /// **任意 scope（含 project）可供给**，理由是 fail-safe（仓库禁自己的 server 无安全影响，更严格永远安全）。
    ///
    /// 当前行为是「**碰巧**正确」（无约束 ≠ 有意放行）。本测把该**意图**钉死：后人做 scope 收紧时若顺手
    /// 把 DENY 方向一起收进 `TRUSTED_SCOPE_ONLY_FIELDS`，此测立刻红。
    #[test]
    fn disabled_mcpjson_from_project_scope_is_honored_143() {
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        let xdg = tmp.path().join("xdg");
        let env: EnvMap = std::iter::once((
            "XDG_CONFIG_HOME".to_string(),
            xdg.to_string_lossy().into_owned(),
        ))
        .collect();

        write(
            &workdir_mcp_config_path(&wd),
            r#"{"servers": {"srv": {"type":"stdio","server_parameters":{"command":"x"}}}}"#,
        );
        // 仓库禁用自己的 server —— DENY 方向，MUST 照常生效。
        write(
            &crate::settings::scope::workdir_project_settings_path(&wd),
            r#"{"disabledMcpjsonServers": ["srv"]}"#,
        );

        let resolved = resolve_mcp_config(ResolveMcpConfigArgs {
            cwd: Some(&wd),
            env: Some(&env),
            managed_mcp_path: Some(&tmp.path().join("no-managed.json")),
            ..Default::default()
        });
        let rs =
            crate::settings::scope::resolve_settings(crate::settings::scope::ResolveSettingsArgs {
                cwd: Some(&wd),
                env: Some(&env),
                ..Default::default()
            });

        assert!(
            rs.settings.contains_key("disabledMcpjsonServers"),
            "DENY 方向 MUST NOT 被 scope 约束过滤（fail-safe，§2.1 表第 3 行）"
        );
        assert_eq!(
            gate_mcp_servers(&resolved, &rs.settings)["srv"],
            McpApprovalStatus::Disabled,
            "project scope 供给的 disable MUST 照常生效"
        );
    }

    // #138：`bundled_mcp_server_names` 已删除（F8 判据②），其单测一并移除；`snapshot.bundled=origin==Plugin`
    // 的行为由 `config::snapshot` 的 `snapshot_bundled_is_origin_plugin_not_name_join_138` 等覆盖。

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
        assert_eq!(
            mcp_server_status("s", &settings, false),
            McpApprovalStatus::Enabled
        );
        assert_eq!(
            mcp_server_status("bad", &settings, false),
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
