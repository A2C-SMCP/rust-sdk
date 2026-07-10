/*!
* 文件名: snapshot.rs
* 作者: JQQ
* 创建日期: 2026/07/10
* 最后修改日期: 2026/07/10
* 版权: 2023 JQQ. All rights reserved.
* 依赖: serde, serde_json, sha2, settings::{mcp_config, scope, schema, store}, skills::home
* 描述: #107 S1（#108）—— 统一 `ComputerConfig` 快照类型 + "多 scope reconcile 投影" 读入口。
*       快照携带 per-entity provenance（origin scope）与 content-derived `revision`，是 #107 全套的地基。
*       #108: unified `ComputerConfig` snapshot + multi-scope reconcile-projection read entry point.
*       Carries per-entity provenance (origin scope) + a content-derived `revision`.
*
* 边界 / Boundaries（对齐 #107 / design-107 §4）:
*   - 纯**新增读 API**：只组合既有各 family resolver（`resolve_mcp_config` / settings 层合并 /
*     home 账本），**不重写任何写路径**（main 保持可消费）。
*   - **不读 store / 不渲染 resolved 值**：快照只从 `mcp.json` / `settings*.json` / home 意图文件读入，
*     **绝不触碰 `value_store` / `secret_store`**、绝不解析 `${input:*}` / `${env:*}` 占位符；`inputs` 只含 **定义**（D1）。
*     ⚠️ 注意：这**不等于**「快照绝无明文」——用户若在 `mcp.json` 的 `env` 里硬编字面值、或 input `default` 写死明文，
*     这些 **config 内嵌明文**仍会进入序列化快照。故快照非「可安全落日志/导出」产物；S4 import/export 与日志路径须另行清洗。
*   - provenance = `ProvenanceScope`（= `SettingsScope` ∪ `Intent`）；home 物化/意图资产 → `Intent`
*     （非手编 scope 文件，供 S2 写目标消解器判定不可当 user 写）。
*   - `revision` = 规范化投影的确定性 sha256 摘要（内容变 → 摘要变）；capability revision 不在此结构，
*     属 S7 `ComputerStatusSnapshot`。
*
* S1 已知收敛（下游 S2/S5 补齐）/ known S1 convergences:
*   - per-entity origin 目前覆盖 4 类实体（Mcp / Marketplace / Plugin 意图 / PluginEnablement）。
*     input **定义**（`resolve_mcp_config` 未暴露 per-input origin）、settings 内联 `extraKnownMarketplaces`、
*     治理标量 `strict/trusted/blocked` 暂无 per-entity origin —— 留给 S5(inputs 订正) / S2(写目标) 补。
*/

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Serialize, Serializer};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::mcp_clients::model::{MCPServerConfig, MCPServerInput};
use crate::skills::home::resolve_skill_home;

use super::super::mcp_config::{
    bundled_mcp_server_names, resolve_mcp_config, ResolveMcpConfigArgs,
};
use super::super::schema::{validate_settings, SettingsScope};
use super::super::scope::{
    load_settings_file, merge_layers, resolve_cwd, user_settings_path, workdir_local_settings_path,
    workdir_project_settings_path, EnvMap,
};
use super::super::store::{load_installed_plugins_intent, load_known_marketplaces};

/// 快照 schema 版本（独立于协议版本）/ snapshot schema version (independent of PROTOCOL_VERSION)。
pub const SNAPSHOT_VERSION: u32 = 1;

// ===========================================================================
// Provenance / 来源
// ===========================================================================

/// 每实体 origin scope = `SettingsScope` ∪ `Intent`。
///
/// `SettingsScope`（user/project/local/flag/policy）无法表达 SKILL Home 物化/意图资产
/// （`known_marketplaces.json` / `installed_plugins_intent.json`）——它们**非手编 scope 文件**、
/// 只由 reconcile 写。故新增 `Intent`：让 S2 写目标消解器判定"不可当作 user config 就地改"。
/// per-entity origin scope = `SettingsScope` ∪ `Intent` (home-materialized, non-hand-editable).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProvenanceScope {
    /// user scope（XDG `a2c/`）。
    User,
    /// project scope（`<cwd>/.tfrobot/`）。
    Project,
    /// local scope（`<cwd>/.tfrobot/*.local.json`）。
    Local,
    /// CLI `--flag` 注入（只读）。
    Flag,
    /// 企业策略层（只读、最高）。
    Policy,
    /// SKILL Home 物化/意图文件（非手编 scope、仅 reconcile 写）/ home-materialized intent.
    Intent,
}

impl From<SettingsScope> for ProvenanceScope {
    fn from(scope: SettingsScope) -> Self {
        match scope {
            SettingsScope::User => ProvenanceScope::User,
            SettingsScope::Project => ProvenanceScope::Project,
            SettingsScope::Local => ProvenanceScope::Local,
            SettingsScope::Flag => ProvenanceScope::Flag,
            SettingsScope::Policy => ProvenanceScope::Policy,
        }
    }
}

