# -*- coding: utf-8 -*-
# filename: sse_client.py
# @Time    : 2025/8/19 10:55
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm
from collections.abc import Awaitable, Callable

from mcp import ClientSession
from mcp.client.session import MessageHandlerFnT
from mcp.client.session_group import SseServerParameters
from mcp.client.sse import sse_client

from a2c_smcp.computer.mcp_clients.base_client import BaseMCPClient


class SseMCPClient(BaseMCPClient[SseServerParameters]):
    def __init__(
        self,
        params: SseServerParameters,
        state_change_callback: Callable[[str, str], None | Awaitable[None]] | None = None,
        message_handler: MessageHandlerFnT | None = None,
    ) -> None:
        """
        初始化SSE客户端，支持传入自定义 message_handler
        Initialize SSE client with optional message_handler
        """
        assert isinstance(params, SseServerParameters), "params must be an instance of SseServerParameters"
        super().__init__(params, state_change_callback, message_handler)

    async def _create_async_session(self) -> ClientSession:
        """
        创建异步会话

        Returns:
            ClientSession: 异步会话
        """
        aread_stream, awrite_stream = await self._aexit_stack.enter_async_context(sse_client(**self.params.model_dump(mode="python")))
        # 如果提供了 message_handler，则一并传入 ClientSession
        # If message_handler is provided, pass it into ClientSession
        client_session = await self._aexit_stack.enter_async_context(
            ClientSession(aread_stream, awrite_stream, message_handler=self._message_handler),
        )
        return client_session
