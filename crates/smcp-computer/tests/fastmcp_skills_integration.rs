//! AS-40 UAT/集成：真实 stdio MCP server 暴露「可注册形状」的 FastMCP-style skill → `computer.get_skills()`。
//!
//! 验证 `smcp-computer 0.2.2` 现状能力（**不改 SDK**）：当 provider 以 `_meta.source = "resources"` 的
//! skill 根 + 子资源（`skill://<name>/root/SKILL.md` …）暴露 FastMCP skill 时，经 `boot_up` → 连接 →
//! `restage_mcp_skills` 后，`get_skills()` 能收集到该 MCP skill（对齐 AS-40 comment 13849 的本地验证）。
//!
//! 注：FastMCP **默认** Provider 的裸 `skill://<name>/SKILL.md`（无 `_meta.source`）当前不会被注册——
//! 那是 provider 侧适配范畴，本测试覆盖的是适配后的可注册形状。
//!
//! 需要 Node.js（gated）。运行：
//! ```
//! cargo test --package smcp-computer --test fastmcp_skills_integration -- --ignored
//! ```

use std::collections::HashMap;
use std::path::Path;

use smcp_computer::{
    computer::{Computer, SilentSession},
    mcp_clients::model::{MCPServerConfig, StdioServerConfig, StdioServerParameters},
};
use tempfile::TempDir;

/// FastMCP-style skill stdio MCP server fixture（相对 workspace 根）。
fn fastmcp_server_path() -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    format!("{}/../../tests/fastmcp-skill-server/index.js", manifest_dir)
}

#[tokio::test]
#[ignore] // Requires Node.js
async fn test_get_skills_collects_fastmcp_resources_mode_skill() {
    let td = TempDir::new().unwrap();
    let computer = Computer::new(
        "computer",
        SilentSession::new("session"),
        None,
        None,
        true,
        true,
    )
    .with_skill_home(td.path().join("skills"))
    .with_blob_cache_root(td.path().join("blob"));
    // 先 boot（解析 skill_home / 装配子系统）——未 boot 时 restage 直接返回空（AS-40 comment）。
    computer.boot_up().await.expect("boot_up");

    // 连接真实 stdio MCP server（暴露 _meta.source=resources 的 skill 根 + 子资源）。
    let server = MCPServerConfig::Stdio(StdioServerConfig {
        env_file: None,
        name: "fastmcp-skill-test".to_string(),
        disabled: false,
        forbidden_tools: vec![],
        tool_meta: HashMap::new(),
        default_tool_meta: None,
        vrl: None,
        server_parameters: StdioServerParameters {
            command: "node".to_string(),
            args: vec![fastmcp_server_path()],
            env: HashMap::new(),
            cwd: None,
        },
    });
    computer
        .add_or_update_server(server)
        .await
        .expect("add fastmcp server");
    // 显式连接（测试 Computer 不依赖 manager auto_connect）→ 激活 client 才能枚举 skill:// 资源。
    computer
        .start_mcp_client("fastmcp-skill-test")
        .await
        .expect("connect fastmcp server");

    // 全量重物化：识别 resources-mode skill 根并注册。
    let registered = computer.restage_mcp_skills(None).await;
    assert!(
        registered.contains(&"mcp:fastmcp-skill-test:fastmcp-demo".to_string()),
        "expected FastMCP skill registered, got: {registered:?}"
    );

    // get_skills 收集到该 skill，字段与既有 mcp 源规则一致。
    let skills = computer.get_skills().await;
    let demo = skills
        .iter()
        .find(|s| s.name == "mcp:fastmcp-skill-test:fastmcp-demo")
        .unwrap_or_else(|| panic!("FastMCP skill not in get_skills, got: {skills:?}"));
    assert_eq!(demo.source, "mcp:fastmcp-skill-test");
    assert_eq!(
        demo.description,
        "A FastMCP-style demo skill registered via resources mode"
    );
    assert_eq!(demo.uri.as_deref(), Some("skill://fastmcp-demo/root"));

    // 物化进统一 runtime skill home：SKILL.md + resources-mode 子资源 reference.md 都落盘。
    let path = Path::new(&demo.path);
    assert!(path.ends_with("mcp/fastmcp-skill-test/fastmcp-demo"));
    assert!(path.join("SKILL.md").is_file(), "SKILL.md materialized");
    assert!(
        path.join("reference.md").is_file(),
        "resources-mode sub-resource materialized"
    );

    computer.shutdown().await.ok();
}
