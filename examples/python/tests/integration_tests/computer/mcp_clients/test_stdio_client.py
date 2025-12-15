# -*- coding: utf-8 -*-
# filename: test_stdio_client.py
# @Time    : 2025/8/19 17:00
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm
# filename: test_std_mcp_client.py
import sys
from collections.abc import Callable
from pathlib import Path

import pytest
from mcp import StdioServerParameters
from mcp.types import (
    CallToolResult,
    PromptListChangedNotification,
    ResourceListChangedNotification,
    ServerNotification,
    ToolListChangedNotification,
)
from pydantic import BaseModel
from transitions import MachineError

from a2c_smcp.computer.mcp_clients.stdio_client import StdioMCPClient


@pytest.mark.asyncio
async def test_state_transitions(
    stdio_params: StdioServerParameters,
    track_state: tuple[Callable[[str, str], None], list[tuple[str, str]]],
) -> None:
    """测试客户端状态转换 Test client state transitions"""
    callback, history = track_state

    client: StdioMCPClient = StdioMCPClient(stdio_params, state_change_callback=callback)

    # 初始状态检查 Initial state check
    assert client.state == "initialized"

    # 连接服务器 Connect to server
    await client.aconnect()
    assert client.state == "connected"
    assert ("initialized", "connected") in history

    # 断开连接 Disconnect
    await client.adisconnect()
    assert client.state == "disconnected"
    assert ("connected", "disconnected") in history

    # 重新初始化 Re-initialize
    await client.ainitialize()
    assert client.state == "initialized"
    assert ("disconnected", "initialized") in history


@pytest.mark.asyncio
async def test_list_tools(stdio_params: StdioServerParameters) -> None:
    """测试获取工具列表功能 Test getting tool list"""
    client: StdioMCPClient = StdioMCPClient(stdio_params)
    await client.aconnect()

    tools = await client.list_tools()

    # 验证工具列表 Validate tool list
    assert len(tools) == 1
    assert tools[0].name == "hello"
    assert tools[0].description == "Say hello to someone."

    # 清理客户端 Cleanup client
    await client.adisconnect()


@pytest.mark.asyncio
async def test_call_tool_success(stdio_params: StdioServerParameters) -> None:
    """测试成功调用工具 Test successful tool call"""
    client: StdioMCPClient = StdioMCPClient(stdio_params)
    await client.aconnect()

    # 使用默认参数调用 Call with default params
    result_default: CallToolResult = await client.call_tool("hello", {})
    assert isinstance(result_default, CallToolResult)
    assert not result_default.isError
    assert result_default.content[0].text == "Hello, World!"

    # 使用自定义参数调用 Call with custom params
    result_custom: CallToolResult = await client.call_tool("hello", {"name": "Alice"})
    assert not result_custom.isError
    assert result_custom.content[0].text == "Hello, Alice!"

    # 清理客户端 Cleanup client
    await client.adisconnect()


@pytest.mark.asyncio
async def test_call_tool_failure(stdio_params: StdioServerParameters) -> None:
    """测试工具调用失败场景 Test tool call failure"""
    client: StdioMCPClient = StdioMCPClient(stdio_params)
    await client.aconnect()

    # 调用不存在的工具 Call non-existent tool
    result: CallToolResult = await client.call_tool("nonexistent_tool", {})
    assert result.isError, "调用不存在的工具应该失败 Calling non-existent tool should fail"

    # 调用工具但参数错误 Call tool with wrong params
    result = await client.call_tool("hello", {"invalid_param": "value"})
    assert isinstance(result, CallToolResult)
    # 虽然参数错误，但由于mcp server有一定的容错能力，可以返回成功 Even with wrong params, mcp server may return success
    assert not result.isError
    assert result.content[0].text == "Hello, World!"

    # 清理客户端 Cleanup client
    await client.adisconnect()


@pytest.mark.asyncio
async def test_async_session_property(stdio_params: StdioServerParameters) -> None:
    """测试 async_session 属性 Test async_session property"""
    client: StdioMCPClient = StdioMCPClient(stdio_params)

    # 未连接状态下会话为空 Session is None when not connected
    assert client._async_session is None

    # 在未连接状态下访问 @async_property async_session 会触发自动连接，因此会话不为空 Accessing async_session triggers auto-connect
    assert (await client.async_session) is not None

    with pytest.raises(MachineError) as e:
        # 在连接状态下访问 Access when connected
        await client.aconnect()

    assert "Can't trigger event aconnect from state connected!" in str(e.value)
    session = await client.async_session
    assert session is not None

    # 清理客户端 Cleanup client
    await client.adisconnect()


@pytest.mark.asyncio
async def test_invalid_state_operations(stdio_params: StdioServerParameters) -> None:
    """测试在无效状态下执行操作 Test operations in invalid state"""
    client: StdioMCPClient = StdioMCPClient(stdio_params)

    # 在未连接状态下调用工具 Call tool when not connected
    with pytest.raises(ConnectionError):
        await client.call_tool("hello", {})

    # 在未连接状态下获取工具列表 Get tool list when not connected
    with pytest.raises(ConnectionError):
        await client.list_tools()

    # 尝试在未连接状态下断开 Try disconnect when not connected
    with pytest.raises(MachineError) as e:
        await client.adisconnect()
    assert "Can't trigger event adisconnect from state initialized!" in str(e.value)


