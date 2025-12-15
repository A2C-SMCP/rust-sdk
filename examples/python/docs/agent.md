# Agent模块使用文档 / Agent Module Usage Documentation

A2C-SMCP Agent模块提供了Agent端的SMCP协议客户端实现，支持同步和异步两种模式。

## 概述 / Overview

Agent模块主要包含以下组件：

- **认证系统** / Authentication System: 提供抽象的认证接口和默认实现
- **客户端实现** / Client Implementation: 同步和异步的SMCP协议客户端
- **事件处理** / Event Handling: 灵活的事件处理机制
- **类型定义** / Type Definitions: 完整的类型系统支持

## 快速开始 / Quick Start

### 基本使用 / Basic Usage

```python
from a2c_smcp.agent import DefaultAgentAuthProvider, SMCPAgentClient

# 创建认证提供者
auth_provider = DefaultAgentAuthProvider(
    agent_id="my_agent",
    office_id="my_office",
    api_key="your_api_key"
)

# 创建同步客户端
client = SMCPAgentClient(auth_provider=auth_provider)

# 连接到服务器
client.connect_to_server("http://localhost:8000")

# 调用远程工具
result = client.emit_tool_call(
    computer="target_computer",
    tool_name="example_tool",
    params={"param1": "value1"},
    timeout=30
)

print(result)
```

### 异步使用 / Async Usage

```python
import asyncio
from a2c_smcp.agent import DefaultAgentAuthProvider, AsyncSMCPAgentClient

async def main():
    # 创建认证提供者
    auth_provider = DefaultAgentAuthProvider(
        agent_id="my_agent",
        office_id="my_office",
        api_key="your_api_key"
    )

    # 创建异步客户端
    client = AsyncSMCPAgentClient(auth_provider=auth_provider)

    # 连接到服务器
    await client.connect_to_server("http://localhost:8000")

    # 异步调用远程工具
    result = await client.emit_tool_call(
        computer="target_computer",
        tool_name="example_tool",
        params={"param1": "value1"},
        timeout=30
    )

    print(result)

# 运行异步函数
asyncio.run(main())
```

## 认证系统 / Authentication System

### 自定义认证提供者 / Custom Authentication Provider

```python
from a2c_smcp.agent import AgentAuthProvider, AgentConfig

class MyAuthProvider(AgentAuthProvider):
    def __init__(self, agent_id: str, office_id: str):
        self.agent_id = agent_id
        self.office_id = office_id

    def get_agent_id(self) -> str:
        return self.agent_id

    def get_connection_auth(self) -> dict | None:
        # 返回Socket.IO连接认证数据
        return {"token": "my_custom_token"}

    def get_connection_headers(self) -> dict[str, str]:
        # 返回HTTP请求头
        return {"Authorization": "Bearer my_token"}

    def get_agent_config(self) -> AgentConfig:
        return AgentConfig(
            agent=self.agent_id,
            office_id=self.office_id
        )

# 使用自定义认证提供者
auth_provider = MyAuthProvider("my_agent", "my_office")
client = SMCPAgentClient(auth_provider=auth_provider)
```

## 事件处理 / Event Handling

### 同步事件处理器 / Synchronous Event Handler

```python
from a2c_smcp.agent.types import AgentEventHandler
from a2c_smcp.smcp import EnterOfficeNotification, LeaveOfficeNotification, UpdateMCPConfigNotification, SMCPTool

class MyEventHandler:
    def on_computer_enter_office(self, data: EnterOfficeNotification, client: SMCPAgentClient) -> None:
        print(f"Computer {data['computer']} joined office {data['office_id']}")

    def on_computer_leave_office(self, data: LeaveOfficeNotification, client: SMCPAgentClient) -> None:
        print(f"Computer {data['computer']} left office {data['office_id']}")

    def on_computer_update_config(self, data: UpdateMCPConfigNotification, client: SMCPAgentClient) -> None:
        print(f"Computer {data['computer']} updated configuration")

    def on_tools_received(self, computer: str, tools: list[SMCPTool], client: SMCPAgentClient) -> None:
        print(f"Received {len(tools)} tools from computer {computer}")
        for tool in tools:
            print(f"  - {tool['name']}: {tool['description']}")

# 使用事件处理器
event_handler = MyEventHandler()
client = SMCPAgentClient(
    auth_provider=auth_provider,
    event_handler=event_handler
)
```

### 异步事件处理器 / Asynchronous Event Handler

