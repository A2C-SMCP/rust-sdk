/*!
* 文件名: request_builders.rs
* 作者: JQQ
* 创建日期: 2026/06/01
* 最后修改日期: 2026/06/01
* 版权: 2023 JQQ. All rights reserved.
* 依赖: smcp, serde_json
* 描述: Agent 请求构造纯函数 + 事件合法性校验 / Pure request-builder functions + emit-event validation
*/

//! Agent 请求构造纯函数 + 发送事件合法性校验，供 async/sync Agent 客户端共享。
//! 对标 Python `a2c_smcp/agent/_request_builders.py`。
//!
//! 这些函数无 I/O、无 async 差异；除每请求一次的 `req_id`（`uuid`，对齐 Python `uuid4().hex`）外，
//! 相同输入产生相同 payload。`agent` 标识符由调用方传入（Rust 的 [`smcp::AgentCallData::agent`]
//! 对应 Python 的 `agent_config["agent"]`；本 crate 的 `SmcpAgentConfig` 仅承载超时/重试配置）。

use smcp::{
    AgentCallData, GetBlobReq, GetDesktopReq, GetResourcesReq, GetSkillReq, GetSkillsReq,
    GetToolsReq, ReqId, ToolCallReq,
};

use crate::error::{Result, SmcpAgentError};

/// 构造 `base`：每请求一次新 `req_id` / build the `base` with a fresh `req_id` per request。
fn base(agent: &str) -> AgentCallData {
    AgentCallData {
        agent: agent.to_string(),
        req_id: ReqId::new(),
    }
}

/// 创建工具调用请求 / Create a `client:tool_call` request。
pub fn build_tool_call_request(
    agent: &str,
    computer: &str,
    tool_name: &str,
    params: serde_json::Value,
    timeout: i32,
) -> ToolCallReq {
    ToolCallReq {
        base: base(agent),
        computer: computer.to_string(),
        tool_name: tool_name.to_string(),
        params,
        timeout,
    }
}

/// 创建获取工具列表请求 / Create a `client:get_tools` request。
pub fn build_get_tools_request(agent: &str, computer: &str) -> GetToolsReq {
    GetToolsReq {
        base: base(agent),
        computer: computer.to_string(),
    }
}

/// 创建获取资源请求（透明转发 MCP `resources/list`）/ Create a `client:get_resources` request。
///
/// `cursor`：MCP 标准翻页游标；首次传 `None`。
pub fn build_get_resources_request(
    agent: &str,
    computer: &str,
    mcp_server: &str,
    cursor: Option<&str>,
) -> GetResourcesReq {
    GetResourcesReq {
        base: base(agent),
        computer: computer.to_string(),
        mcp_server: mcp_server.to_string(),
        cursor: cursor.map(str::to_string),
    }
}

/// 创建获取桌面请求 / Create a `client:get_desktop` request。
///
/// `desktop_size`：桌面窗口数量上限；`window`：指定窗口 URI。
pub fn build_get_desktop_request(
    agent: &str,
    computer: &str,
    desktop_size: Option<i32>,
    window: Option<&str>,
) -> GetDesktopReq {
    GetDesktopReq {
        base: base(agent),
        computer: computer.to_string(),
        desktop_size,
        window: window.map(str::to_string),
    }
}

/// 创建获取 SKILL 清单请求 / Create a `client:get_skills` request（轻量元数据，不含 SKILL.md body）。
pub fn build_get_skills_request(agent: &str, computer: &str) -> GetSkillsReq {
    GetSkillsReq {
        base: base(agent),
        computer: computer.to_string(),
    }
}

/// 创建获取 SKILL 包内单资源请求 / Create a `client:get_skill` request。
///
/// `rel_path` 缺省时由 Computer 端解析为 `SKILL.md` 入口；非默认时 MUST 相对、无 `..`、无绝对路径
/// （沙箱由 Computer 端 reapply）。
pub fn build_get_skill_request(
    agent: &str,
    computer: &str,
    name: &str,
    rel_path: Option<&str>,
) -> GetSkillReq {
    GetSkillReq {
        base: base(agent),
        computer: computer.to_string(),
        name: name.to_string(),
        rel_path: rel_path.map(str::to_string),
    }
}

/// 创建通用二进制拉取单块请求 / Create a `client:get_blob` chunk request。
///
/// `chunk_offset` 缺省 0；`max_chunk_bytes` 缺省由 Computer clamp 到 `BlobThresholds.chunk_max_bytes`。
pub fn build_get_blob_request(
    agent: &str,
    computer: &str,
    blob_handle: &str,
    chunk_offset: Option<u64>,
    max_chunk_bytes: Option<u64>,
) -> GetBlobReq {
    GetBlobReq {
        base: base(agent),
        computer: computer.to_string(),
        blob_handle: blob_handle.to_string(),
        chunk_offset,
        max_chunk_bytes,
    }
}

