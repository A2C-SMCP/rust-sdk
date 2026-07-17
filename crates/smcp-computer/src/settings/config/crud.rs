/*!
* 文件名: crud.rs
* 作者: JQQ
* 创建日期: 2026/07/10
* 最后修改日期: 2026/07/10
* 版权: 2023 JQQ. All rights reserved.
* 依赖: serde_json, settings::{scope, mcp_config, store, config::{snapshot, write_target, executor}}
* 描述: #107 S3（#110）—— SDK-owned Config CRUD 顶层入口。
*       读 = 多 scope reconcile 投影（S1 快照）；写 = 经 S2 消解器 fan-out + S3 执行器落盘。
*       Config CRUD top-level entry: read = reconcile projection (S1), write = via S2 resolver + S3 executor.
*
* `config_dir` 语义（design-107 §2.3）/ semantics:
*   - `config_dir` = **project-scope 锚点**（client 唯一合法拥有的目录，`<config_dir>/.tfrobot/` 下），
*     **不是**单文件 config。user/policy/home 为 env-resolved ambient（`ConfigContext` 注入）。
*   - 读是 many→one（合并投影），写是 one→many（经消解器 fan-out）。这样既守「SDK 不维护 Computer registry」
*     （只吃递进来的目录），又保住五级 scope 模型。
*
* 边界（本次范围，design-107 §9 / #110）/ scope boundary:
*   - **仅 config 层**：CRUD + 执行器。**不碰** `computer.rs` 的 runtime mutate 接线（`add_or_update_server`/
*     `remove_server` → 落盘 → reload → bump revision）——那是 S6（blocked-by S3+S7），本层是其底座。
*   - `init/delete/duplicate` 只作用于 **project 锚点**（含机器本地 `*.local.json`）；**绝不**触碰
*     user / policy / ambient home（含 SKILL Home 账本）。故 `duplicate_config` 天然不搬 installPath
*     （账本在 home、不在 `.tfrobot`）→ 由 boot 按协议 §5.8 重建（design §12 R4）。
*/

use std::io;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use super::super::mcp_config::{workdir_mcp_config_path, workdir_mcp_local_config_path};
use super::super::scope::{workdir_local_settings_path, workdir_project_settings_path, EnvMap};
use super::super::store::{atomic_write_settings_json, with_settings_lock};
use super::executor::{execute_write_plans, ExecutorError};
use super::snapshot::{resolve_snapshot, ComputerConfigSnapshot, SnapshotArgs};
use super::write_target::{
    resolve_write_target, ConfigEntity, EditIntent, ScopeAnchors, WriteTargetError,
    WriteTargetOptions,
};
use crate::mcp_clients::model::MCPServerConfig;

// ===========================================================================
// 上下文 / Context
// ===========================================================================

/// 一次 config 操作的 ambient 解析输入（镜像 [`SnapshotArgs`] + 写选项）/ ambient inputs for one config op。
///
/// 持借用字段以便在一次 `update_config` 内多次重建 [`SnapshotArgs`]（其未实现 `Clone`）。
/// 生产恒传 `config_dir` = 进程 cwd（client 递来的 project 锚点）；env/home/managed 为 `None` 时走进程环境/默认。
#[derive(Debug, Clone, Copy)]
pub struct ConfigContext<'a> {
    /// project-scope 锚点（= config_dir）/ project-scope anchor。
    pub config_dir: &'a Path,
    /// 环境映射（解析 user config dir）/ env map。
    pub env: Option<&'a EnvMap>,
    /// SKILL Home（账本根）/ SKILL Home。
    pub home: Option<&'a Path>,
    /// `--settings <file>` flag scope。
    pub flag_settings_path: Option<&'a Path>,
    /// `--mcp-config <file>` flag scope。
    pub flag_mcp_config_path: Option<&'a Path>,
    /// policy managed mcp.json 路径（测试注入接缝）/ managed mcp path。
    pub managed_mcp_path: Option<&'a Path>,
    /// policy 平台标识（managed dir 解析）/ policy platform。
    pub platform: Option<&'a str>,
    /// policy settings 原始视图 / raw policy settings。
    pub policy_settings: Option<&'a Map<String, Value>>,
    /// 宿主构造入参 embed 层（`Computer::new(mcp_servers=…)`）——供 remove 守卫按 origin 判定覆盖 embed（#147）。
    pub embed_servers: &'a [MCPServerConfig],
    /// 写目标消解选项（upsert/disable scope）/ write-target options。
    pub opts: WriteTargetOptions,
}

