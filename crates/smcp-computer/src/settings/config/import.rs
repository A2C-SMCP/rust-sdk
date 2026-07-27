/*!
* 文件名: import.rs
* 作者: JQQ
* 创建日期: 2026/07/22
* 最后修改日期: 2026/07/22
* 版权: 2023 JQQ. All rights reserved.
* 依赖: serde_json, settings::config::{crud, executor, snapshot, write_target}, settings::mcp_config, mcp_clients
* 描述: #151 Part 2 —— typed MCP import + 零写 preflight（SDK-owned config 边界，#107）。
*       typed MCP import + zero-write preflight (SDK-owned config boundary).
*/

//! #151 Part 2 —— typed MCP import + 零写 preflight（SDK-owned config 边界，#107）。
//!
//! 下游（tfrobot-client TFRC-66）要只通过 SDK 公共 API 完成「MCP 配置校验 + 可恢复导入」，不再手工选 scope、
//! 改 `ProjectConfigDoc`、写 `mcp.local.json`。本模块提供 typed 入口：SDK 决定序列化
//! （`canonicalize_persist_body`）、provenance 与 write-target（[`resolve_write_target`] 纯函数）、多实体结果
//! 语义（**全有或全无**），并暴露**零写 preflight**——在调用方写 inputs/事务日志前以只读方式暴露确定性错误
//! （schema / 只读来源 / 损坏目标 / `${input:}` 不可达），**永不取真实值**（守 #107：不接管 client-owned
//! inputs/profile/secrets；不探测 secret 可解性 / 文件存在 / MCP 可启动——那是运行期 start 阶段）。
//!
//! 纯 config 层：**不 mount / 不 render 取值**（运行期物化归 `Computer::mount_server`，本 API 守 config 边界）。

use std::collections::HashSet;
use std::path::PathBuf;

use serde_json::Value;

use super::crud::{load_config, update_config, ConfigContext, ConfigCrudError, ConfigEdit};
use super::executor::read_raw_object;
use super::write_target::{
    resolve_write_target, ConfigEntity, EditIntent, WriteScope, WriteTargetError,
};
use crate::mcp_clients::bundle_id::{resolve_bundle_id, BundleId};
use crate::mcp_clients::model::MCPServerConfig;
use crate::settings::mcp_config::{canonicalize_persist_body, validate_server};
use crate::settings::schema::{SettingsScope, SettingsValidationError};

// ===========================================================================
// 报告类型 / Report types
// ===========================================================================

/// 一个**会**被导入落盘的 server（preflight 零写预测）/ a server that WILL be persisted (zero-write preview).
#[derive(Debug, Clone)]
pub struct PlannedServer {
    /// server 名（= mcp.json map key）/ server name (map key).
    pub name: String,
    /// 落点 scope（User/Project/Local）/ target scope.
    pub scope: WriteScope,
    /// 解析后的身份键 / resolved identity key.
    pub bundle_id: BundleId,
}

/// 零写 preflight 结果 / zero-write preflight result.
///
/// `planned` 是「本会落盘」的预测（调用方事务提交后即可期待），`diagnostics` 是阻止安全导入的全部确定性
/// 问题。`diagnostics` 非空 ⇒ [`import_mcp_servers`] 拒绝写盘（全有或全无）。
#[derive(Debug, Clone, Default)]
pub struct PreflightReport {
    /// 确定性诊断（schema / 只读来源 / 损坏目标 / `${input:}` 不可达；非阻断式收集）/ deterministic diagnostics.
    pub diagnostics: Vec<SettingsValidationError>,
    /// 零写预测的落盘清单 / zero-write predicted persist list.
    pub planned: Vec<PlannedServer>,
}

impl PreflightReport {
    /// 是否干净（无诊断 → 可安全导入）/ clean (safe to import)?
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.diagnostics.is_empty()
    }
}

/// typed MCP import 失败 / typed MCP import failure.
#[derive(Debug)]
pub enum ImportError {
    /// preflight 拦截（诊断非空，零写）/ preflight blocked (zero-write).
    Preflight(PreflightReport),
    /// 落盘失败（I/O / 锁 / 损坏目标；executor 已 best-effort 零写探测）/ persist failed.
    Persist(ConfigCrudError),
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImportError::Preflight(r) => write!(
                f,
                "preflight blocked import with {} diagnostic(s)",
                r.diagnostics.len()
            ),
            ImportError::Persist(e) => write!(f, "persist failed: {e}"),
        }
    }
}

