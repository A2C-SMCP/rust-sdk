# -*- coding: utf-8 -*-
# filename: resources_subscribe_b_stdio_server.py
# @Time    : 2025/10/02 20:22
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm
"""
中文: 提供 Resources + Subscribe + Tool 的 MCP Stdio 服务器（B 版本），用于 e2e 测试多服务窗口与调用顺序。
英文: MCP Stdio server exposing Resources + Subscribe + Tool (variant B) for e2e tests.
"""

from __future__ import annotations

import anyio
import mcp.types as types
from mcp.server.lowlevel.server import Server as LowLevelServer
from mcp.server.stdio import stdio_server


class TestServer(LowLevelServer):
    def get_capabilities(self, notification_options, experimental_capabilities) -> types.ServerCapabilities:  # type: ignore[override]
        caps = super().get_capabilities(notification_options, experimental_capabilities)
        if types.SubscribeRequest in self.request_handlers or types.UnsubscribeRequest in self.request_handlers:
            resources_cap = caps.resources or types.ResourcesCapability(subscribe=False, listChanged=False)
            caps = types.ServerCapabilities(
                prompts=caps.prompts,
                resources=types.ResourcesCapability(subscribe=True, listChanged=resources_cap.listChanged),
                tools=caps.tools,
                logging=caps.logging,
                experimental=caps.experimental,
                completions=caps.completions,
            )
        return caps


WINDOW_HOST = "example.desktop.subscribe.b"
WINDOW_RESOURCES: list[types.Resource] = [
    types.Resource(
        uri=f"window://{WINDOW_HOST}/main?priority=50",
        name="Main-B",
        description="中文: B 主窗口; 英文: B main window",
        mimeType="text/markdown",
    ),
    types.Resource(
        uri=f"window://{WINDOW_HOST}/board?priority=85",
        name="Board-B",
        description="中文: B 看板; 英文: B board",
        mimeType="text/markdown",
    ),
]


async def run() -> None:
    server = TestServer(name="itest-resources-subscribe-b", version="0.0.1", instructions="itest-desktop-b")

    @server.list_resources()
    async def list_resources(req: types.ListResourcesRequest | None = None) -> list[types.Resource]:  # noqa: ARG001
        return WINDOW_RESOURCES

    @server.read_resource()
    async def read_resource(uri: types.AnyUrl):  # noqa: ARG001
        return "# B Window Resource\n\n中文: B 版本窗口; 英文: B variant window.\n"

    # 工具：mark_b
    @server.list_tools()
    async def list_tools() -> list[types.Tool]:
        return [
            types.Tool(
                name="mark_b",
                description="中文: 标记B; 英文: mark B",
                inputSchema={"type": "object", "properties": {}},
            ),
        ]

    @server.call_tool()
    async def call_tool(name: str, arguments: dict | None):  # noqa: ARG001
        return [types.TextContent(type="text", text=f"ok:{name}")]

    @server.subscribe_resource()
    async def subscribe(uri: types.AnyUrl):  # noqa: ARG001
        ctx = server.request_context
        await ctx.session.send_resource_updated(uri)

    @server.unsubscribe_resource()
    async def unsubscribe(_: types.AnyUrl):  # noqa: ARG001
        return None

    async with stdio_server() as (read_stream, write_stream):
        init_opts = server.create_initialization_options()
        await server.run(read_stream, write_stream, init_opts)


if __name__ == "__main__":
    anyio.run(run)
