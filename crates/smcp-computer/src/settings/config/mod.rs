/*!
* 文件名: mod.rs
* 作者: JQQ
* 创建日期: 2026/07/10
* 最后修改日期: 2026/07/10
* 版权: 2023 JQQ. All rights reserved.
* 描述: #107 SDK-owned config 层（#108 S1 起）。统一 `ComputerConfig` 快照 + 多 scope reconcile 投影读入口。
*       SDK-owned config layer (from #108 S1): unified `ComputerConfig` snapshot + reconcile-projection read.
*
* 子任务地基 / foundation of #107 sub-tasks: S1 快照(本模块) → S2 写目标消解器 → S3 CRUD → …（见 design-107 §9）。
*/

pub mod crud;
pub mod executor;
pub mod import;
pub mod portability;
pub mod snapshot;
pub mod validate;
pub mod write_target;

pub use crud::{
    delete_config, duplicate_config, init_config, load_config, load_project_config_doc,
    save_config, update_config, ConfigContext, ConfigCrudError, ConfigEdit, ProjectConfigDoc,
};
pub use executor::{execute_write_plans, ExecutorError};
pub use import::{
    import_mcp_servers, preflight_mcp_import, ImportError, PlannedServer, PreflightReport,
};
pub use portability::{export_config, import_config, REDACTED_PLACEHOLDER};
pub use snapshot::{
    resolve_snapshot, ComputerConfigSnapshot, ConfigRevision, EntityKey, InputDefsView,
    MarketplaceGovView, MarketplaceView, McpConfigView, McpServerView, PluginConfigView,
    PluginEnablementView, PluginRecordView, ProvenanceScope, RuntimeDefaults, SkillConfigView,
    SnapshotArgs, SNAPSHOT_VERSION,
};
pub use validate::{migrate_config, validate_config, ValidationReport};
pub use write_target::{
    resolve_write_target, ConfigEntity, EditIntent, ScopeAnchors, WritePlan, WriteScope,
    WriteTargetError, WriteTargetOp, WriteTargetOptions,
};