impl<'a> ConfigContext<'a> {
    /// 最小构造：仅 project 锚点，其余 ambient 走进程环境/默认 / minimal: just the project anchor。
    pub fn new(config_dir: &'a Path) -> Self {
        Self {
            config_dir,
            env: None,
            home: None,
            flag_settings_path: None,
            flag_mcp_config_path: None,
            managed_mcp_path: None,
            platform: None,
            policy_settings: None,
            embed_servers: &[],
            opts: WriteTargetOptions::default(),
        }
    }

    /// 组装 S1 快照入参（`cwd` = project 锚点）/ build the S1 snapshot args。
    fn snapshot_args(&self) -> SnapshotArgs<'a> {
        SnapshotArgs {
            cwd: Some(self.config_dir),
            env: self.env,
            home: self.home,
            flag_settings_path: self.flag_settings_path,
            flag_mcp_config_path: self.flag_mcp_config_path,
            managed_mcp_path: self.managed_mcp_path,
            platform: self.platform,
            policy_settings: self.policy_settings,
            embed_servers: self.embed_servers,
        }
    }

    /// 组装 S2 写目标锚点（env-resolved 路径拼接）/ build the S2 write-target anchors。
    fn anchors(&self) -> ScopeAnchors {
        ScopeAnchors::new(self.config_dir, self.env.cloned().unwrap_or_default())
    }
}

// ===========================================================================
// 编辑意图 + 错误 / Edits + errors
// ===========================================================================

/// 一次实体级编辑（entity + intent）/ one entity-level edit。
#[derive(Debug, Clone)]
pub struct ConfigEdit {
    /// 目标实体 / target entity。
    pub entity: ConfigEntity,
    /// 编辑意图 / edit intent。
    pub intent: EditIntent,
}

impl ConfigEdit {
    /// 构造 / construct。
    pub fn new(entity: ConfigEntity, intent: EditIntent) -> Self {
        Self { entity, intent }
    }
}

/// Config CRUD 错误 / config CRUD errors。
#[derive(Debug, Clone, PartialEq)]
pub enum ConfigCrudError {
    /// 写目标消解失败（只读 origin / synthesized / unsupported）/ write-target resolution failed。
    WriteTarget(WriteTargetError),
    /// 执行器落盘失败（锁 / I/O / 损坏文件）/ executor failed。
    Executor(ExecutorError),
    /// 文件系统操作失败（init/delete/duplicate）/ filesystem op failed。
    Io {
        /// 目标路径。
        path: PathBuf,
        /// 原因。
        reason: String,
    },
}

impl std::fmt::Display for ConfigCrudError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigCrudError::WriteTarget(e) => write!(f, "write-target: {e}"),
            ConfigCrudError::Executor(e) => write!(f, "executor: {e}"),
            ConfigCrudError::Io { path, reason } => {
                write!(f, "I/O on {}: {reason}", path.display())
            }
        }
    }
}

impl std::error::Error for ConfigCrudError {}

impl From<WriteTargetError> for ConfigCrudError {
    fn from(e: WriteTargetError) -> Self {
        ConfigCrudError::WriteTarget(e)
    }
}

impl From<ExecutorError> for ConfigCrudError {
    fn from(e: ExecutorError) -> Self {
        ConfigCrudError::Executor(e)
    }
}

// ===========================================================================
// CRUD：读 + 改 / read + mutate
// ===========================================================================

/// 读多 scope reconcile 投影（= S1 [`resolve_snapshot`]）/ read the reconcile projection。
pub fn load_config(ctx: &ConfigContext) -> ComputerConfigSnapshot {
    resolve_snapshot(ctx.snapshot_args())
}