@pytest.mark.asyncio
async def test_error_recovery(
    stdio_params: StdioServerParameters,
    track_state: tuple[Callable[[str, str], None], list[tuple[str, str]]],
) -> None:
    """测试错误状态恢复 Test error recovery"""
    callback, history = track_state

    client: StdioMCPClient = StdioMCPClient(stdio_params, state_change_callback=callback)
    await client.aconnect()

    # 强制进入错误状态 Force error state
    await client.aerror()
    assert client.state == "error"
    assert any(from_state == "connected" and to_state == "error" for from_state, to_state in history)

    # 从错误状态恢复 Recover from error
    await client.ainitialize()
    assert client.state == "initialized"
    assert ("error", "initialized") in history

    # 尝试重新连接 Try reconnect
    await client.aconnect()
    assert client.state == "connected"

    # 清理 Cleanup
    await client.adisconnect()


@pytest.mark.asyncio
async def test_stdio_tools_list_and_call():
    """
    中文: 连接带工具的服务器，列出工具并调用 echo。
    英文: Connect to server with tools, list tools and call echo.
    """
    server_py = Path(__file__).resolve().parents[2] / "computer" / "mcp_servers" / "tool_stdio_server.py"
    assert server_py.exists(), f"server script not found: {server_py}"

    params = StdioServerParameters(command=sys.executable, args=[str(server_py)])

    client = StdioMCPClient(params)

    # 连接 / connect
    await client.aconnect()
    await client._create_session_success_event.wait()

    # 初始化结果检查 / initialize_result
    assert client.initialize_result is not None
    assert client.initialize_result.serverInfo.name == "tool-itest-server"
    assert client.initialize_result.capabilities.tools, "tools配置非空"
    assert not client.initialize_result.capabilities.tools.listChanged, "工具列表未开始变化通知"

    # 获取工具列表 / list tools
    tools = await client.list_tools()
    names = {t.name for t in tools}
    assert "echo" in names

    # 调用工具 / call tool
    result = await client.call_tool("echo", {"text": "hello"})
    # CallToolResult.content 是 list[ContentBlock]，期望第一条为 TextContent("echo: hello")
    assert result.content and getattr(result.content[0], "type", None) == "text"
    assert getattr(result.content[0], "text", None) == "echo: hello"

    # 断开 / disconnect
    await client.adisconnect()
    await client._async_session_closed_event.wait()
    assert client.initialize_result is None


class DummyParams(BaseModel):
    pass


@pytest.mark.asyncio
async def test_stdio_initialize_result_end_to_end():
    """
    中文: 通过真实stdio子进程建立连接，校验 initialize_result 的赋值与清理。
    英文: Establish real stdio connection via subprocess and validate initialize_result set and cleared.
    """
    # 服务器脚本路径 / server script path
    server_py = Path(__file__).resolve().parents[2] / "computer" / "mcp_servers" / "minimal_stdio_server.py"
    assert server_py.exists(), f"server script not found: {server_py}"

    # 构建 StdioServerParameters，使用当前解释器运行脚本 / use current interpreter to run script
    params = StdioServerParameters(command=sys.executable, args=[str(server_py)])

    client = StdioMCPClient(params)

    # 连接 / connect
    await client.aconnect()
    await client._create_session_success_event.wait()

    # 验证初始化结果存在且包含服务器信息 / ensure initialize_result present with server info
    assert client.initialize_result is not None
    assert client.initialize_result.serverInfo.name == "itest-server"

    # 断开 / disconnect
    await client.adisconnect()
    await client._async_session_closed_event.wait()

    # 验证清理 / ensure cleared
    assert client.initialize_result is None


@pytest.mark.asyncio
async def test_stdio_message_handler_receives_list_changed_notifications() -> None:
    """
    中文: 验证在初始化 StdioMCPClient 时传入 message_handler，能接收 listChanged 通知。
    英文: Verify that passing message_handler to StdioMCPClient captures listChanged notifications.
    """
    server_py = Path(__file__).resolve().parents[2] / "computer" / "mcp_servers" / "notifications_stdio_server.py"
    assert server_py.exists(), f"server script not found: {server_py}"

    params = StdioServerParameters(command=sys.executable, args=[str(server_py)])

    received = {"tools": 0, "resources": 0, "prompts": 0}

    async def message_handler(message):
        from mcp.types import (
            PromptListChangedNotification,
            ResourceListChangedNotification,
            ServerNotification,
            ToolListChangedNotification,
        )

        if isinstance(message, ServerNotification):
            if isinstance(message.root, ToolListChangedNotification):
                received["tools"] += 1
            elif isinstance(message.root, ResourceListChangedNotification):
                received["resources"] += 1
            elif isinstance(message.root, PromptListChangedNotification):
                received["prompts"] += 1

    client = StdioMCPClient(params, message_handler=message_handler)

    await client.aconnect()
    await client._create_session_success_event.wait()

    # 触发通知 / trigger notifications
    result = await client.call_tool("trigger_changes", {})
    assert result.content and getattr(result.content[0], "type", None) == "text"

    import anyio

    # 等待异步消息处理 / wait up to ~2s
    for _ in range(20):
        if received["tools"] >= 1 and received["resources"] >= 1 and received["prompts"] >= 1:
            break
        await anyio.sleep(0.1)

    assert received["tools"] >= 1
    assert received["resources"] >= 1
    assert received["prompts"] >= 1

    await client.adisconnect()
