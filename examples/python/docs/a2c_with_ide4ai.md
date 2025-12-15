# A2C SMCP Computer CLI 测试指南 - MCP 传输模式验证

本文档说明如何使用 A2C SMCP Computer CLI 测试三种不同的 MCP 传输模式（stdio、SSE、Streamable HTTP）。

## 前置准备

### 1. 安装测试用 MCP Server

我们使用 `py-ide4ai-mcp` 作为测试服务器，它支持所有三种传输模式。

```bash
# 使用 uvx 安装（推荐）
uvx ide4ai py-ide4ai-mcp --help

# 或者使用 pip 安装
pip install ide4ai
```

### 2. 启动 A2C SMCP Computer CLI

```bash
# 在项目根目录下启动 CLI
cd /Users/jqq/A2C-SMCP/python-sdk
python -m a2c_smcp.computer.cli
```

启动后会进入交互模式，显示提示符 `a2c>`。

---

## 测试场景 1: stdio 模式（标准输入输出）

### 特点
- **传输方式**: 通过标准输入输出（stdin/stdout）进行通信
- **适用场景**: 本地进程间通信，最常用的模式
- **优点**: 简单、稳定、无需网络配置

### 配置步骤

#### 1. 准备配置文件

创建 `stdio_config.json`:

**注意目前版本的ide4ai不支持自动创建 root_dir，因此在所有模式下启动ide4ai时，指定的 root_dir 需要使用者保证存在且可用。**

```json
{
  "name": "python-ide-stdio",
  "type": "stdio",
  "disabled": false,
  "forbidden_tools": [],
  "tool_meta": {},
  "server_parameters": {
    "command": "uvx",
    "args": [
      "ide4ai",
      "py-ide4ai-mcp",
      "--transport", "stdio",
      "--root-dir", "/tmp/test-workspace",
      "--project-name", "test-project",
      "--cmd-white-list", "ls,pwd,echo,cat,grep,find,head,tail,wc",
      "--cmd-timeout", "30"
    ],
    "env": null,
    "cwd": null,
    "encoding": "utf-8",
    "encoding_error_handler": "strict"
  }
}
```

#### 2. 在 CLI 中添加配置

**方式 1：使用文件（推荐）**

```bash
a2c> server add @stdio_config.json
```

**方式 2：使用行内 JSON（直接输入）**

```bash
server add {"name": "python-ide-stdio", "type": "stdio", "disabled": false, "forbidden_tools": [], "tool_meta": {}, "default_tool_meta": {"auto_apply": true}, "server_parameters": {"command": "uv", "args": ["run", "py-ide4ai-mcp", "--transport", "stdio", "--root-dir", "需要替换为实际的项目文件夹", "--project-name", "test-project", "--cmd-white-list", "ls,pwd,echo,cat,grep,find,head,tail,wc", "--cmd-timeout", "30"], "env": null, "cwd": "如果可执行文件在本地，需要指定至可执行文件的文件夹位置。如果是全局安装，设置为null", "encoding": "utf-8", "encoding_error_handler": "strict"}}
```

**预期输出**:
```
✅ 已添加/更新 MCP 配置: python-ide-stdio / Added/updated MCP config: python-ide-stdio
```

#### 3. 启动 MCP 客户端

```bash
a2c> start python-ide-stdio
```

**预期输出**:
```
✅ 已启动 MCP 客户端: python-ide-stdio / Started MCP client: python-ide-stdio
```

#### 4. 查看可用工具

```bash
a2c> tools
```

**预期输出**: 

```bash
                                         工具列表 / Tools                                         
┏━━━━━━┳━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┳━━━━━━━━━━━━┓
┃ Name ┃ Description                                                            ┃ Has Return ┃
┡━━━━━━╇━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━╇━━━━━━━━━━━━┩
│ Bash │ 在 IDE 环境中执行 Bash 命令 | Execute Bash commands in the IDE            │ Yes        │
│      │ environment                                                            │            │
└──────┴────────────────────────────────────────────────────────────────────────┴────────────┘
```

#### 5. 测试工具调用

使用行内 JSON 格式调用工具:

```bash
a2c> tc {"req_id":"test-sse-001","tool_name":"Bash","params":{"command":"echo", "args":  "Hello from Stdio mode!"},"timeout":30, "robot_id": "friday", "computer": "mock"}
```

