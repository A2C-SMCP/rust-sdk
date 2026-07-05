/*!
* 文件名: mod.rs
* 作者: JQQ
* 创建日期: 2026/06/03
* 最后修改日期: 2026/06/03
* 版权: 2023 JQQ. All rights reserved.
* 依赖: smcp::skill_name, smcp::utils::path, serde_json
* 描述: Computer 端 SKILL 治理子系统（命名 / Home 布局 / source 解析）
*       Computer-side SKILL governance subsystem (naming / home layout / source resolution).
*/

//! Computer 端 SKILL 治理子系统 / Computer-side SKILL governance subsystem。
//!
//! 对标 Python 治理层资产 `a2c_smcp/computer/skills/` / Mirrors the Python governance assets。
//! 本模块组装 SKILL 通道 Computer 端的底层基座，供 registry / staging / sandbox / reconciler 复用：
//!
//! - [`naming`]：桥接协议级命名 lexer（[`smcp::skill_name`]）+ source → name 合成链（SKL-01 / #40）。
//! - [`home`]：XDG 优先的 SKILL Home 解析与安装目录布局（SKL-03 / #46）。
//! - [`sources`]：plugin source 5 类 disjoint union 解析与简写糖归一化（SKL-03 / #46）。
//! - [`frontmatter`]：SKILL.md YAML frontmatter 解析/剥离（共享，#43/#49/#52）。
//! - [`registry`]：`name → A2CSkillRef` 物化索引 + 孤儿状态机（SKL-02 / #43）。
//! - [`watcher`] / [`debouncer`]：user 源文件 watch + 多源去抖结算（SKL-06 / #55）。

pub mod debouncer;
pub mod frontmatter;
pub mod home;
pub mod manifest;
pub mod naming;
pub mod registry;
pub mod resource;
pub mod sandbox;
pub mod sources;
pub mod staging;
pub mod watcher;

pub use naming::{
    is_valid_skill_name, normalize_mcp_server_segment, parse_skill_name,
    synthesize_marketplace_name, synthesize_mcp_name, synthesize_name, synthesize_user_name,
    ParsedSkillName, SkillNameError, SkillNameKind, SkillNameSpec, MAX_SEGMENT_LEN, MCP_SEGMENT,
    SEPARATOR,
};

pub use home::{
    ensure_skill_home, marketplace_skill_dir, mcp_skill_dir, resolve_skill_home, user_dropin_root,
    user_skill_dir, SKILL_HOME_ENV, SOURCE_MARKETPLACE, SOURCE_MCP, SOURCE_USER, XDG_DATA_HOME_ENV,
};

pub use sources::{
    marketplace_clone_url, normalize_repo_shorthand, resolve_plugin_source, GitCloneSpec,
    LocalPluginSource, ResolvedPluginSource, SkillSourceError, CNB_HOST, DEFAULT_PLUGIN_ROOT,
    GITHUB_HOST,
};

pub use frontmatter::{parse_skill_frontmatter, strip_skill_frontmatter};

pub use registry::SkillRegistry;

pub use sandbox::{
    ensure_within_size_cap, resolve_skill_resource, resolve_skill_resource_with, SkillSandboxError,
    SkillSandboxReason, DEFAULT_SKILL_FILE, FORBIDDEN_SKILL_FILES,
};

pub use resource::{resolve_skill_view, SkillResourceView, DEFAULT_SKILL_MIME};

pub use manifest::{
    check_strict_conflict, entry_is_strict, enumerate_bundled_server_files, find_plugin_entry,
    iter_plugin_entries, load_bundled_servers, manifest_declared_components, parse_bundled_server,
    plugin_root_base, read_marketplace_manifest, read_plugin_metadata, resolve_plugin_version,
    resolve_skill_override_dirs, PluginManifestError, COMPONENT_FIELDS, MARKETPLACE_MANIFEST,
    MARKETPLACE_MANIFEST_DIR, MCP_INPUTS_FILENAME, MCP_SERVERS_SUBDIR, PLUGIN_MANIFEST,
};

pub use staging::{
    stage_marketplace_skills, stage_mcp_skills, stage_user_skills, ArchiveFetcher,
    DefaultArchiveFetcher, KnownMarketplaceRecord, KnownMarketplaceRecorder,
    MarketplaceStageOptions, McpResource, SkillResourceManager, SkillStagingError,
    DEFAULT_GIT_TIMEOUT, MAX_ARCHIVE_DOWNLOAD_BYTES, MAX_ARCHIVE_MEMBERS, MAX_EXTRACTED_BYTES,
};

pub use debouncer::{
    AsyncCallback, CallbackResult, SkillEventDebouncer, SkillEventDebouncerBuilder,
    DEFAULT_DEBOUNCE_MS,
};

pub use watcher::{
    should_fire, OnChange, SkillFileWatcher, SkillFileWatcherBuilder, SkillWatchError,
    DEFAULT_INTERNAL_WRITE_TTL, DEFAULT_POLL_INTERVAL, POLLING_INTERNAL_WRITE_TTL_FLOOR,
};
