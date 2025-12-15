# -*- coding: utf-8 -*-
# filename: mock_uv_server.py
# @Time    : 2025/8/21 14:29
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm

import asyncio

# 3rd party imports
import uvicorn

# socketio imports
from socketio import ASGIApp

PORT = 8000

# deactivate monitoring task in python-socketio to avoid errores during shutdown
# sio.eio.start_service_task = False

"""
参考：
https://github.com/miguelgrinberg/python-socketio/issues/332?utm_source=chatgpt.com
"""


class UvicornTestServer(uvicorn.Server):
    """Uvicorn test server

    Usage:
        @pytest.fixture
        async def start_stop_server():
            server = UvicornTestServer()
            await server.up()
            yield
            await server.down()
    """

    def __init__(self, app: ASGIApp = None, host: str = "127.0.0.1", port: int = PORT):
        """Create a Uvicorn test server

        Args:
            app (ASGIApp, optional): the ASGIApp app. Defaults to main.asgi_app.
            host (str, optional): the host ip. Defaults to '127.0.0.1'.
            port (int, optional): the port. Defaults to PORT.
        """
        self._startup_done = asyncio.Event()
        super().__init__(config=uvicorn.Config(app, host=host, port=port))

    async def startup(self, sockets: list | None = None) -> None:
        """Override uvicorn startup"""
        await super().startup(sockets=sockets)
        # self.config.setup_event_loop()  # 从0.36版本开始，不再需要这个方法
        self._startup_done.set()

    async def up(self) -> None:
        """Start up server asynchronously"""
        self._serve_task = asyncio.create_task(self.serve())
        await self._startup_done.wait()

    async def down(self, force: bool = False) -> None:
        """Shut down server asynchronously

        Args:
            force (bool): 中文: 是否强制快速关闭，跳过优雅关闭等待 / English: Force fast shutdown without graceful wait
        """
        self.should_exit = True
        if force:
            # 中文: 强制退出，不等待连接清理 / English: Force exit without waiting for connection cleanup
            self.force_exit = True
            # 中文: 取消服务任务以立即停止 / English: Cancel serve task to stop immediately
            if hasattr(self, "_serve_task") and not self._serve_task.done():
                self._serve_task.cancel()
                try:
                    # 中文: 减少超时时间到 0.2 秒以加快测试速度 / English: Reduce timeout to 0.2s to speed up tests
                    await asyncio.wait_for(self._serve_task, timeout=0.2)
                except (TimeoutError, asyncio.CancelledError):
                    pass
        else:
            await self._serve_task
