# -*- coding: utf-8 -*-
# filename: resources_paged_mixed_stdio_server.py
# @Time    : 2025/10/02 22:36
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm
"""
中文: 提供 Resources 能力，包含分页与混合（含非 window://）资源，用于覆盖分页循环与过滤逻辑。
英文: MCP Stdio server exposing Resources with pagination and mixed schemes (includes non-window) to cover pagination
    loop and filtering logic.

文件名: resources_paged_mixed_stdio_server.py / filename: resources_paged_mixed_stdio_server.py
作者: JQQ / author: JQQ
创建日期: 2025/10/02 / created at: 2025/10/02
最后修改日期: 2025/10/02 / last modified at: 2025/10/02
版权: 2023 JQQ. All rights reserved. / copyright: 2023 JQQ. All rights reserved.
依赖: anyio, mcp / dependencies: anyio, mcp
描述: 第一页返回一个 window:// 资源，并携带 nextCursor='page2'；第二页返回一个非 window 资源与一个 window 资源。
      用于测试 `list_resources` 的分页与 `window://` 过滤逻辑。
"""

from __future__ import annotations

import anyio
from mcp import types
from mcp.server.lowlevel.server import Server as LowLevelServer
from mcp.server.stdio import stdio_server

HOST = "example.desktop.paged"
PAGE1 = [
    types.Resource(
        uri=f"window://{HOST}/p1?priority=10",
        name="P1",
        description="中文: 第1页窗口; 英文: page1 window",
        mimeType="text/markdown",
    ),
]
PAGE2 = [
    # 非 window 资源 / non-window resource
    types.Resource(
        uri="file://tmp/some.txt",
        name="NotWindow",
        description="中文: 非窗口; 英文: not window",
        mimeType="text/plain",
    ),
    types.Resource(
        uri=f"window://{HOST}/p2?priority=99",
        name="P2",
        description="中文: 第2页窗口; 英文: page2 window",
        mimeType="text/markdown",
    ),
]


class TestServer(LowLevelServer):
    """
    中文: 覆盖 capabilities，当存在订阅处理器时 resources.subscribe=True。
    英文: Override capabilities to advertise resources.subscribe=True when subscribe handlers exist.
    """

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


async def run() -> None:
    server = TestServer(name="itest-resources-paged-mixed", version="0.0.1", instructions="itest-desktop")

    @server.list_resources()
    async def list_resources(req: types.ListResourcesRequest):  # type: ignore[override]
        # 分页返回 / paginated return
        if req.params and req.params.cursor == "page2":
            return types.ListResourcesResult(resources=PAGE2, nextCursor=None)
        return types.ListResourcesResult(resources=PAGE1, nextCursor="page2")

    @server.read_resource()
    async def read_resource(uri: types.AnyUrl):  # noqa: ARG001
        return "# Window (paged)\n\n中文: 分页窗口; 英文: paged window.\n"

    # 订阅/取消订阅，便于客户端调用 subscribe_resource
    @server.subscribe_resource()
    async def subscribe(uri: types.AnyUrl):  # noqa: ARG001
        # 发送一次更新，帮助覆盖订阅路径
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
