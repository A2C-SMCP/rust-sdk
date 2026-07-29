# `marketplace/plugin-with-bundled-mcp`

**Axis**: MK-BMC-01 (plugin 捆绑 MCP server)

**形态**: marketplace 工作树，plugin 含 `mcp-servers/*.json` 捆绑配置

**用途**: 供 `plugin-management` UAT 场景复用

**提供**:
- marketplace 名: `mp-bundled-mcp`（UAT 中用 `--name mp-bundled-mcp` 显式指定）
- plugin `foo`，含 1 个 skill `valid-skill-pkg`（派生自 `_common`）+ 1 个捆绑 MCP server `figma-mcp`
- 捆绑 MCP server 配置: `plugins/foo/mcp-servers/figma-mcp.json`

**期望被测行为（Rust CLI 视角）**:
- `plugin install foo@mp-bundled-mcp` 成功，输出 `bundledMcpServers: ["figma-mcp"]`
- `plugin info` 的 records 显示 `bundledMcpServers` 含 `figma-mcp`
- `plugin uninstall foo@mp-bundled-mcp` 级联移除捆绑 MCP server（`keptServers: false`）
- `plugin uninstall --keep-servers` 保留捆绑 server（`keptServers: true`）

## ⚠️ 与 python-sdk 的种子差异（Rust 适配）

捆绑 MCP server 配置 `figma-mcp.json` 的 schema 两端**不同**：

| | python-sdk | rust-sdk |
|---|---|---|
| 格式 | `{"name":"figma-mcp","server_parameters":{...}}` | `{"type":"stdio","name":"figma-mcp","server_parameters":{...}}` |
| 必填 | name | **type**(stdio/sse/http) + name + server_parameters |

Rust 的 MCP server 配置是 internally-tagged enum（`type` 区分 stdio/sse/http），缺 `type`
会报 `invalid MCP server config ... missing field type`。见
`crates/smcp-computer/tests/cli/commands_test.rs` 的 stdio 示例。

marketplace.json 工作树本体（`source:"./plugins/foo"` 形式）Rust 可正常解析，无需改。