/// provenance map 的实体键（跨 family 唯一标识一个可溯源实体）/ cross-family entity identity。
///
/// 序列化为字符串键（`"mcp:<name>"` 等），使 `BTreeMap<EntityKey, _>` 可直接落 JSON object。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum EntityKey {
    /// MCP server（key = server name）。
    Mcp(String),
    /// 已知 marketplace（key = marketplace name）。
    Marketplace(String),
    /// 安装意图记录（key = `<plugin>@<marketplace>` id）。
    Plugin(String),
    /// `enabledPlugins` 条目（key = `<plugin>@<marketplace>` id）。
    PluginEnablement(String),
}

impl EntityKey {
    /// 规范化字符串键（provenance JSON object 的键）/ canonical string key。
    fn as_key(&self) -> String {
        match self {
            EntityKey::Mcp(name) => format!("mcp:{name}"),
            EntityKey::Marketplace(name) => format!("marketplace:{name}"),
            EntityKey::Plugin(id) => format!("plugin:{id}"),
            EntityKey::PluginEnablement(id) => format!("pluginEnablement:{id}"),
        }
    }
}

impl std::fmt::Display for EntityKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.as_key())
    }
}

impl Serialize for EntityKey {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.as_key())
    }
}

/// config revision = 规范化投影的 sha256 摘要（`"sha256:<hex>"`）/ content-derived digest。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConfigRevision(pub String);

// ===========================================================================
// 字段族视图 / Field-family views
// ===========================================================================

/// 单 MCP server 投影（合并后，带 origin）/ one merged MCP server (with origin)。
#[derive(Debug, Clone, Serialize)]
pub struct McpServerView {
    /// server 身份（= map key）/ server identity。
    pub name: String,
    /// 最高定义 scope / highest-defining scope。
    pub origin: ProvenanceScope,
    /// origin ∈ {user, flag, policy} → 免批准门控 / pre-trusted。
    pub trusted_origin: bool,
    /// plugin-bundled server（属主 plugin enablement 管、**MUST NOT 走 project 信任门**，§5.10）/ owned by a plugin.
    ///
    /// 写目标消解（S2）asset-class-aware 判定的地基：bundled server 无独立可编辑文件 → `Synthesized`。
    /// 派生自安装账本（`bundled_mcp_server_names`）。
    pub bundled: bool,
    /// 校验后的 A2C 配置（**含占位符、未渲染**——不解析 `${input:*}`/`${env:*}`、不读 store）/ placeholders unrendered.
    ///
    /// ⚠️ 仍可能携带用户在 `mcp.json` 中硬编的 `env` 字面值（config 内嵌明文）；见模块头「不等于快照绝无明文」。
    pub config: MCPServerConfig,
}

/// 多 scope 合并后的 MCP 视图 / multi-scope merged MCP view。
#[derive(Debug, Clone, Default, Serialize)]
pub struct McpConfigView {
    /// 按 name 保序的 server 列表 / ordered servers。
    pub servers: Vec<McpServerView>,
}

/// input **定义** 视图（D1：无明文值 / secret，值走 `RuntimeOptions` resolver）/ input DEFS only.
#[derive(Debug, Clone, Default, Serialize)]
pub struct InputDefsView {
    /// 去重后的 input 定义 / deduped input definitions。
    pub inputs: Vec<MCPServerInput>,
}

/// skill 配置视图 / skill config view。
#[derive(Debug, Clone, Serialize)]
pub struct SkillConfigView {
    /// SKILL Home（skill 发现/物化根）/ resolved skill home。
    pub skill_home: PathBuf,
}

/// 单已知 marketplace 投影 / one known marketplace。
#[derive(Debug, Clone, Serialize)]
pub struct MarketplaceView {
    /// marketplace 名 / marketplace name。
    pub name: String,
    /// 记录的 git 源（`{type,url}`）/ recorded git source。
    pub source: Value,
    /// 来源恒为 `Intent`（home 账本）/ always `Intent`。
    pub origin: ProvenanceScope,
}

/// marketplace 治理视图（账本 + settings 层策略）/ marketplace governance view。
#[derive(Debug, Clone, Default, Serialize)]
pub struct MarketplaceGovView {
    /// home 账本已知 marketplaces（origin=Intent）/ known marketplaces from home ledger。
    pub known: Vec<MarketplaceView>,
    /// `strictKnownMarketplaces`（settings 层）/ strict mode。
    pub strict: Option<bool>,
    /// `trustedMarketplaces`（settings 层）/ trusted names。
    pub trusted: Vec<String>,
    /// `blockedMarketplaces`（settings 层）/ blocked names。
    pub blocked: Vec<String>,
    /// `extraKnownMarketplaces`（settings 层内联声明）/ inline extra marketplaces。
    pub extra_known: BTreeMap<String, Value>,
}