/// 批量实体编辑：经 S2 消解器 fan-out → S3 执行器落盘 → 重投影返回新快照 / apply edits, return fresh snapshot.
///
/// **两阶段**（先消解、后执行）：所有 edit 先对**同一** pre-batch 快照全量消解为 `Vec<WritePlan>`——任一消解
/// 报错（只读 origin / synthesized / unsupported）即整批放弃、**零落盘**（不半改）。消解全过后统一执行。
/// 返回**重投影**快照：`revision` 是内容摘要，内容变则自动 bump（无需手动计数）。
pub fn update_config(
    ctx: &ConfigContext,
    edits: &[ConfigEdit],
) -> Result<ComputerConfigSnapshot, ConfigCrudError> {
    let snapshot = load_config(ctx);
    let anchors = ctx.anchors();
    let mut plans = Vec::new();
    for edit in edits {
        let edit_plans =
            resolve_write_target(&edit.entity, &edit.intent, &snapshot, &anchors, &ctx.opts)?;
        plans.extend(edit_plans);
    }
    execute_write_plans(&plans)?;
    Ok(load_config(ctx))
}

// ===========================================================================
// CRUD：project 锚点文件生命周期 / project-anchor file lifecycle
// ===========================================================================

/// project 锚点的 4 个 SDK 文件（固定顺序：project settings / local settings / project mcp / local mcp）。
fn project_anchor_files(config_dir: &Path) -> [PathBuf; 4] {
    [
        workdir_project_settings_path(config_dir),
        workdir_local_settings_path(config_dir),
        workdir_mcp_config_path(config_dir),
        workdir_mcp_local_config_path(config_dir),
    ]
}

/// 建 project 锚点并幂等 seed 空骨架（`settings.json={}`、`mcp.json={"servers":{}}`）/ init the project anchor。
///
/// 已存在的文件**不覆盖**（幂等）。只 seed 两个「主」文件（`*.local.json` 按需惰性创建）。
pub fn init_config(config_dir: &Path) -> Result<(), ConfigCrudError> {
    seed_if_absent(
        &workdir_project_settings_path(config_dir),
        &Value::Object(Map::new()),
    )?;
    seed_if_absent(
        &workdir_mcp_config_path(config_dir),
        &json!({ "servers": {} }),
    )?;
    Ok(())
}

/// 删 project 锚点的 4 个 SDK 文件（含 `*.local.json`）；缺失 = noop（幂等）/ delete project-anchor SDK files。
///
/// 保留 `.tfrobot` 目录及其中的非 SDK 文件；**绝不**触碰 user/policy/ambient home。
pub fn delete_config(config_dir: &Path) -> Result<(), ConfigCrudError> {
    for path in project_anchor_files(config_dir) {
        remove_if_present(&path)?;
    }
    Ok(())
}

/// 把 `src` project 锚点复制到 `dst`（经 [`load_project_config_doc`] + [`save_config`]）/ duplicate the project anchor.
///
/// 只搬 project 锚点的 4 个 SDK 文件（存在者）；**ambient home 账本不在 `.tfrobot` 内 → 天然不搬** →
/// installPath 由 boot 按协议 §5.8 重建（design §12 R4）。`src` 缺失的文件在 `dst` 保持缺失。
pub fn duplicate_config(src: &Path, dst: &Path) -> Result<(), ConfigCrudError> {
    let doc = load_project_config_doc(src)?;
    save_config(dst, &doc)
}

// ===========================================================================
// project 配置文档（save/load 结构化持久化）/ project config document
// ===========================================================================

/// project 锚点拥有的原始配置文档（4 文件的原始 JSON object；`None` = 该文件缺失）/ raw project-anchor doc。
///
/// 承载 project 锚点**逐字节**内容（无校验、无脱敏）：供 `duplicate_config`、S4 import/export 的结构化落盘。
/// `None` 与 `Some({})` 有别：前者「文件缺失」（save 时不建），后者「文件存在且为空对象」（save 时写 `{}`）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProjectConfigDoc {
    /// `<config_dir>/.tfrobot/settings.json`。
    pub settings: Option<Map<String, Value>>,
    /// `<config_dir>/.tfrobot/settings.local.json`。
    pub settings_local: Option<Map<String, Value>>,
    /// `<config_dir>/.tfrobot/mcp.json`。
    pub mcp: Option<Map<String, Value>>,
    /// `<config_dir>/.tfrobot/mcp.local.json`。
    pub mcp_local: Option<Map<String, Value>>,
}

impl ProjectConfigDoc {
    /// 按 [`project_anchor_files`] 顺序取 4 个 slot（与 save/load 对齐）/ slots in anchor-file order。
    fn slots(&self) -> [&Option<Map<String, Value>>; 4] {
        [
            &self.settings,
            &self.settings_local,
            &self.mcp,
            &self.mcp_local,
        ]
    }
}

