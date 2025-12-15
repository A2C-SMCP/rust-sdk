# -*- coding: utf-8 -*-
# filename: stdio_client.py
# @Time    : 2025/8/19 10:55
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm
from collections.abc import Awaitable, Callable

from mcp import ClientSession, StdioServerParameters, stdio_client
from mcp.client.session import MessageHandlerFnT

from a2c_smcp.computer.mcp_clients.base_client import BaseMCPClient


class StdioMCPClient(BaseMCPClient[StdioServerParameters]):
    def __init__(
        self,
        params: StdioServerParameters,
        state_change_callback: Callable[[str, str], None | Awaitable[None]] | None = None,
        message_handler: MessageHandlerFnT | None = None,
    ) -> None:
        """
        初始化STDIO客户端，支持传入自定义 message_handler
        Initialize STDIO client with optional message_handler
        """
        assert isinstance(params, StdioServerParameters), "params must be an instance of StdioServerParameters"
        super().__init__(params, state_change_callback, message_handler)

    async def _create_async_session(self) -> ClientSession:
        """
        创建异步会话

        Returns:
            ClientSession: 异步会话
        """
        stdout, stdin = await self._aexit_stack.enter_async_context(stdio_client(self.params))
        # 如果提供了 message_handler，则一并传入 ClientSession
        # If message_handler is provided, pass it into ClientSession
        client_session = await self._aexit_stack.enter_async_context(
            ClientSession(stdout, stdin, message_handler=self._message_handler),
        )
        return client_session
