/*!
* 文件名: write_target.rs
* 作者: JQQ
* 创建日期: 2026/07/10
* 最后修改日期: 2026/07/10
* 版权: 2023 JQQ. All rights reserved.
* 依赖: serde_json, settings::{scope::WriteValue, schema, mcp_config, config::snapshot}
* 描述: #107 S2（#109）—— runtime 对 config 增删改的**写目标决策纯函数**。
*       由 origin + scope 代码逻辑（不靠 per-entity 元数据）确定落哪个文件/哪个 scope。
*       #109: pure write-target resolver — decides which file/scope a runtime config edit lands in.
*
* 核心语义 / core semantics（design-107 §5 / §6）:
*   - **disable ≠ remove**：`Remove` 动**声明**（删 mcp.json 里的 server key）；`Disable` 动**override**
*     （写 `disabledMcpjsonServers` / `enabledPlugins[id]=false` 到固定 writable scope，**不碰声明**、天然可逆）。
*   - **对称纯函数**：无 I/O；输入 = 实体 + 意图 + S1 快照（**provenance**，定 origin）+ scope 锚点；输出 = `WritePlan` 计划。
*   - **asset-class-aware**（§6）：独立 MCP → `disabledMcpjsonServers`（§9.2 ③ 跨 scope override）；
*     plugin → `enabledPlugins[id]`。⚠️ #126：本层**对插件归属无感知**——bundled server 的配置来自插件安装目录、
*     runtime-only 挂载、**从不落 `mcp.json`**（#122），故凡进 config 快照的 server 必有可编辑声明文件，`Synthesized`
*     不再在此按 `bundled` 名冲突产出。"插件占用同名"的归属门控上移到 **Computer 层**
*     （`add_or_update_server`/`remove_server`，复用 `managedBy` 查询同源的 enabled-bundled 归属集），与 config-file 层解耦。
*   - **Remove 策略**（design §12 R1 已拍板）：删**所有可写 scope**的声明（真删干净）。因 S1 快照只暴露
*     **胜出 origin**、不带 per-scope 存在性，无法预判哪些 scope 真声明了该实体，故对三个可写 scope **盲发** Delete。
*     origin ∈ {policy, flag} → 结构化错 `ReadOnlyOrigin`（#109 验收）。
*
* ⚠️ **执行器契约（S3 已兑现，见 `executor.rs`）/ Executor contract (honored by `executor.rs`)**：
*   - `apply_write`（`scope.rs`）对**缺失父键**的 `WriteValue::Object{..Delete}` 会**物化空对象**（如
*     `{"servers":{}}`），**不是**干净 noop。故落盘前须判 no-change 跳过写——但**字节级** `updated == existing`
*     **不够**（`{"servers":{}}` ≠ `{}` 仍会误写）；正确规则是**语义比对**：剥离两侧的纯空对象脚手架后相等则跳过
*     （`executor::apply_value_op` 实现）。否则本函数 Remove 的 fan-out 会在**从未声明该实体**的 scope 凭空建空
*     `{"servers":{}}` 文件。
*   - `StringSetInsert/Remove`：S3 读-改-写目标 scope 的该数组——insert **去重**、对**缺失字段** insert 则**新建数组**、
*     对缺失成员/缺失字段 remove 为 **noop**。这两个 op 是本函数对「复用 `WriteValue`」的必要偏离（`WriteValue::Set`
*     整体替换数组、无法表达成员增删，且 S1 未投影 `disabledMcpjsonServers` 现值故纯函数无法就地构造新数组）。
*
* 消费 S1（[`ComputerConfigSnapshot`]）：**只读 `provenance`（定 origin）**——#126 起本层不再读 `mcp.servers[].bundled`
* （插件归属门控上移 Computer 层，见上）。WriteScope 只含 {User, Project, Local}（可写子集）；Flag/Policy/Intent 只读。
*/

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::Value;

use super::super::mcp_config::{
    user_mcp_config_path, workdir_mcp_config_path, workdir_mcp_local_config_path,
};
use super::super::schema::{SettingsScope, FIELD_DISABLED_MCPJSON_SERVERS, FIELD_ENABLED_PLUGINS};
use super::super::scope::{
    user_settings_path, workdir_local_settings_path, workdir_project_settings_path, EnvMap,
    WriteValue,
};
use super::snapshot::{ComputerConfigSnapshot, EntityKey, ProvenanceScope};

// ===========================================================================
// 输入类型 / Inputs
// ===========================================================================

/// 可写 scope 子集（Flag/Policy/Intent 只读，不在此）/ writable scope subset。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteScope {
    /// user scope（XDG `a2c/`）。
    User,
    /// project scope（`<cwd>/.tfrobot/`）。
    Project,
    /// local scope（`<cwd>/.tfrobot/*.local.json`）。
    Local,
}