impl std::error::Error for ImportError {}

// ===========================================================================
// preflight（零写）/ preflight (zero-write)
// ===========================================================================

/// **零写 preflight**：对一批 typed MCP server 做确定性 + 引用语法可达校验，预测落盘清单 / zero-write preflight.
///
/// 检查四类确定性错误（守 #107：**不取真实值**、不探测 secret 可解性 / 文件存在 / MCP 可启动——那是运行期）：
/// 1. **schema**：`validate_server`（按 planned scope，与读侧同一道门）。
/// 2. **write-target**：[`resolve_write_target`]（纯函数）的只读 origin / synthesized / unsupported。
/// 3. **目标可读性**：executor 同款只读探针读既有目标文件（损坏 / IO）。
/// 4. **`${input:<id>}` 可达**：引用的 input id 须在已声明 input 集（`snapshot.inputs`）内；`${env:}` 仅语法。
///
/// 全程零写。返回预测的 `planned` + 汇总的 `diagnostics`。`ConfigContext.opts.upsert_new_scope` 决定新声明落点。
#[must_use]
pub fn preflight_mcp_import(ctx: &ConfigContext, servers: &[MCPServerConfig]) -> PreflightReport {
    let snapshot = load_config(ctx);
    let anchors = ctx.anchors();
    let declared_inputs: HashSet<String> = snapshot
        .inputs
        .inputs
        .iter()
        .map(|i| i.id().to_string())
        .collect();

    let mut diagnostics: Vec<SettingsValidationError> = Vec::new();
    let mut planned: Vec<PlannedServer> = Vec::new();
    let mut target_files: Vec<PathBuf> = Vec::new();

    for server in servers {
        let name = server.name().to_string();
        let body = canonicalize_persist_body(serde_json::to_value(server).unwrap_or(Value::Null));
        let entity = ConfigEntity::McpServer(name.clone());
        let intent = EditIntent::Upsert(body.clone());

        // (2) write-target（纯函数）：只读 origin / synthesized / unsupported → 诊断、不计入 planned。
        let plans = match resolve_write_target(&entity, &intent, &snapshot, &anchors, &ctx.opts) {
            Ok(p) => p,
            Err(e) => {
                diagnostics.push(write_target_diag(
                    &name,
                    &e,
                    ctx.opts.upsert_new_scope.into(),
                ));
                continue;
            }
        };
        let scope_ss = plans
            .first()
            .map(|p| p.scope)
            .unwrap_or_else(|| ctx.opts.upsert_new_scope.into());
        let write_scope = to_write_scope(scope_ss, ctx.opts.upsert_new_scope);
        target_files.extend(plans.iter().map(|p| p.file.clone()));

        // (1) schema：按 planned scope 校验（与读侧同一道门）。
        let (resolved, errs) = validate_server(&name, &body, scope_ss, None);
        diagnostics.extend(errs);

        // (4) `${input:}` 引用可达（纯语法扫描，不取真实值）。
        diagnostics.extend(input_ref_diags(&name, &body, &declared_inputs, scope_ss));

        if let Some(r) = resolved {
            planned.push(PlannedServer {
                name: name.clone(),
                scope: write_scope,
                bundle_id: resolve_bundle_id(&r.config),
            });
        }
    }

    // (3) 目标可读性：executor 同款只读探针（零写）——既有目标损坏 / IO 确定性暴露。
    for file in unique(target_files) {
        if let Err(e) = read_raw_object(&file) {
            diagnostics.push(SettingsValidationError {
                scope: ctx.opts.upsert_new_scope.into(),
                field: "<file>".to_string(),
                reason: format!("import target unreadable/corrupt: {e}"),
                source_path: Some(file.to_string_lossy().into_owned()),
            });
        }
    }

    PreflightReport {
        diagnostics,
        planned,
    }
}

// ===========================================================================
// import（全有或全无）/ import (all-or-nothing)
// ===========================================================================

