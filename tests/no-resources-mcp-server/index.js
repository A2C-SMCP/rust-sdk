#!/usr/bin/env node
/**
 * 无 `resources` 能力的最小 MCP stdio server（UAT 测试 fixture）。
 * No-resources-capability minimal MCP stdio server (UAT fixture).
 *
 * 用途：resource-discovery 场景 R-04（4015 MCP_CAPABILITY_NOT_SUPPORTED）——Computer 侧
 * `client:get_resources` 能力预检（INT-04 #78）在目标 server 未声明 `resources` 能力时回
 * flat ErrorPayload 4015。本 server **仅声明 `tools` 能力**，不实现 resources/list，以触发该预检。
 *
 * 框架：换行分隔 JSON（每行一个对象），对齐 rmcp 0.11.0（MCP spec 2025-03-26）。
 * 独立于 tests/echo-mcp-server 与 tests/v022-mcp-server，避免污染既有断言。
 */

const readline = require("readline");

const SERVER_INFO = { name: "no-resources-mcp-server", version: "1.0.0" };

const TOOLS = [
  {
    name: "noop",
    description: "Returns a fixed string; this server has no resources capability",
    inputSchema: { type: "object", properties: {}, required: [] },
  },
];

function ok(id, result) {
  return { jsonrpc: "2.0", id, result };
}
function err(id, code, message) {
  return { jsonrpc: "2.0", id, error: { code, message } };
}

function handleRequest(request) {
  const { id, method, params } = request;

  switch (method) {
    case "initialize":
      return ok(id, {
        protocolVersion: "2025-03-26",
        // ⚠️ 故意只声明 tools，不声明 resources —— 触发 Computer 4015 预检。
        capabilities: { tools: { listChanged: false } },
        serverInfo: SERVER_INFO,
      });

    case "notifications/initialized":
      return null;

    case "tools/list":
      return ok(id, { tools: TOOLS });

    case "tools/call": {
      const toolName = params && params.name;
      if (toolName === "noop") {
        return ok(id, { content: [{ type: "text", text: "noop" }], isError: false });
      }
      return err(id, -32601, `Unknown tool: ${toolName}`);
    }

    case "ping":
      return ok(id, {});

    // 不实现 resources/list / resources/read —— 该 server 无 resources 能力。
    default:
      return err(id, -32601, `Method not found: ${method}`);
  }
}

const rl = readline.createInterface({ input: process.stdin, terminal: false });

rl.on("line", (line) => {
  const trimmed = line.trim();
  if (!trimmed) return;
  let request;
  try {
    request = JSON.parse(trimmed);
  } catch (e) {
    return;
  }
  const resp = handleRequest(request);
  if (resp !== null && resp !== undefined) {
    process.stdout.write(JSON.stringify(resp) + "\n");
  }
});