impl From<WriteScope> for SettingsScope {
    fn from(scope: WriteScope) -> Self {
        match scope {
            WriteScope::User => SettingsScope::User,
            WriteScope::Project => SettingsScope::Project,
            WriteScope::Local => SettingsScope::Local,
        }
    }
}

/// 待落盘的实体（本函数解析 scope-file 写目标；install/uninstall 归 installer）/ target entity。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigEntity {
    /// MCP server（key = server name；#126 起本层不判 bundled，插件归属门控在 Computer 层）。
    McpServer(String),
    /// plugin（key = `<plugin>@<marketplace>` id；仅 enable/disable，install/uninstall 归 installer）。
    Plugin(String),
}

/// 编辑意图 / edit intent。
#[derive(Debug, Clone, PartialEq)]
pub enum EditIntent {
    /// 声明或就地改（value = 实体定义 JSON）/ declare or edit in place。
    Upsert(Value),
    /// 让它不再被声明（删声明）/ remove the declaration。
    Remove,
    /// 我这层盖掉它（override，不碰声明）/ mask via override。
    Disable,
    /// 撤销压制 / lift the override。
    Enable,
}

/// 写目标消解选项 / resolver options。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteTargetOptions {
    /// `Upsert` 新实体的落点 scope（design §5.3 默认 project）/ scope for newly-declared entities。
    pub upsert_new_scope: WriteScope,
    /// `Disable`/`Enable` override 的固定 scope（design §5.3 默认 local）/ fixed disable-scope。
    pub disable_scope: WriteScope,
}

impl Default for WriteTargetOptions {
    fn default() -> Self {
        Self {
            upsert_new_scope: WriteScope::Project,
            disable_scope: WriteScope::Local,
        }
    }
}

/// scope 锚点（env-resolved，纯函数据此拼路径、无 I/O）/ env-resolved scope anchors。
#[derive(Debug, Clone)]
pub struct ScopeAnchors {
    /// project/local 锚定工作目录（`<workdir>/.tfrobot/*`）/ project/local anchor。
    pub workdir: PathBuf,
    /// 环境映射（解析 user config dir）/ env map (resolves user config dir)。
    pub env: EnvMap,
}

impl ScopeAnchors {
    /// 构造锚点 / construct anchors。
    pub fn new(workdir: impl Into<PathBuf>, env: EnvMap) -> Self {
        Self {
            workdir: workdir.into(),
            env,
        }
    }

    /// 该 scope 的 `mcp.json` / `mcp.local.json` 路径 / mcp config path for a scope。
    fn mcp_path(&self, scope: WriteScope) -> PathBuf {
        match scope {
            WriteScope::User => user_mcp_config_path(Some(&self.env)),
            WriteScope::Project => workdir_mcp_config_path(&self.workdir),
            WriteScope::Local => workdir_mcp_local_config_path(&self.workdir),
        }
    }

    /// 该 scope 的 `settings.json` / `settings.local.json` 路径 / settings path for a scope。
    fn settings_path(&self, scope: WriteScope) -> PathBuf {
        match scope {
            WriteScope::User => user_settings_path(Some(&self.env)),
            WriteScope::Project => workdir_project_settings_path(&self.workdir),
            WriteScope::Local => workdir_local_settings_path(&self.workdir),
        }
    }
}

// ===========================================================================
// 输出类型 / Outputs
// ===========================================================================

/// 单文件写操作 / one file's write operation。
///
/// `WriteValue`（复用 `scope.rs`）表达 scalar/object/删键；但**信任门数组**（`disabledMcpjsonServers`）的
/// 成员增删无法用 `WriteValue`（其 `Set` 整体替换数组）表达，故补两个集合成员 op —— executor 读-改-写该数组。
#[derive(Debug, Clone, PartialEq)]
pub enum WriteTargetOp {
    /// 直接写值（scalar/object/删键）/ direct value write。
    Value(WriteValue),
    /// 向字符串数组字段插入成员（去重）/ insert into a string-array field。
    StringSetInsert {
        /// 字段名（如 `disabledMcpjsonServers`）。
        field: String,
        /// 成员值（server name）。
        value: String,
    },
    /// 从字符串数组字段移除成员 / remove from a string-array field。
    StringSetRemove {
        /// 字段名。
        field: String,
        /// 成员值。
        value: String,
    },
}

/// 单条写计划（落哪个文件 / 哪个 scope / 做什么）/ one write plan。
#[derive(Debug, Clone, PartialEq)]
pub struct WritePlan {
    /// 目标 scope / target scope。
    pub scope: SettingsScope,
    /// 目标文件绝对路径 / target file path。
    pub file: PathBuf,
    /// 写操作 / the write op。
    pub op: WriteTargetOp,
}