/// **typed MCP import（全有或全无）**：preflight 干净后两阶段原子落盘 / typed import (all-or-nothing).
///
/// 先 [`preflight_mcp_import`]（零写）——`diagnostics` 非空即 [`ImportError::Preflight`] **零写**返回；
/// 干净则构造 edits 经 [`update_config`] 两阶段原子落盘（任一实体消解失败 → 整批零写，对齐既有 abort-on-error）。
/// SDK 决定序列化（`canonicalize_persist_body`）、provenance 与 write-target（`opts.upsert_new_scope`）。
///
/// **原子性边界**：上述「全有或全无」对**确定性**失败成立（preflight 已挡 schema / write-target / 目标可读 /
/// 引用可达，`update_config` phase 1 挡消解期错）；**非确定性执行期 I/O 失败**（磁盘满 / 权限 / 与外部写者竞态）
/// 发生时，executor 顺序写**不回滚**前序已落盘条目（FS 固有限制、非事务性），此时可能留下部分写 + `Err(Persist)`。
///
/// **不 mount / 不 render 取值**（运行期物化归 `Computer::mount_server`，本 API 守 #107 config 边界）。
pub fn import_mcp_servers(
    ctx: &ConfigContext,
    servers: &[MCPServerConfig],
) -> Result<Vec<PlannedServer>, ImportError> {
    let report = preflight_mcp_import(ctx, servers);
    if !report.is_clean() {
        return Err(ImportError::Preflight(report));
    }
    let edits: Vec<ConfigEdit> = servers
        .iter()
        .map(|s| {
            let name = s.name().to_string();
            let body = canonicalize_persist_body(serde_json::to_value(s).unwrap_or(Value::Null));
            ConfigEdit::new(ConfigEntity::McpServer(name), EditIntent::Upsert(body))
        })
        .collect();
    update_config(ctx, &edits).map_err(ImportError::Persist)?;
    Ok(report.planned)
}

// ===========================================================================
// 内部助手 / Internal helpers
// ===========================================================================

/// 构造一条诊断 / build one diagnostic.
fn diag(scope: SettingsScope, field: &str, reason: impl Into<String>) -> SettingsValidationError {
    SettingsValidationError {
        scope,
        field: field.to_string(),
        reason: reason.into(),
        source_path: None,
    }
}

/// write-target 错误 → 诊断（scope 取 planned default，read-only origin 入 reason）/ translate.
fn write_target_diag(
    name: &str,
    e: &WriteTargetError,
    default_scope: SettingsScope,
) -> SettingsValidationError {
    let reason = match e {
        WriteTargetError::ReadOnlyOrigin { origin, .. } => {
            format!("cannot import: existing declaration at read-only origin {origin:?}")
        }
        WriteTargetError::Synthesized { .. } => {
            "cannot import: server is plugin-bundled (operate via the owning plugin)".to_string()
        }
        WriteTargetError::Unsupported { reason, .. } => format!("cannot import: {reason}"),
    };
    diag(default_scope, &format!("servers.{name}"), reason)
}

/// `${input:<id>}` 引用扫描 → 不可达诊断（不取真实值；文法权威 `mcp_clients::render`）/ flag undeclared refs.
fn input_ref_diags(
    name: &str,
    body: &Value,
    declared: &HashSet<String>,
    scope: SettingsScope,
) -> Vec<SettingsValidationError> {
    crate::mcp_clients::render::collect_input_placeholder_ids(body)
        .into_iter()
        .filter(|id| !declared.contains(id))
        .map(|id| {
            diag(
                scope,
                &format!("servers.{name}"),
                format!("references undeclared input '{id}' (declare it in mcp.json inputs first)"),
            )
        })
        .collect()
}

/// SettingsScope → WriteScope（writable 三者；其余退回 default）/ convert.
fn to_write_scope(s: SettingsScope, default: WriteScope) -> WriteScope {
    match s {
        SettingsScope::User => WriteScope::User,
        SettingsScope::Project => WriteScope::Project,
        SettingsScope::Local => WriteScope::Local,
        _ => default,
    }
}