/// 单安装意图记录 / one install-intent record。
#[derive(Debug, Clone, Serialize)]
pub struct PluginRecordView {
    /// `<plugin>@<marketplace>` id。
    pub id: String,
    /// 来源恒为 `Intent`（installedPlugins 意图，权威）/ always `Intent`。
    pub origin: ProvenanceScope,
}

/// 单 `enabledPlugins` 条目投影 / one plugin-enablement entry。
#[derive(Debug, Clone, Serialize)]
pub struct PluginEnablementView {
    /// `<plugin>@<marketplace>` id。
    pub id: String,
    /// 合并后的启用态 / merged enabled state。
    pub enabled: bool,
    /// 决定该条目的最高 settings scope / winning settings scope。
    pub origin: ProvenanceScope,
}

/// plugin 配置视图（安装意图 + 启用态）/ plugin config view。
#[derive(Debug, Clone, Default, Serialize)]
pub struct PluginConfigView {
    /// installedPlugins 意图（权威；账本降派生，见 #102/#104）/ authoritative install intent。
    pub installed: Vec<PluginRecordView>,
    /// enabledPlugins per-scope 合并 / merged per-scope enablement。
    pub enabled: Vec<PluginEnablementView>,
}

/// runtime 默认值视图 / runtime defaults view。
///
/// 现 settings schema 无专用 runtime 字段（timeout / cache / capability-revision 策略）；此处为
/// 前向占位（passthrough 未识别 runtime 键），待后续子任务填充具体 schema。
#[derive(Debug, Clone, Default, Serialize)]
pub struct RuntimeDefaults {
    /// passthrough 占位（现无专用字段）/ passthrough placeholder。
    pub extra: Map<String, Value>,
}

// ===========================================================================
// 快照 / Snapshot
// ===========================================================================

/// 统一 `ComputerConfig` 快照 = 带 provenance 的多 scope reconcile 投影 / the unified snapshot。
#[derive(Debug, Clone, Serialize)]
pub struct ComputerConfigSnapshot {
    /// 快照 schema 版本 / snapshot schema version。
    pub version: u32,
    /// 内容摘要 revision（内容变 → 摘要变）/ content-derived revision。
    pub revision: ConfigRevision,
    /// MCP servers（合并 + origin）/ merged MCP servers。
    pub mcp: McpConfigView,
    /// input 定义（D1：仅定义）/ input definitions only。
    pub inputs: InputDefsView,
    /// skill 配置 / skill config。
    pub skills: SkillConfigView,
    /// marketplace 治理 / marketplace governance。
    pub marketplace: MarketplaceGovView,
    /// plugin 意图 + 启用态 / plugin intent + enablement。
    pub plugins: PluginConfigView,
    /// runtime 默认值 / runtime defaults。
    pub runtime: RuntimeDefaults,
    /// 每实体 origin scope（写目标消解输入）/ per-entity origin (write-target input)。
    pub provenance: BTreeMap<EntityKey, ProvenanceScope>,
}

/// [`resolve_snapshot`] 入参（镜像各 resolver 的注入接缝）/ inputs (mirrors resolver seams)。
#[derive(Default)]
pub struct SnapshotArgs<'a> {
    /// project/local 锚定工作目录；`None` → 进程 cwd / project/local anchor。
    pub cwd: Option<&'a Path>,
    /// 环境映射（解析 user config dir / skill home）；`None` → 进程环境 / env map。
    pub env: Option<&'a EnvMap>,
    /// SKILL Home 覆盖（marketplace/plugin 意图文件根 + skill home）；`None` → env 解析 / home override。
    pub home: Option<&'a Path>,
    /// `--settings <file>` 注入 / the `--settings` flag file。
    pub flag_settings_path: Option<&'a Path>,
    /// `--config @file` 老接口 mcp 文件（最低优先级）/ legacy mcp flag config path。
    pub flag_mcp_config_path: Option<&'a Path>,
    /// policy `managed-mcp.json` 覆盖路径（缺省按平台推导）/ managed mcp path override。
    pub managed_mcp_path: Option<&'a Path>,
    /// 平台标识（缺省 `std::env::consts::OS`）/ platform override。
    pub platform: Option<&'a str>,
    /// policy scope settings 原始视图（first-source-wins 结果）/ raw policy settings。
    pub policy_settings: Option<&'a Map<String, Value>>,
}