**预期输出**:
```json
{
  "content": [
    {
      "type": "text",
      "text": "Hello from Stdio mode!"
    }
  ],
  "isError": false,
  "meta": null
}
```

#### 6. 更多测试命令

**测试 2: 列出文件**

```bash
a2c> tc {"req_id":"test-sse-003","tool_name":"Bash","params":{"command":"ls", "args": ["-l", "."]},"timeout":30, "robot_id": "friday", "computer": "mock"}
```

#### 8. 停止客户端和服务器

```bash
# 在 CLI 中停止客户端
a2c> stop python-ide-sse

# 在服务器终端按 Ctrl+C 停止服务器
```

#### 7. 停止客户端

```bash
a2c> stop python-ide-stdio
```

---

## 测试场景 2: SSE 模式（Server-Sent Events）

### 特点
- **传输方式**: 基于 HTTP 的单向流式传输（服务器到客户端）
- **适用场景**: Web 应用、远程访问
- **优点**: 支持跨网络访问，适合 Web 集成

### 配置步骤

#### 1. 启动 SSE 服务器

在**另一个终端**中启动 SSE 服务器:

**注意目前版本的ide4ai不支持自动创建 root_dir，因此在所有模式下启动ide4ai时，指定的 root_dir 需要使用者保证存在且可用。**

```bash
# 方式 1: 使用 uvx
uvx ide4ai py-ide4ai-mcp \
  --transport sse \
  --host 0.0.0.0 \
  --port 8000 \
  --root-dir /tmp/test-workspace \
  --project-name test-project \
  --cmd-white-list "ls,pwd,echo,cat,grep,find,head,tail,wc" \
  --cmd-timeout 30

# 方式 2: 使用环境变量
TRANSPORT=sse HOST=0.0.0.0 PORT=8000 PROJECT_ROOT=/tmp/test-workspace \
  uvx ide4ai py-ide4ai-mcp
```

**预期输出**:
```
Server running on http://0.0.0.0:8000
SSE endpoint: http://0.0.0.0:8000/sse
Messages endpoint: http://0.0.0.0:8000/messages/
```

#### 2. 准备配置文件

创建 `sse_config.json`:

```json
{
  "name": "python-ide-sse",
  "type": "sse",
  "disabled": false,
  "forbidden_tools": [],
  "tool_meta": {},
  "server_parameters": {
    "url": "http://localhost:8000/sse",
    "headers": null,
    "timeout": 30.0,
    "sse_read_timeout": 60.0
  }
}
```

#### 3. 在 CLI 中添加配置

**方式 1：使用文件（推荐）**

```bash
a2c> server add @sse_config.json
```

**方式 2：：使用双引号包裹并转义内部引号**

```bash
server add {"name": "python-ide-sse", "type": "sse", "disabled": false, "forbidden_tools": [], "tool_meta": {}, "default_tool_meta": {"auto_apply": true}, "server_parameters": {"url": "http://localhost:8000/sse", "headers": null, "timeout": 30.0, "sse_read_timeout": 60.0}}
```

**预期输出**:
```
✅ 已添加/更新 MCP 配置: python-ide-sse / Added/updated MCP config: python-ide-sse
# OR
✅ 服务器配置已添加/更新并正在启动 / Server config added/updated and starting
```

#### 4. 启动 MCP 客户端

```bash
a2c> start python-ide-sse
```

**预期输出**:
```
✅ 已启动 MCP 客户端: python-ide-sse / Started MCP client: python-ide-sse
```

#### 5. 查看可用工具

```bash
a2c> tools
```

#### 6. 测试工具调用

使用行内 JSON 格式调用工具:

```bash
a2c> tc {"req_id":"test-sse-001","tool_name":"Bash","params":{"command":"echo", "args":  "Hello from SSE mode!"},"timeout":30, "robot_id": "friday", "computer": "mock"}
```

**预期输出**:
```json
{
  "content": [
    {
      "type": "text",
      "text": "Hello from SSE mode!"
    }
  ],
  "isError": false,
  "meta": {
    "a2c_tool_meta": {
      "server_name": "python-ide-sse",
      "tool_name": "execute_bash_command"
    }
  }
}
```