/// 去重保序 / dedup preserving order.
fn unique(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    paths
        .into_iter()
        .filter(|p| seen.insert(p.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::config::ConfigContext;
    use crate::settings::scope::EnvMap;
    use serde_json::json;
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    fn xdg_env(tmp: &TempDir) -> EnvMap {
        std::iter::once((
            "XDG_CONFIG_HOME".to_string(),
            tmp.path().join("xdg").to_string_lossy().into_owned(),
        ))
        .collect()
    }

    /// 构造 stdio server（命令可带 input 引用）/ build a stdio server.
    fn stdio(name: &str, command: &str) -> MCPServerConfig {
        serde_json::from_value(json!({
            "type": "stdio",
            "name": name,
            "server_parameters": {"command": command},
        }))
        .unwrap()
    }

    /// stdio + env map（env 嵌在 `server_parameters.env`，对齐 StdioServerParameters）/ stdio with env.
    fn stdio_with_env(name: &str, env: serde_json::Value) -> MCPServerConfig {
        serde_json::from_value(json!({
            "type": "stdio",
            "name": name,
            "server_parameters": {"command": "x", "env": env},
        }))
        .unwrap()
    }

    /// 隔离的 ConfigContext（managed 指向调用方持有的路径，通常是不存在文件）/ isolated context.
    fn ctx<'a>(config_dir: &'a Path, env: &'a EnvMap, managed: &'a Path) -> ConfigContext<'a> {
        ConfigContext {
            config_dir,
            env: Some(env),
            home: None,
            flag_settings_path: None,
            flag_mcp_config_path: None,
            managed_mcp_path: Some(managed),
            platform: None,
            policy_settings: None,
            embed_servers: &[],
            opts: Default::default(),
        }
    }

    /// #151 Part 2：preflight 零写——`.tfrobot/` 前后都不存在（磁盘无任何新声明）。
    #[test]
    fn preflight_is_zero_write_151() {
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        let env = xdg_env(&tmp);
        let managed = tmp.path().join("no-managed.json");
        let tfrobot = wd.join(".tfrobot");
        let c = ctx(&wd, &env, &managed);

        assert!(!tfrobot.exists(), "前置：.tfrobot 不存在");
        let report = preflight_mcp_import(&c, &[stdio("new-srv", "cmd")]);
        assert!(
            !tfrobot.exists(),
            "preflight MUST 零写（.tfrobot 不应被创建）"
        );
        // planned 预测有该项（零写预览），但磁盘无物。
        assert!(
            report.planned.iter().any(|p| p.name == "new-srv"),
            "preflight 预测落盘项"
        );
        assert!(
            report.is_clean(),
            "合法 server preflight 应干净（实得 {:?}）",
            report.diagnostics
        );
    }

    /// #151 Part 2：preflight 标确定性错——只读 origin（policy 已声明）/ `${input:}` 不可达。
    /// （typed 入口经类型系统已挡非法 type，schema 门作 defense-in-depth，故此处覆盖真实可达错类。）
    #[test]
    fn preflight_flags_readonly_origin_and_unreachable_input_151() {
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        let env = xdg_env(&tmp);
        // policy(managed) 已声明 `locked`（只读 origin）——upsert 须报 ReadOnlyOrigin。
        let managed = tmp.path().join("managed.json");
        write(
            &managed,
            r#"{"servers": {"locked": {"type":"stdio","server_parameters":{"command":"m"}}}}"#,
        );
        let c = ctx(&wd, &env, &managed);

        let report = preflight_mcp_import(
            &c,
            &[
                stdio("locked", "try-override"), // 只读 origin
                stdio_with_env("ref-bad", json!({"X": "${input:nope}"})), // 不可达 input
            ],
        );
        assert!(
            report
                .diagnostics
                .iter()
                .any(|d| d.field == "servers.locked" && d.reason.contains("read-only origin")),
            "只读 origin MUST 报（实得 {:?}）",
            report.diagnostics
        );
        assert!(
            report
                .diagnostics
                .iter()
                .any(|d| d.field == "servers.ref-bad" && d.reason.contains("undeclared input")),
            "${{input:}} 不可达 MUST 报（实得 {:?}）",
            report.diagnostics
        );
    }

    /// #151 Part 2：import 全有或全无——批内 1 非法 + N 合法 → Err(Preflight)、磁盘零新增。
    #[test]
    fn import_all_or_nothing_one_bad_writes_none_151() {
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        let env = xdg_env(&tmp);
        let managed = tmp.path().join("no-managed.json");
        let tfrobot = wd.join(".tfrobot");
        let c = ctx(&wd, &env, &managed);

        // bad = 不可达 input（确定性错）；good 合法。
        let bad = stdio_with_env("bad", json!({"X": "${input:ghost}"}));
        let good = stdio("good", "cmd");
        let res = import_mcp_servers(&c, &[bad, good]);

        assert!(matches!(res, Err(ImportError::Preflight(_))), "全有或全无");
        assert!(
            !tfrobot.exists(),
            "全有或全无：任一非法 → 零写（.tfrobot 不应被创建）"
        );
    }

    /// #151 Part 2：import 成功——合法 batch 落盘、planned 回执、磁盘可见。
    #[test]
    fn import_persists_clean_batch_151() {
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        let env = xdg_env(&tmp);
        let managed = tmp.path().join("no-managed.json");
        let project_mcp = crate::settings::mcp_config::workdir_mcp_config_path(&wd);
        let c = ctx(&wd, &env, &managed); // default upsert_new_scope = Project

        let planned = import_mcp_servers(&c, &[stdio("alpha", "a"), stdio("beta", "b")])
            .expect("clean batch imports");
        assert_eq!(planned.len(), 2);
        // 落盘可见（project mcp.json）。
        let on_disk: Value =
            serde_json::from_str(&fs::read_to_string(&project_mcp).unwrap()).unwrap();
        assert!(on_disk["servers"]["alpha"].is_object());
        assert!(on_disk["servers"]["beta"].is_object());
    }

    /// #151 Part 2：不同 origin——既有 Local server 的 update 落其 origin；新 server 落 upsert_new_scope。
    #[test]
    fn import_different_origins_update_and_add_151() {
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        let env = xdg_env(&tmp);
        let managed = tmp.path().join("no-managed.json");
        let local = crate::settings::mcp_config::workdir_mcp_local_config_path(&wd);
        let project_mcp = crate::settings::mcp_config::workdir_mcp_config_path(&wd);
        // 既有 local 声明 `existing`。
        write(
            &local,
            r#"{"servers": {"existing": {"type":"stdio","server_parameters":{"command":"old"}}}}"#,
        );
        // 新 server 落 Project（default upsert_new_scope）。
        let mut c = ctx(&wd, &env, &managed);
        c.opts.upsert_new_scope = WriteScope::Project;

        let planned =
            import_mcp_servers(&c, &[stdio("existing", "updated"), stdio("fresh", "new")])
                .expect("import ok");
        let scopes: Vec<(&String, &WriteScope)> =
            planned.iter().map(|p| (&p.name, &p.scope)).collect();
        // existing（update）落其 origin Local；fresh（新）落 Project。
        assert!(scopes.contains(&(&"existing".to_string(), &WriteScope::Local)));
        assert!(scopes.contains(&(&"fresh".to_string(), &WriteScope::Project)));
        // 两者落盘到各自 scope 文件。
        let local_disk: Value = serde_json::from_str(&fs::read_to_string(&local).unwrap()).unwrap();
        assert_eq!(
            local_disk["servers"]["existing"]["server_parameters"]["command"],
            "updated"
        );
        let proj_disk: Value =
            serde_json::from_str(&fs::read_to_string(&project_mcp).unwrap()).unwrap();
        assert!(proj_disk["servers"]["fresh"].is_object());
    }

    /// #151 Part 2（守验收⑤）：preflight 不取真实值——`${input:declared}`（可达，不报）+ `${env:X}`（语法过、不探测存在）。
    #[test]
    fn import_respects_107_no_value_resolution_151() {
        let tmp = TempDir::new().unwrap();
        let wd = tmp.path().join("wd");
        let env = xdg_env(&tmp);
        let managed = tmp.path().join("no-managed.json");
        let user_mcp = crate::settings::mcp_config::user_mcp_config_path(Some(&env));
        // 声明 input `tok`（user mcp.json）。
        write(
            &user_mcp,
            r#"{"inputs": [{"type":"PromptString","id":"tok","description":"d"}]}"#,
        );
        let c = ctx(&wd, &env, &managed);

        let report = preflight_mcp_import(
            &c,
            &[stdio_with_env(
                "ref-srv",
                json!({"TOK": "${input:tok}", "AMBIENT": "${env:DEFINITELY_UNSET_VAR}"}),
            )],
        );
        // ${input:tok} 可达（tok 已声明）→ 不报；${env:} 仅语法、不探测存在 → 不报。整体干净。
        assert!(
            report.is_clean(),
            "preflight MUST 不取真实值/不探测 env 存在（实得 {:?}）",
            report.diagnostics
        );
    }
}