```python
from a2c_smcp.agent.types import AsyncAgentEventHandler

class MyAsyncEventHandler:
    async def on_computer_enter_office(self, data: EnterOfficeNotification, client: AsyncSMCPAgentClient) -> None:
        # 异步处理Computer加入事件
        await self.update_computer_registry(data['computer'])

    async def on_computer_leave_office(self, data: LeaveOfficeNotification, client: AsyncSMCPAgentClient) -> None:
        # 异步处理Computer离开事件
        await self.cleanup_computer_data(data['computer'])

    async def on_computer_update_config(self, data: UpdateMCPConfigNotification, client: AsyncSMCPAgentClient) -> None:
        # 异步处理配置更新事件
        await self.refresh_computer_config(data['computer'])

    async def on_tools_received(self, computer: str, tools: list[SMCPTool], client: AsyncSMCPAgentClient) -> None:
        # 异步处理工具列表接收事件
        await self.register_tools(computer, tools)

    async def update_computer_registry(self, computer: str) -> None:
        # 自定义异步逻辑
        pass

    async def cleanup_computer_data(self, computer: str) -> None:
        # 自定义异步逻辑
        pass

    async def refresh_computer_config(self, computer: str) -> None:
        # 自定义异步逻辑
        pass

    async def register_tools(self, computer: str, tools: list[SMCPTool]) -> None:
        # 自定义异步逻辑
        pass

# 使用异步事件处理器
event_handler = MyAsyncEventHandler()
client = AsyncSMCPAgentClient(
    auth_provider=auth_provider,
    event_handler=event_handler
)
```

## 工具调用 / Tool Calling

### 基本工具调用 / Basic Tool Calling

```python
from mcp.types import CallToolResult

# 同步工具调用
result: CallToolResult = client.emit_tool_call(
    computer="target_computer",
    tool_name="file_read",
    params={"path": "/path/to/file.txt"},
    timeout=30
)

if result.isError:
    print(f"Tool call failed: {result.content}")
else:
    print(f"Tool call succeeded: {result.content}")

# 异步工具调用
result: CallToolResult = await async_client.emit_tool_call(
    computer="target_computer",
    tool_name="file_read",
    params={"path": "/path/to/file.txt"},
    timeout=30
)
```

### 获取工具列表 / Get Tools List

```python
# 同步获取工具列表
tools_response = client.get_tools_from_computer("target_computer", timeout=20)
print(f"Available tools: {len(tools_response['tools'])}")

# 异步获取工具列表
tools_response = await async_client.get_tools_from_computer("target_computer", timeout=20)
print(f"Available tools: {len(tools_response['tools'])}")
```

### 获取桌面信息 / Get Desktop Information

```python
# 同步获取桌面信息
desktop_response = client.get_desktop_from_computer(
    "target_computer",
    size=10,  # 限制窗口数量
    window="window://specific_window",  # 可选：指定窗口URI
    timeout=20
)
print(f"Desktop windows: {len(desktop_response['desktops'])}")

# 异步获取桌面信息
desktop_response = await async_client.get_desktop_from_computer(
    "target_computer",
    size=10,
    window="window://specific_window",
    timeout=20
)
print(f"Desktop windows: {len(desktop_response['desktops'])}")
```

### 获取房间内的Computer列表 / Get Computers in Office

```python
from a2c_smcp.smcp import SessionInfo

# 同步获取房间内的所有Computer
computers: list[SessionInfo] = client.get_computers_in_office("my_office", timeout=20)
for computer in computers:
    print(f"Computer: {computer['name']} (sid: {computer['sid']})")

# 异步获取房间内的所有Computer
computers: list[SessionInfo] = await async_client.get_computers_in_office("my_office", timeout=20)
for computer in computers:
    print(f"Computer: {computer['name']} (sid: {computer['sid']})")
```

## 错误处理 / Error Handling

### 连接错误处理 / Connection Error Handling

```python
try:
    client.connect_to_server("http://localhost:8000")
    print("Connected successfully")
except Exception as e:
    print(f"Connection failed: {e}")
```

### 工具调用错误处理 / Tool Call Error Handling

```python
try:
    result = client.emit_tool_call(
        computer="target_computer",
        tool_name="risky_tool",
        params={},
        timeout=10
    )
    
    if result.isError:
        print(f"Tool execution error: {result.content}")
    else:
        print(f"Tool executed successfully: {result.content}")
        
except TimeoutError:
    print("Tool call timed out")
except Exception as e:
    print(f"Unexpected error: {e}")
```

## 配置选项 / Configuration Options

### 默认认证提供者配置 / Default Auth Provider Configuration

