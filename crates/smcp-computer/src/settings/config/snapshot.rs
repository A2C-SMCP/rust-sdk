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
*   - provenance = `ProvenanceScope`（= `SettingsScope` ∪ `{Plugin, Embed, Intent}`）；home 物化/意图资产 →
*     `Intent`（非手编 scope 文件，供 S2 写目标消解器判定不可当 user 写）。注：plugin 基线读侧投影**不**写此
*     provenance 映射（写目标输入只认可写声明面），见 `resolve_snapshot` 内投影处注释。
*   - `revision` = 规范化投影的确定性 sha256 摘要（内容变 → 摘要变）；capability revision 不在此结构，
*     属 S7 `ComputerStatusSnapshot`。
*
* S1 已知收敛（下游 S2/S5 补齐）/ known S1 convergences:
*   - per-entity origin 目前覆盖 4 类实体（Mcp / Marketplace / Plugin 意图 / PluginEnablement）。
*     input **定义**（`resolve_mcp_config` 未暴露 per-input origin）、settings 内联 `extraKnownMarketplaces`、
*     治理标量 `strict/trusted/blocked` 暂无 per-entity origin —— 留给 S5(inputs 订正) / S2(写目标) 补。
*/

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Serialize, Serializer};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::mcp_clients::bundle_id::resolve_bundle_id;
use crate::mcp_clients::model::{BundleId, MCPServerConfig, MCPServerInput};
use crate::skills::home::resolve_skill_home;

use super::super::mcp_config::{resolve_mcp_config, ResolveMcpConfigArgs};
use super::super::recovery::collect_enabled_bundled_servers;
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

/// 每实体 origin scope = `SettingsScope` ∪ `{Plugin, Embed, Intent}`。
///
/// `SettingsScope`（user/project/local/flag/policy）是**手编 config 文件 scope**；本枚举额外表达三类
/// 非文件 origin：
/// - `Plugin`：plugin 声明依赖的 bundled server（**最低基线**，其可信性由 install∧enable 门保证、**不走
///   settings 信任面**；协议 `runtime-contract.md §2.5`）。
/// - `Embed`：宿主构造挂载（`Computer::new(mcp_servers=…)`，代码级显式受信层，插在 local 与 flag 之间）。
///   本轮（#137）只落**优先序骨架 + 受信集扩位**；其运行期投影/接线归 **#147（S14）**。
/// - `Intent`：SKILL Home 物化/意图文件（`known_marketplaces.json` / `installed_plugins_intent.json`），
///   **非手编 scope**、仅 reconcile 写——让 S2 写目标消解器判定"不可当作 user config 就地改"。
///
/// per-entity origin scope = `SettingsScope` ∪ `{Plugin, Embed, Intent}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProvenanceScope {
    /// plugin 声明依赖的 bundled server（最低基线；install∧enable 门保证可信、不走 settings 信任面）。
    Plugin,
    /// user scope（XDG `a2c/`）。
    User,
    /// project scope（`<cwd>/.tfrobot/`）。
    Project,
    /// local scope（`<cwd>/.tfrobot/*.local.json`）。
    Local,
    /// 宿主构造挂载（`Computer::new(mcp_servers=…)`，代码级显式受信层；运行期接线归 #147）。
    Embed,
    /// CLI `--flag` 注入（只读）。
    Flag,
    /// 企业策略层（只读、最高）。
    Policy,
    /// SKILL Home 物化/意图文件（非手编 scope、仅 reconcile 写）/ home-materialized intent.
    Intent,
}