/// 写目标消解错误（结构化，不靠日志）/ structured write-target errors。
#[derive(Debug, Clone, PartialEq)]
pub enum WriteTargetError {
    /// remove/edit 命中只读 origin（policy/flag/intent）/ hit a read-only origin。
    ReadOnlyOrigin {
        /// 实体键（`mcp:<name>` 等）。
        entity: String,
        /// 只读 origin scope。
        origin: ProvenanceScope,
    },
    /// plugin-bundled server：无独立可编辑文件，须操作属主 plugin / synthesized (owned by a plugin)。
    Synthesized {
        /// 实体键。
        entity: String,
    },
    /// 本函数不负责的 (实体, 意图) 组合（如 plugin install/uninstall 归 installer）/ unsupported combo。
    Unsupported {
        /// 实体键。
        entity: String,
        /// 原因。
        reason: String,
    },
}

impl std::fmt::Display for WriteTargetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteTargetError::ReadOnlyOrigin { entity, origin } => {
                write!(f, "read-only origin {origin:?} for entity {entity}")
            }
            WriteTargetError::Synthesized { entity } => {
                write!(
                    f,
                    "synthesized (plugin-bundled) entity {entity} has no editable file"
                )
            }
            WriteTargetError::Unsupported { entity, reason } => {
                write!(f, "unsupported write target for {entity}: {reason}")
            }
        }
    }
}

impl std::error::Error for WriteTargetError {}

// ===========================================================================
// 消解器 / Resolver
// ===========================================================================

/// 消解一次 config 编辑的写目标（纯函数）/ resolve the write target(s) for one config edit.
///
/// 返回 `Vec<WritePlan>`：多数意图单条；`Remove` 按已拍板策略产出**所有可写 scope**的删键计划。
pub fn resolve_write_target(
    entity: &ConfigEntity,
    intent: &EditIntent,
    snapshot: &ComputerConfigSnapshot,
    anchors: &ScopeAnchors,
    opts: &WriteTargetOptions,
) -> Result<Vec<WritePlan>, WriteTargetError> {
    match entity {
        ConfigEntity::McpServer(name) => resolve_mcp_server(name, intent, snapshot, anchors, opts),
        ConfigEntity::Plugin(id) => resolve_plugin(id, intent, anchors, opts),
    }
}

fn resolve_mcp_server(
    name: &str,
    intent: &EditIntent,
    snapshot: &ComputerConfigSnapshot,
    anchors: &ScopeAnchors,
    opts: &WriteTargetOptions,
) -> Result<Vec<WritePlan>, WriteTargetError> {
    let entity_key = format!("mcp:{name}");

    // #126：本层**只认 config 声明**、对插件归属无感知。凡出现在 config 快照里的 server 必有可编辑声明
    // 文件——bundled server 的配置来自插件安装目录、runtime-only 挂载、**从不落 `mcp.json`**（#122），故绝
    // 无法在此被 provenance 命中。据此按 origin 决策即可：writable→编辑该 scope、flag/policy→`ReadOnlyOrigin`、
    // 无声明→新建/幂等。**"插件占用同名"的归属门控（`Synthesized`）改由 Computer 层强制**
    // （`add_or_update_server`/`remove_server`，复用 `managedBy` 查询同源的 enabled-bundled 归属集），不再靠
    // `mcp.servers[].bundled` 名冲突在此误拦用户自己的真实声明。
    let origin = snapshot
        .provenance
        .get(&EntityKey::Mcp(name.to_string()))
        .copied();

    match intent {
        // Upsert：改已有 → origin scope（只读则错）；新实体 → 默认 project（可 opts 覆盖）。
        EditIntent::Upsert(value) => {
            let scope = match origin {
                Some(o) => writable_scope(o).ok_or(WriteTargetError::ReadOnlyOrigin {
                    entity: entity_key,
                    origin: o,
                })?,
                None => opts.upsert_new_scope,
            };
            Ok(vec![WritePlan {
                scope: scope.into(),
                file: anchors.mcp_path(scope),
                op: WriteTargetOp::Value(mcp_server_write(name, WriteValue::Set(value.clone()))),
            }])
        }
        // Remove：动声明。只读 origin → 结构化错；不存在 → 幂等空；否则删所有可写 scope（真删干净）。
        EditIntent::Remove => {
            match origin {
                Some(o) if writable_scope(o).is_none() => {
                    return Err(WriteTargetError::ReadOnlyOrigin {
                        entity: entity_key,
                        origin: o,
                    });
                }
                None => return Ok(Vec::new()),
                Some(_) => {}
            }
            Ok([WriteScope::User, WriteScope::Project, WriteScope::Local]
                .into_iter()
                .map(|scope| WritePlan {
                    scope: scope.into(),
                    file: anchors.mcp_path(scope),
                    op: WriteTargetOp::Value(mcp_server_write(name, WriteValue::Delete)),
                })
                .collect())
        }
        // Disable：override 到固定 disable-scope 的 disabledMcpjsonServers（不碰声明、跨 scope 生效、可逆）。
        EditIntent::Disable => Ok(vec![WritePlan {
            scope: opts.disable_scope.into(),
            file: anchors.settings_path(opts.disable_scope),
            op: WriteTargetOp::StringSetInsert {
                field: FIELD_DISABLED_MCPJSON_SERVERS.to_string(),
                value: name.to_string(),
            },
        }]),
        // Enable：撤销 disable-scope 上的 override。
        EditIntent::Enable => Ok(vec![WritePlan {
            scope: opts.disable_scope.into(),
            file: anchors.settings_path(opts.disable_scope),
            op: WriteTargetOp::StringSetRemove {
                field: FIELD_DISABLED_MCPJSON_SERVERS.to_string(),
                value: name.to_string(),
            },
        }]),
    }
}

