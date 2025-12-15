# -*- coding: utf-8 -*-
# filename: test_http_client.py
# @Time    : 2025/8/20 10:12
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm
from collections.abc import Generator

import pytest
from mcp.client.session_group import StreamableHttpParameters

from a2c_smcp.computer.mcp_clients.http_client import HttpMCPClient


@pytest.mark.anyio
async def test_state_transitions_basic(basic_server: Generator[None, None, None], http_client: HttpMCPClient) -> None:
    """
    # 测试HttpMCPClient状态转移
    # Test HttpMCPClient state transitions
    """
    # 这里只做基础连接与断开，详细功能后续补充
    # 连接
    await http_client.aconnect()
    assert http_client.state == "connected"
    # 断开
    await http_client.adisconnect()
    assert http_client.state == "disconnected"


@pytest.mark.anyio
async def test_list_tools_basic(basic_server: Generator[None, None, None], http_client: HttpMCPClient) -> None:
    """
    # 测试获取工具列表功能
    # Test list_tools functionality
    """
    await http_client.aconnect()
    tools = await http_client.list_tools()
    assert isinstance(tools, list)
    assert any(tool.name == "test_tool" for tool in tools)
    await http_client.adisconnect()


@pytest.mark.anyio
async def test_call_tool_success_basic(basic_server: Generator[None, None, None], http_client: HttpMCPClient) -> None:
    """
    # 测试成功调用工具
    # Test successful tool call
    """
    await http_client.aconnect()
    result = await http_client.call_tool("test_tool", {})
    assert hasattr(result, "content")
    assert result.content[0].text.startswith("Called test_tool")
    await http_client.adisconnect()


@pytest.mark.anyio
async def test_call_tool_failure_basic(basic_server: Generator[None, None, None], http_client: HttpMCPClient) -> None:
    """
    # 测试调用不存在的工具失败
    # Test tool call failure for non-existent tool
    """
    await http_client.aconnect()
    result = await http_client.call_tool("nonexistent_tool", {})
    assert getattr(result, "isError", True)
    await http_client.adisconnect()


@pytest.mark.anyio
async def test_async_session_property_basic(basic_server: Generator[None, None, None], http_client: HttpMCPClient) -> None:
    """
    # 测试 async_session 属性行为
    # Test async_session property
    """
    assert http_client._async_session is None
    session = await http_client.async_session
    assert session is not None
    await http_client.adisconnect()


@pytest.mark.anyio
async def test_invalid_state_operations_basic(basic_server: Generator[None, None, None], http_params: StreamableHttpParameters) -> None:
    """
    # 测试无效状态下的操作
    # Test operations in invalid state
    """
    from transitions import MachineError

    client = HttpMCPClient(http_params)
    with pytest.raises(ConnectionError):
        await client.call_tool("test_tool", {})
    with pytest.raises(ConnectionError):
        await client.list_tools()
    with pytest.raises(MachineError):
        await client.adisconnect()


@pytest.mark.anyio
async def test_http_message_handler_receives_list_changed_notifications(
    basic_server: Generator[None, None, None],
    http_params: StreamableHttpParameters,
) -> None:
    """
    中文: 验证在初始化 HttpMCPClient 时传入 message_handler，能接收 listChanged 通知。
    英文: Verify that passing message_handler to HttpMCPClient captures listChanged notifications.
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

    client = HttpMCPClient(http_params, message_handler=message_handler)
    await client.aconnect()

    # 触发通知 / trigger notifications via server tool
    result = await client.call_tool("trigger_list_changed", {})
    assert hasattr(result, "content")

    import anyio

    for _ in range(20):  # wait up to ~2s
        if received["tools"] >= 1 and received["resources"] >= 1 and received["prompts"] >= 1:
            break
        await anyio.sleep(0.1)

    assert received["tools"] >= 1
    assert received["resources"] >= 1
    assert received["prompts"] >= 1

    await client.adisconnect()