```python
auth_provider = DefaultAgentAuthProvider(
    agent_id="my_agent",              # Agent唯一标识
    office_id="my_office",            # 办公室ID
    api_key="your_api_key",           # API密钥
    api_key_header="x-api-key",       # API密钥请求头名称
    extra_headers={                   # 额外请求头
        "User-Agent": "MyAgent/1.0",
        "Custom-Header": "custom_value"
    },
    auth_data={                       # 额外认证数据
        "token": "auth_token",
        "user_id": "user_123"
    }
)
```

### Socket.IO连接配置 / Socket.IO Connection Configuration

```python
# 同步客户端连接配置
client.connect_to_server(
    url="http://localhost:8000",
    namespace="/smcp",
    transports=["websocket"],
    wait_timeout=10
)

# 异步客户端连接配置
await async_client.connect_to_server(
    url="http://localhost:8000",
    namespace="/smcp",
    transports=["websocket"],
    wait_timeout=10
)
```

## 最佳实践 / Best Practices

### 1. 资源管理 / Resource Management

```python
# 同步客户端资源管理
try:
    client = SMCPAgentClient(auth_provider=auth_provider)
    client.connect_to_server("http://localhost:8000")
    
    # 执行业务逻辑
    result = client.emit_tool_call(...)
    
finally:
    # 确保断开连接
    if client.connected:
        client.disconnect()

# 异步客户端资源管理
async with AsyncSMCPAgentClient(auth_provider=auth_provider) as client:
    await client.connect_to_server("http://localhost:8000")
    
    # 执行业务逻辑
    result = await client.emit_tool_call(...)
    
    # 自动断开连接
```

### 2. 错误重试 / Error Retry

```python
import time
from typing import Optional

def retry_tool_call(
    client: SMCPAgentClient,
    computer: str,
    tool_name: str,
    params: dict,
    max_retries: int = 3,
    timeout: int = 30
) -> Optional[CallToolResult]:
    """带重试机制的工具调用"""
    
    for attempt in range(max_retries):
        try:
            result = client.emit_tool_call(computer, tool_name, params, timeout)
            
            if not result.isError:
                return result
                
            print(f"Tool call failed (attempt {attempt + 1}): {result.content}")
            
        except Exception as e:
            print(f"Tool call exception (attempt {attempt + 1}): {e}")
            
        if attempt < max_retries - 1:
            time.sleep(2 ** attempt)  # 指数退避
            
    return None
```

### 3. 日志记录 / Logging

```python
import logging
from a2c_smcp.utils.logger import logger

# 配置日志级别
logger.setLevel(logging.DEBUG)

# 在事件处理器中使用日志
class LoggingEventHandler:
    def on_computer_enter_office(self, data: EnterOfficeNotification, client: SMCPAgentClient) -> None:
        logger.info(f"Computer {data['computer']} entered office {data['office_id']}")
        
    def on_tools_received(self, computer: str, tools: list[SMCPTool], client: SMCPAgentClient) -> None:
        logger.debug(f"Received {len(tools)} tools from {computer}")
        for tool in tools:
            logger.debug(f"  Tool: {tool['name']}")
```

## 故障排除 / Troubleshooting

### 常见问题 / Common Issues

1. **连接失败** / Connection Failed
   - 检查服务器URL是否正确
   - 验证网络连接
   - 确认认证信息是否有效

2. **工具调用超时** / Tool Call Timeout
   - 增加超时时间
   - 检查目标Computer是否在线
   - 验证工具名称和参数是否正确

3. **事件处理器未被调用** / Event Handler Not Called
   - 确认事件处理器已正确注册
   - 检查办公室ID是否匹配
   - 验证Socket.IO连接状态

### 调试技巧 / Debugging Tips

```python
# 启用详细日志
import logging
logging.basicConfig(level=logging.DEBUG)

# 检查连接状态
if client.connected:
    print("Client is connected")
else:
    print("Client is not connected")

# 监听所有事件（调试用）
@client.on('*')
def catch_all(event, *args):
    print(f"Received event: {event}, args: {args}")
```

## API参考 / API Reference

详细的API文档请参考各个模块的docstring注释。主要类和方法包括：

- `AgentAuthProvider`: 认证提供者抽象基类
- `DefaultAgentAuthProvider`: 默认认证提供者实现
- `SMCPAgentClient`: 同步SMCP Agent客户端
- `AsyncSMCPAgentClient`: 异步SMCP Agent客户端
- `AgentEventHandler`: 同步事件处理器协议
- `AsyncAgentEventHandler`: 异步事件处理器协议
