//! #115 / S8 —— 集成回归守护（依赖图末端唯一汇聚点）。
//!
//! 纯测试子任务：守护跨 sub-task（S1–S7）边界的不变量，逐条映射 #107 验收项（按 D1 订正后）。
//! 与各 sub-task 的**单元**测试互补——这里只走**端到端 / 跨层**路径（config CRUD ↔ runtime ↔ 磁盘 ↔
//! 重投影），复现真实使用序而非重测单元。
//!
//! 覆盖矩阵（← #115 验收 + S6/S7 隔离审查延后项）：
//! 1. config CRUD roundtrip（runtime mutate → 落盘 → fresh reload 保真）
//! 2. migration 幂等
//! 3. `validate_config` schema-only（不探测外部环境）
//! 4. D1 inputs 边界 + import/export 不泄 secret（引用留、明文脱敏）
//! 5. disable≠remove（override 落 local、声明不动、不 bump config revision）
//! 6. 只读 scope（policy origin）→ 结构化 `ReadOnlyOrigin`，整批零落盘
//! 7. runtime mutate → 事件 + revision（`config_revision ⊥ capability_revision`，§12 R2）
//! 8. R2（S6 审查）：Http server 落盘 `type=="streamable"` 全链路往返
//! 9. enable/disable → resolved-scope 落盘（非恒定 user）+ R1（S6 审查）幂等重放不虚假 bump
//! 10. lifecycle 不变量（boot/shutdown 终态 + 未连接 gate）
//! 11. 跨-SDK 快照 fixture round-trip **桩**（python 未实现，守护 schema 漂移）

use std::collections::HashMap;
use std::path::Path;

use tempfile::TempDir;

use smcp_computer::computer::{Computer, SilentSession};
use smcp_computer::errors::ComputerError;
use smcp_computer::mcp_clients::bundle_id::resolve_bundle_id;
use smcp_computer::mcp_clients::model::{
    HttpServerConfig, HttpServerParameters, MCPServerConfig, StdioServerConfig,
    StdioServerParameters,
};
use smcp_computer::settings::config::{
    export_config, import_config, load_config, migrate_config, update_config, validate_config,
    ConfigContext, ConfigCrudError, ConfigEdit, ConfigEntity, EditIntent, ProjectConfigDoc,
    ProvenanceScope, WriteTargetError, REDACTED_PLACEHOLDER,
};
use smcp_computer::settings::mcp_config::{
    user_mcp_config_path, workdir_mcp_config_path, workdir_mcp_local_config_path,
};
use smcp_computer::settings::scope::{
    workdir_local_settings_path, workdir_project_settings_path, EnvMap,
};
use smcp_computer::settings::store::installed_plugins_path;
use smcp_computer::{ComputerEvent, LifecycleState};

use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// 公共脚手架 / shared harness
// ---------------------------------------------------------------------------

/// 隔离一台待 boot 的 Computer（skill_home / blob / config_dir 全注入 tempdir，绝不污染仓库工作树）。
/// 镜像 `tests/computer_integration.rs::isolate_boot`。
fn isolate_boot(c: Computer<SilentSession>, td: &TempDir) -> Computer<SilentSession> {
    c.with_skill_home(td.path().join("skills"))
        .with_blob_cache_root(td.path().join("blob"))
        .with_config_dir(td.path().join("config"))
}

/// 该 tempdir 的 config 锚点（= isolate_boot 注入的 `with_config_dir`）。
fn config_dir_of(td: &TempDir) -> std::path::PathBuf {
    td.path().join("config")
}

