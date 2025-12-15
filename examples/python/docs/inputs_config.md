# Inputs 配置规范（CLI 启动）

本规范说明在启动 A2C Computer CLI 时，如何通过 inputs 配置文件为 MCP Server 配置中的占位符 `${input:<id>}` 提供数据源与交互收集准则。

- 代码依据
  - 类型定义（Pydantic 模型）：[a2c_smcp/computer/mcp_clients/model.py](cci:7://file:///Users/JQQ/PycharmProjects/A2C-SMCP/python-computer-sdk/a2c_smcp/computer/mcp_clients/model.py:0:0-0:0)
  - 解析与缓存基类：[a2c_smcp/computer/inputs/base.py](cci:7://file:///Users/JQQ/PycharmProjects/A2C-SMCP/python-computer-sdk/a2c_smcp/computer/inputs/base.py:0:0-0:0)
  - CLI 交互命令实现：[a2c_smcp/computer/cli/interactive_impl.py](cci:7://file:///Users/JQQ/PycharmProjects/A2C-SMCP/python-computer-sdk/a2c_smcp/computer/cli/interactive_impl.py:0:0-0:0)
  - 运行期应用位置：[a2c_smcp/computer/computer.py](cci:7://file:///Users/JQQ/PycharmProjects/A2C-SMCP/python-computer-sdk/a2c_smcp/computer/computer.py:0:0-0:0)（渲染与解析、缓存读写）

## 1. 目标与作用

- 通过 inputs 定义告知系统：
  - 可用的动态输入项有哪些（id 唯一）。
  - 每种输入项的交互方式、默认值或可选项。
- 在渲染 MCP Server 配置（例如 `env`、`headers` 等字段）时，遇到 `${input:<id>}` 就会按需解析：
  - 若该 id 的值已在缓存中，直接使用缓存值。
  - 若未缓存，则根据该 id 对应的 inputs 定义，通过交互或命令解析出值并缓存。

## 2. 文件格式与加载方式

- 文件内容可以是：
  - 单个对象（一个 input 定义）
  - 数组（多个 input 定义）
- CLI 启动参数
  - `--inputs, -i @path/to/inputs.json` 或 `--inputs path/to/inputs.json`
  - 启动阶段允许单对象或对象数组
- 交互命令
  - `inputs load @file` 期望文件是“数组”
  - `inputs add <json|@file>` / `inputs update <json|@file>` 支持单对象或数组
- 内部管理
  - 内部以 [set](cci:1://file:///Users/JQQ/PycharmProjects/A2C-SMCP/python-computer-sdk/a2c_smcp/computer/computer.py:305:4-310:69) 存储 inputs，且以 [id](cci:1://file:///Users/JQQ/PycharmProjects/A2C-SMCP/python-computer-sdk/a2c_smcp/computer/inputs/base.py:94:4-100:33) 作为唯一键（见 `MCPServerInputBase.__hash__/__eq__`）
  - 重复 [id](cci:1://file:///Users/JQQ/PycharmProjects/A2C-SMCP/python-computer-sdk/a2c_smcp/computer/inputs/base.py:94:4-100:33) 视为“更新覆盖”

## 3. 类型与字段

三个类型，均继承 [MCPServerInputBase](cci:2://file:///Users/JQQ/PycharmProjects/A2C-SMCP/python-computer-sdk/a2c_smcp/computer/socketio/smcp.py:111:0-115:20)：
- 共有字段
  - `id: string`（唯一）
  - `description: string`

1) promptString（自由文本输入）
- 字段
  - `type: "promptString"`
  - `default: string | null`（按回车时可取默认值）
  - `password: boolean | null`（true 时掩码输入）
- 解析行为
  - 通过命令行提示采集字符串；若空输入且提供了 default，则返回 default

2) pickString（从候选项中选择）
- 字段
  - `type: "pickString"`
  - `options: string[]`
  - `default: string | null`（若为空输入则可采用默认项）
- 解析行为
  - 列出 options 并让用户选择一个（支持默认索引）

3) command（执行命令得到值）
- 字段
  - `type: "command"`
  - `command: string`（执行的命令，如 `echo $USER`、`python script.py`）
  - `args: { [k]: string } | null`（可选的参数对象，具体解析由实现决定）
- 解析行为
  - 执行命令并读取 stdout 作为值（详见 [inputs/cli_io.py](cci:7://file:///Users/JQQ/PycharmProjects/A2C-SMCP/python-computer-sdk/a2c_smcp/computer/inputs/cli_io.py:0:0-0:0) 的 [arun_command](cci:1://file:///Users/JQQ/PycharmProjects/A2C-SMCP/python-computer-sdk/a2c_smcp/computer/inputs/cli_io.py:123:0-181:14)）

模型约束
- Pydantic 配置为 `extra="forbid"`, `frozen=True`，字段严格校验且实例不可变
- [id](cci:1://file:///Users/JQQ/PycharmProjects/A2C-SMCP/python-computer-sdk/a2c_smcp/computer/inputs/base.py:94:4-100:33) 唯一（集合内去重），重复 [id](cci:1://file:///Users/JQQ/PycharmProjects/A2C-SMCP/python-computer-sdk/a2c_smcp/computer/inputs/base.py:94:4-100:33) 会视为更新

## 4. 示例

最小示例（数组）
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

单对象示例（启动参数支持单对象）
```json
{
  "id": "ORG_ID",
  "type": "promptString",
  "description": "Your organization id",
  "default": "default-org",
  "password": false
}
```

command 类型示例
```json
[
  {
    "id": "GIT_SHA",
    "type": "command",
    "description": "Current git short SHA",
    "command": "git rev-parse --short HEAD",
    "args": null
  }
]
```

## 5. 在 Server 配置中的使用

当 server 配置中包含 `${input:<id>}` 时，渲染过程会按需解析：
- 位置示例：`env`、`headers`、任意字符串字段
- 引用示例：
```json
{
  "name": "my-streamable",
  "type": "streamable",
  "disabled": false,
  "forbidden_tools": [],
  "tool_meta": {},
  "server_parameters": {
    "url": "https://api.example.com/mcp",
    "headers": { "Authorization": "Bearer ${input:API_TOKEN}" },
    "timeout": "PT20S",
    "sse_read_timeout": "PT60S",
    "terminate_on_close": true
  }
}
```

注意
- 若引用了未定义的 id，渲染阶段会记录警告并尽量保留原值（参考 [Computer.boot_up()](cci:1://file:///Users/JQQ/PycharmProjects/A2C-SMCP/python-computer-sdk/a2c_smcp/computer/computer.py:85:4-121:61) 与渲染器行为）

## 6. 运行期命令与缓存

交互式 CLI 提供一组命令管理 inputs 定义与当前值缓存（参考 [cli/interactive_impl.py](cci:7://file:///Users/JQQ/PycharmProjects/A2C-SMCP/python-computer-sdk/a2c_smcp/computer/cli/interactive_impl.py:0:0-0:0) 与 [computer.py](cci:7://file:///Users/JQQ/PycharmProjects/A2C-SMCP/python-computer-sdk/a2c_smcp/computer/computer.py:0:0-0:0)）：

- 定义管理
  - `inputs load @file`：从文件加载 inputs（需要数组）
  - `inputs add <json|@file>` / `inputs update <json|@file>`：新增或更新定义（支持单对象或数组）
  - `inputs rm <id>`：删除定义
  - `inputs get <id>`：查看定义
  - `inputs list`：列出所有定义

- 值缓存管理（解析结果）
  - `inputs value list`：列出全部缓存值
  - `inputs value get <id>`：获取指定 id 的缓存值
  - `inputs value set <id> <json|text>`：设置缓存值（会覆盖自动解析结果）
  - `inputs value rm <id>`：删除指定 id 的缓存值
  - `inputs value clear [<id>]`：清空全部或指定 id 的缓存

缓存说明
- 解析出的值会缓存在 [InputResolver](cci:2://file:///Users/JQQ/PycharmProjects/A2C-SMCP/python-computer-sdk/a2c_smcp/computer/inputs/base.py:37:0-134:33) 中，以避免重复交互
- 更新 inputs 定义时，会重建解析器并清理缓存（确保后续解析与定义同步）
- 也可手动通过上述命令管理缓存（便于脚本化或 CI 环境）

## 7. 启动阶段与交互阶段的差异

- 启动阶段（`--inputs`）
  - 文件可为单对象或数组，读取后合并进入 [Computer(inputs=set(...))](cci:2://file:///Users/JQQ/PycharmProjects/A2C-SMCP/python-computer-sdk/a2c_smcp/computer/computer.py:46:0-489:17)
- 交互阶段
  - `inputs load` 仅接受“数组”
  - `inputs add/update` 可接受单对象或数组
- 内部一律去重（按 [id](cci:1://file:///Users/JQQ/PycharmProjects/A2C-SMCP/python-computer-sdk/a2c_smcp/computer/inputs/base.py:94:4-100:33)），同 id 代表更新

## 8. 最佳实践与注意事项

- id 命名规范
  - 使用大写与下划线（例如 `API_TOKEN`, `REGION`），便于在 `${input:...}` 中清晰引用
- 秘密与敏感信息
  - 对敏感输入使用 `promptString + password=true`，避免明文回显
  - 若需持久化，建议在上层安全存储或通过命令型 input 读取（例如从安全仓读取）
- 批量维护
  - 使用 `inputs load @file` 批量导入；用 `inputs value set` 在自动化环境中直接注入值
- 与 server 配置联动
  - 修改 inputs 定义不会自动重启或重渲染已运行的 Server
  - 如需将新值应用到已运行服务，建议结合 `server add` 重新提交配置（热更新策略）

## 9. 快速上手示例

1) 准备 `inputs.json`
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
    "description": "Select region",
    "options": ["us-east-1", "eu-west-1"],
    "default": "us-east-1"
  }
]
```

2) 启动并加载
```bash
a2c-computer run --inputs @./inputs.json
```

3) 交互中查看与设置
```bash
inputs list
inputs value list
inputs value set API_TOKEN "sk-***"
```

4) 渲染测试
```bash
render {"auth":"Bearer ${input:API_TOKEN}","region":"${input:REGION}"}
```

## 10. 参考源码

- [a2c_smcp/computer/mcp_clients/model.py](cci:7://file:///Users/JQQ/PycharmProjects/A2C-SMCP/python-computer-sdk/a2c_smcp/computer/mcp_clients/model.py:0:0-0:0)：`MCPServerInput*` 模型定义与约束（严格校验、冻结、不允许额外字段）
- [a2c_smcp/computer/inputs/base.py](cci:7://file:///Users/JQQ/PycharmProjects/A2C-SMCP/python-computer-sdk/a2c_smcp/computer/inputs/base.py:0:0-0:0)：输入解析与缓存基类（按 id 缓存、清理、CRUD）
- [a2c_smcp/computer/cli/interactive_impl.py](cci:7://file:///Users/JQQ/PycharmProjects/A2C-SMCP/python-computer-sdk/a2c_smcp/computer/cli/interactive_impl.py:0:0-0:0)：交互命令（inputs 定义与缓存管理）
- [a2c_smcp/computer/computer.py](cci:7://file:///Users/JQQ/PycharmProjects/A2C-SMCP/python-computer-sdk/a2c_smcp/computer/computer.py:0:0-0:0)：启动时渲染与按需解析逻辑（`ConfigRender` + [InputResolver](cci:2://file:///Users/JQQ/PycharmProjects/A2C-SMCP/python-computer-sdk/a2c_smcp/computer/inputs/base.py:37:0-134:33)）