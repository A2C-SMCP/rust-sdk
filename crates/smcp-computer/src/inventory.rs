/*!
* 文件名: inventory.rs
* 作者: JQQ
* 创建日期: 2026/07/06
* 最后修改日期: 2026/07/06
* 版权: 2023 JQQ. All rights reserved.
* 依赖: serde
* 描述: Computer 级 MCP server 归属 + 活跃 inventory 查询的 SDK-facing 元数据类型（#97 / 诉求 #96）
*       SDK-facing ownership + active-inventory metadata for MCP servers.
*/

//! MCP server **归属（ownership）+ 活跃 inventory** 的 SDK-facing 元数据类型（#97；诉求 rust-sdk #96）。
//!
//! ## 这些类型是 **SDK-facing、不进 Agent-facing `client:*` wire**
//!
//! 协议依据 / Protocol: a2c-smcp-protocol **v0.2.3** `computer-management/runtime-contract.md` §4.8
//! （#93 client-owns-MCP-config 边界）。§4.8 要求：重建后的能力归属元数据 MUST 为 boot 的**纯函数**输出
//! （意图 + resolved location + manifest 重推导，每次 boot 可复现，**不**依赖任何调用方持有的内存 ownership
//! map）；且 enabled bundled server **即使进程未拉起**也 MUST **可查询**。本模块的类型正是该「可查询归属」的
//! 载体，供 client（如 `tfrobot-client`）的 Skill / MCP tab 直接消费——判定某 server 是否可从普通 MCP tab
//! 编辑 / 启停，无需读 SDK ledger、无需解析 plugin manifest、无需持内存 ownership map。
//!
//! **刻意不进协议 wire**：归属 / 生命周期字段仅在 Rust SDK 表面（[`Computer::list_mcp_servers_with_metadata`]），
//! **不**加入 `client:*` 事件数据结构——Agent 侧协议表面与能力归属无关（Agent-User 能力等价，不给协议加角色
//! 门控字段）。serde 用 camelCase 对齐 #96 JSON 示例（`managedBy` / `pluginId` / `canEditFromMcpTab` …）。
//!
//! [`Computer::list_mcp_servers_with_metadata`]: crate::computer::Computer::list_mcp_servers_with_metadata

use serde::{Deserialize, Serialize};

/// 一个 MCP server 的**归属**：用户配置 vs 已启用 plugin 派生 / MCP server ownership。
///
/// `#[serde(tag = "type")]` 判别联合，对齐 #96 示例 `managedBy`：
/// - user →  `{"type":"user"}`
/// - plugin → `{"type":"plugin","marketplace":…,"plugin":…,"pluginId":…}`
///
/// `pluginId` = `<plugin>@<marketplace>`（与 Python `installed_plugins.json` 的 map 键、`BundledServerRecord`
/// 严格同构）。归属为 ledger + manifest 的纯函数推导（§4.8.3），不含运行期状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum McpOwnership {
    /// 用户配置 / client 传入的 MCP server（配置态权威在 client 用户配置）/ user-owned。
    User,
    /// 已启用 marketplace plugin 派生的 bundled MCP server / plugin-owned bundled server。
    Plugin {
        /// marketplace 名 / marketplace name。
        marketplace: String,
        /// plugin 名 / plugin name。
        plugin: String,
        /// plugin id：`<plugin>@<marketplace>` / plugin id。
        #[serde(rename = "pluginId")]
        plugin_id: String,
    },
}

/// 面向 UI 入口权限的生命周期能力 / lifecycle capabilities for UI entry gating。
///
/// 由 [`McpOwnership`] 纯函数派生（[`McpOwnership::lifecycle`]），client 据此决定 MCP tab 能否编辑 / 启停：
/// user server 可从 MCP tab 全权管理；plugin bundled server 只读展示、引导到 Marketplace 管理。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpLifecycle {
    /// 是否可在普通 MCP tab 编辑 / 删除 / can edit from the MCP tab。
    pub can_edit_from_mcp_tab: bool,
    /// 是否可在普通 MCP tab 启停 / can start-stop from the MCP tab。
    pub can_start_from_mcp_tab: bool,
    /// 管理入口：`"mcp"`（用户）/ `"marketplace"`（插件）/ where this server is managed from。
    pub manage_from: String,
}

/// 管理入口常量 / manage-from constants。
impl McpLifecycle {
    /// 用户 MCP server 的管理入口 = 普通 MCP tab / manage-from for user servers。
    pub const MANAGE_FROM_MCP: &'static str = "mcp";
    /// plugin bundled server 的管理入口 = Marketplace / manage-from for plugin servers。
    pub const MANAGE_FROM_MARKETPLACE: &'static str = "marketplace";
}

