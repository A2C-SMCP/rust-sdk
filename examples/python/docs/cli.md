# A2C Computer CLI 使用指南

本指南介绍如何使用 A2C Computer CLI 进行计算机端的运行、配置管理、工具查询与 Socket.IO 连接。

- 技术栈：`typer + rich + prompt_toolkit`
- 交互原则：
  - 计算机的配置管理、启停管理与状态查询通过 CLI 暴露。
  - 工具的实际调用与控制通过 Socket.IO 通道由远端 Agent 驱动。
  - 对配置中出现的动态变量如 `${input:<id>}`，CLI 会在渲染阶段按需解析（需要你先提供对应的 inputs 定义）。

## 安装与启动

- 安装（示例）
```bash
pip install -e .
```

- 启动 CLI
```bash
python -m a2c_smcp.computer.cli.main run
# 或（如果配置了 console_scripts）
a2c-computer run
```

- 常用参数
```bash
# 自动连接 MCP Server（在添加配置时立即尝试启动）和自动重连
python -m a2c_smcp.computer.cli.main run --auto-connect true --auto-reconnect true
```

启动后将进入交互模式（prompt: `a2c>`），输入 `help` 查看可用命令。

## 交互命令总览

- status
  - 查看当前 MCP 管理器中各服务器的状态。
- tools
  - 列出当前可用工具（来源于所有已激活的 MCP Server）。
- mcp
  - 打印当前内存中的 MCP 配置（含 `servers` 与 `inputs` 概览）。
- server add <json|@file>
  - 动态添加或更新某个 MCP Server 的配置。支持直接输入 JSON 字符串或 `@path/to.json` 从文件加载。
  - 配置会按需渲染 `${input:<id>}` 占位符，并通过强校验确认结构有效。
- server rm <name>
  - 移除指定名称的 MCP Server 配置，如果该服务已启动会一并停止。
- start <name>|all
  - 启动单个或全部（未禁用）MCP Server 客户端。
- stop <name>|all
  - 停止单个或全部 MCP Server 客户端。
- inputs load @file
  - 从文件加载 inputs 定义（用于占位符的按需解析）。文件必须是包含 inputs 列表的 JSON。
  - 说明：内部以 set 管理，按 id 唯一去重。
 - inputs add <json|@file>
   - 新增或更新一个或多个 inputs 定义（id 相同即视为更新）。
 - inputs update <json|@file>
   - 与 add 等价的同义命令。
 - inputs rm <id>
   - 按 id 删除一个 input 定义。
 - inputs get <id>
   - 查看某个 input 的当前定义。
 - inputs list
   - 列出当前全部 inputs 定义。
 - inputs value list
   - 列出当前所有已解析的 inputs 缓存值（只包含已经解析或手动设置过的项）。
 - inputs value get <id>
   - 查看指定 id 的当前缓存值，若尚未解析则返回提示。
 - inputs value set <id> [<json|text>]
   - 设置指定 id 的当前值；当省略值参数时，将尝试使用该 input 定义中的 `default` 值（不支持 command 类型、且必须存在 default）。
 - inputs value rm <id>
   - 删除指定 id 的当前缓存值。
 - inputs value clear [<id>]
   - 清空全部或指定 id 的缓存值。
- desktop [size] [window_uri]
  - 获取当前 Desktop 信息，size 为可选数量上限，window_uri 为可选的特定 WindowURI 过滤条件。
- tc <json|@file>
  - 使用与 Socket.IO `ToolCallReq` 一致的 JSON 结构调试工具调用，直接走本地 MCP 执行链路，便于在无 Agent 的场景下排查问题。
- history [n]
  - 查看最近的工具调用历史记录（CLI 内部仅保存最近 10 条）；可选参数 n 限制输出条数。
- socket connect <url>
  - 连接到信令服务器（Socket.IO）。如果省略 `<url>`，CLI 会交互式询问 URL、auth 与 headers。
- socket join <office_id> <computer_name>
  - 加入一个 office（房间）。成功后会接收与该 office 相关的事件。
- socket leave
  - 离开当前 office。
- notify update
  - 在已连接并加入 office 的前提下，向服务器发送 `server:update_config` 事件，通知远端刷新配置。
- render <json|@file>
  - 测试渲染任意 JSON 结构，内部按需解析其中的 `${input:<id>}` 占位符并打印结果。
- quit | exit
  - 退出 CLI。

## 配置与 Inputs 格式

CLI 使用的是 SMCP 协议同构结构（与 `a2c_smcp/computer/socketio/smcp.py` 中的类型一致）。

- Server 配置（示例，stdio 类型）：
```json
{
  "name": "my-stdio-server",
  "type": "stdio",
  "disabled": false,
  "forbidden_tools": [],
  "tool_meta": {
    "echo": {"auto_apply": true}
  },
  "server_parameters": {
    "command": "my_mcp_server",
    "args": ["--flag"],
    "env": {"MY_ENV": "${input:MY_ENV_VALUE}"},
    "cwd": null,
    "encoding": "utf-8",
    "encoding_error_handler": "strict"
  }
}
```

- Inputs 定义（示例）：
```json
[
  {
    "id": "MY_ENV_VALUE",
    "type": "promptString",
    "description": "Environment variable for the server",
    "default": "hello",
    "password": false
  },
  {
    "id": "REGION",
    "type": "pickString",
    "description": "Select a region",
    "options": ["us-east-1", "eu-west-1"],
    "default": "us-east-1"
  }
]
```

当 Server 配置中出现 `${input:MY_ENV_VALUE}` 这样的占位符时，会在「渲染」阶段按需解析，解析逻辑来自你通过 `inputs load` 提供的 inputs 定义。