fn resolve_plugin(
    id: &str,
    intent: &EditIntent,
    anchors: &ScopeAnchors,
    opts: &WriteTargetOptions,
) -> Result<Vec<WritePlan>, WriteTargetError> {
    match intent {
        EditIntent::Disable => Ok(vec![plugin_enablement_plan(id, false, anchors, opts)]),
        EditIntent::Enable => Ok(vec![plugin_enablement_plan(id, true, anchors, opts)]),
        // install/uninstall 是 home 账本 + 物化流程，归 installer（install_plugin/uninstall_plugin），非本函数。
        EditIntent::Upsert(_) | EditIntent::Remove => Err(WriteTargetError::Unsupported {
            entity: format!("plugin:{id}"),
            reason:
                "plugin install/uninstall goes through installer, not the write-target resolver"
                    .to_string(),
        }),
    }
}

// ===========================================================================
// 内部辅助 / Helpers
// ===========================================================================

/// 只读判定：User/Project/Local → 对应 `WriteScope`；其余 → `None`（不可写）。
///
/// 不可写包含：Flag/Policy（只读 scope）、Intent（reconcile 写、非手编）、Plugin（bundled server runtime-only、
/// 不落 mcp.json，走 installer）、Embed（宿主构造入参、非持久文件；#137 骨架，运行期接线归 #147）。
fn writable_scope(origin: ProvenanceScope) -> Option<WriteScope> {
    match origin {
        ProvenanceScope::User => Some(WriteScope::User),
        ProvenanceScope::Project => Some(WriteScope::Project),
        ProvenanceScope::Local => Some(WriteScope::Local),
        ProvenanceScope::Plugin
        | ProvenanceScope::Embed
        | ProvenanceScope::Flag
        | ProvenanceScope::Policy
        | ProvenanceScope::Intent => None,
    }
}

/// 单键 `WriteValue::Object`（`{key: leaf}`）/ single-key object write。
fn one(key: &str, leaf: WriteValue) -> WriteValue {
    let mut map = BTreeMap::new();
    map.insert(key.to_string(), leaf);
    WriteValue::Object(map)
}

/// `mcp.json` 里定位单 server 的嵌套写（`{servers: {name: leaf}}`）/ nested write to one server。
fn mcp_server_write(name: &str, leaf: WriteValue) -> WriteValue {
    one("servers", one(name, leaf))
}

/// `enabledPlugins[id] = enabled` 的 settings 写计划（disable-scope）/ plugin enablement plan。
fn plugin_enablement_plan(
    id: &str,
    enabled: bool,
    anchors: &ScopeAnchors,
    opts: &WriteTargetOptions,
) -> WritePlan {
    WritePlan {
        scope: opts.disable_scope.into(),
        file: anchors.settings_path(opts.disable_scope),
        op: WriteTargetOp::Value(one(
            FIELD_ENABLED_PLUGINS,
            one(id, WriteValue::Set(Value::Bool(enabled))),
        )),
    }
}

