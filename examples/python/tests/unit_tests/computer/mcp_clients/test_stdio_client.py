# -*- coding: utf-8 -*-
# filename: test_stdio_client.py
# @Time    : 2025/8/19 16:43
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm
from unittest.mock import AsyncMock, MagicMock, patch

import pytest
from mcp import StdioServerParameters
from polyfactory.factories.pydantic_factory import ModelFactory

from a2c_smcp.computer.mcp_clients.stdio_client import StdioMCPClient


@pytest.mark.asyncio
async def test_abefore_connect_and_on_enter_connected():
    """
    测试 abefore_connect 与 on_enter_connected 的协作，确保 EventData.kwargs 能正确传递 stdout/stdin。
    Test abefore_connect and on_enter_connected, ensuring EventData.kwargs passes stdout/stdin correctly.
    """
    # 构造 mock 对象 Construct mock objects
    mock_stdout = MagicMock(name="stdout")
    mock_stdin = MagicMock(name="stdin")
    mock_session = AsyncMock(name="ClientSession")
    mock_initialize = AsyncMock()
    mock_session.initialize = mock_initialize

    mock_params = ModelFactory.create_factory(model=StdioServerParameters).build()

    # Patch stdio_client 和 ClientSession
    with (
        patch("a2c_smcp.computer.mcp_clients.stdio_client.stdio_client", new=AsyncMock(return_value=(mock_stdout, mock_stdin))),
        patch("a2c_smcp.computer.mcp_clients.stdio_client.ClientSession", new=AsyncMock(return_value=mock_session)) as mock_cs,
    ):
        # 构造 client 和 event
        client = StdioMCPClient(params=mock_params)
        assert client._async_session is None
        client._aexit_stack = AsyncMock()  # 避免真实上下文
        client._aexit_stack.enter_async_context = AsyncMock(side_effect=[(mock_stdout, mock_stdin), mock_session])
        # 调用 abefore_connect
        await client.aconnect()
        # 断言 ClientSession 被正确初始化
        mock_cs.assert_called_once_with(mock_stdout, mock_stdin, message_handler=None)
        # 断言 initialize 被调用
        mock_initialize.assert_awaited()
        # 断言 client._async_session 设置正确
        assert client._async_session is mock_session
        assert client._async_session is not None