impl ProvenanceScope {
    /// MCP-config-scope 合并优先级（低→高，数值越大越高）/ merge priority for MCP-config scopes。
    ///
    /// 单一权威 = 协议 `runtime-contract.md §2.5 第3条`完整序：
    /// `plugin < user < project < local < embed < flag < policy`。settings.json 与 mcp.json 两套来源
    /// **MUST 同序**（本函数即该序的唯一 rust 落点）。
    ///
    /// `Intent` **非 MCP-config scope**（marketplace/plugin 账本实体的 origin，永不作 `McpServerView.origin`），
    /// 不参与 MCP server 合并；此处给它 `0` 仅为 `match` 完备，**不应**被当作可比较优先级使用。
    #[must_use]
    pub fn priority(self) -> u8 {
        match self {
            ProvenanceScope::Plugin => 0,
            ProvenanceScope::User => 1,
            ProvenanceScope::Project => 2,
            ProvenanceScope::Local => 3,
            ProvenanceScope::Embed => 4,
            ProvenanceScope::Flag => 5,
            ProvenanceScope::Policy => 6,
            // 非 MCP-config scope；见函数文档。
            ProvenanceScope::Intent => 0,
        }
    }

    /// origin 是否**预信任**（免批准门控直挂）/ pre-trusted origin (bypasses the approval gate)。
    ///
    /// 协议审批门指南档④ = `{user, flag, embed, policy}`：CLI 显式传入（flag）、宿主代码级挂载（embed）、
    /// 企业策略（policy）、user scope 自加 —— 均视调用方受信。`project`/`local` 受门控；`plugin` **不**在此列
    /// （其可信性由 install∧enable 门保证、不走 settings 信任面，且 MUST 不进审批门迭代，见 [`super::super::mcp_config::mcp_server_status`]）。
    #[must_use]
    pub fn is_trusted_origin(self) -> bool {
        matches!(
            self,
            ProvenanceScope::User
                | ProvenanceScope::Embed
                | ProvenanceScope::Flag
                | ProvenanceScope::Policy
        )
    }
}