#### 7. 更多测试命令

**测试 2: 列出文件**

```bash
a2c> tc {"req_id":"test-sse-003","tool_name":"Bash","params":{"command":"ls", "args": ["-l", "."]},"timeout":30, "robot_id": "friday", "computer": "mock"}
```

#### 8. 停止客户端和服务器

```bash
# 在 CLI 中停止客户端
a2c> stop python-ide-sse

# 在服务器终端按 Ctrl+C 停止服务器
```

---

## 测试场景 3: Streamable HTTP 模式

### 特点
- **传输方式**: 基于 HTTP 的双向流式传输
- **适用场景**: 需要流式响应的复杂场景
- **优点**: 支持流式数据传输，适合大数据量场景

### 配置步骤

#### 1. 启动 Streamable HTTP 服务器

在**另一个终端**中启动服务器:

**注意目前版本的ide4ai不支持自动创建 root_dir，因此在所有模式下启动ide4ai时，指定的 root_dir 需要使用者保证存在且可用。**

```bash
# 方式 1: 使用 uvx
uvx ide4ai py-ide4ai-mcp \
  --transport streamable-http \
  --host 0.0.0.0 \
  --port 8001 \
  --root-dir /tmp/test-workspace \
  --project-name test-project \
  --cmd-white-list "ls,pwd,echo,cat,grep,find,head,tail,wc" \
  --cmd-timeout 30

# 方式 2: 使用环境变量
TRANSPORT=streamable-http HOST=0.0.0.0 PORT=8001 PROJECT_ROOT=/tmp/test-workspace \
  uvx ide4ai py-ide4ai-mcp
```

**预期输出**:
```
Server running on http://0.0.0.0:8001
Message endpoint: http://0.0.0.0:8001/message
```

#### 2. 准备配置文件

创建 `streamable_http_config.json`:

```json
{
  "name": "python-ide-http",
  "type": "streamable",
  "disabled": false,
  "forbidden_tools": [],
  "tool_meta": {},
  "server_parameters": {
    "url": "http://localhost:8001/message",
    "headers": null,
    "timeout": "PT30S",
    "sse_read_timeout": "PT60S",
    "terminate_on_close": true
  }
}
```

#### 3. 在 CLI 中添加配置

**方式 1：使用文件（推荐）**

```bash
a2c> server add @streamable_http_config.json
```

**方式 2：使用行内 JSON（直接输入）**

```bash
server add {"name": "python-ide-http", "type": "streamable", "disabled": false, "forbidden_tools": [], "tool_meta": {}, "default_tool_meta": {"auto_apply": true}, "server_parameters": {"url": "http://localhost:8001/mcp", "headers": null, "timeout": "PT30S", "sse_read_timeout": "PT60S", "terminate_on_close": true}}
```

**预期输出**:
```
✅ 已添加/更新 MCP 配置: python-ide-http / Added/updated MCP config: python-ide-http
```

#### 4. 启动 MCP 客户端

```bash
a2c> start python-ide-http
```

**预期输出**:
```
✅ 已启动 MCP 客户端: python-ide-http / Started MCP client: python-ide-http
```

#### 5. 查看可用工具

```bash
a2c> tools
```

#### 6. 测试工具调用

使用行内 JSON 格式调用工具:

```bash
a2c> tc {"req_id":"test-sse-001","tool_name":"Bash","params":{"command":"echo", "args":  "Hello from Streamable mode!"},"timeout":30, "robot_id": "friday", "computer": "mock"}
```

**预期输出**:
```json
{
  "content": [
    {
      "type": "text",
      "text": "Hello from Streamable mode!"
    }
  ],
  "isError": false,
  "meta": {
    "a2c_tool_meta": {
      "server_name": "python-ide-sse",
      "tool_name": "execute_bash_command"
    }
  }
}
```

#### 7. 更多测试命令

**测试 2: 列出文件**

```bash
a2c> tc {"req_id":"test-sse-003","tool_name":"Bash","params":{"command":"ls", "args": ["-l", "."]},"timeout":30, "robot_id": "friday", "computer": "mock"}
```

#### 8. 停止客户端和服务器

```bash
# 在 CLI 中停止客户端
a2c> stop python-ide-http

# 在服务器终端按 Ctrl+C 停止服务器
```
