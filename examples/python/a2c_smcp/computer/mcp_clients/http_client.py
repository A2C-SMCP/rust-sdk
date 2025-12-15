# -*- coding: utf-8 -*-
# filename: http_client.py
# @Time    : 2025/8/19 10:55
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm
from collections.abc import Awaitable, Callable

from mcp import ClientSession
from mcp.client.session import MessageHandlerFnT
from mcp.client.session_group import StreamableHttpParameters
from mcp.client.streamable_http import streamablehttp_client

from a2c_smcp.computer.mcp_clients.base_client import BaseMCPClient


class HttpMCPClient(BaseMCPClient[StreamableHttpParameters]):
    def __init__(
        self,
        params: StreamableHttpParameters,
        state_change_callback: Callable[[str, str], None | Awaitable[None]] | None = None,
        message_handler: MessageHandlerFnT | None = None,
    ) -> None:
        """
        初始化HTTP客户端，支持传入自定义 message_handler
        Initialize HTTP client with optional message_handler
        """
        assert isinstance(params, StreamableHttpParameters), "params must be an instance of StreamableHttpParameters"
        super().__init__(params, state_change_callback, message_handler)

    async def _create_async_session(self) -> ClientSession:
        """
        创建异步会话

        Returns:
            ClientSession: 异步会话
        """
        # 目前忽略了 GetSessionIdCallback。只有在手动管理Session才有必要，在封装内全部使用自动管理。
        # 需要注意 self.params.model_dump() 的 mode 参数使用默认python，不可以使用json，因为当前Params中有 timedelta，如果使用json会序列化
        # 为str，导致连接报错。
        aread_stream, awrite_stream, _ = await self._aexit_stack.enter_async_context(
            streamablehttp_client(**self.params.model_dump(mode="python")),
        )
        # 如果提供了 message_handler，则一并传入 ClientSession
        # If message_handler is provided, pass it into ClientSession
        client_session = await self._aexit_stack.enter_async_context(
            ClientSession(aread_stream, awrite_stream, message_handler=self._message_handler),
        )
        return client_session