/// 写 JSON 文本到 path（自动建父目录）。
fn seed(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

/// 读回 path 的 JSON。
fn read_json(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// 最小合法 stdio server。
fn stdio(name: &str) -> MCPServerConfig {
    MCPServerConfig::Stdio(StdioServerConfig::new(
        name,
        StdioServerParameters {
            command: "echo".to_string(),
            args: vec!["hi".to_string()],
            env: HashMap::new(),
            cwd: None,
        },
    ))
}

/// 最小合法 Http（streamable）server。
fn http(name: &str, url: &str) -> MCPServerConfig {
    MCPServerConfig::Http(HttpServerConfig::new(
        name,
        HttpServerParameters {
            url: url.to_string(),
            headers: HashMap::new(),
        },
    ))
}

// ---------------------------------------------------------------------------
// 1. config CRUD roundtrip：runtime mutate → 落盘 → fresh reload 保真
// ---------------------------------------------------------------------------

#[tokio::test]
async fn runtime_add_server_persists_and_survives_fresh_reload() {
    let td = TempDir::new().unwrap();
    let cd = config_dir_of(&td);
    let computer = isolate_boot(
        Computer::new("c", SilentSession::new("s"), None, None, false, false),
        &td,
    );
    computer.boot_up().await.unwrap();

    computer.add_or_update_server(stdio("srv1")).await.unwrap();

    // 全新 ConfigContext（不复用 Computer 内存态）从 config 锚点重投影 → server 仍在（跨进程重启保真）。
    let snap = load_config(&ConfigContext::new(&cd));
    assert!(
        snap.mcp.servers.iter().any(|s| s.name == "srv1"),
        "add_or_update_server 落盘后必须能被独立 load_config 读回"
    );
    // 盘上判别符是协议 §9.1 规范小写 stdio（跨 SDK 可读），非 Rust 变体名 Stdio。
    let disk = read_json(&workdir_mcp_config_path(&cd));
    assert_eq!(disk["servers"]["srv1"]["type"], json!("stdio"));
    assert!(
        disk["servers"]["srv1"].get("name").is_none(),
        "内嵌 name 应被剥（map key 即身份）"
    );

    computer.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// 2. migration 幂等
// ---------------------------------------------------------------------------

#[test]
fn migrate_config_is_idempotent() {
    let td = TempDir::new().unwrap();
    let cd = td.path().join("config");
    seed(
        &workdir_mcp_config_path(&cd),
        r#"{"servers": {"srv": {"type": "stdio", "server_parameters": {"command": "p"}}}}"#,
    );

    // 首次迁移可能规范化（true/false 取决于种子是否已规范）；关键不变量=**再跑一次不再改**（幂等）。
    let _first = migrate_config(&cd).unwrap();
    let second = migrate_config(&cd).unwrap();
    assert!(!second, "migrate_config 必须幂等：第二次运行零改动");
}

// ---------------------------------------------------------------------------
// 3. validate_config schema-only（不探测外部环境）
// ---------------------------------------------------------------------------

#[test]
fn validate_config_is_schema_only_never_probes_environment() {
    // 合法 schema → 通过。
    let ok = ProjectConfigDoc {
        mcp: Some(
            json!({"servers": {"srv": {"type": "stdio", "server_parameters": {"command": "x"}}}})
                .as_object()
                .unwrap()
                .clone(),
        ),
        ..Default::default()
    };
    assert!(validate_config(&ok).is_valid());

    // 非法 enum（type）→ 拒（schema 校验）。
    let bad = ProjectConfigDoc {
        mcp: Some(
            json!({"servers": {"srv": {"type": "carrier-pigeon"}}})
                .as_object()
                .unwrap()
                .clone(),
        ),
        ..Default::default()
    };
    assert!(
        !validate_config(&bad).is_valid(),
        "非法 transport type 必须报 schema 错"
    );

    // schema-only 铁律：command 指向绝不存在的可执行文件，仍**通过**（validate 不探测 PATH / 文件系统）。
    let unprobed = ProjectConfigDoc {
        mcp: Some(
            json!({"servers": {"srv": {"type": "stdio", "server_parameters": {
                "command": "this-binary-does-not-exist-anywhere-xyzzy"
            }}}})
            .as_object()
            .unwrap()
            .clone(),
        ),
        ..Default::default()
    };
    assert!(
        validate_config(&unprobed).is_valid(),
        "validate_config 不得探测外部环境可用性（#107 §4.1）"
    );
}

// ---------------------------------------------------------------------------
// 4. D1 inputs 边界 + import/export 不泄 secret
// ---------------------------------------------------------------------------

#[test]
fn export_redacts_plaintext_secret_keeps_input_ref_and_drops_local_then_import_roundtrips() {
    let src = TempDir::new().unwrap();
    let scd = src.path().join("config");
    // project mcp.json：混合明文 secret（须脱敏）+ ${input:*} 引用（D1：引用是定义、须留）+ 普通值。
    seed(
        &workdir_mcp_config_path(&scd),
        r#"{"servers": {"db": {"type": "stdio", "server_parameters": {
            "command": "run",
            "env": {"API_KEY": "sk-live-super-secret", "REF": "${input:tok}", "MODE": "prod"}
        }}}}"#,
    );
    // client-owned local 层：export 必须整层丢弃（不外带 local override）。
    seed(
        &workdir_mcp_local_config_path(&scd),
        r#"{"servers": {"db-local": {"type": "stdio", "server_parameters": {"command": "secret-local"}}}}"#,
    );

    let exported = export_config(&scd).unwrap();
    let env = &exported.mcp.as_ref().unwrap()["servers"]["db"]["server_parameters"]["env"];
    assert_eq!(
        env["API_KEY"],
        json!(REDACTED_PLACEHOLDER),
        "明文 secret 必须脱敏"
    );
    assert_eq!(
        env["MODE"],
        json!(REDACTED_PLACEHOLDER),
        "非引用普通值保守脱敏"
    );
    assert_eq!(
        env["REF"],
        json!("${input:tok}"),
        "input 引用（定义）逐字保留、不脱敏"
    );
    assert!(
        exported.mcp_local.is_none(),
        "export 丢弃 client-owned local 层"
    );

    // 往返：导入到全新 config_dir → 校验通过 + 盘上仍无明文。
    let dst = TempDir::new().unwrap();
    let dcd = dst.path().join("config");
    let report = import_config(&dcd, &exported).unwrap();
    assert!(report.is_valid(), "脱敏后的 doc 导入应通过 schema 校验");
    let on_disk = read_json(&workdir_mcp_config_path(&dcd));
    let denv = &on_disk["servers"]["db"]["server_parameters"]["env"];
    assert_eq!(denv["API_KEY"], json!(REDACTED_PLACEHOLDER));
    assert!(
        !std::fs::read_to_string(workdir_mcp_config_path(&dcd))
            .unwrap()
            .contains("sk-live-super-secret"),
        "导入落盘绝不含明文 secret"
    );
}