impl From<SettingsScope> for ProvenanceScope {
    fn from(scope: SettingsScope) -> Self {
        match scope {
            SettingsScope::User => ProvenanceScope::User,
            SettingsScope::Project => ProvenanceScope::Project,
            SettingsScope::Local => ProvenanceScope::Local,
            SettingsScope::Embed => ProvenanceScope::Embed,
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
    /// origin ∈ {user, embed, flag, policy} → 免批准门控 / pre-trusted（见 [`ProvenanceScope::is_trusted_origin`]）。
    pub trusted_origin: bool,
    /// 该条目的权威 origin 是否为 plugin（`origin == Plugin`，#138 纯推导）/ is this entry plugin-origin。
    ///
    /// #138：语义由旧「name 命中已装插件 bundled 名集」（`bundled_mcp_server_names`，name-join、**不分启用态**、
    /// #126 假阳性）翻转为 `origin == ProvenanceScope::Plugin` —— 与 F1 `managedBy` 同式。文件 scope 声明
    /// （user/project/local/flag/policy，含撞名者）恒 `false`；唯**已启用 plugin 的读侧投影条目**为 `true`。
    /// 未启用 / 待 gc 的插件不投影 ⇒ 不再造成假阳性。
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
    /// `--mcp-config` flag 层 mcp.json 文件（**次高**，仅低于 policy；F6）/ flag-scope mcp config path。
    pub flag_mcp_config_path: Option<&'a Path>,
    /// policy `managed-mcp.json` 覆盖路径（缺省按平台推导）/ managed mcp path override。
    pub managed_mcp_path: Option<&'a Path>,
    /// 平台标识（缺省 `std::env::consts::OS`）/ platform override。
    pub platform: Option<&'a str>,
    /// policy scope settings 原始视图（first-source-wins 结果）/ raw policy settings。
    pub policy_settings: Option<&'a Map<String, Value>>,
    /// 宿主构造入参 `Computer::new(mcp_servers=…)` 的 **embed 层**（origin=embed，local<embed<flag；#147/S14）。
    /// 透传给 [`resolve_mcp_config`]；非-plugin 路径全投影，供**回收判据(#139)**（过滤 `origin != Plugin`）与
    /// **remove 守卫**（只读 embed → `ReadOnlyOrigin`）消费。
    pub embed_servers: &'a [MCPServerConfig],
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
        embed_servers,
    } = args;

    let mut provenance: BTreeMap<EntityKey, ProvenanceScope> = BTreeMap::new();

    // --- MCP + inputs（已带 per-server origin）/ MCP + input defs (per-server origin) ---
    let mcp_resolved = resolve_mcp_config(ResolveMcpConfigArgs {
        cwd,
        env,
        flag_config_path: flag_mcp_config_path,
        managed_mcp_path,
        platform,
        embed_servers,
    });
    let mut servers: Vec<McpServerView> = Vec::with_capacity(mcp_resolved.servers.len());
    // 文件 scope 已占用的 `bundle_id`（身份键，#117）——用于 plugin 投影去重（user > plugin）。
    let mut claimed_bundle_ids: HashSet<BundleId> = HashSet::new();
    for (name, server) in &mcp_resolved.servers {
        let origin: ProvenanceScope = server.origin.into();
        provenance.insert(EntityKey::Mcp(name.clone()), origin);
        claimed_bundle_ids.insert(resolve_bundle_id(&server.config));
        servers.push(McpServerView {
            name: name.clone(),
            origin,
            trusted_origin: server.trusted_origin,
            // #138：`origin == Plugin` 纯推导（文件 scope 声明 origin != Plugin ⇒ false）。旧 name-join
            // 账本（`bundled_mcp_server_names`，不分启用态）会把撞名的用户声明误标 true（#126 假阳性）——已删。
            bundled: origin == ProvenanceScope::Plugin,
            config: server.config.clone(),
        });
    }
    let inputs = InputDefsView {
        inputs: mcp_resolved.inputs.clone(),
    };

    // --- settings 五层（cleaned，high→low）→ merged + 逐键 provenance ---
    let layers = scoped_settings_layers(cwd, env, flag_settings_path, policy_settings);
    let low_to_high: Vec<Map<String, Value>> =
        layers.iter().rev().map(|(_, m)| m.clone()).collect();
    let merged = merge_layers(&low_to_high);

    // --- #137 A4：origin=plugin 读侧投影（协议 `runtime-contract.md` §2.5 第5条：运行期权威配置集 MUST
    //     携 origin；机制不钉 ⇒ 采 **scope 纯推导**，不落盘、每次从声明式输入重建）。已启用 plugin 的 bundled
    //     server 以最低基线 `origin=plugin` 追加；文件 scope 已声明同 `bundle_id` 者**吸收**之（user > plugin）。
    //     home 缺省 → 无账本 → 跳过。plugin 声明 **不进** [`resolve_mcp_config`]（审批门迭代集）——本投影是**独立
    //     的读侧权威集**，供 managedBy(F1) / `bundled`(#138) / 回收判据(#139) 消费，与审批门输入两条线（F8）。---
    if let Some(home_path) = home {
        for rec in collect_enabled_bundled_servers(home_path, env, &merged) {
            let bid = resolve_bundle_id(&rec.config);
            if !claimed_bundle_ids.insert(bid) {
                // 文件 scope（user/project/local/flag/policy）已占用同 bundle_id ⇒ user > plugin，吸收基线。
                continue;
            }
            let name = rec.config.name().to_string();
            // **不**写 `provenance` 映射：该映射是**写目标消解输入**（写侧只认可写声明面），plugin 基线 runtime-only
            // 不落盘、不可写（`writable_scope(Plugin)=None`）——若混入，`write_target`/`add_or_update_server` 会把
            // 「用户声明同 display 名（异 bundle_id）的新 server」误判 plugin-owned 而拒写（回归 #127/#131）。
            // 读侧权威集（bundled/#138 · managedBy/F1）经 `McpServerView.origin` 表达，与写侧 provenance 两条线。
            servers.push(McpServerView {
                name,
                origin: ProvenanceScope::Plugin,
                // plugin 非预信任（install∧enable 门保证、不走 settings 信任面；MUST 不进审批门迭代）。
                trusted_origin: false,
                // origin == Plugin ⇒ bundled=true（#138 全表 `origin == Plugin` 推导，此条恒真）。
                bundled: true,
                config: rec.config,
            });
        }
    }
    let mcp = McpConfigView { servers };

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

    /// #137 A1：`ProvenanceScope` 优先序骨架 = 协议 §2.5 第3条完整序（含 embed 扩位）。
    #[test]
    fn provenance_scope_priority_is_protocol_full_order_137() {
        use ProvenanceScope::*;
        // plugin < user < project < local < embed < flag < policy（严格递增）。
        let order = [Plugin, User, Project, Local, Embed, Flag, Policy];
        for pair in order.windows(2) {
            assert!(
                pair[0].priority() < pair[1].priority(),
                "{:?} 优先级须 < {:?}（协议 §2.5 完整序）",
                pair[0],
                pair[1]
            );
        }
        // embed 恰插在 local 与 flag 之间（Discussion #32）。
        assert!(Local.priority() < Embed.priority() && Embed.priority() < Flag.priority());
    }

    /// #137 A1：预信任 origin 集 = 审批门指南档④ `{user, embed, flag, policy}`（embed 受信集扩位）。
    #[test]
    fn provenance_scope_trusted_origin_set_137() {
        use ProvenanceScope::*;
        for s in [User, Embed, Flag, Policy] {
            assert!(s.is_trusted_origin(), "{s:?} 应预信任（档④）");
        }
        for s in [Plugin, Project, Local, Intent] {
            assert!(!s.is_trusted_origin(), "{s:?} 不应预信任");
        }
    }

    /// #137 A1：新变体 lowercase 线名（与 python StrEnum + 协议一致）。
    #[test]
    fn provenance_scope_serializes_plugin_and_embed_lowercase_137() {
        assert_eq!(
            serde_json::to_value(ProvenanceScope::Plugin).unwrap(),
            serde_json::json!("plugin")
        );
        assert_eq!(
            serde_json::to_value(ProvenanceScope::Embed).unwrap(),
            serde_json::json!("embed")
        );
    }

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

    /// 播种一个已装 plugin `audit@acme`（install_path + bundled server 文件 + 意图记录）/ seed an installed plugin。
    ///
    /// `load_bundled_servers` 读 `<install>/mcp-servers/<name>.json`（stem == config name），故据此写盘。
    fn seed_enabled_plugin(home: &Path, install: &Path, srv_name: &str, command: &str) {
        write(
            &install.join("mcp-servers").join(format!("{srv_name}.json")),
            &format!(
                r#"{{"type":"stdio","name":"{srv_name}","server_parameters":{{"command":"{command}"}}}}"#
            ),
        );
        update_installed_plugins(
            |file| {
                file.account.plugins.insert(
                    "audit@acme".to_string(),
                    vec![InstalledPluginRecord {
                        install_path: Some(install.to_string_lossy().into_owned()),
                        bundled_mcp_servers: vec![srv_name.to_string()],
                        extra: Map::new(),
                    }],
                );
            },
            Some(home),
            None,
        )
        .unwrap();
        update_installed_plugins_intent(
            |intent| {
                intent
                    .account
                    .installed_plugins
                    .insert("audit@acme".to_string());
            },
            Some(home),
            None,
        )
        .unwrap();
    }

    /// #137 A4：已启用 plugin 的 bundled server 进 snapshot、携 `origin=Plugin`（协议 §2.5 第5条运行期权威集
    /// 携 origin，scope 纯推导实现）。该 server 未在任何 mcp.json 声明 ⇒ 唯一来源是 plugin 投影。
    #[test]
    fn snapshot_projects_enabled_plugin_bundled_server_as_origin_plugin_137() {
        let tmp = TempDir::new().unwrap();
        let env = xdg_env(&tmp);
        let home = tmp.path().join("home");
        let wd = tmp.path().join("wd");
        let install = home.join("plugins").join("audit");
        seed_enabled_plugin(&home, &install, "plug-srv", "p");
        // enabledPlugins（settings 层）→ merged 消费（v0.3.0：absent 不默认启用）。
        write(
            &user_settings_path(Some(&env)),
            r#"{"enabledPlugins": {"audit@acme": true}}"#,
        );

        let snap = resolve_snapshot(SnapshotArgs {
            cwd: Some(&wd),
            env: Some(&env),
            home: Some(&home),
            managed_mcp_path: Some(&no_managed(&tmp)),
            ..Default::default()
        });

        let srv = snap
            .mcp
            .servers
            .iter()
            .find(|s| s.name == "plug-srv")
            .expect("已启用 plugin 的 bundled server 应进 snapshot（§2.5 权威集携 origin）");
        assert_eq!(
            srv.origin,
            ProvenanceScope::Plugin,
            "投影条目 origin=plugin"
        );
        assert!(
            srv.bundled,
            "plugin 投影条目 origin==Plugin ⇒ bundled=true（#138）"
        );
        assert!(
            !srv.trusted_origin,
            "plugin origin 非预信任（install∧enable 门保证、不走 settings 信任面）"
        );
        // plugin 基线**不进** `provenance` 写目标映射（写侧只认可写声明面；见投影处注释）——读侧 origin 走
        // `McpServerView.origin`，写侧 provenance 两条线。
        assert!(
            !snap
                .provenance
                .contains_key(&EntityKey::Mcp("plug-srv".to_string())),
            "plugin 基线 MUST NOT 进写目标 provenance 映射（否则 write_target 误拒同名用户声明）"
        );
    }

    /// #137 A4：文件 scope 声明同 `bundle_id` ⇒ **吸收** plugin 基线（`user > plugin`，协议 §2.5 优先序）。
    #[test]
    fn snapshot_user_declaration_subsumes_plugin_baseline_137() {
        let tmp = TempDir::new().unwrap();
        let env = xdg_env(&tmp);
        let home = tmp.path().join("home");
        let wd = tmp.path().join("wd");
        let install = home.join("plugins").join("audit");
        seed_enabled_plugin(&home, &install, "plug-srv", "plugincmd");
        write(
            &user_settings_path(Some(&env)),
            r#"{"enabledPlugins": {"audit@acme": true}}"#,
        );
        // user mcp.json 声明同名 server（⇒ 同 derived bundle_id）——MUST 吸收 plugin 基线、胜出。
        write(
            &user_mcp_config_path(Some(&env)),
            r#"{"servers": {"plug-srv": {"type":"stdio","server_parameters":{"command":"usercmd"}}}}"#,
        );

        let snap = resolve_snapshot(SnapshotArgs {
            cwd: Some(&wd),
            env: Some(&env),
            home: Some(&home),
            managed_mcp_path: Some(&no_managed(&tmp)),
            ..Default::default()
        });

        let matches: Vec<&McpServerView> = snap
            .mcp
            .servers
            .iter()
            .filter(|s| s.name == "plug-srv")
            .collect();
        assert_eq!(
            matches.len(),
            1,
            "user 声明与 plugin 同 bundle_id ⇒ 单条（plugin 基线被吸收），实得 {matches:?}"
        );
        assert_eq!(
            matches[0].origin,
            ProvenanceScope::User,
            "user > plugin 胜出"
        );
        assert_eq!(
            snap.provenance[&EntityKey::Mcp("plug-srv".to_string())],
            ProvenanceScope::User
        );
    }

    /// #138：`McpServerView.bundled` = `origin == Plugin` 纯推导，**非** name-join 账本（#126 假阳性根治）。
    ///
    /// 场景：用户 mcp.json 声明 `bundled-srv`，且账本里有一个**未启用**（无 intent / 无 `enabledPlugins`）的
    /// plugin 恰好 bundle 同名 `bundled-srv`。旧实现 `bundled_names.contains(name)`（不分启用态）会把用户自己的
    /// 声明误标 `bundled=true`（#126 假阳性）；新实现按 `origin`——该 server origin=User ⇒ `bundled=false`。
    #[test]
    fn snapshot_bundled_is_origin_plugin_not_name_join_138() {
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
        // 未启用（install_path=None、无 intent、无 enabledPlugins）的 plugin 恰 bundle 同名。
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
            Some(&env),
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
        assert_eq!(bundled.origin, ProvenanceScope::User, "该条是用户声明");
        assert!(
            !bundled.bundled,
            "撞未启用 plugin bundled 名的用户声明 MUST NOT 标 bundled（#126 假阳性根治）"
        );
        let plain = snap
            .mcp
            .servers
            .iter()
            .find(|s| s.name == "plain-srv")
            .unwrap();
        assert!(!plain.bundled, "独立 server 不标 bundled");
    }
}