/// 逐字节读 project 锚点的 4 文件为文档（缺失 → `None`；损坏 → 硬错，不静默当空）/ raw-load the project doc。
pub fn load_project_config_doc(config_dir: &Path) -> Result<ProjectConfigDoc, ConfigCrudError> {
    let [settings_p, local_p, mcp_p, mcp_local_p] = project_anchor_files(config_dir);
    Ok(ProjectConfigDoc {
        settings: read_raw_object_opt(&settings_p)?,
        settings_local: read_raw_object_opt(&local_p)?,
        mcp: read_raw_object_opt(&mcp_p)?,
        mcp_local: read_raw_object_opt(&mcp_local_p)?,
    })
}

/// 把文档整体写到 `config_dir` 的 project 锚点（每个 `Some` 落盘、`None` 不触碰）/ wholesale-write the doc。
///
/// **整体替换**语义（与 `update_config` 的实体级 fan-out 不同）：`Some(map)` 覆盖该文件、`None` 跳过（不建不删）。
pub fn save_config(config_dir: &Path, doc: &ProjectConfigDoc) -> Result<(), ConfigCrudError> {
    let paths = project_anchor_files(config_dir);
    for (path, slot) in paths.iter().zip(doc.slots()) {
        if let Some(map) = slot {
            write_raw_object(path, map)?;
        }
    }
    Ok(())
}

// ===========================================================================
// 内部辅助 / Helpers（持锁 + 原子写，复用 store 原语）
// ===========================================================================

/// 持锁读原始 JSON object：缺失 → `None`；损坏/非 object → 硬错（不静默当空，防 save 丢数据）。
fn read_raw_object_opt(path: &Path) -> Result<Option<Map<String, Value>>, ConfigCrudError> {
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path).map_err(|e| ConfigCrudError::Io {
        path: path.to_path_buf(),
        reason: e.to_string(),
    })?;
    if text.trim().is_empty() {
        return Ok(Some(Map::new()));
    }
    let value: Value = serde_json::from_str(&text).map_err(|e| ConfigCrudError::Io {
        path: path.to_path_buf(),
        reason: format!("corrupt JSON: {e}"),
    })?;
    match value {
        Value::Object(map) => Ok(Some(map)),
        _ => Err(ConfigCrudError::Io {
            path: path.to_path_buf(),
            reason: "top-level JSON is not an object".to_string(),
        }),
    }
}

/// 持锁原子写一个 JSON object（无写保护头，对齐人编意图层）/ locked atomic write of a JSON object。
fn write_raw_object(path: &Path, map: &Map<String, Value>) -> Result<(), ConfigCrudError> {
    let value = Value::Object(map.clone());
    io_locked(path, || atomic_write_settings_json(path, &value))
}

/// 仅当文件缺失时 seed（幂等 init）/ seed only if absent。
fn seed_if_absent(path: &Path, seed: &Value) -> Result<(), ConfigCrudError> {
    io_locked(path, || {
        if path.exists() {
            Ok(())
        } else {
            atomic_write_settings_json(path, seed)
        }
    })
}

/// 仅当文件存在时删（幂等 delete）/ remove only if present。
fn remove_if_present(path: &Path) -> Result<(), ConfigCrudError> {
    io_locked(path, || match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    })
}

