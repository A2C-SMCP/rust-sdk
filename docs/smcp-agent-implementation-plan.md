我会按你的 `/arch` 目标，把 Python SDK（`examples/python`）里 Agent 的协议/能力清单梳理出来（事件名、入参字段、大小写、返回/ACK结构、错误语义等），再对照 Rust 的 `crates/smcp-agent` 现状逐项比对，最后输出一份“能力对照表 + 差异/风险清单 + 修正建议”，重点检查你提醒的 **参数命名/大小写/返回结构是否被 Rust 风格(Result/Ok/Err)污染**。我先用工具在两个目录里做定向检索与阅读。



我刚才并行读取 Rust Agent 的关键文件时，其中一个路径 `crates/smcp-agent/src/constants.rs` 不存在导致批量工具调用被拒绝。接下来我会：
1) 先用 `find` 确认 `smcp-agent/src` 下有哪些文件（特别是事件常量/协议常量放哪）。
2) 再把 `lib.rs / transport / sync_agent / auth` 等核心文件读出来，补齐 Rust 侧的能力清单，然后继续做对照分析。







# A2C-SMCP Agent 能力对比分析报告（Python vs Rust）

> 结论先行：**Rust 的 `smcp-agent` 已覆盖 Python Agent 的核心“主动能力”（join/leave、get_tools、get_desktop、tool_call、list_room、超时取消），但在“被动通知处理/自动行为/协议返回结构一致性”上存在明显偏差与风险点**。其中最关键的是：**Rust 侧在协议层（`crates/smcp`）定义了 `ToolCallRet { success/result/error }` 这种 Result 风格结构，与 Python（以及 MCP 标准的 `CallToolResult`）不一致，极易导致跨语言互通失败**；另外 [transport.rs](cci:7://file:///Users/jqq/RustroverProjects/rust-sdk/crates/smcp-agent/src/transport.rs:0:0-0:0) 的 notify 回调实现目前是“临时 dummy agent”，会直接破坏事件处理语义。

---

## 1. Python（`examples/python`）Agent：协议与行为基线（作为“标准答案”）

以下基于 [a2c_smcp/smcp.py](cci:7://file:///Users/jqq/RustroverProjects/rust-sdk/examples/python/a2c_smcp/smcp.py:0:0-0:0) + `a2c_smcp/agent/{base.py,client.py,sync_client.py,types.py}`：

## 1.1 事件（Event）/Namespace 约定
- **Namespace**：`/smcp`（`SMCP_NAMESPACE`）
- **Agent 主动调用的事件**
  - `server:join_office`
  - `server:leave_office`
  - `client:get_tools`
  - `client:get_desktop`
  - `client:tool_call`
  - `server:tool_call_cancel`
  - `server:list_room`
- **Agent 被动监听的通知事件（notify）**
  - `notify:enter_office`
  - `notify:leave_office`
  - `notify:update_config`
  - `notify:update_tool_list`（协议定义了，但 Python Agent 当前主要通过 enter/update_config 后触发拉取 tools）
  - `notify:update_desktop`

## 1.2 请求/返回结构（字段名、大小写、返回习惯）
Python 的协议结构是“纯协议 JSON”，**字段全部 snake_case**，并且有明确约束：

### 1) 通用请求基类
- [AgentCallData](cci:2://file:///Users/jqq/RustroverProjects/rust-sdk/examples/python/a2c_smcp/smcp.py:47:0-49:15)：
  - `agent: str`
  - `req_id: str`（Python 使用 `uuid.uuid4().hex`，**32位无连字符**）

### 2) get_tools
- Req：`{ agent, req_id, computer }`
- Ret：`{ tools: [...], req_id }`
- **强校验**：`response.req_id == request.req_id`（不一致直接抛异常）

### 3) get_desktop
- Req：`{ agent, req_id, computer, desktop_size?, window? }`
- Ret：`{ desktops: [...], req_id }`
- **强校验 req_id**

### 4) tool_call（最关键）
- Req：`{ agent, req_id, computer, tool_name, params, timeout }`
- Ret：**Python 将返回体直接按 MCP `CallToolResult` 解析**：
  - 期望字段典型为：`content`、`isError`（注意大小写：`isError` 是 camelCase）
  - Python 端失败时也**返回 CallToolResult 结构**（而不是抛异常），例如：
    - 超时：`{"content":[...], "isError": true}`（同时额外发送 `server:tool_call_cancel`）
    - 异常：同上，`isError=true`

### 5) list_room（房间会话查询）
- Req：`{ agent, req_id, office_id }`
- Ret：`{ sessions: [...], req_id }`
- Python Agent 辅助方法 [get_computers_in_office](cci:1://file:///Users/jqq/RustroverProjects/rust-sdk/examples/python/a2c_smcp/agent/sync_client.py:312:4-346:17) 会进一步过滤 `role == "computer"`

## 1.3 Python 的“自动行为”（这部分是 Rust 容易遗漏的）
- **收到 `notify:enter_office`**：
  - 校验 `office_id` 与自身一致
  - 自动触发 `client:get_tools` 拉取并回调 `on_tools_received`
- **收到 `notify:update_config`**：
  - 自动触发 `client:get_tools`（重新拉取工具）
- **收到 `notify:update_desktop`**：
  - 自动触发 `client:get_desktop` 拉取一次桌面（仅日志处理，留扩展）

---

# 2. Rust（`crates/smcp-agent`）能力盘点

基于 `crates/smcp-agent/src/{async_agent.rs,transport.rs,events.rs,config.rs}` + 协议 [crates/smcp/src/lib.rs](cci:7://file:///Users/jqq/RustroverProjects/rust-sdk/crates/smcp/src/lib.rs:0:0-0:0)：

## 2.1 已实现的主动能力（接口层面）
`AsyncSmcpAgent` 对外方法（与 Python 基本一一对应）：
- [connect(url)](cci:1://file:///Users/jqq/RustroverProjects/rust-sdk/crates/smcp-agent/src/sync_agent.rs:42:4-45:5)
- [join_office(agent_name)](cci:1://file:///Users/jqq/RustroverProjects/rust-sdk/crates/smcp-agent/src/sync_agent.rs:47:4-51:5) -> 发送 `server:join_office`
- [leave_office()](cci:1://file:///Users/jqq/RustroverProjects/rust-sdk/crates/smcp-agent/src/sync_agent.rs:53:4-56:5) -> `server:leave_office`
- [get_tools(computer)](cci:1://file:///Users/jqq/RustroverProjects/rust-sdk/crates/smcp-agent/src/sync_agent.rs:58:4-61:5) -> `client:get_tools` + req_id 校验 + 本地 tools_cache
- [get_desktop(computer, size, window)](cci:1://file:///Users/jqq/RustroverProjects/rust-sdk/crates/smcp-agent/src/sync_agent.rs:63:4-72:5) -> `client:get_desktop` + req_id 校验
- [tool_call(computer, tool_name, params)](cci:1://file:///Users/jqq/RustroverProjects/rust-sdk/crates/smcp-agent/src/sync_agent.rs:74:4-83:5) -> `client:tool_call`
  - 超时：发送 `server:tool_call_cancel`，并**返回一个 JSON：`{"content":[...],"isError":true}`**（这里是对齐 Python 的）
- [list_room(office_id)](cci:1://file:///Users/jqq/RustroverProjects/rust-sdk/crates/smcp-agent/src/sync_agent.rs:85:4-88:5) -> `server:list_room` + req_id 校验

同步版 [SyncSmcpAgent](cci:2://file:///Users/jqq/RustroverProjects/rust-sdk/crates/smcp-agent/src/sync_agent.rs:20:0-23:1) 是 Tokio runtime 包装，这点 OK。

## 2.2 已实现的被动能力（notify 处理）
在 [transport.rs](cci:7://file:///Users/jqq/RustroverProjects/rust-sdk/crates/smcp-agent/src/transport.rs:0:0-0:0) 里用 `on_any` 只处理 `notify:*`：
- 匹配：
  - `notify:enter_office`
  - `notify:leave_office`
  - `notify:update_config`
  - `notify:update_desktop`
- 但目前处理逻辑非常“半成品”：
  - `notify:enter_office` 会调用事件处理器，但传入的是**临时构造的 dummy agent（dummy auth + config）**
  - `notify:leave_office/update_config/update_desktop` 只日志，不触发 handler 的对应回调
  - `auto_fetch_desktop`、`tools_cache` 在 notify 分支里基本没用起来

---

# 3. 逐项对照表（能力/事件/结构）

## 3.1 事件常量：对齐情况
- **对齐良好**：Rust `crates/smcp::events::*` 与 Python [smcp.py](cci:7://file:///Users/jqq/RustroverProjects/rust-sdk/examples/python/a2c_smcp/smcp.py:0:0-0:0) 事件名一致（大小写/前缀一致）
  - `client:get_tools/get_desktop/tool_call`
  - `server:join_office/leave_office/update_desktop/tool_call_cancel/list_room`
  - `notify:*` 系列

## 3.2 请求字段命名：对齐情况
- **对齐良好**：
  - `req_id`、`office_id`、`tool_name`、`desktop_size`、`isError`（Rust 的超时 fallback JSON）均符合 Python
- **req_id 格式**：
  - Rust [ReqId::new()](cci:1://file:///Users/jqq/RustroverProjects/rust-sdk/crates/smcp/src/lib.rs:54:4-57:5) 使用 `Uuid::new_v4().simple().to_string()`，与 Python `uuid4().hex` 一致（测试也覆盖了）

---

# 4. 关键差异与风险清单（按严重程度排序）

## P0（必须优先处理）：协议返回结构被 Rust 风格污染的风险
### 4.1 `crates/smcp` 中定义了 `ToolCallRet { success, result, error }`
在 [crates/smcp/src/lib.rs](cci:7://file:///Users/jqq/RustroverProjects/rust-sdk/crates/smcp/src/lib.rs:0:0-0:0) 存在：
```rust
pub struct ToolCallRet {
  pub req_id: ReqId,
  pub success: bool,
  pub result: Option<Value>,
  pub error: Option<String>,
}
```
这套结构 **与 Python Agent 的 tool_call 返回（MCP `CallToolResult`）完全不是一回事**：

- Python（以及 MCP）期望：
  - `content: [...]`
  - `isError: bool`
- Rust [ToolCallRet](cci:2://file:///Users/jqq/RustroverProjects/rust-sdk/crates/smcp/src/lib.rs:126:0-133:1) 是典型 `Result` 风格包装：
  - `success/result/error`

虽然 `smcp-agent` 里 [tool_call()](cci:1://file:///Users/jqq/RustroverProjects/rust-sdk/crates/smcp-agent/src/sync_agent.rs:74:4-83:5) 当前返回 `serde_json::Value`，并且超时 fallback 也按 `CallToolResult` 拼了 JSON，但**只要 Server 或 Computer 端开始使用 [ToolCallRet](cci:2://file:///Users/jqq/RustroverProjects/rust-sdk/crates/smcp/src/lib.rs:126:0-133:1) 作为 ACK 返回结构，跨语言就会直接断**。

**建议（方向性）：**
- 协议层（`crates/smcp`）应以 Python 为准，提供与 MCP 兼容的工具返回结构（至少要能表达 `content` + `isError`），避免引入 `success/error` 这种“语言风格层”字段。
- 你提醒的“Rust 工程师习惯写 `Ok/Err` 模式要避免”，这里就是典型雷区。

## P0（必须优先处理）：[transport.rs](cci:7://file:///Users/jqq/RustroverProjects/rust-sdk/crates/smcp-agent/src/transport.rs:0:0-0:0) notify 回调传递 dummy agent，语义错误
当前 `notify:enter_office` 调 handler 的方式是：
- 临时 new 一个 [AsyncSmcpAgent::new(DefaultAuthProvider("dummy","dummy"), config)](cci:1://file:///Users/jqq/RustroverProjects/rust-sdk/crates/smcp/src/lib.rs:54:4-57:5)
- 这意味着 handler 拿到的 agent：
  - **不是实际连接的那个 agent**
  - 其 `transport` 是 `None`
  - `auth_provider` 也是假的
- 任何在 handler 里尝试调用 [agent.get_tools()](cci:1://file:///Users/jqq/RustroverProjects/rust-sdk/crates/smcp-agent/src/sync_agent.rs:58:4-61:5) / [agent.get_desktop()](cci:1://file:///Users/jqq/RustroverProjects/rust-sdk/crates/smcp-agent/src/sync_agent.rs:63:4-72:5) 都会失败或产生隐蔽 bug。

Python 的 handler 回调里传的 client/agent 引用是**真实可用的连接实例**。

**建议（方向性）：**
- notify 分发层必须持有“真实 agent 上下文”或至少可调用的 transport 引用，而不是临时 dummy。

## P1（高风险）：Rust notify 自动行为未对齐 Python
Python 的默认行为：
- enter_office -> 自动 get_tools
- update_config -> 自动 get_tools
- update_desktop -> 自动 get_desktop（并可控扩展）

Rust 当前：
- enter_office：仅日志 + handler（但 agent 引用是假的）
- update_config：仅日志
- update_desktop：仅日志
- `SmcpAgentConfig.auto_fetch_desktop` 目前没有真正生效

这会导致：
- Rust Agent 加入房间后**不会自动同步工具列表**（而 Python 会）
- Rust Agent 接到桌面更新通知**不会刷新桌面缓存/状态**
- 从“业务行为一致性”角度，Rust 复现不完整

## P1（高风险）：notify:update_tool_list 事件未处理
协议里定义了 `notify:update_tool_list`，Python 的实现路径主要是 enter/update_config 后拉取 tools，但协议语义上这也是一个明确触发点。Rust transport 没有覆盖它。

## P2（中风险）：list_room 的调用事件使用 `server:list_room`
Python 的 list_room 走的是 `server:list_room`（没问题）。但你需要确保 Server 端对该事件的 ACK 返回结构严格是 `{ sessions, req_id }`，不要返回 `{ ok: true, data: ... }` 之类 Rust 风格包裹。

---

# 5. “有没有遗漏/错处”总结

## 5.1 Rust 已覆盖的部分（做得对的）
- **事件名/namespace**：对齐 Python
- **请求字段 snake_case**：对齐 Python
- **req_id 格式**：对齐 Python（还加了单测）
- **tool_call 超时返回**：在 Agent 侧 fallback JSON 已对齐 MCP 的 `CallToolResult` 风格（`content` + `isError`），这是正确方向

## 5.2 Rust 当前明显缺失/错误的部分
- **notify 分发语义错误**（dummy agent）——这是“错处”，不是简单遗漏
- **自动拉取工具/桌面行为缺失**（enter_office/update_config/update_desktop）
- **协议层工具返回结构存在 [ToolCallRet](cci:2://file:///Users/jqq/RustroverProjects/rust-sdk/crates/smcp/src/lib.rs:126:0-133:1) 的 Result 风格污染风险**（最需要警惕）
- **notify:update_tool_list 未覆盖**

---

# 6. 建议修正优先级（你后续给 Rust 工程师的行动清单）

## P0
- **移除/禁止在协议层使用 `ToolCallRet {success/result/error}` 作为 tool_call ACK 返回**
  - 协议层应以 Python/MCP 的返回结构为准（至少要兼容 `CallToolResult`）
- **重构 notify handler 传参：必须传真实可用的 agent/transport 上下文**
  - 禁止 dummy agent

## P1
- **补齐 Python 同等自动行为**
  - `notify:enter_office` -> 自动 [get_tools](cci:1://file:///Users/jqq/RustroverProjects/rust-sdk/crates/smcp-agent/src/sync_agent.rs:58:4-61:5)（并回调 `on_tools_received`）
  - `notify:update_config` / `notify:update_tool_list` -> 自动 [get_tools](cci:1://file:///Users/jqq/RustroverProjects/rust-sdk/crates/smcp-agent/src/sync_agent.rs:58:4-61:5)
  - `notify:update_desktop` -> 若 `auto_fetch_desktop=true` 自动 [get_desktop](cci:1://file:///Users/jqq/RustroverProjects/rust-sdk/crates/smcp-agent/src/sync_agent.rs:63:4-72:5)，并回调 `on_desktop_updated`

## P2
- **补测试**
  - 集成/E2E：验证 Rust Agent 收到 notify 后会发起对应 call，并且返回结构字段名严格一致（含大小写，如 `isError`）

---

# 7. 任务完成状态

- **已完成**：对比 `examples/python` 与 `crates/smcp-agent` 的 Agent 协议能力与实现差异，并给出风险点与修正建议（重点覆盖你关心的命名/大小写/返回结构风格污染问题）。