/// 解析统一 `ComputerConfig` 快照 = 多 scope reconcile 投影（读，无写）/ resolve the unified snapshot.
///
/// 组合既有 resolver：`resolve_mcp_config`（mcp + inputs，已带 per-server origin）、settings 五层合并
/// （skills/marketplace/plugin 治理）、home 账本（marketplaces + installedPlugins 意图）。全程只读，
/// 绝不触碰 `value_store`/`secret_store`（→ 无 secret / input 明文）。
#[must_use]
pub fn resolve_snapshot(args: SnapshotArgs<'_>) -> ComputerConfigSnapshot {
    let SnapshotArgs {
        cwd,
        env,
        home,
        flag_settings_path,
        flag_mcp_config_path,
        managed_mcp_path,
        platform,
        policy_settings,
    } = args;

    let mut provenance: BTreeMap<EntityKey, ProvenanceScope> = BTreeMap::new();

    // --- MCP + inputs（已带 per-server origin）/ MCP + input defs (per-server origin) ---
    let mcp_resolved = resolve_mcp_config(ResolveMcpConfigArgs {
        cwd,
        env,
        flag_config_path: flag_mcp_config_path,
        managed_mcp_path,
        platform,
    });
    let bundled_names = bundled_mcp_server_names(home, env);
    let mut servers: Vec<McpServerView> = Vec::with_capacity(mcp_resolved.servers.len());
    for (name, server) in &mcp_resolved.servers {
        let origin: ProvenanceScope = server.origin.into();
        provenance.insert(EntityKey::Mcp(name.clone()), origin);
        servers.push(McpServerView {
            name: name.clone(),
            origin,
            trusted_origin: server.trusted_origin,
            bundled: bundled_names.contains(name),
            config: server.config.clone(),
        });
    }
    let mcp = McpConfigView { servers };
    let inputs = InputDefsView {
        inputs: mcp_resolved.inputs.clone(),
    };

    // --- settings 五层（cleaned，high→low）→ merged + 逐键 provenance ---
    let layers = scoped_settings_layers(cwd, env, flag_settings_path, policy_settings);
    let low_to_high: Vec<Map<String, Value>> =
        layers.iter().rev().map(|(_, m)| m.clone()).collect();
    let merged = merge_layers(&low_to_high);

    // --- marketplace 治理 / marketplace governance ---
    let mk_ledger = load_known_marketplaces(home, env);
    let mut known: Vec<MarketplaceView> = Vec::with_capacity(mk_ledger.account.marketplaces.len());
    for (name, entry) in &mk_ledger.account.marketplaces {
        provenance.insert(
            EntityKey::Marketplace(name.clone()),
            ProvenanceScope::Intent,
        );
        known.push(MarketplaceView {
            name: name.clone(),
            source: entry.source.clone(),
            origin: ProvenanceScope::Intent,
        });
    }
    let marketplace = MarketplaceGovView {
        known,
        strict: get_bool(&merged, "strictKnownMarketplaces"),
        trusted: get_str_vec(&merged, "trustedMarketplaces"),
        blocked: get_str_vec(&merged, "blockedMarketplaces"),
        extra_known: get_object(&merged, "extraKnownMarketplaces"),
    };

    // --- plugin 意图（权威）+ enabledPlugins（合并 + 逐键 origin）/ plugin intent + enablement ---
    let intent = load_installed_plugins_intent(home, env);
    let mut installed: Vec<PluginRecordView> =
        Vec::with_capacity(intent.account.installed_plugins.len());
    for id in &intent.account.installed_plugins {
        provenance.insert(EntityKey::Plugin(id.clone()), ProvenanceScope::Intent);
        installed.push(PluginRecordView {
            id: id.clone(),
            origin: ProvenanceScope::Intent,
        });
    }
    let mut enabled: Vec<PluginEnablementView> = Vec::new();
    for (id, value) in get_object(&merged, "enabledPlugins") {
        let flag = value.as_bool().unwrap_or(false);
        let origin = enabled_plugin_scope(&layers, &id, &value);
        provenance.insert(EntityKey::PluginEnablement(id.clone()), origin);
        enabled.push(PluginEnablementView {
            id,
            enabled: flag,
            origin,
        });
    }
    let plugins = PluginConfigView { installed, enabled };

    // --- skills / runtime ---
    let skills = SkillConfigView {
        skill_home: home
            .map(Path::to_path_buf)
            .unwrap_or_else(|| resolve_skill_home(env)),
    };
    let runtime = RuntimeDefaults::default();

    // --- revision = 规范化投影的确定性摘要 / content-derived digest ---
    let revision = compute_revision(
        SNAPSHOT_VERSION,
        &mcp,
        &inputs,
        &skills,
        &marketplace,
        &plugins,
        &runtime,
        &provenance,
    );

    ComputerConfigSnapshot {
        version: SNAPSHOT_VERSION,
        revision,
        mcp,
        inputs,
        skills,
        marketplace,
        plugins,
        runtime,
        provenance,
    }
}

// ===========================================================================
// 内部辅助 / Internal helpers
// ===========================================================================