// ---------------------------------------------------------------------------
// 5. disable≠remove（override 落 local、声明不动、不 bump config revision）
// ---------------------------------------------------------------------------

#[test]
fn disable_is_override_not_removal_and_does_not_bump_config_revision() {
    let td = TempDir::new().unwrap();
    let cd = td.path().join("config");
    seed(
        &workdir_mcp_config_path(&cd),
        r#"{"servers": {"srv": {"type": "stdio", "server_parameters": {"command": "p"}}}}"#,
    );
    let ctx = ConfigContext::new(&cd);

    let before = load_config(&ctx).revision;
    let after_disable = update_config(
        &ctx,
        &[ConfigEdit::new(
            ConfigEntity::McpServer("srv".into()),
            EditIntent::Disable,
        )],
    )
    .unwrap();

    // override 落 local settings；声明（mcp.json）纹丝不动。
    assert_eq!(
        read_json(&workdir_local_settings_path(&cd)),
        json!({"disabledMcpjsonServers": ["srv"]}),
        "disable 写 local override"
    );
    assert!(
        read_json(&workdir_mcp_config_path(&cd))["servers"]
            .get("srv")
            .is_some(),
        "disable 不删声明（disable≠remove）"
    );
    assert_eq!(
        after_disable.revision, before,
        "MCP disable=gating，不改 config revision（§12 R2）"
    );

    // Remove 则删声明、内容真变 → revision 变。
    let after_remove = update_config(
        &ctx,
        &[ConfigEdit::new(
            ConfigEntity::McpServer("srv".into()),
            EditIntent::Remove,
        )],
    )
    .unwrap();
    assert!(
        read_json(&workdir_mcp_config_path(&cd))["servers"]
            .get("srv")
            .is_none(),
        "remove 删声明"
    );
    assert_ne!(
        after_remove.revision, before,
        "remove 改声明 → config revision 变"
    );
}

// ---------------------------------------------------------------------------
// 6. 只读 scope（policy origin）→ ReadOnlyOrigin，整批零落盘
// ---------------------------------------------------------------------------