## 常见操作示例

1) 加载 inputs 定义
```bash
# 假设 inputs.json 含上面示例
inputs load @./inputs.json
```

2) 添加/更新一个 Server 配置
```bash
# 从文件加载（推荐）
server add @./server_stdio.json

# 或直接输入 JSON 字符串
server add {"name":"my-stdio-server","type":"stdio","disabled":false,"forbidden_tools":[],"tool_meta":{},"server_parameters":{"command":"my_mcp_server","args":[],"env":null,"cwd":null,"encoding":"utf-8","encoding_error_handler":"strict"}}
# 或者使用这个测试用例
server add {"name": "e2e-test", "type": "stdio", "disabled": false, "forbidden_tools": [], "tool_meta": {}, "server_parameters": {"command": "python", "args": ["tests/integration_tests/mcp_servers/direct_execution.py"], "env": null, "cwd": null, "encoding": "utf-8", "encoding_error_handler": "strict"}}
```

针对通过 npx 启动 Playwright MCP（端口 8931）的 stdio 配置示例：

```json
{
  "name": "playwright-mcp",
  "type": "stdio",
  "disabled": false,
  "forbidden_tools": [],
  "tool_meta": {},
  "server_parameters": {
    "command": "npx",
    "args": ["@playwright/mcp@latest", "--port", "8931"],
    "env": null,
    "cwd": null,
    "encoding": "utf-8",
    "encoding_error_handler": "strict"
  }
}
```

如果要直接在交互式 CLI 中添加，可粘贴为一行：

```bash
server add {"name":"playwright-mcp","type":"stdio","disabled":false,"forbidden_tools":[],"tool_meta":{},"server_parameters":{"command":"npx","args":["@playwright/mcp@latest"],"env":null,"cwd":null,"encoding":"utf-8","encoding_error_handler":"strict"}}
```

3) 启动所有服务，查看状态与工具
```bash
start all
status
tools
```

4) 连接信令服务器并加入 office
```bash
socket connect http://localhost:7000
socket join office-123 "My Computer"
```

5) 通知远端刷新配置
```bash
notify update
```

6) 测试渲染任意 JSON
```bash
render {"env":"${input:MY_ENV_VALUE}","regions":"${input:REGION}"}
# 或
render @./any.json
```

7) 使用 inputs value 管理当前值
```bash
# 使用 default 值填充（当定义中存在 default，且类型不是 command）
inputs value set MY_ENV_VALUE

# 手动设置一个 JSON 或文本值
inputs value set REGION "us-east-1"

# 查看当前缓存值
inputs value list
inputs value get MY_ENV_VALUE
```

8) 获取 Desktop 信息
```bash
# 获取最多 10 个窗口的 Desktop 概览
desktop 10

# 仅获取指定 WindowURI 对应的窗口
desktop window://my_window
```

9) 使用 tc 调试工具调用
```bash
# 直接传入 JSON
tc {"computer": "local", "agent": "debug-agent", "req_id": "dbg-1", "tool_name": "echo", "params": {"text": "hello"}, "timeout": 30}

# 或从文件加载与 Socket.IO 一致的 ToolCallReq JSON
tc @./tool_call_req.json
```

10) 查看调用历史
```bash
# 查看最近所有历史（最多 10 条）
history

# 只看最近 3 条
history 3
```

11) 停止与移除
```bash
stop all
server rm my-stdio-server
```

## 注意事项与最佳实践

- Server 名称唯一性
  - 当多个服务器存在相同工具名时，系统会抛出冲突警告。建议使用 `tool_meta.alias` 为工具添加别名避免冲突。
- 禁用工具
  - 可通过 `forbidden_tools` 禁用特定工具；禁用后该工具无法被调用。
- auto_apply
  - 当 `tool_meta.<tool>.auto_apply = true` 时，将跳过调用前的用户二次确认（若你的应用侧设置了确认策略）。
- 渲染与报错
  - 若引用了未定义的 `${input:<id>}`，渲染时会记录警告并尽量保留原值继续；建议确保 inputs 定义完整。
- Socket.IO 会话
  - `notify update` 需要已 `socket connect` 并成功 `socket join` 后才能正确通知；否则会提示未连接。
- 参数大小与复杂 JSON
  - 在 `server add` 与 `render` 中直接粘贴长 JSON 可能不便，建议使用 `@file.json` 方式。

## 故障排查

- 看不到任何工具
  - 确认已 `start all` 或 `start <name>`，并检查对应进程是否正常启动。
- 工具名冲突报错
  - 为冲突的工具配置 `tool_meta.alias`，保证在全局唯一。
- 输入占位符没有被替换
  - 确认已通过 `inputs load @file` 提供 inputs 定义，或检查 id 是否拼写正确。
- Inputs 唯一性
  - inputs 在内存中以 set 管理，且模型以 `id` 作为唯一标准（通过 `__hash__`/`__eq__` 实现）。当多次添加相同 id 的输入时，视为更新覆盖。
- 无法通知远端刷新
  - 确认已 `socket connect <url>` 且 `socket join <office_id> <computer_name>` 成功，再执行 `notify update`。

## 参考

- 代码位置
  - CLI 主入口：`a2c_smcp/computer/cli/main.py`
  - 计算机核心：`a2c_smcp/computer/computer.py`
  - Socket.IO SMCP 类型：`a2c_smcp/computer/socketio/smcp.py`
  - Socket.IO 客户端：`a2c_smcp/computer/socketio/client.py`
  - 输入渲染：`a2c_smcp/computer/inputs/render.py`
  - CLI I/O 工具：`a2c_smcp/computer/inputs/cli_io.py`