/// 校验发送事件的合法性 / Validate an emitted event name。
///
/// Agent 客户端**不得**发起 `notify:*`（Server 广播）与 `agent:*`（保留）事件。
/// 对标 Python `validate_emit_event`（Rust 改 `raise ValueError` 为 `Err(SmcpAgentError::InvalidEvent)`）。
pub fn validate_emit_event(event: &str) -> Result<()> {
    if event.starts_with("notify:") || event.starts_with("agent:") {
        return Err(SmcpAgentError::InvalidEvent {
            event: event.to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn to_val<T: serde::Serialize>(req: &T) -> serde_json::Value {
        serde_json::to_value(req).unwrap()
    }

    #[test]
    fn test_tool_call_request_shape() {
        let v = to_val(&build_tool_call_request(
            "agent1",
            "c1",
            "echo",
            json!({"text": "hi"}),
            30,
        ));
        // base 经 #[serde(flatten)] 平铺到顶层 agent / req_id
        assert_eq!(v["agent"], "agent1");
        assert_eq!(v["computer"], "c1");
        assert_eq!(v["tool_name"], "echo");
        assert_eq!(v["params"], json!({"text": "hi"}));
        assert_eq!(v["timeout"], 30);
        assert!(v["req_id"].as_str().is_some_and(|s| !s.is_empty()));
    }

    #[test]
    fn test_get_tools_request_shape() {
        let v = to_val(&build_get_tools_request("agent1", "c1"));
        assert_eq!(v["agent"], "agent1");
        assert_eq!(v["computer"], "c1");
        assert!(v.get("tool_name").is_none());
    }

    #[test]
    fn test_get_resources_cursor_optional() {
        // 首次（无 cursor）：不产出 cursor 键
        let v = to_val(&build_get_resources_request("a", "c1", "srv", None));
        assert_eq!(v["mcp_server"], "srv");
        assert!(v.get("cursor").is_none());
        // 翻页：cursor 平铺
        let v = to_val(&build_get_resources_request("a", "c1", "srv", Some("CUR")));
        assert_eq!(v["cursor"], "CUR");
    }

    #[test]
    fn test_get_desktop_optionals() {
        let v = to_val(&build_get_desktop_request("a", "c1", None, None));
        assert!(v.get("desktop_size").is_none());
        assert!(v.get("window").is_none());
        let v = to_val(&build_get_desktop_request(
            "a",
            "c1",
            Some(5),
            Some("window://x"),
        ));
        assert_eq!(v["desktop_size"], 5);
        assert_eq!(v["window"], "window://x");
    }

    #[test]
    fn test_get_skill_rel_path_optional() {
        let v = to_val(&build_get_skill_request("a", "c1", "skill-x", None));
        assert_eq!(v["name"], "skill-x");
        assert!(v.get("rel_path").is_none());
        let v = to_val(&build_get_skill_request(
            "a",
            "c1",
            "skill-x",
            Some("docs/readme.md"),
        ));
        assert_eq!(v["rel_path"], "docs/readme.md");
    }

    #[test]
    fn test_get_skills_request_shape() {
        let v = to_val(&build_get_skills_request("a", "c1"));
        assert_eq!(v["agent"], "a");
        assert_eq!(v["computer"], "c1");
    }

    #[test]
    fn test_get_blob_optionals() {
        let v = to_val(&build_get_blob_request("a", "c1", "h1", None, None));
        assert_eq!(v["blob_handle"], "h1");
        assert!(v.get("chunk_offset").is_none());
        assert!(v.get("max_chunk_bytes").is_none());
        let v = to_val(&build_get_blob_request(
            "a",
            "c1",
            "h1",
            Some(2),
            Some(1024),
        ));
        assert_eq!(v["chunk_offset"], 2);
        assert_eq!(v["max_chunk_bytes"], 1024);
    }

    #[test]
    fn test_builders_are_deterministic_except_req_id() {
        // 相同输入 → 除 req_id 外 payload 一致（req_id 每请求一次 uuid）
        let mut a = to_val(&build_get_tools_request("a", "c1"));
        let mut b = to_val(&build_get_tools_request("a", "c1"));
        assert_ne!(a["req_id"], b["req_id"], "req_id 应每次唯一");
        a.as_object_mut().unwrap().remove("req_id");
        b.as_object_mut().unwrap().remove("req_id");
        assert_eq!(a, b);
    }

    #[test]
    fn test_validate_emit_event() {
        assert!(validate_emit_event("client:tool_call").is_ok());
        assert!(validate_emit_event("server:join_office").is_ok());
        assert!(matches!(
            validate_emit_event("notify:update_skills"),
            Err(SmcpAgentError::InvalidEvent { .. })
        ));
        assert!(matches!(
            validate_emit_event("agent:whatever"),
            Err(SmcpAgentError::InvalidEvent { .. })
        ));
    }
}