#[test]
fn readonly_policy_origin_upsert_and_remove_abort_batch_zero_write() {
    let td = TempDir::new().unwrap();
    let cd = td.path().join("config");
    let managed = td.path().join("managed-mcp.json");
    seed(
        &managed,
        r#"{"servers": {"srv-pol": {"type": "stdio", "server_parameters": {"command": "p"}}}}"#,
    );
    // 注入 policy scope（managed_mcp_path）→ srv-pol origin=Policy（只读、最高）。
    let ctx = ConfigContext {
        managed_mcp_path: Some(&managed),
        ..ConfigContext::new(&cd)
    };

    // Upsert 改只读 origin server → 结构化 ReadOnlyOrigin。
    let err = update_config(
        &ctx,
        &[ConfigEdit::new(
            ConfigEntity::McpServer("srv-pol".into()),
            EditIntent::Upsert(json!({"type": "stdio", "server_parameters": {"command": "q"}})),
        )],
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            ConfigCrudError::WriteTarget(WriteTargetError::ReadOnlyOrigin {
                origin: ProvenanceScope::Policy,
                ..
            })
        ),
        "policy origin 上 Upsert 必须报 ReadOnlyOrigin，got {err:?}"
    );

    // Remove 只读 origin server 同样报错；且合法首条也不落盘（整批零写）。
    let err2 = update_config(
        &ctx,
        &[
            ConfigEdit::new(
                ConfigEntity::McpServer("fresh".into()),
                EditIntent::Upsert(json!({"type": "stdio", "server_parameters": {"command": "z"}})),
            ),
            ConfigEdit::new(
                ConfigEntity::McpServer("srv-pol".into()),
                EditIntent::Remove,
            ),
        ],
    )
    .unwrap_err();
    assert!(matches!(
        err2,
        ConfigCrudError::WriteTarget(WriteTargetError::ReadOnlyOrigin { .. })
    ));
    assert!(
        !workdir_mcp_config_path(&cd).exists(),
        "整批放弃 → 合法首条也零落盘（不半改）"
    );
}