/// 在目标文件旁车锁下执行一次 `io::Result` 操作，双层错误映射到 [`ConfigCrudError::Io`]。
fn io_locked(path: &Path, work: impl FnOnce() -> io::Result<()>) -> Result<(), ConfigCrudError> {
    let to_io = |reason: String| ConfigCrudError::Io {
        path: path.to_path_buf(),
        reason,
    };
    with_settings_lock(path, work)
        .map_err(|e| to_io(e.to_string()))?
        .map_err(|e| to_io(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::mcp_config::user_mcp_config_path;
    use crate::settings::reconciler::InstalledPluginRecord;
    use crate::settings::store::{installed_plugins_path, update_installed_plugins};
    use serde_json::Map as JsonMap;
    use tempfile::TempDir;

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    fn read_json(path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

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
            let env: EnvMap = std::iter::once((
                "XDG_CONFIG_HOME".to_string(),
                tmp.path().join("xdg").to_string_lossy().into_owned(),
            ))
            .collect();
            let wd = tmp.path().join("wd");
            let home = tmp.path().join("home");
            let managed = tmp.path().join("no-managed.json");
            Self {
                _tmp: tmp,
                env,
                wd,
                home,
                managed,
            }
        }

        fn ctx(&self) -> ConfigContext<'_> {
            ConfigContext {
                config_dir: &self.wd,
                env: Some(&self.env),
                home: Some(&self.home),
                managed_mcp_path: Some(&self.managed),
                ..ConfigContext::new(&self.wd)
            }
        }
    }

    #[test]
    fn init_creates_idempotent_skeleton() {
        let fx = Fixture::new();
        init_config(&fx.wd).unwrap();
        let settings = workdir_project_settings_path(&fx.wd);
        let mcp = workdir_mcp_config_path(&fx.wd);
        assert_eq!(read_json(&settings), json!({}));
        assert_eq!(read_json(&mcp), json!({ "servers": {} }));
        // 幂等：已存在的用户内容不被覆盖。
        write(&mcp, r#"{"servers": {"srv": {"type": "stdio"}}}"#);
        init_config(&fx.wd).unwrap();
        assert_eq!(
            read_json(&mcp),
            json!({"servers": {"srv": {"type": "stdio"}}})
        );
    }

    #[test]
    fn load_config_projects_reconcile_snapshot() {
        let fx = Fixture::new();
        write(
            &user_mcp_config_path(Some(&fx.env)),
            r#"{"servers": {"srv": {"type": "stdio", "server_parameters": {"command": "u"}}}}"#,
        );
        let snap = load_config(&fx.ctx());
        assert!(snap.mcp.servers.iter().any(|s| s.name == "srv"));
    }

    #[test]
    fn update_config_upsert_edit_lands_at_origin_scope_not_constant_user() {
        // 验收：update_config 落到消解器判定的 scope（非恒 user）。
        // srv 声明在 project → 改它落 project mcp.json（而非 user）。
        let fx = Fixture::new();
        write(
            &workdir_mcp_config_path(&fx.wd),
            r#"{"servers": {"srv": {"type": "stdio", "server_parameters": {"command": "p"}}}}"#,
        );
        let ctx = fx.ctx();
        let snap = update_config(
            &ctx,
            &[ConfigEdit::new(
                ConfigEntity::McpServer("srv".into()),
                EditIntent::Upsert(
                    json!({"type": "stdio", "server_parameters": {"command": "edited"}}),
                ),
            )],
        )
        .unwrap();
        // 落 project、user 未被创建。
        assert_eq!(
            read_json(&workdir_mcp_config_path(&fx.wd))["servers"]["srv"]["server_parameters"]
                ["command"],
            json!("edited")
        );
        assert!(!user_mcp_config_path(Some(&fx.env)).exists(), "不得落 user");
        // 重投影快照仍见该 server。
        assert!(snap.mcp.servers.iter().any(|s| s.name == "srv"));
    }

    #[test]
    fn update_config_disable_lands_in_local_settings_via_override() {
        // disable ≠ remove：写 disabledMcpjsonServers 到 local settings（不碰 mcp.json 声明）。
        let fx = Fixture::new();
        write(
            &workdir_mcp_config_path(&fx.wd),
            r#"{"servers": {"srv": {"type": "stdio", "server_parameters": {"command": "p"}}}}"#,
        );
        let ctx = fx.ctx();
        update_config(
            &ctx,
            &[ConfigEdit::new(
                ConfigEntity::McpServer("srv".into()),
                EditIntent::Disable,
            )],
        )
        .unwrap();
        // override 落 local settings。
        assert_eq!(
            read_json(&workdir_local_settings_path(&fx.wd)),
            json!({"disabledMcpjsonServers": ["srv"]})
        );
        // 声明（mcp.json）不动。
        assert!(read_json(&workdir_mcp_config_path(&fx.wd))["servers"]
            .get("srv")
            .is_some());
    }

    #[test]
    fn update_config_readonly_origin_aborts_batch_zero_write() {
        // policy origin 上 Upsert(改) → 整批放弃、零落盘（两阶段：消解期报错、不半改）。
        let fx = Fixture::new();
        write(
            &fx.managed,
            r#"{"servers": {"srv-pol": {"type": "stdio", "server_parameters": {"command": "p"}}}}"#,
        );
        let ctx = fx.ctx();
        // 第二条会命中 policy 只读 → 整批放弃；验证第一条（合法 upsert）也未落盘。
        let err = update_config(
            &ctx,
            &[
                ConfigEdit::new(
                    ConfigEntity::McpServer("fresh".into()),
                    EditIntent::Upsert(json!({"type": "stdio"})),
                ),
                ConfigEdit::new(
                    ConfigEntity::McpServer("srv-pol".into()),
                    EditIntent::Upsert(json!({"type": "stdio"})),
                ),
            ],
        )
        .unwrap_err();
        assert!(matches!(err, ConfigCrudError::WriteTarget(_)));
        assert!(
            !workdir_mcp_config_path(&fx.wd).exists(),
            "整批放弃 → 合法的第一条也不得落盘"
        );
    }

    #[test]
    fn crud_roundtrip_save_then_load() {
        // save_config → load_project_config_doc 往返一致；None 文件保持缺失。
        let fx = Fixture::new();
        let mut settings = JsonMap::new();
        settings.insert("strictKnownMarketplaces".into(), json!(true));
        let doc = ProjectConfigDoc {
            settings: Some(settings.clone()),
            mcp: Some(
                json!({"servers": {"srv": {"type": "stdio"}}})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
            ..Default::default()
        };
        save_config(&fx.wd, &doc).unwrap();
        let loaded = load_project_config_doc(&fx.wd).unwrap();
        assert_eq!(loaded, doc);
        // None slot（settings_local / mcp_local）未落盘。
        assert!(!workdir_local_settings_path(&fx.wd).exists());
        assert!(!workdir_mcp_local_config_path(&fx.wd).exists());
    }

    #[test]
    fn delete_config_removes_only_project_anchor_files() {
        let fx = Fixture::new();
        // project 锚点 + 一个非 SDK 文件 + ambient home 账本。
        write(&workdir_project_settings_path(&fx.wd), r#"{"a": 1}"#);
        write(&workdir_mcp_config_path(&fx.wd), r#"{"servers": {}}"#);
        let non_sdk = workdir_project_settings_path(&fx.wd)
            .parent()
            .unwrap()
            .join("notes.txt");
        write(&non_sdk, "keep me");
        write(
            &user_mcp_config_path(Some(&fx.env)),
            r#"{"servers": {"u": {"type": "stdio"}}}"#,
        );
        delete_config(&fx.wd).unwrap();
        // SDK 文件删。
        assert!(!workdir_project_settings_path(&fx.wd).exists());
        assert!(!workdir_mcp_config_path(&fx.wd).exists());
        // 非 SDK 文件 + user ambient 保留。
        assert!(non_sdk.exists(), "非 SDK 文件不得删");
        assert!(
            user_mcp_config_path(Some(&fx.env)).exists(),
            "user ambient 不得碰"
        );
    }

    #[test]
    fn duplicate_config_copies_project_anchor_not_ambient_ledger() {
        // 验收 §5.8/R4：duplicate 搬 project 锚点，不搬 ambient home 账本（installPath 由 boot 重建）。
        let fx = Fixture::new();
        // src project 锚点。
        write(
            &workdir_mcp_config_path(&fx.wd),
            r#"{"servers": {"srv": {"type": "stdio"}}}"#,
        );
        write(
            &workdir_project_settings_path(&fx.wd),
            r#"{"strictKnownMarketplaces": true}"#,
        );
        // ambient home 账本（带 installPath）——不应被 duplicate 搬运。
        update_installed_plugins(
            |file| {
                file.account.plugins.insert(
                    "plug@mp".into(),
                    vec![InstalledPluginRecord {
                        install_path: Some("/abs/install/path".into()),
                        bundled_mcp_servers: vec![],
                        extra: Map::new(),
                    }],
                );
            },
            Some(&fx.home),
            None,
        )
        .unwrap();
        let ledger_before =
            std::fs::read_to_string(installed_plugins_path(Some(&fx.home), None)).unwrap();

        let dst = fx._tmp.path().join("dst");
        duplicate_config(&fx.wd, &dst).unwrap();

        // project 锚点被复制到 dst。
        assert_eq!(
            read_json(&workdir_mcp_config_path(&dst)),
            json!({"servers": {"srv": {"type": "stdio"}}})
        );
        assert_eq!(
            read_json(&workdir_project_settings_path(&dst)),
            json!({"strictKnownMarketplaces": true})
        );
        // dst 下的 .tfrobot 不含账本（账本在 home、不在锚点）→ installPath 不被照搬。
        assert!(!workdir_mcp_local_config_path(&dst).exists());
        // ambient home 账本原封不动（duplicate 未触碰）。
        let ledger_after =
            std::fs::read_to_string(installed_plugins_path(Some(&fx.home), None)).unwrap();
        assert_eq!(
            ledger_before, ledger_after,
            "ambient home 账本不得被 duplicate 触碰"
        );
    }

    #[test]
    fn duplicate_absent_src_file_stays_absent_in_dst() {
        // src 无 settings.local.json → dst 也不得凭空建。
        let fx = Fixture::new();
        write(&workdir_mcp_config_path(&fx.wd), r#"{"servers": {}}"#);
        let dst = fx._tmp.path().join("dst");
        duplicate_config(&fx.wd, &dst).unwrap();
        assert!(workdir_mcp_config_path(&dst).exists());
        assert!(
            !workdir_local_settings_path(&dst).exists(),
            "src 缺失的文件在 dst 保持缺失"
        );
    }

    #[test]
    fn plugin_disable_enable_through_update_config() {
        // plugin 资产类：disable/enable 落 enabledPlugins（settings），走完整 CRUD 路径。
        let fx = Fixture::new();
        let ctx = fx.ctx();
        update_config(
            &ctx,
            &[ConfigEdit::new(
                ConfigEntity::Plugin("plug@mp".into()),
                EditIntent::Disable,
            )],
        )
        .unwrap();
        assert_eq!(
            read_json(&workdir_local_settings_path(&fx.wd)),
            json!({"enabledPlugins": {"plug@mp": false}})
        );
        // enable 翻正。
        update_config(
            &ctx,
            &[ConfigEdit::new(
                ConfigEntity::Plugin("plug@mp".into()),
                EditIntent::Enable,
            )],
        )
        .unwrap();
        assert_eq!(
            read_json(&workdir_local_settings_path(&fx.wd)),
            json!({"enabledPlugins": {"plug@mp": true}})
        );
    }

    #[test]
    fn update_config_upsert_edit_bumps_config_revision() {
        // 声明变更（改 server 的 command）→ mcp 投影内容变 → config revision 摘要 bump。
        let fx = Fixture::new();
        write(
            &workdir_mcp_config_path(&fx.wd),
            r#"{"servers": {"srv": {"type": "stdio", "server_parameters": {"command": "p"}}}}"#,
        );
        let ctx = fx.ctx();
        let before = load_config(&ctx).revision;
        let after = update_config(
            &ctx,
            &[ConfigEdit::new(
                ConfigEntity::McpServer("srv".into()),
                EditIntent::Upsert(
                    json!({"type": "stdio", "server_parameters": {"command": "edited"}}),
                ),
            )],
        )
        .unwrap()
        .revision;
        assert_ne!(before, after, "声明内容变 → config revision 摘要 bump");
    }

    #[test]
    fn mcp_disable_changes_gating_not_config_revision() {
        // 语义锁：独立 MCP 的 disable 写 `disabledMcpjsonServers`（信任门/gating），**不改声明**——
        // 该字段不入 config 快照投影，故 **config revision 不变**（对齐 design §12 R2：config revision ⊥
        // capability revision；disable 是能力/gating 变更，归 S7/S8 capability revision，非 config 变更）。
        let fx = Fixture::new();
        write(
            &workdir_mcp_config_path(&fx.wd),
            r#"{"servers": {"srv": {"type": "stdio", "server_parameters": {"command": "p"}}}}"#,
        );
        let ctx = fx.ctx();
        let before = load_config(&ctx).revision;
        let after = update_config(
            &ctx,
            &[ConfigEdit::new(
                ConfigEntity::McpServer("srv".into()),
                EditIntent::Disable,
            )],
        )
        .unwrap()
        .revision;
        // 落盘发生（gating override 写入 local settings），但 config revision 稳定。
        assert_eq!(
            read_json(&workdir_local_settings_path(&fx.wd)),
            json!({"disabledMcpjsonServers": ["srv"]})
        );
        assert_eq!(
            before, after,
            "mcp disable 是 gating 变更、非 config 声明变更 → config revision 稳定"
        );
    }
}