/// 加载五层 settings（cleaned，**high → low** 优先级）供逐键 provenance 判定 / scoped layers, high→low。
///
/// 与 `resolve_settings` 同源（`user < project < local < flag < policy`），但保留每层 scope 标签
/// （合并会丢失）。各层经 `load_settings_file` / `validate_settings` 清洗，故层内出现即该 scope 合法定义。
fn scoped_settings_layers(
    cwd: Option<&Path>,
    env: Option<&EnvMap>,
    flag_settings_path: Option<&Path>,
    policy_settings: Option<&Map<String, Value>>,
) -> Vec<(ProvenanceScope, Map<String, Value>)> {
    let mut layers: Vec<(ProvenanceScope, Map<String, Value>)> = Vec::new();

    // policy（最高）— 按 policy scope 校验，镜像 resolve_settings。
    if let Some(raw) = policy_settings {
        let (clean, _) =
            validate_settings(&Value::Object(raw.clone()), SettingsScope::Policy, None);
        layers.push((ProvenanceScope::Policy, clean));
    }
    // flag。
    if let Some(path) = flag_settings_path {
        let (clean, _) = load_settings_file(path, SettingsScope::Flag);
        layers.push((ProvenanceScope::Flag, clean));
    }
    // local + project（锚定 cwd；cwd 不可读 → 两层缺席）。
    if let Some(base) = resolve_cwd(cwd) {
        let (local, _) =
            load_settings_file(&workdir_local_settings_path(&base), SettingsScope::Local);
        layers.push((ProvenanceScope::Local, local));
        let (project, _) = load_settings_file(
            &workdir_project_settings_path(&base),
            SettingsScope::Project,
        );
        layers.push((ProvenanceScope::Project, project));
    }
    // user（最低）。
    let (user, _) = load_settings_file(&user_settings_path(env), SettingsScope::User);
    layers.push((ProvenanceScope::User, user));

    layers
}

/// 逐键 `enabledPlugins` 的胜出 scope：high→low 首个"值等于合并值"的层；退化取首个含该键的层。
fn enabled_plugin_scope(
    layers: &[(ProvenanceScope, Map<String, Value>)],
    id: &str,
    merged_value: &Value,
) -> ProvenanceScope {
    for (scope, layer) in layers {
        if let Some(entry) = layer
            .get("enabledPlugins")
            .and_then(Value::as_object)
            .and_then(|ep| ep.get(id))
        {
            if entry == merged_value {
                return *scope;
            }
        }
    }
    // 退化（跨层深合并的边角）：取首个（最高）含该键的层。
    for (scope, layer) in layers {
        let has = layer
            .get("enabledPlugins")
            .and_then(Value::as_object)
            .is_some_and(|ep| ep.contains_key(id));
        if has {
            return *scope;
        }
    }
    ProvenanceScope::User
}

/// 规范化投影 → sha256 摘要。经 `serde_json::to_value` 把所有 map 键规范排序（BTreeMap），
/// 消除 `HashMap`（如 `tool_meta`）迭代序带来的非确定性；数组保插入序（构造即确定）。
#[allow(clippy::too_many_arguments)]
fn compute_revision(
    version: u32,
    mcp: &McpConfigView,
    inputs: &InputDefsView,
    skills: &SkillConfigView,
    marketplace: &MarketplaceGovView,
    plugins: &PluginConfigView,
    runtime: &RuntimeDefaults,
    provenance: &BTreeMap<EntityKey, ProvenanceScope>,
) -> ConfigRevision {
    let mut canonical = Map::new();
    canonical.insert("version".to_string(), Value::from(version));
    canonical.insert("mcp".to_string(), to_canonical(mcp));
    canonical.insert("inputs".to_string(), to_canonical(inputs));
    canonical.insert("skills".to_string(), to_canonical(skills));
    canonical.insert("marketplace".to_string(), to_canonical(marketplace));
    canonical.insert("plugins".to_string(), to_canonical(plugins));
    canonical.insert("runtime".to_string(), to_canonical(runtime));
    canonical.insert("provenance".to_string(), to_canonical(provenance));

    let bytes = serde_json::to_vec(&Value::Object(canonical))
        .expect("snapshot projection is serializable to canonical JSON");
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let hex: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    ConfigRevision(format!("sha256:{hex}"))
}

/// `serde_json::to_value` 包装：把任意 map 键规范排序（→ 确定性摘要）/ canonicalize map key order。
fn to_canonical<T: Serialize>(value: &T) -> Value {
    serde_json::to_value(value).expect("snapshot field is serializable")
}

fn get_bool(map: &Map<String, Value>, key: &str) -> Option<bool> {
    map.get(key).and_then(Value::as_bool)
}

