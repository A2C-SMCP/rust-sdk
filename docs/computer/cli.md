<!--
文件名: cli.md
作者: JQQ
创建日期: 2025/12/17
最后修改日期: 2025/12/17
版权: 2023 JQQ. All rights reserved.
依赖: None
描述: smcp-computer（Computer/CLI）命令行使用说明
-->

# smcp-computer（Computer/CLI）命令行使用说明（基础功能）

本文档基于当前仓库 `crates/smcp-computer/src/cli/*` 源码整理，用于说明如何在命令行启动 `smcp-computer`，并介绍基础功能使用方法。

---

## 1. 安装/启动方式

### 1.1 从源码运行（推荐开发态）

`smcp-computer` 二进制需要开启 `cli` feature（否则会提示 *CLI feature is not enabled*）。

在仓库根目录执行：

```bash
cargo run -p smcp-computer --features cli -- run
```

也可以省略 `run`（CLI 会默认执行 `run`）：

```bash
cargo run -p smcp-computer --features cli
```

### 1.2 构建二进制

```bash
cargo build -p smcp-computer --features cli
```

产物一般在：

- `target/debug/smcp-computer`
- 或 `target/release/smcp-computer`（加 `--release`）

---

## 2. 全局参数（启动参数）

`smcp-computer` 支持以下参数（均为 `--long` 形式）：

- `--auto-connect <bool>`
  - 是否自动连接（默认 `true`）
- `--auto-reconnect <bool>`
  - 是否自动重连（默认 `true`）
- `--url <string>`
  - Socket.IO 服务器 URL（可选），例如 `http://localhost:3000`
- `--namespace <string>`
  - Socket.IO 命名空间（默认：`/smcp`）
- `--auth <string>`
  - 认证参数（可选），格式：`key:value,foo:bar`
- `--headers <string>`
  - 请求头参数（可选），格式：`key:value,foo:bar`
- `--no-color`
  - 关闭彩色输出

示例：

```bash
cargo run -p smcp-computer --features cli -- \
  --url http://localhost:3000 \
  --namespace /smcp \
  --auth token:xxx,uid:123 \
  --headers x-env:dev \
  --auto-connect true \
  --auto-reconnect true
```

---

## 3. 子命令：run

### 3.1 进入持续运行 + 交互模式（REPL）

启动后会进入交互式提示符：

- 提示符：`a2c> `
- 输入 `help` 查看命令列表
- `quit` / `exit` 退出
- `Ctrl-D` 退出（EOF）

### 3.2 run 的可选参数

- `-c, --config <path>`
  - 启动时从文件加载 MCP Servers 配置（以及可选 inputs）
- `-i, --inputs <path>`
  - 启动时从文件加载 inputs 定义

示例：

```bash
cargo run -p smcp-computer --features cli -- run \
  --url http://localhost:3000 \
  -c ./config.json \
  -i ./inputs.json
```

---

## 4. 交互模式基础命令（REPL）

进入 REPL 后，核心就是三件事：

- 连接 Socket.IO（可选）
- 加载/管理 MCP servers 配置
- 启动 MCP clients、查看工具、调试工具调用

下面按常用流程介绍。

---

## 5. 推荐的“最小可用”操作流程

### 5.1 查看当前状态

```text
a2c> status
```

会输出：

- Socket.IO 是否已连接、URL、namespace、是否加入 office
- MCP Manager 是否初始化
- Active servers 数量
- 可用工具数量（Available Tools）

### 5.2 连接 Socket.IO（可选）

如果你启动时没有传 `--url`，可以在 REPL 里连接：

```text
a2c> socket connect http://localhost:3000
```

也可以不带 URL：它会尝试用启动参数里的 `--url`；如果也没有，则回退到 `http://localhost:3000`。

加入房间（office）：

```text
a2c> socket join <office_id> <computer_name>
```

离开房间：

```text
a2c> socket leave
```

### 5.3 加载/添加 MCP Servers 配置（初始化 MCP Manager 的关键）

你可以用 `server add` 直接传 JSON，或从文件读（`@file.json`）。下面以 Playwright MCP 为例，演示在 A2C-SMCP 中添加一个 MCP Server，并继续验证工具可以正常注册。

Playwright MCP 官方配置（供对照）：

```json
{"name":"playwright","command":"npx","args":["@playwright/mcp@latest"]}
```

A2C-SMCP 配置示例（`type=stdio` + `server_parameters` + 可选 `default_tool_meta`/`vrl`）：

```json
{
  "name": "playwright",
  "type": "stdio",
  "disabled": false,
  "forbidden_tools": [],
  "tool_meta": {},
  "default_tool_meta": {
    "tags": ["browser"],
    "auto_apply": true
  },
  "server_parameters": {
    "command": "npx",
    "args": ["@playwright/mcp@latest"],
    "env": null,
    "cwd": null,
    "encoding": "utf-8",
    "encoding_error_handler": "strict"
  },
  "vrl": "# 中文: 只对 browser_navigate 和 browser_navigate_back 工具进行转换\n# English: Only transform for browser_navigate and browser_navigate_back tools\nif .tool_name == \\\"browser_navigate\\\" || .tool_name == \\\"browser_navigate_back\\\" {\n    # 中文: 提取URL，browser_navigate_back 可能没有url参数\n    # English: Extract URL, browser_navigate_back may not have url parameter\n    url = if exists(.parameters.url) {\n        .parameters.url\n    } else {\n        \\\"[BROWSER_BACK_OPERATION]\\\"\n    }\n\n    # 中文: 提取内容 / English: Extract content\n    content = if length!(.content) > 0 {\n        .content[0].text\n    } else {\n        \\\"\\\"\n    }\n\n    # 中文: 使用重新赋值方式，只保留需要的字段\n    # English: Use reassignment to keep only needed fields\n    . = {\n        \\\"url\\\": url,\n        \\\"content\\\": content\n    }\n}"
}
```

