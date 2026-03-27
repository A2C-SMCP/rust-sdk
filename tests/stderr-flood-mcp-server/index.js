#!/usr/bin/env node
/**
 * MCP server that floods stderr during startup to test pipe buffer handling.
 * Writes >128 KB to stderr synchronously BEFORE responding to MCP initialize,
 * which would deadlock if the parent process doesn't drain stderr.
 *
 * Used by: smcp-computer stdio_integration::test_stdio_stderr_flood_no_deadlock
 */

const readline = require("readline");
const fs = require("fs");

// Write >128 KB to stderr synchronously (blocks until OS pipe buffer accepts it)
const line = "STDERR_FLOOD: " + "x".repeat(200) + "\n"; // ~216 bytes per line
const count = 700; // ~151 KB total, well above 64 KB pipe buffer
for (let i = 0; i < count; i++) {
  fs.writeSync(2, line); // fd 2 = stderr, synchronous write
}

const SERVER_INFO = {
  name: "stderr-flood-mcp-server",
  version: "1.0.0",
};

function handleRequest(request) {
  const { id, method } = request;

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
      return null;
    case "tools/list":
      return { jsonrpc: "2.0", id, result: { tools: [] } };
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
    process.stdout.write(
      JSON.stringify({
        jsonrpc: "2.0",
        id: null,
        error: { code: -32700, message: "Parse error" },
      }) + "\n"
    );
  }
});

rl.on("close", () => process.exit(0));
