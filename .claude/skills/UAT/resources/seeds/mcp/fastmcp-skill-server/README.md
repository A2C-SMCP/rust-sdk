# Seed: mcp/fastmcp-skill-server

FastMCP-style skill MCP server 种子（场景 `mcp-fastmcp-skill`）。

## 项目上下文 / MCP skill staging 特有

最小 stdio MCP server，暴露 `smcp-computer 0.2.2` **可注册形状**的 FastMCP-style skill 资源：
`_meta.source = "resources"` 的 skill 根 `skill://fastmcp-demo/root` + 子资源 `.../root/SKILL.md`、
`.../root/reference.md`。用于验证 `restage_mcp_skills` → `get_skills()` 收集 MCP skill。

## 规范实现（单一真源）

种子脚本即集成测试 fixture：

```
tests/fastmcp-skill-server/index.js   （项目根）
```

不在此处复制，避免漂移。该 fixture 由 gated 集成测试
`crates/smcp-computer/tests/fastmcp_skills_integration.rs` 与本场景共用。

## 运行（验证可注册性）

```bash
cargo test --package smcp-computer --test fastmcp_skills_integration -- --ignored
```

预期：注册 `mcp:fastmcp-skill-test:fastmcp-demo`，物化 `mcp/fastmcp-skill-test/fastmcp-demo/{SKILL.md,reference.md}`。

详见 `../../scenarios/mcp-fastmcp-skill.md`。
