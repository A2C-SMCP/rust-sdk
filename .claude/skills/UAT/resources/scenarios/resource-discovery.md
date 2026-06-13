# 场景：resource-discovery

## 测试目标

验证 `client:get_resources` 协议事件端到端：MCP 资源透明转发、强类型 snake_case 规整、
`window://` 资源发现，以及错误码 4014（MCP Server 未注册）/ 4015（未声明 resources 能力）。
对标 python-sdk 同名场景 R-01~R-05；Rust 经强类型 `A2CResource`（`mime_type` 等已 snake_case）。

## 类型

完整链路（Agent → Server → Computer 三真实进程，真实 socket.io）。

## 状态

✅ **端到端通过**（#82 ack 拆封修复后首次跑通）。由 `full-protocol-uat.sh` 的 `get_resources` mode 驱动。

## 环境准备

一键编排：`bash .claude/skills/UAT/resources/full-protocol-uat.sh`。
该脚本启动放行鉴权 server（`examples/uat_test_server`）、Computer（挂两个 stdio MCP）、
Agent 驱动器（`examples/e2e_test_agent`，`SMCP_TEST_MODE=all`），并隔离 `A2C_SKILL_HOME`/`XDG_CONFIG_HOME`。

### MCP fixture（编排脚本 cfg.json 挂载）

| 注册名 | fixture | 能力 | 用途 |
|---|---|---|---|
| `echo` | `tests/v022-mcp-server/index.js` | tools + **resources** | R-01/R-02：window:// 资源透传 |
| `no-resources` | `tests/no-resources-mcp-server/index.js` | **仅 tools** | R-04：4015 能力预检 |

## 测试用例

### R-01: get_resources 成功返回资源列表（透明转发）

- **优先级**: P0
- **步骤**: Agent `get_resources(computer, "echo", None)`
- **预期**: 返回 `GetResourcesRet`，`resources` 非空（fixture 含 3 条：2×`window://` + 1×`file://`），
  `next_cursor=None`（资源少无分页）。

### R-02: window:// 资源透传 + snake_case 字段

- **优先级**: P0
- **步骤**: 检查 R-01 返回的 `A2CResource`
- **预期**: 至少一条 `uri` 以 `window://` 起始；字段经强类型 `A2CResource`（`uri`/`name`/
  `description`/`mime_type`/`annotations`），`mime_type` 为 snake_case（Rust 强类型天然保证，无 camelCase 残留）。

### R-03: 指定不存在的 MCP Server → 4014

- **优先级**: P0
- **步骤**: `get_resources(computer, "nonexistent-server", None)`
- **预期**: `Err(SmcpAgentError::Protocol)`，`code == 4014`（MCP_SERVER_NOT_FOUND），顶层 `mcp_server_name` 分流字段存在。

### R-04: 目标 server 无 resources 能力 → 4015

- **优先级**: P0
- **前置**: `no-resources` server 已挂载
- **步骤**: `get_resources(computer, "no-resources", None)`
- **预期**: `Err(Protocol)`，`code == 4015`（MCP_CAPABILITY_NOT_SUPPORTED），顶层 `capability == "resources"`。
  （Computer 侧 INT-04 #78 的 `list_resources` 能力预检）

### R-05: 指定不存在的 Computer → 路由失败（404）

- **优先级**: P1
- **说明**: 由 error-codes 场景 E-13 类覆盖（`get_resources("ghost-computer-999", ...)` → Server 路由层
  flat ErrorPayload 404 `build_computer_not_found_error`）。

## 驱动器实现

`crates/smcp-agent/examples/e2e_test_agent.rs` 的 `get_resources` mode：单次运行内连测 R-01/R-02/R-03/R-04，
全命中才打 `UAT_RESULT: PASS get_resources`。

## 与 Python 的差异

- Rust 强类型 `A2CResource` 已 snake_case，无需运行时 camelCase→snake_case 规整断言（编译期保证）。
- fixture 资源 URI 为 `window://v022.mcp.test/...`（非 python 的 `window://main-editor`），断言只校验 `window://` 前缀与非空，不绑定具体 URI。
