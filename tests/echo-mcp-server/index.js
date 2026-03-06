#!/usr/bin/env node
/**
 * Minimal MCP server for testing stdio transport.
 * Uses newline-delimited JSON framing (one JSON object per line on stdout),
 * matching rmcp 0.11.0's expected format (MCP spec 2025-03-26).
 *
 * Supports: initialize, notifications/initialized, tools/list, tools/call
 */

const readline = require("readline");

const SERVER_INFO = {
  name: "echo-mcp-server",
  version: "1.0.0",
};

const TOOLS = [
  {
    name: "echo",
    description: "Echoes back the input message",
    inputSchema: {
      type: "object",
      properties: {
        message: { type: "string", description: "Message to echo" },
      },
      required: ["message"],
    },
  },
];

function handleRequest(request) {
  const { id, method, params } = request;

  switch (method) {
    case "initialize":
      return {
        jsonrpc: "2.0",
        id,
        result: {
          protocolVersion: "2025-03-26",
          capabilities: { tools: { listChanged: false } },
          serverInfo: SERVER_INFO,
        },
      };

    case "notifications/initialized":
      // Notification — no response
      return null;

    case "tools/list":
      return {
        jsonrpc: "2.0",
        id,
        result: { tools: TOOLS },
      };

    case "tools/call": {
      const toolName = params?.name;
      const args = params?.arguments || {};

      if (toolName === "echo") {
        return {
          jsonrpc: "2.0",
          id,
          result: {
            content: [{ type: "text", text: args.message || "" }],
            isError: false,
          },
        };
      }

      return {
        jsonrpc: "2.0",
        id,
        error: { code: -32601, message: `Unknown tool: ${toolName}` },
      };
    }

    case "ping":
      return { jsonrpc: "2.0", id, result: {} };

    default:
      return {
        jsonrpc: "2.0",
        id,
        error: { code: -32601, message: `Method not found: ${method}` },
      };
  }
}

const rl = readline.createInterface({ input: process.stdin, terminal: false });

rl.on("line", (line) => {
  const trimmed = line.trim();
  if (!trimmed) return;

  try {
    const request = JSON.parse(trimmed);
    const response = handleRequest(request);
    if (response !== null) {
      process.stdout.write(JSON.stringify(response) + "\n");
    }
  } catch (e) {
    const errResp = {
      jsonrpc: "2.0",
      id: null,
      error: { code: -32700, message: "Parse error" },
    };
    process.stdout.write(JSON.stringify(errResp) + "\n");
  }
});

rl.on("close", () => process.exit(0));
