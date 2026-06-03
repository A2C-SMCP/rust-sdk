/*!
* 文件名: mod.rs
* 作者: JQQ
* 创建日期: 2026/06/03
* 最后修改日期: 2026/06/03
* 版权: 2023 JQQ. All rights reserved.
* 依赖: smcp::skill_name
* 描述: Computer 端 SKILL 治理子系统（命名基座）
*       Computer-side SKILL governance subsystem (naming base).
*/

//! Computer 端 SKILL 治理子系统 / Computer-side SKILL governance subsystem。
//!
//! 对标 Python 治理层资产 `a2c_smcp/computer/skills/` / Mirrors the Python governance assets。
//! 本模块组装 SKILL 通道 Computer 端的底层基座，供 registry / staging / sandbox / reconciler 复用：
//!
//! - [`naming`]：桥接协议级命名 lexer（[`smcp::skill_name`]）+ source → name 合成链（SKL-01 / #40）。

pub mod naming;

pub use naming::{
    is_valid_skill_name, normalize_mcp_server_segment, parse_skill_name,
    synthesize_marketplace_name, synthesize_mcp_name, synthesize_name, synthesize_user_name,
    ParsedSkillName, SkillNameError, SkillNameKind, SkillNameSpec, MAX_SEGMENT_LEN, MCP_SEGMENT,
    SEPARATOR,
};
