#!/usr/bin/env node
/**
 * #106 专用「可变工具/资源集」stdio MCP server / mutable-tools stdio MCP server for #106.
 *
 * 目的 / Purpose：作为运行期变化通知的**受控驱动器**——恒存 `set_phase(n)` 工具，调用即切换 phase 并主动发出
 *   `notifications/tools/list_changed` + `notifications/resources/list_changed`，用于端到端验证
 *   「MCP 变化 → Computer 检测 → server:update_* → notify:update_* → Agent 回拉」全链路。
 *
 * 声明 `tools.listChanged=true` / `resources.listChanged=true, subscribe=true`（区别于静态的 v022-mcp-server）。
 * 帧格式 / framing：换行分隔 JSON，对齐 rmcp 0.11.0（MCP 2025-03-26）。
 *
 * 工具三态（对齐 python#127 严格 e2e：新增 → 同名换 schema → 移除）/ tool tri-state:
 *   phase 0: [set_phase]                              基线 / baseline
 *   phase 1: [set_phase, dyn_tool(schema A: {x:str})] 新增 / add
 *   phase 2: [set_phase, dyn_tool(schema B: {y:int})] 同名换 schema / rename schema
 *   phase 3: [set_phase]                              移除 / remove
 *
 * 桌面窗口集随 phase 变化（用于 desktop 集合去抖链）：phase>=1 多一个 window://。
 */

const readline = require("readline");

const SERVER_INFO = { name: "mutable-mcp-server", version: "1.0.0" };

let phase = 0;

const SET_PHASE_TOOL = {
  name: "set_phase",
  description: "Switch the server phase; mutates the tool/resource set and emits list_changed",
  inputSchema: {
    type: "object",
    properties: { phase: { type: "integer" } },
    required: ["phase"],
  },
};

// 各 phase 的动态工具（set_phase 恒在，dyn_tool 视 phase 增删/换 schema）。
function dynamicTools(p) {
  if (p === 1) {
    return [
      {
        name: "dyn_tool",
        description: "dynamic tool (schema A)",
        inputSchema: {
          type: "object",
          properties: { x: { type: "string" } },
          required: ["x"],
        },
      },
    ];
  }
  if (p === 2) {
    // 同名 dyn_tool，但 schema 改为 {y:integer} —— 验证「同名换 schema」被下游正确更新。
    return [
      {
        name: "dyn_tool",
        description: "dynamic tool (schema B, renamed)",
        inputSchema: {
          type: "object",
          properties: { y: { type: "integer" } },
          required: ["y"],
        },
      },
    ];
  }
  return []; // phase 0 / 3：无动态工具
}

function currentTools() {
  return [SET_PHASE_TOOL, ...dynamicTools(phase)];
}

// 桌面窗口资源随 phase 变化（phase>=1 多一个 window）。
function currentResources() {
  const base = [
    { uri: "window://mutable.test/main?priority=10", name: "Main", mimeType: "text/plain" },
  ];
  if (phase >= 1) {
    base.push({
      uri: "window://mutable.test/extra?priority=5",
      name: "Extra",
      mimeType: "text/plain",
    });
  }
  return base;
}

function ok(id, result) {
  return { jsonrpc: "2.0", id, result };
}
function err(id, code, message) {
  return { jsonrpc: "2.0", id, error: { code, message } };
}
function writeMessage(msg) {
  if (msg !== null && msg !== undefined) {
    process.stdout.write(JSON.stringify(msg) + "\n");
  }
}
function emitNotification(method) {
  writeMessage({ jsonrpc: "2.0", method });
}

function handleRequest(req) {
  const { id, method, params } = req;

  switch (method) {
    case "initialize":
      return ok(id, {
        protocolVersion: "2025-03-26",
        capabilities: {
          tools: { listChanged: true },
          resources: { subscribe: true, listChanged: true },
        },
        serverInfo: SERVER_INFO,
      });

    case "notifications/initialized":
      return null;

    case "tools/list":
      return ok(id, { tools: currentTools() });

    case "resources/list":
      return ok(id, { resources: currentResources() });

    case "resources/read": {
      const uri = params && params.uri;
      return ok(id, {
        contents: [{ uri, mimeType: "text/plain", text: `content of ${uri} @phase${phase}` }],
      });
    }

    case "resources/subscribe":
    case "resources/unsubscribe":
      return ok(id, {});

    case "tools/call": {
      const toolName = params && params.name;
      const args = (params && params.arguments) || {};

      if (toolName === "set_phase") {
        const next = typeof args.phase === "number" ? args.phase : 0;
        phase = next;
        // 先回响应，再主动发变化通知（换行分隔，接收方各自处理）。
        // 用 setImmediate 确保响应先于通知落到 stdout（避免同一 tick 顺序歧义）。
        setImmediate(() => {
          emitNotification("notifications/tools/list_changed");
          emitNotification("notifications/resources/list_changed");
        });
        return ok(id, {
          content: [{ type: "text", text: `phase set to ${phase}` }],
          isError: false,
        });
      }

      if (toolName === "dyn_tool") {
        return ok(id, {
          content: [{ type: "text", text: `dyn_tool called @phase${phase}` }],
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
  let req;
  try {
    req = JSON.parse(trimmed);
  } catch (e) {
    writeMessage({ jsonrpc: "2.0", id: null, error: { code: -32700, message: "Parse error" } });
    return;
  }
  try {
    writeMessage(handleRequest(req));
  } catch (e) {
    writeMessage({ jsonrpc: "2.0", id: req.id, error: { code: -32603, message: String(e) } });
  }
});

rl.on("close", () => process.exit(0));
