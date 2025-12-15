# -*- coding: utf-8 -*-
# filename: resources_stdio_server.py
# @Time    : 2025/10/02 17:01
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm
"""
中文: 提供仅 Resources 能力（不支持订阅）的最小 MCP Stdio 服务器，用于 Desktop 集成测试。
英文: Minimal MCP Stdio server exposing Resources only (without subscribe) for Desktop integration tests.

文件名: resources_stdio_server.py / filename: resources_stdio_server.py
作者: JQQ / author: JQQ
创建日期: 2025/10/02 / created at: 2025/10/02
最后修改日期: 2025/10/02 / last modified at: 2025/10/02
版权: 2023 JQQ. All rights reserved. / copyright: 2023 JQQ. All rights reserved.
依赖: anyio, mcp / dependencies: anyio, mcp
描述: 暴露 window:// 资源，符合 Desktop 资源协议要求；用于测试不支持订阅的情况。
      Expose window:// resources compatible with Desktop requirements; no subscribe support.
"""

from __future__ import annotations

import anyio
import mcp.types as types
from mcp.server.lowlevel.server import Server
from mcp.server.stdio import stdio_server

# 预置若干窗口资源 / preset window resources
WINDOW_HOST = "example.desktop.itest"
WINDOW_RESOURCES: list[types.Resource] = [
    types.Resource(
        uri=f"window://{WINDOW_HOST}/main?priority=80",
        name="Main Window",
        description="中文: 主窗口; 英文: Main window",
        mimeType="text/markdown",
    ),
    types.Resource(
        uri=f"window://{WINDOW_HOST}/secondary?priority=20",
        name="Secondary Window",
        description="中文: 次级窗口; 英文: Secondary window",
        mimeType="text/markdown",
    ),
    types.Resource(
        uri=f"window://{WINDOW_HOST}/full?priority=100&fullscreen=true",
        name="Fullscreen Window",
        description="中文: 全屏窗口; 英文: Fullscreen window",
        mimeType="text/markdown",
    ),
]


async def run() -> None:
    """
    中文: 启动仅支持 Resources 的服务器，提供 window:// 资源枚举与读取能力。
    英文: Start a server supporting only Resources, providing window:// listing and reading.
    """
    server = Server(name="itest-resources-only", version="0.0.1", instructions="itest-desktop")

    # 列举资源（分页模拟）/ list resources with pagination simulation
    @server.list_resources()
    async def list_resources(req: types.ListResourcesRequest | None = None) -> list[types.Resource]:
        return WINDOW_RESOURCES

    # 读取资源内容 / read resource contents
    @server.read_resource()
    async def read_resource(uri: types.AnyUrl):
        text = f"# Window Resource\n\nURI: {uri}\n\n中文: 这是测试用窗口内容。\n英文: This is test window content.\n"
        # 直接返回文本，由底层封装为 TextResourceContents / return str, lowlevel will wrap
        return text

    async with stdio_server() as (read_stream, write_stream):
        init_opts = server.create_initialization_options()
        await server.run(read_stream, write_stream, init_opts)


if __name__ == "__main__":
    anyio.run(run)