// ---------------------------------------------------------------------------
// 7. runtime mutate → 事件 + revision（config ⊥ capability，§12 R2）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn runtime_mutation_bumps_config_and_capability_independently_and_emits_events() {
    let td = TempDir::new().unwrap();
    let computer = isolate_boot(
        Computer::new("c", SilentSession::new("s"), None, None, false, false),
        &td,
    );
    computer.boot_up().await.unwrap();
    // boot 只 bump capability，不 bump config。
    assert_eq!(
        computer.config_revision(),
        0,
        "boot 不 bump config revision"
    );
    let cap0 = computer.capability_revision();

    let mut rx = computer.subscribe_events();
    computer.add_or_update_server(stdio("s7")).await.unwrap();

    // add 新 server：config（声明变）+ capability（挂载）双 bump——两条独立单调计数（§12 R2）。
    assert_eq!(
        computer.config_revision(),
        1,
        "add 新 server → config revision +1"
    );
    assert!(
        computer.capability_revision() > cap0,
        "add 新 server → capability revision +"
    );

    // 广播出现 config + capability 两类事件（robot 同步的进程内信号；连线态经此驱动 server:update_config）。
    let mut saw_config = false;
    let mut saw_cap = false;
    while let Ok(ev) = rx.try_recv() {
        match ev {
            ComputerEvent::ConfigRevisionBumped { revision } => {
                assert_eq!(revision, 1);
                saw_config = true;
            }
            ComputerEvent::CapabilityRevisionBumped { .. } => saw_cap = true,
            ComputerEvent::LifecycleChanged { .. } => {}
        }
    }
    assert!(saw_config, "config mutate 必广播 ConfigRevisionBumped");
    assert!(saw_cap, "挂载必广播 CapabilityRevisionBumped");

    // 幂等重放（同内容再 add）：config 内容未变 → 不虚假 bump（对齐 R1 语义）。
    let cfg_before = computer.config_revision();
    computer.add_or_update_server(stdio("s7")).await.unwrap();
    assert_eq!(
        computer.config_revision(),
        cfg_before,
        "同内容重放 → config revision 不虚假 bump"
    );

    computer.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// 8. R2（S6 审查）：Http server 落盘 type=="streamable" 全链路往返
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_server_persists_as_streamable_token_and_roundtrips_back_to_http() {
    let td = TempDir::new().unwrap();
    let cd = config_dir_of(&td);
    let computer = isolate_boot(
        Computer::new("c", SilentSession::new("s"), None, None, false, false),
        &td,
    );
    computer.boot_up().await.unwrap();

    computer
        .add_or_update_server(http("hsrv", "https://example.test/mcp"))
        .await
        .unwrap();

    // 盘上判别符=协议 §9.1 规范 token "streamable"（对齐 Python Literal，跨 SDK 可读），非 Rust 变体名 Http。
    let disk = read_json(&workdir_mcp_config_path(&cd));
    assert_eq!(disk["servers"]["hsrv"]["type"], json!("streamable"));

    // 全链路往返：独立 load_config（内部读 alias="streamable"）读回 → 仍是 Http 变体。
    let snap = load_config(&ConfigContext::new(&cd));
    let view = snap
        .mcp
        .servers
        .iter()
        .find(|s| s.name == "hsrv")
        .expect("hsrv 应可被读回");
    assert!(
        matches!(view.config, MCPServerConfig::Http(_)),
        "streamable token 往返后应解析回 MCPServerConfig::Http"
    );

    computer.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// 9. enable/disable → resolved-scope 落盘（非恒定 user）+ R1 幂等重放不虚假 bump
// ---------------------------------------------------------------------------

#[tokio::test]
async fn plugin_disable_resolves_scope_from_install_record_and_r1_no_spurious_bump() {
    let td = TempDir::new().unwrap();
    let home = td.path().join("skills");
    let proj = td.path().join("proj");
    std::fs::create_dir_all(&home).unwrap();

    // 防御性隔离：注入 env 使**任何** scope 解析结局都锁在 tempdir——若 resolve 万一回退到 user（如账本缺
    // scope 字段），user_settings 也落 <xdg>/a2c 而非开发者真实 ~/.config（对齐 S6 审查的 cwd 污染教训：
    // 回归守卫测试自身的 hermetic 绝不依赖被测项的正确性）。
    let env: EnvMap = [
        (
            "XDG_CONFIG_HOME".to_string(),
            td.path().join("xdg").to_string_lossy().into_owned(),
        ),
        (
            "HOME".to_string(),
            td.path().join("home").to_string_lossy().into_owned(),
        ),
    ]
    .into_iter()
    .collect();

    // 账本记录：p@mp 安装于 scope=project（**非** user）——scope 消解应据此，非恒定 user。
    let ledger = installed_plugins_path(Some(&home), Some(&env));
    seed(
        &ledger,
        r#"{"plugins": {"p@mp": [{"installPath": "x", "scope": "project"}]}}"#,
    );

    let computer = Computer::new("c", SilentSession::new("s"), None, None, false, false)
        .with_skill_home(&home);
    let proj_str = proj.to_string_lossy().into_owned();
    let opts = || smcp_computer::settings::installer::DisableOptions {
        scope: None, // ← 缺省，须从账本记录消解为 "project"
        project_path: Some(&proj_str),
        env: Some(&env),
    };

    // 首次 disable：enabledPlugins 内容真变 → config revision 0→1。
    computer.disable_plugin("p@mp", opts(), None).await.unwrap();
    assert_eq!(
        computer.config_revision(),
        1,
        "首次 disable → config revision +1"
    );

    // 落点=project scope 声明文件（非 user），证明 scope 由账本记录消解、非恒定 user。
    let proj_settings = read_json(&workdir_project_settings_path(&proj));
    assert_eq!(
        proj_settings["enabledPlugins"]["p@mp"],
        json!(false),
        "enabledPlugins 应落 resolved(project) scope"
    );

    // R1（S6 审查，方案 A）：幂等重放（重复 disable，内容不变）→ **不虚假 bump**、不惊动 robot。
    computer.disable_plugin("p@mp", opts(), None).await.unwrap();
    assert_eq!(
        computer.config_revision(),
        1,
        "幂等重复 disable → config revision 不虚假 bump（R1）"
    );
}

// ---------------------------------------------------------------------------
// 10. lifecycle 不变量（boot/shutdown 终态 + 未连接 gate）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lifecycle_boot_shutdown_terminal_and_notconnected_gates() {
    let td = TempDir::new().unwrap();
    let computer = isolate_boot(
        Computer::new("c", SilentSession::new("s"), None, None, false, false),
        &td,
    );
    assert_eq!(computer.lifecycle_state(), LifecycleState::Created);

    computer.boot_up().await.unwrap();
    let snap = computer.status().await;
    assert_eq!(snap.lifecycle, LifecycleState::Started);
    assert_eq!(snap.config_revision, 0);
    assert!(snap.capability_revision >= 1, "boot bump capability 一次");

    // 未连接 gate：join/leave 须报 InvalidState 且**不**改 lifecycle（连接态编排前置约束）。
    assert!(matches!(
        computer.join_office("office", "c").await,
        Err(ComputerError::InvalidState(_))
    ));
    assert!(matches!(
        computer.leave_office().await,
        Err(ComputerError::InvalidState(_))
    ));
    // disconnect 幂等：未连接时 Ok（client 本已 None），回 Started 对 Started 是 no-op（§4.5）。
    assert!(computer.disconnect_socketio().await.is_ok());
    assert_eq!(
        computer.lifecycle_state(),
        LifecycleState::Started,
        "未连接 gate / 幂等 disconnect 后 lifecycle 仍 Started"
    );

    // shutdown → Shutdown 终态。
    computer.shutdown().await.unwrap();
    assert_eq!(computer.lifecycle_state(), LifecycleState::Shutdown);

    // 终态闸门：shutdown 后 mutate 不得把 lifecycle 拉出 Shutdown（§4.7 CAS 永不离开终态）。
    let _ = computer.add_or_update_server(stdio("late")).await;
    assert_eq!(
        computer.lifecycle_state(),
        LifecycleState::Shutdown,
        "shutdown 后任何操作都不得离开 Shutdown 终态"
    );
}