// ===========================================================================
// 测试 / Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::super::snapshot::{
        resolve_snapshot, ComputerConfigSnapshot, ConfigRevision, InputDefsView,
        MarketplaceGovView, McpConfigView, PluginConfigView, RuntimeDefaults, SkillConfigView,
        SnapshotArgs, SNAPSHOT_VERSION,
    };
    use super::*;
    use crate::settings::reconciler::InstalledPluginRecord;
    use crate::settings::store::update_installed_plugins;
    use serde_json::{json, Map};
    use std::path::Path;
    use tempfile::TempDir;

    /// 直接构造仅带 provenance 的最小快照（纯函数单测用，避开文件铺陈）/ minimal snapshot for unit tests。
    fn snapshot_with_provenance(
        provenance: BTreeMap<EntityKey, ProvenanceScope>,
    ) -> ComputerConfigSnapshot {
        ComputerConfigSnapshot {
            version: SNAPSHOT_VERSION,
            revision: ConfigRevision("test".into()),
            mcp: McpConfigView::default(),
            inputs: InputDefsView::default(),
            skills: SkillConfigView {
                skill_home: PathBuf::from("/nonexistent"),
            },
            marketplace: MarketplaceGovView::default(),
            plugins: PluginConfigView::default(),
            runtime: RuntimeDefaults::default(),
            provenance,
        }
    }

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }
    fn xdg_env(tmp: &TempDir) -> EnvMap {
        std::iter::once((
            "XDG_CONFIG_HOME".to_string(),
            tmp.path().join("xdg").to_string_lossy().into_owned(),
        ))
        .collect()
    }
    fn no_managed(tmp: &TempDir) -> PathBuf {
        tmp.path().join("no-managed.json")
    }

    /// 搭一个 fixture：返回 (snapshot, anchors)，各 test 注入自己的文件。
    struct Fixture {
        _tmp: TempDir,
        env: EnvMap,
        wd: PathBuf,
        home: PathBuf,
        managed: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let tmp = TempDir::new().unwrap();
            let env = xdg_env(&tmp);
            let wd = tmp.path().join("wd");
            let home = tmp.path().join("home");
            let managed = no_managed(&tmp);
            Self {
                _tmp: tmp,
                env,
                wd,
                home,
                managed,
            }
        }
        fn snapshot(&self) -> ComputerConfigSnapshot {
            resolve_snapshot(SnapshotArgs {
                cwd: Some(&self.wd),
                env: Some(&self.env),
                home: Some(&self.home),
                managed_mcp_path: Some(&self.managed),
                ..Default::default()
            })
        }
        fn anchors(&self) -> ScopeAnchors {
            ScopeAnchors::new(&self.wd, self.env.clone())
        }
    }

    const OPTS: WriteTargetOptions = WriteTargetOptions {
        upsert_new_scope: WriteScope::Project,
        disable_scope: WriteScope::Local,
    };

    #[test]
    fn mcp_upsert_new_declares_at_project() {
        let fx = Fixture::new();
        let snap = fx.snapshot(); // 空，srv 不存在 → 新声明
        let plans = resolve_write_target(
            &ConfigEntity::McpServer("srv".into()),
            &EditIntent::Upsert(json!({"type": "stdio"})),
            &snap,
            &fx.anchors(),
            &OPTS,
        )
        .unwrap();
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].scope, SettingsScope::Project);
        assert_eq!(plans[0].file, workdir_mcp_config_path(&fx.wd));
    }

    #[test]
    fn mcp_upsert_edit_at_writable_origin() {
        let fx = Fixture::new();
        write(
            &user_mcp_config_path(Some(&fx.env)),
            r#"{"servers": {"srv": {"type":"stdio","server_parameters":{"command":"u"}}}}"#,
        );
        let snap = fx.snapshot(); // srv origin=User（可写）
        let plans = resolve_write_target(
            &ConfigEntity::McpServer("srv".into()),
            &EditIntent::Upsert(json!({"type": "stdio"})),
            &snap,
            &fx.anchors(),
            &OPTS,
        )
        .unwrap();
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].scope, SettingsScope::User, "改已有落 origin scope");
        assert_eq!(plans[0].file, user_mcp_config_path(Some(&fx.env)));
    }

    #[test]
    fn mcp_remove_readonly_policy_origin_errors() {
        let fx = Fixture::new();
        let managed = fx._tmp.path().join("managed-mcp.json");
        write(
            &managed,
            r#"{"servers": {"srv-pol": {"type":"stdio","server_parameters":{"command":"p"}}}}"#,
        );
        // 用真实 managed 路径 → srv-pol origin=Policy。
        let snap = resolve_snapshot(SnapshotArgs {
            cwd: Some(&fx.wd),
            env: Some(&fx.env),
            home: Some(&fx.home),
            managed_mcp_path: Some(&managed),
            ..Default::default()
        });
        let err = resolve_write_target(
            &ConfigEntity::McpServer("srv-pol".into()),
            &EditIntent::Remove,
            &snap,
            &fx.anchors(),
            &OPTS,
        )
        .unwrap_err();
        assert_eq!(
            err,
            WriteTargetError::ReadOnlyOrigin {
                entity: "mcp:srv-pol".into(),
                origin: ProvenanceScope::Policy,
            }
        );
    }

    /// #147/S14：remove 守卫覆盖 embed —— 宿主构造入参（origin=embed，只读 scope）声明的 server durable
    /// 删除 → `ReadOnlyOrigin`（不静默假成功 + 下次 boot 复活；对齐 python `remove_mcp_server` 档3）。
    #[test]
    fn mcp_remove_readonly_embed_origin_errors_147() {
        let fx = Fixture::new();
        let embed: Vec<crate::mcp_clients::model::MCPServerConfig> = vec![serde_json::from_value(
            json!({"type":"stdio","name":"srv-embed","server_parameters":{"command":"e"}}),
        )
        .unwrap()];
        let snap = resolve_snapshot(SnapshotArgs {
            cwd: Some(&fx.wd),
            env: Some(&fx.env),
            home: Some(&fx.home),
            managed_mcp_path: Some(&fx._tmp.path().join("no-managed.json")),
            embed_servers: &embed,
            ..Default::default()
        });
        let err = resolve_write_target(
            &ConfigEntity::McpServer("srv-embed".into()),
            &EditIntent::Remove,
            &snap,
            &fx.anchors(),
            &OPTS,
        )
        .unwrap_err();
        assert_eq!(
            err,
            WriteTargetError::ReadOnlyOrigin {
                entity: "mcp:srv-embed".into(),
                origin: ProvenanceScope::Embed,
            }
        );
    }

    #[test]
    fn mcp_remove_writable_origin_deletes_all_writable_scopes() {
        let fx = Fixture::new();
        write(
            &user_mcp_config_path(Some(&fx.env)),
            r#"{"servers": {"srv": {"type":"stdio","server_parameters":{"command":"u"}}}}"#,
        );
        let snap = fx.snapshot(); // srv origin=User（可写）
        let plans = resolve_write_target(
            &ConfigEntity::McpServer("srv".into()),
            &EditIntent::Remove,
            &snap,
            &fx.anchors(),
            &OPTS,
        )
        .unwrap();
        // 真删干净：user/project/local 三个可写 scope 各一条 Delete。
        assert_eq!(plans.len(), 3);
        let scopes: Vec<SettingsScope> = plans.iter().map(|p| p.scope).collect();
        assert_eq!(
            scopes,
            vec![
                SettingsScope::User,
                SettingsScope::Project,
                SettingsScope::Local
            ]
        );
        let expected_op = WriteTargetOp::Value(mcp_server_write("srv", WriteValue::Delete));
        assert!(plans.iter().all(|p| p.op == expected_op));
    }

    #[test]
    fn mcp_remove_absent_is_idempotent_empty() {
        let fx = Fixture::new();
        let snap = fx.snapshot(); // 空
        let plans = resolve_write_target(
            &ConfigEntity::McpServer("ghost".into()),
            &EditIntent::Remove,
            &snap,
            &fx.anchors(),
            &OPTS,
        )
        .unwrap();
        assert!(plans.is_empty(), "不存在的实体 remove → 幂等空计划");
    }

    #[test]
    fn mcp_disable_independent_targets_disabled_mcpjson_at_local() {
        let fx = Fixture::new();
        write(
            &user_mcp_config_path(Some(&fx.env)),
            r#"{"servers": {"srv": {"type":"stdio","server_parameters":{"command":"u"}}}}"#,
        );
        let snap = fx.snapshot();
        let plans = resolve_write_target(
            &ConfigEntity::McpServer("srv".into()),
            &EditIntent::Disable,
            &snap,
            &fx.anchors(),
            &OPTS,
        )
        .unwrap();
        assert_eq!(plans.len(), 1);
        assert_eq!(
            plans[0].scope,
            SettingsScope::Local,
            "disable 落固定 disable-scope"
        );
        assert_eq!(plans[0].file, workdir_local_settings_path(&fx.wd));
        assert_eq!(
            plans[0].op,
            WriteTargetOp::StringSetInsert {
                field: "disabledMcpjsonServers".into(),
                value: "srv".into(),
            },
            "disable 动 override（信任门数组），不碰声明"
        );
    }

    #[test]
    fn mcp_enable_removes_from_disabled_mcpjson() {
        let fx = Fixture::new();
        let snap = fx.snapshot();
        let plans = resolve_write_target(
            &ConfigEntity::McpServer("srv".into()),
            &EditIntent::Enable,
            &snap,
            &fx.anchors(),
            &OPTS,
        )
        .unwrap();
        assert_eq!(
            plans[0].op,
            WriteTargetOp::StringSetRemove {
                field: "disabledMcpjsonServers".into(),
                value: "srv".into(),
            }
        );
    }

    #[test]
    fn mcp_bundled_name_disable_is_override_not_synthesized() {
        // #126：同名用户声明的 Disable = 正常 override（写 disabledMcpjsonServers、不碰声明），
        // 不再因 bundled 名冲突返回 Synthesized——该名冲突 guard 已从本层移除，"插件占用同名" 的归属
        // 门控上移到 Computer 层（`add_or_update_server`/`remove_server`）。本层对插件归属无感知。
        let fx = Fixture::new();
        write(
            &user_mcp_config_path(Some(&fx.env)),
            r#"{"servers": {"bundled-srv": {"type":"stdio","server_parameters":{"command":"b"}}}}"#,
        );
        update_installed_plugins(
            |file| {
                file.account.plugins.insert(
                    "plug@mp".into(),
                    vec![InstalledPluginRecord {
                        install_path: None,
                        bundled_mcp_servers: vec!["bundled-srv".into()],
                        extra: Map::new(),
                    }],
                );
            },
            Some(&fx.home),
            None,
        )
        .unwrap();
        let snap = fx.snapshot();
        let plans = resolve_write_target(
            &ConfigEntity::McpServer("bundled-srv".into()),
            &EditIntent::Disable,
            &snap,
            &fx.anchors(),
            &OPTS,
        )
        .unwrap();
        assert_eq!(plans.len(), 1);
        assert_eq!(
            plans[0].op,
            WriteTargetOp::StringSetInsert {
                field: "disabledMcpjsonServers".into(),
                value: "bundled-srv".into(),
            },
            "同名用户声明 Disable → override（不再 Synthesized）"
        );
    }

    #[test]
    fn mcp_bundled_name_with_user_declaration_is_editable() {
        // #126：用户在自己 mcp.json 声明的 server，名字撞已装插件 bundled server（同名冲突）时，
        // 仍是 writable origin 的**真实声明** → Upsert 落其 origin scope、Remove 删所有可写 scope，
        // 绝不因 bundled 名冲突被误判为 Synthesized（"无可编辑文件"——但它明明有可编辑声明文件）。
        let fx = Fixture::new();
        write(
            &user_mcp_config_path(Some(&fx.env)),
            r#"{"servers": {"audit-mcp": {"type":"stdio","server_parameters":{"command":"u"}}}}"#,
        );
        update_installed_plugins(
            |file| {
                file.account.plugins.insert(
                    "plug@mp".into(),
                    vec![InstalledPluginRecord {
                        install_path: None,
                        bundled_mcp_servers: vec!["audit-mcp".into()],
                        extra: Map::new(),
                    }],
                );
            },
            Some(&fx.home),
            None,
        )
        .unwrap();
        let snap = fx.snapshot(); // audit-mcp origin=User（可写）+ bundled 名冲突。

        // Upsert（改已有）→ 落 origin scope（User），不 Synthesized。
        let upsert = resolve_write_target(
            &ConfigEntity::McpServer("audit-mcp".into()),
            &EditIntent::Upsert(json!({"type": "stdio"})),
            &snap,
            &fx.anchors(),
            &OPTS,
        )
        .unwrap();
        assert_eq!(upsert.len(), 1);
        assert_eq!(
            upsert[0].scope,
            SettingsScope::User,
            "同名用户声明改动应落其 origin scope，不被 bundled 名冲突拦截"
        );

        // Remove → 删所有可写 scope（真删干净），不 Synthesized。
        let remove = resolve_write_target(
            &ConfigEntity::McpServer("audit-mcp".into()),
            &EditIntent::Remove,
            &snap,
            &fx.anchors(),
            &OPTS,
        )
        .unwrap();
        assert_eq!(
            remove.len(),
            3,
            "同名用户声明删除应产出三条可写 scope 删计划"
        );
        let expected_op = WriteTargetOp::Value(mcp_server_write("audit-mcp", WriteValue::Delete));
        assert!(remove.iter().all(|p| p.op == expected_op));
    }

    #[test]
    fn mcp_readonly_origin_with_bundled_name_is_readonly_not_synthesized() {
        // #126：名字既是**只读 origin**（policy）声明、又命中 ledger bundled 名 → 仍返回 `ReadOnlyOrigin`
        // （bundled 名冲突不改变错误类型；write_target 完全无视 bundled）。守护「bundled 名不改只读 origin 契约」。
        let fx = Fixture::new();
        let managed = fx._tmp.path().join("managed-mcp.json");
        write(
            &managed,
            r#"{"servers": {"audit-mcp": {"type":"stdio","server_parameters":{"command":"p"}}}}"#,
        );
        update_installed_plugins(
            |file| {
                file.account.plugins.insert(
                    "plug@mp".into(),
                    vec![InstalledPluginRecord {
                        install_path: None,
                        bundled_mcp_servers: vec!["audit-mcp".into()],
                        extra: Map::new(),
                    }],
                );
            },
            Some(&fx.home),
            None,
        )
        .unwrap();
        // audit-mcp origin=Policy（只读）+ ledger bundled 名。
        let snap = resolve_snapshot(SnapshotArgs {
            cwd: Some(&fx.wd),
            env: Some(&fx.env),
            home: Some(&fx.home),
            managed_mcp_path: Some(&managed),
            ..Default::default()
        });
        for intent in [
            EditIntent::Upsert(json!({"type": "stdio"})),
            EditIntent::Remove,
        ] {
            let err = resolve_write_target(
                &ConfigEntity::McpServer("audit-mcp".into()),
                &intent,
                &snap,
                &fx.anchors(),
                &OPTS,
            )
            .unwrap_err();
            assert_eq!(
                err,
                WriteTargetError::ReadOnlyOrigin {
                    entity: "mcp:audit-mcp".into(),
                    origin: ProvenanceScope::Policy,
                },
                "只读 origin + bundled 名 → ReadOnlyOrigin（非 Synthesized）"
            );
        }
    }

    #[test]
    fn plugin_disable_sets_enabled_plugins_false_at_local() {
        let fx = Fixture::new();
        let snap = fx.snapshot();
        let plans = resolve_write_target(
            &ConfigEntity::Plugin("plug@mp".into()),
            &EditIntent::Disable,
            &snap,
            &fx.anchors(),
            &OPTS,
        )
        .unwrap();
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].scope, SettingsScope::Local);
        assert_eq!(plans[0].file, workdir_local_settings_path(&fx.wd));
        assert_eq!(
            plans[0].op,
            WriteTargetOp::Value(one(
                "enabledPlugins",
                one("plug@mp", WriteValue::Set(Value::Bool(false)))
            ))
        );
    }

    #[test]
    fn plugin_enable_sets_enabled_plugins_true() {
        let fx = Fixture::new();
        let snap = fx.snapshot();
        let plans = resolve_write_target(
            &ConfigEntity::Plugin("plug@mp".into()),
            &EditIntent::Enable,
            &snap,
            &fx.anchors(),
            &OPTS,
        )
        .unwrap();
        assert_eq!(
            plans[0].op,
            WriteTargetOp::Value(one(
                "enabledPlugins",
                one("plug@mp", WriteValue::Set(Value::Bool(true)))
            ))
        );
    }

    #[test]
    fn mcp_upsert_edit_readonly_origin_errors() {
        // 验收 #2 的 edit 分支：policy / flag origin 上 Upsert(改) → ReadOnlyOrigin（不得静默落可写 scope）。
        let fx = Fixture::new();
        for origin in [ProvenanceScope::Policy, ProvenanceScope::Flag] {
            let mut prov = BTreeMap::new();
            prov.insert(EntityKey::Mcp("srv".into()), origin);
            let snap = snapshot_with_provenance(prov);
            let err = resolve_write_target(
                &ConfigEntity::McpServer("srv".into()),
                &EditIntent::Upsert(json!({"type": "stdio"})),
                &snap,
                &fx.anchors(),
                &OPTS,
            )
            .unwrap_err();
            assert_eq!(
                err,
                WriteTargetError::ReadOnlyOrigin {
                    entity: "mcp:srv".into(),
                    origin,
                },
                "改只读 origin 的 server 必须结构化报错、不静默落可写 scope"
            );
        }
    }

    #[test]
    fn mcp_upsert_new_honors_upsert_scope_option() {
        // opts.upsert_new_scope 透传：非默认 User → 新声明落 user mcp.json（守护 opts 非死参数）。
        let fx = Fixture::new();
        let snap = fx.snapshot(); // 空 → 新实体
        let opts = WriteTargetOptions {
            upsert_new_scope: WriteScope::User,
            disable_scope: WriteScope::Local,
        };
        let plans = resolve_write_target(
            &ConfigEntity::McpServer("srv".into()),
            &EditIntent::Upsert(json!({"type": "stdio"})),
            &snap,
            &fx.anchors(),
            &opts,
        )
        .unwrap();
        assert_eq!(plans[0].scope, SettingsScope::User);
        assert_eq!(plans[0].file, user_mcp_config_path(Some(&fx.env)));
    }

    #[test]
    fn disable_honors_disable_scope_option() {
        // opts.disable_scope 透传：非默认 Project → mcp disable 与 plugin disable 均落 project settings。
        let fx = Fixture::new();
        let snap = fx.snapshot();
        let opts = WriteTargetOptions {
            upsert_new_scope: WriteScope::Project,
            disable_scope: WriteScope::Project,
        };
        let mcp = resolve_write_target(
            &ConfigEntity::McpServer("srv".into()),
            &EditIntent::Disable,
            &snap,
            &fx.anchors(),
            &opts,
        )
        .unwrap();
        assert_eq!(mcp[0].scope, SettingsScope::Project);
        assert_eq!(mcp[0].file, workdir_project_settings_path(&fx.wd));
        let plugin = resolve_write_target(
            &ConfigEntity::Plugin("plug@mp".into()),
            &EditIntent::Disable,
            &snap,
            &fx.anchors(),
            &opts,
        )
        .unwrap();
        assert_eq!(plugin[0].scope, SettingsScope::Project);
        assert_eq!(plugin[0].file, workdir_project_settings_path(&fx.wd));
    }

    #[test]
    fn plugin_install_uninstall_is_unsupported() {
        let fx = Fixture::new();
        let snap = fx.snapshot();
        for intent in [EditIntent::Upsert(json!({})), EditIntent::Remove] {
            let err = resolve_write_target(
                &ConfigEntity::Plugin("plug@mp".into()),
                &intent,
                &snap,
                &fx.anchors(),
                &OPTS,
            )
            .unwrap_err();
            assert!(
                matches!(err, WriteTargetError::Unsupported { .. }),
                "plugin install/uninstall 归 installer、非本函数"
            );
        }
    }
}
