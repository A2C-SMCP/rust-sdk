# -*- coding: utf-8 -*-
# filename: resources_subscribe_stdio_server.py
# @Time    : 2025/10/02 17:01
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm
"""
中文: 提供 Resources 能力并支持 Subscribe 的 MCP Stdio 服务器，用于 Desktop 集成测试。
英文: MCP Stdio server exposing Resources with Subscribe support for Desktop integration tests.

文件名: resources_subscribe_stdio_server.py / filename: resources_subscribe_stdio_server.py
作者: JQQ / author: JQQ
创建日期: 2025/10/02 / created at: 2025/10/02
最后修改日期: 2025/10/02 / last modified at: 2025/10/02
版权: 2023 JQQ. All rights reserved. / copyright: 2023 JQQ. All rights reserved.
依赖: anyio, mcp / dependencies: anyio, mcp
描述: 暴露 window:// 资源，且在 capabilities 中声明 subscribe=True；订阅后立即发送一次更新通知。
      Expose window:// resources and declare subscribe=True; send one ResourceUpdated notification on subscribe.
"""

from __future__ import annotations

import anyio
from mcp import types
from mcp.server.lowlevel.server import Server as LowLevelServer
from mcp.server.stdio import stdio_server


class TestServer(LowLevelServer):
    """
    中文: 测试子类，覆盖 capabilities，使得当注册了订阅处理器时 resources.subscribe=True。
    英文: Test subclass overriding capabilities to set resources.subscribe=True when subscribe handlers exist.
    """

    def get_capabilities(  # type: ignore[override]
        self,
        notification_options,
        experimental_capabilities,
    ) -> types.ServerCapabilities:
        caps = super().get_capabilities(notification_options, experimental_capabilities)
        # 如果注册了订阅处理器，则开启 subscribe 标记 / enable subscribe flag if handlers exist
        if types.SubscribeRequest in self.request_handlers or types.UnsubscribeRequest in self.request_handlers:
            resources_cap = caps.resources or types.ResourcesCapability(
                subscribe=False,
                listChanged=False,
            )
            caps = types.ServerCapabilities(
                prompts=caps.prompts,
                resources=types.ResourcesCapability(
                    subscribe=True,
                    listChanged=resources_cap.listChanged,
                ),
                tools=caps.tools,
                logging=caps.logging,
                experimental=caps.experimental,
                completions=caps.completions,
            )
        return caps


# 预置若干窗口资源 / preset window resources
WINDOW_HOST = "example.desktop.subscribe.a"
WINDOW_RESOURCES: list[types.Resource] = [
    types.Resource(
        uri=f"window://{WINDOW_HOST}/main?priority=60",
        name="Main Window",
        description="中文: 主窗口; 英文: Main window",
        mimeType="text/markdown",
    ),
    types.Resource(
        uri=f"window://{WINDOW_HOST}/dashboard?priority=90&fullscreen=true",
        name="Dashboard",
        description="中文: 仪表盘; 英文: Dashboard",
        mimeType="text/markdown",
    ),
]


async def run() -> None:
    """
    中文: 启动支持 Resources + Subscribe 的服务器，提供 window:// 的列举、读取与订阅更新。
    英文: Start a server with Resources + Subscribe, providing window:// list/read/subscribe updates.
    """
    server = TestServer(name="itest-resources-subscribe", version="0.0.1", instructions="itest-desktop")

    # 列举资源（分页模拟）/ list resources with pagination simulation
    @server.list_resources()
    async def list_resources(req: types.ListResourcesRequest | None = None) -> list[types.Resource]:
        """
        中文: 模拟真实服务的分页返回；第一页返回一条并携带 nextCursor='page2'，第二页返回剩余条目。
        英文: Mimic real server pagination; page1 returns one item with nextCursor='page2', page2 returns the rest.
        """
        return WINDOW_RESOURCES

    # 读取资源内容 / read resource contents
    @server.read_resource()
    async def read_resource(uri: types.AnyUrl):
        text = (
            f"# Window Resource (subscribe)\n\nURI: {uri}\n\n中文: 这是订阅版A测试窗口内容。\n"
            f"英文: This is subscribed-A test window content.\n"
        )
        return text

    # 工具列表与调用处理器 / tools listing and calling handlers
    # 中文: 提供一个简单工具 mark_a，便于在测试中通过 tc 切换“最近调用服务”为 A。
    # 英文: Provide a simple tool `mark_a` so tests can switch the "most-recently-called server" to A via tc.
    @server.list_tools()
    async def list_tools() -> list[types.Tool]:
        return [
            types.Tool(
                name="mark_a",
                description="中文: 标记A; 英文: mark A",
                inputSchema={"type": "object", "properties": {}},
            ),
        ]

    @server.call_tool()
    async def call_tool(name: str, arguments: dict | None):  # noqa: ARG001
        # 中文: 回显工具名，便于 CLI 端确认调用已发生。
        # 英文: Echo tool name so the CLI can confirm the call happened.
        return [types.TextContent(type="text", text=f"ok:{name}")]

    # 订阅与取消订阅 / subscribe and unsubscribe
    @server.subscribe_resource()
    async def subscribe(uri: types.AnyUrl):
        # 订阅后立即发送一次更新通知，便于测试端验证 / send one update right after subscribing
        ctx = server.request_context
        await ctx.session.send_resource_updated(uri)

    @server.unsubscribe_resource()
    async def unsubscribe(_: types.AnyUrl):
        # 此处无需额外处理 / no-op for tests
        return None

    async with stdio_server() as (read_stream, write_stream):
        init_opts = server.create_initialization_options()
        await server.run(read_stream, write_stream, init_opts)


if __name__ == "__main__":
    anyio.run(run)
