#!/usr/bin/env node
/**
 * v0.2.2 集成测试矩阵专用 MCP server / Dedicated MCP server for the v0.2.2 integration matrix.
 *
 * 在最小 echo server 基础上额外提供：
 *   - `sleep`     : 延迟 N 毫秒后返回（用于 tool_call 取消场景 —— 给取消信号留出在途窗口）
 *   - `gen_image` : 返回 N 字节确定性二进制 image content（用于 blob 旁路 / 二进制 round-trip 场景）
 * Extra tools over the minimal echo server:
 *   - `sleep`     : returns after N ms (lets a cancel signal land mid-flight)
 *   - `gen_image` : returns N deterministic bytes as an image content block (blob sideband round-trip)
 *
 * 帧格式 / framing：换行分隔 JSON（newline-delimited JSON），对齐 rmcp 0.11.0（MCP 2025-03-26）。
 * 独立于 tests/echo-mcp-server/index.js，避免改动共享 fixture 影响既有断言。
 */

const readline = require("readline");

const SERVER_INFO = { name: "v022-mcp-server", version: "1.0.0" };

const TOOLS = [
  {
    name: "echo",
    description: "Echoes back the input message",
    inputSchema: {
      type: "object",
      properties: { message: { type: "string" } },
      required: ["message"],
    },
  },
  {
    name: "sleep",
    description: "Sleeps for `ms` milliseconds then returns (cancellation target)",
    inputSchema: {
      type: "object",
      properties: { ms: { type: "integer" } },
      required: ["ms"],
    },
  },
  {
    name: "gen_image",
    description: "Returns `bytes` deterministic bytes as an image/png content block",
    inputSchema: {
      type: "object",
      properties: { bytes: { type: "integer" } },
      required: ["bytes"],
    },
  },
];

const RESOURCES = [
  { uri: "window://v022.mcp.test/status?priority=10", name: "Status Window", mimeType: "text/plain" },
  { uri: "window://v022.mcp.test/logs?priority=5", name: "Log Window", mimeType: "text/plain" },
  { uri: "file://v022.mcp.test/readme", name: "Readme File", mimeType: "text/plain" },
];

const RESOURCE_CONTENTS = {
  "window://v022.mcp.test/status?priority=10": "System status: OK",
  "window://v022.mcp.test/logs?priority=5": "Log entry 1\nLog entry 2",
  "file://v022.mcp.test/readme": "This is the v022 readme content",
};

// 确定性字节：第 i 字节 = (i * 31 + 7) & 0xff —— 与 Rust 侧重算后比对 sha256 一致。
// Deterministic bytes: byte[i] = (i * 31 + 7) & 0xff, recomputed + sha256-checked on the Rust side.
function deterministicImage(n) {
  const buf = Buffer.alloc(n);
  for (let i = 0; i < n; i++) buf[i] = (i * 31 + 7) & 0xff;
  return buf.toString("base64");
}

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

async function handleRequest(request) {
  const { id, method, params } = request;

  switch (method) {
    case "initialize":
      return ok(id, {
        protocolVersion: "2025-03-26",
        capabilities: {
          tools: { listChanged: false },
          resources: { subscribe: true, listChanged: false },
        },
        serverInfo: SERVER_INFO,
      });

    case "notifications/initialized":
      return null;

    case "tools/list":
      return ok(id, { tools: TOOLS });

    case "resources/list":
      return ok(id, { resources: RESOURCES });

    case "resources/read": {
      const uri = params && params.uri;
      const content = RESOURCE_CONTENTS[uri];
      if (content !== undefined) {
        return ok(id, { contents: [{ uri, mimeType: "text/plain", text: content }] });
      }
      return err(id, -32602, `Resource not found: ${uri}`);
    }

    case "resources/subscribe":
    case "resources/unsubscribe":
      return ok(id, {});

    case "tools/call": {
      const toolName = params && params.name;
      const args = (params && params.arguments) || {};

      if (toolName === "echo") {
        return ok(id, { content: [{ type: "text", text: args.message || "" }], isError: false });
      }

      if (toolName === "sleep") {
        const ms = typeof args.ms === "number" ? args.ms : 0;
        await new Promise((r) => setTimeout(r, ms));
        return ok(id, { content: [{ type: "text", text: `slept ${ms}ms` }], isError: false });
      }

      if (toolName === "gen_image") {
        const n = typeof args.bytes === "number" ? args.bytes : 0;
        return ok(id, {
          content: [{ type: "image", data: deterministicImage(n), mimeType: "image/png" }],
          isError: false,
        });
      }

      return err(id, -32601, `Unknown tool: ${toolName}`);
    }

    case "ping":
      return ok(id, {});

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
    writeResponse({ jsonrpc: "2.0", id: null, error: { code: -32700, message: "Parse error" } });
    return;
  }
  // 不阻塞 readline：每条请求独立 await，sleep 期间仍可接收后续行。
  // Non-blocking: each request is awaited independently so sleep does not stall the input loop.
  handleRequest(request).then(writeResponse).catch((e) => {
    writeResponse({ jsonrpc: "2.0", id: request.id, error: { code: -32603, message: String(e) } });
  });
});

rl.on("close", () => process.exit(0));