方式 A：以命令行文本形式添加（注意 `vrl` 中引号与换行需要转义）

```text
a2c> server add {"name":"playwright","type":"stdio","disabled":false,"forbidden_tools":[],"tool_meta":{},"default_tool_meta":{"tags":["browser"],"auto_apply":true},"server_parameters":{"command":"npx","args":["@playwright/mcp@latest"],"env":null,"cwd":null,"encoding":"utf-8","encoding_error_handler":"strict"},"vrl":"# 中文: 只对 browser_navigate 和 browser_navigate_back 工具进行转换\n# English: Only transform for browser_navigate and browser_navigate_back tools\nif .tool_name == \\\"browser_navigate\\\" || .tool_name == \\\"browser_navigate_back\\\" {\n    # 中文: 提取URL，browser_navigate_back 可能没有url参数\n    # English: Extract URL, browser_navigate_back may not have url parameter\n    url = if exists(.parameters.url) {\n        .parameters.url\n    } else {\n        \\\"[BROWSER_BACK_OPERATION]\\\"\n    }\n\n    # 中文: 提取内容 / English: Extract content\n    content = if length!(.content) > 0 {\n        .content[0].text\n    } else {\n        \\\"\\\"\n    }\n\n    # 中文: 使用重新赋值方式，只保留需要的字段\n    # English: Use reassignment to keep only needed fields\n    . = {\n        \\\"url\\\": url,\n        \\\"content\\\": content\n    }\n}"}
```

方式 B：将上述 JSON 保存为文件后，以文件形式添加

```text
a2c> server add @path_to_json
```

另一组示例工具（iterm-mcp）：

```json
{
  "name": "iterm-mcp",
  "type": "stdio",
  "disabled": false,
  "forbidden_tools": [],
  "tool_meta": {},
  "default_tool_meta": null,
  "vrl": null,
  "server_parameters": {
    "command": "npx",
    "args": ["-y", "iterm-mcp"],
    "env": null,
    "cwd": null,
    "encoding": "utf-8",
    "encoding_error_handler": "strict"
  }
}
```

文本形式添加：

```text
a2c> server add {"name":"iterm-mcp","type":"stdio","disabled":false,"forbidden_tools":[],"tool_meta":{},"default_tool_meta":null,"vrl":null,"server_parameters":{"command":"npx","args":["-y","iterm-mcp"],"env":null,"cwd":null,"encoding":"utf-8","encoding_error_handler":"strict"}}
```

移除服务器：

```text
a2c> server rm <name>
```

查看当前 MCP 配置（servers + inputs）：

```text
a2c> mcp
```

### 5.4 启动/停止 MCP clients

启动某一个：

```text
a2c> start <name>
```

启动全部：

```text
a2c> start all
```

停止某一个：

```text
a2c> stop <name>
```

停止全部：

```text
a2c> stop all
```

启动后查看工具列表：

```text
a2c> tools
```

---

## 6. Inputs（输入变量）管理

这部分用于“渲染配置占位符 / 工具参数模板”等场景（CLI 提供了 `render` 来验证）。

### 6.1 从文件加载 inputs

注意：REPL 的 `inputs load` 要求参数形如 `@file.json`（带 `@`）。

```text
a2c> inputs load @./inputs.json
```

### 6.2 增删改查 inputs 定义

- 添加：

```text
a2c> inputs add @./one_input.json
```

- 更新：

```text
a2c> inputs update @./one_input.json
```

- 删除：

```text
a2c> inputs rm <id>
```

- 获取定义：

```text
a2c> inputs get <id>
```

- 列出所有 inputs 定义：

```text
a2c> inputs list
```

---

## 7. 调试：工具调用 / 渲染

### 7.1 tc：用 Socket.IO 一致 JSON 结构调试工具

帮助中提供：

```text
a2c> tc <json|@file>
```

具体 JSON 结构以系统的 tool-call 协议为准；CLI 本身会把 JSON 参数交给执行层。

### 7.2 render：测试渲染（占位符解析）

```text
a2c> render @./some_config.json
```

它会用当前缓存的 input values 去解析占位符（如果缺少 input，会提示 InputNotFound）。

---

## 8. 历史记录与桌面（占位）

### 8.1 历史记录

```text
a2c> history
a2c> history 20
```

显示最近 N 条工具调用记录（成功/失败/错误信息）。

### 8.2 desktop（当前是占位实现）

```text
a2c> desktop
a2c> desktop 10
a2c> desktop 10 test://uri
```

目前会返回占位 JSON（`windows: []`）。

---

## 9. 配置更新通知（Socket.IO）

当你希望通知服务端“配置更新了”，可以：

```text
a2c> notify update
```

---

## 10. 常见问题排查（FAQ）

### 10.1 启动后提示 CLI feature 未开启

原因：`smcp-computer` 的 `[[bin]]` 要求 `required-features = ["cli"]`。

解决：运行/构建时加 `--features cli`。

### 10.2 tools 显示为空 / tc 提示 MCP 管理器未初始化

原因：你还没有添加 server 配置并启动 client。

解决顺序：

1. `server add ...`
2. `start <name>` 或 `start all`
3. `tools` / `status` 再确认

### 10.3 Socket.IO 连接失败

- 确认 URL（协议/端口）可访问
- 需要鉴权时，正确传 `--auth key:value` 或在 `socket connect` 后按系统流程处理
- 必要时打开更详细日志（如果项目接入了 `tracing`/`RUST_LOG`，可用环境变量控制）
