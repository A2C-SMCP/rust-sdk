# -*- coding: utf-8 -*-
# filename: test_utils.py
# @Time    : 2025/8/20 14:47
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm
from typing import Any

import pytest
from mcp import StdioServerParameters
from mcp.client.session_group import SseServerParameters

from a2c_smcp.computer.mcp_clients.model import SseServerConfig, StdioServerConfig
from a2c_smcp.computer.mcp_clients.sse_client import SseMCPClient
from a2c_smcp.computer.mcp_clients.stdio_client import StdioMCPClient
from a2c_smcp.computer.mcp_clients.utils import client_factory


@pytest.mark.anyio
async def test_factory(stdio_params: StdioServerParameters) -> None:
    """测试工厂函数可以正常创建"""
    std_config = StdioServerConfig(name="stdio_server", server_parameters=stdio_params)
    std_client = client_factory(std_config)
    assert isinstance(std_client, StdioMCPClient)
    assert std_client.state == "initialized"

    await std_client.aconnect()
    assert std_client.state == "connected"

    await std_client.adisconnect()
    assert std_client.state == "disconnected"


@pytest.mark.anyio
async def test_factory_multi_client(stdio_params: StdioServerParameters, sse_params: SseServerParameters, sse_server: Any) -> None:
    """测试工厂函数可以正常依次创建与关闭多个客户端"""
    std_config = StdioServerConfig(name="stdio_server", server_parameters=stdio_params)
    std_client = client_factory(std_config)
    assert isinstance(std_client, StdioMCPClient)
    assert std_client.state == "initialized"

    await std_client.aconnect()
    assert std_client.state == "connected"

    await std_client.adisconnect()
    assert std_client.state == "disconnected"

    sse_config = SseServerConfig(name="sse_server", server_parameters=sse_params)
    sse_client = client_factory(sse_config)
    assert isinstance(sse_client, SseMCPClient)
    assert sse_client.state == "initialized"

    await sse_client.aconnect()
    assert sse_client.state == "connected"

    await sse_client.adisconnect()
    assert sse_client.state == "disconnected"


# 测试两个不同客户端依次创建，然后再依次连接，再依次断开
# 参考： tests/unit_tests/utils/test_async_exit_stack_anyio.py 因为MCP内部使用了anyio的task_group，因此退出时必须按入栈顺序退出，否则会出错
@pytest.mark.anyio
async def test_factory_multi_client2(stdio_params: StdioServerParameters, sse_params: SseServerParameters, sse_server: Any) -> None:
    std_config = StdioServerConfig(name="stdio_server", server_parameters=stdio_params)
    std_client = client_factory(std_config)
    assert isinstance(std_client, StdioMCPClient)
    assert std_client.state == "initialized"

    sse_config = SseServerConfig(name="sse_server", server_parameters=sse_params)
    sse_client = client_factory(sse_config)
    assert isinstance(sse_client, SseMCPClient)
    assert sse_client.state == "initialized"

    await std_client.aconnect()
    assert std_client.state == "connected"

    await sse_client.aconnect()
    assert sse_client.state == "connected"

    await std_client.adisconnect()
    assert std_client.state == "disconnected"

    await sse_client.adisconnect()
    assert sse_client.state == "disconnected"
