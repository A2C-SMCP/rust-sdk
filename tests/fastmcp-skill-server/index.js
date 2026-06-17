#!/usr/bin/env node
/**
 * FastMCP-style skill MCP server fixture（AS-40 UAT / 集成测试）。
 * FastMCP-style skill MCP server fixture (AS-40 UAT / integration test).
 *
 * 暴露 **`smcp-computer 0.2.2` 当前可注册的形状**（见 AS-40 comment 13849）：
 * 一个带 `_meta.source = "resources"` 的 **skill 根 resource** + 其下子资源（至少含 `SKILL.md`）：
 *
 *   skill://fastmcp-demo/root              _meta={ "source": "resources" }   ← skill 根
 *   skill://fastmcp-demo/root/SKILL.md     ← 入口（YAML frontmatter，name=fastmcp-demo）
 *   skill://fastmcp-demo/root/reference.md ← supporting file（resources 模式子资源）
 *
 * 注意：FastMCP **默认** Provider 暴露的 `skill://<name>/SKILL.md`（无 `_meta.source`、技能名在 host 段）
 * 当前**不会**被 `restage_mcp_skills` 注册——需 provider 侧改用上面这种 `_meta.source=resources` 约定。
 * 本 fixture 即模拟「provider 已适配后」的可注册形状。
 *
 * 帧格式：换行分隔 JSON（对齐 rmcp / MCP 2025-03-26）。
 */

const readline = require("readline");

const SERVER_INFO = { name: "fastmcp-skill-server", version: "1.0.0" };

const SKILL_MD =
  "---\n" +
  "name: fastmcp-demo\n" +
  "description: A FastMCP-style demo skill registered via resources mode\n" +
  "license: MIT\n" +
  "---\n" +
  "# FastMCP Demo Skill\n\nEntry SKILL.md body.\n";

const REFERENCE_MD = "# Reference\n\nSupporting reference content (resources-mode sub-resource).\n";

// skill 根带 _meta.source=resources；子资源在 root URI 之下。
const RESOURCES = [
  {
    uri: "skill://fastmcp-demo/root",
    name: "fastmcp-demo skill root",
    _meta: { source: "resources" },
  },
  { uri: "skill://fastmcp-demo/root/SKILL.md", name: "SKILL.md", mimeType: "text/markdown" },
  { uri: "skill://fastmcp-demo/root/reference.md", name: "reference.md", mimeType: "text/markdown" },
];

const RESOURCE_CONTENTS = {
  "skill://fastmcp-demo/root": "",
  "skill://fastmcp-demo/root/SKILL.md": SKILL_MD,
  "skill://fastmcp-demo/root/reference.md": REFERENCE_MD,
};

function ok(id, result) {
  return { jsonrpc: "2.0", id, result };
}
function err(id, code, message) {
  return { jsonrpc: "2.0", id, error: { code, message } };
}
function writeResponse(resp) {
  if (resp !== null && resp !== undefined) {
    process.stdout.write(JSON.stringify(resp) + "\n");
  }
}

function handleRequest(request) {
  const { id, method, params } = request;
  switch (method) {
    case "initialize":
      return ok(id, {
        protocolVersion: "2025-03-26",
        capabilities: { resources: { subscribe: false, listChanged: false } },
        serverInfo: SERVER_INFO,
      });
    case "notifications/initialized":
      return null;
    case "tools/list":
      return ok(id, { tools: [] });
    case "resources/list":
      return ok(id, { resources: RESOURCES });
    case "resources/read": {
      const uri = params && params.uri;
      const content = RESOURCE_CONTENTS[uri];
      if (content !== undefined) {
        return ok(id, { contents: [{ uri, mimeType: "text/markdown", text: content }] });
      }
      return err(id, -32602, `Resource not found: ${uri}`);
    }
    default:
      return err(id, -32601, `Method not found: ${method}`);
  }
}

const rl = readline.createInterface({ input: process.stdin });
rl.on("line", (line) => {
  const trimmed = line.trim();
  if (!trimmed) return;
  let request;
  try {
    request = JSON.parse(trimmed);
  } catch (e) {
    return;
  }
  writeResponse(handleRequest(request));
});
