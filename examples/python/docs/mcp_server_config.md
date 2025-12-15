# MCP Server 配置规范（CLI 启动）

本规范描述 A2C Computer CLI 在启动与交互模式下加载/使用的 MCP 配置结构与约束。

- 代码依据
  - 配置类型定义：[a2c_smcp/computer/socketio/smcp.py](cci:7://file:///Users/JQQ/PycharmProjects/A2C-SMCP/python-computer-sdk/a2c_smcp/computer/socketio/smcp.py:0:0-0:0)
  - Pydantic 模型：[a2c_smcp/computer/mcp_clients/model.py](cci:7://file:///Users/JQQ/PycharmProjects/A2C-SMCP/python-computer-sdk/a2c_smcp/computer/mcp_clients/model.py:0:0-0:0)
  - CLI 加载逻辑：[a2c_smcp/computer/cli/main.py](cci:7://file:///Users/JQQ/PycharmProjects/A2C-SMCP/python-computer-sdk/a2c_smcp/computer/cli/main.py:0:0-0:0)、[a2c_smcp/computer/cli/interactive_impl.py](cci:7://file:///Users/JQQ/PycharmProjects/A2C-SMCP/python-computer-sdk/a2c_smcp/computer/cli/interactive_impl.py:0:0-0:0)
- 配置由两部分构成
  - `servers`: MCP Server 列表（支持 `stdio`、`streamable`、`sse`）
  - `inputs`: 动态占位符输入项定义，用于渲染 `${input:<id>}`

## 一、文件与加载方式

- 启动参数加载（进入交互前执行），入口参见 [a2c_smcp/computer/cli/main.py::_run_impl()](cci:1://file:///Users/JQQ/PycharmProjects/A2C-SMCP/python-computer-sdk/a2c_smcp/computer/cli/main.py:94:0-165:25)
  - `--config, -c`: 从文件加载 servers 配置。文件可为单对象或对象数组。路径支持 `@path` 或直接路径。
  - `--inputs, -i`: 从文件加载 inputs 定义。文件可为单对象或对象数组。路径支持 `@path` 或直接路径。
- 交互命令（参见 [a2c_smcp/computer/cli/interactive_impl.py](cci:7://file:///Users/JQQ/PycharmProjects/A2C-SMCP/python-computer-sdk/a2c_smcp/computer/cli/interactive_impl.py:0:0-0:0)）
  - `server add <json|@file>`：添加或更新 1 个 Server 配置（交互命令期望单对象；文件为数组请逐条添加）
  - `server rm <name>`：移除指定 Server
  - `start <name>|all` / `stop <name>|all`：启停客户端
  - `inputs load @file`：从文件加载 inputs（需要数组）
  - `inputs add|update <json|@file>`：添加/更新 1 个或多个 inputs（支持数组/单对象）
  - [inputs value ...](cci:1://file:///Users/JQQ/PycharmProjects/A2C-SMCP/python-computer-sdk/a2c_smcp/computer/cli/main.py:168:0-200:5)：管理 inputs 的运行期解析缓存值
  - `mcp`：打印当前内存配置快照（servers + inputs）

## 二、占位符渲染

- Server 配置中可使用 `${input:<id>}` 占位符。
- 渲染依赖已加载的 inputs 定义；在交互命令如 `render` 或内部使用时解析。
- 若引用了未定义的 id，将提示警告并尽量保留原值。

## 三、Servers 配置详解

每个 Server 配置为一个对象，顶层可为单对象或对象数组。共有字段如下（3 种类型通用）：

- `name`: string。唯一标识，作为去重与管理键。
- `disabled`: boolean，默认 false。是否禁用（禁用后不会自动启动）。
- `forbidden_tools`: string[]。禁用的工具名列表（用于屏蔽部分工具）。
- `tool_meta`: { [toolName]: ToolMeta }。按工具名配置元数据：
  - `auto_apply`: boolean | null。若你的上层应用有“调用前确认”策略，true 可跳过确认。
  - `alias`: string | null。工具别名，解决跨 Server 同名冲突；设置后工具以别名对外暴露。
  - `ret_object_mapper`: object | null。返回结构映射表，用于把 MCP 工具返回结构转换为自定义结构，便于前端统一渲染。

支持的 Server 类型与参数：

1) type = `stdio`（本地进程）
- `server_parameters`：
  - `command`: string（如 "python"、"node"、"npx"...）
  - `args`: string[]
  - `env`: { [key]: string } | null
  - `cwd`: string | null
  - `encoding`: string（默认 "utf-8"）
  - `encoding_error_handler`: "strict" | "ignore" | "replace"

2) type = `streamable`（HTTP + SSE，支持流式）
- 注意：超时字段采用字符串（ISO 8601 持续时间），用于规避官方 SDK 中不同类型的序列化差异。
- `server_parameters`：
  - `url`: string
  - `headers`: { [key]: any } | null
  - `timeout`: string（ISO 8601，如 "PT30S"）
  - `sse_read_timeout`: string（ISO 8601，如 "PT60S"）
  - `terminate_on_close`: boolean

3) type = `sse`（服务端事件流）
- `server_parameters`：
  - `url`: string
  - `headers`: { [key]: any } | null
  - `timeout`: number（秒）
  - `sse_read_timeout`: number（秒）

### 示例

- stdio（以 npx 启动 Playwright MCP）
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

- streamable
```json
{
  "name": "my-streamable",
  "type": "streamable",
  "disabled": false,
  "forbidden_tools": [],
  "tool_meta": {
    "search": { "alias": "web_search", "auto_apply": true }
  },
  "server_parameters": {
    "url": "https://api.example.com/mcp",
    "headers": { "Authorization": "Bearer ${input:API_TOKEN}" },
    "timeout": "PT20S",
    "sse_read_timeout": "PT60S",
    "terminate_on_close": true
  }
}
```

- sse
```json
{
  "name": "my-sse",
  "type": "sse",
  "disabled": false,
  "forbidden_tools": ["dangerous_tool"],
  "tool_meta": {},
  "server_parameters": {
    "url": "https://example.com/sse",
    "headers": null,
    "timeout": 20.0,
    "sse_read_timeout": 60.0
  }
}
```

## 四、Inputs 定义

用于为 `${input:<id>}` 占位符提供数据与交互收集准则。顶层为数组（交互命令 `inputs load` 期望数组；启动参数 `--inputs` 支持单对象或数组）。

- 共有字段
  - `id`: string（唯一）
  - `description`: string
- 类型与字段
  - `promptString`
    - 字段：`type="promptString"`, `default: string|null`, `password: boolean|null`
  - `pickString`
    - 字段：`type="pickString"`, `options: string[]`, `default: string|null`
  - `command`
    - 字段：`type="command"`, `command: string`, `args: { [k]: string } | null`

示例：
```json
[
  {
    "id": "API_TOKEN",
    "type": "promptString",
    "description": "API token for the server",
    "default": null,
    "password": true
  },
  {
    "id": "REGION",
    "type": "pickString",
    "description": "Select deployment region",
    "options": ["us-east-1", "eu-west-1"],
    "default": "us-east-1"
  }
]
```

## 五、CLI 用法速查

- 启动并从文件加载（servers 与 inputs 可分别为单对象或数组）
```bash
a2c-computer run --config @./servers.json --inputs @./inputs.json
```

- 进入交互后常用命令
```bash
# 添加单个 server（推荐用 @file；交互命令期望单对象）
server add @./server_stdio.json

# 加载 inputs（需要数组）
inputs load @./inputs.json

# 启动、查看
start all
status
tools

# 查看当前配置快照
mcp

# 渲染占位符测试
render {"env":"${input:API_TOKEN}"}
```

## 六、注意与最佳实践

- 名称与去重
  - Server 使用 `name` 唯一；Inputs 使用 `id` 唯一。重复添加同名/同 id 视为更新。
- 工具名冲突
  - 跨 Server 可能存在同名工具，建议通过 `tool_meta.<tool>.alias` 在全局去重。
- 禁用工具
  - 使用 `forbidden_tools` 屏蔽不应暴露的工具。
- 超时字段差异（重要）
  - `streamable` 的 `timeout/sse_read_timeout` 为字符串（ISO 8601，如 "PT30S"）。
  - `sse` 的 `timeout/sse_read_timeout` 为数字（秒）。
- 长 JSON 的输入
  - 建议使用 `@file` 方式，避免在命令行粘贴超长单行 JSON。
- 与信令服务器配合
  - 通过 Socket.IO 与远端 Agent 协作时，更新配置后可执行 `notify update` 触发远端刷新。

## 七、参考源码

- [a2c_smcp/computer/socketio/smcp.py](cci:7://file:///Users/JQQ/PycharmProjects/A2C-SMCP/python-computer-sdk/a2c_smcp/computer/socketio/smcp.py:0:0-0:0)：SMCP 协议与配置结构（TypedDict）
- [a2c_smcp/computer/mcp_clients/model.py](cci:7://file:///Users/JQQ/PycharmProjects/A2C-SMCP/python-computer-sdk/a2c_smcp/computer/mcp_clients/model.py:0:0-0:0)：Pydantic 模型（严格校验、冻结）
- [a2c_smcp/computer/cli/main.py](cci:7://file:///Users/JQQ/PycharmProjects/A2C-SMCP/python-computer-sdk/a2c_smcp/computer/cli/main.py:0:0-0:0)：启动参数、文件加载、初始化逻辑
- [a2c_smcp/computer/cli/interactive_impl.py](cci:7://file:///Users/JQQ/PycharmProjects/A2C-SMCP/python-computer-sdk/a2c_smcp/computer/cli/interactive_impl.py:0:0-0:0)：交互命令实现（server/inputs 管理、渲染、Socket.IO 操作）