// ---------------------------------------------------------------------------
// 11. 跨-SDK 快照 fixture round-trip 桩（守护 schema 漂移）
// ---------------------------------------------------------------------------

/// #115 明列：跨-SDK 快照 fixture round-trip **桩**（python 未实现，先置桩守护 schema 漂移）。
///
/// 真正的跨-SDK round-trip 需 python-sdk 产出对照 fixture；在其落地前，本桩把 Rust `ComputerConfigSnapshot`
/// 序列化的**顶层 schema 形态**钉死——任一字段增删/改名即失败，提示须同步跨-SDK fixture 与协议 schema。
#[test]
fn cross_sdk_snapshot_schema_shape_is_pinned_stub() {
    let td = TempDir::new().unwrap();
    let cd = td.path().join("config");
    seed(
        &workdir_mcp_config_path(&cd),
        r#"{"servers": {"srv": {"type": "stdio", "server_parameters": {"command": "p"}}}}"#,
    );

    let snap = load_config(&ConfigContext::new(&cd));
    let json = serde_json::to_value(&snap).unwrap();
    let obj = json.as_object().expect("snapshot 序列化为 object");

    let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec![
            "inputs",
            "marketplace",
            "mcp",
            "plugins",
            "provenance",
            "revision",
            "runtime",
            "skills",
            "version",
        ],
        "快照顶层 schema 形态漂移——须同步跨-SDK fixture 与协议 §2/§4"
    );
    // revision 是内容摘要（"sha256:<hex>"），跨-SDK 须逐字节一致的规范化前置。
    assert!(
        obj["revision"].as_str().unwrap().starts_with("sha256:"),
        "revision 应为 sha256 内容摘要"
    );
}

// ---------------------------------------------------------------------------
// #121：实例 User-config 上下文（env/home 注入）+ CRUD bundle_id 寻址
// ---------------------------------------------------------------------------

/// 实例 User-config 环境（XDG_CONFIG_HOME → 隔离目录，与宿主进程环境解耦）。
fn instance_env(td: &TempDir) -> EnvMap {
    let mut env = EnvMap::new();
    env.insert(
        "XDG_CONFIG_HOME".to_string(),
        td.path()
            .join("instance-xdg")
            .to_string_lossy()
            .into_owned(),
    );
    env
}

/// 指定 command 的最小 stdio server（区分 before/after 落盘内容）。
fn stdio_cmd(name: &str, command: &str) -> MCPServerConfig {
    MCPServerConfig::Stdio(StdioServerConfig::new(
        name,
        StdioServerParameters {
            command: command.to_string(),
            args: vec![],
            env: HashMap::new(),
            cwd: None,
        },
    ))
}

