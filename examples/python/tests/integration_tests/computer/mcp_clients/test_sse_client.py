# -*- coding: utf-8 -*-
# filename: test_sse_client.py
# @Time    : 2025/8/19 19:28
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm
from collections.abc import Callable

import pytest
from mcp.client.session_group import SseServerParameters
from mcp.types import CallToolResult, Tool
from transitions import MachineError

from a2c_smcp.computer.mcp_clients.sse_client import SseMCPClient


@pytest.mark.asyncio
async def test_state_transitions(
    sse_server,
    sse_params: SseServerParameters,
    track_state: tuple[Callable[[str, str], None], list[tuple[str, str]]],
) -> None:
    """
    测试客户端状态转换
    Test client state transitions
    """
    callback, history = track_state
    client: SseMCPClient = SseMCPClient(sse_params, state_change_callback=callback)
    assert client.state == "initialized"
    await client.aconnect()
    assert client.state == "connected"
    assert ("initialized", "connected") in history
    await client.adisconnect()
    assert client.state == "disconnected"
    assert ("connected", "disconnected") in history
    await client.ainitialize()
    assert client.state == "initialized"
    assert ("disconnected", "initialized") in history


@pytest.mark.asyncio
async def test_list_tools(sse_server, sse_params: SseServerParameters) -> None:
    """
    测试获取工具列表功能
    Test list_tools functionality
    """
    client: SseMCPClient = SseMCPClient(sse_params)
    await client.aconnect()
    tools: list[Tool] = await client.list_tools()
    assert len(tools) == 2
    assert tools[0].name == "test_tool"
    assert tools[0].description == "A test tool"
    await client.adisconnect()


@pytest.mark.asyncio
async def test_call_tool_success(sse_server, sse_params: SseServerParameters) -> None:
    """
    测试成功调用工具
    Test successful tool call
    """
    client: SseMCPClient = SseMCPClient(sse_params)
    await client.aconnect()
    result: CallToolResult = await client.call_tool("test_tool", {})
    assert isinstance(result, CallToolResult)
    assert not result.isError
    assert result.content[0].text == "Called test_tool"
    await client.adisconnect()


@pytest.mark.asyncio
async def test_call_tool_failure(sse_server, sse_params: SseServerParameters) -> None:
    """
    测试工具调用失败场景
    Test tool call failure
    """
    client = SseMCPClient(sse_params)
    await client.aconnect()

    # 调用不存在的工具
    result = await client.call_tool("nonexistent_tool", {})
    assert result.isError, "调用不存在的工具应该失败"

    await client.adisconnect()


@pytest.mark.asyncio
async def test_async_session_property(sse_server, sse_params: SseServerParameters) -> None:
    """
    测试 async_session 属性
    Test async_session property
    """
    client = SseMCPClient(sse_params)
    # 未连接状态下会话为 None
    assert client._async_session is None

    # 访问 async_session 会自动连接
    session = await client.async_session
    assert session is not None

    await client.adisconnect()


@pytest.mark.asyncio
async def test_error_recovery(
    sse_server,
    sse_params: SseServerParameters,
    track_state: tuple[Callable[[str, str], None], list[tuple[str, str]]],
) -> None:
    """
    测试错误状态恢复
    Test error recovery
    """
    callback, history = track_state
    client = SseMCPClient(sse_params, state_change_callback=callback)
    await client.aconnect()

    # 强制进入错误状态
    await client.aerror()
    assert client.state == "error"
    assert any(from_state == "connected" and to_state == "error" for from_state, to_state in history)

    # 从错误状态恢复
    await client.ainitialize()
    assert client.state == "initialized"
    assert ("error", "initialized") in history

    # 尝试重新连接
    await client.aconnect()
    assert client.state == "connected"

    await client.adisconnect()


@pytest.mark.asyncio
async def test_invalid_state_operations(sse_server, sse_params: SseServerParameters) -> None:
    """
    测试在无效状态下执行操作
    Test invalid state operations
    """
    client: SseMCPClient = SseMCPClient(sse_params)
    with pytest.raises(ConnectionError):
        await client.call_tool("test_tool", {})
    with pytest.raises(ConnectionError):
        await client.list_tools()
    with pytest.raises(MachineError):
        await client.adisconnect()


@pytest.mark.asyncio
async def test_sse_message_handler_receives_list_changed_notifications(
    sse_server,
    sse_params: SseServerParameters,
) -> None:
    """
    中文: 验证在初始化 SseMCPClient 时传入 message_handler，能接收 listChanged 通知。
    英文: Verify that passing message_handler to SseMCPClient captures listChanged notifications.
    """
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

    client = SseMCPClient(sse_params, message_handler=message_handler)
    await client.aconnect()

    # 触发通知 / trigger notifications via server tool
    result = await client.call_tool("trigger_list_changed", {})
    assert isinstance(result, CallToolResult) or hasattr(result, "content")

    import anyio

    for _ in range(20):  # wait up to ~2s
        if received["tools"] >= 1 and received["resources"] >= 1 and received["prompts"] >= 1:
            break
        await anyio.sleep(0.1)

    assert received["tools"] >= 1
    assert received["resources"] >= 1
    assert received["prompts"] >= 1

    await client.adisconnect()