impl McpOwnership {
    /// 从归属纯函数派生生命周期能力 / derive lifecycle capabilities from ownership。
    ///
    /// user → 全权（可编辑 / 可启停，入口 `mcp`）；plugin → 只读（禁编辑 / 禁启停，入口 `marketplace`）。
    #[must_use]
    pub fn lifecycle(&self) -> McpLifecycle {
        match self {
            McpOwnership::User => McpLifecycle {
                can_edit_from_mcp_tab: true,
                can_start_from_mcp_tab: true,
                manage_from: McpLifecycle::MANAGE_FROM_MCP.to_string(),
            },
            McpOwnership::Plugin { .. } => McpLifecycle {
                can_edit_from_mcp_tab: false,
                can_start_from_mcp_tab: false,
                manage_from: McpLifecycle::MANAGE_FROM_MARKETPLACE.to_string(),
            },
        }
    }
}

/// 一条 MCP server 的**活跃 inventory** 条目 + 归属 / 生命周期元数据 / one active-inventory entry。
///
/// [`Computer::list_mcp_servers_with_metadata`] 的返回元素。合并两个来源（client 无需自己拼）：
/// - 运行期已物化的 server（`self.mcp_servers`）——用户配置 or client 经 hooks 物化的 plugin bundled；
/// - ledger 派生的**已启用但尚未物化**的 plugin bundled server（§4.8：进程未拉起也须可观测）。
///
/// `disabled` 取自 server 配置本身（[`crate::mcp_clients::model::MCPServerConfig::disabled`]）；`managed_by`
/// 决定 `lifecycle`。**不**含运行期「进程是否已启动」状态——那由 [`Computer::get_server_status`] 单独提供，
/// 本 inventory 只承载「有哪些 + 归谁 + 能否从 MCP tab 管」这一稳定归属视图（对齐 #96 示例四字段）。
///
/// [`Computer::list_mcp_servers_with_metadata`]: crate::computer::Computer::list_mcp_servers_with_metadata
/// [`Computer::get_server_status`]: crate::computer::Computer::get_server_status
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerWithMetadata {
    /// server 名（inventory 主键）/ server name。
    pub name: String,
    /// 是否禁用（配置态）/ disabled flag from config。
    pub disabled: bool,
    /// 归属：用户 vs 插件 / ownership。
    pub managed_by: McpOwnership,
    /// 由归属派生的生命周期能力 / lifecycle capabilities derived from ownership。
    pub lifecycle: McpLifecycle,
}

impl McpServerWithMetadata {
    /// 由 `name` + `disabled` + 归属组装（`lifecycle` 从归属派生）/ assemble from name + disabled + ownership。
    #[must_use]
    pub fn new(name: impl Into<String>, disabled: bool, managed_by: McpOwnership) -> Self {
        let lifecycle = managed_by.lifecycle();
        Self {
            name: name.into(),
            disabled,
            managed_by,
            lifecycle,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_ownership_serializes_camelcase_and_grants_full_lifecycle() {
        let entry = McpServerWithMetadata::new("everything", false, McpOwnership::User);
        assert!(entry.lifecycle.can_edit_from_mcp_tab);
        assert!(entry.lifecycle.can_start_from_mcp_tab);
        assert_eq!(entry.lifecycle.manage_from, "mcp");

        let v = serde_json::to_value(&entry).unwrap();
        assert_eq!(v["name"], "everything");
        assert_eq!(v["disabled"], false);
        assert_eq!(v["managedBy"]["type"], "user");
        assert_eq!(v["lifecycle"]["canEditFromMcpTab"], true);
        assert_eq!(v["lifecycle"]["canStartFromMcpTab"], true);
        assert_eq!(v["lifecycle"]["manageFrom"], "mcp");
    }

    #[test]
    fn plugin_ownership_serializes_camelcase_and_is_read_only() {
        let entry = McpServerWithMetadata::new(
            "audit-mcp",
            false,
            McpOwnership::Plugin {
                marketplace: "acme".to_string(),
                plugin: "audit".to_string(),
                plugin_id: "audit@acme".to_string(),
            },
        );
        assert!(!entry.lifecycle.can_edit_from_mcp_tab);
        assert!(!entry.lifecycle.can_start_from_mcp_tab);
        assert_eq!(entry.lifecycle.manage_from, "marketplace");

        // 对齐 #96 JSON 示例键名 / mirror the #96 example.
        let v = serde_json::to_value(&entry).unwrap();
        assert_eq!(v["managedBy"]["type"], "plugin");
        assert_eq!(v["managedBy"]["marketplace"], "acme");
        assert_eq!(v["managedBy"]["plugin"], "audit");
        assert_eq!(v["managedBy"]["pluginId"], "audit@acme");
        assert_eq!(v["lifecycle"]["manageFrom"], "marketplace");
    }

    #[test]
    fn metadata_roundtrips_through_serde() {
        let entry = McpServerWithMetadata::new(
            "audit-mcp",
            true,
            McpOwnership::Plugin {
                marketplace: "acme".to_string(),
                plugin: "audit".to_string(),
                plugin_id: "audit@acme".to_string(),
            },
        );
        let json = serde_json::to_string(&entry).unwrap();
        let back: McpServerWithMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, back);
    }
}
