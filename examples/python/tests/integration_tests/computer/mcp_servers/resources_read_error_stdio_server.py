# -*- coding: utf-8 -*-
# filename: resources_read_error_stdio_server.py
# @Time    : 2025/10/02 22:45
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm
"""
中文: 列举一个 window:// 资源，但在 read_resource 时抛出异常，用于覆盖 BaseMCPClient.get_window_detail 的异常路径。
英文: List a window:// resource but raise in read_resource to cover BaseMCPClient.get_window_detail exception path.
"""

from __future__ import annotations

import anyio
from mcp import types
from mcp.server.lowlevel.server import Server
from mcp.server.stdio import stdio_server

HOST = "example.desktop.readerr"
RES = types.Resource(
    uri=f"window://{HOST}/err?priority=1",
    name="ErrWin",
    description="中文: 出错窗口; 英文: error window",
    mimeType="text/markdown",
)


async def run() -> None:
    server = Server(name="itest-resources-readerr", version="0.0.1", instructions="itest-desktop")

    @server.list_resources()
    async def list_resources() -> list[types.Resource]:
        return [RES]

    @server.read_resource()
    async def read_resource(uri: types.AnyUrl):  # noqa: ARG001
        raise RuntimeError("boom")

    async with stdio_server() as (read_stream, write_stream):
        init_opts = server.create_initialization_options()
        await server.run(read_stream, write_stream, init_opts)


if __name__ == "__main__":
    anyio.run(run)