/// #121 A：`add_or_update_server` 落到**注入的实例 User scope**，不误落宿主 ambient / project。
///
/// 修复前（`config_env` 未接线）：env 被忽略 → 快照读宿主 ambient（无此唯一探针）→ 视为新 server → 落 project
/// scope（tempdir）→ 实例 User 文件保持 `before` → 断言失败（RED，且不污染真实 `~/.config`）。
#[tokio::test]
async fn issue121_add_or_update_targets_injected_user_scope_not_ambient() {
    let td = TempDir::new().unwrap();
    let env = instance_env(&td);
    let probe = "a2c-issue121-upsert-probe";
    let user_mcp = user_mcp_config_path(Some(&env));
    seed(
        &user_mcp,
        &format!(
            r#"{{"servers":{{"{probe}":{{"type":"stdio","server_parameters":{{"command":"before","args":[]}}}}}}}}"#
        ),
    );

    let computer = isolate_boot(
        Computer::new("c", SilentSession::new("s"), None, None, false, false),
        &td,
    )
    .with_config_env(env.clone());
    computer.boot_up().await.unwrap();

    computer
        .add_or_update_server(stdio_cmd(probe, "after"))
        .await
        .unwrap();

    let disk = read_json(&user_mcp);
    assert_eq!(
        disk["servers"][probe]["server_parameters"]["command"], "after",
        "add_or_update 必须更新注入的实例 User scope（env-context 未继承时会误落宿主 ambient / project）"
    );

    // inventory 亦以实例 env 解析；新增 bundle_id 字段（管理/删除寻址键）。
    let inv = computer.list_mcp_servers_with_metadata().await;
    let entry = inv
        .iter()
        .find(|e| e.name == probe)
        .expect("探针应出现在 inventory");
    assert_eq!(
        entry.bundle_id,
        resolve_bundle_id(&stdio_cmd(probe, "after")),
        "inventory 须暴露 bundle_id（= raw config 派生，与 manager 同键）"
    );
}

/// #121 A+B：`remove_server(bundle_id)` 从**注入的实例 User scope** 删声明，不误删宿主同名 MCP。
///
/// 唯一探针名杜绝命中真实用户 server；修复前 remove 目标为宿主 ambient → 探针不在其中 → 实例文件仍含探针（RED）。
#[tokio::test]
async fn issue121_remove_by_bundle_id_targets_injected_user_scope() {
    let td = TempDir::new().unwrap();
    let env = instance_env(&td);
    let probe = "a2c-issue121-rm-probe";
    let user_mcp = user_mcp_config_path(Some(&env));
    seed(
        &user_mcp,
        &format!(
            r#"{{"servers":{{"{probe}":{{"type":"stdio","server_parameters":{{"command":"x","args":[]}}}}}}}}"#
        ),
    );

    let computer = isolate_boot(
        Computer::new("c", SilentSession::new("s"), None, None, false, false),
        &td,
    )
    .with_config_env(env.clone());
    computer.boot_up().await.unwrap();

    let bid = resolve_bundle_id(&stdio_cmd(probe, "x"));
    computer.remove_server(&bid).await.unwrap();

    let disk = read_json(&user_mcp);
    assert!(
        disk["servers"].get(probe).is_none(),
        "remove_server(bundle_id) 必须从注入的实例 User scope 删声明（env-context 未继承时会误删宿主同名 MCP）"
    );
}

/// #121 B：CRUD 按 **bundle_id（软件唯一身份）** 寻址，非 name（协议 §身份 MUST 用 bundle_id）。
///
/// 用 name ≠ bundle_id 的 server（`my__server` 折叠 `__` → bundle_id `my_server`）验证：按 name 删是 no-op、
/// 按 bundle_id 删才真删——消除此前 name 寻址在同名 + 显式 bundle_id 时的非确定性。
#[tokio::test]
async fn issue121_remove_addresses_by_bundle_id_not_name() {
    let td = TempDir::new().unwrap();
    let cd = config_dir_of(&td);
    let computer = isolate_boot(
        Computer::new("c", SilentSession::new("s"), None, None, false, false),
        &td,
    );
    computer.boot_up().await.unwrap();

    let name = "my__server";
    computer
        .add_or_update_server(stdio_cmd(name, "echo"))
        .await
        .unwrap();
    let bid = resolve_bundle_id(&stdio_cmd(name, "echo"));
    assert_ne!(bid, name, "该名规范化后 bundle_id ≠ name（折叠连续 `_`）");

    // 按 name（非身份键）删 → no-op：声明仍在。
    computer.remove_server(name).await.unwrap();
    assert!(
        load_config(&ConfigContext::new(&cd))
            .mcp
            .servers
            .iter()
            .any(|s| s.name == name),
        "按 name（非身份键）删应 no-op"
    );

    // 按 bundle_id（身份键）删 → 真删。
    computer.remove_server(&bid).await.unwrap();
    assert!(
        !load_config(&ConfigContext::new(&cd))
            .mcp
            .servers
            .iter()
            .any(|s| s.name == name),
        "按 bundle_id（身份键）删应移除声明"
    );
}