fn get_str_vec(map: &Map<String, Value>, key: &str) -> Vec<String> {
    map.get(key)
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn get_object(map: &Map<String, Value>, key: &str) -> BTreeMap<String, Value> {
    map.get(key)
        .and_then(Value::as_object)
        .map(|obj| {
            obj.iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect::<BTreeMap<String, Value>>()
        })
        .unwrap_or_default()
}

// ===========================================================================
// 测试 / Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::reconciler::{InstalledPluginRecord, KnownMarketplaceEntry};
    use crate::settings::store::{
        empty_known_marketplaces, save_known_marketplaces, update_installed_plugins,
        update_installed_plugins_intent,
    };
    use serde_json::json;
    use tempfile::TempDir;

    /// 写文件（自建父目录）/ write, creating parents。
    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    /// user scope 锚到 `<tmp>/xdg`（隔离真实系统）/ pin user scope under tmp。
    fn xdg_env(tmp: &TempDir) -> EnvMap {
        std::iter::once((
            "XDG_CONFIG_HOME".to_string(),
            tmp.path().join("xdg").to_string_lossy().into_owned(),
        ))
        .collect()
    }

    /// 不存在的 managed 路径，隔离真实 managed-mcp.json / isolate real policy mcp。
    fn no_managed(tmp: &TempDir) -> PathBuf {
        tmp.path().join("no-managed.json")
    }

    use super::super::super::mcp_config::{user_mcp_config_path, workdir_mcp_config_path};
    use super::super::super::scope::{user_settings_path, workdir_local_settings_path};

    #[test]
    fn snapshot_projects_multiscope_mcp_with_origin_provenance() {
        // user srv-u（origin=User、trusted）；project srv-p（origin=Project、须门控）。
        let tmp = TempDir::new().unwrap();
        let env = xdg_env(&tmp);
        let home = tmp.path().join("home");
        let wd = tmp.path().join("wd");
        write(
            &user_mcp_config_path(Some(&env)),
            r#"{"servers": {"srv-u": {"type":"stdio","server_parameters":{"command":"u"}}}}"#,
        );
        write(
            &workdir_mcp_config_path(&wd),
            r#"{"servers": {"srv-p": {"type":"stdio","server_parameters":{"command":"p"}}}}"#,
        );

        let snap = resolve_snapshot(SnapshotArgs {
            cwd: Some(&wd),
            env: Some(&env),
            home: Some(&home),
            managed_mcp_path: Some(&no_managed(&tmp)),
            ..Default::default()
        });

        let names: Vec<&str> = snap.mcp.servers.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"srv-u"));
        assert!(names.contains(&"srv-p"));
        let srv_p = snap.mcp.servers.iter().find(|s| s.name == "srv-p").unwrap();
        assert_eq!(srv_p.origin, ProvenanceScope::Project);
        assert!(!srv_p.trusted_origin);
        assert_eq!(
            snap.provenance[&EntityKey::Mcp("srv-u".to_string())],
            ProvenanceScope::User
        );
        assert_eq!(
            snap.provenance[&EntityKey::Mcp("srv-p".to_string())],
            ProvenanceScope::Project
        );
    }

    #[test]
    fn snapshot_projects_enabled_plugins_with_winning_scope() {
        // user a@mp=true；local a@mp=false + b@mp=true → merged: a=false(Local 胜)、b=true(Local)。
        let tmp = TempDir::new().unwrap();
        let env = xdg_env(&tmp);
        let home = tmp.path().join("home");
        let wd = tmp.path().join("wd");
        write(
            &user_settings_path(Some(&env)),
            r#"{"enabledPlugins": {"a@mp": true}}"#,
        );
        write(
            &workdir_local_settings_path(&wd),
            r#"{"enabledPlugins": {"a@mp": false, "b@mp": true}}"#,
        );

        let snap = resolve_snapshot(SnapshotArgs {
            cwd: Some(&wd),
            env: Some(&env),
            home: Some(&home),
            managed_mcp_path: Some(&no_managed(&tmp)),
            ..Default::default()
        });

        let a = snap
            .plugins
            .enabled
            .iter()
            .find(|e| e.id == "a@mp")
            .unwrap();
        assert!(!a.enabled, "local a@mp=false 应覆盖 user true");
        assert_eq!(a.origin, ProvenanceScope::Local);
        let b = snap
            .plugins
            .enabled
            .iter()
            .find(|e| e.id == "b@mp")
            .unwrap();
        assert!(b.enabled);
        assert_eq!(b.origin, ProvenanceScope::Local);
        assert_eq!(
            snap.provenance[&EntityKey::PluginEnablement("a@mp".to_string())],
            ProvenanceScope::Local
        );
    }

    #[test]
    fn snapshot_marks_home_ledger_assets_as_intent() {
        // home 账本 marketplace + 安装意图 → origin=Intent（非手编 scope）。
        let tmp = TempDir::new().unwrap();
        let env = xdg_env(&tmp);
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();

        let mut mk = empty_known_marketplaces();
        mk.account.marketplaces.insert(
            "local".to_string(),
            KnownMarketplaceEntry {
                source: json!({"type": "git", "url": "https://example.com/mp.git"}),
                extra: Map::new(),
            },
        );
        save_known_marketplaces(&mk, Some(&home), None).unwrap();
        update_installed_plugins_intent(
            |intent| {
                intent
                    .account
                    .installed_plugins
                    .insert("plug@local".to_string());
            },
            Some(&home),
            None,
        )
        .unwrap();

        let snap = resolve_snapshot(SnapshotArgs {
            cwd: Some(&tmp.path().join("wd")),
            env: Some(&env),
            home: Some(&home),
            managed_mcp_path: Some(&no_managed(&tmp)),
            ..Default::default()
        });

        assert_eq!(snap.marketplace.known.len(), 1);
        assert_eq!(snap.marketplace.known[0].name, "local");
        assert_eq!(snap.marketplace.known[0].origin, ProvenanceScope::Intent);
        assert_eq!(
            snap.provenance[&EntityKey::Marketplace("local".to_string())],
            ProvenanceScope::Intent
        );
        assert_eq!(snap.plugins.installed.len(), 1);
        assert_eq!(snap.plugins.installed[0].id, "plug@local");
        assert_eq!(
            snap.provenance[&EntityKey::Plugin("plug@local".to_string())],
            ProvenanceScope::Intent
        );
    }

    #[test]
    fn revision_is_deterministic_across_hashmap_and_content_sensitive() {
        // 守护「to_value 规范化 HashMap 键序」：多键 env（HashMap<String,String>）两次独立 resolve →
        // revision 必相等（若规范化失效，两 HashMap 实例迭代序不同 → revision 漂移）。
        let tmp = TempDir::new().unwrap();
        let env = xdg_env(&tmp);
        let home = tmp.path().join("home");
        let wd = tmp.path().join("wd");
        write(
            &workdir_mcp_config_path(&wd),
            r#"{"servers": {"srv": {"type":"stdio","server_parameters":{
                "command":"a","env":{"ZED":"1","ALPHA":"2","MIKE":"3","BRAVO":"4"}
            }}}}"#,
        );
        let mk = |cmd_dir: &TempDir| -> ConfigRevision {
            resolve_snapshot(SnapshotArgs {
                cwd: Some(&wd),
                env: Some(&env),
                home: Some(&home),
                managed_mcp_path: Some(&no_managed(cmd_dir)),
                ..Default::default()
            })
            .revision
        };
        let r1 = mk(&tmp);
        let r2 = mk(&tmp);
        assert_eq!(
            r1, r2,
            "多键 HashMap env 下同输入 revision 必须稳定（规范化键序）"
        );
        assert!(r1.0.starts_with("sha256:"));

        // 改 env 内容 → revision 变。
        write(
            &workdir_mcp_config_path(&wd),
            r#"{"servers": {"srv": {"type":"stdio","server_parameters":{
                "command":"a","env":{"ZED":"CHANGED","ALPHA":"2","MIKE":"3","BRAVO":"4"}
            }}}}"#,
        );
        let r3 = mk(&tmp);
        assert_ne!(r1, r3, "内容变 revision 必须变");
    }

    #[test]
    fn snapshot_never_injects_store_values_or_renders_placeholders() {
        // 非欺骗性守护验收 #3：即便磁盘上残留旧明文（legacy input-values.json），快照也绝不读它、绝不渲染 ${input:tok}。
        // 判定标准：若实现回归为读盘并渲染，`SUPER_SECRET_XYZ` 会出现在 TOKEN 里 → 断言失败。
        // 注：明文 value store 已于 #112 S5 硬退役，此处直接投放 legacy 文件模拟旧版本残留。
        let tmp = TempDir::new().unwrap();
        let mut env = xdg_env(&tmp);
        env.insert(
            "XDG_STATE_HOME".to_string(),
            tmp.path().join("state").to_string_lossy().into_owned(),
        );
        let home = tmp.path().join("home");
        let wd = tmp.path().join("wd");
        write(
            &workdir_mcp_config_path(&wd),
            r#"{
                "servers": {"s": {"type":"stdio","server_parameters":{"command":"c","env":{"TOKEN":"${input:tok}"}}}},
                "inputs": [{"type":"PromptString","id":"tok","description":"a token","password":true}]
            }"#,
        );
        // 直接在 legacy XDG state 路径投放明文残留——快照必须不带出它（回归读盘即会命中）。
        write(
            &tmp.path()
                .join("state")
                .join("a2c")
                .join("input-values.json"),
            r#"{"tok":"SUPER_SECRET_XYZ"}"#,
        );

        let snap = resolve_snapshot(SnapshotArgs {
            cwd: Some(&wd),
            env: Some(&env),
            home: Some(&home),
            managed_mcp_path: Some(&no_managed(&tmp)),
            ..Default::default()
        });

        assert_eq!(snap.inputs.inputs.len(), 1, "input 定义在场（仅定义）");
        let serialized = serde_json::to_string(&snap).unwrap();
        assert!(
            serialized.contains("${input:tok}"),
            "占位引用应原样保留、未被渲染"
        );
        assert!(
            !serialized.contains("SUPER_SECRET_XYZ"),
            "store 明文绝不得进入快照（回归读 store 即触发此断言）"
        );
    }

    #[test]
    fn snapshot_projects_policy_scope_with_origin() {
        // policy（最高、只读）：managed mcp server → origin=Policy+预信任；policy settings enabledPlugins → origin=Policy。
        let tmp = TempDir::new().unwrap();
        let env = xdg_env(&tmp);
        let home = tmp.path().join("home");
        let wd = tmp.path().join("wd");
        let managed = tmp.path().join("managed-mcp.json");
        write(
            &managed,
            r#"{"servers": {"srv-pol": {"type":"stdio","server_parameters":{"command":"pol"}}}}"#,
        );
        let policy = json!({"enabledPlugins": {"p@mp": true}});
        let policy_obj = policy.as_object().unwrap().clone();

        let snap = resolve_snapshot(SnapshotArgs {
            cwd: Some(&wd),
            env: Some(&env),
            home: Some(&home),
            managed_mcp_path: Some(&managed),
            policy_settings: Some(&policy_obj),
            ..Default::default()
        });

        let srv = snap
            .mcp
            .servers
            .iter()
            .find(|s| s.name == "srv-pol")
            .unwrap();
        assert_eq!(srv.origin, ProvenanceScope::Policy);
        assert!(srv.trusted_origin, "policy origin 预信任、免门控");
        assert_eq!(
            snap.provenance[&EntityKey::Mcp("srv-pol".to_string())],
            ProvenanceScope::Policy
        );
        let p = snap
            .plugins
            .enabled
            .iter()
            .find(|e| e.id == "p@mp")
            .unwrap();
        assert!(p.enabled);
        assert_eq!(p.origin, ProvenanceScope::Policy);
        assert_eq!(
            snap.provenance[&EntityKey::PluginEnablement("p@mp".to_string())],
            ProvenanceScope::Policy
        );
    }

    #[test]
    fn snapshot_projects_flag_scope_with_origin() {
        // flag（--settings，高于 local/project、低于 policy）：enabledPlugins 覆盖 user → origin=Flag。
        let tmp = TempDir::new().unwrap();
        let env = xdg_env(&tmp);
        let home = tmp.path().join("home");
        let wd = tmp.path().join("wd");
        write(
            &user_settings_path(Some(&env)),
            r#"{"enabledPlugins": {"f@mp": false}}"#,
        );
        let flag_settings = tmp.path().join("flag-settings.json");
        write(&flag_settings, r#"{"enabledPlugins": {"f@mp": true}}"#);

        let snap = resolve_snapshot(SnapshotArgs {
            cwd: Some(&wd),
            env: Some(&env),
            home: Some(&home),
            flag_settings_path: Some(&flag_settings),
            managed_mcp_path: Some(&no_managed(&tmp)),
            ..Default::default()
        });

        let f = snap
            .plugins
            .enabled
            .iter()
            .find(|e| e.id == "f@mp")
            .unwrap();
        assert!(f.enabled, "flag f@mp=true 覆盖 user false");
        assert_eq!(f.origin, ProvenanceScope::Flag);
        assert_eq!(
            snap.provenance[&EntityKey::PluginEnablement("f@mp".to_string())],
            ProvenanceScope::Flag
        );
    }

    #[test]
    fn snapshot_marks_plugin_bundled_mcp_server() {
        // 账本记录 bundledMcpServers=["bundled-srv"]，同名 mcp server → McpServerView.bundled=true（S2 地基）。
        let tmp = TempDir::new().unwrap();
        let env = xdg_env(&tmp);
        let home = tmp.path().join("home");
        let wd = tmp.path().join("wd");
        write(
            &user_mcp_config_path(Some(&env)),
            r#"{"servers": {
                "bundled-srv": {"type":"stdio","server_parameters":{"command":"b"}},
                "plain-srv": {"type":"stdio","server_parameters":{"command":"p"}}
            }}"#,
        );
        update_installed_plugins(
            |file| {
                file.account.plugins.insert(
                    "plug@mp".to_string(),
                    vec![InstalledPluginRecord {
                        install_path: None,
                        bundled_mcp_servers: vec!["bundled-srv".to_string()],
                        extra: Map::new(),
                    }],
                );
            },
            Some(&home),
            None,
        )
        .unwrap();

        let snap = resolve_snapshot(SnapshotArgs {
            cwd: Some(&wd),
            env: Some(&env),
            home: Some(&home),
            managed_mcp_path: Some(&no_managed(&tmp)),
            ..Default::default()
        });

        let bundled = snap
            .mcp
            .servers
            .iter()
            .find(|s| s.name == "bundled-srv")
            .unwrap();
        assert!(bundled.bundled, "账本 bundled server 应标记 bundled=true");
        let plain = snap
            .mcp
            .servers
            .iter()
            .find(|s| s.name == "plain-srv")
            .unwrap();
        assert!(!plain.bundled, "独立 server 不标 bundled");
    }
}
