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

pub mod home;
pub mod naming;
pub mod sources;

pub use naming::{
    is_valid_skill_name, normalize_mcp_server_segment, parse_skill_name,
    synthesize_marketplace_name, synthesize_mcp_name, synthesize_name, synthesize_user_name,
    ParsedSkillName, SkillNameError, SkillNameKind, SkillNameSpec, MAX_SEGMENT_LEN, MCP_SEGMENT,
    SEPARATOR,
};

pub use home::{
    ensure_skill_home, marketplace_skill_dir, mcp_skill_dir, resolve_skill_home, user_dropin_root,
    user_skill_dir, workdir_skill_root, SKILL_HOME_ENV, SOURCE_MARKETPLACE, SOURCE_MCP,
    SOURCE_USER, XDG_DATA_HOME_ENV,
};

pub use sources::{
    marketplace_clone_url, normalize_repo_shorthand, resolve_plugin_source, GitCloneSpec,
    LocalPluginSource, ResolvedPluginSource, SkillSourceError, CNB_HOST, DEFAULT_PLUGIN_ROOT,
    GITHUB_HOST,
};
