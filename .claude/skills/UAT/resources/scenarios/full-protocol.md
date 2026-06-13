# 场景：full-protocol（Rust SDK）

## 测试目标

验证 Agent ↔ Server ↔ Computer 完整协议流程：连接、office 加入、tool_call 路由、
get_config / get_tools / get_desktop / list_room、SKILL 通知广播、版本握手、断连守卫、
tool_call_cancel、leave_office。

## 类型

完整链路（需要 Server + Computer + Agent 三进程）

## Rust 组件映射

| 角色 | python-sdk | rust-sdk |
|---|---|---|
| Server | `uv run python ... _run_server_process` | `cargo run -p smcp-server-hyper -- 127.0.0.1:<PORT>`（或 `target/debug/smcp-server-hyper <addr>`）|
| Computer | `uv run a2c-computer run --url ...` | `target/debug/smcp-computer run --url http://127.0.0.1:<PORT> --config <cfg>` |
| Agent 驱动 | `agent_protocol_driver.py` | `cargo run -p smcp-agent --example e2e_test_agent`（env 驱动）|

> Agent example 入口：`crates/smcp-agent/examples/e2e_test_agent.rs`。
> env 参数：`SMCP_SERVER_URL`、`SMCP_AGENT_ID`、`SMCP_OFFICE_ID`、`SMCP_API_KEY`、
> `SMCP_TEST_MODE`（`tool_call` 时调用 `get_tools("test-computer")`）。

## 前置条件

1. 三个二进制已编译：
   ```bash
   cargo build -p smcp-server-hyper
   cargo build -p smcp-computer --features cli
   cargo build -p smcp-agent --example e2e_test_agent
   ```
2. Computer 至少挂载一个 MCP server（有工具 + 资源）。本仓库自带 `tests/echo-mcp-server/index.js`
   （提供 `echo` 工具 + `window://` 资源），可直接复用。
3. （多用例）tmux MCP 工具可用。

## 环境准备

按 `resources/test-env-setup.md` 完整链路步骤。Computer 的 MCP 配置示例（`cfg.json`）：

```json
{"type":"stdio","name":"echo","disabled":false,
 "server_parameters":{"command":"node","args":["tests/echo-mcp-server/index.js"]}}
```

Computer 启动后在其 REPL 内 `socket join <office_id> test-computer`
（Agent 的 `get_tools` 默认目标 computer 名为 `test-computer`）。

## 测试用例

> **一键编排**：`bash .claude/skills/UAT/resources/full-protocol-uat.sh`——三进程真实链路，
> 驱动下列全部「✅ 已实现」用例并断言 `UAT_RESULT: PASS`。依赖 #80 修复（polling 握手）。
>
> `e2e_test_agent` example 现为 **env 驱动多用例驱动器**（`SMCP_TEST_MODE=all` 或逗号分隔单选），
> 覆盖 **F-02 / F-08 / F-09 / F-10 / F-11 / F-12** 与 skill-discovery **D-05**。F-05（版本 4008）
> 由编排脚本经 curl/HTTP 覆盖。仍 ⏳ 的：**F-03**（需注册 `on_skills_received` handler + 运行时改
> skill 集触发广播）、**F-06**（断连重连）、**F-07**（Agent SDK 暂未暴露 `get_config` 方法）。

### F-01: Computer 连接并加入 office ✅(冒烟可测)

- **优先级**: P0
- **步骤**: 启动 Server → 启动 Computer(`run --url`) → Computer REPL `socket join proto-uat-office test-computer`
- **预期结果**:
  - Computer 输出 connected / joined office
  - Server 日志含 Computer sid + name

### F-08: get_tools 获取工具列表 ✅(冒烟可测)

- **优先级**: P0 / **前置**: F-01 + echo MCP server 已挂载
- **步骤**: 运行 agent：
  ```bash
  SMCP_SERVER_URL=http://127.0.0.1:<PORT> SMCP_OFFICE_ID=proto-uat-office \
  SMCP_AGENT_ID=agent1 SMCP_TEST_MODE=tool_call \
  cargo run -p smcp-agent --example e2e_test_agent
  ```
- **预期结果**:
  - Agent 日志 `Got N tools`，列出 `echo`（或 `echo/echo`）
  - 无 timeout / error

### F-02: tool_call 路由 ✅

- **驱动**: `SMCP_TEST_MODE=call_tool`
- **步骤**: agent `tool_call(computer, "echo", {message:"hello-uat"})`
- **预期**: 结果回显含 `hello-uat` → `UAT_RESULT: PASS call_tool`

### F-09: get_desktop ✅

- **驱动**: `SMCP_TEST_MODE=get_desktop`；agent `get_desktop(computer)` 返回 `window://` 列表
  （v022 MCP fixture 提供 status/logs 两个 window 资源）。

### F-10: list_room 查询房间成员 ✅

- **驱动**: `SMCP_TEST_MODE=list_room`；agent `list_room(office)` 返回会话列表，断言含自身 `agent1`。

### F-11: leave_office ✅

- **驱动**: `SMCP_TEST_MODE=leave_office`；agent `leave_office()` 成功。

### F-12: tool_call_cancel ✅（传输契约）

- **驱动**: `SMCP_TEST_MODE=tool_call_cancel`；验证 `server:tool_call_cancel` fire-and-forget
  emit 契约（无 ack、不报错）。结果级 `a2c_cancelled` 由 crate 级单测 + in-process 矩阵覆盖。

### F-05: 版本不兼容 4008 ✅

- **驱动**: 编排脚本 curl `/socket.io/?...&a2c_version=0.1.0` → HTTP 400（4008）。
  （Agent SDK 注入权威 `a2c_version`、无法从 example 伪造旧版本，故用 curl 直探 HTTP 闸门。）

### F-03 / F-06 / F-07 ⏳

- **F-03** SKILL 通知广播：需 agent 注册 `on_skills_received` handler + 运行时改 Computer skill 集
  触发 `notify:update_skills`（参 in-process 矩阵 `update_skills` 思路）。
- **F-06** 断连重连：需注入断连 + 验证重连恢复。
- **F-07** get_config：Rust Agent SDK 当前未暴露 `get_config` 方法（Python 有），需先补 SDK 方法。
- 详细期望见 python-sdk 同名场景（协议同构，断言可复用）。

## F-01/F-08 联合冒烟（可一键脚本化）

参考 `resources/full-protocol-smoke.sh`（如已提供）：后台起 server → 经 FIFO 驱动
Computer join → 跑 agent get_tools → 断言 `Got N tools` 含 echo → 清理。

## 清理

Kill server / computer 进程；删 `/tmp/a2c-uat-*`、FIFO。

## 日志收集

三端日志：server.log / computer.log / agent.log（`tee` 双写）。失败时各 capture ≥200 行。
